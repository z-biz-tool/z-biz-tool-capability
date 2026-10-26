//! cap-doc — 文档处理原语。
//!
//! 从 `z-biz-tool-file/src-tauri/src/` 原样搬迁（矩阵规划 03 §7 GAP-06 的文档一片）：
//! **路径进 → 结构出**的纯函数，不持状态、不建资产库、不调模型、不起外部进程。
//!
//! ## 与 file 的边界
//!
//! 搬迁只做了三件事，语义零漂移：
//!
//! 1. 删掉 11 处 `#[tauri::command]` —— 命令边界属于调用方，原语层不认 Tauri。
//! 2. `crate::path_guard::*` 换成 [`paths`] 的归一化（**只查存在性，不做安全校验**）。
//!    读黑名单/写黑名单/敏感目录拒绝仍由 `file` 的 `path_guard` 在命令层执行 ——
//!    与 `cap-img` 头注确立的约定一致。
//! 3. 测试脚手架 `test_bridge::TempDir` 内移为 `test_support::TempDir`（实现原样保留）。
//!
//! ## 不属于本 crate 的
//!
//! - `office.rs`（调 LibreOffice 外部进程）、`ocr.rs`（调 tesseract）留在 file ——
//!   它们不是「路径进结构出」的原语，是**外部工具的编排**，进能力层会把工具依赖
//!   传染给所有下游。
//! - `convert.rs`（md→html/pdf/epub/mobi 的导出）也留在 file —— 它是导出工序，
//!   不是解析原语。
//!
//! ## 给调用方的建议
//!
//! 需要「一次调用拿到文档全貌」的下游（kb 建索引、note 做导入预览、aigen 读参考文档），
//! 组合 [`get_pdf_pages`] + [`extract_pdf_text`] + [`get_pdf_metadata`]，或 EPUB 走
//! [`parse_epub`]。这四个都是只读的，互不依赖，可以并发。

mod paths;

#[cfg(test)]
mod test_support;

pub mod ebook;
pub mod pdf;

pub use ebook::{get_epub_cover, parse_epub, parse_mobi, Chapter, EpubBook};
pub use pdf::compress::{compress_pdf_blocking, CompressReport};
pub use pdf::image::{extract_pdf_images_blocking, ExtractReport};
pub use pdf::ops::{get_pdf_pages, merge_pdfs, split_pdf, PdfPageInfo};
pub use pdf::text::{extract_pdf_text, get_pdf_metadata, PdfMeta};
pub use pdf::watermark::watermark_pdf_blocking;

/// 本 crate 认得的文档扩展名（小写，不含点）。
///
/// 供调用方做「这个文件我能不能处理」的前置判断，避免每个下游各写一份表 ——
/// 那份表早晚和实际实现长歪。
pub const SUPPORTED_EXTENSIONS: &[&str] = &["pdf", "epub", "mobi"];

/// 按扩展名判断是否可处理（大小写不敏感）。
pub fn supports(path: impl AsRef<std::path::Path>) -> bool {
    path.as_ref()
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .map(|e| SUPPORTED_EXTENSIONS.contains(&e.as_str()))
        .unwrap_or(false)
}
