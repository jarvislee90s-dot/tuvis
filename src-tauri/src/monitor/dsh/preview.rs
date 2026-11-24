// dsh 标题与消息预览（设计 P3）：
// 预览只取真人输入（source.kind=="user"）与助手回复；注入内容一律过滤；
// 助手纯 tool-call 帧回落「执行 <工具名>」

use crate::monitor::dsh::log::DshEvent;
use crate::monitor::dsh::projcache::ProjcacheView;

fn content_texts(value: &serde_json::Value) -> Vec<(String, String)> {
    // content[] → (type, text_or_name)
    value
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|c| {
                    let ty = c.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    let text = c.get("text").and_then(|v| v.as_str()).map(String::from);
                    let name = c.get("name").and_then(|v| v.as_str()).map(String::from);
                    match (ty, text, name) {
                        ("text", Some(t), _) => Some((ty.to_string(), t)),
                        ("tool-call", _, Some(n)) => Some((ty.to_string(), n)),
                        _ => None,
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

/// 预览文本截断（对齐 kimi_parser 口径）：超 100 字符取前 100 字符 + "..."
/// （卡片单行预览，超长文本不截断会撑爆布局；按字符数切，中文不碎字）
fn truncate_100(text: String) -> String {
    if text.chars().count() > 100 {
        format!("{}...", text.chars().take(100).collect::<String>())
    } else {
        text
    }
}

/// (role, text)：真人输入与助手回复取 seq 更新的一方
pub fn extract(events: &[DshEvent]) -> (Option<String>, Option<String>) {
    let mut last_user: Option<(i64, String)> = None;
    let mut last_assistant: Option<(i64, String)> = None;

    for e in events {
        let seq = e.seq.unwrap_or(0);
        match e.kind.as_str() {
            "user/message" => {
                // 注入过滤：只有 source.kind == "user" 是真人输入（M0 F8）
                if e.data.pointer("/source/kind").and_then(|v| v.as_str()) == Some("user") {
                    let text =
                        content_texts(e.data.get("content").unwrap_or(&serde_json::Value::Null))
                            .into_iter()
                            .map(|(_, t)| t)
                            .collect::<Vec<_>>()
                            .join(" ");
                    if !text.is_empty() {
                        last_user = Some((seq, text));
                    }
                }
            }
            "assistant/message" => {
                let content = e
                    .data
                    .pointer("/message/content")
                    .unwrap_or(&serde_json::Value::Null);
                let parts = content_texts(content);
                let text = if parts.iter().any(|(ty, _)| ty == "text") {
                    parts
                        .into_iter()
                        .filter(|(ty, _)| ty == "text")
                        .map(|(_, t)| t)
                        .collect::<Vec<_>>()
                        .join(" ")
                } else if let Some((_, name)) = parts.first() {
                    format!("执行 {}", name) // 纯工具帧回落（设计 P3）
                } else {
                    String::new()
                };
                if !text.is_empty() {
                    last_assistant = Some((seq, text));
                }
            }
            _ => {}
        }
    }

    // 返回前统一截断（I3 终审：预览无长度上限会撑爆卡片单行预览）
    match (last_user, last_assistant) {
        (Some(u), Some(a)) => {
            if a.0 >= u.0 {
                (Some("assistant".into()), Some(truncate_100(a.1)))
            } else {
                (Some("user".into()), Some(truncate_100(u.1)))
            }
        }
        (Some(u), None) => (Some("user".into()), Some(truncate_100(u.1))),
        (None, Some(a)) => (Some("assistant".into()), Some(truncate_100(a.1))),
        (None, None) => (None, None),
    }
}

/// 标题：projcache 优先 → session/title 最新事件（provider 生成覆盖 fallback，M0 F6）
pub fn title(events: &[DshEvent], cache: Option<&ProjcacheView>) -> Option<String> {
    if let Some(t) = cache.and_then(|c| c.title.clone()) {
        return Some(t);
    }
    events
        .iter()
        .filter(|e| e.kind == "session/title")
        .filter_map(|e| {
            e.data
                .get("title")
                .and_then(|v| v.as_str())
                .map(String::from)
        })
        .next_back()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitor::dsh::log::DshEvent;
    use serde_json::json;

    fn ev(kind: &str, seq: i64, data: serde_json::Value) -> DshEvent {
        DshEvent {
            kind: kind.into(),
            seq: Some(seq),
            time: None,
            data,
        }
    }

    #[test]
    fn filters_injected_user_messages() {
        // M0 F8：只有 source.kind=="user" 是真人输入；agent-instructions/skill-catalog 注入必须过滤
        let events = vec![
            ev(
                "user/message",
                8,
                json!({
                "content": [ { "type": "text", "text": "reply ok" } ],
                "source": { "kind": "user" } }),
            ),
            ev(
                "user/message",
                9,
                json!({
                "content": [ { "type": "text", "text": "<16913 chars AGENTS.md>" } ],
                "source": { "kind": "agent-instructions" } }),
            ),
            ev(
                "user/message",
                11,
                json!({
                "content": [ { "type": "text", "text": "<8894 chars skills>" } ],
                "source": { "kind": "skill-catalog" } }),
            ),
        ];
        let (role, text) = extract(&events);
        assert_eq!(role.as_deref(), Some("user"));
        assert_eq!(text.as_deref(), Some("reply ok"));
    }

    #[test]
    fn assistant_text_and_tool_fallback() {
        let mut events = vec![
            ev(
                "user/message",
                8,
                json!({
                "content": [ { "type": "text", "text": "run it" } ],
                "source": { "kind": "user" } }),
            ),
            ev(
                "assistant/message",
                15,
                json!({
                "message": { "content": [ { "type": "text", "text": "mock ok" } ] } }),
            ),
        ];
        let (role, text) = extract(&events);
        assert_eq!(role.as_deref(), Some("assistant"));
        assert_eq!(text.as_deref(), Some("mock ok"));
        // 纯 tool-call 帧 → 回落文案「执行 <工具名>」（M0 F8 边界）
        events.push(ev(
            "assistant/message",
            16,
            json!({
            "message": { "content": [ { "type": "tool-call", "name": "bash" } ] } }),
        ));
        let (role, text) = extract(&events);
        assert_eq!(role.as_deref(), Some("assistant"));
        assert_eq!(text.as_deref(), Some("执行 bash"));
    }

    #[test]
    fn golden_sample1_preview() {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/dsh/sample1-completed.sanitized.jsonl");
        let events = crate::monitor::dsh::log::parse_events(&std::fs::read_to_string(&p).unwrap());
        let (role, text) = extract(&events);
        assert_eq!(role.as_deref(), Some("assistant"));
        assert_eq!(text.as_deref(), Some("mock ok"));
        // 标题回落：session/title 事件（sanitized 样本内 fallback title 为 "reply ok"）
        let t = title(&events, None);
        assert!(t.is_some());
    }

    #[test]
    fn golden_v4_completed_preview_and_title() {
        // v4 夹具：真人输入（source.kind=user）+ 助手回复 + 注入（agent-instructions /
        // skill-catalog / system-message 不在 user/message 流）——语义与 v3 完全同套
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/dsh/sample7-v4-completed.sanitized.jsonl");
        let events = crate::monitor::dsh::log::parse_events(&std::fs::read_to_string(&p).unwrap());
        let (role, text) = extract(&events);
        assert_eq!(role.as_deref(), Some("assistant"));
        assert_eq!(text.as_deref(), Some("v4 done"));
        // 标题：session/title 最新事件（provider 生成 "v4 done" 覆盖 fallback "v4 hello"）
        let t = title(&events, None);
        assert_eq!(t.as_deref(), Some("v4 done"));
    }

    #[test]
    fn golden_v4_running_preview_prefers_real_user_input() {
        // 运行中 v4：末条可见消息 = 真人输入（agent-instructions 注入 seq 更新也不覆盖）
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/dsh/sample8-v4-running.sanitized.jsonl");
        let events = crate::monitor::dsh::log::parse_events(&std::fs::read_to_string(&p).unwrap());
        let (role, text) = extract(&events);
        assert_eq!(role.as_deref(), Some("user"));
        assert_eq!(text.as_deref(), Some("v4 keep going"));
    }

    #[test]
    fn title_prefers_projcache() {
        let events = vec![ev("session/title", 3, json!({ "title": "from-log" }))];
        let cache = crate::monitor::dsh::projcache::ProjcacheView {
            title: Some("from-cache".into()),
            last_prompt_at: None,
            open_turn: None,
        };
        assert_eq!(title(&events, Some(&cache)).as_deref(), Some("from-cache"));
        assert_eq!(title(&events, None).as_deref(), Some("from-log"));
    }

    #[test]
    fn long_preview_truncated_to_100_chars() {
        // I3 终审：预览超长文本截断（对齐 kimi_parser 口径：前 100 字符 + "..."）。
        // 用中文字符验证按字符数切（非字节切，多字节不碎字）
        let long = "长".repeat(250);
        let events = vec![ev(
            "user/message",
            8,
            json!({
            "content": [ { "type": "text", "text": long } ],
            "source": { "kind": "user" } }),
        )];
        let (role, text) = extract(&events);
        assert_eq!(role.as_deref(), Some("user"));
        let text = text.unwrap();
        assert_eq!(text.chars().count(), 103, "100 字符 + 省略号 3 字符");
        assert!(text.ends_with("..."));
        assert_eq!(text.chars().take(100).collect::<String>(), "长".repeat(100));
        // 恰 100 字符不截断（kimi 同款边界：>100 才切）
        let exact = "x".repeat(100);
        let events = vec![ev(
            "user/message",
            8,
            json!({
            "content": [ { "type": "text", "text": exact.clone() } ],
            "source": { "kind": "user" } }),
        )];
        let (_, text) = extract(&events);
        assert_eq!(text.as_deref(), Some(exact.as_str()));
    }
}
