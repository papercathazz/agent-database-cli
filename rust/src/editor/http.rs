use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader,
};

pub const TOKEN_HEADER: &str = "x-agentdb-editor-token";
pub const HTML_CONTENT_TYPE: &str = "text/html; charset=utf-8";
pub const JSON_CONTENT_TYPE: &str = "application/json; charset=utf-8";
const MAX_HEADER_BYTES: usize = 16 * 1024;
/// 单行上限：请求行或某个请求头超长时直接拒绝，避免本地进程用超长行撑爆内存
const MAX_LINE_BYTES: usize = 8 * 1024;
const MAX_HEADER_COUNT: usize = 64;
const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;

/// 服务端已经读完的请求：只保留路由和鉴权需要的字段。
pub struct Request {
    pub method: String,
    pub path: String,
    pub(crate) headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    pub fn json_body(&self) -> std::result::Result<Value, ApiFailure> {
        if self.body.is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_slice(&self.body)
            .map_err(|error| ApiFailure::bad_request(format!("请求体不是合法 JSON: {error}")))
    }
}

/// 需要回给页面的业务错误，携带 HTTP 状态码。
#[derive(Debug)]
pub struct ApiFailure {
    pub status: u16,
    pub message: String,
}

impl ApiFailure {
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: 400,
            message: message.into(),
        }
    }

    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self {
            status: 401,
            message: message.into(),
        }
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: 404,
            message: message.into(),
        }
    }

    pub fn method_not_allowed(message: impl Into<String>) -> Self {
        Self {
            status: 405,
            message: message.into(),
        }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            status: 500,
            message: message.into(),
        }
    }
}

pub struct HttpResponse {
    pub status: u16,
    content_type: &'static str,
    pub(crate) body: Vec<u8>,
    pub(crate) extra_headers: Vec<(&'static str, &'static str)>,
}

impl HttpResponse {
    pub fn html(body: &str, extra_headers: Vec<(&'static str, &'static str)>) -> Self {
        Self {
            status: 200,
            content_type: HTML_CONTENT_TYPE,
            body: body.as_bytes().to_vec(),
            extra_headers,
        }
    }

    pub fn json(status: u16, payload: &Value) -> Self {
        Self {
            status,
            content_type: JSON_CONTENT_TYPE,
            body: serde_json::to_vec(payload).unwrap_or_else(|_| b"{}".to_vec()),
            extra_headers: Vec::new(),
        }
    }

    pub fn failure(failure: ApiFailure) -> Self {
        Self::json(failure.status, &json!({ "error": failure.message }))
    }
}

/// 拒绝非本机 Host，避免外部站点通过 DNS 重绑定读写本地配置文件。
pub fn is_local_host(request: &Request) -> bool {
    match request.header("host") {
        None => true,
        Some(host) => {
            let hostname = host.rsplit_once(':').map(|(name, _)| name).unwrap_or(host);
            matches!(hostname, "127.0.0.1" | "localhost" | "[::1]")
        }
    }
}

/// 读一行，但最多读 `MAX_LINE_BYTES`，避免单个超长行把内存撑爆。
/// 被截断的行解析时必然失败，调用方直接拒绝即可。
async fn read_line_limited<R>(reader: &mut R, out: &mut String) -> Result<usize>
where
    R: AsyncBufRead + Unpin,
{
    let mut bytes = Vec::new();
    // 显式走 AsyncReadExt::take，避免和 Iterator::take / by_ref 撞名
    let read = tokio::io::AsyncReadExt::take(reader, MAX_LINE_BYTES as u64)
        .read_until(b'\n', &mut bytes)
        .await?;
    if bytes.len() == MAX_LINE_BYTES && !bytes.ends_with(b"\n") {
        // 已经到达单行上限且还没读到换行，视为超长行
        out.clear();
        out.push_str(&"x".repeat(MAX_LINE_BYTES + 1));
        return Ok(read);
    }
    *out = String::from_utf8(bytes).map_err(|_| anyhow!("请求行不是合法 UTF-8"))?;
    Ok(read)
}

pub async fn read_request<S>(stream: S) -> Result<(Request, S)>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    if read_line_limited(&mut reader, &mut line).await? == 0 {
        anyhow::bail!("客户端未发送请求行");
    }
    if line.len() > MAX_LINE_BYTES {
        anyhow::bail!("请求行过长");
    }
    let mut parts = line.trim_end().split(' ');
    let method = parts.next().unwrap_or_default().to_ascii_uppercase();
    let target = parts.next().unwrap_or_default();
    if method.is_empty() || target.is_empty() {
        anyhow::bail!("请求行非法: {}", line.trim_end());
    }
    // query 和 fragment 不参与路由，只保留 path
    let path = target.split('?').next().unwrap_or_default().to_string();

    let mut headers = Vec::new();
    let mut header_bytes = line.len();
    loop {
        let mut header_line = String::new();
        if read_line_limited(&mut reader, &mut header_line).await? == 0 {
            anyhow::bail!("请求头不完整");
        }
        header_bytes += header_line.len();
        if header_line.len() > MAX_LINE_BYTES {
            anyhow::bail!("请求头过长");
        }
        if header_bytes > MAX_HEADER_BYTES {
            anyhow::bail!("请求头过大");
        }
        if header_line.trim_end().is_empty() {
            break;
        }
        if headers.len() >= MAX_HEADER_COUNT {
            anyhow::bail!("请求头数量超出限制");
        }
        let (name, value) = header_line
            .split_once(':')
            .ok_or_else(|| anyhow!("请求头缺少冒号: {}", header_line.trim_end()))?;
        headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
    }

    let content_length = headers
        .iter()
        .find(|(name, _)| name == "content-length")
        .map(|(_, value)| {
            value
                .parse::<usize>()
                .map_err(|_| anyhow!("content-length 非法: {value}"))
        })
        .transpose()?
        .unwrap_or(0);
    if content_length > MAX_BODY_BYTES {
        anyhow::bail!("请求体过大: {content_length} 字节");
    }
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body).await?;
    }
    Ok((
        Request {
            method,
            path,
            headers,
            body,
        },
        reader.into_inner(),
    ))
}

pub async fn write_response<W>(stream: &mut W, response: &HttpResponse) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    let mut head = format!(
        "HTTP/1.1 {} {}\r\ncontent-type: {}\r\ncontent-length: {}\r\ncache-control: no-store\r\nx-content-type-options: nosniff\r\nconnection: close\r\n",
        response.status,
        status_reason(response.status),
        response.content_type,
        response.body.len()
    );
    for (name, value) in &response.extra_headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(&response.body).await?;
    stream.flush().await?;
    // 主动关闭写端，让 CLI 侧的 read_to_end 正常结束
    let _ = stream.shutdown().await;
    Ok(())
}

fn status_reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        500 => "Internal Server Error",
        _ => "Unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(method: &str, path: &str, headers: &[(&str, &str)]) -> Request {
        Request {
            method: method.to_string(),
            path: path.to_string(),
            headers: headers
                .iter()
                .map(|(name, value)| (name.to_string(), value.to_string()))
                .collect(),
            body: Vec::new(),
        }
    }

    #[tokio::test]
    async fn read_request_parses_line_headers_and_body() {
        let (mut client, server) = tokio::io::duplex(4096);
        let body = r#"{"raw":"{}"}"#;
        let raw = format!(
            "POST /api/config?debug=1 HTTP/1.1\r\nHost: 127.0.0.1:9999\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        tokio::spawn(async move {
            client
                .write_all(raw.as_bytes())
                .await
                .expect("写入请求失败");
        });

        let (request, _stream) = read_request(server).await.expect("解析请求失败");
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/api/config");
        assert_eq!(request.header("host"), Some("127.0.0.1:9999"));
        assert_eq!(
            request.json_body().expect("解析 body 失败"),
            json!({ "raw": "{}" })
        );
    }

    #[tokio::test]
    async fn read_request_rejects_oversized_body() {
        let (mut client, server) = tokio::io::duplex(1024);
        let raw = "POST /api/config HTTP/1.1\r\ncontent-length: 999999999\r\n\r\n";
        tokio::spawn(async move {
            let _ = client.write_all(raw.as_bytes()).await;
            // 保持连接不关闭，验证服务端在读取前就拒绝
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        });
        assert!(read_request(server).await.is_err());
    }

    #[tokio::test]
    async fn read_request_rejects_oversized_line() {
        let (mut client, server) = tokio::io::duplex(64 * 1024);
        let raw = format!("GET /{} HTTP/1.1\r\n\r\n", "a".repeat(MAX_LINE_BYTES + 64));
        tokio::spawn(async move {
            let _ = client.write_all(raw.as_bytes()).await;
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        });
        assert!(read_request(server).await.is_err());
    }

    #[test]
    fn only_loopback_host_is_accepted() {
        assert!(is_local_host(&request("GET", "/", &[])));
        assert!(is_local_host(&request(
            "GET",
            "/",
            &[("host", "127.0.0.1:52341")]
        )));
        assert!(is_local_host(&request(
            "GET",
            "/",
            &[("host", "localhost")]
        )));
        assert!(is_local_host(&request(
            "GET",
            "/",
            &[("host", "[::1]:52341")]
        )));
        assert!(!is_local_host(&request(
            "GET",
            "/",
            &[("host", "evil.example.com")]
        )));
        assert!(!is_local_host(&request(
            "GET",
            "/",
            &[("host", "127.0.0.1.evil.example.com")]
        )));
    }
}
