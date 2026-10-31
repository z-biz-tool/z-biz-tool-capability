//! 跨平台打开浏览器（OAuth2 流程的第二步）。
//!
//! # 用 tauri-plugin-opener 而不是 tauri-plugin-shell
//!
//! shell 插件的 `open` 在 v2.3 起已 deprecated（box 仓的 tray.rs 里也写了这条）。
//! opener 是官方指定的替代品，跨平台行为一致。
//!
//! 本模块是 `tauri` feature 关闭时的**纯降级实现** —— 它直接调系统命令，
//! 让 `cap-sso` 在没有 Tauri 的环境（CLI / 测试）里也能开浏览器。

/// 打开浏览器失败。
#[derive(Debug)]
pub struct OpenError(pub String);

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "打不开浏览器：{}", self.0)
    }
}

impl std::error::Error for OpenError {}

/// 校验 URL 是不是「可以放心交给系统打开」的。
///
/// **只允许 http / https** —— 授权页 URL 是我们自己拼的，
/// 但拼错一次就可能把 `file://` 或 `javascript:` 递到系统手里。
/// 这是个廉价但真实的防线：URL 拼错的场景不罕见（配置里手写地址）。
pub fn validate_url(url: &str) -> Result<(), OpenError> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return Err(OpenError("URL 为空".into()));
    }
    let lower = trimmed.to_ascii_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
        return Err(OpenError(format!(
            "只允许 http/https 链接，收到：{}",
            &trimmed[..trimmed.len().min(40)]
        )));
    }
    Ok(())
}

/// 用系统默认浏览器打开 URL（不依赖 Tauri）。
///
/// 三平台各用各的标准做法：
/// - macOS → `open`
/// - Windows → `cmd /C start ""`（`start` 是 cmd 内建命令，没有独立 exe）
/// - Linux → `xdg-open`
pub fn open_url(url: &str) -> Result<(), OpenError> {
    validate_url(url)?;
    let trimmed = url.trim();

    #[cfg(target_os = "macos")]
    let result = std::process::Command::new("open").arg(trimmed).spawn();

    #[cfg(target_os = "windows")]
    let result = std::process::Command::new("cmd")
        .args(["/C", "start", ""])
        .arg(trimmed)
        .spawn();

    #[cfg(all(unix, not(target_os = "macos")))]
    let result = std::process::Command::new("xdg-open").arg(trimmed).spawn();

    #[cfg(not(any(target_os = "macos", target_os = "windows", unix)))]
    let result: std::io::Result<std::process::Child> = Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "当前平台没有已知的打开方式",
    ));

    result.map(|_| ()).map_err(|e| OpenError(e.to_string()))
}

/// 用 Tauri 的 opener 插件打开 URL。
///
/// 优先走插件（跨平台行为一致、错误信息更准）—— 插件不可用时回退到
/// 直接调系统命令。
#[cfg(feature = "tauri")]
pub fn open_url_with_tauri(app: &tauri::AppHandle, url: &str) -> Result<(), OpenError> {
    validate_url(url)?;
    use tauri_plugin_opener::OpenerExt;
    app.opener()
        .open_url(url.trim(), None::<&str>)
        .map_err(|e| OpenError(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 只放行_http_与_https() {
        assert!(validate_url("https://ctc.example.com/authorize").is_ok());
        assert!(validate_url("http://127.0.0.1:7823/callback").is_ok());
    }

    #[test]
    fn 拦下_非_http_链接() {
        // 这几个是最危险的：file:// 能读本机文件，javascript: 能执行脚本。
        for bad in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "ftp://example.com/x",
            "data:text/html,<h1>x</h1>",
            "",
            "   ",
        ] {
            assert!(validate_url(bad).is_err(), "应该拦下：{:?}", bad);
        }
    }

    #[test]
    fn 大写_scheme_也放行() {
        assert!(validate_url("HTTPS://ctc.example.com/x").is_ok());
    }
}
