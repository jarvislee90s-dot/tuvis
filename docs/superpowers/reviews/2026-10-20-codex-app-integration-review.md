# Codex APP 集成 · 门禁结果与独立复审（2026-10-20）

> 计划 Task 9 交付物：记录质量门禁的命令/平台/结果、独立复审方法与结论、未测格。
> 复审对象是 `codex/codex-app-delivery` 相对 **最新 main** 的真实 diff，不以 #165 曾过审
> 代替。

## 1. 复审对象

| 项 | 值 |
| --- | --- |
| base | `origin/main` `77fcdb487e8bd32eb4109cf8a274e7663a0ef43c` |
| head | `8d3e237b`（本文档之前的交付提交：`67b01a13`/`d7d1d732`/`d6d2d348`/`8d3e237b`） |
| diff 规模 | 83 文件、约 +40.6k/−0.1k（含夹具与文档） |
| 独立复审 | 独立 reviewer 子代理（只读，禁改代码），对本 diff 逐项核对；主线另做自查与门禁复跑 |
| 复审材料 | 本文件 + `2026-10-20-codex-app-extraction-ledger.md` + `2026-10-20-codex-app-preflight-results.md` |

## 2. 质量门禁（Windows 11 26200 / rustc 1.97.0 / Node 26.8.1 / pnpm 11.13.0）

| # | 命令 | 结果 | 备注 |
| --- | --- | --- | --- |
| 1 | `pnpm install --frozen-lockfile` | PASS | lockfile 未改 |
| 2 | `cargo fmt --check` | PASS | 全仓 |
| 3 | `cargo clippy --all-targets --features marker-helper -- -D warnings` | PASS | 零告警 |
| 4 | `cargo test --features marker-helper --no-fail-fast` | **2631 passed / 5 failed / 45 ignored** | 5 例为 main 基线既有（§3） |
| 5 | `cargo test --test codex_desktop_ipc` | PASS 42 | 协议/连接/投影/重连 |
| 6 | `cargo test --test codex_ipc_remote` | PASS 50 | HTTP/SSE/审批/队列/审计 |
| 7 | `cargo test --test codex_control_remote` | PASS 34 | 账本/FIFO/屏障/Unknown/撤回 |
| 8 | `cargo test --test headless_e2e -- --list` | 1 例（ignored） | `codex_queue_consumed` 未执行、零配额 |
| 9 | `cargo check --release --features codex-ipc-e2e-stop` | **按预期编译失败** | `compile_error!`：只允许 debug 证据构建 |
| 10 | `pnpm test` | PASS 146 文件 / 1519 例 / 5 skipped | 前端全量 |
| 11 | `pnpm check`（format/lint/i18n/error-codes/build） | PASS | 构建产物正常 |
| 12 | `pnpm build:mobile` | PASS | `dist-mobile/` |
| 13 | `pnpm check:i18n` | PASS 1417 键 | zh/en 键集一致 |
| 14 | `node scripts/check-error-codes.mjs` | PASS 284 码 / 285 键 / legacy 5 | 三个新码已入 zh/en；棘轮未上升 |
| 15 | Windows 目标检查 | 原生 `--all-targets --features marker-helper` 已覆盖 | 本机即 Windows；CI 的 gnu 交叉目标在 Windows 宿主按 `ci-backend-local.sh` 跳过（脚本明示） |

**一次失败与处置**：首次 `cargo test --features marker-helper` 全量并行编译/链接触发
Windows `os error 1455（页面文件太小）`，不是代码失败；以 `CARGO_BUILD_JOBS=2` 重跑通过。
记录在此，避免被误读成测试红。

## 3. 基线既有失败（已用 pristine main 复核）

`E:\LLMproject\Github\tuvis-main-baseline`（`origin/main` 77fcdb48）同样失败：

- `monitor::claude_parser::title_tests::get_claude_sessions_badge_end_to_end_new_layout`
- `monitor::codex_parser::report_flag_tests::report_flag_true_when_task_complete_within_window`
- `monitor::opencode_parser::v2_tests::report_flag_true_when_child_updated_within_window`
- `services::subagent_detection_tests::detect_subagents_missing_dir_is_supported_empty`
- `services::subagent_detection_tests::detect_subagents_existing_dir_lists_agents`

根因同类：用例以 `HOME` 环境变量重定向 home，Windows 的 `dirs::home_dir()` 不读 `HOME`
→ 读到真实 profile。本轮未改被测逻辑，未用忽略/棘轮/关闸让它变绿。

## 4. 逐项对照冻结 spec（F1–F8）与计划决定

| 项 | 落地 | 证据 |
| --- | --- | --- |
| F1 接入/owner | discovery → following → snapshot；owner 换代清旧世代 | `codex_desktop_ipc` 42 例、`last_frame_from_old_owner_is_dropped` |
| F2 实时状态 | canonical 投影 + SSE；Loading/Partial/Unsupported 与真空分离 | `mirror`/`history_view` 33 例、`codex-ipc` 前端用例 |
| F3 远程发送 | **普通消息经统一 router**（含 A 不可用），actionId 幂等，FIFO by seq | `same_action_never_sends_twice`、`same_millisecond_fifo_orders_by_seq` |
| F4 审批 | command 审批远程应答（accept/decline），不混 permissions 载荷 | `windows-4894-command-approval-*`、`codex_ipc_remote` |
| F5 结构化输入 | Plan 阻塞问答（整数 requestId 原类型） | `windows-4894-plan-input`、approval 端点用例 |
| F6 恢复 | 断开 → 重连 → 重新初始化 → 重新发现 → 重新订阅；旧 owner 不复用 | `recovery_loop_reconnects_with_backoff_after_peer_drop` 等 |
| F7 路由/防双投 | 只有**明确零投递**才允许兼容；Unknown 绝不自动切兼容/重试；兼容分支受 H3 | `fallback_only_before_dispatch`、`unknown_delivery_never_unlocks_turn_restriction` |
| F8 能力可见 | 四档 availability + capabilities；未解锁明确拒绝 | `profile.rs` + send-info 能力档 |

计划决定逐条承接：见账本 §3 与验收报告 §2；不整包 #117（diff 无 zcode/wb/dsh/cli/codex_app/
tty_map 文件）；Windows permissions=false；不发版。

## 5. 重点复查项（抽取边界 / 权限门 / 幂等 / 迁移 / 双语）

- **抽取边界**：`git diff --name-only origin/main...HEAD` 不含 #117 其他驱动文件；
  `inject/headless/mod.rs` 只注册 5 个模块；`routing.rs` 只有 `CodexQueue/CodexExec`。
- **权限门**：`profile_for("windows","26.1013.4894.0")` 的 permissions=false；H3 门在
  `session_send`/`send_info` 均先判 hub（A 可用不看 H3）；`exec` 的 `approval_policy`
  只作 spawn 入参，无远程审批承诺。
- **幂等/Unknown**：`action_id_required` 400（不代造）；`same_action_never_sends_twice`；
  Unknown/缺 turnId 存保守屏障，只认精确 turnId 终态；撤回 CAS queued→cancelled 保留墓碑。
- **迁移**：`codex_actions` 表 + 4 个 partial index；`barrier_force_released_at` 幂等
  加列（旧库重放两次不报错）；无版本号抢占。
- **双语**：`mobile.codex.*` 46 键与 `settings.remote.headless*` 10 键 zh/en 成对；
  新错误码 3 个（`MIGRATE_CODEX_BARRIER_RELEASE_FAIL`/`CODEX_QUEUE_RETRACT_PENDING`/
  `CODEX_QUEUE_FORCE_IN_FLIGHT`）已在两侧补文案。

## 6. 未测格（不冒充）

- PRODUCT 层 Windows E1–E12（需用户在场操作 Desktop/审批/`/plan`/重启/关闭 Desktop）；
  Mac 全部写路径（需实际执行者）；真实手机主路径。
- E11 的实机强杀（停点单测与 release 门已过，实机步骤见验收 §4）。
- 详细缺口与交接顺序：`2026-10-20-codex-app-product-acceptance.md`。

## 7. 独立复审发现与处置

> 独立 reviewer 对上述 diff 做只读审查（git diff + 读源码，非转述文档）。结论
> **PASS_WITH_FINDINGS，无 Critical**。以下逐条记录并给出处置。

### Important

| # | 发现 | 处置 |
| --- | --- | --- |
| I1 | 文档（`turn.rs` 的调用点节、`codex.rs`、`normalize.rs`）声称花名白名单在「签名组装 / 配对登记 / 改名」三处生效，实际只有 compose 一处调用；`DEVICE_NAME_UNSAFE_CODE` 零引用，配对与改名仍可登记 emoji/`&` 等花名（投递时才拒绝，零投递但无即时提示） | **修正为诚实的单点文档**：删除未接线的错误码常量；`turn.rs` 明确写「本轮只有 compose 一处；注册/改名 gate 属 #117 续作」；`codex.rs`/`normalize.rs` 的重复承诺同步删除。**不**在本轮扩 main 既有配对/改名行为（计划边界：不扩终端/配对合同）。 |

### Minor（已修）

| # | 发现 | 处置 |
| --- | --- | --- |
| M1 | `ACTION_HEADLESS_APPROVE` 无生产调用（#117 claude 审批桥词表孤儿） | 删除常量与对应断言；`write_audit` 注释只保留 `headless/headless_cancel` |
| M2 | `normalize::truncate_with_size_marker` 无生产调用（仅自测 + 一处文档引用） | 删除函数与自测；`mirror` 注释改为描述同一展示纪律、不再引用已删函数 |
| M3 | `headless/mod.rs`、`turn.rs`、`codex.rs`、`gate.rs` 残留 zcode/WB/cli_three 的陈旧注释 | 改为 Codex 口径；`gate` 的失败 TTL 注释去掉 ZCode 归因 |
| M4 | `router.rs` 注释称「HTTP 层 409 action_conflict」，实现是同 ID 异载荷返回 200 `rejected` | 注释更正为 200 rejected（只有 retract 用 409） |
| M5 | `turn.rs` 的 `more` 片段误带 `TURN_ETC_KINDS:` 码前缀（#117 迁移残留，用户可见文本变脏） | 改为 ` (+N)`，不引入伪码；同步删除 `turnEtcKinds` 语言键 |
| M6 | `settings.remote.headlessConfirm` / `headlessApprovalLive` 为无引用孤儿，且文案宣传未交付的 zcode/claude 能力；`headlessHint` 也点名 ZCode/WorkBuddy | 删除两键；`headlessHint` 改为只描述 Codex APP 兼容通道；zh/en 同步 |

### 待确认（不在本轮自行裁决）

| # | 发现 | 处置 |
| --- | --- | --- |
| Q1 | Codex 两条发送路径（A 路 router 与兼容路）都调 `compose_injection`（恒带 `[mobile 花名]`），**忽略** main 的 `remote_message_signature` 开关；终端路径按开关 | 保留 #117 设计与「来源标记单点组装」决定；在验收报告 §8 登记为**通道差异**，是否改为受开关约束留待用户裁决（不影响投递安全，只影响 token 与溯源形态） |
| Q2 | compose 的花名白名单对终端四家同样生效，轻微改变 main 终端发送行为（拒绝不安全花名） | 保留（安全优先，且只在花名含 cmd 元字符时触发）；在验收报告 §8 登记为**有意偏差**，不视为功能回归 |

### 复审已核对无问题（摘要）

- 抽取边界：无 zcode/wb/dsh/cli_three/codex_app/tty_map/旧 APP 控制文件；路由枚举仅 Codex。
- 权限门：4894/3656 permissions=false；H3 默认关且只约束兼容分支；hub 在 H3 之前。
- 幂等/Unknown：缺 actionId 400；同 action 一次派发；Unknown 不回 queued；只有 NotSent 才
  handoff；撤回 CAS + 墓碑；屏障只认精确 turnId 终态；FIFO 按 seq；compat fence 跨重启。
- 迁移：新表 + partial index；`barrier_force_released_at` 幂等加列。

## 8. 结论

（待回填）
