//! 检索 → 带引用编号的问答提示组装。
//!
//! 这是 kb 与 note 合并的那一半：kb 原来只能问上传的 PDF/Word，note 手里有大批
//! 本地文档却缺提问入口。合并后只有一套语料，提问、回答、引用指回同一批文件。
//! 本模块负责提示词、引用装配、以及「模型没答好」时的兜底 —— 纯函数，脱库可测。
//!
//! 分工：[`chunk`]（切片）→ [`bm25`]（排序）→ 本模块（拼提示词/引用/兜底）。
//! 落库与缓存由 host 负责，这不在这层。

use crate::bm25::{normalize_scores, Bm25Index};
use serde::{Deserialize, Serialize};

/// 一次问答里的一条引用（前端引用卡片的数据源）
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Citation {
    /// 文档绝对路径
    pub path: String,
    /// 文档标题
    pub title: String,
    /// 标题路径，例如 `安装 › macOS`
    pub heading: String,
    /// 定位锚点（编辑器里按这段文字找回位置）
    pub anchor: String,
    /// 片段正文
    pub text: String,
    /// 相对相关度，0~1（本段分数 / 本次最高分）
    pub score: f64,
    /// 在回答里的编号，从 1 开始（对应 `[1]`）
    pub no: usize,
}

/// 索引规模，给前端显示「已索引 1,284 段 / 312 篇」
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RagStatus {
    pub notes: usize,
    pub chunks: usize,
}

/// 默认取多少条片段做证据。
///
/// 5 条是 kb 用下来的值：再多，模型开始把不相干的片段硬凑进答案；
/// 再少，跨两篇文档的综合问题会缺一半依据。
pub const DEFAULT_TOP_K: usize = 5;

/// 系统提示词。
///
/// 三条约束都是踩过的坑：
/// - 不编：模型对「个人语料」这类私有内容会顺着常识补全，必须钉死只看片段；
/// - 必须标号：没有 `[n]` 的答案没法回跳到原文，等于把问答退化成聊天；
/// - 允许说没有：否则它会拿最相近的一条片段硬答，用户以为文档里真这么写过。
pub const SYSTEM_PROMPT: &str = "你是用户个人词库的问答助手。用户会给你若干编号片段，\
每条片段来自他自己的某篇文档（或词条）。请遵守：\n\
1. 只依据片段内容回答，不要补充片段之外的常识；片段里没有的信息，直接说「没有找到关于……的内容」。\n\
2. 每一条结论后面用方括号标注依据，例如 [1] 或 [2][3]，编号必须来自给定的片段。\n\
3. 用简洁的中文作答，先给结论，再给必要的细节；不要复述片段原文，不要输出片段编号清单。";

/// 拼用户消息：编号片段 + 问题。
///
/// 片段之间用分隔线而不是空行 —— 空行会让长片段里的列表和下一片段粘连，
/// 模型会把两篇文档的内容读成一篇。
pub fn build_user_message(question: &str, citations: &[Citation]) -> String {
    let mut ctx = String::new();
    for c in citations {
        ctx.push_str(&format!(
            "[{}] 《{}》· {}\n{}\n\n---\n\n",
            c.no, c.title, c.heading, c.text
        ));
    }
    format!(
        "{}\n问题：{}",
        ctx.trim_end_matches(['-', '\n', ' ']),
        question
    )
}

/// 把检索命中装成引用列表（编号从 1 开始，分数归一化）
pub fn to_citations(index: &Bm25Index, hits: &[(usize, f64)]) -> Vec<Citation> {
    let mut hits = hits.to_vec();
    normalize_scores(&mut hits);
    hits.iter()
        .enumerate()
        .map(|(i, (doc, score))| {
            let chunk = &index.chunks()[*doc];
            Citation {
                path: chunk.path.clone(),
                title: chunk.title.clone(),
                heading: chunk.heading.clone(),
                anchor: chunk.anchor.clone(),
                text: chunk.text.clone(),
                score: *score,
                no: i + 1,
            }
        })
        .collect()
}

/// 检索不到证据时给用户的话（不调用模型，省钱也省一次失望）。
///
/// 这句话要说清楚两件事：**语料是空的**还是**只是没命中** —— 前者要引导去索引，
/// 后者要引导改写问题。
pub fn no_evidence_message(corpus_chunks: usize, scoped: bool) -> String {
    if corpus_chunks == 0 {
        "还没有可检索的内容。先在侧栏里打开一个语料文件夹，索引建好后就能问它了。".to_string()
    } else if scoped {
        "这份文档里没找到相关内容。试试切到「全部」，或者换个说法。".to_string()
    } else {
        "没有找到相关内容。换个关键词试试，或者确认内容已经保存。".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunk::{chunk_markdown, DEFAULT_MAX_CHARS};

    fn corpus() -> Bm25Index {
        let mut chunks = chunk_markdown(
            "/notes/部署.md",
            "部署",
            "# 部署\n\n上线前要跑一遍冒烟测试。\n\n## 回滚\n\n出问题就回滚到上一个 tag。\n",
            DEFAULT_MAX_CHARS,
        );
        chunks.extend(chunk_markdown(
            "/notes/会议.md",
            "会议",
            "# 周会\n\n结论是先做合并，再做界面。\n",
            DEFAULT_MAX_CHARS,
        ));
        Bm25Index::build(chunks)
    }

    #[test]
    fn citations_are_numbered_from_one() {
        let idx = corpus();
        let hits = idx.search("回滚", 3, None);
        let cites = to_citations(&idx, &hits);
        assert!(!cites.is_empty());
        assert_eq!(cites[0].no, 1, "编号从 1 开始，前端要显示 [1]");
        assert!(cites[0].text.contains("回滚"));
        assert!((cites[0].score - 1.0).abs() < 1e-9, "第一名归一化为 1");
    }

    #[test]
    fn user_message_contains_numbered_evidence_and_question() {
        let idx = corpus();
        let hits = idx.search("冒烟测试", 2, None);
        let cites = to_citations(&idx, &hits);
        let msg = build_user_message("上线前要做什么？", &cites);
        assert!(msg.starts_with("[1] 《部署》"));
        assert!(msg.contains("问题：上线前要做什么？"));
        assert!(msg.trim_end().ends_with("上线前要做什么？"));
    }

    #[test]
    fn user_message_has_no_trailing_rule() {
        let idx = corpus();
        let hits = idx.search("回滚", 1, None);
        let cites = to_citations(&idx, &hits);
        let msg = build_user_message("q", &cites);
        assert!(!msg.contains("---\n\n问题"), "分隔线不该拖到问题前面: {msg}");
    }

    #[test]
    fn empty_corpus_message_points_at_onboarding() {
        let msg = no_evidence_message(0, false);
        assert!(msg.contains("打开") && msg.contains("文件夹"));
    }

    #[test]
    fn scoped_miss_suggests_switching_scope() {
        assert!(no_evidence_message(42, true).contains("全部"));
        assert!(no_evidence_message(42, false).contains("关键词"));
    }
}