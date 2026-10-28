// M5 B1 黄金夹具（kimi/zcode 用户附件形态）：全部合成数据——路径、session id、
// uuid 均为虚构（C:/fixt/*、sess_fixt_*、1a2b3c4d…），零真实用户目录、零私有路径。
//
// 调研结论（2026-09-17 实测，详见计划文档「B1 附件调研结论」）：
// - kimi（wire.jsonl）：用户输入为纯文本 turn.steer / context.append_message；原始
//   文件路径以内联标记出现在文本里（`<image path="…">` / `<file path="…">`）；贴图
//   走 blobs/<sha256> 内容寻址（blobref:，无扩展名无原始名，不可按路径预览）。
// - zcode（cli/db/db.sqlite）：用户附件在 part 表 `{"type":"file",…}`——
//   `source.path` = 原始落盘路径（可直接预览）；无 source 的粘贴截图走
//   `zcode-artifact://<session>/<tool-result-<uuid>>`，落盘于
//   `~/.zcode/cli/artifacts/<session>/*<tool-result-<uuid>>*`（尾段 glob 可解析）。
// - claude（transcript JSONL，#69 补登 2026-10-18）：图片附件以 base64 内嵌于
//   消息体，无独立文件路径可提取 → 不入文件池（结论落档
//   docs/engineering/specs/2026-10-18-claude-opencode-不入池结论.md）。
// - opencode（opencode.db，#69 补登 2026-10-18）：part 表无 file 类型 → 不入
//   文件池（同上档）。重开条件：任一工具引入带路径的附件形态时重评。
//
// 本模块只产出夹具与形状冒烟断言；抽取逻辑属 B2（files.rs）。

/// kimi 黄金夹具行（tests/fixtures/attachments/kimi-attachments.wire.jsonl）：
/// 三行分别覆盖 用户消息内联 <image path> / 用户输入内联 <file path> / 工具读取 blobref
#[cfg(test)]
pub fn kimi_fixture_lines() -> Vec<String> {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../tests/fixtures/attachments/kimi-attachments.wire.jsonl");
    let text = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("读取夹具失败 {p:?}: {e}"));
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(String::from)
        .collect()
}

#[cfg(test)]
pub const ZCODE_SESSION_ID: &str = "sess_fixt_attach_demo";
#[cfg(test)]
pub const ZCODE_TOOL_RESULT_ID: &str = "tool-result-1a2b3c4d";

/// zcode 黄金 part 行（type=file）：`source.path` 在建库时注入 tempdir 路径——
/// B2 断言「抽取路径与落盘文件一致」用；artifact 行（无 source）覆盖粘贴截图形态
#[cfg(test)]
pub fn zcode_part_file_row(source_path: &str) -> String {
    serde_json::json!({
        "type": "file",
        "mime": "image/png",
        "filename": "cover.png",
        "url": format!("zcode-artifact://{ZCODE_SESSION_ID}/{ZCODE_TOOL_RESULT_ID}"),
        "source": { "type": "file", "path": source_path },
    })
    .to_string()
}

#[cfg(test)]
pub fn zcode_part_artifact_row() -> String {
    serde_json::json!({
        "type": "file",
        "mime": "image/png",
        "filename": "image.png",
        "url": format!("zcode-artifact://{ZCODE_SESSION_ID}/{ZCODE_TOOL_RESULT_ID}"),
    })
    .to_string()
}

/// 在 `<home>/.zcode/cli/db/db.sqlite` 种入黄金 message/part 行，并在
/// `<home>/.zcode/cli/artifacts/<session>/` 落一个 artifact 文件（粘贴截图形态的
/// 磁盘实体）。返回 (source.path, artifact 落盘路径)，均在 tempdir 内。
/// 表结构为真实库的消费面子集（content.rs 只查 id/session_id/time_created/data 与
/// message_id/data/sequence），足够 B2 抽取与既有读取链复用。
#[cfg(test)]
pub fn seed_zcode_attachment_db(
    home: &std::path::Path,
) -> Result<(String, std::path::PathBuf), String> {
    let db_dir = home.join(".zcode/cli/db");
    std::fs::create_dir_all(&db_dir).map_err(|e| format!("建 db 目录失败: {e}"))?;
    let db_path = db_dir.join("db.sqlite");
    let conn = rusqlite::Connection::open(&db_path).map_err(|e| format!("开库失败: {e}"))?;
    conn.execute_batch(
        "CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, data TEXT);
         CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT, sequence INTEGER, data TEXT);",
    )
    .map_err(|e| format!("建表失败: {e}"))?;

    // 磁盘实体：source.path 与 artifact 文件都落在 tempdir（「原始路径可预览」断言面）
    let src_dir = home.join("fixt-src");
    std::fs::create_dir_all(&src_dir).map_err(|e| format!("建源目录失败: {e}"))?;
    let source_path = src_dir.join("cover.png");
    std::fs::write(&source_path, b"\x89PNG\r\n\x1a\n fixture").map_err(|e| e.to_string())?;

    let msg_data = serde_json::json!({
        "role": "user",
        "time": { "created": 1783562500000i64 },
        "agent": "zcode-agent",
    })
    .to_string();

    conn.execute(
        "INSERT INTO message (id, session_id, time_created, data) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params!["msg_fixt_u1", ZCODE_SESSION_ID, 1783562500000i64, msg_data],
    )
    .map_err(|e| e.to_string())?;
    // 真实会话形态：附件必有伴随文本（content.rs user 分支只拼 text parts——
    // 无 text part 的消息不出条目，会话会被判「不存在」）。故种一条 text part。
    conn.execute(
        "INSERT INTO part (id, message_id, session_id, sequence, data) VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![
            "part_fixt_t0",
            "msg_fixt_u1",
            ZCODE_SESSION_ID,
            0i64,
            serde_json::json!({ "type": "text", "text": "看这两张图" }).to_string()
        ],
    )
    .map_err(|e| e.to_string())?;
    conn.execute(
        "INSERT INTO part (id, message_id, session_id, sequence, data) VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![
            "part_fixt_f1",
            "msg_fixt_u1",
            ZCODE_SESSION_ID,
            1i64,
            zcode_part_file_row(&source_path.to_string_lossy())
        ],
    )
    .map_err(|e| e.to_string())?;
    conn.execute(
        "INSERT INTO part (id, message_id, session_id, sequence, data) VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![
            "part_fixt_f2",
            "msg_fixt_u1",
            ZCODE_SESSION_ID,
            2i64,
            zcode_part_artifact_row()
        ],
    )
    .map_err(|e| e.to_string())?;
    drop(conn);

    // artifact 落盘：文件名内嵌 tool-result 尾段（真实库同构——glob 尾段匹配的依据）。
    // 逐段 join（勿用含 `/` 的整串 join）：返回的路径必须与 files.rs resolver 的
    // entry.path()（全平台原生分隔符）逐字符一致，消费方按原始串比较
    let art_dir = home
        .join(".zcode")
        .join("cli")
        .join("artifacts")
        .join(ZCODE_SESSION_ID);
    std::fs::create_dir_all(&art_dir).map_err(|e| format!("建 artifacts 目录失败: {e}"))?;
    let artifact = art_dir.join(format!(
        "prompt-attachment-upload-xx9z-{ZCODE_TOOL_RESULT_ID}.png"
    ));
    std::fs::write(&artifact, b"\x89PNG\r\n\x1a\n artifact").map_err(|e| e.to_string())?;

    Ok((source_path.to_string_lossy().into_owned(), artifact))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// kimi 夹具三行：逐行合法 JSON，且三类标记各在其位
    #[test]
    fn kimi_fixture_has_inline_markup_and_blobref() {
        let lines = kimi_fixture_lines();
        assert_eq!(lines.len(), 3, "夹具应恰三行：用户消息/用户输入/工具读取");
        let blob_lines = lines
            .iter()
            .filter(|l| l.contains("blobref:image/png;"))
            .count();
        assert_eq!(blob_lines, 2, "用户消息贴图 + 工具读取各一个 blobref");
        assert!(lines.iter().any(|l| l.contains("<image path=")));
        assert!(lines.iter().any(|l| l.contains("<file path=")));
        // 逐行合法 JSON（B2 解析器的前置契约）
        for l in &lines {
            let v: serde_json::Value =
                serde_json::from_str(l).unwrap_or_else(|e| panic!("夹具行非 JSON: {e}"));
            assert!(v.get("type").is_some());
        }
        // 合成性：夹具内不含真实用户目录前缀
        let joined = lines.join("\n");
        assert!(!joined.contains(".kimi-code"), "夹具不得引用真实数据目录");
    }

    /// zcode 夹具建库：黄金行落位 + artifact 实体存在 + source.path 与磁盘一致
    #[test]
    fn zcode_fixture_db_seeds_golden_rows_and_artifact_file() {
        let tmp = tempfile::tempdir().unwrap();
        let (source_path, artifact) = seed_zcode_attachment_db(tmp.path()).unwrap();
        assert!(std::path::Path::new(&source_path).is_file());
        assert!(artifact.is_file());
        assert!(artifact.to_string_lossy().contains(ZCODE_TOOL_RESULT_ID));

        let conn = rusqlite::Connection::open(tmp.path().join(".zcode/cli/db/db.sqlite")).unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM part WHERE session_id = ?1",
                [ZCODE_SESSION_ID],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            n, 3,
            "text part 一行 + file part 两行（本地路径型 + artifact 型）"
        );
        let data: String = conn
            .query_row("SELECT data FROM part WHERE id = 'part_fixt_f1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        // JSON 里反斜杠转义（Windows 路径），解析后比对而非子串包含
        let v: serde_json::Value = serde_json::from_str(&data).unwrap();
        assert_eq!(
            v["source"]["path"].as_str().unwrap(),
            source_path,
            "source.path 必须等于磁盘实体"
        );
        assert!(data.contains(ZCODE_TOOL_RESULT_ID));
    }
}
