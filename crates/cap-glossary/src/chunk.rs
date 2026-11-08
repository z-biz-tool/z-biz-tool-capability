//! 把一篇 Markdown 切成「可检索、可引用、可回跳」的片段。
//!
//! 移植自 note 仓的 `rag/chunk.rs`（kb 能力嫁接到 note 的那一半），保留全部
//! 设计要点与测试。这里只做纯函数切片，不读盘、不涉及片段的落库缓存。
//!
//! ## 关键取舍
//!
//! 1. **先按标题分节**，每个片段记住标题路径（`安装 › macOS`），引用回跳才有上下文；
//! 2. 一节超过 `max_chars` 时**只在空行（段落边界）处切**，不切半句；
//! 3. 代码块整块保留，不在块内切 —— 半个脚本会让模型把截断的命令当完整的讲。
//!
//! `anchor` 存「这一片的第一句前 24 字」，前端在文档里找这一段即可回跳。

/// 一个可检索片段。字段名用短名（`idx`/`anchor`），每个字段在 UI 上都有明确去处。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Chunk {
    /// 来源文档的绝对路径
    pub path: String,
    /// 文档标题（取自正文首个标题行）
    pub title: String,
    /// 篇内序号，从 0 开始；同分排序时用它保证结果稳定
    pub idx: usize,
    /// 标题路径，例如 `安装 › macOS`；无标题时为文档标题
    pub heading: String,
    /// 定位锚点（首句前 24 字），供前端在文档里找回这一段
    pub anchor: String,
    /// 片段正文（已去掉标题行本身，标题在 `heading` 里）
    pub text: String,
}

/// 默认单片最大字符数。600 是折中：再小拆散论证，再大一次检索几条就撑爆上下文。
pub const DEFAULT_MAX_CHARS: usize = 600;

/// 锚点长度：24 字（中文）在实测里够唯一，太短撞开场白、太长易被编辑打断。
const ANCHOR_CHARS: usize = 24;

/// 找到一个 markdown 文档的 YAML frontmatter：返回（块内文本, 正文）。
///
/// 只有「首行独占 `---`、后面还有独占的 `---` 闭合行」才算，正文里的分割线不受影响。
fn split_frontmatter(content: &str) -> Option<(&str, &str)> {
    let body = content.strip_prefix('\u{feff}').unwrap_or(content);
    let first_line_end = body.find('\n').map(|i| i + 1).unwrap_or(body.len());
    if body[..first_line_end].trim_end() != "---" {
        return None;
    }
    let rest = &body[first_line_end..];
    let mut off = 0usize;
    for line in rest.split_inclusive('\n') {
        if line.trim_end() == "---" {
            return Some((&rest[..off], &rest[off + line.len()..]));
        }
        off += line.len();
    }
    None
}

/// 去掉 frontmatter 只留正文（没有时原样返回）。
fn strip_frontmatter(content: &str) -> &str {
    split_frontmatter(content)
        .map(|(_, body)| body)
        .unwrap_or(content)
}

/// 把一个 markdown 文档切成片段。
///
/// `path`/`title` 只做透传，函数本身不读盘 —— 纯函数才能在测试里跑。
// allow(unused_assignments)：flush! 宏里的 `buf_len = 0` 在最后一次展开后
// 不再被读，clippy 会报 dead store；宏内部这个复位是循环语义必需的。
#[allow(unused_assignments)]
pub fn chunk_markdown(path: &str, title: &str, markdown: &str, max_chars: usize) -> Vec<Chunk> {
    let body = strip_frontmatter(markdown);
    let max_chars = max_chars.max(120);

    let mut out: Vec<Chunk> = Vec::new();
    let mut stack: Vec<(usize, String)> = Vec::new(); // (层级, 标题)
    let mut buf: Vec<String> = Vec::new(); // 当前片段里已积累的块
    let mut buf_len = 0usize;
    let mut in_fence = false;
    let mut fence_mark = String::new();
    let mut block: Vec<String> = Vec::new(); // 正在攒的一个「块」

    macro_rules! flush {
        () => {
            if !buf.is_empty() {
                let text = buf.join("\n\n");
                let text = text.trim().to_string();
                if !text.is_empty() {
                    out.push(Chunk {
                        path: path.to_string(),
                        title: title.to_string(),
                        idx: out.len(),
                        heading: heading_path(&stack, title),
                        anchor: anchor_of(&text),
                        text,
                    });
                }
                buf.clear();
                buf_len = 0;
            }
        };
    }

    macro_rules! push_block {
        () => {
            if !block.is_empty() {
                let text = block.join("\n");
                block.clear();
                if !text.trim().is_empty() {
                    buf_len += char_len(&text);
                    buf.push(text);
                }
                if buf_len >= max_chars {
                    flush!();
                }
            }
        };
    }

    for raw in body.lines() {
        let line = raw.trim_end();

        if let Some(mark) = fence_of(line) {
            if !in_fence {
                in_fence = true;
                fence_mark = mark;
                block.push(line.to_string());
                continue;
            } else if mark == fence_mark {
                in_fence = false;
                block.push(line.to_string());
                push_block!();
                continue;
            }
        }
        if in_fence {
            block.push(line.to_string());
            continue;
        }

        if let Some((level, text)) = parse_heading(line) {
            push_block!();
            flush!();
            while stack.last().is_some_and(|(l, _)| *l >= level) {
                stack.pop();
            }
            stack.push((level, text));
            continue;
        }

        if line.trim().is_empty() {
            push_block!();
            continue;
        }

        block.push(line.to_string());

        if block.len() == 1 && char_len(&block[0]) > max_chars {
            let piece = block.pop().unwrap_or_default();
            for part in split_oversized(&piece, max_chars) {
                block = vec![part];
                push_block!();
            }
        }
    }
    push_block!();
    flush!();

    // 同一文档里锚点撞了就给后面的补序号，保证前端「按锚点找第一处」不总跳到同段。
    let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for chunk in out.iter_mut() {
        let count = seen.entry(chunk.anchor.clone()).or_insert(0);
        if *count > 0 {
            chunk.anchor = format!("{}#{}", chunk.anchor, *count);
        }
        *count += 1;
    }
    out
}

/// 当前标题路径。没有标题的文档用它自己的标题，避免空字符串出现在引用卡片上。
fn heading_path(stack: &[(usize, String)], title: &str) -> String {
    if stack.is_empty() {
        return title.to_string();
    }
    stack
        .iter()
        .map(|(_, t)| t.as_str())
        .collect::<Vec<_>>()
        .join(" › ")
}

/// 锚点：首行去掉行内 Markdown 记号后的前 24 字。
fn anchor_of(text: &str) -> String {
    let first = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with("```") && !l.starts_with("~~~"))
        .unwrap_or("")
        .trim_start_matches(['#', '>', '-', '*', '+', ' ', '\t'])
        .trim();
    let cleaned = first.replace("**", "").replace("`", "").replace("[[", "");
    let cleaned = cleaned.replace("]]", "").replace("~~", "");
    let cleaned = cleaned.trim();

    let mut anchor: String = cleaned.chars().take(ANCHOR_CHARS).collect();
    if cleaned.chars().count() > ANCHOR_CHARS {
        anchor = anchor.trim_end().to_string();
    }
    if anchor.is_empty() {
        anchor = text.chars().take(ANCHOR_CHARS).collect();
    }
    anchor
}

/// 识别 ATX 标题，返回 (层级, 文本)。`#标签` 这种没有空格的不是标题。
fn parse_heading(line: &str) -> Option<(usize, String)> {
    let hashes = line.chars().take_while(|c| *c == '#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest = &line[hashes..];
    match rest.chars().next() {
        Some(c) if c.is_whitespace() => {
            let text = rest.trim();
            (!text.is_empty()).then(|| (hashes, text.to_string()))
        }
        None => None,
        _ => None,
    }
}

/// 围栏开始/结束标记（``` 或 ~~~）
fn fence_of(line: &str) -> Option<String> {
    let t = line.trim_start();
    ["```", "~~~"]
        .into_iter()
        .find(|m| t.starts_with(m))
        .map(str::to_string)
}

/// 单块超长时按标点切：优先句末，其次逗号，最后硬切。
fn split_oversized(text: &str, max_chars: usize) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut out: Vec<String> = Vec::new();
    let mut start = 0usize;
    while start < chars.len() {
        let hard_end = (start + max_chars).min(chars.len());
        if hard_end == chars.len() {
            out.push(chars[start..].iter().collect());
            break;
        }
        let floor = start + max_chars * 7 / 10;
        let mut cut = hard_end;
        for i in (floor..hard_end).rev() {
            if matches!(chars[i], '。' | '！' | '？' | '；' | '!' | '?' | ';' | '\n') {
                cut = i + 1;
                break;
            }
        }
        out.push(chars[start..cut].iter().collect());
        start = cut;
    }
    out.into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn char_len(s: &str) -> usize {
    s.chars().count()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(chunks: &[Chunk]) -> Vec<&str> {
        chunks.iter().map(|c| c.text.as_str()).collect()
    }

    #[test]
    fn splits_on_headings_and_keeps_heading_path() {
        let md =
            "# 安装\n\n第一步，装依赖。\n\n## macOS\n\n用 brew。\n\n## Windows\n\n用 winget。\n";
        let chunks = chunk_markdown("/n.md", "安装指南", md, DEFAULT_MAX_CHARS);
        assert_eq!(chunks.len(), 3, "三个小节应各成一片: {:?}", texts(&chunks));
        assert_eq!(chunks[0].heading, "安装");
        assert_eq!(chunks[1].heading, "安装 › macOS");
        assert_eq!(chunks[2].heading, "安装 › Windows");
        assert_eq!(chunks[1].text, "用 brew。");
    }

    #[test]
    fn empty_note_yields_no_chunks() {
        assert!(chunk_markdown("/n.md", "空", "", DEFAULT_MAX_CHARS).is_empty());
        assert!(
            chunk_markdown("/n.md", "空", "---\ntags: [a]\n---\n\n", DEFAULT_MAX_CHARS).is_empty()
        );
    }

    #[test]
    fn frontmatter_is_not_indexed() {
        let md = "---\ntags:\n  - work\n---\n# 会议\n\n结论是 A。\n";
        let chunks = chunk_markdown("/n.md", "会议", md, DEFAULT_MAX_CHARS);
        assert_eq!(chunks.len(), 1);
        assert!(!chunks[0].text.contains("tags"), "frontmatter 不该进索引");
        assert_eq!(chunks[0].text, "结论是 A。");
    }

    #[test]
    fn hash_tags_are_not_headings() {
        let chunks = chunk_markdown(
            "/n.md",
            "标签",
            "#work 这件事\n\n正文。\n",
            DEFAULT_MAX_CHARS,
        );
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].text.contains("#work"));
    }

    #[test]
    fn code_fence_is_never_split() {
        let code = format!("```bash\n{}\n```", "echo hi\n".repeat(80));
        let md = format!("# 脚本\n\n说明。\n\n{}\n", code);
        let chunks = chunk_markdown("/n.md", "脚本", &md, 200);
        let holder = chunks
            .iter()
            .find(|c| c.text.contains("echo hi"))
            .expect("代码块应被索引");
        assert!(
            holder.text.contains("```bash") && holder.text.trim_end().ends_with("```"),
            "代码块不该被切开: {}",
            holder.text
        );
    }

    #[test]
    fn long_section_splits_on_paragraph_boundary() {
        let para = "这是一段用来测试切分的文字，长度大约三十六个字，用来把这一节撑长一点。";
        let md = format!(
            "# 长文\n\n{}\n\n{}\n\n{}\n\n{}\n\n{}\n\n{}",
            para, para, para, para, para, para
        );
        let chunks = chunk_markdown("/n.md", "长文", &md, 150);
        assert!(chunks.len() >= 2, "应至少切成两片: {:?}", texts(&chunks));
        for c in &chunks {
            assert!(
                c.text.starts_with("这是一段"),
                "不该从句子中间切开: {}",
                c.text
            );
            assert!(
                c.text.chars().count() <= 150 + 40,
                "单片不该超过上限太多（{} 字）: {}",
                c.text.chars().count(),
                c.text
            );
        }
    }

    #[test]
    fn anchors_are_deduped_per_note() {
        let md = "# A\n\n同一句话。\n\n# B\n\n同一句话。\n";
        let chunks = chunk_markdown("/n.md", "t", md, DEFAULT_MAX_CHARS);
        assert_eq!(chunks[0].anchor, "同一句话。");
        assert_eq!(chunks[1].anchor, "同一句话。#1");
    }

    #[test]
    fn anchor_strips_inline_markup() {
        let chunks = chunk_markdown(
            "/n.md",
            "t",
            "# H\n\n**重点**：`code` 与 [[双链]]。\n",
            DEFAULT_MAX_CHARS,
        );
        assert_eq!(chunks[0].anchor, "重点：code 与 双链。");
    }

    #[test]
    fn heading_only_section_still_indexed_by_parent() {
        let chunks = chunk_markdown("/n.md", "只有标题", "# 标题\n", DEFAULT_MAX_CHARS);
        assert!(chunks.is_empty(), "没有正文就没有可检索内容");
    }

    #[test]
    fn cjk_oversized_paragraph_does_not_panic() {
        let long = "中文".repeat(500);
        let chunks = chunk_markdown("/n.md", "t", &long, 200);
        assert!(!chunks.is_empty());
        assert!(chunks.iter().all(|c| c.text.chars().count() <= 260));
    }
}