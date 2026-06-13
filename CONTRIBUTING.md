# 为 NovaCode 贡献

感谢你有意贡献 NovaCode！本文件说明如何提交代码。角色与权力见 [GOVERNANCE.md](GOVERNANCE.md)，仓库开发细则见 [.dev-rules.md](.dev-rules.md)。

## 开始之前

- NovaCode 是本地优先的桌面 AI 编码 Agent（Tauri 2 + Rust + React，面向 Windows 桌面）。
- 本项目采用 **Modified MIT License**：允许自由二开与借鉴，但**必须保留出处署名**（见 [LICENSE](LICENSE)）。提交贡献即表示你的代码以同一许可证授权。

## 环境与自测

```powershell
npm install
npm test
npm run typecheck
npm run build

cd apps/desktop
npx tauri dev      # 本地运行
```

Rust 侧：`cargo check --workspace`、`cargo test --workspace`。

## 分支与 PR 流程

NovaCode 使用 `dev → main` 模型：

1. **Fork** 本仓库，基于 `dev` 分支创建你的工作分支：`feat/<主题>` 或 `fix/<主题>`。
2. 完成开发，确保 `cargo test --workspace` 与 `npm run typecheck` **全部通过**。
3. 发起 **PR，base 选 `dev`**（不要直接对 `main`）。
4. CI（`build.yml`）会对 PR 跑编译验证；维护者 review。
5. 合并进 `dev` 后，由主创做真机 app 体验验证。
6. 验证通过后，由主创合并 `dev → main` 并发版。

> `main` 受保护，不接受直接 push 或直接 PR（紧急修复例外，需主创批准）。

## Commit 规范

```
<type>(<scope>): <summary>
```

- type：`feat` `fix` `refactor` `style` `docs` `chore` `test`。
- summary 用祈使句、客观、≤ 80 字符；一次提交只做一件事。
- 文件统一 UTF-8（无 BOM），确认中文不乱码。
- 若由 AI 协助完成，请在 commit 末尾以 `Co-Authored-By:` 如实标注所用模型。

## 不要提交

API Key、密码、token 等明文密钥；`target/`、`node_modules/`、`dist/`、`*.db`、`*.local.md` 等本地产物（已在 `.gitignore`）。

## 行为准则

- 提案与实现以质量与可验证性为先；不夸大未实现/未验证的能力。
- 尊重既有代码风格，做最小必要变更。
- 讨论对事不对人，保持友善。

## 想做独立二开？

你可以 fork 后自行演进。届时你是自己 fork 的主创，可自定治理；唯一硬约束是遵守 [LICENSE](LICENSE) 的出处署名。本仓库的治理/贡献文档可作为你的参考模板。
