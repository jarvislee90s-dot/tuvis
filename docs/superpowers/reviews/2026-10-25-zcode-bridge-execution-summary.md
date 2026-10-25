# ZCode 统一远控：A 包 + B 包执行总汇报

> 日期：2026-10-25。工作目录 `/Users/jarvis/Documents/tuvis-worktree2`，分支 `codex/zcode-unified-remote`（base `35698a33`，docs cherry-pick `8d0f7b00`）。未 push、未合并、未发版。过程账本（本地、gitignored）：`.superpowers/sdd/2026-10-24-remote-settings-and-zcode-read-recovery/progress.md`、`.superpowers/sdd/2026-10-24-zcode-official-bridge-validation/progress.md`。

## 1. 一句话结论

A 包（远程设置重排 + ZCode 读取恢复）全部完成并通过独立评审；B 包完成 B0–B3 实跑取证（授权/只读/发送/问答/打断），B4 完成源码合同与合同层代码（实跑延后），**B5 生产实施计划已交付**。所有"统一控制"能力按实际取证逐项开放，未取证项在合同与计划中明确标 false/延后。

## 2. A 包（全部完成）

| 项 | 内容 | 提交 |
| --- | --- | --- |
| A1 | H3 与兼容发送缺键默认=true、显式 false 保留、非法值 false、资格守卫不变 | `da02fd6f` |
| A2 | 设置六段顺序 + 无头区抽取 + 工具区装配点 | `760b5140` `cc8c22e9` |
| A3 | PIN 初始遮罩、眼睛切换、失焦/重挂隐藏、零存储副作用 | `bdd7b073` |
| A4 | ZCode 官方链接配置（严格校验/IPC/配对只读 GET/TS 客户端） | `ae3e3353` |
| A5 | 折叠 ZCode 卡 + 应用内教程 + 手机局部控制提示（Saved≠Connected） | `13134608` |
| A6 | 双形态会话 ID（sess_+UUID / session_+32hex），原 ID 保留 | `73cceb29` |
| A7 | 附件锚点对齐最终显示序号（同源投影，file-only 中性 0） | `893c27a4` |
| A8 | 双语手册/CHANGELOG/升级须知 + 全量门禁（pnpm 2213 通过；cargo 3423 通过） | `55d9626b` `dce33a0f` |

另有整分支终审修复波 `a8678a96`（注释/账本口径）。

## 3. B 包（按门推进）

| 任务 | 结论 | 证据 |
| --- | --- | --- |
| B0 取证准备 | 完成（源码 SHA、许可证、模块清单；relay 服务端不在仓库） | `fe01bd28` |
| B1 官方身份/只读连接 | **PROVEN**：bigmodel 回环 OAuth 被接受；token 60s/一次性；`mid` 路由；自动配对（无 APP 确认）；只读两查询成功 | run `macos-B120261024T105940Z-NarmSw`；`4148cbc2` |
| B2 单连接/会话映射/发送 | **闭环**：119 行任务索引、精确 id 归属、单帧发送 accepted、用户在 APP 亲见 marker 且 agent 已处理 | run `macos-B220261024T132316Z-prItvw`；`5f6e7cdb`；contract send=true |
| B3 审批/问答/打断 | 问答 **RESOLVED**、打断 **accepted**；审批：本会话无 `permission` 类卡（用户所称"审批"实为 `userInput` 问答卡，已用 `--existing-pending` 应答 accepted） | runs `macos-B320261025T012945Z-aMtnqA`、`...T013128Z-X8HkPg`、`...T024812Z-zi883Q`；`b300d5b3` |
| B4 模型/思考强度 | **思考强度实跑通过**（读→切 max→low→新快照确认→恢复 max→确认恢复，`COMPLETED:selection-switch-restored`）；模型切换未取证（目录仅当前模型，需 getView 角色验证）；合同层 82/82 | run `macos-B320261025T025928Z-3WyTFA`；`60dda0d5` `13152880` |
| B5 生产实施计划 | **已交付**（未执行）：冻结接口、T1–T8 任务卡、人机分工、风险、DoD | `docs/superpowers/plans/2026-10-25-zcode-official-bridge-implementation.md`；`d5245338` |

## 4. 关键发现（对后续生产最重要）

1. **`mid` 是设备路由键**：terminal 的 ws2 URL 必须带链接里的 `mid`（官方条件性），缺失即永远 `waiting`——B2 两轮 pair-timeout 的真因。
2. **官方没有"APP 侧配对确认"**：配对在 token 后 ~131ms 自动 matched；用户观察与探针证据一致。
3. **会话事件流要走通道订阅信封**：EventListen(`onDynamicConversationFrame`) + subscribe + activate 才有 snapshot/deltas；普通方法调用只回 ack 不给事件。
4. **桥上有二进制 Initialize 握手**（`04 01 06 C8 01 00`）：必须先回执再发请求，否则帧被静默丢弃。
5. **卡片类型要对齐**：`pendingInteractions` 里 `kind` 可为 `permission`/`userInput`/`workspaceHookReview`；计划审批投影为 `userInput`（`schema.interaction:"plan_approval"`），必须按 `toolName` 判别，禁止把计划审批当普通问答。
6. **autoResolution**：部分卡带自动推进计时；应答前需先发 snooze 帧。

## 5. 未取证 / 待办（恢复点）

| 待办 | 具体缺口 | 恢复动作 |
| --- | --- | --- |
| B4 模型切换 | 思考强度已证；模型切换需 `model-selection.getView()` 目录 | ①验证 getView 对 mobileRemote 角色可见 ②补一次 model 切换实跑 |
| B4 原计划项（已部分完成） | 合同层已提交（`selectionBridge.ts` + 6 合同测试，82/82 绿）；缺探针接线与一次真实"读取→切换→确认→恢复原值" | ①在 session-probe 增加 `--interaction` 同族的 `--selection` 分支（复用连接/映射/topic 流，捕获 `snapshot.config`）②`selection-probe.ts` 薄包装 ③一次实跑（用户点授权）④按证据翻 `capabilities.model/thoughtLevel` |
| 审批（`permission` 类卡） | 本会话无 `permission` 卡（自动批准或仅 `userInput` 卡）；卡路径 wire 形状已源码取证但未实跑 | 在一个会弹 permission 卡的环境（不同权限模式/工具）复跑 `approve`/`reject` 两轮；在此之前 UI 审批保持关闭 |
| 双手机并发 / 断线重连 / 链接失效流转 / 官方网页互踢实测 / Windows 同口径 | 未覆盖 | 按 B5 计划 T8 验收清单执行 |
| A8 平台验收 | Windows 六段/PIN/默认值实机；真机手机读取与发送区状态；真实链接生命周期 | 按 A8 报告 `awaiting_platform_validation` |

## 6. 交付物索引

- 生产实施计划（B5）：`docs/superpowers/plans/2026-10-25-zcode-official-bridge-implementation.md`
- 官方桥接合同与证据：`docs/superpowers/decisions/2026-10-24-zcode-official-bridge-contract.md`、`tests/fixtures/zcode-official-bridge/contract.json`
- 探针工具：`scripts/zcode-bridge-probe/`（auth-check / session-probe / interaction-probe / selectionBridge / relayAuth / oauthFlow / sessionBridge / interactionBridge / sanitize）
- 测试：`tests/remote/zcodeBridgeRelay|Probe|Session|Interaction|Selection.test.ts`（82 通过）
- 过程账本与逐任务报告：`.superpowers/sdd/2026-10-24-*/`（本地，gitignored）
- 私有脱敏证据：`~/Documents/tuvis-verify-logs/zcode/<run_id>/`（不入库）

## 7. 合规声明

- 未 push、未合并、未发版、未打 tag；`docs/MASTER-PLAN.md` 全程未改。
- 真实账号凭据：仅本机内存；探针证据全部脱敏（0700/0600 + fail-closed 断言）；仓库内无真实链接/SID/JWT/token/会话正文。
- 真实动作授权：B2 一条 marker 发送、B3 问答/打断/既有卡应答，均在用户指定的 Travel/「hi」会话内、逐批授权；未使用 full-access、未改会话权限模式、未 kill 用户进程。
- 官方登录/扫码均由用户本人在浏览器完成。

## 8. 我代为作出的裁决（均可回溯到账本）

1. A1 文案/提示 minor 并入 A2（同文件动土）。
2. A4 空白保存写 `""` 不删行（DAO 无删除原语；读侧归一 Missing）。
3. A6 未知新形态子代理 ID 维持拒绝（后代仲裁按 parent_id 列，不受 ID 门影响）。
4. A8「与 B 接口登记」以协调层登记满足（B 账本 + 决策文件）。
5. 审批 `reject` 轮在自动批准会话中跳过（同结论，省一次用户授权）。
6. B3 收尾代码在连续 5 次子代理被平台中断后由 controller 落地，并补独立评审（2 Critical + 8 Important 全部修复后放行）。
7. `--existing-pending` 模式：用户显式指定已存在的卡，跳过触发与"触发后"关联（仅 approve/reject/answer）。
8. B4 实现延后（子代理输出上限反复触发），先交付源码合同 + 合同层代码 + 测试，实跑留恢复点。
