# ZCode 复审第二轮修复计划（2026-10-27）

审查基线 `90ae20be..81a16b26`；本计划处理复审提出的 3 项 P2，以及复审后升级为 P2 的“恢复帧能力矩阵”问题，不连接真实 ZCode。
每项要求：先复现 RED → 最小修复 → GREEN → 独立审查 → 提交。完整账本在
`.superpowers/sdd/2026-10-27-zcode-review-fixes-round2/`（gitignore）。

| # | 严重度 | 状态 | 根因 | 修复 | 回归证据 | 提交 |
|---|---|---|---|---|---|---|
| P2-1 | P2 | **已审已提交 `0d2a4cca`** | RelayLag 只降级最后一个 `last_session`；手机收到 `ready=false` 只落地不 GET；旧桥帧被忽略后无自愈 | Hub 有界 LRU `tracked_sessions` 逐会话降级/广播；离开 paired 也逐会话通知；手机 `staleRefreshRef` 同代一次 refresh，`ready=true`（含 GET 落地）重置；GET 在途掉线不登记 | 两会话 lag→两条 ready=false→GET 恢复；LRU 淘汰最久未登记；第二次 lag 零事件；同代重复 false 不刷、GET 恢复后可再失效刷新；掉线逐会话通知/GET 在途掉线零登记 | `0d2a4cca` |
| P2-2 | P2 | **已审已提交 `269f3394`** | CRC/解码/序号错误使投影未就绪，但 `on_data` 在 `Err` 时丢弃差异视图，Hub/手机收不到降级 | `DataOutcome{result, changed}` 分离传播；`on_data_outcome` 错误也做 diff；Hub 泵先广播 changed 再处理 Err；未就绪视图能力回基线全关；`session()` GET 对 ready=false 同样清零能力 | 坏帧 Err + changed ready=false → Session 事件；有效快照恢复；连续错误泵不退；GET 降级能力全关 | `269f3394` |
| P2-4 | P2（复审升级） | **已审已提交 `b70a459c`** | 坏帧降级后自愈 GET 可能返回 `not_ready`；有效快照恢复 `ready=true` 时 Hub 事件沿用 `last_session` 的全关能力，手机清守卫后不再 GET，发送按钮持续灰 | `applyRead` 对 GET 落地 `ready=false` 也布防 stale；失效后首次 `ready=true` 自动补一次权威 GET；每代次恢复尝试封顶 2 次；GET 落地 ready=true 才清 stale/recovery 与计数；snapshot/切会话/换代同规 | 初始 GET ready=false 无降级帧→ready=true 全关帧→自动 GET→send enabled、零 POST；GET 1→2→3 无手动 refresh；重复 true 无 GET4；两次失败封顶；mobile 1043/0、hook 18/0 | `b70a459c` |
| P2-3 | P2 | **已审已提交 `a7956072`** | `finish_disconnect` 无 epoch 归属检查，重叠清理可让后一个先完成、较早任务随后 `backend.disconnect()` 关掉新连接 | `disconnect_lock` 串行清理；进入与等 `connect_lock` 后两次 `CloseGuard::is_current`；陈旧 owner 零 cleanup；文档化“关闭活性以 backend.disconnect 终止为前提”，不加撤回清理的超时 | 交错测试：A 卡第一次 disconnect→B 接管→放行后 calls=3→重连 paired→旧 A 不再 disconnect；真实 stale guard（A 未 poll、B 完成、重连后 finish A）零额外 disconnect | `a7956072` |

## 最终门禁（2026-10-27）

- Rust 单测：普通 `3312 passed / 0 failed / 38 ignored`；证据构建
  `3331 passed / 0 failed / 38 ignored`。
- Rust 集成：普通与证据构建全部套件 0 failed（含 `zcode_control_e2e` 8/8、
  `zcode_control_remote` 25/25、`wire_e2e` 9/9、全链 4/4）。
- `cargo fmt --check` 0；普通/证据构建 `clippy --all-targets ... -D warnings` 0。
- 前端：`pnpm test` 194 files / **2355 passed** / 5 skipped；`pnpm check` 0；
  `pnpm build:mobile` 0。
- 定向复审：hub 47/0、hook 18/0、mobile 1043/0；P2-4 复审 0 Critical/Important；
  三项合并结论 **“可合并待实机”**。

## 已知 P3（实机验收清单，不阻合并）

1. **LRU cap 32**：极端情况下同一代际 GET/Data 登记 >32 个会话，某台手机的会话可能被
   淘汰而漏收该次逐会话降级；SSE 重连 snapshot 会重新登记，连接事件恒广播，属有界
   tradeoff。实机需做“多手机 + 32 会话”压测。
2. ~~恢复帧能力矩阵~~ **已在 P2-4 修复**：失效后首次 `ready=true` 自动补权威 GET，
   每代次最多 2 次；实机仍需验“坏帧后恢复帧→控制面自动复原、无需手动刷新”。
3. **lag 事件量**：单次 lag 最多 32 条 Session 落在 broadcast(64)，慢客户端由 `resync`
   兜底；无风暴，实机观察 SSE 重连行为。

## 验收边界

- 本轮只做代码层与自动测试；真实登录/手机/Windows 验收后置。
- 不修改 `docs/MASTER-PLAN.md`；不 push/merge/tag。
- `platform_validated=false`、`spec_complete=false` 保持不变。
