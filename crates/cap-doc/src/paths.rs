//! 路径归一化 —— **不是**安全边界。
//!
//! 与 `cap-img` 的既有约定一致（见其 `lib.rs` 头注）：读黑名单 / 写黑名单 /
//! 敏感目录拒绝属于**调用方**的策略（例如 `z-biz-tool-file/src-tauri/src/path_guard.rs`），
//! 不在原语层实现。原语层不知道调用方的安全策略，也不该知道。
//!
//! 这里只做两件事：把 `&str` 转成 `PathBuf`，以及对输入侧补一条「文件存在」的
//! 基本可用性检查（否则 lopdf / zip 会抛出更难读的底层错误）。
//!
//! 返回 `Result` 是为了让调用点的 `?` 保持不变 —— 搬迁前后调用形态一致，
//! 便于逐行对账。将来若某个调用方需要更强的校验，应在自己的包装层做。

use std::path::PathBuf;

/// 输入路径归一化。只校验「存在」，不校验是否在敏感目录。
pub(crate) fn input(raw: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(raw);
    if !path.exists() {
        return Err(format!("文件不存在: {}", raw));
    }
    Ok(path)
}

/// 输出路径归一化。不建父目录、不校验写权限 —— 由调用方或 `save_doc` 自行处理。
pub(crate) fn output(raw: &str) -> Result<PathBuf, String> {
    Ok(PathBuf::from(raw))
}
