//! 记忆（Memory）：用户显式管理的跨会话事实 / 偏好。
//!
//! 需求见 `docs/AI_PANEL_REQUIREMENTS.md` F7：**不做**无提示的自动提炼，
//! 写入只有两条路 —— 用户手填，或者用户在面板里确认一张 AI 提议卡片。
//!
//! 注入点在 system prompt 末尾（`render_section`），且每条内容都会在离开
//! 本机前过一遍本地脱敏规则（`redact_for_prompt`）：记忆由用户自己写，
//! 但里面照样可能贴了内网 IP 或 token。

use serde::{Deserialize, Serialize};

use crate::redact::{IndexedRedactor, RedactionMap};

/// 总量上限：防 prompt 膨胀（一条记忆会被注入到每一轮请求里）。
pub const MAX_MEMORIES: usize = 50;
/// 单条上限（字符数）。
pub const MAX_MEMORY_CHARS: usize = 500;

/// 记忆的来源。`manual` = 用户手填；`ai-proposed` = AI 提议、用户确认后落库。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum MemorySource {
    #[default]
    Manual,
    AiProposed,
}

impl MemorySource {
    pub fn as_str(self) -> &'static str {
        match self {
            MemorySource::Manual => "manual",
            MemorySource::AiProposed => "ai-proposed",
        }
    }

    pub fn parse(s: &str) -> Self {
        if s.eq_ignore_ascii_case("ai-proposed") {
            MemorySource::AiProposed
        } else {
            MemorySource::Manual
        }
    }
}

/// 一条记忆。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryRecord {
    pub id: String,
    pub content: String,
    #[serde(default)]
    pub source: MemorySource,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// 写入时间（rfc3339，空串 = 老数据没记）。
    #[serde(default)]
    pub created_at: String,
}

fn default_enabled() -> bool {
    true
}

impl MemoryRecord {
    pub fn new(id: impl Into<String>, content: impl Into<String>, source: MemorySource) -> Self {
        Self {
            id: id.into(),
            content: content.into(),
            source,
            enabled: true,
            created_at: String::new(),
        }
    }
}

/// CLI / 宿主校验记忆内容的规则：空内容、重复内容、超长都拒。
///
/// 返回规范化后的内容（去首尾空白、按字符截断到上限）。
pub fn validate_content(
    content: &str,
    existing: &[MemoryRecord],
    editing_id: Option<&str>,
) -> Result<String, String> {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return Err("memory content must not be empty".into());
    }
    let normalized: String = trimmed.chars().take(MAX_MEMORY_CHARS).collect();
    if existing
        .iter()
        .any(|m| m.content.trim() == normalized && Some(m.id.as_str()) != editing_id)
    {
        return Err("the same memory already exists".into());
    }
    Ok(normalized)
}

/// 注入 system prompt 的「记住这些」段落。空列表返回空串（不出现段落）。
///
/// `redactor` 为 Some 时每条内容先过脱敏 —— 占位符写进 `map`，模型回显的
/// 内容在 UI 侧还能还原成真实值。
pub fn render_section(memories: &[MemoryRecord], redactor: Option<&IndexedRedactor>) -> String {
    let active: Vec<&MemoryRecord> = memories
        .iter()
        .filter(|m| m.enabled && !m.content.trim().is_empty())
        .collect();
    if active.is_empty() {
        return String::new();
    }
    let mut s = String::from(
        "\n\n---\n\n# User memory (explicitly saved by the user; treat as standing instructions)\n\n",
    );
    s.push_str(
        "These are facts and preferences the user saved in FengheShell. Honour them unless the \
         user's current message overrides them. Never repeat them back verbatim unless asked.\n\n",
    );
    for (index, m) in active.iter().enumerate() {
        let mut map = RedactionMap::new();
        let text = match redactor {
            Some(r) if !r.is_empty() => r.apply(m.content.trim(), &mut map),
            _ => m.content.trim().to_string(),
        };
        s.push_str(&format!("{}. {}\n", index + 1, text));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem(content: &str) -> MemoryRecord {
        MemoryRecord::new("m1", content, MemorySource::Manual)
    }

    #[test]
    fn empty_list_renders_nothing() {
        assert!(render_section(&[], None).is_empty());
        let empty = MemoryRecord::new("m", "", MemorySource::Manual);
        assert!(render_section(&[empty], None).is_empty());
    }

    #[test]
    fn disabled_memories_are_not_injected() {
        let mut m = mem("生产集群禁执行 kubectl delete");
        m.enabled = false;
        assert!(render_section(&[m], None).is_empty());
    }

    #[test]
    fn enabled_memory_reaches_the_prompt_section() {
        let section = render_section(&[mem("生产集群禁执行 kubectl delete")], None);
        assert!(section.contains("User memory"));
        assert!(section.contains("1. 生产集群禁执行 kubectl delete"));
    }

    #[test]
    fn memory_is_redacted_before_it_leaves_the_machine() {
        let redactor = IndexedRedactor::builtin();
        let section = render_section(
            &[mem("跳板机是 10.1.2.3，密钥 sk-abcdefghijklmnopqrstuvwx")],
            Some(&redactor),
        );
        assert!(
            !section.contains("10.1.2.3"),
            "private IP leaked: {section}"
        );
        assert!(!section.contains("sk-abcdefghijklmnopqrstuvwx"));
        assert!(section.contains("<INTERNAL_IP_1>"));
    }

    #[test]
    fn validate_rejects_empty_duplicate_and_truncates_long_content() {
        let existing = vec![mem("keep sshd on")];
        assert!(validate_content("   ", &existing, None).is_err());
        assert!(validate_content("keep sshd on", &existing, None).is_err());
        // 编辑自己时不算重复。
        assert_eq!(
            validate_content("keep sshd on", &existing, Some("m1")).unwrap(),
            "keep sshd on"
        );
        let long = "x".repeat(MAX_MEMORY_CHARS + 40);
        assert_eq!(
            validate_content(&long, &[], None).unwrap().chars().count(),
            MAX_MEMORY_CHARS
        );
    }

    #[test]
    fn source_round_trips_through_serde() {
        let m = MemoryRecord::new("m", "x", MemorySource::AiProposed);
        let raw = serde_json::to_string(&m).unwrap();
        assert!(raw.contains("\"ai-proposed\""));
        let back: MemoryRecord = serde_json::from_str(&raw).unwrap();
        assert_eq!(back.source, MemorySource::AiProposed);
    }
}
