//! 测试隔离：让各仓的测试不碰用户真实的配置与钥匙串。
//!
//! ## 为什么需要这个模块
//!
//! 统一落点有两处「全局状态」：
//!
//! - 配置目录 `~/.z-biz-tool/llm/`
//! - 钥匙串条目 service=`z-biz-tool` / account=`llm.apikey`
//!
//! 十个仓的测试都要读写它。若不隔离，后果有三：
//!
//! 1. **污染用户配置** —— 测试把 `~/.z-biz-tool/llm/config.json` 改成
//!    `model: "test-model"`，用户下次开应用发现自己的模型名被改了。
//! 2. **测试互相踩** —— 十仓的测试并行跑，甲仓写完配置，乙仓读出来
//!    就是甲仓的值，表现为「单跑绿、全跑红」，且红的仓与代码无关。
//! 3. **失败留残骸** —— 测试中途 panic，用户配置停在半截状态。
//!
//! ## 用法
//!
//! ```no_run
//! use cap_llm_config::testing::IsolatedLlm;
//!
//! #[test]
//! fn my_test() {
//!     let iso = IsolatedLlm::new("my-test");
//!     // 此后读写统一落点都落在临时目录 + 临时钥匙串条目上
//!     assert!(iso.config_path().starts_with(iso.root()));
//! }
//! ```
//!
//! ## 为什么用环境变量而不是参数
//!
//! 统一落点的读取入口是**无参**的（`load_config()` / `shared_secret_store()`），
//! 这样十个仓的调用点都一样。代价是没法用参数传隔离路径 ——
//! 环境变量是唯一能让「无参入口」保持无参的办法。
//!
//! ⚠️ 环境变量是**进程级**的，所以 `IsolatedLlm` 只能隔离**目录**，
//! 不能并发隔离同一进程内的两个测试。同一进程内需要并发时，
//! 用 [`IsolatedLlm::new_in_process`] 之外的手段（直接调底层函数）。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(test)]
use std::sync::{Mutex, MutexGuard};

/// 计数器：保证同一进程内多次 `new` 得到不同的目录。
static COUNTER: AtomicU64 = AtomicU64::new(0);

/// crate 级的**环境变量锁**。
///
/// 统一落点的隔离靠环境变量（见本模块文档），而环境变量是**进程级**的 ——
/// 两个测试同时改 `Z_BIZ_TOOL_LLM_DIR`，后构造的那个会覆盖前一个的设置，
/// 前一个的断言随即失败。症状是「单跑绿、全跑红」，且红的测试与代码无关。
///
/// 所以**凡是本 crate 里碰统一落点的测试都要先拿这把锁**。
/// 下游各仓（file / kb / db …）在自己的进程里跑，各自带自己的锁即可 ——
/// 进程之间互不干扰。
#[cfg(test)]
pub(crate) static LLM_ENV_LOCK: Mutex<()> = Mutex::new(());

/// 拿环境变量锁。锁被 poison 时照样放行 ——
/// 一个测试 panic 不该让后续所有测试都拒绝执行。
#[cfg(test)]
pub(crate) fn lock_llm_env() -> MutexGuard<'static, ()> {
    LLM_ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 隔离的统一落点。
///
/// 构造时即把进程的环境变量指向临时目录与临时钥匙串 service，
/// `Drop` 时恢复原值。
///
/// ## 环境变量是进程级的
///
/// 这意味着**同一进程内不能并发跑两个 `IsolatedLlm`** ——
/// 后构造的会覆盖前一个的设置。需要并发隔离时，
/// 每个测试应跑在**独立进程**里（`cargo test` 的每个集成测试文件
/// 就是一个独立进程；单元测试则在同一个进程里，需自行串行化）。
pub struct IsolatedLlm {
    root: PathBuf,
    dir_var: Option<std::ffi::OsString>,
    service_var: Option<std::ffi::OsString>,
}

impl IsolatedLlm {
    /// 建一个隔离环境，名字用于区分不同测试的临时目录。
    pub fn new(name: &str) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "z-biz-tool-llm-test-{}-{}-{}",
            std::process::id(),
            name,
            n
        ));
        Self::with_root(root)
    }

    /// 用指定目录建隔离环境。
    pub fn with_root(root: PathBuf) -> Self {
        let dir_var = std::env::var_os("Z_BIZ_TOOL_LLM_DIR");
        let service_var = std::env::var_os("Z_BIZ_TOOL_SECRET_SERVICE");

        let _ = std::fs::create_dir_all(&root);
        std::env::set_var("Z_BIZ_TOOL_LLM_DIR", &root);
        // 钥匙串 service 带测试后缀 ⇒ 测试条目不会落到用户的真实 service 上。
        // ⚠️ 这**不会**完全避免污染：钥匙串里会留下一条
        // `z-biz-tool-test-*` 条目。所以测试应当在结束时删掉它
        // （[`IsolatedLlm::cleanup`]），否则钥匙串里会积累垃圾。
        std::env::set_var(
            "Z_BIZ_TOOL_SECRET_SERVICE",
            format!("z-biz-tool-test-{}", std::process::id()),
        );

        Self {
            root,
            dir_var,
            service_var,
        }
    }

    /// 隔离后的配置目录。
    pub fn dir(&self) -> &Path {
        &self.root
    }

    /// 隔离根目录（别名）。
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 隔离后的配置文件路径。
    pub fn config_path(&self) -> PathBuf {
        self.root.join(crate::CONFIG_FILE)
    }

    /// 删掉测试期间在钥匙串里留下的条目。
    ///
    /// **必须调用** —— 否则用户的钥匙串里会积累一条条
    /// `z-biz-tool-test-<pid>` 垃圾条目。
    pub fn cleanup(&self) {
        if let Ok(entry) = keyring::Entry::new(&secret_service_name(), crate::SECRET_ACCOUNT) {
            match entry.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => {}
                Err(_) => {}
            }
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

impl Drop for IsolatedLlm {
    fn drop(&mut self) {
        // 恢复环境变量：不恢复的话，同进程里后续的测试会一直用着
        // 上一个测试的临时目录。
        match self.dir_var.take() {
            Some(v) => std::env::set_var("Z_BIZ_TOOL_LLM_DIR", v),
            None => std::env::remove_var("Z_BIZ_TOOL_LLM_DIR"),
        }
        match self.service_var.take() {
            Some(v) => std::env::set_var("Z_BIZ_TOOL_SECRET_SERVICE", v),
            None => std::env::remove_var("Z_BIZ_TOOL_SECRET_SERVICE"),
        }
    }
}

/// 当前生效的密钥库 service 名（与 store 内部同一份逻辑）。
fn secret_service_name() -> String {
    std::env::var("Z_BIZ_TOOL_SECRET_SERVICE")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| crate::SECRET_SERVICE.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn isolation_redirects_the_config_path() {
        let _guard = lock_llm_env();
        let iso = IsolatedLlm::new("redirect");
        let path = crate::config_path().expect("配置路径");
        assert!(
            path.starts_with(iso.root()),
            "配置路径应落在隔离目录内，实际 {} vs {}",
            path.display(),
            iso.root().display()
        );
    }

    #[test]
    fn isolation_is_undone_on_drop() {
        let _guard = lock_llm_env();
        let before = std::env::var_os("Z_BIZ_TOOL_LLM_DIR");
        {
            let _iso = IsolatedLlm::new("undone");
            assert!(std::env::var_os("Z_BIZ_TOOL_LLM_DIR").is_some());
        }
        assert_eq!(
            std::env::var_os("Z_BIZ_TOOL_LLM_DIR"),
            before,
            "隔离对象析构后必须恢复原环境变量"
        );
    }

    #[test]
    fn isolation_uses_a_distinct_keyring_service() {
        let _guard = lock_llm_env();
        let iso = IsolatedLlm::new("service");
        let svc = std::env::var("Z_BIZ_TOOL_SECRET_SERVICE").expect("service");
        assert_ne!(
            svc,
            crate::SECRET_SERVICE,
            "测试必须用不同的 service，否则会往用户真实条目里写"
        );
        assert!(svc.starts_with("z-biz-tool-test-"));
        drop(iso);
    }
}
