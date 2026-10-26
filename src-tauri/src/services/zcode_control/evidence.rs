//! T8.2 **仅 debug 证据构建**的「单次验收许可」（`zcode-bridge-evidence`）。
//!
//! 生产能力闸（[`CapabilityPolicy`](super::types::CapabilityPolicy)）把八项控制能力
//! 全部关闭且原因码冻结为 `needs_evidence`；没有实机证据就不能开闸。验收未开闸能力时
//! 又不能伪造证据，因此提供本模块：由**桌面**在指定会话上发放一条内存许可，手机上的
//! 配对设备随后可以执行**恰一次**用户动作，从而跑出真实证据。
//!
//! # 启用条件（全部满足才编入）
//! 1. 构建含 `zcode-bridge-evidence` feature 且为 debug——release 由始终编译的
//!    `zcode_control/mod.rs` 编译门直接拒绝（与 `codex-ipc-e2e-stop` 同一纪律）；
//! 2. 只经桌面专用 IPC 发放，手机不提供发放接口；
//! 3. 许可只存进程内存：不写 settings、不写 DB、不写日志/夹具、不随重启恢复。
//!
//! # 许可语义
//! - 键 = `(sessionId, generation, capability)`；有效期 [`EVIDENCE_PERMIT_TTL_MS`]
//!   （120,000ms），**一次**用户动作后即失效；换代（Hub 代际变化）、换服务
//!   （[`EvidencePermitStore::invalidate_all`]）立即失效；
//! - snooze 是该许可的一次**辅助**操作：首次 snooze 绑定 `interactionId` 且
//!   **不消费**许可；最终 answer 原子消费许可；其他题目不得借用；
//! - 每条命令仍然只发送一次（不因许可而重发）。
//!
//! # 边界（本模块**不**解锁任何协议守卫）
//! 许可只放宽「能力闸」一项。调用方（Hub/命令派发）仍必须先跑全部其他守卫：
//! 目标绑定（会话 + 代际 + 工作区）、权限 option kind（只准 `allowOnce`/`deny`，
//! `fullAccess` 与其他 kind 一律拒绝）、`ready`/新鲜投影、去重与单发。缺卡、错目标、
//! `fullAccess` 等情况**不因**持有许可而通过。
//!
//! 本许可是 Tuvis 的验收设施，不是官方协议功能，也不把未证能力永久打开。

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

/// 许可有效期（毫秒）：一次用户动作的窗口，过期即失效。
pub const EVIDENCE_PERMIT_TTL_MS: u64 = 120_000;

/// 可发放许可的能力名（与 [`CapabilityView`](super::types::CapabilityView) 的八项
/// 字段一一对应；桌面命令只接受这些字面量，避免发放永远匹配不到闸门的空许可）。
pub const CAPABILITY_NAMES: [&str; 8] = [
    "send",
    "single_select",
    "multi_select",
    "free_text",
    "interrupt",
    "approval_reply",
    "model",
    "thought_level",
];

/// 能力名是否属于八项受证据闸约束的能力（去空白后精确匹配）。
pub fn is_known_capability(name: &str) -> bool {
    CAPABILITY_NAMES.contains(&name.trim())
}

/// 一条内存验收许可的快照（只用于回执/显示；真值在
/// [`EvidencePermitStore`] 内，`consumed` 反映发放瞬间的状态）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EvidencePermit {
    pub session_id: String,
    pub generation: u64,
    pub capability: String,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
    pub consumed: bool,
}

/// 许可键：会话 + 代际 + 能力。换代后旧键自然不命中。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct PermitKey {
    session_id: String,
    generation: u64,
    capability: String,
}

/// 进程内的许可真值（只保留「是否可用」所需的最小状态）。
#[derive(Debug, Clone)]
struct PermitEntry {
    expires_at_ms: u64,
    /// snooze 辅助锁：首次 snooze 绑定的题目（绑定后只能由该题 answer 消费）。
    snoozed_interaction: Option<String>,
}

/// 单调时钟注入点（epoch ms）；测试用可控时钟推进过期。
type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

fn now_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn lock_permits(
    permits: &Mutex<HashMap<PermitKey, PermitEntry>>,
) -> MutexGuard<'_, HashMap<PermitKey, PermitEntry>> {
    permits
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 内存许可册（一个 Hub 实例一份；不落盘、不跨进程）。
///
/// 所有方法可在手机动作并发到达时安全调用；未发放、已消费、已过期、换代或换服务后
/// 的 `consume*` 一律返回 `false`（保守拒绝，绝不放宽）。
pub struct EvidencePermitStore {
    permits: Mutex<HashMap<PermitKey, PermitEntry>>,
    clock: Clock,
}

impl EvidencePermitStore {
    /// 生产构造：系统单调时钟（epoch ms）。
    pub fn new() -> Self {
        Self::with_clock(Arc::new(now_epoch_ms))
    }

    fn with_clock(clock: Clock) -> Self {
        Self {
            permits: Mutex::new(HashMap::new()),
            clock,
        }
    }

    fn now(&self) -> u64 {
        (self.clock)()
    }

    /// 桌面发放一条许可（唯一发放入口；不经 settings/DB，也不向手机开放）。
    ///
    /// 空 `capability`（去空白后）不登记——它匹配不到任何真实能力闸，因此永远不可消费。
    /// 同一 `(session, capability)` 的旧代际许可在发放时被清除（换代失效），过期项顺带清理。
    pub fn arm(&self, session_id: &str, generation: u64, capability: &str) -> EvidencePermit {
        let capability = capability.trim().to_owned();
        let now = self.now();
        let expires_at_ms = now.saturating_add(EVIDENCE_PERMIT_TTL_MS);
        if !capability.is_empty() {
            let mut permits = lock_permits(&self.permits);
            permits.retain(|key, entry| {
                // 过期项与「同会话同能力的旧代际」一并清除（换代即失效）。
                entry.expires_at_ms > now
                    && !(key.session_id == session_id
                        && key.capability == capability
                        && key.generation != generation)
            });
            permits.insert(
                PermitKey {
                    session_id: session_id.to_owned(),
                    generation,
                    capability: capability.clone(),
                },
                PermitEntry {
                    expires_at_ms,
                    snoozed_interaction: None,
                },
            );
        }
        EvidencePermit {
            session_id: session_id.to_owned(),
            generation,
            capability,
            issued_at_ms: now,
            expires_at_ms,
            consumed: false,
        }
    }

    /// 原子消费许可（最终 answer / 单次动作）。成功仅一次。
    ///
    /// 已被 snooze 绑题的许可**不**由此消费（必须带 `interactionId` 走
    /// [`Self::consume_answer`]），防止别的题借用。
    pub fn consume(&self, session_id: &str, generation: u64, capability: &str) -> bool {
        self.take(
            session_id,
            generation,
            capability,
            Consumption::AnyInteraction,
        )
    }

    /// 最终 answer 的原子消费：若许可已被 snooze 绑题，绑定必须与本 `interactionId`
    /// 一致；未绑定时与 [`Self::consume`] 等价。
    pub fn consume_answer(
        &self,
        session_id: &str,
        generation: u64,
        capability: &str,
        interaction_id: &str,
    ) -> bool {
        self.take(
            session_id,
            generation,
            capability,
            Consumption::BoundTo(interaction_id),
        )
    }

    /// snooze 辅助锁：首次 snooze 绑定 `interactionId` 且**不消费**许可。
    ///
    /// 同一题重复 snooze 幂等（绑定态不变）；其他题、已消费、已过期、换代或未发放
    /// 一律 `false`（不得借用）。
    pub fn snooze(
        &self,
        session_id: &str,
        generation: u64,
        capability: &str,
        interaction_id: &str,
    ) -> bool {
        let capability = capability.trim();
        if capability.is_empty() || interaction_id.trim().is_empty() {
            return false;
        }
        let now = self.now();
        let mut permits = lock_permits(&self.permits);
        let key = PermitKey {
            session_id: session_id.to_owned(),
            generation,
            capability: capability.to_owned(),
        };
        let state = permits
            .get(&key)
            .map(|entry| (entry.expires_at_ms, entry.snoozed_interaction.clone()));
        match state {
            None => false,
            Some((expires_at_ms, _)) if expires_at_ms <= now => {
                // 过期项立即清除。
                permits.remove(&key);
                false
            }
            // 已绑题：只有同一题的重试幂等通过。
            Some((_, Some(bound))) => bound == interaction_id,
            Some((_, None)) => {
                if let Some(entry) = permits.get_mut(&key) {
                    entry.snoozed_interaction = Some(interaction_id.to_owned());
                }
                true
            }
        }
    }

    /// snooze 辅助锁（**动作路径入口**）：把本会话/本代际下所有尚可用的许可绑定到
    /// 同一 `interactionId`，且**不消费**任何许可。
    ///
    /// - 首次 snooze：绑定所有未绑定许可，返回 `true`；
    /// - 同一题重复 snooze：幂等（绑定态不变），返回 `true`；
    /// - 已有许可绑定到**其他**题：整体拒绝（`false`），且不改写任何绑定——
    ///   其他交互不得借用或抢走已绑定的许可；
    /// - 未发放、已过期、换代：不绑定（无可绑项时返回 `false`）。
    pub fn snooze_interaction(
        &self,
        session_id: &str,
        generation: u64,
        interaction_id: &str,
    ) -> bool {
        if interaction_id.trim().is_empty() {
            return false;
        }
        let now = self.now();
        let mut permits = lock_permits(&self.permits);
        let mut bound_any = false;
        for (key, entry) in permits.iter() {
            if key.session_id != session_id || key.generation != generation {
                continue;
            }
            if entry.expires_at_ms <= now {
                continue;
            }
            if let Some(bound) = entry.snoozed_interaction.as_deref() {
                // 已绑定到别的题：整体拒绝（不猜、不改写）。
                if bound != interaction_id {
                    return false;
                }
            }
        }
        for (key, entry) in permits.iter_mut() {
            if key.session_id != session_id || key.generation != generation {
                continue;
            }
            if entry.expires_at_ms <= now {
                continue;
            }
            if entry.snoozed_interaction.is_none() {
                entry.snoozed_interaction = Some(interaction_id.to_owned());
            }
            bound_any = true;
        }
        bound_any
    }

    /// 只读查询：本会话/本代际下**仍可用**（未消费、未过期）的许可能力名。
    ///
    /// 供 `Hub::session` 在下发手机前覆盖 `SessionView.capabilities`：查询**不消费**
    /// 许可（动作路径仍由 [`Self::consume`]/[`Self::consume_answer`] 原子消费），
    /// 过期项顺带清理。返回顺序固定为 [`CAPABILITY_NAMES`] 的顺序。
    pub fn armed_capabilities(&self, session_id: &str, generation: u64) -> Vec<String> {
        let now = self.now();
        let mut permits = lock_permits(&self.permits);
        permits.retain(|_, entry| entry.expires_at_ms > now);
        let mut armed = Vec::new();
        for capability in CAPABILITY_NAMES {
            let key = PermitKey {
                session_id: session_id.to_owned(),
                generation,
                capability: capability.to_owned(),
            };
            if permits.contains_key(&key) {
                armed.push(capability.to_owned());
            }
        }
        armed
    }

    /// 换服务/关连接：清空全部许可（Hub 换代或 relay 关闭时调用）。
    pub fn invalidate_all(&self) {
        lock_permits(&self.permits).clear();
    }

    /// 单个会话失效（如该会话投影失鲜 / 目标解绑）。
    pub fn invalidate_session(&self, session_id: &str) {
        lock_permits(&self.permits).retain(|key, _| key.session_id != session_id);
    }

    fn take(
        &self,
        session_id: &str,
        generation: u64,
        capability: &str,
        consumption: Consumption<'_>,
    ) -> bool {
        let capability = capability.trim();
        if capability.is_empty() {
            return false;
        }
        let now = self.now();
        let mut permits = lock_permits(&self.permits);
        let key = PermitKey {
            session_id: session_id.to_owned(),
            generation,
            capability: capability.to_owned(),
        };
        let state = permits
            .get(&key)
            .map(|entry| (entry.expires_at_ms, entry.snoozed_interaction.clone()));
        match state {
            None => false,
            Some((expires_at_ms, _)) if expires_at_ms <= now => {
                permits.remove(&key);
                false
            }
            Some((_, Some(bound))) => match consumption {
                // 已绑题：只有该题自己的 answer 能消费。
                Consumption::BoundTo(expected) => {
                    if bound != expected {
                        return false;
                    }
                    permits.remove(&key);
                    true
                }
                // 不带题号：不得绕开 snooze 绑题。
                Consumption::AnyInteraction => false,
            },
            // 未绑题：任意路径都可原子消费（单次）。
            Some((_, None)) => {
                permits.remove(&key);
                true
            }
        }
    }
}

impl Default for EvidencePermitStore {
    fn default() -> Self {
        Self::new()
    }
}

/// 消费语汇：是否要求与 snooze 绑定一致。
#[derive(Debug, Clone, Copy)]
enum Consumption<'a> {
    /// 不带题号（未被 snooze 绑定时可用）。
    AnyInteraction,
    /// 最终 answer 携带的 `interactionId`。
    BoundTo(&'a str),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// 合成会话 ID（不用真实账号数据）。
    const SESSION: &str = "sess_00000000000000000000000000000001";

    /// 可控时钟：测试用 `advance` 制造过期，不睡真实时间。
    struct TestClock {
        now: AtomicU64,
    }

    impl TestClock {
        fn advance(&self, ms: u64) {
            self.now.fetch_add(ms, Ordering::SeqCst);
        }
    }

    fn store_with_clock() -> (EvidencePermitStore, Arc<TestClock>) {
        let clock = Arc::new(TestClock {
            now: AtomicU64::new(1_700_000_000_000),
        });
        let handle = Arc::clone(&clock);
        let store =
            EvidencePermitStore::with_clock(Arc::new(move || handle.now.load(Ordering::SeqCst)));
        (store, clock)
    }

    #[test]
    fn evidence_permit_single_use_and_expires() {
        let (store, clock) = store_with_clock();

        let permit = store.arm(SESSION, 7, "multi_select");
        assert!(!permit.consumed);
        assert_eq!(
            permit.expires_at_ms - permit.issued_at_ms,
            EVIDENCE_PERMIT_TTL_MS,
            "许可有效期必须是 120,000ms"
        );

        // 一次动作后立即失效。
        assert!(store.consume(SESSION, 7, "multi_select"));
        assert!(
            !store.consume(SESSION, 7, "multi_select"),
            "单次许可不得复用"
        );

        // 未发放的能力、其他会话一律不认。
        assert!(!store.consume(SESSION, 7, "interrupt"));
        assert!(!store.consume("sess_other", 7, "multi_select"));

        // 过期前一刻仍有效…
        let _ = store.arm(SESSION, 7, "interrupt");
        clock.advance(EVIDENCE_PERMIT_TTL_MS - 1);
        assert!(store.consume(SESSION, 7, "interrupt"));

        // …到点即失效（`now >= expiresAt`）。
        let _ = store.arm(SESSION, 7, "send");
        clock.advance(EVIDENCE_PERMIT_TTL_MS);
        assert!(!store.consume(SESSION, 7, "send"), "过期许可不得消费");
        assert!(
            !store.consume(SESSION, 7, "send"),
            "过期许可已清除，再试仍拒绝"
        );

        // snooze 辅助锁：首次 snooze 绑题且不消费；最终 answer 原子消费。
        let _ = store.arm(SESSION, 9, "single_select");
        assert!(store.snooze(SESSION, 9, "single_select", "interaction-1"));
        assert!(
            store.snooze(SESSION, 9, "single_select", "interaction-1"),
            "同一题重复 snooze 幂等，绑定态不变"
        );
        assert!(
            !store.snooze(SESSION, 9, "single_select", "interaction-2"),
            "其他题目不得借用该许可"
        );
        assert!(
            !store.consume(SESSION, 9, "single_select"),
            "已绑题的许可必须由该题 answer 消费"
        );
        assert!(
            !store.consume_answer(SESSION, 9, "single_select", "interaction-2"),
            "别的题 answer 不得消费"
        );
        assert!(store.consume_answer(SESSION, 9, "single_select", "interaction-1"));
        assert!(
            !store.consume_answer(SESSION, 9, "single_select", "interaction-1"),
            "answer 消费后即失效"
        );
        // 绑题后过期的许可，最终 answer 也认不回来。
        let _ = store.arm(SESSION, 9, "single_select");
        assert!(store.snooze(SESSION, 9, "single_select", "interaction-3"));
        clock.advance(EVIDENCE_PERMIT_TTL_MS);
        assert!(!store.consume_answer(SESSION, 9, "single_select", "interaction-3"));

        // 空能力不登记（匹配不到任何真实闸门）。
        let empty = store.arm(SESSION, 9, "   ");
        assert!(empty.capability.is_empty());
        assert!(!store.consume(SESSION, 9, ""));
    }

    /// 最小调用方守卫（模拟 Hub 命令派发的检查顺序）。
    ///
    /// 顺序固定为「目标绑定 → 权限 option kind（拒绝 fullAccess）→ 能力闸（证据许可）」；
    /// 许可只放宽最后一项。
    struct Request<'a> {
        session_id: &'a str,
        generation: u64,
        capability: &'a str,
        option_kind: &'a str,
        full_access: bool,
    }

    #[derive(Debug, PartialEq, Eq)]
    enum CallerError {
        TargetMismatch,
        ForbiddenOption,
        NeedsEvidence,
    }

    const BOUND_SESSION: &str = "sess_00000000000000000000000000000002";
    const BOUND_GENERATION: u64 = 11;

    fn caller_guard(store: &EvidencePermitStore, request: &Request<'_>) -> Result<(), CallerError> {
        if request.session_id != BOUND_SESSION || request.generation != BOUND_GENERATION {
            return Err(CallerError::TargetMismatch);
        }
        if request.full_access || !matches!(request.option_kind, "allowOnce" | "deny") {
            return Err(CallerError::ForbiddenOption);
        }
        if !store.consume(request.session_id, request.generation, request.capability) {
            return Err(CallerError::NeedsEvidence);
        }
        Ok(())
    }

    #[test]
    fn evidence_permit_does_not_bypass_target_or_permission_guards() {
        let (store, _clock) = store_with_clock();
        let _permit = store.arm(BOUND_SESSION, BOUND_GENERATION, "approval_reply");

        // 错目标（他会话 / 他代际）：许可在场也不得通过，且许可未被消耗。
        let wrong_session = Request {
            session_id: "sess_00000000000000000000000000000003",
            generation: BOUND_GENERATION,
            capability: "approval_reply",
            option_kind: "allowOnce",
            full_access: false,
        };
        assert_eq!(
            caller_guard(&store, &wrong_session),
            Err(CallerError::TargetMismatch)
        );
        let wrong_generation = Request {
            session_id: BOUND_SESSION,
            generation: BOUND_GENERATION + 1,
            ..wrong_session
        };
        assert_eq!(
            caller_guard(&store, &wrong_generation),
            Err(CallerError::TargetMismatch)
        );

        // fullAccess / 未知 option kind：一律拒绝，许可在场不放宽。
        let full_access = Request {
            session_id: BOUND_SESSION,
            generation: BOUND_GENERATION,
            capability: "approval_reply",
            option_kind: "allowOnce",
            full_access: true,
        };
        assert_eq!(
            caller_guard(&store, &full_access),
            Err(CallerError::ForbiddenOption)
        );
        let unknown_kind = Request {
            option_kind: "alwaysAllowAll",
            ..full_access
        };
        let unknown_kind = Request {
            full_access: false,
            ..unknown_kind
        };
        assert_eq!(
            caller_guard(&store, &unknown_kind),
            Err(CallerError::ForbiddenOption)
        );

        // 能力不匹配：许可只授权它自己那项能力。
        let other_capability = Request {
            capability: "send",
            option_kind: "allowOnce",
            ..unknown_kind
        };
        assert_eq!(
            caller_guard(&store, &other_capability),
            Err(CallerError::NeedsEvidence)
        );

        // 守卫全部满足才轮到许可：合法请求恰好通过一次。
        let legit = Request {
            capability: "approval_reply",
            ..other_capability
        };
        assert_eq!(caller_guard(&store, &legit), Ok(()));
        assert_eq!(
            caller_guard(&store, &legit),
            Err(CallerError::NeedsEvidence),
            "许可是单次的，第二次必须回到 needs_evidence"
        );
    }

    #[test]
    fn evidence_permit_invalidates_on_generation_change() {
        let (store, _clock) = store_with_clock();

        // 换代：旧许可不命中新代际，反之亦然。
        let _ = store.arm(SESSION, 5, "send");
        assert!(!store.consume(SESSION, 6, "send"), "换代后旧许可不得借用");
        let _ = store.arm(SESSION, 6, "send");
        assert!(!store.consume(SESSION, 5, "send"), "旧代际许可不得复活");
        assert!(store.consume(SESSION, 6, "send"), "当前代际许可有效");

        // 关服务：整体失效。
        let _ = store.arm(SESSION, 6, "interrupt");
        store.invalidate_all();
        assert!(!store.consume(SESSION, 6, "interrupt"));
        assert!(
            !store.snooze(SESSION, 6, "interrupt", "interaction-1"),
            "换服务后 snooze 也不得绑定"
        );

        // 单会话失效不影响其他会话。
        let other = "sess_00000000000000000000000000000004";
        let _ = store.arm(SESSION, 6, "send");
        let _ = store.arm(other, 6, "send");
        store.invalidate_session(SESSION);
        assert!(!store.consume(SESSION, 6, "send"));
        assert!(store.consume(other, 6, "send"));
    }

    /// `release_evidence_build_rejected`：编译门（不是运行时用例）。
    ///
    /// release + `zcode-bridge-evidence` 时整包编译失败，测试二进制根本不会产出，
    /// 因此无法用运行时断言直接触发那条 `compile_error!`。这里改为两条可执行的守护：
    /// 1. 本测试进程必须是 debug（release 下本模块不编入，用例不会运行）；
    /// 2. 始终编译的 `mod.rs` 里仍写着那条门（防有人删掉）。
    ///
    /// 人工校验命令（预期编译失败，错误信息含 `zcode-bridge-evidence is only allowed
    /// in debug evidence builds`）：
    /// `cd src-tauri && cargo check --release --lib --features zcode-bridge-evidence`
    #[test]
    fn release_evidence_build_rejected() {
        // 经 `black_box` 读取，避免把构建属性当成常量断言（clippy 语义也更贴近运行时）。
        let debug_build = std::hint::black_box(cfg!(debug_assertions));
        assert!(debug_build, "证据许可只在 debug 证据构建中编译");
        const MOD_SRC: &str = include_str!("mod.rs");
        assert!(
            MOD_SRC.contains(r#"feature = "zcode-bridge-evidence", not(debug_assertions)"#),
            "mod.rs 必须保留 release+feature 的编译门 cfg"
        );
        assert!(
            MOD_SRC.contains(r#"compile_error!("zcode-bridge-evidence is only allowed in debug evidence builds")"#),
            "mod.rs 必须保留 release+feature 的 compile_error"
        );
    }
}
