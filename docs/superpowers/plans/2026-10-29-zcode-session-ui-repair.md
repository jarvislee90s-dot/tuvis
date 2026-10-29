# ZCode 会话状态与紧凑控制区修复计划

> **For agentic workers:** REQUIRED SUB-SKILL: 使用 superpowers:subagent-driven-development（本任务推荐，沿用用户执行方式）或 superpowers:executing-plans。步骤以复选框跟踪；任务级复审后仍须执行本文 §7 的整批设计核查，禁止以局部修复通过代替最终验收。

**Goal:** 修复同版本 ZCode 的新卡不可答、审批缺失、队列不可见及交互布局偏差，为正文腾出空间。

**Architecture:** 保留已有认证、保险箱、relay、codec、桥绑定、命令派发及动作账本。以官方会话事件建立一致的 pending／control／queue／selection 视图；GET 与 SSE 使用同一能力解析源。前端复用 Tuvis 的卡片外观与发送习惯，但 ZCode 写动作始终走官方协议。

**Tech Stack:** Tauri 2、Rust、tokio、rusqlite、React 19、TypeScript、Vitest。

**Spec:** `docs/MASTER-PLAN.md`（最高依据）；`docs/superpowers/specs/2026-10-24-zcode-unified-remote-design.md` §5；`docs/superpowers/specs/2026-10-28-zcode-capability-parity-spec.md`。本文 §1 为 2026-10-29 用户最新专项裁决，细化旧设计的 UI 与状态要求；不改宪法。

## Global Constraints

- 工作区 `/Users/jarvis/Documents/tuvis-worktree2`，执行分支 `codex/zcode-unified-remote`；写稿基线 `cb79b7cb`。实施前核实真实 BASE，不重置其他改动、不另从 main 覆盖现有实现。
- 不修改 `AGENTS.md`、`docs/MASTER-PLAN.md`；不 push、merge、tag、发版或升级 ZCode。
- 本机现场 build 为 `3.16.2.8769`；本轮先修同版本功能，未来升级兼容验证另行进行。
- 不重写 OAuth、钥匙串、relay、codec、幂等账本；不另起 app-server、不新建第二个控制连接器。
- 官方 clientId 与握手同源；手机设备身份仍负责 Tuvis 配对、动作归属和审计。
- 目标、代际、就绪、原始 optionId、执行守卫、CAS 与 Unknown 不重发等既有约束均保留。
- 实施与自动测试使用替身／回环／临时目录，不碰真实钥匙串，不自动发送消息、审批或打断。实机验证单列待验，不要求用户辅助编码。
- 日志、证据账本不得保存真实链接、SID、JWT、token、会话正文及答案；手机 DTO 只返回展示所需的会话数据，不返回凭据或上游错误原文。
- 文案中英同步。每任务严格 RED → 最小实现 → GREEN → 独立复审；确认基线后按任务提交，不 amend 已审提交。

## Review Focus

1. 页面初读无 pending，后续同代 SSE 才出现问答／审批：不刷新、不重进也能按新事件开放；R1 覆盖。
2. 三题回答过程中收到旁路 revision、控制状态变化：不丢草稿、不提前提交；R3 覆盖。
3. 运行中消息被官方排队，随后暂停／消费／移除：只显示官方状态，不把 accepted 冒充执行完成；R2 覆盖。
4. 运行目标消失、换代或输入含附件：主按钮不会误发 stop／空消息；R5 覆盖。
5. 360px、中英、深浅主题与长模型名：紧凑工具栏不遮挡正文或裁掉功能；R4、R6 覆盖。

## 1. 用户确认的范围与验收口径

| 编号 | 本轮要求                                                 | 边界                                                                          |
| ---- | -------------------------------------------------------- | ----------------------------------------------------------------------------- |
| U1   | 附件“＋”旁／下方补问号，展示既有附件存储说明             | 复用现有上传与说明，不改存储目录或接官方原生附件上传                          |
| U2   | “打开官方页面／模型／思考强度”合并为一行紧凑工具栏       | 模型与档位切换已获用户实测认可；不继续修桌面延迟显示，不改变协议实现          |
| U3   | 运行中、输入为空时主按钮为打断；有文字或就绪附件时为发送 | 有内容发送不得先打断；运行中发送按官方支持的入队语义处理，不做 Tuvis 自建队列 |
| U4   | 真实问答卡可在远程答题，蓝色、纵向、一次显示一题         | 一组多题本地暂存，最终确认后整体提交；不按 CLI 键序逐题远程发送               |
| U5   | 已存在的 Bash 等审批请求正确出现，可按既有受限选项应答   | 不保证每次 Bash 都必须弹审批；是否弹卡由官方决定                              |
| U6   | 手机显示官方待处理队列，包括暂停／已消费等变化           | 本轮只读队列展示与发送入队；不新增队列编辑、撤回、越队或恢复按钮              |
| U7   | 局部修复后必须整批对照设计／计划复审                     | 规则只约束本任务，写在本文，不写入全局 AGENTS.md                              |

**明确排除：**切换“完全访问／变更前确认”等权限模式、常驻授权、全访问选项开放、认证或保险箱变更、ZCode 升级、修改官方桌面的刷新行为。已有审批卡的单次允许／拒绝仍在本轮范围，权限模式切换与回答单个请求是不同操作。

**布局裁决：**正文优先。工具栏在 360px／100% 字号下保持一行；长模型名可省略但保留可访问全名。320px／125% 等极窄场景允许最多两行，禁止横向溢出、隐藏必要动作或截断卡内按钮。问号说明按需展开，不常驻长文。

## 2. 现场事实与尚待定位项

- `hub.rs::apply_relay_session` 沿用最后 GET 的 capabilities；新卡出现时没有按新形状重算。手机正常 ready=true SSE 又不触发纠偏 GET，是确定的事件链缺陷。
- `SessionDetail.tsx` 对非 Codex 会话继续挂旧 `QuestionCard`，包含 ZCode；与 ZCode 原生 pending 面板并存。现场看到过旧题只读卡残留。
- `SessionView` 没有 queue，`Projection` 也不保留官方排队状态；数据库现场有 admitted/queue 输入，官方桌面显示队列暂停，Tuvis 没有展示链路。
- `PendingCard` 把全部题目同时 map 出来，选项 flex-wrap；未落实既有统一卡片交互要求。
- 附件只复用了函数／条目形状，漏了 MessageComposer 已有问号入口。
- 问答禁用与旧能力矩阵一致，但图片三审批消失伴随 unknown，尚无原始 permission 帧及对应请求错误的完整归因。R0 必须先核实，不能把所有审批问题都归为旧矩阵，也不能靠全开能力消除症状。
- 模型与思考强度已由用户确认可用；桌面切出再切入会刷新，按本次范围关闭该问题，保留回归即可。

## 3. 文件与职责

| 文件                                                                                                         | 职责／预计变化                                                                                 |
| ------------------------------------------------------------------------------------------------------------ | ---------------------------------------------------------------------------------------------- |
| `src-tauri/src/services/zcode_control/{bridge,projection,pending,types}.rs`                                  | 官方形状核实、会话投影、queue DTO；不混入 UI 草稿                                              |
| `src-tauri/src/services/zcode_control/{hub,command}.rs`                                                      | 实时能力计算、已存在审批的正确映射、发送入队语义；不自建队列                                   |
| `src/lib/api/zcodeControl.ts`                                                                                | 与 Rust 同步的手机 DTO                                                                         |
| `src/mobile/{useZcodeControl,ZcodeControlPanel,SessionDetail}.tsx`                                           | 新鲜状态落地、唯一原生交互入口、紧凑控制区                                                     |
| 新建 `src/mobile/ZcodePendingCard.tsx`                                                                       | 从现有 PendingCard 提取，负责逐题本地暂存及审批展示；可复用 InteractiveCard，不复用 CLI 写动作 |
| 新建 `src/mobile/ZcodeQueuePanel.tsx`                                                                        | 官方队列只读展示，未知与空队列有区别                                                           |
| 新建 `src/mobile/zcode-primary-action.ts`                                                                    | 主按钮纯判据，避免 UI 里混入协议判断                                                           |
| `src/mobile/{InteractiveCard,MessageComposer,attachments}.tsx/ts`                                            | 仅复用已有积木；不把全套 CLI 编排搬进 ZCode                                                    |
| `tests/mobile/{ZcodeControlPanel,ZcodeControlAttachments,ZcodeSessionIntegration,zcodeControlHook}.test.tsx` | 集成、状态时序及上传旧回归                                                                     |
| 新建 `tests/mobile/{ZcodePendingCard,zcodePrimaryAction,ZcodeQueuePanel}.test.tsx`                           | 新组件／纯判据回归                                                                             |
| `src-tauri/src/services/zcode_control/wire_e2e.rs`                                                           | 宿主忠实回环，不使用全部能力硬编码 true 代替生产 resolver                                      |

## 4. 任务与执行顺序

依赖：R0 → R1 → R2 → R3 → R4 → R5 → R6。可由新子代理逐任务执行；R6 最终 reviewer 必须是新上下文，读取完整依据，不能只拿修复清单。

### R0：核实审批与队列合同，留下可证伪样本

**Files:** 上述 Rust 桥／投影／命令；本计划 §2；必要时新增 `tests/fixtures/zcode-official-bridge/session-state/` 的纯合成结构夹具。

**Interfaces:** 保留既有 `BridgeManager::on_data_outcome`、`HubBackend::session`。官方字段必须从当前安装包／固定公开源码确认，以下 Tuvis DTO 名称不等于官方 wire 字段。

- [ ] 读取安装 build、源码来源和现有合同，记录审批 pending、队列 snapshot/delta、send ACK 的实际字段与来源定位。公开源码和安装包不同的面分别记载。
- [ ] 写失败测试：`permission_snapshot_is_retained_with_original_options`、`queue_snapshot_and_delta_preserve_input_identity`；输入来自已核实结构的合成样本，断言原 interaction/option/input 身份保留，旧代际样本丢弃。不得手造“expected code”让假宿主迁就实现。
- [ ] Run：`cd src-tauri && cargo test --lib services::zcode_control::`，记录具体 RED；对于图片三 unknown，先定位到请求／解码／投影哪层，再修那个具体错误；无法重现时如实保留未定位项，不虚报关闭。
- [ ] 最小修正支持已核实形状；错误只做允许列表分类。未知形状保留安全禁用与可诊断的本地原因，不把只读正文隐藏。
- [ ] 同一命令 GREEN；单独提交 `test(zcode): pin permission and queue state contracts`（如含修复，提交信息按实际内容命名）。

**停止门：**若无法证明队列或 permission 的字段来源，不猜 RPC、不伪造真实样本；先完成其他独立任务，报告该项未闭环，最终不得称整批完成。自动测试不要求用户真实授权或操作审批。

### R1：实时能力同源，ZCode 只留官方交互入口

**Files:** `hub.rs`、`useZcodeControl.ts`、`SessionDetail.tsx`；hub inline tests、`wire_e2e.rs`、`zcodeControlHook.test.tsx`、`ZcodeSessionIntegration.test.tsx`。

**Interfaces:** `CapabilitySource = Arc<dyn Fn() -> CapabilityView + Send + Sync>` 继续复用。`apply_relay_session` 接收本帧现算的 `CapabilityView`（新增入参）；source 捕获方式与现有 `session_capabilities` 一致。现算在 Hub 状态锁外执行，发布前锁内复核 generation/Paired；debug evidence 的会话与代际限制不得绕过。

- [ ] 写 RED：`first_question_after_empty_get_is_answerable_without_refresh`、`first_permission_after_empty_get_is_answerable_without_refresh`；GET 时无卡，随后同代真实 SSE 新卡，断言无需手动 GET 能力即更新。另测两个会话交替读写，不借用最后 GET 的其他会话矩阵；ready=false 必须全关。
- [ ] 写前端 RED：ZCode 不挂旧 CLI QuestionCard/ApproveCard，不调用 `/session-question`、CLI answer 或旧 `/session-send`；Claude 等原卡仍正常。
- [ ] Run：Rust zcode_control 测试及 `pnpm exec vitest run tests/mobile/zcodeControlHook.test.tsx tests/mobile/ZcodeSessionIntegration.test.tsx`，确认失败原因对应缺陷。
- [ ] 最小实现：按已吸收的最新接口证据计算能力并发布，继续按真实 pending/control/目录判断动作资格。ZCode 在详情页只消费原生 pending；连接失败保留正文阅读和就地原因，不用旧只读题卡充当原生降级卡。
- [ ] 上述命令 GREEN，保留先前“完整模型目录经实时帧不丢失”回归；提交 `fix(zcode): publish fresh capabilities with native session state`。

### R2：投影官方队列，运行中发送保留入队语义

**Files:** Rust `types.rs`、`projection.rs`、`bridge.rs`、`command.rs`、必要的回执持久化；TS `zcodeControl.ts`、新建 `ZcodeQueuePanel.tsx`；对应 queue 单测及 wire e2e。

**Interfaces:** 新增 Tuvis `QueueView { known: bool, paused: Option<bool>, items: Vec<QueueItemView> }`；`QueueItemView { input_id: String, text: String, position: Option<u32> }`，序列化 camelCase。`SessionView.queue: QueueView`，旧载荷缺字段时前端视为未知；`paused=None` 不当成未暂停。队列具体 wire 路径由 R0 钉住，按官方身份与次序映射，不按正文哈希猜身份。

- [ ] RED：`queue_is_unknown_before_host_declaration`、`queue_snapshot_then_paused_delta_reconciles`、`promoted_input_leaves_queue_without_duplicate_message`；空队列／未知／暂停三种状态可区分，代际更替清除旧队列。
- [ ] RED：回环运行中 send → 官方接受入队 → SSE 显示同一 inputId → 桌面侧消费后消失；无额外 send 帧，手机未重发。前端刷新／重进后从宿主重建，不丢失待处理项，也不复活已消费项。
- [ ] Run：Rust zcode_control 测试；`pnpm exec vitest run tests/mobile/ZcodeQueuePanel.test.tsx`。
- [ ] 实现投影与只读紧凑队列组件：默认显示数量／暂停标志，可展开文字列表；无队列时不占常驻长段落。运行中发送沿官方入队合同；若需要 `requestedDelivery="queue"`，仅在 R0 证明当前宿主支持后按本次动作携带，不改全局 followupMode、不发送 stop。不把当前忙态本地猜测当成已入队事实。
- [ ] 若使用 ACK 的 inputAccepted/delivery 提供即时反馈，保持 accepted 与 queued／执行完成的语义区别；新增回执字段必须覆盖幂等重放持久化。可以只以宿主 queue 事件确认入队，避免无必要地扩展回执表。
- [ ] GREEN，提交 `feat(zcode): project official input queue for mobile`。

### R3：蓝色逐题卡、本地暂存、最终一次提交

**Files:** 从 `ZcodeControlPanel.tsx` 提取 `ZcodePendingCard.tsx`；复用 `InteractiveCard.tsx` 的 question tone；新建 `ZcodePendingCard.test.tsx`，更新现有面板测试。

**Interfaces:** 沿用现有 `PendingCardProps` 的 pending/sessionId/generation/capabilities/gateReason/submitting/revision/submit；导出 `ZcodePendingCard`。仍提交既有 `ControlAction::Answer`，携带同一 interaction 的完整 answers 数组；不新增 CLI 注入接口。

- [ ] RED：三题只展示当前题；选项纵向、单选／多选／自由文本语义正确；前进后退保留答案；尚未进入最终确认时零 answer POST。
- [ ] RED：确认页显示三题答案，最终提交恰一次 answer、原始题干和值准确；未答完整不发。一个 interaction 的无关 revision 更新不丢草稿、不解锁重复提交；请求被替换／撤销／换代时清理对应草稿，迟到应答不污染新卡。
- [ ] RED：现有 autoResolution 首次编辑 snooze 仍按合同执行，未受理不得提前最终提交；sensitive 值不入普通缓存；fullAccess/常驻授权仍不开放。
- [ ] Run：`pnpm exec vitest run tests/mobile/ZcodePendingCard.test.tsx tests/mobile/ZcodeControlPanel.test.tsx`。
- [ ] 最小实现：复用外观，不复用旧 QuestionCard 的逐题写终端逻辑。默认只显示一组当前交互卡；多组并存时给紧凑切组入口，不能悄悄丢掉其他请求。问答蓝色、审批沿用现有审批 tone；三个问题属于同一 interaction 时最后一次提交。不同 interaction 不拼成一个未定义 RPC。
- [ ] GREEN，提交 `fix(mobile): unify zcode question cards and staged submission`。

### R4：单行工具栏与附件说明

**Files:** `ZcodeControlPanel.tsx`、两份 locales、面板与附件测试。复用 MessageComposer 已有 `attachHint`／问号可访问文案。

**Interfaces:** 原 `openOfficial`、`handleModelChange`、`handleThoughtChange` 保持行为；三者移到同一工具栏容器 `data-testid="zcode-control-toolbar"`，不改模型切换协议。

- [ ] RED：三个控件在同一工具栏；保留当前选择、禁用原因、官方页显式打开语义；问号默认折叠、点击展示存储说明，再次点击折叠；不触发上传／发送。
- [ ] Run：`pnpm exec vitest run tests/mobile/ZcodeControlPanel.test.tsx tests/mobile/ZcodeControlAttachments.test.tsx`。
- [ ] 最小布局与文案实现；对窄屏、长模型名及字体缩放执行 §1 布局规则，不把“同一容器”测试冒充真实一行视觉验证。
- [ ] GREEN，浏览器 mock 场景检查 360/390px、浅深色、中英、100/125% 字号；导出截图供最终 reviewer 核对。提交 `fix(mobile): compact zcode toolbar and explain attachments`。

### R5：发送／打断共用主按钮

**Files:** 新建 `zcode-primary-action.ts`、`zcodePrimaryAction.test.tsx`；面板及其测试。

**Interfaces:** 导出纯函数 `deriveZcodePrimaryAction(input: { hasContent: boolean; running: boolean; sendAllowed: boolean; stopAllowed: boolean; busy: boolean }): "send" | "stop" | "disabled"`。hasContent 包括文字或就绪附件；running 与 stopAllowed 来自官方新鲜 control 和既有守卫，不能用看板颜色推导。

- [ ] 先写如下核心 RED，并增加 busy、无守卫、附件-only、旧代际集成分支：

```ts
expect(
  deriveZcodePrimaryAction({
    hasContent: false,
    running: true,
    sendAllowed: true,
    stopAllowed: true,
    busy: false,
  })
).toBe("stop");
expect(
  deriveZcodePrimaryAction({
    hasContent: true,
    running: true,
    sendAllowed: true,
    stopAllowed: true,
    busy: false,
  })
).toBe("send");
expect(
  deriveZcodePrimaryAction({
    hasContent: false,
    running: true,
    sendAllowed: true,
    stopAllowed: false,
    busy: false,
  })
).toBe("disabled");
```

- [ ] Run：`pnpm exec vitest run tests/mobile/zcodePrimaryAction.test.tsx tests/mobile/ZcodeControlPanel.test.tsx`。
- [ ] 最小实现：非忙态无内容时显示发送占位但不可提交空消息；忙态无内容使用原 stop 守卫；文字或就绪附件优先发送，有 uploading/failed 附件时沿原提示阻止发送，不能退化成 stop。删除独立常驻打断行。双击零重复动作；Unknown 不自动重发或假清草稿。
- [ ] GREEN，提交 `fix(mobile): switch zcode primary action between send and stop`。

### R6：整批验收与最终设计对照复审

**Files:** 本文检查项；更新旧设计／计划中受本批影响的条款及 `docs/user-manual/{zh,en}/08-mobile-remote.md`；新增 `docs/superpowers/reviews/2026-10-29-zcode-session-ui-repair-review.md`。

- [ ] 按 §6 全量门禁执行，记录代码 HEAD、每条命令和真实结果；失败保留用例名、首轮日志与复跑记录，不把未定位失败断言为环境原因。
- [ ] 按 §7 派新 reviewer 做整批规格核查；输入真实 BASE/HEAD、本文、原设计 §5、能力 spec、执行报告、全部差异与验证证据，不能仅传“已修复项摘要”。
- [ ] 每项 U1—U7 和 §7 的继承功能均给出代码位置＋自动证据＋实机状态；缺项回到对应任务修复。独立 reviewer 必须单独评价计划偏差，不由实施者自称“全部合理”。
- [ ] 修复 review 的阻塞项后，先 scoped re-review，再更新整批覆盖表；仅阻塞清零时给出“代码可合并、实机待验”，不得自动合并。
- [ ] 提交 `docs(zcode): record session repair coverage and final review`。

## 5. 测试必须经过的真实时序

1. 空闲无卡 GET → 发消息 → 工作中 → 新问答／审批 SSE：无需重新进入详情页即可应答。
2. 同一组 3 题 → 分题本地暂存 → 无关 revision → 返回上一题改答 → 最终确认 → 一次提交 → 宿主撤卡；全过程不走 CLI answer。
3. 运行中带文字或附件发送 → 官方队列出现 → 官方暂停 → 刷新手机仍显示暂停 → 官方消费／移除 → 手机相应收敛；不自动恢复队列。
4. 活跃回合空输入 → stop 一次 → 未知结果保留现场且不重发；目标刚好变化时零误停。
5. 上传、题卡、队列同时存在时，保持正文可读；切会话／换代不串卡、不串附件、不复活旧队列。

## 6. 门禁与实机状态

自动门禁（同一最终代码 HEAD；不以历史轮次外推）：

```bash
cd src-tauri
cargo test --locked --lib services::zcode_control::
cargo test --locked --features marker-helper
cargo test --locked --tests --features marker-helper,zcode-bridge-evidence
cargo clippy --all-targets --features marker-helper -- -D warnings
cargo clippy --all-targets --features marker-helper,zcode-bridge-evidence -- -D warnings
cargo fmt --check
cd ..
pnpm test
pnpm check
pnpm build:mobile
git diff --check
```

实机另表：模型／档位切换为“用户已认可，继承”；新卡动态出现与答题、已有 permission 应答、队列跨端展示、紧凑布局分别记“待验／通过／失败＋证据”。自动回环不能冒充 Desktop 原卡已撤或手机视觉已验。本批不升级 ZCode、不反复索要 OAuth，不把需要用户实机确认的事项提前混进编码流程。

## 7. 本任务专属的最终复审机制（不写 AGENTS.md）

采用 **任务级复审 → 修复 → 整批规格核查 → 最终结论** 四步。本节就是最终 reviewer 的验收合同，实施 Agent 必须原文引用它。

最终报告必须有以下覆盖表，禁止只列“发现 0 问题”或只复述测试数量：

| 要求                                | 设计／计划依据       | 最终代码位置       | 自动测试／回环证据 | 实机证据或待验         | 状态／偏差裁决                     |
| ----------------------------------- | -------------------- | ------------------ | ------------------ | ---------------------- | ---------------------------------- |
| U1—U7，逐行列出                     | 本文 §1、原设计 §5   | 文件:行            | 测试名与实际结果   | 不得把未跑写通过       | 满足／部分／缺失／明确排除         |
| 已有八项动作，逐项列出              | 能力 spec R1—R6      | 同上               | 同上               | 同上                   | 不以八个 enabled=true 替代真实交互 |
| 阅读／附件／配置／授权复用          | 原设计、凭据复用设计 | 本批改动或继承位置 | 相关回归           | 已有证据／待验         | 无意回归必须列问题                 |
| 目标、代际、幂等、Unknown、凭据保护 | 宪法及既有合同       | 最终执行闸         | 负例测试           | 不要求真实凭据才能判定 | 不因 UI 合并弱化守卫               |

审查责任：

- **规格覆盖：**对照设计与计划的最终整体行为，不局限于本轮 diff。明确区分“已经有实现”“本次实现”“明确不做”“仍缺失”，不把旧文档的一句范围排除掩盖本次用户新裁决。
- **状态链：**至少逐层走读 host → bridge → projection → capabilities → GET/SSE → hook → UI → command → host 确认，核查各层字段是否真正接通；用 §5 的时序验证，不能只看初始全 true 夹具。
- **交互／视觉：**对比共用 Tuvis 组件和窄屏截图；问号、单行工具栏、单主按钮、蓝色逐题卡、队列展开区都需有证据。颜色或布局没有渲染证据时标待验，不能仅凭 JSX 判“视觉完全一致”。
- **偏差：**每条计划偏差独立说明影响和理由；权限模式变更、放宽审批、重写连接器等越界必须阻塞，不能由执行者自裁后隐去。
- **结论：**先列问题、严重度和定位，再分别给“代码是否可合并”与“产品／实机是否完成”。缺失核心展示或状态链未接通是代码缺口，不能全部转为实机待验。
- **修复复核：**scoped 通过仅关闭对应问题；更新整批覆盖表后才能给最终结论。真实测试未完成不伪造，但也不要求用户先协助编码来掩盖可静态／回环发现的缺口。

完成文档自检：U1→R4；U2→R4；U3→R2/R5；U4→R1/R3；U5→R0/R1/R3；U6→R0/R2；U7→R6/§7。模型／档位已通过、权限模式切换排除均在 §1 明示。剩余审批未知已设证据门，没有将未确认推测写成既定根因。
