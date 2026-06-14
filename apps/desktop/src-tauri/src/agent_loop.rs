//! Agent 主流程：send_message 命令、内置工具循环 chat_with_builtin_tools、
//! 工作区上下文注入 messages_with_workspace_context 与项目长期记忆读取。
//! 依赖 chat / tools / subagent / permissions / checkpoint / compaction / mcp 等全部下游模块。
//!
//! 从 main.rs 抽出（Plan16，最后一步）：纯代码搬移与可见性调整，无行为变更。

use crate::chat::{
    assistant_message_for_tool_calls, chat_messages_to_wire, recover_tool_calls_from_content,
    stream_round_with_retry,
};
use crate::checkpoint::{capture_checkpoint_before, persist_checkpoint, post_execution_diff};
use crate::command_run::{execute_bg_shell_tool, execute_run_command_tool};
use crate::compaction::{
    compact_tool_outputs, context_soft_limit_tokens, maybe_persist_large_output,
    summarize_wire_history,
};
use crate::hooks::{read_diagnostics_command, run_post_tool_hooks, run_pre_tool_hooks};
use crate::mcp::{
    collect_mcp_bindings, execute_add_mcp_server, execute_mcp_resource_tool, execute_mcp_tool,
    McpBinding,
};
use crate::permissions::{
    execute_ask_user, feed_tool_denial, gate_tool_decision, request_tool_approval, ToolGate,
};
use crate::state::AppState;
use crate::subagent::{execute_bg_task_tool, execute_run_subtask};
use crate::tools::{
    all_builtin_tool_schemas, execute_builtin_tool_call, execute_load_skill, execute_readonly_call,
    execute_remember, execute_todo_write, load_workspace_skills, PARALLEL_READONLY_TOOLS,
};
use crate::web::{execute_web_fetch, execute_web_search};
use crate::{commands::permission_mode_from_str, merge_usage, record_tool_event};
use novacode_deepseek_client::{chat_stream, resolve_base_url, ChatMessage};
use novacode_sandbox_runtime::{session_security_context, NetworkMode};
use novacode_shared::PermissionMode;
use novacode_storage::{get_conversation, get_model_provider, list_permission_rules};
use novacode_token_accounting::{compute_cost_summary, deepseek_pricing_for_model};
use novacode_tool_runtime::RunCommandRequest;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tauri::{AppHandle, Emitter, Manager, State};

/// 压缩时保留最近 N 次工具结果全文，更早的大体积结果替换为短桩。
const KEEP_RECENT_TOOL_RESULTS: usize = 3;
/// 仅压缩正文超过该字符数的旧工具结果；小结果不动，避免无谓信息损失。
const TOOL_RESULT_STUB_THRESHOLD: usize = 1_500;

// ── DeepSeek ──────────────────────────────────────────────────────────────

/// 发起流式聊天请求。
///
/// 通过 "chat-chunk" 事件逐块推送内容；流结束后发送 "chat-usage" 事件；
/// 最后发送 "chat-done"。错误时返回字符串供前端展示。
#[tauri::command]
pub(crate) async fn send_message(
    app: AppHandle,
    state: State<'_, AppState>,
    conversation_id: String,
    messages: Vec<ChatMessage>,
    model: String,
    permission_mode: String,
    plan_mode: Option<bool>,
) -> Result<(), String> {
    let plan_mode = plan_mode.unwrap_or(false);
    let (conversation, permission_rules, base_url, api_key) = {
        let db = state.db.lock().map_err(|e| e.to_string())?;
        let conversation = get_conversation(&db, &conversation_id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "会话不存在".to_string())?;
        let rules = list_permission_rules(&db).unwrap_or_default();
        // 主模型 provider（Plan17 D3）：base_url/api_key 一律从 DB 取；base_url 为空时解析 preset 官方端点。
        // DB 无主 provider 即报错引导去设置页，不再回退环境变量。
        let (base_url, api_key) = match get_model_provider(&db, "main") {
            Ok(Some(p)) => {
                let bu = resolve_base_url(p.base_url.as_deref(), p.preset.as_deref())
                    .ok_or_else(|| "未配置主模型：请在 设置 → 模型供应商 配置".to_string())?;
                (bu, p.api_key)
            }
            _ => return Err("未配置主模型：请在 设置 → 模型供应商 配置".to_string()),
        };
        (conversation, rules, base_url, api_key)
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
        chat_stream(&base_url, &api_key, messages, &model, |chunk| {
            let _ = app.emit("chat-chunk", chunk);
        })
        .await
        .map_err(|e| e.to_string())
    } else if let Some(workspace_path) = conversation.workspace_path.as_deref() {
        let mcp_bindings = collect_mcp_bindings(&app);
        chat_with_builtin_tools(
            &base_url,
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
        chat_stream(&base_url, &api_key, messages, &model, |chunk| {
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

/// Agent 工具循环：每轮带工具问模型、执行返回的工具、把结果回灌，直到模型不再调用工具
/// （自然终止）或用户中断。不设轮数上限——上下文自动压缩兜底体积，取消按钮兜底失控；
/// 所有工具执行前都经 SessionSecurityContext 裁决。
#[allow(clippy::too_many_arguments)]
async fn chat_with_builtin_tools(
    base_url: &str,
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
                    summarize_wire_history(base_url, api_key, model, std::mem::take(&mut wire_messages), app)
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
            stream_round_with_retry(base_url, api_key, wire_messages.clone(), model, tool_schemas.clone(), app)
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
                        base_url,
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
    fn executes_create_file_tool_call_inside_workspace() {
        let workspace = std::env::temp_dir().join(format!(
            "novacode-desktop-tool-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time should be available")
                .as_nanos()
        ));
        std::fs::create_dir_all(&workspace).expect("workspace should be created");

        let output = crate::tools::execute_create_file_tool_call(
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
