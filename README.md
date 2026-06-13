# NovaCode

> **Ignite the Spark, Unleash the Code.**
> 星火燎原，代码无限。

NovaCode 是一个**本地优先（local-first）的桌面 AI 编码 Agent**，一个用来创造、实验和定义「AI 编码客户端应该是什么样」的试验场。它把对话绑定到本地工作区，把项目记忆留在你自己的机器上，并为国产大模型量身定制——让中国自研模型也能拥有媲美一线编码 Agent 的客户端体验。

NovaCode 不只为某一个模型而生。它的目标是成为**所有国产大模型**（DeepSeek、GLM、Kimi 等）的通用编码客户端载体：把成本透明、本地数据、可审计权限作为第一性原则，把模型当作可替换的引擎。

---

## 这是什么 / 定位

- **本地优先**：对话、工作区绑定、项目记忆、令牌账本全部存在本地 SQLite（`novacode.db`），不依赖任何云端会话后端。
- **桌面原生**：基于 Tauri 2（Rust 后端 + React 前端），当前面向 Windows 桌面，发布为带签名与自动更新的安装包。
- **Agent 内核**：具备多轮工具调用、上下文压缩、模型回退、子代理委派、中途转向（steering）等真实 Agent 能力，而不是简单的聊天壳。
- **可审计、可扩展**：权限分级 + 规则裁决、生命周期钩子、技能、自定义命令与子代理，全部以工作区下的明文配置（`.novacode/`）呈现，行为可见、可控、可改。
- **为国产模型定制**：当前内置 DeepSeek 客户端，并以「模型是可替换引擎」为架构方向，逐步接入更多国产大模型。

## 核心价值

| 价值 | 含义 |
|------|------|
| **成本透明** | 每轮调用的 token 用量与花费实时计入本地账本，可设单任务预算上限，可导出 CSV。 |
| **数据本地** | 代码、对话、记忆都留在本机；命令子进程会自动擦除密钥类环境变量。 |
| **权限可审计** | 工具按能力分级，支持 allow/deny 规则与人工审批，敏感操作（如 `.env`、`git push`）可精确拦截。 |
| **客户端可定义** | 技能 / 钩子 / 自定义命令 / 自定义子代理 / 诊断回路全部开放给用户，把「客户端能做什么」交还给使用者。 |

---

## 平台

NovaCode 当前面向 **Windows 桌面**，通过 Tauri 构建，发布 `.msi` / `.exe` 安装包，并支持基于签名清单的自动更新。

## 模型配置

当前迁移完成的代码内置 **DeepSeek 客户端**。运行前设置 API Key：

```powershell
$env:DEEPSEEK_API_KEY="your-api-key"
```

> NovaCode 的产品身份独立于任何单一模型供应商。多模型供应商（GLM、Kimi 等）适配为后续工作，架构上按「供应商可插拔」演进。

---

## 功能详解

### 工作区与会话
- **工作区-会话绑定**：每个对话绑定一个本地目录作为工作区，工具操作严格限定在工作区内。
- **项目长期记忆**：工作区根目录的 `NovaCode.md` 作为项目记忆，自动注入上下文。
- **上下文压缩**：超出软上限时自动压缩较早的大体积工具结果，保留最近若干轮与 system 消息，并保证 tool_call 配对不被拆散。可用 `NOVACODE_CONTEXT_SOFT_LIMIT` 调低阈值压测。

### 工具运行时
- **文件工具**：create / write / edit / read（支持 offset/limit 分页）/ delete / list_dir / make_dir / stat_path。
- **代码检索**：`search_text` 基于 `ignore` crate 的 ripgrep 式搜索（遵守 .gitignore、跳过隐藏文件），支持正则。
- **命令执行**：`run_command` 支持前台/后台；前台可启用**受限令牌沙箱**（受限 token + Job Object + 密钥环境变量擦除）；后台 shell 可查询输出、列举、终止。
- **仓库结构图**：按会话缓存的 repo map，降低重复扫描成本、保持缓存友好。
- **网络工具**：`web_fetch`（HTML 转正文）、`web_search`。

### Agent 能力
- **模型回退**：持续失败时在同档模型间自动回退（flash ↔ pro）。
- **子代理委派**：`run_subtask` 把只读探索任务交给拥有独立上下文的子代理；可通过 `.novacode/agents/<type>.md` 指定自定义子代理角色。
- **中途转向（steering）**：运行过程中向队列追加指令，无需打断当前任务。
- **成本预算**：可设单任务 token 上限，超限自动收尾。
- **诊断回路**：在 `.novacode/diagnostics` 配置诊断命令，编辑产生后自动运行，失败时把输出回注给模型自我修复。

### 可扩展配置（均在工作区 `.novacode/` 下）
- **技能** `.novacode/skills/<名>/SKILL.md`：按需 `load_skill` 加载完整说明。
- **钩子** `.novacode/hooks.json`：PreToolUse / PostToolUse 生命周期钩子。
- **自定义斜杠命令** `.novacode/commands/<名>.md`：带 frontmatter 的可复用提示。
- **自定义子代理** `.novacode/agents/<类型>.md`：自定义子代理角色。
- **诊断命令** `.novacode/diagnostics`。

### MCP（Model Context Protocol）
- **stdio 传输**：拉起本地 MCP 服务器进程。
- **Streamable HTTP / SSE 传输**：连接远程 MCP 服务。
- **OAuth 2.1 PKCE**：401 时自动发现元数据、动态注册、PKCE 授权、本地回调换取令牌。*(OAuth 路径尚未完成真机验证)*
- **`add_mcp_server` 工具**：Agent 通过 NovaCode 的真实机制注册 MCP（写入本地库 + 实际建连），而非编辑配置文件。

### 权限与数据治理
- **权限分级**：工具按能力（文件读/写/删、命令执行、网络访问）分级裁决。
- **规则裁决**：allow/deny 规则，deny 优先；支持按路径 glob 与命令前缀匹配。
- **数据治理**：导出对话、导出 token 账本（CSV）、清空全部对话。

### 成本与令牌
- **令牌账本**：每轮 prompt/completion/cache 命中/推理 token 实时记账。
- **预算上限**：单任务 token 预算，超限收尾。

---

## 开发

```powershell
npm install
npm test
npm run typecheck
npm run build

cd apps/desktop
npx tauri dev      # 本地开发运行
npx tauri build    # 构建安装包
```

Rust 侧验证：

```powershell
cargo check --workspace
cargo test --workspace
```

---

## 许可证

NovaCode 采用 **Modified MIT License**（修改版 MIT，参照 Kimi K2 范式）：

- ✅ 允许自由使用、修改、二次开发、借鉴、再分发、商用。
- ⚠️ **必须保留出处署名**：任何衍生作品/产品需在文档或「关于」等显著位置标注
  `Based on NovaCode by aki66938 (https://github.com/aki66938/NovaCode)`。
- ⚠️ **大规模商用需显著展示**：若衍生产品月活 > 1 亿或月营收 > 2000 万美元，需在界面显著位置展示 "NovaCode"。
- ⚠️ 不授予 "NovaCode" 名称与 logo 的商标权（署名所需除外）。

详见 [LICENSE](LICENSE)。本项目全面开源,既欢迎二开借鉴,也保护源头的署名与地位。
