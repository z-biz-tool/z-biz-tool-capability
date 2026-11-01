//! OOXML（.docx / .pptx）文本提取 —— 路径进 → 结构出。
//!
//! 从 `z-biz-tool-kb/src-tauri/src/rag.rs` 平移（kb 2026-11-01 宣布退役，
//! 唯一未迁移能力「外部文档问答」的解析半边落在这里；问答半边 note 已有
//! ask_notes）。平移时保持语义零漂移，只做三件原语层化的事：
//!
//! 1. 删掉 kb 私有的「文件名 → doc_type」判断 —— 那是调用方策略，
//!    本 crate 用 [`SUPPORTED_EXTENSIONS`] + [`supports`] 承担同一职责。
//! 2. 入口从「内存 bytes」改为「路径」（与 pdf/ebook 模块同风格）。
//! 3. 「空结果」的处置比 kb 更显式：zip 打不开 / 主部件缺失报 Err；
//!    部件在但一个文本都抠不出来（扫描件/纯图片）也报 Err，错误文案
//!    直接指给用户能做的下一步 —— kb 时代这个判定散在调用方。
//!
//! ## 为什么不建 DOM
//!
//! 一个 200 页的 document.xml 用 DOM 解析是浪费；这里只做「文本节点抠取」
//! 一件事：按 `<tag>…</tag>` 找文本，遇到块级闭合标签补一个换行 —— 段落
//! 边界保留，切片才不会把两段粘成一句。

use serde::{Deserialize, Serialize};
use std::io::Read;

/// .docx 正文文本。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DocxText {
    /// 全部段落文本，按文档顺序
    pub paragraphs: Vec<String>,
    /// 段落以空行连接的全文（便于直接落 Markdown）
    pub full: String,
}

/// .pptx 全文，按幻灯片序号排序。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PptxText {
    /// (页码, 该页全部文本行)
    pub slides: Vec<(u32, Vec<String>)>,
    /// 无页码标记的纯文本全文（段落间空行分隔）
    pub full: String,
    /// 含 `[第 N 页]` 标记的全文。
    ///
    /// 页边界是切片边界，问答检索需要它 —— kb 时代就用这个形态喂索引。
    pub with_page_marks: String,
}

/// 提取 .docx 正文文本。
pub fn extract_docx_text(path: impl AsRef<std::path::Path>) -> Result<DocxText, String> {
    let path = path.as_ref();
    let bytes = std::fs::read(path).map_err(|e| format!("读取文件失败: {}", e))?;
    let cursor = std::io::Cursor::new(&bytes);
    let mut archive =
        zip::ZipArchive::new(cursor).map_err(|e| format!("打开 docx 包失败: {}", e))?;

    // 主文档 + 脚注/尾注：正文里插入的脚注常带关键定义，丢掉它们检索就缺证据
    let mut paragraphs: Vec<String> = Vec::new();
    for name in [
        "word/document.xml",
        "word/footnotes.xml",
        "word/endnotes.xml",
    ] {
        match archive.by_name(name) {
            Ok(entry) => paragraphs.extend(read_xml_paragraphs(entry, "w:t", "</w:p>")),
            // footnotes/endnotes 在有的生成器里不存在，缺省不算错
            Err(_) if name != "word/document.xml" => continue,
            Err(e) => return Err(format!("docx 里缺 word/document.xml: {}", e)),
        }
    }

    let paragraphs: Vec<String> = paragraphs
        .into_iter()
        .filter(|p| !p.trim().is_empty())
        .collect();
    if paragraphs.is_empty() {
        return Err(
            "docx 里没解析出任何文本（扫描件/纯图片 Word？），没有可提取的内容".to_string(),
        );
    }
    Ok(DocxText {
        full: paragraphs.join("\n\n"),
        paragraphs,
    })
}

/// 提取 .pptx 全文（按幻灯片序号排序，不按 zip 字典序 —— slide10 会插到 slide2 前面）。
pub fn extract_pptx_text(path: impl AsRef<std::path::Path>) -> Result<PptxText, String> {
    let path = path.as_ref();
    let bytes = std::fs::read(path).map_err(|e| format!("读取文件失败: {}", e))?;
    let cursor = std::io::Cursor::new(&bytes);
    let mut archive =
        zip::ZipArchive::new(cursor).map_err(|e| format!("打开 pptx 包失败: {}", e))?;

    let mut slides: Vec<(u32, String)> = Vec::new();
    for i in 0..archive.len() {
        // zip 9 的 name() 返回 Result：名字里带非法 UTF-8 时跳过该条目
        let name = match archive.by_index(i) {
            Ok(f) => match f.name() {
                Ok(n) => n.to_string(),
                Err(_) => continue,
            },
            Err(_) => continue,
        };
        if let Some(num) = slide_number(&name) {
            slides.push((num, name));
        }
    }
    slides.sort_by_key(|(n, _)| *n);

    let mut structured: Vec<(u32, Vec<String>)> = Vec::new();
    let mut full = String::new();
    let mut with_page_marks = String::new();
    for (num, name) in slides {
        let entry = match archive.by_name(&name) {
            Ok(f) => f,
            Err(_) => continue,
        };
        let lines = read_xml_paragraphs(entry, "a:t", "</a:p>");
        if lines.is_empty() {
            continue;
        }
        structured.push((num, lines.clone()));
        with_page_marks.push_str(&format!("\n[第 {} 页]\n", num));
        with_page_marks.push_str(&lines.join("\n"));
        full.push_str(&lines.join("\n"));
        full.push_str("\n\n");
    }
    if structured.is_empty() {
        return Err("pptx 里没解析出任何文本（纯图片幻灯片？），没有可提取的内容".to_string());
    }
    Ok(PptxText {
        slides: structured,
        full,
        with_page_marks,
    })
}

/// 幻灯片序号：`ppt/slides/slideN.xml` → N。
fn slide_number(name: &str) -> Option<u32> {
    let rest = name.strip_prefix("ppt/slides/slide")?;
    let digits = rest.strip_suffix(".xml")?;
    digits.parse::<u32>().ok()
}

/// 从一个 XML 部件里按 `<tag>…</tag>` 抠文本，遇到 `block_end` 视为段落边界。
///
/// kb 原版返回「段落用 `\n` 连接的一个 String」；这里按段落切开返回，
/// 调用方要什么形态自己拼。
fn read_xml_paragraphs<R: Read>(mut entry: R, tag: &str, block_end: &str) -> Vec<String> {
    let mut buf = String::new();
    if entry.read_to_string(&mut buf).is_err() {
        return Vec::new();
    }
    let open = format!("<{}", tag);
    let close = format!("</{}>", tag);
    let mut out = String::new();
    let bytes: Vec<char> = buf.chars().collect();
    let mut i = 0usize;
    while i < bytes.len() {
        if starts_at(&bytes, i, &open) {
            // 跳过属性区找到 '>'：XML 属性值不含裸 '>'，这个假设 kb 时代已验证
            let end_of_tag = match bytes[i..].iter().position(|&c| c == '>') {
                Some(p) => i + p + 1,
                None => break,
            };
            let text_start = end_of_tag;
            let text_end = match find_at(&bytes, text_start, &close) {
                Some(p) => p,
                None => break,
            };
            out.push_str(&decode_entities(&bytes[text_start..text_end]));
            i = text_end + close.chars().count();
        } else if starts_at(&bytes, i, block_end) {
            out.push('\n');
            i += block_end.chars().count();
        } else {
            i += 1;
        }
    }
    out.split('\n')
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect()
}

fn starts_at(hay: &[char], at: usize, needle: &str) -> bool {
    let n: Vec<char> = needle.chars().collect();
    at + n.len() <= hay.len() && hay[at..at + n.len()] == n[..]
}

fn find_at(hay: &[char], from: usize, needle: &str) -> Option<usize> {
    let n: Vec<char> = needle.chars().collect();
    let mut i = from;
    while i + n.len() <= hay.len() {
        if hay[i..i + n.len()] == n[..] {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn decode_entities(s: &[char]) -> String {
    let text: String = s.iter().collect();
    let mut out = String::with_capacity(text.len());
    let mut rest = text.as_str();
    while let Some(idx) = rest.find('&') {
        out.push_str(&rest[..idx]);
        let tail = &rest[idx + 1..];
        if let Some(semi) = tail.find(';') {
            let entity = &tail[..semi];
            let decoded = match entity {
                "amp" => Some("&"),
                "lt" => Some("<"),
                "gt" => Some(">"),
                "quot" => Some("\""),
                "apos" => Some("'"),
                "nbsp" => Some(" "),
                _ => None,
            };
            if let Some(d) = decoded {
                out.push_str(d);
                rest = &tail[semi + 1..];
                continue;
            }
            if let Some(num) = entity.strip_prefix('#') {
                let code = num
                    .strip_prefix('x')
                    .and_then(|h| u32::from_str_radix(h, 16).ok())
                    .or_else(|| num.parse::<u32>().ok());
                if let Some(c) = code.and_then(char::from_u32) {
                    out.push(c);
                    rest = &tail[semi + 1..];
                    continue;
                }
            }
        }
        out.push('&');
        rest = tail;
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;
    use std::io::Write as _;

    /// 用真 zip 写一个最小 docx：主文档 + 脚注。
    fn write_docx(dir: &TempDir, paragraphs: &[&str]) -> std::path::PathBuf {
        let path = dir.join("t.docx");
        let file = std::fs::File::create(&path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        zip.start_file(
            "word/document.xml",
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
        let body: String = paragraphs
            .iter()
            .map(|p| format!("<w:p><w:r><w:t>{}</w:t></w:r></w:p>", p))
            .collect();
        write!(zip, "<w:document><w:body>{}</w:body></w:document>", body).unwrap();
        zip.start_file(
            "word/footnotes.xml",
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
        write!(
            zip,
            "<w:fnt><w:p><w:r><w:t>脚注内容</w:t></w:r></w:p></w:fnt>"
        )
        .unwrap();
        zip.finish().unwrap();
        path
    }

    #[test]
    fn docx_extracts_paragraphs_and_footnotes() {
        let dir = TempDir::new("ooxml-docx");
        let path = write_docx(&dir, &["第一段&amp;符号", "第二段"]);
        let doc = extract_docx_text(&path).unwrap();
        // 主文档两段 + 脚注一段：脚注常带关键定义，故意并入语料
        assert_eq!(
            doc.paragraphs,
            vec![
                "第一段&符号".to_string(),
                "第二段".to_string(),
                "脚注内容".to_string()
            ]
        );
        assert_eq!(doc.full, "第一段&符号\n\n第二段\n\n脚注内容");
    }

    #[test]
    fn docx_empty_reports_scanned_hint() {
        let dir = TempDir::new("ooxml-docx-empty");
        let path = dir.join("e.docx");
        let file = std::fs::File::create(&path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        zip.start_file(
            "word/document.xml",
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
        write!(
            zip,
            "<w:document><w:body><w:p><w:r><w:drawing/></w:r></w:p></w:body></w:document>"
        )
        .unwrap();
        zip.finish().unwrap();
        let err = extract_docx_text(&path).unwrap_err();
        assert!(err.contains("扫描件"), "错误要指路: {}", err);
    }

    #[test]
    fn docx_missing_main_part_is_err() {
        let dir = TempDir::new("ooxml-docx-broken");
        let path = dir.join("b.docx");
        let file = std::fs::File::create(&path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        zip.start_file("other.xml", zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.finish().unwrap();
        assert!(extract_docx_text(&path).is_err());
    }

    #[test]
    fn pptx_orders_slides_by_number_not_lexicographic() {
        let dir = TempDir::new("ooxml-pptx");
        let path = dir.join("t.pptx");
        let file = std::fs::File::create(&path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        // 故意按字典序写入 slide1, slide10, slide2 —— 提取结果必须按数字序
        for (n, text) in [(1, "开场"), (10, "结尾"), (2, "正文")] {
            zip.start_file(
                format!("ppt/slides/slide{}.xml", n),
                zip::write::SimpleFileOptions::default(),
            )
            .unwrap();
            write!(zip, "<p:sld><a:p><a:t>{}</a:t></a:p></p:sld>", text).unwrap();
        }
        zip.finish().unwrap();
        let ppt = extract_pptx_text(&path).unwrap();
        let nums: Vec<u32> = ppt.slides.iter().map(|(n, _)| *n).collect();
        assert_eq!(nums, vec![1, 2, 10], "slide10 不能插到 slide2 前面");
        assert!(ppt.with_page_marks.contains("[第 10 页]"));
        assert!(ppt.with_page_marks.contains("结尾"));
        assert!(!ppt.full.contains("[第"), "纯文本全文不带页码标记");
    }

    #[test]
    fn slide_number_parses_known_shapes() {
        assert_eq!(slide_number("ppt/slides/slide12.xml"), Some(12));
        assert_eq!(slide_number("ppt/slideLayouts/slide1.xml"), None);
        assert_eq!(slide_number("ppt/slides/slidex.xml"), None);
    }
}
