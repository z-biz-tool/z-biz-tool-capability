//! 协议适配：**十套实现的差异全部收敛在本文件**。
//!
//! 上层（[`crate::chat`]）只认 `Protocol` + `Message`，不认 `choices`、
//! `candidates`、`content_block_delta` 这些协议细节。想加一家新厂商，
//! 只需要在这里加一个分支，而不是在十个产品里各改一遍。
//!
//! ## 四种协议的真实差异（不是想象的差异，是报文级的）
//!
//! | 项 | OpenAI 兼容 | Anthropic | Gemini | Ollama 原生 |
//! |---|---|---|---|---|
//! | 鉴权头 | `Authorization: Bearer` | `x-api-key` + `anthropic-version` | `?key=` 查询串 | 无 |
//! | system | messages 里 role=system | **顶层 `system` 字段** | `systemInstruction` | messages 里 role=system |
//! | assistant 角色名 | `assistant` | `assistant` | **`model`** | `assistant` |
//! | 输出上限 | `max_tokens` | `max_tokens`（**必填**） | `generationConfig.maxOutputTokens` | `options.num_predict` |
//! | 取正文 | `choices[0].message.content` | `content[].text` 拼接 | `candidates[0].content.parts[].text` | `message.content` |
//! | 流式增量 | `choices[0].delta.content` | `delta.text`（仅 content_block_delta） | `candidates[0].content.parts[].text` | 每行一个 JSON |
//! | 结束标记 | `data: [DONE]` | `event: message_stop` | 无（流关闭即结束） | `done: true` |
//!
//! 这张表里任何一格搞错，表现都是「能连上但结果不对」——最难排查的一类 bug。
//! 所以每个分支都配了报文级的单测。

use serde_json::{json, Map, Value};

use crate::config::{LlmConfig, Protocol};
use crate::error::{truncate_chars, LlmError};
use crate::message::{ChatReply, Message, Role, ToolDef, Usage};

/// Anthropic 的 `max_tokens` 是必填项，不给会被服务端直接拒。
/// 各家旧实现里这个值散落着 1024 / 2048 / 4096，这里取一个「够用且不意外烧钱」的默认。
pub const DEFAULT_MAX_TOKENS: u32 = 2048;

/// Anthropic 要求带版本头；写死当前稳定版（2023-06-01），
/// 与 shared/clients.ts 以及 pet 保持一致。
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// 构造鉴权头。
///
/// Gemini 的 key 走 query 而非 header，所以这里不含它 —— 见 [`needs_query_key`]。
pub fn auth_headers(cfg: &LlmConfig) -> Vec<(&'static str, String)> {
    if cfg.api_key.is_empty() {
        return Vec::new();
    }
    match cfg.kind.protocol() {
        Protocol::Anthropic => vec![
            ("x-api-key", cfg.api_key.clone()),
            ("anthropic-version", ANTHROPIC_VERSION.to_string()),
        ],
        // Ollama 本地不需要鉴权；填了 key 也照发（有些反代要）
        Protocol::Ollama => Vec::new(),
        _ => vec![("authorization", format!("Bearer {}", cfg.api_key))],
    }
}

/// Gemini 把 key 放在 `?key=` 上。
pub fn needs_query_key(cfg: &LlmConfig) -> bool {
    cfg.kind.protocol() == Protocol::Gemini && !cfg.api_key.is_empty()
}

/// 温度值转 JSON。
///
/// `f32` 的 0.3 转成 `f64` 是 `0.30000001192092896`，直接 `json!` 会把这个
/// 20 位小数原样发出去。虽然服务端不在乎，但抓包和日志里很难看，
/// 也会让「配置写 0.3、请求里是 0.30000001192092896」看起来像 bug。
/// 规整到 3 位小数：对温度这个量级（0.0–2.0）精度绰绰有余。
fn temp_json(t: f32) -> Value {
    json!((t as f64 * 1000.0).round() / 1000.0)
}

/// 构造请求体。`stream` 决定是否要求服务端走 SSE。
///
/// `tools` 给了就把工具声明翻译成对应协议的格式（pet 的 ai.rs 曾为
/// claude/gemini 各写一套双向转换，现在收口在这一层）。
pub fn build_body(
    cfg: &LlmConfig,
    messages: &[Message],
    stream: bool,
    tools: &[ToolDef],
) -> Value {
    match cfg.kind.protocol() {
        Protocol::OpenAiCompat => {
            // 注意不能写成 `"temperature": cfg.temperature` —— None 会变成
            // JSON 的 null，而部分兼容网关见到 null 会直接 400。
            let mut body = json!({
                "model": cfg.model,
                "messages": openai_messages(messages),
                "stream": stream,
            });
            if let Some(t) = cfg.temperature {
                body["temperature"] = temp_json(t);
            }
            if let Some(m) = cfg.max_tokens {
                body["max_tokens"] = json!(m);
            }
            if !tools.is_empty() {
                body["tools"] = json!(openai_tools(tools));
            }
            body
        }
        Protocol::Anthropic => {
            // system 必须提到顶层，且只能有一个：多条 system 要拼接，
            // 留在 messages 里会被服务端认成「未知角色」
            let system = messages
                .iter()
                .filter(|m| m.role == Role::System)
                .map(|m| m.content.as_str())
                .collect::<Vec<_>>()
                .join("\n\n");
            let turns: Vec<Value> = messages
                .iter()
                .filter(|m| m.role != Role::System)
                .map(|m| {
                    // Claude 的视觉是 content 数组：text 块 + image 块（base64 source）
                    let content = if m.images.is_empty() {
                        json!(m.content)
                    } else {
                        let mut parts = vec![json!({ "type": "text", "text": m.content })];
                        for img in &m.images {
                            parts.push(json!({
                                "type": "image",
                                "source": {
                                    "type": "base64",
                                    "media_type": guess_mime(img),
                                    "data": strip_data_url(img),
                                }
                            }));
                        }
                        json!(parts)
                    };
                    json!({
                        "role": if m.role == Role::Assistant { "assistant" } else { "user" },
                        "content": content,
                    })
                })
                .collect();
            let mut body = json!({
                "model": cfg.model,
                "messages": turns,
                "max_tokens": cfg.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
                "stream": stream,
            });
            if !system.is_empty() {
                body["system"] = json!(system);
            }
            if let Some(t) = cfg.temperature {
                body["temperature"] = temp_json(t);
            }
            if !tools.is_empty() {
                body["tools"] = json!(anthropic_tools(tools));
            }
            body
        }
        Protocol::Gemini => {
            let system = messages
                .iter()
                .filter(|m| m.role == Role::System)
                .map(|m| m.content.as_str())
                .collect::<Vec<_>>()
                .join("\n\n");
            let contents: Vec<Value> = messages
                .iter()
                .filter(|m| m.role != Role::System)
                .map(|m| {
                    let mut parts = vec![json!({ "text": m.content })];
                    for img in &m.images {
                        parts.push(json!({
                            "inline_data": {
                                "mime_type": guess_mime(img),
                                "data": strip_data_url(img),
                            }
                        }));
                    }
                    json!({
                        // Gemini 的 assistant 叫 "model"，写 assistant 会 400
                        "role": if m.role == Role::Assistant { "model" } else { "user" },
                        "parts": parts,
                    })
                })
                .collect();

            let mut generation = Map::new();
            if let Some(t) = cfg.temperature {
                generation.insert("temperature".into(), temp_json(t));
            }
            if let Some(m) = cfg.max_tokens {
                generation.insert("maxOutputTokens".into(), json!(m));
            }

            let mut body = Map::new();
            body.insert("contents".into(), json!(contents));
            if !system.is_empty() {
                body.insert(
                    "systemInstruction".into(),
                    json!({ "parts": [{ "text": system }] }),
                );
            }
            if !generation.is_empty() {
                body.insert("generationConfig".into(), Value::Object(generation));
            }
            if !tools.is_empty() {
                body.insert("tools".into(), json!(gemini_tools(tools)));
            }
            // Gemini 不需要 stream 字段：流式与否由 URL 决定
            Value::Object(body)
        }
        Protocol::Ollama => {
            let mut options = Map::new();
            if let Some(t) = cfg.temperature {
                options.insert("temperature".into(), temp_json(t));
            }
            if let Some(m) = cfg.max_tokens {
                // 这一格是十套实现里最容易错的一处：Ollama 原生端点
                // **不认 max_tokens**，写错了不会报错，只是静默按默认长度截断
                options.insert("num_predict".into(), json!(m));
            }
            let mut body = Map::new();
            body.insert("model".into(), json!(cfg.model));
            body.insert("messages".into(), json!(ollama_messages(messages)));
            body.insert("stream".into(), json!(stream));
            if !options.is_empty() {
                body.insert("options".into(), Value::Object(options));
            }
            if !tools.is_empty() {
                // Ollama 原生 /api/chat 的 tools 就是 OpenAI 形状
                body.insert("tools".into(), json!(openai_tools(tools)));
            }
            Value::Object(body)
        }
    }
}

// ---------- 工具声明的协议翻译 ----------

/// OpenAI / Ollama：`{"type":"function","function":{name,description,parameters}}`
fn openai_tools(tools: &[ToolDef]) -> Vec<Value> {
    tools
        .iter()
        .map(|t| {
            json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.parameters,
                },
            })
        })
        .collect()
}

/// Anthropic：`{name, description, input_schema}`
fn anthropic_tools(tools: &[ToolDef]) -> Vec<Value> {
    tools
        .iter()
        .map(|t| {
            json!({
                "name": t.name,
                "description": t.description,
                "input_schema": t.parameters,
            })
        })
        .collect()
}

/// Gemini：包一层 `functionDeclarations`，parameters 直用（同一套 JSON Schema 语义）
fn gemini_tools(tools: &[ToolDef]) -> Vec<Value> {
    let declarations: Vec<Value> = tools
        .iter()
        .map(|t| {
            json!({
                "name": t.name,
                "description": t.description,
                "parameters": t.parameters,
            })
        })
        .collect();
    vec![json!({ "functionDeclarations": declarations })]
}

fn openai_messages(messages: &[Message]) -> Vec<Value> {
    messages
        .iter()
        .map(|m| {
            if m.images.is_empty() {
                json!({ "role": m.role.as_str(), "content": m.content })
            } else {
                // OpenAI 视觉走 content 数组
                let mut parts = vec![json!({ "type": "text", "text": m.content })];
                for img in &m.images {
                    parts.push(json!({
                        "type": "image_url",
                        "image_url": { "url": as_data_url(img) }
                    }));
                }
                json!({ "role": m.role.as_str(), "content": parts })
            }
        })
        .collect()
}

fn ollama_messages(messages: &[Message]) -> Vec<Value> {
    messages
        .iter()
        .map(|m| {
            if m.images.is_empty() {
                json!({ "role": m.role.as_str(), "content": m.content })
            } else {
                // Ollama 原生用 images 数组，元素是**纯 base64**（不带 data: 前缀）
                json!({
                    "role": m.role.as_str(),
                    "content": m.content,
                    "images": m.images.iter().map(|i| strip_data_url(i)).collect::<Vec<_>>(),
                })
            }
        })
        .collect()
}

fn as_data_url(img: &str) -> String {
    if img.starts_with("data:") {
        img.to_string()
    } else {
        format!("data:{};base64,{}", guess_mime(img), img)
    }
}

fn strip_data_url(img: &str) -> String {
    match img.split_once(";base64,") {
        Some((_, b64)) => b64.to_string(),
        None => img.to_string(),
    }
}

fn guess_mime(img: &str) -> &'static str {
    if let Some(rest) = img.strip_prefix("data:") {
        if let Some((mime, _)) = rest.split_once(';') {
            // 只会命中有限的几个值，泄漏内存可以忽略（这里直接静态化常见项）
            return match mime {
                "image/png" => "image/png",
                "image/webp" => "image/webp",
                "image/gif" => "image/gif",
                _ => "image/jpeg",
            };
        }
    }
    "image/jpeg"
}

/// 解析非流式响应。
pub fn parse_reply(cfg: &LlmConfig, payload: &Value) -> Result<ChatReply, LlmError> {
    match cfg.kind.protocol() {
        Protocol::OpenAiCompat => {
            let choice = payload
                .get("choices")
                .and_then(Value::as_array)
                .and_then(|a| a.first())
                .ok_or_else(|| shape_error("choices[0]", payload))?;
            let msg = choice.get("message").unwrap_or(&Value::Null);
            let content = msg
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let tool_calls = msg
                .get("tool_calls")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            // 有些兼容网关在推理模型上只回 reasoning_content，正文留空 ——
            // 这时不该报错（调用方看到空串自会处理），但也不该伪装成有内容
            Ok(ChatReply {
                content,
                tool_calls,
                usage: payload.get("usage").map(parse_openai_usage),
                latency: Default::default(),
                model: payload
                    .get("model")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            })
        }
        Protocol::Anthropic => {
            let blocks = payload
                .get("content")
                .and_then(Value::as_array)
                .ok_or_else(|| shape_error("content[]", payload))?;
            // 只拼 text 块，跳过 thinking / tool_use（后者单独透传）
            let content: String = blocks
                .iter()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect();
            if content.is_empty() && blocks.is_empty() {
                return Err(shape_error("content[].text", payload));
            }
            let tool_calls: Vec<Value> = blocks
                .iter()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_use"))
                .map(claude_block_to_openai_tool_call)
                .collect();
            Ok(ChatReply {
                content,
                tool_calls,
                usage: payload.get("usage").map(|u| Usage {
                    prompt_tokens: u.get("input_tokens").and_then(Value::as_u64).map(|v| v as u32),
                    completion_tokens: u
                        .get("output_tokens")
                        .and_then(Value::as_u64)
                        .map(|v| v as u32),
                    total_tokens: {
                        let a = u.get("input_tokens").and_then(Value::as_u64);
                        let b = u.get("output_tokens").and_then(Value::as_u64);
                        match (a, b) {
                            (Some(a), Some(b)) => Some((a + b) as u32),
                            _ => None,
                        }
                    },
                }),
                latency: Default::default(),
                model: payload
                    .get("model")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            })
        }
        Protocol::Gemini => {
            let content: String = payload
                .get("candidates")
                .and_then(Value::as_array)
                .and_then(|a| a.first())
                .and_then(|c| c.get("content"))
                .and_then(|c| c.get("parts"))
                .and_then(Value::as_array)
                .map(|parts| {
                    parts
                        .iter()
                        .filter_map(|p| p.get("text").and_then(Value::as_str))
                        .collect()
                })
                .unwrap_or_default();
            if content.is_empty() && payload.get("candidates").is_none() {
                return Err(shape_error("candidates[0].content.parts", payload));
            }
            // Gemini 的工具调用：parts[].functionCall → OpenAI 形状。
            // callId 可能缺（Gemini 不强制），缺了就按序号补一个。
            let tool_calls: Vec<Value> = payload
                .get("candidates")
                .and_then(Value::as_array)
                .and_then(|a| a.first())
                .and_then(|c| c.get("content"))
                .and_then(|c| c.get("parts"))
                .and_then(Value::as_array)
                .map(|parts| {
                    parts
                        .iter()
                        .filter_map(|p| p.get("functionCall"))
                        .enumerate()
                        .map(|(i, fc)| gemini_call_to_openai_tool_call(fc, i))
                        .collect()
                })
                .unwrap_or_default();
            Ok(ChatReply {
                content,
                tool_calls,
                usage: payload.get("usageMetadata").map(|u| Usage {
                    prompt_tokens: u
                        .get("promptTokenCount")
                        .and_then(Value::as_u64)
                        .map(|v| v as u32),
                    completion_tokens: u
                        .get("candidatesTokenCount")
                        .and_then(Value::as_u64)
                        .map(|v| v as u32),
                    total_tokens: u
                        .get("totalTokenCount")
                        .and_then(Value::as_u64)
                        .map(|v| v as u32),
                }),
                latency: Default::default(),
                model: payload
                    .get("modelVersion")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            })
        }
        Protocol::Ollama => {
            let msg = payload
                .get("message")
                .ok_or_else(|| shape_error("message.content", payload))?;
            Ok(ChatReply {
                content: msg
                    .get("content")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                tool_calls: msg
                    .get("tool_calls")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().map(normalize_ollama_tool_call).collect())
                    .unwrap_or_default(),
                usage: Some(Usage {
                    prompt_tokens: payload
                        .get("prompt_eval_count")
                        .and_then(Value::as_u64)
                        .map(|v| v as u32),
                    completion_tokens: payload
                        .get("eval_count")
                        .and_then(Value::as_u64)
                        .map(|v| v as u32),
                    total_tokens: {
                        let a = payload.get("prompt_eval_count").and_then(Value::as_u64);
                        let b = payload.get("eval_count").and_then(Value::as_u64);
                        match (a, b) {
                            (Some(a), Some(b)) => Some((a + b) as u32),
                            _ => None,
                        }
                    },
                }),
                latency: Default::default(),
                model: payload
                    .get("model")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            })
        }
    }
}

fn parse_openai_usage(u: &Value) -> Usage {
    Usage {
        prompt_tokens: u
            .get("prompt_tokens")
            .and_then(Value::as_u64)
            .map(|v| v as u32),
        completion_tokens: u
            .get("completion_tokens")
            .and_then(Value::as_u64)
            .map(|v| v as u32),
        total_tokens: u
            .get("total_tokens")
            .and_then(Value::as_u64)
            .map(|v| v as u32),
    }
}

// ---------- tool_calls 归一（恒为 OpenAI 形状） ----------

/// Anthropic 的 `tool_use` 块 → OpenAI tool_call。
/// arguments：Anthropic 的 `input` 是 JSON **对象**，OpenAI 约定是**字符串**，转一次。
fn claude_block_to_openai_tool_call(b: &Value) -> Value {
    let args = b
        .get("input")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let arguments = match args {
        Value::String(s) => s,
        other => other.to_string(),
    };
    json!({
        "id": b.get("id").cloned().unwrap_or(Value::Null),
        "type": "function",
        "function": {
            "name": b.get("name").cloned().unwrap_or(Value::Null),
            "arguments": arguments,
        },
    })
}

/// Gemini 的 `functionCall` → OpenAI tool_call。
/// Gemini 没有 id（部分网关给 name 当 id），缺省按序号合成一个稳定的。
fn gemini_call_to_openai_tool_call(fc: &Value, index: usize) -> Value {
    let args = fc.get("args").cloned().unwrap_or_else(|| json!({}));
    let arguments = match args {
        Value::String(s) => s,
        other => other.to_string(),
    };
    let id = fc
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| format!("gemini-call-{index}"));
    json!({
        "id": id,
        "type": "function",
        "function": {
            "name": fc.get("name").cloned().unwrap_or(Value::Null),
            "arguments": arguments,
        },
    })
}

/// Ollama 的 tool_call：形状已接近 OpenAI，只有 arguments 是**对象**而不是字符串。
fn normalize_ollama_tool_call(v: &Value) -> Value {
    let mut out = v.clone();
    if let Some(f) = out.get_mut("function") {
        if let Some(args) = f.get("arguments") {
            if !args.is_string() {
                let s = args.to_string();
                f["arguments"] = json!(s);
            }
        }
    }
    out
}

/// 解析流式单帧，返回 `(文本增量, 是否结束, usage)`。
///
/// 文本为空不代表异常（Anthropic 的 `message_start`、OpenAI 首个带 role 的
/// delta 都是空文本），所以返回 `Option<String>` 而不是 `String`。
pub fn parse_delta(
    cfg: &LlmConfig,
    payload: &Value,
) -> (Option<String>, bool, Option<Usage>) {
    match cfg.kind.protocol() {
        Protocol::OpenAiCompat => {
            let text = payload
                .get("choices")
                .and_then(Value::as_array)
                .and_then(|a| a.first())
                .and_then(|c| c.get("delta"))
                .and_then(|d| d.get("content"))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            let done = payload
                .get("choices")
                .and_then(Value::as_array)
                .and_then(|a| a.first())
                .and_then(|c| c.get("finish_reason"))
                .map(|f| !f.is_null())
                .unwrap_or(false);
            let usage = payload.get("usage").filter(|u| !u.is_null()).map(parse_openai_usage);
            (text, done, usage)
        }
        Protocol::Anthropic => {
            let etype = payload.get("type").and_then(Value::as_str).unwrap_or("");
            match etype {
                "content_block_delta" => {
                    let text = payload
                        .get("delta")
                        .and_then(|d| d.get("text"))
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string);
                    (text, false, None)
                }
                "message_delta" => {
                    let usage = payload.get("usage").map(|u| Usage {
                        prompt_tokens: u
                            .get("input_tokens")
                            .and_then(Value::as_u64)
                            .map(|v| v as u32),
                        completion_tokens: u
                            .get("output_tokens")
                            .and_then(Value::as_u64)
                            .map(|v| v as u32),
                        total_tokens: None,
                    });
                    (None, false, usage)
                }
                "message_stop" => (None, true, None),
                _ => (None, false, None),
            }
        }
        Protocol::Gemini => {
            let text: Option<String> = payload
                .get("candidates")
                .and_then(Value::as_array)
                .and_then(|a| a.first())
                .and_then(|c| c.get("content"))
                .and_then(|c| c.get("parts"))
                .and_then(Value::as_array)
                .map(|parts| {
                    parts
                        .iter()
                        .filter_map(|p| p.get("text").and_then(Value::as_str))
                        .collect::<String>()
                })
                .filter(|s| !s.is_empty());
            let done = payload
                .get("candidates")
                .and_then(Value::as_array)
                .and_then(|a| a.first())
                .and_then(|c| c.get("finishReason"))
                .map(|f| !f.is_null())
                .unwrap_or(false);
            (text, done, None)
        }
        Protocol::Ollama => {
            let text = payload
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            let done = payload
                .get("done")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            (text, done, None)
        }
    }
}

/// 从列表响应里抽出模型 id。
pub fn parse_models(cfg: &LlmConfig, payload: &Value) -> Result<Vec<String>, LlmError> {
    let ids: Vec<String> = match cfg.kind.protocol() {
        Protocol::OpenAiCompat => payload
            .get("data")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|m| m.get("id").and_then(Value::as_str))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        Protocol::Anthropic => payload
            .get("data")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|m| m.get("id").and_then(Value::as_str))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        Protocol::Gemini => payload
            .get("models")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|m| m.get("name").and_then(Value::as_str))
                    // Gemini 返回的是 "models/gemini-2.0-flash"，剥掉前缀
                    .map(|n| n.trim_start_matches("models/").to_string())
                    .collect()
            })
            .unwrap_or_default(),
        Protocol::Ollama => payload
            .get("models")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|m| m.get("name").and_then(Value::as_str))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
    };
    if ids.is_empty() && !payload.is_object() {
        return Err(shape_error("模型列表", payload));
    }
    Ok(ids)
}

/// 构造「响应结构不符预期」的错误。
///
/// **只带键名，不带整包 body**。旧实现（file 仓 `parse_response`）把
/// `format!("无法从响应中提取文本: {}", body)` 写进错误串 —— 响应体里
/// 可能含有用户输入的原文、甚至（被网关回显的）鉴权头。错误信息是给人看的，
/// 不是给 dump 用的。
fn shape_error(expected: &str, payload: &Value) -> LlmError {
    let keys: Vec<String> = payload
        .as_object()
        .map(|o| o.keys().take(8).cloned().collect())
        .unwrap_or_default();
    LlmError::parse(format!(
        "响应结构不符预期：缺少 {expected}（实际顶层字段：{}{}）",
        if keys.is_empty() {
            "非对象".to_string()
        } else {
            truncate_chars(&keys.join(", "), 120)
        },
        payload.as_str().map(|s| format!("，正文前 80 字：{}", truncate_chars(s, 80))).unwrap_or_default()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProviderKind;
    use std::time::Duration;

    fn cfg(kind: ProviderKind) -> LlmConfig {
        LlmConfig {
            kind,
            base_url: String::new(),
            api_key: "sk-test-1234567890".into(),
            model: "m1".into(),
            timeout: Duration::from_secs(30),
            temperature: Some(0.3),
            max_tokens: Some(100),
            preset: None,
        }
    }

    // ── 请求体 ───────────────────────────────────────────────

    #[test]
    fn openai_body_shape() {
        let b = build_body(&cfg(ProviderKind::OpenAi), &[Message::user("hi")], false, &[]);
        assert_eq!(b["model"], "m1");
        assert_eq!(b["messages"][0]["role"], "user");
        assert_eq!(b["stream"], false);
        assert_eq!(b["max_tokens"], 100);
        assert_eq!(b["temperature"], 0.3);
    }

    #[test]
    fn anthropic_moves_system_to_top_level() {
        let b = build_body(
            &cfg(ProviderKind::Anthropic),
            &[Message::system("be brief"), Message::user("hi")],
            false, &[],
        );
        assert_eq!(b["system"], "be brief");
        // system 不能留在 messages 里
        assert_eq!(b["messages"].as_array().unwrap().len(), 1);
        assert_eq!(b["messages"][0]["role"], "user");
    }

    #[test]
    fn anthropic_always_sends_max_tokens() {
        let mut c = cfg(ProviderKind::Anthropic);
        c.max_tokens = None;
        let b = build_body(&c, &[Message::user("hi")], false, &[]);
        assert_eq!(b["max_tokens"], DEFAULT_MAX_TOKENS);
    }

    #[test]
    fn anthropic_joins_multiple_system_messages() {
        let b = build_body(
            &cfg(ProviderKind::Anthropic),
            &[Message::system("a"), Message::system("b"), Message::user("x")],
            false, &[],
        );
        assert_eq!(b["system"], "a\n\nb");
    }

    #[test]
    fn gemini_renames_assistant_to_model() {
        let b = build_body(
            &cfg(ProviderKind::Gemini),
            &[Message::user("hi"), Message::assistant("yo"), Message::user("again")],
            false, &[],
        );
        let contents = b["contents"].as_array().unwrap();
        assert_eq!(contents[0]["role"], "user");
        assert_eq!(contents[1]["role"], "model");
        assert_eq!(contents[2]["role"], "user");
    }

    #[test]
    fn gemini_uses_generation_config() {
        let b = build_body(&cfg(ProviderKind::Gemini), &[Message::user("hi")], false, &[]);
        assert_eq!(b["generationConfig"]["maxOutputTokens"], 100);
        assert_eq!(b["generationConfig"]["temperature"], 0.3);
        // 顶层不该出现 max_tokens
        assert!(b.get("max_tokens").is_none());
        // Gemini 的流式由 URL 决定，body 里不带 stream
        assert!(b.get("stream").is_none());
    }

    #[test]
    fn ollama_maps_max_tokens_to_num_predict() {
        // 这一格搞错不会报错，只会静默按默认长度截断
        let b = build_body(&cfg(ProviderKind::Ollama), &[Message::user("hi")], false, &[]);
        assert_eq!(b["options"]["num_predict"], 100);
        assert!(b.get("max_tokens").is_none());
    }

    #[test]
    fn ollama_images_are_plain_base64() {
        let m = Message::user("看图").with_images(vec!["data:image/png;base64,QUJD".into()]);
        let b = build_body(&cfg(ProviderKind::Ollama), &[m], false, &[]);
        assert_eq!(b["messages"][0]["images"][0], "QUJD");
    }

    #[test]
    fn openai_images_are_data_urls() {
        let m = Message::user("看图").with_images(vec!["QUJD".into()]);
        let b = build_body(&cfg(ProviderKind::OpenAi), &[m], false, &[]);
        let parts = b["messages"][0]["content"].as_array().unwrap();
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(parts[1]["type"], "image_url");
        assert!(parts[1]["image_url"]["url"]
            .as_str()
            .unwrap()
            .starts_with("data:image/jpeg;base64,"));
    }

    // ── 鉴权 ─────────────────────────────────────────────────

    #[test]
    fn anthropic_uses_api_key_header() {
        let h = auth_headers(&cfg(ProviderKind::Anthropic));
        assert!(h.iter().any(|(k, _)| *k == "x-api-key"));
        assert!(h.iter().any(|(k, _)| *k == "anthropic-version"));
        assert!(!h.iter().any(|(k, _)| *k == "authorization"));
    }

    #[test]
    fn openai_uses_bearer() {
        let h = auth_headers(&cfg(ProviderKind::OpenAi));
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].0, "authorization");
        assert!(h[0].1.starts_with("Bearer "));
    }

    #[test]
    fn ollama_sends_no_auth_header() {
        assert!(auth_headers(&cfg(ProviderKind::Ollama)).is_empty());
    }

    #[test]
    fn gemini_needs_query_key() {
        assert!(needs_query_key(&cfg(ProviderKind::Gemini)));
        assert!(!needs_query_key(&cfg(ProviderKind::OpenAi)));
    }

    #[test]
    fn empty_key_produces_no_headers() {
        let mut c = cfg(ProviderKind::OpenAi);
        c.api_key.clear();
        assert!(auth_headers(&c).is_empty());
    }

    // ── 响应解析 ─────────────────────────────────────────────

    #[test]
    fn parses_openai_reply() {
        let p = json!({
            "model": "gpt-4o",
            "choices": [{ "message": { "content": "你好" }, "finish_reason": "stop" }],
            "usage": { "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15 }
        });
        let r = parse_reply(&cfg(ProviderKind::OpenAi), &p).unwrap();
        assert_eq!(r.content, "你好");
        assert_eq!(r.usage.unwrap().total_tokens, Some(15));
        assert_eq!(r.model.as_deref(), Some("gpt-4o"));
    }

    #[test]
    fn parses_anthropic_reply_and_skips_thinking_blocks() {
        let p = json!({
            "content": [
                { "type": "thinking", "thinking": "hmm" },
                { "type": "text", "text": "答案" }
            ],
            "usage": { "input_tokens": 7, "output_tokens": 3 }
        });
        let r = parse_reply(&cfg(ProviderKind::Anthropic), &p).unwrap();
        assert_eq!(r.content, "答案");
        let u = r.usage.unwrap();
        assert_eq!(u.total_tokens, Some(10));
    }

    #[test]
    fn parses_gemini_reply() {
        let p = json!({
            "candidates": [{ "content": { "parts": [{ "text": "结果" }] }, "finishReason": "STOP" }],
            "usageMetadata": { "promptTokenCount": 4, "candidatesTokenCount": 6, "totalTokenCount": 10 }
        });
        let r = parse_reply(&cfg(ProviderKind::Gemini), &p).unwrap();
        assert_eq!(r.content, "结果");
        assert_eq!(r.usage.unwrap().total_tokens, Some(10));
    }

    #[test]
    fn parses_ollama_reply() {
        let p = json!({
            "message": { "role": "assistant", "content": "本地回答" },
            "prompt_eval_count": 12, "eval_count": 8, "done": true
        });
        let r = parse_reply(&cfg(ProviderKind::Ollama), &p).unwrap();
        assert_eq!(r.content, "本地回答");
        assert_eq!(r.usage.unwrap().total_tokens, Some(20));
    }

    #[test]
    fn shape_error_does_not_dump_full_body() {
        // 响应体里塞一段很长的用户原文，错误信息不该整段带出来
        let long = "机密内容".repeat(500);
        let p = json!({ "unexpected": long, "request_id": "abc" });
        let err = parse_reply(&cfg(ProviderKind::OpenAi), &p).unwrap_err();
        assert!(err.message.contains("choices[0]"));
        assert!(err.message.contains("unexpected"));
        // 输出长度被限住，不是 4000 字的原文
        assert!(err.message.chars().count() < 400, "错误串过长: {}", err.message.chars().count());
        assert!(!err.message.contains(&"机密内容".repeat(3)));
    }

    // ── 流式增量 ─────────────────────────────────────────────

    #[test]
    fn openai_delta_text() {
        let (t, done, _) = parse_delta(
            &cfg(ProviderKind::OpenAi),
            &json!({ "choices": [{ "delta": { "content": "你" } }] }),
        );
        assert_eq!(t.as_deref(), Some("你"));
        assert!(!done);
    }

    #[test]
    fn openai_delta_finish_reason_marks_done() {
        let (_, done, _) = parse_delta(
            &cfg(ProviderKind::OpenAi),
            &json!({ "choices": [{ "delta": {}, "finish_reason": "stop" }] }),
        );
        assert!(done);
    }

    #[test]
    fn anthropic_delta_only_from_content_block_delta() {
        let (t, _, _) = parse_delta(
            &cfg(ProviderKind::Anthropic),
            &json!({ "type": "content_block_delta", "delta": { "text": "好" } }),
        );
        assert_eq!(t.as_deref(), Some("好"));
        // message_start 不该被当成文本
        let (t2, _, _) = parse_delta(
            &cfg(ProviderKind::Anthropic),
            &json!({ "type": "message_start", "message": { "content": [] } }),
        );
        assert!(t2.is_none());
    }

    #[test]
    fn anthropic_message_stop_marks_done() {
        let (_, done, _) = parse_delta(
            &cfg(ProviderKind::Anthropic),
            &json!({ "type": "message_stop" }),
        );
        assert!(done);
    }

    #[test]
    fn gemini_delta_text() {
        let (t, _, _) = parse_delta(
            &cfg(ProviderKind::Gemini),
            &json!({ "candidates": [{ "content": { "parts": [{ "text": "字" }] } }] }),
        );
        assert_eq!(t.as_deref(), Some("字"));
    }

    #[test]
    fn ollama_delta_done_flag() {
        let (t, done, _) = parse_delta(
            &cfg(ProviderKind::Ollama),
            &json!({ "message": { "content": "尾" }, "done": true }),
        );
        assert_eq!(t.as_deref(), Some("尾"));
        assert!(done);
    }

    #[test]
    fn empty_delta_is_none_not_empty_string() {
        // 空文本与「没有文本」必须区分：前者会往输出里塞空串
        let (t, _, _) = parse_delta(
            &cfg(ProviderKind::OpenAi),
            &json!({ "choices": [{ "delta": { "content": "" } }] }),
        );
        assert!(t.is_none());
    }

    // ── 模型列表 ─────────────────────────────────────────────

    #[test]
    fn parses_openai_models() {
        let p = json!({ "data": [{ "id": "gpt-4o" }, { "id": "gpt-4o-mini" }] });
        assert_eq!(
            parse_models(&cfg(ProviderKind::OpenAi), &p).unwrap(),
            vec!["gpt-4o", "gpt-4o-mini"]
        );
    }

    #[test]
    fn parses_gemini_models_strips_prefix() {
        let p = json!({ "models": [{ "name": "models/gemini-2.0-flash" }] });
        assert_eq!(
            parse_models(&cfg(ProviderKind::Gemini), &p).unwrap(),
            vec!["gemini-2.0-flash"]
        );
    }

    #[test]
    fn parses_ollama_tags() {
        let p = json!({ "models": [{ "name": "qwen2.5:7b" }] });
        assert_eq!(
            parse_models(&cfg(ProviderKind::Ollama), &p).unwrap(),
            vec!["qwen2.5:7b"]
        );
    }

    #[test]
    fn empty_model_list_is_not_an_error() {
        // 有些自建代理不支持 /models，返回 {"data":[]} —— 这是"没有"，不是"坏了"
        let p = json!({ "data": [] });
        assert!(parse_models(&cfg(ProviderKind::OpenAi), &p).unwrap().is_empty());
    }

    // ── 工具声明翻译（pet 的 ai.rs 下沉，2026-10-26）──────────────

    fn tool() -> ToolDef {
        ToolDef::new(
            "get_weather",
            "查天气",
            json!({"type": "object", "properties": {"city": {"type": "string"}}}),
        )
    }

    #[test]
    fn tools_openai_shape_is_passthrough() {
        let b = build_body(&cfg(ProviderKind::OpenAi), &[Message::user("hi")], false, &[tool()]);
        let ts = b["tools"].as_array().unwrap();
        assert_eq!(ts.len(), 1);
        assert_eq!(ts[0]["type"], "function");
        assert_eq!(ts[0]["function"]["name"], "get_weather");
        assert!(ts[0]["function"]["parameters"]["properties"].is_object());
        // 没有工具时 tools 字段不能出现（部分网关见 null/[] 报 400）
        let b0 = build_body(&cfg(ProviderKind::OpenAi), &[Message::user("hi")], false, &[]);
        assert!(b0.get("tools").is_none());
    }

    #[test]
    fn tools_anthropic_uses_input_schema() {
        let b = build_body(&cfg(ProviderKind::Anthropic), &[Message::user("hi")], false, &[tool()]);
        let ts = b["tools"].as_array().unwrap();
        assert_eq!(ts[0]["name"], "get_weather");
        assert_eq!(ts[0]["input_schema"]["type"], "object");
        assert!(ts[0].get("function").is_none(), "不得残留 OpenAI 包裹");
    }

    #[test]
    fn tools_gemini_wraps_function_declarations() {
        let b = build_body(&cfg(ProviderKind::Gemini), &[Message::user("hi")], false, &[tool()]);
        let ts = b["tools"].as_array().unwrap();
        assert_eq!(ts.len(), 1);
        assert_eq!(ts[0]["functionDeclarations"][0]["name"], "get_weather");
    }

    #[test]
    fn tools_ollama_is_openai_shape() {
        let b = build_body(&cfg(ProviderKind::Ollama), &[Message::user("hi")], false, &[tool()]);
        assert_eq!(b["tools"][0]["type"], "function");
    }

    // ── 响应 tool_calls 归一（恒为 OpenAI 形状）──────────────────

    #[test]
    fn claude_tool_use_becomes_openai_shape() {
        let payload = json!({
            "content": [
                {"type": "text", "text": "我来查一下"},
                {"type": "tool_use", "id": "toolu_1", "name": "get_weather",
                 "input": {"city": "上海"}}
            ],
            "usage": {"input_tokens": 10, "output_tokens": 5},
            "model": "m1"
        });
        let r = parse_reply(&cfg(ProviderKind::Anthropic), &payload).unwrap();
        let tc = &r.tool_calls[0];
        assert_eq!(tc["type"], "function");
        assert_eq!(tc["id"], "toolu_1");
        assert_eq!(tc["function"]["name"], "get_weather");
        // arguments 必须是字符串（OpenAI 约定），Anthropic 的 input 是对象
        assert_eq!(tc["function"]["arguments"], r#"{"city":"上海"}"#);
    }

    #[test]
    fn gemini_function_call_becomes_openai_shape() {
        let payload = json!({
            "candidates": [{"content": {"parts": [
                {"text": "查一下"},
                {"functionCall": {"name": "get_weather", "args": {"city": "北京"}}}
            ]}}],
            "usageMetadata": {"promptTokenCount": 8, "candidatesTokenCount": 4, "totalTokenCount": 12}
        });
        let r = parse_reply(&cfg(ProviderKind::Gemini), &payload).unwrap();
        let tc = &r.tool_calls[0];
        assert_eq!(tc["type"], "function");
        assert_eq!(tc["function"]["name"], "get_weather");
        assert_eq!(tc["function"]["arguments"], r#"{"city":"北京"}"#);
        // Gemini 不给 id 时要有稳定的合成 id
        assert_eq!(tc["id"], "gemini-call-0");
    }

    #[test]
    fn ollama_tool_call_arguments_become_string() {
        let payload = json!({
            "message": {
                "content": "",
                "tool_calls": [{"function": {"name": "get_weather", "arguments": {"city": "广州"}}}]
            },
            "prompt_eval_count": 5, "eval_count": 3
        });
        let r = parse_reply(&cfg(ProviderKind::Ollama), &payload).unwrap();
        assert_eq!(r.tool_calls[0]["function"]["arguments"], r#"{"city":"广州"}"#);
    }

    // ── Anthropic 视觉（content 数组）───────────────────────────

    #[test]
    fn anthropic_images_become_base64_source_blocks() {
        let m = Message::user("看图").with_images(vec!["data:image/png;base64,QUJD".into()]);
        let b = build_body(&cfg(ProviderKind::Anthropic), &[m], false, &[]);
        let content = b["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "text");
        let img = &content[1];
        assert_eq!(img["type"], "image");
        assert_eq!(img["source"]["type"], "base64");
        assert_eq!(img["source"]["media_type"], "image/png");
        assert_eq!(img["source"]["data"], "QUJD", "data: 前缀必须剥掉");
    }
}
