//! app/ai_store.rs 的纯逻辑单测：落盘 round-trip、恢复规则、上限、脱敏。
//! 不需要 Slint / SSH —— 只操作内存里的会话表与临时文件。

use super::*;
use crate::app::ai::{ROLE_CARD, ROLE_USER, UNTITLED_TITLE};
use ai_copilot::triage::CopilotMode;

fn msg(role: i32, text: &str) -> AiMessage {
    AiMessage {
        role,
        text: text.into(),
        ..Default::default()
    }
}

fn card(id: &str, disposition: &str) -> AiMessage {
    AiMessage {
        role: ROLE_CARD,
        card_id: id.into(),
        cmd: "df -h".into(),
        disposition: disposition.into(),
        gate: "direct".into(),
        ..Default::default()
    }
}

fn tmp_path(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("fengheshell-ai-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(format!("{name}.json"))
}

#[test]
fn store_round_trips_through_disk() {
    let path = tmp_path("roundtrip");
    let _ = std::fs::remove_file(&path);
    let mut store = ConversationStore::with_path(Some(path.clone()));
    let id = store.create(
        Some("tab-1".into()),
        CopilotMode::Agent,
        UNTITLED_TITLE.into(),
    );
    {
        let conv = store.get_mut(&id).unwrap();
        conv.messages.push(msg(ROLE_USER, "nginx 502 怎么排查"));
        conv.messages.push(card("cmd-1", "executed"));
        conv.derive_title("nginx 502 怎么排查");
    }
    store.save(None).expect("write");

    let mut restored = ConversationStore::with_path(Some(path.clone()));
    restored.load(None);
    let conv = restored.get(&id).expect("conversation survived");
    assert_eq!(conv.title, "nginx 502 怎么排查");
    assert_eq!(conv.anchor_tab.as_deref(), Some("tab-1"));
    assert_eq!(conv.mode, CopilotMode::Agent);
    assert_eq!(conv.messages.len(), 2);
    assert_eq!(conv.messages[1].card_id.as_str(), "cmd-1");
    assert_eq!(conv.messages[1].disposition.as_str(), "executed");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn running_and_pending_are_neutralised_on_load() {
    // 审批通道是进程内对象：重启后 pending 卡片一律作废，running 一律中断。
    let stored = StoredStore {
        version: 1,
        active_id: "c1".into(),
        conversations: vec![StoredConversation {
            id: "c1".into(),
            title: "半路重启".into(),
            anchor_tab: Some("tab-1".into()),
            anchor_lost: false,
            mode: "ask".into(),
            model_ref: None,
            memory_enabled: true,
            status: "running".into(),
            created_at: None,
            updated_at: None,
            messages: vec![
                StoredMessage::from(&msg(ROLE_USER, "看看磁盘")),
                StoredMessage::from(&card("cmd-9", "pending")),
            ],
        }],
    };
    let mut store = ConversationStore::in_memory();
    store.load_stored(stored);
    let conv = store.get("c1").unwrap();
    assert_eq!(conv.status, ConversationStatus::Interrupted);
    assert!(!conv.is_busy());
    assert_eq!(conv.messages[1].disposition.as_str(), "expired");
    assert_eq!(store.active_id, "c1");
}

#[test]
fn corrupt_file_is_kept_aside_instead_of_silently_cleared() {
    let path = tmp_path("corrupt");
    std::fs::write(&path, b"{ this is not json").unwrap();
    let mut store = ConversationStore::with_path(Some(path.clone()));
    store.load(None);
    assert!(store.conversations.is_empty());
    // 原文件被改名留档，用户还能自己抢救。
    let kept = path.with_extension("json.corrupt");
    assert!(kept.exists(), "corrupt file must be preserved");
    let _ = std::fs::remove_file(&kept);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn persisted_file_is_owner_only() {
    let path = tmp_path("perm");
    let _ = std::fs::remove_file(&path);
    let mut store = ConversationStore::with_path(Some(path.clone()));
    store.create(None, CopilotMode::Ask, "x".into());
    store.save(None).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "conversation file must stay owner-only");
    }
    let _ = std::fs::remove_file(&path);
}

#[test]
fn secrets_are_redacted_before_they_hit_the_disk() {
    let path = tmp_path("redact");
    let _ = std::fs::remove_file(&path);
    let mut store = ConversationStore::with_path(Some(path.clone()));
    let id = store.create(None, CopilotMode::Ask, "redact".into());
    store.get_mut(&id).unwrap().messages.push(msg(
        ROLE_USER,
        "curl -H 'Authorization: Bearer abcdef123456' http://10.1.2.3/api sk-abcdefghijklmnopqrstuvwx",
    ));
    let redactor = ai_copilot::redact::IndexedRedactor::builtin();
    store.save(Some(&redactor)).unwrap();
    let raw = std::fs::read_to_string(&path).unwrap();
    assert!(!raw.contains("10.1.2.3"), "private IP leaked: {raw}");
    assert!(!raw.contains("sk-abcdefghijklmnopqrstuvwx"), "key leaked");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn cards_survive_trimming_while_old_plain_messages_do_not() {
    let mut rows: Vec<AiMessage> = Vec::new();
    rows.push(card("cmd-old", "executed"));
    for i in 0..(MAX_MESSAGES_PER_CONVERSATION + 40) {
        rows.push(msg(ROLE_USER, &format!("m{i}")));
    }
    rows.push(card("cmd-new", "executed"));
    trim_messages(&mut rows);
    assert!(rows.len() <= MAX_MESSAGES_PER_CONVERSATION);
    // 卡片是审计痕迹，不参与淘汰。
    assert!(rows.iter().any(|m| m.card_id.as_str() == "cmd-old"));
    assert!(rows.iter().any(|m| m.card_id.as_str() == "cmd-new"));
}

#[test]
fn concurrency_gate_counts_only_running_rounds() {
    let mut store = ConversationStore::in_memory();
    let a = store.create(None, CopilotMode::Ask, "a".into());
    let b = store.create(None, CopilotMode::Ask, "b".into());
    assert_eq!(store.running_count(), 0);
    store.get_mut(&a).unwrap().begin_round();
    store.get_mut(&b).unwrap().begin_round();
    assert_eq!(store.running_count(), 2);
    let gen = store.get_mut(&a).unwrap().generation;
    assert!(store.get_mut(&a).unwrap().finish_round(gen));
    assert_eq!(store.running_count(), 1);
}

#[test]
fn stopping_a_round_bumps_the_generation_and_rejects_the_card() {
    let mut store = ConversationStore::in_memory();
    let id = store.create(None, CopilotMode::Ask, "stop".into());
    let conv = store.get_mut(&id).unwrap();
    conv.messages.push(msg(ROLE_USER, "看看磁盘"));
    let flag = conv.begin_round();
    let (tx, mut rx) = tokio::sync::oneshot::channel();
    conv.set_approval("cmd-1".into(), tx);
    let gen = conv.cancel_generation();
    assert_eq!(gen, 1);
    assert!(flag.load(std::sync::atomic::Ordering::Relaxed));
    assert_eq!(
        rx.try_recv(),
        Ok(false),
        "pending approval becomes a rejection"
    );
    // 世代变了，在飞的旧 ask() 不能再往回写（也不会把状态改回 idle）。
    assert!(!conv.finish_round(0));
    assert_eq!(conv.status, ConversationStatus::Interrupted);
}

#[test]
fn closing_the_anchor_tab_keeps_the_conversation() {
    let mut store = ConversationStore::in_memory();
    let id = store.create(Some("tab-9".into()), CopilotMode::Ask, "anchored".into());
    store.mark_anchor_lost("tab-9");
    let conv = store.get(&id).expect("conversation outlives its tab");
    assert!(conv.anchor_lost);
    assert!(by_anchor(&store, "tab-9").is_none());
}

#[test]
fn removing_a_conversation_repoints_the_active_id() {
    let mut store = ConversationStore::in_memory();
    let a = store.create(None, CopilotMode::Ask, "a".into());
    let b = store.create(None, CopilotMode::Ask, "b".into());
    assert_eq!(store.active_id, b);
    assert!(store.remove(&b));
    assert_eq!(store.active_id, a);
    assert!(store.get(&b).is_none());
}

#[test]
fn title_defaults_to_the_first_question() {
    let mut store = ConversationStore::in_memory();
    let id = store.create(None, CopilotMode::Ask, UNTITLED_TITLE.into());
    let conv = store.get_mut(&id).unwrap();
    conv.derive_title("磁盘快满了，帮我找出最大的目录并给出清理建议，越详细越好");
    assert_eq!(
        conv.title.chars().count(),
        "磁盘快满了，帮我找出最大的目录并给出清理建议，越详细越好"
            .chars()
            .count()
            .min(30)
    );
    // 已经有名字就不再覆盖。
    conv.derive_title("另一句话");
    assert!(conv.title.starts_with("磁盘快满了"));
}

#[test]
fn explicit_rename_survives_later_questions() {
    let mut store = ConversationStore::in_memory();
    let id = store.create(None, CopilotMode::Ask, UNTITLED_TITLE.into());
    let conv = store.get_mut(&id).unwrap();
    // UI 侧重命名走的是 title 字段直接赋值，这里模拟一次。
    conv.title = "我的排障会话".into();
    conv.derive_title("新提问");
    assert_eq!(conv.title, "我的排障会话");
}

#[test]
fn summaries_are_newest_first_and_carry_the_anchor_name() {
    let mut store = ConversationStore::in_memory();
    let a = store.create(Some("tab-a".into()), CopilotMode::Ask, "a".into());
    let b = store.create(Some("tab-b".into()), CopilotMode::Ask, "b".into());
    store.get_mut(&a).unwrap().updated_at = chrono::Utc::now() - chrono::Duration::seconds(60);
    let list = summaries(&store, |tab| format!("name:{tab}"));
    assert_eq!(list[0].id, b);
    assert_eq!(list[0].anchor_name, "name:tab-b");
    assert!(list[0].anchor_alive);
    assert_eq!(list[1].id, a);
}

#[test]
fn save_without_a_path_is_a_no_op() {
    let mut store = ConversationStore::in_memory();
    store.create(None, CopilotMode::Ask, "x".into());
    store
        .save(None)
        .expect("in-memory stores never touch the disk");
}

// --- F2 / F3 / F7：会话级模型、模式、记忆开关 --------------------------------

#[test]
fn model_ref_mode_and_memory_survive_a_round_trip() {
    let path = tmp_path("session-fields");
    let _ = std::fs::remove_file(&path);
    let mut store = ConversationStore::with_path(Some(path.clone()));
    let id = store.create(None, CopilotMode::Ask, "会话".into());
    assert!(store.set_model_ref(&id, Some("deepseek-chat".into())));
    assert!(store.set_mode(&id, CopilotMode::Agent));
    assert!(store.set_memory_enabled(&id, false));
    store.save(None).unwrap();

    let mut restored = ConversationStore::with_path(Some(path.clone()));
    restored.load(None);
    let conv = restored.get(&id).unwrap();
    assert_eq!(conv.model_ref.as_deref(), Some("deepseek-chat"));
    assert_eq!(conv.mode, CopilotMode::Agent);
    assert!(!conv.memory_enabled);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn rename_rejects_an_empty_title() {
    let mut store = ConversationStore::in_memory();
    let id = store.create(None, CopilotMode::Ask, UNTITLED_TITLE.into());
    assert!(!store.rename(&id, "   "));
    assert!(store.rename(&id, "  nginx 502  "));
    assert_eq!(store.get(&id).unwrap().title, "nginx 502");
}

// --- F6：历史检索与脱敏导出 --------------------------------------------------

#[test]
fn search_matches_titles_and_message_bodies() {
    let mut store = ConversationStore::in_memory();
    let a = store.create(None, CopilotMode::Ask, "nginx 502 排查".into());
    let b = store.create(None, CopilotMode::Ask, "磁盘占满".into());
    store
        .get_mut(&b)
        .unwrap()
        .messages
        .push(msg(ROLE_USER, "upstream sent too big header, 502 again"));
    // 标题命中。
    let hits = search_summaries(&store, "502", None, |t| t.to_string());
    assert_eq!(hits.len(), 2);
    // 只有正文命中。
    let hits = search_summaries(&store, "upstream", None, |t| t.to_string());
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].id, b);
    // 空前缀 = 全部。
    assert_eq!(search_summaries(&store, "", None, |t| t.to_string()).len(), 2);
    // 状态过滤。
    let hits = search_summaries(&store, "", Some("interrupted"), |t| t.to_string());
    assert!(hits.is_empty());
    let _ = a;
}

#[test]
fn exported_markdown_is_redacted_and_keeps_the_cards() {
    let mut store = ConversationStore::in_memory();
    let id = store.create(None, CopilotMode::Agent, "10.1.2.3 上的 502".into());
    {
        let conv = store.get_mut(&id).unwrap();
        conv.messages.push(msg(
            ROLE_USER,
            "连 10.1.2.3 的 sk-abcdefghijklmnopqrstuvwx 为什么 502",
        ));
        let mut command_card = card("cmd-1", "executed");
        command_card.risk = "read-only".into();
        command_card.explain = "看后端状态".into();
        command_card.side_effect = "只读".into();
        conv.messages.push(command_card);
        conv.messages.push(AiMessage {
            role: crate::app::ai::ROLE_PLAN_STEP,
            text: "查看 nginx 日志".into(),
            step_state: "done".into(),
            card_id: "plan-0".into(),
            ..Default::default()
        });
    }
    let conv = store.get(&id).unwrap();
    let md = export_markdown(conv, &ai_copilot::redact::IndexedRedactor::builtin());
    assert!(!md.contains("10.1.2.3"), "private IP leaked: {md}");
    assert!(!md.contains("sk-abcdefghijklmnopqrstuvwx"));
    assert!(md.contains("<INTERNAL_IP_1>"));
    assert!(md.contains("### Command card (read-only, gate direct, executed)"));
    assert!(md.contains("df -h"));
    assert!(md.contains("- [x] 查看 nginx 日志"));
}
