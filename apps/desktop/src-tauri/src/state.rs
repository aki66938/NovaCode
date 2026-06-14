//! 应用共享状态与后台句柄结构。
//!
//! 从 main.rs 抽出（Plan16 阶段1）：纯结构/静态量搬移，无行为变更。

use novacode_mcp_client::McpClient;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;

/// 审批请求自增序号，用于生成唯一 action_id。
pub(crate) static APPROVAL_SEQ: AtomicU64 = AtomicU64::new(1);
/// ask_user 结构化提问自增序号，生成唯一 question_id。
pub(crate) static QUESTION_SEQ: AtomicU64 = AtomicU64::new(1);
/// 后台 shell 自增序号，生成唯一 shell_id。
pub(crate) static BG_SHELL_SEQ: AtomicU64 = AtomicU64::new(1);
/// 后台子代理任务自增序号，生成唯一 task_id。
pub(crate) static BG_TASK_SEQ: AtomicU64 = AtomicU64::new(1);

/// Host 可信的应用状态：数据库、运行时句柄、各类按会话/任务索引的共享表。
pub(crate) struct AppState {
    pub(crate) db: Mutex<rusqlite::Connection>,
    /// 正在运行的 Agent 会话取消标志，按 conversation_id 索引。用户点击停止时置 true，
    /// 工具循环在轮次之间和工具执行前检查并安全收尾。
    pub(crate) cancels: Mutex<HashMap<String, Arc<AtomicBool>>>,
    /// 等待用户审批的高风险动作，按 action_id 索引，附带该动作对应的「总是允许」规则串。
    /// respond_approval 命令收到前端决定后，通过 oneshot 通道唤醒正在 await 的工具循环；
    /// 用户勾选记住时把规则写入 permission_rules 表。
    pub(crate) approvals: Mutex<HashMap<String, (oneshot::Sender<bool>, String)>>,
    /// 等待用户回答的 ask_user 结构化提问，按 question_id 索引。respond_ask_user 命令收到
    /// 前端选择后，通过 oneshot 通道把答案 JSON 回送给正在 await 的工具循环。
    pub(crate) ask_questions: Mutex<HashMap<String, oneshot::Sender<String>>>,
    /// 已连接的 MCP server 客户端，按配置 id 索引。Arc 包裹以便在锁外调用。
    pub(crate) mcp: Mutex<HashMap<String, Arc<McpClient>>>,
    /// Steering：用户在 Agent 运行中排队的插话消息，按 conversation_id 索引。
    /// 工具循环在每轮开始时取出并作为 user 消息注入，实现「运行中纠偏」。
    pub(crate) steering: Mutex<HashMap<String, Vec<String>>>,
    /// repo map 按会话缓存：避免每轮重新遍历工作区，并让 system 前缀字节稳定，
    /// 最大化 DeepSeek prompt 缓存命中（缓存友好上下文）。
    pub(crate) repo_maps: Mutex<HashMap<String, String>>,
    /// 托管后台 shell：background=true 启动的命令，按 shell_id 索引，可轮询输出 / 杀进程。
    pub(crate) bg_shells: Mutex<HashMap<String, BgShell>>,
    /// 后台子代理任务：run_subtask background=true 启动的探索代理，按 task_id 索引，
    /// 可用 get_task_output 轮询报告/状态、kill_task 终止；完成时 usage 由首次 get_task_output 结算进账本。
    pub(crate) bg_tasks: Mutex<HashMap<String, BgTask>>,
    /// 命令沙箱开关（默认开）：前台 run_command 是否在受限令牌沙箱中执行。
    pub(crate) command_sandbox: AtomicBool,
    /// 单次任务 token 预算（累计 total_tokens 上限）；0 = 不限。超出则暂停工具循环。
    pub(crate) task_token_budget: AtomicU64,
}

/// 一个托管的后台 shell 进程状态。
#[derive(Clone)]
pub(crate) struct BgShell {
    pub(crate) command: String,
    pub(crate) output: Arc<Mutex<String>>,
    pub(crate) status: Arc<Mutex<String>>, // running | done | killed | error
    pub(crate) cancel: Arc<AtomicBool>,
}

/// 一个后台子代理任务的共享状态（仿 BgShell）。
#[derive(Clone)]
pub(crate) struct BgTask {
    pub(crate) description: String,
    pub(crate) report: Arc<Mutex<String>>,
    pub(crate) status: Arc<Mutex<String>>, // running | done | killed | error
    pub(crate) usage: Arc<Mutex<Option<novacode_shared::RawUsage>>>,
    /// usage 是否已被某次 get_task_output 结算进会话账本，避免重复计费。
    pub(crate) settled: Arc<AtomicBool>,
    pub(crate) cancel: Arc<AtomicBool>,
}
