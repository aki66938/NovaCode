use rusqlite::{params, Connection, Result as SqlResult};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

// ── 公共类型 ─────────────────────────────────────────────────────────────

/// 会话记录，对应 conversations 表一行。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Conversation {
    pub id: String,
    pub title: String,
    pub workspace_path: Option<String>,
    pub workspace_name: Option<String>,
    pub mode: String,
    /// 置顶：列表排序时优先于普通会话。
    pub pinned: bool,
    /// 归档：从主列表移入「已归档」区，不删除数据。
    pub archived: bool,
    pub created_at: i64,
    pub updated_at: i64,
}

/// 消息记录，对应 messages 表一行。
///
/// usage_json 保存序列化后的 CostSummary JSON，为 None 表示本条消息无 token 统计。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredMessage {
    pub id: String,
    pub conversation_id: String,
    pub role: String,
    pub content: String,
    pub usage_json: Option<String>,
    /// 序列化后的消息 parts（文字块 + 工具卡片交错），用于重启后还原内联工具执行记录。
    /// 为 None 表示旧数据或纯文字消息，前端回退为单个 text part。content 仍保留纯文字供模型上下文。
    pub parts_json: Option<String>,
    pub created_at: i64,
}

/// 工作区记录，对应用户授权给 Agent 的本地目录。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Workspace {
    pub id: String,
    pub name: String,
    pub path: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub active: bool,
}

/// Activity Event 记录，对应一次工具调用的请求、裁决和执行结果，用于审计与前端过程展示。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivityEventRecord {
    pub id: String,
    pub conversation_id: String,
    pub event_type: String,
    pub tool_name: Option<String>,
    pub status: String,
    pub input_json: Option<String>,
    pub output_json: Option<String>,
    pub error_message: Option<String>,
    pub workspace_path: Option<String>,
    pub created_at: i64,
}

/// 文件变更检查点：每次写类工具执行前的文件快照，支撑回退（rewind）。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileCheckpoint {
    pub id: String,
    pub conversation_id: String,
    /// 会话内单调递增序号，回退按序号倒序执行。
    pub seq: i64,
    pub tool_name: String,
    pub rel_path: String,
    /// 变更前的文件全文；None 表示此前文件不存在（回退 = 删除）。
    pub prev_content: Option<String>,
    /// 工具相关附加信息（如 move_path 的 from/to）。
    pub extra_json: Option<String>,
    /// 是否可回退（delete_dir 递归删除等不可回退）。
    pub revertible: bool,
    /// 是否已被回退。
    pub reverted: bool,
    pub created_at: i64,
}

// ── 初始化 ────────────────────────────────────────────────────────────────

/// 打开或创建 SQLite 数据库，执行 schema 迁移。
///
/// 输入数据库文件路径；首次调用自动建表，后续调用幂等；
/// 返回已就绪的 Connection，错误时返回 rusqlite::Error。
pub fn init_db(path: &Path) -> SqlResult<Connection> {
    let conn = Connection::open(path)?;
    conn.execute_batch(
        "
        PRAGMA journal_mode = WAL;
        PRAGMA foreign_keys = ON;

        CREATE TABLE IF NOT EXISTS conversations (
            id         TEXT PRIMARY KEY,
            title      TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS messages (
            id              TEXT PRIMARY KEY,
            conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
            role            TEXT NOT NULL,
            content         TEXT NOT NULL,
            usage_json      TEXT,
            created_at      INTEGER NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_messages_conv
            ON messages (conversation_id, created_at);

        CREATE TABLE IF NOT EXISTS workspaces (
            id         TEXT PRIMARY KEY,
            name       TEXT NOT NULL,
            path       TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL,
            active     INTEGER NOT NULL DEFAULT 0
        );

        CREATE INDEX IF NOT EXISTS idx_workspaces_active
            ON workspaces (active, updated_at);

        CREATE TABLE IF NOT EXISTS activity_events (
            id              TEXT PRIMARY KEY,
            conversation_id TEXT NOT NULL,
            event_type      TEXT NOT NULL,
            tool_name       TEXT,
            status          TEXT NOT NULL,
            input_json      TEXT,
            output_json     TEXT,
            error_message   TEXT,
            workspace_path  TEXT,
            created_at      INTEGER NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_activity_conv
            ON activity_events (conversation_id, created_at);

        CREATE TABLE IF NOT EXISTS file_checkpoints (
            id              TEXT PRIMARY KEY,
            conversation_id TEXT NOT NULL,
            seq             INTEGER NOT NULL,
            tool_name       TEXT NOT NULL,
            rel_path        TEXT NOT NULL,
            prev_content    TEXT,
            extra_json      TEXT,
            revertible      INTEGER NOT NULL DEFAULT 1,
            reverted        INTEGER NOT NULL DEFAULT 0,
            created_at      INTEGER NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_checkpoints_conv
            ON file_checkpoints (conversation_id, seq);

        CREATE TABLE IF NOT EXISTS permission_rules (
            id         TEXT PRIMARY KEY,
            rule       TEXT NOT NULL UNIQUE,
            created_at INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS mcp_servers (
            id         TEXT PRIMARY KEY,
            name       TEXT NOT NULL,
            command    TEXT NOT NULL,
            enabled    INTEGER NOT NULL DEFAULT 1,
            created_at INTEGER NOT NULL
        );
        ",
    )?;
    add_column_if_missing(&conn, "conversations", "workspace_path", "TEXT")?;
    add_column_if_missing(&conn, "conversations", "workspace_name", "TEXT")?;
    add_column_if_missing(&conn, "conversations", "mode", "TEXT NOT NULL DEFAULT 'chat_only'")?;
    add_column_if_missing(&conn, "messages", "parts_json", "TEXT")?;
    add_column_if_missing(&conn, "conversations", "pinned", "INTEGER NOT NULL DEFAULT 0")?;
    add_column_if_missing(&conn, "conversations", "archived", "INTEGER NOT NULL DEFAULT 0")?;
    add_column_if_missing(&conn, "mcp_servers", "auth_token", "TEXT")?;
    Ok(conn)
}

// ── 工具 ──────────────────────────────────────────────────────────────────

/// 返回当前 Unix 时间戳（秒）。
pub fn now_ts() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn add_column_if_missing(
    conn: &Connection,
    table: &str,
    column: &str,
    definition: &str,
) -> SqlResult<()> {
    // SQLite 不支持 ADD COLUMN IF NOT EXISTS，需先检查 schema，避免重复迁移失败。
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let columns = stmt.query_map([], |row| row.get::<_, String>(1))?;
    for existing in columns {
        if existing? == column {
            return Ok(());
        }
    }

    conn.execute(&format!("ALTER TABLE {table} ADD COLUMN {column} {definition}"), [])?;
    Ok(())
}

// ── Conversation CRUD ─────────────────────────────────────────────────────

/// 创建新会话，初始标题为"新对话"。
///
/// 输入数据库连接；插入一条 conversations 记录并返回完整结构体。
pub fn create_conversation(conn: &Connection) -> SqlResult<Conversation> {
    create_conversation_with_workspace(conn, None, None)
}

/// 创建新会话，并可绑定创建时选择的工作区快照。
///
/// 输入数据库连接、可选工作区路径和名称；输出完整 Conversation。路径存在时 mode 为
/// `local_workspace`，否则为 `chat_only`，用于后续权限和 Agent cwd 判定。
pub fn create_conversation_with_workspace(
    conn: &Connection,
    workspace_path: Option<&str>,
    workspace_name: Option<&str>,
) -> SqlResult<Conversation> {
    let id = Uuid::new_v4().to_string();
    let now = now_ts();
    let mode = if workspace_path.is_some() {
        "local_workspace"
    } else {
        "chat_only"
    };
    conn.execute(
        "INSERT INTO conversations
         (id, title, workspace_path, workspace_name, mode, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
        params![id, "新对话", workspace_path, workspace_name, mode, now],
    )?;
    Ok(Conversation {
        id,
        title: "新对话".to_string(),
        workspace_path: workspace_path.map(str::to_string),
        workspace_name: workspace_name.map(str::to_string),
        mode: mode.to_string(),
        pinned: false,
        archived: false,
        created_at: now,
        updated_at: now,
    })
}

/// 查询所有会话：置顶在前，其余按最近更新时间倒序；归档的也返回，由前端分区展示。
pub fn list_conversations(conn: &Connection) -> SqlResult<Vec<Conversation>> {
    let mut stmt = conn.prepare(
        "SELECT id, title, workspace_path, workspace_name, mode, pinned, archived, created_at, updated_at
         FROM conversations
         ORDER BY pinned DESC, updated_at DESC",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(Conversation {
            id: row.get(0)?,
            title: row.get(1)?,
            workspace_path: row.get(2)?,
            workspace_name: row.get(3)?,
            mode: row.get(4)?,
            pinned: row.get::<_, i64>(5)? != 0,
            archived: row.get::<_, i64>(6)? != 0,
            created_at: row.get(7)?,
            updated_at: row.get(8)?,
        })
    })?;
    rows.collect()
}

/// 设置会话置顶状态。
pub fn set_conversation_pinned(conn: &Connection, conv_id: &str, pinned: bool) -> SqlResult<()> {
    conn.execute(
        "UPDATE conversations SET pinned = ?1 WHERE id = ?2",
        params![pinned as i64, conv_id],
    )?;
    Ok(())
}

/// 设置会话归档状态。归档不删除数据，只影响前端分区展示。
pub fn set_conversation_archived(conn: &Connection, conv_id: &str, archived: bool) -> SqlResult<()> {
    conn.execute(
        "UPDATE conversations SET archived = ?1 WHERE id = ?2",
        params![archived as i64, conv_id],
    )?;
    Ok(())
}

/// 按 ID 查询单个会话。
///
/// 输入数据库连接和会话 ID；输出完整 Conversation 或 None，供发送链路读取 session 级工作区快照。
pub fn get_conversation(conn: &Connection, conv_id: &str) -> SqlResult<Option<Conversation>> {
    let mut stmt = conn.prepare(
        "SELECT id, title, workspace_path, workspace_name, mode, pinned, archived, created_at, updated_at
         FROM conversations
         WHERE id = ?1
         LIMIT 1",
    )?;
    let mut rows = stmt.query([conv_id])?;

    if let Some(row) = rows.next()? {
        Ok(Some(Conversation {
            id: row.get(0)?,
            title: row.get(1)?,
            workspace_path: row.get(2)?,
            workspace_name: row.get(3)?,
            mode: row.get(4)?,
            pinned: row.get::<_, i64>(5)? != 0,
            archived: row.get::<_, i64>(6)? != 0,
            created_at: row.get(7)?,
            updated_at: row.get(8)?,
        }))
    } else {
        Ok(None)
    }
}

/// 更新会话标题，同时刷新 updated_at。
pub fn update_title(conn: &Connection, conv_id: &str, title: &str) -> SqlResult<()> {
    conn.execute(
        "UPDATE conversations SET title = ?1, updated_at = ?2 WHERE id = ?3",
        params![title, now_ts(), conv_id],
    )?;
    Ok(())
}

/// 删除会话及其所有消息（ON DELETE CASCADE）。
pub fn delete_conversation(conn: &Connection, conv_id: &str) -> SqlResult<()> {
    conn.execute("DELETE FROM conversations WHERE id = ?1", params![conv_id])?;
    Ok(())
}

/// 清除全部会话（及级联消息），用于数据治理「清除所有会话」。
pub fn delete_all_conversations(conn: &Connection) -> SqlResult<()> {
    conn.execute("DELETE FROM conversations", [])?;
    Ok(())
}

// ── Message CRUD ──────────────────────────────────────────────────────────

/// 保存一条消息，并刷新所属会话的 updated_at。
///
/// 输入会话 ID、角色（user / assistant）、消息内容和可选的 usage JSON；
/// 写入成功后同步更新会话时间戳，使会话列表排序保持最新。
pub fn save_message(
    conn: &Connection,
    conv_id: &str,
    role: &str,
    content: &str,
    usage_json: Option<&str>,
    parts_json: Option<&str>,
) -> SqlResult<()> {
    let id = Uuid::new_v4().to_string();
    let now = now_ts();
    conn.execute(
        "INSERT INTO messages (id, conversation_id, role, content, usage_json, parts_json, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![id, conv_id, role, content, usage_json, parts_json, now],
    )?;
    conn.execute(
        "UPDATE conversations SET updated_at = ?1 WHERE id = ?2",
        params![now, conv_id],
    )?;
    Ok(())
}

/// 查询会话的所有消息，按时间正序排列。
pub fn get_messages(conn: &Connection, conv_id: &str) -> SqlResult<Vec<StoredMessage>> {
    let mut stmt = conn.prepare(
        "SELECT id, conversation_id, role, content, usage_json, parts_json, created_at
         FROM messages
         WHERE conversation_id = ?1
         ORDER BY created_at ASC",
    )?;
    let rows = stmt.query_map([conv_id], |row| {
        Ok(StoredMessage {
            id: row.get(0)?,
            conversation_id: row.get(1)?,
            role: row.get(2)?,
            content: row.get(3)?,
            usage_json: row.get(4)?,
            parts_json: row.get(5)?,
            created_at: row.get(6)?,
        })
    })?;
    rows.collect()
}

/// 删除会话的全部消息（/compact 手动压缩时由摘要替换原文前调用）。
pub fn delete_messages(conn: &Connection, conv_id: &str) -> SqlResult<()> {
    conn.execute(
        "DELETE FROM messages WHERE conversation_id = ?1",
        params![conv_id],
    )?;
    Ok(())
}

// ── File Checkpoint CRUD ─────────────────────────────────────────────────

/// 记录一次文件变更检查点（写类工具执行成功后调用，保存变更前快照）。
pub fn record_file_checkpoint(
    conn: &Connection,
    conv_id: &str,
    tool_name: &str,
    rel_path: &str,
    prev_content: Option<&str>,
    extra_json: Option<&str>,
    revertible: bool,
) -> SqlResult<FileCheckpoint> {
    let id = Uuid::new_v4().to_string();
    let now = now_ts();
    let seq: i64 = conn.query_row(
        "SELECT COALESCE(MAX(seq), 0) + 1 FROM file_checkpoints WHERE conversation_id = ?1",
        params![conv_id],
        |row| row.get(0),
    )?;
    conn.execute(
        "INSERT INTO file_checkpoints
         (id, conversation_id, seq, tool_name, rel_path, prev_content, extra_json, revertible, reverted, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0, ?9)",
        params![id, conv_id, seq, tool_name, rel_path, prev_content, extra_json, revertible as i64, now],
    )?;
    Ok(FileCheckpoint {
        id,
        conversation_id: conv_id.to_string(),
        seq,
        tool_name: tool_name.to_string(),
        rel_path: rel_path.to_string(),
        prev_content: prev_content.map(str::to_string),
        extra_json: extra_json.map(str::to_string),
        revertible,
        reverted: false,
        created_at: now,
    })
}

/// 查询会话的全部检查点，按序号正序（含已回退的，供 UI 展示状态）。
pub fn list_file_checkpoints(conn: &Connection, conv_id: &str) -> SqlResult<Vec<FileCheckpoint>> {
    let mut stmt = conn.prepare(
        "SELECT id, conversation_id, seq, tool_name, rel_path, prev_content, extra_json, revertible, reverted, created_at
         FROM file_checkpoints
         WHERE conversation_id = ?1
         ORDER BY seq ASC",
    )?;
    let rows = stmt.query_map([conv_id], |row| {
        Ok(FileCheckpoint {
            id: row.get(0)?,
            conversation_id: row.get(1)?,
            seq: row.get(2)?,
            tool_name: row.get(3)?,
            rel_path: row.get(4)?,
            prev_content: row.get(5)?,
            extra_json: row.get(6)?,
            revertible: row.get::<_, i64>(7)? != 0,
            reverted: row.get::<_, i64>(8)? != 0,
            created_at: row.get(9)?,
        })
    })?;
    rows.collect()
}

/// 把一个检查点标记为已回退。
pub fn mark_checkpoint_reverted(conn: &Connection, checkpoint_id: &str) -> SqlResult<()> {
    conn.execute(
        "UPDATE file_checkpoints SET reverted = 1 WHERE id = ?1",
        params![checkpoint_id],
    )?;
    Ok(())
}

// ── MCP Server CRUD ──────────────────────────────────────────────────────

/// MCP server 配置记录。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerRecord {
    pub id: String,
    pub name: String,
    /// 启动命令行（stdio）或 http(s):// URL（HTTP 传输）。
    pub command: String,
    pub enabled: bool,
    /// HTTP 传输的静态 Bearer Token（OAuth 取得后也存于此）；None 表示无。
    pub auth_token: Option<String>,
    pub created_at: i64,
}

/// 新增一个 MCP server 配置（可选静态 token）。
pub fn add_mcp_server(
    conn: &Connection,
    name: &str,
    command: &str,
    auth_token: Option<&str>,
) -> SqlResult<McpServerRecord> {
    let id = Uuid::new_v4().to_string();
    let now = now_ts();
    conn.execute(
        "INSERT INTO mcp_servers (id, name, command, enabled, auth_token, created_at) VALUES (?1, ?2, ?3, 1, ?4, ?5)",
        params![id, name, command, auth_token, now],
    )?;
    Ok(McpServerRecord {
        id,
        name: name.to_string(),
        command: command.to_string(),
        enabled: true,
        auth_token: auth_token.map(str::to_string),
        created_at: now,
    })
}

/// 列出全部 MCP server 配置。
pub fn list_mcp_servers(conn: &Connection) -> SqlResult<Vec<McpServerRecord>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, command, enabled, auth_token, created_at FROM mcp_servers ORDER BY created_at ASC",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(McpServerRecord {
            id: row.get(0)?,
            name: row.get(1)?,
            command: row.get(2)?,
            enabled: row.get::<_, i64>(3)? != 0,
            auth_token: row.get(4)?,
            created_at: row.get(5)?,
        })
    })?;
    rows.collect()
}

/// 更新 MCP server 的 auth_token（OAuth 取得 token 后持久化）。
pub fn set_mcp_server_token(conn: &Connection, id: &str, token: &str) -> SqlResult<()> {
    conn.execute(
        "UPDATE mcp_servers SET auth_token = ?1 WHERE id = ?2",
        params![token, id],
    )?;
    Ok(())
}

/// 设置 MCP server 启用状态。
pub fn set_mcp_server_enabled(conn: &Connection, id: &str, enabled: bool) -> SqlResult<()> {
    conn.execute(
        "UPDATE mcp_servers SET enabled = ?1 WHERE id = ?2",
        params![enabled as i64, id],
    )?;
    Ok(())
}

/// 删除一个 MCP server 配置。
pub fn remove_mcp_server(conn: &Connection, id: &str) -> SqlResult<()> {
    conn.execute("DELETE FROM mcp_servers WHERE id = ?1", params![id])?;
    Ok(())
}

// ── Permission Rule CRUD ─────────────────────────────────────────────────

/// 保存一条「总是允许」权限规则（如 `cmd:git push`、`tool:write_file`）；重复插入幂等。
pub fn add_permission_rule(conn: &Connection, rule: &str) -> SqlResult<()> {
    conn.execute(
        "INSERT OR IGNORE INTO permission_rules (id, rule, created_at) VALUES (?1, ?2, ?3)",
        params![Uuid::new_v4().to_string(), rule, now_ts()],
    )?;
    Ok(())
}

/// 读取全部权限规则。
pub fn list_permission_rules(conn: &Connection) -> SqlResult<Vec<String>> {
    let mut stmt = conn.prepare("SELECT rule FROM permission_rules ORDER BY created_at ASC")?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
    rows.collect()
}

/// 删除一条权限规则。
pub fn remove_permission_rule(conn: &Connection, rule: &str) -> SqlResult<()> {
    conn.execute("DELETE FROM permission_rules WHERE rule = ?1", params![rule])?;
    Ok(())
}

// ── Workspace CRUD ────────────────────────────────────────────────────────

/// 保存当前活动工作区。
///
/// 输入数据库连接和用户授权目录路径；输出新的 Workspace 记录。MVP 阶段只保留一个活动工作区，
/// 写入新工作区前会清除旧记录，后续项目列表能力成熟后再扩展为多工作区。
pub fn save_active_workspace(conn: &Connection, path: &str) -> SqlResult<Workspace> {
    let id = Uuid::new_v4().to_string();
    let now = now_ts();
    let name = workspace_name_from_path(path);

    conn.execute("DELETE FROM workspaces", [])?;
    conn.execute(
        "INSERT INTO workspaces (id, name, path, created_at, updated_at, active)
         VALUES (?1, ?2, ?3, ?4, ?4, 1)",
        params![id, name, path, now],
    )?;

    Ok(Workspace {
        id,
        name,
        path: path.to_string(),
        created_at: now,
        updated_at: now,
        active: true,
    })
}

/// 读取当前活动工作区。
///
/// 输入数据库连接；如果用户已绑定工作区，返回 Workspace，否则返回 None。
pub fn get_active_workspace(conn: &Connection) -> SqlResult<Option<Workspace>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, path, created_at, updated_at, active
         FROM workspaces
         WHERE active = 1
         ORDER BY updated_at DESC
         LIMIT 1",
    )?;
    let mut rows = stmt.query([])?;

    if let Some(row) = rows.next()? {
        Ok(Some(Workspace {
            id: row.get(0)?,
            name: row.get(1)?,
            path: row.get(2)?,
            created_at: row.get(3)?,
            updated_at: row.get(4)?,
            active: row.get::<_, i64>(5)? == 1,
        }))
    } else {
        Ok(None)
    }
}

/// 清除当前活动工作区绑定。
///
/// 输入数据库连接；删除 MVP 阶段的工作区记录，用于用户撤销当前授权目录。
pub fn clear_active_workspace(conn: &Connection) -> SqlResult<()> {
    conn.execute("DELETE FROM workspaces WHERE active = 1", [])?;
    Ok(())
}

fn workspace_name_from_path(path: &str) -> String {
    // 从路径末尾提取目录名，提取失败时退回完整路径，避免 UI 出现空标题。
    Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.trim().is_empty())
        .unwrap_or(path)
        .to_string()
}

// ── Activity Event ────────────────────────────────────────────────────────

/// 记录一条 Activity Event。
///
/// 输入会话 ID、事件类型、状态及可选的工具名、输入/输出 JSON、错误信息和工作区快照；
/// 写入 activity_events 表用于审计与前端折叠过程展示。本方法不做权限判断。
#[allow(clippy::too_many_arguments)]
pub fn record_activity_event(
    conn: &Connection,
    conversation_id: &str,
    event_type: &str,
    tool_name: Option<&str>,
    status: &str,
    input_json: Option<&str>,
    output_json: Option<&str>,
    error_message: Option<&str>,
    workspace_path: Option<&str>,
) -> SqlResult<ActivityEventRecord> {
    let id = Uuid::new_v4().to_string();
    let now = now_ts();
    conn.execute(
        "INSERT INTO activity_events
         (id, conversation_id, event_type, tool_name, status, input_json,
          output_json, error_message, workspace_path, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            id,
            conversation_id,
            event_type,
            tool_name,
            status,
            input_json,
            output_json,
            error_message,
            workspace_path,
            now
        ],
    )?;
    Ok(ActivityEventRecord {
        id,
        conversation_id: conversation_id.to_string(),
        event_type: event_type.to_string(),
        tool_name: tool_name.map(str::to_string),
        status: status.to_string(),
        input_json: input_json.map(str::to_string),
        output_json: output_json.map(str::to_string),
        error_message: error_message.map(str::to_string),
        workspace_path: workspace_path.map(str::to_string),
        created_at: now,
    })
}

/// 查询会话的所有 Activity Event，按时间正序。
pub fn get_activity_events(
    conn: &Connection,
    conv_id: &str,
) -> SqlResult<Vec<ActivityEventRecord>> {
    let mut stmt = conn.prepare(
        "SELECT id, conversation_id, event_type, tool_name, status, input_json,
                output_json, error_message, workspace_path, created_at
         FROM activity_events
         WHERE conversation_id = ?1
         ORDER BY created_at ASC",
    )?;
    let rows = stmt.query_map([conv_id], |row| {
        Ok(ActivityEventRecord {
            id: row.get(0)?,
            conversation_id: row.get(1)?,
            event_type: row.get(2)?,
            tool_name: row.get(3)?,
            status: row.get(4)?,
            input_json: row.get(5)?,
            output_json: row.get(6)?,
            error_message: row.get(7)?,
            workspace_path: row.get(8)?,
            created_at: row.get(9)?,
        })
    })?;
    rows.collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_and_lists_file_checkpoints_with_seq() {
        let db_path = std::env::temp_dir().join(format!("novacode-storage-{}.db", Uuid::new_v4()));
        let conn = init_db(&db_path).expect("db should initialize");
        let conv = create_conversation(&conn).expect("conv");

        let c1 = record_file_checkpoint(&conn, &conv.id, "write_file", "a.txt", Some("old"), None, true)
            .expect("checkpoint 1");
        let c2 = record_file_checkpoint(&conn, &conv.id, "create_file", "b.txt", None, None, true)
            .expect("checkpoint 2");
        assert_eq!(c1.seq, 1);
        assert_eq!(c2.seq, 2);

        mark_checkpoint_reverted(&conn, &c2.id).expect("mark reverted");
        let list = list_file_checkpoints(&conn, &conv.id).expect("list");
        assert_eq!(list.len(), 2);
        assert!(!list[0].reverted);
        assert!(list[1].reverted);
        assert_eq!(list[0].prev_content.as_deref(), Some("old"));
        assert_eq!(list[1].prev_content, None);

        let _ = std::fs::remove_file(db_path);
    }

    #[test]
    fn permission_rules_are_idempotent() {
        let db_path = std::env::temp_dir().join(format!("novacode-storage-{}.db", Uuid::new_v4()));
        let conn = init_db(&db_path).expect("db should initialize");

        add_permission_rule(&conn, "cmd:git push").expect("rule 1");
        add_permission_rule(&conn, "cmd:git push").expect("rule dup");
        add_permission_rule(&conn, "tool:write_file").expect("rule 2");

        let rules = list_permission_rules(&conn).expect("list");
        assert_eq!(rules.len(), 2);
        assert!(rules.contains(&"cmd:git push".to_string()));

        let _ = std::fs::remove_file(db_path);
    }

    #[test]
    fn saves_and_replaces_active_workspace() {
        let db_path = std::env::temp_dir().join(format!("novacode-storage-{}.db", Uuid::new_v4()));
        let conn = init_db(&db_path).expect("db should initialize");

        let first = save_active_workspace(&conn, "C:\\Users\\AIT\\Desktop\\NovaCode")
            .expect("workspace should save");
        assert_eq!(first.name, "NovaCode");
        assert_eq!(first.path, "C:\\Users\\AIT\\Desktop\\NovaCode");

        let second = save_active_workspace(&conn, "C:\\Users\\AIT\\Desktop\\Other")
            .expect("workspace should replace");
        let loaded = get_active_workspace(&conn).expect("workspace should load");

        assert_eq!(loaded.map(|workspace| workspace.path), Some(second.path));
        assert_ne!(first.id, second.id);

        clear_active_workspace(&conn).expect("workspace should clear");
        assert!(get_active_workspace(&conn).expect("query should succeed").is_none());

        let _ = std::fs::remove_file(db_path);
    }

    #[test]
    fn creates_conversation_with_workspace_snapshot() {
        let db_path = std::env::temp_dir().join(format!("novacode-storage-{}.db", Uuid::new_v4()));
        let conn = init_db(&db_path).expect("db should initialize");

        let conv = create_conversation_with_workspace(
            &conn,
            Some("C:\\Users\\AIT\\Desktop\\NovaCode"),
            Some("NovaCode"),
        )
        .expect("conversation should save workspace snapshot");
        let stored = list_conversations(&conn).expect("conversation should list");

        assert_eq!(conv.workspace_path.as_deref(), Some("C:\\Users\\AIT\\Desktop\\NovaCode"));
        assert_eq!(conv.workspace_name.as_deref(), Some("NovaCode"));
        assert_eq!(conv.mode, "local_workspace");
        assert_eq!(stored[0].workspace_path.as_deref(), Some("C:\\Users\\AIT\\Desktop\\NovaCode"));

        let _ = std::fs::remove_file(db_path);
    }

    #[test]
    fn gets_conversation_by_id_with_workspace_snapshot() {
        let db_path = std::env::temp_dir().join(format!("novacode-storage-{}.db", Uuid::new_v4()));
        let conn = init_db(&db_path).expect("db should initialize");

        let conv = create_conversation_with_workspace(
            &conn,
            Some("C:\\Users\\AIT\\Desktop\\NovaCode"),
            Some("NovaCode"),
        )
        .expect("conversation should save workspace snapshot");
        let stored = get_conversation(&conn, &conv.id)
            .expect("query should succeed")
            .expect("conversation should exist");

        assert_eq!(stored.id, conv.id);
        assert_eq!(stored.workspace_path.as_deref(), Some("C:\\Users\\AIT\\Desktop\\NovaCode"));
        assert_eq!(stored.workspace_name.as_deref(), Some("NovaCode"));

        let _ = std::fs::remove_file(db_path);
    }
}
