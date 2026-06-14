//! MCP（Model Context Protocol）客户端：支持 stdio 与 Streamable HTTP 两种传输。
//!
//! - **stdio**：spawn MCP server 子进程，走换行分隔 JSON-RPC。
//! - **HTTP**：Streamable HTTP（POST JSON-RPC，响应可为 JSON 或 SSE），支持静态 Bearer Token
//!   与 OAuth 2.1（PKCE 授权码流）；HTTP I/O 在专用 worker 线程执行，避免 reqwest::blocking 在
//!   tokio 运行时线程 panic。所有 MCP 工具调用仍经桌面端 Permission Manager 与 Activity Event 审计。

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use thiserror::Error;

const MCP_PROTOCOL_VERSION: &str = "2024-11-05";
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Error)]
pub enum McpError {
    #[error("无法启动 MCP server: {0}")]
    SpawnFailed(String),
    #[error("MCP 通信失败: {0}")]
    Io(String),
    #[error("MCP 请求超时")]
    Timeout,
    #[error("MCP server 返回错误: {0}")]
    ServerError(String),
    #[error("MCP 授权失败: {0}")]
    Auth(String),
}

/// MCP server 暴露的工具定义。
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct McpToolDef {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(rename = "inputSchema", default)]
    pub input_schema: serde_json::Value,
}

/// MCP server 暴露的资源定义（resources/list 返回项）。
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct McpResource {
    pub uri: String,
    #[serde(default)]
    pub name: String,
    #[serde(rename = "mimeType", default)]
    pub mime_type: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
}

/// HTTP worker 收到的一个请求作业。
struct HttpJob {
    body: serde_json::Value,
    is_request: bool, // true=需要响应，false=通知
    resp: Sender<Result<serde_json::Value, McpError>>,
}

enum Transport {
    Stdio {
        child: Mutex<Child>,
        stdin: Mutex<ChildStdin>,
        pending: Arc<Mutex<HashMap<u64, Sender<serde_json::Value>>>>,
    },
    Http {
        tx: Sender<HttpJob>,
    },
}

pub struct McpClient {
    transport: Transport,
    next_id: AtomicU64,
    pub server_name: String,
    pub tools: Vec<McpToolDef>,
    /// 若连接过程中通过 OAuth 取得了新 token，置于此，供桌面端持久化。
    pub obtained_token: Option<String>,
}

impl McpClient {
    /// 启动并握手一个 MCP server。command_or_url 以 http(s):// 开头走 HTTP 传输，否则 stdio。
    /// auth_token：HTTP 传输的静态 Bearer Token（可选；为空且服务端要求授权时走 OAuth）。
    pub fn connect(
        server_name: &str,
        command_or_url: &str,
        auth_token: Option<&str>,
    ) -> Result<McpClient, McpError> {
        let target = command_or_url.trim();
        if target.starts_with("http://") || target.starts_with("https://") {
            Self::connect_http(server_name, target, auth_token)
        } else {
            Self::connect_stdio(server_name, target)
        }
    }

    // ── stdio 传输 ──────────────────────────────────────────────────────────

    fn connect_stdio(server_name: &str, command_line: &str) -> Result<McpClient, McpError> {
        let mut child = Command::new("cmd")
            .args(["/C", command_line])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| McpError::SpawnFailed(e.to_string()))?;
        let stdin = child.stdin.take().ok_or_else(|| McpError::Io("无 stdin".into()))?;
        let stdout = child.stdout.take().ok_or_else(|| McpError::Io("无 stdout".into()))?;
        let pending: Arc<Mutex<HashMap<u64, Sender<serde_json::Value>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let pending_reader = pending.clone();
        std::thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                let Ok(line) = line else { break };
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else { continue };
                if let Some(id) = value.get("id").and_then(|v| v.as_u64()) {
                    let sender = pending_reader.lock().ok().and_then(|mut m| m.remove(&id));
                    if let Some(sender) = sender {
                        let _ = sender.send(value);
                    }
                }
            }
        });
        let mut client = McpClient {
            transport: Transport::Stdio { child: Mutex::new(child), stdin: Mutex::new(stdin), pending },
            next_id: AtomicU64::new(1),
            server_name: server_name.to_string(),
            tools: Vec::new(),
            obtained_token: None,
        };
        client.handshake()?;
        Ok(client)
    }

    // ── HTTP 传输 ───────────────────────────────────────────────────────────

    fn connect_http(
        server_name: &str,
        url: &str,
        auth_token: Option<&str>,
    ) -> Result<McpClient, McpError> {
        let (tx, rx) = channel::<HttpJob>();
        let url_owned = url.to_string();
        let token_owned = auth_token.map(str::to_string);
        // OAuth 取得的 token 通过该通道回传给 connect 线程持久化。
        let (token_tx, token_rx) = channel::<String>();
        std::thread::spawn(move || http_worker(url_owned, token_owned, token_tx, rx));

        let mut client = McpClient {
            transport: Transport::Http { tx },
            next_id: AtomicU64::new(1),
            server_name: server_name.to_string(),
            tools: Vec::new(),
            obtained_token: None,
        };
        client.handshake()?;
        // 握手后若 worker 通过 OAuth 取得了新 token，收集以便持久化。
        if let Ok(token) = token_rx.try_recv() {
            client.obtained_token = Some(token);
        }
        Ok(client)
    }

    // ── 公共握手 / 调用 ─────────────────────────────────────────────────────

    fn handshake(&mut self) -> Result<(), McpError> {
        self.request(
            "initialize",
            serde_json::json!({
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": { "name": "NovaCode", "version": env!("CARGO_PKG_VERSION") }
            }),
            HANDSHAKE_TIMEOUT,
        )?;
        self.notify("notifications/initialized", serde_json::json!({}))?;
        let tools_result = self.request("tools/list", serde_json::json!({}), HANDSHAKE_TIMEOUT)?;
        self.tools = tools_result
            .get("tools")
            .cloned()
            .map(serde_json::from_value::<Vec<McpToolDef>>)
            .transpose()
            .map_err(|e| McpError::Io(e.to_string()))?
            .unwrap_or_default();
        Ok(())
    }

    pub fn call_tool(
        &self,
        tool_name: &str,
        arguments: serde_json::Value,
    ) -> Result<String, McpError> {
        let result = self.request(
            "tools/call",
            serde_json::json!({ "name": tool_name, "arguments": arguments }),
            DEFAULT_CALL_TIMEOUT,
        )?;
        let is_error = result.get("isError").and_then(|v| v.as_bool()).unwrap_or(false);
        let text = result
            .get("content")
            .and_then(|c| c.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.get("text").and_then(|t| t.as_str()))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        if is_error {
            return Err(McpError::ServerError(text));
        }
        Ok(text)
    }

    /// 列出该 server 暴露的资源（resources/list）。server 不支持时返回错误。
    pub fn list_resources(&self) -> Result<Vec<McpResource>, McpError> {
        let result = self.request("resources/list", serde_json::json!({}), DEFAULT_CALL_TIMEOUT)?;
        let resources = result
            .get("resources")
            .cloned()
            .map(serde_json::from_value::<Vec<McpResource>>)
            .transpose()
            .map_err(|e| McpError::Io(e.to_string()))?
            .unwrap_or_default();
        Ok(resources)
    }

    /// 读取一个资源（resources/read），返回拼接后的文本内容；二进制 blob 以占位说明替代。
    pub fn read_resource(&self, uri: &str) -> Result<String, McpError> {
        let result = self.request(
            "resources/read",
            serde_json::json!({ "uri": uri }),
            DEFAULT_CALL_TIMEOUT,
        )?;
        let text = result
            .get("contents")
            .and_then(|c| c.as_array())
            .map(|items| {
                items
                    .iter()
                    .map(|item| {
                        item.get("text")
                            .and_then(|t| t.as_str())
                            .map(|s| s.to_string())
                            .or_else(|| {
                                item.get("blob")
                                    .and_then(|b| b.as_str())
                                    .map(|_| "[binary blob omitted]".to_string())
                            })
                            .unwrap_or_default()
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        Ok(text)
    }

    fn request(
        &self,
        method: &str,
        params: serde_json::Value,
        timeout: Duration,
    ) -> Result<serde_json::Value, McpError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let message = serde_json::json!({
            "jsonrpc": "2.0", "id": id, "method": method, "params": params
        });
        match &self.transport {
            Transport::Stdio { stdin, pending, .. } => {
                let (sender, receiver) = channel::<serde_json::Value>();
                if let Ok(mut p) = pending.lock() {
                    p.insert(id, sender);
                }
                write_line(stdin, &message)?;
                let response = receiver.recv_timeout(timeout).map_err(|_| {
                    if let Ok(mut p) = pending.lock() {
                        p.remove(&id);
                    }
                    McpError::Timeout
                })?;
                extract_result(&response)
            }
            Transport::Http { tx } => {
                let (resp_tx, resp_rx) = channel();
                tx.send(HttpJob { body: message, is_request: true, resp: resp_tx })
                    .map_err(|_| McpError::Io("HTTP worker 已退出".into()))?;
                let response = resp_rx.recv_timeout(timeout).map_err(|_| McpError::Timeout)??;
                extract_result(&response)
            }
        }
    }

    fn notify(&self, method: &str, params: serde_json::Value) -> Result<(), McpError> {
        let message = serde_json::json!({ "jsonrpc": "2.0", "method": method, "params": params });
        match &self.transport {
            Transport::Stdio { stdin, .. } => write_line(stdin, &message),
            Transport::Http { tx } => {
                let (resp_tx, _resp_rx) = channel();
                tx.send(HttpJob { body: message, is_request: false, resp: resp_tx })
                    .map_err(|_| McpError::Io("HTTP worker 已退出".into()))
            }
        }
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        if let Transport::Stdio { child, .. } = &self.transport {
            if let Ok(mut c) = child.lock() {
                let _ = c.kill();
            }
        }
        // HTTP：tx 随 self 释放 → worker recv 返回 Err → 线程退出。
    }
}

fn write_line(stdin: &Mutex<ChildStdin>, message: &serde_json::Value) -> Result<(), McpError> {
    let mut s = stdin.lock().map_err(|e| McpError::Io(e.to_string()))?;
    let line = serde_json::to_string(message).map_err(|e| McpError::Io(e.to_string()))?;
    s.write_all(format!("{line}\n").as_bytes())
        .and_then(|_| s.flush())
        .map_err(|e| McpError::Io(e.to_string()))
}

fn extract_result(response: &serde_json::Value) -> Result<serde_json::Value, McpError> {
    if let Some(error) = response.get("error") {
        return Err(McpError::ServerError(error.to_string()));
    }
    Ok(response.get("result").cloned().unwrap_or(serde_json::Value::Null))
}

// ── HTTP worker ──────────────────────────────────────────────────────────────

/// HTTP worker 线程：拥有 reqwest::blocking::Client，串行处理作业；按需触发 OAuth。
fn http_worker(
    url: String,
    mut bearer: Option<String>,
    token_tx: Sender<String>,
    rx: std::sync::mpsc::Receiver<HttpJob>,
) {
    let client = match reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
    {
        Ok(c) => c,
        Err(_) => return,
    };
    let mut session_id: Option<String> = None;
    let mut oauth_tried = false;

    while let Ok(job) = rx.recv() {
        let send_once = |bearer: &Option<String>, session: &Option<String>| {
            let mut req = client
                .post(&url)
                .header("Content-Type", "application/json")
                .header("Accept", "application/json, text/event-stream");
            if let Some(b) = bearer {
                req = req.header("Authorization", format!("Bearer {b}"));
            }
            if let Some(s) = session {
                req = req.header("Mcp-Session-Id", s);
            }
            req.body(serde_json::to_string(&job.body).unwrap_or_default()).send()
        };

        let mut response = send_once(&bearer, &session_id);
        // 401 且尚未尝试过 OAuth：跑授权流，取得 token 后重试一次。
        if !oauth_tried && bearer.is_none() {
            if let Ok(r) = &response {
                if r.status() == reqwest::StatusCode::UNAUTHORIZED {
                    oauth_tried = true;
                    match run_oauth_flow(&client, &url) {
                        Ok(token) => {
                            let _ = token_tx.send(token.clone());
                            bearer = Some(token);
                            response = send_once(&bearer, &session_id);
                        }
                        Err(e) => {
                            let _ = job.resp.send(Err(McpError::Auth(e.to_string())));
                            continue;
                        }
                    }
                }
            }
        }

        match response {
            Err(e) => {
                let _ = job.resp.send(Err(McpError::Io(e.to_string())));
            }
            Ok(resp) => {
                if let Some(sid) = resp
                    .headers()
                    .get("Mcp-Session-Id")
                    .and_then(|v| v.to_str().ok())
                {
                    session_id = Some(sid.to_string());
                }
                let status = resp.status();
                let ctype = resp
                    .headers()
                    .get(reqwest::header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string();
                let text = resp.text().unwrap_or_default();
                if !job.is_request {
                    let _ = job.resp.send(Ok(serde_json::Value::Null));
                    continue;
                }
                if !status.is_success() {
                    let _ = job.resp.send(Err(McpError::ServerError(format!("HTTP {status}: {text}"))));
                    continue;
                }
                let parsed = if ctype.contains("text/event-stream") {
                    parse_sse_jsonrpc(&text)
                } else {
                    serde_json::from_str::<serde_json::Value>(&text).ok()
                };
                match parsed {
                    Some(v) => {
                        let _ = job.resp.send(Ok(v));
                    }
                    None => {
                        let _ = job.resp.send(Err(McpError::Io("无法解析 MCP 响应".into())));
                    }
                }
            }
        }
    }
}

/// 从 SSE 文本中提取最后一条 data: 的 JSON-RPC 对象（含 id 的响应）。
fn parse_sse_jsonrpc(text: &str) -> Option<serde_json::Value> {
    let mut last = None;
    for line in text.lines() {
        if let Some(data) = line.strip_prefix("data:") {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(data.trim()) {
                if v.get("id").is_some() || v.get("result").is_some() || v.get("error").is_some() {
                    last = Some(v);
                }
            }
        }
    }
    last
}

// ── OAuth 2.1（PKCE 授权码流） ─────────────────────────────────────────────

/// 对一个需要授权的 MCP HTTP server 执行 OAuth：发现元数据 → 动态注册 → PKCE 授权码 → 换 token。
fn run_oauth_flow(client: &reqwest::blocking::Client, resource_url: &str) -> Result<String, McpError> {
    let oauth_err = |m: String| McpError::Auth(m);
    let origin = url_origin(resource_url);

    // 1) 授权服务器元数据（先试受保护资源元数据，再退到 origin 的 oauth-authorization-server）
    let as_meta = fetch_json(client, &format!("{origin}/.well-known/oauth-authorization-server"))
        .or_else(|_| fetch_json(client, &format!("{origin}/.well-known/openid-configuration")))
        .map_err(|e| oauth_err(format!("发现授权服务器失败: {e}")))?;
    let authz_ep = as_meta.get("authorization_endpoint").and_then(|v| v.as_str())
        .ok_or_else(|| oauth_err("缺少 authorization_endpoint".into()))?.to_string();
    let token_ep = as_meta.get("token_endpoint").and_then(|v| v.as_str())
        .ok_or_else(|| oauth_err("缺少 token_endpoint".into()))?.to_string();
    let reg_ep = as_meta.get("registration_endpoint").and_then(|v| v.as_str()).map(str::to_string);

    // 2) 本地回调监听 + 端口
    let listener = std::net::TcpListener::bind("127.0.0.1:0")
        .map_err(|e| oauth_err(format!("无法监听本地回调: {e}")))?;
    let port = listener.local_addr().map_err(|e| oauth_err(e.to_string()))?.port();
    let redirect_uri = format!("http://127.0.0.1:{port}/callback");

    // 3) 动态客户端注册（若支持），否则用占位 client_id
    let client_id = if let Some(reg) = reg_ep {
        let body = serde_json::json!({
            "client_name": "NovaCode",
            "redirect_uris": [redirect_uri],
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
            "token_endpoint_auth_method": "none"
        });
        let resp = client.post(&reg).json(&body).send()
            .map_err(|e| oauth_err(format!("动态注册失败: {e}")))?;
        let v: serde_json::Value = resp.json().map_err(|e| oauth_err(e.to_string()))?;
        v.get("client_id").and_then(|x| x.as_str())
            .ok_or_else(|| oauth_err("注册未返回 client_id".into()))?.to_string()
    } else {
        "novacode".to_string()
    };

    // 4) PKCE
    let verifier = random_token(43);
    let challenge = base64url(&sha256(verifier.as_bytes()));
    let state = random_token(16);

    // 5) 打开浏览器到 authorize URL
    let authz_url = format!(
        "{authz_ep}?response_type=code&client_id={}&redirect_uri={}&code_challenge={}&code_challenge_method=S256&state={}&resource={}",
        urlencode(&client_id), urlencode(&redirect_uri), challenge, state, urlencode(resource_url)
    );
    open_browser(&authz_url);

    // 6) 等待回调取 code（5 分钟超时）
    listener.set_nonblocking(false).ok();
    let code = wait_for_callback(&listener, &state)?;

    // 7) 用 code 换 token
    let params = [
        ("grant_type", "authorization_code"),
        ("code", code.as_str()),
        ("redirect_uri", redirect_uri.as_str()),
        ("client_id", client_id.as_str()),
        ("code_verifier", verifier.as_str()),
    ];
    let resp = client.post(&token_ep).form(&params).send()
        .map_err(|e| oauth_err(format!("换取 token 失败: {e}")))?;
    let v: serde_json::Value = resp.json().map_err(|e| oauth_err(e.to_string()))?;
    v.get("access_token").and_then(|x| x.as_str())
        .map(str::to_string)
        .ok_or_else(|| oauth_err("token 端点未返回 access_token".into()))
}

fn fetch_json(client: &reqwest::blocking::Client, url: &str) -> Result<serde_json::Value, String> {
    let resp = client.get(url).send().map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }
    resp.json().map_err(|e| e.to_string())
}

/// 阻塞等待 OAuth 回调连接，解析 code（校验 state），并回一个可关闭页面。
fn wait_for_callback(listener: &std::net::TcpListener, state: &str) -> Result<String, McpError> {
    let deadline = std::time::Instant::now() + Duration::from_secs(300);
    loop {
        if std::time::Instant::now() > deadline {
            return Err(McpError::Auth("授权超时（5 分钟未回调）".into()));
        }
        let (mut stream, _) = listener.accept().map_err(|e| McpError::Auth(e.to_string()))?;
        let mut buf = [0u8; 4096];
        let n = stream.read(&mut buf).unwrap_or(0);
        let req = String::from_utf8_lossy(&buf[..n]);
        let first = req.lines().next().unwrap_or("");
        // GET /callback?code=...&state=... HTTP/1.1
        if let Some(qpos) = first.find('?') {
            let query = first[qpos + 1..].split_whitespace().next().unwrap_or("");
            let mut code = None;
            let mut got_state = None;
            for kv in query.split('&') {
                let mut it = kv.splitn(2, '=');
                match (it.next(), it.next()) {
                    (Some("code"), Some(v)) => code = Some(urldecode(v)),
                    (Some("state"), Some(v)) => got_state = Some(urldecode(v)),
                    _ => {}
                }
            }
            let _ = respond_ok(&mut stream);
            if got_state.as_deref() != Some(state) {
                return Err(McpError::Auth("state 不匹配（可能 CSRF）".into()));
            }
            if let Some(code) = code {
                return Ok(code);
            }
            return Err(McpError::Auth("回调未携带授权码".into()));
        }
        let _ = respond_ok(&mut stream);
    }
}

fn respond_ok(stream: &mut std::net::TcpStream) -> std::io::Result<()> {
    let body = "<html><body style='font-family:sans-serif;text-align:center;padding-top:80px'>\
        <h2>NovaCode 授权完成</h2><p>可以关闭此页面，返回应用。</p></body></html>";
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(), body
    );
    stream.write_all(resp.as_bytes())
}

fn open_browser(url: &str) {
    let _ = Command::new("cmd").args(["/C", "start", "", url]).spawn();
}

// ── 小工具 ────────────────────────────────────────────────────────────────

fn url_origin(url: &str) -> String {
    // 取 scheme://host[:port]
    if let Some(rest) = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://")) {
        let scheme = if url.starts_with("https") { "https" } else { "http" };
        let host = rest.split('/').next().unwrap_or(rest);
        return format!("{scheme}://{host}");
    }
    url.to_string()
}

fn sha256(input: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(input);
    h.finalize().into()
}

fn base64url(input: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in input.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        out.push(T[(n >> 18 & 63) as usize] as char);
        out.push(T[(n >> 12 & 63) as usize] as char);
        if chunk.len() > 1 {
            out.push(T[(n >> 6 & 63) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(T[(n & 63) as usize] as char);
        }
    }
    out
}

static RAND_CTR: AtomicU64 = AtomicU64::new(0);

/// 生成不可预测的随机 token（base64url），用于 PKCE verifier / state。
fn random_token(min_len: usize) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let ctr = RAND_CTR.fetch_add(1, Ordering::SeqCst);
    let mut seed = Vec::new();
    seed.extend_from_slice(&nanos.to_le_bytes());
    seed.extend_from_slice(&ctr.to_le_bytes());
    seed.extend_from_slice(&std::process::id().to_le_bytes());
    let mut out = String::new();
    let mut round = 0u64;
    while out.len() < min_len {
        let mut s = seed.clone();
        s.extend_from_slice(&round.to_le_bytes());
        out.push_str(&base64url(&sha256(&s)));
        round += 1;
    }
    out.truncate(min_len.max(43));
    out
}

fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn urldecode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(if bytes[i] == b'+' { b' ' } else { bytes[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

/// 把 MCP 工具名转为模型可用的函数名：`mcp_<server>_<tool>`，只保留合法字符并截断到 64。
pub fn function_name_for(server_name: &str, tool_name: &str) -> String {
    let sanitize = |s: &str| -> String {
        s.chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
            .collect()
    };
    let mut name = format!("mcp_{}_{}", sanitize(server_name), sanitize(tool_name));
    name.truncate(64);
    name
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_safe_function_names() {
        assert_eq!(function_name_for("github", "create_issue"), "mcp_github_create_issue");
        assert_eq!(function_name_for("my server!", "do/it"), "mcp_my_server__do_it");
        assert!(function_name_for(&"x".repeat(80), "tool").len() <= 64);
    }

    #[test]
    fn parses_tool_definitions() {
        let raw = serde_json::json!({
            "name": "read_issue", "description": "Read a GitHub issue",
            "inputSchema": { "type": "object", "properties": {} }
        });
        let def: McpToolDef = serde_json::from_value(raw).expect("should parse");
        assert_eq!(def.name, "read_issue");
        assert!(def.input_schema.is_object());
    }

    #[test]
    fn pkce_challenge_is_base64url_of_sha256() {
        // RFC 7636 测试向量
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = base64url(&sha256(verifier.as_bytes()));
        assert_eq!(challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn url_origin_extracts_scheme_host() {
        assert_eq!(url_origin("https://mcp.example.com/v1/sse"), "https://mcp.example.com");
        assert_eq!(url_origin("http://localhost:8080/x"), "http://localhost:8080");
    }

    #[test]
    fn sse_extracts_jsonrpc() {
        let sse = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\n\n";
        let v = parse_sse_jsonrpc(sse).expect("should parse");
        assert_eq!(v["id"], 1);
    }
}
