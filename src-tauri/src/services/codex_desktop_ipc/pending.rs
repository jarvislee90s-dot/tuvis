//! Pending Request 层：`conversationState.requests[]` → `PendingView` 投影。
//!
//! 纪律（spec 3.4/3.5 + 计划公共约定）：
//! * requestId 按不透明标识处理（整数/字符串），对外只发无损 `request_key`；
//! * `context_token` 是服务端随机 UUID，绑定 boot/epoch/generation/thread/request；
//!   同一 request 的流式 revision 变化不换 token；内容变化、消失/重现、owner 换代
//!   必须换 token；
//! * 未知 method → `PendingKind::Unsupported`（actionable=false），不暴露 raw
//!   method/参数对象；已知但 profile 能力未验证同样 actionable=false；
//! * 决策词只由**服务端策略**产出（单一来源），来源优先级：
//!   `pending.params.availableDecisions`（**仅字符串词**）> schema 档 > 证据档；
//!   对象形态的决策词（`acceptWithExecpolicyAmendment` / `applyNetworkPolicyAmendment`
//!   等执行策略修订 = 权限扩大）**永不发**。
//!   证据档（已知 build）固定 `accept` / `decline`，**不读** `availableDecisions`
//!   （4894 真机已证实 `decision:"decline"` 被接受）；schema 档（未知 build）取
//!   `availableDecisions` 字符串词与 schema 允许词的交集，缺失时才回退 schema 词；
//!   权限/问答/未知类别永远空。

use super::types::{
    AnswerPolicy, CapabilitySet, PendingKind, PendingRequestId, PendingView, ProtocolError,
    QuestionOption, QuestionView,
};
use std::collections::{HashMap, HashSet};

/// token 缓存条目：内容指纹 + 当前 token。
#[derive(Clone)]
struct TokenEntry {
    fingerprint: u64,
    token: String,
}

/// 由能力位 + 策略推导的逐操作事实（**兼容形态**）。
///
/// 生产投影/写入口必须用 adapter 的逐操作策略（含契约匹配与真实成功证据）；
/// 这里只服务历史调用点与测试，schema 档的合法词也进入统一事实。
fn operations_from_caps_with_policy(
    caps: &CapabilitySet,
    policy: &DecisionPolicy,
) -> super::capability::EffectiveOperations {
    let mut shipped = std::collections::BTreeMap::new();
    shipped.insert(
        super::capability::CodexOperation::BlockingInput,
        caps.blocking_input,
    );
    // 兼容构造：命令审批沿用 caps.command（与历史投影一致）；文件审批没有独立
    // shipped 位——不制造证据（缺条目 = 不表态，由策略/契约另行判定）。
    shipped.insert(
        super::capability::CodexOperation::CommandApproval,
        caps.command,
    );
    let context = super::capability::OperationContext {
        target_fresh: true,
        auto_adapt_enabled: false,
        runtime_status: "idle".into(),
        blocking_pending: true,
        queue_paused: false,
        read_evidence: caps.state,
        shipped,
        observed: Default::default(),
        schema: Default::default(),
        contract: Default::default(),
        contract_identified: false,
        contract_error: None,
        display_support: Default::default(),
        display_identity_verified: false,
    };
    let mut operations = super::capability::OperationPolicy.evaluate(&context);
    // 兼容入口（project_with_policy / 旧镜像直投影）：文件审批没有独立 shipped 位，
    // 由**该策略自己的词**放行（生产路径用 adapter 的真实逐操作事实，不经过这里）。
    let file_accept = policy
        .for_kind(PendingKind::FileChangeApproval)
        .iter()
        .any(|word| word == DECISION_ACCEPT);
    if file_accept {
        if let Some(status) = operations
            .operations
            .get_mut(&super::capability::CodexOperation::FileChangeApproval)
        {
            status.allowed = true;
            status.reason_code = None;
            status.evidence = super::capability::EvidenceSource::SchemaMatched;
        }
    }
    if policy.source() == PolicySource::Schema {
        // schema 档：策略里的合法词进入统一事实（否则卡片会被"无证据"拒绝，
        // 而详情/写入口却认为可用——H07 要消灭的两套事实）。
        for (kind, operation) in [
            (
                PendingKind::CommandApproval,
                super::capability::CodexOperation::CommandApproval,
            ),
            (
                PendingKind::FileChangeApproval,
                super::capability::CodexOperation::FileChangeApproval,
            ),
        ] {
            let allowed = policy
                .for_kind(kind)
                .iter()
                .any(|word| word == DECISION_ACCEPT);
            if let (true, Some(status)) = (allowed, operations.operations.get_mut(&operation)) {
                status.allowed = true;
                status.reason_code = None;
                status.evidence = super::capability::EvidenceSource::SchemaMatched;
            }
        }
    }
    operations
}

/// 兼容入口：由 caps/策略构造逐操作事实（镜像直投影的历史路径使用；生产投影
/// 由 adapter 计算真实事实后覆盖）。
pub(crate) fn compat_operations(
    caps: &CapabilitySet,
    policy: &DecisionPolicy,
) -> super::capability::EffectiveOperations {
    operations_from_caps_with_policy(caps, policy)
}

/// pending 投影器（每 Thread 一个；owner/epoch 换代时由 mirror 调 `reset`）。
/// token 缓存用内部互斥锁：投影只需 `&self`（mirror.project 免 &mut）。
#[derive(Default)]
pub struct PendingProjector {
    tokens: std::sync::Mutex<HashMap<String, TokenEntry>>,
}

impl PendingProjector {
    pub fn new() -> Self {
        Self::default()
    }

    /// owner/epoch 换代：全部 token 失效（旧卡一律拒绝）。
    pub fn reset(&self) {
        self.tokens.lock().expect("token lock").clear();
    }

    /// 把 `requests[]` 投影为 `PendingView` 列表（输入数组顺序即输出顺序）。
    /// 投影后清除已消失请求的 token（R4：`[r]→[]→[r]` 重现时生成新 token，
    /// 旧卡不可复活）。
    ///
    /// 策略缺省 = 证据档（`caps.command` 有真机证据 → accept/decline）；未知 build 的
    /// schema 档由调用方用 [`PendingProjector::project_with_policy`] 传入。
    pub fn project(
        &self,
        requests: &[serde_json::Value],
        caps: &CapabilitySet,
    ) -> Vec<PendingView> {
        self.project_with_policy(requests, caps, &DecisionPolicy::from_caps(caps))
    }

    /// 同上，但显式指定决策策略（未知 build 的 schema 档 / 测试注入）。
    ///
    /// 兼容入口：阻塞问答的可操作性由能力位推导（老调用/测试语义不变）。生产投影
    /// 与写入口走 [`Self::project_with_operations`]（adapter 的逐操作策略）。
    pub fn project_with_policy(
        &self,
        requests: &[serde_json::Value],
        caps: &CapabilitySet,
        policy: &DecisionPolicy,
    ) -> Vec<PendingView> {
        let operations = operations_from_caps_with_policy(caps, policy);
        self.project_with_operations(
            requests,
            caps,
            policy,
            &operations,
            "",
            &serde_json::Value::Null,
        )
    }

    /// T04：显式给**逐操作有效策略**的投影入口（与 HTTP 写入口同一份事实）。
    pub fn project_with_operations(
        &self,
        requests: &[serde_json::Value],
        caps: &CapabilitySet,
        policy: &DecisionPolicy,
        operations: &super::capability::EffectiveOperations,
        thread_id: &str,
        state: &serde_json::Value,
    ) -> Vec<PendingView> {
        let mut tokens = self.tokens.lock().expect("token lock");
        // R4：本轮仍在场的 request_key 之外的缓存条目一律清除
        let live: std::collections::HashSet<String> = requests
            .iter()
            .filter_map(|req| {
                let id: PendingRequestId = serde_json::from_value(req.get("id")?.clone()).ok()?;
                Some(id.to_key())
            })
            .collect();
        tokens.retain(|key, _| live.contains(key));
        requests
            .iter()
            .filter_map(|req| {
                project_one(req, caps, policy, operations, thread_id, state, &mut tokens)
            })
            .collect()
    }

    /// 校验请求回显的 context_token 是否仍有效。
    pub fn token_valid(&self, request_key: &str, context_token: &str) -> bool {
        self.tokens
            .lock()
            .expect("token lock")
            .get(request_key)
            .is_some_and(|e| e.token == context_token)
    }
}

fn fingerprint(method: &str, value: &serde_json::Value) -> u64 {
    // 规范化序列化指纹（R4：含 method——同 params 不同 method 必须视为不同上下文）：
    // `method + "\n" + canonical params`；内容变化即可检出（无需密码学强度）
    let canonical = format!(
        "{method}\n{}",
        serde_json::to_string(value).unwrap_or_default()
    );
    canonical.bytes().fold(0xcbf29ce484222325u64, |acc, b| {
        (acc ^ u64::from(b)).wrapping_mul(0x100000001b3)
    })
}

fn project_one(
    req: &serde_json::Value,
    caps: &CapabilitySet,
    policy: &DecisionPolicy,
    operations: &super::capability::EffectiveOperations,
    thread_id: &str,
    state: &serde_json::Value,
    tokens: &mut HashMap<String, TokenEntry>,
) -> Option<PendingView> {
    let id_value = req.get("id")?;
    let request_id: PendingRequestId = serde_json::from_value(id_value.clone()).ok()?;
    let request_key = request_id.to_key();
    let params = req
        .get("params")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");

    let (
        kind,
        actionable_caps,
        disabled,
        command,
        cwd,
        reason,
        file_summary,
        permission_summary,
        questions,
    ) = match method {
        "item/commandExecution/requestApproval" => {
            let cap_ok = caps.command;
            (
                PendingKind::CommandApproval,
                cap_ok,
                // 方法已知、该 build 能力未验证 → unverified（与「方法未知」拆码，
                // 前端文案可区分：「当前 Desktop 版本未验证该操作」）
                (!cap_ok).then(|| "capability_unverified".to_string()),
                Some(
                    params
                        .get("command")
                        .and_then(|c| c.as_str())
                        .unwrap_or("")
                        .to_string(),
                ),
                params
                    .get("cwd")
                    .and_then(|c| c.as_str())
                    .map(str::to_string),
                params
                    .get("reason")
                    .and_then(|r| r.as_str())
                    .map(str::to_string),
                None,
                None,
                Vec::new(),
            )
        }
        // #172/T7：文件变更审批（与命令审批同属 L3；decision 方法名不同）
        "item/fileChange/requestApproval" => {
            // H07：卡片可操作性 = **统一逐操作策略**（与详情/最终写入口同一事实）。
            let cap_ok = operations
                .get(super::capability::CodexOperation::FileChangeApproval)
                .is_some_and(|status| status.implemented && status.allowed);
            (
                PendingKind::FileChangeApproval,
                cap_ok,
                (!cap_ok).then(|| "capability_unverified".to_string()),
                None,
                params
                    .get("cwd")
                    .and_then(|c| c.as_str())
                    .map(str::to_string),
                params
                    .get("reason")
                    .and_then(|r| r.as_str())
                    .map(str::to_string),
                // H07：**只**接受可定位的安全摘要（参数内 changes 必须含路径）；
                // 旧"条数摘要"不再作为可批准依据（canonical 关联优先，见下方）。
                super::interaction_summary::safe_pending_summary(method, &params),
                None,
                Vec::new(),
            )
        }
        "item/permissions/requestApproval" => {
            let cap_ok = caps.permissions;
            (
                PendingKind::PermissionsApproval,
                cap_ok,
                // 同上：已知方法的未验证能力 → unverified
                (!cap_ok).then(|| "capability_unverified".to_string()),
                None,
                params
                    .get("cwd")
                    .and_then(|c| c.as_str())
                    .map(str::to_string),
                params
                    .get("reason")
                    .and_then(|r| r.as_str())
                    .map(str::to_string),
                None,
                Some(permissions_summary(params.get("permissions"))),
                Vec::new(),
            )
        }
        "item/tool/requestUserInput" => {
            // T04：阻塞问答的可操作性 = **逐操作有效策略**（契约匹配/精确 shipped/
            // 真实成功），不再只看静态 `caps.blocking_input`——否则"契约已匹配但
            // build 未登记"的会话会显示一张永远不能操作的卡。原因码沿用策略的
            // 机器码（前端按码取文案）。
            let cap_ok = operations
                .get(super::capability::CodexOperation::BlockingInput)
                .is_some_and(|status| status.implemented && status.allowed);
            let questions = params
                .get("questions")
                .and_then(|q| q.as_array())
                .map(|arr| arr.iter().filter_map(question_view).collect())
                .unwrap_or_default();
            (
                PendingKind::UserInput,
                cap_ok,
                // 同上：已知方法的未验证能力 → unverified
                (!cap_ok).then(|| {
                    operations
                        .get(super::capability::CodexOperation::BlockingInput)
                        .and_then(|status| status.reason_code.clone())
                        .unwrap_or_else(|| "capability_unverified".to_string())
                }),
                None,
                None,
                None,
                None,
                None,
                questions,
            )
        }
        _ => (
            // 未知 method：只显示「此请求暂不支持远程处理」，不暴露 raw method/参数
            // （保持 capability_unavailable——与「已知但未验证」的 unverified 拆分）
            PendingKind::Unsupported,
            false,
            Some("capability_unavailable".to_string()),
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
        ),
    };

    // 决策词由**服务端策略**决定（单一来源）：空 = 只能到桌面处理。
    // 卡片只按这个列表渲染按钮，前端不做策略判断。
    //
    // T7-R2：未知 build（schema 档）优先用 pending 自带的 `availableDecisions`
    // **字符串词**（与策略词取交集）；schema 词只作兜底。证据档不读该字段
    // （4894 真机已证实 `decision:"decline"` 被接受）。对象形态（执行策略修订 =
    // 权限扩大）在 [`string_decision_words`] 里就被跳过，永不进入 allowed_decisions。
    let allowed_decisions = decision_words_for(kind, policy, &params);
    // H07：决策词必须与**当前逐操作准入**一致（详情/卡片/最终写入口同一事实）。
    let operation_status = match kind {
        PendingKind::CommandApproval => Some(super::capability::CodexOperation::CommandApproval),
        PendingKind::FileChangeApproval => {
            Some(super::capability::CodexOperation::FileChangeApproval)
        }
        _ => None,
    };
    // 缺该操作条目（测试/旧载荷）→ 不表态（沿用策略）；条目存在则以它为准。
    let ops_gate = operation_status
        .and_then(|operation| operations.get(operation))
        .map(|status| {
            (
                status.implemented && status.allowed,
                status.reason_code.clone(),
            )
        });
    let allowed_decisions = match &ops_gate {
        Some((false, _)) => Vec::new(),
        _ => allowed_decisions,
    };
    // T05/H07：文件审批的权威变更以 canonical 关联为准（params 的安全摘要只作兜底，
    // 且同样要求可定位）；关联不到且没有可定位参数摘要 = 无摘要。
    let file_summary = match (kind, file_summary.clone()) {
        (PendingKind::FileChangeApproval, fallback) => {
            let turn_id = params
                .get("turnId")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let item_id = params
                .get("itemId")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            // N09：canonical **找到了但不安全** → 撤批准，绝不用参数摘要子集覆盖；
            // 只有 really Missing 才允许参数侧的安全摘要兜底。
            match super::interaction_summary::resolve_file_summary_state(
                state, thread_id, turn_id, item_id,
            ) {
                super::interaction_summary::FileSummaryState::Valid(summary) => Some(summary),
                super::interaction_summary::FileSummaryState::Unsafe => None,
                super::interaction_summary::FileSummaryState::Missing => fallback,
            }
        }
        (_, summary) => summary,
    };
    // 摘要不足 → 移除"批准"（保留可靠的拒绝/中止）；不猜 diff、不盲目批准。
    let allowed_decisions = if kind == PendingKind::FileChangeApproval && file_summary.is_none() {
        allowed_decisions
            .into_iter()
            .filter(|word| word != DECISION_ACCEPT)
            .collect::<Vec<String>>()
    } else {
        allowed_decisions
    };
    let actionable = match kind {
        // 命令/文件审批：能给出决策词才算可操作（未知 build 未取证 = 不可操作）
        PendingKind::CommandApproval | PendingKind::FileChangeApproval => {
            !allowed_decisions.is_empty()
        }
        // 权限/问答/未知：沿用能力门（问答走 Answer 动作，不是决策词）
        _ => actionable_caps,
    };
    let disabled = if actionable {
        None
    } else if let Some((false, reason)) = ops_gate {
        Some(reason.unwrap_or_else(|| "not_actionable".to_string()))
    } else {
        disabled
    };

    // token：同 request 同 method 同内容 → 复用；内容/方法变化、重现 → 换新。
    // R12：文件审批的**关联变更摘要**也进入指纹——params 不变而 canonical 变更内容
    // 更新时，旧 token 必须失效（否则旧页面能批准新内容）。
    let fp = match kind {
        PendingKind::FileChangeApproval => {
            // H07：权限版本绑定**完整关联内容**（不是展示摘要/路径列表）。
            let turn_id = params
                .get("turnId")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let item_id = params
                .get("itemId")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let content = super::interaction_summary::resolve_file_change_fingerprint(
                state, thread_id, turn_id, item_id,
            );
            fingerprint(
                method,
                &serde_json::json!({ "params": params, "content": content }),
            )
        }
        _ => fingerprint(method, &params),
    };
    let token = match tokens.get(&request_key) {
        Some(entry) if entry.fingerprint == fp => entry.token.clone(),
        _ => {
            let t = super::types::new_uuid();
            tokens.insert(
                request_key.clone(),
                TokenEntry {
                    fingerprint: fp,
                    token: t.clone(),
                },
            );
            t
        }
    };

    Some(PendingView {
        request_key,
        context_token: token,
        kind,
        actionable,
        disabled_reason_code: disabled,
        allowed_decisions,
        command,
        cwd,
        reason,
        file_summary,
        permission_summary,
        questions,
    })
}

/// 从 pending `params.availableDecisions` 里取**字符串词**（T7-R2）。
///
/// 真机形态（26.1016.6573.0）：`["accept", {acceptWithExecpolicyAmendment: {…}},
/// "cancel"]`——字符串词与对象形态混排，且**没有** `decline` 字面量。对象形态属
/// 执行策略修订（权限扩大），**一律跳过**；本函数只收 `Value::String`，保持原顺序
/// 并去重。字段缺失 / 非数组 / 没有任何字符串词 → `None`（调用方回退策略词）。
/// 从 pending `params.availableDecisions` 里取**安全字符串词**（跳过对象扩权词）。
///
/// 字段缺失 / 非数组 → `None`（调用方按"缺失"处理）；显式空数组或只有对象 →
/// `Some(vec![])`（显式列表为空，**不回退开放**）。保持请求顺序并去重。
#[derive(Debug, Clone, PartialEq, Eq)]
enum DecisionWords {
    /// 字段缺失 → 该种类已核验的最小策略。
    Missing,
    /// 字段存在但形状异常（null/对象/字符串）→ 保守空集（不猜）。
    Invalid,
    /// 显式数组：只保留安全字符串词（对象扩权词跳过）。
    Words(Vec<String>),
}

fn string_decision_words(params: &serde_json::Value) -> DecisionWords {
    let Some(value) = params.get("availableDecisions") else {
        return DecisionWords::Missing;
    };
    let Some(available) = value.as_array() else {
        // R10：协议形状漂移（null/字符串/对象）**不是缺字段**——不进入缺省兜底，
        // 否则未知形状会被当成"没提供 → 开放最小集"，等于把畸形当授权。
        return DecisionWords::Invalid;
    };
    let mut seen = HashSet::new();
    let mut words = Vec::new();
    for value in available {
        if let Some(word) = value.as_str() {
            if seen.insert(word) {
                words.push(word.to_string());
            }
        }
    }
    DecisionWords::Words(words)
}

/// T05：**决策词矩阵**（逐种类、逐证据档统一规则）。
///
/// 规则（spec §7.1）：
/// * `availableDecisions` **存在且为数组**：取安全字符串词 ∩ 已实现词 ∩ 策略词；
///   **显式空列表 / 只有对象（扩权词）→ 空**（不回退开放，不因旧证据档绕过）；
/// * 字段**缺失**：按该请求种类**已核验的最小策略**（command = accept/cancel、
///   file = accept/decline）再与策略词交集；未知契约不猜；
/// * 非数组（字段存在但形状异常）→ 空（不猜）。
fn decision_words_for(
    kind: PendingKind,
    policy: &DecisionPolicy,
    params: &serde_json::Value,
) -> Vec<String> {
    if !matches!(
        kind,
        PendingKind::CommandApproval | PendingKind::FileChangeApproval
    ) {
        return policy.for_kind(kind).to_vec();
    }
    let policy_words = policy.for_kind(kind);
    if policy_words.is_empty() {
        return Vec::new();
    }
    match string_decision_words(params) {
        DecisionWords::Words(request_words) => request_words
            .into_iter()
            .filter(|word| {
                matches!(
                    word.as_str(),
                    DECISION_ACCEPT | DECISION_DECLINE | DECISION_CANCEL
                )
            })
            .filter(|word| policy_words.iter().any(|allowed| allowed == word))
            .collect(),
        DecisionWords::Invalid => Vec::new(),
        DecisionWords::Missing => {
            let minimal: &[&str] = match kind {
                PendingKind::CommandApproval => &[DECISION_ACCEPT, DECISION_CANCEL],
                _ => &[DECISION_ACCEPT, DECISION_DECLINE],
            };
            minimal
                .iter()
                .filter(|word| policy_words.iter().any(|allowed| allowed == *word))
                .map(|word| (*word).to_string())
                .collect()
        }
    }
}

/// 请求词 ∩ 策略词（保持**请求词顺序**）；交集为空 → 关闭操作。
///
/// 交集保证 schema 档不会发出 schema 未取证的词，也保证 pending 没提供的词
/// （例如命令侧的 `decline`）不会凭空出现。
/// **决策策略**（服务端单一来源）：每一类审批允许哪些决策词。
///
/// 两档来源（[`DecisionPolicy::source`]）：
/// * [`DecisionPolicy::evidence`]：该 build 有真机证据（`caps.command`）→ `accept` +
///   `decline`（老行为，`decline` = 拒绝但回合继续）；**不读** `availableDecisions`；
/// * [`DecisionPolicy::schema`]：未知 build 但官方 schema 里两个词都在 → `accept` +
///   `cancel`（`cancel` = 拒绝并**立即中断本轮**）——投影时优先采用 pending 自带的
///   `availableDecisions` 字符串词（与本档词取交集），本档词只作兜底。
///
/// 空策略 = 不可远程决策（卡片只显示「到桌面处理」）。**权限/问答永远是空**。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DecisionPolicy {
    source: PolicySource,
    command: Vec<String>,
    file: Vec<String>,
}

/// 策略来源（T7-R2）：决定投影时能否用 pending 自带的 `availableDecisions` 修正决策词。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PolicySource {
    /// 空策略：没有任何已授信的决策词（不可远程决策）。
    #[default]
    Empty,
    /// 证据档：该 build 有真机证据（shipped `caps.command`）。
    Evidence,
    /// schema 档：未知 build，官方 schema 已取证。
    Schema,
}

/// `accept`：允许执行。
pub const DECISION_ACCEPT: &str = "accept";
/// `decline`：拒绝，但回合继续（老行为；仅证据档提供）。
pub const DECISION_DECLINE: &str = "decline";
/// `cancel`：拒绝并**立即中断本轮**（schema 档的「终止本轮」）。
pub const DECISION_CANCEL: &str = "cancel";

impl DecisionPolicy {
    /// 兼容构造（无 schema 证据时）：有真机证据（`caps.command`）→ 证据档；否则空。
    pub fn from_caps(caps: &CapabilitySet) -> Self {
        if caps.command {
            Self::evidence()
        } else {
            Self::default()
        }
    }

    /// 证据档（已知 build）：`accept` + `decline`。
    pub fn evidence() -> Self {
        Self {
            source: PolicySource::Evidence,
            // 证据档 = 已实现的三个词都可用；**实际出现哪些**由 pending 自带的
            // availableDecisions（显式列表取交集）或种类最小策略决定。
            command: vec![
                DECISION_ACCEPT.to_string(),
                DECISION_DECLINE.to_string(),
                DECISION_CANCEL.to_string(),
            ],
            file: vec![
                DECISION_ACCEPT.to_string(),
                DECISION_DECLINE.to_string(),
                DECISION_CANCEL.to_string(),
            ],
        }
    }

    /// schema 档（未知 build）：只有 schema 同时认识 `accept` 与 `cancel` 才开放，
    /// 且**只开这两个词**（`decline` 不在未知 build 上冒险——语义差异会让用户以为
    /// 「拒绝后回合继续」，实际我们更希望明确终止）。
    pub fn schema(command_ok: bool, file_ok: bool) -> Self {
        let words = || vec![DECISION_ACCEPT.to_string(), DECISION_CANCEL.to_string()];
        Self {
            source: PolicySource::Schema,
            command: if command_ok { words() } else { Vec::new() },
            file: if file_ok { words() } else { Vec::new() },
        }
    }

    /// R09：**逐类** schema 词（由 schema 证据的实际词表构造）。
    ///
    /// 与 [`Self::schema`] 的区别：文件审批不再被命令策略覆盖——schema 里文件明确
    /// 支持 `decline` 时，缺 `availableDecisions` 的合法文件请求仍可「拒绝但继续」，
    /// 而不是被命令侧的最小集（accept/cancel）压成只剩 accept。
    pub fn schema_with_words(command: Vec<String>, file: Vec<String>) -> Self {
        Self {
            source: PolicySource::Schema,
            command,
            file,
        }
    }

    /// 策略来源（决定投影时是否采用 pending 的 `availableDecisions` 字符串词）。
    pub fn source(&self) -> PolicySource {
        self.source
    }

    /// 该类别允许的决策词（顺序即 UI 顺序）。
    pub fn for_kind(&self, kind: PendingKind) -> &[String] {
        match kind {
            PendingKind::CommandApproval => &self.command,
            PendingKind::FileChangeApproval => &self.file,
            // 权限 / 问答 / 未知：永不开放决策词
            _ => &[],
        }
    }

    /// 是否完全为空（两类都不可决策）。
    pub fn is_empty(&self) -> bool {
        self.command.is_empty() && self.file.is_empty()
    }
}

/// 权限对象 → 人类可读摘要（不含原始对象；远端不得上送自定义 permissions）。
fn permissions_summary(permissions: Option<&serde_json::Value>) -> String {
    let Some(p) = permissions else {
        return "—".into();
    };
    let mut parts: Vec<String> = Vec::new();
    if p.get("network").is_some_and(|n| !n.is_null()) {
        parts.push("network".into());
    }
    if let Some(fs) = p.get("fileSystem") {
        if let Some(read) = fs.get("read").and_then(|r| r.as_array()) {
            if !read.is_empty() {
                parts.push(format!("fs.read: {}", read.len()));
            }
        }
        if let Some(write) = fs.get("write").and_then(|w| w.as_array()) {
            if !write.is_empty() {
                parts.push(format!("fs.write: {}", write.len()));
            }
        }
    }
    if parts.is_empty() {
        "—".into()
    } else {
        parts.join(" / ")
    }
}

fn question_view(q: &serde_json::Value) -> Option<QuestionView> {
    let id = q.get("id")?.as_str()?.to_string();
    let is_other = q.get("isOther").and_then(|b| b.as_bool()).unwrap_or(false);
    Some(QuestionView {
        id,
        header: q
            .get("header")
            .and_then(|h| h.as_str())
            .unwrap_or("")
            .to_string(),
        question: q
            .get("question")
            .and_then(|q| q.as_str())
            .unwrap_or("")
            .to_string(),
        is_other,
        is_secret: q.get("isSecret").and_then(|b| b.as_bool()).unwrap_or(false),
        options: q
            .get("options")
            .and_then(|o| o.as_array())
            .map(|arr| {
                arr.iter()
                    .map(|o| QuestionOption {
                        label: o
                            .get("label")
                            .and_then(|l| l.as_str())
                            .unwrap_or("")
                            .to_string(),
                        description: o
                            .get("description")
                            .and_then(|d| d.as_str())
                            .unwrap_or("")
                            .to_string(),
                    })
                    .collect()
            })
            .unwrap_or_default(),
        // 统一策略：允许自由文本只看该题 `isOther`；多值来自已核验数组契约。
        answer_policy: AnswerPolicy::verified(is_other),
    })
}

/// 原生请求的解决证据（T04：**响应条目优先于单纯消失**）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeResolution {
    /// 请求仍在场。
    Pending,
    /// 请求已消失，且有**准确关联**的已完成响应条目（`userInputResponse.completed`）
    /// ——"已答"，比"消失"强。
    Answered,
    /// 请求消失但没有可关联的响应条目：只能写"请求已结束"，**不猜**具体决策。
    Ended,
}

/// 由原始 conversationState 判定某个原生请求的解决证据。
///
/// * `request_id`：请求 id 的 JSON 值（整数/字符串原样，类型必须一致）；
/// * `answer`：本次动作是"作答"（看 `userInputResponse`）还是审批（不看响应条目）。
///
/// 纪律：`completed` 必须是 `true`；`answers` 缺失不影响"已完成"这一事实，但
/// 关联必须按 **requestId 精确**（不按位置、不按题干）。
pub fn resolve_native_request(
    state: &serde_json::Value,
    request_id: &serde_json::Value,
    answer: bool,
) -> NativeResolution {
    let still_pending = state
        .get("requests")
        .and_then(|r| r.as_array())
        .is_some_and(|requests| {
            requests.iter().any(|request| {
                request
                    .get("id")
                    .map(|id| id == request_id)
                    .unwrap_or(false)
            })
        });
    if still_pending {
        return NativeResolution::Pending;
    }
    if !answer {
        return NativeResolution::Ended;
    }
    let completed = super::history_view::HistoryView::new(state)
        .turns()
        .into_iter()
        .flat_map(|turn| turn.items.iter())
        .filter(|item| item.get("type").and_then(|t| t.as_str()) == Some("userInputResponse"))
        .any(|item| {
            item.get("requestId")
                .map(|id| id == request_id)
                .unwrap_or(false)
                && item.get("completed").and_then(|c| c.as_bool()) == Some(true)
        });
    if completed {
        NativeResolution::Answered
    } else {
        NativeResolution::Ended
    }
}

/// 从 `PendingView` 找回原始 requestId（响应构造用；不经过网络）。
pub fn parse_request_key(key: &str) -> Result<PendingRequestId, ProtocolError> {
    PendingRequestId::from_key(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps_all() -> CapabilitySet {
        CapabilitySet {
            send: true,
            state: true,
            command: true,
            permissions: true,
            blocking_input: true,
            interrupt: false,
        }
    }

    fn caps_unknown_build() -> CapabilitySet {
        CapabilitySet {
            send: false,
            state: true,
            command: false,
            permissions: false,
            blocking_input: false,
            interrupt: false,
        }
    }

    /// #172/T7：文件变更审批的投影——类别、安全摘要、决策词都按策略来；
    /// **不暴露 raw 参数**（真机参数形状未取证）。
    #[test]
    fn file_change_approval_projects_kind_summary_and_policy_words() {
        let p = PendingProjector::new();
        let views = p.project_with_policy(
            &[serde_json::json!({
                "id": 21,
                "method": "item/fileChange/requestApproval",
                "params": { "cwd": "/w", "reason": "r", "changes": [{}, {}, {}] }
            })],
            &caps_all(),
            &DecisionPolicy::evidence(),
        );
        assert_eq!(views.len(), 1);
        let v = &views[0];
        assert_eq!(v.kind, PendingKind::FileChangeApproval);
        // H07/H11：`changes:[{}]` 无任何可定位变更 → 摘要不足 → **不得批准**，
        // 但已核验的 decline 出口保留（不能连可靠拒绝一起丢掉）。
        assert!(v.actionable, "仍可拒绝（decline）");
        assert_eq!(v.allowed_decisions, vec!["decline"]);
        assert_eq!(v.file_summary, None, "条数摘要不再能单独作为可批准摘要");
        assert_eq!(v.command, None, "文件审批没有 command 字段");
        assert_eq!(v.disabled_reason_code, None);
    }

    /// 未知 build 的 schema 档：**只有** `accept` + `cancel`（没有 decline）。
    #[test]
    fn schema_policy_offers_accept_and_cancel_only() {
        let p = PendingProjector::new();
        let policy = DecisionPolicy::schema(true, true);
        let views = p.project_with_policy(
            &[
                serde_json::json!({
                    "id": 1, "method": "item/commandExecution/requestApproval",
                    "params": { "command": "echo hi", "cwd": "/w" }
                }),
                serde_json::json!({
                    "id": 2, "method": "item/fileChange/requestApproval",
                    "params": { "reason": "r" }
                }),
            ],
            &caps_unknown_build(),
            &policy,
        );
        assert_eq!(views.len(), 2);
        assert_eq!(views[0].allowed_decisions, vec!["accept", "cancel"]);
        // T05 语义细化：文件审批的 `params` 没有 `changes`，canonical 里也关联不到
        // 权威 fileChange 条目 → 摘要不足 ⇒ **移除 accept**（不许盲目批准），
        // 但保留可靠拒绝/中止词。命令审批不受影响（可操作）。
        assert!(views[1].allowed_decisions.is_empty());
        assert!(!views[1].actionable);
        assert!(views[0].actionable);
        // 认不出的变更形状 → 无摘要（不猜）
        assert_eq!(views[1].file_summary, None);
    }

    /// 策略为空（未知 build 且未取证）：不可操作 + 明确原因码，绝不暴露决策词。
    #[test]
    fn empty_policy_keeps_command_approval_non_actionable() {
        let p = PendingProjector::new();
        let views = p.project_with_policy(
            &[serde_json::json!({
                "id": 3, "method": "item/commandExecution/requestApproval",
                "params": { "command": "echo hi" }
            })],
            &caps_unknown_build(),
            &DecisionPolicy::default(),
        );
        let v = &views[0];
        assert!(!v.actionable);
        assert!(v.allowed_decisions.is_empty());
        // H07：统一事实原因码（见上）。
        assert_eq!(
            v.disabled_reason_code.as_deref(),
            Some("operation_no_evidence")
        );
    }

    /// 权限/问答**永远**没有决策词（施工单 §3：一律不开放）。
    #[test]
    fn permissions_and_user_input_never_get_decision_words() {
        let p = PendingProjector::new();
        let views = p.project_with_policy(
            &[
                serde_json::json!({
                    "id": 4, "method": "item/permissions/requestApproval",
                    "params": { "cwd": "/w", "permissions": {} }
                }),
                serde_json::json!({
                    "id": 5, "method": "item/tool/requestUserInput",
                    "params": { "questions": [] }
                }),
            ],
            &caps_all(),
            &DecisionPolicy::evidence(),
        );
        for v in &views {
            assert!(
                v.allowed_decisions.is_empty(),
                "{:?} 不得有任何决策词",
                v.kind
            );
        }
    }

    #[test]
    fn unknown_method_is_unsupported_without_raw_leak() {
        let p = PendingProjector::new();
        let views = p.project(
            &[serde_json::json!({
                "id": 5,
                "method": "item/file/requestApproval",
                "params": { "path": "/etc/hosts", "secretStuff": "x" }
            })],
            &caps_all(),
        );
        assert_eq!(views.len(), 1);
        let v = &views[0];
        assert_eq!(v.kind, PendingKind::Unsupported);
        assert!(!v.actionable);
        assert_eq!(
            v.disabled_reason_code.as_deref(),
            Some("capability_unavailable")
        );
        let rendered = serde_json::to_string(v).unwrap();
        assert!(!rendered.contains("item/file/requestApproval"));
        assert!(!rendered.contains("secretStuff"));
        assert!(!rendered.contains("/etc/hosts"));
        assert_eq!(v.request_key, "n:5");
    }

    #[test]
    fn capability_false_is_not_actionable_but_visible() {
        let p = PendingProjector::new();
        let caps = CapabilitySet {
            command: false,
            ..caps_all()
        };
        let views = p.project(
            &[serde_json::json!({
                "id": "s-9",
                "method": "item/commandExecution/requestApproval",
                "params": { "command": "cargo test", "cwd": "/w", "reason": "r" }
            })],
            &caps,
        );
        let v = &views[0];
        assert_eq!(v.kind, PendingKind::CommandApproval);
        assert!(!v.actionable);
        // H07：原因码改为**统一逐操作事实**的 `operation_no_evidence`（卡片与详情/
        // 写入口同一来源；旧 kind 级 capability_unverified 不再单独存在）。
        assert_eq!(
            v.disabled_reason_code.as_deref(),
            Some("operation_no_evidence")
        );
        assert_eq!(v.request_key, "s:s-9");
        assert_eq!(v.command.as_deref(), Some("cargo test"));
    }

    #[test]
    fn token_stable_across_revision_touches_rotates_on_content_change() {
        let p = PendingProjector::new();
        let caps = caps_all();
        let req = serde_json::json!({
            "id": 12,
            "method": "item/permissions/requestApproval",
            "params": { "cwd": "/w", "permissions": { "fileSystem": { "write": ["/tmp/x"] } } }
        });
        let v1 = p.project(std::slice::from_ref(&req), &caps).remove(0);
        let v2 = p.project(&[req], &caps).remove(0);
        assert_eq!(
            v1.context_token, v2.context_token,
            "同内容同 token（流式刷新不换）"
        );

        let changed = serde_json::json!({
            "id": 12,
            "method": "item/permissions/requestApproval",
            "params": { "cwd": "/w", "permissions": { "fileSystem": { "write": ["/tmp/y"] } } }
        });
        let v3 = p.project(&[changed], &caps).remove(0);
        assert_ne!(v1.context_token, v3.context_token, "内容变化换 token");
        assert!(p.token_valid(&v3.request_key, &v3.context_token));
        assert!(!p.token_valid(&v3.request_key, &v1.context_token));

        p.reset();
        let v4 = p.project(&[v3_origin()], &caps).remove(0);
        assert_ne!(
            v4.context_token, v3.context_token,
            "owner 换代后 token 全换"
        );
    }

    fn v3_origin() -> serde_json::Value {
        serde_json::json!({
            "id": 12,
            "method": "item/permissions/requestApproval",
            "params": { "cwd": "/w", "permissions": { "fileSystem": { "write": ["/tmp/y"] } } }
        })
    }

    #[test]
    fn vanished_request_revokes_token_and_reappear_mints_new() {
        // R4：[r]→[]→[r] 重现时必须生成新 token（消失即撤销，旧卡不可复用）
        let p = PendingProjector::new();
        let caps = caps_all();
        let req = serde_json::json!({
            "id": 12, "method": "item/permissions/requestApproval", "params": { "cwd": "/w" }
        });
        let v1 = p.project(std::slice::from_ref(&req), &caps).remove(0);
        // 消失
        let gone = p.project(&[], &caps);
        assert!(gone.is_empty());
        assert!(
            !p.token_valid(&v1.request_key, &v1.context_token),
            "请求消失即撤销 token"
        );
        // 重现：新 token
        let v2 = p.project(&[req], &caps).remove(0);
        assert_ne!(
            v1.context_token, v2.context_token,
            "重现生成新 token，不复活旧 token"
        );
        assert!(p.token_valid(&v2.request_key, &v2.context_token));
    }

    #[test]
    fn fingerprint_includes_method() {
        // R4：指纹 = method + canonical params——同 params 不同 method 的 token 必须不同
        let p = PendingProjector::new();
        let caps = caps_all();
        let a = serde_json::json!({
            "id": 7, "method": "item/permissions/requestApproval", "params": { "cwd": "/w" }
        });
        let b = serde_json::json!({
            "id": 7, "method": "item/commandExecution/requestApproval", "params": { "cwd": "/w" }
        });
        let va = p.project(&[a], &caps).remove(0);
        let vb = p.project(&[b], &caps).remove(0);
        assert_ne!(
            va.context_token, vb.context_token,
            "指纹含 method：同 params 不同 method 换 token"
        );
    }

    // ===== T7-R2：未知 build 用 pending.availableDecisions 的**字符串词** =====

    /// 未知 build（schema 档）优先取 pending 自带的字符串词，并与 schema 允许词取交集。
    #[test]
    fn available_decisions_strings_override_schema_words() {
        let p = PendingProjector::new();
        let views = p.project_with_policy(
            &[
                serde_json::json!({
                    "id": 31,
                    "method": "item/commandExecution/requestApproval",
                    "params": {
                        "command": "echo hi",
                        "cwd": "/w",
                        "availableDecisions": [
                            "accept",
                            { "acceptWithExecpolicyAmendment": {
                                "execpolicy_amendment": ["echo hi"]
                            } },
                            "cancel"
                        ]
                    }
                }),
                // 请求侧没有 cancel → schema 兜底的 cancel 不得凭空出现（证明取交集）
                serde_json::json!({
                    "id": 32,
                    "method": "item/commandExecution/requestApproval",
                    "params": { "command": "echo hi", "availableDecisions": ["accept"] }
                }),
            ],
            &caps_unknown_build(),
            &DecisionPolicy::schema(true, true),
        );
        assert_eq!(views.len(), 2);
        assert_eq!(views[0].allowed_decisions, vec!["accept", "cancel"]);
        assert!(views[0].actionable);
        assert_eq!(views[1].allowed_decisions, vec!["accept"]);
        let json = serde_json::to_string(&views[0]).expect("serialize");
        assert!(
            !json.contains("acceptWithExecpolicyAmendment") && !json.contains("execpolicy"),
            "对象形态的决策词不得进入远端 DTO：{json}"
        );
    }

    /// 未知 build 的 pending 只给 `accept` + `cancel`（无 `decline`）→ 前端不得出现
    /// 「拒绝」（decline）按钮，但仍可批准/终止。
    #[test]
    fn unknown_build_without_decline_has_no_reject() {
        let p = PendingProjector::new();
        let views = p.project_with_policy(
            &[serde_json::json!({
                "id": 33,
                "method": "item/commandExecution/requestApproval",
                "params": {
                    "command": "echo hi",
                    "availableDecisions": ["accept", "cancel"]
                }
            })],
            &caps_unknown_build(),
            &DecisionPolicy::schema(true, true),
        );
        let v = &views[0];
        assert!(v.actionable);
        assert_eq!(v.allowed_decisions, vec!["accept", "cancel"]);
        assert!(
            !v.allowed_decisions.iter().any(|w| w == "decline"),
            "未知 build 不得向 UI 提供 decline：{:?}",
            v.allowed_decisions
        );
    }

    /// 对象形态（执行策略修订 = 权限扩大）一律忽略：既不入 allowedDecisions，也绝不
    /// 会出现在序列化 DTO 里；字符串词照样可用（此例 schema 兜底会给出 accept+cancel，
    /// 实际只剩请求侧的 cancel，证明请求词优先）。
    #[test]
    fn object_decision_words_are_never_propagated() {
        let p = PendingProjector::new();
        let views = p.project_with_policy(
            &[serde_json::json!({
                "id": 34,
                "method": "item/commandExecution/requestApproval",
                "params": {
                    "command": "echo hi",
                    "availableDecisions": [
                        { "acceptWithExecpolicyAmendment": {
                            "execpolicy_amendment": ["echo hi"]
                        } },
                        "cancel"
                    ]
                }
            })],
            &caps_unknown_build(),
            &DecisionPolicy::schema(true, true),
        );
        let v = &views[0];
        assert_eq!(v.allowed_decisions, vec!["cancel"]);
        assert!(v.actionable);
        let json = serde_json::to_string(v).expect("serialize");
        assert!(
            !json.contains("acceptWithExecpolicyAmendment")
                && !json.contains("execpolicy_amendment"),
            "对象形态的决策词不得外泄：{json}"
        );
    }

    #[test]
    fn disjoint_available_decisions_disable_approval() {
        let views = PendingProjector::new().project_with_policy(
            &[serde_json::json!({
                "id": 36, "method": "item/commandExecution/requestApproval",
                "params": { "command": "echo hi", "availableDecisions": ["acceptForSession"] }
            })],
            &caps_unknown_build(),
            &DecisionPolicy::schema(true, true),
        );
        assert!(!views[0].actionable);
        assert!(views[0].allowed_decisions.is_empty());
    }

    /// pending 没带 `availableDecisions` → 按该种类**已核验最小策略**与策略词交集
    /// （T05：文件 = accept/decline，故 schema 档只剩 accept）。
    #[test]
    fn missing_available_decisions_falls_back_to_kind_minimal_policy() {
        let p = PendingProjector::new();
        // H07/H11：摘要不可定位（changes:[{}]）→ 不得批准；schema 明确含 decline，
        // 因此缺词表的合法文件请求仍保留「拒绝但继续」。
        let views = p.project_with_policy(
            &[serde_json::json!({
                "id": 35,
                "method": "item/fileChange/requestApproval",
                "params": { "reason": "r", "changes": [{}] }
            })],
            &caps_unknown_build(),
            &DecisionPolicy::schema_with_words(
                vec![DECISION_ACCEPT.into(), DECISION_CANCEL.into()],
                vec![
                    DECISION_ACCEPT.into(),
                    DECISION_DECLINE.into(),
                    DECISION_CANCEL.into(),
                ],
            ),
        );
        let v = &views[0];
        assert!(v.actionable);
        assert_eq!(v.allowed_decisions, vec!["decline"]);
    }

    /// T05：**所有证据档**都遵守显式 `availableDecisions` 的安全交集（spec §7.1：
    /// 此规则不因旧证据档而被绕过）。显式列表 `["accept","cancel"]` → 就这两个词。
    #[test]
    fn evidence_policy_intersects_explicit_available_decisions() {
        let p = PendingProjector::new();
        let views = p.project_with_policy(
            &[serde_json::json!({
                "id": 36,
                "method": "item/commandExecution/requestApproval",
                "params": {
                    "command": "echo hi",
                    "availableDecisions": ["accept", "cancel"]
                }
            })],
            &caps_all(),
            &DecisionPolicy::evidence(),
        );
        let v = &views[0];
        assert!(v.actionable);
        assert_eq!(
            v.allowed_decisions,
            vec!["accept", "cancel"],
            "显式列表取安全交集（不再按证据档忽略请求自带词）"
        );
    }

    /// `availableDecisions` 只影响命令/文件审批；权限/问答/未知类别仍然永远空。
    #[test]
    fn schema_policy_never_opens_other_kinds_via_available_decisions() {
        let p = PendingProjector::new();
        let views = p.project_with_policy(
            &[
                serde_json::json!({
                    "id": 37,
                    "method": "item/permissions/requestApproval",
                    "params": {
                        "cwd": "/w",
                        "permissions": {},
                        "availableDecisions": ["accept", "cancel"]
                    }
                }),
                serde_json::json!({
                    "id": 38,
                    "method": "item/tool/requestUserInput",
                    "params": {
                        "questions": [],
                        "availableDecisions": ["accept", "cancel"]
                    }
                }),
                serde_json::json!({
                    "id": 39,
                    "method": "item/plan/requestImplementation",
                    "params": {
                        "planContent": "x",
                        "availableDecisions": ["accept", "cancel"]
                    }
                }),
            ],
            &caps_unknown_build(),
            &DecisionPolicy::schema(true, true),
        );
        for v in &views {
            assert!(
                v.allowed_decisions.is_empty(),
                "{:?} 不得因 availableDecisions 开放决策词",
                v.kind
            );
            assert!(!v.actionable);
        }
    }

    /// 纯函数：只收字符串词、保序、去重；对象与空数组 → 空词表；字段缺失 → None。
    #[test]
    fn string_decision_words_keep_order_skip_objects_and_dedupe() {
        let params = serde_json::json!({
            "availableDecisions": [
                "cancel",
                { "acceptWithExecpolicyAmendment": {} },
                "accept",
                "cancel"
            ]
        });
        assert_eq!(
            string_decision_words(&params),
            DecisionWords::Words(vec!["cancel".to_string(), "accept".to_string()])
        );
        assert_eq!(
            string_decision_words(&serde_json::json!({})),
            DecisionWords::Missing
        );
        assert_eq!(
            string_decision_words(&serde_json::json!({ "availableDecisions": [] })),
            DecisionWords::Words(vec![])
        );
        assert_eq!(
            string_decision_words(&serde_json::json!({
                "availableDecisions": [{ "acceptWithExecpolicyAmendment": {} }]
            })),
            DecisionWords::Words(vec![])
        );
    }

    /// T04：响应条目（completed=true）比"请求消失"更强；无条目只能记"已结束"。
    #[test]
    fn f08p_t04_response_completed_is_stronger_than_disappearance() {
        use serde_json::json;
        let state_with_response = json!({
            "requests": [],
            "turnHistory": {"history": {
                "isComplete": true,
                "entitiesByKey": {"turn:t1": {
                    "turnId": "t1",
                    "status": "completed",
                    "items": [{
                        "type": "userInputResponse",
                        "id": "uir-1",
                        "requestId": 187,
                        "completed": true,
                        "answers": {"m1": ["甲"]}
                    }]
                }},
                "islands": [{"id": "i", "entries": [{"key": "turn:t1", "value": "turn:t1"}]}]
            }}
        });
        assert_eq!(
            resolve_native_request(&state_with_response, &json!(187), true),
            NativeResolution::Answered
        );
        // 审批类动作不看 userInputResponse（命令/文件决策有各自终态载体）。
        assert_eq!(
            resolve_native_request(&state_with_response, &json!(187), false),
            NativeResolution::Ended
        );
        // 请求仍在场 → Pending（不被响应条目抢占）。
        let mut with_pending = state_with_response.clone();
        with_pending["requests"] = json!([{"id": 187, "method": "item/tool/requestUserInput"}]);
        assert_eq!(
            resolve_native_request(&with_pending, &json!(187), true),
            NativeResolution::Pending
        );
        // 请求消失且无响应条目 → Ended（不猜"已答"）。
        let mut gone = state_with_response;
        gone["turnHistory"]["history"]["entitiesByKey"]["turn:t1"]["items"] = json!([]);
        assert_eq!(
            resolve_native_request(&gone, &json!(187), true),
            NativeResolution::Ended
        );
        // id 类型必须精确（整数 187 ≠ 字符串 "187"）。
        assert_eq!(
            resolve_native_request(&gone, &json!("187"), true),
            NativeResolution::Ended
        );
    }

    /// T05：决策词**存在性矩阵**——显式空 / 仅扩权对象 / 缺失 / 混合。
    #[test]
    fn f08p_t05_decision_presence_matrix() {
        let p = PendingProjector::new();
        let caps = caps_all();
        // 显式空列表 → 空（不回退开放）
        let empty = p.project_with_policy(
            &[serde_json::json!({
                "id": 40, "method": "item/commandExecution/requestApproval",
                "params": { "command": "echo", "availableDecisions": [] }
            })],
            &caps,
            &DecisionPolicy::evidence(),
        );
        assert!(empty[0].allowed_decisions.is_empty());
        assert!(!empty[0].actionable, "显式空列表 = 不能远程决策");
        // 只有对象（扩权词）→ 空（绝不把扩权当允许）
        let objects = p.project_with_policy(
            &[serde_json::json!({
                "id": 41, "method": "item/commandExecution/requestApproval",
                "params": {
                    "command": "echo",
                    "availableDecisions": [
                        {"acceptWithExecpolicyAmendment": {"execpolicy_amendment": []}}
                    ]
                }
            })],
            &caps,
            &DecisionPolicy::evidence(),
        );
        assert!(objects[0].allowed_decisions.is_empty());
        // 缺失 → 种类最小策略 ∩ 策略词（command = accept/cancel）
        let missing = p.project_with_policy(
            &[serde_json::json!({
                "id": 42, "method": "item/commandExecution/requestApproval",
                "params": { "command": "echo" }
            })],
            &caps,
            &DecisionPolicy::evidence(),
        );
        assert_eq!(missing[0].allowed_decisions, vec!["accept", "cancel"]);
        // 混合（字符串 + 对象）→ 只取字符串词
        let mixed = p.project_with_policy(
            &[serde_json::json!({
                "id": 43, "method": "item/commandExecution/requestApproval",
                "params": {
                    "command": "echo",
                    "availableDecisions": [
                        "accept",
                        {"acceptWithExecpolicyAmendment": {"execpolicy_amendment": []}},
                        "decline"
                    ]
                }
            })],
            &caps,
            &DecisionPolicy::evidence(),
        );
        assert_eq!(mixed[0].allowed_decisions, vec!["accept", "decline"]);
        // 文件缺失字段用**文件**最小策略（不套命令词）：只有 accept
        let file_missing = p.project_with_policy(
            &[serde_json::json!({
                "id": 44, "method": "item/fileChange/requestApproval",
                "params": { "changes": [{"path": "a.rs"}] }
            })],
            &caps,
            &DecisionPolicy::evidence(),
        );
        assert_eq!(file_missing[0].allowed_decisions, vec!["accept", "decline"]);
        // 文件摘要缺失 → 移除 accept，保留可靠拒绝/中止
        let no_summary = p.project_with_policy(
            &[serde_json::json!({
                "id": 45, "method": "item/fileChange/requestApproval",
                "params": { "threadId": "t", "turnId": "missing-turn", "itemId": "missing-item" }
            })],
            &caps,
            &DecisionPolicy::evidence(),
        );
        assert!(
            !no_summary[0]
                .allowed_decisions
                .contains(&"accept".to_string()),
            "摘要不足不得开放批准：{:?}",
            no_summary[0].allowed_decisions
        );
        assert!(no_summary[0]
            .allowed_decisions
            .contains(&"decline".to_string()));
        assert_eq!(no_summary[0].file_summary, None);
    }

    /// T05：`declined` 是真实命令终态（不当作仍在运行/未知），且不自动中断回合。
    #[test]
    fn f08p_t05_desktop_declined_is_terminal_without_interruption() {
        let state = serde_json::json!({
            "requests": [],
            "turnHistory": {"history": {
                "isComplete": true,
                "entitiesByKey": {"turn:t1": {
                    "turnId": "t1",
                    "status": "completed",
                    "items": [
                        {"type": "commandExecution", "id": "cmd-1",
                         "status": "declined", "command": "echo hi"}
                    ]
                }},
                "islands": [{"id": "i", "entries": [{"key": "turn:t1", "value": "turn:t1"}]}]
            }}
        });
        let resolution =
            super::super::pending::resolve_native_request(&state, &serde_json::json!(18), false);
        assert_eq!(resolution, NativeResolution::Ended);
        // 回合终态仍是 completed（decline 不等于 cancel/interrupt）。
        let turn = &state["turnHistory"]["history"]["entitiesByKey"]["turn:t1"];
        assert_eq!(turn["status"], "completed");
        let item_status = turn["items"][0]["status"].as_str();
        assert_eq!(item_status, Some("declined"));
    }
}

// ============================================================
// R09–R12：审批策略/形状/摘要完整性/内容 token
// ============================================================

#[cfg(test)]
mod r09_r12_tests {
    use super::*;
    use crate::services::codex_desktop_ipc::capability::{
        CodexOperation, EffectiveOperations, OperationContext, OperationPolicy,
    };

    fn caps_all() -> CapabilitySet {
        CapabilitySet {
            send: true,
            state: true,
            command: true,
            permissions: true,
            blocking_input: true,
            interrupt: false,
        }
    }

    fn ops_allowing() -> EffectiveOperations {
        let mut ctx = OperationContext {
            auto_adapt_enabled: true,
            target_fresh: true,
            ..Default::default()
        };
        ctx.shipped.insert(CodexOperation::CommandApproval, true);
        ctx.shipped.insert(CodexOperation::FileChangeApproval, true);
        OperationPolicy.evaluate(&ctx)
    }

    fn file_req(params: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "id": 41, "method": "item/fileChange/requestApproval", "params": params
        })
    }

    fn file_state(item: serde_json::Value) -> serde_json::Value {
        serde_json::json!({ "turnHistory": { "history": {
            "isComplete": true,
            "entitiesByKey": {"turn:t1": {
                "turnId": "t1", "status": "inProgress",
                "items": [item]
            }},
            "islands": [{"id": "i", "entries": [{"key": "turn:t1", "value": "turn:t1"}]}]
        }}})
    }

    fn project_file(
        params: serde_json::Value,
        state: serde_json::Value,
        policy: &DecisionPolicy,
    ) -> PendingView {
        let p = PendingProjector::new();
        p.project_with_operations(
            &[file_req(params)],
            &caps_all(),
            policy,
            &ops_allowing(),
            "th",
            &state,
        )
        .remove(0)
    }

    /// R10：**形状异常**（null/字符串/对象）不得进入"缺字段"兜底开放最小集。
    #[test]
    fn r10_invalid_decision_shapes_are_closed() {
        let policy = DecisionPolicy::evidence();
        for bad in [
            serde_json::json!({ "availableDecisions": null }),
            serde_json::json!({ "availableDecisions": "accept" }),
            serde_json::json!({ "availableDecisions": { "accept": true } }),
            serde_json::json!({ "availableDecisions": [] }),
            serde_json::json!({ "availableDecisions": [{ "acceptWithExecpolicyAmendment": {} }] }),
        ] {
            assert!(
                decision_words_for(PendingKind::CommandApproval, &policy, &bad).is_empty(),
                "异常形状不得开放决策：{bad}"
            );
        }
        // 真正缺字段 → 该种类已核验最小集
        assert_eq!(
            decision_words_for(
                PendingKind::CommandApproval,
                &policy,
                &serde_json::json!({})
            ),
            vec![DECISION_ACCEPT.to_string(), DECISION_CANCEL.to_string()]
        );
    }

    /// R09：文件审批用**本类** schema 词——缺 availableDecisions 时保留 decline，
    /// 不被命令侧最小集压成只剩 accept。
    #[test]
    fn r09_file_policy_keeps_schema_decline_when_words_missing() {
        let policy = DecisionPolicy::schema_with_words(
            vec![DECISION_ACCEPT.into(), DECISION_CANCEL.into()],
            vec![
                DECISION_ACCEPT.into(),
                DECISION_DECLINE.into(),
                DECISION_CANCEL.into(),
            ],
        );
        let state = file_state(serde_json::json!({
            "type": "fileChange", "id": "call-1",
            "changes": [{"path": "src/a.ts", "kind": "update"}]
        }));
        let view = project_file(
            serde_json::json!({ "turnId": "t1", "itemId": "call-1" }),
            state,
            &policy,
        );
        assert_eq!(
            view.allowed_decisions,
            vec![DECISION_ACCEPT.to_string(), DECISION_DECLINE.to_string()],
            "文件策略必须保留 decline"
        );
        assert!(view.actionable);
    }

    /// R09/R11：摘要不足（只有 kind / 空对象）→ 只剩 decline（保留可靠拒绝）。
    #[test]
    fn r09_r11_insufficient_summary_removes_accept_but_keeps_decline() {
        let policy = DecisionPolicy::schema_with_words(
            Vec::new(),
            vec![DECISION_ACCEPT.into(), DECISION_DECLINE.into()],
        );
        for changes in [
            serde_json::json!([{}]),
            serde_json::json!([{"kind": "update"}]),
        ] {
            let state = file_state(serde_json::json!({
                "type": "fileChange", "id": "call-1", "changes": changes
            }));
            let view = project_file(
                serde_json::json!({ "turnId": "t1", "itemId": "call-1" }),
                state,
                &policy,
            );
            assert_eq!(
                view.allowed_decisions,
                vec![DECISION_DECLINE.to_string()],
                "摘要无法说明改什么 → 不得批准：{changes}"
            );
        }
    }

    /// R11：数量字符串（`N changes`）不再能单独作为可批准摘要。
    #[test]
    fn r11_count_only_summary_is_not_approvable() {
        assert!(super::super::interaction_summary::safe_pending_summary(
            "item/fileChange/requestApproval",
            &serde_json::json!({ "changes": [{}, {}] })
        )
        .is_none());
        assert!(super::super::interaction_summary::safe_pending_summary(
            "item/fileChange/requestApproval",
            &serde_json::json!({ "changes": [{"path": "a.ts"}] })
        )
        .is_some());
    }

    /// R12：params 不变、关联 fileChange 内容变化 → token 必须换（旧卡失效）。
    #[test]
    fn r12_linked_file_change_content_rotates_token() {
        let policy = DecisionPolicy::evidence();
        let params = serde_json::json!({ "turnId": "t1", "itemId": "call-1" });
        let first = file_state(serde_json::json!({
            "type": "fileChange", "id": "call-1",
            "changes": [{"path": "src/a.ts", "kind": "update"}]
        }));
        let second = file_state(serde_json::json!({
            "type": "fileChange", "id": "call-1",
            "changes": [{"path": "src/b.ts", "kind": "update"}]
        }));
        let p = PendingProjector::new();
        let a = p
            .project_with_operations(
                &[file_req(params.clone())],
                &caps_all(),
                &policy,
                &ops_allowing(),
                "th",
                &first,
            )
            .remove(0);
        let b = p
            .project_with_operations(
                &[file_req(params)],
                &caps_all(),
                &policy,
                &ops_allowing(),
                "th",
                &second,
            )
            .remove(0);
        assert_ne!(
            a.context_token, b.context_token,
            "关联变更内容变化必须换 token（旧页面不得批准新内容）"
        );
    }
}

#[cfg(test)]
mod n09_canonical_unsafe_tests {
    use super::*;
    use crate::services::codex_desktop_ipc::capability::{
        ContractMatch, EffectiveOperations, EvidenceSource, OperationStatus,
    };
    use serde_json::json;

    fn ops_allowing_file() -> EffectiveOperations {
        let mut operations = EffectiveOperations::unknown();
        operations.operations.insert(
            crate::services::codex_desktop_ipc::capability::CodexOperation::FileChangeApproval,
            OperationStatus {
                implemented: true,
                allowed: true,
                requires_auto_adapt: true,
                evidence: EvidenceSource::ContractMatched,
                support: crate::services::codex_desktop_ipc::capability::SupportState::Supported,
                support_inherited: false,
                support_identity_verified: false,
                reason_code: None,
            },
        );
        let _ = ContractMatch {
            method: String::new(),
            base_version: 1,
            request_rule: String::new(),
            response_rule: String::new(),
            semantic_rule: String::new(),
        };
        operations
    }

    /// N09：canonical 已关联但不安全（a.rs + 空对象）→ 即使 params 子集合法，
    /// 也不得回退成可批准摘要。
    #[test]
    fn n09_canonical_unsafe_is_not_overridden_by_params_subset() {
        let state = json!({ "turnHistory": { "history": {
            "isComplete": true,
            "entitiesByKey": {"turn:t1": {
                "turnId": "t1", "status": "completed",
                "items": [{
                    "type": "fileChange", "id": "item-1",
                    "changes": [{"path": "a.rs"}, {}]
                }]
            }},
            "islands": [{"id": "i", "entries": [{"key": "turn:t1", "value": "turn:t1"}]}]
        }}});
        let views = PendingProjector::new().project_with_operations(
            &[json!({
                "id": 71,
                "method": "item/fileChange/requestApproval",
                "params": {
                    "threadId": "th", "turnId": "t1", "itemId": "item-1",
                    "changes": [{"path": "a.rs"}]
                }
            })],
            &crate::services::codex_desktop_ipc::types::CapabilitySet {
                send: true,
                state: true,
                command: true,
                permissions: false,
                blocking_input: false,
                interrupt: false,
            },
            &DecisionPolicy::evidence(),
            &ops_allowing_file(),
            "th",
            &state,
        );
        let view = &views[0];
        assert!(!view.allowed_decisions.iter().any(|word| word == "accept"));
        assert_eq!(view.file_summary, None);
    }
}
