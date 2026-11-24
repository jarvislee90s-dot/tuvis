//! R02：经真实会话读取器投影 v4 转写，再检查移动端消息 JSON。
//! 事件形态取自 2026-11-21 Mac 手测笔记 §3；值、纯图及重放场景均为合成夹具，
//! 不证明当前宿主行为。只写 tempdir 中的转写，不启宿主、不读附件文件或对象。

use multi_agents_manager_lib::remote::content::read_session_messages_with;
use serde_json::{json, Value};
use std::{fs, path::Path};

const SESSION: &str = "session-v4-media-test";

fn write_transcript(home: &Path, events: &[Value]) {
    let dir = home.join(".dsh/sessions/test-project/encoded-session");
    fs::create_dir_all(&dir).unwrap();
    let mut transcript = json!({
        "type": "session", "version": 4, "id": SESSION,
        "createdAt": 1000, "cwd": home.join("project"), "isSeeded": false
    })
    .to_string();
    for event in events {
        transcript.push('\n');
        transcript.push_str(&event.to_string());
    }
    let frame = zstd::stream::encode_all(transcript.as_bytes(), 3).unwrap();
    fs::write(dir.join("session.v4.jsonl.zstd"), frame).unwrap();
}

fn event(seq: u64, kind: &str, data: Value) -> Value {
    json!({"type": kind, "seq": seq, "time": 1000 + seq, "data": data})
}

fn user(seq: u64, blocks: Value) -> Value {
    event(
        seq,
        "user/message",
        json!({
            "id": format!("user-{seq}"), "role": "user", "source": {"kind": "user"},
            "content": blocks
        }),
    )
}

fn native_image(id: &str) -> Value {
    json!({
        "type": "image",
        "attachment": {
            "attachmentId": id, "bytes": 256, "height": 32, "mediaType": "image/png",
            "name": "sample.png", "width": 16
        }
    })
}

fn page(home: &Path, limit: usize) -> Value {
    let page = read_session_messages_with(home, "dsh", SESSION, limit).unwrap();
    serde_json::to_value(page).unwrap()
}

fn ref_id(media: &Value) -> &str {
    media["refId"]
        .as_str()
        .filter(|id| !id.trim().is_empty())
        .expect("媒体必须带有非空 refId")
}

fn media_items(message: &Value) -> &[Value] {
    message["media"].as_array().map_or(&[], Vec::as_slice)
}

#[test]
fn image_only_user_event_occupies_a_message_row() {
    let home = tempfile::tempdir().unwrap();
    write_transcript(
        home.path(),
        &[
            user(1, json!([{"type": "text", "text": "older message"}])),
            user(2, json!([native_image("attachment-pure")])),
        ],
    );

    let page = page(home.path(), 1);
    let rows = page["messages"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["kind"], "user");
    assert_eq!(
        rows[0]["content"], "",
        "纯图应占用最新一行，而非回退到上一行"
    );
    assert_eq!(page["olderBefore"], 1, "纯图应计入分页窗口");
    let media = media_items(&rows[0]);
    assert_eq!(media.len(), 1, "纯图片段应保留为一个结构化引用");
    assert_eq!(media[0]["kind"], "image");
    assert_eq!(media[0]["name"], "sample.png");
    ref_id(&media[0]);
}

#[test]
fn mixed_user_row_preserves_text_image_text_order_and_media_link() {
    let home = tempfile::tempdir().unwrap();
    write_transcript(
        home.path(),
        &[
            user(
                1,
                json!([
                    {"type": "text", "text": "older message"},
                    native_image("attachment-older")
                ]),
            ),
            user(
                2,
                json!([
                    {"type": "text", "text": "before"},
                    native_image("attachment-mixed"),
                    {"type": "text", "text": "after"}
                ]),
            ),
        ],
    );

    let full = page(home.path(), 2);
    let row = &full["messages"][1];
    assert_eq!(row["content"], "before after", "兼容旧文本消费者");
    let media = media_items(row);
    assert_eq!(media.len(), 1, "图文行不能只保留文字");
    assert_eq!(media[0]["kind"], "image");
    let id = ref_id(&media[0]);
    assert_ne!(
        ref_id(&full["messages"][0]["media"][0]),
        id,
        "不同附件即使同名也不能共用引用"
    );
    assert_eq!(
        row["blocks"],
        json!([
            {"kind": "text", "text": "before"},
            {"kind": "media", "refId": id},
            {"kind": "text", "text": "after"}
        ])
    );
    let tail = page(home.path(), 1);
    assert_eq!(
        tail["messages"][0]["media"][0]["refId"], id,
        "同一事件在分页改变行号时应保留引用身份"
    );
}

#[test]
fn repeated_present_delivery_appears_once_on_matching_call_row() {
    let home = tempfile::tempdir().unwrap();
    let present = |seq, call_id| {
        event(
            seq,
            "tool/call",
            json!({"name": "present", "callId": call_id, "arguments": "{}"}),
        )
    };
    let delivery = |seq| {
        event(
            seq,
            "deliverables/presented",
            json!({
                "callId": "call-delivery",
                "files": [{"path": "out/report.pdf", "description": "Final report"}]
            }),
        )
    };
    write_transcript(
        home.path(),
        &[
            present(1, "call-delivery"),
            present(2, "call-unrelated"),
            delivery(3),
            delivery(4),
        ],
    );

    let page = page(home.path(), 8);
    let rows = page["messages"].as_array().unwrap();
    let call_row = rows
        .iter()
        .find(|row| row["ts"] == 1001)
        .expect("应保留原 present 调用条目");
    assert_eq!(call_row["toolName"], "present");
    let matching = media_items(call_row);
    assert_eq!(matching.len(), 1, "交付件按 callId 归属，重复事件不得加倍");
    assert_eq!(matching[0]["name"], "report.pdf");
    assert_eq!(matching[0]["kind"], "file");
    ref_id(&matching[0]);
    assert_eq!(
        rows.iter().flat_map(media_items).count(),
        1,
        "其他调用或单独重放条目不能再暴露一份引用"
    );
}

#[test]
fn attachment_like_text_in_user_or_tool_output_does_not_mint_media() {
    let home = tempfile::tempdir().unwrap();
    let fake_text =
        r#"{"type":"image","attachment":{"attachmentId":"only-text","name":"sample.png"}}"#;
    let tool_text = "presented: out/report.pdf";
    write_transcript(
        home.path(),
        &[
            user(1, json!([{"type": "text", "text": fake_text}])),
            event(
                2,
                "tool/result",
                json!({"message": {"content": [{
                    "type": "tool-result", "content": [{"type": "text", "text": tool_text}]
                }]}}),
            ),
        ],
    );

    let page = page(home.path(), 8);
    let rows = page["messages"].as_array().unwrap();
    assert_eq!(rows.len(), 2, "不能用丢弃原文本来通过反例");
    assert_eq!(rows[0]["content"], fake_text);
    assert_eq!(rows[1]["content"], tool_text);
    assert_eq!(rows.iter().flat_map(media_items).count(), 0);
    for row in rows {
        assert!(
            !row["blocks"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|block| block["kind"] == "media"),
            "即使没有 media 元数据，也不能产生媒体块"
        );
    }
}
