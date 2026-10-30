//! 消息与结果类型。
//!
//! 刻意保持「薄」：核心只有 `role` / `content` / `images`。tool 相关字段
//! （`tool_call_id` / `tool_name` / `tool_calls`）是 2026-10-30 加的，
//! 为了让「工具结果」能按各家原生协议回灌（OpenAI 要 `role:"tool"` +
//! `tool_call_id`，Anthropic 要 `tool_result` 块，Gemini 要 `functionResponse`）。
//! 加之前 terminal 只能把工具结果包成 user 消息降级回灌 —— 见 terminal 的
//! `ai_tools.rs`。字段全部默认空，不带工具的调用方完全无感。

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// 角色。用枚举而不是字符串，是为了让「写错 role 拼写」变成编译错误。
/// 旧实现（file/pet）都写的是字符串，拼错 `"asistant"` 只会在服务端 400。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    /// 工具结果。**不是**「模型说的」也不是「用户说的」——
    /// 各家协议里它是独立角色（OpenAI/Ollama）或特定 content 块（Anthropic/Gemini），
    /// 编码细节在 `wire` 层。
    Tool,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        }
    }
}

/// 一条消息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
    /// base64（不带 data: 前缀）或完整 data URL。仅对支持视觉的模型有意义。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<String>,
    /// 工具结果关联的调用 id（仅 `Role::Tool` 用）。OpenAI/Anthropic 用它配对。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// 工具名（仅 `Role::Tool` 用）。Gemini 的 `functionResponse` 与
    /// Ollama 的 tool 消息按**名字**配对，没有 id 字段 —— 所以两个都要带。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    /// assistant 这一轮请求调用的工具（恒为 OpenAI 形状，同
    /// [`ChatReply::tool_calls`]）。**回灌时必须原样带回去** ——
    /// OpenAI 会校验「tool 结果前面必须有一条带对应 tool_calls 的 assistant 消息」，
    /// 缺了直接 400。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<serde_json::Value>,
}

/// 工具声明（协议无关形状）。
///
/// `parameters` 是 JSON Schema **对象**（`{"type":"object","properties":{…}}`），
/// 由 `wire` 层负责翻译成各家的声明格式（OpenAI 原样、Anthropic 的
/// `input_schema`、Gemini 的 `functionDeclarations`）。消费方写一份定义，
/// 四家都能用 —— pet 原来为 claude/gemini 各写一套双向转换，就是缺这层。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolDef {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub parameters: serde_json::Value,
}

impl ToolDef {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: serde_json::Value,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
        }
    }
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: content.into(),
            images: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: Vec::new(),
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
            images: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: Vec::new(),
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            images: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: Vec::new(),
        }
    }

    /// 一条工具结果消息。`tool_call_id` 是模型给的调用 id，`tool_name` 是工具名。
    ///
    /// 两个 id 都要带：OpenAI/Anthropic 按 `tool_call_id` 配对，
    /// Gemini/Ollama 按 `tool_name` 配对 —— 只带一个会在某家协议上配不上对。
    pub fn tool(
        tool_call_id: impl Into<String>,
        tool_name: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        Self {
            role: Role::Tool,
            content: content.into(),
            images: Vec::new(),
            tool_call_id: Some(tool_call_id.into()),
            tool_name: Some(tool_name.into()),
            tool_calls: Vec::new(),
        }
    }

    /// 一条带工具调用的 assistant 消息（回灌历史时用）。
    ///
    /// `tool_calls` 必须是**原样**的 OpenAI 形状（见 [`ChatReply::tool_calls`]），
    /// 不能改写 —— 服务端会按 id 校验后续 tool 结果。
    pub fn assistant_with_tools(
        content: impl Into<String>,
        tool_calls: Vec<serde_json::Value>,
    ) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            images: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            tool_calls,
        }
    }

    pub fn with_images(mut self, images: Vec<String>) -> Self {
        self.images = images;
        self
    }
}

/// token 用量。各家字段名不同，这里归一；拿不到就是 `None`，
/// **不要填 0** —— 「未知」和「零消耗」是两件事，混在一起就无法做成本统计。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Usage {
    pub prompt_tokens: Option<u32>,
    pub completion_tokens: Option<u32>,
    pub total_tokens: Option<u32>,
}

/// 一次对话的结果。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChatReply {
    pub content: String,
    /// 模型要调的工具，**恒为 OpenAI 形状**（`{"id","type":"function",
    /// "function":{"name","arguments"}}`，arguments 是 JSON 字符串）——
    /// Anthropic 的 tool_use 与 Gemini 的 functionCall 在这里归一，
    /// 消费方只处理一种形状。空数组 = 模型没有要求调工具。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<serde_json::Value>,
    pub usage: Option<Usage>,
    /// 实际耗时（含网络）。旧实现里有几处自己算 elapsed，现在统一在这里给。
    #[serde(skip)]
    pub latency: Duration,
    /// 服务端回报的模型名（可能与请求的不完全一致，如带日期后缀）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// 流式增量。
#[derive(Debug, Clone, PartialEq)]
pub enum Delta {
    /// 一段文本增量
    Text(String),
    /// 服务端明确表示结束（OpenAI 的 `[DONE]`、Ollama 的 `done:true`）
    Done,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_serializes_lowercase() {
        assert_eq!(serde_json::to_string(&Role::System).unwrap(), "\"system\"");
        assert_eq!(serde_json::to_string(&Role::User).unwrap(), "\"user\"");
    }

    #[test]
    fn empty_images_are_not_serialized() {
        let m = Message::user("hi");
        let s = serde_json::to_string(&m).unwrap();
        assert!(!s.contains("images"), "空 images 不该出现在线上报文里: {s}");
    }

    #[test]
    fn images_round_trip() {
        let m = Message::user("看图").with_images(vec!["AAA".into()]);
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains("\"images\":[\"AAA\"]"));
    }

    #[test]
    fn usage_unknown_is_none_not_zero() {
        let u = Usage::default();
        assert_eq!(u.total_tokens, None);
    }

    #[test]
    fn tool_message_carries_both_ids() {
        let m = Message::tool("call_1", "get_weather", "晴");
        assert_eq!(m.role, Role::Tool);
        assert_eq!(m.tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(m.tool_name.as_deref(), Some("get_weather"));
        assert_eq!(m.content, "晴");
    }

    #[test]
    fn tool_fields_absent_on_plain_messages() {
        // 不带工具的调用方完全无感：新字段不出现在线上报文里
        let m = Message::user("hi");
        let s = serde_json::to_string(&m).unwrap();
        assert!(!s.contains("tool_call_id"), "{s}");
        assert!(!s.contains("tool_name"), "{s}");
        assert!(!s.contains("tool_calls"), "{s}");
    }

    #[test]
    fn assistant_with_tools_keeps_calls_verbatim() {
        let calls = vec![serde_json::json!({
            "id": "call_1", "type": "function",
            "function": {"name": "get_weather", "arguments": "{\"city\":\"上海\"}"}
        })];
        let m = Message::assistant_with_tools("我来查一下", calls.clone());
        assert_eq!(m.role, Role::Assistant);
        assert_eq!(m.tool_calls, calls);
    }

    #[test]
    fn tool_result_round_trips_through_serde() {
        let m = Message::tool("call_1", "get_weather", "晴");
        let s = serde_json::to_string(&m).unwrap();
        let back: Message = serde_json::from_str(&s).unwrap();
        assert_eq!(back.role, Role::Tool);
        assert_eq!(back.tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(back.tool_name.as_deref(), Some("get_weather"));
    }
}
