# Codex APP 集成 · 产品验收记录（双平台）

> 计划 Task 10 交付物。**证据分层**：IPC-FIXTURE / HTTP-INTEGRATION / UNIT 是自动化层；
> PRODUCT 才是「真实 Tuvis + 真实 Desktop + 移动端」的产品验收。本文不把前两层写成
> PRODUCT PASS，也不把 Mac 回传报告写成当前执行者亲测。
>
> 执行环境（Windows）：Windows 11 家庭版中文版 10.0.26200 x64；Desktop
> `OpenAI.Codex` **26.1013.4894.0**；能力档 `send/state/command/blocking_input=true`、
> `permissions=false`；Tuvis 分支 `codex/codex-app-delivery`，HEAD 见 §6。

## 1. 前置（Windows）

| 项 | 结果 |
| --- | --- |
| `pnpm install --frozen-lockfile` | PASS（lockfile 未改） |
| `pnpm build:mobile` | PASS（`dist-mobile/` 产物生成） |
| `pnpm check`（format/lint/i18n/error-codes/build） | PASS |
| `cargo fmt --check` / `cargo clippy --all-targets --features marker-helper -- -D warnings` | PASS |
| Shadow 数据目录 | 用于任何启动 Tuvis 的验收（`TUVIS_HOME`），不写真实 `~/.tuvis` |

## 2. 适用验收表

| 编号 | 验收项（计划 §5） | 状态 | 证据/说明 |
| --- | --- | --- | --- |
| E1 | 打开一个 Codex 会话：快照 → 投影 → 刷新恢复 | **IPC-FIXTURE PASS**；PRODUCT 未执行 | `windows-4894-snapshot` 金样投影；`codex_desktop_ipc` 42 例（follow/snapshot/重连/owner 换代） |
| E2 | 桌面忙碌时发送普通消息 | **HTTP-INTEGRATION PASS**；PRODUCT 未执行 | `codex_ipc_remote` 的 busy/FIFO、`codex_control_remote` 的 queued/dispatch 计数 |
| E3 | 手机端连续发送、队列顺序与撤回 | **HTTP-INTEGRATION PASS**；PRODUCT 未执行 | `same_millisecond_fifo_orders_by_seq`、`retract_reserve_race_single_winner_and_no_redispatch`、`tombstoned_terminal_replay_stays_terminal` |
| E4 | 远程批准/拒绝 command 审批 | **IPC-FIXTURE PASS**（协议层）；PRODUCT 未执行 | `windows-4894-command-approval-{accept,decline}` 夹具 + `codex_ipc_remote` 审批端点；真实审批需 Desktop 弹卡与用户在场 |
| E5 | Plan 阻塞式问答远程作答 | **IPC-FIXTURE PASS**；PRODUCT 未执行 | `windows-4894-plan-input` 夹具（整数 requestId）；真实 `/plan` 回合需用户操作 |
| E6 | Desktop 缺席时普通消息走兼容 exec | **NOT_EXECUTED**（PRODUCT） | 需关闭 Desktop 并对目标会话发消息；自动化只覆盖 dispatch 纯核与 H3 门 |
| E7 | Desktop 在场走 queue 兼容；两臂 H3 关闭拒绝 | **HTTP-INTEGRATION PASS**（H3 拒绝）；PRODUCT 未执行 | `send_not_sent_hands_off_to_compat_when_h3_on`、H3 403 用例；E10 双臂见下 |
| E8 | permissions 关闭时如实提示 | **PASS（实现）/ PRODUCT 未执行** | `ipc_available()` 与能力档四档；4894 的 `permissions` 能力档保持 **false**——`item/permissions/requestApproval` 曾出现并完成 accept/decline 双臂（审批证据），但**从未据此解锁权限档**；「未触发」只适用于 T7 的读升权场景，不能写成从未出现请求 |
| E9 | Desktop 重启后重新发现 owner 并恢复 | **IPC-FIXTURE PASS**（重连/换代）；PRODUCT 未执行 | `recovery_loop_reconnects_with_backoff_after_peer_drop`、`unknown_build_closes_and_verified_reopens`；**恢复时延按实际 backoff 与恢复条件描述**（首轮 0.94s 断开→stale 是单次实测，不是固定 ≤2s 承诺；重启后需用户在 Desktop 重新打开会话才恢复，见 2026-10-19 报告 T9 旧基线） |
| E10 | queue 臂与 exec 臂各一次（Desktop 在/不在） | **NOT_EXECUTED**（PRODUCT） | 需真实打开/关闭 Desktop 两次并各发一条消息；H3 两臂拒绝已由集成测试覆盖 |
| E11 | 未知投递恢复（Accepted 后强杀，重启为 unknown） | **NOT_EXECUTED**（PRODUCT） | 停点实现与单测 PASS；实机步骤需构建影子 Tuvis、真实 Accepted 后强杀（见 §4 交接） |
| E12 | 同 Thread 连续三回合有序 | **HTTP-INTEGRATION PASS**；PRODUCT 未执行 | `turn_restriction_needs_matching_completion_evidence`、`turn_restriction_survives_restart_until_completion`、`receipt_without_turn_id_keeps_restriction` |

**PRODUCT 未执行的原因（如实）**：本轮执行环境没有可安全操作的官方 Desktop 会话
（打开会话、触发 command 审批、`/plan` 提问、重启 Desktop、关闭 Desktop 后发消息），
也没有真实手机；执行代理不代替用户驱动鼠标/键盘，也不把夹具层结果冒充产品 PASS。
E6/E10/E11 需要桌面端有人在场按 §4 的点击顺序操作。

## 3. 自动化层证据（可复现）

| 套件 | 命令 | 结果 |
| --- | --- | --- |
| Rust lib | `cargo test --lib`（第三轮修复后） | 2635 passed / **5 failed（均为 main 基线既有失败，见 §5）** |
| IPC 协议 | `cargo test --test codex_desktop_ipc` | 42 passed |
| IPC HTTP | `cargo test --test codex_ipc_remote` | 52 passed |
| 控制面 | `cargo test --test codex_control_remote` | 36 passed |
| Codex E2E（ignored） | `cargo test --test headless_e2e -- --list` | 1 例 `codex_queue_consumed`（未实机执行，零配额） |
| 前端 | `pnpm test` | 151 文件 / 1598 passed / 5 skipped |
| 错误码/双语 | `pnpm check:i18n`、`node scripts/check-error-codes.mjs` | PASS（1417 键；284 码 / 285 键 / legacy 5） |

## 4. 未执行步骤的交接顺序（Windows，需用户在场）

1. 影子 `TUVIS_HOME=<空目录>` 启动 debug Tuvis；`remote.enabled=true`、设置 PIN；
2. 浏览器（手机同款移动端）配对 → 打开 Codex 会话卡（E1）；
3. 让 Desktop 打开目标会话并跑一个**长回合**（忙态）→ 手机发送（E2/E3）；
4. 触发一次 command 审批（让 Codex 跑需要批准的命令）→ 手机批准/拒绝（E4）；
5. `/plan` 发起提问 → 手机作答（E5）；
6. 关闭 Desktop → 手机发送（E6 exec 臂）；再打开 Desktop → 发送（E10 queue 臂）；
7. 两臂各在 H3 关闭时重发，确认 403 `headless_disabled`（E7/E10 拒绝面）；
8. 记录 threadId/turnId 与审计行（`inject_list_audit`）后，用 `codex-ipc-e2e-stop`
   debug 构建 + `TUVIS_E2E_STOP_ACTION=<actionId>` 复现 E11：看到 `reached.json` 后
   强杀影子 Tuvis，重启查 `codex/action` 应为 `unknown` 且队列不自动重发；
9. Desktop 重启（E9）后重新打开会话，确认 owner 换代与 ≤2s 恢复（T9 旧证据的复验）。

## 5. 基线既有失败（不是本轮引入；已用 pristine main 复核）

在 `E:\LLMproject\Github\tuvis-main-baseline`（origin/main `77fcdb48`）上同样失败：

| 测试 | 根因 |
| --- | --- |
| `monitor::claude_parser::title_tests::get_claude_sessions_badge_end_to_end_new_layout` | 测试用 `HOME` 重定向，Windows 的 `dirs::home_dir()` 不吃 `HOME` → 读到真实 profile |
| `monitor::codex_parser::report_flag_tests::report_flag_true_when_task_complete_within_window` | 同上（HOME 隔离在 Windows 失效） |
| `monitor::opencode_parser::v2_tests::report_flag_true_when_child_updated_within_window` | 同上 |
| `services::subagent_detection_tests::{missing_dir_is_supported_empty, existing_dir_lists_agents}` | 同上 |

这些是 Linux/CI 语义用例在 Windows 本机的已知环境差异；本轮未改其被测逻辑。

## 6. Mac 验收交接

- **执行者**：需交接给实际 Mac 执行者（本会话无 Mac 访问，不冒充）。
- **已具备的 Mac 证据（MAC-RELAY）**：只读收敛（首 snapshot rev3、10/10 completed、
  Ready/idle、`stale=false`）；**未具备**：command approval、permissions、
  阻塞式 Plan 问答、Desktop 重启恢复、真实手机主路径。
- **交接清单**：`2026-10-20-codex-app-remaining-handoff.md` §3（命令、环境变量、
  验收表、证据回收目录）。Windows 侧结果不得复制为 Mac 的 PASS。

## 8. 已登记的有意偏差（不视为回归，留待用户裁决 Q1/Q2）

1. **签名开关不适用于 Codex 兼容/Desktop IPC 路**：两条 Codex 发送路径都经
   `normalize::compose_injection` 恒带 `[mobile <花名>]`，而终端注入按
   `remote_message_signature` 开关决定是否带签名。理由是 #117 的「来源标记单点组装」
   与 Desktop IPC 审计溯源；影响仅是 token 与终端可见文本，不影响投递安全。是否让
   Codex 路也受该开关约束，留待用户裁决（复审 Q1）。
2. ~~花名白名单对终端四家同样生效~~ **第三轮复审已修复**：`compose_injection` 不再按
   花名拒绝（emoji/撇号/冒号/组合音标在终端、Desktop IPC、原生 exe 均可发送）；安全门只
   存在于 Windows cmd 垫片的**最终 argv**（`argv_shim_refusal` / `cmd_shim_body_refusal`，
   只拒 `CMD_SHIM_METACHARS` 且仅 `via_cmd_shim` + 载荷进 argv 时）。注册/改名点不加限制，
   也不存在「昵称仅拒绝 cmd 元字符」的全局规则——旧表述已删除。

## 7. 结论

本轮交付的**自动化层**（IPC 协议 / HTTP 集成 / 控制面 / 前端接线 / 双语与错误码门禁）
全部通过；第三轮审查的 7 项缺陷已修复并有确定性回归（见复审 §10）。**PRODUCT 层双平台
适用表仍未完成**，Windows 需用户在场按 §4 操作、Mac 需实际执行者按交接清单跑。按计划
纪律，不把本次交付宣称为「冻结 spec F1–F8 双平台真机验收完成」。
