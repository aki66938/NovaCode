import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { open, save } from "@tauri-apps/plugin-dialog";
import { useEffect, useRef, useState } from "react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import rehypeHighlight from "rehype-highlight";
import "highlight.js/styles/github.css";
import {
  SquarePen, Search, Pin, Archive, ArchiveRestore, Trash2, Settings2,
  Paperclip, ListChecks, Square, ArrowUp, GitCompare, Plug, Sun, Moon,
  Check, X, Ban, Info, CircleDot, CheckSquare, ChevronRight,
  ChevronDown, FolderOpen, Gauge, AtSign, CornerDownRight,
} from "lucide-react";

/** NovaCode 品牌标识：深海声纳波纹（致敬 DeepSeek 的「deep」，非官方鲸鱼 logo） */
function BrandMark({ size = 24 }: { size?: number }) {
  return (
    <svg width={size} height={size} viewBox="0 0 32 32" aria-hidden="true">
      <path d="M4 19 C9 13 14 13 16 16 C18 19 23 19 28 13" fill="none"
        stroke="var(--brand)" strokeWidth="2.6" strokeLinecap="round" />
      <circle cx="16" cy="16" r="2.4" fill="var(--brand)" />
    </svg>
  );
}
import {
  DEEPSEEK_MODELS,
  DEFAULT_DEEPSEEK_MODEL_ID,
  getApiKeyStatusLabel,
  getPermissionModeLabel,
  type ApiKeyStatus,
  type DeepSeekModelId,
  type PermissionMode,
} from "@novacode/ui";
import "./styles.css";

// ── 类型定义 ──────────────────────────────────────────────────────────────

/** 消息中的文字块，直接 Markdown 渲染 */
type TextPart = { type: "text"; content: string };

/** 消息中的工具执行卡片，内联展示于叙述文字之间 */
type ToolPart = {
  type: "tool";
  toolName: string;
  target: string;
  status: "running" | "succeeded" | "failed" | "denied";
  error?: string;
  /** 文件写类工具的行级 diff（unified 格式），点击行可展开查看 */
  diff?: string;
  added?: number;
  removed?: number;
  /** run_command 运行中的实时输出（行累积，截断保留尾部） */
  liveOutput?: string;
};

/** Agent 自维护的任务清单项（todo_write 工具） */
type TodoItem = {
  text: string;
  status: "pending" | "in_progress" | "done";
};

/** 文件变更检查点（rewind 用） */
type FileCheckpoint = {
  id: string;
  conversationId: string;
  seq: number;
  toolName: string;
  relPath: string;
  revertible: boolean;
  reverted: boolean;
  createdAt: number;
};

type AppInfo = { version: string; dataDir: string };

/** DeepSeek 账户余额（官方 /user/balance，唯一账户信息来源） */
type BalanceInfo = {
  currency: string;
  totalBalance: string;
  grantedBalance: string;
  toppedUpBalance: string;
};
type UserBalance = { isAvailable: boolean; balanceInfos: BalanceInfo[] };
type BalanceState =
  | { status: "idle" }
  | { status: "loading" }
  | { status: "ok"; data: UserBalance }
  | { status: "error"; message: string };

/** MCP server 配置与连接状态 */
type McpServer = {
  id: string;
  name: string;
  command: string;
  enabled: boolean;
  connected: boolean;
  toolCount: number;
};

/** 高风险动作审批请求，由后端在 AskEveryTime / 越界场景下发起 */
type ApprovalRequest = {
  actionId: string;
  toolName: string;
  target: string;
};

/** ask_user 结构化提问：Agent 在需求含糊时主动发起，前端弹选择卡片 */
type AskOption = { label: string; description?: string };
type AskQuestion = {
  question: string;
  header?: string;
  multiSelect?: boolean;
  options: AskOption[];
};
type AskUserRequest = { questionId: string; questions: AskQuestion[] };

/** 系统通知卡片：上下文压缩等用户需要感知的运行时事件，内联显示在对话流中 */
type NoticePart = { type: "notice"; text: string };

type MessagePart = TextPart | ToolPart | NoticePart;

type Message = {
  role: "user" | "assistant";
  /** 所有内容都用 parts 表示，文字与工具卡片交错排列 */
  parts: MessagePart[];
  usage?: UsageSummary;
};

type UsageSummary = {
  promptTokens: number;
  completionTokens: number;
  totalTokens: number;
  cacheHitTokens: number;
  cacheMissTokens: number;
  reasoningTokens: number;
  estimatedCostUsd: number;
  usageSource: string;
  pricingVersion: string;
};

type Conversation = {
  id: string;
  title: string;
  workspacePath?: string | null;
  workspaceName?: string | null;
  mode: "chat_only" | "local_workspace";
  pinned: boolean;
  archived: boolean;
  createdAt: number;
  updatedAt: number;
};

type StoredMessage = {
  id: string;
  conversationId: string;
  role: string;
  content: string;
  usageJson: string | null;
  partsJson: string | null;
  createdAt: number;
};

type ToolEvent = {
  toolName: string;
  status: string;
  inputJson?: string | null;
  outputJson?: string | null;
  errorMessage?: string | null;
};

type DraftWorkspace = {
  name: string;
  path: string;
};

type UpdateState =
  | { status: "idle" }
  | { status: "checking" }
  | { status: "uptodate" }
  | { status: "available"; version: string }
  | { status: "downloading"; progress: number }
  | { status: "error"; message: string };

const PERMISSION_MODES: PermissionMode[] = [
  "restricted",
  "ask_every_time",
  "workspace_auto",
  "full_access",
];

/** 斜杠命令清单：输入框以 / 开头时弹出 */
const SLASH_COMMANDS: Array<{ cmd: string; desc: string }> = [
  { cmd: "/compact", desc: "把当前会话历史压缩为摘要，释放上下文" },
  { cmd: "/clear", desc: "开启一个全新会话" },
  { cmd: "/init", desc: "让 Agent 分析项目并生成 NovaCode.md 长期记忆" },
  { cmd: "/rewind", desc: "打开文件变更记录，可回退改动" },
  { cmd: "/model", desc: "切换模型，例如 /model deepseek-v4-pro" },
];

/** /init 发送的固定提示词（对标 CC 的 /init 生成 CLAUDE.md） */
const INIT_PROMPT =
  "请分析当前工作区项目：阅读关键文件、理解项目目标、技术栈、目录结构与开发约定，然后在工作区根目录创建（或更新）NovaCode.md 文件，写入项目长期记忆：项目目标、架构概览、关键约定、常用命令。内容要精炼，控制在 100 行以内。";

// ── App ───────────────────────────────────────────────────────────────────

export function App() {
  const [apiKeyStatus, setApiKeyStatus] = useState<ApiKeyStatus>({ state: "missing" });
  const [conversations, setConversations] = useState<Conversation[]>([]);
  const [activeConvId, setActiveConvId] = useState<string | null>(null);
  const [messages, setMessages] = useState<Message[]>([]);
  const [input, setInput] = useState("");
  const [sending, setSending] = useState(false);
  const [model, setModel] = useState<DeepSeekModelId>(DEFAULT_DEEPSEEK_MODEL_ID);
  const [permissionMode, setPermissionMode] = useState<PermissionMode>("workspace_auto");
  const [draftWorkspace, setDraftWorkspace] = useState<DraftWorkspace | null>(null);
  const [workspaceError, setWorkspaceError] = useState<string | null>(null);
  const [approval, setApproval] = useState<ApprovalRequest | null>(null);
  const [askUser, setAskUser] = useState<AskUserRequest | null>(null);
  // 侧边栏：搜索过滤、行内重命名、归档区展开
  const [searchQuery, setSearchQuery] = useState("");
  const [editingConvId, setEditingConvId] = useState<string | null>(null);
  const [editingTitle, setEditingTitle] = useState("");
  const [showArchived, setShowArchived] = useState(false);
  // Agent 实时状态：思考中 / 执行工具 / 压缩上下文 / 输出中，发送期间常驻显示
  const [agentStatus, setAgentStatus] = useState<string | null>(null);
  const [elapsedSec, setElapsedSec] = useState(0);
  // Agent 自维护任务清单（todo_write），常驻输入框上方
  const [todos, setTodos] = useState<TodoItem[]>([]);
  // 计划模式：先出计划等确认，本轮不执行工具
  const [planMode, setPlanMode] = useState(false);
  // 设置面板 / 变更记录面板
  const [showSettings, setShowSettings] = useState(false);
  const [showChanges, setShowChanges] = useState(false);
  const [checkpoints, setCheckpoints] = useState<FileCheckpoint[]>([]);
  const [appInfo, setAppInfo] = useState<AppInfo | null>(null);
  const [mcpServers, setMcpServers] = useState<McpServer[]>([]);
  const [balance, setBalance] = useState<BalanceState>({ status: "idle" });
  const [permRules, setPermRules] = useState<string[]>([]);
  const [commandSandbox, setCommandSandbox] = useState(true);
  const [taskBudget, setTaskBudget] = useState(0);
  // 自定义斜杠命令（工作区 .novacode/commands/*.md）
  const [customCommands, setCustomCommands] = useState<Array<{ name: string; description: string; body: string }>>([]);
  // Steering：运行中已排队但尚未被注入的插话消息
  const [queuedSteering, setQueuedSteering] = useState<string[]>([]);
  const [theme, setTheme] = useState<"light" | "dark">("light");
  // @文件引用补全
  const [workspaceFiles, setWorkspaceFiles] = useState<string[]>([]);
  const [fileMention, setFileMention] = useState<string | null>(null);
  // 上下文用量（上次请求 prompt_tokens / 压缩阈值），常驻状态栏
  const [ctxUsage, setCtxUsage] = useState<{ promptTokens: number; softLimit: number } | null>(null);
  const [update, setUpdate] = useState<UpdateState>({ status: "idle" });
  const messagesEndRef = useRef<HTMLDivElement>(null);
  const streamingTextRef = useRef(""); // 只累积纯文字内容，用于 chat-done 持久化（供模型上下文）
  const streamingPartsRef = useRef<MessagePart[]>([]); // 跟踪当前 assistant 的完整 parts，用于持久化交错的工具卡片
  const streamingUsageRef = useRef<UsageSummary | null>(null);

  useEffect(() => {
    invoke<string>("get_deepseek_api_key_status")
      .then((raw) => {
        const state =
          raw === "Configured" ? "configured" :
          raw === "ConnectionFailed" ? "connection_failed" :
          "missing";
        setApiKeyStatus({ state });
      })
      .catch(() => setApiKeyStatus({ state: "missing" }));
  }, []);

  useEffect(() => {
    loadConversations();
  }, []);

  useEffect(() => {
    if (!activeConvId) { setMessages([]); return; }
    invoke<StoredMessage[]>("load_messages", { conversationId: activeConvId })
      .then((stored) => setMessages(stored.map(storedToMessage)))
      .catch(() => setMessages([]));
  }, [activeConvId]);

  useEffect(() => {
    messagesEndRef.current?.scrollIntoView({ behavior: "smooth" });
  }, [messages]);

  // 持续监听高风险动作审批请求，弹出确认框
  useEffect(() => {
    const unlisten = listen<ApprovalRequest>("approval-request", (e) => {
      setApproval(e.payload);
    });
    return () => {
      unlisten.then((fn) => fn());
    };
  }, []);

  // 监听 Agent 发起的 ask_user 结构化提问，弹出选择卡片
  useEffect(() => {
    const unlisten = listen<AskUserRequest>("ask-user-request", (e) => {
      setAskUser(e.payload);
    });
    return () => {
      unlisten.then((fn) => fn());
    };
  }, []);

  // 发送期间的耗时计时器：每秒刷新，让用户确信 agent 仍在工作而不是被截断
  useEffect(() => {
    if (!sending) {
      setElapsedSec(0);
      setAgentStatus(null);
      return;
    }
    const start = Date.now();
    const timer = setInterval(
      () => setElapsedSec(Math.floor((Date.now() - start) / 1000)),
      1000
    );
    return () => clearInterval(timer);
  }, [sending]);

  // 主题：启动时从 localStorage 恢复（默认跟随系统），并同步到 <html data-theme>
  useEffect(() => {
    const saved = localStorage.getItem("novacode.theme") as "light" | "dark" | null;
    const initial =
      saved ?? (window.matchMedia?.("(prefers-color-scheme: dark)").matches ? "dark" : "light");
    setTheme(initial);
  }, []);
  useEffect(() => {
    document.documentElement.setAttribute("data-theme", theme);
    localStorage.setItem("novacode.theme", theme);
  }, [theme]);

  // 启动时恢复默认模型与权限模式（localStorage 持久化）
  useEffect(() => {
    const savedModel = localStorage.getItem("novacode.defaultModel");
    if (savedModel && DEEPSEEK_MODELS.some((m) => m.id === savedModel)) {
      setModel(savedModel as DeepSeekModelId);
    }
    const savedMode = localStorage.getItem("novacode.defaultPermissionMode");
    if (savedMode && PERMISSION_MODES.includes(savedMode as PermissionMode)) {
      setPermissionMode(savedMode as PermissionMode);
    }
  }, []);

  // todo 清单实时更新（todo_write 工具）
  useEffect(() => {
    const unlisten = listen<TodoItem[]>("todo-update", (e) => {
      setTodos(Array.isArray(e.payload) ? e.payload : []);
    });
    return () => {
      unlisten.then((fn) => fn());
    };
  }, []);

  // Steering：一条排队的插话被注入后，从待注入列表里移除一条
  useEffect(() => {
    const unlisten = listen<string>("steering-injected", () => {
      setQueuedSteering((prev) => prev.slice(1));
    });
    return () => {
      unlisten.then((fn) => fn());
    };
  }, []);

  // 后台命令完成通知：插入通知卡片
  useEffect(() => {
    const unlisten = listen<{ command: string; exitCode?: number | null; error?: string }>(
      "background-command-done",
      (e) => {
        const { command, exitCode, error } = e.payload;
        const text = error
          ? `后台命令失败：${command} — ${error}`
          : `后台命令完成：${command}（退出码 ${exitCode ?? "?"}）`;
        appendNoticeToLastMessage(text);
      }
    );
    return () => {
      unlisten.then((fn) => fn());
    };
  }, []);

  // 命令实时输出：附加到最近一个运行中的 run_command 卡片
  useEffect(() => {
    const unlisten = listen<string>("command-output", (e) => {
      setMessages((prev) => {
        const updated = [...prev];
        const lastIdx = updated.length - 1;
        const last = updated[lastIdx];
        if (!last || last.role !== "assistant") return prev;
        const parts = [...last.parts];
        for (let i = parts.length - 1; i >= 0; i--) {
          const p = parts[i];
          if (p.type === "tool" && p.toolName === "run_command" && p.status === "running") {
            const existing = p.liveOutput ?? "";
            // 只保留尾部 4000 字符，防止超长输出拖垮渲染
            const next = (existing + e.payload + "\n").slice(-4000);
            parts[i] = { ...p, liveOutput: next };
            updated[lastIdx] = { ...last, parts };
            streamingPartsRef.current = parts;
            return updated;
          }
        }
        return prev;
      });
    });
    return () => {
      unlisten.then((fn) => fn());
    };
  }, []);

  // 会话切换时加载工作区文件列表（@引用补全），并清空 todo
  useEffect(() => {
    setTodos([]);
    setWorkspaceFiles([]);
    setCustomCommands([]);
    if (!activeConvId) return;
    invoke<string[]>("list_workspace_files", { conversationId: activeConvId })
      .then(setWorkspaceFiles)
      .catch(() => setWorkspaceFiles([]));
    invoke<Array<{ name: string; description: string; body: string }>>("list_custom_commands", { conversationId: activeConvId })
      .then(setCustomCommands)
      .catch(() => setCustomCommands([]));
  }, [activeConvId]);

  /** 向当前最后一条 assistant 消息追加通知卡片 */
  function appendNoticeToLastMessage(text: string) {
    setMessages((prev) => {
      const updated = [...prev];
      const lastIdx = updated.length - 1;
      const last = updated[lastIdx];
      if (!last || last.role !== "assistant") return prev;
      const parts: MessagePart[] = [...last.parts, { type: "notice", text }];
      updated[lastIdx] = { ...last, parts };
      streamingPartsRef.current = parts;
      return updated;
    });
  }

  useEffect(() => {
    const timer = setTimeout(() => {
      invoke<string | null>("check_update")
        .then((v) => { if (v) setUpdate({ status: "available", version: v }); })
        .catch(() => {});
    }, 3000);
    const unlistenProgress = listen<number>("update-progress", (e) => {
      setUpdate({ status: "downloading", progress: e.payload });
    });
    return () => {
      clearTimeout(timer);
      unlistenProgress.then((fn) => fn());
    };
  }, []);

  // ── 工具函数 ────────────────────────────────────────────────────────────

  function storedToMessage(s: StoredMessage): Message {
    const usage = s.usageJson ? JSON.parse(s.usageJson) as UsageSummary : undefined;
    // 优先用持久化的结构化 parts 还原文字+工具卡片交错；缺失时回退为单个 text part。
    let parts: MessagePart[] = [{ type: "text", content: s.content }];
    if (s.partsJson) {
      try {
        const parsed = JSON.parse(s.partsJson) as MessagePart[];
        if (Array.isArray(parsed) && parsed.length > 0) parts = parsed;
      } catch {
        // 解析失败保留纯文字回退
      }
    }
    return { role: s.role as "user" | "assistant", parts, usage };
  }

  /** 从工具输入参数提取展示目标（path / from / command）*/
  function extractTarget(inputJson: string | null | undefined): string {
    if (!inputJson) return "";
    try {
      const parsed = JSON.parse(inputJson) as Record<string, unknown>;
      const target = parsed.path ?? parsed.from ?? parsed.command ?? "";
      return typeof target === "string" ? target : "";
    } catch {
      return "";
    }
  }

  async function loadConversations() {
    const list = await invoke<Conversation[]>("get_conversations").catch(() => []);
    setConversations(list);
    return list;
  }

  // ── 会话操作 ────────────────────────────────────────────────────────────

  async function handleNewConversation() {
    if (sending) return;
    setActiveConvId(null);
    setMessages([]);
    setInput("");
    setDraftWorkspace(null);
    setWorkspaceError(null);
  }

  async function handleSelectConversation(id: string) {
    if (id === activeConvId || sending) return;
    setActiveConvId(id);
    setDraftWorkspace(null);
    setWorkspaceError(null);
  }

  async function handleDeleteConversation(e: React.MouseEvent, id: string) {
    e.stopPropagation();
    await invoke("remove_conversation", { conversationId: id }).catch(() => {});
    setConversations((prev) => prev.filter((c) => c.id !== id));
    if (activeConvId === id) {
      setActiveConvId(null);
      setMessages([]);
    }
  }

  async function handleTogglePin(e: React.MouseEvent, conv: Conversation) {
    e.stopPropagation();
    await invoke("pin_conversation", {
      conversationId: conv.id,
      pinned: !conv.pinned,
    }).catch(() => {});
    await loadConversations();
  }

  async function handleToggleArchive(e: React.MouseEvent, conv: Conversation) {
    e.stopPropagation();
    await invoke("archive_conversation", {
      conversationId: conv.id,
      archived: !conv.archived,
    }).catch(() => {});
    await loadConversations();
  }

  function startRename(e: React.MouseEvent, conv: Conversation) {
    e.stopPropagation();
    setEditingConvId(conv.id);
    setEditingTitle(conv.title);
  }

  async function commitRename() {
    const id = editingConvId;
    const title = editingTitle.trim();
    setEditingConvId(null);
    if (!id || !title) return;
    await invoke("rename_conversation", { conversationId: id, title }).catch(() => {});
    setConversations((prev) =>
      prev.map((c) => (c.id === id ? { ...c, title } : c))
    );
  }

  async function handleSelectWorkspace() {
    try {
      const selected = await open({ directory: true, multiple: false });
      if (!selected || Array.isArray(selected)) return;
      setDraftWorkspace({ path: selected, name: basenameFromPath(selected) });
      setWorkspaceError(null);
    } catch (err) {
      setWorkspaceError(String(err));
    }
  }

  // ── 发送消息 ────────────────────────────────────────────────────────────

  /** 处理斜杠命令；返回 true 表示已消费输入，不再走正常发送。 */
  async function handleSlashCommand(text: string): Promise<boolean> {
    if (!text.startsWith("/")) return false;
    const [cmd, ...rest] = text.split(/\s+/);
    switch (cmd) {
      case "/clear":
        setInput("");
        await handleNewConversation();
        return true;
      case "/rewind":
        setInput("");
        await openChangesPanel();
        return true;
      case "/model": {
        const target = rest.join(" ").trim();
        const found = DEEPSEEK_MODELS.find((m) => m.id === target);
        setInput("");
        if (found) {
          setModel(found.id);
          localStorage.setItem("novacode.defaultModel", found.id);
        }
        return true;
      }
      case "/compact": {
        setInput("");
        if (!activeConvId) return true;
        setAgentStatus("正在压缩会话…");
        setSending(true);
        try {
          await invoke("compact_history", { conversationId: activeConvId, model });
          const stored = await invoke<StoredMessage[]>("load_messages", {
            conversationId: activeConvId,
          });
          setMessages(stored.map(storedToMessage));
        } catch (err) {
          appendNoticeToLastMessage(humanizeError(String(err)));
        } finally {
          setSending(false);
        }
        return true;
      }
      default: {
        // 自定义斜杠命令（.novacode/commands/<name>.md）：用命令体替换发送，$ARGUMENTS 替换为参数
        const custom = customCommands.find((c) => c.name === cmd);
        if (custom) {
          const args = rest.join(" ");
          const filled = custom.body.replace(/\$ARGUMENTS/g, args);
          setInput("");
          await sendText(filled);
          return true;
        }
        return false;
      }
    }
  }

  async function handleSend() {
    let text = input.trim();
    if (!text || sending) return;
    // /init 替换为固定提示词走正常发送；其余斜杠命令直接消费
    if (text === "/init") {
      text = INIT_PROMPT;
    } else if (await handleSlashCommand(text)) {
      return;
    }
    await sendText(text);
  }

  /** 发送一段文本（供输入框与文件导入复用）。 */
  async function sendText(text: string) {
    if (!text || sending) return;

    let convId = activeConvId;
    if (!convId) {
      const conv = await invoke<Conversation>("new_conversation_with_workspace", {
        workspacePath: draftWorkspace?.path ?? null,
      }).catch((err) => {
        setWorkspaceError(String(err));
        return null;
      });
      if (!conv) return;
      convId = conv.id;
      setConversations((prev) => [conv, ...prev]);
      setActiveConvId(conv.id);
      setWorkspaceError(null);
    }

    await invoke("persist_message", {
      conversationId: convId,
      role: "user",
      content: text,
      usageJson: null,
    }).catch(() => {});

    const currentConv = conversations.find((c) => c.id === convId);
    if (!currentConv || currentConv.title === "新对话") {
      const title = text.slice(0, 20);
      await invoke("rename_conversation", { conversationId: convId, title }).catch(() => {});
      setConversations((prev) =>
        prev.map((c) => (c.id === convId ? { ...c, title } : c))
      );
    }

    // 构建发给后端的纯文字消息（parts 中只取 text 块）
    const outgoing: Message[] = [...messages, { role: "user", parts: [{ type: "text", content: text }] }];
    setMessages([...outgoing, { role: "assistant", parts: [] }]);
    setInput("");
    setSending(true);
    streamingTextRef.current = "";
    streamingPartsRef.current = [];
    streamingUsageRef.current = null;

    // ── 流式事件监听 ────────────────────────────────────────────────────

    // chat-chunk：追加文字到当前 assistant 消息的最后一个 text part
    const unlistenChunk = await listen<string>("chat-chunk", (e) => {
      setAgentStatus("正在输出回复…");
      streamingTextRef.current += e.payload;
      setMessages((prev) => {
        const updated = [...prev];
        const lastIdx = updated.length - 1;
        const last = updated[lastIdx];
        const parts = [...last.parts];
        const tail = parts[parts.length - 1];
        if (tail?.type === "text") {
          parts[parts.length - 1] = { type: "text", content: tail.content + e.payload };
        } else {
          parts.push({ type: "text", content: e.payload });
        }
        updated[lastIdx] = { ...last, parts };
        streamingPartsRef.current = parts;
        return updated;
      });
    });

    // tool-event：running 时插入新卡片，succeeded/failed 时更新最近匹配的 running 卡片
    const unlistenTool = await listen<ToolEvent>("tool-event", (e) => {
      const { toolName, status, inputJson, outputJson, errorMessage } = e.payload;
      if (status === "running") setAgentStatus(`正在执行 ${toolName}…`);
      const target = extractTarget(inputJson);
      // 解析输出中的 diff 信息（文件写类工具）
      let diff: string | undefined;
      let added: number | undefined;
      let removed: number | undefined;
      if (outputJson) {
        try {
          const out = JSON.parse(outputJson) as Record<string, unknown>;
          if (typeof out.diff === "string") diff = out.diff;
          if (typeof out.added === "number") added = out.added;
          if (typeof out.removed === "number") removed = out.removed;
        } catch {
          // 输出非 JSON 时忽略
        }
      }
      setMessages((prev) => {
        const updated = [...prev];
        const lastIdx = updated.length - 1;
        const last = updated[lastIdx];
        if (last.role !== "assistant") return prev;
        const parts = [...last.parts];
        if (status === "running") {
          parts.push({ type: "tool", toolName, target, status: "running" });
        } else if (status === "denied") {
          // 被拒绝的工具没有 running 阶段，直接插入一张拒绝卡片
          parts.push({
            type: "tool",
            toolName,
            target,
            status: "denied",
            error: errorMessage ?? undefined,
          });
        } else {
          // 从后往前找同名的最近 running 卡片并更新状态（附带 diff 与最终输出）
          for (let i = parts.length - 1; i >= 0; i--) {
            const p = parts[i];
            if (p.type === "tool" && p.toolName === toolName && p.status === "running") {
              parts[i] = {
                ...p,
                status: status as "succeeded" | "failed",
                error: errorMessage ?? undefined,
                diff,
                added,
                removed,
              };
              break;
            }
          }
        }
        updated[lastIdx] = { ...last, parts };
        streamingPartsRef.current = parts;
        return updated;
      });
    });

    // agent-status：后端推送的实时状态（思考中第 N 轮 / 压缩上下文中）
    const unlistenStatus = await listen<{ state: string; round?: number }>(
      "agent-status",
      (e) => {
        const { state, round } = e.payload;
        if (state === "thinking") {
          setAgentStatus(round ? `正在思考…（第 ${round} 轮）` : "正在思考…");
        } else if (state === "compacting") {
          setAgentStatus("正在压缩上下文…");
        }
      }
    );

    // context-usage：每轮请求后的真实上下文体积，驱动状态栏百分比
    const unlistenCtx = await listen<{ promptTokens: number; softLimit: number }>(
      "context-usage",
      (e) => setCtxUsage(e.payload)
    );

    // context-compacted：压缩事件，在对话流中插入通知卡片（对标 CC/Codex 的 compact 提示）
    const unlistenCompact = await listen<{ kind: string; count?: number }>(
      "context-compacted",
      (e) => {
        const text =
          e.payload.kind === "summary"
            ? "上下文较长，已自动压缩为任务进度摘要，对话继续"
            : `上下文较长，已压缩 ${e.payload.count ?? 0} 条较早的工具输出`;
        setMessages((prev) => {
          const updated = [...prev];
          const lastIdx = updated.length - 1;
          const last = updated[lastIdx];
          if (last.role !== "assistant") return prev;
          const parts: MessagePart[] = [...last.parts, { type: "notice", text }];
          updated[lastIdx] = { ...last, parts };
          streamingPartsRef.current = parts;
          return updated;
        });
      }
    );

    const unlistenUsage = await listen<UsageSummary>("chat-usage", (e) => {
      streamingUsageRef.current = e.payload;
      setMessages((prev) => {
        const updated = [...prev];
        const last = updated[updated.length - 1];
        updated[updated.length - 1] = { ...last, usage: e.payload };
        return updated;
      });
    });

    const finalConvId = convId;
    const unlistenDone = await listen("chat-done", async () => {
      setSending(false);
      setApproval(null);
      setAskUser(null);
      setQueuedSteering([]);
      unlistenChunk();
      unlistenTool();
      unlistenStatus();
      unlistenCtx();
      unlistenCompact();
      unlistenUsage();
      unlistenDone();

      // 兜底清洗：模型可能把工具调用标记直接吐进正文（纯聊天会话尤其常见——没有工具
      // 时模型会幻觉 <ToolCall>/DSML 语法）。截断标记、保留叙述，并提示用户原因。
      const leakIdx = findToolMarkupIndex(streamingTextRef.current);
      if (leakIdx >= 0) {
        const sanitized = streamingTextRef.current.slice(0, leakIdx).trimEnd();
        streamingTextRef.current = sanitized;
        const conv = conversations.find((c) => c.id === finalConvId);
        const noticeText = conv?.workspacePath
          ? "已清理模型输出中的内部工具标记"
          : "本会话未绑定工作区，Agent 无法执行本地文件操作。请点击「+ 新对话」并选择工作区后再试。";
        const sanitizedParts: MessagePart[] = [
          ...(sanitized ? [{ type: "text", content: sanitized } as MessagePart] : []),
          { type: "notice", text: noticeText },
        ];
        streamingPartsRef.current = sanitizedParts;
        setMessages((prev) => {
          const updated = [...prev];
          const lastIdx = updated.length - 1;
          if (updated[lastIdx]?.role === "assistant") {
            updated[lastIdx] = { ...updated[lastIdx], parts: sanitizedParts };
          }
          return updated;
        });
      }

      const usageJson = streamingUsageRef.current
        ? JSON.stringify(streamingUsageRef.current)
        : null;
      // content 存纯文字（供模型上下文）；partsJson 存文字+工具卡片交错结构（供重启后还原展示）
      const finalParts = streamingPartsRef.current;
      const partsJson = finalParts.length > 0 ? JSON.stringify(finalParts) : null;
      await invoke("persist_message", {
        conversationId: finalConvId,
        role: "assistant",
        content: streamingTextRef.current,
        usageJson,
        partsJson,
      }).catch(() => {});

      const list = await invoke<Conversation[]>("get_conversations").catch(() => []);
      setConversations(list);
    });

    try {
      await invoke("send_message", {
        conversationId: finalConvId,
        messages: outgoing.map((m) => ({
          role: m.role,
          content: m.parts.filter((p) => p.type === "text").map((p) => (p as TextPart).content).join(""),
        })),
        model,
        permissionMode,
        planMode,
      });
    } catch (err) {
      setMessages((prev) => {
        const updated = [...prev];
        updated[updated.length - 1] = {
          role: "assistant",
          parts: [{ type: "text", content: humanizeError(String(err)) }],
        };
        return updated;
      });
      setSending(false);
      unlistenChunk();
      unlistenTool();
      unlistenStatus();
      unlistenCtx();
      unlistenCompact();
      unlistenUsage();
      unlistenDone();
    }
  }

  async function handleStop() {
    // 若有挂起的审批请求，先拒绝它，避免后端工具循环卡在等待中
    if (approval) {
      await invoke("respond_approval", { actionId: approval.actionId, approved: false }).catch(() => {});
      setApproval(null);
    }
    if (!activeConvId) return;
    await invoke("cancel_agent", { conversationId: activeConvId }).catch(() => {});
  }

  async function respondApproval(approved: boolean, remember = false) {
    if (!approval) return;
    const actionId = approval.actionId;
    setApproval(null);
    await invoke("respond_approval", { actionId, approved, remember }).catch(() => {});
  }

  async function respondAskUser(answer: string) {
    if (!askUser) return;
    const questionId = askUser.questionId;
    setAskUser(null);
    await invoke("respond_ask_user", { questionId, answer }).catch(() => {});
  }

  async function openChangesPanel() {
    if (!activeConvId) return;
    const list = await invoke<FileCheckpoint[]>("get_checkpoints", {
      conversationId: activeConvId,
    }).catch(() => [] as FileCheckpoint[]);
    setCheckpoints(list);
    setShowChanges(true);
  }

  async function handleRevert(checkpointId: string) {
    if (!activeConvId) return;
    try {
      const count = await invoke<number>("revert_to_checkpoint", {
        conversationId: activeConvId,
        checkpointId,
      });
      appendNoticeToLastMessage(`已回退 ${count} 处文件变更`);
      await openChangesPanel(); // 刷新列表状态
    } catch (err) {
      appendNoticeToLastMessage(humanizeError(String(err)));
    }
  }

  async function refreshMcpServers() {
    const list = await invoke<McpServer[]>("get_mcp_servers").catch(() => [] as McpServer[]);
    setMcpServers(list);
  }

  async function refreshBalance() {
    setBalance({ status: "loading" });
    try {
      const data = await invoke<UserBalance>("get_account_balance");
      setBalance({ status: "ok", data });
    } catch (err) {
      setBalance({ status: "error", message: humanizeError(String(err)) });
    }
  }

  async function refreshPermRules() {
    const list = await invoke<string[]>("get_permission_rules").catch(() => [] as string[]);
    setPermRules(list);
  }

  async function handleAddPermRule(rule: string) {
    await invoke("create_permission_rule", { rule }).catch(() => {});
    await refreshPermRules();
  }

  async function handleDeletePermRule(rule: string) {
    await invoke("delete_permission_rule", { rule }).catch(() => {});
    await refreshPermRules();
  }

  async function handleToggleSandbox(enabled: boolean) {
    setCommandSandbox(enabled);
    await invoke("set_command_sandbox", { enabled }).catch(() => {});
  }

  async function handleSetBudget(budget: number) {
    setTaskBudget(budget);
    await invoke("set_task_budget", { budget }).catch(() => {});
  }

  async function handleExportConversation() {
    if (!activeConvId) return;
    const path = await save({ defaultPath: "conversation.md", filters: [{ name: "Markdown", extensions: ["md"] }] }).catch(() => null);
    if (!path) return;
    await invoke("export_conversation", { conversationId: activeConvId, path }).catch((e) => appendNoticeToLastMessage(humanizeError(String(e))));
  }

  async function handleExportLedger() {
    const path = await save({ defaultPath: "novacode-token-ledger.csv", filters: [{ name: "CSV", extensions: ["csv"] }] }).catch(() => null);
    if (!path) return;
    await invoke("export_token_ledger", { path }).catch((e) => appendNoticeToLastMessage(humanizeError(String(e))));
  }

  async function handleClearData() {
    if (!window.confirm("确定清除所有会话与消息？此操作不可撤销。")) return;
    await invoke("clear_all_conversations").catch(() => {});
    setConversations([]);
    setActiveConvId(null);
    setMessages([]);
  }

  async function openSettings() {
    const info = await invoke<AppInfo>("get_app_info").catch(() => null);
    setAppInfo(info);
    await refreshMcpServers();
    await refreshPermRules();
    setCommandSandbox(await invoke<boolean>("get_command_sandbox").catch(() => true));
    setTaskBudget(await invoke<number>("get_task_budget").catch(() => 0));
    setShowSettings(true);
    refreshBalance();
  }

  async function handleAddMcpServer(name: string, command: string, authToken?: string) {
    await invoke("create_mcp_server", { name, command, authToken: authToken || null }).catch((err) => {
      appendNoticeToLastMessage(humanizeError(String(err)));
    });
    await refreshMcpServers();
  }

  async function handleToggleMcpServer(id: string, enabled: boolean) {
    await invoke("toggle_mcp_server", { serverId: id, enabled }).catch(() => {});
    await refreshMcpServers();
  }

  async function handleDeleteMcpServer(id: string) {
    await invoke("delete_mcp_server", { serverId: id }).catch(() => {});
    await refreshMcpServers();
  }

  /** 导入本地文档：抽取文本后自动作为消息发送，进入问答流程。 */
  async function handleImportFile() {
    if (sending) return;
    try {
      const selected = await open({
        multiple: false,
        filters: [{ name: "文档", extensions: ["txt", "md", "csv", "json", "log", "pdf", "docx", "xml", "html", "toml", "yaml", "yml"] }],
      });
      if (!selected || Array.isArray(selected)) return;
      const result = await invoke<{ name: string; text: string; truncated: boolean }>(
        "import_file_text",
        { path: selected }
      );
      const note = result.truncated ? "\n\n（文档过长，已截断导入前 10 万字符）" : "";
      const prepared = `请阅读以下导入文档《${result.name}》，先给出简要总结，然后准备回答我关于它的问题：${note}\n\n${result.text}`;
      setInput(prepared.slice(0, 200) + (prepared.length > 200 ? "…" : ""));
      await sendText(prepared);
    } catch (err) {
      appendNoticeToLastMessage(humanizeError(String(err)));
    }
  }

  function handleModelChange(next: DeepSeekModelId) {
    setModel(next);
    localStorage.setItem("novacode.defaultModel", next);
  }

  function handlePermissionModeChange(next: PermissionMode) {
    setPermissionMode(next);
    localStorage.setItem("novacode.defaultPermissionMode", next);
  }

  /** 输入框 @文件引用：取光标前最后一个 @ 开头的 token 作为过滤词 */
  function updateFileMention(value: string) {
    const match = /(?:^|\s)@([^\s@]*)$/.exec(value);
    setFileMention(match ? match[1] : null);
  }

  function applyFileMention(path: string) {
    setInput((prev) => prev.replace(/(?:^|\s)@([^\s@]*)$/, (m) =>
      m.startsWith(" ") ? ` @${path} ` : `@${path} `
    ));
    setFileMention(null);
  }

  function handleKeyDown(e: React.KeyboardEvent<HTMLTextAreaElement>) {
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      if (sending) {
        queueSteeringMessage();
      } else {
        handleSend();
      }
    }
  }

  /** Agent 运行中，把输入作为插话排队（下一轮注入），不打断当前任务。 */
  async function queueSteeringMessage() {
    const text = input.trim();
    if (!text || !activeConvId) return;
    setInput("");
    setQueuedSteering((prev) => [...prev, text]);
    await invoke("queue_steering", { conversationId: activeConvId, text }).catch(() => {
      setQueuedSteering((prev) => prev.filter((m) => m !== text));
    });
  }

  async function handleInstallUpdate() {
    setUpdate({ status: "downloading", progress: 0 });
    try {
      await invoke("install_update");
    } catch (err) {
      setUpdate({ status: "error", message: String(err) });
    }
  }

  const hasMessages = messages.length > 0;
  const activeConversation = conversations.find((conv) => conv.id === activeConvId);
  const conversationUsage = aggregateUsage(messages);

  // 侧边栏列表：按标题搜索过滤，归档的拆到独立折叠区（置顶排序由后端 SQL 保证）
  const query = searchQuery.trim().toLowerCase();
  const filteredConversations = conversations.filter(
    (c) => !query || c.title.toLowerCase().includes(query)
  );
  const visibleConversations = filteredConversations.filter((c) => !c.archived);
  const archivedConversations = filteredConversations.filter((c) => c.archived);

  function renderConvItem(conv: Conversation) {
    return (
      <div
        key={conv.id}
        className={`conv-item${conv.id === activeConvId ? " conv-item--active" : ""}`}
        onClick={() => handleSelectConversation(conv.id)}
        role="button"
        tabIndex={0}
        onKeyDown={(e) => e.key === "Enter" && handleSelectConversation(conv.id)}
      >
        {editingConvId === conv.id ? (
          <input
            className="conv-item__rename"
            value={editingTitle}
            autoFocus
            onChange={(e) => setEditingTitle(e.target.value)}
            onClick={(e) => e.stopPropagation()}
            onBlur={commitRename}
            onKeyDown={(e) => {
              if (e.key === "Enter") commitRename();
              if (e.key === "Escape") setEditingConvId(null);
            }}
          />
        ) : (
          <span
            className="conv-item__title"
            onDoubleClick={(e) => startRename(e, conv)}
            title="双击重命名"
          >
            {conv.pinned && <Pin size={11} className="conv-item__pin-mark" />}
            {conv.title}
          </span>
        )}
        <span className="conv-item__actions">
          <button
            className="conv-item__action"
            type="button"
            aria-label={conv.pinned ? "取消置顶" : "置顶"}
            title={conv.pinned ? "取消置顶" : "置顶"}
            onClick={(e) => handleTogglePin(e, conv)}
          >
            <Pin size={14} />
          </button>
          <button
            className="conv-item__action"
            type="button"
            aria-label={conv.archived ? "取消归档" : "归档"}
            title={conv.archived ? "取消归档" : "归档"}
            onClick={(e) => handleToggleArchive(e, conv)}
          >
            {conv.archived ? <ArchiveRestore size={14} /> : <Archive size={14} />}
          </button>
          <button
            className="conv-item__delete"
            type="button"
            aria-label="删除会话"
            title="删除"
            onClick={(e) => handleDeleteConversation(e, conv.id)}
          >
            <Trash2 size={14} />
          </button>
        </span>
      </div>
    );
  }

  // ── UI ──────────────────────────────────────────────────────────────────

  return (
    <main className="app-shell">
      {/* 侧边栏 */}
      <aside className="sidebar" aria-label="NovaCode navigation">
        <div className="brand-row">
          <BrandMark size={22} />
          <span className="brand-row__name">NovaCode</span>
        </div>
        <button className="new-chat" type="button" onClick={handleNewConversation}>
          <SquarePen size={16} /> 新对话
        </button>

        {conversations.length > 0 && (
          <nav className="conv-list" aria-label="会话列表">
            <div className="conv-search-wrap">
              <Search size={14} className="conv-search-wrap__icon" />
              <input
                className="conv-search"
                type="search"
                placeholder="搜索会话…"
                value={searchQuery}
                onChange={(e) => setSearchQuery(e.target.value)}
                aria-label="搜索会话"
              />
            </div>
            <p className="nav-label">历史对话</p>
            {visibleConversations.map(renderConvItem)}
            {archivedConversations.length > 0 && (
              <>
                <button
                  className="archived-toggle"
                  type="button"
                  onClick={() => setShowArchived((v) => !v)}
                >
                  {showArchived ? <ChevronDown size={13} /> : <ChevronRight size={13} />} 已归档（{archivedConversations.length}）
                </button>
                {showArchived && archivedConversations.map(renderConvItem)}
              </>
            )}
          </nav>
        )}

        {update.status === "available" && (
          <div className="update-banner">
            <p className="update-banner__title">发现新版本</p>
            <p className="update-banner__version">v{update.version}</p>
            <div className="update-banner__actions">
              <button className="update-banner__btn update-banner__btn--primary" type="button" onClick={handleInstallUpdate}>
                立即更新
              </button>
              <button className="update-banner__btn" type="button" onClick={() => setUpdate({ status: "idle" })}>
                稍后
              </button>
            </div>
          </div>
        )}

        {update.status === "downloading" && (
          <div className="update-banner">
            <p className="update-banner__title">正在下载更新…</p>
            <div className="update-banner__progress-bar">
              <div className="update-banner__progress-fill" style={{ width: `${update.progress}%` }} />
            </div>
            <p className="update-banner__version">{update.progress}%</p>
          </div>
        )}

        {update.status === "error" && (
          <div className="update-banner update-banner--error">
            <p className="update-banner__title">更新失败</p>
            <p className="update-banner__version">{update.message}</p>
            <button className="update-banner__btn" type="button" onClick={() => setUpdate({ status: "idle" })}>
              关闭
            </button>
          </div>
        )}

        {/* 侧边栏底部 footer：设置 + 主题切换 */}
        <div className="sidebar-footer">
          <button className="sidebar-footer__btn" type="button" onClick={openSettings}>
            <Settings2 size={16} /> 设置
          </button>
          <button
            className="sidebar-footer__icon"
            type="button"
            aria-label={theme === "dark" ? "切换到亮色" : "切换到深色"}
            title={theme === "dark" ? "切换到亮色" : "切换到深色「深海」"}
            onClick={() => setTheme((t) => (t === "dark" ? "light" : "dark"))}
          >
            {theme === "dark" ? <Sun size={16} /> : <Moon size={16} />}
          </button>
        </div>
      </aside>

      {/* 工作区 */}
      <section className="workspace" aria-label="NovaCode workspace">
        <header className="topbar">
          <div>
            <p className="eyebrow">Ignite the Spark, Unleash the Code.</p>
            <h1>NovaCode</h1>
          </div>
          <div className="status-strip" aria-label="status">
            {activeConversation?.workspaceName ? (
              <span className="chip" title={activeConversation.workspacePath ?? undefined}>
                <FolderOpen size={13} /> {activeConversation.workspaceName}
              </span>
            ) : activeConversation ? (
              <span className="chip" title="本会话未绑定工作区，Agent 无法执行本地操作；新建对话时可选择工作区">
                纯聊天
              </span>
            ) : null}
            {ctxUsage && ctxUsage.softLimit > 0 && (
              <span
                className="chip chip--accent"
                title={`上次请求 ${ctxUsage.promptTokens.toLocaleString()} tokens / 自动压缩阈值 ${ctxUsage.softLimit.toLocaleString()}`}
              >
                <Gauge size={13} /> {Math.round((ctxUsage.promptTokens / ctxUsage.softLimit) * 100)}%
              </span>
            )}
            {activeConvId && (
              <button
                className="topbar-btn"
                type="button"
                title="文件变更记录（可回退）"
                onClick={openChangesPanel}
              >
                <GitCompare size={14} /> 变更
              </button>
            )}
          </div>
        </header>

        {hasMessages ? (
          <section className="message-list" aria-label="Conversation">
            {messages.map((msg, i) => (
              <div key={i} className="message-row">
                <div className={`message message--${msg.role}`}>
                  <MessageContent msg={msg} />
                </div>
                {msg.role === "assistant" && msg.usage && (
                  <UsageBadge usage={msg.usage} />
                )}
              </div>
            ))}
            {sending && (
              <div className="agent-working" aria-label="Agent 工作状态">
                <span className="star-spin" aria-hidden="true">✦</span>
                <span>{agentStatus ?? "正在思考…"}</span>
                <span className="agent-working__elapsed">{elapsedSec}s</span>
              </div>
            )}
            <div ref={messagesEndRef} />
          </section>
        ) : (
          <section className="hero-panel" aria-label="New conversation">
            <h2>我们应该在 NovaCode 中做些什么？</h2>
            <section className="workspace-picker" aria-label="New conversation workspace">
              <button type="button" onClick={handleSelectWorkspace}>
                选择工作区
              </button>
              {draftWorkspace ? (
                <div className="workspace-picker__selected">
                  <strong>{draftWorkspace.name}</strong>
                  <span title={draftWorkspace.path}>{draftWorkspace.path}</span>
                </div>
              ) : (
                <p>未绑定工作区，仅聊天模式</p>
              )}
              {workspaceError && <p className="workspace-picker__error">{workspaceError}</p>}
            </section>
            <section className="mvp-grid" aria-label="MVP status cards">
              <article>
                <h3>DeepSeek 连接</h3>
                <p>只从环境变量读取 API Key，不在应用内保存。</p>
              </article>
              <article>
                <h3>Token 账本</h3>
                <p>记录请求级 usage、缓存命中与估算费用。</p>
              </article>
              <article>
                <h3>权限模式</h3>
                <p>默认受限，高风险动作进入审批与审计。</p>
              </article>
            </section>
          </section>
        )}

        {conversationUsage && (
          <ConversationUsageSummary usage={conversationUsage} />
        )}

        {todos.length > 0 && <TodoPanel items={todos} />}

        {queuedSteering.length > 0 && (
          <div className="steering-queue" aria-label="排队的插话">
            {queuedSteering.map((m, i) => (
              <span key={i} className="steering-chip" title={m}>
                <CornerDownRight size={12} /> {m.length > 30 ? m.slice(0, 30) + "…" : m}
              </span>
            ))}
            <span className="steering-queue__hint">将在下一轮注入</span>
          </div>
        )}

        <div className="composer-area">
          {/* 斜杠命令菜单 */}
          {input.startsWith("/") && !input.includes(" ") && !sending && (
            <div className="slash-menu" role="menu">
              {[...SLASH_COMMANDS, ...customCommands.map((c) => ({ cmd: c.name, desc: c.description || "自定义命令" }))]
                .filter((c) => c.cmd.startsWith(input))
                .map((c) => (
                  <button
                    key={c.cmd}
                    className="slash-menu__item"
                    type="button"
                    onClick={() => setInput(c.cmd === "/model" ? "/model " : c.cmd)}
                  >
                    <span className="slash-menu__cmd">{c.cmd}</span>
                    <span className="slash-menu__desc">{c.desc}</span>
                  </button>
                ))}
            </div>
          )}

          {/* @文件引用补全 */}
          {fileMention !== null && workspaceFiles.length > 0 && (
            <div className="slash-menu" role="menu">
              {workspaceFiles
                .filter((f) => f.toLowerCase().includes(fileMention.toLowerCase()))
                .slice(0, 8)
                .map((f) => (
                  <button
                    key={f}
                    className="slash-menu__item"
                    type="button"
                    onClick={() => applyFileMention(f)}
                  >
                    <AtSign size={14} className="slash-menu__icon" />
                    <span className="slash-menu__cmd">{f}</span>
                  </button>
                ))}
            </div>
          )}

          {/* 控制行：权限模式 + 计划（左）/ 模型（右）。可随时切换，改动在当前回复结束后的下一轮生效。 */}
          <div className="composer-controls">
            <div className="composer-controls__left">
              <select
                className="control-select"
                value={permissionMode}
                onChange={(e) => handlePermissionModeChange(e.target.value as PermissionMode)}
                aria-label="权限模式"
                title={sending ? "切换将在当前回复结束后的下一轮生效" : "权限模式"}
              >
                {PERMISSION_MODES.map((mode) => (
                  <option key={mode} value={mode}>{getPermissionModeLabel(mode)}</option>
                ))}
              </select>
              <button
                type="button"
                className={`control-toggle${planMode ? " control-toggle--on" : ""}`}
                title="计划模式：让 Agent 先给出分步计划，确认后再执行"
                onClick={() => setPlanMode((v) => !v)}
              >
                <ListChecks size={14} /> 计划
              </button>
            </div>
            <select
              className="control-select"
              value={model}
              onChange={(e) => handleModelChange(e.target.value as DeepSeekModelId)}
              aria-label="模型选择"
              title={sending ? "切换将在当前回复结束后的下一轮生效" : DEEPSEEK_MODELS.find((item) => item.id === model)?.description}
            >
              {DEEPSEEK_MODELS.map((item) => (
                <option key={item.id} value={item.id}>{item.label}</option>
              ))}
            </select>
          </div>

          <div className="composer">
            <button
              type="button"
              className="composer__attach"
              title="导入本地文档（txt/md/csv/pdf/docx），总结并问答"
              aria-label="导入文档"
              onClick={handleImportFile}
              disabled={sending}
            >
              <Paperclip size={18} />
            </button>
            <textarea
              aria-label="Message"
              placeholder={sending ? "Agent 运行中：输入并回车可插话，下一轮生效（不打断当前任务）" : planMode ? "计划模式：先出计划，确认后再执行（Enter 发送）" : "随心输入（Enter 发送，Shift+Enter 换行，/ 命令，@ 引用文件）"}
              value={input}
              onChange={(e) => {
                setInput(e.target.value);
                updateFileMention(e.target.value);
              }}
              onKeyDown={handleKeyDown}
            />
            {sending ? (
              <button type="button" className="composer__send composer__send--stop" onClick={handleStop} aria-label="停止">
                <Square size={14} fill="currentColor" />
              </button>
            ) : (
              <button type="button" className="composer__send" onClick={handleSend} disabled={!input.trim()} aria-label="发送">
                <ArrowUp size={18} />
              </button>
            )}
          </div>
        </div>
      </section>

      {approval && (
        <ApprovalModal
          approval={approval}
          onAllow={() => respondApproval(true)}
          onAlwaysAllow={() => respondApproval(true, true)}
          onDeny={() => respondApproval(false)}
        />
      )}

      {askUser && (
        <AskUserModal
          request={askUser}
          onSubmit={respondAskUser}
          onCancel={() => respondAskUser("")}
        />
      )}

      {showChanges && (
        <ChangesModal
          checkpoints={checkpoints}
          onRevert={handleRevert}
          onClose={() => setShowChanges(false)}
        />
      )}

      {showSettings && (
        <SettingsModal
          appInfo={appInfo}
          apiKeyLabel={getApiKeyStatusLabel(apiKeyStatus)}
          balance={balance}
          onRefreshBalance={refreshBalance}
          model={model}
          permissionMode={permissionMode}
          mcpServers={mcpServers}
          permRules={permRules}
          commandSandbox={commandSandbox}
          onToggleSandbox={handleToggleSandbox}
          taskBudget={taskBudget}
          onSetBudget={handleSetBudget}
          hasActiveConv={!!activeConvId}
          onExportConversation={handleExportConversation}
          onExportLedger={handleExportLedger}
          onClearData={handleClearData}
          onModelChange={handleModelChange}
          onPermissionModeChange={handlePermissionModeChange}
          onAddMcpServer={handleAddMcpServer}
          onToggleMcpServer={handleToggleMcpServer}
          onDeleteMcpServer={handleDeleteMcpServer}
          onRefreshMcp={refreshMcpServers}
          onAddPermRule={handleAddPermRule}
          onDeletePermRule={handleDeletePermRule}
          onUpdateAvailable={(v) => setUpdate({ status: "available", version: v })}
          onClose={() => setShowSettings(false)}
        />
      )}
    </main>
  );
}

// ── TodoPanel ───────────────────────────────────────────────────────────────

function TodoPanel({ items }: { items: TodoItem[] }) {
  // Agent 自维护任务清单：常驻输入框上方，实时反映多步任务进度。
  const done = items.filter((t) => t.status === "done").length;
  return (
    <div className="todo-panel" aria-label="任务清单">
      <span className="todo-panel__head">任务 {done}/{items.length}</span>
      <div className="todo-panel__items">
        {items.map((t, i) => (
          <span key={i} className={`todo-item todo-item--${t.status}`}>
            {t.status === "done" ? <CheckSquare size={13} /> : t.status === "in_progress" ? <CircleDot size={13} /> : <Square size={13} />} {t.text}
          </span>
        ))}
      </div>
    </div>
  );
}

// ── ChangesModal ────────────────────────────────────────────────────────────

function ChangesModal({
  checkpoints,
  onRevert,
  onClose,
}: {
  checkpoints: FileCheckpoint[];
  onRevert: (id: string) => void;
  onClose: () => void;
}) {
  return (
    <div className="approval-overlay" role="dialog" aria-modal="true" aria-label="文件变更记录">
      <div className="approval-card panel-card">
        <p className="approval-card__title">文件变更记录</p>
        {checkpoints.length === 0 ? (
          <p className="approval-card__hint">本会话还没有文件变更。</p>
        ) : (
          <div className="changes-list">
            {checkpoints.map((c) => (
              <div key={c.id} className={`changes-row${c.reverted ? " changes-row--reverted" : ""}`}>
                <span className="changes-row__tool">{c.toolName}</span>
                <span className="changes-row__path" title={c.relPath}>{c.relPath}</span>
                {c.reverted ? (
                  <span className="changes-row__state">已回退</span>
                ) : c.revertible ? (
                  <button
                    className="changes-row__revert"
                    type="button"
                    title="回退此变更及其后的所有变更"
                    onClick={() => onRevert(c.id)}
                  >
                    回退到此前
                  </button>
                ) : (
                  <span className="changes-row__state">不可回退</span>
                )}
              </div>
            ))}
          </div>
        )}
        <div className="approval-card__actions">
          <button type="button" className="approval-card__btn" onClick={onClose}>
            关闭
          </button>
        </div>
      </div>
    </div>
  );
}

// ── SettingsModal ───────────────────────────────────────────────────────────

function SettingsModal({
  appInfo,
  apiKeyLabel,
  balance,
  onRefreshBalance,
  model,
  permissionMode,
  mcpServers,
  permRules,
  commandSandbox,
  onToggleSandbox,
  taskBudget,
  onSetBudget,
  hasActiveConv,
  onExportConversation,
  onExportLedger,
  onClearData,
  onModelChange,
  onPermissionModeChange,
  onAddMcpServer,
  onToggleMcpServer,
  onDeleteMcpServer,
  onRefreshMcp,
  onAddPermRule,
  onDeletePermRule,
  onUpdateAvailable,
  onClose,
}: {
  appInfo: AppInfo | null;
  apiKeyLabel: string;
  balance: BalanceState;
  onRefreshBalance: () => void;
  model: DeepSeekModelId;
  permissionMode: PermissionMode;
  mcpServers: McpServer[];
  permRules: string[];
  commandSandbox: boolean;
  onToggleSandbox: (enabled: boolean) => void;
  taskBudget: number;
  onSetBudget: (budget: number) => void;
  hasActiveConv: boolean;
  onExportConversation: () => void;
  onExportLedger: () => void;
  onClearData: () => void;
  onModelChange: (m: DeepSeekModelId) => void;
  onPermissionModeChange: (m: PermissionMode) => void;
  onAddMcpServer: (name: string, command: string, authToken?: string) => void;
  onToggleMcpServer: (id: string, enabled: boolean) => void;
  onDeleteMcpServer: (id: string) => void;
  onRefreshMcp: () => void;
  onAddPermRule: (rule: string) => void;
  onDeletePermRule: (rule: string) => void;
  onUpdateAvailable: (version: string) => void;
  onClose: () => void;
}) {
  const [mcpName, setMcpName] = useState("");
  const [mcpCommand, setMcpCommand] = useState("");
  const [mcpToken, setMcpToken] = useState("");
  const [ruleInput, setRuleInput] = useState("");
  // 检查更新按钮自管理：idle → checking → result(10s) → idle，期间禁用、尺寸不变。
  const [checkLabel, setCheckLabel] = useState("检查更新");
  const [checkBusy, setCheckBusy] = useState(false);
  const checkTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  useEffect(() => () => { if (checkTimerRef.current) clearTimeout(checkTimerRef.current); }, []);

  async function handleCheckUpdateBtn() {
    if (checkBusy) return;
    setCheckBusy(true);
    setCheckLabel("检查中…");
    let label = "已是最新版本";
    try {
      const v = await invoke<string | null>("check_update");
      if (v) {
        label = `发现新版本 v${v}`;
        onUpdateAvailable(v);
      }
    } catch {
      label = "检查失败，请稍后重试";
    }
    setCheckLabel(label);
    checkTimerRef.current = setTimeout(() => {
      setCheckLabel("检查更新");
      setCheckBusy(false);
    }, 10000);
  }
  const [section, setSection] = useState<"account" | "model" | "rules" | "mcp" | "data" | "about">("account");
  const [budgetInput, setBudgetInput] = useState(String(taskBudget));

  const NAV: Array<{ id: typeof section; label: string }> = [
    { id: "account", label: "账户" },
    { id: "model", label: "模型与权限" },
    { id: "rules", label: "权限规则" },
    { id: "mcp", label: "MCP 服务器" },
    { id: "data", label: "数据" },
    { id: "about", label: "关于" },
  ];

  return (
    <div className="approval-overlay" role="dialog" aria-modal="true" aria-label="设置">
      <div className="approval-card panel-card settings-modal">
        <nav className="settings-nav" aria-label="设置分类">
          <p className="settings-nav__title">设置</p>
          {NAV.map((n) => (
            <button
              key={n.id}
              type="button"
              className={`settings-nav__item${section === n.id ? " settings-nav__item--active" : ""}`}
              onClick={() => setSection(n.id)}
            >
              {n.label}
            </button>
          ))}
          <button type="button" className="settings-nav__close" onClick={onClose}>
            关闭
          </button>
        </nav>

        <div className="settings-content">
          {section === "account" && (
            <>
              <h3 className="settings-content__h">账户</h3>
              <div className="settings-row">
                <span className="settings-row__label">DeepSeek API Key</span>
                <span className="settings-row__value">{apiKeyLabel}</span>
              </div>
              <p className="settings-desc">API Key 仅从系统环境变量 <code>DEEPSEEK_API_KEY</code> 读取，不在应用内保存。</p>

              <div className="settings-section__head" style={{ marginTop: 16 }}>
                <span>账户余额</span>
                <button
                  className="changes-row__revert"
                  type="button"
                  onClick={onRefreshBalance}
                  disabled={balance.status === "loading"}
                >
                  {balance.status === "loading" ? "查询中…" : "刷新"}
                </button>
              </div>
              {balance.status === "error" && (
                <p className="settings-row__value" style={{ color: "var(--danger)" }}>{balance.message}</p>
              )}
              {balance.status === "ok" && (
                <>
                  <div className="settings-row">
                    <span className="settings-row__label">状态</span>
                    <span className="settings-row__value" style={{ color: balance.data.isAvailable ? "var(--success)" : "var(--danger)" }}>
                      {balance.data.isAvailable ? "余额充足，可正常调用" : "余额不足"}
                    </span>
                  </div>
                  {balance.data.balanceInfos.length === 0 && <p className="settings-row__value">未返回余额明细</p>}
                  {balance.data.balanceInfos.map((b) => (
                    <div key={b.currency} className="balance-card">
                      <div className="balance-card__total">
                        <span className="balance-card__amount">{b.totalBalance}</span>
                        <span className="balance-card__currency">{b.currency}</span>
                      </div>
                      <div className="balance-card__detail">
                        <span>充值 {b.toppedUpBalance}</span>
                        <span>赠送 {b.grantedBalance}</span>
                      </div>
                    </div>
                  ))}
                </>
              )}
            </>
          )}

          {section === "model" && (
            <>
              <h3 className="settings-content__h">模型与权限</h3>
              <div className="settings-row">
                <span className="settings-row__label">默认模型</span>
                <select className="model-select" value={model} onChange={(e) => onModelChange(e.target.value as DeepSeekModelId)}>
                  {DEEPSEEK_MODELS.map((item) => (
                    <option key={item.id} value={item.id}>{item.label}</option>
                  ))}
                </select>
              </div>
              <p className="settings-desc">新会话的默认模型。会话中也可在输入框上方临时切换，改动在下一轮生效。</p>

              <div className="settings-row" style={{ marginTop: 12 }}>
                <span className="settings-row__label">默认权限模式</span>
                <select className="model-select" value={permissionMode} onChange={(e) => onPermissionModeChange(e.target.value as PermissionMode)}>
                  {PERMISSION_MODES.map((mode) => (
                    <option key={mode} value={mode}>{getPermissionModeLabel(mode)}</option>
                  ))}
                </select>
              </div>
              <ul className="settings-desc settings-desc--list">
                <li><b>受限</b>：只允许聊天与只读，不能改文件或执行命令。</li>
                <li><b>每次询问</b>：每个写入/命令/越界动作都弹窗确认。</li>
                <li><b>工作区自动</b>：工作区内读写自动执行，低风险命令直接跑，其余审批。</li>
                <li><b>完全访问</b>：放开执行，仍保留审计与回退。</li>
              </ul>

              <div className="settings-row" style={{ marginTop: 16 }}>
                <span className="settings-row__label">命令沙箱</span>
                <label className="toggle">
                  <input type="checkbox" checked={commandSandbox} onChange={(e) => onToggleSandbox(e.target.checked)} />
                  <span>{commandSandbox ? "已开启" : "已关闭"}</span>
                </label>
              </div>
              <p className="settings-desc">
                开启后，run_command 在<b>受限令牌沙箱</b>中执行：剥离管理员特权、进程随会话干净销毁、子进程环境擦除 API Key 等密钥。
                少数需要特权的命令可能受影响，可临时关闭。（网络与文件路径隔离将在后续 AppContainer 版本提供。）
              </p>

              <div className="settings-row" style={{ marginTop: 16 }}>
                <span className="settings-row__label">单任务 token 预算</span>
                <span style={{ display: "flex", gap: 6 }}>
                  <input
                    className="conv-search"
                    style={{ width: 120, marginBottom: 0 }}
                    type="number"
                    min={0}
                    value={budgetInput}
                    onChange={(e) => setBudgetInput(e.target.value)}
                    onBlur={() => onSetBudget(Math.max(0, parseInt(budgetInput) || 0))}
                  />
                </span>
              </div>
              <p className="settings-desc">单次任务累计 token 超过此值时自动暂停，防止失控烧 token（0 = 不限）。当前 {taskBudget === 0 ? "不限" : taskBudget.toLocaleString()}。</p>
            </>
          )}

          {section === "data" && (
            <>
              <h3 className="settings-content__h">数据</h3>
              <p className="settings-desc">所有数据保存在本地，可随时导出、备份或删除。</p>
              <div style={{ display: "flex", flexDirection: "column", gap: 8, marginTop: 12, maxWidth: 320 }}>
                <button type="button" className="approval-card__btn" onClick={onExportConversation} disabled={!hasActiveConv}>导出当前会话（Markdown）</button>
                <button type="button" className="approval-card__btn" onClick={onExportLedger}>导出 Token 账本（CSV）</button>
                <button type="button" className="approval-card__btn" onClick={onClearData} style={{ color: "var(--danger)" }}>清除所有会话</button>
              </div>
              <p className="settings-desc">Token 账本 CSV 可与 DeepSeek 官方账单对照核对消费。</p>
            </>
          )}

          {section === "rules" && (
            <>
              <h3 className="settings-content__h">权限规则</h3>
              <p className="settings-desc">
                细粒度规则按 <b>deny 优先</b> 生效。格式：<code>[allow:|deny:]&lt;工具&gt;:&lt;路径glob&gt;</code>，
                或 <code>cmd:&lt;命令前缀&gt;</code>、<code>tool:&lt;工具名&gt;</code>。
                例：<code>deny:read_file:**/.env</code>、<code>allow:cmd:git push</code>。审批弹窗的「总是允许」会自动生成 allow 规则。
              </p>
              {permRules.length === 0 && <p className="settings-row__value">暂无规则。</p>}
              {permRules.map((r) => (
                <div key={r} className="changes-row">
                  <span className={`mcp-dot${r.startsWith("deny:") ? "" : " mcp-dot--on"}`} aria-hidden="true">●</span>
                  <span className="changes-row__path" title={r} style={{ fontFamily: "monospace" }}>{r}</span>
                  <button className="changes-row__revert" type="button" onClick={() => onDeletePermRule(r)}>删除</button>
                </div>
              ))}
              <div className="mcp-add">
                <input className="conv-search" placeholder="如 deny:read_file:**/.env 或 allow:cmd:git push" value={ruleInput} onChange={(e) => setRuleInput(e.target.value)} />
                <button className="approval-card__btn" type="button" disabled={!ruleInput.trim()} onClick={() => { onAddPermRule(ruleInput.trim()); setRuleInput(""); }}>添加规则</button>
              </div>
            </>
          )}

          {section === "mcp" && (
            <>
              <div className="settings-content__h" style={{ display: "flex", justifyContent: "space-between", alignItems: "center" }}>
                <span><Plug size={15} style={{ verticalAlign: "-2px" }} /> MCP 服务器</span>
                <button className="changes-row__revert" type="button" onClick={onRefreshMcp}>刷新状态</button>
              </div>
              <p className="settings-desc">接入外部 MCP 服务器，其工具会并入模型工具集，统一经权限审批与审计。<b>stdio</b>：填启动命令（需 Node/npx 等）；<b>HTTP</b>：填 http(s):// 地址，可选填 Token；留空且服务端要求授权时自动走浏览器 OAuth。</p>
              {mcpServers.map((s) => (
                <div key={s.id} className="changes-row">
                  <span className={`mcp-dot${s.connected ? " mcp-dot--on" : ""}`} aria-hidden="true">●</span>
                  <span className="changes-row__tool">{s.name}</span>
                  <span className="changes-row__path" title={s.command}>
                    {s.connected ? `${s.toolCount} 个工具` : s.enabled ? "未连接" : "已停用"}
                  </span>
                  <button className="changes-row__revert" type="button" onClick={() => onToggleMcpServer(s.id, !s.enabled)}>{s.enabled ? "停用" : "启用"}</button>
                  <button className="changes-row__revert" type="button" onClick={() => onDeleteMcpServer(s.id)}>删除</button>
                </div>
              ))}
              <div className="mcp-add">
                <input className="conv-search" placeholder="名称（如 filesystem / github）" value={mcpName} onChange={(e) => setMcpName(e.target.value)} />
                <input className="conv-search" placeholder="启动命令 或 http(s):// 地址" value={mcpCommand} onChange={(e) => setMcpCommand(e.target.value)} />
                <input className="conv-search" placeholder="Bearer Token（仅 HTTP，可选；留空走 OAuth）" value={mcpToken} onChange={(e) => setMcpToken(e.target.value)} />
                <button className="approval-card__btn" type="button" disabled={!mcpName.trim() || !mcpCommand.trim()} onClick={() => { onAddMcpServer(mcpName.trim(), mcpCommand.trim(), mcpToken.trim() || undefined); setMcpName(""); setMcpCommand(""); setMcpToken(""); }}>添加并连接</button>
              </div>
            </>
          )}

          {section === "about" && (
            <>
              <h3 className="settings-content__h">关于</h3>
              <div className="settings-row">
                <span className="settings-row__label">版本</span>
                <span className="settings-row__value">v{appInfo?.version ?? "…"}</span>
              </div>
              <div className="settings-row">
                <span className="settings-row__label">数据目录</span>
                <span className="settings-row__value" title={appInfo?.dataDir}>{appInfo?.dataDir ?? "…"}</span>
              </div>
              <p className="settings-desc">会话、token 账本、权限规则等本地数据保存在数据目录的 SQLite 中。</p>
              <button
                type="button"
                className="approval-card__btn check-update-btn"
                style={{ marginTop: 12 }}
                onClick={handleCheckUpdateBtn}
                disabled={checkBusy}
              >
                {checkLabel}
              </button>
              <p className="settings-desc">发现新版本时，可在左下角横幅一键安装更新。</p>
            </>
          )}
        </div>
      </div>
    </div>
  );
}

// ── ApprovalModal ───────────────────────────────────────────────────────────

function ApprovalModal({
  approval,
  onAllow,
  onAlwaysAllow,
  onDeny,
}: {
  approval: ApprovalRequest;
  onAllow: () => void;
  onAlwaysAllow: () => void;
  onDeny: () => void;
}) {
  return (
    <div className="approval-overlay" role="dialog" aria-modal="true" aria-label="高风险动作审批">
      <div className="approval-card">
        <p className="approval-card__title">Agent 请求执行高风险动作</p>
        <div className="approval-card__detail">
          <span className="approval-card__tool">{approval.toolName}</span>
          {approval.target && (
            <span className="approval-card__target">{approval.target}</span>
          )}
        </div>
        <p className="approval-card__hint">是否允许本次操作？「总是允许」会记住同类动作，后续免审批。</p>
        <div className="approval-card__actions">
          <button type="button" className="approval-card__btn approval-card__btn--allow" onClick={onAllow}>
            允许一次
          </button>
          <button type="button" className="approval-card__btn" onClick={onAlwaysAllow}>
            总是允许
          </button>
          <button type="button" className="approval-card__btn" onClick={onDeny}>
            拒绝
          </button>
        </div>
      </div>
    </div>
  );
}

// ── AskUserModal ──────────────────────────────────────────────────────────────

/** Agent 发起的结构化澄清提问：每题渲染可点选项卡片，自动附「其他」自定义输入。 */
function AskUserModal({
  request,
  onSubmit,
  onCancel,
}: {
  request: AskUserRequest;
  onSubmit: (answer: string) => void;
  onCancel: () => void;
}) {
  const questions = request.questions;
  // 每题已选 label 集合；"__other__" 代表选中了自定义项。
  const [selected, setSelected] = useState<Record<number, Set<string>>>(() =>
    Object.fromEntries(questions.map((_, i) => [i, new Set<string>()])),
  );
  const [otherText, setOtherText] = useState<Record<number, string>>({});

  function toggle(qIndex: number, label: string, multi: boolean) {
    setSelected((prev) => {
      const next = { ...prev };
      const set = new Set(next[qIndex]);
      if (multi) {
        if (set.has(label)) set.delete(label);
        else set.add(label);
      } else {
        if (set.has(label)) set.clear();
        else {
          set.clear();
          set.add(label);
        }
      }
      next[qIndex] = set;
      return next;
    });
  }

  // 每题至少有一项选择（普通选项，或选了「其他」且填了文本）才能提交。
  const ready = questions.every((_, i) => {
    const set = selected[i] ?? new Set<string>();
    const hasOption = Array.from(set).some((s) => s !== "__other__");
    const hasOther = set.has("__other__") && (otherText[i] ?? "").trim().length > 0;
    return hasOption || hasOther;
  });

  function submit() {
    const answer = questions.map((q, i) => {
      const set = selected[i] ?? new Set<string>();
      const picks = Array.from(set).filter((s) => s !== "__other__");
      if (set.has("__other__") && (otherText[i] ?? "").trim()) {
        picks.push(otherText[i].trim());
      }
      return { question: q.question, header: q.header ?? "", selected: picks };
    });
    onSubmit(JSON.stringify(answer));
  }

  return (
    <div className="approval-overlay" role="dialog" aria-modal="true" aria-label="Agent 提问">
      <div className="approval-card askuser-card">
        <p className="approval-card__title">Agent 需要你确认</p>
        <div className="askuser-questions">
          {questions.map((q, i) => {
            const multi = q.multiSelect === true;
            const set = selected[i] ?? new Set<string>();
            return (
              <div className="askuser-question" key={i}>
                <div className="askuser-question__head">
                  {q.header && <span className="askuser-chip">{q.header}</span>}
                  <span className="askuser-question__text">{q.question}</span>
                </div>
                <div className="askuser-options">
                  {q.options.map((opt, j) => (
                    <button
                      type="button"
                      key={j}
                      className={`askuser-option${set.has(opt.label) ? " askuser-option--on" : ""}`}
                      onClick={() => toggle(i, opt.label, multi)}
                    >
                      <span className="askuser-option__label">{opt.label}</span>
                      {opt.description && (
                        <span className="askuser-option__desc">{opt.description}</span>
                      )}
                    </button>
                  ))}
                  <button
                    type="button"
                    className={`askuser-option${set.has("__other__") ? " askuser-option--on" : ""}`}
                    onClick={() => toggle(i, "__other__", multi)}
                  >
                    <span className="askuser-option__label">其他…</span>
                    <span className="askuser-option__desc">自定义回答</span>
                  </button>
                  {set.has("__other__") && (
                    <input
                      className="askuser-other-input"
                      type="text"
                      placeholder="输入你的回答"
                      value={otherText[i] ?? ""}
                      onChange={(e) =>
                        setOtherText((prev) => ({ ...prev, [i]: e.target.value }))
                      }
                      autoFocus
                    />
                  )}
                </div>
                {multi && <p className="askuser-multi-hint">可多选</p>}
              </div>
            );
          })}
        </div>
        <div className="approval-card__actions">
          <button
            type="button"
            className="approval-card__btn approval-card__btn--allow"
            disabled={!ready}
            onClick={submit}
          >
            提交
          </button>
          <button type="button" className="approval-card__btn" onClick={onCancel}>
            取消
          </button>
        </div>
      </div>
    </div>
  );
}

// ── MessageContent ──────────────────────────────────────────────────────────

/** 渲染块：单个非工具 part，或一段连续的工具调用（聚合为可折叠组） */
type RenderBlock =
  | { kind: "part"; part: TextPart | NoticePart; index: number }
  | { kind: "tools"; parts: ToolPart[]; index: number };

function MessageContent({ msg }: { msg: Message }) {
  // 连续的工具卡片聚合成一个可折叠组；叙述文字与通知保持原位，时间轴不变。
  const blocks: RenderBlock[] = [];
  msg.parts.forEach((part, i) => {
    if (part.type === "tool") {
      const tail = blocks[blocks.length - 1];
      if (tail && tail.kind === "tools") {
        tail.parts.push(part);
      } else {
        blocks.push({ kind: "tools", parts: [part], index: i });
      }
    } else {
      blocks.push({ kind: "part", part, index: i });
    }
  });

  // ChangeSet 汇总：本条消息内所有带 diff 的文件变更聚合为一行摘要。
  const changedFiles = new Set<string>();
  let totalAdded = 0;
  let totalRemoved = 0;
  msg.parts.forEach((part) => {
    if (part.type === "tool" && (part.added !== undefined || part.removed !== undefined)) {
      changedFiles.add(part.target);
      totalAdded += part.added ?? 0;
      totalRemoved += part.removed ?? 0;
    }
  });

  return (
    <>
      {blocks.map((block) => {
        if (block.kind === "tools") {
          return <ToolGroup key={`t${block.index}`} parts={block.parts} />;
        }
        const { part, index } = block;
        if (part.type === "text") {
          return msg.role === "user" ? (
            <p key={index}>{part.content}</p>
          ) : (
            <ReactMarkdown
              key={index}
              remarkPlugins={[remarkGfm]}
              rehypePlugins={[rehypeHighlight]}
              components={{ pre: CodeBlock }}
            >
              {part.content}
            </ReactMarkdown>
          );
        }
        return (
          <div key={index} className="notice-inline" aria-label="系统通知">
            <Info size={13} /> {part.text}
          </div>
        );
      })}
      {changedFiles.size > 0 && (
        <div className="changeset-summary" aria-label="变更汇总">
          本轮变更 {changedFiles.size} 个文件
          <span className="diff-added"> +{totalAdded}</span>
          <span className="diff-removed"> −{totalRemoved}</span>
        </div>
      )}
    </>
  );
}

// ── ToolGroup ───────────────────────────────────────────────────────────────

function ToolGroup({ parts }: { parts: ToolPart[] }) {
  // 连续工具调用的折叠组：执行中实时显示运行行，全部完成后默认折叠为一行摘要，
  // 点击可展开查看每一步（对标 CC/Codex 的工具过程折叠）。
  const [expanded, setExpanded] = useState(false);
  const running = parts.filter((p) => p.status === "running");
  const failed = parts.filter((p) => p.status === "failed").length;
  const denied = parts.filter((p) => p.status === "denied").length;

  // 只有一条时不加折叠壳，直接显示
  if (parts.length === 1) {
    return <ToolInlineRow part={parts[0]} />;
  }

  const visibleRows = expanded ? parts : running;

  return (
    <div className="tool-group">
      <button
        className="tool-group__summary"
        type="button"
        onClick={() => setExpanded((v) => !v)}
      >
        <span className="tool-group__caret">{expanded ? <ChevronDown size={13} /> : <ChevronRight size={13} />}</span>
        已执行 {parts.length} 个工具动作
        {failed > 0 && <span className="tool-group__failed"> · {failed} 失败</span>}
        {denied > 0 && <span className="tool-group__failed"> · {denied} 被拒</span>}
        {running.length > 0 && <span className="tool-group__running"> · 进行中…</span>}
      </button>
      {visibleRows.length > 0 && (
        <div className="tool-group__rows">
          {visibleRows.map((p, i) => (
            <ToolInlineRow key={i} part={p} />
          ))}
        </div>
      )}
    </div>
  );
}

// ── CodeBlock ───────────────────────────────────────────────────────────────

function CodeBlock(props: React.HTMLAttributes<HTMLPreElement>) {
  // 代码块外壳：右上角悬浮复制按钮，点击复制整块代码文本。
  const preRef = useRef<HTMLPreElement>(null);
  const [copied, setCopied] = useState(false);

  async function handleCopy() {
    const text = preRef.current?.innerText ?? "";
    try {
      await navigator.clipboard.writeText(text);
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    } catch {
      // 剪贴板不可用时静默失败
    }
  }

  return (
    <div className="code-block">
      <button className="code-block__copy" type="button" onClick={handleCopy}>
        {copied ? <><Check size={12} /> 已复制</> : "复制"}
      </button>
      <pre ref={preRef} {...props} />
    </div>
  );
}

// ── ToolInlineRow ───────────────────────────────────────────────────────────

function ToolInlineRow({ part }: { part: ToolPart }) {
  const { toolName, target, status, error, diff, added, removed, liveOutput } = part;
  const [showDiff, setShowDiff] = useState(false);
  const icon =
    status === "running" ? <span className="star-spin tool-inline__star" aria-hidden="true">✦</span> :
    status === "succeeded" ? <Check size={13} /> :
    status === "denied" ? <Ban size={13} /> : <X size={13} />;
  const hasDiff = typeof diff === "string" && diff.trim().length > 0;
  return (
    <div className="tool-inline-wrap">
      <div
        className={`tool-inline tool-inline--${status}${hasDiff ? " tool-inline--clickable" : ""}`}
        aria-label={`${toolName} ${status}`}
        onClick={hasDiff ? () => setShowDiff((v) => !v) : undefined}
        role={hasDiff ? "button" : undefined}
      >
        <span className="tool-inline__icon" aria-hidden="true">{icon}</span>
        <span className="tool-inline__name">{toolName}</span>
        {target && <span className="tool-inline__target">{target}</span>}
        {(added !== undefined || removed !== undefined) && (
          <span className="tool-inline__stats">
            {added ? <span className="diff-added">+{added}</span> : null}
            {removed ? <span className="diff-removed">−{removed}</span> : null}
          </span>
        )}
        {hasDiff && (
          <span className="tool-inline__expand">
            {showDiff ? <ChevronDown size={13} /> : <ChevronRight size={13} />} diff
          </span>
        )}
        {status === "running" && <span className="tool-inline__dots" aria-hidden="true">…</span>}
        {status === "denied" && <span className="tool-inline__error">{error ?? "已拒绝"}</span>}
        {status === "failed" && error && (
          <span className="tool-inline__error">{error}</span>
        )}
      </div>
      {status === "running" && liveOutput && (
        <pre className="tool-live-output">{liveOutput}</pre>
      )}
      {showDiff && hasDiff && <DiffBlock diff={diff!} />}
    </div>
  );
}

// ── DiffBlock ───────────────────────────────────────────────────────────────

function DiffBlock({ diff }: { diff: string }) {
  // 按行渲染 unified diff：+ 绿、- 红、@@ 弱化。
  return (
    <pre className="diff-block">
      {diff.split("\n").map((line, i) => {
        const cls = line.startsWith("+")
          ? "diff-line--add"
          : line.startsWith("-")
            ? "diff-line--del"
            : line.startsWith("@@")
              ? "diff-line--hunk"
              : "";
        return (
          <span key={i} className={`diff-line ${cls}`}>
            {line}
            {"\n"}
          </span>
        );
      })}
    </pre>
  );
}

// ── UsageBadge ────────────────────────────────────────────────────────────

function UsageBadge({ usage }: { usage: UsageSummary }) {
  const costStr = formatUsd(usage.estimatedCostUsd);
  const isEstimate = usage.usageSource !== "deepseek_usage";

  return (
    <div className="usage-badge" aria-label="Token usage">
      <span>{usage.totalTokens.toLocaleString()} tokens</span>
      <span className="usage-sep">·</span>
      <span>{usage.promptTokens.toLocaleString()} in</span>
      <span className="usage-sep">/</span>
      <span>{usage.completionTokens.toLocaleString()} out</span>
      {usage.cacheHitTokens > 0 && (
        <>
          <span className="usage-sep">·</span>
          <span className="usage-cache">{usage.cacheHitTokens.toLocaleString()} cached</span>
        </>
      )}
      <span className="usage-sep">·</span>
      <span className="usage-cost">
        {costStr}{isEstimate && " (估算)"}
      </span>
    </div>
  );
}

// ── ConversationUsageSummary ───────────────────────────────────────────────

function ConversationUsageSummary({ usage }: { usage: UsageSummary }) {
  return (
    <div className="conversation-usage" aria-label="Conversation token summary">
      <span className="conversation-usage__label">会话累计</span>
      <span>{usage.totalTokens.toLocaleString()} tokens</span>
      <span className="usage-sep">·</span>
      <span>{usage.promptTokens.toLocaleString()} in</span>
      <span className="usage-sep">/</span>
      <span>{usage.completionTokens.toLocaleString()} out</span>
      {usage.reasoningTokens > 0 && (
        <>
          <span className="usage-sep">·</span>
          <span>{usage.reasoningTokens.toLocaleString()} reasoning</span>
        </>
      )}
      {usage.cacheHitTokens > 0 && (
        <>
          <span className="usage-sep">·</span>
          <span className="usage-cache">{usage.cacheHitTokens.toLocaleString()} cached</span>
        </>
      )}
      <span className="usage-sep">·</span>
      <span className="usage-cost">{formatUsd(usage.estimatedCostUsd)}</span>
    </div>
  );
}

// ── 工具函数 ──────────────────────────────────────────────────────────────

function aggregateUsage(messages: Message[]): UsageSummary | null {
  const usages = messages
    .map((m) => m.usage)
    .filter((u): u is UsageSummary => Boolean(u));
  if (usages.length === 0) return null;
  const pricingVersions = new Set(usages.map((u) => u.pricingVersion));
  const usageSources = new Set(usages.map((u) => u.usageSource));
  return usages.reduce<UsageSummary>(
    (total, u) => ({
      promptTokens: total.promptTokens + u.promptTokens,
      completionTokens: total.completionTokens + u.completionTokens,
      totalTokens: total.totalTokens + u.totalTokens,
      cacheHitTokens: total.cacheHitTokens + u.cacheHitTokens,
      cacheMissTokens: total.cacheMissTokens + u.cacheMissTokens,
      reasoningTokens: total.reasoningTokens + u.reasoningTokens,
      estimatedCostUsd: total.estimatedCostUsd + u.estimatedCostUsd,
      usageSource: usageSources.size === 1 ? u.usageSource : "mixed",
      pricingVersion: pricingVersions.size === 1 ? u.pricingVersion : "mixed",
    }),
    {
      promptTokens: 0, completionTokens: 0, totalTokens: 0,
      cacheHitTokens: 0, cacheMissTokens: 0, reasoningTokens: 0,
      estimatedCostUsd: 0, usageSource: "", pricingVersion: "",
    }
  );
}

function formatUsd(cost: number): string {
  if (cost < 0.0001 && cost > 0) return "<$0.0001";
  return `$${cost.toFixed(6).replace(/\.?0+$/, "")}`;
}

/** 查找正文中工具调用标记的最早位置（<ToolCall>、DSML 各变体）；无标记返回 -1。 */
function findToolMarkupIndex(text: string): number {
  const markers = ["<ToolCall", "<DSML", "<｜DSML", "<｜｜DSML"];
  let min = -1;
  for (const marker of markers) {
    const idx = text.indexOf(marker);
    if (idx >= 0 && (min < 0 || idx < min)) min = idx;
  }
  return min;
}

/** 把后端原始错误串映射为面向用户的友好提示与建议动作；未识别的错误保留原文便于反馈。 */
function humanizeError(raw: string): string {
  if (raw.includes("DEEPSEEK_API_KEY")) {
    return "未检测到 API Key：请在 Windows 系统环境变量中配置 DEEPSEEK_API_KEY，然后重启应用。";
  }
  if (raw.includes("认证失败")) {
    return "API Key 无效：请检查系统环境变量 DEEPSEEK_API_KEY 是否填写正确。";
  }
  if (raw.includes("余额不足")) {
    return "DeepSeek 账户余额不足：请前往 DeepSeek 开放平台充值后重试。";
  }
  if (raw.includes("限流")) {
    return "请求被限流：当前请求过于频繁，请稍等片刻再发送。";
  }
  if (raw.includes("上下文长度超限")) {
    return "上下文超出模型上限：建议新建会话继续，或精简本次输入后重试。";
  }
  if (raw.includes("网络连接失败") || raw.toLowerCase().includes("error sending request")) {
    return "网络连接失败：已自动重试仍未成功，请检查网络（或代理设置）后重新发送。";
  }
  if (raw.includes("会话不存在")) {
    return "会话状态异常：请新建一个会话后继续。";
  }
  if (raw.includes("工作区路径不存在")) {
    return "工作区不可用：所选目录不存在或已被移动，请重新选择工作区。";
  }
  return `出错了：${raw}`;
}

function basenameFromPath(path: string): string {
  const normalized = path.replace(/[\\/]+$/, "");
  const parts = normalized.split(/[\\/]/);
  return parts[parts.length - 1] || path;
}
