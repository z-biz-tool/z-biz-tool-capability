//! 测试脚手架：随作用域结束递归删除的临时目录。
//!
//! 从 `z-biz-tool-file/src-tauri/src/lib.rs` 的 `test_bridge::TempDir` 原样搬来
//! （原实现有两条实测结论，一并保留）：
//!
//! 1. 只用「不建不删」的临时目录会让一次 `cargo test` 在 /tmp 留下上千个目录、
//!    两百多 MB，每次 CI 与本地跑都翻一倍。`Drop` 连测试 panic 的路径也能清掉。
//! 2. 本机时钟粒度实测只有 1 µs（连续取 199 次时间戳，185 次完全相同），
//!    靠纳秒戳保证唯一是自欺欺人：两个并行测试会拿到同一个目录，先结束那个的
//!    `Drop` 会把另一个还在用的目录整个删掉，表现成「偶发 ENOENT」的假故障。
//!    因此改成原子创建撞名就重试。
//!
//! 通过 `Deref<Target = Path>` 保持和返回 `PathBuf` 时完全一致的写法。

use std::path::{Path, PathBuf};

pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    pub fn new(tag: &str) -> Self {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let pid = std::process::id();
        for _ in 0..1000 {
            let nonce: u128 = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("cap-doc-{}-{}-{}-{}", tag, pid, nonce, seq));
            if std::fs::create_dir_all(&path).is_ok() {
                return Self { path };
            }
        }
        panic!("连续 1000 次都没能建出唯一临时目录（tag={tag}）");
    }
}

impl std::ops::Deref for TempDir {
    type Target = Path;
    fn deref(&self) -> &Self::Target {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

impl AsRef<Path> for TempDir {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}
