# F6 运行时事件覆盖能力表实施计划

关联规格：[F6 运行时事件覆盖能力表](../specs/2026-11-01-f6-runtime-event-coverage.md)

## 变更边界

- 最小行为差距：cadence 当前只知道固定的 `dsh/openclaw` 缺口，不知道已启用 WorkBuddy 空闲宿主（无新鲜心跳）/ZCode 的无信号状态，也不知道 Claude/Kimi 当前启动所需的 hook 产物是否真正可用；bash hook 同 sid 原地写文件不会可靠改变目录 mtime。
- 行为所在位置：覆盖分类与未覆盖节奏判断在 `src-tauri/src/adapter/`；hook 注册结果和 bash 写入在 `src-tauri/src/monitor/hooks.rs`；事件目录签名回归在 `src-tauri/src/adapter/wakeup.rs`。
- 预期文件：新增 `adapter/wakeup_coverage.rs`；修改 `adapter/mod.rs`、`adapter/scan_bus.rs`、`monitor/hooks.rs`、`adapter/wakeup.rs`；只在这些 Rust 模块的单元测试中增加回归。
- 不做：不增加进程扫描、固定文件监控、事件清理/合批改动，不修改 hook JSON 语义，不改 dsh/OpenClaw 的 Idle 兜底。

## 任务 1：覆盖表和节奏判定

**文件：**新增 `src-tauri/src/adapter/wakeup_coverage.rs`；修改 `src-tauri/src/adapter/mod.rs`、`src-tauri/src/adapter/scan_bus.rs`。

1. 先新增失败测试：
   - `coverage_table_matches_registered_tool_ids`：覆盖表排序后与 `adapter::TOOL_IDS` 完全相等。
   - `runtime_hook_requires_current_success`：默认 Claude/Kimi 未覆盖；当前启动核验/注册成功后覆盖；清空状态后恢复未覆盖。
   - `codex_and_no_signal_tools_stay_uncovered`：Codex、dsh、OpenClaw、WorkBuddy、ZCode 一直归入兜底；OpenCode 为固定文件覆盖。
   - `unknown_tool_is_uncovered`：未知 tool id 不得进入 Away 资格。
   - `workbuddy_host_without_fresh_heartbeat_is_still_running`：WorkBuddy adapter 已启用、没有新鲜会话心跳，但共享宿主判定为运行时，运行工具集合仍包含 WorkBuddy。
   - `workbuddy_and_zcode_running_block_away`：二者进程出现在运行工具集合时，未覆盖判定为真；Idle/Finished 会话不单独触发，活跃会话仍进入 Active。
2. 运行定向 Rust 测试并确认失败原因是覆盖表/运行时状态行为缺失。
3. 实现穷尽覆盖表、进程内 hook 覆盖集合及纯判定函数；`scan_bus` 每轮将运行工具和快照活跃会话交给该判定，删除旧的局部硬编码名单。
4. 在现有两段刷新后的 `System` 快照中，仅当 WorkBuddy adapter 已启用时调用已存在的 `tool_host_alive_in(system, "workbuddy")`，将宿主事实并入运行工具集合；不创建第二个 `System` 或刷新线程。ZCode 宿主仍来自启用 adapter 的 `find_processes`。
5. 重跑上述定向测试，确认全部通过。

## 任务 2：把注册结果接到运行时覆盖

**文件：**修改 `src-tauri/src/monitor/hooks.rs`。

1. 先新增失败测试：
   - 覆盖条件纯函数测试：Claude 只在实际生效的 helper 或内容等于托管版本的 bash 脚本在场，且配置核验/注册成功时覆盖；Kimi 还必须满足实际 helper 命令生效；任一条件失败为未覆盖；Codex 不因配置写入成功而覆盖。
   - 脚本文件内容测试：旧版原地写脚本虽然存在，仍判为当前脚本不可用；精确匹配本轮 `HOOK_SCRIPT` 才通过。
   - 运行时集合测试：默认空集；Claude/Kimi 成功时加入；启动重置清除上次状态；注册失败及 helper 缺席不加入。
2. 运行覆盖状态定向测试，确认失败在尚不存在的覆盖谓词/状态行为上。
3. 在 `register_all_hooks` 开始处清理旧进程内状态，并在 verified / `Ok(())` 两条成功路径更新 Claude/Kimi 状态；所有 continue/error 路径保守留为未覆盖。Codex 的存量配置与信任提醒逻辑保持原样，但不进入覆盖集合。
4. 重跑 hook 定向测试。

## 任务 3：shell hook 原子替换

**文件：**修改 `src-tauri/src/monitor/hooks.rs`。

1. 先新增测试 `shell_hook_replaces_same_sid_through_same_directory_temp_file`：脚本文本必须在 `$EVENTS_DIR` 生成唯一 temp、写完后 `mv` 到目标、并在失败/退出时清理 temp；写入与替换失败须返回非零；禁止使用 `> "$EVENTS_DIR/$SESSION_ID.json"` 原地截断。
2. 运行该测试并确认旧脚本因原地重定向失败。
3. 只替换 shell 脚本的发布步骤：白名单校验后 `mktemp "$EVENTS_DIR/.${SESSION_ID}.XXXXXX"`，trap 清理，`printf` 完整行至 temp，再同目录 `mv -f` 至 sid 目标；任一步骤失败时退出非零并由 trap 清掉临时文件。
4. 重跑 hook 脚本测试及该模块测试。

## 任务 4：Idle 旧 sid 事件目录唤醒回归

**文件：**修改 `src-tauri/src/adapter/wakeup.rs`。

1. 先新增契约测试 `same_sid_atomic_replace_wakes_without_per_file_monitoring`：签名只包含事件目录、首次观察后对已有 sid 执行同目录原子替换，再次观察必须唤醒。该测试验证当前签名对目录变化的依赖，可在改脚本前通过。
2. 先运行契约测试，确认生产签名包含目录 stamp；平台 mtime 分辨率不足时只在夹具中等待一个短间隔。
3. 任务 3 的 shell 文本测试是本组写侧缺陷的红灯；原子替换实现通过后，再运行本契约测试，确认写侧行为与既有目录信号闭环。
4. 重跑 wakeup 模块测试。
5. 首次监控签名建立后排一次启动 Event 对账，封住 ColdStart 读取事件与首拍基线落位之间的窗口；用 `first_wakeup_baseline_requests_one_startup_reconciliation` 锁定只触发一次。

## 最终检查

按顺序执行：

```powershell
cd src-tauri; cargo test adapter::wakeup_coverage::tests --lib
cd src-tauri; cargo test adapter::scan_bus::tests --lib
cd src-tauri; cargo test monitor::hooks::tests --lib
cd src-tauri; cargo test adapter::wakeup::tests --lib
cd src-tauri; cargo fmt -- --check
cd src-tauri; cargo clippy --lib -- -D warnings
pnpm check
git diff --check
```

每条命令以退出码 0 为通过；若仓库既有告警仍被 `pnpm check` 报告，应列明告警文件且区分是否为本组改动。
