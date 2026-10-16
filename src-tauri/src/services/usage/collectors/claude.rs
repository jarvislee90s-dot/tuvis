//! Claude Code 采集器（`~/.claude/projects/**/*.jsonl`）。
//! 单遍产出：`message.usage` 四桶 + `message.model` + 记录级 `cwd` + 工具调用/耗时配对 +
//! 三层报错 + 原生 `system/turn_duration`。**必须用独立 namespace**：既有
//! `claude-cwd` / `claude-digest` 缓存里存的是别的类型，同 ns 混存会让 downcast 失配。
// （`HashSet` 用全路径 `std::collections::HashSet`；测试模块自带 `use std::collections::HashMap;`
//   —— 文件级不再重复导入未使用的 `HashMap`，否则 `cargo clippy` 会报 unused import。）
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{file_key_of, file_read_failure_is_benign, DeltaBuilder, PendingTools};
use crate::monitor::session_scan::SessionFileScan;
use crate::services::usage::collect::{scan_namespace, CollectContext, UsageCollector};
use crate::services::usage::dedup::claude_request_key;
// **不得**导入 `CursorDelta`：本文件从头到尾没有显式写它的类型名（`read.next` 一路类型推断），
// 导进来就是 `unused_imports`，而门禁是 `-D warnings`（任务书 Step 4 的导入清单里带着它，
// 属 D-10 同族的「导入清单与代码不自洽」；见报告「偏差申报」）。
use crate::services::usage::delta::{DetailKey, SourceDelta};
use crate::services::usage::error::UsageError;
use crate::services::usage::model::UsageSourceId;
use crate::services::usage::project::project_key_of;
use crate::services::usage::range::hour_key_of_host;
use crate::services::usage::semantics::{
    default_policy, normalize, resolve_semantics, user_est_of, RawUsage,
};

/// 本源 `SessionFileScan` 的 namespace（GC 5 / **W-19**）：**必须**由 `scan_namespace(sid)`
/// 派生，不得手写字符串字面量——同 ns 混存别的产物类型会让 `Arc::downcast` 失配、
/// 缓存静默失效（每轮重新解析，正是 L2 要省掉的那笔开销）。
const SCAN_NAMESPACE: &str = scan_namespace(UsageSourceId::Claude);
const SCAN: SessionFileScan = SessionFileScan::new(SCAN_NAMESPACE);

// **Q-4 / A-1**：claude 无自带时区；即便将来带了，也**只许读入备查**——键一律走
// `hour_key_of_host`（spec §P6 改写后：一律以宿主本地时区为基准）。

pub struct ClaudeCollector;

/// 跨轮请求去重种子的容量（**裁决 A，fix round 1/5**）：上一轮已 admit 的请求键**尾部**条数。
/// 流式副本是**相邻**写入的（同一 `message.id` 的若干条挨着落盘），跨轮边界只可能切在最后一个
/// id 的副本序列中间 → 尾部 K 条足够。**K 是小常数**（W-11：不得把无上界的 `DedupSet`
/// 挂到长生命周期对象上；这里存进 `state_json` 的是**有界**的尾巴）。
const REQUEST_KEY_TAIL: usize = 8;

/// 本文件里「会话 → 是否可判定为子代理会话」的记忆容量（**裁决 B**）。
/// **失败方向是不对称的**：错判成 `true` → 父会话被 Task 18 整条排除（灾难：连会话数带
/// turn 数一起消失）；错判成 `false` → 子代理会话没被计数分层（轻微）。因此超限时
/// **置饱和位、此后一律不置真**（fail-safe），而不是反过来。守 W-11 / M-6 的有界纪律。
const SUBAGENT_SESSION_CAP: usize = 64;

/// 跨轮已 admit 的请求键（裁决 A）：`DedupSet` 每轮新建，靠这份**有界**尾巴把上一轮末尾的
/// 流式副本带过来。键必须**含会话**（GC 9：claude 的 `message.id` 会跨会话复用，只按 id
/// 去重会误删别的会话的真实用量）→ 这里存 `(session_id, message_id)` 两元组。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct RequestKeyTail {
    session_id: String,
    message_id: String,
}

/// 续读状态（进游标 state_json）：跨轮保住未配对工具调用 + **模型归属兜底** +
/// **请求去重种子**（裁决 A）+ **子代理会话身份**（裁决 B）
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct ClaudeFileState {
    tools: PendingTools,
    /// 本会话**最后已知模型**（跨轮继承）：user / system 记录没有 `message.model`（真实形态），
    /// 若直接落 `model=""` 会产出污染按模型分布的幽灵行（B2 缺陷③）。
    last_model: String,
    /// **裁决 A**：上一轮已 admit 的请求键**尾部**（有界，K = `REQUEST_KEY_TAIL` 条）。
    /// 没有它就会出现「副本 1 在本轮末尾、副本 2 落在下一轮追加区」→ 新 `DedupSet` 里没有
    /// 副本 1 → `requests += 1` 且四桶再累一次（**静默多算**；矩阵实测本机流式重复 2.68×）。
    #[serde(default)]
    recent_request_keys: Vec<RequestKeyTail>,
    /// **裁决 B**：会话 → 「截至目前，该会话的**每一条**记录都是侧链」。
    /// 缺失 = 从未见过该会话；`false` = 已出现非侧链记录（**永久**，跨轮保持）。
    /// 为什么需要跨轮记忆：增量读只看到文件中段时，「本轮全是侧链」**不等于**「这个会话是子代理」；
    /// 而 `usage_session.is_subagent` 在库里是 `MAX(...)`（只升不降，Task 3 的锁），
    /// 一旦某轮误置真，父会话就被 Task 18 硬过滤掉且**再也回不来**。
    #[serde(default)]
    session_all_sidechain: std::collections::BTreeMap<String, bool>,
    /// 会话记忆饱和（超出 `SUBAGENT_SESSION_CAP`）→ 此后**一律不置真**（fail-safe）
    #[serde(default)]
    session_memory_saturated: bool,
}

impl ClaudeFileState {
    /// 记一条记录并按**会话身份**回答「该会话是否可判定为子代理会话」（裁决 B）。
    ///
    /// * 非侧链记录 → 该会话**永久** `false`（"本会话自身是子代理" 已被证伪）；
    /// * 侧链记录 → 沿用记忆（首次见到则**暂定** `true`，等后续记录证伪）；
    /// * 记忆饱和 → 一律 `false`（fail-safe，见 `SUBAGENT_SESSION_CAP`）。
    fn note_record_sidechain(&mut self, session_id: &str, is_sidechain: bool) -> bool {
        if !is_sidechain {
            if let Some(v) = self.session_all_sidechain.get_mut(session_id) {
                *v = false;
            } else if self.session_all_sidechain.len() < SUBAGENT_SESSION_CAP {
                self.session_all_sidechain
                    .insert(session_id.to_string(), false);
            } else {
                self.session_memory_saturated = true;
            }
            return false;
        }
        if self.session_memory_saturated {
            return false;
        }
        match self.session_all_sidechain.get(session_id) {
            Some(v) => *v,
            None => {
                if self.session_all_sidechain.len() < SUBAGENT_SESSION_CAP {
                    self.session_all_sidechain
                        .insert(session_id.to_string(), true);
                    true
                } else {
                    self.session_memory_saturated = true;
                    false
                }
            }
        }
    }

    /// **裁决 A**：把一个刚 admit 的请求键推进有界尾巴（超限丢**最旧**的）。
    fn push_recent_key(&mut self, session_id: &str, message_id: &str) {
        self.recent_request_keys.push(RequestKeyTail {
            session_id: session_id.to_string(),
            message_id: message_id.to_string(),
        });
        if self.recent_request_keys.len() > REQUEST_KEY_TAIL {
            let overflow = self.recent_request_keys.len() - REQUEST_KEY_TAIL;
            self.recent_request_keys.drain(0..overflow);
        }
    }

    /// **裁决 A**：本轮 `DedupSet` 的种子——上一轮尾部那些键先占位，再跑本轮的新记录。
    fn seed_dedup(&self, dedup: &mut crate::services::usage::dedup::DedupSet) {
        for k in &self.recent_request_keys {
            dedup.admit(claude_request_key(&k.session_id, &k.message_id));
        }
    }
}

fn ts_ms_of(v: &Value) -> Option<i64> {
    v.get("timestamp")
        .and_then(|s| s.as_str())
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.timestamp_millis())
}

/// 本文件（本轮）的**主模型**：assistant 记录里 `message.model` 出现次数最多者（平局取**先出现**）。
///
/// 用途：claude 的 user / system 记录**没有** `message.model`（实测；见
/// `research/七源字段可得性矩阵.md` §3.1 与 Task 11 fixture 的第 1/4/6/7 行）。这些记录承载
/// `turns` / `userEst` / 工具耗时 / `turn_duration`——必须归到一个**真实模型行**上，否则会：
/// ① 造出 `model=""` 幽灵行（进明细表主键、进日聚合主键、在 UI 的「供应商 / 模型」行里显空名）；
/// ② 让 `turns`/`userEst`/工具耗时在正确的模型行上恒为 0（评审 B2 缺陷③）。
///
/// **边界（§3.2.2 次要 7）**：`None` 只表示"**本轮读到的行里**没有带模型的 assistant 记录"。
/// 若整个文件从来没有 assistant 记录（只剩 user/system，例如一条被中止的会话），
/// `ClaudeFileState.last_model` 也会一直是空串 → 明细行的 `model` / `provider` 落空串：
/// 这是**已知边界**（模型确实不可得），刻意**不丢计数**（丢 `turns`/`turn_ms` 比空模型名更糟），
/// 与 dsh 63/99 个缺 `modelSelection` 的会话同款。锁：
/// `file_without_any_assistant_model_keeps_counters_on_an_empty_model_row`。
///
/// **文件仍然只读一遍**：本函数遍历的是 `read_incremental` 已经读进内存的 `read.lines`，
/// 不碰文件、不计入 `CollectStats.reads`（`repeat_reads` 仍恒为 0）。
fn dominant_model(lines: &[String]) -> Option<String> {
    let mut counts: Vec<(String, usize)> = Vec::new();
    for line in lines {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if v.get("type").and_then(|t| t.as_str()) != Some("assistant") {
            continue;
        }
        let Some(m) = v
            .pointer("/message/model")
            .and_then(|s| s.as_str())
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        match counts.iter_mut().find(|(name, _)| name == m) {
            Some((_, n)) => *n += 1,
            None => counts.push((m.to_string(), 1)),
        }
    }
    // 严格大于才替换 → 平局保留**先出现**的那个（顺序确定，不随迭代序漂移）
    let mut best: Option<(String, usize)> = None;
    for (name, n) in counts {
        if best.as_ref().map(|(_, bn)| n > *bn).unwrap_or(true) {
            best = Some((name, n));
        }
    }
    best.map(|(m, _)| m)
}

/// 用户文本（仅 text 块；用于「用户输入(估)」）
fn user_text(v: &Value) -> String {
    v.pointer("/message/content")
        .and_then(|c| c.as_array())
        .map(|arr| {
            arr.iter()
                .filter(|i| i.get("type").and_then(|t| t.as_str()) == Some("text"))
                .filter_map(|i| i.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default()
}

fn tool_result_text(item: &Value) -> String {
    match item.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|i| i.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
}

/// 单文件读取失败是否**可跳过**（W-06 / D-24）：**已收进 `collectors/mod.rs` 共用**
/// （Task 12 评审 Important #3 / 裁决 B——两份逐字拷贝的漂移会把「读不了」静默降级成
/// 「这个源没有用量」，且不会有任何测试报红）。本文件经 `use super::file_read_failure_is_benign;`
/// 共用同一份实现；行为与原来逐字相同（用例 `only_the_vanished_file_race_is_skipped` 仍锁着它）。
impl UsageCollector for ClaudeCollector {
    fn source_id(&self) -> UsageSourceId {
        UsageSourceId::Claude
    }

    fn collect(&self, ctx: &CollectContext<'_>) -> Result<SourceDelta, UsageError> {
        let root = ctx.home.join(".claude").join("projects");
        if !root.exists() {
            return Ok(SourceDelta::default());
        }
        // **供应商规则从 ctx 注入**（§3.2.3 FIX-6）：lib 单测构建注入空规则 → 零 DB；
        // 生产/集成构建才回落到 settings::load()。采集器不得直连设置层。
        let rules = ctx.provider_rules();
        let mut b = DeltaBuilder::new(UsageSourceId::Claude, rules);
        let files = SCAN.collect(&root, |p| {
            p.extension().map(|e| e == "jsonl").unwrap_or(false)
        });
        let mut live = std::collections::HashSet::new();
        for (path, _mtime) in &files {
            live.insert(path.clone());
            // 游标键 = **相对 `~/.claude/projects` 的路径**（B5：不得用文件名 stem）
            let file_key = file_key_of(&root, path);
            let prev = ctx.cursor_of(&file_key).cloned();
            let mut state: ClaudeFileState = prev
                .as_ref()
                .and_then(|p| serde_json::from_str(&p.state_json).ok())
                .unwrap_or_default();
            let read = match ctx.read_incremental(path, &file_key) {
                Ok(r) => r,
                Err(e) => {
                    // **W-06**：只有「枚举与读取之间的轮转竞态」（文件已消失）才允许跳过；
                    // 其余读失败必须整源响亮失败（`usage-source-io` → `UsageSourceStatus.errorCode`）。
                    if file_read_failure_is_benign(path, &e) {
                        log::warn!(
                            "usage/claude: 读取 {} 失败（文件已消失，跳过本轮）: {}",
                            path.display(),
                            e
                        );
                        continue;
                    }
                    return Err(UsageError::new(
                        "usage-source-io",
                        format!("read {} failed: {e}", path.display()),
                    ));
                }
            };
            if read.lines.is_empty() && !read.rescan {
                b.push_cursor(read.next);
                continue;
            }
            if read.rescan {
                state = ClaudeFileState::default(); // 截断/重写：状态与累计一起重置
            }
            // 模型归属兜底：先用**本轮主模型**更新跨轮状态（遍历的是已读进内存的
            // `read.lines`，**不重读文件**——`repeat_reads` 仍恒为 0）。
            // 之后所有记录（含没有 `message.model` 的 user/system）都能拿到一个真实模型。
            if let Some(m) = dominant_model(&read.lines) {
                state.last_model = m;
            }
            // ---- 一遍扫描：四桶 + turn + 工具 + 报错 + 时长 全在这一轮里 ----
            // W-11：`DedupSet` 无上界 → **每个文件、每一轮各自新建**，不挂到长生命周期对象上。
            // **裁决 A（fix round 1/5）**：新建后**先把上一轮的尾部种子塞进去**——否则
            // 「副本 1 在本轮末尾、副本 2 落在下一轮追加区」会让副本 2 再计一次请求、
            // 四桶再累一次（静默多算）。种子是**有界**的（`REQUEST_KEY_TAIL` 条）。
            let mut dedup = crate::services::usage::dedup::DedupSet::new();
            state.seed_dedup(&mut dedup);
            for line in &read.lines {
                let Ok(v) = serde_json::from_str::<Value>(line) else {
                    continue;
                };
                b.new_records += 1;
                let ts = ts_ms_of(&v).unwrap_or(ctx.now_ms);
                let hour = hour_key_of_host(ts);
                let session_id = v
                    .get("sessionId")
                    .and_then(|s| s.as_str())
                    .map(str::to_string)
                    .unwrap_or_else(|| file_key.clone());
                let cwd = v.get("cwd").and_then(|s| s.as_str());
                let is_sidechain = v
                    .get("isSidechain")
                    .and_then(|x| x.as_bool())
                    .unwrap_or(false);
                // **裁决 B（fix round 1/5）**：`is_subagent` 的口径是「**本会话自身就是**子代理
                // 会话」，**不是**「本会话里出现过侧链记录」。claude 的侧链记录与父会话**同文件
                // 同 sessionId**（矩阵 §4.3「架构上含」）→ 把记录级的 `isSidechain` 或进去，
                // 唯一消费者 Task 18 按该位**硬过滤**时会把「用过子代理的父会话」**整条**排除
                // （连会话数带 turn 数一起消失）。这里按**会话身份**判定，且跨轮记住
                // 「该会话已出现过非侧链记录」（增量读只看到文件中段时，本轮全侧链 ≠ 子代理）。
                let is_subagent = state.note_record_sidechain(&session_id, is_sidechain);
                // 会话维度登记（**会话级近似**：首个带 cwd 的记录胜出）
                b.session(&session_id, cwd, None, is_subagent, None, None, ts);
                // **记录级**项目键：claude 每条记录自带 cwd（本机实测同一 jsonl 内可出现两个 cwd）
                // → 直接由该条记录的 cwd 生成，进 DetailKey（R2；不得回退会话级）。
                // 个别无 cwd 的记录落 `unknown` 桶：D21 已知代价，**不得**回退会话级项目。
                // W-13：逐记录一律走零 FS 的 `project_key_of`（`record_project` 只在会话级调一次）。
                let pk = project_key_of(cwd);
                // 模型：assistant 记录取 `message.model`；user / system 记录**没有**该字段（实测）
                // → 用本会话最后已知模型兜底（`state.last_model` 跨轮保有），**不落** `model=""`
                // 幽灵行（B2 缺陷③）。**边界**：文件里从未出现 assistant 记录时 `last_model` 仍是
                // 空串 → 该文件的计数落在一条空模型名行上（模型确实不可得；**不丢** turns/turn_ms，
                // 与 dsh 缺 `modelSelection` 的会话同款，见 `dominant_model` 文档与 §3.2.2 次要 7）
                let model = v
                    .pointer("/message/model")
                    .and_then(|s| s.as_str())
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(|| state.last_model.clone());
                // **供应商唯一入口**（GC 6 / B6）：经 `DeltaBuilder::provider_of` 记录三态。
                // 直接调 `provider::resolve_provider` 不写 `kind_by_provider` → `detail()` 只能
                // `unwrap_or(Unknown)` → 明细/日聚合 `provider_kind` 恒 `unknown` → D8 三态 UI 死掉。
                let (provider, _) = b.provider_of(None, &model);
                let key = DetailKey {
                    session_id: session_id.clone(),
                    hour_key: hour.clone(),
                    day_key: hour.chars().take(10).collect(),
                    project_key: pk.clone(),
                    model,
                    provider,
                };
                let kind = v.get("type").and_then(|s| s.as_str()).unwrap_or("");
                match kind {
                    "assistant" => {
                        // 1) 四桶（逐条语义判定 → 归一化；**唯一口径入口**）
                        if let Some(u) = v.pointer("/message/usage") {
                            let msg_id = v
                                .pointer("/message/id")
                                .and_then(|s| s.as_str())
                                .unwrap_or("");
                            // 流式重复去重：键含会话（§4.3；claude message.id 会跨会话复用）
                            if !msg_id.is_empty()
                                && dedup.admit(claude_request_key(&session_id, msg_id))
                            {
                                // **裁决 A**：admit 成功的键进有界尾巴，供**下一轮**去重
                                state.push_recent_key(&session_id, msg_id);
                                let raw = RawUsage {
                                    input_raw: u
                                        .get("input_tokens")
                                        .and_then(|x| x.as_i64())
                                        .unwrap_or(0),
                                    cache_read: u
                                        .get("cache_read_input_tokens")
                                        .and_then(|x| x.as_i64())
                                        .unwrap_or(0),
                                    cache_write: u
                                        .get("cache_creation_input_tokens")
                                        .and_then(|x| x.as_i64())
                                        .unwrap_or(0),
                                    output: u
                                        .get("output_tokens")
                                        .and_then(|x| x.as_i64())
                                        .unwrap_or(0),
                                    reasoning: 0,
                                    total_raw: None, // claude 无 total 判据 → 声明互斥
                                };
                                let sem =
                                    resolve_semantics(default_policy(UsageSourceId::Claude), &raw);
                                if let Some(n) = normalize(&raw, sem) {
                                    let d = b.detail(key.clone(), ts);
                                    d.buckets.add(&n.buckets);
                                    d.request_total += n.request_total;
                                    d.requests += 1;
                                    d.cache_semantics = sem;
                                }
                            }
                        }
                        // 2) API 错误（模型/传输层）
                        if v.get("isApiErrorMessage")
                            .and_then(|x| x.as_bool())
                            .unwrap_or(false)
                        {
                            b.detail(key.clone(), ts).counters.error_model += 1;
                        }
                        // 3) 工具调用（**发起侧**）：计数在这里，配对键 = **本记录的顶层 `uuid`**
                        //    —— 不是 `tool_use.id`！真实数据里回执写在 **user 记录**内、用
                        //    `sourceToolAssistantUUID` 指回发起它的 assistant 记录的 `uuid`
                        //    （矩阵 §Q2：配对 375/375 = 100%）。用 `tool_use.id` 作键永远配不上
                        //    → `tool_ms` 恒 0、`tool_stats` 耗时恒 0（B2 缺陷②）。
                        if let Some(arr) = v.pointer("/message/content").and_then(|c| c.as_array())
                        {
                            let rec_uuid = v
                                .get("uuid")
                                .and_then(|s| s.as_str())
                                .filter(|s| !s.is_empty());
                            for item in arr {
                                if item.get("type").and_then(|t| t.as_str()) != Some("tool_use") {
                                    continue;
                                }
                                let name = item
                                    .get("name")
                                    .and_then(|s| s.as_str())
                                    .unwrap_or("unknown");
                                let d = b.detail(key.clone(), ts);
                                d.counters.tool_calls += 1;
                                d.counters
                                    .tool_stats
                                    .entry(name.to_string())
                                    .or_insert((0, 0))
                                    .0 += 1;
                                // 一条 assistant 记录通常只带一个 tool_use；若带多个，只留**第一个**
                                // 参与耗时配对（计数仍各算各的），避免后一个覆盖前一个
                                if let Some(rec_uuid) = rec_uuid {
                                    state.tools.start_if_absent(rec_uuid, ts, name);
                                }
                            }
                        }
                    }
                    "user" => {
                        let has_tool_result = v
                            .get("toolUseResult")
                            .map(|x| !x.is_null())
                            .unwrap_or(false);
                        let is_meta = v.get("isMeta").and_then(|x| x.as_bool()).unwrap_or(false);
                        // ① 回合计数：user ∧ toolUseResult==null ∧ isMeta!=true（矩阵 §3.1）
                        if !has_tool_result && !is_meta {
                            let est = user_est_of(&user_text(&v));
                            let d = b.detail(key.clone(), ts);
                            d.counters.turns += 1;
                            // claude 是「能读到用户文本」的源（§4.3）：该条估算值为 0 也是**真值**
                            // （此处不是「不可得」——不可得的源压根不进这个分支，其 user_est 恒为 None）
                            d.user_est = Some(d.user_est.unwrap_or(0) + est);
                        }
                        // ② 工具回执配对（跨轮状态）：`sourceToolAssistantUUID` → assistant 的顶层 `uuid`
                        if let Some(call_uuid) =
                            v.get("sourceToolAssistantUUID").and_then(|s| s.as_str())
                        {
                            if let Some((dur, name)) = state.tools.finish(call_uuid, ts) {
                                let d = b.detail(key.clone(), ts);
                                d.counters.tool_ms += dur;
                                d.counters.tool_stats.entry(name).or_insert((0, 0)).1 += dur;
                            }
                        }
                        // ③ 工具错误 / 用户拒绝工具（D18 三层分列，绝不合并）：
                        //    **回执在 user 记录里**（真实形态）——写在 assistant 分支里会永不计数
                        //    （B2 缺陷①：工具报错/中断永不计数、error_tool 与 interrupted 恒 0）。
                        if let Some(arr) = v.pointer("/message/content").and_then(|c| c.as_array())
                        {
                            for item in arr {
                                if item.get("type").and_then(|t| t.as_str()) != Some("tool_result")
                                {
                                    continue;
                                }
                                if item
                                    .get("is_error")
                                    .and_then(|x| x.as_bool())
                                    .unwrap_or(false)
                                {
                                    let d = b.detail(key.clone(), ts);
                                    d.counters.error_tool += 1;
                                    // 用户拒绝工具 → 用户主动打断**单列**（不额外再加一次错误）
                                    if tool_result_text(item).contains(
                                        "The user doesn't want to proceed with this tool use",
                                    ) {
                                        d.counters.interrupted += 1;
                                    }
                                }
                            }
                        }
                    }
                    // 原生 turn 时长（§9.5 claude 行：`system/turn_duration.durationMs`）。
                    // **用 match guard，不在 arm 体内再套一层 `if`**：后者在门禁
                    // `-D warnings` 下触发 `clippy::collapsible_match`（任务书正文的写法如此，
                    // 属 D-05/D-18 同族的「正文代码过不了本项目自己的门禁」）。语义完全等价：
                    // `type=="system"` 但 subtype 非 turn_duration 的记录一样落 `_ => {}`。
                    "system"
                        if v.get("subtype").and_then(|s| s.as_str()) == Some("turn_duration") =>
                    {
                        if let Some(ms) = v.get("durationMs").and_then(|x| x.as_i64()) {
                            // 模型/供应商沿用上面统一算好的那一份（本记录同样没有
                            // `message.model` → 落到本文件主模型，不造幽灵行）。真实
                            // `system/turn_duration` 记录**带顶层 `cwd`**
                            // （research/可得性探测-文件类三源.md:54 的逐字样例行），
                            // 因此 key 与同小时的 assistant 行同桶；若真缺 cwd，按 D21 落
                            // `unknown` 项目桶（已知代价，**不得**回退成会话级项目）。
                            b.detail(key, ts).counters.turn_ms.push(ms);
                        }
                    }
                    _ => {}
                }
            }
            b.parsed_files += 1;
            let mut next = read.next;
            next.state_json = serde_json::to_string(&state).unwrap_or_default();
            b.push_cursor(next);
        }
        SCAN.retain_existing(&live);
        Ok(b.finish())
    }
}

// ==== usage 自省锁：以下为排除区（测试代码），勿删勿复制 ====
#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::usage::collect::{CollectContext, UsageCollector};
    // D-01：`use super::*;` 只带进**本模块自己** `use` 进来的名字与本地 item，
    // 不会穿透到 `services/usage/mod.rs` 的 `pub use model::*;`。
    // `SourceKind` / `parse_provider_rules` 都不在上面的模块级导入里 → 必须显式补。
    use crate::services::usage::model::SourceKind;
    use crate::services::usage::provider::parse_provider_rules;
    use serde_json::json;
    use std::collections::HashMap;

    fn write_line(dir: &std::path::Path, name: &str, v: serde_json::Value) -> std::path::PathBuf {
        use std::io::Write;
        let p = dir.join(name);
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&p)
            .unwrap();
        writeln!(f, "{}", serde_json::to_string(&v).unwrap()).unwrap();
        p
    }

    /// fixture 覆盖：流式重复去重 / 工具配对（sourceToolAssistantUUID）/ 工具错误 /
    /// 用户拒绝工具（interrupted）/ API 错误 / 回合计数（toolUseResult 过滤）/ 原生 turn 时长。
    ///
    /// **行号以本夹具为准（M-9，fix round 1/5：原文写的行号差一位）**——本夹具共 7 行：
    /// 1 user（回合）/ 2 assistant u1（m1 + tool_use call_1）/ 3 assistant u2（m1 流式副本）/
    /// 4 user（call_1 的回执）/ 5 assistant（m2 + isApiErrorMessage）/ 6 user（call_2 的回执，被拒）/
    /// 7 system turn_duration。
    ///
    /// **行序与真实形态对齐的三处（评审 B2）**：
    /// ① 第 1 行是 **user** 记录且**无 `message.model`**——真实 claude 的 user/system 记录都没有
    ///    该字段，所以它们必须靠"本文件主模型"归属，否则 `turns`/`userEst`/工具耗时全部落到
    ///    `model=""` 的幽灵行（进明细表主键、进日聚合主键、在 UI 的「供应商 / 模型」行里显空名）；
    /// ② **第 4 / 6 行**是 **user 记录里的 `tool_result`**（真实 claude 把回执写在 user 记录，
    ///    不是 assistant；见 `research/七源字段可得性矩阵.md` §Q2 抽样行）；
    /// ③ **第 4 行**的 `sourceToolAssistantUUID`（= `u1`）指向**第 2 行** assistant 记录的顶层
    ///    `uuid`，与 `tool_use.id`（= `call_1`）**不相等**——这正是配对键必须取记录 `uuid` 的原因
    ///    （矩阵 §Q2：「配对成功率 375/375 = 100%（按 `sourceToolAssistantUUID` → `uuid`）」）。
    /// **第 7 行**是 `system/turn_duration`：真实记录**带顶层 `cwd`**（`research/可得性探测-文件类三源.md:54`
    /// 的逐字样例行），因此这里也必须带——不带就会落 `unknown` 项目桶（D21 已知代价），
    /// 而不是本用例要找的那一行。
    fn fixture(dir: &std::path::Path) {
        let proj = dir.join(".claude/projects/-p-A");
        std::fs::create_dir_all(&proj).unwrap();
        let f = proj.join("sess-1.jsonl");
        let lines = [
            json!({"type":"user","sessionId":"s1","cwd":"/p/A","isMeta":false,
                   "message":{"role":"user","content":[{"type":"text","text":"你好 world"}]},
                   "timestamp":"2026-10-03T09:00:00Z"}),
            json!({"type":"assistant","sessionId":"s1","cwd":"/p/A","uuid":"u1",
                   "message":{"id":"m1","model":"claude-sonnet-4","usage":{
                       "input_tokens":100,"cache_read_input_tokens":800,
                       "cache_creation_input_tokens":50,"output_tokens":20},
                       "content":[{"type":"tool_use","id":"call_1","name":"Bash"}]},
                   "timestamp":"2026-10-03T09:00:01Z"}),
            // 流式重复：同 message.id 再来一条（本机 2.68×），必须被去重
            json!({"type":"assistant","sessionId":"s1","cwd":"/p/A","uuid":"u2",
                   "message":{"id":"m1","model":"claude-sonnet-4","usage":{
                       "input_tokens":100,"cache_read_input_tokens":800,
                       "cache_creation_input_tokens":50,"output_tokens":20},"content":[]},
                   "timestamp":"2026-10-03T09:00:01Z"}),
            json!({"type":"user","sessionId":"s1","cwd":"/p/A","toolUseResult":"ok",
                   "sourceToolAssistantUUID":"u1",
                   "message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"call_1",
                       "is_error":false,"content":"done"}]},
                   "timestamp":"2026-10-03T09:00:02.500Z"}),
            json!({"type":"assistant","sessionId":"s1","cwd":"/p/A","isApiErrorMessage":true,
                   "apiErrorStatus":404,"message":{"id":"m2","model":"claude-haiku-4",
                       "usage":{"input_tokens":10,"cache_read_input_tokens":0,
                                "cache_creation_input_tokens":0,"output_tokens":5},"content":[]},
                   "timestamp":"2026-10-03T09:00:03Z"}),
            json!({"type":"user","sessionId":"s1","cwd":"/p/A","toolUseResult":"denied",
                   "message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"call_2",
                       "is_error":true,
                       "content":"The user doesn't want to proceed with this tool use."}]},
                   "timestamp":"2026-10-03T09:00:04Z"}),
            json!({"type":"system","sessionId":"s1","cwd":"/p/A","subtype":"turn_duration","durationMs":60000,
                   "timestamp":"2026-10-03T09:01:00Z"}),
        ];
        let mut body = String::new();
        for l in lines {
            body.push_str(&serde_json::to_string(&l).unwrap());
            body.push('\n');
        }
        std::fs::write(&f, body).unwrap();
    }

    fn fixture_path(dir: &std::path::Path) -> std::path::PathBuf {
        dir.join(".claude/projects/-p-A/sess-1.jsonl")
    }

    fn run(
        home: &std::path::Path,
        cursors: &HashMap<String, crate::services::usage::delta::CursorDelta>,
    ) -> (
        crate::services::usage::delta::SourceDelta,
        crate::services::usage::collect::CollectStats,
    ) {
        let ctx = CollectContext::new(home, 1_700_000_000_000, cursors);
        let delta = ClaudeCollector.collect(&ctx).unwrap();
        (delta, ctx.stats())
    }

    #[test]
    fn single_pass_produces_all_metrics() {
        let dir = tempfile::tempdir().unwrap();
        fixture(dir.path());
        let (delta, stats) = run(dir.path(), &HashMap::new());
        assert_eq!(stats.repeat_reads, 0, "一遍扫描纪律");
        // **W-15**：只断言「值对了」不等于「路径对了」——把读取**次数**与**字节数**也钉住。
        // **M-1（fix round 1/5）**：`reads` 在 `read_incremental` **入口**自增，
        // **L2 快速路也会 +1**（见 `collect.rs::repeat_reads_are_counted` 的同款口径）→
        // 它只能证明「采集器对每个候选文件**走到了读取入口一次**」，**证明不了**「真读了一遍」。
        // 真正干活的是紧随其后的 `bytes_read == 文件字节数`（0 字节 = 走了快速路 / 压根没读；
        // 2× = 重复读）。
        assert_eq!(stats.reads, 1, "每个候选文件一轮只经读取入口一次（W-15）");
        assert_eq!(
            stats.bytes_read,
            std::fs::metadata(fixture_path(dir.path())).unwrap().len(),
            "首轮必须真读整个文件（全量路）：0 字节 = 没读，2× = 重复读（W-15 / M-1）"
        );
        assert_eq!(delta.parsed_files, 1);
        // 用量：m1（去重后 1 次）+ m2（1 次）；Exclusive 语义
        let total_requests: i64 = delta.details.iter().map(|d| d.requests).sum();
        assert_eq!(total_requests, 2, "流式重复的 m1 只能算一次请求");
        let sonnet = delta
            .details
            .iter()
            .find(|d| d.key.model == "claude-sonnet-4")
            .unwrap();
        assert_eq!(sonnet.buckets.input_fresh, 100);
        assert_eq!(sonnet.buckets.cache_read, 800);
        assert_eq!(sonnet.buckets.cache_write, 50);
        assert_eq!(sonnet.buckets.output, 20);
        assert_eq!(
            sonnet.request_total, 950,
            "Exclusive：input + cache_read + cache_write"
        );
        assert_eq!(sonnet.counters.tool_calls, 1);
        assert_eq!(
            sonnet.counters.tool_ms, 1500,
            "sourceToolAssistantUUID→uuid 配对耗时"
        );
        assert_eq!(
            sonnet.counters.turns, 1,
            "只有首条 user（isMeta=false 且 toolUseResult 为空）"
        );
        assert_eq!(sonnet.counters.error_model, 0);
        assert_eq!(sonnet.counters.interrupted, 1, "用户拒绝工具单列");
        assert_eq!(sonnet.counters.error_tool, 1);
        assert_eq!(
            sonnet.counters.turn_ms,
            vec![60_000],
            "原生 system/turn_duration"
        );
        assert_eq!(
            sonnet.counters.tool_stats.get("Bash").copied(),
            Some((1, 1500))
        );
        assert_eq!(sonnet.user_est, Some(3), "你好 world = 2 CJK + 1 词");
        assert_eq!(
            sonnet.key.project_key, "a",
            "R2：记录级项目键随明细行落地（D21 小写键）"
        );
        assert_eq!(sonnet.key.session_id, "s1");
        // 另一模型（Haiku 子任务）单独成行（§5.1：同一会话会混用模型）
        let haiku = delta
            .details
            .iter()
            .find(|d| d.key.model == "claude-haiku-4")
            .unwrap();
        assert_eq!(
            haiku.counters.error_model, 1,
            "isApiErrorMessage 计模型/传输层"
        );
        // 会话维度：D21 键 = 小写 projectName（**会话级近似**；记录级归属在每条明细行的
        // `key.project_key` 上，见下一个测试）
        let s = delta
            .sessions
            .iter()
            .find(|s| s.session_id == "s1")
            .unwrap();
        assert_eq!(s.project_key, "a");
        assert_eq!(s.project_label, "A");
        assert_eq!(s.project_path_raw, "/p/A");
        assert!(!s.is_subagent);
        assert_eq!(delta.new_records, 7);
    }

    /// R2：**同一 jsonl 内两个 cwd**（本机 claude 实例）→ 两条记录归入两个 projectKey，
    /// 不得折叠成一行；会话维度表只留「首个带 cwd 的记录」的项目（会话级近似，不参与分组）
    #[test]
    fn same_session_two_cwds_land_in_two_project_keys() {
        let dir = tempfile::tempdir().unwrap();
        let proj = dir.path().join(".claude/projects/-p-A");
        std::fs::create_dir_all(&proj).unwrap();
        let lines = [
            json!({"type":"assistant","sessionId":"s1","cwd":"/w/Alpha","uuid":"u1",
                   "message":{"id":"m1","model":"claude-sonnet-4",
                       "usage":{"input_tokens":10,"cache_read_input_tokens":0,
                                "cache_creation_input_tokens":0,"output_tokens":1},"content":[]},
                   "timestamp":"2026-10-03T09:00:00Z"}),
            // 同一会话 / 同一小时 / 同一模型与供应商，**只有 cwd 不同**
            json!({"type":"assistant","sessionId":"s1","cwd":"/w/Beta","uuid":"u2",
                   "message":{"id":"m2","model":"claude-sonnet-4",
                       "usage":{"input_tokens":20,"cache_read_input_tokens":0,
                                "cache_creation_input_tokens":0,"output_tokens":2},"content":[]},
                   "timestamp":"2026-10-03T09:10:00Z"}),
        ];
        let mut body = String::new();
        for l in lines {
            body.push_str(&serde_json::to_string(&l).unwrap());
            body.push('\n');
        }
        std::fs::write(proj.join("sess-2.jsonl"), body).unwrap();
        let (delta, _) = run(dir.path(), &HashMap::new());
        assert_eq!(
            delta.details.len(),
            2,
            "两个 cwd 必须各自成行（键含记录级 project_key）"
        );
        let alpha = delta
            .details
            .iter()
            .find(|d| d.key.project_key == "alpha")
            .unwrap();
        let beta = delta
            .details
            .iter()
            .find(|d| d.key.project_key == "beta")
            .unwrap();
        assert_eq!(alpha.buckets.input_fresh, 10);
        assert_eq!(beta.buckets.input_fresh, 20);
        assert_eq!(
            alpha.key.session_id, beta.key.session_id,
            "同一会话、两个项目"
        );
        assert_eq!(alpha.key.hour_key, beta.key.hour_key, "同一小时桶");
        // 会话维度（会话级近似）：首个带 cwd 的记录胜出，后一条不得覆盖
        let s = delta
            .sessions
            .iter()
            .find(|s| s.session_id == "s1")
            .unwrap();
        assert_eq!(s.project_key, "alpha");
        assert_eq!(s.project_path_raw, "/w/Alpha");
    }

    /// **B2 回归①（回执位置）**：`tool_result` 在 **user 记录**里——只把回执处理写在
    /// assistant 分支的实现会**永不计数**工具错误/用户拒绝，且 `tool_ms` 恒 0。
    /// 这里造一个"assistant 记录里**没有**任何 tool_result、回执全在 user 记录"的文件：
    /// 旧实现下 `error_tool == 0`、`interrupted == 0`、`tool_ms == 0`，本用例逐条钉住真值。
    #[test]
    fn tool_result_in_user_records_is_counted() {
        let dir = tempfile::tempdir().unwrap();
        let proj = dir.path().join(".claude/projects/-p-A");
        std::fs::create_dir_all(&proj).unwrap();
        let lines = [
            json!({"type":"assistant","sessionId":"s1","cwd":"/p/A","uuid":"u1",
                   "message":{"id":"m1","model":"claude-sonnet-4",
                       "usage":{"input_tokens":10,"cache_read_input_tokens":0,
                                "cache_creation_input_tokens":0,"output_tokens":1},
                       "content":[{"type":"tool_use","id":"call_1","name":"Bash"}]},
                   "timestamp":"2026-10-03T09:00:00Z"}),
            // 回执在 **user** 记录里：非零退出（工具失败）
            json!({"type":"user","sessionId":"s1","cwd":"/p/A","toolUseResult":"error",
                   "sourceToolAssistantUUID":"u1",
                   "message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"call_1",
                       "is_error":true,"content":"Process exited with code 1"}]},
                   "timestamp":"2026-10-03T09:00:01Z"}),
            json!({"type":"assistant","sessionId":"s1","cwd":"/p/A","uuid":"u2",
                   "message":{"id":"m2","model":"claude-sonnet-4",
                       "usage":{"input_tokens":10,"cache_read_input_tokens":0,
                                "cache_creation_input_tokens":0,"output_tokens":1},
                       "content":[{"type":"tool_use","id":"call_2","name":"Bash"}]},
                   "timestamp":"2026-10-03T09:00:02Z"}),
            // 回执在 user 记录里：用户拒绝（打断单列，不并入错误）
            json!({"type":"user","sessionId":"s1","cwd":"/p/A","toolUseResult":"denied",
                   "sourceToolAssistantUUID":"u2",
                   "message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"call_2",
                       "is_error":true,
                       "content":"The user doesn't want to proceed with this tool use."}]},
                   "timestamp":"2026-10-03T09:00:03Z"}),
        ];
        let mut body = String::new();
        for l in lines {
            body.push_str(&serde_json::to_string(&l).unwrap());
            body.push('\n');
        }
        std::fs::write(proj.join("sess-u.jsonl"), body).unwrap();
        let (delta, _) = run(dir.path(), &HashMap::new());
        let d = delta
            .details
            .iter()
            .find(|d| d.key.model == "claude-sonnet-4")
            .unwrap();
        assert_eq!(d.counters.tool_calls, 2, "两次 tool_use 都计");
        assert_eq!(
            d.counters.error_tool, 2,
            "user 记录里的 is_error 必须计数（旧实现恒 0）"
        );
        assert_eq!(
            d.counters.interrupted, 1,
            "用户拒绝单列一条，不重复计入 error_tool 之外"
        );
        assert_eq!(d.counters.tool_ms, 2000, "两次配对：1s + 1s（旧实现恒 0）");
        assert_eq!(
            d.counters.turns, 0,
            "两条 user 都带 toolUseResult → 不是「已发起回合」"
        );
    }

    /// **B2 回归②（配对键）**：配对键必须是 **assistant 记录的顶层 `uuid`**，
    /// 由 user 记录的 `sourceToolAssistantUUID` 指回；**不是** `tool_use.id`。
    /// 反例构造：`uuid` 与 `tool_use.id` **不相等**（真实数据处处如此，矩阵 §Q2 375/375 按 uuid 配对）——
    /// 用 `tool_use.id` 作键的实现配不上 → `tool_ms` 恒 0 / `tool_stats` 耗时恒 0。
    /// 另有"配对键不存在"的负例：`sourceToolAssistantUUID` 指向一个从未发起过的 uuid →
    /// 必须安静地不计数（不是 panic、不是记错误）。
    #[test]
    fn pairing_key_is_assistant_record_uuid_not_tool_use_id() {
        let dir = tempfile::tempdir().unwrap();
        let proj = dir.path().join(".claude/projects/-p-A");
        std::fs::create_dir_all(&proj).unwrap();
        let lines = [
            json!({"type":"assistant","sessionId":"s1","cwd":"/p/A","uuid":"uuid-aaa",
                   "message":{"id":"m1","model":"claude-sonnet-4",
                       "usage":{"input_tokens":1,"cache_read_input_tokens":0,
                                "cache_creation_input_tokens":0,"output_tokens":1},
                       "content":[{"type":"tool_use","id":"call-zzz","name":"Read"}]},
                   "timestamp":"2026-10-03T09:00:00Z"}),
            // 正例：回执指回 uuid-aaa（≠ tool_use.id call-zzz）→ 必须配上
            json!({"type":"user","sessionId":"s1","cwd":"/p/A","toolUseResult":"ok",
                   "sourceToolAssistantUUID":"uuid-aaa",
                   "message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"call-zzz",
                       "is_error":false,"content":"ok"}]},
                   "timestamp":"2026-10-03T09:00:02Z"}),
            // 负例：指向不存在的 uuid → 安静跳过
            json!({"type":"user","sessionId":"s1","cwd":"/p/A","toolUseResult":"ok",
                   "sourceToolAssistantUUID":"uuid-does-not-exist",
                   "message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"call-zzz",
                       "is_error":false,"content":"ok"}]},
                   "timestamp":"2026-10-03T09:00:03Z"}),
        ];
        let mut body = String::new();
        for l in lines {
            body.push_str(&serde_json::to_string(&l).unwrap());
            body.push('\n');
        }
        std::fs::write(proj.join("sess-p.jsonl"), body).unwrap();
        let (delta, _) = run(dir.path(), &HashMap::new());
        let d = delta
            .details
            .iter()
            .find(|d| d.key.model == "claude-sonnet-4")
            .unwrap();
        assert_eq!(d.counters.tool_calls, 1);
        assert_eq!(
            d.counters.tool_stats.get("Read").copied(),
            Some((1, 2000)),
            "按 tool_use.id 配对的实现这里会拿到 Some((1, 0))"
        );
        assert_eq!(d.counters.tool_ms, 2000);
        assert_eq!(
            d.counters
                .tool_stats
                .values()
                .map(|(_, ms)| *ms)
                .sum::<i64>(),
            2000,
            "负例不得贡献任何耗时"
        );
    }

    /// **B2 回归③（幽灵行）**：user / system 记录没有 `message.model`（真实形态）——
    /// 实现必须把它们归到**本文件主模型**，不得产出 `model == ""` 的明细行。
    /// 幽灵行的后果是确定的：它进明细表主键、进日聚合主键，并在 UI「供应商 / 模型」行里
    /// 显示一个空名行，同时把 `turns` / `userEst` / 工具耗时从正确模型行上偷走。
    ///
    /// **边界**：这条只在"文件里存在带 `message.model` 的 assistant 记录"时成立——
    /// 完全没有 assistant 记录的文件请看下一条用例（§3.2.2 次要 7）。
    #[test]
    fn records_without_message_model_never_create_ghost_rows() {
        let dir = tempfile::tempdir().unwrap();
        fixture(dir.path());
        let (delta, _) = run(dir.path(), &HashMap::new());
        assert!(
            delta.details.iter().all(|d| !d.key.model.is_empty()),
            "不得产出 model 为空的幽灵行：{:?}",
            delta
                .details
                .iter()
                .map(|d| (&d.key.model, d.key.project_key.as_str()))
                .collect::<Vec<_>>()
        );
        // 计数类事件（user 回合 / 工具耗时 / turn 时长）全部落在 sonnet 行上，一条都不外泄
        let sonnet = delta
            .details
            .iter()
            .find(|d| d.key.model == "claude-sonnet-4")
            .unwrap();
        assert_eq!(
            // 任务书勘误（D-22 同族，**已登记在报告「偏差申报」**）：原式
            // `turns + error_tool + turn_ms.len()` 是 **E0277/E0308**（`i64 + usize`）。
            // **最小修复**：边界处 `as i64`，判据与期望值（3）一字未改。
            sonnet.counters.turns
                + sonnet.counters.error_tool
                + sonnet.counters.turn_ms.len() as i64,
            3
        );
        let totals = (
            delta.details.iter().map(|d| d.counters.turns).sum::<i64>(),
            delta
                .details
                .iter()
                .map(|d| d.counters.error_tool)
                .sum::<i64>(),
            delta
                .details
                .iter()
                .map(|d| d.counters.turn_ms.len())
                .sum::<usize>(),
        );
        assert_eq!(
            totals,
            (1, 1, 1),
            "全文件只有 1 个回合 / 1 次工具失败 / 1 条 turn 时长样本"
        );
    }

    /// **B2③ 的边界（独立验证复核 §3.2.2 次要 7）**：文件里**从头到尾没有**带
    /// `message.model` 的 assistant 记录时，`dominant_model()` 返回 `None`、`last_model`
    /// 仍是空串——此时**主模型确实不可得**，实现的选择是"**照记但落空模型名**"
    /// （与 dsh 63/99 个缺 `modelSelection` 的会话同款已知边界），**不是**静默丢弃
    /// `turns` / `turn_ms`（丢掉会让 §9.5 里 claude 本可得的 turn 指标凭空变 0）。
    /// 这条用例把该分支**钉死**：谁改成"直接跳过"或"改成别的哨兵串"，这里立刻红——
    /// B2③ 那句"不再产出 `model=""` 行"的适用范围到此为止，不得外推。
    #[test]
    fn file_without_any_assistant_model_keeps_counters_on_an_empty_model_row() {
        let dir = tempfile::tempdir().unwrap();
        let proj = dir.path().join(".claude/projects/-p-A");
        std::fs::create_dir_all(&proj).unwrap();
        // 只有 user + system 两类记录（真实形态：两者都没有 message.model）
        let lines = [
            json!({"type":"user","sessionId":"s-nomodel","cwd":"/p/A","uuid":"u1",
                "message":{"role":"user","content":[{"type":"text","text":"你好"}]},
                "timestamp":"2026-10-03T09:00:00Z"}),
            json!({"type":"system","subtype":"turn_duration","sessionId":"s-nomodel","cwd":"/p/A",
                "uuid":"u2","durationMs":60000,"timestamp":"2026-10-03T09:01:00Z"}),
        ];
        let mut body = String::new();
        for l in &lines {
            body.push_str(&serde_json::to_string(l).unwrap());
            body.push('\n');
        }
        std::fs::write(proj.join("sess-nomodel.jsonl"), body).unwrap();

        let (delta, _) = run(dir.path(), &HashMap::new());
        assert_eq!(
            delta.details.len(),
            1,
            "两条记录同桶（同小时/同项目/同模型）→ 一条明细"
        );
        let d = &delta.details[0];
        assert_eq!(d.key.session_id, "s-nomodel");
        assert_eq!(
            d.key.model, "",
            "从未出现 assistant 记录 → 主模型不可得（已知边界，不是幽灵行）"
        );
        assert_eq!(d.key.provider, "", "供应商随模型一起不可得");
        assert_eq!(d.key.project_key, "a", "D21：项目键仍来自记录内 cwd");
        assert_eq!(d.counters.turns, 1, "计数不得因模型不可得而丢");
        assert_eq!(d.counters.turn_ms, vec![60_000], "turn 时长同样不得丢");
        assert!(
            d.user_est.is_some(),
            "claude 能读到用户文本 → userEst 有真值"
        );
    }

    /// **登记锁（独立验证复核 §3.2.2 阻塞 4）**：采集器必须出现在 `collectors::all()` 里。
    /// 漏登记**不是编译错**——`run_collection` 只遍历登记表，这个源会被静默跳过，
    /// 直到 Task 23B 的 `sources.len() == 7` 或 Task 24 的人工验收面才发现。
    /// 每个采集器文件都带一条同名用例，就地锁住自己那一行。
    #[test]
    fn collector_is_registered_in_all() {
        let ids: Vec<UsageSourceId> = crate::services::usage::collectors::all()
            .iter()
            .map(|c| c.source_id())
            .collect();
        assert!(
            ids.contains(&UsageSourceId::Claude),
            "claude 采集器必须登记进 collectors::all()，实际登记：{ids:?}"
        );
    }

    /// **W-19 登记锁**：`SessionFileScan` 的 namespace 必须来自 `scan_namespace(sid)`
    /// （`"usage-" + db_id()`），不得手写字符串字面量——同 ns 混存不同类型会让
    /// `Arc::downcast` 失配、缓存静默失效（GC 5）。
    #[test]
    fn scan_namespace_is_derived_from_the_source_id() {
        assert_eq!(
            SCAN_NAMESPACE,
            crate::services::usage::collect::scan_namespace(UsageSourceId::Claude)
        );
        assert_eq!(SCAN_NAMESPACE, "usage-claude");
        assert_ne!(
            crate::services::usage::collect::scan_namespace(UsageSourceId::Claude),
            crate::services::usage::collect::scan_namespace(UsageSourceId::Codex),
            "七源 ns 互不相同（Task 17 会全表断言）"
        );
    }

    /// **B5 回归（游标键）**：游标键 = **相对 `~/.claude/projects` 的路径**（分隔符归一为 `:`），
    /// **不是文件名 stem**——kimi 的 82 个 `wire.jsonl` 会撞成一行游标（后写覆盖先写），
    /// 其余 81 个文件每轮 `rescan = true` → 整文件重读重算并累加，账目按轮次线性膨胀。
    /// 这里断言采集器真的把 `file_key_of(root, path)` 当键：
    /// 改用 stem 的实现拿到的是 `"sess-1"`，本用例立刻红。
    #[test]
    fn cursor_key_is_the_path_relative_to_the_source_root_not_the_file_stem() {
        let dir = tempfile::tempdir().unwrap();
        fixture(dir.path());
        let (delta, _) = run(dir.path(), &HashMap::new());
        assert_eq!(delta.cursors.len(), 1);
        assert_eq!(
            delta.cursors[0].session_id, "-p-A:sess-1.jsonl",
            "游标键必须是相对源根的路径（用文件名 stem 的实现这里拿到 \"sess-1\"）"
        );
    }

    /// **B6 的单元级前置锁**：供应商必须经 `DeltaBuilder::provider_of` 记录三态。
    /// 采集器若绕过它直接调 `provider::resolve_provider`，`kind_by_provider` 永远是空的 →
    /// `DeltaBuilder::detail()` 只能 `unwrap_or(SourceKind::Unknown)` → 明细行的
    /// `provider_kind` 恒 `unknown`（落库后 D8 三态在 UI 上永久退化）。
    /// 落库级回归见 `tests/usage_ledger_test.rs::claude_provider_kind_is_persisted_to_usage_detail`。
    #[test]
    fn provider_attribution_goes_through_provider_of_and_records_the_three_state() {
        let dir = tempfile::tempdir().unwrap();
        let proj = dir.path().join(".claude/projects/-p-A");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(
            proj.join("sess-kind.jsonl"),
            format!(
                "{}\n",
                json!({"type":"assistant","sessionId":"s-kind","cwd":"/p/A","uuid":"u1",
                    "message":{"id":"m1","model":"claude-sonnet-4",
                        "usage":{"input_tokens":10,"cache_read_input_tokens":0,
                                 "cache_creation_input_tokens":0,"output_tokens":1},"content":[]},
                    "timestamp":"2026-10-03T09:00:00Z"})
            ),
        )
        .unwrap();
        let no_cursors = HashMap::new();
        let rules =
            parse_provider_rules(r#"{"rules":[{"prefix":"claude-","provider":"anthropic"}]}"#);
        let ctx = CollectContext::new(dir.path(), 1_700_000_000_000, &no_cursors)
            .with_provider_rules(rules);
        let delta = ClaudeCollector.collect(&ctx).unwrap();
        let d = delta
            .details
            .iter()
            .find(|d| d.key.session_id == "s-kind")
            .unwrap();
        assert_eq!(
            d.key.provider, "anthropic",
            "claude 无供应商字段 → 用户规则命中"
        );
        assert_eq!(
            d.provider_kind,
            SourceKind::Inferred,
            "provider_kind 必须由 provider_of 记录；绕开它直接调 resolve_provider → 这里恒 Unknown（B6）"
        );
    }

    /// **W-06 回归（响亮失败，不得只 `log::warn!`）**：`stat_of` 在 mtime 不可得 / 早于 1970
    /// 时返回 `Err`（Task 9 的 M4），这类文件**每轮都会 Err**；若 `collect()` 只 warn 后跳过，
    /// 用户在 UI 上看到的是「这个源没有用量」，而不是「这个源读不了」（W-06 补充条款）。
    /// 这里用 `filetime` 把 mtime 打到 1969（跨平台、不依赖权限位）确定性地造出该错误。
    ///
    /// 取舍说明（与任务书 Step 4 的 `log::warn + continue` 冲突，见报告「偏差申报」）：
    /// **只有「文件已消失」（NotFound = 原文点名的「刚被删/轮转」场景）才跳过**，其余一律整源 Err。
    #[test]
    fn unreadable_file_fails_the_source_loudly_instead_of_silently_skipping() {
        let dir = tempfile::tempdir().unwrap();
        let proj = dir.path().join(".claude/projects/-p-A");
        std::fs::create_dir_all(&proj).unwrap();
        let good = proj.join("sess-good.jsonl");
        let bad = proj.join("sess-bad.jsonl");
        let line = json!({"type":"assistant","sessionId":"s","cwd":"/p/A","uuid":"u1",
            "message":{"id":"m1","model":"claude-sonnet-4",
                "usage":{"input_tokens":10,"cache_read_input_tokens":0,
                         "cache_creation_input_tokens":0,"output_tokens":1},"content":[]},
            "timestamp":"2026-10-03T09:00:00Z"});
        std::fs::write(&good, format!("{line}\n")).unwrap();
        std::fs::write(&bad, format!("{line}\n")).unwrap();
        filetime::set_file_mtime(&bad, filetime::FileTime::from_unix_time(-1, 0)).unwrap();
        // 前提断言（防假绿）：这次改写**真的**让 stat_of 失败——否则本用例根本没走到那条分支。
        assert!(
            crate::services::usage::cursor::stat_of(&bad).is_err(),
            "前提不成立：mtime 没落到 1970 之前（平台夹取了负时间戳），本用例测不到该路径"
        );
        let no_cursors = HashMap::new();
        let ctx = CollectContext::new(dir.path(), 1_700_000_000_000, &no_cursors);
        let err = ClaudeCollector
            .collect(&ctx)
            .expect_err("mtime 不可得的文件必须让整源响亮失败（W-06），不得只 log::warn! 后跳过");
        assert_eq!(
            err.code, "usage-source-io",
            "错误码必须落进 UsageSourceStatus.errorCode"
        );
        assert!(
            err.detail.contains("sess-bad.jsonl"),
            "错误信息要点名是哪个文件读不了：{}",
            err.detail
        );
    }

    /// `NotFound` = 枚举与读取之间的轮转竞态（该文件下一轮已不在清单里）→ 允许跳过；
    /// 其余错误都是「这个源读不了」的信号 → 必须上抛（W-06）。
    /// **M-4（fix round 1/5）**：`NotFound` 还不够——**悬空符号链接**每轮都在清单里、
    /// 每轮 `NotFound`（`DirmEntry::metadata` 不跟随、`fs::metadata` 跟随）→ 必须再核链接本身。
    #[test]
    fn only_the_vanished_file_race_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let really_gone = dir.path().join("gone.jsonl"); // 连链接/文件都没有
        let not_found = std::io::Error::new(std::io::ErrorKind::NotFound, "gone");
        assert!(file_read_failure_is_benign(&really_gone, &not_found));
        assert!(!file_read_failure_is_benign(
            &really_gone,
            &std::io::Error::new(std::io::ErrorKind::InvalidData, "mtime before UNIX_EPOCH")
        ));
        assert!(!file_read_failure_is_benign(
            &really_gone,
            &std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied")
        ));
        // 悬空符号链接：链接**还在**（只是指向的目标没了）→ 不得当成「已消失」跳过
        #[cfg(unix)]
        {
            let dangling = dir.path().join("dangling.jsonl");
            std::os::unix::fs::symlink(dir.path().join("no-such-target.jsonl"), &dangling).unwrap();
            assert!(
                std::fs::symlink_metadata(&dangling).is_ok(),
                "前提不成立：链接本身应当还在"
            );
            assert!(
                !file_read_failure_is_benign(&dangling, &not_found),
                "M-4：悬空符号链接每轮都在清单里、每轮 NotFound → 只有整源 Err 才不会静默泄漏"
            );
        }
    }

    #[test]
    fn second_round_is_incremental_and_keeps_tool_pairing_state() {
        let dir = tempfile::tempdir().unwrap();
        fixture(dir.path());
        let (d1, _) = run(dir.path(), &HashMap::new());
        let cursors: HashMap<_, _> = d1
            .cursors
            .iter()
            .map(|c| (c.session_id.clone(), c.clone()))
            .collect();
        // 第二轮：追加一条 assistant 发起工具调用 + 一条 user 回执（跨轮配对）
        let proj = dir.path().join(".claude/projects/-p-A");
        let appended = json!({"type":"assistant","sessionId":"s1","cwd":"/p/A","uuid":"u9",
            "message":{"id":"m9","model":"claude-sonnet-4",
                "usage":{"input_tokens":1,"cache_read_input_tokens":0,"cache_creation_input_tokens":0,"output_tokens":1},
                "content":[{"type":"tool_use","id":"call_9","name":"Read"}]},
            "timestamp":"2026-10-03T09:05:00Z"});
        // **M-2（fix round 1/5）**：追加的字节数是**已知的**（`write_line` = `to_string` + '\n'）
        // → 断言必须取**等式**：`> 0` 会放过「整文件重读」这类退化（读回全文件也 > 0）。
        let appended_bytes = serde_json::to_string(&appended).unwrap().len() as u64 + 1;
        write_line(&proj, "sess-1.jsonl", appended);
        let (d2, stats2) = run(dir.path(), &cursors);
        assert_eq!(
            stats2.bytes_read, appended_bytes,
            "追加路必须**恰好**读到新写的那一行（>0 会放过整文件重读；0 = 没读到增量）"
        );
        let proj2 = dir.path().join(".claude/projects/-p-A");
        write_line(
            &proj2,
            "sess-1.jsonl",
            json!({"type":"user","sessionId":"s1","cwd":"/p/A",
            "toolUseResult":"ok","sourceToolAssistantUUID":"u9",
            "message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"call_9","is_error":false,"content":"ok"}]},
            "timestamp":"2026-10-03T09:05:02Z"}),
        );
        let cursors2: HashMap<_, _> = d2
            .cursors
            .iter()
            .map(|c| (c.session_id.clone(), c.clone()))
            .collect();
        let (d3, _) = run(dir.path(), &cursors2);
        // 第三轮必须只产出第二条的新增（第一轮的 m9 不重复计）
        assert_eq!(
            d3.details.iter().map(|d| d.requests).sum::<i64>(),
            0,
            "本轮没有新用量记录"
        );
        let read_call = d3
            .details
            .iter()
            .find(|d| d.key.model == "claude-sonnet-4")
            .and_then(|d| d.counters.tool_stats.get("Read").copied());
        assert_eq!(
            read_call,
            Some((0, 2000)),
            "跨轮配对的耗时落到第三轮（配对起点在上一轮的状态里）"
        );
    }

    /// **裁决 A 回归（fix round 1/5）：跨轮请求去重**——流式副本被**轮边界**切开 →
    /// 副本 1 在本轮末尾被 consume、游标推进过它，副本 2 落在下一轮追加区 → 新 `DedupSet`
    /// 里没有副本 1 → `admit` 返回 true → **`requests += 1` 且四桶再累一次（静默多算）**。
    /// 矩阵实测本机流式重复 **2.68×**（851 assistant 行 / 317 个 id），所以这不是理论情形。
    ///
    /// 四轮把三条判据一起钉死：
    /// * 第 2 轮：追加**已计过** `message.id`（`m1`）的第二份副本 → `requests == 0` **且四桶不涨**；
    /// * 第 3 轮：再追加一份副本 → 仍为 0（**尾巴必须跨过「本轮没有任何 admit」的那一轮**——
    ///   「用本轮 admit 的键覆盖尾巴」的实现会在这里红）；
    /// * 第 4 轮：追加一个**全新** id → `requests == 1`（防「把去重做成一律丢弃」的假绿）。
    #[test]
    fn streaming_duplicate_across_the_round_boundary_is_not_counted_twice() {
        let dir = tempfile::tempdir().unwrap();
        fixture(dir.path());
        let proj = dir.path().join(".claude/projects/-p-A");
        let dup = |uuid: &str| {
            json!({"type":"assistant","sessionId":"s1","cwd":"/p/A","uuid":uuid,
                "message":{"id":"m1","model":"claude-sonnet-4","usage":{
                    "input_tokens":100,"cache_read_input_tokens":800,
                    "cache_creation_input_tokens":50,"output_tokens":20},"content":[]},
                "timestamp":"2026-10-03T09:02:00Z"})
        };
        // 第一轮：m1 已计过 1 次（fixture 内部那条 + 它自己的流式副本已被同轮去重）
        let (d1, _) = run(dir.path(), &HashMap::new());
        assert_eq!(
            d1.details.iter().map(|d| d.requests).sum::<i64>(),
            2,
            "前提：第一轮 m1 + m2 各计一次"
        );
        let cur = |d: &crate::services::usage::delta::SourceDelta| {
            d.cursors
                .iter()
                .map(|c| (c.session_id.clone(), c.clone()))
                .collect::<HashMap<_, _>>()
        };
        let c1 = cur(&d1);

        // 第二轮：**已计过**的 m1 的副本被轮边界切到这一轮
        write_line(&proj, "sess-1.jsonl", dup("u-dup-2"));
        let (d2, _) = run(dir.path(), &c1);
        assert_eq!(
            d2.details.iter().map(|d| d.requests).sum::<i64>(),
            0,
            "轮边界另一侧的流式副本不得再计一次请求（裁决 A：静默多算）"
        );
        assert_eq!(
            (
                d2.details
                    .iter()
                    .map(|d| d.buckets.input_fresh)
                    .sum::<i64>(),
                d2.details.iter().map(|d| d.buckets.cache_read).sum::<i64>(),
                d2.details
                    .iter()
                    .map(|d| d.buckets.cache_write)
                    .sum::<i64>(),
                d2.details.iter().map(|d| d.buckets.output).sum::<i64>(),
            ),
            (0, 0, 0, 0),
            "四桶一个都不许涨"
        );

        // 第三轮：再来一份副本——**本轮没有任何 admit**，尾巴必须仍然有效
        write_line(&proj, "sess-1.jsonl", dup("u-dup-3"));
        let (d3, _) = run(dir.path(), &cur(&d2));
        assert_eq!(
            d3.details.iter().map(|d| d.requests).sum::<i64>(),
            0,
            "「用本轮 admit 的键覆盖尾巴」的实现在这里会红（空轮不得把尾巴清空）"
        );

        // 第四轮：全新的 message.id 必须照常计入（去重不得退化成「一律丢弃」）
        write_line(
            &proj,
            "sess-1.jsonl",
            json!({"type":"assistant","sessionId":"s1","cwd":"/p/A","uuid":"u-new",
                "message":{"id":"m-brand-new","model":"claude-sonnet-4","usage":{
                    "input_tokens":7,"cache_read_input_tokens":0,
                    "cache_creation_input_tokens":0,"output_tokens":3},"content":[]},
                "timestamp":"2026-10-03T09:03:00Z"}),
        );
        let (d4, _) = run(dir.path(), &cur(&d3));
        assert_eq!(
            d4.details.iter().map(|d| d.requests).sum::<i64>(),
            1,
            "新 id 必须照常计（防假绿：种子不得把新记录也吞掉）"
        );
        assert_eq!(
            d4.details
                .iter()
                .map(|d| d.buckets.input_fresh)
                .sum::<i64>(),
            7
        );
    }

    /// **裁决 B 回归（fix round 1/5）：同一会话混合侧链 + 非侧链记录**。
    /// claude 的侧链记录与父会话**同文件同 sessionId**（矩阵 §4.3「架构上含」）——
    /// `is_subagent` 的口径必须是「**本会话自身就是**子代理会话」，否则唯一消费者 Task 18
    /// 按该位**硬过滤**会把「用过子代理的父会话」**整条**排除（连会话数带 turn 数一起消失）。
    /// 用 `|=` 的实现会在第 1 条断言处红。
    #[test]
    fn mixed_sidechain_records_do_not_mark_the_parent_session_as_subagent() {
        let dir = tempfile::tempdir().unwrap();
        let proj = dir.path().join(".claude/projects/-p-A");
        std::fs::create_dir_all(&proj).unwrap();
        let lines = [
            // 侧链记录**在前**：`|=` 的实现一上来就把它钉成 true（且库里 MAX 只升不降）
            json!({"type":"assistant","sessionId":"s1","cwd":"/p/A","uuid":"u-side","isSidechain":true,
                   "message":{"id":"m-side","model":"claude-sonnet-4",
                       "usage":{"input_tokens":10,"cache_read_input_tokens":0,
                                "cache_creation_input_tokens":0,"output_tokens":1},"content":[]},
                   "timestamp":"2026-10-03T09:00:00Z"}),
            // 父会话自己的记录
            json!({"type":"user","sessionId":"s1","cwd":"/p/A","isMeta":false,
                   "message":{"role":"user","content":[{"type":"text","text":"你好"}]},
                   "timestamp":"2026-10-03T09:00:01Z"}),
        ];
        let mut body = String::new();
        for l in &lines {
            body.push_str(&serde_json::to_string(l).unwrap());
            body.push('\n');
        }
        std::fs::write(proj.join("sess-mixed.jsonl"), body).unwrap();
        let (delta, _) = run(dir.path(), &HashMap::new());
        let s = delta
            .sessions
            .iter()
            .find(|s| s.session_id == "s1")
            .unwrap();
        assert!(
            !s.is_subagent,
            "混合会话（侧链 + 非侧链）**不是**子代理会话：置真会让 Task 18 把父会话整条排除（裁决 B）"
        );
        // 前提断言（防假绿）：这条会话真的被登记了、且侧链记录真的进了桶（不是「没走到」）
        let d = delta
            .details
            .iter()
            .find(|d| d.key.model == "claude-sonnet-4")
            .unwrap();
        assert_eq!(
            d.requests, 1,
            "侧链记录的用量照常计入（矩阵：架构上天然含）"
        );
        assert_eq!(d.counters.turns, 1, "父会话的回合照常计");

        // 反向前提：**一份纯子代理 transcript**（从头到尾都是侧链）必须置真——
        // 否则「一律 false」的退化解也能过上面那条断言。
        let dir2 = tempfile::tempdir().unwrap();
        let proj2 = dir2.path().join(".claude/projects/-p-A");
        std::fs::create_dir_all(&proj2).unwrap();
        let pure = [
            json!({"type":"assistant","sessionId":"sub-1","cwd":"/p/A","uuid":"p1","isSidechain":true,
                   "message":{"id":"p-m1","model":"claude-sonnet-4",
                       "usage":{"input_tokens":5,"cache_read_input_tokens":0,
                                "cache_creation_input_tokens":0,"output_tokens":1},"content":[]},
                   "timestamp":"2026-10-03T09:00:00Z"}),
            json!({"type":"assistant","sessionId":"sub-1","cwd":"/p/A","uuid":"p2","isSidechain":true,
                   "message":{"id":"p-m2","model":"claude-sonnet-4",
                       "usage":{"input_tokens":6,"cache_read_input_tokens":0,
                                "cache_creation_input_tokens":0,"output_tokens":1},"content":[]},
                   "timestamp":"2026-10-03T09:00:01Z"}),
        ];
        let mut body2 = String::new();
        for l in &pure {
            body2.push_str(&serde_json::to_string(l).unwrap());
            body2.push('\n');
        }
        std::fs::write(proj2.join("sess-pure.jsonl"), body2).unwrap();
        let (delta2, _) = run(dir2.path(), &HashMap::new());
        let s2 = delta2
            .sessions
            .iter()
            .find(|s| s.session_id == "sub-1")
            .unwrap();
        assert!(
            s2.is_subagent,
            "一份纯子代理 transcript（全部记录都是侧链）必须置真，否则 D17 分层永远失效"
        );
    }

    /// **裁决 B 的跨轮面（本任务额外补，评审未点名）**：第 1 轮看到父会话的非侧链记录 →
    /// 第 2 轮追加的**只有**该会话的侧链记录。此时「本轮全是侧链」**不等于**「这个会话是子代理」——
    /// 只按本轮 AND 的实现会在这轮置真，而 `usage_session.is_subagent` 在库里是 `MAX(...)`
    /// （只升不降，Task 3 的锁）→ **父会话被永久排除且再也回不来**。
    #[test]
    fn sidechain_appended_in_a_later_round_never_promotes_the_parent_session() {
        let dir = tempfile::tempdir().unwrap();
        let proj = dir.path().join(".claude/projects/-p-A");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(
            proj.join("sess-late.jsonl"),
            format!(
                "{}\n",
                json!({"type":"user","sessionId":"s1","cwd":"/p/A","isMeta":false,
                    "message":{"role":"user","content":[{"type":"text","text":"你好"}]},
                    "timestamp":"2026-10-03T09:00:00Z"})
            ),
        )
        .unwrap();
        let (d1, _) = run(dir.path(), &HashMap::new());
        assert!(
            !d1.sessions
                .iter()
                .find(|s| s.session_id == "s1")
                .unwrap()
                .is_subagent,
            "前提：父会话第 1 轮不得是子代理"
        );
        // 第 2 轮：同一会话追加一条**侧链**记录（父会话里跑了子代理）
        write_line(
            &proj,
            "sess-late.jsonl",
            json!({"type":"assistant","sessionId":"s1","cwd":"/p/A","uuid":"late-1","isSidechain":true,
                "message":{"id":"late-m1","model":"claude-sonnet-4",
                    "usage":{"input_tokens":9,"cache_read_input_tokens":0,
                             "cache_creation_input_tokens":0,"output_tokens":1},"content":[]},
                "timestamp":"2026-10-03T09:01:00Z"}),
        );
        let cursors: HashMap<_, _> = d1
            .cursors
            .iter()
            .map(|c| (c.session_id.clone(), c.clone()))
            .collect();
        let (d2, _) = run(dir.path(), &cursors);
        assert!(
            !d2.sessions
                .iter()
                .find(|s| s.session_id == "s1")
                .unwrap()
                .is_subagent,
            "增量轮「全是侧链」不等于「这个会话是子代理」：跨轮记忆必须在（库里 MAX 只升不降）"
        );
    }
}
// ==== usage 自省锁：排除区结束，勿删勿复制 ====
