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
    CapabilitySet, PendingKind, PendingRequestId, PendingView, ProtocolError, QuestionOption,
    QuestionView,
};
use std::collections::{HashMap, HashSet};

/// token 缓存条目：内容指纹 + 当前 token。
#[derive(Clone)]
struct TokenEntry {
    fingerprint: u64,
    token: String,
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
    pub fn project_with_policy(
        &self,
        requests: &[serde_json::Value],
        caps: &CapabilitySet,
        policy: &DecisionPolicy,
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
            .filter_map(|req| project_one(req, caps, policy, &mut tokens))
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
            let cap_ok = caps.command;
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
                // 真机参数形状尚未取证（见 T7 记录）：只做**安全的展示摘要**，
                // 认不出就 None（绝不 dump raw 参数）
                file_change_summary(&params),
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
            let cap_ok = caps.blocking_input;
            let questions = params
                .get("questions")
                .and_then(|q| q.as_array())
                .map(|arr| arr.iter().filter_map(question_view).collect())
                .unwrap_or_default();
            (
                PendingKind::UserInput,
                cap_ok,
                // 同上：已知方法的未验证能力 → unverified
                (!cap_ok).then(|| "capability_unverified".to_string()),
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
    let allowed_decisions = match (policy.source(), kind) {
        (PolicySource::Schema, PendingKind::CommandApproval | PendingKind::FileChangeApproval) => {
            match string_decision_words(&params) {
                Some(request_words) => {
                    intersect_decision_words(policy.for_kind(kind), &request_words)
                }
                None => policy.for_kind(kind).to_vec(),
            }
        }
        // 证据档 / 权限 / 问答 / 未知类别：沿用策略词，绝不因 availableDecisions 开放
        _ => policy.for_kind(kind).to_vec(),
    };
    let actionable = match kind {
        // 命令/文件审批：能给出决策词才算可操作（未知 build 未取证 = 不可操作）
        PendingKind::CommandApproval | PendingKind::FileChangeApproval => {
            !allowed_decisions.is_empty()
        }
        // 权限/问答/未知：沿用能力门（问答走 Answer 动作，不是决策词）
        _ => actionable_caps,
    };
    let disabled = if actionable { None } else { disabled };

    // token：同 request 同 method 同内容 → 复用；内容/方法变化、重现 → 换新
    let fp = fingerprint(method, &params);
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

/// 文件变更审批的展示摘要（**只做安全摘要**：认不出就 `None`，绝不 dump raw 参数）。
///
/// 真机参数形状尚未取证（2026-10-21 抓到的 pending 是计划实现审批，不是 file 变更），
/// 所以这里只认 `changes` 数组的**长度**（最保守的可用信息）；字段名不符 → 无摘要。
fn file_change_summary(params: &serde_json::Value) -> Option<String> {
    let n = params.get("changes")?.as_array()?.len();
    (n > 0).then(|| n.to_string())
}

/// 从 pending `params.availableDecisions` 里取**字符串词**（T7-R2）。
///
/// 真机形态（26.1016.6573.0）：`["accept", {acceptWithExecpolicyAmendment: {…}},
/// "cancel"]`——字符串词与对象形态混排，且**没有** `decline` 字面量。对象形态属
/// 执行策略修订（权限扩大），**一律跳过**；本函数只收 `Value::String`，保持原顺序
/// 并去重。字段缺失 / 非数组 / 没有任何字符串词 → `None`（调用方回退策略词）。
fn string_decision_words(params: &serde_json::Value) -> Option<Vec<String>> {
    let available = params.get("availableDecisions")?.as_array()?;
    let mut seen: HashSet<&str> = HashSet::new();
    let mut words = Vec::new();
    for value in available {
        if let Some(word) = value.as_str() {
            if seen.insert(word) {
                words.push(word.to_string());
            }
        }
    }
    (!words.is_empty()).then_some(words)
}

/// 请求词 ∩ 策略词（保持**请求词顺序**）；交集为空 → 策略词兜底。
///
/// 交集保证 schema 档不会发出 schema 未取证的词，也保证 pending 没提供的词
/// （例如命令侧的 `decline`）不会凭空出现。
fn intersect_decision_words(policy_words: &[String], request_words: &[String]) -> Vec<String> {
    let picked: Vec<String> = request_words
        .iter()
        .filter(|word| policy_words.contains(word))
        .cloned()
        .collect();
    if picked.is_empty() {
        policy_words.to_vec()
    } else {
        picked
    }
}

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
            command: vec![DECISION_ACCEPT.to_string(), DECISION_DECLINE.to_string()],
            file: vec![DECISION_ACCEPT.to_string(), DECISION_DECLINE.to_string()],
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
        is_other: q.get("isOther").and_then(|b| b.as_bool()).unwrap_or(false),
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
    })
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
        }
    }

    fn caps_unknown_build() -> CapabilitySet {
        CapabilitySet {
            send: false,
            state: true,
            command: false,
            permissions: false,
            blocking_input: false,
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
        assert!(v.actionable, "证据档下文件审批可远程决策");
        assert_eq!(v.allowed_decisions, vec!["accept", "decline"]);
        assert_eq!(
            v.file_summary.as_deref(),
            Some("3"),
            "只给条数，不 dump 变更内容"
        );
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
        assert_eq!(views[1].allowed_decisions, vec!["accept", "cancel"]);
        assert!(views[0].actionable && views[1].actionable);
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
        assert_eq!(
            v.disabled_reason_code.as_deref(),
            Some("capability_unverified")
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
        // 拆码契约：方法已知但能力未验证 → capability_unverified（非 unavailable——
        // 那是「方法未知」的码；前端据此显示「当前 Desktop 版本未验证该操作」）
        assert_eq!(
            v.disabled_reason_code.as_deref(),
            Some("capability_unverified")
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

    /// pending 没带 `availableDecisions`（或没有字符串词）→ 回退 schema 档词。
    #[test]
    fn missing_available_decisions_falls_back_to_schema() {
        let p = PendingProjector::new();
        let views = p.project_with_policy(
            &[serde_json::json!({
                "id": 35,
                "method": "item/fileChange/requestApproval",
                "params": { "reason": "r", "changes": [{}] }
            })],
            &caps_unknown_build(),
            &DecisionPolicy::schema(true, true),
        );
        let v = &views[0];
        assert!(v.actionable);
        assert_eq!(v.allowed_decisions, vec!["accept", "cancel"]);
    }

    /// 证据档（已知 build）**不读** `availableDecisions`：仍是 accept + decline
    /// （`decline` 已在 4894 真机验证被接受）。
    #[test]
    fn evidence_policy_ignores_available_decisions() {
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
            vec!["accept", "decline"],
            "证据档不得因 availableDecisions 把 reject 词换成 cancel"
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

    /// 纯函数：只收字符串词、保序、去重；对象/空数组/缺失 → None。
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
            Some(vec!["cancel".to_string(), "accept".to_string()])
        );
        assert_eq!(string_decision_words(&serde_json::json!({})), None);
        assert_eq!(
            string_decision_words(&serde_json::json!({ "availableDecisions": [] })),
            None
        );
        assert_eq!(
            string_decision_words(&serde_json::json!({
                "availableDecisions": [{ "acceptWithExecpolicyAmendment": {} }]
            })),
            None
        );
    }
}
