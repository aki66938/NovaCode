use novacode_storage::{init_db, list_mcp_servers, record_activity_event};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::Mutex;
use tauri::{AppHandle, Emitter, Manager};

/// 触发上下文压缩的软上限默认值（以上一次响应返回的 prompt_tokens 为准）。
/// DeepSeek V4 Flash / Pro 官方标称 1M 上下文，故取 800K：在接近上限前压缩，
/// 留约 200K headroom 给模型输出与当轮工具结果，避免顶满 1M 触发服务端退化。
/// 可用环境变量 NOVACODE_CONTEXT_SOFT_LIMIT 覆盖（便于低阈值压测验证压缩机制）。
pub(crate) const CONTEXT_SOFT_LIMIT_TOKENS: u64 = 800_000;
/// 摘要压缩时保留最近 N 条 wire 消息原文，更早的历史压缩成任务进度摘要。
pub(crate) const KEEP_RECENT_WIRE_MESSAGES: usize = 8;
/// 工具结果被压缩后替换成的短桩内容。
pub(crate) const COMPACTED_TOOL_STUB: &str =
    "{\"ok\":true,\"note\":\"[此前的工具结果已省略以节省上下文；如需该文件/目录/命令的最新内容，请重新调用对应工具读取]\"}";

mod state;
use state::AppState;

mod commands;
use commands::{
    archive_conversation, cancel_agent, check_update, clear_all_conversations, clear_workspace,
    compact_history, create_mcp_server, create_permission_rule, delete_mcp_server,
    delete_permission_rule, export_conversation, export_token_ledger, get_account_balance,
    get_app_info, get_checkpoints, get_command_sandbox, get_conversation_events, get_conversations,
    get_deepseek_api_key_status, get_mcp_servers, get_permission_rules, get_task_budget,
    get_workspace, import_file_text, install_update, list_custom_commands, list_workspace_files,
    load_messages, new_conversation, new_conversation_with_workspace, persist_message,
    pin_conversation, queue_steering, remove_conversation, rename_conversation, respond_approval,
    respond_ask_user, revert_to_checkpoint, set_command_sandbox, set_task_budget, set_workspace_path,
    toggle_mcp_server,
};

mod agent_loop;
use agent_loop::send_message;

mod mcp;
use mcp::spawn_mcp_connect;

/// 记录一条工具 Activity Event，并同时推送给前端用于过程展示。
///
/// 输入会话 ID、工具名、状态、输入参数、可选输出/错误和工作区快照；
/// 写入失败不阻塞主流程，只忽略错误，保证一次工具失败不会拖垮整条对话。
#[allow(clippy::too_many_arguments)]
pub(crate) fn record_tool_event(
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

mod permissions;
mod checkpoint;
mod hooks;
mod web;
mod tools;
mod command_run;
mod subagent;
mod compaction;
mod chat;
// 已完成模块 compaction.rs 通过 `crate::chat_completion_with_retry` 引用，保留 crate 根再导出。
pub(crate) use chat::chat_completion_with_retry;

pub(crate) fn merge_usage(
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
