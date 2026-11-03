# ZCode 官方桥接生产实施：执行总结（2026-10-26）

> 供独立复审。分支 `codex/zcode-unified-remote`，基线 `8e663f09`；2026-10-27 复审修复
> 基线 `90ae20be`，最终代码提交 `ee63d644`（本文档提交紧随其后）。工作树干净；未 push、
> 未合并、未发版、未打 tag；`docs/MASTER-PLAN.md` 零改动。实施方式：
> subagent-driven-development + TDD，每任务独立实现子代理＋独立审查代理。

## 1. 结论（先说不满足的部分）

- **更正：此前把 T1.1–T7.2 与 T8.2 写成“code_complete”是过早表述。** 准确口径是
  **代码层完成（code-layer complete）**：自动门禁全绿，但未经真实登录、真实手机、
  真实 Windows/宿主的平台验收。**platform_validated=false、spec_complete=false 不变。**
- **T8.1：代码层完成（假服务/回环），真实产品链未验收。** 2026-10-27 复审补齐了
  **ready/Initialize/hello/订阅/发送经真实 relay 的成功拼装**：新增 4 个全链用例，
  在回环假官方端上组装真实 `AuthService/RelayClient/codec/BridgeManager/CommandSender/
  ZcodeHub/router/RemoteState/SSE`，覆盖发送、回执、账本、审计、真实 SSE 与断链重放；
  另覆盖 rejected/malformed ACK 与默认全关零副作用。**仍未经真实 ZCode 账号/会话实跑。**
- **T8.2：代码层完成，platform_validated=false，spec_complete=false。** 策略、证据许可、
  文档与验收表已交付；真实登录/专用会话/Windows 行全部 `blocked/not_run`，未伪造 PASS。
  普通构建八项控制能力全部关闭。
- **因此：不满足完整 spec。** 可交付的是“统一 UI＋官方桥接生产链＋按证据开闸的封闭
  底座＋生产组件假链回归”；ZCode 手机控制尚未在真实产品链上开闸。

## 2. 任务逐项状态

| 任务                 | 状态                        | 提交范围（短）                                                                                     | 主要实现/证据                                                                                                                                                                                              |
| -------------------- | --------------------------- | -------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| T1.1 授权/内存凭据   | 完成                        | `f9a6808c` `323f246e` `61b0265f`                                                                   | `auth.rs`；state 一次性、120s、epoch 绑定、终端 token 线性化；auth 16 测试                                                                                                                                 |
| T1.2 单 relay        | 完成                        | `e33a53dd` `537f55b8`                                                                              | `relay.rs`；单 socket、waiting≠paired、KICKED 终态、5s±1s/15s 心跳、路由不互踢；relay 35 测试                                                                                                              |
| T2.1 二进制 RPC      | 完成                        | `98d5feed` `7c8a3964`                                                                              | `codec.rs`＋golden 向量；1MiB 物理/16MiB 逻辑、64 分片、CRC、去重/TTL；codec 23 测试                                                                                                                       |
| T2.2 桥/投影/pending | 完成                        | `176fe237` `6860be2e` `09744e8a`                                                                   | 真实桥序列、controller tasks-index、订阅 activate、会话级 fragment 重组、按工作区失效；bridge 33/projection 11/pending 10                                                                                  |
| T3 发送/账本/审计    | 完成                        | `a8c8bdfc` `bad3fa8b` `e6e87fdb`                                                                   | `action/command/audit.rs`；20s ACK、reserved CAS、幂等、无重发、计数审计；command 21/action 14                                                                                                             |
| T4.1 问答            | 完成                        | `7a880587` `efed9484` `ff04b781` `65594176`                                                        | 原值映射、snooze、宿主撤卡终态、sensitive 不落草稿；command 40                                                                                                                                             |
| T4.2 打断            | 完成                        | `464ed447` `f641c118` `fe43d62d` `4153e97b`                                                        | 仅停用户选定的 primaryTurn；10s 控制窗、kind 白名单、未知基线不确认；stop 17                                                                                                                               |
| T5 审批既有卡        | 完成（门关闭）              | `3daff419` `582d43aa`                                                                              | allowOnce/deny 白名单、原 optionId、fullAccess 硬拒、默认关闭、snooze 复用；permission 16                                                                                                                  |
| T6 模型/档位         | 完成（门关闭）              | `e93a370e` `10e0f3b8` `4b73fb1c` `37a46ba1`                                                        | 当前 provider、逐模型档位、CAS、无 baseLogEpoch、无恢复；selection 14/command 7                                                                                                                            |
| T7.1 Hub/HTTP/M11    | 完成                        | `059bbc9e`…`3c2880bc`（11 提交）                                                                   | `hub.rs`、`remote/zcode.rs`、409 旧发送、SSE、Tauri 命令、M11 退役、按设备过滤、隐藏会话 404；remote 25/hub 17                                                                                             |
| T7.2 统一 UI         | 完成                        | `7e04ef2b`…`503b94ef`（14 提交）                                                                   | DTO/hook/panel、真实回执文案、能力门、19 码 zh/en、设置授权/连接、多选与草稿修复；手机 2343 通过                                                                                                           |
| T8.1 假服务集成      | **代码层完成；真实链未验收** | 前序 + `0ec5d5bc` `3d85d4bb` | 进程内 FakeOfficial＋回环 HTTP/WS 真实组件链；4 个全链用例覆盖 SSE paired、真实 bind/订阅/快照、发送恰一帧、HTTP accepted、账本/审计无正文、真实 session/action SSE、断链重放零新增；rejected/malformed 终态不重发；默认全关 bind 前 409 零副作用。真机/真实账号仍未跑 |
| T8.2 验收/开闸       | 代码层完成；live blocked    | `2829d35e` `e450bf0e` `981ee02b` `926a250f` `6c7239e4` `f6833952` `049af5e3` `9ef86037`            | 能力策略、debug 许可、release 编译拒绝、验收表、双语手册/CHANGELOG、contract `production` 段；`platform_validated=false`、`spec_complete=false`                                                              |

## 3. Spec 逐项覆盖

- **产品已可用**：ZCode 看板/正文/文件预览；官方链接保存/替换/删除与严格校验；设置六段顺序、PIN 遮罩、H3/兼容缺键默认 true；ZCode 设置卡的 BigModel 授权/连接/断开/真实状态；手机 ZCode 发送区/问答卡/审批卡/打断/模型控件的统一 UI（能力关时只读并就地说明）。
- **实现但默认关闭（按证据开闸）**：send、单选/多选/自由文本问答、审批既有卡、打断、模型、思考强度。普通构建 `SessionView.capabilities` 全关；仅 debug+`zcode-bridge-evidence` 构建可逐次发放 120s 单次许可（不绕过目标/权限/fullAccess 守卫）。
- **尚未实现/未取证（真实产品链）**：T8.1 的回环假链已覆盖 ready/Initialize/hello/订阅/发送经真实 relay 的成功拼装，但**真实 ZCode 账号/会话/手机**的登录→发送→终态仍未跑；Windows 构建/实跑；ZAI 实跑；多选/自由文本/打断宿主终态；审批催卡（Tuvis 发送→弹卡）仍为负结果；双手机并发、断线/链接失效、官方页互踢的真机流；`zcode_actions` 保留策略。

## 4. 最终接口与计划差异

| 差异                                                                                               | 原因/源码依据                                                                                      | 影响                                          |
| -------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------- | --------------------------------------------- |
| `HubBackend` 测试端口；`RelaySink`/`CommandPort`/`PendingPort`/`StopPort`/`SelectionPort` 窄 trait | 避免每个测试起真实 WS/桥；生产均以真实组件实现                                                     | 接口可替换；T8.1 已用回环全链覆盖真实组件，真实产品链仍待验收 |
| `BoundTarget.bridge_generation`（内部）＋公开 Hub `generation`                                     | 桥投影代际与配置代际从首连即分叉；真实动作曾全判 stale                                             | 已修；公开回执/手机只用 Hub 代际              |
| `CapabilityPolicy` 未知宿主基线＋证据许可并不能直接开闸                                            | 无真实产品证据不得填 true                                                                          | 普通构建全关；debug 许可逐次                  |
| 设置卡改用 `src/lib/api/zcodeControl.ts` 单一 DTO                                                  | 消除本地重复 `ConnectionState/View`                                                                | 已对齐                                        |
| T8.1 进程内 FakeOfficial 为主，另补 loopback auth/WS/data                                          | loopback 分片实现后已跑通真实 AuthService/RelayClient；桥与发送经真实 relay 的拼装仍超出已交付切片 | **合同部分满足**；桥/发送拼装仍列为 Important |
| ZAI 授权按钮禁用                                                                                   | 仅源码/模拟证据，未实跑                                                                            | 明确说明，不冒充支持                          |

## 5. 验证命令与结果（最终树）

- `pnpm test` → 194 文件，**2343 passed，5 skipped**，exit 0。
- `pnpm check` → exit 0；`pnpm build:mobile` → exit 0。
- `cargo test --features marker-helper` → **3770 passed，0 failed，46 ignored**。
- `cargo test --features marker-helper,zcode-bridge-evidence` → **3779 passed，0 failed，46 ignored**。
- `wire_e2e` loopback 4 用例：HTTP OAuth/terminal token、WS auth_init/auth_ack 配对、RelayClient 数据回环、真实 BridgeManager bootstrap→listing→bridge-open。
- `cargo fmt --check` → 0；`cargo clippy --all-targets --features marker-helper -- -D warnings` → 0；同命令加 `zcode-bridge-evidence` → 0。
- `cargo check --release --lib --features zcode-bridge-evidence` → **按设计失败**（`zcode-bridge-evidence is only allowed in debug evidence builds`，`mod.rs`）。该拒绝在 `e450bf0e` 实测；最终树未再单独复跑该项（release 依赖未变）。
- 抖动/失败：开发早期曾出现一次手机 bookmark 并行负载抖动（隔离通过）；T8.1/T7.2 多个切片因模型单次输出上限中断，均以更小切片重做或记录为缺口；watch 无静默丢弃。

## 6. 真实能力证据（脱敏）

平台 macOS 27.0.1 arm64；APP build `3.16.2.8769`；固定源码 `3c5b2276`。

| 能力                        | run（脱敏）                                            | 预期                                        | 实际                                      | ACK/终态                     |
| --------------------------- | ------------------------------------------------------ | ------------------------------------------- | ----------------------------------------- | ---------------------------- |
| 授权/只读                   | `macos-B120261024T105940Z-NarmSw`                      | BigModel 回环授权、terminal token、只读查询 | 通过                                      | paired/只读成功              |
| 单条发送                    | `macos-B220261024T132316Z-prItvw`                      | 精确 session 一帧 sendText，APP 可见        | 通过；用户亲见 marker 并被处理            | accepted＋用户可见；非“终态” |
| 问答（单选）                | `macos-B320261025T012945Z-aMtnqA`                      | resolve 受理、宿主撤卡                      | 仅 ACK；未观察撤卡                        | ACK-only                     |
| 打断（守卫执行）            | `macos-B320261025T013128Z-X8HkPg`                      | stop 受理、执行停止                         | 仅 ACK；未观察停止                        | ACK-only                     |
| 审批既有卡（批准/拒绝）     | 决策 §8 记录的 `…T032425Z-K7I6ZA` / `…T034803Z-noorSA` | 原 optionId 应答、产物核对                  | 通过（既有卡）                            | 撤卡/产物已证                |
| 模型切换                    | `macos-B420261025T030637Z-bS5dOg`                      | 同 provider 切换、新快照确认                | 通过                                      | confirmed by fresh snapshot  |
| 思考强度                    | `macos-B420261025T025928Z-3WyTFA`                      | 逐模型档位切换                              | 通过                                      | confirmed by fresh snapshot  |
| 审批催卡（Tuvis 发送→弹卡） | 同上 B3 轮                                             | 发送触发 permission                         | **负结果：terminal send 直接执行不弹卡**  | 保持 approval 关闭           |
| T8.2 产品实跑               | 无                                                     | —                                           | `blocked`（需用户登录/指定专用会话/平台） | 未伪造                       |

## 7. 发送、M11、H3

- **发送是否真正经过 Tuvis 手机 UI**：代码路径与自动测试是（`MessageComposer/SessionDetail → ZcodeControlPanel → POST /m/api/v1/zcode/actions → Hub → CommandSender → 官方桥`）；**真实手机+真实 APP 的产品发送尚未执行**，因此不能宣称端到端产品验收。
- **M11**：已消除。`inject/routing.rs` 对 ZCode 保留不可注入门但 reason 改为 `official_control_required`，零 CLI/终端回退；旧 `/session-send` 对 ZCode 返回 409 `official_control_required`；测试断言 reason 不含 M11/headless_only。
- **H3 关闭时官方发送**：ZCode 官方链路不读取 H3/compat 开关，手机测试断言零 `/session-send`、零 headless 调用；未单独做“H3 显式 false”开关测试（行为独立，缺口已记录）。

## 8. 凭据、日志、审计、并发/断线

- JWT/terminal token 仅 Rust 内存；无 settings/DB/keychain/手机/事件/错误返回；OAuth state/回调一次性；`TerminalToken` 不 Debug/Serialize。
- 审计 channel `zcode_official`，仅 kind+计数；动作账本只存 payload hash，不存正文/答案/JWT/URL。
- 幂等：同 actionId 同 payload 只一帧；不同 payload 冲突；未知结果不重发、不落 CLI；崩溃恢复返回 unknown 不派发。
- 单 owner：同 SID/role 单 socket；KICKED→conflict 零自动重连；断线→offline；旧代际动作零帧；配置变更先断 WS/清 JWT/投影再返新态。
- 两台手机共享一个 Hub；SSE 按会话/设备过滤，lag→resync 不重放；设备吊销关闭流。
- 未实跑：双手机并发、官方页面互踢、链接过期、Windows 同口径。

## 9. Review 发现与修复

- **B 包三轮复审（历史）**：ACK≠终态、审批催卡负结果、问答形态、token 一次性、逐模型档位、恢复语义、脱敏、stop 绑定新执行、初始 control 门——均已在探针/合同修复。
- **本批逐任务复审**：T1.1 epoch 竞态 2 轮；T1.2 所有权/liveness 1 轮；T2.1 逻辑/物理限额 1 轮；T2.2 fragment/失效/工作区 3 轮；T3 20s ACK/ exactly-once/审计 1 轮；T4.1 snooze noop/inflight/目标作用域 1 轮；T4.2 控制等待/kind 2 轮；T5 snooze/尾部/回放 1 轮；T6 回放/hash/fallback 1 轮；T7.1 generation/事件/隐藏会话 2 轮；T7.2 门控/DTO/官方页/i18n/actionId/SSE/多选/草稿多轮。
- **最终复审**：Rust 0 Critical；已修 I-A（策略/许可经 `SessionView.capabilities` 下发）与 I-C（宿主证据源缺口入 contract）；**I-B 已由 2026-10-27 复审闭环**：ready/Initialize/hello/订阅/发送经真实 relay 的拼装已在回环假官方链上覆盖（`3d85d4bb`）。UI 0 合并阻断；多选/草稿已修。
- **2026-10-27 独立复审修复（12 项，基线 `90ae20be`）**：物理分片去重 `b94be0e0`；Hub
  Data→Session 事件 `0d36d516`；幂等前置 `0b5e5dda`；断链意图/清理 `89c61231`；
  relay 队列 State 必达/Data lag 失效 `ba43b6f7`；宿主证据策略源 `de25937e`；同代
  paired 刷新 `5c531b17`；问答选项/文本互斥 `10080c13`；设置卡轮询 `1a0453c1`；宿主
  题型能力闸 `955b380d`；snooze 限定绑定 `d8d92fa5`；生产假链 `3d85d4bb`。每项独立
  审查，门禁见第 11 节。
- **剩余阻塞/待裁决**：
  1. 真实产品链验收（真实 ZCode 登录/专用会话/手机收发/Windows）；回环假链已闭环。
  2. T8.2 真机/平台验收（用户登录、专用会话、Windows）。
  3. 审批催卡负结果未消除（approval 保持关闭）。
  4. 多选/自由文本/打断终态、ZAI、双手机、链接失效、官方页互踢未实跑。
  5. 次要：SSE 在会话隐藏后仍开、`zcode_actions` 无保留策略、`with_parts` release 可见、部分 UI 残项、事件泵能力回落未接实时策略源。

## 10. 合规与最小协助请求

- 未 push、未合并、未发版、未打 tag；`docs/MASTER-PLAN.md` 未改；真实链接/SID/JWT/正文未入库；真实账号凭据仅在用户浏览器与本机内存。
- **满足完整 spec？否。** 当前是**代码层完成、未经平台验证**的封闭底座；
  **platform_validated=false、spec_complete=false 均未达成**；此前 `code_complete`
  表述已按 2026-10-27 复审口径更正。
- 需用户本人完成的最小动作（之后 Agent 可继续 T8.2）：
  1. 在 debug 验收构建（`pnpm tauri dev --features hook-listener,zcode-bridge-evidence`）完成 BigModel 官方登录/配对并连接；
  2. 指定一个专用测试工作区/会话（当前约定 Travel/「hi」）与允许的真实动作范围；
  3. 提供 Mac 真机与 Windows 运行环境；ZAI 若要标支持需单独实跑；
  4. 审批催卡需用户亲发 guarded 消息触发；若仍不弹卡，保持 approval 关闭并记录负结果。

## 11. 2026-10-27 复审修复门禁（实测证据）

- **范围**：`90ae20be..ee63d644`（12 项修复 + 文档/测试夹具提交）；逐项 RED→GREEN→
  独立审查；每项提交见修复计划 `docs/superpowers/plans/2026-10-27-zcode-review-fix-plan.md`。
- **Rust 单测**：普通 `cargo test --lib --features marker-helper` **3300 passed / 0 failed /
  38 ignored**；证据构建 `--features marker-helper,zcode-bridge-evidence`
  **3319 passed / 0 failed / 38 ignored**。
- **Rust 集成**：`cargo test --features marker-helper` 与
  `cargo test --features marker-helper,zcode-bridge-evidence` 全部套件 0 failed
  （含 `zcode_control_e2e` 8/8、`zcode_control_remote` 25/25、全链 4 例；后两者为
  2026-10-27 新增/修正夹具）。
- **静态门禁**：`cargo fmt --check` 0；普通与证据构建
  `cargo clippy --all-targets ... -- -D warnings` 0；`cargo check --all-targets` 0。
- **前端**：`pnpm test` **194 files / 2349 passed / 5 skipped / 0 failed**；
  `pnpm check`（format/lint/i18n/error-codes/build）0；`pnpm build:mobile` 0。
- **未做（按要求留待用户在场）**：真实 ZCode 登录/配对/连接、真实手机收发、Windows
  实跑、真实问答/审批/打断终态、真机 SSE/断链。**platform_validated=false、
  spec_complete=false**；`code_complete` 已改为“代码层完成”。
- **已知非阻塞风险**：事件泵在 `last_session` 缺失/异会话时回落 `CapabilityView::default()`
  （未接实时策略源）；`host_evidence()` 每次读 Info.plist（小文件同步读）；旧公开
  `snooze_interaction` 全量绑定入口无生产调用；`mark_authorized_for_test` 与生产
  authorize 落定逻辑重复；accepted 回执的 proof 仍非终态（ACK≠终态）。
