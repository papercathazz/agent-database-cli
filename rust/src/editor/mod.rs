//! 配置文件可视化编辑：本地 HTTP 服务 + 单文件页面。
//!
//! 服务只监听 127.0.0.1，所有 `/api/*` 请求都要求访问令牌，
//! 令牌随 URL fragment 交给页面，浏览器不会把它发给服务端日志或 Referer。

pub mod config_store;
pub mod control;
pub mod http;
#[cfg(test)]
pub mod page_check;
#[cfg(test)]
pub mod page_smoke;
pub mod server;
#[cfg(test)]
pub mod test_support;

use crate::utils::paths::runtime_dir;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

/// 编辑服务状态，落盘到状态文件，供 start / stop / status 之间共享。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditorState {
    pub pid: u32,
    pub port: u16,
    /// 访问令牌，只有持有令牌的请求才能读写配置
    pub token: String,
    pub url: String,
    pub config_path: String,
    pub started_at: u64,
}

/// 状态文件所在目录的环境变量覆盖，只给测试用：避免测试碰到真实的运行时目录。
pub const STATE_DIR_ENV: &str = "AGENT_DATABASE_CLI_EDITOR_STATE_DIR";

/// 编辑服务状态文件：记录 pid、端口、令牌和页面地址。
pub fn state_path() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os(STATE_DIR_ENV) {
        return Ok(PathBuf::from(dir).join("editor.json"));
    }
    Ok(runtime_dir()?.join("editor.json"))
}

/// 测试用串行锁：状态目录是进程级环境变量，多个用例不能同时改它。
/// 用异步锁是因为用例要在 await 期间一直持有它。
#[cfg(test)]
pub fn state_env_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// 生成一次性访问令牌，服务每次启动都会刷新。
pub fn generate_token() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

pub fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}
