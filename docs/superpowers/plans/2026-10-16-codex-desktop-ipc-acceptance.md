# Codex Desktop IPC 双平台验收记录（2026-10-16）

> 分支 `codex/codex-desktop-ipc`（自集成点 I = `codex/headless-main-integration@d5cc28b` 开出）。
> 本文件随 Task 10 产出：自动检查结果 + 双平台真机验收表。**「已编码/自动检查通过」与「真机
> PASS」分开陈述；无法触发的格记 NOT_TRIGGERED 并写明阻塞原因，不写「不支持」或「已完成」。**

## 1. 自动检查结果（本机 macOS，Apple Silicon）

| 门禁 | 命令 | 结果 |
| --- | --- | --- |
| 前端测试 | `pnpm test` | **152 文件 / 1622 passed / 5 skipped / 0 failed** |
| 前端静态 | `pnpm check` 各分项 | format✓ lint✓ check:i18n✓（1604 键）build✓ |
| 移动端构建 | `pnpm build:mobile` | **PASS** |
| 错误码对账 | `node scripts/check-error-codes.mjs` | **PASS：454 码 / 454 键对账，棘轮 440 无上升** |
| 后端门禁 | `bash scripts/ci-backend-local.sh` | ①fmt✓ ②check✓ ③clippy -D✓ ④test **2841 passed / 6 failed（见下方既有归因）**；⑤⑥⑦ 因④失败被脚本短路，已单独补跑：Win 交叉 check **0 错误**、Win 交叉 clippy **0 警告** |
| Rust 集成测试 | `cargo test --test codex_{ipc_remote,codex_control_remote,desktop_ipc}` | **25 + 18 + 16 = 59 例全绿**（另有 DAO/store 单测 12 例绿） |
| Windows 交叉 | 同上 | **check 0 错误 / clippy 0 警告**（原生 pipe 测试仍须 Windows 真机跑，见 §4） |

**既有失败归因（非本分支引入，已在干净基线 `a777c5a` 复核）**：
- `inject::headless::gate::tests::probe_env_sets_electron_flag_and_mac_provider_config` ×1
  ——本机 `/Applications/ZCode.app` 在场触发环境依赖分支（Windows 且推导路径不在场的
  断言前提被破坏），环境缺项；
- `remote::server` ×5 —— dsh 夹具使用 Windows 风格 cwd（`E:/t2`）在 macOS 被绝对路径
  门拒绝（#117 自带平台项，与 CI「13 红」同源）。

## 2. 双平台真机验收表

运行环境事实：本执行环境 = macOS（用户日常机，Codex Desktop 26.1013.31256 曾由独立
Probe 验证过协议面）；**无 Windows 真机**；真机 E2E 需要用户在场授权（重启 Desktop 会
打断其正在进行的工作；完整链路还需一个受控的 Tuvis 运行时——按 AGENTS.md 采集域纪律，
**不启动可能写真实用量账本的 Tuvis 开发进程做自动化测试**）。因此本执行会话的自动化
覆盖 = 本地假 IPC 全链集成测试（ScriptedPeer 假 Desktop → mirror → 认证 HTTP → 账本），
下表真机格按实测状态如实记录。

| 格 | Windows | macOS | PASS 判据 | 状态/阻塞 |
| --- | --- | --- | --- | --- |
| connect + owner + following | NOT_TRIGGERED | NOT_TRIGGERED | 版本来自实际官方进程；两 Thread 同 owner 均可跟随 | 需用户在场运行 Tuvis+Desktop（协议面已由独立 Mac Probe OBSERVED 验证；自动化面 = `discover_and_follow_*` 假 IPC 集成测试绿） |
| 手机发普通消息 | NOT_TRIGGERED | NOT_TRIGGERED | 同原 Thread 恰 1 Turn、回执 turnId 与账本对应 | 同上（自动化面 = `native_start_turn_targets_original_thread` + `same_action_never_sends_twice`） |
| streaming / history | NOT_TRIGGERED | NOT_TRIGGERED | `turns=[]` 仍完整显示正史；增量同气泡更新 | 同上（自动化面 = mirror 11 例 + SSE 集成测试） |
| command approve / reject | NOT_TRIGGERED（Probe 已验协议面，产品未复验） | NOT_TRIGGERED（Mac Probe 未触发该变体） | 原 pending 消失、批准真执行/拒绝未执行 | 需真机触发 command 变体（构造器已按 CODE 表实现，`native_approval_variants_*` 绿） |
| permissions approve / reject | NOT_TRIGGERED（Windows probe 未触发） | NOT_TRIGGERED（Mac Probe 已 OBSERVED，产品未复验） | 原权限对象回显、scope=turn、拒绝={} | 同上 |
| Plan blocking input | NOT_TRIGGERED | **必须补验**（Probe 未触发） | 捕获原 request id，提交后原 Turn 继续 | Default 模式下 `features.default_mode_request_user_input=false`，需用户切 Plan 模式配合 |
| Desktop restart / owner 变化 | NOT_TRIGGERED | **必须补验** | 旧 owner 失效、重连重 following、旧卡拒绝、无写重放 | **重启 Desktop 影响用户工作，未获该操作授权**（自动化面 = `owner_rebind_invalidates_old_lease_end_to_end` + epoch/恢复测试绿） |
| fallback | NOT_TRIGGERED | NOT_TRIGGERED | A 不可用且 H3 开 → 仅普通消息走 #117；H3 关 → 诚实拒绝 | 需真机构造 A 不可用条件（自动化面 = `fallback_only_before_dispatch` 等 18 例） |
| 未知结果 | NOT_TRIGGERED | NOT_TRIGGERED | 故障丢响应 → UNKNOWN；重连/刷新/重启无重复动作 | 需真机故障注入（自动化面 = `partial_write_is_unknown`/`timeout_is_unknown`/`restart_reserved_becomes_unknown` 绿） |

**生产 capability flag 结论**：维持 Task 1 基线不变（Windows command/blocking_input=true
其余 false；macOS permissions=true 其余交互 false）——本会话未产生任何真机闭环 PASS
证据，**没有翻转任何 flag**。

## 3. 自动化全链覆盖清单（fake Desktop 集成测试，非真机证据）

- `tests/codex_desktop_ipc.rs`（16 例）：RPC 关联/未知投递边界/discovery 旁路/在飞上限/
  超时/owner 换代/end-to-end rebind/start-turn 键集/两类审批 wire 隔离/阻塞问答绑定。
- `tests/codex_control_remote.rs`（18 例）：账本/FIFO/fence 全场景。
- `tests/codex_ipc_remote.rs`（25 例）：认证矩阵/SSE/三回执形态/降级一次性/审计哨兵/生命周期。
- `tests/mobile/*.test.tsx`：54 例新增（投影/审批卡/队列/发送 actionId 语义）。

## 4. 遗留与待办（交回原聊天 review）

1. 双平台真机 E2E：需用户安排可重启 Desktop 的时机 + 受控 Tuvis 运行时（见 §2 阻塞列）。
2. Windows named pipe 原生测试（`connects_to_fake_pipe_server_and_echoes`）必须在 Windows
   原生运行（CI windows job）；交叉编译只保证编译面。
3. 生产 flag 翻转：仅在各格真机闭环 PASS 后逐格提交。
