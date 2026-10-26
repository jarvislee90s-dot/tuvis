# ZCode 官方桥接：独立复审修复清单与进度（2026-10-27）

> 本文件是本轮复审修复的**唯一进度账本**（随仓库提交），用于上下文压缩后恢复。
> 分支 `codex/zcode-unified-remote`，复审基线 `8e663f09..90ae20be`，本轮起点 HEAD
> `90ae20be`（工作树仅 `codec.rs`/`wire_e2e.rs` 两个未提交的 T8.1 回环补测）。
> 不 push、不合并、不发版、不打 tag；`docs/MASTER-PLAN.md` 只读。
> 执行方式：单一实现子代理写入（共享工作树串行）＋ TDD（先 RED）＋ 独立审查后提交。
> 本轮不连接真实 ZCode、不发送真实消息/审批、不请求用户协助；`platform_validated` /
> `spec_complete` 保持未完成。

## 0. 复审结论与修复原则

上一轮独立复审的结论是 **“需修正，不能仅凭自动门禁全绿判定 code_complete”**。本轮
接受该结论，不争论口径；只修代码层问题，真机/Windows/手机验收后置。

必须保留的设计（复审明确认可，不得在修复中改掉）：

- 窄 trait（`HubBackend`/`RelaySink`/`CommandPort`/`PendingPort`/`StopPort`/
  `SelectionPort`）；
- Hub 公开 generation 与桥内部 `bridge_generation` 分离；
- 独立控制面板；未实跑能力默认关闭；ZAI 入口暂禁用；
- ACK 与终态分开、`unknown` 不自动重发、无 CLI/终端兜底；
- 真实验收后置，**不得**为了让测试通过把生产能力直接设为 `true`。

## 1. 逐项核实（源码根因）

状态：`已核实` = 已对照源码/测试确认问题成立；`待修` / `修复中` / `已修待审` /
`已审已提交` / `有异议`。

| # | 级别 | 状态 | 根因（源码位置） | 最小修复方向 | 回归测试 |
| --- | --- | --- | --- | --- | --- |
| 1 | 严重 | 已审已提交 `b94be0e0` | 物理分片去重只看 `messageSeq` 单调，首片即把 `last_message_seq` 推进（`bridge.rs:2526-2542`），同一逻辑消息后续物理分片被当重复丢弃 | 只丢弃**严格更旧**的 `messageSeq`；同值交给 `FrameAssembler` 按 `(messageSeq, fragmentIndex)` 去重/组装；物理 `seq` 重复/缺口仍由原检查负责 | 经 `BridgeManager::on_data` 注入同 `messageSeq`、递增 `seq` 的双物理分片，断言 ready/revision/pending、首片 0 ACK、整条恰好 1 ACK |
| 2 | 严重 | 已审已提交 `0d36d516` | Hub 事件泵 `RelayEvent::Data` 只调 `bridge.on_data`，不发布 Session 事件（`hub.rs:585-588`） | `on_data` 同锁前后快照返回变化 `SessionView`；Hub 泵校验同代际+paired、覆写 Hub 代际、同会话沿用能力后锁外广播 Session | bridge：真实 `on_data` delta 恰返回 1 条变化视图、重放空；hub：FakePump 变化视图发布 Session（pending/control/能力/无变化不发） |
| 3 | 严重 | 待修 | hook 连接事件只在 generation 增大时 `refresh()`（`useZcodeControl.ts` connection 分支）；同代 `waiting/needs_auth → paired` 不拉会话 | 连接态从非 paired 进入 paired（或首次 paired 且 session 为空）时静默重拉；snapshot 同规 | 手机先打开、桌面后连接，断言 session 由 null 变非空 |
| 4 | 一般 | 待修 | `Snooze` 在 `check_policy` 直接 `return Ok(Vec::new())`（`hub.rs:1029-1045`）；`snooze_interaction` 返回值被忽略 | snooze 只允许：匹配的问答/审批能力已开启，或存在本会话/本代际可绑题许可；绑题失败即拒（零动作） | 能力全关 + 无许可时 snooze 拒绝；有合法许可时绑定成功；错题/过期拒绝 |
| 5 | 一般 | 待修 | 问答能力按客户端 `selectedValues` 数量挑单选/多选（`hub.rs:1046-1070`），未看宿主题型；混合题只查一项 | 以宿主 pending 卡题型推导**全部**所需能力（每题单选/多选/自由文本），逐项过闸；客户端声明只做形状校验 | 多选卡只传 1 个值不得借单选许可；混合题缺任一能力即拒 |
| 6 | 一般 | 已审已提交 `0b5e5dda` | `Hub::execute` 先 `control_generation`/`check_policy`，而持久幂等查询在 `CommandSender` 内部（`hub.rs:953-985`） | 代际判定后先做只读幂等识别（device+session+generation+kind+hash），命中直接返回既有回执（不发 Action 事件）；冲突先上抛；未命中再判连接/许可/新投递 | Hub needs_auth 重放/冲突/stale-first；真实 ActionStore accepted/unknown 取回且零帧零行零审计；换载荷/跨设备/换代 conflict |
| 7 | 一般 | 待修 | 关停用 5s `timeout` 等待 `hub.disconnect()`（`remote/mod.rs:751-770`）；`disconnect` 先等 `connect_lock`（`hub.rs:863-880`），超时取消清理，在途 connect 仍可成功 | 关闭意图先原子推进代际/离线并使连接尝试失效；清理任务不被超时取消（超时只停止等待） | connect 持锁跨网络等待时触发关闭：状态立即离线、旧 connect 不得 paired、relay 被关闭 |
| 8 | 一般 | 待修 | relay 状态/数据事件用 `try_send` 静默丢（`relay.rs:609,698,729,742`）；Data 丢失无显式失效，GET 可能继续读旧投影 | 终态 State 必达（不丢）；Data 溢出显式标记投影失效并通知 Hub；GET 不得返回错误缓存 | 队列满丢终态 → Hub 不会永久 paired；Data 溢出 → 投影 ready=false，GET 不返回旧缓存 |
| 9 | 一般 | 待修 | 问答 UI 的选中值与自由文本互不清除（`ZcodeControlPanel.tsx:231-235,307-343`），同题同时提交两类答案 | 选选项清同题 freeText/sensitiveRef；输入自由文本清同题 picked；敏感引用同步清理 | 先选后输入/先输入后选，提交只含一类答案；敏感输入切换后旧值不提交 |
| 10 | 一般 | 待修 | 设置卡只在挂载与按钮操作后 `refreshConnection`（`ZcodeRemoteSettingsCard.tsx:147-150,177-200`），无异步状态源 | 挂载期间轮询/焦点或可见性刷新连接态；不改变按钮语义 | 外部进入 paired/offline/conflict 后卡片在有限时间内更新，重连按钮恢复可用 |
| 11 | 交付缺口 | 待修 | Hub 固定未知宿主常量（`hub.rs:47-53,688-699`），`CapabilityPolicy::for_host` 无运行时输入；`session_capabilities` 与实际执行门各用一套策略 | 采集 wire 协议/hello/schema（桥握手）＋宿主 build（本地来源）；统一策略源同时供 session 能力矩阵与执行门（含 CommandSender） | 合成宿主证据进入后，策略构造函数被实际调用；未知/升级仍全关 |
| 12 | 交付缺口 | 待修 | T8.1 缺完整生产假链：现有 loopback 只到 auth/WS/data/bridge bootstrap/ready-hello；无 CommandSender send/回执/审计经 Hub+HTTP | 在 loopback 假官方端上组装真实 `AuthService/RelayClient/codec/BridgeManager/CommandSender/Hub/HTTP`，跑通发送、回执、审计与关键失败 | 成功链 + 幂等重试/能力关闭/断线/溢出等失败场景（不手推 SSE、不改结果变量） |

### 未提交补测的处置

`codec.rs`（`encode_response_for_test`）与 `wire_e2e.rs`（ready/Initialize/hello/
订阅回环）是上一轮 T8.1 的未提交增量。先独立审查、通过后作为**测试专用**提交
（不混入任何修复项），作为 #12 的基线；不通过则退回补测作者修正，不直接丢弃。

## 2. 修复顺序与提交纪律

1. 先测试专用基线（未提交补测审查/提交），保持工作树干净。
2. #1（桥分片）→ #2（Hub 会话事件）→ #6（幂等前置）→ #7（关闭语义）→ #8（队列/
   失效）→ #11（宿主证据）→ #4/#5（能力门）→ #3/#9/#10（手机/桌面 UI）→ #12（全链）。
3. 每项：写复现测试并确认 RED → 最小实现 → GREEN → 独立审查（spec + quality）→
   修正 → 提交。共享工作树同一时刻只有一个实现子代理写入。
4. 每完成一项，在本文件表格与“进度日志”更新：提交短哈希、测试命令、审查结论。
5. 生产 `CapabilityView` 默认值、证据门、无 CLI 兜底不得因修复改变。

## 3. 进度日志

- 2026-10-27：建立本清单；核对 12 项源码根因，全部成立；工作树仍只有两个未提交
  补测；开始验证补测。
- #0 基线补测：独立审查通过（0 Critical；I-2/I-3 留 #12），按建议修正“200 门已证明”
  的注释口径、协议常量、超时与订阅断言后，作为 test-only 基线提交 `0ec5d5bc`；
  `wire_e2e` 5/0。
- #1 物理分片：RED（第二片被旧 `<=` 丢弃）→ 修复为 `<`（同值交给 assembler）→
  独立审查 0 Critical/Important（要求加强 ACK 断言）→ 首片 0 ACK、整条恰好 1 ACK；
  bridge 48/0；提交 `b94be0e0`。同 messageSeq 不同布局重放为 fail-closed（unready），
  不是二次交付；消息间交错的 messageGap 硬化留待后续。
- #2 Hub 会话事件：RED（桩返回空 / Hub 收不到 Session 事件）→ 桥返回变化视图、Hub
  发布 Session → 独立审查 0 Critical/Important（3 Minor：错误帧不发事件、单槽
  `last_session`、测试白盒点；留后续/#8）→ 2 个新用例 + bridge 49/0 + hub 21/0；
  提交 `0d36d516`。
- #6 幂等前置：RED（旧顺序 needs_auth 挡住既有回执；command 用例编译缺方法）→
  ActionStore 显式身份查询 + CommandSender 复用 lookup + Hub 代际后立即查询 →
  独立审查 0 Critical/Important（4 Minor：replay 后不复核代际、仅同代际可重放文档、
  冲突先于 needs_auth 的有限信息、默认实现需注意）→ hub 24/0、command 88/0、
  action 15/0；提交 `0b5e5dda`。
