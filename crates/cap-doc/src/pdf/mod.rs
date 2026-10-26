//! PDF 原语族。
//!
//! 六个子模块的依赖方向是单向的，搬迁时未改动拓扑：
//!
//! ```text
//! font   ← text          （文本抽取要按字体取 ToUnicode 映射）
//! image  ← compress      （压缩要解 XObject 里的图）
//! ops    ← text, compress, watermark   （三者都要 load_doc / save_doc / 继承属性）
//! ```
//!
//! 因此只能自底向上引用：`ops` 不知道 `text` 存在，`compress` 不知道 `watermark` 存在。

pub mod compress;
pub mod font;
pub mod image;
pub mod ops;
pub mod text;
pub mod watermark;
