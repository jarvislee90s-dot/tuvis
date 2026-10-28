# issue #35 修复手工检测清单

> 分支：`fix/issue-35-session-monitor-p2`（基于 main 92d9f03）
> 启动方式：`pnpm tauri:dev`；WorkBuddy 已登录并跑过至少一个任务会话
> 涉及路径（Windows，`~` = `C:\Users\bunny`）：
> - MAM 数据库：`~\.mam\mam.db`
> - WorkBuddy 心跳：`~\.workbuddy\sessions\<PID>.json`
> - WorkBuddy 会话历史：`~\.workbuddy\projects\<编码目录>\<sessionId>.jsonl`
> - WorkBuddy 数据库：`~\.workbuddy\workbuddy.db`
>
> 对照修复项：#1 已读复活洞 / #2 重启丢补偿 / #3 误清未读池 / #4 重复扫描 /
> #5 孤儿 sidecar / #6 长会话标题 / #7 兜底扫描 / nit1 pid 交叉校验 / nit2 连接复用

---

## 0. 准备

- [ ] `pnpm tauri:dev` 启动 MAM，控制台可见每轮日志（`WorkBuddy: N processes, M sessions`）
- [ ] WorkBuddy 里跑一个小任务，看板出现 WorkBuddy 卡
- [ ] 装好查询工具：DB Browser for SQLite（或 sqlite3 CLI）
- [ ] （可选）Sysinternals `pssuspend`/`psresume`：模拟「sidecar 挂起」，比睡眠电脑更可控
- [ ] 辅助 PowerShell 速查：

```powershell
# 列出心跳文件（文件名 = 会话 sidecar 的 PID）
dir $env:USERPROFILE\.workbuddy\sessions\*.json
# 看心跳内容（sessionId / cwd / lastHeartbeat）
type $env:USERPROFILE\.workbuddy\sessions\<PID>.json
# 数某会话 JSONL 行数（判断是否 >500 行）
(Get-Content "$env:USERPROFILE\.workbuddy\projects\<目录>\<sessionId>.jsonl").Count
# 模拟 prewarm 回池删心跳（⚠️ 只在 MAM 退出后做，见 TC-3）
Remove-Item $env:USERPROFILE\.workbuddy\sessions\<PID>.json
```

DB 速查（MAM 运行中只读查询一般安全；若遇锁再短暂关闭 MAM）：

```sql
-- 未读池
SELECT tool_id, session_id, project_name, turned_green_at, expires_at FROM unread_sessions;
-- 已读墓碑（#1 新增表）
SELECT * FROM unread_read_tombstones;
-- 心跳观测影子表（#2 新增表）
SELECT * FROM heartbeat_observations;
-- 某会话上一轮状态
SELECT * FROM session_status_cache WHERE session_id = '<sid>';
-- WorkBuddy 侧会话标题（TC-6 用）
SELECT id, custom_title, title FROM sessions WHERE deleted_at IS NULL;
```

---

## 1. 核心用例（对应修复项）

### TC-1 已读后「心跳间隙」不复活未读（#1 短间隙）

- [ ] 1.1 WorkBuddy 跑完任务 → 绿卡/未读卡出现；`unread_sessions` 有该行
- [ ] 1.2 点击跳转（或 X 关闭）→ 卡消失；`unread_sessions` 该行删除；`unread_read_tombstones` 出现该会话行
- [ ] 1.3 模拟心跳间隙 ≥90s：电脑睡眠 >90s 后唤醒；**或** `pssuspend <PID>` 挂起该会话 sidecar >90s 再 `psresume <PID>`（PID = 心跳文件名；期间看板该卡消失）
- [ ] 1.4 恢复后等 1–2 个轮询周期（~30s/轮）
- [ ] ✅ 期望：会话卡重现但**无未读徽标**、`unread_sessions` 无该行、**完成通知不重播**
- ❌ 修复前失败特征：绿会话复活为未读卡 + firstSeenUnread 2 分钟内重播完成通知

### TC-2 已读后「跨 MAM 重启」不复活（#1 长间隙，墓碑生效）

- [ ] 2.1 重复 TC-1 的 1.1–1.2（已读、墓碑在场）
- [ ] 2.2 退出 MAM
- [ ] 2.3 睡眠 / 挂起 sidecar ≥90s 后恢复
- [ ] 2.4 重启 MAM，等 1–2 轮
- [ ] ✅ 期望：同 TC-1.4，不复活、不通知（墓碑 7 天 + 落库的状态缓存 24h 双保险）

### TC-3 停机期间完成 → 重启后补偿触发（#2 主场景）

- [ ] 3.1 WorkBuddy 会话**运行中**（黄卡），MAM 在跑；确认 `heartbeat_observations` 有该 pid 行
- [ ] 3.2 退出 MAM
- [ ] 3.3 等 WorkBuddy 任务完成，**并**模拟 prewarm 回池：删除 `~\.workbuddy\sessions\<PID>.json`
- [ ] 3.4 重启 MAM，等 1–2 轮
- [ ] ✅ 期望：WorkBuddy 未读卡出现（标题/末条消息正确）+ 完成通知；`unread_sessions` 新增该行；`heartbeat_observations` 中该 pid 被移除
- ❌ 修复前失败特征：卡静默丢失（补偿机制完全失效）
- 注意：该用例要求此前未已读（无墓碑）、停机前会话是黄卡；若删除心跳后会话尾部仍是运行态则不补（符合 spec）

### TC-4 强杀主进程后未读卡清理（#5，Windows 关键）

- [ ] 4.1 WorkBuddy 产生未读卡（完成后先不点击）
- [ ] 4.2 任务管理器 → 详细信息 → 添加「命令行」列 → 找到 WorkBuddy.exe 各进程：**只结束持窗口的主进程**；cmdline 含 `cli\bin\codebuddy` 的 sidecar 保留
- [ ] 4.3 确认 sidecar 仍活：其心跳文件 `lastHeartbeat` 持续刷新
- [ ] 4.4 等 1–2 轮
- [ ] ✅ 期望：该工具全部未读卡消失（宿主死 → 清池）；`unread_sessions` 中该工具行清空
- ❌ 修复前失败特征：sidecar 活着 → 误判宿主存活 → 未读卡永不清理
- [ ] 4.5 重启 WorkBuddy，功能恢复正常
- 注：若主进程被杀时 sidecar 也被带走了，用例退化为普通「宿主退出清池」，仍应通过（但没验证到 #5 的 cmdline 排除路径，可换 Process Explorer 精确杀主进程重试）

### TC-5 已读会话的新回合 → 新未读正常出现（#1 最关键回归）

- [ ] 5.1 找一个已读过的会话（墓碑在场），在 WorkBuddy 里**发起新任务**
- [ ] 5.2 卡变黄（运行中）
- [ ] 5.3 任务完成
- [ ] ✅ 期望：新未读卡正常出现 + 完成通知；`unread_sessions` 有新行（`turned_green_at` 为新完成时刻）
- ❌ 若被墓碑挡住不出现 = 过度防御，属 bug（墓碑只该在 prev=None 时生效）

### TC-6 长会话标题显示首条 user 消息（#6）

- [ ] 6.1 找一个 JSONL >500 行、且 `workbuddy.db` 中 `custom_title` 与 `title` 均为空的会话
      （用 0 节 SQL + PowerShell 数行数；没有就跑一段长对话再找；确认标题为空时卡片才会走降级链）
- [ ] 6.2 让该会话出卡（保持心跳新鲜 / 刚完成）
- [ ] ✅ 期望：卡标题 = 首条 user 消息（超 60 字符截断），**而不是 sessionId**
- ❌ 修复前失败特征：长会话标题恒为空 → 回退显示 sessionId

---

## 2. 回归用例（确认没修坏旧功能）

- [ ] R1 跳转已读：点未读卡跳转 → 仅该卡消失，同工具其他未读卡保留
- [ ] R2 X 关闭未读卡：点 X → 卡消失、行删除、墓碑写入
- [ ] R3 变黄删行：未读卡对应的会话重新运行 → 未读卡被活跃卡替代，无双卡
- [ ] R4 24h 过期：把 `unread_sessions` 某行 `expires_at` 手动改为过去时间 → 1–2 轮后物理消失
- [ ] R5 W5 停用：设置里取消勾选 WorkBuddy → 卡全消失、通知静默；重新勾选恢复
- [ ] R6 Codex APP 绿卡（若用 Codex APP）：已读后下一轮被剔除，且**不再闪回**（#1 的缓存 TTL 化对 P1-3 的顺带修复）
- [ ] R7 其他工具（Claude/Codex CLI 等）会话卡不受影响

---

## 3. 可选观察项（单测已覆盖，手工难触发）

- [ ] O1（#3）挂机 30–60 分钟（有未读卡状态）：未读卡不无故消失
- [ ] O2（#4）任务管理器观察 MAM 进程 CPU：未读池非空 + 双工具时，每 30s 轮询无异常 CPU 尖刺（修复后少 2 次全量进程扫描/轮）
- [ ] O3（#7）若存在 mangle 未命中环境（如 UNC cwd）：Process Monitor 看 `projects` 目录枚举频率，兜底命中后应只在首轮扫描、后续走缓存
- [ ] O4（nit1）pid 复用竞态：手工不可复现，由单测 `heartbeat_pid_mismatch_is_skipped` 覆盖
- [ ] O5（nit2）SQLite 连接复用：Process Monitor 看 `workbuddy.db` 打开频率，多会话单轮内应只开一次

---

## 4. 对照「修复前行为」（可选）

想直观对比，可切回 main（92d9f03）跑 TC-1 / TC-3 / TC-4 / TC-6，应复现各「失败特征」；
再切回本分支全部转绿，即为修复生效的直接证据。
