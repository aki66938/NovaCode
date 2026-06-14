//! 文件变更检查点与 diff：写类工具执行前后的快照捕获、行级 diff 计算、检查点落库与回退。
//!
//! 从 main.rs 抽出（Plan16）：纯代码搬移，无行为变更。

use crate::state::AppState;
use novacode_storage::{record_file_checkpoint, FileCheckpoint};
use tauri::{AppHandle, Manager};

/// 写类工具执行前捕获的回退数据快照。
pub(crate) struct CheckpointCapture {
    pub(crate) rel_path: String,
    pub(crate) prev_content: Option<String>,
    pub(crate) extra_json: Option<String>,
    pub(crate) revertible: bool,
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
pub(crate) fn capture_checkpoint_before(
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
pub(crate) fn post_execution_diff(
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
pub(crate) fn persist_checkpoint(
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
pub(crate) fn apply_checkpoint_revert(
    workspace: &str,
    checkpoint: &FileCheckpoint,
) -> Result<(), String> {
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
