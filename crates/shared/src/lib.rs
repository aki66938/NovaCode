use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum PermissionMode {
    Restricted,
    AskEveryTime,
    WorkspaceAuto,
    FullAccess,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ApiKeyStatus {
    Configured,
    Missing,
    ConnectionFailed,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ActivityEvent {
    pub event_type: String,
    pub summary: String,
}

/// DeepSeek API 返回的原始 token 用量，字段对齐官方 usage 结构。
///
/// 输入来自服务端 JSON，保存时不做任何推算；缺失字段保留为 0，由 usage_source 区分来源。
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct RawUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    pub prompt_cache_hit_tokens: u64,
    pub prompt_cache_miss_tokens: u64,
    pub reasoning_tokens: u64,
    /// 服务端返回的完整 usage JSON，用于审计和后续回放。
    pub raw_json: String,
}

/// 返回权限模式面向用户的中文标签。
///
/// 输入权限模式枚举，输出稳定的中文展示文案；本方法不做权限判断，只负责展示映射。
pub fn permission_mode_label(mode: &PermissionMode) -> &'static str {
    match mode {
        PermissionMode::Restricted => "受限模式",
        PermissionMode::AskEveryTime => "每次询问",
        PermissionMode::WorkspaceAuto => "工作区自动",
        PermissionMode::FullAccess => "完全访问",
    }
}
