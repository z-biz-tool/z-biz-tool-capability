//! HTTP 执行层：建连、发请求、错误映射、流式驱动。
//!
//! 上层只有四个动作：`chat` / `chat_stream` / `list_models` / `ping`。
//!
//! ## 三个必须在这里挡住的问题
//!
//! 1. **Gemini 的 key 在 URL 里**（`?key=`）。reqwest 的错误 Display 会带上
//!    完整 URL，于是「网络错误」的日志里就躺着用户的 key。所有错误消息
//!    出口都必须过 [`redact`]。
//! 2. **超时要真的生效**。shared 仓修过这个：`AIConfig.timeout` 声明了、
//!    默认值也给了，但 fetch 从来不带 AbortController —— 请求挂住就永远挂着。
//!    这里用 `reqwest::RequestBuilder::timeout` 逐请求落地，不用全局 client 超时
//!    （那样配置文件里改超时就不生效了）。
//! 3. **连接池要复用**。每次请求新建 `reqwest::Client` 会丢掉 TLS 会话复用，
//!    对话场景下每轮多付一次握手。用 `OnceLock` 存一个只带 connect_timeout
//!    的共享 client，总超时靠逐请求覆盖。

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use serde_json::Value;

use crate::config::{Action, LlmConfig, Protocol};
use crate::error::{redact, truncate_chars, ErrorKind, LlmError};
use crate::message::{ChatReply, Delta, Message, ToolDef};
use crate::sse::{parse_data, Utf8LineDecoder};
use crate::wire;

/// 仅建立连接的超时。总超时由 `LlmConfig::timeout` 逐请求覆盖。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// 错误体最多带多少字（人眼够看，且不会把用户原文灌进日志）
const ERROR_BODY_LIMIT: usize = 300;

fn shared_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            // 不设全局总超时：它会被逐请求的 .timeout() 覆盖，但设了容易让人
            // 误以为「改配置不生效」
            .user_agent(concat!("cap-llm/", env!("CARGO_PKG_VERSION")))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new())
    })
}

/// 供 [`crate::asr`] 复用同一个连接池。
///
/// ASR 与 chat 是同一类请求（同源、同鉴权、同超时语义），各建一个
/// client 会丢掉 TLS 会话复用 —— 上传音频时多付一次握手是实打实的秒级开销。
pub(crate) fn client() -> &'static reqwest::Client {
    shared_client()
}

/// 供 [`crate::asr`] 复用传输层错误映射。
pub(crate) fn transport_error(cfg: &LlmConfig, e: reqwest::Error) -> LlmError {
    map_transport(cfg, e)
}

/// 把「错误分类 + 原始错误串」变成用户可读、且不含密钥的消息。
///
/// 抽成纯函数是为了能**脱网测试**：`reqwest::Error` 没法手工构造，而
/// 「错误消息里不能出现 key」是一条纪律，不能只靠「跑一次真实失败」来保证 ——
/// 开发机上有代理时，`127.0.0.1:1` 会返回 502 而不是连接错误，测试就假绿了。
fn describe_error(cfg: &LlmConfig, kind: ErrorKind, raw: &str) -> LlmError {
    // 关键：raw 里可能带完整 URL（含 Gemini 的 ?key=）
    let safe = redact(raw, &cfg.api_key);
    let msg = match kind {
        ErrorKind::Timeout => format!(
            "请求超时（{}s）：{}",
            cfg.timeout.as_secs(),
            truncate_chars(&safe, 200)
        ),
        _ => format!("网络请求失败：{}", truncate_chars(&safe, 200)),
    };
    LlmError::new(kind, msg)
}

/// 把 reqwest 的错误翻成带分类的 [`LlmError`]。
fn map_transport(cfg: &LlmConfig, e: reqwest::Error) -> LlmError {
    let kind = if e.is_timeout() {
        ErrorKind::Timeout
    } else {
        ErrorKind::Network
    };
    describe_error(cfg, kind, &e.to_string())
}

/// 读取非 2xx 的响应体，脱敏 + 截断后拼成可读错误。
pub(crate) async fn http_error(cfg: &LlmConfig, status: u16, resp: reqwest::Response) -> LlmError {
    let body = resp.text().await.unwrap_or_default();
    http_error_from_body(cfg, status, &body)
}

/// 同 [`http_error`]，但 body 已经被上层读走了。
///
/// ASR 的 verbose_json 回落路径要先读 400 的 body 判断是否为格式问题，
/// 判完才决定报错 —— 那时 body 不在 `reqwest::Response` 手里了，
/// 只能按已读出的字符串构造。
pub(crate) fn http_error_from_body(cfg: &LlmConfig, status: u16, body: &str) -> LlmError {
    let safe = redact(body, &cfg.api_key);
    // 尽量从 JSON 里取 error.message，取不到再用原文
    let detail = serde_json::from_str::<Value>(&safe)
        .ok()
        .and_then(|v| {
            v.get("error")
                .and_then(|e| {
                    e.get("message")
                        .and_then(Value::as_str)
                        .or_else(|| e.as_str())
                })
                .map(str::to_string)
        })
        .unwrap_or_else(|| truncate_chars(&safe, ERROR_BODY_LIMIT));

    let hint = match status {
        401 | 403 => "（API Key 无效或无权限）",
        404 => "（接口地址或模型名不存在，请检查 Base URL 是否多写/少写 /v1）",
        429 => "（触发限流，请稍后重试）",
        413 => "（音频文件过大，请压缩或裁剪后重试）",
        s if (500..600).contains(&s) => "（服务端错误，稍后重试）",
        _ => "",
    };
    LlmError::http(
        status,
        format!("{}{}", truncate_chars(&detail, ERROR_BODY_LIMIT), hint),
    )
}

/// 发一次对话（非流式）。
pub async fn chat(cfg: &LlmConfig, messages: &[Message]) -> Result<ChatReply, LlmError> {
    chat_with_tools(cfg, messages, &[]).await
}

/// 发一次带工具声明的对话（非流式）。
///
/// 工具定义是协议无关的 [`ToolDef`]，`wire` 层负责翻译成各家格式；
/// 返回的 [`ChatReply::tool_calls`] 恒为 OpenAI 形状（Anthropic/Gemini
/// 的响应在这里归一），消费方只处理一种形状。
///
/// 执行循环（调工具 → 把结果作为新消息再发回来）由消费方自己驱动 ——
/// 那是业务语义（哪些工具、怎么确认、怎么限流），不是传输细节。
/// 注意流式带工具目前只支持 OpenAI 兼容系（见 [`chat_stream`]）。
pub async fn chat_with_tools(
    cfg: &LlmConfig,
    messages: &[Message],
    tools: &[ToolDef],
) -> Result<ChatReply, LlmError> {
    cfg.validate()?;
    let url = cfg.endpoint(Action::Chat)?;
    let body = wire::build_body(cfg, messages, false, tools);

    let started = Instant::now();
    let mut req = shared_client()
        .post(&url)
        .timeout(cfg.timeout)
        .header("content-type", "application/json");

    for (k, v) in wire::auth_headers(cfg) {
        req = req.header(k, v);
    }
    // Gemini 的 key 走 query（auth_headers 里不含它）
    if wire::needs_query_key(cfg) {
        req = req.query(&[("key", cfg.api_key.as_str())]);
    }

    let resp = req
        .json(&body)
        .send()
        .await
        .map_err(|e| map_transport(cfg, e))?;

    let status = resp.status();
    if !status.is_success() {
        return Err(http_error(cfg, status.as_u16(), resp).await);
    }

    let payload: Value = resp.json().await.map_err(|e| {
        if e.is_timeout() {
            map_transport(cfg, e)
        } else {
            LlmError::parse(format!(
                "响应不是合法 JSON：{}",
                truncate_chars(&redact(&e.to_string(), &cfg.api_key), 150)
            ))
        }
    })?;

    let mut reply = wire::parse_reply(cfg, &payload)?;
    reply.latency = started.elapsed();
    Ok(reply)
}

/// 发一次对话（流式）。
///
/// `on_delta` 每收到一段就调一次；函数返回时 `ChatReply.content` 是完整拼接结果，
/// 调用方不必自己攒。
///
/// 回调是同步 `FnMut` 而不是 async —— 上层（Tauri 的 `emit`、CLI 的 print）
/// 都是同步投递。要跨线程就由调用方套 channel，这一层不替它决定并发模型。
pub async fn chat_stream<F>(
    cfg: &LlmConfig,
    messages: &[Message],
    on_delta: F,
) -> Result<ChatReply, LlmError>
where
    F: FnMut(Delta),
{
    chat_stream_if(cfg, messages, || false, on_delta).await
}

/// 流式 + 主动停止。
///
/// `stop` 在**每个网络 chunk**到达时被调用一次，返回 true 就立即停止：
/// 不再读后续数据、不发起新请求，返回已累积的部分回答（内容是到停止点为止的，
/// 消费方据此区分「生成完毕」与「用户叫停」靠内容不够 —— 需要语义就比对
/// `Delta::Done` 是否由本函数在停止路径上发出，这里停止路径与正常路径一致地补发）。
///
/// 粒度说明：停在 chunk 边界而不是 token 边界 —— 停止时最后半帧会被丢弃，
/// 最多丢一个网络包里的几句话，对「用户按了停止」这个场景足够。
pub async fn chat_stream_if<F, S>(
    cfg: &LlmConfig,
    messages: &[Message],
    stop: S,
    mut on_delta: F,
) -> Result<ChatReply, LlmError>
where
    S: Fn() -> bool,
    F: FnMut(Delta),
{
    cfg.validate()?;
    let url = cfg.stream_endpoint()?;
    let body = wire::build_body(cfg, messages, true, &[]);

    let started = Instant::now();
    let mut req = shared_client()
        .post(&url)
        .timeout(cfg.timeout)
        .header("content-type", "application/json")
        // 有些网关（尤其反代）不加这两行会缓冲整段再吐，等于没有流式
        .header("accept", "text/event-stream")
        .header("cache-control", "no-cache");

    for (k, v) in wire::auth_headers(cfg) {
        req = req.header(k, v);
    }
    if wire::needs_query_key(cfg) {
        req = req.query(&[("key", cfg.api_key.as_str())]);
    }

    let resp = req
        .json(&body)
        .send()
        .await
        .map_err(|e| map_transport(cfg, e))?;

    let status = resp.status();
    if !status.is_success() {
        return Err(http_error(cfg, status.as_u16(), resp).await);
    }

    // 少数网关在流式请求出错时仍返回 200 + 普通 JSON（不是 SSE）。
    // 这时按非流式解析一次，至少能把错误原因带出来。
    let is_sse = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.contains("event-stream"))
        .unwrap_or(false);
    if !is_sse {
        let text = resp.text().await.unwrap_or_default();
        let safe = redact(&text, &cfg.api_key);
        if let Ok(v) = serde_json::from_str::<Value>(&safe) {
            // 能解析成完整响应 ⇒ 网关忽略了 stream 参数，退化成一次性返回
            if let Ok(mut r) = wire::parse_reply(cfg, &v) {
                if !r.content.is_empty() {
                    on_delta(Delta::Text(r.content.clone()));
                }
                on_delta(Delta::Done);
                r.latency = started.elapsed();
                return Ok(r);
            }
        }
        return Err(LlmError::parse(format!(
            "流式响应不是 SSE（content-type 非 event-stream），且无法按完整响应解析：{}",
            truncate_chars(&safe, ERROR_BODY_LIMIT)
        )));
    }

    let mut decoder = Utf8LineDecoder::new();
    let mut stream = resp.bytes_stream();
    let mut full = String::new();
    let mut usage = None;
    let mut done_sent = false;

    // Ollama 原生流不是 SSE，每行就是一个裸 JSON，没有 data: 前缀
    let needs_data_prefix = cfg.kind.protocol() != Protocol::Ollama;

    'outer: while let Some(chunk) = stream.next().await {
        // 用户叫停：停在 chunk 边界，已收到的内容照常返回
        if stop() {
            break 'outer;
        }
        let chunk = chunk.map_err(|e| map_transport(cfg, e))?;
        for line in decoder.push(&chunk) {
            let payload_str = if needs_data_prefix {
                match parse_data(&line) {
                    Some(p) => p,
                    None => continue,
                }
            } else {
                let t = line.trim();
                if t.is_empty() {
                    continue;
                }
                t
            };

            // OpenAI 系的结束标记
            if payload_str == "[DONE]" {
                break 'outer;
            }

            let Ok(v) = serde_json::from_str::<Value>(payload_str) else {
                // 单帧坏掉不该终止整个流 —— 长回答里偶尔出现半帧是常态
                continue;
            };

            // 流里也可能夹错误对象
            if let Some(err) = v.get("error") {
                let msg = err
                    .get("message")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| truncate_chars(&v.to_string(), 200));
                return Err(LlmError::http(
                    v.get("code").and_then(Value::as_u64).unwrap_or(0) as u16,
                    format!("流式过程中服务端报错：{}", truncate_chars(&msg, 300)),
                ));
            }

            let (text, done, u) = wire::parse_delta(cfg, &v);
            if let Some(t) = text {
                full.push_str(&t);
                on_delta(Delta::Text(t));
            }
            if u.is_some() {
                usage = u;
            }
            if done {
                on_delta(Delta::Done);
                done_sent = true;
                break 'outer;
            }
        }
    }

    // 补上解码器残留（末行不带换行的情况）
    for line in decoder.finish() {
        if let Some(p) = parse_data(&line) {
            if p == "[DONE]" {
                break;
            }
            if let Ok(v) = serde_json::from_str::<Value>(p) {
                let (text, _, _) = wire::parse_delta(cfg, &v);
                if let Some(t) = text {
                    full.push_str(&t);
                    on_delta(Delta::Text(t));
                }
            }
        }
    }

    if !done_sent {
        on_delta(Delta::Done);
    }

    Ok(ChatReply {
        content: full,
        tool_calls: Vec::new(),
        usage,
        latency: started.elapsed(),
        model: None,
    })
}

/// 列模型。失败**不抛错而是返回空**的语义留给调用方决定 ——
/// 这里如实返回 `Err`，因为「静默返回空列表」会让「地址填错」看起来像
/// 「这个服务没有模型」。
pub async fn list_models(cfg: &LlmConfig) -> Result<Vec<String>, LlmError> {
    // 列模型不需要 model 字段，所以不能直接 validate（它会因为 model 为空而报错）
    let mut probe = cfg.clone();
    if probe.model.trim().is_empty() {
        probe.model = "probe".into();
    }
    if probe.needs_api_key() && probe.api_key.trim().is_empty() {
        return Err(LlmError::not_configured(format!(
            "{} 需要 API Key 才能列出模型",
            probe.kind.label()
        )));
    }
    let url = probe.endpoint(Action::Models)?;

    let mut req = shared_client()
        .get(&url)
        .timeout(probe.timeout)
        .header("accept", "application/json");
    for (k, v) in wire::auth_headers(&probe) {
        req = req.header(k, v);
    }
    if wire::needs_query_key(&probe) {
        req = req.query(&[("key", probe.api_key.as_str())]);
    }

    let resp = req.send().await.map_err(|e| map_transport(&probe, e))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(http_error(&probe, status.as_u16(), resp).await);
    }
    let payload: Value = resp.json().await.map_err(|e| {
        LlmError::parse(format!(
            "模型列表不是合法 JSON：{}",
            truncate_chars(&e.to_string(), 120)
        ))
    })?;
    wire::parse_models(&probe, &payload)
}

/// 连通性探测。
///
/// 刻意**不用 `/models`**：很多自建代理不实现它，用它探活会把「服务好好的，
/// 只是没这个接口」误报成「连不上」。直接发一个 1 token 的对话，
/// 能拿到回复就说明 key / 地址 / 模型三项全对 —— 这才是用户想测的东西。
pub async fn ping(cfg: &LlmConfig) -> Result<Duration, LlmError> {
    let mut probe = cfg.clone();
    // 探测时把输出压到最小，避免误烧钱
    probe.max_tokens = Some(1);
    probe.temperature = Some(0.0);
    let started = Instant::now();
    let msgs = [Message::user("hi")];
    chat(&probe, &msgs).await?;
    Ok(started.elapsed())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProviderKind;

    fn cfg(kind: ProviderKind) -> LlmConfig {
        LlmConfig {
            kind,
            base_url: String::new(),
            api_key: "sk-very-secret-key-123456".into(),
            model: "m".into(),
            ..Default::default()
        }
    }

    #[test]
    fn transport_error_redacts_key_from_url() {
        // Gemini 的 key 在 query 上，reqwest 的错误 Display 会带完整 URL。
        // 用真实形态的串直接验证脱敏，不依赖网络环境。
        let c = cfg(ProviderKind::Gemini);
        let raw = format!(
            "error sending request for url (https://generativelanguage.googleapis.com/v1beta/models/x:generateContent?key={})",
            c.api_key
        );
        let mapped = describe_error(&c, ErrorKind::Network, &raw);
        assert!(
            !mapped.message.contains(&c.api_key),
            "错误消息泄露了 key: {}",
            mapped.message
        );
        assert!(mapped.message.contains("REDACTED"));
    }

    #[test]
    fn timeout_error_mentions_the_configured_seconds() {
        let mut c = cfg(ProviderKind::OpenAi);
        c.timeout = Duration::from_secs(45);
        let mapped = describe_error(&c, ErrorKind::Timeout, "operation timed out");
        assert!(mapped.message.contains("45s"));
        assert_eq!(mapped.kind, ErrorKind::Timeout);
    }

    #[tokio::test]
    async fn validate_runs_before_any_request() {
        let mut c = cfg(ProviderKind::OpenAi);
        c.api_key.clear();
        // 不该有任何网络动作就返回 NotConfigured
        let e = chat(&c, &[Message::user("x")]).await.unwrap_err();
        assert_eq!(e.kind, ErrorKind::NotConfigured);
    }

    #[tokio::test]
    async fn unreachable_host_fails_with_classification_not_panic() {
        let mut c = cfg(ProviderKind::OpenAi);
        c.base_url = "http://127.0.0.1:1".into();
        c.timeout = Duration::from_secs(2);
        let e = chat(&c, &[Message::user("x")]).await.unwrap_err();
        // 开发机上有 HTTP 代理时，这个地址会被代理拦下返回 502 ——
        // 所以 Http 也是合法结果。这里只钉「有分类、不 panic、不泄露 key」。
        assert!(
            matches!(
                e.kind,
                ErrorKind::Network | ErrorKind::Timeout | ErrorKind::Http
            ),
            "实际: {:?} {}",
            e.kind,
            e.message
        );
        assert!(!e.message.contains(&c.api_key));
    }

    #[tokio::test]
    async fn list_models_does_not_require_model_name() {
        // 列模型时 model 为空是正常的，不该报 NotConfigured
        let mut c = cfg(ProviderKind::OpenAi);
        c.model.clear();
        c.base_url = "http://127.0.0.1:1".into();
        c.timeout = Duration::from_secs(2);
        let e = list_models(&c).await.unwrap_err();
        assert_ne!(e.kind, ErrorKind::NotConfigured);
    }
}
