//! provider 模型与 URL 拼接。
//!
//! ## 为什么需要这个文件
//!
//! 十套实现里，`base_url` 的语义**没有一处是一样的**：
//!
//! | 仓 | 用户填什么 | 代码怎么拼 |
//! |---|---|---|
//! | file | `https://api.anthropic.com` | `{base}/v1/messages` |
//! | shared(clients.ts) | `https://api.anthropic.com/v1` | `{base}/messages` |
//! | pet | `https://api.openai.com` | `{base}/v1/chat/completions` |
//! | mind | `https://api.xx.com/v1` | `{base}/chat/completions` |
//!
//! 同一个字段，两种含义。用户把 A 产品的配置抄到 B 产品里就会 404，
//! 而报错只会说「404」，没人能看出是少写或多写了一段 `/v1`。
//!
//! 本文件定了唯一语义：**`base_url` 可以带版本段，也可以不带；拼接时补齐**。
//! 目的是让上面四种历史写法**全都继续能用** —— 统一层不能要求用户去改配置，
//! 否则「归并」就变成了「让每个用户重填一遍 key 和地址」。
//!
//! ## 拼接规则
//!
//! 先对 base 做归一（去尾斜杠、去掉误填的完整 endpoint），再按协议补版本段：
//!
//! ```text
//! OpenAiCompat:  {base}[/v1] + /chat/completions
//! Anthropic:     {base}[/v1] + /messages
//! Gemini:        {base}[/v1beta] + /models/{model}:generateContent
//! Ollama 原生:   {base 去掉 /v1} + /api/chat
//! ```

use std::time::Duration;

use crate::error::{truncate_chars, LlmError};

/// 协议族。**同一个协议族的请求体、鉴权头、响应结构完全一致**，
/// 差别只在默认地址 —— 这是「归并」能成立的前提。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    /// OpenAI `/chat/completions`，Bearer 鉴权。DeepSeek / 通义 / MiniMax /
    /// Moonshot / 智谱 / 各类自建代理都属于这一族。
    OpenAiCompat,
    /// Anthropic `/messages`，`x-api-key` + `anthropic-version`。
    Anthropic,
    /// Google `:generateContent`，key 走 query。
    Gemini,
    /// Ollama 原生 `/api/chat`（不是 OpenAI 兼容端点）。
    Ollama,
}

/// provider 类别。故意做得比 `Protocol` 细：`Protocol` 决定「怎么发」，
/// `ProviderKind` 决定「默认地址是什么」，也决定 UI 上给用户看什么名字。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderKind {
    OpenAi,
    Anthropic,
    Gemini,
    Ollama,
    /// 任意 OpenAI 兼容服务（自建代理 / DeepSeek / 通义 / Moonshot / 智谱 …）。
    /// 必须显式给 `base_url` —— 见 [`ProviderKind::default_base_url`]。
    Custom,
    /// MiniMax 开放平台。协议与 OpenAI 完全一致，单列一项只为让
    /// 「配置写 minimax 就真的打 minimax」在代码里看得见（这是 shared 仓
    /// 当初拆分 provider 分支的理由，照抄过来）。
    MiniMax,
}

impl ProviderKind {
    /// 从各仓历史配置里的字符串解析。**同时接受十套实现用过的所有写法**，
    /// 否则迁移等于要求用户改配置文件。
    pub fn parse(raw: &str) -> Result<Self, LlmError> {
        let key = raw.trim().to_ascii_lowercase();
        match key.as_str() {
            // OpenAI 及同族别名
            "openai" | "gpt" | "azure" | "azure-openai" => Ok(Self::OpenAi),
            "anthropic" | "claude" => Ok(Self::Anthropic),
            "gemini" | "google" | "google-gemini" => Ok(Self::Gemini),
            "ollama" => Ok(Self::Ollama),
            "minimax" | "minimaxi" => Ok(Self::MiniMax),
            // custom 是 file/terminal/pet 里都出现过的兜底值
            "custom" | "openai-compatible" | "openai_compatible" | "local"
            | "deepseek" | "qwen" | "dashscope" | "moonshot" | "zhipu" | "bigmodel"
            | "siliconflow" | "volces" | "ark" => Ok(Self::Custom),
            "" => Err(LlmError::not_configured(
                "未选择 AI 提供商（provider 为空）",
            )),
            other => Err(LlmError::new(
                crate::error::ErrorKind::Unsupported,
                format!(
                    "未知的 AI 提供商：{}。支持 openai / anthropic / gemini / ollama / minimax / custom",
                    truncate_chars(other, 40)
                ),
            )),
        }
    }

    pub fn protocol(self) -> Protocol {
        match self {
            Self::OpenAi | Self::MiniMax | Self::Custom => Protocol::OpenAiCompat,
            Self::Anthropic => Protocol::Anthropic,
            Self::Gemini => Protocol::Gemini,
            Self::Ollama => Protocol::Ollama,
        }
    }

    /// 默认 baseUrl。`Custom` 返回空串 —— **刻意不猜**。
    ///
    /// shared 仓在 `LocalClient` 上写过这个判断，理由同样适用：猜错的默认地址
    /// 会让请求打到一个不存在的端口，报出来的「连接被拒绝」比「你没填地址」
    /// 难懂得多。把不确定性摆在明面上。
    pub fn default_base_url(self) -> &'static str {
        match self {
            Self::OpenAi => "https://api.openai.com/v1",
            Self::Anthropic => "https://api.anthropic.com",
            Self::Gemini => "https://generativelanguage.googleapis.com/v1beta",
            Self::Ollama => "http://localhost:11434",
            Self::MiniMax => "https://api.minimaxi.com/v1",
            Self::Custom => "",
        }
    }

    /// 是否需要 apiKey。只有 Ollama 本地服务可以不带。
    pub fn requires_api_key(self) -> bool {
        !matches!(self, Self::Ollama)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::OpenAi => "OpenAI",
            Self::Anthropic => "Anthropic",
            Self::Gemini => "Google Gemini",
            Self::Ollama => "Ollama（本地）",
            Self::MiniMax => "MiniMax",
            Self::Custom => "自定义（OpenAI 兼容）",
        }
    }
}

/// 统一配置。字段刻意少 —— 每多一个字段，十套实现各自的历史包袱就多一条迁移成本。
#[derive(Debug, Clone)]
pub struct LlmConfig {
    pub kind: ProviderKind,
    /// 留空则用 [`ProviderKind::default_base_url`]；`Custom` 留空直接报错。
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub timeout: Duration,
    /// `None` 表示不带该字段（让服务端用默认值），而不是发一个 0.7。
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            kind: ProviderKind::OpenAi,
            base_url: String::new(),
            api_key: String::new(),
            model: String::new(),
            timeout: Duration::from_secs(60),
            temperature: None,
            max_tokens: None,
        }
    }
}

/// 请求动作 —— 决定末端路径。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// 发一次对话
    Chat,
    /// 列模型
    Models,
}

impl LlmConfig {
    /// 解析出实际使用的 baseUrl。
    fn resolved_base(&self) -> Result<String, LlmError> {
        let raw = if self.base_url.trim().is_empty() {
            self.kind.default_base_url()
        } else {
            self.base_url.trim()
        };
        if raw.is_empty() {
            return Err(LlmError::not_configured(format!(
                "{} 需要显式填写 Base URL（本层不猜测服务地址）",
                self.kind.label()
            )));
        }
        Ok(strip_trailing_slash(raw))
    }

    /// 校验配置是否足以发请求。**在发请求之前调用**，这样「没配 key」
    /// 得到的是一句人话，而不是一个 401。
    pub fn validate(&self) -> Result<(), LlmError> {
        if self.model.trim().is_empty() {
            return Err(LlmError::not_configured("未填写模型名（model 为空）"));
        }
        if self.kind.requires_api_key() && self.api_key.trim().is_empty() {
            return Err(LlmError::not_configured(format!(
                "{} 需要 API Key，请先到「设置 → AI」填写",
                self.kind.label()
            )));
        }
        // Custom 必须给地址；其余有默认值，resolved_base 会兜住
        self.resolved_base()?;
        Ok(())
    }

    /// 拼出最终请求地址。
    ///
    /// 归一化容忍历史写法的三件事：
    /// - 尾部斜杠
    /// - 用户把**完整 endpoint** 填进了 baseUrl（`…/chat/completions`）
    /// - 版本段有或没有（`…/v1` vs 根域名）
    pub fn endpoint(&self, action: Action) -> Result<String, LlmError> {
        let mut base = self.resolved_base()?;

        // ① 用户误把完整 endpoint 填进 baseUrl：剥掉，否则会拼成
        //    `…/chat/completions/v1/chat/completions`
        for tail in [
            "/chat/completions",
            "/messages",
            "/api/chat",
            "/completions",
        ] {
            if let Some(stripped) = base.strip_suffix(tail) {
                base = stripped.to_string();
                break;
            }
        }
        // Gemini 的 `:generateContent` 也可能被整段粘进来
        if let Some(idx) = base.find("/models/") {
            if base[idx..].contains(":generateContent") {
                base = base[..idx].to_string();
            }
        }

        // ② Ollama 原生与 OpenAI 兼容的分岔：
        //    用户给 Ollama 填了 `…/v1` ⇒ 他想要的是 OpenAI 兼容端点。
        //    这不是"猜"，是把域里唯一自洽的读法落地（`/api/chat` 前面永远不会有 `/v1`）。
        let protocol = if self.kind == ProviderKind::Ollama && base.ends_with("/v1") {
            Protocol::OpenAiCompat
        } else {
            self.kind.protocol()
        };

        match protocol {
            Protocol::OpenAiCompat => {
                let root = ensure_version(&base, "/v1");
                Ok(match action {
                    Action::Chat => format!("{root}/chat/completions"),
                    Action::Models => format!("{root}/models"),
                })
            }
            Protocol::Anthropic => {
                let root = ensure_version(&base, "/v1");
                Ok(match action {
                    Action::Chat => format!("{root}/messages"),
                    // Anthropic 的 /models 自 2025 起可用；不可用时调用方会拿到
                    // 404，本层不为此单独做降级分支（避免隐藏真实的鉴权问题）
                    Action::Models => format!("{root}/models"),
                })
            }
            Protocol::Gemini => {
                let root = ensure_version(&base, "/v1beta");
                Ok(match action {
                    Action::Chat => format!(
                        "{root}/models/{}:generateContent",
                        encode_path(&self.model)
                    ),
                    Action::Models => format!("{root}/models"),
                })
            }
            Protocol::Ollama => Ok(match action {
                Action::Chat => format!("{base}/api/chat"),
                Action::Models => format!("{base}/api/tags"),
            }),
        }
    }

    /// 流式端点：OpenAI 兼容与 Anthropic 共用同一路径，只有 body 里 `stream: true`。
    /// Gemini 用 `:streamGenerateContent?alt=sse`。Ollama 同路径。
    pub fn stream_endpoint(&self) -> Result<String, LlmError> {
        let base = self.endpoint(Action::Chat)?;
        Ok(match self.kind.protocol() {
            Protocol::Gemini => base.replace(":generateContent", ":streamGenerateContent")
                + "?alt=sse",
            _ => base,
        })
    }
}

fn strip_trailing_slash(s: &str) -> String {
    s.trim_end_matches('/').to_string()
}

/// 补版本段。已经以该段结尾就原样返回；否则补上。
///
/// `allow_root` 的判断刻意保守：只认 `/v1`、`/v1beta`、`/v2`、`/openai` 这类
/// 明确版本尾巴。自建代理常见的 `https://gw.corp/llm` 这种不含版本段的路径，
/// 补 `/v1` 是业界默认（vLLM / LM Studio / one-api 都挂在 /v1 下）。
fn ensure_version(base: &str, default_version: &str) -> String {
    if ends_with_version(base) {
        return base.to_string();
    }
    format!("{}{}", base, default_version)
}

fn ends_with_version(base: &str) -> bool {
    let last = base.rsplit('/').next().unwrap_or("");
    // v1 / v1beta / v2 / v3 / v4 / v1alpha 等
    let mut chars = last.chars();
    if chars.next() != Some('v') {
        return false;
    }
    let rest: String = chars.collect();
    if rest.is_empty() {
        return false;
    }
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return false;
    }
    let tail = &rest[digits.len()..];
    tail.is_empty() || tail.chars().all(|c| c.is_ascii_alphabetic())
}

/// Gemini 的模型名要放进路径，做最小编码（只编码会破坏路径的字符）。
fn encode_path(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            ' ' => "%20".to_string(),
            '#' => "%23".to_string(),
            '?' => "%3F".to_string(),
            _ => c.to_string(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(kind: ProviderKind, base: &str) -> LlmConfig {
        LlmConfig {
            kind,
            base_url: base.to_string(),
            api_key: "sk-test-1234567890".into(),
            model: "m".into(),
            ..Default::default()
        }
    }

    #[test]
    fn parse_accepts_all_legacy_names() {
        // 十套实现用过的写法，一个都不能漏
        for (raw, want) in [
            ("openai", ProviderKind::OpenAi),
            ("claude", ProviderKind::Anthropic),
            ("anthropic", ProviderKind::Anthropic),
            ("gemini", ProviderKind::Gemini),
            ("ollama", ProviderKind::Ollama),
            ("minimax", ProviderKind::MiniMax),
            ("custom", ProviderKind::Custom),
            ("deepseek", ProviderKind::Custom),
            ("qwen", ProviderKind::Custom),
            ("local", ProviderKind::Custom),
            ("OpenAI", ProviderKind::OpenAi),
        ] {
            assert_eq!(ProviderKind::parse(raw).unwrap(), want, "raw={raw}");
        }
    }

    #[test]
    fn parse_rejects_unknown_without_silent_fallback() {
        // 静默回落成 OpenAI 正是 shared 仓治理过的病：配置写 A、实际打 B
        let err = ProviderKind::parse("some-new-vendor").unwrap_err();
        assert_eq!(err.kind, crate::error::ErrorKind::Unsupported);
        assert!(err.message.contains("some-new-vendor"));
    }

    #[test]
    fn openai_endpoint_adds_version_when_absent() {
        // pet 的写法：base 不带 /v1
        let c = cfg(ProviderKind::OpenAi, "https://api.openai.com");
        assert_eq!(
            c.endpoint(Action::Chat).unwrap(),
            "https://api.openai.com/v1/chat/completions"
        );
    }

    #[test]
    fn openai_endpoint_keeps_existing_version() {
        // mind / shared 的写法：base 已带 /v1
        let c = cfg(ProviderKind::OpenAi, "https://api.openai.com/v1/");
        assert_eq!(
            c.endpoint(Action::Chat).unwrap(),
            "https://api.openai.com/v1/chat/completions"
        );
    }

    #[test]
    fn anthropic_endpoint_both_ways() {
        // file 的写法：base 不含 /v1
        assert_eq!(
            cfg(ProviderKind::Anthropic, "https://api.anthropic.com")
                .endpoint(Action::Chat)
                .unwrap(),
            "https://api.anthropic.com/v1/messages"
        );
        // shared 的写法：base 含 /v1
        assert_eq!(
            cfg(ProviderKind::Anthropic, "https://api.anthropic.com/v1")
                .endpoint(Action::Chat)
                .unwrap(),
            "https://api.anthropic.com/v1/messages"
        );
    }

    #[test]
    fn strips_pasted_full_endpoint() {
        // 用户把整个 endpoint 粘进 Base URL 是真实高频误操作
        let c = cfg(
            ProviderKind::OpenAi,
            "https://api.deepseek.com/v1/chat/completions",
        );
        assert_eq!(
            c.endpoint(Action::Chat).unwrap(),
            "https://api.deepseek.com/v1/chat/completions"
        );
    }

    #[test]
    fn ollama_native_by_default() {
        let c = cfg(ProviderKind::Ollama, "");
        assert_eq!(
            c.endpoint(Action::Chat).unwrap(),
            "http://localhost:11434/api/chat"
        );
        assert_eq!(
            c.endpoint(Action::Models).unwrap(),
            "http://localhost:11434/api/tags"
        );
    }

    #[test]
    fn ollama_with_v1_means_openai_compat() {
        // 填了 /v1 就是想要兼容端点 —— /api/chat 前面永远不会有 /v1
        let c = cfg(ProviderKind::Ollama, "http://localhost:11434/v1");
        assert_eq!(
            c.endpoint(Action::Chat).unwrap(),
            "http://localhost:11434/v1/chat/completions"
        );
    }

    #[test]
    fn gemini_endpoint_puts_model_in_path() {
        let c = cfg(ProviderKind::Gemini, "");
        assert_eq!(
            c.endpoint(Action::Chat).unwrap(),
            "https://generativelanguage.googleapis.com/v1beta/models/m:generateContent"
        );
        assert!(c.stream_endpoint().unwrap().contains(":streamGenerateContent"));
        assert!(c.stream_endpoint().unwrap().ends_with("?alt=sse"));
    }

    #[test]
    fn custom_without_base_is_not_configured() {
        let c = cfg(ProviderKind::Custom, "");
        let err = c.validate().unwrap_err();
        assert_eq!(err.kind, crate::error::ErrorKind::NotConfigured);
        assert!(err.message.contains("Base URL"));
    }

    #[test]
    fn missing_key_is_not_configured_before_http() {
        let mut c = cfg(ProviderKind::OpenAi, "");
        c.api_key.clear();
        let err = c.validate().unwrap_err();
        assert_eq!(err.kind, crate::error::ErrorKind::NotConfigured);
    }

    #[test]
    fn ollama_does_not_require_key() {
        let mut c = cfg(ProviderKind::Ollama, "");
        c.api_key.clear();
        assert!(c.validate().is_ok());
    }

    #[test]
    fn version_detection_is_conservative() {
        assert!(ends_with_version("https://a.com/v1"));
        assert!(ends_with_version("https://a.com/v1beta"));
        assert!(ends_with_version("https://a.com/v2"));
        assert!(!ends_with_version("https://a.com"));
        assert!(!ends_with_version("https://a.com/llm"));
        // 域名里的 v1 不算版本段
        assert!(!ends_with_version("https://v1.example.com"));
    }
}
