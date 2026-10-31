//! zBiz 家族统一 SSO（ctc 侧）的 **IO 胶水**。
//!
//! # 这个 crate 负责什么、不负责什么
//!
//! | | 位置 |
//! |---|---|
//! | OAuth2 / OIDC 协议流程（state 校验、PKCE、换 token、解析身份） | `z-biz-tool-shared/src/sso/oauth.ts` |
//! | **本地回环回调服务器** | 本 crate `callback` 模块 |
//! | **跨平台开浏览器** | 本 crate `opener` 模块 |
//! | **OS 密钥库安全存储** | 本 crate `secrets` 模块 |
//!
//! ## 为什么协议逻辑不放进 Rust
//!
//! 两套实现**必然漂移**。OAuth 的坑集中在细节上（state 要回传、PKCE 的
//! code_verifier 要一字不差、token 端点要发 form-urlencoded 而不是 JSON、
//! 回调要先查 error 再查 code）—— 每一处都需要单测覆盖。
//!
//! TypeScript 侧的协议层能被 node 的 `--test` 直接跑（30 条用例，实测覆盖到
//! 中文 id_token 解码、字节数计算、state 不匹配的拒绝路径），而 Rust 侧要做
//! 同样的事需要一套完全不同的测试基建。
//!
//! 所以：**协议只有一份，在 TS；Rust 只做「IO 机械活」** ——
//! 占一个端口、开一个浏览器、读写一条密钥库条目。这三件事没有协议含量，
//! 却必须在系统层做（webview 里不能起 TCP 监听，也读不到 OS 密钥库）。
//!
//! ## feature 隔离
//!
//! `default = []` —— 核心能力（回环服务器 / 密钥库）零 tauri 依赖，
//! 可被 node 测试、CLI 工具、非 Tauri 宿主直接复用。
//! Tauri 应用打开 `tauri` feature。

pub mod callback;
pub mod opener;
pub mod secrets;

pub use callback::{CallbackError, CallbackServer};
pub use opener::{open_url, validate_url, OpenError};
pub use secrets::{SecretError, SecureStore};

/// 本仓 SSO 使用的默认回环路径。
///
/// 与 ctc 侧登记的 `redirect_uri` 路径部分必须一致 ——
/// 换句话说，ctc 那边登记的应当是 `http://127.0.0.1:<端口>/callback`。
pub const DEFAULT_CALLBACK_PATH: &str = "/callback";

/// 回环服务器的默认等待时长。
///
/// 5 分钟：够用户去 ctc 那边输密码走完 2FA，也不至于让一个没人管的
/// 回调服务在后台挂太久。
pub const DEFAULT_CALLBACK_TIMEOUT_SECS: u64 = 300;
