//! Codex 采集器。实测约束（说明书 §5.3，逐条落码）：
//! 1. **扫描范围必须是两棵树**：`sessions/**`（398 文件）+ `archived_sessions/*.jsonl`（27 文件，
//!    归档树命中率仅 51.5%，漏掉会失真）；**不需要 SQLite 作路径来源**（0 条在外）；
//! 2. **不得使用 `threads.tokens_used`**（与 rollout 仅 44.1% 吻合）——本采集器全程只读 rollout；
//! 3. **逐轮累加前必须相邻去重**：相邻四桶完全相同则丢弃（本机 1,945 条 / 114 文件）；
//!    **禁止用 `turn_id` 去重四桶**（`turn_context` 非每轮写入、`turn_id` 缺失 1.5%）；
//!    而 `task_complete` 的**时长样本**才按 (会话, turn_id) 去重（2,885 → 2,308）——两件事；
//! 4. **stub 事件必须剔除**：四桶全 0 而 `total_tokens` 非 0（840 条 / 86 文件）→ semantics::normalize；
//! 5. 模型靠前序 `turn_context.model` **有状态继承**（跨轮续读状态里保住）；
//! 6. **时区一律以宿主本地为基准**（spec §P6 / Q-4 用户裁决）：`turn_context.timezone`
//!    （本机 `Asia/Shanghai`）**只读入续读状态备查**（`CodexFileState::tz_name`），
//!    **绝不得参与键计算**——小时/日键一律由 `hour_key_of_host` 产生。
//!    **W-14 随本次裁决消除**：本源曾是七源里唯一用命名时区落库键的采集器，而查询侧
//!    `resolve_range` 一律按宿主本地生成窗口键 ⇒ 两者偏移不同时边界行会落到窗口外
//!    （静默少算）。现在两侧同源，该分叉在结构上不可能发生（有 A-1 跨时区锁钉住）。
//!
//! ## 本文件要守住的三条跨任务纪律
//! * **B1（`continue` 而非 `return`）**：行循环里有三处「跳过这一行」——stub 剔除、
//!   相邻重放丢弃、`last_token_usage` 缺失。写成 `return` 会结束**整个文件**的扫描：
//!   后续行永不入账，而且连文件末尾的 `b.push_cursor(next)` 都被跳过 → 该文件游标永不落库
//!   → 下一轮整文件重读重算（明细是累加语义 → 账目按轮次线性膨胀）。本机 stub 出现在 86 个
//!   文件、相邻重放出现在 114 个文件 → 命中面覆盖绝大多数 codex 文件。
//! * **W-11 / W-12**：`AdjacentDrop` 与 `DedupSet` 都是**单文件、单轮**实例（每个文件各自新建）；
//!   跨轮的那一对相邻重放靠 `AdjacentDrop::seed/last` 把上一轮末条四桶种回来。
//! * **W-19**：`SessionFileScan` 的 namespace 由 `scan_namespace(sid)` 派生，不手写字符串字面量。
//!
//! ## 两处相对任务书代码块的**有意偏离**（详见报告「偏差申报」）
//! * **W-06 / D-24**：单文件读失败**不一律 warn + 跳过**——只有「枚举与读取之间的轮转竞态」
//!   （文件已消失）才跳过，其余一律整源 `Err("usage-source-io")`（否则用户看到的是
//!   「这个源没有用量」而不是「这个源读不了」）。为此 `scan_one` 返回 `Result`。
//! * **R1 / 说明书 §4.3**：补上 `user_message` → `user_est` 的一支（任务书的代码块漏了它，
//!   而 spec 与 GC 18 都明写 codex 属于「能读到用户文本」的三源之一）。
use std::collections::{HashSet, VecDeque};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{file_key_of, file_read_failure_is_benign, DeltaBuilder, PendingTools};
use crate::monitor::session_scan::SessionFileScan;
use crate::services::usage::collect::{scan_namespace, CollectContext, UsageCollector};
use crate::services::usage::dedup::{turn_key, AdjacentDrop, DedupSet};
// **不得**导入 `CursorDelta`：本文件从头到尾没有显式写它的类型名（`read.next` 一路类型推断），
// 导进来就是 `unused_imports`，而门禁是 `-D warnings`（与 Task 11 的 D-11-2 同族）。
use crate::services::usage::delta::{DetailKey, SourceDelta};
use crate::services::usage::error::UsageError;
use crate::services::usage::model::{UsageBuckets, UsageSourceId};
use crate::services::usage::project::project_key_of;
// **Q-4 / A-1**：只留宿主本地键入口——`SourceTz` / `hour_key_of(ts, tz)` 已从本源退出
// （键一律宿主本地），导进来就是 `unused_imports`，而门禁是 `-D warnings`。
use crate::services::usage::range::hour_key_of_host;
use crate::services::usage::semantics::{
    default_policy, normalize, resolve_semantics, user_est_of, RawUsage,
};

/// 本源 `SessionFileScan` 的 namespace（GC 5 / **W-19**）：**必须**由 `scan_namespace(sid)`
/// 派生，不得手写字符串字面量——同 ns 混存别的产物类型会让 `Arc::downcast` 失配、
/// 缓存静默失效（每轮重新解析，正是 L2 要省掉的那笔开销）。
const SCAN_NAMESPACE: &str = scan_namespace(UsageSourceId::Codex);
const SCAN: SessionFileScan = SessionFileScan::new(SCAN_NAMESPACE);

/// 同 turn 去重集合的**有界**上限（防长文件状态膨胀；超限丢最老，代价=跨轮重复计一次时长）
const TURN_SEEN_CAP: usize = 1024;

/// 一棵 rollout 树：「本树的根 + 本树的文件筛选」。
/// **两棵树各自用各自的根**算游标键（B5），所以它必须是 (根, 筛选) 的成对形态。
/// （写成裸元组数组会触发 `clippy::type_complexity` —— 门禁 `-D warnings` 下即 error，
/// 与 D-05 / D-18 / D-25 同族的「任务书代码块过不了本仓门禁」；语义一字未改。）
type CodexRoot = (std::path::PathBuf, fn(&std::path::Path) -> bool);

pub struct CodexCollector;

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
// `#[serde(default)]`：**前向兼容**——将来给本状态加字段时，旧 `state_json` 只丢该字段而不是
// 整份反序列化失败（后者会静默丢掉相邻去重末条与 turn 记忆）。
#[serde(default)]
struct CodexFileState {
    session_id: String,
    cwd: String,
    originator: Option<String>,
    is_subagent: bool,
    parent: Option<String>,
    model: String,
    tz_name: Option<String>,
    /// 相邻去重上一条（跨轮必须保住，否则每轮第一条都会被误判）
    adjacent_last: Option<UsageBuckets>,
    tools: PendingTools,
    /// 已计过时长的 `(会话 id, turn_id)` 对。
    /// **为什么带会话 id**（相对任务书代码块的一处收紧）：一个 rollout 可含多条
    /// `session_meta`（本机 119/425 文件，父子线程同文件），而 `turn_id` 只在**本会话**内唯一；
    /// 只存 turn_id、播种时统一挂到「上一轮最后那个 session_id」上，会让 A 线程的 turn
    /// 记在 B 线程名下 → A 的重复 `task_complete` 漏判（**静默多算一次时长**）。
    turn_seen: VecDeque<(String, String)>,
}

fn is_rollout(p: &std::path::Path) -> bool {
    p.file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.starts_with("rollout") && n.ends_with(".jsonl"))
        .unwrap_or(false)
}

fn is_jsonl(p: &std::path::Path) -> bool {
    p.extension().map(|e| e == "jsonl").unwrap_or(false)
}

fn ts_ms_of(v: &Value) -> Option<i64> {
    v.get("timestamp")
        .and_then(|s| s.as_str())
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.timestamp_millis())
}

/// `session_meta.payload` → 线程身份（id/cwd/originator/子代理血缘）。
/// `source` 可为对象（本机 65 条子代理），另有 `thread_source`（user 1541 / subagent 66）。
fn thread_identity(payload: &Value) -> (String, String, Option<String>, bool, Option<String>) {
    let id = payload
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let cwd = payload
        .get("cwd")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    // 用 originator 而非 source（source 是混合类型，§5.2）
    let originator = payload
        .get("originator")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let spawn = payload.pointer("/source/subagent/thread_spawn");
    let thread_source_sub =
        payload.get("thread_source").and_then(|v| v.as_str()) == Some("subagent");
    let is_sub = spawn.is_some() || thread_source_sub;
    let parent = spawn
        .and_then(|s| s.get("parent_thread_id"))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    (id, cwd, originator, is_sub, parent)
}

/// `event_msg.mcp_tool_call_end.payload.duration` 是**对象** `{secs, nanos}`
/// （实测逐字样例：`{"secs":3,"nanos":517177833}` → 3,517 ms；
/// 见 `research/可得性探测-文件类三源.md:171`）。旧实现按整数读（`as_i64()`）恒 `None`，
/// 1,406 条原生 MCP 耗时一条都进不了账（评审 B7）。
///
/// **判据是「字段在不在」，不是「值等不等于 0」**（**W-36**，Task 12 评审裁决 C）：
/// `{"secs":0,"nanos":0}` 是**在场的真数据**（一次快得测不出毫秒的调用），必须计成 **0 ms**
/// 且**次数照记**；只有 `duration` **字段缺失**（或不是整数/字符串/`{secs,nanos}` 对象）才是
/// 「不可得」→ `None`（调用方 `continue`）。
/// 「在场值的 0」与「缺失的不可得」是 GC 7 的两面：把前者当后者，就是本分支
/// （Task 3/4/9/10/11 一路）反复抓到的根因类——**把在场值误当缺失**。
/// 兼容性防御：某版本给整数/字符串时按**毫秒**解释。
fn mcp_duration_ms(payload: &Value) -> Option<i64> {
    let d = payload.get("duration")?; // 字段缺失 → 不可得（**只有这一种**才返回 None）
    if let Some(ms) = d.as_i64() {
        return Some(ms);
    }
    if let Some(s) = d.as_str().and_then(|s| s.parse::<i64>().ok()) {
        return Some(s); // 兜底：字符串形态的毫秒
    }
    // 对象形态 {secs, nanos}：**任何一个子字段在场**就说明 duration 在场 → 在场的 0 是数据
    let secs = d.get("secs").and_then(|v| v.as_i64());
    let nanos = d.get("nanos").and_then(|v| v.as_i64());
    if secs.is_none() && nanos.is_none() {
        return None; // 不是 {secs,nanos} 对象（形状不认识）→ 不可得，不猜
    }
    Some(secs.unwrap_or(0) * 1000 + nanos.unwrap_or(0) / 1_000_000)
}

impl UsageCollector for CodexCollector {
    fn source_id(&self) -> UsageSourceId {
        UsageSourceId::Codex
    }

    fn collect(&self, ctx: &CollectContext<'_>) -> Result<SourceDelta, UsageError> {
        // **供应商规则从 ctx 注入**（§3.2.3 FIX-6）：lib 单测构建注入空规则 → 零 DB；
        // 生产/集成构建才回落到 settings::load()。采集器不得直连设置层。
        let rules = ctx.provider_rules();
        let mut b = DeltaBuilder::new(UsageSourceId::Codex, rules);
        let mut live: HashSet<std::path::PathBuf> = HashSet::new();
        let roots: [CodexRoot; 2] = [
            (ctx.home.join(".codex").join("sessions"), is_rollout),
            (ctx.home.join(".codex").join("archived_sessions"), is_jsonl),
        ];
        for (root, accept) in roots {
            for (path, _m) in SCAN.collect(&root, accept) {
                live.insert(path.clone());
                // 把**本棵树的根**传下去：游标键 = 相对根的路径（B5）
                self.scan_one(ctx, &root, &path, &mut b)?;
            }
        }
        SCAN.retain_existing(&live);
        Ok(b.finish())
    }
}

impl CodexCollector {
    /// 单文件一遍扫描（GC 4）。**行内三处跳过一律 `continue`，绝不 `return`**（B1）。
    fn scan_one(
        &self,
        ctx: &CollectContext<'_>,
        root: &std::path::Path,
        path: &std::path::Path,
        b: &mut DeltaBuilder,
    ) -> Result<(), UsageError> {
        // 游标键 = **相对本棵树的根**的路径（B5：`sessions/…` 与 `archived_sessions/…`
        // 各自成前缀，同名 rollout 不会互撞；也绝不用文件名 stem）
        let file_key = file_key_of(root, path);
        let prev = ctx.cursor_of(&file_key).cloned();
        let mut st: CodexFileState = prev
            .as_ref()
            .and_then(|p| serde_json::from_str(&p.state_json).ok())
            .unwrap_or_default();
        let read = match ctx.read_incremental(path, &file_key) {
            Ok(r) => r,
            Err(e) => {
                // **W-06 / D-24**：只有「文件已消失」这一种轮转竞态才允许跳过；其余读失败
                // 必须整源响亮失败（`usage-source-io` → `UsageSourceStatus.errorCode`），
                // 不得只 `log::warn!` 后跳过（那会让 UI 显示「这个源没有用量」）。
                if file_read_failure_is_benign(path, &e) {
                    log::warn!(
                        "usage/codex: 读取 {} 失败（文件已消失，跳过本轮）: {}",
                        path.display(),
                        e
                    );
                    return Ok(());
                }
                return Err(UsageError::new(
                    "usage-source-io",
                    format!("read {} failed: {e}", path.display()),
                ));
            }
        };
        if read.lines.is_empty() && !read.rescan {
            b.push_cursor(read.next);
            return Ok(());
        }
        if read.rescan {
            // 截断/重写：续读状态与相邻去重末条一起重置（上一代文件的末条对新文件无意义）
            st = CodexFileState::default();
        }
        // ---- W-11 / W-12：比较器与去重集**每个文件、每一轮各自新建** ----
        let mut adjacent = AdjacentDrop::new();
        // 把上一轮的「最后一条四桶」种回来：否则跨轮边界的那一对相邻重放会漏判（静默多算）
        adjacent.seed(st.adjacent_last);
        // `DedupSet` 无上界 → 只在单轮扫描内持有（不挂到长生命周期对象上）
        let mut turn_dedup = DedupSet::new();
        for (sid, id) in &st.turn_seen {
            turn_dedup.admit(turn_key("codex", sid, id));
        }
        // ---- 一遍扫描 ----
        for line in &read.lines {
            let Ok(v) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            b.new_records += 1;
            let ts = ts_ms_of(&v).unwrap_or(ctx.now_ms);
            let kind = v.get("type").and_then(|s| s.as_str()).unwrap_or("");
            // 借用而非 `cloned()`：单行可达 2.6 MB（实测 2,745,222 B），深拷贝整份 payload
            // 对本源是白付一次 memcpy（语义完全一致，见报告「偏差申报」）。
            let null = Value::Null;
            let payload = v.get("payload").unwrap_or(&null);
            match kind {
                "session_meta" => {
                    let (id, cwd, orig, is_sub, parent) = thread_identity(payload);
                    if !id.is_empty() {
                        // 会话维度登记（**会话级近似**：首个带 cwd 的记录胜出）；
                        // `is_subagent` 走 DeltaBuilder 的 AND 合并（D17/D24 裁决 B）
                        b.session(
                            &id,
                            Some(&cwd),
                            None,
                            is_sub,
                            parent.clone(),
                            orig.clone(),
                            ts,
                        );
                        st.session_id = id;
                        st.cwd = cwd;
                        st.originator = orig;
                        st.is_subagent = is_sub;
                        st.parent = parent;
                    }
                }
                "turn_context" => {
                    // 模型有状态继承（跨轮靠 state_json）
                    if let Some(m) = payload.get("model").and_then(|v| v.as_str()) {
                        st.model = m.to_string();
                    }
                    // **Q-4 裁决**：各源自带时区**只读入备查**（随 `state_json` 持久化），
                    // **绝不参与键计算**——下面的 `hour` 一律走 `hour_key_of_host`。
                    if let Some(tz) = payload.get("timezone").and_then(|v| v.as_str()) {
                        st.tz_name = Some(tz.to_string());
                    }
                }
                "event_msg" => {
                    let etype = payload.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    // 落库键只由**宿主本地**算法产生（spec §P6 / A-1 锁）
                    let hour = hour_key_of_host(ts);
                    // **供应商唯一入口**（GC 6 / B6）：经 `provider_of` 记录三态
                    let (provider, _) = b.provider_of(None, &st.model);
                    // 记录级项目键：codex 的项目归属来自**当前线程**的 session_meta.cwd
                    // （同一 rollout 可含父子两条 session_meta，各自 cwd 不同 → 必须按 st.cwd 取）
                    let key = DetailKey {
                        session_id: st.session_id.clone(),
                        hour_key: hour.clone(),
                        day_key: hour.chars().take(10).collect(),
                        project_key: project_key_of(Some(&st.cwd)),
                        model: st.model.clone(),
                        provider,
                    };
                    match etype {
                        "token_count" => {
                            // 四桶：total_token_usage 只作诊断，一律取 last_token_usage（§7 未证实项 6）
                            // **B1：下面三处一律用 `continue`，绝不用 `return`**
                            let Some(u) = payload.pointer("/info/last_token_usage") else {
                                continue;
                            };
                            let raw = RawUsage {
                                input_raw: u
                                    .get("input_tokens")
                                    .and_then(|x| x.as_i64())
                                    .unwrap_or(0),
                                cache_read: u
                                    .get("cached_input_tokens")
                                    .and_then(|x| x.as_i64())
                                    .unwrap_or(0),
                                cache_write: u
                                    .get("cache_write_input_tokens")
                                    .and_then(|x| x.as_i64())
                                    .unwrap_or(0),
                                output: u
                                    .get("output_tokens")
                                    .and_then(|x| x.as_i64())
                                    .unwrap_or(0),
                                reasoning: 0,
                                total_raw: u.get("total_tokens").and_then(|x| x.as_i64()),
                            };
                            let sem = resolve_semantics(default_policy(UsageSourceId::Codex), &raw);
                            let Some(n) = normalize(&raw, sem) else {
                                continue; // stub 剔除（B1：continue）
                            };
                            // 相邻重复事件丢弃（**不得用 turn_id**）
                            if !adjacent.admit(n.buckets) {
                                continue; // B1：continue —— 只跳过这一行，后面的行还要处理
                            }
                            let d = b.detail(key, ts);
                            d.buckets.add(&n.buckets);
                            d.request_total += n.request_total;
                            d.requests += 1;
                            d.cache_semantics = sem;
                        }
                        // **R1 / 说明书 §4.3**：codex 属于「能读到用户文本」的三源之一
                        // （claude / codex / kimi）→ 「用户输入(估)」必须出真值，两档都不得是 0
                        // 或 null。文本在 `event_msg.payload.message`（本机 2,426 条 / 2.24 M 字符）。
                        // 任务书 Step 3 的代码块没有这一支（见报告「偏差申报」）。
                        "user_message" => {
                            if let Some(text) = payload.get("message").and_then(|v| v.as_str()) {
                                let est = user_est_of(text);
                                let d = b.detail(key, ts);
                                // 与 claude 同口径：该条估算值为 0 也是**真值**
                                // （能读到文本就不算「不可得」；不可得的源压根不进这一支）
                                d.user_est = Some(d.user_est.unwrap_or(0) + est);
                            }
                        }
                        "task_started" => {
                            let d = b.detail(key, ts);
                            d.counters.turns += 1;
                        }
                        "task_complete" => {
                            let turn_id = payload
                                .get("turn_id")
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                            // 时长样本按 (源, 会话, turn) 去重（2,885 → 2,308）
                            if turn_id.is_empty()
                                || turn_dedup.admit(turn_key("codex", &st.session_id, turn_id))
                            {
                                if !turn_id.is_empty() {
                                    st.turn_seen
                                        .push_back((st.session_id.clone(), turn_id.to_string()));
                                    while st.turn_seen.len() > TURN_SEEN_CAP {
                                        st.turn_seen.pop_front();
                                    }
                                }
                                if let Some(ms) =
                                    payload.get("duration_ms").and_then(|x| x.as_i64())
                                {
                                    b.detail(key.clone(), ts).counters.turn_ms.push(ms);
                                }
                                if payload.pointer("/error/message").is_some() {
                                    b.detail(key, ts).counters.error_model += 1;
                                }
                            }
                        }
                        "error" => {
                            b.detail(key, ts).counters.error_model += 1;
                        }
                        "turn_aborted" => {
                            let reason =
                                payload.get("reason").and_then(|v| v.as_str()).unwrap_or("");
                            let d = b.detail(key, ts);
                            if reason == "interrupted" {
                                d.counters.interrupted += 1;
                            } else {
                                d.counters.error_turn += 1;
                            }
                        }
                        "patch_apply_end" => {
                            if payload.get("success").and_then(|v| v.as_bool()) == Some(false) {
                                b.detail(key, ts).counters.error_tool += 1;
                            }
                        }
                        // **B7 修复点**：原生 MCP 耗时是 **`event_msg`**（不是 `response_item`！），
                        // `duration` 是**对象** `{"secs":3,"nanos":517177833}`（不是整数），
                        // 工具名在 `payload.invocation.tool`（`payload.tool` 不存在）。
                        // 三重错叠加的后果：矩阵列明的 **1,406 条原生 MCP 耗时全部丢失**，
                        // `tool_ms` / `tool_stats` 系统性偏低，而 `caps_of(Codex).tool_calls = true`
                        // 会让 UI 以为这是完整值（"可得却漏采"，违反 D 列"不可得才空态"）。
                        //
                        // **口径（§3.2.2 次要 8）**：调用**同时计入 `tool_calls` 与
                        // `tool_stats[name].0`**——只加 `ms` 会让工具条目变成 `(0, 3517)`，
                        // 进 `top_tool_ms`（Task 18）后在 UI 上显示"0 次调用却有 3.5 秒"；
                        // 次数与耗时必须同源。
                        // **判据是「字段在不在」**（**W-36**，评审裁决 C）：`duration` **字段缺失**
                        // 的事件整条丢弃（不可得 → 不记 0）；而 `{"secs":0,"nanos":0}` 是**在场的
                        // 真数据**（本机 1,406 条里有 2 条）→ 计入次数、耗时记 0。
                        "mcp_tool_call_end" => {
                            let Some(ms) = mcp_duration_ms(payload) else {
                                continue; // B1：continue（不记 0、也不终止整个文件）
                            };
                            let name = payload
                                .pointer("/invocation/tool")
                                .and_then(|v| v.as_str())
                                .filter(|s| !s.is_empty())
                                .unwrap_or("mcp");
                            let d = b.detail(key, ts);
                            d.counters.tool_calls += 1;
                            d.counters.tool_ms += ms;
                            let stat = d
                                .counters
                                .tool_stats
                                .entry(name.to_string())
                                .or_insert((0, 0));
                            stat.0 += 1;
                            stat.1 += ms;
                        }
                        _ => {}
                    }
                }
                "response_item" => {
                    let ptype = payload.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    // 落库键只由**宿主本地**算法产生（spec §P6 / A-1 锁）
                    let hour = hour_key_of_host(ts);
                    // **供应商唯一入口**（GC 6 / B6）
                    let (provider, _) = b.provider_of(None, &st.model);
                    let key = DetailKey {
                        session_id: st.session_id.clone(),
                        hour_key: hour.clone(),
                        day_key: hour.chars().take(10).collect(),
                        project_key: project_key_of(Some(&st.cwd)),
                        model: st.model.clone(),
                        provider,
                    };
                    match ptype {
                        "function_call" | "custom_tool_call" => {
                            let name = payload
                                .get("name")
                                .and_then(|v| v.as_str())
                                .unwrap_or("unknown");
                            let call_id = payload
                                .get("call_id")
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                            let d = b.detail(key, ts);
                            d.counters.tool_calls += 1;
                            d.counters
                                .tool_stats
                                .entry(name.to_string())
                                .or_insert((0, 0))
                                .0 += 1;
                            if !call_id.is_empty() {
                                st.tools.start(call_id, ts, name);
                            }
                        }
                        // `custom_tool_call_output` 一并接进来（**有意放宽任务书原文**，见报告
                        // 「偏差申报」）：本机 2,809 条 `custom_tool_call` 的回执**全部**是
                        // `custom_tool_call_output`（call_id 配对 2,809/2,809 = 100%），
                        // 只认 `function_call_output` 会让 41,412 次调用里 2,809 次（6.8%）
                        // 有次数无耗时（"可得却漏采"）。
                        "function_call_output" | "custom_tool_call_output" => {
                            let call_id = payload
                                .get("call_id")
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                            if let Some((dur, name)) = st.tools.finish(call_id, ts) {
                                let d = b.detail(key.clone(), ts);
                                d.counters.tool_ms += dur;
                                d.counters.tool_stats.entry(name).or_insert((0, 0)).1 += dur;
                            }
                            // 非零退出 → 工具失败（本机 1,897 条）。**只认任务书给的文案**
                            // （`function_call_output`）；`custom_tool_call_output` 的失败文案是
                            // `Exit code: N`，另立判据属新增口径，不在本任务范围。
                            if ptype == "function_call_output" {
                                let out =
                                    payload.get("output").and_then(|v| v.as_str()).unwrap_or("");
                                if out.contains("Process exited with code")
                                    && !out.contains("code 0")
                                {
                                    b.detail(key, ts).counters.error_tool += 1;
                                }
                            }
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
        b.parsed_files += 1;
        st.adjacent_last = adjacent.last();
        let mut next = read.next;
        next.state_json = serde_json::to_string(&st).unwrap_or_default();
        b.push_cursor(next);
        Ok(())
    }
}

// ==== usage 自省锁：以下为排除区（测试代码），勿删勿复制 ====
#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::usage::collect::{CollectContext, UsageCollector};
    use serde_json::json;
    use std::collections::HashMap;

    fn rollout(
        dir: &std::path::Path,
        name: &str,
        lines: Vec<serde_json::Value>,
    ) -> std::path::PathBuf {
        let d = dir.join(".codex/sessions/2026/10/03");
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join(name);
        let body: String = lines
            .iter()
            .map(|l| format!("{}\n", serde_json::to_string(l).unwrap()))
            .collect();
        std::fs::write(&p, body).unwrap();
        p
    }

    /// 追加一行（**不截断**）：跨轮用例用它模拟「文件在会话生命周期内持续追加」
    /// （实测同一 rollout 可被追写 23.3 天）。
    fn append_line(path: &std::path::Path, v: serde_json::Value) {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        writeln!(f, "{}", serde_json::to_string(&v).unwrap()).unwrap();
    }

    fn meta(id: &str, cwd: &str, originator: &str, subagent: bool) -> serde_json::Value {
        let source = if subagent {
            json!({"subagent":{"thread_spawn":{"parent_thread_id":"parent-1","depth":1}}})
        } else {
            json!({"cli":{"origin":"user"}})
        };
        json!({"timestamp":"2026-10-03T09:00:00Z","type":"session_meta",
               "payload":{"id":id,"cwd":cwd,"originator":originator,"source":source,"thread_source": if subagent {"subagent"} else {"user"}}})
    }

    fn token_count(input: i64, cached: i64, write: i64, out: i64, total: i64) -> serde_json::Value {
        json!({"timestamp":"2026-10-03T09:00:10Z","type":"event_msg","payload":{"type":"token_count",
            "info":{"total_token_usage":{"total_tokens":total},
                    "last_token_usage":{"input_tokens":input,"cached_input_tokens":cached,
                        "cache_write_input_tokens":write,"output_tokens":out,"total_tokens":total}}}})
    }

    /// 毫秒时间戳 → RFC3339（毫秒精度、UTC `Z`）：供 `ts_ms_of` 原样解析。
    fn iso_ms(ms: i64) -> String {
        chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms)
            .unwrap()
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    }

    /// 指定时间戳的 `token_count`（上面那个助手的时间戳是写死的，跨轮用例要自己给时间）
    fn token_count_at(
        ts: &str,
        input: i64,
        cached: i64,
        write: i64,
        out: i64,
        total: i64,
    ) -> serde_json::Value {
        let mut v = token_count(input, cached, write, out, total);
        v["timestamp"] = json!(ts);
        v
    }

    #[test]
    fn two_trees_are_both_scanned_and_adjacent_replays_dropped() {
        let dir = tempfile::tempdir().unwrap();
        // sessions 树：包含型（total == input + output）
        rollout(
            dir.path(),
            "rollout-a.jsonl",
            vec![
                meta("sess-a", "/p/A", "codex-tui", false),
                json!({"timestamp":"2026-10-03T09:00:01Z","type":"turn_context","payload":{"model":"gpt-5-codex","timezone":"Asia/Shanghai"}}),
                json!({"timestamp":"2026-10-03T09:00:02Z","type":"event_msg","payload":{"type":"task_started","turn_id":"t1"}}),
                token_count(1000, 800, 0, 200, 1200),
                token_count(1000, 800, 0, 200, 1200), // 相邻重放（本机 1,945 条）
                json!({"timestamp":"2026-10-03T09:00:20Z","type":"event_msg","payload":{"type":"task_complete","turn_id":"t1","duration_ms":18000}}),
                json!({"timestamp":"2026-10-03T09:00:21Z","type":"event_msg","payload":{"type":"task_complete","turn_id":"t1","duration_ms":18000}}), // 同 turn 重复
            ],
        );
        // archived_sessions 树（命中率仅 51.5%，漏掉会失真）
        let arch = dir.path().join(".codex/archived_sessions");
        std::fs::create_dir_all(&arch).unwrap();
        std::fs::write(
            arch.join("rollout-old.jsonl"),
            format!(
                "{}\n{}\n",
                serde_json::to_string(&meta("sess-b", "/p/B", "Codex Desktop", false)).unwrap(),
                serde_json::to_string(&token_count(500, 0, 0, 100, 600)).unwrap()
            ),
        )
        .unwrap();

        // 空游标表必须先绑成变量：`&HashMap::new()` 是**临时值**，借用进 `ctx` 就是
        // E0716（temporary value dropped while borrowed）——语句结束即释放。
        let no_cursors = HashMap::new();
        let ctx = CollectContext::new(dir.path(), 1_700_000_000_000, &no_cursors);
        let delta = CodexCollector.collect(&ctx).unwrap();
        assert_eq!(delta.parsed_files, 2, "两棵树都必须扫");
        assert_eq!(ctx.stats().repeat_reads, 0, "一遍扫描纪律");
        // 相邻重放丢一条 → 只算一次请求
        assert_eq!(delta.details.iter().map(|d| d.requests).sum::<i64>(), 2);
        let a = delta
            .details
            .iter()
            .find(|d| d.key.session_id == "sess-a")
            .unwrap();
        assert!(matches!(
            a.cache_semantics,
            crate::services::usage::semantics::CacheSemantics::Subset
        ));
        assert_eq!(a.request_total, 1000, "Subset：请求输入 = input 原文");
        assert_eq!(
            a.buckets.input_fresh, 200,
            "Subset：未缓存输入 = input - cache_read"
        );
        assert_eq!(a.counters.turns, 1, "task_started 计数");
        assert_eq!(
            a.counters.turn_ms,
            vec![18_000],
            "同 turn 多条 task_complete 必须去重"
        );
        assert_eq!(a.key.model, "gpt-5-codex", "模型来自前序 turn_context 继承");
        // 会话维度：originator 留档（§5.2 CLI/APP 拆分的唯一依据）
        let sa = delta
            .sessions
            .iter()
            .find(|s| s.session_id == "sess-a")
            .unwrap();
        assert_eq!(sa.originator.as_deref(), Some("codex-tui"));
        assert_eq!(sa.project_key, "a");
        let sb = delta
            .sessions
            .iter()
            .find(|s| s.session_id == "sess-b")
            .unwrap();
        assert_eq!(sb.originator.as_deref(), Some("Codex Desktop"));
    }

    #[test]
    fn stub_events_are_dropped_and_subagent_threads_are_layered() {
        let dir = tempfile::tempdir().unwrap();
        rollout(
            dir.path(),
            "rollout-c.jsonl",
            vec![
                meta("parent", "/p/C", "Codex Desktop", false),
                // stub：四桶全 0 而 total 非 0（本机 840 条 / 86 文件）→ 必须剔除
                token_count(0, 0, 0, 0, 999),
                // 同一 rollout 内嵌子代理线程（本机 25 个文件含 >1 个 session_meta.id）
                meta("child", "/p/C", "Codex Desktop", true),
                json!({"timestamp":"2026-10-03T09:01:00Z","type":"turn_context","payload":{"model":"gpt-5-codex","timezone":"Asia/Shanghai"}}),
                token_count(100, 0, 0, 50, 150),
                json!({"timestamp":"2026-10-03T09:01:30Z","type":"response_item","payload":{"type":"function_call","name":"shell","call_id":"c1"}}),
                json!({"timestamp":"2026-10-03T09:01:31Z","type":"response_item","payload":{"type":"function_call_output","call_id":"c1","output":"Process exited with code 1"}}),
            ],
        );
        // 空游标表必须先绑成变量：`&HashMap::new()` 是**临时值**，借用进 `ctx` 就是
        // E0716（temporary value dropped while borrowed）——语句结束即释放。
        let no_cursors = HashMap::new();
        let ctx = CollectContext::new(dir.path(), 1_700_000_000_000, &no_cursors);
        let delta = CodexCollector.collect(&ctx).unwrap();
        assert_eq!(
            delta.details.iter().map(|d| d.requests).sum::<i64>(),
            1,
            "stub 不得计入请求；子代理线程的用量另计一条"
        );
        let parent = delta
            .sessions
            .iter()
            .find(|s| s.session_id == "parent")
            .unwrap();
        let child = delta
            .sessions
            .iter()
            .find(|s| s.session_id == "child")
            .unwrap();
        assert!(!parent.is_subagent);
        assert!(child.is_subagent, "thread_spawn 存在 → 子代理线程");
        assert_eq!(child.parent_session_id.as_deref(), Some("parent-1"));
        let c = delta
            .details
            .iter()
            .find(|d| d.key.session_id == "child")
            .unwrap();
        assert_eq!(c.counters.tool_calls, 1);
        assert_eq!(c.counters.error_tool, 1, "非零退出计工具失败");
        assert_eq!(c.counters.tool_ms, 1000, "call_id 配对（96.9%）");
    }

    #[test]
    fn turn_aborted_interrupted_is_counted_separately_from_errors() {
        let dir = tempfile::tempdir().unwrap();
        rollout(
            dir.path(),
            "rollout-d.jsonl",
            vec![
                meta("sess-d", "/p/D", "codex-tui", false),
                json!({"timestamp":"2026-10-03T09:02:00Z","type":"event_msg","payload":{"type":"error","message":"stream failed"}}),
                json!({"timestamp":"2026-10-03T09:02:30Z","type":"event_msg","payload":{"type":"turn_aborted","turn_id":"t9","reason":"interrupted"}}),
                json!({"timestamp":"2026-10-03T09:03:00Z","type":"event_msg","payload":{"type":"turn_aborted","turn_id":"t10","reason":"error"}}),
            ],
        );
        // 空游标表必须先绑成变量：`&HashMap::new()` 是**临时值**，借用进 `ctx` 就是
        // E0716（temporary value dropped while borrowed）——语句结束即释放。
        let no_cursors = HashMap::new();
        let ctx = CollectContext::new(dir.path(), 1_700_000_000_000, &no_cursors);
        let delta = CodexCollector.collect(&ctx).unwrap();
        let d = &delta.details[0];
        assert_eq!(d.counters.error_model, 1, "event_msg.error → 模型/传输层");
        assert_eq!(
            d.counters.interrupted, 1,
            "reason=interrupted 单独一列（本机 280 条全是用户中断）"
        );
        assert_eq!(d.counters.error_turn, 1, "reason=error → 回合失败层");
    }

    /// **B1 回归**：stub 剔除 / 相邻重放丢弃 三处必须 `continue`，**绝不能 `return`**。
    /// 行序按真机形态构造：第 2 行是 stub（四桶全 0 而 total 非 0，本机 840 条 / 86 文件）、
    /// 第 5 行是相邻重放（本机 1,945 条 / 114 文件）。旧实现（`else { return }`）在第 2 行就把
    /// **整个文件**的扫描结束掉：task_complete / function_call / function_call_output 永不入账，
    /// 而且连文件末尾的 `b.push_cursor(next)` 都跳过 → 游标永不落库 → 下一轮整文件重读重算
    /// （明细是累加语义 → 账目按轮次线性膨胀）。
    #[test]
    fn adjacent_replay_and_stub_do_not_truncate_the_scan() {
        let dir = tempfile::tempdir().unwrap();
        rollout(
            dir.path(),
            "rollout-e.jsonl",
            vec![
                meta("sess-e", "/p/E", "codex-tui", false),
                token_count(0, 0, 0, 0, 999), // ① 第 2 行：stub → 只跳过这一行
                json!({"timestamp":"2026-10-03T09:00:02Z","type":"event_msg","payload":{"type":"task_started","turn_id":"t1"}}),
                token_count(1000, 800, 0, 200, 1200), // 正常一行
                token_count(1000, 800, 0, 200, 1200), // ② 第 5 行：相邻重放 → 只跳过这一行
                json!({"timestamp":"2026-10-03T09:00:20Z","type":"event_msg","payload":{"type":"task_complete","turn_id":"t1","duration_ms":18000}}),
                json!({"timestamp":"2026-10-03T09:00:30Z","type":"response_item","payload":{"type":"function_call","name":"shell","call_id":"c1"}}),
                json!({"timestamp":"2026-10-03T09:00:31Z","type":"response_item","payload":{"type":"function_call_output","call_id":"c1","output":"ok"}}),
            ],
        );
        // 空游标表必须先绑成变量：`&HashMap::new()` 是**临时值**，借用进 `ctx` 就是
        // E0716（temporary value dropped while borrowed）——语句结束即释放。
        let no_cursors = HashMap::new();
        let ctx = CollectContext::new(dir.path(), 1_700_000_000_000, &no_cursors);
        let delta = CodexCollector.collect(&ctx).unwrap();
        let d = delta
            .details
            .iter()
            .find(|d| d.key.session_id == "sess-e")
            .unwrap();
        assert_eq!(d.requests, 1, "stub 与相邻重放都不得计入请求");
        assert_eq!(
            d.counters.turns, 1,
            "第 3 行 task_started 必须仍被处理（旧实现拿 0）"
        );
        assert_eq!(
            d.counters.turn_ms,
            vec![18_000],
            "第 6 行 task_complete 必须仍被处理（旧实现拿 vec![]）"
        );
        assert_eq!(
            d.counters.tool_calls, 1,
            "第 7 行工具发起必须仍被处理（旧实现拿 0）"
        );
        assert_eq!(
            d.counters.tool_ms, 1000,
            "第 8 行回执配对必须仍被处理（旧实现拿 0）"
        );
        // 游标必须落库：`return` 会跳过 push_cursor → 下一轮整文件重算
        assert_eq!(
            delta.cursors.len(),
            1,
            "含 stub/重放的文件同样必须产出 1 条游标"
        );
        // 第二轮（回放同一份游标）不得重复计入
        let cursors: HashMap<_, _> = delta
            .cursors
            .iter()
            .map(|c| (c.session_id.clone(), c.clone()))
            .collect();
        let ctx2 = CollectContext::new(dir.path(), 1_700_000_000_000 + 60_000, &cursors);
        let d2 = CodexCollector.collect(&ctx2).unwrap();
        assert_eq!(
            d2.new_records, 0,
            "第二轮没有新增行（游标已落库 → 从文件末尾续读）"
        );
        assert_eq!(
            d2.details.iter().map(|d| d.requests).sum::<i64>(),
            0,
            "不得重复计入用量"
        );
    }

    /// **B7 回归**：`mcp_tool_call_end` 是 **`event_msg`**、`duration` 是**对象** `{secs,nanos}`、
    /// 工具名在 `invocation.tool`。旧实现把它写在 `response_item` 分支且按整数读 `duration`
    /// → 本机 **1,406 条**原生 MCP 耗时全部丢失（且 UI 因 `caps.tool_calls = true` 以为这是完整值）。
    /// fixture 用逐字真实形态（`research/可得性探测-文件类三源.md:171`）。
    ///
    /// **口径（§3.2.2 次要 8）**：原生 MCP 调用**同时计入 `tool_calls` 与 `tool_stats[name].0`**
    /// ——"有耗时却没有次数"的工具条目会进 `top_tool_ms`（Task 18），在 UI 上显示成
    /// "0 次调用却有 3517 ms"的鬼条目；次数与耗时必须同源。
    #[test]
    fn native_mcp_duration_is_counted() {
        let dir = tempfile::tempdir().unwrap();
        rollout(
            dir.path(),
            "rollout-m.jsonl",
            vec![
                meta("sess-m", "/p/M", "codex-tui", false),
                json!({"timestamp":"2026-10-03T09:00:01Z","type":"turn_context","payload":{"model":"gpt-5-codex","timezone":"Asia/Shanghai"}}),
                // 逐字真实样例：event_msg + invocation.tool + duration 对象
                json!({"timestamp":"2026-10-03T09:00:02Z","type":"event_msg","payload":{
                "type":"mcp_tool_call_end","call_id":"call_ToYyRnIx4wIomdPS31B3N1Cr",
                "invocation":{"server":"computer-use","tool":"list_apps"},
                "duration":{"secs":3,"nanos":517177833}}}),
                json!({"timestamp":"2026-10-03T09:00:03Z","type":"event_msg","payload":{
                "type":"mcp_tool_call_end","call_id":"call_2",
                "invocation":{"server":"computer-use","tool":"screenshot"},
                "duration":{"secs":0,"nanos":8000000}}}),
            ],
        );
        // 空游标表必须先绑成变量：`&HashMap::new()` 是**临时值**，借用进 `ctx` 就是
        // E0716（temporary value dropped while borrowed）——语句结束即释放。
        let no_cursors = HashMap::new();
        let ctx = CollectContext::new(dir.path(), 1_700_000_000_000, &no_cursors);
        let delta = CodexCollector.collect(&ctx).unwrap();
        let d = delta
            .details
            .iter()
            .find(|d| d.key.session_id == "sess-m")
            .unwrap();
        assert_eq!(
            d.counters.tool_ms,
            3517 + 8,
            "secs*1000 + nanos/1e6（旧实现恒 0）"
        );
        assert_eq!(
            d.counters.tool_calls, 2,
            "原生 MCP 调用必须计入次数（条目自洽）"
        );
        assert_eq!(
            d.counters.tool_stats.get("list_apps").copied(),
            Some((1, 3517))
        );
        assert_eq!(
            d.counters.tool_stats.get("screenshot").copied(),
            Some((1, 8))
        );
        assert_eq!(
            d.counters.tool_stats.values().map(|(n, _)| *n).sum::<i64>(),
            d.counters.tool_calls,
            "tool_stats 的次数之和必须等于 tool_calls（防「0 次调用却有耗时」的条目）"
        );
        assert!(
            !d.counters.tool_stats.contains_key("mcp"),
            "工具名取 invocation.tool，不是缺省值"
        );
    }

    /// **B7 后半条 + W-36（评审裁决 C）**：判据是「**字段在不在**」，不是「值等不等于 0」。
    ///
    /// * `duration` **字段缺失** → 不可得 → 整条丢弃（GC 7：不可得即不填 0）；
    /// * `duration: {"secs":0,"nanos":0}` → **在场的真数据**（一次快到测不出毫秒的调用）
    ///   → **次数照记、耗时记 0**（本机 1,406 条原生 MCP 里有 **2 条**正是这个形态；
    ///   旧实现把它们当「缺失」丢掉，是「把在场值误当缺失」——GC 7 的另一面）。
    ///
    /// 两种形态都不得让 `tool_stats` 的次数之和 ≠ `tool_calls`。
    #[test]
    fn mcp_present_zero_duration_is_counted_but_absent_field_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        rollout(
            dir.path(),
            "rollout-m3.jsonl",
            vec![
                meta("sess-m3", "/p/M", "codex-tui", false),
                json!({"timestamp":"2026-10-03T09:00:01Z","type":"turn_context","payload":{"model":"gpt-5-codex"}}),
                // ① 完全没有 duration 字段 → 不可得 → 整条丢弃
                json!({"timestamp":"2026-10-03T09:00:02Z","type":"event_msg","payload":{
                "type":"mcp_tool_call_end","call_id":"c1",
                "invocation":{"server":"computer-use","tool":"list_apps"}}}),
                // ② duration 在场但全 0 → **真数据**：次数照记、耗时记 0（W-36）
                json!({"timestamp":"2026-10-03T09:00:03Z","type":"event_msg","payload":{
                "type":"mcp_tool_call_end","call_id":"c2",
                "invocation":{"server":"computer-use","tool":"screenshot"},
                "duration":{"secs":0,"nanos":0}}}),
                // ③ 正常一条：必须照常入账（防假绿：不是「整段被吃掉」）
                json!({"timestamp":"2026-10-03T09:00:04Z","type":"event_msg","payload":{
                "type":"mcp_tool_call_end","call_id":"c3",
                "invocation":{"server":"computer-use","tool":"click"},
                "duration":{"secs":1,"nanos":0}}}),
            ],
        );
        let no_cursors = HashMap::new();
        let ctx = CollectContext::new(dir.path(), 1_700_000_000_000, &no_cursors);
        let delta = CodexCollector.collect(&ctx).unwrap();
        let d = delta
            .details
            .iter()
            .find(|d| d.key.session_id == "sess-m3")
            .unwrap();
        assert_eq!(
            d.counters.tool_calls, 2,
            "只有「字段缺失」的那条丢弃；在场全 0 的那条必须计入次数"
        );
        assert_eq!(
            d.counters.tool_ms, 1000,
            "在场全 0 记 0 ms，只有第 ③ 条贡献 1000"
        );
        assert!(
            !d.counters.tool_stats.contains_key("list_apps"),
            "缺字段的那条整条丢弃（不记 0、也不进 tool_stats）"
        );
        assert_eq!(
            d.counters.tool_stats.get("screenshot").copied(),
            Some((1, 0)),
            "在场全 0 → 条目 (1, 0)：有次数、耗时 0（不是「0 次调用却有耗时」）"
        );
        assert_eq!(d.counters.tool_stats.get("click").copied(), Some((1, 1000)));
        assert_eq!(
            d.counters.tool_stats.values().map(|(n, _)| *n).sum::<i64>(),
            d.counters.tool_calls,
            "W-36 修完仍必须满足既有不变量：tool_stats 次数之和 == tool_calls"
        );
    }

    /// **B1 的第三处 `continue`**：`last_token_usage` 缺失（本机 `info == null` 65 条）
    /// 同样只能跳过**这一行**——用 `return` 会让文件后半段的 turn / 工具 / 回执全部丢失，
    /// 且跳过游标写入（每轮重算）。
    #[test]
    fn token_count_without_last_token_usage_does_not_truncate_the_scan() {
        let dir = tempfile::tempdir().unwrap();
        rollout(
            dir.path(),
            "rollout-n.jsonl",
            vec![
                meta("sess-n", "/p/N", "codex-tui", false),
                json!({"timestamp":"2026-10-03T09:00:01Z","type":"turn_context","payload":{"model":"gpt-5-codex"}}),
                // info == null（真实形态：65 条）
                json!({"timestamp":"2026-10-03T09:00:02Z","type":"event_msg","payload":{"type":"token_count","info":null}}),
                token_count(1000, 800, 0, 200, 1200),
                json!({"timestamp":"2026-10-03T09:00:20Z","type":"event_msg","payload":{"type":"task_started","turn_id":"t1"}}),
                json!({"timestamp":"2026-10-03T09:00:30Z","type":"response_item","payload":{"type":"function_call","name":"shell","call_id":"c1"}}),
                json!({"timestamp":"2026-10-03T09:00:31Z","type":"response_item","payload":{"type":"function_call_output","call_id":"c1","output":"ok"}}),
            ],
        );
        let no_cursors = HashMap::new();
        let ctx = CollectContext::new(dir.path(), 1_700_000_000_000, &no_cursors);
        let delta = CodexCollector.collect(&ctx).unwrap();
        let d = delta
            .details
            .iter()
            .find(|d| d.key.session_id == "sess-n")
            .unwrap();
        assert_eq!(d.requests, 1, "缺 last_token_usage 的那条不得计入请求");
        assert_eq!(
            d.counters.turns, 1,
            "后续 task_started 必须仍被处理（旧实现拿 0）"
        );
        assert_eq!(
            d.counters.tool_calls, 1,
            "后续工具发起必须仍被处理（旧实现拿 0）"
        );
        assert_eq!(
            d.counters.tool_ms, 1000,
            "后续回执配对必须仍被处理（旧实现拿 0）"
        );
        assert_eq!(
            delta.cursors.len(),
            1,
            "游标必须照常落库（return 会跳过 push_cursor）"
        );
    }

    /// **B5 + 两棵树**：游标键 = **各自树根**下的相对路径（分隔符归一为 `:`），
    /// **不是文件名 stem**、也不是「相对 `~/.codex` 的路径」。
    /// 三处任一写错都会让两棵树 / 同名 rollout 撞键 → `usage_cursor` 后写覆盖先写 →
    /// 其余文件每轮尾指纹不符 → 整文件重读重算并累加（账目按轮次线性膨胀）。
    #[test]
    fn cursor_keys_of_the_two_trees_are_relative_to_each_root() {
        let dir = tempfile::tempdir().unwrap();
        rollout(
            dir.path(),
            "rollout-a.jsonl",
            vec![
                meta("sess-a", "/p/A", "codex-tui", false),
                token_count(10, 0, 0, 5, 15),
            ],
        );
        let arch = dir.path().join(".codex/archived_sessions");
        std::fs::create_dir_all(&arch).unwrap();
        std::fs::write(
            arch.join("rollout-a.jsonl"),
            format!(
                "{}\n{}\n",
                serde_json::to_string(&meta("sess-b", "/p/B", "codex-tui", false)).unwrap(),
                serde_json::to_string(&token_count(20, 0, 0, 7, 27)).unwrap()
            ),
        )
        .unwrap();
        let no_cursors = HashMap::new();
        let ctx = CollectContext::new(dir.path(), 1_700_000_000_000, &no_cursors);
        let delta = CodexCollector.collect(&ctx).unwrap();
        let mut keys: Vec<String> = delta.cursors.iter().map(|c| c.session_id.clone()).collect();
        keys.sort();
        assert_eq!(
            keys,
            vec![
                "2026:10:03:rollout-a.jsonl".to_string(),
                "rollout-a.jsonl".to_string(),
            ],
            "两棵树各自用各自的根：同名 rollout 也必须得到两个不同的键（stem 键会让它们撞成一行）"
        );
        assert_eq!(delta.cursors.len(), 2, "两个文件 = 两条游标");
    }

    /// **W-12 回归（跨轮相邻重放）**：`AdjacentDrop` 是单轮实例 → 必须把上一轮的「最后一条四桶」
    /// 种回比较器（`seed` / `last`），否则**跨轮边界的那一对相邻重放**会漏判（静默多算）。
    /// 本用例同时钉住「只丢相邻的那一条」：紧随其后**不同**的四桶必须照常计入。
    #[test]
    fn cross_round_adjacent_replay_is_dropped_by_the_seeded_comparator() {
        let dir = tempfile::tempdir().unwrap();
        let p = rollout(
            dir.path(),
            "rollout-r.jsonl",
            vec![
                meta("sess-r", "/p/R", "codex-tui", false),
                json!({"timestamp":"2026-10-03T09:00:01Z","type":"turn_context","payload":{"model":"gpt-5-codex"}}),
                token_count(1000, 800, 0, 200, 1200),
            ],
        );
        let no_cursors = HashMap::new();
        let ctx = CollectContext::new(dir.path(), 1_700_000_000_000, &no_cursors);
        let d1 = CodexCollector.collect(&ctx).unwrap();
        assert_eq!(
            d1.details.iter().map(|d| d.requests).sum::<i64>(),
            1,
            "前提：第一轮计入一次请求（否则第二轮无从对照）"
        );
        let cursors: HashMap<_, _> = d1
            .cursors
            .iter()
            .map(|c| (c.session_id.clone(), c.clone()))
            .collect();
        // 第二轮追加：① 上一轮**末条**的四桶重发（跨轮那一对相邻重放）② 一条不同的四桶
        append_line(&p, token_count(1000, 800, 0, 200, 1200));
        append_line(&p, token_count(1000, 900, 0, 200, 1200));
        let ctx2 = CollectContext::new(dir.path(), 1_700_000_000_000 + 60_000, &cursors);
        let d2 = CodexCollector.collect(&ctx2).unwrap();
        assert_eq!(
            d2.new_records, 2,
            "前提：两条追加行都被读入（否则本用例假绿）"
        );
        let r2: i64 = d2
            .details
            .iter()
            .filter(|d| d.key.session_id == "sess-r")
            .map(|d| d.requests)
            .sum();
        assert_eq!(
            r2, 1,
            "跨轮边界的相邻重放必须被种入的末条抓住；不同四桶必须照常计入（未 seed 的实现这里拿 2）"
        );
        assert_eq!(
            d2.details
                .iter()
                .find(|d| d.key.session_id == "sess-r")
                .unwrap()
                .buckets
                .cache_read,
            900,
            "存活下来的必须是**不同**的那一条（证明不是把两条都丢了）"
        );
    }

    /// **跨轮状态持久化的回归锁（评审 Important #2 / 裁决 A）**。
    ///
    /// `state_json` 承载 `session_id` / `cwd` / `model` / `tz_name` / `adjacent_last` / `tools` /
    /// `turn_seen` 七项，而原有的跨轮用例**只追加 `token_count` 行** → 只隐式锁住 `session_id`、
    /// 显式锁住 `adjacent_last`；**`model` / `tz_name` / `tools` / `turn_seen` 四项零覆盖**。
    /// 这四项丢了都不编译报错、只**静默错数**（任务书 §5.3#5「跨轮续读状态里保住」那半句原本没有锁）：
    ///
    /// * `model` 掉了 → 第二轮新行落 `model=""`，而 **model 是明细行键的一部分**（GC 15）
    ///   → 同一小时的用量被**裂成两行**；
    /// * `tz_name` 掉了 → 「各源自带时区**可读入备查**」（spec §P6 / Q-4 裁决）落空：
    ///   键不受它影响（A-1 锁已单独钉死），所以这里改为**直接断言 `state_json` 里跨轮仍在**；
    /// * `tools` 掉了 → 跨轮边界那条**在飞的调用**静默丢 `tool_ms`（并留下「有次数无耗时」的条目）；
    /// * `turn_seen` 掉了 → 跨轮同 `turn_id` 的 `task_complete` **重复计入一次时长**。
    ///
    /// 本用例与上面那条 `cross_round_adjacent_replay_is_dropped_by_the_seeded_comparator` **分开**：
    /// 那条是 W-12 的**单一**锁（`seed` 缺失时必须只有它红），本用例是四字段的联合锁。
    /// （裁决原文建议「加在既有跨轮用例里」；我拆成独立用例以便失败点可归因，见 fix report 偏差 F-A。）
    ///
    /// **A-1 追记（Q-4 裁决）**：本用例原先靠「`hour_key` 漂到源时区桶」来间接观测 `tz_name`
    /// 持久化，并会**主动挑一个与宿主不同小时的时区**——键改回宿主本地后那条判据不可能成立，
    /// 故改为：① `hour_key` 一律断言宿主本地桶；② `tz_name` 改为直接读 `state_json` 断言。
    /// 覆盖面（model / tz_name / tools / turn_seen 四项）一项未减。
    #[test]
    fn cross_round_state_carries_model_timezone_and_pending_tools() {
        let dir = tempfile::tempdir().unwrap();
        // 三个小时桶：H1（第一轮主行）/ H2（工具发起在 H2、回执在第二轮也落 H2）/
        // H3（第二轮新增的那条 token_count → **必是新行**，用来验 model / tz_name 的继承）
        let ts_r2: i64 = 1_700_000_000_000;
        let ts_r1 = ts_r2 - 3_600_000;
        let ts3 = ts_r2 + 3_600_000;
        // **A-1**：源时区固定取与宿主本地偏移最大的候选之一（UTC+14），用来证明
        // 「时区在场也不影响键」——若实现里还留着按源时区算键的血脉，本用例会当场红。
        let tz_name = "Pacific/Kiritimati";
        let hour_host3 = hour_key_of_host(ts3);
        // 前提：工具发起（ts_r2 - 10 s）与回执（ts_r2）必须同桶——否则「次数」与「耗时」会落两行
        assert_eq!(
            hour_key_of_host(ts_r2 - 10_000),
            hour_key_of_host(ts_r2),
            "前提：跨轮配对的这一对必须同小时桶"
        );
        let r1 = iso_ms(ts_r1);
        let r2 = iso_ms(ts_r2);
        let r3 = iso_ms(ts3);

        let p = rollout(
            dir.path(),
            "rollout-x.jsonl",
            vec![
                meta("sess-x", "/p/X", "codex-tui", false),
                json!({"timestamp": r1, "type":"turn_context",
                       "payload":{"model":"gpt-5-codex","timezone": tz_name}}),
                json!({"timestamp": r1, "type":"event_msg","payload":{"type":"task_started","turn_id":"t1"}}),
                token_count_at(&r1, 1000, 800, 0, 200, 1200),
                json!({"timestamp": r1, "type":"event_msg",
                       "payload":{"type":"task_complete","turn_id":"t1","duration_ms":18000}}),
                // 第一轮：一个**发起了但还没回执**的工具调用 —— 配对状态必须活到第二轮
                json!({"timestamp": iso_ms(ts_r2 - 10_000), "type":"response_item",
                       "payload":{"type":"function_call","name":"shell","call_id":"c9"}}),
            ],
        );
        // 供 `provider_of` 判定「模型是否真的继承下来了」：模型丢了 → 规则不命中 → provider 落空串
        let rules = crate::services::usage::provider::parse_provider_rules(
            r#"{"rules":[{"prefix":"gpt-","provider":"openai"}]}"#,
        );
        let no_cursors = HashMap::new();
        let ctx =
            CollectContext::new(dir.path(), ts_r1, &no_cursors).with_provider_rules(rules.clone());
        let d1 = CodexCollector.collect(&ctx).unwrap();
        assert_eq!(d1.details.len(), 2, "前提：H1 主行 + H2 工具发起行");
        let h1row = d1
            .details
            .iter()
            .find(|d| d.key.hour_key == hour_key_of_host(ts_r1))
            .unwrap();
        assert_eq!(h1row.key.model, "gpt-5-codex");
        assert_eq!(h1row.key.provider, "openai", "前提：供应商规则真的命中");
        let h2row = d1
            .details
            .iter()
            .find(|d| d.key.hour_key == hour_key_of_host(ts_r2))
            .unwrap();
        assert_eq!(
            h2row.counters.tool_calls, 1,
            "前提：第一轮记下这次调用（回执在第二轮）"
        );
        assert_eq!(h2row.counters.tool_ms, 0, "前提：第一轮还没有回执");
        let cursors: HashMap<_, _> = d1
            .cursors
            .iter()
            .map(|c| (c.session_id.clone(), c.clone()))
            .collect();

        // ---- 第二轮：三行，各自钉一个字段 ----
        append_line(
            &p,
            json!({"timestamp": r2, "type":"response_item",
            "payload":{"type":"function_call_output","call_id":"c9","output":"ok"}}),
        ); // tools
        append_line(&p, token_count_at(&r3, 500, 100, 0, 50, 550)); // model + tz_name
        append_line(
            &p,
            json!({"timestamp": r3, "type":"event_msg",
            "payload":{"type":"task_complete","turn_id":"t1","duration_ms":18000}}),
        ); // turn_seen
        let ctx2 = CollectContext::new(dir.path(), ts3, &cursors).with_provider_rules(rules);
        let d2 = CodexCollector.collect(&ctx2).unwrap();
        assert_eq!(
            d2.new_records, 3,
            "前提：三条追加行都被读入（否则本用例假绿）"
        );

        // ⓪ `tz_name`（A-1 追记）：键不看它，但**备查**必须跨轮仍在 `state_json` 里
        let st2 = d2
            .cursors
            .iter()
            .map(|c| serde_json::from_str::<serde_json::Value>(&c.state_json).unwrap())
            .find(|s| s["session_id"] == "sess-x")
            .expect("第二轮必须带回 sess-x 的续读状态");
        assert_eq!(
            st2["tz_name"], tz_name,
            "各源自带时区必须跨轮持久化备查（Q-4 裁决：可读入备查、不参与键计算）"
        );

        // ① `model` + `tz_name`：第二轮新增的那条 token_count 落在**宿主本地**的 H3（A-1：
        //    源时区在场也不参与键），且模型继承下来
        let r3row = d2
            .details
            .iter()
            .find(|d| d.key.hour_key == hour_host3)
            .unwrap_or_else(|| {
                panic!(
                    "第二轮的新行必须落在**宿主本地**的小时桶 {hour_host3}（键改回源时区时会落别的桶）；实际行键：{:?}",
                    d2.details.iter().map(|d| d.key.hour_key.clone()).collect::<Vec<_>>()
                )
            });
        assert_eq!(
            r3row.key.model, "gpt-5-codex",
            "model 必须跨轮继承（丢了 → 新行落 model=\"\"，而 model 在明细行键里 → 同一小时用量裂成两行）"
        );
        assert_eq!(
            r3row.key.provider, "openai",
            "provider 随继承下来的模型一起解析（model 丢了这里就是空串/unknown）"
        );

        // ② `tools`：第一轮发起的调用 + 第二轮的回执 → **跨轮**配对（`tools` 未持久化时，
        //    第二轮的 `finish()` 配不上 → `tool_ms` 恒 0、`tool_stats` 里连条目都没有）
        let toolrow = d2
            .details
            .iter()
            .find(|d| d.counters.tool_stats.contains_key("shell"))
            .expect("回执必须配到第一轮那条在飞的调用（tools 未持久化时这里找不到条目）");
        assert_eq!(
            toolrow.counters.tool_ms, 10_000,
            "跨轮配对耗时 = 回执 − 发起（tools 未持久化时恒 0）"
        );
        // **单轮 delta 视角**：`tool_stats` 的「次数」记在第一轮那条 delta 上（同一行键），
        // 所以第二轮这条 delta 里是 `(0, 10_000)`——这不是 B7 那种「0 次调用却有耗时」的鬼条目，
        // 而是**跨轮配对**的必然中间形态：落库由 DAO 的 `tool_stats` **累加**合并
        // （W-03：`merge_samples` 是读改写，跨批/跨轮累加成立）→ 库里仍是 `(1, 10_000)`。
        // 这里把两轮 delta 逐项相加，钉住「合并后条目自洽」这条不变量。
        assert_eq!(
            h2row.counters.tool_stats.get("shell").copied(),
            Some((1, 0)),
            "前提：次数在第一轮那条 delta 上（耗时那一半在第二轮）"
        );
        let s = toolrow
            .counters
            .tool_stats
            .get("shell")
            .copied()
            .unwrap_or((0, 0));
        assert_eq!(
            (1 + s.0, s.1),
            (1, 10_000),
            "两轮 delta 合并后必须是 (1, 10_000)：次数与耗时同源（库里不得出现「0 次调用却有耗时」）"
        );

        // ③ `turn_seen`：跨轮同 turn_id 的 task_complete 不得再计一次时长。
        //    同样按**两轮 delta 之和**看（样本本身是第一轮记的）。
        let s1: usize = d1.details.iter().map(|d| d.counters.turn_ms.len()).sum();
        let s2: usize = d2.details.iter().map(|d| d.counters.turn_ms.len()).sum();
        assert_eq!(
            (s1, s2),
            (1, 0),
            "第一轮记过 t1 的时长；第二轮的重复 task_complete 必须被 turn_seen 挡住（未持久化时 s2 = 1 → 库里重复计一次）"
        );
        assert_eq!(
            d2.details.iter().map(|d| d.requests).sum::<i64>(),
            1,
            "第二轮只应有一条新请求（H3 那条 token_count）"
        );
    }

    /// **A-1 锁（Q-4 裁决 / W-14 的封棺钉）：落库键只由宿主本地算法产生。**
    ///
    /// 两个 rollout **除 `turn_context.timezone` 之外完全对称**，而这两个 IANA 名相差
    /// **26 小时**（`Pacific/Kiritimati` UTC+14 与 `Etc/GMT+12` UTC−12）⇒ 只要键里还剩
    /// 一丝「按源时区算」的血脉，同一时间戳上两行的 `hour_key` 就**在任何宿主上都绝不可能
    /// 相等**。这条判据**与宿主时区无关**，正是本用例不假绿的根据（不依赖"本机恰好不等于
    /// Kiritimati"，也不依赖 δ=0 的巧合）。
    ///
    /// 按裁决（spec §P6 改写后：一律以宿主本地时区为基准；**各源自带时区可读入备查但不得参与
    /// 键计算**）两行必须落**同一个** `hour_key`，且逐字 == `hour_key_of_host(ts)`。
    ///
    /// **能变红**：把 codex 改回 `SourceTz::Named(...)`（或让 `turn_context.timezone` 重新回到
    /// 键计算路径）→ 两键相差 26 小时 → 必红。
    #[test]
    fn detail_hour_key_is_host_local_even_when_source_timezone_is_present() {
        let dir = tempfile::tempdir().unwrap();
        let ts: i64 = 1_700_000_000_000; // 2023-11-14T22:13:20Z（两源时区相差 26h）
        let r = iso_ms(ts);
        // 除 timezone 外逐字段对称：同模型、同桶、同时间戳、同结构
        for (sid, tz_name) in [
            ("sess-tz-a", "Pacific/Kiritimati"), // UTC+14
            ("sess-tz-b", "Etc/GMT+12"),         // UTC−12
        ] {
            rollout(
                dir.path(),
                &format!("rollout-{sid}.jsonl"),
                vec![
                    meta(sid, "/p/TZ", "codex-tui", false),
                    json!({"timestamp": r, "type":"turn_context",
                           "payload":{"model":"gpt-5-codex","timezone": tz_name}}),
                    token_count_at(&r, 100, 0, 0, 10, 110),
                ],
            );
        }
        let no_cursors = HashMap::new();
        let ctx = CollectContext::new(dir.path(), ts, &no_cursors);
        let delta = CodexCollector.collect(&ctx).unwrap();
        assert_eq!(
            delta.details.len(),
            2,
            "前提：两个 rollout 各出一条明细行（否则本用例假绿）"
        );
        let row = |sid: &str| {
            delta
                .details
                .iter()
                .find(|d| d.key.session_id == sid)
                .unwrap_or_else(|| {
                    panic!(
                        "{sid} 必须有明细行；实际：{:?}",
                        delta
                            .details
                            .iter()
                            .map(|d| (d.key.session_id.clone(), d.key.hour_key.clone()))
                            .collect::<Vec<_>>()
                    )
                })
        };
        let (a, b) = (row("sess-tz-a"), row("sess-tz-b"));
        let host = hour_key_of_host(ts);
        assert_eq!(
            a.key.hour_key, host,
            "落库键必须**只由宿主本地算法**产生（源时区在场也不得参与键计算）"
        );
        assert_eq!(b.key.hour_key, host, "同上：另一源的时区也不得参与键计算");
        assert_eq!(
            a.key.hour_key, b.key.hour_key,
            "两个源时区相差 26 小时（UTC+14 vs UTC−12）⇒ 按源时区算键时两键必不相等；\
             按裁决两者必须落在同一个宿主本地桶（这是本用例与宿主时区无关的红/绿判据）"
        );
        assert_eq!(
            a.key.day_key,
            &host[..10],
            "日键 = 宿主本地小时键的日期前缀（两档不得分叉）"
        );
        // **备查**（裁决原文：各源自带时区「可读入备查」）：时区名仍必须被读进续读状态，
        // 只是**不得**回到键计算路径。这条与上面两条互为约束——防止后来者"修回去"时
        // 顺手把读取也删掉（那会把「可读入备查」也一起做没）。
        for (sid, tz_name) in [
            ("sess-tz-a", "Pacific/Kiritimati"),
            ("sess-tz-b", "Etc/GMT+12"),
        ] {
            let st = delta
                .cursors
                .iter()
                .map(|c| serde_json::from_str::<serde_json::Value>(&c.state_json).unwrap())
                .find(|s| s["session_id"] == sid)
                .unwrap_or_else(|| panic!("{sid} 必须有一条续读状态（state_json）"));
            assert_eq!(
                st["tz_name"], tz_name,
                "各源自带时区必须仍被读入备查（{sid}）"
            );
        }
        // 前提（**真实产物**，P2-4 / B-4）：两份夹具真的被**扫描**到了。
        // `parsed_files` 是采集器自己数的「本轮真读到的文件数」（`b.parsed_files += 1`），
        // 不是本用例 `push` 出来的局部清单长度——旧断言 `paths.len() == 2` 由循环**无条件 push
        // 两次**得到，**不可能失败**，证不了"被扫到"（真正的承担者一直是上面的
        // `delta.details.len() == 2`；这里补一条与扫描结果直接相关的产物断言）。
        assert_eq!(
            delta.parsed_files, 2,
            "前提：两份 rollout 夹具真的被扫到了（parsed_files 是采集器自己数的文件数）"
        );
    }

    /// **W-06 / D-24 回归（响亮失败，不得只 `log::warn!`）**：`stat_of` 在 mtime 不可得 /
    /// 早于 1970 时返回 `Err`（Task 9 的 M4），这类文件**每轮都会 Err**；若 `collect()` 只
    /// warn 后跳过，用户在 UI 上看到的是「这个源没有用量」，而不是「这个源读不了」。
    /// 用 `filetime` 把 mtime 打到 1969（跨平台、不依赖权限位）确定性地造出该错误。
    #[test]
    fn unreadable_file_fails_the_source_loudly_instead_of_silently_skipping() {
        let dir = tempfile::tempdir().unwrap();
        rollout(
            dir.path(),
            "rollout-good.jsonl",
            vec![
                meta("sess-good", "/p/G", "codex-tui", false),
                token_count(1000, 800, 0, 200, 1200),
            ],
        );
        let bad = rollout(
            dir.path(),
            "rollout-bad.jsonl",
            vec![
                meta("sess-bad", "/p/G", "codex-tui", false),
                token_count(1, 0, 0, 1, 2),
            ],
        );
        filetime::set_file_mtime(&bad, filetime::FileTime::from_unix_time(-1, 0)).unwrap();
        // 前提断言（防假绿）：这次改写**真的**让 stat_of 失败——否则本用例根本没走到那条分支。
        assert!(
            crate::services::usage::cursor::stat_of(&bad).is_err(),
            "前提不成立：mtime 没落到 1970 之前（平台夹取了负时间戳），本用例测不到该路径"
        );
        let no_cursors = HashMap::new();
        let ctx = CollectContext::new(dir.path(), 1_700_000_000_000, &no_cursors);
        let err = CodexCollector
            .collect(&ctx)
            .expect_err("mtime 不可得的文件必须让整源响亮失败（W-06），不得只 log::warn! 后跳过");
        assert_eq!(
            err.code, "usage-source-io",
            "错误码必须落进 UsageSourceStatus.errorCode"
        );
        assert!(
            err.detail.contains("rollout-bad.jsonl"),
            "错误信息要点名是哪个文件读不了：{}",
            err.detail
        );
    }

    /// **W-19 登记锁**：`SessionFileScan` 的 namespace 必须来自 `scan_namespace(sid)`
    /// （`"usage-" + db_id()`），不得手写字符串字面量——同 ns 混存不同类型会让
    /// `Arc::downcast` 失配、缓存静默失效（GC 5）。
    #[test]
    fn scan_namespace_is_derived_from_the_source_id() {
        assert_eq!(SCAN_NAMESPACE, scan_namespace(UsageSourceId::Codex));
        assert_eq!(SCAN_NAMESPACE, "usage-codex");
        assert_ne!(
            scan_namespace(UsageSourceId::Codex),
            scan_namespace(UsageSourceId::Claude),
            "七源 ns 互不相同（Task 17 会全表断言）"
        );
    }

    /// **R1 / 说明书 §4.3 回归**：codex 是「能读到用户文本」的三源之一（claude / codex / kimi）
    /// → 「用户输入(估)」必须出真值（既不是 `null`、也不是恒 0）。任务书 Step 3 的代码块
    /// **漏了这一支**，照抄会让 codex 的该指标在小时档与日档**永久空态**（见报告「偏差申报」）。
    /// 真实形态：`event_msg.payload.type == "user_message"`、文本在 `payload.message`
    /// （本机 2,426 条 / 2.24 M 字符）。
    #[test]
    fn codex_user_est_comes_from_user_message_events() {
        let dir = tempfile::tempdir().unwrap();
        rollout(
            dir.path(),
            "rollout-u.jsonl",
            vec![
                meta("sess-u", "/p/U", "codex-tui", false),
                json!({"timestamp":"2026-10-03T09:00:01Z","type":"turn_context","payload":{"model":"gpt-5-codex"}}),
                json!({"timestamp":"2026-10-03T09:00:02Z","type":"event_msg","payload":{"type":"user_message","message":"你好 world"}}),
                json!({"timestamp":"2026-10-03T09:00:03Z","type":"event_msg","payload":{"type":"user_message","message":"second"}}),
            ],
        );
        let no_cursors = HashMap::new();
        let ctx = CollectContext::new(dir.path(), 1_700_000_000_000, &no_cursors);
        let delta = CodexCollector.collect(&ctx).unwrap();
        let d = delta
            .details
            .iter()
            .find(|d| d.key.session_id == "sess-u")
            .unwrap();
        assert_eq!(
            d.user_est,
            Some(2 + 1 + 1),
            "2 个 CJK 字 + 1 个 ASCII 词 + 1 个 ASCII 词（与 claude 同公式、逐条累加）"
        );
        // 前提断言（防假绿）：这条明细行确实存在、且是 user_message 归到的那一行
        assert_eq!(d.key.model, "gpt-5-codex");
        assert_eq!(d.requests, 0, "user_message 不是用量记录（不产生请求）");
    }

    /// **`custom_tool_call_output` 配对**（相对任务书原文的一处放宽，见报告「偏差申报」）：
    /// 本机 2,809 条 `custom_tool_call`（`apply_patch` / `web_search` 等）的回执**全部**是
    /// `custom_tool_call_output`，`call_id` 配对 2,809/2,809 = 100%。只认 `function_call_output`
    /// 会让 41,412 次调用里 2,809 次（6.8%）「有次数、无耗时」（**可得却漏采**，与 B7 同族）。
    /// （本用例是变异 M13 抓出来的缺口——任务书原文的 5 条用例对这条改动**零覆盖**。）
    #[test]
    fn custom_tool_call_output_is_paired_for_tool_ms() {
        let dir = tempfile::tempdir().unwrap();
        rollout(
            dir.path(),
            "rollout-ct.jsonl",
            vec![
                meta("sess-ct", "/p/CT", "codex-tui", false),
                json!({"timestamp":"2026-10-03T09:00:01Z","type":"turn_context","payload":{"model":"gpt-5-codex"}}),
                // 逐字真实形态：call_id 形如 `call-85595f17-…`（与 function_call 的 `call_…` 前缀不同）
                json!({"timestamp":"2026-10-03T09:00:02Z","type":"response_item","payload":{
                "type":"custom_tool_call","status":"completed","call_id":"call-abc","name":"apply_patch"}}),
                json!({"timestamp":"2026-10-03T09:00:03Z","type":"response_item","payload":{
                "type":"custom_tool_call_output","call_id":"call-abc",
                "output":"Exit code: 0\nWall time: 0 seconds\nOutput:\nSuccess."}}),
            ],
        );
        let no_cursors = HashMap::new();
        let ctx = CollectContext::new(dir.path(), 1_700_000_000_000, &no_cursors);
        let delta = CodexCollector.collect(&ctx).unwrap();
        let d = delta
            .details
            .iter()
            .find(|d| d.key.session_id == "sess-ct")
            .unwrap();
        assert_eq!(d.counters.tool_calls, 1, "custom_tool_call 计入次数");
        assert_eq!(
            d.counters.tool_ms, 1000,
            "回执必须配上（只认 function_call_output 时恒 0）"
        );
        assert_eq!(
            d.counters.tool_stats.get("apply_patch").copied(),
            Some((1, 1000)),
            "次数与耗时同源（不出现「0 次调用却有耗时」或「有次数无耗时」）"
        );
    }

    /// **R2 / D21 规则④（记录级项目归属）**：一个 rollout 可含父子两条 `session_meta`
    /// 且 **cwd 不同**（本机 119/425 文件含 >1 条 `session_meta`）→ 明细行必须落**各自的**
    /// 项目键，不得用会话级近似（「首个带 cwd 的记录」）把两条折成一行。
    #[test]
    fn two_threads_in_one_rollout_keep_their_own_project_keys() {
        let dir = tempfile::tempdir().unwrap();
        rollout(
            dir.path(),
            "rollout-p.jsonl",
            vec![
                meta("thread-a", "/p/Alpha", "Codex Desktop", false),
                json!({"timestamp":"2026-10-03T09:00:01Z","type":"turn_context","payload":{"model":"gpt-5-codex"}}),
                token_count(100, 0, 0, 10, 110),
                meta("thread-b", "/p/Beta", "Codex Desktop", false),
                token_count(200, 0, 0, 20, 220),
            ],
        );
        let no_cursors = HashMap::new();
        let ctx = CollectContext::new(dir.path(), 1_700_000_000_000, &no_cursors);
        let delta = CodexCollector.collect(&ctx).unwrap();
        assert_eq!(delta.details.len(), 2, "前提：两条线程各自一条明细行");
        let a = delta
            .details
            .iter()
            .find(|d| d.key.session_id == "thread-a")
            .unwrap();
        let b = delta
            .details
            .iter()
            .find(|d| d.key.session_id == "thread-b")
            .unwrap();
        assert_eq!(a.key.project_key, "alpha");
        assert_eq!(
            b.key.project_key, "beta",
            "R2：记录级 cwd → 同一文件里的两条线程各自成键（会话级近似只会留一个）"
        );
        // 会话维度是**近似**（首个带 cwd 的记录胜出），记录级才是分组口径
        let sa = delta
            .sessions
            .iter()
            .find(|s| s.session_id == "thread-a")
            .unwrap();
        let sb = delta
            .sessions
            .iter()
            .find(|s| s.session_id == "thread-b")
            .unwrap();
        assert_eq!(
            (sa.project_key.as_str(), sb.project_key.as_str()),
            ("alpha", "beta")
        );
    }

    /// 真机全量回归（§10）：正确判据命中率应落在 92.69% 附近。
    /// 默认忽略（需 2.21 GB 语料 + 1–3 分钟）：`cargo test --lib -- --ignored codex_real_corpus`
    ///
    /// 判据（`research/Codex-用量路径实测.md` §4.8，逐条 `total_tokens` 算术归一后）：
    /// 正确 **0.9269**；一律按包含 **2.0169**；一律按互斥 **0.6685**。
    /// 三种口径两两相差 ≥ 0.25，故 ±0.01 的容差足以把「判据或去重被改坏」全部抓住。
    /// 顺带打印全量诊断数字（W-16 的峰值 RSS 由外层 `/usr/bin/time -l` 采集，见报告）。
    #[test]
    #[ignore]
    fn codex_real_corpus_hit_rate_matches_measurement() {
        let home = dirs::home_dir().unwrap();
        // 空游标表必须先绑成变量：`&HashMap::new()` 是**临时值**，借用进 `ctx` 就是
        // E0716（temporary value dropped while borrowed）——语句结束即释放。
        let no_cursors = HashMap::new();
        let ctx = CollectContext::new(&home, crate::services::usage::now_ms(), &no_cursors);
        let delta = CodexCollector.collect(&ctx).unwrap();
        let (cache_read, request_total): (i64, i64) =
            delta.details.iter().fold((0, 0), |(c, r), d| {
                (c + d.buckets.cache_read, r + d.request_total)
            });
        let hit = cache_read as f64 / request_total as f64;
        // 诊断数字（--nocapture 可见）：只作报告证据，不作断言
        let requests: i64 = delta.details.iter().map(|d| d.requests).sum();
        let tool_calls: i64 = delta.details.iter().map(|d| d.counters.tool_calls).sum();
        let tool_ms: i64 = delta.details.iter().map(|d| d.counters.tool_ms).sum();
        let turns: i64 = delta.details.iter().map(|d| d.counters.turns).sum();
        let user_est: i64 = delta.details.iter().filter_map(|d| d.user_est).sum();
        let stats = ctx.stats();
        println!(
            "codex 真机全量：parsed_files={} details={} sessions={} new_records={} \
             requests={} cache_read={} request_total={} hit={hit:.6} \
             turns={turns} tool_calls={tool_calls} tool_ms={tool_ms} user_est={user_est} \
             reads={} repeat_reads={} bytes_read={}",
            delta.parsed_files,
            delta.details.len(),
            delta.sessions.len(),
            delta.new_records,
            requests,
            cache_read,
            request_total,
            stats.reads,
            stats.repeat_reads,
            stats.bytes_read,
        );
        assert_eq!(stats.repeat_reads, 0, "一遍扫描纪律（GC 4）");
        assert!(
            (hit - 0.9269).abs() < 0.01,
            "真机命中率 {hit:.4} 偏离实测 0.9269（判据或去重被改坏了）"
        );
    }

    /// **登记锁（独立验证复核 §3.2.2 阻塞 4）**：采集器必须出现在 `collectors::all()` 里。
    /// 漏登记不是编译错——`run_collection` 只遍历登记表，这个源会被静默跳过。
    #[test]
    fn collector_is_registered_in_all() {
        let ids: Vec<UsageSourceId> = crate::services::usage::collectors::all()
            .iter()
            .map(|c| c.source_id())
            .collect();
        assert!(
            ids.contains(&UsageSourceId::Codex),
            "codex 采集器必须登记进 collectors::all()，实际登记：{ids:?}"
        );
    }
}
// ==== usage 自省锁：排除区结束，勿删勿复制 ====
