use crate::config::resolve_config_path;
use crate::editor::{generate_token, state_path, EditorState};
use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::fs;
use std::process::{Child, Command, Stdio};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::{sleep, timeout, Duration};

/// 本地探测单次请求超时，端口被占用或服务无响应时不会长时间挂起。
const PROBE_TIMEOUT: Duration = Duration::from_millis(1200);
const START_TIMEOUT: Duration = Duration::from_secs(8);
const STOP_TIMEOUT: Duration = Duration::from_secs(8);
const POLL_INTERVAL: Duration = Duration::from_millis(80);
const TOKEN_HEADER: &str = "x-agentdb-editor-token";

struct HttpReply {
    status: u16,
    body: Vec<u8>,
}

pub async fn start_editor(port: u16, open_browser: bool) -> Result<Value> {
    let config_path = resolve_config_path()?;
    if let Some(state) = read_state()? {
        if probe(state.port, &state.token).await.is_some() {
            let browser_opened = open_browser && open_in_browser(&state.url).is_ok();
            return Ok(json!({
                "started": false,
                "running": true,
                "url": state.url,
                "port": state.port,
                "pid": state.pid,
                "configPath": state.config_path,
                "browserOpened": browser_opened,
            }));
        }
    }

    let token = generate_token();
    let mut child = spawn_editor_process(port, &token, &config_path)?;
    let state = match wait_for_ready(&mut child, &token).await {
        Ok(state) => state,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
    };
    let browser_opened = open_browser && open_in_browser(&state.url).is_ok();
    Ok(json!({
        "started": true,
        "running": true,
        "url": state.url,
        "port": state.port,
        "pid": state.pid,
        "configPath": state.config_path,
        "browserOpened": browser_opened,
    }))
}

pub async fn stop_editor() -> Result<Value> {
    let Some(state) = read_state()? else {
        return Ok(json!({ "stopped": false, "running": false }));
    };
    let running = probe(state.port, &state.token).await.is_some();
    if running {
        // 优先优雅关闭，让服务自己写完退出；失败再按 pid 终止
        let graceful = api_request(state.port, "POST", "/api/shutdown", &state.token, None)
            .await
            .is_ok();
        if !graceful || !wait_until_down(&state).await {
            terminate_process(state.pid)?;
            wait_until_down(&state).await;
        }
    }
    clear_state(state.pid, &state.token)?;
    Ok(json!({ "stopped": running, "running": false }))
}

pub async fn editor_status() -> Result<Value> {
    let Some(state) = read_state()? else {
        return Ok(json!({ "running": false, "connections": [] }));
    };
    match probe(state.port, &state.token).await {
        Some(health) => Ok(json!({
            "running": true,
            "url": state.url,
            "port": state.port,
            "pid": state.pid,
            "configPath": state.config_path,
            "startedAt": state.started_at,
            "health": health,
        })),
        // 状态文件残留但服务已退出，提示 stale 供用户判断
        None => Ok(json!({
            "running": false,
            "stale": true,
            "url": state.url,
            "port": state.port,
            "pid": state.pid,
        })),
    }
}

pub fn write_state(state: &EditorState) -> Result<()> {
    let path = state_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, serde_json::to_vec_pretty(state)?)
        .with_context(|| format!("写入编辑服务状态失败: {}", path.display()))?;
    restrict_permissions(&path, 0o600);
    Ok(())
}

pub fn read_state() -> Result<Option<EditorState>> {
    let path = state_path()?;
    if !path.is_file() {
        return Ok(None);
    }
    let raw = fs::read_to_string(&path)
        .with_context(|| format!("读取编辑服务状态失败: {}", path.display()))?;
    match serde_json::from_str::<EditorState>(&raw) {
        Ok(state) => Ok(Some(state)),
        Err(error) => {
            // 状态文件损坏时按未启动处理，下一次 start 会覆盖
            eprintln!("编辑服务状态文件无法解析，已忽略: {error}");
            Ok(None)
        }
    }
}

pub fn clear_state(pid: u32, token: &str) -> Result<()> {
    let path = state_path()?;
    let Some(state) = read_state()? else {
        return Ok(());
    };
    // 只清理属于本次实例的状态文件，避免删掉后来者的状态
    if state.pid != pid || state.token != token {
        return Ok(());
    }
    if path.is_file() {
        fs::remove_file(&path)
            .with_context(|| format!("清理编辑服务状态失败: {}", path.display()))?;
    }
    Ok(())
}

fn spawn_editor_process(port: u16, token: &str, config_path: &std::path::Path) -> Result<Child> {
    let current_exe = std::env::current_exe()?;
    let mut command = Command::new(current_exe);
    command
        .arg("editor")
        .arg("run")
        .arg("--port")
        .arg(port.to_string())
        .arg("--token")
        .arg(token)
        .arg("--config-path")
        .arg(config_path.display().to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null());
    if std::env::var("AGENT_DATABASE_CLI_DEBUG").is_ok() {
        command.stderr(Stdio::inherit());
    } else {
        command.stderr(Stdio::null());
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // 独立进程组，避免终端退出或 Ctrl+C 连带结束编辑服务
        command.process_group(0);
    }
    command.spawn().context("启动编辑服务进程失败")
}

async fn wait_for_ready(child: &mut Child, token: &str) -> Result<EditorState> {
    let deadline = tokio::time::Instant::now() + START_TIMEOUT;
    loop {
        if let Some(state) = read_state()? {
            if state.token == token && probe(state.port, &state.token).await.is_some() {
                return Ok(state);
            }
        }
        if let Some(status) = child.try_wait()? {
            return Err(anyhow!("编辑服务启动失败，子进程已退出: {status}"));
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(anyhow!("编辑服务启动超时"));
        }
        sleep(POLL_INTERVAL).await;
    }
}

async fn wait_until_down(state: &EditorState) -> bool {
    let deadline = tokio::time::Instant::now() + STOP_TIMEOUT;
    loop {
        if probe(state.port, &state.token).await.is_none() {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        sleep(POLL_INTERVAL).await;
    }
}

async fn probe(port: u16, token: &str) -> Option<Value> {
    let reply = api_request(port, "GET", "/api/health", token, None)
        .await
        .ok()?;
    if reply.status != 200 {
        return None;
    }
    serde_json::from_slice(&reply.body).ok()
}

async fn api_request(
    port: u16,
    method: &str,
    path: &str,
    token: &str,
    body: Option<Value>,
) -> Result<HttpReply> {
    let body = match body {
        Some(value) => serde_json::to_string(&value)?,
        None => String::new(),
    };
    let request = format!(
        "{method} {path} HTTP/1.1\r\nhost: 127.0.0.1:{port}\r\nconnection: close\r\n{TOKEN_HEADER}: {token}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
        body.len()
    );
    let mut stream = timeout(PROBE_TIMEOUT, TcpStream::connect(("127.0.0.1", port)))
        .await
        .with_context(|| format!("连接 127.0.0.1:{port} 超时"))?
        .with_context(|| format!("连接 127.0.0.1:{port} 失败"))?;
    stream.write_all(request.as_bytes()).await?;
    stream.flush().await?;
    let mut raw = Vec::new();
    timeout(PROBE_TIMEOUT, stream.read_to_end(&mut raw))
        .await
        .context("读取编辑服务响应超时")??;
    parse_http_reply(&raw)
}

fn parse_http_reply(raw: &[u8]) -> Result<HttpReply> {
    let boundary = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| anyhow!("编辑服务响应不完整"))?;
    let head = String::from_utf8_lossy(&raw[..boundary]);
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| anyhow!("编辑服务响应状态行非法"))?;
    Ok(HttpReply {
        status,
        body: raw[boundary + 4..].to_vec(),
    })
}

fn open_in_browser(url: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = Command::new("open");
        command.arg(url);
        command
    };
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut command = Command::new("cmd");
        command.args(["/C", "start", "", url]);
        command
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut command = {
        let mut command = Command::new("xdg-open");
        command.arg(url);
        command
    };
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("打开浏览器失败")?;
    Ok(())
}

#[cfg(unix)]
fn terminate_process(pid: u32) -> Result<()> {
    let result = unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
    if result == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    // 进程已不存在时视为已终止
    if error.raw_os_error() == Some(libc::ESRCH) {
        return Ok(());
    }
    Err(error).with_context(|| format!("结束编辑服务进程 {pid} 失败"))
}

#[cfg(windows)]
fn terminate_process(pid: u32) -> Result<()> {
    let status = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .with_context(|| format!("结束编辑服务进程 {pid} 失败"))?;
    if status.success() {
        Ok(())
    } else {
        Err(anyhow!("结束编辑服务进程 {pid} 失败"))
    }
}

#[cfg(unix)]
fn restrict_permissions(path: &std::path::Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode));
}

#[cfg(windows)]
fn restrict_permissions(_path: &std::path::Path, _mode: u32) {}
