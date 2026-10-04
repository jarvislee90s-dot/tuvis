//! TTY 靶向消歧（C0-③ / 缺口 L13）——**纯核**：把「同 agent + 同 cwd 多实例」的注入
//! 靶向从「任选一个（猜）」收窄为「精确 TTY 命中；取不到 TTY 即拒绝并报错」，与既有
//! A1 分层写入确认闭环。
//!
//! ## 根因定位：缺口在**写侧的 pid 来源**，不在终端层
//!
//! 终端层**已经是 TTY 精确匹配**：[`crate::inject::engine`] 的 macOS 执行层先
//! `window::get_tty_for_pid(pid)`，再按全路径相等命中 tmux pane（
//! [`crate::inject::engine::parse_panes_find`]）/ iTerm session / Terminal.app tab
//! （P2-3 回归锁：`/dev/ttys100` 不撞 `/dev/ttys1000`）。即：**给定正确 pid，注入必落
//! 到该 pid 的窗口**。
//!
//! 乱窜因此产生在 **pid 是怎么来的**：解析器按「进程名 + cwd」把会话文件与进程配对
//! （claude 走同 cwd 桶内下标配对 `monitor/claude_parser.rs:88-100`；codex 走同 cwd
//! 首个未占用文件 `monitor/codex_parser.rs:422-456`），同 cwd 多实例时配对可交叉；
//! 写侧（[`crate::inject::queue`]）再原样信任卡片 pid → 消息打进兄弟实例的终端。
//! Mac 实测（2026-10-04 报告 ③-4）：投给 `bfa67692` 的消息全部落进 `61be685d` 窗口，
//! 而读侧上板（3 张卡、id 各异）是对的——**读侧与写侧的一致性缺口**。
//!
//! ## 判定纪律（不猜）
//!
//! - **候选** = 同工具 + 同 cwd 的**活动进程**（Mac 报告所指「(进程名, cwd) 匹配集」），
//!   由进程扫描直接给出——不经解析器配对，故与卡片 pid 是否交叉无关；
//! - **候选唯一 → 放行**（无论有没有 TTY：没有歧义可消）；
//! - **候选 ≥2 → 只有精确 TTY 命中能消歧**（[`resolve_by_tty`]）；命中不唯一
//!   （两个候选同 TTY = TTY 不具判别力）/ 无 TTY 证据 / 平台不采 TTY → **拒绝**，
//!   绝不任选（[`TargetDecision::Ambiguous`]）；
//! - **候选 0**（进程已死 / cwd 口径漂移）→ **不做歧义拒绝**：交既有链路如实失败。
//!   拒绝文案只对「有多个候选」成立——「0 个候选」不是歧义（见 [`decide_target`]）。
//!
//! ## 今天两平台生产路径都走「拒绝」臂（诚实申报，勿误读为 Mac 已消歧）
//!
//! - **Windows**：TTY 采集未实现（本任务边界：窗口-进程链等价键登记后续优化）→
//!   [`agent_tty`] 恒 None，且 [`tty_collection_supported`] 对非 macOS 恒 false →
//!   候选 ≥2 一律拒绝；
//! - **macOS**：唯一可得的 TTY 是 `agent_tty(卡片 pid)` = pid→TTY，而**卡片 pid 正是
//!   被怀疑配对交叉的那一个**——拿它当「目标会话的 TTY」是**自证循环**：多候选恒命中
//!   卡片自己 → 拒绝规则永不触发、乱窜原样保留（正是 Mac 实测的失败形态）。故生产不
//!   接该输入：写侧恒传 `session_tty = None`。待**独立**的会话↔TTY 证据源落地
//!   （如 hook 事件携带 tty / pane→session 反查）后由该参数接入，精确匹配臂即转生产
//!   活路（纯核与全表测试已就位）。
//!
//! 复用（不重复造）：TTY 采数复用 `window::get_tty_for_pid`（`ps -p <pid> -o tty=`，
//! 拒空/`??`）与 `window::normalize_dev_tty`（补 `/dev/` 全路径——`#{pane_tty}` 与
//! AppleScript `tty of ...` 一律全路径，相等匹配的入参形态）；候选过滤复用
//! `monitor::cwd::normalize_cwd_for_match`（与解析器配对同一归一化域）与 adapter 的
//! 进程发现（`adapter::adapter_by_id(tool).find_processes`，与读侧同一套进程名/孤儿/
//! 子代理过滤）。

use crate::adapter::AgentProcess;

/// cwd 回退策略结论（plan 契约）：`Allow(n)` / `Reject(n)` 的载荷都是**候选数 n**。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CwdFallback {
    /// 候选唯一（n=1）：无歧义可消 → 放行（调用方取唯一 pid）
    Allow(u32),
    /// 候选 ≥2：无从确定投递目标 → 拒绝（载荷 = 候选数，供报错文案）
    Reject(usize),
}

/// cwd 回退策略（plan 原文语义；**不猜**）：只有恰好一个候选才放行，其余一律拒绝。
///
/// 注意 **n=0 也落 Reject(0)**（plan 原文 `if n == 1 { Allow } else { Reject(n) }`）：
/// 本函数是「歧义策略」本身，不做「无候选」的分流——`0 个候选`不是歧义，[`decide_target`]
/// 在调本函数**之前**就短路为放行（见其文档），故生产路径不会产出 `Reject(0)` 文案。
pub fn fallback_cwd_policy(n: usize) -> CwdFallback {
    if n == 1 {
        CwdFallback::Allow(n as u32)
    } else {
        CwdFallback::Reject(n)
    }
}

/// 精确 TTY 命中：`(pid, tty)` 候选表里按**相等**找目标 TTY，返回该 pid；无命中 `None`。
///
/// 相等而非包含/前缀：与 `parse_panes_find` 同纪律（`/dev/ttys100` 不得撞
/// `/dev/ttys1000`）。入参两侧必须同形态（全路径，经 `normalize_dev_tty` 归一）。
pub fn resolve_by_tty(cands: &[(u32, &str)], target: &str) -> Option<u32> {
    cands.iter().find(|(_, t)| *t == target).map(|(p, _)| *p)
}

/// 靶向结论（写侧闸的输出）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetDecision {
    /// 靶向可确定：候选唯一，或 TTY 精确命中唯一候选 → 注入该 pid
    /// （候选 0 时原样返回会话 pid：不是歧义，交既有链路如实失败）
    Resolved(u32),
    /// **拒绝**（≥2 候选且无可用 TTY 证据；载荷 = 候选数）：**零注入副作用**，报错
    /// [`ambiguous_target_error`] + 审计 `action='fail' result='ambiguous_target'`
    Ambiguous(usize),
}

/// 本平台是否支持 TTY 采数（plan 边界：**Windows 本任务不采**——窗口-进程链等价键
/// 登记后续优化）。非 macOS 一律 false：即便调用方塞进 TTY 表也不据此消歧
/// （未验数据不得当证据；Windows 生产行为 = 候选 ≥2 即拒绝）。
pub fn tty_collection_supported(os: &str) -> bool {
    os == "macos"
}

/// 候选进程 TTY 采集：逐个候选 `ps -o tty= -p <pid>`（macOS），归一为 `/dev/` 全路径。
/// 其余平台（含 Windows）返回空表 = 采数未实现（[`tty_collection_supported`] 同源）。
pub fn collect_candidate_ttys(pids: &[u32]) -> Vec<(u32, String)> {
    pids.iter()
        .filter_map(|pid| agent_tty(*pid).map(|tty| (*pid, tty)))
        .collect()
}

/// agent 进程 TTY：sysinfo 无直接口，经 `ps -o tty= -p <pid>`（macOS）——**复用**
/// [`crate::window::get_tty_for_pid`]（含空/`??` 拒绝：桌面 APP 无 TTY）与
/// [`crate::window::normalize_dev_tty`]（裸 `ttys006` → `/dev/ttys006`，与
/// `#{pane_tty}` / AppleScript `tty of ...` 同形态，相等匹配的前置）。
///
/// Windows：本任务不采（plan 边界）→ 恒 None（写侧闸据此走拒绝规则）。
#[cfg(target_os = "macos")]
pub fn agent_tty(pid: u32) -> Option<String> {
    super::get_tty_for_pid(pid)
        .ok()
        .map(|tty| super::normalize_dev_tty(&tty))
}

/// Windows/其他平台：TTY 采数未实现（见模块文档「今天两平台生产路径都走拒绝臂」）。
#[cfg(not(target_os = "macos"))]
pub fn agent_tty(_pid: u32) -> Option<String> {
    None
}

/// 靶向决策纯核（**OS 参数化**：全决策表在任何平台都可钉——本仓
/// `confirm::triage_screen_recovery(recovery, tool, os)` 同款缝式）。
///
/// 输入：候选 pid 表（判定基数，来源 = 同工具 + 同 cwd 活动进程）、候选 TTY 表
/// （`(pid, tty)`，可能只覆盖部分候选 = 采不到者缺席）、**会话级 TTY 证据**
/// （必须**独立于卡片 pid**，见模块文档「自证循环」）、会话 pid、平台。
///
/// 判定顺序（与模块文档纪律一一对应）：
/// 1. 候选 0 → `Resolved(session_pid)`：不是歧义（进程已死 / cwd 口径漂移），交既有
///    链路如实失败——**不产出 `Reject(0)` 文案**；
/// 2. 候选唯一（[`fallback_cwd_policy`] 的 `Allow` 臂）→ `Resolved(session_pid)`：无歧义
///    可消，TTY 有无不影响（不因「取不到 TTY」把单候选路径一起打死）；
/// 3. 候选 ≥2 → 仅当平台支持采数 **且** 有会话级 TTY 证据 **且** 候选表中**恰好一个**
///    命中该 TTY（[`resolve_by_tty`]）→ `Resolved(命中 pid)`；其余一切情形 → `Ambiguous(n)`。
pub fn decide_target(
    candidate_pids: &[u32],
    candidate_ttys: &[(u32, &str)],
    session_tty: Option<&str>,
    session_pid: u32,
    os: &str,
) -> TargetDecision {
    // 1) 无候选：不是歧义（拒绝文案只对「有多个候选」成立）
    if candidate_pids.is_empty() {
        return TargetDecision::Resolved(session_pid);
    }
    // 2) 唯一候选（fallback_cwd_policy 的 Allow 臂）：放行
    if matches!(
        fallback_cwd_policy(candidate_pids.len()),
        CwdFallback::Allow(_)
    ) {
        return TargetDecision::Resolved(session_pid);
    }
    // 3) ≥2 候选：只有精确 TTY 命中（且命中唯一）能消歧
    if tty_collection_supported(os) {
        if let Some(target) = session_tty {
            if let Some(pid) = resolve_by_tty(candidate_ttys, target) {
                // 命中唯一性：两个候选同 TTY = 该 TTY 不具判别力（同终端内多实例），
                // 照 find 的首个命中走就是「猜」——拒绝
                if candidate_ttys.iter().filter(|(_, t)| *t == target).count() == 1 {
                    return TargetDecision::Resolved(pid);
                }
            }
        }
    }
    TargetDecision::Ambiguous(candidate_pids.len())
}

/// 拒绝文案（plan 定形，N = 候选数）：单一措辞出口——写侧落账与端点回执同源。
pub fn ambiguous_target_error(n: usize) -> String {
    format!(
        "同目录存在 {n} 个候选会话，无法确定投递目标（已知缺口 L13 修复）：请关闭多余窗口或改用无头通道"
    )
}

/// 纯核：同 cwd 候选过滤（可与真进程扫描分离测试）。归一化域与解析器配对**同一套**
/// （`monitor::cwd::normalize_cwd_for_match`：尾部分隔符/分隔符方向/Windows 大小写），
/// 故候选集 = 解析器当初可用于配对的「(进程名, cwd) 匹配集」。
/// 无有效 cwd（空串 / 根路径归一为空）→ 空表（无从判定：写侧按「无候选」放行）。
///
/// **已知残余（低估 ⇒ 放行 ⇒ 修复前行为，宁松不误拒）**：
///
/// - **`cwd` 读不到的进程不计入**（`process.cwd()` 为 None：权限受限的提权进程等）——
///   它可能是真兄弟实例，漏计即候选数偏低，闸门放行；
/// - 上游进程发现（`monitor::process::find_processes_by_names`，本函数消费其产物）另有
///   两类过滤：**同名子进程按父进程同工具判为子 Agent 剔除**、**CLI 孤儿进程剔除**——
///   真正的兄弟实例若被判成子 Agent/孤儿同样漏计，方向一致（放行）。
///
/// 两条都是**放行侧**的低估（不会误拒；代价是乱窜保护在这类实例上失效），登记为已知
/// 残余；要收口需更精确的「会话 ↔ 进程」证据源（届时与 TTY 证据一并接入）。
pub fn candidates_in_cwd(processes: &[AgentProcess], project_path: &str) -> Vec<u32> {
    let target = crate::monitor::cwd::normalize_cwd_for_match(project_path);
    if target.is_empty() {
        return Vec::new();
    }
    processes
        .iter()
        .filter(|p| {
            p.cwd
                .as_ref()
                .map(|c| {
                    crate::monitor::cwd::normalize_cwd_for_match(&c.to_string_lossy()) == target
                })
                .unwrap_or(false)
        })
        .map(|p| p.pid)
        .collect()
}

/// **靶向拒绝**（L13）：同工具 + 同 cwd 候选 **≥2** 且无独立 TTY 证据 → 无从确定投递目标。
/// 载荷 = 候选数（报错文案用）。**调用方必须如实拒绝（零注入）**，不得任选一个。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AmbiguousTarget(pub usize);

/// 靶向证据（写侧已知的「目标是谁」的全部信息）——判定内核 [`resolve_target_pid`] 的输入。
/// 生产装配见 [`tool_target_evidence`]；测试注入合成表（零真实进程 / 零窗口）。
#[derive(Debug, Clone, Default)]
pub struct TargetEvidence {
    /// 该工具的**活动进程**（未经 cwd 过滤；候选过滤由 [`candidates_in_cwd`] 做）
    pub processes: Vec<AgentProcess>,
    /// 候选 TTY 表 `(pid, tty)`（生产 = macOS [`agent_tty`] 采数；非 macOS 空表 = 未采）
    pub candidate_ttys: Vec<(u32, String)>,
    /// 会话级 TTY 证据（**只有独立于卡片 pid 的来源才算证据**；生产恒 None——见模块文档）
    pub session_tty: Option<String>,
}

/// 靶向证据源缝：`(工具 id, 会话 cwd) -> 证据`。生产 = [`tool_target_evidence`]；
/// 测试注入合成证据（`RemoteState.target_evidence`）。
pub type TargetEvidenceFn = dyn Fn(&str, &str) -> TargetEvidence + Send + Sync;

/// 该工具的活动进程表（生产：共享进程快照 + adapter 进程发现，与读侧卡片同源同轮；
/// 快照未建立 → 空表，见 `adapter::with_shared_processes`）。
pub fn tool_processes(tool_id: &str) -> Vec<AgentProcess> {
    let Some(adapter) = crate::adapter::adapter_by_id(tool_id) else {
        return Vec::new();
    };
    crate::adapter::with_shared_processes(|system| adapter.find_processes(system))
        .unwrap_or_default()
}

/// 生产证据源：候选进程 + 候选 TTY 采数（macOS）+ **恒 None 的会话级证据**。
///
/// 为什么 `session_tty` 恒 None（模块文档「自证循环」的落地处）：唯一可得的 TTY 是
/// `agent_tty(卡片 pid)`，而卡片 pid 正是被怀疑配对交叉的那一个——用它当证据必然
/// 命中自己，拒绝规则永不触发、乱窜原样保留。故生产两平台都走拒绝臂；独立证据源
/// （hook 事件携带 tty / pane→session 反查）落地后**只改这一处**。
pub fn tool_target_evidence(tool_id: &str, project_path: &str) -> TargetEvidence {
    let processes = tool_processes(tool_id);
    let pids = candidates_in_cwd(&processes, project_path);
    TargetEvidence {
        processes,
        candidate_ttys: collect_candidate_ttys(&pids),
        session_tty: None,
    }
}

/// **靶向解析（所有注入 / 按键 / 菜单路径的唯一入口）**：把会话的靶向 pid 解析成**可信
/// 目标**并就地改写 `session.pid`——调用方此后一律用 `session.pid`（不再有「拿了返回值
/// 忘了用」的漏改面）。
/// - `Ok(())` = 可安全注入（无候选 / 唯一候选 → 卡片 pid 原样；TTY 证据精确命中唯一
///   候选 → 改道为该候选 pid）；
/// - `Err(AmbiguousTarget(n))` = **必须拒绝**：零注入 + 如实回执 + 审计（各端点按其既有
///   失败契约上报，文案取 [`ambiguous_target_error`]）。
///
/// 判定全在纯核 [`decide_target`]（OS 参数化，全表可测）；[`resolve_target_pid`] 是
/// 它的「会话 → pid」投影，本函数再落实改道。
pub fn resolve_session_target(
    session: &mut crate::session::Session,
    evidence: &TargetEvidence,
    os: &str,
) -> Result<(), AmbiguousTarget> {
    let pid = resolve_target_pid(session, evidence, os)?;
    session.pid = pid;
    Ok(())
}

/// [`resolve_session_target`] 的纯投影（不改写会话；单测直接钉判定结果）。
pub fn resolve_target_pid(
    session: &crate::session::Session,
    evidence: &TargetEvidence,
    os: &str,
) -> Result<u32, AmbiguousTarget> {
    let pids = candidates_in_cwd(&evidence.processes, &session.project_path);
    let tty_view: Vec<(u32, &str)> = evidence
        .candidate_ttys
        .iter()
        .map(|(p, t)| (*p, t.as_str()))
        .collect();
    match decide_target(
        &pids,
        &tty_view,
        evidence.session_tty.as_deref(),
        session.pid,
        os,
    ) {
        TargetDecision::Resolved(pid) => Ok(pid),
        TargetDecision::Ambiguous(n) => Err(AmbiguousTarget(n)),
    }
}

/// 生产候选枚举（[`tool_processes`] 的 cwd 投影）：同工具 + 同 cwd 的**活动进程** pid 表。
///
/// 走 adapter 的进程发现（`find_processes`：与读侧同一套进程名/孤儿/子代理过滤——
/// 候选集必须与解析器当初可用的配对集同源），进程表取
/// `adapter::with_shared_processes` 的**共享快照**（与看板卡片同一份、同轮新鲜度：
/// 写侧不再自开全表扫描——一来「数据同源铁律」，二来写侧临界区（注入 in-flight
/// 守卫）内多一次全表刷新会拉长守卫占用，实测放大既有「裸会话 id 撞 INFLIGHT 键」
/// 的测试假红）。cwd 比较经 [`candidates_in_cwd`]。
/// 未注册工具 / 无有效 cwd → 空表（写侧按「无候选」放行，不误报歧义）。
pub fn cwd_candidate_pids(tool_id: &str, project_path: &str) -> Vec<u32> {
    if crate::monitor::cwd::normalize_cwd_for_match(project_path).is_empty() {
        return Vec::new();
    }
    // 共享快照从未建立（None）→ 空表：「无进程表可判」不是歧义，写侧按无候选放行
    //（= 修复前行为）。生产路径上写侧刚经 session_source（同源的 get_all_sessions）取到
    // 会话，快照必然已建立 → 此降级只出现在测试注入源等非生产形态。读侧正在刷新时会
    // 等待该轮完成（不静默降级）——见 with_shared_processes 的两条纪律
    candidates_in_cwd(&tool_processes(tool_id), project_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{AgentType, ProcessForm, Session, SessionStatus};

    /// 会话夹具（字段形状对齐 queue / server 测试）
    fn sess(id: &str, cwd: &str, pid: u32) -> Session {
        Session {
            id: id.into(),
            agent_type: AgentType::Claude,
            project_name: "proj".into(),
            project_path: cwd.into(),
            title: None,
            git_branch: None,
            github_url: None,
            status: SessionStatus::Waiting,
            last_message: None,
            last_message_role: None,
            last_activity_at: "2026-09-18T00:00:00Z".into(),
            pid,
            cpu_usage: 0.0,
            active_subagent_count: 0,
            form: ProcessForm::Cli,
            jump_supported: false,
            unread: false,
        }
    }

    /// 合成候选进程（同 cwd 的 n 个实例）
    fn procs_in_cwd(cwd: &str, pids: &[u32]) -> Vec<AgentProcess> {
        pids.iter()
            .map(|pid| AgentProcess {
                pid: *pid,
                cpu_usage: 0.0,
                cwd: Some(std::path::PathBuf::from(cwd)),
                form: ProcessForm::Cli,
                exe: None,
            })
            .collect()
    }

    /// **真靶向入口（生产 seam）**：真 `candidates_in_cwd` + 真 `decide_target`，
    /// 只把证据换成合成表——≥2 候选 → 拒绝（这是闸门在生产走的同一条代码路径）
    #[test]
    fn resolve_target_pid_refuses_on_real_gate_with_two_candidates() {
        let s = sess("s-1", "/work/demo", 44161);
        // 两兄弟 + 卡片 pid 是其中之一 → 卡片 pid 不可信 → 拒绝（载荷 = 候选数）
        let ev = TargetEvidence {
            processes: procs_in_cwd("/work/demo", &[44161, 44638]),
            ..Default::default()
        };
        assert_eq!(
            super::resolve_target_pid(&s, &ev, "windows"),
            Err(super::AmbiguousTarget(2)),
            "≥2 候选（Windows 无 TTY 证据）→ 真闸拒绝"
        );
        assert_eq!(
            super::resolve_target_pid(&s, &ev, "macos"),
            Err(super::AmbiguousTarget(2)),
            "macOS 生产证据同样无独立 TTY 证据（session_tty=None）→ 拒绝"
        );
        // 证据里只有一个同 cwd 进程 → 无歧义，放行卡片 pid
        let ev1 = TargetEvidence {
            processes: procs_in_cwd("/work/demo", &[44161]),
            ..Default::default()
        };
        assert_eq!(super::resolve_target_pid(&s, &ev1, "windows"), Ok(44161));
        // 同工具但别处的进程（cwd 不同）不计入候选 → 放行
        let ev_other = TargetEvidence {
            processes: procs_in_cwd("/work/other", &[1, 2, 3]),
            ..Default::default()
        };
        assert_eq!(
            super::resolve_target_pid(&s, &ev_other, "windows"),
            Ok(44161)
        );
        // 无候选（进程已死 / 证据缺位）→ 放行（交既有链路如实失败）
        assert_eq!(
            super::resolve_target_pid(&s, &Default::default(), "windows"),
            Ok(44161)
        );
    }

    /// 真靶向入口的 TTY 臂（证据可注入：候选 TTY + 会话级证据）：精确命中唯一候选 →
    /// 改道到该候选；命中不唯一 / 无命中 → 仍拒绝
    #[test]
    fn resolve_target_pid_uses_independent_tty_evidence() {
        let s = sess("s-2", "/work/demo", 44161);
        let mut ev = TargetEvidence {
            processes: procs_in_cwd("/work/demo", &[44161, 44638]),
            candidate_ttys: vec![
                (44161, "/dev/ttys000".to_string()),
                (44638, "/dev/ttys006".to_string()),
            ],
            session_tty: Some("/dev/ttys006".to_string()),
        };
        assert_eq!(
            super::resolve_target_pid(&s, &ev, "macos"),
            Ok(44638),
            "独立证据指明目标在 44638 → 改道（卡片 pid 被丢弃）"
        );
        // 命中不唯一（两候选同 TTY）→ 不猜
        ev.candidate_ttys = vec![
            (44161, "/dev/ttys000".to_string()),
            (44638, "/dev/ttys000".to_string()),
        ];
        ev.session_tty = Some("/dev/ttys000".to_string());
        assert_eq!(
            super::resolve_target_pid(&s, &ev, "macos"),
            Err(super::AmbiguousTarget(2))
        );
        // Windows：采数未实现 → 即便证据里塞了 TTY 也不据未验数据消歧
        ev.candidate_ttys = vec![(44638, "/dev/ttys006".to_string())];
        ev.session_tty = Some("/dev/ttys006".to_string());
        assert_eq!(
            super::resolve_target_pid(&s, &ev, "windows"),
            Err(super::AmbiguousTarget(2))
        );
    }

    /// plan 原文用例①：精确 TTY 命中压过同 cwd 歧义（Mac 实测三胞胎的真实 pid/tty）
    #[test]
    fn exact_tty_match_beats_cwd_ambiguity() {
        // 三胞胎同 cwd：候选 = [(pid 44161, ttys000), (pid 44638, ttys006), (pid 45543, ttys007)]
        // 目标会话进程 tty = ttys006 → 唯一命中 pid 44638
        let cands = vec![
            (44161u32, "ttys000"),
            (44638, "ttys006"),
            (45543, "ttys007"),
        ];
        assert_eq!(super::resolve_by_tty(&cands, "ttys006"), Some(44638));
        assert_eq!(super::resolve_by_tty(&cands, "ttys999"), None);
    }

    /// plan 原文用例②：取不到 TTY 时单候选放行、多候选拒绝
    #[test]
    fn no_tty_multi_candidate_rejects() {
        assert_eq!(super::fallback_cwd_policy(1), super::CwdFallback::Allow(1));
        assert_eq!(super::fallback_cwd_policy(3), super::CwdFallback::Reject(3));
    }

    /// 回退策略完整表（含 n=0）：**n=0 也 Reject(0)** 是 plan 原文语义——本函数只管
    /// 「歧义策略」，`0 个候选`不是歧义，[`decide_target`] 在调用前短路（见下条用例）。
    /// 此断言是**有意钉住的非对称**：改 `fallback_cwd_policy` 为 `n==0 => Allow` 会让
    /// 本用例先红，提醒同步修订两处语义。
    #[test]
    fn fallback_policy_table_including_zero() {
        assert_eq!(super::fallback_cwd_policy(0), super::CwdFallback::Reject(0));
        assert_eq!(super::fallback_cwd_policy(2), super::CwdFallback::Reject(2));
        assert_eq!(super::fallback_cwd_policy(9), super::CwdFallback::Reject(9));
    }

    /// **单候选 + 无 TTY → 照常投递**（回归锁：不得把一切非 TTY 路径一起打死——
    /// macOS 无 TTY 证据、Windows 恒无 TTY，误拒即全平台注入瘫痪）
    #[test]
    fn single_candidate_without_tty_proceeds() {
        assert_eq!(
            super::decide_target(&[44161], &[], None, 44161, "windows"),
            super::TargetDecision::Resolved(44161)
        );
        // macOS 同判（候选唯一时 TTY 有无不影响）
        assert_eq!(
            super::decide_target(&[44161], &[], None, 44161, "macos"),
            super::TargetDecision::Resolved(44161)
        );
        // 有 TTY 表也只影响 ≥2 候选的臂：单候选恒放行
        assert_eq!(
            super::decide_target(&[44161], &[(44161, "/dev/ttys000")], None, 44161, "macos"),
            super::TargetDecision::Resolved(44161)
        );
    }

    /// 候选 0（进程已死 / cwd 口径漂移）**不是歧义**：原样放行（交既有链路如实失败，
    /// 不产出「同目录存在 0 个候选会话」这种荒谬文案）
    #[test]
    fn zero_candidates_is_not_ambiguity() {
        assert_eq!(
            super::decide_target(&[], &[], None, 7, "windows"),
            super::TargetDecision::Resolved(7)
        );
        assert_eq!(
            super::decide_target(&[], &[], None, 7, "macos"),
            super::TargetDecision::Resolved(7)
        );
    }

    /// macOS + 独立 TTY 证据：精确命中的候选胜出（真实 `/dev/` 全路径形态）
    #[test]
    fn macos_exact_tty_resolves_sibling() {
        let pids = [44161u32, 44638, 45543];
        let ttys = [
            (44161u32, "/dev/ttys000"),
            (44638, "/dev/ttys006"),
            (45543, "/dev/ttys007"),
        ];
        assert_eq!(
            super::decide_target(&pids, &ttys, Some("/dev/ttys006"), 44161, "macos"),
            super::TargetDecision::Resolved(44638),
            "TTY 证据指明目标在 44638（即使卡片 pid 是 44161——配对交叉的正是这种形态）"
        );
    }

    /// TTY 相等是**全路径相等**：`/dev/ttys100` 不得命中 `/dev/ttys1000`（前缀撞号，
    /// 与 `parse_panes_find` 同回归锁）——命中不了即拒绝，不猜
    #[test]
    fn tty_match_is_exact_not_prefix() {
        let pids = [1u32, 2];
        let ttys = [(1u32, "/dev/ttys1000"), (2, "/dev/ttys100")];
        assert_eq!(
            super::decide_target(&pids, &ttys, Some("/dev/ttys10"), 1, "macos"),
            super::TargetDecision::Ambiguous(2),
            "无命中 → 拒绝"
        );
        assert_eq!(
            super::decide_target(&pids, &ttys, Some("/dev/ttys100"), 1, "macos"),
            super::TargetDecision::Resolved(2)
        );
    }

    /// TTY 证据命中**不唯一**（两候选同 TTY = 同终端内多实例）→ TTY 不具判别力 → 拒绝
    /// （`resolve_by_tty` 的 `find` 首个命中在这里就是「猜」，不得采信）
    #[test]
    fn duplicated_tty_match_is_refused() {
        let pids = [1u32, 2, 3];
        let ttys = [
            (1u32, "/dev/ttys000"),
            (2, "/dev/ttys000"),
            (3, "/dev/ttys007"),
        ];
        assert_eq!(
            super::decide_target(&pids, &ttys, Some("/dev/ttys000"), 1, "macos"),
            super::TargetDecision::Ambiguous(3)
        );
    }

    /// 无 TTY 证据 / 非 macOS：候选 ≥2 一律拒绝（**Windows 生产行为**——拒绝规则
    /// 在本机可达且被钉住，不是死代码）
    #[test]
    fn multi_candidate_without_evidence_refuses() {
        let pids = [44161u32, 44638, 45543];
        let ttys = [
            (44161u32, "/dev/ttys000"),
            (44638, "/dev/ttys006"),
            (45543, "/dev/ttys007"),
        ];
        // macOS 有候选 TTY 表但**无会话级证据** → 不猜
        assert_eq!(
            super::decide_target(&pids, &ttys, None, 44161, "macos"),
            super::TargetDecision::Ambiguous(3)
        );
        // macOS 有证据但一个都不命中 → 不猜
        assert_eq!(
            super::decide_target(&pids, &ttys, Some("/dev/ttys999"), 44161, "macos"),
            super::TargetDecision::Ambiguous(3)
        );
        // Windows：即便调用方塞进 TTY 表也不据未验数据消歧（采数未实现）
        assert!(!super::tty_collection_supported("windows"));
        assert_eq!(
            super::decide_target(&pids, &ttys, Some("/dev/ttys006"), 44161, "windows"),
            super::TargetDecision::Ambiguous(3)
        );
        // 其他平台同样保守
        assert_eq!(
            super::decide_target(&pids, &ttys, Some("/dev/ttys006"), 44161, "linux"),
            super::TargetDecision::Ambiguous(3)
        );
    }

    /// 拒绝文案定形（plan 原文；N = 候选数）：写侧落账与端点回执的单一措辞出口
    #[test]
    fn ambiguous_error_names_candidate_count() {
        let msg = super::ambiguous_target_error(3);
        assert_eq!(
            msg,
            "同目录存在 3 个候选会话，无法确定投递目标（已知缺口 L13 修复）：请关闭多余窗口或改用无头通道"
        );
    }

    /// 候选过滤（纯核）：归一化域与解析器配对同一套——分隔符方向/尾部分隔符等价，
    /// 不同目录不混入；无有效 cwd → 空表（无从判定，写侧放行）
    #[test]
    fn candidates_share_parser_normalization_domain() {
        let procs = vec![
            proc_with_cwd(11, Some("/work/demo/")),
            proc_with_cwd(22, Some("/work/demo")),
            proc_with_cwd(33, Some("/work/other")),
            proc_with_cwd(44, None),
        ];
        assert_eq!(super::candidates_in_cwd(&procs, "/work/demo"), vec![11, 22]);
        assert_eq!(super::candidates_in_cwd(&procs, "/work/other/"), vec![33]);
        assert!(super::candidates_in_cwd(&procs, "/work/none").is_empty());
        // 空 / 根路径（归一为空串）：无从判定 → 空表（写侧按无候选放行）
        assert!(super::candidates_in_cwd(&procs, "").is_empty());
        assert!(super::candidates_in_cwd(&procs, "/").is_empty());
        // 未注册工具 / 空 cwd 的生产入口同样不产出候选（零进程扫描的早退路径）
        assert!(super::cwd_candidate_pids("no-such-tool", "/work/demo").is_empty());
        assert!(super::cwd_candidate_pids("claude", "").is_empty());
    }

    /// Windows/非 macOS：TTY 采数未实现 → 恒 None（plan 边界钉住；本机即此臂）
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn tty_collection_unimplemented_off_macos() {
        assert!(super::agent_tty(std::process::id()).is_none());
        assert!(super::collect_candidate_ttys(&[std::process::id()]).is_empty());
        assert!(!super::tty_collection_supported(std::env::consts::OS));
    }

    fn proc_with_cwd(pid: u32, cwd: Option<&str>) -> AgentProcess {
        AgentProcess {
            pid,
            cpu_usage: 0.0,
            cwd: cwd.map(std::path::PathBuf::from),
            form: ProcessForm::Cli,
            exe: None,
        }
    }
}
