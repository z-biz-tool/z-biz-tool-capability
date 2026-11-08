//! # cap-glossary — 通用词库能力层
//!
//! 家族里「知识库 / 词库」（kb）作为**通用板块**由本 crate 承载，而不是被
//! 某个 app 吞掉。一个词库能力包含四块，各 host（note、file 等）按需取用：
//!
//! | 模块 | 干什么 | 谁在用 |
//! |---|---|---|
//! | [`chunk`] | 把一篇 Markdown 切成可检索、可引用、可回跳的片段 | note 问笔记、file 词库 |
//! | [`bm25`] | 中文「一元+相邻二元」BM25 检索与排序 | 同上 |
//! | [`rag`] | 检索 → 带引用编号的问答提示组装（含兜底话术） | note 问笔记 / file 问答 |
//! | [`glossary`] | 通用词条存取：增删改查 + 检索（可嵌入任意 host） | file 词库板块 |
//!
//! ## 设计约定
//!
//! - **本层是纯算法 / 纯数据模型**：不读写盘、不碰网络、不依赖 tokio/reqwest，
//!   保证可以脱库脱网跑单测。落盘与缓存由 host 负责（见 file 的 `glossary_app`）。
//! - **能力来自 kb 仓嫁接到 note 的那套**（`note/src-tauri/src/rag/`），先把验证过
//!   的结论沉淀成 crate，note 后续某一轮再切换依赖、去掉本地副本。
//! - 检索质量三条不可退的底线：中文切开词、稀有词值钱、同分稳定排序。
//!
//! ## host 要自己做的
//!
//! 片段的**持久化**（存哪里、怎么缓存失效）、**前端 UI**、**LLM 鉴权与流式**
//! （那属于 [`cap-llm`](https://crates.io/crates/cap-llm) 的边界）都不在本层。
//! 本层只保证「同一段检索逻辑，note 与 file 结果一致」。

pub mod bm25;
pub mod chunk;
pub mod glossary;
pub mod rag;

pub use bm25::{normalize_scores, tokenize, Bm25Index};
pub use chunk::{chunk_markdown, Chunk, DEFAULT_MAX_CHARS};
pub use glossary::{GlossaryStore, GlossaryTerm};
pub use rag::{
    build_user_message, no_evidence_message, to_citations, Citation, RagStatus, DEFAULT_TOP_K,
    SYSTEM_PROMPT,
};