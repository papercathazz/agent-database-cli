use anyhow::{Context, Result};
use std::path::PathBuf;

/// 本地运行时目录，统一存放配置文件、daemon 状态和编辑服务状态。
pub fn runtime_dir() -> Result<PathBuf> {
    Ok(dirs::home_dir()
        .context("无法解析用户主目录")?
        .join(".agent-database-cli"))
}
