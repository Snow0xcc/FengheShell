# AI Copilot 交接文档

> 面向接手的开发/维护者：先读这里，再读 `AI_COPILOT.md` / `AI_PANEL_REQUIREMENTS.md`。
> 重点回答两件事：**AI Copilot 面板当前到底放在哪、怎么算宽度**，以及
> **截图里那些“位置异常 / 空白 / 命令跑错 shell / 退出码不红”的结论哪些是真实 bug、现在修到哪一步**。
>
> 文档基准：`main` @ `cadfb49` + 本轮交接过程中落地的三处修复（见 §9）。

---

## 1. 一句话结论

**AI Copilot 面板是“应用级右侧 dock”的设计意图没错，但落地时被错误嵌套进了分屏
`for pane` 循环里，导致面板宽度被双重扣减、并在分屏下重复渲染。** 这正是截图里
“终端窄 + 中间空白 + 面板夹在中间”的根因，而不是旧版 overlay 残留。该问题连同两个
“命令跑错 shell / 失败仍显示绿色”的逻辑问题，已在本轮修复并通过编译 + 单测。

---

## 2. 项目与技术栈速览

| 项          | 值                                                                                |
| ----------- | --------------------------------------------------------------------------------- |
| 名称 / 版本 | FengheShell `0.7.8`（`Cargo.toml`，上游 MeatShell 更名）                          |
| 定位        | 轻量 FinalShell 风格 SSH/终端客户端                                               |
| 语言 / UI   | Rust 2021 + Slint `1.18.1`（`vendor/` 里是打过补丁的 `i-slint-*`）                |
| Workspace   | 根 crate + `crates/ai-copilot`（纯逻辑，不持有 SSH/PTY）                          |
| 构建门禁    | `scripts/cargo-safe.sh`（本机单任务峰值 ~3 GiB，勿裸跑 `cargo test --workspace`） |
| AI 方案     | “FengheShell × rssh 缝合”：rssh 的 AI 运维范式以独立 crate 注入                   |

关键布局：

```text
src/app/           —— 宿主胶水（状态机 + Slint 回调 + 落盘 + 设置页）
crates/ai-copilot/ —— 纯逻辑（脱敏 / 风险 / 编排 / shell 适配，可单测）
ui/*.slint         —— 界面（app.slint 顶层窗口；ai_panel.slint 面板）
docs/              —— AI_COPILOT.md / AI_PANEL_REQUIREMENTS.md / AI_PANEL_M1_PLAN.md
```

---

## 3. AI Copilot 三层架构

```text
┌────────────────────────────────────────────────────────────┐
│ Slint 视图层                                                │
│  资源监控侧边栏(sidebar) | PTY 终端视窗 | AI 面板(右侧 dock) │
├────────────────────────────────────────────────────────────┤
│ crates/ai-copilot（纯逻辑，可单测）                          │
│  blocks     OSC133 语义块 + 8KB 上下文窗口                   │
│  redact     编号占位符脱敏 + 本地反向还原                    │
│  sanitize   tree-sitter 命令形状校验 + 黑名单                │
│  risk       三档风险分级 + Run Gate 裁决                     │
│  triage     LLM 编排 / 工具循环 / HITL 审批                  │
│  shell      POSIX / PowerShell / cmd / serial / telnet 适配 │
├────────────────────────────────────────────────────────────┤
│ 宿主 src/app/ai.rs                                           │
│  Conversation 会话（消息/状态/锚点/审批/中断）                │
│  ToolRuntime 实现：写 PTY / 读远端文件                       │
└────────────────────────────────────────────────────────────┘
```

---

## 4. AI 面板位置与宽度（核心，先看这里）

### 4.1 设计意图（M1）

面板是**应用级右侧 dock**：`ui/app.slint` 的 `quick-area` 内，与 `central` 平级；
`central` 宽度按 `ai-panel-width` 收缩，面板占据被让出的右侧空间。

```slint
// ui/app.slint —— central 宽度已扣 ai-panel-width
width: (...) - (root.ai-open ? root.ai-panel-width * 1px : 0px);
```

宽度只有两档 **360 / 520 px，默认 360**，持久化到 `sessions.json` 的
`ai_panel_width`；**刻意不做拖拽**（需求 D1 拍板）。开关 `Ctrl+Space`，
新建会话 `Ctrl+Shift+O`（macOS `⌘⇧O`）。

### 4.2 曾经的真实 bug（本轮已修复）

多 agent 核对 + 逐层数括号确认，面板 `if root.ai-open : Rectangle` **原本被放在了
`for pane` 循环的内容区 `Rectangle`（`ui/app.slint:2072`）里**，而不是 `quick-area`
下、与 `central` 平级。后果：

- `parent.width` 指向的是**已经收缩后的 pane 宽度**，再 `x = parent.width - ai-panel-width`
  就**多扣了一个面板宽** → 终端更窄、面板右侧留白、面板还压在终端上。
- 分屏时 `for pane` 会为每个 pane 各渲染一个 `AiPanel`（N 份），与“全局唯一 dock”矛盾。

**修复**：把整块 `if root.ai-open` 移到 `central` 之后、`if false : QuickDockPanel` 之前，
成为 `quick-area` 的直接子节点（与 `central` 平级）。这样 `parent` = 未收缩的
`quick-area`，`x = parent.width - ai-panel-width` 正好填进 `central` 让出的空间。

### 4.3 资源监控侧边栏（“运行状态”）

- 资源监控是 `sidebar.slint` 的 `Sidebar`，几何由 Rust dock 引擎（`#dock-stack`）拥有
  （`src/app/dock_stacks.rs`、`dock-central-x/y/w/h`）。
- `ui/app.slint` 里 `if false` 的旧侧栏/分割条块（L1679、L1763、L1834）是 `#dock-stack`
  之前的死代码，属清理项（P3），不影响运行。
- dock 引擎对四条边的 `dock-central-*` 扣减本身正确；问题只在 AI 面板自身用了错误的父级。

---

## 5. 关键代码位置索引

| 主题                             | 位置                                                                   |
| -------------------------------- | ---------------------------------------------------------------------- |
| 面板组件（消息流/卡片/输入框）   | `ui/ai_panel.slint`（`AiPanel` L226 起；`AiCommandCard` L43 起）       |
| 面板挂载 + 宽度收缩              | `ui/app.slint`（`central` 宽度算式；AI dock 现位于 `central` 之后）    |
| 补全层 + 入口星标                | `ui/terminal_view.slint` L311–319、L650–657、L1595–1631                |
| 状态机 / 退出码 → 卡片           | `src/app/ai.rs`（`message_card` L325–354、`disposition_tag` L303–311） |
| 会话表 / 持久化 / 检索 / 导出    | `src/app/ai_store.rs`                                                  |
| Slint 回调 + 设置落盘 + 锚点解析 | `src/app/ai_callbacks.rs`（`tab_session`/`shell_kind`）                |
| M2 设置页                        | `src/app/ai_m2.rs`                                                     |
| 编排 / Run Gate / HITL           | `crates/ai-copilot/src/triage.rs`                                      |
| shell 适配（关键）               | `crates/ai-copilot/src/shell.rs`                                       |
| 上下文块 / 退出码解析            | `crates/ai-copilot/src/blocks.rs`                                      |
| 脱敏                             | `crates/ai-copilot/src/redact.rs`                                      |
| 系统提示词                       | `crates/ai-copilot/src/prompts/general.md`                             |

---

## 6. 命令跑错 shell 与退出码的现状（本轮补了缺口）

### 6.1 shell 上下文：机制有，但曾漏掉内置/agent 标签（已修）

- 机制正确：`ShellKind` 枚举 + `prompt_section()` 注入 system prompt（`triage.rs`
  `build_system_prompt`），PowerShell 会明确用 `Get-Process`/`Get-CimInstance` 并禁用
  `uname`/`/etc/*`（`shell.rs` L214–220）。
- **曾有的缺口**：`tab_session()`（`src/app/ai_callbacks.rs`）只查 `ConfigStore::get()`
  （= `cache.sessions`），而 `system:powershell` / `system:cmd` / `system:wsl` / `agent:*`
  这些内置会话**从不写进 cache**，于是 `session = None` → `shell_kind(None) = Posix`。
  结果就是截图里“PowerShell 标签上跑 uname”的直接原因。
- **修复**：`tab_session()` 在 `store.get()` 落空时回退 `resolve_builtin_session()`，
  与连接流程（`session_callbacks.rs` 的 builtin 分支）保持一致。

### 6.2 退出码 → 红/绿：主链正确，但通道失败曾是绿色（已修）

`src/app/ai.rs::message_card`（L325–354）：

| 结果                   | 文案                                    | 颜色 |
| ---------------------- | --------------------------------------- | ---- |
| `exit_code == Some(0)` | `executed · exit 0`                     | 绿   |
| `exit_code == Some(n)` | `failed · exit n`（`step_failed=true`） | 红   |
| 超时                   | `timed out`（`step_failed=true`）       | 红   |
| 未知退出码             | `exit unknown`（`step_failed=true`）    | 红   |

- **曾有的边角**：`triage.rs` 的 `run_command` 在执行通道失败（`Err`，如
  `anchor_busy` / `session_channel_closed` / `anchor_lost`）时，`outcome` 保持 `None`、
  `disposition = Rejected` → `message_card` 走 `outcome == None` 分支 → `step_failed=false`，
  只读命令卡片甚至是**绿色底**、灰字 `rejected`。
- **修复**：`Err` 分支写入 `outcome = Some(CommandOutcome { exit_code: None, output: e.to_string(), timed_out: false })`，
  让卡片显示为红色的 `exit unknown`（并把错误信息放进卡片输出区）。

### 6.3 硬件快照注入（本就正常）

`SystemSnapshot::to_prompt_section()`（`triage.rs` L234–242）生成
“Host resource snapshot (live from the FengheShell monitor sidebar)” 段，
`build_system_prompt` 在 `ctx.system` 存在时注入（L839–842）。截图里“右侧已有监控却让 AI
再敲命令查”的担心，在当前实现里本就不存在。

---

## 7. 仍开着的项（交接重点）

按 `docs/AI_PANEL_REQUIREMENTS.md` §6.1：

| 优先级    | 事项                                   | 说明                                                                                                                                                                                                  |
| --------- | -------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **P1-N4** | **M1 真机手工回归（WP8 矩阵）未做**    | Windows/macOS/Linux × 中文 IME / htop 全屏 / 分屏 / 多窗口拖 tab / 睡眠唤醒。**这是唯一自动化盲区，也是最可能藏“UI 位置异常”的地方**——本次修的是“单 pane 宽度算错”，但分屏/多窗口场景仍没有真机验证。 |
| P1-N5     | M2 设置页真机回归                      | 模型/记忆/技能三页在 360/520 两档下的中文排版、真实多模型切换、导出对话框行为。                                                                                                                       |
| P0-N2     | `russh` 升级立项                       | 22 条依赖告警豁免已复检并写进 `audit.toml`；触发条件：ssh-key 0.7 / rsa 0.10 / pkcs1 0.8 转正，或出现可达告警。                                                                                       |
| P2-N7     | 外部 MCP client RFC 评审               | `docs/AI_MCP_CLIENT_RFC.md` 只是草案，评审通过前不得有实现代码。                                                                                                                                      |
| P2-N8     | 记忆自动提炼 / 导出增强 / LLM 自动命名 | 依赖真实使用数据，当前不提前开工。                                                                                                                                                                    |
| P3-N9     | 分支清理                               | `m2`、`auto/ai-multi-session-e371`、`work-m3` 等已合入，可删。                                                                                                                                        |
| P3-N10    | Issue #1 收口                          | 待 N4/N5 完成后决定继续 M3 还是关 #1。                                                                                                                                                                |

已知但“按设计如此”/后续项：

- AI 面板**不做拖拽缩放**（固定 360/520），需求 D1 决策。
- `ui/app.slint` 的 `if false` 旧侧栏/分割条块是 `#dock-stack` 之前的死代码，清理项。
- 记忆的 “AI 提议卡片”只落了数据链路（`ai-proposed`），交互未实现（F7 说明）。
- **SSH 远端 shell 一律按 Posix**，无远端 shell 探测（Windows SSH 远端会被误判）；这是
  现有设计，需要远端探测能力才能解决，本轮未做。
- **`run_file_op` 的 `Err` 分支**和 `run_command` 一样存在“通道失败→绿色 rejected”的旧
  模式（`triage.rs` `run_file_op` Err 分支），本轮只修了 `run_command`，`run_file_op`
  建议按同法补。
- `shell.rs` 注释里提到的前端 `findSentinel` 正则并不存在，真实匹配是 `src/app/ai.rs`
  `ExecCapture::feed` 的字符串 `find` + `parse::<i32>()`；注释漂移，可顺手修。

---

## 8. 最近相关提交

`cadfb49` **Improve AI Copilot guidance and panel UI**（当前 HEAD）已针对“卡片可读性、
滚动可预测、诊断绑定选中终端、shell 指引、输出脱敏”做了一轮大改。本轮交接在其基础上
再修复了 §4.2 / §6.1 / §6.2 三处。

---

## 9. 本轮落地的修复（多 agent 核对后）

| #   | 文件                              | 改动                                                                                                          | 验证                              |
| --- | --------------------------------- | ------------------------------------------------------------------------------------------------------------- | --------------------------------- |
| 1   | `ui/app.slint`                    | 把 `if root.ai-open` 从 `for pane` 内容区移到 `quick-area` 下、与 `central` 平级，消除双重扣宽 + 分屏重复渲染 | `cargo check` ✅                  |
| 2   | `src/app/ai_callbacks.rs`         | `tab_session()` 在 `store.get()` 落空时回退 `resolve_builtin_session()`，内置/agent 标签获得正确 `ShellKind`  | `cargo check` ✅ + app AI 单测 ✅ |
| 3   | `crates/ai-copilot/src/triage.rs` | `run_command` 通道失败 `Err` 分支写入 `outcome`，卡片变红 `exit unknown`                                      | `ai-copilot` 274 单测 ✅          |

验证汇总：

- `sh scripts/cargo-safe.sh check` —— 通过（含 Slint `.slint` 编译）。
- `sh scripts/cargo-safe.sh test -p ai-copilot` —— **274 passed / 0 failed**。
- `sh scripts/cargo-safe.sh test -p fengheshell --bin fengheshell ai_` —— **29 passed / 0 failed**。

---

## 10. 交接检查清单（建议按序）

1. 读 `docs/AI_COPILOT.md` + `docs/AI_PANEL_REQUIREMENTS.md`（决策/验收）。
2. **真机打开面板**，确认：终端按面板宽收缩、无右侧空白、无重叠；切 360/520 即时生效；
   分屏（两 pane）时**只出现一个 AI 面板**。
3. 在 PowerShell 5.1 标签里提问，确认 AI 不再发 `uname`/`cat /etc/*`，而是走
   `Get-*`；跑一个失败命令确认卡片红 `failed · exit n`，跑一个 cmdlet 确认不误报超时。
4. 若再出现“位置/空白”类问题，优先查 `ui/app.slint` 的 `central` 宽度算式与 AI dock 的
   父级，而不是去改 `ai_panel.slint` 内部的 `VerticalLayout`。
5. 改 `.slint` 后用 `sh scripts/cargo-safe.sh check` 快速反馈，别裸跑全量测试。

---

## 11. 已知限制

- shell 识别 best-effort：fish/csh/nushell 回退 POSIX 模板，sentinel 可能失败（命令仍执行，只是退出码拿不到）。
- 补全层不携带工具、永不可执行（runtime 是恒拒绝实现）。
- 会话历史只在 `ai_conversations.json` 落盘；旧版本对话从不落盘，无迁移。
- 审批通道是进程内 `oneshot`，跨进程不可恢复（重启后 pending 卡片作废为 `expired`）。
