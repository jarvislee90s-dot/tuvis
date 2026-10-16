//! dsh 采集器（**投影缓存 + 原始 zstd 日志**双数据源，说明书 §5.1 / §9.5 / 矩阵 §6.2）。
//!
//! * **投影缓存** `storages/session_projcache/sessions/*.json`（真机 170 文件，四桶 **170/170 恒存在**）
//!   → 四桶 / turns / steps / toolMs / 模型 / 供应商 / 子代理分层。**会话级累计值 → 取差值**；
//! * **原始会话日志** `sessions/<proj>/<dir>/session[.vN].jsonl.zstd`（多帧 zstd，复用既有
//!   `monitor::dsh::{log,decode}` 解码器）→ 报错三层 / 用户打断单列 / 工具调用次数 / **turn 时长样本**
//!   （最长单 turn 由样本算出）—— 投影缓存拿不到这几类，且真机只有一部分会话留有原始日志
//!   （**比例不写死**：覆盖率随机器与清理策略变化，spec 探测期的「矩阵口径 19/99」已作废，
//!   2026-10-06 实测账本口径已接近全量），不可得的部分按空态，**绝不填 0**。
//!   **逐工具**维度（`tool_stats`：每个工具的次数与耗时）**在同一次扫描里产出**——
//!   配对键是 `tool/call.data.callId` ↔ **`tool/result.data.message.source.callId`**
//!   （真机 173 文件全量 14,584/14,584，配对率 100.0%；**不是**任务书写的顶层
//!   `data.toolCallId`，真机 0 条 —— 见 `result_call_id`），配不上的只计次数、不计耗时；
//! * `val.last == null`（真机 10/170）＝本源 TotalOnly 等价情形 → 只用 totals，不参与逐轮；
//! * 四桶**无 reasoningTokens**（dsh 特有）；字段名 `uncachedInputTokens` 即互斥 → 声明 `Exclusive`。
//!
//! ## W-30：`is_subagent` **按文件自身身份**判定（不得走记录级 OR）
//! dsh 的父/子会话**各成一个投影缓存文件**（矩阵 §3.5），子代理身份就写在文件自己的
//! `record.rows.subagent.val.identity` 上。真机 170 文件实测三种形态：
//! `val.identity` 非空（113 个**本文件就是子代理会话**）、`val = {}`（57 个父会话：**行在场**、
//! 值为空对象）、以及旧快照连该行都没有（缺行 = 父）。**只有第一种**才是子代理会话；
//! 「行在场」不是判据，「文件里出现过侧链」更不是（claude 那次 `|=` 把用过子代理的父会话
//! 整条判成子代理 → 唯一消费者按该位硬过滤 → 父会话连会话数带 turn 数一起消失）。
//! 锁：`subagent_identity_comes_from_the_file_not_from_the_row_presence`（混合夹具：
//! 真机父形态 + 真机子形态同场，父必须 `false`、子必须 `true`）。
//!
//! ## GC 4 / W-15：文件读取只经 `CollectContext::read_incremental`
//! 投影缓存是**整份重写**的 JSON 文档（不是只追加的日志），所以它的字节水位没有意义：
//! 每份游标一律把 `byte_offset` 置 0 → 文件代际一变就从 0 全量读回整份文档（正是「一遍扫描」），
//! 而 `(mtime, size)` 未变时仍走 L2 快速路（零读取）。**不得**改成真实 byte_offset：
//! 同尺寸重写/尾部追加会让 append 路只回吐半份 JSON（静默少算）。
//! 断言：`stats.reads` / `stats.bytes_read` / `stats.repeat_reads`（W-15：只断言「值对了」
//! 不等于「路径对了」）。
//!
//! **继承的盲区（如实登记，不得静默）**：`read_incremental` 的快速路只比 `(mtime_ms, size)`，
//! 因此「**同一毫秒内 + 同一大小**」的重写会被判「未变」而整轮跳过 —— 这是 stream/cursor 层的
//! **已登记盲区**（`cursor.rs` 模块文档 + `stream.rs` 的同名锁），本采集器**继承**它。
//! 生产暴露面的量级：要漏掉一次重写，必须让「上一轮游标里记录的 mtime」与本轮的重写时刻落在
//! **同一毫秒**（= 采集读该文件的那一刻与前后两次写入全挤进同一毫秒）；采集轮次是分钟级间隔，
//! 概率 ~0。**夹具纪律**：不得在亚毫秒内做「同尺寸重写」（T16 第一版夹具即因此 flake）。
//! 锁：`same_millisecond_same_size_rewrite_is_the_inherited_fast_path_blind_spot`。
//!
//! **原始日志侧的读取不经过 `read_incremental`**（偏差申报）：zstd 是二进制负载，
//! 而 `read_incremental` 的产物是 `Vec<String>`（`from_utf8_lossy` 切行）——让它去读压缩帧
//! 只会把负载撕成 U+FFFD，且无 `\n` 的文件连一行都切不出来。该侧改用 GC 5 明令复用的
//! `SessionFileScan`（namespace = `scan_namespace(Dsh)` 派生 + `-log`）做 `(mtime,size)`
//! 内容摘要缓存：**同一代际只解码一次**（比「每轮一遍」更强），代际未变则零解码。

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{file_read_failure_is_benign, DeltaBuilder};
use crate::monitor::session_scan::SessionFileScan;
use crate::services::usage::collect::{scan_namespace, CollectContext, UsageCollector};
// **第 14 轮起必须导入 `CursorDelta`**：`event_tail_plan` 的签名显式写它（续读水位的
// 四条结构判据要读 `byte_offset` / `ordinal`）。旧注记「不得导入」的前提是「本文件不出现
// 它的类型名」——改了代码就要同步改注记，否则注释与代码互相打架（D-10 / D-25 同族：
// 导入清单与代码不自洽）。
use crate::services::usage::delta::{CursorDelta, DetailKey, SourceDelta};
use crate::services::usage::error::UsageError;
use crate::services::usage::model::UsageSourceId;
use crate::services::usage::project::project_key_of;
use crate::services::usage::range::{day_key_of_host, hour_key_of_host};
use crate::services::usage::semantics::{
    default_policy, normalize, resolve_semantics, CacheSemantics, RawUsage,
};

/// 本源 `SessionFileScan` 的 namespace（GC 5 / **W-19**）：**必须**由 `scan_namespace(sid)`
/// 派生，不得手写字符串字面量——同 ns 混存别的产物类型会让 `Arc::downcast` 失配、
/// 缓存静默失效（每轮重新解析，正是 L2 要省掉的那笔开销）。
const SCAN_NAMESPACE: &str = scan_namespace(UsageSourceId::Dsh);

/// 原始日志侧的 L2 namespace = `scan_namespace(Dsh)` + `-log`。
/// 为什么要有独立后缀：投影缓存侧不建 `SessionFileScan`（它的 (mtime,size) 判定由
/// `read_incremental` 的游标负责），但**万一**将来有人给投影缓存也建一个，同 ns 混存
/// 两种产物类型就正上面那条静默失效。派生方式：`concat!` 不折叠 `const fn` 调用，
/// 故运行时拼一次、`OnceLock` 缓存（值恒定，之后零成本）。
fn log_scan_namespace() -> &'static str {
    static NS: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    NS.get_or_init(|| format!("{SCAN_NAMESPACE}-log")).as_str()
}

pub struct DshCollector;

/// **A-4**：子代理身份的两个判据源（投影缓存 / 日志头）**收敛成单一决定性取值**的计数器。
///
/// `observe` 同时做两件事：① 返回**收敛后的取值**（单一决定性取值）；② 累加
/// 「可比较数 / 不一致数（分方向）」。采集器在 B 段逐会话调用它，收尾时**一条带计数的
/// `log::warn!`**——可观测，不是"一行 log 就完"（诊断就是触发条件：线上真观察到身份被
/// 重写，再回头上 claude 式 latch，有数据再决定）。
#[derive(Debug, Default)]
struct JudgeTally {
    /// 两源**同时在场**的比较次数（缺源不算"可比较"）
    compared: u64,
    /// 两源同时在场且取值不同的次数
    conflicts: u64,
    /// 冲突方向：投影=子代理 / 日志=父会话
    proj_sub_log_parent: u64,
    /// 冲突方向：投影=父会话 / 日志=子代理
    proj_parent_log_sub: u64,
}

impl JudgeTally {
    /// 记一次比较并返回**收敛后的取值**（A-4：单一决定性取值）。
    ///
    /// **优先级（明文）**：两源同时在场 → **投影缓存权威、日志兜底**；不一致时按优先级取
    /// **投影缓存**取值并计入冲突（分方向）。只有一侧在场 → 取在场的那一侧。两源都缺 →
    /// 保守按**父会话**（与 DAO 的 `COALESCE(s.is_subagent,0)` 同向：未知按父会话计）。
    fn observe(&mut self, proj: Option<bool>, log: Option<bool>) -> bool {
        match (proj, log) {
            (Some(p), Some(l)) => {
                self.compared += 1;
                if p != l {
                    self.conflicts += 1;
                    if p {
                        self.proj_sub_log_parent += 1;
                    } else {
                        self.proj_parent_log_sub += 1;
                    }
                }
                p // ← **投影缓存权威**：兜底源不得否决权威源（旧的 `p && l` 就是那个缺陷）
            }
            (Some(p), None) => p,
            (None, Some(l)) => l,
            (None, None) => false,
        }
    }
}

/// 续读状态：投影缓存是**累计值**，必须记住上次快照才能算差值
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct DshState {
    total: (i64, i64, i64, i64),
    turns: i64,
    steps: i64,
    tool_ms: i64,
}

impl DshState {
    /// 「还没有任何累计快照」= 首轮（或游标里的状态不可解析）。
    /// 归桶策略据此分岔：首轮这一笔是**全部历史**（归 `createdAt`），之后是**增量**（归当轮）。
    fn is_empty(&self) -> bool {
        self.total == (0, 0, 0, 0) && self.turns == 0 && self.steps == 0 && self.tool_ms == 0
    }
}

/// **补录挂账（第 14 轮）**：A 段算出的「投影缓存侧本轮累计增量 `D`」与「当前累计 `P`」
/// 暂存于此，由 B 段按 `P − (已记录水位 R + 本轮实测 E)` 决定补录（任务书 §3.3 的
/// `D − E` 对账；用 R 而不是本轮 D 的理由见 `emit_backfill` —— 日志领先投影缓存一轮时
/// 那个写法会重复入账）。
///
/// A 段**不再**自己写四桶（那是「时间平移」的根因）；没有 B 段可归属的会话（日志被清理）
/// 在收尾时整笔按补录入账。
struct PendingBackfill {
    /// 投影缓存侧本轮的四桶增量 `D`（补录的**构成**按它分配；与 `DshState.total` 同序）
    d: (i64, i64, i64, i64),
    /// 投影缓存侧**当前**累计四桶（`D` 全 0 却仍有差额时的构成兜底）
    p_now: (i64, i64, i64, i64),
    /// 投影缓存侧的步数增量（补录行的请求数 = 它减去本轮已按事件计入的条数）
    steps: i64,
    project_key: String,
    model: String,
    provider: String,
}

/// 补录四桶：把**差额总量** `surplus` 按给定构成分配（最大余数法）。
///
/// 为什么按「总量 + 构成比例」而不是逐桶相减：逐桶会出**混合符号**（投影缓存的某桶大于
/// 事件侧、另一桶小于），逐桶 clamp 会把差额放大（例如 `(+100, −60)` 记成 100，
/// 而真实差额是 40）—— 那正是「总量护栏」要防的账本损坏。最大余数法保证：
/// * 四桶之和**恰好等于** `surplus`（总量不偏）；
/// * 构成沿用调用方给的构成（投影缓存自己的比例），不凭空发明桶。
///
/// 构成全 0 时全部落 `input_fresh`（确定性兜底）。
fn backfill_buckets(comp: (i64, i64, i64, i64), surplus: i64) -> Option<(i64, i64, i64, i64)> {
    if surplus <= 0 {
        return None; // 「绝不能重复入账」：没有正差额就什么都不记
    }
    let buckets = [comp.0, comp.1, comp.2, comp.3];
    let cs: i64 = buckets.iter().sum();
    if cs <= 0 {
        return Some((surplus, 0, 0, 0));
    }
    let mut out = [0i64; 4];
    let mut rest = surplus;
    let mut frac: Vec<(i128, usize)> = Vec::with_capacity(4);
    for (i, v) in buckets.iter().enumerate() {
        let num = (*v as i128) * (surplus as i128);
        let q = num / (cs as i128);
        out[i] = q as i64;
        rest -= q as i64;
        frac.push((num % (cs as i128), i));
    }
    // 余数按小数部分从大到小补 1（同分按下标序 ⇒ 确定性）
    frac.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    for (k, &(_, i)) in frac.iter().enumerate() {
        if (k as i64) < rest {
            out[i] += 1;
        }
    }
    Some((out[0], out[1], out[2], out[3]))
}

/// 落一条**补录**行：只进全天账（`hour_key` = 日键，10 字符），带 `backfill` 标记。
///
/// 返回是否真的落了行。
///
/// **「只进全天账」的机制**（不是靠约定，靠三条结构事实，见
/// `backfill_rows_are_day_only_and_never_land_in_an_hour_window`）：
/// 1. 小时档取数 `query_detail_conn` 之后还有一道 `keys.contains(&r.hour_key)` 的**精确成员**
///    判定（`load_rows_for_keys`，query.rs）⇒ 10 字符的日键**永远不在**小时键集合里；
/// 2. 工作小结的计数查询是 `hour_key BETWEEN ?1 AND ?2`，而日档窗口的界是
///    `"{day}T00" .. "{day}T23"` ⇒ 日键字典序**小于**当日任何小时键（前缀更短）；
/// 3. `usage_daily` 由 `ledger::daily_rows_of` 从明细行折叠 ⇒ 日档**必然**含这笔
///    （「日档与小时档不得分叉」由此自动成立）。
fn emit_backfill(
    b: &mut DeltaBuilder,
    session_id: &str,
    item: &PendingBackfill,
    e: (i64, i64, i64, i64),
    e_events: i64,
    recorded: i64,
    now_ms: i64,
) -> bool {
    // **目标口径（防重复入账的关键）**：补录只补「投影缓存当前累计」与
    // 「已经记录过的量」之间的差额 —— `recorded` = 上一轮结束时的**已记录水位** R
    //（实测 E 的累计 + 历次补录），再加上本轮实测的四桶和。
    // **不能**用「本轮投影缓存增量 D − 本轮实测 E」：日志**领先**投影缓存一轮时
    //（缓存是懒刷新的快照：某轮日志已追加、投影缓存文件还没重写 ⇒ 那一轮 A 段走
    // `(mtime,size)` 快速路、根本不产出 D），下一轮投影缓存追上时 D 会把这批事件再算一遍
    // ⇒ 重复入账。用 R 就自然吸收了这个错位：`R` 已经把那一轮实测过的量记进去了。
    let p_now: i64 = item.p_now.0 + item.p_now.1 + item.p_now.2 + item.p_now.3;
    let surplus = p_now - (recorded + e.0 + e.1 + e.2 + e.3);
    let comp = if item.d.0 + item.d.1 + item.d.2 + item.d.3 > 0 {
        item.d
    } else {
        item.p_now
    };
    let Some(raw4) = backfill_buckets(comp, surplus) else {
        return false; // 没有正差额：什么都不记（日志已覆盖，绝不能重复入账）
    };
    let raw = RawUsage {
        input_raw: raw4.0,
        cache_read: raw4.2,
        cache_write: raw4.3,
        output: raw4.1,
        reasoning: 0,
        total_raw: None,
    };
    // 与实测行**同一套**归一化（Exclusive：request_total = input + cache_read + cache_write）
    let Some(n) = normalize(&raw, CacheSemantics::Exclusive) else {
        return false;
    };
    let day = day_key_of_host(now_ms);
    let key = DetailKey {
        session_id: session_id.to_string(),
        hour_key: day.clone(),
        day_key: day,
        project_key: item.project_key.clone(),
        model: item.model.clone(),
        provider: item.provider.clone(),
    };
    let d = b.detail(key, now_ms);
    d.buckets.add(&n.buckets);
    d.request_total += n.request_total;
    d.requests += (item.steps - e_events).max(0);
    // **来源可辨**：`cache_semantics = backfill` 把「补录」与「实测」分开
    //（列已存在，不走 migration；取值集合的扩充在契约里做了日期化申报）。
    d.cache_semantics = CacheSemantics::Backfill;
    true
}

impl UsageCollector for DshCollector {
    fn source_id(&self) -> UsageSourceId {
        UsageSourceId::Dsh
    }
    fn collect(&self, ctx: &CollectContext<'_>) -> Result<SourceDelta, UsageError> {
        DshCollector::collect_with_home(ctx, &crate::monitor::dsh::dsh_home_with(ctx.home))
    }
}

impl DshCollector {
    pub fn collect_with_home(
        ctx: &CollectContext<'_>,
        home: &std::path::Path,
    ) -> Result<SourceDelta, UsageError> {
        // **供应商规则从 ctx 注入**（§3.2.3 FIX-6）：lib 单测构建注入空规则 → 零 DB；
        // 生产/集成构建才回落到 settings::load()。采集器不得直连设置层。
        let rules = ctx.provider_rules();
        let mut b = DeltaBuilder::new(UsageSourceId::Dsh, rules);
        // ---- A. 投影缓存（四桶/turns/steps/toolMs/模型/供应商/子代理）----
        let dir = home.join("storages/session_projcache/sessions");
        // **不得**在投影缓存目录缺失时提前 `return`（任务书原文如此，会把 B 段一起短路）：
        // 原始日志可能仍在——B4 的夹具就是「只有日志、没有投影缓存」这种形态，真机也存在
        // 「日志在、投影缓存被清理/尚未落盘」的窗口。目录缺失只跳过 A 段。
        //
        // **M2（fix round 1）**：目录级读失败原来被 `.ok()` 静默吞掉。按 `ErrorKind` 区分
        //（D-24 的口径）：`NotFound` = 「未装 dsh / 缓存被清理」这一种可静默；
        // 其余（EACCES / EIO）是「这个源读不了」→ **整源响亮失败**（落 `errorCode`，别让用户
        // 以为是「这个源没有用量」）。
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => Some(e),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                return Err(UsageError::new(
                    "usage-source-io",
                    format!(
                        "dsh projection cache dir {} read failed: {e}",
                        dir.display()
                    ),
                ))
            }
        };
        // **M1（fix round 1）**：结构性缺 `modelSelection` 行的会话数（真机 63/171）。
        // 这些行按**空模型 + Unknown 供应商**落库（数据 > 分组整洁，见模块头与报告），
        // 但**不得静默**：本轮结束前给一条带数量的告警。
        let mut no_model_sessions = 0i64;
        // 「会话 → (模型, 供应商)」：投影缓存是唯一带这两个字段的一侧，
        // B 段（原始日志）靠它补位，免得报错/工具计数落进 `model=""` 的幽灵行
        let mut model_by_session: HashMap<String, (String, String)> = HashMap::new();
        // **A-4**：投影缓存侧的**子代理判据**（权威源）——B 段据此收敛出单一决定性取值
        let mut proj_judge: HashMap<String, bool> = HashMap::new();
        // **补录挂账（第 14 轮）**：A 段算出的差额 `D` 暂存于此，B 段按 `D − E` 决定补录
        //（差额只进全天账，不进任何短窗口）；B 段没机会认领的（该会话没有原始日志可归属）
        // 在收尾时按整笔 D 补录。
        let mut pending_backfill: HashMap<String, PendingBackfill> = HashMap::new();
        // **A-4**：两源不一致的**计数**（收尾一条带计数的告警；可观测，不是一行 log 就完）
        let mut judge_tally = JudgeTally::default();
        for e in entries.into_iter().flatten().flatten() {
            let path = e.path();
            if path.extension().map(|x| x != "json").unwrap_or(true) {
                continue;
            }
            let session_id = path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            if session_id.is_empty() {
                continue;
            }
            // 游标键 = 文件 stem（目录扁平，`<sessionId>.json` 在源内唯一）
            let file_key = session_id.clone();
            let prev = ctx.cursor_of(&file_key).cloned();
            // **GC 4：文件读取只经唯一入口** `CollectContext::read_incremental`
            //（它同时统计 reads / bytes_read / repeat_reads，W-15 的断言就落在这里）
            let read = match ctx.read_incremental(&path, &file_key) {
                Ok(r) => r,
                Err(err) => {
                    // **W-06 / D-24 / D-38**：只有「枚举与读取之间的消失竞态」才允许跳过；
                    // 其余读失败（权限 / IO / mtime 不可得）必须整源响亮失败，落
                    // `UsageSourceStatus.errorCode`——否则用户看到的是「这个源没有用量」。
                    if file_read_failure_is_benign(&path, &err) {
                        log::warn!(
                            "usage/dsh: 投影缓存 {} 读取失败（文件已消失，跳过本轮）: {err}",
                            path.display()
                        );
                        continue;
                    }
                    return Err(UsageError::new(
                        "usage-source-io",
                        format!("dsh projection cache {} read failed: {err}", path.display()),
                    ));
                }
            };
            if read.lines.is_empty() {
                if read.bytes_read == 0 {
                    // `(mtime,size)` 未变 → L2 快速路（零读取）→ 本轮无新数据，游标原样推回
                    b.push_cursor(read.next);
                } else {
                    // 读了字节却切不出一行完整行 = 文件不以 '\n' 收尾（撕裂写 / 非文本）。
                    // **不得**当成「没有数据」静默跳过（GC 7 / D-23 同族）：告警 + **不推游标**，
                    // 下一轮仍从 0 重读同一份文档（不推进就不会永久漏）。
                    log::warn!(
                        "usage/dsh: 投影缓存 {} 没有以换行收尾（撕裂写？），本轮跳过且不推进游标",
                        path.display()
                    );
                }
                continue;
            }
            // 投影缓存是**整份重写**的多行 JSON 文档（真机 170/170 都以 '\n' 收尾）：
            // 行切分产物按 '\n' 拼回正文即可（JSON 不在乎行尾空白）。
            let text = read.lines.join("\n");
            let Ok(rec) = serde_json::from_str::<Value>(&text) else {
                // 形状不认识：响亮但**不冻结整源**（一个坏文件不该让其余文件本轮不入账，
                // 与 D-24 的取舍一致）；不推游标 → 下轮重试。
                log::warn!(
                    "usage/dsh: 投影缓存 {} JSON 解析失败（本轮跳过且不推进游标）",
                    path.display()
                );
                continue;
            };
            b.parsed_files += 1;
            let rows = rec.pointer("/record/rows").cloned().unwrap_or(Value::Null);
            let cwd = rec
                .pointer("/record/identity/cwd")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            // **W-30：子代理分层按「本文件自身身份」判定**（不得走记录级 OR，也不得拿
            // 「行在场」当判据）。真机 170 个投影文件实测三种形态：
            // ① `subagent.val.identity` 非空 → **113 个文件本身就是子代理会话**；
            // ② `subagent.val = {}` → **57 个父会话**（行在场、值是空对象）；
            // ③ 旧快照连该行都没有 → 父会话。
            // 「行在场 ⇒ 子代理」的退化实现会把 170/170 全判成子代理 → 消费层按该位硬过滤
            // → 父会话连会话数带 turn 数一起消失（与 claude 那次 `|=` 是同一族错误的两个入口）。
            let is_sub = rows
                .pointer("/subagent/val/identity")
                .map(|id| id.as_object().map(|o| !o.is_empty()).unwrap_or(false))
                .unwrap_or(false);
            // 模型/供应商：modelSelection.val.lastUsed（真机 99/170 有行且都有值）
            let last_used = rows.pointer("/modelSelection/val/lastUsed");
            let model = last_used
                .and_then(|v| v.get("model"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let explicit = last_used
                .and_then(|v| v.get("provider"))
                .and_then(|v| v.as_str());
            if model.is_empty() {
                no_model_sessions += 1;
            }
            // **供应商唯一入口**（GC 6 / B6）：dsh 有同对象 `.provider` 字段
            // → 有值即 Measured；无值落 Unknown（**不是**空串推断）
            let (provider, _) = b.provider_of(explicit, model);
            // 累计四桶 + turns/steps/toolMs
            let tot = rows
                .pointer("/tokenUsage/val/totals")
                .cloned()
                .unwrap_or(Value::Null);
            let now_total = (
                tot.get("uncachedInputTokens")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0),
                tot.get("outputTokens")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0),
                tot.get("cacheReadTokens")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0),
                tot.get("cacheWriteTokens")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0),
            );
            let stats = rows
                .pointer("/sessionStats/val")
                .cloned()
                .unwrap_or(Value::Null);
            let now_turns = stats.get("turns").and_then(|v| v.as_i64()).unwrap_or(0);
            let now_steps = stats.get("steps").and_then(|v| v.as_i64()).unwrap_or(0);
            let now_tool_ms = stats.get("toolMs").and_then(|v| v.as_i64()).unwrap_or(0);
            // 差值（累计 → 增量）：**必须从游标里读回上次快照**，只写不读就是每轮重复计
            //（D-34 在 opencode 上的形态）。回退（重写/重置）时按「本轮 = 当前值」计。
            let prev_state: DshState = prev
                .as_ref()
                .and_then(|c| serde_json::from_str(&c.state_json).ok())
                .unwrap_or_default();
            // **裁决 B（fix round 1）：时间桶必须来自会话自己的时间，不是采集时刻。**
            // 首轮（账本为空 / 本文件还没有累计快照）这一笔是**全部历史累计**；若按采集时刻归桶，
            // 用户第一次跑就会把整个历史（只读冒烟实测 input 53.1M / cache_read 2.64G / output 12.5M）
            // 塞进「当轮那一小时」，hero（近 24 小时）与小时趋势被历史淹没，而且这些行**永久**留在错的小时。
            // 链：`record.identity.createdAt`（真机实测是 ms epoch）→ 该文件**真实 mtime**
            // → 都拿不到才 `ctx.now_ms` + 告警。
            let first_snapshot = prev_state.is_empty();
            let ts = if first_snapshot {
                match rec
                    .pointer("/record/identity/createdAt")
                    .and_then(|v| v.as_i64())
                    .filter(|v| *v > 0)
                {
                    Some(t) => t,
                    None if read.next.mtime_ms > 0 => read.next.mtime_ms,
                    None => {
                        log::warn!(
                            "usage/dsh: 会话 {session_id} 既无 createdAt 也无文件 mtime，归桶退回采集时刻"
                        );
                        ctx.now_ms
                    }
                }
            } else {
                // **唯一把采集时刻当主判据的地方**（其余三处 `ctx.now_ms` 都是「真值拿不到」
                // 时的兜底，两处还带 warn；已在模块头与报告「偏差申报」点名）：
                // 有快照之后的差值记的是「自上次采集以来」发生的事，归**当轮**才是对的
                //（opencode 的 `session.time_updated` 在活跃会话上同理 ≈ 当轮）。
                // 把增量也钉在创建时刻会让会话寿命越长、趋势越歪。
                ctx.now_ms
            };
            let prev = prev_state;
            let diff = |cur: i64, old: i64| if cur >= old { cur - old } else { cur };
            let d_total = (
                diff(now_total.0, prev.total.0),
                diff(now_total.1, prev.total.1),
                diff(now_total.2, prev.total.2),
                diff(now_total.3, prev.total.3),
            );
            let d_steps = diff(now_steps, prev.steps);
            let d_turns = diff(now_turns, prev.turns);
            let d_tool_ms = diff(now_tool_ms, prev.tool_ms);
            // **第 14 轮：逐事件归属（时间归属的唯一出处搬到 B 段）** ——
            // 投影缓存只有**累计值**、没有「什么时候用的」；按 `ts`（首轮 createdAt、
            // 之后采集时刻）归桶正是「时间平移」的根因（睡眠/停摆/首轮历史全挤进当轮那一小时）。
            // 所以 A 段**不再**写四桶与请求数，只把差额 D **挂账**，交给 B 段按
            // 「每条 `assistant/message` 自己的 time」归属；`D − E` 的零头进补录（只进全天账）。
            // **游标照旧推进**：它是投影缓存侧的累计水位，与「这一轮记了什么」无关。
            pending_backfill.insert(
                session_id.clone(),
                PendingBackfill {
                    d: d_total,
                    p_now: now_total,
                    steps: d_steps,
                    project_key: project_key_of(Some(cwd)),
                    model: model.to_string(),
                    provider: provider.clone(),
                },
            );
            // A 段只剩「没有逐事件来源」的会话级事实：turns / toolMs（steps 已由逐事件计入）。
            let changed = d_turns != 0 || d_tool_ms != 0;
            if changed {
                b.new_records += 1; // 本条记录确实带来了新数据
                let hour = hour_key_of_host(ts);
                let key = DetailKey {
                    session_id: session_id.clone(),
                    hour_key: hour.clone(),
                    day_key: hour.chars().take(10).collect(),
                    project_key: project_key_of(Some(cwd)),
                    model: model.to_string(),
                    provider: provider.clone(),
                };
                let d = b.detail(key, ts);
                d.counters.turns += d_turns;
                d.counters.tool_ms += d_tool_ms;
            }
            // **fix round 2 Minor#3**：会话维度的 `last_seen_at` 用**该投影缓存文件的真实 mtime**
            //（= 这个会话最后一次被写入的时间），**不是** `ts` —— `ts` 在首轮是 `createdAt`，
            // 把它当「最后见到」会让「只由投影缓存代表、此后不再变化」的会话永远停在创建时刻
            //（DAO 取 MAX，一旦写入就回不来）。mtime 正是本文件回退链里已经信任的那个真值；
            // 取不到（理论不可达：`stat_of` 拿不到 mtime 时读早就 Err 了）才退回 ts。
            let last_seen = if read.next.mtime_ms > 0 {
                read.next.mtime_ms
            } else {
                ts
            };
            b.session(&session_id, Some(cwd), None, is_sub, None, None, last_seen);
            // 游标：真实代际（`mtime_ms` / `file_size` 取自**读取前**那次 stat —— C3/W-23：
            // 不得写 token 累计数那种语义无关的垃圾值，也不得把失败伪装成 0）+ 累计快照。
            let mut cur = read.next;
            cur.session_id = session_id.clone();
            cur.last_cumulative = Some(now_total.0 + now_total.1 + now_total.2 + now_total.3);
            // **投影缓存的字节水位语义（有意为之，不得改）**：它是**整份重写**的 JSON 文档，
            // 不是只追加的日志 —— 记真实 `byte_offset` 会让「同尺寸重写 / 尾部追加」命中
            // append 快速判定，只回吐半份 JSON（静默少算）。恒置 0 的效果是：
            // * `(mtime,size)` 未变 → L2 快速路，零读取（不变语义照旧）；
            // * 代际一变 → 从 0 全量读回**整份文档**（正是「一遍扫描」要的），
            //   增量 = 新累计 − 游标 state_json 里的旧累计。
            // `fingerprint` 同时清空 = 「没有已消费前缀」，任何将来按 `fingerprint_mismatch`
            // 判定的路径都会往**重扫**这个安全方向倒。
            cur.byte_offset = 0;
            cur.ordinal = 0;
            cur.fingerprint = String::new();
            cur.state_json = serde_json::to_string(&DshState {
                total: now_total,
                turns: now_turns,
                steps: now_steps,
                tool_ms: now_tool_ms,
            })
            .unwrap_or_default();
            b.push_cursor(cur);
            model_by_session.insert(session_id.clone(), (model.to_string(), provider.clone()));
            // **A-4 权威源登记**：本文件的子代理身份（B 段收敛时以它为准）
            proj_judge.insert(session_id.clone(), is_sub);
        }
        if no_model_sessions > 0 {
            log::warn!(
                "usage/dsh: 本轮 {no_model_sessions} 个投影缓存文件结构性缺 modelSelection 行 \
                 → 四桶照常入账、明细行按空模型 + Unknown 供应商（真机 63/171，矩阵 §5.1/§C.2；不得静默）"
            );
        }
        // ---- B. 原始会话日志（报错三层 / 最长单 turn / 逐工具 / **逐事件用量归属**）----
        scan_raw_logs(
            ctx,
            home,
            &mut b,
            &model_by_session,
            &proj_judge,
            &mut judge_tally,
            &mut pending_backfill,
        )?;
        // **补录收尾（任务书 §3.2/§3.3）**：挂账里剩下的会话 = 投影缓存有增量、却**没有任何
        // 原始日志**可归属（日志被清理 / 会话目录缺失）→ 整笔 `D` 都是「只能从累计差额补录」的
        // 部分：只进全天账、打 `backfill` 标记，**不进任何短窗口**。
        let mut backfilled_sessions = 0i64;
        for (sid, item) in pending_backfill.drain() {
            let log_key = format!("log:{sid}");
            // 已记录水位：优先本会话日志游标（实测 + 补录的累计），首轮退回投影缓存侧的
            // 已入账累计（旧实现记过的量）—— 两者语义一致。
            let r_prev = ctx
                .cursor_of(&log_key)
                .and_then(|c| c.last_cumulative)
                .or_else(|| ctx.cursor_of(&sid).and_then(|c| c.last_cumulative))
                .unwrap_or(0);
            if emit_backfill(&mut b, &sid, &item, (0, 0, 0, 0), 0, r_prev, ctx.now_ms) {
                backfilled_sessions += 1;
            }
            // **无论有没有落行都要把已记录水位推到投影缓存当前累计**：否则将来该会话的日志
            // 出现时，定界会退回更早的位置 ⇒ 把已经补录过的量再测一遍（重复入账）。
            // 只改 `last_cumulative`，`byte_offset` / `ordinal` / `state_json` 原样留着。
            let mut cur = ctx.cursor_of(&log_key).cloned().unwrap_or_default();
            cur.session_id = log_key;
            cur.last_cumulative = Some(item.p_now.0 + item.p_now.1 + item.p_now.2 + item.p_now.3);
            b.push_cursor(cur);
        }
        if backfilled_sessions > 0 {
            log::warn!(
                "usage/dsh: 本轮 {backfilled_sessions} 个会话的投影缓存增量**没有原始日志可归属**\
                 （日志被清理 / 目录缺失）→ 整笔进补录（只进全天账、带 backfill 标记，不进短窗口）"
            );
        }
        // **A-4**：两源不一致 → **一条带计数**的告警（可观测）。诊断本身就是触发条件：
        // 投影缓存是**会被宿主重写的缓存**，「identity 从非空变空」在原理上不是不可能
        // ——若线上持续出现，再回头上 claude 式 latch（有数据再决定，不预先拍一个）。
        if judge_tally.conflicts > 0 {
            log::warn!(
                "usage/dsh: 子代理身份两源判据不一致 {} / 可比较 {}（投影缓存**权威**、日志兜底，A-4）\
                 —— 投影=子代理而日志=父会话 {} 条；投影=父会话而日志=子代理 {} 条。\
                 已按优先级取**投影缓存**取值（旧实现是 AND 合并：兜底源会否决权威源）；\
                 若持续出现，先看这两个计数再决定是否上 latch",
                judge_tally.conflicts,
                judge_tally.compared,
                judge_tally.proj_sub_log_parent,
                judge_tally.proj_parent_log_sub
            );
        }
        Ok(b.finish())
    }
}

/// 原始日志扫描（复用既有 `monitor::dsh::log` 的三代际选择与 zstd 解码）。
///
/// **一遍扫描产出全部指标**：`LogDigest::of` 在**一次**遍历里同时产出
/// 报错三层 / 用户打断 / turn 时长样本 / **逐工具（次数 + 耗时）** —— 绝不为新指标再解一遍。
/// 这个**扫描产物整体**进 L2 缓存（`(mtime,size)` 未变 → 连解析都不做）：稳态下一份未变的日志
/// 是**零解码、零解析**（GC 3/5），由 `parsed_files == 0` 的断言钉住
///（fix round 1 裁决 C：原来缓存的是解压后的**正文**，每轮仍要把全部事件重新 `parse_events` 一遍）。
/// **A-4**：后两个参数是子代理判据的收敛输入——
/// `proj_judge` = 投影缓存侧（**权威源**，会话不在表里 = 该侧缺源）；
/// `judge_tally` 由 `observe` 顺带累计「可比较 / 不一致（分方向）」，收尾出一条带计数的告警。
fn scan_raw_logs(
    ctx: &CollectContext<'_>,
    home: &std::path::Path,
    b: &mut DeltaBuilder,
    model_by_session: &HashMap<String, (String, String)>,
    proj_judge: &HashMap<String, bool>,
    judge_tally: &mut JudgeTally,
    pending_backfill: &mut HashMap<String, PendingBackfill>,
) -> Result<(), UsageError> {
    let sessions_root = home.join("sessions");
    // **M2（fix round 1）**：根目录读失败按 `ErrorKind` 区分 —— `NotFound` = 未装 dsh /
    // 会话日志从未落盘（正常空态）；其余（EACCES / EIO）是「这个源读不了」→ **整源响亮失败**。
    let projects = match std::fs::read_dir(&sessions_root) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => {
            return Err(UsageError::new(
                "usage-source-io",
                format!(
                    "dsh session log root {} read failed: {e}",
                    sessions_root.display()
                ),
            ))
        }
    };
    let mut live = std::collections::HashSet::new();
    // L2 内容摘要缓存的句柄（GC 5：namespace 由 `scan_namespace(Dsh)` 派生 + `-log`）——
    // `SessionFileScan` 只是个零成本句柄（真正的缓存在 `session_scan` 的全局注册表里），
    // 每轮现造一个即可。
    let log_scan = SessionFileScan::new(log_scan_namespace());
    // 聚合告警计数（M1 / 工具配对失败）：**不得静默**，但也不按会话刷屏
    let mut unpaired = 0i64;
    let mut no_model_rows = 0i64;
    // 纯日志会话（没有投影缓存文件、也没有已入账累计）：本轮**不入账**（量的权威是投影缓存）
    let mut out_of_scope = 0i64;
    for p in projects.flatten() {
        let ppath = p.path();
        if !ppath.is_dir() {
            continue;
        }
        let dirs = match std::fs::read_dir(&ppath) {
            Ok(d) => d,
            // 目录在枚举与读取之间消失 = 真竞态 → 跳过（D-24 唯一允许静默的 kind）
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            // 单个项目目录读不了：**响亮到日志**、不冻结整源（一个子目录不该让整个源本轮不入账；
            // 与 `kimi.rs` 的家族口径同形，已登记）
            Err(e) => {
                log::warn!(
                    "usage/dsh: 会话项目目录 {} 读取失败（跳过该项目）: {e}",
                    ppath.display()
                );
                continue;
            }
        };
        for sd in dirs.flatten() {
            let spath = sd.path();
            if !spath.is_dir() {
                continue;
            }
            let Some((_v, gen_path)) = crate::monitor::dsh::log::generation_logs(&spath).pop()
            else {
                continue;
            };
            live.insert(gen_path.clone());
            // 预过滤反模式禁令（AGENTS.md L3-4）：内容未变 → 不打开、不解压、不解析。
            // `parsed_now` 记「本轮真的解了码」（W-50 的文件计数口径）。
            let parsed_now = std::cell::Cell::new(false);
            let digest = log_scan.parse(&gen_path, |p| {
                parsed_now.set(true);
                p.parent()
                    .and_then(crate::monitor::dsh::log::read_best_generation)
                    .and_then(|r| LogDigest::of(&r))
            });
            let Some(digest) = digest.as_ref() else {
                // 解不开（截断 / 非 zstd / 空文件）：**响亮但不冻结整源**——一个坏文件不该让
                // 其余会话本轮不入账（与 D-24 的取舍一致）；该会话的报错/工具指标按**空态**，
                // 绝不填 0（GC 7 / §9.5 的「部分覆盖」）。
                // **重试时机（M6，措辞已更正）**：`get_or_parse` 把 `None` **也**按 `(mtime,size)`
                // 缓存 → 该文件是「**下次变化时**才重试」，不是每轮重试。
                // 这是有意保留的（坏文件不该每轮重复解压 + 刷屏），**不是**「不推进游标所以下轮重试」。
                log::warn!(
                    "usage/dsh: 原始日志无法解码（本次跳过，不产出报错/工具指标）: {}",
                    gen_path.display()
                );
                continue;
            };
            if parsed_now.get() {
                b.parsed_files += 1;
            }
            let header = &digest.header;
            let facts = &digest.facts;
            let cwd = header.cwd.clone().unwrap_or_default();
            // **A-4：两个判据源收敛成单一决定性取值**（投影缓存权威、日志兜底）。
            // 旧实现在这里直接传日志头判据，再由 `DeltaBuilder::session` 的 AND 合并 ——
            // 「投影=子代理、日志=父会话」会被兜底源推翻成父会话（权威源被否决）。
            let is_sub = judge_tally.observe(
                proj_judge.get(&header.id).copied(),
                Some(crate::monitor::dsh::log::is_subagent(header)),
            );
            b.session(
                &header.id,
                Some(&cwd),
                None,
                is_sub,
                header.parent_session.clone(),
                None,
                // 会话维度的 last_seen_at 用日志自己的最后活动时间（裁决 B 同源），
                // 不用采集时刻
                facts.last_event_ms.unwrap_or(ctx.now_ms),
            );
            // 事实计数水位（键带 `log:` 前缀，避免与投影缓存的同名会话键互相覆盖）
            let log_key = format!("log:{}", header.id);
            // **D-41 同族（跨轮持久化模型 / 供应商）**：原始日志侧不产生模型字段，
            // 若本轮的投影缓存不可得（被清理 / 该文件结构性缺 `modelSelection` 行），
            // 必须沿用**上一轮已知**的模型与供应商——否则这些报错/工具计数会落进
            // `model=""` / `provider=""` 的幽灵行（进明细表与日聚合主键、UI 显空名）。
            let prev: DshLogState = ctx
                .cursor_of(&log_key)
                .and_then(|c| serde_json::from_str(&c.state_json).ok())
                .unwrap_or_default();
            let (model, provider) = match model_by_session.get(&header.id) {
                Some((m, p)) if !m.is_empty() => (m.clone(), p.clone()),
                // 本轮的投影缓存没给出模型：退回游标里的上一轮值（结构性不可得时两边都空）
                _ => (prev.model.clone(), prev.provider.clone()),
            };
            let diff = |cur: i64, old: i64| if cur >= old { cur - old } else { cur };
            // **M3 / W-49（fix round 1）**：水位按**代际**而不只按会话 —— 真机 7/172 个会话目录
            // 有多代际文件（v2+v3 / plain+v3 / v3+v4）。代际一变：
            // * 新文件若是旧水位的**超集**（各项计数都不减）→ 差值恰好是「切换期间新增的事件」，照常计；
            // * 否则（截断 / 重建）→ **对齐水位而不计数**。旧语义「回退按整份重计」会把新文件里
            //   **已有的**全部报错/工具/样本再计一遍（重复计入）。
            let gen_changed = prev.gen != 0 && prev.gen != digest.version;
            let superset = facts.error_model >= prev.error_model
                && facts.error_turn >= prev.error_turn
                && facts.error_tool >= prev.error_tool
                && facts.interrupted >= prev.interrupted
                && facts.tool_calls >= prev.tool_calls
                && (facts.turn_ms.len() as i64) >= prev.turn_samples;
            let align_only = gen_changed && !superset;
            if align_only {
                log::warn!(
                    "usage/dsh: 会话 {} 的日志代际 {} → {} 不是旧水位的超集 → 只对齐水位、本轮不计任何事实",
                    header.id,
                    prev.gen,
                    digest.version
                );
            }
            let now_samples = facts.turn_ms.len();
            let mut new_samples: Vec<i64> = if align_only {
                Vec::new()
            } else if now_samples as i64 >= prev.turn_samples {
                // 只取**新增的尾部样本**（日志只追加）；水位回退时按整份重计
                let from = prev.turn_samples.max(0) as usize;
                facts.turn_ms[from.min(now_samples)..].to_vec()
            } else {
                facts.turn_ms.clone()
            };
            let mut d_error_model = diff(facts.error_model, prev.error_model);
            let mut d_error_turn = diff(facts.error_turn, prev.error_turn);
            let mut d_error_tool = diff(facts.error_tool, prev.error_tool);
            let mut d_interrupted = diff(facts.interrupted, prev.interrupted);
            let mut d_tool_calls = diff(facts.tool_calls, prev.tool_calls);
            let mut d_unpaired = diff(facts.tool_unpaired, prev.tool_unpaired);
            // 逐工具：次数与耗时**都**按差值算（同一个累计水位）。
            // **绝不只填次数**：那会让该工具的 ms 变成假 0（GC 7）。
            let mut d_tool_stats: Vec<(String, (i64, i64))> = Vec::new();
            if !align_only {
                for (name, (cnt, ms)) in &facts.tool_stats {
                    let (pc, pm) = prev.tool_stats.get(name).copied().unwrap_or((0, 0));
                    let (dc, dm) = (diff(*cnt, pc), diff(*ms, pm));
                    if dc != 0 || dm != 0 {
                        d_tool_stats.push((name.clone(), (dc, dm)));
                    }
                }
            }
            if align_only {
                d_error_model = 0;
                d_error_turn = 0;
                d_error_tool = 0;
                d_interrupted = 0;
                d_tool_calls = 0;
                d_unpaired = 0;
                new_samples.clear();
                d_tool_stats.clear();
            }
            let new_total = d_error_model
                + d_error_turn
                + d_error_tool
                + d_interrupted
                + d_tool_calls
                + new_samples.len() as i64;
            unpaired += d_unpaired;
            // **fix round 2 裁决 A（行为性、静默永久丢数）**：落行闸门原来只看 `new_total`，
            // 而**迟到回执**恰好是「回执是该轮**唯一**新增事实」的形态（真机单工具最长 3.1 h
            // → 跨轮必然发生）：第 N 轮读到 `tool/call`（无回执）记下 `(1,0)` 并**推进水位**；
            // 第 N+1 轮回执到达（成功工具 → 无 error_tool、turn 未结束、无新调用）⇒ `new_total == 0`
            // ⇒ 不落行，**但水位已把这段 ms 记走**；第 N+2 轮 turn/end 落行时 `dm` 已是 0
            // ⇒ **该次工具耗时永久丢失**。
            // 所以「逐工具耗时」必须与 `new_total` 并列成为落行条件（codex 在回执事件上
            // 无条件 `b.detail(...)`，同款）。
            // 注意 `new_records` **仍按事实数**（此处 +0）：它记的是「新增事实」，
            // 而这一轮新增的是**已计过次数的调用的耗时**，不是新事实 —— Task 23B 的
            // 「零差值轮 newRecords == 0」语义不受影响。
            let has_ms_only = d_tool_stats.iter().any(|(_, (_, dm))| *dm != 0);
            if new_total > 0 || has_ms_only {
                b.new_records += new_total;
                // **裁决 B（fix round 1）**：原始日志侧归桶用**日志自己的时间** ——
                // 最后一条带时间的事件 → 该代际文件的真实 mtime → 都拿不到才采集时刻 + 告警。
                // 绝不用采集时刻当唯一来源（首轮会把整个历史的报错/工具/时长记进当轮小时桶）。
                let ts = match facts.last_event_ms {
                    Some(t) => t,
                    None if digest.mtime_ms > 0 => digest.mtime_ms,
                    None => {
                        log::warn!(
                            "usage/dsh: 会话 {} 的日志既无事件时间戳也无文件 mtime，归桶退回采集时刻",
                            header.id
                        );
                        ctx.now_ms
                    }
                };
                let hour = hour_key_of_host(ts);
                if model.is_empty() {
                    no_model_rows += 1;
                }
                // 供应商一律经唯一入口（GC 6）——**包括**从游标里继承来的这一路：
                // 空模型时 `provider_of` 落 (`""`, Unknown)，与投影缓存侧同一口径。
                let (provider, _) = b.provider_of(Some(&provider), &model);
                let key = DetailKey {
                    session_id: header.id.clone(),
                    hour_key: hour.clone(),
                    day_key: hour.chars().take(10).collect(),
                    project_key: project_key_of(Some(&cwd)),
                    // `clone()`：同一个 `model` 后面还要写进跨轮水位（D-41），直接 move 是 E0382
                    model: model.clone(),
                    provider,
                };
                let d = b.detail(key, ts);
                d.counters.error_model += d_error_model;
                d.counters.error_turn += d_error_turn;
                d.counters.error_tool += d_error_tool;
                d.counters.interrupted += d_interrupted;
                d.counters.tool_calls += d_tool_calls;
                d.counters.turn_ms.extend(new_samples);
                for (name, (dc, dm)) in d_tool_stats {
                    let e = d.counters.tool_stats.entry(name).or_default();
                    e.0 += dc;
                    e.1 += dm;
                }
            }
            // ---- B-token：逐事件用量归属（**时间归属的唯一出处**，任务书 §3.1）----
            // 每条 `assistant/message` 的四桶按**它自己的 time** 落 `hour_key_of_host(event.time)`。
            // A 段的投影缓存只有累计值、没有时间 ⇒ 它只提供差额 D（挂账在 `pending_backfill`）。
            let accounted = ctx.cursor_of(&header.id).and_then(|c| c.last_cumulative);
            // **已记录水位 R**：本游标的 `last_cumulative` 就是它（实测 + 补录的累计）。
            // 首轮（还没有本游标）退回投影缓存侧的已入账累计 —— 那是旧实现已经记过的量，
            // 两者语义一致（都回答「这个会话已经记了多少」）。
            let r_prev = ctx
                .cursor_of(&log_key)
                .and_then(|c| c.last_cumulative)
                .or(accounted)
                .unwrap_or(0);
            let plan = event_tail_plan(digest, ctx.cursor_of(&log_key), prev.gen, Some(r_prev));
            // **只在「投影缓存侧认得这个会话」时才归账**：日志是**时间**的来源，不是量的来源
            //（量的权威是投影缓存）。纯日志会话（没有投影缓存文件、也没有已入账累计）不入账
            //——否则会凭空多出一笔谁也没算过的量；**也不推进水位**（等它进范围再算，
            //「宁可重扫，不可漏」）。
            let in_scope = model_by_session.contains_key(&header.id) || accounted.is_some();
            let mut e_total = (0i64, 0i64, 0i64, 0i64);
            let mut e_events = 0i64;
            // 本轮认领到的投影缓存当前累计（`None` = 投影缓存本轮没读到 / 不在范围）
            let mut claimed_p_now: Option<(i64, i64, i64, i64)> = None;
            if in_scope {
                // 按事件自己的小时聚合（**必须**经 `hour_key_of_host`：键函数的唯一入口）
                let mut by_hour: std::collections::BTreeMap<String, EventHourAgg> =
                    std::collections::BTreeMap::new();
                for ev in &digest.usage_events[plan.start..] {
                    let h = hour_key_of_host(ev.time_ms);
                    let a = by_hour.entry(h).or_insert(EventHourAgg {
                        raw: (0, 0, 0, 0),
                        events: 0,
                        first_ms: ev.time_ms,
                    });
                    a.raw.0 += ev.buckets.0;
                    a.raw.1 += ev.buckets.1;
                    a.raw.2 += ev.buckets.2;
                    a.raw.3 += ev.buckets.3;
                    a.events += 1;
                    a.first_ms = a.first_ms.min(ev.time_ms);
                }
                for (hour, agg) in &by_hour {
                    let raw = RawUsage {
                        input_raw: agg.raw.0,
                        cache_read: agg.raw.2,
                        cache_write: agg.raw.3,
                        output: agg.raw.1,
                        reasoning: 0,
                        // 无 total 判据 → 声明互斥（字段名 uncachedInputTokens 即互斥）
                        total_raw: None,
                    };
                    let sem = resolve_semantics(default_policy(UsageSourceId::Dsh), &raw);
                    // 供应商一律经唯一入口（GC 6）——含从游标继承来的这一路
                    let (provider, _) = b.provider_of(Some(&provider), &model);
                    let key = DetailKey {
                        session_id: header.id.clone(),
                        hour_key: hour.clone(),
                        day_key: hour.chars().take(10).collect(),
                        project_key: project_key_of(Some(&cwd)),
                        model: model.clone(),
                        provider,
                    };
                    let d = b.detail(key, agg.first_ms);
                    if let Some(n) = normalize(&raw, sem) {
                        d.buckets.add(&n.buckets);
                        d.request_total += n.request_total;
                    }
                    // 请求数 = 逐事件条数（真机 `steps` 与 `assistant/message` 条数同源）
                    d.requests += agg.events;
                    b.new_records += agg.events;
                    e_total.0 += agg.raw.0;
                    e_total.1 += agg.raw.1;
                    e_total.2 += agg.raw.2;
                    e_total.3 += agg.raw.3;
                    e_events += agg.events;
                }
                // **`D − E` 对账（任务书 §3.3）**：投影缓存当前累计 −（已记录水位 + 本轮实测）
                // 的正零头只能补录（只进全天账）；差额 ≤ 0 ⇒ 什么都不记（绝不能重复入账）。
                // 用「已记录水位 R」而不是「本轮增量 D」的理由见 `emit_backfill` 文档。
                if let Some(item) = pending_backfill.remove(&header.id) {
                    emit_backfill(b, &header.id, &item, e_total, e_events, r_prev, ctx.now_ms);
                    claimed_p_now = Some(item.p_now);
                }
            } else {
                out_of_scope += 1;
            }
            // 日志侧游标 = **两维水位**（键 `log:<session>`，与投影缓存侧的会话键分开存，
            // 因为两侧的「上次快照」语义完全不同）：
            // * `byte_offset` = **逐事件用量的解压正文字节水位**（最后一条已消费事件的行尾）。
            //   落在**行边界**上，只前进、不按时间戳推进（时间戳会重复/回退，按字节才不会
            //   重复入账）。**不是**压缩文件偏移：帧边界与事件行边界不对齐，按压缩偏移续读
            //   会读到半行（见 `event_tail_plan` 的四条结构判据）。
            // * `ordinal` = 已消费事件条数（「字节双水位的第二维」，与 `byte_offset` 互相校验）。
            // * `last_cumulative` = **已记录水位 R**（本会话实测 + 历次补录的累计）。它与投影缓存侧
            //   会话键游标的同名字段**不是**同一个量：那个是「投影缓存侧的已入账累计」，这个是
            //   「日志侧已经记过的量」—— 两者在「缓存与日志严丝合缝」时相等，而**对账要用 R**：
            //   日志领先投影缓存一轮时，R 已经把那一轮实测过的量记进去了（见 `emit_backfill`）。
            // * `mtime_ms` / `file_size` = 该代际文件的真实代际（W-23：不得填语义无关的垃圾值）。
            // * `fingerprint` 留空（= 本游标不做 sha256 前缀指纹），改写检测由
            //   `event_tail_plan` 的四条结构判据负责（代际一致 / 水位不越界 / 水位落在事件行尾 /
            //   ordinal 自洽）——**不用哈希**的理由：哈希要么每轮重读整份前缀（O(正文)），
            //   要么为每条事件存一份摘要（内存 × 事件数），而四条结构判据在真机形态下
            //   已经足够（重写/截断必破其中至少一条）。
            let mut cur = ctx.cursor_of(&log_key).cloned().unwrap_or_default();
            cur.session_id = log_key;
            if in_scope {
                // 字节/条数水位推到**文件里最后一条已消费事件的行尾**（不是「本轮新增的第一条
                // 之前」）：漏推 = 下一轮重复入账；推过撕裂写的尾帧 = 那批事件永久丢失。
                let (bytes, ordinal, _events_sum) = event_watermark_of(&digest.usage_events);
                cur.byte_offset = bytes;
                cur.ordinal = ordinal;
                // `last_cumulative` = **已记录水位 R**（本会话实测 + 补录的累计），
                // 不是「事件四桶累计」（两者只在「日志与投影缓存严丝合缝」时相等，
                // 而 R 才是对账要用的那个量）。投影缓存本轮没读到时（`(mtime,size)` 快速路）
                // R 只按本轮实测增长 —— 这正是日志领先一轮时不重复入账的原因。
                let measured = e_total.0 + e_total.1 + e_total.2 + e_total.3;
                let r_now = match claimed_p_now {
                    Some(p) => (r_prev + measured).max(p.0 + p.1 + p.2 + p.3),
                    None => r_prev + measured,
                };
                cur.last_cumulative = Some(r_now);
            }
            cur.mtime_ms = digest.mtime_ms;
            cur.file_size = digest.text_len as i64;
            cur.state_json = serde_json::to_string(&DshLogState {
                // **M3**：记住产出这份水位的**代际**
                gen: digest.version,
                error_model: facts.error_model,
                error_turn: facts.error_turn,
                error_tool: facts.error_tool,
                interrupted: facts.interrupted,
                tool_calls: facts.tool_calls,
                turn_samples: now_samples as i64,
                tool_stats: facts.tool_stats.clone(),
                tool_unpaired: facts.tool_unpaired,
                // **D-41**：模型 / 供应商随水位跨轮持久化（本项目其它源同款纪律）
                model,
                provider,
            })
            .unwrap_or_default();
            b.push_cursor(cur);
        }
    }
    log_scan.retain_existing(&live);
    if unpaired > 0 {
        log::warn!(
            "usage/dsh: 本轮 {unpaired} 次工具调用拿不到耗时（无 callId / 无回执 / 任一侧无时间戳）\
             → 只计次数、不计耗时（不写假 0，GC 7）"
        );
    }
    if no_model_rows > 0 {
        log::warn!(
            "usage/dsh: 本轮 {no_model_rows} 个会话的原始日志事实落在空模型行 \
             （该会话投影缓存结构性缺 modelSelection，矩阵 §5.1/§C.2；不得静默）"
        );
    }
    if out_of_scope > 0 {
        log::warn!(
            "usage/dsh: 本轮 {out_of_scope} 个纯日志会话（没有投影缓存文件、也没有已入账累计）\
             **不入账**（日志只提供时间、量的权威是投影缓存）——水位不推进，等它进范围再算"
        );
    }
    Ok(())
}

/// 原始日志侧的**事实计数水位**（B4）。与投影缓存侧的 `DshState` **分开**存
/// （游标键 `log:<session_id>`），因为两侧的「上次快照」语义不同。
#[derive(Default, Clone, serde::Serialize, serde::Deserialize)]
struct DshLogState {
    /// **M3**：产出这份水位的日志**代际**（`GenerationRead.version`）。
    /// 0 = 旧版游标（没有这个字段）→ 不做代际判定，行为与修复前一致。
    #[serde(default)]
    gen: i64,
    error_model: i64,
    error_turn: i64,
    error_tool: i64,
    interrupted: i64,
    tool_calls: i64,
    /// `facts.turn_ms` 的样本条数（只取新增尾部样本）
    turn_samples: i64,
    /// **逐工具累计水位**：工具名 → (调用次数, 累计耗时毫秒)
    #[serde(default)]
    tool_stats: std::collections::BTreeMap<String, (i64, i64)>,
    /// 计数了但拿不到耗时的工具调用数（无 callId / 无回执 / 任一侧无 time）
    #[serde(default)]
    tool_unpaired: i64,
    /// **D-41 同族**：该会话最后一次已知的模型 / 供应商（跨轮持久化）。
    /// 原始日志侧不产生模型字段，而 63/171 个投影文件**结构性**地缺 `modelSelection` 行 ——
    /// 有值就必须记住，免得下一轮把报错/工具计数落进 `model=""` 幽灵行。
    #[serde(default)]
    model: String,
    #[serde(default)]
    provider: String,
}

/// L2 缓存的原始日志**扫描产物**（纯内容产物：同一份文本必然产出同一份产物，
/// 不含任何随轮次变化的东西——这是它能进 `(mtime,size)` 摘要缓存的前提）。
#[derive(Debug, Clone)]
struct LogDigest {
    /// 代际号（`session.vN` 的 N；`session.jsonl` = 0）
    version: i64,
    /// 该代际文件的 mtime（毫秒）：B 段归桶的第二顺位来源
    mtime_ms: i64,
    header: crate::monitor::dsh::log::DshHeader,
    facts: LogFacts,
    /// **解压正文的总字节数**（续读水位的上界判据：水位 > 它 = 文件被重写/截断 → 重新定界）
    text_len: usize,
    /// 本文件里**逐事件**的用量（`assistant/message` + `data.usage` + `time`），按行序。
    /// 这是「时间归属」的唯一数据源（A 段的投影缓存只有累计值、没有时间）。
    usage_events: Vec<EventUsage>,
}

impl LogDigest {
    /// **一遍扫描**（GC 4「一次扫描产出全部指标」）：报错三层 / 打断 / turn 时长样本 /
    /// 逐工具（次数 + 耗时）全在同一次 `parse_events` 之上产出。
    ///
    /// 逐工具口径（真机实测）：配对键 `tool/call.data.callId` ↔
    /// `tool/result.data.message.source.callId`（**不是同一个字段名，也不是同一层**，见
    /// `result_call_id` 的文档），耗时 = `tool/result.time − tool/call.time`；
    /// 真机 `callId` **全不重复**（没有 codex 式相邻重放）。
    fn of(read: &crate::monitor::dsh::log::GenerationRead) -> Option<Self> {
        use crate::monitor::dsh::log::{parse_events, parse_header};
        let header = parse_header(&read.text)?;
        let mut facts = LogFacts::default();
        let mut open_start: Option<i64> = None;
        // 本文件内的 未回执调用表：callId → (工具名, 调用时刻)
        let mut open_calls: HashMap<String, (String, i64)> = HashMap::new();
        for e in parse_events(&read.text) {
            if let Some(t) = e.time {
                facts.last_event_ms = Some(facts.last_event_ms.map_or(t, |m| m.max(t)));
            }
            match e.kind.as_str() {
                "turn/start" => open_start = e.time,
                "turn/end" => {
                    if let (Some(s), Some(end)) = (open_start.take(), e.time) {
                        facts.turn_ms.push((end - s).max(0));
                    }
                    match e
                        .data
                        .pointer("/reason/kind")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                    {
                        "error" => facts.error_turn += 1,
                        "aborted" | "interrupted" => facts.interrupted += 1,
                        _ => {}
                    }
                    if e.data.pointer("/reason/error/code").is_some() {
                        facts.error_model += 1;
                    }
                }
                "llm/retry" => facts.error_model += 1,
                "tool/call" => {
                    facts.tool_calls += 1;
                    // 工具名缺失时落 `unknown` 桶（真机 14,148/14,148 全部带 `name`；
                    // 与 claude 采集器同款兜底）——**这样 Σ tool_stats.0 才恒等于 tool_calls**
                    let name = e
                        .data
                        .get("name")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                        .unwrap_or("unknown")
                        .to_string();
                    facts.tool_stats.entry(name.clone()).or_insert((0, 0)).0 += 1;
                    match (e.data.get("callId").and_then(|v| v.as_str()), e.time) {
                        (Some(id), Some(t)) => {
                            open_calls.insert(id.to_string(), (name, t));
                        }
                        // 没有 callId 或没有时间戳 → 次数照记、耗时不计（下面计数 + 告警）
                        _ => facts.tool_unpaired += 1,
                    }
                }
                "tool/result" => {
                    // 工具失败判据：真机 `data.error` 是在场对象 `{name, code}`
                    //（406/406 全为 dict，无 null、无其它形状）
                    let is_err = e.data.get("error").map(|x| !x.is_null()).unwrap_or(false);
                    if is_err {
                        facts.error_tool += 1;
                    }
                    if let Some(id) = result_call_id(&e.data) {
                        if let Some((name, t0)) = open_calls.remove(id) {
                            match e.time {
                                Some(t1) => {
                                    facts.tool_stats.entry(name).or_insert((0, 0)).1 +=
                                        (t1 - t0).max(0);
                                }
                                // 回执没有时间戳 → 同样只计次数、不计耗时
                                None => facts.tool_unpaired += 1,
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        // 整份日志读完仍没有回执的调用（被用户打断 / 无回执）→ 计入 unpaired（计数 + 告警，不写假 0）
        facts.tool_unpaired += open_calls.len() as i64;
        Some(LogDigest {
            version: read.version,
            mtime_ms: read.mtime_ms,
            header,
            facts,
            text_len: read.text.len(),
            usage_events: usage_events_of(&read.text),
        })
    }
}

/// 原始日志里**一条带用量的助手消息**（`assistant/message` + `data.usage` + `time`）。
///
/// **纯内容产物**（同一份正文必然产出同一份列表）⇒ 随 `LogDigest` 整体进 L2 `(mtime,size)`
/// 摘要缓存：同一代际只解一次码、只扫一遍行（GC 3/5）。
///
/// **真机形态（2026-10-07 全库只读实测，327 份日志 / 27,183 条 `assistant/message`）**：
/// * 用量在 **`data.usage`**，**顶层 `usage` 真机 0 条**（仓库那份 `sample*.sanitized.jsonl`
///   黄金夹具把 usage 放在顶层 —— 照夹具写就是真机零命中，D-29/D-36 同族假绿）；
/// * `inputTokens` / `outputTokens` / `cacheReadTokens`(99.5%) / `cacheWriteTokens`(仅 12 条且全 0)；
/// * `inputTokens` 是**未缓存新增**、与 `cacheReadTokens` 互斥
///   （`input + output + cacheRead == totalTokens` 恒成立）；
/// * `time` 是**顶层**毫秒 epoch，`assistant/message` 100% 带（本采集器不发明时间）；
/// * `compaction` / `summary` 事件**也带** `data.usage`，但**不计入**投影缓存 `totals`
///   ⇒ 逐事件累加**必须**按 `type == "assistant/message"` 过滤，否则总量对不上；
/// * 每份日志的事件四桶之和与投影缓存 `tokenUsage.val.totals` **逐字段相等**
///   （319 份合计 5,607,849,531 vs 5,607,450,511，比 1.00007）—— 这是 `D − E`
///   对账（`event_tail_plan`）能成立的前提，也是「总量不偏」的实证。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EventUsage {
    /// 该事件所在行在**解压正文**里的结束字节偏移（含行尾 `\n`）。
    /// 续读水位（`usage_cursor.byte_offset`）就落在这些边界上：**按字节推进**，
    /// **不按时间戳推进**（时间戳会重复/回退，按字节才不会重复入账）。
    text_end: usize,
    /// 事件自己的毫秒时间戳（`event.time`）——归属的**唯一**依据
    time_ms: i64,
    /// 四桶，**与 `DshState.total` 同序**：(uncachedInput, output, cacheRead, cacheWrite)
    buckets: (i64, i64, i64, i64),
}

impl EventUsage {
    fn sum(&self) -> i64 {
        let (fresh, output, cache_read, cache_write) = self.buckets;
        fresh + output + cache_read + cache_write
    }
}

/// 一个**小时桶**里本轮新增事件的聚合（逐事件归属的中间产物）。
struct EventHourAgg {
    /// 四桶原始值，与 `EventUsage::buckets` 同序
    raw: (i64, i64, i64, i64),
    /// 该小时的事件条数（= 请求数）
    events: i64,
    /// 该小时最早的事件时刻（明细行的 `ts_ms`，只影响 `first_seen_at` 的可读性）
    first_ms: i64,
}

/// 逐事件用量抽取：**同一次解码产物**里再扫一遍行（零额外解压）。
///
/// 只认「整行以 `\n` 收尾」的行：撕裂写的半行**不消费**（不计入水位），下一轮补全后再计
/// ——绝不把半行当整行（否则水位越过该事件，它的用量永久丢失）。
fn usage_events_of(text: &str) -> Vec<EventUsage> {
    let mut out = Vec::new();
    let mut offset = 0usize;
    for line in text.split_inclusive('\n') {
        offset += line.len();
        if !line.ends_with('\n') {
            continue; // 撕裂写的尾行：不消费
        }
        // 廉价预过滤：绝大多数行不含这个子串（省掉一次完整 JSON 解析）
        if !line.contains("assistant/message") {
            continue;
        }
        let Ok(e) = serde_json::from_str::<crate::monitor::dsh::log::DshEvent>(line) else {
            continue;
        };
        if e.kind != "assistant/message" {
            continue;
        }
        // 缺时间或缺用量 → 不参与归属（也绝不填 0 假装量到）
        let (Some(time_ms), Some(usage)) =
            (e.time, e.data.get("usage").and_then(|v| v.as_object()))
        else {
            continue;
        };
        let g = |k: &str| usage.get(k).and_then(|v| v.as_i64()).unwrap_or(0);
        out.push(EventUsage {
            text_end: offset,
            time_ms,
            buckets: (
                g("inputTokens"),
                g("outputTokens"),
                g("cacheReadTokens"),
                g("cacheWriteTokens"),
            ),
        });
    }
    out
}

/// 逐事件续读计划：从哪条事件起算是「本轮新增」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EventTailPlan {
    /// 新事件 = `usage_events[start..]`（**一直消费到文件末尾**）
    start: usize,
    /// 本次是**重新定界**（首轮 / 代际切换 / 水位不可用），不是续读
    rearmed: bool,
}

/// 一轮结束后的水位三元组：`(解压正文字节, 已消费事件条数, 已消费四桶累计)`。
///
/// **水位永远落在文件里最后一条已消费事件的行尾**（不是「本轮新增的第一条之前」）——
/// 这是「消费到末尾」的直接后果：漏推水位会让下一轮把同一批事件再计一遍（重复入账），
/// 推过头（越过撕裂写的尾帧）会让那批事件永久丢失。首轮（bootstrap）也必须推到位。
fn event_watermark_of(events: &[EventUsage]) -> (i64, i64, i64) {
    (
        events.last().map(|e| e.text_end as i64).unwrap_or(0),
        events.len() as i64,
        events.iter().map(EventUsage::sum).sum(),
    )
}

/// 定界：`accounted` = 投影缓存侧**已入账**的累计（会话键游标的 `last_cumulative`）。
///
/// 三条规则（口径见任务书 §3）：
/// 1. **没有已入账累计**（首轮 / 账本为空）→ 从 0 起：整份日志按它自己的事件时间入账
///    （bootstrap 的历史落回**真实小时**，不落进采集当轮）；
/// 2. **有已入账累计 `C`** → 跳过前缀累计 `≤ C` 的事件。跨界的那一条（`P(i) < C < P(i+1)`）
///    也跳过：**「绝不重复入账」是硬要求**，宁可少算一条事件的零头 —— 那点零头由
///    `D − E` 对账兜住（进补录）。真机 319/319 份日志的事件合计与投影缓存 totals 逐字段相等
///    ⇒ 这个定界在真机上是**逐字节精确**的，不会系统性偏移；
/// 3. **日志全部事件之和 < C**（日志被清理 / 投影缓存跑在前面 / 换了非超集代际）→ 全部跳过：
///    本轮 E = 0，差额 `D − E` 走补录（只进全天账，不进任何短窗口）。
fn event_tail_plan(
    digest: &LogDigest,
    prev: Option<&CursorDelta>,
    prev_gen: i64,
    accounted: Option<i64>,
) -> EventTailPlan {
    let events = &digest.usage_events;
    let text_len = digest.text_len as i64;
    // ① 续读：水位可用才继续。四条结构判据任一不成立 → 重新定界：
    //    * 代际一致（换文件 = 偏移空间换了）；
    //    * 水位不越过正文（越过 = 重写/截断/中间帧丢失）；
    //    * 水位**正好落在一条事件的行尾**上（不是半行、也不是行中间）；
    //    * `ordinal` 与「该水位之前的事件条数」自洽（两维水位互相校验）。
    if let Some(c) = prev {
        // **必须先有本实现写下的「已记录水位 R」**（`last_cumulative`）才算「有可用水位」。
        // 第 14 轮之前的日志游标里 `byte_offset` / `ordinal` **一律是 0**（当时的语义是
        // 「本游标不按字节判定 ⇒ 0 = 不适用」）——把它当成「有效水位 = 停在文件开头」，
        // 会把**整份日志当成本轮新增**：老游标 + 新代码的第一轮就会把每个会话的**全部历史**
        // 再入账一遍（实测在开发机上真发生过：dev 应用热重载跑了一轮，账本被灌到 4×）。
        // R 是本实现才写的字段，它缺席即「没有可用水位」⇒ 重新定界（按已入账累计定界）。
        let start = c.ordinal.max(0) as usize;
        let boundary_ok = c.last_cumulative.is_some()
            && start <= events.len()
            && c.byte_offset <= text_len
            && (start == 0 || events[start - 1].text_end as i64 == c.byte_offset);
        if prev_gen == digest.version && boundary_ok {
            return EventTailPlan {
                start,
                rearmed: false,
            };
        }
    }
    // ② 重新定界
    let mut acc = 0i64;
    let mut start = events.len();
    if let Some(c) = accounted.filter(|c| *c > 0) {
        for (i, e) in events.iter().enumerate() {
            acc += e.sum();
            if acc >= c {
                start = i + 1;
                break;
            }
        }
    } else {
        start = 0; // 首轮：整份日志都是「本轮新增」
    }
    EventTailPlan {
        start,
        rearmed: true,
    }
}

/// 回执侧的配对键解析（**真机形态优先**，逐层兜底）。
///
/// 真机 173 文件全量实测：**14,584/14,584** 条 `tool/result` 的键在
/// `data.message.source.callId`（对象形态 `{"kind":"tool","callId":"call_…"}`），
/// 配对率 **14,552/14,554 = 100.0%**（矩阵旧口径 1,299/1,302）；
/// 而**任务书正文写的顶层 `data.toolCallId` 真机一个都没有**（探测文档里的旧样本形态
/// `data.message.toolCallId` 也只有 72 条）——照抄扁平形态会让配对率恒 0、
/// 逐工具耗时**全 0**，而任务书夹具恰好把它放在顶层 → 用例全绿（D-29 / D-36 同族假绿）。
/// 三层都认（新形态 → 旧形态 → 任务书形态），只为形状演进兜底，**不以它们为主判据**。
fn result_call_id(data: &Value) -> Option<&str> {
    data.pointer("/message/source/callId")
        .and_then(|v| v.as_str())
        .or_else(|| data.pointer("/message/toolCallId").and_then(|v| v.as_str()))
        .or_else(|| data.get("toolCallId").and_then(|v| v.as_str()))
        .filter(|s| !s.is_empty())
}

/// 一份原始日志的**累计事实**（纯内容产物，整份进 L2 缓存）
#[derive(Debug, Default, Clone)]
struct LogFacts {
    turn_ms: Vec<i64>,
    error_model: i64,
    error_turn: i64,
    error_tool: i64,
    interrupted: i64,
    tool_calls: i64,
    /// 逐工具累计：工具名 → (调用次数, 累计耗时毫秒)。
    /// **只有能配对的调用才计耗时**（配不上/缺时间戳的只计次数，见 `tool_unpaired`）——
    /// 绝不写「假 0 耗时」（GC 7）。
    tool_stats: std::collections::BTreeMap<String, (i64, i64)>,
    /// 该日志里**最后一条带时间的事件**的毫秒时间戳（裁决 B：归桶用真实活动时间）
    last_event_ms: Option<i64>,
    /// 计数了但拿不到耗时的工具调用数
    tool_unpaired: i64,
}

// ==== usage 自省锁：以下为排除区（测试代码），勿删勿复制 ====
#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::usage::caps::caps_of;
    use crate::services::usage::collect::{CollectContext, CollectStats};
    use crate::services::usage::delta::{CursorDelta, DetailDelta};
    use crate::services::usage::model::SourceKind;
    use serde_json::json;
    use std::collections::HashMap;

    /// **本文件所有用例串行**（家族先例：`monitor::kimi_parser` 的 `HOME_LOCK`，见 D-30）。
    ///
    /// 理由：日志侧的 L2 缓存是**进程级全局注册表**（`monitor::session_scan::registry`），
    /// 而 `retain_existing(&live)` 会清掉**同 namespace 里不在本轮 live 集合中的条目** ——
    /// 并行跑的另一个用例的本轮 live 集合里当然没有本用例的临时文件 → 顺手把本用例的缓存条目
    /// 清掉 → 「未变代际零解码零解析」这类**路径类断言**（裁决 C）会随机变红（实测：
    /// 单跑 5/5 绿、全文件并行跑时偶发红）。**生产侧不存在这个问题**：一个源一个采集器，
    /// 进程内只有一份 live 集合。锁本身不改变任何生产行为。
    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn serial() -> std::sync::MutexGuard<'static, ()> {
        TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 投影缓存里 `subagent` 行的**三种真机形态**（W-30）。
    #[derive(Debug, Clone, Copy, PartialEq)]
    enum SubAgentRow {
        /// 连该行都没有（旧快照文件）
        Missing,
        /// **真机父会话形态**：行在场、`val` 是**空对象** `{}`（真机 57/170）
        ParentEmpty,
        /// **真机子会话形态**：`val.identity` 非空（真机 113/170）
        ChildIdentity,
    }

    /// 写一个投影缓存文件。**真机形态**：`version:7` + 多行缩进 JSON + 行尾 `\n`
    /// （真机 170/170 都是如此）。采集器经 `read_incremental` 读它——
    /// **没有行尾换行 = 一行完整行都切不出来**，夹具必须同形（D-29 同族：夹具绕开真机形态 = 假绿）。
    #[allow(clippy::too_many_arguments)]
    fn projcache(
        home: &std::path::Path,
        id: &str,
        cwd: &str,
        totals: (i64, i64, i64, i64),
        turns: i64,
        steps: i64,
        tool_ms: i64,
        subagent: SubAgentRow,
        model: Option<(&str, &str)>,
    ) {
        projcache_full(
            home, id, cwd, 1, totals, turns, steps, tool_ms, subagent, model,
        );
    }

    /// 同上，但显式给 `record.identity.createdAt`（**裁决 B 的归桶锁**要它跟采集时刻分处不同小时）
    #[allow(clippy::too_many_arguments)]
    fn projcache_full(
        home: &std::path::Path,
        id: &str,
        cwd: &str,
        created_at: i64,
        totals: (i64, i64, i64, i64),
        turns: i64,
        steps: i64,
        tool_ms: i64,
        subagent: SubAgentRow,
        model: Option<(&str, &str)>,
    ) {
        let d = home.join("storages/session_projcache/sessions");
        std::fs::create_dir_all(&d).unwrap();
        // `val.last == null`（真机 10/170）＝本源 TotalOnly 等价情形 → 只走 totals
        let mut rows = json!({
            "tokenUsage": {"ver":2,"seq":1,"val":{"totals":{
                "uncachedInputTokens":totals.0,"outputTokens":totals.1,
                "cacheReadTokens":totals.2,"cacheWriteTokens":totals.3},
                "last": null}},
            "sessionStats": {"ver":1,"seq":1,"val":{"turns":turns,"steps":steps,"toolMs":tool_ms}}
        });
        if let Some((p, m)) = model {
            rows["modelSelection"] =
                json!({"ver":1,"seq":1,"val":{"lastUsed":{"provider":p,"model":m}}});
        }
        match subagent {
            SubAgentRow::Missing => {}
            SubAgentRow::ParentEmpty => {
                rows["subagent"] = json!({"ver":1,"seq":1,"val":{}});
            }
            SubAgentRow::ChildIdentity => {
                rows["subagent"] =
                    json!({"ver":1,"seq":1,"val":{"identity":{"mode":"continuable","label":"x"}}});
            }
        }
        let rec = json!({"version":7,"record":{"identity":{"formatVersion":3,"createdAt":created_at,"cwd":cwd},"rows":rows}});
        let body = format!("{}\n", serde_json::to_string_pretty(&rec).unwrap());
        std::fs::write(d.join(format!("{id}.json")), body).unwrap();
    }

    /// 行 ⇒ `\n` 结尾的 JSONL 正文
    fn log_body(events: &[serde_json::Value]) -> String {
        events
            .iter()
            .map(|e| format!("{}\n", serde_json::to_string(e).unwrap()))
            .collect()
    }

    /// 写一份**真机代际布局**的原始日志：`sessions/<proj>/<dir>/session.v3.jsonl.zstd`
    /// （与既有 `monitor/dsh` 日志测试同款 zstd 编码）。`events[0]` 必须是 session 头。
    fn write_log(
        home: &std::path::Path,
        proj: &str,
        dir: &str,
        events: &[serde_json::Value],
    ) -> std::path::PathBuf {
        let sess_dir = home.join("sessions").join(proj).join(dir);
        std::fs::create_dir_all(&sess_dir).unwrap();
        let p = sess_dir.join("session.v3.jsonl.zstd");
        let frame = zstd::stream::encode_all(log_body(events).as_bytes(), 3).unwrap();
        std::fs::write(&p, frame).unwrap();
        p
    }

    /// 追加**一帧** zstd（真机 `.zstd` 就是多帧拼接，`decode_zstd_frames` 专为它存在）
    fn append_log(path: &std::path::Path, events: &[serde_json::Value]) {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        f.write_all(&zstd::stream::encode_all(log_body(events).as_bytes(), 3).unwrap())
            .unwrap();
    }

    /// 采集一轮（home 注入版）
    fn run(
        home: &std::path::Path,
        cursors: &HashMap<String, CursorDelta>,
    ) -> (SourceDelta, CollectStats) {
        let ctx = CollectContext::new(home, 1_700_000_000_000, cursors);
        let delta = DshCollector::collect_with_home(&ctx, home).unwrap();
        (delta, ctx.stats())
    }

    fn cursors_of(d: &SourceDelta) -> HashMap<String, CursorDelta> {
        d.cursors
            .iter()
            .map(|c| (c.session_id.clone(), c.clone()))
            .collect()
    }

    /// 取某会话的用量行：`backfill = false` 取**实测**行（逐事件归属 / 会话级事实），
    /// `true` 取**补录**行（只进全天账）。
    ///
    /// **必须显式选行，不能用「第一条 session_id 命中的行」**：第 14 轮起一个会话在同一轮里
    /// 可以同时有事实行与补录行，而 `SourceDelta.details` 是 `HashMap` 出来的（**序不定**）
    /// —— 随手 `.find()` 会让用例变成顺序依赖的 flake。
    fn row_of<'a>(d: &'a SourceDelta, sid: &str, backfill: bool) -> &'a DetailDelta {
        d.details
            .iter()
            .find(|x| x.key.session_id == sid && x.cache_semantics.is_backfill() == backfill)
            .unwrap_or_else(|| {
                panic!(
                    "缺会话 {sid} 的行（backfill={backfill}）；现有：{:?}",
                    d.details
                        .iter()
                        .map(|x| (
                            x.key.session_id.clone(),
                            x.key.hour_key.clone(),
                            x.cache_semantics
                        ))
                        .collect::<Vec<_>>()
                )
            })
    }

    #[test]
    fn cumulative_diff_subagent_layering_and_missing_model_row() {
        let _serial = serial();
        let dir = tempfile::tempdir().unwrap();
        projcache(
            dir.path(),
            "par",
            "/p/A",
            (100, 10, 50, 0),
            3,
            9,
            1000,
            SubAgentRow::Missing,
            Some(("ollama-pro", "deepseek-v4.1-flash")),
        );
        projcache(
            dir.path(),
            "sub",
            "/p/A",
            (20, 5, 0, 0),
            1,
            2,
            300,
            SubAgentRow::ChildIdentity,
            None,
        );
        // 空游标表必须先绑成变量：`&HashMap::new()` 是**临时值**，借用进 `ctx` 就是
        // E0716（temporary value dropped while borrowed）——语句结束即释放。
        let no_cursors = HashMap::new();
        let ctx = CollectContext::new(dir.path(), 1_700_000_000_000, &no_cursors);
        let d1 = DshCollector::collect_with_home(&ctx, dir.path()).unwrap();
        // 首轮 = 全量（无上次累计）
        let out: i64 = d1.details.iter().map(|d| d.buckets.output).sum();
        assert_eq!(out, 15, "10 + 5：首轮是全量累计");
        // 子代理分层（D17）：47/99 文件本身是子代理会话 → 会话维度 is_subagent=true
        assert!(
            d1.sessions
                .iter()
                .find(|s| s.session_id == "sub")
                .unwrap()
                .is_subagent
        );
        assert!(
            !d1.sessions
                .iter()
                .find(|s| s.session_id == "par")
                .unwrap()
                .is_subagent
        );
        // 缺 modelSelection 行（真机 63/170）→ 模型/供应商为空，但四桶与 turns 照常
        let sub = row_of(&d1, "sub", false);
        assert_eq!(sub.key.model, "");
        assert_eq!(sub.counters.turns, 1);
        // 供应商不可得 → 空串 + **未知态**（经 provider_of 记录；不得猜、不得填 "unknown" 字面量）
        assert_eq!(sub.key.provider, "");
        assert_eq!(sub.provider_kind, SourceKind::Unknown);
        // **四桶映射 + 语义**：`uncachedInputTokens` 即互斥 → Exclusive（本机无 total 判据），
        // 请求输入 = 未缓存 + 缓存读 + 缓存写（命中率由查询层算，采集器不得自己算，GC 6/W-9）。
        // **第 14 轮起四桶在「用量行」上**：这两个夹具**没有原始日志** ⇒ 投影缓存差额只能补录
        //（全天账行），所以这里取的是 backfill 行 —— 有日志时同一断言落在实测行上
        //（见 `cross_hour_events_land_in_their_own_hours`）。
        let par = row_of(&d1, "par", true);
        assert_eq!(par.buckets.input_fresh, 100);
        assert_eq!(par.buckets.cache_read, 50);
        assert_eq!(par.buckets.cache_write, 0);
        assert_eq!(par.request_total, 150, "Exclusive：100 + 50 + 0");
        // turns / toolMs（真机 99/99 存在；turns 与原始 turn/start 精确相等）——
        // 这两个是**会话级事实**，没有逐事件来源，仍在事实行上
        let par_facts = row_of(&d1, "par", false);
        assert_eq!(par_facts.counters.turns, 3);
        assert_eq!(par_facts.counters.tool_ms, 1000);
        assert_eq!(
            par.requests, 9,
            "dsh 无逐请求字段 → 请求数用 sessionStats.steps（LLM 调用步数）；\
             补录行的请求数 = steps 差额 − 本轮已按事件计入的条数"
        );
        // 供应商经唯一入口解析出的三态必须落进明细行（GC 6：不得退化成 unknown）
        assert_eq!(par.key.provider, "ollama-pro");
        assert_eq!(par.provider_kind, SourceKind::Measured);
        // W-50：文件型源 → parsed_files 记**本轮真读到的文件数**（不是会话数）
        assert_eq!(d1.parsed_files, 2);
        // 二轮：文件更新（累计增长）→ **只计差值**，不重复计
        projcache(
            dir.path(),
            "par",
            "/p/A",
            (150, 25, 60, 0),
            4,
            11,
            1500,
            SubAgentRow::Missing,
            Some(("ollama-pro", "deepseek-v4.1-flash")),
        );
        let cursors: HashMap<_, _> = d1
            .cursors
            .iter()
            .map(|c| (c.session_id.clone(), c.clone()))
            .collect();
        let ctx2 = CollectContext::new(dir.path(), 1_700_000_000_000 + 60_000, &cursors);
        let d2 = DshCollector::collect_with_home(&ctx2, dir.path()).unwrap();
        let out2: i64 = d2.details.iter().map(|d| d.buckets.output).sum();
        assert_eq!(out2, 15, "差值：输出 25 - 10 = 15");
        let par2 = row_of(&d2, "par", false);
        assert_eq!(par2.counters.turns, 1, "turns 差值 4 - 3 = 1");
        assert_eq!(par2.counters.tool_ms, 500, "toolMs 差值 1500 - 1000");
        assert_eq!(
            row_of(&d2, "par", true).requests,
            2,
            "steps 差值 11 - 9（本轮没有逐事件用量 ⇒ 补录行带走全部请求数）"
        );
    }

    /// **C3 回归**：投影缓存游标的 `mtime_ms` / `file_size` 必须是**真实文件元数据**。
    /// 旧实现写的是 `now_total.0`（token 累计数）——一个语义完全无关的垃圾值：
    /// 任何将来按 `file_unchanged()` 语义使用该列的代码都会得到错误结论。
    #[test]
    fn projcache_cursor_carries_real_mtime_and_size_not_token_total() {
        let _serial = serial();
        let dir = tempfile::tempdir().unwrap();
        projcache(
            dir.path(),
            "par",
            "/p/A",
            (100, 10, 50, 0),
            3,
            9,
            1000,
            SubAgentRow::Missing,
            Some(("ollama-pro", "deepseek-v4-flash")),
        );
        let path = dir
            .path()
            .join("storages/session_projcache/sessions/par.json");
        let md = std::fs::metadata(&path).unwrap();
        let real_mtime = md
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        let real_size = md.len() as i64;

        // 空游标表必须先绑成变量：`&HashMap::new()` 是**临时值**，借用进 `ctx` 就是
        // E0716（temporary value dropped while borrowed）——语句结束即释放。
        let no_cursors = HashMap::new();
        let ctx = CollectContext::new(dir.path(), 1_700_000_000_000, &no_cursors);
        let d1 = DshCollector::collect_with_home(&ctx, dir.path()).unwrap();
        let cur = d1.cursors.iter().find(|c| c.session_id == "par").unwrap();
        assert_eq!(cur.file_size, real_size, "file_size 必须是真实字节数");
        assert!(
            (cur.mtime_ms - real_mtime).abs() <= 2000,
            "mtime_ms 必须是真实文件 mtime（旧实现写的是 token 累计数 100）：{}",
            cur.mtime_ms
        );
        assert_ne!(cur.mtime_ms, 100, "不得再把 token 累计数当 mtime（C3）");
        // **投影缓存的水位语义（有意为之，锁住）**：整份重写的 JSON 文档没有可用的字节水位 →
        // `byte_offset` 恒 0（配合 `(mtime,size)` 变化即从 0 全量重读）；**不得**记真实水位。
        assert_eq!(
            cur.byte_offset, 0,
            "整份重写文档不得记字节水位（否则 append 路只回吐半份 JSON）"
        );
    }

    /// **B4 回归**：原始日志侧的报错/工具/时长**必须做增量**。
    /// 旧实现每轮把 `facts` 原样再加一遍（游标既不读也不写）→ `error_model` / `error_turn` /
    /// `error_tool` / `interrupted` / `tool_calls` / `turn_ms` 随采集轮次线性膨胀。
    /// fixture 用真实代际布局：`sessions/<proj>/<dir>/session.v3.jsonl.zstd`
    /// （`zstd::stream::encode_all` 与既有 dsh 日志测试同款编码）。
    #[test]
    fn raw_log_second_round_adds_nothing() {
        let _serial = serial();
        let dir = tempfile::tempdir().unwrap();
        let sess_dir = dir.path().join("sessions/proj-a/session-raw-1");
        std::fs::create_dir_all(&sess_dir).unwrap();
        let body = [
            r#"{"type":"session","version":3,"id":"raw-1","cwd":"/p/A"}"#,
            r#"{"type":"turn/start","time":1000}"#,
            r#"{"type":"tool/call","time":1100,"data":{"name":"Bash"}}"#,
            r#"{"type":"tool/result","time":1200,"data":{"error":{"code":"E1"}}}"#,
            r#"{"type":"turn/end","time":2000,"data":{"reason":{"kind":"error","error":{"code":"X"}}}}"#,
        ]
        .join("\n")
            + "\n";
        let frame = zstd::stream::encode_all(body.as_bytes(), 3).unwrap();
        std::fs::write(sess_dir.join("session.v3.jsonl.zstd"), frame).unwrap();

        // 空游标表必须先绑成变量：`&HashMap::new()` 是**临时值**，借用进 `ctx` 就是
        // E0716（temporary value dropped while borrowed）——语句结束即释放。
        let no_cursors = HashMap::new();
        let ctx1 = CollectContext::new(dir.path(), 1_700_000_000_000, &no_cursors);
        let d1 = DshCollector::collect_with_home(&ctx1, dir.path()).unwrap();
        let raw1 = d1
            .details
            .iter()
            .find(|d| d.key.session_id == "raw-1")
            .expect("原始日志必须产出报错/工具指标");
        assert_eq!(raw1.counters.error_tool, 1, "tool/result.error");
        assert_eq!(raw1.counters.error_turn, 1, "turn/end reason.kind=error");
        assert_eq!(raw1.counters.tool_calls, 1);
        assert_eq!(
            raw1.counters.turn_ms,
            vec![1000],
            "turn/start→turn/end 时长"
        );
        assert!(
            d1.cursors.iter().any(|c| c.session_id == "log:raw-1"),
            "原始日志侧必须落一条 `log:<session>` 事实水位游标（旧实现一条都没有）"
        );
        assert!(d1.new_records > 0, "首轮必须有新增（否则用例是空跑）");
        // W-50：原始日志侧记的同样是**文件数**（本轮真解码的文件数；未变代际不计）
        assert_eq!(d1.parsed_files, 1);
        // 该会话没有投影缓存 → 模型/供应商**结构性不可得**（真机 63/170 是这种文件）：
        // 计数照常入账（绝不因为拿不到模型而丢 tokens/报错），模型与供应商留空 + 未知态。
        assert_eq!(raw1.key.model, "");
        assert_eq!(raw1.key.provider, "");
        assert_eq!(raw1.provider_kind, SourceKind::Unknown);

        // 第二轮：同一份日志 → 一个事实都不能重复计
        let cursors: HashMap<_, _> = d1
            .cursors
            .iter()
            .map(|c| (c.session_id.clone(), c.clone()))
            .collect();
        let ctx2 = CollectContext::new(dir.path(), 1_700_000_000_000 + 60_000, &cursors);
        let d2 = DshCollector::collect_with_home(&ctx2, dir.path()).unwrap();
        let counters2: i64 = d2
            .details
            .iter()
            .map(|d| {
                d.counters.error_model
                    + d.counters.error_turn
                    + d.counters.error_tool
                    + d.counters.interrupted
                    + d.counters.tool_calls
                    + d.counters.turn_ms.len() as i64
            })
            .sum();
        assert_eq!(
            counters2, 0,
            "第二轮不得重复计入任何报错/工具事实（旧实现每轮再加一遍）"
        );
    }

    /// **W-30 语义锁（混合夹具）**：父/子**按文件自身身份**判定，三种真机形态同场。
    ///
    /// 真机 170 文件实测：`subagent.val.identity` 非空 **113**（本文件就是子代理会话）、
    /// `subagent.val = {}` **57**（父会话：**行在场**、值为空对象）、旧快照无该行。
    /// 「行在场 ⇒ 子代理」的退化实现会把 **170/170** 全判成子代理（真机后果：Task 18 按该位
    /// 硬过滤 → 父会话连会话数带 turn 数一起消失）；本用例把父会话钉成 `false` 即可变红。
    ///
    /// 同时钉住「token 总量含子代理」：两侧的四桶都必须照常入账（不得因为分层而丢数）。
    #[test]
    fn subagent_identity_comes_from_the_file_not_from_the_row_presence() {
        let _serial = serial();
        let dir = tempfile::tempdir().unwrap();
        projcache(
            dir.path(),
            "p-parent",
            "/p/A",
            (100, 10, 50, 0),
            1,
            1,
            0,
            SubAgentRow::ParentEmpty,
            None,
        );
        projcache(
            dir.path(),
            "c-child",
            "/p/A",
            (20, 5, 0, 0),
            1,
            1,
            0,
            SubAgentRow::ChildIdentity,
            None,
        );
        projcache(
            dir.path(),
            "p-old",
            "/p/A",
            (7, 3, 0, 0),
            1,
            1,
            0,
            SubAgentRow::Missing,
            None,
        );
        let no_cursors = HashMap::new();
        let (d1, _) = run(dir.path(), &no_cursors);
        let sub = |id: &str| {
            d1.sessions
                .iter()
                .find(|s| s.session_id == id)
                .unwrap_or_else(|| panic!("缺会话 {id}"))
                .is_subagent
        };
        assert!(
            !sub("p-parent"),
            "真机父形态 `subagent.val = {{}}` 不是子代理（行在场 ≠ 子代理）"
        );
        assert!(
            sub("c-child"),
            "`subagent.val.identity` 非空 = 本文件就是子代理会话"
        );
        assert!(!sub("p-old"), "缺 `subagent` 行的旧快照也是父会话");
        // 前提断言（防假绿）：三个文件都产出了明细行，上面的判定不是「空跑出来的 false」。
        // **第 14 轮起每会话两行**：会话级事实行（turns/toolMs，A 段）+ 用量行
        //（这三个夹具都没有原始日志 ⇒ 投影缓存差额只能补录 → 补录行）。
        for id in ["p-parent", "c-child", "p-old"] {
            assert!(
                d1.details
                    .iter()
                    .any(|d| d.key.session_id == id && !d.cache_semantics.is_backfill()),
                "缺 {id} 的会话级事实行"
            );
            assert!(
                d1.details
                    .iter()
                    .any(|d| d.key.session_id == id && d.cache_semantics.is_backfill()),
                "缺 {id} 的补录行"
            );
        }
        // token 总量含子代理：子会话的产出必须照常入账（分层只影响计数类，不影响四桶）
        assert_eq!(
            d1.details.iter().map(|d| d.buckets.output).sum::<i64>(),
            18,
            "10 + 5 + 3：分层不得吃掉任何一侧的 token"
        );
    }

    /// **A-4 锁（Q-3「部分同意」的落地）：两个判据源必须收敛成单一决定性取值。**
    ///
    /// dsh 的子代理身份有**两个**文件级判据源（每轮重新推导、不受增量窗口影响）：
    /// ① **投影缓存** `record.rows.subagent.val.identity`（`dsh.rs` A 段，**权威**）；
    /// ② **日志头** `monitor::dsh::log::is_subagent(header)`（B 段，**兜底**）。
    ///
    /// 修前两者各自 `b.session(...)` ⇒ `DeltaBuilder` 的 **AND** 合并会把
    /// 「投影=子代理、日志=父会话」判成**父会话**（`true & false = false`）——
    /// 即**兜底源悄悄推翻了权威源**。本用例钉住：两源同时在场且不一致时取**投影缓存**取值。
    ///
    /// **能变红**：把日志侧改回直接传 `is_subagent(header)`（或把优先级反过来）→ 第一段 FAILED。
    #[test]
    fn subagent_judges_from_cache_and_log_converge_with_cache_authoritative() {
        let _serial = serial();
        let dir = tempfile::tempdir().unwrap();
        // ① 投影=子代理 / 日志头=父会话（不一致，且**没有** origin/delegationDepth）
        projcache(
            dir.path(),
            "c-cache-wins",
            "/p/A",
            (20, 5, 0, 0),
            1,
            1,
            0,
            SubAgentRow::ChildIdentity,
            None,
        );
        write_log(
            dir.path(),
            "proj-a",
            "c-cache-wins",
            &[json!({"type":"session","version":3,"id":"c-cache-wins","cwd":"/p/A"})],
        );
        // ② 投影=父会话 / 日志头=子代理（反方向：权威源说"父"，兜底源不得把它改成"子"）
        projcache(
            dir.path(),
            "p-cache-wins",
            "/p/A",
            (10, 2, 0, 0),
            1,
            1,
            0,
            SubAgentRow::ParentEmpty,
            None,
        );
        write_log(
            dir.path(),
            "proj-a",
            "p-cache-wins",
            &[
                json!({"type":"session","version":3,"id":"p-cache-wins","cwd":"/p/A",
                     "origin":"subagent","delegationDepth":1}),
            ],
        );
        let no_cursors = HashMap::new();
        let (d, _) = run(dir.path(), &no_cursors);
        let sub = |id: &str| {
            d.sessions
                .iter()
                .find(|s| s.session_id == id)
                .unwrap_or_else(|| panic!("缺会话 {id}（两源的 session 维度必须合并到同一条）"))
                .is_subagent
        };
        // 前提（防假绿）：两源**都真的在场**——否则本用例退化成"只有一侧"的旧覆盖
        assert!(
            d.cursors.iter().any(|c| c.session_id == "c-cache-wins"),
            "前提：投影缓存侧在场（游标键 = 文件 stem）"
        );
        assert!(
            d.cursors.iter().any(|c| c.session_id == "log:c-cache-wins"),
            "前提：日志侧在场（游标键带 log: 前缀）"
        );
        assert!(
            sub("c-cache-wins"),
            "投影缓存权威：投影说子代理、日志头说父会话 → 必须判**子代理**\
             （AND 合并会让兜底源推翻权威源，这里拿 false）"
        );
        assert!(
            !sub("p-cache-wins"),
            "反方向同理：投影说父会话 → 必须判**父会话**（日志头的 subagent 不得把它翻上去）"
        );
    }

    /// **A-4 的计数锁**：两源不一致必须**带计数**（可观测，不是"一行 log 就完"）。
    /// `JudgeTally::observe` 既是**收敛判据**（返回值 = 单一决定性取值）又是计数器。
    ///
    /// **能变红**：把 `observe` 的冲突分支改成不计数（或优先级反转）→ FAILED。
    #[test]
    fn judge_tally_counts_conflicts_and_returns_the_cache_value() {
        // 纯函数用例也取串行锁：本文件有源码自省锁要求**每个** `#[test]` 都取
        // （`every_test_in_this_file_takes_the_serial_lock`，漏一个即红）
        let _serial = serial();
        let mut t = JudgeTally::default();
        // 两源都在场且一致 → 计一次比较、零冲突
        assert!(t.observe(Some(true), Some(true)));
        assert!(!t.observe(Some(false), Some(false)));
        assert_eq!((t.compared, t.conflicts), (2, 0), "一致不计冲突");
        // 不一致 ×2（两个方向）
        assert!(
            t.observe(Some(true), Some(false)),
            "投影=子代理 权威 → 取 true"
        );
        assert!(
            !t.observe(Some(false), Some(true)),
            "投影=父会话 权威 → 取 false（不得被日志翻成 true）"
        );
        assert_eq!(
            (
                t.compared,
                t.conflicts,
                t.proj_sub_log_parent,
                t.proj_parent_log_sub
            ),
            (4, 2, 1, 1),
            "两个方向各计一条，可观测（这条计数就是告警文案里的数字）"
        );
        // 缺源：不比较、不冲突，按在场的那一侧取值
        assert!(t.observe(Some(true), None), "只有投影 → 取投影");
        assert!(t.observe(None, Some(true)), "只有日志 → 兜底取日志");
        assert!(
            !t.observe(None, None),
            "两源都缺 → 保守按父会话（与 DAO 的 COALESCE(s.is_subagent,0) 同向）"
        );
        assert_eq!(
            (t.compared, t.conflicts),
            (4, 2),
            "缺源不参与'可比较'计数（只有两源同时在场才比）"
        );
    }

    /// **GC 4 / W-15 锁**：投影缓存的文件读取**只经** `CollectContext::read_incremental`，
    /// 且「一遍扫描」不能只看结果对不对——把 `reads` / `bytes_read` 一起钉住：
    /// * 首轮：每文件各进一次读取入口，`bytes_read` = 各文件真实字节数之和；
    /// * 第二轮未变更：仍各进一次入口（L2 快速路），但 **零字节读取**、零新增、parsed_files 归零。
    #[test]
    fn projection_cache_reads_go_through_the_single_entry_point_once_per_round() {
        let _serial = serial();
        let dir = tempfile::tempdir().unwrap();
        projcache(
            dir.path(),
            "s1",
            "/p/A",
            (100, 10, 50, 0),
            1,
            1,
            0,
            SubAgentRow::ParentEmpty,
            None,
        );
        projcache(
            dir.path(),
            "s2",
            "/p/B",
            (200, 20, 60, 0),
            2,
            2,
            0,
            SubAgentRow::ParentEmpty,
            None,
        );
        let sizes: i64 = ["s1", "s2"]
            .iter()
            .map(|id| {
                std::fs::metadata(
                    dir.path()
                        .join(format!("storages/session_projcache/sessions/{id}.json")),
                )
                .unwrap()
                .len() as i64
            })
            .sum();
        let no_cursors = HashMap::new();
        let (d1, s1) = run(dir.path(), &no_cursors);
        assert_eq!(s1.reads, 2, "两个文件各进一次读取入口（GC 4：唯一入口）");
        assert_eq!(s1.bytes_read, sizes as u64, "首读必须真读（不是快速路）");
        assert_eq!(s1.repeat_reads, 0, "一遍扫描纪律");
        assert_eq!(d1.parsed_files, 2);
        assert_eq!(d1.new_records, 2, "两个文件都带来了新数据");

        let cursors = cursors_of(&d1);
        let (d2, s2) = run(dir.path(), &cursors);
        assert_eq!(s2.reads, 2, "未变文件仍各进一次入口（只是走 L2 快速路）");
        assert_eq!(
            s2.bytes_read, 0,
            "未变文件必须零字节读取（W-15：把 bytes_read 也纳入断言）"
        );
        assert_eq!(s2.repeat_reads, 0);
        assert_eq!(
            d2.parsed_files, 0,
            "未变更文件不计入「本轮真读到的文件数」（W-50）"
        );
        assert_eq!(d2.new_records, 0);
        assert!(d2.details.is_empty(), "不得重复计入");
    }

    /// **B4 补强**：日志**增长**时只计新增尾部（不是每轮重算整份），且跨轮状态真的被读回。
    #[test]
    fn log_growth_counts_only_the_new_tail() {
        let _serial = serial();
        let dir = tempfile::tempdir().unwrap();
        let p = write_log(
            dir.path(),
            "proj-a",
            "raw-2",
            &[
                json!({"type":"session","version":3,"id":"raw-2","cwd":"/p/A"}),
                json!({"type":"turn/start","time":1000}),
                json!({"type":"tool/call","time":1100,"data":{"name":"Bash"}}),
                json!({"type":"turn/end","time":2000,"data":{"reason":{"kind":"completed"}}}),
            ],
        );
        let no_cursors = HashMap::new();
        let (d1, s1) = run(dir.path(), &no_cursors);
        let one = |d: &SourceDelta| {
            d.details
                .iter()
                .find(|x| x.key.session_id == "raw-2")
                .map(|x| {
                    (
                        x.counters.tool_calls,
                        x.counters.turn_ms.clone(),
                        x.counters.error_turn,
                    )
                })
        };
        assert_eq!(one(&d1), Some((1, vec![1000], 0)));
        assert_eq!(s1.repeat_reads, 0);
        let c1 = &d1
            .cursors
            .iter()
            .find(|c| c.session_id == "log:raw-2")
            .unwrap()
            .clone();
        assert!(
            c1.state_json.contains("\"tool_calls\":1"),
            "前提：事实水位必须落库：{}",
            c1.state_json
        );

        // 第二轮：日志追加一帧（真机 .zstd 多帧）→ 只计新增的那条工具调用与那一轮时长
        append_log(
            &p,
            &[
                json!({"type":"tool/call","time":3000,"data":{"name":"Read"}}),
                json!({"type":"turn/start","time":3000}),
                json!({"type":"turn/end","time":3500,"data":{"reason":{"kind":"aborted"}}}),
            ],
        );
        let (d2, _) = run(dir.path(), &cursors_of(&d1));
        assert_eq!(
            one(&d2),
            Some((1, vec![500], 0)),
            "新增尾部：工具 1 次 + 本轮 500ms；aborted 计入 interrupted 而不是 error_turn"
        );
        assert_eq!(
            d2.details
                .iter()
                .map(|x| x.counters.interrupted)
                .sum::<i64>(),
            1
        );
        // 第三轮：文件未变 → 零新增（L2 未变代际**不重复解码、也不重复解析**）
        let (d3, s3) = run(dir.path(), &cursors_of(&d2));
        assert!(d3.details.is_empty(), "未变代际不得再产出任何事实");
        assert_eq!(d3.new_records, 0);
        assert_eq!(s3.repeat_reads, 0);
        assert_eq!(
            d3.parsed_files, 0,
            "**裁决 C（fix round 1）**：未变代际的日志必须零解码零解析 —— 摘掉日志侧 L2 \
             （每轮直接 read_best_generation）时它会变成 1（W-15：只断「值对了」没断「路径对了」）"
        );
    }

    /// **D-41 同族（跨轮持久化模型 / 供应商）**：原始日志侧**不产生**模型字段，
    /// 若某轮投影缓存不可得（被清理 / 缺行），不得把该会话的报错/工具事实落进
    /// `model=""` 幽灵行——必须沿用**上一轮已知**的模型与供应商。
    ///
    /// 触发面是真的：投影缓存是**缓存**（可被清理），而原始日志才是权威；
    /// 而本机 63/170 个投影文件**结构性地**没有 `modelSelection` 行（不是「本轮还没读到」）。
    #[test]
    fn log_side_model_survives_a_missing_projection_cache() {
        let _serial = serial();
        let dir = tempfile::tempdir().unwrap();
        projcache(
            dir.path(),
            "s1",
            "/p/A",
            (100, 10, 50, 0),
            1,
            1,
            0,
            SubAgentRow::ParentEmpty,
            Some(("ollama-pro", "deepseek-v4.1-flash")),
        );
        let p = write_log(
            dir.path(),
            "proj-a",
            "s1",
            &[
                json!({"type":"session","version":3,"id":"s1","cwd":"/p/A"}),
                json!({"type":"tool/call","time":1100,"data":{"name":"Bash"}}),
            ],
        );
        let no_cursors = HashMap::new();
        let (d1, _) = run(dir.path(), &no_cursors);
        let rows: Vec<_> = d1
            .details
            .iter()
            .filter(|d| d.key.session_id == "s1")
            .collect();
        // **第 14 轮起同一会话是两行**：会话级事实行（turns/toolMs/工具计数，B 段按日志事件时间）
        // + 用量行（本夹具的日志里**没有** `assistant/message` 事件 ⇒ 投影缓存差额只能补录，
        // 落在**全天账**行上、`cache_semantics = backfill`）。
        // 本用例真正要锁的是：**两行都不得落成空模型幽灵行**（D-41）。
        assert_eq!(
            rows.len(),
            2,
            "事实行 + 补录行：{:?}",
            rows.iter().map(|r| r.key.clone()).collect::<Vec<_>>()
        );
        for r in &rows {
            assert_eq!(r.key.model, "deepseek-v4.1-flash", "两行都要带模型");
            assert_eq!(r.key.provider, "ollama-pro");
            assert_eq!(r.provider_kind, SourceKind::Measured);
        }
        let facts = rows
            .iter()
            .find(|r| !r.cache_semantics.is_backfill())
            .expect("会话级事实行");
        assert_eq!(facts.counters.tool_calls, 1);
        let backfill = rows
            .iter()
            .find(|r| r.cache_semantics.is_backfill())
            .expect("补录行");
        assert_eq!(
            backfill.buckets.input_fresh + backfill.buckets.cache_read + backfill.buckets.output,
            160,
            "补录行带的是投影缓存本轮差额（100 + 50 + 10）"
        );

        // 第二轮：投影缓存被清理（缓存可被清），日志又长了一条工具调用
        std::fs::remove_file(
            dir.path()
                .join("storages/session_projcache/sessions/s1.json"),
        )
        .unwrap();
        append_log(
            &p,
            &[json!({"type":"tool/call","time":3000,"data":{"name":"Read"}})],
        );
        let (d2, _) = run(dir.path(), &cursors_of(&d1));
        let rows2: Vec<_> = d2
            .details
            .iter()
            .filter(|d| d.key.session_id == "s1")
            .collect();
        assert_eq!(
            rows2.len(),
            1,
            "不得落出第二条（空模型）幽灵行：{:?}",
            rows2.iter().map(|r| r.key.clone()).collect::<Vec<_>>()
        );
        assert_eq!(
            rows2[0].key.model, "deepseek-v4.1-flash",
            "投影缓存不可得时必须沿用游标里跨轮持久化的模型（D-41：绝不写 model=\"\" 行）"
        );
        assert_eq!(rows2[0].key.provider, "ollama-pro");
        assert_eq!(rows2[0].counters.tool_calls, 1);
    }

    /// **GC 10（隐私白名单）锁**：原始日志里含 prompt 正文 / 工具参数原文，
    /// 账本里**只允许**结构化数字与静态文本（会话 id / 项目路径 / 模型名）。
    /// 本用例把正文哨兵塞进日志的三个真实位置（`turnOutline` 行、`tool/call.arguments`、
    /// `user/message` 文本），再把整份 `SourceDelta` 序列化，断言**一个字节都没有漏进来**。
    #[test]
    fn prompt_text_never_reaches_the_ledger() {
        let _serial = serial();
        const SECRET: &str = "PROMPT-BODY-SECRET-9f2c";
        let dir = tempfile::tempdir().unwrap();
        projcache(
            dir.path(),
            "s1",
            "/p/A",
            (100, 10, 50, 0),
            1,
            1,
            0,
            SubAgentRow::ParentEmpty,
            None,
        );
        write_log(
            dir.path(),
            "proj-a",
            "s1",
            &[
                json!({"type":"session","version":3,"id":"s1","cwd":"/p/A"}),
                json!({"type":"user/message","time":10,"data":{"text":SECRET}}),
                json!({"type":"tool/call","time":1100,"data":{"name":"Bash","callId":"c1","arguments":SECRET}}),
                json!({"type":"tool/result","time":1200,"data":{"toolCallId":"c1","error":{"name":"X","code":"E1"}}}),
            ],
        );
        let no_cursors = HashMap::new();
        let (d1, _) = run(dir.path(), &no_cursors);
        assert!(
            d1.new_records > 0,
            "前提：本轮真的有产出（否则本用例是空跑）"
        );
        let dump = format!("{:?}", d1.details)
            + &format!("{:?}", d1.sessions)
            + &format!("{:?}", d1.cursors);
        assert!(
            !dump.contains("PROMPT-BODY-SECRET"),
            "prompt 正文 / 工具参数原文**不得**进入任何字段（GC 10）"
        );
        assert!(
            d1.sessions.iter().all(|s| s.title.is_none()),
            "标题也不采（投影缓存的 title/titleInput 一律不读）"
        );
    }

    /// **边界**：dsh 未安装（home 下既无投影缓存也无会话日志）→ 零产物的 `Ok`，
    /// **不是**失败（该源在 `UsageSourceStatus` 上是 `ok=true` + 零文件）。
    #[test]
    fn no_dsh_home_yields_a_zero_product_delta() {
        let _serial = serial();
        let dir = tempfile::tempdir().unwrap();
        let no_cursors = HashMap::new();
        let (d, s) = run(dir.path(), &no_cursors);
        assert!(d.details.is_empty());
        assert!(d.sessions.is_empty());
        assert!(d.cursors.is_empty());
        assert_eq!(d.parsed_files, 0);
        assert_eq!(d.new_records, 0);
        assert_eq!(s.reads, 0, "没有文件 → 一次读取都不该发生");
        assert_eq!(s.bytes_read, 0);
    }

    /// **边界**：原始日志解不开（截断 / 非 zstd / 空文件）→ 该文件不产出指标，
    /// 但**不得冻结整源**：同一轮里其它会话的日志照常入账。
    /// （与 D-24 的取舍一致：一个坏文件不该让其余文件本轮不入账；但也不能静默。）
    #[test]
    fn undecodable_log_is_skipped_without_freezing_the_other_sessions() {
        let _serial = serial();
        let dir = tempfile::tempdir().unwrap();
        let bad_dir = dir.path().join("sessions/proj-a/broken");
        std::fs::create_dir_all(&bad_dir).unwrap();
        std::fs::write(
            bad_dir.join("session.v3.jsonl.zstd"),
            b"not a zstd frame at all",
        )
        .unwrap();
        write_log(
            dir.path(),
            "proj-a",
            "good",
            &[
                json!({"type":"session","version":3,"id":"good","cwd":"/p/A"}),
                json!({"type":"tool/call","time":1100,"data":{"name":"Bash"}}),
            ],
        );
        let no_cursors = HashMap::new();
        let (d, _) = run(dir.path(), &no_cursors);
        assert!(
            d.details.iter().any(|x| x.key.session_id == "good"),
            "坏文件不得冻结整源：好的会话必须照常入账"
        );
        assert!(
            !d.details.iter().any(|x| x.key.session_id == "broken"),
            "解不开的日志不得产出任何指标（按空态，不填 0 —— GC 7）"
        );
        assert_eq!(d.parsed_files, 1, "解不开的文件不算「本轮真读到的文件」");
    }

    /// **W-19 锁**：`SessionFileScan` 的 namespace 必须由 `scan_namespace(sid)` **派生**
    /// （不得手写字符串字面量），两套缓存（投影缓存 / 原始日志）之间也必须各自独立——
    /// 同 ns 混存两种产物类型会让 `Arc::downcast` 失配、缓存静默失效（GC 5 的原始动机）。
    #[test]
    fn scan_namespace_is_derived_from_the_source_id() {
        let _serial = serial();
        assert_eq!(SCAN_NAMESPACE, scan_namespace(UsageSourceId::Dsh));
        assert_eq!(SCAN_NAMESPACE, "usage-dsh");
        assert_eq!(
            log_scan_namespace(),
            format!("{}-log", scan_namespace(UsageSourceId::Dsh)),
            "原始日志侧的 ns 必须与 scan_namespace 的值自洽（W-19 / D-1 先例）"
        );
        assert_ne!(log_scan_namespace(), SCAN_NAMESPACE, "两套缓存不得同 ns");
        assert!(log_scan_namespace().starts_with("usage-"));
        // 不得复用 monitor/ 既有 ns（同 ns 混存会让 downcast 失配）
        assert_ne!(log_scan_namespace(), "dsh-log");
        assert_ne!(SCAN_NAMESPACE, scan_namespace(UsageSourceId::Claude));
    }

    /// **D-24 / D-38 同族**：单文件读失败里只有「**文件已消失**」这一种可以跳过，
    /// 其余（权限 / IO 故障 / mtime 不可得）必须**整源响亮失败** —— 落进
    /// `UsageSourceStatus.errorCode`，否则用户在 UI 上看到的是「这个源没有用量」，
    /// 而不是「这个源读不了」。
    #[cfg(unix)]
    #[test]
    fn unreadable_projection_cache_fails_the_whole_source_loudly() {
        let _serial = serial();
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        projcache(
            dir.path(),
            "s1",
            "/p/A",
            (100, 10, 50, 0),
            1,
            1,
            0,
            SubAgentRow::ParentEmpty,
            None,
        );
        let p = dir
            .path()
            .join("storages/session_projcache/sessions/s1.json");
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o000)).unwrap();
        // 前提断言（防假绿）：权限真的生效了——本进程读它必须失败
        assert!(std::fs::read(&p).is_err(), "前提：0000 权限下必须读不了");
        let no_cursors = HashMap::new();
        let ctx = CollectContext::new(dir.path(), 1_700_000_000_000, &no_cursors);
        let err = DshCollector::collect_with_home(&ctx, dir.path()).unwrap_err();
        assert_eq!(
            err.code, "usage-source-io",
            "读不了必须整源响亮失败：{err:?}"
        );
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).ok();
    }

    /// **继承的盲区（如实登记，不是本任务新缺陷）**：投影缓存走 `read_incremental`，
    /// 而它的快速路 `file_unchanged` 只比 `(mtime_ms, size)` —— **同一毫秒内 + 同一大小**的
    /// 重写会被判「未变」→ 整轮零字节读取、增量漏掉。这是 stream/cursor 层的**已登记盲区**
    ///（`cursor.rs` 模块文档「已知盲区」+ `stream.rs` 的同名锁），本采集器**继承**它。
    ///
    /// 生产暴露面的量级（模块头同款分析）：要漏掉一次重写，必须让「上一轮游标里记录的 mtime」
    /// 与本轮的重写时刻落在**同一毫秒** —— 即采集读该文件的那一刻与前后两次写入全挤进同一毫秒；
    /// 采集轮次间隔是分钟级，概率 ~0。**夹具纪律**：不要在亚毫秒内做「同尺寸重写」，
    /// 否则会随机踩中它（本任务 T16 的第一版夹具就是这么 flake 的）。
    ///
    /// 本用例用 `set_modified` **确定性地**复现该盲区，把它写死（免得将来有人以为它是新 bug，
    /// 也免得有人用「夹具恰好不触发」当成它不存在）。要彻底关掉它只能改**冻结的**增量读契约，
    /// 或给投影缓存另起一套「每轮必读」的入口（后者违反 GC 4 的唯一入口）。
    #[test]
    fn same_millisecond_same_size_rewrite_is_the_inherited_fast_path_blind_spot() {
        let _serial = serial();
        let dir = tempfile::tempdir().unwrap();
        projcache_full(
            dir.path(),
            "s1",
            "/p/A",
            1,
            (100, 10, 50, 0),
            1,
            1,
            0,
            SubAgentRow::ParentEmpty,
            None,
        );
        let path = dir
            .path()
            .join("storages/session_projcache/sessions/s1.json");
        let first_len = std::fs::metadata(&path).unwrap().len();
        let fixed =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_millis(1_700_000_000_000);
        let set_mtime = |p: &std::path::Path, t: std::time::SystemTime| {
            std::fs::File::options()
                .write(true)
                .open(p)
                .unwrap()
                .set_modified(t)
                .unwrap();
        };
        set_mtime(&path, fixed);
        let no_cursors = HashMap::new();
        let (d1, _) = run(dir.path(), &no_cursors);
        assert_eq!(
            d1.details.len(),
            2,
            "前提：首轮必须产出明细行（第 14 轮起 = 会话级事实行 + 无日志可归属的补录行）"
        );

        // 同尺寸重写（数字位数完全相同）+ **同一毫秒** mtime
        projcache_full(
            dir.path(),
            "s1",
            "/p/A",
            1,
            (150, 25, 60, 0),
            2,
            3,
            0,
            SubAgentRow::ParentEmpty,
            None,
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            first_len,
            "前提：两次写入必须**同尺寸**（否则踩不到这条快速路）"
        );
        set_mtime(&path, fixed);
        let (d2, s2) = run(dir.path(), &cursors_of(&d1));
        assert_eq!(
            s2.bytes_read, 0,
            "前提：命中 (mtime,size) 快速路（零字节读取）"
        );
        assert!(
            d2.details.is_empty(),
            "继承的盲区：同毫秒 + 同尺寸的重写会被判「未变」→ 本轮不计增量（已登记，非新缺陷）"
        );
    }

    /// **裁决 A（fix round 1，`plan-mandated`）**：dsh 的**逐工具**维度必须落地 ——
    /// spec §5.1 dsh 行把「逐工具耗时」与报错次数、最长单 turn **并列**为「投影缓存拿不到、
    /// 必须读原始日志」，§8.6 记它属**部分覆盖**（`available=true + reason=部分覆盖`）。
    /// 配对键是 `tool/call.data.callId` ↔ **`tool/result.data.message.source.callId`**
    ///（真机 173 文件全量 14,584/14,584 都是这一层；**不是同一个字段名、也不是同一层**；
    /// 旧样本 `data.message.toolCallId` 与任务书写的顶层 `data.toolCallId` 只是兜底），
    /// 耗时 = `tool/result.time − tool/call.time`；两者就在**同一次**扫描里（GC 4：绝不二次读取）。
    ///
    /// 两条不变量：① `Σ tool_stats.0 == tool_calls`（兄弟源同款，也防「0 次调用却有耗时」）；
    /// ② **绝不只填次数**（那会让该工具的 ms 变成假 0，GC 7）；配不上回执的只计次数、不计耗时。
    #[test]
    fn tool_stats_carries_counts_and_durations_and_sums_to_tool_calls() {
        let _serial = serial();
        const T0: i64 = 1_680_000_000_000;
        let dir = tempfile::tempdir().unwrap();
        write_log(
            dir.path(),
            "proj-a",
            "raw-3",
            &[
                json!({"type":"session","version":3,"id":"raw-3","cwd":"/p/A"}),
                json!({"type":"tool/call","time":T0,"data":{"name":"Bash","callId":"c1","arguments":"ls"}}),
                json!({"type":"tool/call","time":T0 + 1_000,"data":{"name":"Read","callId":"c2"}}),
                json!({"type":"tool/call","time":T0 + 3_000,"data":{"name":"Edit","callId":"c3"}}),
                json!({"type":"tool/call","time":T0 + 4_000,"data":{"name":"Write","callId":"c4"}}),
                // **真机主形态**（173 文件全量 14,584/14,584）：键在
                // `data.message.source.callId`，**不在** data 顶层 ——
                // 任务书正文写的顶层 `data.toolCallId` 真机**一个都没有**（D-29/D-36 同族）
                json!({"type":"tool/result","time":T0 + 500,
                       "data":{"message":{"role":"user","source":{"kind":"tool","callId":"c1"},"content":[]}}}),
                // 旧样本形态（探测文档里的 v3-era，真机 72 条）：`data.message.toolCallId`
                json!({"type":"tool/result","time":T0 + 1_250,
                       "data":{"message":{"role":"tool","toolCallId":"c2"}}}),
                // 任务书正文的扁平形态（兜底）：`data.toolCallId`
                json!({"type":"tool/result","time":T0 + 3_100,"data":{"toolCallId":"c3"}}),
                // 第四次调用**没有回执**：次数照记、耗时不计（不写假 0）
            ],
        );
        let no_cursors = HashMap::new();
        let (d1, _) = run(dir.path(), &no_cursors);
        let row = d1
            .details
            .iter()
            .find(|x| x.key.session_id == "raw-3")
            .expect("原始日志必须产出逐工具指标");
        assert_eq!(row.counters.tool_calls, 4);
        assert_eq!(
            row.counters.tool_stats.get("Bash").copied(),
            Some((1, 500)),
            "真机主形态 `data.message.source.callId` 必须配上（顶层 `data.toolCallId` 一个都没有）"
        );
        assert_eq!(
            row.counters.tool_stats.get("Read").copied(),
            Some((1, 250)),
            "旧样本形态 `data.message.toolCallId` 兜底"
        );
        assert_eq!(
            row.counters.tool_stats.get("Edit").copied(),
            Some((1, 100)),
            "任务书扁平形态 `data.toolCallId` 兜底"
        );
        assert_eq!(
            row.counters.tool_stats.get("Write").copied(),
            Some((1, 0)),
            "配不上回执的调用：**次数照记、耗时不计**（宁可少一个耗时，不写假 0）"
        );
        assert_eq!(
            row.counters
                .tool_stats
                .values()
                .map(|(n, _)| *n)
                .sum::<i64>(),
            row.counters.tool_calls,
            "Σ tool_stats.0 必须等于 tool_calls（兄弟源同款不变量）"
        );
        // 前提断言（防假绿）：耗时真的配对算出来了（不是恒 0 的退化实现）
        assert_eq!(
            row.counters
                .tool_stats
                .values()
                .map(|(_, ms)| *ms)
                .sum::<i64>(),
            850,
            "500 + 250 + 100（三种回执形态各配一条；无回执的那条不计）"
        );
        assert_eq!(row.counters.tool_stats.len(), 4, "四个工具各一条");
    }

    /// **fix round 2 裁决 A（行为性：静默永久丢数）**：迟到回执的逐工具耗时**必须**落行。
    ///
    /// 形态（真机必然发生：单工具最长 3.1 h）：第 N 轮只有 `tool/call`（无回执）→ 记 `(1,0)`
    /// 并推进水位；第 N+1 轮**只有它的回执**到达（成功工具 → 无 `error_tool`、turn 未结束、
    /// 无新调用）⇒ `new_total == 0` —— 旧闸门 `if new_total > 0` 会**不落行**，而水位已把这段
    /// ms 记走 → 第 N+2 轮 `dm` 已是 0 ⇒ **该次耗时永久丢失**。
    ///
    /// 锁：三轮夹具，断言该工具的耗时**恰好出现一次**（少了 = 丢；多了 = 重），
    /// 且三轮的 ms 之和恒等于真实耗时。
    #[test]
    fn late_result_duration_survives_the_new_total_gate() {
        let _serial = serial();
        const T0: i64 = 1_680_000_000_000;
        let dir = tempfile::tempdir().unwrap();
        let p = write_log(
            dir.path(),
            "proj-a",
            "raw-6",
            &[
                json!({"type":"session","version":3,"id":"raw-6","cwd":"/p/A"}),
                json!({"type":"tool/call","time":T0,"data":{"name":"Bash","callId":"c1"}}),
            ],
        );
        let no_cursors = HashMap::new();
        let (d1, _) = run(dir.path(), &no_cursors);
        let bash = |d: &SourceDelta| {
            d.details
                .iter()
                .find_map(|x| x.counters.tool_stats.get("Bash").copied())
        };
        assert_eq!(bash(&d1), Some((1, 0)), "第 N 轮：只有调用、没有回执");
        assert_eq!(
            d1.details.len(),
            1,
            "前提：这一轮有事实（tool_calls）→ 落行"
        );

        // 第 N+1 轮：回执到达，**它是本轮唯一新增事实**（new_total == 0）
        append_log(
            &p,
            &[json!({"type":"tool/result","time":T0 + 500,
                     "data":{"message":{"role":"user","source":{"kind":"tool","callId":"c1"},"content":[]}}})],
        );
        let (d2, _) = run(dir.path(), &cursors_of(&d1));
        assert_eq!(
            d2.details.len(),
            1,
            "迟到回执必须落行（旧闸门只看 new_total 时这里会是 0 行 → ms 永久丢失）"
        );
        assert_eq!(
            bash(&d2),
            Some((0, 500)),
            "这一行只带耗时（次数差值 0），耗时 500ms"
        );
        assert_eq!(
            d2.new_records, 0,
            "`new_records` 仍按**事实数**：本轮没有新事实"
        );

        // 第 N+2 轮：turn/end（新的报错/时长事实）→ **不得**把那段 ms 再算一遍
        append_log(
            &p,
            &[
                json!({"type":"turn/start","time":T0 + 600}),
                json!({"type":"turn/end","time":T0 + 1_100,"data":{"reason":{"kind":"completed"}}}),
            ],
        );
        let (d3, _) = run(dir.path(), &cursors_of(&d2));
        assert_eq!(
            bash(&d3),
            None,
            "耗时已经在上一条 delta 里给过 → 这一轮不得再给（多了 = 重复计）"
        );
        // 「恰好一次」的等价表述：三轮里给出 ms 的轮数 == 1，且总量 == 500
        let with_ms = [&d1, &d2, &d3]
            .iter()
            .filter(|d| {
                d.details
                    .iter()
                    .any(|x| x.counters.tool_stats.values().any(|(_, ms)| *ms != 0))
            })
            .count();
        let total_ms: i64 = [&d1, &d2, &d3]
            .iter()
            .flat_map(|d| d.details.iter())
            .map(|x| {
                x.counters
                    .tool_stats
                    .values()
                    .map(|(_, ms)| *ms)
                    .sum::<i64>()
            })
            .sum();
        assert_eq!(
            with_ms, 1,
            "该工具的耗时必须**恰好出现一次**（少了=丢、多了=重）"
        );
        assert_eq!(total_ms, 500, "三轮合计恒等于真实耗时");
    }

    // ══════════════════════════════════════════════════════════════════════════════
    // 第 14 轮：dsh「时间归属」改造的必交用例（口径见任务书 §3 / §六）
    // ══════════════════════════════════════════════════════════════════════════════

    /// 本地整点毫秒。**夹具的时刻必须用本地时钟构造**（与 `hour_key_of_host` 同一时钟），
    /// 否则断言会在别的时区下漂成另一天/另一小时。
    fn local_ms(y: i32, mo: u32, d: u32, h: u32) -> i64 {
        use chrono::TimeZone;
        chrono::Local
            .with_ymd_and_hms(y, mo, d, h, 0, 0)
            .single()
            .expect("测试时刻必须存在（避开 DST 跳变）")
            .timestamp_millis()
    }

    /// 取某会话**某个小时桶**的实测行（逐事件归属的断言用它；补录行不参与）。
    fn hour_row<'a>(d: &'a SourceDelta, sid: &str, hour: &str) -> &'a DetailDelta {
        d.details
            .iter()
            .find(|x| {
                x.key.session_id == sid
                    && x.key.hour_key == hour
                    && !x.cache_semantics.is_backfill()
            })
            .unwrap_or_else(|| {
                panic!(
                    "缺 {sid} 的 {hour} 实测行；现有：{:?}",
                    d.details
                        .iter()
                        .map(|x| (
                            x.key.session_id.clone(),
                            x.key.hour_key.clone(),
                            x.cache_semantics
                        ))
                        .collect::<Vec<_>>()
                )
            })
    }

    /// 本轮所有明细行的四桶之和
    fn total_buckets(d: &SourceDelta) -> i64 {
        d.details
            .iter()
            .map(|x| {
                x.buckets.input_fresh
                    + x.buckets.cache_read
                    + x.buckets.cache_write
                    + x.buckets.output
            })
            .sum()
    }

    /// 用一份日志正文造 `LogDigest`（走真实解析链：header + 逐事件抽取）
    fn digest_of(text: &str, version: i64) -> LogDigest {
        let read = crate::monitor::dsh::log::GenerationRead {
            version,
            text: text.to_string(),
            torn_frames: 0,
            mtime_ms: 1,
        };
        LogDigest::of(&read).expect("夹具必须可解析")
    }

    /// 一条**真机形态**的用量事件（`data.usage`，四桶之和 = totalTokens）
    fn usage_event(time_ms: i64, fresh: i64, cache_read: i64, output: i64) -> serde_json::Value {
        json!({"type":"assistant/message","time":time_ms,
               "data":{"usage":{"inputTokens":fresh,"outputTokens":output,
                                "cacheReadTokens":cache_read,
                                "totalTokens":fresh+output+cache_read}}})
    }

    /// 取 `log:<session>` 游标
    fn log_cursor<'a>(d: &'a SourceDelta, sid: &str) -> &'a CursorDelta {
        let key = format!("log:{sid}");
        d.cursors
            .iter()
            .find(|c| c.session_id == key)
            .unwrap_or_else(|| panic!("缺游标 {key}"))
    }

    /// **必交 #1：逐事件归属**。造一条**跨小时**的事件流：每个 token 必须落在**它自己的小时**，
    /// 而不是采集当轮那一小时。
    ///
    /// **这条在旧实现上必红**：旧实现把投影缓存的整份增量按 `ctx.now_ms` 归桶 ⇒ 三行都会
    /// 落在 `hour_key_of_host(now)`（甚至只有一行）。
    #[test]
    fn cross_hour_events_land_in_their_own_hours() {
        let _serial = serial();
        let now = local_ms(2026, 10, 7, 18);
        let t_a = local_ms(2026, 10, 6, 18);
        let t_b = local_ms(2026, 10, 7, 2);
        assert_ne!(
            hour_key_of_host(t_a),
            hour_key_of_host(t_b),
            "前提：两个事件必须分处不同小时"
        );
        assert_ne!(
            hour_key_of_host(t_a),
            hour_key_of_host(now),
            "前提：事件小时 ≠ 采集小时"
        );
        let dir = tempfile::tempdir().unwrap();
        // 事件四桶之和**逐字段等于**投影缓存 totals（真机 319/319 如此）⇒ D == E，
        // 不会产出补录行，断言只盯「逐事件归属」这一件事。
        write_log(
            dir.path(),
            "proj-a",
            "s1",
            &[
                json!({"type":"session","version":3,"id":"s1","cwd":"/p/A"}),
                usage_event(t_a + 1_000, 10, 100, 1),
                usage_event(t_a + 2_000, 20, 200, 2),
                usage_event(t_b + 1_000, 30, 300, 3),
            ],
        );
        projcache(
            dir.path(),
            "s1",
            "/p/A",
            (60, 6, 600, 0),
            1,
            3,
            0,
            SubAgentRow::ParentEmpty,
            Some(("ollama-pro", "deepseek-v4.1-flash")),
        );
        let no_cursors = HashMap::new();
        let ctx = CollectContext::new(dir.path(), now, &no_cursors);
        let d = DshCollector::collect_with_home(&ctx, dir.path()).unwrap();

        let a = hour_row(&d, "s1", &hour_key_of_host(t_a));
        assert_eq!(a.buckets.input_fresh, 30, "18 点那两条：10 + 20");
        assert_eq!(a.buckets.cache_read, 300, "100 + 200");
        assert_eq!(a.buckets.output, 3, "1 + 2");
        assert_eq!(a.buckets.cache_write, 0, "dsh 不报 cacheWriteTokens");
        assert_eq!(a.requests, 2, "请求数 = 逐事件条数");
        let b = hour_row(&d, "s1", &hour_key_of_host(t_b));
        assert_eq!(b.buckets.input_fresh, 30);
        assert_eq!(b.buckets.cache_read, 300);
        assert_eq!(b.requests, 1);
        assert_ne!(
            a.key.hour_key, b.key.hour_key,
            "两个事件必须落在**两个**小时桶里（旧实现只有一个桶）"
        );
        // 采集当轮那一小时**不得**有任何行
        assert!(
            !d.details
                .iter()
                .any(|x| x.key.hour_key == hour_key_of_host(now)),
            "采集当轮不得出现任何行（旧实现把整份增量记在采集时刻 = 时间平移）"
        );
        // D == E ⇒ 没有补录
        assert!(
            d.details.iter().all(|x| !x.cache_semantics.is_backfill()),
            "D == E 时不得产出补录行"
        );
        assert_eq!(total_buckets(&d), 666, "10+100+1 + 20+200+2 + 30+300+3");
    }

    /// **必交 #4：bootstrap**（首次见到的会话，`last_cumulative` 为空）。
    /// 它的历史**不得**落进采集当轮那一小时 —— 必须落回它自己事件的小时。
    #[test]
    fn bootstrap_history_never_lands_in_the_collection_hour() {
        let _serial = serial();
        let now = local_ms(2026, 10, 7, 18);
        let old = local_ms(2026, 10, 4, 9); // 三天前
        let dir = tempfile::tempdir().unwrap();
        write_log(
            dir.path(),
            "proj-a",
            "s1",
            &[
                json!({"type":"session","version":3,"id":"s1","cwd":"/p/A"}),
                usage_event(old + 1_000, 11, 111, 1),
            ],
        );
        projcache(
            dir.path(),
            "s1",
            "/p/A",
            (11, 1, 111, 0),
            1,
            1,
            0,
            SubAgentRow::ParentEmpty,
            None,
        );
        let no_cursors = HashMap::new();
        assert!(
            no_cursors.is_empty(),
            "前提：首轮没有任何游标（= 没有已入账累计）"
        );
        let ctx = CollectContext::new(dir.path(), now, &no_cursors);
        let d = DshCollector::collect_with_home(&ctx, dir.path()).unwrap();
        let row = hour_row(&d, "s1", &hour_key_of_host(old));
        assert_eq!(row.buckets.input_fresh, 11);
        assert_eq!(row.buckets.cache_read, 111);
        assert!(
            !d.details
                .iter()
                .any(|x| x.key.hour_key == hour_key_of_host(now)),
            "bootstrap 的历史不得落进采集当轮"
        );
        // 首轮水位要**武装好**：否则下一轮会把这批事件再计一遍
        let c = log_cursor(&d, "s1");
        assert!(c.byte_offset > 0, "首轮结束必须留下字节水位");
        assert_eq!(c.ordinal, 1, "已消费事件条数");
        assert_eq!(c.last_cumulative, Some(123), "已消费事件四桶累计");
    }

    /// **水位按字节推进**：第二轮（日志未变）零新增；第三轮只计**追加的那一条**，
    /// 且落在它自己的小时。这条锁住「不按时间戳推进、不重复入账」。
    #[test]
    fn second_round_adds_nothing_and_the_next_round_counts_only_the_new_tail() {
        let _serial = serial();
        let now = local_ms(2026, 10, 7, 18);
        let t1 = local_ms(2026, 10, 7, 10);
        let t2 = local_ms(2026, 10, 7, 11);
        let dir = tempfile::tempdir().unwrap();
        let p = write_log(
            dir.path(),
            "proj-a",
            "s1",
            &[
                json!({"type":"session","version":3,"id":"s1","cwd":"/p/A"}),
                usage_event(t1, 10, 100, 1),
            ],
        );
        projcache(
            dir.path(),
            "s1",
            "/p/A",
            (10, 1, 100, 0),
            1,
            1,
            0,
            SubAgentRow::ParentEmpty,
            None,
        );
        let no_cursors = HashMap::new();
        let ctx = CollectContext::new(dir.path(), now, &no_cursors);
        let d1 = DshCollector::collect_with_home(&ctx, dir.path()).unwrap();
        assert_eq!(total_buckets(&d1), 111);

        // 第二轮：日志与投影缓存都没变 → 零新增行
        // 借用不能挂在临时值上（E0716）：先绑变量
        let cursors1 = cursors_of(&d1);
        let ctx2 = CollectContext::new(dir.path(), now + 60_000, &cursors1);
        let d2 = DshCollector::collect_with_home(&ctx2, dir.path()).unwrap();
        assert!(d2.details.is_empty(), "日志未变不得再产出任何行");

        // 第三轮：追加一条事件 + 投影缓存跟上 → 只计这一条，落在**它自己的**小时
        append_log(&p, &[usage_event(t2, 20, 200, 2)]);
        projcache(
            dir.path(),
            "s1",
            "/p/A",
            (30, 3, 300, 0),
            1,
            2,
            0,
            SubAgentRow::ParentEmpty,
            None,
        );
        let cursors2 = cursors_of(&d2);
        let ctx3 = CollectContext::new(dir.path(), now + 120_000, &cursors2);
        let d3 = DshCollector::collect_with_home(&ctx3, dir.path()).unwrap();
        assert_eq!(total_buckets(&d3), 222, "只计新增的那一条");
        let row = hour_row(&d3, "s1", &hour_key_of_host(t2));
        assert_eq!(row.buckets.cache_read, 200);
        assert_eq!(row.requests, 1);
        assert_eq!(
            log_cursor(&d3, "s1").ordinal,
            2,
            "水位必须推进到两条事件之后"
        );
    }

    /// **必交 #2：不双计**（`D − E ≤ 0` 分支）。日志覆盖得比投影缓存差额还多时，
    /// **什么都不记**（既不重复入账，也不产出补录行）。
    #[test]
    fn no_double_counting_when_the_log_already_covers_the_delta() {
        let _serial = serial();
        let now = local_ms(2026, 10, 7, 18);
        let t1 = local_ms(2026, 10, 7, 10);
        let t2 = local_ms(2026, 10, 7, 11);
        let dir = tempfile::tempdir().unwrap();
        let p = write_log(
            dir.path(),
            "proj-a",
            "s1",
            &[
                json!({"type":"session","version":3,"id":"s1","cwd":"/p/A"}),
                usage_event(t1, 10, 100, 1),
            ],
        );
        projcache(
            dir.path(),
            "s1",
            "/p/A",
            (10, 1, 100, 0),
            1,
            1,
            0,
            SubAgentRow::ParentEmpty,
            None,
        );
        let no_cursors = HashMap::new();
        let ctx = CollectContext::new(dir.path(), now, &no_cursors);
        let d1 = DshCollector::collect_with_home(&ctx, dir.path()).unwrap();

        // 日志追加两条（+222），而投影缓存**只涨了 100**（D = 100 < E = 222）：
        // 「投影缓存跑在日志后面」是真实形态（缓存是快照，不是权威时间源）。
        append_log(
            &p,
            &[
                usage_event(t2, 20, 200, 2),
                usage_event(t2 + 1_000, 5, 50, 0),
            ],
        );
        projcache(
            dir.path(),
            "s1",
            "/p/A",
            (15, 1, 200, 0),
            1,
            3,
            0,
            SubAgentRow::ParentEmpty,
            None,
        );
        // 借用不能挂在临时值上（E0716）：先绑变量
        let cursors1 = cursors_of(&d1);
        let ctx2 = CollectContext::new(dir.path(), now + 60_000, &cursors1);
        let d2 = DshCollector::collect_with_home(&ctx2, dir.path()).unwrap();
        assert_eq!(
            total_buckets(&d2),
            277,
            "只记事件侧 E（20+200+2 + 5+50+0），**不得**再把 D 加上去"
        );
        assert!(
            d2.details.iter().all(|x| !x.cache_semantics.is_backfill()),
            "D − E ≤ 0 ⇒ 绝不产出补录行（日志已覆盖，重复入账是硬红线）"
        );
    }

    /// **日志领先投影缓存一轮 ⇒ 绝不重复入账**（真实形态：投影缓存是**懒刷新的快照**，
    /// 日志先追加、缓存文件晚一步重写）。
    ///
    /// 这一轮里 A 段走 `(mtime,size)` 快速路、**根本不产出差额 D**；下一轮缓存追上时，
    /// 「本轮 D − 本轮 E」会把上一轮已经实测过的量再算一遍补录 —— 所以补录的目标必须按
    /// **已记录水位 R**（实测 + 历次补录的累计）算（见 `emit_backfill` 文档）。
    #[test]
    fn log_leading_the_projection_cache_is_never_counted_twice() {
        let _serial = serial();
        let now = local_ms(2026, 10, 7, 18);
        let t1 = local_ms(2026, 10, 7, 9);
        let t2 = local_ms(2026, 10, 7, 10);
        let dir = tempfile::tempdir().unwrap();
        let p = write_log(
            dir.path(),
            "proj-a",
            "s1",
            &[
                json!({"type":"session","version":3,"id":"s1","cwd":"/p/A"}),
                usage_event(t1, 10, 100, 1),
            ],
        );
        projcache(
            dir.path(),
            "s1",
            "/p/A",
            (10, 1, 100, 0),
            1,
            1,
            0,
            SubAgentRow::ParentEmpty,
            None,
        );
        let no_cursors = HashMap::new();
        let ctx = CollectContext::new(dir.path(), now, &no_cursors);
        let d1 = DshCollector::collect_with_home(&ctx, dir.path()).unwrap();
        assert_eq!(total_buckets(&d1), 111);
        assert!(d1.details.iter().all(|x| !x.cache_semantics.is_backfill()));

        // 第二轮：**日志先追加**（+222），投影缓存文件**原样没动**（A 段零读取、无差额）
        append_log(&p, &[usage_event(t2, 20, 200, 2)]);
        let cursors1 = cursors_of(&d1);
        let ctx2 = CollectContext::new(dir.path(), now + 60_000, &cursors1);
        let d2 = DshCollector::collect_with_home(&ctx2, dir.path()).unwrap();
        assert_eq!(total_buckets(&d2), 222, "这一轮只有事件侧的量");
        assert!(
            d2.details.iter().all(|x| !x.cache_semantics.is_backfill()),
            "投影缓存没重写 ⇒ 没有差额可补录"
        );

        // 第三轮：投影缓存**追上了**（当前累计 = 111 + 222 = 333），日志未变
        projcache(
            dir.path(),
            "s1",
            "/p/A",
            (30, 3, 300, 0),
            1,
            2,
            0,
            SubAgentRow::ParentEmpty,
            None,
        );
        let cursors2 = cursors_of(&d2);
        let ctx3 = CollectContext::new(dir.path(), now + 120_000, &cursors2);
        let d3 = DshCollector::collect_with_home(&ctx3, dir.path()).unwrap();
        assert_eq!(
            total_buckets(&d3),
            0,
            "追赶不能变成补录（那会把上一轮实测过的 222 再记一遍）"
        );
        assert!(
            d3.details.iter().all(|x| !x.cache_semantics.is_backfill()),
            "已记录水位 R 已经把上一轮的 222 记进去了 ⇒ 差额为 0"
        );
        // 三轮累计 == 投影缓存当前累计（总量护栏：一笔不多、一笔不少）
        assert_eq!(
            total_buckets(&d1) + total_buckets(&d2) + total_buckets(&d3),
            333
        );
    }

    /// **必交 #5：总量护栏**（采集侧的可伪证形态）。日志只覆盖投影缓存差额的一部分时：
    /// 实测行 + 补录行 == 投影缓存本轮差额 D —— 一笔不多、一笔不少。
    #[test]
    fn hour_rows_plus_backfill_equal_the_projection_cache_delta() {
        let _serial = serial();
        let now = local_ms(2026, 10, 7, 18);
        let t1 = local_ms(2026, 10, 7, 9);
        let dir = tempfile::tempdir().unwrap();
        // 日志只有一条事件（合计 60），投影缓存累计却是 160 ⇒ 差额 100 必须走补录
        write_log(
            dir.path(),
            "proj-a",
            "s1",
            &[
                json!({"type":"session","version":3,"id":"s1","cwd":"/p/A"}),
                usage_event(t1, 10, 50, 0),
            ],
        );
        projcache(
            dir.path(),
            "s1",
            "/p/A",
            (100, 10, 50, 0),
            1,
            1,
            0,
            SubAgentRow::ParentEmpty,
            None,
        );
        let no_cursors = HashMap::new();
        let ctx = CollectContext::new(dir.path(), now, &no_cursors);
        let d = DshCollector::collect_with_home(&ctx, dir.path()).unwrap();
        let measured: i64 = d
            .details
            .iter()
            .filter(|x| !x.cache_semantics.is_backfill())
            .map(|x| {
                x.buckets.input_fresh
                    + x.buckets.cache_read
                    + x.buckets.cache_write
                    + x.buckets.output
            })
            .sum();
        let backfill: i64 = d
            .details
            .iter()
            .filter(|x| x.cache_semantics.is_backfill())
            .map(|x| {
                x.buckets.input_fresh
                    + x.buckets.cache_read
                    + x.buckets.cache_write
                    + x.buckets.output
            })
            .sum();
        assert_eq!(measured, 60, "实测 = 事件四桶和");
        assert_eq!(backfill, 100, "补录 = D − E = 160 − 60");
        assert_eq!(measured + backfill, 160, "总量必须恰好等于投影缓存差额");
    }

    /// **必交 #3：补录只进全天账**。补录行必须在**结构上**进不了任何短窗口：
    /// 小时档的键集合（用**真实**的 `resolve_range` 算出来）里没有它，而日档键集合里有它。
    ///
    /// 这条不是「约定」而是三条结构事实（见 `emit_backfill` 文档）：小时档取数还有一道
    /// `keys.contains(&hour_key)` 的精确成员判定；计数查询按 `BETWEEN` 比较且日档界是
    /// `"{day}T00".."{day}T23"`，而 10 字符的日键字典序小于当日任何小时键。
    #[test]
    fn backfill_rows_are_day_only_and_never_land_in_an_hour_window() {
        let _serial = serial();
        use crate::services::usage::model::{UsageRange, UsageRangePreset};
        use crate::services::usage::range::resolve_range;
        let now = local_ms(2026, 10, 7, 18);
        let dir = tempfile::tempdir().unwrap();
        // 没有原始日志（日志被清理）⇒ 整笔投影缓存差额只能补录
        projcache(
            dir.path(),
            "s1",
            "/p/A",
            (100, 10, 50, 0),
            1,
            1,
            0,
            SubAgentRow::ParentEmpty,
            Some(("ollama-pro", "deepseek-v4.1-flash")),
        );
        let no_cursors = HashMap::new();
        let ctx = CollectContext::new(dir.path(), now, &no_cursors);
        let d = DshCollector::collect_with_home(&ctx, dir.path()).unwrap();
        let bf = row_of(&d, "s1", true);
        // ① 结构：hour_key 就是**日键**（10 字符），不是任何小时键
        assert_eq!(bf.key.hour_key, day_key_of_host(now));
        assert_eq!(bf.key.hour_key.chars().count(), 10);
        assert_eq!(bf.key.hour_key, bf.key.day_key, "两档键必须同为日键");
        assert_eq!(bf.cache_semantics, CacheSemantics::Backfill, "来源可辨");
        assert!(bf.cache_semantics.is_backfill());
        // ② 取数面：真实窗口键集合（不是手写字符串）
        let r = |p: UsageRangePreset| UsageRange {
            preset: p,
            from: None,
            to: None,
        };
        let last5h = resolve_range(&r(UsageRangePreset::Last5h), now).unwrap();
        let today = resolve_range(&r(UsageRangePreset::Today), now).unwrap();
        assert!(
            !last5h.cur.keys.contains(&bf.key.hour_key),
            "补录行不得进近 5 小时窗口：{:?}",
            last5h.cur.keys
        );
        assert!(
            !today.cur.keys.contains(&bf.key.hour_key),
            "补录行不得进今天（小时档）窗口：{:?}",
            today.cur.keys
        );
        // ③ 日档键集合里**必须**有它（全天账要收这笔）
        let last7d = resolve_range(&r(UsageRangePreset::Last7d), now).unwrap();
        assert!(
            last7d.cur.keys.contains(&bf.key.day_key),
            "补录必须进全天账：{:?}",
            last7d.cur.keys
        );
    }

    /// **水位定界的纯判据**（不碰盘）：首轮从 0 起；有已入账累计时跳过前缀（含跨界那条，
    /// 宁可少算一条事件的零头 —— 那点零头由 `D − E` 兜住）；日志不覆盖累计时全部跳过；
    /// 水位落在非事件边界 / 代际变化 → 重新定界。
    #[test]
    fn event_tail_plan_skips_the_already_accounted_prefix() {
        let _serial = serial();
        let text = [
            r#"{"type":"session","version":4,"id":"s1"}"#.to_string(),
            serde_json::to_string(&usage_event(10, 10, 0, 0)).unwrap(),
            serde_json::to_string(&usage_event(20, 20, 0, 0)).unwrap(),
            serde_json::to_string(&usage_event(30, 30, 0, 0)).unwrap(),
        ]
        .join("\n")
            + "\n";
        let digest = digest_of(&text, 4);
        assert_eq!(digest.usage_events.len(), 3, "前提：三条用量事件");
        // 水位三元组：永远落在**最后一条已消费事件的行尾**
        assert_eq!(
            event_watermark_of(&digest.usage_events),
            (digest.usage_events[2].text_end as i64, 3, 10 + 20 + 30)
        );
        assert_eq!(
            event_watermark_of(&[]),
            (0, 0, 0),
            "空日志的水位是 0（不是假值）"
        );
        // 首轮（没有已入账累计）→ 从 0 起，整份日志都算新增
        let p = event_tail_plan(&digest, None, 0, None);
        assert_eq!(p.start, 0);
        assert!(p.rearmed);
        // 已入账 25 → 前两条（10 + 20 = 30 ≥ 25）都算已入账（含跨界那条）
        let p = event_tail_plan(&digest, None, 0, Some(25));
        assert_eq!(p.start, 2);
        // 已入账 1000（日志被清理 / 投影缓存跑在前面）→ 全部跳过，差额走补录
        let p = event_tail_plan(&digest, None, 0, Some(1000));
        assert_eq!(p.start, 3);
        assert!(p.rearmed);
        // 水位可用（同代际 + 落在事件边界 + ordinal 自洽）→ 续读
        let mut cur = CursorDelta {
            byte_offset: digest.usage_events[1].text_end as i64,
            ordinal: 2,
            // 有效水位**必须**带本实现写下的已记录水位 R（`last_cumulative`）
            last_cumulative: Some(30),
            ..Default::default()
        };
        let p = event_tail_plan(&digest, Some(&cur), 4, Some(1000));
        assert_eq!((p.start, p.rearmed), (2, false));
        // **老版本游标（第 14 轮之前）：byte_offset/ordinal 一律 0、且没有 R（`last_cumulative`）
        // ⇒ 必须当成「没有可用水位」重新定界。** 当成「停在文件开头」会把整份日志重记一遍
        //（dev 应用热重载真发生过：账本被灌到 4×）。
        let legacy = CursorDelta {
            byte_offset: 0,
            ordinal: 0,
            last_cumulative: None,
            ..Default::default()
        };
        let p = event_tail_plan(&digest, Some(&legacy), 4, Some(25));
        assert!(p.rearmed, "老游标不得被当成有效水位");
        assert_eq!(
            p.start, 2,
            "重新定界必须按**已入账累计**（25）跳过前缀，而不是从 0 起"
        );
        // 代际变了 → 偏移空间换了 → 重新定界
        assert!(event_tail_plan(&digest, Some(&cur), 3, Some(1000)).rearmed);
        // 水位落在**半行**上（不是任何事件的 text_end）→ 重新定界
        cur.byte_offset -= 1;
        assert!(event_tail_plan(&digest, Some(&cur), 4, Some(1000)).rearmed);
        // ordinal 越过事件总数 → 重新定界
        cur.byte_offset = digest.usage_events[1].text_end as i64;
        cur.ordinal = 99;
        assert!(event_tail_plan(&digest, Some(&cur), 4, Some(1000)).rearmed);
    }

    /// **补录四桶的纯分配**：四桶之和恰好等于给定差额（逐桶相减 + clamp 会放大差额），
    /// 构成沿用调用方给的投影缓存比例；差额 ≤ 0 → `None`（什么都不记）。
    #[test]
    fn backfill_buckets_keeps_the_total_exact() {
        let _serial = serial();
        assert_eq!(
            backfill_buckets((100, 10, 50, 0), 0),
            None,
            "差额 0 ⇒ 什么都不记"
        );
        assert_eq!(
            backfill_buckets((100, 10, 50, 0), -5),
            None,
            "差额为负 ⇒ 什么都不记"
        );
        // 构成全 0 → 确定性兜底：全部落 input_fresh
        assert_eq!(backfill_buckets((0, 0, 0, 0), 80), Some((80, 0, 0, 0)));
        // 逐桶 clamp 会把 (+100, −60) 记成 100，而真实差额是 40 —— 这里必须恰好 40
        let got = backfill_buckets((100, 0, 0, 0), 40).unwrap();
        assert_eq!(got.0 + got.1 + got.2 + got.3, 40);
        // 最大余数：构成 (1,1,1,0)、差额 2 → 每桶 2/3 ⇒ 余数给前两桶
        assert_eq!(backfill_buckets((1, 1, 1, 0), 2), Some((1, 1, 0, 0)));
        // 构成比例：构成 (100,10,0,0)、差额 50 ⇒ 45.5 / 4.5 → 45 / 5
        let got = backfill_buckets((100, 10, 0, 0), 50).unwrap();
        assert_eq!(got.0 + got.1 + got.2 + got.3, 50);
        assert_eq!(got.0, 45);
        assert_eq!(got.1, 5);
    }

    /// **真机形态锁**：用量在 **`data.usage`**（真机 27,183 条 `assistant/message` 里顶层
    /// `usage` **0 条**），而仓库那份 `sample*.sanitized.jsonl` 黄金夹具写的是顶层 usage ——
    /// 照夹具写就是真机零命中（D-29 / D-36 同族的假绿）。
    /// 另外锁两条：`compaction`/`summary` 也带 usage 但**不计入**投影缓存 totals（必须按 type
    /// 过滤）；撕裂写的尾行（无 `\n`）**不消费**。
    #[test]
    fn usage_events_read_data_usage_and_ignore_the_top_level_fixture_shape() {
        let _serial = serial();
        let text = [
            r#"{"type":"session","version":4,"id":"s1"}"#,
            r#"{"type":"assistant/message","time":1000,"data":{"usage":{"inputTokens":7,"outputTokens":1,"cacheReadTokens":70,"totalTokens":78}}}"#,
            // 夹具形态（顶层 usage）——**不认**
            r#"{"type":"assistant/message","time":2000,"usage":{"inputTokens":9999}}"#,
            // compaction 带 data.usage，但真机不计入投影缓存 totals ⇒ 必须按 type 过滤
            r#"{"type":"compaction","time":3000,"data":{"usage":{"inputTokens":8888}}}"#,
        ]
        .join("\n")
            + "\n"
            // 撕裂写的尾行（**没有**换行）→ 不消费
            + r#"{"type":"assistant/message","time":4000,"data":{"usage":{"inputTokens":555}}}"#;
        let evs = usage_events_of(&text);
        assert_eq!(evs.len(), 1, "只认 data.usage + assistant/message + 整行");
        assert_eq!(evs[0].buckets, (7, 1, 70, 0));
        assert_eq!(evs[0].time_ms, 1000);
        // 缺 time 或缺 usage 的事件不参与归属
        let two_bad_events = [
            r#"{"type":"session","version":4,"id":"s1"}"#,
            r#"{"type":"assistant/message","data":{"usage":{"inputTokens":1}}}"#,
            r#"{"type":"assistant/message","time":2000}"#,
        ]
        .join("\n")
            + "\n";
        let digest = digest_of(&two_bad_events, 4);
        assert!(digest.usage_events.is_empty());
    }

    /// **纯日志会话不入账**（量的权威是投影缓存）：没有投影缓存文件、也没有已入账累计的会话
    /// 不得凭空多出一笔量；**水位也不推进**（等它进范围再算，「宁可重扫，不可漏」）。
    #[test]
    fn log_only_session_is_never_credited() {
        let _serial = serial();
        let now = local_ms(2026, 10, 7, 18);
        let t1 = local_ms(2026, 10, 7, 10);
        let dir = tempfile::tempdir().unwrap();
        write_log(
            dir.path(),
            "proj-a",
            "only-log",
            &[
                json!({"type":"session","version":3,"id":"only-log","cwd":"/p/A"}),
                usage_event(t1, 10, 100, 1),
            ],
        );
        let no_cursors = HashMap::new();
        let ctx = CollectContext::new(dir.path(), now, &no_cursors);
        let d = DshCollector::collect_with_home(&ctx, dir.path()).unwrap();
        assert!(
            d.details.is_empty(),
            "纯日志会话不得入账：{:?}",
            d.details.iter().map(|x| x.key.clone()).collect::<Vec<_>>()
        );
        let c = log_cursor(&d, "only-log");
        assert_eq!(c.byte_offset, 0, "不入账就不推进水位");
        assert_eq!(c.ordinal, 0);
    }

    /// **代际切换重新定界**（真机 7 个会话各有 2 个代际文件，且新代际**不是**旧代际超集）：
    /// 旧水位在新文件里没有意义 ⇒ 必须按「已入账累计」重新定界 ——
    /// 既不重复计入已入账的前缀，也不丢掉切换期间新增的尾巴。
    #[test]
    fn generation_switch_rearms_the_watermark_without_double_counting() {
        let _serial = serial();
        let now = local_ms(2026, 10, 7, 18);
        let t1 = local_ms(2026, 10, 7, 10);
        let t2 = local_ms(2026, 10, 7, 11);
        let dir = tempfile::tempdir().unwrap();
        write_log(
            dir.path(),
            "proj-a",
            "s1",
            &[
                json!({"type":"session","version":3,"id":"s1","cwd":"/p/A"}),
                usage_event(t1, 10, 100, 1),
            ],
        );
        projcache(
            dir.path(),
            "s1",
            "/p/A",
            (10, 1, 100, 0),
            1,
            1,
            0,
            SubAgentRow::ParentEmpty,
            None,
        );
        let no_cursors = HashMap::new();
        let ctx = CollectContext::new(dir.path(), now, &no_cursors);
        let d1 = DshCollector::collect_with_home(&ctx, dir.path()).unwrap();
        assert_eq!(total_buckets(&d1), 111);

        // 新代际 v4：与 v3 **同尾**（同一条已入账事件）再追加一条新事件
        let sess_dir = dir.path().join("sessions/proj-a/s1");
        let frame = zstd::stream::encode_all(
            log_body(&[
                json!({"type":"session","version":4,"id":"s1","cwd":"/p/A"}),
                usage_event(t1, 10, 100, 1),
                usage_event(t2, 20, 200, 2),
            ])
            .as_bytes(),
            3,
        )
        .unwrap();
        std::fs::write(sess_dir.join("session.v4.jsonl.zstd"), frame).unwrap();
        projcache(
            dir.path(),
            "s1",
            "/p/A",
            (30, 3, 300, 0),
            1,
            2,
            0,
            SubAgentRow::ParentEmpty,
            None,
        );
        // 借用不能挂在临时值上（E0716）：先绑变量
        let cursors1 = cursors_of(&d1);
        let ctx2 = CollectContext::new(dir.path(), now + 60_000, &cursors1);
        let d2 = DshCollector::collect_with_home(&ctx2, dir.path()).unwrap();
        assert_eq!(
            total_buckets(&d2),
            222,
            "只计**切换期间新增**的那一条：已入账的前缀不得重计，新尾巴也不得丢"
        );
        let row = hour_row(&d2, "s1", &hour_key_of_host(t2));
        assert_eq!(row.buckets.cache_read, 200);
        assert!(
            !d2.details
                .iter()
                .any(|x| x.key.hour_key == hour_key_of_host(t1)),
            "已入账的那条事件不得在新代际里重记"
        );
    }

    /// **裁决 B（原始日志侧）**：归桶用**日志自己的时间** —— 最后一条带时间的事件
    /// （退路是该代际文件的真实 mtime），**不是采集时刻**。
    #[test]
    fn log_facts_bucket_by_the_last_event_time_not_collection_time() {
        let _serial = serial();
        const EVENT: i64 = 1_680_000_000_000;
        const NOW: i64 = 1_700_000_000_000;
        let h_event = hour_key_of_host(EVENT);
        assert_ne!(
            h_event,
            hour_key_of_host(NOW),
            "前提：事件时刻与采集时刻必须分处不同小时"
        );
        let dir = tempfile::tempdir().unwrap();
        write_log(
            dir.path(),
            "proj-a",
            "raw-4",
            &[
                json!({"type":"session","version":3,"id":"raw-4","cwd":"/p/A"}),
                json!({"type":"tool/call","time":EVENT - 2_000,"data":{"name":"Bash","callId":"c1"}}),
                json!({"type":"tool/result","time":EVENT - 1_000,"data":{"toolCallId":"c1"}}),
                json!({"type":"turn/start","time":EVENT - 500}),
                json!({"type":"turn/end","time":EVENT,"data":{"reason":{"kind":"completed"}}}),
            ],
        );
        let no_cursors = HashMap::new();
        let ctx = CollectContext::new(dir.path(), NOW, &no_cursors);
        let d1 = DshCollector::collect_with_home(&ctx, dir.path()).unwrap();
        let row = d1
            .details
            .iter()
            .find(|x| x.key.session_id == "raw-4")
            .unwrap();
        assert_eq!(row.counters.tool_calls, 1, "前提：本轮真的产出了事实");
        assert_eq!(
            row.key.hour_key,
            h_event,
            "日志侧事实必须归日志自己的时间（用采集时刻会落在 {}）",
            hour_key_of_host(NOW)
        );
    }

    /// **M3 / W-49（fix round 1）**：日志水位按**代际**记（真机 7/172 个会话目录有多代际文件）。
    /// ① 新代际是旧水位的**超集** → 差值恰好是「切换期间新增的事件」，照常计（不得整份丢掉）；
    /// ② 新代际**不是**超集（截断 / 重建）→ 只对齐水位、本轮一个事实都不计
    ///   （旧语义「回退按整份重计」会把新文件里已有的全部报错/工具/样本再计一遍）；
    /// ③ 对齐之后水位要能继续正常工作（不是冻死）。
    #[test]
    fn generation_switch_counts_a_superset_tail_and_aligns_a_non_superset() {
        let _serial = serial();
        const T0: i64 = 1_680_000_000_000;
        let dir = tempfile::tempdir().unwrap();
        let sess_dir = dir.path().join("sessions/proj-a/raw-5");
        let v3 = write_log(
            dir.path(),
            "proj-a",
            "raw-5",
            &[
                json!({"type":"session","version":3,"id":"raw-5","cwd":"/p/A"}),
                json!({"type":"tool/call","time":T0,"data":{"name":"Bash","callId":"c1"}}),
                json!({"type":"tool/result","time":T0 + 500,"data":{"toolCallId":"c1"}}),
            ],
        );
        let no_cursors = HashMap::new();
        let (d1, _) = run(dir.path(), &no_cursors);
        assert_eq!(
            d1.details
                .iter()
                .map(|x| x.counters.tool_calls)
                .sum::<i64>(),
            1
        );
        let c1 = d1
            .cursors
            .iter()
            .find(|c| c.session_id == "log:raw-5")
            .expect("前提：必须落日志侧水位")
            .clone();
        assert!(
            c1.state_json.contains("\"gen\":3"),
            "前提：水位必须记住代际号：{}",
            c1.state_json
        );

        // ① v4 = v3 的内容 + 一条新工具调用（**超集**）→ 只计新增的那一条
        let v4 = sess_dir.join("session.v4.jsonl.zstd");
        let mut body = std::fs::read(&v3).unwrap();
        body.extend(
            zstd::stream::encode_all(
                log_body(&[
                    json!({"type":"tool/call","time":T0 + 2_000,"data":{"name":"Read","callId":"c2"}}),
                    json!({"type":"tool/result","time":T0 + 2_500,"data":{"toolCallId":"c2"}}),
                ])
                .as_bytes(),
                3,
            )
            .unwrap(),
        );
        std::fs::write(&v4, body).unwrap();
        let (d2, _) = run(dir.path(), &cursors_of(&d1));
        assert_eq!(
            d2.details
                .iter()
                .map(|x| x.counters.tool_calls)
                .sum::<i64>(),
            1,
            "代际切换 + 超集：只计「切换期间新增的事件」，旧事实一个都不能重计"
        );
        assert_eq!(
            d2.details
                .iter()
                .find_map(|x| x.counters.tool_stats.get("Read").copied()),
            Some((1, 500)),
            "新代际里新增的工具调用（含耗时）必须照常入账"
        );

        // ② v5 = **更小**的文件（非超集）→ 只对齐水位、不计任何事实
        let v5 = sess_dir.join("session.v5.jsonl.zstd");
        std::fs::write(
            &v5,
            zstd::stream::encode_all(
                log_body(&[
                    json!({"type":"session","version":5,"id":"raw-5","cwd":"/p/A"}),
                    json!({"type":"tool/call","time":T0,"data":{"name":"Bash","callId":"c1"}}),
                ])
                .as_bytes(),
                3,
            )
            .unwrap(),
        )
        .unwrap();
        let (d3, _) = run(dir.path(), &cursors_of(&d2));
        assert!(
            d3.details.is_empty(),
            "非超集的代际切换：本轮不得计任何事实"
        );
        assert_eq!(d3.new_records, 0);

        // ③ 对齐之后水位继续可用：v6 = v5 的内容 + 一条新调用（超集）→ 只计新增那一条
        let v6 = sess_dir.join("session.v6.jsonl.zstd");
        let mut body = std::fs::read(&v5).unwrap();
        body.extend(
            zstd::stream::encode_all(
                log_body(&[
                    json!({"type":"tool/call","time":T0 + 5_000,"data":{"name":"Write","callId":"c9"}}),
                    json!({"type":"tool/result","time":T0 + 5_100,"data":{"toolCallId":"c9"}}),
                ])
                .as_bytes(),
                3,
            )
            .unwrap(),
        );
        std::fs::write(&v6, body).unwrap();
        let (d4, _) = run(dir.path(), &cursors_of(&d3));
        assert_eq!(
            d4.details
                .iter()
                .map(|x| x.counters.tool_calls)
                .sum::<i64>(),
            1,
            "对齐之后水位必须继续正常推进（不是冻死）"
        );
        assert_eq!(
            d4.details
                .iter()
                .find_map(|x| x.counters.tool_stats.get("Write").copied()),
            Some((1, 100))
        );
    }

    /// **M2（fix round 1）**：目录级读失败不得被静默吞。
    /// `NotFound` = 未装 dsh / 从未落盘（正常空态）；其余（权限 / IO）= 「这个源读不了」
    /// → **整源响亮失败**（落 `UsageSourceStatus.errorCode`，与 D-24 同口径）。
    #[cfg(unix)]
    #[test]
    fn unreadable_root_directories_fail_the_whole_source_loudly() {
        let _serial = serial();
        use std::os::unix::fs::PermissionsExt;
        let no_cursors = HashMap::new();

        // ① 投影缓存目录列不出来
        let dir = tempfile::tempdir().unwrap();
        let pc = dir.path().join("storages/session_projcache/sessions");
        std::fs::create_dir_all(&pc).unwrap();
        std::fs::set_permissions(&pc, std::fs::Permissions::from_mode(0o000)).unwrap();
        assert!(
            std::fs::read_dir(&pc).is_err(),
            "前提：0000 权限下目录必须列不出来"
        );
        let ctx = CollectContext::new(dir.path(), 1_700_000_000_000, &no_cursors);
        let err = DshCollector::collect_with_home(&ctx, dir.path()).unwrap_err();
        assert_eq!(
            err.code, "usage-source-io",
            "投影缓存目录读不了必须整源失败：{err:?}"
        );
        std::fs::set_permissions(&pc, std::fs::Permissions::from_mode(0o755)).ok();

        // ② 会话日志根目录列不出来（投影缓存不存在 → 正常空态，必须继续走到 B 段）
        let dir2 = tempfile::tempdir().unwrap();
        let root = dir2.path().join("sessions");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o000)).unwrap();
        assert!(
            std::fs::read_dir(&root).is_err(),
            "前提：0000 权限下目录必须列不出来"
        );
        let ctx2 = CollectContext::new(dir2.path(), 1_700_000_000_000, &no_cursors);
        let err2 = DshCollector::collect_with_home(&ctx2, dir2.path()).unwrap_err();
        assert_eq!(
            err2.code, "usage-source-io",
            "会话日志根目录读不了必须整源失败：{err2:?}"
        );
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).ok();
    }

    /// **结构性锁（fix round 2 实测教训）**：本文件**每一个** `#[test]` 的函数体都必须
    /// 以 `let _serial = serial();` 开头 —— 日志侧 L2 是进程级全局注册表，任何调用
    /// `collect` 的用例都会经 `retain_existing(&live)` 清掉**同 namespace 里不在它本轮
    /// live 集合中的条目**；漏掉锁的用例会把**并行跑的**用例的缓存条目清掉 →
    /// 对方的「未变代际零解码」断言随机变红（fix round 2 就是这样偶发红 2/15：新加的
    /// `late_result_duration_survives_the_new_total_gate` 漏了锁）。
    /// 用**源码自省**把这条纪律变成机械锁：漏一个 → 本用例立刻红。
    #[test]
    fn every_test_in_this_file_takes_the_serial_lock() {
        let _serial = serial();
        let src = include_str!("dsh.rs");
        let lines: Vec<&str> = src.lines().collect();
        let mut missing: Vec<usize> = Vec::new();
        for (i, line) in lines.iter().enumerate() {
            if line.trim() != "#[test]" {
                continue;
            }
            let window = lines.iter().skip(i + 1).take(4);
            if !window.into_iter().any(|l| l.contains("serial()")) {
                missing.push(i + 1);
            }
        }
        assert!(
            missing.is_empty(),
            "这些 #[test]（行号）没有取串行锁 `let _serial = serial();`：{missing:?}"
        );
    }

    /// **W-19 措辞更正（M5，fix round 1）**：本用例只是「派生值两两不撞」的**自证**
    /// （`scan_namespace` = `"usage-"+db_id()`、`db_id` 互异 ⇒ 必然成立），
    /// 它**抓不到**「给 `SessionFileScan::new` 传硬编码 ns」。真正生效的是每个采集器文件里
    /// 自己的 `assert_eq!(SCAN_NAMESPACE, scan_namespace(sid))` 与 `log_scan_namespace()` 的自洽断言。
    #[test]
    fn all_seven_namespaces_are_distinct_and_prefixed() {
        let _serial = serial();
        let nss: Vec<&str> = UsageSourceId::ALL
            .iter()
            .map(|s| scan_namespace(*s))
            .collect();
        for ns in &nss {
            assert!(
                ns.starts_with("usage-"),
                "namespace 必须带 usage- 前缀：{ns}"
            );
        }
        let uniq: std::collections::HashSet<&&str> = nss.iter().collect();
        assert_eq!(
            uniq.len(),
            nss.len(),
            "七源 namespace 必须互不相同：{nss:?}"
        );
    }

    /// **登记锁（独立验证复核 §3.2.2 阻塞 4）**：漏登记不是编译错——`run_collection`
    /// 只遍历 `collectors::all()`，这个源会被静默跳过。
    #[test]
    fn collector_is_registered_in_all() {
        let _serial = serial();
        let ids: Vec<UsageSourceId> = crate::services::usage::collectors::all()
            .iter()
            .map(|c| c.source_id())
            .collect();
        assert!(
            ids.contains(&UsageSourceId::Dsh),
            "dsh 采集器必须登记进 collectors::all()，实际登记：{ids:?}"
        );
    }

    /// **全表锁（§3.2.2 阻塞 4 + W-19 + W-27）**：
    /// ① 七源必须**齐全**且**顺序 = `UsageSourceId::ALL`**（= 契约 `UsageSourceStatus` 的展示顺序）；
    /// ② 逐源断言 `c.caps() == caps_of(c.source_id())`（**W-27**：把「不许覆写 caps」从文档约束
    ///    变成机械锁——`caps.rs` 明写「采集器与查询层共用同一张表，杜绝两边漂移」，
    ///    而可覆写点正是漂移的唯一入口）；
    /// ③（W-19 的 ns 断言已拆到 `all_seven_namespaces_are_distinct_and_prefixed`，M5：措辞降级）。
    /// 少一个源在 `run_collection` 里不会报错，只会让 Task 24 的人工验收面少一行、
    /// Task 23B 的 `sources.len() == 7` 变红。
    #[test]
    fn all_seven_collectors_are_registered_in_order() {
        let _serial = serial();
        let cols = crate::services::usage::collectors::all();
        let ids: Vec<UsageSourceId> = cols.iter().map(|c| c.source_id()).collect();
        assert_eq!(
            ids,
            UsageSourceId::ALL.to_vec(),
            "登记表必须恰好是七个源且顺序与 UsageSourceId::ALL 一致"
        );
        // W-27：`caps()` 是**可覆写点**，生产采集器一律不许自造第二张可得性表
        for c in &cols {
            assert_eq!(
                c.caps(),
                caps_of(c.source_id()),
                "{} 覆写了 caps()（W-27：查询层与采集层必须共用 caps.rs 的同一张表）",
                c.source_id().db_id()
            );
        }
        // W-19 的「七源 ns 互不相同」已拆到同文件的
        // `all_seven_namespaces_are_distinct_and_prefixed`（**M5**：那段是派生值的自证，
        // 抓不到「给 SessionFileScan::new 传硬编码 ns」，措辞已降级）。
    }
}
// ==== usage 自省锁：排除区结束，勿删勿复制 ====
