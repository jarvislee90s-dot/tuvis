// SessionWatcher：2s 周期快照 diff → broadcast（跃迁判定唯一去重点）
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

/// 扫描周期（A3：状态变化 2 秒级到达）
const SCAN_INTERVAL: Duration = Duration::from_secs(2);

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
    /// 启动 2s 周期 watcher，返回事件 broadcast sender（与 event_sender 同一通道）。
    /// 幂等：循环存活期间重复调用复用既有任务、不重复 spawn（见 LOOP_HANDLE）。
    /// 调用点必须在**服务器实际监听成功后**（serve 内 bind 之后）；绑定失败不留后台扫描。
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

/// 周期任务主体：sleep → 单轮内核。sleep 在 scan_tick 之外，保持「守卫判断在 sleep 之后、
/// 扫描之前」的节奏（零订阅者也保持 2s 节拍醒来轻量自检，CPU 代价为一次原子读）
async fn watch_loop(tx: broadcast::Sender<TransitionEvent>) {
    let mut prev: Vec<Session> = Vec::new();
    loop {
        tokio::time::sleep(SCAN_INTERVAL).await;
        scan_tick(&tx, &mut prev, crate::adapter::get_all_sessions, now_ms).await;
    }
}

/// 生产时钟（毫秒）；单测注入固定值
fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// 单轮内核（可测：扫描与时钟注入）。**零订阅者守卫**（评审 Important 2）：
/// 无人订阅（远程已关闭 / 所有 SSE 连接断开）时跳过本轮扫描——否则 watcher 反成
/// 常驻唯一轮询者（桌面窗口隐藏时前端停轮询，watcher 仍在 2s 扫），违反 NFR
/// 「监控扫描不拖慢桌面现有体验」。守卫在 sleep 之后、扫描之前，是单次原子读的零成本检查。
/// 扫描在阻塞线程池执行（与 api.rs 的 sessions handler 同先例）：get_all_sessions 是
/// 同步重活（sysinfo 全进程刷新 + 各工具会话解析），直接 await 会周期性堵死 tokio worker。
/// 返回本轮是否执行了扫描（测试断言点）
async fn scan_tick(
    tx: &broadcast::Sender<TransitionEvent>,
    prev: &mut Vec<Session>,
    scan: impl FnOnce() -> crate::session::SessionsResponse + Send + 'static,
    now_ms: impl FnOnce() -> i64,
) -> bool {
    if tx.receiver_count() == 0 {
        return false; // 零订阅者：跳过扫描，基线保持（下轮订阅恢复后立即照常比对）
    }
    let resp = match tokio::task::spawn_blocking(scan).await {
        Ok(resp) => resp,
        Err(e) => {
            // 扫描任务 panic（如 DB 锁中毒）：保留旧基线，下轮重试，任务不退出
            log::error!("SessionWatcher 会话扫描任务异常: {e}");
            return true; // 已尝试扫描（失败），不因单轮失败改变节拍语义
        }
    };
    let now = now_ms();
    for e in diff_transitions(prev, &resp.sessions, now) {
        // 订阅者可能在扫描期间断开（send 返回 Err）：正常，忽略
        let _ = tx.send(e);
    }
    *prev = resp.sessions;
    true
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
            unread_turned_green_at_ms: None,
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

    /// 零订阅者守卫（评审 Important 2）：无人订阅时（远程已关闭 / SSE 全断）单轮内核
    /// 必须跳过扫描——否则 watcher 反成常驻唯一轮询者（违反 NFR「监控扫描不拖慢桌面
    /// 现有体验」）。有订阅者时正常扫描且事件广播到订阅端
    #[tokio::test]
    async fn scan_tick_skips_scanning_without_subscribers() {
        let (tx, _) = broadcast::channel(64);
        let scans = Arc::new(AtomicUsize::new(0));
        let mut prev: Vec<Session> = Vec::new();

        // (1) 零订阅者：扫描函数零调用（计数闭包证明），基线不变
        let scanned = scan_tick(
            &tx,
            &mut prev,
            {
                let scans = scans.clone();
                move || {
                    scans.fetch_add(1, Ordering::SeqCst);
                    crate::session::SessionsResponse {
                        sessions: vec![],
                        total_count: 0,
                        waiting_count: 0,
                    }
                }
            },
            || 1,
        )
        .await;
        assert!(!scanned, "零订阅者时本轮必须报告未扫描");
        assert_eq!(
            scans.load(Ordering::SeqCst),
            0,
            "零订阅者时扫描函数不得被调用"
        );

        // (2) 有订阅者：正常扫描、基线刷新、状态变化广播到订阅端
        let mut rx = tx.subscribe();
        let mut prev = vec![session("a", SessionStatus::Idle, "p")];
        let scanned = scan_tick(
            &tx,
            &mut prev,
            || crate::session::SessionsResponse {
                sessions: vec![session("a", SessionStatus::Waiting, "p")],
                total_count: 1,
                waiting_count: 1,
            },
            || 777,
        )
        .await;
        assert!(scanned, "有订阅者时必须执行扫描");
        assert_eq!(prev.len(), 1, "基线应刷新为最新快照");
        assert_eq!(prev[0].status, SessionStatus::Waiting);
        let e = rx.try_recv().expect("状态变化应广播给订阅者");
        assert_eq!(e.session_id, "a");
        assert_eq!(e.from, "idle");
        assert_eq!(e.to, "waiting");
        assert_eq!(e.ts, 777, "ts 采用注入时钟（确定性断言）");

        // (3) 订阅者断开（模拟 toggle 关闭 / SSE 断线）→ 恢复零扫描
        drop(rx);
        let scanned = scan_tick(
            &tx,
            &mut prev,
            {
                let scans = scans.clone();
                move || {
                    scans.fetch_add(1, Ordering::SeqCst);
                    crate::session::SessionsResponse {
                        sessions: vec![],
                        total_count: 0,
                        waiting_count: 0,
                    }
                }
            },
            || 1,
        )
        .await;
        assert!(!scanned, "订阅者断开后应回到零扫描");
        assert_eq!(
            scans.load(Ordering::SeqCst),
            0,
            "订阅者断开后扫描函数不得被调用"
        );
    }
}
