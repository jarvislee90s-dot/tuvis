# ZCode 官方桥接生产实施计划（基于已取证合同）

> 日期：2026-10-25。前置：B1–B3 实跑通过，B4 仅完成源码取证（实现/实跑延后）。本文件是 **B5 交付物**：把已证明的官方协议合同落成可施工的生产任务单；**本计划不自动执行**，需用户确认后再实施。设计权威：`docs/superpowers/specs/2026-10-24-zcode-unified-remote-design.md`；证据权威：`docs/superpowers/decisions/2026-10-24-zcode-official-bridge-contract.md` 与 `tests/fixtures/zcode-official-bridge/contract.json`。

## 1. 目标与边界

- 手机经 Tuvis 配对门使用统一 UI，底层由**本机适配器**走官方协议：授权、会话映射、发送、问答、打断、（延后）审批与模型/思考强度。
- **能力逐项开放**：只有实跑取证过的能力才在 UI 可用；未取证能力保持不可用并就地说明原因（不默认 true、不以"保存链接"冒充联机）。
- 只读（看板/正文/文件）与写向完全解耦：任何控制失效都不影响读取。
- H3 无头 CLI 与官方协议适配互不依赖；官方链接为本机单条、配对手机共享；凭据（账号 JWT）**只留本机**，手机端只持有 Tuvis 配对 cookie。
- 不做：隐藏浏览器 DOM 自动点击、读取他应用凭据、full-access 捷径、跨会话批量操作、向未指定会话写。

## 2. 冻结接口（消费同一份已验证 schema，前端不复制协议字段）

### 2.1 后端模块（`src-tauri/src/services/zcode_control/`）

| 模块 | 职责 |
| --- | --- |
| `auth.rs`（扩展现有 config.rs） | 官方 OAuth 回环授权（bigmodel/zai 双通道）、账号 JWT 内存持有与过期清理、terminal 一次性 token（60s）申请 |
| `relay.rs` | 单条上游 WS（`ws2?token&mid`）、auth_init/auth_ack、`pair_status` 归一（无 APP 确认步）、KICKED 终态、**连接代际** |
| `projection.rs` | bootstrap / workspace-list / tasks-index / 会话 topic 流（EventListen+subscribe+activate）→ 归一化投影：workspaces、tasks、pendingInteractions、control |
| `command.rs` | `sendConversationCommandV4` / `resolveInteraction` / `stop` / `switchModelConfig` 的**唯一构造点**（含 CAS `baseRevision`、禁 `baseLogEpoch`） |
| `pending.rs` | PendingBook 移植：四元组绑定（session/turn/interaction/代际）、三查闸、消费幂等、autoResolution snooze |
| `action.rs` | `ActionState = Accepted | Noop | Rejected | SendFailed | Unknown`（附 generation + reasonCode）；**unknown 不重发** |
| `audit.rs` | 复用既有 write_audit：动作、设备、时间、目标会话、内容摘要（不落正文/账号） |
| `mod.rs` | 装配与生命周期（单连接闸、代际推进、错误分类：needs_config/needs_auth/offline/conflict/unknown） |

### 2.2 手机 API（沿用现有 `/m/api/v1` 配对门 + no-store）

- `GET /m/api/v1/zcode/connection` → `{state: needs_auth|connecting|paired|conflict|offline|needs_config, generation, deviceLabel}`（**不含** JWT/token）
- `GET /m/api/v1/zcode/workspaces|sessions` → 精确 id 投影（供发送/审批卡绑定）
- `GET /m/api/v1/zcode/pending` → 本会话 pending 投影（interactionId、kind、题干/选项原值、turn 锚、generation、autoResolution）
- `POST /m/api/v1/zcode/send|resolve|stop|model` → `ActionState` 回执（accepted/noop/rejected/send_failed/unknown + reasonCode）
- `GET /m/api/v1/zcode/events`（SSE）→ pending/control/action 事件；断线重连后以 `GET pending` 纠偏，不重放写动作

### 2.3 安全与凭据

- 账号 JWT：仅本机内存（进程退出即弃）；本轮不落 OS keychain（后续单列任务评估 keychain 可用性），**不写 settings、不进日志、不进手机响应**。
- 所有 wire 值（sid/mid/interactionId/模型 id）在日志中脱敏；证据目录 0700/0600 + fail-closed 断言（沿用探针 `sanitize.ts` 纪律）。
- 同 SID/role 单 owner：检测 KICKED 即终态并展示"连接被另一终端接管"，**禁止自动循环重连互踢**。

## 3. 能力证据状态（施工前置的事实边界）

| 能力 | 状态 | 证据 |
| --- | --- | --- |
| 官方授权（bigmodel 回环 OAuth） | ✅ 实跑 proven | 决策 §6.9；B1 实连 run `macos-B120261024T105940Z-NarmSw` |
| terminal 一次性 token（60s、一次性） | ✅ 实跑 | 同上（两次申请值不同） |
| 单连接 + mid 路由 + 自动配对（无 APP 确认） | ✅ 实跑 | 同上（matched 131ms；用户观察无任何 APP 弹窗） |
| 只读镜像（bootstrap/workspace-list/tasks-index） | ✅ 实跑 | B2 run `macos-B220261024T132316Z-prItvw`（119 行任务） |
| 会话 topic 事件流（snapshot/deltas/pending/control） | ✅ 实跑 | B3 answer run `macos-B320261025T012945Z-aMtnqA` |
| 发送（sendConversationCommandV4，单帧、unknown 不重发） | ✅ 实跑 + 用户可见 | B2 同上（marker 可见且被 agent 处理） |
| 问答（resolveInteraction，原题/选项回传） | ✅ 实跑 | B3 answer 同上（RESOLVED） |
| 打断（带守卫 stop） | ✅ 实跑 | B3 interrupt run `macos-B320261025T013128Z-X8HkPg`（STOPPED:interrupt-accepted） |
| 审批（权限卡应答） | ⚠️ **未取证** | 指定测试会话为自动批准模式：无卡、文件被直接创建（B3 approve run `macos-B320261024T230311Z-aTrTIg`）；卡路径的 wire 形状已从源码取证，但**未实跑** |
| 思考强度（switchModelConfig + CAS） | ✅ 实跑（切档+恢复） | `.superpowers/sdd/2026-10-24-zcode-official-bridge-validation/b4-source-notes.md`（READ `snapshot.config.*`；CHANGE `switchModelConfig{provider,model,thought}` + `baseRevision`；值源 `model-selection.getView()`/`thoughtLevels`）；B4 实跑延后 |
| 模型切换（换 model id） | ⚠️ 未取证（目录需 getView；本项目前会话目录仅当前模型） | 待 `model-selection.getView()` 角色可见性验证后补测 |
| 两台配对手机并发 / 断线重连 / 链接失效流转 / 官方网页互踢实测 | ⚠️ 未覆盖 | 计划 T8 专门验收 |

## 4. 生产任务拆解（每任务：红测 → 实现 → 验证命令 → 通过条件 → 提交点）

### T1 授权与会话生命周期
- 红测：`zcode_auth_loopback_only`（回环 redirect 接受/拒绝两分支）；`token_ttl_and_one_time`；`kicked_is_terminal_no_reconnect`；`single_upstream_connection`。
- 实现：`auth.rs`（overlay 现有 config.rs，URL 校验/保存不变）+ `relay.rs`（代际、KICKED、错误分类）。
- 验证：`cargo test --lib services::zcode_control::auth`、`cargo test --lib services::zcode_control::relay`、`cargo test --test zcode_relay_remote`（fake relay）。
- 通过：未授权不得申请 token/发帧；KICKED 后无第二连接；JWT 不出现在任何日志/DB/HTTP 响应（grep 审计）。
- 提交点：`feat(zcode): add official relay lifecycle with proven auth`。

### T2 只读投影与事件归一
- 红测：`projection_matches_verified_shapes`（bootstrap/workspace/task/topic 四类真实帧脱敏夹具）；`generation_binds_pending_and_control`；`foreign_topic_frames_rejected`。
- 实现：`projection.rs` + `pending.rs`（PendingBook 移植）。
- 验证：`cargo test --lib services::zcode_control::projection`、`cargo test --test zcode_projection_remote`。
- 通过：同 session 只收本会话帧；控制态 `activeWorks[].foregroundExecutionId` 可观察；无写向。
- 提交点：`feat(zcode): project official session topics for control`。

### T3 发送闭环与审计
- 红测：`send_single_frame_and_unknown_no_resend`；`send_requires_bound_target`；`send_audited_without_body`。
- 实现：`command.rs` send + `action.rs` + `audit.rs`（复用 write_audit，内容只记摘要）。
- 验证：`cargo test --test zcode_send_remote`（fake）+ 既有 codex/remote 回归。
- 通过：错目标/未绑定拒绝；一次动作一帧；unknown 不重发；审计无正文。
- 提交点：`feat(zcode): send through official command channel with audit`。

### T4 问答与打断
- 红测：`resolve_binds_turn_and_epoch`；`wrong_or_consumed_rejected`；`stop_requires_observed_guard`；`auto_resolution_snoozes_first`。
- 实现：`command.rs` resolve/stop + `pending.rs` 三查闸 + snooze 前置。
- 验证：`cargo test --test zcode_interaction_remote`。
- 通过：错 ID/过期/重复不消费他卡；stop 无守卫即拒；迟到结果不入新代际。
- 提交点：`feat(zcode): resolve pending interactions with guarded stop`。

### T5 审批（受阻：卡在场才可应答）
- 现状：指定测试会话自动批准（无权限卡）→ **当前无法取证**。禁止为取证而改会话权限模式或使用 fullAccess。
- 红测（可先写）：`approval_requires_pending_card`；`approval_refuses_standing_grants`（allowAlways/allow_project/allowSession 一律拒绝）；`full_access_blocked`。
- 实现：`command.rs` permission 分支 + 选项白名单（仅 `allowOnce`/`deny` 类，原值回传）。
- 验证：fake：`cargo test --test zcode_approval_remote`。
- 通过条件（**既 gate**）：需在一个会产生权限卡的会话中实跑"批准/拒绝各一轮 + 产物核对"；在此之前 UI 审批卡能力保持不可用并解释原因。
- 提交点：`feat(zcode): answer permission pendings when a card exists`（能力默认关闭，取证后开）。

### T6 模型/思考强度（延后：先完成 B4 实跑）
- 前置：完成 B4 的 `selection-probe` 实跑（读取 → 切换 → 新鲜快照确认 → 恢复原值），合同见 b4-source-notes。
- 红测：`only_server_supported_ids`；`switch_requires_base_revision_and_forbids_base_log_epoch`；`restore_original_on_finish`；`error_taxonomy_distinct`。
- 实现：`command.rs` switchModelConfig + 目录解析（`model-selection.getView()` 或 `snapshot.config.thoughtLevels`）。
- 验证：`cargo test --test zcode_selection_remote`。
- 通过：显示名不得作 wire 值；未知 id 拒绝；结束恢复原值、不改全局默认。
- 提交点：`feat(zcode): switch model and thought level with restore`。

### T7 手机控制区（统一 UI）
- 红测：`control_availability_matrix`（needs_config/needs_auth/offline/conflict/unknown 各就位说明）；`read_view_unaffected_by_control_state`；`link_saved_never_connected`。
- 实现：SessionDetail/MessageComposer 复用现有发送区与卡面；`src/lib/api/zcodeControl.ts` 单一 wire 类型来源（不复制协议字段）。
- 验证：`pnpm exec vitest run tests/mobile` + `pnpm build:mobile`。
- 通过：未取证能力按钮不可用且有原因；发送失败保留草稿；只读始终可用。
- 提交点：`feat(mobile): expose zcode control actions per proven capability`。

### T8 全链验收（含双端与冲突）
- 用例：两台配对手机并发（单上游、无互踢循环）；断线→需重新授权（UX 明示）；链接失效/更新（needs_config）；官方网页同时打开（KICKED 展示为接管冲突）；Windows 与 macOS 同口径；审计行覆盖 send/resolve/stop/switch；只读回归。
- 验证：`pnpm check`、`cargo test --features marker-helper`、`cargo clippy --all-targets --features marker-helper -- -D warnings`、实机清单（下表）。
- 通过：全部用例有证据（脱敏 run id/截图不入库）；无未取证能力被标 true。

## 5. 末期人机分工

- **用户**：完成官方登录/必要配对（本人浏览器）；指定专用测试工作区/会话；手机端体验确认（发送/问答/打断/模型选择各一次）。
- **Agent**：其余批准动作自动执行；跑全部红测/回归；对每项能力产出脱敏证据并更新合同与能力矩阵。
- 不重复证明官方网页已有能力；只证明 Tuvis 独立客户端的归属、回执与 UI 闭环。

## 6. 文档与发版落点

- 用户手册 zh/en：新增"ZCode 统一控制（按能力开放）"章节 + 故障排查（未授权/冲突/链接失效/能力不可用原因）；教程只写 APP 内获取链接的真实步骤。
- `docs/capabilities.json` + README 能力矩阵：**随发版**同步（当前不得提前宣称发送/审批上线）。
- CHANGELOG zh/en：每项能力开闸单独记一条；行为变化写升级须知。
- 措辞纪律：读取与配置可用；统一控制"按已验证能力逐项开放"；官网页仅辅助入口。

## 7. 风险与回滚

| 风险 | 处置 |
| --- | --- |
| 官方认证/协议版本漂移 | 固定源码指纹 + 版本探针；failed 即降级 needs_auth/needs_config，只读不受影响 |
| 同 SID 单 owner 互踢 | KICKED 终态 + UI 明示"被另一终端接管"；禁止自动重连循环 |
| 凭据泄漏 | JWT 仅本机内存；日志脱敏 fail-closed；手机端永不接触 JWT/token |
| 误应答/误打断 | 四元组+代际绑定；守卫 stop；错目标/过期/重复一律拒 |
| 审批取证受阻 | 能力保持关闭；不改会话权限、不用 fullAccess；另找会弹卡的环境或等官方形态变化 |
| 模型能力未取证 | T6 延后，UI 不开放；先完成 B4 实跑 |

## 8. 完成定义（DoD）

1. T1–T4、T7 完成且全绿；T5/T6 按 gate 状态为"实现就绪、能力关闭/待取证"。
2. 两项平台（macOS/Windows）与两台配对手机的验收清单有实机证据。
3. 合同 `tests/fixtures/zcode-official-bridge/contract.json` 与能力矩阵只记录已取证为 true 的能力。
4. 手册/CHANGELOG 双语同步；未发版不打 tag。
