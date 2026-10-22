//! AI Copilot 的会话存储（多会话骨架）。
//!
//! 面板从「每个终端标签一个抽屉」提升为应用级 dock 之后，会话就不能再挂在
//! [`crate::app::TabRoute`] 上：标签关掉、面板收起、窗口换了，会话都还得活着。
//! 这里的模型把两件事拆开：
//!
//! - [`Conversation`]：一条会话 —— 消息、状态、锚点（哪个终端标签给它上下文）、
//!   审批通道、中断开关。生命周期独立于标签。
//! - [`ConversationStore`]：进程级会话表 + `ai_conversations.json` 落盘
//!   （tmp + rename 原子写、unix 0600、损坏时保留原文件并回退为空表）。
//!
//! 终端侧只读上下文（命令块窗口、sentinel 执行捕获）仍按标签保存，见
//! [`crate::app::ai::TabContext`]；每轮开始时从锚点取快照。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use ai_copilot::redact::IndexedRedactor;
use ai_copilot::triage::CopilotMode;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

use crate::ui::AiMessage;

/// 会话状态。`running` 只在内存里存在 —— 重启后一律降级为 `interrupted`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConversationStatus {
    Idle,
    Running,
    Interrupted,
}

impl ConversationStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ConversationStatus::Idle => "idle",
            ConversationStatus::Running => "running",
            ConversationStatus::Interrupted => "interrupted",
        }
    }
}

/// 「该写盘了」的请求钩子（防抖 500ms + 退出 flush）。
///
/// 只在 UI 线程上触发（把写盘排进 Slint 计时器）；会话表本身要跨线程，
/// 所以钩子只放一个「通知事件循环」的 Send 闭包，不持有 `Rc`。
pub type PersistSink = Arc<dyn Fn() + Send + Sync>;

/// 会话的触发来源（只有自动排障需要按来源去重）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TriggerTag {
    /// 硬件越界时自动起的排查轮次。
    Monitor,
}

/// 一条 AI 会话。
pub struct Conversation {
    pub id: String,
    pub title: String,
    /// 锚点终端标签 id：会话的终端上下文（命令块、命令执行）从这里来。
    pub anchor_tab: Option<String>,
    /// 锚点标签已经被关闭 —— 会话保留，但后续轮次没有终端上下文。
    pub anchor_lost: bool,
    pub mode: CopilotMode,
    /// 触发来源；`None` = 用户自己提问的普通会话。
    pub trigger: Option<TriggerTag>,
    pub status: ConversationStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// 消息的权威副本；Slint 模型只是它的镜像。
    pub messages: Vec<AiMessage>,
    /// 本轮的中断开关；点「停止」置位。
    pub cancel: Option<Arc<AtomicBool>>,
    /// 等待用户裁决的卡片：卡片 id → 回复通道。
    pub approval: Option<(String, oneshot::Sender<bool>)>,
    /// 每次「停止 / 清空」自增：在飞的旧 ask() 看到世代变了就放弃 UI 写入。
    pub generation: u64,
}

impl Conversation {
    pub fn new(id: String, anchor_tab: Option<String>, mode: CopilotMode, title: String) -> Self {
        let now = Utc::now();
        Self {
            id,
            title,
            anchor_tab,
            anchor_lost: false,
            mode,
            trigger: None,
            status: ConversationStatus::Idle,
            created_at: now,
            updated_at: now,
            messages: Vec::new(),
            cancel: None,
            approval: None,
            generation: 0,
        }
    }

    pub fn is_busy(&self) -> bool {
        self.status == ConversationStatus::Running
    }

    pub fn begin_round(&mut self) -> Arc<AtomicBool> {
        let flag = Arc::new(AtomicBool::new(false));
        self.cancel = Some(flag.clone());
        self.status = ConversationStatus::Running;
        self.updated_at = Utc::now();
        flag
    }

    /// 「停止 / 清空」：作废当前轮次，结清等待中的审批，并中断在飞的请求。
    pub fn cancel_generation(&mut self) -> u64 {
        if let Some(flag) = self.cancel.take() {
            flag.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        if let Some((_, tx)) = self.approval.take() {
            let _ = tx.send(false);
        }
        self.status = if self.messages.is_empty() {
            ConversationStatus::Idle
        } else {
            ConversationStatus::Interrupted
        };
        self.generation += 1;
        self.updated_at = Utc::now();
        self.generation
    }

    /// 一轮正常结束：回到 idle（被取消的轮次不会走到这里）。
    pub fn finish_round(&mut self, generation: u64) -> bool {
        if generation != self.generation {
            return false;
        }
        self.status = ConversationStatus::Idle;
        self.cancel = None;
        self.updated_at = Utc::now();
        true
    }

    /// 登记一次等待用户裁决的卡片。上一张没回应就又来一张 —— 直接按拒绝结算。
    pub fn set_approval(&mut self, id: String, tx: oneshot::Sender<bool>) {
        if let Some((_, prev)) = self.approval.take() {
            let _ = prev.send(false);
        }
        self.approval = Some((id, tx));
    }

    pub fn answer_approval(&mut self, id: &str, ok: bool) -> bool {
        match self.approval.take() {
            Some((cur, tx)) if cur == id => {
                let _ = tx.send(ok);
                true
            }
            other => {
                self.approval = other;
                false
            }
        }
    }

    /// 标题默认取首条用户消息的前 30 字；显式重命名后不再自动改。
    pub fn derive_title(&mut self, first_user_text: &str) {
        if !self.title.trim().is_empty() && self.title != crate::app::ai::UNTITLED_TITLE {
            return;
        }
        let text: String = first_user_text.trim().chars().take(30).collect();
        if !text.is_empty() {
            self.title = text;
        }
    }
}

/// 落盘的单条消息。**不要**给 Slint 生成的 `AiMessage` 加 `serde` 派生。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredMessage {
    pub role: i32,
    #[serde(default)]
    pub text: String,
    #[serde(default, rename = "card_id")]
    pub card_id: String,
    #[serde(default)]
    pub cmd: String,
    #[serde(default)]
    pub explain: String,
    #[serde(default)]
    pub side_effect: String,
    #[serde(default)]
    pub risk: String,
    #[serde(default)]
    pub gate: String,
    #[serde(default)]
    pub disposition: String,
    #[serde(default)]
    pub blocked_reason: String,
    #[serde(default)]
    pub step_failed: bool,
    #[serde(default)]
    pub step_state: String,
}

impl From<&AiMessage> for StoredMessage {
    fn from(m: &AiMessage) -> Self {
        Self {
            role: m.role,
            text: m.text.to_string(),
            card_id: m.card_id.to_string(),
            cmd: m.cmd.to_string(),
            explain: m.explain.to_string(),
            side_effect: m.side_effect.to_string(),
            risk: m.risk.to_string(),
            gate: m.gate.to_string(),
            disposition: m.disposition.to_string(),
            blocked_reason: m.blocked_reason.to_string(),
            step_failed: m.step_failed,
            step_state: m.step_state.to_string(),
        }
    }
}

impl From<StoredMessage> for AiMessage {
    fn from(s: StoredMessage) -> Self {
        AiMessage {
            role: s.role,
            text: s.text.into(),
            card_id: s.card_id.into(),
            cmd: s.cmd.into(),
            explain: s.explain.into(),
            side_effect: s.side_effect.into(),
            risk: s.risk.into(),
            gate: s.gate.into(),
            // 恢复的会话里没人能再应答这张卡：pending → expired。
            disposition: if s.disposition == "pending" {
                "expired".to_string()
            } else {
                s.disposition
            }
            .into(),
            blocked_reason: s.blocked_reason.into(),
            step_failed: s.step_failed,
            step_state: s.step_state.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredConversation {
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub anchor_tab: Option<String>,
    #[serde(default)]
    pub anchor_lost: bool,
    #[serde(default = "default_mode")]
    pub mode: String,
    #[serde(default = "default_status")]
    pub status: String,
    #[serde(default)]
    pub created_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub updated_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub messages: Vec<StoredMessage>,
}

fn default_mode() -> String {
    "ask".to_string()
}

fn default_status() -> String {
    "idle".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredStore {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub active_id: String,
    #[serde(default)]
    pub conversations: Vec<StoredConversation>,
}

fn default_version() -> u32 {
    1
}

impl Default for StoredStore {
    fn default() -> Self {
        Self {
            version: 1,
            active_id: String::new(),
            conversations: Vec::new(),
        }
    }
}

/// 同时运行的会话数上限（设置里可调 1–5）。
pub const DEFAULT_MAX_PARALLEL: usize = 3;
/// 默认会话数量上限（超出淘汰最旧的非 running 会话）。
pub const MAX_CONVERSATIONS: usize = 100;
/// 单会话消息条数上限：超限淘汰最旧的**非卡片**消息（卡片是审计痕迹）。
pub const MAX_MESSAGES_PER_CONVERSATION: usize = 500;
/// 单会话文本上限（宽松按字节估算）：超出后同样从最旧的非卡片消息淘汰。
pub const MAX_TEXT_BYTES_PER_CONVERSATION: usize = 4 * 1024 * 1024;

/// 进程级会话表。
#[derive(Default)]
pub struct ConversationStore {
    pub conversations: Vec<Conversation>,
    /// 新建会话的默认选中项；每窗口的实际选中态存在 `WindowState`。
    pub active_id: String,
    path: Option<PathBuf>,
    /// 防抖落盘：每轮结束 / 结构性变更后调用。
    persist: Option<PersistSink>,
    /// 已排队一次写盘的标记，避免每轮都重复排队。
    persist_pending: bool,
}

impl ConversationStore {
    /// 只做内存管理的空表（测试与 headless 路径）。
    #[allow(dead_code)]
    pub fn in_memory() -> Self {
        Self::default()
    }

    /// 打开真实数据目录下的会话文件并恢复：`running → interrupted`、
    /// `pending → expired`、损坏文件留档不静默清空。
    pub fn load_default() -> Self {
        let mut store = Self::with_path(Some(Self::default_path()));
        store.load(None);
        store
    }

    /// 绑定落盘路径；`path = None` 时完全不落盘。
    pub fn with_path(path: Option<PathBuf>) -> Self {
        Self {
            path,
            ..Self::default()
        }
    }

    /// 默认落盘路径：`<data_dir>/ai_conversations.json`（与 sessions.json 同级）。
    pub fn default_path() -> PathBuf {
        crate::config::data_dir().join("ai_conversations.json")
    }

    /// 当前落盘路径（headless / 测试用）。
    #[allow(dead_code)]
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn set_persist_hook(&mut self, hook: Option<PersistSink>) {
        self.persist = hook;
    }

    pub fn get(&self, id: &str) -> Option<&Conversation> {
        self.conversations.iter().find(|c| c.id == id)
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut Conversation> {
        self.conversations.iter_mut().find(|c| c.id == id)
    }

    /// 正在运行的会话数（并发闸门用）。
    pub fn running_count(&self) -> usize {
        self.conversations.iter().filter(|c| c.is_busy()).count()
    }

    /// 新建会话并返回 id。`active_id` 同步指向它（新窗口的默认选中）。
    pub fn create(
        &mut self,
        anchor_tab: Option<String>,
        mode: CopilotMode,
        title: String,
    ) -> String {
        let id = uuid::Uuid::new_v4().simple().to_string();
        let title = if title.trim().is_empty() {
            crate::app::ai::UNTITLED_TITLE.to_string()
        } else {
            title
        };
        self.conversations
            .push(Conversation::new(id.clone(), anchor_tab, mode, title));
        self.active_id = id.clone();
        self.enforce_limits();
        self.touch();
        id
    }

    /// 删除会话。运行中的会话先取消（审批按拒绝结算）。
    pub fn remove(&mut self, id: &str) -> bool {
        let Some(pos) = self.conversations.iter().position(|c| c.id == id) else {
            return false;
        };
        let mut conv = self.conversations.remove(pos);
        conv.cancel_generation();
        if self.active_id == id {
            self.active_id = self
                .conversations
                .iter()
                .max_by_key(|c| c.updated_at)
                .map(|c| c.id.clone())
                .unwrap_or_default();
        }
        self.touch();
        true
    }

    /// 锚点标签关闭：会话保留，标记失效。
    pub fn mark_anchor_lost(&mut self, tab_id: &str) {
        for conv in self.conversations.iter_mut() {
            if conv.anchor_tab.as_deref() == Some(tab_id) {
                conv.anchor_lost = true;
            }
        }
    }

    /// 关闭面板 / 切标签都不影响会话，这里只处理「应用退出」：取消所有运行中轮次。
    pub fn cancel_all(&mut self) {
        for conv in self.conversations.iter_mut() {
            if conv.is_busy() {
                conv.cancel_generation();
            }
        }
    }

    /// 结构性变更后请求一次落盘（由防抖钩子合并）。
    pub fn touch(&mut self) {
        if self.persist.is_none() || self.persist_pending {
            return;
        }
        self.persist_pending = true;
        if let Some(hook) = self.persist.clone() {
            hook();
        }
    }

    /// 解除 `persist_pending`（由防抖计时器在真正写盘前调用）。
    pub fn clear_pending(&mut self) {
        self.persist_pending = false;
    }

    // --- 序列化 -----------------------------------------------------------

    /// 导出成落盘形态。落盘前消息过脱敏（`redact` 为 None 表示不做）。
    pub fn to_stored(&self, redact: Option<&IndexedRedactor>) -> StoredStore {
        StoredStore {
            version: 1,
            active_id: self.active_id.clone(),
            conversations: self
                .conversations
                .iter()
                .map(|c| StoredConversation {
                    id: c.id.clone(),
                    title: c.title.clone(),
                    anchor_tab: c.anchor_tab.clone(),
                    anchor_lost: c.anchor_lost,
                    mode: c.mode.as_str().to_string(),
                    status: match c.status {
                        // 关掉的进程里不存在「正在跑」：重启后一律是中断。
                        ConversationStatus::Running => "interrupted".to_string(),
                        other => other.as_str().to_string(),
                    },
                    created_at: Some(c.created_at),
                    updated_at: Some(c.updated_at),
                    messages: c
                        .messages
                        .iter()
                        .map(|m| {
                            let mut sm = StoredMessage::from(m);
                            // pending 卡片不落盘为 pending：审批通道是进程内对象。
                            if sm.disposition == "pending" {
                                sm.disposition = "expired".to_string();
                            }
                            if let Some(r) = redact {
                                sm.text = redact_text(r, &sm.text);
                                sm.cmd = redact_text(r, &sm.cmd);
                                sm.explain = redact_text(r, &sm.explain);
                                sm.side_effect = redact_text(r, &sm.side_effect);
                                sm.blocked_reason = redact_text(r, &sm.blocked_reason);
                            }
                            sm
                        })
                        .collect(),
                })
                .collect(),
        }
    }

    /// 从落盘形态恢复：`running → interrupted`，`pending → expired`。
    pub fn load_stored(&mut self, stored: StoredStore) {
        self.conversations.clear();
        for sc in stored.conversations {
            let mut conv = Conversation::new(
                sc.id,
                sc.anchor_tab,
                CopilotMode::parse(&sc.mode),
                if sc.title.trim().is_empty() {
                    crate::app::ai::UNTITLED_TITLE.to_string()
                } else {
                    sc.title
                },
            );
            conv.anchor_lost = sc.anchor_lost;
            conv.status = match sc.status.as_str() {
                "running" | "interrupted" => ConversationStatus::Interrupted,
                _ => ConversationStatus::Idle,
            };
            if let Some(t) = sc.created_at {
                conv.created_at = t;
            }
            if let Some(t) = sc.updated_at {
                conv.updated_at = t;
            }
            conv.messages = sc.messages.into_iter().map(AiMessage::from).collect();
            self.conversations.push(conv);
        }
        self.active_id = if self.get(&stored.active_id).is_some() {
            stored.active_id
        } else {
            self.conversations
                .iter()
                .max_by_key(|c| c.updated_at)
                .map(|c| c.id.clone())
                .unwrap_or_default()
        };
        self.enforce_limits();
    }

    // --- 落盘 -------------------------------------------------------------

    /// 写 `ai_conversations.json`：tmp + rename 原子写，unix 0600。
    pub fn save(&self, redact: Option<&IndexedRedactor>) -> std::io::Result<()> {
        let Some(path) = self.path.as_deref() else {
            return Ok(());
        };
        let stored = self.to_stored(redact);
        let raw = serde_json::to_vec_pretty(&stored)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        write_private_atomic(path, &raw)
    }

    /// 从磁盘恢复。文件不存在 = 空表；损坏则**保留原文件**、记日志、回退为空表。
    pub fn load(&mut self, redact: Option<&IndexedRedactor>) {
        let _ = redact;
        let Some(path) = self.path.as_deref() else {
            return;
        };
        let raw = match std::fs::read(path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(e) => {
                tracing::error!("read {} failed: {e}", path.display());
                return;
            }
        };
        match serde_json::from_slice::<StoredStore>(&raw) {
            Ok(stored) => self.load_stored(stored),
            Err(e) => {
                // 不静默清空：损坏文件改名留档，用户还能手工抢救。
                let keep = path.with_extension("json.corrupt");
                let moved = std::fs::rename(path, &keep);
                tracing::error!(
                    "parse {} failed ({e}); moved to {} ({moved:?})",
                    path.display(),
                    keep.display()
                );
                self.conversations.clear();
                self.active_id.clear();
            }
        }
    }

    // --- 上限 -------------------------------------------------------------

    fn enforce_limits(&mut self) {
        // 会话数上限：淘汰最旧的非 running 会话。
        while self.conversations.len() > MAX_CONVERSATIONS {
            let victim = self
                .conversations
                .iter()
                .enumerate()
                .filter(|(_, c)| !c.is_busy())
                .min_by_key(|(_, c)| c.updated_at)
                .map(|(i, _)| i);
            match victim {
                Some(i) => {
                    let removed = self.conversations.remove(i);
                    if self.active_id == removed.id {
                        self.active_id.clear();
                    }
                }
                None => break,
            }
        }
        for conv in self.conversations.iter_mut() {
            trim_messages(&mut conv.messages);
        }
    }
}

/// 消息条数 / 体积上限：从最旧的非卡片消息开始淘汰，卡片（审计痕迹）保留。
fn trim_messages(messages: &mut Vec<AiMessage>) {
    let mut total: usize = messages
        .iter()
        .map(|m| {
            m.text.len()
                + m.cmd.len()
                + m.explain.len()
                + m.side_effect.len()
                + m.blocked_reason.len()
        })
        .sum();
    let mut idx = 0;
    while (messages.len() > MAX_MESSAGES_PER_CONVERSATION
        || total > MAX_TEXT_BYTES_PER_CONVERSATION)
        && idx < messages.len()
    {
        let is_card =
            !messages[idx].card_id.is_empty() && messages[idx].role == crate::app::ai::ROLE_CARD;
        if is_card {
            idx += 1;
            continue;
        }
        let m = messages.remove(idx);
        total = total.saturating_sub(
            m.text.len()
                + m.cmd.len()
                + m.explain.len()
                + m.side_effect.len()
                + m.blocked_reason.len(),
        );
    }
}

fn redact_text(redactor: &IndexedRedactor, text: &str) -> String {
    if text.is_empty() || redactor.is_empty() {
        return text.to_string();
    }
    let mut map = ai_copilot::redact::RedactionMap::new();
    redactor.apply(text, &mut map)
}

/// tmp + rename 原子写，unix 上权限 0600（sessions.json 同款）。
pub fn write_private_atomic(path: &Path, raw: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, raw)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, path)
}

/// 会话列表快照（UI popover 用）。
#[derive(Debug, Clone, PartialEq)]
pub struct ConversationSummary {
    pub id: String,
    pub title: String,
    pub anchor_name: String,
    /// 锚点终端是否还在（锚点关掉后会话仍可继续，但列表里置灰）。
    pub anchor_alive: bool,
    pub status: ConversationStatus,
    pub updated_at: DateTime<Utc>,
}

/// 按更新时间倒序生成列表快照。`anchor_name` 由调用方按 tab 元数据填。
pub fn summaries<F>(store: &ConversationStore, anchor_name: F) -> Vec<ConversationSummary>
where
    F: Fn(&str) -> String,
{
    let mut out: Vec<ConversationSummary> = store
        .conversations
        .iter()
        .map(|c| ConversationSummary {
            id: c.id.clone(),
            title: c.title.clone(),
            anchor_name: match c.anchor_tab.as_deref() {
                Some(tab) if !c.anchor_lost => anchor_name(tab),
                _ => String::new(),
            },
            anchor_alive: c.anchor_tab.is_some() && !c.anchor_lost,
            status: c.status,
            updated_at: c.updated_at,
        })
        .collect();
    out.sort_by_key(|c| std::cmp::Reverse(c.updated_at));
    out
}

/// 把「标签 id → 锚点会话」映射出来（一次性建会话时用）。
pub fn by_anchor<'a>(store: &'a ConversationStore, tab_id: &str) -> Option<&'a Conversation> {
    // 同一锚点可能有历史会话，取最近更新的一条。
    store
        .conversations
        .iter()
        .filter(|c| c.anchor_tab.as_deref() == Some(tab_id) && !c.anchor_lost)
        .max_by_key(|c| c.updated_at)
}

/// 会话表要被 tokio 侧的 `spawn_ask` 持有：钩子必须是 Send + Sync。
#[allow(dead_code)]
fn _assert_handles_are_send_sync() {
    fn check<T: Send + Sync>() {}
    check::<PersistSink>();
    check::<ConversationStoreHandle>();
    let _ = HashMap::<String, String>::new();
}

/// 进程级会话表的句柄（UI 线程 + tokio 侧共用；短临界区，不在 await 期间持锁）。
pub type ConversationStoreHandle = Arc<std::sync::Mutex<ConversationStore>>;

#[cfg(test)]
#[path = "../../tests/app/ai_store/mod.rs"]
mod ai_store_tests;
