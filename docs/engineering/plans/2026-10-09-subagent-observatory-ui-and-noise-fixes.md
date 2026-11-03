# 子 Agent 观察台 · UI sheet 化与噪声修复 · 实施计划

日期：2026-10-09 ｜ 状态：待用户确认 ｜ 前置：观察台批次（2026-10-09-subagent-observatory.md，
T1–T8 已实现）+ C3 真机验收（docs/release-notes/c3-subagent-chips-acceptance-2026-10-08.md）已完成。
**设计依据（唯一权威）**：本文档「已拍板决策」节（用户 2026-10-09 三问三答裁决 + 同日
二轮修正三条款）+ 线稿 v2 `2026-10-09-subagent-observatory-ui-wireframe.html`（同目录，
浏览器打开可交互，状态 A/B/C 对应决策 2/3/7）。
事实底稿：C3 验收报告乙部分 + 本日真机验收观察（弹窗时刻表/信任库碎片化实证）。

**Goal:** ① 预览区 sheet 化——「文件 / 子 Agent」两个看板平级切换，子 Agent 清单不再混入
文件看板；② chip 点击默认左右分屏（宽屏 split / 窄屏全屏，保留自适应）；③ 修复真机验收
发现的三个既有缺陷：A 会话停止后浮窗反复弹、B 历史会话重开遇信任弹窗手机无法通过、
C 子 agent 跑动中主会话黄绿抖动弹窗。

**Tech Stack:** React 19 + TypeScript + vitest + Tailwind CSS v4；Rust（axum / serde / serde_json）。

---

## 一、现状锚点核对表（2026-10-09 快照，实施以符号检索为准）

| 锚点 | 现状 | 本计划动作 |
|---|---|---|
| `src/mobile/SessionDetail.tsx:91-97` | `PreviewState = list \| file \| subagent`（T5 三形）；两处布局分支（split ~:1211 / fullscreen ~:1292）各含 FilePanel/FilePreview/SubagentDetail 三分 | 重构为 **sheet 模型**（T1/T2） |
| `src/mobile/SessionDetail.tsx:601-621` | `openSubagent`（isWideViewport ? split : fullscreen, backToList=false）/ `openSubagentFromList` | 宽屏默认改 `split-h` / 窄屏 fullscreen（决策 2：现状 split 须改），随 sheet 模型适配 |
| `src/mobile/FilePanel.tsx:92-96,316-318` | `subagents`/`onOpenSubagent` 可选 props + 卡区宿主 | **移除**（子 Agent 清单移出文件看板，T1） |
| `src/mobile/FilePanel.tsx:128,161-173,178-193` | 筛选状态四件套：`kind`（全部/文档/图片）、`origin`（全部来源/我上传的/工具读取/工具读写）、`scope`（200/500/1000，`onScopeChange` 上抛）、`activeSearch` + 三组 chips/类型 tabs 渲染 | **筛选条重排**：控件形态改三组下拉 + 竖分隔符 + 搜索框（状态与过滤语义零变化，T1） |
| `src/mobile/SubagentList.tsx` | 独立清单组件（绿/灰点、派发序、冻结锚） | 移挂「子 Agent」sheet（T1），组件本体不动 |
| `src/mobile/SubagentDetail.tsx` | 详情组件（71f591d 含 I1 滚动修复） | 分屏右栏挂载（T2） |
| `src/hooks/useNotification.ts:169+` | `notifyCompletion`：颜色变化即弹（同色 5s 去重、绿 3s 稳定窗）；§四 开关过滤 | 增**同方向跃迁节流**（F2a）+ **打标静默**（F2b） |
| `src-tauri/src/adapter/mod.rs:154-210` | `apply_hook_event_to_session`：Stop→grace（CLI 默认 5s，`cli_grace_secs` KV）内强制黄、过期强制 Idle；PostToolUse→Clear（文件推导） | F2b 打标数据源（审查 S1 谓词）：`active_subagent_count>0` ∧ 最新 hook ∈ PostToolUse 族 ∧ < 30s TTL |
| `src-tauri/src/remote/watcher.rs:152+` | `diff_transitions`：状态变化边沿 → TransitionEvent → SSE | F2b：事件附打标字段（供手机端同规则静默） |
| `src-tauri/src/commands/notification.rs:37` | `show_notification_window`（前端唯一浮窗入口，无来源日志） | F1 插桩点（取证后修复） |
| `src-tauri/src/inject/resume.rs:128` | `build_spawn_command_windows(wt, cwd, resume)`——cwd 取自会话记录 | F3 cwd 归一注入点 |
| `~/.claude.json` projects 键 | 实证碎片：`E:/…Test2=false` 与 `e:/…Test2=true` 并存（盘符大小写两条款） | F3 匹配数据源（只读，MAM 不写该文件） |

## 二、已拍板决策（用户 2026-10-09 裁决，实施不得重议）

1. **预览区 sheet 化（用户二轮修正确认）**：预览区顶栏 = 「▤ 文件 / ◉ 子 Agent(n)」两个
   sheet 切换钮 + **分屏组件（PreviewModeSwitcher）保留在顶栏** + 关闭钮。子 Agent 清单
   **移出文件看板**，独占「子 Agent」sheet；文件专属组件（筛选条/文件列表）只在「文件」
   sheet 显示。**层级模型（2026-10-09 验收反馈终版）**：左栏 = 会话对话区，只承载会话消息 + 子 agent
   chip；右栏 = 预览区，内含两个平级 sheet，**每个 sheet 均为两层级导航、窗格内不分屏**
   （看板 → 详情/文件预览，页头返回钮回看板；文件 sheet 亦然——验收实测窗格内 46/54
   分栏过窄，已统一回退）。顶栏布局切换器现只控制预览窗格的停靠方式。
2. **分屏口径统一（用户二轮修正 + 计划审查 B2 定案）**：宽屏默认 **`split-h`（左右分屏）** /
   窄屏默认全屏——**文件预览屏与子 agent 屏都走此逻辑**。注意代码语义：`split`=上下分行
   （Rows2）、`split-h`=左右分行（Columns2）——「左右」措辞对应的是 **`split-h`**；
   `openSubagent`/`openFile` 宽屏默认 mode 由 `split` 改为 `split-h`（现状是 split，须改）。
   清单点卡沿用当前 mode 往返。左栏对话区只带 chip 不额外占用（chip 点击 = 切「子 Agent」
   sheet 并进详情）。
3. **文件 sheet 筛选条重排（用户二轮修正）**：现散 chips（追溯范围 200/500/1000 pills、
   类型 全部/文档/图片 tabs、来源 全部来源/我上传的/工具读取/工具读写 pills）重排为
   **单行三组下拉 + 分隔符 + 搜索框**：`追溯范围[200 ▾] ┃ 文件来源[全部 ▾] ┃ 文件类型
   [全部 ▾] ┃ 文件名搜索框`——三组以竖分隔符分组明示是三个筛选元素（窄屏自动换行，
   分组结构与分隔符不删）；**筛选语义零变化**（kind/origin/scope/search 四状态与叠加
   过滤逻辑不动，代码文件仅在类型=全部可见的口径保留），只改控件形态。搜索激活纪律
   （审查 S3）保留既有裁决：searchDraft/activeSearch 两态分离 + 「搜索」按钮/回车触发，
   勿改受控即时过滤。
4. **Bug C 修复 = a+b 都做**：a. 同会话同方向跃迁（**黄→绿与绿→黄两个边都算**；红/
   waiting 不节流）**节流**（基线，常量起步）；b. 后端对「子 agent 活动引发的跃迁」**打标**
   并可配置静默。打标数据源（计划审查 S1 修正）：**不存在 teammate 标记的 hook 事件**——
   可实施谓词 = `Session.active_subagent_count > 0`（claude_parser 既有产出）∧ 该会话最新
   hook 事件 ∈ PostToolUse 族 ∧ 事件年龄 < 30s TTL；与完成识别（JSONL 文本判据）**不同源
   不同信号**，共同点是「后端判定、前端只消费布尔、永不匹配文案」。开关命名
   `silence_subagent_activity_flap`（默认 "true" = 静默；`silence_` 前缀避免与
   `notify_subagent_report` 的「true=提醒」语义倒挂；手机端仅消费默认静默，开关可见面在
   桌面设置页）。
5. **Bug B 修复 = cwd 归一 + 未信任预检提醒**：spawn 前把 cwd 与 `~/.claude.json` 已有
   项目键做**大小写不敏感 + 路径分隔符归一**匹配；命中多条时**择优规则（审查 S6）= 优先
   取 `hasTrustDialogAccepted=true` 的条款**复用其精确 casing（实证场景恰是双条并存一真
   一假——命中 false 条款则归一白做）；全 false / 无命中 → 原样 + 预检提醒。提醒**投递面
   （审查 S7）按发起侧**：远程（手机）发起的重开 → 手机端 surface（resume 回执字段/会话页
   提示）；桌面发起 → 桌面浮窗。
6. **Bug A（停止后浮窗反复弹）根因未闭合**：本批先**插桩取证**（Phase 1），拿到生产者
   证据后按证据修复；若根因确认为状态抖动，F2a 节流即修复主体。插桩判别点（审查 N6）：
   Rust `show_notification_window` 入口、前端 `notifyCompletion`、系统 toast 降级路径——
   若浮窗持续而全部插桩点静默，即排除自定义浮窗路径、直指系统 toast。**复现须在 dev
   构建**（release 可能剥 console.debug）。
7. **不做**：不动桌面看板徽标（issue #134）；不向子 agent 发消息；不改 spec/plan 既有
   两份文档；F1 取证插桩为临时日志，定位后移除。

## 三、任务分解（TDD，每任务一 commit、门禁全绿）

### T1 预览区 sheet 化（骨架）

**Files:** `src/mobile/SessionDetail.tsx`、`src/mobile/FilePanel.tsx`（去 subagents props/卡区 +
  筛选条重排）、`src/mobile/SubagentList.tsx`（不变或微调）、`tests/mobile/SessionDetail.test.tsx`、
  `tests/mobile/SubagentList.test.tsx`、`tests/mobile/SubagentDetail.test.tsx`（props 适配）、
  `tests/mobile/FilePanel.test.tsx`（审查 S4：筛选条重排的全量适配）。

- **状态模型**：`PreviewState` 收敛为
  `{ sheet: "files" | "subagents"; open: { kind: "none" } | { kind: "file"; path } | { kind: "subagent"; id }; mode; backToList }`
  （等价重构：原 list/file/subagent 三形 → sheet + sheet 内选中态；文件预览 = files sheet +
  open.file；子 agent 详情 = subagents sheet + open.subagent）。
- **顶栏**：sheet 两钮（「子 Agent」钮带运行数徽标 `子 Agent (n)`，n=running 数；无子 agent
  名单时该钮不渲染=决策 7）+ PreviewModeSwitcher + 关闭。
- **布局**：split = 左栏 sheet 看板（FilePanel 或 SubagentList）+ 右栏内容（文件预览 /
  SubagentDetail / 空态「选择左侧项目查看」）；fullscreen = 单栏整宽（看板与内容互斥切换，
  既有互斥语义保留）；split-h 同构。
- **子 Agent 详情分屏**：subagents sheet + open.subagent → 左=清单（选中卡高亮）、右=SubagentDetail
  （T6 组件复用；**审查 S5 唯一实例裁决**：PreviewModeSwitcher 与关闭钮**上收顶栏唯一
  实例**，SubagentDetail/FilePreview/FilePanel 内部的切换器/关闭钮删除或不再渲染——
  SubagentDetail 页头仅保留 状态点/名字/back 钮，props 相应精简）。
- **文件 sheet 筛选条重排（决策 6）**：FilePanel 的筛选 UI 重构为单行三组——
  `追溯范围[select 200/500/1000] ┃ 文件来源[select 全部/我上传的/工具读取/工具读写] ┃
  文件类型[select 全部/文档/图片] ┃ 文件名搜索框`，组间竖分隔符；原类型 tabs 与来源/scope
  chips 行删除。`kind/origin/scope/search` 四状态与叠加过滤逻辑**零变化**（同一批 useState
  换控件形态），「代码文件仅在类型=全部可见」口径保留。**搜索激活纪律（审查 S3）保留
  既有裁决**：searchDraft/activeSearch 两态分离、点「搜索」按钮或回车才执行（新单行含
  显式「搜索」按钮），勿改成受控即时过滤。测试：`tests/mobile/FilePanel.test.tsx` 全量
  适配（36 用例的 file-chip-*/file-scope-*/file-origin-* testId 改下拉形态断言——审查
  S4）+ 结构断言（三组 select 存在、默认值 200/全部/全部、组间分隔符）+ 既有过滤行为
  用例语义不变。
- **回归红线**：SessionDetail 既有消息渲染/折叠/返回语义测试全绿；SubagentList 四用例语义不变
  （仅宿主变化）；SubagentDetail 五用例 props 适配（onBack 语义随 sheet 模型）。
- 门禁：`pnpm vitest run tests/mobile && pnpm build:mobile && pnpm format:check && pnpm lint`。
- commit：`refactor(subagent-observatory): 预览区 sheet 化——文件/子 Agent 两看板平级`
  （git add 含 tests/mobile/FilePanel.test.tsx）

### T2 chip 直达与详情分屏收口

- chip 点击 → 切 subagents sheet + open.subagent + mode=自适应（**宽 `split-h` / 窄
  fullscreen**——`openSubagent` 现默认 split，依决策 2 改 `split-h`）；文件 sheet 点击文件
  → file 预览沿用既有（右侧栏/全屏）。
- 分屏下右栏内容互斥：subagents sheet 右栏只显 SubagentDetail；files sheet 右栏只显
  FilePreview。返回钮（backToList）= 清 open 回看板。
- 门禁同 T1 + `npx tsc --noEmit`。
- commit：`feat(subagent-observatory): chip 点击直达分屏详情与文件预览布局收口`

### T3 F3：重开信任归一 + 未信任预检提醒

**Files:** `src-tauri/src/inject/resume.rs`（spawn cwd 归一）、`src-tauri/src/monitor/claude_config.rs`
（新，只读 `~/.claude.json` projects 键的小工具：`trusted_casing_for(home, cwd) -> Option<String>`、
`is_trusted(home, cwd) -> Option<bool>`——None=条目不存在）、`src-tauri/src/commands/notification.rs`
或 resume 完成回调（预检提醒）。

- **归一**：spawn 前对 cwd 做大小写不敏感 **+ 路径分隔符归一** 匹配 → 命中多条时**优先取
  `hasTrustDialogAccepted=true` 条款**复用其精确 casing 作为 spawn cwd（已信任目录不再触发
  信任弹窗；实证场景恰是 `E:`=false / `e:`=true 并存——命中 false 条款则归一白做）；全
  false / 未命中 → 原样（全新目录的首次信任属正常流程）。
- **预检提醒（投递面按发起侧，审查 S7）**：命中条目且 `hasTrustDialogAccepted != true` →
  远程（手机）发起的重开 → 手机端 surface（resume 回执附 `trustPromptExpected: true` 字段 /
  会话页提示条）；桌面发起 → 桌面浮窗。文案：「重开的会话所在目录未做信任确认，请在主机
  终端应答信任提示，否则会话将挂起」。
- **测试**：tempdir 假 `~/.claude.json`（**双 casing 一真一假并存**——实证形态，锁「择优
  取真条款」方向）；未信任 → is_trusted=false 锁提醒触发条件。MAM **只读** `~/.claude.json`，
  永不写。
- 门禁：`cargo test --lib -j 2`（零新增失败）+ clippy + fmt；涉 spawn 参数改动需过相关 inject 测试。
- commit：`fix(subagent-observatory): 重开 cwd 信任归一与未信任预检提醒（终审发现 B）`

### T4 F2：跃迁节流 + 子 agent 活动打标静默

**Files:** `src/hooks/useNotification.ts`（F2a 节流 + F2b 消费）、**`src/mobile/Board.tsx`
（F2b/F2a 手机端消费面——审查 B1：pushBanner/提示音/振动入口按同规则静默与节流）**、
`src-tauri/src/adapter/mod.rs`（F2b 打标：`apply_hook_event_to_session` 执行点——
`active_subagent_count > 0` ∧ 最新 hook 事件 ∈ PostToolUse 族 ∧ < TTL → Session 布尔
`flap_from_subagent_activity`，camelCase wire）+ 横切补桩（编译器驱动）、
`src-tauri/src/remote/watcher.rs`（TransitionEvent 附该字段）、`src/types/session.ts`、
`tests/mobile/` 手机端静默/节流用例。

- **F2a 节流**（基线）：`notifyCompletion` 色变弹窗增**同会话同方向节流**——**黄→绿与
  绿→黄两个边** 60s 内不重复弹（常量 `SAME_DIRECTION_NOTIFY_MS`；红/waiting 不节流——
  等待提醒不延迟）。与既有 5s 同色去重叠加。**手机端 Board.tsx 的横幅/提示音/振动入口
  消费同一节流字段**（审查 B1：TransitionEvent 携带节流语义或前端按同规则自算）。
- **F2b 打标静默**：`Session.flapFromSubagentActivity=true`（判定谓词见决策 4：活跃子
  agent 在场 ∧ 最新 hook ∈ PostToolUse 族 ∧ < TTL）→ 颜色跃迁弹窗默认静默；设置页新增
  进阶开关 `silence_subagent_activity_flap`（默认 "true"=静默）——与 §四 开关并排，
  i18n zh/en 同步；**手机端 Board.tsx 横幅/振动同样过该静默门**（提示音按目标色=绿即响
  的既有口径改为先过静默门）。
- **测试**：节流用例（60s 内两次黄→绿只弹一次）；打标用例（flap=true 静默 / false 照常）；
  既有 notifyOnce/greenStableWindow/subagentReportFilter 回归。
- 门禁：cargo test --lib 零新增失败 + clippy + fmt + `pnpm test` + build + format + lint + check:i18n。
- commit：`feat(subagent-observatory): 跃迁节流与子 agent 活动打标静默（终审发现 C）`

### T5 F1：停止后浮窗反复弹——插桩取证与修复（两段式）

- **段 1 取证（Phase 1 纪律，不预设修复）**：
  - `commands/notification.rs::show_notification_window` 入口 `log::info!`（payload 摘要 + 时间）；
  - `useNotification.notifyCompletion` 触发点 `console.debug`（session id / prev color / curr color）；
  - 浮窗前端（#/notification）挂载/收到 payload 时 `console.debug`；
  - 用户复现（跑一轮 teammate spawn→完成，或直接观察停止后的 2 分钟窗口）→ 收集三方时间线 →
    定位生产者与触发条件（假设 H1 store 双源分歧 / H2 浮窗重复 show / H3 时间相对输入）。
- **段 2 修复**：按证据。预期两类：(a) 若为前端 store 状态抖动 → F2a 节流覆盖 + 按根因收口
  （如 store 合并去重）；(b) 若为浮窗重复 show → show 幂等/去重。修复带回归测试。
- 门禁：同 T4；取证日志定位后**移除插桩**（或降为 debug 级保留在观测面）。
- commit：`fix(subagent-observatory): 停止后浮窗反复弹——根因定位与修复（终审发现 A）`
  （若拆取证/修复两 commit 以实际为准，message 注明）。

### T6 收口

- 全量门禁：`cd src-tauri && cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --check`；
  `pnpm test && pnpm build:mobile && pnpm build && pnpm format:check && pnpm lint && pnpm check:i18n && npx tsc --noEmit`。
- 真机验收清单（需用户配合，参照 C3 模式执行方只读取证）：
  1. sheet 化：文件/子 Agent 两 sheet 切换、文件工具条只在文件 sheet、子 Agent 清单全宽；
  2. chip 点击（宽屏）默认左右分屏：左看板右详情、逐步输出；清单点卡往返；窄屏全屏；
  3. Bug C：teammate 跑动期间黄绿抖动不再逐次弹窗（60s 节流 + 打标静默）；真正完成提醒保留；
  4. Bug B：关闭 Test2 终端重开会话——不再出现信任死等（或出现时手机收到预检提醒）；
  5. Bug A：会话停止后 2 分钟窗口内同内容浮窗不再重复弹（以 T5 取证结论为准）；
  6. 回归：经典通知（主会话自身动作）照常；§四 开关行为不变。

## 四、门禁（每 commit 全绿；命令逐段执行）

```bash
cd src-tauri && cargo test --lib -j 2 && cargo clippy --all-targets -- -D warnings && cargo fmt --check
# 已知基线：4 个环境性失败（database::connection ×2 / tailscale::probe / usage::project::realpath）不算新增
pnpm vitest run tests/mobile && pnpm test && pnpm build:mobile && pnpm build
pnpm format:check && pnpm lint && pnpm check:i18n && npx tsc --noEmit
```

## 五、不做（本批范围红线）

- 不向子 agent 发消息/操控（注入域另立设计）。
- 不动桌面看板徽标（issue #134）。
- 不修改既有 spec/plan 两份文档（本计划为新文档）。
- 不清理 `~/.claude.json` 碎片条目（claude 侧用户数据，MAM 只读；手动清理指引写验收报告）。
- Bug A 修复不做无证据的猜测性改动（systematic-debugging 纪律：取证先行）。
- 观察台既有非阻断携带项（T4/T5/T6 审查 Minor）不并入，留独立小批次。

## 六、已知边界（实施不修，如实申报）

1. F2a 节流可能延迟「子 agent 完成」绿提醒与「恢复工作」黄提醒各最多 60s（同方向去重
   代价，两个边都节流）；红/waiting 不节流。
2. F2b 打标是启发式（活跃子 agent 在场 ∧ 最新 hook ∈ PostToolUse 族 ∧ < TTL），**两个方向
   都会偏差**：子 agent 活动停止超 TTL 后的状态残留跃迁会漏标（静默失效，退回逐次弹）；
   TTL 窗口内的非子 agent 原因跃迁会误标（被静默吞掉）——窗口即 30s，量级如实申报；
   与完成识别同一取证纪律（不猜，随判据层再取证）。
3. F3 归一只覆盖 MAM spawn 路径；用户在终端手动 `cd e:/...` 启动 claude 仍可能造成新碎片
   （claude 侧行为，MAM 管不到）；碎片条目可手动清理（删除 ~/.claude.json 中 false 条目）。
4. sheet 化后「返回」语义：subagents sheet 内详情返回 = 清空 open 回清单；跨 sheet（files↔
   subagents）不互返。
5. Bug A 在 T5 取证前，9:35/9:37 类重复弹窗可能继续出现（观测面，不影响功能）。

## 七、真机验收清单

见 T6 门禁后清单（§三 T6）；执行模式照 C3：用户驱动 teammate 会话 + 执行方只读取证 +
dev 控制台 `subagents … running / total` 日志对账。

---

## 自审记录

- **spec 覆盖**：用户三裁决 → 决策 1/3/4；sheet 化线稿 → T1/T2；发现项 A/B/C → T5/T4/T3。
- **占位符**：F1 修复段按证据定案（取证先行是纪律不是 TBD——取证步骤本身可执行可验收）；
  F2b 打标启发式已显式申报（边界 2）。
- **类型一致性**：`PreviewState` 新模型 ↔ 顶栏 tabs/布局分支；`TransitionEvent.flap_from_subagent_activity`
  ↔ TS Session 同名字段（camelCase）；`SubagentMessageSourceFn`/`SubagentSourceFn` 不动。
- **与观察台批次关系**：全部为增量（sheet 化重构 T5/T6 产物 + 三个既有域缺陷修复）；
  观察台端点契约（/session-subagents、/session-subagent-messages）本批零改动。
