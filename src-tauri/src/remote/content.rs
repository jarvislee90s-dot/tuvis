// 会话内容读取层（M3 Task 7，C2 后端）：八工具统一出口（P9）
//
// 每个工具从其原生存储读取消息流尾部，映射为统一的 `SessionMessage`（camelCase，
// 对齐移动端契约）。映射关系逐工具对齐既有 parser（monitor/*_parser.rs）的实测格式：
// - zcode    → ~/.zcode/cli/db/db.sqlite（message/part 表，见 zcode_parser.rs 数据事实）
// - dsh      → ~/.dsh/sessions/<project>/<dir>/session.vN.jsonl.zstd（代际日志，dsh/log.rs）
// - claude   → ~/.claude/projects/*/<session-uuid>.jsonl（role + content[] 块协议）
// - codex    → ~/.codex/sessions/**/rollout-*.jsonl（type+payload 协议）优先，
//              未命中回退 thread_history_*.sqlite 的 thread_items（APP 线路，codex_thread_parser）
// - kimi     → session_index.jsonl 定位 + agents/main/wire.jsonl 事件流
// - workbuddy → ~/.workbuddy/projects/*/<sessionId>.jsonl（OpenAI 风格 type/role/content）
// - opencode → ~/.local/share/opencode/opencode.db（message/part 表）
// - openclaw → ~/.openclaw/state/openclaw.sqlite 的 acp_replay_events（ACP 协议 replay 日志，
//              2026-09-15 实机探测确认）；无该存储 → Err 优雅降级（Task 7 裁决，不阻塞其余七工具）
//
// 预算说明：本层是**按需单会话读取**（移动端点开详情才调用），不是 3 秒轮询路径，
// 不接 monitor::session_scan 的 L2/L3 缓存；但有界防御——文件读取一律走尾部窗口
// （read_recent_lines，512KB 上限），SQLite 查询一律 LIMIT + 参数化（session_id 走 ?1 占位，
// LIMIT 为 usize 格式化无注入面）。
//
// 测试约束（宪法级）：一律 tempdir / tmp sqlite / 黄金夹具，绝不触真实 ~/.zcode ~/.dsh 等；
// 生产薄壳只做 dirs::home_dir() → `*_with(home)` 注入核的转发（zcode_home_with 先例）。

use serde::Serialize;
use std::path::Path;

/// 统一消息条目（camelCase 序列化 = 移动端契约，勿改字段名）。
/// `seq` 是返回数组内的稳定顺序号（0 起文件序递增，前端按 seq 排序稳定）；
/// `collapsed`：thinking 与 tool-call 默认折叠（plan 一等卡片恒展开）。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionMessage {
    pub seq: i64,
    /// user / assistant（thinking、tool-call、tool-result、plan 属 agent 侧工作产物，
    /// 归 assistant）
    pub role: String,
    /// user / assistant / thinking / tool-call / tool-result / plan
    pub kind: String,
    /// 文本内容或工具摘要（kind = plan 时为计划 markdown 原文）
    pub content: String,
    /// epoch 毫秒（原生存储无时间戳的条目为 None）
    pub ts: Option<i64>,
    /// kind = tool-call / plan 时的工具名（plan 保留原名供前端辨识，不参与折叠标签）
    pub tool_name: Option<String>,
    /// kind = tool-call 时的参数 JSON 字符串（原生存储无参数则 None；plan 恒 None——
    /// 计划正文已升格进 content，参数串不再透传）
    pub tool_args: Option<String>,
    pub collapsed: bool,
}

/// 计划标签（批次丙 T4）：codex 的「计划」是**含 `<proposed_plan>…</proposed_plan>`
/// 标签的 assistant 消息**（非工具调用——这是 T1 工具参数升格接不住的那一类）。
/// 实机形态（本机 rollout 实录 `~/.codex/sessions/2026/08/03/rollout-*.jsonl`）：
/// 标签前可能还有一段自由文本（"未收到选择，按推荐默认：…以下是修复计划。"），
/// 标签内是完整 markdown 计划，标签后无后缀。
/// 升格口径：整段消息剥出标签内容 → `kind="plan"`（一等计划卡，默认展开）；标签**外**
/// 的前导文本若非空，作为普通 assistant 消息保留在计划卡之前（不丢用户可见内容）。
const PROPOSED_PLAN_OPEN: &str = "<proposed_plan>";
const PROPOSED_PLAN_CLOSE: &str = "</proposed_plan>";

/// 从 assistant 文本中剥出 `<proposed_plan>` 段（批次丙 T4 纯函数）。
/// 返回 `(标签前的文本, 标签内计划正文)`：
/// - 无开标签 / 无闭标签 / 标签内为空（trim）→ `None`（不升格，调用方原样出文本）；
/// - 开闭标签顺序颠倒 → `None`（防御：不猜）；
/// - 多个标签段 → 取**第一段**（实机只见一段；多段时第一段即主计划）。
///
/// 标签本身不进入 content（T4 的验收之一：无标签残留）。
fn split_proposed_plan(text: &str) -> Option<(String, String)> {
    let start = text.find(PROPOSED_PLAN_OPEN)?;
    let after_open = start + PROPOSED_PLAN_OPEN.len();
    let rest = &text[after_open..];
    let close_rel = rest.find(PROPOSED_PLAN_CLOSE)?;
    let plan = rest[..close_rel].trim();
    if plan.is_empty() {
        return None;
    }
    let preamble = text[..start].trim();
    Some((preamble.to_string(), plan.to_string()))
}

/// 计划文件引用识别（批次丙 T7）：工具结果/消息正文里出现的**计划文件路径** →
/// 消息流补一张「计划文件卡」（kind="plan-file"，前端渲染文件名 + 可点预览）。
///
/// **为何需要**：kimi 的计划是一等文件（`~/.kimi-code/sessions/…/agents/main/plans/
/// x.md`），写入动作在消息流里只留一行工具结果（"Wrote 4263 bytes to …"）——手机端
/// 因此只见一行而读不到计划全文（图1 诉求）。识别该引用后出卡，卡片经既有文件预览
/// 端点打开（豁免面见 `files::EXEMPT_SUBPATHS` 的 T7 尾段序列匹配）。
///
/// **识别判据（实测形态，勿凭想象扩面）**：本机 kimi wire 实录的三种引用都含
/// `plans/<name>.md` 路径片段——
///
/// - `Wrote <N> bytes to <path>/plans/x.md`（Write 工具结果，实测原文）
/// - `Plan file: <path>` / `Planning: <path>`（前缀变体，防御性覆盖）
///
/// 判据 = 文本含 `plans` 目录段（`/` 或 `\` 分隔皆可——Windows 反斜杠形态实测存在）
/// 且其后紧跟 `<文件名>.md`，且文件名不含空白/引号（防把整句吞进路径）。命中即
/// 产出——**按形态不按工具名**（claude/codex 的同类引用一并接住）。
///
/// 只取**第一个**命中（一次工具结果通常只写一个计划文件；多个时按出现序取首个，
/// 与「计划」语义一致）。
fn extract_plan_file_ref(text: &str) -> Option<String> {
    // 逐候选扫描 `plans` 目录段（双分隔符）：其后须紧跟 `<文件名>.md`
    let mut search_from = 0usize;
    while search_from < text.len() {
        let fwd = text[search_from..].find("plans/").map(|i| search_from + i);
        let bwd = text[search_from..].find("plans\\").map(|i| search_from + i);
        let plans_at = match (fwd, bwd) {
            (Some(a), Some(b)) => a.min(b),
            (Some(a), None) => a,
            (None, Some(b)) => b,
            (None, None) => return None,
        };
        let start = plans_at + "plans/".len();
        let rest = &text[start..];
        // 文件名：到 `.md` 为止，字符集排除空白与路径/引号分隔符
        if let Some(name_end) = rest.find(".md") {
            let name = &rest[..name_end];
            let ok = !name.is_empty()
                && !name.contains([' ', '\t', '\n', '\r', '"', '\'', '<', '>', '|'])
                && !name.contains('/')
                && !name.contains('\\');
            if ok {
                // 回取 `plans` 之前的路径部分（到最近的空白/引号/行首为止）——
                // 拼出可直接交给文件预览端点的**完整路径**（端点按会话 cwd 或绝对
                // 路径解析）。注意 `before` 取到 `plans` 之前，`tail` 从 `plans`
                // 开始——两段拼起来才是完整路径（省掉 tail 就会丢 `plans/` 段）
                let before = &text[..plans_at];
                let path_start = before
                    .rfind([' ', '\t', '\n', '\r', '"', '\'', '<', '>'])
                    .map(|i| i + 1)
                    .unwrap_or(0);
                let full = format!(
                    "{}{}",
                    &before[path_start..],
                    &text[plans_at..start + name_end + 3]
                );
                if !full.trim().is_empty() {
                    return Some(full.trim().to_string());
                }
            }
        }
        search_from = start;
    }
    None
}

impl SessionMessage {
    /// 文本类条目（user / assistant / thinking / tool-result）。
    /// collapsed 由 kind 推导：thinking 恒折叠
    fn text(kind: &str, content: impl Into<String>, ts: Option<i64>) -> Self {
        let kind = kind.to_string();
        let role = if kind == "user" { "user" } else { "assistant" };
        SessionMessage {
            seq: 0,
            role: role.to_string(),
            collapsed: kind == "thinking",
            kind,
            content: content.into(),
            ts,
            tool_name: None,
            tool_args: None,
        }
    }

    /// 一等计划卡构造（`plan` / `plan-file` 共用形态骨架，五处升格点的差异只在
    /// kind / content / tool_name）：role=assistant、collapsed=false（一等卡默认
    /// 展开）、tool_args=None——正文已升格进 content，参数串不再透传
    fn plan_card(kind: &str, content: String, ts: Option<i64>, tool_name: Option<String>) -> Self {
        SessionMessage {
            seq: 0,
            role: "assistant".to_string(),
            kind: kind.to_string(),
            content,
            ts,
            tool_name,
            tool_args: None,
            collapsed: false,
        }
    }

    /// assistant 文本条目（批次丙 T4）：含 `<proposed_plan>` 标签时**升格为一等计划卡**
    /// ——剥标签产 `kind="plan"`（与工具参数升格同构，见 [`Self::tool_call`]）。
    /// 返回 `Vec`（可能两条：前导文本 + 计划卡）；无标签 → 单条普通 assistant 消息。
    ///
    /// 收口在构造器侧（而非各工具的映射分支）：codex 走 `"message"` 分支产
    /// assistant 文本，其他工具也可能出现同形标签（形态判据天然通用）。
    fn assistant_text(content: impl Into<String>, ts: Option<i64>) -> Vec<Self> {
        let text: String = content.into();
        match split_proposed_plan(&text) {
            Some((preamble, plan)) => {
                let mut out = Vec::with_capacity(2);
                if !preamble.is_empty() {
                    out.push(Self::text("assistant", preamble, ts));
                }
                out.push(Self::plan_card("plan", plan, ts, None));
                out
            }
            None => vec![Self::text("assistant", text, ts)],
        }
    }

    /// 工具结果条目（批次丙 T7）：正文含**计划文件引用**时，除原 tool-result 外
    /// **追加一张计划文件卡**（kind="plan-file"，content=可预览路径，默认展开）。
    ///
    /// 卡片与 tool-result **并存**（不替换）：工具结果原文（"Wrote 4263 bytes to …"）
    /// 是操作事实，计划文件卡是「去读正文」的入口——两者对不同读者都有价值。
    fn tool_result_with_plan_ref(content: impl Into<String>, ts: Option<i64>) -> Vec<Self> {
        let text: String = content.into();
        let mut out = vec![Self::text("tool-result", text.clone(), ts)];
        if let Some(path) = extract_plan_file_ref(&text) {
            out.push(Self::plan_card("plan-file", path, ts, None));
        }
        out
    }

    /// 工具调用条目（collapsed = true）。**计划形态升格收口（T1 一等卡片，按形态
    /// 不按工具名）**：args JSON 顶层 `plan` 字段存在且为非空字符串（trim 口径）→
    /// 改产出 kind="plan" 一等消息——content = 计划 markdown 原文、role=assistant、
    /// tool_name 保留、tool_args=None、collapsed=false（默认展开，正合一等卡片语义）。
    ///
    /// 收口在本构造器的理由：八工具 13 个 tool-call 构造点全部经过此处，args 一律是
    /// 序列化后的 JSON 字符串——codex function_call 的字符串形态 arguments 天然覆盖，
    /// 无需逐点改写。解析失败 / plan 缺失 / plan 非字符串 / plan 空白 → 维持
    /// tool-call 原状（不 panic、不丢条目）。本层是按需单会话读取路径（非 3s
    /// 轮询），逐条 args 解析的开销可接受。
    fn tool_call(
        content: impl Into<String>,
        ts: Option<i64>,
        name: Option<String>,
        args: Option<String>,
    ) -> Self {
        if let Some(args_str) = args.as_deref() {
            let plan = serde_json::from_str::<serde_json::Value>(args_str)
                .ok()
                .and_then(|v| v.get("plan").and_then(|p| p.as_str()).map(String::from));
            if let Some(plan) = plan {
                if !plan.trim().is_empty() {
                    return Self::plan_card("plan", plan, ts, name);
                }
            }
        }
        SessionMessage {
            seq: 0,
            role: "assistant".to_string(),
            kind: "tool-call".to_string(),
            content: content.into(),
            ts,
            tool_name: name,
            tool_args: args,
            collapsed: true,
        }
    }
}

/// 会话内容读取结果：消息页 + 头部截断标记（Bug 1，M3 验收）。
/// truncated = 文件头部被字节窗截断（存在更早但未在本页的内容）——移动端据此
/// 显示「加载更早消息」；SQLite 系（zcode/opencode/openclaw/codex thread）与 dsh
/// （自有 zstd 代际读取）恒 false
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MessagesPage {
    pub messages: Vec<SessionMessage>,
    pub truncated: bool,
    /// 会话工作目录（可选，2026-10-13 会话产物批）：codex rollout（首行
    /// session_meta.payload.cwd）已填；其余工具暂 None——文件面板「会话产物」
    /// 相对路径的存在性校验基准。skip 序列化：/session-messages 载荷零变化
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

/// 会话内容源函数形态（RemoteState 注入缝的类型别名，生产 = read_session_messages）
pub type MessageSourceFn = dyn Fn(&str, &str, usize) -> Result<MessagesPage, String> + Send + Sync;

/// env 双参读取（DSH_HOME / KIMI_CODE_HOME）——**生产薄壳专用单一归口**（终审
/// Important 1）：/session-messages 与 /session-files 两条生产薄壳都从这里取值，
/// 禁止在别处复制 std::env::var 逻辑。值一律以参数传入注入核（impl），测试路径
/// 零真实 env 接触（与并行 env 测试互斥锁无交集）
pub(crate) fn read_env_homes() -> (Option<String>, Option<String>) {
    (
        std::env::var("DSH_HOME").ok(),
        std::env::var("KIMI_CODE_HOME").ok(),
    )
}

/// 统一出口（生产薄壳）：真实 home + 环境重定向（DSH_HOME / KIMI_CODE_HOME，
/// 与各工具看板扫描同源——dsh 走 scan_sessions(&dsh_home()) 的 M0 F14 优先级，
/// kimi 走 resolve_data_root 的 env 优先级）→ 注入核。
/// agent_type 与 `AgentType::tool_id()` 的小写形态一致（zcode/dsh/claude/codex/kimi/
/// workbuddy/opencode/openclaw）。
pub fn read_session_messages(
    agent_type: &str,
    session_id: &str,
    limit: usize,
) -> Result<MessagesPage, String> {
    let home = dirs::home_dir().ok_or_else(|| "无法确定用户主目录".to_string())?;
    // env 只在生产薄壳读取（fix round 1 Important 1；读取归口 read_env_homes，
    // 终审 Important 1 起 files.rs 生产薄壳同源复用）
    let (dsh_env, kimi_env) = read_env_homes();
    read_session_messages_impl(
        &home,
        dsh_env.as_deref(),
        kimi_env.as_deref(),
        agent_type,
        session_id,
        limit,
    )
}

/// 注入核（测试直调 tempdir home，零真实数据目录接触；env 重定向恒 None）
pub fn read_session_messages_with(
    home: &Path,
    agent_type: &str,
    session_id: &str,
    limit: usize,
) -> Result<MessagesPage, String> {
    read_session_messages_impl(home, None, None, agent_type, session_id, limit)
}

/// 派发核（env 双参注入，dsh/kimi 消费；kimi 的 resolve_data_root 双参模式同款）。
/// `pub(crate)`（终审 Important 1）：files.rs 的 env 注入核（extract_file_paths_with_env）
/// 同源复用——文件面板提取与 /session-messages 走同一派发路径
pub(crate) fn read_session_messages_impl(
    home: &Path,
    dsh_env_home: Option<&str>,
    kimi_env_home: Option<&str>,
    agent_type: &str,
    session_id: &str,
    limit: usize,
) -> Result<MessagesPage, String> {
    // 防御：空 session_id 直接拒绝（各工具文件名/SQL 均以 id 拼 key，空串会形成
    // ".jsonl" 这类危险形态）；路径分隔符与 ".." 拒绝——claude/workbuddy 的文件
    // 定位是 `目录.join(format!("{sid}.jsonl"))`，穿越字符会逃出项目根（端点在
    // 设备门禁之后，但内容层自身 fail-closed）；limit 夹取 [1, 1000]
    if session_id.trim().is_empty() {
        return Err("CONTENT_SESSION_ID_EMPTY: session_id 为空".to_string());
    }
    if session_id.contains(['/', '\\', '\0']) || session_id.contains("..") {
        return Err(format!(
            "CONTENT_SESSION_ID_INVALID: session_id 含非法字符: {session_id}"
        ));
    }
    let limit = limit.clamp(1, 1000);
    match agent_type {
        "zcode" => read_zcode_messages_with(home, session_id, limit),
        "dsh" => read_dsh_messages_with(home, dsh_env_home, session_id, limit),
        "claude" => read_claude_messages_with(home, session_id, limit),
        "codex" => read_codex_messages_with(home, session_id, limit),
        "kimi" => read_kimi_messages_with(home, kimi_env_home, session_id, limit),
        "workbuddy" => read_workbuddy_messages_with(home, session_id, limit),
        "opencode" => read_opencode_messages_with(home, session_id, limit),
        "openclaw" => read_openclaw_messages_with(home, session_id, limit),
        _ => Err(format!("CONTENT_UNKNOWN_TOOL: 未知工具: {agent_type}")),
    }
}

// ============================================================
// 公共小件
// ============================================================

/// 尾部截取 + seq 重排：取**文件序尾部**至多 limit 条（更早的内容由前端以更大
/// limit 重拉——Task 7 裁决：M3 不做 before 游标），seq 用返回数组内全局递增号
fn finalize(mut msgs: Vec<SessionMessage>, limit: usize) -> Vec<SessionMessage> {
    if msgs.len() > limit {
        msgs = msgs.split_off(msgs.len() - limit);
    }
    for (i, m) in msgs.iter_mut().enumerate() {
        m.seq = i as i64;
    }
    msgs
}

/// 组页小件：finalize + 截断标记打包（各工具读取器统一出口）
fn page(msgs: Vec<SessionMessage>, limit: usize, truncated: bool) -> MessagesPage {
    MessagesPage {
        messages: finalize(msgs, limit),
        truncated,
        cwd: None,
    }
}

/// JSONL 行读取预算：映射后条数上限 limit 对应的行窗口（assistant 消息会展开为
/// 多条目，按 4 倍预留；下限 500 对齐各 parser 的 RECENT_LINES 口径）
fn line_budget(limit: usize) -> usize {
    (limit.saturating_mul(4)).clamp(500, 4096)
}

/// JSONL 字节窗预算（Bug 1，M3 验收）：512KB 基准按 limit 放大（每 200 条一档），
/// 封顶 4MB——「加载更早消息」以更大 limit 重拉时字节窗必须同放大，否则按钮
/// 拉不到更早内容（死功能根因之一）。limit 在派发核入口已夹 [1,1000]，此处
/// saturating 防御即可；封顶属纵深防御（limit=1000 时 2.5MB 未触顶）
fn byte_budget(limit: usize) -> u64 {
    const BASE: u64 = 512 * 1024;
    const CAP: u64 = 4 * 1024 * 1024;
    BASE.saturating_mul(limit.div_ceil(200) as u64).min(CAP)
}

/// ISO 8601 时间戳 → epoch 毫秒（claude/codex 行内 timestamp 形态）；解析失败 None
fn iso_to_ms(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.timestamp_millis())
}

/// 数组内 text 块拼接（content[]: [{type:"text",text:...}, ...] → 单串，空格连接）。
/// 元素不是对象或无 text 字段时跳过该元素
fn join_text_parts(v: &serde_json::Value) -> String {
    v.as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|c| c.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default()
}

/// content[] 里第一个非空字符串字段值（Claude tool_result 的 block.content 双形态：
/// 字符串直取；数组取 text 拼接）
fn content_block_text(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Array(_) => {
            let t = join_text_parts(v);
            (!t.is_empty()).then_some(t)
        }
        _ => None,
    }
}

/// TEXT/BLOB 双形态防御读取（ZCode/OpenCode 私有库类型不保证；zcode_parser 同款）
fn sqlite_text(row: &rusqlite::Row<'_>, idx: usize) -> rusqlite::Result<String> {
    use rusqlite::types::ValueRef;
    match row.get_ref(idx)? {
        ValueRef::Text(t) => Ok(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => Ok(String::from_utf8_lossy(b).into_owned()),
        other => Err(rusqlite::Error::FromSqlConversionFailure(
            idx,
            rusqlite::types::Type::Text,
            Box::new(rusqlite::Error::InvalidColumnType(
                idx,
                "text/blob".to_string(),
                other.data_type(),
            )),
        )),
    }
}

/// 工具摘要文案：有名给「调用 <名>」，无名兜底「工具调用」
fn tool_summary(name: Option<&str>) -> String {
    match name {
        Some(n) => format!("调用 {n}"),
        None => "工具调用".to_string(),
    }
}

// ============================================================
// ZCode：~/.zcode/cli/db/db.sqlite（message + part 表）
// ============================================================

/// 在 <tool_dir>/projects/*/ 下按 session_id 定位 .jsonl 文件（claude / workbuddy 共用）
fn find_jsonl_in_projects(
    home: &std::path::Path,
    tool_dir: &str,
    session_id: &str,
) -> Result<std::path::PathBuf, String> {
    let projects = home.join(tool_dir).join("projects");
    let Ok(entries) = std::fs::read_dir(&projects) else {
        return Err(format!(
            "CONTENT_PROJECTS_DIR_MISSING: {tool_dir} projects 目录不存在"
        ));
    };
    for dir in entries.flatten() {
        let candidate = dir.path().join(format!("{session_id}.jsonl"));
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(format!(
        "CONTENT_SESSION_FILE_MISSING: 会话文件不存在: {session_id}"
    ))
}

fn read_zcode_messages_with(
    home: &Path,
    session_id: &str,
    limit: usize,
) -> Result<MessagesPage, String> {
    use crate::monitor::zcode_parser::ZcodeRoots;
    let roots = ZcodeRoots::from_home(home);
    let conn = crate::monitor::sqlite::open_readonly_with_timeout(&roots.cli_db)
        .ok_or_else(|| "ZCode cli db 不可读".to_string())?;

    // 顺序列探测（load_tail_messages 同款）：sequence 存在按流顺序（懒落库时间戳
    // 不可靠、顺序可靠），否则降级 time_created；两列皆无（升级改表）→ Err
    let order_col = if conn.prepare("SELECT sequence FROM message LIMIT 0").is_ok() {
        "sequence"
    } else if conn
        .prepare("SELECT time_created FROM message LIMIT 0")
        .is_ok()
    {
        "time_created"
    } else {
        return Err("CONTENT_ZCODE_ORDER_COLUMN_MISSING: ZCode message 表缺顺序列".to_string());
    };
    // LIMIT 为 usize 格式化（无注入面），session_id 走 ?1 参数化
    let sql = format!(
        "SELECT id, data, time_created FROM message WHERE session_id = ?1 \
         ORDER BY {order_col} DESC LIMIT {limit}"
    );
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| format!("CONTENT_ZCODE_QUERY_FAIL: ZCode 查询失败: {e}"))?;
    let rows = stmt
        .query_map([session_id], |row| {
            Ok((
                sqlite_text(row, 0)?,
                sqlite_text(row, 1)?,
                row.get::<_, Option<i64>>(2)?,
            ))
        })
        .map_err(|e| format!("CONTENT_ZCODE_QUERY_FAIL: ZCode 查询失败: {e}"))?;
    // 查询取「最新在前」，反转为文件序（旧 → 新）
    let mut messages: Vec<(String, String, Option<i64>)> = rows.filter_map(|r| r.ok()).collect();
    messages.reverse();
    // 会话不存在（0 行）→ Err（404 语义），与「存在但无消息条目」区分
    if messages.is_empty() {
        return Err(format!(
            "CONTENT_ZCODE_SESSION_MISSING: ZCode 会话不存在: {session_id}"
        ));
    }

    // parts 按 message_id 分组（IN 子句单查询，保序同 load_parts_for_messages）
    let parts = zcode_parts_by_message(&conn, &messages)?;

    let mut out = Vec::new();
    for (id, data, ts) in &messages {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(data) else {
            continue; // data 损坏 → 跳过该消息（防御）
        };
        let role = v.get("role").and_then(|r| r.as_str()).unwrap_or_default();
        let semantic = v
            .pointer("/semantics/kind")
            .and_then(|k| k.as_str())
            .unwrap_or_default();
        if matches!(
            (role, semantic),
            ("user", "todo_reminder") | ("user", "timeline_event")
        ) {
            continue; // 记账消息整条跳过（zcode_parser flatten_entries 同口径）
        }
        let msg_parts = parts.get(id);
        match role {
            "user" => {
                // 真实用户消息：text parts 拼接（实测 user 消息带 type=text part）
                let text = msg_parts
                    .map(|ps| zcode_text_of(ps, "text"))
                    .unwrap_or_default();
                if !text.is_empty() {
                    out.push(SessionMessage::text("user", text, *ts));
                }
            }
            "assistant" => {
                let Some(ps) = msg_parts else { continue };
                for p in ps {
                    let ptype = p.get("type").and_then(|t| t.as_str()).unwrap_or_default();
                    match ptype {
                        "text" => {
                            let t = p
                                .get("text")
                                .and_then(|t| t.as_str())
                                .unwrap_or_default()
                                .trim();
                            if !t.is_empty() {
                                out.push(SessionMessage::text("assistant", t, *ts));
                            }
                        }
                        "reasoning" => {
                            let t = p
                                .get("text")
                                .and_then(|t| t.as_str())
                                .unwrap_or_default()
                                .trim();
                            if !t.is_empty() {
                                out.push(SessionMessage::text("thinking", t, *ts));
                            }
                        }
                        "tool" => {
                            // 实测形态 {"type":"tool","tool":"Skill","state":{"input":{...}}}
                            let name = p
                                .get("tool")
                                .and_then(|t| t.as_str())
                                .or_else(|| p.get("name").and_then(|t| t.as_str()));
                            let args = p
                                .pointer("/state/input")
                                .filter(|v| !v.is_null())
                                .and_then(|v| serde_json::to_string(v).ok());
                            out.push(SessionMessage::tool_call(
                                tool_summary(name),
                                *ts,
                                name.map(String::from),
                                args,
                            ));
                        }
                        // step-start / step-finish / timeline / file / compaction 是
                        // 步进边界与记账标记，无内容载荷，不产出条目（接口 kind 集无
                        // turn-start/turn-end 形态——Task 7 修订，见报告）
                        _ => {}
                    }
                }
            }
            _ => {} // 未知角色 → 跳过
        }
    }
    // SQLite 查询自带 LIMIT：无「头部截断」语义（truncated 恒 false，Bug 1 契约）
    Ok(page(out, limit, false))
}

/// M5 B2：zcode 用户附件第二抽取源——窗口内消息的 part 表 `type=file` 行
/// （B1 调研：`source.path` = 原始落盘路径；无 source 时 `url` = artifact URI，
/// 由 files.rs 解析到 `~/.zcode/cli/artifacts/` 磁盘实体）。
/// 返回 (窗口序号, 路径或 URI, 消息时间戳)——序号与 `read_zcode_messages_with`
/// 同一窗口查询（同序同 LIMIT），与 SessionMessage.seq 同刻度，合并排序语义一致。
/// 库不可读 / 会话不存在 → 空表（附件是增强能力，与提取失败同口径）
pub(crate) fn read_zcode_attachment_refs_with(
    home: &Path,
    session_id: &str,
    limit: usize,
) -> Vec<(i64, String, Option<i64>)> {
    use crate::monitor::zcode_parser::ZcodeRoots;
    let roots = ZcodeRoots::from_home(home);
    let Some(conn) = crate::monitor::sqlite::open_readonly_with_timeout(&roots.cli_db) else {
        return Vec::new();
    };
    // 顺序列探测：与 read_zcode_messages_with 同款（sequence 优先，time_created 兜底）
    let order_col = if conn.prepare("SELECT sequence FROM message LIMIT 0").is_ok() {
        "sequence"
    } else if conn
        .prepare("SELECT time_created FROM message LIMIT 0")
        .is_ok()
    {
        "time_created"
    } else {
        return Vec::new();
    };
    let sql = format!(
        "SELECT id, time_created FROM message WHERE session_id = ?1 \
         ORDER BY {order_col} DESC LIMIT {limit}"
    );
    let Ok(mut stmt) = conn.prepare(&sql) else {
        return Vec::new();
    };
    let Ok(rows) = stmt.query_map([session_id], |row| {
        Ok((sqlite_text(row, 0)?, row.get::<_, Option<i64>>(1)?))
    }) else {
        return Vec::new();
    };
    // 查询取「最新在前」，反转为文件序（旧 → 新）——序号与 finalize 的 seq 同刻度
    let mut msgs: Vec<(String, Option<i64>)> = rows.filter_map(|r| r.ok()).collect();
    msgs.reverse();
    if msgs.is_empty() {
        return Vec::new();
    }
    let part_keys: Vec<(String, String, Option<i64>)> = msgs
        .iter()
        .map(|(id, ts)| (id.clone(), String::new(), *ts))
        .collect();
    let Ok(parts) = zcode_parts_by_message(&conn, &part_keys) else {
        return Vec::new();
    };
    let mut out: Vec<(i64, String, Option<i64>)> = Vec::new();
    for (ordinal, (id, ts)) in msgs.iter().enumerate() {
        let Some(ps) = parts.get(id) else { continue };
        for p in ps {
            let ptype = p.get("type").and_then(|t| t.as_str()).unwrap_or_default();
            if ptype != "file" {
                continue;
            }
            // 优先原始落盘路径（source.path）；无则 artifact URI（调用方解析磁盘实体）
            let r = p
                .pointer("/source/path")
                .and_then(|v| v.as_str())
                .or_else(|| p.get("url").and_then(|v| v.as_str()));
            if let Some(r) = r {
                out.push((ordinal as i64, r.to_string(), *ts));
            }
        }
    }
    out
}

/// ZCode：一组消息的全部 parts（IN 子句，sequence 列存在则保流顺序，否则 rowid）
fn zcode_parts_by_message(
    conn: &rusqlite::Connection,
    messages: &[(String, String, Option<i64>)],
) -> Result<std::collections::HashMap<String, Vec<serde_json::Value>>, String> {
    let mut map: std::collections::HashMap<String, Vec<serde_json::Value>> =
        std::collections::HashMap::new();
    if messages.is_empty() {
        return Ok(map);
    }
    let order_col = if conn.prepare("SELECT sequence FROM part LIMIT 0").is_ok() {
        "sequence"
    } else {
        "rowid"
    };
    let placeholders: Vec<String> = (0..messages.len()).map(|i| format!("?{}", i + 1)).collect();
    let sql = format!(
        "SELECT message_id, data FROM part WHERE message_id IN ({}) ORDER BY {order_col}",
        placeholders.join(",")
    );
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| format!("CONTENT_ZCODE_PARTS_QUERY_FAIL: ZCode parts 查询失败: {e}"))?;
    let ids: Vec<&str> = messages.iter().map(|(id, _, _)| id.as_str()).collect();
    let params: Vec<&dyn rusqlite::types::ToSql> = ids
        .iter()
        .map(|id| id as &dyn rusqlite::types::ToSql)
        .collect();
    let rows = stmt
        .query_map(params.as_slice(), |row| {
            Ok((sqlite_text(row, 0)?, sqlite_text(row, 1)?))
        })
        .map_err(|e| format!("CONTENT_ZCODE_PARTS_QUERY_FAIL: ZCode parts 查询失败: {e}"))?;
    for row in rows.filter_map(|r| r.ok()) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&row.1) {
            map.entry(row.0).or_default().push(v);
        }
    }
    Ok(map)
}

/// ZCode：parts 里指定 type 的 text 字段拼接（user 消息正文）
fn zcode_text_of(parts: &[serde_json::Value], ptype: &str) -> String {
    parts
        .iter()
        .filter(|p| p.get("type").and_then(|t| t.as_str()) == Some(ptype))
        .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
        .collect::<Vec<_>>()
        .join(" ")
}

// ============================================================
// dsh：~/.dsh/sessions/<project>/<session-dir>/session.vN.jsonl.zstd
// ============================================================

fn read_dsh_messages_with(
    home: &Path,
    env_home: Option<&str>,
    session_id: &str,
    limit: usize,
) -> Result<MessagesPage, String> {
    use crate::monitor::dsh::log;
    // 数据根（fix round 1 Important 1）：$DSH_HOME 覆盖优先（M0 F14：env 非空 >
    // ~/.dsh），与看板生产扫描 scan_sessions(&dsh_home()) **同源**——否则设了
    // DSH_HOME 的机器上会话卡在板、详情 404。env 值由生产薄壳作参数传入
    // （dsh_home_with 的双参注入同款），测试传 None 走回落分支，零真实 env 接触
    let dsh_root = match env_home {
        Some(v) if !v.trim().is_empty() => std::path::PathBuf::from(v),
        _ => home.join(".dsh"),
    };
    let sessions_root = dsh_root.join("sessions");
    let Ok(entries) = std::fs::read_dir(&sessions_root) else {
        return Err("CONTENT_DSH_DIR_MISSING: dsh sessions 目录不存在".to_string());
    };
    // 目录名是转义形态不可反解 id（M0 评审漏项 #3）——必须逐会话读 header 匹配
    for project_dir in entries.flatten() {
        let ppath = project_dir.path();
        if !ppath.is_dir() {
            continue;
        }
        let Ok(session_dirs) = std::fs::read_dir(&ppath) else {
            continue;
        };
        for sdir in session_dirs.flatten() {
            let spath = sdir.path();
            if !spath.is_dir() {
                continue;
            }
            // 代际选择：取版本最大者（scan_sessions 同款）；读不到（0 字节/损坏）跳过。
            // 先探测代际存在（与 read_best_generation 重复枚举，但让「无代际文件」与
            // 「解码失败」同走 continue，分支语义清晰）
            if log::generation_logs(&spath).is_empty() {
                continue;
            }
            let Some(read) = log::read_best_generation(&spath) else {
                continue;
            };
            let Some(header) = log::parse_header(&read.text) else {
                continue;
            };
            if header.id != session_id {
                continue;
            }
            // 命中：事件流映射（纯函数，独立可测）
            let events = log::parse_events(&read.text);
            // dsh 走自有 zstd 代际整读，无字节尾窗（truncated 恒 false，Bug 1 契约）
            return Ok(page(map_dsh_events(&events), limit, false));
        }
    }
    Err(format!(
        "CONTENT_DSH_SESSION_MISSING: dsh 会话不存在或日志不可读: {session_id}"
    ))
}

/// dsh 事件流 → 统一条目（纯函数）。跳过注入与记账事件（M0 F8：user/message 仅
/// source.kind=="user" 是真人；system/message 是插件注入）。
/// 产出映射：assistant/message.content[] 的 text→assistant、reasoning→thinking、
/// tool-call→tool-call；tool/call→tool-call；tool/result→tool-result。
/// 同一调用在事件流里两路呈现（assistant 内嵌块 `id` + 独立 tool/call `callId`，
/// 黄金样本实证同 id）→ 按调用 id 去重保留先出现者；id 缺失保守保留（无键判等，
/// 去重有误杀真实并行调用的风险）（fix round 1 Important 2）
fn map_dsh_events(events: &[crate::monitor::dsh::log::DshEvent]) -> Vec<SessionMessage> {
    let mut out = Vec::new();
    // 已出条的调用 id 集（内嵌块 id 与 tool/call callId 同一 id 空间）
    let mut seen_calls: std::collections::HashSet<String> = std::collections::HashSet::new();
    for e in events {
        let ts = e.time;
        match e.kind.as_str() {
            "user/message" => {
                if e.data.pointer("/source/kind").and_then(|v| v.as_str()) != Some("user") {
                    continue; // agent-instructions / plugin / skill-catalog 注入，过滤（M0 F8）
                }
                let text =
                    join_text_parts(e.data.get("content").unwrap_or(&serde_json::Value::Null));
                if !text.trim().is_empty() {
                    out.push(SessionMessage::text("user", text, ts));
                }
            }
            "assistant/message" => {
                let content = e
                    .data
                    .pointer("/message/content")
                    .unwrap_or(&serde_json::Value::Null);
                let Some(arr) = content.as_array() else {
                    continue;
                };
                for c in arr {
                    match c.get("type").and_then(|t| t.as_str()).unwrap_or_default() {
                        "text" => {
                            let t = c.get("text").and_then(|t| t.as_str()).unwrap_or_default();
                            if !t.trim().is_empty() {
                                out.push(SessionMessage::text("assistant", t, ts));
                            }
                        }
                        "reasoning" => {
                            let t = c.get("text").and_then(|t| t.as_str()).unwrap_or_default();
                            if !t.trim().is_empty() {
                                out.push(SessionMessage::text("thinking", t, ts));
                            }
                        }
                        "tool-call" => {
                            let name = c.get("name").and_then(|t| t.as_str());
                            // arguments 已是 JSON 字符串；防御非串形态（序列化兜底）
                            let args = match c.get("arguments") {
                                Some(serde_json::Value::String(s)) => Some(s.clone()),
                                Some(v) if !v.is_null() => serde_json::to_string(v).ok(),
                                _ => None,
                            };
                            // 同 id 去重（保留先出现者）；id 缺失保守保留
                            match c.get("id").and_then(|i| i.as_str()) {
                                Some(id) if !seen_calls.insert(id.to_string()) => continue,
                                _ => {}
                            }
                            out.push(SessionMessage::tool_call(
                                tool_summary(name),
                                ts,
                                name.map(String::from),
                                args,
                            ));
                        }
                        _ => {}
                    }
                }
            }
            "tool/call" => {
                // 独立工具调用事件：data.name + data.arguments（JSON 字符串）
                let name = e.data.get("name").and_then(|t| t.as_str());
                let args = match e.data.get("arguments") {
                    Some(serde_json::Value::String(s)) => Some(s.clone()),
                    Some(v) if !v.is_null() => serde_json::to_string(v).ok(),
                    _ => None,
                };
                // 同 id 去重（保留先出现者）；id 缺失保守保留
                match e.data.get("callId").and_then(|i| i.as_str()) {
                    Some(id) if !seen_calls.insert(id.to_string()) => continue,
                    _ => {}
                }
                out.push(SessionMessage::tool_call(
                    tool_summary(name),
                    ts,
                    name.map(String::from),
                    args,
                ));
            }
            "tool/result" => {
                // 实测形态：data.message.content[] = [{type:"tool-result", content:[{type:"text",...}]}]
                let blocks = e
                    .data
                    .pointer("/message/content")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                let mut texts = Vec::new();
                if let Some(arr) = blocks.as_array() {
                    for b in arr {
                        if let Some(t) = b.get("content").and_then(content_block_text) {
                            texts.push(t);
                        }
                    }
                }
                let text = texts.join(" ");
                out.push(SessionMessage::text(
                    "tool-result",
                    if text.is_empty() {
                        "工具结果".to_string()
                    } else {
                        text
                    },
                    ts,
                ));
            }
            // session / turn|step 边界 / approval / request / session/title /
            // system/message（插件注入）/ agent/inbox / permission / sandbox → 跳过
            _ => {}
        }
    }
    out
}

// ============================================================
// Claude：~/.claude/projects/<mangled-cwd>/<session-uuid>.jsonl
// ============================================================

fn read_claude_messages_with(
    home: &Path,
    session_id: &str,
    limit: usize,
) -> Result<MessagesPage, String> {
    let path = find_jsonl_in_projects(home, ".claude", session_id)?;
    // Bug 1：字节窗随 limit 放大（byte_budget），头部截断经 truncated 上报
    let (lines, truncated) = crate::monitor::jsonl::read_recent_lines_with_budget(
        &path,
        line_budget(limit),
        byte_budget(limit),
    );
    Ok(page(map_claude_lines(&lines), limit, truncated))
}

/// Claude JSONL 行 → 统一条目（纯函数）。行协议：type user/assistant +
/// message.content（字符串或块数组：text/thinking/tool_use/tool_result）。
/// isMeta 行是系统注入，跳过；summary/system/file-history-snapshot 等记账行跳过
fn map_claude_lines(lines: &[String]) -> Vec<SessionMessage> {
    let mut out = Vec::new();
    for line in lines {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if v.get("isMeta").and_then(|m| m.as_bool()) == Some(true) {
            continue; // 系统注入（Meta 行）
        }
        let ts = v
            .get("timestamp")
            .and_then(|t| t.as_str())
            .and_then(iso_to_ms);
        let msg_type = v.get("type").and_then(|t| t.as_str()).unwrap_or_default();
        let Some(content) = v.pointer("/message/content") else {
            continue;
        };
        match msg_type {
            "user" => match content {
                serde_json::Value::String(s) => {
                    if !s.trim().is_empty() {
                        out.push(SessionMessage::text("user", s.trim().to_string(), ts));
                    }
                }
                serde_json::Value::Array(blocks) => {
                    for b in blocks {
                        match b.get("type").and_then(|t| t.as_str()).unwrap_or_default() {
                            "text" => {
                                let t = b.get("text").and_then(|t| t.as_str()).unwrap_or_default();
                                if !t.trim().is_empty() {
                                    out.push(SessionMessage::text("user", t, ts));
                                }
                            }
                            "tool_result" => {
                                // content 双形态（字符串 / text 块数组）
                                let text = b
                                    .get("content")
                                    .and_then(content_block_text)
                                    .unwrap_or_else(|| "工具结果".to_string());
                                // T7：含计划文件引用 → 追加计划文件卡（形态判据跨工具）
                                out.extend(SessionMessage::tool_result_with_plan_ref(text, ts));
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            },
            "assistant" => {
                let Some(blocks) = content.as_array() else {
                    continue;
                };
                for b in blocks {
                    match b.get("type").and_then(|t| t.as_str()).unwrap_or_default() {
                        "text" => {
                            let t = b.get("text").and_then(|t| t.as_str()).unwrap_or_default();
                            if !t.trim().is_empty() {
                                out.push(SessionMessage::text("assistant", t, ts));
                            }
                        }
                        "thinking" => {
                            let t = b
                                .get("thinking")
                                .and_then(|t| t.as_str())
                                .unwrap_or_default();
                            if !t.trim().is_empty() {
                                out.push(SessionMessage::text("thinking", t, ts));
                            }
                        }
                        "tool_use" => {
                            let name = b.get("name").and_then(|t| t.as_str());
                            let args = b
                                .get("input")
                                .filter(|v| !v.is_null())
                                .and_then(|v| serde_json::to_string(v).ok());
                            out.push(SessionMessage::tool_call(
                                tool_summary(name),
                                ts,
                                name.map(String::from),
                                args,
                            ));
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// 子 agent 详情（观察台 §三，仅 claude）：会话 subagents/agent-<id>.jsonl 尾窗 →
/// `map_claude_lines` 复用（与会话消息同一映射——乙.3.1 实证两机制子转写格式一致，
/// 单一渲染器通吃）。发现层复用 `subagents::claude::locate`（同一 projects 扫描，
/// 不造第二份路径推导）；预算与 session-messages 同口径（line_budget/byte_budget）。
pub fn read_claude_subagent_messages(
    session_id: &str,
    subagent_id: &str,
    limit: usize,
) -> Result<MessagesPage, String> {
    let home = dirs::home_dir().ok_or_else(|| "无法确定用户主目录".to_string())?;
    read_claude_subagent_messages_with(&home, session_id, subagent_id, limit)
}

/// 注入核（测试 tempdir home）。**limit clamp 在本函数入口**（评审 P1-2）：既有
/// clamp 在 read_session_messages_impl（派发核）内，本新路径不经它——不自带的
/// 话 limit=0 会产出空消息表；与派发核同口径 [1,1000]。
pub(crate) fn read_claude_subagent_messages_with(
    home: &Path,
    session_id: &str,
    subagent_id: &str,
    limit: usize,
) -> Result<MessagesPage, String> {
    let limit = limit.clamp(1, 1000);
    let projects = home.join(".claude").join("projects");
    let Some((_parent_jsonl, subagents_dir)) =
        crate::monitor::subagents::claude::locate(&projects, session_id)
    else {
        return Err(format!("CONTENT_SESSION_MISSING: 会话不存在: {session_id}"));
    };
    let path = subagents_dir.join(format!("agent-{subagent_id}.jsonl"));
    if !path.is_file() {
        return Err(format!(
            "CONTENT_SUBAGENT_MISSING: 子 agent 转写不存在: {subagent_id}"
        ));
    }
    let (lines, truncated) = crate::monitor::jsonl::read_recent_lines_with_budget(
        &path,
        line_budget(limit),
        byte_budget(limit),
    );
    Ok(page(map_claude_lines(&lines), limit, truncated))
}

// ============================================================
// Codex：rollout JSONL（CLI 线路）→ 未命中回退 thread_history SQLite（APP 线路）
// ============================================================

fn read_codex_messages_with(
    home: &Path,
    session_id: &str,
    limit: usize,
) -> Result<MessagesPage, String> {
    // 路线 1：rollout JSONL（CLI 前端线路；两条线路共享同一会话 id 空间——
    // codex_thread_parser 以 rollout session id 排除 DB 侧重复出卡，即此结论）
    if let Ok(msgs) = read_codex_rollout_with(home, session_id, limit) {
        return Ok(msgs);
    }
    // 路线 2：thread_history_*.sqlite 内容投影（APP 线路；remote_control 活跃对话
    // 投影滞后属上游限制，此处如实返回已投影内容）
    if let Ok(msgs) = read_codex_thread_with(home, session_id, limit) {
        return Ok(msgs);
    }
    Err(format!(
        "codex 会话不存在（rollout 与 thread history 均未命中）: {session_id}"
    ))
}

/// rollout 递归收集（sessions/<y>/<m>/<d>/rollout-*.jsonl；codex_parser
/// is_rollout_file 判据的本地同款——原函数私有，判据三行不值得跨模块提权）
fn collect_rollout_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect_rollout_files(&p, out);
        } else if p
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.starts_with("rollout") && n.ends_with(".jsonl"))
            .unwrap_or(false)
        {
            out.push(p);
        }
    }
}

/// rollout 首行 session_meta → 会话 id（payload.id；缺失/损坏 None）
fn rollout_session_id(path: &Path) -> Option<String> {
    let first = crate::monitor::jsonl::read_first_lines(path, 1)
        .into_iter()
        .next()?;
    let v: serde_json::Value = serde_json::from_str(&first).ok()?;
    v.pointer("/payload/id")
        .and_then(|i| i.as_str())
        .map(String::from)
}

/// rollout 首行 session_meta → 会话工作目录（payload.cwd；缺失/损坏 None）。
/// 解析口径与 rollout_session_id 同款（同读首行 + 同指针取法）。**代价如实申报**：
/// 本批为最小改动**再读一次首行**（rollout_session_id 已读过一次，两次各自开文件
/// 读 1 行）；每请求 1 次、微秒级，代价可忽略
fn rollout_session_cwd(path: &Path) -> Option<String> {
    let first = crate::monitor::jsonl::read_first_lines(path, 1)
        .into_iter()
        .next()?;
    let v: serde_json::Value = serde_json::from_str(&first).ok()?;
    v.pointer("/payload/cwd")
        .and_then(|c| c.as_str())
        .map(String::from)
        .filter(|s| !s.is_empty())
}

fn read_codex_rollout_with(
    home: &Path,
    session_id: &str,
    limit: usize,
) -> Result<MessagesPage, String> {
    let sessions_dir = home.join(".codex").join("sessions");
    let mut files = Vec::new();
    collect_rollout_files(&sessions_dir, &mut files);
    if files.is_empty() {
        return Err("CONTENT_CODEX_ROLLOUT_EMPTY: codex rollout 目录为空".to_string());
    }
    // 快路径：rollout 文件名内嵌 session id（rollout-<ts>-<uuid>.jsonl，实测命名）；
    // 命中后仍读首行校验 payload.id，防同名巧合。快路径未中 → 全量首行匹配兜底
    // （历史会话文件名可能不含 id 的旧形态；单次请求路径，非轮询，可接受）
    let mut hit: Option<std::path::PathBuf> = None;
    if let Some(f) = files.iter().find(|f| {
        f.file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.contains(session_id))
            .unwrap_or(false)
    }) {
        if rollout_session_id(f).as_deref() == Some(session_id) {
            hit = Some(f.clone());
        }
    }
    if hit.is_none() {
        for f in &files {
            if rollout_session_id(f).as_deref() == Some(session_id) {
                hit = Some(f.clone());
                break;
            }
        }
    }
    let Some(f) = hit else {
        return Err(format!(
            "CONTENT_CODEX_ROLLOUT_MISSING: codex rollout 未命中: {session_id}"
        ));
    };
    // Bug 1：字节窗随 limit 放大，头部截断经 truncated 上报
    let (lines, truncated) = crate::monitor::jsonl::read_recent_lines_with_budget(
        &f,
        line_budget(limit),
        byte_budget(limit),
    );
    let mut pg = page(map_codex_lines(&lines), limit, truncated);
    // 会话产物批（2026-10-13）：cwd 作相对路径产物的解析基准。这里会**再读一次**
    // rollout 首行（rollout_session_id 已读过一次）——每请求 1 次、微秒级，代价可忽略
    pg.cwd = rollout_session_cwd(&f);
    Ok(pg)
}

/// Codex rollout 行 → 统一条目（纯函数；codex_entry_kind 的内容版映射）。
/// response_item 外壳：message(user/assistant)、function_call、function_call_output、
/// reasoning(summary)；session_meta / event_msg / turn_context 记账跳过
fn map_codex_lines(lines: &[String]) -> Vec<SessionMessage> {
    let mut out = Vec::new();
    for line in lines {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if v.get("type").and_then(|t| t.as_str()) != Some("response_item") {
            continue;
        }
        let ts = v
            .get("timestamp")
            .and_then(|t| t.as_str())
            .and_then(iso_to_ms);
        let Some(payload) = v.get("payload") else {
            continue;
        };
        match payload
            .get("type")
            .and_then(|t| t.as_str())
            .unwrap_or_default()
        {
            "message" => {
                let role = payload
                    .get("role")
                    .and_then(|r| r.as_str())
                    .unwrap_or_default();
                let text =
                    join_text_parts(payload.get("content").unwrap_or(&serde_json::Value::Null));
                // user / assistant 正文（developer 等系统角色跳过）。
                // T4：assistant 侧走 [`SessionMessage::assistant_text`]——含
                // `<proposed_plan>` 标签时升格一等计划卡（剥标签 + 前导文本保留）
                if matches!(role, "user" | "assistant") && !text.trim().is_empty() {
                    if role == "assistant" {
                        out.extend(SessionMessage::assistant_text(text, ts));
                    } else {
                        out.push(SessionMessage::text(role, text, ts));
                    }
                }
            }
            "function_call" => {
                let name = payload.get("name").and_then(|t| t.as_str());
                let args = payload
                    .get("arguments")
                    .and_then(|a| a.as_str())
                    .map(String::from);
                out.push(SessionMessage::tool_call(
                    tool_summary(name),
                    ts,
                    name.map(String::from),
                    args,
                ));
            }
            "custom_tool_call" => {
                // T5（手工验收修复批 D5）：apply_patch 补丁调用——从补丁**头行**提取
                // 文件变更（*** Add/Update/Delete File: <path>），args 对齐 APP 线路
                // fileChange 形态（{"changes":[{"op","path"}]}——files.rs PATH_KEYS
                // 的 "path" 键据此吸收入文件面板）。只解析头行、不碰补丁正文；旧
                // function_call+command 串「不收串内路径」防误收规则保持
                let name = payload.get("name").and_then(|t| t.as_str());
                let input = payload
                    .get("input")
                    .and_then(|i| i.as_str())
                    .unwrap_or_default();
                let mut changes: Vec<serde_json::Value> = Vec::new();
                let mut paths: Vec<String> = Vec::new();
                for line in input.lines() {
                    if let Some((op, path)) = parse_patch_header(line) {
                        paths.push(path.clone());
                        changes.push(serde_json::json!({ "op": op, "path": path }));
                    }
                }
                if !paths.is_empty() {
                    out.push(SessionMessage::tool_call(
                        format!("修改 {}", paths.join(", ")),
                        ts,
                        name.map(String::from),
                        serde_json::to_string(&changes).ok(),
                    ));
                }
            }
            "function_call_output" => {
                // output 双形态（字符串 / 对象）
                let text = match payload.get("output") {
                    Some(serde_json::Value::String(s)) => Some(s.clone()),
                    Some(v) if !v.is_null() => serde_json::to_string(v).ok(),
                    _ => None,
                }
                .unwrap_or_else(|| "工具结果".to_string());
                // T7：含计划文件引用 → 追加计划文件卡（codex 亦写 plans/x.md 形态）
                out.extend(SessionMessage::tool_result_with_plan_ref(text, ts));
            }
            "reasoning" => {
                // summary 元素双形态（实测纯字符串；亦容忍 {text} 对象）
                let text = payload
                    .get("summary")
                    .and_then(|s| s.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|i| {
                                i.as_str().map(String::from).or_else(|| {
                                    i.get("text").and_then(|t| t.as_str()).map(String::from)
                                })
                            })
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
                    .unwrap_or_default();
                if !text.trim().is_empty() {
                    out.push(SessionMessage::text("thinking", text, ts));
                }
            }
            _ => {}
        }
    }
    out
}

/// 解析 apply_patch 补丁头行：`*** Add File: <path>` / `*** Update File: <path>` /
/// `*** Delete File: <path>` → (op, path)；其余行（含补丁正文 +前缀行）→ None。
/// 只认这三个前缀——T5 防误收边界（路径带空格时取 `: ` 之后整段 trim）
fn parse_patch_header(line: &str) -> Option<(&'static str, String)> {
    let line = line.trim_end();
    for (prefix, op) in [
        ("*** Add File: ", "add"),
        ("*** Update File: ", "update"),
        ("*** Delete File: ", "delete"),
    ] {
        if let Some(path) = line.strip_prefix(prefix) {
            let path = path.trim();
            if !path.is_empty() {
                return Some((op, path.to_string()));
            }
        }
    }
    None
}

/// Codex APP 线路：thread_history_*.sqlite 的 thread_items（codex_thread_parser
/// 的表/列实测布局；item_type ∈ userMessage/agentMessage/commandExecution/
/// reasoning/fileChange/imageView/plan）
fn read_codex_thread_with(
    home: &Path,
    session_id: &str,
    limit: usize,
) -> Result<MessagesPage, String> {
    use crate::monitor::codex_thread_parser::CodexThreadRoots;
    let roots = CodexThreadRoots::from_home(home);
    let conn = crate::monitor::sqlite::open_readonly_with_timeout(&roots.history_db)
        .ok_or_else(|| "codex thread history db 不可读".to_string())?;
    let sql = format!(
        "SELECT item_type, item_json, created_at_ms FROM thread_items \
         WHERE thread_id = ?1 ORDER BY rollout_ordinal DESC LIMIT {}",
        line_budget(limit)
    );
    let Ok(mut stmt) = conn.prepare(&sql) else {
        return Err("CONTENT_CODEX_THREAD_ITEMS_FAIL: codex thread_items 表不可查".to_string());
    };
    let rows = stmt.query_map([session_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<i64>>(2)?,
        ))
    });
    let Ok(rows) = rows else {
        return Err(format!(
            "CONTENT_CODEX_THREAD_MISSING: codex thread 未命中: {session_id}"
        ));
    };
    let mut items: Vec<(String, String, Option<i64>)> = rows.filter_map(|r| r.ok()).collect();
    if items.is_empty() {
        return Err(format!(
            "CONTENT_CODEX_THREAD_MISSING: codex thread 未命中: {session_id}"
        ));
    }
    items.reverse(); // 「最新在前」→ 文件序
    let mut out = Vec::new();
    for (item_type, item_json, ts) in &items {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(item_json) else {
            continue;
        };
        match item_type.as_str() {
            "userMessage" => {
                let text = join_text_parts(v.get("content").unwrap_or(&serde_json::Value::Null));
                if !text.trim().is_empty() {
                    out.push(SessionMessage::text("user", text, *ts));
                }
            }
            "agentMessage" => {
                let t = v.get("text").and_then(|t| t.as_str()).unwrap_or_default();
                if !t.trim().is_empty() {
                    out.push(SessionMessage::text("assistant", t, *ts));
                }
            }
            "commandExecution" => {
                let cmd = v
                    .get("command")
                    .and_then(|t| t.as_str())
                    .unwrap_or_default();
                let args = serde_json::json!({ "command": cmd }).to_string();
                out.push(SessionMessage::tool_call(
                    if cmd.is_empty() {
                        "工具调用".to_string()
                    } else {
                        cmd.to_string()
                    },
                    *ts,
                    Some("exec".to_string()),
                    Some(args),
                ));
            }
            "reasoning" => {
                let text = v
                    .get("summary")
                    .and_then(|s| s.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|i| {
                                i.as_str().map(String::from).or_else(|| {
                                    i.get("text").and_then(|t| t.as_str()).map(String::from)
                                })
                            })
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
                    .unwrap_or_default();
                if !text.trim().is_empty() {
                    out.push(SessionMessage::text("thinking", text, *ts));
                }
            }
            "fileChange" => {
                let paths: Vec<String> = v
                    .get("changes")
                    .and_then(|c| c.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|c| c.get("path").and_then(|p| p.as_str()))
                            .map(String::from)
                            .collect()
                    })
                    .unwrap_or_default();
                if !paths.is_empty() {
                    let args =
                        serde_json::to_string(&v.get("changes").cloned().unwrap_or_default()).ok();
                    out.push(SessionMessage::tool_call(
                        format!("修改 {}", paths.join(", ")),
                        *ts,
                        Some("file".to_string()),
                        args,
                    ));
                }
            }
            // imageView / plan / 未知类型 → 跳过（codex_thread_parser 记账同口径）
            _ => {}
        }
    }
    // SQLite 查询自带 LIMIT：无「头部截断」语义（truncated 恒 false，Bug 1 契约）
    Ok(page(out, limit, false))
}

// ============================================================
// Kimi：session_index.jsonl 定位 + agents/main/wire.jsonl 事件流
// ============================================================

fn read_kimi_messages_with(
    home: &Path,
    env_home: Option<&str>,
    session_id: &str,
    limit: usize,
) -> Result<MessagesPage, String> {
    use crate::monitor::kimi_parser::{
        parse_session_index, resolve_data_root, resolve_session_dir,
    };
    // 数据根（fix round 1 Minor①）：KIMI_CODE_HOME env 值以**参数**传入
    // resolve_data_root（双参注入，kimi_parser 自用同款），生产薄壳读 env、
    // 测试传 None——内容层自身不读真实进程 env，与并行 env 测试无互斥交集
    let Some(root) = resolve_data_root(env_home, home) else {
        return Err("CONTENT_KIMI_ROOT_MISSING: kimi 数据根不存在".to_string());
    };
    let Some(entry) = parse_session_index(&root)
        .into_iter()
        .find(|e| e.session_id == session_id)
    else {
        return Err(format!(
            "CONTENT_KIMI_NOT_INDEXED: kimi 会话不在索引中: {session_id}"
        ));
    };
    let session_dir = resolve_session_dir(&root, &entry.session_dir);
    // 信任边界（stat_index_entry 同款）：sessionDir 必须落在 sessions 根之下
    if !session_dir.starts_with(&root.sessions) {
        return Err(format!(
            "CONTENT_KIMI_SESSIONDIR_ESCAPE: kimi sessionDir 越界: {session_id}"
        ));
    }
    let wire = session_dir.join("agents").join("main").join("wire.jsonl");
    // Bug 1：字节窗随 limit 放大，头部截断经 truncated 上报（kimi 实测 854KB 胖
    // 会话头部 39.5% 被旧 512KB 恒定窗切掉）
    let (lines, truncated) = crate::monitor::jsonl::read_recent_lines_with_budget(
        &wire,
        line_budget(limit),
        byte_budget(limit),
    );
    if lines.is_empty() {
        return Err(format!(
            "CONTENT_KIMI_WIRE_UNREADABLE: kimi wire.jsonl 不可读: {session_id}"
        ));
    }
    let msgs = map_kimi_lines(&lines);
    Ok(page(msgs, limit, truncated))
}

/// skill 装载注入标记（Bug 3，M3 验收）：kimi CLI 把 skill 装载全文写成
/// turn.steer 与 append_message(role=user) 两条 user 载荷（真实 wire 取证，内容
/// 全等）——映射前按标记跳过，对齐 claude 跳 isMeta / dsh 过滤 source.kind 注入
/// 的先例。真实用户输入不含此标记不受影响；短 tool.result（loaded inline）留在
/// 折叠块，无信息损失
fn is_kimi_skill_injection(text: &str) -> bool {
    text.contains("Skill tool loaded instructions for this request.")
        || text.contains("<skill-loaded ")
}

/// Kimi wire.jsonl 行 → 统一条目（纯函数；entry_text/entry_status 的内容版映射）。
/// 流式 content.part 事件会碎片化 → 连续同 kind 文本行合并为一条（tool 行为边界）；
/// turn.prompt 与 append_message(user) 是同一输入的两次落笔（实机取证）→ 连续全等去重
pub(crate) fn map_kimi_lines(lines: &[String]) -> Vec<SessionMessage> {
    let mut out: Vec<SessionMessage> = Vec::new();
    for line in lines {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let etype = v.get("type").and_then(|t| t.as_str()).unwrap_or_default();
        let ts = v.get("time").and_then(|t| t.as_i64());
        match etype {
            // 用户输入（turn 级事件，input[]: [{type:"text",text}]
            "turn.prompt" | "turn.steer" => {
                let text = v
                    .get("input")
                    .and_then(|i| i.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
                    .unwrap_or_default();
                if !text.trim().is_empty() && !is_kimi_skill_injection(&text) {
                    push_kimi(&mut out, SessionMessage::text("user", text, ts));
                }
            }
            "context.append_message" => {
                let msg = v.get("message");
                let role = msg
                    .and_then(|m| m.get("role"))
                    .and_then(|r| r.as_str())
                    .unwrap_or_default();
                let text = msg
                    .map(|m| join_text_parts(m.get("content").unwrap_or(&serde_json::Value::Null)))
                    .unwrap_or_default();
                // 注入过滤只作用于 user 侧（assistant 正文是模型输出，不做标记过滤）
                let injected = role == "user" && is_kimi_skill_injection(&text);
                if !text.trim().is_empty() && !injected {
                    let kind = if role == "user" { "user" } else { "assistant" };
                    push_kimi(&mut out, SessionMessage::text(kind, text, ts));
                }
            }
            "context.append_loop_event" => {
                let Some(event) = v.get("event") else {
                    continue;
                };
                match event
                    .get("type")
                    .and_then(|t| t.as_str())
                    .unwrap_or_default()
                {
                    "content.part" => {
                        let Some(part) = event.get("part") else {
                            continue;
                        };
                        match part
                            .get("type")
                            .and_then(|t| t.as_str())
                            .unwrap_or_default()
                        {
                            "text" => {
                                let t = part
                                    .get("text")
                                    .and_then(|t| t.as_str())
                                    .unwrap_or_default();
                                if !t.trim().is_empty() {
                                    push_kimi(&mut out, SessionMessage::text("assistant", t, ts));
                                }
                            }
                            "think" => {
                                // 实测 think 块的字段名是 think（非 text）
                                let t = part
                                    .get("think")
                                    .and_then(|t| t.as_str())
                                    .unwrap_or_default();
                                if !t.trim().is_empty() {
                                    push_kimi(&mut out, SessionMessage::text("thinking", t, ts));
                                }
                            }
                            _ => {}
                        }
                    }
                    "tool.call" => {
                        let name = event.get("name").and_then(|t| t.as_str());
                        // 实测 args 是对象（非 JSON 字符串）→ 序列化
                        let args = match event.get("args") {
                            Some(v) if !v.is_null() => serde_json::to_string(v).ok(),
                            _ => None,
                        };
                        out.push(SessionMessage::tool_call(
                            tool_summary(name),
                            ts,
                            name.map(String::from),
                            args,
                        ));
                    }
                    "tool.result" => {
                        // 实测 result 是对象：{"output": "..."}
                        let text = event
                            .pointer("/result/output")
                            .and_then(|o| o.as_str())
                            .map(String::from)
                            .or_else(|| match event.get("result") {
                                Some(s @ serde_json::Value::String(_)) => {
                                    s.as_str().map(String::from)
                                }
                                Some(v) if !v.is_null() => serde_json::to_string(v).ok(),
                                _ => None,
                            })
                            .unwrap_or_else(|| "工具结果".to_string());
                        // T7：含计划文件引用 → 追加计划文件卡（kimi 计划是一等文件，
                        // 消息流只有 "Wrote N bytes to …/plans/x.md" 一行——图1 诉求）
                        out.extend(SessionMessage::tool_result_with_plan_ref(text, ts));
                    }
                    _ => {} // step.begin/end 等边界 → 跳过
                }
            }
            // ===== 丁T2：kimi 计划审批（`interaction.request`）→ 计划双卡 =====
            //
            // 根因（问题 3 的正文半）：`interaction.*` 原在本 `_ =>` 分支整类跳过，
            // 于是 kimi 的计划**正文**（wire 的 `request.display.plan` 内联全文）从未
            // 进入消息流——终端里看得见、手机上看不见，违反裁1「计划不得以未渲染形态
            // 留在消息流里」。
            //
            // 数据形态（本机 wire 实录，`wd_proj-ki1_89e8318bc1ff`）：
            // ```json
            // {"type":"interaction.request","kind":"approval",
            //  "request":{"toolName":"ExitPlanMode","action":"Presenting plan and exiting plan mode",
            //   "display":{"kind":"plan_review","plan":"# Plan: …","path":"…/agents/main/plans/x.md"}}}
            // ```
            // `display.kind` 实测取值族：plan_review / file_io / command（另见 agent_call /
            // skill_call / todo_list / url_fetch / goal_start）——**只有 plan_review 是计划**
            // （file_io 的 path 是普通业务文件，拿它当计划文件会误出卡）。
            //
            // 产出（裁1 计划双卡矩阵：kimi 正文来源=**文件**，但 wire 已内联全文，
            // 故直接吃内联正文、无需读盘；文件卡给出可预览路径）：
            // - `display.plan` 非空白 → `kind="plan"`（正文卡，markdown 原文）；
            // - `display.path` 非空白 → **追加** `kind="plan-file"`（文件卡，前端「查看
            //   计划」按钮走既有文件预览；路径豁免见 files::EXEMPT_SUBPATHS 的
            //   agents/main/plans）；
            // - 两者皆缺 → 零产出（不产空卡，§2.8 兜底）。
            "interaction.request" => {
                if let Some((plan, path)) = kimi_plan_review_parts(&v) {
                    out.extend(kimi_plan_cards(plan.as_deref(), path.as_deref(), ts));
                }
            }
            // metadata / config.update / usage.record / turn.ended / interaction.resolved
            // 等跳过
            _ => {}
        }
    }
    out
}

/// 从 kimi `interaction.request` 行里抽计划双卡的原料（纯函数；**只认计划审批形态**）。
///
/// 判据（实测形态，勿凭想象扩面）：`kind == "approval"` ∧
/// `request.display.kind == "plan_review"`。命中后取 `display.plan`（内联正文）与
/// `display.path`（落盘路径）两个**可缺省**字段（各自 trim 后非空才算在场）。
///
/// 非计划审批（file_io / command）与问答（kind=question）一律 None——file_io 的
/// `path` 是业务文件，收它会让普通 Write 审批也冒出「计划文件卡」（误报）。
fn kimi_plan_review_parts(v: &serde_json::Value) -> Option<(Option<String>, Option<String>)> {
    if v.get("kind").and_then(|k| k.as_str()) != Some("approval") {
        return None;
    }
    let display = v.pointer("/request/display")?;
    if display.get("kind").and_then(|k| k.as_str()) != Some("plan_review") {
        return None;
    }
    // 在场判据用 trim（空白不算正文），但**下发原文不 trim**——与
    // [`SessionMessage::tool_call`] 的 claude 升格口径逐字一致（那边也是
    // trim 判空、原样下发），两家的正文卡内容口径不得漂移
    let raw = |key: &str| -> Option<String> {
        display
            .get(key)
            .and_then(|x| x.as_str())
            .filter(|s| !s.trim().is_empty())
            .map(String::from)
    };
    Some((raw("plan"), raw("path")))
}

/// 计划双卡构造（裁1「能上就都上」）：正文卡（`kind="plan"`，markdown 原文）在前、
/// 文件卡（`kind="plan-file"`，路径）在后；任一侧缺失则只出另一侧，两者皆缺零产出。
///
/// 与 claude/codex 的升格口径一致（content=计划原文、role=assistant、collapsed=false
/// ——一等卡默认展开），故前端渲染分支零新增（`SessionDetail.tsx` 的 plan / plan-file
/// 两个 case 直接接住）。
fn kimi_plan_cards(plan: Option<&str>, path: Option<&str>, ts: Option<i64>) -> Vec<SessionMessage> {
    let mut out = Vec::with_capacity(2);
    if let Some(plan) = plan {
        out.push(SessionMessage::plan_card(
            "plan",
            plan.to_string(),
            ts,
            None,
        ));
    }
    if let Some(path) = path {
        out.push(SessionMessage::plan_card(
            "plan-file",
            path.to_string(),
            ts,
            None,
        ));
    }
    out
}

/// Kimi 追加条目（连续合并 + 全等去重）：
/// - 与上一条同为文本类且同 kind → content 以换行拼接（流式碎片合并）；
/// - 与上一条全等（role+kind+content）→ 丢弃（turn.prompt 与 append_message 双写）。
/// - tool-call / tool-result 恒独立成条（天然是边界）
///
/// 合并只作用于 assistant / thinking（流式碎片）；两条不同内容的 user 消息是两次
/// 独立输入，保持独立条目
fn push_kimi(out: &mut Vec<SessionMessage>, msg: SessionMessage) {
    if let Some(last) = out.last_mut() {
        if last.kind == msg.kind && msg.tool_name.is_none() && last.tool_name.is_none() {
            if last.content == msg.content {
                return; // 全等去重（双写）
            }
            // 流式碎片合并仅限 agent 侧连续正文/思考；user 不合并（两次输入是两条）
            if matches!(msg.kind.as_str(), "assistant" | "thinking") {
                last.content.push('\n');
                last.content.push_str(&msg.content);
                return;
            }
        }
    }
    out.push(msg);
}

// ============================================================
// WorkBuddy：~/.workbuddy/projects/*/<sessionId>.jsonl（OpenAI 风格）
// ============================================================

fn read_workbuddy_messages_with(
    home: &Path,
    session_id: &str,
    limit: usize,
) -> Result<MessagesPage, String> {
    let path = find_jsonl_in_projects(home, ".workbuddy", session_id)?;
    // Bug 1：字节窗随 limit 放大，头部截断经 truncated 上报
    let (lines, truncated) = crate::monitor::jsonl::read_recent_lines_with_budget(
        &path,
        line_budget(limit),
        byte_budget(limit),
    );
    Ok(page(map_workbuddy_lines(&lines), limit, truncated))
}

/// WorkBuddy JSONL 行 → 统一条目（纯函数；workbuddy_entry_kind 的内容版映射）。
/// message(user/assistant) 正文；function_call → tool-call；function_call_result →
/// tool-result；reasoning / file-history-snapshot 等记账行跳过
fn map_workbuddy_lines(lines: &[String]) -> Vec<SessionMessage> {
    let mut out = Vec::new();
    for line in lines {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let vtype = v.get("type").and_then(|t| t.as_str()).unwrap_or_default();
        let ts = v
            .get("timestamp")
            .and_then(|t| t.as_str())
            .and_then(iso_to_ms);
        match vtype {
            "message" => {
                let role = v.get("role").and_then(|r| r.as_str()).unwrap_or_default();
                // content 双形态（数组 text 块为主，实测；字符串防御兼容）
                let text = match v.get("content") {
                    Some(c @ serde_json::Value::Array(_)) => join_text_parts(c),
                    Some(serde_json::Value::String(s)) => s.clone(),
                    _ => String::new(),
                };
                if !text.trim().is_empty() {
                    let kind = if role == "user" { "user" } else { "assistant" };
                    out.push(SessionMessage::text(kind, text, ts));
                }
            }
            "function_call" => {
                let name = v.get("name").and_then(|t| t.as_str());
                let args = v
                    .get("arguments")
                    .and_then(|a| a.as_str())
                    .map(String::from);
                out.push(SessionMessage::tool_call(
                    tool_summary(name),
                    ts,
                    name.map(String::from),
                    args,
                ));
            }
            "function_call_result" => {
                let text = match v.get("output") {
                    Some(serde_json::Value::String(s)) => Some(s.clone()),
                    Some(c @ serde_json::Value::Array(_)) => Some(join_text_parts(c)),
                    Some(vv) if !vv.is_null() => serde_json::to_string(vv).ok(),
                    _ => None,
                }
                .unwrap_or_else(|| "工具结果".to_string());
                // T7：含计划文件引用 → 追加计划文件卡（形态判据跨工具）
                out.extend(SessionMessage::tool_result_with_plan_ref(text, ts));
            }
            _ => {}
        }
    }
    out
}

// ============================================================
// OpenCode：~/.local/share/opencode/opencode.db（message + part 表）
// ============================================================

/// opencode 问答类部件的**终态**判定（丁T1 复评 F-3，纯函数可测）：
/// `type=="tool"` 且（`tool=="question"` ∨ `state.input` 满足 T3 问答形态）且
/// `state.status ∈ {"completed","error"}` → true（已答/被拒，销卡信号该补）。
///
/// 与 `monitor::opencode_parser::pending_question_part` 同口径的**互补面**：那边判
/// 「待决」（pending/running 且无 answers → 红灯），这边判「终态」（该补 tool-result
/// 让问答卡销掉）。两处判据必须同源同形——否则会出现「状态链说已答、端点说还在问」
/// 的自相矛盾（F-3 的根因教训）。复评 F-4 后形态分支统一收窄到 T3 口径
/// （`questions[]` 非空 + 元素含 question + options[] 非空 + 每项含 label）。
fn is_answered_question_part(part: &serde_json::Value) -> bool {
    if part.get("type").and_then(|t| t.as_str()) != Some("tool") {
        return false;
    }
    let state = part.get("state").unwrap_or(&serde_json::Value::Null);
    // 工具名双键（适配批评审 I1 同根因）：v1 键 `tool` / v2 content 元素键 `name`
    // （D0-1）——同函数上方的 tool_call 名字提取已双键，这里对齐，否则 v2 下
    // 非标形态问答永不销卡（F-3 回归）
    let tool_name = part
        .get("name")
        .and_then(|t| t.as_str())
        .or_else(|| part.get("tool").and_then(|t| t.as_str()));
    let by_name = tool_name == Some("question");
    let by_shape = state
        .pointer("/input/questions")
        .and_then(|q| q.as_array())
        .map(|arr| {
            !arr.is_empty()
                && arr.iter().all(|q| {
                    q.get("question").and_then(|x| x.as_str()).is_some()
                        && q.get("options")
                            .and_then(|o| o.as_array())
                            .map(|opts| {
                                !opts.is_empty()
                                    && opts
                                        .iter()
                                        .all(|o| o.get("label").and_then(|l| l.as_str()).is_some())
                            })
                            .unwrap_or(false)
                })
        })
        .unwrap_or(false);
    if !(by_name || by_shape) {
        return false;
    }
    matches!(
        state.get("status").and_then(|s| s.as_str()),
        Some("completed") | Some("error")
    )
}

fn read_opencode_messages_with(
    home: &Path,
    session_id: &str,
    limit: usize,
) -> Result<MessagesPage, String> {
    let db = home
        .join(".local")
        .join("share")
        .join("opencode")
        .join("opencode.db");
    let conn = crate::monitor::sqlite::open_readonly_with_timeout(&db)
        .ok_or_else(|| "opencode.db 不可读".to_string())?;
    // schema 分派（D1 定案 §D0-4）：2.x 会话/消息表改名 session_v2/session_message
    // 且 part 内联进 data.content[]；`session`/`message`/`part` 三表冻结（新消息只进
    // v2 表）。查旧表会读到迁移前的快照 → 新会话「不存在」/新消息读不到（create 物化
    // stamp 命不中即此）。表不存在则走 1.x 旧路径（零回归）。判据单点 =
    // monitor::opencode_parser::Schema::detect（评审 P2 收口：parser/content 双实现
    // 漂移会让状态与内容走不同 schema）
    if crate::monitor::opencode_parser::schema_is_v2(&conn) {
        return read_opencode_messages_v2(&conn, session_id, limit);
    }
    let sql = format!(
        "SELECT id, data, time_created FROM message WHERE session_id = ?1 \
         ORDER BY time_created DESC LIMIT {limit}"
    );
    let Ok(mut stmt) = conn.prepare(&sql) else {
        return Err("CONTENT_OPENCODE_MESSAGE_TABLE_FAIL: opencode message 表不可查".to_string());
    };
    let rows = stmt.query_map([session_id], |row| {
        Ok((
            sqlite_text(row, 0)?,
            sqlite_text(row, 1)?,
            row.get::<_, Option<i64>>(2)?,
        ))
    });
    let Ok(rows) = rows else {
        return Err(format!(
            "CONTENT_OPENCODE_QUERY_FAIL: opencode 查询失败: {session_id}"
        ));
    };
    let mut messages: Vec<(String, String, Option<i64>)> = rows.filter_map(|r| r.ok()).collect();
    if messages.is_empty() {
        return Err(format!(
            "CONTENT_OPENCODE_SESSION_MISSING: opencode 会话不存在: {session_id}"
        ));
    }
    messages.reverse(); // 文件序
    let mut out = Vec::new();
    for (id, data, ts) in &messages {
        let Ok(mv) = serde_json::from_str::<serde_json::Value>(data) else {
            continue;
        };
        let role = mv.get("role").and_then(|r| r.as_str()).unwrap_or_default();
        if role != "user" && role != "assistant" {
            continue;
        }
        // parts 按消息读取（opencode_parser get_message_text 同款：time_created ASC）
        let Ok(mut pstmt) =
            conn.prepare("SELECT data FROM part WHERE message_id = ?1 ORDER BY time_created ASC")
        else {
            continue;
        };
        let prows = pstmt.query_map([id], |row| sqlite_text(row, 0));
        let Ok(prows) = prows else { continue };
        for pdata in prows.filter_map(|r| r.ok()) {
            let Ok(pv) = serde_json::from_str::<serde_json::Value>(&pdata) else {
                continue;
            };
            match pv.get("type").and_then(|t| t.as_str()).unwrap_or_default() {
                "text" => {
                    let t = pv.get("text").and_then(|t| t.as_str()).unwrap_or_default();
                    if !t.trim().is_empty() {
                        out.push(SessionMessage::text(
                            if role == "user" { "user" } else { "assistant" },
                            t,
                            *ts,
                        ));
                    }
                }
                "reasoning" => {
                    let t = pv.get("text").and_then(|t| t.as_str()).unwrap_or_default();
                    if !t.trim().is_empty() {
                        out.push(SessionMessage::text("thinking", t, *ts));
                    }
                }
                "tool" => {
                    // 实测形态未定（本机库无 tool part 样本）——多字段名防御取值
                    let name = pv
                        .get("tool")
                        .and_then(|t| t.as_str())
                        .or_else(|| pv.get("name").and_then(|t| t.as_str()));
                    let args = pv
                        .pointer("/state/input")
                        .filter(|v| !v.is_null())
                        .and_then(|v| serde_json::to_string(v).ok());
                    out.push(SessionMessage::tool_call(
                        tool_summary(name),
                        *ts,
                        name.map(String::from),
                        args,
                    ));
                    // ===== 丁T1 复评 F-3：问答类部件的**销卡信号** =====
                    //
                    // 根因：本 reader 的 tool 分支**只产 tool-call、不产 tool-result**
                    // （其他 reader 都产：codex L1231 / claude / kimi 同款），而问答端点
                    // `question_scan_sync` 的销卡判据是「该 tool-call 之后**存在**
                    // tool-result」——对 opencode 该条件**恒假**，于是已答完的 opencode
                    // 问题仍判 available=true，问答卡永不消失（违反 T1 验收「回答后
                    // 回落 / 正常运行零误报」）。
                    //
                    // **只对问答类部件补**（`tool=="question"` ∨ `state.input` 含非空
                    // `questions[]` 形态——与 monitor::opencode_parser::pending_question_part
                    // 同口径），且只在**终态**（`state.status ∈ {completed, error}`）补：
                    // - 不动其它工具（bash/read/write/…）：它们的 tool-result 缺失是既有
                    //   形态，补上会改变消息流的渲染与 T7 计划文件引用链（超出本任务面）；
                    // - 待决期间（pending/running）**不补**：否则问答卡会在待决窗口内被
                    //   自己误销（这正是 F-3 测试要锁的反面）。
                    // 内容取 `state.output`（缺失给兜底文案）——问答端点的销卡判据只看
                    // 「存在一条 tool-result」，不解析其内容。
                    if is_answered_question_part(&pv) {
                        let text = pv
                            .pointer("/state/output")
                            .map(|o| match o {
                                serde_json::Value::String(s) => s.clone(),
                                v if !v.is_null() => serde_json::to_string(v).unwrap_or_default(),
                                _ => String::new(),
                            })
                            .filter(|s| !s.trim().is_empty())
                            .unwrap_or_else(|| "问题已作答".to_string());
                        out.push(SessionMessage::text("tool-result", text, *ts));
                    }
                }
                // step-start / patch / file / 未知 → 跳过
                _ => {}
            }
        }
    }
    // SQLite 查询自带 LIMIT：无「头部截断」语义（truncated 恒 false，Bug 1 契约）
    Ok(page(out, limit, false))
}

/// 2.x（`session_v2`/`session_message`）消息读取（D1 定案 §D0-1）。
///
/// 与 1.x 路径的形态差异：
/// - 角色来自 `session_message.type` 列（1.x 是 `message.data.role`）；
/// - 内容内联在 `data.content[]`（1.x 分 `part` 表）；元素 type = text/reasoning/tool，
///   **工具名键为 `name`**（1.x 是 `tool`，本函数两键兼容取值）；
/// - `idle`/`system`/`synthetic` 类型消息**无对话内容**，跳过（与 1.x 的
///   role 过滤同效）。
///
/// 产出的 SessionMessage 种类与 1.x 路径逐一同形（text/thinking/tool_call/
/// tool-result）——尤其**问答类 tool 的销卡 tool-result 必须继续产出**（丁T1 复评
/// F-3 根因：该 reader 不产 tool-result 时已答的 opencode 问答卡永不消失）。
fn read_opencode_messages_v2(
    conn: &rusqlite::Connection,
    session_id: &str,
    limit: usize,
) -> Result<MessagesPage, String> {
    let sql = format!(
        "SELECT type, data, time_created FROM session_message WHERE session_id = ?1 \
         ORDER BY seq DESC LIMIT {limit}"
    );
    let Ok(mut stmt) = conn.prepare(&sql) else {
        return Err("CONTENT_OPENCODE_MESSAGE_TABLE_FAIL: opencode message 表不可查".to_string());
    };
    let rows = stmt.query_map([session_id], |row| {
        Ok((
            sqlite_text(row, 0)?,
            sqlite_text(row, 1)?,
            row.get::<_, Option<i64>>(2)?,
        ))
    });
    let Ok(rows) = rows else {
        return Err(format!(
            "CONTENT_OPENCODE_QUERY_FAIL: opencode 查询失败: {session_id}"
        ));
    };
    let mut messages: Vec<(String, String, Option<i64>)> = rows.filter_map(|r| r.ok()).collect();
    if messages.is_empty() {
        return Err(format!(
            "CONTENT_OPENCODE_SESSION_MISSING: opencode 会话不存在: {session_id}"
        ));
    }
    messages.reverse(); // seq 升序（文件序）
    let mut out = Vec::new();
    for (mtype, data, ts) in &messages {
        // 只有 user/assistant 承载对话内容（idle/system/synthetic 无 content）
        let role = match mtype.as_str() {
            "user" | "assistant" => mtype.as_str(),
            _ => continue,
        };
        let Ok(mv) = serde_json::from_str::<serde_json::Value>(data) else {
            continue;
        };
        // user 消息：文本在 data.text（1.x 同）；assistant：内容在 data.content[]
        if role == "user" {
            if let Some(t) = mv.get("text").and_then(|t| t.as_str()) {
                if !t.trim().is_empty() {
                    out.push(SessionMessage::text("user", t, *ts));
                }
            }
            continue;
        }
        let Some(content) = mv.get("content").and_then(|c| c.as_array()) else {
            continue;
        };
        for c in content {
            match c.get("type").and_then(|t| t.as_str()).unwrap_or_default() {
                "text" => {
                    let t = c.get("text").and_then(|t| t.as_str()).unwrap_or_default();
                    if !t.trim().is_empty() {
                        out.push(SessionMessage::text("assistant", t, *ts));
                    }
                }
                "reasoning" => {
                    let t = c.get("text").and_then(|t| t.as_str()).unwrap_or_default();
                    if !t.trim().is_empty() {
                        out.push(SessionMessage::text("thinking", t, *ts));
                    }
                }
                "tool" => {
                    // 2.x 工具名键为 `name`（1.x 为 `tool`）——两键兼容取值
                    let name = c
                        .get("name")
                        .and_then(|t| t.as_str())
                        .or_else(|| c.get("tool").and_then(|t| t.as_str()));
                    let args = c
                        .pointer("/state/input")
                        .filter(|v| !v.is_null())
                        .and_then(|v| serde_json::to_string(v).ok());
                    out.push(SessionMessage::tool_call(
                        tool_summary(name),
                        *ts,
                        name.map(String::from),
                        args,
                    ));
                    // 问答类终态 → 补 tool-result 销卡（与 1.x 路径同判据、同形）
                    if is_answered_question_part(c) {
                        let text = c
                            .pointer("/state/output")
                            .map(|o| match o {
                                serde_json::Value::String(s) => s.clone(),
                                v if !v.is_null() => serde_json::to_string(v).unwrap_or_default(),
                                _ => String::new(),
                            })
                            .filter(|s| !s.trim().is_empty())
                            .unwrap_or_else(|| "问题已作答".to_string());
                        out.push(SessionMessage::text("tool-result", text, *ts));
                    }
                }
                _ => {}
            }
        }
    }
    Ok(page(out, limit, false))
}

// ============================================================
// OpenClaw：~/.openclaw/state/openclaw.sqlite 的 acp_replay_events（ACP 协议
// replay 日志，2026-09-15 实机探测：session_id/seq/at/update_json，
// update_json 为 ACP sessionUpdate 结构）。无该存储 → Err 优雅降级（Task 7 裁决）
// ============================================================

const OPENCLAW_NO_HISTORY: &str = "OpenClaw 暂无本地消息历史可读";

fn read_openclaw_messages_with(
    home: &Path,
    session_id: &str,
    limit: usize,
) -> Result<MessagesPage, String> {
    let db = home.join(".openclaw").join("state").join("openclaw.sqlite");
    if !db.exists() {
        return Err(OPENCLAW_NO_HISTORY.to_string());
    }
    let Some(conn) = crate::monitor::sqlite::open_readonly_with_timeout(&db) else {
        return Err(OPENCLAW_NO_HISTORY.to_string());
    };
    let sql = format!(
        "SELECT seq, at, update_json FROM acp_replay_events \
         WHERE session_id = ?1 ORDER BY seq ASC LIMIT {}",
        line_budget(limit)
    );
    let Ok(mut stmt) = conn.prepare(&sql) else {
        return Err(OPENCLAW_NO_HISTORY.to_string()); // 表缺失（旧版存储布局）
    };
    let rows = stmt.query_map([session_id], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, Option<i64>>(1)?,
            sqlite_text(row, 2)?,
        ))
    });
    let Ok(rows) = rows else {
        return Err(OPENCLAW_NO_HISTORY.to_string());
    };
    let events: Vec<(i64, Option<i64>, String)> = rows.filter_map(|r| r.ok()).collect();
    if events.is_empty() {
        return Err(OPENCLAW_NO_HISTORY.to_string());
    }
    let mut out = Vec::new();
    for (_seq, at, update_json) in &events {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(update_json) else {
            continue;
        };
        let update = v
            .get("sessionUpdate")
            .and_then(|u| u.as_str())
            .unwrap_or_default();
        // ACP ContentBlock 单对象形态 {type:"text",text}；防御数组变体
        let block_text = |v: &serde_json::Value| -> String {
            match v {
                serde_json::Value::Array(_) => join_text_parts(v),
                other => other
                    .get("text")
                    .and_then(|t| t.as_str())
                    .unwrap_or_default()
                    .to_string(),
            }
        };
        match update {
            "user_message_chunk" => {
                let t = v.get("content").map(block_text).unwrap_or_default();
                if !t.trim().is_empty() {
                    out.push(SessionMessage::text("user", t, *at));
                }
            }
            "agent_message_chunk" => {
                let t = v.get("content").map(block_text).unwrap_or_default();
                if !t.trim().is_empty() {
                    out.push(SessionMessage::text("assistant", t, *at));
                }
            }
            "agent_thought_chunk" => {
                let t = v.get("content").map(block_text).unwrap_or_default();
                if !t.trim().is_empty() {
                    out.push(SessionMessage::text("thinking", t, *at));
                }
            }
            "tool_call" | "tool_call_update" => {
                let title = v
                    .get("title")
                    .and_then(|t| t.as_str())
                    .or_else(|| v.get("toolCallId").and_then(|t| t.as_str()));
                let args = v
                    .get("rawInput")
                    .filter(|x| !x.is_null())
                    .and_then(|x| serde_json::to_string(x).ok());
                out.push(SessionMessage::tool_call(
                    tool_summary(title),
                    *at,
                    title.map(String::from),
                    args,
                ));
            }
            // session_info_update / available_commands_update / plan 等会话级
            // 元事件 → 跳过
            _ => {}
        }
    }
    // SQLite 查询自带 LIMIT：无「头部截断」语义（truncated 恒 false，Bug 1 契约）
    Ok(page(out, limit, false))
}

// ============================================================
// 测试（Task 7 Step 3）：黄金夹具 / 合成数据，零真实 ~/.zcode ~/.dsh 接触。
// 命名延续各 parser 测试惯例；ZCode/OpenCode/OpenClaw 用 tmp sqlite，
// dsh 用 tests/fixtures/dsh 黄金样本 + tmp home zstd 代际文件，
// JSONL 族（claude/codex/kimi/workbuddy）用合成行直测映射纯函数。
// ============================================================
#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    // ==== 公共小件 ====

    const SID: &str = "sess_0f1e2d3c-4b5a-4948-8276-9a0b8c7d6e5f";
    const UUID: &str = "0f1e2d3c-4b5a-4948-8276-9a0b8c7d6e5f";

    #[test]
    fn dispatch_rejects_unknown_tool_and_empty_session_id() {
        let tmp = tempfile::tempdir().unwrap();
        let r = read_session_messages_with(tmp.path(), "not-a-tool", SID, 200);
        assert!(r.unwrap_err().contains("CONTENT_UNKNOWN_TOOL: 未知工具"));
        let r = read_session_messages_with(tmp.path(), "zcode", "  ", 200);
        assert!(r
            .unwrap_err()
            .contains("CONTENT_SESSION_ID_EMPTY: session_id 为空"));
        // 路径穿越防御：claude/workbuddy 文件定位按 id 拼 `.jsonl` 文件名，
        // 分隔符 / .. / NUL 必须在内容层入口 fail-closed
        for evil in ["../../etc/passwd", "a/b", "a\\b", "..", "x\\0y"] {
            let r = read_session_messages_with(tmp.path(), "claude", evil, 200);
            assert!(
                r.unwrap_err().contains("非法字符"),
                "穿越形态 {evil:?} 必须被拒绝"
            );
        }
    }

    /// camelCase 序列化契约（移动端字段名，勿漂移）：toolName/toolArgs/collapsed
    #[test]
    fn session_message_serializes_camel_case() {
        let m =
            SessionMessage::tool_call("调用 Bash", Some(1), Some("Bash".into()), Some("{}".into()));
        let v = serde_json::to_value(&m).unwrap();
        assert!(v.get("toolName").is_some(), "toolName 字段（camelCase）");
        assert!(v.get("toolArgs").is_some(), "toolArgs 字段（camelCase）");
        assert!(v.get("collapsed").is_some());
        assert!(v.get("seq").is_some() && v.get("role").is_some() && v.get("kind").is_some());
        assert_eq!(
            v["collapsed"],
            serde_json::json!(true),
            "tool-call 默认折叠"
        );
        // thinking 折叠、user/assistant/tool-result 不折叠
        assert!(SessionMessage::text("thinking", "x", None).collapsed);
        assert!(!SessionMessage::text("user", "x", None).collapsed);
        assert!(!SessionMessage::text("assistant", "x", None).collapsed);
        assert!(!SessionMessage::text("tool-result", "x", None).collapsed);
    }

    /// 序列化面锁（2026-10-13 会话产物批）：cwd=None（其余七工具）时 /session-messages
    /// 载荷零变化——不得凭空多出 cwd 键；codex 有基准时按 camelCase 透出
    #[test]
    fn messages_page_serializes_cwd_only_when_present() {
        let none_page = MessagesPage {
            messages: vec![],
            truncated: false,
            cwd: None,
        };
        let obj = serde_json::to_value(&none_page).unwrap();
        let obj = obj.as_object().unwrap();
        assert!(
            !obj.contains_key("cwd"),
            "cwd=None 必须 skip 序列化：{obj:?}"
        );
        assert_eq!(obj.len(), 2, "载荷恒为 messages + truncated 两键");

        let some_page = MessagesPage {
            messages: vec![],
            truncated: false,
            cwd: Some("/w".to_string()),
        };
        let v = serde_json::to_value(&some_page).unwrap();
        assert_eq!(v["cwd"], serde_json::json!("/w"), "cwd 按原样透出");
    }

    // ==== ZCode（tmp sqlite：message + part 表）====

    fn zcode_db(path: &Path) -> Connection {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE message (
                id TEXT PRIMARY KEY, session_id TEXT, sequence INTEGER,
                time_created INTEGER, data TEXT
             );
             CREATE TABLE part (
                id TEXT PRIMARY KEY, message_id TEXT, sequence INTEGER, data TEXT
             );",
        )
        .unwrap();
        conn
    }

    fn zcode_msg(conn: &Connection, id: &str, seq: i64, ts: i64, role: &str, kind: &str) {
        conn.execute(
            "INSERT INTO message (id, session_id, sequence, time_created, data) VALUES (?1,?2,?3,?4,?5)",
            rusqlite::params![
                id,
                SID,
                seq,
                ts,
                format!(r#"{{"role":"{role}","semantics":{{"kind":"{kind}"}}}}"#)
            ],
        )
        .unwrap();
    }

    fn zcode_part(conn: &Connection, id: &str, mid: &str, seq: i64, data: &str) {
        conn.execute(
            "INSERT INTO part (id, message_id, sequence, data) VALUES (?1,?2,?3,?4)",
            rusqlite::params![id, mid, seq, data],
        )
        .unwrap();
    }

    /// 全 kind 映射 + 记账消息跳过（todo_reminder / timeline_event 整条丢弃）；
    /// step-start / step-finish 是步进边界，不产出条目
    #[test]
    fn zcode_maps_all_kinds_and_skips_bookkeeping() {
        let tmp = tempfile::tempdir().unwrap();
        let conn = zcode_db(&tmp.path().join(".zcode/cli/db/db.sqlite"));
        // m1: 真实用户消息（user_prompt，text part）
        zcode_msg(&conn, "m1", 1, 1000, "user", "user_prompt");
        zcode_part(&conn, "p1", "m1", 1, r#"{"type":"text","text":"你好"}"#);
        // m2/m3: 记账消息（user 角色注入）——整条跳过，text part 不得泄入
        zcode_msg(&conn, "m2", 2, 1100, "user", "todo_reminder");
        zcode_part(
            &conn,
            "p2",
            "m2",
            1,
            r#"{"type":"text","text":"<todo>…</todo>"}"#,
        );
        zcode_msg(&conn, "m3", 3, 1200, "user", "timeline_event");
        zcode_part(&conn, "p3", "m3", 1, r#"{"type":"text","text":"timeline"}"#);
        // m4: assistant——reasoning → thinking、tool → tool-call（含 name/args）、
        //     text → assistant、step-start/step-finish → 无条目
        zcode_msg(&conn, "m4", 4, 1300, "assistant", "assistant");
        zcode_part(&conn, "q1", "m4", 1, r#"{"type":"step-start"}"#);
        zcode_part(
            &conn,
            "q2",
            "m4",
            2,
            r#"{"type":"reasoning","text":"先想一想"}"#,
        );
        zcode_part(
            &conn,
            "q3",
            "m4",
            3,
            r#"{"type":"tool","tool":"Skill","state":{"status":"completed","input":{"skill":"demo"}}}"#,
        );
        zcode_part(&conn, "q4", "m4", 4, r#"{"type":"text","text":"做完了"}"#);
        zcode_part(
            &conn,
            "q5",
            "m4",
            5,
            r#"{"type":"step-finish","reason":"end_turn"}"#,
        );

        let msgs = read_session_messages_with(tmp.path(), "zcode", SID, 200)
            .unwrap()
            .messages;
        let kinds: Vec<&str> = msgs.iter().map(|m| m.kind.as_str()).collect();
        assert_eq!(
            kinds,
            vec!["user", "thinking", "tool-call", "assistant"],
            "记账消息与步进边界不得产出条目，实得 {kinds:?}"
        );
        assert_eq!(msgs[0].content, "你好");
        assert_eq!(msgs[0].role, "user");
        assert_eq!(msgs[0].ts, Some(1000));
        assert_eq!(msgs[1].content, "先想一想");
        assert!(msgs[1].collapsed, "thinking 默认折叠");
        assert_eq!(msgs[2].tool_name.as_deref(), Some("Skill"));
        assert!(
            msgs[2]
                .tool_args
                .as_deref()
                .unwrap_or_default()
                .contains("\"skill\":\"demo\""),
            "tool_args 应为 state.input 的 JSON 串"
        );
        assert!(msgs[2].collapsed);
        assert_eq!(msgs[3].content, "做完了");
        // seq 是返回数组内的稳定递增号
        assert_eq!(msgs[3].seq, 3);
    }

    /// limit = 尾部窗口：更早的条目被截去，保留文件序末尾 N 条且 seq 重排 0..n
    #[test]
    fn zcode_limit_takes_tail_in_file_order() {
        let tmp = tempfile::tempdir().unwrap();
        let conn = zcode_db(&tmp.path().join(".zcode/cli/db/db.sqlite"));
        for i in 0..5 {
            zcode_msg(&conn, &format!("m{i}"), i, 1000 + i, "user", "user_prompt");
            zcode_part(
                &conn,
                &format!("p{i}"),
                &format!("m{i}"),
                1,
                &format!(r#"{{"type":"text","text":"msg{i}"}}"#),
            );
        }
        let msgs = read_session_messages_with(tmp.path(), "zcode", SID, 3)
            .unwrap()
            .messages;
        let contents: Vec<&str> = msgs.iter().map(|m| m.content.as_str()).collect();
        assert_eq!(contents, vec!["msg2", "msg3", "msg4"], "取尾部且保持文件序");
        assert_eq!(msgs[0].seq, 0, "seq 重排为 0..n");
        assert_eq!(msgs[2].seq, 2);
    }

    /// fix round 1 Minor②：limit 夹取边界——0（未传/坏参路径的 0 值）按 1 生效、
    /// 超大值截回上限 1000（读端在 impl 入口 clamp，此处锁定两端行为）。
    /// 两套独立 tmp home（ZCodeRoots 固定读 .zcode/cli/db/db.sqlite，不能同目录换库）
    #[test]
    fn limit_is_clamped_to_1_1000() {
        let tmp = tempfile::tempdir().unwrap();
        let conn = zcode_db(&tmp.path().join(".zcode/cli/db/db.sqlite"));
        for i in 0..5 {
            zcode_msg(&conn, &format!("m{i}"), i, 1000 + i, "user", "user_prompt");
            zcode_part(
                &conn,
                &format!("p{i}"),
                &format!("m{i}"),
                1,
                &format!(r#"{{"type":"text","text":"msg{i}"}}"#),
            );
        }
        // 下界：0 → 按 1 生效（只出文件序最后 1 条）
        let msgs = read_session_messages_with(tmp.path(), "zcode", SID, 0)
            .unwrap()
            .messages;
        assert_eq!(msgs.len(), 1, "limit=0 必须夹为 1");
        assert_eq!(msgs[0].content, "msg4", "夹取后仍取尾部");
        // 上界：999999 → 截回 1000（1001 条消息夹去 1 条）
        let big = tempfile::tempdir().unwrap();
        let conn = zcode_db(&big.path().join(".zcode/cli/db/db.sqlite"));
        for i in 0..1001 {
            zcode_msg(&conn, &format!("m{i}"), i, 1000 + i, "user", "user_prompt");
            zcode_part(
                &conn,
                &format!("p{i}"),
                &format!("m{i}"),
                1,
                &format!(r#"{{"type":"text","text":"big{i}"}}"#),
            );
        }
        let r = read_session_messages_impl(big.path(), None, None, "zcode", SID, 999_999)
            .unwrap()
            .messages;
        assert_eq!(r.len(), 1000, "limit 上限截回 1000");
        assert_eq!(r[0].content, "big1", "截去最旧的 1 条（文件序头部）");
    }

    #[test]
    fn zcode_missing_session_or_db_errs() {
        let tmp = tempfile::tempdir().unwrap();
        // 库不存在
        assert!(read_session_messages_with(tmp.path(), "zcode", SID, 200).is_err());
        // 库在但会话不在
        let conn = zcode_db(&tmp.path().join(".zcode/cli/db/db.sqlite"));
        zcode_msg(&conn, "m1", 1, 1000, "user", "user_prompt");
        zcode_part(&conn, "p1", "m1", 1, r#"{"type":"text","text":"hi"}"#);
        let r = read_session_messages_with(
            tmp.path(),
            "zcode",
            "sess_ffffffff-ffff-ffff-ffff-ffffffffffff",
            200,
        );
        assert!(r.is_err());
    }

    // ==== dsh（黄金夹具 + tmp home zstd 代际文件）====

    fn dsh_fixture(name: &str) -> String {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/dsh")
            .join(name);
        std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("读取夹具失败 {p:?}: {e}"))
    }

    /// 黄金样本 sample5 映射：真人输入保留、注入（agent-instructions / plugin /
    /// skill-catalog）过滤；同一调用（内嵌块 id 与 tool/call callId 同 id）去重后
    /// 恰好一条 tool-call（fix round 1 Important 2）
    #[test]
    fn dsh_sanitized_fixture_filters_injection_and_maps_tools() {
        let text = dsh_fixture("sample5-approval-pending.sanitized.jsonl");
        let events = crate::monitor::dsh::log::parse_events(&text);
        let msgs = map_dsh_events(&events);
        // 真人输入在板（M0 F8：source.kind=="user" 才是真人）
        assert!(
            msgs.iter()
                .any(|m| m.kind == "user" && m.content.contains("run a command")),
            "真人输入必须出条目"
        );
        // 注入过滤：agent-instructions / skill-catalog / plugin 的大段注入不得出条目
        let injected = msgs.iter().any(|m| {
            m.kind == "user" && (m.content.contains("chars") || m.content.contains("AGENTS"))
        });
        assert!(!injected, "注入 user/message 必须被过滤");
        // 同一调用两路呈现（assistant content 内嵌块 id == tool/call callId，均 bash）：
        // 按调用 id 去重后恰好 1 条，不得在板上看成两次调用
        let tool_calls: Vec<&SessionMessage> =
            msgs.iter().filter(|m| m.kind == "tool-call").collect();
        assert_eq!(
            tool_calls.len(),
            1,
            "同 id 调用必须去重为一条，实得 {tool_calls:?}"
        );
        assert_eq!(tool_calls[0].tool_name.as_deref(), Some("bash"));
        assert!(
            tool_calls[0]
                .tool_args
                .as_deref()
                .is_some_and(|a| !a.is_empty()),
            "arguments（JSON 字符串）应透传为 toolArgs"
        );
        // 记账事件（session / turn / step / approval / session/title / system/message）不出条目
        assert!(!msgs
            .iter()
            .any(|m| m.kind == "user" && m.content.contains("permission")));
    }

    /// 去重防御：调用 id 缺失（内嵌块无 id / tool/call 无 callId）时保守保留两条——
    /// 无键可判等，去重有误杀真实并行调用的风险
    #[test]
    fn dsh_tool_calls_without_ids_are_kept_separately() {
        let events = vec![
            crate::monitor::dsh::log::DshEvent {
                kind: "assistant/message".into(),
                seq: Some(1),
                time: Some(10),
                data: serde_json::json!({
                    "message": { "content": [
                        { "type": "tool-call", "name": "bash" }
                    ]}
                }),
            },
            crate::monitor::dsh::log::DshEvent {
                kind: "tool/call".into(),
                seq: Some(2),
                time: Some(11),
                data: serde_json::json!({ "name": "bash", "arguments": "{}" }),
            },
        ];
        let msgs = map_dsh_events(&events);
        let tool_calls: Vec<&SessionMessage> =
            msgs.iter().filter(|m| m.kind == "tool-call").collect();
        assert_eq!(
            tool_calls.len(),
            2,
            "id 缺失时不得去重（无键判等，保守保留）"
        );
    }

    /// tool/result 事件 → tool-result 条目（sample2 含 isError 工具失败样本）
    #[test]
    fn dsh_tool_result_maps_from_fixture() {
        let text = dsh_fixture("sample2-tool-error.sanitized.jsonl");
        let events = crate::monitor::dsh::log::parse_events(&text);
        let msgs = map_dsh_events(&events);
        let results: Vec<&SessionMessage> =
            msgs.iter().filter(|m| m.kind == "tool-result").collect();
        assert!(!results.is_empty(), "tool/result 必须映射 tool-result");
        assert!(
            results.iter().any(|m| m.content.contains("Error")),
            "工具结果文本应进入 content"
        );
        assert!(!results[0].collapsed, "tool-result 不折叠");
    }

    /// reasoning 块 → thinking（黄金样本无 reasoning 形态，合成事件补位）
    #[test]
    fn dsh_reasoning_block_maps_to_thinking() {
        let events = vec![crate::monitor::dsh::log::DshEvent {
            kind: "assistant/message".into(),
            seq: Some(1),
            time: Some(42),
            data: serde_json::json!({
                "message": { "content": [
                    { "type": "reasoning", "text": "推理过程" },
                    { "type": "text", "text": "结论" }
                ]}
            }),
        }];
        let msgs = map_dsh_events(&events);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].kind, "thinking");
        assert_eq!(msgs[0].content, "推理过程");
        assert!(msgs[0].collapsed);
        assert_eq!(msgs[1].kind, "assistant");
    }

    /// 在指定 dsh 根下写一个 zstd 代际会话（tmp 夹具共享助手）
    fn write_dsh_generation(dsh_root: &Path, session_id: &str) {
        let header = format!(
            r#"{{"type":"session","version":3,"id":"{session_id}","cwd":"/tmp/proj","createdAt":1000,"isSeeded":false}}"#
        );
        let events = concat!(
            r#"{"type":"turn/start","seq":4,"data":{}}"#,
            "\n",
            r#"{"type":"user/message","seq":8,"time":1111,"data":{"content":[{"type":"text","text":"hi dsh"}],"source":{"kind":"user"}}}"#,
            "\n",
            r#"{"type":"assistant/message","seq":9,"time":1112,"data":{"message":{"content":[{"type":"text","text":"done"}]}}}"#,
            "\n",
        );
        let frame = zstd::stream::encode_all(format!("{header}\n{events}").as_bytes(), 3).unwrap();
        let sess = dsh_root
            .join("sessions")
            .join("--proj--")
            .join("escaped~dir");
        std::fs::create_dir_all(&sess).unwrap();
        std::fs::write(sess.join("session.v3.jsonl.zstd"), &frame).unwrap();
    }

    /// tmp home 上的代际文件读取：header.id 匹配路由到会话；未知 id → Err。
    /// 默认根 = home/.dsh（home 为 dsh 数据根，与 dsh_home_with 的回落分支同形）
    #[test]
    fn dsh_reads_zstd_generation_by_header_id() {
        let tmp = tempfile::tempdir().unwrap();
        write_dsh_generation(&tmp.path().join(".dsh"), "session-abc");

        let msgs = read_session_messages_with(tmp.path(), "dsh", "session-abc", 200)
            .unwrap()
            .messages;
        let kinds: Vec<&str> = msgs.iter().map(|m| m.kind.as_str()).collect();
        assert_eq!(kinds, vec!["user", "assistant"], "turn/start 不出条目");
        assert_eq!(msgs[0].content, "hi dsh");
        assert_eq!(msgs[0].ts, Some(1111));
        // 未知会话 → Err（404 语义）
        assert!(read_session_messages_with(tmp.path(), "dsh", "session-other", 200).is_err());
    }

    /// fix round 1 Important 1：$DSH_HOME 重定向必须与看板同源（M0 F14：env >
    /// ~/.dsh，生产扫描是 scan_sessions(&dsh_home())）。env 值以**参数**注入
    /// （测试绝不读真实进程 env，杜绝与并行 env 测试互踩）：
    /// Some(env 根) → 命中 env 根下会话；None（env 未设形态）→ 回落 home/.dsh
    #[test]
    fn dsh_env_home_redirects_data_root() {
        let tmp = tempfile::tempdir().unwrap();
        let env_dir = tempfile::tempdir().unwrap();
        // 会话数据在 env 根下；home 下无任何 dsh 数据（旧的「恒 ~/.dsh」实现在此必红）
        write_dsh_generation(env_dir.path(), "session-abc");
        let env = env_dir.path().to_str().unwrap().to_string();

        let msgs = read_dsh_messages_with(tmp.path(), Some(env.as_str()), "session-abc", 200)
            .unwrap()
            .messages;
        assert_eq!(msgs[0].content, "hi dsh", "env 根指向必须生效");
        // env 未设（None）→ 回落 home/.dsh → 找不到（数据只在 env 根下）
        assert!(read_dsh_messages_with(tmp.path(), None, "session-abc", 200).is_err());
    }

    // ==== Claude（合成 JSONL 行）====

    #[test]
    fn claude_maps_blocks_and_skips_meta() {
        let lines = vec![
            // 记账行：summary / file-history-snapshot → 跳过
            r#"{"type":"summary","summary":"标题","leafUuid":"x"}"#.to_string(),
            r#"{"type":"file-history-snapshot","messageId":"m"}"#.to_string(),
            // isMeta 注入行 → 跳过
            r#"{"type":"user","isMeta":true,"timestamp":"2026-09-15T00:00:00.000Z","message":{"role":"user","content":"<系统注入>"}}"#.to_string(),
            // 真实用户消息（content 字符串形态）
            r#"{"type":"user","timestamp":"2026-09-15T00:00:01.000Z","message":{"role":"user","content":"查一下状态"}}"#.to_string(),
            // assistant：thinking + tool_use + text
            r#"{"type":"assistant","timestamp":"2026-09-15T00:00:02.000Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"想一想"},{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}},{"type":"text","text":"好了"}]}}"#.to_string(),
            // user 侧 tool_result（块数组形态）
            r#"{"type":"user","timestamp":"2026-09-15T00:00:03.000Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"total 0"}]}}"#.to_string(),
        ];
        let msgs = map_claude_lines(&lines);
        let kinds: Vec<&str> = msgs.iter().map(|m| m.kind.as_str()).collect();
        assert_eq!(
            kinds,
            vec!["user", "thinking", "tool-call", "assistant", "tool-result"]
        );
        assert_eq!(msgs[0].content, "查一下状态");
        // timestamp 是 ISO 8601 → epoch 毫秒（期望值就地解析，不写魔法数字）
        assert_eq!(
            msgs[0].ts,
            chrono::DateTime::parse_from_rfc3339("2026-09-15T00:00:01.000Z")
                .ok()
                .map(|dt| dt.timestamp_millis())
        );
        assert_eq!(msgs[2].tool_name.as_deref(), Some("Bash"));
        assert!(msgs[2]
            .tool_args
            .as_deref()
            .unwrap_or_default()
            .contains("\"command\":\"ls\""));
        assert_eq!(msgs[4].content, "total 0");
    }

    /// Bug 1 修复（M3 验收）：胖 JSONL（>512KB）头部被字节尾窗切掉——内容层必须
    /// 报告 truncated，且放大 limit 时字节窗同放大（512KB×⌈limit/200⌉）、头部行回归。
    /// 这是「加载更早消息」死功能的根因之一：按钮出现后重拉拿不到更早内容
    #[test]
    fn claude_fat_file_reports_truncation_and_grows_window_with_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".claude/projects/-Users-x-demo");
        std::fs::create_dir_all(&dir).unwrap();
        // 头部小行（首条用户指令）+ 600KB 大行 + 尾行：512KB 窗必切进大行
        let big_text = "x".repeat(600 * 1024);
        let head = r#"{"type":"user","timestamp":"2026-09-15T00:00:01.000Z","message":{"role":"user","content":"首条指令"}}"#;
        let fat = format!(
            r#"{{"type":"assistant","timestamp":"2026-09-15T00:00:02.000Z","message":{{"role":"assistant","content":[{{"type":"text","text":"{big_text}"}}]}}}}"#
        );
        let tail = r#"{"type":"assistant","timestamp":"2026-09-15T00:00:03.000Z","message":{"role":"assistant","content":[{"type":"text","text":"尾行"}]}}"#;
        std::fs::write(
            dir.join(format!("{UUID}.jsonl")),
            format!("{head}\n{fat}\n{tail}\n"),
        )
        .unwrap();

        // limit=200：字节窗 512KB → truncated=true，头部行缺席（旧实现的静默截断）
        let page = read_session_messages_impl(tmp.path(), None, None, "claude", UUID, 200).unwrap();
        assert!(page.truncated, "超窗文件必须报告头部截断");
        assert!(
            !page.messages.iter().any(|m| m.content == "首条指令"),
            "窗外的头部行本页不可见"
        );
        // limit=400：字节窗放大到 1MB → 全文件进窗，头部行回归
        let page = read_session_messages_impl(tmp.path(), None, None, "claude", UUID, 400).unwrap();
        assert!(!page.truncated, "放大窗后不得再报告截断");
        assert_eq!(page.messages[0].content, "首条指令", "头部行必须可见");
    }

    #[test]
    fn claude_reads_file_by_session_id() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".claude/projects/-Users-x-demo");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(format!("{UUID}.jsonl")),
            r#"{"type":"user","timestamp":"2026-09-15T00:00:01.000Z","message":{"role":"user","content":"hi claude"}}"#,
        )
        .unwrap();
        let msgs = read_session_messages_with(tmp.path(), "claude", UUID, 200)
            .unwrap()
            .messages;
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].content, "hi claude");
        assert!(read_session_messages_with(
            tmp.path(),
            "claude",
            "ffffffff-ffff-ffff-ffff-ffffffffffff",
            200
        )
        .is_err());
    }

    // ==== 2026-10-09 观察台 T4：claude 子 agent 详情读取 ====

    /// 子转写 → SessionMessage（map_claude_lines 复用：thinking/tool_use/tool_result
    /// 全量映射；attachment 行无 message.content 天然跳过——乙.3.1 噪声过滤免费继承）
    #[test]
    fn claude_subagent_messages_maps_transcript() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp
            .path()
            .join(".claude/projects/-Users-x-demo/s1/subagents");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("agent-abg-test-1111.jsonl"),
            concat!(
                r#"{"type":"user","timestamp":"2026-10-08T14:55:56.488Z","message":{"role":"user","content":"<teammate-message teammate_id=\"team-lead\">设计新方案</teammate-message>"}}"#, "\n",
                r#"{"type":"attachment","timestamp":"2026-10-08T14:55:57Z","attachment":{"path":"x"}} "#, "\n",
                r#"{"type":"assistant","timestamp":"2026-10-08T14:56:10Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"先看目录"},{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}]}}"#, "\n",
                r#"{"type":"user","timestamp":"2026-10-08T14:56:12Z","message":{"role":"user","content":[{"type":"tool_result","content":"file-a\nfile-b"}]}}"#, "\n",
            ),
        )
        .unwrap();
        let pg =
            read_claude_subagent_messages_with(tmp.path(), "s1", "abg-test-1111", 200).unwrap();
        let kinds: Vec<&str> = pg.messages.iter().map(|m| m.kind.as_str()).collect();
        assert_eq!(kinds, ["user", "thinking", "tool-call", "tool-result"]);
        assert_eq!(
            pg.messages[0].content,
            "<teammate-message teammate_id=\"team-lead\">设计新方案</teammate-message>"
        );
        assert_eq!(pg.messages[2].tool_name.as_deref(), Some("Bash"));
        assert!(!pg.truncated);
    }

    /// 会话/转写不存在 → Err（端点映射 404）；subagent_id 穿越形态由端点 400 拦截
    #[test]
    fn claude_subagent_messages_missing_is_err() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(read_claude_subagent_messages_with(tmp.path(), "nope", "x", 200).is_err());
        let dir = tmp
            .path()
            .join(".claude/projects/-Users-x-demo/s2/subagents");
        std::fs::create_dir_all(&dir).unwrap();
        assert!(
            read_claude_subagent_messages_with(tmp.path(), "s2", "ghost", 200).is_err(),
            "会话在、转写不在 → Err"
        );
    }

    /// 终审修复回归锁：入口 clamp(1,1000)（评审 P1-2）——limit=0 若不夹取，
    /// finalize(·, 0) 会产出空消息表；夹取后返回末 1 条（非空）。clamp 无此测试
    /// 则被删不会红
    #[test]
    fn claude_subagent_messages_limit_zero_is_clamped_to_one() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp
            .path()
            .join(".claude/projects/-Users-x-demo/s1/subagents");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("agent-abg-clamp-1111.jsonl"),
            concat!(
                r#"{"type":"user","timestamp":"2026-10-08T14:55:56.488Z","message":{"role":"user","content":"任务原文"}}"#, "\n",
                r#"{"type":"assistant","timestamp":"2026-10-08T14:56:10Z","message":{"role":"assistant","content":"步骤 1"}}"#, "\n",
                r#"{"type":"assistant","timestamp":"2026-10-08T14:56:40Z","message":{"role":"assistant","content":"步骤 2"}}"#, "\n",
            ),
        )
        .unwrap();
        let pg = read_claude_subagent_messages_with(tmp.path(), "s1", "abg-clamp-1111", 0).unwrap();
        assert_eq!(pg.messages.len(), 1, "limit=0 → 夹取为 1 → 恰返回末 1 条");
        assert_eq!(pg.messages[0].seq, 0, "finalize 重新编 seq（单条恒 0）");
        assert!(!pg.truncated);
    }

    // ==== 计划形态升格（T1 一等卡片，按形态不按工具名）====

    /// ① claude JSONL：tool_use(input.plan=非空 markdown) → kind="plan" 一等消息
    /// （content=计划原文、role=assistant、collapsed=false、tool_name 保留、tool_args 清空）
    #[test]
    fn claude_plan_input_promotes_to_first_class_plan_message() {
        let lines = vec![r##"{"type":"assistant","timestamp":"2026-09-15T00:00:02.000Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"ExitPlanMode","input":{"plan":"# 执行计划\n\n- 第一步\n- 第二步"}}]}}"##.to_string()];
        let msgs = map_claude_lines(&lines);
        assert_eq!(msgs.len(), 1, "升格后只产 plan 一条，不产 tool-call");
        let m = &msgs[0];
        assert_eq!(m.kind, "plan");
        assert_eq!(m.role, "assistant");
        assert_eq!(
            m.content, "# 执行计划\n\n- 第一步\n- 第二步",
            "计划原文原样"
        );
        assert!(!m.collapsed, "plan 默认展开（一等卡片）");
        assert_eq!(m.tool_name.as_deref(), Some("ExitPlanMode"), "工具名保留");
        assert_eq!(m.tool_args, None, "参数串不透传（正文已升格）");
    }

    /// ② input.plan 空串 / 空白 / 缺失 → 维持 tool-call 原样（不升格、不丢条目）
    #[test]
    fn claude_empty_or_missing_plan_keeps_tool_call() {
        let lines = vec![
            // plan 空串
            r#"{"type":"assistant","timestamp":"2026-09-15T00:00:01.000Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"ExitPlanMode","input":{"plan":""}}]}}"#.to_string(),
            // plan 纯空白（trim 口径视同空）
            r#"{"type":"assistant","timestamp":"2026-09-15T00:00:02.000Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"t2","name":"ExitPlanMode","input":{"plan":"  \n "}}]}}"#.to_string(),
            // 无 plan 字段（普通工具）
            r#"{"type":"assistant","timestamp":"2026-09-15T00:00:03.000Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"t3","name":"Bash","input":{"command":"ls"}}]}}"#.to_string(),
            // args 非 JSON 形态（防御：解析失败维持原状，不 panic）
            r#"{"type":"assistant","timestamp":"2026-09-15T00:00:04.000Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"t4","name":"Weird","input":"raw-string"}]}}"#.to_string(),
        ];
        let msgs = map_claude_lines(&lines);
        assert_eq!(msgs.len(), 4, "四条都保留（形态不达标不升格也不丢弃）");
        for (i, m) in msgs.iter().enumerate() {
            assert_eq!(m.kind, "tool-call", "第 {i} 条应维持 tool-call");
            assert!(m.collapsed, "tool-call 默认折叠语义不变");
            assert!(m.tool_args.is_some(), "参数串原样透传");
        }
    }

    /// ③ 非 ExitPlanMode 工具名但 input.plan 非空 → 同样升格（「按形态不按工具名」
    /// 的回归锁——zcode 等工具记录的是显示 title，按名字匹配会漏）
    #[test]
    fn plan_promotion_is_form_based_not_name_based() {
        let msgs = map_claude_lines(vec![r###"{"type":"assistant","timestamp":"2026-09-15T00:00:02.000Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"SomeCustomPlanner","input":{"plan":"## 方案正文"}}]}}"###.to_string()].as_slice());
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].kind, "plan", "工具名无关，形态命中即升格");
        assert_eq!(msgs[0].content, "## 方案正文");
        assert_eq!(msgs[0].tool_name.as_deref(), Some("SomeCustomPlanner"));
    }

    // ==== Codex（rollout 合成行 + thread_history tmp sqlite）====

    // ---- 批次丙 T4：`<proposed_plan>` 剥标签升格 ----

    /// T4 纯函数：开闭标签剥出 + 前导文本分离（真实形态取自本机 rollout 实录）
    #[test]
    fn split_proposed_plan_handles_real_shape() {
        // 真机实录形态：前导文本 + 标签 + markdown 计划 + 闭标签
        let real = "未收到选择，按规则采用推荐默认：历史档案保留原样。以下是修复计划。\n\n<proposed_plan>\n# 修复 DM 文档同步遗留风险\n\n## 根因\n1. xxx\n</proposed_plan>";
        let (pre, plan) = split_proposed_plan(real).expect("真实形态必须可剥");
        assert_eq!(
            pre,
            "未收到选择，按规则采用推荐默认：历史档案保留原样。以下是修复计划。"
        );
        assert_eq!(plan, "# 修复 DM 文档同步遗留风险\n\n## 根因\n1. xxx");
        assert!(!plan.contains("proposed_plan"), "剥离后不得有标签残留");

        // 无前导文本（标签在开头）
        let (pre, plan) = split_proposed_plan("<proposed_plan># P</proposed_plan>").unwrap();
        assert!(pre.is_empty());
        assert_eq!(plan, "# P");

        // 退化形态 → None（不升格，不猜）
        for bad in [
            "普通消息，没有标签",
            "<proposed_plan>只有开标签",
            "</proposed_plan>先闭后开<proposed_plan>",
            "<proposed_plan>   </proposed_plan>", // 空白内容
            "<proposed_plan></proposed_plan>",    // 空内容
        ] {
            assert!(
                split_proposed_plan(bad).is_none(),
                "退化形态不得升格: {bad}"
            );
        }
    }

    /// T4：assistant 文本含 `<proposed_plan>` → 产 [前导 assistant 文本, plan 一等卡]
    /// （kind=plan、collapsed=false、content=剥标签后的 markdown）
    #[test]
    fn codex_proposed_plan_promotes_to_plan_card() {
        let text = "说明一句。\n<proposed_plan>\n# 计划标题\n\n- 步骤一\n</proposed_plan>";
        let msgs = SessionMessage::assistant_text(text, Some(123));
        assert_eq!(msgs.len(), 2, "前导文本 + 计划卡两条");
        assert_eq!(msgs[0].kind, "assistant");
        assert_eq!(msgs[0].content, "说明一句。");
        assert_eq!(msgs[1].kind, "plan", "标签内容升格一等计划卡");
        assert_eq!(msgs[1].content, "# 计划标题\n\n- 步骤一");
        assert!(
            !msgs[1].content.contains("proposed_plan"),
            "计划卡不得有标签残留（T4 验收）"
        );
        assert!(!msgs[1].collapsed, "一等卡默认展开");
        assert_eq!(msgs[1].role, "assistant");
        assert_eq!(msgs[1].seq, 0, "seq 由调用方统一编号（构造器恒 0）");
    }

    /// T4 端到端：codex rollout 的 assistant message 行 → 消息流出现 kind=plan
    /// （走 `map_codex_lines` 真实分派路径，非只测纯函数）
    #[test]
    fn codex_rollout_assistant_with_plan_tag_yields_plan_kind() {
        let line = r#"{"timestamp":"2026-08-03T17:16:27.100Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"以下是计划。\n<proposed_plan>\n## 方案\n1. 做 A\n</proposed_plan>"}]}}"#;
        let msgs = map_codex_lines(&[line.to_string()]);
        let kinds: Vec<&str> = msgs.iter().map(|m| m.kind.as_str()).collect();
        assert_eq!(
            kinds,
            vec!["assistant", "plan"],
            "升格后两条（前导 + 计划）"
        );
        assert_eq!(msgs[1].content, "## 方案\n1. 做 A");
        assert!(!msgs[1].content.contains("proposed_plan"));
        // 无标签的 assistant 消息不受影响（零回归）
        let plain = r#"{"timestamp":"2026-08-03T17:16:27.100Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"普通回复"}]}}"#;
        let msgs = map_codex_lines(&[plain.to_string()]);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].kind, "assistant");
    }

    // ---- 批次丙 T7：计划文件引用识别 ----

    /// T7 纯函数：kimi 实测原文形态（"Wrote N bytes to …/plans/x.md"）→ 完整路径
    #[test]
    fn extract_plan_file_ref_handles_real_wire_text() {
        // 本机 kimi wire 实测原文（Write 工具结果）
        let real = "Wrote 4263 bytes to C:/Users/bunny/.kimi-code/sessions/wd_test_72c4040eb71a/session_46b9cd5f-c817-4b84-a7ef-ee1ff8a4c59d/agents/main/plans/miss-martian-she-hulk-beast.md";
        assert_eq!(
            extract_plan_file_ref(real).as_deref(),
            Some("C:/Users/bunny/.kimi-code/sessions/wd_test_72c4040eb71a/session_46b9cd5f-c817-4b84-a7ef-ee1ff8a4c59d/agents/main/plans/miss-martian-she-hulk-beast.md")
        );
        // Windows 反斜杠形态
        let win = r"Wrote 12 bytes to C:\Users\u\.kimi-code\sessions\s\agents\main\plans\p.md";
        assert_eq!(
            extract_plan_file_ref(win).as_deref(),
            Some(r"C:\Users\u\.kimi-code\sessions\s\agents\main\plans\p.md")
        );
        // 前缀变体
        assert!(extract_plan_file_ref("Plan file: /w/plans/a.md").is_some());
        assert!(extract_plan_file_ref("Planning: /w/plans/a.md").is_some());
        // 引号包裹（工具结果里 JSON 转义后的常见形态）
        assert_eq!(
            extract_plan_file_ref(r#""/w/plans/a.md""#).as_deref(),
            Some("/w/plans/a.md")
        );

        // 不命中：无 plans/ / 非 .md / 文件名含空白（防吞整句）
        for bad in [
            "Wrote 5 bytes to /w/other/x.md",
            "plans/note.txt",
            "普通回复，没有路径",
            "plans/ two words.md",
        ] {
            assert!(
                extract_plan_file_ref(bad).is_none(),
                "不得命中: {bad:?} → {:?}",
                extract_plan_file_ref(bad)
            );
        }
    }

    /// T7：工具结果含计划文件引用 → 产 [tool-result 原文, plan-file 卡]（原文不丢）
    #[test]
    fn plan_file_ref_yields_card_alongside_tool_result() {
        let text = "Wrote 99 bytes to /w/plans/round-1.md";
        let msgs = SessionMessage::tool_result_with_plan_ref(text, Some(7));
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].kind, "tool-result", "工具结果原文保留");
        assert_eq!(msgs[0].content, text);
        assert_eq!(msgs[1].kind, "plan-file", "追加计划文件卡");
        assert_eq!(msgs[1].content, "/w/plans/round-1.md");
        assert!(!msgs[1].collapsed, "入口卡默认展开");

        // 无引用 → 只产 tool-result（零回归）
        let plain = SessionMessage::tool_result_with_plan_ref("ok", Some(7));
        assert_eq!(plain.len(), 1);
        assert_eq!(plain[0].kind, "tool-result");
    }

    // ---- 丁T2：kimi `interaction.request`（计划审批）→ 计划双卡（裁1）----

    /// 真实形态夹具（本机 wire 实录，`wd_proj-ki1_89e8318bc1ff`）——`display.plan`
    /// **内联全文** + `display.path` 落盘路径。缩录正文为最小可辨形态。
    fn kimi_plan_review_line() -> String {
        r##"{"type":"interaction.request","agentId":"main","id":"approval_38a02479","kind":"approval","toolCallId":"call_546f","request":{"id":"approval_38a02479","sessionId":"session_5f4b","agentId":"main","turnId":0,"toolCallId":"call_546f","toolName":"ExitPlanMode","action":"Presenting plan and exiting plan mode","display":{"kind":"plan_review","plan":"# Plan: Create hi.txt\n\n## Goal\nCreate `hi.txt`.\n","path":"C:/Users/u/.kimi-code/sessions/wd_x/session_y/agents/main/plans/rogue-multiple-man-fire.md"}},"time":1789977432472}"##.to_string()
    }

    /// 丁T2 主用例：`interaction.request(kind=approval, display.kind=plan_review)` →
    /// 消息流产**计划双卡**（裁1「能上就都上」）：
    /// - `kind="plan"`（正文卡）= `display.plan` 内联全文（**不读文件**——wire 已给全文）；
    /// - `kind="plan-file"`（文件卡）= `display.path`（前端「查看计划」读文件预览）。
    ///
    /// 修复的根因（问题 3 的正文半）：`interaction.*` 原被 `_ => {}` 整类跳过，kimi 的
    /// 计划正文因此**从未进入消息流**——终端里看得见、手机上看不见。
    #[test]
    fn kimi_plan_review_yields_plan_and_file_cards() {
        let msgs = map_kimi_lines(&[kimi_plan_review_line()]);
        let kinds: Vec<&str> = msgs.iter().map(|m| m.kind.as_str()).collect();
        assert_eq!(
            kinds,
            vec!["plan", "plan-file"],
            "计划审批 → 正文卡 + 文件卡（正文在前，文件入口在后）"
        );
        assert_eq!(
            msgs[0].content, "# Plan: Create hi.txt\n\n## Goal\nCreate `hi.txt`.\n",
            "正文卡 = display.plan 内联全文（markdown 原文，不做任何加工）"
        );
        assert!(!msgs[0].collapsed, "正文卡恒展开（一等卡语义）");
        assert_eq!(msgs[0].role, "assistant");
        assert_eq!(
            msgs[1].content,
            "C:/Users/u/.kimi-code/sessions/wd_x/session_y/agents/main/plans/rogue-multiple-man-fire.md",
            "文件卡 = display.path（可预览路径原样）"
        );
        assert!(!msgs[1].collapsed, "文件入口卡恒展开");
        // 裁1 硬要求：计划不得以 JSON/未渲染形态留在消息流里
        assert!(
            !msgs.iter().any(|m| m.content.contains("plan_review")
                || m.content.contains("\"display\"")
                || m.kind == "tool-call"),
            "wire 的 interaction JSON 结构不得泄漏进消息流：{msgs:?}"
        );
        assert_eq!(msgs[0].ts, Some(1789977432472));
    }

    /// 丁T2 降级：`display.plan` 缺失/空白但 `display.path` 在场 → **只出文件卡**
    /// （前端「查看计划」按钮读文件，正文照样可达——§2.8 兜底，不假造正文）。
    #[test]
    fn kimi_plan_review_without_inline_plan_yields_file_card_only() {
        for display in [
            r#"{"kind":"plan_review","path":"C:/u/.kimi-code/sessions/s/agents/main/plans/a.md"}"#,
            r#"{"kind":"plan_review","plan":"   ","path":"C:/u/.kimi-code/sessions/s/agents/main/plans/a.md"}"#,
        ] {
            let line = format!(
                r#"{{"type":"interaction.request","id":"approval_1","kind":"approval","request":{{"toolName":"ExitPlanMode","display":{display}}},"time":5}}"#
            );
            let msgs = map_kimi_lines(&[line]);
            let kinds: Vec<&str> = msgs.iter().map(|m| m.kind.as_str()).collect();
            assert_eq!(kinds, vec!["plan-file"], "正文缺失 → 只出文件卡：{display}");
            assert!(msgs[0].content.ends_with("plans/a.md"));
        }
        // 两者皆缺 → 零产出（不产空卡）
        let line = r#"{"type":"interaction.request","id":"approval_1","kind":"approval","request":{"toolName":"ExitPlanMode","display":{"kind":"plan_review"}},"time":5}"#;
        assert!(map_kimi_lines(&[line.to_string()]).is_empty());
    }

    /// 丁T2 范围守卫：**只有 `kind=approval ∧ display.kind=plan_review`** 产计划卡。
    /// 其余 interaction 形态一律零产出（零回归）：
    /// - `file_io`（Write 审批，display 是 {operation,path,content}——**不是计划**，不能
    ///   拿 `path` 当计划文件：本机 wire 实测 file_io 的 path 常指向普通业务文件）；
    /// - `kind=question`（问答，display 无 plan/path）；
    /// - 缺 display / 缺 request / display.kind 未知。
    #[test]
    fn kimi_non_plan_review_interactions_yield_nothing() {
        for line in [
            // file_io：Write 审批（真实形态，path 是业务文件）
            r#"{"type":"interaction.request","id":"a1","kind":"approval","request":{"toolName":"Write","display":{"kind":"file_io","operation":"write","path":"C:/tmp/hi.txt","content":"hi"}},"time":5}"#,
            // command：Bash 审批
            r#"{"type":"interaction.request","id":"a2","kind":"approval","request":{"toolName":"Bash","display":{"kind":"command","command":"ls"}},"time":5}"#,
            // question：问答（不产计划卡——问答走问答端点）
            r#"{"type":"interaction.request","id":"q1","kind":"question","request":{"questions":[{"question":"q","options":[{"label":"a"}]}]},"time":5}"#,
            // 缺 request / 缺 display / 未知 display.kind
            r#"{"type":"interaction.request","id":"a3","kind":"approval","time":5}"#,
            r#"{"type":"interaction.request","id":"a4","kind":"approval","request":{},"time":5}"#,
            r#"{"type":"interaction.request","id":"a5","kind":"approval","request":{"display":{"kind":"todo_list"}},"time":5}"#,
            // resolved：销卡信号，不产消息
            r#"{"type":"interaction.resolved","id":"a1","response":{"decision":"approved"},"time":6}"#,
        ] {
            assert!(
                map_kimi_lines(&[line.to_string()]).is_empty(),
                "非计划审批形态必须零产出：{line}"
            );
        }
    }

    /// 丁T2 端到端：真实 wire 序列（计划文件写入 → 计划审批 → 批准 → ExitPlanMode 回执）
    /// 走完整映射，消息流末态形状与实机一致（plan 卡在场且其后跟着 tool-call/tool-result
    /// ——后者是审批端点的「待决与否」判据输入，见 `remote::api::kimi_plan_approval_pending`）。
    #[test]
    fn kimi_wire_plan_review_sequence_end_to_end() {
        let lines = vec![
            r#"{"type":"turn.prompt","input":[{"type":"text","text":"plan a change"}],"time":1}"#
                .to_string(),
            r##"{"type":"context.append_loop_event","event":{"type":"tool.call","toolCallId":"c1","name":"Write","args":{"path":"p.md","content":"# P"}},"time":2}"##.to_string(),
            r#"{"type":"context.append_loop_event","event":{"type":"tool.result","toolCallId":"c1","result":{"output":"Wrote 400 bytes to C:/u/.kimi-code/sessions/s/agents/main/plans/plan-a.md"}},"time":3}"#.to_string(),
            kimi_plan_review_line(),
            r#"{"type":"interaction.resolved","id":"approval_38a02479","response":{"decision":"approved","selectedLabel":"Approve"},"time":4}"#.to_string(),
            r#"{"type":"context.append_loop_event","event":{"type":"tool.call","toolCallId":"c2","name":"ExitPlanMode","args":{}},"time":5}"#.to_string(),
            r#"{"type":"context.append_loop_event","event":{"type":"tool.result","toolCallId":"c2","result":{"output":"Exited plan mode."}},"time":6}"#.to_string(),
        ];
        let msgs = map_kimi_lines(&lines);
        let kinds: Vec<&str> = msgs.iter().map(|m| m.kind.as_str()).collect();
        assert_eq!(
            kinds,
            vec![
                "user",
                "tool-call",
                "tool-result",
                "plan-file",
                "plan",
                "plan-file",
                "tool-call",
                "tool-result"
            ],
            "实机序列末态形状（Write 计划 → T7 文件卡 → 审批正文卡+文件卡 → ExitPlanMode 回执）"
        );
        // 尾部（审批窗口关闭后）：最后一条 plan 之后存在 tool-call/tool-result
        // → 审批端点的「待决」判据必须为假（本用例锁语义契约，判据单测见 api.rs）
        let last_plan = msgs.iter().rposition(|m| m.kind == "plan").unwrap();
        assert!(
            msgs[last_plan + 1..]
                .iter()
                .any(|m| m.kind == "tool-call" || m.kind == "tool-result"),
            "批准后 plan 之后必有工具事件（审批窗口已关）"
        );
    }

    /// T7 端到端：kimi wire 的 tool.result 行 → 消息流出 plan-file（走真实分派路径）
    #[test]
    fn kimi_wire_plan_write_yields_plan_file_kind() {
        let line = r#"{"type":"context.append_loop_event","agentId":"main","event":{"type":"tool.result","toolCallId":"c1","result":{"output":"Wrote 4263 bytes to C:/Users/u/.kimi-code/sessions/wd_x/session_y/agents/main/plans/plan-a.md"}}}"#;
        let msgs = map_kimi_lines(&[line.to_string()]);
        let kinds: Vec<&str> = msgs.iter().map(|m| m.kind.as_str()).collect();
        assert_eq!(kinds, vec!["tool-result", "plan-file"]);
        assert!(msgs[1].content.ends_with("plans/plan-a.md"));
        // 普通工具结果零回归
        let plain = r#"{"type":"context.append_loop_event","agentId":"main","event":{"type":"tool.result","toolCallId":"c2","result":{"output":"done"}}}"#;
        assert_eq!(map_kimi_lines(&[plain.to_string()]).len(), 1);
    }

    #[test]
    fn codex_maps_rollout_lines() {
        let lines = vec![
            // session_meta / event_msg 记账 → 跳过
            r#"{"timestamp":"2026-09-06T05:40:24.228Z","type":"session_meta","payload":{"id":"x","cwd":"/w"}}"#.to_string(),
            r#"{"timestamp":"2026-09-06T05:40:24.300Z","type":"event_msg","payload":{"type":"task_started","turn_id":"t1"}}"#.to_string(),
            r#"{"timestamp":"2026-09-06T05:40:30.320Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"检查仓库"}]}}"#.to_string(),
            r#"{"timestamp":"2026-09-06T05:40:34.579Z","type":"response_item","payload":{"type":"reasoning","summary":["先看 git 状态"]}}"#.to_string(),
            r#"{"timestamp":"2026-09-06T05:40:34.952Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"我看一下"}]}}"#.to_string(),
            r#"{"timestamp":"2026-09-06T05:40:34.953Z","type":"response_item","payload":{"type":"function_call","id":"fc_1","name":"exec_command","arguments":"{\"cmd\":\"git status\"}"}}"#.to_string(),
            r#"{"timestamp":"2026-09-06T05:40:35.831Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call_1","output":"ok"}}"#.to_string(),
            r#"{"timestamp":"2026-09-06T05:40:35.887Z","type":"event_msg","payload":{"type":"token_count","info":{}}}"#.to_string(),
        ];
        let msgs = map_codex_lines(&lines);
        let kinds: Vec<&str> = msgs.iter().map(|m| m.kind.as_str()).collect();
        assert_eq!(
            kinds,
            vec!["user", "thinking", "assistant", "tool-call", "tool-result"]
        );
        assert_eq!(msgs[0].content, "检查仓库");
        assert_eq!(msgs[1].content, "先看 git 状态");
        assert_eq!(msgs[3].tool_name.as_deref(), Some("exec_command"));
        assert_eq!(
            msgs[3].tool_args.as_deref(),
            Some(r#"{"cmd":"git status"}"#)
        );
        assert_eq!(msgs[4].content, "ok");
    }

    #[test]
    fn codex_custom_tool_call_extracts_files() {
        // T5（D5）：apply_patch（custom_tool_call）从整条丢弃改为头行提取上板——
        // md×3（新增×2 + 更新×1）/svg×1 出路径、PNG 照旧（view_image 结构化引用）、
        // 串内路径不误收。F5③ 夹具补至 md×3（对齐真实 patch 形态：多文件批量变更）
        // patch 用 JSON 转义形态（字面 \n）——拼进 rollout 行后才是合法 JSON 字符串，
        // serde 解析回真实换行（parse_patch_header 走 lines() 逐行吃）
        let patch = "*** Begin Patch\\n*** Add File: notes/a.md\\n+# 计划\\n*** Add File: notes/b.md\\n+## 附录\\n*** Add File: notes/b.svg\\n+<svg/>\\n*** Update File: docs/c.md\\n@@\\n*** End Patch";
        let lines = vec![
            format!(
                r#"{{"timestamp":"2026-09-06T05:41:00.000Z","type":"response_item","payload":{{"type":"custom_tool_call","id":"ctc_1","name":"apply_patch","input":"{patch}"}}}}"#
            ),
            // PNG 照旧：view_image 结构化引用（file_path 键既有收集）
            r#"{"timestamp":"2026-09-06T05:41:10.000Z","type":"response_item","payload":{"type":"function_call","name":"view_image","arguments":"{\"file_path\":\"/w/img.png\"}"}}"#.to_string(),
            // 串内路径不误收：exec_command 的 cmd 串带类路径文本（PATH_KEYS 无 cmd 键）
            r#"{"timestamp":"2026-09-06T05:41:20.000Z","type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"cat src/lib.rs\"}"}}"#.to_string(),
        ];
        let msgs = map_codex_lines(&lines);
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0].kind, "tool-call");
        assert_eq!(msgs[0].tool_name.as_deref(), Some("apply_patch"));
        for p in ["notes/a.md", "notes/b.md", "notes/b.svg", "docs/c.md"] {
            assert!(msgs[0].content.contains(p), "content 应含 {p}");
        }
        let args = msgs[0]
            .tool_args
            .as_deref()
            .expect("args 应为 changes JSON");
        for p in ["notes/a.md", "notes/b.md", "notes/b.svg", "docs/c.md"] {
            assert!(args.contains(p), "args 应含 {p}");
        }
        // files.rs PATH_KEYS 吸收（origin=tool_write）：纯收集函数对 args 的回收断言
        let v: serde_json::Value = serde_json::from_str(args).unwrap();
        let mut out = Vec::new();
        let mut seen = std::collections::HashSet::new();
        crate::remote::files::collect_path_values(&v, &mut out, &mut seen);
        assert_eq!(out.len(), 4, "PATH_KEYS 按路径吸收四条变更");
        assert!(out.iter().all(|p| !p.ends_with(".png")));
        // md×3 计数（F5③ 口径锁：三条 md 变更全部上板，无去重误并）
        assert_eq!(
            out.iter().filter(|p| p.ends_with(".md")).count(),
            3,
            "md 变更应为 3 条：{out:?}"
        );
        // PNG 照旧 + 串内不误收
        assert!(msgs[1].tool_args.as_deref().unwrap().contains("img.png"));
        assert!(msgs[2].tool_args.as_deref().unwrap().contains("src/lib.rs"));
    }

    #[test]
    fn codex_reads_rollout_by_id_and_falls_back_to_thread_db() {
        let tmp = tempfile::tempdir().unwrap();
        // (a) rollout 快路径：文件名内嵌 session id
        let dir = tmp.path().join(".codex/sessions/2026/09/06");
        std::fs::create_dir_all(&dir).unwrap();
        let rollout = dir.join(format!("rollout-2026-09-06T13-40-16-{UUID}.jsonl"));
        std::fs::write(
            &rollout,
            format!(
                r#"{{"timestamp":"2026-09-06T05:40:24.228Z","type":"session_meta","payload":{{"id":"{UUID}","cwd":"/w"}}}}"#,
            ),
        )
        .unwrap();
        // rollout 只有 meta（无内容条目）→ 应继续落空但不炸
        let pg = read_session_messages_with(tmp.path(), "codex", UUID, 200).unwrap();
        assert!(pg.messages.is_empty(), "仅 meta 的 rollout 映射为空");
        assert_eq!(
            pg.cwd.as_deref(),
            Some("/w"),
            "rollout 首行 session_meta.payload.cwd 必须透传（会话产物相对路径的解析基准）"
        );

        // (b) APP 线路回退：thread_history sqlite（无 rollout 文件时）
        std::fs::remove_file(&rollout).unwrap();
        let hist = tmp.path().join(".codex/thread_history_1.sqlite");
        std::fs::create_dir_all(hist.parent().unwrap()).unwrap();
        let conn = Connection::open(&hist).unwrap();
        conn.execute_batch(
            "CREATE TABLE thread_items (
                thread_id TEXT, turn_id TEXT, item_id TEXT,
                rollout_ordinal INTEGER, created_at_ms INTEGER,
                item_json TEXT NOT NULL, item_type TEXT NOT NULL DEFAULT ''
             );",
        )
        .unwrap();
        let ins = |ord: i64, ms: i64, ty: &str, json: &str| {
            conn.execute(
                "INSERT INTO thread_items (thread_id, turn_id, item_id, rollout_ordinal, created_at_ms, item_json, item_type) VALUES (?1,'t',?2,?3,?4,?5,?6)",
                rusqlite::params![UUID, format!("i{ord}"), ord, ms, json, ty],
            )
            .unwrap();
        };
        ins(
            1,
            100,
            "userMessage",
            r#"{"type":"userMessage","content":[{"type":"text","text":"查状态"}]}"#,
        );
        ins(
            2,
            200,
            "reasoning",
            r#"{"type":"reasoning","summary":["想一想","再查"]}"#,
        );
        ins(
            3,
            300,
            "agentMessage",
            r#"{"type":"agentMessage","text":"一切正常"}"#,
        );
        ins(
            4,
            400,
            "commandExecution",
            r#"{"type":"commandExecution","command":"git status","aggregatedOutput":"clean"}"#,
        );
        ins(
            5,
            500,
            "fileChange",
            r#"{"type":"fileChange","changes":[{"path":"/w/a.rs","kind":{"type":"update"}}]}"#,
        );
        let msgs = read_session_messages_with(tmp.path(), "codex", UUID, 200)
            .unwrap()
            .messages;
        let kinds: Vec<&str> = msgs.iter().map(|m| m.kind.as_str()).collect();
        assert_eq!(
            kinds,
            vec!["user", "thinking", "assistant", "tool-call", "tool-call"]
        );
        assert_eq!(msgs[0].content, "查状态");
        assert_eq!(
            msgs[1].content, "想一想 再查",
            "reasoning.summary 字符串数组拼接"
        );
        assert_eq!(msgs[3].tool_name.as_deref(), Some("exec"));
        assert!(msgs[3]
            .tool_args
            .as_deref()
            .unwrap_or_default()
            .contains("git status"));
        assert!(msgs[4].content.contains("a.rs"), "fileChange 出路径摘要");
        // 未知会话 → Err
        assert!(read_session_messages_with(
            tmp.path(),
            "codex",
            "ffffffff-ffff-ffff-ffff-ffffffffffff",
            200
        )
        .is_err());
    }

    /// rollout_session_cwd：payload.cwd 透传；缺失/空串/损坏/文件不存在一律 None
    #[test]
    fn rollout_session_cwd_reads_payload_cwd_and_tolerates_absent_forms() {
        let tmp = tempfile::tempdir().unwrap();
        let write = |name: &str, body: &str| {
            let p = tmp.path().join(name);
            std::fs::write(&p, body).unwrap();
            p
        };
        let with_cwd = write(
            "a.jsonl",
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"x\",\"cwd\":\"/w\"}}\n",
        );
        assert_eq!(rollout_session_cwd(&with_cwd).as_deref(), Some("/w"));
        let no_cwd = write(
            "b.jsonl",
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"x\"}}\n",
        );
        assert_eq!(
            rollout_session_cwd(&no_cwd),
            None,
            "payload.cwd 缺失 → None"
        );
        let empty_cwd = write(
            "c.jsonl",
            "{\"type\":\"session_meta\",\"payload\":{\"cwd\":\"\"}}\n",
        );
        assert_eq!(rollout_session_cwd(&empty_cwd), None, "空串 cwd 视同缺失");
        let garbage = write("d.jsonl", "not json\n");
        assert_eq!(rollout_session_cwd(&garbage), None, "损坏首行 → None");
        assert_eq!(
            rollout_session_cwd(&tmp.path().join("nope.jsonl")),
            None,
            "文件不存在 → None"
        );
    }

    // ==== Kimi（合成 wire 行 + tmp home session_index）====

    #[test]
    fn kimi_maps_wire_lines_with_merge_and_dedup() {
        let lines = vec![
            // 记账行 → 跳过
            r#"{"type":"metadata","protocol_version":"1.4"}"#.to_string(),
            r#"{"type":"config.update","config":{}}"#.to_string(),
            // 用户输入：turn.prompt 与 append_message(user) 双写 → 全等去重只留一条
            r#"{"type":"turn.prompt","input":[{"type":"text","text":"帮我写个函数"}],"time":1000}"#.to_string(),
            r#"{"type":"context.append_message","message":{"role":"user","content":[{"type":"text","text":"帮我写个函数"}]},"time":1001}"#.to_string(),
            // 流式正文碎片：连续 text part 合并为一条（\n 连接）
            r#"{"type":"context.append_loop_event","event":{"type":"content.part","part":{"type":"text","text":"第一段"}},"time":1100}"#.to_string(),
            r#"{"type":"context.append_loop_event","event":{"type":"content.part","part":{"type":"text","text":"第二段"}},"time":1101}"#.to_string(),
            // think 块字段名是 think（非 text）
            r#"{"type":"context.append_loop_event","event":{"type":"content.part","part":{"type":"think","think":"推理中"}},"time":1200}"#.to_string(),
            // 工具调用（args 是对象）与结果（result.output）
            r#"{"type":"context.append_loop_event","event":{"type":"tool.call","toolCallId":"c1","name":"Bash","args":{"command":"ls"}},"time":1300}"#.to_string(),
            r#"{"type":"context.append_loop_event","event":{"type":"tool.result","toolCallId":"c1","result":{"output":"a.rs"}},"time":1400}"#.to_string(),
            // 第二次用户输入：两条不同内容的 user 不合并（独立输入独立条目）
            r#"{"type":"turn.prompt","input":[{"type":"text","text":"再改一下"}],"time":1600}"#.to_string(),
            // 轮尾记账 → 跳过
            r#"{"type":"turn.ended","reason":"completed","time":1700}"#.to_string(),
            r#"{"type":"usage.record","usage":{},"time":1701}"#.to_string(),
        ];
        let msgs = map_kimi_lines(&lines);
        let kinds: Vec<&str> = msgs.iter().map(|m| m.kind.as_str()).collect();
        assert_eq!(
            kinds,
            vec![
                "user",
                "assistant",
                "thinking",
                "tool-call",
                "tool-result",
                "user"
            ]
        );
        assert_eq!(msgs[0].content, "帮我写个函数", "双写去重只留一条");
        assert_eq!(msgs[1].content, "第一段\n第二段", "连续 text part 合并");
        assert_eq!(msgs[2].content, "推理中");
        assert_eq!(msgs[3].tool_name.as_deref(), Some("Bash"));
        assert!(msgs[3]
            .tool_args
            .as_deref()
            .unwrap_or_default()
            .contains("\"command\":\"ls\""));
        assert_eq!(msgs[4].content, "a.rs");
        assert_eq!(msgs[4].ts, Some(1400));
        assert_eq!(msgs[5].content, "再改一下", "不同内容的两次用户输入不合并");
    }

    /// Bug 3 修复（M3 验收）：kimi skill 装载注入泄漏为 user 正文。
    /// 真实 wire 取证：CLI 把 skill 装载全文写成 turn.steer + append_message(role=user)
    /// 两条同文 user 载荷（与 turn.prompt 双写不同，中间隔 tool-result 时相邻全等
    /// 去重失效 → 同文泄漏两次）。映射前按注入标记过滤：真实用户输入不受影响；
    /// 短 tool.result（loaded inline）留在折叠块，无信息损失
    #[test]
    fn kimi_skill_loaded_injection_never_leaks_as_user_content() {
        // mark 内嵌双引号会破坏合成 JSON（parse 失败被跳过即假绿），故用无引号形态
        let mark =
            "<skill-loaded name=demo>Skill tool loaded instructions for this request.</skill-loaded>";
        let lines = vec![
            // 注入载荷 1：turn.steer（skill 装载全文）
            format!(
                r#"{{"type":"turn.steer","input":[{{"type":"text","text":"{mark}"}}],"time":100}}"#
            ),
            // 工具链路夹在两条注入载荷之间（相邻去重对此失效的真实形态）
            r#"{"type":"context.append_loop_event","event":{"type":"tool.call","toolCallId":"c1","name":"Skill","args":{"skill":"demo"}},"time":110}"#.to_string(),
            r#"{"type":"context.append_loop_event","event":{"type":"tool.result","toolCallId":"c1","result":{"output":"Skill \"demo\" loaded inline."}},"time":120}"#.to_string(),
            // 注入载荷 2：append_message(role=user) 同文
            format!(
                r#"{{"type":"context.append_message","message":{{"role":"user","content":[{{"type":"text","text":"{mark}"}}]}},"time":130}}"#
            ),
        ];
        let msgs = map_kimi_lines(&lines);
        let leaked: Vec<&SessionMessage> = msgs.iter().filter(|m| m.kind == "user").collect();
        assert!(
            leaked.is_empty(),
            "skill 装载注入不得泄漏为 user 正文，实得 {leaked:?}"
        );
        let kinds: Vec<&str> = msgs.iter().map(|m| m.kind.as_str()).collect();
        assert_eq!(
            kinds,
            vec!["tool-call", "tool-result"],
            "工具链路照常出条目"
        );
    }

    #[test]
    fn kimi_reads_via_session_index() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join(".kimi-code");
        let session_dir = home
            .join("sessions/wd_demo_0123456789ab")
            .join(format!("session_{UUID}"));
        std::fs::create_dir_all(session_dir.join("agents/main")).unwrap();
        std::fs::write(
            session_dir.join("agents/main/wire.jsonl"),
            r#"{"type":"turn.prompt","input":[{"type":"text","text":"hi kimi"}],"time":1782300900000}"#,
        )
        .unwrap();
        let index_line = serde_json::json!({
            "sessionId": UUID,
            "sessionDir": session_dir.to_string_lossy(),
            "workDir": "/work/demo",
        })
        .to_string();
        std::fs::write(home.join("session_index.jsonl"), format!("{index_line}\n")).unwrap();

        let msgs = read_session_messages_with(tmp.path(), "kimi", UUID, 200)
            .unwrap()
            .messages;
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].content, "hi kimi");
        // 索引外会话 → Err
        assert!(read_session_messages_with(
            tmp.path(),
            "kimi",
            "ffffffff-ffff-ffff-ffff-ffffffffffff",
            200
        )
        .is_err());
    }

    /// fix round 1 Minor①：KIMI_CODE_HOME 重定向以**参数**注入（env 值作参，
    /// 测试不读真实进程 env——与 kimi_parser 的 run_with_home 互斥锁不再有交集）。
    /// Some(env 根) → 命中 env 根下的索引与会话；None → 回落 home/.kimi-code
    #[test]
    fn kimi_env_home_redirects_data_root() {
        let tmp = tempfile::tempdir().unwrap();
        let env_dir = tempfile::tempdir().unwrap();
        let session_dir = env_dir
            .path()
            .join("sessions/wd_demo_0123456789ab")
            .join(format!("session_{UUID}"));
        std::fs::create_dir_all(session_dir.join("agents/main")).unwrap();
        std::fs::write(
            session_dir.join("agents/main/wire.jsonl"),
            r#"{"type":"turn.prompt","input":[{"type":"text","text":"kimi in env"}],"time":1}"#,
        )
        .unwrap();
        let index_line = serde_json::json!({
            "sessionId": UUID,
            "sessionDir": session_dir.to_string_lossy(),
            "workDir": "/work/demo",
        })
        .to_string();
        std::fs::write(
            env_dir.path().join("session_index.jsonl"),
            format!("{index_line}\n"),
        )
        .unwrap();
        let env = env_dir.path().to_str().unwrap().to_string();

        let msgs = read_kimi_messages_with(tmp.path(), Some(env.as_str()), UUID, 200)
            .unwrap()
            .messages;
        assert_eq!(msgs[0].content, "kimi in env", "env 根指向必须生效");
        // env 未设（None）→ 回落 home/.kimi-code → 找不到
        assert!(read_kimi_messages_with(tmp.path(), None, UUID, 200).is_err());
    }

    // ==== WorkBuddy（合成 JSONL 行 + tmp home projects 扫描）====

    #[test]
    fn workbuddy_maps_lines() {
        let lines = vec![
            r#"{"type":"file-history-snapshot"}"#.to_string(),
            r#"{"type":"reasoning","providerData":{"messageId":"m1"}}"#.to_string(),
            r#"{"type":"message","role":"user","content":[{"type":"text","text":"跑个测试"}]}"#.to_string(),
            r#"{"type":"function_call","name":"shell","arguments":"{\"cmd\":\"pnpm test\"}"}"#.to_string(),
            r#"{"type":"function_call_result","output":"all green"}"#.to_string(),
            r#"{"type":"message","role":"assistant","content":[{"type":"text","text":"测试通过"}]}"#.to_string(),
        ];
        let msgs = map_workbuddy_lines(&lines);
        let kinds: Vec<&str> = msgs.iter().map(|m| m.kind.as_str()).collect();
        assert_eq!(kinds, vec!["user", "tool-call", "tool-result", "assistant"]);
        assert_eq!(msgs[0].content, "跑个测试");
        assert_eq!(msgs[1].tool_name.as_deref(), Some("shell"));
        assert_eq!(msgs[2].content, "all green");
        assert_eq!(msgs[3].content, "测试通过");
    }

    #[test]
    fn workbuddy_reads_by_projects_scan() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".workbuddy/projects/Users-jarvis-demo");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(format!("{UUID}.jsonl")),
            r#"{"type":"message","role":"user","content":[{"type":"text","text":"hi wb"}]}"#,
        )
        .unwrap();
        let msgs = read_session_messages_with(tmp.path(), "workbuddy", UUID, 200)
            .unwrap()
            .messages;
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].content, "hi wb");
        assert!(read_session_messages_with(
            tmp.path(),
            "workbuddy",
            "ffffffff-ffff-ffff-ffff-ffffffffffff",
            200
        )
        .is_err());
    }

    // ==== OpenCode（tmp sqlite）====

    #[test]
    fn opencode_maps_message_parts() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join(".local/share/opencode/opencode.db");
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, data TEXT);
             CREATE TABLE part (message_id TEXT, data TEXT, time_created INTEGER);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message VALUES ('m1', ?1, 100, '{\"role\":\"user\"}')",
            [SID],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO part VALUES ('m1', '{\"type\":\"text\",\"text\":\"看下这个\"}', 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message VALUES ('m2', ?1, 200, '{\"role\":\"assistant\"}')",
            [SID],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO part VALUES ('m2', '{\"type\":\"reasoning\",\"text\":\"推演\"}', 2)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO part VALUES ('m2', '{\"type\":\"tool\",\"tool\":\"bash\",\"state\":{\"input\":{\"cmd\":\"ls\"}}}', 3)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO part VALUES ('m2', '{\"type\":\"text\",\"text\":\"结论\"}', 4)",
            [],
        )
        .unwrap();

        let msgs = read_session_messages_with(tmp.path(), "opencode", SID, 200)
            .unwrap()
            .messages;
        let kinds: Vec<&str> = msgs.iter().map(|m| m.kind.as_str()).collect();
        assert_eq!(kinds, vec!["user", "thinking", "tool-call", "assistant"]);
        assert_eq!(msgs[0].content, "看下这个");
        assert_eq!(msgs[2].tool_name.as_deref(), Some("bash"));
        assert!(msgs[2]
            .tool_args
            .as_deref()
            .unwrap_or_default()
            .contains("\"cmd\":\"ls\""));
        // 未知会话 → Err
        assert!(read_session_messages_with(tmp.path(), "opencode", "ses_other", 200).is_err());
    }

    /// 2.x 派发：`session_v2` 在场 → 走 `session_message`（type 列 + 内联 content[]，
    /// 工具名键 `name`）。若仍查冻结的 `message`/`part` 表，新会话报「不存在」而
    /// create 物化 stamp 命不中（D2 E2E 实测根因）
    #[test]
    fn opencode2_maps_session_message_content() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join(".local/share/opencode/opencode.db");
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE session_v2 (id TEXT PRIMARY KEY, project_id TEXT, directory TEXT, title TEXT,
                 time_updated INTEGER, version TEXT, time_idle INTEGER, idle_outcome TEXT);
             CREATE TABLE session_message (id TEXT PRIMARY KEY, session_id TEXT, type TEXT, seq INTEGER,
                 time_created INTEGER, time_updated INTEGER, data TEXT);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session_message VALUES ('m1', ?1, 'user', 1, 100, 100,
                '{\"time\":{\"created\":100},\"text\":\"看下这个\",\"files\":[],\"agents\":[]}')",
            [SID],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session_message VALUES ('m2', ?1, 'assistant', 2, 200, 200,
                '{\"time\":{\"created\":200},\"agent\":\"build\",\"finish\":\"tool-calls\",\"content\":[\
                    {\"type\":\"reasoning\",\"text\":\"推演\"},\
                    {\"type\":\"tool\",\"id\":\"c1\",\"name\":\"bash\",\"state\":{\"status\":\"completed\",\"input\":{\"cmd\":\"ls\"}}},\
                    {\"type\":\"text\",\"text\":\"结论\"}]}')",
            [SID],
        )
        .unwrap();
        // 回合收尾的 idle 消息（无 content）——须被跳过，不得产空条
        conn.execute(
            "INSERT INTO session_message VALUES ('m3', ?1, 'idle', 3, 300, 300,
                '{\"time\":{\"created\":300},\"outcome\":\"succeeded\"}')",
            [SID],
        )
        .unwrap();

        let msgs = read_session_messages_with(tmp.path(), "opencode", SID, 200)
            .unwrap()
            .messages;
        let kinds: Vec<&str> = msgs.iter().map(|m| m.kind.as_str()).collect();
        assert_eq!(
            kinds,
            vec!["user", "thinking", "tool-call", "assistant"],
            "v2 content[] 三类元素映射；idle 消息跳过"
        );
        assert_eq!(msgs[0].content, "看下这个");
        assert_eq!(msgs[2].tool_name.as_deref(), Some("bash"), "工具名键 name");
        assert!(msgs[2]
            .tool_args
            .as_deref()
            .unwrap_or_default()
            .contains("\"cmd\":\"ls\""));
        assert_eq!(msgs[3].content, "结论");
    }

    /// 2.x 问答类部件终态 → 补 tool-result 销卡（与 1.x 路径同判据；F-3 兼容）
    #[test]
    fn opencode2_question_part_emits_tool_result_when_answered() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join(".local/share/opencode/opencode.db");
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE session_v2 (id TEXT PRIMARY KEY, project_id TEXT, directory TEXT, title TEXT,
                 time_updated INTEGER, version TEXT);
             CREATE TABLE session_message (id TEXT PRIMARY KEY, session_id TEXT, type TEXT, seq INTEGER,
                 time_created INTEGER, time_updated INTEGER, data TEXT);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session_message VALUES ('m1', ?1, 'assistant', 1, 100, 100,
                '{\"time\":{\"created\":100},\"finish\":\"tool-calls\",\"content\":[\
                    {\"type\":\"tool\",\"id\":\"c1\",\"name\":\"question\",\"state\":{\
                        \"status\":\"completed\",\
                        \"input\":{\"questions\":[{\"header\":\"h\",\"question\":\"q\",\"options\":[{\"label\":\"a\"}]}]},\
                        \"metadata\":{\"answers\":[[\"a\"]],\"truncated\":false}}}]}')",
            [SID],
        )
        .unwrap();
        let msgs = read_session_messages_with(tmp.path(), "opencode", SID, 200)
            .unwrap()
            .messages;
        let kinds: Vec<&str> = msgs.iter().map(|m| m.kind.as_str()).collect();
        assert!(
            kinds.contains(&"tool-call") && kinds.contains(&"tool-result"),
            "已答 question 须补 tool-result 销卡，实际 {kinds:?}"
        );
    }

    /// 评审 I1 同根因（content 侧）：**仅 `name` 键、无 T3 形态**的已答 question
    /// （非标形态问答——v1 证据 13 条里 1 条 multiple 变体）也须补 tool-result
    /// 销卡；单查 v1 `tool` 键在 v2 下该分支恒 false，卡片永不销（F-3 回归锁）
    #[test]
    fn opencode2_question_part_name_key_alone_still_emits_tool_result() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join(".local/share/opencode/opencode.db");
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE session_v2 (id TEXT PRIMARY KEY, project_id TEXT, directory TEXT, title TEXT,
                 time_updated INTEGER, version TEXT);
             CREATE TABLE session_message (id TEXT PRIMARY KEY, session_id TEXT, type TEXT, seq INTEGER,
                 time_created INTEGER, time_updated INTEGER, data TEXT);",
        )
        .unwrap();
        // name 键 + 终态 + output，但 input 无 T3 形态（非标：options 缺 label 结构）
        conn.execute(
            "INSERT INTO session_message VALUES ('m1', ?1, 'assistant', 1, 100, 100,
                '{\"time\":{\"created\":100},\"finish\":\"tool-calls\",\"content\":[\
                    {\"type\":\"tool\",\"id\":\"c1\",\"name\":\"question\",\"state\":{\
                        \"status\":\"completed\",\
                        \"input\":{},\
                        \"output\":\"{\\\"answers\\\":[[\\\"x\\\"]]}\"}}]}')",
            [SID],
        )
        .unwrap();
        let msgs = read_session_messages_with(tmp.path(), "opencode", SID, 200)
            .unwrap()
            .messages;
        let kinds: Vec<&str> = msgs.iter().map(|m| m.kind.as_str()).collect();
        assert!(
            kinds.contains(&"tool-result"),
            "name 键已答 question 须销卡（双键判据），实际 {kinds:?}"
        );
    }

    /// 丁T1 复评 F-3：opencode 问答类部件在**终态**（completed / error）→ 追加一条
    /// tool-result（销卡信号，问答端点据此判「已答」让卡片消失）；
    /// **待决**（pending/running）→ 不追加（否则卡片在待决窗口内被自己误销）。
    /// 夹具用 2026-09-21 本机 part 表实测形态（state.status / input.questions / output）
    #[test]
    fn opencode_question_part_emits_tool_result_only_when_answered() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join(".local/share/opencode/opencode.db");
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, data TEXT);
             CREATE TABLE part (message_id TEXT, data TEXT, time_created INTEGER);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message VALUES ('m1', ?1, 100, '{\"role\":\"assistant\"}')",
            [SID],
        )
        .unwrap();
        // 已答（completed + metadata.answers + output）
        conn.execute(
            r#"INSERT INTO part VALUES ('m1', '{"type":"tool","tool":"question","state":{"status":"completed","input":{"questions":[{"question":"Which folder?","options":[{"label":"out"}]}]},"metadata":{"answers":[["out"]],"truncated":false},"output":"{\"answers\":[[\"out\"]]}"}}', 1)"#,
            [],
        )
        .unwrap();
        let msgs = read_session_messages_with(tmp.path(), "opencode", SID, 200)
            .unwrap()
            .messages;
        let kinds: Vec<&str> = msgs.iter().map(|m| m.kind.as_str()).collect();
        assert_eq!(
            kinds,
            vec!["tool-call", "tool-result"],
            "已答的 question 部件必须补一条 tool-result（销卡信号）"
        );
        assert!(
            msgs[1].content.contains("out"),
            "tool-result 取 state.output，实际：{}",
            msgs[1].content
        );

        // 待决（running，无 answers/output）→ **不产** tool-result
        conn.execute("DELETE FROM part", []).unwrap();
        conn.execute(
            r#"INSERT INTO part VALUES ('m1', '{"type":"tool","tool":"question","state":{"status":"running","input":{"questions":[{"question":"Which folder?","options":[{"label":"out"}]}]},"time":{"start":1783326720870}}}', 2)"#,
            [],
        )
        .unwrap();
        let msgs = read_session_messages_with(tmp.path(), "opencode", SID, 200)
            .unwrap()
            .messages;
        let kinds: Vec<&str> = msgs.iter().map(|m| m.kind.as_str()).collect();
        assert_eq!(
            kinds,
            vec!["tool-call"],
            "待决期间不得补 tool-result（否则卡片被自己误销）"
        );

        // 被拒（error，无 output）→ 补 tool-result（终态，兜底文案）
        conn.execute("DELETE FROM part", []).unwrap();
        conn.execute(
            r#"INSERT INTO part VALUES ('m1', '{"type":"tool","tool":"question","state":{"status":"error","error":"The user dismissed this question","input":{"questions":[{"question":"q","options":[{"label":"a"}]}]}}}', 3)"#,
            [],
        )
        .unwrap();
        let msgs = read_session_messages_with(tmp.path(), "opencode", SID, 200)
            .unwrap()
            .messages;
        let kinds: Vec<&str> = msgs.iter().map(|m| m.kind.as_str()).collect();
        assert_eq!(
            kinds,
            vec!["tool-call", "tool-result"],
            "被拒（error）是终态 → 补销卡信号"
        );
        assert_eq!(msgs[1].content, "问题已作答", "缺 output → 兜底文案");

        // 非问答工具（bash，即便 completed）→ **不产** tool-result（零回归锁：
        // 其它工具的消息流形态不动，见上方 opencode_maps_message_parts 的 kinds 断言）
        conn.execute("DELETE FROM part", []).unwrap();
        conn.execute(
            r#"INSERT INTO part VALUES ('m1', '{"type":"tool","tool":"bash","state":{"status":"completed","input":{"cmd":"ls"},"output":"file.txt"}}', 4)"#,
            [],
        )
        .unwrap();
        let msgs = read_session_messages_with(tmp.path(), "opencode", SID, 200)
            .unwrap()
            .messages;
        let kinds: Vec<&str> = msgs.iter().map(|m| m.kind.as_str()).collect();
        assert_eq!(kinds, vec!["tool-call"], "非问答工具仍不产 tool-result");
    }

    /// F-3 纯函数单测：判据本身的三层（名字 / 形态 / 终态），含「名字异但形态同」
    /// 与「名字同但非终态」两个方向
    #[test]
    fn answered_question_part_predicate() {
        let q = |tool: &str, status: &str, with_shape: bool| {
            let input = if with_shape {
                serde_json::json!({"questions": [{"question": "q", "options": [{"label": "a"}]}]})
            } else {
                serde_json::json!({"cmd": "ls"})
            };
            serde_json::json!({
                "type": "tool",
                "tool": tool,
                "state": {"status": status, "input": input}
            })
        };
        // 终态（两种）：名字命中 / 名字异但形态命中
        assert!(is_answered_question_part(&q("question", "completed", true)));
        assert!(is_answered_question_part(&q("question", "error", true)));
        assert!(is_answered_question_part(&q(
            "question_v2",
            "completed",
            true
        )));
        // 非终态 → 不判已答（待决窗口内不销卡）
        assert!(!is_answered_question_part(&q("question", "pending", true)));
        assert!(!is_answered_question_part(&q("question", "running", true)));
        // 非问答工具 / 非 tool 类型 → 不判
        assert!(!is_answered_question_part(&q("bash", "completed", false)));
        assert!(!is_answered_question_part(
            &serde_json::json!({"type": "text", "text": "x"})
        ));
        // 名字命中但形态缺失：仍按名字判（与 monitor 侧 pending_question_part 同口径）
        assert!(is_answered_question_part(&q(
            "question",
            "completed",
            false
        )));
    }

    // ==== OpenClaw（acp_replay tmp sqlite + 优雅降级）====

    fn openclaw_db(path: &Path) -> Connection {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        Connection::open(path).unwrap()
    }

    fn openclaw_event(conn: &Connection, session: &str, seq: i64, at: i64, update: &str) {
        conn.execute(
            "INSERT INTO acp_replay_events (session_id, seq, at, session_key, update_json) VALUES (?1,?2,?3,'k',?4)",
            rusqlite::params![session, seq, at, update],
        )
        .unwrap();
    }

    /// 优雅降级裁决的三态：无 ~/.openclaw、库在但表不在、表在但会话不在 →
    /// 一律 Err("OpenClaw 暂无本地消息历史可读")，不阻塞其余七工具
    #[test]
    fn openclaw_degrades_gracefully_without_history() {
        let tmp = tempfile::tempdir().unwrap();
        // (1) 目录不存在
        let r = read_session_messages_with(tmp.path(), "openclaw", "sess-x", 200);
        assert!(r.unwrap_err().contains("OpenClaw 暂无本地消息历史可读"));
        // (2) 库在但 acp_replay_events 表缺失（旧版存储布局）
        openclaw_db(&tmp.path().join(".openclaw/state/openclaw.sqlite"));
        let r = read_session_messages_with(tmp.path(), "openclaw", "sess-x", 200);
        assert!(r.unwrap_err().contains("OpenClaw 暂无本地消息历史可读"));
        // (3) 表在但会话无事件
        let conn = openclaw_db(&tmp.path().join(".openclaw/state/openclaw.sqlite"));
        conn.execute_batch(
            "CREATE TABLE acp_replay_events (
                session_id TEXT NOT NULL, seq INTEGER NOT NULL, at INTEGER NOT NULL,
                session_key TEXT NOT NULL, run_id TEXT, update_json TEXT NOT NULL,
                PRIMARY KEY (session_id, seq)
             );",
        )
        .unwrap();
        openclaw_event(
            &conn,
            "other-session",
            1,
            100,
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hi"}}"#,
        );
        let r = read_session_messages_with(tmp.path(), "openclaw", "sess-x", 200);
        assert!(r.unwrap_err().contains("OpenClaw 暂无本地消息历史可读"));
    }

    /// ACP sessionUpdate → 统一条目映射（2026-09-15 实机探测的 update_json 结构）
    #[test]
    fn openclaw_maps_acp_replay_events() {
        let tmp = tempfile::tempdir().unwrap();
        let conn = openclaw_db(&tmp.path().join(".openclaw/state/openclaw.sqlite"));
        conn.execute_batch(
            "CREATE TABLE acp_replay_events (
                session_id TEXT NOT NULL, seq INTEGER NOT NULL, at INTEGER NOT NULL,
                session_key TEXT NOT NULL, run_id TEXT, update_json TEXT NOT NULL,
                PRIMARY KEY (session_id, seq)
             );",
        )
        .unwrap();
        openclaw_event(
            &conn,
            "s1",
            1,
            100,
            r#"{"sessionUpdate":"session_info_update","title":"t"}"#,
        );
        openclaw_event(
            &conn,
            "s1",
            2,
            200,
            r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"帮我看看"}}"#,
        );
        openclaw_event(
            &conn,
            "s1",
            3,
            300,
            r#"{"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":"思考中"}}"#,
        );
        openclaw_event(
            &conn,
            "s1",
            4,
            400,
            r#"{"sessionUpdate":"tool_call","toolCallId":"c1","title":"Bash","rawInput":{"command":"ls"}}"#,
        );
        openclaw_event(
            &conn,
            "s1",
            5,
            500,
            r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"完成"}}"#,
        );

        let msgs = read_session_messages_with(tmp.path(), "openclaw", "s1", 200)
            .unwrap()
            .messages;
        let kinds: Vec<&str> = msgs.iter().map(|m| m.kind.as_str()).collect();
        assert_eq!(
            kinds,
            vec!["user", "thinking", "tool-call", "assistant"],
            "会话级元事件不出条目"
        );
        assert_eq!(msgs[0].content, "帮我看看");
        assert_eq!(msgs[0].ts, Some(200));
        assert_eq!(msgs[2].tool_name.as_deref(), Some("Bash"));
        assert!(msgs[2]
            .tool_args
            .as_deref()
            .unwrap_or_default()
            .contains("\"command\":\"ls\""));
        assert_eq!(msgs[3].content, "完成");
    }
}
