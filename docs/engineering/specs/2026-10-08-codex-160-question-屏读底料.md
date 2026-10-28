# codex 0.160.0 问答面板屏读底料（Task 1 取证批）

> 日期：2026-10-09（探测会话于 00:20 启动、00:38 清场，档名沿用任务书 runid `20261009`）
> 工具：codex-cli 0.160.0（真机 `~/.codex`，真实 CODEX_HOME，接受会话进 history）
> 宿主：conhost（launch-session.ps1 -HostKind conhost -Cli codex）
> 模型：deepseek-v4.1-flash（Plan mode 下降为 medium）
> 探测项目目录：`C:\Users\bunny\AppData\Local\Temp\probe-codex-q-20261008`
> 证据根：`C:\Users\bunny\mam-probe-m6r\evidence\`
> 本档性质：**四闸门①活体取证底料**——只登记实测到的形态，给 Task 2 夹具生成器供料；不含任何解析器判据改动。

## 0. 头部信息

| 项 | 值 |
|---|---|
| run-id | `20261009`（会话档 `conhost-codex-20261009-002018.txt`） |
| HOST_PID | 39356（conhost，已清场） |
| CMD_PID | 45388（cmd.exe，已清场；**截图须用此 PID**，node 无可见主窗口） |
| TARGET_PID | 29424（node.exe = codex TUI，注入与 `MAM_PROBE_PID` dump 目标，已清场） |
| codex 版本 | codex-cli 0.160.0 |
| 清场确认 | `Stop-Process -Id 39356,45388,29424 -Force` 后 tasklist 复核三 PID 均无匹配；用户自有 codex.exe（33752 / 28768，启动前基线快照）原样健在 |
| 首启信任框 | 未出现（该 ProjDir 曾被本系列探测使用过，信任已持久化），baseline 即主界面 |
| 干扰因素 | 屏面顶部有升级提示框（0.160.0 -> 0.160.1），不影响问答面板形态 |

### 方法备注（影响解读的三件事）

1. **dump 管道**：`MAM_PROBE_PID=<TARGET_PID> cargo test --lib kimi_screen_live_dump -- --ignored --nocapture`，输出 `FIXTURE|NN|<行文本>`。个别帧出现 UTF-8 解码错位（中文行乱码/字符交叠，如 `鈥`、`•uWhichncache?answered`），**重跑一次 dump 即恢复正常**——是读取侧瞬态，不是 TUI 渲染漂移。抄录逐字转写时以正常帧为准，乱码帧只作「管道已知瞬态」登记。
2. **MSYS 路径转换事故（已纠正）**：Git Bash 里直接 `-Text '/plan'` 被改写成 `D:/Program Files/Git/plan` 发给了模型（模型当普通消息回了话，见 baseline 后第一轮回复）。加 `MSYS_NO_PATHCONV=1` 重发后 Plan mode 正常生效。**后续探测脚本经 Git Bash 调用时一律带 `MSYS_NO_PATHCONV=1`**。
3. **footer 逐字抄录的抖动**：多帧 footer 同一段落存在字符交叠（如 `←/→ tnavigate questions`、`esc tosinterrupt`、`←/ to navigate questions | escsto interrupt`），对照多帧可还原真实词形为 `←/→ to navigate questions` / `esc to interrupt`。任务书「保留原始空格/前缀」的要求以各证据文件原文为准，本文转写段标注「（多帧还原）」处即属此类。

## 1. 定案总表（对照设计 §6 A–H + §2.3 键序）

> 「定案」= 本轮实测确认；「推翻/不符」= 实测与设计预期不一致，**以实测为准**；「不可达」= 尝试过但没拿到。

| # | 设计条目 | 结论 | 一句话实测 |
|---|---|---|---|
| A | 数字 = 选中 + 自动推进 | **定案（符合 §2.3）** | 未答题上 `-Char '1'`：选中第 1 项 + 题号自动跳下一题 + 未答计数 −1（S1: `Question 1/2 (2 unanswered)` → `Question 2/2 (1 unanswered)`） |
| B | `›` 语义 = 焦点行（非选中） | **定案** | 焦点与选中可分离：焦点在未选中项时空格选中它，`›` 跟焦点走，选中态在属性层加亮（见 §4 属性层登记） |
| C | 单选互斥原子性 | **定案** | 空格选中新项即替换旧选中（Q1 Postgres → 改选 SQLite 后 summary 只报 `answer: SQLite`）；未见多选共存帧 |
| D | notes 全链 | **部分定案 + 部分不符** | tab 在**任意行**（含非 Other 行）直接可用并顺带选中焦点行；notes 态无可见输入框行；notes 文本未进 summary answer 行（见 D-E 详述） |
| E | 长 label 折行 | **定案（形态修正）** | 63 字符 label **不折行**——整行加宽，desc 列右移，屏幕宽度（120 列）装得下就单行放下 |
| F | 首帧字形 `?` vs `›` | **定案（? 形不可达）** | 200ms 连拍抓到的第一帧即 `›` 高亮形，未见 `?` 形态帧 |
| G | 滚回残留 | **定案（不可见=无）** | 交卷后面板从可见窗清除，只剩 summary 行；read_screen_window 只读可见窗，滚回区不可见（如实登记） |
| H | 确认屏活体 | **不可达（未复现）** | 末题有未答题时 `-EnterVK` 直接交卷（Q3 answer 记为焦点项），`Submit with unanswered questions?` 确认屏未出现 |

### §2.3 关键键序实测形态

| 键 | 注入形态 | 实测效果 | 备注 |
|---|---|---|---|
| 数字 `1` | `-Char '1'`（字符形态） | 选中该序号项 + 末题上直接交卷 | 有效；未答题上自动推进 |
| 空格 | `-Char ' '`（字符形态） | 选中焦点行 / **再次按下 = 反选**（未答计数 +1，见 §3 #5） | toggle 语义实证 |
| 回车 | `-EnterVK`（VK_RETURN+扫描码，首选形态） | 提交整卷（已答状态）/ notes 态提交焦点项 | 有效 |
| 回车对照 | `-VtName enter`（纯 `\r`） | 本轮未单独对照（EnterVK 全程可靠） | — |
| tab | `-Char "$(printf '\t')"` | 全局键：任意行直开 notes 态 + 顺带选中焦点行 | 两次独立复现 |
| esc | `-VtName esc`（孤立 0x1B）/ `-Vk 0x1B -Scan 0x01` | **在 notes 态均未取消输入**；`-Vk 0x1B -Scan 0x01` 实测**中断了整个回答回合**（`■ Conversation interrupted - use /feedback if something went wrong`） | 「esc 在此面板不可达（取消语义）」+ 附带发现强中断语义 |
| 方向键 | `-VtName down/up/left/right`（VT 序列 ESC[B/A/C/D） | **全部无效**（屏面零变化，written 正常） | VT 序列族被 codex TUI 免疫（与 M6R 批 crossterm 孤立 ESC 免疫一脉相承） |
| 方向键 | `-Vk 0x25/0x27 + 扫描码`（VK_LEFT/VK_RIGHT） | 无切题效果（S3 单题上恰好也是预期无效；S2 多题上用真 VK_LEFT 亦未切题——见 §3 S2 备注） | 真 VK 箭头在面板上也未观察到切题 |
| **vim 键位（新发现）** | `-Vk 0x48/0x4A/0x4B/0x4C + 扫描码`（h/j/k/l 字符） | **j/k = 选项上下走位；h/l = 上一题/下一题** | codex 0.160.0 问答面板的走位主通道；`-Char 'j'/'k'` 字符形态同样有效 |

> **⚠ 本节对 Task 2 的最重要输出**：codex 0.160.0 面板内导航不能依赖 VT 箭头序列；可用通道 = 数字键 + 空格 + tab + EnterVK + vim h/j/k/l。设计 §2.3 的「数字/空格/回车」三定案全部实测成立，无停机条件触发。

## 2. S1 两题场景（label 逐字转写）

提示词：`Use the request_user_input tool now: ask me exactly 2 questions. Q1 single-select 'Which DB?' with options Postgres, SQLite. Q2 single-select 'Which cache?' with options Redis, Memcached, None needed. Do nothing else.`

### s1-q1（证据 `codex-q-20261009-s1-q1.txt` 屏行 21–29）

```
  Question 1/2 (2 unanswered)
  Which DB?

  › 1. Postgres           使用 PostgreSQL 作为数据库。
    2. SQLite             使用 SQLite 作为数据库。
    3. None of the above  Optionally, add details in notes (tab)

  tab to add notes | enter to submit answer | ←/→ to navigate questions | esc to interrupt
```

### A/#1 数字推进（`codex-q-20261009-s1-q1-after-digit.txt`）

按 `-Char '1'` 后屏行 20–29：

```
  Question 2/2 (1 unanswered)
  Which cache?

  › 1. Redis              使用 Redis 作为缓存。
    2. Memcached          使用 Memcached 作为缓存。
    3. None needed        不需要缓存。
    4. None of the above  Optionally, add details in notes (tab)

  tab to add notes | enter to submit all | ←/→ tnavigate questions | esc tosinterrupt
```

（footer 后段为读取抖动帧，多帧还原 = `←/→ to navigate questions | esc to interrupt`）
**定案点**：题号头 `Question 2/2 (1 unanswered)`——数字 = 选中 + 自动推进 + 计数 −1，三件事一次发生；**footer 从 `enter to submit answer` 变为 `enter to submit all`**（仍有未答题）。

### #5 已答改答与空格反选（`codex-q-20261009-s1-q2-focus-memcached.txt` → `-s1-q2-select-space.txt` → `-s1-back-to-q1-h.txt` → `-s1-q1-walk-sqlite.txt` → `-s1-q1-unanswered.txt` → `-s1-q1-reselect-sqlite.txt`）

1. Q2 上 j 走位到 Memcached（焦点 `›` 移动，选中不变）；
2. 空格选中 Memcached → 题号头变 **`Question 2/2`（计数段消失）**；
3. h 切回 Q1 → **焦点落已选项 Postgres**（`› 1. Postgres`，`Question 1/2` 无计数段）；
4. j 走到 SQLite → 空格 → **计数段重现**：`Question 1/2 (1 unanswered)`（`-s1-q1-unanswered.txt`）——**空格在已答题上把旧选中反选掉了**（焦点不在已选项时，空格语义 = 选中焦点项并清掉其它），**计数 +1 实证**；
5. 再空格选回 SQLite → 计数段再次消失（`-s1-q1-reselect-sqlite.txt`，属性层确认 SQLite 行加亮）。

**定案点（B/C 补强）**：未答计数是「已答题数」的真实计数器，随选中态实时增减；`›` 是焦点不是选中。

**焦点消歧补注（2026-10-09 验收批 b5 对照，评审 P1-2 随批调和）**：本节步骤 4 的空格打在**非已选项**（焦点 j 走到 SQLite）——反选实证成立。验收批（证据 `evidence\codex-q-acceptance-20261009\b5-space-toggle-probe.txt`，key log `probe-key2-20261009-062525`）在**已选项本身**上按空格（h 回 Q1 焦点落 Postgres 已选项）→ dump `Question 1/2` 计数段**未重现**。两批合读的消歧：空格的 toggle 语义在「已选项 vs 非已选项」两个焦点位置上行为可能不同（已选项上再按 = 抵消未落账 / 非已选项上 = 旧选中被清+计数 +1），**但两种形态下单次空格都不是「换选到焦点项」的原子替换**——底料 dump（步骤 4）是「空格 ≠ 原子换选」的直接证据。编排据此按「已答 ∧ 焦点≠目标 → 零键中止」实现（`run_codex_select_final`，评审 P1-2），两种解读下都是安全侧；「已选项上空格」的精确语义登记为版本复验触发点（见 §10 偏差清单补记）。

### #2 归零词形（`codex-q-20261009-s1-q2-zero.txt`，l 键切回 Q2）

```
  Question 2/2
  Which cache?

    1. Redis              使用 Redis 作为缓存。
  › 2. Memcached          使用 Memcached 作为缓存。
    3. None needed        不需要缓存。
    4. None of the above  Optionally, add details in notes (tab)
```

**定案点**：全答完时题号头**只剩 `Question 2/2`，`(N unanswered)` 计数段整个消失**——不是变成 `(0 unanswered)`。与设计 §2.3 归零词形预期一致。

### S1 摘要屏（`codex-q-20261009-s1-summary-recap.txt`）

末题 `-EnterVK` 后：

```
• Questions 2/2 answered
  • Which DB?
    answer: SQLite
  • Which cache?
    answer: Memcached
```

模型随后自己复述（`• 已回答：` / `• DB：SQLite` …）。摘要头逐字 = **`Questions 2/2 answered`**，answer 行逐字 = **`answer: <选项文本>`**（`answer:` 后一个空格）。

## 3. S2 三题场景（环绕 + 确认屏）

提示词：三题（DB / cache / language: Rust, Go, Python）。

### s2-q1（`codex-q-20261009-s2-q1.txt` 屏行 21–29）

```
  Question 1/3 (3 unanswered)
  Which DB?

  › 1. Postgres           使用 PostgreSQL 作为数据库。
    2. SQLite             使用 SQLite 作为数据库。
    3. None of the above  Optionally, add details in notes (tab)
  deepseek-v4.1-flash medium · ~\AppData\Local\Temp\probe-codex-q-20261008 · 看 D:/Program Files/Git/plan  Plan mode
  tab to add notes | enter to submit answer | ←/→ to navigate questions | esc to interrupt
```

**注意**：状态栏行插在选项区和 footer 之间（屏高富余时 codex 把状态行渲染进面板下方空隙）——Task 2 的夹具要包含这一变体。

### #3 环绕（`-s2-wrap-down.txt` / `-s2-wrap-up2.txt`）

- Q1 焦点在末项 `None of the above` 再按 j → **回首项** `› 1. Postgres`（下环绕）；
- Q1 焦点在首项按 k → **到末项** `› 3. None of the above`（上环绕）。
**定案**：选项列表循环导航，双向环绕成立。

### Q1→Q2→Q3 流转

Q1 空格选 Postgres（j 回首项 + 空格；**注意这次没有自动推进**——空格选中停留在原题，只有数字键才推进）→ l 切 Q2 → `-Char '1'` 选 Redis **自动推进到 Q3**（`codex-q-20261009-s2-q3.txt`：`Question 3/3 (1 unanswered)`）。

> 真键盘 VK_LEFT 复验：Q2 上发 `-Vk 0x25 -Scan 0x4B` 后焦点停在 Memcached 行未切题。此帧与「h/l 才是切题键」一致：真 VK 箭头对 codex 面板同样无效（详见 §1 键序表）。

### #11/H 确认屏（`codex-q-20261009-s2-confirm.txt`）

Q3 留空，末题直接 `-EnterVK`，预期弹 `Submit with unanswered questions?` 确认屏 → **实测直接交卷**，dump 抓到的是摘要屏：

```
• Questions 3/3 answered
  • Which DB?
    answer: Postgres
  • Which cache?
    answer: Redis
  • Which language?
    answer: Rust
```

（该 dump 帧同时叠了上一轮 S1 尾巴，见文件原文；乱码已按读取瞬态处理）
延迟 8s 复查仍无确认屏。**结论：确认屏在 0.160.0 + 此模型组合下未复现，EnterVK 在末题含未答题时 = 无提示交卷（Q3 answer 记为当时焦点项 Rust）**。设计 §6-H 预期的确认屏词形本轮**拿不到活体**，Task 2 不得凭档案造该夹具；esc 处置链因确认屏未出现而未消耗。

### S2 摘要（`codex-q-20261009-s2-summary-recap.txt`）

```
• Questions 3/3 answered
  • Which DB?      answer: Postgres
  • Which cache?   answer: Redis
  • Which language? answer: Rust
```

## 4. S3 单题场景

提示词：单题 `Which DB?`（Postgres/SQLite）。

### s3-single（`codex-q-20261009-s3-single.txt` 屏行 21–29）

```
  Question 1/1 (1 unanswered)
  Which DB?

  › 1. Postgres           使用 PostgreSQL 作为数据库。
    2. SQLite             使用 SQLite 作为数据库。
    3. None of the above  Optionally, add details in notes (tab)
  deepseek-v4.1-flash medium · ~\AppData\Local\Temp\probe-codex-q-20261008 · 看 D:/Program Files/Git/plan  Plan mode
  tab to add notes | enter to submit answer | esc to interrupt
```

**定案点（与任务书假设不符，如实登记）**：
- 题号头仍是 `Question 1/1 (1 unanswered)`（单题也带计数段）；
- **footer 只有三段：`tab to add notes | enter to submit answer | esc to interrupt`——单题时没有 `←/→ to navigate questions` 段**。任务书预期「footer 是否仍含该段」，实测 = 不含。这是 footer 三段/四段词形的分界：多题四段、单题三段。

### 左右键无效实证（`-s3-after-vkleft.txt` / `-s3-after-vkright.txt`）

单题上 `-Vk 0x25`（VK_LEFT）与 `-Vk 0x27`（VK_RIGHT）各一发，dump 屏面**逐字零变化**。单题方向键无效成立（也印证单题 footer 不渲染导航段）。

### 答题与归零（`-s3-zero.txt`）

j + 空格选 SQLite → `Question 1/1`（计数段消失，单题归零词形与多题一致）→ `-EnterVK` 交卷 → summary：

```
• Questions 1/1 answered
  • Which DB?
    answer: SQLite
```

（`codex-q-20261009-s3-summary.txt` 屏行 17–19）

### 属性层登记（选中态的字符读法事实）

- 选中行：属性掩码 `0x0030` 族（行前缀 `x2-…` 整段高亮），如 Q1 已选 Postgres 时 `ATTR 24 | x2-24:0x0030 …`；
- 焦点行（含未选中焦点）：`0x0107/0x0207` 族小块；
- 屏面**字符层无 `●`/`×`/`(*)` 之类选中标记**，`›` 只表焦点——选中态只活在属性层。**Task 2 判据必须吃属性层**，字符层读不到选中。

## 5. S4 Other/notes 链

### s4-q1（`codex-q-20261009-s4-q1.txt` 屏行 21–29）

提示词：`Which database do you want for a hello-world script?` options Oracle RAC / DB2（诱导 Other）。

```
  Question 1/1 (1 unanswered)
  Which database do you want for a hello-world script?

  › 1. Oracle RAC         使用 Oracle RAC 作为数据库。
    2. DB2                使用 DB2 作为数据库。
    3. None of the above  Optionally, add details in notes (tab)
  deepseek-v4.1-flash medium · ~\AppData\Local\Temp\probe-codex-q-20261008 · 看 D:/Program Files/Git/plan  Plan mode
  tab to add notes | enter to submit answer | esc to interrupt
```

### tab 直开 Other（`codex-q-20261009-s4-notes-open.txt`，§6-D 定案）

j×2 走到 `None of the above`（**不按空格**）→ 直接 tab → dump：

```
  Question 1/1 (1 unanswered)
  Which database do you want for a hello-world script?

    1. Oracle RAC         使用 Oracle RAC 作为数据库。
    2. DB2                使用 DB2 作为数据库。
  › 3. None of the above  Optionally, add details in notes (tab)
```

字符层与按 tab 前几乎同形，但属性层证据（同帧 `ATTR 24/26`）：
- **Other 行已被选中**（`ATTR 24` 前半 `0x0030` 高亮段出现——tab 前该行无此段）；
- **notes 输入态激活**（`ATTR 26` 出现整行 `x2-116:0x0030` 高亮条 = 输入行占位）。

**但字符层无可见的 notes 输入框边框/提示行**（`tab to add notes` footer 未变）。任务书问「notes 输入框出现？」——**答：无独立可见输入框行，输入态只在属性层可见**（0x0030 整行高亮条）。

### notes 打字（`codex-q-20261009-s4-notes-typed.txt`）

probe-type 打 `I will just run it locally` 后：
- 题号头变 `Question 1/1`（计数段消失，Other 选中态生效）；
- **焦点 `›` 跳回第 1 项 Oracle RAC**（选项区重排：`› 1. Oracle RAC`）；
- 字符层看不到输入的文字回显。

### §6-A 定案实验：notes 态回车连带交卷（`codex-q-20261009-s4-after-enter.txt`）

单题 notes 态直接 `-EnterVK` → **整卷立即提交**，dump：

```
• Questions 1/1 answered
  • Which database do you want for a hello-world script?
    answer: Oracle RAC
```

**定案点**：
1. **单题上 notes 回车 = 连带整卷提交**（§6-A 预期成立）；
2. **answer 落的是焦点行 `Oracle RAC`，不是 Other/notes 路径**——notes 打字后焦点跳回第 1 项的行为 + 回车提交焦点项，意味着**notes 文本实际没有随卷提交**（summary 无 notes 内容行，模型复述行也只有 `Oracle RAC`）。
3. 「Other 行 + notes」在 0.160.0 上**未形成可交付的 Other 答案**——本例 answer 直接是常规选项。

> 补充事实：tab 在**非 Other 行**也全局可用（见 §6 tab ensure：Azure 行直接 tab 同样选中+开输入态），所以「tab 直开 Other」的特化说法应修正为「tab = ensure 选中焦点行 + 开 notes 输入态」，Other 行只是恰好是焦点行时的一般情形。

### tab ensure 实验（`codex-q-20261009-s4-tab-ensure.txt`，`Which cloud?` Azure/AWS）

新一轮面板焦点停在第 1 项 Azure，**不空格直接 tab** → dump 屏行 21–29：

```
  Question 1/1 (1 unanswered)
  Which cloud?

  › 1. Azure              使用 Azure 作为云平台。
    2. AWS                使用 AWS 作为云平台。
    3. None of the above  Optionally, add details in notes (tab)
```

字符层计数段**仍显示** `(1 unanswered)`（与第一轮 tab 后立刻消失不同——此帧先于选中态落账），但**属性层 `ATTR 24` 出现整段 `0x0030` 高亮（Azure 已被选中）+ `ATTR 27` 输入行高亮条**（ensure 已发生，字符层计数刷新滞后一帧）。**定案：tab = 「选中焦点行（ensure）+ 开 notes 输入态」的复合键**。

### notes 态 esc（`-s4-tab-ensure-esc.txt` / `-esc2.txt`）

- `-VtName esc`（孤立 0x1B）：屏面零变化，仍在面板；
- `-Vk 0x1B -Scan 0x01`：**整个回答回合被中断**——

```
■ Conversation interrupted -use /feedback if something went w ong

› Ask Codex to do anything
```

（`codex-q-20261009-s4-tab-ensure-esc2.txt` 屏行 23；截图 `codexq-esc-interrupt-20261009-003641.png`）

**esc 实测结论（任务书点名要求）**：
- **取消 notes 输入态的 esc：不可达**。两种形态（`-VtName esc`、`-Vk 0x1B -Scan 0x01`）都试了，前者无效、后者直接杀了会话回合。按任务书纪律如实登记，不用 Enter 兜底；
- 附带发现：**面板存在时 `-Vk 0x1B -Scan 0x01` 的语义 = 中断当前回答回合**（`■ Conversation interrupted - use /feedback if something went wrong`）。这本身是 MAM「中止」按钮可用的强中断通道，但绝不能当取消键用。

## 6. F / G / E 实验

### F/#9 首帧字形（`codex-q-20261009-s9-firstframe-003704.txt`）

`Which OS?`（Linux/Windows）提示词发出后以 ~200ms 间隔连续 dump（30 次循环上限），抓到的**第一帧**即：

```
  Question 1/1 (1 unanswered)
  Which OS?

  › 1. Linux              使用 Linux 作为操作系统。
    2. Windows            使用 Windows 作为操作系统。
    3. None of the above  Optionally, add details in notes (tab)
```

**定案：首帧高亮行首即 `›`，`?` 形态帧未抓到（不可达/不存在）**。轮询首拍耗时 > 200ms 亦可解释为 `?` 形是渲染中间态早于首拍窗口——无论如何，read_screen_window 的工程视角里**只认 `›`**。

### G/滚回残留（`codex-q-20261009-g-residual.txt`）

上一轮（Which OS）交卷、模型收尾后 dump 可见窗：

```
• Questions 1/1 answered
  • Which OS?
    answer: Linux
…
• Working (2s • esc to interrupt)
  Worked for 22s • 00:37

› Ask Codex to do anything
```

**定案：可见窗内无旧面板/旧题号头残留**——面板交卷即整体消失，只留 summary 行组。滚回区（scrollback）在 read_screen_window 可见窗之外，**如实登记：不可见，未取證**。E 轮（下一轮）面板弹出后可见窗同样干净（见 `-e-wrap.txt` 屏行 15–17 只有新提示词）。

### E/折行（`codex-q-20261009-e-wrap.txt` 屏行 21–29）

提示词要求 60+ 字符 label：`Which deployment target?` options：`Amazon Elastic Kubernetes Service with multi-region fleet`（63 字符）/ `Plain Docker Compose on a single VM`。

```
  Question 1/1 (1 unanswered)
  Which deployment target?

  › 1. Amazon Elastic Kubernetes Service with multi-region fleet  使用 Amazon EKS 多区域集群部署。
    2. Plain Docker Compose on a single VM                        在单台虚拟机上使用 Docker Compose 部署。
    3. None of the above                                          Optionally, add details in notes (tab)
  deepseek-v4.1-flash medium · ~\AppData\Local\Temp\probe-codex-q-20261008 · 看 D:/Program Files/Git/plan  Plan mode
  tab to add notes | enter to submit answer | esc to interrupt
```

**定案（形态修正）**：63 字符 label **不折行**——codex 把 desc 列整体右移（对齐到最长 label 后两格），单行放下。属性层 `ATTR 24` 的高亮段延伸到 `x2-64`（64 列），确认单行宽行渲染。**「label 列固定宽、desc 固定列」的假设不成立：desc 列起点 = max(label 宽) + 2 空格浮动对齐**。屏幕宽度不够时的行为（真折行）本轮未测出（120 列装下了），如实登记为未测。

## 7. footer 三段/四段词形汇总（Task 2 词形库）

多题（N≥2）面板 footer（多帧还原）：

```
  tab to add notes | enter to submit all | ←/→ to navigate questions | esc to interrupt
```

单题（N=1）面板 footer：

```
  tab to add notes | enter to submit answer | esc to interrupt
```

规则：`enter to submit all` vs `enter to submit answer` 随「是否存在其它未答题」切换（S1 Q2 未答时已见 `submit all`，即使当前题已答）；`←/→ to navigate questions` 段仅多题渲染。

题号头词形：
- 未答：`Question i/N (M unanswered)`（M = 全卷未答数，非当前题状态）
- 全答：`Question i/N`（计数段整体消失）

摘要屏头：`Questions N/N answered` + `answer: <文本>` 行。
中断行（esc 强中断）：`■ Conversation interrupted - use /feedback if something went wrong`。

## 8. 证据索引

### dump 文本（`C:\Users\bunny\mam-probe-m6r\evidence\`）

| 文件 | label | 内容 |
|---|---|---|
| `codex-q-20261009-baseline.txt` | baseline | 首启主界面（无信任框） |
| `codex-q-20261009-plan-mode.txt` | plan-mode(事故帧) | MSYS 路径转换把 /plan 变路径（留证） |
| `codex-q-20261009-plan-mode2.txt` | trust-after/plan-mode | Plan mode 生效帧（footer `Plan mode`） |
| `codex-q-20261009-s1-q1.txt` | s1-q1 | S1 两题面板首帧 |
| `codex-q-20261009-s1-q1-after-digit.txt` | s1-q1-after-digit | A/#1 数字推进 |
| `codex-q-20261009-s1-q1-focus-back.txt` | s1-q1-focus-back | （VT left 无效帧） |
| `codex-q-20261009-s1-q1-focus-back-h.txt` | s1-focus-back-h | h 键切回 Q1 焦点落已选项 |
| `codex-q-20261009-s1-q2-focus-memcached.txt` | s1-q2-focus | Q2 焦点 Memcached |
| `codex-q-20261009-s1-q2-select-space.txt` | s1-q2-space | 空格选中 Memcached（计数消失） |
| `codex-q-20261009-s1-back-to-q1-h.txt` | s1-back-q1-h | h 切回 Q1（已答态） |
| `codex-q-20261009-s1-q1-walk-sqlite.txt` | s1-q1-walk | j 走到 SQLite（计数重现） |
| `codex-q-20261009-s1-q1-unanswered.txt` | s1-q1-unanswered | 反选实证（计数 +1） |
| `codex-q-20261009-s1-q1-reselect-sqlite.txt` | s1-q1-reselect | 再空格选回 SQLite |
| `codex-q-20261009-s1-q2-zero.txt` | s1-q2-zero | 归零词形 `Question 2/2` |
| `codex-q-20261009-s1-summary.txt` | s1-summary | 摘要屏（乱码帧） |
| `codex-q-20261009-s1-summary-recap.txt` | s1-summary(recap) | 摘要屏正常帧 |
| `codex-q-20261009-s2-q1.txt` | s2-q1 | S2 三题面板（含状态行插入变体） |
| `codex-q-20261009-s2-wrap-down.txt` | s2-wrap-down | 下环绕回首项 |
| `codex-q-20261009-s2-wrap-up.txt` / `-up2.txt` | s2-wrap-up | 上环绕到末项（up2 为复核帧） |
| `codex-q-20261009-s2-q3.txt` | s2-q3 | 数字推进到 Q3 |
| `codex-q-20261009-s2-confirm.txt` | s2-confirm | 末题 Enter 直接交卷（确认屏未出现） |
| `codex-q-20261009-s2-summary-recap.txt` | s2-summary | S2 摘要 |
| `codex-q-20261009-s3-single.txt` | s3-single | 单题 footer 三段词形 |
| `codex-q-20261009-s3-after-vkleft.txt` | s3-vkleft | 单题 VK_LEFT 无效 |
| `codex-q-20261009-s3-after-vkright.txt` | s3-vkright | 单题 VK_RIGHT 无效 |
| `codex-q-20261009-s3-zero.txt` | s3-zero | 单题归零 |
| `codex-q-20261009-s3-summary.txt` | s3-summary | S3 摘要 |
| `codex-q-20261009-s4-q1.txt` | s4-q1 | S4 面板（Oracle RAC/DB2） |
| `codex-q-20261009-s4-notes-open.txt` | s4-notes-open | tab 直开（Other 选中+输入态，属性层） |
| `codex-q-20261009-s4-notes-typed.txt` | s4-notes-typed | notes 打字后（焦点跳回第 1 项） |
| `codex-q-20261009-s4-after-enter.txt` | s4-after-enter | §6-A notes 回车连带交卷 |
| `codex-q-20261009-s4-tab-ensure.txt` | s4-tab-ensure | 非 Other 行 tab ensure |
| `codex-q-20261009-s4-tab-ensure-esc.txt` | s4-esc1 | VtName esc 无效帧 |
| `codex-q-20261009-s4-tab-ensure-esc2.txt` | s4-esc2 | Vk esc 中断回合 |
| `codex-q-20261009-s9-firstframe-003704.txt` | s9-first-frame | 首帧 `›` 形 |
| `codex-q-20261009-g-residual.txt` | g-residual | 交卷后可见窗无残留 |
| `codex-q-20261009-e-wrap.txt` | e-wrap | 63 字符 label 单行加宽 |
| `sessions/conhost-codex-20261009-002018.txt` | session | PID 记录（39356/45388/29424） |

### 截图（PNG，同目录）

- `codexq-baseline-20261009-002129.png`（baseline）
- `codexq-s1-q1-20261009-002306.png`（S1 面板，窗口标题 `[ . ] Action Required`）
- `codexq-s1-q2-zero-20261009-002904.png`（S1 归零态）
- `codexq-s1-summary-20261009-002924.png`（S1 摘要）
- `codexq-s4-summary-20261009-003523.png`（S4 摘要）
- `codexq-esc-interrupt-20261009-003641.png`（esc 强中断）
- `codexq-e-wrap-20261009-003825.png`（E 折行）

## 9. 给 Task 2 夹具生成器点名（夹具源清单）

| 夹具 | 源文件 + label | 取段 |
|---|---|---|
| 多题面板首屏（4 选项） | `codex-q-20261009-s1-q1-after-digit.txt` | FIXTURE 20–29 |
| 多题面板首屏（3 选项） | `codex-q-20261009-s2-q1.txt` | FIXTURE 21–29（含状态行插入变体） |
| 单题面板 | `codex-q-20261009-s3-single.txt` | FIXTURE 21–29 |
| 归零词形（多题） | `codex-q-20261009-s1-q2-zero.txt` | FIXTURE 20–26 |
| 归零词形（单题） | `codex-q-20261009-s3-zero.txt` | FIXTURE 21–29 |
| 计数重现（反选态） | `codex-q-20261009-s1-q1-unanswered.txt` | FIXTURE 21–26 |
| 摘要屏（N=2/3/1） | `…-s1-summary-recap.txt` / `…-s2-summary-recap.txt` / `…-s3-summary.txt` | `Questions N/N answered` 起 3–8 行 |
| notes 输入态（字符层） | `codex-q-20261009-s4-notes-open.txt` | FIXTURE 21–28 |
| 连带交卷摘要（Other 失败案例） | `codex-q-20261009-s4-after-enter.txt` | FIXTURE 17–19 |
| tab ensure 帧 | `codex-q-20261009-s4-tab-ensure.txt` | FIXTURE 21–29 |
| 中断行 | `codex-q-20261009-s4-tab-ensure-esc2.txt` | FIXTURE 23 |
| 首帧 | `codex-q-20261009-s9-firstframe-003704.txt` | FIXTURE 21–26 |
| 长 label 单行 | `codex-q-20261009-e-wrap.txt` | FIXTURE 21–29 |

**夹具生成注意**：
1. 每个夹具必须**成对携带字符层 FIXTURE + 属性层 ATTR**（选中态只在属性层，`0x0030` 族）；
2. footer 词形按 §7 的四段/三段规则生成，不得手抄抖动帧（`tnavigate`/`escsto` 类）；
3. 确认屏（`Submit with unanswered questions?`）**没有活体夹具**，本轮 H 不可达——不得伪造；
4. `?` 首帧形同理无活体证据。

## 10. 偏差与未尽事项清单

1. **S2 确认屏未复现**（H 不可达）：末题带未答题 EnterVK 直接交卷。若设计文档已把「确认屏」写死为 0.160.0 行为，需修正；
2. **单题 footer 无 `←/→` 段**（任务书预期「仍含」）：实测三段；
3. **VT 箭头族全无效 + vim h/j/k/l 有效**：设计 §2.3 若依赖箭头键序须改；
4. **notes 文本未随卷提交**（D 部分不符）：answer 落焦点行；「Other+notes」链在 0.160.0 不构成 Other 交付路径；
   **验收批 F1 对照（2026-10-09，`evidence\codex-q-acceptance-20261009\ACCEPTANCE-REPORT.md`）**：验收批 notes 态回车后摘要屏出现 `note: Postgres please` 且模型收到备注——本批「不落卷」未复现。可能性 = 焦点宿主差异（notes 输入框是否持焦点决定回车提交 notes 还是焦点选项行）。两批合读不定案，**「notes 文本去向 + notes 态回车交卷事件」登记为版本复验触发点**（下轮 codex 升级一并复验两形态）；能力面（具名中止）按本批证据收口维持——零注入在两种解读下都安全。
5. **esc 取消 notes 输入态不可达**；`-Vk 0x1B -Scan 0x01` = 强中断回合（副作用通道，勿用作取消）；
6. 空格选中后**不自动推进**（只有数字键推进）——S1 中一次「空格后未见推进」的观察据此定案；
7. 滚回区不可见未取证；E 实验的「宽度不够真折行」未测出（120 列单行放下）。
8. **已选项上空格语义待消歧**（验收批 b5 对照发现，见 §3 #5 焦点消歧补注）：底料批（非已选项空格 = 反选实证）与验收批（已选项空格 = 计数未重现）合读，空格 toggle 在两个焦点位置可能不同形——不影响编排安全侧（已答∧焦点≠目标 → 零键中止），登记为版本复验触发点。
