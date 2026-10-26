//! 统一错误类型。
//!
//! 这一层的目标不是「把 reqwest 的错误包一层」，而是保证三件事：
//!
//! 1. **拿得到分类**。调用方要能区分「没配 key」（该弹设置面板）和
//!    「401」（该让用户检查 key）和「超时」（该重试）——全都返回一个
//!    字符串的话，每家用起来又得自己 `contains("401")` 猜。
//! 2. **不泄露密钥**。上游网关有时会把请求头回显进错误体；把 body
//!    原样带出来的实现（file 仓旧版就是这样）等于把 key 写进日志。
//! 3. **不 panic**。旧实现用 `&text[..text.len().min(300)]` 截断错误体，
//!    遇到中文响应会切在字符中间直接 panic —— 报错误的过程自己崩了。
//!
//! 所以本文件的两个函数 `truncate_chars` / `redact` 是所有错误文本的必经之路。

use std::fmt;

/// 错误分类。调用方按它决定「提示用户」还是「自动重试」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// 配置不全（缺 baseUrl / 缺 key / 缺 model）。属于「用户要去设置面板」。
    NotConfigured,
    /// 连不上（DNS / 拒绝连接 / TLS 握手失败）。属于「检查网络或 baseUrl」。
    Network,
    /// 请求超时。可重试。
    Timeout,
    /// 服务端返回非 2xx。`status` 字段带具体码。
    Http,
    /// 响应体不是预期结构（含流式里的坏帧）。
    Parse,
    /// 该 provider 不支持这个动作（例如给 Gemini 要 models 列表的某些形态）。
    Unsupported,
    /// 传输层以外的 IO（读配置、建客户端）。
    Io,
}

impl ErrorKind {
    /// 是否值得自动重试。**只有超时与网络抖动值得** —— 401 重试一百次还是 401，
    /// 而 429 该按 Retry-After 退避（属调用方策略，这里只标「可重试」）。
    pub fn is_retryable(self) -> bool {
        matches!(self, ErrorKind::Timeout | ErrorKind::Network)
    }
}

/// 统一错误。
#[derive(Debug, Clone)]
pub struct LlmError {
    pub kind: ErrorKind,
    /// HTTP 状态码（仅 `ErrorKind::Http`）
    pub status: Option<u16>,
    /// 已脱敏、已按字符截断的可读消息
    pub message: String,
}

impl LlmError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            status: None,
            message: message.into(),
        }
    }

    pub fn http(status: u16, message: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::Http,
            status: Some(status),
            message: message.into(),
        }
    }

    pub fn not_configured(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::NotConfigured, message)
    }

    pub fn parse(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Parse, message)
    }
}

impl fmt::Display for LlmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.status {
            Some(code) => write!(f, "HTTP {} — {}", code, self.message),
            None => write!(f, "{}", self.message),
        }
    }
}

impl std::error::Error for LlmError {}

/// 按**字符**（不是字节）截断，超长补省略号。
///
/// 旧实现在这里踩过坑：`&text[..text.len().min(300)]` 对纯 ASCII 没问题，
/// 但 MiniMax / DeepSeek 的错误体通常是中文 JSON，`300` 很可能落在一个
/// 三字节汉字的中间 —— Rust 的字符串切片要求落在字符边界，否则直接 panic。
/// 于是「调用失败」会变成「调用失败且进程崩了」，日志里连失败原因都留不下。
pub fn truncate_chars(s: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i >= max_chars {
            out.push('…');
            return out;
        }
        out.push(ch);
    }
    out
}

/// 把密钥从文本里抹掉。
///
/// 不只替换完整 key：短于 8 字符的 key 一律按「疑似占位符」处理（避免把
/// `sk-1` 这种误伤成无关文本）；同时掩盖 key 的**前 4 位之后**——因为
/// 有些网关只回显前缀。
pub fn redact(text: &str, api_key: &str) -> String {
    if api_key.is_empty() || api_key.len() < 8 {
        return text.to_string();
    }
    if text.contains(api_key) {
        return text.replace(api_key, "sk-***REDACTED***");
    }
    // 网关有时只回显前几位
    let head: String = api_key.chars().take(6).collect();
    if head.len() >= 6 && text.contains(&head) {
        return text.replace(&head, "sk-***");
    }
    text.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_cuts_on_char_boundary() {
        // 100 个三字节汉字，按字符截到 5 个 —— 绝不能 panic
        let s = "中".repeat(100);
        let out = truncate_chars(&s, 5);
        assert_eq!(out.chars().count(), 6); // 5 + 省略号
        assert!(out.starts_with("中中中中中"));
    }

    #[test]
    fn truncate_keeps_short_text_intact() {
        assert_eq!(truncate_chars("好的", 10), "好的");
    }

    #[test]
    fn truncate_handles_empty() {
        assert_eq!(truncate_chars("", 5), "");
    }

    #[test]
    fn redact_removes_full_key() {
        let key = "sk-abcdefghijklmnop";
        let got = redact(&format!("bad key {key} rejected"), key);
        assert!(!got.contains(key));
        assert!(got.contains("REDACTED"));
    }

    #[test]
    fn redact_removes_prefix_echo() {
        let key = "sk-abcdefghijklmnop";
        let got = redact("your key sk-abcd is invalid", key);
        assert!(!got.contains("sk-abcd"));
    }

    #[test]
    fn redact_ignores_short_key() {
        // 少于 8 字符的不处理，避免误伤正常文本
        assert_eq!(redact("abc def", "abc"), "abc def");
    }

    #[test]
    fn retryable_only_for_transient() {
        assert!(ErrorKind::Timeout.is_retryable());
        assert!(ErrorKind::Network.is_retryable());
        assert!(!ErrorKind::Http.is_retryable());
        assert!(!ErrorKind::NotConfigured.is_retryable());
    }
}
