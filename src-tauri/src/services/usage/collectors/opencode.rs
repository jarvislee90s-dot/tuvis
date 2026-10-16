//! OpenCode 采集器（V1/V2 双 schema，说明书 §9.1：**本次只做用量兼容**，
//! 会话上板与状态链缺口另开线——半截改造比不改更糟）。
//! * 表探测决定读哪代；`session_v2` 是 `session` 的**超集**（同 id 两表值一致）→
//!   **按 id 去重、V2 优先**（矩阵 §6.1 四类重复源之一）；
//! * 四桶取**会话级扁平列**（`tokens_input/output/reasoning/cache_read/cache_write`，
//!   V1/V2 同名同构）；`reasoning` 并入产出（Global Constraints 14 同族裁定）；
//! * 逐消息只用于**请求数 / 回合数 / 工具 / 报错**：assistant 是 step（数它会虚高 71%~81%），
//!   **回合数必须数 user 行**；
//! * 工具耗时字段路径两代不同：V1 `state.time.{start,end}`、**V2 在顶层** `time.{created,completed}`
//!   ——不分支会静默拿 0；
//! * 项目一律取 `session.directory`（**禁用 `project_id`**：`global` 哨兵吞掉 4 个目录）。
//!
//! ## 口径登记（Task 18/19 请勿当缺陷重查）
//! * **首轮的小时归属**（fix round 1 / 评审 Minor #7）：本源的**唯一**历史用量是**会话级累计列**
//!   （没有逐请求的逐桶事件，V2 连 `step-finish` 都没有）→ 首轮只能把这**一整段历史四桶**记进
//!   `session.time_updated` 所在的小时/日。**稳态（第二轮起）正确**：每轮只记增量，归属到
//!   「该轮结束时 `time_updated` 所在的小时」。跨小时的活跃会话因此会把**本轮增量**记到
//!   `time_updated` 的小时，而不是逐条消息各自的小时——这是本源的粒度上限，不是 bug。
//! * `parsed_files` 对本源 = **1**（SQLite 源读的是**一个库文件**，不是会话行数；
//!   fix round 1 / 评审 Minor #6——该字段是 `UsageSourceStatus` 的用户可见数字）。
use rusqlite::Connection;
use serde_json::Value;
use std::collections::HashSet;

use super::DeltaBuilder;
use crate::services::usage::collect::{CollectContext, UsageCollector};
use crate::services::usage::delta::{DetailKey, SourceDelta};
use crate::services::usage::error::UsageError;
use crate::services::usage::model::UsageSourceId;
use crate::services::usage::project::project_key_of;
use crate::services::usage::range::hour_key_of_host;
use crate::services::usage::semantics::{default_policy, normalize, resolve_semantics, RawUsage};

pub struct OpenCodeCollector;

impl UsageCollector for OpenCodeCollector {
    fn source_id(&self) -> UsageSourceId {
        UsageSourceId::OpenCode
    }
    fn collect(&self, ctx: &CollectContext<'_>) -> Result<SourceDelta, UsageError> {
        let db = ctx
            .home
            .join(".local")
            .join("share")
            .join("opencode")
            .join("opencode.db");
        if !db.exists() {
            return Ok(SourceDelta::default());
        }
        Self::collect_with_db(ctx, &db)
    }
}

impl OpenCodeCollector {
    /// DB 路径注入版（单测直调，不触真实 home）
    pub fn collect_with_db(
        ctx: &CollectContext<'_>,
        db_path: &std::path::Path,
    ) -> Result<SourceDelta, UsageError> {
        let Some(conn) = crate::monitor::sqlite::open_usage_db(db_path) else {
            return Err(UsageError::new(
                "usage-source-db-open",
                "opencode.db open failed (ro and immutable)",
            ));
        };
        // **供应商规则从 ctx 注入**（§3.2.3 FIX-6）：lib 单测构建注入空规则 → 零 DB；
        // 生产/集成构建才回落到 settings::load()。采集器不得直连设置层。
        let rules = ctx.provider_rules();
        let mut b = DeltaBuilder::new(UsageSourceId::OpenCode, rules);
        // **Minor #6（fix round 1）**：`parsed_files` 是 `UsageSourceStatus` 的**用户可见**数字，
        // 口径是「解析了**几个文件**」——SQLite 源读的是**一个库文件**（不是会话行数；
        // 任务书按会话行累加会让 UI 把「库里的会话数」当文件数显示）。
        b.parsed_files = 1;
        // **A-3 三态（第三轮评审必改 P1）**：`table_exists` 的布尔便捷形态已删除——它把
        // `ProbeFailed`（探测本身失败）压成 `false`，与三态语义矛盾。此处是「在场才继续」的
        // gate，同样必须显式 match：`Absent` 是 V1-only 安装的**预期**分支（静默退回 V1），
        // 而 `ProbeFailed` 必须**响亮**（否则安静地少一批 V2 会话，正是 A-3 要消灭的静默族）。
        let has_v2 = match table_state(&conn, "session_v2") {
            TableState::Present => true,
            TableState::Absent => false,
            TableState::ProbeFailed(e) => {
                log::warn!(
                    "usage/opencode: `session_v2` 表探测本身失败（库损坏 / 锁死 / 权限？）：{e} → \
                     本轮按 V1 口径采集（该源可能少一批 V2 会话）"
                );
                false
            }
        };
        // 会话行：V2 优先，V1 仅补 V2 没有的 id（**超集去重**）
        let mut sessions: Vec<SessionRow> = Vec::new();
        let v2_ids: HashSet<String> = if has_v2 {
            load_sessions(&conn, "session_v2", &mut sessions)
        } else {
            HashSet::new()
        };
        let mut v1_only: Vec<SessionRow> = Vec::new();
        let _ = load_sessions(&conn, "session", &mut v1_only);
        let v1_only_n = v1_only.iter().filter(|s| !v2_ids.contains(&s.id)).count();
        // **Minor #3（fix round 1，不静默）**：逐消息/工具层是**另一张表**——`session_v2` 在场
        // **不代表** `session_message` 在场（两张表一起迁移，但库可能停在中间/被改名）。
        // 缺表时 turn / 请求 / 工具会**整层归零**而四桶照常出数 → 用户看到的是「这个源没有回合/
        // 请求/工具」，而不是「表缺了」。这里**每轮一次性**告警（避免逐会话刷屏）；
        // 「表在场却查不动」（列名不符 / 库被锁）由各扫描器自报。
        if has_v2 {
            warn_if_table_missing(
                &conn,
                "session_message",
                "逐消息层整层不可得（turn / 请求 / 工具本轮为 0，四桶仍出数）",
            );
        }
        if v1_only_n > 0 {
            warn_if_table_missing(
                &conn,
                "message",
                &format!("{v1_only_n} 个 V1 独有会话的 turn / 请求 / 工具本轮为 0（四桶仍出数）"),
            );
        }
        sessions.extend(v1_only.into_iter().filter(|s| !v2_ids.contains(&s.id)));

        for s in &sessions {
            // **Minor #4（fix round 1，D-23 同族）**：`time_updated` 缺失（NULL）与「真的是 0」
            // 是两件事——`unwrap_or(0)` 会把「缺字段」伪装成一个合法时间戳 0 → 落 **1970 桶**、
            // `last_seen_at = 0`（正是 D-23 尾部点名的「把失败伪装成合法值」那一族）。
            // 缺字段 → 用**本轮采集时刻**兜底并 `log::warn!`（不静默造一个 1970 的假时间）；
            // 真的是 0 → 照 0 走（那时落 1970 桶是**事实**，不是伪装）。
            let ts = match s.time_updated {
                Some(t) => t,
                None => {
                    log::warn!(
                        "usage/opencode: 会话 {} 的 time_updated 为 NULL → 本轮按采集时刻 {} 归桶\
                         （不伪装成 0 / 1970）",
                        s.id,
                        ctx.now_ms
                    );
                    ctx.now_ms
                }
            };
            let hour = hour_key_of_host(ts);
            let (model_id, provider_id) = parse_model(&s.model);
            // **供应商唯一入口**（GC 6 / B6）：opencode 有直接字段 providerID → Measured
            let (provider, _) = b.provider_of(
                if provider_id.is_empty() {
                    None
                } else {
                    Some(&provider_id)
                },
                &model_id,
            );
            let key = DetailKey {
                session_id: s.id.clone(),
                hour_key: hour.clone(),
                day_key: hour.chars().take(10).collect(),
                // 记录级项目键：opencode 的归属来自 session.directory
                // （**禁用 project_id**：global 哨兵吞掉 4 个不同目录，§5.1）
                project_key: project_key_of(Some(&s.directory)),
                model: model_id.clone(),
                provider,
            };
            // **B4 修复点**：四桶是**会话级累计列**（不是逐请求事件）→ 必须读游标（上次快照）
            // 做差值；游标只写不读的话每轮都会把整份再加一遍（明细是累加语义 → 按轮次膨胀）。
            // 差值语义与 dsh 投影缓存同款：变小（库重写/会话重置）按「本轮 = 当前值」整份计。
            let mut state: OpenCodeState = ctx
                .cursor_of(&s.id)
                .and_then(|c| serde_json::from_str(&c.state_json).ok())
                .unwrap_or_default();
            let diff = |cur: i64, old: i64| if cur >= old { cur - old } else { cur };
            let raw = RawUsage {
                input_raw: diff(s.input, state.input),
                cache_read: diff(s.cache_read, state.cache_read),
                cache_write: diff(s.cache_write, state.cache_write),
                output: diff(s.output, state.output),
                reasoning: diff(s.reasoning, state.reasoning),
                total_raw: None,
            };
            let sem = resolve_semantics(default_policy(UsageSourceId::OpenCode), &raw);
            // **任务书缺陷修复（Task 14 实测，见报告「偏差申报」）**：任务书的代码只把 `state`
            // 当**读**的差值基，从头到尾**没有把本轮的会话级累计值写回去**——于是每轮的差值基
            // 恒为「全 0」，第二轮起把整份四桶再加一遍（B4 用例第二轮断言立刻红：1010 ≠ 0）。
            // 累计快照与 rowid 水位是同一件事的两半（都是「下一轮的差值基」），必须同轮推进。
            state.input = s.input;
            state.output = s.output;
            state.reasoning = s.reasoning;
            state.cache_read = s.cache_read;
            state.cache_write = s.cache_write;
            // 零差值不写行：`normalize` 对「四桶全 0 且无 total」会保留（真空回合计一次请求），
            // 若不加这道门，第二轮起每轮都会往账本里 upsert 一条全 0 明细（行数不涨但语义是脏的）
            let changed = raw.input_raw != 0
                || raw.cache_read != 0
                || raw.cache_write != 0
                || raw.output != 0
                || raw.reasoning != 0;
            if changed {
                if let Some(n) = normalize(&raw, sem) {
                    let d = b.detail(key.clone(), ts);
                    d.buckets.add(&n.buckets);
                    d.request_total += n.request_total;
                }
            }
            // 逐消息：请求数 / user 行 / 工具 / 报错（**按 rowid 水位只读新增行**）。
            // 逐会话选代际：在 `session_v2` 里的会话只读 `session_message`（同一请求在
            // V1/V2 各有一份表示，两张表都读就是双计）；V1 独有会话只读 `message`/`part`。
            let new_rows = if has_v2 && v2_ids.contains(&s.id) {
                scan_v2_messages(&conn, &mut b, &key, s, &mut state, ts)
            } else {
                scan_v1_messages(&conn, &mut b, &key, s, &mut state, ts)
            };
            b.new_records += new_rows as i64;
            b.session(
                &s.id,
                Some(&s.directory),
                s.title.clone(),
                false,
                None,
                None,
                ts,
            );
            // 游标：`last_cumulative` 记会话级四桶总和（快速判"是否变化"），
            // `state_json` 存逐桶快照 + 逐消息/工具 parts 的 rowid 水位（下一轮的差值基）
            b.push_cursor(crate::services::usage::delta::CursorDelta {
                session_id: s.id.clone(),
                fingerprint: String::new(),
                byte_offset: 0,
                ordinal: state.msg_rowid,
                last_cumulative: Some(
                    s.input + s.output + s.reasoning + s.cache_read + s.cache_write,
                ),
                mtime_ms: ts,
                file_size: 0,
                state_json: serde_json::to_string(&state).unwrap_or_default(),
            });
        }
        Ok(b.finish())
    }
}

/// opencode 的增量基：会话级累计四桶快照 + 逐消息/工具 parts 的 rowid 水位。
/// 两者都必须进游标 `state_json`，否则第二轮起整份重算（B4）。
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
struct OpenCodeState {
    input: i64,
    output: i64,
    reasoning: i64,
    cache_read: i64,
    cache_write: i64,
    /// `message`（V1）/ `session_message`（V2）的 rowid 水位
    msg_rowid: i64,
    /// `part`（V1 工具行）的 rowid 水位
    part_rowid: i64,
}

struct SessionRow {
    id: String,
    directory: String,
    title: Option<String>,
    /// **`None` = 缺字段**（不是「真的是 0」）；调用点据此区分（Minor #4 / D-23 同族）
    time_updated: Option<i64>,
    model: String,
    input: i64,
    output: i64,
    reasoning: i64,
    cache_read: i64,
    cache_write: i64,
}

/// 表探测的**三态**（**A-3 连带修复 / P2-1 的"不可能触发的守卫"**）：
/// * `Present` —— 表在场；
/// * `Absent` —— **确定不存在**：V2-only 安装里试 V1 表就是这一支，**预期**、不告警；
/// * `ProbeFailed(e)` —— **探测本身失败**（NOTADB / 库损坏 / 锁死 / 权限）。
///   这是本轮修掉的盲区：原先 `.unwrap_or(false)` 把这一支伪装成 `Absent`
///   ⇒ `load_sessions` 里那条「表在场却查不动」的 `log::warn!` **永远不会响**。
#[derive(Debug)]
enum TableState {
    Present,
    Absent,
    ProbeFailed(rusqlite::Error),
}

/// 表探测（三态）。**调用方必须显式处理 `ProbeFailed`**（响亮），不得再压回 bool——
/// 旧的布尔便捷形态 `table_exists` 已删除（第三轮评审必改 P1：它把 `ProbeFailed` 压成 `false`，
/// 与本三态的语义直接矛盾）。有源码级自省锁钉住：
/// `tests::production_half_never_folds_the_table_probe_back_to_bool`。
fn table_state(conn: &Connection, name: &str) -> TableState {
    match conn.query_row(
        "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name=?1",
        [name],
        |r| r.get::<_, bool>(0),
    ) {
        Ok(true) => TableState::Present,
        Ok(false) => TableState::Absent,
        // **A-3**：探测本身失败**必须原样带出来**（响亮），不得压回 `false`
        // —— 那会让下面的告警与"表不存在"不可区分（不可能触发的守卫）。
        Err(e) => TableState::ProbeFailed(e),
    }
}

/// **「表不在场」的告警（A-3 三态 / 第三轮评审必改 P1）**：两种"不在场"必须**分开说**——
/// `Absent` = 该代际安装里**确定**没有这张表（V1/V2 双 schema 的**预期**分支）；
/// `ProbeFailed` = **探测本身失败**（库损坏 / 锁死 / 权限），处置完全不同（要去看库）。
/// 旧形态 `!table_exists(..)` 把两者说成同一句「表不存在」= **误报**（与三态语义矛盾，
/// 且把探测失败藏进"预期分支"的静默里）。两种都**响亮**，话术不同、可区分。
fn warn_if_table_missing(conn: &Connection, table: &str, consequence: &str) {
    match table_state(conn, table) {
        TableState::Absent => log::warn!("usage/opencode: `{table}` 表不存在 → {consequence}"),
        TableState::ProbeFailed(e) => log::warn!(
            "usage/opencode: `{table}` 表探测本身失败（库损坏 / 锁死 / 权限？）：{e} → {consequence}"
        ),
        // 在场：调用点只在「它不在场」时才告警，走到这里说明前提已变（不告警、不误报）
        TableState::Present => {}
    }
}

/// **A-3 推广**：`prepare()` 失败时统一留痕（把 fix round 1 在 `load_sessions` 里做的
/// 「区分」模式推广到**本文件其余三处**同款调用点——`message` / `part` / `session_message`）。
///
/// 三态各走各的话术：`Present` = 表在场却查不动（列名不符 / 库被锁）；
/// `ProbeFailed` = **探测本身失败**（库损坏 / 锁死 / 权限）——旧 `.unwrap_or(false)` 形态下
/// 这一支被伪装成「表不存在」，于是这些告警**全部不可能触发**（A-3 的根因类）；
/// `Absent` = 预期的代际分支，不告警。
fn warn_if_table_unreadable(conn: &Connection, table: &str, consequence: &str) {
    match table_state(conn, table) {
        TableState::Present => log::warn!(
            "usage/opencode: `{table}` 表在场但查询准备失败（列名不符 / 库不可读？）→ {consequence}"
        ),
        TableState::ProbeFailed(e) => log::warn!(
            "usage/opencode: `{table}` 表探测本身失败（库损坏 / 锁死 / 权限？）：{e} → {consequence}"
        ),
        TableState::Absent => {}
    }
}

/// 读会话行（表名来自本文件的常量，无注入面）——`project_id` **刻意不读**（禁用）
fn load_sessions(conn: &Connection, table: &str, out: &mut Vec<SessionRow>) -> HashSet<String> {
    let sql = format!(
        "SELECT id, directory, title, time_updated, model, tokens_input, tokens_output,
                tokens_reasoning, tokens_cache_read, tokens_cache_write FROM {}",
        table
    );
    // **这里的软失败是刻意的，不要"顺手"改成返回 `Err`**（与 zcode 的 `load_sessions` 不同）：
    // 本函数是 **V1/V2 双 schema 的表探测**——调用方对 `"session"`（V1）无条件试一次，
    // 而 **V2-only 的安装里 `session` 表本来就不存在**，此时 `prepare()` 必然失败且属预期，
    // 所以只能软失败为空集（调用方再用 `table_state("session_v2")` 与超集去重决定用哪代）。
    // 已知边界（登记在 §3.2.2 的"未改/有保留"）：**表存在但列名不符**时也会走这条静默路径，
    // 表现为该源安静地少一批会话。要收紧就得先 `table_state` 再区分"表不存在"与"列不符"，
    // 属可选加固、不在本轮范围（现有 fixture 四表合法，不会红）。
    let Ok(mut stmt) = conn.prepare(&sql) else {
        // **Minor #3（fix round 1，不静默）+ A-3（三态）**：三种情况必须分开——
        // 表在场 = **表在场却查不动**（列名不符 / 库被锁 / 损坏）→ 留痕；
        // 探测本身失败 = 库损坏 / 锁死 / 权限 → **同样必须响亮**（原先 `.unwrap_or(false)`
        // 把它伪装成"表不存在"，这条告警因此**永不触发**）；
        // 确定不存在 = V2-only 安装的**预期**分支（不告警）。
        warn_if_table_unreadable(conn, table, "本表会话本轮全部跳过");
        return HashSet::new();
    };
    let rows = stmt.query_map([], |row| {
        Ok(SessionRow {
            id: row.get(0)?,
            directory: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
            title: row.get(2)?,
            time_updated: row.get::<_, Option<i64>>(3)?,
            model: row.get::<_, Option<String>>(4)?.unwrap_or_default(),
            input: row.get::<_, Option<i64>>(5)?.unwrap_or(0),
            output: row.get::<_, Option<i64>>(6)?.unwrap_or(0),
            reasoning: row.get::<_, Option<i64>>(7)?.unwrap_or(0),
            cache_read: row.get::<_, Option<i64>>(8)?.unwrap_or(0),
            cache_write: row.get::<_, Option<i64>>(9)?.unwrap_or(0),
        })
    });
    let mut ids = HashSet::new();
    if let Ok(rows) = rows {
        for r in rows.flatten() {
            ids.insert(r.id.clone());
            out.push(r);
        }
    }
    ids
}

/// `session.model` 是对象（V1 就已是对象）：{id|modelID, providerID}
fn parse_model(model: &str) -> (String, String) {
    let Ok(v) = serde_json::from_str::<Value>(model) else {
        return (model.to_string(), String::new());
    };
    let id = v
        .get("id")
        .or_else(|| v.get("modelID"))
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let provider = v
        .get("providerID")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    (id, provider)
}

/// V1：`message.data`（role 在 JSON；工具耗时在 `part.data.state.time`）。
/// **B4**：按 `rowid > state.msg_rowid / state.part_rowid` 只读新增行，并把水位推进到
/// 本轮最大值（否则每轮把全部消息再加一遍）。返回本轮新处理的行数（喂给 `new_records`）。
fn scan_v1_messages(
    conn: &Connection,
    b: &mut DeltaBuilder,
    key: &DetailKey,
    s: &SessionRow,
    state: &mut OpenCodeState,
    ts_fallback: i64,
) -> usize {
    let mut new_rows = 0usize;
    // 逐消息（rowid 水位）
    let Ok(mut stmt) = conn.prepare(
        "SELECT rowid, data FROM message WHERE session_id = ?1 AND rowid > ?2 ORDER BY rowid",
    ) else {
        // **Minor #3**：表在场却查不动 → 留痕（缺表的一次性告警在 `collect_with_db`）
        warn_if_table_unreadable(
            conn,
            "message",
            &format!("会话 {} 的 turn/请求本轮为 0", s.id),
        );
        return 0;
    };
    let rows: Vec<(i64, String)> = stmt
        .query_map(rusqlite::params![&s.id, state.msg_rowid], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, Option<String>>(1)?.unwrap_or_default(),
            ))
        })
        .map(|it| it.flatten().collect())
        .unwrap_or_default();
    for (rowid, data) in rows {
        state.msg_rowid = state.msg_rowid.max(rowid);
        new_rows += 1;
        let Ok(v) = serde_json::from_str::<Value>(&data) else {
            continue;
        };
        let ts = v
            .pointer("/time/created")
            .and_then(|x| x.as_i64())
            .unwrap_or(ts_fallback);
        let hour = hour_key_of_host(ts);
        let k = DetailKey {
            hour_key: hour.clone(),
            day_key: hour.chars().take(10).collect(),
            ..key.clone()
        };
        match v.get("role").and_then(|r| r.as_str()).unwrap_or("") {
            "user" => {
                b.detail(k, ts).counters.turns += 1; // 回合数 = user 行（assistant 是 step）
            }
            "assistant" => {
                // 请求数：带 tokens 的 assistant 行（四桶全 0 也是一次模型响应）
                if v.get("tokens").is_some() {
                    b.detail(k.clone(), ts).requests += 1;
                }
                if let Some(err) = v.get("error") {
                    let name = err.get("name").and_then(|x| x.as_str()).unwrap_or("");
                    let d = b.detail(k.clone(), ts);
                    if name == "MessageAbortedError" {
                        d.counters.interrupted += 1; // 用户中断单列
                    } else {
                        d.counters.error_model += 1;
                    }
                }
                let completed = v.pointer("/time/completed").and_then(|x| x.as_i64());
                if let Some(end) = completed {
                    b.detail(k.clone(), ts)
                        .counters
                        .turn_ms
                        .push((end - ts).max(0));
                }
            }
            _ => {}
        }
    }
    // 工具：V1 `part.data.state.time.{start,end}`（同样按 rowid 水位增量）
    let Ok(mut stmt) = conn.prepare(
        "SELECT rowid, data FROM part WHERE session_id = ?1 AND rowid > ?2 ORDER BY rowid",
    ) else {
        // **Minor #3**：同上（V1 的工具行在 `part`）
        warn_if_table_unreadable(conn, "part", &format!("会话 {} 的工具耗时本轮为 0", s.id));
        return new_rows;
    };
    let parts: Vec<(i64, String)> = stmt
        .query_map(rusqlite::params![&s.id, state.part_rowid], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, Option<String>>(1)?.unwrap_or_default(),
            ))
        })
        .map(|it| it.flatten().collect())
        .unwrap_or_default();
    for (rowid, data) in parts {
        state.part_rowid = state.part_rowid.max(rowid);
        new_rows += 1;
        let Ok(v) = serde_json::from_str::<Value>(&data) else {
            continue;
        };
        if v.get("type").and_then(|t| t.as_str()) != Some("tool") {
            continue;
        }
        let start = v.pointer("/state/time/start").and_then(|x| x.as_i64());
        let end = v.pointer("/state/time/end").and_then(|x| x.as_i64());
        let ts = start.or(end).unwrap_or(ts_fallback);
        let hour = hour_key_of_host(ts);
        let k = DetailKey {
            hour_key: hour.clone(),
            day_key: hour.chars().take(10).collect(),
            ..key.clone()
        };
        // 工具名：真机在 `$.tool`（28/28）；`/state/tool` 是防御性兜底（V1 数据里不存在）
        let name = v
            .pointer("/state/tool")
            .or_else(|| v.get("tool"))
            .and_then(|x| x.as_str())
            .unwrap_or("unknown");
        let d = b.detail(k, ts);
        d.counters.tool_calls += 1;
        d.counters
            .tool_stats
            .entry(name.to_string())
            .or_insert((0, 0))
            .0 += 1;
        if let (Some(a), Some(bb)) = (start, end) {
            let dur = (bb - a).max(0);
            d.counters.tool_ms += dur;
            d.counters
                .tool_stats
                .entry(name.to_string())
                .or_insert((0, 0))
                .1 += dur;
        }
    }
    new_rows
}

/// V2：`session_message`（**type 在行级列**、模型是对象、工具 time 在**顶层**）。
/// **B4**：`rowid > state.msg_rowid` 只读新增行；返回本轮新处理的行数。
/// （V2 的工具条目内嵌在 assistant 消息的 `content[]` 里，天然随该行一起增量。）
fn scan_v2_messages(
    conn: &Connection,
    b: &mut DeltaBuilder,
    key: &DetailKey,
    s: &SessionRow,
    state: &mut OpenCodeState,
    ts_fallback: i64,
) -> usize {
    let mut new_rows = 0usize;
    let Ok(mut stmt) = conn.prepare(
        "SELECT rowid, type, data FROM session_message WHERE session_id = ?1 AND rowid > ?2 ORDER BY rowid",
    ) else {
        // **Minor #3**：`session_v2` 在场不代表 `session_message` 可查——表在场却查不动要留痕
        // （表**不存在**的一次性告警在 `collect_with_db`，避免逐会话刷屏）
        warn_if_table_unreadable(
            conn,
            "session_message",
            &format!("会话 {} 的 turn/请求/工具本轮为 0（四桶仍出数）", s.id),
        );
        return 0;
    };
    let rows: Vec<(i64, String, String)> = stmt
        .query_map(rusqlite::params![&s.id, state.msg_rowid], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?.unwrap_or_default(),
            ))
        })
        .map(|it| it.flatten().collect())
        .unwrap_or_default();
    for (rowid, mtype, data) in rows {
        state.msg_rowid = state.msg_rowid.max(rowid);
        new_rows += 1;
        let Ok(v) = serde_json::from_str::<Value>(&data) else {
            continue;
        };
        let ts = v
            .pointer("/time/created")
            .and_then(|x| x.as_i64())
            .unwrap_or(ts_fallback);
        let hour = hour_key_of_host(ts);
        let k = DetailKey {
            hour_key: hour.clone(),
            day_key: hour.chars().take(10).collect(),
            ..key.clone()
        };
        match mtype.as_str() {
            "user" => {
                b.detail(k, ts).counters.turns += 1;
            }
            "assistant" => {
                if v.get("tokens").is_some() {
                    b.detail(k.clone(), ts).requests += 1;
                }
                if let Some(err) = v.get("error") {
                    let ty = err.get("type").and_then(|x| x.as_str()).unwrap_or("");
                    let d = b.detail(k.clone(), ts);
                    if ty.contains("aborted") {
                        d.counters.interrupted += 1;
                    } else {
                        d.counters.error_model += 1;
                    }
                }
                if v.get("finish").and_then(|x| x.as_str()) == Some("error") {
                    b.detail(k.clone(), ts).counters.error_turn += 1;
                }
                if let Some(end) = v.pointer("/time/completed").and_then(|x| x.as_i64()) {
                    b.detail(k.clone(), ts)
                        .counters
                        .turn_ms
                        .push((end - ts).max(0));
                }
                // V2 工具：content[] 里 type=="tool"，**time 在条目顶层**（state 里没有）
                if let Some(items) = v.get("content").and_then(|c| c.as_array()) {
                    for item in items {
                        if item.get("type").and_then(|t| t.as_str()) != Some("tool") {
                            continue;
                        }
                        let start = item.pointer("/time/created").and_then(|x| x.as_i64());
                        let end = item.pointer("/time/completed").and_then(|x| x.as_i64());
                        let tts = start.unwrap_or(ts);
                        let ihour = hour_key_of_host(tts);
                        let ik = DetailKey {
                            hour_key: ihour.clone(),
                            day_key: ihour.chars().take(10).collect(),
                            ..key.clone()
                        };
                        // 工具名：真机在**条目顶层 `name`**（28/28；`tool` 键在 V2 不存在
                        // ——照抄 V1 的 `tool` 会让 V2 的全部工具静默落 "unknown"）；
                        // 末尾的 `/state/tool` 是既有解析器的历史形态兜底。
                        let name = item
                            .get("name")
                            .or_else(|| item.get("tool"))
                            .or_else(|| item.pointer("/state/tool"))
                            .and_then(|x| x.as_str())
                            .unwrap_or("unknown");
                        let d = b.detail(ik, tts);
                        d.counters.tool_calls += 1;
                        d.counters
                            .tool_stats
                            .entry(name.to_string())
                            .or_insert((0, 0))
                            .0 += 1;
                        if let (Some(a), Some(bb)) = (start, end) {
                            let dur = (bb - a).max(0);
                            d.counters.tool_ms += dur;
                            d.counters
                                .tool_stats
                                .entry(name.to_string())
                                .or_insert((0, 0))
                                .1 += dur;
                        }
                    }
                }
            }
            _ => {}
        }
    }
    new_rows
}

// ==== usage 自省锁：以下为排除区（测试代码），勿删勿复制 ====
#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::usage::caps::caps_of;
    // `UsageCollector` **刻意不导入**（D-25 同族门禁修复）：本模块的用例只直调
    // `OpenCodeCollector::collect_with_db`（DB 路径注入版），`all()` 上的 `source_id()`
    // 是 trait object 的方法调用、不需要 trait 进作用域——照抄任务书的 `{CollectContext,
    // UsageCollector}` 会留一个 `unused_imports`（`-D warnings` 下即 error）。
    use crate::services::usage::collect::CollectContext;
    use crate::services::usage::model::{SourceKind, UsageSourceId};
    // `day_key_of_host` 只在用例里用（现算期望值，跨宿主时区无关，fix round 2 / 裁决 A）——
    // 故**不能**提到模块级 `use`（生产路径会 `unused_imports`）
    use crate::services::usage::range::day_key_of_host;
    use rusqlite::Connection;
    use serde_json::json;
    use std::collections::HashMap;

    /// fixture：V1 表与 V2 表**同 id 并存**（V2 是超集）+ V1 独有会话 + 两代工具耗时路径。
    ///
    /// 工具条目按**真机形态**落字段（`research/OpenCode-V2-schema-查证.md` §4.3/4.4 实测：
    /// V1 `part.data.tool` 28/28、V2 内嵌条目 `content[].name` 28/28 而 `content[].tool` 0/28）
    /// ——夹具若只桩「成功路径」形状的名字字段，名字分支就永远拿不到红。
    fn fixture(dir: &std::path::Path) -> std::path::PathBuf {
        let p = dir.join(".local/share/opencode/opencode.db");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let conn = Connection::open(&p).unwrap();
        conn.execute_batch(
            "CREATE TABLE session (id TEXT PRIMARY KEY, directory TEXT, title TEXT, time_updated INTEGER,
                 model TEXT, tokens_input INTEGER, tokens_output INTEGER, tokens_reasoning INTEGER,
                 tokens_cache_read INTEGER, tokens_cache_write INTEGER);
             CREATE TABLE session_v2 (id TEXT PRIMARY KEY, directory TEXT, title TEXT, time_updated INTEGER,
                 model TEXT, tokens_input INTEGER, tokens_output INTEGER, tokens_reasoning INTEGER,
                 tokens_cache_read INTEGER, tokens_cache_write INTEGER);
             CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, data TEXT);
             CREATE TABLE session_message (id TEXT PRIMARY KEY, session_id TEXT, type TEXT, seq INTEGER, data TEXT);
             CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT, data TEXT);",
        ).unwrap();
        let model = json!({"id":"DeepSeek-V4-Flash","providerID":"volcengine-plan"}).to_string();
        // 同 id 双表并存（V2 是超集，实测值一致）——按 id 去重时 V2 优先
        for t in ["session", "session_v2"] {
            conn.execute(&format!(
                "INSERT INTO {t} (id, directory, title, time_updated, model, tokens_input, tokens_output,
                   tokens_reasoning, tokens_cache_read, tokens_cache_write)
                 VALUES ('ses_dup','/p/A','t',100,?1,1000,100,50,800,0)"), [&model]).unwrap();
        }
        // V1 独有会话（未升级）：**也有自己的 message/part 行**——否则"两代工具耗时路径
        // 都得取到"这条断言是空跑（旧 fixture 的 ses_v1only 没有任何消息行）
        conn.execute(
            "INSERT INTO session (id, directory, title, time_updated, model, tokens_input,
             tokens_output, tokens_reasoning, tokens_cache_read, tokens_cache_write)
             VALUES ('ses_v1only','/p/B','t',90,?1,10,5,0,0,0)",
            [&model],
        )
        .unwrap();
        conn.execute("INSERT INTO message (id, session_id, time_created, data) VALUES
            ('m4','ses_v1only',50,?1), ('m5','ses_v1only',51,?2)",
            rusqlite::params![
                json!({"role":"user","time":{"created":50}}).to_string(),
                json!({"role":"assistant","modelID":"DeepSeek-V4-Flash","providerID":"volcengine-plan",
                       "tokens":{"total":11,"input":7,"output":3,"reasoning":0,
                                 "cache":{"read":0,"write":0}},
                       "time":{"created":51,"completed":1051}}).to_string(),
            ]).unwrap();
        // V1 工具条目：工具名在 `$.tool`（真机 28/28）
        conn.execute("INSERT INTO part (id, message_id, session_id, data) VALUES
            ('p2','m5','ses_v1only',?1)",
            [json!({"type":"tool","tool":"bash","state":{"status":"completed","time":{"start":1000,"end":1500}}}).to_string()]).unwrap();
        // V1：user 行（turn 数）+ assistant 行（请求数；其中一条四桶全 0）+ 工具 part（V1 state.time）
        conn.execute("INSERT INTO message (id, session_id, time_created, data) VALUES
            ('m1','ses_dup',1,?1), ('m2','ses_dup',2,?2), ('m3','ses_dup',3,?3)",
            rusqlite::params![
                json!({"role":"user","time":{"created":1}}).to_string(),
                json!({"role":"assistant","modelID":"DeepSeek-V4-Flash","providerID":"volcengine-plan",
                       "tokens":{"total":150,"input":100,"output":20,"reasoning":10,
                                 "cache":{"read":20,"write":0}},
                       "time":{"created":2,"completed":2002}}).to_string(),
                json!({"role":"assistant","finish":"stop","tokens":{"total":0,"input":0,"output":0,
                       "reasoning":0,"cache":{"read":0,"write":0}},"time":{"created":3,"completed":4}}).to_string(),
            ]).unwrap();
        conn.execute("INSERT INTO part (id, message_id, session_id, data) VALUES
            ('p1','m2','ses_dup',?1)",
            [json!({"type":"tool","tool":"read","state":{"status":"completed","time":{"start":2000,"end":2500}}}).to_string()]).unwrap();
        // V2：type 在**行级列**（V2 无 $.role）、model 是对象、工具 time 在**顶层**、
        // 工具名在**顶层 `name`**（真机 28/28；`tool` 键在 V2 不存在）
        conn.execute("INSERT INTO session_message (id, session_id, type, seq, data) VALUES
            ('v1','ses_dup','user',1,?1), ('v2','ses_dup','assistant',2,?2), ('v3','ses_dup','assistant',3,?3)",
            rusqlite::params![
                json!({"time":{"created":10},"text":"hi"}).to_string(),
                json!({"time":{"created":11,"completed":3011},"model":{"id":"DeepSeek-V4-Flash","providerID":"volcengine-plan"},
                       "tokens":{"input":100,"output":20,"reasoning":10,"cache":{"read":20,"write":0}},
                       "content":[{"type":"tool","name":"bash","time":{"created":3000,"completed":3800}}]}).to_string(),
                json!({"time":{"created":12,"completed":13},"error":{"type":"provider.auth","message":"bad key"},
                       "model":{"id":"DeepSeek-V4-Flash","providerID":"volcengine-plan"},
                       "tokens":{"input":1,"output":1,"reasoning":0,"cache":{"read":0,"write":0}}}).to_string(),
            ]).unwrap();
        p
    }

    fn collect(db: &std::path::Path) -> crate::services::usage::delta::SourceDelta {
        // 空游标表必须先绑成变量：`&HashMap::new()` 是**临时值**，借用进 `ctx` 就是
        // E0716（temporary value dropped while borrowed）——语句结束即释放。
        let no_cursors = HashMap::new();
        let ctx = CollectContext::new(db, 1_700_000_000_000, &no_cursors);
        OpenCodeCollector::collect_with_db(&ctx, db).unwrap()
    }

    #[test]
    fn v2_supersedes_v1_by_session_id_and_all_metrics_come_out_in_one_pass() {
        let dir = tempfile::tempdir().unwrap();
        let db = fixture(dir.path());
        let delta = collect(&db);
        // 逐会话选择代际：`ses_dup` 在 V2 表里 → **只读 V2 的逐消息行**（同一请求在 V1/V2
        // 各有一份表示，两张表都读就是双计）；`ses_v1only` 在 V2 里没有 → 只读 V1 的逐消息行。
        // 合计 = V2 的 v2+v3（2）+ V1 的 m5（1）= 3。
        assert_eq!(delta.details.iter().map(|d| d.requests).sum::<i64>(), 3);
        let dup: i64 = delta
            .details
            .iter()
            .filter(|d| d.key.session_id == "ses_dup")
            .map(|d| d.requests)
            .sum();
        assert_eq!(
            dup, 2,
            "ses_dup 只按 V2 计：v2 + v3（V1 的 m2 是同一次请求的另一代表示）"
        );
        // 四桶取**会话级扁平列**（V1/V2 同名同构），reasoning 并入产出
        let d = delta
            .details
            .iter()
            .find(|d| d.key.session_id == "ses_dup")
            .unwrap();
        assert_eq!(d.buckets.input_fresh, 1000);
        assert_eq!(d.buckets.cache_read, 800);
        assert_eq!(d.buckets.output, 150, "output 100 + reasoning 50");
        assert_eq!(
            d.key.provider, "volcengine-plan",
            "供应商是直接字段（实测）"
        );
        // **GC 6 / B6 唯一入口锁**：供应商三态只能由 `DeltaBuilder::provider_of` 记录。
        // 若改成直连 `provider::resolve_provider`，行键里的 provider 一字不差，
        // 但 `provider_kind` 会退化成 Unknown（UI 三态标记全丢）→ 下面这条必须红。
        assert_eq!(
            d.provider_kind,
            SourceKind::Measured,
            "供应商必须经 DeltaBuilder::provider_of（直连 resolve_provider 会让三态退化成 unknown）"
        );
        // V1 独有会话也在（左连不丢会话），且它的逐消息行走 V1 路径
        let v1_only = delta
            .details
            .iter()
            .find(|d| d.key.session_id == "ses_v1only")
            .unwrap();
        assert_eq!(v1_only.requests, 1, "V1 独有会话的 m5");
        assert_eq!(v1_only.buckets.input_fresh, 10);
        // turn 数 = user 行（V1 message.role / V2 行级 type），不是 assistant 行
        let turns: i64 = delta.details.iter().map(|d| d.counters.turns).sum();
        assert_eq!(turns, 2, "ses_dup 一条 V2 user + ses_v1only 一条 V1 user");
        // 工具耗时两代路径都取到：V1 state.time 500ms + V2 顶层 time 800ms
        let ms: i64 = delta.details.iter().map(|d| d.counters.tool_ms).sum();
        assert_eq!(ms, 1300);
        let calls: i64 = delta.details.iter().map(|d| d.counters.tool_calls).sum();
        assert_eq!(calls, 2);
        // 工具名路径两代不同（真机实测：V1 `$.tool` / V2 顶层 `$.name`）——
        // 只读 V1 的键名或只读 V2 的 `tool` 键都会静默落 "unknown"
        assert_eq!(
            v1_only.counters.tool_stats.get("bash"),
            Some(&(1, 500)),
            "V1 工具名取 `part.data.tool`（真机 28/28）"
        );
        assert_eq!(
            d.counters.tool_stats.get("bash"),
            Some(&(1, 800)),
            "V2 工具名取 `content[].name`（真机 28/28；`content[].tool` 在 V2 不存在）"
        );
        // 报错：V2 provider.auth → 模型层；工具层不可得（caps=false）
        let em: i64 = delta.details.iter().map(|d| d.counters.error_model).sum();
        assert_eq!(em, 1);
        assert!(
            !caps_of(UsageSourceId::OpenCode).error_tool,
            "工具级报错不可得（part.state.status 全 completed）"
        );
        // 项目：目录字段（禁用 project_id —— global 哨兵吞掉 4 个目录）
        let s = delta
            .sessions
            .iter()
            .find(|s| s.session_id == "ses_dup")
            .unwrap();
        assert_eq!(s.project_key, "a");
        assert_eq!(s.project_path_raw, "/p/A");
        // **Minor #6（fix round 1）**：`parsed_files` 对 SQLite 源 = **1 个库文件**
        // （任务书按会话行累加会给出「库里几个会话」→ UI 把它当文件数显示）。
        // 变异：把 `b.parsed_files = 1;` 换回逐会话 `+= 1` → 本断言读到 2 → 红。
        assert_eq!(
            delta.parsed_files, 1,
            "SQLite 源解析的是 1 个库文件（真机 11 个会话也不该报 11）"
        );
    }

    /// **B4 回归**：opencode 是「会话级累计列 + 逐消息行」型源——**必须读游标做增量**：
    /// 四桶取 `state_json` 里的上次累计快照算差值、逐消息/工具 part 按 **rowid 水位**只读新增行。
    /// 旧实现（游标只写不读）第二轮会把整份四桶与全部消息**再加一遍** → 明细是累加语义
    /// → `requests` / 四桶 / `tool_ms` 随采集轮次线性膨胀（默认 10 分钟一轮 + 每次手动采集）。
    /// 三轮断言：首轮有新增 → 第二轮全 0 → 第三轮只计**新增的那一条**。
    #[test]
    fn second_round_is_incremental_and_third_round_counts_only_new_rows() {
        let dir = tempfile::tempdir().unwrap();
        let db = fixture(dir.path());

        // 空游标表必须先绑成变量：`&HashMap::new()` 是**临时值**，借用进 `ctx` 就是
        // E0716（temporary value dropped while borrowed）——语句结束即释放。
        let no_cursors = HashMap::new();
        let ctx1 = CollectContext::new(&db, 1_700_000_000_000, &no_cursors);
        let d1 = OpenCodeCollector::collect_with_db(&ctx1, &db).unwrap();
        assert!(d1.new_records > 0, "首轮必须有新增（否则本用例是空跑）");
        assert!(
            d1.cursors.iter().all(|c| !c.state_json.is_empty()),
            "游标必须带续读状态"
        );
        let cursors: HashMap<_, _> = d1
            .cursors
            .iter()
            .map(|c| (c.session_id.clone(), c.clone()))
            .collect();

        // 第二轮：同一份数据 → 一个字节的新增都不能有
        let ctx2 = CollectContext::new(&db, 1_700_000_000_000 + 60_000, &cursors);
        let d2 = OpenCodeCollector::collect_with_db(&ctx2, &db).unwrap();
        assert_eq!(
            d2.new_records, 0,
            "第二轮不得有新增记录（旧实现会整份重算）"
        );
        assert_eq!(
            d2.details
                .iter()
                .map(|d| d.buckets.input_fresh)
                .sum::<i64>(),
            0,
            "不得重复计入四桶"
        );
        assert_eq!(
            d2.details.iter().map(|d| d.requests).sum::<i64>(),
            0,
            "不得重复计入请求数"
        );
        assert_eq!(
            d2.details.iter().map(|d| d.counters.tool_ms).sum::<i64>(),
            0,
            "不得重复计入工具耗时"
        );

        // 第三轮：追加一条 V2 assistant 消息（含 1 次工具调用）→ 只计这一条
        let conn = Connection::open(&db).unwrap();
        conn.execute(
            "INSERT INTO session_message (id, session_id, type, seq, data) VALUES
                ('v4','ses_dup','assistant',4,?1)",
            [json!({"time":{"created":100,"completed":2600},
                    "model":{"id":"DeepSeek-V4-Flash","providerID":"volcengine-plan"},
                    "tokens":{"input":7,"output":3,"reasoning":0,"cache":{"read":0,"write":0}},
                    "content":[{"type":"tool","name":"bash","time":{"created":2500,"completed":2600}}]})
            .to_string()],
        )
        .unwrap();
        drop(conn);
        let ctx3 = CollectContext::new(&db, 1_700_000_000_000 + 120_000, &cursors);
        let d3 = OpenCodeCollector::collect_with_db(&ctx3, &db).unwrap();
        assert_eq!(d3.new_records, 1, "只新增了 1 行 session_message");
        assert_eq!(
            d3.details.iter().map(|d| d.requests).sum::<i64>(),
            1,
            "只计新增请求"
        );
        assert_eq!(
            d3.details.iter().map(|d| d.counters.tool_ms).sum::<i64>(),
            100,
            "只计新增工具耗时"
        );
        assert_eq!(
            d3.details
                .iter()
                .map(|d| d.buckets.input_fresh)
                .sum::<i64>(),
            0,
            "会话级累计列没变 → 差值为 0"
        );
    }

    /// **M2 回归**：本用例必须**真的走进 immutable 回退分支**。
    /// 旧版只断言「`open_usage_db` 能打开」，那是**假绿**——`mode=ro` 成功时它同样为 `Some`，
    /// immutable 分支零覆盖。
    ///
    /// **构造（Task 14 重造，见报告偏差申报）**：任务书原本要求「WAL 库 + 删掉 `-wal/-shm`」
    /// 后 `mode=ro` **必须返回 None**——实测不成立：`sqlite3_open_v2` 是**惰性**的，
    /// 缺伴随文件的 WAL 库在 `mode=ro` 下同样拿到 `Some`，`SQLITE_CANTOPEN(14)` 只在
    /// **第一句 SQL** 时抛出（本机 rusqlite 0.32 / 内置 sqlite 3.46；同机 sqlite3 CLI 3.54
    /// 的表现与计划注释一致，是**库版本差异**）。按任务书自己写的处理办法「**必须重造场景**」，
    /// 改为把 `-wal` 的位置放一个**目录**（SQLite 打不开 WAL → 与 §9.4「缺 -wal/-shm」同一条
    /// 失败路径：WAL 建不起来 → CANTOPEN 14），且不依赖 chmod / 平台，跨平台可跑。
    /// **前提断言仍是本用例的价值**：ro 路径若某天真的能读了，这里会**响亮失败**并提示重造。
    #[test]
    fn immutable_fallback_opens_wal_db_with_wal_unopenable() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("db.sqlite");
        {
            let conn = Connection::open(&p).unwrap();
            conn.execute_batch(
                "PRAGMA journal_mode=WAL; CREATE TABLE t (a INTEGER); INSERT INTO t VALUES (1);",
            )
            .unwrap();
        } // 连接正常关闭：WAL 标记留在库头（字节 18/19 = 0x02），数据已落在主库文件里
        let _ = std::fs::remove_file(dir.path().join("db.sqlite-wal"));
        let _ = std::fs::remove_file(dir.path().join("db.sqlite-shm"));
        // 让 SQLite 打不开 WAL：`-wal` 位置上放一个目录
        std::fs::create_dir(dir.path().join("db.sqlite-wal")).unwrap();
        // 前提 ①：ro 的**句柄**是惰性打开的（一定拿得到 Some）……
        let ro = crate::monitor::sqlite::open_readonly_with_timeout(&p)
            .expect("`sqlite3_open_v2` 是惰性的：句柄一定开得出来（这正是本用例要立的规矩）");
        // ……前提 ②：但**第一次真正读**必须 SQLITE_CANTOPEN(14) —— 否则本用例已无回退可验
        let e = ro
            .query_row("SELECT count(*) FROM sqlite_master", [], |r| {
                r.get::<_, i64>(0)
            })
            .unwrap_err();
        assert_eq!(
            e.sqlite_error_code(),
            Some(rusqlite::ErrorCode::CannotOpen),
            "构造失效：WAL 打不开的库本应第一句 SQL 就 CANTOPEN(14)，实际 {e:?} → \
             本用例已无回退可验，必须重造场景（不得就此放过）"
        );
        drop(ro);
        // 真正要验证的分支 ①：immutable 能打开，且打开后能读到数据
        let imm = crate::monitor::sqlite::open_readonly_immutable(&p)
            .expect("immutable=1 必须能打开 WAL 建不起来的库（zcode 762 MB 库即此形态）");
        assert_eq!(
            imm.query_row("SELECT COUNT(*) FROM t", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1,
            "immutable 打开后仍要能读到数据"
        );
        // **裁决 A（fix round 1）**：**只读**这条安全性质必须在这把 immutable 句柄上**真的开火**。
        // 上一轮它零覆盖：另一条用例自称覆盖 immutable，实际 `probe_readable` 通过后拿到的是
        // **ro** 句柄（缺伴随文件的 WAL 库在本机内置 sqlite 3.46 下 ro 照样可读）→ 拒写断言
        // 只是把 ro 路径又测了一遍；而本用例当时**只做 SELECT**。变异：把
        // `open_readonly_immutable` 换成普通 `Connection::open`（可写）→ 本断言红。
        assert!(
            imm.execute_batch("CREATE TABLE probe_imm (x INTEGER)")
                .is_err(),
            "immutable=1 路径必须拒写他人库（只读是安全性质，不是实现细节）"
        );
        drop(imm);
        // 真正要验证的分支 ②：组合入口必须**回退到 immutable**（只 or_else 句柄的实现在这里红：
        // 它返回的是上面那把「查什么都 CANTOPEN」的 ro 连接 → query_row 直接 Err）
        let conn = crate::monitor::sqlite::open_usage_db(&p).expect("组合入口必须回退到 immutable");
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM t", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "immutable 打开后仍要能读到数据");
    }

    /// **表探测的软失败是刻意的（命门，不得"顺手"改成 Err）**：`load_sessions` 对 `"session"`
    /// （V1）无条件试一次，而 **2.0.22 全新安装里 `session` 表本就不存在**——那是**正常的代际
    /// 探测**，不是源故障。本用例用「只有 `session_v2` + `session_message`」的库把它钉死：
    /// 把 `load_sessions` 的 `else { return HashSet::new() }` 改成 `?` / `expect` → 本用例红。
    #[test]
    fn v2_only_install_without_v1_session_table_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join(".local/share/opencode/opencode.db");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let conn = Connection::open(&p).unwrap();
        // **刻意不建** `session` / `message` / `part`：V2-only 安装的真实形态
        conn.execute_batch(
            "CREATE TABLE session_v2 (id TEXT PRIMARY KEY, directory TEXT, title TEXT, time_updated INTEGER,
                 model TEXT, tokens_input INTEGER, tokens_output INTEGER, tokens_reasoning INTEGER,
                 tokens_cache_read INTEGER, tokens_cache_write INTEGER);
             CREATE TABLE session_message (id TEXT PRIMARY KEY, session_id TEXT, type TEXT, seq INTEGER, data TEXT);",
        ).unwrap();
        conn.execute(
            "INSERT INTO session_v2 (id, directory, title, time_updated, model, tokens_input, tokens_output,
                tokens_reasoning, tokens_cache_read, tokens_cache_write)
             VALUES ('ses_v2only','/p/C','t',200,?1,500,20,5,300,0)",
            [json!({"id":"DeepSeek-V4-Flash","providerID":"volcengine-plan"}).to_string()],
        ).unwrap();
        conn.execute(
            "INSERT INTO session_message (id, session_id, type, seq, data) VALUES
                ('w1','ses_v2only','user',1,?1), ('w2','ses_v2only','assistant',2,?2), ('w3','ses_v2only','assistant',3,?3)",
            rusqlite::params![
                json!({"time":{"created":40},"text":"hi"}).to_string(),
                json!({"time":{"created":41,"completed":341},"finish":"error",
                       "error":{"type":"provider.auth","message":"bad key"},
                       "model":{"id":"DeepSeek-V4-Flash","providerID":"volcengine-plan"},
                       "tokens":{"input":5,"output":2,"reasoning":1,"cache":{"read":0,"write":0}},
                       "content":[{"type":"tool","name":"bash","time":{"created":300,"completed":400}}]}).to_string(),
                json!({"time":{"created":42,"completed":43},
                       "model":{"id":"DeepSeek-V4-Flash","providerID":"volcengine-plan"},
                       "tokens":{"input":1,"output":1,"reasoning":0,"cache":{"read":0,"write":0}}}).to_string(),
            ]).unwrap();
        drop(conn);
        let no_cursors = HashMap::new();
        let ctx = CollectContext::new(&p, 1_700_000_000_000, &no_cursors);
        let d = OpenCodeCollector::collect_with_db(&ctx, &p)
            .expect("V2-only 安装不得整源失败：V1 `session` 表不存在属代际探测的预期分支");
        assert_eq!(d.details.iter().map(|d| d.requests).sum::<i64>(), 2);
        assert_eq!(d.details.iter().map(|d| d.counters.turns).sum::<i64>(), 1);
        assert_eq!(
            d.details.iter().map(|d| d.buckets.input_fresh).sum::<i64>(),
            500
        );
        assert_eq!(
            d.details.iter().map(|d| d.buckets.cache_read).sum::<i64>(),
            300
        );
        assert_eq!(
            d.details.iter().map(|d| d.buckets.output).sum::<i64>(),
            25,
            "会话级 output 20 + reasoning 5"
        );
        // V2 的 error_turn 判据 = `finish=="error"`（真机 3 条，与 `$.error` 并存）
        assert_eq!(
            d.details.iter().map(|d| d.counters.error_turn).sum::<i64>(),
            1
        );
        assert_eq!(
            d.details
                .iter()
                .map(|d| d.counters.error_model)
                .sum::<i64>(),
            1,
            "w2 的 $.error"
        );
        assert_eq!(
            d.details.iter().map(|d| d.counters.tool_ms).sum::<i64>(),
            100
        );
        assert_eq!(
            d.sessions.len(),
            1,
            "V2-only 库里的会话必须照常登记（软失败 ≠ 丢会话）"
        );
    }

    /// **降级路径（桩不得只覆盖成功路径）**：库打不开时必须**整源响亮失败**
    /// （`usage-source-db-open` → `UsageSourceStatus.errorCode`），不得静默返回空集
    /// ——否则用户在 UI 上看到的是「这个源没有用量」而不是「这个源读不了」（W-06 补充条款）。
    #[test]
    fn unopenable_db_is_a_loud_source_error() {
        let dir = tempfile::tempdir().unwrap();
        let no_cursors = HashMap::new();
        // ① 路径不存在（`collect()` 里被 `exists()` 早退，但 Task 15/16 直调注入路径时走这里）
        let missing = dir.path().join("nope/opencode.db");
        let ctx = CollectContext::new(dir.path(), 1_700_000_000_000, &no_cursors);
        let e = OpenCodeCollector::collect_with_db(&ctx, &missing)
            .expect_err("打不开的库必须整源 Err，不得静默空集");
        assert_eq!(e.code, "usage-source-db-open");
        // ② 路径是目录：ro 与 immutable 都打不开 → 同样整源 Err
        let ctx2 = CollectContext::new(dir.path(), 1_700_000_000_000, &no_cursors);
        assert!(
            OpenCodeCollector::collect_with_db(&ctx2, dir.path()).is_err(),
            "目录路径同样必须整源 Err（open_usage_db 两条路都失败）"
        );
        // 前提断言（防假绿）：同一个夹具形状下，**合法库**必须成功——证明上面的 Err 来自
        // 「打不开」，不是被某种无关的早退吃掉的
        let db = fixture(dir.path());
        let ctx3 = CollectContext::new(&db, 1_700_000_000_000, &no_cursors);
        assert!(OpenCodeCollector::collect_with_db(&ctx3, &db).is_ok());
    }

    /// **A-3 锁（P2-1 的根因；宿主已核实）**：**NOTADB / 损坏库**必须让该源**响亮失败**
    /// —— `ok=false` + `errorCode = "usage-source-db-open"`，**不得** `ok=true, new_records=0`
    /// （后者在 UI 上表现为「这个源没有用量」，用户永远看不到「这个源读不了」）。
    ///
    /// 根因：`open_usage_db` 的 **immutable 回退分支**原先**没有探针**——`sqlite3_open_v2` 是
    /// 惰性的，NOTADB 文件在 `?immutable=1` 下**照样拿得到句柄**（第一句 SQL 才抛 NOTADB）；
    /// 而 opencode 的每个查询点都是软失败（`unwrap_or(false)` / `let ... else`）⇒ 静默 0。
    ///
    /// **能变红**（已实测）：去掉 immutable 分支的 `.filter(probe_readable)` → 本用例 FAILED。
    #[test]
    fn notadb_file_is_a_loud_source_error_not_a_silent_zero() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("opencode.db");
        std::fs::write(&db, b"this file is definitely not a sqlite database\n").unwrap();
        // 前提（防假绿）：ro 句柄是**惰性**开的，但第一句 SQL 必须失败——否则测不到 NOTADB
        let ro = crate::monitor::sqlite::open_readonly_with_timeout(&db)
            .expect("惰性句柄一定开得出来（这正是 A-3 的根因）");
        let e = ro
            .query_row("SELECT count(*) FROM sqlite_master", [], |r| {
                r.get::<_, i64>(0)
            })
            .expect_err("构造失效：该文件居然是可读的 SQLite 库");
        assert_eq!(
            e.sqlite_error_code(),
            Some(rusqlite::ErrorCode::NotADatabase),
            "构造失效：本用例要的是 NOTADB，实际 {e:?}"
        );
        drop(ro);
        // 前提②：immutable 回退分支同样**能开出句柄**（惰性）——修好之后它必须被探针挡掉
        assert!(
            crate::monitor::sqlite::open_readonly_immutable(&db).is_some(),
            "构造失效：immutable 分支若连句柄都开不出来，本用例就测不到那条漏掉探针的路径"
        );
        let no_cursors = HashMap::new();
        let ctx = CollectContext::new(dir.path(), 1_700_000_000_000, &no_cursors);
        let e = OpenCodeCollector::collect_with_db(&ctx, &db)
            .expect_err("NOTADB 必须整源 Err（ok=false），不得静默 ok=true / new_records=0");
        assert_eq!(
            e.code, "usage-source-db-open",
            "损坏库的错误码必须是 usage-source-db-open（错误详情：{}）",
            e.detail
        );
    }

    /// **A-3 连带锁（「不可能触发的守卫」）**：表探测必须能区分
    /// 「**表不存在**」（V2-only 安装里试 V1 表 = **预期**分支，不告警）与
    /// 「**探测本身失败**」（NOTADB / 损坏 / 锁死 / 权限 —— **必须响亮**）。
    ///
    /// 修前 `table_exists` 是 `.unwrap_or(false)`：NOTADB 连接上它恒 `false`
    /// ⇒ `load_sessions` 里那条「表在场却查不动」的 `log::warn!` **永远不会响**
    /// （告警条件 `table_exists(..) == true` 与触发前提「探测失败」在 NOTADB 上互斥
    /// = 一个**不可能触发**的守卫）。
    ///
    /// **能变红**：把 `table_state` 的 `Err` 分支改回 `Absent`（旧的 `.unwrap_or(false)` 语义）
    /// → 第二段断言 FAILED（已实测）。
    #[test]
    fn table_probe_distinguishes_absent_from_probe_failure() {
        // ① 合法库：在场 / 确定不存在两态分得开（V2-only 安装的预期分支）
        let v2_only = Connection::open_in_memory().unwrap();
        v2_only
            .execute_batch("CREATE TABLE session_v2 (id TEXT PRIMARY KEY);")
            .unwrap();
        assert!(
            matches!(table_state(&v2_only, "session_v2"), TableState::Present),
            "前提：V2 表必须在场"
        );
        assert!(
            matches!(table_state(&v2_only, "session"), TableState::Absent),
            "V2-only 安装里 V1 的 `session` 表**确定不存在** = 预期分支（不告警）"
        );
        // 旧的布尔便捷形态 `table_exists` 已删除（它会 `ProbeFailed → false`）——该断言的
        // 语义等价物就是上面这一条 `Absent`，而「不得再压回 bool」由源码级自省锁
        // `production_half_never_folds_the_table_probe_back_to_bool` 承担（覆盖**全部**调用点）。
        // ② NOTADB：探测**本身失败** → 必须是 ProbeFailed（响亮），不得伪装成「表不存在」
        let dir = tempfile::tempdir().unwrap();
        let bad = dir.path().join("bad.db");
        std::fs::write(&bad, b"not a database at all\n").unwrap();
        let conn = crate::monitor::sqlite::open_readonly_immutable(&bad)
            .expect("immutable 惰性句柄（A-3 的根因构造）");
        match table_state(&conn, "session") {
            TableState::ProbeFailed(e) => assert_eq!(
                e.sqlite_error_code(),
                Some(rusqlite::ErrorCode::NotADatabase),
                "损坏库的探测失败原因必须原样带出来（不能只给一个 bool）"
            ),
            other => panic!(
                "NOTADB 上探测本身失败 → 必须是 ProbeFailed（响亮），实际 {other:?} \
                 —— 退回 `.unwrap_or(false)` 语义则那条告警永不触发"
            ),
        }
    }

    /// **判据面由共享实现给出**（第 5 轮：显式哨兵标记，删掉全部「结构推断」；两份手抄判据
    /// 一并下岗 —— 见 `crate::services::usage::self_lock`）。
    ///
    /// 面 = `self_lock::scan_face("collectors/opencode.rs", include_str!("opencode.rs"))`
    /// = 文件头 → 排除区 BEGIN 之前 **∪** END 之后 → 文件尾（去行内空白、剥**整行** `//` 注释）。
    /// 本文件的标记**包住测试模块**（BEGIN 在 `#[cfg(test)]` 前一行、END 在测试模块收口 `}` 之后）
    /// ⇒ 面 = **生产半边**，且**测试模块之后追加的生产代码也在面内**（第 4 轮「收口于 EOF」判据
    /// 正是在那里漏掉 `R8` 型构造）。带**源码行号**是为了让失败信息指向源码行。
    ///
    /// `TableState` 的 doc 明文写着「调用方必须显式处理 `ProbeFailed`（响亮），不得再压回 bool」，
    /// 而修前有一支布尔便捷形态 `table_exists`（`ProbeFailed → false`）、**三处调用点全走它**
    /// ⇒ 那条要求当时只是**纸面纪律**（P1 复核实证：`!table_exists(..)` 在探测失败时会误报
    /// 「表不存在」，把库损坏/锁死/权限说成该代际的预期缺表）。本锁把它变成机械纪律。
    ///
    /// **判据（第三轮复审后收尾修正：由名字级升级为形态级；最后一批复审 P2-1：全称补严）**：
    /// ① 名字级：生产半边不得出现 `table_exists(`（布尔便捷形态的名字）；
    /// ② **形态级**：**每一个 `table_state` 名字出现处**（**不带括号**的全名 + **词边界**，
    ///    排除它自己的定义行）的 **5 行窗口**内必须出现 `TableState::ProbeFailed`。
    ///    只钉名字时，`let has_v2 = matches!(table_state(&conn, "session_v2"), TableState::Present);`
    ///    这种**无告警折叠**照样全绿（第三轮评审的**变异 D** 实测假绿），`ProbeFailed` 重新静默；
    ///    形态级判据把它打红（已实测，见 `FINAL-FIX-report.md`）。
    ///    **最后一批复审 P2-1（别名绕过，EXP-7 实测）**：旧针是**字面量** `table_state(`，
    ///    于是 `let probe = table_state; matches!(probe(&conn, "session_v2"), TableState::Present)`
    ///    **不进普查**（而 `sites >= 3` 仍被其余合法调用点满足）⇒「每一个调用点」这个**全称失效**。
    ///    改扫**不带括号的名字** + 词边界后，别名形态同样进普查（改名/取引用都绕不过「名字在场」）。
    /// ③ 正向：两种"不在场"话术、三态臂、以及**调用点数 ≥ 3** 必须仍在场——
    ///    防「零调用点 / 只留一个」时 ② 退化成恒真。
    ///
    /// **扫描面 / 判据层级 / 反例**（台账 C-【Important-1】的登记原文；最后一批复审后补全）：
    /// * **扫描面** = 本文件的**生产半边**，由**显式哨兵标记**划出（`self_lock::scan_face`：
    ///   文件头 → BEGIN 之前 **∪** END 之后 → 文件尾；去行内空白、剥**整行** `//` 注释）。
    ///   **第 5 轮起不再有任何结构推断**（旧「精确 `mod tests` + 大括号配口于 EOF」判据已删）——
    ///   随之消失的致盲形态见 `self_lock` 模块文档与台账 C-【Important-1】。
    /// * **判据层级** = **形态级但非语义级**：只要求调用点附近**指名** `ProbeFailed`；
    ///   **不校验**该分支真的告警、也**不校验** 5 行窗口里的 `ProbeFailed` 属于同一个 match。
    /// * **反例（抓不到的回归）** = ①写一个**点名 `ProbeFailed` 却不告警**的包装（如
    ///   `TableState::ProbeFailed(_) => false` 静默折叠，或窗口内另有一处无关的 `ProbeFailed`
    ///   文本）——本锁抓不到，它需要**行为锁 / 日志捕获设施**（仓内没有 ⇒ 与 W-37 同族，
    ///   见台账【B-5】登记）；②把折叠搬进 `mod tests` / 真测试模块**之后**；
    ///   ③**行尾注释**或字符串里写字面量形态（`code_lines` 只剥**整行**注释）；
    ///   ④在 5 行窗口**之外**（>5 行）另起折叠逻辑。反向地，`_ => false` 这类 catch-all 折叠
    ///   在本判据下**是红的**（窗口里没有 `ProbeFailed` 这个名字）。
    ///   ⑤**窗口内有无关的 `ProbeFailed` 文本**（R7 实测）：只要 5 行窗口里**任意一处**出现
    ///   `TableState::ProbeFailed` 字样（**哪怕在字符串里**、哪怕属于另一个 `match`），静默折叠就
    ///   **不会被判红** ⇒ 这是**过松（假阴性）**方向，**不是**误报方向（第 4 轮登记把它标反了，
    ///   第 5 轮按复审实测更正）。构造：把 `:78-88` 的三态 `match` 换成
    ///   `let has_v2 = matches!(table_state(&conn, "session_v2"), TableState::Present);`
    ///   **并在其后 5 行内塞一句含 `ProbeFailed` 的字符串**（如 `log::warn!("{e} ProbeFailed")`）
    ///   ⇒ 本锁全绿，而 `ProbeFailed` 事实上已被折叠掉。**判据层级 = 形态级，改窗口解决不了**。
    /// * **反例（会误报的情形）** = ⑥**名字级普查的过计**：任何含 `table_state` 字样、后一个字符又不是标识符字符的**非调用
    ///   形态**（`use …::table_state;`、类型别名、非整行注释里的说明）都会被当成一户 ⇒ 它附近
    ///   5 行内没有 `ProbeFailed` 就**假红**（响亮、安全方向；`x_table_state` / `table_stateful`
    ///   由词边界挡住，不算户）；
    ///   ⑦**边界事实**：窗口 = `prod[i..(i+5)]`，**整行注释不占槽**（已被剥掉）、**空行占槽**
    ///   ⇒ 源码 `:320` 的调用点距 `ProbeFailed` 是 **4/5**，**在其后插一个空行即假红**——
    ///   **刻意不改窗口大小**（改大只会更松、改小会误报现状）。
    ///
    /// 针一律 `concat!` 拆开写（否则本用例自己的字面量命中自己）；只看**生产半边 + 去注释**
    /// ——注释里为说明历史确实写了旧形态的名字，不看注释才谈得上机械纪律。
    #[test]
    fn production_half_never_folds_the_table_probe_back_to_bool() {
        let prod = crate::services::usage::self_lock::scan_face(
            "collectors/opencode.rs",
            include_str!("opencode.rs"),
        );
        let code: String = prod.iter().map(|(_, l)| l.as_str()).collect();
        // ① 名字级：布尔便捷形态不得存在（它正是「压回 bool」的载体）
        let forbidden = concat!("table_", "exists(");
        assert!(
            !code.contains(forbidden),
            "生产半边不得出现 `{forbidden}`：三态探测的调用方必须**显式 match**——\
             压回 bool 会让 `ProbeFailed`（库损坏 / 锁死 / 权限）伪装成「表不存在」（误报），\
             正是 A-3 三态要消灭的那类静默"
        );
        // ② 形态级：**每一个 `table_state` 名字出现处**（不带括号 + 词边界）的 5 行窗口内
        //    必须**指名** `ProbeFailed`——扫名字而非字面量 `table_state(`，别名形态才绕不过去
        let needle = concat!("table_", "state");
        let failed = concat!("TableState::", "ProbeFailed");
        let mut sites = 0usize;
        for (i, (ln, text)) in prod.iter().enumerate() {
            for (at, _) in text.match_indices(needle) {
                // 词边界：**只看后一个字符**（`code_lines` 已去掉全部空白，所以 `match table_state(`
                // 会变成 `matchtable_state(`——前一个字符永远是标识符字符，不可用作边界判据；
                // 后一个字符则稳定：调用/取用时是 `(` `,` `;` `)`，而 `table_stateful` 是 `f`）
                let after = text[(at + needle.len())..].chars().next();
                if after.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_') {
                    continue;
                }
                // 定义行（`fn table_state(`）不是调用点
                if text[..at].ends_with("fn") {
                    continue;
                }
                sites += 1;
                let window: String = prod[i..(i + 5).min(prod.len())]
                    .iter()
                    .map(|(_, l)| l.as_str())
                    .collect();
                assert!(
                    window.contains(failed),
                    "`opencode.rs:{ln}`（源码行号）的 `{needle}` 出现处附近（5 行内）没有 \
                     `{failed}`：三态探测的任何调用点 / 别名（`let probe = table_state;`）都不得\
                     **无告警折叠**（`matches!(…)` / `_ => false` / 再包一个返回 bool 的便捷形态）\
                     ——那会让「探测本身失败」重新静默，正是 A-3 三态、第三轮评审变异 D 与\
                     最后一批复审 EXP-7（别名绕过普查）的失败模式"
                );
            }
        }
        assert!(
            sites >= 3,
            "生产半边只找到 {sites} 处 `{needle}`（应 ≥ 3：`has_v2` gate、\
             `warn_if_table_missing`、`warn_if_table_unreadable`）——调用点被删空时上面的\
             形态级断言会退化成恒真"
        );
        // ③ 正向：两种"不在场"必须各有一句可区分的话术，且三态必须都还在场
        // （针里**不得带空格**：`code` 已去掉全部空白）
        for required in [
            concat!("warn_if_table", "_missing("),
            concat!("TableState::", "Absent"),
            concat!("TableState::", "ProbeFailed"),
            concat!("TableState::", "Present"),
        ] {
            assert!(
                code.contains(required),
                "生产半边必须仍有 `{required}`：缺了它，「表不存在」与「探测本身失败」\
                 就无法区分 / 无法被显式处理"
            );
        }
    }

    /// **A-3 复核锁（宿主点名的取舍复核，写进提交信息的那一条）**：
    /// 给 immutable 分支补 `.filter(probe_readable)` **不会**破坏既定的
    /// 「**BUSY → immutable 读旧值**」取舍 —— 理由：`immutable=1` **跳过全部加锁**，
    /// 另一连接持写锁时它的 `sqlite_master` 探针**照样通过**。
    ///
    /// 本用例把这条论证变成可执行断言（否则后来者会以为加探针改了行为）：
    /// ① 造一把**真**写锁（另一连接 `BEGIN EXCLUSIVE` 未提交）——
    ///    **前提断言**：此时 mode=ro + `busy_timeout(1000)` 的探针必须失败（回退的触发条件）；
    /// ② `open_usage_db` 仍必须**成功**（走 immutable），且拿到的是**能读**的连接。
    ///
    /// **能变红**：把 `open_usage_db` 的 immutable 回退去掉（或改成再走一次 mode=ro）
    /// → ② 断言 FAILED（该源会在别进程写入期间整轮 `ok=false`，正是取舍要避免的代价）。
    #[test]
    fn busy_write_lock_still_falls_back_to_immutable_with_the_probe_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("busy.db");
        {
            let c = Connection::open(&p).unwrap();
            c.execute_batch("CREATE TABLE t (a INTEGER); INSERT INTO t VALUES (1);")
                .unwrap();
        }
        // 另一连接持**排他**写锁且不提交（rollback-journal 模式下 EXCLUSIVE 会挡住读者）
        let writer = Connection::open(&p).unwrap();
        writer
            .execute_batch("BEGIN EXCLUSIVE; INSERT INTO t VALUES (2);")
            .unwrap();
        // 前提（防假绿）：ro 探针必须**失败**——否则本用例根本没测到 BUSY 那条路
        let ro_ok = crate::monitor::sqlite::open_readonly_with_timeout(&p)
            .map(|c| {
                c.query_row("SELECT count(*) FROM sqlite_master", [], |r| {
                    r.get::<_, i64>(0)
                })
                .is_ok()
            })
            .unwrap_or(false);
        assert!(
            !ro_ok,
            "构造失效：写锁期间 ro 探针竟然通过 → 本用例没走到 BUSY 回退那条路（必须重造场景）"
        );
        // ① immutable 探针**照样通过**（immutable=1 不加锁）——这是"加探针不改行为"的关键
        let imm_probe = crate::monitor::sqlite::open_readonly_immutable(&p)
            .expect("immutable 句柄（惰性）")
            .query_row("SELECT count(*) FROM sqlite_master", [], |r| {
                r.get::<_, i64>(0)
            });
        assert!(
            imm_probe.is_ok(),
            "immutable=1 跳过加锁 ⇒ 写锁期间探针必须照样通过；若这里失败，\
             说明探针把 immutable 分支也打死了（取舍被破坏）：{imm_probe:?}"
        );
        // ② 组合入口仍必须成功（BUSY → immutable 读旧值的取舍被保住）
        let conn = crate::monitor::sqlite::open_usage_db(&p)
            .expect("写锁期间必须仍能回退到 immutable（否则该源整轮 ok=false）");
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM t", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1,
            "immutable 读到的是 checkpoint 之前的旧值（**这是既定取舍**，不是回归）"
        );
        drop(conn);
        writer.execute_batch("ROLLBACK").unwrap();
    }

    /// **只读纪律锁**：用量采集打开的是**他人的库**（opencode.db），两条路径（`mode=ro` 与
    /// `immutable=1`）都必须拒写。
    ///
    /// **裁决 A（fix round 1）**：上一版 ② 段写「immutable=1 路径（缺 -wal/-shm 的 WAL 库）」是**假覆盖**——
    /// 缺伴随文件的 WAL 库在本机内置 sqlite 3.46 下 `mode=ro` **照样可读**（探针实测），
    /// `probe_readable` 通过 → `open_usage_db` 返回的是 **ro** 句柄，immutable 分支根本没走到。
    /// 本版把 ② 段改成与 fallback 用例同款的 **wal-as-directory** 构造（唯一真正落到 immutable 的形态），
    /// 并**先断言前提**（该构造下 ro 探针确实失败）；③ 段再**直接**对健康库取一把 immutable 句柄做拒写断言——
    /// 这样「immutable 拒写」这条性质既在回退路径上、也在健康库上各有一把会开火的锁。
    #[test]
    fn usage_db_is_opened_read_only_on_both_paths() {
        let dir = tempfile::tempdir().unwrap();
        // ① mode=ro 路径（普通库，非 WAL）
        let plain = dir.path().join("plain.db");
        {
            let conn = Connection::open(&plain).unwrap();
            conn.execute_batch("CREATE TABLE t (a INTEGER); INSERT INTO t VALUES (1);")
                .unwrap();
        }
        let ro = crate::monitor::sqlite::open_usage_db(&plain).expect("普通库必须能打开");
        assert_eq!(
            ro.query_row("SELECT COUNT(*) FROM t", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert!(
            ro.execute_batch("CREATE TABLE probe_ro (x INTEGER)")
                .is_err(),
            "mode=ro 路径必须拒写他人库"
        );
        drop(ro);
        // ② immutable=1 回退路径（**真** immutable：`-wal` 位置上放一个目录 → ro 探针必然失败）
        let wal = dir.path().join("wal.db");
        {
            let conn = Connection::open(&wal).unwrap();
            conn.execute_batch(
                "PRAGMA journal_mode=WAL; CREATE TABLE t (a INTEGER); INSERT INTO t VALUES (1);",
            )
            .unwrap();
        }
        let _ = std::fs::remove_file(dir.path().join("wal.db-wal"));
        let _ = std::fs::remove_file(dir.path().join("wal.db-shm"));
        std::fs::create_dir(dir.path().join("wal.db-wal")).unwrap();
        // 前提（防假绿）：这条构造下 ro 探针**必须**失败，否则测到的还是 ro 句柄
        let ro_would_work = crate::monitor::sqlite::open_readonly_with_timeout(&wal)
            .map(|c| {
                c.query_row("SELECT count(*) FROM sqlite_master", [], |r| {
                    r.get::<_, i64>(0)
                })
                .is_ok()
            })
            .unwrap_or(false);
        assert!(
            !ro_would_work,
            "构造失效：此构造下 ro 仍可读 → 本段测到的不是 immutable 句柄（必须重造场景）"
        );
        let imm =
            crate::monitor::sqlite::open_usage_db(&wal).expect("缺伴随文件的 WAL 库必须能打开");
        assert_eq!(
            imm.query_row("SELECT COUNT(*) FROM t", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1,
            "回退到 immutable 后必须真能读到数据"
        );
        assert!(
            imm.execute_batch("CREATE TABLE probe_imm (x INTEGER)")
                .is_err(),
            "immutable=1 回退路径必须拒写他人库"
        );
    }

    /// **裁决 A（fix round 1）·只读性质与「库打不开」解耦**：上一轮「immutable 拒写」的变异归属是**假的**
    /// ——当时唯一拿到 immutable 句柄的用例先跑 `query_row(...).unwrap()`，若把那把句柄换成可写句柄，
    /// 它会**先在读断言上红**（那条构造里 WAL 打不开，读本身失败），写断言根本没机会开火。
    /// 本用例只在**健康库**上取一把 immutable 句柄：读必须成功（前提防假绿）→ 写必须被拒。
    /// 变异（实测红）：`open_readonly_immutable` 换成 `Connection::open` 式的可写打开 →
    /// 读断言仍绿（健康库读得到），**只有**下面这条拒写断言红。
    #[test]
    fn immutable_handle_is_read_only_even_on_a_healthy_db() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("healthy.db");
        {
            let conn = Connection::open(&p).unwrap();
            conn.execute_batch("CREATE TABLE t (a INTEGER); INSERT INTO t VALUES (1);")
                .unwrap();
        }
        let imm = crate::monitor::sqlite::open_readonly_immutable(&p)
            .expect("immutable=1 必须能打开健康库");
        // 前提（防假绿）：这把句柄真的**可读**——否则「写被拒」可能只是因为库根本打不开
        assert_eq!(
            imm.query_row("SELECT COUNT(*) FROM t", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1,
            "前提：immutable 句柄在健康库上必须读得到数据"
        );
        assert!(
            imm.execute_batch("CREATE TABLE probe_imm_healthy (x INTEGER)")
                .is_err(),
            "immutable=1 句柄在**健康库**上同样必须拒写（只读是安全性质，不是「库打不开」的副产品）"
        );
    }

    /// **GC 5 豁免锁**：SQLite 源（opencode/zcode/workbuddy）**豁免 L2/L3**、只受 L1 约束——
    /// 它不得走 `CollectContext::read_incremental`（那是文件型源的一遍扫描入口）。
    /// 用 `stats().reads` 断言（W-15：只断言「值对了」不等于「路径对了」）。
    #[test]
    fn sqlite_source_never_touches_the_file_read_entry() {
        let dir = tempfile::tempdir().unwrap();
        let db = fixture(dir.path());
        let no_cursors = HashMap::new();
        let ctx = CollectContext::new(&db, 1_700_000_000_000, &no_cursors);
        let _ = OpenCodeCollector::collect_with_db(&ctx, &db).unwrap();
        let stats = ctx.stats();
        assert_eq!(
            stats.reads, 0,
            "SQLite 源豁免 L2/L3（GC 5）：不得经 read_incremental 读文件"
        );
        assert_eq!(stats.repeat_reads, 0);
    }

    /// **Minor #5（fix round 1）**：两条判据在上一版里是「死行 / 零覆盖」——
    /// ① 「带 `tokens` 的 assistant 即计一次请求（**哪怕四桶全 0**）」：任务书为它造的 `m3` 属
    ///    `ses_dup`，被 V2 代际去重后**永不读取**（`m1/m2/m3/p1` 都是死行）→ **V1 侧零覆盖**；
    /// ② `interrupted` 分支（V1 `MessageAbortedError` / V2 `contains("aborted")`）**全库无一条断言**
    ///    ——而真机有 **5 条 aborted**、`caps` 也声明 `interrupted: true`。
    /// 本用例在两代各造一条，把两个判据一起钉死（真机能红的变异：把 aborted 并进 `error_model`）。
    #[test]
    fn interrupted_and_all_zero_request_rows_are_counted_on_both_generations() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join(".local/share/opencode/opencode.db");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let conn = Connection::open(&p).unwrap();
        conn.execute_batch(
            "CREATE TABLE session (id TEXT PRIMARY KEY, directory TEXT, title TEXT, time_updated INTEGER,
                 model TEXT, tokens_input INTEGER, tokens_output INTEGER, tokens_reasoning INTEGER,
                 tokens_cache_read INTEGER, tokens_cache_write INTEGER);
             CREATE TABLE session_v2 (id TEXT PRIMARY KEY, directory TEXT, title TEXT, time_updated INTEGER,
                 model TEXT, tokens_input INTEGER, tokens_output INTEGER, tokens_reasoning INTEGER,
                 tokens_cache_read INTEGER, tokens_cache_write INTEGER);
             CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, data TEXT);
             CREATE TABLE session_message (id TEXT PRIMARY KEY, session_id TEXT, type TEXT, seq INTEGER, data TEXT);
             CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT, data TEXT);",
        )
        .unwrap();
        let model = json!({"id":"DeepSeek-V4-Flash","providerID":"volcengine-plan"}).to_string();
        conn.execute(
            "INSERT INTO session_v2 (id, directory, title, time_updated, model, tokens_input, tokens_output,
                tokens_reasoning, tokens_cache_read, tokens_cache_write)
             VALUES ('ses_v2x','/p/V2','t',100,?1,100,10,0,0,0)",
            [&model],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session_message (id, session_id, type, seq, data) VALUES
                ('u1','ses_v2x','user',1,?1), ('a1','ses_v2x','assistant',2,?2), ('a2','ses_v2x','assistant',3,?3)",
            rusqlite::params![
                json!({"time":{"created":10},"text":"hi"}).to_string(),
                // 四桶全 0 的一次模型响应 → 仍是一次请求
                json!({"time":{"created":11,"completed":12},"model":{"id":"DeepSeek-V4-Flash","providerID":"volcengine-plan"},
                       "tokens":{"input":0,"output":0,"reasoning":0,"cache":{"read":0,"write":0}}}).to_string(),
                // 用户主动打断 → interrupted（**不是** error_model）
                json!({"time":{"created":13,"completed":14},
                       "model":{"id":"DeepSeek-V4-Flash","providerID":"volcengine-plan"},
                       "error":{"type":"provider.aborted","message":"aborted by user"},
                       "tokens":{"input":5,"output":1,"reasoning":0,"cache":{"read":0,"write":0}}}).to_string(),
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session (id, directory, title, time_updated, model, tokens_input, tokens_output,
                tokens_reasoning, tokens_cache_read, tokens_cache_write)
             VALUES ('ses_v1x','/p/V1','t',90,?1,50,20,0,0,0)",
            [&model],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message (id, session_id, time_created, data) VALUES
                ('ub','ses_v1x',1,?1), ('b1','ses_v1x',2,?2), ('b2','ses_v1x',3,?3), ('b3','ses_v1x',4,?4)",
            rusqlite::params![
                json!({"role":"user","time":{"created":1}}).to_string(),
                // 四桶全 0 的 assistant（**V1 独有路径**，上一版这条判据是死行）
                json!({"role":"assistant","finish":"stop","tokens":{"total":0,"input":0,"output":0,
                       "reasoning":0,"cache":{"read":0,"write":0}},"time":{"created":2,"completed":3}}).to_string(),
                // V1 的用户打断（`$.error.name == "MessageAbortedError"`）
                json!({"role":"assistant","error":{"name":"MessageAbortedError","data":{"message":"aborted"}},
                       "tokens":{"total":6,"input":6,"output":0,"reasoning":0,"cache":{"read":0,"write":0}},
                       "time":{"created":3,"completed":4}}).to_string(),
                // V1 的模型层报错（对照：不得被算成 interrupted）
                json!({"role":"assistant","error":{"name":"APIError","data":{"message":"boom"}},
                       "tokens":{"total":6,"input":6,"output":0,"reasoning":0,"cache":{"read":0,"write":0}},
                       "time":{"created":4,"completed":5}}).to_string(),
            ],
        )
        .unwrap();
        drop(conn);
        let delta = collect(&p);
        // 请求数：V2 的 a1+a2（2）+ V1 的 b1+b2+b3（3）= 5（四桶全 0 的两条都在内）
        assert_eq!(
            delta.details.iter().map(|d| d.requests).sum::<i64>(),
            5,
            "带 tokens 的 assistant 就是一次请求——四桶全 0 也算（V1/V2 两侧都要覆盖）"
        );
        assert_eq!(
            delta.details.iter().map(|d| d.counters.turns).sum::<i64>(),
            2
        );
        assert_eq!(
            delta
                .details
                .iter()
                .map(|d| d.counters.interrupted)
                .sum::<i64>(),
            2,
            "两代的用户打断各 1 条（V1 MessageAbortedError / V2 type 含 aborted）"
        );
        assert_eq!(
            delta
                .details
                .iter()
                .map(|d| d.counters.error_model)
                .sum::<i64>(),
            1,
            "只有 V1 的 APIError 是模型层报错（打断不得混入）"
        );
    }

    /// **Minor #4（fix round 1，D-23 同族）**：`time_updated` **缺字段（NULL）**与「**真的是 0**」
    /// 必须区分——`unwrap_or(0)` 会把缺字段伪装成一个合法时间戳 0 → 落 **1970 桶**、
    /// `last_seen_at = 0`（正是 D-23 尾部点名的「把失败伪装成合法值」那一族）。
    /// 缺字段 → 按**本轮采集时刻**归桶（并 `log::warn!`）；真的是 0 → 照 0 走（1970 是**事实**）。
    /// 变异：把 `match s.time_updated { … }` 改回 `unwrap_or(0)` → 第一条断言红。
    #[test]
    fn null_time_updated_is_not_disguised_as_epoch_zero() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join(".local/share/opencode/opencode.db");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let conn = Connection::open(&p).unwrap();
        conn.execute_batch(
            "CREATE TABLE session_v2 (id TEXT PRIMARY KEY, directory TEXT, title TEXT, time_updated INTEGER,
                 model TEXT, tokens_input INTEGER, tokens_output INTEGER, tokens_reasoning INTEGER,
                 tokens_cache_read INTEGER, tokens_cache_write INTEGER);
             CREATE TABLE session_message (id TEXT PRIMARY KEY, session_id TEXT, type TEXT, seq INTEGER, data TEXT);",
        )
        .unwrap();
        let model = json!({"id":"DeepSeek-V4-Flash","providerID":"volcengine-plan"}).to_string();
        conn.execute(
            "INSERT INTO session_v2 (id, directory, title, time_updated, model, tokens_input, tokens_output,
                tokens_reasoning, tokens_cache_read, tokens_cache_write)
             VALUES ('ses_nots','/p/N','t',NULL,?1,10,1,0,0,0),
                    ('ses_zero','/p/Z','t',0,?1,20,2,0,0,0)",
            [&model],
        )
        .unwrap();
        drop(conn);
        let delta = collect(&p);
        let nots = delta
            .details
            .iter()
            .find(|d| d.key.session_id == "ses_nots")
            .expect("缺 time_updated 的会话仍要出明细行（不得整条丢掉）");
        // **fix round 2（裁决 A）**：期望值一律**现算**，不得写死 `"1970-01-01"`——
        // `hour_key_of_host` 走宿主本地时区，epoch 0 在 UTC 以西的宿主上落的是
        // **前一天的本地日**（`1969-12-31`）。写死它会让：① 那些机器上全量测试变红（把「零新增失败」
        // 这条门禁口径变成机器相关）；② 更糟——`assert_ne!(…, "1970-01-01")` 在那些机器上
        // **对错误实现也是绿的**（epoch 0 的键恰好不等于写死的那个值 → 假绿）。
        let now_ts = 1_700_000_000_000i64;
        let now_day = day_key_of_host(now_ts);
        let epoch_zero_day = day_key_of_host(0);
        assert_ne!(
            now_day, epoch_zero_day,
            "前提（防假绿）：本用例的两个候选本地日必须不同，否则下面的断言不可伪证"
        );
        assert_eq!(
            nots.key.day_key, now_day,
            "缺字段不得伪装成合法时间戳 0（应回落到本轮采集时刻所在的本地日）"
        );
        assert_ne!(
            nots.key.day_key, epoch_zero_day,
            "缺字段不得落 epoch 0 所在的本地日（该日随宿主时区而变，故现算而不是写死 1970-01-01）"
        );
        assert_eq!(
            nots.key.hour_key,
            hour_key_of_host(now_ts),
            "缺字段按本轮采集时刻归桶"
        );
        let s_nots = delta
            .sessions
            .iter()
            .find(|s| s.session_id == "ses_nots")
            .unwrap();
        assert_eq!(
            s_nots.last_seen_at, 1_700_000_000_000,
            "last_seen_at 不得被伪装成 0"
        );
        // 对照（防假绿）：`time_updated` **真的是 0** 的会话，落 epoch 0 的本地日是**事实**，不是伪装
        let zero = delta
            .details
            .iter()
            .find(|d| d.key.session_id == "ses_zero")
            .expect("真的是 0 的会话也要出明细行");
        assert_eq!(
            zero.key.day_key, epoch_zero_day,
            "真的是 0 → 落在 epoch 0 所在的本地日（现算；不得与缺字段混为一谈）"
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
            ids.contains(&UsageSourceId::OpenCode),
            "opencode 采集器必须登记进 collectors::all()，实际登记：{ids:?}"
        );
    }
}
// ==== usage 自省锁：排除区结束，勿删勿复制 ====
