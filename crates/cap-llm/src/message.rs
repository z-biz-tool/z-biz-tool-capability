//! 消息与结果类型。
//!
//! 刻意保持「薄」：只有 `role` / `content` / `images`。十套实现里没有一套
//! 用到了比这更多的东西（tool 调用是请求级参数，不是消息级），
//! 所以消息结构不加 `name`、`tool_call_id` 这些 OpenAI 协议细节 ——
//! 需要时由 `wire` 层在内部补，接口面保持最小。

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
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
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
    pub fn new(name: impl Into<String>, description: impl Into<String>, parameters: serde_json::Value) -> Self {
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
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
            images: Vec::new(),
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            images: Vec::new(),
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
        assert_eq!(
            serde_json::to_string(&Role::System).unwrap(),
            "\"system\""
        );
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
}
