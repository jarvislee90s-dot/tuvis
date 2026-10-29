# ZCode 远程能力对齐与 build 兼容 · Spec

日期：2026-10-28.
状态：待用户审阅；未批准实现。
基线：`codex/zcode-unified-remote`（ZCode 实现分支）；写稿时工作树停在 `probe/dsh-native-rpc-macos` 探测分支，执行前切到实现分支。
参考：`origin/main` 的 Codex Desktop IPC 兼容模型、ZCode 官方合同与既有探测 run。

## 1. 一句话

把 ZCode 的八类远程动作从“生产环境全部 needs_evidence 关闭”改成 **build 级静默能力解析**：接口能匹配就先开放；用户第一次真实使用该动作时做 ACK + 终端确认；失败按 build+能力降级；手机端可在冲突态断开并抢回控制权。不增加用户可见的探测页面，不额外发送测试消息。

## 2. 为什么

- 官方 ZCode 页面与 Tuvis 是**同一个单 owner 终端角色**。打开官方页面会把 Tuvis 踢成 `conflict`；手机当前没有断开/抢回入口，只能回桌面。
- 当前 `CapabilityPolicy::for_host` 即使 build/protocol/schema/hello 全匹配也返回八项全关 `needs_evidence`；合同里的 `capabilities: true` 只是探针观察，不是生产能力。
- 官方 build 会升级。若每次都等 shipped 表，升级后所有功能都会变灰；若直接信合同 true，又会把未验证能力放出去。
- Codex APP 已经有一套可借鉴的模型：exact-build profile、证据等级、账本、读探测、ACK ≠ 生效、未知 build 保守兼容。ZCode 需要把这套模型落到自己的协议和动作上。

## 3. 目标 / 非目标

### 目标

1. 八项能力按 build 证据逐项开放：`send / single_select / multi_select / free_text / interrupt / approval_reply / model / thought_level`。
2. 未知 build 不再直接全关：接口形状匹配 + 真实目标存在 → 候选开放；首次真实动作确认。
3. 能力判断可追溯、可失效、可降级；build/协议/schema/probe epoch 变化即重算。
4. 手机冲突态可「断开并抢回」，不用回桌面。
5. UI 只展示可用/不可用及原因；不展示探测页面/探测按钮/兼容徽标。

### 非目标

- 不追求 Tuvis 与官方页面同时控制同一会话（协议单 owner）。
- 不修改官方协议/服务端。
- 不在后台发送探针文本，不自动审批/打断/切模型。
- 不把本地文件池上传伪装成官方附件能力；本轮（2026-10-29 用户确认）接入 Tuvis 通用附件上传（`uploadAttachment` → `/m/api/v1/session-attachment`，落盘 `.tuvis-attachments/<session_id>/`，正文按 `<file path>`/`<image path>` 约定引用，经官方 actions/sendText 发送），不实现官方原生附件协议。
- 不重写 relay/bridge/command 主链；只在能力解析与手机抢回处增量改造。

## 4. 已知事实

### 4.1 ZCode 八项与现有证据

| 能力 | 代码 | 合同/实测 | 结论 |
|---|---|---|---|
| send | 有 | accepted + 用户可见 + agent 回复 | 强证据 |
| single_select | 有 | ACK accepted；主机撤卡未观察 | 弱，需首次确认 |
| multi_select | 有 | 仅 fake/合同 | 未实连 |
| free_text | 有 | 仅 fake/合同 | 未实连 |
| interrupt | 有 | ACK accepted；执行停止未观察 | 弱，需首次确认 |
| approval_reply | 有 | 回答既有 permission 卡 approve/deny 已实测；发送触发审批卡为负结果 | 既有卡可，elicitation 不可 |
| model | 有 | 同 provider 切换 + 新 snapshot 确认 | 强 |
| thought_level | 有 | 目标模型档位 + 新 snapshot 确认 | 强 |

合同 `capabilitiesOpened: []`，明确“宽泛 true 不复制”。生产 `CapabilityPolicy` 当前全关。

### 4.2 冲突根因

- 手机/桌面 `connect()` 对 `Conflict` 直接返回 `conflict`；只有 `disconnect()` 后重新 `connect()` 才能反抢。
- 手机 `ZcodeControlPanel` 没有断开/抢回按钮；桌面设置卡有断开，但连接按钮在 conflict 下禁用。
- 官方页面是另一个同 role 控制端，不是 Tuvis 的内嵌视图；打开它必然交接 owner。

### 4.3 Codex 可借鉴点

- `ProtocolProfile`：exact build → 能力集；`EvidenceLevel = Shipped | Unverified | Closed`。
- 未知 build → compat send/state；空 build → closed。
- 读探测惰性、单飞、有界、账本缓存；build/协议/epoch 变化失效。
- ACK 不等于生效；审批看 pending 消失，打断看守卫消失。
- 兼容发送用户显式开关；destructive 能力证据门控。

ZCode 的不同：未知 build 采用 **A 策略**，接口匹配 + 真实目标即候选开放；首次真实动作确认。比 Codex 的未知 build 更积极，但不发额外探针文本。

## 5. 功能需求

### R1 手机断开并抢回

- 冲突态显示「断开并抢回」。
- 流程：确认 → `disconnect`（取消在途握手、等 connect_lock、关旧链）→ `connect_requested`（保留 JWT、auto=true、不 OAuth）→ 刷新连接/会话。
- 只尝试一次；仍 conflict 则提示关闭官方页后重试，不循环。
- 端点：`POST /m/api/v1/zcode/takeover`，配对门内、no-store、空 body；成功返回 `ConnectionView`，失败返回冻结码。
- 不新增第二连接器；复用 `CloseGuard`/`connect_lock`。

### R2 build 能力模型

- key：`platform | app_build | protocol_version | schema_fingerprint | probe_epoch`。
- 状态：`unsupported / interface_matched / confirmed / degraded / inconclusive`。
- 解析优先级：confirmed > interface_matched > inconclusive（仍候选）> unsupported；degraded 优先于候选，直到 key 变化。
- 已知 build 用 shipped 表；未知 build 走接口候选。
- build/协议/schema/probe_epoch 变化 → 旧 confirmed 不命中。

### R3 静默接口解析

- 采集：握手协议、hello/schema、send/resolve/stop/selection 方法存在性、pending 形状（permission/user_input、multiSelect、freeText）、control.activeWorks、selection 目录/档位。
- 只记录存在性与形状；不记录正文、题目、选项、命令、答案、JWT、链接。
- 无 pending 不显示审批/问答；无运行回合不显示打断；无目录不显示模型/档位。
- 不展示探测 UI；用户只看到能力可用/不可用及原因。

### R4 首次真实动作确认

- 候选能力首次真实使用时，捕获 baseline（generation/revision/target），提交动作。
- ACK 后等新投影/卡片消失/守卫消失/config 精确相等；5s 有界；不重发。
- 成功 → 该 build 该能力 `confirmed`；明确失败 → `degraded`；超时/未知 → `inconclusive`（保持候选，不升级、不重发）。
- 旧代际/旧 revision/别的 session 的确认结果丢弃。
- 判据：
  - send：revision 前进 + 新用户消息可见；
  - single/multi/free：目标 pending 消失 + revision 前进；
  - interrupt：目标 guard 从 activeWorks 消失 + 停止终态；
  - approval：目标 pending 消失 + revision 前进；
  - model：新 snapshot `selection.{provider,model}` 精确相等；
  - thought：新 snapshot `selection.thought` 精确相等。

### R5 UI 与执行闸

- `CapabilityView`、`check_policy`、`CommandSender` 使用同一个 resolver；UI 不自行推断。
- UI 只显示“可用/不可用 + 冻结码原因”；不显示兼容候选/已确认/探测中。
- 不可用时不显示动作按钮，或显示禁用+原因；不得伪造审批/模型切换。
- 旧 T8.2 证据许可可叠加，仅 debug/evidence 构建，不参与生产 shipped 表。

### R6 账本与失败

- DB 表 `zcode_capability_evidence(key, capability, state, reason_code, evidence, updated_at)`；migration 幂等。
- 写失败：内存继续，日志 warn，不假报 confirmed。
- 读失败：退化为“无账本 + 接口候选”；不阻塞连接。
- 不存正文/凭据/链接。

## 6. 设计

### 6.1 模块

新增 `src-tauri/src/services/zcode_control/capability/`：

- `key.rs`：`CapabilityKey`、`probe_epoch`、schema fingerprint。
- `state.rs`：`Capability`、`CapabilityState`、`CapabilityRecord`。
- `interface.rs`：`ZcodeInterfaceEvidence` + 纯函数 `interface_capabilities()`。
- `confirm.rs`：`ConfirmationContext` + 纯判据。
- `resolver.rs`：shipped + ledger + evidence → `CapabilityView`。

现有 `types.rs` 的 `CapabilityView` 保留八字段；`HostEvidence` 扩展接口证据或旁挂 `ZcodeInterfaceEvidence`。

### 6.2 数据流

```text
bridge hello/snapshot/pending/selection/control
  -> HostEvidence / ZcodeInterfaceEvidence
  -> CapabilityResolver (shipped + ledger + interface)
  -> CapabilityView
  -> Hub::session_capabilities / check_policy / CommandSender

user real action
  -> ConfirmationContext(baseline)
  -> action ACK
  -> event pump/session projection
  -> confirm predicate -> ledger confirmed/degraded/inconclusive
```

### 6.3 Shipped 表初始值

唯一已知 build `3.16.2.8769`，protocol 3：

- `send` confirmed
- `model` / `thought_level` confirmed
- `approval_reply` confirmed（仅回答既有 permission 卡）
- `single_select` interface_matched（ACK only）
- `interrupt` interface_matched（ACK only）
- `multi_select` / `free_text` interface_matched（2026-10-29 三轮范围裁决：「未实测」≠
  「明确不支持」——本仓解析/编码已支持，按接口形状开放为候选，首次真实动作确认
  兜底；历史证据记录保留在 §4.1，不改写为实机通过）

build 升级后不走该表。

### 6.4 未知 build 候选

- 协议必须为 v4/3 且 hello/schema 形状匹配；否则不开放写能力（只读/离线）。
- 候选条件见 R3；真实目标不存在不显示。
- 候选默认可用；首次动作确认后升级；明确失败降级。

### 6.5 手机抢回时序

```text
POST takeover
 -> takeover_lock
 -> if paired/connecting: return current
 -> if !configured: NEEDS_CONFIG
 -> if !authorized: NEEDS_AUTH
 -> begin_disconnect -> finish_disconnect
 -> connect_requested (auto=true, JWT reuse)
 -> return view; conflict => one attempt, no loop
```

### 6.6 失败/回滚

- Resolver 异常 → `production_unverified()` 全关（与现状一致）。
- 账本异常 → 接口候选+shipped；不阻塞连接。
- 首次确认失败 → 仅该 key+capability 降级；不全局关闭。
- 手机抢回失败 → 返回冻结码；不自动重试。

## 7. 验收标准

1. 冲突态手机出现「断开并抢回」；成功 paired；仍 conflict 只提示，不循环；不 OAuth。
2. 已知 build：shipped confirmed 能力可用；accepted 不显示成 confirmed；unsupported 不可用且有原因。
3. 未知 build：接口匹配 + 真实目标 → 候选可用；首次真实动作确认/降级；无额外测试消息。
4. 八项逐项：send/selection/pending 答案/approval/interrupt 的确认判据可证伪；ACK-only 不升级。
5. build/协议/schema/probe_epoch 变化后旧 confirmed 不命中。
6. UI 无探测页/按钮/徽标；no pending 无审批卡/问答卡；无目录无模型控件。
7. JWT/链接/正文/答案不进入账本、日志、手机 DTO。
8. 上传文件不在八项能力矩阵；ZCode 面板附件走通用上传通道（2026-10-29 范围修订，原「本轮不接」条款撤销），不实现官方原生附件协议。

## 8. 测试策略

- 纯核：key/state/interface/confirm/resolver 单测。
- DB：migration、upsert、读失败、build 失效。
- bridge：形状采集、invalidate、无正文。
- Hub：resolver 与执行闸同源；确认状态更新；手机抢回单飞。
- e2e：未知 build + fake bridge；真实动作触发确认；无探针文本。
- 前端：按钮/控件按 capabilities；无探测 UI；抢回按钮状态。
- 真机项单独登记，不伪造。

## 9. 风险

- 首次动作失败是 A 策略的预期成本；失败即降级，提示重试/换 build。
- 形状匹配不等于语义匹配；确认判据必须严格，ACK-only 不升级。
- 超时/事件丢失 → inconclusive；不宣称 confirmed。
- 官方页面自动反抢 → 手机只一次，提示关闭页面。
- 账本写失败 → 不假报 confirmed，日志可观测。

## 10. 已裁决

- **A 策略**：接口匹配 + 真实目标 → 候选开放；首次真实动作确认；失败降级；无额外测试消息、无探测 UI。
- 上传文件：原定排除、单独立项；**2026-10-29 用户确认修订**——本轮接入通用附件上传（正文引用方式），官方原生附件协议仍不在范围。
- 手机抢回：正式端点，配对门内，单飞一次。

## 11. 待审阅

- 本 spec 是唯一需求/设计文档；审阅通过后再执行 implementation plan。
- 当前未改代码、未提交；执行前切到 `codex/zcode-unified-remote`。

## 12. 实现偏差（已裁决，2026-10-28 执行期记录）

以下为实现期经控制者裁决的偏差，与上文条款冲突处**以本节为准**；其余章节结论不变。

- **schema_fingerprint 已实现真实形状哈希（2026-10-29 外部评审修复轮取代原 `"v4"` 占位）/ probe_epoch 恒 0**（R2/§6.1 的 fingerprint/epoch 字段）：指纹取 hello 应答与首个有效 conversation snapshot（含其 `config` 子对象）的**键名骨架**哈希（FNV-1a 64，定长 16 位十六进制 id 形状；值/正文不可见；骨架只取会话状态无关的稳定面，pending 卡形态不参与——避免同 build 指纹随会话状态漂移分裂账本）。同 build 同协议下帧形状变化即换 key，旧 confirmed/degraded 不命中。epoch 推进（探测复位语义）仍留待后续任务，恒 0。
- **send 确认的「新用户消息可见」用 `logEpoch:seq` 日志头指纹作代理判据**（R4 send 判据）：协议未提供「最后一条用户消息 id」的只读面；指纹是 id 形状（`"{epoch}:{seq}"`），消息追加即前进，语义与「新用户消息可见」等价且不存正文。指纹缺任一半（快照未携带 epoch/seq）时保守不确认。
- **A 策略确认窗 5s vs 命令回执 rpc 超时 10s 的时序**（R4「5s 有界」）：确认窗自 **ACK 受理后**起算并重置（`rearm`），而命令 rpc 回执本身超时上限 10s——回执最晚 10s 才到达时，确认窗观察可能已先行收口（confirmed / 到点 inconclusive）；ACK-only 回执绝不升级判决，真实确认只来自事件泵的宿主状态观察，回执只负责终态（rejected/unknown）降级收口。两侧时序各自有界、互不阻塞。
- **手机抢回的「确认」形态 = 常驻说明 + 单击即发**（R1「流程：确认 → disconnect → …」）：不做二次弹窗确认；「断开并抢回」按钮旁常驻一句说明（点击即执行一次抢回），busy 期间禁用，仍 conflict 只提示先关官方页面、失败显示冻结原因码——单飞一次、不循环的契约不变。
