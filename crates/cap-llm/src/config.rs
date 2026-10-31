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

use crate::error::{truncate_chars, ErrorKind, LlmError};

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
            "custom" | "openai-compatible" | "openai_compatible" | "local" | "deepseek"
            | "qwen" | "dashscope" | "moonshot" | "zhipu" | "bigmodel" | "siliconflow"
            | "volces" | "ark" => Ok(Self::Custom),
            "" => Err(LlmError::not_configured(
                "未选择 AI 提供商（provider 为空）",
            )),
            other => {
                // 老表里没有的名字，去预设目录问一次。
                //
                // 这样 "kimi" / "glm" / "doubao" 这类**别名**也能用 ——
                // 它们是用户会输入的词（尤其在中文界面里选了「Kimi（月之暗面）」之后），
                // 却不在十套历史实现留下的老表里。走预设目录补齐，
                // 且协议族由预设自己给出，不会猜。
                //
                // ⚠️ 但要挡住 "local"：它在老表里是 Custom（已命中上面的分支），
                // 在预设目录里却是 Ollama。老表优先，所以不会走到这里 ———
                // 这一层只处理老表**完全不认识**的名字。
                if let Some(p) = crate::presets::find_by_name(other) {
                    return Ok(p.kind);
                }
                Err(LlmError::new(
                    crate::error::ErrorKind::Unsupported,
                    format!(
                    "未知的 AI 提供商：{}。支持 openai / anthropic / gemini / ollama / minimax / custom",
                    truncate_chars(other, 40)
                ),
                ))
            }
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

    /// 解析 provider 字符串，**同时解析出预设身份**。
    ///
    /// 这是 [`ProviderKind::parse`] 的超集，推荐新代码用它：
    ///
    /// ```text
    /// "deepseek"  → (Custom, Some("deepseek"))
    /// "openai"    → (OpenAi, Some("openai"))
    /// "custom"    → (Custom, None)     ← 通用兜底，没有预设
    /// ```
    ///
    /// 为什么要多这一步：用户配置里写 `"deepseek"`，`ProviderKind::parse`
    /// 只能告诉他「你是 OpenAI 兼容族」，但**地址**还得自己去查。
    /// 带上预设 id 之后，`base_url` 留空也能自动填对地址。
    ///
    /// 别名优先走预设目录（`claude` → `anthropic` 预设），
    /// 目录里没有的再回落到 [`ProviderKind::parse`] 的老逻辑。
    pub fn parse_with_preset(raw: &str) -> Result<(Self, Option<&'static str>), LlmError> {
        let key = raw.trim().to_ascii_lowercase();
        if key.is_empty() {
            return Err(LlmError::not_configured(
                "未选择 AI 提供商（provider 为空）",
            ));
        }
        // ⚠️ 顺序很重要：**先**用老逻辑定协议族，**再**看预设。
        //
        // 原因：「local」在两处含义不同 ——
        //   - `parse` 的历史表里，"local" = 自定义 OpenAI 兼容地址（file/terminal 存量配置）
        //   - presets 的别名里，"local" = Ollama
        // 若先查 presets，存量配置里的 "local" 会被悄悄改成 Ollama，
        // 请求从用户自己的自建代理改打到 localhost:11434 —— 静默改路由，
        // 比报错更难查。所以协议族以 `parse` 为准，预设只做补充。
        let kind = Self::parse(raw)?;
        // 只有当预设的协议族与解析结果一致时才采纳预设，
        // 避免 "local" 这种歧义别名被错误绑定。
        let preset = crate::presets::find_by_name(&key).filter(|p| p.kind == kind);
        Ok((kind, preset.map(|p| p.id)))
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
    /// 供应商预设 id（见 [`crate::presets`]），可选。
    ///
    /// 存在时的作用只有一个：当 `base_url` 为空时，
    /// 用预设里的服务地址兜底（而不是报「请填写 Base URL」）。
    ///
    /// 为什么是可选字段而不是新增 [`ProviderKind`] 变体：消费方
    /// （note / terminal / mind / file）各自维护了一份 provider 映射，
    /// 新增枚举变体会让它们全部要改。而协议上 deepseek / qwen / moonshot
    /// 与 OpenAI 兼容族毫无差别 —— 报文、鉴权、流式格式完全一致，
    /// 差别只在**地址**，而地址正是预设提供的东西。
    ///
    /// 留空 = 老行为（地址必须自己填），存量配置零影响。
    pub preset: Option<&'static str>,
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
            preset: None,
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
    /// 语音转写（ASR）。
    ///
    /// **只有 OpenAI 兼容系有 `/audio/transcriptions`**。Anthropic / Gemini /
    /// Ollama 都没有这个端点 —— [`Self::endpoint`] 会返回 `ErrorKind::Unsupported`
    /// 并写清原因，而不是拼一个必然 404 的地址发出去。
    ///
    /// 为什么归到这一层：会议纪要（cap-meeting）要转写音频，而「multipart 上传 +
    /// 状态码映射 + 密钥脱敏」是传输细节，与 chat 同源；归进来后 note / mind /
    /// 将来的会议纪要壳都不必各自再写一遍上传逻辑。
    Transcription,
}

impl LlmConfig {
    /// 解析出实际使用的 baseUrl。
    ///
    /// 三级回落：
    /// 1. `base_url` 有值 → 用它（用户显式填的，永远优先）
    /// 2. `base_url` 为空 + 有预设 → 用预设里的地址
    /// 3. `base_url` 为空 + 无预设 → 用 [`ProviderKind::default_base_url`]；
    ///    该值为空（`Custom`）则报错
    fn resolved_base(&self) -> Result<String, LlmError> {
        let raw = if self.base_url.trim().is_empty() {
            // ② 预设兜底：选了「DeepSeek」就自动填好它的地址，
            //    不必让用户手抄一遍 —— 抄错一个字符就是 404。
            self.preset_base_url()
                .unwrap_or_else(|| self.kind.default_base_url())
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

    /// 预设里登记的服务地址。没有预设、或预设 id 查不到时返回 `None`。
    fn preset_base_url(&self) -> Option<&'static str> {
        let id = self.preset?;
        crate::presets::find(id).map(|p| p.base_url)
    }

    /// 这份配置是否**必须**带 ApiKey。
    ///
    /// 判定顺序：**预设说了算；没有预设（或预设 id 查不到）才退回
    /// [`ProviderKind::requires_api_key`] 的默认规则**。
    ///
    /// 为什么不能只看 `kind`：`Custom` 同时覆盖两类服务 ——
    /// 「云厂商的 OpenAI 兼容端点」（必须有 key）和「本机自建服务」
    /// （回环流量，服务端根本不校验 key）。仅凭 `kind` 无法区分，
    /// 而 `ProviderPreset::requires_api_key` 是唯一知道答案的地方。
    ///
    /// 2026-10-31 修：此前只按 `kind` 判，于是预设表上明明写着「免 Key」，
    /// 运行时却弹「请先到设置里填写 API Key」—— 用户对着自己刚选的
    /// 「本地网关」预设，被要求编一个假密钥才能用。预设的字段不能只是装饰。
    pub fn needs_api_key(&self) -> bool {
        if let Some(id) = self.preset {
            if let Some(p) = crate::presets::find(id) {
                return p.requires_api_key;
            }
        }
        if !self.kind.requires_api_key() {
            return false;
        }
        // 第二道判据：**地址落在本机或局域网 → 视为无鉴权**。
        //
        // 预设表覆盖不到「用户直接粘了一个地址」的场景：把 mini 的
        // `http://192.168.31.249:8200/v1` 填进来时，按 URL 认预设必然认不出来，
        // 于是退回 `Custom`（恒要 key），用户又得编假密钥。可这类地址上的
        // llama.cpp / Ollama / LM Studio / vLLM 本来就没有鉴权层。
        //
        // 判决仍在服务端：真需要 key 的内网代理会回 401，比本地拦住更准确。
        match self.resolved_base() {
            Ok(base) => !is_private_endpoint(&base),
            // 地址都没有 —— 交给 resolved_base 在 validate 里报「要填地址」
            Err(_) => true,
        }
    }

    /// 校验配置是否足以发请求。**在发请求之前调用**，这样「没配 key」
    /// 得到的是一句人话，而不是一个 401。
    pub fn validate(&self) -> Result<(), LlmError> {
        if self.model.trim().is_empty() {
            return Err(LlmError::not_configured("未填写模型名（model 为空）"));
        }
        if self.needs_api_key() && self.api_key.trim().is_empty() {
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
            "/audio/transcriptions",
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
                    Action::Transcription => format!("{root}/audio/transcriptions"),
                })
            }
            Protocol::Anthropic => {
                let root = ensure_version(&base, "/v1");
                Ok(match action {
                    Action::Chat => format!("{root}/messages"),
                    // Anthropic 的 /models 自 2025 起可用；不可用时调用方会拿到
                    // 404，本层不为此单独做降级分支（避免隐藏真实的鉴权问题）
                    Action::Models => format!("{root}/models"),
                    // Claude 系列没有音频转写端点，不猜一个发出去。
                    Action::Transcription => return Err(unsupported_asr(self.kind)),
                })
            }
            Protocol::Gemini => {
                let root = ensure_version(&base, "/v1beta");
                Ok(match action {
                    Action::Chat => {
                        format!("{root}/models/{}:generateContent", encode_path(&self.model))
                    }
                    Action::Models => format!("{root}/models"),
                    // Gemini 的音频理解走 generateContent 的 inline_data，不是
                    // OpenAI 那套 multipart 转写接口；这里不做第二套报文构造。
                    Action::Transcription => return Err(unsupported_asr(self.kind)),
                })
            }
            Protocol::Ollama => Ok(match action {
                Action::Chat => format!("{base}/api/chat"),
                Action::Models => format!("{base}/api/tags"),
                // Ollama 本地推理没有 /audio/transcriptions。
                Action::Transcription => return Err(unsupported_asr(self.kind)),
            }),
        }
    }

    /// 流式端点：OpenAI 兼容与 Anthropic 共用同一路径，只有 body 里 `stream: true`。
    /// Gemini 用 `:streamGenerateContent?alt=sse`。Ollama 同路径。
    pub fn stream_endpoint(&self) -> Result<String, LlmError> {
        let base = self.endpoint(Action::Chat)?;
        Ok(match self.kind.protocol() {
            Protocol::Gemini => {
                base.replace(":generateContent", ":streamGenerateContent") + "?alt=sse"
            }
            _ => base,
        })
    }
}

fn strip_trailing_slash(s: &str) -> String {
    s.trim_end_matches('/').to_string()
}

/// 该地址是否指向「本机 / 局域网」。
///
/// 用于判断「这份配置有没有可能是个不需要鉴权的自建服务」。只看 host，
/// **不看端口**：llama.cpp 8200、Ollama 11434、LM Studio 1234、vLLM 8000，
/// 按端口列举必然过时，而 host 属于私有段的判断依据是 IPv4/IPv6 保留地址
/// （RFC 1918 / RFC 4193），不会随时间失效。
///
/// 认得三类：
/// - 回环：`localhost` / `127.0.0.0/8` / `::1` / `0.0.0.0`
/// - 私有段：`10/8`、`172.16/12`、`192.168/16`
/// - 本地域名：`.local`（mDNS）、`.lan`、`.internal`、`host.docker.internal`
///
/// 不判「云厂商域名」—— 那由预设表负责，两套规则各管一段，不重叠。
pub fn is_private_endpoint(url: &str) -> bool {
    let host = host_of(url);
    if host.is_empty() {
        return false;
    }
    let h = host
        .trim_matches(|c| c == '[' || c == ']')
        .to_ascii_lowercase();
    // 名称类：mDNS / 局域网后缀 / 容器回流名。这些后缀不可能属于公网服务，
    // 但它们解析不成 IP，必须显式列出，否则 `.local` 的 mini 会被当成公网。
    if h == "localhost"
        || h == "host.docker.internal"
        || h.ends_with(".local")
        || h.ends_with(".lan")
        || h.ends_with(".internal")
    {
        return true;
    }
    match h.parse::<std::net::IpAddr>() {
        // `is_private` / `is_link_local` 是 Ipv4Addr 的方法，IpAddr 上没有，
        // 按版本分派；IPv6 那边对应「唯一本地地址」fc00::/7。
        Ok(std::net::IpAddr::V4(v4)) => {
            v4.is_loopback() || v4.is_private() || v4.is_link_local() || v4.is_unspecified()
        }
        Ok(std::net::IpAddr::V6(v6)) => v6.is_loopback() || (v6.segments()[0] & 0xfe00) == 0xfc00,
        // 解析不出来的是域名 —— 不可能是回环/私网
        Err(_) => false,
    }
}

/// 从 URL 里取 host（去掉 scheme、userinfo、端口、路径）。
///
/// 不引 `url` crate：本 crate 只解析 baseUrl 这一种输入，且历史配置里存在
/// `192.168.1.2:8200/v1`（无 scheme）这种写法，引一个严格解析器反而要
/// 额外容错。取不到就返回空串，由调用方当作「不是私有地址」处理。
fn host_of(url: &str) -> &str {
    let rest = match url.find("://") {
        Some(i) => &url[i + 3..],
        None => url,
    };
    let rest = rest.split('/').next().unwrap_or("");
    // userinfo@host
    let rest = match rest.rfind('@') {
        Some(i) => &rest[i + 1..],
        None => rest,
    };
    // IPv6 字面量 [::1]:8200
    if rest.starts_with('[') {
        return match rest.find(']') {
            Some(i) => &rest[..=i],
            None => rest,
        };
    }
    rest.split(':').next().unwrap_or("")
}

/// 该 provider 没有 `/audio/transcriptions` 端点时的统一报错。
///
/// **不静默回落**是本 crate 的第一条纪律（见 lib.rs）：拼一个 OpenAI 的地址
/// 发给 Anthropic 只会拿到一个看不懂的 404，用户无从判断是地址错、key 错
/// 还是根本没这个能力。这里直接说清「哪家没有、该换哪家」。
///
/// 为什么只支持 OpenAI 兼容系：目前业界可用的自托管 / 聚合网关 ASR 端点
/// 几乎都是 OpenAI 形状（`multipart` + `model` 字段 + `text` 响应）。
/// Gemini 的音频理解是 generateContent 里塞 inline_data，报文构造完全不同；
/// 为了「看起来三家都支持」去拼第二套报文，反而会把错误藏在半路。
fn unsupported_asr(kind: ProviderKind) -> LlmError {
    LlmError::new(
        ErrorKind::Unsupported,
        format!(
            "{} 不提供 /audio/transcriptions 语音转写端点。目前仅 OpenAI 兼容系 \
             （OpenAI / SiliconFlow / one-api / vLLM 等）支持音频转写，请改用这些 \
             provider；或直接传入 OpenAI 兼容的 Base URL。",
            kind.label()
        ),
    )
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

    // ── 预设接线（2026-10-28）──────────────────────────────────

    #[test]
    fn parse_with_preset_finds_preset_for_known_vendors() {
        for (raw, want_kind, want_preset) in [
            ("deepseek", ProviderKind::Custom, Some("deepseek")),
            ("qwen", ProviderKind::Custom, Some("qwen")),
            ("kimi", ProviderKind::Custom, Some("moonshot")),
            ("glm", ProviderKind::Custom, Some("zhipu")),
            ("doubao", ProviderKind::Custom, Some("volcengine")),
            ("openai", ProviderKind::OpenAi, Some("openai")),
            ("claude", ProviderKind::Anthropic, Some("anthropic")),
            ("gemini", ProviderKind::Gemini, Some("gemini")),
            ("ollama", ProviderKind::Ollama, Some("ollama")),
        ] {
            let (kind, preset) = ProviderKind::parse_with_preset(raw).unwrap();
            assert_eq!(kind, want_kind, "raw={raw}");
            assert_eq!(preset, want_preset, "raw={raw}");
        }
    }

    #[test]
    fn parse_with_preset_keeps_custom_without_preset() {
        // "custom" 没有对应预设 —— 地址必须用户自己填
        let (kind, preset) = ProviderKind::parse_with_preset("custom").unwrap();
        assert_eq!(kind, ProviderKind::Custom);
        assert_eq!(preset, None);
    }

    #[test]
    fn parse_with_preset_does_not_rebind_local() {
        // ⚠️ 这条最容易出事：
        // "local" 在历史配置里 = 自定义 OpenAI 兼容地址（file/terminal 存量），
        // 在 presets 别名里 = Ollama。若预设优先，存量配置会被静默改路由到
        // localhost:11434 —— 请求打到了另一个服务，且没有任何提示。
        let (kind, preset) = ProviderKind::parse_with_preset("local").unwrap();
        assert_eq!(
            kind,
            ProviderKind::Custom,
            "local 的协议族不该被改成 Ollama"
        );
        assert_eq!(preset, None, "local 不该绑定到 ollama 预设");
    }

    #[test]
    fn preset_fills_base_url_when_empty() {
        // 这是本轮改动的主目的：选了 DeepSeek 就不必手抄地址
        let c = LlmConfig {
            kind: ProviderKind::Custom,
            base_url: String::new(),
            api_key: "sk-test-1234567890".into(),
            model: "deepseek-chat".into(),
            preset: Some("deepseek"),
            ..Default::default()
        };
        assert_eq!(
            c.endpoint(Action::Chat).unwrap(),
            "https://api.deepseek.com/v1/chat/completions"
        );
    }

    #[test]
    fn explicit_base_url_wins_over_preset() {
        // 用户显式填的地址永远优先 —— 预设只是兜底
        let c = LlmConfig {
            kind: ProviderKind::Custom,
            base_url: "https://my-proxy.corp/v1".into(),
            api_key: "k".into(),
            model: "m".into(),
            preset: Some("deepseek"),
            ..Default::default()
        };
        assert_eq!(
            c.endpoint(Action::Chat).unwrap(),
            "https://my-proxy.corp/v1/chat/completions"
        );
    }

    #[test]
    fn custom_without_preset_or_base_url_still_errors() {
        // 老行为不变：没有预设也没有地址 → 报错，不猜
        let c = cfg(ProviderKind::Custom, "");
        let err = c.validate().unwrap_err();
        assert_eq!(err.kind, crate::error::ErrorKind::NotConfigured);
    }

    #[test]
    fn unknown_preset_id_is_ignored_not_fatal() {
        // 配置里的预设 id 可能是老版本留下的、目录里已经没有的。
        // 不该因此让整个配置报废 —— 退化到「必须手填地址」即可。
        let mut c = cfg(ProviderKind::Custom, "");
        c.preset = Some("vendor-from-the-future");
        let err = c.validate().unwrap_err();
        assert_eq!(err.kind, crate::error::ErrorKind::NotConfigured);
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
        assert!(c
            .stream_endpoint()
            .unwrap()
            .contains(":streamGenerateContent"));
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
    fn preset_decides_whether_a_key_is_required() {
        // 这条钉住 2026-10-31 的修复：预设写了「免 Key」，运行时就必须真免 Key。
        // 反面教材是只按 kind 判 —— Custom 既是云厂商也是本机自建，
        // 一刀切会让本地网关用不了。
        let mut local = cfg(ProviderKind::Custom, "http://127.0.0.1:8200/v1");
        local.preset = Some("zbiz-local");
        local.api_key.clear();
        assert!(!local.needs_api_key(), "zbiz-local 不该要 key");
        assert!(
            local.validate().is_ok(),
            "免 Key 预设应当通过校验：{:?}",
            local.validate()
        );

        // 同一份 kind，换成需要 key 的云厂商预设 → 必须拦住
        let mut cloud = cfg(ProviderKind::Custom, "https://api.deepseek.com/v1");
        cloud.preset = Some("deepseek");
        cloud.api_key.clear();
        assert!(cloud.needs_api_key(), "deepseek 必须要求 key");
        assert!(cloud.validate().is_err());

        // 没有预设 → 退回 kind 的默认规则（Custom 要 key）
        let mut bare = cfg(ProviderKind::Custom, "https://example.com/v1");
        bare.api_key.clear();
        assert!(bare.needs_api_key());
        assert!(bare.validate().is_err());

        // 预设 id 拼错 → 查不到就退回默认规则，不能因为「查不到」而放行
        let mut typo = cfg(ProviderKind::Custom, "https://example.com/v1");
        typo.preset = Some("zbiz-locl");
        typo.api_key.clear();
        assert!(typo.needs_api_key(), "预设 id 查不到时必须保守地要求 key");
    }

    #[test]
    fn preset_wins_over_kind_for_key_requirement() {
        // 一个 Custom 预设免 key、一个 Ollama（kind 本身免 key）要 key 的反向
        // 情况不存在（目录里没有 requires_api_key:true 的 Ollama），
        // 所以这里验的是「预设优先于 kind」这一条方向：
        // zbiz-local 的 kind 是 Custom（默认要 key），但预设说不。
        let mut c = cfg(ProviderKind::Custom, "http://127.0.0.1:8200/v1");
        c.preset = Some("zbiz-local");
        assert!(
            ProviderKind::Custom.requires_api_key(),
            "前提：Custom 默认要 key"
        );
        assert!(!c.needs_api_key(), "预设必须覆盖 kind 的默认判断");
    }

    #[test]
    fn private_endpoint_is_recognised_without_preset() {
        // 2026-10-31 补：用户不选预设、直接把 mini 的地址粘进来时，
        // 按 URL 认不出预设 → 退回 Custom（要 key）→ 又得编假密钥。
        // 私有地址本身就该免 key，与「选没选预设」无关。
        let mut pasted = cfg(ProviderKind::Custom, "http://192.168.31.249:8200/v1");
        pasted.api_key.clear();
        assert!(!pasted.needs_api_key(), "局域网地址不该要 key");
        assert!(pasted.validate().is_ok(), "{:?}", pasted.validate());

        // 回环、无 scheme 的历史写法、mDNS 主机名
        for url in [
            "http://127.0.0.1:8200/v1",
            "http://localhost:11434/v1",
            "192.168.1.2:8200/v1",
            "http://zifangs-mac-mini.local:8200/v1",
            "http://host.docker.internal:1234/v1",
            "http://10.0.0.9/v1",
            "http://172.20.1.4/v1",
            "http://[::1]:8200/v1",
            "http://user:pw@127.0.0.1:8200/v1",
        ] {
            assert!(is_private_endpoint(url), "{url} 应判为私有地址");
        }

        // 公网地址一个都不能误判 —— 放行公网等于把「没填 key」的错误
        // 从设置页挪到了 401 响应里
        for url in [
            "https://api.openai.com/v1",
            "https://api.deepseek.com/v1",
            "http://172.32.0.1/v1",
            "http://192.169.1.1/v1",
            "http://11.0.0.1/v1",
            "https://example.local.evil.com/v1",
        ] {
            assert!(!is_private_endpoint(url), "{url} 不该判为私有地址");
        }
    }

    #[test]
    fn private_endpoint_does_not_rescue_key_requiring_preset() {
        // 反向约束：预设明说要 key（云厂商），即便地址被改成本机代理，
        // 也按预设走 —— 用户显式选的厂商优先于地址猜测。
        let mut cloud = cfg(ProviderKind::Custom, "http://127.0.0.1:8200/v1");
        cloud.preset = Some("deepseek");
        cloud.api_key.clear();
        assert!(cloud.needs_api_key());
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
