use novacode_shared::PermissionMode;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum NetworkMode {
    Disabled,
    AllowListed,
    FullAccess,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SandboxPolicy {
    pub workspace_root: String,
    pub network_mode: NetworkMode,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ToolCapability {
    FileList,
    FileRead,
    FileWrite,
    FileDelete,
    CommandRun,
    NetworkAccess,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ToolDecision {
    Allow,
    AskUser,
    Deny,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSecurityContext {
    pub workspace_root: String,
    pub permission_mode: PermissionMode,
    pub network_mode: NetworkMode,
    pub approval_policy: String,
}

#[derive(Debug, Error)]
pub enum SandboxRuntimeError {
    #[error("会话缺少工作区边界")]
    MissingWorkspace,
    #[error("工具能力需要用户审批")]
    ApprovalRequired,
    #[error("当前权限模式不允许此工具能力")]
    CapabilityDenied,
}

/// 判断沙箱策略是否声明了工作区根目录。
///
/// 输入沙箱策略，输出该策略是否拥有最小执行边界；本方法不启动进程。
pub fn has_workspace_boundary(policy: &SandboxPolicy) -> bool {
    !policy.workspace_root.trim().is_empty()
}

/// 构造会话级安全上下文。
///
/// 输入用户在新对话时绑定的工作区路径、权限模式和网络模式；输出 Host 可信的执行边界。
/// 模型只能读取该上下文描述，不能修改它；所有本地工具执行前都必须先经过该上下文裁决。
pub fn session_security_context(
    workspace_root: impl Into<String>,
    permission_mode: PermissionMode,
    network_mode: NetworkMode,
) -> Result<SessionSecurityContext, SandboxRuntimeError> {
    let workspace_root = workspace_root.into();
    if workspace_root.trim().is_empty() {
        return Err(SandboxRuntimeError::MissingWorkspace);
    }

    Ok(SessionSecurityContext {
        workspace_root,
        permission_mode,
        network_mode,
        approval_policy: "host-enforced".to_string(),
    })
}

/// 为单个工具能力做权限裁决。
///
/// 输入 Host 可信安全上下文和工具能力；输出允许、需要审批或拒绝。
/// 该方法不执行工具，只负责把产品权限模式转换成稳定的运行时决策。
pub fn decide_tool_access(
    context: &SessionSecurityContext,
    capability: ToolCapability,
) -> ToolDecision {
    match context.permission_mode {
        PermissionMode::Restricted => match capability {
            ToolCapability::FileList | ToolCapability::FileRead => ToolDecision::Allow,
            _ => ToolDecision::Deny,
        },
        // 只读能力风险低，直接放行，避免每次读文件、列目录都打断用户；
        // 仅对变更类、命令和网络能力逐次审批。修复此前 AskEveryTime 对所有能力（含只读）
        // 都返回 AskUser 导致整模式不可用的问题。
        PermissionMode::AskEveryTime => match capability {
            ToolCapability::FileList | ToolCapability::FileRead => ToolDecision::Allow,
            _ => ToolDecision::AskUser,
        },
        PermissionMode::WorkspaceAuto => match capability {
            ToolCapability::FileList
            | ToolCapability::FileRead
            | ToolCapability::FileWrite
            | ToolCapability::FileDelete => ToolDecision::Allow,
            // 命令与网络默认需审批；命令是否属于低风险白名单由调用方结合命令串进一步判断。
            ToolCapability::CommandRun | ToolCapability::NetworkAccess => ToolDecision::AskUser,
        },
        PermissionMode::FullAccess => ToolDecision::Allow,
    }
}

/// 判断命令是否属于低风险白名单（只读检查、构建、测试、版本查询等）。
///
/// 输入完整命令串；输出是否可在 Workspace Auto 下免审批直接执行。
/// 任何包含管道、串联、重定向或命令替换的命令一律不算低风险，避免白名单前缀被绕过。
pub fn is_low_risk_command(command: &str) -> bool {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return false;
    }
    if trimmed.contains("&&")
        || trimmed.contains("||")
        || trimmed.contains('|')
        || trimmed.contains(';')
        || trimmed.contains('>')
        || trimmed.contains('<')
        || trimmed.contains('`')
        || trimmed.contains('&')
    {
        return false;
    }

    const LOW_RISK_PREFIXES: &[&str] = &[
        "cargo check",
        "cargo build",
        "cargo test",
        "cargo fmt",
        "cargo clippy",
        "cargo --version",
        "npm test",
        "npm run build",
        "npm run test",
        "git status",
        "git diff",
        "git log",
        "git branch",
        "git show",
        "dir",
        "ls",
        "type",
        "echo",
        "pwd",
        "where",
        "node -v",
        "node --version",
        "npm --version",
        "python --version",
        "rustc --version",
    ];

    let lower = trimmed.to_lowercase();
    LOW_RISK_PREFIXES
        .iter()
        .any(|prefix| lower == *prefix || lower.starts_with(&format!("{prefix} ")))
}

/// 校验工具能力是否可以在当前会话中直接执行。
///
/// 输入安全上下文和工具能力；允许时返回 Ok，拒绝或需要审批时返回明确错误。
/// 桌面端当前还没有审批 UI，因此 AskUser 会被转成 ApprovalRequired。
pub fn ensure_tool_allowed(
    context: &SessionSecurityContext,
    capability: ToolCapability,
) -> Result<(), SandboxRuntimeError> {
    match decide_tool_access(context, capability) {
        ToolDecision::Allow => Ok(()),
        ToolDecision::AskUser => Err(SandboxRuntimeError::ApprovalRequired),
        ToolDecision::Deny => Err(SandboxRuntimeError::CapabilityDenied),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_auto_allows_workspace_file_tools() {
        let context = session_security_context(
            "C:\\workspace\\demo",
            PermissionMode::WorkspaceAuto,
            NetworkMode::Disabled,
        )
        .expect("context should build");

        assert_eq!(
            decide_tool_access(&context, ToolCapability::FileWrite),
            ToolDecision::Allow
        );
        assert_eq!(
            decide_tool_access(&context, ToolCapability::FileDelete),
            ToolDecision::Allow
        );
    }

    #[test]
    fn restricted_mode_denies_mutating_file_tools() {
        let context = session_security_context(
            "C:\\workspace\\demo",
            PermissionMode::Restricted,
            NetworkMode::Disabled,
        )
        .expect("context should build");

        assert_eq!(
            decide_tool_access(&context, ToolCapability::FileRead),
            ToolDecision::Allow
        );
        assert_eq!(
            decide_tool_access(&context, ToolCapability::FileWrite),
            ToolDecision::Deny
        );
    }

    #[test]
    fn ask_every_time_allows_read_but_asks_for_mutations() {
        let context = session_security_context(
            "C:\\workspace\\demo",
            PermissionMode::AskEveryTime,
            NetworkMode::Disabled,
        )
        .expect("context should build");

        // 只读能力直接放行，不再误报需要审批。
        assert_eq!(
            decide_tool_access(&context, ToolCapability::FileRead),
            ToolDecision::Allow
        );
        assert_eq!(
            decide_tool_access(&context, ToolCapability::FileList),
            ToolDecision::Allow
        );
        // 变更类与命令仍需逐次审批。
        assert_eq!(
            decide_tool_access(&context, ToolCapability::FileWrite),
            ToolDecision::AskUser
        );
        assert_eq!(
            ensure_tool_allowed(&context, ToolCapability::CommandRun)
                .expect_err("approval should be required")
                .to_string(),
            "工具能力需要用户审批"
        );
    }

    #[test]
    fn low_risk_command_whitelist_matches_safe_commands_only() {
        assert!(is_low_risk_command("cargo test"));
        assert!(is_low_risk_command("cargo check --all"));
        assert!(is_low_risk_command("git status"));
        assert!(is_low_risk_command("npm run build"));
        assert!(is_low_risk_command("dir"));

        // 非白名单命令
        assert!(!is_low_risk_command("rm -rf /"));
        assert!(!is_low_risk_command("git push"));
        // 串联/管道/重定向不算低风险，防止绕过
        assert!(!is_low_risk_command("cargo test && rm x"));
        assert!(!is_low_risk_command("git status | cat"));
        assert!(!is_low_risk_command("echo hi > file"));
        assert!(!is_low_risk_command(""));
    }

    #[test]
    fn rejects_missing_workspace_boundary() {
        let err = session_security_context("", PermissionMode::WorkspaceAuto, NetworkMode::Disabled)
            .expect_err("workspace is required");

        assert_eq!(err.to_string(), "会话缺少工作区边界");
    }
}
