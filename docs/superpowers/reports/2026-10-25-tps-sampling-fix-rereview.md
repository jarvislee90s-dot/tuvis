# TPS 修复轮复审

日期：2026-10-25。分支：`codex/tps-sampling`。审查范围：`095a11f492c17bc4aeb12feabc027768a26e8a99..a2079f351a6e8d507c9d2c8a7380ef8d7863cd40`。

**限定复审结论：Ready = No。原问题 7 项关闭，R3/R7/R10/R11 尚需收尾，均为 P2。** 修复方向正确，普通增量读取、重复安装恢复、真实事件入口等已改善；当前代码仍不能认定 R1—R11 全部 ADDRESSED。

本轮依据当前设计、计划和首次独立复核报告，只核查修复及直接回归，没有重开七工具全面探测。三个独立评审分别检查来源、安装、统计与曲线；协调者复核生产编排和测试。没有修改实现、用户配置或真实账本，没有发送模型请求、合并、推送、打 tag。

## 1. 已关闭与验证

| 编号 | 结论      | 核对结果                                                                       |
| ---- | --------- | ------------------------------------------------------------------------------ |
| R1   | ADDRESSED | 平台 writer 成对定义，实际 Windows 目标 cargo check 通过                       |
| R2   | ADDRESSED | 重装保留首次接管前旧值，用户后改后显式转交仍重新记录当前用户值；两来源反例覆盖 |
| R4   | ADDRESSED | WorkBuddy 实际本地时间前缀可解析，保留 RFC3339 兼容和坏前缀诊断                |
| R5   | ADDRESSED | Kimi 顶层请求与嵌套结算按真实层级关联，模型归属及可见歧义重试已修              |
| R6   | ADDRESSED | 按来源/宿主/会话/回合联合身份分组，缺必要身份保守降级                          |
| R8   | ADDRESSED | DSH 旧 profile 仅用于包含关系标记，不再整体禁用其他完整生成窗                  |
| R9   | ADDRESSED | TTFT 使用独立成功且首点可对齐的样本池，不要求输出计数/生成窗完整               |

本轮协调者实际复跑：

- `pnpm exec vitest run tests/tps`：4 文件、36 项通过。
- `pnpm check`：exit 0，格式/lint/i18n/错误码/build 均通过，仍有既有大 chunk 提示。
- Rust TPS lib 25 项、七套 TPS 集成 128 项，共 153 项通过。
- `cargo test --manifest-path src-tauri/Cargo.toml --lib database::migration::tests::migrate_adds_cadence_ms_to_existing_tps_point`：1 项通过。
- `cargo check --manifest-path src-tauri/Cargo.toml --target x86_64-pc-windows-gnu --lib`：exit 0。
- `git diff --check 095a11f4..HEAD`：通过。

未重跑全仓 Rust/vitest 与 clippy，不把实施者报告中的结果写成本轮亲跑结果。Windows 编译通过不代表 Windows 真机运行或 ACL 已验收。

## 2. 剩余四项

### R3 [P2] DSH 仍可绕过读取上限，解码量没有预算

位置：`src-tauri/src/services/tps/sources/dsh.rs:319`—`:324`、`:778`。

普通尾读 seek 和生产 IO 不持全局数据库锁已经修好；但超大行/帧分支调用 `read_tail(..., usize::MAX)`，再次取消读取上限；`decode_all` 也没有限制解码后字节数。

具体反例：游标后追加一条尚无换行的 24 MiB plain 行。先读 8 MiB 不能形成完整单元，随后读取整个 24 MiB 余量；offset 不推进，bad 标记清空，下次即使文件完全不变仍重复整余量读取。注释的“回退一次”没有对应状态。另一个合法输入是压缩尺寸小于 8 MiB、展开 128 MiB 的单个 zstd 帧：观察条数预算生效之前已经完整解码、复制文本并创建行向量。

要求：字节与解码量都必须有硬预算；超过预算使用有界续读/可恢复状态或明确降级，不能取消上限，也不能反复读取未变化的大尾部。补大行未结束且文件未变化、压缩小但解码大的反例，验证实际读取/解码工作量，而非只测输出条数。

### R7 [P2] 近期尾读取仍会被保护行后的历史挡住，保护探针也可能漏行

位置：`src-tauri/src/services/tps/sources/zcode.rs:300`—`:343`，以及 `load_rows`。

反例一：rowid 1 是长调用，刚在 5 分钟前完成；rowid 2—501 是在它执行期间插入、1 小时前完成的历史调用；rowid 502 在 30 秒前完成。10 分钟保护将起点退至 0，随后升序 LIMIT 200 只读到 rowid 200。后台下一轮 600 秒后，rowid 502 再次错过 120 秒采样窗。隔离内存 SQLite 已复现选择结果；不依赖大量僵尸行。

反例二：前 64 行是僵尸 running，第 65 行为新鲜 running，后面再有 500 行旧终态。保护查询 LIMIT 64 只返回僵尸行，跳跃游标越过第 65 行，其原地 UPDATE 收尾后无法再读到。这一边界此前报告为 Minor，但实际违反“新鲜运行行不得被跨过”的水位保证。

要求：近期完成事实的读取与连续水位/未终态保护分开处理，历史保护行不能阻塞本轮近期样本；水位不可越过被截断探针漏掉的新鲜运行行。保持单批有界、幂等和成功提交后推进游标。两个反例都需回归，不仅把 LIMIT 64 调大。

### R10 [P2] 卸载入口已恢复，但成功卸载后可能无法重新安装

位置：`src/components/usage/tps/TpsSourceStates.tsx:123`—`:125`。

安装按钮仍要求 status=unconfigured。卸载清除安装回执，却保留最近 24 小时事实与来源状态，因此正常可出现 `active + installed=false`、`no-sample + installed=false` 或 `read-failed + installed=false`。这时卸载按钮消失，安装按钮也不出现，用户不能点击重新启用。

要求：配置动作根据持久安装事实决定，不再由历史样本是否存在限制。补已有历史、成功卸载、重新安装完整 UI 流程，分别覆盖 active/no-sample/read-failed，且不删除历史数据来绕过问题。

### R11 [P2] 手动点仍会掩盖缺失的计划点

位置：`src/lib/usage/tpsSeries.ts:31`—`:34`、`:69`—`:73`；`scheduler.rs` 的 manual_refresh 保持计划锚点不变。

同 run、cadence 都为 60000 的输入：`scheduled@0 → manual@50000 → scheduled@120000`。60000 的计划点缺失，但当前函数仅比较相邻 50/70 秒间隔，两者都小于 90 秒，实际返回单段 `[[0,50000,120000]]`。已在内存转译执行现有 TypeScript 函数确认。手动刷新并不重置调度器的 last_point_ms，不能代替缺失的计划点。

要求：计划连续性单独跟踪，manual 点不能重置或遮蔽计划锚点；保留合法手动插点的显示。补上述反例和后台对应场景，同时保留现有模式切换、显式 gap、runId、旧行 NULL 和迁移测试。单纯继续调整相邻间距阈值不足以修复。

## 3. 本轮范围外的原有问题及未验事项

- 安装评审另复现了 base 已存在的部分卸载冲突问题（`install.rs:1196`）：首次卸载恢复部分键后保留完整 receipt；解决剩余冲突再次卸载时，已恢复的键反被当成用户新修改，安装记录无法了结。它是原有 P2，非这次重装修复引入，不据此撤销 R2 的关闭结论；应单独跟踪处理。隔离复现仅写专用 home，文件在 `/Users/jarvis/Documents/tps-review-install-afc3k5a2/repro.rs`。
- Windows 真机运行/ACL、OpenCode 2.1.x、多步 step.ended usage 语义、本轮未授权的正常模型请求，仍未验证。不得据本报告宣称这些已经通过，也不把它们重新扩成全面探测前置条件。
- 行源单行超过读取上限的恢复策略属于已登记边界；R3 在此仅要求不能通过取消预算制造资源放大。

## 4. 下一轮交付

继续当前分支，按 R3/R7/R10/R11 四项修复并补对应反例，返回新 HEAD、每项提交和实际测试结果。同步实施报告的关闭状态，不能继续写“11/11 全部 ADDRESSED”。不需要新增全面探测、不发送测速请求、不修改用户模型 API/认证/provider/网关、不主动重启 Harness，不写真实账本。

本报告保留首次复核报告不变；最终合并判定需等剩余问题复审通过。
