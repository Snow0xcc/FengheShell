# AI Copilot（FengheShell × rssh 缝合方案）

两个项目互补：FengheShell 提供「全功能低开销的传统运维底座」（资源监控、多标签终端、
SFTP），rssh 提供「AI 运维 Copilot 范式」（无感上下文捕获、AI Triage、本地脱敏、
带副作用评级的命令执行）。两边都是 Rust，因此不需要跨语言 FFI 胶水层 —— AI 侧的
逻辑以独立 crate 的形式注入基座。

> **多会话面板（GitHub Copilot 风格布局：模型/模式/会话切换、记忆/技能/MCP 配置、
> 多会话并行与历史管理）的需求分析与功能规划见 [AI_PANEL_REQUIREMENTS.md](AI_PANEL_REQUIREMENTS.md)。**

```text
┌──────────────────────────────────────────────────────────┐
│                      Slint 视图层（ui-slint）            │
│  ┌────────────────┐ ┌─────────────────┐ ┌──────────────┐ │
│  │ 资源监控侧边栏 │ │  PTY 终端视窗   │ │ AI 面板 dock │ │
│  │ (FengheShell)    │ │ 命令块 / ANSI   │ │ ai_panel     │ │
│  └────────────────┘ └─────────────────┘ └──────────────┘ │
├──────────────────────────────────────────────────────────┤
│                    Rust 核心（crates/ai-copilot）         │
│  blocks       OSC 133 语义块 + 8KB 上下文窗口             │
│  redact       编号占位符脱敏 + 本地反向还原映射           │
│  sanitize     tree-sitter 命令形状校验 + 黑名单           │
│  risk         三档风险分级 + Run Gate 裁决                │
│  triage       LLM 编排 / 工具循环 / HITL 审批             │
│  shell         POSIX / PowerShell / cmd 适配             │
├──────────────────────────────────────────────────────────┤
│                 FengheShell 宿主（src/app/ai.rs）           │
│  TabContext   每标签的终端上下文（命令块 + 执行锁）        │
│  Conversation 会话：消息 / 状态 / 锚点 / 审批 / 中断       │
│  UiRuntime    ToolRuntime 实现：写 PTY / 读远端文件       │
└──────────────────────────────────────────────────────────┘
```

## 怎么用

1. 终端状态条右侧的**五芒星**（或 **Ctrl+Space**）开关右侧 AI 面板；头部标题点击
   展开会话列表，`+` / **Ctrl+Shift+O**（macOS `⌘⇧O`）新建会话。面板打开时终端区
   按面板宽度（360 / 520 两档）收缩，不会盖住终端。
2. 配置 LLM：设置 → Interface → **AI Copilot** 页（或抽屉标题栏的齿轮）：协议 /
   端点 / 模型 / API Key，以及「本地脱敏」与「只读命令免确认执行」两个开关。设置写进
   `sessions.json` 的 `ai_*` 字段，API Key 与其它会话密码一样用 ChaCha20-Poly1305
   加密落盘；输入框里的密钥是只写的，保存后即清空，已保存的密钥不会被读回界面。
3. 直接提问，或什么都不问 —— CPU / 内存 / 磁盘越界时打开抽屉会自动起一轮排查。
4. AI 给出的命令会以卡片形式出现在对话里，按风险等级决定你能怎么操作（见下）。

## 多会话面板（M1）

面板是**应用级右侧 dock**，会话不属于任何终端标签：

| 动作 | 行为 |
| --- | --- |
| 收起面板 / 切标签 / 切会话 / 把标签拖到别的窗口 | **都不打断**正在跑的一轮；消息继续写进 Rust 侧会话表，切回即重放 |
| 新建会话 | 锚定当前终端标签（welcome 标签上建纯对话会话），标题取首条提问前 30 字 |
| 关闭会话 | 直接删除；正在跑的轮次按「停止」结算（审批按拒绝、pending 卡片作废） |
| 锚点标签被关闭 | 会话保留，标记锚点失效；后续轮次以无终端上下文运行并在对话里明示 |
| 并发 | 同时最多 3 条会话在跑（`sessions.json` 的 `ai_max_parallel`，可调 1–5）；满了发送即提示，**不排队** |
| 同锚点执行 | 标签级执行锁：两个会话抢同一个 PTY 时，第二个拿到 `anchor_busy`，一个字节都不写进去 |
| 退出应用 | 所有在跑的轮次按「停止」取消，会话同步 flush 落盘 |

持久化：`ai_conversations.json`（数据目录，与 `sessions.json` 同级），0600 +
tmp/rename 原子写，每轮结束防抖 500ms 批量写、退出时 flush；**落盘前消息过脱敏规则**。
重启后会话恢复为 `interrupted`，`pending` 审批卡片一律作废为 `expired` —— 审批通道是
进程内的 `oneshot`，跨不了进程。旧版本的对话本来就不落盘，因此**不做迁移**。

Ask / Agent 是**会话级**的：切换模式作用于当前会话，全局 `ai_mode` 只是新会话的默认值。

## 模型 / 记忆 / 技能 / 历史（M2）

M2 把「只能在 JSON 里手改的东西」搬进了设置页，也给会话补上了模型锁定与历史管理。

| 能力 | 位置 | 行为 |
| --- | --- | --- |
| 多份模型配置 | 设置 → Interface → **模型配置** | 每份是 name / protocol / endpoint / model / API Key，其中一份为默认；API Key 同样加密落盘、只写不回读 |
| 会话级模型锁定 | 面板头部 **模型 chip** | 会话创建时快照当时默认配置的名字；之后改全局默认不影响已有会话，运行中切换只作用于之后发起的轮次 |
| 记忆注入 | 设置 → Interface → **记忆** | 全局单列表（内容 / 来源 / 开关 / 时间），来源只有 `manual` 与 `ai-proposed`；注入 system prompt 前置段落，注入前过脱敏规则；每条上限 500 字、总量 50 条 |
| 会话级记忆开关 | 面板设置抽屉 | 关掉后这条会话不再注入记忆，其它会话不受影响 |
| 技能管理 | 设置 → Interface → **技能** | 增删改查用户技能（id 唯一且不能是 `general`，内容上限 8 KB），保存即写 `ai_skills`；`general` 内置集始终生效 |
| 历史检索 | 面板会话列表的检索框 | 标题 + 消息全文大小写不敏感匹配，列表行显示命中片段 |
| 会话导出 | 会话列表行尾的导出按钮 | 脱敏后的 Markdown（含命令卡片的风险 / 闸门 / 处置、Agent 计划与步骤痕迹），按敏感文件保管 |
| 并发上限 | 设置 → Interface → AI Copilot 页 | 1–5 档，写 `ai_max_parallel`，超出后发送即拒绝、不排队 |
| 内置 MCP | 设置 → Interface → MCP 页 | 开关与权限矩阵（原有），新增文档入口（`docs/REMOTE_MCP.md`）；底部「配置位置」一行直接给出 `sessions.json` 的绝对路径（便携模式下跟着可执行文件走） |

记忆**不会自动提炼**：目前只能由用户自己写。`ai-proposed` 来源与落库链路已就绪，
但「AI 提议 → 卡片确认」的交互**尚未实现**（见 `AI_PANEL_REQUIREMENTS.md` 的 F7 说明）。
外部 MCP client（作为 client 连别人的 server）仍在 M3，需先有独立 RFC：
[docs/AI_MCP_CLIENT_RFC.md](AI_MCP_CLIENT_RFC.md) 是设计草案，**未实现** ——
评审通过前代码里不应出现任何外部 server 连接路径。

## 工作方式：Ask / Agent（参考 GitHub Copilot 的能力分层）

GitHub Copilot 有三层能力：补全 → 对话/编辑 → 代理。在运维场景里对应成两层：

| Copilot 层 | FengheShell 对应 | 行为 |
| --- | --- | --- |
| Chat / Edit | **Ask**（默认） | 解释现象、定位原因、给出命令建议；工具回合上限 2，不主动落地改动 |
| Agent | **Agent** | 自己规划 → 取证 → 执行 → 验证 → 汇报；工具回合上限 6，每一步在抽屉里留痕 |

两种模式共用同一套工具与 Run Gate：Agent 并不会绕过审批，橙/红命令仍然只填入终端。
区别只是**回合预算**和**系统提示**（Agent 会额外收到「先取证、再动手、最后验证」的工作方式）。

抽屉里的星形步骤行就是 Agent 的工具调用记录：取证跑过什么、哪一步失败了，用户不必只信结论。

入口：终端状态条右侧的**五芒星**（打开时填充强调色），或 **Ctrl+Space**。空会话时会给出三个
起手建议（都是先只读的排障问法）。设置里的 **Test connection** 会用当前填写的配置请求一次
`/models` 做连通性自检，不写盘。

### 计划清单（Agent）

Agent 模式会先用 `set_plan` 工具把计划发布成一份**清单**（最多 8 条），抽屉里渲染成
带复选框的步骤行，状态随执行推进：

| 状态 | 图标 | 含义 |
| --- | --- | --- |
| `pending` | 空心框 | 还没轮到 |
| `running` | 星标（强调色） | 正在执行 |
| `done` | 勾选框（绿） | 对应的一次工具调用成功 |
| `failed` | 破折框（红） | 对应的一次工具调用失败或被拦下 |

推进规则是「第 n 次动作型工具调用对应第 n 步」——这是有意做的简化：让模型逐步回报进度
既啰嗦又不可靠。清单只用于**展示进度**，真正的执行结果仍以命令卡片和步骤痕迹为准。

### 补全层（命令输入框的幽灵文本）

对应 Copilot 第一层：在命令栏输入框里，敲到一半时模型把后续命令以灰色幽灵文本画在光标后面，Tab 接受，Esc 只清建议不丢焦点。

- 输入停顿 400ms 才发请求，少于 3 个字符不发；没有配置 API Key 时整层不启用
- 请求带最近执行过的 4 条命令作为上下文，但不带工具：补全阶段永远不可能执行任何东西（runtime 是恒拒绝的实现）
- max_tokens 只有 64，一次补全几十个 token；10 秒超时即静默放弃
- 每次输入变化都会作废上一次响应（序号守卫），旧建议不会盖到新输入上

## 上下文是怎么抓的


传统终端只是一串无状态的字符流，AI 要能诊断，前提是分清「哪一段是命令、哪一段是输出」。
`crates/ai-copilot/src/blocks.rs` 在会话 pump 线程上对输出流做两件事：

- **OSC 133 语义块**（iTerm2 / VSCode / Kitty 同一套协议）：遇到 `ESC ] 133 ; A/B/C/D`
  就切出「命令 + 输出 + 退出码」。远端 shell 装了 shell integration 时优先走这条路。
- **FengheShell shell hook 兜底**：FengheShell 连上后会在远端 shell 里装钩子，每条命令
  跑完在 precmd 阶段回报 `OSC 697`（事件 `SessionEvent::CommandRan`），据此切块。
  此时退出码未知，AI 看到的是 `[exit ?]`。

两条路都在数据离开本机之前先 `strip_ansi` 清洗 CSI / OSC 等转义序列，并按照 8 KB
滑动窗口截断（`tail -f` 这种滚屏不会产生 token 爆炸）。

## 脱敏与还原

`redact` 在转发之前把内置 8 条规则命中的内容替换成编号占位符
（`<INTERNAL_IP_1>`），映射表只存在本地内存。规则集与上游 rssh 的 seed 一致：
内网 IP（`10.x` / `172.16-31.x` / `192.168.x`）、`Bearer` 头、`sk-` 开头密钥、
`AKIA` 开头 AWS key、JWT（`eyJ…`）、32 位及以上的长 hex 串（哈希 / 会话号）。
**不覆盖** IPv6、内网域名、私钥块与 `export` 敏感字段 —— 那些内容会原样出网，
涉及它们的问题请自行规避或先改成本机占位符再提问。

AI 若把占位符写回命令 —— 例如 `curl -s http://<INTERNAL_IP_1>/api/health` ——
宿主在执行前会用同一张映射表反向还原成真实命令。注意顺序：**先还原、再过 shape
validator**，否则占位符里的尖括号会被当成 shell 重定向节点，把一条无害的探测命令误杀。

## 风险分级与 Run Gate

AI 提议的每条命令都要过四道墙，任何一道失败都不会碰到你的机器：

| 墙 | 作用 |
| --- | --- |
| 1. shape validator | tree-sitter 解析后按黑名单 + 每命令规则硬拦（`rm` / `bash -c` / `chmod -R` / 写重定向 / 无界循环 …） |
| 2. 风险分级 | `ReadOnly` 绿 / `Mutating` 橙 / `Destructive` 红 |
| 3. Human-in-the-loop | 绿灯（且开启免确认）自动放行；其余一律等你点按钮 |
| 4. 可见执行 | 命令写进**用户看得见的那个 PTY**，靠注入的 sentinel 回声取退出码与输出 |

UI 上的对应关系：

- **绿灯 + Run**：直接写入终端并回车（仍然发生在你眼前，没有后台静默执行）。
- **橙 / 红灯 + 填入终端**：只把命令写进终端的输入行，回车由你按。
- **红色 + blocked_reason**：shape validator 已经拒绝，按钮都不给你 —— AI 只会收到
  拒绝原因并调整策略。

审批有 180 秒上限，超时或抽屉关闭都按「拒绝」结算，AI 不会卡死。

`run_command` 可以带 `timeout_s`（1..=300 秒）：LLM 声明的等待上限会透传给宿主，
没声明则用宿主默认（30 秒）。file_ops 的后端模板命令不走 LLM 超时。

## YOLO 模式（完全信任）

设置 → Interface → AI Copilot 页的 **YOLO 模式（完全信任）** 开关（默认关，
即时生效、跨窗口同步）打开后，上面所有的护栏一次性全部撤掉：

- **脱敏关闭**：系统提示、终端上下文、命令输出、网页正文都不再过脱敏规则 ——
  终端里出现的密钥、Token 会原样发往你所配置的模型服务商。
- **shape validator 跳过**：`rm -rf`、无界循环、写重定向等形态照单全收。
- **审批全免**：只读 / 有副作用 / 高危命令一律直接执行，`run_command`、
  file_ops 的四个步骤、`web_fetch` 都不再弹卡；「只读命令免确认执行」被覆盖。
- **审计留痕**：每轮开始时会记一条 `yolo mode active` 的 Note，执行记录照常，
  导出后能看到这个模式期间发生的一切。

开启期间抽屉顶部常驻一条红色警示横幅。适用场景：受控环境（容器 / 快照可随时
回滚的虚拟机）、完全本地的模型（如 Ollama）、或你就是要 AI 全自动干活的演练。
任何面向生产凭据或不可逆操作的场景都不要开。关闭后所有护栏立即恢复。

## 审计日志

每轮诊断都会在引擎里记一份审计（rssh 同款事件点移植）：会话开始 / 用户提问 /
每次 LLM 请求与回包的 token 数 / 命令的提议、审批、拦截与执行（含脱敏后的输出、
字节数与耗时）/ Skill 加载 / web_search 与 web_fetch 的完成情况 / 备注与错误。

- 只在**进程内存**里累积，不自动落盘；退出即清空。
- 实时写入：用户中途「停止」的轮次，已发生的事件同样留在日志里。
- 输出内容按当轮的「本地脱敏」开关处理，但仍含命令与输出 —— 导出文件按敏感文件保管。
- 设置 → Interface → **AI Copilot** 页的 **Export audit log** 可保存为文本
  （`fengheshell-ai-audit.log`），grep / less 友好。

## 隐私与遥测

FengheShell **没有任何遥测**：没有使用统计、埋点、崩溃上报或行为采集，也不会把终端
内容、命令历史或会话数据发往任何由我们控制的服务器。

- **外发请求只有三类，且都由用户行为触发**：你自己配置的 LLM 端点；`web_search` /
  `web_fetch` 请求的目标站点；可选的更新检查（只查询 GitHub releases 的 tag 与资源列表，
  下载只接受本仓库 `https://github.com` 路径下带 SHA-256 摘要的安装包）。
- 审计日志只在**进程内存**里累积，退出即清空，只有你点了导出才会落盘。
- 会话历史 `ai_conversations.json` 只写在**本机**数据目录（0600、原子写、落盘前脱敏）。
- 资源监控侧边栏与系统信息窗口里的 CPU / 内存 / 网络数据是**本机展示**，不外发；提问时
  附带的硬件快照同样只进这一轮对话的 prompt。
- 出网前的脱敏规则见上文，它覆盖不到 IPv6、内网域名、私钥块等 —— 涉及这些内容请自行
  规避或先改成本机占位符。
- 这条不是口头承诺：CI 有门禁（`scripts/check-no-telemetry.sh`），在参与构建的
  `src` / `crates` / `tests` / `ui` 里命中遥测 / 埋点 / 统计上报的特征词即构建失败。

## 硬件快照融合

Triage 的 system prompt 里带着监控侧边栏同源的本机快照：CPU、内存 / 交换、负载、
每个挂载点的用量、网络收发速率。所以 AI 不只是读到你的终端，还读到主机状态 ——
CPU 飙到 90% 或某个挂载点剩余不足 10% 时，打开抽屉就会自动以
`[AUTOMATIC TRIAGE]` 的方式起一轮排查，不用自己组织问题。

## MCP 反向调用

FengheShell 自带 MCP Server（对外给别的客户端用）。AI Copilot 则是同一 automation 层的
另一个客户端：`read_remote_file` 工具直接复用 `automation::read_text`，通过已保存的
会话（含跳板链）读远端文件，因此你可以在抽屉里问「看看 /var/log/nginx/error.log 最后
50 行错误」而不用自己去 SFTP 面板翻。

## Web 工具（web_search / web_fetch）

排障到一半需要查公开资料时，AI 可以直接用两个 web 工具（rssh 同款移植）：

- **web_search**：走 Exa / Parallel 的匿名 Hosted MCP。查询**发出前**先在本地过
  脱敏规则（内网 IP / Token / 密钥换成 `<REDACTED:…>`）—— 这是网络出站边界，
  不靠模型自律；提供方按「模型@端点」的哈希二选一（负载均衡），可用环境变量
  `FENGHESHELL_WEB_SEARCH_PROVIDER=exa|parallel` 固定。MCP 的文本响应原样喂给
  LLM，不做结构化解析。
- **web_fetch**：抓一个具体 URL 并转成 Markdown（HTML 全页转换，DOM 层丢弃
  script/style 等，script 文本永远不会变成 Markdown）。5 MiB 响应上限、
  64 KiB Markdown 截断；每次抓取都会弹出审批卡片，URL 对用户可见。
  出站边界另有一层硬校验：URL 里**字面写出**的非公网地址（loopback / 私网 /
  link-local 含 `169.254.169.254` 云元数据 / CGNAT / `localhost.local` 一类本地
  域名）在解析阶段就拒绝，重定向的**每一跳**和最终落点都复查同一规则；YOLO
  自动批准审批卡也绕不过它。只查字面量、不做 DNS 解析（fake-ip 代理下逐地址
  校验既无效又误杀），域名解析到私网 / DNS rebinding 属残余风险，见已知限制。

两个工具返回的内容都是**不可信外部数据**：引擎会在结果前加警告，提示注入面
（「忽略之前指令」之类的页面文本）由这层警告 + 模型指令共同防。

## 文件修改（match_file / patch_file）

排障经常要改配置（超时、开关、prometheus job…）。FengheShell **禁止一切自由形式
的写文件命令**（`>` / `>>` / `tee` / `cp` / `mv` / `sed -i` / `python` … 都会被
shape validator 拦下），唯一通路是 rssh 同款的两步工具（Agent 模式可用）：

| 工具 | 作用 | 审批 |
| --- | --- | --- |
| `match_file(path, find)` | 在远端文件里定位字面文本，回 `{count, matches:[{line, context}]}`（远端 cap 50 条） | 只读：开了「只读命令免确认执行」时自动放行，否则一张卡 |
| `patch_file(path, find, replace, expected_count)` | 把 `find` 全部替换为 `replace`，原子写回（tmp + mv），回 unified diff | 四张卡：cp → modify → diff → mv，每步都要点 |

安全设计与 rssh 一致：

- 命令由**后端模板**拼装（远端 `python3 -c $'…'`，perl 降级），不过 shape
  validator；LLM 可控的只有 `find` / `replace` 文本，且全部经 ANSI-C 引号编码
  （控制字符 hex 化，防终端转义注入；长多行脚本也因此保持单行，绕开 zsh ZLE 的
  multi-line quote race）。
- `patch_file` 落盘前重新读取文件并**校验出现次数**，与 `expected_count` 不符就
  拒绝（race / 陈旧防护）；替换内容与原内容相同时跳过 `mv`，不碰 mtime/inode。
- diff 为 0（无差异）时不覆盖文件，tmp 留给用户自己处置。
- 远端需要 `python3`（首选）或 `perl`；`patch_file` 另需 `diff`。连上后第一次使用
  时会在终端里跑一行只读探测（`echo "py3=… perl=… diff=…"`，不弹卡片），结果按轮次
  缓存。仅 POSIX shell（bash/zsh）支持 —— Windows / 串口 / Telnet 会话会得到明确错误。

## 技能（load_skill）

`general` 规则集（诊断方法论 + Java/Go 剧本）永远展开在 system prompt 里。用户还
可以把**自己的排查流程**写成 Skill，按需加载：

在 设置 → Interface → **技能** 页增删改查（M2 起有管理界面，不再手改配置；`id` 不能
是 `general`，内容上限 8 KB）。表单保存即写 `sessions.json` 的 `ai_skills`，
下一轮生效。字段形状大致是：

```json
"ai_skills": [
  {
    "id": "user-mysql",
    "name": "MySQL 慢查询排查",
    "description": "慢查询 / 锁等待 / 主从延迟的标准流程",
    "content": "1. show full processlist; 2. …"
  }
]
```

配置了之后，system prompt 会带上「Available skills」目录（id + 一句话描述），模型
判断匹配就调 `load_skill(id)` 拉全文。会话启动时快照进引擎，中途改配置文件不影响
正在跑的轮次。

## LLM 支持矩阵

| protocol 值 | 端点示例 | 说明 |
| --- | --- | --- |
| `openai-completions` | `https://api.openai.com/v1` | OpenAI 及任意 OpenAI 兼容服务（DeepSeek / Groq / Moonshot …） |
| `deepseek-thinking` | `https://api.deepseek.com` | 带 reasoning_content 回传 |
| `anthropic-messages` | `https://api.anthropic.com` | Claude Messages API |
| `ollama` | `http://localhost:11434/v1` | 本地模型，数据完全不出本机 |

## 已知限制

- 本地终端标签没有安装 hooks（`OSC 697` 是连远端 SSH 时装进去的），这类标签的命令块
  只能来自远端自身启用的 OSC 133，否则 AI 只看到你的提问文本。
- 「停止」会置位本轮的中断开关：引擎在下一个回合边界退出，客户端在下一个流式分块处
  放弃请求，命令等待也会立刻结束。已经生成的那一小段文本会被丢弃（generation 机制）。
- 串口 / Telnet 这类裸设备没有 shell，sentinel 无处附着，因此命令执行不适用，AI 只会
  给文本建议。
- match_file / patch_file 只支持 POSIX shell（bash/zsh）的远端，且需要 python3 或 perl
  （patch_file 另需 diff）；Windows / 串口 / Telnet 会话会拿到明确错误，应改用
  run_command 的 grep / head 只读查看。
- 用户自定义 Skill 有管理界面（设置 → Interface → 技能），但没有技能市场 / 导入 URL / 版本管理。
- web_search 的查询脱敏目前只走内置 8 条规则（内网 IP / Token / 密钥），用户自定义
  脱敏规则暂不参与；需要时可以经引擎透传。
- web_fetch 只拦截 URL 里**字面写出**的非公网 IP / 本地域名；一个公网域名若被
  DNS 解析到内网地址，或遭遇 DNS rebinding，字面量校验看不到 —— 这类场景仍靠
  审批卡片兜底，请勿在不可信页面上让模型抓取陌生域名，生产环境建议配合出站
  代理 / 防火墙策略。
- 审计日志只在进程内存里累积，不自动落盘；退出即清空，需要留痕请用设置页的导出按钮。

---

# AI Copilot (FengheShell × rssh)

English summary of the sections above.

## What it is

FengheShell supplies the "full-featured, low-overhead classic ops workbench"
(resource monitoring, multi-tab terminal, SFTP). rssh supplies the "AI ops Copilot
paradigm" (invisible terminal context capture, AI triage, local redaction,
side-effect-rated command execution). Both are Rust, so there is no FFI glue —
the AI side is injected as an independent crate.

## Usage

1. Click the five-pointed star on the terminal status strip (or press
   **Ctrl+Space**) to toggle the right-hand AI panel; click the header title for the
   conversation list, `+` / **Ctrl+Shift+O** (macOS `⌘⇧O`) for a new conversation.
   While open, the terminal area shrinks by the panel width (360 / 520) instead of
   being covered.
2. Configure the LLM under Settings -> Interface -> **AI Copilot** (or the gear in
   the drawer header): protocol /
   endpoint / model / API key, plus "Redact secrets locally" and
   "Auto-run read-only commands". Settings are stored in `sessions.json`
   under the `ai_*` fields; the API key is encrypted with ChaCha20-Poly1305
   like every other stored secret.
3. Ask a question, or ask nothing at all — when CPU / memory / disk cross the
   alert thresholds, opening the drawer starts a triage round automatically.
4. Suggested commands arrive as cards; the risk tier decides what you can do
   with them (see below).

## Multi-conversation panel (M1)

The panel is an **application-level right dock**, and conversations do not belong
to a terminal tab:

| Action | Behaviour |
| --- | --- |
| Close the panel / switch tabs / switch conversations / drag a tab to another window | **Never interrupts** a running round; messages keep landing in the Rust-side store and are replayed when you return |
| New conversation | Anchored to the active terminal tab (a text-only one on the welcome tab); the title is the first 30 characters of the first question |
| Close conversation | Deletes it; a running round is settled like "Stop" (approvals rejected, pending cards voided) |
| Anchor tab closed | The conversation survives, marked anchor-lost; later rounds run text-only and say so in the transcript |
| Concurrency | At most 3 conversations run at once (`ai_max_parallel`, 1-5 in `sessions.json`); sending beyond that is refused with a message, never queued |
| Execution on the same anchor | Per-tab execution lock: the second conversation gets `anchor_busy` and writes nothing into the PTY |
| App exit | Every running round is cancelled like "Stop" and the store is flushed synchronously |

Persistence: `ai_conversations.json` in the data directory (next to
`sessions.json`), 0600 with tmp/rename atomic writes, debounced 500 ms per round
plus an exit flush; **messages are redacted before they hit the disk**. On restart
conversations come back as `interrupted` and pending approval cards become
`expired` - the approval channel is a process-local `oneshot` and cannot cross
processes. The previous build never persisted conversations, so **nothing is
migrated**.

Ask / Agent is **per conversation**: switching the mode applies to the current
conversation, and the global `ai_mode` is only the default for new ones.

## Model profiles / memory / skills / history (M2)

M2 moves the things that used to live only in JSON into the settings pages, and adds
per-conversation model locking plus history management.

| Capability | Where | Behaviour |
| --- | --- | --- |
| Multiple model profiles | Settings -> Interface -> **Model profiles** | name / protocol / endpoint / model / API key each, one of them the default; the key is encrypted on disk and write-only |
| Per-conversation model lock | **Model chip** in the panel header | A conversation snapshots the default profile's name when created; changing the global default later does not affect it, and switching mid-run only applies to later rounds |
| Memory injection | Settings -> Interface -> **Memory** | One global list (content / source / toggle / timestamp); sources are `manual` and `ai-proposed`; injected as a leading system-prompt section after redaction; 500 characters per entry, 50 entries max |
| Per-conversation memory switch | Panel settings drawer | Turning it off stops memory injection for that conversation only |
| Skill management | Settings -> Interface -> **Skills** | Create, edit, delete user skills (unique id, never `general`, 8 KB cap); saving writes `ai_skills`; the built-in `general` set always applies |
| History search | Search box in the conversation list | Case-insensitive match over titles and message bodies, with the matching snippet on the row |
| Conversation export | Export button at the end of a list row | Redacted Markdown (command-card risk / gate / disposition, the Agent plan and step trail) - treat it as sensitive |
| Concurrency cap | Settings -> Interface -> AI Copilot | 1-5, stored as `ai_max_parallel`; sending beyond it is refused, never queued |
| Built-in MCP | Settings -> Interface -> MCP | Existing switches and permission matrix, plus a doc link (`docs/REMOTE_MCP.md`) and a "Settings" line showing the absolute path of `sessions.json` |

Memory is **never distilled automatically**: currently you can only write it
yourself. The `ai-proposed` source and its persistence path exist, but the
"AI proposes -> confirm in a card" interaction is **not implemented** yet (see F7
in AI_PANEL_REQUIREMENTS.md). Acting as an MCP **client** for external servers
stays in M3 behind its own RFC: [docs/AI_MCP_CLIENT_RFC.md](AI_MCP_CLIENT_RFC.md) is a draft and
**not implemented** - no external-server connection path may appear in the code
until the review passes.

## Working modes: Ask / Agent (mirroring GitHub Copilot's layers)

GitHub Copilot stacks three capabilities - completion, chat/edit, agent. Mapped
onto an ops terminal they become two explicit modes:

| Copilot layer | FengheShell mode | Behaviour |
| --- | --- | --- |
| Chat / Edit | **Ask** (default) | Explain, locate the cause, suggest commands; 2 tool turns, never acts on its own |
| Agent | **Agent** | Plan -> gather evidence -> act -> verify -> report; 6 tool turns, every step leaves a trace |

Both modes share the same tools and Run Gate - Agent never bypasses approval,
amber/red commands are still only filled into the terminal. The difference is the
**turn budget** and the **system prompt** (Agent additionally gets the
"gather first, act second, verify last" working style).

The starred step rows in the drawer are the Agent's tool calls: what it probed and
which step failed, so the user does not have to trust the conclusion alone.

Entry point: the **five-pointed star** on the right of the terminal status strip
(filled with the accent colour while open), or **Ctrl+Space**. An empty session
offers three starter prompts, all read-only first. **Test connection** in the
settings block issues a single `/models` request with the typed values - it writes
nothing to disk.

### Plan checklist (Agent)

In Agent mode the model first publishes a plan through the `set_plan` tool (up to 8
steps). The drawer renders it as a checklist whose state advances as it works:

| State | Icon | Meaning |
| --- | --- | --- |
| `pending` | empty box | not reached yet |
| `running` | star (accent) | executing now |
| `done` | checked box (green) | its tool call succeeded |
| `failed` | dash box (red) | its tool call failed or was refused |

Progress follows "the n-th action tool call completes step n" - a deliberate
simplification, since asking the model to report progress per step is verbose and
unreliable. The checklist shows progress only; the authoritative result is still the
command cards and the step log.

### Completion layer (ghost text in the command box)

Mirrors Copilot's first layer: while you type in the command bar, the model draws what
should come next as grey ghost text after the caret. Tab accepts it, Esc dismisses the
suggestion without losing focus.

- Requests are debounced by 400 ms and skipped below 3 characters; with no API key
  configured the layer stays off entirely
- The last 4 executed commands ride along as context, but the request carries no tools:
  completion can never execute anything (its runtime refuses every call)
- max_tokens is 64, so a completion costs tens of tokens; it gives up silently after a
  10 s timeout
- Every keystroke invalidates the previous response (sequence guard), so a stale
  suggestion can never be painted over new input

## Context capture


The terminal is an unlabelled character stream. Before AI can diagnose anything,
it has to know which part was the command and which part was its output.
`crates/ai-copilot/src/blocks.rs` runs two strategies over the stream on the
session pump thread:

- **OSC 133 semantic blocks** (the iTerm2 / VSCode / Kitty protocol):
  `ESC ] 133 ; A/B/C/D` boundaries cut "command + output + exit code".
- **FengheShell shell hook fallback**: FengheShell installs a hook in the remote
  shell after login; every command reports `OSC 697` at precmd (event
  `SessionEvent::CommandRan`), which we use to cut blocks. The exit code is
  unknown in this mode, so the AI sees `[exit ?]`.

Both paths `strip_ansi`-clean CSI / OSC noise and cap the window at the last
8 KB, so `tail -f` cannot blow up the token budget.

## Redaction and restoration

Before anything leaves the machine, `redact` replaces what the built-in eight
rules match with numbered placeholders (`<INTERNAL_IP_1>`). The rule set is the
same as upstream rssh's seed: private IPs (`10.x` / `172.16-31.x` / `192.168.x`),
`Bearer` headers, `sk-` keys, `AKIA` AWS keys, JWTs (`eyJ…`) and long hex
strings (32+ chars - hashes / session ids). **Not covered**: IPv6, internal
hostnames, private-key blocks and sensitive `export` fields — those leave the
machine verbatim, so avoid them in questions or pre-replace them yourself.

## Run gate

Every proposed command crosses four walls, and failing any of them means it
never touches your box:

| Wall | Effect |
| --- | --- |
| 1. shape validator | hard block by blacklist + per-command rules (`rm`, `bash -c`, `chmod -R`, write redirects, unbounded loops, …) |
| 2. risk tier | `ReadOnly` green / `Mutating` amber / `Destructive` red |
| 3. human in the loop | green passes automatically when auto-run is enabled; everything else waits for your click |
| 4. visible execution | the command goes to the **PTY you are looking at**, and the exit code comes back through an injected sentinel echo |

Red: `systemctl stop`, `git reset --hard`, `useradd`, `chmod 777` …
Amber: `systemctl restart`, `apt install`, `docker image prune` …
Green: `cat`, `df -h`, `ps aux --sort=-%cpu | head`, `journalctl -u nginx` …

Approval requests time out after 180 s; a timeout or a closed drawer counts as a
rejection, so the AI never wedges.

`run_command` may carry `timeout_s` (1..=300 s): the LLM's requested wait is
forwarded to the host; without it the host default (30 s) applies. file_ops
backend-template commands never take an LLM timeout.

## YOLO mode (full trust)

The **YOLO mode (full trust)** switch on the Settings -> Interface -> AI Copilot
page (off by default, applied instantly and synced across windows) removes every
guard rail described above at once:

- **Redaction off**: system prompt, terminal context, command output and fetched
  pages no longer pass through the redaction rules - secrets and tokens visible
  in your terminal are sent verbatim to whichever model provider you configured.
- **Shape validator skipped**: `rm -rf`, unbounded loops, write redirects and the
  rest are accepted as they come.
- **No approvals at all**: read-only, mutating and destructive commands all run
  directly; `run_command`, the four file_ops steps and `web_fetch` stop showing
  cards, and "auto-run read-only" is superseded.
- **Audit trail kept**: each round starts with a `yolo mode active` note, and
  executions are recorded as usual, so an export shows everything that happened
  while the mode was on.

A red warning banner stays pinned to the top of the drawer while the mode is on.
Good uses: controlled environments (containers, VMs you can snapshot), fully
local models (e.g. Ollama), or rehearsals where you want the AI to work
unsupervised. Do not enable it around production credentials or irreversible
operations. Turning it off restores every guard rail immediately.

## Audit log

Every diagnosis round records an audit inside the engine (same event points as
rssh): session start / user question / each LLM request with its token counts /
command proposed, rejected, blocked and executed (with redacted output, byte
counts and duration) / skill loads / web_search and web_fetch completions /
notes and errors.

- It accumulates in **process memory** only and is never written automatically;
  exiting the app clears it.
- Entries are written as they happen, so a round the user stops mid-way still
  keeps everything that already occurred.
- Output honours the round's "Redact secrets locally" switch, but still contains
  commands and output - treat an export as a sensitive file.
- **Export audit log** on the Settings -> Interface -> **AI Copilot** page saves
  it as text (`fengheshell-ai-audit.log`), grep / less friendly.

## Privacy and telemetry

FengheShell has **no telemetry**: no usage statistics, no event tracking, no crash
reporting and no behavioural collection. Terminal content, command history and
conversation data never go to any server we control.

- Only three kinds of requests ever leave the machine, and each is triggered by your
  own action: the LLM endpoint you configured, the target site of a `web_search` /
  `web_fetch` call, and the optional update check (which only queries GitHub
  releases for tags and assets, and downloads installers solely from this
  repository's `https://github.com` path with a SHA-256 digest).
- The audit log accumulates in **process memory** only and is cleared on exit; it
  reaches the disk only when you press export.
- Conversation history (`ai_conversations.json`) is written **locally** in the data
  directory (0600, atomic write, redacted before it hits the disk).
- The CPU / memory / network figures in the resource sidebar and the system info
  window are a **local display** and are never sent anywhere; the hardware snapshot
  attached to a question only enters that round's prompt.
- What redaction does and does not cover is listed above (IPv6, internal hostnames
  and private-key blocks are out of scope) - avoid those, or replace them with local
  placeholders first.
- This is enforced, not just promised: a CI gate (`scripts/check-no-telemetry.sh`)
  fails the build if a telemetry / tracking / analytics keyword appears anywhere in
  the built sources (`src` / `crates` / `tests` / `ui`).

## Hardware snapshot fusion

The triage system prompt carries the same host snapshot the resource sidebar shows:
CPU, memory / swap, load, per-mount usage and network throughput. So the AI reads
your machine state as well as your terminal - when CPU passes 90% or a mount drops
below 10% free, opening the drawer starts a triage round on its own as
`[AUTOMATIC TRIAGE]`.

## MCP, the other direction

FengheShell ships an MCP **server** (for other clients). AI Copilot is another client
of the same automation layer: the `read_remote_file` tool reuses
`automation::read_text` over a saved session (jump-host chains included), so you can
ask "show me the last 50 error lines of /var/log/nginx/error.log" instead of digging
through the SFTP panel.

## Web tools (web_search / web_fetch)

When a diagnosis needs public references, the model can reach two web tools (ported
from rssh):

- **web_search** goes through the anonymous Hosted MCP of Exa / Parallel. The query
  is redacted locally (private IPs / tokens / keys become `<REDACTED:…>`) **before**
  it leaves the device — that is the network egress boundary, not model discipline.
  The provider is picked by hashing "model@endpoint" (load balancing) and can be
  pinned with `FENGHESHELL_WEB_SEARCH_PROVIDER=exa|parallel`. The MCP text response
  is fed to the LLM verbatim, without structured parsing.
- **web_fetch** fetches one exact URL and converts it to Markdown (full-page
  conversion; script/style are dropped at DOM level, so script text never becomes
  Markdown). 5 MiB response cap, 64 KiB Markdown truncation, and every fetch shows
  the user an approval card with the URL visible. A hard egress check also refuses
  **literal** non-public hosts (loopback / private / link-local incl. the
  `169.254.169.254` metadata endpoint / CGNAT / `localhost.local`-style domains)
  at parse time, and re-checks every redirect hop and the final URL; YOLO's
  auto-approved card cannot bypass it. It inspects literals only, without DNS
  resolution (per-address checks are ineffective and break real usage behind a
  fake-ip proxy), so a public domain resolving to a private address / DNS
  rebinding remains a residual risk covered by the approval card.

Both tools return **untrusted external data**: the engine prefixes the result with a
warning, and that warning plus the system prompt together cover the prompt-injection
surface (page text saying "ignore previous instructions").

## File modification (match_file / patch_file)

Diagnosis often ends in a config change (a timeout, a flag, a prometheus job…).
FengheShell **forbids every free-form file write** (`>` / `>>` / `tee` / `cp` / `mv` /
`sed -i` / `python` … are all rejected by the shape validator); the only path is the
two-tool workflow ported from rssh (Agent mode):

| Tool | Effect | Approval |
| --- | --- | --- |
| `match_file(path, find)` | Locates literal text in a remote file, returns `{count, matches:[{line, context}]}` (remote cap 50) | read-only: auto-approved when "auto-run read-only" is on, otherwise one card |
| `patch_file(path, find, replace, expected_count)` | Replaces every `find` with `replace`, writes back atomically (tmp + mv), returns the unified diff | four cards: cp → modify → diff → mv, each needs a click |

Safety mirrors rssh:

- The commands are assembled from **backend templates** (remote `python3 -c $'…'`,
  perl as fallback) and skip the shape validator; the LLM only controls `find` /
  `replace`, which go through ANSI-C quoting (control characters hex-encoded against
  terminal escape injection; long multi-line scripts stay single-line, avoiding the
  zsh ZLE multi-line quote race).
- `patch_file` re-reads the file before writing and **verifies the occurrence count**
  against `expected_count` (race / staleness guard); a replacement identical to the
  original skips the `mv` so mtime/inode stay untouched.
- diff exit 0 (no difference) leaves the file alone and the tmp for the user.
- The remote needs `python3` (preferred) or `perl`; `patch_file` additionally needs
  `diff`. The first use runs one read-only probe in the terminal
  (`echo "py3=… perl=… diff=…"`, no card), cached per round. POSIX shells (bash/zsh)
  only — Windows / serial / Telnet sessions get a clear error.

## Skills (load_skill)

The `general` rule set (methodology + Java/Go playbooks) is always expanded into the
system prompt. Users can additionally write **their own runbooks** as skills, loaded
on demand:

Create, edit and delete them on the Settings -> Interface -> **Skills** page (M2
added the management UI, so hand-editing the config is no longer required; `id` must
not be `general`, content caps at 8 KB). Saving writes the `ai_skills` array to
`sessions.json`, which looks like this:

```json
"ai_skills": [
  {
    "id": "user-mysql",
    "name": "MySQL slow-query triage",
    "description": "Standard flow for slow queries / lock waits / replica lag",
    "content": "1. show full processlist; 2. …"
  }
]
```

With skills configured, the system prompt carries an "Available skills" catalog
(id + one-line description) and the model calls `load_skill(id)` when one matches.
The catalog is snapshotted into the engine at round start; editing the file mid-round
does not affect a running round.

## Known limitations

- Local terminal tabs have no hook (`OSC 697` is installed into remote shells over
  SSH), so their context relies on the remote shell emitting OSC 133 — otherwise
  the AI only sees your question.
- "Stop" flips the round's cancel flag: the engine exits at the next turn boundary, the
  client gives up at the next streamed chunk, and any command wait ends immediately. The
  text already generated is dropped (generation counter).
- Raw-device sessions (serial / Telnet) have no shell to carry a sentinel, so command
  execution does not apply there; the AI answers in prose.
- match_file / patch_file support POSIX shells (bash/zsh) only and need python3 or perl
  on the remote (patch_file additionally `diff`); Windows / serial / Telnet sessions
  get a clear error and should fall back to read-only run_command (grep / head).
- User-defined skills have a management UI (Settings -> Interface -> Skills), but
  there is no skill marketplace, import URL or versioning.
- web_search query redaction currently uses the built-in eight rules only (private
  IPs / tokens / keys); user-defined redaction rules do not participate yet.
- web_fetch blocks only **literal** non-public IPs / local domains. A public domain
  that resolves to a private address (or DNS rebinding) is invisible to that check
  and still relies on the approval card; pair it with an egress proxy / firewall
  policy in production.
- The audit log lives in process memory only and is never written automatically;
  it clears on exit - use the export button on the settings page to keep a copy.
