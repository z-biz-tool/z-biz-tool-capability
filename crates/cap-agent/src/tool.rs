//! 工具注册表：把「App 自己的函数」声明成模型能调的工具。
//!
//! 工具用 JSON Schema 描述入参（与 cap-llm 的 [`cap_llm::ToolDef`] 同形状），
//! 调用是异步的。这里**不引入任何 crate**：`Tool` 用 `Pin<Box<dyn Future>>`
//! 而非 `async fn in trait`，是为了保住 `dyn Tool` 的可对象化能力
//! —— 注册表要装异构工具，不能要求每个工具都装箱成泛型。

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use cap_llm::ToolDef;
use serde_json::Value;

/// 一次工具调用的返回值。成功是文本，失败也是文本（错误要喂回模型，
/// 让它自己改正，而不是把整轮对话打断）。
pub type ToolOutput = Result<String, ToolError>;

/// 工具调用的异步返回值。`'static` 是为了能放进 `Arc<dyn Tool>` 里。
pub type ToolFuture<'a> = Pin<Box<dyn Future<Output = ToolOutput> + Send + 'a>>;

/// 一个可被模型调用的工具。
pub trait Tool: Send + Sync {
    /// 工具名。模型用它来调用，必须与注册表里的键一致。
    fn name(&self) -> &str;

    /// 给模型看的说明。**要写清楚这个工具做什么、什么时候用**——
    /// 模型选工具全靠这段话。
    fn description(&self) -> &str;

    /// 入参的 JSON Schema（对象形状：`{"type":"object","properties":{…}}`）。
    fn parameters(&self) -> Value;

    /// 执行。参数是模型给的 JSON 对象。
    fn call<'a>(&'a self, args: Value) -> ToolFuture<'a>;
}

/// 工具错误。三类分开，是为了调用方能决定「要不要重试 / 要不要提示用户」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolError {
    /// 注册表里没有这个工具名（模型幻觉出来的名字最常见）。
    NotFound(String),
    /// 工具名对，但参数不合法（缺字段、类型不对）。
    InvalidArguments(String),
    /// 工具自己执行失败（磁盘只读、权限不足、外部服务挂了）。
    Failed(String),
}

impl ToolError {
    pub fn kind(&self) -> &'static str {
        match self {
            ToolError::NotFound(_) => "not_found",
            ToolError::InvalidArguments(_) => "invalid_arguments",
            ToolError::Failed(_) => "failed",
        }
    }

    pub fn message(&self) -> &str {
        match self {
            ToolError::NotFound(m) | ToolError::InvalidArguments(m) | ToolError::Failed(m) => m,
        }
    }

    /// 喂回模型的内容。带上前缀让模型知道是「工具没跑成」而不是用户说的话。
    pub fn to_model_text(&self) -> String {
        format!("[工具错误·{}] {}", self.kind(), self.message())
    }
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.kind(), self.message())
    }
}

impl std::error::Error for ToolError {}

/// 工具注册表。
///
/// 用 `BTreeMap` 而不是 `HashMap`：工具列表要发给模型，顺序稳定便于
/// 快照测试与 diff。
#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: BTreeMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: BTreeMap::new(),
        }
    }

    /// 注册一个工具。**同名会失败** —— 静默覆盖会让模型在两次调用之间
    /// 得到不同的实现，排查起来极难。
    pub fn register(&mut self, tool: Arc<dyn Tool>) -> Result<(), ToolError> {
        let name = tool.name().to_string();
        if self.tools.contains_key(&name) {
            return Err(ToolError::Failed(format!("工具名重复：{name}")));
        }
        self.tools.insert(name, tool);
        Ok(())
    }

    /// 用一个闭包注册工具 —— 这是最常见的用法：
    ///
    /// ```no_run
    /// # use std::sync::Arc;
    /// # use cap_agent::{ToolRegistry, ToolFuture};
    /// # fn demo(reg: &mut ToolRegistry) {
    /// reg.register_fn(
    ///     "list_dir",
    ///     "列出目录下的文件",
    ///     serde_json::json!({"type":"object","properties":{"path":{"type":"string"}}}),
    ///     |args| {
    ///         let path = args["path"].as_str().unwrap_or_default().to_string();
    ///         Box::pin(async move { Ok(format!("contents of {path}")) }) as ToolFuture<'static>
    ///     },
    /// )
    /// .unwrap();
    /// # }
    /// ```
    pub fn register_fn<F>(
        &mut self,
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: Value,
        f: F,
    ) -> Result<(), ToolError>
    where
        F: Fn(Value) -> ToolFuture<'static> + Send + Sync + 'static,
    {
        self.register(Arc::new(FnTool {
            name: name.into(),
            description: description.into(),
            parameters,
            f: Arc::new(f),
        }))
    }

    pub fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.tools.get(name)
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    pub fn names(&self) -> Vec<&str> {
        self.tools.keys().map(|s| s.as_str()).collect()
    }

    /// 转成 cap-llm 的工具声明，交给模型。
    pub fn defs(&self) -> Vec<ToolDef> {
        self.tools
            .values()
            .map(|t| ToolDef::new(t.name(), t.description(), t.parameters()))
            .collect()
    }
}

struct FnTool {
    name: String,
    description: String,
    parameters: Value,
    f: Arc<dyn Fn(Value) -> ToolFuture<'static> + Send + Sync>,
}

impl Tool for FnTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> Value {
        self.parameters.clone()
    }

    fn call<'a>(&'a self, args: Value) -> ToolFuture<'a> {
        (self.f)(args)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn echo_tool() -> Arc<dyn Tool> {
        Arc::new(FnTool {
            name: "echo".into(),
            description: "回显".into(),
            parameters: json!({"type":"object","properties":{"text":{"type":"string"}}}),
            f: Arc::new(|args: Value| {
                Box::pin(async move { Ok(args["text"].as_str().unwrap_or_default().to_string()) })
                    as ToolFuture<'static>
            }),
        })
    }

    #[test]
    fn register_and_lookup() {
        let mut reg = ToolRegistry::new();
        reg.register(echo_tool()).unwrap();
        assert_eq!(reg.len(), 1);
        assert!(reg.get("echo").is_some());
        assert!(reg.get("nope").is_none());
    }

    #[test]
    fn duplicate_name_is_rejected() {
        let mut reg = ToolRegistry::new();
        reg.register(echo_tool()).unwrap();
        let err = reg.register(echo_tool()).unwrap_err();
        assert!(matches!(err, ToolError::Failed(_)));
        assert_eq!(reg.len(), 1, "重复注册不能覆盖已有工具");
    }

    #[test]
    fn defs_carry_schema() {
        let mut reg = ToolRegistry::new();
        reg.register(echo_tool()).unwrap();
        let defs = reg.defs();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "echo");
        assert_eq!(defs[0].parameters["type"], "object");
        assert_eq!(defs[0].parameters["properties"]["text"]["type"], "string");
    }

    #[tokio::test]
    async fn register_fn_executes() {
        let mut reg = ToolRegistry::new();
        reg.register_fn(
            "sum",
            "求和",
            json!({"type":"object","properties":{"a":{"type":"number"},"b":{"type":"number"}}}),
            |args| {
                Box::pin(async move {
                    let a = args["a"].as_f64().unwrap_or(0.0);
                    let b = args["b"].as_f64().unwrap_or(0.0);
                    Ok((a + b).to_string())
                }) as ToolFuture<'static>
            },
        )
        .unwrap();
        let out = reg.get("sum").unwrap().call(json!({"a": 2, "b": 3})).await;
        assert_eq!(out.unwrap(), "5");
    }

    #[test]
    fn tool_error_kinds_are_distinct() {
        assert_eq!(ToolError::NotFound("x".into()).kind(), "not_found");
        assert_eq!(
            ToolError::InvalidArguments("x".into()).kind(),
            "invalid_arguments"
        );
        assert_eq!(ToolError::Failed("x".into()).kind(), "failed");
        assert!(ToolError::Failed("磁盘满".into())
            .to_model_text()
            .contains("磁盘满"));
    }
}
