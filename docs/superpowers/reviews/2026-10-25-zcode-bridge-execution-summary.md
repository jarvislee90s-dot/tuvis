# ZCode 统一远控：A 包 + B 包执行总汇报

> 日期：2026-10-25。工作目录 `/Users/jarvis/Documents/tuvis-worktree2`，分支 `codex/zcode-unified-remote`（base `35698a33`，docs cherry-pick `8d0f7b00`）。未 push、未合并、未发版。过程账本（本地、gitignored）：`.superpowers/sdd/2026-10-24-remote-settings-and-zcode-read-recovery/progress.md`、`.superpowers/sdd/2026-10-24-zcode-official-bridge-validation/progress.md`。

## 1. 一句话结论

A 包代码与自动门禁完成并通过独立评审，**平台验收（Windows/真机/链接生命周期）仍待完成**；B 包 B0–B5 完成：授权 / 只读 / 发送 / 模型 / 档位已实跑；审批仅"应答既有卡"（批准+拒绝+产物）已证，**Tuvis 发送→弹卡未证且有负结果**；问答仅单选且为 ACK-only，打断为 ACK-only——**终态未证**。合同已按证据分层标注 true/false，**B5 生产计划已交付（未执行）并按复审补了停止门**。

## 2. A 包（代码与门禁完成，平台验收待完成）

| 项  | 内容                                                                                                                   | 提交                  |
| --- | ---------------------------------------------------------------------------------------------------------------------- | --------------------- |
| A1  | H3 与兼容发送缺键默认=true、显式 false 保留、非法值 false、资格守卫不变                                                | `da02fd6f`            |
| A2  | 设置六段顺序 + 无头区抽取 + 工具区装配点                                                                               | `760b5140` `cc8c22e9` |
| A3  | PIN 初始遮罩、眼睛切换、失焦/重挂隐藏、零存储副作用                                                                    | `bdd7b073`            |
| A4  | ZCode 官方链接配置（严格校验/IPC/配对只读 GET/TS 客户端）                                                              | `ae3e3353`            |
| A5  | 折叠 ZCode 卡 + 应用内教程 + 手机局部控制提示（Saved≠Connected）                                                       | `13134608`            |
| A6  | 双形态会话 ID（sess_+UUID / session_+32hex），原 ID 保留                                                               | `73cceb29`            |
| A7  | 附件锚点对齐最终显示序号（同源投影，file-only 中性 0）                                                                 | `893c27a4`            |
| A8  | 双语手册/CHANGELOG/升级须知 + 全量门禁（pnpm 2213 通过；cargo 3423 通过）；**Windows/真机/链接生命周期平台验收待完成** | `55d9626b` `dce33a0f` |

另有整分支终审修复波 `a8678a96`（注释/账本口径）与 2026-10-25 复审修复波 `71db44a6` / `33b8e6af` / `cf494594`（写入错误传播、草稿竞态、终态观察、逐模型档位、证据/合同口径）。

## 3. B 包（按门推进）

| 任务                    | 结论                                                                                                                                                               | 证据                                                                                                               |
| ----------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------ |
| B0 取证准备             | 完成（源码 SHA、许可证、模块清单；relay 服务端不在仓库）                                                                                                           | `fe01bd28`                                                                                                         |
| B1 官方身份/只读连接    | **PROVEN**：bigmodel 回环 OAuth 被接受；token 60s（一次性=source+fake，实跑未做重放拒绝）；`mid` 路由；自动配对（无 APP 确认）；只读两查询成功                     | run `macos-B120261024T105940Z-NarmSw`；`4148cbc2`                                                                  |
| B2 单连接/会话映射/发送 | **闭环**：119 行任务索引、精确 id 归属、单帧发送 accepted、用户在 APP 亲见 marker 且 agent 已处理                                                                  | run `macos-B220261024T132316Z-prItvw`；`5f6e7cdb`；contract send=true                                              |
| B3 审批/问答/打断       | 问答（单选）**ACK accepted，终态未证**；打断 **ACK accepted，执行停止未证**；审批**应答既有卡**批准+拒绝（产物核对）；**Tuvis 发送→弹卡未证（negative evidence）** | runs `...T012945Z-aMtnqA`、`...T013128Z-X8HkPg`、`...T032425Z-K7I6ZA`、`...T034803Z-noorSA`；`b300d5b3` `9ef62834` |
| B4 模型/思考强度        | **实跑通过**：目录读取（getView）+ 同 provider 模型切换（Flash→GLM-5.3）+ 档位切换（max→low）+ 新鲜快照确认；**恢复原值仅探针清理**                                | runs `...T030637Z-bS5dOg` / `...T025928Z-3WyTFA`；`60dda0d5` `13152880` `3eb47a14`                                 |
| B5 生产实施计划         | **已交付**（未执行）：冻结接口、T1–T8 任务卡、人机分工、风险、DoD                                                                                                  | `docs/superpowers/plans/2026-10-25-zcode-official-bridge-implementation.md`；`d5245338`                            |

## 4. 关键发现（对后续生产最重要）

1. **`mid` 是设备路由键**：terminal 的 ws2 URL 必须带链接里的 `mid`（官方条件性），缺失即永远 `waiting`——B2 两轮 pair-timeout 的真因。
2. **官方没有"APP 侧配对确认"**：配对在 token 后 ~131ms 自动 matched；用户观察与探针证据一致。
3. **会话事件流要走通道订阅信封**：EventListen(`onDynamicConversationFrame`) + subscribe + activate 才有 snapshot/deltas；普通方法调用只回 ack 不给事件。
4. **桥上有二进制 Initialize 握手**（`04 01 06 C8 01 00`）：必须先回执再发请求，否则帧被静默丢弃。
5. **卡片类型要对齐**：`pendingInteractions` 里 `kind` 可为 `permission`/`userInput`/`workspaceHookReview`；计划审批投影为 `userInput`（`schema.interaction:"plan_approval"`），必须按 `toolName` 判别，禁止把计划审批当普通问答。
6. **autoResolution**：部分卡带自动推进计时；应答前需先发 snooze 帧。
7. **审批卡的催卡前提**：本构建只有**用户在 APP 亲发**（「变更前确认」模式）的提交会被拦截弹 `permission` 卡；探针合成的 sendText（含 `mode:"guarded"`）直接执行不弹卡——生产 UI 必须由用户真实发起动作，适配器只应答。
8. **提交模式是逐条消息的意图**：`sendText.payload.mode ∈ build|edit|plan|yolo|guarded`（command.ts:95-115；submission.ts:4），不是持久会话设置。
9. **terminal 发送绕过 guarded**：本构建 terminal 发起（默认或 `mode:"guarded"`）直接执行、不弹 permission 卡；只有用户在 APP 亲发（guarded）才弹。故审批已证的是"应答既有卡"，**不是**"Tuvis 发送→弹卡→应答"；合同 `approvalForTerminalSend=false`，生产 T5 加停止门。
10. **ACK ≠ 终态**：历史 answer/interrupt run 在 ACK 后 4–5ms 即 teardown，未观察宿主撤卡/执行停止；`RESOLVED` / `STOPPED:interrupt-accepted` 只能算"命令受理已证"。探针已加 10s 终态观察窗，未来 run 分别报 `RESOLVED:pending-cleared` / `STOPPED:interrupt-confirmed`，否则只报 `ACCEPTED:*:unconfirmed`。

## 5. 未取证 / 待办（恢复点）

| 待办                                                                     | 具体缺口                                                                | 恢复动作                                                          |
| ------------------------------------------------------------------------ | ----------------------------------------------------------------------- | ----------------------------------------------------------------- |
| B3 终态补证                                                              | answer 未观察宿主撤卡；interrupt 未观察守卫执行消失                     | 用户再授权一次 OAuth + 一次会话触发，跑新终态观察窗（探针已就绪） |
| 问答多选 / 自由文本                                                      | 仅 fake/合同层，无实跑入口                                              | 先补实跑取证，再开 `userInputMultiSelect/FreeText`                |
| 双手机并发 / 断线重连 / 链接失效流转 / 官方网页互踢实测 / Windows 同口径 | 未覆盖                                                                  | 按 B5 计划 T8 验收清单执行                                        |
| A8 平台验收                                                              | Windows 六段/PIN/默认值实机；真机手机读取与发送区状态；真实链接生命周期 | 按 A8 报告 `awaiting_platform_validation`                         |

## 6. 交付物索引

- 生产实施计划（B5）：`docs/superpowers/plans/2026-10-25-zcode-official-bridge-implementation.md`
- 官方桥接合同与证据：`docs/superpowers/decisions/2026-10-24-zcode-official-bridge-contract.md`、`tests/fixtures/zcode-official-bridge/contract.json`
- 探针工具：`scripts/zcode-bridge-probe/`（auth-check / session-probe / interaction-probe / selectionBridge / relayAuth / oauthFlow / sessionBridge / interactionBridge / sanitize）
- 测试：`tests/remote/zcodeBridgeRelay|Probe|Session|Interaction|Selection.test.ts`（71 通过）；全量 `pnpm test` 2285 通过 / 5 skipped，`cargo test --features marker-helper` 3424 通过 / 46 ignored；`pnpm check`、`pnpm build:mobile`、`cargo fmt --check`、`cargo clippy -D warnings` 全绿
- 过程账本与逐任务报告：`.superpowers/sdd/2026-10-24-*/`（本地，gitignored）
- 私有脱敏证据：`~/Documents/tuvis-verify-logs/zcode/<run_id>/`（不入库）

## 7. 合规声明

- 未 push、未合并、未发版、未打 tag；`docs/MASTER-PLAN.md` 全程未改。
- 真实账号凭据：仅本机内存；**证据落盘面**脱敏（0700/0600 + fail-closed 断言），仓库内无真实链接/SID/JWT/token/会话正文。OAuth URL 连同 state 只打印到 **stdout** 供用户点击（state/link 已登记 redactor，不写证据文件）；桥消息证据只记结构头，正文不再入盘。此边界不等于"所有终端输出均脱敏"。
- 真实动作授权：B2 一条 marker 发送、B3 问答/打断/既有卡应答，均在用户指定的 Travel/「hi」会话内、逐批授权；未使用 full-access、未改会话权限模式、未 kill 用户进程。
- 官方登录/扫码均由用户本人在浏览器完成；审批卡的触发消息也由用户在 APP 手打（探针只应答）。
- 最终代码提交：`cf494594`（复审修复三提交 `71db44a6` / `33b8e6af` / `cf494594`）；本文件口径更新随后单独提交。

## 8. 我代为作出的裁决（均可回溯到账本）

1. A1 文案/提示 minor 并入 A2（同文件动土）。
2. A4 空白保存写 `""` 不删行（DAO 无删除原语；读侧归一 Missing）。
3. A6 未知新形态子代理 ID 维持拒绝（后代仲裁按 parent_id 列，不受 ID 门影响）。
4. A8「与 B 接口登记」以协调层登记满足（B 账本 + 决策文件）。
5. 审批 `reject` 轮在自动批准会话中跳过（同结论，省一次用户授权）。
6. B3 收尾代码在连续 5 次子代理被平台中断后由 controller 落地，并补独立评审（2 Critical + 8 Important 全部修复后放行）。
7. `--existing-pending` 模式：用户显式指定已存在的卡，跳过触发与"触发后"关联（仅 approve/reject/answer）。
8. B4 曾因平台输出上限延后；后由 controller 落地 `--interaction=select` 接线（CAS baseRevision）并完成实跑，裁决已闭合。
9. 审批催卡由用户亲发消息触发（探针不代造动作）；`reject` 轮在自动批准会话中曾按同结论跳过，后在「变更前确认」模式补跑完成。
10. 2026-10-25 独立复审修正：answer/interrupt 历史证据降级为 ACK-only；`approvalForTerminalSend=false`；问答仅单打开放；档位逐模型校验；生产不得恢复用户选择；桥消息正文不再入证据。
11. A8 口径改为"代码/门禁完成，平台验收待完成"（不把待验项写成全部完成）。
