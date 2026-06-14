//! MCP（Model Context Protocol）集成：工具绑定收集、后台连接、外部工具/资源调用、注册新 server。
//!
//! 从 main.rs 抽出（Plan16 阶段2）：纯函数搬移，无行为变更。

use crate::state::AppState;
use novacode_mcp_client::{function_name_for, McpClient};
use novacode_storage::{add_mcp_server, McpServerRecord};
use std::sync::Arc;
use tauri::{AppHandle, Emitter, Manager};

/// MCP 工具与模型函数名的绑定关系，按 send 周期收集后传入工具循环。
#[derive(Clone)]
pub(crate) struct McpBinding {
    pub(crate) fn_name: String,
    pub(crate) server_id: String,
    pub(crate) tool_name: String,
    pub(crate) schema: serde_json::Value,
}

/// 收集所有已连接 MCP server 的工具绑定（函数名、调度信息与 schema）。
pub(crate) fn collect_mcp_bindings(app: &AppHandle) -> Vec<McpBinding> {
    let state = app.state::<AppState>();
    let guard = state.mcp.lock();
    let Ok(map) = guard else {
        return Vec::new();
    };
    let mut bindings = Vec::new();
    for (server_id, client) in map.iter() {
        for tool in &client.tools {
            let fn_name = function_name_for(&client.server_name, &tool.name);
            let parameters = if tool.input_schema.is_object() {
                tool.input_schema.clone()
            } else {
                serde_json::json!({ "type": "object", "properties": {} })
            };
            bindings.push(McpBinding {
                fn_name: fn_name.clone(),
                server_id: server_id.clone(),
                tool_name: tool.name.clone(),
                schema: serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": fn_name,
                        "description": format!("[MCP:{}] {}", client.server_name, tool.description),
                        "parameters": parameters
                    }
                }),
            });
        }
    }
    bindings
}

/// 后台连接一个 MCP server；成功后放入 AppState 并 emit 状态事件。
pub(crate) fn spawn_mcp_connect(app: &AppHandle, record: McpServerRecord) {
    let app = app.clone();
    std::thread::spawn(move || {
        let result = McpClient::connect(&record.name, &record.command, record.auth_token.as_deref());
        match result {
            Ok(client) => {
                let tool_count = client.tools.len();
                let state = app.state::<AppState>();
                // OAuth 取得的新 token：持久化到该 server 记录，下次连接直接复用。
                if let Some(token) = client.obtained_token.clone() {
                    if let Ok(db) = state.db.lock() {
                        let _ = novacode_storage::set_mcp_server_token(&db, &record.id, &token);
                    }
                }
                if let Ok(mut map) = state.mcp.lock() {
                    map.insert(record.id.clone(), Arc::new(client));
                }
                let _ = app.emit(
                    "mcp-status",
                    serde_json::json!({ "id": record.id, "status": "connected", "toolCount": tool_count }),
                );
            }
            Err(err) => {
                let _ = app.emit(
                    "mcp-status",
                    serde_json::json!({ "id": record.id, "status": "failed", "error": err.to_string() }),
                );
            }
        }
    });
}

/// add_mcp_server 工具：让 Agent 用 NovaCode 真正的机制注册并连接一个 MCP 服务器
/// （写入 SQLite 服务器表 + 后台连接），而不是去编辑无效的配置文件。
pub(crate) fn execute_add_mcp_server(app: &AppHandle, arguments: &str) -> Result<serde_json::Value, String> {
    let parsed: serde_json::Value =
        serde_json::from_str(arguments).map_err(|e| format!("工具参数解析失败: {e}"))?;
    let name = parsed.get("name").and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty())
        .ok_or("add_mcp_server 缺少 name")?;
    let command = parsed.get("command").and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty())
        .ok_or("add_mcp_server 缺少 command（stdio 启动命令或 http(s):// URL）")?;
    let token = parsed.get("authToken").and_then(|v| v.as_str()).map(str::trim).filter(|t| !t.is_empty());
    let record = {
        let state = app.state::<AppState>();
        let db = state.db.lock().map_err(|e| e.to_string())?;
        add_mcp_server(&db, name, command, token).map_err(|e| e.to_string())?
    };
    let server_id = record.id.clone();
    spawn_mcp_connect(app, record);
    Ok(serde_json::json!({
        "serverId": server_id,
        "note": format!("MCP 服务器 {name} 已注册并正在后台连接。连接成功后其工具会以 mcp_{name}_<工具名> 形式出现，可直接调用。")
    }))
}

/// 执行一次 MCP 外部工具调用：经已连接客户端转发，结果文本截断后回灌模型。
pub(crate) fn execute_mcp_tool(
    app: &AppHandle,
    binding: &McpBinding,
    arguments: &str,
) -> Result<serde_json::Value, String> {
    let args: serde_json::Value =
        serde_json::from_str(arguments).unwrap_or(serde_json::json!({}));
    let client = {
        let state = app.state::<AppState>();
        let guard = state.mcp.lock().map_err(|e| e.to_string())?;
        guard.get(&binding.server_id).cloned()
    };
    let client = client.ok_or("MCP server 未连接，请在设置中检查其状态")?;
    let text = client
        .call_tool(&binding.tool_name, args)
        .map_err(|e| e.to_string())?;
    let capped: String = text.chars().take(32_000).collect();
    Ok(serde_json::json!({ "content": capped }))
}

/// MCP 资源工具：list_mcp_resources / read_mcp_resource，从 AppState 已连接客户端读取。
pub(crate) fn execute_mcp_resource_tool(
    app: &AppHandle,
    tool_name: &str,
    arguments: &str,
) -> Result<serde_json::Value, String> {
    let parsed: serde_json::Value = serde_json::from_str(arguments).unwrap_or(serde_json::json!({}));
    // 克隆 (name, Arc<McpClient>) 到锁外，避免持锁阻塞期间调用网络。
    let clients: Vec<Arc<McpClient>> = {
        let st = app.state::<AppState>();
        let guard = st.mcp.lock().map_err(|e| e.to_string())?;
        guard.values().cloned().collect()
    };
    match tool_name {
        "list_mcp_resources" => {
            let filter = parsed.get("server").and_then(|v| v.as_str());
            let mut out = Vec::new();
            for client in &clients {
                if let Some(f) = filter {
                    if client.server_name != f {
                        continue;
                    }
                }
                // server 不支持 resources 时返回错误，跳过即可。
                if let Ok(resources) = client.list_resources() {
                    for r in resources {
                        out.push(serde_json::json!({
                            "server": client.server_name,
                            "uri": r.uri,
                            "name": r.name,
                            "mimeType": r.mime_type,
                            "description": r.description,
                        }));
                    }
                }
            }
            Ok(serde_json::json!({ "resources": out }))
        }
        "read_mcp_resource" => {
            let server = parsed
                .get("server")
                .and_then(|v| v.as_str())
                .ok_or("read_mcp_resource 缺少 server")?;
            let uri = parsed
                .get("uri")
                .and_then(|v| v.as_str())
                .ok_or("read_mcp_resource 缺少 uri")?;
            let client = clients
                .iter()
                .find(|c| c.server_name == server)
                .ok_or_else(|| format!("未找到已连接的 MCP server: {server}"))?;
            let text = client.read_resource(uri).map_err(|e| e.to_string())?;
            Ok(serde_json::json!({ "server": server, "uri": uri, "content": text }))
        }
        other => Err(format!("未知 MCP 资源工具: {other}")),
    }
}
