//! M2 设置面：模型配置（F2）、记忆（F7）、技能（F8）、并发上限（D3）。
//!
//! 这一层只做「UI ↔ 配置」的翻译，不碰终端也不碰 LLM：
//! 每个回调写盘一次、刷新模型，然后把结果同步到所有窗口（这些设置都是
//! 进程级共享的）。真正的会话行为在 `ai_callbacks` / `ai_store` 里。

use super::*;

use crate::app::ai_store::ConversationStoreHandle;
use crate::config::AiModelProfileSpec;

/// 记忆列表的展示时间：只留日期，面板里一行装得下。
fn memory_date(rfc3339: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(rfc3339)
        .map(|t| {
            t.with_timezone(&chrono::Local)
                .format("%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_default()
}

/// 设置页的模型列表（新建/编辑/删除后都要刷一遍）。
fn refresh_profile_rows(store: &Rc<RefCell<ConfigStore>>, win: &AppWindow) {
    let s = store.borrow();
    let default_name = s.ai_default_profile().name;
    let rows: Vec<AiProfileRow> = s
        .ai_model_profiles()
        .into_iter()
        .map(|p| AiProfileRow {
            name: p.name.clone().into(),
            protocol: p.protocol.into(),
            endpoint: p.endpoint.into(),
            model: p.model.into(),
            is_default: p.name == default_name,
        })
        .collect();
    win.set_ai_profiles(ModelRc::from(Rc::new(VecModel::from(rows))));
}

fn refresh_memory_rows(store: &Rc<RefCell<ConfigStore>>, win: &AppWindow) {
    let rows: Vec<AiMemoryRow> = store
        .borrow()
        .ai_memory()
        .into_iter()
        .map(|m| AiMemoryRow {
            id: m.id.into(),
            content: m.content.into(),
            source: m.source.as_str().into(),
            enabled: m.enabled,
            created: memory_date(&m.created_at).into(),
        })
        .collect();
    win.set_ai_memories(ModelRc::from(Rc::new(VecModel::from(rows))));
}

fn refresh_skill_rows(store: &Rc<RefCell<ConfigStore>>, win: &AppWindow) {
    let rows: Vec<AiSkillRow> = store
        .borrow()
        .ai_skills()
        .into_iter()
        .map(|s| AiSkillRow {
            id: s.id.into(),
            name: s.name.into(),
            description: s.description.into(),
            content: s.content.into(),
        })
        .collect();
    win.set_ai_skill_rows(ModelRc::from(Rc::new(VecModel::from(rows))));
}

/// 这些设置是进程级共享的：写盘后同步到每个窗口。
fn for_each_window(core: &AppCore, mut f: impl FnMut(&AppWindow)) {
    core.registry.for_each(|weak| {
        if let Some(w) = weak.upgrade() {
            f(&w);
        }
    });
}

/// 模型配置变了：面板的 chip 列表也要跟着变（会话锁定的是名字）。
fn sync_model_choice(core: &AppCore, store: &Rc<RefCell<ConfigStore>>) {
    let s = store.borrow();
    let default_name = s.ai_default_profile().name;
    let rows: Vec<AiProfileInfo> = s
        .ai_model_profiles()
        .into_iter()
        .map(|p| AiProfileInfo {
            name: p.name.clone().into(),
            model: p.model.into(),
            protocol: p.protocol.into(),
            is_default: p.name == default_name,
        })
        .collect();
    drop(s);
    for_each_window(core, |w| {
        w.set_ai_model_profiles(ModelRc::from(Rc::new(VecModel::from(rows.clone()))));
    });
}

/// 改了模型设置，旧的补全缓存不再适用。
fn reset_completion() {
    clear_completion_cache();
}

pub(super) fn wire_ai_m2(ctx: &WinCtx) {
    let WinCtx {
        core,
        store,
        window,
        ..
    } = ctx;

    // 播种设置页 + 面板 chip。
    refresh_profile_rows(store, window);
    refresh_memory_rows(store, window);
    refresh_skill_rows(store, window);
    window.set_ai_max_parallel(store.borrow().ai_max_parallel() as i32);
    sync_model_choice(core, store);

    // --- 模型配置 ---------------------------------------------------------
    {
        let store = store.clone();
        let core = core.clone();
        let win = window.clone();
        window.on_ai_save_profile(
            move |edit, name, protocol, endpoint, model, key, is_default| {
                let spec = AiModelProfileSpec {
                    name: name.to_string(),
                    protocol: protocol.to_string(),
                    endpoint: endpoint.to_string(),
                    model: model.to_string(),
                    api_key: Secret::new(key.to_string()),
                    default: is_default,
                };
                // 改名 = 删除旧的 + 新建；会话里记录的 model_ref 不跟着改，
                // 所以把「引用它的会话」一起迁过去，避免静默回落到默认模型。
                let edit = edit.trim().to_string();
                if !edit.is_empty() && edit != spec.name.trim() {
                    let mut s = store.borrow_mut();
                    if s.ai_profile_by_name(&edit).is_some() {
                        s.remove_ai_model_profile(&edit);
                    }
                    let renamed = spec.name.trim().to_string();
                    let convs: ConversationStoreHandle = core.conversations.clone();
                    {
                        let mut guard = convs.lock().unwrap_or_else(|e| e.into_inner());
                        for conv in guard.conversations.iter_mut() {
                            if conv.model_ref.as_deref() == Some(edit.as_str()) {
                                conv.model_ref = Some(renamed.clone());
                            }
                        }
                        guard.touch();
                    }
                }
                let outcome = {
                    let mut s = store.borrow_mut();
                    match s.upsert_ai_model_profile(spec.clone()) {
                        Ok(()) => s.save().map_err(|e| e.to_string()),
                        Err(e) => Err(e.to_string()),
                    }
                };
                let desc: String = match outcome {
                    Ok(()) => {
                        reset_completion();
                        if crate::i18n::is_en() {
                            format!("Model profile `{}` saved.", spec.name)
                        } else {
                            format!("模型配置 `{}` 已保存。", spec.name)
                        }
                    }
                    Err(e) => {
                        if crate::i18n::is_en() {
                            format!("Save failed: {e}")
                        } else {
                            format!("保存失败：{e}")
                        }
                    }
                };
                win.set_ai_profile_status(desc.into());
                refresh_profile_rows(&store, &win);
                sync_model_choice(&core, &store);
            },
        );
    }
    {
        let store = store.clone();
        let core = core.clone();
        let win = window.clone();
        window.on_ai_delete_profile(move |name: SharedString| {
            let mut s = store.borrow_mut();
            let removed = s.remove_ai_model_profile(name.as_str());
            let _ = s.save();
            drop(s);
            if removed {
                reset_completion();
                // 被删配置的会话回落到默认模型（不静默留空）。
                let convs: ConversationStoreHandle = core.conversations.clone();
                let mut guard = convs.lock().unwrap_or_else(|e| e.into_inner());
                for conv in guard.conversations.iter_mut() {
                    if conv.model_ref.as_deref() == Some(name.as_str()) {
                        conv.model_ref = None;
                    }
                }
                guard.touch();
            }
            win.set_ai_profile_status(
                if crate::i18n::is_en() {
                    format!("Model profile `{}` removed.", name.as_str())
                } else {
                    format!("模型配置 `{}` 已删除。", name.as_str())
                }
                .into(),
            );
            refresh_profile_rows(&store, &win);
            sync_model_choice(&core, &store);
        });
    }
    {
        let store = store.clone();
        let win = window.clone();
        window.on_ai_edit_profile(move |name: SharedString| {
            let found = store.borrow().ai_profile_by_name(name.as_str());
            let Some(p) = found else {
                return;
            };
            win.set_ai_profile_edit(p.name.clone().into());
            win.set_ai_profile_name(p.name.into());
            win.set_ai_profile_protocol(p.protocol.into());
            win.set_ai_profile_endpoint(p.endpoint.into());
            win.set_ai_profile_model(p.model.into());
            // 密钥是只写的：不回读，留空即保持。
            win.set_ai_profile_key("".into());
            win.set_ai_profile_default(p.default);
            win.set_ai_profile_status("".into());
        });
    }
    {
        let win = window.clone();
        window.on_ai_clear_profile_form(move || {
            win.set_ai_profile_edit("".into());
            win.set_ai_profile_name("".into());
            win.set_ai_profile_protocol("openai-completions".into());
            win.set_ai_profile_endpoint("".into());
            win.set_ai_profile_model("".into());
            win.set_ai_profile_key("".into());
            win.set_ai_profile_default(false);
            win.set_ai_profile_status("".into());
        });
    }

    // --- 记忆 -------------------------------------------------------------
    {
        let store = store.clone();
        let core = core.clone();
        let win = window.clone();
        window.on_ai_save_memory(move |id: SharedString, content: SharedString| {
            let outcome = {
                let mut s = store.borrow_mut();
                match s.upsert_ai_memory(id.as_str(), content.as_str(), "manual", true) {
                    Ok(_) => s.save().map_err(|e| e.to_string()),
                    Err(e) => Err(e.to_string()),
                }
            };
            match outcome {
                Ok(()) => {
                    win.set_ai_memory_edit("".into());
                    win.set_ai_memory_draft("".into());
                    win.set_ai_memory_status(crate::i18n::t("已保存", "Saved").into());
                }
                Err(e) => {
                    win.set_ai_memory_status(
                        if crate::i18n::is_en() {
                            format!("Rejected: {e}")
                        } else {
                            format!("未保存：{e}")
                        }
                        .into(),
                    );
                }
            }
            refresh_memory_rows(&store, &win);
            let _ = &core;
        });
    }
    {
        let store = store.clone();
        let win = window.clone();
        window.on_ai_delete_memory(move |id: SharedString| {
            let mut s = store.borrow_mut();
            s.remove_ai_memory(id.as_str());
            let _ = s.save();
            drop(s);
            win.set_ai_memory_status(crate::i18n::t("已删除", "Removed").into());
            refresh_memory_rows(&store, &win);
        });
    }
    {
        let store = store.clone();
        let win = window.clone();
        window.on_ai_toggle_memory(move |id: SharedString, on: bool| {
            let found = store
                .borrow()
                .ai_memory()
                .into_iter()
                .find(|m| m.id == id.as_str());
            let Some(m) = found else {
                return;
            };
            let mut s = store.borrow_mut();
            let _ = s.upsert_ai_memory(&m.id, &m.content, m.source.as_str(), on);
            let _ = s.save();
            drop(s);
            refresh_memory_rows(&store, &win);
        });
    }
    {
        let store = store.clone();
        let win = window.clone();
        window.on_ai_clear_memories(move || {
            let mut s = store.borrow_mut();
            s.clear_ai_memory();
            let _ = s.save();
            drop(s);
            win.set_ai_memory_edit("".into());
            win.set_ai_memory_draft("".into());
            win.set_ai_memory_status(crate::i18n::t("全部记忆已清除", "All memory cleared").into());
            refresh_memory_rows(&store, &win);
        });
    }
    {
        let store = store.clone();
        let win = window.clone();
        window.on_ai_edit_memory(move |id: SharedString| {
            let found = store
                .borrow()
                .ai_memory()
                .into_iter()
                .find(|m| m.id == id.as_str());
            let Some(m) = found else {
                return;
            };
            win.set_ai_memory_edit(m.id.into());
            win.set_ai_memory_draft(m.content.into());
            win.set_ai_memory_status("".into());
        });
    }

    // --- 技能 -------------------------------------------------------------
    {
        let store = store.clone();
        let core = core.clone();
        let win = window.clone();
        window.on_ai_save_skill(
            move |edit: SharedString,
                  id: SharedString,
                  name: SharedString,
                  desc: SharedString,
                  content: SharedString| {
                const MAX_SKILL_BYTES: usize = 8 * 1024;
                let id = id.trim().to_string();
                if id.is_empty() || id == ai_copilot::skills::GENERAL_ID {
                    win.set_ai_skill_status(
                        crate::i18n::t(
                            "id 不能为空，也不能占用内置的 general。",
                            "id must not be empty and must not take the built-in `general`.",
                        )
                        .into(),
                    );
                    return;
                }
                if content.len() > MAX_SKILL_BYTES {
                    win.set_ai_skill_status(
                        if crate::i18n::is_en() {
                            format!(
                                "Skill content is {} bytes; the limit is 8 KB.",
                                content.len()
                            )
                        } else {
                            format!("技能内容 {} 字节，超过 8 KB 上限。", content.len())
                        }
                        .into(),
                    );
                    return;
                }
                // 同 id 且不是正在编辑的那条 = 重复。
                let edit = edit.trim().to_string();
                let duplicate = store
                    .borrow()
                    .ai_skills()
                    .into_iter()
                    .any(|s| s.id == id && s.id != edit);
                if duplicate {
                    win.set_ai_skill_status(
                        if crate::i18n::is_en() {
                            format!("A skill with id `{id}` already exists.")
                        } else {
                            format!("id `{id}` 已被占用。")
                        }
                        .into(),
                    );
                    return;
                }
                let mut kept: Vec<(String, String, String, String)> = store
                    .borrow()
                    .ai_skills()
                    .into_iter()
                    .filter(|s| s.id != edit && s.id != id)
                    .map(|s| (s.id, s.name, s.description, s.content))
                    .collect();
                kept.push((
                    id.clone(),
                    name.to_string(),
                    desc.to_string(),
                    content.to_string(),
                ));
                let saved = {
                    let mut s = store.borrow_mut();
                    let saved = s.set_ai_skills(kept);
                    let write = s.save();
                    (saved, write)
                };
                match saved {
                    (n, Ok(())) => {
                        win.set_ai_skill_edit("".into());
                        win.set_ai_skill_id("".into());
                        win.set_ai_skill_name("".into());
                        win.set_ai_skill_desc("".into());
                        win.set_ai_skill_content("".into());
                        win.set_ai_skill_status(
                            if crate::i18n::is_en() {
                                format!("Saved. {n} skill(s) active; the next round picks it up.")
                            } else {
                                format!("已保存，当前 {n} 个技能生效，下一轮对话即可用。")
                            }
                            .into(),
                        );
                    }
                    (_, Err(e)) => {
                        win.set_ai_skill_status(
                            if crate::i18n::is_en() {
                                format!("Save failed: {e}")
                            } else {
                                format!("保存失败：{e}")
                            }
                            .into(),
                        );
                    }
                }
                refresh_skill_rows(&store, &win);
                let _ = &core;
            },
        );
    }
    {
        let store = store.clone();
        let win = window.clone();
        window.on_ai_delete_skill(move |id: SharedString| {
            let kept: Vec<(String, String, String, String)> = store
                .borrow()
                .ai_skills()
                .into_iter()
                .filter(|s| s.id != id.as_str())
                .map(|s| (s.id, s.name, s.description, s.content))
                .collect();
            let mut s = store.borrow_mut();
            s.set_ai_skills(kept);
            let _ = s.save();
            drop(s);
            win.set_ai_skill_status(
                if crate::i18n::is_en() {
                    format!("Skill `{}` removed.", id.as_str())
                } else {
                    format!("技能 `{}` 已删除。", id.as_str())
                }
                .into(),
            );
            refresh_skill_rows(&store, &win);
        });
    }
    {
        let store = store.clone();
        let win = window.clone();
        window.on_ai_edit_skill(move |id: SharedString| {
            let found = store
                .borrow()
                .ai_skills()
                .into_iter()
                .find(|s| s.id == id.as_str());
            let Some(sk) = found else {
                return;
            };
            win.set_ai_skill_edit(sk.id.clone().into());
            win.set_ai_skill_id(sk.id.into());
            win.set_ai_skill_name(sk.name.into());
            win.set_ai_skill_desc(sk.description.into());
            win.set_ai_skill_content(sk.content.into());
            win.set_ai_skill_status("".into());
        });
    }
    {
        let win = window.clone();
        window.on_ai_clear_skill_form(move || {
            win.set_ai_skill_edit("".into());
            win.set_ai_skill_id("".into());
            win.set_ai_skill_name("".into());
            win.set_ai_skill_desc("".into());
            win.set_ai_skill_content("".into());
            win.set_ai_skill_status("".into());
        });
    }

    // --- 并发上限 ---------------------------------------------------------
    {
        let store = store.clone();
        let core = core.clone();
        window.on_set_ai_max_parallel(move |n: i32| {
            let clamped = n.clamp(1, 5);
            {
                let mut s = store.borrow_mut();
                s.set_ai_max_parallel(clamped as usize);
                let _ = s.save();
            }
            for_each_window(&core, |w| w.set_ai_max_parallel(clamped));
        });
    }

    // --- MCP 文档入口（F9-P1）--------------------------------------------
    {
        let weak = window.as_weak();
        window.on_open_mcp_docs(move || {
            // docs/REMOTE_MCP.md 在仓库里；离线安装包旁边不一定有，所以先看
            // 本地有没有，没有就开仓库路径（用户点开浏览器自己看）。
            let rel = "docs/REMOTE_MCP.md";
            let absolute = std::env::current_exe()
                .ok()
                .and_then(|exe| exe.parent().map(|d| d.join(rel)))
                .filter(|p| p.exists());
            let message = match absolute {
                Some(path) => {
                    let text = std::fs::read_to_string(&path).unwrap_or_default();
                    if text.trim().is_empty() {
                        crate::i18n::t("文档内容为空。", "The document is empty.").to_string()
                    } else {
                        crate::i18n::t(
                            "docs/REMOTE_MCP.md 在安装目录里；这里只做提醒，完整内容请用编辑器打开。",
                            "docs/REMOTE_MCP.md lives next to the app; open it in an editor for the full text.",
                        )
                        .to_string()
                    }
                }
                None => crate::i18n::t(
                    "MCP 的协议与权限说明见仓库里的 docs/REMOTE_MCP.md。",
                    "See docs/REMOTE_MCP.md in the repository for the MCP protocol and permission matrix.",
                )
                .to_string(),
            };
            DialogOwner::of_weak(&weak)
                .message()
                .set_title(crate::i18n::t("MCP 说明", "MCP guide"))
                .set_description(message)
                .show();
        });
    }
}
