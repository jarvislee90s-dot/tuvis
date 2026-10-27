//! WorkBuddy 真实数字兼容与旧 EOF 恢复（2026-10-26 桌面工作区与输出速度 V2 计划 Task 2）。
//!
//! ## 夹具来源与脱敏（Spec §9「已核对的 17 条 Mac 5.8.3 qua.end」）
//!
//! `tests/fixtures/tps-v2/workbuddy-qua.txt` = 本机 WorkBuddy 5.8.3 真实日志
//! （`~/.workbuddy/logs/2026-10-26/test__*.log`，**只读**提取）中的全部 17 条 qua.end 行，
//! 脱敏规则：
//! * `conversation_id` / `conversation_request_id` / `prompt_request_id` / `trace_id` /
//!   `message_id` 全部替换为合成稳定 id（按字段独立编号 `wb-conv-N` / `wb-creq-N` /
//!   `wb-preq-N` / `wb-trace-N` / `wb-msg-NN`）；total=6711 / ttft=1402 / completion=339
//!   的那条（相对序 15）message_id 固定为 `wb-known-6711`；
//! * 日志框架前缀 `[M/D/YYYY, h:mm:ss AM|PM.mmm]` 逐字保留（`pid` 与 `SseMonitor#` 内存
//!   标签归一为合成值）；17 行相对顺序与真实时间间隔原样；
//! * 全部数值字段保持**整数字符串**原始形态（这正是旧解析器整源跳过的根因）；
//! * 载荷其余字段（finish/model/token/interval 直方图等）无用户内容，逐字段保留。
//!
//! ## 隔离纪律（2026-10-07 事故族）
//!
//! 全部用例只触 tempdir 影子 home（`read_incremental(home,…)` 以入参 home 为唯一根）与
//! `support::open_ledger_db` 的用例私有库；绝不触真机 `~/.workbuddy` / `~/.tuvis`，
//! 绝不起 Tauri、不发模型请求。`now_ms` 取当前时钟：读取器的 24h 门只作用于**文件
//! mtime**（夹具复制后 mtime = 测试时刻），事件时刻只参与行前缀解析、不设窗。
//!
//! ## 覆盖（brief Step1/Step4 逐条）
//!
//! * 字符串 6711/1402/339 → 生成 5309 / 响应 6711；数字形态同值；
//! * 空串/布尔/小数/溢出 → 按字段诊断、不默认 0；
//! * 旧 v1 EOF 游标 → 有界恢复恰 17 条、稳定键 UPSERT 重放不翻倍、失败批不推进；
//! * 恢复完成后下一轮只消费追加字节；恢复预算 200/轮跨轮续跑；
//! * `parser-errors=<n>` detail 前缀可被查询面识别且不含载荷；
//! * 失败/取消两个速度均无值；AM/PM 括号前缀与 RFC3339 回退正例保留。

mod support;

use multi_agents_manager_lib::services::tps::model::{Outcome, SourceCursor};
use multi_agents_manager_lib::services::tps::sources::workbuddy;
use multi_agents_manager_lib::services::tps::store;
use rusqlite::Connection;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

// ==== 夹具与影子 home ====

/// 夹具全文（17 条真实 qua.end，已脱敏，数值保持字符串形态）
fn fixture_text() -> String {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tps-v2/workbuddy-qua.txt");
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("读取夹具失败（{}）：{e}", path.display()))
}

/// 夹具里 total=6711 / ttft=1402 / completion=339 的那条（message_id = wb-known-6711）
fn known_line() -> String {
    fixture_text()
        .lines()
        .find(|l| l.contains("\"wb-known-6711\""))
        .unwrap_or_else(|| panic!("夹具缺 wb-known-6711 行"))
        .to_string()
}

/// 已知行的载荷 JSON（字符串数值形态）
fn known_payload() -> Value {
    let l = known_line();
    let start = l.find('{').unwrap();
    let close = l.rfind('}').unwrap();
    serde_json::from_str(&l[start..=close]).unwrap()
}

/// 影子 home + 夹具落成读取器期望的日期目录 `.log`（tempdir 泄漏保活至用例结束）
fn shadow_home_with_fixture(tag: &str) -> (PathBuf, PathBuf) {
    let (home, log) = shadow_home(tag);
    std::fs::write(&log, fixture_text()).unwrap();
    (home, log)
}

fn shadow_home(tag: &str) -> (PathBuf, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join(tag);
    let log = home
        .join(".workbuddy")
        .join("logs")
        .join("2026-10-26")
        .join("cli_host__0__wbv2.log");
    std::fs::create_dir_all(log.parent().unwrap()).unwrap();
    std::mem::forget(temp); // 用例存活期间影子目录不得被清理（support.rs 同款）
    (home, log)
}

/// 「现在」：24h 门只看文件 mtime（夹具复制后 = 测试时刻），事件时刻不设窗
fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// 旧解析器会写下的 v1 游标形态：只有 files 字节界（无 format_version 字段 → 反序列化为 v1）
fn v1_eof_cursor(log: &Path) -> SourceCursor {
    let size = std::fs::metadata(log).unwrap().len();
    SourceCursor {
        source: "workbuddy".into(),
        value: json!({
            "files": { log.to_string_lossy(): { "offset": size, "skip_lines": 0 } }
        })
        .to_string(),
    }
}

/// 旧 v1 游标的字节界（断言恢复/增量推进量用）
fn v1_offset_of(cursor: &SourceCursor, log: &Path) -> u64 {
    let st: Value = serde_json::from_str(&cursor.value).unwrap();
    st["files"][log.to_string_lossy().as_ref()]["offset"]
        .as_u64()
        .unwrap_or_else(|| panic!("游标缺该文件字节界：{}", cursor.value))
}

// ==== 合成 qua 行（无效形态 / 追加行 / 大量行用）====

/// 真机日志框架的行前缀：`[M/D/YYYY, h:mm:ss AM|PM.mmm]`（本地时区，12 小时制）
fn wb_prefix(ts_ms: i64) -> String {
    use chrono::TimeZone;
    let dt = chrono::Local
        .timestamp_millis_opt(ts_ms)
        .single()
        .expect("测试时刻必须唯一（非 DST 歧义）");
    format!("[{}]", dt.format("%-m/%-d/%Y, %-I:%M:%S %p.%3f"))
}

/// 一条 qua.end 行（前缀形态 = 真机框架；载荷逐字给定）
fn qua_line(ts_ms: i64, fields: Value) -> String {
    format!(
        "{} [Info] [pid=1] [ModelProvider]  [SseMonitor#wb1] qua.end: {}",
        wb_prefix(ts_ms),
        fields
    )
}

// ==== 私有库断言助手 ====

fn conn() -> Connection {
    support::open_ledger_db("tps-workbuddy-v2")
}

fn count(conn: &Connection, table: &str) -> i64 {
    conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
        .unwrap_or_else(|e| panic!("计数查询失败（{table}，表缺失？）：{e}"))
}

/// 播种一行旧 Token/费用游标（恢复提交前后必须原样——只恢复 TPS，不动旧账本）
fn seed_old_usage_cursor(conn: &Connection) {
    conn.execute_batch(
        "INSERT INTO usage_cursor
             (source_id, session_id, fingerprint, byte_offset, ordinal, last_cumulative,
              mtime_ms, file_size, state_json, updated_at)
             VALUES ('workbuddy','s1','fp',42,3,99,123,456,'{}',789);",
    )
    .expect("播种旧 usage_cursor 失败");
}

// ===========================================================================
// 数字归一：字符串数值与数字孪生
// ===========================================================================

/// Spec §9 样例：字符串 "6711"/"1402"/"339" → 生成 5309、响应 6711；
/// 流式 ≈ 63.85、响应 ≈ 50.51（由记录在测试内推导）
#[test]
fn string_numbers_admit_and_fill_both_windows() {
    let (home, log) = shadow_home("wbv2-str");
    std::fs::write(&log, known_line() + "\n").unwrap();

    let batch =
        workbuddy::read_incremental(home.as_path(), &SourceCursor::empty("workbuddy"), now_ms())
            .unwrap();
    assert_eq!(
        batch.observations.len(),
        1,
        "字符串数值必须被归一接纳（旧实现整源跳过）"
    );
    let o = &batch.observations[0];

    assert_eq!(o.stable_call_key, "wb-known-6711");
    assert_eq!(o.generation_ms, Some(6711 - 1402), "生成窗 = total − ttft");
    assert_eq!(
        o.response_elapsed_ms,
        Some(6711),
        "响应窗 = total（含等待）"
    );
    assert_eq!(o.native_duration_ms, Some(6711));
    assert_eq!(o.output_tokens, Some(339));
    assert_eq!(o.ttft_ms(), Some(1402));

    // 两条速度都有效，且数值由记录本身推导（Spec §9：≈63.85 / ≈50.51）
    assert!(o.is_tps_valid());
    assert!(o.is_response_valid());
    let stream = 1000.0 * o.output_tokens.unwrap() as f64 / o.generation_ms.unwrap() as f64;
    let response =
        1000.0 * o.output_tokens.unwrap() as f64 / o.response_sample_ms().unwrap() as f64;
    assert!(
        (stream - 1000.0 * 339.0 / 5309.0).abs() < 1e-9,
        "stream={stream}"
    );
    assert!((stream - 63.85).abs() < 0.01, "流式 ≈63.85，实得 {stream}");
    assert!(
        (response - 1000.0 * 339.0 / 6711.0).abs() < 1e-9,
        "response={response}"
    );
    assert!(
        (response - 50.51).abs() < 0.01,
        "响应 ≈50.51，实得 {response}"
    );
}

/// 数字形态孪生：同载荷把三个字段换成 JSON 整数 → 事实逐字段一致
#[test]
fn numeric_typed_twin_gives_identical_facts() {
    let str_line = known_line();
    let mut twin = known_payload();
    twin["message_id"] = json!("wb-twin-6711");
    twin["total_cost_ms"] = json!(6711);
    twin["ttft_ms"] = json!(1402);
    twin["completion_tokens"] = json!(339);
    let (home, log) = shadow_home("wbv2-twin");
    std::fs::write(&log, format!("{str_line}\n{}\n", qua_line(now_ms(), twin))).unwrap();

    let batch =
        workbuddy::read_incremental(home.as_path(), &SourceCursor::empty("workbuddy"), now_ms())
            .unwrap();
    assert_eq!(batch.observations.len(), 2, "字符串形态与数字形态各一条");
    let s = batch
        .observations
        .iter()
        .find(|o| o.stable_call_key == "wb-known-6711")
        .unwrap();
    let n = batch
        .observations
        .iter()
        .find(|o| o.stable_call_key != "wb-known-6711")
        .unwrap();
    assert_eq!(s.generation_ms, n.generation_ms);
    assert_eq!(s.response_elapsed_ms, n.response_elapsed_ms);
    assert_eq!(s.native_duration_ms, n.native_duration_ms);
    assert_eq!(s.output_tokens, n.output_tokens);
    assert_eq!(s.outcome, n.outcome);
    assert_eq!(s.is_tps_valid(), n.is_tps_valid());
    assert_eq!(s.is_response_valid(), n.is_response_valid());
    // 时刻锚定在各自行前缀：同一 6711/1402/339 必得同一内部窗结构
    for o in [s, n] {
        assert_eq!(o.ended_at_ms - o.started_at_ms, 6711);
        assert_eq!(o.first_token_at_ms.map(|t| t - o.started_at_ms), Some(1402));
    }
}

/// 空串/布尔/小数/溢出逐形态诊断、不默认 0；total 被拒的行整体跳过（不造起点）
#[test]
fn invalid_numeric_forms_diagnosed_not_zeroed() {
    let (home, log) = shadow_home("wbv2-bad");
    let t = now_ms();
    let mk = |msg: &str, total: Value, ttft: Value, comp: Value| {
        let mut p = known_payload();
        p["message_id"] = json!(msg);
        p["total_cost_ms"] = total;
        p["ttft_ms"] = ttft;
        p["completion_tokens"] = comp;
        p["finish_reason"] = json!("stop");
        p["finish_flag"] = json!("done");
        qua_line(t, p)
    };
    // completion_tokens 的四种拒绝形态（行内身份/计时齐全 → 事实保留、不入速、不报 0）
    let lines = [
        mk("wb-bad-empty", json!("6711"), json!("1402"), json!("")),
        mk("wb-bad-bool", json!("6711"), json!("1402"), json!(true)),
        mk(
            "wb-bad-decimal",
            json!("6711"),
            json!("1402"),
            json!("339.0"),
        ),
        mk(
            "wb-bad-overflow",
            json!("6711"),
            json!("1402"),
            json!("9223372036854775808"),
        ),
        // total_cost_ms 被拒 → 无法定位起点，整行跳过（绝不造起点/默认 0）
        mk("wb-bad-total", json!("6711.0"), json!("1402"), json!("339")),
    ];
    std::fs::write(&log, lines.join("\n") + "\n").unwrap();

    let batch =
        workbuddy::read_incremental(home.as_path(), &SourceCursor::empty("workbuddy"), now_ms())
            .unwrap();
    assert_eq!(
        batch.observations.len(),
        4,
        "total 被拒的行跳过，其余四行事实保留"
    );
    for o in &batch.observations {
        assert_eq!(
            o.output_tokens, None,
            "被拒数值是缺失，绝不伪装成 0：{}",
            o.stable_call_key
        );
        assert!(!o.is_tps_valid(), "被拒数值不入速度");
        assert!(!o.is_response_valid());
    }
    // 诊断按 类型/字段 可辨（五种拒绝各一条），且不含载荷原文
    let detail = batch.source_state.detail.clone().unwrap_or_default();
    assert!(
        detail.starts_with("parser-errors=5"),
        "五种拒绝形态必须计入 parser-errors 前缀：{detail}"
    );
    for needle in [
        "completion_tokens:empty-string",
        "completion_tokens:bool",
        "completion_tokens:bad-string",
        "completion_tokens:overflow",
        "total_cost_ms:bad-string",
    ] {
        assert!(detail.contains(needle), "detail 缺 {needle}：{detail}");
    }
    assert!(
        !detail.contains("wb-bad-"),
        "detail 只含类型/字段/计数，不得含载荷原文：{detail}"
    );
}

/// 负起点护栏（二轮复审 B1 回归）：宿主写出的 total_cost_ms 超过行前缀时刻 →
/// 起点 = ended − total 为负 → 旧代码会把负值直塞观察，落库撞
/// `CHECK(started_at_ms >= 0)` 整批 Err → 游标不推进 → 同批重放死循环。
/// 现约定：该行以 `negative-start` ASCII 诊断保守跳过（既有跳过路径），批仍 ok、
/// 游标照常越过该行，落库零错误。
#[test]
fn negative_start_from_oversized_total_is_skipped_and_cursor_advances() {
    let (home, log) = shadow_home("wbv2-negstart");
    let t = now_ms();
    // 坏行：total = 9e15 ms（≈28.5 万年，任何真实时刻都远小于它）→ 起点必为负
    let mut bad = known_payload();
    bad["message_id"] = json!("wb-negstart-bad");
    bad["total_cost_ms"] = json!("9000000000000000");
    bad["ttft_ms"] = json!("1402");
    bad["completion_tokens"] = json!("339");
    // 好行：同批的正常记录必须照常入批（单行异常不拖死整批）
    let good = qua_line(t, {
        let mut p = known_payload();
        p["message_id"] = json!("wb-negstart-ok");
        p
    });
    std::fs::write(&log, format!("{}\n{}\n", qua_line(t, bad), good)).unwrap();

    let batch =
        workbuddy::read_incremental(home.as_path(), &SourceCursor::empty("workbuddy"), now_ms())
            .unwrap();
    assert_eq!(
        batch.observations.len(),
        1,
        "负起点行跳过，好行照常入批：{:?}",
        batch.diagnostics
    );
    assert_eq!(batch.observations[0].stable_call_key, "wb-negstart-ok");
    assert!(
        batch
            .diagnostics
            .iter()
            .any(|d| d.contains("negative-start") && d.contains("field=total_cost_ms")),
        "负起点行必须带 ASCII 诊断：{:?}",
        batch.diagnostics
    );
    assert!(batch.source_state.ok, "批不得因单行负起点整体失败");
    // 游标推进到文件尾：跳过的行也被消费（否则下轮重放同批 → 死循环）
    let next: Value = serde_json::from_str(&batch.next_cursor.value).unwrap();
    let offset = next["files"][log.to_string_lossy().as_ref()]["offset"]
        .as_u64()
        .unwrap_or_else(|| panic!("v2 游标缺文件字节界：{}", batch.next_cursor.value));
    assert_eq!(
        offset,
        std::fs::metadata(&log).unwrap().len(),
        "跳过行也要推进游标"
    );
    // 落库零错误（护栏缺失时 CHECK(started_at_ms >= 0) 会在这里整批失败）
    let mut c = conn();
    store::commit_batch(&mut c, &batch).unwrap();
    assert_eq!(count(&c, "tps_observation"), 1, "只有好行落库");
}

/// 零起点边界（scoped 评审 ① 回归）：total_cost_ms **恰等于**行前缀时刻 →
/// started_at_ms == 0——守卫是 `Some(s) if s >= 0`，恰等边界合法放行（满足库表
/// `CHECK(started_at_ms >= 0)`，起点 = epoch 0 是真实值不是造值）。锁死下界形态：
/// 未来若有人把 `s >= 0` 误收紧为 `s > 0`（把零起点一并拒了），本用例必须报红。
#[test]
fn zero_start_boundary_total_equals_line_time_is_admitted() {
    let (home, log) = shadow_home("wbv2-zerostart");
    let t = now_ms();
    // 边界行：total == 行时刻（字符串数值三件套 + 成功收尾，复用已知夹具载荷）
    let edge = qua_line(t, {
        let mut p = known_payload();
        p["message_id"] = json!("wb-zerostart-edge");
        p["total_cost_ms"] = json!(t.to_string());
        p
    });
    // 相邻正常行：零起点行不拖累同批
    let good = qua_line(t, {
        let mut p = known_payload();
        p["message_id"] = json!("wb-zerostart-ok");
        p
    });
    std::fs::write(&log, format!("{edge}\n{good}\n")).unwrap();

    let batch =
        workbuddy::read_incremental(home.as_path(), &SourceCursor::empty("workbuddy"), now_ms())
            .unwrap();
    assert_eq!(batch.observations.len(), 2, "边界行与正常行都照常入批");
    let edge_obs = batch
        .observations
        .iter()
        .find(|o| o.stable_call_key == "wb-zerostart-edge")
        .expect("零起点行必须照常发射");
    assert_eq!(
        edge_obs.started_at_ms, 0,
        "total == 行时刻 ⇒ 起点恰为 epoch 0（合法，不是负起点）"
    );
    assert_eq!(
        edge_obs.ended_at_ms, t,
        "终点 = 行前缀时刻（前缀往返逐毫秒精确）"
    );
    assert!(
        !batch
            .diagnostics
            .iter()
            .any(|d| d.contains("negative-start")),
        "恰等边界不得触发 negative-start 诊断：{:?}",
        batch.diagnostics
    );
    // 游标推进到文件尾，落库零错误（started_at_ms=0 满足 CHECK）
    let next: Value = serde_json::from_str(&batch.next_cursor.value).unwrap();
    let offset = next["files"][log.to_string_lossy().as_ref()]["offset"]
        .as_u64()
        .unwrap_or_else(|| panic!("v2 游标缺文件字节界：{}", batch.next_cursor.value));
    assert_eq!(offset, std::fs::metadata(&log).unwrap().len());
    let mut c = conn();
    store::commit_batch(&mut c, &batch).unwrap();
    assert_eq!(count(&c, "tps_observation"), 2, "零起点行照常落库");
}

// ===========================================================================
// 旧 v1 EOF 游标的有界恢复
// ===========================================================================

/// 核心：v1 游标已越过全部记录（EOF、零事实）→ 有界恢复恰 17 条；
/// 稳定键 UPSERT 重放不翻倍；旧账本游标与其他来源游标原样
#[test]
fn v1_eof_cursor_recovers_17_and_replay_idempotent() {
    let (home, log) = shadow_home_with_fixture("wbv2-recover");
    let mut c = conn();
    seed_old_usage_cursor(&c);

    let old_cursor = v1_eof_cursor(&log);
    // 恢复批（旧实现：EOF 游标 → 0 条 = 本测试的 RED 根由）
    let recovered = workbuddy::read_incremental(home.as_path(), &old_cursor, now_ms()).unwrap();
    assert_eq!(recovered.observations.len(), 17);
    let sample = recovered
        .observations
        .iter()
        .find(|o| o.stable_call_key == "wb-known-6711")
        .unwrap();
    assert_eq!(sample.response_elapsed_ms, Some(6711));

    // 恢复完成后游标升 v2 且无残留恢复队列
    let next: Value = serde_json::from_str(&recovered.next_cursor.value).unwrap();
    assert_eq!(next["format_version"], 2, "恢复完成必须写 format_version=2");
    assert!(next["recovery"].is_null(), "恢复队列必须清空");
    assert_eq!(
        v1_offset_of(&recovered.next_cursor, &log),
        std::fs::metadata(&log).unwrap().len()
    );

    // 事实 + 游标同事务落库；旧 usage_cursor / 其他来源游标不动
    store::commit_batch(&mut c, &recovered).unwrap();
    let after_first_count = count(&c, "tps_observation");
    assert_eq!(after_first_count, 17, "恢复批 17 条全部落库");
    assert_eq!(
        count(&c, "usage_cursor"),
        1,
        "旧 Token/费用游标不得被恢复触碰"
    );
    assert!(
        store::load_cursor(&c, "dsh").unwrap().is_none(),
        "其他来源游标不得被创建"
    );

    // 重放（同 v1 游标再恢复一遍 → 同一批事实 UPSERT）：行数不翻倍
    let replay = workbuddy::read_incremental(home.as_path(), &old_cursor, now_ms()).unwrap();
    assert_eq!(
        replay.observations.len(),
        17,
        "恢复可重入（UPSERT 幂等的前提）"
    );
    store::commit_batch(&mut c, &replay).unwrap();
    let after_replay_count = count(&c, "tps_observation");
    assert_eq!(after_replay_count, after_first_count);
}

/// 恢复中途失败批不推进：事实/游标/来源状态同事务回滚，游标保持原水位
#[test]
fn failed_commit_leaves_cursor_and_facts_uncommitted() {
    let (home, log) = shadow_home_with_fixture("wbv2-fail");
    let mut c = conn();
    seed_old_usage_cursor(&c);
    // 先落一个已知水位（恢复前半段形态：v1 + 恢复队列），作为「失败后不变」的基准
    let old_cursor = v1_eof_cursor(&log);
    store::commit_batch(
        &mut c,
        &workbuddy::read_incremental(home.as_path(), &old_cursor, now_ms()).unwrap(),
    )
    .unwrap();
    let baseline = store::load_cursor(&c, "workbuddy").unwrap().unwrap();
    let baseline_rows = count(&c, "tps_observation");
    assert_eq!(baseline_rows, 17);
    assert_eq!(
        serde_json::from_str::<Value>(&baseline.value).unwrap()["format_version"],
        2,
        "前提：17 条一轮恢复完，基准游标已是 v2"
    );

    // 制造一个失败的恢复批（epoch 非负 CHECK 是确定性注入缝）
    let mut bad = workbuddy::read_incremental(home.as_path(), &old_cursor, now_ms()).unwrap();
    assert!(!bad.observations.is_empty(), "v1 游标重入必须可再恢复");
    bad.observations[8].ended_at_ms = -1;
    assert!(
        store::commit_batch(&mut c, &bad).is_err(),
        "批内失败必须冒泡"
    );

    // 原子性：失败批的事实、游标、来源状态全部未落
    assert_eq!(
        count(&c, "tps_observation"),
        baseline_rows,
        "失败批不得残留事实"
    );
    assert_eq!(
        store::load_cursor(&c, "workbuddy").unwrap().unwrap(),
        baseline,
        "失败批的恢复检查点不得推进"
    );
}

/// 恢复完成后下一轮只消费追加字节：偏移推进量 = 追加字节数，旧行零重放
#[test]
fn after_recovery_only_appended_bytes_consumed() {
    let (home, log) = shadow_home_with_fixture("wbv2-append");
    let old_cursor = v1_eof_cursor(&log);
    let first = workbuddy::read_incremental(home.as_path(), &old_cursor, now_ms()).unwrap();
    assert_eq!(first.observations.len(), 17);
    let offset_after_recovery = v1_offset_of(&first.next_cursor, &log);

    // 追加一条（字符串数值形态，同真实日志的追加方式）
    let mut p = known_payload();
    p["message_id"] = json!("wb-msg-appended");
    p["total_cost_ms"] = json!("2000");
    p["ttft_ms"] = json!("400");
    p["completion_tokens"] = json!("64");
    p["finish_reason"] = json!("stop");
    let appended = qua_line(now_ms(), p);
    let appended_len = appended.len() as u64 + 1; // 含结尾换行
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
        f.write_all(appended.as_bytes()).unwrap();
        f.write_all(b"\n").unwrap();
    }

    let second = workbuddy::read_incremental(home.as_path(), &first.next_cursor, now_ms()).unwrap();
    assert_eq!(
        second.observations.len(),
        1,
        "只消费追加行，不重读恢复过的前缀"
    );
    assert_eq!(second.observations[0].stable_call_key, "wb-msg-appended");
    assert_eq!(
        v1_offset_of(&second.next_cursor, &log),
        offset_after_recovery + appended_len,
        "字节界推进量必须恰等于追加字节数（恢复后不反复全读）"
    );

    let mut c = conn();
    store::commit_batch(&mut c, &first).unwrap();
    store::commit_batch(&mut c, &second).unwrap();
    assert_eq!(count(&c, "tps_observation"), 18, "17 条恢复 + 1 条追加");
}

/// 恢复预算 200/轮：>200 条时跨轮续跑（检查点 = 字节界 + 行号），恢复完成后只余增量
#[test]
fn recovery_checkpoint_resumes_across_passes() {
    let (home, log) = shadow_home("wbv2-budget");
    let mut p = known_payload();
    p["total_cost_ms"] = json!("5000");
    p["ttft_ms"] = json!("1000");
    p["completion_tokens"] = json!("100");
    p["finish_reason"] = json!("stop");
    let mut body = String::new();
    for i in 0..220 {
        p["message_id"] = json!(format!("wb-budget-{i:03}"));
        body.push_str(&qua_line(now_ms(), p.clone()));
        body.push('\n');
    }
    std::fs::write(&log, body).unwrap();

    let old_cursor = v1_eof_cursor(&log);
    let pass1 = workbuddy::read_incremental(home.as_path(), &old_cursor, now_ms()).unwrap();
    assert_eq!(
        pass1.observations.len(),
        200,
        "单批观察上限 200（恢复同受此预算）"
    );
    let st1: Value = serde_json::from_str(&pass1.next_cursor.value).unwrap();
    assert_eq!(st1["format_version"], 1, "恢复未完成：游标必须保持 v1");
    assert!(
        !st1["recovery"]["pending"].as_array().unwrap().is_empty(),
        "未恢复完的文件必须留在恢复队列"
    );

    let mut c = conn();
    store::commit_batch(&mut c, &pass1).unwrap();

    let pass2 = workbuddy::read_incremental(home.as_path(), &pass1.next_cursor, now_ms()).unwrap();
    assert_eq!(
        pass2.observations.len(),
        20,
        "第二轮从检查点续跑，只补余量（不重启）"
    );
    let st2: Value = serde_json::from_str(&pass2.next_cursor.value).unwrap();
    assert_eq!(st2["format_version"], 2, "恢复队列清空后升 v2");
    assert!(st2["recovery"].is_null());
    store::commit_batch(&mut c, &pass2).unwrap();
    assert_eq!(
        count(&c, "tps_observation"),
        220,
        "两轮合计恰 220 条，无重无漏"
    );

    let pass3 = workbuddy::read_incremental(home.as_path(), &pass2.next_cursor, now_ms()).unwrap();
    assert!(
        pass3.observations.is_empty(),
        "恢复完成后重读为空（不反复全读）"
    );
}

// ===========================================================================
// 诊断面与速度有效性
// ===========================================================================

/// 解析错误可被查询面识别：`SourceState.detail` 以 `parser-errors=<n>` 开头、只含
/// 类型/字段/计数；零解析错误时不带该前缀
#[test]
fn parser_error_detail_prefix_visible_and_payload_free() {
    let (home, log) = shadow_home("wbv2-detail");
    let mut p = known_payload();
    p["message_id"] = json!("wb-ok");
    let t = now_ms();
    let mut bad1 = p.clone();
    bad1["message_id"] = json!("wb-bad-comp");
    bad1["completion_tokens"] = json!("339.0");
    let mut bad2 = p.clone();
    bad2["message_id"] = json!("wb-bad-ttft");
    bad2["ttft_ms"] = json!(true);
    std::fs::write(
        &log,
        format!(
            "{}\n{}\n{}\n",
            qua_line(t, p),
            qua_line(t + 1_000, bad1),
            qua_line(t + 2_000, bad2)
        ),
    )
    .unwrap();

    let batch =
        workbuddy::read_incremental(home.as_path(), &SourceCursor::empty("workbuddy"), now_ms())
            .unwrap();
    assert_eq!(
        batch.observations.len(),
        3,
        "被拒字段按缺失处理，事实行保留"
    );
    let detail = batch.source_state.detail.clone().unwrap_or_default();
    assert!(
        detail.starts_with("parser-errors=2"),
        "detail 必须以稳定的 parser-errors 前缀开头（Task 6 依赖）：{detail}"
    );
    assert!(detail.contains("completion_tokens:bad-string"), "{detail}");
    assert!(detail.contains("ttft_ms:bool"), "{detail}");
    assert!(!detail.contains('{'), "detail 不得含载荷原文：{detail}");
    assert!(
        !detail.contains("wb-bad"),
        "detail 不得含载荷标识：{detail}"
    );

    // 零解析错误：不带前缀（保持原有来源限制说明）
    let (home2, log2) = shadow_home("wbv2-detail-clean");
    std::fs::write(&log2, known_line() + "\n").unwrap();
    let clean =
        workbuddy::read_incremental(home2.as_path(), &SourceCursor::empty("workbuddy"), now_ms())
            .unwrap();
    let clean_detail = clean.source_state.detail.clone().unwrap_or_default();
    assert!(
        !clean_detail.starts_with("parser-errors="),
        "零解析错误不得带前缀：{clean_detail}"
    );
    assert!(
        clean_detail.contains("insufficient-evidence"),
        "来源限制说明保留"
    );
}

/// 失败/取消的记录照发事实（outcome 原样），但流式与响应两个速度均无值
#[test]
fn cancelled_and_failed_have_neither_speed() {
    let (home, log) = shadow_home("wbv2-cancel");
    let t = now_ms();
    let mut cancelled = known_payload();
    cancelled["message_id"] = json!("wb-cancel");
    cancelled["finish_flag"] = json!("canceled");
    let mut failed = known_payload();
    failed["message_id"] = json!("wb-failed");
    failed["model_error"] = json!("stream_timeout");
    failed["finish_reason"] = json!("error");
    std::fs::write(
        &log,
        format!(
            "{}\n{}\n",
            qua_line(t, cancelled),
            qua_line(t + 1_000, failed)
        ),
    )
    .unwrap();

    let batch =
        workbuddy::read_incremental(home.as_path(), &SourceCursor::empty("workbuddy"), now_ms())
            .unwrap();
    assert_eq!(batch.observations.len(), 2, "失败/取消照发事实");
    for o in &batch.observations {
        assert_ne!(o.outcome, Outcome::Success, "outcome 判定不变");
        assert!(!o.is_tps_valid(), "{}：流式速度无值", o.stable_call_key);
        assert!(
            !o.is_response_valid(),
            "{}：响应速度无值",
            o.stable_call_key
        );
        assert!(o.failure_reason.is_some());
    }
}

/// 正例保留：AM/PM 括号本地前缀与 RFC3339 回退都能锚定结束时刻（字符串数值形态）
#[test]
fn am_pm_prefix_and_rfc3339_fallback_positive() {
    use chrono::TimeZone;
    // AM：夹具已知行的原始时刻形态（10/26/2026, 9:30:56 AM.681，本地时区）
    let (home, log) = shadow_home("wbv2-ampm");
    std::fs::write(&log, known_line() + "\n").unwrap();
    let batch =
        workbuddy::read_incremental(home.as_path(), &SourceCursor::empty("workbuddy"), now_ms())
            .unwrap();
    assert_eq!(batch.observations.len(), 1);
    let want_am = chrono::Local
        .with_ymd_and_hms(2026, 10, 26, 9, 30, 56)
        .single()
        .unwrap()
        .timestamp_millis()
        + 681;
    assert_eq!(
        batch.observations[0].ended_at_ms, want_am,
        "AM 括号前缀按本地时区锚定结束时刻"
    );

    // PM：真机文档形态 [10/26/2026, 10:03:57 PM.945]（22 时）
    let mut pm = known_payload();
    pm["message_id"] = json!("wb-pm");
    let (home2, log2) = shadow_home("wbv2-pm");
    std::fs::write(
        &log2,
        format!("[10/26/2026, 10:03:57 PM.945] [Info] [pid=1] [ModelProvider]  [SseMonitor#wb1] qua.end: {pm}\n"),
    )
    .unwrap();
    let batch2 =
        workbuddy::read_incremental(home2.as_path(), &SourceCursor::empty("workbuddy"), now_ms())
            .unwrap();
    assert_eq!(batch2.observations.len(), 1);
    let want_pm = chrono::Local
        .with_ymd_and_hms(2026, 10, 26, 22, 3, 57)
        .single()
        .unwrap()
        .timestamp_millis()
        + 945;
    assert_eq!(
        batch2.observations[0].ended_at_ms, want_pm,
        "PM 前缀 12→24 时换算正确"
    );
    assert_eq!(batch2.observations[0].generation_ms, Some(5309));
    assert_eq!(batch2.observations[0].response_elapsed_ms, Some(6711));

    // RFC3339 回退：ISO 前缀行仍可解析（兼容锁）
    let (home3, log3) = shadow_home("wbv2-rfc");
    std::fs::write(
        &log3,
        format!("[2026-10-26T09:30:56.681Z] [Info] qua.end: {pm}\n"),
    )
    .unwrap();
    let batch3 =
        workbuddy::read_incremental(home3.as_path(), &SourceCursor::empty("workbuddy"), now_ms())
            .unwrap();
    assert_eq!(batch3.observations.len(), 1, "RFC3339 前缀仍可解析");
    let want_rfc = chrono::DateTime::parse_from_rfc3339("2026-10-26T09:30:56.681Z")
        .unwrap()
        .timestamp_millis();
    assert_eq!(
        batch3.observations[0].ended_at_ms, want_rfc,
        "RFC3339 时刻按绝对时刻锚定"
    );
}

/// 恢复只触 qua 路线：sdk/conversations 的 hasUsage 行不产观察也不进恢复集（其字节界原样）
#[test]
fn recovery_leaves_sdk_route_untouched() {
    let (home, log) = shadow_home_with_fixture("wbv2-sdk");
    let sdk_dir = home
        .join(".workbuddy")
        .join("logs")
        .join("2026-10-26")
        .join("sdk")
        .join("conversations");
    std::fs::create_dir_all(&sdk_dir).unwrap();
    let sdk_log = sdk_dir.join("conv-1.log");
    let sdk_body = "[2026-10-26T01:00:00.000Z] [Info] usage_update {\"hasUsage\":true}\n";
    std::fs::write(&sdk_log, sdk_body).unwrap();

    let old_cursor = v1_eof_cursor(&log);
    let batch = workbuddy::read_incremental(home.as_path(), &old_cursor, now_ms()).unwrap();
    assert_eq!(batch.observations.len(), 17, "恢复只产 qua 观察");
    let next: Value = serde_json::from_str(&batch.next_cursor.value).unwrap();
    assert_eq!(
        next["files"][sdk_log.to_string_lossy().as_ref()]["offset"],
        sdk_body.len() as u64,
        "sdk 文件走正常增量消费（不产观察），字节界照常推进"
    );
}
