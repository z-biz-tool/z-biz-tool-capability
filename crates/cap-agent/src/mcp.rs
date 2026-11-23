//! MCP（Model Context Protocol）stdio 服务端：把同一份工具注册表暴露给外部 agent。
//!
//! ## 这一层解决什么
//!
//! 自建 agent loop（[`crate::loop`]）解决「App 自己就用 AI 干活」；
//! MCP 解决「**别的** agent 来用 App 的能力」—— Pi / claude-code / codex
//! 这些外部 CLI 通过 stdio 说 MCP 协议，就能调用 App 注册的工具。
//!
//! 两个形态共用**同一份** [`ToolRegistry`]：注册一次，既能被内部 agent loop 用，
//! 也能被外部 agent 用。这是这一层存在的全部意义 —— 否则两边会各写一份工具表，
//! 慢慢长歪。
//!
//! ## 传输
//!
//! stdio：每行一条 JSON-RPC 2.0 消息（换行分隔）。请求回响应，通知不回。
//! 这里手写而不是引 rmcp，是为了让这个 crate **零新增依赖** —— 它要被 13 个
//! app 以 path 依赖链进来，每多一个依赖就多一份编译与升级成本。
//! 要换 rmcp 时，只需替换 `handle` 的实现，上层不受影响。

use std::io::{BufRead, Write};

use serde_json::{json, Value};

use crate::tool::ToolRegistry;

/// 本实现支持的协议版本。客户端报别的版本时，我们回它报的那个
/// （服务端回自己的版本会让严格客户端直接拒连）。
pub const DEFAULT_PROTOCOL_VERSION: &str = "2025-06-18";

/// MCP 服务端。
pub struct McpServer {
    registry: ToolRegistry,
    name: String,
    version: String,
}

impl McpServer {
    pub fn new(registry: ToolRegistry) -> Self {
        Self {
            registry,
            name: "z-biz-tool-cap-agent".into(),
            version: env!("CARGO_PKG_VERSION").into(),
        }
    }

    pub fn with_info(
        registry: ToolRegistry,
        name: impl Into<String>,
        version: impl Into<String>,
    ) -> Self {
        Self {
            registry,
            name: name.into(),
            version: version.into(),
        }
    }

    pub fn registry(&self) -> &ToolRegistry {
        &self.registry
    }

    /// 处理一条 JSON-RPC 消息。
    ///
    /// 返回 `None` 表示这是**通知**（不该回响应）。其余都回一条响应对象。
    pub async fn handle(&self, req: &Value) -> Option<Value> {
        let method = req.get("method").and_then(|v| v.as_str()).unwrap_or("");
        let id = req.get("id").cloned();
        let params = req.get("params").cloned().unwrap_or(json!({}));

        // 通知没有 id，不回响应。
        let id = id?;

        let result = match method {
            "initialize" => {
                let requested = params
                    .get("protocolVersion")
                    .and_then(|v| v.as_str())
                    .unwrap_or(DEFAULT_PROTOCOL_VERSION);
                Ok(json!({
                    "protocolVersion": requested,
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": self.name, "version": self.version},
                }))
            }
            "ping" => Ok(json!({})),
            "tools/list" => {
                let tools: Vec<Value> = self
                    .registry
                    .defs()
                    .into_iter()
                    .map(|d| {
                        json!({
                            "name": d.name,
                            "description": d.description,
                            "inputSchema": d.parameters,
                        })
                    })
                    .collect();
                Ok(json!({"tools": tools}))
            }
            "tools/call" => {
                let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
                match self.registry.get(name) {
                    None => Ok(json!({
                        "content": [{"type": "text", "text": format!("没有名为 {name} 的工具")}],
                        "isError": true,
                    })),
                    Some(tool) => match tool.call(arguments).await {
                        Ok(out) => Ok(json!({
                            "content": [{"type": "text", "text": out}],
                            "isError": false,
                        })),
                        Err(err) => Ok(json!({
                            "content": [{"type": "text", "text": err.to_model_text()}],
                            "isError": true,
                        })),
                    },
                }
            }
            other => Err(json!({
                "code": -32601,
                "message": format!("不支持的方法：{other}"),
            })),
        };

        Some(match result {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err(err) => json!({"jsonrpc": "2.0", "id": id, "error": err}),
        })
    }

    /// 跑 stdio 服务循环：从 stdin 读一行、回一行，直到 stdin 关闭。
    pub async fn serve_stdio(&self) -> std::io::Result<()> {
        let stdin = std::io::stdin();
        let mut stdout = std::io::stdout();
        let mut reader = stdin.lock();

        let mut line = String::new();
        loop {
            line.clear();
            let n = reader.read_line(&mut line)?;
            if n == 0 {
                break;
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let response = match serde_json::from_str::<Value>(trimmed) {
                Ok(req) => self.handle(&req).await,
                Err(e) => Some(json!({
                    "jsonrpc": "2.0",
                    "id": Value::Null,
                    "error": {"code": -32700, "message": format!("JSON 解析失败：{e}")},
                })),
            };
            if let Some(resp) = response {
                writeln!(stdout, "{resp}")?;
                stdout.flush()?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolFuture;

    fn server() -> McpServer {
        let mut reg = ToolRegistry::new();
        reg.register_fn(
            "echo",
            "回显输入",
            json!({"type":"object","properties":{"text":{"type":"string"}}}),
            |args| {
                Box::pin(async move { Ok(args["text"].as_str().unwrap_or_default().to_string()) })
                    as ToolFuture<'static>
            },
        )
        .unwrap();
        McpServer::new(reg)
    }

    #[tokio::test]
    async fn initialize_reports_capabilities() {
        let s = server();
        let resp = s
            .handle(&json!({"jsonrpc":"2.0","id":1,"method":"initialize",
                            "params":{"protocolVersion":"2025-06-18"}}))
            .await
            .unwrap();
        assert_eq!(resp["result"]["protocolVersion"], "2025-06-18");
        assert!(resp["result"]["capabilities"]["tools"].is_object());
        assert_eq!(resp["result"]["serverInfo"]["name"], "z-biz-tool-cap-agent");
    }

    #[tokio::test]
    async fn initialize_echoes_client_protocol_version() {
        let s = server();
        let resp = s
            .handle(&json!({"jsonrpc":"2.0","id":1,"method":"initialize",
                            "params":{"protocolVersion":"2024-11-05"}}))
            .await
            .unwrap();
        assert_eq!(resp["result"]["protocolVersion"], "2024-11-05");
    }

    #[tokio::test]
    async fn tools_list_exposes_schema() {
        let s = server();
        let resp = s
            .handle(&json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}))
            .await
            .unwrap();
        let tools = resp["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "echo");
        assert_eq!(tools[0]["inputSchema"]["type"], "object");
    }

    #[tokio::test]
    async fn tools_call_runs_the_tool() {
        let s = server();
        let resp = s
            .handle(&json!({"jsonrpc":"2.0","id":3,"method":"tools/call",
                            "params":{"name":"echo","arguments":{"text":"hi"}}}))
            .await
            .unwrap();
        assert_eq!(resp["result"]["content"][0]["text"], "hi");
        assert_eq!(resp["result"]["isError"], false);
    }

    #[tokio::test]
    async fn tools_call_unknown_tool_is_error_result_not_protocol_error() {
        let s = server();
        let resp = s
            .handle(&json!({"jsonrpc":"2.0","id":4,"method":"tools/call",
                            "params":{"name":"ghost","arguments":{}}}))
            .await
            .unwrap();
        assert_eq!(resp["result"]["isError"], true);
        assert!(resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("ghost"));
    }

    #[tokio::test]
    async fn notification_gets_no_response() {
        let s = server();
        let resp = s
            .handle(&json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
            .await;
        assert!(resp.is_none());
    }

    #[tokio::test]
    async fn unknown_method_is_method_not_found() {
        let s = server();
        let resp = s
            .handle(&json!({"jsonrpc":"2.0","id":5,"method":"resources/list"}))
            .await
            .unwrap();
        assert_eq!(resp["error"]["code"], -32601);
    }

    #[tokio::test]
    async fn ping_is_answered() {
        let s = server();
        let resp = s
            .handle(&json!({"jsonrpc":"2.0","id":6,"method":"ping"}))
            .await
            .unwrap();
        assert!(resp["result"].is_object());
    }
}
