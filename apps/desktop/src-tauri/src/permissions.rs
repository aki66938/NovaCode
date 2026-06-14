//! 权限裁决与用户交互：能力分级、规则匹配（allow/deny + glob）、门控决策、审批弹窗、
//! ask_user 结构化提问、拒绝回灌。
//!
//! 从 main.rs 抽出（Plan16 阶段2）：纯逻辑搬移，无行为变更。feed_tool_denial 复用
//! main.rs 的 record_tool_event（活动事件落库后续再独立）。

use crate::record_tool_event;
use crate::state::{AppState, APPROVAL_SEQ, QUESTION_SEQ};
use novacode_sandbox_runtime::{
    decide_tool_access, is_low_risk_command, SessionSecurityContext, ToolCapability, ToolDecision,
};
use novacode_tool_runtime::RunCommandRequest;
use std::sync::atomic::Ordering;
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::oneshot;

/// 单次工具调用的权限门控结果。
pub(crate) enum ToolGate {
    /// 直接放行执行。
    Allow,
    /// 需要用户逐次审批。
    Ask,
    /// 当前权限模式直接拒绝，附带原因。
    Deny(String),
}

/// 把工具名映射到能力等级（只读 / 写 / 删 / 命令 / 网络）；未知工具报错。
pub(crate) fn tool_capability_for_name(tool_name: &str) -> Result<ToolCapability, String> {
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
pub(crate) fn gate_tool_decision(
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
pub(crate) async fn request_tool_approval(app: &AppHandle, tool_name: &str, arguments: &str) -> bool {
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
pub(crate) async fn execute_ask_user(
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
pub(crate) fn feed_tool_denial(
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

#[cfg(test)]
mod tests {
    use super::{glob_match, permission_rules_decision};

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
}
