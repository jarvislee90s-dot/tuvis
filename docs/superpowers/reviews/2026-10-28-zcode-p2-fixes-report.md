# ZCode 凭据复用 · 8 项 P2 修复报告

日期：2026-10-28。基线：`d66abf9e`（上一轮 C1–H3 + 终审修整）。最终代码 HEAD：`5b790daf`。
范围：定点复审确认的 8 项 P2，全部先补交错失败测试、最小修复、独立复审；未改 MASTER、未 push/merge/tag、未访问真实保险箱/OAuth/relay。

## 一、结论

- **8 项 P2 全部 ADDRESSED**，整批终审无 Critical。
- 逐项复审中又发现并修复 3 个批内回归/残留：P2-1 的“内存新 JWT 取消删除义务”、P2-4 的“先删身份后拆 relay 的在途握手”、P2-5 的“退出路径补唤醒导致退出新连”。
- 退出路径进一步加 `shutting_down` 单调闸，覆盖在途握手/退避中 worker，退出后不再自动重连；普通远程停服与快速重开仍保留唤醒。
- 最终判定：**代码可合并、实机待验**；三平台真实保险箱/真实 OAuth/重启/401 仍未跑。

## 二、逐项修复与提交

| 项 | 修复 | 提交 |
| --- | --- | --- |
| P2-1 忘记删除被新授权取消 | `purge_pending` 删除义务；purge 不再按 epoch 跳过；save 成功在持 `store_lock` 的阻塞任务内提交 `remembered/restore_blocked/purge_pending`；authorize/restore 先 flush；保存失败不取消义务 | `11e93db3` → `ed493631` |
| P2-2 debug/正式恢复标记共用 | `restore_allowed_key_for(debug)`：prod `remote.zcode_restore_allowed`、debug `.dev`；settings 测试去硬编码；设计/验收限定“debug 不再读写旧共享键” | `11e93db3` |
| P2-3 config 清理窗口可连新目标 | `begin_config_change` 锁内置 `closing` + `CloseGuard`，覆盖 `backend.reset`/auth_status，末态按代际落库，drop 后 wake；旧 guard 不清新窗口 | `745332bb` |
| P2-4 forget 关闭保护过短 | `quiesce_disconnect`（disconnect→等 connect_lock→再 disconnect）先于 `backend.forget`；guard 覆盖静默+删除；真实 `AuthService` 慢删除用例 | `745332bb` → `8b5bd545` |
| P2-5 快关快开丢唤醒 | `finish_disconnect`/`config_changed` 清位后 wake；退出改用 silent 入口；`shutting_down` 闸阻止在途/退避 worker 重连 | `745332bb` → `0808f962` → `5b790daf` |
| P2-6 KICKED 晚写覆盖新意图 | Hub 意图代际 `Arc<AtomicU64>`；`write_auto_intent`（Latest 锁内预占；Captured 锁内复核）；6 个用户写点+KICKED 泵统一；晚写跳过，用户写错误上抛 | `745332bb`（conflict 写改 Captured 在 `8b5bd545`） |
| P2-7 启动双重 restore | `auto_restore_attempted` 入口置位（失败也置），worker 只读内存快照；显式 connect 仍可重试 | `11e93db3` |
| P2-8 忘记失败 UI 无法重试 | `canForget = auth !== null && (authorized || remembered || problem !== null)`；auth=null 仍禁用；删除失败可连续重试 | `83986583` |

关键 RED 证据（均实现前真实失败）：
- P2-1：删除义务零执行、`storage_failed` 返回 unknown、restore 不重试删除；修整后新增“未保存内存 JWT 仍删旧记录”“save 成功后 purge 不删新记录”“save 失败重启不恢复”“失败不复活义务”。
- P2-2：debug 键仍是旧共享键。
- P2-3/4：reset/forget 途中 `connect=Ok(Paired)`；P2-4 修整前“删除先于第一次 disconnect”（`forget_calls==0` 时 `disconnect_calls==0`）。
- P2-5：finish 后等待超时、不重连；退出 silent 反证 `wake=true` 会重新 paired。
- P2-6：用户写未预占更高代、旧代延迟写未跳过；KICKED 事件闸复现晚写覆盖 true。
- P2-7：启动 restore 计数 2（含成功/锁定/失败/显式连接）。
- P2-8：删除失败态按钮 disabled。

## 三、最终自动门禁（HEAD `5b790daf`）

| 门禁 | 结果 |
| --- | --- |
| `cargo test --features marker-helper` | **3941 passed / 0 failed / 46 ignored**，EXIT 0 |
| `cargo test --tests --features marker-helper,zcode-bridge-evidence` | **3960 passed / 0 failed / 46 ignored**，EXIT 0 |
| `cargo test --lib --features marker-helper,zcode-bridge-evidence` | **3427 passed / 0 failed / 38 ignored** |
| 集成 4 套件 | `zcode_control_remote` 25、`zcode_control_e2e` 8、`zcode_config_remote` 3、`zcode_read_remote` 5，全 0 failed |
| clippy 普通/证据 `--all-targets -D warnings` | 两构建 **0 warning**，EXIT 0 |
| `cargo fmt --check` | EXIT 0 |
| `pnpm test` | **194 files / 2366 passed / 5 skipped**，EXIT 0 |
| `pnpm check` | EXIT 0（format/lint/i18n/error-codes/tsc+build） |
| `pnpm build:mobile` | EXIT 0 |

环境说明：本机默认 `~/.cargo` 被沙箱拒绝，Rust 门禁使用 `CARGO_HOME=/tmp/dsh-cargo-home`。证据构建的 doctest 在该临时 CARGO_HOME 下 rustdoc 无法解析 extern crate（普通全量 doctest 通过），因此证据构建以 `--tests`/`--lib` 取证；一次 relay 半开写测试在并行负载下超时，单独复跑 PASS，属既有 timing 敏感测试。

## 四、独立审查

- Phase A：首次 Spec ❌（P2-1 内存捷径），修整后 **可提交**。
- Phase B：首次 Spec ❌（P2-4 在途握手），`8b5bd545` 后 **通过**；退出唤醒回归 `0808f962`、在途/退避 `5b790daf` 均 scoped 通过。
- Phase C：**Spec ✅ / APPROVE（可合并）**。
- 整批终审：8 项全 ADDRESSED，无 Critical；Important-1（退出补唤醒）已修，Important-2（`config_changed` 的 stale `backend.reset` 无代际归属）为既有残留，记录不改，建议后续单列。

## 五、剩余边界

- **平台未验**：macOS Keychain、Windows Credential Manager（2,560 边界）、Linux Secret Service、真实 OAuth/relay、真实进程重启、真实 401/撤销、双手机并发，全部 `awaiting_platform_validation` / `not_run`，未标 PASS。
- `config_changed` 的 `backend.reset` 仍只靠末态展示代际复核，未把 reset 纳入 `CloseGuard`/disconnect 归属；极端重叠下陈旧 reset 理论上可断开新连接（既有残留，未在本批扩大）。
- P2-1：`purge_pending` 仅进程内；跨重启依赖 `delete()` 先写 `restore_allowed=false`。进程恰在 purge 拿到 `store_lock` 前退出，重启仍可能恢复旧记录（storage failure 极端窗口）。
- P2-6：`Latest` 写失败不回滚代际（避免 ABA）；失败如实回 `storage_failed`，内存暂停仍封自动连接。
- P2-8：所有非 null problem 都启用忘记按钮（含 expired/revoked/locked），属幂等重试入口，非安全风险。
- P2-7：启动锁定/暂不可用后不自动重试，设计明确需用户点击连接。
- 验收报告仍以 `328c9395` 为基线；本报告补齐 2026-10-28 P2 门禁。

## 六、提交与工作树

- 本轮提交：`11e93db3`、`ed493631`、`745332bb`、`83986583`、`8b5bd545`、`0808f962`、`5b790daf`。
- 最终代码 HEAD：`5b790daf`；本报告提交后文档 HEAD 另计。
- 工作树：提交前干净；未 push/merge/tag；`docs/MASTER-PLAN.md` 未改。
- 逐项账本：`.superpowers/sdd/2026-10-28-zcode-p2-fixes/{progress,phase-A-report,phase-B-report,phase-C-report}.md`（gitignore，不入库）。
