//! 命令执行工具：run_command（前台流式 / 后台 shell）与后台 shell 控制工具
//! （list_shells / get_shell_output / kill_shell）。
//!
//! 从 main.rs 抽出（Plan16 阶段2）：纯函数搬移，无行为变更。

use crate::state::{AppState, BgShell, BG_SHELL_SEQ};
use novacode_sandbox_runtime::SessionSecurityContext;
use novacode_tool_runtime::RunCommandRequest;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter, Manager};

/// 构造把命令输出逐行推送到前端的回调（"command-output" 事件）。
pub(crate) fn command_line_callback(app: &AppHandle) -> novacode_tool_runtime::CommandLineCallback {
    let app = app.clone();
    std::sync::Arc::new(move |line: String| {
        let _ = app.emit("command-output", line);
    })
}

/// run_command 工具的桌面端执行：前台流式输出；background=true 时立即返回、
/// 后台线程跑完后通过 "background-command-done" 事件通知。
pub(crate) fn execute_run_command_tool(
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
pub(crate) fn execute_bg_shell_tool(
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
