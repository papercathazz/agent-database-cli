use crate::utils::paths::runtime_dir;
#[cfg(windows)]
use anyhow::Context;
use anyhow::Result;
use std::path::PathBuf;

pub fn pid_path() -> Result<PathBuf> {
    Ok(runtime_dir()?.join("agent-database-cli.pid"))
}

#[cfg(unix)]
pub fn socket_path() -> Result<PathBuf> {
    Ok(runtime_dir()?.join("agent-database-cli.sock"))
}

#[cfg(windows)]
pub fn socket_path_string() -> Result<String> {
    use sha1::{Digest, Sha1};
    let home = dirs::home_dir().context("无法解析用户主目录")?;
    let mut hasher = Sha1::new();
    hasher.update(home.to_string_lossy().as_bytes());
    let digest = format!("{:x}", hasher.finalize());
    Ok(format!(r"\\.\pipe\agent-database-cli-{}", &digest[..12]))
}
