# ZCode 本地待发队列 TDD 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: 使用 superpowers:subagent-driven-development 或 superpowers:executing-plans，Q1–Q7 逐项执行与复审。

**Goal:** 忙态只在 Tuvis 保存，手机立即可见；支持编辑、撤回、FIFO、指定一条停止后发送及重启防重复。

**Architecture:** 新增 `services/zcode_control/local_queue/` 业务模块；既有 CommandSender/动作账本继续承担实际写出和幂等。队列版本独立于官方 revision，官方队列不迁入、不复制重发。

**Tech Stack:** Rust、tokio、rusqlite、axum、React/TypeScript、Vitest；零新依赖。

**Spec:** [设计 §2–3、§7–8](../specs/2026-11-01-zcode-local-queue-and-controls-design.md)；先读 [总索引约束与 G0](./2026-11-01-zcode-remote-control-tdd.md)。

## Global Constraints

- 固定工作区 worktree2；继承 `4c4050ec + W6 修正`，不覆盖现场、不改 MASTER-PLAN/AGENTS。
- 不做真实连接/发送/停止/凭据操作；官方 `queueEdit/queueNow/queueResume` 普通构建继续关闭。
- 只停预期 primaryTurn，其他待发项保持暂停；Unknown 不自动重试；只组装一次正文。
- edit 结果快照最长 24 小时；业务正文只在本机队列表；HTTP 身份从 gate 派生，不接受客户端 workspace/设备身份。

## Review Focus

- Accepted 但执行未开始：Q3 屏障不放下一条。
- 编辑 CAS 成功后丢响应：Q2/Q6 可重取全文附件。
- 单条插队 HTTP 断开：Q4/Q7 不撤销编排、不重复停止。
- 实际 admission=queue：Q1/Q3 记录真实结果，不留成可重发项。
- 主动停止与普通 FIFO 并发：Q3/Q4 同会话锁和持久暂停先于 stop。

## 文件与职责

| 文件                                                                           | 责任                                                            |
| ------------------------------------------------------------------------------ | --------------------------------------------------------------- |
| `types.rs`、`action.rs`、`command.rs`                                          | 持久 admission、冻结正文的内部派发；不挪动其余审批实现          |
| 新 `local_queue/{mod,types,store}.rs`                                          | 业务 DTO、唯一队列事务、版本、归属、编辑/操作快照               |
| 新 `local_queue/{host,service}.rs`                                             | 最小宿主端口、FIFO、指定项编排、worker 生命周期；分开 I/O 与 DB |
| `pending.rs`、`projection.rs`、`bridge.rs`、`hub.rs`                           | 最小输入/执行关联投影及现有生产接线，不另建 transport           |
| 新 `remote/zcode_queue.rs`、现有 `remote/{mod,server,zcode}.rs`                | 新路由、装配、旧发送/停止防绕过、既有 SSE 追加本地队列事件      |
| 新 `mobile/ZcodeLocalQueuePanel.tsx`、`mobile/zcode-local-queue-state.ts`      | 本地行及纯落地规则；原 ZcodeQueuePanel 保留为官方队列观察       |
| `lib/api/zcodeControl.ts`、`mobile/{useZcodeControl,ZcodeControlPanel}.tsx/ts` | API/hook、统一写槽、入队清草稿与编辑恢复                        |

下列**新接口是本计划的施工合同**，不是声称当前已有；后续任务只消费前序 Interfaces，不另起同义函数。

### Q1：admission 持久回执与冻结正文派发

**Files:** Modify `src-tauri/src/services/zcode_control/{types,action,command,mod}.rs`、`src-tauri/src/database/{schema,migration}.rs`、`src/lib/api/zcodeControl.ts`；Test `action.rs/command.rs` 内测试、`tests/mobile/zcodeControlApi.test.ts`。

新增回执字段时同步 `hub.rs` 和 `src-tauri/tests/zcode_control_remote.rs` 中 ZCode ActionReceipt 字面量；同名 Codex ActionReceipt 不属于本任务，不做批量误改。

**Interfaces:**

- `types.rs`: `SendDelivery::{StartNow, Queue, Guide}`（wire camelCase）；`SendAdmission { delivery: SendDelivery, input_id: String }`；`ActionReceipt.send_admission: Option<SendAdmission>`，serde default + skip None；TS 同形 `sendAdmission?: {delivery:"startNow"|"queue"|"guide";inputId:string}`。
- `command.rs`: `PreparedSend`（crate 内部、非 Deserialize）；`freeze_send_text(device_name:&str, body:&str, signature_on:bool)->ZResult<PreparedSend>`；`PreparedSend::from_stored(wire_text:String)->ZResult<Self>`、`wire_text(&self)->&str`。前者复用 compose_official_text，后者只验证不重组装。
- `CommandSender::execute_prepared_send(&self, ctx:&ActionContext, target:&BoundTarget, prepared:&PreparedSend, held_queue:Option<&HeldQueueChoice>)->ZResult<ActionReceipt>`（async）：只对新鲜空闲目标，显式 requestedDelivery=startNow；复用身份/能力/预留/一次 call/落账，不调用会再次 compose 的旧 execute_send。
- `ActionStore::receipt_for_device(&self, action_id:&str, device_id:&str)->ZResult<Option<ActionReceipt>>`：纯读旧代回执，仅供队列恢复已绑定的动作；不经拒绝旧代的 Hub.execute 重放。

- [ ] **RED：**新增 `admission_survives_db_reopen_and_replay`、`old_action_row_has_no_admission`、`prepared_send_freezes_signature_once`、`bare_accepted_stays_unknown`，以及 TS optional 字段兼容测试。关键断言：

```rust
assert_eq!(reopened.receipt_for_device(&id, &owner)?.unwrap().send_admission, first.send_admission);
assert_eq!(prepared.wire_text().matches("[mobile ").count(), 1);
assert!(old_receipt.send_admission.is_none());
assert_eq!(bare_accepted.state, ActionState::Unknown);
```

- [ ] **验证 RED：**src-tauri 下 `cargo test --locked --lib services::zcode_control:: --quiet`；根目录 `pnpm exec vitest run tests/mobile/zcodeControlApi.test.ts --pool=threads`。新断言失败或新字段尚不存在；不得依赖 keychain/网络。
- [ ] **最小实现：**为 zcode_actions 增 nullable `send_admission` JSON 列，沿本项目 column_exists 的幂等迁移；同步 SELECT_COLS/get_row/receipt/record_result。严格 type/inputId/commandId/delivery 校验后才构造 admission；旧行 None。prepared 路径的 hash 绑定冻结正文及 heldQueue，且每次写前仍核对当前状态，不能把 PreparedSend 标记作为免闸许可。
- [ ] **GREEN：**同两命令全过；加跨设备读旧回执不泄漏、同 ID 换冻结正文冲突、写库失败零派发、旧直接发送仍只签一次。测试 fake 的“裸 accepted”必须真返回裸 ACK，不被默认补全逻辑改写。
- [ ] **Review/提交：**按上述文件白名单提交 `feat(zcode): persist send admission and dispatch frozen text`，报告 OLD_ROW/新回执重放结果。

### Q2：业务队列、编辑与操作快照的事务

**Files:** Create `src-tauri/src/services/zcode_control/local_queue/{mod,types,store}.rs`；Modify `src-tauri/src/services/zcode_control/mod.rs`、`src-tauri/src/database/{schema,migration}.rs`；Test 新 store.rs 内测试。

**Interfaces:** `types.rs` 定义以下 crate 内类型，wire DTO 都 camelCase：

- `QueueActor {device_id,device_name:String}`（非 Deserialize）；`QueueDraft {text:String,attachments:Vec<QueuedAttachment>}`；`QueuedAttachment {name,path:String,is_image:bool}`，只使用既有上传的就绪引用。
- `QueueItemState::{Queued,Dispatching,Accepted,Unknown,Failed,Cancelled}`（wire snake_case）。`LocalQueueView {session_id:String,version:u64,auto_drain:bool,pause_reason:Option<String>,items:Vec<LocalQueueItemView>,recoverable_edits:Vec<EditSnapshotRef>}`；`LocalQueueItemView {item_id:String,item_version:u64,state:QueueItemState,position:u32,text_preview:String,has_attachments:bool}`；`EditSnapshotRef {request_id:String,item_id:String,expires_at_ms:i64}`。不返回他设备正文。
- `EnqueueCommand {request_id:String,actor:QueueActor,session_id:String,workspace_path:String,workspace_identity:Option<String>,draft:QueueDraft,prepared:PreparedSend,edited_from:Option<String>}`；路径/身份仅服务端解析。`EnqueueReceipt {item_id:String,item_version:u64,queue_version:u64,position:u32}`。
- `ItemCommand {request_id,item_id:String,expected_version:u64}`；`EditResult {request_id,item_id:String,draft:QueueDraft,expires_at_ms:i64}`；`QueueOperationView {request_id:String,state:QueueOperationState,receipt:Option<ActionReceipt>}`。操作状态固定 `in_flight/completed/unknown/failed`，不拿它冒充消息终态。
- `SqlLocalQueueStore::new(DeviceStore)->Self`；同步方法 `enqueue(&EnqueueCommand,now_ms:i64)->ZResult<EnqueueReceipt>`、`view(&QueueActor,&TargetKey)->ZResult<LocalQueueView>`、`edit(&QueueActor,&ItemCommand,now_ms:i64)->ZResult<EditResult>`、`retract(&QueueActor,&ItemCommand)->ZResult<LocalQueueView>`、`finish_edit(&QueueActor,request_id:&str)->ZResult<()>`、`operation(&QueueActor,request_id:&str)->ZResult<Option<QueueOperationView>>`。
- `DispatchClaim {item_id:String,ctx:ActionContext,target:BoundTarget,prepared:PreparedSend}`；恢复时另有 `RecoveryClaim {dispatch:DispatchClaim,stop_action_id:Option<String>}`，存储载体不下发手机。`PriorityClaim {operation:QueueOperationView,is_owner:bool,stop_action_id:String,send_action_id:String}`，重复操作不得产生第二个 owner。
- Store 内部接口固定：`claim_next(&TargetKey,&BoundTarget,now_ms:i64)->ZResult<Option<DispatchClaim>>`；`claim_item(&QueueActor,&ItemCommand,&BoundTarget,operation_id:&str,now_ms:i64)->ZResult<DispatchClaim>`；`begin_priority(&QueueActor,&PriorityCommand,now_ms:i64)->ZResult<PriorityClaim>`；`mark_stop_claimed(&QueueActor,request_id:&str,generation:u64,execution_id:&str)->ZResult<bool>`；`settle_dispatch(&DispatchClaim,&ActionReceipt,now_ms:i64)->ZResult<()>`；`pause(&TargetKey,reason:&str,request_id:&str)->ZResult<u64>`；`resume(&QueueActor,&TargetKey,request_id:&str,expected_version:u64)->ZResult<LocalQueueView>`；`recover(now_ms:i64)->ZResult<Vec<RecoveryClaim>>`；`pending_targets()->ZResult<Vec<TargetKey>>`。claim_item 仅接受对应编排 owner；begin_priority 后其他编辑/撤回不能抢走该条。

`PriorityCommand` 在 Q4 定义，是 Q2 可先写入 types.rs 的共享 DTO，不依赖 Q4 实现。恢复用 BoundTarget 只供识别原派发，永远不作为新写许可；重写目标必须重新绑定。

- [ ] **RED：**写内存库用例 `busy_enqueue_is_durable_fifo`、`same_request_payload_is_one_item`、`claim_and_edit_have_one_winner`、`edit_response_loss_recovers_same_snapshot`、`foreign_device_cannot_read_edit_or_retract`。对同一文件库关闭再开验证：

```rust
assert_eq!(again.item_id, first.item_id);
assert_eq!(view.items.iter().map(|x| x.position).collect::<Vec<_>>(), vec![1, 2]);
assert_eq!(replayed_edit.draft, original_draft);
assert_eq!(dispatch_claims + edit_winners, 1);
```

- [ ] **验证 RED：**`cargo test --locked --lib services::zcode_control::local_queue::store --quiet`；预期新模块/事务不存在，随后必须有行为断言失败，不能只用 schema 存在作验收。
- [ ] **最小实现：**新增 zcode_send_queue、zcode_queue_state、zcode_queue_operations（包括编辑结果快照）及活动态索引。requestId 绑定 actor/session/kind/完整参数 hash；编辑取消与全文/附件快照同事务，成功快照保留到关联新入队/明确放弃，最长 24h。CAS/审计元数据失败整事务回滚；DB 锁不跨 await。队列材料只放业务表，终态按设计清理，快照有独立保留期。
- [ ] **GREEN：**上述加 `dispatching_on_restart_is_unknown`、`plain_queued_survives_restart`、`expired_edit_snapshot_is_removed`、`replayed_edit_does_not_cancel_twice`、`zero_write_error_keeps_queued`、同 requestId 异参数拒绝、全局队位与设备可见性测试。每条携带附件的编辑结果必须等于原始结构，不能反拆 markup。
- [ ] **Review/提交：**`feat(zcode): persist local outbox and recoverable edits`，报告迁移、事务赢家和 24h 清理证据。

### Q3：新鲜执行关联、冻结发送适配与 FIFO worker

**Files:** Create `src-tauri/src/services/zcode_control/local_queue/{host,service}.rs`；Modify `src-tauri/src/services/zcode_control/{pending,projection,bridge,types,hub}.rs`、`src-tauri/src/remote/mod.rs`；Test service.rs、wire_e2e.rs。

**Interfaces:**

- `host.rs`: `BoundQueueSession {target:BoundTarget,view:SessionView}`；`QueueHostPort` 使用现有 `futures::future::BoxFuture`，方法 `session_lock(&self,key:&TargetKey)->Arc<tokio::sync::Mutex<()>>`、`read<'a>(&'a self,key:&'a TargetKey,timeout:Duration)->BoxFuture<'a,ZResult<BoundQueueSession>>`、`send_prepared<'a>(&'a self,ctx:&'a ActionContext,target:&'a BoundTarget,body:&'a PreparedSend,held:Option<&'a HeldQueueChoice>)->BoxFuture<'a,ZResult<ActionReceipt>>`、`stop<'a>(&'a self,ctx:&'a ActionContext,target:&'a BoundTarget,execution_id:&'a str)->BoxFuture<'a,ZResult<ActionReceipt>>`、`receipt<'a>(&'a self,action_id:&'a str,device_id:&'a str)->BoxFuture<'a,ZResult<Option<ActionReceipt>>>`。
- 生产 QueueHostPort 复用一个 Hub/PartsBackend；read 不自动 OAuth/建第二连接；prepared 先按当前 Hub 代际/能力/绑定检查再委托 Q1；不走旧 HTTP Send 递归。stop 委托既有定向停止实现。
- `service.rs`: `QueueRuntimeGate:Send+Sync` 提供 `remote_enabled(&self)->bool`、`device_valid(&self,device_id:&str)->bool`、`shutting_down(&self)->bool`；生产实现只读既有远程状态/设备库。`ZcodeQueueService::new(store:SqlLocalQueueStore,host:Arc<dyn QueueHostPort>,gate:Arc<dyn QueueRuntimeGate>)->Self`；`start(self:&Arc<Self>)` 幂等启动单 worker；`enqueue(&self,cmd:EnqueueCommand)->ZResult<EnqueueReceipt>`、`notify(&self,key:&TargetKey)`、async `flush_one(&self,key:&TargetKey)->ZResult<()>`、`on_session(&self,view:&SessionView)`、async `recover(&self)->ZResult<()>`、`shutdown(&self)`。恢复要 await host.receipt；启动恢复完成后再 start，关闭/退出拒绝新派发。
- `ActiveWork.source_command_id:Option<String>`；只增加经 G0 验证的最小终态事实投影（inputId/resultType/revision），不收录正文。关联屏障状态固定 `AwaitingStart / Running(executionId) / Terminal / Unconfirmed`；只接受本代同目标本 input 的关联。
- `ZcodeHub::session_write_lock(&self,session_id:&str)->Arc<tokio::sync::Mutex<()>>` 是本域共同锁源；QueueHostPort 转发，后续 C1/M1 也消费。外部 Send/Stop 在拿锁前转交队列服务；服务已持锁时内部派发不重新进入外部 execute，防止可重入死锁。`ExecutionObservation {input_id:String,execution_id:Option<String>,terminal:bool,revision:u64,generation:u64}` 与 `store.observe_execution(&TargetKey,&ExecutionObservation)->ZResult<bool>` 定义在本模块，true 仅表示本条屏障被确证解除。

- [ ] **RED：**写 `busy_enqueue_emits_zero_upstream_frames`、`idle_fifo_dispatches_one_then_waits`、`accepted_before_execution_start_keeps_barrier`、`another_execution_ending_does_not_release_ours`：假宿主 A 忙，入 B/C 零帧；A 终态后只 B；B Accepted 但尚无启动关联时不得发 C；本次 sourceCommandId 关联执行终态后才能 C。

```rust
assert!(fake.sent_texts().is_empty()); // A 忙、B/C 已入队
assert_eq!(fake.sent_texts(), vec!["B"]); // B Accepted 后控制仍空、或别的执行结束
assert_eq!(fake.sent_texts(), vec!["B", "C"]); // 确认 B 的关联执行终态后
```

`fake.sent_texts/stopped_ids/events` 是此任务测试夹具的记录接口，测试内定义；不是新生产 API。

- [ ] **验证 RED：**`cargo test --locked --lib services::zcode_control::local_queue --quiet`。必须捕获“ACK 后空 control 就当完工”的错误实现。
- [ ] **最小实现：**一会话一个 tokio 调度锁、单 worker/合并通知；预检 queued 项设备和原工作区、fresh ready、send 能力、无 pending/未解屏障；先 CAS 取得派发权，再 Q1 写一次；保存固定 dispatchActionId/真实代际。离线首次入队的 canonical workspace_path 与现绑路径一致时可补 pin workspaceIdentity，不把“原先没有 identity”误当换目标；已有 identity 改变则暂停。普通暂停/官方非空队列时不自动发送。旧 API 新 Send 有待发/暂停/运行时拒绝并要求刷新；无队列的原空闲发送可兼容，但使用同一后端会话锁。
- [ ] **恢复与 GREEN：**receipt 先落、queue 落账失败时，recover 纯读 Q1 旧代回执补齐；找不到或未知只暂停，不换新 ID 重发。加 `unexpected_official_queue_admission_never_resends`（delivery=queue 后离开可发队列、暂停接力）、`restart_uses_original_dispatch_receipt`、`remote_disabled_never_flushes`、`revoked_device_zero_dispatch`、旧代/他会话/缺关联不解屏障。启动接在现有 restore 完成后，只复用内存身份。
- [ ] **Review/提交：**`feat(zcode): dispatch local fifo with execution evidence`；必须点出生产宿主适配器与启动/退出接线，不能只提交 fake service。

### Q4：持久停止认领、指定项优先与队列暂停

**Files:** Modify `src-tauri/src/services/zcode_control/local_queue/{types,store,service}.rs`、`src-tauri/src/services/zcode_control/hub.rs`；Test service.rs。

**Interfaces:** `PriorityCommand {request_id,item_id:String,expected_version:u64,confirm_stop:bool,expected_execution_id:Option<String>,held_queue:Option<HeldQueueChoice>}`；async `ZcodeQueueService::prioritize(&self,actor:&QueueActor,cmd:PriorityCommand)->ZResult<QueueOperationView>`、`stop_current(&self,actor:&QueueActor,key:&TargetKey,request_id:&str,expected_execution_id:&str)->ZResult<QueueOperationView>`；`resume(&self,actor:&QueueActor,key:&TargetKey,request_id:&str,expected_queue_version:u64)->ZResult<LocalQueueView>`。

- [ ] **RED：**`prioritize_c_stops_a_then_sends_only_c`，A 忙、B/C queued，C 确认插队；校验事件顺序 `[pause_persisted,stop_claimed,stop(A),terminal(A),send(C)]`，B 不发送、仍暂停；`idle_prioritize_does_not_require_preexisting_pause` 在空闲直接 C。

```rust
assert_eq!(fake.stopped_ids(), vec!["A"]);
assert_eq!(fake.sent_texts(), vec!["C"]);
assert!(!store.view(&actor, &key)?.auto_drain);
assert_eq!(store.view(&actor, &key)?.items[0].item_id, b_id);
```

- [ ] **验证 RED：**`cargo test --locked --lib services::zcode_control::local_queue::service --quiet`；新编排未存在或顺序不符失败。
- [ ] **最小实现：**会话锁覆盖 fresh read→持久暂停/编排认领→定向停止→终态判据→认领指定项发送。请求不接受正文副本；运行中没有 confirmStop/旧 expectedExecutionId 零 stop/send；空闲不要求 paused。首次可能停止写出前保存稳定 stopActionId/claim；一个 durable operation 对应后台独立任务，不随 HTTP future 取消。前台等待预算 60s，超时只停止等待后台任务，不取消写者；后台每个新写步骤开始前复核截止时间，超时不开始下一段，已开始的端口调用按既有 ACK/终态硬上界收尾。不要外套 timeout 直接取消桥 call，也不固定 sleep 后发。
- [ ] **GREEN：**`cancelled_prioritize_does_not_repeat_stop`、`stop_unknown_is_zero_send_even_after_restart`、`new_execution_after_stop_is_not_stopped_again`、`manual_stop_pauses_before_fifo_can_flush`、`priority_and_auto_flush_have_one_winner`、`resume_uses_queue_version_and_preserves_fifo`。纯 stop 的对外路径也先持久 pause，内部 port.stop 不再调用会重复 pause/加锁的外层入口。明确零写与可能写出分别记录，未知不能通过新 requestId 绕过仍在册的停止尝试。
- [ ] **Review/提交：**`feat(zcode): prioritize exactly one queued message after stop`，停止和消息帧分别计数，不以总帧数 1 掩盖停止缺失。

### Q5：生产 HTTP / SSE 和服务装配

**Files:** Create `src-tauri/src/remote/zcode_queue.rs`；Modify `src-tauri/src/remote/{mod,server,zcode}.rs`、`src-tauri/src/services/zcode_control/{hub,types}.rs`、`src/lib/api/zcodeControl.ts`；Create `src-tauri/tests/zcode_local_queue_http.rs`，复用 tests/common/zcode.rs 的真实 router 装配。

**Interfaces:** 新端点在 `/m/api/v1/zcode/local-queue`：GET `?sessionId=`；POST `/enqueue`、`/prioritize`、`/edit`、`/retract`、`/resume`、`/edit/finish`；GET `/operations?requestId=`、`/edits?requestId=`。请求仅接受对应命令的客户端字段，actor/workspace/prepared/generation/能力由服务端构造。enqueue body 为 `{requestId,sessionId,draft,editedFrom?}`；其余为 ID/version/确认字段，不允许正文副本插队。GET view 的 recoverableEdits 只列请求 ID/到期时间，全文另取。

`ControlEventKind::LocalQueue` 与 SSE `event: local_queue`，data 为 LocalQueueView；按 session/device 过滤，snapshot 追加可选 localQueue，resync 只 GET。复用既有 Hub 单实例/事件总线；QueueService 槽不得产生第二个 Hub/上游。

- [ ] **RED：**从真实生产 router 发请求，`unpaired_queue_api_never_calls_service`、`enqueue_busy_returns_local_item_and_zero_wire`、`queue_edit_http_returns_exact_snapshot`、`prioritize_forbids_text_and_foreign_item`、`old_actions_send_cannot_bypass_outbox`、`queue_get_never_dials_or_sends`。

```rust
assert_eq!(enqueue_response.status(), StatusCode::OK);
assert!(enqueue_json["itemId"].as_str().is_some());
assert_eq!(fake_wire_count, 0);
assert_eq!(unpaired_response.status(), StatusCode::FORBIDDEN);
assert_eq!(extra_text_prioritize.status(), StatusCode::BAD_REQUEST);
```

- [ ] **验证 RED：**`cargo test --locked --test zcode_local_queue_http --features marker-helper,zcode-bridge-evidence --quiet`；新路由 404/白名单失败是初始红，随后必须断言持久状态和帧数。
- [ ] **最小实现：**严格嵌套字段白名单、no-store、当前 ZCode 可见会话门、设备归属、workspace 从既有会话/附件解析；拒绝自由路径/自报设备字段；错误沿冻结码映射（冲突 `invalid_action`、归属不存在 `no_session`、存储 `storage_failed`），操作结果状态提供额外可理解的阶段，不透传厂商错误原文。
- [ ] **GREEN：**覆盖 POST 取消后同 requestId 查原结果、同 ID 换参数冲突、编辑响应丢失后 GET 原快照、跨设备/会话零泄漏；SSE snapshot/事件/lag 重取可恢复一致队列。检查 RemoteState 安装、启动 restore、远程关闭、退出均已装配。
- [ ] **Review/提交：**`feat(remote): expose guarded zcode local queue routes`。

### Q6：手机即时列表、再编辑与统一写槽

**Files:** Create `src/mobile/{ZcodeLocalQueuePanel.tsx,zcode-local-queue-state.ts}`；Modify `src/mobile/ZcodeControlPanel.tsx`、`src/mobile/useZcodeControl.ts`、`src/lib/api/zcodeControl.ts`、`src/i18n/locales/{zh,en}.json`；Create `tests/mobile/{ZcodeLocalQueuePanel.test.tsx,zcodeLocalQueueFlow.test.tsx}`；Modify 原 hook/Panel 回归。

**Interfaces:** API 明确导出 `getZcodeLocalQueue(sessionId):Promise<LocalQueueView>`、`enqueueZcodeMessage(input):Promise<EnqueueReceipt>`、`prioritizeZcodeMessage(input):Promise<QueueOperationView>`、`editZcodeQueuedMessage(input):Promise<EditResult>`、`retractZcodeQueuedMessage(input):Promise<LocalQueueView>`、`resumeZcodeLocalQueue(input):Promise<LocalQueueView>`、`getZcodeQueueOperation(requestId)`、`getZcodeEditSnapshot(requestId)`、`finishZcodeEdit(requestId)`；input 的 camelCase 字段与 Q2/Q4/Q5 一一对应。

`mergeLocalQueue(current:LocalQueueView|null,incoming:LocalQueueView):LocalQueueView` 只比较本地 version/会话；hook 在现有一条 SSE 上消费 local_queue，不另开上游。Panel 抽 `runWrite<T>(operation:()=>Promise<T>):Promise<T>` 共享同一同步 ref；既有 runAction 与新 API 都通过它。新 Panel props 为 `{view,busy,onPrioritize,onEdit,onRetract,onResume}`，只传 itemId/version，不组装官方信封。

- [ ] **RED：**`enqueue_during_running_is_visible_before_host_finishes`、`continued_draft_survives_enqueue_ack`、`local_queue_has_index_preview_and_three_buttons`；断言本地入队不直接 POST `/zcode/actions send`，摘要 20 字素并保留全文可访问入口，按钮名为“插队/再编辑/撤回”。

```tsx
expect(screen.getByText("第 2 条")).toBeVisible();
expect(screen.getByRole("button", { name: "再编辑" })).toBeEnabled();
expect(posts.filter((p) => p.url.endsWith("/local-queue/enqueue"))).toHaveLength(1);
expect(posts.filter((p) => p.url.endsWith("/zcode/actions"))).toHaveLength(0);
expect(draft.value).toBe("等待入队回执期间继续输入的内容");
```

复数行按钮用 `within(row)` 定位；`posts` 为本测试文件的受控 fetch 记录，不访问真实服务。

- [ ] **验证 RED：**`pnpm exec vitest run tests/mobile/ZcodeLocalQueuePanel.test.tsx tests/mobile/zcodeLocalQueueFlow.test.tsx --pool=threads`。
- [ ] **最小实现：**入队成功按本次提交清草稿/附件；Unknown 入队查询原 requestId、不新建。默认展开待发行；官方队列另标来源、不套本地编辑。编辑前草稿冲突默认取消；CAS 后用户续写再次询问；快照从 recoverableEdits 找回，只有用户确认才落草稿。引用原附件，不重传、不重复 markup。旧 Get/SSE 不回退本地 version，切会话/卸载仍 abort 上传及丢弃迟到 UI。
- [ ] **GREEN：**加 `edit_response_loss_recovers_same_snapshot`、已有/续写草稿、附件-only、上传/失败阻发、LAN 无 randomUUID 仍可入队、`permission_and_enqueue_same_frame_posts_once`、相反点击顺序、旧 GET/他会话/重连 resync、停止未知可见阶段。所有写入口共享槽，不能只禁按钮不挡同帧。320/360/390、中英亮暗 100/125% 用假链截图检查行与正文空间。
- [ ] **Review/提交：**`feat(mobile): manage zcode local outbox and edit recovery`。

### Q7：真实路由全链假宿主与主线收口

**Files:** Modify `src-tauri/src/services/zcode_control/wire_e2e.rs`、`src-tauri/tests/zcode_local_queue_http.rs`、`tests/mobile/ZcodeControlFlow.test.tsx`、中英文 `docs/user-manual/{zh,en}/08-mobile-remote.md`；Update 执行报告。

- [ ] **RED：**新增穿过实际路由/业务库/宿主端口的 `outbox_http_stop_send_flow_is_single_item`；假宿主 A 运行时 B/C 已持久化且零消息帧；选 C 后 ACK-only 阶段仍零 C；确认 A 终态后一条 C，B 仍 paused。另新增 handler 取消/重建服务重放、官方实际 queue、错客户端身份、双手机并发及旧官方队列 CAS 的反例。

```rust
assert_eq!(wire.stop_count(), 1);
assert_eq!(wire.send_count(), 1);
assert_eq!(wire.sent_texts(), vec![c_frozen_body]);
assert_eq!(replayed_operation, first_operation);
assert_eq!(wire.stop_count(), 1); // 取消/重放之后仍不多一次
```

- [ ] **验证 RED：**`cargo test --locked --test zcode_local_queue_http --features marker-helper,zcode-bridge-evidence --quiet`；`cargo test --locked --lib services::zcode_control::wire_e2e --quiet`；移动端 `pnpm exec vitest run tests/mobile/ZcodeControlFlow.test.tsx tests/mobile/zcodeLocalQueueFlow.test.tsx --pool=threads`。
- [ ] **最小实现：**只补已暴露的跨层接线，不调整 fake 迁就错误协议。HTTP、store、真实派发器及 UI 控制链必须参与，不能仅断言直接调用 service 的结果。
- [ ] **GREEN/复审：**同命令过后把 U1–U7 与原八能力定向回归加入总索引 V0；手册区分“Tuvis 待发”与“官方已入队”，不得沿用“官方所有操作已开放”。无真实机只写 NOT RUN。
- [ ] **提交：**`test(zcode): verify local outbox across production routes`。队列核心代码/假链闭环后再推进控制工作包，仍不自动合并。
