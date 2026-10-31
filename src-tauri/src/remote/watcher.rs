// SessionWatcher：500ms 轮询扫描总线版本，版本变化时对快照 diff → broadcast
// （跃迁判定唯一去重点）。watcher 不自行扫描——扫描归 adapter::scan_bus 单线程
// 消费者（SSE/推送/桌面通知）只收边沿事件，不独立去重

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use once_cell::sync::Lazy;
use tokio::sync::broadcast;

use crate::session::{Session, SessionStatus};

/// 事件通道容量（broadcast 语义：消费者滞后超容量时丢最旧事件——宁可缺边沿也不阻塞
/// watcher 生产；SSE 首帧全量快照可校正状态）
pub const EVENT_CHANNEL_CAPACITY: usize = 64;

/// 版本轮询周期：只读一次原子量，代价可忽略；实际出卡节奏由扫描总线的节拍决定
/// （watcher 自身不再是扫描者，故此处不设扫描周期）
const VERSION_POLL: Duration = Duration::from_millis(500);

/// 跃迁事件（wire 形状 = 移动端契约，评审 Important 1 修复）：字段一律 camelCase
/// 序列化（与 Session 同款 rename_all）——Task 6 前端按 ev.sessionId / ev.agentType /
/// ev.projectName / ev.lastMessage 读取，snake_case 会静默 undefined
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TransitionEvent {
    pub session_id: String,
    pub agent_type: String,
    pub from: String,
    pub to: String,
    pub project_name: String,
    pub last_message: Option<String>,
    /// 「跃迁由子 agent 活动引发」打标（T4 F2b / 决策 4）：取 curr 会话的
    /// `flap_from_subagent_activity`——移动端 Board 横幅/提示音/振动入口按本字段
    /// 默认静默（桌面端经 /sessions 快照的同名字段消费，两路同一权威源）
    pub flap_from_subagent_activity: bool,
    pub ts: i64,
}

/// 事件通道单例槽：通道全进程唯一（RemoteState 装配与扫描循环共用同一 sender）
static EVENT_TX: OnceLock<broadcast::Sender<TransitionEvent>> = OnceLock::new();

/// 扫描循环句柄槽（评审 Minor ③：对齐 SERVER_HANDLE 的 `Mutex<Option<JoinHandle>>` +
/// 完成自愈——Once 的闭包 panic 会把闩**永久中毒**，之后所有 start 调用都 panic，
/// watcher 再也起不来；句柄式在任务结束（panic / abort / runtime 停机）后由
/// is_finished 判定为死，允许重建自愈）
static LOOP_HANDLE: Lazy<Mutex<Option<tauri::async_runtime::JoinHandle<()>>>> =
    Lazy::new(|| Mutex::new(None));

/// 事件 sender（幂等：首次调用建通道，之后恒返回同一 sender），**不启动**扫描循环。
/// RemoteState.watcher_tx 装配用：即便服务器从未启动成功也不会遗留后台扫描
pub fn event_sender() -> broadcast::Sender<TransitionEvent> {
    channel_at(&EVENT_TX)
}

/// 通道幂等创建内核（可测核心，无全局依赖）
fn channel_at(
    slot: &OnceLock<broadcast::Sender<TransitionEvent>>,
) -> broadcast::Sender<TransitionEvent> {
    slot.get_or_init(|| broadcast::channel(EVENT_CHANNEL_CAPACITY).0)
        .clone()
}

pub struct SessionWatcher;

impl SessionWatcher {
    /// 启动 watcher 循环（500ms 轮询扫描总线版本），返回事件 broadcast sender
    /// （与 event_sender 同一通道）。
    /// 幂等：循环存活期间重复调用复用既有任务、不重复 spawn（见 LOOP_HANDLE）。
    /// 调用点必须在**服务器实际监听成功后**（serve 内 bind 之后）；绑定失败不留后台循环。
    pub fn start() -> broadcast::Sender<TransitionEvent> {
        let mut h = LOOP_HANDLE.lock().unwrap();
        start_with(&EVENT_TX, &mut h, |tx| {
            // 见 M2 SERVER_HANDLE 同款裁决（remote/mod.rs）：裸 tokio::spawn 在无
            // runtime 上下文的线程上 panic（no reactor running），
            // tauri::async_runtime::spawn 内部先 enter 再 spawn，任意线程可用
            tauri::async_runtime::spawn(watch_loop(tx))
        })
    }
}

/// 循环句柄存活判定（纯函数，对齐 remote::handle_is_live）：存在且任务未结束才算活。
/// 已完成（任务退出 / panic 结束 / abort）视为不存在，允许重新 spawn 自愈
fn loop_handle_is_live(h: &Option<tauri::async_runtime::JoinHandle<()>>) -> bool {
    h.as_ref()
        .map(|jh| !jh.inner().is_finished())
        .unwrap_or(false)
}

/// 启动内核（可测核心，通道槽 / 句柄槽 / spawn 全注入）：通道幂等创建 + 循环存活期间
/// 只 spawn 一次；已结束的旧句柄（任务退出 / panic）取走重建，自愈对齐 remote::start_server_core
fn start_with(
    tx_slot: &OnceLock<broadcast::Sender<TransitionEvent>>,
    handle_slot: &mut Option<tauri::async_runtime::JoinHandle<()>>,
    spawn: impl FnOnce(broadcast::Sender<TransitionEvent>) -> tauri::async_runtime::JoinHandle<()>,
) -> broadcast::Sender<TransitionEvent> {
    let tx = channel_at(tx_slot);
    if loop_handle_is_live(handle_slot) {
        return tx; // 循环存活：幂等复用
    }
    *handle_slot = None; // 自愈：取走已完成（或本就为空）的旧句柄
    *handle_slot = Some(spawn(tx.clone()));
    tx
}

/// 循环主体：纯消费者——500ms 读一次扫描总线版本号，版本变化才 diff 共享快照并广播。
/// 不再自行扫描（扫描归 adapter::scan_bus），本循环里没有阻塞重活，无需 spawn_blocking。
async fn watch_loop(tx: broadcast::Sender<TransitionEvent>) {
    let mut prev: Vec<Session> = Vec::new();
    let mut last_version = 0u64;
    loop {
        tokio::time::sleep(VERSION_POLL).await;
        // 零订阅者：不 diff、不保留基线（恢复订阅后从当前快照重建，不再为历史
        // 状态刷一轮伪跃迁）；版本号照常推进，避免恢复订阅后做无谓的重放
        if tx.receiver_count() == 0 {
            prev.clear();
            last_version = crate::adapter::scan_bus::version();
            continue;
        }
        let v = crate::adapter::scan_bus::version();
        if v == last_version {
            continue;
        }
        last_version = v;
        // 版本已推进但快照缺失（理论窗口：首次发布前）：本轮无数据可比，基线保持，
        // 下轮以 prev 为比较起点——不重复做同一版本的工作
        let Some(snap) = crate::adapter::scan_bus::snapshot() else {
            continue;
        };
        for e in diff_transitions(&prev, &snap.data.sessions, now_ms()) {
            // 订阅者可能在两轮之间断开（send 返回 Err）：正常，忽略
            let _ = tx.send(e);
        }
        prev = snap.data.sessions.clone();
    }
}

/// 生产时钟（毫秒）
fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// 纯函数：快照 diff → 边沿事件（可单测；去重唯一来源）。
/// 语义（控制者裁决 2026-09-15，对简报的修订）：**只对 prev 中已存在且状态变化的会话
/// 发事件**——新增会话（prev 无）不发（新卡由全量快照自然呈现，事件语义是「状态变化」；
/// 也避免服务开启瞬间给全部在跑会话刷一轮伪跃迁）。
/// 键取 (工具, 会话 id)：与 adapter::dedup_sessions 同一唯一性口径——id 只在工具内唯一，
/// 不同工具撞 id 不得互相顶掉基线。
/// `now` 由调用方注入毫秒时间戳（生产 = now_ms；测试注入固定值做确定性断言）
pub fn diff_transitions(prev: &[Session], curr: &[Session], now: i64) -> Vec<TransitionEvent> {
    let prev_map: HashMap<(&'static str, &str), &Session> = prev
        .iter()
        .map(|s| ((s.agent_type.tool_id(), s.id.as_str()), s))
        .collect();
    curr.iter()
        .filter_map(|c| {
            let p = prev_map.get(&(c.agent_type.tool_id(), c.id.as_str()))?;
            if p.status != c.status {
                Some(TransitionEvent {
                    session_id: c.id.clone(),
                    agent_type: c.agent_type.tool_id().to_string(),
                    from: status_wire(&p.status),
                    to: status_wire(&c.status),
                    project_name: c.project_name.clone(),
                    last_message: c.last_message.clone(),
                    flap_from_subagent_activity: c.flap_from_subagent_activity,
                    ts: now,
                })
            } else {
                None
            }
        })
        .collect()
}

/// 状态 wire 字符串（serde 单源）：SessionStatus 带 `#[serde(rename_all = "lowercase")]`，
/// 其 serde 序列化即 wire 形态（与 /sessions 快照的 status 字段同一路径，前端
/// src/types/session.ts 的 SessionStatus 联合类型消费）。
/// 不用 `format!("{:?}", variant).to_lowercase()`：那是「变体名小写 == wire 字符串」的
/// 隐式约定（先例警告与穷尽 match 先例见 session/model.rs:20 的 tool_id 注释）
fn status_wire(s: &SessionStatus) -> String {
    serde_json::to_value(s)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_else(|| {
            // 理论不可达（C 风格枚举 + derive(Serialize) 必产出 String）：防御性降级为
            // 空串而非 panic——watcher 循环不得因单个事件构造异常而中断
            log::warn!("SessionStatus 序列化未产出字符串，事件状态字段降级为空");
            String::new()
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use crate::session::{AgentType, ProcessForm};

    fn session(id: &str, status: SessionStatus, project: &str) -> Session {
        Session {
            id: id.into(),
            agent_type: AgentType::Claude,
            project_name: project.into(),
            project_path: String::new(),
            title: None,
            git_branch: None,
            github_url: None,
            status,
            last_message: None,
            last_message_role: None,
            last_message_subagent_report: false,
            flap_from_subagent_activity: false,
            last_activity_at: String::new(),
            pid: 1,
            cpu_usage: 0.0,
            active_subagent_count: 0,
            form: ProcessForm::Cli,
            jump_supported: false,
            unread: false,
        }
    }

    fn ev(id: &str) -> TransitionEvent {
        TransitionEvent {
            session_id: id.into(),
            agent_type: "claude".into(),
            from: "idle".into(),
            to: "processing".into(),
            project_name: "p".into(),
            last_message: None,
            flap_from_subagent_activity: false,
            ts: 1,
        }
    }

    /// T4 F2b：跃迁事件携带当前会话的子 agent 活动打标（取 curr 会话的值）——
    /// 移动端 Board 的静默门按该字段默认静默（决策 4：后端判定、前端只消费布尔）
    #[test]
    fn diff_carries_flap_tag_from_curr_session() {
        let prev = session("a", SessionStatus::Processing, "p");
        let mut curr = session("a", SessionStatus::Idle, "p");
        curr.flap_from_subagent_activity = true;
        let events = diff_transitions(&[prev], &[curr], 1);
        assert_eq!(events.len(), 1);
        assert!(
            events[0].flap_from_subagent_activity,
            "跃迁事件必须携带 curr 会话的打标值"
        );
        // wire 键 camelCase（移动端契约，与 Session 同款 rename_all）
        let v = serde_json::to_value(&events[0]).unwrap();
        assert_eq!(v["flapFromSubagentActivity"], true);
        assert!(v.get("flap_from_subagent_activity").is_none());
        // 未打标会话 → false 原样透传
        let prev = session("b", SessionStatus::Processing, "p");
        let curr = session("b", SessionStatus::Idle, "p");
        let events = diff_transitions(&[prev], &[curr], 1);
        assert!(!events[0].flap_from_subagent_activity);
    }

    /// 控制者裁决（2026-09-15，对简报的修订）：新增会话**不产生**跃迁事件——
    /// 事件语义是「状态变化」，新卡由全量快照自然呈现。故 c 不再计入 events
    #[test]
    fn diff_emits_only_state_changes() {
        let prev = vec![
            session("a", SessionStatus::Processing, "p1"),
            session("b", SessionStatus::Idle, "p2"),
        ];
        let curr = vec![
            session("a", SessionStatus::Waiting, "p1"), // changed → emit
            session("b", SessionStatus::Idle, "p2"),    // same → skip
            session("c", SessionStatus::Processing, "p3"), // new → skip（裁决）
        ];
        let events = diff_transitions(&prev, &curr, 12345);
        assert_eq!(
            events.len(),
            1,
            "仅 a（状态变化）触发事件；b 同态、c 新增均不发"
        );
        assert_eq!(events[0].session_id, "a");
        assert_eq!(events[0].from, "processing");
        assert_eq!(events[0].to, "waiting");
        assert_eq!(
            events[0].agent_type, "claude",
            "agent_type 用 tool_id() 穷尽映射"
        );
        assert_eq!(events[0].project_name, "p1");
        assert_eq!(events[0].ts, 12345, "ts 必须原样采用注入时钟");
    }

    #[test]
    fn diff_no_change_no_event() {
        let prev = vec![session("a", SessionStatus::Idle, "p")];
        let curr = vec![session("a", SessionStatus::Idle, "p")];
        assert!(diff_transitions(&prev, &curr, 1).is_empty());
    }

    /// 显式锁定「新增不发事件」语义（控制者裁决⑤）：首轮空基线、已有会话旁新增、
    /// 消失会话三个场景均不得产生事件
    #[test]
    fn diff_skips_new_sessions() {
        // 首轮基线为空（watcher 冷启动）：全部当前会话都是"新增"→ 零事件，
        // 否则服务开启瞬间会给所有在跑会话刷一轮伪跃迁
        let first = vec![session("a", SessionStatus::Processing, "p1")];
        assert!(
            diff_transitions(&[], &first, 1).is_empty(),
            "空基线首轮不得产生事件"
        );
        // 已有基线旁出现新会话 → 只发既有会话的变化，新会话不发
        let prev = vec![session("a", SessionStatus::Idle, "p1")];
        let curr = vec![
            session("a", SessionStatus::Waiting, "p1"),
            session("new", SessionStatus::Processing, "p9"),
        ];
        let events = diff_transitions(&prev, &curr, 1);
        assert_eq!(events.len(), 1, "仅既有会话 a 的变化计事件");
        assert_eq!(events[0].session_id, "a");
        // 会话从当前快照消失（进程退出）→ 不产生事件（diff 只遍历 curr）
        assert!(
            diff_transitions(&prev, &[], 1).is_empty(),
            "消失会话不产生事件（当前快照已无该会话）"
        );
    }

    /// 状态 wire 字符串与前端 SessionStatus 联合类型（src/types/session.ts）一致：
    /// 全变体枚举断言，serde rename 漂移即刻变红
    #[test]
    fn status_wire_matches_frontend_union() {
        for (s, want) in [
            (SessionStatus::Waiting, "waiting"),
            (SessionStatus::Processing, "processing"),
            (SessionStatus::Thinking, "thinking"),
            (SessionStatus::Compacting, "compacting"),
            (SessionStatus::Idle, "idle"),
            (SessionStatus::Finished, "finished"),
        ] {
            assert_eq!(
                status_wire(&s),
                want,
                "wire 字符串必须与前端 SessionStatus 联合类型逐字一致"
            );
        }
    }

    /// 事件 wire 形状锁定（评审 Important 1）：Task 6 前端按 camelCase 读
    /// ev.sessionId / ev.agentType / ev.projectName / ev.lastMessage——若仍是 snake_case，
    /// 这些字段会静默 undefined（横幅工具/项目/消息预览全空）。与 Session 同款 rename_all
    #[test]
    fn transition_event_serializes_camel_case() {
        let mut e = ev("s1");
        e.last_message = Some("hello".into());
        let v = serde_json::to_value(&e).unwrap();
        let obj = v.as_object().unwrap();
        for k in [
            "sessionId",
            "agentType",
            "projectName",
            "lastMessage",
            "from",
            "to",
            "flapFromSubagentActivity",
            "ts",
        ] {
            assert!(obj.contains_key(k), "缺少 camelCase 键 {k}：{obj:?}");
        }
        for k in [
            "session_id",
            "agent_type",
            "project_name",
            "last_message",
            "flap_from_subagent_activity",
        ] {
            assert!(
                !obj.contains_key(k),
                "不得出现 snake_case 键 {k}（前端契约 camelCase）"
            );
        }
        assert_eq!(v["sessionId"], "s1");
        assert_eq!(v["lastMessage"], "hello");
        assert_eq!(v["from"], "idle");
        assert_eq!(v["to"], "processing");
    }

    /// 全局通道单例：event_sender 每次返回同一通道——RemoteState.watcher_tx（订阅端）与
    /// 扫描循环（生产端）必须指向同一通道，否则 SSE 永远收不到事件（静默断链）。
    /// 零污染：只建一个空通道，不 spawn、不触 DB/文件系统
    #[test]
    fn event_sender_is_singleton_channel() {
        let a = event_sender();
        let b = event_sender();
        assert!(
            a.same_channel(&b),
            "event_sender 必须返回全进程唯一通道（Task 6 的 SSE 订阅依赖此不变量）"
        );
    }

    /// 幂等启动内核（评审 Minor ③ 自愈版）：循环存活期间重复 start 必须复用既有任务、
    /// 不重复 spawn（重复 spawn 会让同一跃迁广播多份）；任务已结束时视为不存在、允许重建。
    /// 零污染：槽都是局部变量、spawn 是计数闭包，不触全局 EVENT_TX/LOOP_HANDLE、不扫真实会话
    #[test]
    fn start_with_spawns_once_reuses_and_self_heals() {
        let tx_slot: OnceLock<broadcast::Sender<TransitionEvent>> = OnceLock::new();
        let handle_slot: Mutex<Option<tauri::async_runtime::JoinHandle<()>>> = Mutex::new(None);
        let spawns = Arc::new(AtomicUsize::new(0));
        // 假 spawn：spawn 一个 pending 任务（代表存活循环），并计数
        let fake_spawn = |spawns: &Arc<AtomicUsize>| {
            let spawns = spawns.clone();
            move |_tx: broadcast::Sender<TransitionEvent>| {
                spawns.fetch_add(1, Ordering::SeqCst);
                tauri::async_runtime::spawn(std::future::pending::<()>())
            }
        };
        let (s1, s2) = {
            let mut h = handle_slot.lock().unwrap();
            let s1 = start_with(&tx_slot, &mut h, fake_spawn(&spawns));
            // (1) 运行中：幂等复用，不重复 spawn
            let s2 = start_with(&tx_slot, &mut h, fake_spawn(&spawns));
            (s1, s2)
        };
        assert_eq!(
            spawns.load(Ordering::SeqCst),
            1,
            "第二次 start 必须复用存活循环，不得重复 spawn"
        );
        assert!(
            loop_handle_is_live(&handle_slot.lock().unwrap()),
            "运行中句柄应判活（is_finished=false）"
        );
        // 同一通道证明：s1 发出、s2 订阅可收
        let mut rx = s2.subscribe();
        s1.send(ev("x")).unwrap();
        assert_eq!(rx.try_recv().unwrap().session_id, "x");

        // (2) 自愈：abort 循环（模拟任务终止：panic 结束 / runtime 停机）→ 视为死，可重建
        handle_slot.lock().unwrap().as_ref().unwrap().abort();
        let mut waited = 0u32;
        while !handle_slot
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .inner()
            .is_finished()
        {
            assert!(waited < 5000, "abort 后 5s 内任务未结束，测试环境异常");
            std::thread::sleep(std::time::Duration::from_millis(1));
            waited += 1;
        }
        let s3 = {
            let mut h = handle_slot.lock().unwrap();
            start_with(&tx_slot, &mut h, fake_spawn(&spawns))
        };
        assert_eq!(
            spawns.load(Ordering::SeqCst),
            2,
            "循环已结束必须允许重建（Once 中毒场景下 watcher 再也起不来的自愈）"
        );
        assert!(
            s1.same_channel(&s3),
            "重建后仍须复用同一通道（RemoteState 订阅端不断链）"
        );
        handle_slot.lock().unwrap().as_ref().unwrap().abort(); // 清理 pending 任务
    }

    /// 源码锁（Task 5）：watcher 只消费扫描总线快照，不得再自行扫描——否则进程内
    /// 出现第二个轮询扫描者，违反「扫描总线是唯一扫描者」契约。注释里提及历史可以，
    /// 可执行调用一律禁止。`concat!` 拆开字面量：include_str! 会把本测试自身读进
    /// `src`，整串字面量会自我命中（永久红）
    #[test]
    fn watcher_source_does_not_call_get_all_sessions() {
        let src = include_str!("watcher.rs");
        assert!(
            !src.contains(concat!("crate::adapter::", "get_all_sessions,")),
            "watcher 不得再调用会话扫描（扫描已归 adapter::scan_bus）"
        );
        assert!(
            !src.contains(concat!("crate::adapter::", "get_all_sessions()")),
            "watcher 不得再调用会话扫描（扫描已归 adapter::scan_bus）"
        );
    }
}
