// APP 类会话状态推导共享核（issue #6）：
// WorkBuddy 与 Codex APP 两个工具的 JSONL 状态判定统一收敛于此——
// 各工具只写一个「格式翻译」适配器（平铺 JSONL / response_item 外壳 → AppEntryKind），
// 判定规则（尾部倒扫取第一条有语义条目 + 300s mtime 叠加）为唯一事实源。
// 不依赖任何工具协议细节；CLI 工具的消息模型判定（monitor::status::determine_status）不在此统一

use crate::session::SessionStatus;

/// 归一化 APP 会话条目（两个工具的 JSONL 格式翻译后的公共语义）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppEntryKind {
    /// 用户消息（提问/指令）→ Thinking
    UserMessage,
    /// 助手纯文本消息（回合完成信号）→ Idle
    AssistantMessage,
    /// 工具调用（function_call / function_call_output / function_call_result）→ Processing
    ToolCall,
    /// **用户输入类工具调用（待决）**→ Waiting。语义 = 「终端在等用户作答」：
    /// 该工具调用的产物就是用户输入，调用尾部无配对结果即说明用户 UI 还开着
    /// （丁T1，2026-09-21）。与 [`AppEntryKind::ToolCall`] 的区别是**语义红**而非
    /// 启发式红——判据是 call_id 配对（见 [`pending_user_input_call`]），有明确
    /// 证据，不是「停更猜等待」。已知唯一来源：codex `request_user_input`
    /// （2026-09-21 本机 rollout 全库扫描：配对 22 次 / 未配对 1 次）。
    /// **本变体不参与任何兜底路径**（兜底红由 `overlay_mtime_stale` 产生）。
    UserInputToolCall,
    /// 回合开始（Codex event_msg task_started）→ Processing
    TurnStart,
    /// 回合结束（Codex event_msg task_complete）→ Idle
    TurnEnd,
    /// 记账/系统条目（reasoning、token_count、item_completed、world_state、turn_context、
    /// thread_settings_applied、session_meta、developer 消息等）→ 不参与判定
    Other,
}

/// App 形态状态叠加阈值（spec §4「叠加 mtime 阈值（App 形态 300s，与 Codex APP 一致）」）：
/// JSONL mtime 停更超过该时长时，函数调用类尾部（Processing）降级为 Waiting
pub const APP_STATUS_STALE_MS: u64 = 300_000;

/// mtime 阈值叠加（spec §4）：函数调用类尾部停更 >= 300s 降级 Waiting；
/// assistant 文本（Idle）等其余状态不受影响。mtime 年龄不可知时按未过期处理（防御）
pub fn overlay_mtime_stale(status: SessionStatus, mtime_age_ms: u64) -> SessionStatus {
    overlay_stale_with_descendants(status, mtime_age_ms, DescendantActivity::Absent)
}

/// 后代（子代理）活跃度（D5 共享层通用能力）：会话自身停更时，其派生的后代
/// 会话是否仍在活动。既有工具无后代语义 → [`DescendantActivity::Absent`]，
/// 行为与 [`overlay_mtime_stale`] 完全一致（ZCode 接入轮的回归基线）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DescendantActivity {
    /// 至少一个后代会话在停更阈值窗口内有更新（如 ZCode 子代理执行期间主会话零写入）
    Active,
    /// 存在后代会话，但全部同样停更（后代窗口内无更新）
    Stale,
    /// 无后代会话（既有工具的空后代语义 = 现状行为不变）
    Absent,
    /// 存在**已派发但未结算**的后台工作（子代理 / 后台 bash / 后台任务）：
    /// 与 Active 的本质区别——Active 依赖"最近有写入"，长思考期会被误判 Stale；
    /// Pending 依赖"派发事件已见、结算事件未见"，语义是"账没结清"，与静默无关。
    /// 语义优先级 Pending > Active > Stale > Absent——各 parser 在**调用点**按
    /// 此优先条件映射（不改 descendant_activity 映射函数本身，Task 5/7/8 的
    /// 接入形态：已有后代活跃信号之上，未结算证据在场时改调完成仲裁）
    Pending,
}

/// 停更降级 + 后代活跃度仲裁（D5，判定核纯函数）：
/// - 非 Processing：透传（与 overlay_mtime_stale 一致）；
/// - Processing 且停更 < 阈值：透传；
/// - Processing 且停更 >= 阈值：
///   - 后代活跃 / 后台未结算 → 保持 Processing（「主会话静默 = 健康等待子代理」，
///     实测 ZCode 子代理执行期间主会话 17 分钟零写入，不得降级；Pending 与
///     Active 同映射——账未结清时静默同样是健康等待，绿态拦截由
///     [`overlay_completion_with_pending`] 单独负责）；
///   - 后代停更 / 无后代 → 降级 Waiting（疑似卡住）。
///
/// Absent 分支即 overlay_mtime_stale 原语义——既有工具（Codex/WorkBuddy）行为零变化
pub fn overlay_stale_with_descendants(
    status: SessionStatus,
    mtime_age_ms: u64,
    descendants: DescendantActivity,
) -> SessionStatus {
    match status {
        SessionStatus::Processing if mtime_age_ms >= APP_STATUS_STALE_MS => match descendants {
            DescendantActivity::Active | DescendantActivity::Pending => SessionStatus::Processing,
            DescendantActivity::Stale | DescendantActivity::Absent => SessionStatus::Waiting,
        },
        other => other,
    }
}

/// 完成态仲裁（2026-09-15 子代理完成信号修复）：
/// 回合收尾推导出的绿（Idle/Finished）在**存在未结算后台工作**时不成立——
/// 真实场景：主会话派发后台子代理后结束本回合（ZCode step-finish(stop) /
/// Claude assistant 尾部 / dsh turn/end completed），子代理仍在跑，MAM 却报完成
/// 并播提示音、发通知、插未读卡。实测 ZCode 392 次转绿中 256 次（65%）属此类。
///
/// 非 Pending 一律透传：Active/Stale/Absent 保持既有语义（既有工具零变化，
/// 见 non_pending_keeps_green 回归锁）。黄/红态不受影响——Pending 只拦"报完成"。
pub fn overlay_completion_with_pending(
    status: SessionStatus,
    descendants: DescendantActivity,
) -> SessionStatus {
    match (status, descendants) {
        (SessionStatus::Idle | SessionStatus::Finished, DescendantActivity::Pending) => {
            SessionStatus::Waiting
        }
        (other, _) => other,
    }
}

/// 归一化条目来源（各 parser 格式翻译层产出，P2 修复的共享判据）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryOrigin {
    /// 真人输入
    Human,
    /// 工具注入的合成条目（子代理/后台任务通知、系统提醒、上下文注入、摘要）
    Synthetic,
    /// 无法判定
    Unknown,
}

/// 合成标记 → 条目来源（各 parser 共享的判据）：
/// 工具侧显式的 synthetic 标记或"UI 不可见"标记任一命中即视为注入。
/// 两标记实测对真实用户输入零误伤（ZCode：1051 条合成全部命中，629 条
/// user_prompt 全部不命中）。
///
/// 为何不用 kind 枚举白名单：kind 取值随工具版本改名（ZCode 已有
/// background_notification / subagent_notification / system_reminder /
/// todo_reminder / compact_summary 五种），枚举法必然随版本失效——这正是
/// 本轮缺陷的成因（现有白名单只列了两种）。**语义标记优先于枚举。**
pub fn entry_origin_from_flags(is_synthetic: bool, ui_hidden: bool) -> EntryOrigin {
    if is_synthetic || ui_hidden {
        EntryOrigin::Synthetic
    } else {
        EntryOrigin::Human
    }
}

/// 判定核（纯函数）：从尾部倒扫，跳过 Other，取第一条有语义条目定状态——
/// 「最后一条说了什么就是什么」的推广（WorkBuddy 语义不变；Codex 修复点 =
/// 工具调用条目不再被无视，第二轮运行期不再恒判 Idle）。
/// entries 须按文件顺序（旧 → 新）传入，尾部 = 切片末端。
/// 无任何语义条目时返回 None，兜底留给各工具（Codex：文件新鲜 → Processing，
/// 停更 → Waiting；WorkBuddy：→ Waiting）——两边边界行为都不变
pub fn derive_app_status(entries: &[AppEntryKind]) -> Option<SessionStatus> {
    entries.iter().rev().find_map(|k| match k {
        AppEntryKind::UserMessage => Some(SessionStatus::Thinking),
        AppEntryKind::AssistantMessage => Some(SessionStatus::Idle),
        AppEntryKind::ToolCall => Some(SessionStatus::Processing),
        // 丁T1：用户输入类工具调用（待决）→ Waiting。**语义红**：不是「停更猜等待」，
        // 而是「该工具调用的产物就是用户输入，且它还没被回答」这一明确证据。
        // 调用方**不得**把它当兜底红就地转 Idle（codex 的兜底红消除只针对
        // overlay_mtime_stale 产生的 Waiting——见 codex_parser::session_from_digest）
        AppEntryKind::UserInputToolCall => Some(SessionStatus::Waiting),
        AppEntryKind::TurnStart => Some(SessionStatus::Processing),
        AppEntryKind::TurnEnd => Some(SessionStatus::Idle),
        AppEntryKind::Other => None,
    })
}

/// 配对判据（丁T1，纯函数，可测）：判定「终端在等用户作答」——某次用户输入类工具
/// 调用（如 codex `request_user_input`）**没有**同名 call_id 的工具结果
/// （`function_call_output`）即待决；有配对结果（用户已作答）不触发。
///
/// 为什么必须按 call_id 配对而不能只看「尾部是 function_call」：真实 rollout 里
/// 工具调用与其结果相邻落盘，且同一文件内含多轮多次调用——只看形态会把**已答完的
/// 历史调用**误判为待决（实测本机全库：`request_user_input` 配对 22 次 vs 未配对
/// 1 次；其他工具配对 916 次 vs 未配对 6 次——只看形态的噪声面大得多）。
///
/// 集合口径（顺序无关）：真实数据里结果恒在调用之后，集合判定更宽容且实现更简；
/// 调用方按「未配对的那个调用在 kinds 流里的位置」标注语义（见 codex_parser），
/// 因此若其后还有更新的语义条目，尾扫仍由更新的条目胜出（不会误红）。
pub fn unpaired_user_input_call_ids<'a>(calls: &[&'a str], outputs: &[&str]) -> Vec<&'a str> {
    calls
        .iter()
        .filter(|id| !outputs.contains(*id))
        .copied()
        .collect()
}

/// [`unpaired_user_input_call_ids`] 的布尔形态：存在未配对的用户输入类调用
/// = 终端在等用户作答
pub fn pending_user_input_call(calls: &[&str], outputs: &[&str]) -> bool {
    !unpaired_user_input_call_ids(calls, outputs).is_empty()
}

/// 尾部第一条有语义条目（= derive_app_status 的判定依据条目；全记账 → None）。
/// 假绿治理共享件（spec 2026-09-16）：WorkBuddy 完成防抖（§4.2）与 Codex 回合守卫（§4.1）
/// 共用——识别「Idle 是否由 assistant 尾导出」
pub fn tail_semantic_kind(entries: &[AppEntryKind]) -> Option<AppEntryKind> {
    entries
        .iter()
        .rev()
        .find(|k| !matches!(k, AppEntryKind::Other))
        .copied()
}

/// 回合开闭判定（spec 2026-09-16 §4.1 方案 A）：最后一个 TurnStart 晚于最后一个
/// TurnEnd → Some(true)；窗口内无任何边界事件 → None（调用方回退尾扫语义，不仲裁）。
/// dsh `TurnFacts::has_open_turn` 的 AppEntryKind 语义镜像（其类型绑定 DshEvent，不可直接复用）
pub fn turn_window_open(entries: &[AppEntryKind]) -> Option<bool> {
    let mut last_start: Option<usize> = None;
    let mut last_end: Option<usize> = None;
    for (i, k) in entries.iter().enumerate() {
        match k {
            AppEntryKind::TurnStart => last_start = Some(i),
            AppEntryKind::TurnEnd => last_end = Some(i),
            _ => {}
        }
    }
    match (last_start, last_end) {
        (Some(s), Some(e)) => Some(s > e),
        (Some(_), None) => Some(true),
        (None, Some(_)) => Some(false),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- 判定核：尾部倒扫取第一条有语义条目 ----

    #[test]
    fn user_message_tail_is_thinking() {
        assert_eq!(
            derive_app_status(&[AppEntryKind::UserMessage]),
            Some(SessionStatus::Thinking)
        );
    }

    #[test]
    fn assistant_message_tail_is_idle() {
        assert_eq!(
            derive_app_status(&[AppEntryKind::AssistantMessage]),
            Some(SessionStatus::Idle)
        );
    }

    #[test]
    fn tool_call_tail_is_processing() {
        assert_eq!(
            derive_app_status(&[AppEntryKind::ToolCall]),
            Some(SessionStatus::Processing)
        );
    }

    #[test]
    fn turn_start_tail_is_processing() {
        assert_eq!(
            derive_app_status(&[AppEntryKind::TurnStart]),
            Some(SessionStatus::Processing)
        );
    }

    #[test]
    fn turn_end_tail_is_idle() {
        assert_eq!(
            derive_app_status(&[AppEntryKind::TurnEnd]),
            Some(SessionStatus::Idle)
        );
    }

    // ---- 丁T1：用户输入类工具调用（待决）语义 ----

    /// 待决的用户输入类工具调用在尾 → Waiting（语义红：终端在等用户作答）
    #[test]
    fn user_input_tool_call_tail_is_waiting() {
        assert_eq!(
            derive_app_status(&[AppEntryKind::UserMessage, AppEntryKind::UserInputToolCall]),
            Some(SessionStatus::Waiting)
        );
    }

    /// 新变体参与尾扫（非 Other）：它比更早的条目优先，也会被更新的语义条目盖过
    #[test]
    fn user_input_tool_call_participates_in_tail_scan() {
        // 尾扫取第一条有语义条目 = 新变体本身
        assert_eq!(
            tail_semantic_kind(&[
                AppEntryKind::UserMessage,
                AppEntryKind::UserInputToolCall,
                AppEntryKind::Other,
            ]),
            Some(AppEntryKind::UserInputToolCall)
        );
        // 更新的 assistant 文本（回合收尾）盖过它 → Idle
        assert_eq!(
            derive_app_status(&[
                AppEntryKind::UserInputToolCall,
                AppEntryKind::AssistantMessage
            ]),
            Some(SessionStatus::Idle)
        );
    }

    /// 回合守卫不看新变体：窗口内的开/闭对不影响 Waiting 判定（守卫只作用于
    /// Idle→Processing 的中间 assistant 消息，不得把语义红改判掉）
    #[test]
    fn open_turn_does_not_override_user_input_waiting() {
        let entries = [
            AppEntryKind::TurnStart,
            AppEntryKind::UserMessage,
            AppEntryKind::UserInputToolCall,
        ];
        assert_eq!(turn_window_open(&entries), Some(true), "回合确实仍开");
        assert_eq!(
            derive_app_status(&entries),
            Some(SessionStatus::Waiting),
            "回合守卫不得把语义红改判（它只对 Idle→Processing 生效）"
        );
    }

    // ---- 丁T1：call_id 配对判据（共享核纯函数） ----

    /// 尾部 call 无配对 output → 待决
    #[test]
    fn pending_user_input_call_detects_unpaired() {
        assert!(pending_user_input_call(&["call_1"], &[]));
        // 前一个已配对、尾部这个未配对 → 仍待决
        assert!(pending_user_input_call(&["call_1", "call_2"], &["call_1"]));
    }

    /// 有配对 output（用户已作答）→ 不触发；空流 → 不触发
    #[test]
    fn pending_user_input_call_requires_no_output() {
        assert!(!pending_user_input_call(&[], &[]));
        assert!(!pending_user_input_call(&["call_1"], &["call_1"]));
        // 真实时序（2026-09-21 rollout）：call → 79s 后 output → 已答
        assert!(!pending_user_input_call(
            &["call_1"],
            &["other_tool_call", "call_1"]
        ));
    }

    /// 同名 call_id 的 output 只配对自己的 call（不同 id 不互相抵消）
    #[test]
    fn pending_user_input_call_pairs_by_call_id() {
        // 别的 call_id 的 output 在场不算配对（真实 rollout：其他工具 output 916 次）
        assert!(pending_user_input_call(&["call_1"], &["call_other"]));
        // 多个调用里任一个未配对即待决
        assert!(pending_user_input_call(
            &["call_1", "call_2"],
            &["call_2", "call_other"]
        ));
    }

    #[test]
    fn last_semantic_entry_wins() {
        // 工具调用 → 助手文本：最后一条有语义条目（assistant）定状态
        let entries = [
            AppEntryKind::UserMessage,
            AppEntryKind::ToolCall,
            AppEntryKind::AssistantMessage,
        ];
        assert_eq!(derive_app_status(&entries), Some(SessionStatus::Idle));
        // 助手文本 → 工具调用：运行中
        let entries = [
            AppEntryKind::UserMessage,
            AppEntryKind::AssistantMessage,
            AppEntryKind::ToolCall,
        ];
        assert_eq!(derive_app_status(&entries), Some(SessionStatus::Processing));
    }

    #[test]
    fn bookkeeping_entries_are_skipped() {
        // 记账条目（Other）在尾不改变判定：其前一条有语义条目定状态
        let entries = [
            AppEntryKind::UserMessage,
            AppEntryKind::ToolCall,
            AppEntryKind::Other,
            AppEntryKind::Other,
        ];
        assert_eq!(derive_app_status(&entries), Some(SessionStatus::Processing));
        // 回合结束 + 尾部记账条目 → 仍为 Idle（顺带改善：旧实现误显运行中）
        let entries = [
            AppEntryKind::UserMessage,
            AppEntryKind::AssistantMessage,
            AppEntryKind::Other,
        ];
        assert_eq!(derive_app_status(&entries), Some(SessionStatus::Idle));
    }

    #[test]
    fn no_semantic_entry_returns_none() {
        assert_eq!(derive_app_status(&[]), None);
        assert_eq!(
            derive_app_status(&[AppEntryKind::Other, AppEntryKind::Other]),
            None
        );
    }

    // ---- 300s mtime 叠加（自 workbuddy_parser 迁入，语义不变）----

    #[test]
    fn processing_stale_downgrades_to_waiting() {
        // 函数调用类尾部 + JSONL 停更 >= 300s → Processing 降级 Waiting
        assert_eq!(
            overlay_mtime_stale(SessionStatus::Processing, APP_STATUS_STALE_MS),
            SessionStatus::Waiting
        );
        assert_eq!(
            overlay_mtime_stale(SessionStatus::Processing, APP_STATUS_STALE_MS + 1),
            SessionStatus::Waiting
        );
    }

    #[test]
    fn processing_fresh_stays_processing() {
        assert_eq!(
            overlay_mtime_stale(SessionStatus::Processing, APP_STATUS_STALE_MS - 1),
            SessionStatus::Processing
        );
        assert_eq!(
            overlay_mtime_stale(SessionStatus::Processing, 0),
            SessionStatus::Processing
        );
    }

    #[test]
    fn idle_stays_idle_regardless_of_mtime() {
        // assistant 纯文本尾部是明确完成信号：文件过旧也不拉回 Waiting（与 determine_status 语义一致）
        assert_eq!(
            overlay_mtime_stale(SessionStatus::Idle, APP_STATUS_STALE_MS * 10),
            SessionStatus::Idle
        );
    }

    #[test]
    fn waiting_passes_through_unaffected() {
        assert_eq!(
            overlay_mtime_stale(SessionStatus::Waiting, APP_STATUS_STALE_MS * 10),
            SessionStatus::Waiting
        );
        assert_eq!(
            overlay_mtime_stale(SessionStatus::Waiting, 0),
            SessionStatus::Waiting
        );
    }

    // ---- D5：后代活跃度仲裁（共享层通用能力，ZCode 接入轮） ----

    /// D5 回归基线：空后代语义（Absent）= overlay_mtime_stale 现状行为，
    /// 逐边界对照证明既有工具（Codex/WorkBuddy）零变化
    #[test]
    fn absent_descendants_matches_legacy_overlay_exactly() {
        for status in [
            SessionStatus::Processing,
            SessionStatus::Thinking,
            SessionStatus::Idle,
            SessionStatus::Waiting,
            SessionStatus::Compacting,
            SessionStatus::Finished,
        ] {
            for age in [
                0u64,
                1,
                APP_STATUS_STALE_MS - 1,
                APP_STATUS_STALE_MS,
                APP_STATUS_STALE_MS + 1,
                APP_STATUS_STALE_MS * 10,
            ] {
                assert_eq!(
                    overlay_stale_with_descendants(status.clone(), age, DescendantActivity::Absent),
                    overlay_mtime_stale(status.clone(), age),
                    "status={:?} age={} 与既有 overlay 语义必须一致",
                    status,
                    age
                );
            }
        }
    }

    /// 后代活跃：主会话停更超时也不降级（子代理长任务全程保持运行中）
    #[test]
    fn active_descendants_keep_processing_when_stale() {
        assert_eq!(
            overlay_stale_with_descendants(
                SessionStatus::Processing,
                APP_STATUS_STALE_MS * 4, // 实测子代理窗口 17 分钟 >> 300s
                DescendantActivity::Active
            ),
            SessionStatus::Processing
        );
    }

    /// 后代也停更：降级 Waiting（疑似卡住）
    #[test]
    fn stale_descendants_downgrade_to_waiting() {
        assert_eq!(
            overlay_stale_with_descendants(
                SessionStatus::Processing,
                APP_STATUS_STALE_MS,
                DescendantActivity::Stale
            ),
            SessionStatus::Waiting
        );
    }

    /// 无后代 + 停更超时：降级 Waiting（= 现状行为）
    #[test]
    fn absent_descendants_downgrade_to_waiting() {
        assert_eq!(
            overlay_stale_with_descendants(
                SessionStatus::Processing,
                APP_STATUS_STALE_MS + 1,
                DescendantActivity::Absent
            ),
            SessionStatus::Waiting
        );
    }

    /// 非运行态不受后代活跃度影响（Idle/Waiting/Thinking/Finished 透传）
    #[test]
    fn non_processing_statuses_ignore_descendants() {
        for status in [
            SessionStatus::Idle,
            SessionStatus::Waiting,
            SessionStatus::Thinking,
            SessionStatus::Finished,
        ] {
            for desc in [
                DescendantActivity::Active,
                DescendantActivity::Stale,
                DescendantActivity::Absent,
            ] {
                assert_eq!(
                    overlay_stale_with_descendants(status.clone(), APP_STATUS_STALE_MS * 10, desc),
                    status
                );
            }
        }
    }

    /// 停更阈值内：后代状态无关紧要（Processing 保持）
    #[test]
    fn fresh_processing_ignores_descendants() {
        for desc in [
            DescendantActivity::Active,
            DescendantActivity::Stale,
            DescendantActivity::Absent,
        ] {
            assert_eq!(
                overlay_stale_with_descendants(
                    SessionStatus::Processing,
                    APP_STATUS_STALE_MS - 1,
                    desc
                ),
                SessionStatus::Processing
            );
        }
    }

    // ---- 假绿治理共享件（spec 2026-09-16 §4.1/§4.2）----

    #[test]
    fn tail_semantic_kind_returns_last_non_other() {
        let entries = [
            AppEntryKind::UserMessage,
            AppEntryKind::Other,
            AppEntryKind::ToolCall,
            AppEntryKind::Other,
        ];
        assert_eq!(tail_semantic_kind(&entries), Some(AppEntryKind::ToolCall));
        // 全记账条目 → None（derive 核同样返回 None，兜底留给各工具）
        assert_eq!(tail_semantic_kind(&[AppEntryKind::Other]), None);
        assert_eq!(tail_semantic_kind(&[]), None);
    }

    #[test]
    fn turn_window_open_detects_pair_order() {
        use AppEntryKind::*;
        // 开：最后一个 TurnStart 晚于最后一个 TurnEnd
        assert_eq!(
            turn_window_open(&[
                TurnEnd,
                AssistantMessage,
                TurnStart,
                UserMessage,
                AssistantMessage
            ]),
            Some(true)
        );
        // 闭：最后一个 TurnEnd 更晚
        assert_eq!(
            turn_window_open(&[TurnStart, ToolCall, TurnEnd, AssistantMessage]),
            Some(false)
        );
        // 只有 TurnStart（回合刚开始，未见 TurnEnd）→ 开
        assert_eq!(turn_window_open(&[UserMessage, TurnStart]), Some(true));
        // 只有 TurnEnd（上回合闭合痕迹，起点不可见）→ 不可证开
        assert_eq!(turn_window_open(&[TurnEnd, AssistantMessage]), Some(false));
        // 窗口内无任何边界事件 → None（调用方回退现状，不仲裁）
        assert_eq!(turn_window_open(&[UserMessage, AssistantMessage]), None);
        assert_eq!(turn_window_open(&[]), None);
    }

    // ---- P1：绿态仲裁（2026-09-15 子代理完成信号修复，Task 1）----

    /// 核心回归锁：回合收尾判绿 + 后台工作未结算 ⇒ 不判绿
    #[test]
    fn pending_descendants_block_green() {
        assert_eq!(
            overlay_completion_with_pending(SessionStatus::Idle, DescendantActivity::Pending),
            SessionStatus::Waiting,
            "后台工作未结算时不得报完成（Z-P1/C-P1/D-P1 根因）"
        );
        assert_eq!(
            overlay_completion_with_pending(SessionStatus::Finished, DescendantActivity::Pending),
            SessionStatus::Waiting,
            "error 提示转绿同样受仲裁约束"
        );
    }

    /// 无未结算工作 → 绿态透传（既有行为零变化）
    #[test]
    fn non_pending_keeps_green() {
        for status in [SessionStatus::Idle, SessionStatus::Finished] {
            for desc in [
                DescendantActivity::Active,
                DescendantActivity::Stale,
                DescendantActivity::Absent,
            ] {
                assert_eq!(
                    overlay_completion_with_pending(status.clone(), desc),
                    status,
                    "只有 Pending 拦截绿态；Active/Stale/Absent 不得改变既有语义"
                );
            }
        }
    }

    /// 非绿态不受影响（黄/红语义不变）
    #[test]
    fn non_green_statuses_ignore_pending() {
        for status in [
            SessionStatus::Processing,
            SessionStatus::Thinking,
            SessionStatus::Waiting,
            SessionStatus::Compacting,
        ] {
            assert_eq!(
                overlay_completion_with_pending(status.clone(), DescendantActivity::Pending),
                status
            );
        }
    }

    /// 组合序锁：先停更仲裁、后完成仲裁，两级互不吞没
    #[test]
    fn overlays_compose_in_order() {
        // 停更的 Processing + 后代活跃 → 保持 Processing；再经完成仲裁仍是 Processing
        let s1 = overlay_stale_with_descendants(
            SessionStatus::Processing,
            APP_STATUS_STALE_MS * 2,
            DescendantActivity::Active,
        );
        assert_eq!(s1, SessionStatus::Processing);
        assert_eq!(
            overlay_completion_with_pending(s1, DescendantActivity::Pending),
            SessionStatus::Processing
        );
        // 停更的 Processing + 无后代 → Waiting；完成仲裁不动 Waiting
        let s2 = overlay_stale_with_descendants(
            SessionStatus::Processing,
            APP_STATUS_STALE_MS * 2,
            DescendantActivity::Absent,
        );
        assert_eq!(s2, SessionStatus::Waiting);
    }

    /// Pending 在停更仲裁里与 Active 同映射（账未结清时静默 = 健康等待，
    /// 不降级；绿态拦截归 overlay_completion_with_pending 单独负责）
    #[test]
    fn pending_maps_like_active_in_stale_overlay() {
        assert_eq!(
            overlay_stale_with_descendants(
                SessionStatus::Processing,
                APP_STATUS_STALE_MS * 2,
                DescendantActivity::Pending
            ),
            SessionStatus::Processing,
            "未结算后台工作在跑 → 主会话静默不降级（与 Active 同理）"
        );
    }

    // ---- P2：注入识别口径（Task 1）----

    #[test]
    fn synthetic_flags_map_to_synthetic_origin() {
        assert_eq!(entry_origin_from_flags(true, false), EntryOrigin::Synthetic);
        assert_eq!(entry_origin_from_flags(false, true), EntryOrigin::Synthetic);
        assert_eq!(entry_origin_from_flags(true, true), EntryOrigin::Synthetic);
    }

    #[test]
    fn clean_flags_map_to_human_origin() {
        assert_eq!(entry_origin_from_flags(false, false), EntryOrigin::Human);
    }
}
