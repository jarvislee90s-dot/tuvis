//! TPS OpenCode 来源集成测试（2026-10-24 计划 Task 6）。
//!
//! 隔离纪律（2026-10-07 事故族，与 tps_native_*_test.rs 同款）：全部夹具是 tempdir
//! 下的**合成**事实 JSONL，行形态逐字取自自家观察插件
//! `resources/tps/opencode/tuvis-tps-observer.ts` 的产出契约（15 键 camelCase）；
//! 事件词表/信封取自宿主 opencode v2.0.22 二进制核对的形态（见 NOTICE.md）。
//! 绝不触真机 `~/.config/opencode` / `~/.opencode` / `~/.tuvis`：
//! `read_incremental(home, …)` 与 `*_at(home, …)` 都以**入参 home 为唯一根**，
//! 测试传 tempdir 即天然隔离。
//!
//! 覆盖（brief Step1 + Spec §7 验收点）：
//! * 标准链路：start → 空delta → 非空delta → end 同键成观察（StreamObserved）
//! * 增量游标：只读新增；同批重复 eventKey 幂等；跨批不翻倍
//! * 已识别未对齐重试剔除速度（retry 行 + end.attempt 双源取大）
//! * 失败/取消不入速度（取消 = MessageAbortedError → cancelled 行）
//! * 两会话/两 message 交错不串键（每键独立 stepIndex）
//! * 残缺尾行不越过（下轮重读不丢不重）；文件收缩按新文件重读
//! * usage 只取 end 行自带值（累计面永不入行 → 不重复入账）
//! * 缺起点/缺模型不落无名行
//! * 安装包嵌入断言（可加载 JS / manifest / MIT 许可）+ 安装所有权生命周期

use multi_agents_manager_lib::services::tps::install::{
    self, InstallChoice, OPENCODE_PLUGIN_JS, OPENCODE_PLUGIN_LICENSE, OPENCODE_PLUGIN_MANIFEST,
    OPENCODE_PLUGIN_NOTICE,
};
use multi_agents_manager_lib::services::tps::model::{
    Outcome, OutputScope, SourceCursor, TimingMethod,
};
use multi_agents_manager_lib::services::tps::sources::opencode;
use multi_agents_manager_lib::services::tps::sources::opencode::ADAPTER_STATUS;
use multi_agents_manager_lib::services::tps::store;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// 唯一临时根（进程内计数避免碰撞）
static SEQ: AtomicU64 = AtomicU64::new(0);

fn temp_home() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "tps-opencode-test-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 事实文件路径（与 reader/插件同式推导）
fn facts_path(home: &Path) -> PathBuf {
    home.join(".tuvis")
        .join("tps")
        .join("opencode")
        .join("facts.jsonl")
}

/// 写一行事实（键集 = 插件产出契约的 15 键；缺省值与插件 blankRow 一致）
/// 夹具时刻 = BASE_MS + 偏移（与 NOW_MS 同一假时钟，保证保留窗语义成立）
fn row(event_key: &str, r#type: &str, created_offset: i64, extra: Value) -> String {
    let mut base = json!({
        "eventKey": event_key,
        "sessionKey": "a1b2c3d4a1b2c3d4a1b2c3d4a1b2c3d4a1b2c3d4a1b2c3d4a1b2c3d4a1b2c3d4",
        "messageKey": "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
        "ordinal": 1,
        "attempt": 0,
        "type": r#type,
        "created": BASE_MS + created_offset,
        "started": null,
        "tokens": null,
        "finish": null,
        "hasNonEmptyDelta": false,
        "deltaChars": 0,
        "model": null,
        "provider": null,
        "tokensReasoning": null,
    });
    if let Value::Object(map) = extra {
        for (k, v) in map {
            base[k] = v;
        }
    }
    base.to_string()
}

const SK: &str = "a1b2c3d4a1b2c3d4a1b2c3d4a1b2c3d4a1b2c3d4a1b2c3d4a1b2c3d4a1b2c3d4";
const MK: &str = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";

fn append_lines(home: &Path, lines: &[String]) {
    let path = facts_path(home);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .unwrap();
    for line in lines {
        writeln!(f, "{line}").unwrap();
    }
}

fn read(
    home: &Path,
    cursor: &SourceCursor,
) -> multi_agents_manager_lib::services::tps::model::ReadBatch {
    opencode::read_incremental(home, cursor, BASE_MS + 2 * 60 * 60 * 1000).unwrap()
}

/// 固定假时钟基准（纯整数，不读系统时钟）
const BASE_MS: i64 = 1_700_000_000_000;

fn empty_cursor() -> SourceCursor {
    SourceCursor::empty("opencode")
}

/// 标准链路夹具：start(1000, started=900) → 空delta(1500) → 非空delta(2000) → end(5000, output=300)
fn happy_lines() -> Vec<String> {
    vec![
        row(
            "e-s",
            "start",
            1000,
            json!({"started": BASE_MS + 900, "model": "glm-5.3-flash", "provider": "volcengine"}),
        ),
        row(
            "e-d0",
            "delta",
            1500,
            json!({"deltaChars": 0, "hasNonEmptyDelta": false}),
        ),
        row(
            "e-d1",
            "delta",
            2000,
            json!({"deltaChars": 3, "hasNonEmptyDelta": true}),
        ),
        row(
            "e-e",
            "end",
            5000,
            json!({"tokens": 300, "finish": "stop", "tokensReasoning": 0}),
        ),
    ]
}

// ==== 读取器 ====

#[test]
fn happy_path_maps_to_stream_observed_observation() {
    let home = temp_home();
    append_lines(&home, &happy_lines());
    let batch = read(&home, &empty_cursor());
    assert_eq!(batch.observations.len(), 1);
    let o = &batch.observations[0];
    assert_eq!(o.source, "opencode");
    assert_eq!(o.method, TimingMethod::StreamObserved);
    assert_eq!(o.outcome, Outcome::Success);
    assert_eq!(o.started_at_ms, BASE_MS + 900);
    assert_eq!(o.first_token_at_ms, Some(BASE_MS + 2000));
    assert_eq!(o.ended_at_ms, BASE_MS + 5000);
    assert_eq!(o.generation_ms, Some(3000)); // 首非空 delta → end
    assert_eq!(o.native_duration_ms, Some(4100)); // 宿主 started → end
    assert_eq!(o.output_tokens, Some(300));
    assert_eq!(o.output_scope, OutputScope::ProviderReported);
    assert_eq!(o.model, "glm-5.3-flash");
    assert_eq!(o.channel.as_deref(), Some("volcengine"));
    assert_eq!(o.session_key.as_deref(), Some(SK));
    assert_eq!(o.turn_key, None); // 无原生回合键 → unknown-turn 降级
    assert_eq!(o.stable_call_key, format!("oc:{SK}:{MK}:1"));
    assert!(o.is_tps_valid());
    assert!(batch.source_state.ok);
    // 机器可读适配状态 + 限制申报都在 detail
    assert!(batch
        .source_state
        .detail
        .as_deref()
        .unwrap()
        .contains(&format!("status={ADAPTER_STATUS}")));
}

#[test]
fn incremental_reads_only_new_bytes_and_does_not_double_count() {
    let home = temp_home();
    append_lines(&home, &happy_lines());
    let first = read(&home, &empty_cursor());
    assert_eq!(first.observations.len(), 1);

    // 同水位重读：无新增 → 空批（sourceState 始终返回）
    let again = read(&home, &first.next_cursor);
    assert!(again.observations.is_empty());

    // 追加第二个调用（同 message 第二步）：只读新增
    append_lines(
        &home,
        &[
            row(
                "e-s2",
                "start",
                6000,
                json!({"started": BASE_MS + 6000, "model": "glm-5.3-flash", "provider": "volcengine"}),
            ),
            row(
                "e-d2",
                "delta",
                6200,
                json!({"deltaChars": 5, "hasNonEmptyDelta": true}),
            ),
            row(
                "e-e2",
                "end",
                7000,
                json!({"tokens": 150, "finish": "stop"}),
            ),
        ],
    );
    let second = read(&home, &first.next_cursor);
    assert_eq!(second.observations.len(), 1);
    let o = &second.observations[0];
    assert_eq!(o.stable_call_key, format!("oc:{SK}:{MK}:2")); // 每键独立 stepIndex
    assert_eq!(o.output_tokens, Some(150)); // 累计 usage 不重复入账：各步取自身 end 值
    assert_eq!(o.generation_ms, Some(800));
}

#[test]
fn duplicate_event_rows_within_batch_are_idempotent() {
    let home = temp_home();
    let mut lines = happy_lines();
    lines.push(lines[2].clone()); // delta 行重复投递（同 eventKey）
    append_lines(&home, &lines);
    let batch = read(&home, &empty_cursor());
    assert_eq!(batch.observations.len(), 1); // 仍恰一个观察
    assert!(batch
        .diagnostics
        .iter()
        .any(|d| d.contains("同批重复 eventKey 行")));
}

#[test]
fn identified_retry_excludes_speed_but_keeps_candidate() {
    let home = temp_home();
    append_lines(
        &home,
        &[
            row(
                "r-s",
                "start",
                1000,
                json!({"started": BASE_MS + 1000, "model": "m", "provider": "p"}),
            ),
            row(
                "r-d",
                "delta",
                2000,
                json!({"deltaChars": 1, "hasNonEmptyDelta": true}),
            ),
            row("r-r", "retry", 2500, json!({"attempt": 2})),
            row(
                "r-e",
                "end",
                5000,
                json!({"tokens": 300, "finish": "stop", "attempt": 2}),
            ),
        ],
    );
    let batch = read(&home, &empty_cursor());
    assert_eq!(batch.observations.len(), 1);
    let o = &batch.observations[0];
    assert_eq!(o.outcome, Outcome::Success);
    assert_eq!(o.generation_ms, None); // 识别到重试且不可对齐 → 剔除速度
    assert_eq!(o.output_tokens, Some(300)); // 候选与起止事实保留
    assert!(batch
        .diagnostics
        .iter()
        .any(|d| d.contains("excluded from speed")));
}

#[test]
fn failed_and_cancelled_keep_candidate_without_speed() {
    let home = temp_home();
    append_lines(
        &home,
        &[
            row(
                "f-s",
                "start",
                1000,
                json!({"started": BASE_MS + 1000, "model": "m", "provider": "p"}),
            ),
            row(
                "f-d",
                "delta",
                1500,
                json!({"deltaChars": 2, "hasNonEmptyDelta": true}),
            ),
            row("f-e", "failed", 3000, json!({"finish": "content-filter"})),
        ],
    );
    // 取消 = MessageAbortedError（插件标 cancelled 行）
    let cancel = [
        row(
            "c-s",
            "start",
            4000,
            json!({"started": BASE_MS + 4000, "model": "m", "provider": "p"}),
        ),
        row("c-e", "cancelled", 4500, json!({})),
    ];
    append_lines(&home, &cancel);
    let batch = read(&home, &empty_cursor());
    assert_eq!(batch.observations.len(), 2);
    let failed = batch
        .observations
        .iter()
        .find(|o| o.ended_at_ms == BASE_MS + 3000)
        .unwrap();
    assert_eq!(failed.outcome, Outcome::Failed);
    assert_eq!(failed.generation_ms, None); // 失败不入速度
    assert_eq!(
        failed.failure_reason.as_deref(),
        Some("step.failed:content-filter")
    );
    let cancelled = batch
        .observations
        .iter()
        .find(|o| o.ended_at_ms == BASE_MS + 4500)
        .unwrap();
    assert_eq!(cancelled.outcome, Outcome::Cancelled);
    assert_eq!(cancelled.failure_reason.as_deref(), Some("message-aborted"));
    assert!(!cancelled.is_tps_valid());
}

#[test]
fn interleaved_sessions_do_not_cross_keys() {
    let home = temp_home();
    let other_sk = "b2345678b2345678b2345678b2345678b2345678b2345678b2345678b2345678";
    let other_mk = "e2345678e2345678e2345678e2345678e2345678e2345678e2345678e2345678";
    let mut lines = Vec::new();
    // 两会话交错投递（时间序交叠）
    lines.push(row(
        "x1",
        "start",
        100,
        json!({"started": BASE_MS + 100, "model": "m", "provider": "p"}),
    ));
    let mut l = row(
        "y1",
        "start",
        110,
        json!({"started": BASE_MS + 110, "model": "m", "provider": "p"}),
    );
    l = l.replace(SK, other_sk).replace(MK, other_mk);
    lines.push(l);
    lines.push(row(
        "x2",
        "delta",
        120,
        json!({"deltaChars": 1, "hasNonEmptyDelta": true}),
    ));
    let mut l = row(
        "y2",
        "delta",
        130,
        json!({"deltaChars": 1, "hasNonEmptyDelta": true}),
    );
    l = l.replace(SK, other_sk).replace(MK, other_mk);
    lines.push(l);
    lines.push(row(
        "x3",
        "end",
        1000,
        json!({"tokens": 100, "finish": "stop"}),
    ));
    let mut l = row("y3", "end", 1100, json!({"tokens": 200, "finish": "stop"}));
    l = l.replace(SK, other_sk).replace(MK, other_mk);
    lines.push(l);
    append_lines(&home, &lines);
    let batch = read(&home, &empty_cursor());
    assert_eq!(batch.observations.len(), 2);
    let a = batch
        .observations
        .iter()
        .find(|o| o.session_key.as_deref() == Some(SK))
        .unwrap();
    let b = batch
        .observations
        .iter()
        .find(|o| o.session_key.as_deref() == Some(other_sk))
        .unwrap();
    assert_eq!(a.output_tokens, Some(100));
    assert_eq!(b.output_tokens, Some(200));
    assert!(a.stable_call_key.starts_with(&format!("oc:{SK}:{MK}:")));
    assert!(b
        .stable_call_key
        .starts_with(&format!("oc:{other_sk}:{other_mk}:")));
}

#[test]
fn torn_tail_is_not_crossed_and_completes_next_round() {
    let home = temp_home();
    let lines = happy_lines();
    // 写前三行 + 第四行的**半个字节**（残缺尾行）
    append_lines(&home, &lines[..3]);
    let path = facts_path(home.as_path());
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    let torn = &lines[3][..lines[3].len() / 2];
    write!(f, "{torn}").unwrap();
    drop(f);

    let first = read(&home, &empty_cursor());
    assert!(first.observations.is_empty()); // end 行残缺 → 调用未收窗，不入观察
                                            // 补完残行（模拟宿主继续写）
    use std::io::Seek;
    let mut f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
    f.seek(std::io::SeekFrom::End(0)).unwrap();
    let rest = &lines[3][lines[3].len() / 2..];
    writeln!(f, "{rest}").unwrap();
    drop(f);

    let second = read(&home, &first.next_cursor);
    assert_eq!(second.observations.len(), 1); // 残行重读不丢
    assert_eq!(second.observations[0].output_tokens, Some(300));
}

#[test]
fn shrunk_file_is_reread_from_scratch_idempotently() {
    let home = temp_home();
    append_lines(&home, &happy_lines());
    let first = read(&home, &empty_cursor());
    assert_eq!(first.observations.len(), 1);
    // 插件轮转：文件重置为空（大小 < 水位）
    std::fs::write(facts_path(&home), "").unwrap();
    let second = read(&home, &first.next_cursor);
    assert!(second.observations.is_empty());
    assert!(second
        .diagnostics
        .iter()
        .any(|d| d.contains("收缩") || d.contains("重读")));
    // 轮转后新的同键调用：step 计数续接（游标状态保留），稳定键不与旧行相撞
    append_lines(&home, &happy_lines());
    let third = read(&home, &second.next_cursor);
    assert_eq!(third.observations.len(), 1);
}

#[test]
fn missing_start_or_model_leaves_no_nameless_row() {
    let home = temp_home();
    append_lines(
        &home,
        &[
            // 终行无在飞 start（游标从调用中部进入）
            row("m-e", "end", 1000, json!({"tokens": 10, "finish": "stop"})),
            // start 缺模型 → 终行跳过（不落无名行）
            row("m-s", "start", 2000, json!({"started": BASE_MS + 2000})),
            row(
                "m-d",
                "delta",
                2100,
                json!({"deltaChars": 1, "hasNonEmptyDelta": true}),
            ),
            row("m-e2", "end", 3000, json!({"tokens": 20, "finish": "stop"})),
        ],
    );
    let batch = read(&home, &empty_cursor());
    assert!(batch.observations.is_empty());
    assert!(batch.diagnostics.iter().any(|d| d.contains("无在飞调用")));
    assert!(batch.diagnostics.iter().any(|d| d.contains("无名行")));
}

#[test]
fn missing_facts_file_is_a_normal_empty_state() {
    let home = temp_home();
    let batch = read(&home, &empty_cursor());
    assert!(batch.observations.is_empty());
    assert!(batch.source_state.ok);
    assert_eq!(batch.source_state.cursor, empty_cursor());
}

// ==== 安装包嵌入与所有权（Task 5 机制复用）====

#[test]
fn embedded_bundle_has_loadable_js_manifest_and_license() {
    // 可加载 JS：非空、ESM 默认导出、id/setup 形态、标准 for-await 消费在位
    assert!(OPENCODE_PLUGIN_JS.len() > 1000);
    assert!(OPENCODE_PLUGIN_JS.contains("export default"));
    assert!(OPENCODE_PLUGIN_JS.contains("Symbol.asyncIterator"));
    assert!(OPENCODE_PLUGIN_JS.contains("for await"));
    assert!(OPENCODE_PLUGIN_JS.contains("tuvis-tps-observer"));
    // manifest 可解析 + 零依赖闭包
    let manifest: Value = serde_json::from_str(OPENCODE_PLUGIN_MANIFEST).unwrap();
    assert_eq!(manifest["name"], "tuvis-tps-opencode-observer");
    assert_eq!(manifest["license"], "MIT");
    assert_eq!(manifest["dependencies"], json!({}));
    // 许可：真实 MIT 授权句逐字在位
    assert!(OPENCODE_PLUGIN_LICENSE.contains("Permission is hereby granted, free of charge"));
    // NOTICE：上游 commit 锁定在位
    assert!(OPENCODE_PLUGIN_NOTICE.contains("a6e050d2cbe340a3906e7233cdc880c3199355e4"));
    assert!(OPENCODE_PLUGIN_NOTICE.contains("MIT"));
}

#[test]
fn install_apply_uninstall_lifecycle_on_tempdir() {
    let home = temp_home();
    let target = home
        .join(".config")
        .join("opencode")
        .join("plugins")
        .join("tuvis-tps-observer.ts");
    assert!(!target.exists());

    // preview：新增态、无冲突
    let preview = install::preview_at(&home, "opencode").unwrap();
    assert_eq!(preview.tool, "opencode");
    assert!(preview.conflict_keys.is_empty());
    assert_eq!(preview.entries.len(), 1);
    assert!(!preview.entries[0].old_exists);
    assert!(!target.exists()); // preview 绝不落盘

    // apply（append-only）：写入嵌入字节
    let receipt = install::apply_at(&home, &preview.preview_id, InstallChoice::AppendOnly).unwrap();
    assert_eq!(receipt.tool, "opencode");
    assert!(receipt.receiver.is_none()); // OpenCode 无接收器
    let installed = std::fs::read_to_string(&target).unwrap();
    assert_eq!(installed, OPENCODE_PLUGIN_JS);

    // 幂等重装：同内容 → 无冲突（append-only 可过）
    let preview2 = install::preview_at(&home, "opencode").unwrap();
    assert!(preview2.conflict_keys.is_empty());
    install::apply_at(&home, &preview2.preview_id, InstallChoice::AppendOnly).unwrap();

    // uninstall：仍匹配安装值 → 删除自有文件
    install::uninstall_at(&home, "opencode").unwrap();
    assert!(!target.exists());
    // 再次卸载：幂等成功
    install::uninstall_at(&home, "opencode").unwrap();
}

#[test]
fn conflict_same_name_different_content_defaults_to_preserve() {
    let home = temp_home();
    let target = home
        .join(".config")
        .join("opencode")
        .join("plugins")
        .join("tuvis-tps-observer.ts");
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    std::fs::write(&target, "// 用户自己的同名文件").unwrap();

    let preview = install::preview_at(&home, "opencode").unwrap();
    assert_eq!(preview.conflict_keys, vec!["tuvis-tps-observer.ts"]);
    assert!(preview.entries[0].old_exists);

    // append-only 拒绝（默认保留不覆盖）
    let err = install::apply_at(&home, &preview.preview_id, InstallChoice::AppendOnly).unwrap_err();
    assert!(format!("{err:?}").contains("tps-install-conflict"));
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "// 用户自己的同名文件"
    );

    // transfer：显式替换（旧内容进回执）
    let receipt = install::apply_at(
        &home,
        &preview.preview_id,
        InstallChoice::TransferStatisticsTarget,
    )
    .unwrap();
    assert_eq!(
        receipt.entries[0].old_value,
        Some(json!("// 用户自己的同名文件"))
    );
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        OPENCODE_PLUGIN_JS
    );

    // uninstall：transfer 替换键原样恢复旧内容
    install::uninstall_at(&home, "opencode").unwrap();
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "// 用户自己的同名文件"
    );
}

#[test]
fn apply_rejects_after_preview_drift_and_uninstall_keeps_user_edits() {
    let home = temp_home();

    // 预览后目标被改（预览时已存在 → 指纹漂移）→ apply 拒绝
    let target = home
        .join(".config")
        .join("opencode")
        .join("plugins")
        .join("tuvis-tps-observer.ts");
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    std::fs::write(&target, "// v1").unwrap();
    let preview = install::preview_at(&home, "opencode").unwrap();
    std::fs::write(&target, "// v2 用户改的").unwrap();
    let err = install::apply_at(&home, &preview.preview_id, InstallChoice::AppendOnly).unwrap_err();
    assert!(format!("{err:?}").contains("tps-install-preview-stale"));
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "// v2 用户改的");

    // 安装后用户改动 → uninstall 报冲突并保留（先清理用户文件，走干净安装）
    std::fs::remove_file(&target).unwrap();
    let preview2 = install::preview_at(&home, "opencode").unwrap();
    assert!(preview2.conflict_keys.is_empty());
    install::apply_at(&home, &preview2.preview_id, InstallChoice::AppendOnly).unwrap();
    std::fs::write(&target, "// 用户装完自己改的").unwrap();
    let err = install::uninstall_at(&home, "opencode").unwrap_err();
    assert!(format!("{err:?}").contains("tps-uninstall-conflict"));
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "// 用户装完自己改的"
    );
}

#[test]
fn uninstall_after_restore_is_completion_not_user_conflict() {
    // M4（三轮复审）OpenCode 文件路径同构：文件内容已回到安装前旧值（等价「首次卸载
    // 已恢复但安装记录滞留」= 配置已写而回执未了结的可恢复面）→ 再次卸载不得把已恢复
    // 态误判为用户改动；必须视为卸载完成（文件不动、记录清除、可幂等重复）。
    let home = temp_home();
    let target = home
        .join(".config")
        .join("opencode")
        .join("plugins")
        .join("tuvis-tps-observer.ts");
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    let user_plugin = "// 用户自己的同名插件 X\nexport const X = 1;\n";
    std::fs::write(&target, user_plugin).unwrap();
    let preview = install::preview_at(&home, "opencode").unwrap();
    install::apply_at(
        &home,
        &preview.preview_id,
        InstallChoice::TransferStatisticsTarget,
    )
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        OPENCODE_PLUGIN_JS
    );

    // 文件已恢复为安装前旧内容，而安装记录仍在（回执未了结）
    std::fs::write(&target, user_plugin).unwrap();

    // 再次卸载：已恢复态 = 卸载完成（不得报冲突、不得覆盖文件）
    install::uninstall_at(&home, "opencode").expect("已恢复态必须视为卸载完成，不得误判为用户改动");
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        user_plugin,
        "文件保持恢复后的旧内容（不重复恢复也不删除）"
    );
    let conn = rusqlite::Connection::open(home.join(".tuvis").join("tuvis.db")).unwrap();
    assert!(
        store::load_state(&conn, &install::record_state_key("opencode"))
            .unwrap()
            .map(|v| v.is_null())
            .unwrap_or(true),
        "卸载完成 → 安装记录清除"
    );
    // 幂等：再次卸载仍成功
    install::uninstall_at(&home, "opencode").unwrap();
}

#[test]
fn reinstall_keeps_first_replaced_user_plugin_for_uninstall() {
    // R2（独立复核 P1）：用户原插件被 transfer 替换后，幂等重装不得把备份重置为
    // 「无被替换内容」——否则卸载直接删文件，用户原插件永久丢失。
    let home = temp_home();
    let target = home
        .join(".config")
        .join("opencode")
        .join("plugins")
        .join("tuvis-tps-observer.ts");
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    let user_plugin = "// 用户自己的同名插件 X\nexport const X = 1;\n";
    std::fs::write(&target, user_plugin).unwrap();

    // 首次：transfer 替换（备份 X）
    let p1 = install::preview_at(&home, "opencode").unwrap();
    assert_eq!(p1.conflict_keys, vec!["tuvis-tps-observer.ts"]);
    let r1 = install::apply_at(
        &home,
        &p1.preview_id,
        InstallChoice::TransferStatisticsTarget,
    )
    .unwrap();
    assert_eq!(r1.entries[0].old_value, Some(json!(user_plugin)));

    // 幂等重装：当前内容 == 我们的安装值 → 无冲突，但所有权记录必须沿用首次备份
    let p2 = install::preview_at(&home, "opencode").unwrap();
    assert!(p2.conflict_keys.is_empty());
    let r2 = install::apply_at(&home, &p2.preview_id, InstallChoice::AppendOnly).unwrap();
    assert!(
        r2.entries[0].old_exists,
        "重装回执必须保留「首次替换了用户内容」的事实"
    );
    assert_eq!(
        r2.entries[0].old_value,
        Some(json!(user_plugin)),
        "重装回执 old_value 仍是用户原插件全文"
    );

    // 卸载：原插件逐字恢复，而不是删文件
    install::uninstall_at(&home, "opencode").unwrap();
    assert!(
        target.exists(),
        "首次被替换的用户插件卸载时必须恢复，不能被删除"
    );
    assert_eq!(std::fs::read_to_string(&target).unwrap(), user_plugin);
}
