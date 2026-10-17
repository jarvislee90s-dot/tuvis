# Windows Agent 验证建议与可复制提示词

日期：2026-10-17（同日返工后修订）。适用分支：`codex/codex-desktop-ipc`。
**基准已更新：审计 R1–R12 返工完成且 code-reviewer 复审 Approve，修复后 HEAD = `1044e225`**（返工链 `3b9cdc7..1044e225` 共 8 提交，见 `2026-10-17-codex-desktop-ipc-review.md` 与其复核报告）。云端入口：PR `codex/codex-desktop-ipc` → `codex/headless-main-integration`（集成分支含 #117 全量 + main 合流；PR diff 只含 IPC 变更）。

## 第一批：手工测验前的Windows自动/只读验证

### 原「必须先修的Windows代码」——已全部修复，转为验证项

1. ~~ERROR_PIPE_BUSY 2231~~ → **已修**（`transport/windows.rs` 用 `windows::Win32::Foundation::ERROR_PIPE_BUSY` SDK 常量，即 231；忙实例释放后的重试与超时路径恢复生效）。验证：grep 无手写数字；跑下方忙重试场景。
2. ~~假 pipe 占用生产名~~ → **已修**（`connects_to_fake_pipe_server_and_echoes` 用随机专属名 `\\.\pipe\codex-ipc-test-{uuid}` + endpoint 注入）。验证：Desktop 开着时跑该测试安全通过，且不连生产管道。
3. ~~CI 无 Windows 原生 job~~ → **已修**（`.github/workflows/ci.yml` 新增 `windows-native` job：windows-latest 跑 `cargo test --lib codex_desktop_ipc::transport::windows::`）。验证：PR 的该 job 绿；本地仍按下方命令原生跑一遍（CI 只覆盖假 pipe 往返，不替代本批场景矩阵）。

### 运行与记录

先记录实际 HEAD（应为 `1044e225` 或其后仅文档/验证提交）、Windows版本/架构、Rust/Node/pnpm版本，确认build:mobile先于Rust测试。不要切换/清理用户有未提交内容的checkout。

```text
pnpm install --frozen-lockfile
pnpm build:mobile
pnpm test            # 预期 152 文件（数量随批增长，以分支 HEAD 实际为准；本轮 1639 passed / 5 skipped）
pnpm check           # 含 format/lint/i18n/error-codes/build

# 以下在src-tauri目录执行
cargo check --all-targets --features marker-helper
cargo clippy --all-targets --features marker-helper -- -D warnings
cargo test --lib --features marker-helper codex_desktop_ipc::transport::windows::tests:: -- --nocapture
cargo test --features marker-helper --test codex_desktop_ipc --test codex_control_remote --test codex_ipc_remote
# 预期：29 / 23 / 36 全绿（若个别红：单跑复现并留存日志，不在本批修码——回传主聊天裁决）
cargo test --lib --features marker-helper          # 预期 0 failed；**passed 数按平台/feature 各自记录**（macOS 与 Windows 的 cfg 差异 + 修复提交都会改变总数，不写死跨平台相同数字——2026-10-17 F6 教训）
```

已知非阻塞（复审登记，遇到勿当新回归）：`client_status_changed_invalidates_all_threads_of_owner` 与 `session_open_endpoint_reuses_trusted_casing_silently` 满载下曾 flaky，修复后单跑/全量均绿；再红则单跑复现并回传。

新增/复验这些真实named-pipe场景（server仍是假Desktop，不改用户Thread）。**标注〔返工新增/改口〕的行为是本轮返工新落地的合同，Windows 实测是它们的首次原生验证**：

| 场景                              | 必须断言                                                            |
| --------------------------------- | ------------------------------------------------------------------- |
| server唯一实例先忙后释放          | 捕获231并重试，3s预算内释放可连接；持续忙诚实超时，不误判成功       |
| 同一handle reader等待，writer发送 | 全双工不死锁；单writer不交错帧                                      |
| 4-byte头分块、中文跨块、两帧粘连  | 完整恢复正确JSON；0/超限先拒绝不大分配                              |
| 写前断连 vs 写出半帧后断连        | 前者证明零投递才NotSent；后者Unknown，无IPC重发和fallback           |
| 回包丢失/handler等待者取消〔返工改口〕 | 独立派发任务落账且**终态 Accepted（非 Reserved）**；等待者取消后同 thread 第二条消息仍能派发（收敛回归 `aborted_waiter_convergence_unblocks_queue`）；超龄 reserved 由周期对账收敛为 Unknown，不永久滞留 |
| owner A/B、同socket失效广播〔返工改口〕 | 两Thread旧lease全失效、旧卡409、**A 断开广播即全量失效并发布 stale 投影**；state 帧按 sourceClientId 绑定（旧 owner 迟到帧丢弃）；断线后退避恢复自动重建关注（无需人工 GET） |
| 真实形状snapshot/patch/result     | **镜像按真实形状寻址（无 conversationState 前缀，如 `["turnHistory","history",…]`、`["threadRuntimeStatus","type"]`）**；start-turn真实嵌套回执（`result.result.turn.id`）可提取turnId；缺口→stale→自动重新following→新snapshot恢复 |
| busy首次snapshot/旧idle更新〔返工改口〕 | 普通消息不抢Turn（首次直接POST且snapshot active只入队零start-turn）；后继只在**对应turnId终态**后派发（Accepted绑定持久、重启后仍等原Turn终态；缺turnId保守不放行） |
| 重建所有内存后有持久queued/fence〔返工改口〕 | **重启 Tuvis 进程（hub 重建）后不手工flush、不打开详情也能恢复关注/核对/接力**（hub 启动恢复 + 30s 周期对账）；Unknown不重投 |
| 动态profile三段〔返工新增〕       | 无Desktop（A关）→已验证build `26.1013.3656.0`（A开、可写）→未知build（A立即关、零写）→恢复已验证（重开）；**Desktop 晚于 Tuvis 启动：不重启 Tuvis 即可恢复 A**；每步核对实际能力与零/有IPC写 |
| FIFO只收send/交互行不混队〔返工新增〕 | 审批/回答动作永不进普通消息出队（SQL `kind='send'` 过滤）；交互失败残留行不被当普通消息发送 |
| 回环HTTP无cookie/吊销、越权参数   | 全部拒绝；owner/raw method/权限对象不能由HTTP指定                   |
| 同ID重放/未知build/H3开关〔返工改口〕 | **未知build的普通消息也先经统一账本**（同ID重放/进程重启均只投一次且有fence）；只有普通消息可兼容兜底；旧APP问答补投无条件503（含stale/无owner/未知build），GET 不广告 |

可只读核实真实Desktop：进程实际exe路径、FileVersion/ProductVersion、管道服务端进程身份，以及**动态 profile 解析是否精确命中 `26.1013.3656.0`**（含 Desktop 先关后开的重解析）。版本不符先报告，不为跑通测试扩大版本范围或翻生产flag。该只读调查不等于发送/审批实测。

## 第二批：用户开启手工测验后才执行真实Thread写入

全部review阻塞关闭后（已满足），用专用测试项目/Thread逐项记录原计划九格。测试前明确用户可重启Desktop的时机；不修改已有日常Thread的权限或collaboration设置。

- 手机发消息：同原Thread、同turnId，恰好一回合；streaming/history/终态正确；**真实patch以无前缀路径流式更新同气泡**。
- command批准/拒绝：必须有原生command pending，批准真执行，拒绝未执行。
- permissions变体：Windows此前未验证；用实验测试profile完成真实闭环，未触发记NOT_TRIGGERED；不能拿command结果代替。
- Plan阻塞问答：用户在Desktop切Plan，拿到原requestId/问题ID，手机回答后原Turn正确使用答案；Default async不算。
- 两Thread共owner、Desktop关闭/启动/重启、切新owner：不重启Tuvis即可恢复（失效广播+退避恢复+动态profile），旧卡拒绝。
- **Desktop晚启/Tuvis先启**：不重启Tuvis，A 能力随 Desktop 启动自动恢复。
- A失效普通消息fallback、H3关闭拒绝、兼容未完成时后继消息留队（fence 对应 Turn 完成才解除）；审批/回答无自然语言fallback（未知build同样走账本兜底）。
- 发送后丢ack：Unknown、不自动重试、不改通道；重连/刷新/重启无双投；等待者取消后队列仍流动。
- 手机详情：入队立即可见/可撤，NotSent后的下一动作可正常再发，无假的headless运行卡，换token卡不被旧响应锁死。
- **Tuvis 进程重启**：持久 queued/在册fence 自动恢复接力（不打开详情、不手工flush）。

如需运行完整Tuvis，使用受控debug环境和影子账本/采集读源；先核当前代码实际支持的隔离配置。**TUVIS_HOME只在debug/test生效，release不靠它隔离；仅重定向账本也不等于隔离真实采集读源。** 自动named-pipe/API测试优先使用memory DB/tempdir，不启动真实采集循环。生产capability仅在对应build/方法的真实闭环PASS后单独提交开启，不提供用户UI绕门开关。

## 可复制给Windows Agent的提示词

```text
请在Windows原生环境验证Tuvis的Codex Desktop IPC实现。先读取AGENTS.md、MASTER-PLAN、2026-10-16 spec/plan，以及2026-10-17审查报告、其返工复核与本验证文档。

基线已更新：审计R1-R12返工完成且独立复审Approve。从云端同步分支 codex/codex-desktop-ipc（PR 目标 codex/headless-main-integration），记录实际HEAD并核对为返工链 3b9cdc7..1044e225 之后的提交；不要把旧 3b9cdc7 当候选版本，也不要自行 rebase/force-push。

返工已包含：ERROR_PIPE_BUSY 用 SDK 常量 231、假pipe随机专属名、ci.yml windows-native job、mirror 真实形状寻址（无 conversationState 前缀）、owner 失效/断线恢复闭环、profile 按连接动态解析、pending 提交互斥、FIFO 只收 send、Turn 终态绑定（嵌套回执解包）、hub 启动恢复与周期对账、所有 Codex APP 消息统一经账本、fence 先持久、派发独立任务落账与超龄收敛。你的职责是**在原生 Windows 上验证这些合同**，不是再修一遍；发现红先单跑复现并留存日志回传，不在验证分支上改码。

先完成本文第一批自动/只读验证：跑列出的 pnpm/cargo 命令（预期数以分支 HEAD 实际为准并**按平台/feature 分别记录**——集成套件 codex_ipc_remote/codex_control_remote/codex_desktop_ipc 的用例数随返工批增长），明确非零退出与实际测试数；交叉check不等价于原生运行。另：前端 `SessionDetail` 满载滚动用例存在已知 flake（Windows 报告 F5：满载偶发红、隔离单跑恒绿）——首轮全量出现该红时**先隔离复跑再定性**，不直接当回归。再逐行过场景矩阵（含〔返工新增/改口〕行）：真实named-pipe往返、busy 231重试、全双工、分块帧、未知投递与收敛、owner A/B 与恢复、动态profile三段、FIFO、认证与统一账本。全程使用假Desktop与memory DB/tempdir，不控制用户真实Thread、不重启Desktop、不启动写真实账本的Tuvis。

真实发送、两类审批、Plan问答和重启恢复属于第二批，待用户开启手工测验再执行。版本不匹配和NOT_TRIGGERED如实记录，不能强开生产flag。

输出：实际HEAD、Windows/Desktop版本（exe 路径与 FileVersion/ProductVersion）、每条命令与结果、逐场景PASS/FAIL/NOT_TRIGGERED、日志证据路径，以及需返工的最小复现。不要push或merge main。
```
