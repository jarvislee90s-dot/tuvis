# F6 R-1 事件文件清理并发边界实施计划

关联规格：[F6 R-1 事件文件清理并发边界](../specs/2026-11-01-f6-r1-quarantine-cleanup.md)

## 变更边界

- 最小行为差距：`cleanup_old_events` 在 stat 后直接删原路径，无法确认删掉的是刚检查的对象；`fold_cleanup` 即使没有删除也豁免目录变化；新事件枚举/取 signature 的并发窗口和相等 mtime 会漏唤醒。
- 行为所在位置：`src-tauri/src/adapter/wakeup.rs` 的 `CleanupReport`、`cleanup_old_events`、`run_cleanup_tick` 和 `Baseline::fold_cleanup`。
- 仅修改该 Rust 模块及其单元测试。不触碰 `MASTER-PLAN.md`，不修改 hook writer、事件协议或用户事件目录。
- 文件身份实现限定 Unix `dev+ino` 与 Windows `volume_serial_number+file_index`；其他平台/身份读取失败时只标记不确定，不删文件。

## 任务 1：先锁定竞态回归

**文件：**`src-tauri/src/adapter/wakeup.rs`。

1. 先写以下失败测试，使用 `tempfile::tempdir()`、同目录原子 rename 与 `filetime::set_file_mtime`：
   - `event_created_after_enumeration_wakes`：在清理首次成员枚举后写入新的 `sid.json`，清理折叠必须唤醒。
   - `event_with_mtime_equal_to_since_is_fresh`：将新事件 mtime 精确设置为 `since`，不得被算作陈旧清理产物。
   - `replacement_after_stale_stat_is_preserved`：先准备 10 分钟陈旧文件，在候选身份已读取后原子替换同路径，运行清理后 canonical 路径仍含新事件，旧身份不能作为成功删除计入。
   - `newer_canonical_event_is_not_overwritten_by_restore`：在 stale 文件 rename 到隔离名后、恢复前，于原路径写入另一条新事件；恢复不得覆盖原路径内容。
   - `non_collision_restore_error_keeps_quarantined_object`：注入 `PermissionDenied`，即使 canonical 路径有普通文件，也不得把非 `AlreadyExists` 错误当成可丢弃隔离对象。
   - `already_exists_with_unknown_identity_keeps_quarantined_object`：即使错误为 `AlreadyExists`，canonical/隔离身份不可核对时仍保留隔离对象。
   - `quarantine_directory_reservation_is_exclusive`：重复/并发分配返回不同隔离目录，已存在的目录不得被替换。
   - `directory_change_without_successful_deletion_wakes`：删除清单为空、清理窗口内有目录/事件文件变化时，`fold_cleanup` 必须唤醒。
   - `unknown_identity_is_kept_and_cleanup_wakes`：身份读取返回不可用时，原路径内容不得被删除，清理报告必须标记不确定。
   - 保留 `cleanup_deletion_alone_does_not_wake` 并增加断言：只把实际删除成功的原路径列入 `removed`。
2. 运行每个新增定向测试，确认当前实现分别因 `remove_file(path)`、严格 `mtime > since` 或无条件目录豁免而失败。

## 任务 2：对象身份与隔离清理

**文件：**`src-tauri/src/adapter/wakeup.rs`。

1. 定义 `FileIdentity`：Unix 存 `(dev, ino)`，Windows 存 `(volume_serial_number, file_index)`；不能取得完整身份返回 `None`。
2. 增加 `EventFileStamp { identity, len, modified }` 与成员快照函数；只枚举普通 `*.json`，读取错误、symlink 或身份未知标记 `uncertain`。
3. 对每个年龄大于 300 秒的候选：保存枚举身份、长度和 mtime 后，用 `std::fs::create_dir` 原子预留 `.tuvis-cleanup-<pid>-<counter>` 隔离目录，再 `rename` 源路径到其中的 `event.tmp`；再次读取隔离文件的身份、长度和 mtime。仅当三项完全相同且仍陈旧时删除隔离文件；仅在删除成功后把原路径记入 `removed`。
4. 对身份变化/不再陈旧的隔离对象，若原路径不存在，使用 `std::fs::hard_link(quarantine, original)` 无覆盖恢复，然后清理隔离文件与空隔离目录。若 hard_link 明确返回 `AlreadyExists` 且 canonical 与隔离对象身份均可读，原路径文件是在隔离 rename 后创建的同 sid 更新，可保留 canonical 并清理已被其取代的隔离文件；`PermissionDenied` 等其他错误、身份不明或目标非普通文件一律保留隔离对象、标记 `uncertain` 并返回唤醒。
5. 保留事件 mtime `>= since` 的 freshness 提示；它只补充信号，不取代身份对账。
6. 重跑任务 1 的对象身份测试，确认隔离对象身份不匹配不会删除新事件。

## 任务 3：清理拍成员对账与基线折叠

**文件：**`src-tauri/src/adapter/wakeup.rs`。

1. 为 `cleanup_old_events` 的测试入口加入同步停点：成员枚举后、候选 identity/mtime 抽取后、移入隔离名后恢复前；生产包装传入 no-op closure。
2. 保存枚举前后成员快照；新增/替换/身份变化而非成功删除解释的路径并入 `fresh`，目录不可读/无法恢复置 `uncertain`。
3. `run_cleanup_tick` 在新签名之后再取一次事件成员快照，吸收“枚举结束后、签名之前”的新 sid 与同路径替换。
4. `fold_cleanup` 只对 `report.removed` 中的已删除路径豁免文件变化。目录 stamp 仅在至少一项删除成功、成员差异全由删除解释、且没有 `fresh` 或 `uncertain` 时豁免；否则按普通签名变化唤醒。
5. 运行新旧 wakeup 测试，确认纯清理不唤醒，无法解释的并发文件变化都会唤醒。

## 最终检查

```powershell
Push-Location src-tauri
cargo test adapter::wakeup::tests --lib
cargo fmt -- --check
cargo clippy --lib -- -D warnings
Pop-Location
pnpm check
git diff --check
```

不得启动真实应用或触碰真实 `~/.tuvis/events`。任何身份未知或无覆盖恢复失败的分支必须保留对象并报告不确定，不能标记为成功清理。
