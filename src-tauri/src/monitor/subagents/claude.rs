//! Claude 子 agent 运行判据（spec §4.1 事件序状态机；判据权威/架构基准）。
//!
//! 落盘布局（spec §2.1 已修正版——评审 P0-1：父转写在**项目层平铺**，不在会话目录内）：
//!   ~/.claude/projects/<项目>/
//!   ├── <会话uuid>.jsonl                          父转写（通知 / SendMessage 事件源）
//!   └── <会话uuid>/subagents/agent-<agentId>.meta.json   登记 spawn（存在即 running 起点）
//!       <会话uuid>/subagents/agent-<agentId>.jsonl       子转写（timestamp + usage 四桶）
//!
//! 排雷（spec §2.1）：①通知 ≠ 终态（stop 后 SendMessage 续跑，同 id 多次通知）；
//! ②启动即有 tool_result（`Async agent launched…`），不能以「无 tool_result」判运行中。
//! **不做 mtime 兜底**（v1 裁决：长静默工具调用会误伤；残留随会话中断整区消失）。

use super::{
    read_increment, sort_views, update_entry, CacheBox, IncrRead, IncrState, SubagentStatus,
    SubagentView, TokenUsage,
};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// 会话目录发现（发现层，非判据；spec §2.1 已修正版——评审 P0-1）：父转写在
/// **项目层平铺**（`projects/<项目>/<sid>.jsonl`，与 remote/content.rs:452-469
/// find_jsonl_in_projects 同一发现模式），登记源 subagents/ 在会话目录内
/// （`projects/<项目>/<sid>/subagents/`）。两处齐备才算命中：无 subagents/ 的
/// 目录（旧平铺布局 / 无子 agent 会话）天然空态。父 jsonl 文件缺失不阻断发现
/// （read_increment 对缺失文件返回空行 → 无事件 = spawn 态，残留口径 §8.2 申报）。
/// T4 起 `remote::content` 的子 agent 详情读取复用本发现（同一 projects 扫描）。
pub(crate) fn locate(projects_root: &Path, session_id: &str) -> Option<(PathBuf, PathBuf)> {
    // 信任边界（review 修复，与 kimi.rs collect_with 的 sessions 根校验对称）：
    // session_id 是 UUID，路径分隔符或 ".." 只可能来自恶意构造的查询——入口拒绝，
    // 防 ../ 逃逸把发现层引出 projects 根。（join 后 starts_with 对含 .. 分量的
    // 路径只做词法前缀比较、不解析，拦不住该形态，故取「入口拒绝」而非出口校验）
    if session_id == ".." || session_id.contains('/') || session_id.contains('\\') {
        return None;
    }
    let Ok(entries) = std::fs::read_dir(projects_root) else {
        return None;
    };
    for e in entries.flatten() {
        let proj = e.path();
        if !proj.is_dir() {
            continue;
        }
        let subagents = proj.join(session_id).join("subagents");
        if subagents.is_dir() {
            return Some((proj.join(format!("{session_id}.jsonl")), subagents));
        }
    }
    None
}

/// meta 登记（readdir subagents/*.meta.json）
struct RegAgent {
    id: String,
    meta: AgentMeta,
}
fn registered_agents(subagents_dir: &Path) -> Vec<RegAgent> {
    let Ok(entries) = std::fs::read_dir(subagents_dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if let Some(id) = agent_id_from_meta_name(&name) {
            if let Ok(text) = std::fs::read_to_string(e.path()) {
                if let Some(meta) = parse_meta(&text) {
                    out.push(RegAgent {
                        id: id.to_string(),
                        meta,
                    });
                }
            }
        }
    }
    out.sort_by(|a, b| a.id.cmp(&b.id)); // 注册序稳定（spawnTs 同值时的次序确定性）
    out
}

/// 名字→档案 id 别名表（甲.5「名字→编号归一」，Stop/Resume 双向经同一映射）：
/// - 每个登记 agent 的 id 恒注册（经典机制事件只带 id；teammate 的 to=长 id 同走此键）；
/// - teammate（meta.taskKind 标记，spec §一.1 机制分派）额外注册 meta.name 别名；
/// - 键冲突（两个 teammate 同名）后者跳过——按 id 排序的注册序先到先得（申报边界）。
fn alias_map(reg: &[RegAgent]) -> HashMap<String, String> {
    let mut m: HashMap<String, String> = HashMap::new();
    for a in reg {
        m.entry(a.id.clone()).or_insert_with(|| a.id.clone());
        if a.meta.task_kind.as_deref() == Some(TASK_KIND_TEAMMATE) {
            if let Some(name) = a.meta.name.as_deref() {
                if !name.is_empty() {
                    m.entry(name.to_string()).or_insert_with(|| a.id.clone());
                }
            }
        }
    }
    m
}

/// 事件键归一：classify 产出的原始键（id 或名字）→ 登记档案 id。
/// 匹配不到登记集（如父会话向 team-lead 发消息——非本会话子 agent）→ None（忽略）
fn normalize_event(aliases: &HashMap<String, String>, ev: ParentEvent) -> Option<ParentEvent> {
    let raw = match &ev {
        ParentEvent::Stop { agent_id } | ParentEvent::Resume { agent_id } => agent_id,
    };
    let canonical = aliases.get(raw)?;
    Some(match ev {
        ParentEvent::Stop { .. } => ParentEvent::Stop {
            agent_id: canonical.clone(),
        },
        ParentEvent::Resume { .. } => ParentEvent::Resume {
            agent_id: canonical.clone(),
        },
    })
}

/// 会话级缓存条目（spec §5.1）
#[derive(Default, Clone)]
struct ClaudeCache {
    parent: IncrState,
    last: HashMap<String, LastEvent>,
    agents: HashMap<String, AgentTrack>,
}
#[derive(Default, Clone)]
struct AgentTrack {
    incr: IncrState,
    tokens: TokenUsage,
    first_ts: Option<String>,
    /// T2：末行 timestamp——idle 终态锚（停写即冻结；resume 后随新行更新但 status
    /// 转 Running → endTs 不再消费它）
    last_ts: Option<String>,
    /// T2（决策 I）：首条 user 原文——meta.description 缺失时的任务摘要回落源
    /// （spec §二.2 四要素：teammate meta 实测无 description 字段，甲.2）
    first_user: Option<String>,
}

/// 倒序分块扫描（重建路径，spec §5.1）：从文件尾向头按 256KB 取块，行序「新→旧」。
/// visitor 返回 true = 登记集收齐即停（不再读更早的块）。
/// 块边界语义：JSONL 行以 '\n' **右终结**——[块 + carry] 切段后，最后一段（其后无
/// '\n'）是跨块半行，进 carry 与左邻块拼接；其余段皆完整可访问。
/// **返回值 = 文件尾未终结半行字节**（空 = 以 '\n' 收尾）——carry 恒等于已处理
/// 区域最后一个 '\n' 之后缀，而处理区域总达文件尾，故 carry 非空 ⟺ 文件尾有未终结
/// 行（早停亦然）。调用方据此把增量游标对齐到 size − carry.len()（T1 不变式），该半行的
/// 事件留给增量路径补齐后消费——评审 P1-1：skip_to(size) 会把它永久丢失。
fn scan_parent_rev(path: &Path, mut visit: impl FnMut(&str) -> bool) -> std::io::Result<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    const BLOCK: u64 = 256 * 1024;
    let mut f = std::fs::File::open(path)?;
    let size = f.metadata()?.len();
    let mut pos = size;
    let mut carry: Vec<u8> = Vec::new(); // 右邻块的跨块半行（文件序在块后）
    while pos > 0 {
        let start = pos.saturating_sub(BLOCK);
        f.seek(SeekFrom::Start(start))?;
        let take = (pos - start) as usize;
        let mut buf = vec![0u8; take];
        f.read_exact(&mut buf)?;
        buf.extend_from_slice(&carry);
        carry.clear();
        let mut complete: Vec<(usize, usize)> = Vec::new(); // [a,b) 完整行字节区间
        let mut seg = 0usize;
        for (i, &b) in buf.iter().enumerate() {
            if b == b'\n' {
                complete.push((seg, i));
                seg = i + 1;
            }
        }
        carry = buf[seg..].to_vec();
        for (a, b) in complete.iter().rev() {
            if visit(&String::from_utf8_lossy(&buf[*a..*b])) {
                return Ok(carry); // 登记集收齐即停（carry 已是本块尾半行）
            }
        }
        pos = start;
    }
    Ok(carry)
}

/// 生产入口（T7 经 RemoteState 缝调用）
pub fn collect(session_id: &str) -> Vec<SubagentView> {
    let Some(root) = dirs::home_dir().map(|h| h.join(".claude").join("projects")) else {
        return Vec::new();
    };
    collect_with(&root, session_id)
}

/// 可注入核心（projects 根注入；测试 tempdir）
pub(crate) fn collect_with(projects_root: &Path, session_id: &str) -> Vec<SubagentView> {
    let Some((parent_jsonl, subagents_dir)) = locate(projects_root, session_id) else {
        return Vec::new();
    };
    collect_paths(parent_jsonl, subagents_dir, session_id)
}

/// 桌面看板热路径入口（#134）：父转写路径已知（claude_parser 手里就是它），
/// 免 locate 的 projects 根遍历——无子代理会话（绝大多数）一次 is_dir stat 即止。
/// 布局推导与 locate 同一规则（等价性测试锁定）；缓存键仍为 ("claude", session_id)
/// ——与 remote chips 端点共用同一增量缓存条目，看板徽标与面板计数机制性一致。
/// 重复 sid 边角（理论上不可达，sid 为 UUID）：locate 取 readdir 首个命中项目，
/// collect_at 以本转写自身的父目录为准——此边角两路分叉且 collect_at 更正确，
/// 不是 bug，勿「修」。
pub(crate) fn collect_at(parent_jsonl: &Path, session_id: &str) -> Vec<SubagentView> {
    // 信任边界同 locate：session_id 是 UUID，入口拒绝路径分量（防 ../ 逃逸）
    if session_id == ".." || session_id.contains('/') || session_id.contains('\\') {
        return Vec::new();
    }
    let Some(project_dir) = parent_jsonl.parent() else {
        return Vec::new();
    };
    let subagents_dir = project_dir.join(session_id).join("subagents");
    if !subagents_dir.is_dir() {
        return Vec::new();
    }
    collect_paths(parent_jsonl.to_path_buf(), subagents_dir, session_id)
}

/// 判据/缓存/状态机共享核：locate（或 collect_at 的路径推导）之后的所有工作
fn collect_paths(
    parent_jsonl: PathBuf,
    subagents_dir: PathBuf,
    session_id: &str,
) -> Vec<SubagentView> {
    let reg = registered_agents(&subagents_dir);
    // 闭包 move parent_jsonl + subagents_dir + reg（发现结果一次取齐，step 不再反推路径）
    update_entry("claude", session_id, move |cur| {
        let fresh = cur.is_none(); // 缓存缺失/被清（兔维斯 重启 / 硬上限清空）→ 走重建
        let mut st = cur
            .and_then(|b| b.downcast::<ClaudeCache>().ok())
            .map(|a| Arc::try_unwrap(a).unwrap_or_else(|a| (*a).clone()))
            .unwrap_or_default();
        if fresh {
            rebuild(&parent_jsonl, &reg, &mut st); // 倒序扫父文件收齐事件 + 游标对齐尾半行
                                                   // agent 转写无游标 → 全量读累计（rebuild 后 step 的 IncrState 从 0 起，
                                                   // 首轮 read_increment 自然全量消费——无需专门分支）
        }
        let out = step(&parent_jsonl, &subagents_dir, &reg, &mut st);
        (Arc::new(st) as CacheBox, out)
    })
}

fn step(
    parent_jsonl: &Path,
    subagents_dir: &Path,
    reg: &[RegAgent],
    st: &mut ClaudeCache,
) -> Vec<SubagentView> {
    let aliases = alias_map(reg); // T1：名字→档案归一（经典 id 键恒等映射，行为不变）
                                  // ① 父文件事件（增量；reset → 清空状态机后按全量行重放）
    match read_increment(parent_jsonl, &mut st.parent) {
        IncrRead::Unchanged => {}
        IncrRead::Lines { lines, reset } => {
            if reset {
                st.last.clear();
            }
            for line in &lines {
                if let Some(ev) = classify_parent_line(line) {
                    if let Some(ev) = normalize_event(&aliases, ev) {
                        apply_event(&mut st.last, &ev);
                    }
                }
            }
        }
    }
    // ② 每 agent 转写增量累计（reset → 清空累计后按全量行重放）
    for a in reg {
        let track = st.agents.entry(a.id.clone()).or_default();
        // 评审 P0-1：subagents 目录来自 locate() 的发现结果，不从父路径反推
        // （父在项目层平铺，parent() 反推会指到错误目录）
        let path = subagents_dir.join(format!("agent-{}.jsonl", a.id));
        match read_increment(&path, &mut track.incr) {
            IncrRead::Unchanged => {}
            IncrRead::Lines { lines, reset } => {
                if reset {
                    track.tokens = TokenUsage::default();
                    track.first_ts = None;
                    track.last_ts = None;
                    track.first_user = None;
                }
                for line in &lines {
                    accumulate_agent_line(
                        line,
                        &mut track.tokens,
                        &mut track.first_ts,
                        &mut track.last_ts,
                        &mut track.first_user,
                    );
                }
            }
        }
    }
    // ③ 视图：全量名单（T2）——登记即出卡，status 由事件序状态机分诊
    //（is_running：None/Spawn/Resume → Running，Stop → Idle）
    let mut views: Vec<SubagentView> = reg
        .iter()
        .map(|a| {
            let t = st.agents.get(&a.id);
            let running = is_running(true, st.last.get(&a.id).copied());
            SubagentView {
                id: a.id.clone(),
                name: a
                    .meta
                    .agent_type
                    .clone()
                    .unwrap_or_else(|| format!("agent-{}", a.id)),
                // 任务摘要（决策 I，spec §二.2 四要素）：meta.description 优先；
                // 缺失（teammate meta 实测无该字段）→ 首条 user 剥壳截断 40 字回落
                description: a.meta.description.clone().or_else(|| {
                    t.and_then(|t| t.first_user.as_deref())
                        .and_then(first_user_summary)
                }),
                spawn_ts: t.and_then(|t| t.first_ts.clone()),
                tokens: t.map(|t| t.tokens).unwrap_or_default(),
                status: if running {
                    SubagentStatus::Running
                } else {
                    SubagentStatus::Idle
                },
                end_ts: if running {
                    None
                } else {
                    t.and_then(|t| t.last_ts.clone())
                },
            }
        })
        .collect();
    sort_views(&mut views);
    views
}

/// 首次进缓存（重建路径）：倒序分块扫父文件收齐登记集事件 + agent 转写全量读
fn rebuild(parent_jsonl: &Path, reg: &[RegAgent], st: &mut ClaudeCache) {
    let wanted: HashSet<&str> = reg.iter().map(|a| a.id.as_str()).collect();
    let aliases = alias_map(reg);
    let mut got: HashSet<String> = HashSet::new();
    let tail_partial = scan_parent_rev(parent_jsonl, |line| {
        if let Some(ev) = classify_parent_line(line) {
            if let Some(ev) = normalize_event(&aliases, ev) {
                let id = match &ev {
                    ParentEvent::Stop { agent_id } | ParentEvent::Resume { agent_id } => {
                        agent_id.clone()
                    }
                };
                apply_event_rev(&mut st.last, &ev);
                got.insert(id);
            }
        }
        wanted.len() == got.len() // 登记集收齐即停；扫到头仍有未命中 = spawn 态（is_running 对 None 放行）
    })
    .unwrap_or_default(); // 读失败 → 无尾半行可续（下次 collect 由 last_size 兜底语义接管）
                          // 增量游标对齐 T1 不变式（评审 P1-1）：partial = 文件尾未终结半行——重建时刻
                          // 它的事件未被消费，skip_to(size) 会永久跳过；补齐换行后由增量路径消费
    if let Ok(m) = std::fs::metadata(parent_jsonl) {
        st.parent.resume_at(m.len(), tail_partial);
    }
}

/// 父 JSONL 行事件
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ParentEvent {
    /// 子 agent 停止通知（文本含 <task-id>该id</task-id>）
    Stop { agent_id: String },
    /// SendMessage 续跑（tool_use name=SendMessage, input.to=该id）
    Resume { agent_id: String },
}

/// 每子 agent 的最近事件（运行中 = 已登记 ∧ last ∈ {spawn, resume}）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LastEvent {
    /// 无任何事件（meta 已登记）——扫到头未命中即此态。**spawn 态由 None 表示**
    /// （重建收齐即停扫到头、无事件即 spawn），本变体保留状态空间文档语义、
    /// 不被构造（spec §4.1 三态 {spawn, stop, resume}）
    #[allow(dead_code)]
    Spawn,
    Stop,
    Resume,
}

/// 提取消息全部文本（content 字符串 / text 块数组两种形态）
fn message_text(v: &serde_json::Value) -> String {
    match &v["message"]["content"] {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(blocks) => blocks
            .iter()
            .filter(|b| b["type"].as_str() == Some("text"))
            .filter_map(|b| b["text"].as_str())
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

/// 文本中提取 <task-id>…</task-id>（无 regex 依赖，朴素查找）
fn task_id_in(text: &str) -> Option<&str> {
    const OPEN: &str = "<task-id>";
    const CLOSE: &str = "</task-id>";
    let start = text.find(OPEN)? + OPEN.len();
    let end = start + text[start..].find(CLOSE)?;
    Some(&text[start..end])
}

/// 文本中提取 `<teammate-message teammate_id="名字" …>` 的属性值（甲.4 机制 B）
fn teammate_id_in(text: &str) -> Option<&str> {
    const OPEN: &str = "<teammate-message";
    const ATTR: &str = "teammate_id=\"";
    let tag = text.find(OPEN)?;
    let rel = text[tag..].find(ATTR)?;
    let start = tag + rel + ATTR.len();
    let end = start + text[start..].find('"')?;
    Some(&text[start..end])
}

/// teammate 完成信号（甲.5 主判据）：文本含 `{"type":"idle_notification","from":"<名字>"}`。
/// 提取顺序：idle 标记之后的 `"from":"…"` 值；from 缺失 → teammate_id 属性辅助
/// （甲.5 明示可作辅助匹配）。无 idle 标记 → None（普通 teammate-message 不算）。
pub(crate) fn teammate_idle_from(text: &str) -> Option<&str> {
    const MARK: &str = "\"idle_notification\"";
    const FROM: &str = "\"from\":\"";
    let mark = text.find(MARK)?;
    if let Some(rel) = text[mark..].find(FROM) {
        let start = mark + rel + FROM.len();
        let end = start + text[start..].find('"')?;
        return Some(&text[start..end]);
    }
    teammate_id_in(text)
}

/// lastMessage 是否为「子 agent 回报」（观察台 §四 提醒开关的打标谓词，
/// 生产消费点 = claude_parser digest 打标 → Session.last_message_subagent_report）。
/// 与判据同一套信号原语（task_id_in + teammate-message 标记）——spec §四.3
/// 「判定与修复项同源，不靠文案匹配」的落点：前端只看后端打出的布尔。
pub(crate) fn is_subagent_report_text(text: &str) -> bool {
    task_id_in(text).is_some() || text.contains("<teammate-message")
}

/// 行分类（纯函数）：坏 JSON / 无关行 → None
pub(crate) fn classify_parent_line(line: &str) -> Option<ParentEvent> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    // resume 优先：assistant 的 SendMessage tool_use（input.to）
    if v["message"]["role"].as_str() == Some("assistant") {
        if let Some(blocks) = v["message"]["content"].as_array() {
            for b in blocks {
                if b["type"].as_str() == Some("tool_use")
                    && b["name"].as_str() == Some("SendMessage")
                {
                    if let Some(to) = b["input"]["to"].as_str() {
                        return Some(ParentEvent::Resume {
                            agent_id: to.to_string(),
                        });
                    }
                }
            }
        }
    }
    // stop：任意消息文本含 task-id 通知块
    let text = message_text(&v);
    if let Some(id) = task_id_in(&text) {
        return Some(ParentEvent::Stop {
            agent_id: id.to_string(),
        });
    }
    // stop②（观察台 T1，甲.5）：teammate 空闲通告——只有名字没有编号，
    // 归一到档案 id 由调用方的别名表完成（键在此保持原始形态）
    if let Some(name) = teammate_idle_from(&text) {
        return Some(ParentEvent::Stop {
            agent_id: name.to_string(),
        });
    }
    None
}

/// 增量路径：文件序应用（后到覆盖——last 语义）
pub(crate) fn apply_event(last: &mut HashMap<String, LastEvent>, ev: &ParentEvent) {
    let (id, val) = match ev {
        ParentEvent::Stop { agent_id } => (agent_id, LastEvent::Stop),
        ParentEvent::Resume { agent_id } => (agent_id, LastEvent::Resume),
    };
    last.insert(id.clone(), val);
}

/// 重建路径：新→旧扫描，每 id 首个（=最新）事件胜出
pub(crate) fn apply_event_rev(seen: &mut HashMap<String, LastEvent>, ev: &ParentEvent) {
    let (id, val) = match ev {
        ParentEvent::Stop { agent_id } => (agent_id, LastEvent::Stop),
        ParentEvent::Resume { agent_id } => (agent_id, LastEvent::Resume),
    };
    seen.entry(id.clone()).or_insert(val);
}

/// 运行中判据（spec §4.1）：已登记 ∧ last ∈ {spawn, resume}。
/// registered=false（meta 消失/未登记）一律不在跑。
pub(crate) fn is_running(registered: bool, last: Option<LastEvent>) -> bool {
    registered
        && matches!(
            last,
            None | Some(LastEvent::Spawn) | Some(LastEvent::Resume)
        )
}

/// 剥 `<teammate-message …>内文</teammate-message>` 壳（乙.3.1 teammate 首条
/// user 形态）；非包裹/残缺形态原样返回（不猜）。
/// **两语言两份实现申报（决策 I）**：与前端 SubagentDetail 的 extractTaskText
/// 同构不同语言（卡片摘要在后端算 / 详情任务原文在前端算），跨 JSON 边界
/// 各测各的——共享需端点透传首条 user 原文（载荷扩面），不值。
fn strip_teammate_wrapper(text: &str) -> &str {
    if !text.starts_with("<teammate-message") {
        return text;
    }
    let Some(open) = text.find('>') else {
        return text;
    };
    let Some(close) = text.rfind("</teammate-message>") else {
        return text;
    };
    if close > open {
        text[open + 1..close].trim()
    } else {
        text
    }
}

/// 任务摘要回落（决策 I）：首条 user 剥壳后按字符截断 40 + 省略号；空文本 → None
fn first_user_summary(text: &str) -> Option<String> {
    let stripped = strip_teammate_wrapper(text);
    if stripped.trim().is_empty() {
        return None;
    }
    let chars: Vec<char> = stripped.chars().take(41).collect();
    Some(if chars.len() > 40 {
        format!("{}…", chars[..40].iter().collect::<String>())
    } else {
        chars.into_iter().collect()
    })
}

/// 子 agent jsonl 单行累计（纯函数）：usage 四桶累加 + 首/末 timestamp 捕获 +
/// 首条 user 原文捕获（任务摘要回落源，决策 I）
pub(crate) fn accumulate_agent_line(
    line: &str,
    acc: &mut TokenUsage,
    first_ts: &mut Option<String>,
    last_ts: &mut Option<String>,
    first_user: &mut Option<String>,
) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
        return;
    };
    if let Some(ts) = v["timestamp"].as_str() {
        if first_ts.is_none() {
            *first_ts = Some(ts.to_string());
        }
        *last_ts = Some(ts.to_string());
    }
    if first_user.is_none() && v["type"].as_str() == Some("user") {
        match &v["message"]["content"] {
            serde_json::Value::String(s) if !s.trim().is_empty() => {
                *first_user = Some(s.clone());
            }
            serde_json::Value::Array(blocks) => {
                let joined: String = blocks
                    .iter()
                    .filter(|b| b["type"].as_str() == Some("text"))
                    .filter_map(|b| b["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("");
                if !joined.trim().is_empty() {
                    *first_user = Some(joined);
                }
            }
            _ => {}
        }
    }
    let u = &v["message"]["usage"];
    if !u.is_object() {
        return; // 无 usage 的消息（user/tool_result 等）不累计
    }
    acc.input += u["input_tokens"].as_u64().unwrap_or(0);
    acc.cache_read += u["cache_read_input_tokens"].as_u64().unwrap_or(0);
    acc.cache_creation += u["cache_creation_input_tokens"].as_u64().unwrap_or(0);
    acc.output += u["output_tokens"].as_u64().unwrap_or(0);
}

/// subagents/agent-<id>.meta.json 内容（字段缺失容忍）。
/// **不解析 spawnDepth、不区分深度**（口径明示，评审 P3-5）：spec §3 只收一层
/// 子 agent——claude 的 subagents/ 登记源天然只有一层（深层派生不落此目录），
/// 其余三工具无深度信息可过滤。
/// 2026-10-09 观察台 T1（甲.4）：teammate 机制带 taskKind/name——taskKind 是
/// 机制分派标记（spec §一.1），name 是名字→档案归一的别名源（甲.5）。
#[derive(Deserialize)]
pub(crate) struct AgentMeta {
    #[serde(rename = "agentType")]
    pub(crate) agent_type: Option<String>,
    pub(crate) description: Option<String>,
    #[serde(rename = "taskKind", default)]
    pub(crate) task_kind: Option<String>,
    #[serde(default)]
    pub(crate) name: Option<String>,
}

/// teammate 机制标记值（甲.4：meta.taskKind == "in_process_teammate"）
pub(crate) const TASK_KIND_TEAMMATE: &str = "in_process_teammate";

pub(crate) fn parse_meta(text: &str) -> Option<AgentMeta> {
    serde_json::from_str(text).ok()
}

/// `agent-<id>.meta.json` → `<id>`（其余文件名 → None）
pub(crate) fn agent_id_from_meta_name(name: &str) -> Option<&str> {
    name.strip_prefix("agent-")?
        .strip_suffix(".meta.json")
        .filter(|id| !id.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::path::PathBuf;

    /// 夹具（spec §2.1 已修正版布局，评审 P0-1）：父转写在项目层平铺
    /// （proj/<sid>.jsonl），subagents/ 在会话目录内（proj/<sid>/subagents/）；
    /// 持有 TempDir 随 drop 清理
    struct Fixture {
        /// 仅借 Drop 清理 tempdir（RAII），运行期不读
        #[allow(dead_code)]
        td: tempfile::TempDir,
        root: PathBuf,
    }
    fn claude_fixture(sid: &str) -> Fixture {
        let td = tempfile::tempdir().unwrap();
        let root = td.path().to_path_buf();
        let proj = root.join("proj--e--demo");
        std::fs::create_dir_all(proj.join(sid).join("subagents")).unwrap();
        // 父转写：项目层平铺（与 remote/content.rs find_jsonl_in_projects 的发现位置同层）
        std::fs::write(proj.join(format!("{sid}.jsonl")), "").unwrap();
        Fixture { td, root }
    }
    impl Fixture {
        /// 父转写路径（项目层平铺）
        fn parent(&self, sid: &str) -> PathBuf {
            self.root.join("proj--e--demo").join(format!("{sid}.jsonl"))
        }
        /// 会话目录（内含 subagents/）
        fn session(&self, sid: &str) -> PathBuf {
            self.root.join("proj--e--demo").join(sid)
        }
        fn add_agent(&self, sid: &str, id: &str, meta: &str, transcript: &str) {
            let d = self.session(sid).join("subagents");
            std::fs::write(d.join(format!("agent-{id}.meta.json")), meta).unwrap();
            std::fs::write(d.join(format!("agent-{id}.jsonl")), transcript).unwrap();
        }
        fn append(&self, p: &Path, s: &str) {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new().append(true).open(p).unwrap();
            f.write_all(s.as_bytes()).unwrap();
        }
    }

    /// 全链路：登记 → 出 chip（名/时长/token 四桶）；追加通知 → 消失；追加 SendMessage → 复现（P0 集成层）
    #[test]
    fn claude_source_lifecycle() {
        super::super::reset_cache_for_tests();
        let f = claude_fixture("sess-1");
        f.add_agent(
            "sess-1",
            "a86f",
            r#"{"agentType":"Plan","description":"设计新建会话两改动实现方案"}"#,
            &format!(
                "{}\n{}\n",
                agent_line("2026-10-08T07:36:35.508Z", 5124, 56448, 0, 9312),
                agent_line("2026-10-08T07:37:00Z", 100, 0, 0, 200)
            ),
        );
        let v = collect_with(&f.root, "sess-1");
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].id, "a86f");
        assert_eq!(v[0].name, "Plan");
        assert_eq!(
            v[0].description.as_deref(),
            Some("设计新建会话两改动实现方案")
        );
        assert_eq!(v[0].spawn_ts.as_deref(), Some("2026-10-08T07:36:35.508Z"));
        assert_eq!(
            v[0].tokens,
            TokenUsage {
                input: 5224,
                cache_read: 56448,
                cache_creation: 0,
                output: 9512
            }
        );
        // ② 追加停止通知 → 转 Idle 在板（T2 全量名单：「消失」语义移交前端 chip 层）
        f.append(&f.parent("sess-1"), &(notify("a86f") + "\n"));
        assert_eq!(
            collect_with(&f.root, "sess-1")[0].status,
            SubagentStatus::Idle
        );
        // ③ 追加 SendMessage → 复现为 Running（增量路径，P0）
        f.append(&f.parent("sess-1"), &(sendmsg("a86f") + "\n"));
        let v = collect_with(&f.root, "sess-1");
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].status, SubagentStatus::Running);
        // ④ token 增量：转写追加一行，四桶只加新增
        f.append(
            &f.session("sess-1")
                .join("subagents")
                .join("agent-a86f.jsonl"),
            &(agent_line("2026-10-08T07:38:00Z", 1, 2, 3, 4) + "\n"),
        );
        let v = collect_with(&f.root, "sess-1");
        assert_eq!(
            v[0].tokens,
            TokenUsage {
                input: 5225,
                cache_read: 56450,
                cache_creation: 3,
                output: 9516
            }
        );
        super::super::reset_cache_for_tests();
    }

    /// 重启重建（缓存缺失）：倒序分块扫父文件收齐事件；agent 无任何事件 → spawn 态运行中
    #[test]
    fn claude_source_rebuild_after_restart() {
        super::super::reset_cache_for_tests();
        let f = claude_fixture("sess-2");
        f.add_agent(
            "sess-2",
            "b1",
            r#"{"agentType":"Explore"}"#,
            &(agent_line("2026-10-08T07:00:00Z", 1, 0, 0, 1) + "\n"),
        );
        f.add_agent("sess-2", "b2", r#"{"agentType":"Plan"}"#, "");
        // b1 有停止通知、b2 无事件（→ spawn 态运行中）
        f.append(&f.parent("sess-2"), &(notify("b1") + "\n"));
        let v = collect_with(&f.root, "sess-2");
        assert_eq!(v.len(), 2, "全量名单（T2）：b1 Idle + b2 Running 均在板");
        assert_eq!(
            v.iter().find(|s| s.id == "b1").unwrap().status,
            SubagentStatus::Idle
        );
        assert_eq!(
            v.iter().find(|s| s.id == "b2").unwrap().status,
            SubagentStatus::Running
        );
        super::super::reset_cache_for_tests();
    }

    /// P1-1 回归锁：重建时刻父文件尾恰为半行通知 → 增量游标不得跳过它——
    /// 半行补齐换行后，增量路径必须消费到该 stop 事件（chip 消失）
    #[test]
    fn claude_source_rebuild_consumes_tail_partial_line() {
        super::super::reset_cache_for_tests();
        let f = claude_fixture("sess-6");
        f.add_agent("sess-6", "d1", r#"{"agentType":"Plan"}"#, "");
        // 完整无关行 + 半行通知（无结尾换行）
        std::fs::write(
            f.parent("sess-6"),
            format!("{}\n{}", r#"{"filler":1}"#, notify("d1")),
        )
        .unwrap();
        // 重建：半行不可解析 → d1 无事件 = spawn 态在板
        assert_eq!(collect_with(&f.root, "sess-6").len(), 1);
        // 补齐换行：增量路径消费该通知 → 转 Idle 在板（不下板）
        f.append(&f.parent("sess-6"), "\n");
        let v = collect_with(&f.root, "sess-6");
        assert_eq!(
            v.len(),
            1,
            "尾半行事件不得被 skip_to 丢弃（T2：停止 = 转灰非消失）"
        );
        assert_eq!(v[0].status, SubagentStatus::Idle);
        super::super::reset_cache_for_tests();
    }

    /// 护栏：父文件重写变短（size<offset）→ 作废重建，不 panic、不丢登记
    #[test]
    fn claude_source_parent_truncated_rebuilds() {
        super::super::reset_cache_for_tests();
        let f = claude_fixture("sess-3");
        f.add_agent("sess-3", "c1", r#"{"agentType":"Plan"}"#, "");
        f.append(&f.parent("sess-3"), &(notify("c1") + "\n"));
        let v = collect_with(&f.root, "sess-3");
        assert_eq!(v.len(), 1, "停止 → 转 Idle 在板（T2 全量名单）");
        assert_eq!(v[0].status, SubagentStatus::Idle);
        std::fs::write(f.parent("sess-3"), "x\n").unwrap(); // 重写变短
        let v = collect_with(&f.root, "sess-3");
        assert_eq!(v.len(), 1, "护栏作废后重建：c1 无事件 → spawn 态在板");
        assert_eq!(v[0].status, SubagentStatus::Running);
        super::super::reset_cache_for_tests();
    }

    /// 会话目录不存在 / 无 subagents 目录 → 空态（发现层防御，非判据）
    #[test]
    fn claude_source_missing_layout_is_empty() {
        super::super::reset_cache_for_tests();
        let f = claude_fixture("sess-4");
        assert!(collect_with(&f.root, "nope").is_empty());
        std::fs::create_dir_all(f.root.join("proj--e--demo").join("flat")).unwrap(); // 无 subagents/ 的目录
        assert!(collect_with(&f.root, "flat").is_empty());
        super::super::reset_cache_for_tests();
    }

    /// spawnTs 升序（None 排尾）
    #[test]
    fn claude_source_orders_by_spawn_ts() {
        super::super::reset_cache_for_tests();
        let f = claude_fixture("sess-5");
        f.add_agent(
            "sess-5",
            "late",
            r#"{"agentType":"A"}"#,
            &(agent_line("2026-10-08T09:00:00Z", 1, 0, 0, 1) + "\n"),
        );
        f.add_agent(
            "sess-5",
            "early",
            r#"{"agentType":"B"}"#,
            &(agent_line("2026-10-08T07:00:00Z", 1, 0, 0, 1) + "\n"),
        );
        let v = collect_with(&f.root, "sess-5");
        let ids: Vec<&str> = v.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["early", "late"]);
        super::super::reset_cache_for_tests();
    }

    /// review 修复：session_id 含路径分隔符 / .. → 空态（信任边界与 kimi.rs
    /// collect_with 的 sessions 根校验对称——防 ../ 逃逸把发现层引出 projects 根）
    #[test]
    fn claude_source_rejects_traversal_session_id() {
        super::super::reset_cache_for_tests();
        let f = claude_fixture("sess-9");
        f.add_agent("sess-9", "ok1", r#"{"agentType":"Plan"}"#, "");
        // 逃逸目标真实存在：proj--e--demo/../escape/subagents/（= projects 根下的
        // root/escape/…）有登记源——无守卫时 locate 会命中它
        let escape = f.root.join("escape");
        std::fs::create_dir_all(escape.join("subagents")).unwrap();
        std::fs::write(
            escape.join("subagents").join("agent-esc.meta.json"),
            r#"{"agentType":"Plan"}"#,
        )
        .unwrap();
        std::fs::write(escape.join("subagents").join("agent-esc.jsonl"), "").unwrap();
        assert!(
            collect_with(&f.root, "../escape").is_empty(),
            "../ 逃逸拒绝"
        );
        assert!(collect_with(&f.root, "..").is_empty());
        assert!(collect_with(&f.root, "..\\escape").is_empty());
        // 正常 session_id 不受影响（守卫只拦路径形态）
        assert_eq!(collect_with(&f.root, "sess-9").len(), 1);
        super::super::reset_cache_for_tests();
    }

    /// 倒序分块扫描边界：新→旧、收齐即停、以换行收尾的文件无尾半行
    #[test]
    fn scan_rev_visits_newest_first_and_stops_early() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("parent.jsonl");
        let mut body = String::new();
        for i in 0..100 {
            body.push_str(&format!("{{\"n\":{i}}}\n"));
        }
        std::fs::write(&p, body).unwrap();
        let mut seen: Vec<u32> = Vec::new();
        let carry = scan_parent_rev(&p, |line| {
            let n: u32 = serde_json::from_str::<serde_json::Value>(line).unwrap()["n"]
                .as_u64()
                .unwrap() as u32;
            seen.push(n);
            n == 90 // 第 90 行（新→旧第 10 个）命中即停
        })
        .unwrap();
        assert!(
            carry.is_empty(),
            "以换行收尾的文件无尾半行（P1-1 返回值语义）"
        );
        assert_eq!(seen.first(), Some(&99), "新→旧");
        assert_eq!(seen.last(), Some(&90));
        assert_eq!(seen.len(), 10, "收齐即停：不扫到头");
    }

    fn notify(id: &str) -> String {
        // 父 JSONL 通知行（spec §2.1：子 agent 停止追加 task-notification；实盘同 id 多次）
        format!(
            r#"{{"type":"user","timestamp":"2026-10-08T07:40:00Z","message":{{"role":"user","content":"<task-notification><task-id>{id}</task-id><summary>Agent stopped</summary></task-notification>"}}}}"#
        )
    }
    fn sendmsg(id: &str) -> String {
        // 父 JSONL SendMessage 续跑行（assistant tool_use，input.to 指向子 agent id）
        format!(
            r#"{{"type":"assistant","timestamp":"2026-10-08T07:41:00Z","message":{{"role":"assistant","content":[{{"type":"tool_use","id":"tu1","name":"SendMessage","input":{{"to":"{id}","body":"continue"}}}}]}}}}"#
        )
    }
    fn agent_line(ts: &str, inp: u64, cr: u64, cc: u64, out: u64) -> String {
        // 子 agent 转写行（每条消息带 timestamp + usage 四桶——spec §2.1）
        format!(
            r#"{{"timestamp":"{ts}","type":"assistant","message":{{"role":"assistant","usage":{{"input_tokens":{inp},"cache_read_input_tokens":{cr},"cache_creation_input_tokens":{cc},"output_tokens":{out}}}}}}}"#
        )
    }

    /// P0 回归锁（spec §2.1 排雷① + §7）：通知 ≠ 终态——stop 后 SendMessage 复现 running
    #[test]
    fn stop_then_sendmessage_is_running_again() {
        let mut last: HashMap<String, LastEvent> = HashMap::new();
        for line in [notify("a88"), sendmsg("a88")] {
            if let Some(ev) = classify_parent_line(&line) {
                apply_event(&mut last, &ev);
            }
        }
        assert!(
            is_running(true, last.get("a88").copied()),
            "stop → resume 后必须回到运行中（实盘 a588fb48 案例）"
        );
        // 反向：只有通知 → 不在跑
        let mut last2: HashMap<String, LastEvent> = HashMap::new();
        if let Some(ev) = classify_parent_line(&notify("a88")) {
            apply_event(&mut last2, &ev);
        }
        assert!(!is_running(true, last2.get("a88").copied()));
        // 同 id 多次通知（实盘 3 条）仍是 Stop
        let mut last3: HashMap<String, LastEvent> = HashMap::new();
        for _ in 0..3 {
            if let Some(ev) = classify_parent_line(&notify("a88")) {
                apply_event(&mut last3, &ev);
            }
        }
        assert_eq!(last3.get("a88"), Some(&LastEvent::Stop));
    }

    /// 倒序重建语义（spec §5.1）：新→旧扫描，每 id 取最新事件；无事件 → Spawn
    #[test]
    fn reverse_rebuild_takes_latest_event() {
        let mut seen: HashMap<String, LastEvent> = HashMap::new();
        // 倒序喂：新（resume）→ 旧（stop）
        for line in [sendmsg("b1"), notify("b1")] {
            if let Some(ev) = classify_parent_line(&line) {
                apply_event_rev(&mut seen, &ev);
            }
        }
        assert_eq!(seen.get("b1"), Some(&LastEvent::Resume));
        assert!(is_running(true, None), "扫到头无事件 = spawn 态（运行中）");
    }

    /// 行分类：无关行/坏 JSON/别的工具调用 → None；SendMessage 给别的 id 不影响本 id
    #[test]
    fn classify_ignores_irrelevant_lines() {
        assert_eq!(classify_parent_line("not json"), None);
        assert_eq!(
            classify_parent_line(r#"{"type":"user","message":{"role":"user","content":"hi"}}"#),
            None
        );
        let other = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"Read","input":{"to":"x"}}]}}"#;
        assert_eq!(
            classify_parent_line(other),
            None,
            "非 SendMessage 工具调用不构成 resume"
        );
    }

    /// usage 四桶累计 + 首/末 timestamp 捕获（timestamp 优先于 usage 在场性）
    #[test]
    fn accumulate_usage_and_first_ts() {
        let mut acc = TokenUsage::default();
        let mut first = None;
        let mut last = None;
        accumulate_agent_line(
            r#"{"timestamp":"2026-10-08T07:36:35.508Z","type":"user","message":{"role":"user","content":"go"}}"#,
            &mut acc,
            &mut first,
            &mut last,
            &mut None,
        );
        assert_eq!(
            first.as_deref(),
            Some("2026-10-08T07:36:35.508Z"),
            "首条 user 消息也带 timestamp"
        );
        accumulate_agent_line(
            &agent_line("2026-10-08T07:36:40Z", 1000, 2000, 0, 500),
            &mut acc,
            &mut first,
            &mut last,
            &mut None,
        );
        accumulate_agent_line(
            &agent_line("2026-10-08T07:37:00Z", 24, 56000, 7, 8812),
            &mut acc,
            &mut first,
            &mut last,
            &mut None,
        );
        assert_eq!(
            acc,
            TokenUsage {
                input: 1024,
                cache_read: 58000,
                cache_creation: 7,
                output: 9312
            }
        );
        assert_eq!(
            first.as_deref(),
            Some("2026-10-08T07:36:35.508Z"),
            "首条 timestamp 不被覆盖"
        );
        assert_eq!(
            last.as_deref(),
            Some("2026-10-08T07:37:00Z"),
            "末行 ts 持续覆盖"
        );
    }

    /// meta 解析（spec §2.1：{"agentType":"Plan","description":…}）+ 文件名提 id
    #[test]
    fn parse_meta_and_id() {
        let m = parse_meta(
            r#"{"agentType":"Plan","description":"设计新建会话两改动实现方案","toolUseId":"tu9","spawnDepth":1}"#,
        )
        .unwrap();
        assert_eq!(m.agent_type.as_deref(), Some("Plan"));
        assert_eq!(
            agent_id_from_meta_name("agent-a86f0c2d.meta.json"),
            Some("a86f0c2d")
        );
        assert_eq!(agent_id_from_meta_name("plan.md"), None);
        assert!(parse_meta("broken").is_none());
    }

    // ==== 2026-10-09 观察台 T1：teammate 机制（甲.4 对照表 · 甲.5 修复立项）====

    /// teammate 登记 meta（甲.4 机制 B 实录字段形态）：taskKind 标记 + 编队名 +
    /// name 字段 + spawnDepth:0；agentType 与 name 同值
    fn tm_meta(name: &str) -> String {
        format!(
            r#"{{"taskKind":"in_process_teammate","teamName":"session-tm","name":"{name}","agentType":"{name}","spawnDepth":0}}"#
        )
    }
    /// teammate 完成信号（甲.5 主判据）：user 行 content 含 teammate-message 包裹的
    /// idle_notification——只有名字没有编号（agentId 长串在父转写 0 次）
    fn tm_idle(name: &str) -> String {
        format!(
            r#"{{"type":"user","timestamp":"2026-10-08T15:02:38.000Z","message":{{"role":"user","content":"<teammate-message teammate_id=\"{name}\">{{\"type\":\"idle_notification\",\"from\":\"{name}\"}}</teammate-message>"}}}}"#
        )
    }
    /// teammate 完成报告（非 idle 的 teammate-message）：不构成 Stop（甲.5 待议不入本批）
    fn tm_report(name: &str) -> String {
        format!(
            r#"{{"type":"user","timestamp":"2026-10-08T15:02:26.000Z","message":{{"role":"user","content":"<teammate-message teammate_id=\"{name}\">任务已全部完成，结果 355/113</teammate-message>"}}}}"#
        )
    }
    /// SendMessage 续跑（to=名字形态；长 id 形态复用既有 sendmsg(id)）
    fn sendmsg_to(name: &str) -> String {
        format!(
            r#"{{"type":"assistant","timestamp":"2026-10-08T15:08:53.000Z","message":{{"role":"assistant","content":[{{"type":"tool_use","id":"tu2","name":"SendMessage","input":{{"to":"{name}","body":"continue"}}}}]}}}}"#
        )
    }

    /// P0 修复锁（spec §一.4/§六.1）：teammate 完成转灰、续跑转绿——全链路
    /// （T2：v1「完成即消失」语义移交前端 chip 过滤层，后端出全量名单）
    #[test]
    fn teammate_source_lifecycle() {
        super::super::reset_cache_for_tests();
        let f = claude_fixture("tm-1");
        // 甲.4：agentId = 名字前缀长串（abg-progress-test-3b76c22694757d64 形态）
        f.add_agent(
            "tm-1",
            "abg-progress-test-3b76c22694757d64",
            &tm_meta("bg-progress-test"),
            &(agent_line("2026-10-08T14:55:56.488Z", 100, 200, 0, 50) + "\n"),
        );
        // ① 出现（spawn 态，名称 = meta.agentType）
        let v = collect_with(&f.root, "tm-1");
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].name, "bg-progress-test");
        // ② 完成报告（非 idle）不转灰；空闲通告 → 转 Idle 在板（FAIL 修复主体）
        f.append(&f.parent("tm-1"), &(tm_report("bg-progress-test") + "\n"));
        assert_eq!(
            collect_with(&f.root, "tm-1")[0].status,
            SubagentStatus::Running,
            "非 idle 的 teammate-message 不构成 Stop（甲.5 待议）"
        );
        f.append(&f.parent("tm-1"), &(tm_idle("bg-progress-test") + "\n"));
        assert_eq!(
            collect_with(&f.root, "tm-1")[0].status,
            SubagentStatus::Idle,
            "idle_notification → 完成转灰在板（不下板）"
        );
        // ③ 续跑：to=名字 → 转回 Running（经同一名字→档案映射）
        f.append(&f.parent("tm-1"), &(sendmsg_to("bg-progress-test") + "\n"));
        assert_eq!(
            collect_with(&f.root, "tm-1")[0].status,
            SubagentStatus::Running,
            "续跑即复现"
        );
        // ④ 二次完成（甲.2 实录两组 teammate-message/两次 idle）→ 再转灰
        f.append(&f.parent("tm-1"), &(tm_idle("bg-progress-test") + "\n"));
        assert_eq!(
            collect_with(&f.root, "tm-1")[0].status,
            SubagentStatus::Idle
        );
        // ⑤ Resume 双形态：to=长 id（agentId 本体）也命中——别名表 id 恒注册
        f.append(
            &f.parent("tm-1"),
            &(sendmsg("abg-progress-test-3b76c22694757d64") + "\n"),
        );
        let v = collect_with(&f.root, "tm-1");
        assert_eq!(v.len(), 1, "to=长 id 经同一映射复现");
        assert_eq!(v[0].status, SubagentStatus::Running);
        super::super::reset_cache_for_tests();
    }

    /// 纯函数层：idle 行分类为 Stop（键=名字）；报告行 None；from 缺失回落 teammate_id（甲.5 辅助）
    #[test]
    fn classify_teammate_idle_line() {
        assert_eq!(
            classify_parent_line(&tm_idle("bg-progress-test")),
            Some(ParentEvent::Stop {
                agent_id: "bg-progress-test".to_string()
            })
        );
        assert_eq!(classify_parent_line(&tm_report("bg-progress-test")), None);
        // 辅助形态：无 from 字段、只有 teammate_id 属性 + idle 标记
        let aux = r#"{"type":"user","message":{"role":"user","content":"<teammate-message teammate_id=\"aux-agent\">{\"type\":\"idle_notification\"}</teammate-message>"}}"#;
        assert_eq!(
            classify_parent_line(aux),
            Some(ParentEvent::Stop {
                agent_id: "aux-agent".to_string()
            }),
            "from 缺失 → teammate_id 属性辅助匹配"
        );
    }

    /// 混跑（spec §一.4）：同会话经典 + teammate 各一，各自完成/续跑互不干扰
    /// （T2：完成不再出列，转灰在板——断言从 len 改为 status）
    #[test]
    fn teammate_and_classic_mixed() {
        super::super::reset_cache_for_tests();
        let f = claude_fixture("tm-2");
        f.add_agent(
            "tm-2",
            "ae78013828b11ec80",
            r#"{"agentType":"Plan","toolUseId":"tu1","spawnDepth":1}"#,
            "",
        );
        f.add_agent("tm-2", "abg-x-ef2d485433592526", &tm_meta("x-agent"), "");
        // 经典按 id 通知 → 转 Idle 在板；teammate 无事件仍 Running（名字空间互不污染）
        f.append(&f.parent("tm-2"), &(notify("ae78013828b11ec80") + "\n"));
        let v = collect_with(&f.root, "tm-2");
        assert_eq!(v.len(), 2, "全量名单：经典转灰不消失");
        assert_eq!(
            v.iter()
                .find(|s| s.id == "ae78013828b11ec80")
                .unwrap()
                .status,
            SubagentStatus::Idle
        );
        assert_eq!(
            v.iter()
                .find(|s| s.id == "abg-x-ef2d485433592526")
                .unwrap()
                .status,
            SubagentStatus::Running
        );
        // teammate 按名字 idle → 也转灰；经典不复活（id ≠ 名字）
        f.append(&f.parent("tm-2"), &(tm_idle("x-agent") + "\n"));
        let v = collect_with(&f.root, "tm-2");
        assert_eq!(v.len(), 2);
        assert!(v.iter().all(|s| s.status == SubagentStatus::Idle));
        super::super::reset_cache_for_tests();
    }

    /// 重启重建（缓存缺失）也能归一：父文件已含 idle → 倒序重建后完成态正确判定
    /// （T2：完成态 = Idle 在板，非消失）
    #[test]
    fn teammate_rebuild_after_restart() {
        super::super::reset_cache_for_tests();
        let f = claude_fixture("tm-3");
        f.add_agent("tm-3", "abg-y-111111111111111", &tm_meta("y-agent"), "");
        f.append(&f.parent("tm-3"), &(tm_idle("y-agent") + "\n"));
        let v = collect_with(&f.root, "tm-3");
        assert_eq!(v.len(), 1);
        assert_eq!(
            v[0].status,
            SubagentStatus::Idle,
            "重建路径同样经归一：名字键命中档案"
        );
        super::super::reset_cache_for_tests();
    }

    /// T7 消费的打标谓词（与判据同一套信号原语）：classic task-notification 与
    /// teammate-message 都算「子 agent 回报」；普通文本不算
    #[test]
    fn is_subagent_report_text_matrix() {
        assert!(is_subagent_report_text(&notify("a88")));
        assert!(is_subagent_report_text(&tm_idle("bg-progress-test")));
        assert!(is_subagent_report_text(&tm_report("bg-progress-test")));
        assert!(!is_subagent_report_text("用户自己发的消息"));
        assert!(!is_subagent_report_text(
            "Another Claude session sent a message: 普通内容"
        ));
    }

    // ==== 2026-10-09 观察台 T2：全量名单 + 终态锚 endTs + 摘要回落 ====

    /// T2 任务摘要回落（spec §二.2 四要素）：meta.description 缺失（teammate meta
    /// 实测无该字段）→ 首条 user 文本截断充当「它在做的任务」摘要；有 description
    /// 时 meta 优先
    #[test]
    fn claude_source_description_falls_back_to_first_user() {
        super::super::reset_cache_for_tests();
        let f = claude_fixture("st-2");
        let first_user = r#"{"timestamp":"2026-10-08T14:55:56.488Z","type":"user","message":{"role":"user","content":"<teammate-message teammate_id=\"team-lead\">设计一个两改动的实现方案并给出取舍</teammate-message>"}}"#;
        std::fs::write(
            f.session("st-2").join("subagents").join("agent-at1.jsonl"),
            format!("{first_user}\n"),
        )
        .unwrap();
        std::fs::write(
            f.session("st-2")
                .join("subagents")
                .join("agent-at1.meta.json"),
            tm_meta("t1"),
        )
        .unwrap();
        let v = collect_with(&f.root, "st-2");
        assert!(
            v[0].description
                .as_deref()
                .unwrap_or("")
                .contains("设计一个两改动的实现方案"),
            "无 description 的 teammate：摘要回落首条 user（剥壳截断）"
        );
        // 有 description 的经典 meta：meta 优先（既有 claude_source_lifecycle 已锁）
        super::super::reset_cache_for_tests();
    }

    /// T2 终态锚：idle 的 endTs = 子转写末行 timestamp（停写即冻结）；running 恒 None
    #[test]
    fn claude_source_end_ts_is_transcript_last_ts() {
        super::super::reset_cache_for_tests();
        let f = claude_fixture("st-1");
        f.add_agent(
            "st-1",
            "a1",
            r#"{"agentType":"Plan"}"#,
            &format!(
                "{}\n{}\n",
                agent_line("2026-10-08T07:00:00Z", 1, 0, 0, 1),
                agent_line("2026-10-08T07:05:00Z", 1, 0, 0, 1)
            ),
        );
        // 运行中：全量出卡 + status=Running + endTs=None
        let v = collect_with(&f.root, "st-1");
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].status, SubagentStatus::Running);
        assert_eq!(v[0].end_ts, None);
        // 完成 → status=Idle + endTs=末行 ts（07:05，不是首行）
        f.append(&f.parent("st-1"), &(notify("a1") + "\n"));
        let v = collect_with(&f.root, "st-1");
        assert_eq!(v.len(), 1, "全量名单：完成后仍在板（观察台 §二.3）");
        assert_eq!(v[0].status, SubagentStatus::Idle);
        assert_eq!(v[0].end_ts.as_deref(), Some("2026-10-08T07:05:00Z"));
        // 续跑 → 转回 Running、endTs 清空
        f.append(&f.parent("st-1"), &(sendmsg("a1") + "\n"));
        let v = collect_with(&f.root, "st-1");
        assert_eq!(v[0].status, SubagentStatus::Running);
        assert_eq!(v[0].end_ts, None);
        super::super::reset_cache_for_tests();
    }

    // ==== #134：桌面看板热路径入口 collect_at（与 locate 等价性锁定）====

    /// 视图比较键（SubagentView 无 PartialEq——不改生产 derive，取判据相关四元组）
    fn view_key(v: &SubagentView) -> (String, String, SubagentStatus, Option<String>) {
        (v.id.clone(), v.name.clone(), v.status, v.end_ts.clone())
    }

    /// 等价性锁定（#134 设计要求）：collect_at（parser 手里的父转写路径直接推导）
    /// 与 collect_with+locate（projects 根遍历发现）对同一布局必须同出。
    /// 两次调用之间清缓存——同键 ("claude", sid) 二次进入会直接命中缓存条目，
    /// 那样 locate 路径根本没跑，等价性断言就失真了。
    #[test]
    fn collect_at_equivalent_to_locate_discovery() {
        super::super::reset_cache_for_tests();
        let f = claude_fixture("badge-1");
        // r1：有停止通知 → Idle（endTs=子转写末行 ts）；r2：无事件 → spawn 态 Running
        f.add_agent(
            "badge-1",
            "r1",
            r#"{"agentType":"Plan"}"#,
            &(agent_line("2026-10-08T07:00:00Z", 1, 0, 0, 1)
                + "\n"
                + &agent_line("2026-10-08T07:05:00Z", 1, 0, 0, 1)
                + "\n"),
        );
        f.add_agent("badge-1", "r2", r#"{"agentType":"Explore"}"#, "");
        f.append(&f.parent("badge-1"), &(notify("r1") + "\n"));

        let via_locate: Vec<_> = collect_with(&f.root, "badge-1")
            .iter()
            .map(view_key)
            .collect();
        super::super::reset_cache_for_tests();
        let via_at: Vec<_> = collect_at(&f.parent("badge-1"), "badge-1")
            .iter()
            .map(view_key)
            .collect();
        assert_eq!(
            via_at, via_locate,
            "同布局两路同出（布局推导规则不得漂移出第二份语义）"
        );
        assert_eq!(via_at.len(), 2);
        assert!(via_at
            .iter()
            .any(|(id, _, st, _)| id == "r2" && *st == SubagentStatus::Running));
        assert!(via_at.iter().any(|(id, _, st, ts)| id == "r1"
            && *st == SubagentStatus::Idle
            && ts.as_deref() == Some("2026-10-08T07:05:00Z")));
        super::super::reset_cache_for_tests();
    }

    /// 旧平铺布局（项目目录内无 <sid>/subagents/）→ collect_at 空态
    /// （spec §1.2 定案：旧布局徽标归零为可接受收窄，此处锁定既定空态）
    #[test]
    fn collect_at_old_flat_layout_is_empty() {
        super::super::reset_cache_for_tests();
        let f = claude_fixture("badge-2");
        // 旧布局形态：项目目录里只有平铺的 agent-*.jsonl（无会话目录/subagents）
        std::fs::write(
            f.root.join("proj--e--demo").join("agent-old.jsonl"),
            agent_line("2026-10-08T07:00:00Z", 1, 0, 0, 1) + "\n",
        )
        .unwrap();
        assert!(
            collect_at(&f.parent("badge-2"), "badge-2").is_empty(),
            "旧布局（无 <sid>/subagents/）→ 空态"
        );
        super::super::reset_cache_for_tests();
    }

    /// 信任边界同 locate：恶意 session_id（../ 逃逸 / 路径分隔符）→ 空态
    #[test]
    fn collect_at_rejects_traversal_session_id() {
        super::super::reset_cache_for_tests();
        let f = claude_fixture("badge-3");
        f.add_agent("badge-3", "ok1", r#"{"agentType":"Plan"}"#, "");
        // 逃逸目标真实存在（root/escape/subagents/ 有登记源）——无守卫时
        // project_dir/../escape 布局推导会命中它
        let escape = f.root.join("escape");
        std::fs::create_dir_all(escape.join("subagents")).unwrap();
        std::fs::write(
            escape.join("subagents").join("agent-esc.meta.json"),
            r#"{"agentType":"Plan"}"#,
        )
        .unwrap();
        assert!(collect_at(&f.parent("badge-3"), "../escape").is_empty());
        assert!(collect_at(&f.parent("badge-3"), "..").is_empty());
        assert!(collect_at(&f.parent("badge-3"), "a/b").is_empty());
        assert!(collect_at(&f.parent("badge-3"), "a\\b").is_empty());
        // 正常 session_id 不受影响（守卫只拦路径形态）
        assert_eq!(collect_at(&f.parent("badge-3"), "badge-3").len(), 1);
        super::super::reset_cache_for_tests();
    }
}
