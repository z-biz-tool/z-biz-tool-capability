//! 结构化失败：给界面「该做什么」，而不只是「出错了」。
//!
//! ## 为什么这一层存在
//!
//! 家族里有五套并存的失败表示（note / mind / file / terminal / pet 各写一份
//! `hint_for`），抄的是同一张表，抄到第 5 遍时就已经开始漂移：
//! note 与 mind 有 `focus_for`（该把光标送到哪一栏），terminal 与 file 没有；
//! 文案则各仓按自己的界面改写（「补齐上方标红的字段」vs「补齐选项 → 高级里
//! 空着的字段」）。
//!
//! 漂移的代价不是「文案不好看」，而是**行为不一致**：同一个 401，在 note 里
//! 会把光标跳到密钥框，在 terminal 里光标不动。用户没法建立预期，
//! 于是每次都要自己扫一遍面板找该改哪一栏。
//!
//! ## 三件事在这里定死
//!
//! 1. **分类**（`kind`）：机器可判，界面据此分流。
//! 2. **动作**（`hint`）：一句话说清「下一步做什么」。每一条都必须指向
//!    **一个具体动作**，而不是复述错误 ——「请求失败，请重试」的价值是零，
//!    用户已经知道失败了。
//! 3. **落点**（`focus`）：该把光标送到哪一栏。**必须与 hint 的文案一致** ——
//!    文案说「重新粘贴密钥」而光标停在地址栏，比不给光标更糟。
//!
//! ## 文案为什么不全归这里
//!
//! `hint` 的**分类与措辞骨架**在这里定死，但各仓可以在措辞上做**一次性**改写
//! （`Hinter` trait），用来指向自己界面里的具体位置（「在设置面板的密钥框」）。
//! 改写只允许发生在**措辞层**，不允许改变分类与落点 —— 否则就退回到各写一份。
//!
//! ## 为什么用 trait 而不是传闭包
//!
//! 各仓的改写点是**编译期已知的**，用 trait 的关联函数最省心；
//! 且 trait 的默认实现就是标准措辞，不想改的仓一行都不用写。

use serde::{Deserialize, Serialize};

use crate::error::{ErrorKind, LlmError};

/// 该把光标送到哪一栏。
///
/// 与 [`hint`] 的措辞必须一致。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Focus {
    /// 密钥栏（401 / 403 / 缺密钥）。
    ApiKey,
    /// 服务地址栏（404 / 协议不匹配）。
    BaseUrl,
    /// 模型名栏（该服务不支持当前动作）。
    Model,
    /// 不该乱跳 —— 焦点留在触发操作的按钮上。
    None,
}

impl Focus {
    /// 机器可判的字符串（与前端 `LlmFailure.focus` 对齐）。
    pub fn as_str(self) -> &'static str {
        match self {
            Focus::ApiKey => "apiKey",
            Focus::BaseUrl => "baseUrl",
            Focus::Model => "model",
            Focus::None => "none",
        }
    }
}

/// 错误分类的机器可判标识。
///
/// 刻意用**字符串**而不是 Rust 枚举对外：前端要拿它做分流，
/// 而 Rust 枚举跨 crate 演进时新增变体会破坏下游的穷尽匹配。
/// 对内（[`ErrorKind`]）仍然是强类型。
pub fn kind_key(kind: ErrorKind) -> &'static str {
    match kind {
        ErrorKind::NotConfigured => "notConfigured",
        ErrorKind::Network => "network",
        ErrorKind::Timeout => "timeout",
        ErrorKind::Http => "http",
        ErrorKind::Parse => "parse",
        ErrorKind::Unsupported => "unsupported",
        ErrorKind::Io => "io",
    }
}

/// 这个失败该把光标送到哪一栏。
///
/// **与 [`hint`] 的措辞必须一致** —— 这是本文件的硬约束，不是建议。
/// 分类与落点的对应关系：
///
/// | 分类 | 落点 | 理由 |
/// |---|---|---|
/// | 缺配置 | 无 | 缺的可能不是密钥（也可能是模型名），不能瞎跳 |
/// | 401 / 403 | 密钥 | 密钥无效或无权限 |
/// | 404 / 协议不匹配 | 地址 | 地址路径不对，或把别家协议填进来了 |
/// | 不支持该动作 | 模型 | 换模型或换服务 |
/// | 网络 / 超时 / 5xx | 无 | 不是配置问题，跳光标是误导 |
pub fn focus_for(kind: ErrorKind, status: Option<u16>) -> Focus {
    match (kind, status) {
        (ErrorKind::Http, Some(401)) | (ErrorKind::Http, Some(403)) => Focus::ApiKey,
        (ErrorKind::Http, Some(404)) | (ErrorKind::Parse, _) => Focus::BaseUrl,
        (ErrorKind::Unsupported, _) => Focus::Model,
        _ => Focus::None,
    }
}

/// 标准措辞：一句动机 + 一个动作。
///
/// 每一条都必须指向**一个具体动作**，而不是复述错误。
pub fn hint(kind: ErrorKind, status: Option<u16>) -> String {
    match (kind, status) {
        (ErrorKind::NotConfigured, _) => "配置还差一项。请补齐上方标红的字段后重试。".into(),
        (ErrorKind::Network, _) => {
            "连不上这个地址。国内厂商（DeepSeek / 通义 / Kimi / 智谱 / 豆包）一般直连即可，\
             如果你开了代理，先关掉再试；海外厂商（OpenAI / Gemini / Anthropic）相反，需要代理。"
                .into()
        }
        (ErrorKind::Timeout, _) => {
            "等太久没等到回应。可能是网络到该地区的链路慢，也可能是模型正在排队。\
             可以先换一个更小的模型试，或把超时调大。"
                .into()
        }
        (ErrorKind::Http, Some(401)) | (ErrorKind::Http, Some(403)) => {
            "API Key 无效、已过期，或这个 key 没有该模型的权限。请重新粘贴密钥；\
             若刚续费过，确认用的是新 key。"
                .into()
        }
        (ErrorKind::Http, Some(404)) => {
            "地址或模型名不存在。最常见的原因是 Base URL 多写／少写了一段路径 —— \
             各家都不完全一样（DeepSeek 带 /v1，智谱是 /api/paas/v4，火山是 /api/v3）。\
             在上方重选一次服务商，地址会自动填对。"
                .into()
        }
        (ErrorKind::Http, Some(429)) => {
            "额度用尽或被限流。这不是配置写错了 —— 换个时段再试，或在服务商后台充值／提额。".into()
        }
        (ErrorKind::Http, Some(s)) if s >= 500 => {
            "服务商那边出故障了，不是你配置的问题。稍后重试即可。".into()
        }
        (ErrorKind::Http, _) => "服务端拒绝了这次请求。原始错误见下方详情。".into(),
        (ErrorKind::Parse, _) => {
            "对方回了内容，但不是预期的格式。多半是这个地址其实不是该协议的服务 —— \
             例如把 Anthropic 的地址填在了 OpenAI 兼容这一档。"
                .into()
        }
        (ErrorKind::Unsupported, _) => {
            "这家服务不支持当前操作。换一个厂商，或改用手动填写模型名。".into()
        }
        (ErrorKind::Io, _) => "本机读写配置出错。检查配置文件所在目录是否可写。".into(),
    }
}

/// 给界面的结构化失败。
///
/// ## 为什么不是 `Result<_, String>`
///
/// 字符串错误只能被「读」不能被「判断」。界面要给出的恰恰是判断：
/// 401 该聚焦密钥框、404 该提示地址、429 该说「这不是你的配置问题」。
/// 靠正则去猜 `HTTP 401 — ...` 里的数字，等于把协议塞进文案里，
/// 哪天文案改一个字（比如加个中文破折号），分流就静默失效 ——
/// 而失效的表现是「所有错误都显示同一句话」，没人会当成 bug 报上来。
///
/// `Deserialize`：kb 仓把 `LlmFailure` 作为 `AnswerResult.failure` 字段
/// 经 IPC 回传（Tauri 返回值要序列化，但前端跨仓复用结构时也需要
/// 反序列化镜像），因此两方向都要支持。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LlmFailure {
    /// 机器可判的类别：notConfigured / network / timeout / http / parse / unsupported / io
    pub kind: &'static str,
    /// HTTP 状态码（仅 kind == "http" 时非空）。
    pub status: Option<u16>,
    /// 原始错误，已脱敏、已按字符截断。给「想知道细节」的用户。
    pub message: String,
    /// 一句话动机 + 一个动作。给「只想让它能用」的用户。
    pub hint: String,
    /// 该把光标送到哪一栏 —— 必须与 hint 的文案一致。
    pub focus: &'static str,
}

impl LlmFailure {
    /// 从协议层错误构造。
    pub fn from_error(e: LlmError) -> Self {
        Self {
            kind: kind_key(e.kind),
            status: e.status,
            message: e.to_string(),
            hint: hint(e.kind, e.status),
            focus: focus_for(e.kind, e.status).as_str(),
        }
    }

    /// 从一条本地校验消息构造（如「AI 未配置：缺 Base URL」）。
    ///
    /// 本地校验失败没有协议层错误可依，`kind` 归为 `notConfigured`，
    /// **落点一律 `none`** —— 本地校验可能缺任意一项，瞎跳比不跳更糟。
    ///
    /// 已知确切缺哪一项时用 [`LlmFailure::local`]，那里能精确落点。
    pub fn from_message(message: impl Into<String>) -> Self {
        Self {
            kind: kind_key(ErrorKind::NotConfigured),
            status: None,
            message: message.into(),
            hint: hint(ErrorKind::NotConfigured, None),
            focus: Focus::None.as_str(),
        }
    }

    /// 本地校验失败，**已知确切缺哪一项**。
    ///
    /// 与 [`LlmFailure::from_message`] 的区别：落点。
    /// `from_message` 面对的是「缺了什么不知道」的场合，落点 `none`；
    /// `local` 面对的是「我刚检查完，缺的就是密钥」这种场合，
    /// 落点必须精确 —— 用户被告知「缺密钥」却光标不动，等于还要自己找。
    ///
    /// ⚠️ 传 `focus` 前先确认文案与落点一致：文案说缺地址，focus 却是
    /// `ApiKey`，就是本文件要根除的那种漂移。
    pub fn local(message: impl Into<String>, focus: Focus) -> Self {
        Self {
            kind: kind_key(ErrorKind::NotConfigured),
            status: None,
            message: message.into(),
            hint: hint(ErrorKind::NotConfigured, None),
            focus: focus.as_str(),
        }
    }

    /// 本地校验失败，**措辞由调用方给**。
    ///
    /// ## 什么时候用这个
    ///
    /// 各仓的界面结构不同，标准措辞里的「上方标红的字段」在某个仓里可能
    /// 指不到任何东西（mind 的设置项在「选项 → 高级」里，note 在「AI 服务商」
    /// 里）。这类**指向自己界面位置**的措辞允许各仓覆盖一次。
    ///
    /// ## 覆盖的边界
    ///
    /// 只能覆盖 **hint 的措辞**，不能覆盖 `kind` 与 `focus`。
    /// 覆盖 `focus` 就退回到各写一份 —— 那正是本文件要根除的漂移。
    ///
    /// ⚠️ 传入的 hint 必须与 `focus` 指向同一栏：文案说「去改密钥」而
    /// focus 是 `BaseUrl`，用户按提示操作但没用。
    pub fn local_with_hint(
        message: impl Into<String>,
        focus: Focus,
        hint: impl Into<String>,
    ) -> Self {
        Self {
            kind: kind_key(ErrorKind::NotConfigured),
            status: None,
            message: message.into(),
            hint: hint.into(),
            focus: focus.as_str(),
        }
    }
}

impl From<String> for LlmFailure {
    fn from(message: String) -> Self {
        Self::from_message(message)
    }
}

impl From<&str> for LlmFailure {
    fn from(message: &str) -> Self {
        Self::from_message(message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_failure_kind_carries_an_actionable_hint() {
        let cases = [
            (ErrorKind::NotConfigured, None, "补齐"),
            (ErrorKind::Network, None, "代理"),
            (ErrorKind::Timeout, None, "超时"),
            (ErrorKind::Http, Some(401), "密钥"),
            (ErrorKind::Http, Some(403), "权限"),
            (ErrorKind::Http, Some(404), "Base URL"),
            (ErrorKind::Http, Some(429), "限流"),
            (ErrorKind::Http, Some(502), "故障"),
            (ErrorKind::Parse, None, "协议"),
            (ErrorKind::Unsupported, None, "不支持"),
            (ErrorKind::Io, None, "配置"),
        ];
        for (kind, status, must_mention) in cases {
            let text = hint(kind, status);
            assert!(
                text.contains(must_mention),
                "{:?}/{:?} 的提示里没提到「{}」，实际：{}",
                kind,
                status,
                must_mention,
                text
            );
            assert!(text.chars().count() >= 12, "提示过短：{}", text);
        }
    }

    #[test]
    fn hint_does_not_leak_the_api_key() {
        for (kind, status) in [
            (ErrorKind::Network, None),
            (ErrorKind::Http, Some(401)),
            (ErrorKind::Parse, None),
        ] {
            let text = hint(kind, status);
            assert!(!text.contains("sk-"), "提示里出现了疑似密钥：{}", text);
        }
    }

    #[test]
    fn focus_agrees_with_hint_about_which_field_to_fix() {
        // 这是本文件的核心不变式：**hint 指向哪一栏，focus 就必须是哪一栏**。
        // 漂移的表现是「提示说去改密钥，光标却跳到地址栏」—— 用户按提示操作但没用。
        let cases = [
            (ErrorKind::Http, Some(401), Focus::ApiKey, "密钥"),
            (ErrorKind::Http, Some(403), Focus::ApiKey, "密钥"),
            (ErrorKind::Http, Some(404), Focus::BaseUrl, "Base URL"),
            (ErrorKind::Parse, None, Focus::BaseUrl, "协议"),
            (ErrorKind::Unsupported, None, Focus::Model, "模型"),
        ];
        for (kind, status, want_focus, want_topic) in cases {
            let text = hint(kind, status);
            let got = focus_for(kind, status);
            assert_eq!(got, want_focus, "{:?}/{:?} 的落点不对", kind, status);
            assert!(
                text.contains(want_topic),
                "{:?}/{:?} 的提示没提到「{}」，实际：{}",
                kind,
                status,
                want_topic,
                text
            );
        }
    }

    #[test]
    fn focus_stays_put_when_the_problem_is_not_the_users_config() {
        // 网络 / 超时 / 5xx 不是配置问题 —— 跳光标是误导。
        for (kind, status) in [
            (ErrorKind::Network, None),
            (ErrorKind::Timeout, None),
            (ErrorKind::Http, Some(500)),
            (ErrorKind::Http, Some(429)),
            (ErrorKind::Io, None),
            (ErrorKind::NotConfigured, None),
        ] {
            assert_eq!(
                focus_for(kind, status),
                Focus::None,
                "{:?}/{:?} 不该跳光标",
                kind,
                status
            );
        }
    }

    #[test]
    fn local_validation_failure_does_not_guess_a_field() {
        // 本地校验可能缺地址也可能缺模型 —— 落点一律 none。
        let f = LlmFailure::from_message("AI 未配置：请先填写模型名");
        assert_eq!(f.kind, "notConfigured");
        assert_eq!(f.focus, "none");
        assert_eq!(f.message, "AI 未配置：请先填写模型名");
    }

    #[test]
    fn local_failure_can_name_the_field_it_knows_is_missing() {
        // 已知缺的就是密钥时，落点必须精确 —— 用户被告知缺密钥却光标不动，
        // 等于还要自己找那一栏。
        let f = LlmFailure::local("AI 未配置：请先填写 API Key", Focus::ApiKey);
        assert_eq!(f.kind, "notConfigured");
        assert_eq!(f.focus, "apiKey");
        assert_eq!(f.message, "AI 未配置：请先填写 API Key");
        // 分类与状态码不因落点而变
        assert_eq!(f.status, None);
    }

    #[test]
    fn local_failure_may_override_only_the_wording() {
        // 各仓的界面结构不同，指向自己界面位置的措辞允许覆盖一次。
        let f = LlmFailure::local_with_hint(
            "尚未配置 AI 接口地址。",
            Focus::BaseUrl,
            "在「选项 → 高级」里补齐这一项后再试。",
        );
        assert_eq!(f.kind, "notConfigured");
        assert_eq!(f.focus, "baseUrl");
        assert!(f.hint.contains("选项"));
        // 分类与落点不因措辞覆盖而变
        assert_eq!(f.status, None);
    }

    #[test]
    fn kind_key_matches_the_wire_contract() {
        // 这些字符串是前后端契约，改名会静默破坏界面的分流。
        assert_eq!(kind_key(ErrorKind::NotConfigured), "notConfigured");
        assert_eq!(kind_key(ErrorKind::Network), "network");
        assert_eq!(kind_key(ErrorKind::Timeout), "timeout");
        assert_eq!(kind_key(ErrorKind::Http), "http");
        assert_eq!(kind_key(ErrorKind::Parse), "parse");
        assert_eq!(kind_key(ErrorKind::Unsupported), "unsupported");
        assert_eq!(kind_key(ErrorKind::Io), "io");
    }

    #[test]
    fn from_error_carries_status_and_focus() {
        let e = LlmError::http(401, "invalid api key");
        let f = LlmFailure::from_error(e);
        assert_eq!(f.kind, "http");
        assert_eq!(f.status, Some(401));
        assert_eq!(f.focus, "apiKey");
        assert!(f.message.contains("401"));
    }

    #[test]
    fn focus_serializes_to_the_frontend_wire_names() {
        assert_eq!(Focus::ApiKey.as_str(), "apiKey");
        assert_eq!(Focus::BaseUrl.as_str(), "baseUrl");
        assert_eq!(Focus::Model.as_str(), "model");
        assert_eq!(Focus::None.as_str(), "none");
        // 序列化后的字段名要与前端 `LlmFailure.focus` 的字面量完全一致
        let json = serde_json::to_string(&Focus::ApiKey).unwrap();
        assert_eq!(json, "\"apiKey\"");
    }
}
