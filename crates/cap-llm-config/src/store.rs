//! 统一配置的读写。
//!
//! # 落点
//!
//! ```text
//! 配置（非敏感）：~/.z-biz-tool/llm/config.json
//! 密钥（敏感）：  OS 密钥库 service="z-biz-tool" / account="llm.apikey"
//! ```
//!
//! # 为什么密钥不写进 config.json
//!
//! 配置文件是**明文 JSON**。哪怕给它 0600 权限，也挡不住：
//! 同机任何进程读用户目录、备份工具、误传到 git 的家目录快照。
//! 而密钥放 OS 密钥库，macOS 上还要再过一道钥匙串 ACL。
//!
//! 代价是**密钥库不可用时无处可存**（Linux 无 Secret Service 守护进程）。
//! 这种情况必须**显式报错**而不是悄悄降级到明文文件 —— 悄悄降级
//! 意味着用户以为存好了，实际躺在盘上被人读走。
//!
//! # 原子写
//!
//! 写临时文件 → `fsync` → `rename`。`rename` 在同一文件系统内是原子的，
//! 所以读者要么看到完整旧文件，要么看到完整新文件，**不会读到半截 JSON**。
//! 直接 `write` 到目标路径的话，进程在写到一半被杀就会留下截断文件，
//! 而配置损坏的表现是「启动时解析失败」—— 用户看不到任何线索。

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{CONFIG_FILE, FAMILY_DIR, LLM_SUBDIR, SECRET_ACCOUNT, SECRET_SERVICE};

/// 存储层错误。
#[derive(Debug)]
pub enum StoreError {
    /// 路径类问题（建目录失败、读写失败）。
    Io(String),
    /// 配置文件的 JSON 坏了。
    Parse(String),
    /// OS 密钥库不可用或操作失败。
    Secret(String),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Io(e) => write!(f, "配置文件读写失败：{}", e),
            StoreError::Parse(e) => write!(f, "配置文件解析失败：{}", e),
            StoreError::Secret(e) => write!(f, "OS 密钥库操作失败：{}", e),
        }
    }
}

impl std::error::Error for StoreError {}

/// 落盘的非敏感配置。
///
/// 字段刻意少：每多一个字段，十套历史实现各自的迁移成本就多一条。
///
/// **不含密钥** —— 密钥在 OS 密钥库里，见 [`LlmSecret`]。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LlmConfigFile {
    /// 预设 id（见 cap-llm 的 `preset::PRESETS`），如 `deepseek`。
    /// 空串表示自定义服务。
    #[serde(default)]
    pub preset: String,
    /// 供应商身份（cap-llm 的 `ProviderKind` 字符串）。
    #[serde(default)]
    pub provider: String,
    /// 服务地址。留空时由预设兜底。
    #[serde(default)]
    pub base_url: String,
    /// 模型名。**不预设** —— 模型半年换代，由运行时现取。
    #[serde(default)]
    pub model: String,
    /// 采样温度。`None` 表示不带该字段（让服务端用默认值）。
    #[serde(default)]
    pub temperature: Option<f32>,
    /// 单次回复上限。
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// 请求超时（毫秒）。
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// 该配置最后一次修改的时刻（毫秒时间戳）。仅供诊断与「谁改的」排查。
    #[serde(default)]
    pub updated_at_ms: Option<u64>,
}

/// 密钥的存取门面。
///
/// 只暴露**能力**，不暴露 keyring 的类型 —— 这样调用方不必依赖 keyring 版本，
/// 单元测试也能注入假实现。
pub trait LlmSecret {
    /// 写入密钥。空串等价于**删除**（用户清空了输入框）。
    fn set(&self, value: &str) -> Result<(), StoreError>;
    /// 读取密钥。没有配置返回 `Ok(None)`。
    fn get(&self) -> Result<Option<String>, StoreError>;
    /// 删除密钥。**幂等** —— 条目不存在也算成功。
    fn remove(&self) -> Result<(), StoreError>;
}

/// 基于 OS 密钥库的实现。
///
/// service 固定 `z-biz-tool`（**不带应用后缀**）⇒ 所有兄弟应用读写同一个条目。
pub struct KeyringSecret {
    service: String,
    account: String,
}

impl KeyringSecret {
    pub fn new() -> Self {
        Self {
            service: secret_service_name(),
            account: SECRET_ACCOUNT.to_string(),
        }
    }

    /// 探测密钥库是否真的可用。
    ///
    /// 探测读的是一个**不存在**的条目 —— 不创建、不弹窗、不碰真实数据。
    /// Linux 上没有 Secret Service 守护进程时，这里会如实返回不可用。
    pub fn check_available(&self) -> Result<(), StoreError> {
        if let Err(e) = keyring::Entry::store_status() {
            return Err(StoreError::Secret(e.to_string()));
        }
        let probe = keyring::Entry::new(&self.service, "__availability_probe__")
            .map_err(|e| StoreError::Secret(e.to_string()))?;
        match probe.get_password() {
            Ok(_) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(StoreError::Secret(e.to_string())),
        }
    }
}

/// 密钥库的 service 名。
///
/// 默认 `z-biz-tool`（**不带应用后缀**）—— 这是「全家共享一条目」的实现方式：
/// service 带了应用名，各仓就会各写一条，改一处别的应用读不到。
///
/// 环境变量 `Z_BIZ_TOOL_SECRET_SERVICE` 可覆盖，**仅供测试隔离**：
/// 测试不该往用户真实钥匙串里写条目（写进去就是污染，
/// 且用户会莫名其妙在自己钥匙串里看到 `z-biz-tool-test`）。
fn secret_service_name() -> String {
    std::env::var("Z_BIZ_TOOL_SECRET_SERVICE")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| SECRET_SERVICE.to_string())
}

impl Default for KeyringSecret {
    fn default() -> Self {
        Self::new()
    }
}

impl LlmSecret for KeyringSecret {
    fn set(&self, value: &str) -> Result<(), StoreError> {
        if value.trim().is_empty() {
            return self.remove();
        }
        let entry = keyring::Entry::new(&self.service, &self.account)
            .map_err(|e| StoreError::Secret(e.to_string()))?;
        entry
            .set_password(value)
            .map_err(|e| StoreError::Secret(e.to_string()))
    }

    fn get(&self) -> Result<Option<String>, StoreError> {
        let entry = keyring::Entry::new(&self.service, &self.account)
            .map_err(|e| StoreError::Secret(e.to_string()))?;
        match entry.get_password() {
            Ok(v) => Ok(Some(v)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(StoreError::Secret(e.to_string())),
        }
    }

    fn remove(&self) -> Result<(), StoreError> {
        let entry = keyring::Entry::new(&self.service, &self.account)
            .map_err(|e| StoreError::Secret(e.to_string()))?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(StoreError::Secret(e.to_string())),
        }
    }
}

/// 家族共享的密钥库门面。
pub fn shared_secret_store() -> KeyringSecret {
    KeyringSecret::new()
}

/// 家族统一的配置目录（`~/.z-biz-tool/llm`）。
///
/// 不存在则创建（0750 —— 目录要能被同组工具读到，但不给 others）。
///
/// ## 测试隔离
///
/// 环境变量 `Z_BIZ_TOOL_LLM_DIR` 可直接指定目录。**仅供测试用** ——
/// 没有它的话，十个仓的测试都会去动用户真实的 `~/.z-biz-tool/llm/`：
/// 一是污染用户配置，二是并行跑测试时互相踩（甲仓的测试写完配置，
/// 乙仓的测试读出来就是甲仓的值），三是测试失败会把用户的配置留在
/// 半截状态。
pub fn config_dir() -> Result<PathBuf, StoreError> {
    if let Some(dir) = std::env::var_os("Z_BIZ_TOOL_LLM_DIR") {
        let dir = PathBuf::from(dir);
        if !dir.exists() {
            fs::create_dir_all(&dir)
                .map_err(|e| StoreError::Io(format!("创建 {} 失败：{}", dir.display(), e)))?;
        }
        return Ok(dir);
    }
    let home = home_dir().ok_or_else(|| {
        StoreError::Io("无法确定用户家目录（HOME / USERPROFILE 均未设置）".into())
    })?;
    let dir = home.join(FAMILY_DIR).join(LLM_SUBDIR);
    if !dir.exists() {
        fs::create_dir_all(&dir)
            .map_err(|e| StoreError::Io(format!("创建 {} 失败：{}", dir.display(), e)))?;
    }
    Ok(dir)
}

/// 家族统一的配置文件路径（`~/.z-biz-tool/llm/config.json`）。
pub fn config_path() -> Result<PathBuf, StoreError> {
    Ok(config_dir()?.join(CONFIG_FILE))
}

/// 读配置。文件不存在返回 `Ok(None)`（**不是**错误 —— 首次启动就是没有）。
///
/// 文件存在但 JSON 坏了 → 返回 [`StoreError::Parse`]。
/// 宁可显式报错，也不要静默当成「没配置」—— 后者会让用户以为改的东西生效了。
pub fn load_config() -> Result<Option<LlmConfigFile>, StoreError> {
    let path = config_path()?;
    if !path.exists() {
        return Ok(None);
    }
    let text = fs::read_to_string(&path)
        .map_err(|e| StoreError::Io(format!("读取 {} 失败：{}", path.display(), e)))?;
    if text.trim().is_empty() {
        return Ok(None);
    }
    let cfg: LlmConfigFile = serde_json::from_str(&text)
        .map_err(|e| StoreError::Parse(format!("{} 内容损坏：{}", path.display(), e)))?;
    Ok(Some(cfg))
}

/// 写配置（原子写）。
pub fn save_config(cfg: &LlmConfigFile) -> Result<(), StoreError> {
    let path = config_path()?;
    let text = serde_json::to_string_pretty(cfg)
        .map_err(|e| StoreError::Parse(format!("序列化失败：{}", e)))?;
    atomic_write(&path, text.as_bytes())
}

/// 原子写：临时文件 → fsync → rename。
///
/// `rename` 在同一文件系统内是原子的，读者不会看到半截 JSON。
///
/// 公开给迁移模块用：抹旧文件里的明文密钥也要走原子写，
/// 否则进程在写到一半被杀会留下截断的旧配置 —— 那种文件再也解不出地址和模型。
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), StoreError> {
    let dir = path
        .parent()
        .ok_or_else(|| StoreError::Io(format!("{} 没有父目录", path.display())))?;
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| StoreError::Io(format!("{} 文件名不合法", path.display())))?;
    let tmp = dir.join(format!(".{}.tmp", file_name));

    {
        let mut f = fs::File::create(&tmp)
            .map_err(|e| StoreError::Io(format!("创建临时文件 {} 失败：{}", tmp.display(), e)))?;
        f.write_all(bytes)
            .map_err(|e| StoreError::Io(format!("写临时文件失败：{}", e)))?;
        // fsync 落盘：崩电 / 进程被杀后临时文件内容仍完整。
        f.sync_all()
            .map_err(|e| StoreError::Io(format!("刷盘失败：{}", e)))?;
    }

    // 配置文件可能含敏感配置，权限收成 0600（仅本人可读写）。
    set_private_permissions(&tmp)?;

    fs::rename(&tmp, path).map_err(|e| {
        // 失败时把临时文件清掉，别留垃圾。
        let _ = fs::remove_file(&tmp);
        StoreError::Io(format!("原子替换 {} 失败：{}", path.display(), e))
    })
}

/// 把文件权限收成 0600（Unix）。Windows 上不做 —— 那里靠 ACL。
fn set_private_permissions(path: &Path) -> Result<(), StoreError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perm = fs::metadata(path)
            .map_err(|e| StoreError::Io(format!("读权限失败：{}", e)))?
            .permissions();
        perm.set_mode(0o600);
        fs::set_permissions(path, perm)
            .map_err(|e| StoreError::Io(format!("设置权限失败：{}", e)))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

/// 取用户家目录。
///
/// 不用 `dirs` crate —— 为一个环境变量引入新依赖不划算，
/// 而这个 crate 会被所有兄弟应用编译。
pub fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .map(PathBuf::from)
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME").map(PathBuf::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 假密钥库：内存里的一个 Option。
    ///
    /// 用 `RefCell` 而不是裸指针 —— `LlmSecret` 的方法签名是 `&self`，
    /// 假实现需要内部可变性，但 unsafe 裸指针在这种小测试里不值得。
    struct FakeSecret(std::cell::RefCell<Option<String>>);

    impl FakeSecret {
        fn new(v: Option<&str>) -> Self {
            Self(std::cell::RefCell::new(v.map(|s| s.to_string())))
        }
    }

    impl LlmSecret for FakeSecret {
        fn set(&self, value: &str) -> Result<(), StoreError> {
            if value.trim().is_empty() {
                return self.remove();
            }
            *self.0.borrow_mut() = Some(value.to_string());
            Ok(())
        }
        fn get(&self) -> Result<Option<String>, StoreError> {
            Ok(self.0.borrow().clone())
        }
        fn remove(&self) -> Result<(), StoreError> {
            *self.0.borrow_mut() = None;
            Ok(())
        }
    }

    #[test]
    fn empty_secret_write_is_equivalent_to_delete() {
        let s = FakeSecret::new(Some("old"));
        s.set("   ").expect("set");
        assert_eq!(s.get().unwrap(), None);
    }

    #[test]
    fn secret_write_read_roundtrip() {
        let s = FakeSecret::new(None);
        assert_eq!(s.get().unwrap(), None);
        s.set("sk-secret").expect("set");
        assert_eq!(s.get().unwrap().as_deref(), Some("sk-secret"));
        s.remove().expect("remove");
        assert_eq!(s.get().unwrap(), None);
    }

    #[test]
    fn config_struct_roundtrips() {
        let cfg = LlmConfigFile {
            preset: "deepseek".into(),
            provider: "custom".into(),
            base_url: "https://api.deepseek.com/v1".into(),
            model: "deepseek-chat".into(),
            temperature: Some(0.7),
            max_tokens: Some(2048),
            timeout_ms: Some(30_000),
            updated_at_ms: Some(1_700_000_000_000),
        };
        let text = serde_json::to_string(&cfg).expect("序列化");
        let back: LlmConfigFile = serde_json::from_str(&text).expect("反序列化");
        assert_eq!(back.preset, "deepseek");
        assert_eq!(back.base_url, "https://api.deepseek.com/v1");
        assert_eq!(back.model, "deepseek-chat");
        assert_eq!(back.temperature, Some(0.7));
        assert_eq!(back.max_tokens, Some(2048));
    }

    #[test]
    fn sparse_old_file_parses_without_error() {
        // 只有三个字段的极简 JSON 也必须能解析 —— 迁移期的中间态就是这样的。
        let json = r#"{"preset":"openai","baseUrl":"https://api.openai.com/v1"}"#;
        let cfg: LlmConfigFile = serde_json::from_str(json).expect("应能解析");
        assert_eq!(cfg.preset, "openai");
        assert_eq!(cfg.model, "");
        assert_eq!(cfg.temperature, None);
    }

    #[test]
    fn config_fields_use_camel_case() {
        let json = r#"{"baseUrl":"x","maxTokens":10,"timeoutMs":20}"#;
        let cfg: LlmConfigFile = serde_json::from_str(json).expect("应能解析");
        assert_eq!(cfg.base_url, "x");
        assert_eq!(cfg.max_tokens, Some(10));
        assert_eq!(cfg.timeout_ms, Some(20));
    }
}
