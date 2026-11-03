# ZCode 官方桥接生产实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development（沿用用户已选方式）逐任务实施；无子代理时才使用 superpowers:executing-plans。所有步骤用 `- [ ]` 跟踪。先读 Spec 与本计划，逐任务经过实现、规格审查、代码审查；本次仅重写计划，尚未开始施工。

复核日期：2026-10-26；原位替换旧版，代码基线 `8e663f09`。

**Goal:** 让配对手机通过 Tuvis 统一 UI 控制正在运行的 ZCode APP 会话，并以逐能力证据门控制开放，保留独立的消息／文件读取。

**Architecture:** Rust 主进程内维持一条官方 terminal relay 连接，复刻已取证的二进制 RPC、工作区桥和 v4 会话订阅；手机只访问现有配对门内的 Tuvis API。所有写操作经同一个有去重账本的 Hub，授权在桌面发起，凭据只在 Rust 内存；前端消费 Tuvis DTO，不直接实现官方协议。

**Tech Stack:** 现有 Tauri 2、Rust 1.89+、Tokio、reqwest 0.12、axum 0.8、SQLite、React 19、TypeScript、Vitest；新增 WebSocket 客户端见 T1.2，不引入 Node 生产 sidecar。

**Spec:** `docs/superpowers/specs/2026-10-24-zcode-unified-remote-design.md`（v3）；上位依据 `docs/MASTER-PLAN.md`；协议证据 `docs/superpowers/decisions/2026-10-24-zcode-official-bridge-contract.md` §6–§8.10 与 `tests/fixtures/zcode-official-bridge/contract.json`。

## Global Constraints

- 「不新增隐藏浏览器自动点击，不默认采集真实账号凭据，不改 MASTER-PLAN。」
- 「H3 无头 CLI 开关不决定官方协议是否可写」；本计划不注册 ZCode HeadlessKind，不启动 app-server，不接管 APP stdio。
- 「保存链接本身不足以完成统一控制」；链接主机 `zcode.z.ai`、`https`、`/remote/vN`、4096 长度及现有保存校验保持不变。
- 「配置归属为运行 Tuvis 的主机；每台电脑首版保存一条 ZCode 设备链接，配对手机共享。」手机不提供配置编辑或 OAuth 凭据接口。
- 「所有状态下，看板、正文、文件预览继续走已有读取链。」控制错误仅在设置字段／控制区显示；发送失败保留草稿。
- 「超时／断线不自动重发原消息。」ACK 只显示“已受理”；宿主撤卡／守卫消失／配置新鲜快照才能显示“已生效”。
- JWT 仅本机内存，退出清除；不写 settings/keychain/日志/手机响应。SID/MID、OAuth state/code、token、正文不得进入新日志或夹具。
- 同 SID/role 单 owner；KICKED 立即冲突终态，不自动重连互抢。两台配对手机共享同一个 Hub。
- 新旧 `sess_+UUID`／`session_+32hex` 保留原 ID；禁止标题、cwd 相似度或手动去横杠猜会话。
- A 包六段布局、默认值、PIN 遮罩、链接校验和只读修复是既有基线，不重做、不回退；显式 false 保留。
- 所有新增用户文案 zh/en 同步；macOS/Windows 同合同；未通过实机门不宣称平台验收完成，不 push/合并/tag/发版。

## Review Focus

1. 保存／删除链接恰逢旧 OAuth 回调或旧 WS 帧到达：旧结果不得重新连接或污染新目标（T1.1、T7.1）。
2. ACK 早于初始快照、`activeWorks` 缺失与 `[]`：未知不得发 stop，已确认空集才可说空闲（T2.2、T4.2）。
3. 两台手机重复点击及发送后断网／重启：同 actionId 不双发，不同 payload 复用 ID 必须拒绝（T3、T8.1）。
4. 多工作区有同名会话、兄弟 topic、同名题干或目录含另一账号 provider：不得靠展示文字猜归属（T2.2、T4.1、T6）。
5. 官方版本变更／KICKED／SSE 丢事件／手机被吊销：能力降级、重取投影、旧 pending 失效，阅读持续可用（T1.2、T7.1、T8.1）。

---

## 施工边界与证据

基线 HEAD `8e663f09`：A 已实现（平台验收未全闭合），B 是探针和合同，不在产品运行链。本计划替代旧 T1–T8 的粗任务；保留编号族，细拆为独立可审查子任务。除 T8.2 外都可用内存 DB、fake HTTP/WS 和合成夹具自动验证；真实登录与动作只在指定专用会话进行。

审计记录：[接口与覆盖核查](../reviews/2026-10-25-zcode-production-plan-audit.md)。官方源码固定 SHA `3c5b227649371507ddc4eb46ac5133afe6db1cab`，不以“当前最新版”替换证据。Tuvis 下述 Rust 名称／HTTP 路径是**本计划新增的内部接口**，不是官方已存在接口。

| 证据键 | 可复用事实与精确入口                                                                                                   | 不得扩张的边界                                                                                                     |
| ------ | ---------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------ |
| E1     | `oauthFlow.ts`、`relayAuth.ts`；合同 auth；B1 `macos-B120261024T105940Z-NarmSw`                                        | BigModel 回环授权已实跑；ZAI 通道仅源码/fake、实跑不确定；token TTL=60s 已见，一次性消费未实跑重放                 |
| E2     | `sessionBridge.ts` codec/assembler/SingleConnectionGate/ControllerTaskIndexReader/SessionTargetMapper；合同 operations | 不是 JSON-RPC 文本直发：序列化、CRC32、分片、ACK、Initialize、桥和握手均必需；长驻心跳另有官方源码支撑（核查记录） |
| E3     | B2 `macos-B220261024T132316Z-prItvw`；`SendOnceLoop`                                                                   | 已证单条 sendText 落到目标 APP；任何“该会话有活动”都不能独自证明本动作完成                                         |
| E4     | `interactionBridge.ts`、`session-probe.ts`；合同 resolve/stop；决策 §8.10                                              | 单选与 stop 历史仅 ACK；新探针终态逻辑还未实跑；探针只停自己触发的新执行不是产品按钮语义                           |
| E5     | 决策 §8.8–§8.10；approve `...T032425Z-K7I6ZA`、reject `...T034803Z-noorSA`                                             | 应答既有卡已证；terminal sendText（含 guarded）不弹卡有负结果；不能承诺手机发送受 guarded 保护                     |
| E6     | `selectionBridge.ts`；模型 `...T030637Z-bS5dOg`、档位 `...T025928Z-3WyTFA`                                             | 同 provider、新快照确认已证；恢复原值只是探针清理，不进入产品                                                      |
| E7     | 固定源码 `command.ts:222–258`、`interaction-broker.ts:623–650,766–780`；已有 buildUserInputAnswer                      | 多选和自由文本有源码支持、尚无 live；代码可先完成，开闸必须 T8.2 分形态实跑                                        |

## 文件结构与公共合同

`src-tauri/src/services/zcode_control/` 新文件按职责分工：`types.rs`（Tuvis DTO/错误/端口）、`auth.rs`、`relay.rs`、`codec.rs`、`bridge.rs`、`projection.rs`、`pending.rs`、`command.rs`、`action.rs`、`audit.rs`、`selection.rs`、`hub.rs`。现有 `config.rs` 保持配置职责，`mod.rs` 导出新域；不要把授权塞进 config 或 remote 大文件。

应用接线在现有 `commands/zcode_remote.rs`、`lib.rs`、`remote/{mod,server,api}.rs`；新增 `remote/zcode.rs` 集中 HTTP DTO 适配。前端新增 `src/lib/api/zcodeControl.ts`（唯一 Tuvis DTO 类型）、`src/mobile/{useZcodeControl,ZcodeControlPanel}.tsx`，沿用 MessageComposer/SessionDetail 和设置卡；官方 wire 只存在 Rust。

公共数据类型统一在 `types.rs` 定义并 `Serialize` 为 camelCase，枚举使用下述 snake_case。Tuvis DTO/issuedAt 时间为 epoch ms；官方 relay 的 client_ts 按源码为 epoch 秒（不能照搬 DTO 单位），generation/revision 为非负整数；Rust 用 u64，前端拒绝非安全整数。不向手机返回未经筛选的官方 snapshot、账号目录或异常原文。

| 类型                            | 字段／变体（所有后续任务引用这一处）                                                                                                                                                                                                                                                                                                                                      |
| ------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `ZResult<T>` / `ZcodeError`     | `Result<T,ZcodeError>`；错误 `{code:String}`，只允许本域常量码（下文码表），不携原文；内部 cause 不自动 Debug 到日志                                                                                                                                                                                                                                                      |
| `ConnectionState`               | `needs_config, needs_auth, connecting, paired, offline, conflict, unknown`                                                                                                                                                                                                                                                                                                |
| `ConnectionView`                | `{state,generation:u64,reasonCode:Option<String>,deviceLabel:Option<String>}`；deviceLabel 仅供显示，不能用于绑定                                                                                                                                                                                                                                                         |
| `TargetKey` / `BoundTarget`     | 前者 `{sessionId:String}`；后者 `{sessionId,workspacePath,workspaceIdentity:Option<String>,workspaceKey,generation}`；后者只由精确 listing 解析，不接受手机自报                                                                                                                                                                                                           |
| `ActionState` / `ActionReceipt` | `accepted,confirmed,noop,rejected,send_failed,unknown`；回执 `{actionId,sessionId,generation,state,reasonCode:Option<String>}`                                                                                                                                                                                                                                            |
| `ActionContext`                 | `{actionId,deviceId,deviceName}`；device 字段必须来自配对门／本机 DB，客户端只能提交 actionId                                                                                                                                                                                                                                                                             |
| `SessionView`                   | `{sessionId,generation,revision:Option<u64>,ready:bool,pending:Vec<PendingView>,control:Option<ControlView>,selection:Option<SelectionView>,capabilities:CapabilityView}`                                                                                                                                                                                                 |
| `PendingView`                   | `{interactionId,kind,anchorRowId:Option<u64>,toolCallId:Option<String>,createdAt:Option<u64>,summary:Option<String>,toolName:Option<String>,sensitive:bool,autoResolution:bool,autoResolutionState:Option<String>,questions:Vec<QuestionView>,options:Vec<PermissionOptionView>}`；四元组使用 session + 真实行／toolCall 锚 + interaction + generation，不伪造必填 turnId |
| `QuestionView` / `AnswerEntry`  | 问题 `{header,questionText,options:Vec<QuestionOption>,multiSelect:bool,freeTextAllowed:bool}`；`QuestionOption{value,label,description:Option<String>,preview:Option<String>}`；答案 `{questionText,selectedValues:Vec<String>,freeText:Option<String>}`；freeTextAllowed从payload.freeText派生（不是虚构上游questions[].freeText字段）；未知schema保守禁用              |
| `PermissionOptionView`          | `{optionId,label,kind}`；准入按宿主 kind=`allowOnce`/`deny`，回传原 optionId；拒绝 fullAccess 和其他 kind                                                                                                                                                                                                                                                                 |
| `ControlView` / `StopRequest`   | control `{activeWorks:Vec<ActiveWork>}`，work `{kind,foregroundExecutionId}`；stop `{expectedForegroundExecutionId}`；未知 control=null，不归一为空数组                                                                                                                                                                                                                   |
| `SelectionView` / `ModelChoice` | selection `{provider,model,thought:Option<String>,models:Vec<ModelChoice>}`；choice `{modelId,thoughtLevels:Vec<String>}`；只含当前 provider                                                                                                                                                                                                                              |
| `CapabilityView`                | `send,singleSelect,multiSelect,freeText,interrupt,approvalReply,model,thoughtLevel` 各为 `{enabled:bool,reasonCode:Option<String>}`；不直接复制 contract capabilities 的宽泛 true                                                                                                                                                                                         |
| `ControlAction`                 | tagged `kind`: `send{text}` / `snooze{interactionId}` / `answer{interactionId,answers:Vec<AnswerEntry>}` / `permission{interactionId,optionId}` / `stop{expectedForegroundExecutionId}` / `selection{model,thought}`；全部携 `sessionId,generation`，统一 actionId 在 ActionContext                                                                                       |
| `ControlEvent`                  | `{seq:u64,generation,sessionId:Option<String>,kind:connection                                                                                                                                                                                                                                                                                                             | session | action,data}`；data 为对应 Tuvis DTO，绝无 JWT/token；SSE lag 后客户端重 GET，不重放动作 |

对外错误码冻结为 `needs_config, needs_auth, offline, conflict, unknown, not_ready, unsupported_protocol, needs_evidence, no_session, target_ambiguous, stale_generation, stale_pending, invalid_action, invalid_answer, forbidden_option, action_payload_conflict, storage_failed, payload_too_large, stop_target_changed`。Capability reasonCode与HTTP error共用它们；selection stale CAS归not_ready并刷新投影，宿主unknown错误归unknown，不把原msg下发。IPC统一映射`ZCODE_CONTROL:<code>`，在errcode约定及zh/en的errors.zcodeControl添加本地域入口，UI通过code查本地化原因；用户可见设备/题目/模型显示值不是日志原文。T7.1加IPC错误映射测试，T7.2覆盖所有reason的中英文案。

所有 `&self` 的 Hub 方法可并发调用，网络发送由单 actor 分配 requestId/seq/messageSeq；Hub用一条mpsc事件泵将RelayEvent::Data送BridgeManager.on_data，State变代时invalidate旧桥/投影，并把进行中动作保守收口；不在 DB/Mutex guard 持有期间 await 网络。不同 workspaces 可有多个逻辑 bridge，但共享同一物理 WS，不在切会话时无故换连接 generation。

执行顺序：T1.1 → T1.2 → T2.1 → T2.2 → T3 → T4.1 → T4.2 → T5 → T6 → T7.1 → T7.2 → T8.1 → T8.2。每任务 Step 1 是测试断言片段（imports/fixture setup按Files中的测试文件补齐；每段注释是精确用例名，变量按用例命名的给定输入/事件时序构造，不得把期望值直接赋给变量代替调用被测代码）；名称和断言不可省。command测试分别放入`tests::answer`、`tests::stop`、`tests::permission`子模块，policy测试放入`types::tests::policy`，与命令filter一致。fake 只注入本机测试端口，生产端点不可由手机传入。

### Task T1.1: 官方授权与内存凭据

**Files:** Create `src-tauri/src/services/zcode_control/{types,auth}.rs`；Modify 同目录 `mod.rs`；Test `auth.rs` 内 tests；后续任务按上表增补 types，不预建空功能。

**Interfaces:** Produces `AuthService::new(http:reqwest::Client)->Self`、`async authorize(&self,provider:OAuthProvider,open:OpenAuthorize)->ZResult<()>`、`async terminal_token(&self)->ZResult<TerminalToken>`、`clear(&self)`；测试注入 `with_endpoints(http:reqwest::Client,authorizeOrigin:String,zcodeOrigin:String)->Self` 仅由测试源码调用，生产factory固定官方origin且无配置/IPC入口。`OAuthProvider=Bigmodel|Zai`；`OpenAuthorize=Arc<dyn Fn(String)->ZResult<()>+Send+Sync>`；`TerminalToken{value:String,expiresIn:u64}` 不实现 Debug/Serialize。授权 state、随机回环端口、callback listener 由 auth 内持有，JWT 不返回上层。

- [ ] **Step 1: 写失败测试。** 以下断言分别放入同名测试；fixture 全为合成值，不读取真实账号／DB。

```rust
// wrong_state_never_exchanges: fake 回调后 HTTP 交换次数
assert_eq!(exchange_count_after_wrong_state, 0);
// oauth_provider_urls: 两通道参数名不同
assert_eq!(bigmodel.path(), "/login"); assert!(bigmodel.query_pairs().any(|(k,v)| k=="appId" && v=="zcode"));
assert_eq!(zai.path(), "/api/oauth/authorize");
// expired_jwt_requires_auth
assert_eq!(expired.terminal_token().await.unwrap_err().code, "needs_auth");
// canceled_authorization_discards_late_callback
assert_eq!(exchanges_after_clear_and_late_callback, 0);
```

- [ ] **Step 2: 验证 RED。** Run: `cd src-tauri && cargo test --lib services::zcode_control::auth`。Expected: 新测试因缺实现／行为断言失败；若编译先因新符号不存在失败，先补仅签名的最小桩并确认断言仍红，不接受依赖下载／环境故障充当 RED。
- [ ] **Step 3: 最小实现。** 实现上述 auth 接口与 OAuthProvider；复制 E1 的端点/参数：BigModel `https://bigmodel.cn/login?redirect&appId=zcode&state`，ZAI `https://chat.z.ai/api/oauth/authorize?redirect_uri&response_type=code&client_id&state`（公开 client_id 取 oauthFlow.ts 常量）；交换 `POST https://zcode.z.ai/api/v1/oauth/token {provider,code,redirect_uri,state}` 只取 `data.token`。listener 仅 bind `127.0.0.1:0`，state 精确一次消费，120s 超时关闭，callback 响应不回显参数；终端 token 请求为 Bearer JWT + `{role:"terminal"}`，401 清凭据，expires_in 必须正整数，逐连接新取。120s 是 Tuvis 新策略，不冒称官方 TTL。用授权 epoch 让 clear 后迟到回调失效。
- [ ] **Step 4: 验证 GREEN。** Run: `cd src-tauri && cargo test --lib services::zcode_control::auth`。Expected: 本任务全部新用例被发现且 PASS、0 failed（0 tests 不算通过）；fake 覆盖正确／错误／缺 state、重复callback、超时、取消、401、两通道 URL、token 字段畸形，所有 secret 不出 Debug/错误/响应。ZAI 不标 live-proven。
- [ ] **Step 5: 提交。** 只 `git add` 本任务 Files 中实际改动路径，再 `git commit -m "feat(zcode): add official oauth with memory-only credentials"`；提交前检查暂存区无链接、凭据、正文与 MASTER-PLAN 改动。

### Task T1.2: 单 relay 连接与错误生命周期

**Files:** Create `src-tauri/src/services/zcode_control/relay.rs`；Modify `types.rs`、`mod.rs`、`src-tauri/{Cargo.toml,Cargo.lock}`；Test relay.rs 内 fake WS tests。

**Interfaces:** Consumes T1.1 `AuthService`。Produces `RelayClient::new(auth:Arc<AuthService>,events:tokio::sync::mpsc::Sender<RelayEvent>)->Self`、`async connect(&self,device:DeviceRoute)->ZResult<ConnectionView>`、`async send_data(&self,payload:serde_json::Value)->ZResult<()>`、`async disconnect(&self)`；测试注入 `with_endpoint(auth:Arc<AuthService>,events:tokio::sync::mpsc::Sender<RelayEvent>,wsOrigin:String)->Self`，同样不注册配置/IPC；`DeviceRoute{sid,mid:Option<String>,label:Option<String>}` 仅从已保存 URL 解析；`RelayEvent=Data(Value)|State(ConnectionView)`。

- [ ] **Step 1: 写失败测试。** 以下断言分别放入同名测试；fixture 全为合成值，不读取真实账号／DB。

```rust
// concurrent_connect_uses_one_socket
assert_eq!(sockets_opened_for_two_connects, 1);
// waiting_is_not_human_approval
assert!(!waiting_connection_is_paired);
// kicked_is_terminal
assert_eq!(view.state, ConnectionState::Conflict); assert_eq!(reconnects_after_kicked, 0);
// unpaired_cannot_send_data
assert_eq!(unpaired_data_frames, 0);
// heartbeat_missing_ack_is_offline_without_reconnect
assert_eq!(after_15s_without_pair_status.state, ConnectionState::Offline); assert_eq!(heartbeat_after_close, 0);
```

- [ ] **Step 2: 验证 RED。** Run: `cd src-tauri && cargo test --lib services::zcode_control::relay`。Expected: 新测试因缺实现／行为断言失败；若编译先因新符号不存在失败，先补仅签名的最小桩并确认断言仍红，不接受依赖下载／环境故障充当 RED。
- [ ] **Step 3: 最小实现。** 新增 `tokio-tungstenite = {version="0.28",default-features=false,features=["connect","rustls-tls-webpki-roots"]}`，使用 connect_async（已核官方 crate 文档，非借用 axum 服务端 WS）；复用 futures，禁止关闭 TLS 校验。实现 E1 的 `wss://zcode.z.ai/api/v1/remote/ws2?token=...&mid=...`、auth_init/device_sid 回显、waiting/matched，30s 路由窗；mid 缺失可保存但连接失败只能如实报缺路由/待连接，不猜 SID 已永久失效。显式连接／断开推进 generation；matched 前不发 data，KICKED→conflict 零自动重连；网络掉线→offline，401→needs_auth。增加官方pair_status_query心跳：5,000ms基础间隔±1,000ms jitter，15,000ms pair_status_ack超时置offline并断连接，断开/KICKED后取消计时器、零自动重连；依据固定源码shared/web-remote-relay/adapters/heartbeat.ts与domain/heartbeatTiming.ts，不用短时探针无心跳冒充长驻可用。WS/HTTP 原始错误含 URL 时仅映射常量码，不直接打印。
- [ ] **Step 4: 验证 GREEN。** Run: `cd src-tauri && cargo test --lib services::zcode_control::relay`。Expected: 本任务全部新用例被发现且 PASS、0 failed（0 tests 不算通过）；确认 SID 回显不符拒绝、掉线、缺 mid、每连接新 token、过期 token、重复 connect、clear/disconnect 清理。真实 token 一次性消费仍仅 source+fake，不新增实跑结论。
- [ ] **Step 5: 提交。** 只 `git add` 本任务 Files 中实际改动路径，再 `git commit -m "feat(zcode): add single-owner official relay lifecycle"`；提交前检查暂存区无链接、凭据、正文与 MASTER-PLAN 改动。

### Task T2.1: 官方二进制 RPC 与可靠收帧

**Files:** Create `src-tauri/src/services/zcode_control/codec.rs`；Modify `mod.rs`；Create `tests/fixtures/zcode-official-bridge/wire-vectors.json`；Test codec.rs 内测试。

**Interfaces:** Produces `encode_call(id:u32,channel:&str,method:&str,args:Vec<Value>)->ZResult<Vec<u8>>`、`encode_listen(id:u32,channel:&str,event:&str,arg:Value)->ZResult<Vec<u8>>`、`decode_channel(bytes:&[u8])->ZResult<ChannelMessage>`、`encode_frame(identity:&RpcIdentity,seq:u64,messageSeq:u64,bytes:&[u8])->ZResult<Value>`、`encode_ack(identity:&RpcIdentity,ackMessageSeq:u64)->Value`、`FrameAssembler::accept(&mut self,frame:Value,nowMs:u64)->ZResult<Vec<Vec<u8>>>`；`ChannelMessage{messageType:u32,id:Option<u32>,body:Value}`；`RpcIdentity{bridgeSessionId:String,bridgeGeneration:Option<u64>,recoveryId:Option<String>}`（后两项仅按ready回显；workspaceKey/taskId属于桥映射，不是rpc-frame identity）。

- [ ] **Step 1: 写失败测试。** 以下断言分别放入同名测试；fixture 全为合成值，不读取真实账号／DB。

```rust
// initialize_golden_vector
assert_eq!(decode_channel(&[4,1,6,200,1,0]).unwrap().message_type, 200);
// call_args_are_an_array_not_a_bare_object
assert_eq!(decoded_call_body, serde_json::json!([{"sessionId":"session_test"}]));
// duplicate_fragment_delivered_once_and_bad_crc_rejected
assert_eq!(assembled_delivery_count, 1); assert_eq!(bad_crc_delivery_count, 0);
```

- [ ] **Step 2: 验证 RED。** Run: `cd src-tauri && cargo test --lib services::zcode_control::codec`。Expected: 新测试因缺实现／行为断言失败；若编译先因新符号不存在失败，先补仅签名的最小桩并确认断言仍红，不接受依赖下载／环境故障充当 RED。
- [ ] **Step 3: 最小实现。** 按 E2 移植 serialization 类型标签/VQL、header+body、rpc-frame Base64+CRC32、rpc-frame-ack；Promise=100、EventListen=102、Initialize=200、成功=201、错误=202、EventFire=204。调用 body 是参数数组；EventListen body 是原 arg，不能混用。物理信封上限 1,048,576 bytes；单条出站写命令只用单分片，超过预算在发帧前拒绝；入站支持已证分片重组、去重、超时清理，32 个并发重组、1024 暂存帧为探针既有限额；另设Tuvis本地总缓存16,777,216 bytes、10,000ms重组TTL（不是宣称官方限额），不按来帧声明的长度直接无界分配；超限拒绝并要求重同步，不能吞掉后仍宣告投影新鲜。Golden vectors 从 TS 纯函数输出的合成输入生成并与 Rust 对拍，不复制真实帧。
- [ ] **Step 4: 验证 GREEN。** Run: `cd src-tauri && cargo test --lib services::zcode_control::codec`。Expected: 本任务全部新用例被发现且 PASS、0 failed（0 tests 不算通过）；包含中文/emoji、乱序/缺片/重复/CRC错、错误 identity、超长帧、截断 VQL、Initialize golden vector；坏帧不交付，ACK不等于业务成功。
- [ ] **Step 5: 提交。** 只 `git add` 本任务 Files 中实际改动路径，再 `git commit -m "feat(zcode): implement verified rpc wire codec"`；提交前检查暂存区无链接、凭据、正文与 MASTER-PLAN 改动。

### Task T2.2: 工作区桥、任务索引、会话精确投影

**Files:** Create `src-tauri/src/services/zcode_control/{bridge,projection,pending}.rs`；Modify `types.rs`、`mod.rs`；Test 各模块内 tests。

**Interfaces:** Consumes T1.2 RelayClient、T2.1 codec。Produces `BridgeManager::new(relay:Arc<RelayClient>)->Self`、`async on_data(&self,payload:Value)->ZResult<()>`、`async invalidate(&self,generation:u64)`、`async bind(&self,key:&TargetKey)->ZResult<BoundTarget>`、`async session(&self,target:&BoundTarget)->ZResult<SessionView>`、`async call(&self,target:&BoundTarget,channel:&str,method:&str,args:Vec<Value>)->ZResult<Value>`；`Projection::new(target:BoundTarget)->Self`、`Projection::apply(&mut self,generation:u64,frame:Value)->ZResult<()>`、`PendingBook::has_confirmed_control(&self)->bool`。bridge actor 唯一管理所有逻辑桥的 requestId/seq/messageSeq 和订阅。

- [ ] **Step 1: 写失败测试。** 以下断言分别放入同名测试；fixture 全为合成值，不读取真实账号／DB。

```rust
// ack_only_does_not_confirm_control
assert!(!pending.has_confirmed_control());
// explicit_empty_control_is_known
assert!(pending_with_active_works_empty.has_confirmed_control());
// same_title_different_workspaces_is_not_a_mapping
assert!(mapper_for_duplicate_exact_task_ids.bind(&key).is_err());
// foreign_topic_or_old_subscription_is_rejected
assert_eq!(foreign_and_stale_frames_applied, 0);
// event_before_subscribe_ack_is_staged_not_lost
assert_eq!(matching_staged_frames_delivered_after_ack, 1);
```

- [ ] **Step 2: 验证 RED。** Run: `cd src-tauri && cargo test --lib services::zcode_control::bridge && cargo test --lib services::zcode_control::projection && cargo test --lib services::zcode_control::pending`。Expected: 新测试因缺实现／行为断言失败；若编译先因新符号不存在失败，先补仅签名的最小桩并确认断言仍红，不接受依赖下载／环境故障充当 RED。
- [ ] **Step 3: 最小实现。** 按 E2 顺序：bootstrap/workspace-list → 工作区 bridge-open（requestId/bridgeSessionId/workspaceKey；枚举期taskId省略，绑定会话后携taskId）→ ready 精确回显 → Initialize(200) → zcode-agent.helloConversationV4 → initializeConversationV4(clientHello)；wire protocolVersion=3，客户端身份 clientKind=web，不伪装 desktop-continuous 或提升权限。WindowController 需先桥接、EventListen(onDynamicControllerFrame) 再 subscribeControllerV4 获取 controller/tasks-index，不能发明 tasks-index-request。会话为 EventListen(onDynamicConversationFrame,workspace)→subscribeConversationV4→ACK后本地 `activate(subscriptionId)` 放行暂存帧（这是本地barrier，不存在activateConversationV4 RPC）；按 session/topic/subscriptionId/代际/seq 处理 snapshot+deltas。缺序/未知 schema/协议不符置 ready=false，重取只读 snapshot，期间零写。bind 只认精确 taskId==Tuvis 原 sessionId，workspace 取宿主地址，冲突拒绝。沿用探针 10s bridge-open、15s Initialize、10s RPC；subscription ACK 不代表快照/控制已到。
- [ ] **Step 4: 验证 GREEN。** Run: `cd src-tauri && cargo test --lib services::zcode_control::bridge && cargo test --lib services::zcode_control::projection && cargo test --lib services::zcode_control::pending`。Expected: 本任务全部新用例被发现且 PASS、0 failed（0 tests 不算通过）；覆盖多工作区、同标题、两个 ID 形态、缺 identity、错回显、错 protocolVersion、ACK前事件、缺序重同步、旧代际、旧 subscription、缺 control 与空数组。此任务只读，不发送 v4 写命令。
- [ ] **Step 5: 提交。** 只 `git add` 本任务 Files 中实际改动路径，再 `git commit -m "feat(zcode): bind workspace bridges and project verified sessions"`；提交前检查暂存区无链接、凭据、正文与 MASTER-PLAN 改动。

### Task T3: 一次性发送、去重账本与审计

**Files:** Create `src-tauri/src/services/zcode_control/{command,action,audit}.rs`；Modify `types.rs`、`mod.rs`、`src-tauri/src/database/schema.rs`；Test 模块内 tests。

**Interfaces:** Consumes BoundTarget、BridgeManager.call、现有 `write_audit::record_conn_checked`。Produces `CommandSender::new(bridge:Arc<BridgeManager>,store:DeviceStore)->Self`、`async execute(&self,ctx:&ActionContext,target:&BoundTarget,action:&ControlAction)->ZResult<ActionReceipt>`；`ActionStore::reserve(&self,ctx:&ActionContext,target:&BoundTarget,payloadHash:&str)->ZResult<Reservation>`，`Reservation=New|Existing(ActionReceipt)`；`ActionStore::record_result(&self,receipt:&ActionReceipt)->ZResult<()>`；模块统一生成信封，不让 HTTP 传任意 wire；`new_command_id()->String` 生成rand128 UUIDv4（参照既有codex types.rs:new_uuid，不新增uuid依赖）；`compose_official_text(deviceName:&str,text:&str,signatureOn:bool)->ZResult<String>` 为本路线唯一正文出口。

- [ ] **Step 1: 写失败测试。** 以下断言分别放入同名测试；fixture 全为合成值，不读取真实账号／DB。

```rust
// same_action_id_same_payload_returns_receipt_without_resend
assert_eq!(wire_send_text_count, 1); assert_eq!(second.action_id, first.action_id);
// action_id_reused_with_different_body_is_conflict
assert_eq!(reused_id_error.code, "action_payload_conflict");
// dropped_ack_after_write_is_unknown_and_never_resent
assert_eq!(lost_ack.state, ActionState::Unknown); assert_eq!(retry_wire_count, 0);
// audit_storage_failure_is_zero_write
assert_eq!(commands_after_db_failure, 0);
// official_text_preserves_newlines_and_optional_device_signature
assert_eq!(compose_official_text("phone", "第一行\n第二行", false).unwrap(), "第一行\n第二行");
// accepted_is_not_confirmed_by_unrelated_activity
assert_eq!(unrelated_activity_receipt.state, ActionState::Accepted);
```

- [ ] **Step 2: 验证 RED。** Run: `cd src-tauri && cargo test --lib services::zcode_control::action && cargo test --lib services::zcode_control::command`。Expected: 新测试因缺实现／行为断言失败；若编译先因新符号不存在失败，先补仅签名的最小桩并确认断言仍红，不接受依赖下载／环境故障充当 RED。
- [ ] **Step 3: 最小实现。** 实现 E3 信封 `{commandId,clientId,sessionId,type:"sendText",payload:{text},issuedAt}`，sendText 不带 CAS，不强塞 mode/model；单个逻辑写命令仅一次发出。普通正文保留实际换行/中文/emoji，不调用终端normalize_newlines把换行变字面\n；按现有remote_message_signature缺省off/on设置决定普通消息后缀` [mobile <设备花名>]`，读取单点复用message_signature_enabled_conn，设备名取可信DB；行首斜杠不加后缀但不在本批承诺新增slash能力。拒绝纯空白/超预算文本，模型payload不混入终端控制字符处理。数据库新建幂等 `zcode_actions`：action_id 主键、device_id/session_id/generation、payload_hash、state、reason_code、预留/更新时间；不存正文/JWT/URL。payload hash 包含 kind+session+generation+所有动作参数，跨设备同 ID 不泄漏旧回执。预留和起始审计同事务，写前先置保守 unknown/已开始状态，崩溃恢复只返回 unknown 不派发；ACK 窗20s，已 accepted 不降成“发送失败”，无因果终态保持 accepted。审计 channel=`zcode_official`，summary 只记 kind+字数或选项数量，不摘正文；send/answer/stop/permission/selection 共用同一 execute。
- [ ] **Step 4: 验证 GREEN。** Run: `cd src-tauri && cargo test --lib services::zcode_control::action && cargo test --lib services::zcode_control::command`。Expected: 本任务全部新用例被发现且 PASS、0 failed（0 tests 不算通过）；重复/并发/重启/换body/跨设备、DB失败、写后断网、ACK丢失均有计数断言；不存在第二次 sendText 或自动降级 CLI。schema 初始化幂等且老库能升级。
- [ ] **Step 5: 提交。** 只 `git add` 本任务 Files 中实际改动路径，再 `git commit -m "feat(zcode): persist idempotent control actions and audited sends"`；提交前检查暂存区无链接、凭据、正文与 MASTER-PLAN 改动。

### Task T4.1: 问答原值映射与宿主终态

**Files:** Modify `src-tauri/src/services/zcode_control/{pending,command,types}.rs`；Test pending.rs/command.rs 内 answer 测试。

**Interfaces:** Consumes T2.2 PendingBook、T3 execute。Produces `build_user_answer(pending:&PendingView,answers:&[AnswerEntry])->ZResult<Value>`、`PendingBook::authorize(&mut self,target:&BoundTarget,interactionId:&str)->ZResult<PendingView>`；execute 的 answer 分支只调用这两个入口，发送前再次验证宿主仍在场及锚未变。

- [ ] **Step 1: 写失败测试。** 以下断言分别放入同名测试；fixture 全为合成值，不读取真实账号／DB。

```rust
// single_select_keeps_exact_question_and_value
assert_eq!(answer["content"]["answers"]["原题？"], "原值 A");
// multi_select_uses_host_values_not_labels
assert_eq!(answer["content"]["answer_0"], serde_json::json!(["A","B"]));
// free_text_matches_broker_contract
assert_eq!(text_answer["content"]["answers"]["原题？"], "用户文本");
// stale_or_consumed_pending_never_sends
assert_eq!(stale_resolution_count, 0);
// local_consume_is_not_host_cleared
assert_eq!(ack_only.state, ActionState::Accepted);
// plan_approval_is_not_a_question_auto_approval
assert_eq!(plan_approval_answer_frames, 0);
// sensitive_question_never_persists_draft
assert_eq!(sensitive_draft_writes, 0);
```

- [ ] **Step 2: 验证 RED。** Run: `cd src-tauri && cargo test --lib services::zcode_control::pending && cargo test --lib services::zcode_control::command::tests::answer`。Expected: 新测试因缺实现／行为断言失败；若编译先因新符号不存在失败，先补仅签名的最小桩并确认断言仍红，不接受依赖下载／环境故障充当 RED。
- [ ] **Step 3: 最小实现。** 投影按固定snapshot.ts:242–297解析：permission.summary/toolName用于让用户知道在审批什么；问答questions[].question/header/options[].value/label/description/preview保留，multiSelect缺省false，freeText取payload.freeText。只支持已建模AskUserQuestion形态；schema.interaction=plan_approval或MCP elicitation等其他子类只读并给needs_evidence，绝不能看见一个Approve选项就按问答代批计划。按 E4/E7 构造 `resolveInteraction{interactionId,answer:{action:"accept",content}}`，题干全文作为 answers 键，兼容 answer_i 和单题 answer；多选使用原 value 数组/逗号空格归一，自由文本使用 content.answers 原题键。拒绝不在宿主集合的选项、重复题干歧义、非文本对象、未支持 schema、未允许自由文本；不猜空 answers 意味用户同意继续。所有题答案合成一次 resolve。sensitive=true按宿主要求走密码输入、禁本地草稿/历史/日志/审计正文，离开或宿主撤卡清输入；普通答案同样不落动作账本正文。新增snooze动作分支，首次有效编辑就调用一次snoozeInteractionAutoResolution；提交answer前若仍未snoozed则先走同一幂等分支，确认 snooze 受理且卡仍在再 resolve；失败/unknown 则零 resolve。本地消费与宿主 pending 集合独立；ACK 后观察10s，只有 hostPendingRevision 前进且该 id 消失才 confirmed。代码包含三种形态；singleSelect/multiSelect/freeText 开闸分别受 T8.2 约束，不能按 userInput=true 全开。
- [ ] **Step 4: 验证 GREEN。** Run: `cd src-tauri && cargo test --lib services::zcode_control::pending && cargo test --lib services::zcode_control::command::tests::answer`。Expected: 本任务全部新用例被发现且 PASS、0 failed（0 tests 不算通过）；错会话/锚/代际/重复不消费别卡；snooze 未证实零resolve；ACK-only维持accepted；撤卡前进才confirmed；多选/自由文本源码/fake已实现而live未过时API拒绝、零帧。
- [ ] **Step 5: 提交。** 只 `git add` 本任务 Files 中实际改动路径，再 `git commit -m "feat(zcode): resolve original question values with terminal checks"`；提交前检查暂存区无链接、凭据、正文与 MASTER-PLAN 改动。

### Task T4.2: 用户打断既有执行，与探针实验分离

**Files:** Modify `src-tauri/src/services/zcode_control/{pending,command,types}.rs`；Test command.rs 内 stop 测试；保留 scripts 探针代码不改语义。

**Interfaces:** Consumes SessionView.control、T3 execute。Produces `authorize_stop(view:&SessionView,request:&StopRequest)->ZResult<String>`，返回精确已观察 execution id；execute stop 分支携 `expectedForegroundExecutionId`。

- [ ] **Step 1: 写失败测试。** 以下断言分别放入同名测试；fixture 全为合成值，不读取真实账号／DB。

```rust
// production_stops_observed_existing_execution_without_trigger
assert_eq!(send_text_frames, 0); assert_eq!(stop_frames, 1); assert_eq!(stop_payload["expectedForegroundExecutionId"], "E-existing");
// unknown_control_is_zero_stop
assert_eq!(missing_control_stop_frames, 0);
// changed_execution_rejects_old_button
assert_eq!(stale_button_stop_frames, 0);
// ack_does_not_prove_stopped
assert_eq!(ack_only.state, ActionState::Accepted);
```

- [ ] **Step 2: 验证 RED。** Run: `cd src-tauri && cargo test --lib services::zcode_control::command::tests::stop`。Expected: 新测试因缺实现／行为断言失败；若编译先因新符号不存在失败，先补仅签名的最小桩并确认断言仍红，不接受依赖下载／环境故障充当 RED。
- [ ] **Step 3: 最小实现。** 产品“打断”只停止用户点击时新鲜 control 指定的既有前台执行，绝不为打断先合成 sendText。拿不到初始 control 时最多等10s，超时零stop；[]为空闲；多个目标无法唯一确定则拒绝，禁止随取第一个。执行前检查 session/generation/id 仍在宿主集合，宿主 stop guard 再防迟到误杀；noop stopTargetChanged 如实显示未停止。ACK后10s观察 controlRevision 前进且目标消失才confirmed，新执行出现不算目标停止证明除非原目标确已消失。探针仍保持“先确认初始control→基线→触发accepted→仅停新id”的实验保护，不移植触发逻辑到产品。
- [ ] **Step 4: 验证 GREEN。** Run: `cd src-tauri && cargo test --lib services::zcode_control::command::tests::stop`。Expected: 本任务全部新用例被发现且 PASS、0 failed（0 tests 不算通过）；fake定时器覆盖ACK先到、延迟旧快照、未知/空集、多执行、旧按钮、宿主noop、ACK-only、代际变化；所有无效条件零stop且永远零隐式sendText。真实已有执行打断须T8.2补证。
- [ ] **Step 5: 提交。** 只 `git add` 本任务 Files 中实际改动路径，再 `git commit -m "feat(zcode): interrupt only the user-selected live execution"`；提交前检查暂存区无链接、凭据、正文与 MASTER-PLAN 改动。

### Task T5: 审批既有卡的安全应答

**Files:** Modify `src-tauri/src/services/zcode_control/{pending,command,types}.rs`；Test command.rs 内 permission 测试。

**Interfaces:** Consumes PendingView.options、T3 execute。Produces `build_permission_answer(pending:&PendingView,optionId:&str)->ZResult<Value>`；返回 `{optionId:宿主原值}`，不硬编码 allowOnce 为真正的 optionId。

- [ ] **Step 1: 写失败测试。** 以下断言分别放入同名测试；fixture 全为合成值，不读取真实账号／DB。

```rust
// kind_is_authority_option_id_is_preserved
assert_eq!(build_permission_answer(&card_with_once_id_custom,"once-id-9").unwrap()["optionId"], "once-id-9");
// standing_or_full_access_never_sent
assert_eq!(full_access_or_standing_grant_frames, 0);
// no_card_means_no_approval
assert_eq!(missing_card_frames, 0);
// approval_gate_is_not_inferred_from_send
assert!(!default_capabilities.approval_reply.enabled);
```

- [ ] **Step 2: 验证 RED。** Run: `cd src-tauri && cargo test --lib services::zcode_control::command::tests::permission`。Expected: 新测试因缺实现／行为断言失败；若编译先因新符号不存在失败，先补仅签名的最小桩并确认断言仍红，不接受依赖下载／环境故障充当 RED。
- [ ] **Step 3: 最小实现。** 按 E5 的真实 kind=permission 卡，以 options[].kind=`allowOnce`/`deny` 做白名单，optionId 必须属于该卡；fullAccess 无论kind都拒绝，allowAlways/allow_session/allow_project/custom未知kind拒绝。应答和撤卡确认复用T4.1，不复用终端按键。沿用既定停止门：approvalReply 默认关闭，在T8.2“Tuvis发送→弹卡→应答”未证前只交付受测实现，不宣传guarded保护；已有卡应答证据和发送催卡证据分别记录，不混成一个true。禁止修改会话权限以造证或用fullAccess绕过。
- [ ] **Step 4: 验证 GREEN。** Run: `cd src-tauri && cargo test --lib services::zcode_control::command::tests::permission`。Expected: 本任务全部新用例被发现且 PASS、0 failed（0 tests 不算通过）；白名单、非字面optionId、重复/旧卡、默认关闭、ACK/终态都测；API关闭时零帧，UI不出现伪审批成功。若催卡仍有负结果，本批记 evidence_blocked，不能声明完整spec完成。
- [ ] **Step 5: 提交。** 只 `git add` 本任务 Files 中实际改动路径，再 `git commit -m "feat(zcode): gate permission replies on original pending options"`；提交前检查暂存区无链接、凭据、正文与 MASTER-PLAN 改动。

### Task T6: 同账号模型和思考强度

**Files:** Create `src-tauri/src/services/zcode_control/selection.rs`；Modify `command.rs`、`types.rs`、`mod.rs`；Test selection.rs 内 tests。

**Interfaces:** Consumes BridgeManager.call、SessionView当前config/revision、T3 execute。Produces `async load_selection(bridge:&BridgeManager,target:&BoundTarget)->ZResult<SelectionView>`、`build_selection_command(view:&SessionView,model:&str,thought:&str)->ZResult<Value>`；命令包含type/payload/baseRevision，由T3补其余信封。

- [ ] **Step 1: 写失败测试。** 以下断言分别放入同名测试；fixture 全为合成值，不读取真实账号／DB。

```rust
// only_target_models_own_levels_are_allowed
assert!(build_selection_command(&view,"A","only-for-B").is_err());
// switching_is_cas_and_does_not_restore
assert!(envelope.get("baseRevision").is_some()); assert!(envelope.get("baseLogEpoch").is_none()); assert_eq!(restore_frames, 0);
// no_original_thought_is_not_a_probe_cleanup_requirement
assert!(build_selection_command(&view_with_null_current_thought,"A","low").is_ok());
// empty_levels_reject_any_thought
assert!(build_selection_command(&view_with_empty_levels,"A","low").is_err());
```

- [ ] **Step 2: 验证 RED。** Run: `cd src-tauri && cargo test --lib services::zcode_control::selection`。Expected: 新测试因缺实现／行为断言失败；若编译先因新符号不存在失败，先补仅签名的最小桩并确认断言仍红，不接受依赖下载／环境故障充当 RED。
- [ ] **Step 3: 最小实现。** 按 E6 调用 channel=`model-selection` 的 getView，严格解析 providerId/models[].modelId/config.optionSpecs.reasoningLevel.values；只向手机返回当前会话provider目录，绝不接受手机指定另一provider。fallback snapshot.config.thoughtLevels只代表当前模型，不用于放行其他模型。构造type=switchModelConfig,payload={provider,model,thought}，必带最新baseRevision、禁baseLogEpoch；stale revision只刷新视图提示重试，不自动重发。ACK后等待新revision/config精确等于目标才confirmed。产品保留用户选择；删掉旧计划从探针清理误带来的“原thought为空就不许切”限制，目标thought仍必须为宿主合法显式值。
- [ ] **Step 4: 验证 GREEN。** Run: `cd src-tauri && cargo test --lib services::zcode_control::selection`。Expected: 本任务全部新用例被发现且 PASS、0 failed（0 tests 不算通过）；跨provider、未知id、模型档位交叉、空列表、null原档位、stale CAS、ACK-only、新鲜snapshot、无恢复写帧均通过；null原档位是source+fake支持，不新增live结论。
- [ ] **Step 5: 提交。** 只 `git add` 本任务 Files 中实际改动路径，再 `git commit -m "feat(zcode): switch model and reasoning within session provider"`；提交前检查暂存区无链接、凭据、正文与 MASTER-PLAN 改动。

### Task T7.1: 装配 Hub、桌面入口、手机 API 和 M11 路由边界

**Files:** Create `src-tauri/src/services/zcode_control/hub.rs`、`src-tauri/src/remote/zcode.rs`、`src-tauri/tests/zcode_control_remote.rs`；Modify `services/zcode_control/{mod,types}.rs`、`commands/zcode_remote.rs`、`lib.rs`、`errcode.rs`、`remote/{mod,server,api}.rs`、`inject/routing.rs` 及其既有断言。

**Interfaces:** Produces `ZcodeHub::new(store:DeviceStore)->Self`、`async authorize(&self,provider:OAuthProvider,open:OpenAuthorize)->ZResult<ConnectionView>`、`async connect(&self)->ZResult<ConnectionView>`、`async disconnect(&self)->ConnectionView`、`async config_changed(&self)->ConnectionView`、`connection(&self)->ConnectionView`、`async session(&self,key:&TargetKey)->ZResult<SessionView>`、`async execute(&self,ctx:ActionContext,action:ControlAction)->ZResult<ActionReceipt>`、`subscribe(&self)->broadcast::Receiver<ControlEvent>`。Test 构造 `ZcodeHub::with_parts(store:DeviceStore,auth:Arc<AuthService>,relay:Arc<RelayClient>,bridge:Arc<BridgeManager>,sender:Arc<CommandSender>,policy:CapabilityView)->Self`（policy注入只供fake测试，不向设置/IPC开放） 注入前述组件。RemoteState持有Arc<ZcodeHub>，Tauri managed state与它为同一个Arc。

- [ ] **Step 1: 写失败测试。** 以下断言分别放入同名测试；fixture 全为合成值，不读取真实账号／DB。

```rust
// unpaired_all_control_endpoints_reject
assert_eq!(unpaired_status, 403); assert_eq!(unpaired_wire_frames, 0);
// config_change_invalidates_callback_and_pending
assert!(new_generation > old_generation); assert_eq!(old_generation_action_frames, 0);
// one_hub_shared_between_desktop_and_mobile
assert_eq!(two_mobile_and_desktop_socket_count, 1);
// zcode_never_falls_back_to_headless
assert_eq!(headless_spawns, 0); assert!(!new_route_reason.contains("M11"));
// stopped_remote_or_revoked_device_cannot_write_or_receive_events
assert_eq!(frames_after_revoke, 0); assert!(revoked_sse_closed);
```

- [ ] **Step 2: 验证 RED。** Run: `cd src-tauri && cargo test --test zcode_control_remote`。Expected: 新测试因缺实现／行为断言失败；若编译先因新符号不存在失败，先补仅签名的最小桩并确认断言仍红，不接受依赖下载／环境故障充当 RED。
- [ ] **Step 3: 最小实现。** 按下表注册IPC/HTTP；所有新路由进现有/m/api/v1配对layer、no-store及既有写请求安全校验，SSE接入SseRegistry设备吊销/停止关闭机制。桌面仅用户点授权才打开浏览器，app启动不自动OAuth；保存/删除成功立即调用config_changed，先推进代际、断WS、清JWT和投影，再返回新态，DB失败不破坏原连接。远程服务关闭/应用退出关闭Hub；重启需授权；网络掉线但JWT仍有效不强说需重授权，用户显式重连，KICKED不自动抢回。旧session-send不再回“无头M11”：ZCode返回409 `official_control_required` 引导统一接口，零CLI投递；send-info给官方控制可用性；新UI只调专用actions，禁止直接把tool_gate移除使其落终端注入。
- [ ] **Step 4: 验证 GREEN。** Run: `cd src-tauri && cargo test --test zcode_control_remote && cargo test --test zcode_config_remote && cargo test --lib inject::routing`。Expected: 本任务全部新用例被发现且 PASS、0 failed（0 tests 不算通过）；无配对403、有配对no-store、错误码脱敏、配置更新/删除/失败、JWT过期、停服务、吊销、旧代际、所有既有RemoteState构造点编译；正文和文件GET仍成功。
- [ ] **Step 5: 提交。** 只 `git add` 本任务 Files 中实际改动路径，再 `git commit -m "feat(zcode): wire official control into application and paired api"`；提交前检查暂存区无链接、凭据、正文与 MASTER-PLAN 改动。

#### T7.1 对外接口冻结（Tuvis 新接口，不是官方 HTTP API）

| 入口                                                                                      | 参数／返回／约束                                                                                                                                                                                       |
| ----------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| Tauri `zcode_control_authorize(provider:String)`                                          | 内部parse为OAuthProvider；Result<ConnectionView,String>，provider非法拒绝；只在Rust调用opener                                                                                                          |
| Tauri `zcode_control_connect()` / `zcode_control_disconnect()` / `zcode_control_status()` | Result<ConnectionView,String>；不返回凭据；status只读                                                                                                                                                  |
| 原 `zcode_remote_config_get/set(raw_url:String)`                                          | 保持原DTO；set从同步改async：先持久化成功、释放DB锁、再通知同一个Hub.config_changed；失败不通知；前端invoke名/参数不变                                                                                 |
| `GET /m/api/v1/zcode/connection`                                                          | ConnectionView                                                                                                                                                                                         |
| `GET /m/api/v1/zcode/session?sessionId=<原ID>`                                            | SessionView；只查当前Tuvis会话快照可定位的ZCode会话，绑定由服务端完成，不接收workspacePath                                                                                                             |
| `POST /m/api/v1/zcode/actions`                                                            | `{actionId,action:ControlAction}` → ActionReceipt；错误为 `{error:常量码}`；未配对403、无会话404、旧代际/未就绪/能力关闭409、参数错400、审计DB错500；accepted/confirmed/noop/unknown均200并按state区分 |
| `GET /m/api/v1/zcode/events?sessionId=<原ID>`                                             | 已配对SSE；发连接/本会话/本设备动作；首帧状态，lag发resync后客户端GET；同源设备撤销即断，不广播别的手机动作id                                                                                          |

新增trait/HTTP handler只做适配，不重写command构造；GET不自动OAuth、不隐式新建第二条relay。Hub只接受现有快照里同agent_type=zcode的sessionId，旧会话/隐藏归档不可因为手机自报ID而越权恢复。session-send旧入口409属于有意兼容拒绝；新MessageComposer测试必须证明它不会再走这里。

### Task T7.2: Tuvis 统一控制 UI 与按能力开放

**Files:** Create `src/lib/api/zcodeControl.ts`、`src/mobile/{useZcodeControl,ZcodeControlPanel}.tsx`、`tests/mobile/zcodeControlApi.test.ts`、`tests/mobile/ZcodeControlPanel.test.tsx`；Modify `src/mobile/api.ts`、`src/mobile/{MessageComposer,SessionDetail}.tsx`、`src/components/settings/ZcodeRemoteSettingsCard.tsx`、`src/i18n/locales/{zh,en}.json`、`tests/settings/zcodeRemoteSettings.test.tsx`。

**Interfaces:** Consumes T7.1 DTO/路由。Produces `getZcodeConnection():Promise<ConnectionView>`、`getZcodeSession(sessionId:string):Promise<SessionView>`、`postZcodeAction(actionId:string,action:ControlAction):Promise<ActionReceipt>`、`useZcodeControl(sessionId:string):{connection:ConnectionView|null,session:SessionView|null,submit:(action:ControlAction)=>Promise<ActionReceipt>,reason:string|null}`；`ZcodeControlPanel({sessionId:string})`。前端types只在zcodeControl.ts定义一次，与Rust golden JSON契约测试对拍；不假称TS能直接import Rust类型。

- [ ] **Step 1: 写失败测试。** 以下断言分别放入同名测试；fixture 全为合成值，不读取真实账号／DB。

```tsx
// saved_never_implies_connected
expect(savedWithoutAuthSendButton).toBeDisabled();
// send_uses_official_api_not_legacy_or_headless
expect(postedPath).toBe("/m/api/v1/zcode/actions");
expect(legacySendCalls).toBe(0);
// accepted_and_unknown_keep_distinct_copy
expect(acceptedText).toBe("已受理");
expect(unknownText).not.toContain("已生效");
// failure_preserves_draft_reading_stays_visible
expect(composer).toHaveValue("待发送草稿");
expect(messageAndFilePreview).toBeVisible();
// unsupported_shape_is_disabled_with_reason
expect(multiSelectButton).toBeDisabled();
expect(reason).toBeVisible();
```

- [ ] **Step 2: 验证 RED。** Run: `pnpm exec vitest run tests/mobile/ZcodeControlPanel.test.tsx tests/mobile/zcodeControlApi.test.ts tests/settings/zcodeRemoteSettings.test.tsx`。Expected: 新测试因缺实现／行为断言失败；若编译先因新符号不存在失败，先补仅签名的最小桩并确认断言仍红，不接受依赖下载／环境故障充当 RED。
- [ ] **Step 3: 最小实现。** 设置卡保留折叠/教程/链接字段，新增官方账号授权通道选择、授权/连接/断开与真实连接态（ZAI未经本平台live时禁用并注明；首轮提供BigModel已证通道）；授权动作走IPC不向JS返回URL token。手机MessageComposer复用样式，ZCode分支避开旧发送/H3资格判断；问答、审批、打断、模型/档位使用Tuvis卡面，数据来自新SessionView，不把官方pending喂给终端按键API。问答保留原选项显示文案与value绑定；sensitive=true显示密码输入且不进入普通composer的草稿缓存，卸载/换会话清空；问答首次有效选择/输入触发snooze动作，等待其受理后才允许最终提交；期间保留本地草稿，最终卡状态仍以宿主事件为准。模型/档位当前选择保持、错误保留草稿、accepted只显示已受理；只在confirmed显示已生效；send accepted可清本次已受理草稿但不得写成执行完成，send_failed/rejected/unknown保留草稿并禁止unknown自动重试。SSE更新后重GET纠偏，lag/代际变化清旧pending，组件卸载关闭SSE；阅读组件不依赖控制成功。手机不拿JWT、不编辑链接；提示缺配置/需授权/离线/冲突/未知/失效分别就地呈现；审批gate未过不展示可用批准。
- [ ] **Step 4: 验证 GREEN。** Run: `pnpm exec vitest run tests/mobile/ZcodeControlPanel.test.tsx tests/mobile/zcodeControlApi.test.ts tests/mobile/MessageComposer.test.tsx tests/settings/zcodeRemoteSettings.test.tsx && pnpm build:mobile`。Expected: 本任务全部新用例被发现且 PASS、0 failed（0 tests 不算通过）；覆盖所有连接状态、权限形态、重挂/失焦、SSE重连/换代、same-action双击、zh/en文案；桌面状态变化不揭示PIN、不改变六段布局，H3关闭时官方send仍可用。
- [ ] **Step 5: 提交。** 只 `git add` 本任务 Files 中实际改动路径，再 `git commit -m "feat(mobile): add unified zcode control with truthful action states"`；提交前检查暂存区无链接、凭据、正文与 MASTER-PLAN 改动。

### Task T8.1: 生产链假服务集成与 A 包回归

**Files:** Create `src-tauri/tests/zcode_control_e2e.rs`、`src-tauri/tests/common/zcode.rs`、`tests/mobile/ZcodeControlFlow.test.tsx`；Modify 必要测试注入构造，不增加生产测试后门。

**Interfaces:** Consumes Hub及HTTP/UI全部接口。Produces 仅测试 `FakeOfficialServer`（loopback HTTP+WS、记录命令、可推进事件/断线）与浏览器mock；测试连接端点只能通过with_parts注入，不能经设置或手机覆盖生产域名。

- [ ] **Step 1: 写失败测试。** 以下断言分别放入同名测试；fixture 全为合成值，不读取真实账号／DB。

```rust
// two_devices_same_action_single_dispatch_across_disconnect
assert_eq!(official_send_text_frames, 1); assert_eq!(second_receipt.action_id, first_receipt.action_id);
// full_chain_reads_are_independent_of_control_failure
assert_eq!(messages_status, 200); assert_eq!(file_preview_status, 200);
// no_initial_control_timeout_zero_stop_and_no_trigger
assert_eq!(timeout_stop_frames, 0); assert_eq!(timeout_trigger_frames, 0);
// sse_lag_resyncs_before_next_write
assert_eq!(writes_while_projection_not_ready, 0);
```

- [ ] **Step 2: 验证 RED。** Run: `cd src-tauri && cargo test --test zcode_control_e2e`。Expected: 新测试因缺实现／行为断言失败；若编译先因新符号不存在失败，先补仅签名的最小桩并确认断言仍红，不接受依赖下载／环境故障充当 RED。
- [ ] **Step 3: 最小实现。** 组装真实生产Rust组件与fake官方端，覆盖授权→relay→bridge→snapshot→手机发送→受理/终态→审计，以及换链接/吊销/断线/代际/多手机。复用A已有zcode_config_remote/zcode_read_remote、settings测试，证明新旧ID/附件锚、六段/PIN/默认值不回归。fake定时器验证有界等待到期零写，不用真实sleep。安全扫描仅输出违规类型/文件位置，不输出匹配的secret；检查网络错误URL、DB、日志、HTTP/SSE、用户文案均无植入的合成token/正文。
- [ ] **Step 4: 验证 GREEN。** Run: `cd src-tauri && cargo test --test zcode_control_e2e --test zcode_control_remote --test zcode_read_remote --test zcode_config_remote`。Expected: 本任务全部新用例被发现且 PASS、0 failed（0 tests 不算通过）；全部fake链与A回归通过；再在根目录跑 `pnpm test`、`pnpm check`、`pnpm build:mobile`，在src-tauri跑 `cargo test --features marker-helper`、`cargo fmt --check`、`cargo clippy --all-targets --features marker-helper -- -D warnings` 全绿后才进入T8.2。
- [ ] **Step 5: 提交。** 只 `git add` 本任务 Files 中实际改动路径，再 `git commit -m "test(zcode): verify production control flow against fake official host"`；提交前检查暂存区无链接、凭据、正文与 MASTER-PLAN 改动。

### Task T8.2: 实机证据开闸、双平台验收与文档收口

**Files:** Create `docs/superpowers/reviews/2026-10-25-zcode-production-acceptance.md`；Modify `tests/fixtures/zcode-official-bridge/contract.json`、决策记录追加段、`src-tauri/src/services/zcode_control/{types,evidence}.rs`中的证据策略、同目录`mod.rs`、`src-tauri/Cargo.toml`、`docs/user-manual/{zh,en}/08-mobile-remote.md`、`CHANGELOG{,.en}.md`；发版时再同步 `docs/capabilities.json`、README中英矩阵及release-notes。

**Interfaces:** Consumes T8.1 已绿生产链。Produces `CapabilityPolicy::for_host(appVersion:&str,protocolVersion:u32)->CapabilityView`；key 不只看App版本，还要求schema/hello能力匹配，未知组合默认关闭并给needs_evidence。验收表每行含平台/APP build/生产commit/run id/目标别名/动作/预期/实际/终态/判决，不写SID/JWT/正文。

- [ ] **Step 1: 写失败测试。** 以下断言分别放入同名测试；fixture 全为合成值，不读取真实账号／DB。

```rust
// policy_never_uses_broad_probe_true_as_production_enable
assert!(!policy_without_live_terminal_evidence.single_select.enabled);
assert!(!policy_without_live_terminal_evidence.interrupt.enabled);
assert!(!policy_without_live_shape_evidence.multi_select.enabled);
assert!(!policy_without_terminal_send_approval.approval_reply.enabled);
// unsupported_protocol_stays_readable_but_control_disabled
assert!(!unknown_protocol_policy.send.enabled);
```

- [ ] **Step 2: 验证 RED。** Run: `cd src-tauri && cargo test --lib services::zcode_control::types::tests::policy`。Expected: 新测试因缺实现／行为断言失败；若编译先因新符号不存在失败，先补仅签名的最小桩并确认断言仍红，不接受依赖下载／环境故障充当 RED。
- [ ] **Step 3: 最小实现。** 先写/跑policy RED，再在专用会话用已完成产品UI逐行执行下表（未开能力使用下述仅debug单次验收许可，不要求先伪造生产证据）；用户本人官方登录并指定会话，Agent执行已授权动作，手机体验由用户确认。实跑后先补脱敏fixture与能力测试，再最小更新policy使对应case GREEN，不能只改文档布尔值。无账号/无Windows/无法制造安全pending时标not_run/blocked，不伪造PASS。T5催卡负结果未消除时保持关闭并明确spec缺口；不改权限或源码造卡。文档只描述本批真实交付，包含内存授权重启重登、KICKED、链接失效、guarded限制和只读持续可用；本任务不执行发版。
- [ ] **Step 4: 验证 GREEN。** Run: `cd src-tauri && cargo test --lib services::zcode_control::types::tests::policy && cargo test --test zcode_control_remote --test zcode_control_e2e`。Expected: 本任务全部新用例被发现且 PASS、0 failed（0 tests 不算通过）；下表全部强制项有证据；补证引起代码变化后重跑T8.1全门禁。若任何能力门未过，整批只能标“部分实现/能力受限”，不得称满足全部spec。
- [ ] **Step 5: 提交。** 只 `git add` 本任务 Files 中实际改动路径，再 `git commit -m "docs(zcode): record platform acceptance and capability gates"`；提交前检查暂存区无链接、凭据、正文与 MASTER-PLAN 改动。

#### T8.2 未开闸能力如何实跑

`evidence.rs` 只在 `all(debug_assertions,feature="zcode-bridge-evidence")` 编译；在始终编译的mod.rs加入release+该feature的compile_error，与既有codex-ipc-e2e-stop纪律相同。桌面专用IPC `zcode_control_arm_evidence(session_id:String,generation:u64,capability:String)->Result<(),String>` 发放内存 `EvidencePermit{sessionId,generation,capability,expiresAtMs,consumed}`；有效120,000ms、一次用户动作、限定已配对设备操作、换代/关服务立即失效，不能覆盖缺卡、错目标、fullAccess等协议守卫。snooze为该permit的一次辅助操作：首次snooze锁定interactionId且不消费permit，最终answer原子消费permit；重复snooze/其他interaction不得借用。每条命令仍只发送一次。手机明确显示“验收模式”，不开放手机发放许可接口，不落settings。

T8.2测试另覆盖 `release_evidence_build_rejected`、`evidence_permit_single_use_and_expires`、`evidence_permit_does_not_bypass_target_or_permission_guards`。验收构建命令：`pnpm tauri dev --features hook-listener,zcode-bridge-evidence`；普通`pnpm tauri:dev`无该许可。先由用户指定测试会话并授权动作，才可在桌面发放；专用run只证明该形态，成功后才更新普通生产policy。此许可是Tuvis验收设施，不是官方协议功能，也不把未证能力永久打开。

## T8.2 强制实机验收矩阵

每行都对 macOS 与 Windows 运行生产构建验证；不能用Mac探针报告替代Windows产品验收。能力开关按已验环境开放，不以版本字符串相等替代实跑。

| 场景                           | 可判定的 PASS 条件                                                                                                                            |
| ------------------------------ | --------------------------------------------------------------------------------------------------------------------------------------------- |
| 官方授权／连接                 | BigModel本人登录后Tuvis显示paired，目标APP可路由；重启后needs_auth且无旧JWT；ZAI通道若要标支持，必须独立通过，未跑则禁用入口并说明            |
| 手机发送到已有APP              | 指定session原ID；点一次发送；官方APP看到同一条合成marker且被处理；手机显示受理，不把任意活动当该动作完成；审计一次业务动作                    |
| 单选／多选／自由文本（各一行） | 原题/原值回传；宿主pending撤卡且宿主后续行为与所选/所填一致；每形态单独run；含多题/重复题干形态的支持或明确拒绝，不误答                       |
| 打断既有执行                   | 用户先在APP启动专用可安全终止任务；Tuvis不额外sendText，停止所选executionId；宿主control中该id消失且APP实际停止；新执行不受误伤               |
| 审批既有卡批准／拒绝           | 原optionId；一次性批准后预设临时产物变化，拒绝后产物不变，宿主撤卡；fullAccess与永久授权按钮不可用                                            |
| Tuvis发送→审批                 | 在不改权限/不走fullAccess前提下，Tuvis发起动作确实触发permission卡并能应答；若仍直接执行，记录负结果，保持approvalReply关闭，整批spec未全闭合 |
| 模型／思考强度                 | 同provider各切一次，新鲜config==目标；APP与Tuvis一致；离开再回来保留用户选择，无自动恢复                                                      |
| 两台手机                       | 共用一条relay；同actionId重复请求只一条上游写命令；不同动作不抢seq、不误答对方卡；设备来源审计正确                                            |
| 网络断线／ACK丢失／重启        | 无自动重发；结果按accepted/unknown如实呈现；用户重连重新绑定；旧pending不可提交，阅读仍正常                                                   |
| 失效链接／更新／删除           | 结构化设备拒绝与网络离线分开；控制区就地提示；更新/删除使旧代际失效；正文和文件仍可读；外部Not Found不被伪装自动检测                          |
| 官方网页同时连接               | KICKED→conflict，零自动互抢；用户显式恢复前不重连                                                                                             |
| A包实机回归                    | 六段顺序、默认折叠、教程、PIN眼睛/失焦隐藏、两个缺键默认true及显式false、新旧session上板、正文/附件锚全部保持                                 |

## Spec 覆盖与完成判据

| spec设计点                               | 本批落实／验收归属                                                              |
| ---------------------------------------- | ------------------------------------------------------------------------------- |
| §1–§3 统一UI、官方协议、不开隐藏浏览器   | T1–T7完整生产链；T8证明“从Tuvis发”而非仅探针能发                                |
| §4.1–§4.2 六段/折叠/配置归属/教程        | A既有；T7.2接真实连接态；T8回归，不另做手机配置编辑                             |
| §4.3 格式校验/错误分类/局部降级          | 保持config；T1.2、T7.1、T7.2与失效矩阵                                          |
| §5.1 发消息、审批、问答、打断、模型/档位 | T3/T4/T5/T6/T7；单选多选自由文本均含代码和各自证据门，不拿一个true全开          |
| §5.2 精确绑定/四元组/代际/单owner/不双发 | T1.2/T2.2/T3/T4/T7.1；SSE初始化与lag由T7.1/T7.2覆盖                             |
| §5.3/§8.1 接入证据与人机分工/双平台      | E1–E7是基线，缺失终态/形态/平台由T8.2在本批末期补证，不再另留一个未执行生产计划 |
| §6 读取与附件                            | A既有；T8.1回归+T8.2真机；控制失效不改变读路径                                  |
| §7 默认值/PIN                            | A既有；T8回归；H3关不挡官方控制（T7.2）                                         |
| §8 凭据/脱敏/未知结果/文档               | T1/T3/T7/T8；手册zh/en与账本，发版矩阵另按发布纪律同步                          |

**完成状态必须分开记：** `code_complete`=本批生产代码+自动门禁全绿；`platform_validated`=双平台/手机实机证据齐；`spec_complete`=全部强制实机行通过且能力实际开放。T5发送催卡、问答/打断终态、多选/自由文本等任一门失败时，前两者也不能替代第三者。

当前已知不能承诺的点：terminal发送审批催卡有负结果；ZAI回环、Windows官方桥及多选/自由文本/answer和interrupt终态尚未实跑。它们在本批**有明确施工/补证任务**，但计划不能保证第三方实现一定支持。若失败，只能交付明确受限能力并记录缺口；不得默改spec、放宽guard、把fake当live、或把“门按预期关闭”写成该功能完成。

## 执行与自审记录

- [x] 逐节核对spec并映射到上表；A已实现项纳入回归，生产装配/M11/授权UI/事件/文档均有归属。
- [x] 所有任务包含Files/Interfaces、RED断言、RED命令、最小实现、GREEN命令/结果、提交点；验收不再仅写“全绿”。
- [x] 统一方法/类型名：官方RPC与Tuvis新接口分开；workspace/controller/topic方法按E2，CAS仅对应命令；不直接复制探针实验语义。
- [x] 五项Review Focus分别有任务与测试；源码/fake/live分层，假服务不冒充官方服务端语义。
- [x] 本次未改产品/MASTER-PLAN、未运行真实连接；未把本计划复核写成生产测试通过。

按用户此前选择继续使用子代理逐任务执行。计划需先由用户审阅确认覆盖预期，再开始实施；这是 writing-plans 的交接步骤，不是在每个普通实现动作前重复请求许可。
