//! 语音转写（ASR）执行层。
//!
//! 与 [`crate::chat`] 是**同一类传输**：同一个 `shared_client()`、同一套
//! 鉴权头、同一个 [`http_error`] 的脱敏/截断出口。差别只在报文形状 ——
//! 这里是 `multipart/form-data`，chat 是 JSON。
//!
//! ## 为什么归到这一层
//!
//! 会议纪要要「音频 → 文字」，而 multipart 上传、状态码映射、密钥脱敏、
//! 超时落地、错误分类全是传输细节。留在业务仓里意味着 note / mind /
//! 将来的会议纪要壳各自再写一遍，正是这个 crate 存在的理由。
//!
//! ## 边界：本层只做转写，不做「转写 → 纪要」
//!
//! 「把散乱的口语文本整理成议题/决议/待办」是**提示词**，属于产品差异
//! （见 lib.rs 的「不做」清单），由 `cap-meeting` 这类业务层提供。
//! 本层只保证「音频进、文字出、错误可分类」。
//!
//! ## 只支持 OpenAI 兼容系
//!
//! 见 [`crate::config::Action::Transcription`] 的说明。其余三家在拼 URL 时
//! 就返回 `ErrorKind::Unsupported`，不会发出必然 404 的请求。

use std::path::Path;
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::config::{Action, LlmConfig};
use crate::error::{redact, truncate_chars, LlmError};

/// OpenAI `verbose_json` 响应里可取到的分段。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Segment {
    /// 段在音频里的起始秒数
    pub start: f64,
    /// 段在音频里的结束秒数
    pub end: f64,
    /// 该段文字
    pub text: String,
}

/// 一次转写的结果。
///
/// 空文本是**合法结果**（音频里确实没说话，或全是背景音），不是错误。
/// 这与「没转成」要区分：后者返回 [`LlmError`]。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Transcript {
    /// 转写全文。
    pub text: String,
    /// 服务端识别出的语言（`verbose_json` 才有）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// 音频时长秒（`verbose_json` 才有）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_secs: Option<f64>,
    /// 分段（`verbose_json` 且分段数在窗口内才有）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub segments: Vec<Segment>,
    /// 服务端回报的模型名
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// 实际耗时（含网络与上传）。与 [`crate::ChatReply::latency`] 同义。
    #[serde(skip)]
    pub latency: std::time::Duration,
}

/// 转写请求参数。
#[derive(Debug, Clone)]
pub struct TranscriptionRequest {
    /// 转写模型名，例如 `whisper-1` / `whisper-large-v3`。
    ///
    /// **不复用 `LlmConfig::model`**：那个字段是对话模型（chat），
    /// 同一个 provider 的对话模型与转写模型几乎从不相同。复用会让
    /// 「配了 DeepSeek 就能转写」这类错觉发生，然后撞上一个 400。
    pub model: String,
    /// 音频语言（ISO-639-1，如 `zh` / `en`）。不填由服务端自动检测。
    ///
    /// 提高准确率最有效的一件事就是填它 —— 自动检测在中英混说时
    /// 经常中途漂移。
    pub language: Option<String>,
    /// 提示词。用于纠正词表（产品名、人名、专业术语）。
    pub prompt: Option<String>,
    /// 采样温度 0.0–1.0。0 = 贪心解码，结果最稳定可复现。
    pub temperature: Option<f32>,
    /// 是否要分段与语言信息。
    ///
    /// 默认 `true`：只要服务端支持 `verbose_json` 就用它，拿不到分段再回落
    /// 到 `json`（只有全文）。见 [`transcribe`]。
    pub verbose: bool,
}

impl Default for TranscriptionRequest {
    fn default() -> Self {
        Self {
            model: String::new(),
            language: None,
            prompt: None,
            temperature: None,
            // 要分段是常态需求（会议纪要按人/按话题切分），
            // 所以默认开着；不需要时调用方显式关。
            verbose: true,
        }
    }
}

/// 响应体。`verbose_json` 与 `json` 都能落到这个结构上 —— 缺的字段就是 None。
#[derive(Debug, Clone, Default, Deserialize)]
struct WireResponse {
    #[serde(default)]
    text: String,
    #[serde(default)]
    language: Option<String>,
    #[serde(default)]
    duration: Option<f64>,
    #[serde(default)]
    segments: Option<Vec<WireSegment>>,
}

#[derive(Debug, Clone, Deserialize)]
struct WireSegment {
    #[serde(default)]
    start: Option<f64>,
    #[serde(default)]
    end: Option<f64>,
    #[serde(default)]
    text: String,
}

/// 按扩展名猜 MIME。猜不出来就用 `application/octet-stream` —— 服务端
/// 多数能自己判断，但明确给出能提高成功率。
fn guess_mime(filename: &str) -> &'static str {
    let ext = Path::new(filename)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "mp3" => "audio/mpeg",
        "mp4" | "m4a" => "audio/mp4",
        "mpeg" | "mpga" => "audio/mpeg",
        "wav" => "audio/wav",
        "webm" => "audio/webm",
        "ogg" | "oga" => "audio/ogg",
        "flac" => "audio/flac",
        "aac" => "audio/aac",
        _ => "application/octet-stream",
    }
}

/// 转写音频字节。
///
/// `filename` 只用于两件事：猜 MIME 类型、以及某些网关会据此决定解码器。
/// 不会读盘 —— 字节已经在这里了。
///
/// # 返回
///
/// [`Transcript`]。`text` 为空是合法结果（音频里没有可识别的语音）。
///
/// # 错误
///
/// - `ErrorKind::NotConfigured` —— 未填转写模型名，或 provider 缺 key/地址
/// - `ErrorKind::Unsupported` —— 该 provider 没有转写端点（非 OpenAI 兼容系）
/// - `ErrorKind::Network` / `Timeout` —— 可重试（`ErrorKind::is_retryable()`）
/// - `ErrorKind::Http` —— 401/429/413（音频过大）等
/// - `ErrorKind::Parse` —— 响应不是预期结构
pub async fn transcribe(
    cfg: &LlmConfig,
    audio: Vec<u8>,
    filename: &str,
    req: &TranscriptionRequest,
) -> Result<Transcript, LlmError> {
    transcribe_inner(cfg, audio, filename, req).await
}

/// 转写本地文件。
///
/// 读盘失败是 `ErrorKind::Io` —— 与网络/解析错误分开，调用方不必
/// 对「文件不在了」做无用重试。
pub async fn transcribe_file(
    cfg: &LlmConfig,
    path: impl AsRef<Path>,
    req: &TranscriptionRequest,
) -> Result<Transcript, LlmError> {
    let path = path.as_ref();
    let bytes = std::fs::read(path).map_err(|e| {
        LlmError::new(
            crate::error::ErrorKind::Io,
            format!("读取音频文件失败 {}：{}", path.display(), e),
        )
    })?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("audio.wav")
        .to_string();
    transcribe_inner(cfg, bytes, &name, req).await
}

/// 真正发请求。与 `chat` 共用 `shared_client` / `auth_headers` / `http_error`。
async fn transcribe_inner(
    cfg: &LlmConfig,
    audio: Vec<u8>,
    filename: &str,
    req: &TranscriptionRequest,
) -> Result<Transcript, LlmError> {
    // 先在本地把「没配模型」挡下。转写模型名是必填的（不能拿对话模型顶），
    // 而 cfg.validate() 检查的是对话模型 —— 那个字段在这里没用。
    if req.model.trim().is_empty() {
        return Err(LlmError::not_configured(
            "未填写转写模型名（如 whisper-1），无法发起音频转写",
        ));
    }
    // provider 级校验（key / 地址）复用 chat 的那套，错误文案一致。
    cfg.validate()?;
    let url = cfg.endpoint(Action::Transcription)?;

    // verbose_json 能拿到语言/时长/分段；部分网关不支持，回落到 json。
    let mut tried_verbose = false;
    if req.verbose {
        tried_verbose = true;
    }

    let started = Instant::now();
    let send = |f: reqwest::multipart::Form| {
        let mut request = crate::chat::client()
            .post(&url)
            .timeout(cfg.timeout)
            .multipart(f);
        for (k, v) in crate::wire::auth_headers(cfg) {
            request = request.header(k, v);
        }
        if crate::wire::needs_query_key(cfg) {
            request = request.query(&[("key", cfg.api_key.as_str())]);
        }
        request
    };

    let mut resp = send(build_form(audio.clone(), filename, req, req.verbose))
        .send()
        .await
        .map_err(|e| crate::chat::transport_error(cfg, e))?;

    // verbose_json 不被支持 → 用 json 再试一次。
    // 只在 400 + 明确提到 response_format 时回落，别的 400 照常报错。
    if tried_verbose && resp.status().as_u16() == 400 {
        let peek = resp.text().await.unwrap_or_default();
        let lower = peek.to_ascii_lowercase();
        if lower.contains("response_format") || lower.contains("verbose_json") {
            resp = send(build_form(audio.clone(), filename, req, false))
                .send()
                .await
                .map_err(|e| crate::chat::transport_error(cfg, e))?;
        } else {
            // 不是格式问题 —— 读走的 body 无法还回去，直接构造错误。
            return Err(crate::chat::http_error_from_body(cfg, 400, &peek));
        }
    }

    let status_code = resp.status();
    if !status_code.is_success() {
        return Err(crate::chat::http_error(cfg, status_code.as_u16(), resp).await);
    }

    // 响应可能是 `{"text":"…"}`（json / verbose_json）或裸文本（部分网关
    // 忽略 response_format，或干脆只支持 text）。
    let raw = resp.text().await.unwrap_or_default();
    let transcript = match serde_json::from_str::<WireResponse>(&raw) {
        Ok(w) => WireResponse::into_transcript(w, &req.model),
        // 不是 JSON 但也不是空 → 当纯文本响应处理
        Err(_) if !raw.trim().is_empty() => Transcript {
            text: raw.trim().to_string(),
            model: Some(req.model.clone()),
            ..Default::default()
        },
        Err(e) => {
            return Err(LlmError::parse(format!(
                "转写响应不是预期结构：{}（正文 {}）",
                e,
                truncate_chars(&redact(&raw, &cfg.api_key), 200)
            )));
        }
    };
    Ok(Transcript {
        latency: started.elapsed(),
        ..transcript
    })
}

/// 构造 multipart 表单。
///
/// 收敛成一处：verbose_json → json 的回落路径要重发同一个表单，
/// 复制粘贴意味着两处的字段会在下次加参数时分叉（然后只有回落路径漏掉）。
fn build_form(
    audio: Vec<u8>,
    filename: &str,
    req: &TranscriptionRequest,
    verbose: bool,
) -> reqwest::multipart::Form {
    let mut form = reqwest::multipart::Form::new()
        .part(
            "file",
            reqwest::multipart::Part::bytes(audio)
                .file_name(filename.to_string())
                .mime_str(guess_mime(filename))
                .unwrap_or_else(|_| {
                    reqwest::multipart::Part::bytes(Vec::new()).file_name(filename.to_string())
                }),
        )
        .text("model", req.model.trim().to_string());
    if let Some(lang) = req.language.as_ref().filter(|s| !s.trim().is_empty()) {
        form = form.text("language", lang.trim().to_string());
    }
    if let Some(p) = req.prompt.as_ref().filter(|s| !s.trim().is_empty()) {
        // 温和裁剪：有些网关对 prompt 有 244 字符上限，超了直接 400。
        form = form.text("prompt", truncate_chars(p.trim(), 200));
    }
    if let Some(t) = req.temperature {
        form = form.text("temperature", t.clamp(0.0, 1.0).to_string());
    }
    // verbose_json 能拿到语言/时长/分段；部分网关不支持，回落到 json。
    // 这不是「静默降级」—— 降的是**响应格式**，不是路由与鉴权：
    // 目标端点、key、协议都没变。
    form.text(
        "response_format",
        if verbose { "verbose_json" } else { "json" },
    )
}

impl WireResponse {
    fn into_transcript(w: WireResponse, model: &str) -> Transcript {
        Transcript {
            text: w.text,
            language: w.language,
            duration_secs: w.duration,
            // 解析阶段不知道耗时；由调用方（transcribe_inner）填上实际值。
            latency: std::time::Duration::ZERO,
            segments: w
                .segments
                .unwrap_or_default()
                .into_iter()
                .map(|s| Segment {
                    start: s.start.unwrap_or(0.0),
                    end: s.end.unwrap_or(0.0),
                    text: s.text,
                })
                .collect(),
            model: Some(model.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mime_guess_covers_common_containers() {
        assert_eq!(guess_mime("a.mp3"), "audio/mpeg");
        assert_eq!(guess_mime("a.m4a"), "audio/mp4");
        assert_eq!(guess_mime("a.MP3"), "audio/mpeg"); // 大小写
        assert_eq!(guess_mime("a.wav"), "audio/wav");
        assert_eq!(guess_mime("a"), "application/octet-stream");
    }

    #[test]
    fn empty_text_is_a_legal_transcript() {
        let t = Transcript::default();
        assert!(t.text.is_empty());
    }

    #[test]
    fn verbose_response_parses_segments() {
        let raw = r#"{
            "text": "大家好",
            "language": "zh",
            "duration": 12.5,
            "segments": [{"start":0.0,"end":2.5,"text":"大家好"}]
        }"#;
        let w: WireResponse = serde_json::from_str(raw).unwrap();
        let t = WireResponse::into_transcript(w, "whisper-1");
        assert_eq!(t.text, "大家好");
        assert_eq!(t.language.as_deref(), Some("zh"));
        assert_eq!(t.duration_secs, Some(12.5));
        assert_eq!(t.segments.len(), 1);
        assert_eq!(t.model.as_deref(), Some("whisper-1"));
    }

    #[test]
    fn plain_json_response_parses_without_segments() {
        let raw = r#"{"text":"hello"}"#;
        let w: WireResponse = serde_json::from_str(raw).unwrap();
        let t = WireResponse::into_transcript(w, "whisper-1");
        assert_eq!(t.text, "hello");
        assert!(t.segments.is_empty());
        assert!(t.language.is_none());
    }

    #[test]
    fn missing_model_is_not_configured() {
        let cfg = LlmConfig {
            api_key: "sk-test-1234567890".into(),
            ..Default::default()
        };
        let req = TranscriptionRequest::default(); // model 为空
                                                   // 同步路径：不需要真正跑起来，直接调同步校验逻辑
        assert!(req.model.trim().is_empty());
        // cfg 在 provider 未配置时 validate 也会失败；两者任一都能挡下请求
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn non_openai_provider_rejects_transcription_at_url_stage() {
        use crate::config::{Action, ProviderKind};
        for kind in [
            ProviderKind::Anthropic,
            ProviderKind::Gemini,
            ProviderKind::Ollama,
        ] {
            let cfg = LlmConfig {
                kind,
                base_url: "https://example.com".into(),
                api_key: "sk-test-1234567890".into(),
                model: "m".into(),
                ..Default::default()
            };
            let err = cfg.endpoint(Action::Transcription).unwrap_err();
            assert_eq!(err.kind, crate::error::ErrorKind::Unsupported, "{kind:?}");
            assert!(err.message.contains("transcriptions"), "{kind:?}");
        }
    }

    #[test]
    fn openai_compat_reaches_audio_transcriptions() {
        use crate::config::{Action, ProviderKind};
        let cfg = LlmConfig {
            kind: ProviderKind::OpenAi,
            base_url: "https://api.openai.com".into(),
            api_key: "sk-test-1234567890".into(),
            model: "whisper-1".into(),
            ..Default::default()
        };
        assert_eq!(
            cfg.endpoint(Action::Transcription).unwrap(),
            "https://api.openai.com/v1/audio/transcriptions"
        );
    }

    #[test]
    fn pasted_transcription_endpoint_is_stripped() {
        use crate::config::{Action, ProviderKind};
        let cfg = LlmConfig {
            kind: ProviderKind::OpenAi,
            // 用户把整段 endpoint 粘进 baseUrl
            base_url: "https://gw.corp/v1/audio/transcriptions".into(),
            api_key: "sk-test-1234567890".into(),
            model: "whisper-1".into(),
            ..Default::default()
        };
        assert_eq!(
            cfg.endpoint(Action::Transcription).unwrap(),
            "https://gw.corp/v1/audio/transcriptions"
        );
    }

    #[test]
    fn transport_timeout_is_retryable() {
        assert!(crate::error::ErrorKind::Timeout.is_retryable());
        assert!(crate::error::ErrorKind::Network.is_retryable());
        assert!(!crate::error::ErrorKind::Http.is_retryable());
    }
}
