//! AI Copilot 面板的 Slint 回调接线（FengheShell x rssh 缝合面）。
//!
//! 面板本身不长任何业务逻辑：这里只负责把 UI 意图翻译成 ai-copilot 的
//! 输入，并把副作用（写 PTY / 读远端文件）交给 app::ai 的 ToolRuntime。

use super::*;

use std::time::Duration;

use ai_copilot::shell::ShellKind;
use ai_copilot::triage::{CompletionContext, CopilotConfig, TriageEngine, Trigger};
use chrono::{DateTime, Local, Utc};

use crate::app::ai_store::ConversationStoreHandle;
use crate::resource::SystemSnapshot;

/// 面板宽度档位（px）：需求文档 D1 定死两档，不做拖拽。
pub(super) const PANEL_WIDTHS: [u16; 2] = [360, 520];

/// 窗口上「当前查看的会话」，没有就按当前标签惰性建一条（空锚点会话）。
fn current_conversation(core: &AppCore, win: &Rc<AppWindow>) -> Option<String> {
    let existing = win.get_ai_active_id().to_string();
    if core
        .conversations
        .lock()
        .ok()
        .map(|g| g.get(&existing).is_some())
        .unwrap_or(false)
    {
        return Some(existing);
    }
    // 每窗口的选中态丢了（新窗口 / 会话被删）：优先当前标签的锚点会话，
    // 否则最近一条，再否则新建一条。
    let active_tab = win.get_active_tab_id().to_string();
    let picked = {
        let store = core.conversations.lock().ok()?;
        crate::app::ai_store::by_anchor(&store, &active_tab)
            .map(|c| c.id.clone())
            .or_else(|| {
                store
                    .conversations
                    .iter()
                    .max_by_key(|c| c.updated_at)
                    .map(|c| c.id.clone())
            })
    };
    let id = match picked {
        Some(id) => id,
        None => create_conversation(core, win, Some(active_tab))?,
    };
    apply_conversation(core, win, &id);
    Some(id)
}

/// 新建会话并选中：锚定当前终端标签（`anchor` 为 None 表示纯对话）。
fn create_conversation(
    core: &AppCore,
    win: &Rc<AppWindow>,
    anchor: Option<String>,
) -> Option<String> {
    let mode = core.store.borrow().ai_mode();
    let id = {
        let mut store = core.conversations.lock().ok()?;
        let id = store.create(anchor, mode, crate::app::ai::UNTITLED_TITLE.to_string());
        store.touch();
        id
    };
    apply_conversation(core, win, &id);
    Some(id)
}

/// 把一条会话重放进窗口：标题、锚点、模式、消息、忙碌态、会话列表。
fn apply_conversation(core: &AppCore, win: &Rc<AppWindow>, id: &str) {
    let (messages, busy, mode, anchor_name, alive) = {
        let store = match core.conversations.lock() {
            Ok(g) => g,
            Err(e) => e.into_inner(),
        };
        let Some(conv) = store.get(id) else {
            return;
        };
        let anchor_name = conv
            .anchor_tab
            .as_deref()
            .filter(|_| !conv.anchor_lost)
            .map(|tab| anchor_name(core, tab))
            .unwrap_or_default();
        (
            conv.messages.clone(),
            conv.is_busy(),
            conv.mode.as_str().to_string(),
            anchor_name,
            !conv.anchor_lost && conv.anchor_tab.is_some(),
        )
    };
    win.set_ai_active_id(id.into());
    win.set_ai_messages(ModelRc::from(std::rc::Rc::new(VecModel::from(messages))));
    win.set_ai_busy(busy);
    // 空会话跟随全局默认模式；有历史的会话恢复它自己的锁定模式。
    if !mode.is_empty() {
        win.set_ai_mode(mode.into());
    }
    win.set_ai_anchor_name(anchor_name.into());
    let _ = alive;
    refresh_header(core, win, id);
    refresh_list(core, win);
}

/// 标题（会话列表里是权威的，面板头部跟着它走）。
fn refresh_header(core: &AppCore, win: &Rc<AppWindow>, id: &str) {
    let title = core
        .conversations
        .lock()
        .ok()
        .and_then(|g| g.get(id).map(|c| c.title.clone()))
        .unwrap_or_else(|| crate::app::ai::UNTITLED_TITLE.to_string());
    win.set_ai_title(title.into());
}

/// 会话列表快照 → Slint 模型。
fn refresh_list(core: &AppCore, win: &Rc<AppWindow>) {
    let rows = {
        let store = match core.conversations.lock() {
            Ok(g) => g,
            Err(e) => e.into_inner(),
        };
        crate::app::ai_store::summaries(&store, |tab| anchor_name(core, tab))
    };
    let items: Vec<ConversationInfo> = rows
        .into_iter()
        .map(|s| ConversationInfo {
            id: s.id.into(),
            title: s.title.into(),
            anchor_name: s.anchor_name.into(),
            status: s.status.as_str().into(),
            updated: format_updated(s.updated_at).into(),
            alive: s.anchor_alive,
        })
        .collect();
    win.set_ai_conversations(ModelRc::from(std::rc::Rc::new(VecModel::from(items))));
}

/// 锚点名：标签标题里人认得的那一段（`name` 优先，退到 id）。
fn anchor_name(core: &AppCore, tab_id: &str) -> String {
    for w in core.window_states.borrow().values() {
        if let Some(row) = w.terminals_model.iter().find(|r| r.id.as_str() == tab_id) {
            let title = row.title.to_string();
            if !title.is_empty() {
                return title;
            }
        }
    }
    tab_id.to_string()
}

fn format_updated(at: DateTime<Utc>) -> String {
    let local: DateTime<Local> = at.into();
    let now = Local::now();
    if local.date_naive() == now.date_naive() {
        local.format("%H:%M").to_string()
    } else {
        local.format("%m-%d %H:%M").to_string()
    }
}

/// 面板开关：收起不打断任何一轮；展开时按当前标签挑会话重放。
fn set_panel_open(core: &AppCore, win: &Rc<AppWindow>, open: bool) {
    win.set_ai_open(open);
    if !open {
        win.set_ai_list_open(false);
        return;
    }
    let id = match current_conversation(core, win) {
        Some(id) => id,
        None => return,
    };
    apply_conversation(core, win, &id);
    // 「1+1>2」：硬件告警时打开面板就顺手起一轮 Triage，不用自己组织问题。
    let Some(snapshot) = local_snapshot(core) else {
        return;
    };
    let Some(reason) = hot_reason(&snapshot) else {
        return;
    };
    // 自动排查独占一条会话：不抢占用户正在看的那条，也不重复起（同一锚点
    // 已经有一条自动排查会话就跳过）。
    let target = win.get_active_tab_id().to_string();
    let anchor = if target.is_empty() || target == "welcome" {
        None
    } else {
        Some(target)
    };
    let existing = core.conversations.lock().ok().and_then(|g| {
        g.conversations
            .iter()
            .find(|c| c.trigger == Some(crate::app::ai_store::TriggerTag::Monitor))
            .map(|c| c.id.clone())
    });
    if let Some(existing) = existing {
        let id = existing;
        apply_conversation(core, win, &id);
        ask_question(core, win, &id, String::new(), Trigger::Monitor(reason));
        return;
    }
    let mode = core.store.borrow().ai_mode();
    let new_id = {
        let mut store = match core.conversations.lock() {
            Ok(g) => g,
            Err(e) => e.into_inner(),
        };
        let id = store.create(anchor, mode, reason.clone());
        if let Some(conv) = store.get_mut(&id) {
            conv.trigger = Some(crate::app::ai_store::TriggerTag::Monitor);
        }
        store.touch();
        id
    };
    apply_conversation(core, win, &new_id);
    ask_question(core, win, &new_id, String::new(), Trigger::Monitor(reason));
}

/// 最近一次的本地资源快照（与监控侧边栏同源）。
fn local_snapshot(core: &AppCore) -> Option<SystemSnapshot> {
    core.window_states
        .borrow()
        .values()
        .find_map(|w| w.local_snap.lock().ok().map(|s| s.clone()))
}

/// 硬件告警判据：AI 抽屉据此主动发起 Triage。
fn hot_reason(s: &SystemSnapshot) -> Option<String> {
    if s.cpu_percent >= 90.0 {
        return Some(format!("CPU 占用 {:.0}%", s.cpu_percent));
    }
    if s.mem_total_mib > 0 && s.mem_used_mib * 100 / s.mem_total_mib.max(1) >= 90 {
        return Some(format!(
            "内存占用 {}/{} MiB",
            s.mem_used_mib, s.mem_total_mib
        ));
    }
    for (mount, available, total) in &s.disks {
        if *total > 0 && available * 100 / total >= 90 {
            return Some(format!("磁盘 {} 剩余不足 10%", mount));
        }
    }
    None
}

/// 标签所在会话的定义 + 跳板链（供 read_remote_file 走 automation 层）。
fn tab_session(
    core: &AppCore,
    store: &Rc<RefCell<ConfigStore>>,
    tab_id: &str,
) -> Option<(crate::config::Session, Vec<crate::config::Session>)> {
    let session_id = core.tab_routes.lock().ok().and_then(|routes| {
        routes.get(tab_id).and_then(|route| {
            route.lock().ok().and_then(|r| {
                r.statuses
                    .lock()
                    .ok()
                    .and_then(|s| s.get(tab_id).map(|st| st.session_id.clone()))
            })
        })
    })?;
    if session_id.is_empty() {
        return None;
    }
    let store = store.borrow();
    let session = store.get(&session_id)?.clone();
    let jump = store.resolve_jump_chain(&session).unwrap_or_default();
    Some((session, jump))
}

fn shell_kind(session: Option<&crate::config::Session>) -> ShellKind {
    use crate::config::SessionKind;
    match session.map(|s| s.kind) {
        Some(SessionKind::Local) => {
            let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());
            ShellKind::from_local_path(&shell)
        }
        Some(SessionKind::Serial) => ShellKind::Serial,
        Some(SessionKind::Telnet) => ShellKind::Telnet,
        _ => ShellKind::Posix,
    }
}

fn endpoint_default(protocol: &str) -> &'static str {
    match protocol {
        "deepseek-thinking" => "https://api.deepseek.com",
        "anthropic-messages" => "https://api.anthropic.com",
        "ollama" => "http://localhost:11434/v1",
        _ => "https://api.openai.com/v1",
    }
}

fn model_default(protocol: &str) -> &'static str {
    match protocol {
        "deepseek-thinking" => "deepseek-chat",
        "anthropic-messages" => "claude-3-5-sonnet-latest",
        "ollama" => "qwen2.5-coder:7b",
        _ => "gpt-4o-mini",
    }
}

fn ai_config(store: &Rc<RefCell<ConfigStore>>, shell: ShellKind) -> CopilotConfig {
    let s = store.borrow();
    let protocol = s.ai_protocol();
    let endpoint = if s.ai_endpoint().trim().is_empty() {
        endpoint_default(protocol).to_string()
    } else {
        s.ai_endpoint().to_string()
    };
    let model = if s.ai_model().trim().is_empty() {
        model_default(protocol).to_string()
    } else {
        s.ai_model().to_string()
    };
    CopilotConfig {
        protocol: protocol.to_string(),
        endpoint,
        api_key: s.ai_api_key().to_string(),
        model,
        shell,
        redaction_enabled: s.ai_redaction_enabled(),
        auto_run_readonly: s.ai_auto_run_readonly(),
        yolo: s.ai_yolo_mode(),
        locale: crate::i18n::current_code().to_string(),
        mode: s.ai_mode(),
        skills: s.ai_skills(),
    }
}

/// 起一轮 Triage。UI 只负责打包上下文，之后全部异步进行。
fn ask_question(
    core: &AppCore,
    win: &AppWindow,
    conversation_id: &str,
    question: String,
    trigger: Trigger,
) {
    let conversations = core.conversations.clone();
    // 本轮要用的锚点、忙碌态、当前模式先取一份快照（短临界区）。
    let (anchor, busy, mode, title) = {
        let store = match conversations.lock() {
            Ok(g) => g,
            Err(e) => e.into_inner(),
        };
        let Some(conv) = store.get(conversation_id) else {
            return;
        };
        (
            conv.anchor_tab.clone().filter(|_| !conv.anchor_lost),
            conv.is_busy(),
            conv.mode,
            conv.title.clone(),
        )
    };
    let _ = title;
    if busy {
        push_notice(
            &conversations,
            &win.as_weak(),
            conversation_id,
            ROLE_ERROR,
            crate::i18n::t(
                "上一次诊断还没结束。",
                "The previous diagnosis is still running.",
            ),
        );
        return;
    }
    // 并发闸门：默认同时最多 3 条会话在跑，满了就明说，不排队、不静默丢弃。
    let max_parallel = core.store.borrow().ai_max_parallel();
    let running = conversations
        .lock()
        .map(|g| g.running_count())
        .unwrap_or_default();
    if running >= max_parallel {
        push_notice(
            &conversations,
            &win.as_weak(),
            conversation_id,
            ROLE_ERROR,
            &if crate::i18n::is_en() {
                format!("{running} conversations are already running (limit {max_parallel}). Stop one to send.")
            } else {
                format!("已有 {running} 条会话在运行（上限 {max_parallel}），先停一条再发送。")
            },
        );
        return;
    }
    // 锚点解析链：anchor_tab → 标签路由 → PTY 写通道 + 终端上下文。
    let (raw, context) = anchor
        .as_deref()
        .map(|tab| {
            let raw = core
                .window_states
                .borrow()
                .values()
                .find(|w| w.terminals_model.iter().any(|r| r.id.as_str() == tab))
                .and_then(|w| w.handles.borrow().get(tab).map(|h| h.commands.clone()));
            (raw, context_handle(core, tab))
        })
        .unwrap_or((None, None));
    let store = core.store.clone();
    let (session, jump) = anchor
        .as_deref()
        .and_then(|tab| tab_session(core, &store, tab))
        .map(|(s, j)| (Some(s), j))
        .unwrap_or((None, Vec::new()));
    let shell = shell_kind(session.as_ref());
    let mut cfg = ai_config(&store, shell);
    // 会话级模式（F3）：这一轮按会话自己的 ask/agent 跑，不看全局默认。
    cfg.mode = mode;
    let local = local_snapshot(core);
    spawn_ask(
        core.runtime.handle(),
        AskParams {
            conversation_id: conversation_id.to_string(),
            question,
            trigger,
            conversations,
            weak: win.as_weak(),
            raw,
            context,
            session,
            jump,
            cfg,
            shell,
            local,
        },
    );
}

/// 卡片按钮：绿灯→批准执行；橙/红灯→拒绝引擎，同时把命令填进终端让用户自己回车。
fn answer_card(core: &AppCore, conversation_id: &str, card_id: &str, approve: bool) {
    let conversations = core.conversations.clone();
    let Some((gate, cmd)) = card_info(&conversations, conversation_id, card_id) else {
        return;
    };
    // 橙/红灯永远只「填入」：Human-in-the-loop 的核心约束。
    let approved = approve && gate == "direct";
    if !approved && gate == "fill" && cmd_has_content(&cmd) {
        let anchor = conversations
            .lock()
            .ok()
            .and_then(|g| g.get(conversation_id).and_then(|c| c.anchor_tab.clone()));
        if let Some(tab) = anchor {
            if let Some(handle) = core
                .window_states
                .borrow()
                .values()
                .find(|w| w.terminals_model.iter().any(|r| r.id.as_str() == tab))
                .and_then(|w| w.handles.borrow().get(&tab).map(|h| h.commands.clone()))
            {
                let _ = handle.send(crate::ssh::SessionCommand::RawInput(cmd.into_bytes()));
            }
        }
    }
    answer_approval(&conversations, conversation_id, card_id, approved);
}

fn cmd_has_content(cmd: &str) -> bool {
    !cmd.trim().is_empty()
}

/// 连通性自检：用本次输入（未填写则回落到已保存配置）请求一次 /models。
/// 不写盘、不改动已保存设置，纯探测。
fn test_connection(
    core: &AppCore,
    win: &Rc<AppWindow>,
    protocol: &str,
    endpoint: &str,
    model: &str,
    api_key: &str,
) {
    let store = core.store.clone();
    // 空白字段回落到已保存配置，这样“只改端点”也能测。
    let cfg = {
        let s = store.borrow();
        (
            s.ai_protocol().to_string(),
            s.ai_endpoint().to_string(),
            s.ai_model().to_string(),
            s.ai_api_key().to_string(),
        )
    };
    let pick = |typed: &str, saved: String| -> String {
        if typed.trim().is_empty() {
            saved
        } else {
            typed.trim().to_string()
        }
    };
    let protocol = pick(protocol, cfg.0);
    let endpoint = pick(endpoint, cfg.1);
    let _model = pick(model, cfg.2);
    let api_key = if api_key.is_empty() {
        cfg.3
    } else {
        api_key.to_string()
    };

    win.set_ai_test_status("".into());
    win.set_ai_test_busy(true);
    let weak = win.as_weak();
    core.runtime.spawn(async move {
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(20), async {
            let client = ai_copilot::build_client(&protocol, api_key, endpoint)?;
            let models = client.list_models().await?;
            Ok::<usize, ai_copilot::AppError>(models.len())
        })
        .await;
        // `t()` 只吃 'static 文案，带变量的句子自己按语言拼。
        let text = match outcome {
            Ok(Ok(count)) => {
                if crate::i18n::is_en() {
                    format!("Connected: the endpoint lists {count} models.")
                } else {
                    format!("连接成功：该端点返回 {count} 个可用模型。")
                }
            }
            Ok(Err(e)) => {
                if crate::i18n::is_en() {
                    format!("Connection failed: {e}")
                } else {
                    format!("连接失败：{e}")
                }
            }
            Err(_) => crate::i18n::t("连接超时（20 秒）。", "Timed out after 20 s.").to_string(),
        };
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(win) = weak.upgrade() {
                win.set_ai_test_status(text.into());
                win.set_ai_test_busy(false);
            }
        });
    });
}

/// 补全只需要一个 LLM 客户端，不需要任何副作用出口；给它一个永远拒绝执行的
/// runtime，万一模型在补全阶段乱调工具也不会碰到主机。
struct CompletionOnlyRuntime;

#[async_trait::async_trait]
impl ai_copilot::triage::ToolRuntime for CompletionOnlyRuntime {
    async fn request_approval(
        &self,
        _p: &ai_copilot::triage::ProposedCommand,
    ) -> ai_copilot::AppResult<bool> {
        Ok(false)
    }
    async fn execute_approved(
        &self,
        _p: &ai_copilot::triage::ProposedCommand,
    ) -> ai_copilot::AppResult<ai_copilot::triage::CommandOutcome> {
        Err(ai_copilot::AppError::other(
            "completion_has_no_side_effects",
            serde_json::json!({}),
        ))
    }
    async fn read_remote_file(
        &self,
        _path: &str,
        _max_bytes: usize,
    ) -> ai_copilot::AppResult<String> {
        Err(ai_copilot::AppError::other(
            "completion_has_no_side_effects",
            serde_json::json!({}),
        ))
    }
}

/// 空会话时的起手建议：对应 Copilot 的 starter suggestions，
/// 都是「先只读、不落地改动」的排障问法。
fn starter_questions() -> Vec<SharedString> {
    let en = crate::i18n::is_en();
    [
        (
            "这台机器为什么卡？先看 CPU、内存和负载",
            "Why is this box slow? Check CPU, memory and load first",
        ),
        (
            "看看 /var/log 里最新的错误，只做只读排查",
            "Show the latest errors in /var/log - read-only investigation",
        ),
        (
            "磁盘快满了，帮我找出最大的目录",
            "Disk is almost full - find the largest directories",
        ),
    ]
    .iter()
    .map(|(zh, en_s)| SharedString::from(if en { *en_s } else { *zh }))
    .collect()
}

pub(super) fn wire_ai(ctx: &WinCtx) {
    let WinCtx {
        core,
        store,
        window,
        ..
    } = ctx;

    window.set_ai_starters(ModelRc::from(std::rc::Rc::new(VecModel::from(
        starter_questions(),
    ))));

    // --- 播种 LLM 设置 ---------------------------------------------------
    {
        let s = store.borrow();
        window.set_ai_protocol(s.ai_protocol().into());
        window.set_ai_endpoint(s.ai_endpoint().into());
        window.set_ai_model(s.ai_model().into());
        window.set_ai_redaction_enabled(s.ai_redaction_enabled());
        window.set_ai_auto_run_readonly(s.ai_auto_run_readonly());
        window.set_ai_yolo_mode(s.ai_yolo_mode());
        window.set_ai_mode(s.ai_mode().as_str().into());
        window.set_ai_completion_enabled(s.ai_completion_enabled());
        window.set_ai_max_parallel(s.ai_max_parallel() as i32);
        window.set_ai_panel_width(s.ai_panel_width() as i32);
        let width = s.ai_panel_width() as i32;
        let _ = width;
    }
    // 面板宽度档位两档，UI 只认 360 / 520。
    debug_assert_eq!(PANEL_WIDTHS, [360, 520]);

    // --- 补全层（Copilot 第一层）-----------------------------------------
    {
        let core = core.clone();
        let store = store.clone();
        let win = window.clone();
        window.on_ai_complete_request(move |tab_id: SharedString, prefix: SharedString| {
            let Some(ctx) = context_handle(&core, &tab_id) else {
                return;
            };
            // 输入太短、没配密钥、或补全层被关掉 —— 直接清空旧建议，不发请求。
            let cleared = prefix.trim().len() < 3
                || !{
                    let s = store.borrow();
                    s.ai_completion_enabled() && !s.ai_api_key().trim().is_empty()
                };
            let seq = completion_request(&ctx);
            if cleared {
                set_completion(&win.as_weak(), &tab_id, "");
                return;
            }
            let prefix_s = prefix.to_string();
            let cfg = ai_config(&store, ShellKind::Posix);
            // 带上这个标签最近跑过的几条命令，补全才能贴合当前在做什么。
            let recent_commands = ctx
                .lock()
                .map(|g| g.context_blocks(4).into_iter().map(|b| b.command).collect())
                .unwrap_or_default();
            let weak = win.as_weak();
            let tab = tab_id.to_string();
            let ctx2 = ctx.clone();
            core.runtime.spawn(async move {
                // 输入停顿 400ms 再发，避免每敲一个字符都打一次模型。
                tokio::time::sleep(Duration::from_millis(400)).await;
                let suggestion = match TriageEngine::new(cfg, Arc::new(CompletionOnlyRuntime))
                    .map(|e| e.with_completion_cache(completion_cache()))
                {
                    Ok(engine) => {
                        let ctx = CompletionContext {
                            recent_commands,
                            cwd: None,
                        };
                        match tokio::time::timeout(
                            Duration::from_secs(10),
                            engine.complete(&prefix_s, &ctx),
                        )
                        .await
                        {
                            Ok(Ok(text)) => text,
                            _ => String::new(),
                        }
                    }
                    Err(_) => String::new(),
                };
                if completion_is_current(&ctx2, seq) {
                    set_completion(&weak, &tab, &suggestion);
                }
            });
        });
    }

    // --- 补全层开关 -------------------------------------------------------
    {
        let core = core.clone();
        let store = store.clone();
        let win = window.clone();
        window.on_ai_set_completion(move |enabled: bool| {
            {
                let mut s = store.borrow_mut();
                s.set_ai_completion_enabled(enabled);
                let _ = s.save();
            }
            core.registry.for_each(|weak| {
                if let Some(w) = weak.upgrade() {
                    w.set_ai_completion_enabled(enabled);
                }
            });
            win.set_ai_completion_enabled(enabled);
        });
    }

    // --- YOLO 模式（完全信任：关脱敏 + 免审批）--------------------------
    {
        let core = core.clone();
        let store = store.clone();
        let win = window.clone();
        window.on_ai_set_yolo(move |enabled: bool| {
            {
                let mut s = store.borrow_mut();
                s.set_ai_yolo_mode(enabled);
                let _ = s.save();
            }
            // 所有窗口共享同一份设置，立刻同步到每个实例。
            core.registry.for_each(|weak| {
                if let Some(w) = weak.upgrade() {
                    w.set_ai_yolo_mode(enabled);
                }
            });
            win.set_ai_yolo_mode(enabled);
        });
    }

    // --- 工作方式（Ask / Agent，会话级）------------------------------------
    // 全局 ai_mode 只是「新会话的默认模式」；已存在的会话各自锁定自己的模式。
    {
        let core = core.clone();
        let store = store.clone();
        let win = window.clone();
        window.on_ai_set_mode(move |mode: SharedString| {
            let parsed = ai_copilot::triage::CopilotMode::parse(mode.as_str());
            {
                let mut s = store.borrow_mut();
                s.set_ai_mode(parsed);
                let _ = s.save();
            }
            let id = win.get_ai_active_id().to_string();
            if let Ok(mut convs) = core.conversations.lock() {
                if let Some(conv) = convs.get_mut(&id) {
                    conv.mode = parsed;
                    conv.updated_at = chrono::Utc::now();
                }
                convs.touch();
            }
            win.set_ai_mode(parsed.as_str().into());
        });
    }

    // --- 面板开关 / 会话列表 ----------------------------------------------
    {
        let core = core.clone();
        let win = window.clone();
        window.on_ai_toggle_panel(move || {
            let open = win.get_ai_open();
            set_panel_open(&core, &win, !open);
            win.set_ai_list_open(false);
        });
    }
    {
        let core = core.clone();
        let win = window.clone();
        window.on_ai_toggle_list(move || {
            let open = win.get_ai_list_open();
            refresh_list(&core, &win);
            win.set_ai_list_open(!open);
        });
    }
    {
        let core = core.clone();
        let win = window.clone();
        window.on_ai_new_conversation(move || {
            // 锚定当前终端标签；welcome 标签没有终端，就建一条纯对话会话。
            let active_tab = win.get_active_tab_id().to_string();
            let anchor = if active_tab.is_empty() || active_tab == "welcome" {
                None
            } else {
                Some(active_tab)
            };
            ensure_panel_open(&core, &win);
            create_conversation(&core, &win, anchor);
        });
    }
    {
        let core = core.clone();
        let win = window.clone();
        window.on_ai_select_conversation(move |id: SharedString| {
            apply_conversation(&core, &win, &id);
        });
    }
    {
        let core = core.clone();
        let win = window.clone();
        window.on_ai_close_conversation(move |id: SharedString| {
            let running = core
                .conversations
                .lock()
                .map(|g| g.get(&id).map(|c| c.is_busy()).unwrap_or(false))
                .unwrap_or(false);
            // 运行中的会话：关闭即取消这一轮（审批按拒绝结算），不再二次确认 ——
            // 面板收起后用户仍能在列表里看到它，误关成本很低。
            let _ = running;
            let removed = {
                let mut convs = match core.conversations.lock() {
                    Ok(g) => g,
                    Err(e) => e.into_inner(),
                };
                let ok = convs.remove(&id);
                convs.touch();
                ok
            };
            if !removed {
                return;
            }
            refresh_list(&core, &win);
            if win.get_ai_active_id().as_str() == id.as_str() {
                let next = core.conversations.lock().ok().and_then(|g| {
                    g.conversations
                        .iter()
                        .max_by_key(|c| c.updated_at)
                        .map(|c| c.id.clone())
                });
                match next {
                    Some(next) => apply_conversation(&core, &win, &next),
                    None => {
                        win.set_ai_active_id("".into());
                        win.set_ai_messages(ModelRc::from(std::rc::Rc::new(VecModel::from(
                            Vec::new(),
                        ))));
                        win.set_ai_title(crate::app::ai::UNTITLED_TITLE.into());
                        win.set_ai_anchor_name("".into());
                        win.set_ai_busy(false);
                    }
                }
            }
        });
    }
    {
        let core = core.clone();
        let win = window.clone();
        window.on_ai_rename_conversation(move |id: SharedString, name: SharedString| {
            let name = name.trim().to_string();
            if name.is_empty() {
                return;
            }
            {
                let mut convs = match core.conversations.lock() {
                    Ok(g) => g,
                    Err(e) => e.into_inner(),
                };
                if let Some(conv) = convs.get_mut(&id) {
                    conv.title = name.clone();
                    conv.updated_at = chrono::Utc::now();
                }
                convs.touch();
            }
            refresh_list(&core, &win);
            if win.get_ai_active_id().as_str() == id.as_str() {
                win.set_ai_title(name.into());
            }
        });
    }
    {
        let store = store.clone();
        let win = window.clone();
        window.on_ai_set_width(move |width: i32| {
            let width = if width >= 520 { 520 } else { 360 };
            {
                let mut s = store.borrow_mut();
                s.set_ai_panel_width(width as u16);
                let _ = s.save();
            }
            win.set_ai_panel_width(width);
        });
    }

    // --- 提问 / 清空 / 停止 ----------------------------------------------
    {
        let core = core.clone();
        let win = window.clone();
        window.on_ai_send(move |question: SharedString| {
            let Some(id) = current_conversation(&core, &win) else {
                return;
            };
            ask_question(&core, &win, &id, question.to_string(), Trigger::User);
        });
    }
    {
        let core = core.clone();
        let win = window.clone();
        window.on_ai_clear(move || {
            let id = win.get_ai_active_id().to_string();
            if id.is_empty() {
                return;
            }
            clear_conversation(&core.conversations, &win.as_weak(), &id);
        });
    }
    {
        let core = core.clone();
        let win = window.clone();
        window.on_ai_cancel(move || {
            // 停止不等同于中止 LLM 请求（请求会自己跑完），但会立刻作废它的
            // UI 写入并结清等待中的审批。
            let id = win.get_ai_active_id().to_string();
            if id.is_empty() {
                return;
            }
            cancel_generation(&core.conversations, &id);
            win.set_ai_busy(false);
            refresh_list(&core, &win);
        });
    }

    // --- Run Gate：执行 / 填入 / 拒绝 -------------------------------------
    {
        let core = core.clone();
        let win = window.clone();
        window.on_ai_run_card(move |card_id: SharedString| {
            let id = win.get_ai_active_id().to_string();
            if !id.is_empty() {
                answer_card(&core, &id, &card_id, true);
            }
        });
    }
    {
        let core = core.clone();
        let win = window.clone();
        window.on_ai_reject_card(move |card_id: SharedString| {
            let id = win.get_ai_active_id().to_string();
            if !id.is_empty() {
                answer_card(&core, &id, &card_id, false);
            }
        });
    }

    // --- 连通性自检 --------------------------------------------------------
    {
        let core = core.clone();
        let win = window.clone();
        let win_owned = win.clone();
        win.on_ai_test_connection(
            move |protocol: SharedString,
                  endpoint: SharedString,
                  model: SharedString,
                  api_key: SharedString| {
                test_connection(&core, &win_owned, &protocol, &endpoint, &model, &api_key);
            },
        );
    }

    // --- 导出审计日志 ------------------------------------------------------
    // rssh 的审计 panel + 保存按钮在 FengheShell 的对应物：引擎实时双写的
    // 进程级审计日志经 rfd 保存对话框写成人类可读文本。内容已按当轮脱敏
    // 开关处理，仍可能含命令与输出，按敏感文件保管。
    {
        let weak = window.as_weak();
        window.on_ai_export_audit(move || {
            let text = {
                let log = super::audit_log();
                let guard = match log.lock() {
                    Ok(g) => g,
                    Err(e) => e.into_inner(),
                };
                if guard.entries.is_empty() {
                    None
                } else {
                    Some(guard.to_log_string())
                }
            };
            let Some(text) = text else {
                DialogOwner::of_weak(&weak)
                    .message()
                    .set_title(crate::i18n::t("AI 审计日志", "AI audit log"))
                    .set_description(crate::i18n::t(
                        "还没有审计记录：先在 AI 抽屉里跑一轮诊断。",
                        "No audit entries yet - run a diagnosis in the AI drawer first.",
                    ))
                    .show();
                return;
            };
            if let Some(path) = DialogOwner::of_weak(&weak)
                .file()
                .set_file_name("fengheshell-ai-audit.log")
                .add_filter("LOG", &["log", "txt"])
                .save_file()
            {
                let res = std::fs::write(&path, text);
                let (title, desc): (&str, String) = match res {
                    Ok(()) => (
                        crate::i18n::t("已导出审计日志", "Audit log exported"),
                        crate::i18n::t(
                            "审计日志可能含命令与输出，请按敏感文件保管。",
                            "The audit log may contain commands and output; treat it as sensitive.",
                        )
                        .to_string(),
                    ),
                    Err(e) => (crate::i18n::t("导出失败", "Export failed"), e.to_string()),
                };
                DialogOwner::of_weak(&weak)
                    .message()
                    .set_title(title)
                    .set_description(desc)
                    .show();
            }
        });
    }

    // --- 设置保存 ----------------------------------------------------------
    {
        let core = core.clone();
        let store = store.clone();
        window.on_ai_apply_settings(
            move |protocol: SharedString,
                  endpoint: SharedString,
                  model: SharedString,
                  api_key: SharedString,
                  redaction: bool,
                  auto_run: bool| {
                // 换模型/端点后的旧补全不该继续命中，先记下原值用于比较。
                let before = {
                    let s = store.borrow();
                    (
                        s.ai_protocol().to_string(),
                        s.ai_endpoint().to_string(),
                        s.ai_model().to_string(),
                    )
                };
                {
                    let mut s = store.borrow_mut();
                    s.set_ai_settings(
                        protocol.to_string(),
                        endpoint.to_string(),
                        model.to_string(),
                        api_key.to_string(),
                        redaction,
                        auto_run,
                    );
                    let _ = s.save();
                }
                // 换了模型/端点/协议，缓存里的旧补全就不再适用。
                let changed = {
                    let s = store.borrow();
                    s.ai_protocol() != before.0
                        || s.ai_endpoint() != before.1
                        || s.ai_model() != before.2
                };
                if changed {
                    clear_completion_cache();
                }
                let s = store.borrow();
                // 所有窗口共享同一份 LLM 设置，改一处要立刻同步到每个实例。
                core.registry.for_each(|weak| {
                    if let Some(win) = weak.upgrade() {
                        win.set_ai_protocol(s.ai_protocol().into());
                        win.set_ai_endpoint(s.ai_endpoint().into());
                        win.set_ai_model(s.ai_model().into());
                        win.set_ai_redaction_enabled(s.ai_redaction_enabled());
                        win.set_ai_auto_run_readonly(s.ai_auto_run_readonly());
                        win.set_ai_yolo_mode(s.ai_yolo_mode());
                    }
                });
            },
        );
    }
}

/// 打开面板（新建会话 / 快捷键入口用）。
fn ensure_panel_open(core: &AppCore, win: &Rc<AppWindow>) {
    if !win.get_ai_open() {
        set_panel_open(core, win, true);
    }
}

/// 窗口换了之后，把面板当前查看的会话重放一遍（tab 拖动 / 窗口重建后调用）。
pub(super) fn resync_active(core: &AppCore, win: &Rc<AppWindow>) {
    let id = win.get_ai_active_id().to_string();
    if id.is_empty() {
        return;
    }
    apply_conversation(core, win, &id);
}

/// 会话落盘：500ms 防抖批量写（不逐 token 写盘），退出时同步 flush 一次。
///
/// 落盘前消息过 `redact`（与终端输出出网同一套规则），文件按敏感文件保管
/// （unix 0600、tmp + rename 原子写）。
pub(super) fn install_persist(core: &Rc<AppCore>) {
    let conversations = core.conversations.clone();
    // 防抖计时器只能在 Slint 线程上用（`slint::Timer` 是 Rc）；会话表是
    // 跨线程的，所以这里传递的只是「唤一下事件循环」的 Send 闭包。
    let handle = conversations.clone();
    let mut store = match handle.lock() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    };
    store.set_persist_hook(Some(Arc::new(move || {
        let conversations = conversations.clone();
        let _ = slint::invoke_from_event_loop(move || {
            schedule_flush(conversations);
        });
    })));
}

thread_local! {
    static PERSIST_TIMER: RefCell<Option<Rc<slint::Timer>>> = const { RefCell::new(None) };
}

/// 排一次 500ms 后的写盘（已有排队就合并）。
fn schedule_flush(conversations: ConversationStoreHandle) {
    let timer = PERSIST_TIMER.with(|t| {
        let existing = t.borrow().clone();
        match existing {
            Some(timer) => timer,
            None => {
                let timer = Rc::new(slint::Timer::default());
                *t.borrow_mut() = Some(timer.clone());
                timer
            }
        }
    });
    timer.start(
        slint::TimerMode::SingleShot,
        Duration::from_millis(500),
        move || {
            flush_conversations(&conversations);
        },
    );
}

/// 立刻把会话写盘（防抖到期 / 应用退出）。
pub(super) fn flush_conversations(conversations: &ConversationStoreHandle) {
    let mut guard = match conversations.lock() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    };
    guard.clear_pending();
    // 落盘前脱敏：会话文件与 sessions.json 同级，按敏感文件保管。
    let redactor = ai_copilot::redact::IndexedRedactor::builtin();
    if let Err(e) = guard.save(Some(&redactor)) {
        tracing::warn!("failed to persist AI conversations: {e}");
    }
}

/// 标签关闭：它的会话保留，只是锚点失效。
pub(super) fn on_tab_closed(core: &AppCore, tab_id: &str) {
    let mut guard = match core.conversations.lock() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    };
    guard.mark_anchor_lost(tab_id);
    guard.touch();
}

/// 应用退出：取消所有在跑的轮次并同步 flush 一次落盘。
pub(super) fn shutdown(core: &AppCore) {
    {
        let mut guard = match core.conversations.lock() {
            Ok(g) => g,
            Err(e) => e.into_inner(),
        };
        guard.cancel_all();
    }
    flush_conversations(&core.conversations);
}
