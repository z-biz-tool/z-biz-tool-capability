//! 通用词条存取：增删改查 + 检索。
//!
//! 这是「词库」作为通用板块的核心数据结构 —— 一堆离散的「词 → 释义/内容」条目，
//! 可嵌入任意 host（file 的词库板块用它，note 的「问笔记」也能在语料层面复用它）。
//!
//! ## 设计
//!
//! - **纯内存 + 可序列化**：`GlossaryStore` 是 `serde` 可序列化的，host 负责落盘
//!   （file 的 `glossary_app` 把它存成 JSON）。本层不碰文件。
//! - **id 稳定**：`add` 幂等（同 id 覆盖）；删改都按 id，不按词名（词名可改）。
//! - **检索用同一套 tokenize**：与 [`crate::bm25`] 同词法，词条检索和片段检索口径一致。

use crate::bm25::tokenize;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// 一个词条。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GlossaryTerm {
    /// 稳定 id（host 生成，如 uuid 或自增），删改都按它
    pub id: String,
    /// 词条名
    pub term: String,
    /// 释义 / 内容（可为 Markdown）
    pub content: String,
    /// 分类标签
    pub tags: Vec<String>,
    /// 更新时间（epoch 毫秒）
    pub updated_at: i64,
}

/// 词条集合。持有一个有序列表，约定按 `term` 排序便于列表展示稳定。
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct GlossaryStore {
    terms: Vec<GlossaryTerm>,
}

impl GlossaryStore {
    pub fn new() -> Self {
        Self { terms: Vec::new() }
    }

    /// 从一组词条重建（host 从磁盘载入后用）。
    pub fn from_terms(mut terms: Vec<GlossaryTerm>) -> Self {
        terms.sort_by(|a, b| a.term.cmp(&b.term));
        Self { terms }
    }

    pub fn len(&self) -> usize {
        self.terms.len()
    }

    pub fn is_empty(&self) -> bool {
        self.terms.is_empty()
    }

    /// 只读列表（已按词名排序）。
    pub fn list(&self) -> &[GlossaryTerm] {
        &self.terms
    }

    pub fn get(&self, id: &str) -> Option<&GlossaryTerm> {
        self.terms.iter().find(|t| t.id == id)
    }

    /// 新增或覆盖（同 id 幂等）；词条名为空拒绝。
    pub fn upsert(&mut self, term: GlossaryTerm) -> Result<(), String> {
        if term.term.trim().is_empty() {
            return Err("词条名不能为空".into());
        }
        if let Some(slot) = self.terms.iter_mut().find(|t| t.id == term.id) {
            *slot = term;
        } else {
            self.terms.push(term);
        }
        self.terms.sort_by(|a, b| a.term.cmp(&b.term));
        Ok(())
    }

    /// 删除一个词条，返回是否真的删到了。
    pub fn remove(&mut self, id: &str) -> bool {
        let before = self.terms.len();
        self.terms.retain(|t| t.id != id);
        self.terms.len() != before
    }

    /// 检索：按 query 与 (词名+内容+标签) 的 token 重叠打分，词名前缀/包含加权。
    ///
    /// 返回按相关度降序的命中引用。空查询返回空（列表用 list()）。
    pub fn search(&self, query: &str, top_n: usize) -> Vec<&GlossaryTerm> {
        if query.trim().is_empty() {
            return Vec::new();
        }
        let q_terms = tokenize(query);
        let q_set: std::collections::HashSet<&str> = q_terms.iter().map(String::as_str).collect();
        if q_set.is_empty() {
            return Vec::new();
        }

        let mut scored: Vec<(&GlossaryTerm, usize)> = self
            .terms
            .iter()
            .map(|t| {
                let haystack = format!("{} {} {}", t.term, t.content, t.tags.join(" "));
                let hits = tokenize(&haystack)
                    .into_iter()
                    .filter(|w| q_set.contains(w.as_str()))
                    .count();
                // 词名前缀/完整包含是强信号：搜「税」应优先「税务」而非正文里提过税的那条
                let mut score = hits;
                if t.term.starts_with(query.trim()) {
                    score += 4;
                } else if t.term.contains(query.trim()) {
                    score += 2;
                }
                (t, score)
            })
            .filter(|(_, s)| *s > 0)
            .collect();

        scored.sort_by(|a, b| {
            b.1.cmp(&a.1).then_with(|| a.0.term.cmp(&b.0.term))
        });
        scored.truncate(top_n);
        scored.into_iter().map(|(t, _)| t).collect()
    }

    /// 标签索引：tag -> 词条数。给前端做标签筛选。
    pub fn tag_counts(&self) -> HashMap<String, usize> {
        let mut out: HashMap<String, usize> = HashMap::new();
        for t in &self.terms {
            for tag in &t.tags {
                *out.entry(tag.clone()).or_insert(0) += 1;
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn term(id: &str, name: &str, content: &str, tags: &[&str]) -> GlossaryTerm {
        GlossaryTerm {
            id: id.to_string(),
            term: name.to_string(),
            content: content.to_string(),
            tags: tags.iter().map(|s| s.to_string()).collect(),
            updated_at: 0,
        }
    }

    #[test]
    fn upsert_is_idempotent_and_sorted() {
        let mut s = GlossaryStore::new();
        s.upsert(term("a", "税务", "如何申报。", &["财务"])).unwrap();
        s.upsert(term("b", "报销", "流程。", &["财务"])).unwrap();
        assert_eq!(s.len(), 2);
        assert_eq!(s.list()[0].term, "报销", "列表按词名排序");
        s.upsert(term("a", "税务", "改成这个。", &["财务"])).unwrap();
        assert_eq!(s.len(), 2, "同 id 覆盖而非新增");
        assert_eq!(s.get("a").unwrap().content, "改成这个。");
    }

    #[test]
    fn empty_term_name_rejected() {
        let mut s = GlossaryStore::new();
        assert!(s.upsert(term("a", "  ", "x", &[])).is_err());
    }

    #[test]
    fn remove_returns_whether_deleted() {
        let mut s = GlossaryStore::new();
        s.upsert(term("a", "税务", "x", &[])).unwrap();
        assert!(s.remove("a"));
        assert!(!s.remove("a"));
        assert!(s.is_empty());
    }

    #[test]
    fn search_prefers_term_prefix_over_body_match() {
        let mut s = GlossaryStore::new();
        s.upsert(term("a", "税务", "地方报税说明。", &[])).unwrap();
        s.upsert(term("b", "报销单", "税务相关单据。", &[])).unwrap();
        let hits = s.search("税务", 2);
        assert_eq!(hits[0].term, "税务", "词名前缀命中应排第一");
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn search_empty_query_returns_nothing() {
        let mut s = GlossaryStore::new();
        s.upsert(term("a", "税务", "x", &[])).unwrap();
        assert!(s.search("", 5).is_empty());
        assert!(s.search("  ", 5).is_empty());
    }

    #[test]
    fn search_no_match_is_empty() {
        let mut s = GlossaryStore::new();
        s.upsert(term("a", "税务", "x", &[])).unwrap();
        assert!(s.search("量子物理", 5).is_empty());
    }

    #[test]
    fn tag_counts_aggregates() {
        let mut s = GlossaryStore::new();
        s.upsert(term("a", "税务", "x", &["财务", "每月"])).unwrap();
        s.upsert(term("b", "报销", "x", &["财务"])).unwrap();
        let counts = s.tag_counts();
        assert_eq!(counts.get("财务"), Some(&2));
        assert_eq!(counts.get("每月"), Some(&1));
    }

    #[test]
    fn store_round_trips_through_json() {
        let mut s = GlossaryStore::new();
        s.upsert(term("a", "税务", "申报。", &["财务"])).unwrap();
        let blob = serde_json::to_string(&s).unwrap();
        let back: GlossaryStore = serde_json::from_str(&blob).unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(back.get("a").unwrap().term, "税务");
    }
}