# 三终端语言自适应 · 验收总表（2026-10-22）

- 需求依据：`docs/engineering/specs/2026-10-22-i18n-auto-language-requirements.md`（下称「需求书」）
- 分支：`feat/i18n-auto-language`（worktree `/Users/jarvis/Documents/tuvis-worktree2`）
- 基线 commit：`2f4fd843`（分支点）；任务 1–7 各一笔 commit + 本验收文档一笔 = 8 笔（U7 一次性交付）
- 验收执行日：2026-10-22（Task 8 终验，本文档即其交付物）
- 范围声明：桌面端（D1–D4）+ 远程手机页（M1–M7）为本计划验收面；§6.2 官网场景在 tuvis-website 独立仓库，范围外；W1–W4 / U11–U13 同范围外（手工清单 H1/H2 另行跟进）

---

## 一、命令回归结果（g6）

按仓库既有约定顺序执行（server 测试消费移动端构建产物：先 `build:mobile` 再 `cargo test`）。

| # | 命令 | 结果 |
|---|---|---|
| 1 | `pnpm check`（format:check + lint + check:i18n + check-error-codes + tsc/vite build） | ✅ 全绿（末步 `✓ built in 727ms`，无告警错误） |
| 2 | `pnpm build:mobile` | ✅ `✓ built in 432ms`（dist-mobile 产出正常） |
| 3 | `cd src-tauri && cargo test` | ✅ **3188 passed / 0 failed / 42 ignored**（主套件 2816 passed / 38 ignored + 集成套件 77/61/48/40/36/20/18/14/9/8/8/7/6/6/6/4/3/1 全绿） |
| 4 | `cd src-tauri && cargo clippy --all-targets`（informational） | ✅ 0 warning；`cargo clippy -- -D warnings` 正常结束 |
| 5 | `pnpm check:i18n` | ✅ 通过：**i18n 键对齐通过（2311 键）**，zh/en 同键集 |
| 6 | `pnpm test` | ✅ **Test Files 179 passed (179)；Tests 2121 passed / 5 skipped（既有跳过）/ 0 failed** |

语言专项测试（本计划新增，全过）：`tests/i18n/` 3 文件 55 用例（language-prefs 32 + desktop-language-coldstart 19 + language-toggle 4）+ `tests/mobile/language-{decision,connect,toggle}.*` 3 文件 43 用例（32 + 4 + 7）= **98 用例全绿**；Rust 侧语言/账本专项 **15 条**（`remote::language::*` 9 + `remote::server::tests` 5 + `remote::tests::set_desktop_language_*` 1）全绿——按姓名 grep 会多计入 `remote::api::tests::plan_pending_cross_language_fixture_cases`，该用例基线 `2f4fd843` 已存在（非本计划新增），不计入。

---

## 二、g2 中文残留全扫归类表

**扫描口径**：需求书附录宽松 grep（`grep -n "[\"'\`][^\"\`]*[一-龥]"`）代入 `src/mobile/` 全部 **42 个** `.ts`/`.tsx`（范围清单文件全覆盖 + 全目录严格扫，无子目录遗漏）——**宽松命中 534 行**。归类采用**权威方法：TypeScript 编译器 AST 扫描**（注释不入 AST，字符串字面量 / 模板串 cooked text / JSX 文本逐节点精确判定，杜绝「注释含引号」与「正则字面量含引号」两类 grep 误报），并对正则字面量与标识符做异常检查。

### 归类终表

| 归类 | 数量 | 位置与依据 | 处置 |
|---|---|---|---|
| **文案字面量（登记豁免外）** | **0** | — | **清零达成（严格口径）** |
| errcode 镜像句（豁免，已登记） | 28 条 | `api.ts` 27 处 + `codexIpcApi.ts:203` 1 处，形态一律 `` `MOBILE_NETWORK_FAIL:<param>: <param> 网络异常: …` `` | 保留。`src/lib/invokeError.ts` 头注既定镜像约定（与 `src-tauri/src/errcode.rs` 互为镜像）；码已注册（`errors.mobileNetworkFail` zh/en 双键），用户可见文案经 `formatInvokeError` 整句换双语模板，原中文句不上屏。属需求书 §5 范围外注记的「既有错误文案体系」，非 UI 文案 |
| 服务端错误串匹配器（豁免，已登记） | 2 条 | `QuestionCard.tsx:858` `res.error.includes("备注文本")` / `includes("备注态意外关闭")` | 保留。后端 freeText 核验失败回执的静默调和判据，须与后端产出逐字一致，翻译即失灵；属后端协议面，展示侧已走 `formatInvokeError`（g5 不受损） |
| JSX 裸文本中文 | **0** | AST `JsxText` 全扫（grep 不可见面，人工 diff 兜底的替代） | 无残留 |
| 正则字面量 / 标识符含中文 | **0** | AST 异常检查 | 无 |
| ApproveCard 模板串（Task 6 豁免②复核） | — | `ApproveCard.tsx:411-412`：中文位于 className 模板串**内嵌的 `//` 注释**中；AST 证实模板 cooked text 无中文，实为注释非字面量 | 保留注释不动 |
| ZH/EN 语言按钮短码（Task 4 豁免） | — | `Board.tsx` 语言按钮 aria-hidden 短码，ASCII 自指码（ISO 语言代码任何语言下同值，翻译无意义） | 不属中文面，扫描零命中，维持豁免登记 |
| **中文注释** | **1850 行**（42 文件） | 逐文件计数见 task-8-report；含 JSDoc / 行注 / JSX 注释 | **全部保留，未删未译**（附录明示注释不属清零对象） |

### 中文注释保留核对（vs 基线 `2f4fd843`）

- `git diff 2f4fd843..HEAD -- src/mobile` 删除行中含「`//` + 中文」的共 **8 行**，逐条对照上下文核实**均为就地改写**（注释内容随代码变更同步修订，语义保留，非删除）：
  1. `Board.tsx`「最后状态中文…STATUS_LABELS」→ 改写为「最后状态显示名…statusLabel 六键…渲染期现取 i18n」（Task 7 键表改造随迁）；
  2. `ArchiveDetail.tsx`「移动端口径硬编码中文（无 i18n 契约）」→ 改写为「（M5 双语键）」（原陈述已失效）；
  3. `Board.tsx`「host 拉取失败时保持 null…」→ 改写并**扩写**（补连接时语言重评估 / I1 代数守卫 / M2 fail-open 三段，Task 3）；
  4. `FilePanel.tsx`「类型下拉档…决策 6」→ 补「文案 M5 双语键现取」；
  5. `MessageComposer.tsx` gone 回执注释 → 补「message 存 i18n 键（M5），渲染期现取」；
  6. `MessageComposer.tsx` blocked 回执注释 → 同上补写；
  7. `api.ts`「常量兜底」→ 改写为「缺省键兜底」（`APP_FORM_PENDING_FALLBACK` 键化退役，Task 7）；
  8. `api.ts` 审批应答错误码 doc 注释 → 随 `approveErrorCopy` 键化改写。
- 新增含中文注释行 **40 行**（Task 2/3 语言基建契约注释等），**净增**。结论：**未以清除中文注释的方式过 g2**。

---

## 三、§6.3 桌面端场景对照表

| # | 场景（需求书原文缩写） | 覆盖测试 / 证据 | 判定 |
|---|---|---|---|
| 1 | 系统语言 zh-TW / zh-HK 首启 → 简体中文；ja / de → 英文 | `tests/i18n/desktop-language-coldstart.test.ts`「6.3-1 首次启动按系统语言归流」（10 参数表驱，含 zh/zh-CN/zh+en/en-US/fr）＋ `tests/i18n/language-prefs.test.ts`「首个可解析候选优先」（混合偏好序 `["en-US","zh-HK"]→en`、`["zh-HK","en"]→zh` 等 4 用例，评审修复轮回归） | 自动化覆盖 ✓ |
| 2 | 未手动选择时改系统语言重启 → 跟随新语言（U8 核心新增） | 同上「6.3-2 未手动选择时跟随系统语言变化」（ja→zh-TW、zh-HK→en-US 双向）＋ C-2 不变量测试（无手动不落盘，故每次冷启动重检测） | 自动化覆盖 ✓ |
| 3 | 手动选中文后系统改英文重启 → 仍中文（手动优先） | 同上「6.3-3 手动选择后系统语言变化不覆盖」（zh+en-US、en+zh-TW 双向）；写点唯一性：`language-toggle.tsx` 手动入口先 `writeManualLanguage` | 自动化覆盖 ✓ |
| 4 | 托盘文案始终与界面语言一致（含切换即时重建） | `tests/i18n/language-toggle.test.tsx`「托盘即时重建（refresh_tray）并广播 language-changed」＋「languageChanged 事件触发 react 重渲染」——前端触发链三要素全锁；Rust `system_tray.rs` 消费端本计划零改动（回归由 cargo test 全量保障） | 前端链自动化 ✓；Rust 托盘消费端**留真机验收**（控制器既定裁决） |
| 5 | 多次手动切换（中→英→中）重启 → 以最后一次为准（U14） | 同上「6.3-5 多次手动切换以最后一次为准」＋ `tests/i18n/language-toggle.test.tsx`「连续切换」＋ `language-prefs.test.ts` 覆盖写用例 | 自动化覆盖 ✓ |

（D3 托盘跟随、D4 归流即场景 1/4，已并入上表；`i18nextLng` 旧键迁移为裁决内代价，见 §八。）

---

## 四、§6.4 远程端场景对照表

| # | 场景（需求书原文缩写） | 覆盖测试 / 证据 | 判定 |
|---|---|---|---|
| 1 | 桌面中文 + 无手动记录手机连接 → 中文；桌面切英文后手机下次连接 → 英文（M1） | Rust 下发面：`host_paired_response_carries_language_fields_without_writing_ledger`（/host 携带 desktopLanguage/manualLanguage 且不写账）＋ `host_language_fields_stay_null_when_unknown_or_unrecorded`；前端决策面：`tests/mobile/language-decision.test.ts`「applyConnectLanguage 连接跟随（6.4-1）」2 用例（含跟随结果不落盘断言）＋ 接线集成 `tests/mobile/language-connect.test.tsx`「6.4-1」（Board 真实消费 C-4 载荷，两次挂载两次重评估） | 自动化覆盖 ✓ |
| 2 | 手机手动选英文 → 桌面记账；此后多次重连 / 换网络 / 换 IP → 仍英文（M3，不随 origin 丢失） | `tests/mobile/language-toggle.test.tsx`「6.4-2」（两次重挂载各自新 /host + 冷启动缓存直通，均 en；桌面源始终 zh）；记账读写在桌面 SQLite `remote_device_language` 表、键 = 配对设备 id（`ledger_manual_write_then_read_back` / `ledger_second_manual_write_overwrites` / `ledger_new_device_identity_has_no_record`），身份与 IP / origin 无关 | 自动化覆盖 ✓ |
| 3 | 该手机重新配对 → 记录清零 → 回跟随桌面语言（L2） | Rust：`repairing_same_device_clears_language_ledger_record`（同 UA 同来源重配复用同一设备 id → 记录读作无记录）＋ `ledger_clear_removes_record_and_is_noop_for_unknown_device`；前端：`language-toggle.test.tsx`「6.4-3」（清钉住 + 清缓存 → 跟随桌面 zh，浏览器 en 作对照）＋ `language-decision.test.ts` L2 收敛 2 用例 | 自动化覆盖 ✓ |
| 4 | 未配对手机配对页：zh-TW 系统 → 中文；英文系统 → 英文（L1 + M2） | Rust：`unpaired_and_revoked_host_responses_omit_language_fields`（403 不含语言字段——安全闸门不放松）；前端：`language-decision.test.ts`「initMobileLanguage 配对页启动归流」6 用例表驱（断言**零 fetch**，不依赖 /host）＋ 403 → null → 浏览器归流负向触发表；PairPage 文案经 Task 5 抽入 `mobile.pair.*` 17 键（zh/en 同键集），双语渲染由键化 + g1 键集对齐保证 | 自动化覆盖 ✓（真机双系统语言过目并入 §八留真机） |
| 5 | 全部页面中 / 英双语完整可用（g2/g3/g4 清零） | g2 全扫见 §二（文案字面量 0）；g1 键集 2311 对齐；组件测试以 zh 键值断言（与迁移前字面量逐字相同）＋ en 面用例（`MessageComposer.test` 3 条 formatInvokeError / reasonCode 英文路径）＋ `language-toggle` 短码 / aria 双语翻转断言 | 自动化覆盖 ✓；两语全页人工过目**留真机验收** |
| 6 | 标题与 PWA 名称两语下均「Tuvis 远程」不变（M6） | `git diff 2f4fd843..HEAD -- mobile.html public-mobile/manifest.json`：**manifest 零改动**（name 仍「Tuvis 远程」）；mobile.html 仅 `lang="zh-CN"` → `lang="zh"`（C-9/M7 静态兜底，控制器预期内）+ 说明注释，`<title>Tuvis 远程</title>` 原文未动 | 自动化覆盖 ✓ |
| 7 | 桌面手动切英文：无记录手机下次连接 → 英文；有中文记录手机 → 仍中文（M1 + §1.5 独立计算） | 无记录半边：`language-connect.test.tsx` 6.4-1（同场景 1）；有记录半边：`language-toggle.test.tsx`「6.4-7」（经真实 `pushDeviceLanguage` 客户端预置 zh → 连接 manual zh 胜 desktop en → 再点覆盖 en，账本 `[zh,en]` 逐字）；两账本独立：`remote::language.rs` 桌面源（LanguageSource）与设备账本分表分实例 | 自动化覆盖 ✓ |
| 8 | 手机手动选英文 → 桌面端界面语言保持不变（§1.5 / U14 反向） | `language-toggle.test.tsx`「6.4-8」（全部请求仅一次 `POST /m/api/v1/device-language`，无任何桌面语言写副作用；切换后 /host 仍下发 `desktopLanguage:"zh"` 且 `manualLanguage:"en"`） | 自动化覆盖 ✓ |

---

## 五、§6.1 门禁 g1–g6 对照

| 门禁 | 口径 | 结果 / 证据 |
|---|---|---|
| g1 键集一致 | `pnpm check:i18n` 中英目录键集对齐 | ✅ 本轮实测通过：**2311 键**（基线 1861 → 本计划 +450：语言基建 2 + Task 5 66 + Task 6 341 + Task 7 41） |
| g2 无中文残留 | 范围清单全部文件中文文案字面量清零；豁免显式登记 | ✅ §二全扫表：**登记豁免外文案字面量 = 0**；豁免 3 类逐条登记（errcode 镜像 28 / QuestionCard 匹配器 2 / ZH-EN 短码）；注释 1850 行保留未删 |
| g3 双语可切换 | 涉及页面中英两语完整可用，无漏网中文（含 toast / 错误 / placeholder） | ✅ 切换链：Board `languageChanged` 订阅整树刷新 + 全部文案渲染期 `i18n.t` 现取（含挂着的旧回执、错误 toast）；toast / 错误全走 `formatInvokeError` 或错误码→键单点函数；en 面用例（MessageComposer 3 条）实证英文路径。全页两语肉眼过目留真机（§八） |
| g4 a11y 文案不漏 | aria-label 等同步双语化，英文为等价描述 | ✅ Task 5/6 各组 aria 键（addAria / jumpAria / paletteAria / closeTerminalAria / 三态布局 aria 等）全部入 `mobile.*`；语言按钮 `toggleToZh`/`toggleToEn` 四键值逐字断言、随语言翻转（「切换界面语言为英文」↔ "Switch interface language to English"，动作等价描述） |
| g5 新错误文案走既有体系 | 不绕过 `formatInvokeError` 私造硬编码 | ✅ Task 7 收口审计：28 条 MOBILE_NETWORK_FAIL 族经 CODED 正则整句换双语模板；**修复 1 处既有绕过**（uploadAttachment 3 串补 `MOBILE_NETWORK_FAIL:session-attachment:` 码）；错误码→文案单点函数（questionAnswerErrorCopy / approveErrorCopy / appFormPendingCopy）改键现取；新增裸串 = 0 |
| g6 构建与既有测试全绿 | `pnpm check`、`pnpm build:mobile`、`cargo test`（先构建再测试） | ✅ §一命令回归全绿（check / build:mobile / cargo test 3188 过 0 败 / clippy 0 告警 / check:i18n / pnpm test 2121 过 0 败） |

---

## 六、M6 / L1–L3 确认

| 项 | 需求口径 | 确认结果 |
|---|---|---|
| **M6** | 标题「Tuvis 远程」与 PWA manifest 名称不切换、不改（U5） | ✅ manifest.json 基线以来零改动；mobile.html title 原文未动（唯一 diff = `lang` 属性二值化 + 注释，属 C-9/M7 预期改动） |
| **L1** | 未配对配对页拿不到桌面语言，按浏览器语言显示；**安全闸门不放松** | ✅ 行为符合：Rust gate 403 响应不含语言字段（`unpaired_and_revoked_host_responses_omit_language_fields`）——配对前结构性不可得；前端配对页启动归流 6 用例（零 fetch）＋ /host 失败 fail-open（M2 兜底链）；闸门本体（身份签发 / 名额门 / cookie / 限速）零改动，唯一触碰点为配对完成时刻的账本清零（L2，见下） |
| **L2** | 重新配对 = 新设备身份，手动记录清零，回自动跟随 | ✅ 行为符合：`repairing_same_device_clears_language_ledger_record`（upsert 复用同 id 路径也在配对完成时刻清零）＋ 前端清钉住 / 清缓存 / 不回翻收敛测试 |
| **L3** | 会话内容 / 终端原文 / 文件内容原样展示，不翻译 | ✅ 行为符合：本计划仅抽**静态 UI 标签**；`message-render.tsx` 仅 5 个包裹标签键（`mobile.msgRender.*`），消息正文 / 折叠摘要原文、后端下发 reason / 档位 label 原样透传（Task 7 豁免 2）；文件内容 / 终端原文零触碰 |

---

## 七、commit 纪律核对（8 笔）

`git log --format='%h %s' 2f4fd843..HEAD` 实测，任务 1–7 各一笔、subject 与计划钦定 message **逐字一致**：

| # | commit | subject |
|---|---|---|
| 1 | `ea8db850` | feat(desktop): follow system language until manual selection, last manual wins |
| 2 | `50e282ee` | feat(remote): desktop language source, pairing-scoped mobile language ledger and delivery |
| 3 | `9bd54eeb` | feat(mobile): language decision chain (manual > desktop > browser > en) with html lang |
| 4 | `652e4516` | feat(mobile): language toggle beside theme icon, syncs per-device manual override |
| 5 | `d37efe16` | feat(mobile-i18n): extract first-screen and pairing copy to mobile.* keys |
| 6 | `a7bcb525` | feat(mobile-i18n): extract remaining tsx component copy to mobile.* keys |
| 7 | `866db14b` | feat(mobile-i18n): extract ts logic copy, enforce error i18n gate |
| 8 | （本笔） | test: final regression and acceptance matrix for i18n auto-language |

- 任务 1–7 分支改动合计：**66 文件 / +4955 −714**（不含本验收文档）；`docs/**`、`README*`、`CHANGELOG*` 在前 7 笔中零触碰（目录级 diff 核实）；`src/i18n/index.ts` 仅 Task 1 按计划改造（`ea8db850`：移除 `i18next-browser-languagedetector`、初始化显式 `lng`、自动检测结果不落盘——检测/缓存语义对齐 D1 自动跟随契约，属计划内核心交付而非越界改动；旧键迁移代价见 §八·实现侧既定权衡 2）。
- **U7 一次性交付核对**：桌面语言基建（D1/D2）+ 远程语言基建（C-3/C-4/C-5）+ 切换入口（M4）+ 约 306 条文案双语化（M5，键集 1861→2311，+450）全部落在同一分支、随合入 main 一次生效，无分批残留。
- **追记（终审，2026-10-22）**：上表 8 笔之后，全分支终审另产出 1 笔修复 commit（英文复数 `_one` 单数形、列表连接符本地化 `mobile.common.listSep`、`kindImage` 单数化），不在上表之内；本矩阵其余内容不受影响。

---

## 八、留真机验收项与已知限制申报

### 留真机验收（控制器既定裁决：无实机环境面，自动化已覆盖到程序化可验证极限）

1. **托盘 Rust 消费端**（§6.3-4 后半）：真机托盘菜单文案随界面语言即时重建——前端触发链三要素（`refresh_tray` invoke / `language-changed` 广播 / `languageChanged` 重渲染）已由测试锁定，`system_tray.rs` 消费端零改动，实机托盘过目一遍即可。
2. **真实手机 E2E**：真手机浏览器 + 真桌面记账全链过一遍 §6.4 场景 1/2/3/7/8（程序化覆盖已全绿，真机验的是浏览器差异 / 网络切换 / PWA 壳等环境面）。
3. **两语全页人工过目**（g3 的肉眼面）：中 / 英两语下逐页过目确认无漏网中文与排版异常。

### 已知限制申报（均为需求书明文接受项，行为已核符）

| 限制 | 行为确认 |
|---|---|
| L1 未配对页无桌面语言（安全闸门不放松） | 符合（§六），配对完成即跟随桌面语言 |
| L2 重新配对清零回自动跟随 | 符合（§六），既定语义非缺陷 |
| L3 内容 / 终端原文 / 文件内容不翻译 | 符合（§六），零触碰 |

### 实现侧既定权衡（如实申报，非缺陷）

1. **C-7 首帧闸代价**：无手动缓存的设备在慢隧道下首帧最多延后 4 秒（纯底色加载态，不闪变；超时常量 `FIRST_DECISION_TIMEOUT_MS` 可调）——Task 3 申报。
2. **旧版缓存键迁移代价（裁决内）**：升级前在桌面端手动选过语言的用户，升级后回落系统语言一次（`i18nextLng` 不可区分手动 / 自动）——Task 1 申报。
3. **`desktopLanguage: null` 防御形态**：前端从未推送时 /host 下发 null（正常时序不可达，前端启动即推）——Task 2 控制器裁决合规。
4. **冷启动双拉 /host**：首帧闸一次 + Board 挂载一次（均为轻量 DAO 读，换取重挂载时的新鲜账本）——Task 3 申报。

### 范围外提醒（非本计划验收面）

- §6.2 官网场景（W1–W4）在 tuvis-website 独立仓库另行实施（U9/U13：待本分支 README 相关改动合入 main 后发出）。
- 手工清单 H1（GitHub About homepage 改 `https://tuvis.app`，网页操作）与 H2（官网实施提示词发出）待用户择时执行。
