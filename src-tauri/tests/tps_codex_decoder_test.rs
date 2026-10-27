//! Codex 纯响应分段与原生 ID 去重集成测试（2026-10-26 桌面工作区与输出速度 V2 计划 Task 4）。
//!
//! ## 被测对象
//!
//! [`tps::sources::codex::decoder`]——纯事件状态机：把 Codex rollout JSONL 行解成
//! 「按原生 response_id 键控的含等待响应观察」。无文件 IO、无 DB、无正文持久化。
//!
//! ## 规则锚点（Spec §6.2 + 控制器裁决 + codex-structure-facts.md 真实时序）
//!
//! * 输入边界 = user/developer message、`*_tool_call_output` 工具返回；`turn_context` /
//!   `task_started` 只重置「未生成」态、不是输入边界；
//! * 响应起点 = 该响应首个生成项之前的最后一条输入边界时刻；终点 = 封口前最后一个
//!   生成项时刻；`task_complete` / `item_completed` / `token_count` 永不作起止；
//! * 原生 `token_usage_record`（response_id + 整数 output_tokens）为唯一封口与去重
//!   身份；伴随 `token_count` 只对账不发射；同 response_id 只计一次；
//! * generation_ms / first_token_at_ms / native_duration_ms 恒缺失；method 恒
//!   `log-boundary-approx`；不存在「回合 duration − 回合 TTFT」的任何准入路径。
//!
//! ## 隔离纪律（2026-10-07 事故族，与 tps_native_*_test.rs 同款）
//!
//! 夹具 = 本机真实 rollout 形态的**脱敏重放**（正文/路径/原文 id 全部剥除，时间整体
//! 平移到固定锚点、保留相对间隔），经 `push_record` 直接喂数；不触真机 `~/.codex`、
//! `~/.tuvis`，绝不起 Tauri / Codex / 模型进程。

use multi_agents_manager_lib::services::tps::model::{Outcome, OutputScope, Purpose, TimingMethod};
use multi_agents_manager_lib::services::tps::sources::codex::decoder::{
    push_record, CodexDecoderState,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

/// 夹具固定时间锚点（2026-10-26T00:00:00.000Z；脱敏整体平移的目标原点）
const ANCHOR_MS: i64 = 1_792_972_800_000;

/// 夹具中的合成 response_id（55 字符，与真实同长；唯一/重复关系与真实对齐）
const RID_A: &str = "resp_00000000000000000000000000000000000000000b1fe68e72"; // tools 响应 A；aborted 缺 model 封口
const RID_B: &str = "resp_0000000000000000000000000000000000000000202344b7f4"; // tools 响应 B（被重复封口）
const RID_C: &str = "resp_00000000000000000000000000000000000000003526a2e176"; // tools 响应 C（子代理模型）
const RID_1: &str = "resp_00000000000000000000000000000000000000004a2a010af8"; // normal / duplicates 主响应
const RID_2: &str = "resp_000000000000000000000000000000000000000058ff347390"; // duplicates 重放后新响应
const RID_5: &str = "resp_00000000000000000000000000000000000000007eb26c72bd"; // aborted 唯一应发射者
const RID_R1: &str = "resp_000000000000000000000000000000000000000063a1f2b30d"; // double-reverse 响应 1（倒序封口）
const RID_R2: &str = "resp_000000000000000000000000000000000000000074b2a3c41e"; // double-reverse 响应 2（应保守跳过）
const RID_R3: &str = "resp_000000000000000000000000000000000000000085c3b4d52f"; // double-reverse 响应 3（新鲜边界）
const RID_S1: &str = "resp_a1f3c5e70bd8e9d467d2277194b9cb8ef85232be7412928833"; // triple-seal 响应 1（唯一合法窗）
const RID_S2: &str = "resp_b2c4d6f80999aa44ff967a9146991bf2fe1ca1573278312319"; // triple-seal 响应 2（陈旧起点，应跳过）
const RID_S3: &str = "resp_c3d5e7f90a7c886d104b75f46f9cc0709cc09356157fd0f1d7"; // triple-seal 响应 3（陈旧起点，应跳过）

const HOST: &str = "host-test";

fn sha256_hex(s: &str) -> String {
    let d = Sha256::digest(s.as_bytes());
    d.iter().map(|b| format!("{b:02x}")).collect()
}

/// 把 JSONL 文本逐行喂进解码器；返回（观察, 诊断）
fn feed(
    text: &str,
) -> (
    Vec<multi_agents_manager_lib::services::tps::model::TpsObservation>,
    Vec<String>,
) {
    let mut state = CodexDecoderState::default();
    let mut observations = Vec::new();
    let mut diagnostics = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let row: Value = serde_json::from_str(line).expect("fixture line must be valid JSON");
        let result = push_record(&mut state, &row, HOST);
        observations.extend(result.observations);
        diagnostics.extend(result.diagnostics);
    }
    (observations, diagnostics)
}

/// Task 4 验收主断言：输入@0 生成结束@5000 原生 usage 100 → 响应 20 Token/s
#[test]
fn normal_response_window_and_rate() {
    let (obs, diags) = feed(include_str!("fixtures/tps-v2/codex-normal.jsonl"));
    assert_eq!(obs.len(), 1, "normal 夹具恰一发一观察；diags={diags:?}");
    let normal = &obs[0];
    assert_eq!(normal.response_elapsed_ms, Some(5000));
    assert_eq!(
        (normal.generation_ms, normal.first_token_at_ms),
        (None, None)
    );
    // 响应口径速度（Task 2 同式）：1000 × 100 输出 / 5000 ms = 20 Token/s（含等待 · 日志近似）
    let response_tps = 1000.0 * normal.output_tokens.expect("output") as f64
        / normal.response_elapsed_ms.expect("elapsed") as f64;
    assert!(
        (response_tps - 20.0).abs() < 1e-9,
        "response rate = {response_tps}"
    );
    // 身份与归属
    assert_eq!(normal.source, "codex");
    assert_eq!(normal.host_instance, HOST);
    assert_eq!(
        normal.stable_call_key,
        sha256_hex(RID_1),
        "稳定键 = 完整 response_id 摘要"
    );
    assert_eq!(
        normal.started_at_ms,
        ANCHOR_MS + 300,
        "起点 = 首生成项前最后输入边界（user@+300）"
    );
    assert_eq!(
        normal.ended_at_ms,
        ANCHOR_MS + 5300,
        "终点 = 最后生成项（custom_tool_call@+5300）"
    );
    assert_eq!(normal.model, "gpt-5.6-luna", "模型取流内最近 turn_context");
    assert_eq!(
        normal.channel.as_deref(),
        Some("openai"),
        "渠道取 session_meta.model_provider"
    );
    assert_eq!(normal.output_tokens, Some(100));
    assert_eq!(normal.output_scope, OutputScope::IncludesReasoning);
    assert_eq!(normal.method, TimingMethod::LogBoundaryApprox);
    assert_eq!(normal.outcome, Outcome::Success);
    assert_eq!(normal.purpose, Purpose::MainTask);
    assert_eq!(normal.failure_reason, None);
    assert_eq!(normal.tool_ms, None);
    // 会话/回合身份同为摘要（不含原值）
    assert_eq!(normal.session_key.as_deref().map(str::len), Some(64));
    assert_eq!(normal.turn_key.as_deref().map(str::len), Some(64));
    assert_ne!(
        normal.session_key.as_deref(),
        Some("00000000-1111-2222-3333-444444444444")
    );
}

/// 工具执行（封口后 → 工具返回）不得进入任何响应分母；下一响应起点 = 组内最后一条匹配工具返回；
/// 重复封口插在工具返回之后不得移动该起点；多工具/子代理第二个 turn_context 的模型归属
#[test]
fn tool_execution_excluded_and_next_start_after_last_matching_return() {
    let (obs, diags) = feed(include_str!("fixtures/tps-v2/codex-tools.jsonl"));
    assert_eq!(
        obs.len(),
        3,
        "tools 夹具恰发 3 观察（A/B/C）；diags={diags:?}"
    );
    let (a, b, c) = (&obs[0], &obs[1], &obs[2]);

    // A：+200 → +1200 = 1000ms；封口后的工具执行（1300..6200）绝不在窗内
    assert_eq!(a.stable_call_key, sha256_hex(RID_A));
    assert_eq!(a.response_elapsed_ms, Some(1000));
    assert_eq!(
        (a.started_at_ms, a.ended_at_ms),
        (ANCHOR_MS + 200, ANCHOR_MS + 1200)
    );

    // B：多工具响应，起点 = A 的匹配工具返回（+6200），终点 = 最后生成项（+8500）= 2300ms
    assert_eq!(b.stable_call_key, sha256_hex(RID_B));
    assert_eq!(b.response_elapsed_ms, Some(2300));
    assert_eq!(
        (b.started_at_ms, b.ended_at_ms),
        (ANCHOR_MS + 6200, ANCHOR_MS + 8500)
    );

    // C：起点 = B 匹配组内**最后**一条工具返回（+15000）；+15050 的重复封口不得移动它
    assert_eq!(c.stable_call_key, sha256_hex(RID_C));
    assert_eq!(c.started_at_ms, ANCHOR_MS + 15000);
    assert_eq!(c.response_elapsed_ms, Some(1800));
    assert_eq!(c.ended_at_ms, ANCHOR_MS + 16800);

    // 子代理/第二个 turn_context：C 的模型归属 = 流内最近一次（gpt-6.1-sol），A/B 不受牵连
    assert_eq!(a.model, "gpt-5.6-luna");
    assert_eq!(b.model, "gpt-5.6-luna");
    assert_eq!(c.model, "gpt-6.1-sol");

    // 重复封口进了诊断面
    assert!(
        diags.iter().any(|d| d.contains("duplicate-response-id")),
        "重复封口应有诊断；diags={diags:?}"
    );
}

/// 复制历史（同 response_id 两次）只计一次；伴随 token_count 永不发射、只对账（不符出诊断）
#[test]
fn companion_token_count_never_emits_and_copied_ids_collapse() {
    let (obs, diags) = feed(include_str!("fixtures/tps-v2/codex-duplicates.jsonl"));
    assert_eq!(obs.len(), 2, "只应有 R1/R2 两个观察；diags={diags:?}");
    let deduplicated: Vec<_> = obs
        .iter()
        .filter(|o| o.stable_call_key == sha256_hex(RID_1))
        .collect();
    assert_eq!(deduplicated.len(), 1);

    // 重放后的新响应照常成窗（起点 = 重放边界 +5500，复制残留生成项已被歧义丢弃）
    let second = obs
        .iter()
        .find(|o| o.stable_call_key == sha256_hex(RID_2))
        .expect("R2");
    assert_eq!(second.response_elapsed_ms, Some(5100));
    assert_eq!(second.started_at_ms, ANCHOR_MS + 5500);

    // 诊断面：重复封口 + token_count 对账不符（last_out=51 vs 封口 50）
    assert!(
        diags.iter().any(|d| d.contains("duplicate-response-id")),
        "diags={diags:?}"
    );
    assert!(
        diags
            .iter()
            .any(|d| d.contains("token-count-mismatch") && d.contains("reconcile-only")),
        "diags={diags:?}"
    );
}

/// 取消/完成前未封口、错配 response_id、封口前生成项间插入工具返回 ⇒ 一律不凑窗；
/// 唯一可发射者是歧义丢弃后从工具返回重新起算的 R5
#[test]
fn aborted_and_malformed_records_yield_no_observations() {
    let (obs, diags) = feed(include_str!("fixtures/tps-v2/codex-aborted.jsonl"));
    assert_eq!(obs.len(), 1, "只有 R5 可发射；diags={diags:?}");
    let only = &obs[0];
    assert_eq!(only.stable_call_key, sha256_hex(RID_5));
    assert_eq!(
        only.started_at_ms,
        ANCHOR_MS + 14500,
        "歧义丢弃后从工具返回重新起算"
    );
    assert_eq!(only.response_elapsed_ms, Some(5100));
    assert_eq!(only.output_tokens, Some(77));

    // 各跳过路径都有诊断（ASCII 机器 token）：缺 model / 非字符串 response_id /
    // 非整数 output_tokens / turn_aborted 丢弃未决封口 / task_complete 丢弃未封口 /
    // 生成项间插入工具返回的歧义丢弃
    let joined = diags.join("\n");
    for reason in [
        "missing-model skipped",
        "response-id-not-string skipped",
        "output-tokens-not-integer skipped",
        "turn_aborted open-response-dropped",
        "task_complete open-response-dropped",
        "generation-before-seal ambiguous open-response-dropped",
    ] {
        assert!(
            joined.contains(reason),
            "诊断缺「{reason}」；diags={joined:?}"
        );
    }
}

/// 不存在「task_complete.duration − 回合 TTFT」准入路径：只剩回合墙钟计时的回合 ⇒ 零观察
#[test]
fn task_complete_never_derives_a_window() {
    let t = "turn-only-complete";
    let rows = vec![
        json!({"timestamp": "2026-10-26T00:00:00.000Z", "type": "session_meta",
               "payload": {"model_provider": "openai", "session_id": "s1"}}),
        json!({"timestamp": "2026-10-26T00:00:00.100Z", "type": "event_msg",
               "payload": {"type": "task_started", "turn_id": t, "root_turn_id": t}}),
        json!({"timestamp": "2026-10-26T00:00:00.200Z", "type": "response_item",
               "payload": {"type": "message", "role": "user"}}),
        json!({"timestamp": "2026-10-26T00:00:00.300Z", "type": "turn_context",
               "payload": {"model": "gpt-5.6-luna", "turn_id": t}}),
        json!({"timestamp": "2026-10-26T00:00:01.000Z", "type": "response_item",
               "payload": {"type": "reasoning"}}),
        json!({"timestamp": "2026-10-26T00:00:05.000Z", "type": "response_item",
               "payload": {"type": "message", "role": "assistant"}}),
        // 回合墙钟：duration 10000 / TTFT 500 —— 即便直接给出，也不得拼出任何响应窗
        json!({"timestamp": "2026-10-26T00:00:10.000Z", "type": "event_msg",
               "payload": {"type": "task_complete", "turn_id": t, "duration_ms": 10000,
                           "time_to_first_token_ms": 500}}),
    ];
    let mut state = CodexDecoderState::default();
    let mut observations = Vec::new();
    for row in &rows {
        observations.extend(push_record(&mut state, row, HOST).observations);
    }
    assert!(
        observations.is_empty(),
        "无封口 ⇒ 无观察，绝不从回合墙钟凑窗"
    );
}

/// 延迟封口的未决状态跨 serde 往返：封口先到（零生成项）→ 序列化 → 反序列化 → 生成项+下一封口
/// 到达后照常提交，与一次性喂完等价；序列化产物不含原 response_id
#[test]
fn delayed_seal_pending_state_round_trips_through_serde() {
    let t = "turn-pending";
    let part1 = vec![
        json!({"timestamp": "2026-10-26T00:00:00.000Z", "type": "session_meta",
               "payload": {"model_provider": "openai", "session_id": "s-pending"}}),
        json!({"timestamp": "2026-10-26T00:00:00.100Z", "type": "event_msg",
               "payload": {"type": "task_started", "turn_id": t, "root_turn_id": t}}),
        json!({"timestamp": "2026-10-26T00:00:00.300Z", "type": "response_item",
               "payload": {"type": "message", "role": "user"}}),
        json!({"timestamp": "2026-10-26T00:00:00.400Z", "type": "turn_context",
               "payload": {"model": "gpt-5.6-luna", "turn_id": t}}),
        // 封口先到（零生成项）→ 保存未决
        json!({"timestamp": "2026-10-26T00:00:00.500Z", "type": "token_usage_record",
               "payload": {"response_id": RID_1, "session_id": "s-pending", "turn_id": t,
                           "usage": {"output_tokens": 100, "reasoning_output_tokens": 30}}}),
    ];
    let part2 = vec![
        json!({"timestamp": "2026-10-26T00:00:01.000Z", "type": "response_item",
               "payload": {"type": "reasoning"}}),
        json!({"timestamp": "2026-10-26T00:00:06.000Z", "type": "response_item",
               "payload": {"type": "message", "role": "assistant"}}),
        // 下一封口触发未决提交（end = 封口后最后生成项 +6000 → elapsed 5700）
        json!({"timestamp": "2026-10-26T00:00:06.100Z", "type": "token_usage_record",
               "payload": {"response_id": RID_2, "session_id": "s-pending", "turn_id": t,
                           "usage": {"output_tokens": 7, "reasoning_output_tokens": 2}}}),
    ];

    // 一次性喂完的基准结果
    let mut all = part1.clone();
    all.extend(part2.clone());
    let mut baseline_state = CodexDecoderState::default();
    let mut baseline = Vec::new();
    for row in &all {
        baseline.extend(push_record(&mut baseline_state, row, HOST).observations);
    }

    // 分两段、中间 serde 往返
    let mut state = CodexDecoderState::default();
    let mut observations = Vec::new();
    for row in &part1 {
        observations.extend(push_record(&mut state, row, HOST).observations);
    }
    assert!(observations.is_empty(), "未决封口不立即发射");
    let saved = serde_json::to_string(&state).expect("state must serialize");
    assert!(
        !saved.contains(RID_1),
        "序列化状态只含摘要，绝不含原 response_id：{saved}"
    );
    assert!(
        !saved.contains("s-pending"),
        "序列化状态只含会话摘要，绝不含原 session id"
    );
    let mut state: CodexDecoderState =
        serde_json::from_str(&saved).expect("state must deserialize");
    for row in &part2 {
        observations.extend(push_record(&mut state, row, HOST).observations);
    }

    assert_eq!(observations.len(), 1, "延迟封口在生成项出现后照常提交");
    assert_eq!(observations, baseline, "serde 往返后与一次性喂完等价");
    assert_eq!(observations[0].stable_call_key, sha256_hex(RID_1));
    assert_eq!(observations[0].started_at_ms, ANCHOR_MS + 300);
    assert_eq!(observations[0].ended_at_ms, ANCHOR_MS + 6000);
    assert_eq!(observations[0].response_elapsed_ms, Some(5700));
    assert_eq!(observations[0].output_tokens, Some(100));
}

/// 双倒序封口（终审裁决 Important 2 回归，2026-10-26）：封口₁ 倒序先于其生成项到达
/// （未决）→ 生成项₁（用封口前的边界配对，R1 正常结算）→ 封口₂ 结算 R1 并转为新未决 →
/// 生成项₂ 在**无新鲜输入边界**时到达 → 工具返回₂ 关闭未决₂。响应₂ 绝不复用封口前的
/// 旧边界（否则其窗 = 输入₀ → 生成项₂，包含 R1 的全部生成段+间隔：时长虚高、速度
/// 低估）——以 `missing-input-boundary` 保守跳过；随后的新鲜边界（工具返回₂）正常起算
/// 响应₃。两条已发射响应窗不重叠（resp.start ≥ 前一响应 end）。
#[test]
fn double_reverse_seal_never_reuses_pre_seal_boundary() {
    let (obs, diags) = feed(include_str!(
        "fixtures/tps-v2/codex-double-reverse-seal.jsonl"
    ));
    assert_eq!(obs.len(), 2, "R1/R3 发射，R2 保守跳过；diags={diags:?}");

    // R1：倒序配对不受「提交后清边界」影响（生成项₁ 用封口前冻结的 T0 结算）
    let r1 = obs
        .iter()
        .find(|o| o.stable_call_key == sha256_hex(RID_R1))
        .expect("R1");
    assert_eq!(
        (r1.started_at_ms, r1.ended_at_ms),
        (ANCHOR_MS + 200, ANCHOR_MS + 1100),
        "R1 = 输入₀(+200) → 生成项₁(+1100)"
    );
    assert_eq!(r1.response_elapsed_ms, Some(900));
    assert_eq!(r1.output_tokens, Some(100));

    // R3：从新鲜工具返回边界(+2000)重新起算，窗不与 R1 重叠
    let r3 = obs
        .iter()
        .find(|o| o.stable_call_key == sha256_hex(RID_R3))
        .expect("R3");
    assert_eq!(
        (r3.started_at_ms, r3.ended_at_ms),
        (ANCHOR_MS + 2000, ANCHOR_MS + 2100),
        "R3 = 工具返回₂(+2000) → 生成项₃(+2100)"
    );
    assert_eq!(r3.response_elapsed_ms, Some(100));
    assert_eq!(r3.output_tokens, Some(30));
    assert!(
        r3.started_at_ms >= r1.ended_at_ms,
        "两条响应窗不得重叠（resp₂.start ≥ resp₁.end）"
    );

    // R2 被保守跳过（身份未消费，不入已发射集合）且带保守跳过诊断
    assert!(
        !obs.iter().any(|o| o.stable_call_key == sha256_hex(RID_R2)),
        "响应₂ 不得复用封口前的旧边界凑窗"
    );
    assert!(
        diags.iter().any(|d| d.contains("missing-input-boundary")),
        "响应₂ 应以 missing-input-boundary 保守跳过；diags={diags:?}"
    );
}

/// 三连封口无边界行（二轮复审 B2 回归）：封口₁→生成₁→封口₂→生成₂→封口₃，中间
/// 无任何输入/工具返回行。封口₂ 关闭未决₁ 正常发射 R1 [+200,+500]（唯一合法窗）；
/// 未决₂ 捕获的 `start_ts` 快照（+200）是 R1 已消费的旧边界——`.or(生成项冻结起点)`
/// 在此救不回（生成项₂ 冻结的起点也是 None，提交后被清空的边界），旧代码会发射
/// R2 [+200,+700]，与 R1 完全重叠（时长虚高、速度低估）。现由 `last_committed_end`
/// 单调护栏以 `stale-start-after-commit` 保守跳过：每条发射窗起点 ≥ 前一条终点。
#[test]
fn triple_seal_without_boundary_rows_never_emits_overlapping_windows() {
    let (obs, diags) = feed(include_str!("fixtures/tps-v2/codex-triple-seal.jsonl"));
    assert_eq!(obs.len(), 1, "只有 R1 可发射；diags={diags:?}");

    let r1 = &obs[0];
    assert_eq!(r1.stable_call_key, sha256_hex(RID_S1));
    assert_eq!(
        (r1.started_at_ms, r1.ended_at_ms),
        (ANCHOR_MS + 200, ANCHOR_MS + 500),
        "R1 = 输入(+200) → 生成₁(+500)"
    );
    assert_eq!(r1.response_elapsed_ms, Some(300));
    assert_eq!(r1.output_tokens, Some(100));

    // R2/S2、R3/S3 的身份未被消费（保守跳过不入已发射集合）
    assert!(
        !obs.iter().any(|o| o.stable_call_key == sha256_hex(RID_S2)),
        "响应₂ 不得用陈旧起点快照发射重叠窗"
    );
    assert!(!obs.iter().any(|o| o.stable_call_key == sha256_hex(RID_S3)));

    // 陈旧起点带 ASCII 诊断，数值可核（起点 = 旧边界 +200，旧终点 = R1 终点 +500）
    let stale = diags
        .iter()
        .find(|d| d.contains("stale-start-after-commit"))
        .expect("陈旧起点必须有诊断");
    assert!(
        stale.contains(&format!("start_ms={}", ANCHOR_MS + 200))
            && stale.contains(&format!("last_end_ms={}", ANCHOR_MS + 500)),
        "诊断应带 start_ms/last_end_ms：{stale}"
    );
}

/// 单调护栏跨批次持久（二轮复审 B2）：R1 提交后 serde 往返（= 采集轮间游标整体重写），
/// 续喂的陈旧起点仍被拒绝；旧游标 JSON 缺 `last_committed_end` 字段时反序列化为 None
/// （`#[serde(default)]`）——存量游标零迁移可加载。
#[test]
fn monotonic_guard_survives_serde_round_trip() {
    let full: Vec<Value> = include_str!("fixtures/tps-v2/codex-triple-seal.jsonl")
        .lines()
        .map(|l| serde_json::from_str(l).expect("fixture line must be valid JSON"))
        .collect();
    // 第一批：到封口₂ 为止（R1 已发射，护栏 = R1 终点）
    let split = full
        .iter()
        .position(|r| r["ordinal"] == 6)
        .map(|p| p + 1)
        .expect("封口₂ 行");
    let mut state = CodexDecoderState::default();
    let mut observations = Vec::new();
    for row in &full[..split] {
        observations.extend(push_record(&mut state, row, HOST).observations);
    }
    assert_eq!(observations.len(), 1, "第一批发射 R1");

    // serde 往返（字段保留，= 采集轮间游标重写的真实形态）后，陈旧起点仍被拒绝
    let saved = serde_json::to_value(&state).expect("state must serialize");
    let mut state: CodexDecoderState =
        serde_json::from_value(saved.clone()).expect("state must deserialize");

    // 第二批：生成₂ + 封口₃ —— 陈旧起点仍被拒绝（护栏随状态持久）
    for row in &full[split..] {
        observations.extend(push_record(&mut state, row, HOST).observations);
    }
    assert_eq!(
        observations.len(),
        1,
        "护栏跨批次持久：生成₂/封口₃ 不得再发射重叠窗"
    );
    assert_eq!(observations[0].stable_call_key, sha256_hex(RID_S1));

    // 存量游标兼容：缺 last_committed_end 字段的旧 JSON 可加载（None，零迁移）
    let mut legacy = saved;
    legacy
        .as_object_mut()
        .expect("state is an object")
        .remove("last_committed_end");
    let _: CodexDecoderState =
        serde_json::from_value(legacy).expect("旧游标缺新字段必须可加载（serde default）");
}

/// 汇总纪律：四个夹具的**每一条**观察都是「仅响应、日志近似」形态
#[test]
fn every_codex_observation_is_response_only_log_approx() {
    for (name, text) in [
        ("normal", include_str!("fixtures/tps-v2/codex-normal.jsonl")),
        ("tools", include_str!("fixtures/tps-v2/codex-tools.jsonl")),
        (
            "duplicates",
            include_str!("fixtures/tps-v2/codex-duplicates.jsonl"),
        ),
        (
            "aborted",
            include_str!("fixtures/tps-v2/codex-aborted.jsonl"),
        ),
    ] {
        let (obs, _) = feed(text);
        assert!(!obs.is_empty(), "{name}: 至少一条观察");
        for o in &obs {
            assert_eq!(o.source, "codex", "{name}");
            assert_eq!(o.method, TimingMethod::LogBoundaryApprox, "{name}");
            assert_eq!(o.generation_ms, None, "{name}: 无生成窗概念，不得伪造");
            assert_eq!(o.first_token_at_ms, None, "{name}: 无首点事实，不得推造");
            assert_eq!(o.native_duration_ms, None, "{name}: 不得伪装原生测量");
            assert!(
                o.response_elapsed_ms.is_some_and(|v| v > 0),
                "{name}: 响应窗必须为正"
            );
            assert!(o.started_at_ms < o.ended_at_ms, "{name}");
            assert_eq!(o.outcome, Outcome::Success, "{name}");
            assert_eq!(o.tool_ms, None, "{name}");
            assert_eq!(o.failure_reason, None, "{name}");
            assert!(o.output_tokens.is_some(), "{name}");
            assert_eq!(o.stable_call_key.len(), 64, "{name}: 稳定键 = SHA-256 hex");
        }
    }
}

/// 无可解析 timestamp 的行：诊断并整行忽略，绝不因此重置状态（边界/累积原样保留）
#[test]
fn rows_without_parseable_timestamp_are_diagnosed_and_ignored() {
    let t = "turn-badts";
    let rows = vec![
        json!({"timestamp": "2026-10-26T00:00:00.000Z", "type": "session_meta",
               "payload": {"model_provider": "openai", "session_id": "s-badts"}}),
        json!({"timestamp": "2026-10-26T00:00:00.100Z", "type": "event_msg",
               "payload": {"type": "task_started", "turn_id": t, "root_turn_id": t}}),
        json!({"timestamp": "2026-10-26T00:00:00.300Z", "type": "response_item",
               "payload": {"type": "message", "role": "user"}}),
        // 缺 timestamp：整行忽略，边界不得被重置
        json!({"type": "response_item", "payload": {"type": "message", "role": "user"}}),
        // 坏 timestamp：同样忽略
        json!({"timestamp": "not-a-time", "type": "response_item",
               "payload": {"type": "message", "role": "user"}}),
        json!({"timestamp": "2026-10-26T00:00:00.400Z", "type": "turn_context",
               "payload": {"model": "gpt-5.6-luna", "turn_id": t}}),
        json!({"timestamp": "2026-10-26T00:00:05.300Z", "type": "response_item",
               "payload": {"type": "reasoning"}}),
        json!({"timestamp": "2026-10-26T00:00:05.300Z", "type": "response_item",
               "payload": {"type": "message", "role": "assistant"}}),
        json!({"timestamp": "2026-10-26T00:00:05.400Z", "type": "token_usage_record",
               "payload": {"response_id": RID_1, "session_id": "s-badts", "turn_id": t,
                           "usage": {"output_tokens": 50, "reasoning_output_tokens": 10}}}),
    ];
    let mut state = CodexDecoderState::default();
    let mut observations = Vec::new();
    let mut diagnostics = Vec::new();
    for row in &rows {
        let result = push_record(&mut state, row, HOST);
        observations.extend(result.observations);
        diagnostics.extend(result.diagnostics);
    }
    assert_eq!(observations.len(), 1);
    // 起点仍 = 有 timestamp 的那条 user 边界（+300）——坏行没有把它抹掉
    assert_eq!(observations[0].started_at_ms, ANCHOR_MS + 300);
    assert_eq!(observations[0].response_elapsed_ms, Some(5000));
    assert_eq!(
        diagnostics
            .iter()
            .filter(|d| d.contains("timestamp"))
            .count(),
        2,
        "diags={diagnostics:?}"
    );
}
