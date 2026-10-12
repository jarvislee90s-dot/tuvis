# codex APP 止血门（`app_form_pending`）底料短记与摘门条件

> 日期：2026-10-12（PR #144 复评 M7 补齐）
> 底料来源：2026-10-11 Task 15 真机手工测试
> 性质：**临时门的证据登记 + 摘门条件**（本档不改任何判据）。门是过渡设施——
> 摘门条件不落档，临时门就会永久化，故单独立档。
> 关联实现：`src-tauri/src/remote/api.rs` 的 `codex_app_form` 与 `APP_FORM_PENDING_*`
> 常量；宪法 `docs/MASTER-PLAN.md` 附 A（二期注入 = CLI、三期 = app-server）。

## 1. 门是什么（现状）

| 入口 | 判据 | 回执 | 门位 |
|---|---|---|---|
| `POST /m/api/v1/session-mode/switch` | 判据单点 | 409 `{error:"app_form_pending", reason:…}` | 任何屏读与注入之前 |
| `POST /m/api/v1/session-mode/menu` | 同上 | 同上 | 同上 |
| `POST /m/api/v1/session-question/answer` | 同上 | 同上 | 同上 |
| `POST /m/api/v1/session-approve` | 同上（复评 I2 并入） | 同上 | 会话定位之后、dialog/映射解析（含屏读）与注入之前 |

- **判据单点**：`codex_app_form(session)` = `agent_type.tool_id() == "codex"` ∧
  `form == ProcessForm::App`。翻转协议臂时改本函数（或其 4 个调用点）即全端点同步。
- **回执文案**：`reason` = 「APP 会话该操作走协议通道，开发中——请在 APP 内操作」
  （移动端原样渲染；前端兜底常量 `APP_FORM_PENDING_FALLBACK` 与之同义）。
- **审计**：`result = refused:app_form_pending`（沿用既有 `failed:{e}` / `refused:xxx`
  同形风格），行携带 `session_id` / `agent_type=codex`，action 沿用既有词表
  （`mode` / `answer` / `approve`）。
- **读侧不受门**：`session-mode` / `session-mode/menu` / `session-question` /
  `session-approve-options` 四个 GET 照常服务（APP 卡片照样能拉到载荷）。
- **CLI 形态零变化**：`form = Cli` 的 codex 会话走既有 CLI 面（端点用例
  `codex_cli_mode_and_question_pass_through_unchanged` /
  `approve_cli_codex_passes_through_unchanged` 钉住）。

## 2. 为什么要这道门（真机证据）

> **证据状态如实申报**：以下四条是 2026-10-11 Task 15 真机手工测试的结论，
> **原始产物（探针日志 / 屏读转录 / 审计导出）未随本档归档**——此前只存在于 PR 描述
> 与代码注释里，本档把它们集中登记并标注未归档，**不伪造原始底料**。补取方法见 §4。

| # | 现象 | 观测 | 解读 |
|---|---|---|---|
| ① | 模式/权限写动作（CLI 键序通道） | 打键报 `AttachConsole 0x80070006`（= `ERROR_INVALID_HANDLE`） | codex APP 是 ChatGPT 桌面 GUI，**没有控制台**——CLI 通道赖以工作的 attach 前提不存在 |
| ② | 屏读 | 「零投递」（读数与目标屏无关） | 没有控制台就没有可读屏，屏读闭环不可能成立 |
| ③ | 问答作答 | `aborted:select`（阶段机在 select 段中止） | 键序通道对 APP 是错误通道，作答动作无法完成 |
| ④ | 目录/文件打开（同批发现，**非 APP 专属**） | `os error 6`（句柄无效） | Release 版 `windows_subsystem = "windows"` GUI 进程继承的 std 句柄不可用（PR #144 前半段修 `resource.rs` 两处；复评 I3 收敛 resume 起窗 / deep_link 兜底 / marker helper 到单点 `window::detached::spawn_detached`）。**根因定案未完成，见 §2.1** |

**结论**：协议臂落地之前，CLI 键序通道对 codex APP 是**错误通道**——此时投递不是
「尽力而为」，而是**确定性无效动作**（还可能污染 APP 内部状态），故止血 = 明确拒绝 +
引导用户回 APP 内操作。

### 2.1 `os error 6` 的根因定案状态（**未完成**，如实申报）

> 复评 I3 追问：若同一 Release 构建下 `resume.rs` 能正常起窗而 explorer 报
> `os error 6`，「继承无效句柄」这个根因叙述就不完整。**本轮只能在 macOS 上读码，
> 无法执行 Windows release GUI（本机无 Windows 主机、无 wine、本机 `~/.tuvis` 库的
> `write_audit` 为 0 行、仓库内也没有 Task 15 的原始产物归档）**，故给出的是源码级
> 分析 + 判定性实验，不是真机结论。

按 rustc 1.97.1 stable（CI/release 均 `dtolnay/rust-toolchain@stable`）的 std 源码：

| 事实 | 出处 |
|---|---|
| `Command::spawn()` 三路 stdio 默认 `Inherit`（`needs_stdin = true`） | `library/std/src/process.rs:1084` |
| 继承句柄不是直接转交：先 `GetStdHandle`，再 `DuplicateHandle(…, inherit = true)` 复制可继承副本 | `sys/process/windows.rs:605-618` |
| 无控制台时槽位为 NULL → `get_handle` 返回 `Err(ERROR_INVALID_HANDLE)` → **句柄置 null 并被放过** | `sys/stdio/windows.rs:68-77` + `sys/process/windows.rs:613-614` |
| **仅当至少一个句柄非 null 才置 `STARTF_USESTDHANDLES`**（源码注释：与 Win7/Win8+ 行为一致） | `sys/process/windows.rs:360-369` |

推论：

1. **「无控制台 ⇒ 继承的句柄是 NULL ⇒ CreateProcess 失败」这条链条不成立**：三路全为
   NULL 时 std 根本不把句柄交给 `CreateProcessW`，按源码不会报错。
2. 真正能产生 `os error 6` 的可达路径是**槽位里留着失效的非 NULL 值**（典型来源：由
   重定向过 stdio 的启动者拉起、启动者随后关闭了那些句柄）→ `to_handle` 的
   `DuplicateHandle` 当场失败，错误码即 `ERROR_INVALID_HANDLE`（此时
   `CreateProcessW` 可能**根本没被调用**）。
3. 因此：**`resource.rs`（修改前）与 `inject/resume.rs:805`（修改前）完全同形**（三路全
   继承 + `Command::spawn()`）——「resume 能起窗、explorer 报错」这一组合**无法**由该
   机制解释（同形即同命）。要么 resume 其实也失败（只是没在真机上点过 / 被 wt→conhost
   二次回退与合并错误文案掩盖），要么当时触发 explorer 失败的条件另有隐情
   （**具体启动方式**导致的失效句柄，例如由安装器/更新器拉起）。
4. **本次 I3 的收敛是「同形加固」而不是「已验证的第二处缺陷」**：置 null stdio 在所有
   路径上都安全，且彻底绕开 `DuplicateHandle` 这条面；但「resume 起窗是否也曾失败」
   仍待真机取证。

**判定性实验（留给真机，30 分钟）**：

1. Release 版**双击**启动（无控制台场景），先点「一键 resume」（`resume.rs` 路径），
   再点资源页「打开目录」（`resource.rs` 路径），**看两条是否同样报 `os error 6`**；
   再把同一个 exe 从**安装器/更新器**拉起（或从一个重定向了 stdio 的启动器拉起）重复
   一次——三条结论组合即可定案「是启动方式相关还是普适」。
2. 用 Process Explorer（或带 `GetStdHandle` 的打点构建）直接看该进程三个 std 句柄槽位：
   NULL / 有效 / **失效的非 NULL**——第三种即印证推论 2。
3. 归档：`research/refs/phase2-消息注入/`。

## 3. 摘门条件（**硬条件，不满足不摘**）

四条写路径（mode / permission / answer / approve）必须**全部**落到协议通道，二者之一：

- **三期 codex app-server 协议臂**覆盖 mode / permission / answer（含 approve）写路径；或
- **H 批 `routing.rs` 的 `Headless(CodexQueue)`** 从消息投递扩到这三条（+approve）写路径
  ——通道判定成为唯一分派点后，按键/文本臂对 APP 会话自然不可达。

摘门动作（**三处同步，缺一即残留**）：

1. **判据单点**：`api::codex_app_form`（及其 4 个写入口的门）——改这一处即全端点同步；
2. **前端文案**：删/改 `APP_FORM_PENDING_FALLBACK`（`src/mobile/api.ts`）与
   `ModeBar` 的 `app_form_pending` 分诊分支（后端不再回该码后它们是死分支，
   留着会把「开发中」误导文案挂在已可用的功能上）；
3. **审计对账**：`refused:app_form_pending` 行应停止新增，回归既有
   `result = ok | failed:{e}`；
4. **复核**：`src/pages/settings.tsx` 的 34a 注释（i18n 文案
   `settings.tools.boardRemoteNote` 讲的是「取消勾选影响看板与远程能力」，与本门**无
   直接文案耦合**；但注释把二者关联，摘门时须复核措辞）。

**摘门验收（必做）**：四条写路径在真机 APP 会话上经协议臂各成功一次、审计落 `ok`；
且 CLI 形态零回归（既有 `codex_cli_*` / `approve_cli_*` 用例保持全绿）。

## 4. 补证方法（Windows 真机，留给后续批次）

1. **① attach 失败码**：注入探针日志（`AttachConsole(pid=…) 失败：0x…`，见
   `src-tauri/src/inject/windows_console.rs`）。注意：门在场时端点不再触达 attach，
   补证须直调探针或临时绕过门（debug 构建）。
2. **② 屏读零投递**：走 `MAM_PROBE_PID` dump 通道（方法同
   `2026-10-08-codex-160-question-屏读底料.md` 的活体取证流程）。
3. **③ 问答 `aborted:select`**：问答端点回执 + `write_audit` 审计行导出。
4. **归档位置**：`research/refs/phase2-消息注入/`（项目探测产出惯例，见项目
   `AGENTS.md` 的 `win-console-inject-probe` 技能条目）。

## 5. 未决登记（只登记，本轮不改）

- **门位窄一档**：问答入口的门在 `question_scan_sync` 之后——无待决问答时回
  `no_question` 而非 `app_form_pending`。安全侧无害（无屏读、无注入），属**需求取舍**，
  待用户裁决。
- **M5**：`/session-send` 的 403 `not_injectable` 与 `reasonCode:"app_form"` 两条诊断码并存。
- **M6**：APP 卡片 GET 仍会盲读屏（读侧不受门的已知代价）。
- **M3/M9**：`FakeInjector` 未覆写 `Injector` 的默认方法
  `locate_and_inject_draft_spec` / `locate_and_send_backspaces_spec`（默认返回 Err 且不
  记录）＝零投递断言的盲区；问答新用例只走通道 A（mark+payload），而 codex APP 的真实
  在场通道是通道 B。
