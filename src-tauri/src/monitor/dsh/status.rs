// dsh 三色状态判定（设计 P2 定死映射）：
// 优先级 error > approval > running > done（foxbell 生产语义复刻，M0 §2）
// interrupted 判定 = "写入者是否存活"（lock 交叉判定，评审升级 #2），与任务时长无关

use crate::monitor::dsh::log::DshEvent;
use crate::session::SessionStatus;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LockState {
    /// 某个 dsh 进程持有该会话写租约（真运行）
    Held,
    /// 锁文件存在但无人持有（写入者已死 → 中断）
    Free,
    /// 无锁文件（v0 旧目录 / 探测失败）→ mtime 静默兜底
    Unknown,
}

/// v0 无锁目录的静默中断阈值（30 分钟）。**刻意不迁入 app_status 共享常量**：
/// dsh 用持有式 fd 租约判定在场，静默阈值只服务"无锁降级"这一独有路径，
/// 与其他工具的停更降级无共性（见 spec 2026-11-04 §7.3 收敛原则）。
pub const V0_SILENCE_INTERRUPT_MS: i64 = 30 * 60 * 1000;

pub struct StatusInput<'a> {
    pub events: &'a [DshEvent],
    pub lock: LockState,
    /// 日志 mtime 距今毫秒（仅 lock=Unknown 时消费）
    pub silence_ms: Option<i64>,
}

pub struct StatusOutcome {
    pub status: SessionStatus,
    /// 最近 turn/end 的 reason.kind（诊断/审计用）
    pub end_kind: Option<String>,
    /// 最近 turn/end 的 seq（水位/未读判定用）
    pub last_end_seq: Option<i64>,
}

/// 事件流的纯内容扫描产物（session_scan.rs L2 预算层："纯内容产物进缓存 +
/// 时间叠加现算"）——不含任何 lock / 时间输入，可安全跨轮询缓存
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TurnFacts {
    /// 最近 turn/start 的 seq
    pub(crate) last_start: Option<i64>,
    /// 最近 turn/end 的 (seq, reason.kind)
    pub(crate) last_end: Option<(i64, String)>,
    /// 未决审批 id 集合（asked 未 decided）
    pub(crate) open_approvals: std::collections::HashSet<String>,
}

impl TurnFacts {
    /// 回合是否打开（mod.rs 据此决定是否做 lsof 锁探测——闭合回合不探测）
    pub fn has_open_turn(&self) -> bool {
        match (self.last_start, &self.last_end) {
            (Some(s), Some((e, _))) => s > *e,
            (Some(_), None) => true,
            _ => false,
        }
    }
}

/// 纯内容扫描：从事件流提取回合事实（每文件只在内容变化时执行一次）
pub fn scan_facts(events: &[DshEvent]) -> TurnFacts {
    let mut facts = TurnFacts::default();
    for e in events {
        match e.kind.as_str() {
            "turn/start" => facts.last_start = e.seq.or(facts.last_start),
            "turn/end" => {
                if let Some(seq) = e.seq {
                    let kind = e
                        .data
                        .pointer("/reason/kind")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    // 只保留最新（seq 单调）
                    if facts
                        .last_end
                        .as_ref()
                        .map(|(s, _)| seq >= *s)
                        .unwrap_or(true)
                    {
                        facts.last_end = Some((seq, kind));
                    }
                }
            }
            "approval/asked" => {
                if let Some(id) = e.data.get("id").and_then(|v| v.as_str()) {
                    facts.open_approvals.insert(id.to_string());
                }
            }
            "approval/decided" => {
                if let Some(id) = e.data.get("id").and_then(|v| v.as_str()) {
                    facts.open_approvals.remove(id);
                }
            }
            _ => {}
        }
    }
    facts
}

/// 时间叠加：回合事实 + 本轮 lock 探测 / 静默时长 → 三色判定（每轮现算，代价 O(1)）
pub fn derive_facts(facts: &TurnFacts, lock: LockState, silence_ms: Option<i64>) -> StatusOutcome {
    let open_turn = facts.has_open_turn();

    let (status, end_kind) = if open_turn {
        if !facts.open_approvals.is_empty() {
            (SessionStatus::Waiting, None) // 红 · 等待批准
        } else {
            match lock {
                LockState::Held => (SessionStatus::Processing, None),
                LockState::Free => (SessionStatus::Waiting, Some("interrupted".into())), // 写入者已死
                LockState::Unknown => {
                    let silence = silence_ms.unwrap_or(0);
                    if silence > V0_SILENCE_INTERRUPT_MS {
                        (SessionStatus::Waiting, Some("interrupted".into()))
                    } else {
                        (SessionStatus::Processing, None)
                    }
                }
            }
        }
    } else {
        let kind = facts.last_end.as_ref().map(|(_, k)| k.clone());
        let st = match kind.as_deref() {
            Some("error") | Some("interrupted") => SessionStatus::Waiting, // 红
            Some("blocked") => SessionStatus::Processing,                  // 黄 · 等待
            Some("max-tokens") | Some("completed") => SessionStatus::Finished, // 绿
            Some("aborted") => SessionStatus::Idle,                        // 用户取消不点红
            _ => SessionStatus::Idle,
        };
        (st, kind)
    };

    StatusOutcome {
        status,
        end_kind,
        last_end_seq: facts.last_end.as_ref().map(|(s, _)| *s),
    }
}

pub fn derive(input: &StatusInput) -> StatusOutcome {
    derive_facts(&scan_facts(input.events), input.lock, input.silence_ms)
}

// 测试：黄金夹具判定 + P2 映射表 + lock 交叉判定分支
#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitor::dsh::log::DshEvent;
    use crate::session::SessionStatus;
    use serde_json::json;

    fn fixture_events(name: &str) -> Vec<DshEvent> {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/dsh")
            .join(name);
        crate::monitor::dsh::log::parse_events(&std::fs::read_to_string(&p).unwrap())
    }

    fn ev(kind: &str, seq: i64, data: serde_json::Value) -> DshEvent {
        DshEvent {
            kind: kind.into(),
            seq: Some(seq),
            time: None,
            data,
        }
    }

    fn input(events: &[DshEvent], lock: LockState) -> StatusInput<'_> {
        StatusInput {
            events,
            lock,
            silence_ms: None,
        }
    }

    #[test]
    fn golden_completed_is_finished() {
        let e = fixture_events("sample1-completed.sanitized.jsonl");
        let out = derive(&input(&e, LockState::Held));
        assert_eq!(out.status, SessionStatus::Finished);
        assert_eq!(out.end_kind.as_deref(), Some("completed"));
    }

    #[test]
    fn golden_tool_error_session_completes_finished() {
        let e = fixture_events("sample2-tool-error.sanitized.jsonl");
        // 探测报告 line 55：sample2 = 工具错误后 turn 正常 completed；工具错误不改 turn
        // 结束 kind，不亮红（P2：红·错误仅指 reason.kind=="error"）。本测试守护：夹具含
        // tool-result 错误文本不得误翻状态
        assert_eq!(
            derive(&input(&e, LockState::Held)).status,
            SessionStatus::Finished
        );
        assert_eq!(
            derive(&input(&e, LockState::Held)).end_kind.as_deref(),
            Some("completed")
        );
    }

    #[test]
    fn golden_approval_pending_is_waiting() {
        let e = fixture_events("sample5-approval-pending.sanitized.jsonl");
        // turn 打开 + 审批未决 → 红（优先于"运行中"）
        assert_eq!(
            derive(&input(&e, LockState::Held)).status,
            SessionStatus::Waiting
        );
    }

    #[test]
    fn golden_approval_decided_recovers_finished() {
        // T5-1 黄金夹具（sample3，此前零消费）：asked(seq19)→decided(seq20) 同 id，
        // 决议必须清除开放审批集；样本随后 turn/end(seq26) 以 completed 收尾——
        // 最终绿（Finished），不得因"曾有审批"滞留红（Waiting）误判
        let e = fixture_events("sample3-approval-decided.sanitized.jsonl");
        let out = derive(&input(&e, LockState::Held));
        assert_eq!(out.status, SessionStatus::Finished);
        assert_eq!(out.end_kind.as_deref(), Some("completed"));
    }

    #[test]
    fn golden_running_is_processing() {
        let e = fixture_events("sample4-running.sanitized.jsonl");
        assert_eq!(
            derive(&input(&e, LockState::Held)).status,
            SessionStatus::Processing
        );
    }

    #[test]
    fn open_turn_with_free_lock_is_interrupted_waiting() {
        // 评审升级 #2：turn 打开 + 写入者已死（锁无人持有）→ 红·中断
        let e = fixture_events("sample4-running.sanitized.jsonl");
        assert_eq!(
            derive(&input(&e, LockState::Free)).status,
            SessionStatus::Waiting
        );
    }

    #[test]
    fn open_turn_unknown_lock_uses_silence() {
        let e = fixture_events("sample4-running.sanitized.jsonl");
        // 静默 10 分钟 → 仍黄（长任务不误判）
        let short = StatusInput {
            events: &e,
            lock: LockState::Unknown,
            silence_ms: Some(10 * 60 * 1000),
        };
        assert_eq!(derive(&short).status, SessionStatus::Processing);
        // 静默 31 分钟 → 红·中断（v0 兜底阈值 30min）
        let long = StatusInput {
            events: &e,
            lock: LockState::Unknown,
            silence_ms: Some(31 * 60 * 1000),
        };
        assert_eq!(derive(&long).status, SessionStatus::Waiting);
    }

    #[test]
    fn end_kind_mapping_table() {
        let base = |kind: &str| {
            vec![
                ev("turn/start", 4, json!({})),
                ev("turn/end", 6, json!({ "reason": { "kind": kind } })),
            ]
        };
        // 设计 P2 定死映射：aborted→空闲（不点红）；blocked→黄；max-tokens→绿；interrupted→红
        assert_eq!(
            derive(&input(&base("aborted"), LockState::Held)).status,
            SessionStatus::Idle
        );
        assert_eq!(
            derive(&input(&base("blocked"), LockState::Held)).status,
            SessionStatus::Processing
        );
        assert_eq!(
            derive(&input(&base("max-tokens"), LockState::Held)).status,
            SessionStatus::Finished
        );
        assert_eq!(
            derive(&input(&base("interrupted"), LockState::Held)).status,
            SessionStatus::Waiting
        );
        // P2"运行失败→红"：reason.kind=="error" 是红·错误的唯一来源（控制者裁决补充行）
        assert_eq!(
            derive(&input(&base("error"), LockState::Held)).status,
            SessionStatus::Waiting
        );
    }

    #[test]
    fn approval_open_without_decided_is_waiting() {
        // approval/asked 无同 id 的 decided → 红（turn 未闭合场景）
        let e = vec![
            ev("turn/start", 4, json!({})),
            ev("approval/asked", 5, json!({ "id": "appr-1" })),
            ev("approval/decided", 6, json!({ "id": "appr-x" })), // 决的是别的审批
        ];
        assert_eq!(
            derive(&input(&e, LockState::Held)).status,
            SessionStatus::Waiting
        );
    }

    #[test]
    fn scan_facts_derive_facts_equivalent_to_derive() {
        // L2 预算层拆分等价性（session_scan.rs："纯内容产物进缓存 + 时间叠加现算"）：
        // derive ≡ derive_facts(scan_facts(events), lock, silence)，对黄金样本与
        // 合成分支逐一断言，保证拆分不改变任何判定语义
        let cases: Vec<(Vec<DshEvent>, LockState, Option<i64>)> = vec![
            (
                fixture_events("sample1-completed.sanitized.jsonl"),
                LockState::Held,
                None,
            ),
            (
                fixture_events("sample2-tool-error.sanitized.jsonl"),
                LockState::Held,
                None,
            ),
            (
                fixture_events("sample3-approval-decided.sanitized.jsonl"),
                LockState::Held,
                None,
            ),
            (
                fixture_events("sample4-running.sanitized.jsonl"),
                LockState::Held,
                None,
            ),
            // Free / Unknown 双档（10min 黄 / 31min 红）
            (
                fixture_events("sample4-running.sanitized.jsonl"),
                LockState::Free,
                None,
            ),
            (
                fixture_events("sample4-running.sanitized.jsonl"),
                LockState::Unknown,
                Some(10 * 60 * 1000),
            ),
            (
                fixture_events("sample4-running.sanitized.jsonl"),
                LockState::Unknown,
                Some(31 * 60 * 1000),
            ),
            (
                fixture_events("sample5-approval-pending.sanitized.jsonl"),
                LockState::Held,
                None,
            ),
            // end-kind 映射代表分支
            (
                vec![
                    ev("turn/start", 4, json!({})),
                    ev("turn/end", 6, json!({ "reason": { "kind": "aborted" } })),
                ],
                LockState::Held,
                None,
            ),
            (
                vec![
                    ev("turn/start", 4, json!({})),
                    ev("turn/end", 6, json!({ "reason": { "kind": "blocked" } })),
                ],
                LockState::Held,
                None,
            ),
            // 开回合 + 审批未决（优先于 lock 分支）
            (
                vec![
                    ev("turn/start", 4, json!({})),
                    ev("approval/asked", 5, json!({ "id": "a" })),
                ],
                LockState::Free,
                Some(40 * 60 * 1000),
            ),
        ];
        for (events, lock, silence) in &cases {
            let a = derive(&StatusInput {
                events,
                lock: *lock,
                silence_ms: *silence,
            });
            let facts = scan_facts(events);
            let b = derive_facts(&facts, *lock, *silence);
            assert_eq!(a.status, b.status, "status 必须等价");
            assert_eq!(a.end_kind, b.end_kind, "end_kind 必须等价");
            assert_eq!(a.last_end_seq, b.last_end_seq, "last_end_seq 必须等价");
        }
    }

    #[test]
    fn has_open_turn_matches_derive_open_branch() {
        // 开裔回合判定（mod.rs 据此决定是否做 lsof 探测——闭合回合不探测）
        let open = scan_facts(&[ev("turn/start", 4, json!({}))]);
        assert!(open.has_open_turn(), "start 无 end → 开");
        let closed = scan_facts(&[
            ev("turn/start", 4, json!({})),
            ev("turn/end", 6, json!({ "reason": { "kind": "completed" } })),
        ]);
        assert!(!closed.has_open_turn(), "end seq 更新 → 闭");
        let empty = scan_facts(&[]);
        assert!(!empty.has_open_turn(), "空事件流 → 闭（降级 Idle）");
        let reopened = scan_facts(&[
            ev("turn/start", 4, json!({})),
            ev("turn/end", 6, json!({ "reason": { "kind": "completed" } })),
            ev("turn/start", 9, json!({})),
        ]);
        assert!(reopened.has_open_turn(), "新 start seq 更新 → 开");
    }
}
