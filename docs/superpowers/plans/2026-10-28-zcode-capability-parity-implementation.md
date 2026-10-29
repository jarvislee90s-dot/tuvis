# ZCode 远程能力对齐与 build 兼容 · Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把 ZCode 八类远程动作改为 build 级静默能力解析；未知 build 接口匹配即候选开放，首次真实动作确认；手机冲突态可断开抢回。

**Architecture:** 新增 `capability` 纯核 + DB 证据账本 + bridge 接口形状采集 + resolver；resolver 同时驱动 `CapabilityView` 与执行闸。首次真实动作由事件泵/投影判据确认，不额外发探针消息。手机抢回复用现有 `disconnect`/`connect_requested`。

**Tech Stack:** Rust/Tauri 2、tokio、rusqlite、React 19/TypeScript、vitest。

**Spec:** `docs/superpowers/specs/2026-10-28-zcode-capability-parity-spec.md`

## Global Constraints

- 执行分支：`codex/zcode-unified-remote`（ZCode 实现分支）；不要从 `probe/dsh-native-rpc-macos` 或本地落后的 `main` 开工。
- 不改 `docs/MASTER-PLAN.md`；不 push/merge/tag。
- 不访问真实凭据/真实官方账号；测试用 fake/loopback；真机项单独登记。
- 每任务一个 commit；不 amend 已审 commit。
- 每个任务先写失败测试并确认 RED，再最小实现。
- 能力判断不得在 UI 侧自行推断；UI 只消费 `CapabilityView`。
- 不向用户展示探测页/探测按钮/兼容徽标；不后台发测试消息。
- 不把 JWT、链接、正文、题目、选项、答案写入账本/日志/手机 DTO。
- 上传文件：原不在本计划范围；**2026-10-29 用户确认修订**——接入通用附件上传（正文引用），官方原生附件协议仍不在范围。

## Task Dependencies

```text
T1 (pure core)
 ├─ T2 (ledger)
 └─ T3 (bridge evidence)
      T2 + T3
        └─ T4 (resolver + gates)
             ├─ T5 (confirm: send/model/thought)
             │    ├─ T6 (confirm: pending answers/approval)
             │    └─ T7 (confirm: interrupt)
             ├─ T8 (mobile takeover)
             └─ T9 (UI + e2e + docs)
```

T2/T3 can be done in either order after T1. Do not start T5/T6/T7 before T4 is green. T8 can start after T4; T9 last.

---

### Task 1: Capability 纯核（key/state/interface/shipped）

**Goal:** 建立无 I/O 的能力类型、key、状态、shipped 表和接口候选纯函数。

**Depends on:** none.

**Acceptance:**
- 八项能力与 `CapabilityView` 字段一一对应；
- key 任一字段变化产生不同 key；
- shipped 精确 build 才命中；
- 接口形状纯函数对未知 build 返回候选，缺方法/缺形状返回 unsupported；
- 无网络/无 DB 单测全绿。

**Files:**
- Create: `src-tauri/src/services/zcode_control/capability/mod.rs`
- Create: `src-tauri/src/services/zcode_control/capability/key.rs`
- Create: `src-tauri/src/services/zcode_control/capability/state.rs`
- Create: `src-tauri/src/services/zcode_control/capability/interface.rs`
- Test: inline `#[cfg(test)]` modules
- Modify: `src-tauri/src/services/zcode_control/mod.rs`（`pub(crate) mod capability;`）

**Interfaces:**
```rust
pub(crate) enum Capability {
    Send, SingleSelect, MultiSelect, FreeText,
    Interrupt, ApprovalReply, Model, ThoughtLevel,
}

pub(crate) struct CapabilityKey {
    pub platform: String,
    pub app_build: String,
    pub protocol_version: u32,
    pub schema_fingerprint: String,
    pub probe_epoch: u32,
}

pub(crate) enum CapabilityState {
    Unsupported, InterfaceMatched, Confirmed, Degraded, Inconclusive,
}

pub(crate) struct ZcodeInterfaceEvidence {
    pub method_send: bool,
    pub method_resolve_interaction: bool,
    pub method_stop: bool,
    pub method_selection_get: bool,
    pub method_selection_switch: bool,
    pub has_user_input_pending: bool,
    pub has_multi_select: bool,
    pub has_free_text: bool,
    pub has_permission_pending: bool,
    pub has_active_work: bool,
    pub has_model_catalog: bool,
    pub has_thought_levels: bool,
}

pub(crate) fn interface_capabilities(
    evidence: &ZcodeInterfaceEvidence,
) -> BTreeMap<Capability, bool>;

pub(crate) fn shipped_state(
    key: &CapabilityKey,
    capability: Capability,
) -> Option<CapabilityState>;
```

- [ ] **Step 1: Write failing tests**

```rust
#[test]
fn key_changes_when_build_or_epoch_changes() {
    let a = CapabilityKey::new("macos", "3.16.2.8769", 3, "shape-a", 1);
    let b = CapabilityKey::new("macos", "3.16.2.9999", 3, "shape-a", 1);
    let c = CapabilityKey::new("macos", "3.16.2.8769", 3, "shape-a", 2);
    assert_ne!(a, b);
    assert_ne!(a, c);
}

#[test]
fn shipped_only_matches_exact_build() {
    let known = CapabilityKey::new("macos", "3.16.2.8769", 3, "shape", 1);
    let unknown = CapabilityKey::new("macos", "3.16.2.9999", 3, "shape", 1);
    assert_eq!(shipped_state(&known, Capability::Send), Some(CapabilityState::Confirmed));
    assert_eq!(shipped_state(&unknown, Capability::Send), None);
}

#[test]
fn interface_requires_method_and_shape() {
    let mut e = ZcodeInterfaceEvidence::default();
    assert_eq!(interface_capabilities(&e).get(&Capability::Send), Some(&false));
    e.method_send = true;
    assert_eq!(interface_capabilities(&e).get(&Capability::Send), Some(&true));
    e.has_user_input_pending = true;
    e.has_multi_select = true;
    assert_eq!(
        interface_capabilities(&e).get(&Capability::MultiSelect),
        Some(&true)
    );
}
```

- [ ] **Step 2: Run RED**

Run: `cd src-tauri && cargo test --lib services::zcode_control::capability`
Expected: FAIL（模块/符号不存在）。

- [ ] **Step 3: Implement minimal core**

- `Capability::as_str()` 八项固定字符串；
- `CapabilityKey::new` 直接构造；`Debug/Clone/PartialEq/Eq/Hash`；
- `shipped_state` 只认 `("macos"|"windows", "3.16.2.8769", 3)`；按 spec §6.3 返回；
- `interface_capabilities` 按 spec §6.4 判定；
- `ZcodeInterfaceEvidence` 实现 `Default`，**不得**存正文。

- [ ] **Step 4: Run GREEN**

Run: `cd src-tauri && cargo test --lib services::zcode_control::capability`
Expected: PASS。

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/services/zcode_control/capability src-tauri/src/services/zcode_control/mod.rs
git commit -m "feat(zcode): add capability core and shipped profiles"
```

---

### Task 2: Capability DB 账本

**Goal:** 每个 `(key, capability)` 持久化 confirmed/degraded/inconclusive，失败可降级且不阻塞连接。

**Depends on:** T1.

**Acceptance:**
- migration 幂等，重复启动不报错；
- upsert 覆盖同一 key+capability，不产生多行；
- 读失败/缺表返回空账本，不 panic；
- 账本不存正文/JWT/链接；
- key 变化不命中旧记录。

**Files:**
- Modify: `src-tauri/src/database/schema.rs`
- Create: `src-tauri/src/database/dao/zcode_capability.rs`
- Modify: `src-tauri/src/database/dao/mod.rs`
- Test: inline DAO tests

**Interfaces:**
```rust
pub(crate) struct CapabilityRecord {
    pub capability: Capability,
    pub state: CapabilityState,
    pub reason_code: Option<String>,
    pub evidence: Option<String>,
}

pub(crate) trait CapabilityLedger: Send + Sync {
    fn get(&self, key: &CapabilityKey) -> Vec<CapabilityRecord>;
    fn put(&self, key: &CapabilityKey, record: &CapabilityRecord) -> Result<(), String>;
}

pub(crate) struct DbCapabilityLedger { /* DeviceStore */ }
```

- [ ] **Step 1: Write failing tests**

```rust
#[test]
fn upsert_is_idempotent_and_overwrites_state() {
    let store = DeviceStore::memory();
    let ledger = DbCapabilityLedger::new(store.clone());
    let key = caps_key();
    ledger.put(&key, &CapabilityRecord::confirmed(Capability::Send)).unwrap();
    ledger.put(&key, &CapabilityRecord::degraded(Capability::Send, "shape_drift")).unwrap();
    let rows = ledger.get(&key);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].state, CapabilityState::Degraded);
}

#[test]
fn missing_table_is_empty_not_panic() {
    let broken = DeviceStore::Owned(Arc::new(Mutex::new(
        rusqlite::Connection::open_in_memory().unwrap(),
    )));
    assert!(DbCapabilityLedger::new(broken).get(&caps_key()).is_empty());
}
```

- [ ] **Step 2: Run RED**

Run: `cd src-tauri && cargo test --lib zcode_capability`
Expected: FAIL（DAO/类型不存在）。

- [ ] **Step 3: Implement schema + DAO**

SQL:
```sql
CREATE TABLE IF NOT EXISTS zcode_capability_evidence (
    key TEXT NOT NULL,
    capability TEXT NOT NULL,
    state TEXT NOT NULL,
    reason_code TEXT,
    evidence TEXT,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (key, capability)
);
```

- `CapabilityKey::to_storage_key()` 只含字段，不含凭据；
- `get` 读失败返回 `Vec::new()`；
- `put` 使用 `INSERT ... ON CONFLICT(key,capability) DO UPDATE`；
- 时间用 `SystemTime::now().duration_since(UNIX_EPOCH)`，不依赖本地时区。

- [ ] **Step 4: Run GREEN**

Run: `cd src-tauri && cargo test --lib zcode_capability`
Expected: PASS。

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/database/schema.rs src-tauri/src/database/dao/zcode_capability.rs src-tauri/src/database/dao/mod.rs
git commit -m "feat(zcode): add capability evidence ledger"
```

---

### Task 3: Bridge 接口形状采集

**Goal:** 从真实 hello/snapshot/pending/selection/control 采集形状，不存正文；断连/invalidate 清空。

**Depends on:** T1.

**Acceptance:**
- hello 记录协议/方法存在性；
- pending 记录 kind/multiSelect/freeText；
- control 记录 activeWorks；
- selection 记录目录/档位；
- invalidate 全清；
- 测试断言账本/证据无题目、选项、命令、答案。

**Files:**
- Modify: `src-tauri/src/services/zcode_control/types.rs`
- Modify: `src-tauri/src/services/zcode_control/bridge.rs`
- Test: `bridge.rs` inline tests

**Interfaces:**
```rust
pub(crate) struct ZcodeInterfaceEvidence { /* T1 字段 */ }

impl HostEvidence {
    pub(crate) fn interface_evidence(&self) -> ZcodeInterfaceEvidence;
}
```

- [ ] **Step 1: Write failing tests**

```rust
#[test]
fn pending_shape_records_multi_and_free_text() {
    let mut ev = ZcodeInterfaceEvidence::default();
    ev.observe_pending(json!({
        "kind": "user_input",
        "questions": [{ "multiSelect": true, "isOther": true }]
    }));
    assert!(ev.has_user_input_pending);
    assert!(ev.has_multi_select);
    assert!(ev.has_free_text);
}

#[test]
fn invalidate_clears_interface_evidence() {
    let mut ev = ZcodeInterfaceEvidence::default();
    ev.method_send = true;
    ev.has_active_work = true;
    ev.invalidate();
    assert!(!ev.method_send);
    assert!(!ev.has_active_work);
}
```

- [ ] **Step 2: Run RED**

Run: `cd src-tauri && cargo test --lib services::zcode_control::bridge`
Expected: FAIL（字段/方法不存在）。

- [ ] **Step 3: Implement**

- `HostEvidence` 增加 `interface: ZcodeInterfaceEvidence`（或旁挂并同步）；
- 在 hello 校验处置 method flags；
- snapshot/pending/selection/control 解析处调用 `observe_*`；
- `invalidate` 清空；
- 禁止把 `command`/`questions[].question`/`options`/`answers` 存进 evidence。

- [ ] **Step 4: Run GREEN**

Run: `cd src-tauri && cargo test --lib services::zcode_control::bridge`
Expected: PASS。

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/services/zcode_control/types.rs src-tauri/src/services/zcode_control/bridge.rs
git commit -m "feat(zcode): collect interface evidence from bridge"
```

---

### Task 4: Resolver + 统一下发/执行闸

**Goal:** 用 resolver 替换生产全关策略；UI 与执行闸同源；异常回退全关。

**Depends on:** T2, T3.

**Acceptance:**
- 已知 build shipped confirmed 能力可用；
- 未知 build 接口候选可用；缺形状不可用；
- ledger confirmed/degraded/inconclusive 正确覆盖；
- resolver 失败回退 `production_unverified()`；
- `session_capabilities`、`check_policy`、`CommandSender` 使用同一结果。

**Files:**
- Create: `src-tauri/src/services/zcode_control/capability/resolver.rs`
- Modify: `src-tauri/src/services/zcode_control/types.rs`
- Modify: `src-tauri/src/services/zcode_control/hub.rs`
- Modify: `src-tauri/src/services/zcode_control/command.rs`
- Test: inline resolver/hub tests

**Interfaces:**
```rust
pub(crate) fn resolve_capabilities(
    key: &CapabilityKey,
    evidence: &ZcodeInterfaceEvidence,
    ledger: &dyn CapabilityLedger,
) -> CapabilityView;
```

- [ ] **Step 1: Write failing tests**

```rust
#[test]
fn unknown_build_opens_only_interface_matched() {
    let key = CapabilityKey::new("macos", "9.9.9.9999", 3, "shape", 1);
    let mut ev = ZcodeInterfaceEvidence::default();
    ev.method_send = true;
    let view = resolve_capabilities(&key, &ev, &MemoryLedger::default());
    assert!(view.send.enabled);
    assert!(!view.approval_reply.enabled);
}

#[test]
fn degraded_overrides_interface_candidate() {
    let key = CapabilityKey::new("macos", "9.9.9.9999", 3, "shape", 1);
    let mut ev = ZcodeInterfaceEvidence::default();
    ev.method_send = true;
    let ledger = MemoryLedger::with(key.clone(), Capability::Send, CapabilityState::Degraded);
    let view = resolve_capabilities(&key, &ev, &ledger);
    assert!(!view.send.enabled);
}
```

- [ ] **Step 2: Run RED**

Run: `cd src-tauri && cargo test --lib services::zcode_control::capability`
Expected: FAIL（resolver 不存在）。

- [ ] **Step 3: Implement**

- `resolve_capabilities` 按 `Capability` 循环；
- shipped/ledger/interface 优先级按 spec §6.4；
- `CapabilityView` reasonCode：不可用 `needs_evidence` 或 `unsupported`；可用 None；
- `PartsBackend::production` 的 policy source 改为捕获 `HostEvidence` + `DbCapabilityLedger` 的 resolver；
- `session_capabilities` 与 `check_policy` 走同一 source；
- 保持 `production_unverified()` fallback。

- [ ] **Step 4: Run GREEN**

Run: `cd src-tauri && cargo test --lib services::zcode_control::hub`
Expected: PASS；现有 68+ 用例不回归。

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/services/zcode_control/capability/resolver.rs src-tauri/src/services/zcode_control/types.rs src-tauri/src/services/zcode_control/hub.rs src-tauri/src/services/zcode_control/command.rs
git commit -m "feat(zcode): resolve capabilities from build evidence"
```

---

### Task 5: 首次确认基础设施 + send/model/thought

**Goal:** 候选能力首次真实使用时，ACK + 终端可见性确认；不重发、不伪造。

**Depends on:** T4.

**Acceptance:**
- ACK-only 不升级；
- send 需 revision 前进 + 新用户消息可见；
- model/thought 需新 snapshot 精确相等；
- 超时 5s → inconclusive，不重发；
- 旧代际/旧 revision 丢弃；
- 明确失败 → degraded，只影响该 key+capability。

**Files:**
- Create: `src-tauri/src/services/zcode_control/capability/confirm.rs`
- Modify: `src-tauri/src/services/zcode_control/hub.rs`
- Modify: `src-tauri/src/services/zcode_control/bridge.rs`（user message fingerprint/selection）
- Test: confirm + hub inline

**Interfaces:**
```rust
pub(crate) struct ConfirmationContext {
    pub capability: Capability,
    pub key: CapabilityKey,
    pub session_id: String,
    pub generation: u64,
    pub baseline_revision: u64,
    pub target: ConfirmationTarget,
    pub deadline: Instant,
}

pub(crate) enum ConfirmationTarget {
    UserMessage,
    Selection { provider: String, model: String, thought: String },
}

pub(crate) enum ConfirmationOutcome { Confirmed, Inconclusive, Degraded }

// 纯判据输入（由 bridge/测试构造；不含正文）
pub(crate) struct SendObservation {
    pub generation: u64,
    pub revision: u64,
    pub last_user_message_id: String,
}
pub(crate) struct SelectionObservation {
    pub generation: u64,
    pub revision: u64,
    pub provider: String,
    pub model: String,
    pub thought: String,
}
```

测试模块按以下签名实现辅助函数（不是生产接口）：
```rust
fn send_ctx(generation: u64, revision: u64) -> ConfirmationContext;
fn send_obs(generation: u64, revision: u64, last_user: &str) -> SendObservation;
fn selection_ctx(model: &str) -> ConfirmationContext;
fn selection_obs(model: &str) -> SelectionObservation;
```

- [ ] **Step 1: Write failing tests**

```rust
#[test]
fn ack_alone_does_not_confirm_send() {
    let ctx = send_ctx(7, 10);
    let before = send_obs(7, 10, "u0");
    assert_eq!(confirm_send(&ctx, &before), ConfirmationOutcome::Inconclusive);
}

#[test]
fn new_user_message_and_revision_confirms_send() {
    let ctx = send_ctx(7, 10);
    let after = send_obs(7, 11, "u1");
    assert_eq!(confirm_send(&ctx, &after), ConfirmationOutcome::Confirmed);
}

#[test]
fn wrong_model_snapshot_is_not_confirmed() {
    let ctx = selection_ctx("GLM-5.3");
    let after = selection_obs("GLM-5.3-Flash");
    assert_eq!(confirm_selection(&ctx, &after), ConfirmationOutcome::Inconclusive);
}
```

- [ ] **Step 2: Run RED**

Run: `cd src-tauri && cargo test --lib services::zcode_control::capability`
Expected: FAIL（confirm 不存在）。

- [ ] **Step 3: Implement**

- 纯判据函数按 spec §5；
- `ConfirmationTracker` 存在 Hub，提交前登记；事件泵的新 Session 调用 `observe`；
- 超时由有界定时器/下一次事件触发，不重发；
- 成功写 ledger confirmed；明确失败 degraded；超时 inconclusive；旧代际丢弃；
- 日志只写 capability/key/state，不写正文/答案/链接。

- [ ] **Step 4: Run GREEN**

Run: `cd src-tauri && cargo test --lib services::zcode_control::hub`
Expected: PASS。

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/services/zcode_control/capability/confirm.rs src-tauri/src/services/zcode_control/hub.rs src-tauri/src/services/zcode_control/bridge.rs
git commit -m "feat(zcode): confirm send and selection on first real use"
```

---

### Task 6: 首次确认 single/multi/free/approval

**Goal:** pending 卡式能力首次真实作答/审批后确认或降级。

**Depends on:** T5.

**Acceptance:**
- 目标 pending 消失 + revision 前进才确认；
- 无 pending 不出卡/不开放；
- multi/free 只在形状存在时候选；
- approval 只确认既有卡，不把“发送触发审批卡”当能力；
- 超时不重发。

**Files:**
- Modify: `src-tauri/src/services/zcode_control/capability/confirm.rs`
- Modify: `src-tauri/src/services/zcode_control/hub.rs`
- Modify: `src-tauri/src/services/zcode_control/bridge.rs`
- Test: inline

**Interfaces:**
```rust
ConfirmationTarget::Pending { request_key: String }
fn confirm_pending(ctx: &ConfirmationContext, after: &SessionView) -> ConfirmationOutcome
```

测试模块辅助：
```rust
fn pending_ctx(request_key: &str, revision: u64) -> ConfirmationContext;
fn session_with(revision: u64, pending: Vec<PendingView>) -> SessionView;
```

- [ ] **Step 1: Write failing tests**

```rust
#[test]
fn pending_cleared_and_revision_advanced_confirms_answer() {
    let ctx = pending_ctx("req-1", 5);
    let after = session_with(6, vec![]);
    assert_eq!(confirm_pending(&ctx, &after), ConfirmationOutcome::Confirmed);
}

#[test]
fn same_pending_does_not_confirm() {
    let ctx = pending_ctx("req-1", 5);
    let after = session_with(5, vec![pending("req-1")]);
    assert_eq!(confirm_pending(&ctx, &after), ConfirmationOutcome::Inconclusive);
}
```

- [ ] **Step 2: Run RED**

Run: `cd src-tauri && cargo test --lib services::zcode_control::hub`
Expected: FAIL（pending target 未实现）。

- [ ] **Step 3: Implement**

- 提交 Answer/Permission 前捕获 `requestKey`/baseline revision；
- 事件泵/Session 更新调用 `confirm_pending`；
- 只确认目标 requestKey 消失，不把旁边的卡消失当成功；
- degradation 只写该 capability。

- [ ] **Step 4: Run GREEN**

Run: `cd src-tauri && cargo test --lib services::zcode_control::hub`
Expected: PASS。

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/services/zcode_control/capability/confirm.rs src-tauri/src/services/zcode_control/hub.rs src-tauri/src/services/zcode_control/bridge.rs
git commit -m "feat(zcode): confirm pending answers and approval on first use"
```

---

### Task 7: 首次确认 interrupt

**Goal:** interrupt 首次真实停止后确认；ACK-only 不升级。

**Depends on:** T5.

**Acceptance:**
- 目标 guard 从 activeWorks 消失 + 停止终态才确认；
- 初始 control 未确认/无守卫 → 不触发/不确认；
- 超时/unknown → inconclusive，不重发；
- 不把其它回合停止当成功。

**Files:**
- Modify: `capability/confirm.rs`
- Modify: `hub.rs`
- Modify: `bridge.rs`
- Test: inline

**Interfaces:**
```rust
ConfirmationTarget::Interrupt { guard_id: String }
fn confirm_interrupt(ctx: &ConfirmationContext, after: &ControlView) -> ConfirmationOutcome
```

测试模块辅助：
```rust
fn interrupt_ctx(guard_id: &str, revision: u64) -> ConfirmationContext;
fn control_with(revision: u64, active_works: Vec<String>) -> ControlView;
```

- [ ] **Step 1: Write failing tests**

```rust
#[test]
fn guard_still_active_is_not_confirmed() {
    let ctx = interrupt_ctx("work-1", 2);
    let after = control_with(2, vec!["work-1".into()]);
    assert_eq!(confirm_interrupt(&ctx, &after), ConfirmationOutcome::Inconclusive);
}

#[test]
fn guard_cleared_confirms_interrupt() {
    let ctx = interrupt_ctx("work-1", 2);
    let after = control_with(3, vec![]);
    assert_eq!(confirm_interrupt(&ctx, &after), ConfirmationOutcome::Confirmed);
}
```

- [ ] **Step 2: Run RED**

Run: `cd src-tauri && cargo test --lib services::zcode_control::hub`
Expected: FAIL。

- [ ] **Step 3: Implement**

- Stop 前捕获 `expectedForegroundExecutionId`；
- 事件泵更新 control 时匹配 guard 消失 + revision；
- 不匹配的停止结果丢弃；超时/unknown 保持候选。

- [ ] **Step 4: Run GREEN**

Run: `cd src-tauri && cargo test --lib services::zcode_control::hub`
Expected: PASS。

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/services/zcode_control/capability/confirm.rs src-tauri/src/services/zcode_control/hub.rs src-tauri/src/services/zcode_control/bridge.rs
git commit -m "feat(zcode): confirm interrupt on first real stop"
```

---

### Task 8: 手机断开并抢回

**Goal:** 冲突态手机可一次断开+重连，TCP 配对门内，无 OAuth。

**Depends on:** T4（Hub 状态稳定）；可与 T5-T7 并行，但 T9 前必须完成。

**Acceptance:**
- `POST /m/api/v1/zcode/takeover` 未配对 403；
- paired 幂等返回当前；
- conflict → disconnect→connect 一次；成功 paired；
- 未授权返回 needs_auth，零 OAuth；
- 失败不循环；手机按钮只在 conflict 出现；
- JWT/链接不出现在响应。

**Files:**
- Modify: `src-tauri/src/services/zcode_control/hub.rs`
- Modify: `src-tauri/src/remote/zcode.rs`
- Modify: `src-tauri/src/remote/server.rs`
- Modify: `src/mobile/api.ts`
- Modify: `src/mobile/ZcodeControlPanel.tsx`
- Modify: `src/i18n/locales/zh.json`, `en.json`
- Test: `src-tauri/src/remote/zcode.rs`（handler）、`src-tauri/src/services/zcode_control/hub.rs`、`tests/mobile/ZcodeControlPanel.test.tsx`

**Interfaces:**
```rust
// Hub
pub async fn takeover_requested(&self) -> ZResult<ConnectionView>;

// HTTP
POST /m/api/v1/zcode/takeover -> 200 ConnectionView | {error: code}

// TS（src/mobile/api.ts）
export const ZCODE_TAKEOVER_PATH = "/m/api/v1/zcode/takeover";
export async function postZcodeTakeover(): Promise<ConnectionView>;
```

- [ ] **Step 1: Write failing tests**

测试模块辅助（Hub 测试内）：
```rust
fn conflict_view() -> ConnectionView {
    ConnectionView { state: ConnectionState::Conflict, generation: 1, reason_code: Some(codes::CONFLICT.into()), device_label: None }
}
```

```rust
#[tokio::test]
async fn takeover_conflict_disconnects_then_connects_once() {
    let fake = Arc::new(FakeBackend::default());
    fake.jwt.store(true, Ordering::SeqCst);
    fake.auth_state.lock().unwrap().authorized = true;
    let hub = fake_hub(fake.clone(), stored(URL));
    hub.mark_authorized_for_test();
    fake.set_connect(Ok(conflict_view()));
    let first = hub.connect().await.unwrap();
    assert_eq!(first.state, ConnectionState::Conflict);

    fake.set_connect(Ok(paired_view()));
    let view = hub.takeover_requested().await.unwrap();
    assert!(view.is_paired());
    assert_eq!(fake.disconnect_calls.load(Ordering::SeqCst), 2);
    assert_eq!(fake.connect_calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn takeover_requires_auth_without_oauth() {
    let fake = Arc::new(FakeBackend::default());
    let hub = fake_hub(fake.clone(), stored(URL));
    let err = hub.takeover_requested().await.unwrap_err();
    assert!(err.is(codes::NEEDS_AUTH));
    assert_eq!(fake.authorize_calls.load(Ordering::SeqCst), 0);
}
```

Frontend（`tests/mobile/ZcodeControlPanel.test.tsx`，沿用该文件 fetch mock）：
```tsx
it("conflict shows takeover and posts endpoint once", async () => {
  mockBackend(saved, { status: connection({ state: "conflict", reasonCode: "conflict" }) });
  render(<ZcodeControlPanel sessionId={SID} />);
  fireEvent.click(await screen.findByTestId("zcode-takeover"));
  await waitFor(() =>
    expect(
      fetchMock.mock.calls.filter(([url]) => String(url).endsWith(ZCODE_TAKEOVER_PATH))
    ).toHaveLength(1)
  );
});
```

- [ ] **Step 2: Run RED**

Run: `cd src-tauri && cargo test --lib services::zcode_control::hub`
Run: `pnpm exec vitest run tests/mobile/ZcodeControlPanel.test.tsx`
Expected: FAIL（takeover 符号/按钮/端点不存在）。

- [ ] **Step 3: Implement**

- Hub `takeover_lock: AsyncMutex<()>`；
- 状态检查顺序按 spec §6.5；
- endpoint 走配对门，空 body，`Cache-Control: no-store`；
- 手机 API 函数 + 冲突态按钮 + 确认文案 + busy/错误；
- 不调用 authorize/OAuth。

- [ ] **Step 4: Run GREEN**

Run: `cd src-tauri && cargo test --lib services::zcode_control::hub`
Run: `pnpm exec vitest run tests/mobile/ZcodeControlPanel.test.tsx`
Expected: PASS。

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/services/zcode_control/hub.rs src-tauri/src/remote/zcode.rs src-tauri/src/remote/server.rs src/mobile/api.ts src/mobile/ZcodeControlPanel.tsx src/i18n/locales/zh.json src/i18n/locales/en.json tests
git commit -m "feat(zcode): let mobile take back control from official page"
```

---

### Task 9: UI 能力呈现 + e2e + 文档 + 全量门禁

**Goal:** 无探测 UI；能力状态可读；未知 build e2e 证明；文档/gates 收口。

**Depends on:** T4, T5, T6, T7, T8.

**Acceptance:**
- 无 pending 不显示审批/问答；无 selection 不显示模型控件；
- 只有 capabilities.enabled 才显示可用；不可用给原因；
- 无探测页/按钮/徽标；
- unknown build fake e2e：send 首条确认、pending 卡首答确认、无额外探针文本；
- 文档更新；全量门禁绿。

**Files:**
- Modify: `src/mobile/ZcodeControlPanel.tsx`
- Modify: `tests/settings/zcodeRemoteSettings.test.tsx` / mobile tests
- Modify: `src-tauri/src/services/zcode_control/wire_e2e.rs`
- Modify: `docs/superpowers/specs/2026-10-28-zcode-capability-parity-spec.md`（实现偏差）
- Modify: `docs/user-manual/{zh,en}/08-mobile-remote.md`
- Modify: `CHANGELOG.md`, `CHANGELOG.en.md`
- Create: `.superpowers` execution report? 按仓库惯例放 `docs/superpowers/reviews/2026-10-28-zcode-capability-parity-acceptance.md`

- [ ] **Step 1: Write failing tests**

```tsx
it("no probe UI and no approval card without pending", async () => {
  mockBackend(saved, { status: PAIRED, authStatus: REMEMBERED, session: noPending });
  render(<ZcodeControlPanel sessionId="s1" />);
  expect(screen.queryByTestId("zcode-probe")).toBeNull();
  expect(screen.queryByTestId("zcode-pending-card")).toBeNull();
  expect(screen.getByTestId("zcode-send")).toBeVisible();
});
```

```rust
#[tokio::test]
async fn unknown_build_send_confirms_without_probe_message() {
    // loopback fake: unknown build hello + snapshot; connect; send one real text;
    // assert send accepted, revision advanced, exactly one command frame.
}
```

- [ ] **Step 2: Run RED**

Run: `cd src-tauri && cargo test --lib services::zcode_control::wire_e2e`
Run: `pnpm exec vitest run tests/mobile tests/settings/zcodeRemoteSettings.test.tsx`
Expected: FAIL。

- [ ] **Step 3: Implement UI + e2e + docs**

- UI 仅消费 server capabilities；无本地推断；
- 无 pending/selection 不渲染对应控件；
- e2e 使用 fake bridge；不访问真实官方/凭据；
- 文档按 spec 的“无探测 UI/A 策略/手机抢回”更新；
- 记录真机未跑项。

- [ ] **Step 4: Run GREEN + full gates**

```bash
cd src-tauri
cargo test --lib services::zcode_control::
cargo test --features marker-helper
cargo test --tests --features marker-helper,zcode-bridge-evidence
cargo clippy --all-targets --features marker-helper -- -D warnings
cargo clippy --all-targets --features marker-helper,zcode-bridge-evidence -- -D warnings
cargo fmt --check
cd ..
pnpm test
pnpm check
pnpm build:mobile
```

- [ ] **Step 5: Commit**

```bash
git add src/mobile/ZcodeControlPanel.tsx tests src-tauri/src/services/zcode_control/wire_e2e.rs docs CHANGELOG.md CHANGELOG.en.md
git commit -m "test(zcode): cover build capability compatibility and mobile takeover"
```

---

## 验收清单

- [ ] T1 纯核通过，八项映射完整。
- [ ] T2 账本 migration/失败降级通过。
- [ ] T3 形状采集无正文，invalidate 清空。
- [ ] T4 resolver 与执行闸同源；异常回退全关。
- [ ] T5 send/model/thought ACK-only 不升级。
- [ ] T6 pending 卡能力确认/降级正确。
- [ ] T7 interrupt 守卫消失才确认。
- [ ] T8 手机抢回成功/失败/未配对/幂等。
- [ ] T9 无探测 UI；e2e 未知 build；文档/门禁全绿。
- [ ] 无 JWT/链接/正文/答案进入账本/日志/DTO。
- [ ] 上传文件未混入八项。
