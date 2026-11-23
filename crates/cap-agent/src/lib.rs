//! # cap-agent — z-biz-tool 原生 Agent 层
//!
//! 一个**不依赖 Node 运行时**的 agent 能力层：工具注册表 + tool calling
//! 执行循环 + MCP stdio 服务端，三样共用同一份工具定义。
//!
//! ## 为什么要有这一层
//!
//! 家族里原先只有 file 一个 tool 带「内置 AI agent」，做法是：打包一份
//! Node.js（**108 MB**）+ agent-proxy（JS，1668 行）+ Pi agent，做成
//! `Resources/runtime/` 塞进 .app。
//!
//! | 东西 | 体积 | 问题 |
//! |---|---|---|
//! | 捆绑 node | 108 MB | 只为跑一个 JS 代理 |
//! | 主二进制 | 49 MB | 其中 34.7 MB 是 zip(lzma/xz/deflate64)+unrar+7z 静态链接 |
//! | `runtime/lib/pi` | 36 KB | **只有 manifest，Pi 根本没装上** —— 这份死重是空的 |
//!
//! 而这一层能提供的东西，`cap-llm` 其实已经准备好了：[`cap_llm::chat_with_tools`]
//! 已经支持工具声明，`cap_llm::Message::tool` 已经支持按各家协议回灌工具结果。
//! **缺的只是执行循环** —— 「模型要调工具 → 执行 → 喂回去 → 再问」。
//!
//! ## 两个形态
//!
//! ```text
//!              ┌──────────────── ToolRegistry ────────────────┐
//!              │  name / description / JSON Schema / call() │
//!              └───────┬───────────────────────────┬──────────┘
//!                      │                           │
//!        ┌─────────────▼─────────────┐   ┌─────────▼──────────────┐
//!        │ 内置 agent loop           │   │ MCP stdio server       │
//!        │ crate::loop::run          │   │ crate::mcp::McpServer  │
//!        │ 模型自己决定调哪些工具     │   │ 外部 CLI（Pi/codex/…） │
//!        │ 工具 = App 自己的函数      │   │ 通过 stdio 协议调用     │
//!        └───────────────────────────┘   └────────────────────────┘
//! ```
//!
//! **内部形态**：App 里的 AI 面板直接用 Rust 循环。工具就是 App 自己的
//! Tauri 命令 —— 不用绕 HTTP，不用起子进程，不用带 node。
//!
//! **外部形态**：同一个工具注册表以 MCP 协议暴露，外部 agent（Pi / claude-code /
//! [codex](https://github.com/openai/codex)，它本身就是 Rust 写的）可以来调 App 的能力。
//! 这条让「App 是被 agent 使用的工具」这件事不需要 App 自带任何 agent 实现。
//!
//! ## 用法
//!
//! ```no_run
//! use cap_agent::{AgentOptions, LlmChat, ToolRegistry};
//! use cap_llm::{LlmConfig, Message};
//!
//! # async fn demo() -> Result<(), Box<dyn std::error::Error>> {
//! let mut reg = ToolRegistry::new();
//! reg.register_fn(
//!     "list_dir",
//!     "列出目录下的文件",
//!     serde_json::json!({"type":"object","properties":{"path":{"type":"string"}}}),
//!     |args| {
//!         let path = args["path"].as_str().unwrap_or_default().to_string();
//!         Box::pin(async move { Ok(format!("contents of {path}")) })
//!             as cap_agent::ToolFuture<'static>
//!     },
//! )?;
//!
//! let cfg = LlmConfig::default();
//! let outcome = cap_agent::run(
//!     &LlmChat,
//!     &cfg,
//!     vec![Message::user("这个目录里有什么？")],
//!     &reg,
//!     &AgentOptions::default(),
//!     |e| println!("{e:?}"),
//! )
//! .await?;
//! println!("{}", outcome.text);
//! # Ok(())
//! # }
//! ```
//!
//! ## 边界（这一层不做什么）
//!
//! - **不持密钥、不写盘**：跟 cap-llm 同规矩。密钥怎么存是各仓自己的事。
//! - **不做权限/确认策略**：「这个工具要不要先问用户」是业务语义，由调用方
//!   在注册工具时决定（不注册＝模型看不见）。
//! - **不做计费与配额**。
//! - **不替调用方决定并发**：一次只有一轮对话在跑，串行。

pub mod loop_;
pub mod mcp;
pub mod tool;

pub use loop_::{
    run, run_streaming, AgentError, AgentEvent, AgentOptions, AgentOutcome, ChatBackend, LlmChat,
};
pub use mcp::{McpServer, DEFAULT_PROTOCOL_VERSION};
pub use tool::{Tool, ToolError, ToolFuture, ToolOutput, ToolRegistry};
