//! ZCode 采集器（说明书 §5.1 / §9.5：**A、B 两个指标都最完整的源**）。
//! * 库为 762 MB 且**无 `-wal/-shm`**（表头 `write/read version = 2` 即 WAL 模式，伴随文件已被
//!   checkpoint 清掉）→ 一律走 `open_usage_db`（先 ro + 探针，探针失败才回退 `immutable=1`）。
//!   **本机实测（报告「实机项」）**：rusqlite（内置 sqlite 3.46）下 `mode=ro` 句柄能开、
//!   探针 `SELECT count(*) FROM sqlite_master` **成功**（中位 4 µs）→ 真机语料实际走的是 **ro** 路径；
//!   同机 CLI 3.54 在 `?mode=ro` 上报 `CANTOPEN(14)`（库差异，D-35）——所以 immutable 回退在本机
//!   不触发，但仍是「工具正在写（有 `-wal`）/ 库差异」形态下的唯一可用路径，由 Task 14 的合成用例锁住；
//! * **turn 数取 `turn_usage` 行数**（权威）——`distinct model_usage.turn_id` 会多出幽灵 turn
//!   （真机 1,209 vs 1,207）；**665 会话中 118 个无 turn 行 → 左连补 0**（不丢会话）；
//! * 四桶取 `model_usage` 逐请求全字段（真机 25,550/25,550 行都满足
//!   `computed_total_tokens == input + output` → 判据在场，逐条判定；其中
//!   `cache_read + cache_creation > 0` 的判 `Subset`，全 0 的 817 行两式并列 → 按规则 2 落互斥）；
//! * 工具耗时 `tool_usage.duration_ms` **非空率 100%**（七源唯一免配对）；
//! * 报错三层 + `cancelled_by_user` 单列（**不得**把 cancelled 计成故障，也**不得**把同一次
//!   用户打断在三张表上各计一次）；
//! * 项目取 `session.directory`（`project_id` 是有损 slug，不可反解）；
//! * 增量：`model_usage` / `turn_usage` / `tool_usage` **三张表都按 rowid 水位**（游标 ordinal
//!   复用为 max rowid；rowid 探测不到（WITHOUT ROWID）时该表退化为每轮全量——本机三表均有 rowid）。
//!
//! ## 偏差（逐条见报告「偏差申报」，均已在测试里配真能变红的锁）
//! * **`parsed_files = 1`**（W-50 控制窗口裁定）：SQLite 源读的是**一个库文件**，
//!   不是会话行数（照计划 L10172 逐会话累加会让 `UsageSourceStatus.parsed_files` 这个用户可见
//!   数字出现两种语义）；
//! * **四段 SQL 的失败一律整源 `Err`**（**含 `load_sessions`**，与 opencode 的 V1/V2 表探测
//!   软失败**刻意相反**：zcode 是单 schema，列名不符 = 缺陷，不是预期分支）；
//! * **行级错误也整源 `Err`**（D-38 同族）：计划写「字段类型不符即跳过该行」（`rows.flatten()`）
//!   ——那是静默少算；
//! * **`time_updated` 为 NULL 不得 `COALESCE(…,0)`**（D-23 尾部同族）：缺字段 → 本轮采集时刻
//!   兜底 + `log::warn!`；真的是 0 照 0 走；
//! * **`interrupted` 只在「模型层 + 回合层」两处累加**：同一回合内的工具 `cancelled_by_user`
//!   与回合的 cancelled 是**同一次**用户打断（本机 9/9 条工具取消行都落在已计的 64 个 cancelled
//!   回合里），再加一次就是重复计。
//!
//! ## fix round 1（评审 Needs fixes：1 Critical + 2 Important + 5 Minor；逐条见报告的 fix 报告段）
//! * **A（Critical）**：`is_subagent = parent_id 非空`，并把 `parent_id` 传进 `parent_session_id`
//!   —— 真机 **71 父 / 594 子**，漏了它「会话数」会显示 665（应 71）、「turn 数」≈1207（应 605）
//!   （GC 18-R3 / D17：计数类只计父会话）；
//! * **B（Important）**：水位**只消费连续终态前缀** —— `status='running'` 的行是插入后**原地 UPDATE**
//!   收尾的，跨过它就会**永久**丢掉该行的原生时长 / 报错 / 打断样本；
//!   **僵尸安全阀**：`started_at` 早于 `ZOMBIE_RUNNING_MS`（24 h）的 `running` 行强制跨过 + 告警 + 计数
//!   （否则水位永久冻死）；
//! * **C（Important）**：turn / tool 行的模型与供应商**跨轮兜底**（回查该会话最后一条 `model_usage`）；
//!   确实拿不到 → **跳过该行** + 计数 + 告警，**绝不**写 `model=""` 明细行（D-32 先例）；
//! * **D（Minor）**：#4 补正向「轮间追加 1 行 → 第二轮恰采 1 行」锁；#5 `started_at` 缺失不再
//!   `COALESCE(…,0)`（D-23 尾部同族）；#6 tool 的 C2 只改 `tool_name` 一列（`cancelled_by_user`
//!   已不在 SQL 里，改名是空动作）；#7 错误码拆开 —— **打不开** = `usage-source-db-open`，
//!   **表/列/行级读不动** = `usage-source-io`（后者会出现在 Task 24 的验收面上）。

use rusqlite::{Connection, OptionalExtension};

use std::collections::HashMap;

use super::DeltaBuilder;
use crate::monitor::zcode_parser::ZcodeRoots;
use crate::services::usage::collect::{CollectContext, UsageCollector};
use crate::services::usage::delta::{CursorDelta, DetailKey, SourceDelta};
use crate::services::usage::error::UsageError;
use crate::services::usage::model::UsageSourceId;
use crate::services::usage::project::project_key_of;
use crate::services::usage::range::hour_key_of_host;
use crate::services::usage::semantics::{default_policy, normalize, resolve_semantics, RawUsage};

/// 僵尸 `running` 行的判定阈值（裁决 B 的**安全阀**，控制窗口追加要求）。
///
/// 依据（真机证据）：`turn_usage.duration_ms` 最长 **>1 h**（13 h 58 min 那条是 `completed`，
/// 见 D19「含挂机」注记）、`tool_usage` 最长 **5,002,673 ms ≈ 83 min** → 取 **24 h**，
/// 比真机任何一次**真的还在跑**的行宽一个数量级（不可能误伤活的在飞行行）。
/// 超过阈值仍 `running` → 判为「崩溃/被杀留下的僵尸行」，**强制跨过**水位（该行照常入账，
/// 只是没有时长样本）+ `log::warn!` + 计数，否则一条僵尸行会把该源的水位**永久冻死**。
const ZOMBIE_RUNNING_MS: i64 = 24 * 60 * 60 * 1000;

/// 采集器**私有**游标状态（进 `model_usage` 游标的 `state_json`；**非契约形状**、不进任何用户可见
/// 数字，只为「不静默」留痕——与 kimi 的 `dropped_no_model`（W-51）同类，只是本源的容器是游标
/// 而不是文件状态）。两个计数都是**跨轮累计**。
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
struct ZcodeCursorState {
    /// 被**强制跨过**的僵尸 `running` 行数（≥ `ZOMBIE_RUNNING_MS` 仍未终态）
    #[serde(default)]
    zombie_running: i64,
    /// 因「会话拿不到模型/供应商来源」被**跳过**（绝不写 `model=""` 行）的行数（D-32 先例）
    #[serde(default)]
    dropped_no_model: i64,
}

pub struct ZCodeCollector;

impl UsageCollector for ZCodeCollector {
    fn source_id(&self) -> UsageSourceId {
        UsageSourceId::ZCode
    }
    fn collect(&self, ctx: &CollectContext<'_>) -> Result<SourceDelta, UsageError> {
        let roots = ZcodeRoots::from_home(ctx.home);
        if !roots.cli_db.exists() {
            return Ok(SourceDelta::default());
        }
        Self::collect_with_roots(ctx, &roots)
    }
}

impl ZCodeCollector {
    /// roots 注入版（单测直调，严禁触真实 `~/.zcode`）
    pub fn collect_with_roots(
        ctx: &CollectContext<'_>,
        roots: &ZcodeRoots,
    ) -> Result<SourceDelta, UsageError> {
        let Some(conn) = crate::monitor::sqlite::open_usage_db(&roots.cli_db) else {
            return Err(UsageError::new(
                "usage-source-db-open",
                "zcode db.sqlite open failed (ro and immutable)",
            ));
        };
        // **供应商规则从 ctx 注入**（§3.2.3 FIX-6）：lib 单测构建注入空规则 → 零 DB；
        // 生产/集成构建才回落到 settings::load()。采集器不得直连设置层。
        let rules = ctx.provider_rules();
        let mut b = DeltaBuilder::new(UsageSourceId::ZCode, rules);
        // **W-50（控制窗口裁定）**：`parsed_files` 是 `UsageSourceStatus` 的**用户可见**数字，
        // 口径 = 「解析了几个文件」——SQLite 源读的是**一个库文件**（计划 L10172 的「逐会话
        // `parsed_files += 1`」会让同一个 UI 字段出现两种语义：本机会显示 665）。
        b.parsed_files = 1;
        // 游标**私有状态**（跨轮累计的「不静默」计数）：只在 `model_usage` 游标上读写一次。
        // 形状不是契约的一部分（W-51 同类），反序列化失败 → 归零重来（不影响任何用户可见数字）。
        let mut state: ZcodeCursorState = ctx
            .cursor_of("model_usage")
            .and_then(|c| serde_json::from_str(&c.state_json).ok())
            .unwrap_or_default();
        // 1) 会话维度：**左连语义**——先登记全部会话（含 118 个无 turn 行的）。
        //    **失败即 Err**（§3.2.2 阻塞 3）：旧实现 `prepare()` 失败只 `warn` + 返回空表，
        //    结果是"会话维度全丢 + 项目/模型反查表全空 + `status.ok = true`"——
        //    与 C2 要防的"静默出零"是同一个病。四段 SQL 一律响亮失败。
        let sessions = load_sessions(&conn)?;
        for s in &sessions {
            // **D-23 尾部同族**：`time_updated` 缺失（NULL）与「真的是 0」是两件事——
            // 计划这四段 SQL 用的是 `COALESCE(time_updated,0)`，会把「缺字段」伪装成
            // 一个合法时间戳 0 → 该会话的 `last_seen_at` 落 0 / 1970 桶（正是 opencode 那条
            // 已被修掉的缺陷）。缺字段 → **本轮采集时刻**兜底并留痕；真的是 0 → 照 0 走。
            let ts = match s.time_updated {
                Some(t) => t,
                None => {
                    log::warn!(
                        "usage/zcode: 会话 {} 的 time_updated 为 NULL → 本轮按采集时刻 {} 归属\
                         （不伪装成 0 / 1970）",
                        s.id,
                        ctx.now_ms
                    );
                    ctx.now_ms
                }
            };
            // **裁决 A（Critical）**：`is_subagent = parent_id 非空`（唯一判据）。
            // 真机 665 会话 = 71 父（`parent_id` 为空）+ 594 子，而 594 = `subagent_child` 586 +
            // `fork` 7 + `selection_side_chat` 1 ——「`parent_id` 非空」与「`task_type != 'interactive'`」
            // **完全等价**（真机两向都验过：`task_type<>'interactive' AND parent_id 非空` = 594、
            // `interactive AND parent_id 非空` = 0）。控制窗口裁定用 `parent_id` **一次解决**，
            // **不引入第二套判据**——`task_type` 只用于下面的分歧留痕，不参与任何分支。
            // 后果（若漏了这一步）：594 个子会话被当成父会话 → 按 GC 18-R3 / D17（会话数、turn 数
            // **都只计父会话**）的口径，「会话数」会是 665（应 71）、「turn 数」≈1207（应 605）。
            let is_subagent = s.parent_id.is_some();
            if is_subagent != (s.task_type != "interactive") {
                log::warn!(
                    "usage/zcode: 会话 {} 的子会话判据分歧（parent_id 非空 = {is_subagent}，\
                     task_type = {:?}）→ 按控制窗口裁定仍以 `parent_id` 为准",
                    s.id,
                    s.task_type
                );
            }
            b.session(
                &s.id,
                Some(&s.directory),
                None,
                is_subagent,
                s.parent_id.clone(),
                None,
                ts,
            );
        }
        // 记录级项目键的反查表：turn_usage / tool_usage 行只带 session_id，
        // 其项目归属就是该会话的 `session.directory`（**不是**有损的 project_id）。
        let pk_by_session: HashMap<String, String> = sessions
            .iter()
            .map(|s| (s.id.clone(), project_key_of(Some(&s.directory))))
            .collect();
        // 「会话 → (model_id, provider_id) 原始对」反查表：turn / tool 行**不带模型**（证据文档：
        // 会话当前模型在 `session_entry(type='runtime/model_selection')` 里，不在 session 行上），
        // 本轮已读到的 model_usage 行优先（首个带模型者胜）；**本轮读不到的会话由
        // `session_raw_model_provider` 回查兜底**（裁决 C：跨轮/单表增量时不得落空模型行）。
        // `None` = 已确认「该会话没有带模型的行」→ 调用方跳过该行（**绝不写 `model=""`**）。
        let mut model_provider_by_session: HashMap<String, Option<(String, String)>> =
            HashMap::new();
        // 2) 四桶：model_usage 逐请求（rowid 增量；**水位不跨在飞行行**，裁决 B）
        let after = ctx.cursor_of("model_usage").map(|c| c.ordinal).unwrap_or(0);
        let until = inflight_boundary(&conn, "model_usage", after, ctx.now_ms, &mut state)?;
        let rows = load_model_usage(&conn, after, until)?;
        b.new_records += rows.len() as i64;
        let mut max_rowid = after;
        for r in rows {
            max_rowid = max_rowid.max(r.rowid);
            let ts = started_at_or_now(r.started_at, ctx.now_ms, "model_usage");
            let hour = hour_key_of_host(ts);
            // **供应商唯一入口**（GC 6 / B6）：zcode 有直接字段 provider_id → Measured
            let (provider, _) = b.provider_of(
                if r.provider_id.is_empty() {
                    None
                } else {
                    Some(&r.provider_id)
                },
                &r.model_id,
            );
            // 顺手记住「会话 → (model_id, provider_id) 原始对」（供本轮后面的 turn / tool 行补位；
            // 首个带模型者胜）。**跨轮**（本轮没有该会话的新 model 行时）由
            // `session_raw_model_provider` 回查兜底 —— 裁决 C。
            if !r.model_id.is_empty() {
                model_provider_by_session
                    .entry(r.session_id.clone())
                    .or_insert_with(|| Some((r.model_id.clone(), r.provider_id.clone())));
            }
            let key = DetailKey {
                session_id: r.session_id.clone(),
                hour_key: hour.clone(),
                day_key: hour.chars().take(10).collect(),
                project_key: pk_of(&pk_by_session, &r.session_id),
                model: r.model_id.clone(),
                provider,
            };
            let raw = RawUsage {
                input_raw: r.input,
                cache_read: r.cache_read,
                cache_write: r.cache_creation,
                output: r.output,
                reasoning: r.reasoning,
                total_raw: Some(r.computed_total),
            };
            let sem = resolve_semantics(default_policy(UsageSourceId::ZCode), &raw);
            if let Some(n) = normalize(&raw, sem) {
                let d = b.detail(key.clone(), ts);
                d.buckets.add(&n.buckets);
                d.request_total += n.request_total;
                d.requests += 1;
                // **后写者胜**（= claude / codex / workbuddy 三源同一模式）：明细行键不含语义，
                // 同一行内真机确有语义混存（本机 216 行）→ 该标签只能是近似；本列在
                // `database/schema.rs:215` 注明「诊断用、不参与聚合」。
                d.cache_semantics = sem;
            }
            // 模型/传输层错误：status='error'（真机 21 条，8 类）；cancelled 单列（真机 53 条）
            let d = b.detail(key, ts);
            match r.status.as_str() {
                "error" => d.counters.error_model += 1,
                "cancelled" => d.counters.interrupted += 1,
                _ => {}
            }
        }
        // 3) turn 行（权威）+ 原生时长 + 回合失败 + 打断
        //    **B4 修复点**：turn_usage 也必须按 rowid 水位增量——旧实现每轮全量重读，
        //    `turns` / `turn_ms` / `error_turn` / `interrupted` 随采集轮次线性膨胀。
        let turn_after = ctx.cursor_of("turn_usage").map(|c| c.ordinal).unwrap_or(0);
        let turn_until =
            inflight_boundary(&conn, "turn_usage", turn_after, ctx.now_ms, &mut state)?;
        let turn_rows = load_turn_usage(&conn, turn_after, turn_until)?;
        let mut turn_max_rowid = turn_after;
        b.new_records += turn_rows.len() as i64;
        for t in turn_rows {
            // **水位先推进再判跳过**（`continue` 的行也必须被水位越过，否则它会永久堵住）
            turn_max_rowid = turn_max_rowid.max(t.rowid);
            let ts = started_at_or_now(t.started_at, ctx.now_ms, "turn_usage");
            // 模型 / 供应商：本轮已读到该会话的 model 行 → 直接用；否则回查（裁决 C）。
            // 两者都没有（该会话根本没有带模型的行，真机 0 个）→ **跳过该行**（D-32 先例：
            // 显式丢弃 + 计数 + 告警，**绝不**写 `model=""` / `provider=""` 的明细行）。
            let Some((raw_model, raw_provider)) =
                session_raw_model_provider(&conn, &mut model_provider_by_session, &t.session_id)?
            else {
                state.dropped_no_model += 1;
                log::warn!(
                    "usage/zcode: 会话 {} 的 turn 行（rowid={}）拿不到模型来源 → 跳过\
                     （不写空模型明细行），累计 {} 行",
                    t.session_id,
                    t.rowid,
                    state.dropped_no_model
                );
                continue;
            };
            let (provider, _) = b.provider_of(
                if raw_provider.is_empty() {
                    None
                } else {
                    Some(&raw_provider)
                },
                &raw_model,
            );
            let hour = hour_key_of_host(ts);
            let key = DetailKey {
                session_id: t.session_id.clone(),
                hour_key: hour.clone(),
                day_key: hour.chars().take(10).collect(),
                project_key: pk_of(&pk_by_session, &t.session_id),
                model: raw_model,
                provider,
            };
            let d = b.detail(key, ts);
            d.counters.turns += 1;
            if let Some(ms) = t.duration_ms {
                d.counters.turn_ms.push(ms);
            }
            match t.status.as_str() {
                "error" => d.counters.error_turn += 1,
                // 回合层 cancelled = 用户主动打断（真机 64 条 = `cancelled_by_user=1` 的 64 条，
                // 两者**完全一致**，证据文档同此）
                "cancelled" => d.counters.interrupted += 1,
                _ => {}
            }
        }
        // 4) 工具：原生 duration_ms（唯一免配对）+ status='error'（真机 343）+ exit_code<>0（真机 654）
        let tool_after = ctx.cursor_of("tool_usage").map(|c| c.ordinal).unwrap_or(0);
        let tool_until =
            inflight_boundary(&conn, "tool_usage", tool_after, ctx.now_ms, &mut state)?;
        let tool_rows = load_tool_usage(&conn, tool_after, tool_until)?;
        let mut tool_max_rowid = tool_after;
        b.new_records += tool_rows.len() as i64;
        for t in tool_rows {
            tool_max_rowid = tool_max_rowid.max(t.rowid);
            let ts = started_at_or_now(t.started_at, ctx.now_ms, "tool_usage");
            let Some((raw_model, raw_provider)) =
                session_raw_model_provider(&conn, &mut model_provider_by_session, &t.session_id)?
            else {
                state.dropped_no_model += 1;
                log::warn!(
                    "usage/zcode: 会话 {} 的 tool 行（rowid={}）拿不到模型来源 → 跳过\
                     （不写空模型明细行），累计 {} 行",
                    t.session_id,
                    t.rowid,
                    state.dropped_no_model
                );
                continue;
            };
            let (provider, _) = b.provider_of(
                if raw_provider.is_empty() {
                    None
                } else {
                    Some(&raw_provider)
                },
                &raw_model,
            );
            let hour = hour_key_of_host(ts);
            let key = DetailKey {
                session_id: t.session_id.clone(),
                hour_key: hour.clone(),
                day_key: hour.chars().take(10).collect(),
                project_key: pk_of(&pk_by_session, &t.session_id),
                model: raw_model,
                provider,
            };
            let d = b.detail(key, ts);
            d.counters.tool_calls += 1;
            if let Some(ms) = t.duration_ms {
                d.counters.tool_ms += ms;
                d.counters
                    .tool_stats
                    .entry(t.name.clone())
                    .or_insert((0, 0))
                    .1 += ms;
            }
            d.counters
                .tool_stats
                .entry(t.name.clone())
                .or_insert((0, 0))
                .0 += 1;
            if t.status == "error" {
                d.counters.error_tool += 1; // 与 exit_code<>0 不重复计（status 已覆盖）
            } else if t.exit_code.map(|c| c != 0).unwrap_or(false) {
                d.counters.error_tool += 1;
            }
            // **偏差申报 D-16-3**：工具的 `cancelled_by_user` **不计入** `interrupted`。
            // 任务的「用户主动打断」以**回合**为单位：同一回合里被取消的工具与回合的
            // cancelled 是**同一次**打断（本机 9/9 条工具取消行都落在已计的 64 个 cancelled
            // 回合内）→ 再加一次是重复计（矩阵口径 = model_usage 53 + turn_usage 64 = 117，
            // 计划原文的三处累加会得 126）。任务书自己的用例也钉死「一次打断只计一次」。
        }
        // 5) 游标：SQLite 源用 ordinal 记 max rowid（**三张表各一条**——B4：
        //    旧实现只给 model_usage 落游标，turn_usage / tool_usage 每轮全量重算）。
        //    `state_json`（私有计数）统一挂在 `model_usage` 这条**主**游标上，下一轮从它读回。
        let state_json = serde_json::to_string(&state).unwrap_or_default();
        for (key, ordinal) in [
            ("model_usage", max_rowid),
            ("turn_usage", turn_max_rowid),
            ("tool_usage", tool_max_rowid),
        ] {
            b.push_cursor(CursorDelta {
                session_id: key.into(),
                ordinal,
                last_cumulative: None,
                state_json: if key == "model_usage" {
                    state_json.clone()
                } else {
                    String::new()
                },
                ..Default::default()
            });
        }
        Ok(b.finish())
    }
}

/// 记录级项目键反查（turn/tool 行只带 session_id）：查不到落 `unknown` 桶——
/// 不得留空串（空串会与「未归属」混在一起；D21 规定无 cwd 落 Unknown，W-01 规定键是 `"unknown"`）
fn pk_of(pk_by_session: &HashMap<String, String>, session_id: &str) -> String {
    pk_by_session
        .get(session_id)
        .cloned()
        .unwrap_or_else(|| project_key_of(None))
}

/// `started_at` 缺失（NULL）**不得**伪装成一个合法时间戳 0（D-23 尾部同族 / 裁决 D#5）：
/// 用**本轮采集时刻**兜底并留痕；真的是 0 照 0 走。
/// 真实 DDL 这三列都是 `not null`（本机 `sqlite_master` 复核），所以这条是私有格式漂移的
/// 纵深防御，真机上不会刷屏。
fn started_at_or_now(started_at: Option<i64>, now_ms: i64, table: &str) -> i64 {
    match started_at {
        Some(t) => t,
        None => {
            log::warn!(
                "usage/zcode: `{table}` 有一行的 started_at 为 NULL → 本轮按采集时刻 {now_ms} 归属\
                 （不伪装成 0 / 1970）"
            );
            now_ms
        }
    }
}

/// **水位边界（裁决 B）**：返回「必须停在其**之前**」的 rowid；`None` = 没有在飞行行，可读到尾。
///
/// 为什么必须停：这些行是**插入后原地更新**的（证据文档 §3.2.2 `:382-388`：`turn_usage.tool_call_count`
/// 与 `tool_usage` 按 `(session_id, turn_id)` 计数**零不一致**——即收尾是 UPDATE 同一行），
/// 而在飞行的行 `duration_ms` 还是 NULL。水位一旦跨过它，这一行**此后再不会回看** →
/// 该 turn 的 `turn_ms` / `error_turn` / `interrupted`（或该 tool 的 `tool_ms` / `error_tool`）**永久缺失**。
/// 真机 turn 最长 >1 h、tool 最长 ≈83 min → 最可能被抓在飞行的正是**最有价值的样本**。
/// 只消费**连续终态前缀**（已读到的终态行照常入账；阻塞行及其后面的行留到下一轮）。
///
/// **僵尸安全阀（控制窗口追加要求）**：阻塞行若老到不可能还在跑（`now_ms - started_at` ≥
/// `ZOMBIE_RUNNING_MS`，见该常量）就**强制跨过**它（返回 `None`：该行照常被读走、照常计数，
/// 只是没有时长样本）+ `log::warn!` + 计数，否则一条崩溃留下的 `running` 行会把水位**永久冻死**。
/// `started_at` 缺失时无法证明它还在跑 → 同样按僵尸处理。
fn inflight_boundary(
    conn: &Connection,
    table: &str,
    after: i64,
    now_ms: i64,
    state: &mut ZcodeCursorState,
) -> Result<Option<i64>, UsageError> {
    // 表名只来自本文件的常量（无注入面，与 opencode 的 `load_sessions(conn, table)` 同款）
    let blocked: Option<i64> = conn
        .query_row(
            &format!("SELECT MIN(rowid) FROM {table} WHERE rowid > ?1 AND status='running'"),
            [after],
            |r| r.get::<_, Option<i64>>(0),
        )
        .map_err(|e| {
            UsageError::new(
                "usage-source-io",
                format!("zcode {table} query failed (in-flight row probe; schema mismatch): {e}"),
            )
        })?;
    let Some(rowid) = blocked else {
        return Ok(None);
    };
    let started: Option<i64> = conn
        .query_row(
            &format!("SELECT started_at FROM {table} WHERE rowid = ?1"),
            [rowid],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| {
            UsageError::new(
                "usage-source-io",
                format!("zcode {table} read failed (in-flight row timestamps): {e}"),
            )
        })?
        .flatten();
    let age_ms = now_ms.saturating_sub(started.unwrap_or(0));
    if started.is_none() || age_ms >= ZOMBIE_RUNNING_MS {
        state.zombie_running += 1;
        log::warn!(
            "usage/zcode: `{table}` rowid={rowid} 仍在 running 且 started_at={started:?}\
             （已过 {age_ms} ms ≥ {ZOMBIE_RUNNING_MS} ms）→ 判为僵尸行，**强制跨过**水位\
             （该行照常入账但没有时长样本；累计 {} 行）",
            state.zombie_running
        );
        Ok(None)
    } else {
        Ok(Some(rowid))
    }
}

/// 会话「当前模型 / 供应商」的**原始对**（裁决 C）：`model_provider_by_session` 只在本轮
/// `model_usage` 循环里填 —— 某轮「有新的 turn/tool 行、却没有新的 model_usage 行」时
/// （例：分钟级才收尾的长工具 `Agent` / `TaskOutput`），明细行会落 `model=""` / `provider=""`
/// （D-32 家族：明细表主键含 model/provider，空串在 UI 上是一条**无名行**，且与「未归属」混在一起）。
/// 兜底 = 回查该会话**最后一条带模型的** `model_usage`（= 会话当前模型；`model_usage_session_turn_idx`
/// 以 `session_id` 打头，代价可控），并把结果（含**否定**结果）缓存，避免同一会话逐行回查。
/// 返回 `None` = 该会话确实没有模型来源 → 调用方**跳过该行**（绝不写空模型行）。
fn session_raw_model_provider(
    conn: &Connection,
    cache: &mut HashMap<String, Option<(String, String)>>,
    session_id: &str,
) -> Result<Option<(String, String)>, UsageError> {
    if let Some(hit) = cache.get(session_id) {
        return Ok(hit.clone());
    }
    let row = conn
        .query_row(
            "SELECT COALESCE(model_id,''), COALESCE(provider_id,'') FROM model_usage
             WHERE session_id = ?1 AND COALESCE(model_id,'') <> '' ORDER BY rowid DESC LIMIT 1",
            [session_id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        )
        .optional()
        .map_err(|e| {
            UsageError::new(
                "usage-source-io",
                format!("zcode model_usage session model lookup failed ({session_id}): {e}"),
            )
        })?;
    cache.insert(session_id.to_string(), row.clone());
    Ok(row)
}

/// **行级读取失败 = 整源 `Err`**（D-38 同族，控制窗口已批准「更严的取舍」）。
/// 计划原文四个 loader 都写「字段类型不符即跳过该行」（`rows.flatten()`）——那是**静默少算**：
/// 该行永久缺失，而 `UsageSourceStatus.ok` 仍是 `true`、UI 上没有任何信号（GC 7 / W-06 补充条款）。
/// 整源 `Err` 的语义是「本轮整体作废、下轮重试」（游标不推进、账本不写），与 D-16/D-24 一致。
///
/// **错误文案带「表名 + 坏行数」**（控制窗口 fix round 1 追加要求）：一次把该表**所有**坏行数出来
/// （`rusqlite` 的 `MappedRows` 在映射失败后仍可继续 step），并附首行错误原文——否则 UI / Task 24
/// 验收面只看到「`usage-source-io`」，不知道坏在哪张表、坏了多少行（W-28 同族）。
fn rows_err(table: &str, bad: usize, first: Option<rusqlite::Error>) -> UsageError {
    let detail = match first {
        Some(e) => format!("zcode {table} {bad} 行读取失败（字段类型不符 / 库损坏）: {e}"),
        None => format!("zcode {table} {bad} 行读取失败（字段类型不符 / 库损坏）"),
    };
    UsageError::new("usage-source-io", detail)
}

/// `session` 行（四段 SQL 之一）：`directory` 是项目归属的唯一来源（`project_id` 是有损 slug）。
/// **刻意不 select `model`**：zcode 的会话当前模型在
/// `session_entry(type='runtime/model_selection').data.{providerId,modelId}` 里，
/// **不在 `session` 行上**（证据文档 §4.1/§5.1）——写成 `session.model` 会让 `prepare()` 失败。
/// **失败即 `Err`**（与另外三段 SQL 同款，§3.2.2 阻塞 3）：旧实现这里只 `warn` + 返回空表，
/// 结果是「会话维度、项目反查表、模型反查表全空，而 `status.ok = true`」——
/// 比响亮失败危险得多（用户看不到任何异常，只看到 0）。
///
/// `time_updated` 是 `Option`（见调用点：`NULL` 不得伪装成 0）；`parent_id` / `task_type` 见
/// `SessionRow` 的字段注释（裁决 A）。
struct SessionRow {
    id: String,
    directory: String,
    /// `None` = 缺字段（不是「真的是 0」）
    time_updated: Option<i64>,
    /// **只用于判据分歧留痕**（不改任何分支）：真机「`parent_id` 非空」与「`task_type != 'interactive'`」
    /// 完全等价（594 = 586 + 7 + 1），私有 schema 漂移时留一条 `log::warn!`。
    task_type: String,
    /// **子会话判据的唯一来源**（裁决 A）：非空 = 本会话是子代理会话。
    /// 真机 71 父 / 594 子；`monitor/zcode_parser.rs:306-310` 同款（「`parent_id` 语义更稳定」）。
    parent_id: Option<String>,
}

fn load_sessions(conn: &Connection) -> Result<Vec<SessionRow>, UsageError> {
    let sql = "SELECT id, directory, time_updated, task_type, parent_id FROM session";
    let mut stmt = conn.prepare(sql).map_err(|e| {
        UsageError::new(
            "usage-source-io",
            format!("zcode session query failed (schema mismatch): {e}"),
        )
    })?;
    let rows = stmt
        .query_map([], |r| {
            Ok(SessionRow {
                id: r.get::<_, String>(0)?,
                directory: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                time_updated: r.get::<_, Option<i64>>(2)?,
                task_type: r.get::<_, Option<String>>(3)?.unwrap_or_default(),
                parent_id: r
                    .get::<_, Option<String>>(4)?
                    .filter(|p| !p.trim().is_empty()),
            })
        })
        .map_err(|e| UsageError::new("usage-source-io", format!("zcode session read: {e}")))?;
    let mut out = Vec::new();
    let mut bad = 0usize;
    let mut first: Option<rusqlite::Error> = None;
    for row in rows {
        match row {
            Ok(r) => out.push(r),
            Err(e) => {
                bad += 1;
                if first.is_none() {
                    first = Some(e);
                }
            }
        }
    }
    if bad > 0 {
        return Err(rows_err("session", bad, first));
    }
    Ok(out)
}

/// model_usage 行（逐请求四桶 + 状态）。`rowid` 是增量水位（ordinal）。
struct ModelUsageRow {
    rowid: i64,
    session_id: String,
    provider_id: String,
    model_id: String,
    input: i64,
    output: i64,
    reasoning: i64,
    cache_creation: i64,
    cache_read: i64,
    computed_total: i64,
    status: String,
    /// `None` = 缺字段（不得伪装成 0 / 1970 桶，裁决 D#5）
    started_at: Option<i64>,
}

/// 真实列名（**逐字**，证据文档 §1.2 + 本机 `sqlite_master` 复核）：`cache_creation_input_tokens`
/// / `cache_read_input_tokens` / `computed_total_tokens` / `provider_id` / `model_id` / `started_at`。
/// `prepare()` 失败 = 列名或表结构不符 → **响亮报错**（不得静默返回空集）。
///
/// `until` = 在飞行行的 rowid（裁决 B：**只消费连续终态前缀**，绝不越过它）；
/// `None` → 读到表尾。
fn load_model_usage(
    conn: &Connection,
    after_rowid: i64,
    until: Option<i64>,
) -> Result<Vec<ModelUsageRow>, UsageError> {
    let sql = "SELECT rowid, session_id, COALESCE(provider_id,''), COALESCE(model_id,''),
                      COALESCE(input_tokens,0), COALESCE(output_tokens,0), COALESCE(reasoning_tokens,0),
                      COALESCE(cache_creation_input_tokens,0), COALESCE(cache_read_input_tokens,0),
                      COALESCE(computed_total_tokens,0), COALESCE(status,''), started_at
               FROM model_usage WHERE rowid > ?1 AND rowid < ?2 ORDER BY rowid";
    let mut stmt = conn.prepare(sql).map_err(|e| {
        UsageError::new(
            "usage-source-io",
            format!("zcode model_usage query failed (schema mismatch): {e}"),
        )
    })?;
    let rows = stmt
        .query_map(
            rusqlite::params![after_rowid, until.unwrap_or(i64::MAX)],
            |r| {
                Ok(ModelUsageRow {
                    rowid: r.get(0)?,
                    session_id: r.get(1)?,
                    provider_id: r.get(2)?,
                    model_id: r.get(3)?,
                    input: r.get(4)?,
                    output: r.get(5)?,
                    reasoning: r.get(6)?,
                    cache_creation: r.get(7)?,
                    cache_read: r.get(8)?,
                    computed_total: r.get(9)?,
                    status: r.get(10)?,
                    started_at: r.get(11)?,
                })
            },
        )
        .map_err(|e| {
            UsageError::new(
                "usage-source-io",
                format!("zcode model_usage read failed: {e}"),
            )
        })?;
    let mut out = Vec::new();
    let mut bad = 0usize;
    let mut first: Option<rusqlite::Error> = None;
    for row in rows {
        match row {
            Ok(r) => out.push(r),
            Err(e) => {
                bad += 1;
                if first.is_none() {
                    first = Some(e);
                }
            }
        }
    }
    if bad > 0 {
        return Err(rows_err("model_usage", bad, first));
    }
    Ok(out)
}

/// turn_usage 行（**权威回合口径**）。真实列（证据文档 §1.2 + 本机复核）：`duration_ms` /
/// `cancelled_by_user` / `error_type` / 四个 token 列 / `primary key(session_id, turn_id)`。
struct TurnUsageRow {
    rowid: i64,
    session_id: String,
    status: String,
    duration_ms: Option<i64>,
    /// `None` = 缺字段（不得伪装成 0 / 1970 桶，裁决 D#5）
    started_at: Option<i64>,
}

/// `after_rowid` = 上一轮的 max rowid（**B4**：turn_usage 同样必须按 rowid 增量，
/// 否则每轮把全部 turn 再加一遍 → `turns` / `turn_ms` / `error_turn` / `interrupted` 线性膨胀）；
/// `until` = 在飞行行 rowid（裁决 B：不越过它）。
fn load_turn_usage(
    conn: &Connection,
    after_rowid: i64,
    until: Option<i64>,
) -> Result<Vec<TurnUsageRow>, UsageError> {
    let sql = "SELECT rowid, session_id, COALESCE(status,''), duration_ms, started_at
               FROM turn_usage WHERE rowid > ?1 AND rowid < ?2 ORDER BY rowid";
    let mut stmt = conn.prepare(sql).map_err(|e| {
        UsageError::new(
            "usage-source-io",
            format!("zcode turn_usage query failed (schema mismatch): {e}"),
        )
    })?;
    let rows = stmt
        .query_map(
            rusqlite::params![after_rowid, until.unwrap_or(i64::MAX)],
            |r| {
                Ok(TurnUsageRow {
                    rowid: r.get(0)?,
                    session_id: r.get(1)?,
                    status: r.get(2)?,
                    duration_ms: r.get(3)?,
                    started_at: r.get(4)?,
                })
            },
        )
        .map_err(|e| UsageError::new("usage-source-io", format!("zcode turn_usage read: {e}")))?;
    let mut out = Vec::new();
    let mut bad = 0usize;
    let mut first: Option<rusqlite::Error> = None;
    for row in rows {
        match row {
            Ok(r) => out.push(r),
            Err(e) => {
                bad += 1;
                if first.is_none() {
                    first = Some(e);
                }
            }
        }
    }
    if bad > 0 {
        return Err(rows_err("turn_usage", bad, first));
    }
    Ok(out)
}

/// tool_usage 行（**唯一免配对的原生耗时源**，非空率 100%）。真实列（证据文档 §2.1 + 本机复核）：
/// `tool_name`（**不是 `name`**）/ `duration_ms` / `exit_code` / `status` / `started_at`。
/// `cancelled_by_user` **不 select**：它不进 `interrupted`（见 collect 第 4 段的偏差申报），
/// 留着会是一个 `dead_code` 字段（本计划门禁 `-D warnings` 下即 error）。
struct ToolUsageRow {
    rowid: i64,
    session_id: String,
    name: String,
    status: String,
    duration_ms: Option<i64>,
    exit_code: Option<i64>,
    /// `None` = 缺字段（不得伪装成 0 / 1970 桶，裁决 D#5）
    started_at: Option<i64>,
}

/// `until` = 在飞行行 rowid（裁决 B：不越过它）。
fn load_tool_usage(
    conn: &Connection,
    after_rowid: i64,
    until: Option<i64>,
) -> Result<Vec<ToolUsageRow>, UsageError> {
    let sql = "SELECT rowid, session_id, tool_name, COALESCE(status,''), duration_ms, exit_code,
                      started_at
               FROM tool_usage WHERE rowid > ?1 AND rowid < ?2 ORDER BY rowid";
    let mut stmt = conn.prepare(sql).map_err(|e| {
        UsageError::new(
            "usage-source-io",
            format!("zcode tool_usage query failed (schema mismatch): {e}"),
        )
    })?;
    let rows = stmt
        .query_map(
            rusqlite::params![after_rowid, until.unwrap_or(i64::MAX)],
            |r| {
                Ok(ToolUsageRow {
                    rowid: r.get(0)?,
                    session_id: r.get(1)?,
                    name: r.get(2)?,
                    status: r.get(3)?,
                    duration_ms: r.get(4)?,
                    exit_code: r.get(5)?,
                    started_at: r.get(6)?,
                })
            },
        )
        .map_err(|e| UsageError::new("usage-source-io", format!("zcode tool_usage read: {e}")))?;
    let mut out = Vec::new();
    let mut bad = 0usize;
    let mut first: Option<rusqlite::Error> = None;
    for row in rows {
        match row {
            Ok(r) => out.push(r),
            Err(e) => {
                bad += 1;
                if first.is_none() {
                    first = Some(e);
                }
            }
        }
    }
    if bad > 0 {
        return Err(rows_err("tool_usage", bad, first));
    }
    Ok(out)
}

// ==== usage 自省锁：以下为排除区（测试代码），勿删勿复制 ====
#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::usage::collect::{CollectContext, UsageCollector};
    use rusqlite::Connection;
    use std::collections::HashMap;

    /// 真实形态的 zcode 库（四张表的列名**逐字**对齐 `load_*` 的 SQL）。
    ///
    /// **列名纪律（C2）**：`tool_usage` 必须带 `started_at`——`load_tool_usage` 的 SQL 里有
    /// `COALESCE(started_at,0)`，少这一列 `prepare()` 直接失败 → 整个源 `Err`（不是静默 0）。
    /// 旧 fixture 漏了这一列，于是本任务头两条用例会在 `.unwrap()` 上炸（独立验证复核 §3.2.2
    /// 阻塞 3 的同族问题：fixture 与 SQL 脱节）。证据：`research/可得性探测-SQLite与投影四源.md`
    /// 的 `tool_usage` 建表语句（`started_at integer not null`）。
    ///
    /// 时间戳用 1970 附近的小整数：本任务所有用例都**不走 `purge_expired`**（保留期清理只在
    /// `run_collection` 末尾跑），所以这里的数值只影响小时桶、不影响断言；
    /// 跨源两轮用例（Task 23B）的 fixture 才必须与 `T0` 同代。
    fn fixture(db: &std::path::Path) {
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        let conn = Connection::open(db).unwrap();
        conn.execute_batch(
            "CREATE TABLE session (id TEXT PRIMARY KEY, directory TEXT, task_type TEXT, parent_id TEXT,
                 time_updated INTEGER);
             CREATE TABLE model_usage (session_id TEXT, turn_id TEXT, trace_id TEXT, query_source TEXT,
                 status TEXT, provider_id TEXT, model_id TEXT, variant TEXT,
                 input_tokens INTEGER, output_tokens INTEGER, reasoning_tokens INTEGER,
                 cache_creation_input_tokens INTEGER, cache_read_input_tokens INTEGER,
                 computed_total_tokens INTEGER, provider_total_tokens INTEGER,
                 started_at INTEGER, completed_at INTEGER, tool_call_count INTEGER);
             CREATE TABLE turn_usage (session_id TEXT, turn_id TEXT, status TEXT, started_at INTEGER,
                 duration_ms INTEGER, input_tokens INTEGER, output_tokens INTEGER, reasoning_tokens INTEGER,
                 cache_creation_input_tokens INTEGER, cache_read_input_tokens INTEGER,
                 computed_total_tokens INTEGER, tool_call_count INTEGER, cancelled_by_user INTEGER,
                 error_type TEXT, PRIMARY KEY (session_id, turn_id));
             CREATE TABLE tool_usage (id TEXT PRIMARY KEY, session_id TEXT, turn_id TEXT, tool_name TEXT,
                 status TEXT, duration_ms INTEGER, exit_code INTEGER, cancelled_by_user INTEGER,
                 started_at INTEGER NOT NULL);",
        ).unwrap();
        // 会话：**1 父 + 1 子**（真机 71 父 / 594 子）——裁决 A 的判据与 `parent_session_id`
        // 在父子各一的夹具上就能锁住（「全父」退化会让 `!is_subagent` 计数从 1 变 2）。
        conn.execute(
            "INSERT INTO session (id, directory, task_type, parent_id, time_updated)
             VALUES ('sess_a','/p/A','interactive',NULL,1000),
                    ('sess_noturn','/p/B','subagent_child','sess_a',900)",
            [],
        )
        .unwrap();
        // 同一 turn 两条 model_usage（多请求）——四桶逐请求累加；computed_total = input + output → Subset
        for (tid, input, out, read) in [("turn_1", 1000i64, 100i64, 800i64), ("turn_1", 200, 20, 0)]
        {
            conn.execute("INSERT INTO model_usage (session_id, turn_id, query_source, status, provider_id,
                 model_id, input_tokens, output_tokens, reasoning_tokens, cache_creation_input_tokens,
                 cache_read_input_tokens, computed_total_tokens, started_at, completed_at, tool_call_count)
                 VALUES ('sess_a', ?1, 'main', 'completed', 'bigmodel', 'GLM-5.3', ?2, ?3, 0, 0, ?4, ?5, 1000, 2000, 0)",
                rusqlite::params![tid, input, out, read, input + out]).unwrap();
        }
        // 权威 turn 行（1 条）+ 原生 duration
        conn.execute("INSERT INTO turn_usage (session_id, turn_id, status, started_at, duration_ms,
             input_tokens, output_tokens, reasoning_tokens, cache_creation_input_tokens,
             cache_read_input_tokens, computed_total_tokens, tool_call_count, cancelled_by_user, error_type)
             VALUES ('sess_a','turn_1','completed',1000,310607,1200,120,0,0,800,1320,2,0,NULL)", []).unwrap();
        conn.execute("INSERT INTO turn_usage (session_id, turn_id, status, started_at, duration_ms,
             input_tokens, output_tokens, reasoning_tokens, cache_creation_input_tokens,
             cache_read_input_tokens, computed_total_tokens, tool_call_count, cancelled_by_user, error_type)
             VALUES ('sess_a','turn_2','cancelled',5000,60000,10,1,0,0,0,11,0,1,'cancelled')", []).unwrap();
        // 工具：原生 duration_ms（非空率 100%）+ error 343 类 + exit_code<>0
        conn.execute("INSERT INTO tool_usage VALUES ('t1','sess_a','turn_1','Bash','completed',293,0,0,1000)", []).unwrap();
        conn.execute(
            "INSERT INTO tool_usage VALUES ('t2','sess_a','turn_1','Edit','error',700,1,0,2000)",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO tool_usage VALUES ('t3','sess_a','turn_2','Bash','cancelled',50,NULL,1,5000)", []).unwrap();
    }

    #[test]
    fn turns_come_from_turn_usage_and_sessions_without_turns_are_zero() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join(".zcode/cli/db/db.sqlite");
        fixture(&db);
        // 空游标表必须先绑成变量：`&HashMap::new()` 是**临时值**，借用进 `ctx` 就是
        // E0716（temporary value dropped while borrowed）——语句结束即释放。
        let no_cursors = HashMap::new();
        let ctx = CollectContext::new(dir.path(), 1_700_000_000_000, &no_cursors);
        let delta = ZCodeCollector::collect_with_roots(
            &ctx,
            &crate::monitor::zcode_parser::ZcodeRoots::from_home(dir.path()),
        )
        .unwrap();
        // turn 数 = turn_usage 行数（2），**不是** distinct model_usage.turn_id（会多算幽灵 turn）
        let turns: i64 = delta.details.iter().map(|d| d.counters.turns).sum();
        assert_eq!(turns, 2);
        // 118/665 无 turn 行的会话必须左连补 0（不丢会话）
        assert!(delta.sessions.iter().any(|s| s.session_id == "sess_noturn"));
        // 四桶逐请求累加 + Subset 判据（computed_total == input + output）
        let a = delta
            .details
            .iter()
            .find(|d| d.key.session_id == "sess_a")
            .unwrap();
        // **期望值修正 ②（偏差申报 D-16-2）**：明细行键不含语义，而本 fixture 的两条请求
        // **语义不同**（第一条 Subset；第二条 `cache_read == 0` → 互斥式与包含式同真 →
        // 按规则 2 落 `Exclusive`）→ 本行的标签只能是近似。本实现按任务书代码块逐字落地
        // 「后写者胜」（= claude / codex / workbuddy 三源同一模式），故标签 = **最后一条**请求
        // 的判定 = `Exclusive`。真机同一明细行内语义混存有 **216 行**（报告实机冒烟），
        // 该列在 `database/schema.rs:215` 注明「诊断用、不参与聚合」。任务书原文期望 `Subset`
        // 与它自己的代码块互相矛盾，见报告「偏差申报」。
        assert!(matches!(
            a.cache_semantics,
            crate::services::usage::semantics::CacheSemantics::Exclusive
        ));
        // **期望值修正 ①（偏差申报 D-16-1，同 D-17 先例：期望值是笔误、判据一字未改）**：
        // 任务书原文写 `200 + 0`，但**同一条断言的说明文字**写的是「(1000-800) + 200」= 400。
        // 第一条（input=1000 / cache_read=800 / total=1100 → Subset）未缓存输入 = 200；
        // 第二条（input=200 / cache_read=0 / total=220 → 并列落互斥）未缓存输入 = 200 → 合计 400。
        // 任何实现都得出 400（得出 200 只能是丢行或错判语义）。
        assert_eq!(
            a.buckets.input_fresh,
            (1000 - 800) + 200,
            "第一条 Subset：(1000-800)=200；第二条 cached=0 落互斥：input 原文 200"
        );
        assert_eq!(a.buckets.cache_read, 800);
        // Subset 判据的另一条**更强**的锁（期望值修正 ① 的补偿）：请求输入 = Subset 的
        // input 原文 1000 + 互斥的 200。若第一条被按互斥判 → 1900 + 200 = 2100 → 必红。
        assert_eq!(
            a.request_total,
            1000 + 200,
            "Subset 的请求输入 = input 原文；若第一条按互斥判会得 1900+200"
        );
        assert_eq!(a.key.provider, "bigmodel");
        assert_eq!(a.key.model, "GLM-5.3");
        // **GC 6 / W-09**：供应商一律经 `DeltaBuilder::provider_of` 记录三态——zcode 有直接字段
        // `provider_id` → `Measured`。若有人绕过唯一入口自己拼 key（或改调
        // `provider::resolve_provider` 而漏记三态），`provider_kind` 会退化成 `Unknown`
        // （UI 三态标记全丢）→ 本断言红。
        assert_eq!(
            a.provider_kind,
            crate::services::usage::model::SourceKind::Measured
        );
        // 原生工具耗时（唯一免配对的源）
        let ms: i64 = delta.details.iter().map(|d| d.counters.tool_ms).sum();
        assert_eq!(ms, 1043, "293 + 700 + 50");
        let calls: i64 = delta.details.iter().map(|d| d.counters.tool_calls).sum();
        assert_eq!(calls, 3);
        // 报错三层 + 打断单列
        let e_tool: i64 = delta.details.iter().map(|d| d.counters.error_tool).sum();
        assert_eq!(
            e_tool, 1,
            "tool_usage.status='error'（exit_code 也非 0，但不得重复计）"
        );
        let intr: i64 = delta.details.iter().map(|d| d.counters.interrupted).sum();
        assert_eq!(
            intr, 1,
            "一次用户打断只计一次：turn_2（回合层 cancelled）已计；同一回合内的工具 t3 \
             cancelled_by_user=1 **不得**再加一次——真机 9/9 条工具 cancelled_by_user 全部落在\
             已计的 cancelled 回合里（加了就是 53+64+9=126 的重复计；矩阵口径 = model_usage 53 + \
             turn_usage 64 = 117）。见报告「偏差申报 D-16-3」"
        );
        // 最长单 turn 样本（duration_ms 原生，含挂机，D19 只逐工具给 p50+max）
        assert_eq!(a.counters.turn_ms, vec![310_607, 60_000]);
        // D21：项目取 session.directory（**禁用 project_id**）
        assert_eq!(
            delta
                .sessions
                .iter()
                .find(|s| s.session_id == "sess_a")
                .unwrap()
                .project_key,
            "a"
        );
        // **W-50（控制窗口裁定）**：zcode 是 SQLite 源 → `parsed_files` 记「**1** 个库文件」，
        // 与 opencode 同口径；计划 L10172 的「逐会话 `parsed_files += 1`」会让同一个 UI 字段
        // 出现两种语义（本夹具 2 个会话 → 会显示 2）。本断言能区分「1」与「会话数」两种退化。
        assert_eq!(
            delta.parsed_files, 1,
            "SQLite 源 = 1 个库文件（W-50），不是会话行数（本夹具 2）"
        );
        // 四段 SQL 至少取到了行（**C2：防"列名写错 → 静默出零"**）：
        // 三个 loader 都返回 > 0 行，说明真实列名（tool_name / cancelled_by_user /
        // computed_total_tokens …）与 SQL 一致；任一为空即说明 SQL 与库结构脱节。
        assert!(
            delta.new_records >= 3,
            "model_usage + turn_usage + tool_usage 都应取到行"
        );
        assert_eq!(
            delta.new_records, 7,
            "2 条 model_usage + 2 条 turn_usage + 3 条 tool_usage"
        );
    }

    /// **B4 回归**：`turn_usage` / `tool_usage` 也必须按 rowid 水位增量。
    /// 旧实现只给 `model_usage` 落游标（`turn_usage` 无游标读取、`tool_usage` 读游标却从不写）
    /// → 第二轮把全部 turn / 工具行再加一遍，`turns` / `turn_ms` / `error_*` / `interrupted` /
    /// `tool_calls` / `tool_ms` 随采集轮次线性膨胀。
    #[test]
    fn second_round_is_incremental_for_turn_and_tool_tables() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join(".zcode/cli/db/db.sqlite");
        fixture(&db);
        let roots = crate::monitor::zcode_parser::ZcodeRoots::from_home(dir.path());

        // 空游标表必须先绑成变量：`&HashMap::new()` 是**临时值**，借用进 `ctx` 就是
        // E0716（temporary value dropped while borrowed）——语句结束即释放。
        let no_cursors = HashMap::new();
        let ctx1 = CollectContext::new(dir.path(), 1_700_000_000_000, &no_cursors);
        let d1 = ZCodeCollector::collect_with_roots(&ctx1, &roots).unwrap();
        assert!(
            d1.cursors.iter().any(|c| c.session_id == "turn_usage")
                && d1.cursors.iter().any(|c| c.session_id == "tool_usage")
                && d1.cursors.iter().any(|c| c.session_id == "model_usage"),
            "三张表都必须落游标（旧实现只有 model_usage）"
        );
        let cursors: HashMap<_, _> = d1
            .cursors
            .iter()
            .map(|c| (c.session_id.clone(), c.clone()))
            .collect();

        // 第二轮：同一份数据 → 三个维度都不得有新增
        let ctx2 = CollectContext::new(dir.path(), 1_700_000_000_000 + 60_000, &cursors);
        let d2 = ZCodeCollector::collect_with_roots(&ctx2, &roots).unwrap();
        assert_eq!(d2.new_records, 0, "第二轮不得有新增行（旧实现会全量重算）");
        assert_eq!(
            d2.details.iter().map(|d| d.counters.turns).sum::<i64>(),
            0,
            "turn 不得重复计"
        );
        assert_eq!(
            d2.details
                .iter()
                .map(|d| d.counters.tool_calls)
                .sum::<i64>(),
            0,
            "工具不得重复计"
        );
        assert_eq!(
            d2.details.iter().map(|d| d.counters.tool_ms).sum::<i64>(),
            0,
            "工具耗时不得重复计"
        );
        assert_eq!(
            d2.details.iter().map(|d| d.requests).sum::<i64>(),
            0,
            "请求不得重复计"
        );
    }

    /// **C2 回归 · 每张表一条**：列名写错必须**响亮失败**，不得静默出零。
    ///
    /// 造库方式：先用 `fixture()` 建**全合法**的四张表（列名与 `load_*` 的 SQL 逐字一致），
    /// 再用 `ALTER TABLE … RENAME COLUMN` **只把目标表的一列改成旧散文里的错名**——
    /// 于是"谁先炸"不再取决于采集顺序：**唯一**可能失败的 `prepare()` 就是目标表那条
    /// （独立验证复核 §3.2.2 阻塞 3：旧版一次性毁了 `tool_usage` 的列名却不建 `turn_usage`，
    /// 而采集顺序是 model_usage → turn_usage → tool_usage → 第一个炸的是 `turn_usage`，
    /// 断言 `contains("tool_usage")` 必然失败）。
    ///
    /// 四条用例共用下面这个断言：`Err` + 错误码 + **报错信息点名目标表**（用
    /// `zcode <表> 查询失败` 这个完整前缀，避免"别的表先炸但消息里恰好含该词"混过去）。
    fn assert_broken_table_fails_loudly(broken: &str) {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join(".zcode/cli/db/db.sqlite");
        fixture(&db); // 四张表全部合法（含 tool_usage.started_at）
        let conn = Connection::open(&db).unwrap();
        let rename = match broken {
            // `session`：真实列是 directory / time_updated（旧散文写成 dir / updated_at）
            "session" => "ALTER TABLE session RENAME COLUMN directory TO dir;",
            // `model_usage`：真实列是 computed_total_tokens / cache_creation_input_tokens
            "model_usage" => {
                "ALTER TABLE model_usage RENAME COLUMN computed_total_tokens TO computed_total;"
            }
            // `turn_usage`：真实列是 duration_ms
            "turn_usage" => "ALTER TABLE turn_usage RENAME COLUMN duration_ms TO duration;",
            // `tool_usage`：真实列是 `tool_name`（旧散文写成 `name`）。**只改这一列**：
            // `cancelled_by_user` 已不在 loader 的 SQL 里（裁决 D-16-3：工具层的 cancelled 不进
            // `interrupted`），对它改名是**空动作**、不构成锁——fix round 1 / 评审 Minor #6。
            "tool_usage" => "ALTER TABLE tool_usage RENAME COLUMN tool_name TO name;",
            other => panic!("未知表名 {other}"),
        };
        conn.execute_batch(rename).unwrap();
        drop(conn);

        // 空游标表必须先绑成变量：`&HashMap::new()` 是**临时值**，借用进 `ctx` 就是
        // E0716（temporary value dropped while borrowed）——语句结束即释放。
        let no_cursors = HashMap::new();
        let ctx = CollectContext::new(dir.path(), 1_700_000_000_000, &no_cursors);
        let roots = crate::monitor::zcode_parser::ZcodeRoots::from_home(dir.path());
        let err = ZCodeCollector::collect_with_roots(&ctx, &roots)
            .expect_err(&format!("{broken} 列名不符必须返回 Err，而不是静默 0"));
        // **裁决 D#7**：库**开了**、只是读不动 → `usage-source-io`（`usage-source-db-open`
        // 只留给「打不开」）。这个码经 W-28 会出现在 Task 24 的验收面上，语义必须对。
        assert_eq!(err.code, "usage-source-io");
        assert!(
            err.detail.contains(&format!("zcode {broken} query failed")),
            "错误信息必须点名「{broken}」这张表的 prepare 失败: {}",
            err.detail
        );
    }

    /// 旧散文把 `tool_name` / `cancelled_by_user` 写成 `name` / `cancelled` —— 这正是
    /// C2 要防的形态（旧行为：`prepare()` 失败即 `return` → zcode 全源 0 用量而
    /// `status.ok = true`，错到人工验收才发现）。
    #[test]
    fn wrong_column_name_fails_loudly_instead_of_silently_reading_zero() {
        assert_broken_table_fails_loudly("tool_usage");
    }

    /// 同族第二例：`turn_usage.duration_ms`（旧散文写成 `duration`）。
    /// 采集顺序里它排在 `tool_usage` **之前**——只毁它一张表，才能确定"第一个炸的就是它"。
    #[test]
    fn wrong_turn_usage_column_name_fails_loudly() {
        assert_broken_table_fails_loudly("turn_usage");
    }

    /// 同族第三例：`model_usage.computed_total_tokens`（旧散文写成 `computed_total`）。
    /// 它是四段 SQL 里**第一条**执行的，任何"只毁 tool_usage"的构造都会先撞上它。
    #[test]
    fn wrong_model_usage_column_name_fails_loudly() {
        assert_broken_table_fails_loudly("model_usage");
    }

    /// 同族第四例：`session.directory`（旧散文写成 `dir`）。这条锁住"会话维度也不许静默降级"
    /// ——旧实现 `load_sessions` 的 `prepare()` 失败只 `log::warn` + 返回空集，
    /// 于是"会话全丢但 `status.ok = true`"（§3.2.2 阻塞 3 的后半条）。现在必须 `Err`。
    #[test]
    fn wrong_session_column_name_fails_loudly() {
        assert_broken_table_fails_loudly("session");
    }

    /// **表整张不存在**（不是列名不符）也必须响亮失败——与 C2 同族，但触发的是另一条
    /// `prepare()` 分支（`no such table`）。四段 SQL 里任意一张表缺失 → 整源 `Err`，
    /// 不得静默 0（否则「这个源没有用量」与「这个源读不了」不可区分，W-06 补充条款）。
    #[test]
    fn missing_table_fails_loudly_too() {
        for (table, drop_sql) in [
            ("session", "DROP TABLE session;"),
            ("model_usage", "DROP TABLE model_usage;"),
            ("turn_usage", "DROP TABLE turn_usage;"),
            ("tool_usage", "DROP TABLE tool_usage;"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let db = dir.path().join(".zcode/cli/db/db.sqlite");
            fixture(&db);
            Connection::open(&db)
                .unwrap()
                .execute_batch(drop_sql)
                .unwrap();
            let no_cursors = HashMap::new();
            let ctx = CollectContext::new(dir.path(), 1_700_000_000_000, &no_cursors);
            let err = ZCodeCollector::collect_with_roots(
                &ctx,
                &crate::monitor::zcode_parser::ZcodeRoots::from_home(dir.path()),
            )
            .expect_err(&format!("{table} 表缺失必须整源 Err，不得静默 0"));
            assert_eq!(
                err.code, "usage-source-io",
                "库开了但表没了 → 读失败码（D#7）"
            );
            assert!(
                err.detail.contains(&format!("zcode {table} query failed")),
                "错误信息必须点名缺失的表「{table}」: {}",
                err.detail
            );
        }
    }

    /// **行级错误不得静默丢行（D-38 同族，控制窗口已批准更严的取舍）**：
    /// 任务书四个 loader 都写「字段类型不符即跳过该行」（`rows.flatten()`）——那是
    /// **静默少算**（该行永久缺失、UI 上没有任何信号），与本分支反复确立的
    /// 「宁可响亮失败」相反。本实现把行级错误也变成整源 `Err`（游标不推进 → 下轮重试）。
    ///
    /// 每张表一条：用 `UPDATE … SET <列> = <错类型>` 造**行级**类型不符，并先做
    /// **前提断言**（`typeof()` 真的不是 SQL 侧期望的类型），否则用例会因为"构造根本没生效"
    /// 而假绿。
    fn assert_bad_row_type_fails_loudly(
        table: &str,
        col: &str,
        assign: &str,
        wanted_type: &str,
        expect_bad: usize,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join(".zcode/cli/db/db.sqlite");
        fixture(&db);
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(&format!("UPDATE {table} SET {assign};"))
            .unwrap();
        // 前提断言（防假绿）：该表**确实**存在类型不符的行。用 `count(*)` 而不是
        // `… LIMIT 1`：SQLite 索引序里 BLOB 排在 TEXT 之后，`LIMIT 1` 会取到另一行（假绿）。
        let bad: i64 = conn
            .query_row(
                &format!("SELECT count(*) FROM {table} WHERE typeof({col}) <> ?1"),
                [wanted_type],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            bad >= 1,
            "构造失效：{table}.{col} 没有任何一行的类型不是 {wanted_type}（本段测不到行级类型不符）"
        );
        drop(conn);

        let no_cursors = HashMap::new();
        let ctx = CollectContext::new(dir.path(), 1_700_000_000_000, &no_cursors);
        let err = ZCodeCollector::collect_with_roots(
            &ctx,
            &crate::monitor::zcode_parser::ZcodeRoots::from_home(dir.path()),
        )
        .expect_err(&format!(
            "{table} 行级类型不符必须整源 Err，不得静默丢行（D-38）"
        ));
        assert_eq!(
            err.code, "usage-source-io",
            "库开了但行读不动 → 读失败码（D#7）"
        );
        // **控制窗口 fix round 1 追加要求**：文案必须带「表名 + 坏行数」（否则 UI / Task 24 的
        // 验收面只知道「这个源读不了」，不知道坏在哪张表、坏了几行——W-28 同族）。
        assert!(
            err.detail.contains(table) && err.detail.contains(&format!("{expect_bad} 行读取失败")),
            "错误信息必须点名「{table}」并给出坏行数（{expect_bad}）: {}",
            err.detail
        );
    }

    #[test]
    fn row_level_type_mismatch_fails_loudly_per_table() {
        // `session.id` 是 TEXT 主键 → 塞 BLOB（TEXT 亲和性不会把它转成字符串）
        assert_bad_row_type_fails_loudly(
            "session",
            "id",
            "id = x'00' WHERE id = 'sess_a'",
            "text",
            1,
        );
        // 数值列塞非数字文本。**坏行数也被断言**（控制窗口 fix round 1 追加要求）：
        // sess_a 有两条 model_usage → 一次 UPDATE 破坏 2 行，采集器必须数出 **2**。
        assert_bad_row_type_fails_loudly(
            "model_usage",
            "input_tokens",
            "input_tokens = 'not-a-number' WHERE session_id = 'sess_a'",
            "integer",
            2,
        );
        assert_bad_row_type_fails_loudly(
            "turn_usage",
            "started_at",
            "started_at = 'not-a-number' WHERE turn_id = 'turn_1'",
            "integer",
            1,
        );
        assert_bad_row_type_fails_loudly(
            "tool_usage",
            "duration_ms",
            "duration_ms = 'not-a-number' WHERE id = 't1'",
            "integer",
            1,
        );
    }

    /// **边界：库文件不存在**（新工具未安装 / 库被删）。生产入口 `collect()` 必须
    /// 「不是错、也不读数」：`Ok(default)` + `parsed_files = 0` + 零明细（与 opencode 同款
    /// 早退）；而**直调注入版**（测试与跨源用例的入口）面对不存在的库必须**响亮失败**，
    /// 不得静默空集。桩不得只覆盖成功路径（执行提示词 §4.3）。
    #[test]
    fn missing_db_is_not_an_error_for_the_source_entry_but_is_loud_on_the_injected_path() {
        let dir = tempfile::tempdir().unwrap();
        let no_cursors = HashMap::new();
        let ctx = CollectContext::new(dir.path(), 1_700_000_000_000, &no_cursors);
        // ① 生产入口：`~/.zcode/cli/db/db.sqlite` 不存在 → 空集（不是错）
        let empty = ZCodeCollector.collect(&ctx).unwrap();
        assert!(empty.details.is_empty() && empty.sessions.is_empty() && empty.cursors.is_empty());
        assert_eq!(
            empty.parsed_files, 0,
            "没有库文件可读 → 0（不是 1：1 的含义是「读到了 1 个库文件」）"
        );
        // ② 直调注入版：库打不开 → 整源 Err（`usage-source-db-open`）
        let err = ZCodeCollector::collect_with_roots(
            &ctx,
            &crate::monitor::zcode_parser::ZcodeRoots::from_home(dir.path()),
        )
        .expect_err("不存在的库直调必须响亮失败");
        assert_eq!(err.code, "usage-source-db-open");
        // ③ 前提断言（防假绿）：同一 home 下把库建出来，同一条调用必须成功
        //    —— 证明 ② 的 Err 来自「打不开」，不是被某种无关早退吃掉的。
        let db = dir.path().join(".zcode/cli/db/db.sqlite");
        fixture(&db);
        let ctx2 = CollectContext::new(dir.path(), 1_700_000_000_000, &no_cursors);
        assert!(ZCodeCollector::collect_with_roots(
            &ctx2,
            &crate::monitor::zcode_parser::ZcodeRoots::from_home(dir.path())
        )
        .is_ok());
    }

    /// **边界：库路径本身是个目录**（`open_usage_db` 的 ro 与 immutable 两条路都失败）
    /// → 必须整源 `Err`。桩只覆盖成功路径 = 没测降级路径。
    #[test]
    fn db_path_is_a_directory_is_a_loud_source_error() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join(".zcode/cli/db/db.sqlite");
        std::fs::create_dir_all(&db).unwrap();
        let no_cursors = HashMap::new();
        let ctx = CollectContext::new(dir.path(), 1_700_000_000_000, &no_cursors);
        let err = ZCodeCollector::collect_with_roots(
            &ctx,
            &crate::monitor::zcode_parser::ZcodeRoots::from_home(dir.path()),
        )
        .expect_err("目录路径：ro 与 immutable 都打不开 → 整源 Err");
        assert_eq!(err.code, "usage-source-db-open");
        // 前提断言（防假绿）：把目录换成真库 → 同一条调用必须成功
        std::fs::remove_dir_all(&db).unwrap();
        fixture(&db);
        let ctx2 = CollectContext::new(dir.path(), 1_700_000_000_000, &no_cursors);
        assert!(ZCodeCollector::collect_with_roots(
            &ctx2,
            &crate::monitor::zcode_parser::ZcodeRoots::from_home(dir.path())
        )
        .is_ok());
    }

    /// **D-23 尾部同族**：`time_updated` **缺失**（NULL）不得被 `unwrap_or(0)` /
    /// `COALESCE(…,0)` 伪装成一个合法时间戳 0（那会让 `last_seen_at` 落 0 / 1970 桶，
    /// 与「真的是 0」不可区分）。缺字段 → 用**本轮采集时刻**兜底（并 `log::warn!` 留痕）；
    /// **真的是 0** 的会话照 0 走（不得顺带把事实改掉）。
    /// 任务书/计划这四段 SQL 里的 `COALESCE(time_updated,0)` 正是 opencode 已被修掉的那处
    /// 缺陷（见报告「偏差申报 D-16-4」）。
    #[test]
    fn null_time_updated_falls_back_to_now_while_a_real_zero_stays_zero() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join(".zcode/cli/db/db.sqlite");
        fixture(&db);
        Connection::open(&db)
            .unwrap()
            .execute_batch(
                "UPDATE session SET time_updated = NULL WHERE id = 'sess_noturn';
                 UPDATE session SET time_updated = 0    WHERE id = 'sess_a';",
            )
            .unwrap();
        let no_cursors = HashMap::new();
        let now = 1_700_000_000_000;
        let ctx = CollectContext::new(dir.path(), now, &no_cursors);
        let delta = ZCodeCollector::collect_with_roots(
            &ctx,
            &crate::monitor::zcode_parser::ZcodeRoots::from_home(dir.path()),
        )
        .unwrap();
        let noturn = delta
            .sessions
            .iter()
            .find(|s| s.session_id == "sess_noturn")
            .expect("前提：该会话必须照常登记（缺失时间戳不得丢会话）");
        assert_eq!(
            noturn.last_seen_at, now,
            "NULL 的 time_updated 必须用本轮采集时刻兜底，不得伪装成 0（1970 桶）"
        );
        let a = delta
            .sessions
            .iter()
            .find(|s| s.session_id == "sess_a")
            .unwrap();
        assert_eq!(
            a.last_seen_at, 0,
            "真的是 0 的会话照 0 走（不得被兜底逻辑顺带改掉事实）"
        );
    }

    /// **W-01 回归**：`Unknown` 桶的键是 **`"unknown"`**（`project_key_of(None)`），**不是 `""`**。
    /// 无 `directory` 的会话（真机 665 会话里 `directory` 是 `not null`，属纵深防御）以及
    /// 归属到它会话的 turn / tool 行都必须落 `"unknown"`——空串会与「未归属」混在一起，
    /// 且明细表主键里的空串会让 UI 分组出现无名行。这条同时锁住：
    /// ① 会话维度走 `record_project` / `project_key_of`（**不得**把 `directory` 原文当键）；
    /// ② 记录级反查 `pk_of` 的兜底同样是 `"unknown"`。
    #[test]
    fn session_without_directory_lands_in_the_unknown_bucket_not_empty_string() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join(".zcode/cli/db/db.sqlite");
        fixture(&db);
        Connection::open(&db)
            .unwrap()
            .execute_batch(
                "UPDATE session SET directory = NULL WHERE id = 'sess_noturn';
                 -- **裁决 C**：无模型来源的行会被跳过，所以先给该会话补一条 model_usage，
                 -- 本用例要测的是「记录级项目键」（W-01）而不是「空模型行」那一条。
                 INSERT INTO model_usage (session_id, turn_id, query_source, status, provider_id, model_id,
                     input_tokens, output_tokens, computed_total_tokens, started_at)
                 VALUES ('sess_noturn','turn_n','main','completed','bigmodel','GLM-5.3',10,1,11,900);
                 -- 把一条工具行挪到「无目录」的那个会话上：它的记录级项目键必须落 unknown
                 UPDATE tool_usage SET session_id = 'sess_noturn' WHERE id = 't3';",
            )
            .unwrap();
        let no_cursors = HashMap::new();
        let ctx = CollectContext::new(dir.path(), 1_700_000_000_000, &no_cursors);
        let delta = ZCodeCollector::collect_with_roots(
            &ctx,
            &crate::monitor::zcode_parser::ZcodeRoots::from_home(dir.path()),
        )
        .unwrap();
        let s = delta
            .sessions
            .iter()
            .find(|s| s.session_id == "sess_noturn")
            .expect("前提：无目录的会话本身不得被丢");
        assert_eq!(
            s.project_key, "unknown",
            "无 cwd 的会话键必须是 unknown（W-01）"
        );
        assert_eq!(s.project_path_raw, "", "无 cwd 时原文就是空串");
        // 该会话的工具行（记录级项目键走 pk_of 兜底）同样必须落 unknown
        let moved = delta
            .details
            .iter()
            .find(|d| d.counters.tool_calls == 1 && d.key.session_id == "sess_noturn")
            .expect("前提：被挪走的那条工具行必须自成一条明细");
        assert_eq!(moved.key.project_key, "unknown");
        // 全量：任何一条明细都不许出现空串项目键（那是「未归属」分裂成两组的形态）
        assert!(
            delta.details.iter().all(|d| !d.key.project_key.is_empty()),
            "明细行的 project_key 不得为空串"
        );
    }

    /// **登记锁（独立验证复核 §3.2.2 阻塞 4）**：漏登记不是编译错——`run_collection`
    /// 只遍历 `collectors::all()`，这个源会被静默跳过。
    #[test]
    fn collector_is_registered_in_all() {
        let ids: Vec<UsageSourceId> = crate::services::usage::collectors::all()
            .iter()
            .map(|c| c.source_id())
            .collect();
        assert!(
            ids.contains(&UsageSourceId::ZCode),
            "zcode 采集器必须登记进 collectors::all()，实际登记：{ids:?}"
        );
    }

    /// **裁决 A（Critical）回归**：`is_subagent = parent_id 非空`，并把 `parent_id` 传进
    /// `parent_session_id`。
    ///
    /// 真机 665 会话 = **71 父 + 594 子**（`parent_id` 非空 594 = `subagent_child` 586 +
    /// `fork` 7 + `selection_side_chat` 1，两种判据完全等价）。消费口径是 GC 18-R3 / D17：
    /// **会话数与 turn 数都只计父会话** —— 漏了这一步，「会话数」会是 665（应 71，9.4×）、
    /// 「turn 数」≈1207（应 605，2.0×）。
    ///
    /// 本夹具 = **1 父（`sess_a`）+ 1 子（`sess_noturn`）**：`!is_subagent` 的会话数必须是 **1**
    /// ——「全父」退化（旧实现恒 `false`）会得 2 → 红。
    /// 同时钉住 R3 的另一半：**子代理的用量照常进四桶**（分层只作用于「计数类」，不过滤 token）。
    #[test]
    fn subagent_flag_comes_from_parent_id_and_child_usage_still_counts() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join(".zcode/cli/db/db.sqlite");
        fixture(&db);
        // 给子会话补一条 model_usage + 一条 turn 行：子代理的用量**必须**照常计入
        Connection::open(&db)
            .unwrap()
            .execute_batch(
                "INSERT INTO model_usage (session_id, turn_id, query_source, status, provider_id, model_id,
                     input_tokens, output_tokens, computed_total_tokens, started_at)
                 VALUES ('sess_noturn','turn_c','main','completed','bigmodel','GLM-5.3',50,5,55,1000);
                 INSERT INTO turn_usage (session_id, turn_id, status, started_at, duration_ms)
                 VALUES ('sess_noturn','turn_c','completed',1000,1234);",
            )
            .unwrap();
        let no_cursors = HashMap::new();
        let ctx = CollectContext::new(dir.path(), 1_700_000_000_000, &no_cursors);
        let delta =
            ZCodeCollector::collect_with_roots(&ctx, &ZcodeRoots::from_home(dir.path())).unwrap();
        // ① 判据：父 / 子 + 血缘
        let parents = delta.sessions.iter().filter(|s| !s.is_subagent).count();
        assert_eq!(
            parents, 1,
            "只计父会话 → 本夹具父会话数必须是 1（「全父」退化会得 2）"
        );
        let child = delta
            .sessions
            .iter()
            .find(|s| s.session_id == "sess_noturn")
            .unwrap();
        assert!(child.is_subagent, "`parent_id` 非空 = 子代理会话（裁决 A）");
        assert_eq!(
            child.parent_session_id.as_deref(),
            Some("sess_a"),
            "`parent_id` 必须传进 `parent_session_id`（D17 血缘）"
        );
        let parent = delta
            .sessions
            .iter()
            .find(|s| s.session_id == "sess_a")
            .unwrap();
        assert!(!parent.is_subagent, "`parent_id` 为空 = 父会话");
        assert_eq!(
            parent.parent_session_id, None,
            "父会话不得有 parent_session_id"
        );
        // ② R3：子代理的用量照常计入四桶（只分层「计数类」）
        assert!(
            delta
                .details
                .iter()
                .any(|d| d.key.session_id == "sess_noturn" && d.requests == 1),
            "子代理的请求必须照常进四桶（R3：token 总量含子代理）"
        );
        assert_eq!(
            delta.details.iter().map(|d| d.counters.turns).sum::<i64>(),
            3,
            "子会话的 turn 也照常计入（父 2 + 子 1）"
        );
    }

    /// **裁决 B（Important）回归**：水位**不得跨过在飞行（`status='running'`）的行**。
    ///
    /// 这些行是「插入后**原地 UPDATE** 收尾」（证据文档 `:382-388`：按 `(session_id, turn_id)`
    /// 计数零不一致），在飞行的行 `duration_ms` 还是 NULL。水位一旦越过它，这一行**此后再不回头**：
    /// 该 turn 的 `turn_ms` / `error_turn` / `interrupted`（tool 侧 `tool_ms` / `error_tool`）
    /// **永久缺失** —— 而真机 turn 最长 >1 h、tool 最长 ≈83 min，最可能被抓在飞行的正是最有价值的样本。
    #[test]
    fn watermark_stops_before_inflight_rows_and_resumes_when_they_finish() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join(".zcode/cli/db/db.sqlite");
        fixture(&db);
        let now = 1_700_000_000_000i64;
        let fresh = now - 60_000; // 1 分钟前开始 → **不是**僵尸（阈值 24h）
        Connection::open(&db)
            .unwrap()
            .execute_batch(&format!(
                "INSERT INTO model_usage (session_id, turn_id, query_source, status, provider_id, model_id,
                     input_tokens, output_tokens, computed_total_tokens, started_at)
                 VALUES ('sess_a','turn_run','main','running','bigmodel','GLM-5.3',0,0,0,{fresh});
                 INSERT INTO model_usage (session_id, turn_id, query_source, status, provider_id, model_id,
                     input_tokens, output_tokens, computed_total_tokens, started_at)
                 VALUES ('sess_a','turn_tail','main','completed','bigmodel','GLM-5.3',30,3,33,{fresh});
                 INSERT INTO turn_usage (session_id, turn_id, status, started_at, duration_ms)
                 VALUES ('sess_a','turn_run','running',{fresh},NULL);
                 INSERT INTO turn_usage (session_id, turn_id, status, started_at, duration_ms)
                 VALUES ('sess_a','turn_tail','completed',{fresh},4321);
                 INSERT INTO tool_usage VALUES ('t_run','sess_a','turn_run','Bash','running',NULL,NULL,0,{fresh});
                 INSERT INTO tool_usage VALUES ('t_tail','sess_a','turn_tail','Bash','completed',222,0,0,{fresh});"
            ))
            .unwrap();
        let roots = ZcodeRoots::from_home(dir.path());

        fn ord(d: &SourceDelta, k: &str) -> i64 {
            d.cursors
                .iter()
                .find(|c| c.session_id == k)
                .unwrap()
                .ordinal
        }

        // ---- 第一轮：三张表各有一条 running 行（rowid 3 / 3 / 4）----
        let no_cursors = HashMap::new();
        let ctx1 = CollectContext::new(dir.path(), now, &no_cursors);
        let d1 = ZCodeCollector::collect_with_roots(&ctx1, &roots).unwrap();
        assert_eq!(
            ord(&d1, "model_usage"),
            2,
            "水位必须停在 running 行（rowid=3）之前"
        );
        assert_eq!(ord(&d1, "turn_usage"), 2, "水位必须停在 running 行之前");
        assert_eq!(ord(&d1, "tool_usage"), 3, "水位必须停在 running 行之前");
        assert_eq!(
            d1.details.iter().map(|d| d.requests).sum::<i64>(),
            2,
            "在飞行行本身与它后面的行都不得入账（连续终态前缀）"
        );
        assert_eq!(d1.details.iter().map(|d| d.counters.turns).sum::<i64>(), 2);
        assert_eq!(
            d1.details
                .iter()
                .map(|d| d.counters.tool_calls)
                .sum::<i64>(),
            3
        );
        assert!(
            !d1.details
                .iter()
                .any(|d| d.counters.turn_ms.contains(&4321)),
            "终态行排在在飞行行之后 → 本轮不得读到"
        );

        // ---- 轮间：三张表的在飞行行**原地 UPDATE** 收尾（rowid 不变）----
        Connection::open(&db)
            .unwrap()
            .execute_batch(
                "UPDATE model_usage SET status='completed', input_tokens=100, output_tokens=10,
                     computed_total_tokens=110 WHERE turn_id='turn_run';
                 UPDATE turn_usage SET status='completed', duration_ms=999 WHERE turn_id='turn_run';
                 UPDATE tool_usage SET status='completed', duration_ms=888 WHERE id='t_run';",
            )
            .unwrap();
        let cursors: HashMap<_, _> = d1
            .cursors
            .iter()
            .map(|c| (c.session_id.clone(), c.clone()))
            .collect();
        let ctx2 = CollectContext::new(dir.path(), now + 60_000, &cursors);
        let d2 = ZCodeCollector::collect_with_roots(&ctx2, &roots).unwrap();
        // 第二轮：收尾的那一行 + 它后面原本被挡住的终态行，**各恰好一次**
        assert_eq!(d2.new_records, 6, "三张表各 2 行（收尾行 + 它后面那行）");
        assert_eq!(d2.details.iter().map(|d| d.requests).sum::<i64>(), 2);
        assert_eq!(d2.details.iter().map(|d| d.counters.turns).sum::<i64>(), 2);
        assert_eq!(
            d2.details
                .iter()
                .map(|d| d.counters.tool_calls)
                .sum::<i64>(),
            2
        );
        let mut tm: Vec<i64> = d2
            .details
            .iter()
            .flat_map(|d| d.counters.turn_ms.clone())
            .collect();
        tm.sort_unstable();
        assert_eq!(
            tm,
            vec![999, 4321],
            "时长样本各**恰好一次**（少了 = 永久丢失，多了 = 重复计）"
        );
        assert_eq!(
            d2.details.iter().map(|d| d.counters.tool_ms).sum::<i64>(),
            888 + 222
        );
        assert_eq!(ord(&d2, "model_usage"), 4, "收尾后水位必须推进到表尾");
        assert_eq!(ord(&d2, "turn_usage"), 4);
        assert_eq!(ord(&d2, "tool_usage"), 5);
    }

    /// **裁决 B 的安全阀回归**：僵尸 `running` 行（`started_at` 远超 `ZOMBIE_RUNNING_MS`）
    /// **不得把水位永久冻死** —— 强制跨过它（该行照常入账、只是没有时长样本）+ `log::warn!`
    /// + 计数（计数落在采集器私有的游标 `state_json`，不静默）。
    #[test]
    fn zombie_running_row_is_crossed_so_the_watermark_never_freezes() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join(".zcode/cli/db/db.sqlite");
        fixture(&db);
        let now = 1_700_000_000_000i64;
        // 25 h 前开始 → 超过 24 h 阈值（真机 turn 最长 >1 h、tool ≈83 min，不可能误伤活的行）
        let zombie = now - 25 * 60 * 60 * 1000;
        let fresh = now - 60_000;
        Connection::open(&db)
            .unwrap()
            .execute_batch(&format!(
                "INSERT INTO model_usage (session_id, turn_id, query_source, status, provider_id, model_id,
                     input_tokens, output_tokens, computed_total_tokens, started_at)
                 VALUES ('sess_a','turn_z','main','running','bigmodel','GLM-5.3',0,0,0,{zombie});
                 INSERT INTO model_usage (session_id, turn_id, query_source, status, provider_id, model_id,
                     input_tokens, output_tokens, computed_total_tokens, started_at)
                 VALUES ('sess_a','turn_z2','main','completed','bigmodel','GLM-5.3',30,3,33,{fresh});
                 INSERT INTO turn_usage (session_id, turn_id, status, started_at, duration_ms)
                 VALUES ('sess_a','turn_z','running',{zombie},NULL);
                 INSERT INTO turn_usage (session_id, turn_id, status, started_at, duration_ms)
                 VALUES ('sess_a','turn_z2','completed',{fresh},7777);
                 INSERT INTO tool_usage VALUES ('t_z','sess_a','turn_z','Bash','running',NULL,NULL,0,{zombie});
                 INSERT INTO tool_usage VALUES ('t_z2','sess_a','turn_z2','Bash','completed',333,0,0,{fresh});"
            ))
            .unwrap();
        let no_cursors = HashMap::new();
        let ctx = CollectContext::new(dir.path(), now, &no_cursors);
        let d =
            ZCodeCollector::collect_with_roots(&ctx, &ZcodeRoots::from_home(dir.path())).unwrap();
        let ord = |k: &str| {
            d.cursors
                .iter()
                .find(|c| c.session_id == k)
                .unwrap()
                .ordinal
        };
        // ① 水位真的跨过去了（含僵尸行**后面**的那一行）
        assert_eq!(
            ord("model_usage"),
            4,
            "僵尸行必须被强制跨过，否则水位永久冻死"
        );
        assert_eq!(ord("turn_usage"), 4);
        assert_eq!(ord("tool_usage"), 5);
        // ② 僵尸行**照常入账**（只是没有时长样本）：turn 数 / 工具数都含它
        assert_eq!(
            d.details.iter().map(|d| d.counters.turns).sum::<i64>(),
            4,
            "父会话 turn：夹具 2 + 僵尸 1 + 它后面 1"
        );
        assert_eq!(
            d.details.iter().map(|d| d.counters.tool_calls).sum::<i64>(),
            5
        );
        assert_eq!(
            d.details.iter().map(|d| d.requests).sum::<i64>(),
            4,
            "含僵尸 model 行（四桶全 0 / total 0 = 真空回合，保留）"
        );
        // 僵尸行没有时长样本；它后面那行的样本必须在
        assert_eq!(
            d.details.iter().map(|d| d.counters.tool_ms).sum::<i64>(),
            1043 + 333
        );
        assert!(d.details.iter().any(|d| d.counters.turn_ms.contains(&7777)));
        // ③ 计数进私有游标状态（不静默）：三张表各一条僵尸
        let st = d
            .cursors
            .iter()
            .find(|c| c.session_id == "model_usage")
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&st.state_json).unwrap();
        assert_eq!(v["zombie_running"], 3, "三条僵尸行都要计数: {v}");
        assert_eq!(v["dropped_no_model"], 0);
    }

    /// **裁决 C（Important #3）回归**：**跨轮**的模型 / 供应商兜底。
    ///
    /// `model_provider_by_session` 只在本轮 `model_usage` 循环里填 —— 某轮「有新的 turn/tool 行、
    /// 却没有新的 model_usage 行」时（例：分钟级才收尾的长工具 `Agent` / `TaskOutput`），
    /// 旧实现会落 `model=""` / `provider=""` 的明细行（D-32 家族：UI 上一条**无名行**）。
    /// 本用例：R1 只读 model 行；R2 **只**追加一条 tool 行 → 必须回查该会话最后一条 `model_usage`。
    #[test]
    fn later_round_tool_rows_get_the_session_model_and_provider_from_the_db() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join(".zcode/cli/db/db.sqlite");
        fixture(&db);
        let now = 1_700_000_000_000i64;
        let roots = ZcodeRoots::from_home(dir.path());
        let no_cursors = HashMap::new();
        let ctx1 = CollectContext::new(dir.path(), now, &no_cursors);
        let d1 = ZCodeCollector::collect_with_roots(&ctx1, &roots).unwrap();
        // 轮间：**只**追加一条 tool 行（同会话、**无**新的 model_usage 行），时间戳换小时桶便于定位。
        // **刻意不掺 turn 行**：turn 循环在同一轮里会先把缓存填好，工具路径自己的兜底调用就再也
        // 观察不到了（变异测试实测：掺一条 turn 行时「工具路径不走兜底」的变异**全绿** = 假锁）。
        Connection::open(&db)
            .unwrap()
            .execute(
                "INSERT INTO tool_usage VALUES ('t4','sess_a','turn_1','Bash','completed',246,0,0,?1)",
                [now - 60_000],
            )
            .unwrap();
        let cursors: HashMap<_, _> = d1
            .cursors
            .iter()
            .map(|c| (c.session_id.clone(), c.clone()))
            .collect();
        let ctx2 = CollectContext::new(dir.path(), now, &cursors);
        let d2 = ZCodeCollector::collect_with_roots(&ctx2, &roots).unwrap();
        assert_eq!(d2.new_records, 1, "本轮只有这一条新 tool 行");
        let row = d2
            .details
            .iter()
            .find(|d| d.counters.tool_calls == 1 && d.key.session_id == "sess_a")
            .expect("前提：新 tool 行必须自成一条明细");
        assert_eq!(
            row.key.model, "GLM-5.3",
            "跨轮兜底：必须回查该会话最后一条 model_usage（旧实现落 `\"\"`）"
        );
        assert_eq!(row.key.provider, "bigmodel");
        assert_eq!(row.counters.tool_ms, 246);
        // 全量：任何明细行都不得出现空串主键（空串 = UI 无名行 + 与「未归属」混在一起）
        assert!(
            d2.details.iter().all(|d| !d.key.model.is_empty()
                && !d.key.provider.is_empty()
                && !d.key.project_key.is_empty()),
            "不得有 model/provider/project_key 为空串的明细行"
        );
    }

    /// 同上，**turn 路径**专用（同样只在 R2 追加 turn 行，避免另一条路径替它填缓存）。
    #[test]
    fn later_round_turn_rows_get_the_session_model_from_the_db() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join(".zcode/cli/db/db.sqlite");
        fixture(&db);
        let now = 1_700_000_000_000i64;
        let roots = ZcodeRoots::from_home(dir.path());
        let no_cursors = HashMap::new();
        let ctx1 = CollectContext::new(dir.path(), now, &no_cursors);
        let d1 = ZCodeCollector::collect_with_roots(&ctx1, &roots).unwrap();
        Connection::open(&db)
            .unwrap()
            .execute(
                "INSERT INTO turn_usage (session_id, turn_id, status, started_at, duration_ms)
                 VALUES ('sess_a','turn_1b','completed',?1,1357)",
                [now - 60_000],
            )
            .unwrap();
        let cursors: HashMap<_, _> = d1
            .cursors
            .iter()
            .map(|c| (c.session_id.clone(), c.clone()))
            .collect();
        let ctx2 = CollectContext::new(dir.path(), now, &cursors);
        let d2 = ZCodeCollector::collect_with_roots(&ctx2, &roots).unwrap();
        assert_eq!(d2.new_records, 1);
        let row = d2
            .details
            .iter()
            .find(|d| d.counters.turns == 1 && d.key.session_id == "sess_a")
            .expect("前提：新 turn 行必须自成一条明细（旧实现会落 `model=\"\"` 的伪行）");
        assert_eq!(row.key.model, "GLM-5.3", "turn 路径同样必须走跨轮兜底");
        assert_eq!(row.key.provider, "bigmodel");
        assert_eq!(row.counters.turn_ms, vec![1357]);
        assert!(
            d2.details
                .iter()
                .all(|d| !d.key.model.is_empty() && !d.key.provider.is_empty()),
            "不得有 model/provider 为空串的明细行"
        );
    }

    /// **裁决 C 的后半**：会话**确实**没有任何模型来源 → **跳过该行**（D-32 先例：显式丢弃 +
    /// 计数 + 告警），**绝不**写 `model=""` / `provider=""` 的明细行。
    #[test]
    fn rows_without_any_model_source_are_skipped_not_written_as_empty_model() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join(".zcode/cli/db/db.sqlite");
        fixture(&db);
        let now = 1_700_000_000_000i64;
        Connection::open(&db)
            .unwrap()
            .execute_batch(&format!(
                "INSERT INTO session (id, directory, task_type, parent_id, time_updated)
                 VALUES ('sess_nomodel','/p/N','interactive',NULL,1000);
                 INSERT INTO tool_usage VALUES
                     ('t9','sess_nomodel','turn_9','Bash','completed',123,0,0,{t});
                 INSERT INTO turn_usage (session_id, turn_id, status, started_at, duration_ms)
                 VALUES ('sess_nomodel','turn_9','completed',{t},1234);",
                t = now - 60_000
            ))
            .unwrap();
        let no_cursors = HashMap::new();
        let ctx = CollectContext::new(dir.path(), now, &no_cursors);
        let d =
            ZCodeCollector::collect_with_roots(&ctx, &ZcodeRoots::from_home(dir.path())).unwrap();
        assert!(
            d.details.iter().all(|x| !x.key.model.is_empty()
                && !x.key.provider.is_empty()
                && !x.key.project_key.is_empty()),
            "绝不写空模型 / 空供应商 / 空项目键的明细行"
        );
        assert_eq!(
            d.details.iter().map(|x| x.counters.tool_calls).sum::<i64>(),
            3,
            "被跳过的行不入账（前提：同轮其它 tool 行照常入账，不是整源失效）"
        );
        assert_eq!(
            d.details.iter().map(|x| x.counters.turns).sum::<i64>(),
            2,
            "被跳过的 **turn** 行同样不入账（turn / tool 两条路径都要锁）"
        );
        // `new_records` 口径与 kimi / D-32 一致：记**本轮读走的行数**（含被跳过的 2 行）
        assert_eq!(
            d.new_records, 9,
            "2 model + 3 turn + 4 tool（含被跳过的 2 行）"
        );
        let st = d
            .cursors
            .iter()
            .find(|c| c.session_id == "model_usage")
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&st.state_json).unwrap();
        assert_eq!(
            v["dropped_no_model"], 2,
            "两条被跳过的行都要计数（不静默）: {v}"
        );
    }

    /// **裁决 D#4（Minor）**：补**正向**锁 —— 轮间往三张表各追加一行 → 第二轮**恰好**采到新增的
    /// 那一行。原来只有「第二轮 0 新增」，`ordinal = max_rowid + 1` 之类的**前向静默漏行**变异全绿。
    #[test]
    fn appended_rows_are_picked_up_exactly_once_in_the_next_round() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join(".zcode/cli/db/db.sqlite");
        fixture(&db);
        let now = 1_700_000_000_000i64;
        let roots = ZcodeRoots::from_home(dir.path());
        let no_cursors = HashMap::new();
        let ctx1 = CollectContext::new(dir.path(), now, &no_cursors);
        let d1 = ZCodeCollector::collect_with_roots(&ctx1, &roots).unwrap();
        assert!(d1.new_records > 0, "空跑护栏：首轮必须真读到行");
        assert_eq!(d1.new_records, 7);
        // 轮间：三张表各追加一行（同一会话、新的小时桶 → 自成一条明细）
        Connection::open(&db)
            .unwrap()
            .execute_batch(&format!(
                "INSERT INTO model_usage (session_id, turn_id, query_source, status, provider_id, model_id,
                     input_tokens, output_tokens, computed_total_tokens, started_at)
                 VALUES ('sess_a','turn_new','main','completed','bigmodel','GLM-5.3',100,10,110,{t});
                 INSERT INTO turn_usage (session_id, turn_id, status, started_at, duration_ms)
                 VALUES ('sess_a','turn_new','completed',{t},4321);
                 INSERT INTO tool_usage VALUES ('t_new','sess_a','turn_new','Bash','completed',321,0,0,{t});",
                t = now - 60_000
            ))
            .unwrap();
        let cursors: HashMap<_, _> = d1
            .cursors
            .iter()
            .map(|c| (c.session_id.clone(), c.clone()))
            .collect();
        let ctx2 = CollectContext::new(dir.path(), now, &cursors);
        let d2 = ZCodeCollector::collect_with_roots(&ctx2, &roots).unwrap();
        assert_eq!(d2.new_records, 3, "三张表各**恰好**采到 1 行");
        assert_eq!(d2.details.iter().map(|d| d.requests).sum::<i64>(), 1);
        assert_eq!(d2.details.iter().map(|d| d.request_total).sum::<i64>(), 100);
        assert_eq!(d2.details.iter().map(|d| d.counters.turns).sum::<i64>(), 1);
        assert_eq!(
            d2.details
                .iter()
                .flat_map(|d| d.counters.turn_ms.clone())
                .collect::<Vec<_>>(),
            vec![4321],
            "新增的时长样本恰好一次"
        );
        assert_eq!(
            d2.details
                .iter()
                .map(|d| d.counters.tool_calls)
                .sum::<i64>(),
            1
        );
        assert_eq!(
            d2.details.iter().map(|d| d.counters.tool_ms).sum::<i64>(),
            321
        );
        let ord = |d: &SourceDelta, k: &str| {
            d.cursors
                .iter()
                .find(|c| c.session_id == k)
                .unwrap()
                .ordinal
        };
        assert_eq!(ord(&d2, "model_usage"), 3, "水位各推进 1 行");
        assert_eq!(ord(&d2, "turn_usage"), 3);
        assert_eq!(ord(&d2, "tool_usage"), 4);
    }

    /// **裁决 D#5（Minor）**：`started_at` 缺失（NULL）**不得**落 1970 桶 / `ts_ms = 0`
    /// （D-23 尾部同族，与 `time_updated` 同一条纪律）；**真的是 0** 的行照 0 走。
    /// 真实 DDL 三列皆 `not null` → 本条是私有格式漂移的纵深防御。
    ///
    /// **三个 loader 各一条、各自独立可锁**（变异测试要求）：同一会话的三行会并进同一条明细，
    /// `ts_ms` 取**首个写入者** → 只要有一条 loader 的兜底还正确，另外两条的变异就会被掩盖
    /// （实测：三条同会话时「model loader 改回 `COALESCE(…,0)`」的变异**全绿** = 假锁）。
    /// 因此三条 NULL 行分属三个会话（`sess_a` / `sess_noturn` / `sess_null3`），模型名也各不相同。
    #[test]
    fn null_started_at_does_not_become_a_1970_bucket() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join(".zcode/cli/db/db.sqlite");
        fixture(&db);
        let now = 1_700_000_000_000i64;
        Connection::open(&db)
            .unwrap()
            .execute_batch(
                // 夹具（照任务书）把 `tool_usage.started_at` 建成 `NOT NULL`（真实 DDL 亦如此）；
                // 本用例要造的是**私有格式漂移**形态 → 先重建一张允许 NULL 的同构表。
                "ALTER TABLE tool_usage RENAME TO tool_usage_old;
                 CREATE TABLE tool_usage (id TEXT PRIMARY KEY, session_id TEXT, turn_id TEXT, tool_name TEXT,
                     status TEXT, duration_ms INTEGER, exit_code INTEGER, cancelled_by_user INTEGER,
                     started_at INTEGER);
                 INSERT INTO tool_usage SELECT * FROM tool_usage_old;
                 DROP TABLE tool_usage_old;
                 -- 第三个会话（tool loader 的 NULL 行专用；给它一条正常 model_usage 供模型来源）
                 INSERT INTO session (id, directory, task_type, parent_id, time_updated)
                 VALUES ('sess_null3','/p/N3','interactive',NULL,1000);
                 INSERT INTO model_usage (session_id, turn_id, query_source, status, provider_id, model_id,
                     input_tokens, output_tokens, computed_total_tokens, started_at)
                 VALUES ('sess_null3','turn_n3','main','completed','bigmodel','GLM-N3',3,1,4,900);
                 -- 第二个会话（turn loader 的 NULL 行专用）
                 INSERT INTO model_usage (session_id, turn_id, query_source, status, provider_id, model_id,
                     input_tokens, output_tokens, computed_total_tokens, started_at)
                 VALUES ('sess_noturn','turn_n2','main','completed','bigmodel','GLM-N2',3,1,4,900);
                 -- ① model loader 的 NULL（模型名唯一 → 自成一条明细）
                 INSERT INTO model_usage (session_id, turn_id, query_source, status, provider_id, model_id,
                     input_tokens, output_tokens, computed_total_tokens, started_at)
                 VALUES ('sess_a','turn_null','main','completed','bigmodel','GLM-N1',5,1,6,NULL);
                 -- ② turn loader 的 NULL
                 INSERT INTO turn_usage (session_id, turn_id, status, started_at, duration_ms)
                 VALUES ('sess_noturn','turn_n2','completed',NULL,111);
                 -- ③ tool loader 的 NULL
                 INSERT INTO tool_usage VALUES ('t_null','sess_null3','turn_n3','Bash','completed',222,0,0,NULL);
                 -- 真的是 0：必须照 0 走（不得被兜底逻辑改掉事实）
                 INSERT INTO model_usage (session_id, turn_id, query_source, status, provider_id, model_id,
                     input_tokens, output_tokens, computed_total_tokens, started_at)
                 VALUES ('sess_a','turn_zero','main','completed','bigmodel','GLM-ZERO',7,1,8,0);",
            )
            .unwrap();
        let no_cursors = HashMap::new();
        let ctx = CollectContext::new(dir.path(), now, &no_cursors);
        let d =
            ZCodeCollector::collect_with_roots(&ctx, &ZcodeRoots::from_home(dir.path())).unwrap();
        let day_now = crate::services::usage::range::day_key_of_host(now);
        // ① model loader
        let m = d
            .details
            .iter()
            .find(|x| x.key.model == "GLM-N1")
            .expect("前提：NULL started_at 的 model 行必须入账");
        assert_eq!(
            m.ts_ms, now,
            "model loader：NULL 必须按采集时刻归属，不得伪装成 0"
        );
        assert_eq!(m.key.day_key, day_now, "日键必须现算（不得是 1970-01-01）");
        // ② turn loader
        // 该会话另有「正常 model 行」（1970 桶）也用同一个模型名 → 用 turn 样本定位，
        // 不能只按 model 找（HashMap 迭代序不确定，会取到那一行 → 断言测错对象）。
        let t = d
            .details
            .iter()
            .find(|x| x.key.model == "GLM-N2" && x.counters.turn_ms == vec![111])
            .expect("前提：NULL started_at 的 turn 行必须入账");
        assert_eq!(t.ts_ms, now, "turn loader：同样不得落 1970");
        assert_eq!(t.counters.turn_ms, vec![111]);
        assert_eq!(t.key.day_key, day_now);
        // ③ tool loader
        let o = d
            .details
            .iter()
            .find(|x| x.key.model == "GLM-N3" && x.counters.tool_ms == 222)
            .expect("前提：NULL started_at 的 tool 行必须入账");
        assert_eq!(o.ts_ms, now, "tool loader：同样不得落 1970");
        assert_eq!(o.counters.tool_ms, 222);
        assert_eq!(o.key.day_key, day_now);
        // ④ 真的是 0 的行照 0 走（与「缺字段」严格区分）
        let z = d
            .details
            .iter()
            .find(|x| x.key.model == "GLM-ZERO")
            .expect("前提：显式 started_at = 0 的行必须存在且 ts_ms = 0");
        assert_eq!(z.ts_ms, 0);
        assert_eq!(z.requests, 1);
        // ⑤ 夹具里 1000 ms 的行照旧（不得被兜底逻辑顺带改写）
        assert!(
            d.details.iter().any(|x| x.ts_ms == 1000),
            "真实的时间戳必须原样保留"
        );
    }
}
// ==== usage 自省锁：排除区结束，勿删勿复制 ====
