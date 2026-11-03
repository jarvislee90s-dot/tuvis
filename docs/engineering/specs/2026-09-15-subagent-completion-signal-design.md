# 子代理完成信号修复——设计规格（spec 伴随物）

> 日期：2026-09-15 定稿｜2026-10-18 随阶段 A 开工落盘（Task 1 产出）
> 计划主文档：`docs/engineering/plans/2026-09-15-subagent-completion-signal-fix.md`（含
> 2026-10-18 适配追记）。本档收录其「背景与缺陷定义 + 三个必须澄清的技术事实 +
> 共享层接口定案」，供实现与评审对照；证据出处见计划主文档及其引用的审计报告。

## 一、缺陷定义（实机确认清单）

两条正交缺陷线：

- **P1 假绿**：主会话派发后台子代理后结束自己的回合（ZCode `step-finish(stop)` /
  Claude assistant 尾部 / dsh `turn/end completed`）→ MAM 判绿、播提示音、发通知、
  插未读卡，而子代理仍在跑。实测 ZCode 392 次转绿中 **256 次（65%）** 属此类；
  dsh `session-946b624e` 绿灯持续 **777 秒**（3 个后台子代理在跑）。
- **P2 脏预览/假黄**：工具注入的合成条目（子代理通知 / 系统提醒 / 上下文注入）
  被当成用户消息——假黄 + `<task-notification>` 原文上卡进通知。ZCode 合成 user
  消息 1051 条，现有 kind 白名单只跳过两种（`todo_reminder`/`timeline_event`），
  其余全漏。

| 编号 | 工具 | 缺陷 | 实机数据 |
|---|---|---|---|
| Z-P1 | ZCode | 派发后台子代理后回合收尾判绿 | 256/392（65%）；`sess_5a57eda4` 连续 3 次假绿 |
| Z-P2 | ZCode | 合成通知译成 UserMessage + 原文上卡 | 1051 条合成 user（白名单只盖 2 种 kind） |
| C-P1 | Claude | assistant 尾部判绿而子代理仍在写 | 3 父会话 24 窗口，最长 15.7 分钟 |
| C-P2 | Claude | isMeta / task-notification / stdout 当用户消息 | 59 会话中 29 个尾部即合成（永不转绿） |
| X-P1 | Codex CLI | `<turn_aborted>`/`<environment_context>` 假黄 + developer 进预览 | 37 假黄 / 45 脏预览 |
| X-P1' | Codex CLI | 子代理 rollout 当会话卡候选（无 source 过滤） | 67 个子 rollout |
| K-P2 | Kimi | origin.kind=injection 当用户消息 | 195 时刻假黄 |
| D-P1 | dsh | turn/end completed 判绿而子代理在跑 | 绿灯 777 秒；`parent_session` 解析了但从未使用 |
| W-P2 | WorkBuddy | `<system-reminder>` 当用户消息且无截断 | 最大 14322 字符 |
| O-P2 | OpenCode | `## New Messages` 合成消息当用户消息 | 5 条（91ms 覆盖窗短） |
| Dsh-P2 | dsh | **无缺陷**——`source.kind=="user"` 白名单是唯一正确实现，**作为 P2 修复范式** | — |

## 二、三个必须澄清的技术事实（决定方案结构）

1. **只跳过通知消息不足以修复假绿**：`sess_5a57eda4` 21:04:58 的绿完全不来自
   通知消息，而是父会话自己的 `step-finish(reason=stop)`——跳过通知后回放仍判绿。
   **P1 与 P2 必须分开修。**
2. **ZCode 自己写了权威的子代理状态文件**：
   `~/.zcode/cli/agents/<parent_sess>/agent_<uuid>/metadata.json` 含
   `status: running|completed|failed` + `completedAt`（383 个文件中 running 恰 1 个）。
   比"子会话是否在写库"更权威，但代价是文件 IO——与 DB 侧信号权衡。
3. **ZCode 后台派发有结构化标记且派发↔结算近全配对**：派发侧
   `part.data.state.input.run_in_background == true`（111/111 可提取 workId）；
   结算侧 `background_notification.metadata.originMeta.workId`。闭环率 99%
   （110/111，唯一未结算是 28882 分钟前僵尸派发）——可直接用于"后台工作在跑吗"。

## 三、共享层接口定案（Task 1 产出，`monitor/app_status.rs`）

```rust
/// 第四种后代态（追加于 DescendantActivity 末尾）
Pending,   // 存在已派发但未结算的后台工作；判定优先级 Pending > Active > Stale > Absent

/// P1 仲裁函数：Idle/Finished + Pending ⇒ Waiting；其余组合透传
/// （Active/Stale/Absent 与全部非绿态零变化——既有工具回归锁钉住）
pub fn overlay_completion_with_pending(status, descendants) -> SessionStatus

/// P2 注入识别口径（语义标记优先于 kind 枚举——枚举随版本改名必失效）
pub enum EntryOrigin { Human, Synthetic, Unknown }
pub fn entry_origin_from_flags(is_synthetic: bool, ui_hidden: bool) -> EntryOrigin
```

**Pending 在停更仲裁（overlay_stale_with_descendants）里的映射**：与 Active 同
（账未结清时主会话静默 = 健康等待，不降级 Processing）；绿态拦截由
`overlay_completion_with_pending` 单独负责——两级仲裁组合序：先停更、后完成。

**两标记实测零误伤**：ZCode 1051 条合成全部命中、629 条 user_prompt 全部不命中。

## 四、阶段划分与实施纪律

阶段 A（Task 1-4 共享层+ZCode）→ B（5-7 Claude+Codex）→ C（8-9 dsh+Kimi）→
D（10-11 WorkBuddy+OpenCode+回归锁），每阶段独立 PR。Task 2 实机探测按
2026-10-18 裁定**推迟**，各 Task 判据沿用计划内既有实测证据；实现中发现判据
失配 → 停下报告，不猜不抄旧档。适配追记（测试夹具签名 / Task 10 双 schema /
content.rs 同步点映射 / teammate 已落地）见计划主文档末尾。
