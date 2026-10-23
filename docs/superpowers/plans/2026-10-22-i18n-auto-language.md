# 三终端语言自适应 · 实施计划（桌面端 + 远程操控端）

> **For agentic workers:** 本计划的执行协议：**每个任务派发一个执行 Agent 实施，主 Agent 在任务完成后逐任务 review，review 通过后才进入下一任务**。执行者实施任何任务前**必须先通读需求书**（见 Spec）。计划不规定实现路线（实现自定），但**跨任务契约（§0）与每个任务的验收条件为硬性约束**，不得偏离。任务以 checkbox（`- [ ]`）跟踪。

**Goal:** 桌面端实现「未手动选择即跟随系统语言（含变化跟随）、手动后以最后一次手动切换为准」，远程操控端（`/m`）实现「手动记录 > 桌面当前语言 > 手机浏览器归流 > 英文」的语言决策链、换肤旁二值切换入口、按配对设备记账，并将约 306 条中文硬编码 UI 文案（tsx 约 250 + ts 约 56；严格口径：成对引号内含中文的字面量行、剔除注释行）一次性全量双语化。

**Architecture:** 语言唯一真值源保持在前端 i18n 实例（托盘现状同路）；Rust 侧新增单一只读「桌面当前语言」内存源与按配对设备的语言手动记账，全部经**既有已配对通道**下发（不新增未认证接口）；远程端将决策链接入既有 mobile i18n 目录（`mobile.*` 键体系），文案抽取不改组件行为。

**Tech Stack:** Tauri 2 + Rust（`src-tauri/src/remote/` 为移动端 server 与配对闸门所在）/ React 19 + TypeScript / i18next（zh.json + en.json 双目录）/ Vite（`pnpm build:mobile`）。

**Spec:** `docs/superpowers/specs/2026-10-22-i18n-auto-language-requirements.md`（需求书，最高依据；冲突时以需求书为准。执行者须先读其 §1 统一原则、§2 裁决记录 U1–U14、§4/§5 功能需求与 §6 验收标准）。

**范围声明：** 官网 W1–W4 在 tuvis-website 独立仓库另行实施（需求书 U9；且须待主仓库 README 英文化合入 main 后才发出实施，U13），**不在本计划**。GitHub About homepage 待办为手工操作，列于计划尾部清单，不属任务。

---

## Global Constraints（每个任务隐含继承，逐条硬性）

1. **语言归流**：`zh` 前缀系（zh、zh-CN、zh-SG、zh-TW、zh-HK、zh-MO 等，含简繁）一律显示**中文（简体版）**；其余任何语言一律显示**英文**；**不做繁体翻译**。
2. **只记手动，最后手动为准**：自动判定/跟随结果**一律不得写入任何「记住」存储**；手动选择后以**最后一次手动切换**为准，自动跟随不再覆盖。
3. **独立账本**：桌面端手动记录与远程端各配对设备记录**互不覆盖**——桌面手动切换只影响远程端**无手动记录**设备的跟随；远程某设备的手动选择**不影响桌面端**语言（需求书 §1.5 / U14）。
4. **配对闸门不放松**（L1）：语言下发与记账读写**仅挂在已配对（已通过 `remote/gate.rs` 闸门）的通道上**；禁止新增任何未认证即可读取桌面语言或读写记账的接口/字段。
5. **标题与 PWA 不动**（M6）：`/mobile.html` 的 title「Tuvis 远程」与 `public-mobile/manifest.json` 的 name **不改、不切换**。
6. **键命名空间**：远程端新增文案键一律 `mobile.*`（沿用既有体系）；zh.json 与 en.json 键集一致，`pnpm check:i18n` 必须通过。
7. **a11y 不漏**（g4）：aria-label 等 a11y 文案同步双语化，英文为**等价描述**而非逐字直译。
8. **错误文案走既有体系**（g5）：新增错误类文案不得绕过 `src/lib/invokeError.ts` 的 `formatInvokeError` 双语体系私造硬编码。
9. **中文残留门禁**（g2）：M5 范围文件**中文文案字面量**清零。判定用**严格口径**：成对引号内含中文的字面量行、剔除行首注释行（需求书附录命令为宽松口径——命中含中文注释与「代码行 + 行尾中文注释」的误报，**命中 ≠ 有文案**，须逐条归类）。确需保留的原文展示须显式登记豁免；**中文注释不属清零对象，禁止以删除中文注释的方式过关**。
10. **不翻译内容**（L3）：会话内容 / 终端原文 / 文件内容原样展示，不在双语化范围内。
11. **交付纪律**（U7）：全计划在同一特性分支上实施；**每任务一个 commit 收尾**（英文 message）；所有任务 + 终验通过后**一次性合入 main，不分批**。
12. **行为不回归**：所有既有功能（托盘、配对流程、server、既有测试）在改动后保持原行为；远程端改动不动 `src-tauri/src/services/usage/**`（采集域纪律与本计划无交集）。

---

## §0 跨任务契约总表（单一权威来源，任务内引用编号；两侧语义以本表逐字为准）

| # | 契约 | 内容 |
|---|---|---|
| C-1 | **语言值集** | 全链路语言值只有 `"zh"` 与 `"en"` 二值（U6 二值语义），无第三值。 |
| C-2 | **桌面持久化语义**（D1/D2） | 桌面端本地语言持久化**只允许存手动选择**（含「手动」事实与所选值）；未手动选择过 → **不持久化语言值**，每次冷启动重新检测系统语言并按归流判定。再次手动切换覆盖旧值。 |
| C-3 | **语言源同步**（前端→Rust） | 桌面当前生效语言唯一真值 = 前端 i18n 实例；Rust 侧维护**单一只读内存源**；更新时机 = 应用启动 i18n 初始化完成后一次 + 每次语言切换时一次（与托盘文案重建同路）；Rust 侧不自行判定语言。 |
| C-4 | **下发语义** | 字段名 **`desktopLanguage`**（值 `"zh"\|"en"`）与 **`manualLanguage`**（该设备手动记录值 `"zh"\|"en"`，无记录为 `null`），随**已配对会话的既有连接/初始化响应**下发；未配对 / 未过闸门的任何响应**不得包含**这两个字段。 |
| C-5 | **记账读写**（M3） | 键 = **既有配对设备身份标识**（与 `remote/pairing.rs` 配对身份同源）；值 = `{ manual: bool, language: "zh"\|"en" }`。**写**：仅手机端手动切换时写 `{ manual: true, language: 新值 }`，覆盖旧值；**读**：连接时按设备身份读；**新设备身份 = 无记录**（重新配对即回自动跟随）；自动跟随结果**绝不写入**。 |
| C-6 | **远程决策链**（M1/M2） | 优先级从高到低：① 该设备 `manualLanguage != null` → 取其值；② 已配对且 `desktopLanguage` 可得 → 取其值；③ 手机浏览器语言按归流（zh 前缀系 → `zh`，其余 → `en`）；④ `en`。重评估时机 = **连接时**（不要求会话中实时推送）；手机端手动切换**即时生效**并使该设备此后决策恒走第①级。 |
| C-7 | **首帧不闪变**（计划级实现质量约束，非需求条目） | 语言决策未定前不得渲染任何错误语言的可见文案（加载态/骨架允许；先渲染 A 语言再翻转为 B 语言的可见闪变不允许）。实现路线自定。 |
| C-8 | **键命名空间** | 新增文案键一律 `mobile.*`，zh/en 两目录同键同集（C-8 与全局约束 6 同义，任务内引用此编号）。 |
| C-9 | **html lang**（M7） | `/mobile.html` 根元素 `lang` 属性随生效语言更新：中文 → `zh`，英文 → `en`。 |

**任务依赖：** 1 → 2 → 3 → 4 串行；任务 5/6/7 仅依赖 C-8 与既有 i18n 实例，与 2/3/4 无强依赖（如并行派发，须各自独立 commit、同分支）；任务 8 依赖全部。

---

### Task 1: 桌面端语言语义改造（D1 / D2 / D4）

**目标：** 未手动选择时冷启动跟随系统语言（含系统语言变化后的跟随）；手动选择后以最后一次手动切换为准且系统语言不再覆盖。

**文件与模块：**
- Modify: `src/i18n/index.ts`（现状：LanguageDetector `order: ["localStorage", "navigator"]` + `caches: ["localStorage"]`——首次检测结果被持久化是 D1 差距根源，改造检测/缓存策略以满足 C-2）
- Modify: `src/components/common/language-toggle.tsx`、`src/pages/settings.tsx`（手动切换入口改为写 C-2 手动记录）
- 定位：语言相关 localStorage 键的读写处（随 i18n 改造一并梳理）

**跨任务契约：**
- 产出：**C-2**（桌面持久化语义）；「当前生效语言」仍唯一取自前端 i18n 实例（供 C-3 消费）。
- 消费：无（首任务）。

**执行要点：**
1. 实现路线自定（可改 detection order / 移除 caches / 引入手动标记存储等任一方案），但必须满足 C-2 逐字语义：未手动 → 不持久化语言值，每次冷启动重新检测。
2. 手动切换写「手动标记 + 值」；再次切换覆盖；归流判定（zh 前缀系 → zh，其余 → en）保持现状行为。
3. 托盘链路（`src-tauri/src/plugins/system_tray.rs:11-16`、`src/pages/home.tsx:74` 的重建触发）不得被破坏。

**验收条件：**
- [ ] 需求书 6.3 场景 1：无手动记录（清除语言本地存储后冷启动）+ 系统语言 zh-TW / zh-HK → 简体中文界面；系统语言 ja / de → 英文界面
- [ ] 需求书 6.3 场景 2：未手动选择时改系统语言后重启应用 → 界面跟随新语言（U8 核心语义）
- [ ] 需求书 6.3 场景 3：手动选中文后系统语言改英文并重启 → 仍中文
- [ ] 需求书 6.3 场景 5：手动切换多次（中 → 英 → 中）后重启 → 以最后一次手动选择为准
- [ ] 需求书 6.3 场景 4（回归）：托盘菜单文案始终与界面语言一致，切换语言后即时重建
- [ ] `pnpm check` 全绿

**Commit:** `feat(desktop): follow system language until manual selection, last manual wins`

---

### Task 2: 桌面侧基建——语言源同步、下发与配对设备记账（Rust）

**目标：** Rust 侧可得桌面当前语言（单一内存源），实现按配对设备的语言手动记账读写，并把 `desktopLanguage` / `manualLanguage` 经已配对通道随连接响应下发。

**文件与模块：**
- Create: Rust 侧语言内存源模块（建议落 `src-tauri/src/remote/` 内新文件或就近模块）
- Modify: `src-tauri/src/remote/server.rs`（已配对会话的连接/初始化响应中附带 C-4 两字段）
- Create/Modify: 记账存储与读写（存储介质自定：SQLite 走 `src-tauri/src/database/` 既有 schema/migration/dao 体系，或文件存储落 remote 模块；**键必须与 `remote/pairing.rs` 配对设备身份同源**）
- Modify: `src-tauri/src/commands/`（前端推送语言的 IPC 入口，新增或就近扩展）
- Test: 上述模块的 Rust 单元测试（`src-tauri/tests/` 或模块内 `#[cfg(test)]`，遵循仓库既有测试布局）

**跨任务契约：**
- 消费：**C-3**（语言真值来自前端推送；任务 1 已确立 i18n 实例为唯一真值）、**C-1**（值集）。
- 产出：**C-3**（Rust 单一只读内存源 + IPC 推送入口，更新时机 = 启动初始化后 + 每次切换）、**C-4**（下发字段）、**C-5**（记账读写语义 + 已配对通道上的手动切换写入接口：请求载荷字段名 `language`，值 `"zh"|"en"`，响应返回该设备最新记录）。
- 边界：**C-5 写接口仅已配对会话可调**（全局约束 4）。

**执行要点：**
1. 语言内存源由前端在启动 i18n 初始化完成后与每次切换时推送（与托盘重建同路触发），Rust 侧只存不判。
2. 记账存储自定，但读写语义逐字满足 C-5；设备身份沿用配对身份，重新配对的新身份天然无记录（旧记录可物理清理，属实现自由）。
3. 下发挂点自定（连接握手或既有初始化响应），但未配对响应不得含 C-4 两字段——加测试钉死。
4. 手动切换写入接口路由风格遵循 server 既有路由，载荷字段名 `language` 钉死（任务 4 消费）。

**验收条件：**
- [ ] Rust 单测覆盖 C-5 全语义并通过：① 无记录设备读 → 无记录（manual 态为 false 或等价）；② 手动写 `{language:"zh"}` 后读 → `{manual:true, language:"zh"}`；③ 再写 `{language:"en"}` → 覆盖为 en（最后手动为准）；④ 新设备身份读 → 无记录；⑤ 连接下发 `desktopLanguage` **不产生任何记账写入**（自动结果绝不写）
- [ ] Rust 单测覆盖 C-3：两次推送（zh 后 en）→ 内存源依次 zh、en；未推送时内存源为「未知」态且不自行判定
- [ ] 未配对 / 未过 `gate.rs` 闸门的响应中不含 `desktopLanguage` 与 `manualLanguage`（测试断言，全局约束 4 / L1）
- [ ] `cd src-tauri && cargo test` 全绿；`cargo clippy` 无新增告警
- [ ] `pnpm check` 全绿（若前端推送入口涉及 ts 侧改动）

**Commit:** `feat(remote): desktop language source, pairing-scoped mobile language ledger and delivery`

---

### Task 3: 远程端语言决策链接入 + html lang（M1 / M2 / M7）

**目标：** 远程端按 C-6 决策链确定生效语言（手动记录 > 桌面当前 > 浏览器归流 > 英文），首帧不闪变，`<html lang>` 随生效语言更新。

**文件与模块：**
- Create: `src/mobile/` 下语言决策模块（定位：mobile 入口初始化处，`src/mobile/main.tsx` / `App.tsx` 就近）
- Modify: `src/mobile/api.ts` 或连接逻辑所在处（消费 C-4 响应字段）
- Modify: `/mobile.html`（`lang` 初始值与 C-9 更新机制）
- 定位：连接/初始化时序（`Board.tsx` 连接链路）——决策重评估挂连接时

**跨任务契约：**
- 消费：**C-4**（响应字段 `desktopLanguage` / `manualLanguage`）、**C-5**（读侧：连接时按设备取 `manualLanguage`）、**C-6**（决策链——本任务是其唯一实现处，任务 4 复用）、**C-1**、**C-9**。
- 产出：mobile 生效语言决策模块（暴露「当前生效语言 + 切换生效入口」给任务 4；切换生效入口语义 = 立即变更生效语言并置顶该设备决策，写回走任务 2 的 C-5 写接口，本任务不建 UI）。
- 边界：**不做切换 UI**（任务 4）；PairPage 阶段无桌面语言可用，走 C-6 第③级（L1 既定）。

**执行要点：**
1. 决策链优先级逐字按 C-6；连接时重评估；已配对会话中桌面切语言**不要求**实时推送（下次连接跟上即可）。
2. 决策未定期间按 C-7 处理首帧（加载态可，闪变不可）。
3. 浏览器语言归流复用与桌面一致的 zh 前缀判定（全局约束 1）。
4. `manualLanguage` 读路径本任务以任务 2 的记账写接口**预置记录**方式验证（UI 在任务 4 才有）。

**验收条件：**
- [ ] 需求书 6.4 场景 1：桌面中文 + 无手动记录手机连接 → 手机页中文；桌面切英文后手机**下次连接** → 英文（M1 跟随，C-6 第②级）
- [ ] 需求书 6.4 场景 4：未配对手机打开配对页：zh-TW 手机浏览器 → 中文配对页；英文系统手机 → 英文配对页（L1 + M2，C-6 第③级）
- [ ] 手动优先读路径：经任务 2 写接口预置 `{language:"zh"}` 的设备连接（此时桌面为英文）→ 手机页中文（C-6 第①级）
- [ ] 桌面语言不可得（获取失败）→ 手机浏览器语言归流生效（C-6 第③级负向触发）
- [ ] 首帧不闪变：连接决策期间不出现「先英文渲染后翻中文」类可见闪变（C-7）
- [ ] 需求书 M7 / C-9：生效语言中文时 `<html lang="zh">`、英文时 `lang="en"`
- [ ] `pnpm build:mobile` 成功；`pnpm check` 全绿

**Commit:** `feat(mobile): language decision chain (manual > desktop > browser > en) with html lang`

---

### Task 4: 远程端切换入口与手动记账同步（M3 / M4）

**目标：** Board 页头换肤图标旁新增二值语言切换按钮，切换即时生效并同步桌面按设备记账；独立账本语义端到端成立。

**文件与模块：**
- Modify: `src/mobile/Board.tsx`（换肤按钮位于 `Board.tsx:511-518`（Sun/Moon 图标在 :517）；新按钮置于同排）
- Modify: 任务 3 的语言决策模块（接入切换生效入口）
- Modify: `src/mobile/api.ts`（调用任务 2 的手动切换写入接口，载荷 `language`）

**跨任务契约：**
- 消费：**C-5**（写路径：手动切换 → `{ manual: true, language: 新值 }`，覆盖旧值）、**C-6**（切换即时生效并恒走第①级）、任务 3 的「切换生效入口」、**C-1**。
- 产出：远程端二值切换入口（中文 ⇄ 英文，无三态，U6）。
- 边界：入口**仅 Board 页头**（M4 指定位置）；PairPage 不加入口；全局约束 3（独立账本）端到端不破坏。

**执行要点：**
1. 按钮样式与同排图标按钮一致（与「历史」/ 声音开关同构），图标语义自定（能区分中/英当前态即可）。
2. 切换 = 即时变更生效语言 + 调写接口记账；写接口失败时 UI 不得假装成功（如实提示，文案走既有错误体系 g5）。
3. 不做「恢复自动跟随」三态（U6）。

**验收条件：**
- [ ] 需求书 6.4 场景 2：手机手动选英文 → 桌面记账；此后桌面保持中文，手机多次重连 / 换网络 / 换 IP 重连 → 仍英文（C-5 记录优先且不随 origin 丢失）
- [ ] 需求书 6.4 场景 3：该手机重新配对 → 记录清零（新设备身份无记录）→ 回到跟随桌面语言
- [ ] 需求书 6.4 场景 7：桌面手动切英文后：无手动记录手机下次连接 → 英文；已有中文手动记录的手机 → 仍中文（§1.5 独立账本）
- [ ] 需求书 6.4 场景 8：手机手动选英文 → 桌面端界面语言保持不变（远程选择不影响桌面，§1.5 / U14）
- [ ] 切换即时生效，无需重连；二值切换无三态（U6）
- [ ] 入口位于换肤图标旁且样式与同排一致（M4）
- [ ] `pnpm check` 全绿

**Commit:** `feat(mobile): language toggle beside theme icon, syncs per-device manual override`

---

### Task 5: 文案双语化 A——首屏与配对组（M5）

**目标：** PairPage、Board、BookmarkBar 及首屏就近组件的中文硬编码文案全部迁入 `mobile.*` 双语键。

**文件与模块：**
- Modify: `src/mobile/PairPage.tsx`、`src/mobile/Board.tsx`、`src/mobile/BookmarkBar.tsx`
- Modify: `src/i18n/locales/zh.json`、`src/i18n/locales/en.json`（新增 `mobile.*` 键）
- 纳入：需求书范围清单未点名但严格口径扫描含文案的文件（命中即纳入；2026-10-22 实测未点名 tsx 文案字面量均为 0、中文仅在注释——此类文件记录后跳过，见执行要点 4）

**跨任务契约：**
- 消费：**C-8**（`mobile.*` 键命名空间）、既有 i18n 实例（`import i18n from "@/i18n"` 既有用法）。
- 产出：本组文件双语键集（zh/en 同键同集）。
- 边界：只抽文案不改组件行为；错误类文案走 `formatInvokeError` 体系（g5，勿在本任务私造）；不触碰 ts 逻辑文件（任务 7）。

**执行要点：**
1. 实现路线自定；键命名沿用既有 `mobile.*` 分组风格。
2. 扫描命令用需求书附录（宽松口径），命中后按全局约束 9 严格口径逐条归类；JSX 裸文本人工过 diff。
3. a11y 文案（aria-label 等）一并双语（g4）。
4. 仅注释含中文、无文案字面量的文件**记录后跳过**（不得为过关删注释）；本组实测文案计数（2026-10-22，供 review 抽查参考）：PairPage 10、Board 14、BookmarkBar 14。

**验收条件：**
- [ ] 本任务文件清单经需求书附录 grep 扫描后**逐条归类**：**文案字面量命中 = 0**（严格口径）；注释 / 裸文本命中经人工 diff 确认非文案后放行并登记（**禁止以清除中文注释的方式过关**）
- [ ] `pnpm check:i18n` 通过；本任务新键全部 `mobile.*` 且 zh/en 同键（C-8）
- [ ] 涉及页面（配对页、Board、书签栏）中 / 英两语完整可用，含 toast / placeholder / aria-label（g3 / g4）
- [ ] 豁免项（如确需保留的原文展示）显式登记于 commit message 或任务记录
- [ ] `pnpm build` 与 `pnpm build:mobile` 通过

**Commit:** `feat(mobile-i18n): extract first-screen and pairing copy to mobile.* keys`

---

### Task 6: 文案双语化 B——其余 tsx 组件（M5）

**目标：** 需求书范围清单其余 tsx 组件与目录内全部未点名 tsx 的中文硬编码文案全部迁入 `mobile.*` 双语键。

**文件与模块：**
- Modify: `src/mobile/QuestionCard.tsx`、`ApproveCard.tsx`、`PlanFeedbackBar.tsx`、`SessionDetail.tsx`、`FilePreview.tsx`、`FilePanel.tsx`、`CreateSessionSheet.tsx`、`MessageComposer.tsx`、`ModeBar.tsx`、`ArchiveBoard.tsx`、`ArchiveDetail.tsx`、`SubagentDetail.tsx`、`SubagentList.tsx`、`PreviewModeSwitcher.tsx`、`message-render.tsx`、`SplitHandle.tsx`
- 纳入：目录内其余严格口径扫描含文案的 tsx（命中即纳入；2026-10-22 实测其余 tsx——InteractiveCard、PendingRequestCard、ProjectedArtifacts、SubagentChips、CodexQueuePanel、App.tsx 等——文案字面量均为 0、中文仅在注释，记录后跳过，见执行要点 3）
- Modify: `src/i18n/locales/zh.json`、`src/i18n/locales/en.json`

**跨任务契约：**
- 消费：**C-8**、既有 i18n 实例。
- 产出：其余 tsx 组件双语键集。
- 边界：同任务 5（只抽文案不改行为；错误文案走既有体系；不触碰 ts 逻辑文件）。

**执行要点：**
1. 与任务 5 同构执行；按卡片 / 详情 / 输入 / 历史子代理分组落键，沿用既有分组风格。
2. a11y 文案一并双语（g4）。
3. 仅注释含中文的文件记录后跳过（不得为过关删注释）；点名文件实测文案计数（2026-10-22，供 review 抽查参考）：QuestionCard 34、CreateSessionSheet 31、MessageComposer 29、FilePanel 25、FilePreview 16、SessionDetail 17、ApproveCard 12、ArchiveDetail 12、ModeBar 10、ArchiveBoard 9。

**验收条件：**
- [ ] 本任务文件清单（含严格口径扫描命中的全部其余 tsx）经需求书附录 grep 扫描后**逐条归类**：**文案字面量命中 = 0**（严格口径）；注释 / 裸文本命中经人工 diff 确认非文案后放行并登记（**禁止以清除中文注释的方式过关**）
- [ ] `pnpm check:i18n` 通过；新键全部 `mobile.*` 且 zh/en 同键（C-8）
- [ ] 问答 / 审批 / 会话详情 / 文件预览 / 新建会话 / 消息输入 / 历史与子代理各页面中 / 英两语完整可用（g3 / g4）
- [ ] 豁免项显式登记
- [ ] `pnpm build` 与 `pnpm build:mobile` 通过

**Commit:** `feat(mobile-i18n): extract remaining tsx component copy to mobile.* keys`

---

### Task 7: 文案双语化 C——ts 逻辑文件与错误文案门禁复核（M5 + g5）

**目标：** ts 逻辑文件的中文硬编码文案（toast / 提示 / 文案常量）全部迁入 `mobile.*` 双语键，并复核新增文案未绕过 `formatInvokeError` 体系。

**文件与模块：**
- Modify: `src/mobile/board-logic.ts`、`resume-gate.ts`、`message-fold.ts`、`archive-logic.ts`、`api.ts`
- 纳入：目录内其余严格口径扫描含文案的 ts 文件（2026-10-22 实测仅 `codexIpcApi.ts` 1 条；artifact-links / bookmarks / codex-history / sound / useCodexThread 等中文仅在注释，记录后跳过，见执行要点 2）
- Modify: `src/i18n/locales/zh.json`、`src/i18n/locales/en.json`
- 复核: `src/lib/invokeError.ts`（不改造，仅复核新错误文案未绕过）

**跨任务契约：**
- 消费：**C-8**、既有 i18n 实例。
- 产出：ts 逻辑文件双语键集；`formatInvokeError` 复核结论（g5 门禁在本任务收口）。
- 边界：任务 3/4 对 `api.ts` 的改动不得回退；只抽文案不改逻辑分支。

**执行要点：**
1. ts 文件中的错误类文案优先复用/扩展 `formatInvokeError` 键体系，缺键补键。
2. 扫描以严格口径命中为准；仅注释含中文的文件记录后跳过（不得为过关删注释）。点名 5 个 ts 文件实测文案计数（2026-10-22，供 review 抽查参考）：api.ts 34、board-logic.ts 12、message-fold.ts 6、resume-gate.ts 2、archive-logic.ts 1。

**验收条件：**
- [ ] 全部含文案的 ts 文件（点名 5 个 + 扫描命中）经需求书附录 grep 扫描后**逐条归类**：**文案字面量命中 = 0**（严格口径）；注释 / 裸文本命中经人工 diff 确认非文案后放行并登记（**禁止以清除中文注释的方式过关**）
- [ ] `pnpm check:i18n` 通过；新键全部 `mobile.*` 且 zh/en 同键（C-8）
- [ ] g5 收口：本任务新增错误类文案全部经 `formatInvokeError` 输出，grep 无绕过性硬编码；复核结论记录于 commit message
- [ ] `pnpm build:mobile` 通过；`cd src-tauri && cargo test` 全绿（server 测试依赖移动端构建产物，先构建再测试为既有约定）
- [ ] 豁免项显式登记

**Commit:** `feat(mobile-i18n): extract ts logic copy, enforce error i18n gate`

---

### Task 8: 终验——全量回归与需求书验收总表对照

**目标:** 全量命令回归 + 需求书 §6 验收面逐项对照通过，确认一次性交付（U7）就绪。

**文件与模块：**
- 无代码改动（发现问题则退回对应任务修复后重跑本任务）。

**跨任务契约：**
- 消费：任务 1–7 全部产出与 §0 全部契约。

**执行要点：**
1. 按「命令回归 → 中文残留全扫 → §6 场景人工总表」顺序执行。
2. §6.2 官网场景不在本计划验收面（独立仓库，见范围声明）；M6（标题 / PWA 不动）与 L3（内容不翻译）作为总表确认项。

**验收条件：**
- [ ] g6 全绿：`pnpm check`、`pnpm build:mobile`、`cd src-tauri && cargo test`（先 build:mobile 再 cargo test）；`pnpm check:i18n` 通过
- [ ] g2 全清：需求书附录 grep 代入 `src/mobile/` 全部 tsx/ts 后**逐条归类**——**文案字面量命中 = 0**（严格口径）；注释 / 裸文本命中经人工 diff 确认非文案后放行并登记（**禁止以清除中文注释的方式过关**）
- [ ] 需求书 §6.3 场景 1–5 逐项人工核对通过（桌面端）
- [ ] 需求书 §6.4 场景 1–8 逐项人工核对通过（远程端）
- [ ] M6：`/mobile.html` title 与 `public-mobile/manifest.json` name 仍为「Tuvis 远程」未改（git diff 确认无改动）
- [ ] L1–L3 行为符合需求书已知限制表（配对页拿不到桌面语言 / 重新配对清零 / 内容不翻译）
- [ ] git log 核对：本分支任务 1–7 各一个 commit、commit message 与计划建议一致或语义等价；全部分支改动一次性可合入 main（U7）

**Commit:** `test: final regression and acceptance matrix for i18n auto-language`

---

## 手工操作清单（非代码、不派执行 Agent）

| # | 操作 | 说明与时序 |
|---|---|---|
| H1 | GitHub 仓库 About 的 homepage 由原地址（`https://jarvislee90s-dot.github.io/tuvis-website/zh`）改为 `https://tuvis.app` | **GitHub 网页设置，非本仓库文件改动**（需求书 U12）。时序建议：与官网 W1（根路径语言分流）上线**同步**做；若提前，可先指 `https://tuvis.app/en/`。最终时序由用户定 |
| H2 | 官网侧实施提示词发出 | 待主仓库 README 英文化改动合入 main 后才发出（需求书 U13），与本计划无代码耦合 |

## 需求覆盖对照表（reviewer 总览用）

| 需求书条目 | 覆盖任务 |
|---|---|
| D1 / D2 / D4 | 任务 1 |
| D3（回归确认） | 任务 1（场景 4）+ 任务 8 |
| M1 / M2 | 任务 3（决策链）+ 任务 2（下发） |
| M3 | 任务 2（记账读写）+ 任务 3（读路径）+ 任务 4（写路径端到端） |
| M4 | 任务 4 |
| M5 | 任务 5 / 6 / 7（覆盖需求书范围清单的**文件行** ∪ 严格口径扫描命中全集；范围清单「语言基建」行由任务 1–4 覆盖） |
| M6 / M7 | 任务 8（M6 确认）/ 任务 3（C-9） |
| L1 | 任务 2（闸门测试）+ 任务 3（场景 4） |
| L2 | 任务 4（场景 3） |
| L3 | 全局约束 10 + 任务 8 确认 |
| §6.1 g1–g6 | 任务 5/6/7（g1–g5）+ 任务 8（g6 汇总） |
| §6.3 / §6.4 场景 | 任务 1 / 任务 3+4 / 任务 8 总表 |
| §6.2 官网场景 | 范围外（独立仓库，见范围声明） |
| W1–W4 / U11–U13 | 范围外（tuvis-website 独立仓库 + 手工清单 H1/H2） |
