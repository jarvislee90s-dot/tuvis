//! Codex 有界读取、游标升级与采集接线集成测试（2026-10-26 桌面工作区与输出速度 V2 计划 Task 5）。
//!
//! ## 被测对象
//!
//! [`tps::sources::codex::read_incremental`]——rollout JSONL 的有界增量读取：
//! 24h stat 预过滤、冷启动 8MiB 回看窗、残行不越过、超大行/坏行不解析且使未决失效、
//! 整源单轮预算（≤8MiB 字节 / ≤4096 完整行 / ≤200 观察）、游标 v1 升级（旧占位
//! 游标绝不重释为文件位置）、事实经真实 `store::commit_batch` 同事务落库。
//!
//! ## 规则锚点（Spec §6.3 + Task 5 brief + codex-structure-facts.md）
//!
//! * 旧占位游标（`Null` / 字符串 / 缺 `format_version` 的对象）= 非本源游标 → 全新
//!   起步，绝不把旧值读成 offset/文件位置；
//! * 冷启动：从 `max(0, size−8MiB)` 对齐到下一行开始；读取起点绝不当请求起点——
//!   窗前开始的响应不补窗（解码器保证：无窗内输入边界必不发射）；
//! * 尾行撕裂不消费，补齐后下轮完成（不丢不重）；重启续读与单轮大读等价；
//! * 原生与伴随/复制历史重复 → 跨文件靠 DB UPSERT（同 host_instance）只计一次；
//! * 失败事务：事实、游标、来源状态全部不推进；
//! * 游标 JSON 只存摘要/时刻/计数/头事实，绝无 raw response_id / session id / 正文。
//!
//! ## 隔离纪律（2026-10-07 事故族，与 tps_workbuddy_v2_test.rs 同款）
//!
//! 夹具 = Task 4 的脱敏 rollout 重放（`tests/fixtures/tps-v2/codex-*.jsonl`），复制进
//! tempdir 影子 home 的 `~/.codex/sessions/2026/10/26/`；`now_ms` 显式注入；库用
//! `support::open_ledger_db` 的用例私有连接。绝不触真机 `~/.codex` / `~/.tuvis`，
//! 绝不起 Tauri / Codex / app-server / 模型进程，绝不读 auth.json。

mod support;

use multi_agents_manager_lib::services::tps::model::SourceCursor;
use multi_agents_manager_lib::services::tps::sources::codex;
use multi_agents_manager_lib::services::tps::store;
use rusqlite::Connection;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::{Path, PathBuf};

/// 夹具固定时间锚点（与 Task 4 夹具一致：2026-10-26T00:00:00.000Z）
const ANCHOR_MS: i64 = 1_792_972_800_000;

/// 冷启动回看窗（与实现常量同值：8 MiB）
const COLD_WINDOW: u64 = 8 * 1024 * 1024;

/// 夹具里的合成 response_id（与 Task 4 夹具一致；断言用）
const RID_1: &str = "resp_00000000000000000000000000000000000000004a2a010af8"; // normal 唯一观察
const RID_A: &str = "resp_00000000000000000000000000000000000000000b1fe68e72"; // tools 响应 A
const RID_B: &str = "resp_0000000000000000000000000000000000000000202344b7f4"; // tools 响应 B
const RID_C: &str = "resp_00000000000000000000000000000000000000003526a2e176"; // tools 响应 C

// ==== 影子 home / 夹具 / 私有库 ====

/// Task 4 脱敏夹具全文
fn fixture(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(format!("tests/fixtures/tps-v2/codex-{name}.jsonl"));
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("读取夹具 {name} 失败：{e}"))
}

/// 影子 home + `.codex/sessions/2026/10/26/` 目录（tempdir 泄漏保活至用例结束）
fn shadow_home(tag: &str) -> (PathBuf, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join(tag);
    let sessions = home.join(".codex").join("sessions").join("2026/10/26");
    std::fs::create_dir_all(&sessions).unwrap();
    std::mem::forget(temp); // 用例存活期间影子目录不得被清理（support.rs 同款）
    (home, sessions)
}

/// 把夹具落成 rollout 文件（真实工具根约定：`sessions/<y>/<m>/<d>/rollout-*.jsonl`）
fn install(sessions: &Path, name: &str, text: &str) -> PathBuf {
    let path = sessions.join(format!("rollout-{name}.jsonl"));
    std::fs::write(&path, text).unwrap();
    path
}

/// 「现在」：24h 门只看文件 mtime（夹具复制后 = 测试时刻），事件时刻不设窗
fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn empty_cursor() -> SourceCursor {
    SourceCursor::empty("codex")
}

fn read(
    home: &Path,
    cursor: &SourceCursor,
) -> multi_agents_manager_lib::services::tps::model::ReadBatch {
    codex::read_incremental(home, cursor, now_ms()).expect("read_incremental 不应整源失败")
}

/// 每条用例独占一个私有账本库（每次调用 = 全新空库，并行安全）
fn conn() -> Connection {
    support::open_ledger_db("tps-codex-source")
}

fn count(conn: &Connection, table: &str) -> i64 {
    conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
        .unwrap_or_else(|e| panic!("计数查询失败（{table}，表缺失？）：{e}"))
}

fn sha256_hex(s: &str) -> String {
    let d = Sha256::digest(s.as_bytes());
    d.iter().map(|b| format!("{b:02x}")).collect()
}

/// 游标 JSON 解析（断言形状用）
fn cursor_json(cursor: &SourceCursor) -> Value {
    serde_json::from_str(&cursor.value).expect("next_cursor 必须是本源 v1 JSON")
}

/// rollout 文件的当前字节大小
fn size_of(path: &Path) -> u64 {
    std::fs::metadata(path).unwrap().len()
}

// ==== 合成行（撕裂 / 预算 / 超大行 / 错误行场景用）====

/// epoch ms → RFC3339（含毫秒，与 rollout 顶层 timestamp 同形态）
fn rfc3339(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .unwrap()
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// 一条完整可发射的响应（输入边界 → 模型归属 → 生成项 → 封口），恰产 1 观察：
/// 起点 = user 时刻，终点 = reasoning 时刻（+600ms），输出 = `out`
fn synth_response(i: usize, base_ms: i64, out: i64) -> Vec<Value> {
    let t = format!("turn-{i:03}");
    let rid = synth_rid(i);
    vec![
        json!({"timestamp": rfc3339(base_ms), "type": "response_item",
               "payload": {"type": "message", "role": "user"}}),
        json!({"timestamp": rfc3339(base_ms + 100), "type": "turn_context",
               "payload": {"turn_id": t, "root_turn_id": t, "model": "gpt-test", "effort": "medium"}}),
        json!({"timestamp": rfc3339(base_ms + 600), "type": "response_item",
               "payload": {"type": "reasoning"}}),
        json!({"timestamp": rfc3339(base_ms + 700), "type": "token_usage_record",
               "payload": {"response_id": rid, "session_id": "s-synth", "turn_id": t,
                           "usage": {"output_tokens": out, "reasoning_output_tokens": 1}}}),
    ]
}

/// 合成 response_id（55 字符，与真实同长：`resp_` + 50 位序号）
fn synth_rid(i: usize) -> String {
    format!("resp_{i:050}")
}

/// 合成响应的稳定键（摘要）
fn synth_key(i: usize) -> String {
    sha256_hex(&synth_rid(i))
}

fn rows_to_text(rows: &[Value]) -> String {
    let mut s = String::new();
    for r in rows {
        s.push_str(&r.to_string());
        s.push('\n');
    }
    s
}

/// 追加行到已有 rollout 文件（真实工具的追加方式）
fn append_rows(path: &Path, rows: &[Value]) {
    let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    for r in rows {
        f.write_all(r.to_string().as_bytes()).unwrap();
        f.write_all(b"\n").unwrap();
    }
}

/// 按行数截断：保留前 `keep_lines` 个完整行 + 第 `keep_lines+1` 行的前 `partial` 字节
fn truncated_text(text: &str, keep_lines: usize, partial: usize) -> String {
    let mut out = String::new();
    for line in text.lines().take(keep_lines) {
        out.push_str(line);
        out.push('\n');
    }
    let next = text.lines().nth(keep_lines).unwrap_or("");
    let cut = partial.min(next.len());
    out.push_str(&next[..cut]);
    out
}

// ===========================================================================
// 1. 旧占位游标 → 全新起步，绝不重释为文件位置
// ===========================================================================

/// `Null` / 旧字符串 / 缺 format_version 的对象（哪怕带 EOF offset 的 files）/
/// 空游标，全部 = 非本源游标 → 冷启动取候选发射观察；旧值绝不读成 offset
#[test]
fn old_placeholder_cursor_starts_fresh_and_never_reinterpreted() {
    let (home, sessions) = shadow_home("codex-placeholder");
    let rollout = install(&sessions, "normal", &fixture("normal"));
    let size = size_of(&rollout);
    // 候选相对路径（游标键）
    let rel = "2026/10/26/rollout-normal.jsonl";

    let placeholders: Vec<SourceCursor> = vec![
        empty_cursor(),
        SourceCursor {
            source: "codex".into(),
            value: "null".into(), // 旧占位：serde Null
        },
        SourceCursor {
            source: "codex".into(),
            value: "insufficient-generation-timing".into(), // 旧占位：自由字符串
        },
        // 旧占位对象：带 files 且 offset 顶在 EOF——若被重释为文件位置则零观察
        SourceCursor {
            source: "codex".into(),
            value: json!({
                "files": [{"path": rel, "offset": size, "size": size, "cold_start": false, "decoder": {}}]
            })
            .to_string(),
        },
    ];

    for cursor in &placeholders {
        let batch = read(&home, cursor);
        assert_eq!(
            batch.observations.len(),
            1,
            "旧占位游标必须全新起步取到候选；输入 = {}",
            cursor.value
        );
        assert_eq!(batch.observations[0].stable_call_key, sha256_hex(RID_1));

        // 升级后的游标是 v1 且推进到 EOF（旧占位未被保留）
        let v = cursor_json(&batch.next_cursor);
        assert_eq!(v["format_version"], 1, "升级游标必须写 format_version=1");
        assert_eq!(v["files"][0]["path"], rel);
        assert_eq!(v["files"][0]["offset"], size, "完整消费后 offset = 文件尾");
        assert_eq!(v["files"][0]["cold_start"], false, "首个边界已过");
    }

    // 空根（未装）= 空态：旧占位逐字原样透传（不升级也不解释）
    let temp = tempfile::tempdir().unwrap();
    let empty_home = temp.path().join("no-codex");
    std::fs::create_dir_all(&empty_home).unwrap();
    std::mem::forget(temp);
    let batch = read(&empty_home, &placeholders[1]);
    assert!(batch.observations.is_empty());
    assert!(batch.source_state.ok);
    assert_eq!(
        batch.next_cursor.value, placeholders[1].value,
        "未装 = 游标原样"
    );
}

/// 24h stat 预过滤只拦「从未读过」的新文件；游标在案的文件超窗也照常续读
#[test]
fn stale_new_files_skipped_but_cursored_files_always_resume() {
    let (home, sessions) = shadow_home("codex-age");
    let fresh = install(&sessions, "normal", &fixture("normal"));
    let aged = install(&sessions, "tools", &fixture("tools"));

    // 第一轮两文件都新鲜：都读、都建游标
    let b1 = read(&home, &empty_cursor());
    assert_eq!(b1.observations.len(), 4, "normal 1 条 + tools 3 条");

    // 第二轮前：aged 文件 mtime 拨到 1970（超 24h 窗），并追加一条新响应
    filetime::set_file_mtime(&aged, filetime::FileTime::zero()).unwrap();
    append_rows(&aged, &synth_response(90, ANCHOR_MS + 100_000, 55));
    // 再放一个从未读过的超窗文件：必须被 stat 预过滤拦下（不打开）
    let aged2 = install(&sessions, "aged2", &fixture("normal"));
    filetime::set_file_mtime(&aged2, filetime::FileTime::zero()).unwrap();

    let b2 = read(&home, &b1.next_cursor);
    let keys: Vec<_> = b2
        .observations
        .iter()
        .map(|o| o.stable_call_key.as_str())
        .collect();
    assert!(
        keys.contains(&synth_key(90).as_str()),
        "游标在案的超窗文件必须照常续读（offset/未决才是权威）：{keys:?}"
    );
    assert_eq!(
        keys.len(),
        1,
        "从未读过的超窗文件不得被打开（stat 预过滤）；实际 = {keys:?}"
    );
    assert!(
        std::fs::read(&fresh).is_ok(),
        "新鲜文件路径仍有效（防误删断言）"
    );
}

// ===========================================================================
// 2. 尾行撕裂：残行不越过，补齐后下轮完成（不丢不重）
// ===========================================================================

#[test]
fn torn_tail_not_consumed_then_completed_next_pass() {
    let (home, sessions) = shadow_home("codex-torn");
    let full = fixture("tools");
    let rollout = install(&sessions, "tools", &full);
    // 前 16 个完整行（封口 A@第8行、封口 B@第16行）+ 第 17 行前 40 字节（撕裂）
    let partial_text = truncated_text(&full, 16, 40);
    std::fs::write(&rollout, &partial_text).unwrap();

    let b1 = read(&home, &empty_cursor());
    let mut keys1: Vec<_> = b1
        .observations
        .iter()
        .map(|o| o.stable_call_key.clone())
        .collect();
    keys1.sort();
    let mut want1 = vec![sha256_hex(RID_A), sha256_hex(RID_B)];
    want1.sort();
    assert_eq!(keys1, want1, "撕裂前只应发射 A/B");
    // offset 只记录完整行字节：16 个完整行的行尾（撕裂的 40 字节不计入）
    let complete_len: usize = full.lines().take(16).map(|l| l.len() + 1).sum();
    let v1 = cursor_json(&b1.next_cursor);
    assert_eq!(
        v1["files"][0]["offset"], complete_len as u64,
        "offset 只记录完整行字节，撕裂残行不消费"
    );

    // 补齐其余字节（真实工具的追加写）
    {
        let rest = &full[partial_text.len()..];
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&rollout)
            .unwrap();
        f.write_all(rest.as_bytes()).unwrap();
    }
    let b2 = read(&home, &b1.next_cursor);
    assert_eq!(
        b2.observations.len(),
        1,
        "补齐后下一轮只补 C，不重放 A/B：{:?}",
        b2.observations
            .iter()
            .map(|o| o.stable_call_key.clone())
            .collect::<Vec<_>>()
    );
    assert_eq!(b2.observations[0].stable_call_key, sha256_hex(RID_C));

    // 与单轮全读基线逐键一致（无丢无重）
    let (home3, sessions3) = shadow_home("codex-torn-baseline");
    install(&sessions3, "tools", &full);
    let baseline = read(&home3, &empty_cursor());
    let mut all: Vec<_> = b1
        .observations
        .iter()
        .chain(&b2.observations)
        .map(|o| o.stable_call_key.clone())
        .collect();
    all.sort();
    let mut want: Vec<_> = baseline
        .observations
        .iter()
        .map(|o| o.stable_call_key.clone())
        .collect();
    want.sort();
    assert_eq!(all, want, "两轮合计必须等价单轮全读");
}

// ===========================================================================
// 3. 重启续读：发射上限停批 → 新游标精确续跑（无丢无重）
// （封口行发射上界 = 2：余量 < 2 时停批，封口密集文件的有效停批 = 199 ≤ 200）
// ===========================================================================

#[test]
fn restart_resume_matches_single_pass() {
    let (home, sessions) = shadow_home("codex-resume");
    let mut rows = vec![
        json!({"timestamp": rfc3339(ANCHOR_MS), "type": "session_meta",
               "payload": {"cli_version": "0.164.0", "model_provider": "openai", "session_id": "s-resume"}}),
    ];
    for i in 0..250 {
        rows.extend(synth_response(i, ANCHOR_MS + 1_000 * i as i64, 100));
    }
    install(&sessions, "resume", &rows_to_text(&rows));

    // 第一轮：发射余量剩 1 时在下一个封口行行界之前停批（该行上界 = 2）
    let b1 = read(&home, &empty_cursor());
    assert!(b1.observations.len() <= 200);
    assert_eq!(b1.observations.len(), 199, "封口密集文件的有效停批 = 199");
    assert_eq!(b1.observations[0].stable_call_key, synth_key(0));
    assert_eq!(
        b1.observations[198].stable_call_key,
        synth_key(198),
        "第一轮按文件序吃到第 199 条"
    );
    let mut c = conn();
    store::commit_batch(&mut c, &b1).unwrap();

    // 第二轮（全新 read_incremental，只带返回的游标 = 模拟重启）：精确续跑
    let b2 = read(&home, &b1.next_cursor);
    assert_eq!(b2.observations.len(), 51);
    assert_eq!(
        b2.observations[0].stable_call_key,
        synth_key(199),
        "续读必须从停批的下一响应开始（不重不漏）"
    );
    store::commit_batch(&mut c, &b2).unwrap();

    // 第三轮：EOF，零重放
    let b3 = read(&home, &b2.next_cursor);
    assert!(b3.observations.is_empty(), "EOF 后重读为空");

    // 合计 = 250 个不同键，无丢无重
    let total = count(&c, "tps_observation");
    assert_eq!(total, 250, "两轮合计恰 250 条");
    let distinct: i64 = c
        .query_row(
            "SELECT COUNT(DISTINCT stable_call_key) FROM tps_observation",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(distinct, 250);
}

// ===========================================================================
// 4. 跨文件复制历史：同一 response_id 两文件各发一次 → DB UPSERT 只一行
// ===========================================================================

#[test]
fn native_and_companion_dedup_via_db_upsert() {
    let (home, sessions) = shadow_home("codex-dedup");
    let mut c = conn();

    // 第一轮：只有 copy1
    let f1 = install(&sessions, "copy1", &fixture("normal"));
    let b1 = read(&home, &empty_cursor());
    assert_eq!(b1.observations.len(), 1);
    store::commit_batch(&mut c, &b1).unwrap();
    assert_eq!(count(&c, "tps_observation"), 1);

    // 第二轮：复制历史出现 copy2（同 response_id）
    let f2 = install(&sessions, "copy2", &fixture("normal"));
    assert_ne!(f1, f2);
    let b2 = read(&home, &b1.next_cursor);
    assert_eq!(
        b2.observations.len(),
        1,
        "本文件解码器首见即发（跨文件去重是 DB 的职责）"
    );
    assert_eq!(b2.observations[0].stable_call_key, sha256_hex(RID_1));
    assert_eq!(
        b2.observations[0].host_instance,
        b1.observations[0].host_instance
    );
    store::commit_batch(&mut c, &b2).unwrap();

    // 同 host_instance + 同稳定键 → UPSERT 覆盖，仍一行
    assert_eq!(count(&c, "tps_observation"), 1, "复制历史不得翻倍");
    let (n, key): (i64, String) = c
        .query_row(
            "SELECT COUNT(*), MIN(stable_call_key) FROM tps_observation",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(n, 1);
    assert_eq!(key, sha256_hex(RID_1));

    // 第三轮：零重放
    let b3 = read(&home, &b2.next_cursor);
    assert!(b3.observations.is_empty());
    store::commit_batch(&mut c, &b3).unwrap();
    assert_eq!(count(&c, "tps_observation"), 1);
}

// ===========================================================================
// 5. 失败事务：事实、游标、来源状态全部不推进
// ===========================================================================

#[test]
fn failed_commit_keeps_cursor_and_facts() {
    let (home, sessions) = shadow_home("codex-fail");
    install(&sessions, "tools", &fixture("tools"));
    let mut c = conn();

    let original_cursor = empty_cursor();
    let mut batch = read(&home, &original_cursor);
    assert_eq!(batch.observations.len(), 3);

    // 在批中段注入确定性失败（epoch 非负 CHECK，tps_store_test 同缝）
    batch.observations[1].ended_at_ms = -1;
    assert!(
        store::commit_batch(&mut c, &batch).is_err(),
        "批内失败必须冒泡"
    );

    assert_eq!(count(&c, "tps_observation"), 0, "失败批不得残留事实");
    let cursor_after_failed_commit = store::load_cursor(&c, "codex")
        .unwrap()
        .unwrap_or_else(|| original_cursor.clone());
    assert_eq!(cursor_after_failed_commit, original_cursor);
    assert!(
        store::load_source_state(&c, "codex").unwrap().is_none(),
        "失败批的来源状态也不得落库"
    );

    // 修复后同批可提交（数据本身没坏）
    batch.observations[1].ended_at_ms = ANCHOR_MS + 8_600;
    store::commit_batch(&mut c, &batch).unwrap();
    assert_eq!(count(&c, "tps_observation"), 3);
}

// ===========================================================================
// 6. 整源单轮预算：字节 ≤ 8MiB、完整行 ≤ 4096、观察 ≤ 200（源级合计）
// ===========================================================================

/// 行预算：4096 个完整行处停批，检查点 = 第 4096 行行尾；下轮续完
#[test]
fn line_budget_stops_at_4096_complete_lines() {
    let (home, sessions) = shadow_home("codex-lines");
    // 5000 个无害小行（合法 JSON、带 timestamp，被解析计入「已处理行」但不发射）
    let pad = json!({"timestamp": rfc3339(ANCHOR_MS), "type": "event_msg", "payload": {"type": "world_state"}});
    let mut lines: Vec<String> = Vec::new();
    // 第一行放 session_meta 以便头事实可断言（其余为 world_state 无害行）
    let meta = json!({"timestamp": rfc3339(ANCHOR_MS), "type": "session_meta",
                  "payload": {"cli_version": "0.164.0", "model_provider": "openai", "session_id": "s-lines"}});
    lines.push(meta.to_string());
    for _ in 0..4999 {
        lines.push(pad.to_string());
    }
    let text = lines.join("\n") + "\n";
    install(&sessions, "lines", &text);

    // 前 4096 行的字节量（实现检查点必须恰在此）
    let prefix_4096: usize = lines.iter().take(4096).map(|l| l.len() + 1).sum();

    let b1 = read(&home, &empty_cursor());
    assert!(b1.observations.is_empty(), "预算内没碰到任何响应");
    let v1 = cursor_json(&b1.next_cursor);
    assert_eq!(
        v1["files"][0]["offset"], prefix_4096 as u64,
        "停批检查点 = 第 4096 个完整行的行尾"
    );
    assert!(
        b1.diagnostics.iter().any(|d| d.contains("行预算")),
        "预算用尽必须留诊断：{:?}",
        b1.diagnostics
    );

    // 下轮续完（904 行）
    let b2 = read(&home, &b1.next_cursor);
    let v2 = cursor_json(&b2.next_cursor);
    assert_eq!(
        v2["files"][0]["offset"],
        text.len() as u64,
        "第二轮消费到 EOF"
    );
}

/// 字节预算：>8MiB 的冷启动文件一轮至多读 8MiB（源级），下轮续读补完
#[test]
fn byte_budget_binds_at_8mib_per_pass() {
    let (home, sessions) = shadow_home("codex-bytes");
    let mut lines: Vec<String> = Vec::new();
    // ~2500 × 4KB ≈ 10.2 MiB 的无害行（合法 JSON，被解析计入行预算但不发射）
    let pad_str = "a".repeat(3_900);
    for i in 0..2_500 {
        lines.push(
            json!({"timestamp": rfc3339(ANCHOR_MS + i), "type": "event_msg",
                   "payload": {"type": "world_state", "pad": pad_str}})
            .to_string(),
        );
    }
    // 文件尾放一条真实响应（无论落在第一轮还是第二轮都必须发出、且恰一次）
    let mut rows: Vec<Value> = Vec::new();
    for l in &lines {
        rows.push(serde_json::from_str(l).unwrap());
    }
    rows.extend(synth_response(7, ANCHOR_MS + 100_000, 42));
    let text = rows_to_text(&rows);
    install(&sessions, "big", &text);
    let size = size_of(&sessions.join("rollout-big.jsonl"));
    assert!(size > COLD_WINDOW, "前提：文件大于冷启动窗口");

    // 冷启动从 max(0, size−8MiB) 开始（测试按文档化规则复算 seek 点）
    let seek = size - COLD_WINDOW;
    let b1 = read(&home, &empty_cursor());
    let v1 = cursor_json(&b1.next_cursor);
    let consumed: u64 = v1["files"][0]["offset"].as_u64().unwrap() - seek;
    let read_bytes_in_pass = consumed;
    assert!(read_bytes_in_pass <= 8 * 1024 * 1024);
    let mut c = conn();
    store::commit_batch(&mut c, &b1).unwrap();

    // 续读至 EOF；响应恰发一次
    let b2 = read(&home, &b1.next_cursor);
    let all: Vec<_> = b1
        .observations
        .iter()
        .chain(&b2.observations)
        .map(|o| o.stable_call_key.clone())
        .collect();
    assert_eq!(all, vec![synth_key(7)], "两轮合计恰一次：{all:?}");
    store::commit_batch(&mut c, &b2).unwrap();
    assert_eq!(count(&c, "tps_observation"), 1);
}

// ===========================================================================
// 7. 超大行 / 坏行：越过不解析正文、未决失效、下个可靠输入边界再计速
// ===========================================================================

#[test]
fn oversized_and_malformed_lines_skip_without_parsing_and_invalidate_pending() {
    let (home, sessions) = shadow_home("codex-oversize");
    let mut rows = vec![
        json!({"timestamp": rfc3339(ANCHOR_MS), "type": "session_meta",
               "payload": {"cli_version": "0.164.0", "model_provider": "openai", "session_id": "s-big"}}),
    ];
    // 响应 1：完整，先发一条
    rows.extend(synth_response(1, ANCHOR_MS + 1_000, 100));
    // 响应 2 开头：输入边界 + 生成项（未决在途）
    rows.push(
        json!({"timestamp": rfc3339(ANCHOR_MS + 10_000), "type": "response_item",
                     "payload": {"type": "message", "role": "user"}}),
    );
    rows.push(
        json!({"timestamp": rfc3339(ANCHOR_MS + 10_100), "type": "turn_context",
                     "payload": {"turn_id": "t2", "model": "gpt-test"}}),
    );
    rows.push(
        json!({"timestamp": rfc3339(ANCHOR_MS + 10_200), "type": "response_item",
                     "payload": {"type": "reasoning"}}),
    );
    // 超大行（>1MiB）：正文是一条本可发射的封口（若被解析必发 rid_big）——
    // 实现必须整行越过不解析，绝不让它发射或残留未决
    let big_pad = "x".repeat(1_100_000);
    rows.push(
        json!({"timestamp": rfc3339(ANCHOR_MS + 10_300), "type": "token_usage_record",
                     "payload": {"response_id": "resp_big", "session_id": "s-big", "turn_id": "t2",
                                 "note": big_pad, "usage": {"output_tokens": 999}}}),
    );
    // 坏行：JSON 解析失败（同样使未决失效）
    let mut text = rows_to_text(&rows);
    text.push_str("{\"timestamp\": broken\n");
    // 响应 3：下个可靠输入边界后照常计速
    let tail = synth_response(3, ANCHOR_MS + 20_000, 77);
    text.push_str(&rows_to_text(&tail));

    install(&sessions, "oversize", &text);
    let batch = read(&home, &empty_cursor());
    let keys: Vec<_> = batch
        .observations
        .iter()
        .map(|o| o.stable_call_key.as_str())
        .collect();
    assert_eq!(
        keys,
        vec![synth_key(1), synth_key(3)],
        "超大行/坏行绝不发射；下个可靠边界后恢复计速：{keys:?}"
    );
    let joined = batch.diagnostics.join("\n");
    assert!(
        joined.contains("oversized line skipped"),
        "超大行必须有诊断：{joined}"
    );
    assert!(joined.contains("malformed"), "坏行必须有诊断：{joined}");
    assert!(
        !batch
            .source_state
            .detail
            .as_deref()
            .unwrap_or_default()
            .contains("resp_big"),
        "detail 是诊断面，不得携带行内身份：{:?}",
        batch.source_state.detail
    );
}

/// Task 4 防御分支补测（inline 喂行，经完整读取器路径）：error / stream_error /
/// turn_failed 到达时丢弃未封口生成项（诊断留痕），后续生成项从原输入边界重开，
/// 封口照常结算——绝不以回合时长补窗、绝不重复发射
#[test]
fn error_rows_drop_inflight_and_resume_at_next_boundary() {
    let (home, sessions) = shadow_home("codex-errorrows");
    let mut rows = vec![
        json!({"timestamp": rfc3339(ANCHOR_MS), "type": "session_meta",
               "payload": {"cli_version": "0.164.0", "model_provider": "openai", "session_id": "s-err"}}),
    ];
    let markers = ["error", "stream_error", "turn_failed"];
    for (i, marker) in markers.iter().enumerate() {
        let base = ANCHOR_MS + 1_000 * (i as i64 + 1);
        let t = format!("turn-err-{i}");
        rows.push(json!({"timestamp": rfc3339(base), "type": "response_item",
                         "payload": {"type": "message", "role": "user"}}));
        rows.push(
            json!({"timestamp": rfc3339(base + 100), "type": "turn_context",
                         "payload": {"turn_id": t, "model": "gpt-test"}}),
        );
        // 错误前已有生成项（未封口）→ 错误行必须把它丢弃（诊断）
        rows.push(
            json!({"timestamp": rfc3339(base + 200), "type": "response_item",
                         "payload": {"type": "reasoning"}}),
        );
        rows.push(
            json!({"timestamp": rfc3339(base + 300), "type": "event_msg",
                         "payload": {"type": marker, "turn_id": t}}),
        );
        // 错误后的生成项从原输入边界重开 → 封口结算（窗 = user → assistant）
        rows.push(
            json!({"timestamp": rfc3339(base + 700), "type": "response_item",
                         "payload": {"type": "message", "role": "assistant"}}),
        );
        rows.push(json!({"timestamp": rfc3339(base + 800), "type": "token_usage_record",
                         "payload": {"response_id": format!("resp_err{i:047}"), "session_id": "s-err",
                                     "turn_id": t, "usage": {"output_tokens": 10}}}));
    }
    install(&sessions, "errorrows", &rows_to_text(&rows));

    let batch = read(&home, &empty_cursor());
    assert_eq!(
        batch.observations.len(),
        3,
        "每个响应恰一条；错误行不补窗也不阻断：{:?}",
        batch
            .observations
            .iter()
            .map(|o| o.stable_call_key.clone())
            .collect::<Vec<_>>()
    );
    // 三种错误标记走同一条防御丢弃路径（Task 4 语义：统一按 error 行丢弃未封口
    // 生成项），聚合诊断 3x
    let joined = batch.diagnostics.join("\n");
    assert!(
        joined.contains("error open-response-dropped"),
        "错误行必须有丢弃诊断：{joined}"
    );
    assert!(joined.contains("3x"), "三种错误标记各丢一次：{joined}");
}

// ===========================================================================
// 8. 游标 JSON 卫生：无 raw id / 无正文；头事实在场
// ===========================================================================

#[test]
fn cursor_json_contains_no_raw_ids_or_payload_text() {
    let (home, sessions) = shadow_home("codex-hygiene");
    install(&sessions, "tools", &fixture("tools"));
    let batch = read(&home, &empty_cursor());
    let cur = &batch.next_cursor.value;

    for secret in [
        RID_A,
        RID_B,
        RID_C,
        "00000000-1111-2222-3333-444444444444", // 原始 session id
        "turn-tools-1",                         // 原始 turn id
        "cA",
        "cB1",
        "cN1", // 原始 call_id
        "shell",
        "spawn_agent", // 工具名/载荷词
    ] {
        assert!(!cur.contains(secret), "游标不得含 {secret}：{cur}");
    }

    // 摘要与头事实在场（Spec §6.2.6 记录宿主版本）
    let v = cursor_json(&batch.next_cursor);
    assert_eq!(v["format_version"], 1);
    let f0 = &v["files"][0];
    assert_eq!(
        f0["cli_version"], "0.164.0",
        "session_meta.cli_version 必须入游标"
    );
    assert_eq!(
        f0["model_provider"], "openai",
        "session_meta.model_provider 必须入游标"
    );
    assert_eq!(f0["cold_start"], false, "首个输入/回合边界已过");
    // 摘要字段形态（64 hex）
    let raw = f0["decoder"].to_string();
    assert!(
        raw.len() > 2 && f0["decoder"]["current_model"] == "gpt-6.1-sol",
        "每文件解码器状态必须随游标持久化（tools 尾响应模型）"
    );
}

// ===========================================================================
// 9. 冷启动：窗前开始的响应不补窗（读取起点绝不当请求起点）
// ===========================================================================

#[test]
fn cold_start_skips_response_spanning_window_start() {
    let (home, sessions) = shadow_home("codex-cold");
    let mut rows = vec![
        json!({"timestamp": rfc3339(ANCHOR_MS), "type": "session_meta",
               "payload": {"cli_version": "0.164.0", "model_provider": "openai", "session_id": "s-cold"}}),
        // 响应 A 的输入边界与模型归属在回看窗**之前**
        json!({"timestamp": rfc3339(ANCHOR_MS + 1_000), "type": "response_item",
               "payload": {"type": "message", "role": "user"}}),
        json!({"timestamp": rfc3339(ANCHOR_MS + 1_100), "type": "turn_context",
               "payload": {"turn_id": "ta", "model": "gpt-outside"}}),
        json!({"timestamp": rfc3339(ANCHOR_MS + 1_200), "type": "response_item",
               "payload": {"type": "reasoning"}}),
    ];
    // ~8.8 MiB 的无害行把响应 A 的封口推到窗内（窗起点落在垫行中部）
    let pad_str = "a".repeat(3_900);
    for i in 0..2_200 {
        rows.push(
            json!({"timestamp": rfc3339(ANCHOR_MS + 2_000 + i), "type": "event_msg",
                         "payload": {"type": "world_state", "pad": pad_str}}),
        );
    }
    // 响应 A 的封口（窗内首见，但其边界/模型在窗外 → 必须跳过不补窗）
    rows.push(json!({"timestamp": rfc3339(ANCHOR_MS + 50_000), "type": "token_usage_record",
                     "payload": {"response_id": "resp_outside", "session_id": "s-cold", "turn_id": "ta",
                                 "usage": {"output_tokens": 500}}}));
    // 响应 B 完整落在窗内 → 正常发射
    rows.extend(synth_response(9, ANCHOR_MS + 60_000, 88));

    install(&sessions, "cold", &rows_to_text(&rows));
    let size = size_of(&sessions.join("rollout-cold.jsonl"));
    assert!(size > COLD_WINDOW);

    let b1 = read(&home, &empty_cursor());
    let mut all: Vec<_> = b1
        .observations
        .iter()
        .map(|o| o.stable_call_key.clone())
        .collect();
    all.sort();
    assert_eq!(
        all,
        vec![synth_key(9)],
        "窗前开始的响应 A 不补窗（事实丢失而非伪造），只发窗内的 B：{all:?}"
    );
    assert!(
        b1.diagnostics
            .iter()
            .any(|d| d.contains("missing-model skipped")),
        "响应 A 的封口必须按缺模型上下文诊断跳过：{:?}",
        b1.diagnostics
    );
    let v = cursor_json(&b1.next_cursor);
    assert_eq!(v["files"][0]["cold_start"], false, "窗内首个输入边界已过");
    assert_eq!(v["files"][0]["offset"], size, "一轮读尽到 EOF");
}

// ===========================================================================
// 10. 来源状态面：ok 语义与流式缺口语言保持真实
// ===========================================================================

#[test]
fn source_state_reports_response_only_capability() {
    let (home, sessions) = shadow_home("codex-state");
    install(&sessions, "normal", &fixture("normal"));
    let batch = read(&home, &empty_cursor());
    assert!(batch.source_state.ok);
    let detail = batch.source_state.detail.clone().unwrap_or_default();
    assert!(
        detail.contains("insufficient-generation-timing"),
        "流式缺口语言必须保留（stream 仍不可用）：{detail}"
    );
    assert!(
        detail.contains("response-only") || detail.contains("响应"),
        "响应口径的接入事实必须说明：{detail}"
    );
    // 观察本身：仅响应、日志近似（与 Task 4 解码器契约一致）
    for o in &batch.observations {
        assert!(o.response_elapsed_ms.is_some());
        assert_eq!(o.generation_ms, None);
        assert_eq!(o.first_token_at_ms, None);
    }
}

// ===========================================================================
// 11. 游标体积有界（裁决 R2）：安静超窗读尽文件剪枝 + 每文件摘要集 128 上限
// ===========================================================================

/// 剪枝规则：mtime 超 24h 窗 + 已读尽 + 无未决封口 → 游标条目剪除；仍挂未决封口的
/// 安静文件不剪（跨轮未决状态绝不随剪枝丢失）。被剪文件日后追加（mtime 回窗）→
/// 按新文件冷启动重进：重叠事实按新解码器重发一次，DB UPSERT 幂等收敛、不翻倍
#[test]
fn aged_fully_consumed_files_are_pruned_from_cursor() {
    let (home, sessions) = shadow_home("codex-prune");
    // X：10 条完整响应（读尽 → 可剪）；Z：尾部是「封口先到、生成项未补齐」的延迟
    // 未决形态（读尽但有 pending_seal → 不剪）
    let mut x_rows: Vec<Value> = Vec::new();
    for i in 0..10 {
        x_rows.extend(synth_response(i, ANCHOR_MS + 1_000 * i as i64, 100));
    }
    let x = install(&sessions, "prune-x", &rows_to_text(&x_rows));
    let z_rows = vec![
        json!({"timestamp": rfc3339(ANCHOR_MS + 90_000), "type": "session_meta",
               "payload": {"cli_version": "0.164.0", "model_provider": "openai", "session_id": "s-z"}}),
        json!({"timestamp": rfc3339(ANCHOR_MS + 90_100), "type": "response_item",
               "payload": {"type": "message", "role": "user"}}),
        json!({"timestamp": rfc3339(ANCHOR_MS + 90_200), "type": "turn_context",
               "payload": {"turn_id": "tz", "model": "gpt-test"}}),
        json!({"timestamp": rfc3339(ANCHOR_MS + 90_300), "type": "token_usage_record",
               "payload": {"response_id": "resp_z", "session_id": "s-z", "turn_id": "tz",
                           "usage": {"output_tokens": 5}}}),
    ];
    let _z = install(&sessions, "prune-z", &rows_to_text(&z_rows));

    // 第一轮（两文件新鲜）：X 读尽发 10；Z 读尽零发射（封口未决挂起）
    let b1 = read(&home, &empty_cursor());
    assert_eq!(b1.observations.len(), 10);
    assert_eq!(
        cursor_json(&b1.next_cursor)["files"]
            .as_array()
            .unwrap()
            .len(),
        2,
        "两个文件都入册"
    );

    // 双双拨老 mtime（> 24h 窗）后第二轮：都无新增字节 → X 满足三条件被剪；
    // Z 有未决封口保留
    filetime::set_file_mtime(&x, filetime::FileTime::zero()).unwrap();
    filetime::set_file_mtime(&_z, filetime::FileTime::zero()).unwrap();
    let b2 = read(&home, &b1.next_cursor);
    assert!(b2.observations.is_empty());
    let binding = cursor_json(&b2.next_cursor);
    let files2 = binding["files"].as_array().unwrap();
    assert_eq!(
        files2.len(),
        1,
        "只有未决封口文件保留：{:?}",
        files2.iter().map(|f| f["path"].clone()).collect::<Vec<_>>()
    );
    assert!(
        files2[0]["path"].as_str().unwrap().contains("prune-z"),
        "保留的必须是未决文件：{:?}",
        files2[0]["path"]
    );
    assert!(
        !files2[0]["decoder"]["pending_seal"].is_null(),
        "未决封口随游标持久（跨轮续算）"
    );

    // 被剪的 X 追加一条（写回 mtime = 回窗）→ 冷启动重进：新解码器重发重叠 10 条
    //（UPSERT 幂等兜底）+ 新 1 条
    append_rows(&x, &synth_response(999, ANCHOR_MS + 500_000, 66));
    let b3 = read(&home, &b2.next_cursor);
    assert_eq!(
        b3.observations.len(),
        11,
        "冷启动重进重发全文件窗（重叠 + 新增），不丢不假同步：{:?}",
        b3.observations
            .iter()
            .map(|o| o.stable_call_key.clone())
            .collect::<Vec<_>>()
    );
    assert!(b3
        .observations
        .iter()
        .any(|o| o.stable_call_key == synth_key(999)));
    assert_eq!(
        cursor_json(&b3.next_cursor)["files"]
            .as_array()
            .unwrap()
            .len(),
        2,
        "X 重新入册"
    );

    // 私有库收敛：10（首轮）+ 11（重进批，10 键 UPSERT 覆盖 + 1 新）= 11 行不翻倍
    let mut c = conn();
    store::commit_batch(&mut c, &b1).unwrap();
    store::commit_batch(&mut c, &b3).unwrap();
    assert_eq!(count(&c, "tps_observation"), 11);
}

/// 每文件已发射摘要集上限 128（裁决 R2a）：单文件 130 个互异封口全发（本文件内均
/// 首见），游标内摘要集/驱逐序被压在 128；跨文件去重仍归 DB UPSERT
#[test]
fn per_file_digest_set_is_capped_at_128() {
    let (home, sessions) = shadow_home("codex-digestcap");
    let mut rows: Vec<Value> = Vec::new();
    for i in 0..130 {
        rows.extend(synth_response(i, ANCHOR_MS + 1_000 * i as i64, 100));
    }
    install(&sessions, "digestcap", &rows_to_text(&rows));

    let batch = read(&home, &empty_cursor());
    assert_eq!(
        batch.observations.len(),
        130,
        "130 个互异封口在单文件内全部首见即发（重放上限只影响跨轮去重距离）"
    );
    let dec = &cursor_json(&batch.next_cursor)["files"][0]["decoder"];
    let seen = dec["seen_response_digests"]
        .as_array()
        .expect("摘要集序列化");
    let order = dec["digest_order"].as_array().expect("驱逐序序列化");
    assert!(
        seen.len() <= 128 && order.len() <= 128,
        "摘要集与驱逐序必须 ≤ 128：seen={} order={}",
        seen.len(),
        order.len()
    );
    assert_eq!(seen.len(), order.len(), "集合与驱逐序同步");
}

// ===========================================================================
// 12. 预算边界的无损检查点（评审 Important 1/2）
// ===========================================================================

/// 封口对恰好压在发射上限上：延迟未决封口（rid 9001）+ 后续封口（rid 9002）跨轮
/// 结算——余量不足封口行发射上界时停在该行行界**之前**（不消费零状态变更），下一
/// 轮足额结算。二轮复审 B2 后的窗口语义：9001 按窗结算 [t, t+700]；9002 的未决快照
/// 捕获的是 9001 已消费的旧边界 t（封口→生成→封口序列中间无输入/工具返回行），候选
/// 窗 [t, t+900] 与 9001 完全重叠 → `stale-start-after-commit` 单调护栏保守跳过
/// （身份不消费）。两条观察 → 一条 + 一诊断，无丢失、无重复、无重叠窗
#[test]
fn seal_pair_at_emit_cap_resumes_without_loss() {
    let (home, sessions) = shadow_home("codex-capseal");
    let mut lines: Vec<String> = Vec::new();
    // 199 条完整响应：首轮后发射余量恰剩 1（封口行上界 = 2 → 触发停批闸）
    for i in 0..199usize {
        for r in synth_response(i, ANCHOR_MS + 1_000 * i as i64, 100) {
            lines.push(r.to_string());
        }
    }
    // 封口对：边界 → 模型 → 封口 rid 9001（延迟未决）→ 生成项 → 封口 rid 9002
    //（本行结算 9001 并保存 9002 为新未决）
    let t = ANCHOR_MS + 300_000;
    let pair_rows = vec![
        json!({"timestamp": rfc3339(t), "type": "response_item",
               "payload": {"type": "message", "role": "user"}}),
        json!({"timestamp": rfc3339(t + 100), "type": "turn_context",
               "payload": {"turn_id": "t-pair", "model": "gpt-test"}}),
        json!({"timestamp": rfc3339(t + 200), "type": "token_usage_record",
               "payload": {"response_id": synth_rid(9001), "session_id": "s-cap",
                           "turn_id": "t-pair", "usage": {"output_tokens": 9}}}),
        json!({"timestamp": rfc3339(t + 700), "type": "response_item",
               "payload": {"type": "reasoning"}}),
        json!({"timestamp": rfc3339(t + 800), "type": "token_usage_record",
               "payload": {"response_id": synth_rid(9002), "session_id": "s-cap",
                           "turn_id": "t-pair", "usage": {"output_tokens": 8}}}),
        json!({"timestamp": rfc3339(t + 900), "type": "response_item",
               "payload": {"type": "reasoning"}}),
        json!({"timestamp": rfc3339(t + 1000), "type": "token_usage_record",
               "payload": {"response_id": synth_rid(9003), "session_id": "s-cap",
                           "turn_id": "t-pair", "usage": {"output_tokens": 7}}}),
    ];
    for r in &pair_rows {
        lines.push(r.to_string());
    }
    install(&sessions, "capseal", &(lines.join("\n") + "\n"));
    // 停批检查点 = 199×4 热身行 + 边界行 + turn_context 行的行尾（封口行未消费）
    let checkpoint: usize = lines[..199 * 4 + 2].iter().map(|l| l.len() + 1).sum();

    // 第一轮：199 条后停在封口对的第一封口行行界之前
    let b1 = read(&home, &empty_cursor());
    assert_eq!(b1.observations.len(), 199, "热身恰 199 条");
    assert_eq!(
        cursor_json(&b1.next_cursor)["files"][0]["offset"],
        checkpoint as u64,
        "封口行必须整行留在检查点之后（不消费、零状态变更）"
    );

    // 第二轮（全新 read_incremental + 返回游标）：足额结算封口对——9001 入窗；
    // 9002 的陈旧起点被单调护栏保守跳过（诊断可核）
    let b2 = read(&home, &b1.next_cursor);
    let keys2: Vec<_> = b2
        .observations
        .iter()
        .map(|o| o.stable_call_key.as_str())
        .collect();
    assert_eq!(
        keys2,
        vec![synth_key(9001).as_str()],
        "延迟未决封口按窗结算；9002 不得用陈旧起点快照发射重叠窗：{keys2:?}"
    );
    let r9001 = &b2.observations[0];
    assert_eq!(
        (r9001.started_at_ms, r9001.ended_at_ms),
        (t, t + 700),
        "9001 = 边界(t) → 生成项(t+700)"
    );
    assert!(
        b2.diagnostics
            .iter()
            .any(|d| d.contains("stale-start-after-commit")),
        "9002 应以 stale-start-after-commit 保守跳过：{:?}",
        b2.diagnostics
    );

    // 第三轮：9003 仍为未决（无后续生成项/边界，文件到 EOF），零重放
    let b3 = read(&home, &b2.next_cursor);
    assert!(b3.observations.is_empty(), "EOF 后无重放");

    // 私有库：199 + 1 = 200 行，键全互异（无丢无重）
    let mut c = conn();
    store::commit_batch(&mut c, &b1).unwrap();
    store::commit_batch(&mut c, &b2).unwrap();
    assert_eq!(count(&c, "tps_observation"), 200);
    let distinct: i64 = c
        .query_row(
            "SELECT COUNT(DISTINCT stable_call_key) FROM tps_observation",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(distinct, 200);
}

/// 共享字节预算只剩 ~30KB 时遇到 60KB 的合法行：本轮原位返回（不消费、无 oversized
/// 诊断），下一轮足额预算整行完整消费——绝不拦腰越过（评审 Important 2）
#[test]
fn small_remaining_budget_does_not_advance_past_valid_line() {
    let (home, sessions) = shadow_home("codex-budgetedge");
    // 目标文件：60KB 合法行（< 1MiB 阈值）+ 一条完整响应
    let big_line = json!({"timestamp": rfc3339(ANCHOR_MS), "type": "event_msg",
                          "payload": {"type": "world_state", "pad": "a".repeat(59_900)}});
    let mut rows: Vec<Value> = vec![big_line];
    rows.extend(synth_response(7, ANCHOR_MS + 100_000, 42));
    let target = install(&sessions, "budget-target", &rows_to_text(&rows));

    // 吃预算文件：~4KB 合法行铺到 8MiB − ~30KB（余量落在 30–35KB，小于目标文件首行）
    let pad_line = json!({"timestamp": rfc3339(ANCHOR_MS), "type": "event_msg",
                          "payload": {"type": "world_state", "pad": "b".repeat(3_900)}})
    .to_string();
    let per = pad_line.len() + 1;
    let n = (8 * 1024 * 1024 - 30_000) / per;
    let eater_text = (0..n)
        .map(|_| pad_line.clone())
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    let eater = install(&sessions, "budget-eater", &eater_text); // 后装 → mtime 更新 → 先吃预算
    assert!(
        std::fs::metadata(&eater).unwrap().len() < 8 * 1024 * 1024,
        "吃预算文件必须小于单轮字节预算（一轮读尽）"
    );

    // 第一轮：eater 读尽吃掉预算；target 只读到 30KB 残块（60KB 行内，无换行）
    let b1 = read(&home, &empty_cursor());
    assert!(
        b1.observations.is_empty(),
        "预算边界轮不得发射任何观察：{:?}",
        b1.observations
            .iter()
            .map(|o| o.stable_call_key.clone())
            .collect::<Vec<_>>()
    );
    let d1 = b1.diagnostics.join("\n");
    assert!(
        !d1.contains("oversized") && !d1.contains("advancing past"),
        "余量不足的合法行绝不得被判超大越过：{d1}"
    );
    assert!(!d1.contains("malformed"), "残块不得被当坏行：{d1}");
    let v1 = cursor_json(&b1.next_cursor);
    assert_eq!(
        v1["files"][1]["offset"], 0,
        "target 水位原位不动（整行留给下轮）"
    );

    // 第二轮（足额预算）：60KB 行整行完整消费 + 响应照发，无任何越过诊断
    let b2 = read(&home, &b1.next_cursor);
    assert_eq!(b2.observations.len(), 1, "合法行完整消费后响应照发");
    assert_eq!(b2.observations[0].stable_call_key, synth_key(7));
    let d2 = b2.diagnostics.join("\n");
    assert!(
        !d2.contains("oversized") && !d2.contains("advancing past"),
        "补读轮也不得有越过诊断：{d2}"
    );
    assert_eq!(
        cursor_json(&b2.next_cursor)["files"][1]["offset"],
        size_of(&target),
        "target 读尽到 EOF"
    );

    // 私有库收敛：恰 1 行
    let mut c = conn();
    store::commit_batch(&mut c, &b2).unwrap();
    assert_eq!(count(&c, "tps_observation"), 1);
}

/// 门控停批落在块首（pos == 0）时不得误触「无换行块越过」分支：重访一个 offset 停在
/// `token_usage_record` 行首、剩余 > 读窗的文件，本轮该封口行被**延迟**（不发射、
/// 状态与水位原样、无越过/失效诊断），下一轮足额正常发射——绝不整窗吞掉（评审修复轮 2）
#[test]
fn gated_seal_row_at_chunk_start_is_deferred_not_swallowed() {
    let (home, sessions) = shadow_home("codex-gatechunk");
    fn pad_rows(n: usize, ch: char) -> Vec<Value> {
        let pad = ch.to_string().repeat(3_900);
        (0..n)
            .map(|i| {
                json!({"timestamp": rfc3339(ANCHOR_MS + i as i64), "type": "event_msg",
                       "payload": {"type": "world_state", "pad": pad}})
            })
            .collect()
    }
    fn seal_row(ms: i64, rid: String, out: i64) -> Value {
        json!({"timestamp": rfc3339(ms), "type": "token_usage_record",
               "payload": {"response_id": rid, "session_id": "s-gate", "turn_id": "t-gate",
                           "usage": {"output_tokens": out}}})
    }
    // A：meta + 199 热身响应 + [user_p][turnctx_p][seal rid 9001][reasoning][seal 9002] + 3.5MiB 垫行
    let mut a_lines: Vec<String> = vec![json!({"timestamp": rfc3339(ANCHOR_MS), "type": "session_meta",
        "payload": {"cli_version": "0.164.0", "model_provider": "openai", "session_id": "s-gate-a"}}).to_string()];
    for i in 0..199usize {
        for r in synth_response(i, ANCHOR_MS + 1_000 * i as i64, 100) {
            a_lines.push(r.to_string());
        }
    }
    let ta = ANCHOR_MS + 500_000;
    for r in [
        json!({"timestamp": rfc3339(ta), "type": "response_item", "payload": {"type": "message", "role": "user"}}),
        json!({"timestamp": rfc3339(ta + 100), "type": "turn_context", "payload": {"turn_id": "t-gate-a", "model": "gpt-test"}}),
        seal_row(ta + 200, synth_rid(9001), 50),
        json!({"timestamp": rfc3339(ta + 700), "type": "response_item", "payload": {"type": "reasoning"}}),
        seal_row(ta + 800, synth_rid(9002), 60),
    ] {
        a_lines.push(r.to_string());
    }
    a_lines.extend(pad_rows(904, 'a').iter().map(|r| r.to_string()));
    // B：meta + 199 热身（rid 200..）+ 5MiB 垫行 + [user_b][turnctx_b][reasoning_b][seal rid 8001]
    let mut b_lines: Vec<String> = vec![json!({"timestamp": rfc3339(ANCHOR_MS), "type": "session_meta",
        "payload": {"cli_version": "0.164.0", "model_provider": "openai", "session_id": "s-gate-b"}}).to_string()];
    for i in 200..399usize {
        for r in synth_response(i, ANCHOR_MS + 100_000_000 + 1_000 * (i - 200) as i64, 100) {
            b_lines.push(r.to_string());
        }
    }
    let tb = ANCHOR_MS + 200_000_000;
    b_lines.extend(pad_rows(1290, 'b').iter().map(|r| r.to_string()));
    for r in [
        json!({"timestamp": rfc3339(tb), "type": "response_item", "payload": {"type": "message", "role": "user"}}),
        json!({"timestamp": rfc3339(tb + 100), "type": "turn_context", "payload": {"turn_id": "t-gate-b", "model": "gpt-test"}}),
        json!({"timestamp": rfc3339(tb + 600), "type": "response_item", "payload": {"type": "reasoning"}}),
        seal_row(tb + 700, synth_rid(8001), 90),
    ] {
        b_lines.push(r.to_string());
    }
    let file_b = install(&sessions, "gate-b", &(b_lines.to_vec().join("\n") + "\n"));
    let file_a = install(&sessions, "gate-a", &(a_lines.to_vec().join("\n") + "\n"));
    // 后装 A（mtime 更新 → 先访问）；行界字节数助手
    let prefix = |lines: &[String], upto: usize| -> u64 {
        lines[..upto].iter().map(|l| l.len() as u64 + 1).sum()
    };
    // A 检查点 = 199×4 热身行 + user_p + turnctx_p 之后（seal 9001 行首，pos > 0）
    let a_checkpoint = prefix(&a_lines, 1 + 199 * 4 + 2);
    // B 首轮检查点 = meta + 首热身响应的 user/turnctx/reasoning 之后（其封口行首）
    let b_checkpoint_p1 = prefix(&b_lines, 4);
    let b_checkpoint_p2 = prefix(&b_lines, b_lines.len() - 1);

    // 第一轮：A 先（199 发射 → 余量 1）在 rid 9001 行界前停批；B 冷启动被同一闸停在
    // 其首个热身封口行行界前
    let b1 = read(&home, &empty_cursor());
    assert_eq!(b1.observations.len(), 199, "只有 A 的热身发射");
    let v1 = cursor_json(&b1.next_cursor);
    let a_off = |v: &Value| {
        v["files"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["path"].as_str().unwrap().contains("gate-a"))
            .unwrap()["offset"]
            .as_u64()
            .unwrap()
    };
    let b_off = |v: &Value| {
        v["files"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["path"].as_str().unwrap().contains("gate-b"))
            .unwrap()["offset"]
            .as_u64()
            .unwrap()
    };
    assert_eq!(a_off(&v1), a_checkpoint, "A 停在 rid 9001 行首");
    assert_eq!(b_off(&v1), b_checkpoint_p1, "B 停在其首个热身封口行首");

    // 第二轮：拨 B 的 mtime（B 先访问）：B 发射 199 热身并吃掉 ~5MiB 字节后停在
    // rid 8001 行界前；随后 A 重访——块首恰是封口行 rid 9001、剩余 > 读窗：
    // 修复前此处整窗吞掉 ~2.6MiB 完整行并误使未决失效
    let _ = filetime::set_file_mtime(&file_b, filetime::FileTime::now());
    let b2 = read(&home, &b1.next_cursor);
    assert_eq!(
        b2.observations.len(),
        199,
        "本轮只有 B 的热身发射；A 被门控延迟"
    );
    let d2 = b2.diagnostics.join("\n");
    assert!(
        !d2.contains("single line exceeds") && !d2.contains("advancing past"),
        "块首封口行绝不得触发无换行块越过：{d2}"
    );
    assert!(
        !d2.contains("pending invalidated"),
        "延迟停批不得使任何未决状态失效：{d2}"
    );
    assert!(
        d2.contains("封口行发射余量不足"),
        "门控停批必须有诊断在案：{d2}"
    );
    let v2 = cursor_json(&b2.next_cursor);
    assert_eq!(
        a_off(&v2),
        a_checkpoint,
        "A 的水位必须原位不动（封口行整行留给下轮）"
    );
    assert_eq!(b_off(&v2), b_checkpoint_p2, "B 停在其尾部封口行首");

    // 第三轮：再拨 B（B 先，0 发射——其尾部封口延迟为未决）；A 足额结算：rid 9001
    // 正常发射（延迟未决 + 生成项 + 下一封口关闭）
    let _ = filetime::set_file_mtime(&file_b, filetime::FileTime::now());
    let b3 = read(&home, &b2.next_cursor);
    let keys3: Vec<_> = b3
        .observations
        .iter()
        .map(|o| o.stable_call_key.as_str())
        .collect();
    assert_eq!(keys3.len(), 2, "两笔延迟封口都下一轮正常结算：{keys3:?}");
    // B 的 rid 8001：窗（user_b → reasoning_b）在第二轮已闭合、只有封口行被延迟 →
    // 本轮照常结算；A 的 rid 9001 同理（延迟封口 + 生成项 + 下一封口关闭）
    assert!(keys3.contains(&synth_key(8001).as_str()), "{keys3:?}");
    assert!(keys3.contains(&synth_key(9001).as_str()), "{keys3:?}");
    let v3 = cursor_json(&b3.next_cursor);
    assert_eq!(a_off(&v3), size_of(&file_a), "A 读尽到 EOF");

    // 私有库收敛：199 + 199 + 2 = 400 行，键全互异（无丢无重）
    let mut c = conn();
    store::commit_batch(&mut c, &b1).unwrap();
    store::commit_batch(&mut c, &b2).unwrap();
    store::commit_batch(&mut c, &b3).unwrap();
    assert_eq!(count(&c, "tps_observation"), 400);
    let distinct: i64 = c
        .query_row(
            "SELECT COUNT(DISTINCT stable_call_key) FROM tps_observation",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(distinct, 400);
}
