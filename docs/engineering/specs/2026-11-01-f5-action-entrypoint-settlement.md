# F5 宠物、跳转与手机归档的变更结算接线

## 背景

桌面卡片 X 已使用统一乐观变更票根、pending 过滤和 `settleSessionMutation`。其他入口尚未接入同一保护：宠物 X 只 fire-and-forget 命令并清本地灯态；跳转成功的后端会写已读并排 Action，但前端没有显式 read pending；手机 APP 软归档在 HTTP 成功后才删本地卡片，期间的 REST/SSE 旧响应可能把它写回，后端 hide/unhide 写入后也没有通知 Scan Bus 重算。

## 目标

1. 宠物 X 复用桌面卡片的变更分类、票根、pending 和结算；宠物状态计算前应用 pending，旧快照不得重建卡片。
2. 直接跳转和歧义窗口选择成功后，使用明确的“标已读”意图立即乐观清未读，并用写入完成后的 epoch 等待器收口；不得把 read 错分为 dismiss。
3. 手机 APP 软归档在确认后立即隐藏；归档开始前启动的 REST 与旧 SSE 连接回调被丢弃；成功后拉取权威列表并重连 SSE；失败时回滚并恢复数据流。
4. 后端 hide/unhide 写入完成后都请求一次 Action 重算。`session-hide` 和 `session-unhide` 的现有路径、请求体、状态码及 JSON 字段保持兼容。

## 方案

### 设计取舍

- 宠物继续只改本地 `light` 的做法无法防旧快照重建卡，故复用同一票根和共享缓存 pending；不另造一套宠物状态缓存。
- 跳转可在前端成功后再重复调用 `mark_session_read`，但直达路径的 focus 命令已经写过已读。优先让现有 focus 回执附带写入后的 gate epoch，避免同一次直达重复写库/补扫；仅旧后端缺少该字段时才回退到 `mark_session_read`。歧义选择仍通过 `mark_session_read`，因为 `focus_hwnd` 本身不写已读。
- 手机仅在旧 REST 返回后再补删卡无法防止 SSE 旧连接帧复活卡片；把每个回调包进连接代次，归档时停止旧流，权威回取后重开，并对 REST 请求使用独立代次。HTTP 形状保持现有契约。

### 宠物 X

- 依据宠物卡对应的 `Session` 走 `applyOptimisticSessionMutation()`，命令分别调用 `mark_session_read` 或 `dismiss_session_card`，最终走 `settleSessionMutation()`。
- 在宠物的会话派生/状态机计算入口先执行 `applyPendingSessions(data.sessions)`。这既屏蔽旧轮询/事件快照，也把 read pending 的 `unread=false` 传给 `computePetStatus()`。
- X 点击时同步从宠物卡片状态移除目标；后端失败或超时回滚仍由票根和下一份权威快照决定。

### 跳转已读

- 新增显式 `applyOptimisticSessionRead(queryClient, target)`：只设置 read pending、只把对应缓存会话 `unread` 置 false，不调用 dismiss 分类、不改变卡片总数。
- 直达跳转由现有 `focus_session` 完成已读写入。后端在写入后捕获 `scan_epoch`，排 Action，并在成功结果 JSON 中附加可选 `readGateEpoch`；既有 `type`、`via`、`windows` 字段不变。
- 新前端在直达成功后开始 read pending，并以 `readGateEpoch` 创建 applied=false epoch waiter。旧后端缺少该可选字段且 focus 路径有会话缓存时，调用现有 `mark_session_read` 获取 `MutationOutcome` 作为兼容回退。
- macOS `via=deep-link` 缺少 gate 时保留既有“路由结果不可证，不标已读”裁决，不走旧后端回退。直达后端本身已写入；没有本地会话缓存的浮窗不创建不可见 waiter。
- 歧义候选只在 `focus_hwnd` 成功后调用 `mark_session_read` 持久写入；即使通知浮窗没有会话缓存也必须写，缓存存在时再创建 read pending。选择失败不标已读。所有可见缓存路径使用同一 `settleSessionMutation()`。
- 对 `unread=false` 的目标不创建 pending，避免无意义缓存写入。

新增 `readGateEpoch` 是向旧 focus 回执增加可忽略字段；旧客户端继续按原字段解析，新客户端连接旧后端时走 `mark_session_read` 回退。

### 手机 APP 软归档

- 以 `(agentType, sessionId)` 作为 pending key，保存票根 token 与“服务端请求已成功”标记；确认归档时先登记 pending 并立即过滤卡片，随后调用原 `hideSession()`。
- 所有 REST 请求在开始时捕获 Board 数据代次；归档开始和结束都会推进代次，较旧请求完成时不得提交。transition 触发的补拉也走同一代次闸。
- 归档开始时停止当前 SSE 连接并提升连接代次；旧连接的迟到回调直接丢弃。请求完成后重建连接，首帧继续使用既有带 `version` 的快照。
- `handleSnapshot`、transition patch 和 REST tick 都在提交前过滤 pending key。服务端成功后主动重取 `/sessions`；目标确实缺席时清 pending 并写入该份权威数据。若重取失败/目标仍在，则保留 pending，靠新 SSE 全量帧或后续 REST tick 对账。
- HTTP 失败立即清除该 token 的 pending、恢复点击前数据并重连数据流。若另一个 token 已接管同一 key，旧请求不得清新 pending。
- CLI 硬杀继续走 `closeSession()` 原路径；本设计聚焦 APP 软归档。
- `/session-hide` 在已读与隐藏写入完成后排一条 `ScanReason::Action`；`/session-unhide` 在实际删除隐藏记录后排一条 Action。API JSON 不新增字段、不改状态码。

不改 mobile REST 的 `SessionsResponse` 形状；使用请求代次、旧 SSE 连接代次和服务端已有的隐藏过滤来阻止归档卡片复活。

## 验收

- 宠物 X：点击后立即消卡；pending 期间注入旧扫描/快照，状态机计算后仍不重建；applied=false 时等 epoch 收口；命令失败且未收到 `session-read` 时恢复操作前的宠物状态，后续权威读取仍可重算。
- 跳转：直达已读成功立即 `unread=false`；pending 保留到 `epoch > readGateEpoch`；macOS 未验证 deep-link 不标已读；歧义分支即使无 Query 缓存也在窗口聚焦成功后持久标已读，失败不写；read 不走 dismiss。
- 手机：确认后卡片在同一交互周期内消失；归档前启动的 REST 返回及旧 EventSource 回调被丢弃；成功后新 SSE 首帧对账；HTTP 失败恢复卡片；随后 fresh REST/SSE 反映服务端隐藏状态。
- hide/unhide 写后各只排对应的 Action；hide/unhide 路由、状态码和原 JSON 字段保持可用。

## 修改边界

- 桌面/宠物共享状态：`src/lib/query/optimisticSessions.ts`、`src/hooks/useSessionJump.ts`、`src/components/pet/FoxbellPet.tsx`。
- focus 回执：`src-tauri/src/commands/session.rs`。
- 手机端：`src/mobile/Board.tsx` 的 REST/SSE 提交代次与连接重启（不改 REST action wire schema）。
- 远控写入：`src-tauri/src/remote/api.rs`。
- 回归：`tests/scanBus/optimisticSessions.test.ts`、`tests/session/`、`tests/pet/`、`tests/mobile/Board.test.tsx`、`src-tauri/src/commands/session.rs` 与 `src-tauri/src/remote/server.rs`。

不改 `docs/MASTER-PLAN.md`、F6 事件覆盖策略、CLI 硬杀 API、普通会话 GET/REST 数据结构、宠物跳转失败的既有提醒语义或移动归档历史页布局。
