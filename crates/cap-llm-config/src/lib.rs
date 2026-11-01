//! cap-llm-config — 家族统一的 LLM 配置存储。
//!
//! # 这个 crate 解决什么
//!
//! 改造前，家族里 10 个有 AI 配置的仓**各有各的存法**：
//!
//! | 仓 | 配置落点 | 密钥 |
//! |---|---|---|
//! | note | 前端 localStorage | OS 密钥库（`z-biz-tool-note`） |
//! | terminal | `~/.z-terminal/config.json` | 自建 AES-256-GCM + `master.key` |
//! | file | `app_config_dir()/llm.json` | **明文** |
//! | db | `app_data_dir()/ai_config.json` | **明文** |
//! | kb | `app_data_dir()/llm_config.json` | **明文** |
//! | mind | `~/.z-mind/config.json` | 自建 AES-256-GCM + `master.key` |
//! | aigen | `~/.z-biz-tool-aigen/config.json` | 自建 XChaCha20 信封 |
//! | pet | `~/.z-bot/config.json` | 自建 AES-256-GCM + `master.key` |
//! | music | 前端 localStorage | **明文**（就在 localStorage 里） |
//! | worker | SQLite `config` 表 | **明文** |
//!
//! 后果有三层：
//!
//! 1. **用户要配十遍**。note 配好的 DeepSeek，terminal 看不到。
//! 2. **四家明文落盘**。任何能读用户目录的进程都能拿到 API Key。
//! 3. **十二种目录、四种密钥机制**。想统一都无从下手。
//!
//! 本 crate 给出**唯一落点**：
//!
//! ```text
//! 配置（非敏感）：~/.z-biz-tool/llm/config.json
//! 密钥（敏感）：  OS 密钥库 service="z-biz-tool" / account="llm.apikey"
//! ```
//!
//! `service` **不带应用后缀** —— 这是「跨应用共享」的关键：
//! macOS Keychain / Windows Credential Manager / Linux Secret Service
//! 都按 service+account 定位条目，共用 service 就等于共用同一个条目。
//!
//! # 与 cap-llm 的分工
//!
//! | | cap-llm | cap-llm-config |
//! |---|---|---|
//! | 职责 | 协议适配：怎么发请求 | 存储：配置与密钥放哪 |
//! | 密钥 | 只在内存里过一遍 | 负责落到 OS 密钥库 |
//! | 落盘 | 不落盘 | 落一份非敏感配置 |
//!
//! cap-llm 明确声明「密钥存储是各仓自己的事」（`lib.rs:32-35`）——
//! 那句话正是散开的根因。本 crate 把这句话收回来。
//!
//! # feature 隔离
//!
//! `default = []` —— 核心能力（路径、配置读写、迁移判定）零 tauri 依赖，
//! 可被 node 测试、CLI 工具、非 Tauri 宿主直接复用。

pub mod migrate;
pub mod store;
pub mod testing;

pub use migrate::{migrate_legacy_configs, write_migrated, LegacyCandidate, MigrationReport};
pub use store::{
    config_path, load_config, save_config, shared_secret_store, KeyringSecret, LlmConfigFile,
    LlmSecret, StoreError,
};

/// 家族统一的配置目录名（家目录下的隐藏目录）。
///
/// 刻意**单数** `z-biz-tool`：家族里已有 `~/.z-biz-tools`（box，带 s）
/// 与 `~/.z-biz-tool-*`（aigen/remote/worker）两套写法，再加一种只会更乱。
/// 本目录是**唯一**的新约定，老目录由 [`migrate`] 负责读旧搬新。
pub const FAMILY_DIR: &str = ".z-biz-tool";

/// 配置子目录（`~/.z-biz-tool/llm/`）。
pub const LLM_SUBDIR: &str = "llm";

/// 配置文件名（`~/.z-biz-tool/llm/config.json`）。
pub const CONFIG_FILE: &str = "config.json";

/// OS 密钥库的 service 名。**不带应用后缀** ⇒ 跨应用共享同一个条目。
pub const SECRET_SERVICE: &str = "z-biz-tool";

/// OS 密钥库的 account 名。
pub const SECRET_ACCOUNT: &str = "llm.apikey";
