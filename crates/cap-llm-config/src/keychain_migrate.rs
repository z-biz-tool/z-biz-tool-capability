//! macOS 存量迁移：把 OS 钥匙串里的旧密钥搬进加密文件。
//!
//! # 为什么需要这一步
//!
//! 2026-11-05 之前，`cap-llm-config` 在 macOS 上用 OS 钥匙串存 LLM 密钥。
//! 钥匙串条目的 ACL 把**创建者的代码签名**记为唯一可信应用，而这些 app
//! 是 adhoc 签名（designated requirement = cdhash），**每次重新打包 cdhash
//! 就变** ⇒ 每次启动都弹：
//!
//! ```text
//! z-biz-tool-file wants to use your confidential information
//! stored in "z-biz-tool" in your keychain.
//! ```
//!
//! 改成加密文件后，新落点 `secret.enc` 是空的 —— 用户的密钥还躺在旧钥匙串
//! 条目里。若不搬，用户会看到「配置还在、模型还在、但 AI 说没配密钥」。
//!
//! # 搬，还是让用户重填？
//!
//! **搬**。理由：
//!
//! - 密钥是用户**已经填过一次**的东西，让他再填一遍是纯粹的体验倒退；
//! - 旧条目还在钥匙串里躺着，就算应用不读它，用户在「钥匙串访问」里
//!   看到它也会困惑「这条还要不要管」；
//! - 迁移是幂等的一次性动作，失败也不阻断启动（见下）。
//!
//! # 搬完就删旧条目
//!
//! 留着旧条目有两个害处：
//!
//! 1. **它自己就是个弹窗源** —— 谁再读它（另一个应用、旧的 file.app、
//!    用户自己双击钥匙串访问）都会触发同一套 ACL 授权；
//! 2. 明文密钥在钥匙串里**多躺一份**，与「统一落点」的前提矛盾。
//!
//! 删条目的动作**不需要授权**（实测：`security delete-generic-password`
//! 对自己创建的条目零弹窗返回）—— 所以这一步不会打扰用户。
//!
//! # 删条目失败怎么办
//!
//! **不影响功能，记账走日志**。密钥已经搬进 `secret.enc` 了，删不掉旧条目
//! 只是「盘上多留一份」，不影响新后端工作。强行报错会让用户以为迁移失败
//! 而重填一遍密钥 —— 那比多留一份糟糕得多。
//!
//! # ⚠️ 一个反直觉的实测事实
//!
//! **读**旧条目会失败，**删**旧条目却不需要授权。
//!
//! 旧条目是**另一个签名**的 app（改后端之前那版）创建的，ACL 不认当前签名 ⇒
//! `get_password()` 读不出来。但 `delete-generic-password` 对自己创建的条目
//! **零授权**返回（实测 `security delete-generic-password` 直接成功）。
//!
//! 后果：迁移**搬不到**旧密钥，只能「统一落点里已有的那份」。
//!
//! # 读不出 ⇒ 删还是留？
//!
//! **留**。读不出有两种原因，性质完全不同：
//!
//! 1. 条目根本不存在 —— 删了也是 NoEntry，无所谓；
//! 2. ACL 不认当前签名 —— 条目**还在**，而且它很可能是用户**唯一的密钥副本**
//!    （统一落点里可能是别的密钥、或压根没有）。
//!
//! 情况 2 下删了就**永久丢密钥**。用户此时看到的是「密钥已配置」，不会再去
//! 备份它 ⇒ 出了事没法恢复。宁可留着旧条目（用户能在「钥匙串访问」里看到、
//! 手动抄走或删掉），也不能替他删掉可能唯一的一份。
//!
//! 代价：旧条目还在 ⇒ 若某个**旧版** app 再读它，还会弹一次。但那是旧版的问题，
//! 新版不再读它 —— 而这恰恰是本次要解决的问题。
//!
//! # 迁移的触发点
//!
//! 由**各应用在启动时**调用（`cap_llm_config::migrate_keychain_secret`）。
//! 为什么不在本 crate 里自动做：本 crate 没有「启动」这个概念，它是纯库；
//! 而迁移必须发生在应用启动、且确定要读密钥之前。

use std::path::PathBuf;

use crate::store::StoreError;

/// 迁移结果。
#[derive(Debug, Clone, Default)]
pub struct KeychainMigrationReport {
    /// 是否从钥匙串搬来了密钥。
    pub migrated: bool,
    /// 旧条目是否已删除。
    pub removed_old_entry: bool,
    /// 诊断信息（给日志，不给用户看）。
    pub detail: String,
}

/// 钥匙串条目的 service / account（与 `crate::SECRET_SERVICE` 一致）。
///
/// 单独写出来是为了让本模块不依赖 `store` 的平台分发逻辑 ——
/// 本模块只在 macOS 上有意义，但代码在所有平台都要能编译。
#[cfg(target_os = "macos")]
fn legacy_service() -> String {
    std::env::var("Z_BIZ_TOOL_SECRET_SERVICE")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| crate::SECRET_SERVICE.to_string())
}

/// 「已经问过钥匙串」的标记文件路径。
///
/// 存在统一落点目录（与 `config.json` 同级），命名带 service 名 ——
/// 不同 service 各记各的。
#[cfg(target_os = "macos")]
fn checked_marker_path(service: &str) -> Option<PathBuf> {
    crate::store::config_dir()
        .ok()
        .map(|d| d.join(format!(".keychain-migrated-{}", service)))
}

/// 是否已经问过钥匙串（问过就不再问）。
#[cfg(target_os = "macos")]
fn already_checked(service: &str) -> bool {
    checked_marker_path(service)
        .map(|p| p.exists())
        .unwrap_or(false)
}

/// 记下「已经问过钥匙串」。
#[cfg(target_os = "macos")]
fn mark_keychain_checked(service: &str) {
    if let Some(p) = checked_marker_path(service) {
        let _ = std::fs::write(&p, b"1");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perm = std::fs::metadata(&p)
                .map(|m| m.permissions())
                .unwrap_or_else(|_| std::fs::Permissions::from_mode(0o600));
            perm.set_mode(0o600);
            let _ = std::fs::set_permissions(&p, perm);
        }
    }
}

/// 从钥匙串读旧密钥（**只读，不删**）。
///
/// # ⚠️ 这里有个必须知道的现实约束
///
/// `keyring` crate 走的是 legacy 钥匙串 API，而**旧条目是由另一个签名
/// 的 app 创建的**（我们改后端之前的那版）。实测：
///
/// - **读**：`get_password()` 会因为 ACL 不认当前签名而失败（返回 None 或错误）
///   —— **读不出旧密钥**；
/// - **删**：`delete-generic-password` 对自己创建的条目**不需要授权**，零弹窗返回。
///
/// 所以迁移的实际策略是：
///
/// 1. **尽量读** —— 读到了就把密钥搬进新落点（最理想）；
/// 2. **读不到也要删** —— 旧条目留着就是个弹窗源，必须清掉。
///    此时密钥以「已在统一落点」为前提（正常路径：应用先读新落点，
///    用户重新填一次密钥即可）。
///
/// 删条目**不弹窗**，所以「读不到就删」是安全的。
#[cfg(target_os = "macos")]
fn read_legacy_from_keychain(service: &str) -> Option<String> {
    if already_checked(service) {
        return None;
    }
    let entry = keyring::Entry::new(service, crate::SECRET_ACCOUNT).ok()?;
    match entry.get_password() {
        Ok(v) if !v.trim().is_empty() => Some(v),
        // 条目不存在 / 读失败（ACL 不认当前签名）/ 用户拒绝授权 —— 一律当「没得搬」。
        _ => None,
    }
}

/// 删掉钥匙串里的旧条目。
#[cfg(target_os = "macos")]
fn delete_legacy_from_keychain(service: &str) -> bool {
    let Some(entry) = keyring::Entry::new(service, crate::SECRET_ACCOUNT).ok() else {
        return false;
    };
    matches!(
        entry.delete_credential(),
        Ok(()) | Err(keyring::Error::NoEntry)
    )
}

/// 把钥匙串里的旧密钥搬进加密文件，然后删掉旧条目。
///
/// **幂等**：新落点已有密钥时不覆盖、不动钥匙串。
///
/// **不阻断启动**：任何一步出错都只体现在 [`KeychainMigrationReport::detail`]，
/// 调用方据此打一行日志即可，不该让应用启动失败。
///
/// ## 平台差异
///
/// 非 macOS 平台上密钥一直在 OS 密钥库里（那儿的密钥库没有「签名绑定」
/// 问题），**没有存量需要搬** ⇒ 直接返回「什么都没做」。
pub fn migrate_keychain_secret() -> KeychainMigrationReport {
    let store = crate::store::shared_secret_store();

    // 新落点已有密钥 ⇒ 密钥不必搬了，**但旧条目仍要清**。
    //
    // ⚠️ 这里不能直接返回：那会让旧钥匙串条目永远留在盘上。留着它有两个害处：
    //   1. 它自己就是个弹窗源 —— 谁再读它（另一个应用、旧版 file.app、
    //      用户自己双击「钥匙串访问」）都会触发同一套 ACL 授权；
    //   2. 明文密钥在钥匙串里多躺一份，与「统一落点」的前提矛盾。
    //
    // 所以「已有密钥」走的是**只清理不搬运**的分支。
    let already_have = match store.get() {
        Ok(Some(_)) => true,
        Ok(None) => false,
        Err(e) => {
            return KeychainMigrationReport {
                migrated: false,
                removed_old_entry: false,
                detail: format!("读加密文件失败, 跳过迁移: {}", e),
            };
        }
    };

    #[cfg(target_os = "macos")]
    {
        let service = legacy_service();

        if already_have {
            // 已有密钥 ⇒ 只清旧条目，但**要确认是同一个密钥**才删。
            //
            // ⚠️ 不能盲删：若 `secret.enc` 里是密钥 A、钥匙串里是密钥 B，
            // 盲删会把 B 弄丢且无法恢复。而用户此时看到的是「密钥已配置」，
            // 不会再去备份 B。
            //
            // 读钥匙串会弹窗 —— 但这是**一次性**的：迁移完成后钥匙串里
            // 就没有这个条目了，之后永远不会再读它。
            //
            // ⚠️ 读过之后**无论哪种结局都要记标记**（下面每个分支都会写），
            // 否则「密钥不一致、保留旧条目」这个分支会每次启动都读一次
            // 钥匙串 ⇒ 每次都弹窗 ⇒ 等于没修。
            let Some(old) = read_legacy_from_keychain(&service) else {
                // 读不到旧密钥 —— **不删**。
                //
                // ⚠️ 为什么这么保守：读不到有两种原因，性质完全不同：
                //   1. 条目根本不存在（删了也是 NoEntry，无害）；
                //   2. ACL 不认当前签名 —— 条目**还在**，而且它很可能就是
                //      用户唯一的密钥副本（统一落点里可能是别的或没有密钥）。
                //
                // 情况 2 下删了就**永久丢密钥**。而「统一落点已有密钥」这个前提
                // 只覆盖了本 crate 的迁移路径，**不覆盖**用户手工在别的应用里
                // 配的密钥。所以宁可留着旧条目（用户可在钥匙串访问里看到、
                // 手动删或手动抄），也不能替他删掉可能唯一的一份。
                //
                // 代价是旧条目还在 ⇒ 若某个旧版 app 再读它，还会弹一次。
                // 但那是「旧版 app」的问题，新版不再读它。
                mark_keychain_checked(&service);
                return KeychainMigrationReport {
                    migrated: false,
                    removed_old_entry: false,
                    detail: format!(
                        "钥匙串 service={} 的旧密钥读不出(签名已变), 保留旧条目请手动确认",
                        service
                    ),
                };
            };

            let current = store.get().ok().flatten().unwrap_or_default();
            if current == old {
                let removed = delete_legacy_from_keychain(&service);
                mark_keychain_checked(&service);
                return KeychainMigrationReport {
                    migrated: false,
                    removed_old_entry: removed,
                    detail: if removed {
                        format!(
                            "统一落点已有相同密钥, 已清理钥匙串 service={} 的旧条目",
                            service
                        )
                    } else {
                        format!(
                            "统一落点已有相同密钥, 钥匙串 service={} 旧条目删除失败",
                            service
                        )
                    },
                };
            }

            // 两个密钥不一样 ⇒ 不动钥匙串，留给用户自己判断。
            mark_keychain_checked(&service);
            return KeychainMigrationReport {
                migrated: false,
                removed_old_entry: false,
                detail: format!(
                    "钥匙串 service={} 的密钥与统一落点里的不同, 保留旧条目请手动确认",
                    service
                ),
            };
        }

        let Some(old) = read_legacy_from_keychain(&service) else {
            // 读不出旧密钥 ⇒ 没法搬，也**不删**（可能是用户唯一的密钥副本）。
            // 记标记 ⇒ 以后启动不再问 ⇒ 不会反复弹窗。
            mark_keychain_checked(&service);
            return KeychainMigrationReport {
                migrated: false,
                removed_old_entry: false,
                detail: format!(
                    "钥匙串 service={} 的旧密钥读不出(签名已变), 保留旧条目请手动确认",
                    service
                ),
            };
        };

        if let Err(e) = store.set(&old) {
            // 记标记：写失败时**保留**钥匙串原条目（数据不丢），
            // 但也不能每次启动都重试读它 —— 那会每次弹窗。
            mark_keychain_checked(&service);
            return KeychainMigrationReport {
                migrated: false,
                removed_old_entry: false,
                detail: format!("写入加密文件失败, 保留钥匙串原条目: {}", e),
            };
        }

        // 密钥已落新位置 ⇒ 删旧条目（失败只记账，不影响功能）。
        let removed = delete_legacy_from_keychain(&service);
        // 记标记：以后启动不再读钥匙串 ⇒ 再也不会弹窗。
        mark_keychain_checked(&service);
        KeychainMigrationReport {
            migrated: true,
            removed_old_entry: removed,
            detail: if removed {
                format!(
                    "已把钥匙串 service={} 的密钥搬进加密文件, 并删除旧条目",
                    service
                )
            } else {
                format!(
                    "已把钥匙串 service={} 的密钥搬进加密文件; 旧条目删除失败, 可手动清理",
                    service
                )
            },
        }
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = (store, already_have);
        KeychainMigrationReport {
            migrated: false,
            removed_old_entry: false,
            detail: "非 macOS 平台, 密钥一直在 OS 密钥库, 无需迁移".into(),
        }
    }
}

/// 加密文件后端的落点（`~/.z-biz-tool/llm/secret.enc`）。
///
/// 供诊断用 —— 「密钥到底存哪了」这个问题，答案就是��个路径。
pub fn encrypted_secret_path() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        crate::store::config_dir()
            .ok()
            .map(|d| d.join(crate::SECRET_FILE))
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

/// 把迁移报告渲染成一行日志。
impl std::fmt::Display for KeychainMigrationReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.migrated {
            write!(f, "{}", self.detail)
        } else if self.detail.is_empty() {
            Ok(())
        } else {
            write!(f, "{}", self.detail)
        }
    }
}

/// 让 `StoreError` 能从字符串构造（便于本模块内部简化错误处理）。
impl From<&str> for StoreError {
    fn from(s: &str) -> Self {
        StoreError::Secret(s.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 迁移在没有钥匙串时什么都不做() {
        // 非 macOS 上直接是「无需迁移」；macOS 上若钥匙串没条目也是同结果。
        let report = migrate_keychain_secret();
        assert!(!report.migrated, "没有存量就不该报迁移过");
    }

    #[test]
    fn 迁移是幂等的_不会覆盖已有密钥() {
        let _guard = crate::testing::lock_llm_env();
        let _iso = crate::testing::IsolatedLlm::new("keychain-migrate");
        let store = crate::store::shared_secret_store();
        store.set("sk-already-here").expect("预置密钥");

        let report = migrate_keychain_secret();
        assert!(!report.migrated, "已有密钥时不该迁移");
        assert_eq!(
            store.get().expect("读").as_deref(),
            Some("sk-already-here"),
            "已有密钥不该被覆盖"
        );
    }

    #[test]
    fn 迁移报告可渲染成日志() {
        let report = KeychainMigrationReport {
            migrated: true,
            removed_old_entry: true,
            detail: "已搬".into(),
        };
        assert_eq!(report.to_string(), "已搬");
    }

    /// 「已问过钥匙串」的标记必须真的挡住再次读取 ——
    /// 这是防弹窗的关键闸门。
    #[cfg(target_os = "macos")]
    #[test]
    fn 标记存在后不再问钥匙串() {
        let _guard = crate::testing::lock_llm_env();
        let _iso = crate::testing::IsolatedLlm::new("keychain-checked");

        let service = "z-biz-tool-checked-test";
        assert!(!already_checked(service), "一开始不该有标记");
        mark_keychain_checked(service);
        assert!(already_checked(service), "标记应已写入");

        // 有标记 ⇒ 读钥匙串直接返回 None（不去碰钥匙串）
        assert!(
            read_legacy_from_keychain(service).is_none(),
            "有标记后不该再读钥匙串"
        );
    }

    /// 迁移跑完之后必须留下标记 —— 否则下次启动还会去读钥匙串（⇒ 弹窗）。
    #[cfg(target_os = "macos")]
    #[test]
    fn 迁移后留下已问标记() {
        let _guard = crate::testing::lock_llm_env();
        let _iso = crate::testing::IsolatedLlm::new("keychain-marker");

        // 预置密钥 ⇒ 走「已有密钥」分支
        let store = crate::store::shared_secret_store();
        store.set("sk-preexisting").expect("预置");

        let service = "z-biz-tool-marker-test";
        std::env::set_var("Z_BIZ_TOOL_SECRET_SERVICE", service);

        let report = migrate_keychain_secret();
        // 无论有没有旧条目，标记都该留下（下次启动不再问）
        assert!(
            already_checked(service),
            "迁移后必须留下标记, 否则下次启动还会弹窗。报告：{}",
            report
        );
        std::env::remove_var("Z_BIZ_TOOL_SECRET_SERVICE");
    }
}
