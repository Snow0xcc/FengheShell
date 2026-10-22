//! AI Copilot：终端上下文采集 + Triage 编排 + Human-in-the-loop Run Gate。
//!
//! 这一层是 FengheShell 与 ai-copilot crate 之间唯一的粘合面：
//!
//! - 每个终端标签一份 [`TabContext`]，从会话输出流里抽取命令块（OSC 133 优先，
//!   FengheShell 自带的 OSC 697 shell hook 兜底），只保留最近 8KB 上下文；
//!   它只读终端，不含任何对话状态；
//! - 会话（消息 / 状态 / 审批 / 中断开关）在 [`crate::app::ai_store`] 里，
//!   生命周期独立于标签 —— 面板收起、切标签、换窗口都不打断正在跑的一轮；
//! - [`UiRuntime`] 实现 ai-copilot 的 `ToolRuntime`：把 AI 提议的命令写回
//!   **用户可见的 PTY**，退出码靠注入的 sentinel 回声抓取；
//! - Run Gate 的三档裁决决定命令是「回车执行」还是「仅填入终端」。
//!   没有任何后台静默执行路径 —— 所有副作用都在看得见的终端里发生。

use std::rc::Rc;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use ai_copilot::blocks::{
    strip_ansi, BlockParser, CommandBlock, ContextWindow, DEFAULT_CONTEXT_BYTES,
};
use ai_copilot::llm::ChatDelta;
use ai_copilot::risk::{Gate, RiskLevel};
use ai_copilot::shell::ShellKind;
use ai_copilot::triage::{
    sentinel_wrap, CommandOutcome, CopilotConfig, DiskUsage, Disposition, NetRate, ProposedCommand,
    SystemSnapshot as AiSnapshot, ToolRuntime, TriageContext, TriageEngine, Trigger,
};
use ai_copilot::{AppError, AppResult};
use async_trait::async_trait;
use chrono::Utc;
use slint::{ModelRc, VecModel};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;

use crate::app::ai_store::ConversationStoreHandle;
use crate::config::Session;
use crate::resource::SystemSnapshot;
use crate::ssh::SessionCommand;
use crate::ui::{AiMessage, AppWindow};

/// Run Gate 等待用户裁决的上限。超时按「拒绝」处理，AI 侧会收到拒绝理由。
pub(super) const APPROVAL_TIMEOUT: Duration = Duration::from_secs(180);
/// 单条命令在 PTY 里的等待上限。
pub(super) const EXEC_TIMEOUT: Duration = Duration::from_secs(30);
/// 送进 prompt 的最大命令块数量（8KB 预算内，够用且便宜）。
pub(super) const MAX_CONTEXT_BLOCKS: usize = 8;
/// 注入 PTY 的 sentinel marker 前缀；带它的命令不进 AI 上下文。
pub(super) const AI_SENTINEL_PREFIX: &str = "__msai_";
/// 新会话还没有首条提问时的占位标题。
pub(super) const UNTITLED_TITLE: &str = "New conversation";

// ─── 每标签的 AI 上下文 ────────────────────────────────────────────

/// 一次带 sentinel 的命令执行捕获。
struct ExecCapture {
    marker: String,
    output: String,
    exit: Option<i32>,
    done: bool,
}

impl ExecCapture {
    /// 在已归一化的文本里寻找 sentinel 行 `<marker>:<exit>`。
    fn feed(&mut self, clean: &str) {
        let mut work = clean.to_string();
        if let Some(pos) = work.find(&self.marker) {
            let marker_end = pos + self.marker.len();
            let tail = &work[marker_end..];
            self.exit = tail
                .trim_start_matches(':')
                .lines()
                .next()
                .and_then(|l| l.trim().parse::<i32>().ok());
            // 丢掉 sentinel 行本身：它不是用户该看到的输出。
            let keep = tail
                .find('\n')
                .map(|i| marker_end + i + 1)
                .unwrap_or(work.len());
            let after = work[keep.min(work.len())..].to_string();
            work.truncate(pos);
            work.push_str(&after);
            self.done = true;
        }
        self.output.push_str(&work);
    }
}

/// 终端侧的只读上下文 + 执行态。会话从这里取快照，自己不再持有命令块。
pub(crate) struct TabContext {
    parser: BlockParser,
    blocks: ContextWindow,
    /// 自上一个块边界以来累积的输出（OSC 697 兜底路径才会用到）。
    hook_out: String,
    /// 见到过 OSC 133 就永远走语义块解析，不再用 hook 兜底（避免双计数）。
    saw_osc133: bool,
    exec: Option<ExecCapture>,
    /// 同锚点执行锁：两个会话同时往同一 PTY 写命令 + 装 sentinel 会互相踩。
    exec_lock: Arc<tokio::sync::Mutex<()>>,
    /// 补全请求序号：输入每变一次就自增，旧的补全响应据此作废。
    completion_seq: u64,
}

pub(crate) type TabContextHandle = Arc<Mutex<TabContext>>;

impl TabContext {
    pub(crate) fn new() -> Self {
        Self {
            parser: BlockParser::new(),
            blocks: ContextWindow::new(DEFAULT_CONTEXT_BYTES),
            hook_out: String::new(),
            saw_osc133: false,
            exec: None,
            exec_lock: Arc::new(tokio::sync::Mutex::new(())),
            completion_seq: 0,
        }
    }

    /// 同锚点串行化的那把锁（`run_with_sentinel` 期间持有）。
    pub(super) fn exec_lock(&self) -> Arc<tokio::sync::Mutex<()>> {
        self.exec_lock.clone()
    }

    /// 喂一段 PTY 输出（会话层已解码为文本）。由会话 pump 线程调用。
    pub(super) fn feed_output(&mut self, text: &str) {
        if !self.saw_osc133 && text.contains("]133;") {
            self.saw_osc133 = true;
        }
        for block in self.parser.feed(text) {
            self.blocks.push(block);
        }
        let clean = normalize_newlines(&strip_ansi(text));
        if clean.is_empty() {
            return;
        }
        if !self.saw_osc133 {
            self.hook_out.push_str(&clean);
        }
        if let Some(exec) = self.exec.as_mut() {
            exec.feed(&clean);
        }
    }

    /// FengheShell shell hook 捕获到一条执行过的命令（`SessionEvent::CommandRan`）。
    ///
    /// OSC 697 是在 precmd 里发出的，也就是**这条命令的输出已经打完之后**，
    /// 因此此刻累积的输出正属于 `cmd` 本身（不是上一条）。自己注入的 sentinel
    /// 命令不进上下文 —— 否则 AI 会看到一堆 `__msai_xxx:$?`。
    pub(super) fn note_command(&mut self, cmd: &str) {
        if self.saw_osc133 || cmd.trim().is_empty() || cmd.contains(AI_SENTINEL_PREFIX) {
            return;
        }
        let out = strip_echo(std::mem::take(&mut self.hook_out), cmd);
        let now = Utc::now();
        self.blocks.push(CommandBlock {
            command: cmd.trim().to_string(),
            output: out.trim_end().to_string(),
            exit_code: None,
            started_at: now,
            finished_at: Some(now),
            cwd: None,
            finished: true,
        });
    }

    /// 最近若干条命令块（最早 → 最新）。
    pub(super) fn context_blocks(&self, limit: usize) -> Vec<CommandBlock> {
        let all = self.blocks.blocks();
        all.iter()
            .skip(all.len().saturating_sub(limit))
            .cloned()
            .collect()
    }

    /// Run Gate 第 4 道墙：开始监听一次带 sentinel 的执行。
    pub(super) fn arm_exec(&mut self, marker: &str) {
        self.exec = Some(ExecCapture {
            marker: marker.to_string(),
            output: String::new(),
            exit: None,
            done: false,
        });
    }

    /// 取出已完成（sentinel 已出现）的执行结果；没完成就保留监听状态。
    pub(super) fn take_finished_exec(&mut self) -> Option<(String, Option<i32>)> {
        let done = self.exec.as_ref().map(|e| e.done).unwrap_or(false);
        if !done {
            return None;
        }
        self.exec.take().map(|e| (e.output, e.exit))
    }

    /// 放弃监听（超时 / 会话断开）。
    pub(super) fn clear_exec(&mut self) {
        self.exec = None;
    }
}

/// PTY 走 CRLF；送给 LLM 之前统一成 LF，省下一批毫无信息量的回车。
fn normalize_newlines(s: &str) -> String {
    s.replace("\r\n", "\n")
}

/// 去掉 PTY 回显的命令本身，只留下它的输出。
fn strip_echo(out: String, cmd: &str) -> String {
    let cmd = cmd.trim();
    if cmd.is_empty() {
        return out;
    }
    let Some(pos) = out.find(cmd) else {
        return out;
    };
    if !out[..pos]
        .chars()
        .all(|c| matches!(c, '\r' | '\n' | ' ' | '\0'))
    {
        return out;
    }
    out[pos + cmd.len()..]
        .trim_start_matches(['\r', '\n'])
        .to_string()
}

// ─── 面板内容的 Slint 镜像 ────────────────────────────────────────

/// 把一份消息副本刷进当前窗口的 AI 面板。
///
/// 只有「该窗口正在看这条会话」时才刷 —— 后台会话每 token 克隆整份消息会
/// 把前台拖卡；切回时 `apply_conversation` 一次性重放。
fn sync_messages(weak: &slint::Weak<AppWindow>, conversation_id: &str, rows: Vec<AiMessage>) {
    let weak = weak.clone();
    let conversation_id = conversation_id.to_string();
    let _ = slint::invoke_from_event_loop(move || {
        let Some(win) = weak.upgrade() else {
            return;
        };
        if win.get_ai_active_id().as_str() != conversation_id {
            return;
        }
        win.set_ai_messages(ModelRc::from(Rc::new(VecModel::from(rows))));
    });
}

/// 忙碌状态同步到面板（只在该会话正被查看时生效）。
fn sync_busy(weak: &slint::Weak<AppWindow>, conversation_id: &str, busy: bool) {
    let weak = weak.clone();
    let conversation_id = conversation_id.to_string();
    let _ = slint::invoke_from_event_loop(move || {
        let Some(win) = weak.upgrade() else {
            return;
        };
        if win.get_ai_active_id().as_str() != conversation_id {
            return;
        }
        win.set_ai_busy(busy);
    });
}

/// 在会话的消息向量上做一次原子修改，并把结果镜像给 Slint。
///
/// `generation` 为 None 表示无条件写入（UI 侧操作）；否则只在世代匹配时写入
/// —— 用户点了「停止」之后，在飞的那次 ask() 就再也不会改动面板。
pub(super) fn with_messages(
    conversations: &ConversationStoreHandle,
    weak: &slint::Weak<AppWindow>,
    conversation_id: &str,
    generation: Option<u64>,
    f: impl FnOnce(&mut Vec<AiMessage>),
) {
    let rows = {
        let mut guard = conversations.lock().unwrap_or_else(|e| e.into_inner());
        let Some(conv) = guard.get_mut(conversation_id) else {
            return;
        };
        if generation.is_some_and(|g| g != conv.generation) {
            return;
        }
        f(&mut conv.messages);
        conv.updated_at = Utc::now();
        conv.messages.clone()
    };
    sync_messages(weak, conversation_id, rows);
}

fn risk_tag(r: RiskLevel) -> &'static str {
    match r {
        RiskLevel::ReadOnly => "read-only",
        RiskLevel::Mutating => "mutating",
        RiskLevel::Destructive => "destructive",
    }
}

fn gate_tag(g: Gate) -> &'static str {
    match g {
        Gate::DirectRun => "direct",
        Gate::FillTerminal => "fill",
        Gate::Blocked => "blocked",
    }
}

fn disposition_tag(d: Disposition) -> &'static str {
    match d {
        Disposition::Pending => "pending",
        Disposition::Executed => "executed",
        Disposition::FilledForManualRun => "filled",
        Disposition::Rejected => "rejected",
        Disposition::Blocked => "blocked",
    }
}

/// 已存在同 id 的卡片就更新（ disposition 会随执行结果变化），否则追加。
fn upsert_card(rows: &mut Vec<AiMessage>, card: AiMessage) {
    if let Some(row) = rows
        .iter_mut()
        .find(|m| !m.card_id.is_empty() && m.card_id == card.card_id)
    {
        *row = card;
    } else {
        rows.push(card);
    }
}

pub(super) fn message_card(p: &ProposedCommand) -> AiMessage {
    AiMessage {
        role: ROLE_CARD,
        text: String::new().into(),
        card_id: p.id.clone().into(),
        cmd: p.cmd.clone().into(),
        explain: p.explain.clone().into(),
        side_effect: p.side_effect.clone().into(),
        risk: risk_tag(p.risk).into(),
        gate: gate_tag(p.gate).into(),
        disposition: disposition_tag(p.disposition).into(),
        blocked_reason: p.blocked_reason.clone().unwrap_or_default().into(),
        step_failed: false,
        step_state: String::new().into(),
    }
}

/// 计划清单的一行。id 稳定（plan-<index>），所以引擎推送新快照时是原地更新，
/// 而不是越堆越长。
fn message_plan_step(index: usize, step: &ai_copilot::triage::PlanStep) -> AiMessage {
    AiMessage {
        role: ROLE_PLAN_STEP,
        text: step.text.clone().into(),
        card_id: format!("plan-{index}").into(),
        step_state: step.state.as_str().into(),
        ..Default::default()
    }
}

/// 把整份计划快照写进对话：已存在的步骤原地更新状态，新增的追加。
fn sync_plan_rows(rows: &mut Vec<AiMessage>, plan: &[ai_copilot::triage::PlanStep]) {
    for (index, step) in plan.iter().enumerate() {
        let row = message_plan_step(index, step);
        if let Some(existing) = rows.iter_mut().find(|m| m.card_id == row.card_id) {
            *existing = row;
        } else {
            rows.push(row);
        }
    }
}

/// 补全缓存是进程级的：所有标签/窗口共用一份，命中就不再打模型。
pub(super) fn completion_cache() -> Arc<ai_copilot::completion::CompletionCache> {
    static CACHE: OnceLock<Arc<ai_copilot::completion::CompletionCache>> = OnceLock::new();
    CACHE
        .get_or_init(|| Arc::new(ai_copilot::completion::CompletionCache::new()))
        .clone()
}

/// 清空补全缓存（改了模型/端点之后调用，避免旧结果继续命中）。
pub(super) fn clear_completion_cache() {
    completion_cache().clear();
}

/// 进程级审计日志（rssh session 级 audit 在 FengheShell 的对应物）。
/// 引擎每轮 ask() 实时双写进来（含被停止 / 出错的轮次），设置页可导出。
pub(super) fn audit_log() -> Arc<std::sync::Mutex<ai_copilot::audit::AuditLog>> {
    static LOG: OnceLock<Arc<std::sync::Mutex<ai_copilot::audit::AuditLog>>> = OnceLock::new();
    LOG.get_or_init(|| Arc::new(std::sync::Mutex::new(ai_copilot::audit::AuditLog::default())))
        .clone()
}

/// 取这个标签的补全序号；同时把「已显示的补全」作废（输入变了）。
pub(super) fn completion_request(ctx: &TabContextHandle) -> u64 {
    match ctx.lock() {
        Ok(mut g) => {
            g.completion_seq += 1;
            g.completion_seq
        }
        Err(e) => {
            let mut g = e.into_inner();
            g.completion_seq += 1;
            g.completion_seq
        }
    }
}

/// 只有最新一次请求的响应才允许写回 UI。
pub(super) fn completion_is_current(ctx: &TabContextHandle, seq: u64) -> bool {
    ctx.lock().map(|g| g.completion_seq == seq).unwrap_or(false)
}

/// 写回补全结果（空串表示清空）。
pub(super) fn set_completion(weak: &slint::Weak<AppWindow>, tab_id: &str, text: &str) {
    let value = text.to_string();
    let weak = weak.clone();
    let tab_id = tab_id.to_string();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(win) = weak.upgrade() {
            crate::app::panes::set_terminal_row(&win, &tab_id, |row| {
                row.ai_completion = value.clone().into()
            });
        }
    });
}

/// 把一次工具调用渲染成 Copilot 那样的步骤行。
fn message_step(entry: &ai_copilot::triage::ToolLogEntry) -> AiMessage {
    AiMessage {
        role: ROLE_STEP,
        text: entry.summary.clone().into(),
        step_failed: entry.is_error,
        step_state: String::new().into(),
        ..Default::default()
    }
}

/// AiMessage.role 取值：用户 / 助手 / 命令卡片 / 错误。
pub(super) const ROLE_USER: i32 = 0;
pub(super) const ROLE_ASSISTANT: i32 = 1;
pub(super) const ROLE_CARD: i32 = 2;
pub(super) const ROLE_ERROR: i32 = 3;
/// Agent 模式下的步骤痕迹（一次工具调用一行）。
pub(super) const ROLE_STEP: i32 = 4;
/// Agent 模式的计划清单条目（复选框行）。
pub(super) const ROLE_PLAN_STEP: i32 = 5;

/// 面板里追加一条纯文本消息（摘要 / 错误提示 / 锚点失效提示）。
pub(super) fn push_notice(
    conversations: &ConversationStoreHandle,
    weak: &slint::Weak<AppWindow>,
    conversation_id: &str,
    role: i32,
    text: &str,
) {
    let msg = AiMessage {
        role,
        text: text.to_string().into(),
        ..Default::default()
    };
    with_messages(conversations, weak, conversation_id, None, |v| v.push(msg));
}

/// 清空会话记录（世代递增，令在飞的回复自行丢弃）。
pub(super) fn clear_conversation(
    conversations: &ConversationStoreHandle,
    weak: &slint::Weak<AppWindow>,
    id: &str,
) {
    let rows = {
        let mut guard = conversations.lock().unwrap_or_else(|e| e.into_inner());
        let Some(conv) = guard.get_mut(id) else {
            return;
        };
        conv.cancel_generation();
        conv.messages.clear();
        guard.touch();
        Vec::new()
    };
    sync_messages(weak, id, rows);
    sync_busy(weak, id, false);
}

/// 作废当前轮次：结清等待中的审批并让在飞的 ask() 放弃 UI 写入。
pub(super) fn cancel_generation(conversations: &ConversationStoreHandle, id: &str) -> u64 {
    match conversations.lock() {
        Ok(mut g) => g
            .get_mut(id)
            .map(|c| c.cancel_generation())
            .unwrap_or_default(),
        Err(e) => e
            .into_inner()
            .get_mut(id)
            .map(|c| c.cancel_generation())
            .unwrap_or_default(),
    }
}

/// 某张卡片的 gate 与命令：UI 据此决定「批准执行」还是「仅填入终端」。
pub(super) fn card_info(
    conversations: &ConversationStoreHandle,
    id: &str,
    card_id: &str,
) -> Option<(String, String)> {
    let guard = conversations.lock().ok()?;
    guard
        .get(id)?
        .messages
        .iter()
        .find(|m| m.card_id == card_id)
        .map(|m| (m.gate.to_string(), m.cmd.to_string()))
}

pub(super) fn answer_approval(
    conversations: &ConversationStoreHandle,
    id: &str,
    card_id: &str,
    ok: bool,
) -> bool {
    match conversations.lock() {
        Ok(mut g) => g
            .get_mut(id)
            .map(|c| c.answer_approval(card_id, ok))
            .unwrap_or(false),
        Err(e) => e
            .into_inner()
            .get_mut(id)
            .map(|c| c.answer_approval(card_id, ok))
            .unwrap_or(false),
    }
}

/// 通过 tab 的投递路由取到它的终端上下文句柄（命令块 + 执行锁）。
pub(super) fn context_handle(core: &crate::app::AppCore, tab_id: &str) -> Option<TabContextHandle> {
    let routes = core.tab_routes.lock().ok()?;
    let route = routes.get(tab_id)?.clone();
    route.lock().ok().map(|g| g.context.clone())
}

// ─── ToolRuntime：AI 唯一的副作用出口 ─────────────────────────────

pub(super) struct UiRuntime {
    conversations: ConversationStoreHandle,
    conversation_id: String,
    weak: slint::Weak<AppWindow>,
    /// 本轮中断开关：置位后命令等待立刻结束（用户点了「停止」）。
    cancel: Arc<AtomicBool>,
    /// PTY 写通道。None = 这个会话没有可写入的终端（AI 只能聊天）。
    raw: Option<UnboundedSender<SessionCommand>>,
    /// 锚点标签的终端上下文（命令块窗口 + sentinel 执行捕获 + 执行锁）。
    context: Option<TabContextHandle>,
    /// 本次 ask() 的世代号；会话被停止 / 清空后其 UI 写入自动作废。
    generation: u64,
    /// 标签对应的会话定义（可能缺失：本地/串口标签没有 SSH 会话）。
    session: Option<Session>,
    jump: Vec<Session>,
    shell: ShellKind,
}

impl UiRuntime {
    /// 把一行文本写进 PTY —— 始终是用户看得见的那个终端。
    fn send_line(&self, line: &str) -> AppResult<()> {
        let mut bytes = line.as_bytes().to_vec();
        bytes.push(b'\n');
        let raw = self
            .raw
            .as_ref()
            .ok_or_else(|| AppError::pty("no_live_terminal", serde_json::json!({})))?;
        raw.send(SessionCommand::RawInput(bytes))
            .map_err(|_| AppError::pty("session_channel_closed", serde_json::json!({})))
    }

    /// 在 PTY 里跑一条带 sentinel 的命令并等待它的退出码 / 输出。
    ///
    /// `execute_approved`（审批过的卡片）与 `execute_internal`（file_ops 远端
    /// 能力探测，无卡片）共用这一条路径 —— 两者都发生在用户可见的终端里。
    ///
    /// 同锚点串行化：整个「arm → 写 PTY → 等结果」期间持有标签的执行锁。
    /// 拿不到锁就返回可识别的 `anchor_busy`，引擎按工具失败继续，第二个会话
    /// 一个字节都不会写进 PTY。
    async fn run_with_sentinel(&self, cmd: &str, timeout: Duration) -> AppResult<CommandOutcome> {
        let Some(context) = self.context.clone() else {
            return Err(AppError::pty(
                "anchor_lost",
                serde_json::json!({"reason": "no_live_terminal"}),
            ));
        };
        let lock = context
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .exec_lock();
        // 持有到本轮执行结束（作用域末尾 drop），别在 await 期间丢锁。
        let _exec_guard = match lock.try_lock() {
            Ok(g) => g,
            Err(_) => {
                return Err(AppError::pty(
                    "anchor_busy",
                    serde_json::json!({"reason": "another_conversation_is_executing"}),
                ))
            }
        };
        let marker = format!("{}{}", AI_SENTINEL_PREFIX, uuid::Uuid::new_v4().simple());
        let line = sentinel_wrap(cmd, self.shell, &marker);
        {
            let mut guard = context.lock().unwrap_or_else(|e| e.into_inner());
            guard.arm_exec(&marker);
        }
        self.send_line(&line)?;
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            tokio::time::sleep(Duration::from_millis(80)).await;
            // 用户点了「停止」：不再等这条命令的结果。
            if self.cancel.load(std::sync::atomic::Ordering::Relaxed) {
                let mut guard = context.lock().unwrap_or_else(|e| e.into_inner());
                guard.clear_exec();
                return Ok(CommandOutcome {
                    exit_code: None,
                    output: String::new(),
                    timed_out: true,
                });
            }
            let finished = {
                context
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take_finished_exec()
            };
            if let Some((output, exit_code)) = finished {
                return Ok(CommandOutcome {
                    exit_code,
                    output,
                    timed_out: false,
                });
            }
            if tokio::time::Instant::now() >= deadline {
                let mut guard = context.lock().unwrap_or_else(|e| e.into_inner());
                guard.clear_exec();
                let partial = String::new();
                return Ok(CommandOutcome {
                    exit_code: None,
                    output: partial,
                    timed_out: true,
                });
            }
        }
    }
}

#[async_trait]
impl ToolRuntime for UiRuntime {
    async fn request_approval(&self, p: &ProposedCommand) -> AppResult<bool> {
        let (tx, rx) = oneshot::channel();
        {
            let mut guard = self.conversations.lock().unwrap_or_else(|e| e.into_inner());
            let Some(conv) = guard.get_mut(&self.conversation_id) else {
                return Ok(false);
            };
            conv.set_approval(p.id.clone(), tx);
        }
        let card = message_card(p);
        with_messages(
            &self.conversations,
            &self.weak,
            &self.conversation_id,
            Some(self.generation),
            |v| upsert_card(v, card),
        );
        match tokio::time::timeout(APPROVAL_TIMEOUT, rx).await {
            Ok(Ok(ok)) => Ok(ok),
            // 通道关闭（会话被删 / 应用退出）与超时的处置一致：按拒绝处理。
            Ok(Err(_)) | Err(_) => Ok(false),
        }
    }

    async fn execute_approved(&self, p: &ProposedCommand) -> AppResult<CommandOutcome> {
        // LLM 可以在 run_command 里请求等待秒数（schema 已钳制 1..=300）；
        // 没给就沿用本宿主的默认上限。file_ops / web_fetch 卡片不带超时。
        let secs = p
            .timeout_s
            .map(|t| t.clamp(1, 300) as u64)
            .unwrap_or(EXEC_TIMEOUT.as_secs());
        self.run_with_sentinel(&p.cmd, Duration::from_secs(secs))
            .await
    }

    /// file_ops 的远端能力探测（python3 / perl / diff）：不弹卡片，但仍然写在
    /// 用户可见的 PTY 里，与 rssh 的 internal_command 一致。等待上限由调用方
    /// （引擎按探测命令的性质给定）传入，宿主不再私自加码。
    async fn execute_internal(&self, cmd: &str, timeout_s: u64) -> AppResult<CommandOutcome> {
        self.run_with_sentinel(cmd, Duration::from_secs(timeout_s))
            .await
    }

    async fn read_remote_file(&self, path: &str, max_bytes: usize) -> AppResult<String> {
        let Some(session) = self.session.clone() else {
            return Err(AppError::other(
                "no_session_for_remote_read",
                serde_json::json!({"path": path}),
            ));
        };
        let value = crate::automation::read_text(
            session,
            self.jump.clone(),
            path.to_string(),
            Duration::from_secs(20),
        )
        .await
        .map_err(|e| {
            AppError::other(
                "read_remote_file",
                serde_json::json!({"path": path, "error": e.to_string()}),
            )
        })?;
        let content = value
            .get("content")
            .and_then(|c| c.as_str())
            .unwrap_or_default();
        Ok(content.chars().take(max_bytes).collect())
    }
}

// ─── 一次 Triage ─────────────────────────────────────────────────

pub(super) struct AskParams {
    pub conversation_id: String,
    pub question: String,
    pub trigger: Trigger,
    pub conversations: ConversationStoreHandle,
    pub weak: slint::Weak<AppWindow>,
    /// PTY 写通道；没有就说明会话尚未连上（此时 Triage 仍然可以纯聊天）。
    pub raw: Option<UnboundedSender<SessionCommand>>,
    pub context: Option<TabContextHandle>,
    pub session: Option<Session>,
    pub jump: Vec<Session>,
    pub cfg: CopilotConfig,
    pub shell: ShellKind,
    /// FengheShell 资源监控侧栏的同源快照。
    pub local: Option<SystemSnapshot>,
}

/// 发起一轮 AI 诊断。返回后立即回到 UI：所有网络 / 工具执行都在 tokio 侧。
pub(super) fn spawn_ask(runtime: &tokio::runtime::Handle, params: AskParams) {
    let AskParams {
        conversation_id,
        question,
        trigger,
        conversations,
        weak,
        raw,
        context,
        session,
        jump,
        cfg,
        shell,
        local,
    } = params;

    // 先把用户提问与一条空的助手占位行压进会话，拿到流式更新的目标下标；
    // 同一时刻给会话开新的一轮（拿中断开关），世代号随之确定。
    let (generation, cancel, stream_index) = {
        let mut guard = conversations.lock().unwrap_or_else(|e| e.into_inner());
        let Some(conv) = guard.get_mut(&conversation_id) else {
            return;
        };
        let first_question = conv.messages.is_empty();
        conv.messages.push(AiMessage {
            role: ROLE_USER,
            text: question.clone().into(),
            ..Default::default()
        });
        conv.messages.push(AiMessage {
            role: ROLE_ASSISTANT,
            ..Default::default()
        });
        let stream_index = conv.messages.len() - 1;
        if first_question {
            conv.derive_title(&question);
        }
        let cancel = conv.begin_round();
        let generation = conv.generation;
        let rows = conv.messages.clone();
        guard.touch();
        drop(guard);
        sync_messages(&weak, &conversation_id, rows);
        (generation, cancel, stream_index)
    };
    // 锚点标签已经关了（或本来就没锚点）：会话照样能聊，但没有终端上下文。
    if context.is_none() {
        push_notice(
            &conversations,
            &weak,
            &conversation_id,
            ROLE_ERROR,
            crate::i18n::t(
                "这条会话没有可用的终端上下文，本轮只做纯文本分析。",
                "This conversation has no live terminal context; this round is text-only.",
            ),
        );
    }

    // 审计日志的目标标识要在 session 移交给 UiRuntime 之前取（本地标签没有会话）。
    let audit_target = session.as_ref().map(|s| format!("ssh:{}", s.name));
    let backend = Arc::new(UiRuntime {
        conversations: conversations.clone(),
        conversation_id: conversation_id.clone(),
        weak: weak.clone(),
        cancel: cancel.clone(),
        raw,
        context: context.clone(),
        generation,
        session,
        jump,
        shell,
    });
    let engine = match TriageEngine::new(cfg.clone(), backend)
        .map(|e| e.with_cancel(cancel).with_audit_sink(audit_log()))
    {
        Ok(e) => e,
        Err(e) => {
            push_notice(
                &conversations,
                &weak,
                &conversation_id,
                ROLE_ERROR,
                &format!("{e}"),
            );
            finish_round(&conversations, &conversation_id, generation);
            return;
        }
    };

    let blocks = context
        .as_ref()
        .and_then(|c| c.lock().ok().map(|g| g.context_blocks(MAX_CONTEXT_BLOCKS)))
        .unwrap_or_default();
    let ctx = TriageContext {
        question,
        blocks,
        system: local.as_ref().map(to_ai_snapshot),
        shell,
        trigger,
        target: audit_target,
    };

    let sink: ai_copilot::DeltaSink = Arc::new({
        let conversations = conversations.clone();
        let weak = weak.clone();
        let conversation_id = conversation_id.clone();
        move |delta| match delta {
            ChatDelta::Text(text) => {
                with_messages(
                    &conversations,
                    &weak,
                    &conversation_id,
                    Some(generation),
                    |v| {
                        if let Some(row) = v.get_mut(stream_index) {
                            row.text = format!("{}{}", row.text, text).into();
                            row.role = ROLE_ASSISTANT;
                        }
                    },
                );
            }
            // Agent 计划清单：整份快照替换，UI 看到的是实时进展。
            ChatDelta::Plan(plan) => {
                with_messages(
                    &conversations,
                    &weak,
                    &conversation_id,
                    Some(generation),
                    |v| {
                        sync_plan_rows(v, &plan);
                    },
                );
            }
            _ => {}
        }
    });

    sync_busy(&weak, &conversation_id, true);
    runtime.spawn(async move {
        let result = engine.ask(&ctx, Some(sink)).await;
        with_messages(
            &conversations,
            &weak,
            &conversation_id,
            Some(generation),
            |v| match result {
                Ok(outcome) => {
                    // 最终结论覆盖流式拼接结果（模型可能重发整段）。
                    if let Some(row) = v.get_mut(stream_index) {
                        row.text = outcome.text.clone().into();
                        row.role = ROLE_ASSISTANT;
                    }
                    // 收尾时用最终快照再刷一次，保证没有残留的「进行中」。
                    sync_plan_rows(v, &outcome.plan);
                    // Agent 模式：把取证/执行/验证的每一步都留在面板里，
                    // 用户不必信任结论 —— 他能看到过程。
                    for entry in &outcome.tool_log {
                        v.push(message_step(entry));
                    }
                    for proposal in &outcome.proposed {
                        upsert_card(v, message_card(proposal));
                    }
                }
                Err(e) => {
                    if let Some(row) = v.get_mut(stream_index) {
                        row.text = format!("{e}").into();
                        row.role = ROLE_ERROR;
                    }
                }
            },
        );
        if finish_round(&conversations, &conversation_id, generation) {
            sync_busy(&weak, &conversation_id, false);
        }
    });
}

/// 一轮结束：世代没变才落回 idle（用户中途点了停止的轮次保持 interrupted）。
fn finish_round(conversations: &ConversationStoreHandle, id: &str, generation: u64) -> bool {
    let mut guard = conversations.lock().unwrap_or_else(|e| e.into_inner());
    let done = guard
        .get_mut(id)
        .map(|c| c.finish_round(generation))
        .unwrap_or(false);
    guard.touch();
    done
}

#[cfg(test)]
#[path = "../../tests/app/ai_copilot/mod.rs"]
mod ai_copilot_tests;

/// FengheShell 本地监控快照 → AI 侧快照（同一批 CPU/内存/磁盘/网络数据）。
fn to_ai_snapshot(s: &SystemSnapshot) -> AiSnapshot {
    const MIB: u64 = 1024 * 1024;
    AiSnapshot {
        cpu_usage_pct: s.cpu_percent,
        mem_used_bytes: s.mem_used_mib * MIB,
        mem_total_bytes: s.mem_total_mib * MIB,
        swap_used_bytes: s.swap_used_mib * MIB,
        swap_total_bytes: s.swap_total_mib * MIB,
        load_avg: [0.0, 0.0, 0.0],
        disks: s
            .disks
            .iter()
            .map(|(mount, available, total)| DiskUsage {
                mount: mount.clone(),
                used_bytes: total.saturating_sub(*available),
                total_bytes: *total,
            })
            .collect(),
        nets: vec![NetRate {
            iface: "total".into(),
            rx_bytes_per_s: s.net_rx_per_sec,
            tx_bytes_per_s: s.net_tx_per_sec,
        }],
        // 本地视图只有整机指标；进程级快照让 AI 自己用 ps 去取更有价值。
        top_processes: Vec::new(),
        sampled_at: Some(Utc::now()),
    }
}
