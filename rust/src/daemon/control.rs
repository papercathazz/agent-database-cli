use crate::daemon::{client, paths};
use crate::types::{DaemonAction, DaemonRequest};
use crate::utils::paths::runtime_dir;
use anyhow::Result;
use serde_json::{json, Value};
use std::process::{Command, Stdio};
use tokio::time::{sleep, Duration};

pub async fn start_daemon() -> Result<Value> {
    if client::is_daemon_running().await {
        return Ok(json!({ "started": false, "socket": socket_display()? }));
    }
    std::fs::create_dir_all(runtime_dir()?)?;
    let current_exe = std::env::current_exe()?;
    let mut command = Command::new(current_exe);
    command
        .arg("daemon")
        .arg("run")
        .stdin(Stdio::null())
        .stdout(Stdio::null());
    if std::env::var("AGENT_DATABASE_CLI_DEBUG").is_ok() {
        command.stderr(Stdio::inherit());
    } else {
        command.stderr(Stdio::null());
    }
    command.spawn()?;

    for _ in 0..30 {
        sleep(Duration::from_millis(100)).await;
        if client::is_daemon_running().await {
            return Ok(json!({ "started": true, "socket": socket_display()? }));
        }
    }
    anyhow::bail!("daemon 启动超时")
}

pub async fn stop_daemon() -> Result<Value> {
    let response = client::send_daemon_request(&DaemonRequest {
        action: DaemonAction::Stop,
        db: None,
        command: None,
        metadata: None,
        config_path: None,
    })
    .await?;
    if !response.ok {
        anyhow::bail!(response
            .error
            .unwrap_or_else(|| "daemon 停止失败".to_string()));
    }
    Ok(json!({ "stopped": true }))
}

pub async fn daemon_status() -> Result<Value> {
    let response = match client::send_daemon_request(&DaemonRequest {
        action: DaemonAction::Status,
        db: None,
        command: None,
        metadata: None,
        config_path: None,
    })
    .await
    {
        Ok(response) => response,
        Err(_) => {
            return Ok(json!({
                "running": false,
                "connections": []
            }));
        }
    };
    if !response.ok {
        anyhow::bail!(response
            .error
            .unwrap_or_else(|| "daemon 状态查询失败".to_string()));
    }
    Ok(with_running_flag(
        response.data.unwrap_or_else(|| json!({})),
        true,
    ))
}

fn with_running_flag(data: Value, running: bool) -> Value {
    match data {
        Value::Object(mut object) => {
            object.insert("running".to_string(), json!(running));
            Value::Object(object)
        }
        _ => json!({
            "running": running,
            "data": data
        }),
    }
}

fn socket_display() -> Result<String> {
    #[cfg(unix)]
    {
        Ok(paths::socket_path()?.display().to_string())
    }
    #[cfg(windows)]
    {
        paths::socket_path_string()
    }
}

#[cfg(test)]
mod tests {
    use super::with_running_flag;
    use serde_json::json;

    #[test]
    fn status_data_keeps_connections_and_adds_running_flag() {
        let data = with_running_flag(json!({ "connections": [] }), true);

        assert_eq!(data, json!({ "running": true, "connections": [] }));
    }
}
