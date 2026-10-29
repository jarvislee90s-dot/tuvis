# 扫描总线与自适应节奏 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把"每个界面各自触发全量扫描"改成单一后端扫描器 + 共享快照 + 2s/10s/30s 自适应节奏，并把用量采集改成"可见即采 + 后端 10 分钟兜底"，使挂机 CPU ≤3% 单核、扫描 ≤6 次/分。

**Architecture:** 新增 `adapter::scan_bus`（Cadence 纯状态机 + 常驻扫描线程 + `Arc<Snapshot>` + 单飞 + `ensure_fresh`），`commands::get_all_sessions`、remote `/sessions`、SSE 首帧、托盘全部读快照；`remote::watcher` 改为订阅快照版本做 diff；事件唤醒用 1s mtime 轮询；用量新增后端 ticker，前端改为 active 边沿 `collect(true)`。

**Tech Stack:** Rust（Tauri 2、sysinfo 0.39、rusqlite 0.40、tokio）+ React 19 / TypeScript / TanStack Query / Vitest。

**Spec:** `docs/engineering/specs/2026-10-29-scan-bus-and-adaptive-cadence-requirements.md`（F1–F10 为本计划的需求来源）

---

## 文件结构

**新建**

| 文件 | 职责 |
| --- | --- |
| `src-tauri/src/adapter/cadence.rs` | 纯状态机：三信号 → 2s/10s/30s，含防抖 |
| `src-tauri/src/adapter/scan_bus.rs` | 常驻扫描线程、单飞、快照发布、`ensure_fresh`、档位/心跳、托盘限频 |
| `src-tauri/src/adapter/wakeup.rs` | 1s mtime 轮询 → `request_scan`；清理陈旧 hook 事件 |
| `src-tauri/src/services/usage/ticker.rs` | 用量后端 10 分钟兜底采集 |
| `src/lib/query/queries/scanBus.ts` | 前端档位订阅 `useScanMode` + 可见性心跳 |
| `tests/scanBus/scanMode.test.tsx` | 前端档位/心跳测试 |
| `tests/usage/collectTrigger.test.tsx` | 用量激活即采（force）测试 |
| `scripts/measure-scan-bus.ps1` | 验收测量脚本（CPU / 扫描次数） |

**修改**

| 文件 | 改动 |
| --- | --- |
| `src-tauri/src/adapter/mod.rs` | 注册新模块；`get_all_sessions_inner` 改 `pub(crate)`；进程两段式；日志降噪 |
| `src-tauri/src/commands/session.rs` | 命令读快照；移除托盘更新（迁入 scan_bus）；新增 `scan_bus_status` / `scan_bus_watch` |
| `src-tauri/src/commands/settings.rs` | 归属查询改 `ensure_fresh` |
| `src-tauri/src/remote/watcher.rs` | 去掉自扫，改为订阅快照版本做 diff |
| `src-tauri/src/remote/server.rs` / `remote/mod.rs` | `RemoteState` 新增 `fresh_session_source`；配对计数 10s 缓存；`is_online()` |
| `src-tauri/src/remote/api.rs` | 动作端点改 `fresh_session_source`；SSE 首帧 ensure_fresh + version |
| `src-tauri/src/inject/queue.rs` | flush 复核改 `fresh_session_source` |
| `src-tauri/src/services/usage/mod.rs` | 注册 `ticker` 模块 |
| `src-tauri/src/lib.rs` | setup 启动 scan_bus / wakeup / usage ticker；注册新命令 |
| `src/lib/query/queries/sessions.ts` | `refetchInterval` 跟随档位 |
| `src/lib/query/queries/usage.ts` | active 边沿 `collect(true)`；删除前端 ticker；监听 `usage-collected` |
| `src/components/pet/UsageMiniBar.tsx` | 挂载期间上报 watching |
| `src/App.tsx`（或主窗根组件） | 挂载可见性心跳 |

---

## Task 0: 基线测量脚本与基线记录

**Files:**
- Create: `scripts/measure-scan-bus.ps1`
- Create: `docs/engineering/evidence/2026-10-29-scan-bus-baseline.md`

- [ ] **Step 1: 写测量脚本**

```powershell
# scripts/measure-scan-bus.ps1
param(
  [int]$ProcessId = 0,
  [int]$Seconds = 60,
  [string]$LogPath = "$env:USERPROFILE\.tuvis\logs\mam.log"
)
if ($ProcessId -eq 0) {
  $ProcessId = (Get-Process tuvis -ErrorAction Stop | Select-Object -First 1).Id
}
$p = Get-Process -Id $ProcessId
$c1 = $p.TotalProcessorTime.TotalSeconds
$w1 = Get-Date
Start-Sleep -Seconds $Seconds
$p = Get-Process -Id $ProcessId
$c2 = $p.TotalProcessorTime.TotalSeconds
$w2 = Get-Date
$wall = ($w2 - $w1).TotalSeconds
$cpu = $c2 - $c1
# 扫描次数：日志中 adapter] OpenCode 行数（= 全量扫描轮数）
$since = $w1.ToUniversalTime().ToString("yyyy-MM-ddTHH:mm:ss")
$lines = Get-Content $LogPath -Tail 20000 | Where-Object {
  $_ -match "adapter\] OpenCode:" -and $_ -ge "[$since"
}
$scans = ($lines | Measure-Object).Count
$hours = $wall / 3600.0
Write-Output ("pid={0} wall={1:N1}s cpu={2:N2}s avgCPU={3:N2}% scans={4} scansPerMin={5:N1}" -f `
  $ProcessId, $wall, $cpu, (100 * $cpu / $wall), $scans, ($scans / ($wall / 60.0)))
```

- [ ] **Step 2: 记录基线（改动前，5 分钟）**

Run: `powershell -NoProfile -File scripts/measure-scan-bus.ps1 -Seconds 300`
Expected: 输出形如 `avgCPU=11.x% scansPerMin=56–61`。把输出原样写入 `docs/engineering/evidence/2026-10-29-scan-bus-baseline.md`，并附上时间、进程数（`(Get-Process).Count`）。

- [ ] **Step 3: 提交**

```bash
git add scripts/measure-scan-bus.ps1 docs/engineering/evidence/2026-10-29-scan-bus-baseline.md
git commit -m "test(perf): add scan bus measurement script and record baseline"
```

---

## Task 1: Cadence 纯状态机

**Files:**
- Create: `src-tauri/src/adapter/cadence.rs`
- Modify: `src-tauri/src/adapter/mod.rs`（模块声明）

- [ ] **Step 1: 写失败测试**

在 `src-tauri/src/adapter/cadence.rs` 末尾：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_is_immediate_and_has_priority() {
        let mut c = Cadence::new(0);
        let m = c.observe(Signals { active_sessions: true, remote_online: false, user_watching: false }, 1_000);
        assert_eq!(m, Some(Mode::Active));
        assert_eq!(c.mode(), Mode::Active);
        assert_eq!(c.mode().interval().as_millis(), 2_000);
    }

    #[test]
    fn downgrade_requires_one_stable_interval() {
        let mut c = Cadence::new(0);
        c.observe(Signals { active_sessions: true, remote_online: false, user_watching: false }, 0);
        // 降到 Idle：第一次只立项，不切换
        let m = c.observe(Signals { active_sessions: false, remote_online: true, user_watching: false }, 2_000);
        assert_eq!(m, None);
        assert_eq!(c.mode(), Mode::Active);
        // 满一个 Active 间隔后切换
        let m = c.observe(Signals { active_sessions: false, remote_online: true, user_watching: false }, 4_000);
        assert_eq!(m, Some(Mode::Idle));
    }

    #[test]
    fn away_requires_no_remote_and_no_watcher() {
        let mut c = Cadence::new(0);
        // Idle → Away 同样要防抖
        c.observe(Signals { active_sessions: false, remote_online: true, user_watching: false }, 0);
        c.observe(Signals { active_sessions: false, remote_online: false, user_watching: false }, 10_000);
        let m = c.observe(Signals { active_sessions: false, remote_online: false, user_watching: false }, 20_000);
        assert_eq!(m, Some(Mode::Away));
        assert_eq!(c.mode().interval().as_millis(), 30_000);
    }

    #[test]
    fn candidate_resets_when_signal_flaps_back() {
        let mut c = Cadence::new(0);
        c.observe(Signals { active_sessions: false, remote_online: true, user_watching: false }, 0);
        c.observe(Signals { active_sessions: false, remote_online: false, user_watching: false }, 10_000);
        // 又出现远控 → 候选作废，保持 Idle
        let m = c.observe(Signals { active_sessions: false, remote_online: true, user_watching: false }, 11_000);
        assert_eq!(m, None);
        assert_eq!(c.mode(), Mode::Idle);
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd src-tauri && cargo test adapter::cadence -q`
Expected: 编译失败（`Cadence` / `Signals` / `Mode` 未定义）

- [ ] **Step 3: 实现最小状态机**

```rust
//! 扫描节奏状态机（纯函数，无 IO）。
//! 输入三信号：活跃会话 / 远控在线 / 有人在看；输出 2s / 10s / 30s。
//! 升档立即生效；降档需在候选状态稳定一个「当前档位间隔」后才切换（防抖）。

use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Active,
    Idle,
    Away,
}

impl Mode {
    pub fn interval(self) -> Duration {
        match self {
            Mode::Active => Duration::from_secs(2),
            Mode::Idle => Duration::from_secs(10),
            Mode::Away => Duration::from_secs(30),
        }
    }

    fn rank(self) -> u8 {
        match self {
            Mode::Active => 2,
            Mode::Idle => 1,
            Mode::Away => 0,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Signals {
    pub active_sessions: bool,
    pub remote_online: bool,
    pub user_watching: bool,
}

impl Signals {
    fn target(&self) -> Mode {
        if self.active_sessions {
            Mode::Active
        } else if self.remote_online || self.user_watching {
            Mode::Idle
        } else {
            Mode::Away
        }
    }
}

pub struct Cadence {
    mode: Mode,
    candidate: Option<Mode>,
    candidate_since_ms: i64,
}

impl Cadence {
    pub fn new(_now_ms: i64) -> Self {
        // 启动先按 Idle（首扫完成后按真实信号收敛）
        Self { mode: Mode::Idle, candidate: None, candidate_since_ms: 0 }
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// 返回 Some(new_mode) 表示发生了切换
    pub fn observe(&mut self, signals: Signals, now_ms: i64) -> Option<Mode> {
        let target = signals.target();
        if target == self.mode {
            self.candidate = None;
            return None;
        }
        if target.rank() > self.mode.rank() {
            self.mode = target;
            self.candidate = None;
            return Some(target);
        }
        match self.candidate {
            Some(c) if c == target => {
                if now_ms - self.candidate_since_ms >= self.mode.interval().as_millis() as i64 {
                    self.mode = target;
                    self.candidate = None;
                    Some(target)
                } else {
                    None
                }
            }
            _ => {
                self.candidate = Some(target);
                self.candidate_since_ms = now_ms;
                None
            }
        }
    }
}
```

在 `src-tauri/src/adapter/mod.rs` 顶部模块声明区加入：

```rust
pub mod cadence;
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cd src-tauri && cargo test adapter::cadence -q`
Expected: `4 passed`

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/adapter/cadence.rs src-tauri/src/adapter/mod.rs
git commit -m "feat(scan): add cadence state machine (2s/10s/30s with debounce)"
```

---

## Task 2: Scanner 与共享快照

**Files:**
- Create: `src-tauri/src/adapter/scan_bus.rs`
- Modify: `src-tauri/src/adapter/mod.rs`（`get_all_sessions_inner` 可见性 + 模块声明）

- [ ] **Step 1: 写失败测试（快照版本 / ensure_fresh 超时 / 单飞）**

在 `scan_bus.rs` 末尾：

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{Session, SessionStatus};

    fn resp(status: SessionStatus) -> crate::session::SessionsResponse {
        crate::session::SessionsResponse {
            sessions: vec![],
            total_count: 0,
            waiting_count: if matches!(status, SessionStatus::Waiting) { 1 } else { 0 },
        }
    }

    #[test]
    fn publish_increments_version_and_stores_snapshot() {
        reset_for_test();
        let s1 = publish_for_test(|| resp(SessionStatus::Idle));
        let s2 = publish_for_test(|| resp(SessionStatus::Idle));
        assert_eq!(s1.version, 1);
        assert_eq!(s2.version, 2);
        assert_eq!(snapshot().unwrap().version, 2);
    }

    #[test]
    fn ensure_fresh_returns_cached_within_window() {
        reset_for_test();
        let _ = publish_for_test(|| resp(SessionStatus::Idle));
        let s = ensure_fresh(Duration::from_secs(60), Duration::from_millis(10));
        assert_eq!(s.version, 1, "窗口内的快照不应触发补扫");
    }

    #[test]
    fn ensure_fresh_falls_back_when_no_scanner_thread() {
        reset_for_test();
        // 未 start（无通道）→ 直接同步扫一次并返回
        let s = ensure_fresh(Duration::from_millis(1), Duration::from_millis(50));
        assert!(s.version >= 1);
    }

    #[test]
    fn has_active_sessions_detects_processing() {
        let mut r = resp(SessionStatus::Idle);
        r.sessions.push(Session::test_fixture("s1", SessionStatus::Processing));
        assert!(has_active_sessions(&r));
    }
}
```

> 注：`Session::test_fixture` 若不存在，用现有的测试构造方式（搜索 `Session {` 的构造样板）替换；若无公有构造器，可改断言函数签名为接收 `&[SessionStatus]`。

- [ ] **Step 2: 运行确认失败**

Run: `cd src-tauri && cargo test adapter::scan_bus -q`
Expected: 编译失败（模块/函数未定义）

- [ ] **Step 3: 实现 scan_bus**

```rust
//! 扫描总线：进程内唯一的会话扫描者。
//! 产出 Arc<Snapshot>（版本单调），所有消费方读快照；事件唤醒与动作路径补扫共用同一把单飞。

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use once_cell::sync::Lazy;
use tauri::Manager;

use super::cadence::{Cadence, Mode, Signals};
use crate::session::SessionsResponse;

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub version: u64,
    pub scanned_at: Instant,
    pub data: SessionsResponse,
}

#[derive(Debug, Clone, Copy)]
pub enum ScanReason {
    Periodic,
    Event,
    Action,
    ColdStart,
}

static LAST: Lazy<Mutex<Option<Arc<Snapshot>>>> = Lazy::new(|| Mutex::new(None));
static VERSION: AtomicU64 = AtomicU64::new(0);
static MODE: Lazy<Mutex<Mode>> = Lazy::new(|| Mutex::new(Mode::Idle));
static WATCH_UNTIL_MS: AtomicI64 = AtomicI64::new(0);
static TX: OnceLock<mpsc::Sender<ScanReason>> = OnceLock::new();
static IN_FLIGHT: AtomicBool = AtomicBool::new(false);

const EVENT_DEBOUNCE: Duration = Duration::from_millis(500);

pub fn version() -> u64 {
    VERSION.load(Ordering::SeqCst)
}

pub fn snapshot() -> Option<Arc<Snapshot>> {
    LAST.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

pub fn mode() -> Mode {
    *MODE.lock().unwrap_or_else(|e| e.into_inner())
}

/// 无快照时同步扫一次（冷启动/单测/扫描线程未启动）
pub fn current_or_scan() -> SessionsResponse {
    snapshot()
        .map(|s| s.data.clone())
        .unwrap_or_else(|| scan_once().data)
}

/// 动作路径：快照龄超过 max_age → 请求补扫并等待（上限 wait），超时返回现有快照
pub fn ensure_fresh(max_age: Duration, wait: Duration) -> Arc<Snapshot> {
    if let Some(s) = snapshot() {
        if s.scanned_at.elapsed() <= max_age {
            return s;
        }
    }
    match TX.get() {
        None => return scan_once(), // 扫描线程未启动 → 同步补扫
        Some(tx) => {
            let _ = tx.send(ScanReason::Action);
        }
    }
    let deadline = Instant::now() + wait;
    while Instant::now() < deadline {
        if let Some(s) = snapshot() {
            if s.scanned_at.elapsed() <= max_age {
                return s;
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    snapshot().unwrap_or_else(|| scan_once())
}

/// 事件唤醒（合并节流：500ms 内只接受一次）
pub fn request_scan(reason: ScanReason) {
    if let Some(tx) = TX.get() {
        let _ = tx.send(reason);
    }
}

/// 前端可见性心跳："有人在看" = 主窗可见且聚焦 / 浮窗展开；TTL 90s
pub fn set_user_watching(watching: bool) {
    if watching {
        WATCH_UNTIL_MS.store(now_ms() + 90_000, Ordering::SeqCst);
    } else {
        WATCH_UNTIL_MS.store(0, Ordering::SeqCst);
    }
}

fn user_watching() -> bool {
    now_ms() < WATCH_UNTIL_MS.load(Ordering::SeqCst)
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// 单飞扫描：并发调用者等待同一次扫描（返回同一快照）
fn scan_once() -> Arc<Snapshot> {
    // 简单互斥：IN_FLIGHT 由扫描线程持有；其他调用者自旋等待结果
    if IN_FLIGHT.swap(true, Ordering::SeqCst) {
        while IN_FLIGHT.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(10));
        }
        return snapshot().expect("scan finished without snapshot");
    }
    struct Clear;
    impl Drop for Clear {
        fn drop(&mut self) {
            IN_FLIGHT.store(false, Ordering::SeqCst);
        }
    }
    let _clear = Clear;
    let data = super::get_all_sessions_inner();
    let snap = Arc::new(Snapshot {
        version: VERSION.fetch_add(1, Ordering::SeqCst) + 1,
        scanned_at: Instant::now(),
        data,
    });
    *LAST.lock().unwrap_or_else(|e| e.into_inner()) = Some(snap.clone());
    snap
}

/// 生产启动：常驻线程（首个扫描立即执行，之后按档位）
pub fn start(app: tauri::AppHandle) {
    let (tx, rx) = mpsc::channel::<ScanReason>();
    if TX.set(tx).is_err() {
        return; // 幂等
    }
    std::thread::spawn(move || {
        let mut cadence = Cadence::new(now_ms());
        let mut last_scan = Instant::now() - Duration::from_secs(60);
        let mut last_tray = Instant::now() - Duration::from_secs(60);
        let mut last_waiting = usize::MAX;
        let mut last_processing = false;
        let mut first = true;
        loop {
            if first {
                first = false;
            } else {
                let _ = rx.recv_timeout(cadence.mode().interval());
            }
            if last_scan.elapsed() < EVENT_DEBOUNCE {
                continue; // 合并节流
            }
            let snap = scan_once();
            last_scan = Instant::now();
            update_tray(&app, &snap, &mut last_tray, &mut last_waiting, &mut last_processing);
            let signals = Signals {
                active_sessions: has_active_sessions(&snap.data),
                remote_online: crate::remote::is_online(),
                user_watching: user_watching(),
            };
            if let Some(mode) = cadence.observe(signals, now_ms()) {
                *MODE.lock().unwrap_or_else(|e| e.into_inner()) = mode;
                log::info!("scan_bus: mode → {:?}", mode);
                crate::remote::events::emit_ui("scan-bus-mode", serde_json::json!({ "mode": mode }));
            }
        }
    });
}

fn update_tray(
    app: &tauri::AppHandle,
    snap: &Snapshot,
    last_tray: &mut Instant,
    last_waiting: &mut usize,
    last_processing: &mut bool,
) {
    let has_processing = snap.data.sessions.iter().any(|s| {
        matches!(
            s.status,
            crate::session::SessionStatus::Processing
                | crate::session::SessionStatus::Thinking
                | crate::session::SessionStatus::Compacting
        )
    });
    let changed = snap.data.waiting_count != *last_waiting || has_processing != *last_processing;
    if !changed && last_tray.elapsed() < Duration::from_secs(30) {
        return;
    }
    *last_tray = Instant::now();
    *last_waiting = snap.data.waiting_count;
    *last_processing = has_processing;
    crate::plugins::system_tray::update_tray_status(
        app,
        snap.data.waiting_count,
        snap.data.total_count,
        has_processing,
    );
}

pub fn has_active_sessions(resp: &SessionsResponse) -> bool {
    resp.sessions.iter().any(|s| {
        matches!(
            s.status,
            crate::session::SessionStatus::Processing
                | crate::session::SessionStatus::Thinking
                | crate::session::SessionStatus::Compacting
                | crate::session::SessionStatus::Waiting
        )
    })
}

#[cfg(test)]
pub fn reset_for_test() {
    *LAST.lock().unwrap() = None;
    VERSION.store(0, Ordering::SeqCst);
    *MODE.lock().unwrap() = Mode::Idle;
    WATCH_UNTIL_MS.store(0, Ordering::SeqCst);
    IN_FLIGHT.store(false, Ordering::SeqCst);
    // 注意：TX 是 OnceLock，测试里不要 start()
}

#[cfg(test)]
pub fn publish_for_test(f: impl FnOnce() -> SessionsResponse) -> Arc<Snapshot> {
    let data = f();
    let snap = Arc::new(Snapshot {
        version: VERSION.fetch_add(1, Ordering::SeqCst) + 1,
        scanned_at: Instant::now(),
        data,
    });
    *LAST.lock().unwrap() = Some(snap.clone());
    snap
}
```

在 `adapter/mod.rs`：
- 顶部加 `pub mod scan_bus;`
- `fn get_all_sessions_inner()` 改为 `pub(crate) fn get_all_sessions_inner()`

- [ ] **Step 4: 运行测试**

Run: `cd src-tauri && cargo test adapter::scan_bus -q`
Expected: `4 passed`（若 `ensure_fresh_falls_back_when_no_scanner_thread` 依赖 TX 未设置，保持单测不启动 `start`）

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/adapter/scan_bus.rs src-tauri/src/adapter/mod.rs
git commit -m "feat(scan): add scan bus with shared snapshot and ensure_fresh"
```

---

## Task 3: 命令层接入（读快照 / status / watching / 托盘迁移）

**Files:**
- Modify: `src-tauri/src/commands/session.rs`
- Modify: `src-tauri/src/lib.rs`

- [ ] **Step 1: 写失败测试（命令层源码锁）**

在 `src-tauri/src/commands/session.rs` 的 `#[cfg(test)]` 模块加：

```rust
#[test]
fn sessions_command_reads_snapshot_instead_of_scanning() {
    let src = include_str!("session.rs");
    assert!(
        !src.contains("adapter::get_all_sessions"),
        "命令层不得再直调扫描；只允许 scan_bus::current_or_scan"
    );
    assert!(src.contains("scan_bus::current_or_scan"));
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cd src-tauri && cargo test sessions_command_reads_snapshot -q`
Expected: FAIL（当前命令写的是 `adapter::get_all_sessions`）

- [ ] **Step 3: 改命令与注册**

`commands/session.rs`：

```rust
#[tauri::command]
pub async fn get_all_sessions(_app: tauri::AppHandle) -> SessionsResponse {
    // 读共享快照；冷启动无快照时才同步扫一次（旧语义兜底）
    tauri::async_runtime::spawn_blocking(crate::adapter::scan_bus::current_or_scan)
        .await
        .unwrap_or_else(|e| {
            log::error!("会话快照读取任务异常: {e}");
            SessionsResponse { sessions: Vec::new(), total_count: 0, waiting_count: 0 }
        })
}
```

删除原函数里的托盘更新块（`update_tray_status` 与 `has_processing` 计算）——托盘已迁到 `scan_bus::update_tray`。保留 `last_preset_count` 的预设刷新逻辑（它与扫描无关，可原样保留在命令内）。

**F5 即时重算**：`dismiss_session_card` 与 `focus_session`（其内部 `mark_read_on_jump`）在 DB 变更成功后追加一次补扫请求，避免"已读/隐藏要等下一个档位周期才生效"：

```rust
crate::adapter::scan_bus::request_scan(crate::adapter::scan_bus::ScanReason::Action);
```

同时在这两个命令返回前，前端已有的 `invalidateQueries(["sessions"])` 会让下一帧读到新快照（补扫在毫秒级完成）。

新增两个命令：

```rust
#[tauri::command]
pub fn scan_bus_status() -> serde_json::Value {
    serde_json::json!({
        "mode": crate::adapter::scan_bus::mode(),
        "version": crate::adapter::scan_bus::version(),
    })
}

#[tauri::command]
pub fn scan_bus_watch(watching: bool) {
    crate::adapter::scan_bus::set_user_watching(watching);
}
```

`lib.rs` 的 `invoke_handler` 列表中 `commands::session::get_all_sessions` 之后追加：

```rust
commands::session::scan_bus_status,
commands::session::scan_bus_watch,
```

`lib.rs` setup 中 `spawn_initial_collection()` 之后追加：

```rust
// 会话扫描总线：常驻线程 + 共享快照（所有消费方读快照）
crate::adapter::scan_bus::start(app.handle().clone());
```

- [ ] **Step 4: 运行测试与编译**

Run: `cd src-tauri && cargo test commands::session -q && cargo check -q`
Expected: 测试 PASS、编译无错误

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/commands/session.rs src-tauri/src/lib.rs
git commit -m "feat(scan): read sessions from shared snapshot; add status/watch commands"
```

---

## Task 4: 动作路径 ensure_fresh

**Files:**
- Modify: `src-tauri/src/commands/settings.rs:157`
- Modify: `src-tauri/src/commands/session.rs`（`session_open` 的会话查找，约 388 行）
- Modify: `src-tauri/src/remote/server.rs`（`RemoteState` 增加 `fresh_session_source`）
- Modify: `src-tauri/src/remote/mod.rs`（生产装配）
- Modify: `src-tauri/src/remote/api.rs`（动作端点）
- Modify: `src-tauri/src/inject/queue.rs:162`

- [ ] **Step 1: 写失败测试（动作路径源码锁）**

在 `commands/settings.rs` 测试模块：

```rust
#[test]
fn session_attribution_uses_fresh_snapshot() {
    let src = include_str!("settings.rs");
    assert!(src.contains("scan_bus::ensure_fresh"));
    assert!(!src.contains("adapter::get_all_sessions"));
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cd src-tauri && cargo test session_attribution_uses_fresh_snapshot -q`
Expected: FAIL（当前写的是 `crate::adapter::get_all_sessions()`）

- [ ] **Step 3: 替换调用点**

`commands/settings.rs` 第 157 行附近：

```rust
let response = crate::adapter::scan_bus::ensure_fresh(
    std::time::Duration::from_secs(2),
    std::time::Duration::from_secs(3),
).data.clone();
```

`commands/session.rs` 的 `session_open`（约 388 行）：

```rust
let mut session = crate::adapter::scan_bus::ensure_fresh(
    std::time::Duration::from_secs(2),
    std::time::Duration::from_secs(3),
).data.sessions.iter().find(...).cloned()...;
```

`remote/server.rs` 的 `RemoteState` 增加字段（与 `session_source` 并列）：

```rust
/// 动作路径专用：保证快照新鲜（显示路径仍用 session_source 读旧快照）
pub fresh_session_source: Box<dyn Fn() -> crate::session::SessionsResponse + Send + Sync>,
```

`remote/mod.rs` 生产装配：

```rust
fresh_session_source: Box::new(|| {
    crate::adapter::scan_bus::ensure_fresh(
        std::time::Duration::from_secs(2),
        std::time::Duration::from_secs(3),
    ).data.clone()
}),
```

`remote/api.rs`：用 `grep -n "(st.session_source)()\|(st2.session_source)()\|(probe_st.session_source)()" src-tauri/src/remote/api.rs` 列出全部调用点，按下表替换（行号会漂移，按所在函数判断）：

| 调用点（当前行） | 所在语义 | 改用 |
| --- | --- | --- |
| 61 | GET /sessions 显示 | 保持 `session_source` |
| 183 | SSE 首帧 | 保持 `session_source`（Task 6 再加 ensure） |
| 666 | 新建会话 preview cwd | `fresh_session_source` |
| 1135 | no_session 404 判定 | `fresh_session_source` |
| 2600 | 问答 GET | `fresh_session_source` |
| 7678 | session-send | `fresh_session_source` |
| 10010 | 队列/权限校验 | `fresh_session_source` |
| 3280/3870/4047/4193/4280/7517/8220/8742/8853/10605 | 测试构造的 probe/st2 | 同步给测试构造器补字段（见下） |

测试构造 `RemoteState` 的地方（`remote/server.rs`、`remote/mod.rs`、`api.rs` 的 `probe_st`/`st2`）统一补：

```rust
fresh_session_source: Box::new(|| crate::session::SessionsResponse {
    sessions: vec![], total_count: 0, waiting_count: 0,
}),
```

`inject/queue.rs:162`：

```rust
let Some(session) = (st.fresh_session_source)()
```

- [ ] **Step 4: 编译与回归**

Run: `cd src-tauri && cargo check -q && cargo test -q`
Expected: 编译通过，既有测试全绿（若 `RemoteState` 构造点漏字段会编译失败，按报错补齐）

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/commands/settings.rs src-tauri/src/commands/session.rs \
        src-tauri/src/remote/server.rs src-tauri/src/remote/mod.rs src-tauri/src/remote/api.rs \
        src-tauri/src/inject/queue.rs
git commit -m "feat(scan): ensure fresh snapshot on action paths"
```

---

## Task 5: Watcher 改为订阅快照

**Files:**
- Modify: `src-tauri/src/remote/watcher.rs`

- [ ] **Step 1: 写失败测试（watcher 不再自己扫描）**

在 `watcher.rs` 测试模块加：

```rust
#[test]
fn watcher_source_does_not_call_get_all_sessions() {
    let src = include_str!("watcher.rs");
    // 允许出现注释；禁止可执行调用
    assert!(!src.contains("crate::adapter::get_all_sessions,"));
    assert!(!src.contains("crate::adapter::get_all_sessions()"));
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cd src-tauri && cargo test watcher_source_does_not_call -q`
Expected: FAIL

- [ ] **Step 3: 改循环**

把 `watch_loop` 与 `scan_tick` 替换为：

```rust
const VERSION_POLL: Duration = Duration::from_millis(500);

async fn watch_loop(tx: broadcast::Sender<TransitionEvent>) {
    let mut prev: Vec<crate::session::Session> = Vec::new();
    let mut last_version = 0u64;
    loop {
        tokio::time::sleep(VERSION_POLL).await;
        // 零订阅者：不 diff、不保留基线（恢复订阅后从当前快照重建）
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
        let Some(snap) = crate::adapter::scan_bus::snapshot() else { continue };
        for e in diff_transitions(&prev, &snap.data.sessions, now_ms()) {
            let _ = tx.send(e);
        }
        prev = snap.data.sessions.clone();
    }
}
```

删除 `scan_tick` 与 `SCAN_INTERVAL`（若无其他引用）；保留 `diff_transitions` 及其测试。引用 `scan_tick` 的既有测试一并删除或改写为直接构造 `prev/curr` 调 `diff_transitions`（不保留"注入扫描函数"的旧测法——扫描已不属于 watcher）。

- [ ] **Step 4: 运行测试**

Run: `cd src-tauri && cargo test remote::watcher -q`
Expected: 全部 PASS（含新源码锁与既有 diff 用例）

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/remote/watcher.rs
git commit -m "refactor(remote): watcher consumes scan snapshots instead of scanning"
```

---

## Task 6: SSE 首帧新鲜度 + 配对计数限频

**Files:**
- Modify: `src-tauri/src/remote/api.rs`（`events` handler）
- Modify: `src-tauri/src/remote/mod.rs`（`pairing_counter`）

- [ ] **Step 1: 写失败测试**

在 `remote/api.rs` 测试模块加：

```rust
#[tokio::test]
async fn sse_snapshot_frame_carries_version() {
    // 直接构造快照帧函数（下方实现）并断言 version 字段存在
    let v = super::snapshot_frame_for_test(
        crate::session::SessionsResponse { sessions: vec![], total_count: 0, waiting_count: 0 },
        42,
    );
    assert_eq!(v.get("version").and_then(|x| x.as_u64()), Some(42));
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cd src-tauri && cargo test sse_snapshot_frame_carries_version -q`
Expected: FAIL

- [ ] **Step 3: 实现**

在 `api.rs` 增加：

```rust
pub(crate) fn snapshot_frame_for_test(
    resp: crate::session::SessionsResponse,
    version: u64,
) -> serde_json::Value {
    let mut v = serde_json::to_value(resp).unwrap_or_else(|_| empty_sessions_value());
    if let Some(obj) = v.as_object_mut() {
        obj.insert("version".into(), serde_json::json!(version));
    }
    v
}
```

`events` handler 中把首帧快照改为：

```rust
let snap = tokio::task::spawn_blocking(|| {
    crate::adapter::scan_bus::ensure_fresh(
        std::time::Duration::from_secs(2),
        std::time::Duration::from_secs(3),
    )
})
.await
.map(|s| (s.data.clone(), s.version))
.unwrap_or_else(|e| {
    log::error!("SSE 快照等待异常: {e}");
    (crate::session::SessionsResponse { sessions: vec![], total_count: 0, waiting_count: 0 }, 0)
});
let value = apply_board_hidden(&st, snap.0);
// 后续帧构造改用 snapshot_frame_for_test(value, snap.1)
```

`remote/mod.rs` 的 `pairing_counter` 改为 10s 缓存：

```rust
pairing_counter: Box::new(|| {
    static CACHE: once_cell::sync::Lazy<std::sync::Mutex<Option<(std::time::Instant, Vec<(String, String)>)>>> =
        once_cell::sync::Lazy::new(|| std::sync::Mutex::new(None));
    #[cfg(windows)]
    {
        let mut g = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((at, v)) = g.as_ref() {
            if at.elapsed() < std::time::Duration::from_secs(10) {
                return v.clone();
            }
        }
        let system = sysinfo::System::new_all();
        let v = crate::commands::session::running_projects_from_processes(&system);
        *g = Some((std::time::Instant::now(), v.clone()));
        v
    }
    #[cfg(not(windows))]
    {
        Vec::new()
    }
}),
```

- [ ] **Step 4: 运行测试**

Run: `cd src-tauri && cargo test remote:: -q`
Expected: PASS（含新用例与既有 remote 测试）

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/remote/api.rs src-tauri/src/remote/mod.rs
git commit -m "feat(remote): fresh SSE snapshot frame and throttled pairing counter"
```

---

## Task 7: 事件唤醒

**Files:**
- Create: `src-tauri/src/adapter/wakeup.rs`
- Modify: `src-tauri/src/adapter/mod.rs`、`src-tauri/src/lib.rs`

- [ ] **Step 1: 写失败测试（路径变化检测）**

`wakeup.rs` 末尾：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_new_event_file() {
        let a = vec![(PathBuf::from("a.json"), Some(100u64))];
        let b = vec![(PathBuf::from("a.json"), Some(100u64)), (PathBuf::from("b.json"), Some(101u64))];
        assert!(changed(&a, &b));
    }

    #[test]
    fn detects_in_place_rewrite() {
        let a = vec![(PathBuf::from("a.json"), Some(100u64))];
        let b = vec![(PathBuf::from("a.json"), Some(200u64))];
        assert!(changed(&a, &b), "原地写只改文件 mtime，必须命中");
    }

    #[test]
    fn unchanged_returns_false() {
        let a = vec![(PathBuf::from("a.json"), Some(100u64))];
        assert!(!changed(&a, &a));
    }
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cd src-tauri && cargo test adapter::wakeup -q`
Expected: FAIL（模块未定义）

- [ ] **Step 3: 实现**

```rust
//! 事件唤醒：1s 级 mtime 轮询（目录 + 事件文件 + facts），变化即请求补扫。
//! 只做"提醒"，不解析内容；不新增依赖。

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::scan_bus::{self, ScanReason};

fn stamp(p: &Path) -> Option<u64> {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as u64)
}

pub(crate) fn signature(events_dir: &Path, session_ids: &[String]) -> Vec<(PathBuf, Option<u64>)> {
    let mut v: Vec<(PathBuf, Option<u64>)> = vec![(events_dir.to_path_buf(), stamp(events_dir))];
    for sid in session_ids.iter().take(8) {
        let p = events_dir.join(format!("{sid}.json"));
        v.push((p.clone(), stamp(&p)));
    }
    v.push((dirs_home("tps/opencode/facts.jsonl"), stamp(&dirs_home("tps/opencode/facts.jsonl"))));
    v
}

fn dirs_home(rel: &str) -> PathBuf {
    let mut p = dirs::home_dir().unwrap_or_default();
    for part in rel.split('/') {
        p.push(part);
    }
    p
}

pub(crate) fn changed(a: &[(PathBuf, Option<u64>)], b: &[(PathBuf, Option<u64>)]) -> bool {
    a != b
}

/// 清理超过 5 分钟的 hook 事件文件（读侧本就按 30s TTL 过滤，这里只防目录膨胀）
fn cleanup_old_events(dir: &Path) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let now = SystemTime::now();
    for e in rd.flatten() {
        let p = e.path();
        let old = std::fs::metadata(&p)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| now.duration_since(t).ok())
            .map(|d| d > Duration::from_secs(300))
            .unwrap_or(false);
        if old {
            let _ = std::fs::remove_file(&p);
        }
    }
}

pub fn spawn_runtime() {
    std::thread::spawn(|| {
        let events_dir = dirs_home(".tuvis/events");
        let mut last = signature(&events_dir, &[]);
        let mut ticks: u64 = 0;
        loop {
            std::thread::sleep(Duration::from_secs(1));
            let ids: Vec<String> = scan_bus::snapshot()
                .map(|s| s.data.sessions.iter().map(|x| x.id.clone()).collect())
                .unwrap_or_default();
            let cur = signature(&events_dir, &ids);
            if changed(&last, &cur) {
                scan_bus::request_scan(ScanReason::Event);
            }
            last = cur;
            ticks += 1;
            if ticks % 60 == 0 {
                cleanup_old_events(&events_dir);
            }
        }
    });
}
```

`adapter/mod.rs` 加 `pub mod wakeup;`；`lib.rs` setup 中 `scan_bus::start(...)` 之后：

```rust
crate::adapter::wakeup::spawn_runtime();
```

- [ ] **Step 4: 运行测试**

Run: `cd src-tauri && cargo test adapter::wakeup -q`
Expected: `3 passed`

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/adapter/wakeup.rs src-tauri/src/adapter/mod.rs src-tauri/src/lib.rs
git commit -m "feat(scan): wake scanning on hook event and facts file changes"
```

---

## Task 8: 用量后端兜底 ticker

**Files:**
- Create: `src-tauri/src/services/usage/ticker.rs`
- Modify: `src-tauri/src/services/usage/mod.rs`、`src-tauri/src/lib.rs`

- [ ] **Step 1: 写失败测试（步长 > 最小间隔；debug 默认关闭）**

`ticker.rs`：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_is_min_interval_plus_buffer() {
        assert_eq!(step_ms(10), 10 * 60_000 + 120_000);
        assert_eq!(step_ms(1), 1 * 60_000 + 120_000);
    }

    #[test]
    fn enabled_by_default_only_in_release() {
        assert_eq!(default_enabled(cfg!(debug_assertions), None), !cfg!(debug_assertions));
        assert!(default_enabled(true, Some("1".into())));
    }
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cd src-tauri && cargo test services::usage::ticker -q`
Expected: FAIL

- [ ] **Step 3: 实现**

```rust
//! 用量采集兜底 ticker：可见触发之外的低频保障，不依赖任何页面。
//! 步长必须大于 `collectIntervalMin`，否则最小间隔会让隔轮空跑。

use std::time::Duration;

pub(crate) fn step_ms(interval_min: u64) -> u64 {
    interval_min.max(1) * 60_000 + 120_000
}

pub(crate) fn default_enabled(debug: bool, env: Option<String>) -> bool {
    if env.as_deref() == Some("1") {
        return true;
    }
    !debug
}

pub fn spawn_runtime() {
    let env = std::env::var("TUVIS_USAGE_TICKER").ok();
    if !default_enabled(cfg!(debug_assertions), env) {
        log::info!("usage: 兜底 ticker 在 debug 构建默认关闭（TUVIS_USAGE_TICKER=1 可开）");
        return;
    }
    std::thread::spawn(|| loop {
        let interval = crate::services::usage::settings::load().collect_interval_min.max(1) as u64;
        std::thread::sleep(Duration::from_millis(step_ms(interval)));
        let r = crate::services::usage::collect::collect(false);
        log::info!(
            "usage: 兜底采集完成（{} 源 / {} 条新记录 / {}ms）",
            r.sources.len(),
            r.total_new_records,
            r.duration_ms
        );
        if r.sources.iter().any(|s| s.new_records > 0) {
            crate::remote::events::emit_ui("usage-collected", serde_json::json!({}));
        }
    });
}
```

> 字段核对：`UsageCollectResult` 的逐源字段名以 `services/usage/model.rs` 为准；若字段不叫 `new_records`，按实际字段名改（编译会报错）。

`usage/mod.rs` 加 `pub mod ticker;`；`lib.rs` setup 中 `spawn_initial_collection()` 之后：

```rust
crate::services::usage::ticker::spawn_runtime();
```

- [ ] **Step 4: 运行测试**

Run: `cd src-tauri && cargo test services::usage::ticker -q`
Expected: `2 passed`

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/services/usage/ticker.rs src-tauri/src/services/usage/mod.rs src-tauri/src/lib.rs
git commit -m "feat(usage): add backend fallback ticker for collection"
```

---

## Task 9: 前端档位跟随 + 可见性心跳

**Files:**
- Create: `src/lib/query/queries/scanBus.ts`
- Modify: `src/lib/query/queries/sessions.ts`
- Modify: `src/App.tsx`（主窗根组件，如无则 `src/main.tsx` 的根）
- Modify: `src/components/pet/UsageMiniBar.tsx`
- Test: `tests/scanBus/scanMode.test.tsx`

- [ ] **Step 1: 写失败测试**

```tsx
// tests/scanBus/scanMode.test.tsx
import { renderHook, waitFor } from "@testing-library/react";
import { describe, expect, it, vi, beforeEach } from "vitest";

const invoke = vi.fn();
const listeners: Array<(e: { payload: { mode: string } }) => void> = [];
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...a: unknown[]) => invoke(...a) }));
vi.mock("@tauri-apps/api/event", () => ({
  listen: async (_n: string, cb: (e: { payload: { mode: string } }) => void) => {
    listeners.push(cb);
    return () => {};
  },
}));

import { useScanMode, MODE_INTERVAL_MS, useScanBusHeartbeat } from "@/lib/query/queries/scanBus";

describe("useScanMode", () => {
  beforeEach(() => {
    invoke.mockReset();
    listeners.length = 0;
    invoke.mockResolvedValue({ mode: "idle", version: 1 });
  });

  it("follows backend mode events", async () => {
    const { result } = renderHook(() => useScanMode());
    await waitFor(() => expect(result.current).toBe(MODE_INTERVAL_MS.idle));
    listeners[0]({ payload: { mode: "active" } });
    await waitFor(() => expect(result.current).toBe(MODE_INTERVAL_MS.active));
  });

  it("heartbeat reports visibility", async () => {
    invoke.mockResolvedValue(undefined);
    renderHook(() => useScanBusHeartbeat());
    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("scan_bus_watch", expect.objectContaining({ watching: expect.any(Boolean) }))
    );
  });
});
```

- [ ] **Step 2: 运行确认失败**

Run: `pnpm vitest run tests/scanBus/scanMode.test.tsx`
Expected: FAIL（模块不存在）

- [ ] **Step 3: 实现**

```ts
// src/lib/query/queries/scanBus.ts
import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

export type ScanMode = "active" | "idle" | "away";

export const MODE_INTERVAL_MS: Record<ScanMode, number> = {
  active: 2_000,
  idle: 10_000,
  away: 30_000,
};

export function useScanMode(): number {
  const [mode, setMode] = useState<ScanMode>("idle");
  useEffect(() => {
    let alive = true;
    void invoke<{ mode: ScanMode }>("scan_bus_status")
      .then((s) => alive && setMode(s.mode))
      .catch(() => {});
    const un = listen<{ mode: ScanMode }>("scan-bus-mode", (e) => setMode(e.payload.mode));
    return () => {
      alive = false;
      void un.then((f) => f());
    };
  }, []);
  return MODE_INTERVAL_MS[mode];
}

export function useScanBusHeartbeat(): void {
  useEffect(() => {
    const send = () => {
      const focused = document.hasFocus ? document.hasFocus() : true;
      const watching = document.visibilityState === "visible" && focused;
      void invoke("scan_bus_watch", { watching }).catch(() => {});
    };
    send();
    const id = window.setInterval(send, 30_000);
    const on = () => send();
    document.addEventListener("visibilitychange", on);
    window.addEventListener("focus", on);
    window.addEventListener("blur", on);
    return () => {
      window.clearInterval(id);
      document.removeEventListener("visibilitychange", on);
      window.removeEventListener("focus", on);
      window.removeEventListener("blur", on);
      void invoke("scan_bus_watch", { watching: false }).catch(() => {});
    };
  }, []);
}
```

`sessions.ts`：

```ts
import { useScanMode } from "./scanBus";

export function useSessionsQuery() {
  const interval = useScanMode();
  return useQuery({
    queryKey: ["sessions"],
    queryFn: getAllSessions,
    refetchInterval: interval,
    refetchIntervalInBackground: false,
    staleTime: 1000,
  });
}
```

主窗根组件挂载心跳（如 `src/App.tsx` 的根组件内）：

```tsx
useScanBusHeartbeat();
```

`UsageMiniBar` 数据容器内（浮窗展开期间视为"有人在看"）：

```tsx
useEffect(() => {
  void invoke("scan_bus_watch", { watching: true }).catch(() => {});
  return () => {
    void invoke("scan_bus_watch", { watching: false }).catch(() => {});
  };
}, []);
```

- [ ] **Step 4: 运行测试**

Run: `pnpm vitest run tests/scanBus/scanMode.test.tsx` 和 `pnpm test`
Expected: 新用例 PASS，既有前端测试全绿

- [ ] **Step 5: 提交**

```bash
git add src/lib/query/queries/scanBus.ts src/lib/query/queries/sessions.ts src/App.tsx \
        src/components/pet/UsageMiniBar.tsx tests/scanBus/scanMode.test.tsx
git commit -m "feat(ui): follow scan cadence and report visibility heartbeat"
```

---

## Task 10: 用量可见即采（前端 active 边沿）

**Files:**
- Modify: `src/lib/query/queries/usage.ts`
- Test: `tests/usage/collectTrigger.test.tsx`

- [ ] **Step 1: 写失败测试**

```tsx
// tests/usage/collectTrigger.test.tsx
import { renderHook } from "@testing-library/react";
import { describe, expect, it, vi, beforeEach } from "vitest";

const usageCollect = vi.fn().mockResolvedValue({});
vi.mock("@/lib/api/usage", () => ({
  usageCollect,
  usageDashboard: vi.fn(),
  usageGetSettings: vi.fn().mockResolvedValue({ enabled: true, collectIntervalMin: 10 }),
  usageRecords: vi.fn(),
}));
vi.mock("@tanstack/react-query", () => ({
  useQueryClient: () => ({ invalidateQueries: vi.fn() }),
  useQuery: () => ({ data: { enabled: true, collectIntervalMin: 10 } }),
}));

import { useUsageCollect } from "@/lib/query/queries/usage";

describe("useUsageCollect", () => {
  beforeEach(() => usageCollect.mockClear());

  it("forces a collect when the page becomes active", async () => {
    const { rerender } = renderHook(({ active }) => useUsageCollect(active), {
      initialProps: { active: false },
    });
    rerender({ active: true });
    expect(usageCollect).toHaveBeenCalledWith(true);
  });
});
```

- [ ] **Step 2: 运行确认失败**

Run: `pnpm vitest run tests/usage/collectTrigger.test.tsx`
Expected: FAIL（当前只调用 `usageCollect(false)`）

- [ ] **Step 3: 实现**

`usage.ts` 中：

1. `collect` 改为 `collect(force = false) => usageCollect(force)`；
2. 新增边沿 effect：

```ts
const prevActive = useRef(false);
useEffect(() => {
  const becameActive = active && !prevActive.current;
  prevActive.current = active;
  if (becameActive) void collect(true);
}, [active, collect]);
```

3. 删除原"兜底 ticker" effect（`setInterval` 那段），只在注释里说明"兜底已移到后端 ticker"；
4. 订阅后端采集成功事件：

```ts
useEffect(() => {
  const un = listen("usage-collected", () => {
    void queryClient.invalidateQueries({ queryKey: USAGE_KEY });
  });
  return () => { void un.then((f) => f()); };
}, [queryClient]);
```

> 若测试环境的 `@tauri-apps/api/event` 未 mock，按 `tests/` 现有惯例加 mock。

- [ ] **Step 4: 运行测试**

Run: `pnpm vitest run tests/usage/collectTrigger.test.tsx && pnpm test`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
git add src/lib/query/queries/usage.ts tests/usage/collectTrigger.test.tsx
git commit -m "feat(usage): collect on page activation and remove UI ticker"
```

---

## Task 11: 进程扫描两段式（第二刀）

**Files:**
- Modify: `src-tauri/src/adapter/mod.rs`（`get_all_sessions_inner` Phase 1）
- Test: `src-tauri/src/adapter/mod.rs` 测试模块

- [ ] **Step 1: 写失败测试（候选集包含解释器）**

```rust
#[test]
fn candidate_names_include_interpreters_for_shim_hosts() {
    let names = candidate_process_names(&[&["kimi"][..]]);
    assert!(names.contains(&"node".to_string()));
    assert!(names.contains(&"bun".to_string()));
    assert!(names.contains(&"kimi".to_string()));
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cd src-tauri && cargo test candidate_names_include_interpreters -q`
Expected: FAIL

- [ ] **Step 3: 实现两段式**

在 `adapter/mod.rs` 加：

```rust
/// npm-shim 宿主：工具实际进程名常是解释器，需在第二段读取 cmd 才能判定
pub(crate) fn is_shim_host(name: &str) -> bool {
    let base = name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(name)
        .to_lowercase();
    let base = base.strip_suffix(".exe").unwrap_or(&base);
    matches!(base, "node" | "bun" | "python" | "python3" | "deno")
}

pub(crate) fn candidate_process_names(adapter_names: &[&[&str]]) -> Vec<String> {
    let mut v: Vec<String> = vec![
        "node".into(), "bun".into(), "python".into(), "python3".into(), "deno".into(),
    ];
    for names in adapter_names {
        for n in *names {
            v.push((*n).to_string());
        }
    }
    v.sort();
    v.dedup();
    v
}

fn bare_name(name: &str) -> String {
    let b = name.rsplit(['/', '\\']).next().unwrap_or(name).to_lowercase();
    b.strip_suffix(".exe").unwrap_or(&b).to_string()
}
```

`get_all_sessions_inner` Phase 1 改为：

```rust
let all_processes: Vec<Vec<AgentProcess>> = {
    let mut guard = SHARED_SYSTEM.lock().unwrap();
    let system = guard.get_or_insert_with(System::new);

    // 第一段：只刷新名字层（轻量）
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing(),
    );

    // 候选集：名字命中工具白名单，或解释器（shim 宿主）
    let wanted_names: Vec<String> = {
        let lists: Vec<&[&str]> = adapters.iter().map(|a| a.process_names()).collect();
        candidate_process_names(&lists)
    };
    let candidates: Vec<sysinfo::Pid> = system
        .processes()
        .iter()
        .filter(|(_, p)| {
            let n = bare_name(&p.name().to_string_lossy());
            wanted_names.iter().any(|w| w == &n)
        })
        .map(|(pid, _)| *pid)
        .collect();

    // 第二段：只对候选 PID 读昂贵字段
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&candidates),
        false,
        ProcessRefreshKind::nothing()
            .with_cmd(sysinfo::UpdateKind::OnlyIfNotSet)
            .with_cwd(sysinfo::UpdateKind::OnlyIfNotSet)
            .with_exe(sysinfo::UpdateKind::OnlyIfNotSet)
            .with_cpu(),
    );

    adapters.iter().map(|a| a.find_processes(system)).collect()
};
```

- [ ] **Step 4: 运行测试与实测**

Run: `cd src-tauri && cargo test adapter:: -q`
Expected: PASS。然后在真机跑 `scripts/measure-scan-bus.ps1 -Seconds 60` 观察单轮耗时（日志 `Found process` 调试输出或一次 tick 时间）。

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/adapter/mod.rs
git commit -m "perf(scan): two-stage process refresh with shim host whitelist"
```

---

## Task 12: 日志降噪与验收复测

**Files:**
- Modify: `src-tauri/src/adapter/mod.rs`（每轮 INFO）
- Modify: `docs/engineering/evidence/2026-10-29-scan-bus-baseline.md`（追加复测）

- [ ] **Step 1: 改日志级别**

`get_all_sessions_inner` 中每轮打印的：

```rust
log::info!("{}: {} processes, {} sessions", adapter.name(), processes.len(), sessions.len());
```

改为 `log::debug!`。档位切换日志（scan_bus 内 `mode →`）保持 INFO。

- [ ] **Step 2: 编译与测试**

Run: `cd src-tauri && cargo test -q && cargo clippy -q`
Expected: 全绿

- [ ] **Step 3: 验收复测（5 分钟）**

Run: `powershell -NoProfile -File scripts/measure-scan-bus.ps1 -Seconds 300`
Expected（门禁）：挂机 `avgCPU ≤3%`、`scansPerMin ≤6`；远控在线空闲 `≤6% / ≤10`。
把结果追加到基线证据文件，形成前后对照表。

- [ ] **Step 4: 提交**

```bash
git add src-tauri/src/adapter/mod.rs docs/engineering/evidence/2026-10-29-scan-bus-baseline.md
git commit -m "chore(scan): quiet per-round logs and record post-change measurements"
```

---

## Self-Review

**Spec coverage**

| 需求 | 任务 |
| --- | --- |
| F1 单一扫描器与共享快照 | Task 2、3 |
| F2 自适应节奏 | Task 1、2 |
| F3 ensure_fresh | Task 4 |
| F4 消费方改造 | Task 3（托盘迁移）、5（watcher）、6（SSE/配对）、9（前端档位） |
| F5 副作用归属 | Task 3（`dismiss_session_card` / `focus_session` 变更后立即 `request_scan(Action)`，配合前端 invalidate 读新快照；扫描内副作用原样保留） |
| F6 事件唤醒 | Task 7 |
| F7 用量采集时机 | Task 8、10 |
| F8 进程两段式 | Task 11 |
| F9 解析器与日志 | Task 12（日志部分；dsh 代际路径缓存拆到后续计划，因需改动 `dsh.rs` 的 `scan_raw_logs` 内部缓存层，单独评审） |
| F10 验收测量 | Task 0、12 |

**已知取舍**

- `scan_bus::scan_once` 用自旋等待单飞（10ms 粒度），实现最简单；若实测 CPU 有毛刺，换成 `Condvar`（Task 2 已留 `IN_FLIGHT` 语义）。
- Task 4 的 `fresh_session_source` 需要给全部 `RemoteState` 测试构造点补字段，编译失败即列出全部位置，按报错补齐。
- Task 10 删除前端 ticker 后，用量兜底完全依赖后端 ticker（Task 8）；两步必须同批合并，否则会出现"没有任何周期采集"的中间态。

**计划外但必须遵守**

- `AGENTS.md` 采集域纪律：跑 Task 8/10 前停 `pnpm tauri:dev` 或设 `TUVIS_HOME` 影子目录。
- 任何"扫描次数"验收都以日志 `adapter] OpenCode:` 计数为准；Task 12 把它改成 debug 后，需要先把日志级别切回 info 再测，或改用 `scan_bus` 的 `mode`/版本日志计数（在 Task 12 Step 3 中注明先临时设 `RUST_LOG=info`）。
