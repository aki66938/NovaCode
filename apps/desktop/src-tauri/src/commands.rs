//! Tauri 命令层：除 send_message（Agent 主流程）外的全部 `#[tauri::command]`，
//! 涵盖会话 CRUD、工作区管理、设置、审批/插话响应、checkpoint 查询、导出/治理、
//! MCP 管理、文档导入与自动更新，以及仅命令用到的小工具。
//!
//! 从 main.rs 抽出（Plan16）：纯代码搬移与可见性调整，无行为变更。

use crate::chat::chat_completion_with_retry;
use crate::checkpoint::apply_checkpoint_revert;
use crate::mcp::spawn_mcp_connect;
use crate::state::AppState;
use novacode_deepseek_client::{detect_api_key_status, get_user_balance, UserBalance};
use novacode_shared::{ApiKeyStatus, PermissionMode};
use novacode_storage::{
    add_mcp_server, add_permission_rule, clear_active_workspace, create_conversation,
    create_conversation_with_workspace, delete_conversation, delete_messages, get_active_workspace,
    get_activity_events, get_conversation, get_messages, list_conversations, list_file_checkpoints,
    list_mcp_servers, list_permission_rules, mark_checkpoint_reverted, remove_mcp_server,
    remove_permission_rule, save_active_workspace, save_message, set_conversation_archived,
    set_conversation_pinned, set_mcp_server_enabled, update_title, ActivityEventRecord,
    Conversation, FileCheckpoint, StoredMessage, Workspace,
};
use std::sync::atomic::Ordering;
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_updater::UpdaterExt;

// ── DeepSeek ──────────────────────────────────────────────────────────────

#[tauri::command]
pub(crate) fn get_deepseek_api_key_status() -> ApiKeyStatus {
    detect_api_key_status(|name| std::env::var(name).ok())
}

// ── 会话管理 ──────────────────────────────────────────────────────────────

/// 创建新会话，初始标题为"新对话"。
#[tauri::command]
pub(crate) fn new_conversation(state: State<AppState>) -> Result<Conversation, String> {
    let db = state.db.lock().map_err(|e| e.to_string())?;
    create_conversation(&db).map_err(|e| e.to_string())
}

/// 创建新会话，并将会话绑定到创建时选择的工作区快照。
///
/// 输入可选工作区路径；路径存在时校验目录并写入 conversation snapshot，未传路径时创建纯聊天会话。
#[tauri::command]
pub(crate) fn new_conversation_with_workspace(
    state: State<AppState>,
    workspace_path: Option<String>,
) -> Result<Conversation, String> {
    let workspace = match workspace_path.as_deref().map(str::trim).filter(|p| !p.is_empty()) {
        Some(path) => {
            let path_buf = std::path::PathBuf::from(path);
            if !path_buf.is_dir() {
                return Err("工作区路径不存在或不是目录".to_string());
            }
            let name = workspace_name_from_path(path);
            Some((path.to_string(), name))
        }
        None => None,
    };

    let db = state.db.lock().map_err(|e| e.to_string())?;
    create_conversation_with_workspace(
        &db,
        workspace.as_ref().map(|(path, _)| path.as_str()),
        workspace.as_ref().map(|(_, name)| name.as_str()),
    )
    .map_err(|e| e.to_string())
}

/// 返回所有会话列表，按最近更新时间倒序。
#[tauri::command]
pub(crate) fn get_conversations(state: State<AppState>) -> Result<Vec<Conversation>, String> {
    let db = state.db.lock().map_err(|e| e.to_string())?;
    list_conversations(&db).map_err(|e| e.to_string())
}

/// 返回指定会话的所有消息，按时间正序。
#[tauri::command]
pub(crate) fn load_messages(
    state: State<AppState>,
    conversation_id: String,
) -> Result<Vec<StoredMessage>, String> {
    let db = state.db.lock().map_err(|e| e.to_string())?;
    get_messages(&db, &conversation_id).map_err(|e| e.to_string())
}

/// 持久化一条消息到数据库。
///
/// 输入会话 ID、角色、内容和可选的 usage JSON 字符串；
/// 同步更新会话的 updated_at 字段，维持列表排序。
#[tauri::command]
pub(crate) fn persist_message(
    state: State<AppState>,
    conversation_id: String,
    role: String,
    content: String,
    usage_json: Option<String>,
    parts_json: Option<String>,
) -> Result<(), String> {
    let db = state.db.lock().map_err(|e| e.to_string())?;
    save_message(
        &db,
        &conversation_id,
        &role,
        &content,
        usage_json.as_deref(),
        parts_json.as_deref(),
    )
    .map_err(|e| e.to_string())
}

/// 更新会话标题。
///
/// 输入会话 ID 和新标题；用于首条消息发送后自动设置有意义的标题。
#[tauri::command]
pub(crate) fn rename_conversation(
    state: State<AppState>,
    conversation_id: String,
    title: String,
) -> Result<(), String> {
    let db = state.db.lock().map_err(|e| e.to_string())?;
    update_title(&db, &conversation_id, &title).map_err(|e| e.to_string())
}

/// 删除会话及其全部消息。
#[tauri::command]
pub(crate) fn remove_conversation(
    state: State<AppState>,
    conversation_id: String,
) -> Result<(), String> {
    let db = state.db.lock().map_err(|e| e.to_string())?;
    delete_conversation(&db, &conversation_id).map_err(|e| e.to_string())
}

/// 设置会话置顶状态；置顶会话在列表中排在最前。
#[tauri::command]
pub(crate) fn pin_conversation(
    state: State<AppState>,
    conversation_id: String,
    pinned: bool,
) -> Result<(), String> {
    let db = state.db.lock().map_err(|e| e.to_string())?;
    set_conversation_pinned(&db, &conversation_id, pinned).map_err(|e| e.to_string())
}

/// 查询 DeepSeek 账户余额，供设置页展示。从环境变量读取 API Key，不缓存、不持久化。
#[tauri::command]
pub(crate) async fn get_account_balance() -> Result<UserBalance, String> {
    let api_key = std::env::var("DEEPSEEK_API_KEY")
        .map_err(|_| "DEEPSEEK_API_KEY 未配置".to_string())?;
    get_user_balance(&api_key).await.map_err(|e| e.to_string())
}

/// 返回应用信息（版本号、数据目录路径），供设置页展示。
#[tauri::command]
pub(crate) fn get_app_info(app: AppHandle) -> Result<serde_json::Value, String> {
    let version = app.package_info().version.to_string();
    let data_dir = app
        .path()
        .app_data_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    Ok(serde_json::json!({ "version": version, "dataDir": data_dir }))
}

/// 设置会话归档状态；归档会话移入侧边栏「已归档」区，数据不删除。
#[tauri::command]
pub(crate) fn archive_conversation(
    state: State<AppState>,
    conversation_id: String,
    archived: bool,
) -> Result<(), String> {
    let db = state.db.lock().map_err(|e| e.to_string())?;
    set_conversation_archived(&db, &conversation_id, archived).map_err(|e| e.to_string())
}

/// 返回指定会话的全部文件变更检查点（含已回退的），供「变更记录」面板展示。
#[tauri::command]
pub(crate) fn get_checkpoints(
    state: State<AppState>,
    conversation_id: String,
) -> Result<Vec<FileCheckpoint>, String> {
    let db = state.db.lock().map_err(|e| e.to_string())?;
    list_file_checkpoints(&db, &conversation_id).map_err(|e| e.to_string())
}

/// 回退到指定检查点之前：把该检查点及其后的所有可回退变更按倒序撤销（CC 的 rewind）。
/// 返回成功回退的条数；不可回退的变更跳过。
#[tauri::command]
pub(crate) fn revert_to_checkpoint(
    state: State<AppState>,
    conversation_id: String,
    checkpoint_id: String,
) -> Result<usize, String> {
    let (workspace, targets) = {
        let db = state.db.lock().map_err(|e| e.to_string())?;
        let conversation = get_conversation(&db, &conversation_id)
            .map_err(|e| e.to_string())?
            .ok_or("会话不存在")?;
        let workspace = conversation
            .workspace_path
            .ok_or("该会话未绑定工作区")?;
        let all = list_file_checkpoints(&db, &conversation_id).map_err(|e| e.to_string())?;
        let target_seq = all
            .iter()
            .find(|c| c.id == checkpoint_id)
            .map(|c| c.seq)
            .ok_or("检查点不存在")?;
        let mut targets: Vec<FileCheckpoint> = all
            .into_iter()
            .filter(|c| c.seq >= target_seq && !c.reverted && c.revertible)
            .collect();
        // 按序号倒序撤销，后发生的变更先回退。
        targets.sort_by(|a, b| b.seq.cmp(&a.seq));
        (workspace, targets)
    };

    let mut reverted = 0;
    for checkpoint in &targets {
        if apply_checkpoint_revert(&workspace, checkpoint).is_ok() {
            let db = state.db.lock().map_err(|e| e.to_string())?;
            let _ = mark_checkpoint_reverted(&db, &checkpoint.id);
            reverted += 1;
        }
    }
    Ok(reverted)
}

/// 手动压缩会话历史（/compact）：把全部消息摘要成一条任务备忘录替换原文。
#[tauri::command]
pub(crate) async fn compact_history(
    app: AppHandle,
    state: State<'_, AppState>,
    conversation_id: String,
    model: String,
) -> Result<(), String> {
    let api_key = std::env::var("DEEPSEEK_API_KEY")
        .map_err(|_| "DEEPSEEK_API_KEY 未配置".to_string())?;
    let messages = {
        let db = state.db.lock().map_err(|e| e.to_string())?;
        get_messages(&db, &conversation_id).map_err(|e| e.to_string())?
    };
    if messages.len() < 4 {
        return Err("当前会话消息较少，无需压缩".to_string());
    }

    let transcript: String = messages
        .iter()
        .map(|m| {
            let body: String = m.content.chars().take(800).collect();
            format!("[{}] {}", m.role, body)
        })
        .collect::<Vec<_>>()
        .join("\n");
    let prompt = format!(
        "你是对话压缩器。请把下面这段用户与 AI 助手的完整对话压缩成简明的中文备忘录，\
保留：1）用户的总体目标与关键需求；2）已完成的事项与结论；3）重要决策与原因；\
4）涉及的文件清单；5）当前进度与待办。只输出备忘录本身。\n\n{transcript}"
    );
    let result = chat_completion_with_retry(
        &api_key,
        vec![serde_json::json!({ "role": "user", "content": prompt })],
        &model,
        None,
        &app,
    )
    .await?;
    let summary = result.content.unwrap_or_default();
    if summary.trim().is_empty() {
        return Err("压缩失败：模型未返回摘要".to_string());
    }

    let db = state.db.lock().map_err(|e| e.to_string())?;
    delete_messages(&db, &conversation_id).map_err(|e| e.to_string())?;
    save_message(
        &db,
        &conversation_id,
        "assistant",
        &format!("📋 对话已手动压缩（/compact），以下为此前内容的摘要：\n\n{summary}"),
        None,
        None,
    )
    .map_err(|e| e.to_string())
}

/// 列出工作区 .novacode/commands/*.md 自定义斜杠命令（name + description + body）。
/// 命令体中的 $ARGUMENTS 由前端替换为用户在 /name 后输入的参数。
#[tauri::command]
pub(crate) fn list_custom_commands(
    state: State<AppState>,
    conversation_id: String,
) -> Result<Vec<serde_json::Value>, String> {
    let workspace = {
        let db = state.db.lock().map_err(|e| e.to_string())?;
        get_conversation(&db, &conversation_id)
            .map_err(|e| e.to_string())?
            .and_then(|c| c.workspace_path)
    };
    let Some(workspace) = workspace else {
        return Ok(Vec::new());
    };
    let dir = std::path::Path::new(&workspace).join(".novacode").join("commands");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Ok(Vec::new());
    };
    let mut cmds = Vec::new();
    for entry in entries.flatten().take(50) {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else { continue };
        let Ok(raw) = std::fs::read_to_string(&path) else { continue };
        // 解析 frontmatter description（可选），其余为命令体。
        let (description, body) = parse_command_frontmatter(&raw);
        cmds.push(serde_json::json!({
            "name": format!("/{stem}"),
            "description": description,
            "body": body,
        }));
    }
    Ok(cmds)
}

/// 从命令 markdown 解析 frontmatter 的 description，返回 (description, body)。
fn parse_command_frontmatter(raw: &str) -> (String, String) {
    let trimmed = raw.trim_start();
    if let Some(rest) = trimmed.strip_prefix("---") {
        if let Some(end) = rest.find("\n---") {
            let front = &rest[..end];
            let body = rest[end + 4..].trim_start().to_string();
            let desc = front
                .lines()
                .find_map(|l| l.trim().strip_prefix("description:").map(|d| d.trim().to_string()))
                .unwrap_or_default();
            return (desc, body);
        }
    }
    // 无 frontmatter：首行非空作描述，全文作命令体
    let desc = raw.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("").to_string();
    (desc, raw.trim().to_string())
}

/// 平铺列出当前会话工作区内的文件相对路径，供输入框 @文件引用补全。
#[tauri::command]
pub(crate) fn list_workspace_files(
    state: State<AppState>,
    conversation_id: String,
) -> Result<Vec<String>, String> {
    let workspace = {
        let db = state.db.lock().map_err(|e| e.to_string())?;
        get_conversation(&db, &conversation_id)
            .map_err(|e| e.to_string())?
            .and_then(|c| c.workspace_path)
            .ok_or("该会话未绑定工作区")?
    };
    Ok(novacode_tool_runtime::workspace_file_list(&workspace, 500))
}

/// 读取命令沙箱开关状态。
#[tauri::command]
pub(crate) fn get_command_sandbox(state: State<AppState>) -> bool {
    state.command_sandbox.load(Ordering::SeqCst)
}

/// 设置命令沙箱开关。开启时前台命令在受限令牌沙箱中执行（降权 + 进程清理 + 密钥擦除）。
#[tauri::command]
pub(crate) fn set_command_sandbox(state: State<AppState>, enabled: bool) {
    state.command_sandbox.store(enabled, Ordering::SeqCst);
}

/// 导出单个会话为 Markdown 文件（数据治理：用户可导出/备份）。
#[tauri::command]
pub(crate) fn export_conversation(
    state: State<AppState>,
    conversation_id: String,
    path: String,
) -> Result<(), String> {
    let db = state.db.lock().map_err(|e| e.to_string())?;
    let conv = get_conversation(&db, &conversation_id)
        .map_err(|e| e.to_string())?
        .ok_or("会话不存在")?;
    let messages = get_messages(&db, &conversation_id).map_err(|e| e.to_string())?;
    let mut md = format!("# {}\n\n", conv.title);
    if let Some(ws) = conv.workspace_path.as_deref() {
        md.push_str(&format!("> 工作区：{ws}\n\n"));
    }
    for m in messages {
        let who = if m.role == "user" { "用户" } else { "助手" };
        md.push_str(&format!("## {who}\n\n{}\n\n", m.content));
    }
    std::fs::write(&path, md).map_err(|e| format!("写入失败: {e}"))
}

/// 导出全部会话的 token 账本为 CSV（数据治理 + 对账）。
#[tauri::command]
pub(crate) fn export_token_ledger(state: State<AppState>, path: String) -> Result<(), String> {
    let db = state.db.lock().map_err(|e| e.to_string())?;
    let convs = list_conversations(&db).map_err(|e| e.to_string())?;
    let mut csv = String::from(
        "conversation_id,title,role,total_tokens,prompt_tokens,completion_tokens,estimated_cost_usd,created_at\n",
    );
    for conv in convs {
        let messages = get_messages(&db, &conv.id).map_err(|e| e.to_string())?;
        for m in messages {
            let Some(usage_json) = m.usage_json.as_deref() else { continue };
            let v: serde_json::Value = serde_json::from_str(usage_json).unwrap_or_default();
            let g = |k: &str| v.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
            let cost = v.get("estimatedCostUsd").and_then(|x| x.as_f64()).unwrap_or(0.0);
            let title = conv.title.replace([',', '\n', '"'], " ");
            csv.push_str(&format!(
                "{},{},{},{},{},{},{:.6},{}\n",
                conv.id, title, m.role, g("totalTokens"), g("promptTokens"),
                g("completionTokens"), cost, m.created_at
            ));
        }
    }
    std::fs::write(&path, csv).map_err(|e| format!("写入失败: {e}"))
}

/// 清除全部会话与消息（数据治理：用户主动删除本地数据）。
#[tauri::command]
pub(crate) fn clear_all_conversations(state: State<AppState>) -> Result<(), String> {
    let db = state.db.lock().map_err(|e| e.to_string())?;
    novacode_storage::delete_all_conversations(&db).map_err(|e| e.to_string())
}

/// 读取单次任务 token 预算（0 = 不限）。
#[tauri::command]
pub(crate) fn get_task_budget(state: State<AppState>) -> u64 {
    state.task_token_budget.load(Ordering::SeqCst)
}

/// 设置单次任务 token 预算；超出后工具循环暂停并提示。
#[tauri::command]
pub(crate) fn set_task_budget(state: State<AppState>, budget: u64) {
    state.task_token_budget.store(budget, Ordering::SeqCst);
}

/// 列出全部权限规则（allow / deny），供设置页管理。
#[tauri::command]
pub(crate) fn get_permission_rules(state: State<AppState>) -> Result<Vec<String>, String> {
    let db = state.db.lock().map_err(|e| e.to_string())?;
    list_permission_rules(&db).map_err(|e| e.to_string())
}

/// 新增一条权限规则（如 `deny:read_file:**/.env`、`allow:cmd:git push`）。
#[tauri::command]
pub(crate) fn create_permission_rule(state: State<AppState>, rule: String) -> Result<(), String> {
    let rule = rule.trim();
    if rule.is_empty() {
        return Err("规则不能为空".to_string());
    }
    let db = state.db.lock().map_err(|e| e.to_string())?;
    add_permission_rule(&db, rule).map_err(|e| e.to_string())
}

/// 删除一条权限规则。
#[tauri::command]
pub(crate) fn delete_permission_rule(state: State<AppState>, rule: String) -> Result<(), String> {
    let db = state.db.lock().map_err(|e| e.to_string())?;
    remove_permission_rule(&db, &rule).map_err(|e| e.to_string())
}

/// 列出 MCP server 配置及连接状态（connected/disconnected + 工具数）。
#[tauri::command]
pub(crate) fn get_mcp_servers(state: State<AppState>) -> Result<Vec<serde_json::Value>, String> {
    let records = {
        let db = state.db.lock().map_err(|e| e.to_string())?;
        list_mcp_servers(&db).map_err(|e| e.to_string())?
    };
    let connected = state.mcp.lock().map_err(|e| e.to_string())?;
    Ok(records
        .into_iter()
        .map(|r| {
            let client = connected.get(&r.id);
            serde_json::json!({
                "id": r.id,
                "name": r.name,
                "command": r.command,
                "enabled": r.enabled,
                "connected": client.is_some(),
                "toolCount": client.map(|c| c.tools.len()).unwrap_or(0),
            })
        })
        .collect())
}

/// 新增 MCP server 配置并立即后台尝试连接。
#[tauri::command]
#[allow(non_snake_case)]
pub(crate) fn create_mcp_server(
    app: AppHandle,
    state: State<AppState>,
    name: String,
    command: String,
    authToken: Option<String>,
) -> Result<(), String> {
    let name = name.trim();
    let command = command.trim();
    if name.is_empty() || command.is_empty() {
        return Err("名称与启动命令/URL 不能为空".to_string());
    }
    let token = authToken.as_deref().map(str::trim).filter(|t| !t.is_empty());
    let record = {
        let db = state.db.lock().map_err(|e| e.to_string())?;
        add_mcp_server(&db, name, command, token).map_err(|e| e.to_string())?
    };
    spawn_mcp_connect(&app, record);
    Ok(())
}

/// 启用 / 停用一个 MCP server：停用立即断开，启用立即后台重连。
#[tauri::command]
pub(crate) fn toggle_mcp_server(
    app: AppHandle,
    state: State<AppState>,
    server_id: String,
    enabled: bool,
) -> Result<(), String> {
    let record = {
        let db = state.db.lock().map_err(|e| e.to_string())?;
        set_mcp_server_enabled(&db, &server_id, enabled).map_err(|e| e.to_string())?;
        list_mcp_servers(&db)
            .map_err(|e| e.to_string())?
            .into_iter()
            .find(|r| r.id == server_id)
    };
    if enabled {
        if let Some(record) = record {
            spawn_mcp_connect(&app, record);
        }
    } else if let Ok(mut map) = state.mcp.lock() {
        map.remove(&server_id); // Drop 时杀子进程
    }
    Ok(())
}

/// 删除一个 MCP server 配置并断开连接。
#[tauri::command]
pub(crate) fn delete_mcp_server(state: State<AppState>, server_id: String) -> Result<(), String> {
    {
        let db = state.db.lock().map_err(|e| e.to_string())?;
        remove_mcp_server(&db, &server_id).map_err(|e| e.to_string())?;
    }
    if let Ok(mut map) = state.mcp.lock() {
        map.remove(&server_id);
    }
    Ok(())
}

/// 导入本地文档并抽取纯文本（TXT/MD/CSV/JSON/PDF/DOCX），供发送给模型问答。
#[tauri::command]
pub(crate) fn import_file_text(path: String) -> Result<serde_json::Value, String> {
    const MAX_IMPORT_CHARS: usize = 100_000;
    let file_path = std::path::Path::new(&path);
    if !file_path.is_file() {
        return Err("文件不存在".to_string());
    }
    let name = file_path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let ext = file_path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();

    let text = match ext.as_str() {
        "txt" | "md" | "markdown" | "csv" | "json" | "log" | "xml" | "html" | "toml"
        | "yaml" | "yml" => std::fs::read_to_string(file_path)
            .map_err(|e| format!("读取文件失败: {e}"))?,
        "pdf" => pdf_extract::extract_text(file_path)
            .map_err(|e| format!("PDF 解析失败: {e}"))?,
        "docx" => extract_docx_text(file_path)?,
        other => {
            return Err(format!(
                "暂不支持 .{other} 格式（支持 txt/md/csv/json/pdf/docx 等文本类文档；图片需视觉模型，后续版本支持）"
            ));
        }
    };

    let truncated = text.chars().count() > MAX_IMPORT_CHARS;
    let capped: String = text.chars().take(MAX_IMPORT_CHARS).collect();
    Ok(serde_json::json!({ "name": name, "text": capped, "truncated": truncated }))
}

/// 从 .docx（zip 内 word/document.xml）抽取纯文本：拼接所有 <w:t> 文本节点，段落换行。
fn extract_docx_text(path: &std::path::Path) -> Result<String, String> {
    let file = std::fs::File::open(path).map_err(|e| format!("打开文件失败: {e}"))?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| format!("DOCX 解析失败: {e}"))?;
    let mut doc = archive
        .by_name("word/document.xml")
        .map_err(|_| "DOCX 缺少 document.xml".to_string())?;
    let mut xml = String::new();
    std::io::Read::read_to_string(&mut doc, &mut xml).map_err(|e| format!("读取失败: {e}"))?;

    // 轻量抽取：<w:p> 段落分行，<w:t> 文本节点取内容；不引入完整 XML 解析依赖。
    let mut out = String::new();
    let paragraphs = xml.split("</w:p>");
    for paragraph in paragraphs {
        let mut cursor = 0;
        let bytes = paragraph;
        while let Some(start) = bytes[cursor..].find("<w:t") {
            let tag_start = cursor + start;
            let Some(open_end) = bytes[tag_start..].find('>') else { break };
            let text_start = tag_start + open_end + 1;
            let Some(close) = bytes[text_start..].find("</w:t>") else { break };
            out.push_str(&bytes[text_start..text_start + close]);
            cursor = text_start + close + 6;
        }
        if !out.ends_with('\n') && !out.is_empty() {
            out.push('\n');
        }
    }
    Ok(out
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'"))
}

/// 返回指定会话的所有工具 Activity Event，按时间正序，供前端展示历史过程面板。
#[tauri::command]
pub(crate) fn get_conversation_events(
    state: State<AppState>,
    conversation_id: String,
) -> Result<Vec<ActivityEventRecord>, String> {
    let db = state.db.lock().map_err(|e| e.to_string())?;
    get_activity_events(&db, &conversation_id).map_err(|e| e.to_string())
}

/// 在 Agent 运行中排队一条插话消息（steering）。下一轮循环开始时作为 user 消息注入，
/// 让用户无需打断即可纠偏 / 追加要求。返回当前队列长度。
#[tauri::command]
pub(crate) fn queue_steering(
    state: State<AppState>,
    conversation_id: String,
    text: String,
) -> Result<usize, String> {
    let text = text.trim().to_string();
    if text.is_empty() {
        return Err("插话内容不能为空".to_string());
    }
    let mut steering = state.steering.lock().map_err(|e| e.to_string())?;
    let queue = steering.entry(conversation_id).or_default();
    queue.push(text);
    Ok(queue.len())
}

/// 请求中断指定会话正在运行的 Agent 工具循环。
///
/// 置位该会话的取消标志；循环在下一个检查点安全收尾，已执行的工具结果保留。
/// 若该会话当前没有运行中的 Agent，则为无操作。
#[tauri::command]
pub(crate) fn cancel_agent(state: State<AppState>, conversation_id: String) -> Result<(), String> {
    let cancels = state.cancels.lock().map_err(|e| e.to_string())?;
    if let Some(token) = cancels.get(&conversation_id) {
        token.store(true, Ordering::SeqCst);
    }
    Ok(())
}

/// 前端对一次高风险动作审批请求作出回应（允许 / 拒绝 / 总是允许）。
///
/// 通过 action_id 找到对应的 oneshot 通道并发送结果，唤醒正在等待的工具循环；
/// remember=true 且批准时，把该动作的规则写入 permission_rules 表，后续同类动作免审批。
#[tauri::command]
pub(crate) fn respond_approval(
    state: State<AppState>,
    action_id: String,
    approved: bool,
    remember: Option<bool>,
) -> Result<(), String> {
    let entry = {
        let mut approvals = state.approvals.lock().map_err(|e| e.to_string())?;
        approvals.remove(&action_id)
    };
    if let Some((sender, rule)) = entry {
        if approved && remember.unwrap_or(false) && !rule.is_empty() {
            if let Ok(db) = state.db.lock() {
                let _ = add_permission_rule(&db, &rule);
            }
        }
        let _ = sender.send(approved);
    }
    Ok(())
}

/// 前端对一次 ask_user 结构化提问作出回应，把答案 JSON（已含用户选择 / 自定义文本）
/// 通过 question_id 对应的 oneshot 通道送回正在等待的工具循环。
#[tauri::command]
pub(crate) fn respond_ask_user(
    state: State<AppState>,
    question_id: String,
    answer: String,
) -> Result<(), String> {
    let sender = {
        let mut map = state.ask_questions.lock().map_err(|e| e.to_string())?;
        map.remove(&question_id)
    };
    if let Some(sender) = sender {
        let _ = sender.send(answer);
    }
    Ok(())
}

// ── 工作区管理 ─────────────────────────────────────────────────────────────

/// 返回当前用户授权的活动工作区。
///
/// 输入应用状态；输出当前 Workspace 或 None，用于前端展示 Agent 可操作目录边界。
#[tauri::command]
pub(crate) fn get_workspace(state: State<AppState>) -> Result<Option<Workspace>, String> {
    let db = state.db.lock().map_err(|e| e.to_string())?;
    get_active_workspace(&db).map_err(|e| e.to_string())
}

/// 保存当前用户授权的工作区路径。
///
/// 输入本地目录路径；后端校验路径存在且为目录，写入 SQLite 后返回 Workspace。
#[tauri::command]
pub(crate) fn set_workspace_path(state: State<AppState>, path: String) -> Result<Workspace, String> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return Err("工作区路径不能为空".to_string());
    }

    let path_buf = std::path::PathBuf::from(trimmed);
    if !path_buf.is_dir() {
        return Err("工作区路径不存在或不是目录".to_string());
    }

    let db = state.db.lock().map_err(|e| e.to_string())?;
    save_active_workspace(&db, trimmed).map_err(|e| e.to_string())
}

/// 清除当前工作区授权。
///
/// 输入应用状态；删除当前活动工作区记录，后续 Agent 文件能力应视为未授权。
#[tauri::command]
pub(crate) fn clear_workspace(state: State<AppState>) -> Result<(), String> {
    let db = state.db.lock().map_err(|e| e.to_string())?;
    clear_active_workspace(&db).map_err(|e| e.to_string())
}

// ── 自动更新 ──────────────────────────────────────────────────────────────

/// 检查 GitHub Releases 是否有新版本。
///
/// 返回新版本号字符串，无更新返回 None，出错返回 Err。
#[tauri::command]
pub(crate) async fn check_update(app: AppHandle) -> Result<Option<String>, String> {
    let updater = app.updater().map_err(|e| e.to_string())?;
    match updater.check().await {
        Ok(Some(update)) => Ok(Some(update.version.clone())),
        Ok(None) => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

/// 下载并安装更新，完成后自动重启应用。
///
/// 必须在用户明确确认后调用；下载进度通过 "update-progress" 事件推送。
#[tauri::command]
pub(crate) async fn install_update(app: AppHandle) -> Result<(), String> {
    let updater = app.updater().map_err(|e| e.to_string())?;
    let update = updater
        .check()
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "没有可用更新".to_string())?;

    let app_clone = app.clone();
    update
        .download_and_install(
            |downloaded, total| {
                // downloaded: usize, total: Option<u64>，统一转 u64 再计算百分比
                let pct = total.map(|t| downloaded as u64 * 100 / t).unwrap_or(0);
                let _ = app_clone.emit("update-progress", pct);
            },
            || {
                let _ = app.emit("update-ready", ());
            },
        )
        .await
        .map_err(|e| e.to_string())?;

    app.restart();
}

// ── 命令层共享小工具 ───────────────────────────────────────────────────────

/// 从工作区路径取末段目录名作为工作区显示名；取不到时回退原路径。
fn workspace_name_from_path(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.trim().is_empty())
        .unwrap_or(path)
        .to_string()
}

/// 将前端权限模式字符串映射为后端枚举，未知值回退到最安全的 Restricted。
pub(crate) fn permission_mode_from_str(value: &str) -> PermissionMode {
    match value {
        "ask_every_time" => PermissionMode::AskEveryTime,
        "workspace_auto" => PermissionMode::WorkspaceAuto,
        "full_access" => PermissionMode::FullAccess,
        _ => PermissionMode::Restricted,
    }
}
