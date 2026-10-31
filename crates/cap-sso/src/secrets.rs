//! OS 密钥库安全存储 —— SSO 的 token 落盘处。
//!
//! # 为什么 token 不能进 localStorage
//!
//! 桌面应用的 localStorage 就是一个明文 JSON 文件：任何能读用户目录的进程
//! 都能拿到 access_token 与 refresh_token。token 等价于「已登录」的凭据，
//! 拿到它就能以用户身份访问云端数据。
//!
//! 所以本模块用 OS 密钥库：
//! - macOS → Keychain
//! - Windows → Credential Manager
//! - Linux → Secret Service（GNOME Keyring / KWallet）
//!
//! 与 db 仓用的是同一个 `keyring` crate（4.2.0），跨平台行为一致。
//!
//! # 命名规范
//!
//! service = 应用名（如 `z-biz-tool-remote`），account = 用途标识
//! （如 `sso.tokens`）。**一个应用一个 service**，避免互相读到对方的条目。

use keyring::Entry;

/// 密钥库不可用时的错误。
#[derive(Debug)]
pub enum SecretError {
    /// 密钥库后端未就绪（Linux 上常见：没有 Secret Service 守护进程）。
    Unavailable(String),
    /// 读 / 写 / 删失败。
    Io(String),
    /// 要读的条目不存在。
    NotFound,
}

impl std::fmt::Display for SecretError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SecretError::Unavailable(e) => write!(f, "OS 密钥库不可用：{}", e),
            SecretError::Io(e) => write!(f, "密钥库操作失败：{}", e),
            SecretError::NotFound => write!(f, "密钥库中没有这个条目"),
        }
    }
}

impl std::error::Error for SecretError {}

/// 基于 OS 密钥库的安全存储。
pub struct SecureStore {
    service: String,
}

impl SecureStore {
    /// 用指定 service 建后端。
    pub fn new(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
        }
    }

    /// 当前平台的后端名（给日志与诊断用）。
    pub fn backend_name() -> &'static str {
        if cfg!(target_os = "macos") {
            "keychain-macos"
        } else if cfg!(target_os = "windows") {
            "keychain-windows"
        } else {
            "secret-service"
        }
    }

    /// 探测密钥库是否真的可用。
    ///
    /// 探测读的是一个**不存在**的条目 —— 不创建、不弹窗、不碰真实数据。
    pub fn check_available(&self) -> Result<(), SecretError> {
        if let Err(e) = Entry::store_status() {
            return Err(SecretError::Unavailable(e.to_string()));
        }
        let probe = self
            .entry("__availability_probe__")
            .map_err(SecretError::Io)?;
        match probe.get_password() {
            Ok(_) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(SecretError::Unavailable(e.to_string())),
        }
    }

    fn entry(&self, account: &str) -> Result<Entry, String> {
        Entry::new(&self.service, account)
            .map_err(|e| format!("创建密钥库条目失败（service={}）：{}", self.service, e))
    }

    /// 写入一个值。
    pub fn set(&self, account: &str, value: &str) -> Result<(), SecretError> {
        let entry = self.entry(account).map_err(SecretError::Io)?;
        // 用 `set_password` 而不是 `set_secret`：SSO 存的是 token（UTF-8 字符串），
        // 走字符串路径省去自己处理字节。密钥库在底层存的仍是原始字节，
        // 两者的持久化结果一致。
        entry
            .set_password(value)
            .map_err(|e| SecretError::Io(e.to_string()))
    }

    /// 读取一个值。条目不存在返回 `NotFound`。
    pub fn get(&self, account: &str) -> Result<String, SecretError> {
        let entry = self.entry(account).map_err(SecretError::Io)?;
        match entry.get_password() {
            Ok(v) => Ok(v),
            Err(keyring::Error::NoEntry) => Err(SecretError::NotFound),
            Err(e) => Err(SecretError::Io(e.to_string())),
        }
    }

    /// 删除一个值。**幂等** —— 条目不存在也算成功。
    pub fn delete(&self, account: &str) -> Result<(), SecretError> {
        let entry = self.entry(account).map_err(SecretError::Io)?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(SecretError::Io(e.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 密钥库不可用时优雅跳过：打印 skip 原因，**不算失败**。
    ///
    /// CI 的 Linux runner 常常没有 Secret Service 守护进程 —— 那时这条用例
    /// 应当跳过而不是红。这不是「掩盖问题」：真正的判据是「可用时必须对」，
    /// 由 `check_available` 的真假两条分支共同保证。
    fn store_or_skip() -> Option<SecureStore> {
        let store = SecureStore::new("z-biz-tool-cap-sso-test");
        match store.check_available() {
            Ok(()) => Some(store),
            Err(e) => {
                eprintln!("[skip] OS 密钥库不可用，跳过实测：{}", e);
                None
            }
        }
    }

    #[test]
    fn 写读删的往返() {
        let Some(store) = store_or_skip() else {
            return;
        };
        store.set("roundtrip", "hello-令牌").expect("set");
        assert_eq!(store.get("roundtrip").expect("get"), "hello-令牌");
        store.delete("roundtrip").expect("delete");
        assert!(matches!(store.get("roundtrip"), Err(SecretError::NotFound)));
    }

    #[test]
    fn 删除不存在的条目是幂等的() {
        let Some(store) = store_or_skip() else {
            return;
        };
        // 删两次都不该报错 —— 否则「退出登录」在重复点击时会失败。
        store.delete("never-existed").expect("第一次删");
        store.delete("never-existed").expect("第二次删");
    }

    #[test]
    fn 读不存在的条目报未找到而不是其它错() {
        let Some(store) = store_or_skip() else {
            return;
        };
        let err = store.get("never-existed").expect_err("应报未找到");
        assert!(matches!(err, SecretError::NotFound), "实际错误：{:?}", err);
    }

    #[test]
    fn 后端名与平台对应() {
        let name = SecureStore::backend_name();
        if cfg!(target_os = "macos") {
            assert_eq!(name, "keychain-macos");
        } else if cfg!(target_os = "windows") {
            assert_eq!(name, "keychain-windows");
        } else {
            assert_eq!(name, "secret-service");
        }
    }
}
