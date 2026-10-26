use anyhow::Result;
use std::time::Duration;

/// 环境变量只接受正整数秒；缺省或无效值沿用明确的默认值。
pub fn timeout_from_env(name: &str, default_secs: u64) -> Duration {
    Duration::from_secs(
        std::env::var(name)
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(default_secs),
    )
}

pub fn query_timeout() -> Duration {
    timeout_from_env("AGENT_DB_QUERY_TIMEOUT_SECS", 60)
}

pub fn timeout_error() -> anyhow::Error {
    anyhow::anyhow!("查询超时：客户端已停止等待；请检查 SQL / 索引，或调整 AGENT_DB_QUERY_TIMEOUT_SECS。超时不代表写操作未执行，请核实数据库状态后再决定是否重试")
}

pub fn debug_log(message: &str) {
    if std::env::var("AGENT_DATABASE_CLI_DEBUG").is_ok() {
        eprintln!("[agent-database-cli] {message}");
    }
}

/// 清理不占用响应时限；关闭失败仍须记录，避免把清理错误静默吞掉。
pub fn log_cleanup(result: Result<()>) {
    if let Err(error) = result {
        debug_log(&format!(
            "连接清理失败: {}",
            crate::utils::masking::to_error_message(&error)
        ));
    }
}
