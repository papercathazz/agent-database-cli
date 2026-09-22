//! 测试小工具。

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// 临时目录：Drop 时自动清理。
///
/// 之前每个用例跑完都会在系统临时目录里留一个目录，`cargo test` 跑多了就是几百个垃圾。
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new(name: &str) -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("系统时间早于 UNIX_EPOCH")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("agent-database-cli-{name}-{unique}"));
        std::fs::create_dir_all(&dir).expect("创建临时目录失败");
        Self(dir)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
