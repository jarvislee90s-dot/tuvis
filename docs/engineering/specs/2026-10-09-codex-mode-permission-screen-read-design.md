# codex 模式/权限两链屏读闭环 · 设计说明书

| 项 | 值 |
|---|---|
| 版本 | v1.3（2026-10-10。v1.2 + 诊断批加固四项 G1–G4 入 §4.1：composer 在场性通用闸 / hijack 对账 / 审批框账本锚 / 失败回执屏面摘要——诊断证据 `codex-mode-perm-diag-20261010` mpd- 前缀 43 份。v1.1 + 用户指令改版：切档语义改 toggle 无条件（零投递闸移除，目标=前读翻转）+ 权限四档 chips 直选 UI + picker Done 轮询升级。v1.0 + 检查轮修订：M-1 确认框 pick 死路纳入修复面、M-2 shift+tab 生产 2 记录零改动、M-3 权限三级回落（屏读→记忆→未知）、M-4 Key 路族隔离、M-5 核验窗独立常量；m-6 observed 字段/m-7 overlay 认知修正入 T4/m-8 内容集差分/m-9 夹具入库位置）。含风险预测试定案——四项风险已真机实测出边界，加固方案随边界写入 §4 |
| 状态 | 待用户终审后转实施计划 |
| 证据基线 | codex-cli **0.160.0** Windows 11 26200；风险预测试批（2026-10-09，自建会话真机取证，证据 `%USERPROFILE%\mam-probe-m6r\evidence\codex-mode-perm-20261009\` mp- 前缀 18 份 dump + 注入日志）；既有底料 `2026-10-05-codex弹窗键序复验.md`（effort 自动降档在档）、`2026-09-21-四家模式切换shift-tab实机探测.md`（T6） |
| 实施边界 | 本文档定**需求、行为、输入输出与加固边界**；函数/模块归属留实施计划 |

---

## 0. 背景与根因（系统调试取证定案）

用户实机走查（2026-10-09 截图三张）暴露两条链的屏读面缺口：

| # | 现象 | 根因（代码逐点核实） |
|---|---|---|
| 1 | 权限组卡面恒显「模式未知 请人工核对」 | `mode.rs` codex 权限组 `readback: false`（旧定案「底栏只有模式文本」在 0.160.0 复核仍成立——权限档状态栏零显示）；`parse_axis_from_screen` 对 codex×Permission 恒 None → **权限轴无任何回读源** |
| 2 | 权限切换实际成功、回执报未核验 | 回执锚 `permissions updated to` 是 0.154 时代词形；0.160.0 实际打 `• Permission selection requested: <档名>`（账本无此行） |
| 3 | 模式切换偶报「回读到档与预期不符（预期计划/实际默认）」 | 发 shift+tab 后固定窗内读状态栏比对期望——**无基线差分**，TUI 重绘慢于窗时「读慢了」与「切换未生效」不可分辨 |
| 4 | 每次操作前后必须有一次屏读确认（用户纪律） | 模式 Key 路有 before 屏读但**不作闸用**（api.rs ~7920 只记不判）；权限 picker 有「前读 overlay」但回执段无基线差分——双屏读纪律未成体系 |
| 5 | Full Access 确认框阶段远程不可达 | `run_codex_menu_pick`（mode.rs ~2532）在屏上是确认框时**一律 Err 零投递**（回归锁 `pick_refuses_when_confirm_box_on_screen`），picker「点 1 = Yes」永远失败——两阶段链第二跳是死路（**本批要修**，见 §3.2/§5） |

另两现象同根：权限记忆 `PERMISSION_TIER_MEMORY` 只在 `verified=true` 时写入（api.rs ~8254），而回执锚过期（现象 2）使 verified 恒 false → **记忆饥饿**，GET 权限组只剩「屏读（无源）∨ 记忆（饥饿）」双空 → 「模式未知」。修好 R2 锚后记忆通道自然恢复供血。

**思考强度被换的裁决（非本批缺陷）**：`~/.codex/config.toml` 的 `model_reasoning_effort = "high"` 只作用 Default 档；codex **按模式绑定默认 effort**（进 Plan 自动降 medium、切回 Default 回 high）——切档时 codex 自行发 `Model changed to … medium for Plan mode.`。在档：2026-10-05 底料「附带发现 2」。**MAM 不干预不解析 effort 值**（事件行判据只取 `for <Mode> mode.` 尾段）；固定强度属 codex 侧配置问题，范围外。

## 1. 需求（REQ）

| REQ | 需求 | 验收标准 |
|---|---|---|
| **R1** 权限回读源 | 权限组有屏读当前档（消除「模式未知」） | 切换成功后 GET `/session-mode` 权限组 current = 实际档；事件行在屏即可读出 |
| **R2** 回执锚更新 | 权限切换回执锚吃 0.160.0 词形 | 切换（含 Full Access 二阶段、重复选当前档）回执 verified=true 依据真实锚 |
| **R3** 模式切换基线差分 | shift+tab 前后双屏读 + 基线差分，区分「未生效/读慢了/切错档」 | 三种情形回执 hint 各自如实；无固定睡判死 |
| **R4** 双屏读纪律 | 每次模式/权限动作：**前读**（基线屏 + 当前档）与**后读**（基线差分轮询 + 回执锚）成对出现 | 实机走查日志每动作可见前后两次屏读记录 |
| **R5** 测试交付 | 判据词形逐字入账本（append-only）+ 解析/编排自动化测试 + 实机走查报告 | 三层齐 |

**非需求**：思考强度（effort）的解析与干预；kimi/claude/opencode 各自链路改动（判据分族，只动 codex 家族）；权限切换主链形态（数字直达 + picker 已是定案，只补屏读面）。

## 2. 屏面与键序规格（风险预测试定案，全部逐字）

### 2.1 事件行词形（0.160.0 实测定案）

```
• Permission selection requested: Read Only
• Permission selection requested: Ask for approval (non-admin sandbox)
• Permission selection requested: Approve for me
• Permission selection requested: Full Access
• Model changed to deepseek-v4.1-flash medium for Plan mode.
• Model changed to deepseek-v4.1-flash high for Default mode.
```

- bullet `•` 前缀；Permission 行**无句号**；Model 行**有句号**；effort 小写（**不解析**）；模式词 Plan/Default 首字母大写。
- **Model 行形态**：`• Model changed to <model> <effort> for <Plan|Default> mode.`——判据锁**行形**：`model changed to` 前缀 + `for plan mode`/`for default mode` 尾段（模型名/effort 是变量不参与匹配）。
- **Permission 行形态**：`• Permission selection requested: <菜单项标签去编号>`——标签用**菜单项本名全形**（档 2 是 `Ask for approval (non-admin sandbox)`，非短形）。

### 2.2 权限菜单（0.160.0 复核）

- 标题 `Update Model Permissions`（账本 TITLE 锚成立）；footer `enter select · esc back`（0.156.1 变体成立，另一变体本次未触发场景）。
- 四项 `1. Read Only` / `2. Ask for approval (non-admin sandbox)` / `3. Approve for me` / `4. Full Access`——**`(current)` 后缀只在非首位当前档显示**（当前档在首位时菜单无任何 current 标记）→ **禁止用 `(current)` 定位当前档**。
- **菜单被确认框「替换」而非叠加**（修正旧档案的 overlay 叠加认知——0.160.0 选 4 后菜单消失、确认框独占）；Cancel 回**菜单**（非主界面）。
- dump 偶发**重绘撕裂**（同一行连续两行）——解析须容忍（重复行去重或取首行）。

### 2.3 键形态（注入通道定案更新）

| 键 | 形态 | 备注 |
|---|---|---|
| shift+tab | **生产在役形态 = 2 记录**（VK_TAB down/up 双带 SHIFT_PRESSED 修饰位——`engine.rs::shift_tab_records` + `windows_console.rs` FlatKeyRecord 转换，回归锁 `shift_tab_records_carry_shift_modifier`；0.160.0 实测生效——现象 3 自证切换动作本身成功）。风险预测试用的 4 记录形态（shift↓/tab↓/tab↑/shift↑，`mp-shifttab.ps1`）是**探测脚本形态**，仅证明「SHIFT_PRESSED 修饰位是生效关键」；**生产维持 2 记录不改**（无失效证据不替换在役形态；探测脚本已补齐 probe-key2 的修饰位能力缺口即可） | 实测 shift+tab 生效（st-1..3）；生产形态零改动 |
| esc | **VK 0x1B + Scan 0x01**（VT 形态 vk=0 被 codex 丢弃——与问答面板底料一脉相承） | 菜单/确认框内 footer 明示 `esc back` 可用；主界面 esc = 中断回合**禁用** |
| 数字 1-4 | `-Char` 字符形态 | 无回车直达（既有定案） |

## 3. 系统行为规格

### 3.1 模式切换（shift+tab）——R3/R4 主修复面（v1.2 用户指令改版：零投递闸移除）

**v1.2 卡面 UI（2026-10-10 用户指令）**：codex 模式组渲染为三段 **[计划] [◀▶] [操作]**
——两个标签是**指示器**（当前档高亮，来自屏读回读的 GET current），中间 ◀▶ 按钮
点击 = 向终端发一次 shift+tab；屏读确认落在哪个档（读「Plan mode」相关内容）后
**卡面跟随屏读结果高亮**（不跟随本地发送记忆）。

```
[卡面点 ◀▶]（仅 codex 模式组——claude/opencode 的 Key 路维持现状回读，见 §5）
      │
      ▼
┌─ 前读（双源判当前档——只记录真值，不再拦截）──────────┐
│ 屏读 → 状态栏判据（Plan mode 短语 / · 状态栏形态）    │
│        ＋ 屏上最新「Model changed …for <M> mode.」行 │
│  ├ 读不到屏 → 零投递，回执 failed + 审计 failed        │
│  │   （实现取更保守口径；原稿「409 零审计」系笔误——    │
│  │    终审 P2-5 修订）                                │
│  └ 读到屏（无论判出哪档）→ **必然发键**（v1.2：        │
│     「前读==目标 → 零投递」分支按用户指令移除——卡面    │
│     current 过期时点按钮也必然动作；实机日志 11:13-15  │
│     六次零投递正是「卡面过期 + 零投递闸」的产物）      │
└────────────────────────────────────────────────┘
      │  核验预期 = 前读真值的翻转（Plan→Default / Default→Plan；
      │  前读不可判 → 取前端 target 兜底）
      ▼
┌─ 动作后核验（基线差分轮询 ≤2s，100ms/拍不动）──────────┐
│ 逐拍读屏直到 屏面 ≠ 基线（变化即验，不等固定睡）：     │
│   变化 → 回读判档：                                  │
│     ├ 状态栏=预期 ∨ 新「…for <预期> mode.」行         │
│     │   （**新于基线**——基线已有旧行不算）             │
│     │   → verified=true；刷新卡面                    │
│     └ 变了但 ≠ 预期 → verified=false                  │
│         hint「屏已切换至 <X>（预期 <Y>）」             │
│   窗尽无变化 → verified=false                         │
│         hint「切换未生效（屏面无变化）——可重试」        │
│   （回执三态：命中 / 屏已切换至 X 预期 Y / 未生效——    │
│    v1.2 起「已在目标档，无需切换」态不复存在）          │
└────────────────────────────────────────────────┘
```

- **前读的当前档双源**：状态栏在场即权威（缺席推断 Default 是既有判据）；事件行只作**动作后核验**的确认源与状态栏不可判时的兜底（见 3.3 优先级）。
- **基线差分语义**：动作前记录「屏面原文 + 屏上最新事件行集合」；动作后出现「新于基线的 `for <目标> mode.` 行」**或**「状态栏翻到目标档」才 verified。防两类误确认：旧行留存误报（§4-R1b）、系统自动切换误报（§4-R1e——plan 批准也打行，核验只认**本动作时间窗内**的新行）。
- **「视口还原揭示旧行」边界（加固判据）**：基线记录于 overlay（菜单）在屏时，overlay 关闭后正文滚回可能让**旧事件行重新入窗**——被判「新于基线」造成假 verified。防法：核验判据用「**新行内容不在基线行集合中**」（内容集差分，非纯行位差分）——同一文案旧行在基线集合里已登记，还原入窗不构成新证据；真正的新切换必然带新文案行（同文案重复切换无法与还原区分 → 该场景判据退化为「状态栏优先」，状态栏权限零显示面如实 unknown，不假 verified）。

### 3.2 权限切换（chips 直选 + picker open/pick，主链不动、屏读面补齐）

**v1.2 卡面 UI（2026-10-10 用户指令）**：四档作为**卡片标签常驻**（只读/默认/自动审批/
完全信任 ↔ ReadOnly/Default/AcceptEdits/Bypass），生效档（GET current，三级回落）
**高亮框选中**；点某档 chip = 走 switch 端点既有 Menu 编排（`/permissions` → 屏读
定位数字 → 发数字 → Full Access 确认框阶段代按）——「切换权限」picker 入口**保留
为兜底**（Menu 编排意外失败/用户想直接在终端菜单上点编号时用）。

```
[卡面点 切换权限] → open：清场（残留 overlay + composer 纯净，既有）
    → /permissions + 回车 → ≥0.5s → 前读：菜单选项表（屏上编号+标签逐字）
    → 回卡面（既有）＋ 顺带记录基线（菜单屏原文 + 事件行集合）
[卡面点选 第 N 项] → pick：
    前读闸（现有 ✓ 补强）：菜单/确认框在场确认 + 复核第 N 项屏上编号
      与标签（防重绘后编号漂移）→ 记基线
      │
      ▼
    发数字键 N（无回车，既有 ✓）
      │
      ▼
    动作后核验（基线差分轮询 ≤2s，判据=**内容集差分**防视口还原旧行）：
      新「• Permission selection requested: <目标档标签>」（新于基线内容集）
        ∨ 旧锚「permissions updated to <label>」（0.154 兼容）
        → verified=true
      Full Access 分叉（**本批修复面**：现状 pick 在确认框在屏时一律 Err
        零投递——mode.rs ~2532 拒绝分支 + 回归锁，二阶段第二跳是死路）：
        选 4 不打行、确认框替换菜单（§4-R1f 边界）
        → 出现「Enable full access?」→ 回执 {status:"confirm"}（既有二阶段）
        → 用户点 1：**pick 分派确认框阶段**——前读=确认框在屏+基线 → 发「1」
          → 有界轮询（v1.2：单拍读屏升级——100ms/拍 × MODE_SWITCH_VERIFY_
          POLL_TOTAL_MS 总窗，与模式 toggle 核验同窗；队首拍可能还是旧态，
          新「… requested: Full Access」行（新于基线内容集）出现即命中）
          → verified=true
          （mode.rs 拒绝分支改「确认框阶段可投递+基线差分核验」，只认确认框
          选项域 1/2；既有零投递回归锁改写为新语义锁）
        → 用户点 2：同路发「2」→ 同窗轮询判「回菜单 + 无新行」→ 回执
          {status:"confirm-cancelled"}（前端据此重开菜单表）；出现新行即异常如实报；
          窗尽（确认框仍在/读不到屏）→ 不装「已取消」，按不确定态交回用户
      窗尽无新行 → verified=false + 前后双读数 hint
```

### 3.3 回读双源优先级（GET current 与动作后核验共用判据表）

| 场景 | 状态栏 | 事件行 | 判定 |
|---|---|---|---|
| **模式轴** | 在场即权威 | 动作后核验的确认源；状态栏不可判时兜底 | 状态栏可判 → 采信；不可判 → 最新 `for <M> mode.` 行 |
| **权限轴** | **零显示**（0.160.0 定案） | **唯一屏读源**：最新 `Permission selection requested: <标签>` 行 → 菜单标签反查 MamMode（复用 `menu_target_label` 映射） | **双源**：事件行在屏 → 屏读优先（如实刷新，并更新记忆表）；滚出/无行 → **回落「上次切换」记忆通道**（`recall_permission_tier`——内存+KV 持久，既有通道；事件行与记忆都无 → None「模式未知」不猜） |

- 标签匹配 = 回执行标签 `starts_with` 菜单标签短形（`Ask for approval` 前缀匹配 `Ask for approval (non-admin sandbox)`——描述列无此词，无误配面，§4-R4 实证）。
- 多行并存 → **取最后一对匹配行**（最新一条 = 当前档，R3 实证模式/权限两面均成立）。
- **权限回读屏读面不具持久性**（约 30 行输出即滚出 + 状态栏零痕迹）→ 屏读是「尽力而为」面；**记忆通道（`PERMISSION_TIER_MEMORY`，verified 时写入）承担持久面**——本批修好回执锚（R2）后记忆供血恢复，两级回落 = 「屏读真值 > 记忆 > 未知」，与 kimi 底栏/记忆双源同构；前端「（上次切换）」标注口径不变。

## 4. 风险边界与加固方案（风险预测试逐项定案）

| 风险 | 边界（实测证据） | 加固方案（写入实现判据） |
|---|---|---|
| **R1a** 回执行的语义 | 开菜单**不打行**（mp-perm-menu-open） | 菜单 open 阶段不预期回执行；picker open 回执以选项表为准（现状不动） |
| **R1b** esc 取消 | **不打行**；VT esc 无效、VK 0x1B/0x01 有效（mp-perm-cancel-esc） | esc 回执预期 =「overlay 消失 + 无新行」；实现 esc 后必须用 VK 形态；判据带「无新行」校验 |
| **R1c** 选当前档 | **照打回执行**（mp-perm-reslect-current） | **回执行 = 选择事件（确认后），不是状态变化信号**——只看「行存在」会被重复选择/旧行误确认 → **核验必须带基线差分**（新于基线且标签=目标档） |
| **R1d/e** 多行并存 | 每次切换都打；多行按序并存；「最新一条 = 状态栏」成立（mp-perm-switch-2 / st-2/st-3） | 最新匹配行 = 当前档；**动作核验只认新于基线的行**（旧行留存不可作证据） |
| **R1f** Full Access 二阶段 | 选 4 **不打行**、确认框**替换**菜单（非叠加）；确认 1 后才打行；Cancel 回菜单无新行（mp-fa-×3） | 二阶段分派判据 =「菜单消失 + 确认框标题在场」（替换语义，非叠加——修正旧档案认知）；确认核验同基线差分；Cancel 预期「回菜单+无新行」 |
| **R2** 事件行滚出 | **约 30 行输出即全部滚出**；权限档状态栏零显示 → 滚出后屏上零权限痕迹（mp-scroll-×2） | 回执核验**内联在动作后立即做**（紧贴注入，不可延后补读）；GET 权限 current 尽力而为（滚出即未知如实）；hint 文案申报「终端已滚出，无法复核」 |
| **R3a** shift+tab 形态 | probe-key2 无修饰位（探测侧缺口）；生产 2 记录形态在役有效（0.160.0 切换成功自证） | **生产零改动**（2 记录维持）；probe 脚本库补修饰位形态（`mp-shifttab.ps1` 已给参考）——只影响未来探测，不影响本批实现 |
| **R3b/c** 外溢面 | `/plan` 斜杠打**同词形**事件行；plan 批准自动切出也打行（mp-slash-plan / mp-plan-exit） | 事件行判据**必须绑定动作时间窗内的新行**（基线内容集差分）——系统自动切换不得「误确认」远程动作；GET 侧事件行兜底只反映最近一次切换（含系统自动），卡面提示「终端最近切至 X」措辞不宣称远程所致 |
| **R5** 视口还原旧行 | 基线记录于 overlay 在屏时，overlay 关闭后正文滚回可使**旧事件行重新入窗**——纯行位差分会假 verified | 核验判据 = **内容集差分**（新行文案不在基线行集合）；同文案重复切换无法与还原区分 → 该面退化为状态栏优先（权限轴状态栏零显示 → 如实 unknown 不假 verified） |
| **R4** 标签匹配 | `(current)` 只在非首位显示（不可定位）；档 2 回执行打全形标签；描述列无误配面（mp-perm-×） | 禁用 `(current)` 判据；标签 = 前缀匹配 + 全形归一（映射表 2.3） |
| **附加** 重绘撕裂 | dump 偶发同行连打（读取侧瞬态） | 菜单解析容忍重复行（去重取首）；与既有「读取抖动多帧还原」同口径 |

### 4.1 诊断批加固四项（v1.3，2026-10-10 实机诊断定案，证据 `codex-mode-perm-diag-20261010` mpd- 前缀 43 份）

**实机复现**（codex 0.162.1）：「/permissions+回车」落在**审批框在场**的终端（Ask for approval 档下命令触发 `Would you like to run the following command? 1. Yes / 2. Yes, always / 3. No`）→ 文本被 overlay 吞、**回车=批准了待审批命令**（真实副作用：curl 被执行，mpd-e7 系列）→ 菜单不开 → 数字直达报「菜单未出现」+ composer 干净（补 enter 兜底不触发）。E5 另证：权限菜单在场时第二次 /permissions+回车 = 字符被吞、回车选在高亮行、菜单无痕关闭无回执行。

| # | 加固 | 判据（实测，mpd 活体 dump） | 行为 |
|---|---|---|---|
| **G1** composer 在场性通用闸 | `codex_preflight` 增第 0 步 | idle/busy 主屏 composer 光标行恒在：idle `› Ask Codex to do anything`；**busy 占位行仍在**（busy 指示行 `◦ Working (… esc to interrupt)` 的 `◦`=U+25E6 非光标标记，mpd-mpd-e4-final）；overlay（权限菜单 mpd-mpd-e4b-final / 审批框 mpd-mpd-e7c-tr111）在场时该行被替换 | composer 不在场 = 有 overlay 占位：**审批框在屏（账本锚快筛命中）→ 直接 Err 零投递**（连 esc 都不发——esc 在审批框上=拒绝待审批命令，另一极副作用，处置权交用户）；其余未知 overlay → esc（VK 形态）+ 条件等待 composer 回归（`RESIDUE_CLEAR_MAX_READS` 节奏）；窗尽 → Err 零投递「终端有待处理交互…请人工核对终端」 |
| **G2** hijack 对账 | open 流程 enter 之后窗尽失败路径 | 屏上出现审批结果新行 `you approved` / `approved to always run`（lowered contains） | 错误文案升级「权限切换未执行，且**误批准了一条待审批命令**（回车落在了审批框上）——请立即到终端核对刚才批准的命令！」；否则维持原文案。picker open 路径同款对账 |
| **G3** 审批框标题入账本 | append-only | `would you like to run the following command`（0.162.1 活体，mpd-mpd-e7c-tr111） | 新场景 `approve_command` / `slot::TITLE`；与 `approve.rs` prompt_markers 的消息层词形（`would you like to run the following` 前缀）**互引不互替**（消息层 vs 屏读层两个检测面） |
| **G4** 失败回执附屏面摘要 | 数字直达失败路径 | 三值快照 = 审批框（G3 锚）/权限菜单（`codex_overlay_kind`）/composer（G1 判据）——与闸同一实现 | 错误文案追加「（屏面摘要：审批框=在/菜单=不在/composer=不在）」，读不到屏如实写「读不到屏」——下次诊断免猜 |

**夹具**：`src-tauri/tests/fixtures/mode-perm-20261009/` 追加三份（程序化提取自 mpd FRAME dump，禁手抄）：`codex-mp-approval-box.txt`（审批框在屏）/ `codex-mp-idle-composer.txt`（idle 主屏）/ `codex-mp-busy-composer.txt`（busy 帧）。

## 5. 分工边界

- **不改**：权限切换主链形态（数字直达 + picker 两动作的交互结构）、`/permissions` open 编排（清场/纯净前置/0.5s 硬间隔均已定案）、kimi/claude/opencode 判据、**shift+tab 生产 2 记录形态**（engine.rs 在役零改动）、effort 值（不解析不干预）。
- **修改（本批修复面）**：`run_codex_menu_pick` 的确认框拒绝分支（~2532）→「确认框阶段可投递 + 基线差分核验」（M-1，§3.2）——这是权限主链的一处**判据修改**而非结构改版。
- **补齐**：账本锚（append-only）、解析器 codex 权限轴 + 模式事件行（行形判据）、switch Key 路（**仅 codex 模式组分派**——claude/opencode 的 Key 分支维持现状回读零改动，M-4）与 picker pick 路的前读基线 + 基线差分核验、GET 权限 current 通道（**屏读优先 → 记忆回落 → 未知**三级，M-3，`recall_permission_tier` 既有通道保留）、前端 ModeBar 消费（「模式未知」降级面收窄，UI 结构不变）。
- **probe 脚本库**：补 shift+tab 修饰位形态（`mp-shifttab.ps1` 同款）——只影响未来探测，不触生产。

## 6. 实施计划要点（Task 分解预告，细节留实施计划）

| Task | 内容 | 层 |
|---|---|---|
| T0 | 底料文档落档（本 spec §2/§4 全部逐字 + 证据索引；随分支入库 docs/engineering/specs/）；**TDD 夹具入库**：dump 提取的屏面行集入 `src-tauri/tests/fixtures/` 既有 e-stage2 惯例（m-9） | 文档+夹具 |
| T1 | 账本：`permission selection requested`（PERMISSION_MENU/RECEIPT，0.160.0）；Full Access 确认回执行词形（若 T0 证实与主回执行同词形则不另立行）；事件行**不进账本**（含变量、行形解析） | anchor_ledger（TDD） |
| T2 | `parse_codex_footer` 扩：模式事件行行形判据（`model changed to` 前缀 + `for plan|default mode` 尾段锁）；GET 优先级表（§3.3） | mode.rs（TDD） |
| T3 | `parse_axis_from_screen` codex×Permission 实现（最新匹配行 → menu_target_label 反查，前缀匹配+全形归一）；`readback: false→true`；重绘撕裂容忍（重复行去重）；GET 权限组「屏读→记忆→未知」三级回落接线 | mode.rs + api.rs（TDD） |
| T4 | **codex 模式组** Key 路分派改造（族隔离：`mode_switch_plan` 已分家，改造落在 dispatch 的 codex 臂——前读基线 → 注入 → 内容集差分轮询 → 回执核验；claude/opencode Key 路零改动，回归测试锁）；picker pick 确认框阶段修复（M-1：拒绝分支→确认框阶段投递，`{status:"confirm-cancelled"}` 新回执态）；既有 overlay 叠加注释与夹具认知修正（m-7：`codex_overlay_kind` 判据顺序注、dialog.rs 同源注、overlay 夹具测试）；核验窗常量**新增独立** `MODE_SWITCH_VERIFY_POLL_TOTAL_MS`（初值 2000ms，注释标注「mp-st 时延粗测 <2s，精确回填待验收」——不动既有 `MODE_READBACK_POLL_TOTAL_MS`，M-5）。**v1.2 用户指令修订**：「已在目标档」零投递回执态**移除**（点切换必然发键，核验预期=前读翻转、前读不可判取前端 target 兜底）；picker Done（确认框 Yes/Cancel 路）核验**升级为同款有界轮询**（复用 `MODE_SWITCH_VERIFY_POLL_TOTAL_MS` + `POLL_STEP_MS`） | mode.rs + api.rs（TDD） |
| T5 | wire/前端对位：switch 回执带 observed（前端自理卡面纠偏，m-6）；`confirm-cancelled` 消费；权限 current 三级回落的前端标注（「（上次切换）」口径不变）；**v1.2 用户指令**：模式组三段 [计划][◀▶][操作]（标签指示器+切换钮，无零投递闸）+ 权限组四档 chips 常驻直选（生效档高亮框，点 chip 走 switch Menu 编排）+ picker 兜底入口保留；probe 脚本库 shift+tab 形态补档（脚本目录，非生产代码） | api.ts/ModeBar 测试 + scripts（TDD） |
| T6 | 门禁（cargo test --lib 豁免基线 4 红 + clippy 0 + fmt + pnpm lint/format:check）+ 实机走查（自建会话：连切 3 次模式、四档权限各一轮、**Full Access 二阶段全链**（选 4→点 1→verified；再选 4→点 2→回菜单）、每动作前后双屏读日志留证；滚出场景一轮验证三级回落） | 收口 |

## 7. 证据索引

- **诊断批（2026-10-10，v1.3 加固四项）**：`%USERPROFILE%\mam-probe-m6r\evidence\codex-mode-perm-diag-20261010\`（mpd- 前缀 43 份 dump：baseline / menu-codepoints / e1–e7 系列；关键帧 mpd-mpd-e4-final（busy 形态）/ mpd-mpd-e4b-final（菜单占位）/ mpd-mpd-e7c-tr111（审批框在屏））；报告在 agent 会话记录。
- 风险预测试：`%USERPROFILE%\mam-probe-m6r\evidence\codex-mode-perm-20261009\`（mp- 前缀 18 份 dump：baseline / perm-menu-open / perm-cancel-esc / perm-reslect-current / perm-switch-ok / perm-switch-2 / fa-confirm-open / fa-done / fa-cancel / scroll-after / scroll-far / st-1..3 / slash-plan / plan-approve-dialog / plan-exit / 基线 tasklist）+ `mam-probe-m6r\mp-shifttab.ps1`（shift+tab 4 记录参考实现）+ key logs `probe-key2-20261009-07*`。
- 既有在档：`2026-10-05-codex弹窗键序复验.md`（effort 自动降档）、`2026-09-21-四家模式切换shift-tab实机探测.md`（T6 底栏词形）、`2026-09-22-codex权限菜单三段式屏读对账与D20时序标定.md`（0.154 菜单锚/回执锚——本批部分过期项以 §2.2/§4 为准）。
- 用户截图三张（2026-10-09）：模式未知卡面 / 权限菜单四项 / Enable full access 确认框。
