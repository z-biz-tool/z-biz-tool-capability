//! BM25 检索。
//!
//! 移植自 note 仓的 `rag/bm25.rs`（原实现继承自 kb 仓的 `rag.rs`），三处关键设计
//! 不能退回：
//!
//! 1. **中文切词用「一元 + 相邻二元」**。FTS5 的 unicode61 分词器把「我的笔记」
//!    当一个 token，中文查询恒 0 命中；单纯给汉字插空格又会让 IDF 被常用字拉平。
//! 2. **稀有词值钱**。BM25 的 IDF 就是为「安装」「使用」这类泛词把无关片段
//!    顶上来这件事设计的。
//! 3. **同分按 (路径, 序号) 稳定排序**。否则同样的问题两次给出不同证据，
//!    用户会以为是自己记错了。
//!
//! 本层是纯函数：入参片段数组，出参下标与分数。

use crate::chunk::Chunk;
use std::collections::HashMap;

const K1: f64 = 1.5;
const B: f64 = 0.75;

fn is_cjk(c: char) -> bool {
    matches!(c, '\u{3400}'..='\u{4dbf}' | '\u{4e00}'..='\u{9fff}' | '\u{f900}'..='\u{faff}')
}

/// 切词：CJK 连续段取一元 + 相邻二元，字母数字段取小写整词（≥2 字符）。
///
/// gram 只在同一段内部取 —— 跨标点取二元会造出「装a」这种哪儿都能匹配的垃圾词。
/// 中文保留一元，是为了让一个字的问题（「税」）也能有命中。
pub fn tokenize(text: &str) -> Vec<String> {
    let mut terms = Vec::new();
    let mut cjk_run: Vec<char> = Vec::new();
    let mut ascii_run = String::new();

    fn flush_cjk(run: &mut Vec<char>, terms: &mut Vec<String>) {
        for (i, c) in run.iter().enumerate() {
            terms.push(c.to_string());
            if i + 1 < run.len() {
                terms.push(format!("{}{}", c, run[i + 1]));
            }
        }
        run.clear();
    }
    fn flush_ascii(run: &mut String, terms: &mut Vec<String>) {
        if run.chars().count() >= 2 {
            terms.push(run.to_lowercase());
        }
        run.clear();
    }

    for c in text.chars() {
        if is_cjk(c) {
            flush_ascii(&mut ascii_run, &mut terms);
            cjk_run.push(c);
        } else if c.is_alphanumeric() {
            flush_cjk(&mut cjk_run, &mut terms);
            ascii_run.push(c);
        } else {
            flush_cjk(&mut cjk_run, &mut terms);
            flush_ascii(&mut ascii_run, &mut terms);
        }
    }
    flush_cjk(&mut cjk_run, &mut terms);
    flush_ascii(&mut ascii_run, &mut terms);
    terms
}

/// 索引好的片段集合
pub struct Bm25Index {
    chunks: Vec<Chunk>,
    postings: HashMap<String, Vec<(usize, usize)>>,
    /// 每片的词数（不是字符数、更不是字节数：拿字节数当长度会让中文系统性
    /// 被罚三倍，这是 kb 修过的 bug）
    doc_len: Vec<f64>,
    avg_len: f64,
}

impl Bm25Index {
    pub fn build(chunks: Vec<Chunk>) -> Bm25Index {
        let mut postings: HashMap<String, Vec<(usize, usize)>> = HashMap::new();
        let mut doc_len = Vec::with_capacity(chunks.len());

        for (i, chunk) in chunks.iter().enumerate() {
            let haystack = format!("{} {}\n{}", chunk.heading, chunk.title, chunk.text);
            let terms = tokenize(&haystack);
            doc_len.push(terms.len() as f64);

            let mut local: HashMap<&str, usize> = HashMap::new();
            for t in &terms {
                *local.entry(t.as_str()).or_insert(0) += 1;
            }
            for (t, tf) in local {
                postings.entry(t.to_string()).or_default().push((i, tf));
            }
        }

        let avg_len = if chunks.is_empty() {
            0.0
        } else {
            doc_len.iter().sum::<f64>() / chunks.len() as f64
        };

        Bm25Index {
            chunks,
            postings,
            doc_len,
            avg_len,
        }
    }

    pub fn chunks(&self) -> &[Chunk] {
        &self.chunks
    }

    /// 检索 top_n。`only_path` 给了就只在那一篇里找（「问当前笔记」）。
    pub fn search(
        &self,
        question: &str,
        top_n: usize,
        only_path: Option<&str>,
    ) -> Vec<(usize, f64)> {
        let mut q_terms = tokenize(question);
        q_terms.sort();
        q_terms.dedup();
        if q_terms.is_empty() || self.chunks.is_empty() {
            return vec![];
        }

        let n = self.chunks.len() as f64;
        let avg_len = self.avg_len.max(1.0);
        let mut scores = vec![0.0f64; self.chunks.len()];
        let mut matched = false;

        for term in q_terms {
            let Some(posting) = self.postings.get(&term) else {
                continue;
            };
            matched = true;
            let df = posting.len() as f64;
            let idf = ((n - df + 0.5) / (df + 0.5) + 1.0).ln();
            for (doc, tf) in posting {
                if let Some(p) = only_path {
                    if self.chunks[*doc].path != p {
                        continue;
                    }
                }
                let tf = *tf as f64;
                let dl = self.doc_len[*doc].max(1.0);
                let norm = tf * (K1 + 1.0) / (tf + K1 * (1.0 - B + B * (dl / avg_len)));
                scores[*doc] += idf * norm;
            }
        }
        if !matched {
            return vec![];
        }

        let mut ranked: Vec<(usize, f64)> = scores
            .iter()
            .enumerate()
            .filter(|(_, s)| **s > 0.0)
            .map(|(i, s)| (i, *s))
            .collect();
        ranked.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| {
                    let la = &self.chunks[a.0];
                    let lb = &self.chunks[b.0];
                    (la.path.as_str(), la.idx).cmp(&(lb.path.as_str(), lb.idx))
                })
        });
        ranked.truncate(top_n);
        ranked
    }
}

/// 把一次检索结果按本次最高分归一化到 (0, 1]。
///
/// BM25 分数没有上界、也不是概率。前端要显示的是「相对相关度」：第一名为 100%，
/// 其余按比例 —— 不能乘 100 说成「相关度 87%」，那是编的数字。
pub fn normalize_scores(hits: &mut [(usize, f64)]) {
    let max = hits.iter().map(|(_, s)| *s).fold(0.0f64, f64::max);
    if max <= 0.0 || !max.is_finite() {
        return;
    }
    for (_, score) in hits.iter_mut() {
        *score = (*score / max).clamp(0.0, 1.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunk::chunk_markdown;

    fn mk(path: &str, title: &str, body: &str) -> Vec<Chunk> {
        chunk_markdown(path, title, body, 600)
    }

    #[test]
    fn tokenize_splits_cjk_into_unigrams_and_bigrams() {
        let t = tokenize("安装依赖");
        assert!(t.contains(&"安".to_string()));
        assert!(t.contains(&"安装".to_string()));
        assert!(t.contains(&"依赖".to_string()));
        assert!(t.contains(&"装依".to_string()));
    }

    #[test]
    fn tokenize_keeps_latin_words_lowercased() {
        let t = tokenize("SQLite FTS5 a");
        assert!(t.contains(&"sqlite".to_string()));
        assert!(t.contains(&"fts5".to_string()));
        assert!(!t.contains(&"a".to_string()), "单字母不建索引");
    }

    #[test]
    fn tokenize_does_not_cross_punctuation() {
        let t = tokenize("装，a");
        assert!(!t.iter().any(|x| x.contains('，')), "{:?}", t);
        assert!(!t.contains(&"装a".to_string()));
    }

    #[test]
    fn chinese_question_ranks_related_chunk_first() {
        let chunks = mk(
            "/a.md",
            "部署笔记",
            "# 部署\n\n服务器上要先装依赖再重启服务。\n\n# 其他\n\n今天天气不错，出去走走。\n",
        );
        let idx = Bm25Index::build(chunks);
        let hits = idx.search("装依赖", 2, None);
        assert!(!hits.is_empty());
        assert!(idx.chunks()[hits[0].0].text.contains("依赖"), "应该先命中部署那段");
    }

    #[test]
    fn rare_word_beats_common_word() {
        let chunks = mk(
            "/a.md",
            "安装",
            "# 安装\n\n安装很简单。\n\n# 证书\n\n安装证书要重启。\n",
        );
        let idx = Bm25Index::build(chunks);
        let hits = idx.search("证书", 2, None);
        assert!(!hits.is_empty());
        assert!(idx.chunks()[hits[0].0].text.contains("证书"));
    }

    #[test]
    fn scope_limits_to_one_note() {
        let mut chunks = mk("/a.md", "A", "# A\n\n苹果派的做法。\n");
        chunks.extend(mk("/b.md", "B", "# B\n\n苹果派的做法，另一份。\n"));
        let idx = Bm25Index::build(chunks);
        let hits = idx.search("苹果派", 5, Some("/b.md"));
        assert_eq!(hits.len(), 1);
        assert_eq!(idx.chunks()[hits[0].0].path, "/b.md");
    }

    #[test]
    fn no_match_returns_empty_not_error() {
        let idx = Bm25Index::build(mk("/a.md", "A", "# A\n\n苹果。\n"));
        assert!(idx.search("量子力学", 3, None).is_empty());
        assert!(idx.search("", 3, None).is_empty());
    }

    #[test]
    fn ties_are_stable_across_calls() {
        let mut chunks = mk("/a.md", "A", "# A\n\n同样的内容。\n");
        chunks.extend(mk("/b.md", "B", "# B\n\n同样的内容。\n"));
        let idx = Bm25Index::build(chunks);
        let first = idx.search("同样的内容", 2, None);
        for _ in 0..5 {
            assert_eq!(idx.search("同样的内容", 2, None), first, "同分排序必须稳定");
        }
    }

    #[test]
    fn normalize_makes_top_hit_one() {
        let mut hits = vec![(0usize, 12.0), (1, 6.0)];
        normalize_scores(&mut hits);
        assert!((hits[0].1 - 1.0).abs() < 1e-9);
        assert!((hits[1].1 - 0.5).abs() < 1e-9);
    }

    #[test]
    fn normalize_is_safe_on_zero_scores() {
        let mut hits = vec![(0usize, 0.0)];
        normalize_scores(&mut hits);
        assert_eq!(hits[0].1, 0.0, "全 0 不能除出 NaN");
    }
}