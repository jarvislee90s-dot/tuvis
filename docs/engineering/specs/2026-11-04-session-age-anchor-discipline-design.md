# 设计文档：会话状态判定统一规划 —— 锚点纪律与机制收敛

- 日期：2026-11-04
- 状态：设计定稿（P0/P1/P2 全做；P3 只做 claude Pending 拦绿；待出实现计划）
- 关联问题：用户实测——WorkBuddy 任务已结束，卡片仍以 30 秒周期绿→黄→绿反复切换，并重复播放完成提醒
- 前置文档：`docs/engineering/specs/2026-09-16-false-green-flicker-design.md`（假绿治理，本篇是其**防抖轨道的锚点纪律补充**）

---

## 1. 问题与根因

### 1.1 症状

WorkBuddy 会话任务真实结束后，卡片状态在绿（Idle）与黄（Processing）之间以 **30 秒周期**反复切换，每次转绿都重新触发完成通知与提示音。

### 1.2 根因：同一个 age 值服务了两个判定语义

`workbuddy_parser.rs` 构卡层把三个活动源合成一个"有效活动时刻"，其年龄同时喂给两个语义完全不同的判定：

```rust
// workbuddy_parser.rs:905-914
let eff_activity_ms = [jsonl_mtime_ms, c.db_activity_ms, c.last_heartbeat_ms]
    .into_iter().flatten().max();
let eff_age_ms = eff_activity_ms.map_or(0, |a| now.saturating_sub(a));

// workbuddy_parser.rs:923-926 —— 误用点
overlay_mtime_stale(
    apply_green_debounce(d.status_core.clone(), d.tail_kind, eff_age_ms), // 需要「内容多久前写下」
    eff_age_ms,                                                          // 需要「会话还有没有生命迹象」
)
```

- `apply_green_debounce` 问的是**最后一条助手消息写完多久了**（`GREEN_DEBOUNCE_MS = 10_000`）；
- `overlay_mtime_stale` 问的是**这个会话还有没有生命迹象**（`APP_STATUS_STALE_MS = 300_000`）；
- 心跳每 **30 秒**刷新一次（真机实测 `10:19:46.753` → `10:20:16.754`），每次刷新都把 `eff_age` 归零 → 防抖窗永远等不满 10s → 黄 10 秒，心跳老化 → 绿 20 秒 → 下一跳再黄。

**这个设计只对 `overlay_mtime_stale` 成立，对完成冷却不成立。** 心跳只能证明"进程还开着"，不能证明"AI 还在干活"。

### 1.3 回归来源

`de3e7d01`（2026-10-30，"judge board presence by runtime, DB activity and working status"）把防抖锚点从纯 `mtime_age_ms` 换成 `eff_age_ms`。该提交的其余部分（`working` 上拉、`runtime_present` 判活、DB-only 候选）都是为"子代理运行期父会话静默"设计的，**应当保留**——它们增效的是"在场"判定，不该污染"完成"判定。

### 1.4 活体取证

会话 `659d10b8`（DB `status=completed`，jsonl 已 750 秒未写），逐秒计算后端输出：

```
hbAge   dbAge   mtimeAge  effAge  status
12596  750763   750974   12596   绿 IDLE
28620  766787   766998   28620   绿 IDLE
  621  768789   769000     621   黄 PROCESSING   ← 心跳刷新
 8630  776798   777009    8630   黄 PROCESSING
10633  778801   779012   10633   绿 IDLE         ← 心跳超 10s
...
  645  798825   799036     645   黄 PROCESSING   ← 又一个 30s 周期
```

完美 30 秒周期，与 `GREEN_DEBOUNCE_MS=10s` + 心跳 30s 的频率关系完全吻合。

### 1.5 放大链路

```
后端判 Off→绿（每 30s 一次）
  → sync_unread 边沿触发（adapter/mod.rs:3777 unread_pool_action）
      prev=Processing → 本轮 Idle = Insert 新未读行，turned_green_at_ms 刷新
  → 前端 GREEN_STABLE_MS=3s 稳定窗（绿持续 20s，远超 3s，必然通过）
  → petStatus.ts 颜色边沿 → 完成语音
```

三层都是边沿触发；后端每 30 秒制造一次假边沿，前端两道防线（3s 稳定窗、60s 同方向节流）都无法拦截——稳定窗只看"绿是否持续够久"，节流只按 `from→to` 方向记账，而"首见未读绿卡补发"路径（2 分钟新鲜）会以另一条路径再次进入播报。

---

## 2. 横向盘点：8 个工具的状态机制现状

| 工具 | 主状态信号 | 心跳 | 完成冷却 | 停更阈值 | 后代仲裁 | 共享核 |
|---|---|---|---|---|---|---|
| **claude** | JSONL 尾 + hook 事件(30s TTL) | 无 | 无（thinking-only 内容守卫） | 3s mtime → **红** | 无（仅 30s flap 打标） | 否 |
| **codex CLI** | rollout 尾 + hook | 无 | **事件序锚**（`turn_window_open`，零时间常数） | 300s → 红，随即消为绿 | 无 | 是 |
| **codex APP** | items 尾 + `turn.status` + updated_at | 无 | 无 | 300s → **保留红** | 有（30s/300s） | 是 |
| **dsh** | 事件流 + `session.lock` fd 租约 | 持有式租约 | 无 | **30 分钟** | 无 | 否 |
| **kimi** | wire.jsonl 尾 | 无 | 无 | 60s → **红** | 无 | 否 |
| **openclaw** | **仅 CPU > 5%** | 无 | 无 | 无 | 无 | 否 |
| **opencode** | DB tail + CPU>15% + 60s recency | 无 | 无（reason≠stop 守卫） | 无 | 无 | 否 |
| **zcode** | 双 SQLite sequence 倒扫 | 无 | 无（step-finish 真边界） | 300s → 红 | **全套**（含 Pending） | 是 |
| **workbuddy** | DB `working` + 心跳 + jsonl 尾 | **有，30s 刷新** | **10s 时间锚**（锚=eff_age，缺陷） | 300s | 无（DB working 替代） | 部分 |

### 2.1 六个"各自为政"的事实

1. **冷却有三套实现，锚点各不相同**：codex CLI 事件序锚（最健壮）、workbuddy 时间锚（受污染）、claude hook 层 Stop grace（5s/30s）。同源于 2026-09-16 假绿治理 spec 的 §4.1/§4.2/§4.3，却是三份独立代码，共享件只有 `tail_semantic_kind` / `turn_window_open` 两个纯函数。
2. **"兜底红"哲学分裂成两派**：保留红 = claude(3s) / kimi(60s) / codex APP(300s) / dsh(30min)；不落红 = codex CLI / zcode / opencode。**codex 自己的 CLI 与 APP 两条路线互相对立。**
3. **停更阈值散布 5 个量级**：3s / 60s / 90s / 300s / 30min，只有 300s 进了共享核。
4. **心跳只有 WorkBuddy 一家有**，且 5.9.3 后已退化为"只写一次的注册文件"；`HEARTBEAT_FRESH_MS=90s` 早已不用于在场判定，但仍参与 `eff_age` 计算。
5. **后代仲裁只有 codex APP 与 zcode 用**；`overlay_completion_with_pending`（Pending 拦绿）全仓仅 zcode 一家调用。
6. **同类绿黄横跳在别家也发生过**：opencode 的 `cpu > 15% && last_role != assistant` 规则就是专为"任务结束后后台 GC/索引导致绿黄横跳"加的补丁（`opencode_parser.rs:1031` 注释）；claude thinking-only 行 5 分钟闪绿 6 次（`status.rs:134-136`）；codex CLI 中间消息与下轮 function_call 间隔 0~3s（靠回合守卫修复）。

---

## 3. 核心不变量（本设计的宪法）

> **一个 age 值只能服务一个判定语义。**
>
> 若某工具需要「内容新鲜度」与「会话在场度」两个量，必须分别计算，不得复用。
>
> - `content_age`（内容写入年龄）→ 只喂**完成冷却** / 内容新鲜度判定
> - `presence_age`（会话在场年龄）→ 只喂**在板判定** / **停更降级** / 未读补偿

违反示例（本次 bug）：`eff_age = max(mtime, db_activity, heartbeat)` 同时喂给 `apply_green_debounce` 与 `overlay_mtime_stale`。

### 3.1 强制手段（用户裁决：具名结构体）

定义 `SessionAges`，**全仓只有一个构造点**，字段名自证语义；误用需显式写错字段名，code review 一眼可见。

```rust
/// 会话年龄双轴（唯一构造点：`SessionAges::derive`）。
///
/// **纪律（2026-11-04 用户裁决）**：一个 age 值只服务一个判定语义。
/// - `content_age_ms` 只喂完成冷却与内容新鲜度判定；
/// - `presence_age_ms` 只喂在板判定、停更降级与未读补偿。
///
/// 违反后果：心跳/DB 活动会把内容年龄打回 0，完成冷却永远等不满，
/// 造成绿↔黄周期振荡并重复播报（2026-11-04 WorkBuddy 缺陷）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionAges {
    /// 距**内容最后写入**的毫秒数：只取内容源（jsonl mtime / DB time_updated /
    /// 事件流末条时间戳）。无内容源时为 `None`（未知 ≠ 刚刚）。
    pub content_age_ms: Option<u64>,
    /// 距**会话最后生命迹象**的毫秒数：内容源 ∨ DB 活动 ∨ 心跳 ∨ 锁租约取最近。
    pub presence_age_ms: Option<u64>,
}

impl SessionAges {
    /// 唯一构造点。`content_ms` / `presence_sources` 由各 parser 的**证据层**提供。
    pub fn derive(now_ms: u64, content_ms: Option<u64>, presence_sources: &[Option<u64>]) -> Self;
}
```

**为什么不用 newtype**：保证更强（编译期禁止互通），但每处算术要 `.0` 拆包或实现 Deref，样板代码显著更大。具名结构体在"误用可见性"上已达目的，且改造面可控。

**为什么不用纯文档约束**：本次 bug 正是"有注释、有常量名字、仍然写错"的案例——`apply_green_debounce(…, eff_age_ms)` 在语法上完全合法。

---

## 4. 分层架构

### 4.1 现状（缺锚点层）

```
L4 消费层    useNotification(GREEN_STABLE_MS=3s) · petStatus 颜色边沿
     ▲
L3 仲裁层    overlay_mtime_stale · 二次强信号(DB working / turn.status)
     ▲
L2 判定核    derive_app_status / turn_window_open（纯函数）
     ▲
L1 翻译层    各工具 format → AppEntryKind（各自为政，合理）
     ▲
L0 证据层    jsonl / SQLite / hook / 心跳 / 进程 CPU
```

**缺口**：L2 与 L3 之间没有锚点层，各 parser 在 L3 现场算 age，于是出现"一个 age 喂两个判定"。

### 4.2 目标（新增 L2.5 锚点层）

```
┌──────────────────────────────────────────────────────────────────────┐
│ L4  消费层（边沿触发，本次不改）                                      │
│     useNotification: GREEN_STABLE_MS=3s · 同方向 60s 节流             │
│     petStatus.ts: 颜色边沿 → 完成语音                                 │
└───────────────────────────▲──────────────────────────────────────────┘
                            │ SessionStatus + unread
┌───────────────────────────┴──────────────────────────────────────────┐
│ L3  仲裁层（overlay，可组合，顺序固定）                               │
│   ① 完成冷却      apply_completion_cooldown   ← content_age          │
│   ② 停更降级      overlay_stale_with_descendants ← presence_age      │
│   ③ 后代仲裁      overlay_completion_with_pending                    │
│   ④ 二次强信号    DB working / turn.status / task_status=error        │
└───────────────────────────▲──────────────────────────────────────────┘
                            │ SessionAges
┌───────────────────────────┴──────────────────────────────────────────┐
│ L2.5 ★锚点层（本次新增 · 唯一构造点）                                 │
│   SessionAges { content_age_ms, presence_age_ms }                     │
│   ★ 不变量：一个 age 只服务一个判定语义                                │
└───────────────────────────▲──────────────────────────────────────────┘
                            │ 证据时间戳
┌───────────────────────────┴──────────────────────────────────────────┐
│ L2  判定核（纯函数，本次仅扩展）  derive_app_status / turn_window_open │
├──────────────────────────────────────────────────────────────────────┤
│ L1  格式翻译层（每工具一个，各自为政在此层合理）                      │
├──────────────────────────────────────────────────────────────────────┤
│ L0  证据层  jsonl / SQLite / hook 事件 / 心跳文件 / 进程 CPU           │
└──────────────────────────────────────────────────────────────────────┘
```

### 4.3 判定顺序（固定，不可调换）

```
derive_app_status(kinds)                        # L2  尾部倒扫
  → 冷却：若绿态且 (冷却信号有效) 则拉黄          # L3① 用 content_age
  → 停更降级：Processing 停更 ≥ 阈值 则降级      # L3② 用 presence_age
  → 二次强信号覆盖                                # L3④ 无条件上拉/终态
  → Pending 拦绿                                  # L3③ 最后一道，覆盖所有转绿路径
```

**为什么 Pending 必须最后**：zcode 注释已实证（`zcode_parser.rs:511-517`）——本函数转绿路径有三条（尾部推导 Idle / 无信号停更兜底 Idle / error 覆盖 Finished），Pending 仲裁放最后才能同时覆盖三者。claude 接入时同理。

**为什么冷却在停更之前**：冷却产出的 Processing 若已超过停更阈值，应继续被降级（真完成场景 10s 后即为 Idle，不会落入 300s 降级路径）。若顺序颠倒，冷却把已降级的 Waiting 拉回 Processing，会掩盖"疑似卡住"。

---

## 5. 机制关联树

```
会话状态判定
├─ 完成信号来源
│  ├─ 回合边界（精确）        codex CLI · zcode · opencode · dsh · kimi
│  ├─ 消息模型（结构免疫）    claude（tool_use 与 text 同条消息）
│  ├─ 二次强信号（APP）       codex APP(turn.status) · workbuddy(DB working)
│  └─ 无信号 → 需冷却         workbuddy（唯一，本 bug 所在）
│
├─ 冷却形态（P2 统一为 CooldownSignal）
│  ├─ 事件序锚（零时间）      codex CLI  turn_window_open
│  ├─ 内容时间锚              workbuddy  10s   ← 锚点必须 content_age
│  └─ 事件时间锚              claude     Stop grace 5s/30s
│
├─ 停更降级（P1 常量集中）
│  ├─ 300s → 红               codex APP · zcode
│  ├─ 300s → 红随即消为绿     codex CLI
│  ├─ 3s / 60s → 红           claude · kimi
│  ├─ 30min → 红              dsh
│  └─ 无                      openclaw · opencode
│
└─ 后代仲裁（P3 下沉 claude）
   ├─ 全套 Active/Stale/Pending   zcode
   ├─ Active/Stale                codex APP
   ├─ DB working 替代             workbuddy
   └─ 无（仅回报打标）            claude · kimi · opencode · dsh · openclaw
```

---

## 6. P0：拆分在场锚点与完成锚点

### 6.1 目标

WorkBuddy 完成冷却改用 `content_age`，修复 30 秒绿黄振荡；同时用 `SessionAges` 固化不变量，防止同类 bug 在别处复发。

### 6.2 改动

**新增 `app_status.rs`：`SessionAges` 定义与唯一构造点**（见 §3.1）。

**改造 `workbuddy_parser.rs:905-926`**：

```rust
// 三源取 max —— 这一层保留，但只在 presence 轴使用
let ages = SessionAges::derive(
    now,
    jsonl_mtime_ms,                                    // content 轴：只取内容源
    &[jsonl_mtime_ms, c.db_activity_ms.map(|a| a.max(0) as u64), c.last_heartbeat_ms],
);

let status = if working {
    SessionStatus::Processing
} else {
    // ① 冷却：content 轴（P0 沿用既有函数名；P2 再替换为 apply_completion_cooldown）
    let cooled = apply_green_debounce(
        d.status_core.clone(),
        d.tail_kind,
        ages.content_age_ms.unwrap_or(u64::MAX),
    );
    // ② 停更降级：presence 轴
    overlay_mtime_stale(cooled, ages.presence_age_ms.unwrap_or(0))
};
```

**`content_age_ms` 为 `None`（mtime 不可得）时的行为**：`unwrap_or(u64::MAX)` 使冷却窗判定必然 `>= GREEN_DEBOUNCE_MS` → 不启用冷却（`None` ≠ 刚刚）。与 `workbuddy_is_live` 的"mtime 未知不是刚刚活跃"口径一致（`workbuddy_parser.rs:774`）。

**P0/P2 的函数名过渡**：P0 只改锚点参数，函数仍是 `apply_green_debounce`；P2 才把它替换为共享核的 `apply_completion_cooldown`。若 P0 与 P2 合并提交，可直接用最终形态。

**`presence_age_ms` 为 `None` 时的行为**：按 0 处理（与现状 `map_or(0, …)` 一致，避免引入行为变更）。

### 6.3 其他工具的改造面

已核实：**只有 workbuddy 存在"一个 age 喂两个判定"**。

| 工具 | age 变量 | 是否需要 P0 改造 |
|---|---|---|
| claude | 单布尔 `file_recently_modified`（3s 阈值，只喂 `determine_status` 兜底分支） | 否 |
| kimi | 单布尔 `file_recently_modified`（60s，只喂兜底） | 否 |
| codex CLI | `mtime_age_ms`，只喂 `overlay_mtime_stale` | 否 |
| codex APP | `age_ms`（updated_at），只喂停更仲裁 + 兜底 | 否 |
| zcode | `age_ms`（time_updated），只喂停更仲裁 + 兜底 | 否 |
| opencode | `is_recent` 布尔（60s），只喂启发式 | 否 |
| dsh | `silence_ms`（log mtime），只喂 v0 兜底 | 否 |
| openclaw | 无 age | 否 |

**结论**：P0 的代码改动只落在 workbuddy + 共享核新增 `SessionAges`。其他工具保持现有局部变量——它们本来就只有一个语义，接入反而增加无谓改动面（决策 4）。`SessionAges` 的价值是**给未来的双轴工具提供唯一构造点**，并作为 review 时的对照基准。

### 6.4 测试

- `workbuddy_heartbeat_does_not_extend_content_age`：心跳新鲜 + jsonl 停更 30s + assistant 尾 → **稳定 Idle**（当前会振荡）
- `workbuddy_working_status_still_pulls_processing`：DB `working` + 心跳新鲜 + jsonl 停更 → **Processing**（保护 AC9）
- `content_age_none_disables_cooldown`：mtime 不可得 + assistant 尾 → 直接 Idle（不因未知而黄）
- `session_ages_derive_takes_max_for_presence_only`：`presence_age` 取多源最大值，`content_age` 只取 content 源
- 现有 `apply_green_debounce` 系列测试全绿（函数语义不变，仅锚点来源改变）

---

## 7. P1：时间常量集中

### 7.1 目标

把散布在 5 个 parser 的停更/新鲜度常量收进 `app_status.rs`，消除"改一个阈值要 grep 全仓"。

### 7.2 常量清单

| 常量 | 现值 | 现位置 | 语义 |
|---|---|---|---|
| `APP_STATUS_STALE_MS` | 300_000 | app_status.rs:37（已集中） | APP 形态停更降级 |
| `FALLBACK_FRESH_MS` | = APP_STATUS_STALE_MS | zcode_parser.rs:56 / codex_thread_parser.rs:53 | 无信号时按新鲜度兜底 |
| `CONTENT_FRESH_CLI_MS` | 3_000 | claude_parser.rs:277（硬编码 `3.0`） | CLI 文件新鲜兜底 |
| `CONTENT_FRESH_KIMI_MS` | 60_000 | kimi_parser.rs:33 `FILE_RECENT_SECS` | Kimi 文件新鲜兜底 |
| `CONTENT_FRESH_OPENCODE_MS` | 60_000 | opencode_parser.rs:1039（硬编码 `60_000`） | OpenCode user 尾 recency |
| `DshV0Silence` | 30min | dsh/status.rs:19 | dsh v0 无锁静默中断 |
| `HEARTBEAT_FRESH_MS` | 90_000 | workbuddy_parser.rs:66 | WorkBuddy 心跳消失补偿 |
| `GREEN_DEBOUNCE_MS` | 10_000 | workbuddy_parser.rs:78 | 完成冷却窗（迁入后改名 `COMPLETION_COOLDOWN_MS`） |

### 7.3 收敛原则

- **语义命名优先于工具命名**：`CONTENT_FRESH_CLI_MS` 而非 `CLAUDE_MS`；同一语义将来若有第二家复用可直接共享。
- **不强行合并同值**：claude 的 3s 与 kimi 的 60s 虽然都叫"新鲜兜底"，但**信任度不同**（claude 是消息模型免疫下的轻兜底，kimi 是事件流缺失时的主力兜底）。合并会掩盖设计意图。
- **保留各家旧名别名**：常量迁入共享核后，原 parser 内以 `pub use` 别名保留旧名，避免一次性改爆测试与外部引用。别名标注 `#[deprecated(note = "迁至 app_status；语义见 CONTENT_FRESH_*")]`。全量改名留到下一次触及这些文件时顺手做（决策 8）。

### 7.4 测试

纯搬家，无行为变更。回归锁：`cargo test` 全绿 + `clippy -D warnings`。额外加一条"常量同源性"测试（可选）：断言 `FALLBACK_FRESH_MS == APP_STATUS_STALE_MS`，防止将来有人单独调一个。

---

## 8. P2：`CooldownSignal` 共享件

### 8.1 目标

把"观测到完成信号 → 压住 hold 时长 → 才允许报绿"抽成通用形态，统一 workbuddy 的**内容时间锚**与 claude 的**事件时间锚**。

### 8.2 设计

```rust
/// 完成冷却信号（纯数据，各 parser 在证据层构造）。
///
/// 语义：在 `observed_at_ms` 观测到"完成候选"，到 `observed_at_ms + hold_ms`
/// 之前不得报绿。`requires_tail` 用于限定只有特定尾部语义才适用冷却
/// （workbuddy 的中间 assistant 假绿只发生在 assistant 尾）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CooldownSignal {
    /// 完成候选被观测到的时刻（**必须是内容/事件时间戳，不得含心跳或 DB 活动**）
    pub observed_at_ms: u64,
    /// 冷却时长
    pub hold_ms: u64,
    /// 仅该尾部语义可被冷却；`None` = 不限尾部
    pub requires_tail: Option<AppEntryKind>,
}

/// 完成冷却（纯函数）。
/// 仅作用于绿态（Idle/Finished）；其余状态透传。
/// `signal` 为 `None` → 透传（无冷却证据 = 不启用）。
pub fn apply_completion_cooldown(
    status: SessionStatus,
    tail_kind: Option<AppEntryKind>,
    signal: Option<CooldownSignal>,
    now_ms: u64,
) -> SessionStatus;
```

### 8.3 两家接入形态

| 工具 | `observed_at_ms` | `hold_ms` | `requires_tail` |
|---|---|---|---|
| **workbuddy** | jsonl mtime（`content_age` 的来源） | `COMPLETION_COOLDOWN_MS = 10_000` | `Some(AssistantMessage)` |
| **claude** | hook `Stop` 事件 `ts` | CLI `cli_grace_secs`(5s) / APP `app_grace_secs`(30s) | `None` |

### 8.4 claude 接入的边界（重要）

claude 当前的 Stop grace 在 hook 应用层（`adapter/mod.rs:173` `apply_hook_event_to_session`），是**状态机内的一段逻辑**，不是纯叠加。把它迁到 `apply_completion_cooldown` 需要：

1. hook 层不再在 Stop 事件后直接压状态，而是**产出 `CooldownSignal`**；
2. parser 构卡层读取该信号并应用冷却。

这条链跨越 hook 通道与 parser 两层，状态传递路径变长。**P2 只做共享件的定义 + workbuddy 接入（行为等价改造）；claude 的迁移不纳入本批**（决策 7）——因为：
- claude 的 Stop grace 目前工作正常（无对应 bug 报告）；
- 迁移涉及 hook → parser 的信号传递，属于架构变更而非缺陷修复；
- 若中途引入回归，会污染本次"修 WorkBuddy 振荡"的主线。

`CooldownSignal` 的 `requires_tail: None` 形态与 claude 的事件时间锚完全兼容，将来迁移无需改共享件接口。

### 8.5 测试

- `cooldown_blocks_green_within_window`：绿态 + 有效信号 + 未到点 → Processing
- `cooldown_passes_green_after_window`：绿态 + 有效信号 + 已到点 → Idle
- `cooldown_passes_non_green`：黄/红态透传
- `cooldown_none_signal_passes_through`：无信号 → 透传
- `cooldown_requires_tail_mismatch_passes`：`requires_tail=AssistantMessage` 但尾部是 ToolCall → 透传
- `workbuddy_cooldown_uses_content_age_not_presence`：心跳新鲜但不影响冷却（P0 与 P2 的联合回归锁）

---

## 9. P3：claude Pending 拦绿（⚠️ 本批不实施）

> **状态更新（2026-11-04，实施期裁决）**：本节设计曾在实施验证分支上完成实现，
> 但真机取证与 code review 发现两个阻断问题，故**未纳入本批交付**；
> 交付范围收敛为 P0 + P1 + P2。阻断问题详见 §9.7。
> 本节其余内容保留作为设计记录与重做依据。

### 9.1 可行性（已完成调查）

| 工具 | 派发/结算配对 | Active/Stale | Pending | 工程量 | 结论 |
|---|---|---|---|---|---|
| **claude** | ⚠️ 状态机现成，但 Stop 落盘形态已变（见 §9.7） | 可行 | **受阻** | 200–350 行 | **本批不做** |
| opencode | ❌ 未取证 | 可行（一条 SQL） | 受阻 | 100–200 行 | 本批不做 |
| kimi | ❌ 完成标记未取样 | 可行（mtime） | 受阻 | 150–250 行 | 本批不做 |

**关键发现**：`subagents/claude.rs:488-496` 已有生产在用的三态机：

```
is_running = registered ∧ last ∈ {None, Spawn, Resume}
```

语义就是 zcode 的"账未结清"（`run_in_background` 派发已见、`background_notification` 结算未见）。**claude 的 Pending 数据是现成的**，只差一个聚合出口 + parser 调用点。

### 9.2 设计

**新增 `subagents/claude.rs`**：

```rust
/// 后代活动聚合出口（P3）：一次遍历既有 ClaudeCache 状态机，产出三元组。
/// - active_count：`is_running` 为真的子 agent 数
/// - latest_child_activity_ms：子转写末条 timestamp 的最大值（活着但静默也可见）
/// - has_unsettled：存在「已登记 ∧ 未见 Stop」的子 agent（zcode Pending 语义）
pub fn descendant_activity_at(
    parent_jsonl: &Path,
    session_id: &str,
    now_ms: u64,
) -> (usize, Option<i64>, bool);
```

**`claude_parser.rs` 接入**（在构卡末尾，`determine_status` 之后）：

```rust
// P3：完成仲裁——存在未结算子代理时，回合收尾不得报绿。
// 必须在所有转绿路径之后（本函数转绿路径：assistant 正文档 Idle + hook 层
// Stop grace 过期 → Idle），这一处调用同时覆盖。
let (_, _, has_unsettled) = claude_subagents::descendant_activity_at(&jsonl, &session_id, now_ms);
if has_unsettled {
    status = overlay_completion_with_pending(status, DescendantActivity::Pending);
}
```

### 9.3 为什么只做 Pending，不做 Active/Stale

claude 的 `determine_status`（`status.rs:128-167`）**没有 300s 停更降级**——"Processing 停更"这个触发点根本不存在：

```rust
Some("assistant") => {
    if has_tool_use { Processing }        // 无停更降级
    else if last_has_text { Idle }
    else { Thinking }
}
_ => if file_recently_modified { Processing } else { Waiting }  // 3s 兜底
```

要让 `overlay_stale_with_descendants`（Active 保黄）生效，必须先给 claude 补一个 300s 停更叠加——那是**行为变更**（会让长工具调用的 claude 卡在 300s 后变红），不是修 bug。**留作 v2，单独立项。**

### 9.4 必须补的僵尸保护

Stop 通知丢失会让 `is_running` 永真 → 会话永远不报绿。需仿 zcode（`zcode_parser.rs:737-748` 的 24h 僵尸保护）：

- 子 agent 登记时间超过 24h 仍未结算 → 不再计入 `has_unsettled`；
- 判定依据：meta 文件的 `spawn_ts`（`subagents/claude.rs` 已捕获）。

### 9.5 已知边界（如实申报）

- `meta` 残留随会话中断整区消失（`claude.rs:11` 注释，v1 裁决不做 mtime 兜底）→ 会话中断后 `has_unsettled` 自然为假，行为回到现状；
- teammate 机制同名冲突（`claude.rs:83` 已申报）；
- `spawnDepth` 刻意不解析（`claude.rs:581-584`，只收一层）→ 孙子代子代理不可见。

### 9.6 测试

- `pending_child_blocks_main_green`：父 assistant 尾（Idle）+ 子 meta 已登记未 Stop → **Waiting**
- `settled_child_allows_green`：子 meta 已 Stop → **Idle**
- `resumed_child_blocks_again`：Stop 后 Resume → 再次拦截
- `zombie_meta_does_not_block_forever`：meta 登记 >24h 未结算 → 不拦截
- `no_children_keeps_behavior`：无子 agent → 行为与现状逐分支一致（零回归锁）
- `non_green_passes_through`：黄/红态不受 Pending 影响

### 9.7 未纳入本批的原因（2026-11-04 实施期发现）

P3 曾在实施验证过程中完成编码并通过 parser 级测试；随后 code review 与真机取证
发现两个阻断问题，经用户裁决**不纳入本批交付**，交付范围收敛为 P0 + P1 + P2。

**阻断 1：Stop 通知落盘形态已变，`classify_parent_line` 命中率约 1.5%**

§9.2 的「Pending 数据是现成的」建立在「`<task-id>` 位于 `message.content`」这一
2026-10-08 的形态假设上。真机扫描（证据：
`docs/engineering/evidence/claude-task-id-location-20261104/`）显示现行 Claude Code
的 134 条 `<task-id>` 落盘位置分布为：

| 条目 type | 承载字段 | 条数 |
|---|---|---|
| `queue-operation` | `.content` | 42 |
| `attachment` | `.attachment.prompt` | 20 |
| `attachment` | 渲染字段 | 63 |
| `assistant` | 消息文本提及 | 2 |
| `user` | `.message.content` | **2** |

`classify_parent_line` 只读 `v["message"]["content"]` → **命中率约 1.5%**。
Stop 读不到 ⇒ 子代理永远 `Running` ⇒ `has_unsettled` 恒真 ⇒
**主会话被拦绿最长 24 小时**（只有僵尸窗兜底）。这比它要修的「误报完成」更糟。

chip 展示层影响小（灰点不准可接受，v1 已申报）；但接成**会话级判定门槛**后果严重。

**阻断 2：Pending 仲裁位置被 hook 叠加层覆盖**

`claude_parser.rs` 内的 Pending 叠加发生在 `adapter.find_sessions()` 内，而
`adapter/mod.rs` 的 hook 叠加层在其**之后**执行：Stop grace 过期（CLI 默认 5s，
事件 TTL 30s）会把 `Waiting` 改回 `Idle`。而「派发后台子代理 → 结束本回合 → Stop」
正是 P3 的目标场景 ⇒ 每轮轮询覆盖一次，功能失效。§4.3 要求的「Pending 最后一道」
没有满足。

**重做 P3 的前置条件**（已登记 issue #200）：

1. 取证并扩写取文范围：`message_text` 覆盖 `queue-operation.content`、
   `attachment.prompt`、`<agent-message from=…>`；用真实脱敏夹具验证。
2. 架构位置调整：Pending 仲裁移到 `apply_wait_mark_overlay` 之后（adapter 层），
   或 parser 只带出 `has_unsettled` 字段、由 adapter 统一仲裁。
3. 补集成级测试覆盖 hook 叠加面（parser 级测试到不了 hook 层）。
4. `spawn_ts=None` 的僵尸保护上界：回落 `meta.json` 文件 mtime
   （`registered_agents` 已 stat，成本为零）。注意 meta 永久留存已实证
   （`c3-subagent-chips-acceptance-2026-10-08.md:147`），不能指望它自己消失。
5. 补 §9.6 缺失用例：`resumed_child_blocks_again`、parser 级 `settled_child_allows_green`。


---

## 10. 明确不动清单

| 对象 | 理由 |
|---|---|
| `derive_app_status` 核契约 | 保持「最后一条说了什么」定位；锚点与仲裁都叠加在外层 |
| codex CLI 回合守卫 | 事件序锚，零时间常数，是最健壮的形态，作为参照实现 |
| zcode 全套仲裁 | 已有 Active/Stale/Pending，且是唯一完整实现 |
| dsh 锁租约 | 持有式 fd 租约语义独特，与心跳不同源，不强行统一 |
| openclaw CPU 启发式 | 无内容信号，属另一族问题，单独立项 |
| opencode `reason=length` 映射 | 与 zcode 语义相反（宁黄不假绿），`opencode_parser.rs:923` 明示刻意不共享 |
| 前端消费层（petStatus / useNotification / 未读池） | 边沿触发链路本身正确；源头状态正确后无需改动 |
| 各工具 L1 格式翻译层 | 工具协议差异是真实差异，统一会引入回归 |

---

## 11. 实施顺序与验证

```
P0 (SessionAges + workbuddy 修锚点)          ✅ 已交付（唯一的行为修复）
  └─ 验证：真机 WorkBuddy 完成任务后稳定绿、无周期振荡、只播一次提醒
P1 (常量集中)                                ✅ 已交付（纯重构）
  └─ 验证：零行为变更（cargo test 全绿 + clippy 干净）
P2 (CooldownSignal + workbuddy 接入)         ✅ 已交付（纯重构）
  └─ 验证：workbuddy 行为与 P0 等价；claude 不接入则零风险
P3 (claude Pending)                         ⚠️ 已撤回（见 §9.7）
  └─ 后续见 issue #200
```

**P0 是唯一的行为修复，P1/P2 是重构。** 若时间紧张，P0 必须落地，P1/P2 可合并为一个提交。

### 11.1 验收标准

1. **单测**：§6.4 / §7.4 / §8.5 全部落地，夹具优先取自真机数据脱敏。
   （§9.6 的 P3 用例随 P3 撤回，重新立项时补）
2. **回归**：`cd src-tauri && cargo test` 与基线失败集一致（本仓存在 8 条既有基线失败，
   与本批无关，见 §11.2）；`cargo clippy --all-targets -- -D warnings` 干净；
   `cargo fmt --check` 干净。
3. **手工验收（P0 核心）**：
   - WorkBuddy 跑一个多轮任务，中途不出现绿→黄振荡；
   - 任务真正完成后转绿，**且只播一次完成提醒**；
   - 完成后静置 5 分钟，卡片保持绿，不回黄。

### 11.2 基线失败集（与本次改动无关）

本仓 `cargo test --lib` 在本次工作前即有 8 条失败，逐条与改动后一致
（2026-11-04 由独立导出的 `eb95df3f` 树验证）：

- `monitor::claude_parser::title_tests::get_claude_sessions_badge_end_to_end_new_layout`（HOME 重定向）
- `monitor::codex_parser::report_flag_tests::*`（90s 打标窗）
- `monitor::opencode_parser::v2_tests::report_flag_*`（90s 打标窗）
- `remote::server::tests::file_endpoints_are_gated_and_safe`（tempdir 大小写）
- `services::subagent_detection_tests::*`（2 条）
- `services::zcode_control::relay::tests::stalled_write_is_bounded_and_goes_offline`
- `services::zcode_control::wire_e2e::tests::attachment_uploads_to_bound_workspace_and_flows_into_send_body`

---

## 12. 已知限制与 follow-up

- **WorkBuddy 冷却漏防**：中间 assistant 消息后模型停顿 >10s 仍会漏放一次假绿（启发式上限，2026-09-16 spec 已接纳）。根治需 WorkBuddy 提供轮次边界信号，属上游依赖。
- **`SessionAges` 仅 workbuddy 使用**：其余工具当前单轴，接入是可选一致性工作；未来出现第二个双轴工具时应强制使用。
- **claude Pending 拦绿**：已撤回（§9.7）；重做前置条件见 issue #200。
- **claude Active/Stale 仲裁**：需先给 claude 定义 300s 停更降级语义（行为变更），留 v2。
- **opencode Pending**：需实机取证 message/part 中是否存在 task 派发行与结算行，未取证前不实施。
- **kimi Pending**：官方 Agent 工具的完成标记未取样（`2026-10-08-mobile-subagent-chips-design.md` §4.3 校准点②），受阻。
- **停更阈值哲学分裂**：claude/kimi/codex-APP 保留兜底红，codex-CLI/zcode/opencode 不落红——这是历史裁决（2026-09-10 / 2026-09-17），本设计**不推翻**，仅在 P1 集中常量时标注各自意图。

---

## 13. 决策记录

| # | 决策点 | 结论 | 日期 |
|---|---|---|---|
| 1 | 「一个 age 只服务一个语义」的强制手段 | **具名结构体 `SessionAges`**（唯一构造点）；不用 newtype（样板过多），不用纯文档（本次 bug 证明不足） | 2026-11-04 |
| 2 | P2 共享件形态 | **`CooldownSignal` 统一两种锚**（内容时间锚 + 事件时间锚）；不只搬 `apply_green_debounce` | 2026-11-04 |
| 3 | P3 本批范围 | ~~只做 claude Pending 拦绿~~ → **实施后整块撤回**（§9.7）；交付收敛为 P0+P1+P2 | 2026-11-04 |
| 4 | P0 改造面 | 只落 workbuddy + 共享核新增 `SessionAges`；其余工具不接入（单轴，接入是无谓改动） | 2026-11-04 |
| 5 | 判定顺序 | 冷却 → 停更降级 → 二次强信号 → Pending 拦绿（Pending 必须最后） | 2026-11-04 |
| 6 | 常量命名原则 | 语义命名优先于工具命名（`CONTENT_FRESH_CLAUDE_MS` 而非 `CLAUDE_MS`）；不强行合并同值 | 2026-11-04 |
| 7 | claude Stop grace 是否迁入 CooldownSignal | 本批**只留接口不接**，避免架构变更污染缺陷修复主线 | 2026-11-04 |
| 8 | P1 旧常量名如何处理 | `GREEN_DEBOUNCE_MS` 别名实际无引用 → 直接删除，未用 `#[deprecated]`（避免 `-D warnings` 失败） | 2026-11-04 |
| 9 | `SessionAges` 字段可见性 | 实施期改为**字段私有 + getter**：「唯一构造点」由类型系统保证，而非 review 纪律 | 2026-11-04 |
| 10 | P3 撤回理由 | ① Stop 通知落盘形态已变（`classify_parent_line` 命中率约 1.5%，会拦绿 24h）；② Pending 位置被 hook 叠加层覆盖 | 2026-11-04 |

---

## 14. 实施记录（2026-11-04）

### 已交付

| 批次 | 内容 | 提交 |
|---|---|---|
| **P0** | `SessionAges` 双轴年龄（字段私有 + getter，唯一构造点）；workbuddy 改用 `content_age` 喂冷却 | `c55625dc` `697c94e0` `0e9c8fcd` |
| **P1** | 时间常量集中到 `app_status.rs`（dsh 30min 刻意不迁） | `1a5427e3` |
| **P2** | `CooldownSignal` + `apply_completion_cooldown` 共享件；workbuddy 接入 | `5f7916a1` `6b667f6e` |

**P0 是唯一的行为修复**：心跳每 30s 刷新曾把 `eff_age` 打回 0，使 10s 完成冷却窗
永远等不满，造成任务结束后的绿↔黄 30 秒周期振荡并重复播报完成提醒。

分支：`fix/session-age-anchor-discipline`（基于 `main` @ `aebe900c`）。

### 未纳入本批

| 批次 | 内容 | 原因 |
|---|---|---|
| P3 | claude Pending 拦绿（`descendant_activity_at` + parser 接入） | §9.7 两个阻断问题 |

重做前置条件见 §9.7；真机取证见
`docs/engineering/evidence/claude-task-id-location-20261104/`；跟踪 issue #200。

### Review 修复清单（`0e9c8fcd`）

| 编号 | 问题 | 处置 |
|---|---|---|
| M2 | `SessionAges` 字段 `pub` 允许绕过唯一构造点 | 改私有 + `content_age_ms()` / `presence_age_ms()` getter |
| M3 | `get_workbuddy_sessions_with` 文档仍描述已删除的 `eff_age` | 改写为双轴口径并注明不可混用 |
| M4 | 常量大小关系断言恒真、依赖方向错误 | 删除，改用 board 级正反例对 |
| M6 | `observed_at_ms: jsonl_mtime_ms.unwrap_or(0)` 是不可达假兜底 | 改为 `jsonl_mtime_ms.map(\|mtime\| CooldownSignal { observed_at_ms: mtime, .. })` |
| I4 | `missing_content_mtime_disables_cooldown` 测错分支（删文件后走不到状态合成） | 改为直接测 `SessionAges::derive(None)` + 信号派生 + 冷却透传 |
| I5 | 缺「内容新鲜 → 冷却必须生效」的正向用例 | 补 `fresh_content_still_triggers_cooldown_despite_heartbeat` |
| I3 | `cargo fmt --check` 有 12 处 diff | 跑 `cargo fmt` |

### 基线失败集

`cargo test --lib`：4410 passed / 8 failed / 46 ignored。8 条失败在本次工作前即存在
（由独立导出的 `eb95df3f` 树逐条验证），清单见 §11.2。
