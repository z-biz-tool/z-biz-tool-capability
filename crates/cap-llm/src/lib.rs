//! # cap-llm — 统一 AI 接入层
//!
//! 把家族里**十套各自为政的 LLM 接入**归并成一个 crate：一套配置模型、
//! 一套协议适配、一套错误语义、一套流式解码。
//!
//! ## 归并了什么（都是实测过的重复）
//!
//! | 仓 | 原来在哪 | 行数 | 问题 |
//! |---|---|---|---|
//! | file | `src-tauri/src/ai.rs` | 247 | 错误体按字节截断，中文响应直接 panic |
//! | pet | `src-tauri/src/ai.rs` | 1,171 | 唯一覆盖 9 家 provider 的，但绑在宠物业务里 |
//! | aigen | `src-tauri/src/ai_client.rs` | 3,227 | 自带 retry/backoff/mask_key，另 9 家都没有 |
//! | db | `src-tauri/src/report/ai.rs` | 2,808 | 接入代码埋在报告生成里 |
//! | note | `src-tauri/src/commands.rs` | 1,863 | 接入代码埋在 1863 行的 commands 里 |
//! | kb | `src-tauri/src/rag.rs` | 1,192 | 自己一套 LlmConfig |
//! | terminal | `src/services/aiClient.ts` | 381 | **前端直接 fetch，key 在前端内存里** |
//! | mind | `src/services/ai.ts` | 451 | 同上 |
//! | worker | `src-tauri/src/agent_session.rs` | 755 | 走 CLI 代理，不是 HTTP 直连 |
//! | shared | `src/ai/clients.ts` | 644 | TS 侧唯一写对的一份，但各仓没用它 |
//!
//! 合计 **12,898 行**，其中大量是同一条 `/chat/completions` 请求的第十次重写。
//!
//! ## 边界：这一层做什么、不做什么
//!
//! **做**：provider 地址归一、鉴权头、请求体构造、响应解析、SSE 流式解码、
//! 错误分类、密钥脱敏、模型列表、连通性探测。
//!
//! **不做**（这些留在各自的业务仓）：
//! - **提示词**。「你是简洁的文件摘要助手」属于 file，「按目录生成报告」
//!   属于 db —— 提示词是产品差异，不是基础设施。
//! - **业务编排**。任务队列、重试策略、并发控制、费用记账。
//! - **密钥存储**。本 crate 只在内存里持有调用方传进来的 `api_key`，
//!   不读配置文件、不落盘、不打印。存哪儿是各仓自己的事
//!   （file 用 `app_config_dir/llm.json`，note 用 `secrets.rs`）。
//! - **图片/视频生成**。aigen 的 `generate_image_api` 是异步任务轮询协议，
//!   与 chat 完全不同，不在本轮范围。
//!
//! ## 与 cap-img / cap-doc 的关系
//!
//! capability 层里前四个 crate（img / audio / video / doc）都是**离线原语**：
//! 路径进、路径出、不联网。`cap-llm` 是第一个**联网**的 —— 但它不持密钥、
//! 不写盘、不缓存，仍然是「无状态原语」。这个区别在 README 里单独标注了，
//! 免得后来人以为 capability 层可以随便放需要登录态的东西。
//!
//! ## 用法
//!
//! ```no_run
//! use cap_llm::{chat, chat_stream, LlmConfig, Message, ProviderKind};
//! use std::time::Duration;
//!
//! # async fn demo() -> Result<(), cap_llm::LlmError> {
//! let cfg = LlmConfig {
//!     kind: ProviderKind::parse("minimax")?,
//!     base_url: String::new(),          // 留空用默认地址
//!     api_key: std::env::var("MINIMAX_API_KEY").unwrap_or_default(),
//!     model: "MiniMax-M3.1-Flash".into(),
//!     timeout: Duration::from_secs(60),
//!     ..Default::default()
//! };
//!
//! // 非流式
//! let reply = chat(&cfg, &[Message::system("你是助手"), Message::user("你好")]).await?;
//! println!("{}", reply.content);
//!
//! // 流式
//! chat_stream(&cfg, &[Message::user("讲个长故事")], |d| {
//!     if let cap_llm::Delta::Text(t) = d {
//!         print!("{t}");
//!     }
//! })
//! .await?;
//! # Ok(())
//! # }
//! ```
//!
//! ## 三个「别退回旧写法」的约定
//!
//! 1. **不静默回落**。provider 不认识就报错，不偷偷按 OpenAI 发。配置写 A、
//!    实际打 B 是最难查的一类故障（shared 仓 01/A-11 记录过这个病）。
//! 2. **错误文本必须过 `redact` + `truncate_chars`**。前者防 key 泄露
//!    （网关会回显请求头），后者防中文截断 panic（file 仓的旧 bug）。
//! 3. **流式必须用 `Utf8LineDecoder`**。手写 `from_utf8_lossy` 会让中文
//!    跨 chunk 时出现方块，而且只在大模型吐长回答时偶发。

#![forbid(unsafe_code)]

mod asr;
mod chat;
mod config;
mod error;
mod message;
mod presets;
mod sse;
mod wire;

pub use asr::{transcribe, transcribe_file, Segment, Transcript, TranscriptionRequest};
pub use chat::{chat, chat_stream, chat_stream_if, chat_with_tools, list_models, ping};
pub use config::{is_private_endpoint, Action, LlmConfig, Protocol, ProviderKind};
pub use error::{redact, truncate_chars, ErrorKind, LlmError};
pub use message::{ChatReply, Delta, Message, Role, ToolDef, Usage};

/// 供应商预设目录（借鉴 cc-switch 的 preset 模型）。
///
/// 选一家 → 地址协议自动填好 → 只补 API Key 与模型名。
pub mod preset {
    pub use crate::presets::{
        by_category, config_for, find, find_by_name, Category, ProviderPreset, PRESETS,
    };
}

/// 协议层的高级入口。**大多数调用方用不到** —— 直接 `chat` / `chat_stream` 即可。
///
/// 需要它们的场景是「自己控制传输」：例如某个产品想在请求之间插自定义重试、
/// 换 HTTP 客户端、或者把同一份报文发给自有网关。这时不用重写协议，
/// 只借用报文构造与响应解析。
pub mod protocol {
    pub use crate::wire::{
        auth_headers, build_body as build_request_body, needs_query_key,
        parse_delta as parse_stream_delta, parse_models as parse_model_list,
        parse_reply as parse_response,
    };
}

/// 本 crate 的版本，便于上层在日志里标注「用的是哪一版接入层」。
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// 默认最大输出 token 数（Anthropic 必填项的兜底值）。
pub use wire::DEFAULT_MAX_TOKENS;
