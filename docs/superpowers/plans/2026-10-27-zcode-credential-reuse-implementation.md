# ZCode 记住授权与自动恢复连接实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development（沿用用户已选方式）或无子代理时使用 superpowers:executing-plans，逐任务实施。步骤使用 `- [ ]` 跟踪。本计划尚未执行。

**Goal:** 在既有 ZCode 官方桥接上增加方案 A 的记住授权与自动恢复连接，用户不再每次重启授权或授权后再点连接。

**Architecture:** 保留 AuthService 的 OAuth／JWT校验和 Hub 的连接／关闭／事件链，新增一个系统凭据存储端口与一个单飞自动连接任务。账号身份和设备路由分开；设置卡消费新增非敏感授权状态，手机现有协议不变。

**Tech Stack:** Tauri 2、Rust 1.89、Tokio、现有reqwest/SQLite/windows 0.62、React 19/TypeScript/Vitest；macOS/Linux增加固定keyring 3.6.3的明确原生feature。

**Spec:** `docs/superpowers/specs/2026-10-27-zcode-credential-reuse-prd.md`；配套设计 `docs/superpowers/specs/2026-10-27-zcode-credential-reuse-design.md`。最高依据 `docs/MASTER-PLAN.md`，只读。基线 `34307def`；执行时先核当前HEAD，不覆盖其他改动。

## Global Constraints

- 「只采用方案 A」；账号JWT仅进入本机系统凭据存储与受控Rust内存，不进settings/SQLite/日志/手机HTTP/SSE。
- 「不重写 relay 协议或现有消息控制能力」；复用原OAuth端点、terminal_token、Hub.connect、关闭epoch、投影、幂等、审计与能力门。
- 「自动恢复只恢复连接和读取订阅，不重发历史消息或动作。」
- 「普通退出程序保留已保存的授权。」用户断开／KICKED／忘记则暂停自动连接，跨重启仍保留暂停意图。
- 「安全存储写入成功才显示“已记住授权”」；失败保留本次内存授权、如实报未记住，不落明文。
- OS操作不持AuthState/DB锁；凭据I/O串行及旧epoch保护是核心底线，不等加固阶段再补。
- 所有普通自动测试注入fake存储，不访问真实保险箱，不真实登录/发送/审批；专用原生验收需明确测试条目及操作范围。
- 现有冻结ConnectionView/手机HTTP/ConnectionState不扩展；AuthStatus走独立桌面IPC，zh/en同步；不改MASTER、不push/merge/tag。

## Review Focus

1. 原生库未启用对应feature而静默用mock，或Windows使用了漫游持久化：生产必须接正确OS后端（C1/H3）。
2. 保存失败、保险箱锁定/服务缺失与401混淆：不假报记住，不把有效身份误删（C2/C4/H2）。
3. 退出/总开关关闭误记成用户暂停，或KICKED后重启互抢：意图与运行时清理分开（C3/H1）。
4. 旧恢复/保存/401在新授权或忘记之后返回：不得覆盖/删除/复活新状态（C2/C3/H2）。
5. 自己的offline事件反复重置重试预算，或多个启动触发产生多连接：有限轮次、单worker，任何自动路径零execute（C3/H1/H3）。

---

## 文件结构与阶段

新增生产文件仅两处：

- `src-tauri/src/services/zcode_control/credential_store.rs`：CredentialStore、原生存储、记录编码、错误映射、非敏感恢复禁用标记；测试fake也放此文件的cfg(test)模块。
- `src-tauri/src/services/zcode_control/auto_connect.rs`：单worker、触发合并与有界重试；只调用既有Hub生命周期，不实现relay协议。

现有文件按归属增量改：auth.rs（保存/恢复/失效），hub.rs（连接意图与编排），commands/zcode_remote.rs与lib.rs/remote/mod.rs（接线），zcodeRemote.ts与设置卡（桌面状态/忘记入口）。不新建凭据数据库、通用账号管理器或第二个连接框架。

| 阶段     | 任务        | 结束时可验证的结果                                                                  |
| -------- | ----------- | ----------------------------------------------------------------------------------- |
| 核心实现 | C1→C2→C3→C4 | 授权并保存→自动连接→重建实例恢复；换链接保留身份；主动暂停/忘记正确；设置卡真实显示 |
| 逐步加固 | H1→H2→H3    | 网络有界退避、完整竞态/故障矩阵、完整生产假链和平台验收记录                         |

**C4是首个可体验里程碑，不等H阶段才展示结果。** 此时不宣称所有网络恢复/平台场景已验；H阶段全绿及未验项登记后才判本批代码交付完整。

公共合同见设计§3–§6，不在任务之间另起同名不同义类型。新增公共名字下文首次定义，后续任务只引用。

### Task C1: 系统凭据存储与可测适配口

**Files:** Create `src-tauri/src/services/zcode_control/credential_store.rs`；Modify 同目录`mod.rs`、`src-tauri/src/database/dao/settings.rs`、`src-tauri/{Cargo.toml,Cargo.lock}`；Test credential_store.rs/settings.rs内tests。

**Interfaces:** Produces `CredentialStore:Send+Sync`：`load(&self)->StoreResult<Option<StoredCredential>>`、`save(&self,record:&StoredCredential)->StoreResult<()>`、`delete(&self)->StoreResult<()>`；`StoreResult<T>=Result<T,CredentialError>`。`CredentialError=LockedOrDenied|Unavailable|StorageFailed|InvalidRecord|TooLarge|DeleteFailed`（无原始cause）。`StoredCredential{schema_version:u8,provider:OAuthProvider,jwt:String}`（存储serde camelCase）为crate内部敏感类型，无Debug；`OsCredentialStore::new(prefs:DeviceStore)->Self`。补 `try_get_setting_conn(conn:&Connection,key:&str)->rusqlite::Result<Option<String>>`，保留旧getter行为。纯函数签名：`encode_record(record:&StoredCredential)->StoreResult<Vec<u8>>`、`decode_record(bytes:&[u8])->StoreResult<StoredCredential>`、`validate_blob_len(bytes:usize,max:Option<usize>)->StoreResult<()>`。测试产物 `MemoryCredentialStore::new()->Self`、`seed(&self,record:StoredCredential)`、`fail_next(&self,error:CredentialError)`、`load_count/save_count/delete_count(&self)->usize`，只cfg(test)。

- [ ] **Step 1: 写失败测试。** 测试代码为断言主体，按本任务Files补imports与fixture。每个注释是精确测试名；Rust异步测试使用tokio::test、前端使用现有it/describe，测试变量必须来自被测实现/记录型fake，不能直接赋期望值。

```rust
// binary_record_roundtrip_and_limit: secret编码为UTF-8 bytes，不走UTF-16 password API
let bytes = encode_record(&record).unwrap();
assert_eq!(decode_record(&bytes).unwrap().provider, OAuthProvider::Bigmodel);
assert_eq!(validate_blob_len(2560, Some(2560)), Ok(()));
assert!(validate_blob_len(2561, Some(2560)).is_err());
// checked_get_does_not_treat_broken_db_as_missing
assert!(try_get_setting_conn(&Connection::open_in_memory().unwrap(), "remote.zcode_restore_allowed").is_err());
// absence_and_delete_are_idempotent
assert!(fake.load().unwrap().is_none()); assert!(fake.delete().is_ok());
```

- [ ] **Step 2: 验证 RED。** Run: `cd src-tauri && cargo test --lib services::zcode_control::credential_store && cargo test --lib database::dao::settings`。Expected: 新用例因目标行为缺失而失败；若先缺符号，补最小失败桩后复跑到行为断言失败；环境错误不算RED，0 tests不算验证。
- [ ] **Step 3: 最小实现。** 实现设计§3的单条目与二进制格式；encode_record/decode_record/validate_blob_len为模块纯函数，拒绝空JWT/未知schema/provider。macOS keyring=3.6.3/apple-native；Linux keyring=3.6.3/sync-secret-service,crypto-rust,vendored，target依赖default-features=false；Windows复用windows0.62添加Win32_Security_Credentials，原生Generic+LOCAL_MACHINE，读取内存RAII释放。不选keyring的windows-native（默认ENTERPRISE），不落mock或文件。OS操作映射到封闭CredentialError，禁止打印原始错误。OsStore读恢复标记失败就报错；save先checked写restore_allowed=false、写OS、成功再true；delete先false再删，NoEntry视成功。单元测试只测纯函数/fake；原生API实测留H3。
- [ ] **Step 4: 验证 GREEN。** Run: `cd src-tauri && cargo test --lib services::zcode_control::credential_store && cargo test --lib database::dao::settings && cargo check --all-targets`。Expected: 新用例全部被发现并PASS，0 failed；pure函数覆盖2,560字节边界、记录畸形、NoEntry、标记读错与禁用；Cargo feature/MSRV/锁文件核对，无真实保险箱副作用。
- [ ] **Step 5: 提交。** 仅暂存本任务Files的实际改动，`git commit -m "feat(zcode): add native credential storage adapter"`；提交前核对无JWT/真实链接/正文或MASTER改动。

### Task C2: 在现有AuthService保存与恢复身份

**Files:** Modify `src-tauri/src/services/zcode_control/{auth,credential_store,mod}.rs`；Test auth.rs内沿用FakeZcode、test_service、run_authorize、valid_jwt、TwoPhaseReply；扩充MemoryCredentialStore。

**Interfaces:** Consumes C1。Produces `AuthService::with_credential_store(self,store:Arc<dyn CredentialStore>)->Self`、`async restore(&self)->AuthStatus`、`status(&self)->AuthStatus`、`async forget(&self)->AuthStatus`。`AuthStatus{authorized:bool,remembered:bool,restoring:bool,provider:Option<OAuthProvider>,problem:Option<AuthProblem>}` serde camelCase；`AuthProblem=expired|revoked|locked_or_denied|unavailable|storage_failed|delete_failed|invalid_record|too_large`。保留authorize/terminal_token/clear签名；OAuthProvider补serde lower-case，JWT不进入DTO。

- [ ] **Step 1: 写失败测试。** 测试代码为断言主体，按本任务Files补imports与fixture。每个注释是精确测试名；Rust异步测试使用tokio::test、前端使用现有it/describe，测试变量必须来自被测实现/记录型fake，不能直接赋期望值。

```rust
// restore_uses_saved_identity_without_oauth
let store = Arc::new(MemoryCredentialStore::new());
store.seed(StoredCredential { schema_version: 1, provider: OAuthProvider::Bigmodel, jwt: valid_jwt() });
let auth = AuthService::new(test_client()).with_credential_store(store.clone());
assert!(auth.restore().await.authorized); assert!(auth.status().remembered);
assert_eq!(store.load_count(), 1);
// save_failure_keeps_current_authorization: 用现有run_authorize走fake OAuth，令store.save失败
assert!(status.authorized); assert!(!status.remembered); assert_eq!(status.problem, Some(AuthProblem::StorageFailed));
// expired_or_current_401_forgets_but_network_error_does_not
assert_eq!(expired_delete_count, 1); assert_eq!(offline_delete_count, 0);
// late_old_401_never_deletes_new_credential
assert_eq!(new_jwt_delete_count, 0);
```

- [ ] **Step 2: 验证 RED。** Run: `cd src-tauri && cargo test --lib services::zcode_control::auth`。Expected: 新用例因目标行为缺失而失败；若先缺符号，补最小失败桩后复跑到行为断言失败；环境错误不算RED，0 tests不算验证。
- [ ] **Step 3: 最小实现。** 在现有commit_jwt后持久保存，在恢复时复用jwt_is_expired及epoch；status仅读内存，不调用OS；restore若已有当前代有效内存JWT直接返回，记住失败不破坏本次身份。新构造builder可叠加现有with_endpoints；默认构造不访问OS，生产C3显式注入。存储失败不让OAuth重来，返回授权成功但remembered=false/problem；native错误不变成needs_auth。沿用旧401代际判断，只有当前JWT失效才排队持久删除；offline/5xx不删除，403/WS身份错误不据此统一删除JWT。OS操作由spawn_blocking串行执行，owned I/O guard随实际调用存活；调用前后检查epoch，不持状态/DB锁。clear保持仅清内存；forget先清/取消，再禁用恢复和删除。不可把新JWT通过public getter/Debug暴露。
- [ ] **Step 4: 验证 GREEN。** Run: `cd src-tauri && cargo test --lib services::zcode_control::auth && cargo test --lib services::zcode_control::credential_store`。Expected: 新用例全部被发现并PASS，0 failed；保存/恢复/过期/401/晚到401/网络错误/写失败/忘记与现有OAuth取消测试通过。新测试无需用户浏览器，使用已有fake回环callback。
- [ ] **Step 5: 提交。** 仅暂存本任务Files的实际改动，`git commit -m "feat(zcode): reuse persisted authorization in AuthService"`；提交前核对无JWT/真实链接/正文或MASTER改动。

### Task C3: 复用Hub完成一次自动连接和用户暂停

**Files:** Create `src-tauri/src/services/zcode_control/auto_connect.rs`；Modify `hub.rs`、`mod.rs`；Test hub.rs/auto_connect.rs内tests，复用FakeBackend、hub_with_policy、stored；必要时适配 `src-tauri/tests/common/zcode.rs` 的授权状态fake。

**Interfaces:** Consumes C2，旧Hub.connect/disconnect与CloseGuard。Produces `HubBackend::restore_authorization<'a>(&'a self)->BoxFuture<'a,AuthStatus>`、`auth_status(&self)->AuthStatus`、`forget_authorization<'a>(&'a self)->BoxFuture<'a,AuthStatus>`（给旧fake默认未授权实现，生产必须委托AuthService；需要验证授权路径的fake记录authorize/forget后的真实状态，不用默认true掩盖漏接线）；`ZcodeHub::restore_authorization(&self)->AuthStatus`、`auth_status(&self)->AuthStatus`、`connect_requested(&self)->ZResult<ConnectionView>`、`disconnect_requested(&self)->ZResult<ConnectionView>`、`forget_authorization(&self)->ZResult<ConnectionView>`、`start_auto_connect(self:&Arc<Self>)`、`wake_auto_connect(&self)`。新偏好键及缺省值按设计§3.3；start幂等，worker持Weak，现有connect_lock仍唯一连接门。

- [ ] **Step 1: 写失败测试。** 测试代码为断言主体，按本任务Files补imports与fixture。每个注释是精确测试名；Rust异步测试使用tokio::test、前端使用现有it/describe，测试变量必须来自被测实现/记录型fake，不能直接赋期望值。

```rust
// startup_restores_and_connects_once_without_authorize
// FakeBackend新增set_restored_auth(AuthStatus)，使用现有hub_with_policy；store设remote.enabled=true
hub.start_auto_connect(); hub.start_auto_connect();
wait_for_connection(&hub, ConnectionState::Paired).await;
assert_eq!(fake.authorize_calls.load(Ordering::SeqCst), 0);
assert_eq!(fake.connect_calls.load(Ordering::SeqCst), 1);
// requested_disconnect_is_persistent_but_runtime_close_is_not
assert_eq!(stored_intent_after_requested_disconnect, "false");
assert_eq!(stored_intent_after_runtime_close, "true");
// route_change_preserves_account_forget_does_not
assert_eq!(route_change_forget_calls, 0); assert_eq!(explicit_forget_calls, 1);
// remote_disabled_never_connects_or_opens_browser
assert_eq!(disabled_connects, 0); assert_eq!(disabled_authorizes, 0);
```

- [ ] **Step 2: 验证 RED。** Run: `cd src-tauri && cargo test --lib services::zcode_control::hub && cargo test --lib services::zcode_control::auto_connect`。Expected: 新用例因目标行为缺失而失败；若先缺符号，补最小失败桩后复跑到行为断言失败；环境错误不算RED，0 tests不算验证。
- [ ] **Step 3: 最小实现。** 先只实现一次启动恢复/一次连接，不加网络重试。PartsBackend生产AuthService显式with_credential_store(OsCredentialStore)，production_test沿用注入fake；构造Hub仍无启动副作用。单worker由start启动，监听现有连接事件/Notify；每次连接前读取remote.enabled、auto意图、auth/链接/closing；不用新relay客户端。authorize成功设auto=true并wake；显式connect设true、必要时重试恢复，再调原connect；显式disconnect先checked写false再原disconnect，停服/退出仍走原begin/finish不写false。KICKED立即持久false并停止，不待H1才加。config_changed复用旧代际/断链/投影失效，reset不再clear JWT；删除链接保留账号。忘记先暂停/推进Hub代际/断连接，再backend.forget；重新授权也先作废旧目标代际，防旧restore覆盖新账号。等待连接的test helper wait_for_connection(hub:&ZcodeHub,state:ConnectionState)用既有事件订阅+1s超时，只供fake测试。
- [ ] **Step 4: 验证 GREEN。** Run: `cd src-tauri && cargo test --lib services::zcode_control::hub && cargo test --lib services::zcode_control::auto_connect`。Expected: 新用例全部被发现并PASS，0 failed；首次自动paired、重复start单worker、manual暂停跨实例、remote off、KICKED暂停、换/删链接不忘记、显式忘记均通过；没有execute调用。沿用现有关闭竞态回归，不能删掉来迁就新编排。
- [ ] **Step 5: 提交。** 仅暂存本任务Files的实际改动，`git commit -m "feat(zcode): restore authorization and connect through existing hub"`；提交前核对无JWT/真实链接/正文或MASTER改动。

### Task C4: 启动接线与设置卡，交付核心纵向功能

**Files:** Modify `src-tauri/src/{lib.rs,remote/mod.rs,commands/zcode_remote.rs}`、`src/lib/api/zcodeRemote.ts`、`src/components/settings/ZcodeRemoteSettingsCard.tsx`、`src/i18n/locales/{zh,en}.json`、`tests/settings/zcodeRemoteSettings.test.tsx`；Test commands/remote现有装配tests。

**Interfaces:** Consumes C3。Produces桌面IPC `zcode_control_auth_status(State<Arc<ZcodeHub>>)->Result<AuthStatus,String>`、`async zcode_control_forget_authorization(State<Arc<ZcodeHub>>)->Result<ConnectionView,String>`；既有connect/disconnect IPC名不变，内部转requested方法。TS在zcodeRemote.ts新增`ZcodeAuthStatus`和`getZcodeAuthStatus():Promise<ZcodeAuthStatus>`、`forgetZcodeAuthorization():Promise<ConnectionView>`；现有zcodeControl.ts手机DTO不改。

- [ ] **Step 1: 写失败测试。** 测试代码为断言主体，按本任务Files补imports与fixture。每个注释是精确测试名；Rust异步测试使用tokio::test、前端使用现有it/describe，测试变量必须来自被测实现/记录型fake，不能直接赋期望值。

```tsx
// 仅新增文案用例显式切zh，afterEach恢复en，既有英文用例不改
await i18n.changeLanguage("zh");
// remembered_authorization_does_not_need_second_connect_click
expect(screen.getByText("已记住授权")).toBeVisible();
expect(invokeMock.mock.calls.filter(([cmd]) => cmd === "zcode_control_connect")).toHaveLength(0);
// authorized_offline_is_not_needs_auth
expect(screen.getByText("已授权，未连接")).toBeVisible();
// persistence_failure_is_not_a_false_success
expect(screen.getByText("本次授权可用，但未记住")).toBeVisible();
// forget_refreshes_authorization_and_keeps_link
expect(
  invokeMock.mock.calls.filter(([cmd]) => cmd === "zcode_control_forget_authorization")
).toHaveLength(1);
expect(screen.getByRole("textbox")).toHaveValue(SAVED_URL);
```

- [ ] **Step 2: 验证 RED。** Run: `pnpm exec vitest run tests/settings/zcodeRemoteSettings.test.tsx`。Expected: 新用例因目标行为缺失而失败；若先缺符号，补最小失败桩后复跑到行为断言失败；环境错误不算RED，0 tests不算验证。
- [ ] **Step 3: 最小实现。** lib.rs保留安装/manage同一Arc顺序，在restore_on_launch后用Tauri async runtime非阻塞start_auto_connect；不在主线程等待保险箱。remote_toggle成功开启后wake，关闭取消旧轮次但保留意图。设置卡复用1.5s/trailing刷新，增加并行auth_status读取（status仅内存）；不在前端手动再调用connect来实现“授权后自动连接”。显示restoring/已记住/已授权未连接/记住失败/失效/存储锁定等，并加忘记按钮；沿用现有错误映射ZCODE_CONTROL:storage_failed，细节来自AuthProblem不显示原生错误。新增中文精确文案与英文对应键，复用卡片/教程/链接布局。新文案测试显式切zh并在afterEach恢复en；原文件默认英文用例保持原语言，沿用invokeMock.mock.calls与SAVED_URL夹具，不另造前端调用记录器。更新旧测试中“启动永远零连接”“换链接清账号”的旧断言为新契约，保留无OAuth记录时零浏览器和零连接。
- [ ] **Step 4: 验证 GREEN。** Run: `pnpm exec vitest run tests/settings/zcodeRemoteSettings.test.tsx && pnpm check && pnpm build:mobile`。Expected: 新用例全部被发现并PASS，0 failed；再运行 `cd src-tauri && cargo test --lib commands::zcode_remote && cargo test --lib remote::tests::` 全通过；启动/总开关接线fake证明同一Hub自动恢复。C1–C4完成后报告“核心已实现”，H阶段仍未完成，不要求用户此刻实机辅助。
- [ ] **Step 5: 提交。** 仅暂存本任务Files的实际改动，`git commit -m "feat(settings): expose remembered ZCode authorization and wire startup restore"`；提交前核对无JWT/真实链接/正文或MASTER改动。

### Task H1: 增加有界网络恢复，保持单owner与用户意图

**Files:** Modify `src-tauri/src/services/zcode_control/{auto_connect,hub}.rs`；Test auto_connect.rs/hub.rs内测试，用现有tokio test-util暂停时间。

**Interfaces:** Consumes C3的单worker、现有ConnectionView事件。Produces `ReconnectBudget::next_delay(&mut self)->Option<Duration>`、`reset(&mut self)`，私有小类型；失败延迟固定1s/3s/10s，最多3次额外尝试。对外入口沿用start/wake，不新增设置项或后台服务。

- [ ] **Step 1: 写失败测试。** 测试代码为断言主体，按本任务Files补imports与fixture。每个注释是精确测试名；Rust异步测试使用tokio::test、前端使用现有it/describe，测试变量必须来自被测实现/记录型fake，不能直接赋期望值。

```rust
// retry_budget_is_finite
let mut budget = ReconnectBudget::default();
assert_eq!(budget.next_delay(), Some(Duration::from_secs(1)));
assert_eq!(budget.next_delay(), Some(Duration::from_secs(3)));
assert_eq!(budget.next_delay(), Some(Duration::from_secs(10)));
assert_eq!(budget.next_delay(), None);
// own_offline_events_do_not_restart_budget
assert_eq!(connects_after_all_time_advances, 4);
// kicked_and_manual_pause_cancel_pending_retry
assert_eq!(calls_after_conflict_restart, 0); assert_eq!(calls_after_pause, 0);
// reconnect_never_replays_actions
assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 0);
```

- [ ] **Step 2: 验证 RED。** Run: `cd src-tauri && cargo test --lib services::zcode_control::auto_connect`。Expected: 新用例因目标行为缺失而失败；若先缺符号，补最小失败桩后复跑到行为断言失败；环境错误不算RED，0 tests不算验证。
- [ ] **Step 3: 最小实现。** 只在已有worker增加budget和可取消等待。初次立即尝试，offline才退避；paired后新的真实掉线可开新故障轮次，自己产出的offline/wake不能重置预算；预算耗尽停，显式连接、授权成功、链接更新、远程重新开启才开新轮。等待被新Hub代际/暂停/remote关闭/忘记取代时取消旧轮次，再读前置条件。KICKED/needs_auth/needs_config/unknown不自动重试；native存储错误不循环弹解锁框，用户点击连接才重试恢复。仍每次connect现取terminal token，不改RelayClient重连策略。
- [ ] **Step 4: 验证 GREEN。** Run: `cd src-tauri && cargo test --lib services::zcode_control::auto_connect && cargo test --lib services::zcode_control::hub`。Expected: 新用例全部被发现并PASS，0 failed；虚拟时间覆盖离线恢复、4次上限、并发触发、换链接、关闭/暂停/KICKED/失效中断和无写动作；不得用真实sleep等待10秒。
- [ ] **Step 5: 提交。** 仅暂存本任务Files的实际改动，`git commit -m "fix(zcode): bound reconnect attempts and honor persistent pause"`；提交前核对无JWT/真实链接/正文或MASTER改动。

### Task H2: 补存储故障与受控竞态回归

**Files:** Modify auth.rs/credential_store.rs/hub.rs中必要修正；Test同模块tests，沿用TwoPhaseReply并给MemoryCredentialStore添加gate_load/gate_save/gate_delete测试闸，不新增生产测试入口。

**Interfaces:** Consumes C1–H1公共合同。测试fake的gate方法返回`Arc<tokio::sync::Notify>`或oneshot句柄，精确控制原生操作开始/结束；所有fixture secret为合成值。无新增生产公共接口。

- [ ] **Step 1: 写失败测试。** 测试代码为断言主体，按本任务Files补imports与fixture。每个注释是精确测试名；Rust异步测试使用tokio::test、前端使用现有it/describe，测试变量必须来自被测实现/记录型fake，不能直接赋期望值。

```rust
// late_restore_cannot_override_new_authorization
assert!(status_after_old_restore.authorized); assert_eq!(current_provider, OAuthProvider::Bigmodel);
// forget_during_slow_save_never_restores_old_record
assert!(!after_restart.authorized); assert_eq!(restored_after_forget, 0);
// late_401_during_new_save_does_not_delete_new_record
assert_eq!(new_record_deleted, 0);
// cancellation_keeps_native_io_serial_until_actual_completion
assert_eq!(max_concurrent_store_calls, 1);
// store_failures_never_delete_valid_identity_or_leak_secret
assert_eq!(deletes_after_locked_or_network_failure, 0); assert!(!captured_output.contains(SYNTHETIC_JWT));
```

- [ ] **Step 2: 验证 RED。** Run: `cd src-tauri && cargo test --lib services::zcode_control::auth && cargo test --lib services::zcode_control::credential_store`。Expected: 新用例因目标行为缺失而失败；若先缺符号，补最小失败桩后复跑到行为断言失败；环境错误不算RED，0 tests不算验证。
- [ ] **Step 3: 最小实现。** 补确切交错而非sleep：load在途→忘记／新授权；save在途→忘记；旧401体在途→新JWT；caller取消→旧native任务继续→删除排队；写后标记失败、删除失败、已禁用恢复、损坏记录、NoStorageAccess、无服务、2561字节。先证实RED再改必要代码；不得重新设计OAuth或替换epoch。所有错误只更新AuthStatus/冻结码，审计与日志不出现secret。说明OS操作不能被取消时保留串行占有直至其结束；若对等待增加超时，不得提前释放仍在执行的底层操作的串行所有权；本批不扩展一个通用原生任务调度框架。
- [ ] **Step 4: 验证 GREEN。** Run: `cd src-tauri && cargo test --lib --features marker-helper,zcode-bridge-evidence services::zcode_control::`。Expected: 新用例全部被发现并PASS，0 failed；新交错和既有OAuth/关闭/回执/SSE/物理分片/恢复测试均PASS；每个未修复失败必须明确报告，不靠skip/删旧测试闭环。
- [ ] **Step 5: 提交。** 仅暂存本任务Files的实际改动，`git commit -m "test(zcode): cover persistent credential failures and lifecycle races"`；提交前核对无JWT/真实链接/正文或MASTER改动。

### Task H3: 真实组件假链、平台验收与文档收口

**Files:** Modify `src-tauri/src/services/zcode_control/wire_e2e.rs`、`tests/settings/zcodeRemoteSettings.test.tsx`；Create `docs/superpowers/reviews/2026-10-27-zcode-credential-reuse-acceptance.md`；Modify统一远控spec、`docs/superpowers/decisions/2026-10-24-zcode-official-bridge-contract.md`追加段／`tests/fixtures/zcode-official-bridge/contract.json`的auth.credentialPersistence说明、`docs/user-manual/{zh,en}/08-mobile-remote.md`、`CHANGELOG{,.en}.md`。

**Interfaces:** Consumes现有wire_e2e.rs的FakeOfficialHttp、authorized_service、full_chain、second_hub、wait_until等fixture；将共享AuthService改为可共享fake持久存储但**重新构造**AuthService/Hub。不使用mark_authorized_for_test冒充恢复，保留真实Auth/Relay/Bridge及路由链。原生验收独立标记，不纳入默认cargo test。

- [ ] **Step 1: 写失败测试。** 测试代码为断言主体，按本任务Files补imports与fixture。每个注释是精确测试名；Rust异步测试使用tokio::test、前端使用现有it/describe，测试变量必须来自被测实现/记录型fake，不能直接赋期望值。

```rust
// whole_new_process_state_reuses_vault_without_oauth
assert_eq!(oauth_exchanges_after_second_hub, 1);
assert_eq!(terminal_token_requests_after_second_hub, 2);
assert_eq!(second_hub.connection().state, ConnectionState::Paired);
// default_closed_capabilities_remain_closed_after_restore
assert!(!second_session.capabilities.send.enabled);
// paused_or_forgotten_restart_has_zero_upstream
assert_eq!(paused_restart_ws, 0); assert_eq!(forgotten_restart_ws, 0);
// auto_connection_only_never_dispatches_messages
assert_eq!(all_business_command_frames, 0);
```

- [ ] **Step 2: 验证 RED。** Run: `cd src-tauri && cargo test --lib services::zcode_control::wire_e2e`。Expected: 新用例因目标行为缺失而失败；若先缺符号，补最小失败桩后复跑到行为断言失败；环境错误不算RED，0 tests不算验证。
- [ ] **Step 3: 最小实现。** 在现有loopback假官方端完成一次OAuth→原生存储fake→自动连接，断开并丢弃旧AuthService/Hub后新建实例→restore→第二次terminal token/WS；断言OAuth仍仅一次，禁止复用同一个已授权AuthService掩盖丢失生命周期。补忘记/暂停/换链接及读取独立性。验收文档逐行记录平台、构建、用例、预期/实际、命令/退出码、是否原生/是否真实官方；平台未跑保持待验。统一spec和合同追加本批持久化裁决，auth.credentialPersistence注明旧探针仅内存/本批生产采用A，保留旧run历史；双语手册/账本写首次升级需授权一次、后续自动恢复、断开/忘记区别与失败提示，不改MASTER。
- [ ] **Step 4: 验证 GREEN。** Run: `cd src-tauri && cargo test --lib services::zcode_control::wire_e2e && cargo test --test zcode_control_remote --test zcode_control_e2e --test zcode_config_remote --test zcode_read_remote`。Expected: 新用例全部被发现并PASS，0 failed；随后根目录 `pnpm test`、`pnpm check`、`pnpm build:mobile`；src-tauri中普通/证据构建各跑 `cargo test --features marker-helper`（证据构建加,zcode-bridge-evidence）、`cargo fmt --check`、两构建clippy --all-targets -- -D warnings。全部自动门禁通过后登记下表原生验收；无环境不得标PASS，代码完成与平台完成分开。
- [ ] **Step 5: 提交。** 仅暂存本任务Files的实际改动，`git commit -m "docs(zcode): verify credential reuse and document automatic restore"`；提交前核对无JWT/真实链接/正文或MASTER改动。

## 原生／实机验收（不作为前置协议探测）

只在实现自动门禁通过后执行。使用单独测试service，不读取浏览器或他应用条目；用户本人完成必要官方登录，不要求用户在每个编码任务协助。

| 平台/场景 | PASS                                                                                                                               |
| --------- | ---------------------------------------------------------------------------------------------------------------------------------- |
| macOS     | 官方授权后保存，完全退出/重启免OAuth且自动paired；签名升级/锁定/拒绝权限分别有准确结果；debug条目不污染正式版                      |
| Windows   | 当前用户同机跨App/登录会话恢复；写入为LOCAL_MACHINE Generic二进制blob；2560边界/超限不截断不假报；其他OS用户不可直接当成自己的凭据 |
| Linux     | 有Secret Service时跨实例恢复；服务缺失/锁定时明确提示、无mock假成功或明文回退                                                      |
| 用户意图  | 总开关关闭零连接；重新开启按意图恢复；主动断开/KICKED后重启仍暂停；显式连接恢复；换链接不增加OAuth                                 |
| 失效/忘记 | 到期/401需重新授权；网络错误不删；忘记成功后重启无恢复，删除失败不假报；真实JWT不进入证据文件                                      |
| 完整性    | 保持所有既有读链、精确控制/幂等/单owner；自动恢复不直接发送、审批或重放动作                                                        |

JWT实际有效期与平台权限由验收记录，不能承诺固定月数。所有平台操作缺环境时列awaiting_platform_validation，不阻止先完成自主编码与fake验证，也不能标spec_complete。

## 覆盖与执行交接

| 需求/验收       | 所属任务                                   |
| --------------- | ------------------------------------------ |
| R1、AC1/AC7     | C1/C2/C4，H2/H3故障和平台验证              |
| R2、AC1/AC2/AC3 | C3/C4核心，H1网络恢复，H3新实例假链        |
| R3、AC2/AC3     | C3持久意图与KICKED底线，H1定时取消，H2交错 |
| R4、AC4         | C3保留身份的route reset，H3换链接回归      |
| R5、AC5/AC6     | C2清理/epoch，C3忘记/代际，H2受控并发      |
| R6、AC7/AC8/AC9 | C4状态文案，H2无泄漏，H3平台/既有链回归    |

自审：已逐条核PRD与设计、跨任务签名/类型、五项Review Focus和RED/GREEN命令；无另设协议探针、无其他方案、无未定义的生产接口或空验收。测试片段仅省去已有fixture setup，不提供与断言重复的实现代码。

按已有用户选择使用子代理执行；同一共享工作树只允许一个实现子代理写入，每任务规格/质量审查后推进。**先交C4可体验核心，再补H阶段**；出现必须守住的安全/正确性缺陷立即处理，不以分阶段为由推迟。设计/计划供用户确认后实施，本次仅交付文档。
