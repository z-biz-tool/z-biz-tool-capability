//! 门面行为测试：验证 `lib.rs` 暴露的公共 API，而不是内部实现。
//!
//! 模块内测试覆盖解析细节（76 条），这里只钉三件下游真正依赖的事：
//! 扩展名判定、缺失文件的错误措辞、以及 `Result<_, String>` 这一错误类型契约。

use std::path::Path;

#[test]
fn supported_extensions_cover_the_three_parsers() {
    assert_eq!(cap_doc::SUPPORTED_EXTENSIONS, &["pdf", "epub", "mobi"]);
}

#[test]
fn supports_is_case_insensitive_and_ignores_directories() {
    assert!(cap_doc::supports("a.pdf"));
    assert!(cap_doc::supports("A.PDF"));
    assert!(cap_doc::supports("/tmp/书.epub"));
    assert!(cap_doc::supports(Path::new("x.mobi")));

    // 无扩展名 / 不认识的扩展名 / 只有目录名 —— 都不该误判
    assert!(!cap_doc::supports("README"));
    assert!(!cap_doc::supports("a.docx"), "docx 由 file 自己处理，不在本层");
    assert!(!cap_doc::supports("/tmp/pdf/"), "目录名里的 pdf 不算扩展名");
}

#[test]
fn missing_file_reports_the_path_it_could_not_find() {
    let missing = "/tmp/cap-doc-definitely-not-here-9f3a.pdf";
    let err = cap_doc::get_pdf_pages(missing).unwrap_err();
    assert!(
        err.contains(missing),
        "错误串要带原路径，便于调用方透传给用户；实际: {err}"
    );
}

#[test]
fn epub_and_pdf_share_the_same_error_type() {
    // 契约：全部公开函数返回 String 错误，调用方不需要分辨错误种类。
    // 这里用类型标注把契约钉在编译期。
    let _: fn(&str) -> Result<cap_doc::EpubBook, String> = cap_doc::parse_epub;
    let _: fn(&str) -> Result<cap_doc::PdfMeta, String> = cap_doc::get_pdf_metadata;
    let _: fn(&str) -> Result<Vec<cap_doc::PdfPageInfo>, String> = cap_doc::get_pdf_pages;
    let _: fn(&str) -> Result<String, String> = cap_doc::extract_pdf_text;
    let _: fn(&str) -> Result<String, String> = cap_doc::get_epub_cover;
}
