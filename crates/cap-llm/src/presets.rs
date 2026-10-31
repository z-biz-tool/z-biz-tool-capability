//! 供应商预设目录 —— 借鉴 cc-switch 的 preset 模型。
//!
//! ## 为什么需要这个文件
//!
//! [`crate::ProviderKind`] 把 `deepseek` / `qwen` / `moonshot` / `zhipu` 等
//! 一律归为 [`ProviderKind::Custom`]，而 `Custom` 的默认地址**刻意留空**
//! （不猜服务地址）。于是用户在任何产品里选「DeepSeek」，都得自己手填
//! `https://api.deepseek.com/v1` —— 抄错一个字符就是 404，而报错只说「404」。
//!
//! cc-switch 的做法是**内置 90+ 预设**：选中一家，地址、协议、默认模型
//! 一起填好，用户只补 API Key。本文件是同一思路的家族版。
//!
//! ## 一条硬规矩：预设**不写死模型名**
//!
//! 本文件只提供「哪一家 + 什么协议 + 什么地址」。**模型名不预设**。
//!
//! 原因（2026-10-28 实测）：各家模型生命周期只有数月到一年 ——
//! OpenAI 在 2026 年内已从 `gpt-4o` 走到 `gpt-5.6`，`gpt-4o` 已进入
//! 2027-01 的关停名单；Gemini 的 `gemini-2.5-pro` 在 2026-10-20 就退役。
//! 预设里写死模型名 = 半年后用户点了预设直接拿到一个已关停的模型。
//!
//! 所以模型名由调用方在**运行时**通过 [`crate::list_models`] 现取，
//! 用户自己选；确实要给推荐值时，用下面标注了「建议」的字段，由 UI
//! 明确标注「推荐，可能已过期」，而不是静默填进配置。
//!
//! ## 地址准确性
//!
//! 每条预设的 `base_url` 与 `verified_on` 一起记录。厂商会改地址
//! （MiniMax 就从 `api.minimaxi.com` 与 `api.minimax.io` 双域名），
//! 因此 `verified_on` 是**可审计字段** —— 超过半年没人复核的预设
//! 应当在 UI 上标「可能已过期」，而不是让用户对着 404 猜。
//!
//! ## 与 ProviderKind 的关系
//!
//! 预设**不新增** [`ProviderKind`] 变体。所有 OpenAI 兼容厂商共用
//! [`ProviderKind::Custom`] 协议族，靠 `base_url` 区分服务地址。
//!
//! 这样做是有意的：新增枚举变体会让消费方的 `match` 全部要改
//! （note / terminal / mind 各自维护了一份 provider 映射），
//! 而协议上它们毫无差别 —— 报文、鉴权、流式格式完全一致。

use crate::config::{LlmConfig, Protocol, ProviderKind};
use crate::error::LlmError;

/// 预设分类。UI 按这个分组展示。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    /// 海外官方厂商
    International,
    /// 国内官方厂商
    Domestic,
    /// 聚合平台 / 中转（一个 key 多家模型）
    Aggregator,
    /// 本地或自建服务
    SelfHosted,
}

impl Category {
    pub fn label(self) -> &'static str {
        match self {
            Self::International => "国际厂商",
            Self::Domestic => "国内厂商",
            Self::Aggregator => "聚合平台",
            Self::SelfHosted => "自建 / 本地",
        }
    }
}

/// 一条供应商预设。
#[derive(Debug, Clone, Copy)]
pub struct ProviderPreset {
    /// 稳定的机器标识，用作配置文件里的键（不要用会变的展示名）。
    pub id: &'static str,
    /// 展示名（中文）。
    pub label: &'static str,
    /// 协议族。决定请求体、鉴权头、流式格式。
    pub protocol: Protocol,
    /// 服务地址。**不带尾斜杠**；带不带版本段都可以（拼接时会归一）。
    pub base_url: &'static str,
    /// 供用户搜索的其他写法（英文名、旧称、域名片段）。
    pub aliases: &'static [&'static str],
    /// 分类。
    pub category: Category,
    /// 地址的核对日期（YYYY-MM-DD）。用于识别「可能已过期」。
    pub verified_on: &'static str,
    /// 是否需要 API Key。Ollama 这类本地服务不需要。
    pub requires_api_key: bool,
    /// 这个预设对应的 provider 身份。
    ///
    /// 与 [`ProviderPreset::protocol`] 是两件事：`protocol` 决定「怎么发」，
    /// `kind` 决定「这是谁」。多数 OpenAI 兼容厂商的 kind 是 `Custom`
    ///（协议上与 OpenAI 无差别，差别只在地址），
    /// 但 openai / minimax / ollama 这几个有独立身份 —— 老配置里
    /// `provider: "openai"` 应当继续解析成 `OpenAi`。
    pub kind: ProviderKind,
}

impl ProviderPreset {
    /// 展示名。
    pub fn label(&self) -> &'static str {
        self.label
    }

    /// 用这个预设造一份配置。调用方补上 `api_key` 与 `model` 即可。
    pub fn to_config(&self, api_key: impl Into<String>, model: impl Into<String>) -> LlmConfig {
        LlmConfig {
            kind: self.kind,
            base_url: self.base_url.to_string(),
            api_key: api_key.into(),
            model: model.into(),
            preset: Some(self.id),
            ..Default::default()
        }
    }
}

/// 全部预设。
///
/// 地址核对日期统一为 2026-10-28。**新增厂商请同时更新 `verified_on`** ——
/// 这个字段是这个目录的可信度锚点，没核对过就不要写进来。
pub static PRESETS: &[ProviderPreset] = &[
    // ── 国际厂商 ───────────────────────────────────────────────
    ProviderPreset {
        id: "openai",
        label: "OpenAI",
        protocol: Protocol::OpenAiCompat,
        base_url: "https://api.openai.com/v1",
        aliases: &["openai", "gpt", "chatgpt", "azure", "openai.com"],
        category: Category::International,
        verified_on: "2026-10-28",
        requires_api_key: true,
        kind: ProviderKind::OpenAi,
    },
    ProviderPreset {
        id: "anthropic",
        label: "Anthropic",
        protocol: Protocol::Anthropic,
        base_url: "https://api.anthropic.com",
        aliases: &["anthropic", "claude"],
        category: Category::International,
        verified_on: "2026-10-28",
        requires_api_key: true,
        kind: ProviderKind::Anthropic,
    },
    ProviderPreset {
        id: "gemini",
        label: "Google Gemini",
        protocol: Protocol::Gemini,
        base_url: "https://generativelanguage.googleapis.com/v1beta",
        aliases: &["gemini", "google", "google-gemini", "generativelanguage"],
        category: Category::International,
        verified_on: "2026-10-28",
        requires_api_key: true,
        kind: ProviderKind::Gemini,
    },
    ProviderPreset {
        id: "minimax",
        label: "MiniMax",
        protocol: Protocol::OpenAiCompat,
        base_url: "https://api.minimaxi.com/v1",
        aliases: &["minimax", "minimaxi", "minimax.io", "海螺"],
        category: Category::Domestic,
        verified_on: "2026-10-28",
        requires_api_key: true,
        kind: ProviderKind::MiniMax,
    },
    // ── 国内厂商（OpenAI 兼容族）─────────────────────────────────
    ProviderPreset {
        id: "deepseek",
        label: "DeepSeek",
        protocol: Protocol::OpenAiCompat,
        base_url: "https://api.deepseek.com/v1",
        aliases: &["deepseek", "深度求索"],
        category: Category::Domestic,
        verified_on: "2026-10-28",
        requires_api_key: true,
        kind: ProviderKind::Custom,
    },
    ProviderPreset {
        id: "qwen",
        label: "通义千问（阿里百炼）",
        protocol: Protocol::OpenAiCompat,
        base_url: "https://dashscope.aliyuncs.com/compatible-mode/v1",
        aliases: &["qwen", "dashscope", "tongyi", "通义", "阿里", "百炼"],
        category: Category::Domestic,
        verified_on: "2026-10-28",
        requires_api_key: true,
        kind: ProviderKind::Custom,
    },
    ProviderPreset {
        id: "moonshot",
        label: "Kimi（月之暗面）",
        protocol: Protocol::OpenAiCompat,
        base_url: "https://api.moonshot.cn/v1",
        aliases: &["moonshot", "kimi", "月之暗面", "moonshot.cn"],
        category: Category::Domestic,
        verified_on: "2026-10-28",
        requires_api_key: true,
        kind: ProviderKind::Custom,
    },
    ProviderPreset {
        id: "zhipu",
        label: "智谱 GLM",
        protocol: Protocol::OpenAiCompat,
        base_url: "https://open.bigmodel.cn/api/paas/v4",
        aliases: &["zhipu", "bigmodel", "glm", "智谱"],
        category: Category::Domestic,
        verified_on: "2026-10-28",
        requires_api_key: true,
        kind: ProviderKind::Custom,
    },
    ProviderPreset {
        id: "volcengine",
        label: "豆包（火山方舟）",
        protocol: Protocol::OpenAiCompat,
        base_url: "https://ark.cn-beijing.volces.com/api/v3",
        aliases: &["volcengine", "ark", "doubao", "豆包", "火山", "volces"],
        category: Category::Domestic,
        verified_on: "2026-10-28",
        requires_api_key: true,
        kind: ProviderKind::Custom,
    },
    ProviderPreset {
        id: "hunyuan",
        label: "腾讯混元",
        protocol: Protocol::OpenAiCompat,
        base_url: "https://api.hunyuan.cloud.tencent.com/v1",
        aliases: &["hunyuan", "tencent", "混元", "腾讯"],
        category: Category::Domestic,
        verified_on: "2026-10-28",
        requires_api_key: true,
        kind: ProviderKind::Custom,
    },
    ProviderPreset {
        id: "siliconflow",
        label: "硅基流动 SiliconFlow",
        protocol: Protocol::OpenAiCompat,
        base_url: "https://api.siliconflow.cn/v1",
        aliases: &["siliconflow", "硅基流动", "silicon"],
        category: Category::Aggregator,
        verified_on: "2026-10-28",
        requires_api_key: true,
        kind: ProviderKind::Custom,
    },
    ProviderPreset {
        id: "stepfun",
        label: "阶跃星辰 Step",
        protocol: Protocol::OpenAiCompat,
        base_url: "https://api.stepfun.com/v1",
        aliases: &["stepfun", "step", "阶跃"],
        category: Category::Domestic,
        verified_on: "2026-10-28",
        requires_api_key: true,
        kind: ProviderKind::Custom,
    },
    ProviderPreset {
        id: "lingyiwanwu",
        label: "零一万物 Yi",
        protocol: Protocol::OpenAiCompat,
        base_url: "https://api.lingyiwanwu.com/v1",
        aliases: &["lingyiwanwu", "yi", "零一万物", "01"],
        category: Category::Domestic,
        verified_on: "2026-10-28",
        requires_api_key: true,
        kind: ProviderKind::Custom,
    },
    ProviderPreset {
        id: "spark",
        label: "讯飞星火 Spark",
        protocol: Protocol::OpenAiCompat,
        base_url: "https://spark-api-open.xf-yun.com/v1",
        aliases: &["spark", "xfyun", "xinghuo", "讯飞", "星火"],
        category: Category::Domestic,
        verified_on: "2026-10-28",
        requires_api_key: true,
        kind: ProviderKind::Custom,
    },
    // ── 聚合平台 ───────────────────────────────────────────────
    ProviderPreset {
        id: "openrouter",
        label: "OpenRouter",
        protocol: Protocol::OpenAiCompat,
        base_url: "https://openrouter.ai/api/v1",
        aliases: &["openrouter", "open router"],
        category: Category::Aggregator,
        verified_on: "2026-10-28",
        requires_api_key: true,
        kind: ProviderKind::Custom,
    },
    ProviderPreset {
        id: "groq",
        label: "Groq",
        protocol: Protocol::OpenAiCompat,
        base_url: "https://api.groq.com/openai/v1",
        aliases: &["groq", "groq.com"],
        category: Category::Aggregator,
        verified_on: "2026-10-28",
        requires_api_key: true,
        kind: ProviderKind::Custom,
    },
    // ── 自建 / 本地 ────────────────────────────────────────────
    ProviderPreset {
        id: "ollama",
        label: "Ollama（本地）",
        protocol: Protocol::Ollama,
        base_url: "http://localhost:11434",
        aliases: &["ollama", "local", "本地", "localhost"],
        category: Category::SelfHosted,
        verified_on: "2026-10-28",
        requires_api_key: false,
        kind: ProviderKind::Ollama,
    },
    // ── zBiz 本地网关 ──────────────────────────────────────────
    //
    // 家族自建的本地推理网关（`~/zbiz-ops/zbiz_ai_api.py`），把一台机器上的
    // 文本 / 语音 / 图像 / 视频能力收敛成 **OpenAI 兼容** 的单一入口。
    //
    // 一条预设解决三件事：
    //   1. 地址不用抄 —— 默认跑在网关所在机器的 `127.0.0.1:8200`；
    //      换成局域网另一台机器（如 mini）时，用户在 UI 里改地址即可，
    //      不需要 `http://` 手打、不需要知道 `/v1` 加在哪。
    //   2. 不需要密钥 —— 本机回环流量，网关本身不校验 Authorization。
    //      填任意占位串也能通，但别让界面逼用户编一个假 Key。
    //   3. 模型名不预设 —— 与目录的硬规矩一致：网关的 `/v1/models`
    //      会返回当前真实加载的模型，运行时现取。
    //
    // 为什么 `kind` 是 `Custom` 而不是新开一个变体：网关对外就是
    // OpenAI 兼容协议，报文、鉴权、SSE 分帧与 OpenAI 完全一致，
    // 差别只在地址和「不需要 Key」。新开变体会让 note / terminal / mind
    // 各自维护的 `match` 全部要改，而协议上毫无差别。
    ProviderPreset {
        id: "zbiz-local",
        label: "zBiz 本地网关（自建）",
        protocol: Protocol::OpenAiCompat,
        base_url: "http://127.0.0.1:8200/v1",
        aliases: &[
            "zbiz",
            "zbiz-local",
            "zbiz-local-gateway",
            "智工栈",
            "本地网关",
            "自建网关",
            "mini",
            "mac-mini",
        ],
        category: Category::SelfHosted,
        verified_on: "2026-10-31",
        requires_api_key: false,
        kind: ProviderKind::Custom,
    },
];

/// 按 id 找预设。id 大小写不敏感。
pub fn find(id: &str) -> Option<&'static ProviderPreset> {
    let key = id.trim().to_ascii_lowercase();
    PRESETS.iter().find(|p| p.id == key)
}

/// 按名字或别名找预设。**先精确匹配 id，再匹配别名** —— 别名表里有
/// `"openai"`、`"local"` 这类很宽的词，所以 id 优先。
pub fn find_by_name(name: &str) -> Option<&'static ProviderPreset> {
    let key = name.trim().to_ascii_lowercase();
    if key.is_empty() {
        return None;
    }
    if let Some(p) = find(&key) {
        return Some(p);
    }
    PRESETS.iter().find(|p| p.aliases.iter().any(|a| *a == key))
}

/// 列出某分类下的全部预设。
pub fn by_category(category: Category) -> impl Iterator<Item = &'static ProviderPreset> {
    PRESETS.iter().filter(move |p| p.category == category)
}

/// 用预设 id 造一份配置（模型名留空，由调用方用 `list_models` 现取）。
///
/// 找不到 id 时返回 `ErrorKind::Unsupported`，并把 id 原文带进错误消息 ——
/// 「未知 provider」这种报错必须让人一眼看出是自己拼错了哪个词。
pub fn config_for(id: &str, api_key: impl Into<String>) -> Result<LlmConfig, LlmError> {
    let preset =
        find(id).ok_or_else(|| LlmError::not_configured(format!("未知的供应商预设：{}", id)))?;
    Ok(preset.to_config(api_key, String::new()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Action;

    #[test]
    fn every_preset_has_unique_id() {
        let mut ids: Vec<&str> = PRESETS.iter().map(|p| p.id).collect();
        ids.sort_unstable();
        let before = ids.len();
        ids.dedup();
        assert_eq!(before, ids.len(), "预设 id 有重复");
    }

    #[test]
    fn every_preset_has_non_empty_base_url() {
        for p in PRESETS {
            assert!(!p.base_url.trim().is_empty(), "{} 的 base_url 为空", p.id);
            assert!(
                p.base_url.starts_with("http://") || p.base_url.starts_with("https://"),
                "{} 的 base_url 不是合法 URL：{}",
                p.id,
                p.base_url
            );
        }
    }

    #[test]
    fn every_preset_has_verified_on_date() {
        // 没有核对日期的预设等于没有 —— 这个字段是可信度锚点
        for p in PRESETS {
            assert!(
                p.verified_on.len() == 10 && p.verified_on.contains('-'),
                "{} 缺少 verified_on（应为 YYYY-MM-DD）",
                p.id
            );
        }
    }

    #[test]
    fn no_preset_carries_a_hardcoded_model_name() {
        // 预设只管「哪一家 + 什么协议 + 什么地址」，模型名交给运行时取。
        // 这条是硬约束，防止后来人「顺手」加个 model 字段。
        // （结构上没有 model 字段，这里是文档级断言的兜底说明。）
        assert!(PRESETS.iter().all(|p| !p.id.contains("gpt")));
        assert!(PRESETS.iter().all(|p| !p.id.contains("claude")));
    }

    #[test]
    fn find_is_case_insensitive() {
        assert!(find("OpenAI").is_some());
        assert!(find("  deepseek  ").is_some());
        assert!(find("nope").is_none());
    }

    #[test]
    fn find_by_name_matches_aliases() {
        assert_eq!(find_by_name("claude").unwrap().id, "anthropic");
        assert_eq!(find_by_name("kimi").unwrap().id, "moonshot");
        assert_eq!(find_by_name("glm").unwrap().id, "zhipu");
        assert_eq!(find_by_name("doubao").unwrap().id, "volcengine");
        assert_eq!(find_by_name("本地").unwrap().id, "ollama");
    }

    #[test]
    fn find_by_name_prefers_exact_id_over_alias() {
        // "local" 是 ollama 的别名，但也是历史配置里 "custom/local" 的写法。
        // id 优先，保证 "ollama" 不会被别名抢走。
        assert_eq!(find_by_name("ollama").unwrap().id, "ollama");
    }

    #[test]
    fn find_by_name_empty_returns_none() {
        assert!(find_by_name("").is_none());
        assert!(find_by_name("   ").is_none());
    }

    #[test]
    fn preset_produces_working_endpoint() {
        // 这是最关键的一条：每条预设在「地址 + 版本段归一」后必须能拼出合法端点。
        // 抄错一个字符就会 404，而 404 是最难查的一类故障。
        for p in PRESETS {
            let cfg = p.to_config("sk-test-0000000000", "test-model");
            let ep = cfg
                .endpoint(Action::Chat)
                .unwrap_or_else(|e| panic!("{} 拼不出端点：{}", p.id, e));
            assert!(ep.starts_with("http"), "{} 端点不是 http：{}", p.id, ep);
            assert!(
                !ep.contains("//v1//") && !ep.ends_with('/'),
                "{} 端点有重复斜杠或尾斜杠：{}",
                p.id,
                ep
            );
        }
    }

    #[test]
    fn keyless_presets_are_loopback_only() {
        // 免 Key 只可能是「跑在自己机器上的服务」—— 云厂商给免费额度也仍要
        // Key，不能因为「免费」就把 requires_api_key 设成 false（那会让用户
        // 以为不填就能用，实际拿到 401）。
        //
        // 2026-10-31 加入 zbiz-local 后，这条从「只有 ollama」放宽为白名单 +
        // 分类双重约束：必须同时满足「在白名单里」和「属于自建分类」。
        // 光有白名单会漏掉「有人把云厂商错误地标成自建」这种情况。
        const KEYLESS_WHITELIST: &[&str] = &["ollama", "zbiz-local"];
        for p in PRESETS {
            if !p.requires_api_key {
                assert!(
                    KEYLESS_WHITELIST.contains(&p.id),
                    "{} 不该免 key —— 免 key 只允许本机/自建服务，新增请先论证",
                    p.id
                );
                assert_eq!(
                    p.category,
                    Category::SelfHosted,
                    "{} 免 key 却不在「自建 / 本地」分类下",
                    p.id
                );
            }
        }
    }

    #[test]
    fn zbiz_local_points_at_loopback_and_needs_no_key() {
        // 这条预设的存在理由：让用户在任何一个 zBiz 产品里选「本地网关」就
        // 直接能用自建推理服务，不用手打 URL、不用编一个假 API Key。
        let p = find("zbiz-local").expect("缺 zbiz-local 预设");
        assert_eq!(p.protocol, Protocol::OpenAiCompat);
        assert_eq!(p.kind, ProviderKind::Custom);
        assert!(!p.requires_api_key, "本机回环流量不该要求密钥");
        assert!(
            p.base_url.starts_with("http://127.0.0.1:") || p.base_url.starts_with("http://localhost:"),
            "默认地址必须是回环地址 —— 写死某台机器的局域网 IP，别人装上就连不上：{}",
            p.base_url
        );
        // 端点拼接必须干净（这条预设也走通用的 endpoint() 归一逻辑）
        let ep = p.to_config("", "test-model").endpoint(Action::Chat).unwrap();
        assert_eq!(ep, "http://127.0.0.1:8200/v1/chat/completions");
    }

    #[test]
    fn zbiz_local_is_discoverable_by_chinese_aliases() {
        // 中文界面里用户会搜「本地网关」「智工栈」，别名表要能命中。
        assert_eq!(find_by_name("本地网关").unwrap().id, "zbiz-local");
        assert_eq!(find_by_name("智工栈").unwrap().id, "zbiz-local");
        assert_eq!(find_by_name("zbiz").unwrap().id, "zbiz-local");
    }

    #[test]
    fn custom_protocol_presets_get_real_base_url() {
        // 这是本模块存在的全部理由：
        // deepseek 之类的预设必须给出真实地址，不能退化成「必须手填」。
        for id in ["deepseek", "qwen", "moonshot", "zhipu", "volcengine"] {
            let p = find(id).unwrap_or_else(|| panic!("缺预设：{}", id));
            assert_eq!(p.kind, ProviderKind::Custom);
            assert!(!p.base_url.is_empty());
            let cfg = p.to_config("k", "m");
            assert_eq!(cfg.base_url, p.base_url);
        }
    }

    #[test]
    fn minimax_keeps_its_own_identity() {
        // minimax 单列一项是有理由的（shared 仓当初拆 provider 分支就是为了它）：
        // 「配置写 minimax 就真的打 minimax」在代码里看得见。
        let p = find("minimax").unwrap();
        assert_eq!(p.kind, ProviderKind::MiniMax);
        assert_eq!(p.protocol, Protocol::OpenAiCompat);
    }

    #[test]
    fn config_for_unknown_id_reports_the_offending_name() {
        let err = config_for("deepseek-v9", "k").unwrap_err();
        assert!(
            err.message.contains("deepseek-v9"),
            "错误消息没带上错的名字"
        );
    }

    #[test]
    fn config_for_known_id_works() {
        let cfg = config_for("deepseek", "sk-1234567890").unwrap();
        assert_eq!(cfg.kind, ProviderKind::Custom);
        assert_eq!(cfg.base_url, "https://api.deepseek.com/v1");
        assert!(cfg.model.is_empty(), "模型名应由运行时取，预设不预填");
    }

    #[test]
    fn by_category_partitions_presets() {
        // 分类是给 UI 用的，必须是完整划分，不能有预设漏掉
        let total: usize = [
            Category::International,
            Category::Domestic,
            Category::Aggregator,
            Category::SelfHosted,
        ]
        .iter()
        .map(|c| by_category(*c).count())
        .sum();
        assert_eq!(total, PRESETS.len());
    }
}
