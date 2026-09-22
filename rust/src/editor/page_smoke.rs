//! 编辑页面的浏览器 smoke 测试。
//!
//! `cargo test` 跑不了浏览器，静态检查也抓不到行为回归（例如「在 URL 框打字不再刷新 JSON」）。
//! 这里用本机 Chrome 的 headless 模式做一次真实渲染 + 交互：
//!
//! 1. 起真实的编辑服务（状态文件指向临时目录，不碰用户运行时目录）；
//! 2. 起一个极小的反向代理：`GET /` 返回注入了自检脚本的页面，其余请求转发给编辑服务，
//!    这样自检脚本和页面同源，可以用真实的 `fetch` 和 DOM；
//! 3. headless Chrome 打开页面，自检脚本跑完把结果 `POST /__selftest` 回代理，测试断言。
//!
//! 用 POST 回传而不是 `--dump-dom`：Chrome 153 上 `--dump-dom` 会挂住，headless 直接
//! 加载 URL 则正常。机器上没有 Chrome 时跳过，不阻塞本地/CI 的其它测试。

use crate::editor::server;
use crate::editor::test_support::TempDir;
use crate::editor::{generate_token, state_path, STATE_DIR_ENV};
use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio::time::{sleep, Duration};

const CHROME_CANDIDATES: &[&str] = &[
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    "/Applications/Chromium.app/Contents/MacOS/Chromium",
    "/usr/bin/google-chrome",
    "/usr/bin/chromium",
    "/usr/bin/chromium-browser",
];

const SELFTEST_PATH: &str = "/__selftest";

/// 注入到页面里的自检脚本：跑完把结果 POST 回代理，测试从代理侧取。
const HARNESS: &str = r##"<script>
(async function () {
  const result = { errors: [], steps: {} };
  window.addEventListener("error", (event) => result.errors.push(String(event.message)));
  const wait = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
  const until = async (probe) => {
    for (let i = 0; i < 200; i += 1) {
      try { if (probe()) return true; } catch (error) { /* 页面还没渲染完 */ }
      await wait(25);
    }
    return false;
  };
  try {
    result.steps.rows = await until(() => document.querySelectorAll("#railList .row").length > 0);
    result.steps.rowCount = document.querySelectorAll("#railList .row").length;

    // 4) 布局：同一行里图标和文字的垂直中心不能偏（已加密胶囊踩过这个坑）
    let worst = 0;
    document.querySelectorAll("body *").forEach((element) => {
      // 显式顶部对齐的容器（多行提示条）不参与，只查声称居中却没居中的
      const align = getComputedStyle(element).alignItems;
      if (align === "flex-start" || align === "baseline") return;
      const svg = Array.from(element.children).find((child) => child.tagName === "svg");
      if (!svg) return;
      const texts = Array.from(element.childNodes).filter(
        (node) => node.nodeType === 3 && node.textContent.trim()
      );
      if (!texts.length) return;
      const iconRect = svg.getBoundingClientRect();
      if (!iconRect.height) return;
      const range = document.createRange();
      range.selectNodeContents(texts[0]);
      const textRect = range.getBoundingClientRect();
      const offset = Math.abs(
        iconRect.top + iconRect.height / 2 - (textRect.top + textRect.height / 2)
      );
      if (offset > worst) worst = offset;
    });
    result.steps.maxIconTextOffset = Math.round(worst * 100) / 100;

    // 1) 密文显隐：点眼睛应解密出明文，且不写回 JSON
    const eye = document.querySelector('[data-act="toggle-secret"][data-field="url"]');
    if (eye) {
      eye.click();
      await until(() => document.querySelector("#pwInput").value !== "");
      result.steps.secretValue = document.querySelector("#pwInput").value;
      result.steps.secretAttr = document.querySelector("#pwInput").getAttribute("value");
      result.steps.jsonHasSecret = document.querySelector("#rawInput").value.indexOf("s3cret-pw") >= 0;
    } else {
      result.errors.push("找不到密码眼睛按钮");
    }

    // 2) URL 输入：改动要实时同步到 JSON 面板
    const url = document.querySelector('[data-bind="url"]');
    url.value = "mysql://root:newpw@127.0.0.1:3307/renamed";
    url.dispatchEvent(new Event("input", { bubbles: true }));
    await until(() => document.querySelector("#rawInput").value.indexOf("3307/renamed") >= 0);
    result.steps.urlInJson = document.querySelector("#rawInput").value.indexOf("3307/renamed") >= 0;
    const chip = document.querySelector(".pw-state");
    result.steps.passwordChip = chip ? chip.textContent.trim() : null;

    // 3) 面板范围：全局看得到所有连接，单个只看当前连接
    document.querySelector('[data-seg="scope"] [data-value="all"]').click();
    await until(() => document.querySelector("#rawInput").value.indexOf("beta") >= 0);
    result.steps.scopeAllHasAlpha = document.querySelector("#rawInput").value.indexOf("alpha") >= 0;
    result.steps.scopeAllHasBeta = document.querySelector("#rawInput").value.indexOf("beta") >= 0;
    document.querySelector('[data-seg="scope"] [data-value="one"]').click();
    await wait(50);
    result.steps.scopeOneHasAlpha = document.querySelector("#rawInput").value.indexOf("alpha") >= 0;
    result.steps.scopeOneHasBeta = document.querySelector("#rawInput").value.indexOf("beta") >= 0;

  } catch (error) {
    result.errors.push("harness: " + (error && error.message ? error.message : String(error)));
  }
  await fetch("/__selftest", { method: "POST", body: JSON.stringify(result) });
})();
</script>"##;

fn find_chrome() -> Option<PathBuf> {
    CHROME_CANDIDATES
        .iter()
        .map(PathBuf::from)
        .find(|path| path.is_file())
}

/// 从请求头里取 Content-Length，方便把 body 读全。
fn content_length(head: &str) -> usize {
    head.lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            if name.trim().eq_ignore_ascii_case("content-length") {
                value.trim().parse::<usize>().ok()
            } else {
                None
            }
        })
        .unwrap_or(0)
}

/// 只有 `GET /` 换成本地注入过自检脚本的页面，`POST /__selftest` 存结果，其余请求转发。
async fn run_test_proxy(
    listener: TcpListener,
    target_port: u16,
    html: String,
    result: Arc<Mutex<Option<String>>>,
    hits: Arc<Mutex<Vec<String>>>,
) {
    loop {
        let Ok((mut client, _)) = listener.accept().await else {
            continue;
        };
        let html = html.clone();
        let result = Arc::clone(&result);
        let hits = Arc::clone(&hits);
        tokio::spawn(async move {
            let mut buffer = vec![0u8; 32 * 1024];
            let Ok(read) = client.read(&mut buffer).await else {
                return;
            };
            if read == 0 {
                return;
            }
            let head = String::from_utf8_lossy(&buffer[..read]).to_string();
            let request_line = head.lines().next().unwrap_or("").to_string();
            if let Ok(mut log) = hits.try_lock() {
                log.push(request_line.clone());
            }

            if request_line.starts_with(&format!("POST {SELFTEST_PATH}")) {
                let mut body = head
                    .split_once("\r\n\r\n")
                    .map(|(_, rest)| rest.to_string())
                    .unwrap_or_default();
                let expected = content_length(&head);
                while body.len() < expected {
                    let mut more = vec![0u8; 8192];
                    match client.read(&mut more).await {
                        Ok(0) | Err(_) => break,
                        Ok(size) => body.push_str(&String::from_utf8_lossy(&more[..size])),
                    }
                }
                *result.lock().await = Some(body);
                let response =
                    "HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok";
                let _ = client.write_all(response.as_bytes()).await;
                let _ = client.shutdown().await;
                return;
            }

            if request_line.starts_with("GET / ") || request_line.starts_with("GET /index.html") {
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    html.len(),
                    html
                );
                let _ = client.write_all(response.as_bytes()).await;
                let _ = client.shutdown().await;
                return;
            }

            let Ok(mut upstream) = TcpStream::connect(("127.0.0.1", target_port)).await else {
                return;
            };
            if upstream.write_all(&buffer[..read]).await.is_err() {
                return;
            }
            let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
        });
    }
}

/// 启动编辑服务 + 注入脚本的代理，返回（页面地址前缀, 访问令牌, 结果槽）。
async fn start_stack(
    config_path: &Path,
) -> Result<(
    String,
    String,
    Arc<Mutex<Option<String>>>,
    Arc<Mutex<Vec<String>>>,
)> {
    let token = generate_token();
    let server_port = {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let port = listener.local_addr()?.port();
        drop(listener);
        port
    };
    let config = config_path.to_path_buf();
    let server_token = token.clone();
    tokio::spawn(async move {
        let _ = server::run_editor_server(server_port, Some(server_token), Some(config)).await;
    });

    // 等状态文件出现，确认服务已经绑定端口
    let mut bound_port = 0u16;
    for _ in 0..100 {
        if let Ok(Some(state)) = crate::editor::control::read_state() {
            if state.token == token {
                bound_port = state.port;
                break;
            }
        }
        sleep(Duration::from_millis(50)).await;
    }
    if bound_port == 0 {
        return Err(anyhow!("编辑服务未在超时时间内启动"));
    }

    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let proxy_port = listener.local_addr()?.port();
    let html = server::INDEX_HTML.replacen("</body>", &format!("{HARNESS}</body>"), 1);
    let result = Arc::new(Mutex::new(None));
    let hits = Arc::new(Mutex::new(Vec::new()));
    tokio::spawn(run_test_proxy(
        listener,
        bound_port,
        html,
        Arc::clone(&result),
        Arc::clone(&hits),
    ));

    Ok((
        format!("http://127.0.0.1:{proxy_port}/"),
        token,
        result,
        hits,
    ))
}

#[tokio::test]
async fn page_smoke_covers_url_secret_and_scope() -> Result<()> {
    let Some(chrome) = find_chrome() else {
        eprintln!("未找到 Chrome，跳过编辑页面的浏览器 smoke 测试");
        return Ok(());
    };
    // 状态目录是进程级环境变量，和空闲测试串行执行，避免互相覆盖
    let _guard = crate::editor::state_env_lock().lock().await;

    let dir = TempDir::new("editor-smoke");
    let dir_path = dir.path();
    let config_path = dir_path.join("config.json");
    // 两个连接：alpha 带密文（用真实的加密函数生成），beta 用于验证面板范围
    crate::secrets::encrypt_secret(&config_path, "agentdbcli:alpha:url", "s3cret-pw")
        .expect("写入密文失败");
    std::fs::write(
        &config_path,
        r#"{
  "databases": {
    "alpha": { "type": "mysql", "url": "mysql://root:@127.0.0.1:3306/a", "passwordRef": "agentdbcli:alpha:url", "readonly": true },
    "beta": { "type": "redis", "url": "redis://127.0.0.1:6379", "readonly": true }
  }
}
"#,
    )
    .expect("写入配置失败");

    let state_dir = dir_path.join("state");
    std::fs::create_dir_all(&state_dir).expect("创建状态目录失败");
    std::env::set_var(STATE_DIR_ENV, &state_dir);

    let (base, token, result, hits) = start_stack(&config_path).await?;
    let mut chrome_process = tokio::process::Command::new(&chrome)
        .args([
            "--headless=new",
            "--disable-gpu",
            "--disable-extensions",
            "--no-first-run",
            "--no-default-browser-check",
        ])
        .arg(format!(
            "--user-data-dir={}",
            dir_path.join("chrome-profile").display()
        ))
        .arg(format!("{base}#t={token}"))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::from(
            std::fs::File::create(dir_path.join("chrome.log")).expect("创建 Chrome 日志失败"),
        ))
        .spawn()
        .context("启动 headless Chrome 失败")?;

    // 等自检脚本回传结果
    let mut payload = None;
    for _ in 0..150 {
        if let Some(body) = result.lock().await.clone() {
            payload = Some(body);
            break;
        }
        sleep(Duration::from_millis(200)).await;
    }
    let state_ok = state_path()?.starts_with(&state_dir);
    let _ = chrome_process.kill().await;
    // 环境变量是进程级的，用完立刻还原，避免影响同一进程里的其它用例
    std::env::remove_var(STATE_DIR_ENV);
    let seen = hits.lock().await.clone();

    let payload = payload.ok_or_else(|| {
        let log = std::fs::read_to_string(dir_path.join("chrome.log")).unwrap_or_default();
        anyhow!(
            "等待页面自检结果超时，代理收到: {:?}；Chrome 日志: {}",
            seen,
            log.chars().take(400).collect::<String>()
        )
    })?;
    let parsed: Value = serde_json::from_str(&payload).context("自检结果不是合法 JSON")?;

    assert_eq!(parsed["errors"], json!([]), "页面报错: {parsed}");
    assert_eq!(
        parsed["steps"]["rows"],
        json!(true),
        "连接列表没渲染出来: {parsed}"
    );
    assert_eq!(
        parsed["steps"]["rowCount"],
        json!(2),
        "连接数量不对: {parsed}"
    );
    assert_eq!(
        parsed["steps"]["secretValue"],
        json!("s3cret-pw"),
        "眼睛没有解密出明文: {parsed}"
    );
    assert_eq!(
        parsed["steps"]["secretAttr"],
        json!(null),
        "明文不该出现在 value 属性里: {parsed}"
    );
    assert_eq!(
        parsed["steps"]["jsonHasSecret"],
        json!(false),
        "明文不该写回 JSON: {parsed}"
    );
    assert_eq!(
        parsed["steps"]["urlInJson"],
        json!(true),
        "URL 改动没有同步到 JSON: {parsed}"
    );
    assert_eq!(
        parsed["steps"]["passwordChip"],
        json!("明文"),
        "密码状态没有跟上: {parsed}"
    );
    assert_eq!(
        parsed["steps"]["scopeAllHasAlpha"],
        json!(true),
        "全局视图缺少 alpha"
    );
    assert_eq!(
        parsed["steps"]["scopeAllHasBeta"],
        json!(true),
        "全局视图缺少 beta"
    );
    assert_eq!(
        parsed["steps"]["scopeOneHasAlpha"],
        json!(true),
        "单连接视图缺少 alpha"
    );
    assert_eq!(
        parsed["steps"]["scopeOneHasBeta"],
        json!(false),
        "单连接视图混入了 beta"
    );
    let offset = parsed["steps"]["maxIconTextOffset"]
        .as_f64()
        .unwrap_or(f64::MAX);
    assert!(
        offset <= 1.0,
        "图标与文字没有垂直居中，最大偏差 {offset}px: {parsed}"
    );
    assert!(state_ok, "测试期内状态文件应写在临时目录");
    Ok(())
}
