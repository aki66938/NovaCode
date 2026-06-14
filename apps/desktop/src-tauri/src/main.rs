use novacode_deepseek_client::{
    chat_completion, chat_stream, chat_stream_with_tools, detect_api_key_status, get_user_balance,
    parse_dsml_tool_calls, strip_dsml_markup, ChatMessage, ToolCall, UserBalance,
};
use novacode_sandbox_runtime::{
    decide_tool_access, is_low_risk_command, session_security_context, NetworkMode,
    SessionSecurityContext, ToolCapability, ToolDecision,
};
use novacode_shared::{ApiKeyStatus, PermissionMode};
use novacode_mcp_client::{function_name_for, McpClient};
use novacode_storage::{
    add_mcp_server, add_permission_rule, clear_active_workspace, create_conversation,
    delete_conversation, delete_messages, get_active_workspace,
    create_conversation_with_workspace, get_activity_events, get_conversation, get_messages,
    init_db, list_conversations, list_file_checkpoints, list_mcp_servers,
    list_permission_rules, mark_checkpoint_reverted, record_activity_event,
    record_file_checkpoint, remove_mcp_server, remove_permission_rule, save_active_workspace,
    save_message,
    set_conversation_archived, set_conversation_pinned, set_mcp_server_enabled, update_title,
    ActivityEventRecord, Conversation, FileCheckpoint, McpServerRecord, StoredMessage, Workspace,
};
use novacode_token_accounting::{compute_cost_summary, deepseek_pricing_for_model};
use novacode_tool_runtime::{
    create_file, delete_dir, delete_file, edit_file, glob_files, list_dir, make_dir, move_path,
    read_file, run_command, search_text, stat_path, write_file, CreateFileRequest, DeleteDirRequest,
    DeleteFileRequest, EditFileRequest, GlobFilesRequest, ListDirRequest, MakeDirRequest,
    MovePathRequest, ReadFileRequest, RunCommandRequest, SearchTextRequest, StatPathRequest,
    WriteFileRequest,
};

/// Agent 工具循环单次会话内允许的最大工具轮数。作为防失控的硬上限兜底；
/// 真正的终止由「模型不再调用工具」自然触发，提高到 20 让多步开发任务不被过早截断。
/// 触发上下文压缩的软上限默认值（以上一次响应返回的 prompt_tokens 为准）。
/// DeepSeek V4 Flash / Pro 官方标称 1M 上下文，故取 800K：在接近上限前压缩，
/// 留约 200K headroom 给模型输出与当轮工具结果，避免顶满 1M 触发服务端退化。
/// 可用环境变量 NOVACODE_CONTEXT_SOFT_LIMIT 覆盖（便于低阈值压测验证压缩机制）。
const CONTEXT_SOFT_LIMIT_TOKENS: u64 = 800_000;
/// 摘要压缩时保留最近 N 条 wire 消息原文，更早的历史压缩成任务进度摘要。
const KEEP_RECENT_WIRE_MESSAGES: usize = 8;
/// 压缩时保留最近 N 次工具结果全文，更早的大体积结果替换为短桩。
const KEEP_RECENT_TOOL_RESULTS: usize = 3;
/// 仅压缩正文超过该字符数的旧工具结果；小结果不动，避免无谓信息损失。
const TOOL_RESULT_STUB_THRESHOLD: usize = 1_500;
/// 工具结果被压缩后替换成的短桩内容。
const COMPACTED_TOOL_STUB: &str =
    "{\"ok\":true,\"note\":\"[此前的工具结果已省略以节省上下文；如需该文件/目录/命令的最新内容，请重新调用对应工具读取]\"}";
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_updater::UpdaterExt;
use tokio::sync::oneshot;

/// 审批请求自增序号，用于生成唯一 action_id。
static APPROVAL_SEQ: AtomicU64 = AtomicU64::new(1);
/// ask_user 结构化提问自增序号，生成唯一 question_id。
static QUESTION_SEQ: AtomicU64 = AtomicU64::new(1);

// ── 应用状态 ──────────────────────────────────────────────────────────────

struct AppState {
    db: Mutex<rusqlite::Connection>,
    /// 正在运行的 Agent 会话取消标志，按 conversation_id 索引。用户点击停止时置 true，
    /// 工具循环在轮次之间和工具执行前检查并安全收尾。
    cancels: Mutex<HashMap<String, Arc<AtomicBool>>>,
    /// 等待用户审批的高风险动作，按 action_id 索引，附带该动作对应的「总是允许」规则串。
    /// respond_approval 命令收到前端决定后，通过 oneshot 通道唤醒正在 await 的工具循环；
    /// 用户勾选记住时把规则写入 permission_rules 表。
    approvals: Mutex<HashMap<String, (oneshot::Sender<bool>, String)>>,
    /// 等待用户回答的 ask_user 结构化提问，按 question_id 索引。respond_ask_user 命令收到
    /// 前端选择后，通过 oneshot 通道把答案 JSON 回送给正在 await 的工具循环。
    ask_questions: Mutex<HashMap<String, oneshot::Sender<String>>>,
    /// 已连接的 MCP server 客户端，按配置 id 索引。Arc 包裹以便在锁外调用。
    mcp: Mutex<HashMap<String, Arc<McpClient>>>,
    /// Steering：用户在 Agent 运行中排队的插话消息，按 conversation_id 索引。
    /// 工具循环在每轮开始时取出并作为 user 消息注入，实现「运行中纠偏」。
    steering: Mutex<HashMap<String, Vec<String>>>,
    /// repo map 按会话缓存：避免每轮重新遍历工作区，并让 system 前缀字节稳定，
    /// 最大化 DeepSeek prompt 缓存命中（缓存友好上下文）。
    repo_maps: Mutex<HashMap<String, String>>,
    /// 托管后台 shell：background=true 启动的命令，按 shell_id 索引，可轮询输出 / 杀进程。
    bg_shells: Mutex<HashMap<String, BgShell>>,
    /// 后台子代理任务：run_subtask background=true 启动的探索代理，按 task_id 索引，
    /// 可用 get_task_output 轮询报告/状态、kill_task 终止；完成时 usage 由首次 get_task_output 结算进账本。
    bg_tasks: Mutex<HashMap<String, BgTask>>,
    /// 命令沙箱开关（默认开）：前台 run_command 是否在受限令牌沙箱中执行。
    command_sandbox: AtomicBool,
    /// 单次任务 token 预算（累计 total_tokens 上限）；0 = 不限。超出则暂停工具循环。
    task_token_budget: AtomicU64,
}

/// 一个托管的后台 shell 进程状态。
#[derive(Clone)]
struct BgShell {
    command: String,
    output: Arc<Mutex<String>>,
    status: Arc<Mutex<String>>, // running | done | killed | error
    cancel: Arc<AtomicBool>,
}

/// 后台 shell 自增序号，生成唯一 shell_id。
static BG_SHELL_SEQ: AtomicU64 = AtomicU64::new(1);

/// 一个后台子代理任务的共享状态（仿 BgShell）。
#[derive(Clone)]
struct BgTask {
    description: String,
    report: Arc<Mutex<String>>,
    status: Arc<Mutex<String>>, // running | done | killed | error
    usage: Arc<Mutex<Option<novacode_shared::RawUsage>>>,
    /// usage 是否已被某次 get_task_output 结算进会话账本，避免重复计费。
    settled: Arc<AtomicBool>,
    cancel: Arc<AtomicBool>,
}

/// 后台子代理任务自增序号，生成唯一 task_id。
static BG_TASK_SEQ: AtomicU64 = AtomicU64::new(1);

/// MCP 工具与模型函数名的绑定关系，按 send 周期收集后传入工具循环。
#[derive(Clone)]
struct McpBinding {
    fn_name: String,
    server_id: String,
    tool_name: String,
    schema: serde_json::Value,
}

/// 收集所有已连接 MCP server 的工具绑定（函数名、调度信息与 schema）。
fn collect_mcp_bindings(app: &AppHandle) -> Vec<McpBinding> {
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
fn spawn_mcp_connect(app: &AppHandle, record: McpServerRecord) {
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

// ── DeepSeek ──────────────────────────────────────────────────────────────

#[tauri::command]
fn get_deepseek_api_key_status() -> ApiKeyStatus {
    detect_api_key_status(|name| std::env::var(name).ok())
}

/// 发起流式聊天请求。
///
/// 通过 "chat-chunk" 事件逐块推送内容；流结束后发送 "chat-usage" 事件；
/// 最后发送 "chat-done"。错误时返回字符串供前端展示。
#[tauri::command]
async fn send_message(
    app: AppHandle,
    state: State<'_, AppState>,
    conversation_id: String,
    messages: Vec<ChatMessage>,
    model: String,
    permission_mode: String,
    plan_mode: Option<bool>,
) -> Result<(), String> {
    let api_key = std::env::var("DEEPSEEK_API_KEY")
        .map_err(|_| "DEEPSEEK_API_KEY 未配置".to_string())?;
    let plan_mode = plan_mode.unwrap_or(false);
    let (conversation, permission_rules) = {
        let db = state.db.lock().map_err(|e| e.to_string())?;
        let conversation = get_conversation(&db, &conversation_id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "会话不存在".to_string())?;
        let rules = list_permission_rules(&db).unwrap_or_default();
        (conversation, rules)
    };
    // 工作区已绑定时生成 repo map 与长期记忆，注入项目结构摘要和持久约定供模型开局认知。
    // repo map 按会话缓存：首轮生成后复用，保持 system 前缀字节稳定以提升 prompt 缓存命中。
    let repo_map = conversation
        .workspace_path
        .as_deref()
        .filter(|path| !path.trim().is_empty())
        .map(|path| {
            let mut maps = state.repo_maps.lock().expect("repo_maps mutex poisoned");
            maps.entry(conversation_id.clone())
                .or_insert_with(|| novacode_tool_runtime::workspace_map(path))
                .clone()
        });
    let workspace_memory = conversation
        .workspace_path
        .as_deref()
        .and_then(read_workspace_memory);
    let skills = conversation
        .workspace_path
        .as_deref()
        .map(load_workspace_skills)
        .unwrap_or_default();
    let mut messages = messages_with_workspace_context(
        messages,
        conversation.workspace_path.as_deref(),
        conversation.workspace_name.as_deref(),
        repo_map.as_deref(),
        workspace_memory.as_deref(),
        &skills,
    );
    // 计划模式：要求模型只产出分步计划并等待确认，本轮不提供工具。
    if plan_mode {
        messages.insert(0, ChatMessage {
            role: "system".to_string(),
            content: "用户开启了计划模式：请基于需求给出清晰的分步执行计划（目标、步骤、涉及文件、风险点），然后停止并等待用户确认。本轮不要执行任何实际操作。".to_string(),
        });
    }
    let permission = permission_mode_from_str(&permission_mode);

    // 注册本轮会话的取消令牌，供 cancel_agent 命令置位、工具循环检查。
    let cancel_token = Arc::new(AtomicBool::new(false));
    {
        let mut cancels = state.cancels.lock().map_err(|e| e.to_string())?;
        cancels.insert(conversation_id.clone(), cancel_token.clone());
    }

    let result = if plan_mode {
        // 计划模式走纯流式（无工具），让模型把计划直接流给用户审阅。
        chat_stream(&api_key, messages, &model, |chunk| {
            let _ = app.emit("chat-chunk", chunk);
        })
        .await
        .map_err(|e| e.to_string())
    } else if let Some(workspace_path) = conversation.workspace_path.as_deref() {
        let mcp_bindings = collect_mcp_bindings(&app);
        chat_with_builtin_tools(
            &api_key,
            messages,
            &model,
            workspace_path,
            permission,
            &conversation_id,
            &app,
            cancel_token.clone(),
            permission_rules,
            mcp_bindings,
        )
        .await
    } else {
        chat_stream(&api_key, messages, &model, |chunk| {
            let _ = app.emit("chat-chunk", chunk);
        })
        .await
        .map_err(|e| e.to_string())
    };

    // 无论成功或失败都要清理取消令牌与残留的 steering 队列，避免影响下一轮。
    {
        if let Ok(mut cancels) = state.cancels.lock() {
            cancels.remove(&conversation_id);
        }
        if let Ok(mut steering) = state.steering.lock() {
            steering.remove(&conversation_id);
        }
    }

    let raw_usage = result?;

    if let Some(raw) = raw_usage {
        let summary = compute_cost_summary(&raw, &deepseek_pricing_for_model(&model));
        let _ = app.emit("chat-usage", summary);
    }

    let _ = app.emit("chat-done", ());
    Ok(())
}

// ── 会话管理 ──────────────────────────────────────────────────────────────

/// 创建新会话，初始标题为"新对话"。
#[tauri::command]
fn new_conversation(state: State<AppState>) -> Result<Conversation, String> {
    let db = state.db.lock().map_err(|e| e.to_string())?;
    create_conversation(&db).map_err(|e| e.to_string())
}

/// 创建新会话，并将会话绑定到创建时选择的工作区快照。
///
/// 输入可选工作区路径；路径存在时校验目录并写入 conversation snapshot，未传路径时创建纯聊天会话。
#[tauri::command]
fn new_conversation_with_workspace(
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
fn get_conversations(state: State<AppState>) -> Result<Vec<Conversation>, String> {
    let db = state.db.lock().map_err(|e| e.to_string())?;
    list_conversations(&db).map_err(|e| e.to_string())
}

/// 返回指定会话的所有消息，按时间正序。
#[tauri::command]
fn load_messages(
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
fn persist_message(
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
fn rename_conversation(
    state: State<AppState>,
    conversation_id: String,
    title: String,
) -> Result<(), String> {
    let db = state.db.lock().map_err(|e| e.to_string())?;
    update_title(&db, &conversation_id, &title).map_err(|e| e.to_string())
}

/// 删除会话及其全部消息。
#[tauri::command]
fn remove_conversation(
    state: State<AppState>,
    conversation_id: String,
) -> Result<(), String> {
    let db = state.db.lock().map_err(|e| e.to_string())?;
    delete_conversation(&db, &conversation_id).map_err(|e| e.to_string())
}

/// 设置会话置顶状态；置顶会话在列表中排在最前。
#[tauri::command]
fn pin_conversation(
    state: State<AppState>,
    conversation_id: String,
    pinned: bool,
) -> Result<(), String> {
    let db = state.db.lock().map_err(|e| e.to_string())?;
    set_conversation_pinned(&db, &conversation_id, pinned).map_err(|e| e.to_string())
}

/// 查询 DeepSeek 账户余额，供设置页展示。从环境变量读取 API Key，不缓存、不持久化。
#[tauri::command]
async fn get_account_balance() -> Result<UserBalance, String> {
    let api_key = std::env::var("DEEPSEEK_API_KEY")
        .map_err(|_| "DEEPSEEK_API_KEY 未配置".to_string())?;
    get_user_balance(&api_key).await.map_err(|e| e.to_string())
}

/// 返回应用信息（版本号、数据目录路径），供设置页展示。
#[tauri::command]
fn get_app_info(app: AppHandle) -> Result<serde_json::Value, String> {
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
fn archive_conversation(
    state: State<AppState>,
    conversation_id: String,
    archived: bool,
) -> Result<(), String> {
    let db = state.db.lock().map_err(|e| e.to_string())?;
    set_conversation_archived(&db, &conversation_id, archived).map_err(|e| e.to_string())
}

/// 返回指定会话的全部文件变更检查点（含已回退的），供「变更记录」面板展示。
#[tauri::command]
fn get_checkpoints(
    state: State<AppState>,
    conversation_id: String,
) -> Result<Vec<FileCheckpoint>, String> {
    let db = state.db.lock().map_err(|e| e.to_string())?;
    list_file_checkpoints(&db, &conversation_id).map_err(|e| e.to_string())
}

/// 回退到指定检查点之前：把该检查点及其后的所有可回退变更按倒序撤销（CC 的 rewind）。
/// 返回成功回退的条数；不可回退的变更跳过。
#[tauri::command]
fn revert_to_checkpoint(
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
async fn compact_history(
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
fn list_custom_commands(
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
fn list_workspace_files(
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
fn get_command_sandbox(state: State<AppState>) -> bool {
    state.command_sandbox.load(Ordering::SeqCst)
}

/// 设置命令沙箱开关。开启时前台命令在受限令牌沙箱中执行（降权 + 进程清理 + 密钥擦除）。
#[tauri::command]
fn set_command_sandbox(state: State<AppState>, enabled: bool) {
    state.command_sandbox.store(enabled, Ordering::SeqCst);
}

/// 导出单个会话为 Markdown 文件（数据治理：用户可导出/备份）。
#[tauri::command]
fn export_conversation(
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
fn export_token_ledger(state: State<AppState>, path: String) -> Result<(), String> {
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
fn clear_all_conversations(state: State<AppState>) -> Result<(), String> {
    let db = state.db.lock().map_err(|e| e.to_string())?;
    novacode_storage::delete_all_conversations(&db).map_err(|e| e.to_string())
}

/// 读取单次任务 token 预算（0 = 不限）。
#[tauri::command]
fn get_task_budget(state: State<AppState>) -> u64 {
    state.task_token_budget.load(Ordering::SeqCst)
}

/// 设置单次任务 token 预算；超出后工具循环暂停并提示。
#[tauri::command]
fn set_task_budget(state: State<AppState>, budget: u64) {
    state.task_token_budget.store(budget, Ordering::SeqCst);
}

/// 列出全部权限规则（allow / deny），供设置页管理。
#[tauri::command]
fn get_permission_rules(state: State<AppState>) -> Result<Vec<String>, String> {
    let db = state.db.lock().map_err(|e| e.to_string())?;
    list_permission_rules(&db).map_err(|e| e.to_string())
}

/// 新增一条权限规则（如 `deny:read_file:**/.env`、`allow:cmd:git push`）。
#[tauri::command]
fn create_permission_rule(state: State<AppState>, rule: String) -> Result<(), String> {
    let rule = rule.trim();
    if rule.is_empty() {
        return Err("规则不能为空".to_string());
    }
    let db = state.db.lock().map_err(|e| e.to_string())?;
    add_permission_rule(&db, rule).map_err(|e| e.to_string())
}

/// 删除一条权限规则。
#[tauri::command]
fn delete_permission_rule(state: State<AppState>, rule: String) -> Result<(), String> {
    let db = state.db.lock().map_err(|e| e.to_string())?;
    remove_permission_rule(&db, &rule).map_err(|e| e.to_string())
}

/// 列出 MCP server 配置及连接状态（connected/disconnected + 工具数）。
#[tauri::command]
fn get_mcp_servers(state: State<AppState>) -> Result<Vec<serde_json::Value>, String> {
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
fn create_mcp_server(
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
fn toggle_mcp_server(
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
fn delete_mcp_server(state: State<AppState>, server_id: String) -> Result<(), String> {
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
fn import_file_text(path: String) -> Result<serde_json::Value, String> {
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

/// 扫描工作区 .novacode/skills/*/SKILL.md，返回技能名与描述（首行 frontmatter 或首段）。
fn load_workspace_skills(workspace: &str) -> Vec<(String, String)> {
    let skills_dir = std::path::Path::new(workspace).join(".novacode").join("skills");
    let Ok(entries) = std::fs::read_dir(&skills_dir) else {
        return Vec::new();
    };
    let mut skills = Vec::new();
    for entry in entries.flatten().take(30) {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        let skill_md = dir.join("SKILL.md");
        let Ok(content) = std::fs::read_to_string(&skill_md) else {
            continue;
        };
        // 描述：取 frontmatter 的 description: 行，否则取第一行非空文本。
        let description = content
            .lines()
            .find_map(|line| line.trim().strip_prefix("description:").map(|d| d.trim().to_string()))
            .or_else(|| {
                content
                    .lines()
                    .map(str::trim)
                    .find(|l| !l.is_empty() && !l.starts_with("---") && !l.starts_with('#'))
                    .map(str::to_string)
            })
            .unwrap_or_default();
        skills.push((name, description));
    }
    skills
}

/// load_skill 工具：按名加载工作区技能的完整 SKILL.md 内容（按需注入，CC 渐进披露同款）。
fn execute_load_skill(workspace: &str, arguments: &str) -> Result<serde_json::Value, String> {
    let parsed: serde_json::Value =
        serde_json::from_str(arguments).map_err(|e| format!("工具参数解析失败: {e}"))?;
    let name = parsed
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or("load_skill 缺少 name")?;
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Err("技能名只允许字母数字、横线与下划线".to_string());
    }
    let path = std::path::Path::new(workspace)
        .join(".novacode")
        .join("skills")
        .join(name)
        .join("SKILL.md");
    let content = std::fs::read_to_string(&path).map_err(|_| format!("技能 {name} 不存在"))?;
    let capped: String = content.chars().take(32_000).collect();
    Ok(serde_json::json!({ "name": name, "skill": capped }))
}

/// 返回指定会话的所有工具 Activity Event，按时间正序，供前端展示历史过程面板。
#[tauri::command]
fn get_conversation_events(
    state: State<AppState>,
    conversation_id: String,
) -> Result<Vec<ActivityEventRecord>, String> {
    let db = state.db.lock().map_err(|e| e.to_string())?;
    get_activity_events(&db, &conversation_id).map_err(|e| e.to_string())
}

/// 在 Agent 运行中排队一条插话消息（steering）。下一轮循环开始时作为 user 消息注入，
/// 让用户无需打断即可纠偏 / 追加要求。返回当前队列长度。
#[tauri::command]
fn queue_steering(
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
fn cancel_agent(state: State<AppState>, conversation_id: String) -> Result<(), String> {
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
fn respond_approval(
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
fn respond_ask_user(
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
fn get_workspace(state: State<AppState>) -> Result<Option<Workspace>, String> {
    let db = state.db.lock().map_err(|e| e.to_string())?;
    get_active_workspace(&db).map_err(|e| e.to_string())
}

/// 保存当前用户授权的工作区路径。
///
/// 输入本地目录路径；后端校验路径存在且为目录，写入 SQLite 后返回 Workspace。
#[tauri::command]
fn set_workspace_path(state: State<AppState>, path: String) -> Result<Workspace, String> {
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
fn clear_workspace(state: State<AppState>) -> Result<(), String> {
    let db = state.db.lock().map_err(|e| e.to_string())?;
    clear_active_workspace(&db).map_err(|e| e.to_string())
}

// ── 自动更新 ──────────────────────────────────────────────────────────────

/// 检查 GitHub Releases 是否有新版本。
///
/// 返回新版本号字符串，无更新返回 None，出错返回 Err。
#[tauri::command]
async fn check_update(app: AppHandle) -> Result<Option<String>, String> {
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
async fn install_update(app: AppHandle) -> Result<(), String> {
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

fn workspace_name_from_path(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.trim().is_empty())
        .unwrap_or(path)
        .to_string()
}

fn messages_with_workspace_context(
    messages: Vec<ChatMessage>,
    workspace_path: Option<&str>,
    workspace_name: Option<&str>,
    repo_map: Option<&str>,
    workspace_memory: Option<&str>,
    skills: &[(String, String)],
) -> Vec<ChatMessage> {
    // 把后端可信的 session 工作区快照注入模型上下文，使 DeepSeek 能回答当前工作区问题。
    let Some(path) = workspace_path.filter(|path| !path.trim().is_empty()) else {
        // 纯聊天会话（未绑定工作区）：明确告知模型没有任何工具，防止它凭训练记忆
        // 幻觉输出 <ToolCall>/DSML 等工具调用标记（0.0.17 dev 实测出现过）。
        let mut injected = Vec::with_capacity(messages.len() + 1);
        injected.push(ChatMessage {
            role: "system".to_string(),
            content: "当前会话未绑定工作区，你没有任何本地文件、目录或命令工具可用。\
如果用户要求读写文件、列目录、修改代码或执行命令，请直接告知：需要点击「+ 新对话」并选择工作区后才能执行本地操作。\
绝对不要输出任何工具调用标记（如 <ToolCall>、DSML 标记等），也不要假装已经执行了本地操作。"
                .to_string(),
        });
        injected.extend(messages);
        return injected;
    };
    let name = workspace_name
        .filter(|name| !name.trim().is_empty())
        .unwrap_or("未命名工作区");
    let mut injected = Vec::with_capacity(messages.len() + 2);
    // 身份锚定：明确 NovaCode 不是 Claude Code，配置在 .novacode/，防止模型沿用 .claude 等训练记忆里的约定。
    injected.push(ChatMessage {
        role: "system".to_string(),
        content: "你是 NovaCode（Ignite the Spark, Unleash the Code.）桌面 Agent 的内置助手，运行在 NovaCode 应用里。\
你不是 Claude Code，也不是 Codex，不要沿用它们的约定：本应用的配置目录是工作区下的 .novacode/（不是 .claude/，NovaCode 没有也不读取 .claude 目录及其中的 settings.json）。\
NovaCode 的可扩展配置都在 .novacode/ 下：技能 .novacode/skills/<名>/SKILL.md，钩子 .novacode/hooks.json，自定义斜杠命令 .novacode/commands/<名>.md，自定义子代理 .novacode/agents/<类型>.md，诊断命令 .novacode/diagnostics；项目长期记忆是工作区根目录的 NovaCode.md。\
安装/配置 MCP 服务器不是通过编辑任何配置文件——请调用 add_mcp_server 工具注册（它会写入 NovaCode 的服务器表并立即连接、其工具随后即可调用），或让用户在「设置 → MCP 服务器」添加。绝不要去查找或编辑 .claude/settings.json 之类文件，那对 NovaCode 完全无效。".to_string(),
    });
    injected.push(ChatMessage {
        role: "system".to_string(),
        content: format!(
            "你正在 NovaCode 桌面端中运行。本轮会话绑定的工作区名称是 {name}，工作区路径是 {path}。\
除非用户明确授权越界，否则你应假定所有本地文件任务都发生在该工作区内。\
当用户询问你当前所在的工作区或工作目录时，应直接回答这个路径；不要声称自己没有工作区。\
当用户要求列目录、读取文件、创建文件、修改文件或删除文件时，必须分别调用 list_dir、read_file、\
create_file、write_file 或 delete_file 工具完成真实本地操作；不要只给出代码示例，\
不要建议用户手动操作，也不要在没有工具结果时声称文件已处理。"
        ),
    });
    injected.push(ChatMessage {
        role: "system".to_string(),
        content: "工具调用规则：所有本地文件和命令操作必须通过工具完成，不能只在正文中声称已经完成。可用工具包括 list_dir、read_file、create_file、write_file、edit_file、delete_file、make_dir、move_path、delete_dir、stat_path、search_text、run_command。修改已有文件时优先使用 edit_file，并提供 oldText/newText；只有需要完整覆盖文件时才使用 write_file。移动或重命名文件用 move_path，不要用 create+delete 模拟。执行前需要了解目录、文件存在性或代码位置时，先使用 list_dir、stat_path 或 search_text。run_command 用于列目录、git status、构建或测试等命令：低风险命令（cargo check/test、npm test/run build、git status/diff、dir 等）在 Workspace Auto 下可直接执行，其余命令需 Full Access 或用户审批。每一步都要基于真实工具结果继续；若某次工具因权限被拒绝或用户拒绝，应说明情况或改用被允许的方式，不要重复硬闯。若某次工具调用失败，请阅读返回的 error，判断是参数、路径还是环境问题，调整后重试或换用其他工具，不要原样重复同一次失败调用。对于多步骤任务，请先调用 todo_write 列出步骤清单并随进度更新状态（同一时刻只有一项 in_progress），让用户实时看到进度。当需求确实含糊、且靠读文件或运行工具也无法判断、继续就会做错方向时，用 ask_user 给出 1-4 个结构化选项让用户选择，而不是擅自假设；能自己查清的事不要问。需要在大型代码库做只读调查（找实现、理结构、读懂模块）时，优先调用 run_subtask 委托独立子代理，避免主对话上下文膨胀。长时间运行的命令（启动服务、watch 等）用 run_command 的 background=true，它会立即返回 shellId；之后用 get_shell_output 轮询其输出与状态、用 kill_shell 终止、用 list_shells 查看所有后台进程。用户消息中的 @相对路径 表示工作区文件引用，直接用 read_file 读取即可。需要查阅在线文档、报错信息或你不确定的最新资料时，用 web_search 搜索、再用 web_fetch 抓取相关 URL 的正文，不要凭记忆臆测。遇到值得跨会话记住的项目约定、关键路径或踩过的坑，用 remember 写入项目长期记忆（精炼、可复用的事实才记，临时细节不要记）。".to_string(),
    });
    // repo map：开局注入工作区结构摘要，让模型无需逐层 list_dir 就了解项目骨架。
    if let Some(map) = repo_map.filter(|map| !map.trim().is_empty()) {
        injected.push(ChatMessage {
            role: "system".to_string(),
            content: format!(
                "当前工作区结构摘要（已忽略 .git/node_modules/target 等噪声目录，可能有省略）：\n{map}\n\
需要查看更深层目录或文件内容时，再调用 list_dir / read_file。"
            ),
        });
    }
    // 项目长期记忆：工作区根目录 NovaCode.md（对标 CLAUDE.md / AGENTS.md），每次请求注入，
    // 永不被上下文压缩冲掉，承载项目目标、规范与架构约定。
    if let Some(memory) = workspace_memory.filter(|m| !m.trim().is_empty()) {
        injected.push(ChatMessage {
            role: "system".to_string(),
            content: format!(
                "项目长期记忆（来自工作区根目录的 NovaCode.md，跨会话持久有效，优先遵循其中的目标与约定）：\n{memory}"
            ),
        });
    }
    // 技能列表（渐进披露）：只注入名称与描述，完整说明由模型按需调用 load_skill 加载。
    if !skills.is_empty() {
        let list = skills
            .iter()
            .map(|(name, desc)| format!("- {name}：{desc}"))
            .collect::<Vec<_>>()
            .join("\n");
        injected.push(ChatMessage {
            role: "system".to_string(),
            content: format!(
                "当前工作区可用技能（来自 .novacode/skills/）。当任务与某项技能匹配时，先调用 load_skill 加载其完整说明再执行：\n{list}"
            ),
        });
    }
    injected.extend(messages);
    injected
}

/// 读取工作区根目录的 NovaCode.md 作为项目长期记忆；不存在或为空时返回 None。
/// 上限 16K 字符，防止超大记忆文件挤占上下文。
fn read_workspace_memory(workspace_root: &str) -> Option<String> {
    let path = std::path::Path::new(workspace_root).join("NovaCode.md");
    let content = std::fs::read_to_string(path).ok()?;
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.chars().take(16_000).collect())
}

/// 将前端权限模式字符串映射为后端枚举，未知值回退到最安全的 Restricted。
fn permission_mode_from_str(value: &str) -> PermissionMode {
    match value {
        "ask_every_time" => PermissionMode::AskEveryTime,
        "workspace_auto" => PermissionMode::WorkspaceAuto,
        "full_access" => PermissionMode::FullAccess,
        _ => PermissionMode::Restricted,
    }
}

/// 记录一条工具 Activity Event，并同时推送给前端用于过程展示。
///
/// 输入会话 ID、工具名、状态、输入参数、可选输出/错误和工作区快照；
/// 写入失败不阻塞主流程，只忽略错误，保证一次工具失败不会拖垮整条对话。
#[allow(clippy::too_many_arguments)]
fn record_tool_event(
    app: &AppHandle,
    conversation_id: &str,
    event_type: &str,
    tool_name: &str,
    status: &str,
    input_json: &str,
    output_json: Option<&str>,
    error_message: Option<&str>,
    workspace_path: &str,
) {
    let state = app.state::<AppState>();
    if let Ok(db) = state.db.lock() {
        let _ = record_activity_event(
            &db,
            conversation_id,
            event_type,
            Some(tool_name),
            status,
            Some(input_json),
            output_json,
            error_message,
            Some(workspace_path),
        );
    }
    let _ = app.emit(
        "tool-event",
        serde_json::json!({
            "toolName": tool_name,
            "status": status,
            "inputJson": input_json,
            "outputJson": output_json,
            "errorMessage": error_message,
        }),
    );
}

/// 单次工具调用的权限门控结果。
enum ToolGate {
    /// 直接放行执行。
    Allow,
    /// 需要用户逐次审批。
    Ask,
    /// 当前权限模式直接拒绝，附带原因。
    Deny(String),
}

/// 由工具名与参数推导「总是允许」规则串：命令取前两个 token 作前缀，其余工具按工具名。
fn permission_rule_for(tool_name: &str, arguments: &str) -> String {
    if tool_name == "run_command" {
        if let Ok(request) = serde_json::from_str::<RunCommandRequest>(arguments) {
            let prefix: Vec<&str> = request.command.split_whitespace().take(2).collect();
            if !prefix.is_empty() {
                return format!("cmd:{}", prefix.join(" "));
            }
        }
        return String::new();
    }
    format!("tool:{tool_name}")
}

/// 极简 glob 匹配：`*` 与 `**` 都视为「任意字符序列（含 /）」，支持 `**/.env`、`src/**`、`*.env`。
fn glob_match(pattern: &str, text: &str) -> bool {
    // 把 ** 折叠为 *，再按 * 切段，依次顺序匹配（首段需前缀对齐、末段需后缀对齐）。
    let pat = pattern.replace("**", "*");
    let parts: Vec<&str> = pat.split('*').collect();
    if parts.len() == 1 {
        return pattern == text; // 无通配，精确匹配
    }
    let mut pos = 0usize;
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        if i == 0 {
            if !text.starts_with(part) {
                return false;
            }
            pos = part.len();
        } else if i == parts.len() - 1 {
            return text[pos..].ends_with(part);
        } else {
            match text[pos..].find(part) {
                Some(idx) => pos += idx + part.len(),
                None => return false,
            }
        }
    }
    true
}

/// 单条权限规则对本次调用的裁决：deny 命中返回 Some(false)，allow 命中返回 Some(true)，未命中 None。
///
/// 规则格式：`[allow:|deny:]<body>`（无前缀默认 allow，向后兼容旧规则）。body 为：
/// `cmd:<前缀>`（run_command 命令前缀）| `tool:<工具名>`（按工具名）| `<工具名>:<glob>`（工具+路径 glob）。
fn rule_decision(rule: &str, tool_name: &str, arguments: &str) -> Option<bool> {
    let (effect, body) = if let Some(r) = rule.strip_prefix("deny:") {
        (false, r)
    } else if let Some(r) = rule.strip_prefix("allow:") {
        (true, r)
    } else {
        (true, rule)
    };

    if let Some(prefix) = body.strip_prefix("cmd:") {
        if tool_name == "run_command" {
            if let Ok(request) = serde_json::from_str::<RunCommandRequest>(arguments) {
                let cmd = request.command.trim().to_lowercase();
                let p = prefix.trim().to_lowercase();
                if !p.is_empty() && (cmd == p || cmd.starts_with(&format!("{p} "))) {
                    return Some(effect);
                }
            }
        }
        return None;
    }
    if let Some(name) = body.strip_prefix("tool:") {
        return if name == tool_name { Some(effect) } else { None };
    }
    // <toolname>:<glob> —— 工具名 + 路径 glob
    if let Some((rtool, glob)) = body.split_once(':') {
        if rtool == tool_name {
            let path = serde_json::from_str::<serde_json::Value>(arguments)
                .ok()
                .and_then(|v| {
                    ["path", "from", "to"]
                        .iter()
                        .find_map(|k| v.get(*k).and_then(|x| x.as_str()).map(str::to_string))
                })
                .unwrap_or_default();
            if glob_match(glob, &path) {
                return Some(effect);
            }
        }
    }
    None
}

/// 汇总权限规则：deny 优先。返回 Some(false)=显式拒绝，Some(true)=显式放行，None=无规则覆盖。
fn permission_rules_decision(rules: &[String], tool_name: &str, arguments: &str) -> Option<bool> {
    let mut allowed = false;
    for rule in rules {
        match rule_decision(rule, tool_name, arguments) {
            Some(false) => return Some(false), // deny 立即否决
            Some(true) => allowed = true,
            None => {}
        }
    }
    if allowed {
        Some(true)
    } else {
        None
    }
}

/// 对单次工具调用做权限门控。
///
/// run_command 在 Workspace Auto 下，若命令属于低风险白名单则直接放行，否则按能力裁决；
/// 裁决为「需审批」时先查用户保存的「总是允许」规则，命中则免审批放行。
fn gate_tool_decision(
    context: &SessionSecurityContext,
    tool_name: &str,
    arguments: &str,
    rules: &[String],
) -> ToolGate {
    let capability = match tool_capability_for_name(tool_name) {
        Ok(capability) => capability,
        Err(message) => return ToolGate::Deny(message),
    };

    // 用户显式 deny 规则最高优先级：任何模式下都拒绝。
    if permission_rules_decision(rules, tool_name, arguments) == Some(false) {
        return ToolGate::Deny("已被用户的拒绝规则阻止".to_string());
    }

    // 低风险命令白名单：Workspace Auto 下免审批直接执行常见检查 / 构建 / 测试命令。
    if tool_name == "run_command"
        && matches!(context.permission_mode, novacode_shared::PermissionMode::WorkspaceAuto)
    {
        if let Ok(request) = serde_json::from_str::<RunCommandRequest>(arguments) {
            if is_low_risk_command(&request.command) {
                return ToolGate::Allow;
            }
        }
    }

    match decide_tool_access(context, capability) {
        ToolDecision::Allow => ToolGate::Allow,
        ToolDecision::AskUser => {
            if permission_rules_decision(rules, tool_name, arguments) == Some(true) {
                ToolGate::Allow
            } else {
                ToolGate::Ask
            }
        }
        ToolDecision::Deny => ToolGate::Deny("当前权限模式不允许此操作".to_string()),
    }
}

/// 向前端发起一次审批请求并等待用户决定。
///
/// 生成唯一 action_id，注册 oneshot 通道（附「总是允许」规则串），emit "approval-request"，
/// 然后 await 前端通过 respond_approval 命令送回的结果。通道异常时默认拒绝（安全优先）。
async fn request_tool_approval(app: &AppHandle, tool_name: &str, arguments: &str) -> bool {
    let action_id = format!("act-{}", APPROVAL_SEQ.fetch_add(1, Ordering::SeqCst));
    let rule = permission_rule_for(tool_name, arguments);
    let (sender, receiver) = oneshot::channel::<bool>();
    {
        let state = app.state::<AppState>();
        let mut approvals = state.approvals.lock().expect("approvals mutex poisoned");
        approvals.insert(action_id.clone(), (sender, rule));
    }

    let _ = app.emit(
        "approval-request",
        serde_json::json!({
            "actionId": action_id,
            "toolName": tool_name,
            "target": approval_target(arguments),
        }),
    );

    receiver.await.unwrap_or(false)
}

/// 生成唯一 question_id，注册 oneshot 通道，emit "ask-user-request"（携带结构化问题），
/// 然后 await 前端通过 respond_ask_user 命令送回的答案 JSON。通道异常 / 用户取消时返回空串。
async fn request_user_answer(app: &AppHandle, questions: &serde_json::Value) -> String {
    let question_id = format!("ask-{}", QUESTION_SEQ.fetch_add(1, Ordering::SeqCst));
    let (sender, receiver) = oneshot::channel::<String>();
    {
        let state = app.state::<AppState>();
        let mut map = state
            .ask_questions
            .lock()
            .expect("ask_questions mutex poisoned");
        map.insert(question_id.clone(), sender);
    }

    let _ = app.emit(
        "ask-user-request",
        serde_json::json!({
            "questionId": question_id,
            "questions": questions,
        }),
    );

    receiver.await.unwrap_or_default()
}

/// ask_user 工具：需求不明确且靠读文件 / 工具也无法判断时，把 1-4 个结构化问题弹给用户，
/// 阻塞等待选择（前端自动附「Other」自定义项），返回用户答案 JSON 供模型据此继续。
async fn execute_ask_user(
    app: &AppHandle,
    arguments: &str,
) -> Result<serde_json::Value, String> {
    let parsed = serde_json::from_str::<serde_json::Value>(arguments)
        .map_err(|err| format!("工具参数解析失败: {err}"))?;
    let questions = parsed
        .get("questions")
        .cloned()
        .ok_or_else(|| "ask_user 缺少 questions".to_string())?;
    let is_nonempty_array = questions
        .as_array()
        .map(|a| !a.is_empty() && a.len() <= 4)
        .unwrap_or(false);
    if !is_nonempty_array {
        return Err("ask_user 的 questions 必须是 1-4 个问题的数组".to_string());
    }

    let answer = request_user_answer(app, &questions).await;
    if answer.trim().is_empty() {
        return Err("用户未作出选择（已取消提问）。请根据已有信息继续，或换一种方式推进。".to_string());
    }
    // 前端回送的是答案 JSON（每题 -> 选择数组 / 自定义文本）；解析失败则按纯文本兜底。
    let answers = serde_json::from_str::<serde_json::Value>(&answer)
        .unwrap_or(serde_json::Value::String(answer));
    Ok(serde_json::json!({ "answers": answers }))
}

/// 从工具参数中提取审批展示用的目标（path / from / command）。
fn approval_target(arguments: &str) -> String {
    serde_json::from_str::<serde_json::Value>(arguments)
        .ok()
        .and_then(|value| {
            ["path", "from", "command"]
                .iter()
                .find_map(|key| value.get(*key).and_then(|v| v.as_str()).map(str::to_string))
        })
        .unwrap_or_default()
}

/// 记录一次工具被拒绝（权限拒绝或用户拒绝），并把拒绝结果回灌给模型，让它换方案或说明。
#[allow(clippy::too_many_arguments)]
fn feed_tool_denial(
    app: &AppHandle,
    conversation_id: &str,
    tool_name: &str,
    arguments: &str,
    workspace_path: &str,
    reason: &str,
    tool_call_id: &str,
    wire_messages: &mut Vec<serde_json::Value>,
) {
    record_tool_event(
        app,
        conversation_id,
        "tool_denied",
        tool_name,
        "denied",
        arguments,
        None,
        Some(reason),
        workspace_path,
    );
    wire_messages.push(serde_json::json!({
        "role": "tool",
        "tool_call_id": tool_call_id,
        "content": serde_json::json!({ "ok": false, "error": reason }).to_string()
    }));
}

// ── 文件变更检查点与 diff ────────────────────────────────────────────────

/// 写类工具执行前捕获的回退数据快照。
struct CheckpointCapture {
    rel_path: String,
    prev_content: Option<String>,
    extra_json: Option<String>,
    revertible: bool,
}

/// 检查点快照的单文件大小上限；超限文件跳过快照并标记不可回退，防止数据库膨胀。
const CHECKPOINT_MAX_SNAPSHOT_BYTES: u64 = 1024 * 1024;

/// 把工作区相对路径安全拼接为绝对路径；含 `..` 的路径拒绝（防越界回写）。
fn safe_workspace_join(workspace: &str, rel: &str) -> Option<std::path::PathBuf> {
    if rel.contains("..") {
        return None;
    }
    Some(std::path::Path::new(workspace).join(rel.trim_start_matches(['\\', '/'])))
}

/// 在写类工具执行前读取目标文件现状，供成功后记录检查点（rewind 用）。
/// 非写类工具返回 None。必须在工具执行前调用。
fn capture_checkpoint_before(
    workspace: &str,
    tool_name: &str,
    arguments: &str,
) -> Option<CheckpointCapture> {
    let args: serde_json::Value = serde_json::from_str(arguments).ok()?;
    let read_prev = |rel: &str| -> Option<String> {
        let path = safe_workspace_join(workspace, rel)?;
        let meta = std::fs::metadata(&path).ok()?;
        if !meta.is_file() || meta.len() > CHECKPOINT_MAX_SNAPSHOT_BYTES {
            return None;
        }
        std::fs::read_to_string(&path).ok()
    };

    match tool_name {
        "create_file" => {
            let rel = args.get("path")?.as_str()?;
            Some(CheckpointCapture {
                rel_path: rel.to_string(),
                prev_content: None,
                extra_json: None,
                revertible: true,
            })
        }
        "write_file" | "edit_file" => {
            let rel = args.get("path")?.as_str()?;
            let prev = read_prev(rel);
            let existed = safe_workspace_join(workspace, rel)
                .map(|p| p.exists())
                .unwrap_or(false);
            // 文件存在但快照失败（过大/非文本）时无法恢复原内容，标记不可回退。
            let revertible = prev.is_some() || !existed;
            Some(CheckpointCapture {
                rel_path: rel.to_string(),
                prev_content: prev,
                extra_json: None,
                revertible,
            })
        }
        "delete_file" => {
            let rel = args.get("path")?.as_str()?;
            let prev = read_prev(rel);
            let revertible = prev.is_some();
            Some(CheckpointCapture {
                rel_path: rel.to_string(),
                prev_content: prev,
                extra_json: None,
                revertible,
            })
        }
        "move_path" => {
            let from = args.get("from")?.as_str()?;
            let to = args.get("to")?.as_str()?;
            Some(CheckpointCapture {
                rel_path: from.to_string(),
                prev_content: None,
                extra_json: Some(serde_json::json!({ "from": from, "to": to }).to_string()),
                revertible: true,
            })
        }
        "make_dir" => {
            let rel = args.get("path")?.as_str()?;
            Some(CheckpointCapture {
                rel_path: rel.to_string(),
                prev_content: None,
                extra_json: None,
                revertible: true,
            })
        }
        "delete_dir" => {
            let rel = args.get("path")?.as_str()?;
            Some(CheckpointCapture {
                rel_path: rel.to_string(),
                prev_content: None,
                extra_json: None,
                revertible: false,
            })
        }
        _ => None,
    }
}

/// 计算行级 unified diff，返回 (diff 文本, 新增行数, 删除行数)；diff 文本截断防膨胀。
fn compute_line_diff(old: &str, new: &str) -> (String, usize, usize) {
    let diff = similar::TextDiff::from_lines(old, new);
    let (mut added, mut removed) = (0usize, 0usize);
    for change in diff.iter_all_changes() {
        match change.tag() {
            similar::ChangeTag::Insert => added += 1,
            similar::ChangeTag::Delete => removed += 1,
            similar::ChangeTag::Equal => {}
        }
    }
    let mut text = diff.unified_diff().context_radius(2).to_string();
    const MAX_DIFF_CHARS: usize = 4_000;
    if text.chars().count() > MAX_DIFF_CHARS {
        text = text.chars().take(MAX_DIFF_CHARS).collect::<String>() + "\n…(diff 过长已截断)";
    }
    (text, added, removed)
}

/// 工具成功后计算该次变更的 diff（仅文本写类工具）；返回 None 表示该工具无 diff 概念。
fn post_execution_diff(
    workspace: &str,
    tool_name: &str,
    arguments: &str,
    capture: &CheckpointCapture,
) -> Option<(String, usize, usize)> {
    let args: serde_json::Value = serde_json::from_str(arguments).ok()?;
    let prev = capture.prev_content.as_deref().unwrap_or("");
    match tool_name {
        "create_file" | "write_file" => {
            let new = args.get("content")?.as_str()?;
            Some(compute_line_diff(prev, new))
        }
        "edit_file" => {
            let path = safe_workspace_join(workspace, &capture.rel_path)?;
            let new = std::fs::read_to_string(path).ok()?;
            Some(compute_line_diff(prev, &new))
        }
        "delete_file" => {
            if prev.is_empty() {
                return None;
            }
            Some(compute_line_diff(prev, ""))
        }
        _ => None,
    }
}

/// 把检查点落库（失败只忽略，不阻塞工具链路）。
fn persist_checkpoint(
    app: &AppHandle,
    conversation_id: &str,
    tool_name: &str,
    capture: &CheckpointCapture,
) {
    let state = app.state::<AppState>();
    let guard = state.db.lock();
    if let Ok(db) = guard {
        let _ = record_file_checkpoint(
            &db,
            conversation_id,
            tool_name,
            &capture.rel_path,
            capture.prev_content.as_deref(),
            capture.extra_json.as_deref(),
            capture.revertible,
        );
    };
}

/// 对单个检查点执行文件系统回退。
fn apply_checkpoint_revert(workspace: &str, checkpoint: &FileCheckpoint) -> Result<(), String> {
    if !checkpoint.revertible {
        return Err("该变更不可回退".to_string());
    }
    match checkpoint.tool_name.as_str() {
        "create_file" => {
            if let Some(path) = safe_workspace_join(workspace, &checkpoint.rel_path) {
                let _ = std::fs::remove_file(path);
            }
            Ok(())
        }
        "write_file" | "edit_file" => {
            let path = safe_workspace_join(workspace, &checkpoint.rel_path)
                .ok_or("路径不安全")?;
            match checkpoint.prev_content.as_deref() {
                Some(content) => std::fs::write(path, content).map_err(|e| e.to_string()),
                None => {
                    let _ = std::fs::remove_file(path);
                    Ok(())
                }
            }
        }
        "delete_file" => {
            let path = safe_workspace_join(workspace, &checkpoint.rel_path)
                .ok_or("路径不安全")?;
            let content = checkpoint
                .prev_content
                .as_deref()
                .ok_or("缺少回退内容")?;
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            std::fs::write(path, content).map_err(|e| e.to_string())
        }
        "move_path" => {
            let extra: serde_json::Value = checkpoint
                .extra_json
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok())
                .ok_or("缺少移动信息")?;
            let from = extra.get("from").and_then(|v| v.as_str()).ok_or("缺少 from")?;
            let to = extra.get("to").and_then(|v| v.as_str()).ok_or("缺少 to")?;
            let from_abs = safe_workspace_join(workspace, from).ok_or("路径不安全")?;
            let to_abs = safe_workspace_join(workspace, to).ok_or("路径不安全")?;
            std::fs::rename(to_abs, from_abs).map_err(|e| e.to_string())
        }
        "make_dir" => {
            if let Some(path) = safe_workspace_join(workspace, &checkpoint.rel_path) {
                let _ = std::fs::remove_dir(path);
            }
            Ok(())
        }
        other => Err(format!("不支持回退的工具: {other}")),
    }
}

// ── todo / 后台命令 / 子任务 ─────────────────────────────────────────────

// ── Hooks 生命周期系统 ──────────────────────────────────────────────────
//
// 从工作区 .novacode/hooks.json 读取用户定义的生命周期钩子。PreToolUse 在工具执行前运行，
// 退出码非 0 则阻断该工具（stdout/stderr 作为原因回灌模型）；PostToolUse 在工具成功后运行（信息性）。
// 钩子命令经 PowerShell 执行，工具的 { tool, arguments } 以 JSON 从 stdin 传入。

#[derive(serde::Deserialize)]
struct HookEntry {
    matcher: String,
    command: String,
}

#[derive(serde::Deserialize, Default)]
struct HooksConfig {
    #[serde(rename = "PreToolUse", default)]
    pre_tool_use: Vec<HookEntry>,
    #[serde(rename = "PostToolUse", default)]
    post_tool_use: Vec<HookEntry>,
}

/// 读取工作区 .novacode/diagnostics 的诊断命令（首个非空非注释行）；不存在则 None。
/// 例：写入 `cargo check` 或 `npm run typecheck`，Agent 改完代码收尾前自动跑、有错则修。
fn read_diagnostics_command(workspace: &str) -> Option<String> {
    let path = std::path::Path::new(workspace).join(".novacode").join("diagnostics");
    let content = std::fs::read_to_string(path).ok()?;
    content
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_string)
}

fn load_hooks(workspace: &str) -> HooksConfig {
    let path = std::path::Path::new(workspace).join(".novacode").join("hooks.json");
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// matcher 匹配工具名：`*` 匹配全部，否则按 `,` 或 `|` 分隔的工具名列表精确匹配。
fn hook_matcher_matches(matcher: &str, tool_name: &str) -> bool {
    let m = matcher.trim();
    m == "*"
        || m.split([',', '|'])
            .any(|t| t.trim() == tool_name)
}

/// 运行一条钩子命令：JSON 从 stdin 传入，30s 超时，返回 (是否成功, 输出文本)。
fn run_hook_command(workspace: &str, command: &str, stdin_json: &str) -> (bool, String) {
    use std::io::{Read, Write};
    use std::process::{Command, Stdio};
    let mut child = match Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", command])
        .current_dir(workspace)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return (false, format!("钩子启动失败: {e}")),
    };
    if let Some(mut si) = child.stdin.take() {
        let _ = si.write_all(stdin_json.as_bytes());
    }
    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();
    let so = std::thread::spawn(move || {
        let mut s = String::new();
        if let Some(mut p) = stdout_pipe {
            let _ = p.read_to_string(&mut s);
        }
        s
    });
    let se = std::thread::spawn(move || {
        let mut s = String::new();
        if let Some(mut p) = stderr_pipe {
            let _ = p.read_to_string(&mut s);
        }
        s
    });
    let start = std::time::Instant::now();
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) => {
                if start.elapsed() > std::time::Duration::from_secs(30) {
                    let _ = child.kill();
                    timed_out = true;
                    break None;
                }
                std::thread::sleep(std::time::Duration::from_millis(40));
            }
            Err(_) => break None,
        }
    };
    let out = so.join().unwrap_or_default();
    let err = se.join().unwrap_or_default();
    let text = format!("{out}{err}").trim().to_string();
    if timed_out {
        return (false, "钩子执行超时（30s）".to_string());
    }
    let ok = status.map(|s| s.success()).unwrap_or(false);
    (ok, text)
}

/// 运行匹配的 PreToolUse 钩子；任一钩子退出码非 0 则返回 Some(阻断原因)。
fn run_pre_tool_hooks(workspace: &str, tool_name: &str, arguments: &str) -> Option<String> {
    let cfg = load_hooks(workspace);
    if cfg.pre_tool_use.is_empty() {
        return None;
    }
    let args_val: serde_json::Value =
        serde_json::from_str(arguments).unwrap_or(serde_json::Value::Null);
    let payload =
        serde_json::json!({ "hook": "PreToolUse", "tool": tool_name, "arguments": args_val })
            .to_string();
    for hook in &cfg.pre_tool_use {
        if hook_matcher_matches(&hook.matcher, tool_name) {
            let (ok, text) = run_hook_command(workspace, &hook.command, &payload);
            if !ok {
                let reason = if text.is_empty() {
                    "被 PreToolUse 钩子阻断".to_string()
                } else {
                    format!("被 PreToolUse 钩子阻断：{text}")
                };
                return Some(reason);
            }
        }
    }
    None
}

/// 运行匹配的 PostToolUse 钩子（信息性，不阻断），把输出作为 activity 事件展示。
fn run_post_tool_hooks(app: &AppHandle, workspace: &str, tool_name: &str, arguments: &str) {
    let cfg = load_hooks(workspace);
    if cfg.post_tool_use.is_empty() {
        return;
    }
    let args_val: serde_json::Value =
        serde_json::from_str(arguments).unwrap_or(serde_json::Value::Null);
    let payload =
        serde_json::json!({ "hook": "PostToolUse", "tool": tool_name, "arguments": args_val })
            .to_string();
    for hook in &cfg.post_tool_use {
        if hook_matcher_matches(&hook.matcher, tool_name) {
            let (ok, text) = run_hook_command(workspace, &hook.command, &payload);
            let _ = app.emit(
                "tool-event",
                serde_json::json!({
                    "toolName": format!("hook:{tool_name}"),
                    "status": if ok { "succeeded" } else { "failed" },
                    "inputJson": serde_json::json!({ "command": hook.command }).to_string(),
                    "outputJson": serde_json::Value::Null,
                    "errorMessage": if ok || text.is_empty() { None } else { Some(text) },
                }),
            );
        }
    }
}

/// remember 工具：把一条值得跨会话记住的事实追加到工作区 NovaCode.md 的「自动记忆」区。
///
/// 让 Agent 在工作中自主沉淀经验（项目约定、踩过的坑、关键路径），下次会话自动注入。
/// 去重：同样内容已存在则不重复追加。
fn execute_remember(workspace: &str, arguments: &str) -> Result<serde_json::Value, String> {
    const SECTION: &str = "## 自动记忆（由 Agent 维护）";
    let parsed: serde_json::Value =
        serde_json::from_str(arguments).map_err(|e| format!("工具参数解析失败: {e}"))?;
    let fact = parsed
        .get("fact")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or("remember 缺少 fact")?;
    if fact.chars().count() > 500 {
        return Err("单条记忆过长（上限 500 字符），请精炼".to_string());
    }
    let path = std::path::Path::new(workspace).join("NovaCode.md");
    let mut content = std::fs::read_to_string(&path).unwrap_or_default();
    let entry = format!("- {fact}");
    if content.contains(&entry) {
        return Ok(serde_json::json!({ "note": "该记忆已存在，未重复添加" }));
    }
    if content.contains(SECTION) {
        // 在 section 标题后插入
        content = content.replacen(SECTION, &format!("{SECTION}\n{entry}"), 1);
    } else {
        if !content.is_empty() && !content.ends_with('\n') {
            content.push('\n');
        }
        content.push_str(&format!("\n{SECTION}\n{entry}\n"));
    }
    std::fs::write(&path, content).map_err(|e| format!("写入 NovaCode.md 失败: {e}"))?;
    Ok(serde_json::json!({ "note": "已记入 NovaCode.md，下次会话自动生效", "fact": fact }))
}

/// todo_write 工具：更新任务清单并推送给前端实时展示，不触碰文件系统。
fn execute_todo_write(app: &AppHandle, arguments: &str) -> Result<serde_json::Value, String> {
    let parsed: serde_json::Value =
        serde_json::from_str(arguments).map_err(|e| format!("工具参数解析失败: {e}"))?;
    let items = parsed
        .get("items")
        .and_then(|v| v.as_array())
        .ok_or("todo_write 需要 items 数组")?;
    if items.len() > 50 {
        return Err("todo 项过多（上限 50）".to_string());
    }
    let _ = app.emit("todo-update", serde_json::Value::Array(items.clone()));
    Ok(serde_json::json!({
        "count": items.len(),
        "note": "任务清单已更新并实时展示给用户"
    }))
}

/// 把 HTML 粗略转为可读纯文本：去掉 script/style 整块，剥离标签，解码实体，压缩空白。
/// UTF-8 安全：按字符处理，不按字节，避免多字节中文被破坏。
fn html_to_text(html: &str) -> String {
    let mut s = html.to_string();
    // 移除 script / style 整块（每轮重算 lower，保证偏移与 s 同步）
    for tag in ["script", "style"] {
        let open = format!("<{tag}");
        let close = format!("</{tag}>");
        loop {
            let lower = s.to_lowercase();
            let Some(start) = lower.find(&open) else { break };
            match lower[start..].find(&close) {
                Some(rel) => {
                    let end = start + rel + close.len();
                    s.replace_range(start..end, " ");
                }
                None => {
                    s.replace_range(start.., " ");
                    break;
                }
            }
        }
    }
    // 按字符剥离标签
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for ch in s.chars() {
        match ch {
            '<' => {
                in_tag = true;
                out.push(' ');
            }
            '>' => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    let decoded = out
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ");
    decoded
        .lines()
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// 可并行执行的只读工具集合（无副作用，并发安全）。
const PARALLEL_READONLY_TOOLS: &[&str] =
    &["read_file", "list_dir", "search_text", "glob_files", "stat_path", "web_fetch", "web_search"];

/// 执行一个只读工具调用（同步文件工具或异步 web 工具），供并行批量执行。
async fn execute_readonly_call(
    security_context: &SessionSecurityContext,
    tool_name: &str,
    arguments: &str,
) -> Result<serde_json::Value, String> {
    match tool_name {
        "web_fetch" => execute_web_fetch(arguments).await,
        "web_search" => execute_web_search(arguments).await,
        _ => execute_builtin_tool_call(security_context, tool_name, arguments),
    }
}

/// web_fetch 工具：抓取一个 URL 并提取可读正文（截断防膨胀）。仅允许 http/https。
async fn execute_web_fetch(arguments: &str) -> Result<serde_json::Value, String> {
    let parsed: serde_json::Value =
        serde_json::from_str(arguments).map_err(|e| format!("工具参数解析失败: {e}"))?;
    let url = parsed.get("url").and_then(|v| v.as_str()).ok_or("web_fetch 缺少 url")?;
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err("仅支持 http/https URL".to_string());
    }
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .user_agent("NovaCode/1.0 (+https://github.com/aki66938/NovaCode)")
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client.get(url).send().await.map_err(|e| format!("抓取失败: {e}"))?;
    let status = resp.status();
    let ctype = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let body = resp.text().await.map_err(|e| format!("读取响应失败: {e}"))?;
    let text = if ctype.contains("html") || body.trim_start().starts_with('<') {
        html_to_text(&body)
    } else {
        body
    };
    let capped: String = text.chars().take(40_000).collect();
    Ok(serde_json::json!({
        "url": url,
        "status": status.as_u16(),
        "text": capped
    }))
}

/// web_search 工具：通过 DuckDuckGo HTML 端点搜索，返回标题 / 链接 / 摘要列表（无需 API Key）。
async fn execute_web_search(arguments: &str) -> Result<serde_json::Value, String> {
    let parsed: serde_json::Value =
        serde_json::from_str(arguments).map_err(|e| format!("工具参数解析失败: {e}"))?;
    let query = parsed.get("query").and_then(|v| v.as_str()).ok_or("web_search 缺少 query")?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .user_agent("Mozilla/5.0 (compatible; NovaCode/1.0)")
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .post("https://html.duckduckgo.com/html/")
        .form(&[("q", query)])
        .send()
        .await
        .map_err(|e| format!("搜索失败: {e}"))?;
    let html = resp.text().await.map_err(|e| format!("读取响应失败: {e}"))?;
    let results = parse_ddg_results(&html, 8);
    if results.is_empty() {
        return Ok(serde_json::json!({
            "query": query,
            "results": [],
            "note": "未解析到结果（搜索页结构可能变化），可改用 web_fetch 直接抓取已知 URL。"
        }));
    }
    Ok(serde_json::json!({ "query": query, "results": results }))
}

/// 从 DuckDuckGo HTML 结果页解析前 limit 条 { title, url, snippet }。
fn parse_ddg_results(html: &str, limit: usize) -> Vec<serde_json::Value> {
    let mut results = Vec::new();
    // 结果链接锚点：class="result__a" href="..."；摘要：class="result__snippet"
    for chunk in html.split("result__a").skip(1) {
        if results.len() >= limit {
            break;
        }
        let Some(href_pos) = chunk.find("href=\"") else { continue };
        let after = &chunk[href_pos + 6..];
        let Some(end) = after.find('"') else { continue };
        let raw_url = &after[..end];
        let url = decode_ddg_url(raw_url);
        // 标题：href 标签 > 之后到 </a>
        let title = after
            .find('>')
            .and_then(|gt| after[gt + 1..].find("</a>").map(|e| &after[gt + 1..gt + 1 + e]))
            .map(|t| html_to_text(t))
            .unwrap_or_default();
        let snippet = chunk
            .find("result__snippet")
            .and_then(|p| chunk[p..].find('>').map(|gt| &chunk[p + gt + 1..]))
            .and_then(|s| s.find("</a>").map(|e| &s[..e]))
            .map(|s| html_to_text(s))
            .unwrap_or_default();
        if url.starts_with("http") && !title.is_empty() {
            results.push(serde_json::json!({
                "title": title,
                "url": url,
                "snippet": snippet.chars().take(300).collect::<String>()
            }));
        }
    }
    results
}

/// DuckDuckGo 跳转链接 `//duckduckgo.com/l/?uddg=<编码真实URL>` 解码为真实 URL。
fn decode_ddg_url(raw: &str) -> String {
    let target = if let Some(pos) = raw.find("uddg=") {
        let enc = &raw[pos + 5..];
        let enc = enc.split('&').next().unwrap_or(enc);
        percent_decode(enc)
    } else {
        raw.to_string()
    };
    if target.starts_with("//") {
        format!("https:{target}")
    } else {
        target
    }
}

/// 最小 percent-decode（仅用于 DDG 跳转 URL 还原）。
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

/// add_mcp_server 工具：让 Agent 用 NovaCode 真正的机制注册并连接一个 MCP 服务器
/// （写入 SQLite 服务器表 + 后台连接），而不是去编辑无效的配置文件。
fn execute_add_mcp_server(app: &AppHandle, arguments: &str) -> Result<serde_json::Value, String> {
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
fn execute_mcp_tool(
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

/// 构造把命令输出逐行推送到前端的回调（"command-output" 事件）。
fn command_line_callback(app: &AppHandle) -> novacode_tool_runtime::CommandLineCallback {
    let app = app.clone();
    std::sync::Arc::new(move |line: String| {
        let _ = app.emit("command-output", line);
    })
}

/// run_command 工具的桌面端执行：前台流式输出；background=true 时立即返回、
/// 后台线程跑完后通过 "background-command-done" 事件通知。
fn execute_run_command_tool(
    app: &AppHandle,
    security_context: &SessionSecurityContext,
    arguments: &str,
) -> Result<serde_json::Value, String> {
    let request = serde_json::from_str::<RunCommandRequest>(arguments)
        .map_err(|e| format!("工具参数解析失败: {e}"))?;
    let workspace = security_context.workspace_root.clone();

    if request.background {
        let shell_id = format!("sh-{}", BG_SHELL_SEQ.fetch_add(1, Ordering::SeqCst));
        let output = Arc::new(Mutex::new(String::new()));
        let status = Arc::new(Mutex::new("running".to_string()));
        let cancel = Arc::new(AtomicBool::new(false));
        // 注册到 AppState，供 get_shell_output / kill_shell / list_shells 访问。
        {
            let st = app.state::<AppState>();
            let mut shells = st.bg_shells.lock().expect("bg_shells mutex poisoned");
            shells.insert(
                shell_id.clone(),
                BgShell {
                    command: request.command.clone(),
                    output: output.clone(),
                    status: status.clone(),
                    cancel: cancel.clone(),
                },
            );
        }
        let app_bg = app.clone();
        let command_label = request.command.clone();
        let out_buf = output.clone();
        let cancel_thread = cancel.clone();
        std::thread::spawn(move || {
            // 输出逐行累积到共享缓冲（尾部截断 32K），供轮询；同时实时推前端。
            let app_line = app_bg.clone();
            let cb: novacode_tool_runtime::CommandLineCallback = std::sync::Arc::new(move |line: String| {
                if let Ok(mut buf) = out_buf.lock() {
                    buf.push_str(&line);
                    buf.push('\n');
                    let len = buf.chars().count();
                    if len > 32_000 {
                        *buf = buf.chars().skip(len - 32_000).collect();
                    }
                }
                let _ = app_line.emit("command-output", line);
            });
            // 后台命令暂不沙箱化（沙箱路径不支持轮询/kill），M8.2 再统一。
            let outcome = novacode_tool_runtime::run_command_streaming(
                &workspace,
                RunCommandRequest { background: false, ..request },
                Some(cb),
                Some(cancel_thread.clone()),
                false,
            );
            let final_status = if cancel_thread.load(Ordering::SeqCst) {
                "killed"
            } else if outcome.is_err() {
                "error"
            } else {
                "done"
            };
            if let Ok(mut s) = status.lock() {
                *s = final_status.to_string();
            }
            let _ = app_bg.emit(
                "background-command-done",
                serde_json::json!({ "command": command_label, "status": final_status }),
            );
        });
        return Ok(serde_json::json!({
            "background": true,
            "shellId": shell_id,
            "note": "命令已在后台启动。用 get_shell_output 轮询输出、kill_shell 终止；你无需等待，继续后续步骤。"
        }));
    }

    // 前台命令：按设置决定是否在受限令牌沙箱中执行。
    let sandbox = app.state::<AppState>().command_sandbox.load(Ordering::SeqCst);
    let cb = command_line_callback(app);
    serde_json::to_value(
        novacode_tool_runtime::run_command_streaming(&workspace, request, Some(cb), None, sandbox)
            .map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())
}

/// 后台 shell 工具：get_shell_output / kill_shell / list_shells。从 AppState 注册表读取/控制。
fn execute_bg_shell_tool(
    app: &AppHandle,
    tool_name: &str,
    arguments: &str,
) -> Result<serde_json::Value, String> {
    let st = app.state::<AppState>();
    let shells = st.bg_shells.lock().map_err(|e| e.to_string())?;
    match tool_name {
        "list_shells" => {
            let list: Vec<serde_json::Value> = shells
                .iter()
                .map(|(id, sh)| {
                    serde_json::json!({
                        "shellId": id,
                        "command": sh.command,
                        "status": sh.status.lock().map(|s| s.clone()).unwrap_or_default(),
                    })
                })
                .collect();
            Ok(serde_json::json!({ "shells": list }))
        }
        "get_shell_output" | "kill_shell" => {
            let parsed: serde_json::Value =
                serde_json::from_str(arguments).map_err(|e| format!("工具参数解析失败: {e}"))?;
            let id = parsed.get("shellId").and_then(|v| v.as_str()).ok_or("缺少 shellId")?;
            let sh = shells.get(id).ok_or("shellId 不存在")?;
            if tool_name == "kill_shell" {
                sh.cancel.store(true, Ordering::SeqCst);
                return Ok(serde_json::json!({ "shellId": id, "note": "已请求终止该后台命令" }));
            }
            let output = sh.output.lock().map(|o| o.clone()).unwrap_or_default();
            let status = sh.status.lock().map(|s| s.clone()).unwrap_or_default();
            Ok(serde_json::json!({ "shellId": id, "status": status, "output": output }))
        }
        other => Err(format!("未知后台 shell 工具: {other}")),
    }
}

/// 子任务探索代理可用的只读工具集合。
fn read_only_tool_schemas() -> Vec<serde_json::Value> {
    const READ_ONLY: &[&str] = &["list_dir", "read_file", "search_text", "glob_files", "stat_path"];
    all_builtin_tool_schemas()
        .into_iter()
        .filter(|schema| {
            schema
                .pointer("/function/name")
                .and_then(|n| n.as_str())
                .map(|name| READ_ONLY.contains(&name))
                .unwrap_or(false)
        })
        .collect()
}

const SUBTASK_MAX_ROUNDS: usize = 15;

/// run_subtask 工具：用独立上下文跑一个只读探索子代理，返回简明报告与消耗的 usage。
/// 子代理只能 list/read/search/stat，工具事件以 `sub:` 前缀推给前端展示。
async fn execute_run_subtask(
    api_key: &str,
    model: &str,
    workspace_path: &str,
    arguments: &str,
    app: &AppHandle,
    conversation_id: &str,
) -> (Result<serde_json::Value, String>, Option<novacode_shared::RawUsage>) {
    let parsed: serde_json::Value = match serde_json::from_str(arguments) {
        Ok(value) => value,
        Err(e) => return (Err(format!("工具参数解析失败: {e}")), None),
    };
    let Some(description) = parsed.get("description").and_then(|v| v.as_str()) else {
        return (Err("run_subtask 缺少 description".to_string()), None);
    };
    // 自定义子代理类型：agentType 指向 .novacode/agents/<type>.md，其内容作为子代理 system prompt。
    let custom_agent = parsed
        .get("agentType")
        .and_then(|v| v.as_str())
        .filter(|t| t.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'))
        .and_then(|t| {
            std::fs::read_to_string(
                std::path::Path::new(workspace_path).join(".novacode").join("agents").join(format!("{t}.md")),
            )
            .ok()
        });
    let system_prompt = match custom_agent {
        Some(def) => format!(
            "你是一个只读探索子代理，工作区路径是 {workspace_path}。你只能使用 list_dir、read_file、search_text、glob_files、stat_path 这些只读工具，禁止写入或命令执行。以下是你的角色定义：\n{def}\n完成后输出简明中文报告。"
        ),
        None => format!(
            "你是一个只读探索子代理，工作区路径是 {workspace_path}。你只能使用 list_dir、read_file、search_text、glob_files、stat_path 这些只读工具调查代码与文件，禁止任何写入或命令执行。完成调查后，输出一份简明、信息密度高的中文报告，直接回答委托内容，不要寒暄。"
        ),
    };

    let background = parsed.get("background").and_then(|v| v.as_bool()).unwrap_or(false);
    if background {
        // 注册后台任务并 spawn 独立 loop；立即返回 taskId，主循环不等待。
        let task_id = format!("task-{}", BG_TASK_SEQ.fetch_add(1, Ordering::SeqCst));
        let report = Arc::new(Mutex::new(String::new()));
        let status = Arc::new(Mutex::new("running".to_string()));
        let usage_slot: Arc<Mutex<Option<novacode_shared::RawUsage>>> = Arc::new(Mutex::new(None));
        let cancel = Arc::new(AtomicBool::new(false));
        {
            let st = app.state::<AppState>();
            let mut tasks = st.bg_tasks.lock().expect("bg_tasks mutex poisoned");
            tasks.insert(
                task_id.clone(),
                BgTask {
                    description: description.to_string(),
                    report: report.clone(),
                    status: status.clone(),
                    usage: usage_slot.clone(),
                    settled: Arc::new(AtomicBool::new(false)),
                    cancel: cancel.clone(),
                },
            );
        }
        let api_key = api_key.to_string();
        let model = model.to_string();
        let workspace_owned = workspace_path.to_string();
        let conversation_owned = conversation_id.to_string();
        let description_owned = description.to_string();
        let app_bg = app.clone();
        let task_id_done = task_id.clone();
        tauri::async_runtime::spawn(async move {
            let (text, sub_usage) = run_subtask_loop(
                &api_key,
                &model,
                &workspace_owned,
                &system_prompt,
                &description_owned,
                &app_bg,
                &conversation_owned,
                Some(cancel.clone()),
            )
            .await;
            let final_status = if cancel.load(Ordering::SeqCst) { "killed" } else { "done" };
            if let Ok(mut r) = report.lock() {
                *r = text;
            }
            if let Ok(mut u) = usage_slot.lock() {
                *u = sub_usage;
            }
            if let Ok(mut s) = status.lock() {
                *s = final_status.to_string();
            }
            let _ = app_bg.emit(
                "background-task-done",
                serde_json::json!({ "taskId": task_id_done, "status": final_status }),
            );
        });
        return (
            Ok(serde_json::json!({
                "background": true,
                "taskId": task_id,
                "note": "子代理已在后台启动。用 get_task_output 轮询报告/状态、kill_task 终止；你无需等待，继续后续步骤。"
            })),
            None,
        );
    }

    // 前台：同步等待子代理 loop 完成，usage 并入当轮账本。
    let (report, usage) = run_subtask_loop(
        api_key,
        model,
        workspace_path,
        &system_prompt,
        description,
        app,
        conversation_id,
        None,
    )
    .await;
    (Ok(serde_json::json!({ "report": report })), usage)
}

/// 子代理工具循环主体（前台/后台共用）。后台模式下每轮检查 cancel 以支持 kill_task。
#[allow(clippy::too_many_arguments)]
async fn run_subtask_loop(
    api_key: &str,
    model: &str,
    workspace_path: &str,
    system_prompt: &str,
    description: &str,
    app: &AppHandle,
    conversation_id: &str,
    cancel: Option<Arc<AtomicBool>>,
) -> (String, Option<novacode_shared::RawUsage>) {
    let security_context = match session_security_context(
        workspace_path.to_string(),
        PermissionMode::Restricted,
        NetworkMode::Disabled,
    ) {
        Ok(ctx) => ctx,
        Err(e) => return (format!("子代理初始化失败: {e}"), None),
    };

    let mut wire = vec![
        serde_json::json!({ "role": "system", "content": system_prompt }),
        serde_json::json!({ "role": "user", "content": description }),
    ];
    let mut usage: Option<novacode_shared::RawUsage> = None;
    let mut report = String::new();

    for _ in 0..SUBTASK_MAX_ROUNDS {
        if cancel.as_ref().map(|c| c.load(Ordering::SeqCst)).unwrap_or(false) {
            break;
        }
        let completion = match chat_completion_with_retry(
            api_key,
            wire.clone(),
            model,
            Some(read_only_tool_schemas()),
            app,
        )
        .await
        {
            Ok(result) => result,
            Err(e) => return (format!("子代理出错: {e}"), usage),
        };
        usage = merge_usage(usage, completion.usage.clone());

        let tool_calls = if completion.tool_calls.is_empty() {
            completion
                .content
                .as_deref()
                .map(recover_tool_calls_from_content)
                .unwrap_or_default()
        } else {
            completion.tool_calls.clone()
        };

        if tool_calls.is_empty() {
            report = completion
                .content
                .as_deref()
                .map(strip_dsml_markup)
                .unwrap_or_default();
            break;
        }

        wire.push(assistant_message_for_tool_calls(
            completion.assistant_message.clone(),
            &tool_calls,
        ));
        for call in &tool_calls {
            let name = call.function.name.as_str();
            let display_name = format!("sub:{name}");
            record_tool_event(
                app,
                conversation_id,
                "tool_started",
                &display_name,
                "running",
                &call.function.arguments,
                None,
                None,
                workspace_path,
            );
            let result = if matches!(
                name,
                "list_dir" | "read_file" | "search_text" | "glob_files" | "stat_path"
            ) {
                execute_builtin_tool_call(&security_context, name, &call.function.arguments)
            } else {
                Err("子任务仅允许只读工具".to_string())
            };
            let (output, status, error) = match &result {
                Ok(value) => (
                    serde_json::json!({ "ok": true, "result": value }),
                    "succeeded",
                    None,
                ),
                Err(message) => (
                    serde_json::json!({ "ok": false, "error": message }),
                    "failed",
                    Some(message.clone()),
                ),
            };
            let output_str = output.to_string();
            record_tool_event(
                app,
                conversation_id,
                if status == "succeeded" { "tool_succeeded" } else { "tool_failed" },
                &display_name,
                status,
                &call.function.arguments,
                Some(&output_str),
                error.as_deref(),
                workspace_path,
            );
            wire.push(serde_json::json!({
                "role": "tool",
                "tool_call_id": call.id,
                "content": output_str
            }));
        }
    }

    if report.trim().is_empty() {
        report = "（子任务在限定轮数内未给出报告）".to_string();
    }
    let capped: String = report.chars().take(8_000).collect();
    (capped, usage)
}

/// MCP 资源工具：list_mcp_resources / read_mcp_resource，从 AppState 已连接客户端读取。
fn execute_mcp_resource_tool(
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

/// 后台子代理任务工具：get_task_output / kill_task / list_tasks。
/// get_task_output 在任务完成且未结算时返回其 usage，供主循环并入会话账本（只结算一次）。
async fn execute_bg_task_tool(
    app: &AppHandle,
    tool_name: &str,
    arguments: &str,
) -> (Result<serde_json::Value, String>, Option<novacode_shared::RawUsage>) {
    let parsed: serde_json::Value = serde_json::from_str(arguments).unwrap_or(serde_json::json!({}));
    match tool_name {
        "list_tasks" => {
            let st = app.state::<AppState>();
            let list: Vec<serde_json::Value> = match st.bg_tasks.lock() {
                Ok(tasks) => tasks
                    .iter()
                    .map(|(id, t)| {
                        serde_json::json!({
                            "taskId": id,
                            "description": t.description,
                            "status": t.status.lock().map(|s| s.clone()).unwrap_or_default(),
                        })
                    })
                    .collect(),
                Err(_) => Vec::new(),
            };
            (Ok(serde_json::json!({ "tasks": list })), None)
        }
        "kill_task" => {
            let Some(id) = parsed.get("taskId").and_then(|v| v.as_str()) else {
                return (Err("kill_task 缺少 taskId".to_string()), None);
            };
            let st = app.state::<AppState>();
            let cancel = st.bg_tasks.lock().ok().and_then(|t| t.get(id).map(|t| t.cancel.clone()));
            match cancel {
                Some(cancel) => {
                    cancel.store(true, Ordering::SeqCst);
                    (
                        Ok(serde_json::json!({ "taskId": id, "note": "已请求终止该后台子代理" })),
                        None,
                    )
                }
                None => (Err(format!("taskId 不存在: {id}")), None),
            }
        }
        "get_task_output" => {
            let Some(id) = parsed.get("taskId").and_then(|v| v.as_str()) else {
                return (Err("get_task_output 缺少 taskId".to_string()), None);
            };
            let block = parsed.get("block").and_then(|v| v.as_bool()).unwrap_or(false);
            let timeout_secs = parsed
                .get("timeoutSecs")
                .and_then(|v| v.as_u64())
                .unwrap_or(30)
                .min(120);

            let task = {
                let st = app.state::<AppState>();
                st.bg_tasks.lock().ok().and_then(|t| t.get(id).cloned())
            };
            let Some(task) = task else {
                return (Err(format!("taskId 不存在: {id}")), None);
            };

            if block {
                let deadline_ms = timeout_secs * 1000;
                let mut waited = 0u64;
                loop {
                    let running = task.status.lock().map(|s| *s == "running").unwrap_or(false);
                    if !running || waited >= deadline_ms {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    waited += 500;
                }
            }

            let status = task.status.lock().map(|s| s.clone()).unwrap_or_default();
            let report = task.report.lock().map(|r| r.clone()).unwrap_or_default();
            // 完成且未结算：取出 usage 并入账本（swap 保证只结算一次）。
            let settled_usage = if status != "running" && !task.settled.swap(true, Ordering::SeqCst) {
                task.usage.lock().ok().and_then(|u| u.clone())
            } else {
                None
            };
            (
                Ok(serde_json::json!({ "taskId": id, "status": status, "report": report })),
                settled_usage,
            )
        }
        other => (Err(format!("未知后台任务工具: {other}")), None),
    }
}

/// 工具结果过大时落盘到 .novacode/tool-results/<seq>.txt，返回给模型的省流摘要（含相对路径 + 开头预览，
/// 完整内容可用 read_file 分页读取）；未超阈值则原样返回。落盘失败退回原文，绝不丢内容。
fn maybe_persist_large_output(workspace_path: &str, output_str: &str) -> String {
    const LARGE_OUTPUT_THRESHOLD: usize = 16_000;
    if output_str.chars().count() <= LARGE_OUTPUT_THRESHOLD {
        return output_str.to_string();
    }
    let seq = LARGE_OUTPUT_SEQ.fetch_add(1, Ordering::SeqCst);
    let rel = format!(".novacode/tool-results/{seq}.txt");
    let dir = std::path::Path::new(workspace_path).join(".novacode").join("tool-results");
    let full = dir.join(format!("{seq}.txt"));
    let bytes = output_str.len();
    let head: String = output_str.chars().take(2_000).collect();
    if std::fs::create_dir_all(&dir).is_ok() && std::fs::write(&full, output_str).is_ok() {
        serde_json::json!({
            "ok": true,
            "note": format!("工具输出过大（约 {bytes} 字节），已落盘以节省上下文。如需完整内容，用 read_file 读取 persistedPath（支持 offset/limit 分页）。"),
            "persistedPath": rel,
            "bytes": bytes,
            "head": head
        })
        .to_string()
    } else {
        output_str.to_string()
    }
}

/// 大工具输出落盘自增序号。
static LARGE_OUTPUT_SEQ: AtomicU64 = AtomicU64::new(1);

/// 读取上下文压缩软上限：优先取环境变量 NOVACODE_CONTEXT_SOFT_LIMIT（便于低阈值压测），否则用默认值。
fn context_soft_limit_tokens() -> u64 {
    std::env::var("NOVACODE_CONTEXT_SOFT_LIMIT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(CONTEXT_SOFT_LIMIT_TOKENS)
}

/// 计算摘要压缩的切分点：返回（开头连续 system 消息数, 摘要区终点）。
///
/// 开头的 system 消息（工作区上下文/工具规则/repo map/长期记忆）永不压缩；
/// 末尾保留 keep_recent 条原文；切分点不允许落在 tool 结果上（向前回退，保证
/// assistant 的 tool_calls 与其 tool 结果不被拆散）。历史不够长时返回 None。
fn summary_split_points(
    wire: &[serde_json::Value],
    keep_recent: usize,
) -> Option<(usize, usize)> {
    let first_non_system = wire
        .iter()
        .position(|m| m.get("role").and_then(|r| r.as_str()) != Some("system"))
        .unwrap_or(wire.len());
    if wire.len().saturating_sub(first_non_system) <= keep_recent {
        return None;
    }
    let mut cut = wire.len() - keep_recent;
    while cut > first_non_system
        && wire[cut].get("role").and_then(|r| r.as_str()) == Some("tool")
    {
        cut -= 1;
    }
    if cut <= first_non_system {
        return None;
    }
    Some((first_non_system, cut))
}

/// 把单条 wire 消息渲染成供摘要模型阅读的紧凑单行文本（角色 + 截断正文 + 工具调用名）。
fn render_wire_message_for_summary(message: &serde_json::Value) -> String {
    const MAX_CHARS: usize = 600;
    let role = message.get("role").and_then(|r| r.as_str()).unwrap_or("?");
    let mut body = message
        .get("content")
        .and_then(|c| c.as_str())
        .unwrap_or("")
        .to_string();
    if let Some(calls) = message.get("tool_calls").and_then(|c| c.as_array()) {
        let names: Vec<String> = calls
            .iter()
            .map(|call| {
                let name = call
                    .pointer("/function/name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                let args: String = call
                    .pointer("/function/arguments")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .chars()
                    .take(120)
                    .collect();
                format!("{name}({args})")
            })
            .collect();
        body = format!("{body} [调用工具: {}]", names.join(", "));
    }
    let truncated: String = body.chars().take(MAX_CHARS).collect();
    format!("[{role}] {truncated}")
}

/// 跨轮摘要压缩（auto-compact）：把较早的对话历史压缩成任务进度摘要，替换原文继续任务。
///
/// 这是 Claude Code / Codex 同款思路：摘要式而非删除式。保留开头 system 消息与最近
/// KEEP_RECENT_WIRE_MESSAGES 条原文，中间历史经一次无工具模型调用压缩为
/// 「目标/已完成/关键决策/文件改动/下一步」备忘录，以 system 消息插回，保证任务方向不丢。
/// 返回压缩后的消息序列与摘要调用消耗的 usage；历史太短时原样返回。
async fn summarize_wire_history(
    api_key: &str,
    model: &str,
    wire_messages: Vec<serde_json::Value>,
    app: &AppHandle,
) -> Result<(Vec<serde_json::Value>, Option<novacode_shared::RawUsage>), String> {
    let Some((first_non_system, cut)) =
        summary_split_points(&wire_messages, KEEP_RECENT_WIRE_MESSAGES)
    else {
        return Ok((wire_messages, None));
    };

    let transcript: String = wire_messages[first_non_system..cut]
        .iter()
        .map(render_wire_message_for_summary)
        .collect::<Vec<_>>()
        .join("\n");
    let prompt = format!(
        "你是对话压缩器。请把下面这段 AI Agent 的历史对话压缩成简明的中文任务备忘录，\
用于替换原始历史、让 Agent 继续执行任务。备忘录必须包含：\
1）用户的总体目标；2）已完成的事项；3）关键决策与原因；\
4）已创建/修改/删除的文件清单；5）当前进度与下一步计划。只输出备忘录本身，不要寒暄。\n\n\
=== 历史对话开始 ===\n{transcript}\n=== 历史对话结束 ==="
    );

    let result = chat_completion_with_retry(
        api_key,
        vec![serde_json::json!({ "role": "user", "content": prompt })],
        model,
        None,
        app,
    )
    .await?;
    let summary = result.content.unwrap_or_default();

    let mut compacted: Vec<serde_json::Value> = wire_messages[..first_non_system].to_vec();
    compacted.push(serde_json::json!({
        "role": "system",
        "content": format!(
            "早前对话已自动压缩。以下是任务进度摘要，请严格按其继续推进，不要偏离原始目标：\n{summary}"
        )
    }));
    compacted.extend_from_slice(&wire_messages[cut..]);
    Ok((compacted, result.usage))
}

/// 压缩 wire_messages 中较早的大体积工具结果，保留最近 keep_recent 个全文。
///
/// 输入构建中的 wire 消息序列；把除最近 keep_recent 个之外、正文超过 stub_threshold 字符的
/// `role==tool` 消息正文替换为短桩，返回被压缩的条数。只动工具结果正文，**不动** assistant 的
/// 工具调用与叙述，因此模型的推理链路和任务方向保持完整，只是丢弃了可重新获取的大体积数据。
/// 幂等：已是短桩的消息会跳过。
fn compact_tool_outputs(
    wire_messages: &mut [serde_json::Value],
    keep_recent: usize,
    stub_threshold: usize,
) -> usize {
    let tool_indices: Vec<usize> = wire_messages
        .iter()
        .enumerate()
        .filter(|(_, msg)| msg.get("role").and_then(|r| r.as_str()) == Some("tool"))
        .map(|(idx, _)| idx)
        .collect();
    if tool_indices.len() <= keep_recent {
        return 0;
    }
    let cutoff = tool_indices.len() - keep_recent;
    let mut compacted = 0;
    for &idx in &tool_indices[..cutoff] {
        let content = wire_messages[idx]
            .get("content")
            .and_then(|c| c.as_str())
            .unwrap_or("");
        if content.len() <= stub_threshold || content == COMPACTED_TOOL_STUB {
            continue;
        }
        if let Some(obj) = wire_messages[idx].as_object_mut() {
            obj.insert(
                "content".to_string(),
                serde_json::Value::String(COMPACTED_TOOL_STUB.to_string()),
            );
            compacted += 1;
        }
    }
    compacted
}

/// 带退避重试的 chat_completion，容忍偶发网络抖动 / 5xx / 限流，避免一次瞬时失败打断整轮长任务。
///
/// 可重试错误（网络收发失败、服务端错误、429）按 0.5s→1s→2s 退避重试，最多 4 次；
/// 确定性错误（认证、余额、参数、上下文超限）立即返回不重试。重试时向前端推送提示。
async fn chat_completion_with_retry(
    api_key: &str,
    messages: Vec<serde_json::Value>,
    model: &str,
    tools: Option<Vec<serde_json::Value>>,
    app: &AppHandle,
) -> Result<novacode_deepseek_client::ChatCompletionResult, String> {
    const MAX_ATTEMPTS: u32 = 4;
    let mut attempt = 0;
    loop {
        attempt += 1;
        match chat_completion(api_key, messages.clone(), model, tools.clone()).await {
            Ok(result) => return Ok(result),
            Err(err) if err.is_retryable() && attempt < MAX_ATTEMPTS => {
                let delay_ms = 500u64 * 2u64.pow(attempt - 1);
                let _ = app.emit(
                    "chat-chunk",
                    format!("\n\n（网络波动，正在重试（第 {attempt}/{} 次）…）\n\n", MAX_ATTEMPTS - 1),
                );
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
            }
            Err(err) => return Err(err.to_string()),
        }
    }
}

/// 带退避重试的流式工具轮：边流边把叙述 token 推到前端（"chat-chunk"），同时累积 tool_calls。
///
/// 仅在「尚未外显任何内容」时才对可重试错误重试，避免重试导致内容重复外显；
/// 一旦已开始流式输出再失败，则直接返回错误。
/// 返回同族备用模型：flash↔pro 互为 fallback，持续失败（如 overload）时切换求生。
fn fallback_model_for(model: &str) -> Option<&'static str> {
    match model {
        "deepseek-v4-flash" => Some("deepseek-v4-pro"),
        "deepseek-v4-pro" => Some("deepseek-v4-flash"),
        _ => None,
    }
}

async fn stream_round_with_retry(
    api_key: &str,
    messages: Vec<serde_json::Value>,
    model: &str,
    tools: Vec<serde_json::Value>,
    app: &AppHandle,
) -> Result<novacode_deepseek_client::ChatCompletionResult, String> {
    const MAX_ATTEMPTS: u32 = 4;
    let mut model = model.to_string();
    let mut fallback_used = false;
    let mut attempt = 0;
    loop {
        attempt += 1;
        let emitted = Arc::new(AtomicBool::new(false));
        let emitted_cb = emitted.clone();
        let app_cb = app.clone();
        let result = chat_stream_with_tools(
            api_key,
            messages.clone(),
            &model,
            tools.clone(),
            move |chunk| {
                emitted_cb.store(true, Ordering::SeqCst);
                let _ = app_cb.emit("chat-chunk", chunk);
            },
        )
        .await;
        match result {
            Ok(value) => return Ok(value),
            Err(err) if err.is_retryable() && !emitted.load(Ordering::SeqCst) => {
                if attempt < MAX_ATTEMPTS {
                    let delay_ms = 500u64 * 2u64.pow(attempt - 1);
                    let _ = app.emit(
                        "chat-chunk",
                        format!("\n\n（网络波动，正在重试（第 {attempt}/{} 次）…）\n\n", MAX_ATTEMPTS - 1),
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                } else if !fallback_used {
                    // 主模型多次失败（overload/限流）：切同族备用模型再试一轮。
                    match fallback_model_for(&model) {
                        Some(fb) => {
                            let _ = app.emit(
                                "chat-chunk",
                                format!("\n\n（主模型持续不可用，已切换备用模型 {fb} 重试…）\n\n"),
                            );
                            model = fb.to_string();
                            fallback_used = true;
                            attempt = 0;
                        }
                        None => return Err(err.to_string()),
                    }
                } else {
                    return Err(err.to_string());
                }
            }
            Err(err) => return Err(err.to_string()),
        }
    }
}

/// Agent 工具循环：每轮带工具问模型、执行返回的工具、把结果回灌，直到模型不再调用工具
/// （自然终止）或用户中断。不设轮数上限——上下文自动压缩兜底体积，取消按钮兜底失控；
/// 所有工具执行前都经 SessionSecurityContext 裁决。
#[allow(clippy::too_many_arguments)]
async fn chat_with_builtin_tools(
    api_key: &str,
    messages: Vec<ChatMessage>,
    model: &str,
    workspace_path: &str,
    permission_mode: PermissionMode,
    conversation_id: &str,
    app: &AppHandle,
    cancel: Arc<AtomicBool>,
    permission_rules: Vec<String>,
    mcp_bindings: Vec<McpBinding>,
) -> Result<Option<novacode_shared::RawUsage>, String> {
    let security_context = session_security_context(
        workspace_path.to_string(),
        permission_mode,
        NetworkMode::Disabled,
    )
    .map_err(|e| e.to_string())?;
    // 工具 schema：Built-in + 已连接 MCP server 的外部工具。
    let tool_schemas: Vec<serde_json::Value> = all_builtin_tool_schemas()
        .into_iter()
        .chain(mcp_bindings.iter().map(|b| b.schema.clone()))
        .collect();
    let mut wire_messages = chat_messages_to_wire(messages);
    let mut usage: Option<novacode_shared::RawUsage> = None;
    // 上一次响应返回的 prompt_tokens，作为当前上下文体积的真实信号，驱动轮内压缩。
    let mut last_prompt_tokens: u64 = 0;
    // 诊断反馈环：记录本轮是否发生文件改动 + 是否已跑过诊断（最多一轮，防循环）。
    let mut edits_made = false;
    let mut diagnostics_ran = false;

    let mut round: usize = 0;
    loop {
        round += 1;
        // 轮次之间检查取消：用户点击停止后安全收尾，保留已执行的工具结果。
        if cancel.load(Ordering::SeqCst) {
            let _ = app.emit("chat-chunk", "\n\n（已中断）".to_string());
            return Ok(usage);
        }

        // Steering：取出用户在运行中排队的插话，作为 user 消息注入本轮，让模型即时纠偏。
        let steering_msgs: Vec<String> = {
            let state = app.state::<AppState>();
            state
                .steering
                .lock()
                .ok()
                .and_then(|mut map| map.get_mut(conversation_id).map(std::mem::take))
                .unwrap_or_default()
        };
        for msg in steering_msgs {
            wire_messages.push(serde_json::json!({ "role": "user", "content": msg }));
            let _ = app.emit("steering-injected", &msg);
        }

        // 两级上下文压缩：超软上限先把较早的大体积工具结果换成短桩（机械、零成本）；
        // 若已无桩可压仍超限，触发摘要压缩（auto-compact），把旧历史压成任务进度摘要。
        let soft_limit = context_soft_limit_tokens();
        if last_prompt_tokens > soft_limit {
            let compacted = compact_tool_outputs(
                &mut wire_messages,
                KEEP_RECENT_TOOL_RESULTS,
                TOOL_RESULT_STUB_THRESHOLD,
            );
            if compacted > 0 {
                let _ = app.emit(
                    "context-compacted",
                    serde_json::json!({ "kind": "stub", "count": compacted }),
                );
            } else {
                let _ = app.emit("agent-status", serde_json::json!({ "state": "compacting" }));
                let (new_wire, summary_usage) =
                    summarize_wire_history(api_key, model, std::mem::take(&mut wire_messages), app)
                        .await?;
                wire_messages = new_wire;
                usage = merge_usage(usage, summary_usage);
                // 重置体积信号，待下一次响应的真实 usage 刷新，避免连续重复触发。
                last_prompt_tokens = 0;
                let _ = app.emit(
                    "context-compacted",
                    serde_json::json!({ "kind": "summary" }),
                );
            }
        }

        // 推送轮次进度与思考状态，让前端展示「第 N 轮 · 思考中」而非黑盒等待。
        let _ = app.emit("agent-round", round);
        let _ = app.emit(
            "agent-status",
            serde_json::json!({ "state": "thinking", "round": round }),
        );

        // 流式获取本轮结果：叙述 token 边流边显（内置标记防泄漏守卫），同时累积 tool_calls。
        let completion =
            stream_round_with_retry(api_key, wire_messages.clone(), model, tool_schemas.clone(), app)
                .await?;
        usage = merge_usage(usage, completion.usage.clone());
        // 成本预算：累计 total_tokens 超过预算则暂停（防失控烧 token）。
        let budget = app.state::<AppState>().task_token_budget.load(Ordering::SeqCst);
        if budget > 0 {
            if let Some(u) = usage.as_ref() {
                if u.total_tokens >= budget {
                    let _ = app.emit(
                        "chat-chunk",
                        format!(
                            "\n\n（已达本次任务 token 预算 {budget}，已暂停以控制费用。如需继续，请回复\"继续\"或在设置中调高预算。）"
                        ),
                    );
                    return Ok(usage);
                }
            }
        }
        if let Some(round_usage) = completion.usage.as_ref() {
            last_prompt_tokens = round_usage.prompt_tokens;
            // 推送上下文用量，前端常驻显示占用百分比（对标 CC/Codex 的 context 指示器）。
            let _ = app.emit(
                "context-usage",
                serde_json::json!({
                    "promptTokens": round_usage.prompt_tokens,
                    "softLimit": soft_limit
                }),
            );
        }

        // tool_calls 优先取结构化（流式 delta 累积），为空时从正文兜底解析 DSML / <ToolCall> 变体。
        // 叙述内容已在流式过程中实时外显（守卫防标记泄漏），此处不再重复 emit。
        let tool_calls = if !completion.tool_calls.is_empty() {
            completion.tool_calls.clone()
        } else {
            completion
                .content
                .as_deref()
                .map(recover_tool_calls_from_content)
                .unwrap_or_default()
        };

        // 模型不再调用工具：本轮叙述即最终回复。收尾前若发生过改动且配置了诊断命令，
        // 自动跑一次（typecheck/lint）；有错则回灌让 agent 修复后再收尾（最多一轮）。
        if tool_calls.is_empty() {
            if edits_made && !diagnostics_ran {
                if let Some(cmd) = read_diagnostics_command(workspace_path) {
                    diagnostics_ran = true;
                    let _ = app.emit("agent-status", serde_json::json!({ "state": "thinking", "round": round }));
                    let _ = app.emit("chat-chunk", "\n\n（正在运行诊断检查…）\n\n".to_string());
                    if let Ok(result) = novacode_tool_runtime::run_command(
                        workspace_path,
                        RunCommandRequest { command: cmd.clone(), timeout_secs: Some(180), background: false },
                    ) {
                        let failed = result.exit_code.unwrap_or(0) != 0 || result.timed_out;
                        if failed {
                            let out: String = format!("{}\n{}", result.stdout, result.stderr)
                                .chars().take(6000).collect();
                            wire_messages.push(serde_json::json!({
                                "role": "user",
                                "content": format!("诊断命令 `{cmd}` 报告了问题，请修复后再结束：\n{out}")
                            }));
                            continue; // 回到循环让 agent 修
                        }
                    }
                }
            }
            return Ok(usage);
        }

        wire_messages.push(assistant_message_for_tool_calls(
            completion.assistant_message,
            &tool_calls,
        ));

        // 并行快路径：当本轮多个调用全部是「自动放行的只读工具」时并发执行（读多文件 / 抓多 URL 提速）。
        let all_parallel_readonly = tool_calls.len() > 1
            && tool_calls.iter().all(|call| {
                PARALLEL_READONLY_TOOLS.contains(&call.function.name.as_str())
                    && matches!(
                        gate_tool_decision(
                            &security_context,
                            &call.function.name,
                            &call.function.arguments,
                            &permission_rules,
                        ),
                        ToolGate::Allow
                    )
            });
        if all_parallel_readonly {
            if cancel.load(Ordering::SeqCst) {
                let _ = app.emit("chat-chunk", "\n\n（已中断）".to_string());
                return Ok(usage);
            }
            // 先发运行事件（卡片同时出现），再并发执行。
            for call in &tool_calls {
                record_tool_event(
                    app, conversation_id, "tool_started", &call.function.name, "running",
                    &call.function.arguments, None, None, workspace_path,
                );
            }
            let results = futures_util::future::join_all(tool_calls.iter().map(|call| {
                let sec = &security_context;
                async move {
                    execute_readonly_call(sec, &call.function.name, &call.function.arguments).await
                }
            }))
            .await;
            for (call, result) in tool_calls.iter().zip(results.into_iter()) {
                let (output, status, error) = match &result {
                    Ok(value) => (serde_json::json!({ "ok": true, "result": value }), "succeeded", None),
                    Err(message) => (
                        serde_json::json!({ "ok": false, "error": message, "hint": "工具执行失败，请阅读 error 调整后重试或换用其他工具。" }),
                        "failed",
                        Some(message.clone()),
                    ),
                };
                let output_str = output.to_string();
                record_tool_event(
                    app, conversation_id,
                    if status == "succeeded" { "tool_succeeded" } else { "tool_failed" },
                    &call.function.name, status, &call.function.arguments,
                    Some(&output_str), error.as_deref(), workspace_path,
                );
                wire_messages.push(serde_json::json!({
                    "role": "tool", "tool_call_id": call.id, "content": maybe_persist_large_output(workspace_path, &output_str)
                }));
            }
            continue;
        }

        for call in tool_calls {
            // 工具执行前检查取消：避免停止后仍继续执行剩余工具。
            if cancel.load(Ordering::SeqCst) {
                let _ = app.emit("chat-chunk", "\n\n（已中断）".to_string());
                return Ok(usage);
            }
            let tool_name = call.function.name.clone();
            let arguments = call.function.arguments.clone();

            // 权限门控：白名单命令与「总是允许」规则直接放行，否则按权限模式放行 / 审批 / 拒绝。
            let decision =
                gate_tool_decision(&security_context, &tool_name, &arguments, &permission_rules);
            let proceed = match decision {
                ToolGate::Allow => true,
                ToolGate::Deny(reason) => {
                    feed_tool_denial(
                        app,
                        conversation_id,
                        &tool_name,
                        &arguments,
                        workspace_path,
                        &reason,
                        &call.id,
                        &mut wire_messages,
                    );
                    false
                }
                ToolGate::Ask => {
                    let approved =
                        request_tool_approval(app, &tool_name, &arguments).await;
                    if !approved {
                        feed_tool_denial(
                            app,
                            conversation_id,
                            &tool_name,
                            &arguments,
                            workspace_path,
                            "用户拒绝了该操作",
                            &call.id,
                            &mut wire_messages,
                        );
                    }
                    approved
                }
            };
            if !proceed {
                continue;
            }

            // PreToolUse 钩子：用户定义的执行前校验，退出码非 0 则阻断（原因回灌模型）。
            if let Some(reason) = run_pre_tool_hooks(workspace_path, &tool_name, &arguments) {
                feed_tool_denial(
                    app, conversation_id, &tool_name, &arguments, workspace_path,
                    &reason, &call.id, &mut wire_messages,
                );
                continue;
            }

            record_tool_event(
                app,
                conversation_id,
                "tool_started",
                &tool_name,
                "running",
                &arguments,
                None,
                None,
                workspace_path,
            );

            // 写类工具执行前捕获回退快照（rewind 用），必须先于执行。
            let capture = capture_checkpoint_before(workspace_path, &tool_name, &arguments);

            // 特殊工具走专用执行器：MCP 外部工具 / todo / 技能 / 子任务 / 命令（流式 + 后台）。
            let mcp_binding = mcp_bindings.iter().find(|b| b.fn_name == tool_name);
            let result = if let Some(binding) = mcp_binding {
                execute_mcp_tool(app, binding, &arguments)
            } else {
                match tool_name.as_str() {
                "todo_write" => execute_todo_write(app, &arguments),
                "ask_user" => execute_ask_user(app, &arguments).await,
                "load_skill" => execute_load_skill(workspace_path, &arguments),
                "remember" => execute_remember(workspace_path, &arguments),
                "add_mcp_server" => execute_add_mcp_server(app, &arguments),
                "web_fetch" => execute_web_fetch(&arguments).await,
                "web_search" => execute_web_search(&arguments).await,
                "list_shells" | "get_shell_output" | "kill_shell" => {
                    execute_bg_shell_tool(app, &tool_name, &arguments)
                }
                "run_command" => execute_run_command_tool(app, &security_context, &arguments),
                "run_subtask" => {
                    let _ = app.emit(
                        "agent-status",
                        serde_json::json!({ "state": "thinking", "round": round }),
                    );
                    let (sub_result, sub_usage) = execute_run_subtask(
                        api_key,
                        model,
                        workspace_path,
                        &arguments,
                        app,
                        conversation_id,
                    )
                    .await;
                    usage = merge_usage(usage, sub_usage);
                    sub_result
                }
                "list_mcp_resources" | "read_mcp_resource" => {
                    execute_mcp_resource_tool(app, &tool_name, &arguments)
                }
                "get_task_output" | "kill_task" | "list_tasks" => {
                    let (task_result, task_usage) =
                        execute_bg_task_tool(app, &tool_name, &arguments).await;
                    usage = merge_usage(usage, task_usage);
                    task_result
                }
                _ => execute_builtin_tool_call(&security_context, &tool_name, &arguments),
                }
            };

            let (output, status, error) = match &result {
                Ok(value) => {
                    let mut out = serde_json::json!({ "ok": true, "result": value });
                    // 标记本轮发生过文件改动（驱动收尾前的诊断检查）。
                    if capture.is_some() {
                        edits_made = true;
                    }
                    // 文本写类工具：附加行级 diff 供 UI 展示，并把回退快照落库。
                    if let Some(cap) = capture.as_ref() {
                        if let Some((diff, added, removed)) =
                            post_execution_diff(workspace_path, &tool_name, &arguments, cap)
                        {
                            out["diff"] = serde_json::Value::String(diff);
                            out["added"] = serde_json::json!(added);
                            out["removed"] = serde_json::json!(removed);
                        }
                        persist_checkpoint(app, conversation_id, &tool_name, cap);
                    }
                    (out, "succeeded", None)
                }
                Err(message) => (
                    serde_json::json!({
                        "ok": false,
                        "error": message,
                        "hint": "工具执行失败。请阅读 error 判断是参数错误、路径不存在还是命令/环境问题，据此调整后重试或改用其他工具/写法；不要原样重复同一次失败的调用。"
                    }),
                    "failed",
                    Some(message.clone()),
                ),
            };
            let output_str = output.to_string();
            record_tool_event(
                app,
                conversation_id,
                if status == "succeeded" { "tool_succeeded" } else { "tool_failed" },
                &tool_name,
                status,
                &arguments,
                Some(&output_str),
                error.as_deref(),
                workspace_path,
            );

            wire_messages.push(serde_json::json!({
                "role": "tool",
                "tool_call_id": call.id,
                "content": maybe_persist_large_output(workspace_path, &output_str)
            }));

            // PostToolUse 钩子：工具成功后运行用户定义的后处理（如自动格式化 / 跑测试），信息性不阻断。
            if status == "succeeded" {
                run_post_tool_hooks(app, workspace_path, &tool_name, &arguments);
            }
        }
    }
}

fn recover_tool_calls_from_content(content: &str) -> Vec<ToolCall> {
    parse_dsml_tool_calls(content)
}

fn assistant_message_for_tool_calls(
    assistant_message: serde_json::Value,
    tool_calls: &[ToolCall],
) -> serde_json::Value {
    if assistant_message
        .get("tool_calls")
        .and_then(|calls| calls.as_array())
        .is_some()
    {
        return assistant_message;
    }

    serde_json::json!({
        "role": "assistant",
        "content": null,
        "tool_calls": tool_calls
    })
}

fn chat_messages_to_wire(messages: Vec<ChatMessage>) -> Vec<serde_json::Value> {
    messages
        .into_iter()
        .map(|message| {
            serde_json::json!({
                "role": message.role,
                "content": message.content,
            })
        })
        .collect()
}

fn all_builtin_tool_schemas() -> Vec<serde_json::Value> {
    vec![
        file_tool_schema(
            "create_file",
            "Create a new text file inside the current NovaCode conversation workspace. Use this when the user asks to create a file. The path must be relative to the workspace.",
            &["path", "content"],
        ),
        file_tool_schema(
            "write_file",
            "Write or overwrite a full UTF-8 text file inside the current NovaCode conversation workspace. Use this only when the user asks to replace the whole file.",
            &["path", "content"],
        ),
        file_tool_schema(
            "edit_file",
            "Edit an existing UTF-8 text file by replacing oldText with newText. Prefer this for code or text modifications.",
            &["path", "oldText", "newText"],
        ),
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "read_file",
                "description": "Read a UTF-8 text file inside the workspace. Returns up to ~1500 lines by default with total line count and has_more. For large files, page through with offset (0-based start line) and limit (lines).",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Relative path inside the workspace." },
                        "offset": { "type": "integer", "description": "0-based start line. Use with has_more to page large files. Default 0." },
                        "limit": { "type": "integer", "description": "Max lines to return (<= 4000). Default ~1500." }
                    },
                    "required": ["path"],
                    "additionalProperties": false
                }
            }
        }),
        file_tool_schema(
            "delete_file",
            "Delete a single file inside the current NovaCode conversation workspace. Use this when the user asks to remove a file.",
            &["path"],
        ),
        file_tool_schema(
            "list_dir",
            "List entries in a directory inside the current NovaCode conversation workspace. Use this when the user asks what files or folders exist.",
            &["path"],
        ),
        file_tool_schema(
            "make_dir",
            "Create a directory inside the current NovaCode conversation workspace.",
            &["path"],
        ),
        file_tool_schema(
            "stat_path",
            "Return whether a relative workspace path exists and whether it is a file or directory.",
            &["path"],
        ),
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "search_text",
                "description": "Search file CONTENTS recursively inside a workspace directory (ripgrep-style, gitignore-aware, skips hidden/ignored files). Use this to find where text/code appears. For finding files by NAME/path pattern, use glob_files instead.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Relative directory to search in (e.g. \".\" for workspace root)." },
                        "query": { "type": "string", "description": "Search pattern (literal substring, or regex when isRegex=true)." },
                        "isRegex": { "type": "boolean", "description": "Interpret query as a regular expression. Default false." },
                        "outputMode": { "type": "string", "enum": ["content", "files_with_matches", "count"], "description": "content = matching lines (default); files_with_matches = just file paths; count = matches per file." },
                        "caseInsensitive": { "type": "boolean", "description": "Case-insensitive match (-i). Default false." },
                        "multiline": { "type": "boolean", "description": "Allow the pattern to span lines (. matches newlines). Default false." },
                        "context": { "type": "integer", "description": "Lines of context before AND after each match (-C). content mode only." },
                        "beforeContext": { "type": "integer", "description": "Lines of context before each match (-B). content mode only." },
                        "afterContext": { "type": "integer", "description": "Lines of context after each match (-A). content mode only." },
                        "fileType": { "type": "string", "description": "Restrict to a file type, e.g. \"rs\", \"ts\", \"py\", \"json\"." },
                        "glob": { "type": "string", "description": "Restrict to files whose name/path matches this glob, e.g. \"*.rs\" or \"src/**\"." },
                        "maxResults": { "type": "integer", "description": "Cap on returned matches/files/counts." }
                    },
                    "required": ["path", "query"],
                    "additionalProperties": false
                }
            }
        }),
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "glob_files",
                "description": "Find files by NAME/path glob pattern inside the workspace (gitignore-aware), returned newest-first. Use this to locate files (e.g. all \"*.rs\", everything under \"src/**\") rather than searching their contents.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "pattern": { "type": "string", "description": "Glob: supports * ? and ** (e.g. \"**/*.rs\", \"src/**\", \"*.toml\"). A pattern without \"/\" matches by file name in any directory." },
                        "path": { "type": "string", "description": "Relative directory to start from. Defaults to workspace root." },
                        "maxResults": { "type": "integer", "description": "Cap on returned file paths." }
                    },
                    "required": ["pattern"],
                    "additionalProperties": false
                }
            }
        }),
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "move_path",
                "description": "Move or rename a file or directory inside the current NovaCode conversation workspace. Use this for moving/renaming instead of create+delete.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "from": { "type": "string", "description": "Existing relative source path inside the workspace." },
                        "to": { "type": "string", "description": "New relative destination path inside the workspace. Must not already exist." }
                    },
                    "required": ["from", "to"],
                    "additionalProperties": false
                }
            }
        }),
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "delete_dir",
                "description": "Delete a directory inside the current NovaCode conversation workspace. Set recursive=true to delete a non-empty directory.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Relative directory path inside the workspace. Cannot be the workspace root." },
                        "recursive": { "type": "boolean", "description": "Delete recursively including contents. Required true for non-empty directories. Defaults to false." }
                    },
                    "required": ["path"],
                    "additionalProperties": false
                }
            }
        }),
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "run_command",
                "description": "Run a single PowerShell command in the workspace directory. Low-risk commands (cargo check/test, npm test, git status/diff, dir) run directly under Workspace Auto; others need Full Access or user approval. Set background=true for long-running commands (servers, watchers): it returns immediately and the result is reported later.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "command": { "type": "string", "description": "The PowerShell command line to execute." },
                        "timeoutSecs": { "type": "integer", "description": "Optional timeout in seconds, default 120, max 600." },
                        "background": { "type": "boolean", "description": "Run in background and return immediately. Defaults to false." }
                    },
                    "required": ["command"],
                    "additionalProperties": false
                }
            }
        }),
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "get_shell_output",
                "description": "Poll a background shell's accumulated output and status (running/done/killed/error). Use with the shellId returned by run_command background=true.",
                "parameters": { "type": "object", "properties": { "shellId": { "type": "string" } }, "required": ["shellId"], "additionalProperties": false }
            }
        }),
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "kill_shell",
                "description": "Terminate a running background shell by shellId.",
                "parameters": { "type": "object", "properties": { "shellId": { "type": "string" } }, "required": ["shellId"], "additionalProperties": false }
            }
        }),
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "list_shells",
                "description": "List all background shells with their id, command and status.",
                "parameters": { "type": "object", "properties": {}, "additionalProperties": false }
            }
        }),
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "todo_write",
                "description": "Maintain a visible todo list for the current multi-step task. Call this at the start of a complex task to plan steps, and update statuses as you progress (exactly one item in_progress at a time). The list is shown to the user as live progress.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "items": {
                            "type": "array",
                            "description": "The full todo list, replacing the previous one.",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "text": { "type": "string", "description": "Short description of the step." },
                                    "status": { "type": "string", "enum": ["pending", "in_progress", "done"], "description": "Current status of this step." }
                                },
                                "required": ["text", "status"]
                            }
                        }
                    },
                    "required": ["items"],
                    "additionalProperties": false
                }
            }
        }),
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "ask_user",
                "description": "Ask the user 1-4 structured multiple-choice questions when requirements are genuinely ambiguous and guessing would risk doing the wrong work. The UI renders clickable option cards; an 'Other' free-text choice is always added automatically, and questions can allow multiple selections. Prefer this over assuming. Do NOT use it for anything you can determine yourself by reading files or running tools — only for real decisions that are the user's to make.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "questions": {
                            "type": "array",
                            "description": "1 to 4 questions to ask at once.",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "question": { "type": "string", "description": "The full, specific question, ending with a question mark." },
                                    "header": { "type": "string", "description": "Very short label (<= 12 chars) shown as a chip, e.g. 'Library', 'Approach'." },
                                    "multiSelect": { "type": "boolean", "description": "Allow selecting multiple options. Defaults to false." },
                                    "options": {
                                        "type": "array",
                                        "description": "2 to 4 mutually-exclusive choices ('Other' is added automatically; do not add it yourself).",
                                        "items": {
                                            "type": "object",
                                            "properties": {
                                                "label": { "type": "string", "description": "Concise option text (1-5 words)." },
                                                "description": { "type": "string", "description": "What this option means or implies (trade-offs)." }
                                            },
                                            "required": ["label"]
                                        }
                                    }
                                },
                                "required": ["question", "options"]
                            }
                        }
                    },
                    "required": ["questions"],
                    "additionalProperties": false
                }
            }
        }),
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "load_skill",
                "description": "Load the full instructions of a workspace skill (from .novacode/skills/<name>/SKILL.md). Call this when the available-skills list (in system context) has a skill matching the current task.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string", "description": "Skill directory name from the available-skills list." }
                    },
                    "required": ["name"],
                    "additionalProperties": false
                }
            }
        }),
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "web_fetch",
                "description": "Fetch a web page or document by URL and return its readable text content. Use this to read documentation, articles, error references, or any known URL. http/https only.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "url": { "type": "string", "description": "The http/https URL to fetch." }
                    },
                    "required": ["url"],
                    "additionalProperties": false
                }
            }
        }),
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "web_search",
                "description": "Search the web and get a list of result titles, URLs and snippets. Use this when you need to find current information, documentation, or solutions you don't already know. Follow up with web_fetch on the most relevant URLs.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "The search query." }
                    },
                    "required": ["query"],
                    "additionalProperties": false
                }
            }
        }),
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "add_mcp_server",
                "description": "Register and connect an MCP server in NovaCode's real mechanism (not by editing config files). Use this when the user asks you to install/add an MCP server for yourself. command is either a stdio launch command (e.g. 'npx -y @modelcontextprotocol/server-memory') or an http(s):// URL. After it connects, its tools become callable as mcp_<server>_<tool>.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string", "description": "Short server name, e.g. memory, github." },
                        "command": { "type": "string", "description": "stdio launch command or http(s):// URL." },
                        "authToken": { "type": "string", "description": "Optional Bearer token for HTTP servers." }
                    },
                    "required": ["name", "command"],
                    "additionalProperties": false
                }
            }
        }),
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "remember",
                "description": "Persist a concise fact worth remembering across sessions (project convention, a gotcha you hit, a key file path). It is appended to the workspace NovaCode.md and auto-injected in future sessions. Use sparingly for durable, reusable facts — not transient details.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "fact": { "type": "string", "description": "One concise fact to remember (<= 500 chars)." }
                    },
                    "required": ["fact"],
                    "additionalProperties": false
                }
            }
        }),
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "run_subtask",
                "description": "Delegate a focused READ-ONLY exploration subtask (e.g. 'find where X is implemented', 'summarize how module Y works') to a sub-agent with its own fresh context. The sub-agent can only list/read/search files and returns a concise text report. Use this to investigate large codebases without bloating the main conversation. Optionally set agentType to use a custom agent role from .novacode/agents/<type>.md. Set background=true to run it asynchronously: it returns a taskId immediately so you can keep working; poll progress with get_task_output and stop it with kill_task.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "description": { "type": "string", "description": "Clear, self-contained description of what to investigate and what the report should contain." },
                        "agentType": { "type": "string", "description": "Optional custom agent type name (loads .novacode/agents/<type>.md as the sub-agent role)." },
                        "background": { "type": "boolean", "description": "Run asynchronously: return a taskId immediately instead of blocking. Poll with get_task_output, stop with kill_task. Default false." }
                    },
                    "required": ["description"],
                    "additionalProperties": false
                }
            }
        }),
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "get_task_output",
                "description": "Poll a background sub-agent task (started by run_subtask background=true) for its accumulated report and status (running/done/killed/error). Set block=true to wait until it finishes or timeoutSecs elapses.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "taskId": { "type": "string", "description": "The taskId returned by run_subtask background=true." },
                        "block": { "type": "boolean", "description": "Wait for completion (up to timeoutSecs) instead of returning immediately. Default false." },
                        "timeoutSecs": { "type": "integer", "description": "Max seconds to wait when block=true. Default 30, max 120." }
                    },
                    "required": ["taskId"],
                    "additionalProperties": false
                }
            }
        }),
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "kill_task",
                "description": "Stop a running background sub-agent task by taskId.",
                "parameters": { "type": "object", "properties": { "taskId": { "type": "string" } }, "required": ["taskId"], "additionalProperties": false }
            }
        }),
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "list_tasks",
                "description": "List all background sub-agent tasks with their id, description and status.",
                "parameters": { "type": "object", "properties": {}, "additionalProperties": false }
            }
        }),
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "list_mcp_resources",
                "description": "List resources exposed by connected MCP servers (resources/list). Optionally filter by server name. Returns each resource's uri, name and mimeType.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "server": { "type": "string", "description": "Optional MCP server name to filter by; omit to list across all connected servers." }
                    },
                    "additionalProperties": false
                }
            }
        }),
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "read_mcp_resource",
                "description": "Read a resource from a connected MCP server (resources/read), returning its text content.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "server": { "type": "string", "description": "MCP server name that exposes the resource." },
                        "uri": { "type": "string", "description": "The resource URI to read (from list_mcp_resources)." }
                    },
                    "required": ["server", "uri"],
                    "additionalProperties": false
                }
            }
        }),
    ]
}

fn file_tool_schema(name: &str, description: &str, required: &[&str]) -> serde_json::Value {
    let mut properties = serde_json::json!({
        "path": {
            "type": "string",
            "description": "Relative path inside the current workspace. Use . for workspace root."
        }
    });
    if required.contains(&"content") {
        properties["content"] = serde_json::json!({
            "type": "string",
            "description": "UTF-8 text content to write. Use an empty string when the user asks for an empty file."
        });
    }
    if required.contains(&"oldText") {
        properties["oldText"] = serde_json::json!({
            "type": "string",
            "description": "Exact existing text to replace. It should be unique unless replaceAll is true."
        });
    }
    if required.contains(&"newText") {
        properties["newText"] = serde_json::json!({
            "type": "string",
            "description": "Replacement text."
        });
        properties["replaceAll"] = serde_json::json!({
            "type": "boolean",
            "description": "Replace every match. Defaults to false."
        });
    }
    if required.contains(&"query") {
        properties["query"] = serde_json::json!({
            "type": "string",
            "description": "Text or regex to search for. Search is gitignore-aware (skips ignored/hidden files)."
        });
        properties["maxResults"] = serde_json::json!({
            "type": "integer",
            "description": "Maximum number of matches to return, up to 50."
        });
        properties["isRegex"] = serde_json::json!({
            "type": "boolean",
            "description": "Treat query as a regular expression. Defaults to false (literal substring)."
        });
    }

    serde_json::json!({
        "type": "function",
        "function": {
            "name": name,
            "description": description,
            "parameters": {
                "type": "object",
                "properties": properties,
                "required": required,
                "additionalProperties": false
            }
        }
    })
}

fn execute_builtin_tool_call(
    security_context: &SessionSecurityContext,
    tool_name: &str,
    arguments: &str,
) -> Result<serde_json::Value, String> {
    // 权限门控已在工具循环（gate_tool_decision）完成，这里只做工具名校验与真实执行。
    tool_capability_for_name(tool_name)?;
    let workspace_path = security_context.workspace_root.as_str();

    match tool_name {
        "create_file" => execute_create_file_tool_call(workspace_path, arguments),
        "write_file" => {
            let request = serde_json::from_str::<WriteFileRequest>(arguments)
                .map_err(|err| format!("工具参数解析失败: {err}"))?;
            serde_json::to_value(write_file(workspace_path, request).map_err(|err| err.to_string())?)
                .map_err(|err| err.to_string())
        }
        "edit_file" => {
            let request = serde_json::from_str::<EditFileRequest>(arguments)
                .map_err(|err| format!("工具参数解析失败: {err}"))?;
            serde_json::to_value(edit_file(workspace_path, request).map_err(|err| err.to_string())?)
                .map_err(|err| err.to_string())
        }
        "read_file" => {
            let request = serde_json::from_str::<ReadFileRequest>(arguments)
                .map_err(|err| format!("工具参数解析失败: {err}"))?;
            serde_json::to_value(read_file(workspace_path, request).map_err(|err| err.to_string())?)
                .map_err(|err| err.to_string())
        }
        "delete_file" => {
            let request = serde_json::from_str::<DeleteFileRequest>(arguments)
                .map_err(|err| format!("工具参数解析失败: {err}"))?;
            serde_json::to_value(delete_file(workspace_path, request).map_err(|err| err.to_string())?)
                .map_err(|err| err.to_string())
        }
        "list_dir" => {
            let request = serde_json::from_str::<ListDirRequest>(arguments)
                .map_err(|err| format!("工具参数解析失败: {err}"))?;
            serde_json::to_value(list_dir(workspace_path, request).map_err(|err| err.to_string())?)
                .map_err(|err| err.to_string())
        }
        "make_dir" => {
            let request = serde_json::from_str::<MakeDirRequest>(arguments)
                .map_err(|err| format!("工具参数解析失败: {err}"))?;
            serde_json::to_value(make_dir(workspace_path, request).map_err(|err| err.to_string())?)
                .map_err(|err| err.to_string())
        }
        "stat_path" => {
            let request = serde_json::from_str::<StatPathRequest>(arguments)
                .map_err(|err| format!("工具参数解析失败: {err}"))?;
            serde_json::to_value(stat_path(workspace_path, request).map_err(|err| err.to_string())?)
                .map_err(|err| err.to_string())
        }
        "search_text" => {
            let request = serde_json::from_str::<SearchTextRequest>(arguments)
                .map_err(|err| format!("工具参数解析失败: {err}"))?;
            serde_json::to_value(search_text(workspace_path, request).map_err(|err| err.to_string())?)
                .map_err(|err| err.to_string())
        }
        "glob_files" => {
            let request = serde_json::from_str::<GlobFilesRequest>(arguments)
                .map_err(|err| format!("工具参数解析失败: {err}"))?;
            serde_json::to_value(glob_files(workspace_path, request).map_err(|err| err.to_string())?)
                .map_err(|err| err.to_string())
        }
        "move_path" => {
            let request = serde_json::from_str::<MovePathRequest>(arguments)
                .map_err(|err| format!("工具参数解析失败: {err}"))?;
            serde_json::to_value(move_path(workspace_path, request).map_err(|err| err.to_string())?)
                .map_err(|err| err.to_string())
        }
        "delete_dir" => {
            let request = serde_json::from_str::<DeleteDirRequest>(arguments)
                .map_err(|err| format!("工具参数解析失败: {err}"))?;
            serde_json::to_value(delete_dir(workspace_path, request).map_err(|err| err.to_string())?)
                .map_err(|err| err.to_string())
        }
        "run_command" => {
            let request = serde_json::from_str::<RunCommandRequest>(arguments)
                .map_err(|err| format!("工具参数解析失败: {err}"))?;
            serde_json::to_value(run_command(workspace_path, request).map_err(|err| err.to_string())?)
                .map_err(|err| err.to_string())
        }
        other => Err(format!("未知工具: {other}")),
    }
}

fn execute_create_file_tool_call(
    workspace_path: &str,
    arguments: &str,
) -> Result<serde_json::Value, String> {
    let request = serde_json::from_str::<CreateFileRequest>(arguments)
        .map_err(|err| format!("工具参数解析失败: {err}"))?;
    let result = create_file(workspace_path, request).map_err(|err| err.to_string())?;
    serde_json::to_value(result).map_err(|err| err.to_string())
}

fn tool_capability_for_name(tool_name: &str) -> Result<ToolCapability, String> {
    // MCP 外部工具统一按网络能力裁决：Workspace Auto 下逐次审批，Full Access 放行。
    if tool_name.starts_with("mcp_") {
        return Ok(ToolCapability::NetworkAccess);
    }
    match tool_name {
        // 只读或纯 UI / 后台控制类工具：自动放行，不打断用户。remember 仅追加项目记忆文件，低风险。
        "list_dir" | "read_file" | "stat_path" | "search_text" | "glob_files" | "todo_write"
        | "ask_user" | "run_subtask" | "load_skill" | "remember" | "list_shells"
        | "get_shell_output" | "kill_shell" | "get_task_output" | "kill_task" | "list_tasks" => {
            Ok(ToolCapability::FileRead)
        }
        "create_file" | "write_file" | "edit_file" | "make_dir" | "move_path" => {
            Ok(ToolCapability::FileWrite)
        }
        "delete_file" | "delete_dir" => Ok(ToolCapability::FileDelete),
        // 注册 MCP 会拉起外部进程/网络服务，按命令执行级别裁决（FullAccess 或审批）。
        "run_command" | "add_mcp_server" => Ok(ToolCapability::CommandRun),
        "web_fetch" | "web_search" | "list_mcp_resources" | "read_mcp_resource" => {
            Ok(ToolCapability::NetworkAccess)
        }
        other => Err(format!("未知工具: {other}")),
    }
}

fn merge_usage(
    first: Option<novacode_shared::RawUsage>,
    second: Option<novacode_shared::RawUsage>,
) -> Option<novacode_shared::RawUsage> {
    match (first, second) {
        (None, None) => None,
        (Some(usage), None) | (None, Some(usage)) => Some(usage),
        (Some(a), Some(b)) => Some(novacode_shared::RawUsage {
            prompt_tokens: a.prompt_tokens + b.prompt_tokens,
            completion_tokens: a.completion_tokens + b.completion_tokens,
            total_tokens: a.total_tokens + b.total_tokens,
            prompt_cache_hit_tokens: a.prompt_cache_hit_tokens + b.prompt_cache_hit_tokens,
            prompt_cache_miss_tokens: a.prompt_cache_miss_tokens + b.prompt_cache_miss_tokens,
            reasoning_tokens: a.reasoning_tokens + b.reasoning_tokens,
            raw_json: serde_json::json!([a.raw_json, b.raw_json]).to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepends_workspace_context_to_deepseek_messages() {
        let messages = vec![ChatMessage {
            role: "user".to_string(),
            content: "你是否清楚现在所在的工作区路径是什么".to_string(),
        }];

        let injected = messages_with_workspace_context(
            messages,
            Some("C:\\workspace\\demo"),
            Some("NovaCode"),
            None,
            None,
            &[],
        );

        // injected[0] 是 NovaCode 身份锚定消息。
        assert_eq!(injected[0].role, "system");
        assert!(injected[0].content.contains("NovaCode"));
        assert!(injected[0].content.contains(".novacode"));
        assert_eq!(injected[1].role, "system");
        assert!(injected[1].content.contains("C:\\workspace\\demo"));
        assert!(injected[1].content.contains("NovaCode"));
        assert!(injected[1].content.contains("除非用户明确授权越界"));
        assert!(injected[1].content.contains("必须分别调用"));
        assert!(injected[1].content.contains("read_file"));
        assert!(injected[1].content.contains("write_file"));
        assert!(injected[1].content.contains("delete_file"));
        assert!(injected[1].content.contains("list_dir"));
        assert_eq!(injected[2].role, "system");
        assert!(injected[2].content.contains("edit_file"));
        assert!(injected[2].content.contains("search_text"));
        assert_eq!(injected[3].role, "user");
    }

    #[test]
    fn compact_tool_outputs_stubs_old_large_results_keeps_recent() {
        let big = "x".repeat(5_000);
        let small = "{\"ok\":true}".to_string();
        let mut wire = vec![
            serde_json::json!({ "role": "system", "content": "sys" }),
            serde_json::json!({ "role": "user", "content": "do it" }),
            serde_json::json!({ "role": "tool", "tool_call_id": "1", "content": big.clone() }), // old big -> stub
            serde_json::json!({ "role": "tool", "tool_call_id": "2", "content": small.clone() }), // old small -> kept
            serde_json::json!({ "role": "tool", "tool_call_id": "3", "content": big.clone() }), // recent -> kept
            serde_json::json!({ "role": "tool", "tool_call_id": "4", "content": big.clone() }), // recent -> kept
            serde_json::json!({ "role": "tool", "tool_call_id": "5", "content": big.clone() }), // recent -> kept
        ];

        let compacted = compact_tool_outputs(&mut wire, 3, 1_500);

        assert_eq!(compacted, 1, "只压缩 1 条较早的大结果");
        assert_eq!(wire[2]["content"], COMPACTED_TOOL_STUB); // 老的大结果被压缩
        assert_eq!(wire[3]["content"], small); // 老的小结果不动
        assert_eq!(wire[5]["content"], big); // 最近的保留全文
        // 非工具消息不受影响
        assert_eq!(wire[0]["content"], "sys");

        // 幂等：再压一次不应重复处理
        assert_eq!(compact_tool_outputs(&mut wire, 3, 1_500), 0);
    }

    #[test]
    fn hook_matcher_matches_tool_lists() {
        assert!(hook_matcher_matches("*", "write_file"));
        assert!(hook_matcher_matches("write_file|edit_file", "edit_file"));
        assert!(hook_matcher_matches("write_file, run_command", "run_command"));
        assert!(!hook_matcher_matches("write_file|edit_file", "read_file"));
    }

    #[test]
    fn glob_match_handles_wildcards() {
        assert!(glob_match("**/.env", "src/config/.env"));
        assert!(glob_match("src/**", "src/a/b.rs"));
        assert!(glob_match("*.env", ".env"));
        assert!(glob_match("*.env", "prod.env"));
        assert!(!glob_match("*.env", "env.txt"));
        assert!(glob_match("exact.txt", "exact.txt"));
        assert!(!glob_match("exact.txt", "other.txt"));
    }

    #[test]
    fn permission_rules_deny_takes_precedence() {
        let rules = vec![
            "allow:tool:read_file".to_string(),
            "deny:read_file:**/.env".to_string(),
        ];
        // 读普通文件：allow 命中
        assert_eq!(
            permission_rules_decision(&rules, "read_file", "{\"path\":\"src/main.rs\"}"),
            Some(true)
        );
        // 读 .env：deny 命中，优先否决
        assert_eq!(
            permission_rules_decision(&rules, "read_file", "{\"path\":\"src/.env\"}"),
            Some(false)
        );
        // 旧式裸规则向后兼容（视为 allow）
        assert_eq!(
            permission_rules_decision(&["tool:write_file".to_string()], "write_file", "{}"),
            Some(true)
        );
        // 命令前缀规则
        assert_eq!(
            permission_rules_decision(&["cmd:git push".to_string()], "run_command", "{\"command\":\"git push origin\"}"),
            Some(true)
        );
    }

    #[test]
    fn html_to_text_strips_tags_and_scripts() {
        let html = "<html><head><style>.a{color:red}</style></head><body><h1>标题</h1><script>alert(1)</script><p>正文 &amp; 内容</p></body></html>";
        let text = html_to_text(html);
        assert!(text.contains("标题"));
        assert!(text.contains("正文 & 内容"));
        assert!(!text.contains("alert"));
        assert!(!text.contains("color:red"));
        assert!(!text.contains('<'));
    }

    #[test]
    fn percent_decode_restores_url() {
        assert_eq!(percent_decode("https%3A%2F%2Fa.com%2Fx"), "https://a.com/x");
    }

    #[test]
    fn injects_repo_map_when_provided() {
        let messages = vec![ChatMessage {
            role: "user".to_string(),
            content: "项目结构是什么".to_string(),
        }];

        let injected = messages_with_workspace_context(
            messages,
            Some("C:\\workspace\\demo"),
            Some("NovaCode"),
            Some("src/\n  main.rs\nCargo.toml"),
            None,
            &[],
        );

        // sys(身份) + sys(workspace) + sys(tools) + sys(repo map) + user
        assert_eq!(injected.len(), 5);
        assert_eq!(injected[3].role, "system");
        assert!(injected[3].content.contains("工作区结构摘要"));
        assert!(injected[3].content.contains("main.rs"));
        assert_eq!(injected[4].role, "user");
    }

    #[test]
    fn injects_workspace_memory_when_provided() {
        let messages = vec![ChatMessage {
            role: "user".to_string(),
            content: "继续开发".to_string(),
        }];

        let injected = messages_with_workspace_context(
            messages,
            Some("C:\\workspace\\demo"),
            Some("NovaCode"),
            None,
            Some("项目目标：做一个计算器。代码规范：KISS。"),
            &[],
        );

        // sys(身份) + sys(workspace) + sys(tools) + sys(memory) + user
        assert_eq!(injected.len(), 5);
        assert_eq!(injected[3].role, "system");
        assert!(injected[3].content.contains("项目长期记忆"));
        assert!(injected[3].content.contains("做一个计算器"));
    }

    #[test]
    fn summary_split_keeps_systems_and_recent_without_breaking_tool_pairs() {
        // systems(2) + 10 条历史；保留最近 3 条时切点落在 tool 结果上，
        // 应回退到它的 assistant 调用者，保证 tool_calls 与 tool 结果不被拆散。
        let mut wire = vec![
            serde_json::json!({ "role": "system", "content": "ws" }),
            serde_json::json!({ "role": "system", "content": "rules" }),
        ];
        for i in 0..6 {
            wire.push(serde_json::json!({ "role": "user", "content": format!("u{i}") }));
        }
        wire.push(serde_json::json!({ "role": "assistant", "content": "", "tool_calls": [] })); // idx 8 调用者
        wire.push(serde_json::json!({ "role": "tool", "tool_call_id": "1", "content": "r1" })); // idx 9
        wire.push(serde_json::json!({ "role": "tool", "tool_call_id": "2", "content": "r2" })); // idx 10
        wire.push(serde_json::json!({ "role": "assistant", "content": "done" })); // idx 11

        // keep_recent=3 时原始切点是 idx9（tool 结果），应回退到 idx8 的 assistant 调用者。
        let (first_non_system, cut) =
            summary_split_points(&wire, 3).expect("should split");

        assert_eq!(first_non_system, 2);
        assert_eq!(cut, 8, "切分点应回退跳过 tool 结果，落在其 assistant 调用者上");
        assert_ne!(wire[cut]["role"], "tool");

        // 历史太短时不切分
        let short = vec![
            serde_json::json!({ "role": "system", "content": "ws" }),
            serde_json::json!({ "role": "user", "content": "hi" }),
        ];
        assert!(summary_split_points(&short, 4).is_none());
    }

    #[test]
    fn renders_wire_message_with_tool_calls_for_summary() {
        let msg = serde_json::json!({
            "role": "assistant",
            "content": "我来读取文件",
            "tool_calls": [{
                "function": { "name": "read_file", "arguments": "{\"path\":\"a.txt\"}" }
            }]
        });
        let line = render_wire_message_for_summary(&msg);
        assert!(line.starts_with("[assistant]"));
        assert!(line.contains("我来读取文件"));
        assert!(line.contains("read_file"));
    }

    #[test]
    fn executes_create_file_tool_call_inside_workspace() {
        let workspace = std::env::temp_dir().join(format!(
            "novacode-desktop-tool-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time should be available")
                .as_nanos()
        ));
        std::fs::create_dir_all(&workspace).expect("workspace should be created");

        let output = execute_create_file_tool_call(
            workspace.to_str().expect("workspace should be utf8"),
            r#"{"path":"test.txt","content":""}"#,
        )
        .expect("tool call should execute");

        assert_eq!(output["relativePath"], "test.txt");
        assert!(workspace.join("test.txt").is_file());

        let _ = std::fs::remove_dir_all(workspace);
    }

    #[test]
    fn executes_write_read_delete_and_list_tool_calls_inside_workspace() {
        let workspace = std::env::temp_dir().join(format!(
            "novacode-desktop-tool-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time should be available")
                .as_nanos()
        ));
        std::fs::create_dir_all(&workspace).expect("workspace should be created");

        let workspace_path = workspace.to_str().expect("workspace should be utf8");
        let security_context = session_security_context(
            workspace_path.to_string(),
            PermissionMode::WorkspaceAuto,
            NetworkMode::Disabled,
        )
        .expect("security context should build");
        execute_builtin_tool_call(
            &security_context,
            "write_file",
            r#"{"path":"note.txt","content":"123456"}"#,
        )
        .expect("write tool should execute");
        let read_output = execute_builtin_tool_call(
            &security_context,
            "read_file",
            r#"{"path":"note.txt"}"#,
        )
        .expect("read tool should execute");
        let list_output = execute_builtin_tool_call(
            &security_context,
            "list_dir",
            r#"{"path":"."}"#,
        )
        .expect("list tool should execute");
        execute_builtin_tool_call(
            &security_context,
            "edit_file",
            r#"{"path":"note.txt","oldText":"123456","newText":"abcdef"}"#,
        )
        .expect("edit tool should execute");
        execute_builtin_tool_call(
            &security_context,
            "make_dir",
            r#"{"path":"src"}"#,
        )
        .expect("mkdir tool should execute");
        let stat_output = execute_builtin_tool_call(
            &security_context,
            "stat_path",
            r#"{"path":"src"}"#,
        )
        .expect("stat tool should execute");
        let search_output = execute_builtin_tool_call(
            &security_context,
            "search_text",
            r#"{"path":".","query":"abcdef","maxResults":10}"#,
        )
        .expect("search tool should execute");
        execute_builtin_tool_call(
            &security_context,
            "delete_file",
            r#"{"path":"note.txt"}"#,
        )
        .expect("delete tool should execute");

        assert_eq!(read_output["content"], "123456");
        assert_eq!(list_output["entries"][0]["name"], "note.txt");
        assert_eq!(stat_output["kind"], "directory");
        assert_eq!(search_output["matches"][0]["path"], "note.txt");
        assert!(!workspace.join("note.txt").exists());

        let _ = std::fs::remove_dir_all(workspace);
    }

    #[test]
    fn recovers_deepseek_dsml_tool_calls_from_text_content() {
        let content = r#"<｜DSML｜tool_calls><｜DSML｜invoke name="write_file"><｜DSML｜parameter name="path" string="true">\helloworld.txt</｜DSML｜parameter><｜DSML｜parameter name="content" string="true">123456</｜DSML｜parameter></｜DSML｜invoke></｜DSML｜tool_calls>"#;

        let calls = recover_tool_calls_from_content(content);

        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "write_file");
        assert_eq!(calls[0].function.arguments, r#"{"content":"123456","path":"helloworld.txt"}"#);
    }
}

// ── 入口 ──────────────────────────────────────────────────────────────────

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .setup(|app| {
            let data_dir = app.path().app_data_dir()?;
            std::fs::create_dir_all(&data_dir)?;
            let db = init_db(&data_dir.join("novacode.db"))
                .map_err(|e| format!("无法初始化数据库: {e}"))?;
            app.manage(AppState {
                db: Mutex::new(db),
                cancels: Mutex::new(HashMap::new()),
                approvals: Mutex::new(HashMap::new()),
                ask_questions: Mutex::new(HashMap::new()),
                mcp: Mutex::new(HashMap::new()),
                steering: Mutex::new(HashMap::new()),
                repo_maps: Mutex::new(HashMap::new()),
                bg_shells: Mutex::new(HashMap::new()),
                bg_tasks: Mutex::new(HashMap::new()),
                command_sandbox: AtomicBool::new(true),
                task_token_budget: AtomicU64::new(0),
            });

            // 启动时后台连接所有已启用的 MCP server，不阻塞窗口加载。
            {
                let state = app.state::<AppState>();
                let servers = state
                    .db
                    .lock()
                    .ok()
                    .and_then(|db| list_mcp_servers(&db).ok())
                    .unwrap_or_default();
                let handle = app.handle().clone();
                for record in servers.into_iter().filter(|s| s.enabled) {
                    spawn_mcp_connect(&handle, record);
                }
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_deepseek_api_key_status,
            send_message,
            new_conversation,
            new_conversation_with_workspace,
            get_conversations,
            load_messages,
            persist_message,
            rename_conversation,
            remove_conversation,
            pin_conversation,
            archive_conversation,
            get_app_info,
            get_account_balance,
            get_checkpoints,
            revert_to_checkpoint,
            compact_history,
            list_workspace_files,
            get_mcp_servers,
            create_mcp_server,
            toggle_mcp_server,
            delete_mcp_server,
            get_permission_rules,
            create_permission_rule,
            delete_permission_rule,
            get_command_sandbox,
            set_command_sandbox,
            get_task_budget,
            set_task_budget,
            export_conversation,
            export_token_ledger,
            clear_all_conversations,
            list_custom_commands,
            import_file_text,
            get_conversation_events,
            cancel_agent,
            queue_steering,
            respond_approval,
            respond_ask_user,
            get_workspace,
            set_workspace_path,
            clear_workspace,
            check_update,
            install_update,
        ])
        .run(tauri::generate_context!())
        .expect("failed to run NovaCode desktop app");
}
