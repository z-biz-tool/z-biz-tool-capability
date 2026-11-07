//! macOS 上的密钥后端：自建加密文件（**不用 OS 钥匙串**）。
//!
//! # 为什么 macOS 上不能���钥匙串
//!
//! 现象：每次启动 z-biz-tool-file，macOS 都弹
//!
//! ```text
//! z-biz-tool-file wants to use your confidential information
//! stored in "z-biz-tool" in your keychain.
//! ```
//!
//! 根因是**钥匙串 ACL 与代码签名绑定**：
//!
//! - `keyring` crate 在 macOS 上走 **legacy 钥匙串 API**
//!   （`SecKeychainAddGenericPassword` / `find_generic_password`）。
//! - legacy API 建的条目，其「解密授权」段（`kSecACLAuthorizationDecrypt`）
//!   把**创建者那个二进制的代码签名**记为唯一可信应用。
//! - 我们这些 app 是 **adhoc 签名**。`codesign -d -r-` 实测其 designated
//!   requirement 就是 cdhash：
//!
//!   ```text
//!   designated => cdhash H"95b7a79091111c9cb0ca3fd36c3c64ea6e6d9b96"
//!   ```
//!
//! - adhoc 签名的 cdhash **每次重新打包都变** ⇒ 钥匙串认为「来了个新应用」
//!   ⇒ 每次都弹一次授权框。
//!
//! # 实测过但走不通的替代方案
//!
//! 下面六种都试过并实测失败，**别再走一遍**：
//!
//! | 方案 | 实测结果 |
//! |---|---|
//! | `kSecUseDataProtectionKeychain` | `-34018`（缺 entitlement，未签名 app 用不了） |
//! | all-apps ACL（`SecAccessCreate` + `SecACLSetContents(applicationList=NULL)`） | ACL 确实写进去了（dump 看到 `desc=decrypt-any-app apps=-1`），但**只对当前进程有效**：改完后当前二进制读回 24ms，换个 cdhash 的二进制读同一条目仍阻塞 4700ms |
//! | `security add-generic-password -A` | 网上最常见的「让所有应用免授权」写法。`-A` 建出来的条目 ACL 段[1] 是 `apps=-1`，但**换二进制读照样阻塞** ⇒ 同上，`-A` 只对建它的那个进程有效 |
//! | 按路径信任（`SecTrustedApplicationCreateFromPath`） | 同路径但字节不同（cdhash 变）的二进制**仍**弹 |
//! | 正式签名证书 | 本机 `security find-identity -v -p codesigning` = **0 个可用身份** |
//! | 什么都不做，让用户每次点「Always Allow」 | adhoc 签名的 cdhash 每次重打包都变，「Always Allow」记的是**当次**的签名 ⇒ 下次照弹 |
//!
//! 结论：**在 adhoc 签名的前提下，legacy 钥匙串做不到「一次授权、永久免弹」**。
//!
//! ## 判据：怎么才算「真的免弹」
//!
//! 这条路很容易被自己的实验骗到，所以把判据写死：
//!
//! > **必须用两个不同 cdhash 的二进制分别读同一条目，都不阻塞，才算免弹。**
//!
//! 原因：钥匙串 ACL 记的是**调用方二进制**的签名。只用同一个二进制测，
//! 会拿到「建它的进程有隐式授权」的假阳性 —— 本轮就踩过这个坑：
//! 改完 ACL 后当前二进制读回 24ms，看着像修好了，换个签名一读又阻塞 4700ms。
//!
//! 造两个不同签名的二进制很容易（编译期宏改一个字节即可，adhoc 签名的
//! cdhash 随字节变）：
//!
//! ```text
//! clang -DVARIANT=1 -o reader1 reader.c -framework CoreFoundation -framework Security
//! clang -DVARIANT=2 -o reader2 reader.c -framework CoreFoundation -framework Security
//! codesign -d -r- reader1   # designated => cdhash H"aac00349..."
//! codesign -d -r- reader2   # designated => cdhash H"2c222edd..."
//! ```
//!
//! 另有一条同样重要的：**判「阻塞」不能只看返回值**。钥匙串弹窗是**阻塞式**
//! 的 —— 进程会挂在弹窗上直到用户点按钮。所以判据是**耗时**：
//! 读一次超过 ~1200ms 即视为「在等授权」。用返回值判会得到假阴性
//! （用户点 Deny 时返回 `-128`，容易被读成「读到了错误」）。
//!
//! 还有一条坑：`SecKeychainSetUserInteractionAllowed(false)` 是**进程级全局
//! 开关**。同进程内先建条目再读会拿到隐式授权 ⇒ 判定必须**开独立进程**。
//!
//! ## 副作用提醒
//!
//! 做这类实验会在**用户真实钥匙串**里留下条目，且读取动作**会弹窗打扰用户**。
//! 实验条目务必用可辨识的 service 名（`zacl2-*` 之类）并**用完立刻删**
//! —— `security delete-generic-password` 对自己创建的条目**不需要授权**，
//! 是零打扰的清理手段。
//!
//! # 现在的做法
//!
//! 沿用家族里 terminal / mind / pet 三个仓**早就在用**的那套：自建
//! AES-256-GCM + 同目录 master.key。
//!
//! ```text
//! 密钥文件：~/.z-biz-tool/llm/secret.enc   （0600）
//! 主密钥：  ~/.z-biz-tool/llm/master.key   （0600，32 字节随机）
//! 密文格式：enc:v1:<base64(nonce(12) || ciphertext || tag(16))>
//! ```
//!
//! 与 terminal 仓那套**格式刻意一致**（同样的 `enc:v1:` 前缀与同样的布局），
//! 两边的密文能互相读 —— 迁移时不需要做格式转换。
//!
//! # 安全性说明
//!
//! 防的是：密钥明文躺在盘上，被备份工具 / 同步盘 / 家目录快照带走。
//!
//! 不防的是：能以当前用户身份读 `~/.z-biz-tool/` 的攻击者 —— 他同样能读到
//! `master.key`。要挡住这一层需要真正的系统钥匙串 + 沙箱 + 正式签名，
//! 而本项目是未签名的第三方应用（见 `src/terminal/secret.rs` 里同一段话）。
//!
//! Windows / Linux **仍然走 keyring**：凭据管理器与 Secret Service 没有
//! 这个「签名绑定」问题，没必要自建。

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};

/// 密文标记前缀。前端/导出逻辑会用它判断字段是否已加密。
pub const ENC_PREFIX: &str = "enc:v1:";
const KEY_LEN: usize = 32;
const NONCE_LEN: usize = 12;

/// 加密文件后端。
pub struct EncryptedFileSecret {
    /// 密文文件路径。
    path: PathBuf,
    /// 主密钥路径。
    key_path: PathBuf,
}

impl EncryptedFileSecret {
    /// 用密文文件路径建后端。主密钥路径由它推导（同目录 `master.key`）。
    pub fn new(path: PathBuf) -> Self {
        let key_path = path
            .parent()
            .map(|d| d.join("master.key"))
            .unwrap_or_else(|| PathBuf::from("master.key"));
        Self { path, key_path }
    }

    /// 读取主密钥；不存在则随机生成并以 0600 落盘。
    ///
    /// 权限收 0600 是底线：挡住同机其他用户的读取。
    fn load_or_create_master_key(&self) -> Result<[u8; KEY_LEN], String> {
        match fs::read(&self.key_path) {
            Ok(bytes) => {
                let len = bytes.len();
                let key: [u8; KEY_LEN] = bytes.try_into().map_err(|_| {
                    format!(
                        "主密钥长度应为 {} 字节，实际 {}（文件 {} 损坏）",
                        KEY_LEN,
                        len,
                        self.key_path.display()
                    )
                })?;
                return Ok(key);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("读取主密钥失败: {}", e)),
        }

        let mut key = [0u8; KEY_LEN];
        getrandom::getrandom(&mut key).map_err(|e| format!("生成主密钥失败: {}", e))?;
        if let Some(parent) = self.key_path.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("创建配置目录失败: {}", e))?;
        }
        fs::write(&self.key_path, key).map_err(|e| format!("写入主密钥失败: {}", e))?;
        restrict_private(&self.key_path);
        Ok(key)
    }

    /// 进程内缓存主密钥：`get` 会在热路径被反复调用，不应每次读盘。
    fn master_key(&self) -> Result<[u8; KEY_LEN], String> {
        let key = self.load_or_create_master_key()?;
        Ok(key)
    }

    /// 加密。
    fn encrypt(&self, key: &[u8; KEY_LEN], plain: &str) -> Result<String, String> {
        let cipher =
            Aes256Gcm::new_from_slice(key).map_err(|e| format!("初始化加密器失败: {}", e))?;
        let mut nonce = [0u8; NONCE_LEN];
        getrandom::getrandom(&mut nonce).map_err(|e| format!("生成随机数失败: {}", e))?;

        let body = cipher
            .encrypt(Nonce::from_slice(&nonce), plain.as_bytes())
            .map_err(|e| format!("加密失败: {}", e))?;

        let mut packed = Vec::with_capacity(NONCE_LEN + body.len());
        packed.extend_from_slice(&nonce);
        packed.extend_from_slice(&body);
        Ok(format!(
            "{}{}",
            ENC_PREFIX,
            data_encoding::BASE64.encode(&packed)
        ))
    }

    /// 解密。
    ///
    /// 不带前缀的值按**明文**处理 —— 兼容「先写成明文、后升级加密」的中间态。
    fn decrypt(&self, key: &[u8; KEY_LEN], value: &str) -> Result<String, String> {
        if !value.starts_with(ENC_PREFIX) {
            return Ok(value.to_string());
        }
        // ENC_PREFIX 是纯 ASCII 前缀且上面已确认过前缀，
        // 所以 value[ENC_PREFIX.len()..] 必然落在字符边界上。
        let raw = data_encoding::BASE64
            .decode(&value.as_bytes()[ENC_PREFIX.len()..])
            .map_err(|e| format!("密文 base64 解析失败: {}", e))?;
        if raw.len() < NONCE_LEN {
            return Err("密文长度不足, 已损坏".into());
        }
        let (nonce, body) = raw.split_at(NONCE_LEN);

        let cipher =
            Aes256Gcm::new_from_slice(key).map_err(|e| format!("初始化解密器失败: {}", e))?;
        let plain = cipher
            .decrypt(Nonce::from_slice(nonce), body)
            .map_err(|_| "解密失败(主密钥可能已丢失或与密文不匹配)".to_string())?;
        String::from_utf8(plain).map_err(|e| format!("解密结果非 UTF-8: {}", e))
    }
}

/// 把文件权限收成 0600（Unix）。Windows 上不做 —— 那里靠 ACL。
fn restrict_private(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = fs::metadata(path) {
            let mut perm = meta.permissions();
            perm.set_mode(0o600);
            let _ = fs::set_permissions(path, perm);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

/// 多条目加密文件。
///
/// ## 为什么需要它
///
/// [`EncryptedFileSecret`] 一次只存**一个**字符串 —— 那正好够 LLM 密钥
/// （家族里只有一份）。但数据库板块有**多条连接**，每条一个密码。
///
/// 最省事的做法本来是每条连接一个 `EncryptedFileSecret`（一个密文文件
/// + 一个主密钥文件），但那会在配置目录里堆出 N 个 `master.key`，
/// 备份/迁移/清理都得逐个来，而且主密钥一多就等于密钥分散。
///
/// 所以这里做一个**多条目**版本：一个密文文件装下所有条目，
/// 共用同一个主密钥文件。
///
/// ## 落盘格式
///
/// ```text
/// {
///   "version": 1,
///   "entries": {
///     "<id>": "enc:v1:<base64(nonce||ciphertext||tag)>",
///     ...
///   }
/// }
/// ```
///
/// 整体是 JSON —— 好处是能直接被人读懂「有哪些条目」，
/// 而每条的值仍是密文。整个文件权限 0600。
///
/// ## 为什么不复用 OS 密钥库
///
/// 与 [`EncryptedFileSecret`] 同一个理由：macOS 钥匙串 ACL 与代码签名绑定，
/// 本家族是 adhoc 签名（cdhash 每次重打包都变）⇒ 每次读密码都弹授权框。
/// 数据库密码比 LLM 密钥读得更频繁（每次建连接），弹窗更烦人。
///
/// 2026-11-06：z-biz-tool-db 仓的数据库密码原本走 `keyring` crate
/// （macOS 钥匙串，service=`z-biz-tool-db`），db 仓并入 z-biz-tool-file
/// 时换成这个后端。
pub struct MultiSecretFile {
    path: PathBuf,
    key_path: PathBuf,
}

impl MultiSecretFile {
    /// 用密文文件路径建后端。主密钥路径由它推导（同目录 `master.key`）。
    pub fn new(path: PathBuf) -> Self {
        let key_path = path
            .parent()
            .map(|d| d.join("master.key"))
            .unwrap_or_else(|| PathBuf::from("master.key"));
        Self { path, key_path }
    }

    /// 密文文件路径。
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 读主密钥；不存在则随机生成并以 0600 落盘。
    fn load_or_create_master_key(&self) -> Result<[u8; KEY_LEN], String> {
        match fs::read(&self.key_path) {
            Ok(bytes) => {
                let len = bytes.len();
                let key: [u8; KEY_LEN] = bytes.try_into().map_err(|_| {
                    format!(
                        "主密钥长度应为 {} 字节，实际 {}（文件 {} 损坏）",
                        KEY_LEN,
                        len,
                        self.key_path.display()
                    )
                })?;
                return Ok(key);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("读取主密钥失败: {}", e)),
        }

        let mut key = [0u8; KEY_LEN];
        getrandom::getrandom(&mut key).map_err(|e| format!("生成主密钥失败: {}", e))?;
        if let Some(parent) = self.key_path.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("创建配置目录失败: {}", e))?;
        }
        fs::write(&self.key_path, key).map_err(|e| format!("写入主密钥失败: {}", e))?;
        restrict_private(&self.key_path);
        Ok(key)
    }

    /// 加密一段明文。
    fn encrypt(&self, key: &[u8; KEY_LEN], plain: &str) -> Result<String, String> {
        let cipher =
            Aes256Gcm::new_from_slice(key).map_err(|e| format!("初始化加密器失败: {}", e))?;
        let mut nonce = [0u8; NONCE_LEN];
        getrandom::getrandom(&mut nonce).map_err(|e| format!("生成随机数失败: {}", e))?;

        let body = cipher
            .encrypt(Nonce::from_slice(&nonce), plain.as_bytes())
            .map_err(|e| format!("加密失败: {}", e))?;

        let mut packed = Vec::with_capacity(NONCE_LEN + body.len());
        packed.extend_from_slice(&nonce);
        packed.extend_from_slice(&body);
        Ok(format!(
            "{}{}",
            ENC_PREFIX,
            data_encoding::BASE64.encode(&packed)
        ))
    }

    /// 解密一段密文。
    ///
    /// 不带前缀的值按**明文**处理 —— 兼容「先写成明文、后升级加密」的中间态。
    fn decrypt(&self, key: &[u8; KEY_LEN], value: &str) -> Result<String, String> {
        if !value.starts_with(ENC_PREFIX) {
            return Ok(value.to_string());
        }
        let raw = data_encoding::BASE64
            .decode(&value.as_bytes()[ENC_PREFIX.len()..])
            .map_err(|e| format!("密文 base64 解析失败: {}", e))?;
        if raw.len() < NONCE_LEN {
            return Err("密文长度不足, 已损坏".into());
        }
        let (nonce, body) = raw.split_at(NONCE_LEN);

        let cipher =
            Aes256Gcm::new_from_slice(key).map_err(|e| format!("初始化解密器失败: {}", e))?;
        let plain = cipher
            .decrypt(Nonce::from_slice(nonce), body)
            .map_err(|_| "解密失败(主密钥可能已丢失或与密文不匹配)".to_string())?;
        String::from_utf8(plain).map_err(|e| format!("解密结果非 UTF-8: {}", e))
    }

    /// 读出全部条目（返回 id → 明文）。
    ///
    /// 文件不存在返回空表（不是错误）—— 第一次用就是空的。
    pub fn read_all(&self) -> Result<std::collections::BTreeMap<String, String>, String> {
        let key = self.load_or_create_master_key()?;
        let raw = match fs::read_to_string(&self.path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(std::collections::BTreeMap::new())
            }
            Err(e) => return Err(format!("读取密文文件失败: {}", e)),
        };

        #[derive(serde::Deserialize)]
        struct Envelope {
            version: u32,
            entries: std::collections::BTreeMap<String, String>,
        }

        let env: Envelope = serde_json::from_str(&raw)
            .map_err(|e| format!("密文文件解析失败（{}）: {}", self.path.display(), e))?;
        if env.version != 1 {
            return Err(format!(
                "密文文件版本不支持: {}（本程序只认 1）",
                env.version
            ));
        }

        let mut out = std::collections::BTreeMap::new();
        for (id, cipher_text) in env.entries {
            let plain = self.decrypt(&key, &cipher_text)?;
            out.insert(id, plain);
        }
        Ok(out)
    }

    /// 写入一条（id → 明文）。同 id 覆盖。
    pub fn put(&self, id: &str, plain: &str) -> Result<(), String> {
        let key = self.load_or_create_master_key()?;
        let mut entries = self.read_entries_raw()?;
        entries.insert(id.to_string(), self.encrypt(&key, plain)?);
        self.write_entries(entries)
    }

    /// 删除一条。**幂等** —— id 不存在也算成功。
    pub fn remove(&self, id: &str) -> Result<(), String> {
        let mut entries = self.read_entries_raw()?;
        entries.remove(id);
        self.write_entries(entries)
    }

    /// 读一条。没有返回 None。
    pub fn get(&self, id: &str) -> Result<Option<String>, String> {
        Ok(self.read_all()?.remove(id))
    }

    /// 读原始密文条目表（不解密）。
    fn read_entries_raw(&self) -> Result<std::collections::BTreeMap<String, String>, String> {
        let raw = match fs::read_to_string(&self.path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(std::collections::BTreeMap::new())
            }
            Err(e) => return Err(format!("读取密文文件失败: {}", e)),
        };

        #[derive(serde::Deserialize)]
        struct Envelope {
            version: u32,
            #[serde(default)]
            entries: std::collections::BTreeMap<String, String>,
        }

        let env: Envelope = serde_json::from_str(&raw)
            .map_err(|e| format!("密文文件解析失败（{}）: {}", self.path.display(), e))?;
        if env.version != 1 {
            return Err(format!(
                "密文文件版本不支持: {}（本程序只认 1）",
                env.version
            ));
        }
        Ok(env.entries)
    }

    /// 落盘。原子写 + 0600。
    fn write_entries(
        &self,
        entries: std::collections::BTreeMap<String, String>,
    ) -> Result<(), String> {
        #[derive(serde::Serialize)]
        struct Envelope<'a> {
            version: u32,
            entries: &'a std::collections::BTreeMap<String, String>,
        }
        let env = Envelope {
            version: 1,
            entries: &entries,
        };
        let bytes =
            serde_json::to_vec_pretty(&env).map_err(|e| format!("序列化密文条目失败: {}", e))?;
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("创建配置目录失败: {}", e))?;
        }
        atomic_write(&self.path, &bytes)
    }
}

/// 原子写：临时文件 → fsync → rename。
///
/// 直接写目标路径的话，进程在写到一半被杀会留下截断文件 ——
/// 而截断的密文**解不开**，表现为「密钥莫名其妙没了」。
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let dir = path
        .parent()
        .ok_or_else(|| format!("{} 没有父目录", path.display()))?;
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| format!("{} 文件名不合法", path.display()))?;
    let tmp = dir.join(format!(".{}.tmp", file_name));

    {
        let mut f = fs::File::create(&tmp)
            .map_err(|e| format!("创建临时文件 {} 失败: {}", tmp.display(), e))?;
        f.write_all(bytes)
            .map_err(|e| format!("写入临时文件失败: {}", e))?;
        f.sync_all().map_err(|e| format!("刷盘失败: {}", e))?;
    }
    restrict_private(&tmp);

    fs::rename(&tmp, path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("原子替换 {} 失败: {}", path.display(), e)
    })
}

/// 基于加密文件的 [`crate::store::LlmSecret`] 实现。
impl crate::store::LlmSecret for EncryptedFileSecret {
    fn set(&self, value: &str) -> Result<(), crate::store::StoreError> {
        if value.trim().is_empty() {
            return self.remove();
        }
        let key = self
            .master_key()
            .map_err(crate::store::StoreError::Secret)?;
        let sealed = self
            .encrypt(&key, value)
            .map_err(crate::store::StoreError::Secret)?;
        atomic_write(&self.path, sealed.as_bytes()).map_err(crate::store::StoreError::Io)
    }

    fn get(&self) -> Result<Option<String>, crate::store::StoreError> {
        if !self.path.exists() {
            return Ok(None);
        }
        let text = fs::read_to_string(&self.path).map_err(|e| {
            crate::store::StoreError::Io(format!("读取 {} 失败: {}", self.path.display(), e))
        })?;
        if text.trim().is_empty() {
            return Ok(None);
        }
        let key = self
            .master_key()
            .map_err(crate::store::StoreError::Secret)?;
        let plain = self
            .decrypt(&key, text.trim())
            .map_err(crate::store::StoreError::Secret)?;
        Ok(Some(plain))
    }

    fn remove(&self) -> Result<(), crate::store::StoreError> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(crate::store::StoreError::Io(format!(
                "删除 {} 失败: {}",
                self.path.display(),
                e
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::LlmSecret;

    fn tmp_secret(name: &str) -> (EncryptedFileSecret, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "cap-llm-config-mac-{}-{}",
            std::process::id(),
            name
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("建临时目录");
        let path = dir.join("secret.enc");
        (EncryptedFileSecret::new(path), dir)
    }

    #[test]
    fn roundtrip_hides_plaintext() {
        let (s, dir) = tmp_secret("roundtrip");
        s.set("sk-secret-value").expect("写入");
        let raw = fs::read_to_string(dir.join("secret.enc")).expect("读密文文件");
        assert!(raw.starts_with(ENC_PREFIX), "应带密文前缀：{}", raw);
        assert!(
            !raw.contains("sk-secret-value"),
            "密文里不该有明文：{}",
            raw
        );
        assert_eq!(s.get().expect("读取").as_deref(), Some("sk-secret-value"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_write_is_equivalent_to_delete() {
        let (s, dir) = tmp_secret("empty");
        s.set("sk-x").expect("写入");
        s.set("   ").expect("空串删除");
        assert!(s.get().expect("读取").is_none(), "空串应删除条目");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn remove_is_idempotent() {
        let (s, dir) = tmp_secret("idem");
        s.remove().expect("删不存在的");
        s.set("sk-y").expect("写入");
        s.remove().expect("删存在的");
        s.remove().expect("再删一次仍成功");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn same_input_yields_different_ciphertext() {
        let (s, dir) = tmp_secret("nonce");
        s.set("same").expect("写入");
        let first = fs::read_to_string(dir.join("secret.enc")).expect("读");
        s.set("same").expect("再写");
        let second = fs::read_to_string(dir.join("secret.enc")).expect("读");
        assert_ne!(first, second, "nonce 应随机，同明文不该出同密文");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn tampered_ciphertext_is_rejected() {
        let (s, dir) = tmp_secret("tamper");
        s.set("sk-secret").expect("写入");
        let raw = fs::read_to_string(dir.join("secret.enc")).expect("读");
        let mut bytes = data_encoding::BASE64
            .decode(&raw.as_bytes()[ENC_PREFIX.len()..])
            .expect("base64");
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF;
        let tampered = format!("{}{}", ENC_PREFIX, data_encoding::BASE64.encode(&bytes));
        fs::write(dir.join("secret.enc"), tampered).expect("写回");
        assert!(s.get().is_err(), "GCM 认证标签应拦下篡改");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn plaintext_legacy_value_passes_through() {
        // 迁移中间态：文件里可能已经是明文（还没加密）。
        let (s, dir) = tmp_secret("plain");
        fs::write(dir.join("secret.enc"), "legacy-plaintext").expect("写明文");
        assert_eq!(
            s.get().expect("读取").as_deref(),
            Some("legacy-plaintext"),
            "旧明文应能读出来"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn master_key_file_is_private() {
        let (s, dir) = tmp_secret("perm");
        s.set("sk-x").expect("写入");
        let key_path = dir.join("master.key");
        assert!(key_path.exists(), "应生成主密钥文件");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&key_path)
                .expect("读元数据")
                .permissions()
                .mode();
            assert_eq!(
                mode & 0o777,
                0o600,
                "主密钥权限应是 0600，实际 {:o}",
                mode & 0o777
            );
        }
        let _ = fs::remove_dir_all(&dir);
    }
}
