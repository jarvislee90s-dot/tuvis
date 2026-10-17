# Codex Desktop IPC · Windows 原生验证结果报告（第一批 + 阻塞项修复）

> 日期：2026-10-17 · 执行环境：**Windows 11 家庭版 中文版 10.0.26200（x64）**，原生 PowerShell 5.1.26100.9711
> 仓库/分支：`E:\LLMproject\Github\mam-worktree2` · `codex/codex-desktop-ipc`（PR #165 → 目标分支 `codex/headless-main-integration`）
> 验证基线：修复前 HEAD = `197e0b0e`（返工链 `3b9cdc7..1044e225` 之后 1 个文档提交，与云端同步 0/0）
> 依据文档：`docs/superpowers/reviews/2026-10-17-codex-desktop-ipc-windows-validation.md`（本批范围与场景矩阵）、同目录 `…-review.md`、`docs/superpowers/specs/2026-10-16-codex-desktop-ipc-remote-control-design.md`、`docs/superpowers/plans/2026-10-16-codex-desktop-ipc-remote-control.md`、`docs/MASTER-PLAN.md`
> 纪律：全程假 Desktop + 内存库/tempdir；**未控制真实 Thread、未重启 Desktop、未启动写真实账本的 Tuvis、未 push/merge main、未改生产 flag**；本文档与修复代码同一提交入库。

---

## 1. 本批范围

1. 跑完验证文档「第一批：手工测验前的 Windows 自动/只读验证」全部 pnpm/cargo 命令，逐条记录 **实际 exit code 与实际测试数**（文档预期：前端 152 文件/1635 绿；集成 29/23/36 绿；lib 2854 绿）。
2. 逐行过场景矩阵 13 行（含〔返工新增/改口〕行）。
3. 可只读核实真实 Desktop：exe 路径、FileVersion/ProductVersion、管道服务端进程身份、动态 profile 是否精确命中 `26.1013.3656.0`。
4. 发现红 → 单跑复现、留存日志；**不在验证分支上顺手改码**（修复单独裁决、单独成提交）。

## 2. 执行环境与真实 Desktop 事实（只读）

| 项 | 值 |
| --- | --- |
| OS / 工具链 | Windows 11 10.0.26200 x64；node v26.8.1；pnpm 11.13.0；rustc/cargo 1.97.0；git 2.56.0.windows.2（该构建只支持 schannel） |
| Desktop 包 | `OpenAI.Codex` **26.1013.4894.0**（MSIX；`InstallLocation = C:\Program Files\WindowsApps\OpenAI.Codex_26.1013.4894.0_x64__2p2nqsd0c76g0`） |
| exe 路径 | `…\OpenAI.Codex_26.1013.4894.0_x64__2p2nqsd0c76g0\app\ChatGPT.exe` |
| FileVersion / ProductVersion | **`155.0.8059.80`** / **`155.0.8059.80`**（字符串与 VS_FIXEDFILEINFO 四段一致） |
| 管道 | `\\.\pipe\codex-ipc`；`GetNamedPipeServerProcessId` → 服务端 **PID 12372（ChatGPT.exe）** |
| 与 spec 已验证基线 | **不符**：基线 `26.1013.3656.0`，本机包 `26.1013.4894.0`；exe 版本资源是 Chromium/Electron 版本 |
| 旁证 | `app\Codex.exe`（20 KB 存根）与 CLI `codex.exe` **无版本资源**；`app\chrome_proxy.exe` 同为 `155.0.8059.80`；app 目录含 `chrome.dll`(329 MB) + `155.0.8059.80.manifest` |
| app 版本真源 | MSIX `AppxManifest.xml` 的 `Identity/@Version` = `26.1013.4894.0`（UTF-8 with BOM，8671 B，非管理员可读） |
| 探测结束时 | Desktop 已被关闭（ChatGPT 进程数 0）→ 最终探针里 `resolve()` 返回 `build=None`（**正确的保守行为**：无 Desktop = 无 build） |

## 3. 自动门禁结果（逐条命令 / 实际 exit / 实际数）

### 3.1 前端（仓库根）

| 命令 | exit | 实际结果 |
| --- | --- | --- |
| `pnpm install --frozen-lockfile` | 0 | Lockfile 最新，`Done in 4.1s` |
| `pnpm build:mobile` | 0 | tsc + vite `✓ built in 1.75s`（**先于** Rust 测试执行 ✓） |
| `pnpm test`（第 1 次） | **1** | **152 文件 / 1634 passed / 1 failed / 5 skipped（1640）**；红=`SessionDetail.test.tsx` 滚动补偿（`expected 1000 to be 4000`） |
| `pnpm test`（第 2、3 次，空闲） | 0 | **152 文件 / 1635 passed / 5 skipped** ✓ 与文档预期一致 |
| `pnpm check` | 0 | format ✓ · lint ✓ · i18n ✓（1606 键）· 错误码对账 ✓（454 码 / errors 455 键 / legacy 5 / 未迁移启发式 440）· build ✓ |

### 3.2 后端（`src-tauri`，均为 `--features marker-helper`）

| 命令 | exit | 实际结果 |
| --- | --- | --- |
| `cargo check --all-targets` | 0 | 0 错误 |
| `cargo clippy --all-targets -- -D warnings` | 0 | 0 警告 |
| `cargo test --lib … codex_desktop_ipc::transport::windows::tests::` | 0 | 真 named pipe 往返 **1 passed**（Desktop 在跑时执行，互不连接） |
| `cargo test --test codex_desktop_ipc --test codex_control_remote --test codex_ipc_remote` | 0 | **29 / 23 / 36 passed，0 failed** ✓ 与文档预期完全一致 |
| `cargo test --lib`（修复前） | **101** | **2883 passed / 1 failed / 46 ignored**；红=`remote::server::tests::codex_cmd_shim_metachar_body_is_refused_before_probe_and_spawn`（`server.rs:8201` 400≠200） |
| 该红用例单跑（`--exact --nocapture`） | 101 | `0 passed; 1 failed` → **确定性红，非 flake** |
| **修复后** `cargo test --lib` | **0** | **2887 passed / 0 failed / 46 ignored** ✓ |
| **修复后** 定向（F2 用例 + 3 条新增解析单测 + echo） | 0 | **5 passed / 0 failed** |

> 文档漂移：验证文档写 `cargo test --lib` 预期 **2854 passed / 0 failed**，实测修复前 2883+1、修复后 2887+0（总数 2933）。前端 1635 与实测一致。

## 4. 场景矩阵逐行结论

证据等级：`测试` = 分支内自动化用例（本轮原生 Windows 实跑）· `探针` = 仓库外探针**直接调用生产函数**（真 named pipe）· `实测` = 只读原生探测。

| # | 场景（含〔返工新增/改口〕） | 结论 | 证据 |
| --- | --- | --- | --- |
| 1 | server 唯一实例先忙后释放 | **PASS** | 探针 P2：忙态第二客户端 `raw_os_error=231` → 释放后生产 `connect()` **739.6 ms 成功**；P3：持续忙 → **3.005 s 诚实 `Timeout`**（不误判成功）。代码判据 `windows.rs` 用 SDK 常量 `ERROR_PIPE_BUSY`（231）+ 50×50 ms 重试 + 3 s 外层超时 |
| 2 | 同一 handle reader 等待，writer 发送 | **PASS** | 探针 P4（真 pipe）：读挂起时写 4096 B **33.5 µs** 完成、`ECHO-OK!` 回显一致、无死锁；测试 `discovery_is_answered_while_write_waits` |
| 3 | 4-byte 头分块 / 中文跨块 / 两帧粘连 | **PASS** | 探针 P5（真 pipe + 生产 codec）：逐字节写（头 1+1+1+1）+ `中文跨块🐇` 跨块 + 同写两帧粘连 → **3 帧内容逐字节正确**；测试 `fragmented_and_coalesced_frames`、`coalesced_frames_in_single_push_decode_in_order`、`zero_length_/oversize_length_rejected_before_allocation`、`headerless_accumulation_is_bounded`、`frame_body_must_be_utf8_and_json` |
| 4 | 写前断连 vs 写出半帧后断连 | **PASS** | 测试 `partial_write_is_unknown`、`half_frame_then_eof_is_unknown`、`peer_eof_after_write_marks_unknown` |
| 5 | 回包丢失 / 等待者取消〔改口〕 | **PASS** | 测试 `aborted_http_waiter_does_not_lose_dispatched_write`（断言终态 **Accepted**+turnId，非 Reserved）、`aborted_waiter_convergence_unblocks_queue`、`stale_reserved_sweep_filters_dimension_and_age`、`finish_persist_failure_keeps_reserved_reports_unknown` |
| 6 | owner A/B、同 socket 失效广播〔改口〕 | **PASS** | 测试 8 例（旧 lease 全失效 / 旧帧丢弃 / 旧卡 409 零写 / following-status 按关注集回复 / connection-reset 全 stale）；**断线→退避→自动重建关注**由探针 **P6/P7** 补齐：无注入时 kill 后 **0.31 ms 重连、0.74 ms 重新 following**；注入 attempt#2/#3/#4 失败时 **1.005 s / 2.004 s / 4.013 s** 阶梯后退避、第 5 次成功并自动重新 following + 新 snapshot 恢复（全程无人工 ensure/GET） |
| 7 | 真实形状 snapshot/patch/result | **PASS** | 测试 `real_root_path_patch_updates_mirror`（path **无** `conversationState` 前缀）、`streaming_replace_updates_same_bubble`、`revision_gap_republishes_stale_and_refollows`、`history_incomplete_triggers_read_only_load`、`nested_turn_receipt_yields_turn_id`（`result.result.turn.id`→`turn-nested`） |
| 8 | busy 首 snapshot / 旧 idle 更新〔改口〕 | **PASS** | 测试 `first_post_with_active_snapshot_queues_without_start_turn`、`turn_restriction_needs_matching_completion_evidence`、`turn_restriction_survives_restart_until_completion`、`receipt_without_turn_id_keeps_restriction`、`reconcile_requires_completion_evidence_not_idle`、`busy_fifo_allows_pending_response` |
| 9 | 重建所有内存后有持久 queued/fence〔改口〕 | **PASS** | 测试 `hub_restart_recovery_replays_queued_without_details`、`periodic_reconcile_dispatches_newly_queued`、`restart_reserved_becomes_unknown`、`lifecycle_queue_only_survives_hub_stop` |
| 10 | 动态 profile 三段〔新增〕 | **修复前 FAIL → 修复后链通**（真机"已验证 build"段仍 NOT_TRIGGERED，见 §5 F1） | 修复前：生产 `resolve()` 返回 `build=Some("155.0.8059.80")` → `profile_for` MISS → `closed()`。修复后：探针 P1b 真路径 `Some("26.1013.4894.0")`；P8 合成包目录 →`26.1013.3656.0`→ `profile_for` **HIT**（caps send/state/command/blocking_input），反例 `155.0.8059.80` 不命中；单测 3 条 |
| 11 | FIFO 只收 send / 交互行不混队〔新增〕 | **PASS** | 测试 `dequeue_queries_only_take_send_rows`、`interaction_row_left_queued_never_becomes_send_head`、`audit_insert_rejection_rolls_back_reserve_and_keeps_queued` |
| 12 | 回环 HTTP 无 cookie/吊销、越权参数 | **PASS** | 测试 `new_routes_require_device_cookie`、`revoked_device_gets_403_on_new_routes`、`pending_response_rejects_owner_params`、`queue_view_and_retract_auth_matrix`、`queue_retract_ownership_race_and_tombstone`、`audit_never_leaks_secrets`、`sse_stream_snapshots_and_revocation_closes` |
| 13 | 同 ID 重放 / 未知 build / H3 开关〔改口〕 | **PASS** | 测试 `same_action_never_sends_twice`、`unknown_build_send_routes_through_ledger_dedup`、`unknown_replay_blocked`、`app_repost_always_blocked_for_codex_app`、`app_repost_blocked_when_native_live`、`send_h3_off_ipc_unavailable_compat_refuses`、`send_not_sent_hands_off_to_compat_when_h3_on`、`compat_barrier_persist_failure_skips_helper`、`production_chain_send_busy_queue_then_idle_relay`（生产 AdapterDispatch/Hub/Router 全链） |

## 5. 发现与处置

### F1（P1，阻塞第二批）· Windows 版本门读的是 Chromium 版本 → A 写能力永不开启 —— **本次已修**

- 事实：`transport/windows.rs` 旧实现读 `ChatGPT.exe` 的 `VS_FIXEDFILEINFO`（实测 `155.0.8059.80`，Chromium 版本），而 `profile.rs` 只精确匹配 app 包 build（`26.1013.3656.0`）⇒ **永不命中** ⇒ `closed()` ⇒ A 写能力在真实 Desktop 上结构性关闭（连装已验证 build 也打不开）。app 版本真源 = MSIX 包清单 `Identity/@Version`（实测 `26.1013.4894.0`）；对照 macOS 侧读 `Info.plist` 的 `CFBundleShortVersionString`（app 版本，语义正确）——两平台旧实现语义不对称。
- 处置：**版本源改读 MSIX 包清单**（见 §6），保留"来源缺失/多义 → None → 关闭能力"的诚实缺省；新增 3 条单测（含真实清单形状、形状守卫、包根定位）。
- 修复后取证（Desktop 已关闭，无法取运行态端到端）：① 单测 3/3 绿；② 探针 P1b：**真实路径** → `Some("26.1013.4894.0")`；③ 探针 P8：合成包目录 `Version="26.1013.3656.0"` → 生产 `package_version_for_exe` → `profile_for` **HIT**（反例 `155.0.8059.80` 不命中）。
- **残留（需裁决）**：本机装的是 `26.1013.4894.0`，**不在** profile 已验证表内 ⇒ 修复后本机 A 仍**保守关闭**（符合 spec F8「未经验证的版本不盲目启用」）。若要在本机跑第二批真机闭环，必须先裁决一条"如何验证新 build"的路径（例如：把 `26.1013.4894.0` 列入验证目标、在第二批中用一次性/受控方式先验后翻 flag），**不得**直接放宽版本匹配或翻生产 flag。

### F2（P1，Windows-only 确定性回归）· `codex_cmd_shim_metachar_body_is_refused_before_probe_and_spawn` 红 —— **本次已修**

- 现象/机制：期望 200 + `receipt.stage=refused`，实际 **400 `action_id_required`**。该用例是 #117 旧用例（集成点 `d5cc28b1` 已在），本 PR 的 `3be5560e` 让 Codex APP 普通消息必须带 `actionId`（`api.rs` 的 `action_id_required`，防重发双投），用例未同步；它带 `#[cfg(windows)]`，macOS 上不编译 ⇒ 作者机与审查机都看不见，属**本 PR 引入的 Windows-only 回归**。
- 处置：给该用例两臂补 `actionId`（`act-cmdshim-refuse` / `act-cmdshim-control`），判据面（cmd 垫片元字符拒绝、顺序在版本探针之前）不变，并补注释说明 R8 契约。
- 复验：修复后定向 `5 passed / 0 failed`；`cargo test --lib` 全量 **2887 passed / 0 failed / 46 ignored**。

### F3 / F4（覆盖缺口，行为已由探针取证，仍建议补测试）

- F3：R10 复审要求的 4 项 Windows 原生回归，分支内只落地 echo 一项；忙重试 / 预算耗尽超时 / 读阻塞不阻塞写由探针 P2/P3/P4 取证（生产路径）。建议按探针形态补 `#[cfg(windows)]` 测试。
- F4：`adapter.rs` 的 `recovery_loop`（退避 `[1,2,4,8,16,30]` s）分支内无用例；由探针 P6/P7 在真 pipe 上取证（首次即刻重连；失败注入下 1s/2s/4s 阶梯）。建议补测试。

### F5（flake，建议登记）· 前端 `SessionDetail` 滚动用例满载下不稳定

第 1 次全量红 `点加载更早后保持阅读位置`（`expected 1000 to be 4000`）；整文件单跑红 `书签目标在首批窗口之外`；两者**隔离单跑均绿**（各 ~1 s）；第 2/3 次全量 1635 全绿。建议登记 known-flaky 或查明 jsdom 滚动补偿的时序假设。

### F6（文档漂移）· 验证文档的 lib 预期数过期

文档写 2854 passed / 0 failed；实测修复前 2883+1、修复后 2887+0（均 46 ignored）。

## 6. 本次代码改动清单（与报告同一提交）

| 文件 | 改动 | 为什么 |
| --- | --- | --- |
| `src-tauri/src/services/codex_desktop_ipc/transport/windows.rs` | ① `desktop_build()` 的取值源由 exe 版本资源改为**新函数 `package_version_for_exe()`**（从 exe 路径上溯包根读 `AppxManifest.xml`）；② 删除 `exe_file_version()`（VS_FIXEDFILEINFO 读取，含相关 windows-crate 导入）；③ 新增纯解析 `identity_version()`（`<Identity … Version="x.y.z.w">`，形状守卫：≥3 段纯数字）；④ 模块头补实机取证说明；⑤ 测试模块补 `use super::*;` 与 3 条单测（真实清单形状 / 形状守卫 / 包根定位） | 修 F1：旧源是 Chromium 版本，`profile_for` 永不命中，A 写能力结构性关闭。新源 = MSIX 包版本（与 profile 键同形），读不到/多义 → `None` → 关能力（不伪装已验证） |
| `src-tauri/src/remote/server.rs` | `codex_cmd_shim_metachar_body_is_refused_before_probe_and_spawn` 两臂补 `actionId` + 文档注释说明 R8 契约 | 修 F2：R8 后 Codex APP 必带 actionId，旧用例缺失在 Windows 上恒 400；判据面不变 |

- 复验：`cargo check --all-targets` 0 错误 · `cargo clippy -D warnings` 0 警告 · 定向 5/5 绿 · `cargo test --lib` **2887 passed / 0 failed** · 集成 29/23/36 绿（见 §3.2）· 探针 P1b/P8 链通。
- 未做（按纪律）：未翻转任何生产 capability flag；未改 spec/plan/MASTER-PLAN；未扩大版本匹配范围；未动前端（本轮零 TS 改动，故未重跑 `pnpm test` 全量，§3.1 的绿为改动前同分支结果 + 后端改动不影响前端产物）。

## 7. NOT_TRIGGERED / 残留

| 项 | 状态与原因 |
| --- | --- |
| 真实 Desktop 管道上的协议往返（handshake/discovery/following/state） | **NOT_TRIGGERED**：本批按令只用假 Desktop（探针亦只用随机专属管道名）；只读核实范围仅 exe 路径、版本资源、管道服务端 PID（12372）、版本门解析。**未** following、未控制任何真实 Thread |
| Desktop 先关后开的重解析 / Desktop 晚于 Tuvis 启动恢复 A（真机） | **NOT_TRIGGERED**：未获重启/关闭 Desktop 授权。（机制面已证：按连接重算 profile 由探针 P6/P7 在生产 adapter+真 pipe 上验证；生产触发点：`GET /codex/thread → live_projection → ensure_thread → 按连接 resolve()` + 30 s 调度 tick，源码核实） |
| 修复后"Desktop 在跑时 `resolve()` 端到端命中" | **未取到**：最终探针运行时 Desktop 已被关闭（进程数 0）→ `build=None`（正确保守）。取值链已由 P1b（真路径）+ P8（合成包→profile HIT）+ 单测三路取证；下次 Desktop 运行时可直接复跑探针 P1 复核 |
| 真实发送、command/permissions 两类审批、Plan 阻塞问答、重启恢复 | **第二批**，待用户开启手工测验；本机 build `26.1013.4894.0` 未在 profile 已验证表内 ⇒ 需先裁决"新 build 如何验证"（§5 F1 残留），否则真机闭环无法取得 PASS |
| 生产 capability flag | **未翻转**（本批零真机闭环 PASS 证据） |
| 环境事实登记（非仓库缺陷） | 仓库外探针若以 path 依赖链接整个 tuvis lib，会因 tauri/webview2 静态库与 tauri-build 生成的 82 B `msvcrt.lib` 桩冲突而链接失败（`LNK4003` + 87 个 CRT 符号未解析）；绕过办法 = 把生产源文件用 `#[path]` 逐字编入独立探针（本报告所用） |

## 8. 附件与复跑方式

### 8.1 仓库外验证产物（不入库）

根目录：`E:\LLMproject\Github\tuvis-verify-logs\2026-10-17\`

| 文件 | 内容 |
| --- | --- |
| `summary.tsv`、`run-batch1.ps1` | 第一批逐条 exit/耗时/命令行；命令链脚本（可原样复跑） |
| `01…14-*.log` | §3.1/§3.2 各命令完整输出（含 F2 单跑复现 `10-…`；前端隔离单跑 `12/13-…`） |
| `30/32/33/40-*.log` | 修复后的 check / clippy / lib 全量 / 集成测试 |
| `22/24/26/36/38-*-build.log`、`23/25/27/35/37/39-*-run*.log` | 生产路径探针构建与运行（最终 = `39-prodprobe2-final-run.log`） |
| `prodprobe2\`（`Cargo.toml`/`Cargo.lock`/`src\main.rs`） | 探针源码：`#[path]` 编入 `types/codec/profile/mirror/owners/pending/connection/adapter/transport` 生产源文件；复跑：`cd prodprobe2; cargo build --offline; .\target\debug\prodprobe.exe` |
| `20-prodprobe-build.log` | 探针 v1（path 依赖整个 lib）的链接失败全文（环境事实） |

### 8.2 探针最终运行输出（`39-prodprobe2-final-run.log` 摘要）

```text
P1  生产 resolve()：path=\\.\pipe\codex-ipc, build=None（探测时 Desktop 已关闭 → 正确保守）
P1  对照 profile_for("windows","26.1013.3656.0") = HIT caps{send,state,command,blocking_input=true}
P1b 生产 package_version_for_exe(真实路径) = Some("26.1013.4894.0")；step0 app\AppxManifest.xml 无 → step1 包根 有 → read OK 8671 B
P8  合成包目录 Version=26.1013.3656.0 → package_version_for_exe=Some(...) → profile_for = HIT；反例 155.0.8059.80 不命中
P2  忙态第二客户端 raw_os_error=231 → 释放后生产 connect 成功，739.6 ms
P3  持续忙 → Err(Timeout("…timed out (3s)")), 3.005 s（诚实失败）
P4  读挂起时写 4096B = 33.5 µs；回显一致 true（全双工无死锁）
P5  逐字节分块 + 中文跨块 + 两帧粘连 → 3 帧全对
P6  断线后 0.31 ms 重连 / 0.74 ms 重新 following；投影 stale=false rev=2
P7  注入 attempt#2/#3/#4 失败 → 间隔 1005 / 2004 / 4013 ms；attempt#5 OK → 自动恢复 stale=false rev=2
```
