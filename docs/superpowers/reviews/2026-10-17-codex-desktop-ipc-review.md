# Codex Desktop IPC 实现审查与返工清单

日期：2026-10-17。审查基线：`d5cc28b`（集成点 I）→ `3b9cdc7`。实现 worktree：`/Users/jarvis/Documents/MultiAgents-Manager/tuvis/.worktrees/headless-main-integration`。

**结论：Request changes，暂不合并，也不把本轮当作只差人工验收。** 计划中的若干核心约束只在假体/单元接口里成立，生产接线没有完成。下面按 12 个返工包组织；R1–R9 是核心协议、安全或恢复问题，R10 是 Windows 原生前置修复，R11–R12 是实际移动端链路问题。每包须有对应回归测试，修复后交回本聊天复审，再进入用户安排的手工测验。

## 审查依据与本轮验证

- 对照：`docs/MASTER-PLAN.md`、2026-10-16 spec、原实施计划及实现分支中的执行追记、验收文档。
- 三名独立只读 reviewer 分别审 IPC 协议、账本/路由、移动端/API；主审核对生产装配、Windows 代码并复核发现。
- 主审重跑三份 Rust 集成测试：**59 passed / 0 failed**；前端命令实际执行全套：**152 文件、1622 passed / 5 skipped**。
- 协议 reviewer 另运行 `cargo test --lib codex_desktop_ipc::`：**37 passed**。
- 主审用现有编译库在系统临时目录运行最小诊断，未改源码：
  ```text
  actual-style patch: need_snapshot=true, stale=true, runtime=idle
  ordinary FIFO selected kind=UserInput, raw_text=""
  ```
  前者输入为 snapshot 后的 `path=["threadRuntimeStatus","type"]` 替换增量；后者只登记一个结构化回答，再读普通 FIFO 队首。
- 未启动真实 Tuvis/Desktop，未控制用户 Thread，未访问真实账本，未修改实现分支。交接中“6 项 Rust 失败在基线同现”的归因是执行方提供的证据，本轮没有重跑整套 Rust 来再次归因，不能把它写成本轮全量 Rust 门禁通过。

## 阻塞合并的返工包

### R1 · P1：真实增量状态的根路径处理错误

位置：[mirror.rs:153](/Users/jarvis/Documents/MultiAgents-Manager/tuvis/.worktrees/headless-main-integration/src-tauri/src/services/codex_desktop_ipc/mirror.rs:153)、[adapter.rs:571](/Users/jarvis/Documents/MultiAgents-Manager/tuvis/.worktrees/headless-main-integration/src-tauri/src/services/codex_desktop_ipc/adapter.rs:571)。

Mirror 保存整个 `change`，并假定 patch 路径以 `conversationState` 起头。此前 Probe 的追踪器保存的是 `change.conversationState`，本聊天 10-16 实际读取 Windows 原日志时观察到 `path=["turnHistory","history",...]`，没有这层前缀。当前测试夹具加了前缀，遮住了合同偏差。主审最小重放也确认不带前缀的增量不能应用。

此外，adapter 只消费 `changed`，忽略 `need_snapshot`；非法/缺口增量不发布 stale、不重新 following。健康 SSE 因此可永久停在旧视图。

计划中的history不完整时调用load-complete-history也没有生产调用方，只有方法表与状态标记；不能把当前snapshot视为已实现完整历史加载。

**返工：** patch 对 conversationState 本体寻址；严格原子应用规则保留。消费 need_snapshot/error，发布 stale 并有界重拉 snapshot，不能等用户刷新救活。原 Probe 临时附件今日已不在本机，修复时保留脱敏真实形状夹具及来源，不继续沿用人为多包一层的 fixture。

**回归：** snapshot→真实根路径 replace/add/remove→流式更新；故意缺一 revision→SSE收到stale→peer收到following:true→新snapshot后恢复，无重复气泡；history不完整时可发起已验证版本的只读加载并接收后续完整snapshot。

### R2 · P1：owner 失效、断线及迟到帧没有闭环处理

位置：[adapter.rs:542](/Users/jarvis/Documents/MultiAgents-Manager/tuvis/.worktrees/headless-main-integration/src-tauri/src/services/codex_desktop_ipc/adapter.rs:542)、[adapter.rs:578](/Users/jarvis/Documents/MultiAgents-Manager/tuvis/.worktrees/headless-main-integration/src-tauri/src/services/codex_desktop_ipc/adapter.rs:578)。

`forward_events` 过滤掉所有非 state 广播，未处理 `client-status-changed`、`ipc-connection-reset`、following-status-requested；断线仅 return。旧 lease/镜像/token 没有立即失效，SSE 也不获知连接已断。处理 state 时未核对帧的 sourceClientId/epoch，而是套用 Registry 当前 lease，迟到旧 owner 帧可能污染新镜像。没有计划中的重连退避、自动重新发现和重新 following 闭环。

**返工：** 失效事件统一驱动 Registry→mirror/pending→projection；写前拒旧 lease；接收帧绑定实际来源与连接 epoch；恢复对仍有关注/待发的 Thread 自动重建，而非仅等下一次 GET。following 查询按当前关注集回复。

**回归：** 同一 socket 上 A disconnected→B接管；EOF后只保留手机SSE不刷新；旧A帧迟到；两Thread共owner失效。旧卡零写入，新snapshot恢复，无写操作重放。对应 F1/F6、S4。

### R3 · P1：生产 profile 固定一次，既不能晚启发现，也不能在升级后重新关门

位置：[codex_ipc.rs:156](/Users/jarvis/Documents/MultiAgents-Manager/tuvis/.worktrees/headless-main-integration/src-tauri/src/remote/codex_ipc.rs:156)、[adapter.rs:63](/Users/jarvis/Documents/MultiAgents-Manager/tuvis/.worktrees/headless-main-integration/src-tauri/src/services/codex_desktop_ipc/adapter.rs:63)。

Hub 装配时 resolve 一次并永久保存 profile；ProductionConnector 重连虽然 resolve，却丢弃新 endpoint 的 build。Tuvis 先启动、Desktop后启动时永久关闭 A；反向场景中，原先验证版本升级为未知 build 后仍沿用旧 profile 和已开启能力。这不是安全的“保守关闭”，后一种情况直接破坏精确版本门。

**返工：** 每次建立/恢复连接都绑定实际 endpoint 身份与 profile，build变化重新计算能力，未知build立即关 A 写能力；支持 Desktop 晚启动而不要求重启 Tuvis。

**回归：** 无Desktop→已验证build→未知build→已验证build；每步检查实际能力与零/有IPC写入，禁止用固定注入profile替代生产resolver链。

### R4 · P1：pending 上下文过期后的写入防线不完整

位置：[adapter.rs:418](/Users/jarvis/Documents/MultiAgents-Manager/tuvis/.worktrees/headless-main-integration/src-tauri/src/services/codex_desktop_ipc/adapter.rs:418)、[pending.rs:48](/Users/jarvis/Documents/MultiAgents-Manager/tuvis/.worktrees/headless-main-integration/src-tauri/src/services/codex_desktop_ipc/pending.rs:48)、[router.rs:380](/Users/jarvis/Documents/MultiAgents-Manager/tuvis/.worktrees/headless-main-integration/src-tauri/src/services/codex_control/router.rs:380)。

- `build_respond` 不检查 mirror.is_stale；缺口后旧 pending/token仍可被批准或回答。
- PendingProjector 不删除已消失请求的 token；`[r]→[]→[r]` 会复活旧token，指纹还未包含method。
- reserve只按actionId锁；不同actionId针对同一个当前pending可并行写，并不满足计划“同request提交中第二次拒绝”。客户端网络失败后再次点击又会生成新ID，放大该窗口。

**返工：** stale/unknown禁止交互；请求消失即撤销token，重现生成新token；指纹包含方法和有效内容；以thread+owner代际+requestKey/token保留唯一提交权，未知结果不能借新actionId再次发同一pending。

**回归：** pending→revision gap→旧卡批准零写；请求消失重现旧token409；两设备不同actionId同时响应同一pending只写一次；丢回执后只读核对，不再次提交。

### R5 · P1：并发跟随与回答存在反向锁序死锁

位置：[adapter.rs:169](/Users/jarvis/Documents/MultiAgents-Manager/tuvis/.worktrees/headless-main-integration/src-tauri/src/services/codex_desktop_ipc/adapter.rs:169)、[adapter.rs:327](/Users/jarvis/Documents/MultiAgents-Manager/tuvis/.worktrees/headless-main-integration/src-tauri/src/services/codex_desktop_ipc/adapter.rs:327)。

ensure_thread持owners等待mirrors；respond持mirrors等待owners。并发首次跟随t2与回答t1可互等，已预留动作也无法落账。

**返工：** 固定锁序或改为短锁取快照后复核，禁止持一个异步锁等待另一个反向取得的锁。

**回归：** 用barrier固定两条路径各持首锁的交错，限时完成；同时验证lease最终复核没有因拆锁而移除。

### R6 · P1：普通消息 FIFO 混入交互动作，且忙态/Turn顺序限制不成立

位置：[codex_actions.rs:224](/Users/jarvis/Documents/MultiAgents-Manager/tuvis/.worktrees/headless-main-integration/src-tauri/src/database/dao/codex_actions.rs:224)、[codex_ipc.rs:458](/Users/jarvis/Documents/MultiAgents-Manager/tuvis/.worktrees/headless-main-integration/src-tauri/src/remote/codex_ipc.rs:458)、[router.rs:426](/Users/jarvis/Documents/MultiAgents-Manager/tuvis/.worktrees/headless-main-integration/src-tauri/src/services/codex_control/router.rs:426)。

这是三个必须一起收口的派发合同缺口：

1. FIFO查询只筛state=queued，没筛kind=send。审批/回答register与reserve之间可被调度器取走；更稳定的触发是交互reserve因审计失败回滚，随后flush把无正文交互行当成普通消息发送。主审诊断已输出 `kind=UserInput, raw_text=""`。
2. 无镜像时busy_snapshot=false；直到AdapterDispatch中才ensure。即使首snapshot是active/unknown，仍直接start-turn，违反忙态留队。
3. active_turns仅存`()`, flush无条件clear，未绑定本动作turnId的完成；首条ack前的遗留idle更新可以放行第二条。`outcome_of`只读result.turn.id，但Mac Probe记录的是result.result.turn；真实回执还可能拿不到turnId。

**返工：** 普通队列严格只收Send；交互有自己的失败/提交生命周期。先取得新鲜状态、最后派发时复核busy。关联并持久保存本次Turn身份，只有对应终态释放顺序限制；缺ID不能视为完成。按真实回执封套解包turnId。

**回归：** 审计INSERT失败的回答不能变成send；首次直接POST且snapshot active只入队；Accepted前后旧idle均不释放；真实嵌套回执提取turnId；进程重启后仍等待原Turn终态。不要再用第二次手工flush替代完成事件。

### R7 · P1/P2：生产恢复驱动与订阅生命周期缺失

位置：[codex_ipc.rs:250](/Users/jarvis/Documents/MultiAgents-Manager/tuvis/.worktrees/headless-main-integration/src-tauri/src/remote/codex_ipc.rs:250)、[adapter.rs:161](/Users/jarvis/Documents/MultiAgents-Manager/tuvis/.worktrees/headless-main-integration/src-tauri/src/services/codex_desktop_ipc/adapter.rs:161)。

Hub启动只订阅未来事件，无queued/fence枚举、恢复关注或周期对账；完整重启后没有投影来源，旧队首也不会因新发送主动flush。compat执行晚于helper返回才结束且无owner时，fence也可能永不再核对。

following在discovery之前占位，失败不回滚；生产无unfollow调用方，该方法自身也不发following:false。浏览或失败发现64个不同Thread便耗尽名额。stop只停调度器，没有完成计划中的订阅/恢复任务关闭。全体镜像64MiB预算也未落实，目前只有单Thread上限。

**返工：** 保留已做的reserved→unknown恢复，补有界queued/fence恢复关注与周期对账；详情/多设备关注引用计数、失败回滚、关闭时解订阅；总镜像预算及临时副本计费。

**回归：** 丢弃所有内存重建hub，不打开详情、不手工flush，旧队列能恢复；无owner兼容完成后自行解除fence；连续65次挂载卸载和64次失败发现不耗尽；远程关闭后无新订阅/派发，已派发仅继续落账。

### R8 · P1：不支持A时绕开统一router，并恢复被禁止的自然语言补投

位置：[api.rs:3204](/Users/jarvis/Documents/MultiAgents-Manager/tuvis/.worktrees/headless-main-integration/src-tauri/src/remote/api.rs:3204)、[api.rs:8225](/Users/jarvis/Documents/MultiAgents-Manager/tuvis/.worktrees/headless-main-integration/src-tauri/src/remote/api.rs:8225)、[SessionDetail.tsx:1561](/Users/jarvis/Documents/MultiAgents-Manager/tuvis/.worktrees/headless-main-integration/src/mobile/SessionDetail.tsx:1561)。

只有ipc_available才走hub。未知build直接走旧helper，忽略actionId、绕过持久去重和compat fence；这恰是应当受统一降级纪律保护的场景。旧问答补投也只在fresh A投影时阻断，stale/无owner/unknown build重新退回AppRepost，前端主动保留入口。

**返工：** 所有Codex APP普通消息进入统一router，不论A是否可用；仅普通消息可经H3兜底。Codex APP审批/结构化回答始终原生端点，不支持就明确告知，旧question/answer的文本补投一律拒绝。删除把这条错误偏差锁住的 `app_repost_kept_when_no_projection` 预期。

**回归：** 未知build/H3开，同action重放、重启均只投一次且有fence；stale/无owner/未知build的旧问答入口均零IPC写、零queue/exec投递。对应spec F7/S5，不能用“兼容”解释放宽。

### R9 · P1/P2：派发前持久化失败及派发后的任务取消处理不完整

位置：[codex_ipc.rs:755](/Users/jarvis/Documents/MultiAgents-Manager/tuvis/.worktrees/headless-main-integration/src-tauri/src/remote/codex_ipc.rs:755)、[router.rs:500](/Users/jarvis/Documents/MultiAgents-Manager/tuvis/.worktrees/headless-main-integration/src-tauri/src/services/codex_control/router.rs:500)。

run_compat忽略fence/evidence写错误后照常调用真实helper；若发送后崩溃，恢复时没有顺序屏障。A派发则内联等待于请求future，取消该future后finish丢失；现有名为 `client_disconnect_does_not_cancel_dispatched_write` 的测试在handler abort、Desktop成功后反而断言Reserved，没有证明计划要求的继续落账。

**返工：** fence/evidence原子持久化成功是compat的派发前提；失败零helper调用。已reserve动作由独立任务持有至结果落账/unknown，不依赖HTTP等待者；终态持久化失败不能对外虚称完成，也不能释放顺序限制。

**回归：** 用SQLite trigger拒绝fence/evidence更新，两个通道写入均0；abort HTTP等待者后peer成功，账本自动Accepted，或确实断连时自动Unknown，不能永久Reserved。区分测试主动取消与实际HTTP连接关闭，不把后者未经测试的取消语义写成已证明。

### R10 · P2：Windows管道错误码错误，原生测试占用生产endpoint

位置：[windows.rs:141](/Users/jarvis/Documents/MultiAgents-Manager/tuvis/.worktrees/headless-main-integration/src-tauri/src/services/codex_desktop_ipc/transport/windows.rs:141)、[windows.rs:185](/Users/jarvis/Documents/MultiAgents-Manager/tuvis/.worktrees/headless-main-integration/src-tauri/src/services/codex_desktop_ipc/transport/windows.rs:185)。

`ERROR_PIPE_BUSY` 被写成2231，实际Windows SDK/crate常量为231；真实忙管道立即失败，不走预期等待重试。应直接使用SDK常量，避免另一处数字漂移。

假server测试 `first_pipe_instance(true).create(PIPE_NAME)` 使用真实 `\\.\pipe\codex-ipc`，Desktop在场时测试会冲突。测试应使用随机专属pipe名和注入endpoint，绝不占用生产命名空间。

此外，[ci.yml:135](/Users/jarvis/Documents/MultiAgents-Manager/tuvis/.worktrees/headless-main-integration/.github/workflows/ci.yml:135) 的测试job在Ubuntu，后续只有Windows交叉check/clippy，没有实际Windows运行job。“等CI windows job跑原生pipe测试”的交接说法不成立，须改成明确Windows实机执行，或真实新增native job。

**回归：** Windows原生：忙返回231→实例释放→连接成功；预算耗尽诚实超时；读阻塞不阻塞写；Desktop运行同时跑假pipe测试也通过且互不连接。

### R11 · P1：新入队的消息在当前详情页不可见、不可撤回

位置：[CodexQueuePanel.tsx:55](/Users/jarvis/Documents/MultiAgents-Manager/tuvis/.worktrees/headless-main-integration/src/mobile/CodexQueuePanel.tsx:55)、[SessionDetail.tsx:1556](/Users/jarvis/Documents/MultiAgents-Manager/tuvis/.worktrees/headless-main-integration/src/mobile/SessionDetail.tsx:1556)。

面板只在挂载/换session/自身撤回后加载，未接投影跃迁或发送入队回执。首次为空时发送codex_queued，面板仍自隐，用户不能查看或撤回本条；compat-only场景还不渲染该新队列面板。

**返工：** 入队、派发、撤回、fence变化和相关状态跃迁触发同源队列刷新；A暂不可用时已持久待发项仍可见且可撤。处理乱序fetch，避免旧session结果回灌。

**回归：** 真正渲染SessionDetail：空队列→发送入队→不离页即可查看/撤回；SSE推进后队位消失；A断连仍可撤回未发。对应宪法F2.2。

### R12 · P2：原生动作的客户端ID与回执状态机仍沿用错误语义

位置：[MessageComposer.tsx:684](/Users/jarvis/Documents/MultiAgents-Manager/tuvis/.worktrees/headless-main-integration/src/mobile/MessageComposer.tsx:684)、[MessageComposer.tsx:704](/Users/jarvis/Documents/MultiAgents-Manager/tuvis/.worktrees/headless-main-integration/src/mobile/MessageComposer.tsx:704)、[PendingRequestCard.tsx:149](/Users/jarvis/Documents/MultiAgents-Manager/tuvis/.worktrees/headless-main-integration/src/mobile/PendingRequestCard.tsx:149)。

- 原生发送先上报headless sending，desktop_ipc/codex_queued分支直接return，父级永久显示“无头回合进行中”及不适用的取消按钮。
- NotSent/Rejected保留同actionId；服务端终态只回放旧回执。条件恢复后重按永远旧失败，编辑正文则action_conflict，需离页才恢复。
- Pending每次点击新ID，网络失败重新解锁却不查询旧action；同时相同requestKey换contextToken后，旧Promise仍能回写receipt/清新草稿，旧accepted可能锁死新卡。

**返工：** native与headless回执分别管理；明确零投递后的下一次用户动作使用新ID，网络未知先查原action；pending提交ID保持到结果核对，按token/generation丢弃旧异步响应。

**回归：** native三种回执不残留headless卡；NotSent后恢复可再发新动作；丢POST响应不新ID重投；新token/草稿不受旧响应污染。

## 执行偏差评价

| 偏差/声明                                             | 评价与处置                                                                                                                            |
| ----------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------- |
| #117合流main得到I，再叠IPC分支，不改原公开历史        | 合理，符合计划。保留此策略                                                                                                            |
| ActionContext.kind、DesktopDispatch/reconcile等测试缝 | 合理的接口细化；但不能让假体取代生产装配测试。kind进入账本后普通FIFO必须筛Send                                                        |
| T9 403→null、useRef保留actionId、SSE三次失败降级      | 方向合理；ref终态更新和真实跨组件刷新未闭合，按R11/R12修                                                                              |
| CompatEvidence.turn_completed恒false                  | 合理、诚实；消费不等于Turn完成。但必须完成实际reconcile调度，不能永久保守卡队列                                                       |
| T8静态profile、“Desktop晚启要求重启Tuvis”             | 不合理，破坏F1/F6，且升级时还会沿用已开启旧能力；按R3改                                                                               |
| 不支持A时保留旧问答补投/旧helper直发                  | 不合理，直接违反最新spec和计划；不能由实施者以兼容语义自行放宽，按R8改                                                                |
| Task1实现与测试同稿，缺红灯证据                       | 过程偏差，如实记录可接受；PASS不等价于曾证明测试能抓错。无需重写历史，但这轮返工必须补能失败的具体回归                                |
| 6项Rust失败在未加IPC的基线复现                        | 若基线证据一致，可归为#117/环境债而非新回归；仍不能称完整门禁全绿。集成分支须明确负责人/修复或平台化fixture方案，最终CI红灯仍不能合并 |
| 所有真机格均NOT_TRIGGERED，未翻生产flag               | 如实申报是正确的；Windows缺机器、重启未获安排可保留待验。但Task10未完成，不能把Task0–10整体报完成                                     |
| 把真实端到端换成各层mock通过                          | 不足以满足Task10。特别是手工flush代替恢复、合成多一层patch路径、断连测试断言Reserved，应改测试而非只改说明                            |

建议更新交接口径为：**Task0–9有实现但本轮审查未通过；自动检查有已通过项和已知红项；Task10真机部分待执行。** 计划中的未完成勾选保持未完成，不因提交了验收文档视为全部验收完成。

## 返工交接要求

1. 在当前IPC分支分包提交修复，保持I不变；不要推送/合并main，不修改宪法。
2. 先修协议合同与安全边界（R1–R9），再修Windows与移动端；每包提供回归测试名和红→绿结果。
3. 至少增加一条使用生产AdapterDispatch/Hub/Router的假IPC全链测试，不注入一个绕过关键装配的FakeDispatch冒充该链。
4. 回报修复HEAD、逐R编号状态、完整检查结果、残留风险。原聊天复审无阻塞后，再由用户开启手工测验。
5. Windows执行范围见同目录 `2026-10-17-codex-desktop-ipc-windows-validation.md`。
