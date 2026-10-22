# Codex APP 正文与文件产物关联 · 验收记录

> 计划任务 8 交付物。日期：2026-10-20（任务 8 执行于 2026-10-21；合并前审查修复轮与本文更新于 2026-10-21）。
> 分支：`codex/message-artifacts`；实施基线：`origin/main@0ad8d763e5bdc55e0934ef82af2eedd1b91cbc15`；
> PR 整理基线：`origin/main@fd75142fd6fdc07d674cb70eb01c3f8e5f75f068`（PR #175 合并提交）；
> 本任务起点（BASE）：`24c93566dc03c6eada018b0f2d6178e32396e664`。
> 记录更新时的 HEAD：`131de5285324a282dc884dbc43a7f928bc51edf6`（最终 HEAD 可能因本文档提交前移）。
> 规格依据：`../specs/2026-10-20-codex-message-artifacts-design.md`（§六 完成标准）。
> 字段依据：`2026-10-20-codex-artifact-field-evidence.md`。

**本文的结论分两层，不可相互替代：**

- **自动化交付：PASS。** §四 所列命令均在对应测点实跑，退出码与计数见 §四（表 A 为任务 8，
  表 B 为合并前修复轮；全量 lib 门禁由控制器另行重跑，本文不为修复轮新造全量数字）。
- **产品验收：未完成。** Windows 目标 Desktop build `26.1013.4894.0`、macOS 真实 Desktop IPC
  附件样本、真实移动端（手机/平板浏览器）闭环均 **NOT EXECUTED**，原因见 §六、§七。
  macOS 安装包的静态字段读取（证据记录 §一／§二）不是 Windows 运行证明，也不是实机产品验收。

## 一、证据分级与验收边界

| 等级 | 含义 | 本轮是否取得 |
| --- | --- | --- |
| AUTOMATION | 本仓库自动化测试（Vitest / Rust 单元与集成测试）在本机实跑通过 | 是（§四） |
| STATIC-FIELD | macOS 安装包 `app.asar` 的只读静态字段读取（前序任务取证） | 是（仅字段名，不是运行时样本） |
| SYNTHETIC | 按字段证据构造的合成夹具（`artifacts/cases.synthetic.json`） | 是；**未提升为 OBSERVED** |
| PRODUCT | 真实 Tuvis + 真实目标 Desktop build + 真实移动端的端到端验收 | **否** |

本轮执行的硬约束（与 spec §三／§五一致）：不读取私人会话正文、不触发真实会话写动作、
不修改真实用量账本、不修改 `docs/MASTER-PLAN.md` 与冻结 IPC spec。因此**没有**取得真实
IPC 附件样本，也**没有**做任何实机点击验收；本文不把 AUTOMATION 写成 PRODUCT PASS。

## 二、执行环境

| 项 | 实测值 |
| --- | --- |
| 机器 | Apple Silicon（`uname -m` = `arm64`） |
| 操作系统 | macOS `27.0.1`（build `26A434`） |
| 本机 ChatGPT.app | `26.1013.40934`（`CFBundleVersion` `20770`），只读读取 Info.plist |
| Windows 目标 Desktop | `26.1013.4894.0` —— **本机没有 Windows 设备，无法启动** |
| Node / pnpm | Node `v26.8.1` / pnpm `11.23.0` |
| Rust | rustc `1.97.1` / cargo `1.97.1` |
| 移动端设备 | **无**（只有 jsdom 自动化；无手机、平板或模拟器） |

## 三、任务 → spec 覆盖映射（八个任务）

| 任务 | 提交 | 主要交付物 | 覆盖 spec | 自动化证据（本轮实跑覆盖） | 状态 |
| --- | --- | --- | --- | --- | --- |
| 1 输入形态取证与附件契约 | `150fa97f` | `codex-artifact-field-evidence.md`、合成夹具、附件 DTO 契约 | §3.1、§二.2、§3.2 字段定义 | 字段证据与夹具为前序任务产物；本轮由全量门禁与 Task 3 用例覆盖其契约 | 完成（真实 IPC 样本缺口保留） |
| 2 共享文件身份与 `fileKey` | `12f7ee70` | `src-tauri/src/file_identity.rs`、`remote/files.rs`、`remote/api.rs`、`src/mobile/api.ts` | §一.2、§二.1、§2.5、§3.2 | `cargo test --lib`（含身份与文件池用例）、`pnpm test`（`tests/mobile/api.test.ts`） | 完成 |
| 3 结构化附件、纯附件消息、有界投影 | `8de86f5d` + `c3e153fc` | `codex_desktop_ipc/artifacts.rs`、`types.rs`、`mirror.rs`、`codexIpcApi.ts` | §一.7、§3.2、§3.3 | `cargo test --test codex_desktop_ipc` 67 例（含 14 个附件用例；修复轮新增 6 条 token v2 用例） | 完成 |
| 4 同代只读消息分页 | `e5f526e6` | `GET /m/api/v1/codex/thread/messages`、`types.rs`／`mirror.rs`／`adapter.rs` | §4.3、§2.5 | `codex_ipc_remote` 61 例、`codex_desktop_ipc` 67 例、`codex_control_remote` 36 例 | 完成（R1/R2 由 token v2 关闭；已知边界见 §八.1） |
| 5 实时尾窗 + 有限历史页 | `7b12868a` + `3ea5a4dd` | `codex-history.ts`、`codexIpcApi.ts`、`useCodexThread.ts` | §4.3、§三 预算、§一.5/§一.6 前置 | `pnpm test`：`codex-history` 25 例、`useCodexThread` 24 例（修复轮新增 11 条保留缓冲纯函数用例与 4 条接缝 hook 用例） | 完成 |
| 6 正文链接、附件卡片、当前文件预览 | `16beec0f` + `7d6ba498` | `ProjectedArtifacts.tsx`、`useArtifactImages.ts`、`SessionDetail.tsx`、`FilePreview.tsx`、`message-render.tsx`、双语文案 | §一.1、§4.1、§五 失败态 | `pnpm test`：`ProjectedArtifacts` 16 例、`SessionDetailArtifacts` 10 例、`FilePreview` 21 例、`useArtifactImages` 9 例 | 完成（R4 已修；布局 remount 限制见 §八.8） |
| 7 文件池来源与五页反向定位 | `2306cc2e` + `7446c4dd` + `24c93566` | `artifact-links.ts`、`useArtifactJump.ts`、`FilePanel.tsx`、`FilePreview.tsx`、`SessionDetail.tsx` | §一.4/§一.5/§一.6、§4.2、§4.4 | `pnpm test`：`artifact-links` 29 例、`useArtifactJump` 26 例、`FilePanel` 47 例、`SessionDetailArtifacts` 10 例 | 完成（R3／R5 已修；遗留 minor 见 §八） |
| 8 完整验收与交付记录 | `93e28b5e` + `f4fedbb4` + `fb01022b` + `26b95e69` + `131de528` | 本文件、字段证据 §五 更新、两处清理 | §六、§3.1、§五 | 本文件 §四 表 A／表 B 的全部命令 | **交付完成；产品验收未执行**（Windows/macOS 实机/移动端真机均未执行，见 §六、§七） |

**spec §六 回归场景对照：** 五类结构化来源、外链、缺失路径、纯图片/纯文档消息 → 任务 3 用例
（`artifacts_extract_all_documented_sources_without_inventing_success` 等）；正文链接打开当前
文件 → 任务 6 用例；同路径不同写法只合并一次 → 任务 2/7 用例（`artifact-links` 身份合并组）；
回合内编号／12 图／6 chip／分页边界／超大元数据 → 任务 3/4 用例；当前页与第 2—5 页命中、
五页未命中最多四次读取、最后一次引用优先 → 任务 7 用例；已加载更早历史不扩大五页、仅正文
链接也能定位 → 任务 5/7 用例；历史未加载／预算拒绝／游标失效／SSE 重连／owner 换代／跨会话
迟到响应 → 任务 4/5/7 用例；窄屏预览返回正文、来源与修改状态、视口加载与资源释放 → 任务 6
用例。服务端附件省略提示、合成文件池行 `modified` 语义与文件池来源摘要 → 任务 6/7 的 R3/R4/R5
修复轮用例。逐条证据见 §五。

## 四、自动化门禁实跑记录

本记录含两批实跑：**表 A** 为任务 8 交付时（`93e28b5e`..`f4fedbb4`）的门禁，**表 B** 为
合并前审查修复轮（2026-10-21，`fb01022b`..`131de528`）的复跑。两批均在本机实跑；修复轮
未重跑的命令在表 B 中注明测点或来源。日志保留在 `/tmp/t8-*.log`（仅本机，不入库）。
`src-tauri` 命令工作目录为 `src-tauri/`。

**表 A：任务 8 交付门禁（修复轮前测点，保留原始记录）**

| 命令 | 退出码 | 真实结果 |
| --- | --- | --- |
| `pnpm build:mobile`（第 1 次，前置） | 0 | `dist-mobile/` 产物生成（`✓ built in 663ms`）；`rust-embed` 编译期需要该目录 |
| `pnpm test` | 0 | 157 个测试文件；**1726 passed / 5 skipped**（共 1731），22.14s |
| `pnpm check` | 0 | `format:check` 全部符合 Prettier；`lint` 无输出（0 error）；`check:i18n` **1494 键对齐**；错误码门禁 **283 码 / errors 段 284 键（legacy 5）/ 未迁移启发式 446**；`tsc && vite build` 成功 |
| `pnpm build:mobile`（第 2 次，`pnpm check` 之后） | 0 | 成功（`✓ built in 421ms`）。注：`pnpm check` 只构建桌面 `dist/`，不重建 `dist-mobile/`；仍重跑一次以确保最终工作树源码与移动端产物一致 |
| `cargo fmt --all -- --check` | 0 | 无输出（无差异） |
| `cargo clippy --all-targets -- -D warnings` | 0 | `Finished dev profile`，0 warning |
| `cargo test --lib` | 0 | 运行 2656 例：**2618 passed / 38 ignored / 0 failed**，12.40s |
| `cargo test --test codex_desktop_ipc` | 0 | **61 passed** / 0 failed，9.19s |
| `cargo test --test codex_ipc_remote` | 0 | **61 passed** / 0 failed，29.73s |
| `cargo test --test codex_control_remote` | 0 | **36 passed** / 0 failed，0.21s |

> 表 A 的前端 1726 / 5、全量 lib 2618 / 38 与各集成套件计数均为**修复轮前**测点，不代表当前
> HEAD 的计数；修复轮后的前端与集成计数见表 B。

**表 B：合并前审查修复轮实跑（2026-10-21）**

| 命令 | 测点 | 真实结果 |
| --- | --- | --- |
| `pnpm exec vitest run`（全量） | `131de528`（R2 接缝提交后） | 157 个测试文件；**1753 passed / 5 skipped（1758）** |
| `pnpm exec vitest run tests/mobile/useCodexThread.test.tsx tests/mobile/codex-history.test.ts tests/mobile/codexIpcApi.test.tsx` | `131de528` | **79 passed**（`useCodexThread` 24、`codex-history` 25、`codexIpcApi` 30） |
| `pnpm exec vitest run`（R3/R4/R5 聚焦 5 文件） | `26b95e69` | **128 passed**（`ProjectedArtifacts` 16、`SessionDetailArtifacts` 10、`FilePanel` 47、`artifact-links` 29、`useArtifactJump` 26） |
| `pnpm check` | `26b95e69` | `format:check` 全部符合 Prettier；`lint` 0 error；`check:i18n` **1495 键对齐**；错误码 **283 码 / errors 284 键（legacy 5）/ 未迁移启发式 446**；`tsc && vite build` 成功 |
| `pnpm exec tsc --noEmit` | `26b95e69`、`131de528` | exit 0（无输出） |
| `pnpm exec eslint` / `prettier --check`（各自改动文件） | `26b95e69`、`131de528` | exit 0（0 warning / 0 error；所有文件符合 Prettier） |
| `cd src-tauri && cargo fmt --all -- --check` | `fb01022b` | exit 0（先 `cargo fmt --all` 落盘，再 check） |
| `cd src-tauri && cargo clippy --all-targets -- -D warnings` | `fb01022b` | exit 0，`Finished dev profile`，0 warning |
| `cd src-tauri && cargo test --lib services::codex_desktop_ipc` | `fb01022b` | **77 passed / 0 failed / 0 ignored**（2579 filtered）——仅该模块，不是全量 lib |
| `cd src-tauri && cargo test --test codex_desktop_ipc` | `fb01022b` | **67 passed / 0 failed / 0 ignored** |
| `cd src-tauri && cargo test --test codex_ipc_remote` | `fb01022b` | **61 passed / 0 failed / 0 ignored** |
| `cd src-tauri && cargo test --test codex_control_remote` | `fb01022b` | **36 passed / 0 failed / 0 ignored** |
| `git diff --check` | `26b95e69`、`131de528` | 无输出（干净） |

> Rust 计数在 `fb01022b` 测点；其后 `26b95e69` 与 `131de528` 均未改 Rust，套件内容不变。
> **全量 lib 与修复轮终态复核（控制器实跑，测点 `131de528`）**：`cargo fmt --all -- --check`
> exit 0、`cargo clippy --all-targets -- -D warnings` exit 0、`cargo test --lib`
> **2618 passed / 38 ignored / 0 failed**（2656 run，12.61s）、`codex_desktop_ipc` **67**、
> `codex_ipc_remote` **61**、`codex_control_remote` **36**；前端 `pnpm exec vitest run`
> **157 文件 / 1753 passed / 5 skipped**、`pnpm exec tsc --noEmit` exit 0、`pnpm check:i18n`
> **1495 键**、`pnpm check` exit 0（含 `tsc && vite build`）。表 A 的 2618 / 38 是修复轮前
> 测点，与此处数值一致但属不同测点，两者都按实测登记，不互相替代。

**关于 `pnpm test` 的 5 个 skipped：** 它们全部来自 `tests/pet/usage-contract-parity.test.tsx` 的
`describe.skipIf` 用量契约对账套件（`tests/fixtures/usage/` 四份真实返回 fixture 不齐备时整组
skip；本机未放 fixture）。该文件单独实跑为 **1 passed / 5 skipped**（常绿结构组通过、真实返回组
跳过，实测 `✓ tests/pet/usage-contract-parity.test.tsx (6 tests | 5 skipped)`），与本功能无关，
不是被禁用的功能测试。

**补充聚焦与取证命令（任务 8 与修复轮实跑）：**

| 命令/操作 | 退出码 | 结果 |
| --- | --- | --- |
| `pnpm vitest run tests/mobile/codex-history.test.ts` | 0 | 25/25 passed（任务 8 时为 14/14；R2 接缝新增 11 条保留缓冲纯函数用例） |
| `pnpm vitest run tests/mobile/useArtifactJump.test.tsx` | 0 | 26/26 passed |
| `cargo test --test codex_desktop_ipc messages_page::`（修复后） | 0 | 10 passed（修复前 RED：3 passed / 7 failed；含 R1/R2 的 6 条新用例与既有分页用例） |
| 测试包含型 `tsc -p <临时配置，include tests/mobile/codex-history.test.ts>`（修复前） | 2 | `tests/mobile/codex-history.test.ts(86,62): error TS2345`（可空返回值直传 `mergeCodexHistory`） |
| 同上（收窄修复后） | 0 | 无错误。该错误不被 `pnpm check` 暴露：仓库 `tsconfig.json` 的 `include` 只有 `src` |
| 临时把 `useArtifactJump` 的会话哨兵从 `useLayoutEffect` 改回 `useEffect` 后跑该测试文件 | 0 | 26/26 passed —— 证明现有「迟到响应不落地」回归在 React 19 `act` 下**不具区分度**（见 §八.7） |

**已知前置条件（构建事实，不是失败记录）：** `remote/server.rs` 用 `#[folder = "../dist-mobile/"]`
在编译期嵌入移动端产物，所以 `cargo test --lib` 必须在 `pnpm build:mobile` 之后运行；本轮按
`build:mobile → 前端门禁 → 后端门禁` 的顺序执行，未出现该类环境失败。

**能力门复核：** 本分支未修改任何能力档定义（`git diff --name-only` 中无 capability 相关文件，
`docs/capabilities.json` 未改）；新增分页端点挂在既有 gated nest 上，复用远程鉴权与
`find_session_sync`，并由 `thread_messages_rejects_bad_parameters_and_missing_auth`、
`thread_messages_cannot_read_another_thread` 覆盖。未翻转任何未实测能力。

## 五、五项复核重点的自动化证据

| # | 复核重点（计划 §复核重点） | 覆盖的自动化证据 | 能证明什么 / 不能证明什么 |
| --- | --- | --- | --- |
| 1 | 同一路径内容被覆盖、预览仍开着时再次点击，必须重读当前文件，不能命中旧图片缓存 | `SessionDetailArtifacts.test.tsx`「预览已打开时再次点同一路径的附件：重新读取并显示当前内容」；`FilePreview.test.tsx`「布局（mode）切换不重新读取；显式打开计数 `refreshKey` 递增才重读并显示新内容」；`tests/mobile/FilePreview.test.tsx:364`「关闭/卸载时取消在途读取（AbortSignal 传递）」 | 证明刷新键只随**显式打开**递增、每次打开都重新走文件读取，且读取可被取消；**不能**证明真实磁盘文件在预览打开期间被覆盖后的实机效果（无真实 Desktop 会话） |
| 2 | 文件仅在普通正文链接中出现、没有结构化附件时也能在限定范围内定位 | `artifact-links.test.ts`「仅正文链接（无结构化附件）也能命中」「同消息多处正文引用：取最后一个对应节点」「不按 basename 推断：同名不同目录不命中」「旧载荷条目缺 `fileKey`：退化为 path 匹配」；`useArtifactJump.test.tsx`「未知来源（目标无 fileKey、正文无已知链接）也能发起查找，不预造 messageId」；`SessionDetailArtifacts.test.tsx`「仅正文链接出现的文件也能从预览定位：窄屏先切回消息区，可返回原预览」 | 证明既有链接渲染器的路径集参与匹配，且不新增文件名猜路径；**不能**证明 Windows 实机正文解析形态 |
| 3 | 已缓存第六页或更多旧消息时仍只搜五页，且第五页末条能命中 | `useArtifactJump.test.tsx`「已缓存的第六页不搜索；缓存满五页时零读取后按未命中结束」「第五页最后一条消息也能命中（四次更早读取用满）」「五页未命中：恰好四次更早读取后停止，不继续加载」；`codex-history.test.ts`「bounded_window_ignores_fifth_earlier_page」「cap_keeps_newest_four_pages」；四次读取硬上限由「回锚页持续重复：恰好在第四次读取后以读取失败收尾」钉住 | 证明五页计**不同页**、缓存不扩大范围、读取调用恒 ≤4、第五页末条可命中；**不能**证明真实镜像在字节预算下丢弃页后的实机回锚行为（该链路由 Task 5 用例与回锚回归共同覆盖） |
| 4 | 查找过程中到达 SSE 新帧、历史改写或切换会话，不能拼接旧代历史、跳错消息或留下过期高亮 | `useCodexThread.test.tsx`「sse_same_token_keeps_pages」「token_change_clears_pages_and_drops_late_page」「session_switch_clears_pages_and_drops_late_page」「history_rewrite_clears_cache_and_reanchors」「cache_caps_four_pages_and_reanchors_without_gap」与「R2 尾窗接缝保留」4 例（`window_rollover_keeps_dropped_message_with_page`、`window_rollover_keeps_dropped_message_without_page`、`token_change_clears_retained`、`session_switch_clears_retained`）；`codex-history.test.ts`「R2 尾窗接缝」11 条（保留追加/去重、页与缓冲三种落位、条数/字节上限）；`useArtifactJump.test.tsx` 失败/失效用例组（`loading`→`history_incomplete`、`request_failed`→`load_failed`、`cursor_invalid`、`generation_mismatch`/`stale_response`→`generation_invalid`）与「会话切换与迟到页同拍」；Rust `codex_ipc_remote::thread_messages_history_rewrite_invalidates_old_page`、`codex_desktop_ipc::client_status_changed_invalidates_all_threads_of_owner`，以及 token v2 的 6 条镜像回归（`same_id_text_rewrite_in_settled_turn_invalidates_tokens`、`same_id_artifact_change_in_settled_turn_invalidates_tokens`、`deletion_of_settled_turn_changes_projection_token`、`append_of_new_turn_after_tail_saturation_keeps_tokens`、`streaming_growth_of_newest_agent_message_keeps_projection_token`、`tail_window_rollover_alone_keeps_projection_token`） | 证明普通追加、最新回合流式增长与仅尾窗前移都保持投影 token 稳定（不再无故清页），同一 id 的正文/附件改写与已结算回合删除使旧页 token 失效；同 token 下滚出尾窗的消息经保留缓冲接回，token 换代/切会话清空缓冲，旧页迟到响应不落地，失败不被伪装成未命中。**但** token v2 按设计只观察已结算回合（§八.1），最新回合内的原位改写不在检测范围；会话切换回归在 React 19 `act` 下不具区分度（§八.7），实现正确性靠 commit 阶段失效的代码结构，测试**不能**捕获“改回被动 effect”的回归；真实 SSE 时序未实机复现 |
| 5 | 纯附件开场消息、超大附件列表、缺路径和非 HTTP 图片引用，不得整条消失、突破预算或被误报为可预览 | `codex_desktop_ipc`：`artifacts_attachment_only_formal_replaces_opening_with_same_identity`、`artifacts_opening_fallback_survives_empty_formal_user_content`、`artifacts_opening_fallback_survives_skill_only_formal_user_content`、`artifacts_missing_and_unsupported_references_have_bounded_reasons`（`missing_path`/`unresolved_file_id`/`missing_url`/`data_url`/`unsupported_url_scheme`/`url_too_long`，且 2 MB data URL 与 fileId 不进 DTO）、`artifacts_bound_fields_counts_and_preserve_full_turn_image_count`（128 附件、`omittedArtifactCount=5`、1024 字符摘要、16 KiB 路径/身份键）、`artifacts_serialized_budget_counts_escaping_and_latest_metadata`、`artifacts_long_summary_keeps_image_cards_renderable`；`ProjectedArtifacts.test.tsx`「缺路径 / fileId / data URL / 其他 scheme / 超长均给可理解占位，且不渲染 base64」「外链仅 HTTP(S)，沿用 target=_blank + rel=noreferrer；不做任何下载」「13 张图片：渲染 12 张卡、汇总只出现一次、第 13 张不请求」「7 个 fileChange：至多 6 个 chip + 一条汇总 + 文件面板入口」「图片解码失败 → 占位并回收 object URL」；`tests/mobile/useArtifactImages.test.tsx:84`「元素未进入视口 → 不发任何请求」、`:133`「滚出视口（isIntersecting=false）：最后一个消费者释放资源；再次进入重新获取一次」、`:159`「最多两个请求在途；完成一个才派发下一个」；`artifact-links.test.ts`「既有 tool_write/modified 条目：未验证的 file_change 引用不得撤销已修改标志」「未验证写来源的合成行保持 modified=false（FilePanel 不因此加粗）」「补充条目：来源映射；未验证的 file_change 不标已修改」；`FilePanel.test.tsx`「未验证的合成写来源行（tool_write + modified=false）不加粗，状态仍原样展示」；`ProjectedArtifacts.test.tsx`「服务端预算省略提示」组（`omittedArtifactCount>0` 每条消息只出现一次、附件全部被省略仍可见、UI 12 图上限与服务端省略并存）；`SessionDetailArtifacts.test.tsx`「服务端预算把附件全部省略：消息级提示仍进入消息列且只出现一次」 | 证明纯附件消息保留、超限有显式省略或原因码、外链不代理不下载；并证明服务端预算省略以消息级提示可见、与 UI 12 图/6 chip 限额分开，附件全被省略时消息不整条消失，且合成文件池行不会把 failed/进行中的 `file_change` 标成已修改、也不撤销后端已确认的 `modified:true`。**不能**证明省略提示能指出具体被省略的附件（只给数量），也不能证明目标 build 的真实枚举与运行时空值（合成夹具，见字段证据 §三） |

## 六、平台证据表（自动化与产品验收分列）

| 平台 | 设备 / 环境 | 版本 | 证据类型 | 结论 |
| --- | --- | --- | --- | --- |
| Windows | 无 Windows 设备（本机为 macOS） | 目标 Desktop `26.1013.4894.0` | **产品验收（实机）** | **NOT EXECUTED** |
| macOS | 本机 Apple Silicon，macOS `27.0.1`（`26A434`） | ChatGPT.app `26.1013.40934`（`20770`） | 自动化（Vitest + Rust，§四 表 A／表 B） | **PASS**（自动化层） |
| macOS | 同上 | 同上 | 静态字段读取（前序任务，只读 `app.asar`） | **PARTIAL**：仅字段名与字段读取位置有据；运行时取值未实测 |
| macOS | 同上 | 同上 | **产品验收（真实 Desktop IPC 附件样本 + 实机点击）** | **NOT EXECUTED**：不读取私人会话、不触发真实写动作，故没有 live IPC 附件样本 |
| 移动端 | 无真机 / 无模拟器（仅有 jsdom） | — | 自动化（`pnpm test` 中的移动端组件与 hook） | **PASS**（自动化层） |
| 移动端 | 无设备 | — | **产品验收（真机闭环：正文/卡片 → 预览 → 定位）** | **NOT EXECUTED**：无可用设备 |
| Windows | 无 Windows 构建环境 | — | 构建门禁（`CARGO_BUILD_JOBS=2` 的 Windows 构建） | **NOT EXECUTED**：计划中的该参数仅在 Windows 构建时适用，本轮未执行 Windows 构建 |

## 七、未执行事项与原因

1. **Windows 目标 build `26.1013.4894.0` 的产品验收（含三张生成图、一个上传文档、一张上传
   图片、五页边界定位、同路径覆盖后再打开、失败占位）**：无 Windows 设备与该 build，无法
   启动 Desktop，也无法驱动其 IPC；本机是 macOS。
2. **macOS 真实 Desktop IPC 附件样本**：取得样本需要读取既有私人会话或触发真实会话动作，
   本轮约束明确禁止；因此字段证据保持 STATIC-FIELD，合成夹具保持 SYNTHETIC。
3. **macOS 实机产品验收（真人点击正文/卡片/预览/定位）**：本轮为无人值守自动化，不做实机
   点击；不把自动化结果写成产品通过。
4. **移动端真机闭环**：无手机/平板/模拟器；只有 jsdom 自动化。
5. **Windows 构建并发门禁（`CARGO_BUILD_JOBS=2`）**：未执行 Windows 构建，故该参数无实跑记录。
6. **真实 SSE/历史改写/owner 换代的实机时序**：仅有自动化桩与集成测试；修复轮新增的 token v2
   （同 id 改写、饱和后追加、最新回合流式增长、仅尾窗前移）与接缝保留用例同样是合成夹具，
   真实镜像与 Desktop 的时序组合未实机复现。
7. **同路径文件在预览打开期间被真实覆盖的实机效果**：自动化用 mock 文件读取证明“再次打开
   会重读”，未在真实磁盘上做覆盖实验。
8. **跨工具统一 `artifacts` 能力**：spec §2.3 明确不在本期范围，见 §十 的本地 issue 草稿
   （未对外创建）。

## 八、已知限制与遗留 Minor（修复轮后）

1. **活动 Turn 末条消息的流式豁免**：只有「状态为 `inProgress` 且在 canonical 最后一个」的
   Turn 的**末条消息**不参与比对（无法区分正常补全与同帧改写）；其余所有 Turn——包括已结算
   的最新 Turn——逐条比对。`inProgress` 这一取值目前只来自观测夹具，目标 build 的真实状态值
   属 R6，未核验；若真实取值不同，活动 Turn 会按已结算处理（每帧换代，代价是历史页被清）。
2. **每帧内容指纹成本**：`project()` 在每次 SSE 广播/轮询、`messages_page()` 在每次分页，都会
   对完整 canonical 投影（不止尾窗 500 条）中已结算回合做一遍逐字节 FNV-1a 与附件 JSON
   序列化；与既有 `project_full_messages` 同阶，但新增一遍线性读取，debug 构建更明显；未做
   增量缓存（同上 §七.1）。
3. **保留缓冲的前提与已知退化**：缓冲上限 2000 条 / 1 MiB，超出从最旧一侧丢弃。保留只在
   **证明**了单调前移时成立（不缩短、或带服务端 `truncated` 且保序后缀/前缀重叠、新增段不含
   旧 id），删除/替换/重排一律清空。已知退化：服务端确实裁剪（`truncated`）但新旧尾窗**完全
   无重叠**时，无法证明连续性，只能清空缓冲——可见接缝在该极端场景下消失（
   `retain_truncated_without_overlap_clears`）。
4. **分页缺口时定位的代价**：retained 被字节预算驱逐中段后，旧缓存页与前沿之间会出现缺口；
   选页只接受与前沿相邻的页，定位必须从真实前沿重锚补页（多一次读取）。四次更早读取用尽仍
   未补齐时按失败终止，不越过缺口取旧引用（`useArtifactJump` 的 `page_repeated`/`load_failed`）。
5. **Task 4 次要项**：`end` 仍返回非空 `historyToken`（与字段注释“不可继续时为空串”漂移）；
   字节预算“多条累计中途截断”与附件省略分支缺独立用例；`unavailable/history_unreadable`
   缺正作用例；并发首请求可能跳过 hydrate 返回 `mirror_unavailable`（需重试）；跨线程 token
   被报成 `history_rewritten`（无语义泄露）；等价全量 snapshot 重发也递增 `buildGeneration`。
   原「缺‘尾部追加移动尾窗、旧 token 仍有效’的独立用例」已由 R1/R2 的
   `append_of_new_turn_after_tail_saturation_keeps_tokens` 与
   `tail_window_rollover_alone_keeps_projection_token` 覆盖，移出本表。
6. **Task 5 其余**：页层 403 抛 `ApiError` → `unavailable/request_failed`；缓存预算驱逐时该页
   仍返回调用方但从缓存与游标链丢弃；abort 桩改为真实响应后，`useCodexThread.ts` 中“迟到但
   已 resolve 的页”分支失去覆盖；4 MiB 预算按 `JSON.stringify` 后的 UTF-8 字节计算
   （`codexHistoryPageBytes`），量的是序列化体积而不是 JS 堆占用——对象头/字符串驻留未计入，
   预算偏保守但不等同内存上限。
7. **Task 7 迟到响应回归不具区分度（本任务实测确认）**：`useArtifactJump.test.tsx` 的
   「会话切换与迟到页同拍」在 React 19 `act` 下，无论哨兵用 `useLayoutEffect` 还是
   `useEffect` 都通过（本任务把实现临时改回 `useEffect` 后 26/26 通过，见 §四）。原因是
   `act` 在作用域结束时同步冲刷被动 effect，掩盖了“commit 阶段失效 vs 被动 effect 失效”的
   差别。未改架构（review 已确认 `useLayoutEffect` 修复本身正确）；若要真正区分，需要
   scheduler 级控制（例如 `flushSync` 或自定义 root 的无 `act` 提交序列），本任务未做。
   已在本表登记为测试盲区，不把它当作已覆盖的回归。
8. **Task 6 布局切换**：宽屏 split ↔ fullscreen 是两棵不同子树，会卸载并重挂消息列 →
   图片卡 detach、重新取最多 12 张缩略图，并伴随一次 `FilePreview` 挂载读取（继承行为被
   “最后一个消费者释放”放大）。未在任务 8 修架构。
9. **Task 6 其余 Minor**：无 `IntersectionObserver` 的环境回退为立即加载；原始 `changeKind`
   未翻译、`sourceStatus` 仅落 data 属性；尾窗从回合中段起算时该回合可能 0 张卡但汇总行仍称
   “前 12 张”；`showFilePanel` 丢弃已打开预览；`mention` 标为“上传”属解释性标注；
   `FilePreview` 既有错误文案仍为硬编码中文（沿用现状）；M4 未覆盖“图片卡跨消息溢出”的
   组合（12 图上限只在单回合内断言，跨消息拼装未单测）；M5 测试辅助里的 StrictMode store
   复活 hack 仍保留（仅测试代码，不在产品实现中）。原「`omittedArtifactCount` 有意不展示」
   已由 R4 关闭，移出本表。
10. **服务端省略提示只给数量，不列明细**：`omittedArtifactCount > 0` 的消息级提示只说
    「另有 N 个附件因服务端预算未下发」，不说明被省略条目的来源、路径或类型；被 UI 12 图 /
    6 chip 上限隐藏的第 13 张图、第 7 个 chip 不在 DOM 中，从文件池/预览定位到这些引用时
    只能高亮所在消息，不能高亮隐藏节点（沿用任务 7「命中节点不可用时只高亮消息」）。
11. **文件池摘要边界**：`ArtifactFileEntry.summary` 只对**有结构化附件引用证据**的行计算
    （同回合提示词 → 附件自身摘要 → null），纯正文链接与后端独有行保持 `null`；24 字封顶，
    `title` 承载的也是封顶文本，不是完整提示词；单行 `truncate` 在极窄屏可能只显示前几个字
    （`fix-r3r4r5.md` §五、§六.3）。
12. **Task 3 遗留**：`file_key_too_long` 在 path 仍存在时也会置位（Task 6 不得仅凭该码隐藏带
    path 的卡片）；缺“仅正文截头也置 `truncated=true`”的独立用例；双回合用例的
    `omittedArtifactCount == 0` 分支为空断言；`content: []` + 匹配 clientId 的去重排列未单测。
13. **Task 7 遗留**：命中后等待目标 DOM 最多 1s 后静默放弃，测试未断言具体节点与滚动目标；
    `useArtifactJump.ts` 模块头原先未写明四次读取上限（本任务已补注释，行为未改）。
14. **门禁盲区**：仓库 `tsconfig.json` 只 `include: ["src"]`，测试文件不在类型检查内；本任务
    修复的 `TS2345` 正是因此长期不被门禁发现（见 §四）。是否把 tests 纳入 tsconfig 是独立
    工程决策，未在本任务扩大范围。
15. **真实字段证据缺口**：`artifacts/cases.synthetic.json` 与所有派生状态均为 SYNTHETIC；
    本轮没有新增 OBSERVED 附件样本，也没有把任何 SYNTHETIC 输入升级为 OBSERVED（见字段
    证据记录 §五）。

## 九、新增英文文案核对

任务 6/7 在本分支新增 49 个 `mobile.artifacts.*` 键（其中定位相关 16 个，位于
`mobile.artifacts.locate.*`，含 4 个 `failure.*`；不存在顶层 `mobile.locate.*` 命名空间）；
R4 修复轮再新增 1 个 `mobile.artifacts.serverOmitted`（zh/en 各 1 键）。核对方式与结果：

- 双语 JSON 扁平化逐键比对：`zh` 1495 键 / `en` 1495 键，键集完全相同，**新增 50 键、删除 0 键**；
- 新增键中 **英文值等于中文值的数量 = 0**（不是照抄中文）；
- 新增键的 `{{placeholder}}` 集合完全一致（**0 处不匹配**）；
- 既有键值改动数 = 0（本分支只新增键，没有改动旧文案的 zh/en 值）；
- 实现者逐条核对（非独立人工复核）`mobile.artifacts.source.*`、`imageIndex`、`imageAlt`、`imagePending`、
  `imageLoading`、`openImage`、`imageOverflow`、`fileChangeOverflow`、`openFilePanel`、
  `sourceFallback`、`externalImage`、`reason.*`（16 个原因码）、`locate.*`（action/aria/short/
  searching/hit/miss/missHint/end/returnPreview/returnPanel/retry/dismiss/failure.* 4 个）
  与 `serverOmitted`（`另有 {{count}} 个附件因服务端预算未下发` → `{{count}} more attachments
  were not delivered due to a server budget limit`）的英文值，均为对应中文语境的独立英文表述
  （例如 `上传`→`Uploaded`、`第 {{n}} 张`→`Image {{n}}`、`最近五页消息中未找到该文件的引用`→
  `No reference to this file was found in the latest five pages of messages`），没有机翻残留
  或中英混排。

## 十、本地 issue 草稿（仅文本，未对外创建）

> 标题：**统一 `artifacts` 能力：把 Codex APP 的正文产物关联推广到其他工具**
>
> 背景：本期只为 Codex APP 的 IPC 投影增加结构化 `artifacts`（图片生成、用户附件、
> `fileChange`、`imageView`、HTTP(S) 图片 URL）与五页反向定位，其他工具仍只有各自的正文
> 链接能力（spec §2.3 明确把“跨工具 artifacts 统一”排除在首期之外）。同一产品语义（文件
> 身份、来源、定位）在 Claude/OpenCode/OpenClaw/Kimi Code 上会重复实现，且各工具的存储形态
> 不同，容易产生第三套路径规则。
>
> 范围设想：
>
> 1. 定义与工具无关的 `artifacts` DTO 与来源枚举（沿用本期 `artifactId`/`fileKey`/`kind`/
>    `source`/`ordinal`/`unavailableReason` 已定字段，避免第二套命名）；
> 2. 把 `file_identity_key` 提升为各工具文件池与附件共用的唯一身份口径；
> 3. 为每个工具写“字段证据 → 提取器 → 预算 → 前端卡片”的接入清单，逐工具独立验收；
> 4. 有界反向定位（五页/四次读取）作为共享纯函数，而不是每个工具各写一版状态机。
>
> 待决问题（不在本期回答）：
>
> - 各工具的 rollout/JSONL 是否都有稳定回合身份与 item 顺序？缺失时如何降级？
> - 跨工具是否共用同一套 `source` 语义（例如 `image_generation` 只在 Codex 有意义）？
> - 结构化提取与现有“正文已知路径链接化”的职责边界如何避免重复？
> - 分页/代际契约是否需要一个工具无关的接口层，还是各工具自行实现？
> - 统一后如何保持旧载荷兼容与错误码棘轮不上升？
>
> 处置：仅本地记录，未在 GitHub 或其他平台创建 issue；待用户裁决是否推进。

## 十一、本地分支与提交范围

- 分支：`codex/message-artifacts`（本地，未推送、未合并、未打 tag）。
- 主线基线：`origin/main@0ad8d763e5bdc55e0934ef82af2eedd1b91cbc15`（PR #171 合并提交）。
- 本计划范围：`be48acb4d80910051f4f378e5bdf85b84946781b..131de5285324a282dc884dbc43a7f928bc51edf6`
  （更新时的 HEAD，含任务 8、文档复核与合并前审查修复提交）。**最终 HEAD 可能因本文档提交
  再前移**，以 `git log -1` 为准。
- 任务 1—7 的 12 个提交：

  | 提交 | 说明 |
  | --- | --- |
  | `150fa97f` | `docs: record Codex artifact evidence contract` |
  | `12f7ee70` | `feat: share file identity keys with session files` |
  | `8de86f5d` | `feat(codex-ipc): project message artifacts with bounded turn metadata` |
  | `c3e153fc` | `fix(codex-ipc): restrict unavailableReason to preview failures` |
  | `e5f526e6` | `feat(codex-ipc): add read-only paginated history pages` |
  | `7b12868a` | `feat(codex-mobile): merge live projection with bounded history pages` |
  | `3ea5a4dd` | `fix(codex-mobile): reject over-limit pages and stale aborted loads` |
  | `16beec0f` | `feat(codex-mobile): render artifact cards with lazy images and fresh previews` |
  | `7d6ba498` | `fix(codex-mobile): document layout remount reloads and cover viewport exit` |
  | `2306cc2e` | `feat(codex-mobile): locate latest file reference within five pages` |
  | `7446c4dd` | `fix(codex-mobile): count distinct pages and couple write-pool flags` |
  | `24c93566` | `fix(codex-mobile): cap earlier-page reads at four per lookup` |

- 任务 8 与合并前修复提交（按时间顺序）：

  | 提交 | 说明 |
  | --- | --- |
  | `93e28b5e` | `docs: record acceptance evidence and tighten jump bounds`（任务 8 交付） |
  | `f4fedbb4` | `docs: tighten task 8 acceptance record wording`（任务 8 评审措辞修正） |
  | `fb01022b` | `fix(codex-ipc): observe settled history for token v2`（R1/R2 服务端） |
  | `26b95e69` | `fix(codex-mobile): correct artifact flags, omissions, summaries`（R3/R4/R5） |
  | `131de528` | `fix(codex-mobile): retain messages dropped by the tail window`（R2 客户端接缝） |

- 工作区在修复轮结束时保持干净（`.superpowers/` 被 `.gitignore` 忽略）；未修改
  `docs/MASTER-PLAN.md`、冻结 IPC spec 与真实用量账本。

## 十二、合并前审查修复记录（2026-10-21）

外部合并前审查提出 R1–R6；本分支修复 R1–R5，R6 保留为验收门：

| 项 | 发现 | 处置 | 提交 |
| --- | --- | --- | --- |
| R1 | 分页 token 不检测同一 id 的正文/附件改写 | token v2：按已结算 Turn 的内容指纹推进 `content_epoch`；同 id 文本改写、同 id 附件改写、已结算 Turn 删除均使旧页 token 失效 | `fb01022b` |
| R2 | 尾窗前移使投影 token 变化，客户端清空已加载历史；同 token 前移时滚出尾窗的消息在接缝处缺失 | 服务端投影 token 改为稳定根锚点（`root`+8 hex 线程标签，不再依赖 dropped 前缀，分页仍绑前缀 digest）；客户端 `loadedMessages` = 缓存页 + 保留缓冲 + 尾窗（按 id 去重），仅在四个既有失效触发点清空 | `fb01022b`、`131de528` |
| R3 | 合成文件池行把 `sourceStatus=failed` 的 `file_change` 标成 `modified:true` | 合成行保留 `origin:"tool_write"`，`modified` 恒为 false；原始 `sourceStatus`/`changeKind` 原样展示；后端既有 `modified:true` 不被撤销或提升 | `26b95e69` |
| R4 | `omittedArtifactCount` 无 UI 消费 | 消息级双语提示 `mobile.artifacts.serverOmitted`，与 UI 12 图 / 6 chip 上限分开；附件全部被服务端省略时仍显示 | `26b95e69` |
| R5 | `ArtifactFileEntry` 无来源摘要 | `ArtifactFileEntry`/`FilePanelEntry` 增加 24 字 `summary`（同回合提示词 → 附件摘要 → null），`FilePanel` 有摘要时渲染一行 | `26b95e69` |
| R6 | 真实证据门未闭（Windows 目标 build / macOS live IPC 附件样本与实机点击 / 移动端真机） | 不修代码，不宣称通过 | 未关闭，属用户裁决 |

- 新增/调整测试：R1/R2 服务端 6 条镜像回归，并使 `codex_desktop_ipc` 达 67 例、`codex_ipc_remote`
  保持 61 例；R2 客户端 15 条（hook 4 + 纯函数 11，全量 1753 passed / 5 skipped）；R3/R4/R5
  12 条（`artifact-links` +6、`ProjectedArtifacts` +3、`FilePanel` +2、`SessionDetailArtifacts` +1）。
- 失效触发点：token 变化、切会话/首次挂载、禁用订阅、卸载，以及服务端明确告知失效的
  `cursor_invalid` / `generation_mismatch`（F5 起统一走 `invalidateHistory`，页与保留缓冲
  一并作废；见 §八.3）。
- 客户端合并的语义顺序是页 → 保留缓冲 → 尾窗；页的实际落位用 `beforeId` 游标与重叠段细化，
  避免页与缓冲重叠时出现可见乱序（`fix-r2-seam.md` §2.3）。
- 证据等级不变：以上均为 AUTOMATION / SYNTHETIC，没有新增 OBSERVED 样本，也没有把任何平台
  验收项升级为 PASS（见 §十三）。

### 后续复核修复（2026-10-21 续，F/G/S 三轮）

外部复核在 R1–R6 之上追加三批问题，全部落在同一实现面（服务端内容观察 + 客户端五页窗口）：

| 项 | 发现 | 处置 |
| --- | --- | --- |
| F1 | 整体排除最新 Turn，单 Turn 超一页时其已分页内容永远陈旧 | 全 Turn 保序观察；只有「有活动证据（`inProgress`）且是 canonical 最后一个」的 Turn 的**末条消息**享流式豁免 |
| F2 | 接缝消息在列表里可见、却搜不到 | 保留缓冲进入五页窗口的第 2..k 批（占批次预算，不再整段并入第 1 页） |
| F3 | 最新 Turn 内删除的消息被保留缓冲复活 | 保留要求单调前移证据：不缩短（或带 `truncated`）、保序后缀/前缀重叠、新增段不含旧 id |
| F4 | 两个已结算 Turn 整体换序不换 token | 观察表保序（`Vec`），按位置逐 Turn 比对 |
| F5 | `cursor_invalid`/`generation_mismatch` 只清页不清保留缓冲 | 两个分支统一走 `invalidateHistory` |
| F6 | `sourceStatus=failed` 的 `file_change` 仍进「工具读写」池 | 中性来源 + 空完成白名单；原始 `artifactSource`/`sourceStatus` 如实展示 |
| G1 | 全部 retained 并入第 1 页，范围扩到 2500 条 | 第 1 页恢复为尾窗；retained 每 50 条一批、与缓存页共用四批预算（上界 700 条） |
| G2 | 非 `completed` 一律视作流式，历史 failed/缺状态 Turn 的末条无保护 | 活动豁免需「已知活动状态 + canonical 最后一个 Turn」双条件 |
| G3 | 末次增量与追加、结算同批到达时误换 token | 上一观察的活动末条只豁免一次，随后按历史内容全量比对 |
| G4 | 合法字节预算收缩被当成删除，保留缓冲整体清空 | 缩短需 `truncated` 证据且结构重叠成立；无证据的缩短仍按删除处理 |
| S1 | retained 被字节预算驱逐中段后，旧缓存页越过缺口命中更旧引用 | 缓存页只在与前沿相邻（`beforeId === frontier`）时参与；有缺口从真实前沿重锚补页，绝不越过 |
| S2 | 池侧与定位 hook 各算一套「有界窗口」 | 选页收敛为 `selectCodexHistoryWindow`（去重/连续性/批次预算/前沿唯一实现），两处共用 |
| T1 | 连续显式补页一律插到最新端，缓存顺序变成 `[旧, 新]` | 按游标连接关系插页：新页 `beforeId` 等于已有页 `nextBefore` → 插在其后；反之插在其前；无连接才退回 front/append |
| T2 | 缓存页与前沿有序重叠（含前沿那条消息）时被误判成缺口 | 相邻性两条路径：`beforeId === 前沿` 收整页；页内含前沿消息则只收其旧侧前缀（新侧已被尾窗覆盖）；两者都不成立才是真缺口 |

- 提交：`d8980d56`（F1/F4）、`b34032f5`（F2/F3/F5/F6）、`9ef495dc`（G2/G3）、
  `211bf184`（G1/G4）、`6b99a8bd`（S1/S2）、`246bf529`（T1/T2）。
- 计数：`codex_desktop_ipc` 67 → 77 例；前端全量 1753 → 1772 passed / 5 skipped（+19）。
- 证据等级不变：以上全部 AUTOMATION；**R6 仍未关闭**，尤其 `inProgress` 是否为目标 Windows
  build 的真实活动状态值、以及移动端实机闭环，均未核验（见 §十三）。

## 十三、验收边界声明

1. **自动化交付已通过**：§四 表 A／表 B 的门禁命令退出码 0（标为「修复前」的探针行除外），
   计数如上；这是本任务唯一可宣称完成的部分。
2. **产品验收未完成**：Windows `26.1013.4894.0` 的三图/一文档/一上传图片/五页定位/同路径
   覆盖/失败占位全部 **NOT EXECUTED**；macOS 真实 IPC 附件样本与实机点击、移动端真机闭环
   同样 **NOT EXECUTED**。本文件没有任何一行把未执行写成通过。
3. **macOS 静态字段证据不是 Windows 运行证明**：它只说明本机安装包中存在这些字段读取，
   不能证明目标 build 的运行时取值、枚举兼容性或 Windows 路径形态。
4. **合成夹具仍为 SYNTHETIC**：本轮未新增、也未升级任何 OBSERVED 样本；五类来源的运行时
   形态在目标平台仍属未核验。
5. 达到产品验收还需：一台装有目标 Desktop build 的 Windows 设备、一个有真实附件/生成图
   的会话（在用户明确授权的前提下），以及一台真实移动端设备；三者当前均不具备。
6. **修复轮不改变本条边界**：R1–R5 只更新实现、自动化计数与已知限制；R6（真实证据门）仍未
   关闭，是否推进由用户裁决。本文没有把任何 SYNTHETIC 输入升级为 OBSERVED，也没有把任何
   平台验收项写成通过。
