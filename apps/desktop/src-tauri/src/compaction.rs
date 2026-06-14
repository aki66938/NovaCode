//! 上下文与输出体积治理：大工具输出落盘、上下文软上限、跨轮摘要压缩、旧工具结果短桩。
//!
//! 从 main.rs 抽出（Plan16 阶段2）：纯逻辑搬移，无行为变更。摘要压缩复用 main.rs 的
//! chat_completion_with_retry（chat 桥接后续再独立成模块）。

use crate::{
    chat_completion_with_retry, COMPACTED_TOOL_STUB, CONTEXT_SOFT_LIMIT_TOKENS,
    KEEP_RECENT_WIRE_MESSAGES,
};
use std::sync::atomic::{AtomicU64, Ordering};
use tauri::AppHandle;

/// 大工具输出落盘自增序号。
static LARGE_OUTPUT_SEQ: AtomicU64 = AtomicU64::new(1);

/// 工具结果过大时落盘到 .novacode/tool-results/<seq>.txt，返回给模型的省流摘要（含相对路径 + 开头预览，
/// 完整内容可用 read_file 分页读取）；未超阈值则原样返回。落盘失败退回原文，绝不丢内容。
pub(crate) fn maybe_persist_large_output(workspace_path: &str, output_str: &str) -> String {
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

/// 读取上下文压缩软上限：优先取环境变量 NOVACODE_CONTEXT_SOFT_LIMIT（便于低阈值压测），否则用默认值。
pub(crate) fn context_soft_limit_tokens() -> u64 {
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
fn summary_split_points(wire: &[serde_json::Value], keep_recent: usize) -> Option<(usize, usize)> {
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
pub(crate) async fn summarize_wire_history(
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
pub(crate) fn compact_tool_outputs(
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

#[cfg(test)]
mod tests {
    use super::{compact_tool_outputs, render_wire_message_for_summary, summary_split_points};
    use crate::COMPACTED_TOOL_STUB;

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
}
