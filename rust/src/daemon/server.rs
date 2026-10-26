use crate::adapters::deadline::timeout_from_env;
use crate::daemon::config_manager::DaemonConfigManager;
use crate::types::{DaemonAction, DaemonRequest, DaemonResponse};
use crate::utils::masking::to_error_message;
use crate::utils::paths::runtime_dir;
use anyhow::Result;
use futures::FutureExt;
use serde_json::{json, Value};
use std::{
    panic::AssertUnwindSafe,
    sync::{Arc, Mutex as StdMutex},
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;
use tokio::time::{sleep, Duration, Instant};

#[cfg(windows)]
use tokio::net::windows::named_pipe::ServerOptions;
#[cfg(unix)]
use tokio::net::UnixListener;

/// 接收请求与决定退出共用一把锁，避免空闲检查之后又有请求开始执行。
struct DaemonActivity {
    active: usize,
    last_activity: Instant,
    shutting_down: bool,
}

impl DaemonActivity {
    fn new() -> Arc<StdMutex<Self>> {
        Arc::new(StdMutex::new(Self {
            active: 0,
            last_activity: Instant::now(),
            shutting_down: false,
        }))
    }

    fn enter(activity: &Arc<StdMutex<Self>>) -> Option<ActiveRequest> {
        let mut state = activity.lock().expect("daemon 活动锁中毒");
        if state.shutting_down {
            return None;
        }
        state.active += 1;
        Some(ActiveRequest(activity.clone()))
    }

    fn begin_shutdown(&mut self, idle: Duration) -> bool {
        if !self.shutting_down && self.active == 0 && self.last_activity.elapsed() >= idle {
            self.shutting_down = true;
            true
        } else {
            false
        }
    }
}

/// 从 accept 到完整响应写出均持有；错误、panic 和任务取消也会刷新活动时间。
struct ActiveRequest(Arc<StdMutex<DaemonActivity>>);

impl Drop for ActiveRequest {
    fn drop(&mut self) {
        let mut activity = self.0.lock().expect("daemon 活动锁中毒");
        activity.last_activity = Instant::now();
        activity.active -= 1;
    }
}

pub async fn run_server() -> Result<()> {
    #[cfg(unix)]
    {
        let runtime_dir = runtime_dir()?;
        tokio::fs::create_dir_all(&runtime_dir).await?;
        let socket_path = crate::daemon::paths::socket_path()?;
        let _ = tokio::fs::remove_file(&socket_path).await;
        let listener = UnixListener::bind(&socket_path)?;
        let pid_path = crate::daemon::paths::pid_path()?;
        tokio::fs::write(&pid_path, std::process::id().to_string()).await?;

        let manager = Arc::new(Mutex::new(DaemonConfigManager::new()));
        let activity = DaemonActivity::new();
        spawn_idle_shutdown(
            manager.clone(),
            activity.clone(),
            socket_path.clone(),
            pid_path.clone(),
        );

        loop {
            let (stream, _) = listener.accept().await?;
            let manager = manager.clone();
            let Some(active) = DaemonActivity::enter(&activity) else {
                continue;
            };
            tokio::spawn(async move {
                if let Err(error) = handle_duplex_stream(stream, manager, active).await {
                    debug_log(&format!(
                        "daemon 请求处理失败: {}",
                        to_error_message(&error)
                    ));
                }
            });
        }
    }
    #[cfg(windows)]
    {
        let runtime_dir = runtime_dir()?;
        tokio::fs::create_dir_all(&runtime_dir).await?;
        let pipe_name = crate::daemon::paths::socket_path_string()?;
        let pid_path = crate::daemon::paths::pid_path()?;
        tokio::fs::write(&pid_path, std::process::id().to_string()).await?;

        let manager = Arc::new(Mutex::new(DaemonConfigManager::new()));
        let activity = DaemonActivity::new();
        spawn_idle_shutdown(manager.clone(), activity.clone(), pid_path.clone());

        loop {
            let server = ServerOptions::new().create(&pipe_name)?;
            server.connect().await?;
            let manager = manager.clone();
            let Some(active) = DaemonActivity::enter(&activity) else {
                continue;
            };
            tokio::spawn(async move {
                if let Err(error) = handle_duplex_stream(server, manager, active).await {
                    debug_log(&format!(
                        "daemon 请求处理失败: {}",
                        to_error_message(&error)
                    ));
                }
            });
        }
    }
}

async fn handle_duplex_stream<S>(
    stream: S,
    manager: Arc<Mutex<DaemonConfigManager>>,
    _active: ActiveRequest,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).await?;
    let response = build_response(line.trim(), manager).await;
    let mut stream = reader.into_inner();
    stream
        .write_all(serde_json::to_string(&response)?.as_bytes())
        .await?;
    stream.write_all(b"\n").await?;
    stream.flush().await?;
    Ok(())
}

async fn build_response(payload: &str, manager: Arc<Mutex<DaemonConfigManager>>) -> DaemonResponse {
    let request = match serde_json::from_str::<DaemonRequest>(payload) {
        Ok(request) => request,
        Err(error) => {
            return DaemonResponse {
                ok: false,
                data: None,
                error: Some(error.to_string()),
            };
        }
    };

    // 单个请求内部的业务错误必须通过协议写回，避免客户端误报 daemon 无响应。
    match AssertUnwindSafe(handle_request(request, manager))
        .catch_unwind()
        .await
    {
        Ok(Ok(data)) => DaemonResponse {
            ok: true,
            data: Some(data),
            error: None,
        },
        Ok(Err(error)) => DaemonResponse {
            ok: false,
            data: None,
            error: Some(to_error_message(&error)),
        },
        Err(payload) => DaemonResponse {
            ok: false,
            data: None,
            error: Some(format!("daemon 请求处理 panic: {}", panic_message(payload))),
        },
    }
}

async fn handle_request(
    request: DaemonRequest,
    manager: Arc<Mutex<DaemonConfigManager>>,
) -> Result<Value> {
    match request.action {
        DaemonAction::Status => {
            let current = manager.lock().await.current_manager();
            match current {
                Some(current) => Ok(current.status().await),
                None => Ok(json!({ "connections": [] })),
            }
        }
        DaemonAction::Stop => {
            tokio::spawn(async {
                sleep(Duration::from_millis(20)).await;
                std::process::exit(0);
            });
            Ok(json!({ "stopped": true }))
        }
        DaemonAction::Test
        | DaemonAction::Execute
        | DaemonAction::Metadata
        | DaemonAction::Reset => {
            let db = request
                .db
                .ok_or_else(|| anyhow::anyhow!("daemon 请求必须提供 db"))?;
            let current = {
                let mut guard = manager.lock().await;
                guard.get_manager(request.config_path).await?
            };
            match request.action {
                DaemonAction::Test => current.test(&db).await,
                DaemonAction::Execute => {
                    let command = request
                        .command
                        .ok_or_else(|| anyhow::anyhow!("execute 请求必须提供 command"))?;
                    Ok(serde_json::to_value(current.execute(&db, &command).await?)?)
                }
                DaemonAction::Metadata => {
                    let metadata = request
                        .metadata
                        .ok_or_else(|| anyhow::anyhow!("metadata 请求必须提供 metadata"))?;
                    Ok(serde_json::to_value(
                        current.metadata(&db, metadata).await?,
                    )?)
                }
                DaemonAction::Reset => current.reset(&db).await,
                _ => unreachable!(),
            }
        }
    }
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        return message.to_string();
    }
    if let Some(message) = payload.downcast_ref::<String>() {
        return message.clone();
    }
    "未知 panic".to_string()
}

fn debug_log(message: &str) {
    if std::env::var("AGENT_DATABASE_CLI_DEBUG").is_ok() {
        eprintln!("[agent-database-cli daemon] {message}");
    }
}

fn spawn_idle_shutdown(
    manager: Arc<Mutex<DaemonConfigManager>>,
    activity: Arc<StdMutex<DaemonActivity>>,
    #[cfg(unix)] socket_path: std::path::PathBuf,
    pid_path: std::path::PathBuf,
) {
    let idle = timeout_from_env("AGENT_DB_DAEMON_IDLE_SECS", 1800);
    tokio::spawn(async move {
        loop {
            sleep(Duration::from_secs(5)).await;
            let mut manager = manager.lock().await;
            if let Err(error) = manager.cleanup_idle().await {
                debug_log(&format!("空闲连接清理失败: {}", to_error_message(&error)));
            }
            let shutdown = activity
                .lock()
                .expect("daemon 活动锁中毒")
                .begin_shutdown(idle);
            if shutdown {
                debug_log("daemon 空闲期限已到且无活跃请求，开始退出");
                if let Err(error) = manager.close_all().await {
                    debug_log(&format!("退出时关闭连接失败: {}", to_error_message(&error)));
                }
                #[cfg(unix)]
                if let Err(error) = tokio::fs::remove_file(&socket_path).await {
                    debug_log(&format!("清理 daemon socket 失败: {error}"));
                }
                if let Err(error) = tokio::fs::remove_file(&pid_path).await {
                    debug_log(&format!("清理 daemon pid 文件失败: {error}"));
                }
                std::process::exit(0);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::timeout;

    #[tokio::test]
    async fn response_write_keeps_request_active_and_shutdown_excludes_new_requests() {
        // 只有一个字节的缓冲区让响应稳定阻塞在写入阶段，而不是依赖慢数据库。
        let (server, mut client) = tokio::io::duplex(1);
        let activity = DaemonActivity::new();
        let active = DaemonActivity::enter(&activity).unwrap();
        let task = tokio::spawn(handle_duplex_stream(
            server,
            Arc::new(Mutex::new(DaemonConfigManager::new())),
            active,
        ));
        client
            .write_all(b"{\"action\":\"status\"}\n")
            .await
            .unwrap();
        tokio::task::yield_now().await;
        {
            let mut state = activity.lock().unwrap();
            state.last_activity -= Duration::from_secs(20);
            assert_eq!(state.active, 1);
            assert!(!state.begin_shutdown(Duration::from_secs(1)));
        }
        let mut response = String::new();
        timeout(
            Duration::from_secs(1),
            BufReader::new(client).read_line(&mut response),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            serde_json::from_str::<DaemonResponse>(&response)
                .unwrap()
                .ok
        );
        task.await.unwrap().unwrap();
        {
            let mut state = activity.lock().unwrap();
            assert_eq!(state.active, 0);
            assert!(
                !state.begin_shutdown(Duration::from_secs(1)),
                "写完后重新计算空闲时间"
            );
            state.last_activity -= Duration::from_secs(20);
            assert!(state.begin_shutdown(Duration::from_secs(1)));
        }
        assert!(DaemonActivity::enter(&activity).is_none());
    }

    #[tokio::test]
    async fn aborted_and_failed_requests_release_activity() {
        let activity = DaemonActivity::new();
        let (server, client) = tokio::io::duplex(1);
        let task = tokio::spawn(handle_duplex_stream(
            server,
            Arc::new(Mutex::new(DaemonConfigManager::new())),
            DaemonActivity::enter(&activity).unwrap(),
        ));
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(activity.lock().unwrap().active, 0);
        drop(client);

        let (server, mut client) = tokio::io::duplex(64);
        client
            .write_all(b"{\"action\":\"status\"}\n")
            .await
            .unwrap();
        drop(client);
        assert!(handle_duplex_stream(
            server,
            Arc::new(Mutex::new(DaemonConfigManager::new())),
            DaemonActivity::enter(&activity).unwrap()
        )
        .await
        .is_err());
        assert_eq!(activity.lock().unwrap().active, 0);
    }
}
