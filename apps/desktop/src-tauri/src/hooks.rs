//! 生命周期钩子与诊断命令：从工作区 .novacode/hooks.json 读取用户定义的 PreToolUse / PostToolUse
//! 钩子，以及 .novacode/diagnostics 的收尾诊断命令。
//!
//! 从 main.rs 抽出（Plan16 阶段2）：纯函数搬移，无行为变更。
//! PreToolUse 在工具执行前运行，退出码非 0 则阻断该工具（stdout/stderr 作为原因回灌模型）；
//! PostToolUse 在工具成功后运行（信息性）。钩子命令经 PowerShell 执行，工具的
//! { tool, arguments } 以 JSON 从 stdin 传入。

use tauri::{AppHandle, Emitter};

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
pub(crate) fn read_diagnostics_command(workspace: &str) -> Option<String> {
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
pub(crate) fn run_pre_tool_hooks(workspace: &str, tool_name: &str, arguments: &str) -> Option<String> {
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

#[cfg(test)]
mod tests {
    use super::hook_matcher_matches;

    #[test]
    fn hook_matcher_matches_tool_lists() {
        assert!(hook_matcher_matches("*", "write_file"));
        assert!(hook_matcher_matches("write_file|edit_file", "edit_file"));
        assert!(hook_matcher_matches("write_file, run_command", "run_command"));
        assert!(!hook_matcher_matches("write_file|edit_file", "read_file"));
    }
}

/// 运行匹配的 PostToolUse 钩子（信息性，不阻断），把输出作为 activity 事件展示。
pub(crate) fn run_post_tool_hooks(app: &AppHandle, workspace: &str, tool_name: &str, arguments: &str) {
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
