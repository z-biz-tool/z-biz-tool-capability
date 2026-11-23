//! Agent 执行循环：模型要调工具 → 执行 → 把结果喂回去 → 再问，直到模型不再调工具。
//!
//! 这一层是 cap-llm **故意不做**的（见 cap-llm 的 `chat_with_tools` 文档）：
//! 「执行循环由消费方自己驱动 —— 那是业务语义（哪些工具、怎么确认、怎么限流），
//! 不是传输细节」。本 crate 就是家族里第一个通用的那份「业务语义」。
//!
//! ## 为什么循环要能离线测
//!
//! [`ChatBackend`] 把「调模型」抽象成 trait：生产用 [`LlmChat`]（走 cap-llm），
//! 测试用脚本化的假后端。**没有网络也能把「模型连调三次工具、参数解析对不对、
//! 工具报错后会不会把对话带崩」这些真问题测掉** —— 这类循环最容易出的 bug
//! 恰恰在回灌顺序上，而回灌顺序只有在假后端上才测得动。

use std::future::Future;
use std::pin::Pin;

use cap_llm::{
    chat_stream, chat_with_tools, ChatReply, LlmConfig, LlmError, Message, ToolDef, Usage,
};
use serde_json::Value;

use crate::tool::{ToolError, ToolRegistry};

/// 一次「调模型」的后端。
pub trait ChatBackend: Send + Sync {
    fn chat<'a>(
        &'a self,
        cfg: &'a LlmConfig,
        messages: &'a [Message],
        tools: &'a [ToolDef],
    ) -> Pin<Box<dyn Future<Output = Result<ChatReply, LlmError>> + Send + 'a>>;
}

/// 生产后端：走 cap-llm。
pub struct LlmChat;

impl ChatBackend for LlmChat {
    fn chat<'a>(
        &'a self,
        cfg: &'a LlmConfig,
        messages: &'a [Message],
        tools: &'a [ToolDef],
    ) -> Pin<Box<dyn Future<Output = Result<ChatReply, LlmError>> + Send + 'a>> {
        Box::pin(async move { chat_with_tools(cfg, messages, tools).await })
    }
}

/// 循环参数。
#[derive(Debug, Clone)]
pub struct AgentOptions {
    /// 最多几轮「模型 → 工具 → 模型」。超了判失败，不无限跑。
    pub max_steps: usize,
    /// 单次工具输出截断长度。工具可能吐出几 MB 的目录列表，全塞回上下文
    /// 会把窗口吃光 —— 截断并在末尾标注，模型能看出来「还有内容但被截了」。
    pub max_tool_output_chars: usize,
}

impl Default for AgentOptions {
    fn default() -> Self {
        Self {
            max_steps: 8,
            max_tool_output_chars: 8000,
        }
    }
}

/// 循环过程中发生的事。给上层（面板 / CLI）用来出进度。
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// 模型这一轮说了什么（可能为空 —— 它可能只调工具不说话）。
    AssistantText(String),
    /// 决定调某个工具。
    ToolCall {
        id: String,
        name: String,
        arguments: Value,
    },
    /// 工具跑完了。`is_error` 为真表示工具失败，结果是错误文本。
    ToolResult {
        id: String,
        name: String,
        output: String,
        is_error: bool,
    },
}

/// 一轮跑完的产物。
#[derive(Debug, Clone)]
pub struct AgentOutcome {
    /// 完整对话历史（含 assistant 的 tool_calls 与每条工具结果），
    /// 可以直接存起来做多轮续聊。
    pub messages: Vec<Message>,
    /// 跑了几轮。
    pub steps: usize,
    /// 模型最后的回答。
    pub text: String,
    /// token 用量。最后一轮的（各家只在最后一轮回传完整用量）。
    pub usage: Option<Usage>,
}

/// Agent 层错误。
#[derive(Debug)]
pub enum AgentError {
    /// 模型调用失败（网络、超时、解析、鉴权……）。
    Llm(LlmError),
    /// 达到最大轮数仍未收敛。
    StepLimit { max_steps: usize },
}

impl std::fmt::Display for AgentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AgentError::Llm(e) => write!(f, "模型调用失败：{e}"),
            AgentError::StepLimit { max_steps } => {
                write!(f, "超过最大轮数 {max_steps}，仍未得到最终回答")
            }
        }
    }
}

impl std::error::Error for AgentError {}

impl From<LlmError> for AgentError {
    fn from(e: LlmError) -> Self {
        AgentError::Llm(e)
    }
}

/// 跑一轮 agent 循环（不流式）。
///
/// `on_event` 每发生一件事调一次；不需要就传 `|_| {}`。
pub async fn run<B: ChatBackend + ?Sized>(
    backend: &B,
    cfg: &LlmConfig,
    messages: Vec<Message>,
    registry: &ToolRegistry,
    opts: &AgentOptions,
    mut on_event: impl FnMut(AgentEvent),
) -> Result<AgentOutcome, AgentError> {
    let mut history = messages;
    let defs = registry.defs();

    for step in 1..=opts.max_steps {
        let reply = backend.chat(cfg, &history, &defs).await?;

        if reply.tool_calls.is_empty() {
            if !reply.content.trim().is_empty() {
                on_event(AgentEvent::AssistantText(reply.content.clone()));
            }
            return Ok(AgentOutcome {
                messages: history,
                steps: step,
                text: reply.content,
                usage: reply.usage,
            });
        }

        // assistant 这一轮带 tool_calls 的消息**必须原样回灌**，
        // 服务端要靠它把后续 tool 结果配对（OpenAI 会校验）。
        history.push(Message::assistant_with_tools(
            reply.content.clone(),
            reply.tool_calls.clone(),
        ));
        if !reply.content.trim().is_empty() {
            on_event(AgentEvent::AssistantText(reply.content.clone()));
        }

        for call in &reply.tool_calls {
            let parsed = parse_tool_call(call);
            let id = parsed.id.clone();
            let name = parsed.name.clone();

            on_event(AgentEvent::ToolCall {
                id: id.clone(),
                name: name.clone(),
                arguments: parsed.arguments.clone().unwrap_or(Value::Null),
            });

            let result = match parsed.arguments {
                Err(msg) => Err(ToolError::InvalidArguments(msg)),
                Ok(args) => match registry.get(&name) {
                    Some(tool) => tool.call(args).await,
                    None => Err(ToolError::NotFound(format!(
                        "没有名为 {name} 的工具。可用工具：{}",
                        registry.names().join(", ")
                    ))),
                },
            };

            let (text, is_error) = match result {
                Ok(out) => (truncate_output(out, opts.max_tool_output_chars), false),
                Err(err) => (err.to_model_text(), true),
            };

            on_event(AgentEvent::ToolResult {
                id: id.clone(),
                name: name.clone(),
                output: text.clone(),
                is_error,
            });

            // 工具结果按 role:"tool" 单独成条，两个 id 都带：
            // OpenAI/Anthropic 靠 tool_call_id 配对，Gemini/Ollama 靠 tool_name。
            history.push(Message::tool(id, name, text));
        }
    }

    Err(AgentError::StepLimit {
        max_steps: opts.max_steps,
    })
}

/// 跑一轮「纯对话」（没有工具），**带流式**。
///
/// 有工具时模型可能先调工具再说话，中途没有可流的内容；cap-llm 的流式
/// 接口也还不带工具声明（见其 `chat_stream` 文档）。所以流式只用在
/// 「这次就是要它写字」的场景 —— 面板里「解释这段」「起个标题」这类。
pub async fn run_streaming(
    cfg: &LlmConfig,
    messages: Vec<Message>,
    mut on_delta: impl FnMut(&str),
) -> Result<AgentOutcome, AgentError> {
    let reply = chat_stream(cfg, &messages, |delta| {
        if let cap_llm::Delta::Text(t) = delta {
            on_delta(&t);
        }
    })
    .await?;
    Ok(AgentOutcome {
        messages,
        steps: 1,
        text: reply.content,
        usage: reply.usage,
    })
}

struct ParsedCall {
    id: String,
    name: String,
    arguments: Result<Value, String>,
}

/// 解析 OpenAI 形状的 tool_call。
///
/// `arguments` 正常是 **JSON 字符串**，但有厂商会直接给对象；两种都收。
/// 解析失败不抛错，而是回一条错误给模型 —— 让它自己改参数重试，
/// 比整轮对话崩掉好。
fn parse_tool_call(call: &Value) -> ParsedCall {
    let id = call
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let func = call.get("function").unwrap_or(&Value::Null);
    let name = func
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();

    let arguments = match func.get("arguments") {
        Some(Value::String(s)) => serde_json::from_str::<Value>(s)
            .map_err(|e| format!("工具 {name} 的 arguments 不是合法 JSON：{e}")),
        Some(v @ Value::Object(_)) => Ok(v.clone()),
        Some(other) => Err(format!("工具 {name} 的 arguments 形状不对：{}", other)),
        None => Ok(Value::Object(Default::default())),
    };

    ParsedCall {
        id,
        name,
        arguments,
    }
}

fn truncate_output(text: String, max: usize) -> String {
    if text.chars().count() <= max {
        return text;
    }
    let head: String = text.chars().take(max).collect();
    format!("{head}\n…（输出过长，已截断）")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 脚本化后端：按预设依次返回 reply，并记录收到的消息历史。
    struct FakeBackend {
        replies: std::sync::Mutex<Vec<ChatReply>>,
        seen: std::sync::Mutex<Vec<Vec<Message>>>,
    }

    impl FakeBackend {
        fn new(replies: Vec<ChatReply>) -> Self {
            Self {
                replies: std::sync::Mutex::new(replies),
                seen: std::sync::Mutex::new(Vec::new()),
            }
        }
        fn calls(&self) -> usize {
            self.seen.lock().unwrap().len()
        }
        fn last_seen(&self) -> Vec<Message> {
            self.seen
                .lock()
                .unwrap()
                .last()
                .cloned()
                .unwrap_or_default()
        }
    }

    impl ChatBackend for FakeBackend {
        fn chat<'a>(
            &'a self,
            _cfg: &'a LlmConfig,
            messages: &'a [Message],
            _tools: &'a [ToolDef],
        ) -> Pin<Box<dyn Future<Output = Result<ChatReply, LlmError>> + Send + 'a>> {
            let mut replies = self.replies.lock().unwrap();
            let reply = replies.remove(0);
            self.seen.lock().unwrap().push(messages.to_vec());
            Box::pin(async move { Ok(reply) })
        }
    }

    fn tool_call(id: &str, name: &str, args: &str) -> Value {
        json!({"id": id, "type": "function", "function": {"name": name, "arguments": args}})
    }

    fn cfg() -> LlmConfig {
        LlmConfig::default()
    }

    fn registry_with_sum() -> ToolRegistry {
        let mut reg = ToolRegistry::new();
        reg.register_fn(
            "sum",
            "求和",
            json!({"type":"object","properties":{"a":{"type":"number"}}}),
            |args| {
                Box::pin(async move { Ok(args["a"].as_f64().unwrap_or(0.0).to_string()) })
                    as crate::tool::ToolFuture<'static>
            },
        )
        .unwrap();
        reg
    }

    #[tokio::test]
    async fn no_tool_call_returns_text() {
        let backend = FakeBackend::new(vec![ChatReply {
            content: "你好".into(),
            ..Default::default()
        }]);
        let out = run(
            &backend,
            &cfg(),
            vec![Message::user("hi")],
            &ToolRegistry::new(),
            &AgentOptions::default(),
            |_| {},
        )
        .await
        .unwrap();
        assert_eq!(out.text, "你好");
        assert_eq!(out.steps, 1);
        assert_eq!(backend.calls(), 1);
    }

    #[tokio::test]
    async fn tool_call_is_executed_and_fed_back() {
        let backend = FakeBackend::new(vec![
            ChatReply {
                content: "我来算".into(),
                tool_calls: vec![tool_call("call_1", "sum", "{\"a\":7}")],
                ..Default::default()
            },
            ChatReply {
                content: "结果是 7".into(),
                ..Default::default()
            },
        ]);
        let mut events = Vec::new();
        let out = run(
            &backend,
            &cfg(),
            vec![Message::user("算一下")],
            &registry_with_sum(),
            &AgentOptions::default(),
            |e| events.push(e),
        )
        .await
        .unwrap();

        assert_eq!(out.text, "结果是 7");
        assert_eq!(out.steps, 2);
        assert_eq!(backend.calls(), 2);

        // 回灌顺序：user → assistant(带 tool_calls) → tool 结果
        let seen = backend.last_seen();
        assert_eq!(seen.len(), 3);
        assert_eq!(seen[0].role, cap_llm::Role::User);
        assert_eq!(seen[1].role, cap_llm::Role::Assistant);
        assert_eq!(
            seen[1].tool_calls.len(),
            1,
            "assistant 必须带原样 tool_calls"
        );
        assert_eq!(seen[2].role, cap_llm::Role::Tool);
        assert_eq!(seen[2].tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(seen[2].tool_name.as_deref(), Some("sum"));
        assert_eq!(seen[2].content, "7");

        assert!(events
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolCall { name, .. } if name == "sum")));
        assert!(events.iter().any(|e| matches!(
            e,
            AgentEvent::ToolResult {
                is_error: false,
                ..
            }
        )));
    }

    #[tokio::test]
    async fn unknown_tool_becomes_error_message_not_crash() {
        let backend = FakeBackend::new(vec![
            ChatReply {
                content: String::new(),
                tool_calls: vec![tool_call("call_1", "nope", "{}")],
                ..Default::default()
            },
            ChatReply {
                content: "抱歉".into(),
                ..Default::default()
            },
        ]);
        run(
            &backend,
            &cfg(),
            vec![Message::user("go")],
            &registry_with_sum(),
            &AgentOptions::default(),
            |_| {},
        )
        .await
        .unwrap();
        let seen = backend.last_seen();
        assert_eq!(seen[2].role, cap_llm::Role::Tool);
        assert!(
            seen[2].content.contains("没有名为 nope 的工具"),
            "{}",
            seen[2].content
        );
    }

    #[tokio::test]
    async fn bad_arguments_are_reported_back_to_model() {
        let backend = FakeBackend::new(vec![
            ChatReply {
                content: String::new(),
                tool_calls: vec![tool_call("call_1", "sum", "not-json")],
                ..Default::default()
            },
            ChatReply {
                content: "换参数".into(),
                ..Default::default()
            },
        ]);
        run(
            &backend,
            &cfg(),
            vec![Message::user("go")],
            &registry_with_sum(),
            &AgentOptions::default(),
            |_| {},
        )
        .await
        .unwrap();
        let seen = backend.last_seen();
        assert!(seen[2].content.contains("arguments"), "{}", seen[2].content);
    }

    #[tokio::test]
    async fn step_limit_stops_the_loop() {
        let reply = ChatReply {
            content: String::new(),
            tool_calls: vec![tool_call("call_1", "sum", "{\"a\":1}")],
            ..Default::default()
        };
        let backend = FakeBackend::new(vec![reply.clone(), reply.clone(), reply]);
        let opts = AgentOptions {
            max_steps: 2,
            ..Default::default()
        };
        let err = run(
            &backend,
            &cfg(),
            vec![Message::user("go")],
            &registry_with_sum(),
            &opts,
            |_| {},
        )
        .await
        .unwrap_err();
        assert!(matches!(err, AgentError::StepLimit { max_steps: 2 }));
        assert_eq!(backend.calls(), 2, "不该超过 max_steps 次调用");
    }

    #[tokio::test]
    async fn arguments_may_be_an_object() {
        let backend = FakeBackend::new(vec![
            ChatReply {
                content: String::new(),
                tool_calls: vec![tool_call("call_1", "sum", "{\"a\":3}")],
                ..Default::default()
            },
            ChatReply {
                content: "ok".into(),
                ..Default::default()
            },
        ]);
        run(
            &backend,
            &cfg(),
            vec![Message::user("go")],
            &registry_with_sum(),
            &AgentOptions::default(),
            |_| {},
        )
        .await
        .unwrap();
        let seen = backend.last_seen();
        assert_eq!(seen[2].content, "3");
    }

    #[test]
    fn long_output_is_truncated() {
        let long = "x".repeat(100);
        let out = truncate_output(long, 10);
        assert!(out.starts_with("xxxxxxxxxx"));
        assert!(out.contains("已截断"));
    }
}
