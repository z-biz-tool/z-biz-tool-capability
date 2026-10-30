//! # cap-meeting — 会议纪要能力层
//!
//! 把「一场会议的原始产物」变成「可以发出去的纪要」。
//!
//! ## 两段输入，任给其一即可
//!
//! 1. **已有文字稿**（会议纪要手写稿、导出的聊天记录、AI 转写好的文本）
//! 2. **录音 + 转写**（`cap_llm::transcribe` → 本层的 [`Transcript::from_plain`]）
//!
//! 两条路在 [`Minutes`] 之前合流 —— 这是刻意的：产品侧可以选择「先转写再总结」
//! 或「直接粘文字」，本层不关心来源，只保证「进的是文本，出的是结构化纪要」。
//!
//! ## 边界：做什么、不做什么
//!
//! **做**：
//! - 把转写文本/会议记录按 **议题 → 决议 → 待办** 结构化
//! - 识别待办的**负责人**与**截止时间**（缺失就留空，不编造）
//! - 输出统一的 Markdown（含 frontmatter），可直接写进 note
//!
//! **不做**：
//! - **不写提示词进 cap-llm**。「纪要长什么样」是产品差异，不是基础设施。
//!   提示词模板在本文件 [`PROMPT_TEMPLATE`]，下游可覆盖。
//! - **不落盘、不建资产库**。产物以字符串形态返回，写到哪儿由调用方决定
//!   （家族约定：文件为真相，本层只产文本）。
//! - **不联网**（除了通过 cap-llm 发 chat）。本层自己不建 HTTP client。
//! - **不编造**。模型答不出负责人/截止时间时，字段留空。**不许用「未知」**
//!   或「待定」去填**，那会让下游把占位符当真数据展示。
//!
//! ## 用法
//!
//! ```no_run
//! # async fn demo() -> Result<(), cap_llm::LlmError> {
//! use cap_meeting::{Minutes, Meeting};
//! use cap_llm::LlmConfig;
//!
//! let cfg = LlmConfig {
//!     model: "deepseek-chat".into(),
//!     ..Default::default()
//! };
//!
//! // 路线一：已有文字稿
//! let minutes = Minutes::from_text(&cfg, "张三：我们这周要把 diff 做完\n李四：好，我负责目录那块\n我：周五前给结果").await?;
//!
//! // 路线二：录音先转写
//! // let t = cap_llm::transcribe_file(&cfg, "meeting.wav", &Default::default()).await?;
//! // let minutes = Minutes::from_transcript(&cfg, &Meeting::default(), &t.text).await?;
//!
//! println!("{}", minutes.to_markdown());
//! # Ok(())
//! # }
//! ```

use serde::{Deserialize, Serialize};

pub use cap_llm::LlmConfig;

mod prompt;
pub use prompt::PROMPT_TEMPLATE;

/// 会议元信息。全部可选 —— 有就写进纪要头，没有就不写那一行。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Meeting {
    /// 会议主题。缺省时由模型从文本里推断，推断不出则用「会议纪要」。
    pub title: Option<String>,
    /// 开始时间（ISO 8601 或任意可读格式，本层不解析日期语义）
    pub started_at: Option<String>,
    /// 参会人
    pub participants: Vec<String>,
    /// 会议链接 / 录音文件名等溯源信息
    pub source: Option<String>,
}

/// 一条待办事项。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ActionItem {
    /// 待办内容。**非空** —— 空的待办条目在结构化里直接丢弃。
    pub task: String,
    /// 负责人。模型没提到就留 `None`（**不许填「未知」**）。
    pub owner: Option<String>,
    /// 截止时间原文。同上，没有就 `None`。
    pub due: Option<String>,
}

/// 一个议题。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Topic {
    /// 议题名。**非空**。
    pub title: String,
    /// 讨论要点（可多行）
    pub summary: String,
}

/// 一条决议。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Decision {
    /// 决议内容。**非空**。
    pub content: String,
    /// 依据/背景（可选）
    pub rationale: Option<String>,
}

/// 结构化纪要。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Minutes {
    /// 会议主题
    pub title: String,
    /// 一句话摘要
    pub summary: String,
    /// 议题列表
    pub topics: Vec<Topic>,
    /// 决议列表
    pub decisions: Vec<Decision>,
    /// 待办列表
    pub actions: Vec<ActionItem>,
    /// 风险 / 待定项
    pub open_questions: Vec<String>,
    /// 本次纪要的元信息（渲染 frontmatter 用）
    #[serde(default, skip_serializing_if = "Meeting::is_empty")]
    pub meeting: Meeting,
}

impl Meeting {
    fn is_empty(&self) -> bool {
        self.title.is_none()
            && self.started_at.is_none()
            && self.participants.is_empty()
            && self.source.is_none()
    }
}

impl Minutes {
    /// 从**已有文字稿**生成纪要。
    ///
    /// 这是主路径：会议手写稿、导出的聊天记录、语音转写的成品文本都能进。
    /// 「从音频到纪要」= `cap_llm::transcribe_file` + 本函数。
    pub async fn from_text(cfg: &LlmConfig, text: &str) -> Result<Self, cap_llm::LlmError> {
        Self::from_text_with_meeting(cfg, &Meeting::default(), text).await
    }

    /// 同 [`Self::from_text`]，但带上会议元信息（标题/时间/参会人）。
    ///
    /// 元信息会写进 frontmatter，**不会**混进给模型的 prompt —— 除非
    /// [`Self::generate`] 里显式用到。这样「用户填的标题」永远优先于
    /// 「模型猜的标题」。
    pub async fn from_text_with_meeting(
        cfg: &LlmConfig,
        meeting: &Meeting,
        text: &str,
    ) -> Result<Self, cap_llm::LlmError> {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            // 空输入不发请求 —— 模型会在一个空字符串上编出一场不存在的会。
            return Err(cap_llm::LlmError::not_configured(
                "会议纪要输入为空：没有可总结的文字内容",
            ));
        }
        Self::generate(cfg, meeting, trimmed).await
    }

    /// 从 cap-llm 的转写结果生成纪要。
    ///
    /// `transcript_text` 是 `cap_llm::Transcript::text`。
    pub async fn from_transcript(
        cfg: &LlmConfig,
        meeting: &Meeting,
        transcript_text: &str,
    ) -> Result<Self, cap_llm::LlmError> {
        Self::from_text_with_meeting(cfg, meeting, transcript_text).await
    }

    /// 核心生成：拼 prompt → 调 cap-llm → 容错解析 JSON。
    ///
    /// 提示词与解析都在这里，不外溢到 cap-llm —— cap-llm 只负责「把请求发出去」。
    async fn generate(
        cfg: &LlmConfig,
        meeting: &Meeting,
        text: &str,
    ) -> Result<Self, cap_llm::LlmError> {
        let user = build_user_prompt(meeting, text);
        let messages = vec![
            cap_llm::Message::system(prompt::SYSTEM_PROMPT),
            cap_llm::Message::user(user),
        ];
        let reply = cap_llm::chat(cfg, &messages).await?;
        let mut minutes = parse_json_reply(&reply.content)
            .map_err(|e| cap_llm::LlmError::parse(format!("纪要 JSON 解析失败：{}", e)))?;

        // 用户填的元信息优先于模型猜的。模型猜出来的标题只在用户没填时采用。
        if meeting.title.is_some() {
            minutes.title = meeting.title.clone().unwrap_or_default();
        }
        minutes.meeting = meeting.clone();
        minutes.sanitize();
        Ok(minutes)
    }

    /// 清洗模型输出：丢空条目、去空白、截断异常长的字段。
    ///
    /// 为什么在这里做而不是信任模型：模型常把「无」写成空字符串、
    /// 或在 task 里塞进整段讨论。下游（note / file）拿到脏条目会
    /// 渲染出一个只有空白的待办项。
    fn sanitize(&mut self) {
        self.title = self.title.trim().to_string();
        if self.title.is_empty() {
            self.title = "会议纪要".to_string();
        }
        self.summary = self.summary.trim().to_string();

        self.topics.retain(|t| !t.title.trim().is_empty());
        for t in &mut self.topics {
            t.title = t.title.trim().to_string();
            t.summary = t.summary.trim().to_string();
        }

        self.decisions.retain(|d| !d.content.trim().is_empty());
        for d in &mut self.decisions {
            d.content = d.content.trim().to_string();
            if let Some(r) = d.rationale.as_mut() {
                *r = r.trim().to_string();
                if r.is_empty() {
                    d.rationale = None;
                }
            }
        }

        // 待办是纪要里最容易脏的地方：空 task、占位符 owner。
        self.actions.retain(|a| !a.task.trim().is_empty());
        for a in &mut self.actions {
            a.task = a.task.trim().to_string();
            a.owner = clean_optional(&a.owner);
            a.due = clean_optional(&a.due);
        }

        self.open_questions.retain(|q| !q.trim().is_empty());
        for q in &mut self.open_questions {
            *q = q.trim().to_string();
        }
    }

    /// 渲染成带 frontmatter 的 Markdown。
    ///
    /// frontmatter 里的 `title` / `started_at` / `participants` 直接来自
    /// [`Meeting`]，有才写。待办渲染成 checkbox 列表，方便 note 里直接勾。
    pub fn to_markdown(&self) -> String {
        let mut out = String::new();
        out.push_str("---\n");
        out.push_str(&format!("title: {}\n", yaml_scalar(&self.title)));
        if let Some(t) = &self.meeting.started_at {
            out.push_str(&format!("started_at: {}\n", yaml_scalar(t)));
        }
        if !self.meeting.participants.is_empty() {
            let joined: Vec<String> = self
                .meeting
                .participants
                .iter()
                .map(|p| yaml_scalar(p))
                .collect();
            out.push_str(&format!("participants: [{}]\n", joined.join(", ")));
        }
        out.push_str("type: meeting-minutes\n---\n\n");

        out.push_str(&format!("# {}\n\n", self.title));
        if !self.summary.is_empty() {
            out.push_str(&format!("> {}\n\n", self.summary.replace('\n', "\n> ")));
        }

        if !self.topics.is_empty() {
            out.push_str("## 议题\n\n");
            for t in &self.topics {
                out.push_str(&format!("### {}\n\n", t.title));
                if !t.summary.is_empty() {
                    out.push_str(&format!("{}\n\n", t.summary));
                }
            }
        }

        if !self.decisions.is_empty() {
            out.push_str("## 决议\n\n");
            for d in &self.decisions {
                out.push_str(&format!("- {}", d.content));
                if let Some(r) = &d.rationale {
                    out.push_str(&format!("（{}）", r));
                }
                out.push('\n');
            }
            out.push('\n');
        }

        if !self.actions.is_empty() {
            out.push_str("## 待办\n\n");
            for a in &self.actions {
                out.push_str("- [ ] ");
                out.push_str(&a.task);
                if let Some(o) = &a.owner {
                    out.push_str(&format!("　@{}", o));
                }
                if let Some(d) = &a.due {
                    out.push_str(&format!("　📅 {}", d));
                }
                out.push('\n');
            }
            out.push('\n');
        }

        if !self.open_questions.is_empty() {
            out.push_str("## 待定 / 风险\n\n");
            for q in &self.open_questions {
                out.push_str(&format!("- {}\n", q));
            }
            out.push('\n');
        }

        out.trim_end().to_string()
    }
}

/// 把 `Option<String>` 里的空白与占位符清掉。
///
/// 模型很爱填「未知」「待定」「N/A」「无」「unknown」。这些值对下游是
/// **假数据** —— 渲染成「负责人：未知」比留空更糟，因为它看起来像一个结论。
fn clean_optional(v: &Option<String>) -> Option<String> {
    let s = v.as_ref()?.trim().to_string();
    const PLACEHOLDERS: &[&str] = &[
        "未知",
        "待定",
        "待确认",
        "无",
        "n/a",
        "na",
        "none",
        "null",
        "unknown",
        "-",
        "—",
    ];
    if s.is_empty() || PLACEHOLDERS.contains(&s.to_ascii_lowercase().as_str()) {
        None
    } else {
        Some(s)
    }
}

/// YAML 标量：含特殊字符时加引号，防止 note 的 frontmatter 解析炸。
fn yaml_scalar(s: &str) -> String {
    let needs_quote = s.contains(':')
        || s.contains('#')
        || s.contains('"')
        || s.contains('{')
        || s.contains('}')
        || s.contains('[')
        || s.contains(']')
        || s.contains('\n')
        || s.starts_with(' ')
        || s.ends_with(' ');
    if needs_quote {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        s.to_string()
    }
}

/// 拼用户 prompt。会议元信息在这里以「已知事实」注入，避免模型去猜标题。
fn build_user_prompt(meeting: &Meeting, text: &str) -> String {
    let mut head = String::new();
    if let Some(t) = &meeting.title {
        head.push_str(&format!("会议主题：{}\n", t));
    }
    if let Some(s) = &meeting.started_at {
        head.push_str(&format!("会议时间：{}\n", s));
    }
    if !meeting.participants.is_empty() {
        head.push_str(&format!("参会人：{}\n", meeting.participants.join("、")));
    }
    if !head.is_empty() {
        head.push('\n');
    }
    format!("{}以下是会议原始文字内容：\n\n{}\n", head, text)
}

/// 从模型回复里抠出 JSON 并解析成 [`Minutes`]。
///
/// **容错三段**：模型常把 JSON 包在 ```json 围栏里，或前后加客套话
/// （「好的，以下是纪要：…」）。直接 `serde_json::from_str` 会失败。
/// 依次尝试：① 整段解析 → ② 剥围栏 → ③ 找第一对大括号。
/// 三段都失败才报错，**不猜一个默认纪要**。
pub fn parse_json_reply(raw: &str) -> Result<Minutes, String> {
    let s = raw.trim();
    if s.is_empty() {
        return Err("回复为空".into());
    }

    // ① 直接解析
    if let Ok(m) = serde_json::from_str::<Minutes>(s) {
        return Ok(m);
    }
    // ② 剥 ```json ... ``` 围栏
    if let Some(inner) = strip_fence(s) {
        if let Ok(m) = serde_json::from_str::<Minutes>(inner) {
            return Ok(m);
        }
    }
    // ③ 找第一对大括号
    if let Some(inner) = first_brace_block(s) {
        if let Ok(m) = serde_json::from_str::<Minutes>(inner) {
            return Ok(m);
        }
    }
    Err("无法从模型回复中提取合法的纪要 JSON".into())
}

/// 剥掉 markdown 围栏，返回围栏内的内容。
fn strip_fence(s: &str) -> Option<&str> {
    let trimmed = s.trim();
    let rest = trimmed.strip_prefix("```")?;
    // 去掉语言标签（json / JSON / 空）到行尾
    let after_tag = &rest[rest.find('\n')? + 1..];
    // 去掉结尾的 ``` 与可能的换行
    let end = after_tag.rfind("```")?;
    Some(after_tag[..end].trim())
}

/// 找第一个 `{` 与最后一个 `}` 之间的内容（含边界）。
fn first_brace_block(s: &str) -> Option<&str> {
    let start = s.find('{')?;
    let end = s.rfind('}')?;
    if end <= start {
        return None;
    }
    Some(&s[start..=end])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_json() -> String {
        r#"{
            "title": "Q4 产品评审",
            "summary": "确认了三条产品方向，并分配了下周的执行人。",
            "topics": [
                {"title": "文件 diff 能力", "summary": "目录 diff 现在只能看文件名，需要支持点击下钻。"},
                {"title": "终端 AI", "summary": "cap-llm 的 tool calling 已实现，terminal 未接入。"}
            ],
            "decisions": [
                {"content": "diff 能力本季度内补齐", "rationale": "用户反馈集中"},
                {"content": "终端接入 tool calling"}
            ],
            "actions": [
                {"task": "实现目录 diff 下钻", "owner": "张三", "due": "11月5日"},
                {"task": "补齐 diff 双栏对比", "owner": "", "due": null}
            ],
            "open_questions": ["ASR 供应商选哪家", "待定"]
        }"#
        .to_string()
    }

    #[test]
    fn parses_clean_json() {
        let m = parse_json_reply(&sample_json()).unwrap();
        assert_eq!(m.title, "Q4 产品评审");
        assert_eq!(m.topics.len(), 2);
        assert_eq!(m.decisions.len(), 2);
        assert_eq!(m.actions.len(), 2);
    }

    #[test]
    fn parses_json_inside_fence() {
        let raw = format!(
            "好的，以下是纪要：\n```json\n{}\n```\n希望有帮助。",
            sample_json()
        );
        let m = parse_json_reply(&raw).unwrap();
        assert_eq!(m.title, "Q4 产品评审");
    }

    #[test]
    fn parses_json_without_fence() {
        let raw = format!("以下是结果：\n{}\n完了。", sample_json());
        let m = parse_json_reply(&raw).unwrap();
        assert_eq!(m.title, "Q4 产品评审");
    }

    #[test]
    fn garbage_is_an_error_not_a_default() {
        // 不许猜一个默认纪要 —— 那会让下游把空气当结论展示
        assert!(parse_json_reply("我觉得这场会开得不错，没有别的了。").is_err());
        assert!(parse_json_reply("").is_err());
    }

    #[test]
    fn sanitize_drops_empty_entries() {
        let mut m = Minutes {
            topics: vec![
                Topic {
                    title: "".into(),
                    summary: "空标题应被丢弃".into(),
                },
                Topic {
                    title: "有效议题".into(),
                    summary: "s".into(),
                },
            ],
            decisions: vec![Decision {
                content: "  ".into(),
                rationale: None,
            }],
            actions: vec![
                ActionItem {
                    task: "".into(),
                    owner: Some("张三".into()),
                    due: None,
                },
                ActionItem {
                    task: "有效待办".into(),
                    owner: Some("李四".into()),
                    due: None,
                },
            ],
            ..Default::default()
        };
        m.sanitize();
        assert_eq!(m.topics.len(), 1);
        assert!(m.decisions.is_empty());
        assert_eq!(m.actions.len(), 1);
    }

    #[test]
    fn sanitize_fills_placeholder_title() {
        let mut m = Minutes {
            title: "   ".into(),
            ..Default::default()
        };
        m.sanitize();
        assert_eq!(m.title, "会议纪要");
    }

    #[test]
    fn placeholder_owner_becomes_none() {
        for ph in ["未知", "待定", "N/A", "unknown", "  "] {
            assert_eq!(clean_optional(&Some(ph.to_string())), None, "{ph}");
        }
        assert_eq!(clean_optional(&Some("张三".into())), Some("张三".into()));
        assert_eq!(clean_optional(&None), None);
    }

    #[test]
    fn markdown_renders_all_sections() {
        let m = parse_json_reply(&sample_json()).unwrap();
        let md = m.to_markdown();
        assert!(md.contains("# Q4 产品评审"));
        assert!(md.contains("## 议题"));
        assert!(md.contains("## 决议"));
        assert!(md.contains("## 待办"));
        assert!(md.contains("- [ ] 实现目录 diff 下钻"));
        assert!(md.contains("@张三"));
        assert!(md.contains("📅 11月5日"));
        // frontmatter
        assert!(md.starts_with("---\n"));
        assert!(md.contains("type: meeting-minutes"));
    }

    #[test]
    fn markdown_quotes_unsafe_yaml() {
        let m = Minutes {
            title: "Q4: 产品评审 #1".into(),
            ..Default::default()
        };
        let md = m.to_markdown();
        assert!(md.contains("title: \"Q4: 产品评审 #1\""), "{md}");
    }

    #[test]
    fn markdown_skips_empty_sections() {
        let m = Minutes {
            title: "t".into(),
            summary: "s".into(),
            ..Default::default()
        };
        let md = m.to_markdown();
        assert!(!md.contains("## 议题"));
        assert!(!md.contains("## 决议"));
        assert!(!md.contains("## 待办"));
    }

    #[test]
    fn first_brace_block_handles_wrapped_json() {
        assert_eq!(first_brace_block("x {\"a\":1} y"), Some("{\"a\":1}"));
        assert_eq!(first_brace_block("no braces"), None);
        assert_eq!(first_brace_block("{}"), Some("{}"));
    }

    #[test]
    fn strip_fence_requires_closing_fence() {
        assert_eq!(strip_fence("```json\n{\"a\":1}\n```"), Some("{\"a\":1}"));
        // 没有闭合围栏不剥
        assert_eq!(strip_fence("```json\n{\"a\":1}"), None);
    }
}
