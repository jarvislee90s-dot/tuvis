# ZCode 官方桥接生产实施：执行总结（2026-10-26）

> 供独立复审。分支 `codex/zcode-unified-remote`，基线 `8e663f09`，计划/核查文档提交 `06b7f759`，最终 HEAD `c53d8010`。工作树干净；未 push、未合并、未发版、未打 tag；`docs/MASTER-PLAN.md` 零改动。实施方式：subagent-driven-development + TDD，每任务独立实现子代理＋规格/质量审查代理。

## 1. 结论（先说不满足的部分）

- **T1.1–T7.2 与 T8.2 的能力门/证据设施：code_complete，自动门禁全绿。**
- **T8.1：partial（缺口已收窄）。** 约 8 个 Rust e2e 用例与 4 个手机流用例已交付；另新增 **loopback HTTP OAuth/terminal token、WS auth_init/auth_ack、真实 RelayClient 数据回环、真实 BridgeManager bootstrap/workspace-list/workspace-bridge-open** 4 个 wire 用例。仍缺的是 **ready/Initialize/hello/订阅/发送经真实 relay 的成功拼装**：四个 brief 指定用例仍走进程内假体。最终复审将该项列为 Important，**未冒充通过**。
- **T8.2：code_complete，platform_validated=false，spec_complete=false。** 策略、证据许可、文档与验收表已交付；真实登录/专用会话/Windows 行全部 `blocked/not_run`，未伪造 PASS。普通构建八项控制能力全部关闭。
- **因此：不满足完整 spec。** 可交付的是“统一 UI＋官方桥接生产链＋按证据开闸的封闭底座”；ZCode 手机控制尚未在真实产品链上开闸。

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
| T8.1 假服务集成      | **partial（缺口收窄）**     | `feb821ff` `88662f9c` `79721b96` `e7b56a0c` `2ba2fb9b` `3b19c190` `e8f7260f` `baf91164` `26ca7fda` | 进程内 FakeOfficial＋8 Rust e2e＋4 手机流＋A 回归；loopback HTTP 授权/token、WS 握手、真实 RelayClient 数据回环、真实 BridgeManager bootstrap/bridge-open 4 用例；ready/Initialize/hello/订阅/发送仍未覆盖 |
| T8.2 验收/开闸       | code_complete；live blocked | `2829d35e` `e450bf0e` `981ee02b` `926a250f` `6c7239e4` `f6833952` `049af5e3` `9ef86037`            | 能力策略、debug 许可、release 编译拒绝、验收表、双语手册/CHANGELOG、contract `production` 段                                                                                                               |

## 3. Spec 逐项覆盖

- **产品已可用**：ZCode 看板/正文/文件预览；官方链接保存/替换/删除与严格校验；设置六段顺序、PIN 遮罩、H3/兼容缺键默认 true；ZCode 设置卡的 BigModel 授权/连接/断开/真实状态；手机 ZCode 发送区/问答卡/审批卡/打断/模型控件的统一 UI（能力关时只读并就地说明）。
- **实现但默认关闭（按证据开闸）**：send、单选/多选/自由文本问答、审批既有卡、打断、模型、思考强度。普通构建 `SessionView.capabilities` 全关；仅 debug+`zcode-bridge-evidence` 构建可逐次发放 120s 单次许可（不绕过目标/权限/fullAccess 守卫）。
- **尚未实现/未取证**：T8.1 的 ready/Initialize/hello/订阅/发送经真实 relay 的成功拼装（loopback 已覆盖 auth/WS/data/桥 bootstrap 与 bridge-open 请求，四个 brief 用例仍走进程内假体）；产品 UI 真实登录→发送→终态；Windows 构建/实跑；ZAI 实跑；多选/自由文本/打断宿主终态；审批催卡（Tuvis 发送→弹卡）仍为负结果；双手机并发、断线/链接失效、官方页互踢的真机流；`zcode_actions` 保留策略。

## 4. 最终接口与计划差异

| 差异                                                                                               | 原因/源码依据                                                                                      | 影响                                          |
| -------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------- | --------------------------------------------- |
| `HubBackend` 测试端口；`RelaySink`/`CommandPort`/`PendingPort`/`StopPort`/`SelectionPort` 窄 trait | 避免每个测试起真实 WS/桥；生产均以真实组件实现                                                     | 接口可替换，但 T8.1 因此停在端口边界          |
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
- **最终复审**：Rust 0 Critical；已修 I-A（策略/许可经 `SessionView.capabilities` 下发）与 I-C（宿主证据源缺口入 contract）；**I-B 已收窄**：loopback HTTP 授权/token、WS 握手、真实 RelayClient 数据回环、真实 BridgeManager bootstrap/bridge-open 已交付；ready/Initialize/hello/订阅/发送经真实 relay 的拼装仍未覆盖。UI 0 合并阻断；多选/草稿已修。
- **剩余阻塞/待裁决**：
  1. T8.1 桥/发送经真实 relay 的成功拼装（loopback auth/WS/data 已交付；四个 brief 用例仍为进程内假体）。
  2. T8.2 真机/平台验收（用户登录、专用会话、Windows）。
  3. 审批催卡负结果未消除（approval 保持关闭）。
  4. 多选/自由文本/打断终态、ZAI、双手机、链接失效、官方页互踢未实跑。
  5. 次要：SSE 在会话隐藏后仍开、`zcode_actions` 无保留策略、`with_parts` release 可见、部分 UI 残项。

## 10. 合规与最小协助请求

- 未 push、未合并、未发版、未打 tag；`docs/MASTER-PLAN.md` 未改；真实链接/SID/JWT/正文未入库；真实账号凭据仅在用户浏览器与本机内存。
- **满足完整 spec？否。** 当前是 code_complete 的封闭底座；platform_validated/spec_complete 均未达成。
- 需用户本人完成的最小动作（之后 Agent 可继续 T8.2）：
  1. 在 debug 验收构建（`pnpm tauri dev --features hook-listener,zcode-bridge-evidence`）完成 BigModel 官方登录/配对并连接；
  2. 指定一个专用测试工作区/会话（当前约定 Travel/「hi」）与允许的真实动作范围；
  3. 提供 Mac 真机与 Windows 运行环境；ZAI 若要标支持需单独实跑；
  4. 审批催卡需用户亲发 guarded 消息触发；若仍不弹卡，保持 approval 关闭并记录负结果。
