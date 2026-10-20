# Codex APP 集成 · 门禁结果与独立复审（2026-10-20）

> 计划 Task 9 交付物：记录质量门禁的命令/平台/结果、独立复审方法与结论、未测格。
> 复审对象是 `codex/codex-app-delivery` 相对 **最新 main** 的真实 diff，不以 #165 曾过审
> 代替。

## 1. 复审对象

| 项 | 值 |
| --- | --- |
| base | `origin/main` `77fcdb487e8bd32eb4109cf8a274e7663a0ef43c` |
| head | 本轮最终提交（`cfb0b7dc`/`cef8057a`/`68916a89`/`48456519`/`6e1e440b`/`55706d1b`/`46bf82b0`/`0ba1f44c`/`f89e18e6` + 第四轮 docs；基线 `67b01a13`/`d7d1d732`/`d6d2d348`/`8d3e237b`/`bf9b2949` + 第三轮 `cfb0b7dc`/`cef8057a`；文档提交随后） |
| diff 规模 | 83+ 文件、约 +40.6k/−0.1k（含夹具与文档） |
| 独立复审 | 独立 reviewer 子代理（只读，禁改代码），对本 diff 逐项核对；主线另做自查与门禁复跑 |
| 复审材料 | 本文件 + `2026-10-20-codex-app-extraction-ledger.md` + `2026-10-20-codex-app-preflight-results.md` |
| 冻结 spec（权威原文，固定提交） | [2026-10-16-codex-desktop-ipc-remote-control-design.md @ d5cc28b1](https://github.com/jarvislee90s-dot/tuvis/blob/d5cc28b1eb20133ee37b788013c6d318e4731d46/docs/superpowers/specs/2026-10-16-codex-desktop-ipc-remote-control-design.md)（本分支未跟踪该文件，读的是未改动原文） |

## 2. 质量门禁（Windows 11 26200 / rustc 1.97.0 / Node 26.8.1 / pnpm 11.13.0）

| # | 命令 | 结果 | 备注 |
| --- | --- | --- | --- |
| 1 | `pnpm install --frozen-lockfile` | PASS | lockfile 未改 |
| 2 | `cargo fmt --check` | PASS | 全仓 |
| 3 | `cargo clippy --all-targets --features marker-helper -- -D warnings` | PASS | 零告警 |
| 4 | `cargo test --lib`（第四轮修复后） | **2641 passed / 5 failed / 45 ignored** | 5 例为 main 基线既有（§3） |
| 5 | `cargo test --test codex_desktop_ipc` | PASS 43 | 协议/连接/投影/重连 |
| 6 | `cargo test --test codex_ipc_remote` | PASS 54 | HTTP/SSE/审批/队列/审计（+签名/IPC、设备吊销交接） |
| 7 | `cargo test --test codex_control_remote` | PASS 36 | 账本/FIFO/屏障/Unknown/撤回（+写前忙回队、旧 Turn 回显、签名冻结） |
| 8 | `cargo test --test headless_e2e -- --list` | 1 例（ignored） | `codex_queue_consumed` 未执行、零配额 |
| 9 | `cargo check --release --features codex-ipc-e2e-stop` | **按预期编译失败** | `compile_error!`：只允许 debug 证据构建 |
| 10 | `pnpm test` | PASS 151 文件 / 1599 例 / 5 skipped | 前端全量（第三轮新增 9 例：详情页 5 + MessageComposer 4，含 H3 本地化） |
| 11 | `pnpm check`（format/lint/i18n/error-codes/build） | PASS | 构建产物正常 |
| 12 | `pnpm build:mobile` | PASS | `dist-mobile/` |
| 13 | `pnpm check:i18n` | PASS 1445 键 | zh/en 键集一致 |
| 14 | `node scripts/check-error-codes.mjs` | PASS 283 码 / 284 键 / legacy 5 | 新码已入 zh/en；删码删键后棘轮未上升 |
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
| Q1 | ~~Codex 两条发送路径恒带签名、忽略 `remote_message_signature`~~ **已于第三轮复审修复**：登记时读取开关一次并冻结（`signature_applied` + 最终 wire），IPC/队列/兼容/CLI 同一份；旧行按升级前恒签名 | 已修复。**行为变化须知**：开关默认关闭 ⇒ Codex APP 从此默认**不带** `[mobile 花名]` 来源标记（与上一轮恒签名不同）；需要标记的用户请在电脑端开启「消息签名」 |
| Q2 | ~~compose 的花名白名单对终端四家同样生效~~ **已于第三轮复审修复**：全局花名白名单移除，安全门收窄到 Windows cmd 垫片的真实 argv（只拒实际进 argv 的 `CMD_SHIM_METACHARS`，签名关闭/Desktop IPC/原生 exe 不因昵称字符被拒） | 已修复；证据见 §7.1 的 P3 行 |

### D–G 复审（第二轮，HEAD 前进后复核）

| # | 发现 | 处置 |
| --- | --- | --- |
| I-2 | 账本/计划把 `tests/mobile/codexIpcApi.test.tsx`、`useCodexThread.test.tsx` 列为验证入口，但仓内不存在；`codexIpcApi.ts`/`useCodexThread.ts`/`PendingRequestCard.tsx`/`CodexQueuePanel.tsx` 零测试 | **从冻结源补回 4 个测试文件**（70 例全绿）：actionId 生命周期、403 保持旧视图、SSE 三连失败降级、unknown 锁死 / not_sent 才重试 / verifying 防双答、队列撤回/强制继续。`SessionDetail` 的 Codex 投影分支仍无组件级测试（见 §8 残余缺口），账本已改为如实描述 |
| D-1 | `let _ = migrate()` 吞错；「表在但缺列」的 ALTER 分支无回归 | 迁移失败改 `log::error!`；新增 `migrate_adds_codex_barrier_force_released_column`（旧表 → 迁移 → 列在 + 存量行保留 + 两遍幂等） |
| E-1 | `mobile.codex.projection.*` 5 键无人消费（详情页硬编码中文） | 删除键；详情页整体未 i18n 化，部分接线会造成中英混排，登记为后续整体迁移（§8） |
| E-2 | zh `headlessConcurrencyHint` 含字面 `**` | 去掉 Markdown 标记，zh/en 值对齐 |
| E-3 | en `turnDeviceNameUnsafe` 仍写 "MAM roster" | 改 "Tuvis device roster" |
| E-4 | zh `displayTruncated` 缺「原文更长」限定 | 改为「内容已截断（原文更长）」 |
| F-1 | stage 夹具声称前端 `HEADLESS_STAGE_TRIAGE` 对账，实际无此符号 | 注释改为只声明 Rust 侧单边钉死，删除跨语言承诺 |
| F-2 | observed README 对 busy 夹具的描述与实际派生不符 | README 改为「替换 params 的 clientUserMessageId/input、items 保留首条真实消息」 |
| F-3 | 集成测试的 `#[cfg(test)]` 缝不生效，`production_presence` 真扫进程表（channel 随本机 Desktop 状态漂移） | 如实登记为**确定性缺口**（§8）；测试未断言 channel，PATH 已隔离不会真 spawn，暂不改生产代码 |
| F-4 | 忙态矩阵缺「两次 busy 检查间转忙」与「start-turn success 返回已完成旧 turnId」两例 | 登记为**已知缺口**（§8）；现安全依赖下一发前 busy 复检（可能丢一条、不会双投），补测列入续作 |
| G-1 | ⑤b 兼容直连臂今日不可达，未来收窄 hub 条件会变成绕过 actionId 账本的双投面 | **删除 ⑤b 与两个只被它调用的 wrapper**（`codex_headless_dispatch`/`_envelope`）；保留 `_with_evidence` 供 hub 兜底。加注释说明 Codex APP 一律走 hub |
| G-2 | 403 `headless_disabled` 无 `reason`，Composer 只读 `reason` | 403 响应补 `reason: HEADLESS_DISABLED_REASON` |
| G-3 | `audit_headless` 仍称「没有生产调用者」；mirror 注释引用已删函数 | 改为「`audit_headless_ctx` 每回合调用」；删掉对 `truncate_with_size_marker` 的引用 |
| G-4 | 复审文档 head 落后 | 本节按最终 HEAD 更新（见 §1） |

### 7.1 第三轮复审修复（2026-10-20，用户审查七项）

| # | 问题 | 修改位置 → 行为变化 | 测试证据 |
| --- | --- | --- | --- |
| P1 | 忙态竞态与旧 Turn 伪成功 | `AdapterDispatch::send_text` 在 ensure 后、start-turn 前用 `pre_write_snapshot`（同一把镜像锁取忙态 + 已存在 Turn 集合）；忙 → `NotSent(busy_before_write)`，router `requeue_after_busy` 恢复 wire 回队（不 fallback、不换 actionId、不消耗正文）；Accepted 回执 turnId ∈ 派发前集合 → 降级 `unknown_delivery(stale_turn_echo)`，无 turnId → `turn_unconfirmed`，均保留保守屏障 | **直接单测**：`pre_write_snapshot_reports_busy_and_known_turns`（ScriptedPeer：idle 不忙 + Turn 集合可用、active 报忙，desktop 43 例）、`turn_ids_collects_canonical_turns`（mirror 金样）、`turn_association_refusal_classifies_stale_and_unconfirmed`（分类器）。**组合覆盖**：`busy_after_idle_check_requeues_without_start_turn`、`stale_turn_echo_is_unknown_and_keeps_barrier`（router 对 NotSent/Unknown 的回队与屏障语义）。**第四轮已补端到端**：`busy_race_new_turn_without_client_identity_is_unknown`（快照后本地建 T1、忙态回显 T1 → Unknown）、`accepted_when_new_turn_contains_client_message_identity`（正式 userMessage 身份 → Accepted） |
| P2 | Codex 不遵守全局签名设置 | 登记时读 `remote_message_signature` 一次并冻结：`queued_text` = 最终 wire、新列 `signature_applied`；router/fallback/CLI/`wire_text_hash` 同一份；旧行（NULL）按升级前恒签名；幂等 hash 仍按原文；忙回队同写标志（防旧行双签名） | `signature_setting_is_frozen_at_registration`（关/开/入队后切设置+重启/旧行/fallback 五段 + 同 actionId 重试）；`signature_on_ipc_start_turn_appends_exactly_once`（HTTP 真 IPC）；`signature_setting_is_frozen_at_registration` 的 `sig-legacy` 段（旧行写前忙回队 → flag=1、只一个签名） |
| P3 | 昵称白名单误伤所有通道 | `compose_injection` 只归一 + 可选签名（不再按花名拒绝）；安全门 = `argv_shim_refusal`/`cmd_shim_body_refusal` 对**最终载荷**且仅 `via_cmd_shim` + 进 argv 时生效 | `compose_does_not_reject_unusual_device_names`、`cmd_shim_still_refuses_final_payload_with_metachars`、`compose_normalizes_device_name_too` |
| P4 | 详情页 Codex 控件被 `!isSummary` 隐藏；队列不随发送刷新；Unknown 草稿复用旧 ID | Codex 能力/pending/队列按投影状态展示（QuestionCard 仍保留总结态门）；`queueTick` 由投影 revision 与发送回执共同驱动；Unknown 回执保留核对锚但清 actionId（新发送换新 ID） | `SessionDetailCodex.test.tsx` 5 例（idle+Unknown 屏障可恢复、idle pending、revision 固定发送 queued 后出现并可撤回、revision=null 兼容、换会话不串台）；MessageComposer 4 例（unknown 换 ID、queued/accepted 均触发 onQueueChanged、H3 403 本地化原因）；第四轮补 `F`（revision=null 兼容超时 → 屏障/恢复入口） |
| P5 | 兼容派发前不复核设备 | `run_compat` 在真实兼容派发前（睡眠/交接后）复核设备在册；吊销 → NotSent(device_revoked)、零 spawn；清 fence 与终态同语句，审计随后另写（失败仅记日志）；helper 在版本探针后再核一次 | `compat_refuses_when_device_revoked_during_ipc_handoff`（dead IPC 交接窗口吊销 → rejected/device_revoked、fence 清除、无 headless 审计） |
| P6 | SSE 首帧订阅竞态 | 先 `subscribe()` 再取快照；启动缓冲按 `(owner_epoch, owner_generation, revision)` 合并（stale 不覆盖新鲜快照；跨 owner 不靠 revision 大小）；Lagged 启动期重取一次快照 | `startup_merge_tests` 8 例（+ 同代际 stale 送达、Lagged 多帧不回退、stale 仅由更高 revision 清除）；首快照与广播同源盖 lease 代际（`turn_ids_collects_canonical_turns` 断言 epoch≠0）；流式阶段与 Lagged 共用 `supersedes` 顺序基线 |
| P7 | H3 拒绝原因未贯通 | hub 分支真实 403 补 `reason` + `reasonCode`；手机优先按 `reasonCode` 本地化（`mobile.composer.headlessDisabled`），`data.reason` 仅兜底 | `send_h3_off_ipc_unavailable_compat_refuses` 断言 reason；MessageComposer `403 headless_disabled 显示服务端 reason` |

### 7.2 第四轮审查修复（2026-10-20，用户二次审查 1×P1 + 3×P2）

| # | 问题 | 修改位置 → 行为变化 | 测试证据 |
| --- | --- | --- | --- |
| P1b | 忙态伪成功仍有「快照后本地新建 Turn」竞态 | `send_text` 在 Accepted 后要求回执 Turn 中出现本次 `clientUserMessageId` 的**正式 userMessage**（`mirror.turn_has_client_message`；`params.input`/params 回显不算证据），有界等待 500ms（25ms×21），等不到 → `unknown_delivery(client_message_unverified)`，保留屏障、不 fallback | `busy_race_new_turn_without_client_identity_is_unknown`（ScriptedPeer 快照后建 T1、忙态回显 T1 → Unknown + 屏障）、`accepted_when_new_turn_contains_client_message_identity`（正向）；既有 accepted 用例补身份帧 |
| P2b | exec→queue 改道漏设备/H3 复核 | `TurnArgs.gate` 在 `run_turn` 入口与**每次改道前**各调用一次；生产 gate 复核设备在册 + H3 开启，拒绝 → `Stage::Refused`、queue 零启动 | `revoked_device_before_diverted_queue_blocks_second_dispatch`（scripted exec 锁拒 + 第二次 gate 拒绝 → 只 1 次 exec、queue 0、refused） |
| P6b | stale 事件被 `supersedes` 丢弃 / Lagged 抽干-重取窗口回退 | stale 视为**失效信号**：同/更高 revision 的 stale 必须送达；stale 恢复只由更高 revision 证明。流式阶段与 Lagged 重取共用 `last` 顺序基线，旧事件不再在新快照后补发 | `stale_disconnect_event_at_same_revision_is_delivered`、`lagged_recovery_frames_do_not_regress_snapshot`、`stale_snapshot_cleared_only_by_newer_revision`（startup 9 例） |
| P4b | 兼容 headless 回执不刷新恢复面板 | `MessageComposer` 的 `headless` 回执（ok/queued/failed/timeout/cancelled）同样调用 `onQueueChanged`；revision=null 时无新帧也能显示屏障/「核对后继续」 | `SessionDetailCodex.test.tsx` F：revision=null 兼容超时 → 面板 + force-continue 可见（6 例） |

### 复审已核对无问题（摘要）

- 抽取边界：无 zcode/wb/dsh/cli_three/codex_app/tty_map/旧 APP 控制文件；路由枚举仅 Codex。
- 权限门：4894/3656 permissions=false；H3 默认关且只约束兼容分支；hub 在 H3 之前。
- 幂等/Unknown：缺 actionId 400；同 action 一次派发；Unknown 不回 queued；只有 NotSent 才
  handoff；撤回 CAS + 墓碑；屏障只认精确 turnId 终态；FIFO 按 seq；compat fence 跨重启。
- 迁移：新表 + partial index；`barrier_force_released_at` 幂等加列。

## 8. 残余缺口（如实登记）

1. **`SessionDetail` 投影五态覆盖不完整**：`SessionDetailCodex.test.tsx` 已覆盖
   idle+屏障/pending/固定 revision 队列刷新/revision=null/换会话 5 条接线；空/loading/
   partial/unsupported 四态的**视觉分支**仍无组件断言（逻辑在 useDelayedUnsupported 与
   projection 渲染分支，建议续作补）。
2. **F-3 确定性**：`codex_ipc_remote` 的 presence 走真实进程扫描；测试不断言 channel，
   故不会红，但「本机是否开着 Desktop」会影响内部走向。建议续作把 presence 注入
   `RemoteState` 测试缝，或在 fixture 固定 channel。
3. **P1 身份核验窗口**：正常状态帧通常先于 start-turn 回执到达，但若 Desktop 的
   `userMessage` 晚于 500ms 才进镜像，正常发送会被保守判 Unknown（安全但可能多一次人工
   核对）。真实 Desktop 忙态未跑；探针后的设备复核（`api.rs` 5c）仍无法在测试构建到达
   `production_exe` 分支。
4. **PRODUCT / Mac / 真机**：见 `2026-10-20-codex-app-product-acceptance.md`；未执行即未执行。

## 9. 结论

独立复审三轮均为 **PASS_WITH_FINDINGS（无 Critical）**；第一/二轮 6 Important + 8 Minor、
第四轮 1 P1 + 3 P2 已全部处置（或如实登记残余窗口）。修复后重跑：

- `cargo fmt --check` / `cargo clippy --all-targets --features marker-helper -- -D warnings`：PASS
- `cargo test --features marker-helper`：2641 passed / 5 failed（均为 main 基线 HOME 隔离失败）
- `cargo test --test codex_desktop_ipc / codex_ipc_remote / codex_control_remote`：43 / 54 / 36 PASS
- `pnpm test`：151 文件 / 1599 例 / 5 skipped 全绿；`pnpm check`、`pnpm check:i18n`、`check-error-codes` PASS
- 残余缺口与未测格见 §8 与验收报告，不以自动化层冒充产品验收。

