// axum 组装：/m 静态（rust-embed，Task 7 已装配）+ /m/api/v1/* + gate
//
// 结构契约（评审 Important 1 修复后，勿退化）：
// - API 一律走 `nest("/m/api/v1", api_router)`；gate 是**内层 layer**，覆盖该 nest 下
//   现在与将来注册的所有路由（含内层 fallback）——结构性生效，不依赖 `Router::layer`
//   的「只包裹此前注册的路由」这一顺序陷阱（评审判定的未来绕过：在已 layer 的 Router 上
//   后加 `.fallback()` 会替换掉被 gate 包裹的默认 fallback，未知 API 路径随之裸奔）；
// - 内层 fallback 直接 403：`/m/api/v1/*` 的未知路径不留裸路径，Task 7 追加的顶层
//   静态 fallback 追不进来；
// - 外层 Router 只负责静态侧：Task 7 在其上追加 `.route("/m", ...)` 与顶层
//   fallback（/m/* 静态资产由该 fallback 的路径分流伺服，没有独立的 /m/assets/*
//   路由），均**不应**经过 gate（配对页必须无 cookie 可加载）；
// - 内层 gate 看到的 path 已被 nest 剥掉前缀（`/pair` 而非 `/m/api/v1/pair`），
//   放行名单必须写相对路径，详见 gate.rs。

use axum::{
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use std::sync::Arc;

use super::api;

// ============================================================
// /m 静态伺服（Task 7）：rust-embed 嵌入 dist-mobile 构建产物
// ============================================================

/// rust-embed 嵌入 `../dist-mobile/`（相对 src-tauri/，即仓库根的移动端产物目录）。
/// 构建顺序铁律：release 下产物在**编译期**打进二进制，debug 下 `get()` 每次**从磁盘直读**
/// （crate 默认行为，便于 tauri:dev 迭代移动端产物而免重编 Rust）——
/// 因此任何 `cargo check/test/build` 之前必须先 `pnpm build:mobile`，否则入口 404。
#[derive(rust_embed::RustEmbed)]
#[folder = "../dist-mobile/"]
// dist-mobile/.gitkeep 是入库占位文件（构建时由 publicDir 拷贝自动恢复，非伺服资产）：
// exclude 防止它被 rust-embed 嵌进二进制 / 被静态伺服（include-exclude feature 即为此开启）
#[exclude = ".gitkeep"]
struct MobileAssets;

/// SPA 入口文件。控制者裁决（2026-09-14）：`vite build` 对 `mobile.html` 入口产出的就是
/// `dist-mobile/mobile.html`（`rollupOptions.input` 键名改不了 HTML 输出名，Task 5 实测）——
/// 简报原稿的 `serve_asset("index.html")` 会 404，故 /m 与各处回落统一伺服 mobile.html
const MOBILE_ENTRY: &str = "mobile.html";

/// §C3 可达性探针的 **兔维斯 特征头**（B-I6）：看板入口响应写入，tailscale 探针读取。
/// 为什么需要它：探针原口径「拿到任意 HTTP 响应即算通」挡不住 TUN / 透明重定向式 MITM
/// ——用户把自签 CA 装进系统信任库后 TLS 仍"成功"，拦截页被读成「已验证」（§C3 残余风险）。
/// 写入点唯一（本文件的 `entry_response`），读取点唯一（`tailscale::probe_http`），
/// 值与本常量同处定义，改一处必然牵动另一处（`board_entry_carries_reach_marker_for_tailscale_probe`
/// 在服务侧锁死其在场）。
/// **诚实边界**：这不是鉴权也不是密码学证明——知道特征值的 MITM 仍可伪造；它的定位是
/// 「廉价判据」：把「任意响应」收紧为「带 兔维斯 特征」，挡掉最常见的透明错误页
pub(crate) const MAM_REACH_HEADER: &str = "x-tuvis-reach";
/// 特征头的值（版本化 token：语义变更时同步改，两侧由编译期常量约束）
pub(crate) const MAM_REACH_VALUE: &str = "board-1";

/// 入口 HTML 响应（/m 精确命中与 /m/* 未命中回落共用）。
/// 抽成非 async 纯函数的原因：若 serve_asset 未命中分支直接 `mobile_index().await`，
/// 会构成相互递归的 async fn（编译不过；且 dist-mobile 未构建时无限循环）。
/// 产物缺失（未跑 pnpm build:mobile）→ 404 显式失败，不挂死
fn entry_response() -> Response {
    match MobileAssets::get(MOBILE_ENTRY) {
        Some(f) => (
            [
                (axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8"),
                // §C3（B-I6）：兔维斯 特征头——tailscale 可达性探针据此判定「是 兔维斯 答的」。
                // 只在**真的伺服了看板入口**时写：产物缺失（404 分支）不带特征
                (
                    axum::http::HeaderName::from_static(MAM_REACH_HEADER),
                    MAM_REACH_VALUE,
                ),
            ],
            f.data,
        )
            .into_response(),
        None => (
            axum::http::StatusCode::NOT_FOUND,
            "mobile assets missing (run `pnpm build:mobile` first)",
        )
            .into_response(),
    }
}

/// 按扩展名给 MIME：简报清单（js/css/png/json/html）+ 补充（svg=manifest/图标可能引用；
/// webmanifest=若产物出现 PWA manifest 的 .webmanifest 形态）。
/// 未知扩展回落 `application/octet-stream`（选型：二进制下载语义，而非简报默认的
/// text/html——把任意未知内容误标成 HTML 会放大注入面）
fn mime_for(path: &str) -> &'static str {
    match path.rsplit('.').next() {
        Some("js") => "text/javascript",
        Some("css") => "text/css",
        Some("png") => "image/png",
        Some("json") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("webmanifest") => "application/manifest+json",
        Some("html") => "text/html; charset=utf-8",
        _ => "application/octet-stream",
    }
}

async fn serve_asset(path: &str) -> Response {
    match MobileAssets::get(path) {
        Some(f) => ([(axum::http::header::CONTENT_TYPE, mime_for(path))], f.data).into_response(),
        // SPA 兜底（控制者裁决 1）：/m/<path> 未命中回落入口 HTML——移动端单页 hash 路由，
        // 刷新/直达任意路径都必须能拿到壳页面
        None => entry_response(),
    }
}

/// 顶层静态 fallback 的路径分流（控制者裁决 2，勿退化）：
/// - 非 `/m` 前缀 → 404（根路径与移动看板无关，不给静态兜底）；
/// - `/m/` 尾斜杠与 `/m`（理论上被 route 收口，防御性兜底）→ 入口 HTML；
/// - `/m/api` 裸前缀与 `/m/api/*`（含未知版本前缀如 `/m/api/v2/*`）→ 403：这是
///   「所有 `/m/api/*` 过 gate（403）」安全不变量的**字面收口**（终审 2026-09-14）——
///   这些路径不匹配 nest 的 catch-all（matchit `{*rest}` 要求至少一个非空段，且裸
///   前缀 `/m/api` 连尾斜杠都没有），否则会落到下方 `serve_asset` 的 SPA 回落返回
///   200 入口 HTML，字面违反不变量。`/m/api/v1/` 精确变体另有 router() 上的显式
///   收口条（结构层保证，见其注释）；
/// - `/m/<path>` → `serve_asset(path)`，未命中回落入口 HTML。
///
/// 注意：`/m/api/v1/<已知或未知子路径>` 永远到不了这里——nest 内层 fallback 先 403
/// （结构隔离，见 router()/api_router 注释与 task7_static_routes_* 测试）
async fn static_fallback(uri: axum::http::Uri) -> Response {
    let path = uri.path();
    if path == "/m" || path == "/m/" {
        return entry_response();
    }
    // 「所有 /m/api/* 过 gate」的字面收口：裸前缀 / 尾斜杠 / 未知版本前缀一律 403
    // （须在 serve_asset 的 SPA 回落之前判定，否则 200 静态内容顶替 gate）
    if path == "/m/api" || path.starts_with("/m/api/") {
        return axum::http::StatusCode::FORBIDDEN.into_response();
    }
    match path.strip_prefix("/m/") {
        Some(rest) => serve_asset(rest).await,
        None => (axum::http::StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

/// /m 入口（配对页/看板壳）。产物文件是 mobile.html（见 MOBILE_ENTRY 注释）
async fn mobile_index() -> Response {
    entry_response()
}

/// 活跃连接句柄表（clippy::type_complexity 门禁适配：复杂类型抽别名，语义同原稿内联形）
type DeviceConns = std::collections::HashMap<String, Vec<(u64, tokio::sync::oneshot::Sender<()>)>>;

/// SSE 连接注册表（M4 T0a）：device_id → 活跃连接句柄表。
/// 断连语义：吊销/停止时对目标设备的全部连接发 oneshot 关闭信号，
/// SSE 流的 `take_until` 收到信号即终止 → axum 关闭该 HTTP 连接；
/// 自然断开（客户端关页）由 CleanupStream 的 Drop 反注册。
/// 锁粒度：单 Mutex 短临界区（register/unregister/disconnect 均无 IO），
/// 不与 store/pairing 锁嵌套（锁序红线：registry 永远最后进最先出）。
#[derive(Default)]
pub struct SseRegistry {
    inner: std::sync::Mutex<DeviceConns>,
    next_id: std::sync::atomic::AtomicU64,
}

impl SseRegistry {
    /// 注册一条连接：返回 (连接 id, 关闭信号接收端)。
    /// 返回的 Receiver 在 disconnect_device/disconnect_all 或 Sender 被 drop 时给出信号
    pub fn register(&self, device: &str) -> (u64, tokio::sync::oneshot::Receiver<()>) {
        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.inner
            .lock()
            .unwrap()
            .entry(device.to_string())
            .or_default()
            .push((id, tx));
        (id, rx)
    }

    /// 自然断开反注册（幂等：未知 id 静默忽略）。
    /// 写法适配（clippy::option_map_unit_fn 门禁）：原稿 `.map(|v| …)` 改 `if let`，语义不变
    pub fn unregister(&self, device: &str, id: u64) {
        if let Some(v) = self.inner.lock().unwrap().get_mut(device) {
            v.retain(|(i, _)| *i != id);
        }
    }

    /// 断开指定设备的全部连接，返回断开数
    pub fn disconnect_device(&self, device: &str) -> usize {
        self.inner
            .lock()
            .unwrap()
            .remove(device)
            .map(|v| {
                // 编译适配（oneshot::Sender::send 消费 self，不能按引用迭代 send）：
                // 先取长度，再按值迭代逐个 send
                let n = v.len();
                for (_, tx) in v {
                    let _ = tx.send(());
                }
                n
            })
            .unwrap_or(0)
    }

    /// 断开全部设备连接（停止远程 / 全部吊销），返回断开数
    pub fn disconnect_all(&self) -> usize {
        let mut map = self.inner.lock().unwrap();
        let n: usize = map.values().map(Vec::len).sum();
        // 编译适配（send 消费 Sender，需持所有权迭代）：drain 等价于原稿的
        // 「逐个 send 后 map.clear()」
        for (_, v) in map.drain() {
            for (_, tx) in v {
                let _ = tx.send(());
            }
        }
        n
    }

    /// 该设备是否存在活跃连接（Task 7 花名册在线口径数据源）
    pub fn has(&self, device: &str) -> bool {
        self.inner
            .lock()
            .unwrap()
            .get(device)
            .is_some_and(|v| !v.is_empty())
    }
}

/// 自然断开清理包装：axum drop SSE 流时反注册注册表项（不留陈旧句柄泄漏）。
/// pub(crate) + 字段同可见性（编译适配）：api.rs（兄弟模块）按简报原稿以字段字面量构造
pub(crate) struct CleanupStream<S> {
    pub(crate) inner: S,
    pub(crate) reg: std::sync::Arc<SseRegistry>,
    pub(crate) device: String,
    pub(crate) conn_id: u64,
}
impl<S: futures::Stream> futures::Stream for CleanupStream<S> {
    type Item = S::Item;
    fn poll_next(
        // 编译适配（unused_mut，-D warnings 门禁）：map_unchecked_mut 按值消费 self，
        // 绑定无需 mut
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        // Safety: 无 Unpin 约束需求经字段投影转发（inner 已被 take_until 包装为 Unpin 流链）
        unsafe { self.map_unchecked_mut(|s| &mut s.inner).poll_next(cx) }
    }
}
impl<S> Drop for CleanupStream<S> {
    fn drop(&mut self) {
        self.reg.unregister(&self.device, self.conn_id);
    }
}

/// via 判定域名源接缝类型（clippy type_complexity 收敛别名）：三路 = (quick, named,
/// tailscale)（§C1 追加第三路）
pub type ViaHostsSource = dyn Fn() -> Option<(Vec<String>, Vec<String>, Vec<String>)> + Send + Sync;

/// A1 写入确认探针缝类型（M9R Task 5，clippy type_complexity 收敛别名，对齐
/// [`ViaHostsSource`] 先例）：参数 = (tool, session_id, stamp)。
pub type ConfirmProbeFn = dyn Fn(&str, &str, &str) -> bool + Send + Sync;

/// 归档删除缝类型（spec §6.1，clippy type_complexity 收敛别名，对齐
/// [`ConfirmProbeFn`] 先例）：参数 = None（全删）| Some(session_id)。
pub type ArchiveDeleteFn = dyn Fn(Option<&str>) -> usize + Send + Sync;

/// 未读已读缝类型（体验批二修订，clippy type_complexity 收敛别名，对齐
/// [`ArchiveDeleteFn`] 先例）：参数 = (tool_id, session_id)，与桌面端「叉」
/// 共用 dao::unread::mark_read。
pub type UnreadMarkReadFn = dyn Fn(&str, &str) + Send + Sync;

/// C7 配对计数缝类型（spec §5，clippy type_complexity 收敛别名，对齐
/// [`ConfirmProbeFn`] 先例）：返回运行进程的 (工具, 项目) 表——端点层计同键
/// 出现次数判「配对不确定」（≥2）。生产 = sysinfo 快照 +
/// `commands::session::running_projects_from_processes`（与桌面跳转门同一实现、
/// 同一口径）；测试注入固定假表（sysinfo 真扫描在单测里造不出 ≥2 同键进程，
/// 故必须缝出）。
pub type PairingCounterFn = dyn Fn() -> Vec<(String, String)> + Send + Sync;

/// 对话框在场探针缝类型（丁T3 §2.7，对齐 [`ConfirmProbeFn`] 先例）：
/// 参数 = (session_id, pid)；返回 `Some(选项表)` = 屏读确认**编号选项对话框在场**。
///
/// **语义与 [`crate::inject::dialog::blocks_control_injection`] 同源**：`None` 表示
/// 「无法判定或确实无对话框」（两义同收敛，能力缺失不阻断——裁决见该函数文档）。
///
/// **为什么需要这条缝**：屏读是 Windows 专有 FFI 能力（`read_screen_window` 需要真实
/// conhost 与目标 pid），端点测试进程没有可 attach 的控制台 → 真实实现在 CI 上恒 `None`
/// → 「在场即拒」这条红线**没有任何自动化证据**。缝把「屏读结果」变成可注入的输入，
/// 使两种形态都能在门禁里断言：在场（假体返回真机屏幕原文解析出的选项表）→ 拒绝且
/// 零投递零审计；不在场（假体返回 None）→ 照常投递。
///
/// 生产装配 = [`crate::inject::dialog::probe_screen_dialog`] 的同一实现（单点，
/// 见 `remote/mod.rs`）。
pub type DialogProbeFn =
    dyn Fn(&str, u32) -> Option<Vec<crate::inject::dialog::DialogOption>> + Send + Sync;

/// **可见窗口屏读**探针缝类型（丁T5）：参数 = (session_id, pid)；返回**逐行屏幕文本**
/// （`read_screen_window` 的产物形态；`None` = 读不到屏/平台无屏读能力）。
///
/// # 为什么需要第二条缝（与 [`DialogProbeFn`] 的关系）
///
/// [`DialogProbeFn`] 把「屏读 + 编号选项簇解析」的**结论**（是否在场）缝出来，够用
/// 于「在场即拒」那种**单一布尔判据**。而丁T5 的提交/自由作答阶段机要在**多次读屏
/// 之间**推进状态（提交行 → Review 屏 → 终态；定位 → 文本 → 终态），且每段的判据
/// 各不相同（各自的锚文本）——若仍只缝「结论」，端点的轮询就得为每一段复制一遍
/// 解析逻辑，那正是「同一判据两处实现」的老路。
///
/// 故本缝缝的是**能力**（读一屏）而不是结论：判据留在内核
/// （`inject::question` 的各 `probe_*`），端点只提供「怎么读」与「读的节奏」。
/// 测试用脚本化屏序列注入，则整条编排（分段、复核、中止）在门禁里可断言，
/// 而不是只有真机能覆盖。
pub type ScreenProbeFn = dyn Fn(&str, u32) -> Option<Vec<String>> + Send + Sync;

/// 物化发现缝类型（C6，主会话裁决：做成缝——单测免真实 FS/家目录）：参数 =
/// (tool, since, 目标目录)；返回候选 session id 列表（最新在前，调用方 confirm 终判）。
/// 生产 = `create_discover::discover_new_session` + 真实家目录 root（闭包内
/// `create_store_root` 推导）；测试注入 tempdir/脚本化假体。
pub type CreateDiscoverFn = dyn Fn(&str, std::time::SystemTime, &str) -> Vec<String> + Send + Sync;

/// TUI pid 锚定缝类型（C6）：参数 = (tool, 目标目录)。生产 =
/// `create::find_tui_pid`（30s 轮询，真实 sysinfo 扫描）；测试注入即时
/// Ok(pid)/Err 假体——真实轮询含 1.2s 步距睡眠，不缝则全链端点测试必烧 30s。
pub type CreatePidFindFn = dyn Fn(&str, &std::path::Path) -> Result<u32, String> + Send + Sync;

/// 远程新建会话任务共享态（C6）：**内存态，不持久化**——兔维斯 重启即丢任务，
/// status 端点 404 引导重试（spec §3）。
#[derive(Debug, Clone)]
pub struct CreateTaskShared {
    /// 当前相（opening_terminal / dialog_handling / injecting_first /
    /// waiting_materialize / done / failed——终态 done|failed 不占单飞额度）
    pub phase: String,
    /// 补充说明（失败回执中文现场 / Done 附带 codex hooks 信任提示）
    pub detail: Option<String>,
    /// 物化成功的会话 id（成功终态才有）
    pub session_id: Option<String>,
    /// 起窗后锚定的 TUI pid
    pub spawned_pid: Option<u32>,
}

/// 远程新建会话任务簿 + create 域缝束（C6）。
///
/// **布局说明（计划字面为 `create_tasks: Arc<Mutex<HashMap<u64, CreateTaskShared>>>`、
/// 自增 id 计数器、create_discoverer 缝；此处束为单字段 `Arc<CreateTaskHub>`）**：
/// RemoteState 既有 20+ 处结构字面量测试夹具（server.rs/queue.rs），逐字段追加会把
/// 每处撑成 4-6 行——束成单字段让既有夹具一行适配；内部 `tasks`（Mutex<HashMap>）、
/// `next_id`（AtomicU64，计划授权的计数器形态）、`discoverer`（裁决缝）与计划语义
/// 一一对应，pid_finder / tool_probe / pacer 为计划测试契约（零真实 FS / 零真实
/// 睡眠 / 确定性工具门）所要求的配套缝，见各字段文档。任务簿仅内存态。
pub struct CreateTaskHub {
    /// 任务账本：task_id → 共享态（锁自愈取锁——毒锁不连坐）
    pub tasks: std::sync::Mutex<std::collections::HashMap<u64, CreateTaskShared>>,
    /// 任务 id 自增（fetch_add + 1，从 1 起）
    pub next_id: std::sync::atomic::AtomicU64,
    /// 物化发现缝（生产 = discover_new_session + 真实家目录；测试 = tempdir/脚本假体）
    pub discoverer: Box<CreateDiscoverFn>,
    /// TUI pid 锚定缝（生产 = find_tui_pid 30s；测试 = 即时假体）
    pub pid_finder: Box<CreatePidFindFn>,
    /// 工具安装探测缝（C6 工具门第三道）：生产 = `create::tool_installed`（PATH
    /// 扫描 + OnceLock）；测试注入真假体——OnceLock 进程级缓存使真探测在无该工具
    /// 的机器上恒 false，「合法 → 200」用例将不确定，故缝出（C6 报告登记）
    pub tool_probe: Box<dyn Fn(&str) -> bool + Send + Sync>,
    /// 步距睡眠缝（C6 pacing 承载点）：生产 = `std::thread::sleep`；测试 no-op。
    /// 屏读步距 / 键间隔 / 物化轮询步距全部经它——内核与管线零真实睡眠（
    /// `SCREEN_POLL_STEP_MS` 的消费契约由本缝 + 管线闭包兑现，C6 报告登记）
    pub pacer: Box<dyn Fn(u64) + Send + Sync>,
    /// 证据落档目录缝（§4.4 补实现，2026-10-03 用户裁决）：生产 = `~/.tuvis/
    /// create-evidence/`；测试 = None（禁用——缝测试驱动 unrecognized_screen 失败
    /// 臂也不写真实 ~/.tuvis，零污染红线）。None 时管线跳过落档（不阻塞失败回执）
    pub evidence_dir: Box<dyn Fn() -> Option<std::path::PathBuf> + Send + Sync>,
}

impl CreateTaskHub {
    /// 生产装配（remote::mod.rs STATE 消费）
    pub fn production() -> Self {
        Self {
            tasks: std::sync::Mutex::new(std::collections::HashMap::new()),
            next_id: std::sync::atomic::AtomicU64::new(0),
            discoverer: Box::new(|tool, since, dir| {
                let Some(home) = dirs::home_dir() else {
                    return Vec::new();
                };
                let Some(root) = crate::inject::create_discover::create_store_root(tool, &home)
                else {
                    return Vec::new();
                };
                crate::inject::create_discover::discover_new_session(tool, &root, since, dir)
            }),
            pid_finder: Box::new(|tool, dir| {
                crate::inject::create::find_tui_pid(tool, dir, std::time::Duration::from_secs(30))
            }),
            tool_probe: Box::new(crate::inject::create::tool_installed),
            pacer: Box::new(|ms| std::thread::sleep(std::time::Duration::from_millis(ms))),
            evidence_dir: Box::new(|| {
                dirs::home_dir().map(|h| h.join(".tuvis").join("create-evidence"))
            }),
        }
    }

    /// 测试装配：安装探测恒 true、pid 锚定即时 Ok(4242)、发现空表、步距 no-op。
    /// 全链端点测试缺省缝——「合法 → 200 / 409 单飞 / 终态复建 / 审计三类行」
    /// 瞬时闭环（零真实 FS、零真实睡眠）。**测试支撑面**（doc(hidden)，对齐
    /// `inject::e2e_support` 先例）：集成测试（tests/，无 cfg(test)）与 lib 内测
    /// 共用；生产装配一律 [`production`]。
    #[doc(hidden)]
    pub fn stub() -> Self {
        Self::stub_with(Box::new(|_, _, _| Vec::new()))
    }

    /// 测试装配变体：自定义物化发现缝（其余同 [`stub`]）。
    #[doc(hidden)]
    pub fn stub_with(discoverer: Box<CreateDiscoverFn>) -> Self {
        Self {
            tasks: std::sync::Mutex::new(std::collections::HashMap::new()),
            next_id: std::sync::atomic::AtomicU64::new(0),
            discoverer,
            pid_finder: Box::new(|_, _| Ok(4242)),
            tool_probe: Box::new(|_| true),
            pacer: Box::new(|_| {}),
            evidence_dir: Box::new(|| None),
        }
    }

    /// 单飞占位（原子）：无在飞任务时登记新任务并返回 id；有在飞（phase ≠
    /// done/failed）→ None。终态任务保留在账本（可查、不占额度）。
    pub fn reserve(&self) -> Option<u64> {
        let mut m = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
        if m.values().any(|t| t.phase != "done" && t.phase != "failed") {
            return None;
        }
        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        m.insert(
            id,
            CreateTaskShared {
                phase: "opening_terminal".to_string(),
                detail: None,
                session_id: None,
                spawned_pid: None,
            },
        );
        Some(id)
    }

    /// 逐段更新任务态（缺失 id 静默忽略——任务簿只增不删，理论不可达）
    pub fn update(&self, id: u64, f: impl FnOnce(&mut CreateTaskShared)) {
        let mut m = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(t) = m.get_mut(&id) {
            f(t);
        }
    }

    /// 快照读（status 端点载荷源）
    pub fn snapshot(&self, id: u64) -> Option<CreateTaskShared> {
        self.tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&id)
            .cloned()
    }
}

/// 子 agent 运行源注入缝（spec §5.2，评审定的「每工具一个 source」模式）：
/// 键 = agent_type；闭包参数 (agent_type, session_id)——与 message_source 的
/// (tool, sid, …) 形态同构（键已是工具名，首参供通用假源复用，实现可忽略）。
/// 生产 = 四工具 monitor::subagents::{claude,opencode,kimi,codex}::collect；
/// 测试注入假源（零真实磁盘/DB）。**端点契约冻结**：kimi/opencode 校准只改
/// 各自 source 实现，不动端点与载荷。
pub type SubagentSourceFn =
    dyn Fn(&str, &str) -> Vec<crate::monitor::subagents::SubagentView> + Send + Sync;

/// 子 agent 详情源注入缝（观察台 §三）：键 = agent_type；闭包 (session_id,
/// subagent_id, limit) → MessagesPage。生产仅 claude；未装配 → 端点回
/// supported:false（其余工具转写格式未普查，spec §三.6 如实申报——清单照常）。
pub type SubagentMessageSourceFn =
    dyn Fn(&str, &str, usize) -> Result<crate::remote::content::MessagesPage, String> + Send + Sync;

pub struct RemoteState {
    /// 会话数据源（P8 同源）：生产 = adapter::get_all_sessions；测试注入
    pub session_source: Box<dyn Fn() -> crate::session::SessionsResponse + Send + Sync>,
    /// C7 配对不确定信号缝（spec §5）：运行进程的 (工具, 项目) 表——/sessions 打标
    /// `pairingAmbiguous`、session-send 成功回执附 `pairingHint`（**只提示不拦截**）。
    /// 生产 = sysinfo 全量快照 + 桌面跳转门同款 `running_projects_from_processes`
    /// （单一实现，桌面门零改动）；测试注入固定假表。
    pub pairing_counter: Box<PairingCounterFn>,
    /// 看板隐藏集合读源（APP 软归档，2026-09-20 体验批二）：生产 =
    /// database::board_hidden_ids；测试注入固定集合（零真实 ~/.tuvis 接触）
    pub board_hidden_ids: Box<dyn Fn() -> Vec<String> + Send + Sync>,
    /// 隐藏写缝（hide）：生产 = database::board_hidden_hide；测试记录型假体
    pub board_hidden_hide: std::sync::Arc<dyn Fn(&str) -> usize + Send + Sync>,
    /// 解除隐藏写缝（unhide）：生产 = database::board_hidden_unhide；
    /// 测试记录型假体
    pub board_hidden_unhide: std::sync::Arc<dyn Fn(&str) -> usize + Send + Sync>,
    /// 未读已读缝（体验批二修订，与桌面端「叉」同源）：生产 =
    /// dao::unread::mark_read（删未读池行 + 已读 tombstone）；测试记录型假体
    pub unread_mark_read: std::sync::Arc<UnreadMarkReadFn>,
    /// CLI 会话硬杀缝（/session-close）：生产 = commands::session::kill_pid；
    /// 测试记录型假体（零真杀进程）
    pub session_close: std::sync::Arc<dyn Fn(u32) -> Result<(), String> + Send + Sync>,
    /// 设备存储注入缝：生产 `DeviceStore::global()`；测试 `DeviceStore::memory()`（零接触真实 ~/.tuvis）
    pub store: super::pairing::DeviceStore,
    /// host 载荷注入缝（M3 Task 1）：生产 = remote::host_info()；测试注入假 json（零 DB）
    pub host_source: Box<dyn Fn() -> serde_json::Value + Send + Sync>,
    /// 会话内容源注入缝（M3 Task 7）：生产 = content::read_session_messages（八工具
    /// 统一出口）；测试注入假源（零接触真实 ~/.zcode ~/.dsh 等数据目录）
    pub message_source: Box<super::content::MessageSourceFn>,
    /// 文件路径源注入缝（M3 Task 8）：生产 = files::extract_file_paths（复用
    /// content 层读取的泛化提取）；测试注入假源（返回固定路径表）
    pub path_source: Box<super::files::PathSourceFn>,
    /// 跃迁事件通道（M3 Task 5）：生产 = watcher::event_sender()（全进程同一通道，
    /// 与 SessionWatcher::start 的循环共享）；测试注入新建空通道即可。
    /// **订阅端消费即去重完成**（铁律 4）：事件只含边沿（见 watcher::diff_transitions）
    pub watcher_tx: tokio::sync::broadcast::Sender<super::watcher::TransitionEvent>,
    /// SSE 连接注册表（M4 T0a）：吊销/停止即时断连 + 在线口径数据源
    pub sse_registry: std::sync::Arc<SseRegistry>,
    /// 设备上限注入缝（M4 T2c）：生产 = 读 remote.max_devices KV；测试注入常量
    /// （零 DAO 接触——端点测试不触碰真实 ~/.tuvis）
    pub max_devices_source: Box<dyn Fn() -> usize + Send + Sync>,
    /// per-IP 限速状态机（M5 A3）：POST /pair/pin 锁内查改；内存态重启即清
    pub pin_limiter: std::sync::Mutex<crate::remote::pin::PinRateLimiter>,
    /// 全局限速桶（§G5）：只拦"多来源一起爆破"，阈值远宽于分来源桶——
    /// 单个捣乱者填不满它，因此伤不到别人。
    /// **该断言依赖滑动窗口记账**（评审 A-I1 修复）：计数 = 最近一个锁定窗口内的失败数，
    /// 窗口外的旧记录在判断前丢弃；单调累计的旧实现下单个来源约 100 分钟即可填满阈值，
    /// 那时"填不满"为假，"伤不到别人"随之不成立。改动本桶前先读 `pin::FAILURE_WINDOW_MS`
    pub global_pin_limiter: std::sync::Mutex<crate::remote::pin::PinRateLimiter>,
    /// PIN 源注入缝（M5 A3）：生产 = pin::get_pin（全局 KV）；测试注入固定值（零 DB）
    pub pin_source: Box<dyn Fn() -> Option<String> + Send + Sync>,
    /// 时钟注入缝（M5 A3）：生产 = chrono 毫秒；测试注入可推进原子量——限速锁定
    /// 到期测试用它推进时间（与 pairing 时代 state_with_clock 同目的，零 sleep）
    pub now_source: Box<dyn Fn() -> i64 + Send + Sync>,
    /// via 分通道域名注入缝（M5 A3，/pair/pin 配对时刻消费）：生产 = snapshot 按
    /// mode 分拣 quick/named/tailscale 域名；测试注入固定域名。**纯展示**（via 为
    /// 装饰标注，零豁免后不再参与任何安全判定，2026-10-06 §G2）——**限速不得消费本缝**
    /// （那是下面 rate_bucket_channels_source 的职责，评审 A-I2）
    pub via_hosts_source: Box<ViaHostsSource>,
    /// **限速专用**的信任通道声明源（§G5；评审 A-I2）：每次请求现算「通道 → 已登记
    /// 域名 + 权威来源头」声明表；`gate::rate_bucket_key` 只信表里声明的通道，未声明的
    /// （如 tailscale Funnel）与一切不可证来源一律回落全局桶。空表 = 零信任（fail-closed）。
    /// **与 via_hosts_source 分离**：限速的信任边界不得寄生于展示字段
    pub rate_bucket_channels_source: Box<crate::remote::gate::RateBucketChannelsSource>,
    /// 注入器缝（M7 Task 5 方案 A 提前缝合）：生产 = RealInjector（macOS 三通道执行层；
    /// Windows 占位，Task 15 补真实现）；测试可替换 FakeInjector。
    /// 消费方：inject::queue::flush_one（flush 投递）+ session-send 直发（Task 6 已接线：
    /// 路由注册 / serve 挂 flush 循环 / 审计写口共用）——channel 名（审计）也取自本缝
    pub injector: std::sync::Arc<dyn crate::inject::engine::Injector>,
    /// R5 一键 resume 终端 spawn 缝（Task 11）：生产 = inject::resume::spawn_terminal
    /// （真开窗）；测试注入记录型假 spawner（零真开窗）。消费方：session-open 端点
    /// （inject::resume::open_session_terminal_with 的 spawner 参数）
    pub resume_spawner: std::sync::Arc<crate::inject::resume::SpawnFn>,
    /// 归档读源注入缝（spec §6.1）：生产 = database::query_archive_all（全量行，
    /// 窗口/排除在端点内做）；测试注入固定行集（零真实 ~/.tuvis 接触）
    pub archive_source: Box<dyn Fn() -> Vec<crate::database::SessionArchiveRow> + Send + Sync>,
    /// 归档删除缝：生产 = database::delete_archive；测试记录型假体返回计数
    pub archive_delete: std::sync::Arc<ArchiveDeleteFn>,
    /// A1 写入确认缝（M9R Task 5）：参数 = (tool, session_id, stamp)。生产 =
    /// 会话消息读路径查 24 字符尾戳（与 /session-messages 数据同源；读失败 =
    /// 未命中，诚实口径）；测试恒 true（确认失败用例就地覆盖恒 false）。
    /// 消费方：inject::confirm（flush_one 直发/插队确认轮询全经本缝，queue 测试
    /// 零接触真实文件）——与 injector 缝同模式（生产装配无法捕获自身 Arc，
    /// 故闭包内直调读路径）。
    pub confirm_probe: std::sync::Arc<ConfirmProbeFn>,
    /// 对话框在场探针缝（丁T3 §2.7）：参数 = (session_id, pid)。生产 =
    /// `inject::dialog::probe_screen_dialog`（Windows 屏读可见窗口 + 编号选项簇解析；
    /// 非 Windows 恒 None）；测试注入假体（在场/不在场两形态可断言，零真实窗口）。
    /// 消费方：`remote::api::session_mode_switch` 的控制类注入守卫（见
    /// [`DialogProbeFn`] 的缝理由与 [`crate::inject::dialog::blocks_control_injection`]
    /// 的裁决）。**注意 pid 也从参数传入**：端点侧已从会话快照取到，不让假体去猜。
    pub dialog_probe: std::sync::Arc<DialogProbeFn>,
    /// **可见窗口屏读**能力缝（丁T5）：参数 = (session_id, pid)，返回逐行屏幕文本。
    /// 生产 = `inject::windows_console::read_screen_window`（Windows）；非 Windows 恒
    /// None（无屏读 → 阶段机如实中止并引导终端，见 `inject::question` 各段的中止文案）。
    /// 消费方：`remote::api::session_question_answer` 的提交/自由作答阶段机（缝的形态
    /// 理由见 [`ScreenProbeFn`] 文档）。**测试注入脚本化屏序列**——否则
    /// 「每段屏读复核」这条控制流只有实机能覆盖（本批已多次栽在这上面）。
    pub screen_probe: std::sync::Arc<ScreenProbeFn>,
    /// **注入能力开关表**（2026-10-03 数字直选自适应）：per-state 持有——端点测试
    /// 各自独立表避免并行探测写回互相污染；生产装配共享同一 Arc（全局一份）
    pub capability_table: crate::inject::capability::Table,
    /// 远程新建会话任务簿 + create 域缝束（C6）：任务账本（内存态）、物化发现缝、
    /// pid 锚定缝、工具安装探测缝、步距睡眠缝。布局说明与各缝语义见
    /// [`CreateTaskHub`] 文档；测试装配 `CreateTaskHub::stub()`。
    pub create_hub: std::sync::Arc<CreateTaskHub>,
    /// **靶向证据源**（L13，C0-③）：`(工具 id, 会话 cwd) -> 靶向证据`（候选进程表 +
    /// 候选 TTY 表 + 会话级 TTY 证据）。生产 = `window::tty_map::tool_target_evidence`
    /// （共享进程快照 + adapter 进程发现 + macOS TTY 采数 + 恒 None 的会话级证据）；
    /// 测试注入合成证据（零真实进程 / 零窗口）。
    /// **消费方 = 写侧全部注入路径的唯一靶向入口**
    /// `window::tty_map::resolve_session_target`（send 漏斗 `inject::queue` +
    /// approve/reject、question、mode menu、mode switch 端点）：缝只收「目标是谁」的
    /// 输入，判定在内核——避免把判定逻辑复制到各端点。
    pub target_evidence: Box<crate::window::tty_map::TargetEvidenceFn>,
    /// 敏感黑名单主目录基准注入缝（M5 P2-a 追记）：生产 = `dirs::home_dir()`；
    /// 测试注入 tempdir home（零接触真实主目录）。**端点必须消费它**——
    /// 3d22e2e 曾传 None 使 ~/.ssh 等黑名单整段失效（单元测试全绿而生产裸奔）
    pub home_source: Box<dyn Fn() -> Option<String> + Send + Sync>,
    /// 远程端外观配置读源（2026-10-05 UI 改版，spec §6.2）：生产 =
    /// `database::get_setting("remote_ui_config")`（settings KV 单行 JSON：
    /// daySkin/nightSkin/font/radius/accent）；测试 = `|| None`（端点回落默认值，
    /// ui-config 专用用例就地覆盖）。**只读缝**——写走桌面 tauri 命令 set_setting
    /// 直写 KV，不经 axum（移动端只 GET，改外观是桌面端职责）
    pub ui_config_source: Box<dyn Fn() -> Option<String> + Send + Sync>,
    /// 子 agent 运行源注入缝（spec §5.2）：见 [`SubagentSourceFn`] 文档。
    /// 空表 = 一切工具空态（测试缺省；未知 agent_type 同形）
    pub subagent_source: std::collections::HashMap<&'static str, Box<SubagentSourceFn>>,
    /// 子 agent 详情源注入缝（观察台 §三）：见 [`SubagentMessageSourceFn`] 文档。
    /// 空表 = 一切工具 supported:false（测试缺省）
    pub subagent_message_source:
        std::collections::HashMap<&'static str, Box<SubagentMessageSourceFn>>,
}

/// API 子路由：业务端点 + /pair/pin + 内层 fallback（未知 API 路径直接 403）+ gate 内层 layer。
/// 注意：gate 在 `with_state` 之前 `layer`，故它包裹的是**本子路由已注册的全部端点与 fallback**；
/// 之后 Task 7 从外部追加的静态路由不在本子路由内，天然不过闸（结构隔离，非顺序巧合）。
fn api_router(state: Arc<RemoteState>) -> Router<Arc<RemoteState>> {
    Router::new()
        .route("/sessions", get(api::sessions))
        .route("/host", get(api::host))
        // /ui-config（2026-10-05 UI 改版）：远程端外观配置下发（settings KV 只读缝；
        // PIN 门禁内层 gate 结构性覆盖——新端点不需要各自鉴权代码）
        .route("/ui-config", get(api::ui_config))
        // /channel（§C5，Task 10）：通道能力**装饰**端点（走哪条通道/带宽受限 +
        // 实测速率）。读 Host 推断、可被伪造、允许不准、**永不得进安全判定**
        // （不变量 §G1 推论③，见 api::channel_info 注释）；gate 由本子路由的
        // 内层 layer 结构性覆盖
        .route("/channel", get(api::channel_info))
        // /events（M3 Task 6）：SSE 长连接，gate 由本子路由的 layer 结构性覆盖
        // （与其余端点同一内层 gate，不需要额外 middleware）
        .route("/events", get(api::events))
        // /session-messages（M3 Task 7）：单会话内容读取（C2 后端，八工具统一出口）
        .route("/session-messages", get(api::session_messages))
        // /session-files（M3 Task 8）：会话涉及的文件路径表（链接化数据源）
        .route("/session-files", get(api::session_files))
        // /session-subagents（2026-10-08 子 agent chip，spec §5.2）：会话运行中子
        // agent 视图（四工具 source 注入缝；PIN 门禁内层 gate 结构性覆盖——新端点
        // 不需要各自鉴权代码；空态唯一权威）
        .route("/session-subagents", get(api::session_subagents))
        // /session-subagent-messages（2026-10-09 观察台 §三）：子 agent 执行过程
        // 详情（claude 复用消息映射；其余工具 supported=false 如实申报）
        .route(
            "/session-subagent-messages",
            get(api::session_subagent_messages),
        )
        // /file（M3 Task 8）：会话 cwd 内安全文件读取（预览）
        .route("/file", get(api::read_file))
        // M7 Task 6：注入三端点（PIN 门禁内层 gate 结构性覆盖——新端点不需要各自
        // 鉴权代码；session-send 直发/入队 + send-info 可用性 + queue 视图/插队/撤回）
        .route("/session-send", post(api::session_send))
        .route("/session-send-info", get(api::session_send_info))
        .route("/session-queue", get(api::session_queue))
        .route("/session-queue/jump", post(api::session_queue_jump))
        .route("/session-queue/retract", post(api::session_queue_retract))
        // Task 8（H7）：无头回合取消（移动端回执卡的取消钮）——PIN 门禁内层 gate
        // 结构性覆盖，新端点不需要各自鉴权代码
        .route(
            "/session-headless-cancel",
            post(api::session_headless_cancel),
        )
        // Task 13（C4 / H11）：claude 无头审批卡两端点——GET 取**当前待答**的审批/问答请求
        // （无在飞回合或无待答项 → `{pending:null}`），POST 投递决策（allow/deny/answer；
        // 送达才落 `headless_approve` 审计行）。PIN 门禁内层 gate 结构性覆盖，与取消端点同规。
        .route(
            "/session-headless-approval",
            get(api::session_headless_approval),
        )
        .route(
            "/session-headless-approve",
            post(api::session_headless_approve),
        )
        // Task 12（H10）：zcode 无头**新建**——POST 建会话并注入首句；GET 是表单前置面
        // （候选列表 + 总开关状态 + 黄字信号）。PIN 门禁内层 gate 结构性覆盖。
        .route("/session-create-zcode", post(api::session_create_zcode))
        .route(
            "/session-create-zcode-info",
            get(api::session_create_zcode_info),
        ) // M8 Task 11：审批端点（红卡一键批准/拒绝——选项可用性 + 按键应答；PIN 门禁
        // 内层 gate 结构性覆盖，新端点不需要各自鉴权代码）
        .route(
            "/session-approve-options",
            get(api::session_approve_options),
        )
        .route("/session-approve", post(api::session_approve))
        .route("/session-plan-feedback", post(api::session_plan_feedback))
        // 批次乙 T8：问答端点（AskUserQuestion 问答卡——可用性/题目结构 + 应答注入
        // 序列；PIN 门禁内层 gate 结构性覆盖，新端点不需要各自鉴权代码）
        .route("/session-question", get(api::session_question))
        .route(
            "/session-question/answer",
            post(api::session_question_answer),
        )
        // 批次丙 T6：模式端点（当前档屏读 + 切档注入；PIN 门禁内层 gate 结构性
        // 覆盖，新端点不需要各自鉴权代码）
        .route("/session-mode", get(api::session_mode))
        .route("/session-mode/switch", post(api::session_mode_switch))
        // 2026-09-23 用户方案：codex 权限组的「终端菜单单选题」——open/pick 两动作
        // （POST）+ 纯屏读重同步（GET，零注入）
        .route("/session-mode/menu", post(api::session_mode_menu))
        .route("/session-mode/menu", get(api::session_mode_menu_read))
        // 2026-09-20：移动端附件上传（落盘会话工作目录 .tuvis-attachments/<会话>/，
        // 路径随消息内联标记注入；PIN 门禁内层 gate 结构性覆盖；20MB 显式上限——
        // axum 默认 2MB；超限时 handler 先按 Content-Length 预检给结构化 413）
        .route(
            "/session-attachment",
            post(api::session_attachment).layer(axum::extract::DefaultBodyLimit::max(
                crate::remote::attachments::MAX_ATTACHMENT_BYTES,
            )),
        )
        // M6R–M9R Task 11：一键 resume 端点（R5，PIN 门禁内层 gate 结构性覆盖，
        // 新端点不需要各自鉴权代码；spawn 缝注入使测试零真开窗）
        .route("/session-open", post(api::session_open))
        // C6 远程新建会话（spec §3/§5）：起窗 + 弹窗处置 + 首句注入 + 物化确认的
        // 异步任务三端点（PIN 门禁内层 gate 结构性覆盖，新端点不需要各自鉴权代码；
        // 任务簿内存态 + 全局单飞 + 审计三类行 create|dialog|send）
        .route("/session-create", post(api::session_create))
        .route("/session-create/status", get(api::session_create_status))
        .route("/create-projects", get(api::create_projects))
        // 历史会话区（spec 2026-09-20-mobile-archive-history §6.1）：懒加载列表 + 手动管理
        .route(
            "/sessions-archived",
            get(api::sessions_archived).delete(api::sessions_archived_delete),
        )
        // 看板关闭/软归档（2026-09-20 体验批二）：CLI 硬杀 + APP 软归档/移回，
        // 同在 nest 内结构性继承 PIN gate
        .route("/session-close", post(api::session_close))
        .route("/session-hide", post(api::session_hide))
        .route("/session-unhide", post(api::session_unhide))
        // M5 A3：访问密码端点——密码制唯一换 cookie 入口（gate 放行名单同步收口为
        // /pair/pin 精确相等；旧 /pair 直通与 /pair/* 审批路由已删除，未知路径落
        // 内层 fallback 403）
        .route("/pair/pin", post(api::pair_pin))
        // 内层 fallback：nest 前缀下的未知/多余路径不得裸奔——没有它，
        // `/m/api/v1/nope` 会落到外层 fallback（Task 7 的静态兜底 → 200 静态内容），
        // 绕开"所有 /m/api/* 过 gate（403）"这条安全不变量（评审实测确认）
        .fallback(|| async { axum::http::StatusCode::FORBIDDEN })
        .layer(middleware::from_fn_with_state(
            state.clone(),
            super::gate::gate,
        ))
}

/// 组装路由：gate 需读 RemoteState（store 注入）——用 from_fn_with_state 而非 from_fn。
/// API 结构化嵌套（Important 1）：`nest` 使 gate 只作用于 `/m/api/v1/*` 且覆盖其全子树；
/// 未知 API 路径由内层 fallback 403 收口，外层（Task 7 静态资源）永不可见 API 路径。
pub fn router(state: Arc<RemoteState>) -> Router {
    Router::new()
        .nest("/m/api/v1", api_router(state.clone()))
        // 裸前缀带尾斜杠 `/m/api/v1/` 实测**不**匹配 nest 的 catch-all（matchit 的 `{*rest}`
        // 要求至少一个非空段，也不做尾斜杠归一化），会落到外层静态 fallback → 200。
        // 与"所有 /m/api/* 过 gate（403）"冲突，故显式 403 收口（any：方法无关一律 403）。
        // 终审修复轮保留了此条（未并入 static_fallback）：它挂在 router() 上，对**任意**
        // 外层 fallback 装配（含测试/未来变体的自定义 fallback）结构性生效，不依赖
        // static_fallback 分流的实现自觉；其余 /m/api 变体（裸前缀 / 尾斜杠 / 未知版本）
        // 由 static_fallback 的字面收口兜住
        .route(
            "/m/api/v1/",
            axum::routing::any(|| async { axum::http::StatusCode::FORBIDDEN }),
        )
        .with_state(state)
}

/// 生产装配：`router()`（gate 结构不变量）+ /m 静态入口 + 顶层静态 fallback。
/// 结构契约（评审裁决 2，勿退化）：静态侧**只**以「在 `router()` 返回的 Router 上追加
/// `.route("/m", ...)` 与顶层 `.fallback(...)`」的形态存在——**绝对不要**改成 catch-all
/// 路由（`/m/{*path}`）或在外层注册 `/m/api/v1/...`：catch-all 会先于 nest 内层 403
/// fallback 命中，让未知 API 路径 200 裸奔
/// （task7_static_routes_do_not_uncover_unknown_api_paths 复刻的正是本函数的追加动作）
pub fn router_with_static(state: Arc<RemoteState>) -> Router {
    router(state)
        .route("/m", get(mobile_index))
        .fallback(static_fallback)
}

pub async fn serve(bind: &str, port: u16, state: Arc<RemoteState>) -> Result<(), String> {
    let listener = tokio::net::TcpListener::bind((bind, port))
        .await
        .map_err(|e| format!("绑定 {bind}:{port} 失败: {e}"))?;
    // M3 Task 5：事件桥在**绑定成功后**启动（幂等——全局 Once 保证扫描循环全进程只
    // spawn 一次）。放在 bind 之后：端口被占等启动失败不遗留 2s 会话扫描（扫描是重活，
    // 见 adapter::get_all_sessions 的单飞护栏注释）；watcher 生命周期随进程结束，
    // stop_server 不显式停止（关闭远程后循环仍扫描，为已有取舍）
    super::watcher::SessionWatcher::start();
    // M7 Task 6：注入 flush 循环接线（Task 5 交付的循环体在此合入编译）——订阅同一
    // watcher 跃迁通道（broadcast 多订阅端各自独立游标，与 SSE 消费互不影响），
    // 会话回到可输入态时逐条投递该会话的待发队列；与 session-send 直发共用
    // in-flight 守卫（同会话并发双投防护）
    crate::inject::queue::spawn_flush_loop(state.clone());
    // M5 A3：来源 IP 记录——into_make_service_with_connect_info 注入 ConnectInfo
    // extension。**消费方只有 pair_pin**（限速分桶键 + 落库来源），且**不参与放行判定**
    // （不变量 G1：来源地址/请求头/进程归属不得影响鉴权；原 gate 本机豁免已整块删除，
    // gate 不再消费它）。oneshot 测试在请求侧自补同一 extension。
    axum::serve(
        listener,
        router_with_static(state).into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await
    .map_err(|e| format!("serve: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    // M4 T0a（编译硬阻断补 import，先例同上）：SSE 流逐帧消费需要 StreamExt::next
    use futures::StreamExt as _;

    /// L13 靶向证据桩（缺省）：空证据 = 无候选进程 → 靶向无歧义 → 放行（与修复前行为
    /// 一致）。需要「≥2 候选 ⇒ 拒绝」的用例用 [`with_target_evidence`] 覆盖。
    fn no_target_evidence() -> Box<crate::window::tty_map::TargetEvidenceFn> {
        Box::new(|_, _| crate::window::tty_map::TargetEvidence::default())
    }

    /// L13 测试用：覆盖 state 的靶向证据源（建造器刚返回的 `Arc` 引用计数为 1 →
    /// `Arc::get_mut` 可取可变引用；一经共享即 panic，不会静默改到别人头上）。
    fn with_target_evidence(
        mut state: Arc<RemoteState>,
        evidence: crate::window::tty_map::TargetEvidence,
    ) -> Arc<RemoteState> {
        Arc::get_mut(&mut state)
            .expect("state 尚未共享（建造器返回值立即覆盖）")
            .target_evidence = Box::new(move |_, _| evidence.clone());
        state
    }

    /// L13 合成证据：`pids` 个同 cwd 候选进程（cwd = 会话夹具的 `project_path`）
    fn target_evidence_in_cwd(cwd: &str, pids: &[u32]) -> crate::window::tty_map::TargetEvidence {
        crate::window::tty_map::TargetEvidence {
            processes: pids
                .iter()
                .map(|pid| crate::adapter::AgentProcess {
                    pid: *pid,
                    cpu_usage: 0.0,
                    cwd: Some(std::path::PathBuf::from(cwd)),
                    exe: None,
                    form: crate::session::ProcessForm::Cli,
                })
                .collect(),
            candidate_ttys: Vec::new(),
            session_tty: None,
        }
    }

    fn test_state() -> Arc<RemoteState> {
        Arc::new(RemoteState {
            ui_config_source: Box::new(|| None),
            subagent_source: std::collections::HashMap::new(),
            subagent_message_source: std::collections::HashMap::new(),
            target_evidence: no_target_evidence(),
            capability_table: crate::inject::capability::new_table(),
            session_source: Box::new(|| crate::session::SessionsResponse {
                sessions: vec![],
                total_count: 7,
                waiting_count: 0,
            }),
            store: crate::remote::pairing::DeviceStore::memory(), // 内存库——测试不碰真实 ~/.tuvis
            // M7 Task 5：注入器缝——本组测试不触 flush 路径，用生产占位
            injector: std::sync::Arc::new(crate::inject::engine::RealInjector),
            // R5 一键 resume spawn 缝（Task 11）：本组测试不触 session-open，注 no-op 桩
            resume_spawner: std::sync::Arc::new(|_: &crate::inject::resume::SpawnSpec| Ok(())),
            // C6：create 缝束缺省 stub（任务簿+发现/pid/工具探测/步距缝），仅
            // session-create 用例就地覆盖
            create_hub: create_hub_stub(),
            archive_source: Box::new(Vec::new),
            archive_delete: std::sync::Arc::new(|_: Option<&str>| 0usize),
            // A1 写入确认缝（M9R Task 5）：测试恒命中（首轮即中，零延迟零等待）
            confirm_probe: std::sync::Arc::new(|_, _, _| true),
            // 丁T3：本组测试的对话框在场探针缺省「无法判定」（None）——控制类注入
            // 照常投递；「在场即拒」的用例就地建 state 覆盖为假体（见 mode_switch_* 用例）
            dialog_probe: std::sync::Arc::new(|_, _| None),
            screen_probe: std::sync::Arc::new(|_, _| None),
            host_source: Box::new(|| {
                serde_json::json!({
                    "host": { "name": "test-host", "platform": "macos", "version": "0.0.0-test" },
                    "enabledTools": ["claude"]
                })
            }),
            // M3 Task 7：本组测试不触 /session-messages，注入恒 Err 的桩
            message_source: Box::new(|_, _, _| Err("测试桩：未注入内容源".to_string())),
            // 本组测试不触 /session-files /file：注入恒空的路径源
            path_source: Box::new(|_, _, _| (Vec::new(), false)),
            // M3 Task 5：测试用空事件通道（不启动 watcher——零后台扫描）
            watcher_tx: tokio::sync::broadcast::channel(64).0,
            board_hidden_ids: Box::new(Vec::new),
            board_hidden_hide: std::sync::Arc::new(|_| 0usize),
            board_hidden_unhide: std::sync::Arc::new(|_| 0usize),
            unread_mark_read: std::sync::Arc::new(|_, _| ()),
            session_close: std::sync::Arc::new(|_| Ok(())),
            // M4 T0a（brief 指定）：本任务新增字段，测试用空注册表即可
            sse_registry: Arc::new(SseRegistry::default()),
            max_devices_source: Box::new(|| 3),
            // M5 A3 新字段：限速器全新；PIN 恒 "1234"；时钟真实毫秒；无展示通道域名、
            // 无限速信任声明（声明表为空 ⇒ 一切回环来源回落全局桶）
            // ——零豁免后 gate 只看设备凭据，既有"无 cookie → 403"断言不受 Host 影响
            pin_limiter: std::sync::Mutex::new(crate::remote::pin::PinRateLimiter::new()),
            global_pin_limiter: std::sync::Mutex::new(crate::remote::pin::PinRateLimiter::global()),
            pin_source: Box::new(|| Some("1234".to_string())),
            now_source: Box::new(|| chrono::Utc::now().timestamp_millis()),
            via_hosts_source: Box::new(|| None),
            rate_bucket_channels_source: Box::new(Vec::new),
            home_source: Box::new(|| None),
            // C7：配对计数缝缺省空表（既有用例零影响；配对打标/提示用例就地覆盖）
            pairing_counter: Box::new(Vec::new),
        })
    }

    async fn body_string(b: axum::http::Response<Body>) -> String {
        String::from_utf8(b.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap()
    }

    /// C6：create 缝束缺省测试装配（安装探测恒真 / pid 锚定即时 Ok(4242) / 物化
    /// 发现空表 / 步距 no-op——零真实 FS 零真实睡眠）。session-create 端点用例
    /// 按需以 `CreateTaskHub::stub_with` / 结构字面量覆盖各缝。
    fn create_hub_stub() -> std::sync::Arc<CreateTaskHub> {
        std::sync::Arc::new(CreateTaskHub::stub())
    }

    #[tokio::test]
    async fn gate_403_matrix_and_pin_pair_flow() {
        let app = crate::remote::server::router(test_state());
        // 1) 无 cookie 访问 sessions → 403（请求无 Host 头：Host 早已不参与放行——
        //    零豁免后无论 Host 是什么、来源是不是回环都 403，不变量 G1）
        let r = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/m/api/v1/sessions")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), 403);
        // 2) PIN 错 → 401 invalid_pin（非 403：配对入口的错误语义是可鉴别的 401+计数）
        let r = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/m/api/v1/pair/pin")
                    .header("content-type", "application/json")
                    // pair_pin 提取 ConnectInfo（来源 IP 入限速与指纹）——oneshot 请求侧自补
                    .extension(axum::extract::ConnectInfo(
                        "127.0.0.1:12345".parse::<std::net::SocketAddr>().unwrap(),
                    ))
                    .body(Body::from(r#"{"pin":"9999"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), 401);
        // 3) PIN 正确 → 200 + Set-Cookie mam_device
        let r = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/m/api/v1/pair/pin")
                    .header("content-type", "application/json")
                    .extension(axum::extract::ConnectInfo(
                        "127.0.0.1:12345".parse::<std::net::SocketAddr>().unwrap(),
                    ))
                    .body(Body::from(r#"{"pin":"1234"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let cookie = r
            .headers()
            .get("set-cookie")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        // 五属性全断言（评审 Important 3）：device id 为 32 位 hex；属性完整串比对——
        // 此前只查 3 项，删掉 Max-Age 或改成 Max-Age=1 全套测试仍绿（180 天不变量失守）。
        // 期望值由 DEVICE_TTL_MS 推导，不写魔法数字；顺序与实现一致（属性顺序即响应语义）
        let device = cookie
            .split("mam_device=")
            .nth(1)
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();
        assert!(
            device.len() == 32 && device.chars().all(|c| c.is_ascii_hexdigit()),
            "device_id 应为 32 位 hex，实际 {device:?}"
        );
        assert_eq!(
            cookie,
            format!(
                "mam_device={device}; Path=/m; HttpOnly; SameSite=Lax; Max-Age={}",
                crate::remote::pairing::DEVICE_TTL_MS / 1000
            ),
            "cookie 必须同时具备 mam_device=hex / Path=/m / HttpOnly / SameSite=Lax / Max-Age=180d"
        );
        // 4) 带 cookie 访问 sessions → 200，数据来自注入源（total_count=7）
        let r = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/m/api/v1/sessions")
                    .header("cookie", format!("mam_device={device}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        // M2-R2 顺手项：会话数据不得被中间层缓存（设备门禁下的私有数据）
        assert_eq!(
            r.headers()
                .get("cache-control")
                .and_then(|v| v.to_str().ok()),
            Some("no-store"),
            "sessions 响应必须带 Cache-Control: no-store"
        );
        assert!(body_string(r).await.contains("\"totalCount\":7"));
        // 5) PIN 重放再配对 → 仍 200（与旧一次性 token 语义相反：访问密码常驻，
        //    同指纹重绑命中旧行——不新增设备）
        let r = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/m/api/v1/pair/pin")
                    .header("content-type", "application/json")
                    .extension(axum::extract::ConnectInfo(
                        "127.0.0.1:12345".parse::<std::net::SocketAddr>().unwrap(),
                    ))
                    .body(Body::from(r#"{"pin":"1234"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "PIN 非一次性——重放可再次配对");
    }

    // ==== 追加测试（简报单个之外；理由：锁定简报未覆盖但已裁决的契约） ====

    /// 构造请求的收敛助手（cookie / body 可选）
    fn req(
        method: &str,
        uri: &str,
        cookie: Option<&str>,
        body: Option<&str>,
    ) -> axum::http::Request<Body> {
        let mut b = axum::http::Request::builder().method(method).uri(uri);
        if body.is_some() {
            b = b.header("content-type", "application/json");
        }
        if let Some(c) = cookie {
            b = b.header("cookie", c);
        }
        // M5 A1 评审修复随记：api::pair 现以 ConnectInfo 取来源 IP 入指纹——
        // oneshot 不注入该 extension，测试请求侧自补（与 post_json 同一适配）
        b = b.extension(axum::extract::ConnectInfo(
            "127.0.0.1:12345".parse::<std::net::SocketAddr>().unwrap(),
        ));
        b.body(match body {
            Some(s) => Body::from(s.to_string()),
            None => Body::empty(),
        })
        .unwrap()
    }

    /// 响应快照：(状态码, 响应体, 是否带 Set-Cookie)——用于比对拒绝三态是否可区分
    async fn snapshot(r: axum::http::Response<Body>) -> (u16, String, bool) {
        let status = r.status().as_u16();
        let has_cookie = r.headers().contains_key("set-cookie");
        (status, body_string(r).await, has_cookie)
    }

    // ==== 2026-10-05 UI 改版：/m/api/v1/ui-config（外观配置下发，spec §6.2）====

    /// 注册一台活跃设备并返回其 gate cookie（paired_at = now，TTL 窗口内即过闸）
    fn paired_cookie(state: &Arc<RemoteState>, id: &str) -> String {
        let now = chrono::Utc::now().timestamp_millis();
        let dev = crate::remote::pairing::NewDevice {
            id: id.into(),
            name: String::new(),
            ua: format!("ua-{id}"),
            origin_ip: format!("ip-{id}"),
            via: String::new(),
            paired_at: now,
        };
        state
            .store
            .with(|c| crate::remote::pairing::persist_device(c, &dev).unwrap());
        format!("mam_device={id}")
    }

    #[tokio::test]
    async fn ui_config_returns_defaults_when_unset() {
        // test_state 的 ui_config_source = || None：KV 未配置 → 回落默认值
        let state = test_state();
        let cookie = paired_cookie(&state, "ui-dev-1");
        let r = router(state)
            .oneshot(req("GET", "/m/api/v1/ui-config", Some(&cookie), None))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["daySkin"], "lpaper");
        assert_eq!(v["nightSkin"], "npaper");
        assert_eq!(v["font"], "std");
        assert_eq!(v["radius"], 12);
        assert_eq!(v["accent"], "edge");
    }

    #[tokio::test]
    async fn ui_config_returns_saved_value() {
        let mut state = test_state();
        // 就地覆盖读缝：桌面外观配置器保存过的自定义配置（KV 里的 JSON 原样透传）
        if let Some(s) = Arc::get_mut(&mut state) {
            s.ui_config_source = Box::new(|| {
                Some(
                    r#"{"daySkin":"lpure","nightSkin":"npure","font":"term","radius":18,"accent":"tint"}"#
                        .to_string(),
                )
            });
        }
        let cookie = paired_cookie(&state, "ui-dev-2");
        let r = router(state)
            .oneshot(req("GET", "/m/api/v1/ui-config", Some(&cookie), None))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["daySkin"], "lpure");
        assert_eq!(v["nightSkin"], "npure");
        assert_eq!(v["font"], "term");
        assert_eq!(v["radius"], 18);
        assert_eq!(v["accent"], "tint");
    }

    /// gate 的滑动 TTL：超窗设备拒绝、活跃设备过闸即刷新 last_seen
    #[tokio::test]
    async fn gate_refreshes_last_seen_and_rejects_stale_device() {
        let state = test_state();
        let now = chrono::Utc::now().timestamp_millis();
        // M5 A1 upsert 按指纹（sha256(ua|ip)）去重：ua/ip 全空的设备会互相撞键合并成一行，
        // 故按 id 派生合成值保证 stale/fresh 两行并存（与真机"不同设备"语义一致）
        let dev = |id: &str, paired_at: i64| crate::remote::pairing::NewDevice {
            id: id.into(),
            name: String::new(),
            ua: format!("ua-{id}"),
            origin_ip: format!("ip-{id}"),
            via: String::new(),
            paired_at,
        };
        state.store.with(|c| {
            crate::remote::pairing::persist_device(
                c,
                &dev("stale", now - crate::remote::pairing::DEVICE_TTL_MS - 1),
            )
            .unwrap();
            crate::remote::pairing::persist_device(c, &dev("fresh", now - 5_000)).unwrap();
        });
        let app = router(state.clone());

        // 超窗（last_seen 早于 TTL 窗口）→ 403
        let r = app
            .clone()
            .oneshot(req(
                "GET",
                "/m/api/v1/sessions",
                Some("mam_device=stale"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 403);

        // 活跃 → 200，且 gate 把 last_seen_at 推到当前时刻（滑动续期）
        let r = app
            .clone()
            .oneshot(req(
                "GET",
                "/m/api/v1/sessions",
                Some("mam_device=fresh"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let seen = state.store.with(|c| {
            c.query_row(
                "SELECT last_seen_at FROM remote_devices WHERE id = 'fresh'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap()
        });
        assert!(seen > now - 5_000, "gate 应刷新 last_seen_at，实际 {seen}");
    }

    /// gate 拒绝不可区分性（评审 Important 4）："无 cookie"与"cookie 无效（未配对 id）"
    /// 必须给出**完全一致**的响应——状态码、响应体、以及无 Set-Cookie 等额外头。
    /// 若变异为"两种失败写入不同响应体/附带头"，即构成设备有效性预言机——本测试锁死该不变量
    #[tokio::test]
    async fn gate_rejections_are_indistinguishable() {
        let state = test_state();
        let app = router(state);
        // (a) 无 cookie
        let no_cookie = snapshot(
            app.clone()
                .oneshot(req("GET", "/m/api/v1/sessions", None, None))
                .await
                .unwrap(),
        )
        .await;
        // (b) cookie 无效：形如设备 id 但从未配对
        let unknown_device = snapshot(
            app.clone()
                .oneshot(req(
                    "GET",
                    "/m/api/v1/sessions",
                    Some("mam_device=0123456789abcdef0123456789abcdef"),
                    None,
                ))
                .await
                .unwrap(),
        )
        .await;
        // (c) cookie 存在但值为空 —— 同属"无效凭据"
        let empty_value = snapshot(
            app.clone()
                .oneshot(req("GET", "/m/api/v1/sessions", Some("mam_device="), None))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(
            no_cookie, unknown_device,
            "无 cookie 与无效 cookie 响应不可区分"
        );
        assert_eq!(
            unknown_device, empty_value,
            "空值 cookie 与无效 cookie 响应不可区分"
        );
        assert_eq!(
            no_cookie,
            (403, String::new(), false),
            "gate 拒绝一律 403 空体无 cookie，不得给有效性预言机"
        );
    }

    /// 结构契约锁定 + 绕过回归实测（评审 Important 1 核心验收）：
    /// 在 `router()` 返回值上**复刻 Task 7 的追加动作**——`.route("/m", ...)`（配对页）
    /// 与顶层 `.fallback(...)`（静态资源兜底）——然后确认：
    /// - 未知 API 路径 `/m/api/v1/nope` 无 cookie 仍 **403**（修复前：被后加 fallback 替换掉
    ///   被 gate 包裹的默认 fallback → 200 静态内容，门禁整体绕过）；
    /// - `/m`、`/m/assets/x.js`（模拟静态）无 cookie 可访问（配对页必须能加载）。
    #[tokio::test]
    async fn task7_static_routes_do_not_uncover_unknown_api_paths() {
        // 模拟 Task 7：静态路由 + 顶层静态 fallback 追加在 router() 之后
        let app = router(test_state())
            .route("/m", get(|| async { "pair-page" }))
            .fallback(|| async { "static-fallback" });

        // (1) 未知 API 路径：内层 fallback 403，绝不被顶层静态兜底接管
        for uri in [
            "/m/api/v1/nope",       // 未知子路径
            "/m/api/v1/",           // 裸前缀带尾斜杠（nest catch-all 不匹配，显式 403 收口）
            "/m/api/v1/sessions/x", // 已知端点下的多余段
        ] {
            let r = app
                .clone()
                .oneshot(req("GET", uri, None, None))
                .await
                .unwrap();
            assert_eq!(
                r.status(),
                403,
                "追加静态 fallback 后 {uri} 仍须过 gate 且不被静态兜底接管"
            );
        }

        // (2) 配对页本身不受 gate 约束（无 cookie 可加载）
        let r = app
            .clone()
            .oneshot(req("GET", "/m", None, None))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(body_string(r).await, "pair-page");

        // (3) 静态资产同理放行
        let r = app
            .clone()
            .oneshot(req("GET", "/m/assets/x.js", None, None))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(body_string(r).await, "static-fallback");

        // (4) 已知 API 路径（sessions）无 cookie 仍 403——gate 未被结构变更放宽
        let r = app
            .oneshot(req("GET", "/m/api/v1/sessions", None, None))
            .await
            .unwrap();
        assert_eq!(r.status(), 403);
    }

    /// 追加静态路由后 pair/pin 仍可换 cookie（放行名单随 nest 剥前缀改为相对
    /// `/pair/pin` 的回归锁定——若名单仍写绝对路径，此测试 403 失败）
    #[tokio::test]
    async fn pair_still_reachable_after_task7_static_appended() {
        let app = router(test_state())
            .route("/m", get(|| async { "pair-page" }))
            .fallback(|| async { "static" });
        let r = app
            .oneshot(req(
                "POST",
                "/m/api/v1/pair/pin",
                None,
                Some(r#"{"pin":"1234"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(
            r.status(),
            200,
            "nest 内层 gate 的相对放行名单必须保住 pair/pin"
        );
        assert!(r.headers().get("set-cookie").is_some());
    }

    // ==== Task 7 静态伺服（router_with_static 真装配；rust-embed debug 态从磁盘直读
    // dist-mobile，零接触真实 ~/.tuvis；产物 hash 文件名动态取，不硬编码） ====

    /// 嵌入清单里 assets/ 下第一个 .js 产物（vite hash 文件名随构建漂移，禁止硬编码）
    fn first_js_asset() -> String {
        MobileAssets::iter()
            .find(|p| p.starts_with("assets/") && p.ends_with(".js"))
            .expect("dist-mobile 缺少 js 产物：先跑 pnpm build:mobile 再 cargo test")
            .to_string()
    }

    fn header<'a>(r: &'a axum::http::Response<Body>, name: &str) -> &'a str {
        r.headers()
            .get(name)
            .expect("响应缺少头")
            .to_str()
            .expect("头值非可见 ASCII")
    }

    /// §C3（B-I6）**变异锚点**：看板入口响应必须带 兔维斯 特征头——tailscale 可达性探针
    /// 据此把「拿到任意 HTTP 响应即算通」收紧为「兔维斯 自己的服务答的」，挡掉 TUN / 透明
    /// 重定向式 MITM 返回的拦截页（自签 CA 在系统信任库时 TLS 仍"成功"）。
    /// 删掉 entry_response 的特征头 → 本测试必红（生产里探针会把一切都判成 Failed）。
    #[tokio::test]
    async fn board_entry_carries_reach_marker_for_tailscale_probe() {
        let app = router_with_static(test_state());
        let r = app.oneshot(req("GET", "/m", None, None)).await.unwrap();
        assert_eq!(r.status(), 200, "前置：入口 HTML 可伺服");
        assert_eq!(
            header(&r, MAM_REACH_HEADER),
            MAM_REACH_VALUE,
            "兔维斯 特征头是 §C3 探针判据的唯一数据源，不得缺失"
        );
    }

    /// 静态伺服全矩阵（含安全不变量回归）：入口 / manifest / 真实产物 MIME /
    /// SPA 回落 / 非 /m 前缀 404 / 未知 API 路径仍 403 / 尾斜杠变体
    #[tokio::test]
    async fn static_serving_matrix() {
        let app = router_with_static(test_state());

        // (1) /m 精确 → 200 入口 HTML（mobile.html 产物：doctype + 移动端标题，防串台桌面 index）
        let r = app
            .clone()
            .oneshot(req("GET", "/m", None, None))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(header(&r, "content-type"), "text/html; charset=utf-8");
        let body = body_string(r).await.to_lowercase();
        assert!(
            body.contains("<!doctype html>"),
            "/m 应返回入口 HTML，实际 {body:?}"
        );
        assert!(
            body.contains("tuvis"),
            "入口应是 mobile.html 产物（含品牌标识；标题文案随品牌批在「兔维斯远程/Tuvis 远程」间合法变动，不写死）"
        );

        // (2) PWA manifest → 200 + application/json（文件名随品牌批 manifest-mam → manifest）
        let r = app
            .clone()
            .oneshot(req("GET", "/m/manifest.json", None, None))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(header(&r, "content-type"), "application/json");

        // (3) 真实 js 产物 → 200 + text/javascript
        let asset = first_js_asset();
        let r = app
            .clone()
            .oneshot(req("GET", &format!("/m/{asset}"), None, None))
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "嵌入产物 {asset} 应可伺服");
        assert_eq!(header(&r, "content-type"), "text/javascript");

        // (4) 未知 /m/* 路径 → SPA 兜底回落入口 HTML（200，非 404）
        let r = app
            .clone()
            .oneshot(req("GET", "/m/nope.js", None, None))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        assert!(body_string(r)
            .await
            .to_lowercase()
            .contains("<!doctype html>"));

        // (5) 非 /m 前缀 → 404（根路径不给静态兜底）
        let r = app
            .clone()
            .oneshot(req("GET", "/favicon.ico", None, None))
            .await
            .unwrap();
        assert_eq!(r.status(), 404);

        // (6) 未知 API 路径 → 仍 403：真装配下 nest 内层 fallback 不被外层静态兜底顶掉
        let r = app
            .clone()
            .oneshot(req("GET", "/m/api/v1/nope", None, None))
            .await
            .unwrap();
        assert_eq!(r.status(), 403, "静态装配不得让未知 API 路径裸奔");

        // (7) 尾斜杠变体 /m/ → 200 入口（route("/m") 不匹配，由 fallback 分流收口）
        let r = app
            .clone()
            .oneshot(req("GET", "/m/", None, None))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        assert!(body_string(r)
            .await
            .to_lowercase()
            .contains("<!doctype html>"));

        // (8) /m/api 裸前缀三变体 → 403（终审修复轮：「所有 /m/api/* 过 gate」的字面
        // 收口）。修复前 /m/api 与 /m/api/ 不匹配任何 route，落到 SPA 回返 200 入口
        // HTML，字面违反安全不变量；/m/api/v2/* 锁定未来版本前缀同样收口
        for uri in ["/m/api", "/m/api/", "/m/api/v2/anything"] {
            let r = app
                .clone()
                .oneshot(req("GET", uri, None, None))
                .await
                .unwrap();
            assert_eq!(r.status(), 403, "{uri} 必须被 /m/api 字面收口为 403");
            assert!(
                !body_string(r)
                    .await
                    .to_lowercase()
                    .contains("<!doctype html>"),
                "{uri} 不得回落静态入口 HTML"
            );
        }
    }

    /// PNG 产物（icon-mobile.png）MIME 与 200：manifest icons 引用的唯一非 js/css 资产
    #[tokio::test]
    async fn png_asset_served_with_image_mime() {
        let app = router_with_static(test_state());
        let png = MobileAssets::iter()
            .find(|p| p.ends_with(".png"))
            .expect("dist-mobile 缺少 png 产物：先跑 pnpm build:mobile");
        let r = app
            .oneshot(req("GET", &format!("/m/{png}"), None, None))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(header(&r, "content-type"), "image/png");
    }

    /// 阻塞源不得卡住 async runtime（评审 Important 2）：注入一个**同步 sleep 300ms** 的
    /// session_source，另起一个轻量 async 任务测量其在扫描期间的调度延迟。
    /// 修复前（handler 里直调同步源）：扫描占满单线程 runtime → 轻量任务被推迟约 300ms；
    /// 修复后（spawn_blocking）：轻量任务应在数十 ms 内完成。
    /// 阈值取宽松的 150ms（CI 抖动容忍），仍能区分 300ms 级的阻塞
    #[tokio::test]
    async fn sessions_scan_does_not_stall_async_runtime() {
        // 重建 state 以注入阻塞源（其余注入缝与 test_state 一致：内存库、预发行 token）
        let state = Arc::new(RemoteState {
            ui_config_source: Box::new(|| None),
            subagent_source: std::collections::HashMap::new(),
            subagent_message_source: std::collections::HashMap::new(),
            target_evidence: no_target_evidence(),
            capability_table: crate::inject::capability::new_table(),
            session_source: Box::new(|| {
                std::thread::sleep(std::time::Duration::from_millis(300));
                crate::session::SessionsResponse {
                    sessions: vec![],
                    total_count: 42,
                    waiting_count: 0,
                }
            }),
            store: crate::remote::pairing::DeviceStore::memory(),
            // M7 Task 5：注入器缝——端点测试不触 flush 路径，用生产占位（Windows 为 Err 桩）
            injector: std::sync::Arc::new(crate::inject::engine::RealInjector),
            // R5 一键 resume spawn 缝（Task 11）：本组测试不触 session-open，注 no-op 桩
            resume_spawner: std::sync::Arc::new(|_: &crate::inject::resume::SpawnSpec| Ok(())),
            // C6：create 缝束缺省 stub（任务簿+发现/pid/工具探测/步距缝），仅
            // session-create 用例就地覆盖
            create_hub: create_hub_stub(),
            archive_source: Box::new(Vec::new),
            archive_delete: std::sync::Arc::new(|_: Option<&str>| 0usize),
            // A1 写入确认缝（M9R Task 5）：测试恒命中（首轮即中，零延迟零等待）
            confirm_probe: std::sync::Arc::new(|_, _, _| true),
            // 丁T3：本组测试的对话框在场探针缺省「无法判定」（None）——控制类注入
            // 照常投递；「在场即拒」的用例就地建 state 覆盖为假体（见 mode_switch_* 用例）
            dialog_probe: std::sync::Arc::new(|_, _| None),
            screen_probe: std::sync::Arc::new(|_, _| None),
            host_source: Box::new(|| serde_json::Value::Null), // 本测试不触 /host
            message_source: Box::new(|_, _, _| Err("测试桩：未注入内容源".to_string())),
            // 本组测试不触 /session-files /file：注入恒空的路径源
            path_source: Box::new(|_, _, _| (Vec::new(), false)),
            watcher_tx: tokio::sync::broadcast::channel(64).0, // M3 Task 5：空事件通道
            board_hidden_ids: Box::new(Vec::new),
            board_hidden_hide: std::sync::Arc::new(|_| 0usize),
            board_hidden_unhide: std::sync::Arc::new(|_| 0usize),
            unread_mark_read: std::sync::Arc::new(|_, _| ()),
            session_close: std::sync::Arc::new(|_| Ok(())),
            // M4 T0a：本组测试不触 SSE 断连，空注册表即可
            sse_registry: Arc::new(SseRegistry::default()),
            // M4 T2（brief 指定）：审批队列固定生成器（code 恒 "0000"——本组测试不触
            // /pair/* 则不被消费）；上限注入常量 3 = 默认上限（零 DAO 接触）
            max_devices_source: Box::new(|| 3),
            // M5 A3 新字段：同 test_state 口径（限速器全新 / PIN "1234" / 真实时钟 / 无隧道域名）
            pin_limiter: std::sync::Mutex::new(crate::remote::pin::PinRateLimiter::new()),
            global_pin_limiter: std::sync::Mutex::new(crate::remote::pin::PinRateLimiter::global()),
            pin_source: Box::new(|| Some("1234".to_string())),
            now_source: Box::new(|| chrono::Utc::now().timestamp_millis()),
            via_hosts_source: Box::new(|| None),
            rate_bucket_channels_source: Box::new(Vec::new),
            home_source: Box::new(|| None),
            // C7：配对计数缝缺省空表（既有用例零影响；配对打标/提示用例就地覆盖）
            pairing_counter: Box::new(Vec::new),
        });
        // 预置有效设备，令 gate 放行（否则不会走到 session_source，测试失去意义）
        let now = chrono::Utc::now().timestamp_millis();
        state.store.with(|c| {
            crate::remote::pairing::persist_device(
                c,
                &crate::remote::pairing::NewDevice {
                    id: "rt".into(),
                    name: String::new(),
                    ua: String::new(),
                    origin_ip: String::new(),
                    via: String::new(),
                    paired_at: now,
                },
            )
            .unwrap();
        });
        let app = router(state);
        let start = std::time::Instant::now();
        let scan = tokio::spawn({
            let app = app.clone();
            async move {
                app.oneshot(req(
                    "GET",
                    "/m/api/v1/sessions",
                    Some("mam_device=rt"),
                    None,
                ))
                .await
                .unwrap()
            }
        });
        // 与扫描并发的最轻任务：若同步扫描占了 runtime，它要等扫描结束才能跑
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        let light_elapsed = start.elapsed();
        let r = scan.await.unwrap();
        assert_eq!(r.status(), 200);
        assert!(body_string(r).await.contains("\"totalCount\":42"));
        assert!(
            light_elapsed < std::time::Duration::from_millis(150),
            "阻塞扫描期间轻量任务被推迟了 {light_elapsed:?}——session_source 未走 spawn_blocking"
        );
    }

    // ==== M3 Task 6：GET /m/api/v1/events（SSE 实时通道，C1 后半） ====
    // 零污染：会话源经注入缝（假 SessionsResponse）、事件源经注入的空 broadcast 通道。
    // SSE 是长连接流式响应：**不能用 body_string（collect 会等流结束、永久挂起）**，
    // 只逐帧读（BodyExt::frame），够断言首帧快照与增量帧即可，读完即 drop（连接关闭）。

    /// 读 SSE 流的下一帧文本（不等待流结束；帧缺失/非数据帧即断言失败）
    async fn next_frame(body: &mut Body) -> String {
        let frame = body
            .frame()
            .await
            .expect("SSE 流在断言帧之前结束")
            .expect("SSE 帧读取失败");
        let bytes = frame
            .into_data()
            .unwrap_or_else(|_| panic!("SSE 应产生数据帧（非 trailers）"));
        String::from_utf8(bytes.to_vec()).expect("SSE 帧应为 UTF-8")
    }

    /// 预置有效设备（SSE 测试专用：复用 state_with_clock 的注入缝，零接触真实设备表）。
    /// M5 A1：ua/ip 按 id 派生——upsert 按指纹去重，全空值会在同库多次预置时撞键合并
    fn persist_device(state: &Arc<RemoteState>, id: &str) {
        let now = chrono::Utc::now().timestamp_millis();
        state.store.with(|c| {
            crate::remote::pairing::persist_device(
                c,
                &crate::remote::pairing::NewDevice {
                    id: id.into(),
                    name: String::new(),
                    ua: format!("ua-{id}"),
                    origin_ip: format!("ip-{id}"),
                    via: String::new(),
                    paired_at: now,
                },
            )
            .unwrap();
        });
    }

    /// gate 覆盖（控制者①）：/events 与其余 /m/api/v1/* 同一门禁——无 cookie 必须 403，
    /// 不得因为「SSE 是长连接」而漏过内层 layer
    #[tokio::test]
    async fn sse_events_is_gated() {
        let state = test_state();
        let app = router(state);
        let r = app
            .oneshot(req("GET", "/m/api/v1/events", None, None))
            .await
            .unwrap();
        assert_eq!(
            r.status(),
            403,
            "/events 必须过 gate（设备失效与未配对同 403 语义）"
        );
    }

    /// SSE 端点主链（控制者③）：认证后 200 + text/event-stream + 首帧 `event: snapshot`
    /// 携带全量会话（注入源 totalCount=7 ⇒ 数据同源），随后 watcher_tx 上的事件以
    /// `event: transition` + camelCase JSON 送达（wire 契约端到端锁定）
    #[tokio::test]
    async fn sse_events_streams_snapshot_then_transitions() {
        let state = test_state();
        persist_device(&state, "ev");
        let app = router(state.clone());
        let r = app
            .oneshot(req("GET", "/m/api/v1/events", Some("mam_device=ev"), None))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(
            header(&r, "content-type"),
            "text/event-stream",
            "SSE 响应必须声明 text/event-stream（浏览器据此走 EventSource 解析）"
        );
        assert_eq!(
            header(&r, "cache-control"),
            "no-cache",
            "SSE 是设备门禁下的私有实时流，禁止中间层缓存"
        );

        let mut body = r.into_body();
        // 首帧：全量快照（event: snapshot + SessionsResponse JSON，数据来自注入源）
        let first = next_frame(&mut body).await;
        assert!(
            first.starts_with("event: snapshot\n"),
            "首帧必须是 snapshot 事件，实际 {first:?}"
        );
        assert!(
            first.contains("\"totalCount\":7"),
            "首帧快照必须直调 session_source（注入源 totalCount=7），实际 {first:?}"
        );

        // 增量帧：watcher_tx 发一条跃迁 → transition 帧 + camelCase 键（前端按
        // ev.sessionId / ev.agentType / ev.projectName 读取；snake_case 会静默 undefined）
        state
            .watcher_tx
            .send(crate::remote::watcher::TransitionEvent {
                session_id: "s1".into(),
                agent_type: "claude".into(),
                from: "idle".into(),
                to: "processing".into(),
                project_name: "proj".into(),
                last_message: Some("hello".into()),
                flap_from_subagent_activity: false,
                ts: 42,
            })
            .unwrap();
        let second = next_frame(&mut body).await;
        assert!(
            second.starts_with("event: transition\n"),
            "增量帧必须是 transition 事件，实际 {second:?}"
        );
        for key in ["\"sessionId\":\"s1\"", "\"to\":\"processing\"", "\"ts\":42"] {
            assert!(
                second.contains(key),
                "transition 帧缺少 {key}（camelCase wire 契约），实际 {second:?}"
            );
        }
        assert!(
            !second.contains("session_id"),
            "transition 帧不得出现 snake_case 键，实际 {second:?}"
        );
    }

    // ---- SseRegistry 单元（M4 T0a）----

    #[test]
    fn sse_registry_register_then_disconnect_device() {
        let reg = SseRegistry::default();
        let (id1, mut rx1) = reg.register("dev-a");
        let (_id2, rx2) = reg.register("dev-a");
        let (_id3, _rx3) = reg.register("dev-b");
        assert!(reg.has("dev-a") && reg.has("dev-b"));
        // 断 dev-a：两条连接全断，dev-b 不受影响
        assert_eq!(reg.disconnect_device("dev-a"), 2);
        assert!(rx1.try_recv().is_ok());
        drop(rx2); // 第二条 receiver 被 send 唤醒后丢弃即可（Sender 已 send）
        assert!(!reg.has("dev-a") && reg.has("dev-b"));
        // 自然断开清理：unregister 幂等
        reg.unregister("dev-a", id1);
        reg.unregister("dev-a", id1); // 不 panic
        assert_eq!(reg.disconnect_all(), 1); // 只剩 dev-b
    }

    #[test]
    fn sse_registry_sender_dropped_when_entry_replaced_by_disconnect() {
        let reg = SseRegistry::default();
        // 编译适配（E0596）：try_recv 需 &mut self，原稿 rx 未加 mut
        let (_id, mut rx) = reg.register("dev");
        reg.disconnect_device("dev");
        // 断连后 Sender 已移除；receiver 侧已收到信号
        // 断言写法适配（clippy::redundant_pattern_matching 门禁）：matches! → is_ok()，语义不变
        assert!(rx.try_recv().is_ok());
    }

    // ---- 吊销即时断流集成（SSE 流随 disconnect 终止）----

    #[tokio::test]
    async fn sse_stream_ends_when_device_disconnected() {
        let state = test_state();
        // 直通配对拿有效 cookie（复用既有 pair 流程的简化版：直接 persist 一个设备）
        state.store.with(|c| {
            let _ = crate::remote::pairing::persist_device(
                c,
                &crate::remote::pairing::NewDevice {
                    id: "dev-sse".into(),
                    name: String::new(),
                    ua: String::new(),
                    origin_ip: String::new(),
                    via: String::new(),
                    // 偏离简报原稿一处（测试语义硬阻断）：原稿 paired_at: 0——persist 以
                    // paired_at 充当 last_seen_at，gate 按真实时钟做滑动 TTL 判定，
                    // 0 必被 403 拒绝。改为当前时刻，语义等价于「刚配对的活跃设备」
                    // （既有 persist_device 测试助手同一取值先例）
                    paired_at: chrono::Utc::now().timestamp_millis(),
                },
            );
        });
        let app = super::router_with_static(state.clone());
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/m/api/v1/events")
                    .header("cookie", "mam_device=dev-sse")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let mut stream = resp.into_body().into_data_stream();
        // 读到首帧（snapshot）证明流已建立
        let first = tokio::time::timeout(std::time::Duration::from_secs(2), stream.next())
            .await
            .expect("首帧超时")
            .unwrap();
        assert!(first.is_ok());
        // 吊销 → 流必须结束（next 返回 None）
        assert_eq!(state.sse_registry.disconnect_device("dev-sse"), 1);
        let end = tokio::time::timeout(std::time::Duration::from_secs(2), stream.next())
            .await
            .expect("断连后流未在 2s 内结束");
        assert!(end.is_none());
    }

    // ==== M5 A4：吊销收窄（gate 级回归） ====

    /// M5 A4 吊销收窄回归：热重启半程（stop revoke=false）后设备 cookie 仍过闸
    /// （「改绑定/改端口热重启不掉线」），显性关闭（revoke=true）后同 cookie 403。
    /// 真实 stop_server 触碰全局 DB / 电源锁 / 隧道进程（零污染红线禁测）——此处以
    /// stop_server_core 注入与生产 stop_server 完全同形的 store/registry 闭包，
    /// 等价锁定「revoke 取值 → 设备有效性」这一收窄语义核心（重启后的监听生效半边
    /// 由 serve/start 既有路径承担，gate 与设备表不受重启影响的判据即本测试）
    #[tokio::test]
    async fn hot_restart_without_revoke_keeps_device_cookie_valid() {
        let state = test_state();
        persist_device(&state, "hn");
        let app = router(state.clone());
        // 初始：cookie 过闸
        let r = app
            .clone()
            .oneshot(req(
                "GET",
                "/m/api/v1/sessions",
                Some("mam_device=hn"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);

        // 热重启半程：停监听不吊销（闭包与生产 stop_server(false) 同形）
        let st_reg = state.clone();
        let st_store = state.clone();
        crate::remote::stop_server_core(
            None,
            false,
            move || {
                st_reg.sse_registry.disconnect_all();
            },
            move || {
                let _ = st_store.store.with(crate::remote::pairing::revoke_all);
            },
            || {},
            || {},
        );
        let r = app
            .clone()
            .oneshot(req(
                "GET",
                "/m/api/v1/sessions",
                Some("mam_device=hn"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(
            r.status(),
            200,
            "revoke=false（热重启）后 cookie 必须仍过闸（设备不掉线）"
        );

        // 显性关闭：吊销 → 同一 cookie 403（收窄前后对照）
        let st_reg = state.clone();
        let st_store = state.clone();
        crate::remote::stop_server_core(
            None,
            true,
            move || {
                st_reg.sse_registry.disconnect_all();
            },
            move || {
                let _ = st_store.store.with(crate::remote::pairing::revoke_all);
            },
            || {},
            || {},
        );
        let r = app
            .oneshot(req(
                "GET",
                "/m/api/v1/sessions",
                Some("mam_device=hn"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 403, "revoke=true（显性关闭）后设备全吊销");
    }

    // ==== M3 Task 1：GET /m/api/v1/host（P8a/P8b 页头数据源） ====
    // 零污染：host 载荷经 host_source 注入缝供给（假 json），不触 settings DAO / 全局 DB。

    /// /host 端点矩阵：无 cookie 403（与 sessions 同一 gate，设备失效语义一致）；
    /// 有效设备 200 返回注入载荷 + Cache-Control: no-store（host 同属门禁下私有数据）
    #[tokio::test]
    async fn host_endpoint_is_gated_and_returns_injected_payload() {
        // 重建 state：host_source 注入假载荷（与 sessions_scan_* 重建 state 的先例一致）
        let state = Arc::new(RemoteState {
            ui_config_source: Box::new(|| None),
            subagent_source: std::collections::HashMap::new(),
            subagent_message_source: std::collections::HashMap::new(),
            target_evidence: no_target_evidence(),
            capability_table: crate::inject::capability::new_table(),
            session_source: Box::new(|| crate::session::SessionsResponse {
                sessions: vec![],
                total_count: 0,
                waiting_count: 0,
            }),
            store: crate::remote::pairing::DeviceStore::memory(),
            // M7 Task 5：注入器缝——端点测试不触 flush 路径，用生产占位（Windows 为 Err 桩）
            injector: std::sync::Arc::new(crate::inject::engine::RealInjector),
            // R5 一键 resume spawn 缝（Task 11）：本组测试不触 session-open，注 no-op 桩
            resume_spawner: std::sync::Arc::new(|_: &crate::inject::resume::SpawnSpec| Ok(())),
            // C6：create 缝束缺省 stub（任务簿+发现/pid/工具探测/步距缝），仅
            // session-create 用例就地覆盖
            create_hub: create_hub_stub(),
            archive_source: Box::new(Vec::new),
            archive_delete: std::sync::Arc::new(|_: Option<&str>| 0usize),
            // A1 写入确认缝（M9R Task 5）：测试恒命中（首轮即中，零延迟零等待）
            confirm_probe: std::sync::Arc::new(|_, _, _| true),
            // 丁T3：本组测试的对话框在场探针缺省「无法判定」（None）——控制类注入
            // 照常投递；「在场即拒」的用例就地建 state 覆盖为假体（见 mode_switch_* 用例）
            dialog_probe: std::sync::Arc::new(|_, _| None),
            screen_probe: std::sync::Arc::new(|_, _| None),
            host_source: Box::new(|| {
                serde_json::json!({
                    "host": { "name": "jarvis-win", "platform": "windows", "version": "9.9.9-test" },
                    "enabledTools": ["claude", "zcode"]
                })
            }),
            message_source: Box::new(|_, _, _| Err("测试桩：未注入内容源".to_string())),
            // 本组测试不触 /session-files /file：注入恒空的路径源
            path_source: Box::new(|_, _, _| (Vec::new(), false)),
            watcher_tx: tokio::sync::broadcast::channel(64).0, // M3 Task 5：空事件通道
            board_hidden_ids: Box::new(Vec::new),
            board_hidden_hide: std::sync::Arc::new(|_| 0usize),
            board_hidden_unhide: std::sync::Arc::new(|_| 0usize),
            unread_mark_read: std::sync::Arc::new(|_, _| ()),
            session_close: std::sync::Arc::new(|_| Ok(())),
            // M4 T0a：本组测试不触 SSE 断连，空注册表即可
            sse_registry: Arc::new(SseRegistry::default()),
            // M4 T2（brief 指定）：审批队列固定生成器（code 恒 "0000"——本组测试不触
            // /pair/* 则不被消费）；上限注入常量 3 = 默认上限（零 DAO 接触）
            max_devices_source: Box::new(|| 3),
            // M5 A3 新字段：同 test_state 口径（限速器全新 / PIN "1234" / 真实时钟 / 无隧道域名）
            pin_limiter: std::sync::Mutex::new(crate::remote::pin::PinRateLimiter::new()),
            global_pin_limiter: std::sync::Mutex::new(crate::remote::pin::PinRateLimiter::global()),
            pin_source: Box::new(|| Some("1234".to_string())),
            now_source: Box::new(|| chrono::Utc::now().timestamp_millis()),
            via_hosts_source: Box::new(|| None),
            rate_bucket_channels_source: Box::new(Vec::new),
            home_source: Box::new(|| None),
            // C7：配对计数缝缺省空表（既有用例零影响；配对打标/提示用例就地覆盖）
            pairing_counter: Box::new(Vec::new),
        });
        let app = router(state.clone());
        let now = chrono::Utc::now().timestamp_millis();
        state.store.with(|c| {
            crate::remote::pairing::persist_device(
                c,
                &crate::remote::pairing::NewDevice {
                    id: "hd".into(),
                    name: String::new(),
                    ua: String::new(),
                    origin_ip: String::new(),
                    via: String::new(),
                    paired_at: now,
                },
            )
            .unwrap();
        });

        // (1) 无 cookie → 403（gate 全量覆盖新端点，与 sessions 语义一致）
        let r = app
            .clone()
            .oneshot(req("GET", "/m/api/v1/host", None, None))
            .await
            .unwrap();
        assert_eq!(
            r.status(),
            403,
            "/host 必须过 gate（设备失效同 sessions 的 403 语义）"
        );

        // (2) 有效设备 → 200 + 注入载荷原样透传
        let r = app
            .clone()
            .oneshot(req("GET", "/m/api/v1/host", Some("mam_device=hd"), None))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let body = body_string(r).await;
        assert!(
            body.contains("\"name\":\"jarvis-win\"") && body.contains("\"version\":\"9.9.9-test\""),
            "host 载荷应原样透传，实际 {body}"
        );
        assert!(
            body.contains("\"enabledTools\""),
            "enabledTools 数据源随载荷返回"
        );
    }

    /// /session-messages（M3 Task 7）端点矩阵：gate 403 → 缺参 400 → 注入源 200
    /// （载荷 camelCase 且 no-store）→ 读取失败 404（错误细节不外泄）。
    /// 零污染：message_source 注入假源，不触任何真实工具数据目录
    #[tokio::test]
    async fn session_messages_endpoint_is_gated_and_shaped() {
        let captured: Arc<std::sync::Mutex<Vec<(String, String, usize)>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let cap = captured.clone();
        let state = Arc::new(RemoteState {
            ui_config_source: Box::new(|| None),
            subagent_source: std::collections::HashMap::new(),
            subagent_message_source: std::collections::HashMap::new(),
            target_evidence: no_target_evidence(),
            capability_table: crate::inject::capability::new_table(),
            session_source: Box::new(|| crate::session::SessionsResponse {
                sessions: vec![],
                total_count: 0,
                waiting_count: 0,
            }),
            store: crate::remote::pairing::DeviceStore::memory(),
            // M7 Task 5：注入器缝——端点测试不触 flush 路径，用生产占位（Windows 为 Err 桩）
            injector: std::sync::Arc::new(crate::inject::engine::RealInjector),
            // R5 一键 resume spawn 缝（Task 11）：本组测试不触 session-open，注 no-op 桩
            resume_spawner: std::sync::Arc::new(|_: &crate::inject::resume::SpawnSpec| Ok(())),
            // C6：create 缝束缺省 stub（任务簿+发现/pid/工具探测/步距缝），仅
            // session-create 用例就地覆盖
            create_hub: create_hub_stub(),
            archive_source: Box::new(Vec::new),
            archive_delete: std::sync::Arc::new(|_: Option<&str>| 0usize),
            // A1 写入确认缝（M9R Task 5）：测试恒命中（首轮即中，零延迟零等待）
            confirm_probe: std::sync::Arc::new(|_, _, _| true),
            // 丁T3：本组测试的对话框在场探针缺省「无法判定」（None）——控制类注入
            // 照常投递；「在场即拒」的用例就地建 state 覆盖为假体（见 mode_switch_* 用例）
            dialog_probe: std::sync::Arc::new(|_, _| None),
            screen_probe: std::sync::Arc::new(|_, _| None),
            host_source: Box::new(|| serde_json::Value::Null),
            message_source: Box::new(move |agent: &str, sid: &str, limit: usize| {
                cap.lock()
                    .unwrap()
                    .push((agent.to_string(), sid.to_string(), limit));
                if sid == "sess_hit" {
                    Ok(crate::remote::content::MessagesPage {
                        messages: vec![
                            crate::remote::content::SessionMessage {
                                seq: 0,
                                role: "user".into(),
                                kind: "user".into(),
                                content: "你好".into(),
                                ts: Some(1000),
                                tool_name: None,
                                tool_args: None,
                                collapsed: false,
                            },
                            crate::remote::content::SessionMessage {
                                seq: 1,
                                role: "assistant".into(),
                                kind: "tool-call".into(),
                                content: "调用 Bash".into(),
                                ts: Some(1001),
                                tool_name: Some("Bash".into()),
                                tool_args: Some(r#"{"cmd":"ls"}"#.into()),
                                collapsed: true,
                            },
                        ],
                        truncated: false,
                    })
                } else {
                    Err("内部路径细节不应出现在响应里".to_string())
                }
            }),
            // 本测试不触 /session-files：注入恒空的路径源
            path_source: Box::new(|_, _, _| (Vec::new(), false)),
            watcher_tx: tokio::sync::broadcast::channel(64).0,
            board_hidden_ids: Box::new(Vec::new),
            board_hidden_hide: std::sync::Arc::new(|_| 0usize),
            board_hidden_unhide: std::sync::Arc::new(|_| 0usize),
            unread_mark_read: std::sync::Arc::new(|_, _| ()),
            session_close: std::sync::Arc::new(|_| Ok(())),
            // M4 T0a：本组测试不触 SSE 断连，空注册表即可
            sse_registry: Arc::new(SseRegistry::default()),
            // M4 T2（brief 指定）：审批队列固定生成器（code 恒 "0000"——本组测试不触
            // /pair/* 则不被消费）；上限注入常量 3 = 默认上限（零 DAO 接触）
            max_devices_source: Box::new(|| 3),
            // M5 A3 新字段：同 test_state 口径（限速器全新 / PIN "1234" / 真实时钟 / 无隧道域名）
            pin_limiter: std::sync::Mutex::new(crate::remote::pin::PinRateLimiter::new()),
            global_pin_limiter: std::sync::Mutex::new(crate::remote::pin::PinRateLimiter::global()),
            pin_source: Box::new(|| Some("1234".to_string())),
            now_source: Box::new(|| chrono::Utc::now().timestamp_millis()),
            via_hosts_source: Box::new(|| None),
            rate_bucket_channels_source: Box::new(Vec::new),
            home_source: Box::new(|| None),
            // C7：配对计数缝缺省空表（既有用例零影响；配对打标/提示用例就地覆盖）
            pairing_counter: Box::new(Vec::new),
        });
        let app = router(state.clone());
        let now = chrono::Utc::now().timestamp_millis();
        state.store.with(|c| {
            crate::remote::pairing::persist_device(
                c,
                &crate::remote::pairing::NewDevice {
                    id: "cm".into(),
                    name: String::new(),
                    ua: String::new(),
                    origin_ip: String::new(),
                    via: String::new(),
                    paired_at: now,
                },
            )
            .unwrap();
        });

        // (1) 无 cookie → 403（nest 内层 gate 结构性覆盖新路由）
        let r = app
            .clone()
            .oneshot(req(
                "GET",
                "/m/api/v1/session-messages?agent_type=zcode&session_id=sess_hit",
                None,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 403, "新端点必须过 gate");

        // (2) 缺 agent_type / 缺 session_id / 空白 session_id → 400
        for uri in [
            "/m/api/v1/session-messages?session_id=sess_hit",
            "/m/api/v1/session-messages?agent_type=zcode",
            "/m/api/v1/session-messages?agent_type=zcode&session_id=%20%20",
        ] {
            let r = app
                .clone()
                .oneshot(req("GET", uri, Some("mam_device=cm"), None))
                .await
                .unwrap();
            assert_eq!(r.status(), 400, "缺参必须 400：{uri}");
        }

        // (3) 命中 → 200 + camelCase 载荷 + no-store；参数正确传入注入源（limit 默认 200）
        let r = app
            .clone()
            .oneshot(req(
                "GET",
                "/m/api/v1/session-messages?agent_type=zcode&session_id=sess_hit",
                Some("mam_device=cm"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(
            r.headers()
                .get("cache-control")
                .and_then(|v| v.to_str().ok()),
            Some("no-store"),
            "会话正文是门禁下私有数据，禁止中间层缓存"
        );
        let body = body_string(r).await;
        assert!(
            body.contains("\"messages\"") && body.contains("\"toolName\":\"Bash\""),
            "载荷必须 camelCase（toolName/toolArgs/collapsed），实际 {body}"
        );
        // Bug 1 修复（M3 验收）：载荷携带 truncated（头部截断标记），移动端据此决定
        // 是否提供「加载更早消息」
        assert!(
            body.contains("\"truncated\":false"),
            "载荷必须含 truncated 字段，实际 {body}"
        );
        assert!(body.contains("\"toolArgs\"") && body.contains("\"collapsed\":true"));
        assert_eq!(
            captured.lock().unwrap().first().cloned(),
            Some(("zcode".to_string(), "sess_hit".to_string(), 200)),
            "handler 应把 agent_type/session_id/默认 limit 传给内容源"
        );

        // (4) 读取失败 → 404 空语义；limit 查询参数透传
        let r = app
            .clone()
            .oneshot(req(
                "GET",
                "/m/api/v1/session-messages?agent_type=dsh&session_id=sess_miss&limit=50",
                Some("mam_device=cm"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 404, "读取失败必须 404");
        let body = body_string(r).await;
        assert!(!body.contains("内部路径细节"), "错误细节只进日志不外泄");
        assert_eq!(
            captured.lock().unwrap().last().cloned(),
            Some(("dsh".to_string(), "sess_miss".to_string(), 50)),
            "limit 查询参数应透传"
        );
    }

    // ==== M3 Task 8：GET /m/api/v1/file + /session-files（安全文件读取与路径提取） ====
    // 零污染：session_source 注入 tempdir 项目目录的会话，被读文件均为 tempdir 内
    // 现造文件；path_source 注入固定路径表——不触任何真实数据目录

    /// file / session-files 端点矩阵：gate 403 → 缺参 400 → 会话不存在 404 →
    /// cwd 内文本 200（JSON 载荷 + no-store）→ 图片 200（二进制 + Content-Type）→
    /// 越界 403 → 超限 403（与越界不可区分，探测面最小化，进度台账 #12）
    #[tokio::test]
    async fn file_endpoints_are_gated_and_safe() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().to_str().unwrap().to_string();
        std::fs::write(tmp.path().join("hello.txt"), "hello mam").unwrap();
        std::fs::write(tmp.path().join("pic.png"), [0x89u8, b'P', b'N', b'G']).unwrap();
        std::fs::write(tmp.path().join("big.txt"), vec![b'a'; 500 * 1024 + 1]).unwrap();
        let session = crate::session::Session {
            id: "sess_file".into(),
            agent_type: crate::session::AgentType::Claude,
            project_name: "proj".into(),
            project_path: cwd.clone(),
            title: None,
            git_branch: None,
            github_url: None,
            status: crate::session::SessionStatus::Idle,
            last_message: None,
            last_message_role: None,
            last_message_subagent_report: false,
            flap_from_subagent_activity: false,
            last_activity_at: "2026-09-15T00:00:00Z".into(),
            pid: 1,
            cpu_usage: 0.0,
            active_subagent_count: 0,
            form: crate::session::ProcessForm::Cli,
            jump_supported: false,
            unread: false,
        };
        let state = Arc::new(RemoteState {
            ui_config_source: Box::new(|| None),
            subagent_source: std::collections::HashMap::new(),
            subagent_message_source: std::collections::HashMap::new(),
            target_evidence: no_target_evidence(),
            capability_table: crate::inject::capability::new_table(),
            session_source: Box::new(move || crate::session::SessionsResponse {
                sessions: vec![session.clone()],
                total_count: 1,
                waiting_count: 0,
            }),
            store: crate::remote::pairing::DeviceStore::memory(),
            // M7 Task 5：注入器缝——端点测试不触 flush 路径，用生产占位（Windows 为 Err 桩）
            injector: std::sync::Arc::new(crate::inject::engine::RealInjector),
            // R5 一键 resume spawn 缝（Task 11）：本组测试不触 session-open，注 no-op 桩
            resume_spawner: std::sync::Arc::new(|_: &crate::inject::resume::SpawnSpec| Ok(())),
            // C6：create 缝束缺省 stub（任务簿+发现/pid/工具探测/步距缝），仅
            // session-create 用例就地覆盖
            create_hub: create_hub_stub(),
            archive_source: Box::new(Vec::new),
            archive_delete: std::sync::Arc::new(|_: Option<&str>| 0usize),
            // A1 写入确认缝（M9R Task 5）：测试恒命中（首轮即中，零延迟零等待）
            confirm_probe: std::sync::Arc::new(|_, _, _| true),
            // 丁T3：本组测试的对话框在场探针缺省「无法判定」（None）——控制类注入
            // 照常投递；「在场即拒」的用例就地建 state 覆盖为假体（见 mode_switch_* 用例）
            dialog_probe: std::sync::Arc::new(|_, _| None),
            screen_probe: std::sync::Arc::new(|_, _| None),
            host_source: Box::new(|| serde_json::Value::Null),
            message_source: Box::new(|_, _, _| Err("测试桩：未注入内容源".to_string())),
            path_source: Box::new(|_, _, _| {
                (
                    vec![crate::remote::files::FileEntry {
                        path: "/absent/proj/src/main.rs".to_string(),
                        last_seq: 1,
                        last_ts: None,
                        hits: 1,
                        modified: false,
                        origin: crate::remote::files::FileOrigin::ToolRead,
                    }],
                    false,
                )
            }),
            watcher_tx: tokio::sync::broadcast::channel(64).0,
            board_hidden_ids: Box::new(Vec::new),
            board_hidden_hide: std::sync::Arc::new(|_| 0usize),
            board_hidden_unhide: std::sync::Arc::new(|_| 0usize),
            unread_mark_read: std::sync::Arc::new(|_, _| ()),
            session_close: std::sync::Arc::new(|_| Ok(())),
            // M4 T0a：本组测试不触 SSE 断连，空注册表即可
            sse_registry: Arc::new(SseRegistry::default()),
            // M4 T2（brief 指定）：审批队列固定生成器（code 恒 "0000"——本组测试不触
            // /pair/* 则不被消费）；上限注入常量 3 = 默认上限（零 DAO 接触）
            max_devices_source: Box::new(|| 3),
            // M5 A3 新字段：同 test_state 口径（限速器全新 / PIN "1234" / 真实时钟 / 无隧道域名）
            pin_limiter: std::sync::Mutex::new(crate::remote::pin::PinRateLimiter::new()),
            global_pin_limiter: std::sync::Mutex::new(crate::remote::pin::PinRateLimiter::global()),
            pin_source: Box::new(|| Some("1234".to_string())),
            now_source: Box::new(|| chrono::Utc::now().timestamp_millis()),
            via_hosts_source: Box::new(|| None),
            rate_bucket_channels_source: Box::new(Vec::new),
            home_source: Box::new(|| None),
            // C7：配对计数缝缺省空表（既有用例零影响；配对打标/提示用例就地覆盖）
            pairing_counter: Box::new(Vec::new),
        });
        let app = router(state.clone());
        persist_device(&state, "fe");

        // (1) gate：无 cookie 访问 file / session-files → 403（nest 内层 gate 结构性覆盖）
        for uri in [
            "/m/api/v1/file?session_id=sess_file&path=hello.txt",
            "/m/api/v1/session-files?agent_type=claude&session_id=sess_file",
        ] {
            let r = app
                .clone()
                .oneshot(req("GET", uri, None, None))
                .await
                .unwrap();
            assert_eq!(r.status(), 403, "新端点必须过 gate：{uri}");
        }

        // (2) 缺参 / 空白参数 → 400
        for uri in [
            "/m/api/v1/file?path=hello.txt",
            "/m/api/v1/file?session_id=sess_file",
            "/m/api/v1/file?session_id=%20&path=hello.txt",
        ] {
            let r = app
                .clone()
                .oneshot(req("GET", uri, Some("mam_device=fe"), None))
                .await
                .unwrap();
            assert_eq!(r.status(), 400, "缺参必须 400：{uri}");
        }

        // (3) 会话不在快照中 → 404
        let r = app
            .clone()
            .oneshot(req(
                "GET",
                "/m/api/v1/file?session_id=sess_miss&path=hello.txt",
                Some("mam_device=fe"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 404, "session_id 不在快照必须 404");

        // (4) cwd 内文本 → 200 JSON {content, mime, size} + no-store（相对路径形态）
        let r = app
            .clone()
            .oneshot(req(
                "GET",
                "/m/api/v1/file?session_id=sess_file&path=hello.txt",
                Some("mam_device=fe"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(
            r.headers()
                .get("cache-control")
                .and_then(|v| v.to_str().ok()),
            Some("no-store"),
            "门禁下的私有文件内容禁止中间层缓存"
        );
        let body = body_string(r).await;
        assert!(
            body.contains("\"content\":\"hello mam\"")
                && body.contains("\"mime\":\"text/plain\"")
                && body.contains("\"size\":9"),
            "文本载荷形状 content/mime/size 三键，实际 {body}"
        );

        // (5) 图片 → 200 二进制 + Content-Type: image/png（绝对路径形态 + 含空格文件名
        //     的 URL 编码变体一并覆盖查询串解码）
        std::fs::write(tmp.path().join("with space.png"), [0x89u8, b'P']).unwrap();
        let r = app
            .clone()
            .oneshot(req(
                "GET",
                &format!(
                    "/m/api/v1/file?session_id=sess_file&path={}",
                    uri_encode(&tmp.path().join("with space.png").to_string_lossy())
                ),
                Some("mam_device=fe"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "含空格的绝对路径（URL 编码）应可读取");
        assert_eq!(header(&r, "content-type"), "image/png");
        // 终审 Important 2：所有图片二进制响应必须带嗅探防护双头——SVG 以顶层文档
        // 加载时可执行内嵌脚本（同源脚本可 fetch 会话数据，SameSite=Lax 不防同源），
        // CSP 断脚本/取资源 + nosniff 防 MIME 嗅探误判（png 与 svg 同一分支，双头断言一致）
        assert_eq!(header(&r, "content-security-policy"), "default-src 'none'");
        assert_eq!(header(&r, "x-content-type-options"), "nosniff");
        let bytes = r.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(bytes.as_ref(), &[0x89, b'P'], "图片走二进制直传");

        // (5b) SVG 直出（终审 Important 2 的直接场景）：image/svg+xml 同样带
        //      CSP + nosniff 双头——内容可含脚本的图片 mime 是防护重点
        std::fs::write(
            tmp.path().join("icon.svg"),
            "<svg xmlns=\"http://www.w3.org/2000/svg\"><script>alert(1)</script></svg>",
        )
        .unwrap();
        let r = app
            .clone()
            .oneshot(req(
                "GET",
                "/m/api/v1/file?session_id=sess_file&path=icon.svg",
                Some("mam_device=fe"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "cwd 内 svg 应可读取");
        assert_eq!(header(&r, "content-type"), "image/svg+xml");
        assert_eq!(
            header(&r, "content-security-policy"),
            "default-src 'none'",
            "SVG 直出必须断一切脚本与子资源"
        );
        assert_eq!(header(&r, "x-content-type-options"), "nosniff");

        // (6) 不存在（两平台都不存在的人造路径——勿用 /etc/passwd 之类真实系统
        // 路径：Linux CI 上它真实存在，全盘放开语义下 200 是正确行为而非失败）
        // → 403 + 原因码 not_found（M5 P2-a：原因写在报错处，仅已过闸设备可见）
        let r = app
            .clone()
            .oneshot(req(
                "GET",
                "/m/api/v1/file?session_id=sess_file&path=/no-such-mam-fixture/nope.txt",
                Some("mam_device=fe"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 403, "不存在必须 403");
        let b = body_string(r).await;
        assert!(b.contains("not_found"), "403 体必须带原因码 not_found: {b}");

        // (7) 超限 → 403 + 原因码 too_large（与 (6) 可区分——M5 P2-a 裁决：
        // 原因写在报错处，替代旧「空体不可区分」口径）
        let r = app
            .clone()
            .oneshot(req(
                "GET",
                "/m/api/v1/file?session_id=sess_file&path=big.txt",
                Some("mam_device=fe"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 403, "超限必须 403");
        let b = body_string(r).await;
        assert!(b.contains("too_large"), "403 体必须带原因码 too_large: {b}");

        // (8) /session-files：缺参 400 → 命中 200 {files:[...]} + no-store
        let r = app
            .clone()
            .oneshot(req(
                "GET",
                "/m/api/v1/session-files?session_id=sess_file",
                Some("mam_device=fe"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 400);
        let r = app
            .clone()
            .oneshot(req(
                "GET",
                "/m/api/v1/session-files?agent_type=claude&session_id=sess_file",
                Some("mam_device=fe"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(
            r.headers()
                .get("cache-control")
                .and_then(|v| v.to_str().ok()),
            Some("no-store"),
            "文件路径表是门禁下私有数据，禁止中间层缓存"
        );
        // M3+ 富化形状：结构化条目（camelCase）+ truncated
        let files_body = body_string(r).await;
        assert!(
            files_body.contains("\"path\":\"/absent/proj/src/main.rs\"")
                && files_body.contains("\"lastSeq\":1")
                && files_body.contains("\"hits\":1")
                && files_body.contains("\"truncated\":false"),
            "session-files 应透传注入路径源的结构化条目，实际 {files_body}"
        );
    }

    /// 敏感黑名单端到端回归锁（M5 P2-a 追记）：/file 端点必须**消费** home_source。
    /// 3d22e2e 曾把端点调用改传 None，使主目录内黑名单在生产链路上整段失效——
    /// 当时单元测试全绿（read_file_safe 每个用例都显式传 Some(home)，无从暴露
    /// 接线缺失），本锁补上这一层：探针 = 调用标记（照
    /// session_messages_endpoint_is_gated_and_shaped 的捕获注入先例），端点漏接
    /// home_source 时 hit 恒 false，断言直接变红。
    #[tokio::test]
    async fn file_endpoint_rejects_sensitive_paths_inside_home() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let proj = home.join("Desktop").join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        let ok_file = proj.join("ok.txt");
        std::fs::write(&ok_file, "fine").unwrap();
        let secret = home.join(".ssh").join("id_rsa");
        std::fs::create_dir_all(secret.parent().unwrap()).unwrap();
        std::fs::write(&secret, "PRIVATE").unwrap();
        let home_s = home.to_str().unwrap().to_string();

        let session = crate::session::Session {
            id: "sess_home".into(),
            agent_type: crate::session::AgentType::Claude,
            project_name: "proj".into(),
            project_path: proj.to_str().unwrap().to_string(),
            title: None,
            git_branch: None,
            github_url: None,
            status: crate::session::SessionStatus::Idle,
            last_message: None,
            last_message_role: None,
            last_message_subagent_report: false,
            flap_from_subagent_activity: false,
            last_activity_at: "2026-09-15T00:00:00Z".into(),
            pid: 1,
            cpu_usage: 0.0,
            active_subagent_count: 0,
            form: crate::session::ProcessForm::Cli,
            jump_supported: false,
            unread: false,
        };
        let hit = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let h = hit.clone();
        let state = Arc::new(RemoteState {
            ui_config_source: Box::new(|| None),
            subagent_source: std::collections::HashMap::new(),
            subagent_message_source: std::collections::HashMap::new(),
            target_evidence: no_target_evidence(),
            capability_table: crate::inject::capability::new_table(),
            session_source: Box::new(move || crate::session::SessionsResponse {
                sessions: vec![session.clone()],
                total_count: 1,
                waiting_count: 0,
            }),
            store: crate::remote::pairing::DeviceStore::memory(),
            injector: std::sync::Arc::new(crate::inject::engine::RealInjector),
            // R5 一键 resume spawn 缝（Task 11）：本组测试不触 session-open，注 no-op 桩
            resume_spawner: std::sync::Arc::new(|_: &crate::inject::resume::SpawnSpec| Ok(())),
            // C6：create 缝束缺省 stub（任务簿+发现/pid/工具探测/步距缝），仅
            // session-create 用例就地覆盖
            create_hub: create_hub_stub(),
            archive_source: Box::new(Vec::new),
            archive_delete: std::sync::Arc::new(|_: Option<&str>| 0usize),
            // A1 写入确认缝（M9R Task 5）：测试恒命中（首轮即中，零延迟零等待）
            confirm_probe: std::sync::Arc::new(|_, _, _| true),
            // 丁T3：本组测试的对话框在场探针缺省「无法判定」（None）——控制类注入
            // 照常投递；「在场即拒」的用例就地建 state 覆盖为假体（见 mode_switch_* 用例）
            dialog_probe: std::sync::Arc::new(|_, _| None),
            screen_probe: std::sync::Arc::new(|_, _| None),
            host_source: Box::new(|| serde_json::Value::Null),
            message_source: Box::new(|_, _, _| Err("测试桩：未注入内容源".to_string())),
            path_source: Box::new(|_, _, _| (Vec::new(), false)),
            watcher_tx: tokio::sync::broadcast::channel(64).0,
            board_hidden_ids: Box::new(Vec::new),
            board_hidden_hide: std::sync::Arc::new(|_| 0usize),
            board_hidden_unhide: std::sync::Arc::new(|_| 0usize),
            unread_mark_read: std::sync::Arc::new(|_, _| ()),
            session_close: std::sync::Arc::new(|_| Ok(())),
            sse_registry: Arc::new(SseRegistry::default()),
            max_devices_source: Box::new(|| 3),
            pin_limiter: std::sync::Mutex::new(crate::remote::pin::PinRateLimiter::new()),
            global_pin_limiter: std::sync::Mutex::new(crate::remote::pin::PinRateLimiter::global()),
            pin_source: Box::new(|| Some("1234".to_string())),
            now_source: Box::new(|| chrono::Utc::now().timestamp_millis()),
            via_hosts_source: Box::new(|| None),
            rate_bucket_channels_source: Box::new(Vec::new),
            // 探针 + 注入 tmpdir home（零接触真实主目录）
            home_source: Box::new(move || {
                h.store(true, std::sync::atomic::Ordering::SeqCst);
                Some(home_s.clone())
            }),
            // C7：配对计数缝缺省空表（本用例不触配对打标）
            pairing_counter: Box::new(Vec::new),
        });
        let app = router(state.clone());
        persist_device(&state, "fe");

        // (1) 主目录内凭据 → 403 + 原因码 sensitive
        let r = app
            .clone()
            .oneshot(req(
                "GET",
                &format!(
                    "/m/api/v1/file?session_id=sess_home&path={}",
                    uri_encode(&secret.to_string_lossy())
                ),
                Some("mam_device=fe"),
                None,
            ))
            .await
            .unwrap();
        assert!(
            hit.load(std::sync::atomic::Ordering::SeqCst),
            "/file 必须消费 home_source（漏接 = 生产黑名单失效，3d22e2e 回归形态）"
        );
        assert_eq!(r.status(), 403, "主目录内凭据必须拒绝");
        let b = body_string(r).await;
        assert!(b.contains("sensitive"), "403 体必须带原因码 sensitive: {b}");

        // (2) 主目录内普通文件 → 200（黑名单不误伤；同时证明 (1) 不是全盘拒绝）
        let r = app
            .clone()
            .oneshot(req(
                "GET",
                &format!(
                    "/m/api/v1/file?session_id=sess_home&path={}",
                    uri_encode(&ok_file.to_string_lossy())
                ),
                Some("mam_device=fe"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "主目录内普通文件必须放行");

        // (3) **区分度断言**：主目录**外**含敏感段名的路径必须放行——
        // 这正是「基准被真正消费」的判据：若端点传 None（3d22e2e 形态），
        // read_file_safe 会走全段保守分支把 .ssh 段一并拒掉（403）；
        // 只有消费了 home 基准、且裁决边界（主目录外不设路径级防线）未被扩大，
        // 才会放行。本条同时是「T1 fail-closed 不得吞掉接线错误」的防回归锁。
        let outside_secret = tmp.path().join("outside").join(".ssh").join("id_rsa");
        std::fs::create_dir_all(outside_secret.parent().unwrap()).unwrap();
        std::fs::write(&outside_secret, "PRIVATE").unwrap();
        let r = app
            .clone()
            .oneshot(req(
                "GET",
                &format!(
                    "/m/api/v1/file?session_id=sess_home&path={}",
                    uri_encode(&outside_secret.to_string_lossy())
                ),
                Some("mam_device=fe"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(
            r.status(),
            200,
            "主目录外路径不设路径级防线（2026-09-18 裁决）——基准被消费时必须放行"
        );
    }

    /// 查询参数值的最小 URL 编码（测试助手：空格 → %20；其余字符测试数据不含）
    fn uri_encode(s: &str) -> String {
        s.replace(' ', "%20")
    }

    /// 从 Set-Cookie 取设备 id（`mam_device=<id>; Path=/m; ...`）
    fn cookie_device_id(resp: &axum::http::Response<Body>) -> String {
        let v = resp
            .headers()
            .get("set-cookie")
            .expect("应携带 Set-Cookie")
            .to_str()
            .unwrap()
            .to_string();
        v.split(';')
            .next()
            .unwrap()
            .strip_prefix("mam_device=")
            .unwrap()
            .to_string()
    }

    // ==== M5 A3：/pair/pin 端点（密码制配对）+ 零豁免矩阵 ====
    // 安全是本任务的存在意义：穿透测试锁定「回环/隧道流量都借不到放行豁免」——
    // 2026-10-06 起鉴权只有一条规则（有效设备凭据），见 gate 模块头。

    /// A3 专用 state：PIN 源 / 设备上限 / via 域名（tunnel_error=true → None 哨兵）/
    /// **限速声明表**（与生产同构：只声明 quick/named 两路 CF 通道）/ 可推进时钟全注入
    /// （零 DB 零真实隧道）。ts_hosts = tailscale 机器域名名单（§C1，**纯装饰 via 用**；
    /// **不进限速声明表**——Funnel 无 Cloudflare 边缘，同名头可伪造，见
    /// `gate::cloudflare_rate_channels`）。
    /// 返回时钟句柄供限速到期测试推进（state_with_clock 同目的，零 sleep）
    fn a3_state(
        pin: Option<&str>,
        max_devices: usize,
        quick_hosts: &[&str],
        named_hosts: &[&str],
        ts_hosts: &[&str],
        tunnel_error: bool,
    ) -> (Arc<RemoteState>, Arc<std::sync::atomic::AtomicI64>) {
        let t = Arc::new(std::sync::atomic::AtomicI64::new(1_000_000));
        let now = t.clone();
        let quick: Vec<String> = quick_hosts.iter().map(|s| s.to_string()).collect();
        let named: Vec<String> = named_hosts.iter().map(|s| s.to_string()).collect();
        let ts: Vec<String> = ts_hosts.iter().map(|s| s.to_string()).collect();
        let pin_owned: Option<String> = pin.map(|s| s.to_string());
        // 限速声明源（§G5 / 评审 A-I2）：与生产**同构**——只声明 Cloudflare 两路
        // （权威头由 gate::cloudflare_rate_channels 钉死）；ts 名单刻意不进表，
        // 这样"未声明通道 → 全局桶"在端点级也可断言。tunnel_error ⇒ 零声明（fail-closed）。
        // 顺序在 via_source 之前：后者 move 掉 quick/named，本闭包先克隆一份
        let rate_channels: Box<crate::remote::gate::RateBucketChannelsSource> = if tunnel_error {
            Box::new(Vec::new)
        } else {
            let (q, n) = (quick.clone(), named.clone());
            Box::new(move || crate::remote::gate::cloudflare_rate_channels(q.clone(), n.clone()))
        };
        // via 域名源：tunnel_error=true 模拟快照错误终态 → None 哨兵（原 gate 豁免
        // 判定随 2026-10-06 §G2 退役，None 哨兵仅供 via 保守回落 lan）
        let via_source: Box<ViaHostsSource> = if tunnel_error {
            Box::new(|| None)
        } else {
            Box::new(move || Some((quick.clone(), named.clone(), ts.clone())))
        };
        (
            Arc::new(RemoteState {
                ui_config_source: Box::new(|| None),
                subagent_source: std::collections::HashMap::new(),
                subagent_message_source: std::collections::HashMap::new(),
                target_evidence: no_target_evidence(),
                capability_table: crate::inject::capability::new_table(),
                session_source: Box::new(|| crate::session::SessionsResponse {
                    sessions: vec![],
                    total_count: 7,
                    waiting_count: 0,
                }),
                store: crate::remote::pairing::DeviceStore::memory(),
                // M7 Task 5：注入器缝——端点测试不触 flush 路径，用生产占位（Windows 为 Err 桩）
                injector: std::sync::Arc::new(crate::inject::engine::RealInjector),
                // R5 一键 resume spawn 缝（Task 11）：本组测试不触 session-open，注 no-op 桩
                resume_spawner: std::sync::Arc::new(|_: &crate::inject::resume::SpawnSpec| Ok(())),
                // C6：create 缝束缺省 stub（任务簿+发现/pid/工具探测/步距缝），仅
                // session-create 用例就地覆盖
                create_hub: create_hub_stub(),
                archive_source: Box::new(Vec::new),
                archive_delete: std::sync::Arc::new(|_: Option<&str>| 0usize),
                // A1 写入确认缝（M9R Task 5）：测试恒命中（首轮即中，零延迟零等待）
                confirm_probe: std::sync::Arc::new(|_, _, _| true),
                // 丁T3：本组测试的对话框在场探针缺省「无法判定」（None）——控制类注入
                // 照常投递；「在场即拒」的用例就地建 state 覆盖为假体（见 mode_switch_* 用例）
                dialog_probe: std::sync::Arc::new(|_, _| None),
                screen_probe: std::sync::Arc::new(|_, _| None),
                host_source: Box::new(|| serde_json::Value::Null),
                message_source: Box::new(|_, _, _| Err("测试桩：未注入内容源".to_string())),
                path_source: Box::new(|_, _, _| (Vec::new(), false)),
                watcher_tx: tokio::sync::broadcast::channel(64).0,
                board_hidden_ids: Box::new(Vec::new),
                board_hidden_hide: std::sync::Arc::new(|_| 0usize),
                board_hidden_unhide: std::sync::Arc::new(|_| 0usize),
                unread_mark_read: std::sync::Arc::new(|_, _| ()),
                session_close: std::sync::Arc::new(|_| Ok(())),
                sse_registry: Arc::new(SseRegistry::default()),
                max_devices_source: Box::new(move || max_devices),
                pin_limiter: std::sync::Mutex::new(crate::remote::pin::PinRateLimiter::new()),
                global_pin_limiter: std::sync::Mutex::new(
                    crate::remote::pin::PinRateLimiter::global(),
                ),
                pin_source: Box::new(move || pin_owned.clone()),
                now_source: Box::new(move || now.load(std::sync::atomic::Ordering::SeqCst)),
                via_hosts_source: via_source,
                rate_bucket_channels_source: rate_channels,
                home_source: Box::new(|| None),
                // C7：配对计数缝缺省空表（A3 用例不触配对打标）
                pairing_counter: Box::new(Vec::new),
            }),
            t,
        )
    }

    /// A3 自由请求构造：来源地址 / Host 头 / UA / cookie / JSON body 全可指定。
    /// host 不给 = 请求**不带 Host 头**（axum oneshot 不会自动补——正好锁定"空 Host fail closed"）
    #[allow(clippy::too_many_arguments)]
    fn http_req(
        method: &str,
        uri: &str,
        addr: &str,
        host: Option<&str>,
        ua: Option<&str>,
        cookie: Option<&str>,
        body: Option<&str>,
    ) -> axum::http::Request<Body> {
        let mut b = axum::http::Request::builder().method(method).uri(uri);
        if body.is_some() {
            b = b.header("content-type", "application/json");
        }
        if let Some(h) = host {
            b = b.header("host", h);
        }
        if let Some(u) = ua {
            b = b.header("user-agent", u);
        }
        if let Some(c) = cookie {
            b = b.header("cookie", c);
        }
        // axum oneshot 不注入 ConnectInfo extension——请求侧自补（post_json 同一适配）
        b = b.extension(axum::extract::ConnectInfo(
            addr.parse::<std::net::SocketAddr>().unwrap(),
        ));
        b.body(match body {
            Some(s) => Body::from(s.to_string()),
            None => Body::empty(),
        })
        .unwrap()
    }

    /// POST /pair/pin 收敛助手（无 cookie——配对入口本就无凭据）
    fn pin_post(
        addr: &str,
        host: Option<&str>,
        ua: Option<&str>,
        body: &str,
    ) -> axum::http::Request<Body> {
        http_req(
            "POST",
            "/m/api/v1/pair/pin",
            addr,
            host,
            ua,
            None,
            Some(body),
        )
    }

    /// 带自定义来源头的 POST /pair/pin（§G5 分桶测试用）
    fn pin_post_with(
        addr: &str,
        host: Option<&str>,
        extra: Option<(&str, &str)>,
        body: &str,
    ) -> axum::http::Request<Body> {
        let mut b = http_req(
            "POST",
            "/m/api/v1/pair/pin",
            addr,
            host,
            None,
            None,
            Some(body),
        );
        if let Some((k, v)) = extra {
            b.headers_mut().insert(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                axum::http::HeaderValue::from_str(v).unwrap(),
            );
        }
        b
    }

    /// 零豁免矩阵（安全关键，本 Task 的变异锚点）：
    /// **回环来源 + 任意 Host + 无 cookie → 一律 403。**
    /// 变异自证：谁把「回环 + Host 不在隧道名单 → 免密」加回来，本测试必红。
    /// 覆盖：本地形态 Host / 隧道域名 Host / 伪造回环 Host / 空 Host / 用户自有域名 Host。
    #[tokio::test]
    async fn gate_never_exempts_loopback_regardless_of_host() {
        let (state, _t) = a3_state(
            Some("1234"),
            3,
            &["mam-test.trycloudflare.com"],
            &[],
            &[],
            false,
        );
        let app = router(state);

        for host in [
            Some("localhost:9420"),             // 本地形态
            Some("127.0.0.1:9420"),             // 伪造回环形态（历史漏洞入口）
            Some("mam-test.trycloudflare.com"), // 隧道域名
            Some("mam.example.com"),            // 用户自有域名（直连反代形态）
            Some("evil.example.com"),           // 完全外部域名
            None,                               // 空 Host
        ] {
            let r = app
                .clone()
                .oneshot(http_req(
                    "GET",
                    "/m/api/v1/sessions",
                    "127.0.0.1:40000",
                    host,
                    None,
                    None,
                    None,
                ))
                .await
                .unwrap();
            assert_eq!(
                r.status(),
                403,
                "回环来源 + Host={host:?} + 无凭据必须 403（不变量 G1：请求头不得影响放行）"
            );
        }
    }

    /// 配对入口仍是唯一例外：`/pair/pin` 无凭据可达（否则无法换取凭据）。
    /// 与上一条配对使用，锁死「放行名单只有一个」这个边界。
    #[tokio::test]
    async fn gate_still_allows_pair_pin_only() {
        let (state, _t) = a3_state(Some("1234"), 3, &[], &[], &[], false);
        let app = router(state);

        // /pair/pin 无凭据可达（PIN 即凭据）——非 403 即可（本测试只断言不被门禁拦）
        let r = app
            .clone()
            .oneshot(pin_post(
                "127.0.0.1:40100",
                Some("localhost:9420"),
                None,
                r#"{"pin":"1234"}"#,
            ))
            .await
            .unwrap();
        assert_ne!(r.status(), 403, "/pair/pin 是换凭据入口，不得被门禁拦");

        // 其余端点无凭据一律 403
        let r = app
            .oneshot(http_req(
                "GET",
                "/m/api/v1/host",
                "127.0.0.1:40101",
                Some("localhost:9420"),
                None,
                None,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 403, "非配对端点无凭据必须 403");
    }

    /// 【2026-10-06 spike 探针 → 修复后的回归锚点】直连域名 + **同机反向代理**场景。
    ///
    /// 直连域名模式的接线是「兔维斯 绑端口供用户反向代理」（一期 spec §P7）。当用户的反代
    /// （nginx / Caddy）与 兔维斯 **同机**运行时，它是从**回环**把公网流量转发进来的，
    /// 而 `Host` 是用户自有域名——该域名**不在** quick/named 隧道名单里。
    /// 豁免判据第 ③ 步是**黑名单**（"不在名单"即算本地），于是这批公网流量会被
    /// **误判为本机**而免密放行。
    ///
    /// 本测试断言**修复后的正确行为**：回环来源 + 自有域名 Host + 无 cookie → 403。
    /// 修复前本测试为**红**（实测见 2026-10-06 spike 报告）。
    #[tokio::test]
    async fn gate_local_exempt_must_not_cover_same_host_reverse_proxy() {
        let (state, _t) = a3_state(
            Some("1234"),
            3,
            &["mam-test.trycloudflare.com"],
            &[],
            &[],
            false,
        );
        let app = router(state);

        // 三种用户域名形态：裸域名 / 带端口 / 与 tunnel.rs 夹具同形态的真实自有域名域名
        // 先收齐全部状态再断言——失败时一次拿到三种形态的完整事实（而非首个 panic 即止）
        let mut observed: Vec<(&str, u16)> = Vec::new();
        for host in [
            "mam.example.com",
            "mam.example.com:443",
            "mam-win.bondtoolbox.asia",
        ] {
            let r = app
                .clone()
                .oneshot(http_req(
                    "GET",
                    "/m/api/v1/sessions",
                    "127.0.0.1:41000",
                    Some(host),
                    None,
                    None,
                    None,
                ))
                .await
                .unwrap();
            observed.push((host, r.status().as_u16()));
        }
        assert!(
            observed.iter().all(|(_, s)| *s == 403),
            "同机反代（回环 + 自有域名 Host）不得免密，实测状态：{observed:?}"
        );
    }

    /// 快照不可信 → via 保守回落 lan（2026-10-06 §G2 重写：原「快照错误关豁免」
    /// 随豁免判定一并退役——其「无 cookie → 403」面由
    /// `gate_never_exempts_loopback_regardless_of_host` 锁定，「有效 cookie → 200」
    /// 由配对/会话各流测试覆盖）。名单哨兵 None 时配对照常成功（配对只认 PIN），
    /// 但 via 不得借用不可信名单打通道标签——即使 Host 是标准 quick 域名也标 lan
    /// （2026-09-18 实测误标根因的守护锚点）。
    /// 变异锚点：把 None 当空名单（fail-open 标注）或映射回本机 → 本测试必红
    #[tokio::test]
    async fn pair_via_falls_back_to_lan_when_tunnel_snapshot_degraded() {
        let (state, _t) = a3_state(
            Some("1234"),
            3,
            &["mam-test.trycloudflare.com"],
            &[],
            &[],
            true,
        );
        let app = router(state.clone());

        let r = app
            .clone()
            .oneshot(pin_post(
                "203.0.113.7:52000",
                Some("mam-test.trycloudflare.com"),
                Some("ua-degraded"),
                r#"{"pin":"1234"}"#,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "快照错误不阻断配对（配对只认 PIN）");
        let id = cookie_device_id(&r);
        let via: String = state.store.with(|c| {
            c.query_row("SELECT via FROM remote_devices WHERE id = ?1", [&id], |r| {
                r.get(0)
            })
            .unwrap()
        });
        assert_eq!(
            via, "lan",
            "名单不可信（None 哨兵）→ via 保守标 lan，绝不判隧道通道/本机"
        );
    }

    /// §C5 /channel 装饰端点（Task 10）：受门禁保护 + via 按 Host 推断 + limited
    /// 与实测速率如实下发。装饰纪律（不变量 §G1 推论③）锁面：本端点**只读不改**、
    /// 不触碰限速器/设备表——变异锚点是「有人拿 via 进安全判定」，本测试以
    /// 「配对流不受 /channel 调用影响」作行为侧守护（安全判定唯一依据仍是 cookie）。
    #[tokio::test]
    async fn channel_endpoint_gated_and_reports_via_limited_and_measured_rates() {
        let (state, _t) = a3_state(
            Some("1234"),
            3,
            &["mam-test.trycloudflare.com"],
            &["mam.example.com"],
            &["jarvismac-mini.example-tailnet.ts.net"],
            false,
        );
        let app = router(state.clone());

        // ① 无 cookie → 403（/channel 在 nest 内受 gate 结构性覆盖，与其他端点同款；
        //    装饰端点也不裸奔）
        let r = app
            .clone()
            .oneshot(http_req(
                "GET",
                "/m/api/v1/channel",
                "203.0.113.7:54000",
                Some("mam-test.trycloudflare.com"),
                None,
                None,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 403, "装饰端点同样受门禁保护（无凭据 403）");

        // ② 配对拿有效 cookie（经 quick 域名）
        let r = app
            .clone()
            .oneshot(pin_post(
                "203.0.113.7:54001",
                Some("mam-test.trycloudflare.com"),
                Some("ua-channel"),
                r#"{"pin":"1234"}"#,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let cookie = format!("mam_device={}", cookie_device_id(&r));

        // ③ Host=quick 域名 → via=quick + limited=true + 实测速率原样下发
        let r = app
            .clone()
            .oneshot(http_req(
                "GET",
                "/m/api/v1/channel",
                "203.0.113.7:54002",
                Some("mam-test.trycloudflare.com"),
                None,
                Some(&cookie),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(
            r.headers()
                .get("cache-control")
                .and_then(|v| v.to_str().ok()),
            Some("no-store"),
            "通道可达性会变化，必须 no-store"
        );
        let body = body_string(r).await;
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["via"], "quick", "Host==quick 域名 → via=quick");
        assert_eq!(v["limited"], true, "隧道通道 → limited=true");
        assert_eq!(
            v["est_mbps_down"], 1.8,
            "下行实测速率照抄（客户端实收口径，勿改服务端发出量口径）"
        );
        assert_eq!(v["est_mbps_up"], 0.8, "上行实测速率照抄（服务端实收口径）");

        // ④ Host=局域网地址 → via=lan + limited=false
        let r = app
            .clone()
            .oneshot(http_req(
                "GET",
                "/m/api/v1/channel",
                "192.168.1.9:54003",
                Some("192.168.1.9:9420"),
                None,
                Some(&cookie),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["via"], "lan", "非隧道 Host → via=lan");
        assert_eq!(v["limited"], false, "局域网直连不受限");

        // ⑤ Host=tailscale 机器域名 → via=tailscale（§C1 四值分类在装饰端点同款生效）
        let r = app
            .clone()
            .oneshot(http_req(
                "GET",
                "/m/api/v1/channel",
                "203.0.113.8:54004",
                Some("jarvismac-mini.example-tailnet.ts.net"),
                None,
                Some(&cookie),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["via"], "tailscale", "机器域名命中 → via=tailscale");
        assert_eq!(v["limited"], true);
    }

    /// §C5 /channel：名单哨兵 None（快照错误态）→ via 保守回落 lan（与 /pair/pin
    /// 同款口径，见 pair_via_falls_back_to_lan_when_tunnel_snapshot_degraded）——
    /// 即使 Host 是标准 quick 域名也不得借不可信名单打隧道标签
    #[tokio::test]
    async fn channel_via_falls_back_to_lan_when_tunnel_snapshot_degraded() {
        let (state, _t) = a3_state(
            Some("1234"),
            3,
            &["mam-test.trycloudflare.com"],
            &[],
            &[],
            true, // tunnel_error → via_hosts_source 恒 None
        );
        let app = router(state.clone());
        let r = app
            .clone()
            .oneshot(pin_post(
                "203.0.113.7:54005",
                Some("mam-test.trycloudflare.com"),
                Some("ua-channel-degraded"),
                r#"{"pin":"1234"}"#,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let cookie = format!("mam_device={}", cookie_device_id(&r));
        let r = app
            .oneshot(http_req(
                "GET",
                "/m/api/v1/channel",
                "203.0.113.7:54006",
                Some("mam-test.trycloudflare.com"),
                None,
                Some(&cookie),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(
            v["via"], "lan",
            "名单不可信（None 哨兵）→ via 保守标 lan，不借不可信名单打隧道标签"
        );
        assert_eq!(v["limited"], false);
    }

    /// §C1 接线：Host 命中 tailscale 机器域名 → via=tailscale（Task 5 给 classify_via
    /// 加的第四分支——漏接时 Tailscale 通道的设备在花名册上会被误标 lan 且无来源徽标）。
    /// 配套断言（§G5 纪律）：伪造 CF-Connecting-IP 在 Tailscale Host 上**不得**被采信
    /// （Funnel 没有 Cloudflare 边缘替我们拒伪造头）——tailscale 不在限速声明表里，
    /// "未声明 ⇒ 不开桶"的行为断言见 `pair_pin_undeclared_channel_never_opens_a_bucket`；
    /// 本测试锁**落库来源**：真实对端地址，既不是伪造头也不是内部哨兵（评审 A-M3）
    #[tokio::test]
    async fn pair_via_tailscale_host_is_labeled_tailscale_and_cf_header_not_trusted() {
        let (state, _t) = a3_state(
            Some("1234"),
            3,
            &[],
            &[],
            &["jarvismac-mini.example-tailnet.ts.net"],
            false,
        );
        let app = router(state.clone());

        // via 装饰标注：机器域名 → tailscale
        let r = app
            .clone()
            .oneshot(pin_post(
                "203.0.113.9:53000",
                Some("jarvismac-mini.example-tailnet.ts.net"),
                Some("ua-ts"),
                r#"{"pin":"1234"}"#,
            ))
            .await
            .unwrap();
        assert_eq!(
            r.status(),
            200,
            "PIN 正确必须 200（via 是纯装饰，不影响配对）"
        );
        let id = cookie_device_id(&r);
        let via: String = state.store.with(|c| {
            c.query_row("SELECT via FROM remote_devices WHERE id = ?1", [&id], |r| {
                r.get(0)
            })
            .unwrap()
        });
        assert_eq!(
            via, "tailscale",
            "Host==机器域名 → via=tailscale（不得误标 lan）"
        );

        // 落库来源（§G5 + 评审 A-M3）：同 Host + 回环来源 + 伪造 CF-Connecting-IP →
        // 伪造头**不得**被采信（tailscale 无 CF 边缘背书，不在声明表里）。旧断言曾读
        // `origin_ip == GLOBAL_BUCKET`——那是把内部哨兵当来源落库的 bug（含 NUL、
        // 且不是来源地址）；正确行为 = 回落**真实 TCP 对端** 127.0.0.1
        let r = app
            .oneshot(pin_post_with(
                "127.0.0.1:53001",
                Some("jarvismac-mini.example-tailnet.ts.net"),
                Some(("cf-connecting-ip", "9.9.9.9")),
                r#"{"pin":"1234"}"#,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let origin_ip: String = state.store.with(|c| {
            c.query_row(
                "SELECT origin_ip FROM remote_devices WHERE id = ?1",
                [cookie_device_id(&r)],
                |r| r.get(0),
            )
            .unwrap()
        });
        assert_eq!(
            origin_ip, "127.0.0.1",
            "伪造的 CF 头不得成为落库来源——回落到真实 TCP 对端"
        );
        assert_ne!(
            origin_ip,
            crate::remote::pin::GLOBAL_BUCKET,
            "内部哨兵（含 NUL）不得外泄进 remote_devices.origin_ip（评审 A-M3）"
        );
        assert!(!origin_ip.contains('\u{0}'), "数据列绝不含 NUL");
    }

    /// 配对流（矩阵 4）：PIN 对 → 200 + Set-Cookie（携带 upsert 返回 id）+ 落行；
    /// via 按 Host 三分支装饰标注（quick 域名 → quick / named 域名 → named / 其余 →
    /// lan，**不再有 local 分支**——本机不是通道，2026-10-06 零豁免）；两台不同
    /// UA/IP 设备 → 各自一行；同 UA+IP 重绑 → 同行 id（cookie 刷新）——A1 upsert
    /// 语义在新端点上存活
    #[tokio::test]
    async fn pin_pair_flow_via_three_branches_two_devices_and_rejoin() {
        let (state, _t) = a3_state(
            Some("1234"),
            10,
            &["mam-test.trycloudflare.com"],
            &["mam.example.com"],
            &[],
            false,
        );
        let app = router_with_static(state.clone());

        // 设备 A：经 quick 隧道域名 → via=quick；cookie 五属性全断言（评审 Important 3 既有口径）
        let r = app
            .clone()
            .oneshot(pin_post(
                "203.0.113.7:51000",
                Some("mam-test.trycloudflare.com"),
                Some("ua-A"),
                r#"{"pin":"1234","name":"我的手机"}"#,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "PIN 正确必须 200");
        let cookie = r
            .headers()
            .get("set-cookie")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        let id_a = cookie
            .split("mam_device=")
            .nth(1)
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();
        assert!(
            id_a.len() == 32 && id_a.chars().all(|c| c.is_ascii_hexdigit()),
            "device id 应为 32 位 hex，实际 {id_a:?}"
        );
        assert_eq!(
            cookie,
            format!(
                "mam_device={id_a}; Path=/m; HttpOnly; SameSite=Lax; Max-Age={}",
                crate::remote::pairing::DEVICE_TTL_MS / 1000
            ),
            "cookie 必须同时具备 mam_device=hex / Path=/m / HttpOnly / SameSite=Lax / Max-Age=180d"
        );
        let row_a: (String, String, String, String) = state.store.with(|c| {
            c.query_row(
                "SELECT via, name, ua, origin_ip FROM remote_devices WHERE id = ?1",
                [&id_a],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap()
        });
        assert_eq!(row_a.0, "quick", "Host==quick 域名 → via=quick");
        assert_eq!(row_a.1, "我的手机", "自报名落库");
        assert_eq!(row_a.2, "ua-A", "真实 UA 落库");
        assert_eq!(row_a.3, "203.0.113.7", "ConnectInfo 来源 IP 落库");

        // 设备 B：局域网直连（非回环 + 本地 Host）→ via=lan；缺 name → 默认名
        let r = app
            .clone()
            .oneshot(pin_post(
                "10.0.0.8:51001",
                Some("192.168.1.9:9420"),
                Some("ua-B"),
                r#"{"pin":"1234"}"#,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let id_b = cookie_device_id(&r);
        let row_b: (String, String) = state.store.with(|c| {
            c.query_row(
                "SELECT via, name FROM remote_devices WHERE id = ?1",
                [&id_b],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
        });
        assert_eq!(row_b.0, "lan", "非隧道 Host 的非回环来源 → via=lan");
        assert_eq!(row_b.1, "新设备", "缺省名回落默认名");

        // 设备 C：经 named 域名 → via=named
        let r = app
            .clone()
            .oneshot(pin_post(
                "198.51.100.3:51002",
                Some("mam.example.com:443"),
                Some("ua-C"),
                r#"{"pin":"1234"}"#,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let id_c = cookie_device_id(&r);
        let via_c: String = state.store.with(|c| {
            c.query_row(
                "SELECT via FROM remote_devices WHERE id = ?1",
                [&id_c],
                |r| r.get(0),
            )
            .unwrap()
        });
        assert_eq!(via_c, "named", "Host==named 域名（带端口归一）→ via=named");

        // 设备 D：本机回环 + 本地 Host → via=lan（2026-10-06 零豁免：本机不是通道，
        // 服务端不再产生 local 标注）
        let r = app
            .clone()
            .oneshot(pin_post(
                "127.0.0.1:51003",
                Some("localhost:9420"),
                Some("ua-D"),
                r#"{"pin":"1234"}"#,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let id_d = cookie_device_id(&r);
        let via_d: String = state.store.with(|c| {
            c.query_row(
                "SELECT via FROM remote_devices WHERE id = ?1",
                [&id_d],
                |r| r.get(0),
            )
            .unwrap()
        });
        assert_eq!(
            via_d, "lan",
            "回环 + 非隧道 Host → via=lan（不再有 local 分支）"
        );

        // 四台设备 = 四行（UA/IP 互异，指纹各不同）
        let rows: i64 = state.store.with(|c| {
            c.query_row("SELECT COUNT(*) FROM remote_devices", [], |r| r.get(0))
                .unwrap()
        });
        assert_eq!(rows, 4, "四台不同 UA/IP 设备必须四行");

        // 设备 A 同 UA + 同来源 IP 重绑 → upsert 命中同行 id（cookie 刷新指向旧行），不新增
        let r = app
            .oneshot(pin_post(
                "203.0.113.7:51000",
                Some("mam-test.trycloudflare.com"),
                Some("ua-A"),
                r#"{"pin":"1234"}"#,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(
            cookie_device_id(&r),
            id_a,
            "同指纹重绑 Set-Cookie 必须携带旧行 id"
        );
        let rows: i64 = state.store.with(|c| {
            c.query_row("SELECT COUNT(*) FROM remote_devices", [], |r| r.get(0))
                .unwrap()
        });
        assert_eq!(rows, 4, "重绑不新增行");
    }

    /// **R12-S2 / 裁决 24b：设备名注册点拒危险花名**（登记 = 配对时刻）——400
    /// `device_name_unsafe` + 原因（**点名具体字符** + 给出路）+ **不落库、不下发 cookie**；
    /// 对照组：正常中文花名照常配对成功（白名单不得误伤中文——票面点名的回归面）。
    ///
    /// **门序**：花名判据在 ③ PIN 校验与 ④ 名额门**之后**（落在 `persist_and_cookie` 内）
    /// ——故本端点不会把「花名合法与否」变成 PIN 预言机（错误 PIN 仍走 401，见 invalid_pin 族）。
    /// 移动端现版本只发 `{pin}`（`name` 可选、缺省回落「新设备」恒安全）⇒ 本门只挡自报
    /// 花名的客户端（QR/第三方），移动端行为不变。
    #[tokio::test]
    async fn pin_pairing_rejects_unsafe_device_name() {
        let (state, _t) = a3_state(Some("1234"), 3, &[], &[], &[], false);
        let app = router(state.clone());
        let r = app
            .clone()
            .oneshot(pin_post(
                "10.7.7.7:7000",
                Some("192.168.1.7:9420"),
                Some("ua-y"),
                r#"{"pin":"1234","name":"花&名"}"#,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 400, "危险花名 ⇒ 拒登记（400，语义在 body）");
        assert!(
            r.headers().get("set-cookie").is_none(),
            "拒登记不得下发任何凭证"
        );
        let body = axum::body::to_bytes(r.into_body(), 4096).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["error"], "device_name_unsafe", "{v}");
        let reason = v["reason"].as_str().unwrap_or_default();
        assert!(
            reason.contains('&') && reason.contains("未投递") && reason.contains("改名"),
            "拒绝原因必须点名具体字符 + 说清零字节投递 + 给出路（改名）: {v}"
        );
        let rows: i64 = state.store.with(|c| {
            c.query_row("SELECT COUNT(*) FROM remote_devices", [], |r| r.get(0))
                .unwrap()
        });
        assert_eq!(rows, 0, "拒登记不得落库（不是「登记后再拒投递」）");

        // 对照组：正常中文花名 ⇒ 200 + cookie + 落库一行（白名单不得误伤中文）
        let r = app
            .clone()
            .oneshot(pin_post(
                "10.7.7.7:7000",
                Some("192.168.1.7:9420"),
                Some("ua-y"),
                r#"{"pin":"1234","name":"小明的手机"}"#,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "正常中文花名必须配对成功");
        assert!(
            r.headers().get("set-cookie").is_some(),
            "配对成功必须下发 cookie"
        );
        let rows: i64 = state.store.with(|c| {
            c.query_row("SELECT COUNT(*) FROM remote_devices", [], |r| r.get(0))
                .unwrap()
        });
        assert_eq!(rows, 1, "对照组落库一行");
    }

    /// 限速矩阵（矩阵 5）：连错 4 次 → 401 且 remaining 递减（4,3,2,1）；第 5 次错 → 401
    /// （remaining 0）；第 6 次请求（即使 PIN 对）→ 429 + retryAfter=600；时钟推进过锁期
    /// （now_source 注入缝）→ 正确 PIN 配对成功
    #[tokio::test]
    async fn pin_rate_limit_locks_after_five_failures_then_expires() {
        let (state, t) = a3_state(Some("1234"), 3, &[], &[], &[], false);
        let app = router(state.clone());
        for want in [4, 3, 2, 1] {
            let r = app
                .clone()
                .oneshot(pin_post(
                    "10.9.9.9:6000",
                    Some("192.168.1.9:9420"),
                    Some("ua-x"),
                    r#"{"pin":"0000"}"#,
                ))
                .await
                .unwrap();
            assert_eq!(r.status(), 401, "第 {} 次错应 401", 5 - want);
            let body = axum::body::to_bytes(r.into_body(), 4096).await.unwrap();
            let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(v["error"], "invalid_pin");
            assert_eq!(v["remaining"], want, "remaining 应递减为 {want}");
        }
        // 第 5 次错 → 401（remaining 0——次数披露到此为止，此后一律 429）
        let r = app
            .clone()
            .oneshot(pin_post(
                "10.9.9.9:6000",
                Some("192.168.1.9:9420"),
                Some("ua-x"),
                r#"{"pin":"0000"}"#,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 401, "第 5 次错仍是 401 invalid_pin");
        let body = axum::body::to_bytes(r.into_body(), 4096).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["remaining"], 0, "第 5 次失败后剩余 0");

        // 第 6 次：即使 PIN 正确 → 429 + retryAfter（锁内正确 PIN 也拒）
        let r = app
            .clone()
            .oneshot(pin_post(
                "10.9.9.9:6000",
                Some("192.168.1.9:9420"),
                Some("ua-x"),
                r#"{"pin":"1234"}"#,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 429, "锁定期内正确 PIN 也必须 429");
        assert!(
            r.headers().get("set-cookie").is_none(),
            "429 不得下发任何凭证"
        );
        let body = axum::body::to_bytes(r.into_body(), 4096).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["retryAfter"], 600, "整锁 10 分钟 → retryAfter 600 秒");

        // 时钟推进过锁期（600_001ms）→ 正确 PIN 配对成功（配对产物落行可验证）
        t.fetch_add(600_001, std::sync::atomic::Ordering::SeqCst);
        let r = app
            .oneshot(pin_post(
                "10.9.9.9:6000",
                Some("192.168.1.9:9420"),
                Some("ua-x"),
                r#"{"pin":"1234"}"#,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "锁定到期后正确 PIN 可配对");
        let id = cookie_device_id(&r);
        let n: i64 = state.store.with(|c| {
            c.query_row(
                "SELECT COUNT(*) FROM remote_devices WHERE id = ?1",
                [&id],
                |r| r.get(0),
            )
            .unwrap()
        });
        assert_eq!(n, 1, "成功配对应落一行设备记录");
    }

    /// §G5 端点级验收 1：回环 + 隧道域名 + 权威头存在 → 按权威头分桶，
    /// 不同 CF-Connecting-IP 互不影响（捣乱者只填自己的桶）。
    #[tokio::test]
    async fn pair_pin_buckets_by_cf_connecting_ip_when_tunneled() {
        let (state, _t) = a3_state(
            Some("1234"),
            3,
            &["mam-test.trycloudflare.com"],
            &[],
            &[],
            false,
        );
        let app = router(state);

        // 捣乱者（1.2.3.4）连错 5 次
        for _ in 0..5 {
            let r = app
                .clone()
                .oneshot(pin_post_with(
                    "127.0.0.1:40200",
                    Some("mam-test.trycloudflare.com"),
                    Some(("cf-connecting-ip", "1.2.3.4")),
                    r#"{"pin":"0000"}"#,
                ))
                .await
                .unwrap();
            assert_ne!(r.status(), 200, "错误 PIN 不得成功");
        }
        // 捣乱者被锁
        let r = app
            .clone()
            .oneshot(pin_post_with(
                "127.0.0.1:40201",
                Some("mam-test.trycloudflare.com"),
                Some(("cf-connecting-ip", "1.2.3.4")),
                r#"{"pin":"1234"}"#,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 429, "捣乱者自己的桶应已锁");
        // **本人不受影响**：换一个来源（5.6.7.8）用正确 PIN 仍可配对
        let r = app
            .oneshot(pin_post_with(
                "127.0.0.1:40202",
                Some("mam-test.trycloudflare.com"),
                Some(("cf-connecting-ip", "5.6.7.8")),
                r#"{"pin":"1234"}"#,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "他人的桶不得影响本人（§G5 核心断言）");
    }

    /// §G5 端点级验收 2：回环 + **非隧道 Host**（直连反代形态）→ 回落全局桶，
    /// 即不同 CF-Connecting-IP 也共用一个桶（fail-closed 的有意选择）。
    #[tokio::test]
    async fn pair_pin_falls_back_to_global_bucket_for_non_tunnel_host() {
        let (state, _t) = a3_state(
            Some("1234"),
            3,
            &["mam-test.trycloudflare.com"],
            &[],
            &[],
            false,
        );
        let app = router(state);

        for _ in 0..5 {
            let _ = app
                .clone()
                .oneshot(pin_post_with(
                    "127.0.0.1:40300",
                    Some("mam.example.com"),
                    Some(("cf-connecting-ip", "1.2.3.4")),
                    r#"{"pin":"0000"}"#,
                ))
                .await
                .unwrap();
        }
        // 换个来源、正确 PIN——仍被锁（因为落的是同一个全局桶）
        let r = app
            .oneshot(pin_post_with(
                "127.0.0.1:40301",
                Some("mam.example.com"),
                Some(("cf-connecting-ip", "5.6.7.8")),
                r#"{"pin":"1234"}"#,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 429, "非隧道 Host 回落全局桶（fail-closed）");
    }

    /// 评审 A-M4：把**全局桶**直接预置为 Locked，断言「隧道 Host + 权威头」（其分桶键
    /// 是 1.2.3.4，即**另一个分来源桶**）的请求也 429——这才真正隔离出全局桶。
    /// 原用例（上一条）全部请求来自 127.0.0.1，"按回环对端分桶"的实现同样会 429，
    /// 抓不住"回落成了按回环对端分桶"。
    #[tokio::test]
    async fn pair_pin_global_bucket_blocks_clean_source_buckets_too() {
        let (state, t) = a3_state(
            Some("1234"),
            3,
            &["mam-test.trycloudflare.com"],
            &[],
            &[],
            false,
        );
        let app = router(state.clone());
        let now = t.load(std::sync::atomic::Ordering::SeqCst);
        // 预置全局桶：恰满阈值即锁（走 state 的注入时钟，零 sleep）
        {
            let mut g = state.global_pin_limiter.lock().unwrap();
            for _ in 0..crate::remote::pin::GLOBAL_MAX_FAILURES {
                g.record_failure(crate::remote::pin::GLOBAL_BUCKET, now);
            }
        }
        // 隔离性前置：该 CF 来源自己的桶**没锁**——429 不可能是分来源桶给的
        assert_eq!(
            state.pin_limiter.lock().unwrap().check("1.2.3.4", now),
            crate::remote::pin::RateDecision::Allowed,
            "前置：1.2.3.4 的分来源桶干净"
        );
        let r = app
            .clone()
            .oneshot(pin_post_with(
                "127.0.0.1:40400",
                Some("mam-test.trycloudflare.com"),
                Some(("cf-connecting-ip", "1.2.3.4")),
                r#"{"pin":"1234"}"#,
            ))
            .await
            .unwrap();
        assert_eq!(
            r.status(),
            429,
            "全局桶锁定 → 已声明通道 + 干净来源桶 + 正确 PIN 也拒（429 只能来自全局桶）"
        );
        let body = axum::body::to_bytes(r.into_body(), 4096).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["retryAfter"], 600, "全局桶锁定期 = LOCK_MS（600 秒）");
        // 局域网来源（自己那一路桶，且是"合法新设备配对"的恢复路径）同样被挡——
        // 这正是评审 A-I1 要消除的伤害面
        let r = app
            .oneshot(pin_post(
                "192.168.1.50:40401",
                Some("192.168.1.9:9420"),
                Some("ua-lan"),
                r#"{"pin":"1234"}"#,
            ))
            .await
            .unwrap();
        assert_eq!(
            r.status(),
            429,
            "全局闸锁定时所有人（含局域网合法新设备）都被挡"
        );
    }

    /// 评审 A-I2 端点级：**未声明的通道**（tailscale Funnel）上伪造权威头不得开桶——
    /// 5 次失败只记进全局桶（阈值 50，远未触顶）；随后在**已声明**通道（quick）上用
    /// 同一个伪造头值 9.9.9.9 正确 PIN → 200。变异锚点：把 tailscale 并进声明表，
    /// 那 5 次失败就会锁掉 9.9.9.9 这个桶 → 末断言红（旧实现靠 api.rs 的特例注释排除
    /// ts，现在由"未声明 ⇒ 回落全局桶"结构性承担）。
    #[tokio::test]
    async fn pair_pin_undeclared_channel_never_opens_a_bucket() {
        let (state, _t) = a3_state(
            Some("1234"),
            3,
            &["mam-test.trycloudflare.com"],
            &[],
            &["jarvismac-mini.example-tailnet.ts.net"],
            false,
        );
        let app = router(state);
        for _ in 0..5 {
            let r = app
                .clone()
                .oneshot(pin_post_with(
                    "127.0.0.1:40500",
                    Some("jarvismac-mini.example-tailnet.ts.net"),
                    Some(("cf-connecting-ip", "9.9.9.9")),
                    r#"{"pin":"0000"}"#,
                ))
                .await
                .unwrap();
            assert_eq!(r.status(), 401, "未声明通道上的失败照常 401（记进全局桶）");
        }
        let r = app
            .oneshot(pin_post_with(
                "127.0.0.1:40501",
                Some("mam-test.trycloudflare.com"),
                Some(("cf-connecting-ip", "9.9.9.9")),
                r#"{"pin":"1234"}"#,
            ))
            .await
            .unwrap();
        assert_eq!(
            r.status(),
            200,
            "未声明通道不得为伪造头值开桶（否则 9.9.9.9 桶已被上面 5 次锁掉 → 这里必 429）"
        );
    }

    /// 评审 A-M3 端点级：落库来源（origin_ip）与限速分桶键**分离**——origin_ip 恒为
    /// 真实来源：① 可证隧道流量 → 权威头值（§G5 要求来源记录能区分公网来源）；
    /// ② 不可证（此处：回环 + 非隧道 Host）→ 真实 TCP 对端 127.0.0.1，**绝不是内部哨兵**
    /// （GLOBAL_BUCKET 含 NUL）。变异锚点：把分桶键当 origin_ip 传回
    /// （persist_and_cookie(&key)）→ ② 档必红。
    #[tokio::test]
    async fn pair_pin_records_real_origin_not_the_rate_key_sentinel() {
        let (state, _t) = a3_state(
            Some("1234"),
            10,
            &["mam-test.trycloudflare.com"],
            &[],
            &[],
            false,
        );
        let app = router(state.clone());
        let read_origin = |id: &str| -> String {
            state.store.with(|c| {
                c.query_row(
                    "SELECT origin_ip FROM remote_devices WHERE id = ?1",
                    [id],
                    |r| r.get(0),
                )
                .unwrap()
            })
        };
        // ② 不可证：回环 + 非隧道 Host（直连域名 + 同机反代形态）→ 真实对端地址
        let r = app
            .clone()
            .oneshot(pin_post(
                "127.0.0.1:40700",
                Some("mam.example.com"),
                Some("ua-unprovable"),
                r#"{"pin":"1234"}"#,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let origin = read_origin(&cookie_device_id(&r));
        assert_eq!(
            origin, "127.0.0.1",
            "不可证来源 → 真实 TCP 对端（不是哨兵、不是伪造头）"
        );
        assert_ne!(
            origin,
            crate::remote::pin::GLOBAL_BUCKET,
            "内部哨兵不得落进 remote_devices.origin_ip"
        );
        assert!(!origin.contains('\u{0}'), "数据列绝不含 NUL");
        // ① 可证：回环 + 已声明通道 + 权威头 → 权威头值（来源记录区分度，§G5）
        let r = app
            .oneshot(pin_post_with(
                "127.0.0.1:40701",
                Some("mam-test.trycloudflare.com"),
                Some(("cf-connecting-ip", "198.51.100.44")),
                r#"{"pin":"1234"}"#,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(
            read_origin(&cookie_device_id(&r)),
            "198.51.100.44",
            "可证公网来源落库（花名册/审计来源可用）"
        );
    }

    /// 评审 B 追加项**变异锚点**（端点级）：设备指纹用**归一化来源**（原始 TCP 对端
    /// `addr.ip()`），展示列 `origin_ip` 仍存真实公网出口 IP（§G5 保留）。同一浏览器
    /// （同 UA + 同 TCP 对端）换网络重配（不同 CF-Connecting-IP）必须**覆盖原行、不新增**
    /// ——否则一部手机在家 WiFi/蜂窝/换地方各建一行，默认上限 3 直接占满，之后连笔记本
    /// 都配不上（正是本功能的目标场景：户外连接）。
    /// 变异自证：`persist_and_cookie` 的指纹输入换回 display_origin（传 origin）→
    /// 设备数变 2、id 不等，本测试必红。
    #[tokio::test]
    async fn pair_pin_fingerprint_uses_peer_so_re_network_reuses_the_row() {
        let (state, _t) = a3_state(
            Some("1234"),
            10,
            &["mam-test.trycloudflare.com"],
            &[],
            &[],
            false,
        );
        let app = router(state.clone());
        let count = || -> i64 {
            state.store.with(|c| {
                c.query_row("SELECT COUNT(*) FROM remote_devices", [], |r| r.get(0))
                    .unwrap()
            })
        };
        // 第一次：家 WiFi 出口 → CF 边缘 → 本机（TCP 对端恒 127.0.0.1）
        let r = app
            .clone()
            .oneshot(pin_post_with(
                "127.0.0.1:40710",
                Some("mam-test.trycloudflare.com"),
                Some(("cf-connecting-ip", "198.51.100.44")),
                r#"{"pin":"1234"}"#,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let id1 = cookie_device_id(&r);
        assert_eq!(count(), 1);
        // 第二次：同一浏览器换到蜂窝（出口 IP 变了，TCP 对端与 UA 未变）
        let r = app
            .clone()
            .oneshot(pin_post_with(
                "127.0.0.1:40711",
                Some("mam-test.trycloudflare.com"),
                Some(("cf-connecting-ip", "203.0.113.7")),
                r#"{"pin":"1234"}"#,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let id2 = cookie_device_id(&r);
        assert_eq!(
            id1, id2,
            "换网络重配必须命中同一行（指纹输入 = 原始 TCP 对端，不是出口 IP）"
        );
        assert_eq!(
            count(),
            1,
            "换网络重配不得新增设备行（名额不被一部手机占满）"
        );
        let origin: String = state.store.with(|c| {
            c.query_row(
                "SELECT origin_ip FROM remote_devices WHERE id = ?1",
                [&id1],
                |r| r.get(0),
            )
            .unwrap()
        });
        assert_eq!(
            origin, "203.0.113.7",
            "展示列仍更新为最新真实公网来源（§G5 要求保留）"
        );
    }

    /// 上限门（矩阵 6）：满员（max=1，已配一台）→ 第二台 PIN 对也拒——沿用既有直通
    /// 上限语义（403 + {"error":"cap_full"}）；门在 PIN 正确**之后**判定（先验 PIN 再谈
    /// 名额）；腾位后同 PIN 可配
    #[tokio::test]
    async fn pin_pair_rejected_when_device_cap_full() {
        let (state, _t) = a3_state(Some("1234"), 1, &[], &[], &[], false);
        let app = router(state.clone());
        // 第一台占满名额
        let r = app
            .clone()
            .oneshot(pin_post(
                "10.0.0.1:7000",
                Some("192.168.1.9:9420"),
                Some("ua-1"),
                r#"{"pin":"1234"}"#,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "前置：第一台配对成功");
        let id1 = cookie_device_id(&r);
        // 第二台：PIN 正确但满员 → 403 cap_full
        let r = app
            .clone()
            .oneshot(pin_post(
                "10.0.0.2:7001",
                Some("192.168.1.9:9420"),
                Some("ua-2"),
                r#"{"pin":"1234"}"#,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 403);
        let body = axum::body::to_bytes(r.into_body(), 4096).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["error"], "cap_full", "上限语义沿用仓库既有 cap_full 契约");
        // 腾位后同 PIN 可配
        state
            .store
            .with(|c| crate::remote::pairing::revoke_device(c, &id1).unwrap());
        let r = app
            .oneshot(pin_post(
                "10.0.0.2:7001",
                Some("192.168.1.9:9420"),
                Some("ua-2"),
                r#"{"pin":"1234"}"#,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "腾位后同 PIN 应可配对");
    }

    /// pin_not_set（矩阵 7）：KV 空 → 401 {"error":"pin_not_set"}，**不计失败**——
    /// 连发 5 次错误 PIN 也不进入锁定（无密可对；A5 开通道时自动生成，本任务只留语义）。
    /// pin 源用可变槽位：切到 Some 后正确 PIN 立即可配，证明 5 次未计失败
    #[tokio::test]
    async fn pin_not_set_is_unauthorized_and_records_no_failure() {
        let pin_slot: Arc<std::sync::Mutex<Option<String>>> = Arc::new(std::sync::Mutex::new(None));
        let slot = pin_slot.clone();
        let (state, _t) = {
            let t = Arc::new(std::sync::atomic::AtomicI64::new(1_000_000));
            let now = t.clone();
            (
                Arc::new(RemoteState {
                    ui_config_source: Box::new(|| None),
                    subagent_source: std::collections::HashMap::new(),
                    subagent_message_source: std::collections::HashMap::new(),
                    target_evidence: no_target_evidence(),
                    capability_table: crate::inject::capability::new_table(),
                    session_source: Box::new(|| crate::session::SessionsResponse {
                        sessions: vec![],
                        total_count: 0,
                        waiting_count: 0,
                    }),
                    store: crate::remote::pairing::DeviceStore::memory(),
                    // M7 Task 5：注入器缝——端点测试不触 flush 路径，用生产占位（Windows 为 Err 桩）
                    injector: std::sync::Arc::new(crate::inject::engine::RealInjector),
                    // R5 一键 resume spawn 缝（Task 11）：本组测试不触 session-open，注 no-op 桩
                    resume_spawner: std::sync::Arc::new(|_: &crate::inject::resume::SpawnSpec| {
                        Ok(())
                    }),
                    // C6：create 缝束缺省 stub（仅 session-create 用例就地覆盖）
                    create_hub: create_hub_stub(),
                    archive_source: Box::new(Vec::new),
                    archive_delete: std::sync::Arc::new(|_: Option<&str>| 0usize),
                    // A1 写入确认缝（M9R Task 5）：测试恒命中（首轮即中，零延迟零等待）
                    confirm_probe: std::sync::Arc::new(|_, _, _| true),
                    // 丁T3：本组测试的对话框在场探针缺省「无法判定」（None）——控制类注入
                    // 照常投递；「在场即拒」的用例就地建 state 覆盖为假体（见 mode_switch_* 用例）
                    dialog_probe: std::sync::Arc::new(|_, _| None),
                    screen_probe: std::sync::Arc::new(|_, _| None),
                    host_source: Box::new(|| serde_json::Value::Null),
                    message_source: Box::new(|_, _, _| Err("测试桩：未注入内容源".to_string())),
                    path_source: Box::new(|_, _, _| (Vec::new(), false)),
                    watcher_tx: tokio::sync::broadcast::channel(64).0,
                    board_hidden_ids: Box::new(Vec::new),
                    board_hidden_hide: std::sync::Arc::new(|_| 0usize),
                    board_hidden_unhide: std::sync::Arc::new(|_| 0usize),
                    unread_mark_read: std::sync::Arc::new(|_, _| ()),
                    session_close: std::sync::Arc::new(|_| Ok(())),
                    sse_registry: Arc::new(SseRegistry::default()),
                    max_devices_source: Box::new(|| 3),
                    pin_limiter: std::sync::Mutex::new(crate::remote::pin::PinRateLimiter::new()),
                    global_pin_limiter: std::sync::Mutex::new(
                        crate::remote::pin::PinRateLimiter::global(),
                    ),
                    pin_source: Box::new(move || slot.lock().unwrap().clone()),
                    now_source: Box::new(move || now.load(std::sync::atomic::Ordering::SeqCst)),
                    via_hosts_source: Box::new(|| None),
                    rate_bucket_channels_source: Box::new(Vec::new),
                    home_source: Box::new(|| None),
                    // C7：配对计数缝缺省空表（限速用例不触配对打标）
                    pairing_counter: Box::new(Vec::new),
                }),
                t,
            )
        };
        let app = router(state);
        for _ in 0..5 {
            let r = app
                .clone()
                .oneshot(pin_post(
                    "10.8.8.8:6100",
                    Some("192.168.1.9:9420"),
                    Some("ua-y"),
                    r#"{"pin":"0000"}"#,
                ))
                .await
                .unwrap();
            assert_eq!(r.status(), 401);
            let body = axum::body::to_bytes(r.into_body(), 4096).await.unwrap();
            let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(v["error"], "pin_not_set", "未设置 PIN 必须 pin_not_set");
        }
        // 切 PIN 源后正确 PIN 立即可配——若 pin_not_set 被计失败，5 次早已锁定 429
        *pin_slot.lock().unwrap() = Some("1234".to_string());
        let r = app
            .oneshot(pin_post(
                "10.8.8.8:6100",
                Some("192.168.1.9:9420"),
                Some("ua-y"),
                r#"{"pin":"1234"}"#,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "pin_not_set 不计失败——5 次后不得锁定");
    }

    /// 名单收口（矩阵 8）：旧 /pair 直通端点与审批三端点全部死亡——POST 一律 403
    /// （旧路由删除后落内层 fallback；gate 放行名单不再含 /pair*）；/pair/pin/extra
    /// 多余段同样 403（名单精确相等，不是 /pair 前缀）
    #[tokio::test]
    async fn legacy_pair_endpoints_are_closed() {
        let (state, _t) = a3_state(Some("1234"), 3, &[], &[], &[], false);
        let app = router(state);
        for (uri, body) in [
            ("/m/api/v1/pair", r#"{"pin":"1234"}"#),
            ("/m/api/v1/pair/request", r#"{"name":"x"}"#),
            ("/m/api/v1/pair/poll", r#"{"requestId":"r"}"#),
            (
                "/m/api/v1/pair/confirm",
                r#"{"requestId":"r","code":"1234"}"#,
            ),
            ("/m/api/v1/pair/pin/extra", r#"{"pin":"1234"}"#),
        ] {
            let r = app
                .clone()
                .oneshot(http_req(
                    "POST",
                    uri,
                    "10.0.0.9:8000",
                    Some("192.168.1.9:9420"),
                    Some("ua"),
                    None,
                    Some(body),
                ))
                .await
                .unwrap();
            assert_eq!(r.status(), 403, "{uri} 必须死亡（旧端点下线 + 名单收口）");
        }
    }

    // ==== M7 Task 6：session-send / send-info / queue 端点（PIN 门禁内 + 注入器缝）====
    // 零污染：DB 依赖全部经 RemoteState.store = DeviceStore::memory()（Task 6 缝演进：
    // flush_one / 队列 DAO / 审计全走 st.store——生产 Global 语义不变，测试内存库）；
    // 会话快照走注入源；注入器用 FakeInjector。不触真实 ~/.tuvis。

    /// 注入器假体（记录 locate_and_inject 调用）；fail=Some 时恒 Err（直发失败回执用）。
    /// Task 11：补 key_calls 记录 locate_and_send_key 调用（审批按键注入路径）。
    /// F2：补 key_spec_calls 记录 locate_and_send_key_spec 收到的族规格（trait 默认实现
    /// 只透传不记录——覆写以断言「审批路径族传递」；键位经委托旧方法照旧入 key_calls，
    /// 既有键位断言不受影响，fail 语义同源）
    struct FakeInjector {
        calls: std::sync::Mutex<Vec<(u32, String)>>,
        key_calls: std::sync::Mutex<Vec<(u32, String)>>,
        key_spec_calls: std::sync::Mutex<Vec<(u32, String, crate::inject::families::TuiFamily)>>,
        fail: Option<&'static str>,
    }

    impl FakeInjector {
        fn ok() -> std::sync::Arc<Self> {
            std::sync::Arc::new(Self {
                calls: std::sync::Mutex::new(Vec::new()),
                key_calls: std::sync::Mutex::new(Vec::new()),
                key_spec_calls: std::sync::Mutex::new(Vec::new()),
                fail: None,
            })
        }
        fn failing(reason: &'static str) -> std::sync::Arc<Self> {
            std::sync::Arc::new(Self {
                calls: std::sync::Mutex::new(Vec::new()),
                key_calls: std::sync::Mutex::new(Vec::new()),
                key_spec_calls: std::sync::Mutex::new(Vec::new()),
                fail: Some(reason),
            })
        }
        fn recorded(&self) -> Vec<(u32, String)> {
            self.calls.lock().unwrap().clone()
        }
        fn recorded_keys(&self) -> Vec<(u32, String)> {
            self.key_calls.lock().unwrap().clone()
        }
        fn recorded_key_specs(&self) -> Vec<(u32, String, crate::inject::families::TuiFamily)> {
            self.key_spec_calls.lock().unwrap().clone()
        }
    }

    impl crate::inject::engine::Injector for FakeInjector {
        fn name(&self) -> &'static str {
            "fake"
        }
        fn locate_and_inject(&self, pid: u32, text: &str) -> Result<(), String> {
            self.calls.lock().unwrap().push((pid, text.to_string()));
            match self.fail {
                Some(e) => Err(e.to_string()),
                None => Ok(()),
            }
        }
        fn locate_and_send_key(&self, pid: u32, key: &str) -> Result<(), String> {
            self.key_calls.lock().unwrap().push((pid, key.to_string()));
            match self.fail {
                Some(e) => Err(e.to_string()),
                None => Ok(()),
            }
        }
        fn locate_and_send_key_spec(
            &self,
            pid: u32,
            key: &str,
            spec: &crate::inject::families::FamilySpec,
        ) -> Result<(), String> {
            self.key_spec_calls
                .lock()
                .unwrap()
                .push((pid, key.to_string(), spec.family));
            self.locate_and_send_key(pid, key)
        }
    }

    /// 会话夹具（字段形状对齐 file_endpoints_* 既有构造）
    fn inj_sess(
        id: &str,
        agent_type: crate::session::AgentType,
        pid: u32,
        status: crate::session::SessionStatus,
    ) -> crate::session::Session {
        crate::session::Session {
            id: id.into(),
            agent_type,
            project_name: "proj".into(),
            project_path: "/tmp/proj".into(),
            title: None,
            git_branch: None,
            github_url: None,
            status,
            last_message: None,
            last_message_role: None,
            last_message_subagent_report: false,
            flap_from_subagent_activity: false,
            last_activity_at: "2026-09-18T00:00:00Z".into(),
            pid,
            cpu_usage: 0.0,
            active_subagent_count: 0,
            form: crate::session::ProcessForm::Cli,
            jump_supported: false,
            unread: false,
        }
    }

    /// Task 6 专用 state：夹具与 `inject_state_with_probe`（Windows-only）同一套，确认缝缺省
    /// 恒命中（首轮即中，零延迟零等待）+ 指定注入器
    fn inject_state(
        injector: std::sync::Arc<dyn crate::inject::engine::Injector>,
    ) -> Arc<RemoteState> {
        inject_state_full(
            injector,
            std::sync::Arc::new(|_, _, _| true),
            std::sync::Arc::new(|_, _| None),
        )
    }

    /// 对话框在场探针可注入的 state（丁T3 模式切换守卫用例）：其余缝缺省（确认恒
    /// 命中、无对话框）。
    fn inject_state_with_dialog(
        injector: std::sync::Arc<dyn crate::inject::engine::Injector>,
        dialog_probe: std::sync::Arc<crate::remote::server::DialogProbeFn>,
    ) -> Arc<RemoteState> {
        inject_state_full(injector, std::sync::Arc::new(|_, _, _| true), dialog_probe)
    }

    /// inject_state 变体：confirm_probe 可注入（D7/T3 直发未确认端点测试用——
    /// probe 恒 false + Windows 屏读假 pid 无滞留草稿 → Submitted 中性回执）。
    ///
    /// Task 6 专用夹具会话（sess_a Waiting / sess_b Processing / sess_c workbuddy
    /// 黑盒 / sess_d zcode headless / sess_e Waiting 供失败回执测试与直发测试错开会话 /
    /// sess_f Processing 备用 / sess_i Waiting 独占——busy 直发测试专用 / sess_t3
    /// Waiting 独占——D7/T3 直发未确认端点测试专用；另三例 sess_send_ready、
    /// sess_send_qof、sess_send_slash 独占——send 族真投递用例专用，守卫 id 立规。
    /// 另加指定注入器；其余缝与 test_state 同口径（内存库，零接触真实 ~/.tuvis）。
    /// **守卫 id 立规（复检裁决，全测试集适用）**：①守卫持到测尾（或长窗口占用）的
    /// 测试必须占**全测试集唯一** id；②两个夹具不得共享同一 id 字符串——INFLIGHT
    /// 按裸 id 字符串全局占用，跨夹具撞 id 即跨夹具串键（sess_h 曾被本夹具 busy
    /// 测试与 approve_state 的 approve_sends_key 双方使用，实测 2/30 假红；本夹具侧
    /// 已改名 sess_i 让 sess_h 归 approve 族独占；sess_t3 同规——T3 端点测试 ~5s
    /// 轮询窗内守卫全程占用，撞 id 会把对方挤成 queued 假红）
    ///
    /// **平台门控（2026-10-07 存量债清理）**：本夹具的**唯一**消费方是
    /// `#[cfg(windows)]` 的 `send_reports_submitted_when_stamp_missed_no_stuck_draft`
    /// ——它依赖 Windows 屏读（假 pid 无滞留草稿 → submitted 中性回执）这条
    /// 非 Windows 平台不存在的能力。非 Windows 上没有消费方 ⇒ 会被
    /// `cargo clippy --all-targets -- -D warnings` 判 dead_code（实测 macOS 红 1 条）。
    /// 因此把门控**加在夹具本身**（而不是无理由 `#[allow(dead_code)]`）：
    /// 与消费方同 cfg，Windows 上门控开启、夹具照旧被使用，其余平台则不编译。
    #[cfg(windows)]
    fn inject_state_with_probe(
        injector: std::sync::Arc<dyn crate::inject::engine::Injector>,
        confirm_probe: std::sync::Arc<crate::remote::server::ConfirmProbeFn>,
    ) -> Arc<RemoteState> {
        inject_state_full(injector, confirm_probe, std::sync::Arc::new(|_, _| None))
    }

    /// 全参数建造器（丁T3 起三缝可分）：确认探针 + 对话框在场探针。
    fn inject_state_full(
        injector: std::sync::Arc<dyn crate::inject::engine::Injector>,
        confirm_probe: std::sync::Arc<crate::remote::server::ConfirmProbeFn>,
        dialog_probe: std::sync::Arc<crate::remote::server::DialogProbeFn>,
    ) -> Arc<RemoteState> {
        let sessions = vec![
            inj_sess(
                "sess_a",
                crate::session::AgentType::Claude,
                11,
                crate::session::SessionStatus::Waiting,
            ),
            inj_sess(
                "sess_b",
                crate::session::AgentType::Claude,
                12,
                crate::session::SessionStatus::Processing,
            ),
            inj_sess(
                "sess_c",
                crate::session::AgentType::WorkBuddy,
                13,
                crate::session::SessionStatus::Idle,
            ),
            inj_sess(
                "sess_d",
                crate::session::AgentType::ZCode,
                14,
                crate::session::SessionStatus::Waiting,
            ),
            inj_sess(
                "sess_e",
                crate::session::AgentType::Claude,
                15,
                crate::session::SessionStatus::Waiting,
            ),
            inj_sess(
                "sess_f",
                crate::session::AgentType::Claude,
                16,
                crate::session::SessionStatus::Processing,
            ),
            inj_sess(
                "sess_i",
                crate::session::AgentType::Claude,
                19,
                crate::session::SessionStatus::Waiting,
            ),
            {
                // D7/T3 直发未确认端点测试独占（守卫 id 立规，见本函数 doc）
                inj_sess(
                    "sess_t3",
                    crate::session::AgentType::Claude,
                    25,
                    crate::session::SessionStatus::Waiting,
                )
            },
            {
                // 签名开关测试独占（守卫 id 立规，2026-10-05）：send_composes_signature_
                // when_setting_on 专用 Waiting 会话（pid 26）
                inj_sess(
                    "sess_sig",
                    crate::session::AgentType::Claude,
                    26,
                    crate::session::SessionStatus::Waiting,
                )
            },
            // ===== send 族真投递用例独占会话（守卫 id 立规①/②，2026-10-05 复检）=====
            // 三例（send_delivers_when_input_ready / send_queue_only_false_keeps_direct_delivery
            // / slash_message_bare_injects_and_audits_slash）原先共用 `sess_a`：INFLIGHT 按
            // 裸 id 全局占用 → 并行跑时互抢守卫，先到者持守卫，后到者收到 queued
            // （Deferred）假红——实测（warm 全量 `cargo test --lib`）：基线 1/6 假红，
            // 测试集增删带来的调度漂移会把假红推成必发；skip 对照（仅跳过这三例）6/6 绿，
            // 各占唯一 id 后同样 6/6 绿。`sess_a` 保留给其余用例（GET 面 / 纯入队路径）。
            inj_sess(
                "sess_send_ready",
                crate::session::AgentType::Claude,
                31,
                crate::session::SessionStatus::Waiting,
            ),
            inj_sess(
                "sess_send_qof",
                crate::session::AgentType::Claude,
                32,
                crate::session::SessionStatus::Waiting,
            ),
            inj_sess(
                "sess_send_slash",
                crate::session::AgentType::Claude,
                33,
                crate::session::SessionStatus::Waiting,
            ),
        ];
        Arc::new(RemoteState {
            ui_config_source: Box::new(|| None),
            subagent_source: std::collections::HashMap::new(),
            subagent_message_source: std::collections::HashMap::new(),
            target_evidence: no_target_evidence(),
            capability_table: crate::inject::capability::new_table(),
            session_source: Box::new(move || crate::session::SessionsResponse {
                sessions: sessions.clone(),
                total_count: sessions.len(),
                waiting_count: 0,
            }),
            store: crate::remote::pairing::DeviceStore::memory(),
            injector,
            // R5 一键 resume spawn 缝（Task 11）：本夹具不触 session-open，注 no-op 桩
            resume_spawner: std::sync::Arc::new(|_: &crate::inject::resume::SpawnSpec| Ok(())),
            // C6：create 缝束缺省 stub（任务簿+发现/pid/工具探测/步距缝），仅
            // session-create 用例就地覆盖
            create_hub: create_hub_stub(),
            archive_source: Box::new(Vec::new),
            archive_delete: std::sync::Arc::new(|_: Option<&str>| 0usize),
            // A1 写入确认缝（M9R Task 5）：参数化（inject_state 缺省恒命中）
            confirm_probe,
            // 丁T3：对话框在场探针（同参数化）——缺省 None = 无法判定 ⇒ 控制类注入
            // 照常投递；「在场即拒」用例经 inject_state_with_dialog 注入假体
            dialog_probe,
            screen_probe: std::sync::Arc::new(|_, _| None),
            host_source: Box::new(|| serde_json::Value::Null),
            message_source: Box::new(|_, _, _| Err("测试桩：未注入内容源".to_string())),
            path_source: Box::new(|_, _, _| (Vec::new(), false)),
            watcher_tx: tokio::sync::broadcast::channel(64).0,
            board_hidden_ids: Box::new(Vec::new),
            board_hidden_hide: std::sync::Arc::new(|_| 0usize),
            board_hidden_unhide: std::sync::Arc::new(|_| 0usize),
            unread_mark_read: std::sync::Arc::new(|_, _| ()),
            session_close: std::sync::Arc::new(|_| Ok(())),
            sse_registry: Arc::new(SseRegistry::default()),
            max_devices_source: Box::new(|| 3),
            pin_limiter: std::sync::Mutex::new(crate::remote::pin::PinRateLimiter::new()),
            global_pin_limiter: std::sync::Mutex::new(crate::remote::pin::PinRateLimiter::global()),
            pin_source: Box::new(|| Some("1234".to_string())),
            now_source: Box::new(|| chrono::Utc::now().timestamp_millis()),
            via_hosts_source: Box::new(|| None),
            rate_bucket_channels_source: Box::new(Vec::new),
            home_source: Box::new(|| None),
            // C7：配对计数缝缺省空表（既有用例零影响；配对打标/提示用例就地覆盖）
            pairing_counter: Box::new(Vec::new),
        })
    }

    /// 预置带花名的有效设备（丁T3 裁2 起 `[mobile <名>]` 是**尾**签名；审计设备名
    /// 列同源）
    fn persist_named_device(state: &Arc<RemoteState>, id: &str, name: &str) {
        let now = chrono::Utc::now().timestamp_millis();
        state.store.with(|c| {
            crate::remote::pairing::persist_device(
                c,
                &crate::remote::pairing::NewDevice {
                    id: id.into(),
                    name: name.into(),
                    ua: format!("ua-{id}"),
                    origin_ip: format!("ip-{id}"),
                    via: String::new(),
                    paired_at: now,
                },
            )
            .unwrap();
        });
    }

    // ==== M8 Task 11：session-approve-options / session-approve 端点 ====
    // 零污染：设备表/队列/审计/KV 全走 RemoteState.store = DeviceStore::memory()；
    // 会话快照与注入器走注入缝；approve 映射 KV 经 store 缝读取（生产 Global=全局库
    // 同语义、测试 memory 自建库，缺省键回默认表），定制映射由各测试在内存库 seed。
    // 不触真实 ~/.tuvis。

    /// Task 11 专用 state：会话夹具与 inject_state 同一套（sess_a Waiting / **sess_t5m**
    /// Processing——原 id 叫 sess_b，但它与 `inject_state` 的 sess_b **撞了裸 id**
    /// 且本族的 `approve_endpoints_honor_wait_mark` 会真投递：INFLIGHT 按裸 id 全局
    /// 占用 → 并行跑时两条测试互抢守卫，`queue_jump_and_retract` 因此偶发假红
    /// （丁T5 的变异验证轮复现：单跑恒绿、并行 3/3 红）。按「守卫 id 立规」② 改名
    /// 独占。 / sess_d zcode Waiting 无映射 / sess_e-f 备用），但 sess_a 可携带
    /// last_message 供 detect 命中（inj_sess 夹具的 last_message 恒 None——approve_state
    /// 局部变体按需补设）；另加 sess_g（Waiting，独占 id）：in-flight 守卫按 session_id
    /// 全局占用，审批 POST 测试错开 id 防并行挤占（Task 6 夹具同规）。sess_h（Waiting，
    /// 独占 id，与 sess_a 同携命中 last_message）：approve_sends_key 独占——复检终修后
    /// sess_h 全测试集唯一归本族（inject_state 侧已改名 sess_i）。sess_j（Waiting，
    /// 独占 id）：audit_action_vocab 独占（Task 7 P3c 审计动作词表）。sess_p（Waiting
    /// claude，独占 id）：approve 忙让位回归独占（F1，守卫持到测尾）。sess_q（Waiting
    /// codex，独占 id）：审批族规格传递断言独占（F2）——p/q 为全测试集未占用字母
    /// （sess_m-sess_o 已归 open_state 族）。其余缝与
    /// inject_state 同口径（内存库，零接触真实 ~/.tuvis）。
    /// **守卫 id 立规（复检裁决，全测试集适用）**：①守卫持到测尾的测试必须占**全测试集
    /// 唯一** id；②两个夹具不得共享同一 id 字符串——INFLIGHT 按裸 id 字符串全局占用，
    /// 跨夹具撞 id 即跨夹具串键（详见 inject_state doc）
    fn approve_state(
        injector: std::sync::Arc<dyn crate::inject::engine::Injector>,
        sess_a_last: Option<&str>,
    ) -> Arc<RemoteState> {
        approve_state_with_dialog(
            injector,
            sess_a_last,
            std::sync::Arc::new(|_, _| None),
            None,
        )
    }

    /// 丁T3 F4-2：approve_state + 可注入对话框探针（审批侧 `read_dialog_options` 经
    /// 该缝取屏读结论——补上 T5 的 dialog 分支在门禁内的自动化证据：真实屏读需要
    /// conhost，CI 恒 None 时那两条断言只在有窗口的机器上才走得到）。
    #[allow(clippy::too_many_arguments)]
    fn approve_state_with_dialog(
        injector: std::sync::Arc<dyn crate::inject::engine::Injector>,
        sess_a_last: Option<&str>,
        dialog_probe: std::sync::Arc<crate::remote::server::DialogProbeFn>,
        // 2026-10-04 审批卡不出修复：可选的消息尾页桩——Some(page) 时 message_source
        // 返回它（驱动 claude 计划预期态/plan 聚合判据）；None 保持既有 Err 桩
        // （既有用例零感知）
        tail_page: Option<Vec<crate::remote::content::SessionMessage>>,
    ) -> Arc<RemoteState> {
        let sessions = vec![
            {
                let mut s = inj_sess(
                    "sess_a",
                    crate::session::AgentType::Claude,
                    11,
                    crate::session::SessionStatus::Waiting,
                );
                s.last_message = sess_a_last.map(str::to_string);
                s
            },
            inj_sess(
                "sess_t5m",
                crate::session::AgentType::Claude,
                12,
                crate::session::SessionStatus::Processing,
            ),
            inj_sess(
                "sess_c",
                crate::session::AgentType::WorkBuddy,
                13,
                crate::session::SessionStatus::Idle,
            ),
            inj_sess(
                "sess_d",
                crate::session::AgentType::ZCode,
                14,
                crate::session::SessionStatus::Waiting,
            ),
            inj_sess(
                "sess_e",
                crate::session::AgentType::Claude,
                15,
                crate::session::SessionStatus::Waiting,
            ),
            inj_sess(
                "sess_f",
                crate::session::AgentType::Claude,
                16,
                crate::session::SessionStatus::Processing,
            ),
            inj_sess(
                "sess_g",
                crate::session::AgentType::Claude,
                17,
                crate::session::SessionStatus::Waiting,
            ),
            {
                // Important 3：approve_sends_key 独占会话——last_message 与 sess_a 同源
                // 命中串（detect 依赖），pid 独立（18）供按键注入断言
                let mut s = inj_sess(
                    "sess_h",
                    crate::session::AgentType::Claude,
                    18,
                    crate::session::SessionStatus::Waiting,
                );
                s.last_message = sess_a_last.map(str::to_string);
                s
            },
            // Task 7 audit_action_vocab 独占会话（Waiting claude；全测试集唯一 id——
            // 守卫 id 立规；POST 不走 detect，无需 last_message）
            inj_sess(
                "sess_j",
                crate::session::AgentType::Claude,
                20,
                crate::session::SessionStatus::Waiting,
            ),
            {
                // M9R Task 10 probe_pending_strict_policy 独占会话（Waiting codex，
                // last_message 恒为 codex 补丁审批框标题原文——证明 detect 命中下严格档
                // 仍压为不可批）；全测试集唯一 id（守卫 id 立规）
                let mut s = inj_sess(
                    "sess_k",
                    crate::session::AgentType::Codex,
                    21,
                    crate::session::SessionStatus::Waiting,
                );
                s.last_message = Some("Would you like to make the following edits?".to_string());
                s
            },
            {
                // M9R 质量评审补锁 probe_pending_hint_even_on_detect_miss 独占会话
                // （Waiting codex，last_message 与任何 marker 无关——证明 detect 未命中
                // 下严格档 hint 仍下发，短路序定格）；全测试集唯一 id（守卫 id 立规）
                let mut s = inj_sess(
                    "sess_l",
                    crate::session::AgentType::Codex,
                    22,
                    crate::session::SessionStatus::Waiting,
                );
                s.last_message = Some("无关文本".to_string());
                s
            },
            {
                // F1 approve 忙让位测试独占会话（Waiting claude，全测试集唯一 id——守卫
                // id 立规：本测守卫持到测尾；sess_p 为全测试集未占用字母，open_state
                // 夹具的 sess_m/sess_n 已被 session-open 族占用，避开）；last_message
                // 与 sess_a 同源（POST 审批不消费 detect，保持夹具一致性而已）
                let mut s = inj_sess(
                    "sess_p",
                    crate::session::AgentType::Claude,
                    23,
                    crate::session::SessionStatus::Waiting,
                );
                s.last_message = sess_a_last.map(str::to_string);
                s
            },
            // F2 审批族规格测试独占会话（Waiting codex，全测试集唯一 id，避让 open_state
            // 既有 sess_m-sess_o）：POST 审批只查 Waiting+映射+选项（不消费 detect），
            // last_message 留 None 即可——缺省 KV 回默认表，codex 映射（verified 0.154.0）
            // 非严格档，approve="y" 可出键
            inj_sess(
                "sess_q",
                crate::session::AgentType::Codex,
                24,
                crate::session::SessionStatus::Waiting,
            ),
            // ===== L13 靶向闸用例独占会话（守卫 id 立规①/②，2026-10-05 复审补口）=====
            // 批准/拒绝两条真投递用例各占唯一 id：与 approve_sends_key(sess_h) /
            // approve_reject_audits_reject(sess_g) 错开——INFLIGHT 按裸 id 全局占用，
            // 撞 id 会让对方收到「投递进行中」假红（本批实测：并行跑即红）。
            // last_message 与 sess_a/sess_h 同源（detect 命中，映射键位才下发）
            {
                let mut s = inj_sess(
                    "sess_l13ap",
                    crate::session::AgentType::Claude,
                    81,
                    crate::session::SessionStatus::Waiting,
                );
                s.last_message = sess_a_last.map(str::to_string);
                s
            },
            {
                let mut s = inj_sess(
                    "sess_l13rj",
                    crate::session::AgentType::Claude,
                    82,
                    crate::session::SessionStatus::Waiting,
                );
                s.last_message = sess_a_last.map(str::to_string);
                s
            },
        ];
        Arc::new(RemoteState {
            ui_config_source: Box::new(|| None),
            subagent_source: std::collections::HashMap::new(),
            subagent_message_source: std::collections::HashMap::new(),
            target_evidence: no_target_evidence(),
            capability_table: crate::inject::capability::new_table(),
            session_source: Box::new(move || crate::session::SessionsResponse {
                sessions: sessions.clone(),
                total_count: sessions.len(),
                waiting_count: 0,
            }),
            store: crate::remote::pairing::DeviceStore::memory(),
            injector,
            // R5 一键 resume spawn 缝（Task 11）：本夹具不触 session-open，注 no-op 桩
            resume_spawner: std::sync::Arc::new(|_: &crate::inject::resume::SpawnSpec| Ok(())),
            // C6：create 缝束缺省 stub（任务簿+发现/pid/工具探测/步距缝），仅
            // session-create 用例就地覆盖
            create_hub: create_hub_stub(),
            archive_source: Box::new(Vec::new),
            archive_delete: std::sync::Arc::new(|_: Option<&str>| 0usize),
            // A1 写入确认缝（M9R Task 5）：测试恒命中（首轮即中，零延迟零等待）
            confirm_probe: std::sync::Arc::new(|_, _, _| true),
            // 丁T3 F4-2：对话框探针参数化（缺省「无法判定」；dialog 分支用例经
            // approve_state_with_dialog 注入**真机屏幕原文**假体）
            dialog_probe,
            screen_probe: std::sync::Arc::new(|_, _| None),
            host_source: Box::new(|| serde_json::Value::Null),
            message_source: Box::new(move |_, _, _| match &tail_page {
                Some(p) => Ok(crate::remote::content::MessagesPage {
                    messages: p.clone(),
                    truncated: false,
                }),
                None => Err("测试桩：未注入内容源".to_string()),
            }),
            path_source: Box::new(|_, _, _| (Vec::new(), false)),
            watcher_tx: tokio::sync::broadcast::channel(64).0,
            board_hidden_ids: Box::new(Vec::new),
            board_hidden_hide: std::sync::Arc::new(|_| 0usize),
            board_hidden_unhide: std::sync::Arc::new(|_| 0usize),
            unread_mark_read: std::sync::Arc::new(|_, _| ()),
            session_close: std::sync::Arc::new(|_| Ok(())),
            sse_registry: Arc::new(SseRegistry::default()),
            max_devices_source: Box::new(|| 3),
            pin_limiter: std::sync::Mutex::new(crate::remote::pin::PinRateLimiter::new()),
            global_pin_limiter: std::sync::Mutex::new(crate::remote::pin::PinRateLimiter::global()),
            pin_source: Box::new(|| Some("1234".to_string())),
            now_source: Box::new(|| chrono::Utc::now().timestamp_millis()),
            via_hosts_source: Box::new(|| None),
            rate_bucket_channels_source: Box::new(Vec::new),
            home_source: Box::new(|| None),
            // C7：配对计数缝缺省空表（既有用例零影响；配对打标/提示用例就地覆盖）
            pairing_counter: Box::new(Vec::new),
        })
    }

    /// **签名开关开**（2026-10-05 用户裁决的另一半）：settings KV
    /// `remote_message_signature=on` → compose 产物带尾部 `[mobile 测试设备]` 签名。
    /// 经 store.with 走本测自己的内存库（零污染；开关读取单点的接线验证）。
    #[tokio::test]
    #[cfg_attr(
        not(any(windows, target_os = "macos")),
        ignore = "注入平台门（inject/routing.rs）：仅 Windows/macOS 可注入，本测走注入链"
    )]
    async fn send_composes_signature_when_setting_on() {
        let fake = FakeInjector::ok();
        let state = inject_state(fake.clone());
        persist_named_device(&state, "mm", "测试设备");
        state.store.with(|conn| {
            crate::database::dao::settings::set_setting_conn(conn, "remote_message_signature", "on")
        });
        let app = router(state.clone());
        let r = app
            .oneshot(req(
                "POST",
                "/m/api/v1/session-send",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_sig","text":"带签名的一句话"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        assert!(
            fake.recorded()
                .iter()
                .any(|(_, t)| t.ends_with(" [mobile 测试设备]")),
            "开关开 → 注入文本带尾部设备签名：{:?}",
            fake.recorded()
        );
    }

    /// 直发可输入态（Waiting）：200 delivered + 注入器收到 compose 产物（裁决 6 归一）
    /// + 审计 action=send result=ok channel=fake + 队列无 pending 残留
    #[tokio::test]
    #[cfg_attr(
        not(any(windows, target_os = "macos")),
        ignore = "注入平台门（inject/routing.rs）：仅 Windows/macOS 可注入，本测走注入链"
    )]
    async fn send_delivers_when_input_ready() {
        let fake = FakeInjector::ok();
        let state = inject_state(fake.clone());
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state.clone());
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-send",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_send_ready","text":"你好\n继续"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(
            r.headers()
                .get("cache-control")
                .and_then(|v| v.to_str().ok()),
            Some("no-store"),
            "发送回执是门禁下私有数据，禁止中间层缓存"
        );
        let body = body_string(r).await;
        assert!(
            body.contains("\"status\":\"delivered\""),
            "可输入态直发应 delivered：{body}"
        );
        // 注入器收到 (pid=31, "你好\n继续")——真实换行归一为字面 \n（裁决 6）。
        // 签名默认关（2026-10-05 用户裁决，KV 未设 = off）→ 裸正文；开关开的形态
        // 由 send_composes_signature_when_setting_on 专测覆盖
        assert_eq!(
            fake.recorded(),
            vec![(31u32, "你好\\n继续".to_string())],
            "直发必须携带归一正文（签名默认关=裸注入）"
        );
        // 审计：最新一条 action=send result=ok channel=fake
        //（flush_one 落账并行写的 action=flush 审计紧随其后——Task 6 最小演进：直发终态
        // 由端点另行落 send 审计，flush 落账审计保持 Task 5 原样）
        let audits = state
            .store
            .with(|c| crate::database::dao::write_audit::recent_conn(c, 10));
        assert_eq!(audits[0].action, "send");
        assert_eq!(audits[0].result, "ok");
        assert_eq!(audits[0].channel, "fake");
        assert_eq!(audits[0].session_id, "sess_send_ready");
        assert_eq!(audits[0].device_name, "测试设备");
        // 队列无残留（直发行 mark_sent 退出 pending）
        let r = app
            .oneshot(req(
                "GET",
                "/m/api/v1/session-queue?session_id=sess_send_ready",
                Some("mam_device=mm"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        assert!(
            body_string(r).await.contains("\"items\":[]"),
            "直发后队列不得有 pending 残留"
        );
    }

    // ==== C7：配对不确定信号（spec §5）——端点层打标 + 投递回执提示，不拦截 ====
    // 与表单黄字（C6 hasActiveSession/activeTools = ≥1 活跃会话）是两个分层信号：
    // 本组锁「同工具同项目 ≥2 运行进程」的 pairingAmbiguous / pairingHint。

    /// C7 夹具：test_state 就地换 session_source / pairing_counter（Arc::get_mut
    /// 先例见 archive_api_tests::archive_state——比整份 RemoteState 字面量轻 30+ 行）。
    /// 其余缝与 test_state 同口径（内存库、空事件通道、确认恒命中等）。
    fn pairing_state(
        sessions: Vec<crate::session::Session>,
        processes: Vec<(String, String)>,
    ) -> Arc<RemoteState> {
        let mut st = test_state();
        let s = Arc::get_mut(&mut st).expect("test_state 独占引用");
        s.session_source = Box::new(move || crate::session::SessionsResponse {
            sessions: sessions.clone(),
            total_count: sessions.len(),
            waiting_count: 0,
        });
        s.pairing_counter = Box::new(move || processes.clone());
        st
    }

    /// C7 断言 1（/sessions 打标）：fake 快照两条同工具同项目会话 + 进程计数 ≥2 →
    /// 两会话 `pairingAmbiguous=true`；单会话（进程计数 1 < 2 阈值）→ false。
    /// 假表混入工具/项目大小写与尾分隔符变体，锁 (tool, project) 归一与桌面门同口径；
    /// 另加异工具会话证明打标按会话键逐条判定（非同工具连坐）。
    #[tokio::test]
    async fn sessions_flags_pairing_ambiguous_per_session() {
        // ① 两条同工具同项目（claude/proj）+ 进程表两条同键（大小写/尾分隔符变体）
        let st = pairing_state(
            vec![
                inj_sess(
                    "c7_a",
                    crate::session::AgentType::Claude,
                    31,
                    crate::session::SessionStatus::Idle,
                ),
                inj_sess(
                    "c7_b",
                    crate::session::AgentType::Claude,
                    32,
                    crate::session::SessionStatus::Idle,
                ),
            ],
            vec![
                ("Claude".into(), "PROJ".into()),
                ("claude".into(), "proj\\".into()),
            ],
        );
        persist_device(&st, "c7");
        let r = router(st)
            .oneshot(req(
                "GET",
                "/m/api/v1/sessions",
                Some("mam_device=c7"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        let arr = v["sessions"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
        for s in arr {
            assert_eq!(
                s["pairingAmbiguous"], true,
                "同工具同项目 ≥2 运行进程 → 该键会话全部打标：{s}"
            );
        }
        // ② 单会话 + 进程表仅一条同键（1 < 2）→ false（字段仍恒在场）
        let st = pairing_state(
            vec![inj_sess(
                "c7_c",
                crate::session::AgentType::Claude,
                33,
                crate::session::SessionStatus::Idle,
            )],
            vec![("claude".into(), "proj".into())],
        );
        persist_device(&st, "c7");
        let r = router(st)
            .oneshot(req(
                "GET",
                "/m/api/v1/sessions",
                Some("mam_device=c7"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(
            v["sessions"][0]["pairingAmbiguous"], false,
            "单进程不构成配对不确定（阈值严格 ≥2）"
        );
        // ③ 异工具不连坐：进程表只歧义 claude/proj，codex/proj 会话恒 false
        let st = pairing_state(
            vec![
                inj_sess(
                    "c7_d",
                    crate::session::AgentType::Codex,
                    34,
                    crate::session::SessionStatus::Idle,
                ),
                inj_sess(
                    "c7_e",
                    crate::session::AgentType::Claude,
                    35,
                    crate::session::SessionStatus::Idle,
                ),
            ],
            vec![
                ("claude".into(), "proj".into()),
                ("claude".into(), "proj".into()),
            ],
        );
        persist_device(&st, "c7");
        let r = router(st)
            .oneshot(req(
                "GET",
                "/m/api/v1/sessions",
                Some("mam_device=c7"),
                None,
            ))
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["sessions"][0]["pairingAmbiguous"], false, "异工具不连坐");
        assert_eq!(v["sessions"][1]["pairingAmbiguous"], true, "同键会话打标");
    }

    /// C7 断言 2（session-send 回执提示）：命中同工具同项目 ≥2 运行进程 → delivered
    /// 回执附 `pairingHint=true` 且**投递照常**（不拦截，status 语义不变）；未命中
    /// （同键进程 1 条）→ delivered + `pairingHint=false`（恒在场 = 无提示）。
    #[tokio::test]
    #[cfg_attr(
        not(any(windows, target_os = "macos")),
        ignore = "注入平台门（inject/routing.rs）：仅 Windows/macOS 可注入，本测走注入链"
    )]
    async fn session_send_receipt_carries_pairing_hint_without_blocking() {
        // ① 命中：同键进程 2 条 → delivered + pairingHint=true + 注入器确实收到
        let fake = FakeInjector::ok();
        let mut st = pairing_state(
            vec![inj_sess(
                "c7_send",
                crate::session::AgentType::Claude,
                41,
                crate::session::SessionStatus::Waiting,
            )],
            vec![
                ("claude".into(), "proj".into()),
                ("claude".into(), "proj".into()),
            ],
        );
        Arc::get_mut(&mut st).unwrap().injector = fake.clone();
        persist_device(&st, "c7");
        let r = router(st)
            .oneshot(req(
                "POST",
                "/m/api/v1/session-send",
                Some("mam_device=c7"),
                Some(r#"{"sessionId":"c7_send","text":"你好"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["status"], "delivered", "提示不改变投递语义（不拦截）");
        assert_eq!(v["pairingHint"], true, "命中配对不确定 → 回执附提示");
        assert_eq!(fake.recorded().len(), 1, "不拦截：注入照常发生");
        // ② 未命中：同键进程仅 1 条 → delivered + pairingHint=false
        let fake2 = FakeInjector::ok();
        let mut st2 = pairing_state(
            vec![inj_sess(
                "c7_send2",
                crate::session::AgentType::Claude,
                42,
                crate::session::SessionStatus::Waiting,
            )],
            vec![("claude".into(), "proj".into())],
        );
        Arc::get_mut(&mut st2).unwrap().injector = fake2.clone();
        persist_device(&st2, "c7");
        let r = router(st2)
            .oneshot(req(
                "POST",
                "/m/api/v1/session-send",
                Some("mam_device=c7"),
                Some(r#"{"sessionId":"c7_send2","text":"你好"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["status"], "delivered");
        assert_eq!(
            v["pairingHint"], false,
            "未命中 = 无提示（字段恒在场 false）"
        );
        assert_eq!(fake2.recorded().len(), 1, "未命中同样照常投递");
    }

    /// 运行中（Processing 黄态）：200 queued + itemId/position + 审计 action=queue +
    /// 注入器不被调用 + GET session-queue 可见该项（content 为 compose 产物）
    #[tokio::test]
    #[cfg_attr(
        not(any(windows, target_os = "macos")),
        ignore = "注入平台门（inject/routing.rs）：仅 Windows/macOS 可注入，本测走注入链"
    )]
    async fn send_queues_when_running() {
        let fake = FakeInjector::ok();
        let state = inject_state(fake.clone());
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state.clone());
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-send",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_b","text":"排队消息"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["status"], "queued");
        let item_id = v["itemId"].as_i64().expect("queued 回执必须带 itemId");
        assert!(item_id > 0, "itemId 应为入队行 id");
        assert_eq!(v["position"], 1, "首条排队 position=1");
        assert!(
            fake.recorded().is_empty(),
            "运行中会话不得直发（等 agent 交回输入框）"
        );
        let audits = state
            .store
            .with(|c| crate::database::dao::write_audit::recent_conn(c, 10));
        assert_eq!(audits[0].action, "queue");
        assert_eq!(audits[0].result, "ok");
        // GET session-queue 可见该项（content = 入队时 compose 完成的最终文本）
        let r = app
            .oneshot(req(
                "GET",
                "/m/api/v1/session-queue?session_id=sess_b",
                Some("mam_device=mm"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(
            r.headers()
                .get("cache-control")
                .and_then(|v| v.to_str().ok()),
            Some("no-store")
        );
        let body = body_string(r).await;
        assert!(
            body.contains(&format!("\"id\":{item_id}"))
                && body.contains("排队消息")
                && body.contains("\"position\":1")
                && body.contains("\"enqueuedAt\":"),
            "排队视图应含 id/content/enqueuedAt/position，实际 {body}"
        );
    }

    /// D6 修改重发只入队：可输入态（Waiting）+ queueOnly=true → 回执 queued（不直发），
    /// 注入器不被调用；审计 action=queue（与「运行中留队」同口径，无 send/failed 直发
    /// 审计）；条目在 GET session-queue 可见（flush 循环转闲按序自动放行——与普通
    /// 队列项同权）
    #[tokio::test]
    #[cfg_attr(
        not(any(windows, target_os = "macos")),
        ignore = "注入平台门（inject/routing.rs）：仅 Windows/macOS 可注入，本测走注入链"
    )]
    async fn send_queue_only_skips_direct_delivery() {
        let fake = FakeInjector::ok();
        let state = inject_state(fake.clone());
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state.clone());
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-send",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_a","text":"修改后重发","queueOnly":true}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["status"], "queued", "queueOnly=true 可输入态也不得直发");
        let item_id = v["itemId"].as_i64().expect("queued 回执必须带 itemId");
        assert_eq!(v["position"], 1, "首条排队 position=1");
        assert!(
            fake.recorded().is_empty(),
            "queueOnly=true 必须跳过直发尝试（防「文件说闲、TUI 实忙」窗口变相插队）"
        );
        // 审计：落 ⑦ 留队臂，action=queue 与「运行中留队」同口径
        let audits = state
            .store
            .with(|c| crate::database::dao::write_audit::recent_conn(c, 10));
        assert_eq!(audits[0].action, "queue");
        assert_eq!(audits[0].result, "ok");
        // 条目在队列可见（不携带 queueOnly 标志，等 flush 循环自动放行）
        let r = app
            .oneshot(req(
                "GET",
                "/m/api/v1/session-queue?session_id=sess_a",
                Some("mam_device=mm"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let body = body_string(r).await;
        assert!(
            body.contains(&format!("\"id\":{item_id}")) && body.contains("修改后重发"),
            "queueOnly 条目应留在队列等自动放行：{body}"
        );
    }

    /// D6 防回归对照：queueOnly 显式 false（与缺省同义）→ 可输入态直发行为不变
    /// （200 delivered + 注入器收到 compose 产物），既有调用面零漂移
    #[tokio::test]
    #[cfg_attr(
        not(any(windows, target_os = "macos")),
        ignore = "注入平台门（inject/routing.rs）：仅 Windows/macOS 可注入，本测走注入链"
    )]
    async fn send_queue_only_false_keeps_direct_delivery() {
        let fake = FakeInjector::ok();
        let state = inject_state(fake.clone());
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state.clone());
        let r = app
            .oneshot(req(
                "POST",
                "/m/api/v1/session-send",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_send_qof","text":"普通发送","queueOnly":false}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let body = body_string(r).await;
        assert!(
            body.contains("\"status\":\"delivered\""),
            "queueOnly=false 可输入态照旧直发：{body}"
        );
        assert_eq!(
            fake.recorded(),
            vec![(32u32, "普通发送".to_string())],
            "queueOnly=false 直发行为不得漂移"
        );
    }

    // ===== 丁T3 裁2：斜杠命令裸注入 + 审计 action=slash（问题 8）=====

    /// **斜杠命令直发（可输入态）**：注入文本必须**裸**（无签名——问题 8 实锤：
    /// 前缀会毁掉 `/permissons`；后缀同样破坏命令与参数），审计 action=**slash**
    /// 且 device_name 在账（裁2：终端不留痕，溯源只此一处）。
    #[tokio::test]
    #[cfg_attr(
        not(any(windows, target_os = "macos")),
        ignore = "注入平台门（inject/routing.rs）：仅 Windows/macOS 可注入，本测走注入链"
    )]
    async fn slash_message_bare_injects_and_audits_slash() {
        let fake = FakeInjector::ok();
        let state = inject_state(fake.clone());
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state.clone());
        let r = app
            .oneshot(req(
                "POST",
                "/m/api/v1/session-send",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_send_slash","text":"/permissions"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let body = body_string(r).await;
        assert!(body.contains("\"status\":\"delivered\""), "{body}");
        assert_eq!(
            fake.recorded(),
            vec![(33u32, "/permissions".to_string())],
            "斜杠命令必须裸注入（不加签名——签名会破坏命令解析）"
        );
        let audits = state
            .store
            .with(|c| crate::database::dao::write_audit::recent_conn(c, 10));
        // 端点行（action=slash）在前，flush 落账行（机制记录 action=flush）紧随其后
        assert_eq!(
            audits[0].action, "slash",
            "端点审计 action=slash（裁2 词表新增）"
        );
        assert_eq!(audits[0].result, "ok");
        assert_eq!(
            audits[0].device_name, "测试设备",
            "slash 的溯源靠设备名在账"
        );
        assert_eq!(audits[0].session_id, "sess_send_slash");
        assert_eq!(
            audits[0].summary, "/permissions",
            "摘要即裸命令原文（无签名可读）"
        );
        // 机制行保持既有词表（投递路径判据不因 slash 消失——见 audit_action_for 注）
        assert_eq!(audits[1].action, "flush");
    }

    /// **斜杠命令走队列（运行中会话）**：同样裸注入入队（队列存的就是 compose 产物）
    /// + 端点审计 action=slash（不是 queue——用户动作是发命令，投递机制另记）。
    #[tokio::test]
    #[cfg_attr(
        not(any(windows, target_os = "macos")),
        ignore = "注入平台门（inject/routing.rs）：仅 Windows/macOS 可注入，本测走注入链"
    )]
    async fn slash_message_queued_keeps_bare_form_and_slash_action() {
        let fake = FakeInjector::ok();
        let state = inject_state(fake.clone());
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state.clone());
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-send",
                Some("mam_device=mm"),
                // sess_b = Processing（留队臂）
                Some(r#"{"sessionId":"sess_b","text":"/plan"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["status"], "queued");
        assert!(fake.recorded().is_empty(), "留队臂不投递");
        let audits = state
            .store
            .with(|c| crate::database::dao::write_audit::recent_conn(c, 10));
        assert_eq!(audits[0].action, "slash", "留队臂的端点审计同样记 slash");
        // 队内内容 = 裸命令（flush 放行时直接照发，不会再加工）
        let r = app
            .oneshot(req(
                "GET",
                "/m/api/v1/session-queue?session_id=sess_b",
                Some("mam_device=mm"),
                None,
            ))
            .await
            .unwrap();
        let body = body_string(r).await;
        assert!(
            body.contains("\"content\":\"/plan\""),
            "队列条目必须是裸命令（flush 直接照发）：{body}"
        );
        assert!(
            !body.contains("[mobile"),
            "斜杠命令的队列条目不得带签名：{body}"
        );
    }

    /// D6：queueOnly=true 且会话 running（Processing 黄态）→ 照常入队（与普通入队
    /// 同路径同回执同审计），注入器不被调用
    #[tokio::test]
    #[cfg_attr(
        not(any(windows, target_os = "macos")),
        ignore = "注入平台门（inject/routing.rs）：仅 Windows/macOS 可注入，本测走注入链"
    )]
    async fn send_queue_only_when_running_queues_normally() {
        let fake = FakeInjector::ok();
        let state = inject_state(fake.clone());
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state.clone());
        let r = app
            .oneshot(req(
                "POST",
                "/m/api/v1/session-send",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_b","text":"运行中也入队","queueOnly":true}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["status"], "queued");
        assert_eq!(v["position"], 1, "与普通入队同回执形态");
        assert!(fake.recorded().is_empty(), "运行中本就不直发");
        let audits = state
            .store
            .with(|c| crate::database::dao::write_audit::recent_conn(c, 10));
        assert_eq!(audits[0].action, "queue");
        assert_eq!(audits[0].result, "ok");
    }

    /// 拒绝矩阵：**H3 门在最前**——开关关闭（缺键 = 默认关）时 workbuddy / zcode 一律
    /// 403 headless_disabled（不再透出路由层原因）；**开关开启后**才轮到路由层判据
    /// （Task 7 起两家都路由进无头通道〈H9/H7〉，**Task 8/9/11 起真分派**——落 headless
    /// 回执封套，不再是过渡拒绝码；旧断言里的 `blackbox` / `headless_only` / `headless_pending`
    /// 三码均已随路由表重写与 Task 13 收口消失——本测更新为新真相，
    /// 语义未削弱：两段仍各自锁住「门在最前」与「开关开启后走路由/分派层」）。
    /// 未知会话 → 404 no_session；空/全空白 text 与超长（MAX_SEND_CHARS+1）→ 400
    #[tokio::test]
    async fn send_rejects_not_injectable_and_missing() {
        let fake = FakeInjector::ok();
        let state = inject_state(fake.clone());
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state.clone());
        // ① H3 门（开关关闭 = 默认态）：workbuddy / zcode 均 403 headless_disabled
        for sid in ["sess_c", "sess_d"] {
            let r = app
                .clone()
                .oneshot(req(
                    "POST",
                    "/m/api/v1/session-send",
                    Some("mam_device=mm"),
                    Some(&format!(r#"{{"sessionId":"{sid}","text":"hi"}}"#)),
                ))
                .await
                .unwrap();
            assert_eq!(r.status(), 403);
            let body = body_string(r).await;
            assert!(
                body.contains("headless_disabled"),
                "H3 门在最前（{sid}）：关闭态拒绝体必须是 headless_disabled：{body}"
            );
        }
        // ② 开关开启 → 门放行，路由层照判：workbuddy → Headless(WbAcp)（Task 11 已**真分派**：
        //    端点未启用 ⇒ 如实 `refused` 回执，**不是**过渡拒绝码）；**zcode →
        //    Headless(Zcode)（Task 8 已真分派）**——落无头回执封套（测试构建零真实安装路径 →
        //    如实报「安装不可达」，**绝不 spawn**）。两家都**不落终端注入臂**（pid 不是终端宿主）
        state.store.with(|c| {
            crate::database::dao::settings::set_setting_conn(c, "remote.headless_enabled", "true")
        });
        // workbuddy：真分派（Task 11）→ HTTP 200 + 无头封套 + 如实「端点未启用」拒绝
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-send",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_c","text":"hi"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "workbuddy 已真分派：回执走 headless 封套");
        let body = body_string(r).await;
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["status"], "headless");
        assert_eq!(v["channel"], "headless_wb_acp");
        assert_eq!(v["receipt"]["status"], "failed", "{v}");
        assert_eq!(
            v["receipt"]["stage"], "refused",
            "端点未启用 = 投递前拒绝（未起跑、零字节投递）：{v}"
        );
        assert!(
            v["receipt"]["reason"]
                .as_str()
                .is_some_and(|s| s.contains("WorkBuddy 远程控制端点未启用")),
            "必须如实说清启用条件（不冒充成功）：{v}"
        );
        assert!(
            !body.contains("headless_pending"),
            "workbuddy（Task 11 已接线）不得再落过渡拒绝码：{body}"
        );
        assert!(
            !body.contains("headless_disabled"),
            "开关开启后不得再被 H3 门拒绝（sess_c）：{body}"
        );
        // zcode：真分派（HTTP 200 + headless 封套 + 如实的安装不可达失败）
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-send",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_d","text":"hi"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "zcode 已真分派：回执走 headless 封套");
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["status"], "headless");
        assert_eq!(v["channel"], "headless_zcode");
        assert_eq!(v["receipt"]["status"], "failed", "{v}");
        assert_eq!(v["receipt"]["stage"], "spawn", "安装不可达 → spawn 档：{v}");
        assert!(
            v["receipt"]["reason"]
                .as_str()
                .is_some_and(|s| s.contains("安装路径不可达")),
            "必须如实说清失败原因（不冒充成功）：{v}"
        );
        assert!(
            !v.to_string().contains("headless_pending"),
            "zcode 不得再落过渡拒绝码：{v}"
        );
        assert!(
            fake.recorded().is_empty(),
            "无头路由会话零终端注入（不落终端注入臂）"
        );
        // 不存在的会话 → 404 no_session（W1：定位失败不入队）
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-send",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"nope","text":"hi"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 404);
        assert!(body_string(r).await.contains("no_session"));
        // 空 / 全空白 text → 400 bad_request
        for payload in [
            r#"{"sessionId":"sess_a","text":""}"#,
            r#"{"sessionId":"sess_a","text":"   "}"#,
        ] {
            let r = app
                .clone()
                .oneshot(req(
                    "POST",
                    "/m/api/v1/session-send",
                    Some("mam_device=mm"),
                    Some(payload),
                ))
                .await
                .unwrap();
            assert_eq!(r.status(), 400, "{payload}");
            assert!(body_string(r).await.contains("bad_request"));
        }
        // 超长（MAX_SEND_CHARS + 1 chars）→ 400
        let payload = serde_json::json!({
            "sessionId": "sess_a",
            "text": "字".repeat(crate::remote::api::MAX_SEND_CHARS + 1),
        })
        .to_string();
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-send",
                Some("mam_device=mm"),
                Some(&payload),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 400, "超长正文必须 400");
        // 全程无任何投递、无任何入队
        assert!(fake.recorded().is_empty());
        let pending = state
            .store
            .with(|c| crate::database::dao::inject_queue::pending_for_session_conn(c, "sess_c"));
        assert!(pending.is_empty(), "拒绝路径不得入队");
    }

    /// guard-busy 回归锁：直发遇 in-flight 占用 → 按 queued 回执（让位，不双投）。
    /// 守卫持到测尾——占全测试集唯一 id sess_i（复检终修：曾用 sess_h 与 approve_state
    /// 夹具的 approve_sends_key 跨夹具撞 id 串键实测 2/30 假红；立规见 inject_state doc）
    #[tokio::test]
    #[cfg_attr(
        not(any(windows, target_os = "macos")),
        ignore = "注入平台门（inject/routing.rs）：仅 Windows/macOS 可注入，本测走注入链"
    )]
    async fn send_input_ready_busy_inflight_falls_back_to_queue() {
        let fake = FakeInjector::ok();
        let state = inject_state(fake.clone());
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state.clone());
        let _busy = crate::inject::queue::try_acquire_inflight("sess_i").unwrap();
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-send",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_i","text":"你好"}"#),
            ))
            .await
            .unwrap();
        let body = body_string(r).await;
        assert!(
            body.contains("\"status\":\"queued\""),
            "in-flight 占用时直发应让位排队：{body}"
        );
        assert_eq!(fake.recorded().len(), 0, "占用期间不得注入");
    }

    /// guard-busy 回归锁（F1 新语义）：jump 遇 in-flight 占用 → 200 queued{itemId,position}
    /// （**裁决变化**：旧忙时回 failed「投递进行中」提示重试，现改 queued——条目保持
    /// pending 语义即排队，让位给进行中的那次投递；移动端 handleJump 对非 delivered 走
    /// reconcileQueued 对账，queued 直认恢复排队视图，无 failed 交互依赖）。守卫持到
    /// 测尾——sess_f 为 inject_state 夹具内 jump 忙测试专用 id
    #[tokio::test]
    #[cfg_attr(
        not(any(windows, target_os = "macos")),
        ignore = "注入平台门（inject/routing.rs）：仅 Windows/macOS 可注入，本测走注入链"
    )]
    async fn queue_jump_busy_inflight_falls_back_to_queue() {
        let fake = FakeInjector::ok();
        let state = inject_state(fake.clone());
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state.clone());
        // 先入队一条（sess_f Processing）
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-send",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_f","text":"第一条"}"#),
            ))
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        let id = v["itemId"].as_i64().unwrap();
        let _busy = crate::inject::queue::try_acquire_inflight("sess_f").unwrap();
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-queue/jump",
                Some("mam_device=mm"),
                Some(&format!(r#"{{"sessionId":"sess_f","itemId":{id}}}"#)),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let body = body_string(r).await;
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            v["status"], "queued",
            "in-flight 占用时 jump 应让位排队回执 queued：{body}"
        );
        assert_eq!(v["itemId"], id, "queued 回执必须携带点名条目 id");
        assert_eq!(v["position"], 1, "唯一 pending 条目队位 1");
        assert_eq!(fake.recorded().len(), 0, "占用期间不得注入");
        // 忙让位不落审计：账内恰一条审计 = 入队时 send 端点的 queue|ok（端点契约），
        // 不得出现 jump/fail——jump 审计只出自 settle（busy 让位未执行 flush_given）
        let audits = state
            .store
            .with(|c| crate::database::dao::write_audit::recent_conn(c, 10));
        assert_eq!(
            audits.len(),
            1,
            "忙让位不落审计（仅入队时的 queue|ok 一条）"
        );
        assert_eq!(audits[0].action, "queue");
        assert_eq!(audits[0].result, "ok");
    }

    /// 插队 + 撤回：黄态入队两条 → jump 第二条 delivered（插队语义：黄态照发，注入器
    /// 收到第二条 compose 产物）+ 审计 action=jump → retract 第一条 ok:true → queue 空
    #[tokio::test]
    #[cfg_attr(
        not(any(windows, target_os = "macos")),
        ignore = "注入平台门（inject/routing.rs）：仅 Windows/macOS 可注入，本测走注入链"
    )]
    async fn queue_jump_and_retract() {
        let fake = FakeInjector::ok();
        let state = inject_state(fake.clone());
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state.clone());
        // 入队两条（sess_b Processing 黄态）
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-send",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_b","text":"第一条"}"#),
            ))
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        let id1 = v["itemId"].as_i64().unwrap();
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-send",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_b","text":"第二条"}"#),
            ))
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        let id2 = v["itemId"].as_i64().unwrap();
        assert_eq!(v["position"], 2, "第二条排位 2");

        // jump 第二条 → delivered（插队语义：黄态照发）
        let payload = serde_json::json!({ "sessionId": "sess_b", "itemId": id2 }).to_string();
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-queue/jump",
                Some("mam_device=mm"),
                Some(&payload),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        assert!(body_string(r).await.contains("\"status\":\"delivered\""));
        // 注入器收到的是插队目标（第二条）的 compose 产物
        assert_eq!(
            fake.recorded(),
            vec![(12u32, "第二条".to_string())],
            "插队必须照发目标条目（运行中 TUI 把消息放进自身输入缓冲）"
        );
        // 审计 action=jump（settle 落账写入；此刻 retract 尚未发生，最新一条即 jump）
        let audits = state
            .store
            .with(|c| crate::database::dao::write_audit::recent_conn(c, 10));
        assert_eq!(audits[0].action, "jump");
        assert_eq!(audits[0].result, "ok");

        // retract 第一条 → ok:true
        let payload = serde_json::json!({ "sessionId": "sess_b", "itemId": id1 }).to_string();
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-queue/retract",
                Some("mam_device=mm"),
                Some(&payload),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        assert!(body_string(r).await.contains("\"ok\":true"));
        // 队列空（第一条被撤、第二条已发）
        let r = app
            .oneshot(req(
                "GET",
                "/m/api/v1/session-queue?session_id=sess_b",
                Some("mam_device=mm"),
                None,
            ))
            .await
            .unwrap();
        assert!(
            body_string(r).await.contains("\"items\":[]"),
            "撤回 + 插队发完后队列应为空"
        );
    }

    /// P1-4 端点精确映射（jump Suspended 分支）：点名快照中不存在的会话的 pending 项 →
    /// flush_given 挂起（红·中断不投递，W2）→ 200 queued + itemId/position（行保持
    /// pending 等会话回来，由 flush 循环/对账接力——Suspended 亦按排队回执）
    #[tokio::test]
    async fn queue_jump_suspended_returns_queued() {
        let fake = FakeInjector::ok();
        let state = inject_state(fake.clone());
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state.clone());
        // 直接入队一个快照外会话的 pending 项（session-send 对快照外会话 404 no_session
        // 拦在入队前，故走 DAO 缝构造——同一内存库，零接触真实 ~/.tuvis）
        let ghost_id = state.store.with(|c| {
            crate::database::dao::inject_queue::enqueue_conn(
                c,
                "sess_ghost",
                "claude",
                "mm",
                "测试设备",
                "幽灵消息",
                1000,
            )
        });
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-queue/jump",
                Some("mam_device=mm"),
                Some(&format!(
                    r#"{{"sessionId":"sess_ghost","itemId":{ghost_id}}}"#
                )),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(
            v["status"], "queued",
            "Suspended 必须按 queued 回执（行保持 pending，不谎报 delivered）"
        );
        assert_eq!(v["itemId"], ghost_id, "queued 回执携带点名条目 id");
        assert_eq!(
            v["position"], 1,
            "唯一 pending 项 position=1（回查实时队列）"
        );
        assert!(
            fake.recorded().is_empty(),
            "会话消失不得注入（pid 无从定位）"
        );
        // 行保持 pending：GET session-queue 仍可见该项
        let r = app
            .oneshot(req(
                "GET",
                "/m/api/v1/session-queue?session_id=sess_ghost",
                Some("mam_device=mm"),
                None,
            ))
            .await
            .unwrap();
        assert!(
            body_string(r).await.contains("幽灵消息"),
            "挂起行必须保持 pending（等会话回来由 flush 循环/对账接力）"
        );
    }

    /// P2-6 撤回与投递共守卫：该会话 in-flight 占用时 retract → 200 failed +
    /// 「投递进行中，请稍后重试」短回执。守卫必须取在 pending 前查**之前**
    /// （照 jump 先例）——本测用无 pending 行的会话：若守卫位置错误（后查），响应会是
    /// 404 not_found 而非 failed，测试即红
    #[tokio::test]
    async fn queue_retract_busy_inflight_returns_failed() {
        let fake = FakeInjector::ok();
        let state = inject_state(fake.clone());
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state.clone());
        // 独占会话 id（sess_r 不在夹具快照、无 pending 行）——in-flight 守卫按
        // session_id 全局占用，错开防止与并行测试互相挤占
        let _busy = crate::inject::queue::try_acquire_inflight("sess_r").unwrap();
        let r = app
            .oneshot(req(
                "POST",
                "/m/api/v1/session-queue/retract",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_r","itemId":1}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let body = body_string(r).await;
        assert!(
            body.contains("\"status\":\"failed\"") && body.contains("投递进行中，请稍后重试"),
            "in-flight 占用时撤回应 200 failed 短回执：{body}"
        );
        assert_eq!(fake.recorded().len(), 0, "占用期间不得注入");
    }

    /// 直发注入失败：注入器恒 Err → 200 {"status":"failed","error":…}（W4 可重试回执）
    /// + 审计 action=send result=failed:… + 队列无残留（W1：失败行 mark_failed 退出 pending）
    #[tokio::test]
    #[cfg_attr(
        not(any(windows, target_os = "macos")),
        ignore = "注入平台门（inject/routing.rs）：仅 Windows/macOS 可注入，本测走注入链"
    )]
    async fn send_reports_inject_failure() {
        let fake = FakeInjector::failing("定位终端失败：pid 不存在");
        let state = inject_state(fake.clone());
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state.clone());
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-send",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_e","text":"失败回执"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "注入失败以 200 failed 回执表达（非 5xx）");
        let body = body_string(r).await;
        assert!(
            body.contains("\"status\":\"failed\"") && body.contains("定位终端失败：pid 不存在"),
            "失败回执必须携带注入器错误原文：{body}"
        );
        assert_eq!(
            fake.recorded().len(),
            1,
            "失败发生在投递阶段（注入器已被调用）"
        );
        let audits = state
            .store
            .with(|c| crate::database::dao::write_audit::recent_conn(c, 10));
        assert_eq!(audits[0].action, "send");
        assert!(
            audits[0].result.starts_with("failed:"),
            "失败审计 result=failed:<原因>，实际 {}",
            audits[0].result
        );
        let pending = state
            .store
            .with(|c| crate::database::dao::inject_queue::pending_for_session_conn(c, "sess_e"));
        assert!(
            pending.is_empty(),
            "失败行必须退出 pending（W1：定位失败不入队重试）"
        );
    }

    /// D7/T3 端点分诊（Windows）：注入 Ok + 戳未中 + 屏读（假 pid）无滞留草稿 →
    /// 200 {"status":"submitted"}（中性回执，不冒充 delivered 也不冒充 failed——
    /// 验收问题 #5：failed 文案会诱导重试 = 双发）+ 审计 action=send
    /// result=unconfirmed 与确认送达 ok 区分（flush_one 落账并行写的 flush 审计
    /// 同为 unconfirmed）+ 行 mark_sent 退出 pending（队列无残留，flush 循环不
    /// 重投 = 防双发）。probe 恒 false 时直发确认走满族规格轮询窗（claude 快族
    /// 5s，端点无超时缝），本例为全测试集唯一 5s 级用例（申报：套件「无 5s 级
    /// 慢测」纪律的已知例外，见 T3 报告）；会话用全测试集唯一 id sess_t3——
    /// ~5s 轮询期间 in-flight 守卫全程占用（守卫按裸 id 全局串键，守卫 id 立规）
    #[cfg(windows)]
    #[tokio::test]
    async fn send_reports_submitted_when_stamp_missed_no_stuck_draft() {
        let fake = FakeInjector::ok();
        let state = inject_state_with_probe(fake.clone(), std::sync::Arc::new(|_, _, _| false));
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state.clone());
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-send",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_t3","text":"中性回执"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(
            r.headers()
                .get("cache-control")
                .and_then(|v| v.to_str().ok()),
            Some("no-store"),
            "submitted 回执同为门禁下私有数据，禁止中间层缓存"
        );
        let body = body_string(r).await;
        assert!(
            body.contains("\"status\":\"submitted\""),
            "未确认但已投递 → 中性 submitted 回执：{body}"
        );
        assert_eq!(
            fake.recorded().len(),
            1,
            "前提自证：注入确实发生（分诊发生在注入成功之后）"
        );
        let audits = state
            .store
            .with(|c| crate::database::dao::write_audit::recent_conn(c, 10));
        assert_eq!(
            audits.len(),
            2,
            "端点 send 终态审计 + flush_one 落账 flush 审计"
        );
        assert_eq!(audits[0].action, "send");
        assert_eq!(
            audits[0].result, "unconfirmed",
            "端点 send 审计单列 unconfirmed（不冒充 ok 也不冒充 failed:e）"
        );
        assert_eq!(audits[1].action, "flush");
        assert_eq!(audits[1].result, "unconfirmed");
        let pending = state
            .store
            .with(|c| crate::database::dao::inject_queue::pending_for_session_conn(c, "sess_t3"));
        assert!(
            pending.is_empty(),
            "Submitted 行已 mark_sent 消费，队列无残留（flush 循环不重投 = 防双发）"
        );
    }

    // ==== H3（Task 5）：无头总开关门（session-send 路由判定之前）====
    // 契约（spec H3 / 裁决 9-10）：无头注入默认关；关闭时对无头绑定会话发送 →
    // 403 {"error":"headless_disabled"}；开启则放行到既有路由层（断言「不是
    // headless_disabled」而非具体成功——Task 7 把 zcode 路由进无头通道后本测语义不变）；
    // **边界**：终端注入四家（claude/kimi/opencode/codex CLI）完全不经此门。
    // 零污染：开关 KV 经 DeviceStore::memory 内存库 seed（生产 Global = 全局 DB 同锁
    // 同连接，同语义）；未 seed = 缺键 = 默认关（正是默认态用例）。

    /// 覆盖 state 的会话源（建造器刚返回的 Arc 引用计数为 1 → `Arc::get_mut` 可取可变
    /// 引用；一经共享即 panic，不会静默改到别人头上）——与 [`with_target_evidence`] 同款
    fn with_sessions(
        mut state: Arc<RemoteState>,
        sessions: Vec<crate::session::Session>,
    ) -> Arc<RemoteState> {
        Arc::get_mut(&mut state)
            .expect("state 尚未共享（建造器返回值立即覆盖）")
            .session_source = Box::new(move || crate::session::SessionsResponse {
            total_count: sessions.len(),
            sessions: sessions.clone(),
            waiting_count: 0,
        });
        state
    }

    /// H3 门专用 state：**zcode APP 形态**会话（无头绑定；form 显式置 APP——真实
    /// ZCode 宿主形态）+ claude / **kimi / opencode** CLI Processing（终端注入家，走
    /// 留队臂：不碰 in-flight 守卫与注入器——边界用例的端点级证据，Minor 2）+ Task 7
    /// 新增 **claude 无进程卡（pid = 0）**：H11 的无头场景（会话在册但无 TUI 可写 →
    /// 路由判 `Headless(ClaudeP)` → 必须同样受 H3 门管辖，义务 2 的端点级证据）。
    /// 会话 id 独占（守卫 id 立规②——虽然本族用例在门/路由处即返回，不占守卫，仍按规
    /// 避开既有 id 字符串）
    fn headless_gate_state(
        injector: std::sync::Arc<dyn crate::inject::engine::Injector>,
    ) -> Arc<RemoteState> {
        let mut zcode = inj_sess(
            "sess_h3_zcode",
            crate::session::AgentType::ZCode,
            41,
            crate::session::SessionStatus::Waiting,
        );
        zcode.form = crate::session::ProcessForm::App;
        let terminal = [
            ("sess_h3_claude", crate::session::AgentType::Claude, 42),
            ("sess_h3_kimi", crate::session::AgentType::Kimi, 43),
            ("sess_h3_opencode", crate::session::AgentType::OpenCode, 44),
        ]
        .into_iter()
        .map(|(id, tool, pid)| inj_sess(id, tool, pid, crate::session::SessionStatus::Processing))
        .collect::<Vec<_>>();
        let mut sessions = vec![zcode];
        sessions.extend(terminal);
        // Task 7：无进程的 claude 卡 = H11 无头会话（未读卡兜底/进程已退出的在册会话）
        let mut claude_no_proc = inj_sess(
            "sess_h3_claude_noproc",
            crate::session::AgentType::Claude,
            0,
            crate::session::SessionStatus::Waiting,
        );
        claude_no_proc.form = crate::session::ProcessForm::Cli;
        sessions.push(claude_no_proc);
        with_sessions(inject_state(injector), sessions)
    }

    /// H3：开关**关闭**（KV 缺键 = 默认 "false"）→ zcode 无头绑定会话 403
    /// headless_disabled，且不落队、不注入（门在最前）
    #[tokio::test]
    async fn headless_gate_refuses_when_disabled() {
        let fake = FakeInjector::ok();
        let state = headless_gate_state(fake.clone());
        assert!(
            state
                .store
                .with(|c| crate::database::dao::settings::get_setting_conn(
                    c,
                    // 键名字面量：与 Rust 端 KEY_HEADLESS 常量双锁（键名是对外约定，
                    // setting_keys_are_stable 另有常量侧断言）
                    "remote.headless_enabled"
                ))
                .is_none(),
            "前提自证：缺键 = 默认关（本用例不 seed 开关 KV）"
        );
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state.clone());
        let r = app
            .oneshot(req(
                "POST",
                "/m/api/v1/session-send",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_h3_zcode","text":"你好"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 403, "无头通道未开启必须拒绝");
        let body = body_string(r).await;
        assert!(
            body.contains("\"error\":\"headless_disabled\""),
            "拒绝体必须是 headless_disabled（不是路由层的 not_injectable）：{body}"
        );
        assert!(fake.recorded().is_empty(), "门在注入之前：不投递任何内容");
        let pending = state.store.with(|c| {
            crate::database::dao::inject_queue::pending_for_session_conn(c, "sess_h3_zcode")
        });
        assert!(pending.is_empty(), "门在入队之前：不落队");
    }

    /// **义务 2 的端点级证据**（Task 7；Task 13/C4 更新收尾判据）：H11 的「无进程 CLI 会话」
    /// （pid = 0 → 路由判 `Headless(ClaudeP)`）**同样受 H3 门管辖**——开关关闭 → 403
    /// headless_disabled；开关开启 → 落无头分派点（**Task 13 起是真分派臂**：回执封套
    /// `channel=headless_claude_p`，本夹具 cwd 不在场 ⇒ 如实 `refused`（续接 cwd 门）——
    /// 不再是过渡码 `headless_pending`）。Task 5 的临时谓词 `is_headless_bound` 对 claude
    /// 恒判 false，且 `no_process` 早退先于一切路由，这条会话在 Task 7 前是**漏管面**
    #[tokio::test]
    async fn headless_gate_covers_h11_processless_cli_sessions() {
        let fake = FakeInjector::ok();
        let state = headless_gate_state(fake.clone());
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state.clone());
        let payload = r#"{"sessionId":"sess_h3_claude_noproc","text":"无进程在册会话"}"#;
        // ① 开关关闭（缺键 = 默认）：判无头绑定 → 403（不是 no_process）
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-send",
                Some("mam_device=mm"),
                Some(payload),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 403, "H11 无头会话必须受 H3 门管辖");
        let body = body_string(r).await;
        assert!(
            body.contains("headless_disabled"),
            "关闭态拒绝体必须是 headless_disabled（不是 no_process）：{body}"
        );
        // ② 开关开启 → 抵达**真分派臂**（Task 13/C4：claude 通道 + 如实拒绝，不再是过渡码）
        state.store.with(|c| {
            crate::database::dao::settings::set_setting_conn(c, "remote.headless_enabled", "true")
        });
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-send",
                Some("mam_device=mm"),
                Some(payload),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "真分派：HTTP 200 + 语义在 body");
        let body = body_string(r).await;
        assert!(
            !body.contains("headless_pending"),
            "过渡码必须已消失（Task 13 收口）：{body}"
        );
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["status"], "headless", "{body}");
        assert_eq!(
            v["channel"], "headless_claude_p",
            "H11 无进程 claude 会话必须进 claude 无头通道：{body}"
        );
        assert_eq!(v["receipt"]["status"], "failed", "{body}");
        assert_eq!(
            v["receipt"]["stage"], "refused",
            "夹具 cwd 不在场 ⇒ 续接 cwd 门如实拒绝（投递前、零字节）：{body}"
        );
        assert!(
            v["receipt"]["reason"]
                .as_str()
                .unwrap()
                .contains("工作目录"),
            "{body}"
        );
        // ③ 与兄弟门用例同强度的收尾断言（Minor 2）：两态全程零注入、零入队
        //    （门与 ⑤b 都在入队之前：H11 无头会话绝不会经终端注入器投递）
        assert!(
            fake.recorded().is_empty(),
            "H11 无头路径全程零注入（两态都在入队/投递之前拦下）"
        );
        let pending = state.store.with(|c| {
            crate::database::dao::inject_queue::pending_for_session_conn(c, "sess_h3_claude_noproc")
        });
        assert!(
            pending.is_empty(),
            "H11 无头路径不得入队（拒绝先于唯一 INSERT）"
        );
    }

    /// H3：开关**开启**（KV "true"）→ 放行到路由/分派层（Task 8 起 zcode 路由进无头通道
    /// 〈`Headless(Zcode)`〉并**真分派**——落 headless 回执封套，不再是过渡拒绝码；
    /// 本测锁「不是 headless_disabled」**且**「确实走到了无头分派点」两段）
    #[tokio::test]
    async fn headless_gate_allows_when_enabled() {
        let state = headless_gate_state(FakeInjector::ok());
        state.store.with(|c| {
            crate::database::dao::settings::set_setting_conn(c, "remote.headless_enabled", "true")
        });
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state.clone());
        let r = app
            .oneshot(req(
                "POST",
                "/m/api/v1/session-send",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_h3_zcode","text":"你好"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(
            r.status(),
            200,
            "开关开启后 zcode 走真分派（headless 封套）"
        );
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["status"], "headless", "{v}");
        assert_eq!(v["channel"], "headless_zcode");
        assert!(
            !v.to_string().contains("headless_disabled"),
            "开关开启后不得再被 H3 门拒绝：{v}"
        );
        assert!(
            !v.to_string().contains("headless_pending"),
            "Task 8 起 zcode 不得再落过渡拒绝码：{v}"
        );
    }

    /// H3 **边界**：终端注入工具（claude）不经此门——开关关闭也照常进入既有路径
    /// （断言请求确实抵达路由层：留队 queued 或平台门 not_injectable，二者皆非本门）。
    /// **三个 id 都是活进程会话**（pid 42/43/44）：有 TUI 可写 → 路由判终端通道（W3），
    /// 故不受无头开关影响；H11 的「无进程」面（pid = 0 → 无头通道）由
    /// [`headless_gate_covers_h11_processless_cli_sessions`] 反向锁住
    #[tokio::test]
    async fn headless_gate_leaves_terminal_tools_untouched() {
        let state = headless_gate_state(FakeInjector::ok());
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state.clone());
        // 端点级边界（Minor 2）：claude / **kimi** / **opencode** 三家 CLI 会话在开关
        // 关闭（默认）下均不得被 H3 门拒绝——请求必须抵达路由层（Windows/macOS 黄灯
        // 留队 queued；其他平台平台门 not_injectable），路由层的正常答复照旧
        for sid in ["sess_h3_claude", "sess_h3_kimi", "sess_h3_opencode"] {
            let r = app
                .clone()
                .oneshot(req(
                    "POST",
                    "/m/api/v1/session-send",
                    Some("mam_device=mm"),
                    Some(&format!(
                        r#"{{"sessionId":"{sid}","text":"终端四家不受影响"}}"#
                    )),
                ))
                .await
                .unwrap();
            let body = body_string(r).await;
            assert!(
                !body.contains("headless_disabled"),
                "终端注入工具绝不被 H3 门拦（spec H3 边界，{sid}）：{body}"
            );
            assert!(
                body.contains("\"status\":\"queued\"") || body.contains("not_injectable"),
                "请求应抵达路由层（Windows/macOS 留队 queued；其他平台平台门拒绝，{sid}）：{body}"
            );
        }
    }

    /// send-info 可用性矩阵：可注入会话 → injectable=true + channels/visibility
    /// （channels 随本机平台——routing platform = std::env::consts::OS，macOS 三通道 /
    /// windows 单通道，断言按编译平台取期望）；workbuddy / zcode（Task 7 起两家都路由进
    /// 无头通道〈H9/H7〉）→ 开关关闭 injectable=false + headless_disabled、开关开启
    /// **两家都已真接线 → injectable=true**（WB = Task 11 / zcode = Task 8；旧断言的
    /// blackbox 码与 Task 7 过渡码 `headless_pending` 均已消失）；
    /// 另锁定缺参 400 与未知会话 404 no_session
    #[tokio::test]
    #[cfg_attr(
        not(any(windows, target_os = "macos")),
        ignore = "注入平台门（inject/routing.rs）：仅 Windows/macOS 可注入，本测走注入链"
    )]
    async fn send_info_matrix() {
        let state = inject_state(FakeInjector::ok());
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state.clone());
        let r = app
            .clone()
            .oneshot(req(
                "GET",
                "/m/api/v1/session-send-info?session_id=sess_a",
                Some("mam_device=mm"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(
            r.headers()
                .get("cache-control")
                .and_then(|v| v.to_str().ok()),
            Some("no-store")
        );
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["injectable"], true);
        let channels: Vec<&str> = v["channels"]
            .as_array()
            .expect("channels 应为数组")
            .iter()
            .map(|c| c.as_str().expect("channel 应为字符串"))
            .collect();
        let want: &[&str] = if cfg!(target_os = "macos") {
            &["tmux", "iterm2", "terminal_app"]
        } else if cfg!(windows) {
            &["windows_console"]
        } else {
            &[]
        };
        assert_eq!(
            channels, want,
            "channels 应为 wire 小写字符串数组（本机平台）"
        );
        assert_eq!(v["visibility"], "realtime");
        // workbuddy（无头绑定：H9 ACP）→ **H3 门在最前**：开关关闭（默认）时
        // injectable=false + reasonCode=headless_disabled + spec H3 逐字置灰文案
        let r = app
            .clone()
            .oneshot(req(
                "GET",
                "/m/api/v1/session-send-info?session_id=sess_c",
                Some("mam_device=mm"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["injectable"], false);
        assert_eq!(v["reasonCode"], "headless_disabled");
        assert!(
            v["reason"]
                .as_str()
                .is_some_and(|s| s.contains("无头通道未开启，请在电脑端 MAM 设置中开启")),
            "关闭态置灰必须带 spec H3 逐字原因：{v}"
        );
        // 开关开启 → 门放行：**两家都已真接线**（zcode = Task 8、workbuddy = Task 11）
        // ——injectable:true + 无头通道名 + 可见性档。**与 session-send 同判据**：
        // 运行时不可用（WB 端点未启用 / zcode 安装不可达）由**回执**如实上报，
        // 不在静态能力面谎报置灰（否则输入端永远点不亮）
        state.store.with(|c| {
            crate::database::dao::settings::set_setting_conn(c, "remote.headless_enabled", "true")
        });
        // ②' zcode（**Task 8 已真分派**）：injectable:true + 通道名 + 可见性档。夹具
        //     home_source = None（读不到信任表）→ 保守判**未信任**（「仅兔维斯可见」——
        //     绝不谎报「重启后可见」）
        let r = app
            .clone()
            .oneshot(req(
                "GET",
                "/m/api/v1/session-send-info?session_id=sess_d",
                Some("mam_device=mm"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["injectable"], true, "zcode 已接线 → 输入区必须可用：{v}");
        assert_eq!(v["channels"], serde_json::json!(["headless_zcode"]));
        assert_eq!(
            v["visibility"], "tuvis_only",
            "读不到信任表 = 保守判未信任：{v}"
        );
        // ②'' workbuddy（**Task 11 已真分派**）：injectable:true + `headless_wb_acp`
        //      + realtime（ACP 写入 APP 内可见——端点即宿主运行时）
        for sid in ["sess_c"] {
            let r = app
                .clone()
                .oneshot(req(
                    "GET",
                    &format!("/m/api/v1/session-send-info?session_id={sid}"),
                    Some("mam_device=mm"),
                    None,
                ))
                .await
                .unwrap();
            assert_eq!(r.status(), 200);
            let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
            assert_eq!(v["injectable"], true, "workbuddy 已接线（{sid}）：{v}");
            assert_eq!(
                v["channels"],
                serde_json::json!(["headless_wb_acp"]),
                "通道名必须来自路由表单源（{sid}）：{v}"
            );
            assert_eq!(v["visibility"], "realtime", "{sid}：{v}");
            assert!(
                !v.to_string().contains("headless_pending"),
                "Task 11 起不得再落过渡拒绝码（{sid}）：{v}"
            );
        }
        // 缺参 → 400；未知会话 → 404 no_session
        let r = app
            .clone()
            .oneshot(req(
                "GET",
                "/m/api/v1/session-send-info",
                Some("mam_device=mm"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 400);
        let r = app
            .oneshot(req(
                "GET",
                "/m/api/v1/session-send-info?session_id=nope",
                Some("mam_device=mm"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 404);
        assert!(body_string(r).await.contains("no_session"));
    }

    /// 审批默认表 marker 命中句（DEFAULT_MAPPINGS_JSON claude.prompt_markers 含
    /// "do you want to proceed"——M9R 评审 F3/D4 收紧后句式）
    const APPROVE_HIT_MSG: &str = "Do you want to proceed?";

    /// opencode 多选题载荷（GET 快照用例；引号直接写——raw string 内不需转义）
    const PAYLOAD_OC_MULTI: &str = r#"{"questions":[{"header":"优化重点","multiSelect":true,"question":"你希望这次优化重点放在哪些方面？","options":[{"label":"画面美感与细节"},{"label":"性能与兼容性"}]}]}"#;

    // ==== Task 8（H7）：zcode 无头分派链 / 取消端点（`/session-headless-cancel`）====
    //
    // **测试构建的确定性**：`zcode::production_roots` 在 `cfg(test)` 下恒空表（宪法级：
    // 本机真装了 ZCode，若单测也去咨询真机安装路径，端点用例就会**真的 spawn 一个真实
    // 回合**——消耗用户真实账号配额并写真实 ~/.zcode）。故分派链在门禁里恒落
    // 「安装不可达」拒绝臂；真回合的执行语义由 `zcode::run_turn` 的脚本缝用例覆盖，
    // 真实一次调用登记在 Task 8 报告里（USER-ASSIST/实机步骤）。

    /// zcode 分派用 state：开关开启 + 设备 + **指定会话**（id 独占，避免与其它夹具
    /// 撞裸 id 字符串——守卫 id 立规②）
    fn zcode_dispatch_state(sessions: Vec<crate::session::Session>) -> Arc<RemoteState> {
        let state = with_sessions(inject_state(FakeInjector::ok()), sessions);
        state.store.with(|c| {
            crate::database::dao::settings::set_setting_conn(c, "remote.headless_enabled", "true")
        });
        persist_named_device(&state, "mm", "测试设备");
        state
    }

    /// zcode 无头分派链（开关开启 → 真分派臂）：
    /// ① HTTP 200 + `headless` 封套（Task 6 回执原样透出）；
    /// ② 安装不可达 = 如实的 `spawn` 失败（**不冒充成功、不落终端注入臂、不入队**）；
    /// ③ 落 `headless` 审计行（result = 终态 + 阶段码，通道列 = `headless_zcode`）；
    /// ④ 拒绝后串行锁**已注销**（否则该会话的下一条永远被自己的锁挡住）
    #[tokio::test]
    async fn zcode_headless_dispatch_is_honest_when_install_is_unreachable() {
        let state = zcode_dispatch_state(vec![inj_sess(
            "sess_h7_dispatch",
            crate::session::AgentType::ZCode,
            14,
            crate::session::SessionStatus::Waiting,
        )]);
        let app = router(state.clone());
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-send",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_h7_dispatch","text":"无头你好"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "无头分派：HTTP 200 + 语义在 body");
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["status"], "headless", "{v}");
        assert_eq!(v["channel"], "headless_zcode");
        assert_eq!(v["receipt"]["status"], "failed");
        assert_eq!(v["receipt"]["stage"], "spawn", "安装不可达 → spawn 档：{v}");
        assert_eq!(v["receipt"]["sessionId"], "sess_h7_dispatch");
        assert!(
            v["receipt"]["reason"]
                .as_str()
                .is_some_and(|s| s.contains("安装路径不可达")),
            "失败必须点明原因：{v}"
        );
        let audits = state
            .store
            .with(|c| crate::database::dao::write_audit::recent_conn(c, 10));
        assert_eq!(audits.len(), 1, "无头回合必须落既有 W5 单账本：{audits:?}");
        assert_eq!(audits[0].action, "headless");
        assert_eq!(audits[0].channel, "headless_zcode", "通道列 = 无头通道名");
        assert_eq!(audits[0].session_id, "sess_h7_dispatch");
        assert_eq!(audits[0].device_name, "测试设备");
        assert_eq!(
            audits[0].result, "failed(spawn) · 0ms",
            "终态 + 阶段码 + 耗时口径（Task 6 单点）"
        );
        assert!(
            !crate::inject::headless::zcode::registry().in_flight("sess_h7_dispatch"),
            "拒绝臂必须注销串行锁（否则该会话被自己的锁永久挡死）"
        );
        let pending = state.store.with(|c| {
            crate::database::dao::inject_queue::pending_for_session_conn(c, "sess_h7_dispatch")
        });
        assert!(pending.is_empty(), "无头回合绝不入队（裁决 8）");
    }

    /// 斜杠命令在无头通道**显式拒绝**（spec H7：字面文本、不具等价性——不静默透传冒充
    /// 支持）；同样如实落账、不入队
    #[tokio::test]
    async fn zcode_headless_refuses_slash_commands_explicitly() {
        let state = zcode_dispatch_state(vec![inj_sess(
            "sess_h7_slash",
            crate::session::AgentType::ZCode,
            14,
            crate::session::SessionStatus::Waiting,
        )]);
        let app = router(state.clone());
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-send",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_h7_slash","text":"/plan 看代码"}"#),
            ))
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["status"], "headless", "{v}");
        // **投递前拒绝**用 `refused` 档（Task 8 复审追补）：回合未起跑、零字节投递——
        // 与 `channel_error`（通道跑过但没拿到有效回执）分列，移动端分诊文案才说得准
        assert_eq!(v["receipt"]["stage"], "refused", "{v}");
        assert!(
            v["receipt"]["reason"]
                .as_str()
                .is_some_and(|s| s.contains("斜杠命令")),
            "必须显式说明斜杠命令不可用：{v}"
        );
        let audits = state
            .store
            .with(|c| crate::database::dao::write_audit::recent_conn(c, 10));
        assert_eq!(audits[0].result, "failed(refused) · 0ms");
        assert!(state
            .store
            .with(
                |c| crate::database::dao::inject_queue::pending_for_session_conn(
                    c,
                    "sess_h7_slash"
                )
            )
            .is_empty());
    }

    /// **会话串行锁**（MAM 自己的）：同会话已有在飞无头回合 → 第二次请求如实拒绝
    /// （不排队、不覆盖），且**不得碰第一个回合的槽位**（取消靶子必须还在——否则移动端
    /// 取消钮变哑的）
    #[tokio::test]
    async fn zcode_headless_serial_lock_refuses_second_turn() {
        let state = zcode_dispatch_state(vec![inj_sess(
            "sess_h7_lock",
            crate::session::AgentType::ZCode,
            14,
            crate::session::SessionStatus::Waiting,
        )]);
        // 模拟「已有在飞回合」：占位 + 已武装取消靶子（真回合由 run_turn 逐尝试 arm）
        let reg = crate::inject::headless::zcode::registry();
        assert!(reg.begin(
            "sess_h7_lock",
            crate::inject::headless::zcode::TurnSlot::placeholder("zcode", "在飞正文".into())
        ));
        let hit = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let hit2 = hit.clone();
        reg.arm(
            "sess_h7_lock",
            std::sync::Arc::new(move || {
                hit2.store(true, std::sync::atomic::Ordering::SeqCst);
                true
            }),
        );
        let app = router(state.clone());
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-send",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_h7_lock","text":"第二条"}"#),
            ))
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["status"], "headless", "{v}");
        assert_eq!(v["receipt"]["status"], "failed");
        assert!(
            v["receipt"]["reason"]
                .as_str()
                .is_some_and(|s| s.contains("串行锁")),
            "必须如实说明被串行锁拒绝：{v}"
        );
        assert!(
            reg.in_flight("sess_h7_lock"),
            "拒绝臂不得注销**别人**的在飞槽位"
        );
        assert_eq!(
            reg.request_cancel("sess_h7_lock"),
            Some(true),
            "第一个回合的取消靶子必须原样保留"
        );
        assert!(hit.load(std::sync::atomic::Ordering::SeqCst));
        reg.end("sess_h7_lock");
    }

    /// 取消端点：在飞回合 + 靶子送达 → `{cancelled:true}` + `headless_cancel` 审计行
    /// （设备 = 按下取消的这台设备；正文/工具 = 回合自身——与 `headless` 行同源）
    #[tokio::test]
    async fn headless_cancel_endpoint_audits_delivered_cancel() {
        let state = inject_state(FakeInjector::ok());
        persist_named_device(&state, "mm", "测试设备");
        let reg = crate::inject::headless::zcode::registry();
        assert!(reg.begin(
            "sess_h7_cancel",
            crate::inject::headless::zcode::TurnSlot::placeholder("zcode", "回合正文".into())
                .with_channel("headless_zcode")
        ));
        reg.arm(
            "sess_h7_cancel",
            std::sync::Arc::new({
                // 模拟 Task 6 的「先到者生效」：取消口一次一臂——第一发取走靶子，第二发报 false
                let armed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
                move || armed.swap(false, std::sync::atomic::Ordering::SeqCst)
            }),
        );
        let app = router(state.clone());
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-headless-cancel",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_h7_cancel"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["cancelled"], true, "{v}");
        let audits = state
            .store
            .with(|c| crate::database::dao::write_audit::recent_conn(c, 10));
        assert_eq!(audits.len(), 1, "取消送达必须落账：{audits:?}");
        assert_eq!(audits[0].action, "headless_cancel");
        assert_eq!(audits[0].channel, "headless_zcode");
        assert_eq!(audits[0].session_id, "sess_h7_cancel");
        assert_eq!(audits[0].device_name, "测试设备");
        assert!(
            audits[0].result.starts_with("cancelled · ") && audits[0].result.ends_with("ms"),
            "终态 + 耗时口径（Task 6 单点）：{}",
            audits[0].result
        );
        assert!(
            audits[0].summary.contains("回合正文"),
            "取消行的内容摘要取回合原文（与回合行同源）：{}",
            audits[0].summary
        );
        // 取消只是请求：槽位由回合自身注销（本端点不得注销——否则第二发取消会谎报
        // 「无在飞回合」）
        assert!(reg.in_flight("sess_h7_cancel"));
        // 重复取消：靶子已被先到者取走 → 如实报「未送达」，不落第二行
        let r = app
            .oneshot(req(
                "POST",
                "/m/api/v1/session-headless-cancel",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_h7_cancel"}"#),
            ))
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["cancelled"], false, "先到者生效：重复取消不生效：{v}");
        assert!(v["reason"].as_str().is_some_and(|s| s.contains("未送达")));
        assert_eq!(
            state
                .store
                .with(|c| crate::database::dao::write_audit::recent_conn(c, 10))
                .len(),
            1,
            "未送达的取消不得落账"
        );
        reg.end("sess_h7_cancel");
    }

    /// 取消端点的诚实面：无在飞回合 → `{cancelled:false, reason}` 且**零审计行**；
    /// 迟到取消（靶子返回 false = 回合已终结/watchdog 先到）同样不落取消行；缺参 400
    #[tokio::test]
    async fn headless_cancel_endpoint_is_honest_when_nothing_to_cancel() {
        let state = inject_state(FakeInjector::ok());
        persist_named_device(&state, "mm", "测试设备");
        let reg = crate::inject::headless::zcode::registry();
        let app = router(state.clone());
        // ① 无在飞回合
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-headless-cancel",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_h7_none"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["cancelled"], false, "{v}");
        assert!(v["reason"]
            .as_str()
            .is_some_and(|s| s.contains("没有在飞的无头回合")));
        assert!(
            state
                .store
                .with(|c| crate::database::dao::write_audit::recent_conn(c, 10))
                .is_empty(),
            "没有取消动作就不落账（不编造审计行）"
        );
        // ② 迟到取消：槽位在但靶子已失效（先到者已生效 / 回合已终结）
        assert!(reg.begin(
            "sess_h7_late",
            crate::inject::headless::zcode::TurnSlot::placeholder("zcode", "旧回合".into())
        ));
        reg.arm("sess_h7_late", std::sync::Arc::new(|| false));
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-headless-cancel",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_h7_late"}"#),
            ))
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["cancelled"], false, "迟到取消不生效（先到者生效）：{v}");
        assert!(
            state
                .store
                .with(|c| crate::database::dao::write_audit::recent_conn(c, 10))
                .is_empty(),
            "未送达的取消不落账"
        );
        reg.end("sess_h7_late");
        // ③ 缺参 400
        let r = app
            .oneshot(req(
                "POST",
                "/m/api/v1/session-headless-cancel",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"  "}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 400);
        assert!(body_string(r).await.contains("bad_request"));
    }

    /// **取消审计行的通道列取自槽位**（Task 11 复审 Important 2）：三条已接线通道各占一槽
    /// （各自 `.with_channel(...)`，与生产分派臂同规）→ 送达取消的审计行必须记**各自的**
    /// 通道名。**回归锁**：修复前该端点把通道写死 `headless_zcode` ⇒ codex/WB 的取消被记错通道。
    /// 另钉死「槽位未声明通道」的诚实哨兵词（不冒充任何既有通道）。
    #[tokio::test]
    async fn headless_cancel_audits_the_armed_slot_channel() {
        use crate::inject::headless::turn::{registry, TurnSlot};
        let state = inject_state(FakeInjector::ok());
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state.clone());
        let arm_true =
            || std::sync::Arc::new(|| true) as std::sync::Arc<dyn Fn() -> bool + Send + Sync>;
        // (会话 id, 槽位通道, 期望审计通道)——覆盖 zcode / codex / WB 三通道 + 未声明哨兵
        let cases: [(&str, &str, &str); 4] = [
            ("sess_t11_cancel_zcode", "headless_zcode", "headless_zcode"),
            (
                "sess_t11_cancel_codex",
                "headless_codex_exec",
                "headless_codex_exec",
            ),
            ("sess_t11_cancel_wb", "headless_wb_acp", "headless_wb_acp"),
            ("sess_t11_cancel_unnamed", "", "headless_unattributed"),
        ];
        for (sid, slot_channel, want) in cases {
            assert!(registry().begin(
                sid,
                TurnSlot::placeholder("codex", "回合正文".into()).with_channel(slot_channel)
            ));
            registry().arm(sid, arm_true());
            let r = app
                .clone()
                .oneshot(req(
                    "POST",
                    "/m/api/v1/session-headless-cancel",
                    Some("mam_device=mm"),
                    Some(&format!(r#"{{"sessionId":"{sid}"}}"#)),
                ))
                .await
                .unwrap();
            let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
            assert_eq!(v["cancelled"], true, "{sid}: {v}");
            let audits = state
                .store
                .with(|c| crate::database::dao::write_audit::recent_conn(c, 10));
            assert_eq!(audits[0].action, "headless_cancel", "{sid}");
            assert_eq!(
                audits[0].channel, want,
                "{sid}：取消行的通道列必须取槽位声明（不得写死常量）：{audits:?}"
            );
            registry().end(sid);
        }
    }

    /// **取消措辞必须分辨「回合仍在跑」的两种未送达形态**（Task 11 复审 Important 3）：
    /// ① WB（H9 ACP）本批**未接线取消**（槽位从不武装靶子）⇒ 必须说「本通道尚未接线取消 +
    /// **回合仍在运行** + 本次未取消」，**不得**说成「回合已终结」；
    /// ② zcode 已接线但靶子尚未武装（版本门控/预检窗口）⇒ 必须说「尚未武装 + 仍在运行」；
    /// ③ 已武装但靶子返回 false（真终结/已取消）⇒ 才说「已终结或已取消」。
    #[tokio::test]
    async fn headless_cancel_wording_distinguishes_unwired_from_finished_turn() {
        use crate::inject::headless::turn::{registry, TurnSlot};
        let state = inject_state(FakeInjector::ok());
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state.clone());
        let cancel = |sid: &str| {
            let app = app.clone();
            let sid = sid.to_string();
            async move {
                let r = app
                    .oneshot(req(
                        "POST",
                        "/m/api/v1/session-headless-cancel",
                        Some("mam_device=mm"),
                        Some(&format!(r#"{{"sessionId":"{sid}"}}"#)),
                    ))
                    .await
                    .unwrap();
                serde_json::from_str::<serde_json::Value>(&body_string(r).await).unwrap()
            }
        };
        // ① WB：在飞、未接线取消 → 如实说「未接线 + 仍在运行」
        assert!(registry().begin(
            "sess_t11_nocancel_wb",
            TurnSlot::placeholder("workbuddy", "WB 在飞回合".into())
                .with_channel("headless_wb_acp")
        ));
        let v = cancel("sess_t11_nocancel_wb").await;
        assert_eq!(v["cancelled"], false, "{v}");
        let reason = v["reason"].as_str().unwrap_or_default();
        assert!(reason.contains("尚未接线取消"), "必须点明未接线：{reason}");
        assert!(
            reason.contains("仍在运行"),
            "必须说清回合仍在跑（不是已终结）：{reason}"
        );
        assert!(
            !reason.contains("已终结"),
            "不得把「未接线」说成「已终结」：{reason}"
        );
        assert!(registry().in_flight("sess_t11_nocancel_wb"));
        registry().end("sess_t11_nocancel_wb");
        // ② zcode 已接线、靶子未武装（版本门控/预检窗口）→ 「尚未武装 + 仍在运行」
        assert!(registry().begin(
            "sess_t11_nocancel_preflight",
            TurnSlot::placeholder("zcode", "预检窗口回合".into()).with_channel("headless_zcode")
        ));
        let v = cancel("sess_t11_nocancel_preflight").await;
        let reason = v["reason"].as_str().unwrap_or_default();
        assert!(
            reason.contains("尚未武装") && reason.contains("仍在运行"),
            "预检窗口必须如实说「靶子尚未武装 + 回合仍在运行」：{reason}"
        );
        registry().end("sess_t11_nocancel_preflight");
        // ③ 已武装 + 靶子返回 false（真终结/已取消）→ 才可以说「已终结或已取消」
        assert!(registry().begin(
            "sess_t11_nocancel_finished",
            TurnSlot::placeholder("zcode", "旧回合".into()).with_channel("headless_zcode")
        ));
        registry().arm("sess_t11_nocancel_finished", std::sync::Arc::new(|| false));
        let v = cancel("sess_t11_nocancel_finished").await;
        let reason = v["reason"].as_str().unwrap_or_default();
        assert!(
            reason.contains("已终结或已取消"),
            "已武装的未送达才说「已终结或已取消」：{reason}"
        );
        registry().end("sess_t11_nocancel_finished");
        // 三种未送达都不得落账
        assert!(
            state
                .store
                .with(|c| crate::database::dao::write_audit::recent_conn(c, 10))
                .is_empty(),
            "未送达的取消不落账"
        );
    }

    /// 审批选项（可批）：sess_a Waiting + last_message 命中 → 200 available=true +
    /// options 恰为 允许/拒绝 两项（**无 key 字段**——键位不外泄给 UI）+
    /// reason=null（非严格档不可批形态不下发降级文案，前端按自隐处理）+
    /// verifiedWith=2.1.251（M8R Task 10 取证回填；drift 按 verified/current 同源重算，
    /// 本机 claude 探测命中同版则 false）
    #[tokio::test]
    async fn approve_options_available() {
        let fake = FakeInjector::ok();
        let state = approve_state(fake.clone(), Some(APPROVE_HIT_MSG));
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state.clone());
        // 无 cookie → 403（nest 内层 gate 结构性覆盖新端点）
        let r = app
            .clone()
            .oneshot(req(
                "GET",
                "/m/api/v1/session-approve-options?session_id=sess_a",
                None,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 403, "审批选项端点必须过 gate");
        let r = app
            .clone()
            .oneshot(req(
                "GET",
                "/m/api/v1/session-approve-options?session_id=sess_a",
                Some("mam_device=mm"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(
            r.headers()
                .get("cache-control")
                .and_then(|v| v.to_str().ok()),
            Some("no-store"),
            "审批可用性是门禁下私有数据，禁止中间层缓存"
        );
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["available"], true);
        let options = v["options"].as_array().expect("options 应为数组");
        assert_eq!(options.len(), 2);
        assert_eq!(options[0]["id"], "approve");
        assert_eq!(options[0]["label"], "允许");
        assert_eq!(options[1]["id"], "reject");
        assert_eq!(options[1]["label"], "拒绝");
        for o in options {
            assert!(
                o.get("key").is_none(),
                "选项载荷不得携带 key（键位不外泄给 UI）：{o}"
            );
        }
        // Task 13 取证回填后：claude verified_with = "2.1.251"（Windows 本机实测）。
        // currentVersion 来自真实 spawn 探测（机器相关：灰1 后裸名直 spawn 失败回退
        // cmd /c 垫片，再失败才 null → "unknown"）——drift 断言按同源纯函数重算期望，
        // 保持 hermetic；codex 保持 probe-pending 的恒漂移断言见 inject::approve::tests
        let current = v["currentVersion"].as_str();
        let expect_drift =
            crate::inject::approve::is_version_drift("2.1.251", current.unwrap_or("unknown"));
        assert_eq!(v["verifiedWith"], "2.1.251");
        assert_eq!(v["drift"], expect_drift, "drift 与 verified/current 一致");
        assert!(
            fake.recorded_keys().is_empty(),
            "查询选项端点不得触发任何按键注入"
        );
    }

    /// 审批选项（不可批）：非 Waiting（sess_t5m Processing）→ available=false；
    /// Waiting 但 last_message 与 marker 无关 → available=false；last_message=None
    /// （sess_e 夹具原样）→ available=false（None 不命中）
    #[tokio::test]
    async fn approve_options_not_waiting_or_no_hit() {
        let state_a_hit = approve_state(FakeInjector::ok(), Some(APPROVE_HIT_MSG));
        persist_named_device(&state_a_hit, "mm", "测试设备");
        let app = router(state_a_hit);
        // sess_t5m Processing → available=false（Waiting 判定先于映射解析）
        let r = app
            .clone()
            .oneshot(req(
                "GET",
                "/m/api/v1/session-approve-options?session_id=sess_t5m",
                Some("mam_device=mm"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["available"], false, "非 Waiting 不得可批");
        assert!(v["options"].as_array().unwrap().is_empty());
        // sess_a Waiting 但 last_message 与任何 marker 无关 → available=false
        let state_a_miss = approve_state(FakeInjector::ok(), Some("无关"));
        persist_named_device(&state_a_miss, "mm", "测试设备");
        let app_miss = router(state_a_miss);
        let r = app_miss
            .clone()
            .oneshot(req(
                "GET",
                "/m/api/v1/session-approve-options?session_id=sess_a",
                Some("mam_device=mm"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["available"], false, "marker 未命中不得可批");
        assert!(v["options"].as_array().unwrap().is_empty());
        // last_message=None（sess_e）→ available=false（detect 对 None 不命中）
        let r = app_miss
            .oneshot(req(
                "GET",
                "/m/api/v1/session-approve-options?session_id=sess_e",
                Some("mam_device=mm"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["available"], false, "无 last_message 不得可批");
    }

    /// 审批选项（无映射）：sess_d zcode（Waiting）工具无映射 → available=false
    #[tokio::test]
    async fn approve_options_no_mapping() {
        let state = approve_state(FakeInjector::ok(), Some(APPROVE_HIT_MSG));
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state);
        let r = app
            .oneshot(req(
                "GET",
                "/m/api/v1/session-approve-options?session_id=sess_d",
                Some("mam_device=mm"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["available"], false, "工具无映射不得可批");
        assert!(v["options"].as_array().unwrap().is_empty());
    }

    /// T4 红卡接铃铛：等待标记路径——sess_t5m（Processing claude、无 last_message，
    /// 旧判定下必 available=false）seed 审批等待标记 → GET available=true 且选项齐
    /// （键位零泄漏）；POST approve 越过 409 not_waiting 直达键位分发（键位 "1"）。
    /// 标记经 store.with 播种（内存库，零接触真实 ~/.tuvis）；state 实例按测试隔离
    #[tokio::test]
    async fn approve_endpoints_honor_wait_mark() {
        let fake = FakeInjector::ok();
        let state = approve_state(fake.clone(), Some(APPROVE_HIT_MSG));
        persist_named_device(&state, "mm", "测试设备");
        state.store.with(|conn| {
            crate::database::dao::approval_wait::mark(conn, "claude", "sess_t5m", 1_000, "测试标记")
        });
        let app = router(state.clone());
        // GET：Processing + 标记 → available=true（跳过 detect）+ 键位零泄漏
        let r = app
            .clone()
            .oneshot(req(
                "GET",
                "/m/api/v1/session-approve-options?session_id=sess_t5m",
                Some("mam_device=mm"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["available"], true, "标记存在即 available（跳过 detect）");
        let options = v["options"].as_array().expect("标记路径应出全选项");
        assert_eq!(options.len(), 2, "标记路径跳过 marker detect 直接出全选项");
        for o in options {
            assert!(o.get("key").is_none(), "键位不外泄");
        }
        // POST：Processing + 标记 → 越过 409 not_waiting，键位照常分发
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-approve",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_t5m","optionId":"approve"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "标记路径越过 409 not_waiting");
        assert!(
            fake.recorded_keys().iter().any(|k| k.1 == "1"),
            "批准键位应经注入器分发：{:?}",
            fake.recorded_keys()
        );
    }

    /// T4 回归锁：无标记 + 非 Waiting → 409 not_waiting 守卫保持（标记不扩大放行面）
    #[tokio::test]
    async fn approve_without_mark_still_409_on_processing() {
        let fake = FakeInjector::ok();
        let state = approve_state(fake.clone(), Some(APPROVE_HIT_MSG));
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state.clone());
        let r = app
            .oneshot(req(
                "POST",
                "/m/api/v1/session-approve",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_t5m","optionId":"approve"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 409, "无标记 Processing 仍 409 not_waiting");
    }

    /// 审批应答（批准）：sess_h optionId=approve → 200 key_sent + FakeInjector 收到
    /// (pid=18, "1")（**无 [mobile] 前缀**——按键非文本）+ 审计 action=approve result=ok。
    /// 独占会话 sess_h——契约行为不变（按键映射/无前缀/审计）；sess_h 全测试集唯一归
    /// 本族（inject_state 侧 busy 测试已改用 sess_i，复检终修，立规见两夹具 doc）
    #[tokio::test]
    async fn approve_sends_key() {
        let fake = FakeInjector::ok();
        let state = approve_state(fake.clone(), Some(APPROVE_HIT_MSG));
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state.clone());
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-approve",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_h","optionId":"approve"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(
            r.headers()
                .get("cache-control")
                .and_then(|v| v.to_str().ok()),
            Some("no-store"),
            "审批回执是门禁下私有数据，禁止中间层缓存"
        );
        let body = body_string(r).await;
        assert!(
            body.contains("\"status\":\"key_sent\""),
            "批准应回执 key_sent：{body}"
        );
        assert_eq!(
            fake.recorded_keys(),
            vec![(18u32, "1".to_string())],
            "按键注入必须带映射键位且无 [mobile] 前缀"
        );
        assert!(fake.recorded().is_empty(), "审批走按键通道，不走文本注入");
        let audits = state
            .store
            .with(|c| crate::database::dao::write_audit::recent_conn(c, 10));
        assert_eq!(audits[0].action, "approve");
        assert_eq!(audits[0].result, "ok");
        assert_eq!(audits[0].channel, "fake");
        assert_eq!(audits[0].session_id, "sess_h");
    }

    /// 审批应答（拒绝）：optionId=reject → 200 key_sent + FakeInjector 收到 (pid=17, "esc")
    /// + 审计 action=reject。用独占会话 sess_g：in-flight 守卫按 session_id 全局占用，
    /// 与 approve_sends_key（sess_a，契约指定）错开，防并行测试互抢守卫（Task 6 夹具同规）
    #[tokio::test]
    async fn approve_reject_audits_reject() {
        let fake = FakeInjector::ok();
        let state = approve_state(fake.clone(), Some(APPROVE_HIT_MSG));
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state.clone());
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-approve",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_g","optionId":"reject"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let body = body_string(r).await;
        assert!(
            body.contains("\"status\":\"key_sent\""),
            "拒绝应回执 key_sent：{body}"
        );
        assert_eq!(
            fake.recorded_keys(),
            vec![(17u32, "esc".to_string())],
            "拒绝键位 esc 必须来自映射表"
        );
        let audits = state
            .store
            .with(|c| crate::database::dao::write_audit::recent_conn(c, 10));
        assert_eq!(audits[0].action, "reject", "拒绝审计 action=reject");
        assert_eq!(audits[0].result, "ok");
        assert_eq!(audits[0].session_id, "sess_g");
    }

    /// 审防 guard 矩阵：非法 optionId → 404 no_mapping；非 Waiting（sess_b）→ 409
    /// not_waiting；不存在会话 → 404 no_session；缺参 → 400 bad_request
    #[tokio::test]
    async fn approve_guards() {
        let fake = FakeInjector::ok();
        let state = approve_state(fake.clone(), Some(APPROVE_HIT_MSG));
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state.clone());
        // 非法 optionId（映射表中无此项）→ 404 no_mapping（降级提示走普通发送）
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-approve",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_a","optionId":"bogus"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 404);
        assert!(body_string(r).await.contains("no_mapping"));
        // 非 Waiting（sess_t5m Processing）→ 409 not_waiting
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-approve",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_t5m","optionId":"approve"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 409);
        assert!(body_string(r).await.contains("not_waiting"));
        // 不存在会话 → 404 no_session
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-approve",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"nope","optionId":"approve"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 404);
        assert!(body_string(r).await.contains("no_session"));
        // 缺参（空 sessionId / 空 optionId）→ 400 bad_request
        for payload in [
            r#"{"sessionId":"","optionId":"approve"}"#,
            r#"{"sessionId":"sess_a","optionId":"  "}"#,
        ] {
            let r = app
                .clone()
                .oneshot(req(
                    "POST",
                    "/m/api/v1/session-approve",
                    Some("mam_device=mm"),
                    Some(payload),
                ))
                .await
                .unwrap();
            assert_eq!(r.status(), 400, "{payload}");
            assert!(body_string(r).await.contains("bad_request"));
        }
        // 全程无任何投递
        assert!(fake.recorded_keys().is_empty());
    }

    /// guard-busy 回归锁（F1）：approve 遇 in-flight 占用 → 200 failed{「投递进行中，
    /// 请稍后重试」} 且零按键出手。守卫取在 spawn_blocking 闭包内（断连双投洞封闭）——
    /// 手动占位模拟「detached 旧投递进行中」，新触发必须让位。审计口径：忙让位无投递
    /// 发生不落审计（与 retract/jump 忙让位同口径）。独占会话 sess_p（守卫持到测尾，
    /// 全测试集唯一 id 立规）
    #[tokio::test]
    async fn approve_busy_inflight_returns_failed_without_key() {
        let fake = FakeInjector::ok();
        let state = approve_state(fake.clone(), Some(APPROVE_HIT_MSG));
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state.clone());
        let _busy = crate::inject::queue::try_acquire_inflight("sess_p").unwrap();
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-approve",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_p","optionId":"approve"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let body = body_string(r).await;
        assert!(
            body.contains("\"status\":\"failed\"") && body.contains("投递进行中，请稍后重试"),
            "in-flight 占用时 approve 应 200 failed 让位：{body}"
        );
        assert!(
            fake.recorded_keys().is_empty() && fake.recorded_key_specs().is_empty(),
            "占用期间不得出手任何按键（零注入）"
        );
        // 忙让位不落审计（无投递发生——retract/jump 忙让位同口径）
        let audits = state
            .store
            .with(|c| crate::database::dao::write_audit::recent_conn(c, 10));
        assert!(audits.is_empty(), "忙让位不落审计");
    }

    /// F2 审批族规格（服务端断言「审批路径族传递」）：codex 会话审批按键必须携带 B 族
    /// 规格（TuiFamily::Crossterm）送达注入器——构造层 key_records_for_family_dispatch
    /// （windows_console.rs，Task 3）已断言 codex 方向键按族走 VK 形态（vk=0x26），本测
    /// 锁端点侧族参数真实下传：缺族时 KV 自定义方向键会按 A 族默认形态（vk=0 字符流）
    /// 被 crossterm 静默吞。FakeInjector 覆写 locate_and_send_key_spec 记录收到的族；
    /// 键位经委托照旧入 key_calls（无 [mobile] 前缀契约一并回归）。独占会话 sess_q
    /// （默认表 codex 映射 verified 0.154.0 非严格档，approve="y"）
    #[tokio::test]
    async fn approve_passes_family_spec_to_injector() {
        let fake = FakeInjector::ok();
        let state = approve_state(fake.clone(), Some(APPROVE_HIT_MSG));
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state.clone());
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-approve",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_q","optionId":"approve"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let body = body_string(r).await;
        assert!(
            body.contains("\"status\":\"key_sent\""),
            "codex 审批应回执 key_sent：{body}"
        );
        assert_eq!(
            fake.recorded_key_specs(),
            vec![(
                24u32,
                "y".to_string(),
                crate::inject::families::TuiFamily::Crossterm
            )],
            "审批按键必须携带 codex 的 B 族（Crossterm）规格送达注入器"
        );
        assert_eq!(
            fake.recorded_keys(),
            vec![(24u32, "y".to_string())],
            "键位经 spec 覆写委托照旧记录（无 [mobile] 前缀）"
        );
        assert!(fake.recorded().is_empty(), "审批走按键通道，不走文本注入");
    }

    // ==== Task 12（H10）：zcode 无头新建端点（POST 建 + GET 前置面）====
    //
    // **测试构建的确定性**（与 Task 8 同款宪法级纪律）：`zcode::production_roots` 在
    // `cfg(test)` 下恒空表（本机真装了 ZCode，若单测也咨询真机安装路径就会**真的 spawn
    // 一个真实回合**——消耗真实账号配额并写真实 `~/.zcode`）。故端点用例覆盖的是
    // **门/参数/路径校验/安装不可达**等确定性臂；真回合的确认语义（会话库发现、诚实
    // 未确认、歧义、争用锁）由 `inject::headless::zcode_create` 的脚本缝用例覆盖。
    // 主目录一律 tempdir 夹具或 None，**绝不触真实 ~/.zcode / ~/.mam**。

    /// H10 新建端点专用 state：开关开启 + 设备 + 指定会话源 + 可注入 home_source
    /// （信任表/黑名单基准的 tempdir 夹具；None = 保守判未信任 + fail-closed 全段黑名单）
    fn zcode_create_state(
        sessions: Vec<crate::session::Session>,
        home: Option<std::path::PathBuf>,
    ) -> Arc<RemoteState> {
        let mut state = with_sessions(inject_state(FakeInjector::ok()), sessions);
        Arc::get_mut(&mut state)
            .expect("state 尚未共享（建造器返回值立即覆盖）")
            .home_source = Box::new(move || home.as_ref().map(|h| h.to_string_lossy().to_string()));
        state.store.with(|c| {
            crate::database::dao::settings::set_setting_conn(c, "remote.headless_enabled", "true")
        });
        persist_named_device(&state, "mm", "测试设备");
        state
    }

    /// zcode 会话夹具（项目路径可指到 tempdir 真实目录——端点侧存在性判定是真的 FS 判定）
    fn create_sess(
        id: &str,
        project_path: &str,
        pid: u32,
        status: crate::session::SessionStatus,
    ) -> crate::session::Session {
        let mut s = inj_sess(id, crate::session::AgentType::ZCode, pid, status);
        s.project_path = project_path.to_string();
        s.project_name = "proj".into();
        s
    }

    fn create_lock_of(project: &str) -> String {
        crate::inject::headless::zcode::create_lock_key(project, std::env::consts::OS)
    }

    /// H3 总开关是新建的**同门**（无头动作一律默认关）：关闭 → POST 403 headless_disabled
    /// （码与文案与 session-send 单点同源）+ 零审计行；GET 前置面 200 但 `available:false`
    /// 且**不给候选**（与 `session-send-info` 关闭态同口径）
    #[tokio::test]
    async fn zcode_create_requires_the_headless_switch() {
        let state = with_sessions(inject_state(FakeInjector::ok()), vec![]);
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state.clone());
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-create-zcode",
                Some("mam_device=mm"),
                Some(r#"{"project":"E:/proj"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 403, "总开关关闭 → 403（门在最前）");
        let b = body_string(r).await;
        assert!(
            b.contains("headless_disabled")
                && b.contains("无头通道未开启，请在电脑端 MAM 设置中开启"),
            "关闭态必须带单点码与逐字文案：{b}"
        );
        assert!(
            state
                .store
                .with(|c| crate::database::dao::write_audit::recent_conn(c, 10))
                .is_empty(),
            "被门拦下的请求什么动作都没发生——不得落审计行"
        );
        let r = app
            .clone()
            .oneshot(req(
                "GET",
                "/m/api/v1/session-create-zcode-info",
                Some("mam_device=mm"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "前置面：可用性在 body（同 send-info）");
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["available"], false, "{v}");
        assert_eq!(v["reasonCode"], "headless_disabled");
        assert!(
            v["reason"]
                .as_str()
                .is_some_and(|s| s.contains("无头通道未开启")),
            "{v}"
        );
        assert!(
            v.get("candidates").is_none(),
            "关闭态不给候选（表单整体置灰，与 send-info 同口径）：{v}"
        );
        // 无设备 cookie → 403 防御（gate 已拦，理论不可达）
        let r = app
            .oneshot(req(
                "GET",
                "/m/api/v1/session-create-zcode-info",
                None,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 403);
    }

    /// 参数与手填路径的**逐格诚实面**：请求不合法 400；非法路径一律 200 + `refused`
    /// （回合未起跑、`sessionId` 空串、`confirmation:"none"`、原因点名）；合法路径走到
    /// 安装不可达（cfg(test) 确定性臂）→ `spawn` 失败但同样不编会话号；每条都落 `headless`
    /// 审计行、入队零残留、项目级串行锁**已注销**
    #[tokio::test]
    async fn zcode_create_refuses_bad_input_and_illegal_paths_honestly() {
        let dir = tempfile::tempdir().unwrap();
        let proj = dir.path().join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        let proj_s = proj.to_string_lossy().to_string();
        let state = zcode_create_state(
            vec![create_sess(
                "sess_h10_act",
                &proj_s,
                0,
                crate::session::SessionStatus::Processing,
            )],
            None,
        );
        let app = router(state.clone());
        let post = |body: String| {
            let app = app.clone();
            async move {
                app.oneshot(req(
                    "POST",
                    "/m/api/v1/session-create-zcode",
                    Some("mam_device=mm"),
                    Some(&body),
                ))
                .await
                .unwrap()
            }
        };

        // ① 缺项目 / 全空白项目 / 首句超长 → 400（请求本身不合法）
        for body in [
            r#"{}"#.to_string(),
            r#"{"project":"   "}"#.to_string(),
            format!(
                r#"{{"project":"E:/proj","firstText":"{}"}}"#,
                "改".repeat(crate::remote::api::MAX_SEND_CHARS + 1)
            ),
        ] {
            let r = post(body.clone()).await;
            assert_eq!(r.status(), 400, "不合法请求：{body}");
        }
        assert!(
            state
                .store
                .with(|c| crate::database::dao::write_audit::recent_conn(c, 10))
                .is_empty(),
            "400 是请求层拒绝：不落审计行（与 session-send 同口径）"
        );

        // ② 相对路径 → refused（点名「绝对路径」）
        let r = post(r#"{"project":"proj/rel"}"#.to_string()).await;
        assert_eq!(r.status(), 200, "投递前拒绝：HTTP 200 + 语义在 body");
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["receipt"]["stage"], "refused", "{v}");
        assert_eq!(v["sessionId"], "", "未确认不得给会话号：{v}");
        assert_eq!(v["confirmation"], "none");
        assert!(
            v["receipt"]["reason"]
                .as_str()
                .is_some_and(|s| s.contains("绝对路径")),
            "拒绝必须点名原因：{v}"
        );
        assert!(
            v.get("visibility").is_none() && v.get("visibilityNote").is_none(),
            "未确认时不得承诺可见性（什么都没落到工作区）：{v}"
        );

        // ③ 敏感目录（同源文件预览黑名单；home 读不到 → fail-closed 全段匹配）
        let r = post(r#"{"project":"C:/Users/me/.ssh/proj"}"#.to_string()).await;
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["receipt"]["stage"], "refused", "{v}");
        assert!(
            v["receipt"]["reason"]
                .as_str()
                .is_some_and(|s| s.contains("敏感目录黑名单")),
            "必须点名黑名单：{v}"
        );

        // ④ 需递归创建（父目录不在场）→ refused（点名「不递归创建」）
        let deep = dir.path().join("nope").join("deep");
        let r = post(format!(
            r#"{{"project":{}}}"#,
            serde_json::json!(deep.to_string_lossy())
        ))
        .await;
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["receipt"]["stage"], "refused", "{v}");
        assert!(
            v["receipt"]["reason"]
                .as_str()
                .is_some_and(|s| s.contains("不递归创建")),
            "必须点名「不递归建目录」：{v}"
        );
        assert!(!deep.exists(), "拒绝即零副作用：绝不真的建目录");

        // ⑤ 合法在册目录 → 走到安装发现（cfg(test) 恒不可达）→ spawn 档失败，仍不编会话号
        let r = post(format!(r#"{{"project":{}}}"#, serde_json::json!(proj_s))).await;
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["receipt"]["stage"], "spawn", "{v}");
        assert!(
            v["receipt"]["reason"]
                .as_str()
                .is_some_and(|s| s.contains("安装路径不可达")),
            "{v}"
        );
        assert_eq!(v["sessionId"], "", "安装不可达 ⇒ 未确认（绝不编 sess_id）");
        assert_eq!(v["confirmation"], "none");
        assert_eq!(v["channel"], "headless_zcode");
        assert!(
            v["warning"].as_str().is_some_and(|s| s.contains("不拦截")),
            "同项目已有在册 zcode 会话 → 黄字信号（**不拦截**：本条照常走到安装发现）：{v}"
        );
        assert!(
            !crate::inject::headless::turn::registry().in_flight(&create_lock_of(&proj_s)),
            "安装不可达的拒绝臂必须注销项目级串行锁（否则该项目被自己的锁挡死）"
        );
        assert!(
            state
                .store
                .with(|c| crate::database::dao::inject_queue::pending_for_session_conn(c, &proj_s))
                .is_empty(),
            "新建绝不入队（裁决 8：无头回合每回合 spawn）"
        );
        // 审计行：四条语义拒绝各一条，末条 = spawn 失败；通道/工具/设备逐列可查，
        // 会话号列**留空**（未确认——不拿项目路径充数）
        let audits = state
            .store
            .with(|c| crate::database::dao::write_audit::recent_conn(c, 10));
        assert_eq!(audits.len(), 4, "四条拒绝必须各落一行：{audits:?}");
        assert_eq!(audits[0].action, "headless");
        assert_eq!(audits[0].channel, "headless_zcode");
        assert_eq!(audits[0].agent_type, "zcode");
        assert_eq!(audits[0].device_name, "测试设备");
        assert_eq!(
            audits[0].result, "failed(spawn) · 0ms",
            "终态+阶段码+耗时口径"
        );
        assert_eq!(audits[0].session_id, "", "未确认 = 会话号列留空（不冒充）");
        assert_eq!(audits[3].result, "failed(refused) · 0ms");
    }

    /// GET 前置面：候选 = 信任表（主源，含信任档标注 + **逐字可见性文案**）+ 看板快照项目，
    /// **准入过滤**（缺席 ∧ 同源黑名单）；`?project=` 命中同项目在册 zcode 会话才给黄字信号
    /// （干净项目不误报）
    #[tokio::test]
    async fn zcode_create_info_lists_candidates_and_yellow_signal() {
        let home = tempfile::tempdir().unwrap();
        let trusted_proj = home.path().join("trusted_proj");
        let board_proj = home.path().join("board_proj");
        let ghost_proj = home.path().join("ghost_proj"); // 不在场 → 候选过滤
        let ssh_proj = home.path().join(".ssh").join("proj"); // 同源黑名单 → 候选过滤
        std::fs::create_dir_all(&trusted_proj).unwrap();
        std::fs::create_dir_all(&board_proj).unwrap();
        std::fs::create_dir_all(&ssh_proj).unwrap();
        let v2 = home.path().join(".zcode").join("v2");
        std::fs::create_dir_all(&v2).unwrap();
        std::fs::write(
            v2.join("setting.json"),
            serde_json::json!({
                "recentProjects": [
                    trusted_proj.to_string_lossy(),
                    ghost_proj.to_string_lossy(),
                    ssh_proj.to_string_lossy(),
                ]
            })
            .to_string(),
        )
        .unwrap();
        let board_s = board_proj.to_string_lossy().to_string();
        let trusted_s = trusted_proj.to_string_lossy().to_string();
        let state = zcode_create_state(
            vec![create_sess(
                "sess_h10_info",
                &board_s,
                0,
                crate::session::SessionStatus::Idle,
            )],
            Some(home.path().to_path_buf()),
        );
        let app = router(state.clone());

        let r = app
            .clone()
            .oneshot(req(
                "GET",
                "/m/api/v1/session-create-zcode-info",
                Some("mam_device=mm"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["available"], true, "{v}");
        assert_eq!(v["tool"], "zcode");
        assert_eq!(
            v["defaultFirstText"], "hi",
            "首句默认值由后端单点下发（spec H10 探针）"
        );
        let cands = v["candidates"].as_array().unwrap();
        assert_eq!(
            cands.len(),
            2,
            "准入过滤后只留两个候选（缺席目录 + **同源黑名单目录**都被滤掉）：{cands:?}"
        );
        assert_eq!(cands[0]["path"], serde_json::json!(trusted_s));
        assert_eq!(cands[0]["source"], "trusted", "信任表主源在前：{cands:?}");
        assert_eq!(cands[0]["trusted"], true);
        assert_eq!(
            cands[0]["note"], "已信任工作区：重启 ZCode 应用后可见",
            "信任档文案由后端逐字下发（Visibility::note() 单点，前端不另编）：{cands:?}"
        );
        assert_eq!(cands[1]["path"], serde_json::json!(board_s));
        assert_eq!(cands[1]["source"], "board");
        assert_eq!(
            cands[1]["trusted"], false,
            "未信任目录必须如实标注（该新会话 APP 永不收录）：{cands:?}"
        );
        assert_eq!(cands[1]["note"], "未信任工作区：仅兔维斯可见");
        assert!(
            !cands
                .iter()
                .any(|c| c["path"].as_str().unwrap_or("").contains(".ssh")),
            "敏感目录不得出现在候选里（列表不提供创建路径必拒的候选，复审 Minor 5）：{cands:?}"
        );
        assert!(
            v.get("warning").is_none(),
            "未点名项目 → 不给黄字信号（不做无判据预警）：{v}"
        );

        // 点名有在册 zcode 会话的项目 → 黄字信号；干净项目 → 无
        let r = app
            .clone()
            .oneshot(req(
                "GET",
                &format!(
                    "/m/api/v1/session-create-zcode-info?project={}",
                    uri_encode(&board_s)
                ),
                Some("mam_device=mm"),
                None,
            ))
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert!(
            v["warning"]
                .as_str()
                .is_some_and(|s| s.contains('1') && s.contains("不拦截")),
            "同项目有在册 zcode 会话 → 黄字信号（不拦截）：{v}"
        );
        let r = app
            .oneshot(req(
                "GET",
                &format!(
                    "/m/api/v1/session-create-zcode-info?project={}",
                    uri_encode(&trusted_s)
                ),
                Some("mam_device=mm"),
                None,
            ))
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert!(v.get("warning").is_none(), "干净项目不得报警：{v}");
    }

    // ==== Task 9（H8）：codex APP 无头分派链（queue 主 / exec resume 兜底）====
    //
    // **测试构建的确定性**（与 Task 8 同款宪法级纪律）：`codex::production_presence` 在
    // `cfg(test)` 下恒判「APP 不在场」、`production_exe` 恒 None、`production_rollout_path`
    // 恒 None（不碰真实进程表 / 真实 ~/.codex / 真实 CLI），故端点用例恒落
    // **exec resume 兜底 → CLI 不可达**的如实失败臂；真回合语义（含 queue 入队 + 消费确认
    // + 单写者锁改道）由 `codex::tests` 的脚本缝用例覆盖，实机一次调用归 USER-ASSIST。

    /// codex 分派用 state：开关开启 + 设备 + **指定会话**（id 独占，守卫 id 立规②）
    fn codex_dispatch_state(sessions: Vec<crate::session::Session>) -> Arc<RemoteState> {
        let state = with_sessions(inject_state(FakeInjector::ok()), sessions);
        state.store.with(|c| {
            crate::database::dao::settings::set_setting_conn(c, "remote.headless_enabled", "true")
        });
        persist_named_device(&state, "mm", "测试设备");
        state
    }

    /// codex APP 会话夹具（H8 主形态：`form = App` + 宿主 pid；**会话号 = thread UUID**，
    /// 与读链路（state 库 `threads.id`）同源）
    const CODEX_THREAD_UUID: &str = "01a10735-1354-7d10-822a-f3bd9e041c12";
    /// 第二个 thread UUID（改道用例独占——守卫 id 立规②：裸 id 字符串不得跨用例共享）
    const CODEX_THREAD_UUID_2: &str = "01a10735-9999-7d10-822a-f3bd9e041c99";

    /// 端点级脚本结局（执行缝钩子用；`cfg(test)` 专用，**绝不真 spawn**）
    fn codex_obs(
        code: i32,
        stdout: &[&str],
        stderr: &str,
    ) -> crate::inject::headless::turn::TurnObs {
        use crate::inject::headless::receipt::{Receipt, Stage};
        let receipt = if code == 0 {
            Receipt::ok("", 3)
        } else {
            Receipt::failed(Stage::Crash, &format!("退出码 {code}；{stderr}"))
        };
        crate::inject::headless::turn::TurnObs {
            receipt,
            stdout: stdout.iter().map(|s| s.to_string()).collect(),
            stderr: stderr.to_string(),
            exit: Some(code),
        }
    }

    /// 钩子收尾守卫（panic 也清——钩子是进程级全局态，别把假 CLI 留给后续用例）
    struct CodexHookClear;
    impl Drop for CodexHookClear {
        fn drop(&mut self) {
            crate::inject::headless::codex::test_hooks::clear();
        }
    }

    fn codex_app_sess(id: &str, pid: u32) -> crate::session::Session {
        let mut s = inj_sess(
            id,
            crate::session::AgentType::Codex,
            pid,
            crate::session::SessionStatus::Idle,
        );
        s.form = crate::session::ProcessForm::App;
        s.project_path = "E:/t2".to_string();
        s
    }

    /// H8 分派链（开关开启 → 真分派臂，**不再是 `headless_pending`**）：
    /// ① HTTP 200 + `headless` 封套 + 通道 = 兜底 `headless_codex_exec`（测试构建 APP 判不在场）；
    /// ② CLI 不可达 = 如实的 `spawn` 失败（**不冒充成功、不落终端注入臂、不入队**）；
    /// ③ 落 `headless` 审计行（通道列 = 实际走向）+ 拒绝后串行锁已注销；
    /// ④ `session-send-info` 对同会话报 `injectable:true` + 路由表候选（queue 首选）——
    ///    输入区可用与实发口径同源，不再置灰。
    #[tokio::test]
    async fn codex_app_send_dispatches_headless_not_pending() {
        // 本用例断言「CLI 不可达」⇒ 必须与装填钩子的改道用例串行（钩子是进程级全局态）
        let _serial = crate::inject::headless::codex::test_hooks::LOCK
            .lock()
            .await;
        let fake = FakeInjector::ok();
        let state = codex_dispatch_state(vec![codex_app_sess(CODEX_THREAD_UUID, 77)]);
        let app = router(state.clone());
        let info = app
            .clone()
            .oneshot(req(
                "GET",
                &format!("/m/api/v1/session-send-info?session_id={CODEX_THREAD_UUID}"),
                Some("mam_device=mm"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(info.status(), 200);
        let v: serde_json::Value = serde_json::from_str(&body_string(info).await).unwrap();
        assert_eq!(
            v["injectable"], true,
            "codex APP 已接线 → 输入区必须可用：{v}"
        );
        assert_eq!(
            v["channels"],
            serde_json::json!(["headless_codex_queue", "headless_codex_exec"]),
            "候选与可见性档必须原样取自路由表单源：{v}"
        );
        assert_eq!(v["visibility"], "realtime", "APP 原生排队 ⇒ 实时可见：{v}");
        assert!(
            !v.to_string().contains("headless_pending"),
            "codex 不得再落过渡拒绝码：{v}"
        );
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-send",
                Some("mam_device=mm"),
                Some(&format!(
                    r#"{{"sessionId":"{CODEX_THREAD_UUID}","text":"hi [mobile]"}}"#
                )),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "无头分派：HTTP 200 + 语义在 body");
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["status"], "headless", "{v}");
        assert_eq!(
            v["channel"], "headless_codex_exec",
            "测试构建 APP 判不在场 ⇒ 兜底计划 exec resume：{v}"
        );
        assert_eq!(v["receipt"]["status"], "failed");
        assert_eq!(v["receipt"]["stage"], "spawn", "CLI 不可达 → spawn 档：{v}");
        assert_eq!(v["receipt"]["sessionId"], CODEX_THREAD_UUID);
        assert!(
            v["receipt"]["reason"]
                .as_str()
                .is_some_and(|s| s.contains("codex CLI 不可达")),
            "失败必须点明原因（不冒充成功）：{v}"
        );
        assert!(
            !v.to_string().contains("headless_pending"),
            "codex 真分派后不得再落过渡拒绝码：{v}"
        );
        assert_eq!(
            fake.recorded().len(),
            0,
            "无头路由会话零终端注入（不落终端注入臂）"
        );
        let audits = state
            .store
            .with(|c| crate::database::dao::write_audit::recent_conn(c, 10));
        assert_eq!(audits.len(), 1, "无头回合必须落既有 W5 单账本：{audits:?}");
        assert_eq!(audits[0].action, "headless");
        assert_eq!(
            audits[0].channel, "headless_codex_exec",
            "通道列 = 实际走向"
        );
        assert_eq!(audits[0].session_id, CODEX_THREAD_UUID);
        assert_eq!(audits[0].result, "failed(spawn) · 0ms");
        assert!(
            !crate::inject::headless::codex::registry().in_flight(CODEX_THREAD_UUID),
            "拒绝臂必须注销串行锁（否则该会话被自己的锁永久挡死）"
        );
        let pending = state.store.with(|c| {
            crate::database::dao::inject_queue::pending_for_session_conn(c, CODEX_THREAD_UUID)
        });
        assert!(pending.is_empty(), "无头回合绝不入队（裁决 8）");
    }

    /// thread id 解析不出（会话号不是 UUID 形态、rollout 路径在测试构建不可得）⇒ **投递前
    /// 拒绝**（`refused`：零字节投递）——**绝不按会话名投递**（spec H8：只认 UUID）
    #[tokio::test]
    async fn codex_app_send_without_uuid_thread_id_is_refused() {
        let fake = FakeInjector::ok();
        let state = codex_dispatch_state(vec![codex_app_sess("sess_h8_named", 78)]);
        let app = router(state.clone());
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-send",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_h8_named","text":"hi"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["status"], "headless", "{v}");
        assert_eq!(
            v["receipt"]["stage"], "refused",
            "投递前拒绝（未起跑、零字节投递）：{v}"
        );
        assert!(
            v["receipt"]["reason"]
                .as_str()
                .is_some_and(|s| s.contains("thread UUID") && s.contains("不按会话名投递")),
            "必须说清为何拒（会话名有重名歧义）：{v}"
        );
        assert_eq!(fake.recorded().len(), 0, "拒绝臂零终端注入");
        let pending = state.store.with(|c| {
            crate::database::dao::inject_queue::pending_for_session_conn(c, "sess_h8_named")
        });
        assert!(pending.is_empty(), "拒绝臂不入队");
    }

    /// **H11 三家（claude/kimi/opencode）已真分派**（Task 13/C4 收口）：`pid == 0` 时路由进
    /// 无头（`ClaudeP`/`KimiP`/`OpencodeRun`）⇒ **不再落 `headless_pending` 过渡码**，
    /// 而是进 [`api::cli_headless_dispatch`]：测试构建里 CLI 发现恒不可达（宪法级测试纪律）
    /// ⇒ 回执**如实**报 `failed(spawn)` + 「CLI 不可达」，HTTP 仍 200（语义在 body）；
    /// 且**零终端注入、零入队**。④ H3 门的最前性一并复核（开关关闭时先落 `headless_disabled`）。
    ///
    /// **串行纪律**（Task 13 复审追补）：本测断言的是「测试构建默认 CLI 不可达」——
    /// 而 [`crate::inject::headless::cli_three::test_hooks`] 能让它暂时可达（断连取证用），
    /// 故**必须**持该钩子的 LOCK 串行执行（否则并行跑到断连用例注入假 CLI 时，这里会看到
    /// `channel_error` 而不是 `spawn`——一次性假红的真实成因）。
    ///
    /// 夹具 cwd 用**真在场目录**（tempdir）：`cli_headless_dispatch` 的续接 cwd 门
    /// （会话工作目录未知/不在场 → `refused`）在 CLI 发现之前——夹具若用 `/tmp/proj`
    /// 这类 Windows 上不在场的路径，本用例就测不到「CLI 不可达」这条真正的测试纪律。
    #[tokio::test]
    async fn h11_pid_zero_sessions_dispatch_to_cli_channel_and_fail_honestly() {
        // 钩子是进程级全局态：本测断言「默认不可达」，必须与注入假 CLI 的断连用例串行
        let _serial = crate::inject::headless::cli_three::test_hooks::LOCK
            .lock()
            .await;
        let fake = FakeInjector::ok();
        let tmp = std::env::temp_dir();
        let sessions: Vec<crate::session::Session> = [
            ("sess_h11_claude", crate::session::AgentType::Claude),
            ("sess_h11_kimi", crate::session::AgentType::Kimi),
            ("sess_h11_oc", crate::session::AgentType::OpenCode),
        ]
        .into_iter()
        .map(|(id, tool)| {
            let mut s = inj_sess(id, tool, 0, crate::session::SessionStatus::Idle);
            s.project_path = tmp.to_string_lossy().to_string();
            s
        })
        .collect();
        let state = with_sessions(inject_state(fake.clone()), sessions);
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state.clone());
        // ④ H3 门在最前（开关默认关）：无头绑定会话先被开关拦下
        for sid in ["sess_h11_claude", "sess_h11_kimi", "sess_h11_oc"] {
            let r = app
                .clone()
                .oneshot(req(
                    "POST",
                    "/m/api/v1/session-send",
                    Some("mam_device=mm"),
                    Some(&format!(r#"{{"sessionId":"{sid}","text":"hi"}}"#)),
                ))
                .await
                .unwrap();
            assert_eq!(r.status(), 403, "{sid}");
            assert!(body_string(r).await.contains("headless_disabled"), "{sid}");
        }
        // 开关开启 → 真分派臂：CLI 不可达（测试构建恒不可达）⇒ 如实失败，**不是**过渡码
        state.store.with(|c| {
            crate::database::dao::settings::set_setting_conn(c, "remote.headless_enabled", "true")
        });
        let want_channel = [
            ("sess_h11_claude", "headless_claude_p", "claude"),
            ("sess_h11_kimi", "headless_kimi_p", "kimi"),
            ("sess_h11_oc", "headless_opencode_run", "opencode"),
        ];
        for (sid, channel, program) in want_channel {
            let r = app
                .clone()
                .oneshot(req(
                    "POST",
                    "/m/api/v1/session-send",
                    Some("mam_device=mm"),
                    Some(&format!(r#"{{"sessionId":"{sid}","text":"hi"}}"#)),
                ))
                .await
                .unwrap();
            assert_eq!(r.status(), 200, "{sid}");
            let body = body_string(r).await;
            assert!(
                !body.contains("headless_pending"),
                "{sid} 过渡码必须已消失（Task 13 收口）: {body}"
            );
            let v: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert_eq!(v["status"], "headless", "{sid}");
            assert_eq!(v["channel"], channel, "{sid}");
            assert_eq!(v["receipt"]["status"], "failed", "{sid}");
            assert_eq!(v["receipt"]["stage"], "spawn", "{sid}");
            assert!(
                v["receipt"]["reason"]
                    .as_str()
                    .unwrap()
                    .contains("CLI 不可达"),
                "{sid} 必须如实报 CLI 不可达（测试构建绝不 spawn 真 CLI）: {body}"
            );
            assert!(
                v["receipt"]["reason"].as_str().unwrap().contains(program),
                "{sid} 失败原因必须点名是哪家 CLI（{program}）: {body}"
            );
            let pending = state
                .store
                .with(|c| crate::database::dao::inject_queue::pending_for_session_conn(c, sid));
            assert!(pending.is_empty(), "{sid} 不入队");
        }
        assert_eq!(fake.recorded().len(), 0, "无头家零终端注入");
    }

    // ==== R3（P0 安全）：cmd 垫片路径上「带元字符的用户正文」= 投递前拒绝 ====

    /// R3 夹具：**无害** `.cmd` 垫片——**被真的执行时**才写一个标记文件。
    /// 返回（垫片路径，标记文件路径）。「零投递」的物证：拒绝臂若失效，垫片就会起进程、
    /// 标记文件出现（对照组反过来证明本机垫片形态**真的会**起进程，否则该断言是空的）。
    fn r3_probe_shim(tag: &str) -> (String, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("mam_r3_{tag}_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("ran.txt");
        let shim = dir.join("probe.cmd");
        std::fs::write(
            &shim,
            format!(
                "@echo off\r\n> \"{}\" echo ran\r\nexit /b 0\r\n",
                marker.display()
            ),
        )
        .unwrap();
        let _ = std::fs::remove_file(&marker); // 起点干净（同进程内重复跑本用例时）
        (shim.to_string_lossy().to_string(), marker)
    }

    /// **R3 票面第一条（端点级取证）**：`cmd` 垫片 + 元字符正文 ⇒ `refused` + **零投递**。
    ///
    /// 形态：直接注入 `cmd /c <无害垫片>`（生产里由 `turn::spawn_shape` 在 Windows 上为
    /// `.cmd`/`.bat` 产出）。判据只看**形态 + 正文**，故与宿主平台无关（非 Windows 上
    /// 注入这种形态在生产不会出现，但「见到 cmd 形态就拒」这条纪律本身是平台无关的）。
    ///
    /// 断言的三件事：① `refused`（既有投递前拒绝档：未起跑、零字节投递）；
    /// ② **零投递**——标记文件必须不存在（没有任何进程被起）；③ 拒绝后串行锁已注销、
    /// 未入队、零终端注入（诚实收尾）。
    ///
    /// 对照组（仅 Windows 可证）：**普通正文**在同一垫片形态下必须**放行**，且垫片
    /// 真的起进程（标记出现）——证明「标记不出现」不是空断言。
    #[tokio::test]
    async fn cmd_shim_metachar_body_is_refused_with_zero_delivery() {
        let _serial = crate::inject::headless::cli_three::test_hooks::LOCK
            .lock()
            .await;
        let _clear = CliHookClear;
        let sid = "sess_r3_kimi_shim";
        let (shim, marker) = r3_probe_shim("kimi");
        crate::inject::headless::cli_three::test_hooks::set_shape(Some(
            crate::inject::headless::cli_three::SpawnShape {
                program: "cmd".to_string(),
                prefix: vec!["/c".to_string(), shim],
            },
        ));
        let fake = FakeInjector::ok();
        let mut s = inj_sess(
            sid,
            crate::session::AgentType::Kimi,
            0,
            crate::session::SessionStatus::Idle,
        );
        s.project_path = std::env::temp_dir().to_string_lossy().to_string();
        let state = with_sessions(inject_state(fake.clone()), vec![s]);
        state.store.with(|c| {
            crate::database::dao::settings::set_setting_conn(c, "remote.headless_enabled", "true")
        });
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state.clone());

        // ── 对照组：普通正文 ⇒ 放行（且垫片真的起进程 —— Windows 上标记文件会出现）──
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-send",
                Some("mam_device=mm"),
                Some(&format!(r#"{{"sessionId":"{sid}","text":"hi"}}"#)),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_ne!(
            v["receipt"]["stage"], "refused",
            "普通正文不得被拒（判据必须只看正文里的元字符）: {v}"
        );
        for _ in 0..200 {
            if marker.is_file() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        #[cfg(windows)]
        assert!(
            marker.is_file(),
            "对照组：本机垫片形态真的会起进程（否则零投递断言无意义）"
        );
        let _ = std::fs::remove_file(&marker); // 只看下面那次的物证
                                               // 对照组回合收尾（槽位在任务内注销）后才能测拒绝臂本身
        for _ in 0..500 {
            if !crate::inject::headless::turn::registry().in_flight(sid) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(
            !crate::inject::headless::turn::registry().in_flight(sid),
            "对照组回合必须已收尾（否则下面的拒绝可能是被串行锁挡下的，测错了东西）"
        );

        // ── 本体：垫片 + 元字符正文 ⇒ 投递前拒绝 + 零投递 ──
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-send",
                Some("mam_device=mm"),
                Some(&format!(
                    r#"{{"sessionId":"{sid}","text":"跑一下 & rem 注入面 %PATH%"}}"#
                )),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "HTTP 200 + 语义在 body");
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["status"], "headless", "{v}");
        assert_eq!(v["receipt"]["status"], "failed", "{v}");
        assert_eq!(
            v["receipt"]["stage"], "refused",
            "cmd 垫片 + 元字符正文必须是**投递前拒绝**（未起跑、零字节投递）: {v}"
        );
        assert!(
            v["receipt"]["reason"]
                .as_str()
                .is_some_and(|s| s.contains("cmd") && s.contains('%')),
            "拒绝原因必须说清「cmd 重解析」并点名元字符（同斜杠命令拒绝的显式告知风格）: {v}"
        );
        assert!(
            !marker.is_file(),
            "零投递：拒绝臂不得起任何进程（垫片标记文件必须不存在）"
        );
        assert!(
            !crate::inject::headless::turn::registry().in_flight(sid),
            "拒绝臂必须注销占位（否则该会话被自己的串行锁永久挡死）"
        );
        let pending = state
            .store
            .with(|c| crate::database::dao::inject_queue::pending_for_session_conn(c, sid));
        assert!(pending.is_empty(), "拒绝臂不入队（零字节投递）");
        assert_eq!(fake.recorded().len(), 0, "拒绝臂零终端注入");
        let audits = state
            .store
            .with(|c| crate::database::dao::write_audit::recent_conn(c, 10));
        assert_eq!(
            audits.len(),
            2,
            "对照组 + 拒绝臂各一行账（拒绝也必须落账，且如实记 refused）: {audits:?}"
        );
        assert!(
            audits.iter().any(|a| a.result.contains("refused")),
            "拒绝臂审计必须记 refused（不得冒充成功）: {audits:?}"
        );
    }

    /// **R3 票面第一条（codex 臂）**：本机 codex 常是 npm 垫片（`codex.cmd`）——而
    /// [`crate::inject::headless::turn::spawn_shape`] 只在 **Windows** 上对 `.cmd`/`.bat`
    /// 产出 `cmd /c`，故本用例 `cfg(windows)`（非 Windows 上形态是直 spawn，拒绝臂
    /// **正确地**不适用；该平台的判据面无此风险）。
    ///
    /// 断言：① 垫片 + 元字符 ⇒ `refused`（点名 cmd 重解析与具体元字符）+ 串行锁已注销
    /// + 不入队；② **对照组**：同一垫片形态 + 普通正文 ⇒ 不得被拒（判据只看正文）。
    ///
    /// 另钉住**顺序**：探针钩子装的是 `probe_pass = false`——拒绝发生在**版本探针之前**，
    /// 故回执必须是 `refused` 而不是版本门档（谁把顺序改成「先探针后拒绝」，本用例即红）。
    #[cfg(windows)]
    #[tokio::test]
    async fn codex_cmd_shim_metachar_body_is_refused_before_probe_and_spawn() {
        use crate::inject::headless::codex::test_hooks;
        // 独占会话号（守卫 id 立规②：裸 id 字符串不得跨用例共享；必须是**合法 UUID**，
        // 否则 dispatch 会先在 thread UUID 解析处拒绝——测不到本用例的判据面）
        const SID: &str = "01a10735-3a05-7d10-822a-f3bd9e041c3a";
        let _serial = test_hooks::LOCK.lock().await;
        test_hooks::set(Some("C:/fake/codex.cmd"), false, Vec::new());
        let _clear = CodexHookClear; // panic 也清钩子
        let state = codex_dispatch_state(vec![codex_app_sess(SID, 0)]);
        let app = router(state.clone());
        // ① 元字符正文 ⇒ 投递前拒绝
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-send",
                Some("mam_device=mm"),
                Some(&format!(
                    r#"{{"sessionId":"{SID}","text":"hi & whoami %PATH%"}}"#
                )),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["status"], "headless", "{v}");
        assert_eq!(v["receipt"]["status"], "failed", "{v}");
        assert_eq!(
            v["receipt"]["stage"], "refused",
            "codex 垫片 + 元字符必须是**投递前拒绝**（未起跑、零字节投递）: {v}"
        );
        assert!(
            v["receipt"]["reason"]
                .as_str()
                .is_some_and(|s| s.contains("cmd") && s.contains('%')),
            "拒绝原因必须说清 cmd 重解析并点名具体元字符（显式告知，不静默改正文）: {v}"
        );
        assert!(
            !crate::inject::headless::codex::registry().in_flight(SID),
            "拒绝臂必须注销占位（否则该会话被自己的串行锁永久挡死）"
        );
        let pending = state
            .store
            .with(|c| crate::database::dao::inject_queue::pending_for_session_conn(c, SID));
        assert!(pending.is_empty(), "拒绝臂不入队（零字节投递）");
        // ② 对照组：同一垫片 + 普通正文 ⇒ 放行（此处由版本门钩子收尾，绝不真 spawn）
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-send",
                Some("mam_device=mm"),
                Some(&format!(r#"{{"sessionId":"{SID}","text":"hi"}}"#)),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let v2: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_ne!(
            v2["receipt"]["stage"], "refused",
            "普通正文不得被拒（判据必须只看正文里的元字符）: {v2}"
        );
    }

    // ==== R12-S2（裁决 24b）：花名 cmd 安全白名单 —— 端点级三臂取证 ====
    //
    // 形态统一：设备表里**直接落**一个危险花名（[`persist_named_device`] 绕过注册点——
    // 「存量设备」正是这个形态；本轮注册点只挡新增，旧的靠**投递点**兜住，见
    // `headless::turn::device_name_refusal` 的调用点节）。三臂都断言：`refused`（未起跑、
    // 零字节投递）+ 原因**点名具体字符** + 串行锁未占/已注销 + 未入队 + 零终端注入。
    // 判据缺席时本条会落到 spawn / 版本门档 ⇒ 断言即红（不是「恰好没走到」）。

    /// 三臂共用夹具：开关开启 + `mm`（正常花名）+ `legacy`（**危险花名**，直接落库）+
    /// 注入器可见（用于断言零终端注入）
    fn unsafe_name_dispatch_state(
        sessions: Vec<crate::session::Session>,
        unsafe_name: &str,
    ) -> (Arc<RemoteState>, Arc<FakeInjector>) {
        let fake = FakeInjector::ok();
        let state = with_sessions(inject_state(fake.clone()), sessions);
        state.store.with(|c| {
            crate::database::dao::settings::set_setting_conn(c, "remote.headless_enabled", "true")
        });
        persist_named_device(&state, "mm", "测试设备");
        persist_named_device(&state, "legacy", unsafe_name);
        (state, fake)
    }

    /// 三臂共用的拒绝断言（票面四件 + 两条诚实收尾）
    fn assert_unsafe_name_refused(
        v: &serde_json::Value,
        sid: &str,
        c: char,
        state: &Arc<RemoteState>,
        fake: &Arc<FakeInjector>,
    ) {
        assert_eq!(v["status"], "headless", "{v}");
        assert_eq!(v["receipt"]["status"], "failed", "{v}");
        assert_eq!(
            v["receipt"]["stage"], "refused",
            "花名白名单 = **投递前拒绝**（未起跑、零字节投递）: {v}"
        );
        let reason = v["receipt"]["reason"].as_str().unwrap_or_default();
        assert!(
            reason.contains(c),
            "拒绝原因必须**点名具体字符**（用户据此改名）: {v}"
        );
        assert!(
            reason.contains("cmd") && reason.contains("未投递") && reason.contains("改名"),
            "拒绝原因必须说清 cmd 重解析面 + 零字节投递 + 给出路（改名）: {v}"
        );
        assert!(
            !crate::inject::headless::turn::registry().in_flight(sid),
            "拒绝臂不得留下占位（否则该会话被自己的串行锁挡死）"
        );
        let pending = state
            .store
            .with(|c| crate::database::dao::inject_queue::pending_for_session_conn(c, sid));
        assert!(pending.is_empty(), "拒绝臂不入队（零字节投递）");
        assert_eq!(fake.recorded().len(), 0, "拒绝臂零终端注入");
    }

    /// **codex 臂**（`queue --message` / `exec resume` 都把载荷放 argv；本机常是 npm 垫片）
    #[tokio::test]
    async fn codex_legacy_unsafe_device_name_is_refused_with_zero_delivery() {
        // 合法 UUID（过 dispatch 的 thread UUID 步）；独占字符串（守卫 id 立规②）
        const SID: &str = "01a10735-3a05-7d10-822a-f3bd9e041c3c";
        let (state, fake) = unsafe_name_dispatch_state(vec![codex_app_sess(SID, 0)], "花&名");
        let app = router(state.clone());
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-send",
                Some("mam_device=legacy"),
                Some(&format!(r#"{{"sessionId":"{SID}","text":"hi"}}"#)),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "HTTP 200 + 语义在 body");
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_unsafe_name_refused(&v, SID, '&', &state, &fake);
    }

    /// **kimi 臂**（`kimi --prompt <载荷>`：签名进 argv）
    ///
    /// 注入的假 CLI 形态只为**过 CLI 发现那一步**（本用例在组装步就被拒，永不 spawn；
    /// program 故意取一个不存在的名字——判据若缺席，回执会是 `spawn` 而不是 `refused`）。
    #[tokio::test]
    async fn kimi_legacy_unsafe_device_name_is_refused_with_zero_delivery() {
        use crate::inject::headless::cli_three::test_hooks;
        const SID: &str = "sess_r12_name_kimi";
        let _serial = test_hooks::LOCK.lock().await;
        test_hooks::set_shape(Some(crate::inject::headless::cli_three::SpawnShape {
            program: "mam-r12-must-not-spawn.exe".to_string(),
            prefix: Vec::new(),
        }));
        let _clear = CliHookClear;
        let mut s = inj_sess(
            SID,
            crate::session::AgentType::Kimi,
            0,
            crate::session::SessionStatus::Idle,
        );
        // cwd 门在 CLI 发现之前：夹具路径必须真在场（tempdir），否则测的是 cwd 门不是花名门
        s.project_path = std::env::temp_dir().to_string_lossy().to_string();
        let (state, fake) = unsafe_name_dispatch_state(vec![s], "手机%1");
        let app = router(state.clone());
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-send",
                Some("mam_device=legacy"),
                Some(&format!(r#"{{"sessionId":"{SID}","text":"hi"}}"#)),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "HTTP 200 + 语义在 body");
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_unsafe_name_refused(&v, SID, '%', &state, &fake);
    }

    /// **opencode 臂**（`opencode run --session <id> <载荷>`：签名进 argv 位置参数）
    #[tokio::test]
    async fn opencode_legacy_unsafe_device_name_is_refused_with_zero_delivery() {
        use crate::inject::headless::cli_three::test_hooks;
        const SID: &str = "sess_r12_name_oc";
        let _serial = test_hooks::LOCK.lock().await;
        test_hooks::set_shape(Some(crate::inject::headless::cli_three::SpawnShape {
            program: "mam-r12-must-not-spawn.exe".to_string(),
            prefix: Vec::new(),
        }));
        let _clear = CliHookClear;
        let mut s = inj_sess(
            SID,
            crate::session::AgentType::OpenCode,
            0,
            crate::session::SessionStatus::Idle,
        );
        s.project_path = std::env::temp_dir().to_string_lossy().to_string();
        let (state, fake) = unsafe_name_dispatch_state(vec![s], "say\"hi");
        let app = router(state.clone());
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-send",
                Some("mam_device=legacy"),
                Some(&format!(r#"{{"sessionId":"{SID}","text":"hi"}}"#)),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "HTTP 200 + 语义在 body");
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_unsafe_name_refused(&v, SID, '"', &state, &fake);
    }

    // ==== Task 13（C4 / H11）：审批卡两端点（GET 待答载荷 / POST 决策）====

    /// H11 state：开关开启 + 设备 + 指定会话（id 独占；cwd 用**真在场目录**——续接 cwd 门
    /// 在 CLI 发现之前，夹具路径不在场就测不到真分派臂）
    fn cli_dispatch_state(
        sessions: Vec<crate::session::Session>,
    ) -> std::sync::Arc<super::RemoteState> {
        let state = with_sessions(inject_state(FakeInjector::ok()), sessions);
        state.store.with(|c| {
            crate::database::dao::settings::set_setting_conn(c, "remote.headless_enabled", "true")
        });
        persist_named_device(&state, "mm", "测试设备");
        state
    }

    /// `session-send-info` 对三家报**真实状态**（已接线 ⇒ 输入区可用 + 路由表候选/可见性同源）
    #[tokio::test]
    async fn h11_send_info_reports_real_state_per_tool() {
        use crate::session::{AgentType, SessionStatus};
        let tmp = std::env::temp_dir().to_string_lossy().to_string();
        let cases = [
            (
                "sess_c3_info_claude",
                AgentType::Claude,
                "headless_claude_p",
                "after_refresh",
            ),
            (
                "sess_c3_info_kimi",
                AgentType::Kimi,
                "headless_kimi_p",
                "after_refresh",
            ),
            (
                "sess_c3_info_oc",
                AgentType::OpenCode,
                "headless_opencode_run",
                "realtime",
            ),
        ];
        let sessions: Vec<crate::session::Session> = cases
            .iter()
            .map(|(id, tool, _, _)| {
                let mut s = inj_sess(id, tool.clone(), 0, SessionStatus::Idle);
                s.project_path = tmp.clone();
                s
            })
            .collect();
        let state = cli_dispatch_state(sessions);
        let app = router(state.clone());
        for (sid, _, channel, visibility) in cases {
            let info = app
                .clone()
                .oneshot(req(
                    "GET",
                    &format!("/m/api/v1/session-send-info?session_id={sid}"),
                    Some("mam_device=mm"),
                    None,
                ))
                .await
                .unwrap();
            assert_eq!(info.status(), 200, "{sid}");
            let v: serde_json::Value = serde_json::from_str(&body_string(info).await).unwrap();
            assert_eq!(v["injectable"], true, "{sid} 已接线 → 输入区可用：{v}");
            assert_eq!(v["channels"], serde_json::json!([channel]), "{sid}: {v}");
            assert_eq!(v["visibility"], visibility, "{sid}: {v}");
            assert!(
                !v.to_string().contains("headless_pending"),
                "{sid} 不得再落过渡码：{v}"
            );
        }
    }

    /// 审批卡**全链（端点侧纯核）**：GET 取待答载荷（工具名 + 命令原文 + tier/permissionMode
    /// + 选项词）→ POST 决策 → `delivered:true` + `headless_approve` 审计行（result=决策词、
    /// 通道列、正文=「工具: 入参」、耗时=待答时长）→ 再 GET 已无待答项 → 重复 POST 如实报未送达。
    #[tokio::test]
    async fn headless_approval_round_trip_then_honest_not_delivered() {
        use crate::inject::headless::cli_three as c3;
        let sid = "sess_c3_approval_ep";
        let state = cli_dispatch_state(vec![]);
        let app = router(state.clone());
        // 装一个待答审批（模拟长驻回合在等）：rx 由本用例持有，验证决策真的投给了回合
        let (pending, mut rx) = c3::pending_for_test(
            "req-ep-1",
            c3::PendingKind::Approval,
            "Bash",
            "ls -la",
            Vec::new(),
            sid,
            "headless_claude_p",
        );
        c3::pending_registry()
            .register(pending)
            .expect("登记待答项");
        // GET：卡面载荷（命令原文 + 档位 + 选项 id 词表）
        let r = app
            .clone()
            .oneshot(req(
                "GET",
                &format!("/m/api/v1/session-headless-approval?session_id={sid}"),
                Some("mam_device=mm"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["pending"]["requestId"], "req-ep-1");
        assert_eq!(v["pending"]["kind"], "approval");
        assert_eq!(v["pending"]["toolName"], "Bash");
        assert_eq!(v["pending"]["input"], "ls -la");
        assert_eq!(v["pending"]["tier"], "stdio");
        assert_eq!(v["pending"]["permissionMode"], "default");
        assert_eq!(
            v["pending"]["options"],
            serde_json::json!([
                {"id": "allow", "label": "允许"},
                {"id": "deny", "label": "拒绝"},
            ])
        );
        // POST allow → 送达 + 审计行
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-headless-approve",
                Some("mam_device=mm"),
                Some(&format!(
                    r#"{{"sessionId":"{sid}","requestId":"req-ep-1","decision":"allow"}}"#
                )),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["delivered"], true, "{v}");
        assert_eq!(
            rx.try_recv().unwrap(),
            c3::Decision::Allow,
            "决策必须交给回合"
        );
        let audits = state
            .store
            .with(|c| crate::database::dao::write_audit::recent_conn(c, 10));
        assert_eq!(audits.len(), 1, "送达才落账：{audits:?}");
        assert_eq!(audits[0].action, "headless_approve");
        assert!(
            audits[0].result.starts_with("allow · "),
            "result 列 = 决策词 + 待答时长（H6 耗时编码同规）：{:?}",
            audits[0].result
        );
        assert_eq!(audits[0].channel, "headless_claude_p");
        assert_eq!(audits[0].agent_type, "claude");
        assert_eq!(audits[0].session_id, sid);
        assert_eq!(audits[0].device_name, "测试设备");
        assert!(
            audits[0].summary.contains("Bash") && audits[0].summary.contains("ls -la"),
            "审计摘要必须能回答「谁批准了什么」：{:?}",
            audits[0].summary
        );
        // 再 GET：无待答项
        let r = app
            .clone()
            .oneshot(req(
                "GET",
                &format!("/m/api/v1/session-headless-approval?session_id={sid}"),
                Some("mam_device=mm"),
                None,
            ))
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert!(v["pending"].is_null(), "{v}");
        // 重复 POST：如实报未送达（不谎报已答、不落第二行审计）
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-headless-approve",
                Some("mam_device=mm"),
                Some(&format!(
                    r#"{{"sessionId":"{sid}","requestId":"req-ep-1","decision":"allow"}}"#
                )),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["delivered"], false, "{v}");
        assert!(v["reason"].as_str().unwrap().contains("没有待答"), "{v}");
        let audits = state
            .store
            .with(|c| crate::database::dao::write_audit::recent_conn(c, 10));
        assert_eq!(audits.len(), 1, "未送达不得落账：{audits:?}");
    }

    /// 端点参数与完整性门（belt & braces 的**端点侧**）：缺参 400 / 未知决策词 400 /
    /// `answer` 未答全 400 且**逐题点名** / 无设备 cookie 403；且未答全时**不消费**待答项
    /// （用户可改答案再提交——防静默丢题的端到端保证）
    #[tokio::test]
    async fn headless_approve_rejects_bad_params_and_incomplete_answers() {
        use crate::inject::headless::cli_three as c3;
        let sid = "sess_c3_approval_q";
        let state = cli_dispatch_state(vec![]);
        let app = router(state.clone());
        let (pending, mut rx) = c3::pending_for_test(
            "req-q-1",
            c3::PendingKind::Question,
            "AskUserQuestion",
            "{\"questions\":[]}",
            // **登记题集是核侧权威**（多选判定与完整性都从它取）：端点用例必须给真题集
            vec![
                c3::Q::multi("选框架", vec!["a", "b"], vec![]),
                c3::Q::single("确认?", vec!["y", "n"], ""),
            ],
            sid,
            "headless_claude_p",
        );
        c3::pending_registry()
            .register(pending)
            .expect("登记待答项");
        let post = |body: String| {
            let app = app.clone();
            async move {
                app.oneshot(req(
                    "POST",
                    "/m/api/v1/session-headless-approve",
                    Some("mam_device=mm"),
                    Some(&body),
                ))
                .await
                .unwrap()
            }
        };
        // 缺参 / 未知词 → 400
        assert_eq!(
            post(r#"{"sessionId":"","requestId":"r","decision":"allow"}"#.into())
                .await
                .status(),
            400
        );
        assert_eq!(
            post(format!(
                r#"{{"sessionId":"{sid}","requestId":"req-q-1","decision":"maybe"}}"#
            ))
            .await
            .status(),
            400
        );
        // answer 无载荷 → 400
        assert_eq!(
            post(format!(
                r#"{{"sessionId":"{sid}","requestId":"req-q-1","decision":"answer"}}"#
            ))
            .await
            .status(),
            400
        );
        // answer 未答全（多选空标签）→ 400 + 逐题点名 + 待答项**仍在**（可改后重提交）
        let r = post(format!(
            r#"{{"sessionId":"{sid}","requestId":"req-q-1","decision":"answer","answers":[{{"question":"选框架","labels":[]}},{{"question":"确认?","labels":["y"]}}]}}"#
        ))
        .await;
        assert_eq!(r.status(), 400);
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert!(v["reason"].as_str().unwrap().contains("未答全"), "{v}");
        assert_eq!(v["missing"], serde_json::json!(["选框架"]), "{v}");
        assert!(
            c3::pending_registry().has(sid),
            "未答全被拒后待答项必须还在（用户可补答——不是静默丢题）"
        );
        // 答全 → 送达（多选 = 数组，单选题 = 字符串——由 build_ask_answers 单点决定）
        let r = post(format!(
            r#"{{"sessionId":"{sid}","requestId":"req-q-1","decision":"answer","answers":[{{"question":"选框架","labels":["a","b"]}},{{"question":"确认?","labels":["y"]}}]}}"#
        ))
        .await;
        assert_eq!(r.status(), 200);
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["delivered"], true, "{v}");
        match rx.try_recv().unwrap() {
            c3::Decision::Answer(set) => {
                let answers = c3::build_ask_answers(set.questions());
                assert!(answers.contains(r#""选框架":["a","b"]"#), "{answers}");
                assert!(answers.contains(r#""确认?":"y""#), "{answers}");
            }
            other => panic!("必须是 Answer 决策：{other:?}"),
        }
        // 无设备 cookie → 403 防御（两道端点同规）
        let r = app
            .clone()
            .oneshot(req(
                "GET",
                &format!("/m/api/v1/session-headless-approval?session_id={sid}"),
                None,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 403);
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-headless-approve",
                None,
                Some(&format!(
                    r#"{{"sessionId":"{sid}","requestId":"r","decision":"allow"}}"#
                )),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 403);
    }

    /// **空题面 = 坏参数**（R11 顺手项）：`HeadlessAnswerEntry.question` 字段**缺席**或**全空白**
    /// 必须走**同一条 400 坏参数路径**（`error=bad_request`）——不是 serde 拒绝的 422，也不是被
    /// `answer_set_from_entries` 的「题面不在本回合的题集里（陈旧页面或串话）」措辞误导
    /// （那会把「客户端漏传字段」说成「页面陈旧」，排查方向是错的）。且**零消费**：待答项仍在、
    /// 决策未交给回合、零审计行——随后答全仍能送达（卡没被吃掉）。
    #[tokio::test]
    async fn headless_approve_rejects_empty_question_text() {
        use crate::inject::headless::cli_three as c3;
        let sid = "sess_c3_approval_empty_q";
        let state = cli_dispatch_state(vec![]);
        let app = router(state.clone());
        let (pending, mut rx) = c3::pending_for_test(
            "req-eq-1",
            c3::PendingKind::Question,
            "AskUserQuestion",
            "{\"questions\":[]}",
            vec![c3::Q::single("确认?", vec!["y", "n"], "")],
            sid,
            "headless_claude_p",
        );
        c3::pending_registry()
            .register(pending)
            .expect("登记待答项");
        let post = |body: String| {
            let app = app.clone();
            async move {
                app.oneshot(req(
                    "POST",
                    "/m/api/v1/session-headless-approve",
                    Some("mam_device=mm"),
                    Some(&body),
                ))
                .await
                .unwrap()
            }
        };
        // ① 题面**字段缺席** → 400（`#[serde(default)]` 之前这里是 serde 拒绝的 422：字段缺失
        //    在 axum 的 `Json` 提取器里是反序列化失败，直接 422 且措辞不提「题面」）
        let r = post(format!(
            r#"{{"sessionId":"{sid}","requestId":"req-eq-1","decision":"answer","answers":[{{"labels":["y"]}}]}}"#
        ))
        .await;
        assert_eq!(
            r.status(),
            400,
            "题面缺席必须走既有 400 坏参数路径（不是 serde 的 422）"
        );
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["error"], "bad_request", "{v}");
        assert!(
            v["reason"].as_str().unwrap().contains("题面"),
            "原因必须点名题面缺失：{v}"
        );
        // ② 题面**全空白** → 同判（空白题面同样不是「题面不在题集」，是坏参数）
        let r = post(format!(
            r#"{{"sessionId":"{sid}","requestId":"req-eq-1","decision":"answer","answers":[{{"question":"   ","labels":["y"]}}]}}"#
        ))
        .await;
        assert_eq!(r.status(), 400, "空白题面同判");
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["error"], "bad_request", "{v}");
        // ③ 零消费：待答项还在、决策没交给回合、零审计行（两次被拒都不许动卡）
        assert!(
            c3::pending_registry().has(sid),
            "坏参数被拒后待答项必须还在（拒的是这一发，不是这张卡）"
        );
        assert!(rx.try_recv().is_err(), "坏参数不得把决策交给回合");
        assert!(
            state
                .store
                .with(|c| crate::database::dao::write_audit::recent_conn(c, 10))
                .is_empty(),
            "被参数门拦下的请求什么动作都没发生——不得落审计行"
        );
        // ④ 卡仍可用：答全 → 送达（证明零消费，不是「静默丢卡」）
        let r = post(format!(
            r#"{{"sessionId":"{sid}","requestId":"req-eq-1","decision":"answer","answers":[{{"question":"确认?","labels":["y"]}}]}}"#
        ))
        .await;
        assert_eq!(r.status(), 200);
        assert!(matches!(rx.try_recv().unwrap(), c3::Decision::Answer(_)));
    }

    /// **审批端点与 H3 总开关同冻**（R4 / 裁决 22）：开关关闭 ⇒ **403 `headless_disabled`**
    /// （码 + 逐字文案与 session-send / session-create-zcode 单点同源），且**零消费**——
    /// 待答项仍在、决策未交给回合、零审计行；**重新开启后同一张卡仍可正常应答**（冻结 ≠ 丢弃，
    /// 这正是「关中途开关 = 冻结在飞审批卡」的语义：回合由 watchdog 诚实终结，不伪造成功）。
    /// 开关开启 ⇒ 既有路径原样（送达 + `headless_approve` 审计行）。
    ///
    /// **门位**（与新建端点 ①参数 ②设备 ③开关 同序）：参数校验（400）与设备身份（403 防御）
    /// 之后、待答项查询之前。用例把三段顺序都钉住：缺参在关闭态仍回 400、无 cookie 仍回
    /// `forbidden`（不是 `headless_disabled`）——否则「门在最前」会把既有失败契约改掉。
    #[tokio::test]
    async fn headless_approve_requires_the_headless_switch() {
        use crate::inject::headless::cli_three as c3;
        let sid = "sess_c3_approval_switch";
        let state = cli_dispatch_state(vec![]); // 开关初始 = 开（cli_dispatch_state 种 "true"）
        let app = router(state.clone());
        let set_switch = |v: &str| {
            state.store.with(|c| {
                // 键名字面量：与 Rust 端 KEY_HEADLESS 常量双锁（与 cli_dispatch_state 同款）
                crate::database::dao::settings::set_setting_conn(c, "remote.headless_enabled", v)
            })
        };
        // **回合已在等应答（在飞审批卡）时，用户去电脑端把开关关了**
        set_switch("false");
        let (pending, mut rx) = c3::pending_for_test(
            "req-sw-1",
            c3::PendingKind::Approval,
            "Bash",
            "ls -la",
            Vec::new(),
            sid,
            "headless_claude_p",
        );
        c3::pending_registry()
            .register(pending)
            .expect("登记待答项");
        let post = |body: String| {
            let app = app.clone();
            async move {
                app.oneshot(req(
                    "POST",
                    "/m/api/v1/session-headless-approve",
                    Some("mam_device=mm"),
                    Some(&body),
                ))
                .await
                .unwrap()
            }
        };
        // ① 关闭态 → 403 + 单点码与逐字文案（与 session-create-zcode 同源，不是第二份）
        let allow_body =
            format!(r#"{{"sessionId":"{sid}","requestId":"req-sw-1","decision":"allow"}}"#);
        let r = post(allow_body.clone()).await;
        assert_eq!(r.status(), 403, "总开关关闭 ⇒ 审批端点必须与 H3 同冻");
        let b = body_string(r).await;
        assert!(
            b.contains("\"error\":\"headless_disabled\"")
                && b.contains("无头通道未开启，请在电脑端 MAM 设置中开启"),
            "关闭态必须带单点码与逐字文案：{b}"
        );
        // ② 零消费：待答项还在、决策没交给回合、零审计行（冻结 ≠ 丢弃，也 ≠ 悄悄吞掉）
        assert!(
            c3::pending_registry().has(sid),
            "关闭态不得消费待答项（重新开启后同一张卡还要能用）"
        );
        assert!(rx.try_recv().is_err(), "关闭态不得把决策交给回合");
        assert!(
            state
                .store
                .with(|c| crate::database::dao::write_audit::recent_conn(c, 10))
                .is_empty(),
            "被门拦下的请求什么动作都没发生——不得落审计行"
        );
        // ③ 门位：参数校验在前（缺参仍 400，不被开关吞成 403）
        assert_eq!(
            post(r#"{"sessionId":"","requestId":"r","decision":"allow"}"#.into())
                .await
                .status(),
            400,
            "参数校验（400）必须排在开关门之前"
        );
        // ④ 门位：设备身份在前——**直调 handler**（无 cookie 的请求在路由内层 gate 就被拦成
        //    空体 403，走不到 handler 的防御分支，故路由面测不出这段顺序）。开关关闭 + 无
        //    cookie ⇒ handler 必须先回防御 403 `forbidden`，**不是** `headless_disabled`。
        let r = crate::remote::api::session_headless_approve(
            axum::extract::State(state.clone()),
            axum::http::HeaderMap::new(),
            axum::Json(crate::remote::api::HeadlessApproveReq {
                session_id: sid.to_string(),
                request_id: "req-sw-1".to_string(),
                decision: "allow".to_string(),
                answers: None,
            }),
        )
        .await;
        assert_eq!(r.status(), 403);
        let b = body_string(r).await;
        assert!(
            b.contains("forbidden") && !b.contains("headless_disabled"),
            "设备身份（防御 403）必须排在开关门之前：{b}"
        );
        // ⑤ 重新开启 → 同一张卡照常可答（正常路径原样：送达 + 审计行）
        set_switch("true");
        let r = post(allow_body).await;
        assert_eq!(r.status(), 200, "开关重新开启后审批端点回到既有契约");
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["delivered"], true, "{v}");
        assert_eq!(
            rx.try_recv().unwrap(),
            c3::Decision::Allow,
            "决策必须交给回合（冻结期内未丢失）"
        );
        assert!(
            !c3::pending_registry().has(sid),
            "送达即注销待答项（既有语义不变）"
        );
        let audits = state
            .store
            .with(|c| crate::database::dao::write_audit::recent_conn(c, 10));
        assert_eq!(audits.len(), 1, "送达才落账：{audits:?}");
        assert_eq!(audits[0].action, "headless_approve");
        assert_eq!(audits[0].session_id, sid);
    }

    // ==== Task 13 复审 Important C：断连后的收尾所有权（审计 + 串行锁注销在**任务内**）====

    /// 假 CLI 形态（**无害系统程序**）：`node -e "setTimeout(…)" -- <多余 argv>` 睡 1.2s 后退出。
    /// 只用于「客户端断连」取证——**绝不 spawn 真 claude/kimi/opencode**（真实配额 +
    /// 真实会话库）。`--` 是必需的：node 会把 `--session` 这类多余 argv 当自己的选项而
    /// 立即 `bad option` 退出（实测），`--` 之后才落进 `process.argv`（无害）。
    fn fake_slow_cli_shape() -> crate::inject::headless::cli_three::SpawnShape {
        crate::inject::headless::cli_three::SpawnShape {
            program: "node".to_string(),
            prefix: vec![
                "-e".to_string(),
                "setTimeout(()=>{},1200)".to_string(),
                "--".to_string(),
            ],
        }
    }

    /// 钩子收尾守卫（panic 也清——钩子是进程级全局态，别把假 CLI 留给后续用例）
    struct CliHookClear;
    impl Drop for CliHookClear {
        fn drop(&mut self) {
            crate::inject::headless::cli_three::test_hooks::set_shape(None);
        }
    }

    /// **断连取证**（kimi 臂）：handler future 被 drop（客户端断开）后，**串行锁注销与
    /// `headless` 审计行仍必须发生**——因为它们写在 detached 任务里（Task 13 复审
    /// Important C）。旧实现把两者写在 handler 尾部：断连即漏 → 该会话此后每一次无头发送
    /// 都被判「已有在飞的无头回合」，直到 MAM 重启。
    #[tokio::test]
    async fn cli_oneshot_arm_releases_the_slot_in_the_detached_task_after_disconnect() {
        use crate::inject::headless::runner;
        // 钩子是进程级全局态：与其它用钩子的用例串行
        let _serial = crate::inject::headless::cli_three::test_hooks::LOCK
            .lock()
            .await;
        let _clear = CliHookClear;
        crate::inject::headless::cli_three::test_hooks::set_shape(Some(fake_slow_cli_shape()));
        let sid = "sess_c3_disconnect_kimi";
        let mut s = inj_sess(
            sid,
            crate::session::AgentType::Kimi,
            0,
            crate::session::SessionStatus::Idle,
        );
        s.project_path = std::env::temp_dir().to_string_lossy().to_string();
        let state = cli_dispatch_state(vec![s]);
        let baseline = runner::global_sem().in_flight(); // 其它用例可能占着名额：只比较相对量
        let app = router(state.clone());
        let fut = app.oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some("mam_device=mm"),
            Some(&format!(r#"{{"sessionId":"{sid}","text":"hi"}}"#)),
        ));
        let handle = tokio::spawn(fut);
        // handler 占锁是**同步**的（在 spawn 之前），故很快可见
        for _ in 0..200 {
            if crate::inject::headless::turn::registry().in_flight(sid) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(
            crate::inject::headless::turn::registry().in_flight(sid),
            "回合必须已占住串行锁"
        );
        // 让 detached 任务真的起跑（假 CLI 在睡），随后**模拟客户端断连**：abort handler
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        handle.abort();
        let _ = handle.await;
        // 槽位此刻仍在飞（回合没跑完）——必须由任务在收尾时注销
        assert!(
            crate::inject::headless::turn::registry().in_flight(sid),
            "断连不该取消已在跑的回合（回合应继续，直到自己收尾）"
        );
        for _ in 0..500 {
            if !crate::inject::headless::turn::registry().in_flight(sid) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(
            !crate::inject::headless::turn::registry().in_flight(sid),
            "断连后槽位必须被**任务自身**注销（否则该会话被永久挡死）"
        );
        // 并发名额回到基线（既不泄漏也不提前释放）
        for _ in 0..200 {
            if runner::global_sem().in_flight() <= baseline {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(
            runner::global_sem().in_flight() <= baseline,
            "并发名额必须随任务结束归还（不自建 GlobalSem、不泄漏）"
        );
        // 审计行在任务内落账（断连不丢）
        let audits = state
            .store
            .with(|c| crate::database::dao::write_audit::recent_conn(c, 10));
        assert_eq!(
            audits.len(),
            1,
            "断连后审计行仍必须落账（写在任务里，不在 handler 里）: {audits:?}"
        );
        assert_eq!(audits[0].action, "headless");
        assert_eq!(audits[0].channel, "headless_kimi_p");
        assert_eq!(audits[0].session_id, sid);
        assert!(audits[0].result.starts_with("failed("));
    }

    /// **断连取证**（claude 臂）：同上，且额外钉住**全局并发名额在任务作用域内持有**——
    /// 断连后名额**不得**提前归还（旧实现把它放在 handler 里，abort 即提前释放 = 反方向的错）。
    #[tokio::test]
    async fn claude_arm_keeps_the_permit_and_release_inside_the_detached_task() {
        use crate::inject::headless::runner;
        let _serial = crate::inject::headless::cli_three::test_hooks::LOCK
            .lock()
            .await;
        let _clear = CliHookClear;
        crate::inject::headless::cli_three::test_hooks::set_shape(Some(fake_slow_cli_shape()));
        let sid = "sess_c3_disconnect_claude";
        let mut s = inj_sess(
            sid,
            crate::session::AgentType::Claude,
            0,
            crate::session::SessionStatus::Idle,
        );
        s.project_path = std::env::temp_dir().to_string_lossy().to_string();
        let state = cli_dispatch_state(vec![s]);
        let baseline = runner::global_sem().in_flight();
        let app = router(state.clone());
        let fut = app.oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some("mam_device=mm"),
            Some(&format!(r#"{{"sessionId":"{sid}","text":"hi"}}"#)),
        ));
        let handle = tokio::spawn(fut);
        for _ in 0..200 {
            if crate::inject::headless::turn::registry().in_flight(sid) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(crate::inject::headless::turn::registry().in_flight(sid));
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        handle.abort();
        let _ = handle.await;
        // **名额仍在占用**（回合未跑完）：断连不得提前释放
        assert!(
            runner::global_sem().in_flight() > baseline,
            "断连后 claude 回合的并发名额必须仍被持有（名额在任务作用域内）"
        );
        assert!(
            crate::inject::headless::turn::registry().in_flight(sid),
            "断连不该取消已在跑的 claude 回合"
        );
        for _ in 0..500 {
            if !crate::inject::headless::turn::registry().in_flight(sid) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(
            !crate::inject::headless::turn::registry().in_flight(sid),
            "断连后 claude 回合的槽位必须被任务自身注销"
        );
        for _ in 0..200 {
            if runner::global_sem().in_flight() <= baseline {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(
            runner::global_sem().in_flight() <= baseline,
            "任务收尾后名额必须归还"
        );
        let audits = state
            .store
            .with(|c| crate::database::dao::write_audit::recent_conn(c, 10));
        assert_eq!(audits.len(), 1, "{audits:?}");
        assert_eq!(audits[0].action, "headless");
        assert_eq!(audits[0].channel, "headless_claude_p");
        assert_eq!(audits[0].session_id, sid);
    }

    // ==== Task 11（H9）：WorkBuddy ACP 分派链（HTTP 型；wire 经 MockHttp 注入，零真网络） ====

    /// WB 分派用 state：开关开启 + 设备 + 指定会话（id 独占，守卫 id 立规②）
    fn wb_dispatch_state(sessions: Vec<crate::session::Session>) -> Arc<RemoteState> {
        let state = with_sessions(inject_state(FakeInjector::ok()), sessions);
        state.store.with(|c| {
            crate::database::dao::settings::set_setting_conn(c, "remote.headless_enabled", "true")
        });
        persist_named_device(&state, "mm", "测试设备");
        state
    }

    /// WB 会话夹具（H9：恒 APP 形态 + 会话宿主 pid）+ ACP wire 常量
    const WB_ACP_SID: &str = "3f12ca20-eae5-4713-a7a4-adf64a44f346";
    /// 串行锁用例独占会话 id（守卫 id 立规②：裸 id 字符串不得跨用例共享）
    const WB_ACP_SID_BUSY: &str = "3f12ca20-bbbb-4713-a7a4-adf64a44fbbb";
    /// 端点未启用用例独占会话 id（同上：**不得与全链用例共享 id**——并行跑时
    /// 两条用例会互抢同一会话的串行锁，后到者收到「已有在飞回合」假红）
    const WB_ACP_SID_OFFLINE: &str = "3f12ca20-cccc-4713-a7a4-adf64a44fccc";
    /// 内层 JoinError（R10）用例独占会话 id（同上：id 立规②）
    const WB_ACP_SID_PANIC: &str = "3f12ca20-dddd-4713-a7a4-adf64a44fddd";
    fn wb_sess(id: &str, pid: u32) -> crate::session::Session {
        let mut s = inj_sess(
            id,
            crate::session::AgentType::WorkBuddy,
            pid,
            crate::session::SessionStatus::Idle,
        );
        s.form = crate::session::ProcessForm::App;
        s.project_path = "E:/t2".to_string();
        s
    }

    /// 钩子收尾守卫（panic 也清——钩子是进程级全局态，别把假端点留给后续用例）
    struct WbHookClear;
    impl Drop for WbHookClear {
        fn drop(&mut self) {
            crate::inject::headless::wb_acp::test_hooks::clear();
        }
    }

    /// **H9 端点未启用路（本机真实形态 = 主可验证交付）**：开关开启 + WB 会话 →
    /// **真分派**（HTTP 200 + `headless` 封套 + channel=`headless_wb_acp`），回执**如实拒绝**
    /// （`refused`：投递前、零字节投递、终态=未起跑）并带出**启用条件**；落 `headless` 审计行、
    /// 注销串行锁、零终端注入、不入队。`session-send-info` 同时报真实状态（已接线 ⇒ 输入区可用；
    /// 运行时不可用由回执如实上报，不在静态能力面谎报）
    #[tokio::test]
    async fn wb_acp_send_refuses_honestly_when_endpoint_not_enabled() {
        // 钩子是进程级全局态：与装填钩子的用例串行，并显式清空 ⇒ 本用例恒「无端点」
        let _serial = crate::inject::headless::wb_acp::test_hooks::LOCK
            .lock()
            .await;
        crate::inject::headless::wb_acp::test_hooks::clear();
        let _clear = WbHookClear;
        let fake = FakeInjector::ok();
        let state = wb_dispatch_state(vec![wb_sess(WB_ACP_SID_OFFLINE, 77765)]);
        let app = router(state.clone());
        let info = app
            .clone()
            .oneshot(req(
                "GET",
                &format!("/m/api/v1/session-send-info?session_id={WB_ACP_SID_OFFLINE}"),
                Some("mam_device=mm"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(info.status(), 200);
        let v: serde_json::Value = serde_json::from_str(&body_string(info).await).unwrap();
        assert_eq!(v["injectable"], true, "Task 11 起 WB 已真分派：{v}");
        assert_eq!(v["channels"], serde_json::json!(["headless_wb_acp"]), "{v}");
        assert_eq!(v["visibility"], "realtime", "ACP 写入 APP 内实时可见：{v}");
        assert!(
            !v.to_string().contains("headless_pending"),
            "不得再落过渡拒绝码：{v}"
        );
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-send",
                Some("mam_device=mm"),
                Some(&format!(
                    r#"{{"sessionId":"{WB_ACP_SID_OFFLINE}","text":"hi [mobile]"}}"#
                )),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "无头分派：HTTP 200 + 语义在 body");
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["status"], "headless", "{v}");
        assert_eq!(v["channel"], "headless_wb_acp");
        assert_eq!(v["receipt"]["status"], "failed");
        assert_eq!(
            v["receipt"]["stage"], "refused",
            "端点未启用 = 投递前拒绝（未起跑、零字节投递）：{v}"
        );
        assert_eq!(v["receipt"]["sessionId"], WB_ACP_SID_OFFLINE);
        let reason = v["receipt"]["reason"].as_str().unwrap_or_default();
        assert!(reason.contains("WorkBuddy 远程控制端点未启用"), "{v}");
        assert!(reason.contains("请在 WorkBuddy 设置中开启远程控制"), "{v}");
        assert!(reason.contains("风险 16"), "必须点明风险 16 跟进项：{v}");
        assert!(
            !v.to_string().contains("headless_pending"),
            "Task 11 起 WB 不得再落过渡拒绝码：{v}"
        );
        assert_eq!(fake.recorded().len(), 0, "无头路由会话零终端注入");
        let audits = state
            .store
            .with(|c| crate::database::dao::write_audit::recent_conn(c, 10));
        assert_eq!(audits.len(), 1, "无头动作必须落既有 W5 单账本：{audits:?}");
        assert_eq!(audits[0].action, "headless");
        assert_eq!(audits[0].channel, "headless_wb_acp", "通道列 = WB ACP");
        assert_eq!(audits[0].result, "failed(refused) · 0ms");
        assert_eq!(audits[0].session_id, WB_ACP_SID_OFFLINE);
        assert!(
            !crate::inject::headless::turn::registry().in_flight(WB_ACP_SID_OFFLINE),
            "拒绝臂必须注销串行锁（否则该会话被自己的锁永久挡死）"
        );
        let pending = state.store.with(|c| {
            crate::database::dao::inject_queue::pending_for_session_conn(c, WB_ACP_SID_OFFLINE)
        });
        assert!(pending.is_empty(), "无头回合绝不入队（裁决 8）");
    }

    /// **H9 全链（wire 由 MockHttp 注入，零真网络零真 ~/.workbuddy）**：心跳端点在场 →
    /// connect → initialize → `session/load` → `session/prompt`（agentPhase + stopReason）
    /// → 回执 `ok` + assistant 摘要；审计 ok、串行锁注销、零终端注入、不入队。
    /// `home_source = None` ⇒ 转写佐证不可确认——回执 `reason` **如实标注**（不谎称已确认）
    #[tokio::test]
    async fn wb_acp_send_dispatches_full_wire_and_audits_ok() {
        use crate::inject::headless::wb_acp::{
            test_hooks, MockHttp, ACCEPT_BOTH, ACP_PATH, CONNECT_PATH,
        };
        let _serial = test_hooks::LOCK.lock().await;
        let _clear = WbHookClear;
        let accept = format!("Accept: {ACCEPT_BOTH}");
        let hdrs: Vec<&str> = vec!["acp-connection-id", "acp-session-token", &accept];
        let update = |v: serde_json::Value| format!("event: message\ndata: {v}\n\n");
        let mut http = MockHttp::new();
        http.expect_post(CONNECT_PATH, None)
            .respond_json(r#"{"connectionId":"c1","sessionToken":"t1"}"#);
        http.expect_post(ACP_PATH, Some(&hdrs))
            .respond_sse(&update(serde_json::json!({"jsonrpc":"2.0","id":1,
                "result":{"protocolVersion":1,"agentCapabilities":{"loadSession":true}}})));
        http.expect_post(ACP_PATH, Some(&hdrs)).respond_sse(&update(
            serde_json::json!({"jsonrpc":"2.0","id":2,"result":{}}),
        ));
        http.expect_post(ACP_PATH, Some(&hdrs)).respond_sse(&format!(
            "{}{}{}",
            update(serde_json::json!({"jsonrpc":"2.0","method":"session/update",
                "params":{"sessionId":WB_ACP_SID,
                          "update":{"sessionUpdate":"agent_phase","agentPhase":"model_requesting"}}})),
            update(serde_json::json!({"jsonrpc":"2.0","method":"session/update",
                "params":{"sessionId":WB_ACP_SID,
                          "update":{"sessionUpdate":"agent_message_chunk",
                                    "content":{"type":"text","text":"收到"}}}})),
            update(serde_json::json!({"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn"}})),
        ));
        test_hooks::set(
            Some(test_hooks::FakeHeartbeat {
                session_id: WB_ACP_SID.into(),
                endpoint: Some("http://127.0.0.1:63928".into()),
                usable: true,
            }),
            Vec::new(),
            Vec::new(),
        );
        test_hooks::set_http(http.clone().seam());
        let fake = FakeInjector::ok();
        let state = wb_dispatch_state(vec![wb_sess(WB_ACP_SID, 77765)]);
        let app = router(state.clone());
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-send",
                Some("mam_device=mm"),
                Some(&format!(
                    r#"{{"sessionId":"{WB_ACP_SID}","text":"hi [mobile]"}}"#
                )),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["status"], "headless", "{v}");
        assert_eq!(v["channel"], "headless_wb_acp");
        assert_eq!(v["receipt"]["status"], "ok", "{v}");
        assert_eq!(v["receipt"]["lastAssistant"], "收到");
        assert!(
            v["receipt"]["reason"]
                .as_str()
                .unwrap_or_default()
                .contains("转写佐证未命中"),
            "佐证不可确认时必须如实标注（ok 也不谎称已确认）：{v}"
        );
        assert_eq!(fake.recorded().len(), 0, "无头路由会话零终端注入");
        let audits = state
            .store
            .with(|c| crate::database::dao::write_audit::recent_conn(c, 10));
        assert_eq!(audits.len(), 1, "{audits:?}");
        assert_eq!(audits[0].action, "headless");
        assert_eq!(audits[0].channel, "headless_wb_acp");
        assert!(
            audits[0].result.starts_with("ok"),
            "回执 ok ⇒ 审计 result 以 ok 开头（含耗时编码）：{}",
            audits[0].result
        );
        assert!(
            !crate::inject::headless::turn::registry().in_flight(WB_ACP_SID),
            "回合终结必须注销串行锁"
        );
        let pending = state
            .store
            .with(|c| crate::database::dao::inject_queue::pending_for_session_conn(c, WB_ACP_SID));
        assert!(pending.is_empty(), "无头回合绝不入队（裁决 8）");
        http.assert_clean();
    }

    /// **R10：WB 回合的内层 `spawn_blocking` JoinError 也必须落审计行**——ACP 链内 panic 时
    /// 旧的 JoinError 臂只写 log 就返回回执：回合「起跑了但从未入账」，审计页上这条投递凭空
    /// 消失（ticket 的「回合起跑无审计行」）。本用例用**必 panic 的 HTTP 缝**把
    /// `wb_acp::run_turn` 打炸（真回合里的等价值 = ACP 链内部 panic）⇒ 断言回执如实
    /// `failed(channel_error)` **且**落一行 WB 通道的 `headless` 账（与兄弟路径同源：
    /// 同 action、同 channel 列、同 agent_type、同 `audit_result` 口径）。
    ///
    /// 还原动作（变异）：删掉内层臂的 `ctx.audit(...)` → 本用例先红（审计 0 行）。
    #[tokio::test]
    async fn wb_acp_inner_join_error_still_leaves_an_audit_row() {
        use crate::inject::headless::wb_acp::test_hooks;
        let _serial = test_hooks::LOCK.lock().await;
        let _clear = WbHookClear;
        test_hooks::set(
            Some(test_hooks::FakeHeartbeat {
                session_id: WB_ACP_SID_PANIC.into(),
                endpoint: Some("http://127.0.0.1:63928".into()),
                usable: true,
            }),
            Vec::new(),
            Vec::new(),
        );
        // ACP 链内 panic（等价形态：回合内部任务异常终止）
        test_hooks::set_http(std::sync::Arc::new(|_req| {
            panic!("模拟 ACP 链内 panic（R10：回合起跑无审计行）")
        }));
        let fake = FakeInjector::ok();
        let state = wb_dispatch_state(vec![wb_sess(WB_ACP_SID_PANIC, 77767)]);
        let app = router(state.clone());
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-send",
                Some("mam_device=mm"),
                Some(&format!(
                    r#"{{"sessionId":"{WB_ACP_SID_PANIC}","text":"hi [mobile]"}}"#
                )),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "无头分派：HTTP 200 + 语义在 body");
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["status"], "headless", "{v}");
        assert_eq!(v["channel"], "headless_wb_acp");
        assert_eq!(v["receipt"]["status"], "failed", "{v}");
        assert_eq!(
            v["receipt"]["stage"], "channel_error",
            "回合内部异常 = 通道异常（不是 refused——正文已经起跑）: {v}"
        );
        assert!(
            v["receipt"]["reason"]
                .as_str()
                .is_some_and(|s| s.contains("回合内部任务异常终止")),
            "reason 必须如实说明结果未知、请到应用内确认: {v}"
        );
        assert_eq!(fake.recorded().len(), 0, "无头路由会话零终端注入");
        let audits = state
            .store
            .with(|c| crate::database::dao::write_audit::recent_conn(c, 10));
        assert_eq!(
            audits.len(),
            1,
            "内层 JoinError 臂必须落账（否则这条回合在账上凭空消失）: {audits:?}"
        );
        assert_eq!(audits[0].action, "headless");
        assert_eq!(audits[0].channel, "headless_wb_acp", "通道列 = WB ACP");
        assert_eq!(
            audits[0].agent_type, "workbuddy",
            "工具列 = WB（与兄弟路径同源：槽位/ctx 的 tool_id）"
        );
        assert_eq!(audits[0].session_id, WB_ACP_SID_PANIC);
        assert!(
            audits[0].result.starts_with("failed(channel_error) · "),
            "result 列 = 终态 + 耗时（audit_result 单点口径）: {}",
            audits[0].result
        );
        assert!(
            !crate::inject::headless::turn::registry().in_flight(WB_ACP_SID_PANIC),
            "异常臂也必须注销串行锁（否则该会话被自己的锁永久挡死）"
        );
    }

    /// H9 会话串行锁：同会话已有在飞无头回合 → **如实的投递前拒绝**（不排队、不覆盖、
    /// 零 HTTP）——与 zcode/codex 同一份进程级登记表
    #[tokio::test]
    async fn wb_acp_send_refuses_when_session_turn_in_flight() {
        let _serial = crate::inject::headless::wb_acp::test_hooks::LOCK
            .lock()
            .await;
        let _clear = WbHookClear;
        assert!(crate::inject::headless::turn::registry().begin(
            WB_ACP_SID_BUSY,
            crate::inject::headless::turn::TurnSlot::placeholder("workbuddy", "在飞回合".into()),
        ));
        let fake = FakeInjector::ok();
        let state = wb_dispatch_state(vec![wb_sess(WB_ACP_SID_BUSY, 77766)]);
        let app = router(state.clone());
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-send",
                Some("mam_device=mm"),
                Some(&format!(
                    r#"{{"sessionId":"{WB_ACP_SID_BUSY}","text":"hi"}}"#
                )),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["receipt"]["stage"], "refused", "{v}");
        assert!(
            v["receipt"]["reason"]
                .as_str()
                .is_some_and(|s| s.contains("串行锁")),
            "必须说清是串行锁拒绝：{v}"
        );
        assert_eq!(fake.recorded().len(), 0);
        // 他人在飞槽位不得被本臂注销（占位前的拒绝若误注销，取消靶子会消失）
        assert!(
            crate::inject::headless::turn::registry().in_flight(WB_ACP_SID_BUSY),
            "拒绝臂不得注销**别人的**在飞槽位"
        );
        crate::inject::headless::turn::registry().end(WB_ACP_SID_BUSY);
    }

    /// **改道链的端点级证据**（复审 Minor 5）：真回合 → 单写者锁 → 自动改道 queue ⇒
    /// 回执封套 `channel` 与**审计 channel 列**都必须是 `headless_codex_queue`（**不是**
    /// 原计划 `headless_codex_exec`），且改道说明落在回执 reason 里。
    ///
    /// **测试构建怎样走到这一步**：`production_presence` 在 `cfg(test)` 恒判 APP 不在场
    /// （走 exec 兜底），`production_exe`/`probe_cli`/执行缝在 `cfg(test)` 由
    /// [`crate::inject::headless::codex::test_hooks`] 提供三个可控面——脚本注入两段结局
    /// （exec 被锁拒 + queue 入队成功），**绝不真 spawn 真 codex**（真实账号配额纪律）。
    /// 钩子是进程级全局态 ⇒ 持 [`crate::inject::headless::codex::test_hooks::LOCK`] 串行。
    #[tokio::test]
    async fn codex_divert_to_queue_is_reflected_in_envelope_and_audit() {
        use crate::inject::headless::codex::test_hooks;
        let _serial = test_hooks::LOCK.lock().await;
        let fake = FakeInjector::ok();
        let state = codex_dispatch_state(vec![codex_app_sess(CODEX_THREAD_UUID_2, 79)]);
        let app = router(state.clone());
        test_hooks::set(
            Some("C:/fake/codex.cmd"),
            true,
            vec![
                // ① exec resume 被单写者锁拒（**非 0 退出**——改道判据要求先是失败的尝试）
                codex_obs(1, &[], "Error: -32600 already has an active writer"),
                // ② 改道 queue：入队回执（exit 0 + `Queued message <id>`）
                codex_obs(0, &["Queued message m-divert"], ""),
            ],
        );
        let _clear = CodexHookClear; // panic 也清钩子
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-send",
                Some("mam_device=mm"),
                Some(&format!(
                    r#"{{"sessionId":"{CODEX_THREAD_UUID_2}","text":"hi"}}"#
                )),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["status"], "headless", "{v}");
        assert_eq!(
            v["channel"], "headless_codex_queue",
            "改道后封套通道必须是**实际走向**（不是原计划 exec）：{v}"
        );
        assert_eq!(
            v["receipt"]["status"], "queued",
            "入队未消费 = 排队态（测试构建 rollout 不可定位 ⇒ 消费确认不可用）：{v}"
        );
        let reason = v["receipt"]["reason"].as_str().unwrap_or_default();
        assert!(
            reason.contains("单写者锁") && reason.contains("改道"),
            "改道说明必须在回执里：{reason}"
        );
        assert!(
            reason.contains("already has an active writer"),
            "原错误证据必须在场：{reason}"
        );
        assert!(
            reason.contains("消费确认不可用"),
            "消费确认给不出时必须如实声明：{reason}"
        );
        let audits = state
            .store
            .with(|c| crate::database::dao::write_audit::recent_conn(c, 10));
        assert_eq!(audits.len(), 1, "无头回合落一行账：{audits:?}");
        assert_eq!(
            audits[0].channel, "headless_codex_queue",
            "审计通道列 = 实际走向（改道不得记成原计划）"
        );
        assert!(
            audits[0].result.starts_with("queued"),
            "终态 = 已入队未消费（queued · Nms）：{}",
            audits[0].result
        );
        assert_eq!(audits[0].session_id, CODEX_THREAD_UUID_2);
        assert_eq!(fake.recorded().len(), 0, "无头回合零终端注入");
        assert!(
            !crate::inject::headless::codex::registry().in_flight(CODEX_THREAD_UUID_2),
            "回合终结必须注销串行锁"
        );
        let pending = state.store.with(|c| {
            crate::database::dao::inject_queue::pending_for_session_conn(c, CODEX_THREAD_UUID_2)
        });
        assert!(pending.is_empty(), "无头回合绝不入 MAM 队列");
    }

    // ==== 批次乙 T8：session-question / session-question/answer 端点 ====
    // 零污染：标记（question/approval）/审计/KV 全走 RemoteState.store = memory()；
    // 会话快照与注入器走注入缝；通道 B 消息走 message_source 注入缝。不触真实 ~/.tuvis。
    // **守卫 id 立规（approve_state 同款）**：每个 POST 用例独占会话 id（sess_u..
    // sess_aj 为全测试集未占用段——approve/inject/open 三族夹具已占 sess_a..sess_q /
    // sess_i / sess_t3 / sess_att）。

    /// 探测档案 §3 单选真实夹具（缩录；questions JSON 原样）
    const Q_SINGLE_PAYLOAD: &str = r#"{"questions":[{"header":"Next step","multiSelect":false,"options":[{"description":"Explain how AskUserQuestion works.","label":"Tool demo"},{"description":"Start a coding or file task in this directory.","label":"Start a task"},{"description":"You have no further request for now.","label":"Nothing yet"}],"question":"This is a demo question — what would you like to do next?"}]}"#;

    /// 探测档案 §3 多选真实夹具（缩录）
    const Q_MULTI_PAYLOAD: &str = r#"{"questions":[{"header":"Favorite fruits","multiSelect":true,"options":[{"description":"A sweet, crisp fruit.","label":"Apple"},{"description":"A soft, tropical fruit.","label":"Banana"},{"description":"A juicy summer fruit.","label":"Peach"}],"question":"Which fruits are your favorites? (Select all that apply)"}]}"#;

    /// 多问题数组夹具（questions.length=2——只读形态，注入面由端点拒绝）
    const Q_TWO_QUESTIONS_PAYLOAD: &str = r#"{"questions":[{"header":"A","question":"First?","options":[{"label":"a1"},{"label":"a2"}]},{"header":"B","question":"Second?","options":[{"label":"b1"},{"label":"b2"}]}]}"#;

    /// 多题·首题多选夹具（2026-09-24 claude 多题接入用例：多题交互 + 首题
    /// multiSelect——与 `screen_fixtures::multi_option_focus*`（3 选项）同屏形态）
    const Q_TWO_Q_MULTI_FIRST_PAYLOAD: &str = r#"{"questions":[{"header":"Favorite fruits","multiSelect":true,"question":"Which fruits are your favorites?","options":[{"label":"Apple"},{"label":"Banana"},{"label":"Peach"}]},{"header":"Next step","question":"What next?","options":[{"label":"Go"},{"label":"Stop"}]}]}"#;

    /// 丁T2：kimi 映射表条目在默认表里的取证版本号（`kimi --version` 本机实测
    /// 2.0.2；该版本正是 R1-1 探测与 wire 形态取证的版本）——断言用，防默认表被误改
    const KIMI_NEVER_TESTED: &str = "2.0.2";

    /// 丁T2：kimi 问答形态载荷（本机 wire 实测缩录：`interaction.request(kind=question)`
    /// 的 request.questions 与 AUQ tool.call args 同形——后者才是消息流里的载体）
    const KIMI_Q_PAYLOAD: &str = r#"{"questions":[{"question":"Which output folder should the build use?","header":"Output dir","options":[{"label":"dist","description":"d"},{"label":"out","description":"o"}],"multiSelect":false}]}"#;

    /// 丁T2：kimi **多问题**载荷（本机 wire 实录 3 题缩录为 2 题——多题只读锁的夹具；
    /// kimi 的 answers map 按**题干文本**键控，故题干是稳定回读点）
    const KIMI_TWO_Q_PAYLOAD: &str = r#"{"questions":[{"question":"是否确认执行？","header":"清空确认","options":[{"label":"确认","description":"删除后重下"},{"label":"不清空","description":"直接下载"}]},{"question":"日期边界如何理解？","header":"日期边界","options":[{"label":"≥2026-07-01","description":"含7月"},{"label":"仅8月","description":"不含7月"}],"multiSelect":false}]}"#;

    /// 通道 B 的 AUQ tool-call 消息条目（content.rs SessionMessage 直构）
    fn auq_tool_call(seq: i64, args: &str) -> crate::remote::content::SessionMessage {
        crate::remote::content::SessionMessage {
            seq,
            role: "assistant".into(),
            kind: "tool-call".into(),
            content: "AskUserQuestion".into(),
            ts: None,
            tool_name: Some("AskUserQuestion".into()),
            tool_args: Some(args.into()),
            collapsed: true,
        }
    }

    /// 通道 B 的 tool-result 消息条目（答完判据的反例形态）
    fn tool_result_msg(seq: i64, content: &str) -> crate::remote::content::SessionMessage {
        crate::remote::content::SessionMessage {
            seq,
            role: "assistant".into(),
            kind: "tool-result".into(),
            content: content.into(),
            ts: None,
            tool_name: None,
            tool_args: None,
            collapsed: true,
        }
    }

    fn user_msg(seq: i64) -> crate::remote::content::SessionMessage {
        crate::remote::content::SessionMessage {
            seq,
            role: "user".into(),
            kind: "user".into(),
            content: "继续".into(),
            ts: None,
            tool_name: None,
            tool_args: None,
            collapsed: false,
        }
    }

    /// T8 专用 state：会话夹具 + 问题/审批标记由各测试经 store.with 播种（内存库）。
    /// 全部 claude；id 语义见各测试。通道 B 缺省为 Err 桩（不触消息源）。
    fn question_state(
        injector: std::sync::Arc<dyn crate::inject::engine::Injector>,
    ) -> Arc<RemoteState> {
        question_state_with_msgs(
            injector,
            Box::new(|_, _, _| Err("测试桩：未注入内容源".to_string())),
        )
    }

    /// question_state 变体：message_source 可注入（通道 B 用例）。
    /// 会话清单：sess_u（Waiting，标记夹具 GET）/ sess_v-w-x-y（POST select/toggle/
    /// submit/cancel 各自独占）/ sess_z（busy 独占）/ sess_aa（多问题 409）/
    /// sess_ab（标记载荷损坏 → 回落通道 B）/ sess_ac（bad_index 400）/
    /// sess_ad（仅审批标记——隔离反差用）/ sess_ae（问题标记 + detect 命中文案——
    /// 问题标记压审批卡的最强隔离形态）/ sess_af（无标记——no_question 409）/
    /// sess_qadv / sess_qsub（阶段机 advance / 已在 Review submit **各自独占**——
    /// 守卫 id 立规：投递类用例不得共用裸 id）/
    /// sess_ai / sess_aj（Processing 无标记——通道 B 可用/已答反例）/
    /// sess_ak（**阶段机中途投递失败**：首错即停 + `failed:<e>` 审计独占——原为
    /// 批次乙「submit 首错即停」用例的夹具，本批重构时该用例被误删（复评 Important-2），
    /// 丁T5 复评已补回同语义覆盖并继续用本夹具）
    fn question_state_with_msgs(
        injector: std::sync::Arc<dyn crate::inject::engine::Injector>,
        message_source: Box<crate::remote::content::MessageSourceFn>,
    ) -> Arc<RemoteState> {
        question_state_full(injector, message_source, std::sync::Arc::new(|_, _| None))
    }

    /// question_state 第三变体（丁T5）：`screen_probe` 缝可注入（阶段机用例的脚本化
    /// 屏序列）。其余与会话清单同 [`question_state_with_msgs`]。
    fn question_state_full(
        injector: std::sync::Arc<dyn crate::inject::engine::Injector>,
        message_source: Box<crate::remote::content::MessageSourceFn>,
        screen_probe: std::sync::Arc<crate::remote::server::ScreenProbeFn>,
    ) -> Arc<RemoteState> {
        let sess = |id: &str, pid: u32, status: crate::session::SessionStatus| {
            inj_sess(id, crate::session::AgentType::Claude, pid, status)
        };
        let sessions = vec![
            sess("sess_u", 31, crate::session::SessionStatus::Waiting),
            sess("sess_v", 32, crate::session::SessionStatus::Waiting),
            sess("sess_w", 33, crate::session::SessionStatus::Waiting),
            sess("sess_x", 34, crate::session::SessionStatus::Waiting),
            sess("sess_y", 35, crate::session::SessionStatus::Waiting),
            sess("sess_z", 36, crate::session::SessionStatus::Waiting),
            sess("sess_aa", 37, crate::session::SessionStatus::Waiting),
            sess("sess_ab", 38, crate::session::SessionStatus::Waiting),
            {
                // opencode toggle 翻转校验独占会话（守卫 id 立规，2026-10-05）
                inj_sess(
                    "sess_ocflip",
                    crate::session::AgentType::OpenCode,
                    48,
                    crate::session::SessionStatus::Waiting,
                )
            },
            {
                // opencode 切题到达验证独占会话 ×2（守卫 id 立规，2026-10-05）
                inj_sess(
                    "sess_ocadv",
                    crate::session::AgentType::OpenCode,
                    49,
                    crate::session::SessionStatus::Waiting,
                )
            },
            {
                inj_sess(
                    "sess_ocadv2",
                    crate::session::AgentType::OpenCode,
                    50,
                    crate::session::SessionStatus::Waiting,
                )
            },
            sess("sess_ac", 39, crate::session::SessionStatus::Waiting),
            sess("sess_ad", 40, crate::session::SessionStatus::Waiting),
            // 守卫 id 立规补正（2026-10-07 存量红清理）：**问题投递**类用例各自独占 id。
            // 原「stage 机 advance」与「已在 Review submit」两条用例都借 sess_ad，而
            // INFLIGHT 守卫按**裸 id 字符串全局占用**（见 inject_state_with_probe 文档）
            // ⇒ 二者并行跑必有一条吃「投递进行中，请稍后重试」（实测并行 3/3 红、
            // 串行 3/3 绿）；sess_ad 归还给它的既定主人「仅审批标记——隔离反差用」。
            sess("sess_qadv", 26, crate::session::SessionStatus::Waiting),
            sess("sess_qsub", 27, crate::session::SessionStatus::Waiting),
            {
                // 最强隔离形态：问题标记 + last_message 恰为审批 marker 命中句——
                // 证明问题标记压审批卡不依赖 detect 未达
                let mut s = sess("sess_ae", 41, crate::session::SessionStatus::Waiting);
                s.last_message = Some(APPROVE_HIT_MSG.to_string());
                s
            },
            sess("sess_af", 42, crate::session::SessionStatus::Waiting),
            sess("sess_ai", 44, crate::session::SessionStatus::Processing),
            sess("sess_aj", 45, crate::session::SessionStatus::Processing),
            // 复评 Minor 1：submit 首错即停 + failed 审计独占会话（守卫 id 立规）
            sess("sess_ak", 46, crate::session::SessionStatus::Waiting),
            // 丁T1 回归锁独占会话（全测试集唯一 id，守卫 id 立规）：问题待决的**语义红**
            // ——状态链（codex request_user_input 配对 / opencode question 部件）推出
            // Waiting，但**没有**任何等待标记（这是本批新引入的形态）。last_message 为
            // 真实问答文本（2026-09-21 rollout 实录形态），**不含**任何审批 marker——
            // 锁「审批卡不得借红灯误出」
            {
                let mut s = inj_sess(
                    "sess_al",
                    crate::session::AgentType::Codex,
                    47,
                    crate::session::SessionStatus::Waiting,
                );
                s.last_message = Some("构建产物放在哪个目录？".to_string());
                s
            },
            {
                let mut s = inj_sess(
                    "sess_am",
                    crate::session::AgentType::OpenCode,
                    48,
                    crate::session::SessionStatus::Waiting,
                );
                s.last_message = Some("Which folder should hold build output?".to_string());
                s
            },
            // 2026-10-05 推广批 T8：kimi 能力位契约独占会话（multiFreeText=false——
            // 多选形态未定案，探测批 K）
            {
                let mut s = inj_sess(
                    "sess_ao",
                    crate::session::AgentType::Kimi,
                    49,
                    crate::session::SessionStatus::Waiting,
                );
                s.last_message = Some("Which fruits do you like?".to_string());
                s
            },
            // 2026-10-05 推广批 T9：sess_ad 守卫竞争根治——advance/submit 两测试
            // 迁到全测试集未占用的 sess_av/sess_aw（守卫 id 立规；pid 断言同步 53/54）
            sess("sess_av", 53, crate::session::SessionStatus::Waiting),
            sess("sess_aw", 54, crate::session::SessionStatus::Waiting),
            // 2026-10-05 推广批 T9：sess_ad 守卫竞争根治——advance/submit/审批标记
            // 三测试各得独占会话（守卫 id 立规；并行 POST 互抢 INFLIGHT 致偶发
            // 「投递进行中」假红，连续多轮全量命中后按立规根治；pid 顺延 50-52）
            // 丁T1 复评 F-2 独占会话（全测试集唯一 id）：**最强对照形态**——问题待决
            // 的语义红 + last_message **恰为审批 marker 命中句**（模型把问题写成审批
            // 措辞的自然形态）。这正是 F-2 要拦的场景：detect 纯文本命中会误出审批卡，
            // 唯一的拦截来自「尾部存在待决问答」判定（question_pending_red_* 用例）
            {
                let mut s = inj_sess(
                    "sess_an",
                    crate::session::AgentType::Codex,
                    49,
                    crate::session::SessionStatus::Waiting,
                );
                // codex 默认映射 marker 之一（DEFAULT_MAPPINGS_JSON）
                s.last_message = Some("Would you like to run the following command?".to_string());
                s
            },
            {
                let mut s = inj_sess(
                    "sess_ao",
                    crate::session::AgentType::OpenCode,
                    50,
                    crate::session::SessionStatus::Waiting,
                );
                s.last_message = Some("Would you like to run the following command?".to_string());
                s
            },
            // 丁T1 复评 F-3 独占会话（全测试集唯一 id）：走**真实 opencode reader**
            // （tempdir 内的 opencode.db）验证销卡信号——sess_ap 待决（应 available=true）、
            // sess_aq 已答（completed，应 available=false）。**必须是 OpenCode 类型**：
            // message_source 按 tool_id 派发到 opencode reader
            inj_sess(
                "sess_ap",
                crate::session::AgentType::OpenCode,
                51,
                crate::session::SessionStatus::Waiting,
            ),
            inj_sess(
                "sess_aq",
                crate::session::AgentType::OpenCode,
                52,
                crate::session::SessionStatus::Waiting,
            ),
            // 丁T1 复评 F2-2 独占会话（全测试集唯一 id）：**claude 假阳性反锁**——
            // 尾部有「AUQ 形态 tool-call 无 result」（评审实测的 claude 真实形态：
            // 0c41365d-… 的 AUQ 被纯文本作答、tool_result 永不落盘）+ last_message
            // 命中 claude 审批 marker。claude 有钩子问答标记通道，故**不叠加**尾部
            // 判据 → 审批必须照常可用（不被静默压制）
            {
                let mut s = inj_sess(
                    "sess_ar",
                    crate::session::AgentType::Claude,
                    53,
                    crate::session::SessionStatus::Waiting,
                );
                // claude 默认映射 marker 之一（DEFAULT_MAPPINGS_JSON）
                s.last_message = Some("Do you want to proceed?".to_string());
                s
            },
            // 2026-09-24 多题闸门改写独占会话（全测试集唯一 id，守卫 id 立规）：
            // **zcode**（不在多题交互族）——`question_answer_multi_questions_refused`
            // 的拒出手载体（claude 已于本日接入多题交互，原载体身份让位）
            inj_sess(
                "sess_bq",
                crate::session::AgentType::ZCode,
                60,
                crate::session::SessionStatus::Waiting,
            ),
            // ===== L13 靶向闸用例独占会话（守卫 id 立规①/②，2026-10-05 复审补口）=====
            // 问答作答两条真投递用例各占唯一 id（与 question_answer_select_sends_digit
            // 的 sess_v 错开——INFLIGHT 按裸 id 全局占用，撞 id 即假红）
            sess("sess_l13q", 81, crate::session::SessionStatus::Waiting),
            sess("sess_l13r", 82, crate::session::SessionStatus::Waiting),
            // **sess_ad 家族收敛（复审 item 6b）**：`sess_ad` 原被两条真投递用例共用
            // （advance→next / submit），默认并行度下 `cargo test --lib
            // remote::server::tests` 必红（复现 3/3，父提交同样如此）。两条各占唯一 id；
            // `sess_ad` 留给不注入的审批标记隔离用例（question_endpoints_blocked_by_*）
            sess("sess_l13s", 83, crate::session::SessionStatus::Waiting),
            sess("sess_l13t", 84, crate::session::SessionStatus::Waiting),
            // ===== 丁T2 计划双卡族（问题 3/4）：sess_as..sess_ax =====
            // 全测试集唯一 id（守卫 id 立规）。**kimi** 四例（sess_au..sess_ax）与
            // **codex** 两例（sess_as/sess_at）：计划预期态是 codex/kimi 的计划确认
            // 类对话框专属门（见 remote/api.rs 的 plan_dialog_family）。
            //
            // sess_as：codex **Processing**（实机计划提案后 codex 不落 Waiting）+
            // 尾部计划提案 → 门放宽到预期态（问题 4 主用例）
            inj_sess(
                "sess_as",
                crate::session::AgentType::Codex,
                54,
                crate::session::SessionStatus::Processing,
            ),
            // sess_at：codex 同形但计划之后已有用户消息 → 预期态清除（反向锁）
            inj_sess(
                "sess_at",
                crate::session::AgentType::Codex,
                55,
                crate::session::SessionStatus::Processing,
            ),
            // sess_au：kimi 计划审批（Waiting——wire interaction.request 的红灯）→
            // 审批卡主用例；sess_av：kimi 计划后已有工具事件 → 预期态清除
            inj_sess(
                "sess_au",
                crate::session::AgentType::Kimi,
                56,
                crate::session::SessionStatus::Waiting,
            ),
            inj_sess(
                "sess_av",
                crate::session::AgentType::Kimi,
                57,
                crate::session::SessionStatus::Processing,
            ),
            // sess_aw：kimi 审批在场（尾部计划提案）→ 问答卡必须不可用（互斥主用例）
            // sess_ax：kimi 计划后已有工具事件（审批窗口关闭）→ 问答必须照常可用（反向锁）
            inj_sess(
                "sess_aw",
                crate::session::AgentType::Kimi,
                58,
                crate::session::SessionStatus::Waiting,
            ),
            inj_sess(
                "sess_ax",
                crate::session::AgentType::Kimi,
                59,
                crate::session::SessionStatus::Waiting,
            ),
            // 丁T2 多题只读扩面（sess_ay/sess_az）：codex 与 kimi 的多问题待决
            // （message_source 按 sid 分派，见 multi_question_readonly_covers_*）
            inj_sess(
                "sess_ay",
                crate::session::AgentType::Codex,
                60,
                crate::session::SessionStatus::Waiting,
            ),
            inj_sess(
                "sess_az",
                crate::session::AgentType::Kimi,
                61,
                crate::session::SessionStatus::Waiting,
            ),
            // 丁T2 kimi 降级态（sess_ba）：Waiting + 审批标记但**无计划预期态**
            // （带标记的 Write/command 审批）→ available=false + 提示条（无键可发）
            inj_sess(
                "sess_ba",
                crate::session::AgentType::Kimi,
                62,
                crate::session::SessionStatus::Waiting,
            ),
            // ===== 丁T2 复评修复族（F3-2/F3-3/F3-5）：sess_bb..sess_bf =====
            // 全测试集唯一 id（守卫 id 立规）。
            // sess_bb：codex **Idle**（真机计划提案后的状态）+ 尾部计划提案 → POST 门
            //   必须与 GET 同口径放行（F3-2 主用例）
            inj_sess(
                "sess_bb",
                crate::session::AgentType::Codex,
                63,
                crate::session::SessionStatus::Idle,
            ),
            // sess_bc：codex Idle + 尾部**非**计划（用户消息在前）→ POST 必须 409
            //   （反向锁：门放宽不得变成「Idle 即放行」）
            inj_sess(
                "sess_bc",
                crate::session::AgentType::Codex,
                64,
                crate::session::SessionStatus::Idle,
            ),
            // sess_bd：claude Waiting + 尾部计划（ExitPlanMode 升格产物）→ 审批卡必须
            //   带 plan 正文（F3-3 回归锁——ed1a868 起静默失效的修复）
            inj_sess(
                "sess_bd",
                crate::session::AgentType::Claude,
                65,
                crate::session::SessionStatus::Waiting,
            ),
            // sess_be：kimi Waiting + 尾部计划 → 审批卡 `plan` 必须 **null**（F3-4 收口）
            inj_sess(
                "sess_be",
                crate::session::AgentType::Kimi,
                66,
                crate::session::SessionStatus::Waiting,
            ),
            // sess_bf：kimi Waiting + 尾部**只有孤立 plan-file**（无正文卡）→ 问答卡
            //   必须**仍可用**（F3-5 反锁：孤立文件卡不得压掉问答卡）
            inj_sess(
                "sess_bf",
                crate::session::AgentType::Kimi,
                67,
                crate::session::SessionStatus::Waiting,
            ),
            // sess_bg：codex Idle + 尾部计划提案 —— **N1 用例独占**（守卫 id 立规：
            //   N1 的 POST 遍历会成功投递吗？不会——预期态一律 404，不出手；但为
            //   避免与 F3-2 用例共享 id 时 in-flight 守卫串键，仍占唯一 id）
            inj_sess(
                "sess_bg",
                crate::session::AgentType::Codex,
                68,
                crate::session::SessionStatus::Idle,
            ),
            // sess_bh：claude Waiting（标记路径）—— **N1 反向锁独占**（该用例会真投递，
            //   必须独占 id：INFLIGHT 按裸 id 字符串全局占用，跨用例撞 id 即串键）
            inj_sess(
                "sess_bh",
                crate::session::AgentType::Claude,
                69,
                crate::session::SessionStatus::Waiting,
            ),
            // ===== N3 短路序计数族（sess_bi..sess_bk）=====
            // sess_bi：codex **Waiting** + 计划族 → 门由状态满足（零读页臂）
            inj_sess(
                "sess_bi",
                crate::session::AgentType::Codex,
                70,
                crate::session::SessionStatus::Waiting,
            ),
            // sess_bj：codex **Idle** + 审批标记（由用例播种）→ 门由标记满足（零读页臂）
            inj_sess(
                "sess_bj",
                crate::session::AgentType::Codex,
                71,
                crate::session::SessionStatus::Idle,
            ),
            // sess_bk：**claude** Idle + 尾部计划 → 族收窄（零读页臂，T1 零额外 IO 口径）
            inj_sess(
                "sess_bk",
                crate::session::AgentType::Claude,
                72,
                crate::session::SessionStatus::Idle,
            ),
            // ===== 丁T5 阶段机族（sess_t5a..sess_t5h）=====
            // **守卫 id 立规**：阶段机用例会真投递，INFLIGHT 按裸 id 全局占用——
            // 每个 POST 用例必须独占一个 id（与批次丙 sess_u..sess_bk 全段互异）。
            // 全 claude Waiting（阶段机只对 claude 放行）。
            sess("sess_t5a", 84, crate::session::SessionStatus::Waiting),
            sess("sess_t5b", 85, crate::session::SessionStatus::Waiting),
            sess("sess_t5c", 86, crate::session::SessionStatus::Waiting),
            sess("sess_t5d", 87, crate::session::SessionStatus::Waiting),
            sess("sess_t5e", 88, crate::session::SessionStatus::Waiting),
            sess("sess_t5f", 89, crate::session::SessionStatus::Waiting),
            sess("sess_t5g", 90, crate::session::SessionStatus::Waiting),
            sess("sess_t5h", 91, crate::session::SessionStatus::Waiting),
            // 工具面用例（非 claude 的拒绝路径不走阶段机，但同样要独占 id）
            inj_sess(
                "sess_t5i",
                crate::session::AgentType::Codex,
                92,
                crate::session::SessionStatus::Waiting,
            ),
            inj_sess(
                "sess_t5j",
                crate::session::AgentType::OpenCode,
                93,
                crate::session::SessionStatus::Waiting,
            ),
            inj_sess(
                "sess_t5k",
                crate::session::AgentType::Kimi,
                94,
                crate::session::SessionStatus::Waiting,
            ),
        ];
        Arc::new(RemoteState {
            ui_config_source: Box::new(|| None),
            subagent_source: std::collections::HashMap::new(),
            subagent_message_source: std::collections::HashMap::new(),
            target_evidence: no_target_evidence(),
            capability_table: crate::inject::capability::new_table(),
            session_source: Box::new(move || crate::session::SessionsResponse {
                sessions: sessions.clone(),
                total_count: sessions.len(),
                waiting_count: 0,
            }),
            store: crate::remote::pairing::DeviceStore::memory(),
            injector,
            resume_spawner: std::sync::Arc::new(|_: &crate::inject::resume::SpawnSpec| Ok(())),
            // C6：create 缝束缺省 stub（任务簿+发现/pid/工具探测/步距缝），仅
            // session-create 用例就地覆盖
            create_hub: create_hub_stub(),
            confirm_probe: std::sync::Arc::new(|_, _, _| true),
            // 丁T3：本组测试的对话框在场探针缺省「无法判定」（None）——控制类注入
            // 照常投递；「在场即拒」的用例就地建 state 覆盖为假体（见 mode_switch_* 用例）
            dialog_probe: std::sync::Arc::new(|_, _| None),
            screen_probe,
            host_source: Box::new(|| serde_json::Value::Null),
            message_source,
            path_source: Box::new(|_, _, _| (Vec::new(), false)),
            archive_source: Box::new(Vec::new),
            archive_delete: std::sync::Arc::new(|_: Option<&str>| 0usize),
            board_hidden_ids: Box::new(Vec::new),
            board_hidden_hide: std::sync::Arc::new(|_| 0usize),
            board_hidden_unhide: std::sync::Arc::new(|_| 0usize),
            unread_mark_read: std::sync::Arc::new(|_, _| ()),
            session_close: std::sync::Arc::new(|_| Ok(())),
            watcher_tx: tokio::sync::broadcast::channel(64).0,
            sse_registry: Arc::new(SseRegistry::default()),
            max_devices_source: Box::new(|| 3),
            pin_limiter: std::sync::Mutex::new(crate::remote::pin::PinRateLimiter::new()),
            global_pin_limiter: std::sync::Mutex::new(crate::remote::pin::PinRateLimiter::global()),
            pin_source: Box::new(|| Some("1234".to_string())),
            now_source: Box::new(|| chrono::Utc::now().timestamp_millis()),
            via_hosts_source: Box::new(|| None),
            rate_bucket_channels_source: Box::new(Vec::new),
            home_source: Box::new(|| None),
            // C7：配对计数缝缺省空表（既有用例零影响；配对打标/提示用例就地覆盖）
            pairing_counter: Box::new(Vec::new),
        })
    }

    /// 问答可用性（通道 A 标记路径）：sess_u 播种问题标记（探测档案单选夹具）→
    /// 200 available=true + source="mark" + questions 结构（header/question/
    /// multiSelect/options[{label,description}]）+ **无 key 字段**（键位不外泄给 UI）+
    /// gate：无 cookie → 403。
    #[tokio::test]
    async fn question_options_available_via_mark() {
        let fake = FakeInjector::ok();
        let state = question_state(fake.clone());
        persist_named_device(&state, "mm", "测试设备");
        state.store.with(|conn| {
            crate::database::dao::question_wait::mark(
                conn,
                "claude",
                "sess_u",
                1_000,
                "等待回答",
                Some(Q_SINGLE_PAYLOAD),
            )
        });
        let app = router(state.clone());
        // 无 cookie → 403（nest 内层 gate 结构性覆盖新端点）
        let r = app
            .clone()
            .oneshot(req(
                "GET",
                "/m/api/v1/session-question?session_id=sess_u",
                None,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 403, "问答端点必须过 gate");
        let r = app
            .clone()
            .oneshot(req(
                "GET",
                "/m/api/v1/session-question?session_id=sess_u",
                Some("mam_device=mm"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(
            r.headers()
                .get("cache-control")
                .and_then(|v| v.to_str().ok()),
            Some("no-store"),
            "问答可用性是门禁下私有数据，禁止中间层缓存"
        );
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["available"], true);
        assert_eq!(v["source"], "mark");
        let qs = v["questions"].as_array().expect("questions 应为数组");
        assert_eq!(qs.len(), 1, "探测档案单选夹具 = 单问题");
        assert_eq!(qs[0]["header"], "Next step");
        assert_eq!(
            qs[0]["question"],
            "This is a demo question — what would you like to do next?"
        );
        assert_eq!(qs[0]["multiSelect"], false);
        let opts = qs[0]["options"].as_array().unwrap();
        assert_eq!(opts.len(), 3);
        assert_eq!(opts[0]["label"], "Tool demo");
        assert_eq!(opts[0]["description"], "Explain how AskUserQuestion works.");
        assert_eq!(opts[2]["label"], "Nothing yet");
        assert!(
            fake.recorded_keys().is_empty() && fake.recorded().is_empty(),
            "查询端点不得触发任何注入"
        );
    }

    /// 问答应答（单选 select）：sess_v → 200 key_sent + FakeInjector 收到 (pid=32, "2")
    /// 恰一键（**无回车无 Esc**——探测 K1/K2：数字直接提交，后补 Esc 中断模型回合）+
    /// 审计 action=answer result=ok content=select#2。
    #[tokio::test]
    async fn question_answer_select_sends_digit() {
        // 2026-10-03：select 走前置焦点守卫编排（ClaudeSelect）→ 需要屏读缝——
        // stage_rig 脚本屏（焦点在选项行 → 守卫不触发，键序与旧断言一致）
        let (state, fake, _script) = stage_rig(
            vec![
                screen_fixtures::multi_option_focus(),
                screen_fixtures::multi_option_focus(),
            ],
            false,
        );
        state.store.with(|conn| {
            crate::database::dao::question_wait::mark(
                conn,
                "claude",
                "sess_v",
                1_000,
                "等待回答",
                Some(Q_SINGLE_PAYLOAD),
            )
        });
        let app = router(state.clone());
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-question/answer",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_v","action":"select","index":1}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(
            r.headers()
                .get("cache-control")
                .and_then(|v| v.to_str().ok()),
            Some("no-store"),
            "问答应答回执是门禁下私有数据，禁止中间层缓存"
        );
        let body = body_string(r).await;
        assert!(
            body.contains("\"status\":\"key_sent\""),
            "单选 select 应回执 key_sent：{body}"
        );
        assert_eq!(
            fake.recorded_keys(),
            vec![(32u32, "2".to_string())],
            "select#2 = 单个数字键（无回车无 Esc，探测 K1/K2 定案）：{:?}",
            fake.recorded_keys()
        );
        assert!(fake.recorded().is_empty(), "问答走按键通道，不走文本注入");
        let audits = state
            .store
            .with(|c| crate::database::dao::write_audit::recent_conn(c, 10));
        assert_eq!(audits.len(), 1);
        assert_eq!(
            audits[0].action, "answer",
            "问答审计 action=answer（词表追加）"
        );
        assert_eq!(audits[0].result, "ok");
        assert_eq!(audits[0].channel, "fake");
        assert_eq!(audits[0].session_id, "sess_v");
        assert_eq!(
            audits[0].summary, "select#2::select",
            "摘要 = 动作#UI编号（从 1 起）"
        );
    }

    /// **2026-09-24 改写**（原「toggle = 单数字」锁被用户实机推翻——claude 2.1.278
    /// 多选屏数字无反应）：toggle 走闭环切勾阶段机（屏读定位 → 空格 → 屏读校验翻转）。
    /// 切勾闭环（**2026-10-03 数字直选自适应**）：Unknown 首探 → 数字 "1" → 屏读
    /// 核验翻转成功 → 记 Supported。断言：键序 = `["1"]`（数字直发，零走位零空格）；
    /// 回执 key_sent + done + checked:true + verified:true + stage toggle-row。
    /// 还原动作：把 toggle 编排的数字探测段删掉 → 键序断言先红（出现 "space"）。
    #[tokio::test]
    async fn question_answer_toggle_sends_digit() {
        let (state, fake, _script) = stage_rig(
            vec![
                screen_fixtures::multi_option_focus(),
                screen_fixtures::multi_option_focus_checked(),
            ],
            false,
        );
        mark_question(&state, "claude", "sess_w", Q_MULTI_PAYLOAD);
        let app = router(state.clone());
        let r = app
            .oneshot(req(
                "POST",
                "/m/api/v1/session-question/answer",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_w","action":"toggle","index":0}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let body = body_string(r).await;
        assert!(
            body.contains("\"status\":\"key_sent\"")
                && body.contains("\"done\":true")
                && body.contains("\"checked\":true")
                && body.contains("\"verified\":true")
                && body.contains("\"stage\":\"toggle-row\""),
            "切勾闭环回执 = key_sent + done + checked + verified + stage：{body}"
        );
        // **屏读快照**（2026-10-03 屏读为准）：回执带切勾后整屏快照——TS 行内容
        // 随回执回传（夹具 TS 行 = "4. [ ] Type something" → freeTextPresent:true）
        assert!(
            body.contains("\"screen\":{") && body.contains("\"freeTextPresent\":true"),
            "回执必须携带屏读快照：{body}"
        );
        assert_eq!(
            fake.recorded_keys(),
            vec![(33u32, "1".to_string())],
            "toggle#1 = 数字直发（探测翻转成功 → Supported）：{:?}",
            fake.recorded_keys()
        );
        let audits = state
            .store
            .with(|c| crate::database::dao::write_audit::recent_conn(c, 10));
        assert_eq!(audits[0].action, "answer");
        assert_eq!(audits[0].summary, "toggle#1::toggle-row");
        assert_eq!(audits[0].result, "ok");
    }

    /// 切勾中止（端点级）：空格发出但勾选态未翻转（版本不消费该形态）——两屏相同。
    /// 断言：回执 failed + aborted + stage toggle-row + 点名「翻转」；审计
    /// `aborted:toggle-row`。还原动作：把核验段删掉 → 键序断言仍过但回执变 key_sent
    /// （谎报成功）→ 第一句断言先红。
    #[tokio::test]
    async fn question_toggle_aborts_when_flip_not_seen() {
        let (state, fake, _script) = stage_rig(
            vec![
                screen_fixtures::multi_option_focus(),
                screen_fixtures::multi_option_focus(),
            ],
            false,
        );
        // 能力预置 Unsupported（per-state 表；评审实施修正）：数字探测不参与，
        // 本用例专测「空格翻转未见过 → 中止」的走位路径
        let ver = crate::inject::approve::cached_cli_version("claude").unwrap_or_default();
        crate::inject::capability::set_digit_toggle_in(
            &state.capability_table,
            "claude",
            &ver, // 与调用侧同源 key（cached_cli_version）
            crate::inject::capability::DigitToggle::Unsupported,
        );
        mark_question(&state, "claude", "sess_af", Q_MULTI_PAYLOAD);
        let app = router(state.clone());
        let r = app
            .oneshot(req(
                "POST",
                "/m/api/v1/session-question/answer",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_af","action":"toggle","index":0}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let body = body_string(r).await;
        assert!(
            body.contains("\"status\":\"failed\"")
                && body.contains("\"aborted\":true")
                && body.contains("\"stage\":\"toggle-row\"")
                && body.contains("翻转"),
            "未翻转必须中止且报清原因：{body}"
        );
        assert_eq!(
            fake.recorded_keys(),
            vec![(42u32, "space".to_string())],
            "只发过空格（Unsupported 预置 = 零探测；无后续键）：{:?}",
            fake.recorded_keys()
        );
        let audits = state
            .store
            .with(|c| crate::database::dao::write_audit::recent_conn(c, 10));
        assert_eq!(audits[0].result, "aborted:toggle-row");
    }

    // ==== 丁T5：提交与自由作答的**阶段机端点用例** ====
    //
    // 阶段机要「每段屏读复核」，而真实屏读需要 conhost（CI 恒 None）→ 经
    // `RemoteState.screen_probe` 缝注入**脚本化屏序列**：每一屏都是**真机屏幕原文**
    // （2026-09-21 探测档案 `C-s8-*` / `C-s7-*` 逐字），序列按「按键会重绘」的时序
    // 推进。于是「段推进 / 段中止 / 回执三态」全部在门禁里可断言。
    //
    // **推进纪律**：屏序列由**假注入器**推（每次 `locate_and_send_key_spec` /
    // `locate_and_inject_spec` 调用推一格）——这模拟生产的时序（按键 → TUI 重绘），
    // 而屏读缝本身**只读当前屏**（不推进）。两者分开才能在测试里表达「按键被吞」
    // （发了键但屏不变）这类形态：那时把两次注入映射到同一屏即可。
    mod stage_screen {
        use std::sync::{Arc, Mutex};

        /// 屏序列 + 推进计数（按键推一格）。`Arc` 便于注入到假注入器与屏读缝两处。
        pub struct Script {
            pub screens: Vec<Vec<String>>,
            pub pos: Mutex<usize>,
        }

        impl Script {
            pub fn new(screens: Vec<Vec<String>>) -> Arc<Self> {
                Arc::new(Self {
                    screens,
                    pos: Mutex::new(0),
                })
            }
            /// 当前屏（不推进）；序列为空 → None（= 读不到屏）
            pub fn current(&self) -> Option<Vec<String>> {
                let p = *self.pos.lock().unwrap();
                self.screens
                    .get(p.min(self.screens.len().saturating_sub(1)))
                    .cloned()
            }
            /// 按键 → TUI 重绘（推一格；到末屏则停——生产侧「屏不再变」）
            pub fn advance(&self) {
                let mut p = self.pos.lock().unwrap();
                if *p + 1 < self.screens.len() {
                    *p += 1;
                }
            }
            /// 屏读缝：只读当前屏（`None` = 读不到屏）
            pub fn probe(self: &Arc<Self>) -> Arc<crate::remote::server::ScreenProbeFn> {
                let me = self.clone();
                Arc::new(move |_sid: &str, _pid: u32| me.current())
            }
        }
    }

    /// 推屏的假注入器（在既有 [`FakeInjector`] 的记账之外，把每次注入映射成一次重绘）。
    /// 直接复用 `FakeInjector` + 一个 `Script`：本包装只做「转发 + 推进」。
    struct AdvancingInjector {
        inner: Arc<FakeInjector>,
        script: Arc<stage_screen::Script>,
        /// 失败开关（透传给内层；本包装只需知道「是否推进」——失败时不推进，因为
        /// 没送到终端就不会重绘）
        failing: bool,
    }

    impl crate::inject::engine::Injector for AdvancingInjector {
        fn name(&self) -> &'static str {
            self.inner.name()
        }
        fn locate_and_inject(&self, pid: u32, text: &str) -> Result<(), String> {
            let r = self.inner.locate_and_inject(pid, text);
            if r.is_ok() && !self.failing {
                self.script.advance();
            }
            r
        }
        fn locate_and_send_key(&self, pid: u32, key: &str) -> Result<(), String> {
            let r = self.inner.locate_and_send_key(pid, key);
            if r.is_ok() && !self.failing {
                self.script.advance();
            }
            r
        }
        fn locate_and_send_key_spec(
            &self,
            pid: u32,
            key: &str,
            spec: &crate::inject::families::FamilySpec,
        ) -> Result<(), String> {
            let r = self.inner.locate_and_send_key_spec(pid, key, spec);
            if r.is_ok() && !self.failing {
                self.script.advance();
            }
            r
        }
        fn locate_and_inject_spec(
            &self,
            pid: u32,
            text: &str,
            spec: &crate::inject::families::FamilySpec,
        ) -> Result<(), String> {
            let r = self.inner.locate_and_inject_spec(pid, text, spec);
            if r.is_ok() && !self.failing {
                self.script.advance();
            }
            r
        }
    }

    /// 阶段机用例的 state：`question_state_full` + 脚本化屏读缝（会话清单同）。
    fn question_state_scripted(
        injector: Arc<dyn crate::inject::engine::Injector>,
        script: Arc<stage_screen::Script>,
    ) -> Arc<RemoteState> {
        question_state_full(
            injector,
            Box::new(|_, _, _| Err("测试桩：未注入内容源".to_string())),
            script.probe(),
        )
    }

    /// 阶段机用例的真机屏幕夹具（探测档案 2026-09-21 原文，逐字；与
    /// `inject::question::tests` 的同名夹具同源——两处都是「一份原文两处引用」，
    /// 端点侧需要独立可用的副本，否则测试模块间要提权互引）。
    mod screen_fixtures {
        fn lines(v: &[&str]) -> Vec<String> {
            v.iter().map(|s| s.to_string()).collect()
        }
        /// `C-s8-cursor-submit-20260921-015844.png`（焦点在 Submit 行）
        pub fn submit_focused() -> Vec<String> {
            lines(&[
                " ← ☒ Favorite fruits  ✔Submit  →",
                "",
                " Which fruits are your favorites? (Select all that apply)",
                "",
                " 1. [✓] Apple",
                " A sweet, crisp fruit available in many varieties.",
                " 2. [ ] Banana",
                " A soft, tropical fruit rich in potassium.",
                " 3. [ ] Peach",
                " A juicy summer fruit with a stone pit.",
                " 4. [ ] Type something",
                " ❯   Submit",
                " 5. Chat about this",
                "",
                " Enter to select · ↑/ to navigate · Esc to cancel",
            ])
        }
        /// `C-s8-submitted-20260921-015906.png`（Review 确认屏）
        pub fn review() -> Vec<String> {
            lines(&[
                " ← ☒ Favorite fruits  ✔Submit  →",
                "",
                " Review your answers",
                "",
                " ● Which fruits are your favorites? (Select all that apply)",
                " → Banana, Apple",
                "",
                " Ready to submit your answers?",
                "",
                " 1. Submit answers",
                " 2. Cancel",
            ])
        }
        /// `C-s8-final-20260921-015941.png`（终态）
        pub fn answered() -> Vec<String> {
            lines(&[
                " ● User answered Claude's questions:)",
                " L  • Which fruits are your favorites? (Select all that apply) → Banana, Apple",
                "",
                " Thought for 2s (ctrl+o to expand)",
            ])
        }
        /// `C-s8-digit1-checked-20260921-015738.png` 形态（焦点在首选项行、未勾）
        pub fn multi_option_focus() -> Vec<String> {
            lines(&[
                " ← ☒ Favorite fruits  ✔Submit  →",
                "",
                " Which fruits are your favorites? (Select all that apply)",
                "",
                " ❯ 1. [ ] Apple",
                " A sweet, crisp fruit available in many varieties.",
                " 2. [ ] Banana",
                " A soft, tropical fruit rich in potassium.",
                " 3. [ ] Peach",
                " A juicy summer fruit with a stone pit.",
                " 4. [ ] Type something",
                "    Submit",
                " 5. Chat about this",
                "",
                " Enter to select · ↑/ to navigate · Esc to cancel",
            ])
        }
        /// 同屏但首项已勾（空格生效后的重绘形态）
        pub fn multi_option_focus_checked() -> Vec<String> {
            lines(&[
                " ← ☒ Favorite fruits  ✔Submit  →",
                "",
                " Which fruits are your favorites? (Select all that apply)",
                "",
                " ❯ 1. [✓] Apple",
                " A sweet, crisp fruit available in many varieties.",
                " 2. [ ] Banana",
                " A soft, tropical fruit rich in potassium.",
                " 3. [ ] Peach",
                " A juicy summer fruit with a stone pit.",
                " 4. [ ] Type something",
                "    Submit",
                " 5. Chat about this",
                "",
                " Enter to select · ↑/ to navigate · Esc to cancel",
            ])
        }
        /// 普通输出屏（无任何阶段锚）
        pub fn plain() -> Vec<String> {
            lines(&["  some output", " > "])
        }
        /// `C-s7-q-ui-20260921-015440.png`（单选自由作答屏）
        pub fn free_row() -> Vec<String> {
            lines(&[
                " ☐ Preferred drink",
                "",
                " Which drink do you prefer?",
                "",
                " 1. Coffee",
                " 2. Tea",
                " 3. Type something.",
                "",
                " 4. Chat about this",
            ])
        }
        /// 焦点已落在自由作答行（`C-s7-text-in-input-*` 形态）
        pub fn free_row_focused() -> Vec<String> {
            lines(&[
                " ☐ Preferred drink",
                "",
                " Which drink do you prefer?",
                "",
                " 1. Coffee",
                " 2. Tea",
                " ❯ 3. Type something.",
                "",
                " 4. Chat about this",
            ])
        }
    }

    /// 阶段机用例的**装配**：脚本屏序列 + 推屏假注入器 + 注入缝的 state + 已配对设备。
    /// 返回 `(state, fake, script)`——`fake` 用于断言「实际发了哪些键/哪些文本」。
    fn stage_rig(
        screens: Vec<Vec<String>>,
        failing: bool,
    ) -> (
        Arc<RemoteState>,
        Arc<FakeInjector>,
        Arc<stage_screen::Script>,
    ) {
        let script = stage_screen::Script::new(screens);
        let inner = if failing {
            FakeInjector::failing("注入通道拒绝")
        } else {
            FakeInjector::ok()
        };
        let adv = Arc::new(AdvancingInjector {
            inner: inner.clone(),
            script: script.clone(),
            failing,
        });
        let state = question_state_scripted(adv, script.clone());
        persist_named_device(&state, "mm", "测试设备");
        (state, inner, script)
    }

    /// 播种问题标记（阶段机用例的公共前置；`sid` 由各用例独占——守卫 id 立规）。
    fn mark_question(state: &Arc<RemoteState>, tool: &str, sid: &str, payload: &str) {
        state.store.with(|conn| {
            crate::database::dao::question_wait::mark(
                conn,
                tool,
                sid,
                1_000,
                "等待回答",
                Some(payload),
            )
        });
    }

    /// **丁T5 端到端①（多选提交 happy path）**：提交屏（焦点已在 Submit 行）→ 回车 →
    /// Review 屏 → 抄屏上编号 '1' → 终态屏。
    ///
    /// 断言：键序 = `[enter, "1"]`（**零 down**——焦点本就在提交行）；回执带
    /// `done:true` + `verified:true`（屏读确认了终态）；审计摘要 = `submit::receipt`
    /// （段名进摘要）。还原动作：把 `run_submit_stages` 换回批次丙的盲发序列
    /// （down×4 + enter + '1'）→ 第一句键序断言先红。
    #[tokio::test]
    async fn question_submit_stage_machine_happy_path() {
        let (state, fake, _script) = stage_rig(
            vec![
                screen_fixtures::submit_focused(),
                screen_fixtures::review(),
                screen_fixtures::answered(),
            ],
            false,
        );
        mark_question(&state, "claude", "sess_t5a", Q_MULTI_PAYLOAD);
        let app = router(state.clone());
        let r = app
            .oneshot(req(
                "POST",
                "/m/api/v1/session-question/answer",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_t5a","action":"submit"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let body = body_string(r).await;
        assert!(
            body.contains("\"status\":\"key_sent\"")
                && body.contains("\"done\":true")
                && body.contains("\"verified\":true"),
            "闭环走完的回执 = key_sent + done + verified=true：{body}"
        );
        assert_eq!(
            fake.recorded_keys(),
            vec![(84u32, "enter".to_string()), (84u32, "1".to_string())],
            "键序 = [enter, '1']（焦点已在 Submit 行 → 零走位；确认键抄自 Review 屏）：{:?}",
            fake.recorded_keys()
        );
        let audits = state
            .store
            .with(|c| crate::database::dao::write_audit::recent_conn(c, 10));
        assert_eq!(audits[0].action, "answer");
        assert_eq!(
            audits[0].summary, "submit::receipt",
            "审计摘要带段名（走完的段 = receipt）"
        );
        assert_eq!(audits[0].result, "ok");
    }

    /// **丁T5 端到端②（提交路径中止：未见 Review 屏）**——问题 7 的正解。
    ///
    /// 脚本：提交屏 → 回车后屏不变（Review 屏未出现，轮询窗尽）。断言：发了 enter、
    /// **没发任何数字**；回执 `status:"failed"` + `aborted:true` + `stage:"review"`
    /// + 中文 error（用户可读）；审计 result = `aborted:review`。
    /// 还原动作：把 Review 段改回「不等屏读直接发 '1'」→ 第二句键序断言先红。
    #[tokio::test]
    async fn question_submit_stage_machine_aborts_when_review_absent() {
        // 两屏都是提交屏：回车推进到第二屏（还是提交屏）→ Review 轮询窗尽
        let (state, fake, _script) = stage_rig(
            vec![
                screen_fixtures::submit_focused(),
                screen_fixtures::submit_focused(),
            ],
            false,
        );
        mark_question(&state, "claude", "sess_t5b", Q_MULTI_PAYLOAD);
        let app = router(state.clone());
        let r = app
            .oneshot(req(
                "POST",
                "/m/api/v1/session-question/answer",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_t5b","action":"submit"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "中止走 200 failed 槽（可重试语义）");
        let body = body_string(r).await;
        assert!(
            body.contains("\"status\":\"failed\"")
                && body.contains("\"aborted\":true")
                && body.contains("\"stage\":\"review\""),
            "中止回执须可程序分诊到段（failed+aborted+stage）：{body}"
        );
        assert!(
            body.contains("Review 确认屏"),
            "error 文案须点名缺什么（用户可读）：{body}"
        );
        assert_eq!(
            fake.recorded_keys(),
            vec![(85u32, "enter".to_string())],
            "**绝不发确认数字**（Review 屏不在场）：{:?}",
            fake.recorded_keys()
        );
        let audits = state
            .store
            .with(|c| crate::database::dao::write_audit::recent_conn(c, 10));
        assert_eq!(audits[0].action, "answer");
        assert_eq!(audits[0].summary, "submit::review", "段名进审计摘要");
        assert_eq!(audits[0].result, "aborted:review");
    }

    /// **丁T5 端到端③（走位失败中止）**：屏上只有提交屏但**焦点在选项行**（每次 ↓
    /// 后屏不变 = 终端吞键）→ 走位上限耗尽 → 中止且**不发回车**。
    ///
    /// 断言：键全是 `down`（一个不落都是走位键）、**没有 enter**；`stage:"submit-row"`。
    /// 还原动作：删掉走位复核（发满就回车）→ 第二句断言先红。
    #[tokio::test]
    async fn question_submit_stage_machine_aborts_when_walk_stalls() {
        // 焦点永在选项行（屏不推进 → 每次读都是同一屏）
        let mut stuck = screen_fixtures::submit_focused();
        for l in stuck.iter_mut() {
            if l.contains("❯   Submit") {
                *l = "    Submit".to_string(); // 提交行在场但无焦点标记
            }
            if l.contains("1. [✓] Apple") {
                *l = format!(" ❯{l}");
            }
        }
        let (state, fake, _script) = stage_rig(vec![stuck], false);
        mark_question(&state, "claude", "sess_t5c", Q_MULTI_PAYLOAD);
        let app = router(state.clone());
        let r = app
            .oneshot(req(
                "POST",
                "/m/api/v1/session-question/answer",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_t5c","action":"submit"}"#),
            ))
            .await
            .unwrap();
        let body = body_string(r).await;
        assert!(
            body.contains("\"stage\":\"submit-row\"") && body.contains("\"aborted\":true"),
            "走位失败的中止段 = submit-row：{body}"
        );
        let keys = fake.recorded_keys();
        assert!(
            !keys.iter().any(|(_, k)| k == "enter"),
            "走位未到位 ⇒ **绝不发回车**：{keys:?}"
        );
        assert!(
            keys.iter().all(|(_, k)| k == "down") && !keys.is_empty(),
            "只发过走位键（且确实发过——上限 3+2=5 次）：{keys:?}"
        );
    }

    /// **丁T5 端到端④（终态回执未见 → 不谎报完成）**：闭环走完（提交屏 → Review →
    /// 确认已发），但终态屏读窗内只有普通输出 → `verified:false`（**仍是 key_sent +
    /// done**——键确实发出去了，事实是「投递完成、未见回执」）。
    ///
    /// 还原动作：把 `StageDone` 的 verified 改成恒 true → 第一句断言先红。
    #[tokio::test]
    async fn question_submit_stage_machine_receipt_unseen_not_lied() {
        let (state, fake, _script) = stage_rig(
            vec![
                screen_fixtures::submit_focused(),
                screen_fixtures::review(),
                screen_fixtures::plain(),
            ],
            false,
        );
        mark_question(&state, "claude", "sess_t5d", Q_MULTI_PAYLOAD);
        let app = router(state.clone());
        let r = app
            .oneshot(req(
                "POST",
                "/m/api/v1/session-question/answer",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_t5d","action":"submit"}"#),
            ))
            .await
            .unwrap();
        let body = body_string(r).await;
        assert!(
            body.contains("\"done\":true") && body.contains("\"verified\":false"),
            "闭环走完但未见终态回执 = done:true + verified:false（不谎报）：{body}"
        );
        assert_eq!(
            fake.recorded_keys(),
            vec![(87u32, "enter".to_string()), (87u32, "1".to_string())]
        );
        let audits = state
            .store
            .with(|c| crate::database::dao::write_audit::recent_conn(c, 10));
        assert_eq!(
            audits[0].result, "ok:receipt-unseen",
            "审计如实记「已投递但未核验到回执」"
        );
    }

    /// **丁T5 复评 F6-2（恢复既有覆盖）：阶段机中途投递失败 = 「投递失败」而非「中止」**。
    ///
    /// 覆盖两条既有断言（父提交 `b92a658` 的
    /// `question_answer_submit_first_error_stops_sequence_and_audits_failed` 锁的是
    /// 同一件事，本批重构时被误删——复评 Important-2）：
    /// ① **首错即停**：走位段第一个 `down` 就 Err → **后续键一个都不再投**；
    /// ② 回执/审计语义：`status:"failed"` **不带** `aborted`（投递失败 ≠ 阶段判据中止），
    ///    审计 `failed:<e>` 前缀口径逐字同批次丙。
    ///
    /// 用 `stage_rig(..., failing=true)`——该参数此前从未被传过 `true`（复评指出为
    /// 死参数），本用例是它唯一的消费者。
    /// 还原动作：把 `StageAbortKind::Delivery` 并回 `Screen`（回执变
    /// `aborted:true`）→ 第二句断言先红；去掉首错即停（继续投后续键）→ 第一句先红。
    #[tokio::test]
    async fn question_submit_stage_machine_delivery_failure_stops_and_audits_failed() {
        // 提交屏（焦点在选项行）→ 需要一个 down 才到位 → 第一个 down 即失败
        let mut stuck = screen_fixtures::submit_focused();
        for l in stuck.iter_mut() {
            if l.contains("❯   Submit") {
                *l = "    Submit".to_string();
            }
            if l.contains("1. [✓] Apple") {
                *l = format!(" ❯{l}");
            }
        }
        let (state, fake, _script) = stage_rig(vec![stuck], true); // failing=true
        state.store.with(|conn| {
            crate::database::dao::question_wait::mark(
                conn,
                "claude",
                "sess_ak",
                1_000,
                "等待回答",
                Some(Q_MULTI_PAYLOAD),
            )
        });
        let app = router(state.clone());
        let r = app
            .oneshot(req(
                "POST",
                "/m/api/v1/session-question/answer",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_ak","action":"submit"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "投递失败走 200 failed 槽（可重试）");
        let body = body_string(r).await;
        assert!(
            body.contains("\"status\":\"failed\"") && body.contains("注入通道拒绝"),
            "投递失败须 200 failed 并透传错误文案：{body}"
        );
        assert_eq!(
            fake.recorded_keys(),
            vec![(46u32, "down".to_string())],
            "**首错即停**：走位第一个 down 即 Err，恰一条、后续键不再出手：{:?}",
            fake.recorded_keys()
        );
        assert!(
            !body.contains("\"aborted\""),
            "投递失败**不得**报成阶段中止（aborted 是「屏上形态不符」的语义，两者用户动作不同）：{body}"
        );
        let audits = state
            .store
            .with(|c| crate::database::dao::write_audit::recent_conn(c, 10));
        assert_eq!(audits.len(), 1);
        assert_eq!(audits[0].action, "answer");
        assert_eq!(
            audits[0].result, "failed:方向键 down 投递失败（注入通道拒绝）",
            "审计 result = failed:错误文案（与批次丙同前缀口径；2026-09-24 走位方向感知后文案带键名）"
        );
        assert_eq!(audits[0].summary, "submit", "投递失败不追加段名");
        assert_eq!(audits[0].session_id, "sess_ak");
    }

    /// **丁T5 端到端⑤（读屏不可用 → 零投递中止）**：`screen_probe` 恒 `None`
    /// （非 Windows / 屏读失败）→ 第 1 段就中止，**零按键**。这是安全面：读不到屏
    /// 就绝不猜着发键。
    /// 还原动作：把第 1 段的屏读检查删掉（默认形态成立）→ 本用例先红（会发 enter）。
    #[tokio::test]
    async fn question_submit_stage_machine_aborts_with_zero_keys_when_screen_unavailable() {
        let (state, fake, _script) = stage_rig(vec![], false); // 空序列 → 恒 None
        mark_question(&state, "claude", "sess_t5e", Q_MULTI_PAYLOAD);
        let app = router(state.clone());
        let r = app
            .oneshot(req(
                "POST",
                "/m/api/v1/session-question/answer",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_t5e","action":"submit"}"#),
            ))
            .await
            .unwrap();
        let body = body_string(r).await;
        assert!(
            body.contains("\"aborted\":true") && body.contains("\"stage\":\"submit-row\""),
            "读不到屏的中止段 = submit-row：{body}"
        );
        assert!(
            fake.recorded_keys().is_empty() && fake.recorded().is_empty(),
            "读不到屏 ⇒ 零投递（一个键都不猜）：keys={:?} texts={:?}",
            fake.recorded_keys(),
            fake.recorded()
        );
    }

    /// **丁T5 端到端⑥（自由作答 happy path，裁3 安全面）**：单选屏 → 定位 `Type
    /// something`（屏上编号 3）→ 文本（**字符通道**）→ 回车 → 终态。
    ///
    /// 断言：键通道 = `['3', 'enter']`（**无 Esc 无第二数字**）；文本通道恰好一次且
    /// 内容 = 用户文本、**不带** `[mobile]` 签名；回执 `done:true` +
    /// `verified:true`；审计摘要 = `freeText::free-text`（**不含正文**）。
    /// 还原动作：把文本改走 `locate_and_send_key_spec`（键通道）→ 第二句断言先红。
    #[tokio::test]
    async fn question_free_text_stage_happy_path_sends_text_via_text_channel() {
        let (state, fake, _script) = stage_rig(
            vec![
                screen_fixtures::free_row(),
                screen_fixtures::free_row_focused(),
                screen_fixtures::free_row_focused(),
                screen_fixtures::answered(),
            ],
            false,
        );
        mark_question(&state, "claude", "sess_t5f", Q_SINGLE_PAYLOAD);
        let app = router(state.clone());
        let r = app
            .oneshot(req(
                "POST",
                "/m/api/v1/session-question/answer",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_t5f","action":"freeText","text":"green tea please"}"#),
            ))
            .await
            .unwrap();
        let body = body_string(r).await;
        assert!(
            body.contains("\"status\":\"key_sent\"")
                && body.contains("\"done\":true")
                && body.contains("\"verified\":true"),
            "自由作答闭环回执：{body}"
        );
        assert_eq!(
            fake.recorded_keys(),
            vec![(89u32, "3".to_string()), (89u32, "enter".to_string())],
            "键通道 = [定位数字(屏上编号 3), 提交回车]——**无 Esc 无第二数字**：{:?}",
            fake.recorded_keys()
        );
        assert_eq!(
            fake.recorded(),
            vec![(89u32, "green tea please".to_string())],
            "文本恰一次、走字符通道、**不带 [mobile] 签名**：{:?}",
            fake.recorded()
        );
        let audits = state
            .store
            .with(|c| crate::database::dao::write_audit::recent_conn(c, 10));
        assert_eq!(audits[0].action, "answer");
        assert_eq!(audits[0].summary, "freeText::free-text");
        assert!(
            !audits[0].summary.contains("green"),
            "审计摘要**不得承载用户正文**：{}",
            audits[0].summary
        );
    }

    /// **丁T5 端到端⑦（自由作答文本归一）**：换行/控制字符经 `normalize_newlines`
    /// 后的形态进字符通道（**不含裸换行 / 控制字符**——注入通道安全面）。
    /// 还原动作：把归一那行删掉（直接投原文）→ 断言先红（文本里会出现裸换行）。
    #[tokio::test]
    async fn question_free_text_text_is_normalized_before_injection() {
        let (state, fake, _script) = stage_rig(
            vec![
                screen_fixtures::free_row(),
                screen_fixtures::free_row_focused(),
                screen_fixtures::free_row_focused(),
                screen_fixtures::answered(),
            ],
            false,
        );
        mark_question(&state, "claude", "sess_t5g", Q_SINGLE_PAYLOAD);
        let app = router(state.clone());
        // JSON 里的 \n 是**真换行**（JSON 转义），\u001b 是真 ESC
        let payload =
            r#"{"sessionId":"sess_t5g","action":"freeText","text":"第一行\n第二行\u001b[31m"}"#;
        let r = app
            .oneshot(req(
                "POST",
                "/m/api/v1/session-question/answer",
                Some("mam_device=mm"),
                Some(payload),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let texts = fake.recorded();
        assert_eq!(texts.len(), 1, "文本恰一次：{texts:?}");
        let sent = &texts[0].1;
        assert!(
            sent.contains("第一行\\n第二行") && sent.ends_with("[31m"),
            "换行 → 字面 \\n、ESC 剥除（归一）：{sent:?}"
        );
        assert!(
            !sent.chars().any(|c| (c as u32) <= 0x1F),
            "注入文本零 C0 残留（通道安全面）：{sent:?}"
        );
        assert!(
            !sent.contains("[mobile"),
            "作答文本**不带** mobile 签名（签名是消息语义，作答不是消息）：{sent:?}"
        );
    }

    /// **丁T5 端到端⑧（自由作答的空文本 → 400）**：入口参数校验拦下（比 200 failed
    /// 更准确——「参数就不对」）。
    /// 还原动作：删掉 handler 的 freeText 空文本检查 → 本用例先红（变成 200）。
    #[tokio::test]
    async fn question_free_text_empty_text_is_bad_request() {
        let (state, fake, _script) = stage_rig(vec![screen_fixtures::free_row()], false);
        mark_question(&state, "claude", "sess_t5h", Q_SINGLE_PAYLOAD);
        let app = router(state.clone());
        for payload in [
            r#"{"sessionId":"sess_t5h","action":"freeText"}"#,
            r#"{"sessionId":"sess_t5h","action":"freeText","text":"   "}"#,
            r#"{"sessionId":"sess_t5h","action":"freeText","text":"\n\t"}"#,
        ] {
            let r = app
                .clone()
                .oneshot(req(
                    "POST",
                    "/m/api/v1/session-question/answer",
                    Some("mam_device=mm"),
                    Some(payload),
                ))
                .await
                .unwrap();
            assert_eq!(r.status(), 400, "空/纯空白文本 → 400：{payload}");
        }
        assert!(
            fake.recorded_keys().is_empty() && fake.recorded().is_empty(),
            "400 路径零投递零审计"
        );
    }

    /// 单工具 state（阶段机工具面用例共用）：会话清单仅一条 + 脚本屏读缝。
    fn single_tool_scripted_state(
        tool: crate::session::AgentType,
        sid: &str,
        pid: u32,
        screens: Vec<Vec<String>>,
    ) -> (
        Arc<RemoteState>,
        Arc<FakeInjector>,
        Arc<stage_screen::Script>,
    ) {
        let script = stage_screen::Script::new(screens);
        let inner = FakeInjector::ok();
        let adv = Arc::new(AdvancingInjector {
            inner: inner.clone(),
            script: script.clone(),
            failing: false,
        });
        let session = inj_sess(sid, tool, pid, crate::session::SessionStatus::Waiting);
        let state = Arc::new(RemoteState {
            ui_config_source: Box::new(|| None),
            subagent_source: std::collections::HashMap::new(),
            subagent_message_source: std::collections::HashMap::new(),
            target_evidence: no_target_evidence(),
            capability_table: crate::inject::capability::new_table(),
            session_source: Box::new(move || crate::session::SessionsResponse {
                sessions: vec![session.clone()],
                total_count: 1,
                waiting_count: 0,
            }),
            store: crate::remote::pairing::DeviceStore::memory(),
            injector: adv,
            resume_spawner: std::sync::Arc::new(|_: &crate::inject::resume::SpawnSpec| Ok(())),
            // C6：create 缝束缺省 stub（任务簿+发现/pid/工具探测/步距缝），仅
            // session-create 用例就地覆盖
            create_hub: create_hub_stub(),
            confirm_probe: std::sync::Arc::new(|_, _, _| true),
            dialog_probe: std::sync::Arc::new(|_, _| None),
            screen_probe: script.probe(),
            host_source: Box::new(|| serde_json::Value::Null),
            message_source: Box::new(|_, _, _| Err("测试桩：未注入内容源".to_string())),
            path_source: Box::new(|_, _, _| (Vec::new(), false)),
            archive_source: Box::new(Vec::new),
            archive_delete: std::sync::Arc::new(|_: Option<&str>| 0usize),
            board_hidden_ids: Box::new(Vec::new),
            board_hidden_hide: std::sync::Arc::new(|_| 0usize),
            board_hidden_unhide: std::sync::Arc::new(|_| 0usize),
            unread_mark_read: std::sync::Arc::new(|_, _| ()),
            session_close: std::sync::Arc::new(|_| Ok(())),
            watcher_tx: tokio::sync::broadcast::channel(64).0,
            sse_registry: Arc::new(SseRegistry::default()),
            max_devices_source: Box::new(|| 3),
            pin_limiter: std::sync::Mutex::new(crate::remote::pin::PinRateLimiter::new()),
            global_pin_limiter: std::sync::Mutex::new(crate::remote::pin::PinRateLimiter::global()),
            pin_source: Box::new(|| Some("1234".to_string())),
            now_source: Box::new(|| chrono::Utc::now().timestamp_millis()),
            via_hosts_source: Box::new(|| None),
            rate_bucket_channels_source: Box::new(Vec::new),
            home_source: Box::new(|| None),
            // C7：配对计数缝缺省空表（既有用例零影响；配对打标/提示用例就地覆盖）
            pairing_counter: Box::new(Vec::new),
        });
        persist_named_device(&state, "mm", "测试设备");
        (state, inner, script)
    }

    /// **丁T5 端到端⑨（自由作答的工具面）→ 批次戊 E4/E5/E6 更新**：kimi（Other 行）
    /// /codex（Tab 备注）/opencode（own answer）均已升格走各自阶段机。本用例改钉
    /// opencode freeText 的**阶段机中止**：喂无 opencode 锚的屏（claude free_row 形态）
    /// → 第 1 段定位不到 own answer 行 → 200 failed{aborted:true}（零投递）。
    #[tokio::test]
    async fn question_free_text_refused_for_unverified_tools() {
        for (tool, tool_id, sid, pid) in [(
            crate::session::AgentType::OpenCode,
            "opencode",
            "sess_t5j",
            93u32,
        )] {
            let (state, inner, _s) =
                single_tool_scripted_state(tool, sid, pid, vec![screen_fixtures::free_row()]);
            mark_question(&state, tool_id, sid, Q_SINGLE_PAYLOAD);
            let app = router(state.clone());
            let r = app
                .oneshot(req(
                    "POST",
                    "/m/api/v1/session-question/answer",
                    Some("mam_device=mm"),
                    Some(&format!(
                        r#"{{"sessionId":"{sid}","action":"freeText","text":"hi"}}"#
                    )),
                ))
                .await
                .unwrap();
            assert_eq!(r.status(), 200, "opencode freeText 走阶段机（非 409）");
            let body = body_string(r).await;
            assert!(
                body.contains("aborted"),
                "无 own answer 锚 → 阶段机中止：{body}"
            );
            assert!(
                inner.recorded_keys().is_empty() && inner.recorded().is_empty(),
                "第 1 段定位即中止 → 零投递"
            );
        }
    }

    /// **多选题自由作答全链（2026-10-02 活体取证后升格）**：claude 多选 freeText
    /// 走 `run_multi_select_free_text_stages`——焦点阶梯走位到 Type something 行
    ///（方向键，多选屏数字无效）→ 字符通道打字 → 屏读核验「文字入行 + 勾选保持」→
    /// **零后续键**（回车会取消勾选——活体取证）。
    /// 还原动作（变异）：编排补发 enter → 键序断言先红。
    #[tokio::test]
    async fn question_claude_multi_select_free_text_inline_edits() {
        let (state, inner, _s) = single_tool_scripted_state(
            crate::session::AgentType::Claude,
            "sess_t5mft",
            95u32,
            vec![
                crate::inject::question::live_fixtures::q1_multi(), // 首段 + 走位 read（焦点选项1）
                crate::inject::question::live_fixtures::q1_focus_at(1),
                crate::inject::question::live_fixtures::q1_focus_at(2),
                crate::inject::question::live_fixtures::q1_focus_at(3),
                crate::inject::question::live_fixtures::q1_focus_at(4), // 焦点到 FreeText 行
                crate::inject::question::live_fixtures::q1_typed_at("hi", true, 37), // 打字后（注入推进一屏）
            ],
        );
        mark_question(&state, "claude", "sess_t5mft", Q_MULTI_PAYLOAD);
        let app = router(state.clone());
        let r = app
            .oneshot(req(
                "POST",
                "/m/api/v1/session-question/answer",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_t5mft","action":"freeText","text":"hi","questionIndex":0}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "claude 多选自由作答放行（2026-10-02）");
        let body = body_string(r).await;
        assert!(
            body.contains("\"status\":\"key_sent\"") && body.contains("\"stage\":\"free-text\""),
            "走完全链的回执：{body}"
        );
        // 键序 = down×4（走位）+ 文本（字符通道）；**无 enter**（回车会取消勾选）
        assert_eq!(
            inner.recorded_keys(),
            vec![
                (95u32, "down".to_string()),
                (95u32, "down".to_string()),
                (95u32, "down".to_string()),
                (95u32, "down".to_string()),
            ],
            "键序 = down×4（走位到 Type something 行）：{:?}",
            inner.recorded_keys()
        );
        assert_eq!(
            inner.recorded(),
            vec![(95u32, "hi".to_string())],
            "文本走字符通道"
        );
        let audits = state
            .store
            .with(|c| crate::database::dao::write_audit::recent_conn(c, 10));
        // 多选自由作答不做终态回执核验（receipt_seen=None）→ 审计如实记不可验证
        assert_eq!(audits[0].result, "ok:receipt-unverifiable");
    }

    /// **多选自由作答·编辑覆盖**（2026-10-02）：overwrite=true 时已有内容行先退格
    /// 清空（按屏读长度迭代）再打新字 + 勾选兜底。断言键序 = down×4 + backspace×3 +
    /// text + space（**无 enter**——回车会取消勾选）。
    #[tokio::test]
    async fn question_claude_multi_select_free_text_overwrite_edits() {
        let (state, inner, _s) = single_tool_scripted_state(
            crate::session::AgentType::Claude,
            "sess_t5mfo",
            95u32,
            vec![
                crate::inject::question::live_fixtures::q1_typed_at("旧内容", true, 29),
                crate::inject::question::live_fixtures::q1_typed_at("旧内容", true, 31),
                crate::inject::question::live_fixtures::q1_typed_at("旧内容", true, 33),
                crate::inject::question::live_fixtures::q1_typed_at("旧内容", true, 35),
                crate::inject::question::live_fixtures::q1_typed_at("旧内容", true, 37),
                // right×3（推到行尾，屏不变）——2026-10-03 行首光标 bug 修复
                crate::inject::question::live_fixtures::q1_typed_at("旧内容", true, 37),
                crate::inject::question::live_fixtures::q1_typed_at("旧内容", true, 37),
                crate::inject::question::live_fixtures::q1_typed_at("旧内容", true, 37),
                crate::inject::question::live_fixtures::q1_typed_at("旧", true, 37),
                crate::inject::question::live_fixtures::q1_typed_at("旧", true, 37),
                crate::inject::question::live_fixtures::q1_typed_at("Type something", false, 37),
                crate::inject::question::live_fixtures::q1_typed_at("新内容", false, 37),
                crate::inject::question::live_fixtures::q1_typed_at("新内容", true, 37),
            ],
        );
        mark_question(&state, "claude", "sess_t5mfo", Q_MULTI_PAYLOAD);
        let app = router(state.clone());
        let r = app
            .oneshot(req(
                "POST",
                "/m/api/v1/session-question/answer",
                Some("mam_device=mm"),
                Some(
                    r#"{"sessionId":"sess_t5mfo","action":"freeText","text":"新内容","questionIndex":0,"overwrite":true}"#,
                ),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let body = body_string(r).await;
        assert!(
            body.contains("\"status\":\"key_sent\""),
            "编辑覆盖全链：{body}"
        );
        assert_eq!(
            inner.recorded_keys(),
            vec![
                (95u32, "down".to_string()),
                (95u32, "down".to_string()),
                (95u32, "down".to_string()),
                (95u32, "down".to_string()),
                (95u32, "right".to_string()),
                (95u32, "right".to_string()),
                (95u32, "right".to_string()),
                (95u32, "backspace".to_string()),
                (95u32, "backspace".to_string()),
                (95u32, "backspace".to_string()),
                (95u32, "space".to_string()),
            ],
            "键序 = down×4 + right×3 + backspace×3 + space：{:?}",
            inner.recorded_keys()
        );
        assert_eq!(inner.recorded(), vec![(95u32, "新内容".to_string())]);
    }

    /// **清空请求端到端**（2026-10-03 回归锁）：空文本 + overwrite=true → 走清空
    /// 模式（right×3 + backspace×3 恢复占位，**零打字零数字**）→ 200 key_sent。
    /// 还原动作（变异）：删掉 handler 的 overwrite 放行 → 400、本用例先红。
    #[tokio::test]
    async fn question_free_text_clear_request_passes_handler() {
        let (state, inner, _s) = single_tool_scripted_state(
            crate::session::AgentType::Claude,
            "sess_t5mfc",
            95u32,
            vec![
                crate::inject::question::live_fixtures::q1_typed_at("旧内容", true, 29),
                crate::inject::question::live_fixtures::q1_typed_at("旧内容", true, 31),
                crate::inject::question::live_fixtures::q1_typed_at("旧内容", true, 33),
                crate::inject::question::live_fixtures::q1_typed_at("旧内容", true, 35),
                crate::inject::question::live_fixtures::q1_typed_at("旧内容", true, 37),
                crate::inject::question::live_fixtures::q1_typed_at("旧内容", true, 37),
                crate::inject::question::live_fixtures::q1_typed_at("旧内容", true, 37),
                crate::inject::question::live_fixtures::q1_typed_at("旧内容", true, 37),
                crate::inject::question::live_fixtures::q1_typed_at("旧", true, 37),
                crate::inject::question::live_fixtures::q1_typed_at("旧", true, 37),
                crate::inject::question::live_fixtures::q1_typed_at("Type something", false, 37),
            ],
        );
        mark_question(&state, "claude", "sess_t5mfc", Q_MULTI_PAYLOAD);
        let app = router(state.clone());
        let r = app
            .oneshot(req(
                "POST",
                "/m/api/v1/session-question/answer",
                Some("mam_device=mm"),
                Some(
                    r#"{"sessionId":"sess_t5mfc","action":"freeText","text":"","questionIndex":0,"overwrite":true}"#,
                ),
            ))
            .await
            .unwrap();
        assert_eq!(
            r.status(),
            200,
            "清空请求必须过 handler 放行：{}",
            body_string(r).await
        );
        let keys: Vec<(u32, String)> = inner
            .recorded_keys()
            .iter()
            .filter(|(_, k)| k == "right" || k == "backspace")
            .cloned()
            .collect();
        assert_eq!(
            keys,
            vec![
                (95u32, "right".to_string()),
                (95u32, "right".to_string()),
                (95u32, "right".to_string()),
                (95u32, "backspace".to_string()),
                (95u32, "backspace".to_string()),
                (95u32, "backspace".to_string()),
            ],
            "键序 = right×3 + backspace×3（零打字）：{keys:?}"
        );
    }

    /// **kimi 多选自由作答维持拒绝**（2026-10-02 放行面仅 claude——kimi 多选形态
    /// 未取证）：409 tool_readonly + 零投递。
    #[tokio::test]
    async fn question_free_text_still_refused_on_kimi_multi_select() {
        let (state, inner, _s) = single_tool_scripted_state(
            crate::session::AgentType::Kimi,
            "sess_t5mftk",
            95u32,
            vec![screen_fixtures::free_row()],
        );
        mark_question(&state, "kimi", "sess_t5mftk", Q_MULTI_PAYLOAD);
        let app = router(state.clone());
        let r = app
            .oneshot(req(
                "POST",
                "/m/api/v1/session-question/answer",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_t5mftk","action":"freeText","text":"hi"}"#),
            ))
            .await
            .unwrap();
        // 2026-10-06 语义变更：multiFreeText 点亮 + 门同面放行——freeText 走
        // KimiFreeText 阶段机（Other 计数位直达；本测试假体屏读 Other 行不在场
        // → 阶段机第 1 段如实中止 = 零投递，「未验不出手」由屏读把守）
        assert_eq!(r.status(), 200, "门与旗标同面（不再 409 一刀切）");
        assert!(
            inner.recorded_keys().is_empty() && inner.recorded().is_empty(),
            "假体屏读无 Other 行 → 阶段机第 1 段如实中止、零投递"
        );
    }

    /// **丁T5 端到端⑩（多选提交的工具面）→ 批次戊 E4 更新**：kimi 多选提交升格为
    /// **阶段机**（run_kimi_submit_stages：Review 在场判读 → tab → Review 汇总屏 →
    /// 屏上编号确认）。喂 claude 形态屏（无 kimi 锚）时阶段机在 review 段中止——
    /// 回执 200 failed{aborted:true, stage:review}，已发键恰为 ["tab"]（Review 不在
    /// 场的第一步），不发确认键。
    #[tokio::test]
    async fn question_submit_kimi_goes_through_stage_machine() {
        let (state, inner, _s) = single_tool_scripted_state(
            crate::session::AgentType::Kimi,
            "sess_t5k",
            94u32,
            vec![screen_fixtures::submit_focused()],
        );
        mark_question(&state, "kimi", "sess_t5k", Q_MULTI_PAYLOAD);
        let app = router(state.clone());
        let r = app
            .oneshot(req(
                "POST",
                "/m/api/v1/session-question/answer",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_t5k","action":"submit"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "kimi 提交走阶段机（非 409 只读）");
        let body = body_string(r).await;
        assert!(
            body.contains("aborted"),
            "屏无 kimi 锚 → 阶段机中止：{body}"
        );
        assert!(body.contains("review"), "中止段 = review：{body}");
        assert_eq!(
            inner.recorded_keys(),
            vec![(94u32, "tab".to_string())],
            "Review 不在场 → 先发 tab，之后中止（不发确认键）：{:?}",
            inner.recorded_keys()
        );
    }

    /// 问答应答（取消）：sess_y → 200 key_sent + (pid=35, "esc") 恰一键（探测 K3：
    /// Esc=取消/拒绝整个问题）+ 审计 answer。
    #[tokio::test]
    async fn question_answer_cancel_sends_esc() {
        let fake = FakeInjector::ok();
        let state = question_state(fake.clone());
        persist_named_device(&state, "mm", "测试设备");
        state.store.with(|conn| {
            crate::database::dao::question_wait::mark(
                conn,
                "claude",
                "sess_y",
                1_000,
                "等待回答",
                Some(Q_SINGLE_PAYLOAD),
            )
        });
        let app = router(state.clone());
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-question/answer",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_y","action":"cancel"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        assert!(body_string(r).await.contains("\"status\":\"key_sent\""));
        assert_eq!(
            fake.recorded_keys(),
            vec![(35u32, "esc".to_string())],
            "cancel = 单键 esc（探测 K3）：{:?}",
            fake.recorded_keys()
        );
        let audits = state
            .store
            .with(|c| crate::database::dao::write_audit::recent_conn(c, 10));
        assert_eq!(audits[0].action, "answer");
        assert_eq!(audits[0].summary, "cancel");
    }

    /// guard-busy 回归（F1 同款）：问答应答遇 in-flight 占用 → 200 failed{「投递进行中，
    /// 请稍后重试」} + 零注入 + 不落审计。sess_z 独占（守卫持到测尾，守卫 id 立规）。
    #[tokio::test]
    async fn question_answer_busy_inflight_returns_failed_without_key() {
        let fake = FakeInjector::ok();
        let state = question_state(fake.clone());
        persist_named_device(&state, "mm", "测试设备");
        state.store.with(|conn| {
            crate::database::dao::question_wait::mark(
                conn,
                "claude",
                "sess_z",
                1_000,
                "等待回答",
                Some(Q_SINGLE_PAYLOAD),
            )
        });
        let app = router(state.clone());
        let _busy = crate::inject::queue::try_acquire_inflight("sess_z").unwrap();
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-question/answer",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_z","action":"select","index":0}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let body = body_string(r).await;
        assert!(
            body.contains("\"status\":\"failed\"") && body.contains("投递进行中，请稍后重试"),
            "in-flight 占用时问答应 200 failed 让位：{body}"
        );
        assert!(
            fake.recorded_keys().is_empty() && fake.recorded().is_empty(),
            "占用期间不得出手任何键（零注入）"
        );
        let audits = state
            .store
            .with(|c| crate::database::dao::write_audit::recent_conn(c, 10));
        assert!(audits.is_empty(), "忙让位不落审计");
    }

    /// 多问题只读（「结论不超证据」——探测档案：questions.length>1 翻页键序未测）：
    /// sess_aa → 409 multi_questions + 零注入。GET 照常 available=true（前端按只读
    /// 卡渲染，见 QuestionCard vitest）。
    ///
    /// **丁T2 扩面**：多题只读对 **codex / kimi 同样生效**（任务书「确认这条对 kimi 也
    /// 生效即可」）——本用例在 claude 之外补两家：端点侧的 `questions.len() != 1` 判据
    /// 在**工具分发之前**（`answer_key_sequence_for` 之前），故与键序档无关；本锁把它
    /// 钉住，防未来某家升格档位时误放行多题注入。逐题注入列入下批（需实机定导航序）。
    #[tokio::test]
    async fn multi_question_readonly_covers_codex_and_kimi() {
        let fake = FakeInjector::ok();
        let state = question_state_with_msgs(
            fake.clone(),
            Box::new(|_, sid: &str, _| match sid {
                // codex：待决多题 tool-call（其后无 tool-result）
                "sess_ay" => Ok(crate::remote::content::MessagesPage {
                    messages: vec![auq_tool_call(0, Q_TWO_QUESTIONS_PAYLOAD)],
                    truncated: false,
                }),
                // kimi：同形（本机 wire 的 3 问题实录缩录为 2 题）
                "sess_az" => Ok(crate::remote::content::MessagesPage {
                    messages: vec![auq_tool_call(0, KIMI_TWO_Q_PAYLOAD)],
                    truncated: false,
                }),
                _ => Err("无消息".to_string()),
            }),
        );
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state.clone());
        // GET：两家多题 GET 照常可用（数据源不变）
        for sid in ["sess_ay", "sess_az"] {
            let r = app
                .clone()
                .oneshot(req(
                    "GET",
                    &format!("/m/api/v1/session-question?session_id={sid}"),
                    Some("mam_device=mm"),
                    None,
                ))
                .await
                .unwrap();
            let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
            assert_eq!(v["available"], true, "{sid}：多题 GET 照常可用");
            assert_eq!(v["questions"].as_array().unwrap().len(), 2);
        }
        // **批次戊 E4 更新**：kimi 多题升格为**交互**（K-5 数字直选+自动推进+Review
        // 汇总屏）——select 走 DigitAdvance 单数字键（**禁尾 Enter**，A3），200 key_sent。
        // **2026-10-09 codex 升格**：单选 select 走 CodexSelect 阶段机（数字=选中+
        // 推进原子、双投核验——旧盲发数字档退役），advance 走 CodexAdvance（h/l 底料
        // 定案）。测试缝 screen_probe=None → 入口闸「读不到屏」**零键中止**：200
        // 回执 failed+aborted（不出手纪律），不落任何键。
        // claude 多题只读由 question_answer_multi_questions_refused 继续钉住。
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-question/answer",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_az","action":"select","index":0}"#),
            ))
            .await
            .unwrap();
        assert_eq!(
            r.status(),
            200,
            "kimi 多题 select：交互放行（DigitAdvance）"
        );
        assert_eq!(
            fake.recorded_keys(),
            vec![(61u32, "1".to_string())],
            "kimi 多题 select = 单个数字（无尾随 Enter——A3 禁令锁）：{:?}",
            fake.recorded_keys()
        );
        let r = app
            .oneshot(req(
                "POST",
                "/m/api/v1/session-question/answer",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_ay","action":"select","index":0}"#),
            ))
            .await
            .unwrap();
        // codex（2026-10-09 升格后）：select 走 CodexSelect 阶段机——测试缝读不到屏，
        // 入口闸**零键中止**（200 failed+aborted；盲发数字档已退役），且不得碰 kimi
        // 已发的那个数字（断言零注入增量）
        assert_eq!(
            r.status(),
            200,
            "codex select：阶段机入口闸中止也是 200（failed+aborted 语义）"
        );
        let body = body_string(r).await;
        assert!(
            body.contains("\"status\":\"failed\"") && body.contains("\"aborted\":true"),
            "codex select 零键中止 = failed+aborted（不出手纪律）：{body}"
        );
        assert_eq!(
            fake.recorded_keys(),
            vec![(61u32, "1".to_string())],
            "kimi 多题 DigitAdvance 恰一键；codex 阶段机中止零注入"
        );
    }

    /// 多问题只读（claude 侧既有回归锁，丁T2 保留原样）
    #[tokio::test]
    async fn question_answer_multi_questions_refused() {
        let fake = FakeInjector::ok();
        let state = question_state(fake.clone());
        persist_named_device(&state, "mm", "测试设备");
        // 2026-09-24 改写：claude 已接入多题交互（toggle/advance/submit 阶段机），
        // 拒出手的载体换成**不在交互族**的 zcode——闸门语义不变（族外多题只读）
        state.store.with(|conn| {
            crate::database::dao::question_wait::mark(
                conn,
                "zcode",
                "sess_bq",
                1_000,
                "等待回答",
                Some(Q_TWO_QUESTIONS_PAYLOAD),
            )
        });
        let app = router(state.clone());
        // GET：available=true（只读展示的数据源）
        let r = app
            .clone()
            .oneshot(req(
                "GET",
                "/m/api/v1/session-question?session_id=sess_bq",
                Some("mam_device=mm"),
                None,
            ))
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["available"], true);
        assert_eq!(v["questions"].as_array().unwrap().len(), 2);
        // POST：拒绝出手
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-question/answer",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_bq","action":"select","index":0}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 409);
        assert!(body_string(r).await.contains("multi_questions"));
        assert!(fake.recorded_keys().is_empty(), "多问题零注入");
    }

    /// **能力位契约逐工具断言**（2026-10-05 推广批 T8/F6）：GET 载荷的
    /// `multiFreeText` / `screen` 按取证状态给值——**codex `freeText` 单题旗标已
    /// 点亮（2026-10-10 notes 链复活，0.162.1 四取样复验全通），multiFreeText /
    /// freeTextOverwrite 维持 false**（多题 notes 面未复采，本用例锁的就是这两个
    /// 多题面旗标）；kimi 多选关（Other 无编号，单选走旗标同面）；快照失败保守 None。
    /// **codex 旗标面 2026-10-09 扩充**：多题载荷 → advance=true + navBoth=true
    /// （0.160.0 h/l 双向环形）；单题载荷 → advance=false（无切题面）但 navBoth
    /// 仍 true（旗标与题数解耦，前端按 advance 分流渲染）。wire 的 `screen` 断言
    /// 在 `question_get_carries_codex_screen_snapshot`（本用例屏读探针无夹具 → null）。
    #[tokio::test]
    async fn question_capability_flags_by_tool() {
        let fake = FakeInjector::ok();
        let state = question_state(fake.clone());
        persist_named_device(&state, "mm", "测试设备");
        let payload = r#"{"questions":[{"header":"h","multiSelect":true,"question":"q?","options":[{"label":"a"},{"label":"b"}]}]}"#;
        for (tool, sid) in [("codex", "sess_al"), ("kimi", "sess_ao")] {
            state.store.with(|conn| {
                crate::database::dao::question_wait::mark(
                    conn,
                    tool,
                    sid,
                    1_000,
                    "等待回答",
                    Some(payload),
                )
            });
        }
        let app = router(state.clone());
        // (sid, multiFreeText, freeTextOverwrite)：覆盖写入/清空能力位逐工具断言
        // ——opencode=多选回删语义实证；kimi=2.1.1 定案 K7 重进带旧文本 + 退格可清
        // （清空面因「空回车 no-op」定案如实前置拒——前端收到失败回执引导终端操作）；
        // **codex 2026-10-09 取证回填双双关闭**：notes 链（含 Tab 清空备注的覆盖
        // 语义）不落卷，multiFreeText/freeTextOverwrite 同面收口
        for (sid, expect_mft, expect_ovw) in [("sess_al", false, false), ("sess_an", false, false)]
        {
            let r = app
                .clone()
                .oneshot(req(
                    "GET",
                    &format!("/m/api/v1/session-question?session_id={sid}"),
                    Some("mam_device=mm"),
                    None,
                ))
                .await
                .unwrap();
            let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
            assert_eq!(
                v["multiFreeText"], expect_mft,
                "{sid} multiFreeText 按取证状态"
            );
            assert_eq!(
                v["freeTextOverwrite"], expect_ovw,
                "{sid} freeTextOverwrite 按取证状态"
            );
            assert_eq!(
                v["screen"],
                serde_json::Value::Null,
                "{sid} 快照解析器留位 → null（前端维持本地状态）"
            );
        }
        // **codex 旗标面（2026-10-09，单题载荷）**：advance=false（单题无切题面）
        // + navBoth=true（h/l 双向环形旗标与题数解耦）
        let r = app
            .clone()
            .oneshot(req(
                "GET",
                "/m/api/v1/session-question?session_id=sess_al",
                Some("mam_device=mm"),
                None,
            ))
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(
            v["advance"], false,
            "codex 单题卡 advance=false（无切题面）"
        );
        assert_eq!(
            v["navBoth"], true,
            "codex navBoth=true（h/l 双向环形——切题无方向限制）"
        );
    }

    /// **codex 多题载荷旗标**（2026-10-09，0.160.0 h/l 切题底料定案）：多题 codex
    /// 会话 GET → advance=true + navBoth=true——前端渲染 ◀/▶ 双钮（navBoth 分流）
    /// 且切题动作放行（advance 门）。与 [`question_capability_flags_by_tool`] 的
    /// 单题 false 断言配对，锁住「旗标乘题数门」的两端。
    #[tokio::test]
    async fn question_codex_multi_question_flags() {
        let fake = FakeInjector::ok();
        let state = question_state(fake.clone());
        persist_named_device(&state, "mm", "测试设备");
        let payload = r#"{"questions":[{"header":"h1","multiSelect":false,"question":"q1?","options":[{"label":"a"}]},{"header":"h2","multiSelect":true,"question":"q2?","options":[{"label":"b"}]}]}"#;
        state.store.with(|conn| {
            crate::database::dao::question_wait::mark(
                conn,
                "codex",
                "sess_al",
                1_000,
                "等待回答",
                Some(payload),
            )
        });
        let app = router(state.clone());
        let r = app
            .clone()
            .oneshot(req(
                "GET",
                "/m/api/v1/session-question?session_id=sess_al",
                Some("mam_device=mm"),
                None,
            ))
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["available"], true);
        assert_eq!(v["questions"].as_array().unwrap().len(), 2);
        assert_eq!(
            v["advance"], true,
            "codex 多题卡 advance=true（h/l 切题已取证）"
        );
        assert_eq!(
            v["navBoth"], true,
            "codex 多题卡 navBoth=true（h/l 双向环形）"
        );
    }

    /// **opencode 屏读快照**（2026-10-05 推广批 F3）：opencode 会话 + 屏读探针返回
    /// 题页夹具（用户实测屏面形态）→ GET 载荷带 `screen.checked`（勾选态按屏上序）
    /// + `multiFreeText=true` 旗标；Confirm 页形态 → `screen.review=true`。
    #[tokio::test]
    async fn question_get_carries_opencode_screen_snapshot() {
        let fake = FakeInjector::ok();
        let oc_page = vec![
            "OC | 优化重点: 你想加的动效".to_string(),
            "你希望这次优化重点放在哪些方面？（可多选）".to_string(),
            "1. [ ] 画面美感与细节".to_string(),
            "2. [v] 性能与兼容性".to_string(),
            "6. [ ] Type your own answer".to_string(),
            "⇆ tab  ↑↓ select  enter toggle  esc dismiss".to_string(),
        ];
        let probe_page = oc_page.clone();
        let state = question_state_full(
            fake.clone(),
            Box::new(|_, _, _| Err("测试桩：未注入内容源".to_string())),
            std::sync::Arc::new(move |tool: &str, _pid: u32| -> Option<Vec<String>> {
                if tool == "opencode" {
                    Some(probe_page.clone())
                } else {
                    None
                }
            }),
        );
        persist_named_device(&state, "mm", "测试设备");
        let payload1 = PAYLOAD_OC_MULTI.to_string();
        state.store.with(|conn| {
            crate::database::dao::question_wait::mark(
                conn,
                "opencode",
                "sess_am",
                1_000,
                "等待回答",
                Some(payload1.as_str()),
            )
        });
        let app = router(state.clone());
        let r = app
            .clone()
            .oneshot(req(
                "GET",
                "/m/api/v1/session-question?session_id=sess_am",
                Some("mam_device=mm"),
                None,
            ))
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["available"], true);
        assert_eq!(v["multiFreeText"], true, "opencode 多选自由作答能力位");
        let screen = &v["screen"];
        assert!(screen.is_object(), "题页必须回快照：{v}");
        assert_eq!(
            screen["checked"],
            serde_json::json!([false, true, false]),
            "勾选态按屏上序"
        );
        assert_eq!(screen["freeTextPresent"], true);
        assert_eq!(
            screen["freeText"],
            serde_json::Value::Null,
            "鲜态无已存文本"
        );

        // Confirm 页形态 → review:true（前端直接进确认卡）
        let probe_confirm = vec![
            "Questions".to_string(),
            "⇆ tab  enter submit  esc dismiss".to_string(),
        ];
        let state2 = question_state_full(
            fake,
            Box::new(|_, _, _| Err("测试桩：未注入内容源".to_string())),
            std::sync::Arc::new(move |tool: &str, _pid: u32| -> Option<Vec<String>> {
                if tool == "opencode" {
                    Some(probe_confirm.clone())
                } else {
                    None
                }
            }),
        );
        persist_named_device(&state2, "mm", "测试设备");
        let payload2 = PAYLOAD_OC_MULTI.to_string();
        state2.store.with(|conn| {
            crate::database::dao::question_wait::mark(
                conn,
                "opencode",
                "sess_am",
                1_000,
                "等待回答",
                Some(payload2.as_str()),
            )
        });
        let app2 = router(state2);
        let r2 = app2
            .oneshot(req(
                "GET",
                "/m/api/v1/session-question?session_id=sess_am",
                Some("mam_device=mm"),
                None,
            ))
            .await
            .unwrap();
        let v2: serde_json::Value = serde_json::from_str(&body_string(r2).await).unwrap();
        assert_eq!(v2["screen"]["review"], true, "Confirm 页 → review 快照");
    }

    /// **codex 屏读快照**（2026-10-09，0.160.0 活体取证）：codex 会话 + 屏读探针
    /// 返回题页夹具（题号头 + footer 锚配对形态）→ GET 载荷带
    /// `screen.questionIdx/questionTotal/unanswered/isLast/options/focused`（camelCase
    /// 七键 wire 形状，与 [`CodexQuestionSnapshot`] 序列化一致——题号直读对位契约）。
    /// 夹具用 `live_fixtures::codex_s1_q2`（活体逐字取证，禁手抄改写——
    /// 屏读纪律四闸门之一）。
    #[tokio::test]
    async fn question_get_carries_codex_screen_snapshot() {
        let fake = FakeInjector::ok();
        let probe_page = crate::inject::question::live_fixtures::codex_s1_q2();
        let state = question_state_full(
            fake.clone(),
            Box::new(|_, _, _| Err("测试桩：未注入内容源".to_string())),
            std::sync::Arc::new(move |tool: &str, _pid: u32| -> Option<Vec<String>> {
                if tool == "codex" {
                    Some(probe_page.clone())
                } else {
                    None
                }
            }),
        );
        persist_named_device(&state, "mm", "测试设备");
        // 载荷两题，第 2 题题干/选项与夹具屏面对齐（fixture = S1 Q2 屏）
        let payload = r#"{"questions":[{"header":"h1","multiSelect":false,"question":"Which DB?","options":[{"label":"Postgres"},{"label":"SQLite"},{"label":"None of the above"}]},{"header":"h2","multiSelect":false,"question":"Which cache?","options":[{"label":"Redis"},{"label":"Memcached"},{"label":"None needed"},{"label":"None of the above"}]}]}"#;
        state.store.with(|conn| {
            crate::database::dao::question_wait::mark(
                conn,
                "codex",
                "sess_al",
                1_000,
                "等待回答",
                Some(payload),
            )
        });
        let app = router(state.clone());
        let r = app
            .clone()
            .oneshot(req(
                "GET",
                "/m/api/v1/session-question?session_id=sess_al",
                Some("mam_device=mm"),
                None,
            ))
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["available"], true);
        let screen = &v["screen"];
        assert!(screen.is_object(), "codex 题页必须回快照：{v}");
        // camelCase 七键形状锁（CodexQuestionSnapshot 直接 serde 序列化——
        // 不经 snapshot_to_json，独立类型独立形状）
        assert_eq!(screen["questionIdx"], 1, "题号直读（0 起）——对位主键");
        assert_eq!(screen["questionTotal"], 2);
        assert_eq!(screen["unanswered"], 1, "计数段 (1 unanswered) 直读");
        assert_eq!(screen["isLast"], true);
        assert_eq!(
            screen["options"],
            serde_json::json!(["Redis", "Memcached", "None needed", "None of the above"]),
            "选项 label 剥焦点标记/编号/(Recommended) 尾缀与描述列"
        );
        assert_eq!(screen["focused"], 0, "› 焦点行读得到 → Some(0)");
        assert_eq!(
            screen["heading"],
            serde_json::json!("Which cache?"),
            "题干也在快照里（题号对位为主、heading 供显示）"
        );
    }

    /// **claude 多题接入**（2026-09-24）：GET 旗标（multiQuestion=true + advance=true）
    /// + 多题 toggle 走闭环切勾阶段机（脚本：焦点在首选项 → 空格后已勾）。
    /// 还原动作：把 GET 闸门里的 claude 摘掉 → 前两句断言先红。
    #[tokio::test]
    async fn question_claude_multi_question_flags_and_toggle() {
        let (state, fake, _script) = stage_rig(
            vec![
                screen_fixtures::multi_option_focus(),
                screen_fixtures::multi_option_focus_checked(),
            ],
            false,
        );
        // 两题载荷（第 1 题多选——Q_TWO_QUESTIONS_PAYLOAD 的形态）
        mark_question(&state, "claude", "sess_aa", Q_TWO_Q_MULTI_FIRST_PAYLOAD);
        // Q_TWO_QUESTIONS_PAYLOAD 是 2 题；脚本屏是 3 选项多选屏——toggle#0 对得上
        // 屏上编号 1 行（切勾机按屏上编号定位，与载荷题号无关）
        let app = router(state.clone());
        let r = app
            .clone()
            .oneshot(req(
                "GET",
                "/m/api/v1/session-question?session_id=sess_aa",
                Some("mam_device=mm"),
                None,
            ))
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["multiQuestion"], true, "claude 多题 GET 放行交互旗标");
        assert_eq!(v["advance"], true, "claude 多题下发切题能力旗标");
        let r = app
            .oneshot(req(
                "POST",
                "/m/api/v1/session-question/answer",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_aa","action":"toggle","index":0,"questionIndex":0}"#),
            ))
            .await
            .unwrap();
        assert_eq!(
            r.status(),
            200,
            "claude 多题 toggle 走切勾阶段机（不再 409）"
        );
        let body = body_string(r).await;
        assert!(
            body.contains("\"status\":\"key_sent\"") && body.contains("\"checked\":true"),
            "切勾回执带屏读真值：{body}"
        );
        assert_eq!(
            fake.recorded_keys(),
            vec![(37u32, "1".to_string())],
            "键序 = [数字]（探测翻转成功 → Supported，2026-10-03）：{:?}",
            fake.recorded_keys()
        );
    }

    /// claude 多题切题（2026-10-02 ←/→ 双向导航，走位+回车退役）：多选子题发 `→`
    /// → 下一题屏（题干区变化）。断言键序 = ["right"]、回执 `advanced:true` +
    /// `direction:"next"` echo。
    /// 还原动作（变异）：导航分支改回走位+回车 → 键序断言先红。
    #[tokio::test]
    async fn question_claude_advance_walks_to_next_row() {
        let (state, fake, _script) = stage_rig(
            vec![
                crate::inject::question::live_fixtures::q1_multi(),
                crate::inject::question::live_fixtures::q2_single(),
            ],
            false,
        );
        mark_question(&state, "claude", "sess_ab", Q_TWO_Q_MULTI_FIRST_PAYLOAD);
        let app = router(state.clone());
        let r = app
            .oneshot(req(
                "POST",
                "/m/api/v1/session-question/answer",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_ab","action":"advance","direction":"next"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let body = body_string(r).await;
        assert!(
            body.contains("\"status\":\"key_sent\"")
                && body.contains("\"advanced\":true")
                && body.contains("\"direction\":\"next\""),
            "切题回执 advanced:true + direction echo：{body}"
        );
        assert_eq!(
            fake.recorded_keys(),
            vec![(38u32, "right".to_string())],
            "键序 = [→]（←/→ 通用导航，零走位）：{:?}",
            fake.recorded_keys()
        );
    }

    /// claude 多题切题·**上一题**（2026-10-02）：Review 确认屏发 `←` 退回上一题页
    ///（活体取证步骤 2 复刻）——「返回上一题修改」按钮的后端全链。
    #[tokio::test]
    async fn question_claude_advance_prev_from_review_returns() {
        let (state, fake, _script) = stage_rig(
            vec![
                crate::inject::question::live_fixtures::review_unanswered(),
                crate::inject::question::live_fixtures::q2_single(),
            ],
            false,
        );
        mark_question(&state, "claude", "sess_af", Q_TWO_Q_MULTI_FIRST_PAYLOAD);
        let app = router(state.clone());
        let r = app
            .oneshot(req(
                "POST",
                "/m/api/v1/session-question/answer",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_af","action":"advance","direction":"prev"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let body = body_string(r).await;
        assert!(
            body.contains("\"status\":\"key_sent\"")
                && body.contains("\"advanced\":true")
                && body.contains("\"direction\":\"prev\""),
            "prev 从 Review 退回：{body}"
        );
        assert_eq!(
            fake.recorded_keys(),
            vec![(42u32, "left".to_string())],
            "键序 = [←]：{:?}",
            fake.recorded_keys()
        );
    }

    /// claude 多题切题·已在 Review 屏（2026-09-24）：零按键、`advanced:false`
    /// （「返回题目」在 claude 上不可达——← 未实测，显式报告不发键乱试）。
    #[tokio::test]
    async fn question_claude_advance_on_review_is_zero_key() {
        let (state, fake, _script) = stage_rig(vec![screen_fixtures::review()], false);
        mark_question(&state, "claude", "sess_ac", Q_TWO_Q_MULTI_FIRST_PAYLOAD);
        let app = router(state.clone());
        let r = app
            .oneshot(req(
                "POST",
                "/m/api/v1/session-question/answer",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_ac","action":"advance","direction":"next"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let body = body_string(r).await;
        assert!(
            body.contains("\"advanced\":false"),
            "已在 Review 屏 → 零按键 advanced:false：{body}"
        );
        assert!(
            fake.recorded_keys().is_empty(),
            "不发任何键：{:?}",
            fake.recorded_keys()
        );
    }

    /// claude 多题切题·**单选子题**（2026-10-02 活体批次）：单选子题没有独立
    /// 推进行 → `→` 切题；末题 `→` 直达 Review 屏（活体取证步骤 1 的端点级复刻，
    /// 屏面 = live_fixtures 逐字黄金夹具）。
    /// 还原动作（变异）：单选分支改回「无推进行即中止」→ 本用例先红。
    #[tokio::test]
    async fn question_claude_advance_single_select_sends_right() {
        let (state, fake, _script) = stage_rig(
            vec![
                crate::inject::question::live_fixtures::q2_single(),
                crate::inject::question::live_fixtures::review_unanswered(),
            ],
            false,
        );
        // 守卫 id 立规：本用例独占 sess_qadv（原借 sess_ad 与 submit 用例并行串键）
        mark_question(&state, "claude", "sess_qadv", Q_TWO_Q_MULTI_FIRST_PAYLOAD);
        let app = router(state.clone());
        let r = app
            .oneshot(req(
                "POST",
                "/m/api/v1/session-question/answer",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_qadv","action":"advance","direction":"next"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let body = body_string(r).await;
        assert!(
            body.contains("\"status\":\"key_sent\"") && body.contains("\"advanced\":true"),
            "单选子题 → 直达 Review → advanced:true：{body}"
        );
        let recorded = fake.recorded_keys();
        let keys: Vec<&str> = recorded.iter().map(|(_, k)| k.as_str()).collect();
        assert_eq!(keys, vec!["right"], "键序 = [→]：{keys:?}");
    }

    /// claude 多选切勾落在**单选子题屏**（卡片与终端脱钩的实机形态）→ 身份闸
    /// 中止（stage=toggle-row、零键）——旧行块解析在这里恒 0 行，误报「读不到问答屏」。
    #[tokio::test]
    async fn question_claude_toggle_identity_abort_on_live_single_select() {
        let (state, fake, _script) = stage_rig(
            vec![crate::inject::question::live_fixtures::q2_single()],
            false,
        );
        mark_question(&state, "claude", "sess_ae", Q_TWO_Q_MULTI_FIRST_PAYLOAD);
        let app = router(state.clone());
        let r = app
            .oneshot(req(
                "POST",
                "/m/api/v1/session-question/answer",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_ae","action":"toggle","index":0,"questionIndex":0}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "中止走 200 failed 槽");
        let body = body_string(r).await;
        assert!(
            body.contains("\"failed\"") && body.contains("\"aborted\":true"),
            "身份闸中止：{body}"
        );
        assert!(
            body.contains("手机卡片不一致"),
            "中止文案点名身份核验：{body}"
        );
        assert!(
            fake.recorded_keys().is_empty(),
            "零按键：{:?}",
            fake.recorded_keys()
        );
    }

    /// claude 多题提交·已在 Review 屏（2026-09-24 第 1.5 段）：直接取屏上编号确认
    /// （零走位零回车）——多题流末题 Next 已把终端带到确认屏的形态。
    #[tokio::test]
    async fn question_claude_submit_already_on_review_confirms_directly() {
        let (state, fake, _script) = stage_rig(
            vec![screen_fixtures::review(), screen_fixtures::answered()],
            false,
        );
        // 守卫 id 立规：本用例独占 sess_qsub（原借 sess_ad 与 advance 用例并行串键）
        mark_question(&state, "claude", "sess_qsub", Q_TWO_Q_MULTI_FIRST_PAYLOAD);
        let app = router(state.clone());
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-question/answer",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_qsub","action":"submit"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let mut body = body_string(r).await;
        // INFLIGHT 守卫按裸 id 全局占用（守卫 id 立规注）：并行测试偶尔让位 →
        // 「投递进行中」是良性可重试态（2026-10-05 起连续全量命中的已知 flaky
        // 根治——仅测试侧重试，生产语义零改动）
        if body.contains("投递进行中") {
            let r = app
                .clone()
                .oneshot(req(
                    "POST",
                    "/m/api/v1/session-question/answer",
                    Some("mam_device=mm"),
                    Some(r#"{"sessionId":"sess_qsub","action":"submit"}"#),
                ))
                .await
                .unwrap();
            assert_eq!(r.status(), 200);
            body = body_string(r).await;
        }
        assert!(
            body.contains("\"status\":\"key_sent\"") && body.contains("\"verified\":true"),
            "已在 Review 屏 → 直接确认并核验终态：{body}"
        );
        assert_eq!(
            fake.recorded_keys(),
            vec![(27u32, "1".to_string())],
            "键序 = ['1']（抄屏上编号；零走位零回车）：{:?}",
            fake.recorded_keys()
        );
    }

    /// **opencode toggle 后置翻转校验**（2026-10-05 屏读标准补齐）：数字发出后
    /// 读屏核对目标行勾选翻转 → 回执 checked=Some(true)/verified=true。
    /// 屏剧本：f0（目标行未勾）→ 守卫探针翻+还原 → 数字落（目标行勾上）。
    /// 守卫 id 立规：sess_ocflip 独占。
    #[tokio::test]
    async fn opencode_toggle_flip_verified_by_screen() {
        let page = |mark: &str| {
            vec![
                format!("1. {mark} alpha"),
                "2. [ ] bravo".to_string(),
                "3. [ ] charlie".to_string(),
                "4. [ ] delta".to_string(),
                "5. [ ] echo".to_string(),
                "6. [ ] Type your own answer".to_string(),
                "⇆ tab  ↑↓ select  enter toggle  esc dismiss".to_string(),
            ]
        };
        let (state, _fake, _script) = stage_rig(
            vec![page("[ ]"), page("[✓]"), page("[ ]"), page("[✓]")],
            false,
        );
        mark_question(
            &state,
            "opencode",
            "sess_ocflip",
            r#"{"questions":[{"header":"h","multiSelect":true,"question":"q?","options":[{"label":"a"},{"label":"b"},{"label":"c"},{"label":"d"},{"label":"e"},{"label":"f"}]}]}"#,
        );
        let app = router(state.clone());
        let r = app
            .oneshot(req(
                "POST",
                "/m/api/v1/session-question/answer",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_ocflip","action":"toggle","index":0}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let body = body_string(r).await;
        assert!(
            body.contains("\"checked\":true") && body.contains("\"verified\":true"),
            "翻转校验必须实证回传：{body}"
        );
    }

    /// **opencode 切题到达验证**（2026-10-05 屏读标准补齐）：tab 前后各读一屏——
    /// ① 屏变化 = 到达 → AdvanceDone（advanced:true + 到达后快照）；
    /// ② 屏相同 = tab 未生效 → Failed「tab 未生效」可重试。
    /// 守卫 id 立规：sess_ocadv / sess_ocadv2 独占。
    #[tokio::test]
    async fn opencode_advance_arrival_verified_by_screen() {
        let q1_page = vec![
            "优化重点".to_string(),
            "你希望这次优化重点放在哪些方面？（可多选）".to_string(),
            "1. [ ] 画面美感与细节".to_string(),
            "2. [ ] 交互动效".to_string(),
            "⇆ tab  ↑↓ select  enter toggle  esc dismiss".to_string(),
        ];
        let q2_page = vec![
            "想加的动效".to_string(),
            "如果加动效，你希望加哪些？".to_string(),
            "1. [ ] 角色动态".to_string(),
            "2. [ ] 海面与云动效".to_string(),
            "⇆ tab  ↑↓ select  enter toggle  esc dismiss".to_string(),
        ];
        // ① tab 生效：pre=q1 页 → post=q2 页（屏变化）
        let (state, _fake, _script) = stage_rig(vec![q1_page.clone(), q2_page.clone()], false);
        mark_question(
            &state,
            "opencode",
            "sess_ocadv",
            r#"{"questions":[{"header":"优化重点","multiSelect":true,"question":"你希望这次优化重点放在哪些方面？","options":[{"label":"画面美感与细节"},{"label":"交互动效"}]},{"header":"想加的动效","multiSelect":true,"question":"如果加动效，你希望加哪些？","options":[{"label":"角色动态"},{"label":"海面与云动效"}]}]}"#,
        );
        let app = router(state.clone());
        let r = app
            .oneshot(req(
                "POST",
                "/m/api/v1/session-question/answer",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_ocadv","action":"advance","direction":"next"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let body = body_string(r).await;
        assert!(
            body.contains("\"advanced\":true") && body.contains("想加的动效"),
            "到达验证必须回传 advanced+到达后快照：{body}"
        );

        // ② tab 未生效：前后屏相同 → Failed
        let (state2, _fake2, _script2) = stage_rig(vec![q1_page.clone(), q1_page], false);
        mark_question(
            &state2,
            "opencode",
            "sess_ocadv2",
            r#"{"questions":[{"header":"优化重点","multiSelect":true,"question":"你希望这次优化重点放在哪些方面？","options":[{"label":"画面美感与细节"},{"label":"交互动效"}]}]}"#,
        );
        let app2 = router(state2.clone());
        let r2 = app2
            .oneshot(req(
                "POST",
                "/m/api/v1/session-question/answer",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_ocadv2","action":"advance","direction":"next"}"#),
            ))
            .await
            .unwrap();
        let body2 = body_string(r2).await;
        assert!(body2.contains("tab 未生效"), "{body2}");
    }

    /// guard 矩阵：缺 index / 域外 action / 空 sessionId → 400 bad_request；越界
    /// index 与单选 submit → 400 bad_index；无标记会话与不存在会话 → 409 no_question；
    /// 全程零注入。
    #[tokio::test]
    async fn question_answer_guards() {
        let fake = FakeInjector::ok();
        let state = question_state(fake.clone());
        persist_named_device(&state, "mm", "测试设备");
        state.store.with(|conn| {
            crate::database::dao::question_wait::mark(
                conn,
                "claude",
                "sess_ac",
                1_000,
                "等待回答",
                Some(Q_SINGLE_PAYLOAD),
            )
        });
        let app = router(state.clone());
        // select 缺 index → 400
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-question/answer",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_ac","action":"select"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 400);
        assert!(body_string(r).await.contains("bad_request"));
        // 域外 action → 400
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-question/answer",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_ac","action":"bogus","index":0}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 400);
        // 空 sessionId → 400
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-question/answer",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"","action":"cancel"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 400);
        // 越界 index（单选 3 选项取 #99）→ 400 bad_index
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-question/answer",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_ac","action":"select","index":99}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 400);
        assert!(body_string(r).await.contains("bad_index"));
        // submit 用在单选题 → 400 bad_index（单选无独立提交步）
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-question/answer",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_ac","action":"submit"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 400);
        assert!(body_string(r).await.contains("bad_index"));
        // 无标记会话（sess_af）→ 409 no_question
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-question/answer",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_af","action":"cancel"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 409);
        assert!(body_string(r).await.contains("no_question"));
        // 不存在会话 → 409 no_question（同形收敛，不给存在性预言机）
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-question/answer",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"nope","action":"cancel"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 409);
        assert!(body_string(r).await.contains("no_question"));
        assert!(fake.recorded_keys().is_empty(), "校验失败全程零注入");
    }

    /// 硬约束①（方向一）：**审批标记不得触发问答卡**——sess_ad 仅播审批标记 →
    /// 问答 GET available=false、POST answer 409 no_question、零注入；同一会话
    /// 审批 GET 照常 available（既有审批行为零回归）。
    #[tokio::test]
    async fn question_endpoints_blocked_by_approval_mark() {
        let fake = FakeInjector::ok();
        let state = question_state(fake.clone());
        persist_named_device(&state, "mm", "测试设备");
        state.store.with(|conn| {
            crate::database::dao::approval_wait::mark(conn, "claude", "sess_ad", 1_000, "等待审批")
        });
        let app = router(state.clone());
        // 问答 GET：不可用
        let r = app
            .clone()
            .oneshot(req(
                "GET",
                "/m/api/v1/session-question?session_id=sess_ad",
                Some("mam_device=mm"),
                None,
            ))
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(
            v["available"], false,
            "审批标记在场时问答卡不可用（硬约束①）"
        );
        // 问答 POST：409 no_question + 零注入
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-question/answer",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_ad","action":"cancel"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 409);
        assert!(body_string(r).await.contains("no_question"));
        assert!(fake.recorded_keys().is_empty());
        // 同会话审批 GET：available=true（审批行为零回归——标记路径跳过 detect）
        let r = app
            .oneshot(req(
                "GET",
                "/m/api/v1/session-approve-options?session_id=sess_ad",
                Some("mam_device=mm"),
                None,
            ))
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["available"], true, "审批标记路径零回归");
    }

    /// 硬约束①（方向二）：**问题标记不得触发审批红卡**——sess_ae 播问题标记且
    /// last_message 恰为审批 marker 命中句（最强隔离形态：不依赖 detect 未达）→
    /// 审批 GET available=false、审批 POST 409 not_waiting、零审批键；问答 GET
    /// 照常 available（问答行为零回归）。
    #[tokio::test]
    async fn question_mark_never_triggers_approve_card() {
        let fake = FakeInjector::ok();
        let state = question_state(fake.clone());
        persist_named_device(&state, "mm", "测试设备");
        state.store.with(|conn| {
            crate::database::dao::question_wait::mark(
                conn,
                "claude",
                "sess_ae",
                1_000,
                "等待回答",
                Some(Q_SINGLE_PAYLOAD),
            )
        });
        let app = router(state.clone());
        // 审批 GET：available=false（问题标记显式排除——Waiting 门会被叠加层满足，
        // 必须由问题标记检查拦下）
        let r = app
            .clone()
            .oneshot(req(
                "GET",
                "/m/api/v1/session-approve-options?session_id=sess_ae",
                Some("mam_device=mm"),
                None,
            ))
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(
            v["available"], false,
            "问题标记在场时审批卡不可用（硬约束①；detect 命中被显式压过）"
        );
        assert!(v["options"].as_array().unwrap().is_empty());
        // 审批 POST：409 not_waiting + 零审批键
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-approve",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_ae","optionId":"approve"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 409);
        assert!(body_string(r).await.contains("not_waiting"));
        assert!(fake.recorded_keys().is_empty(), "审批键零出手");
        // 问答 GET：available=true（问答行为零回归）
        let r = app
            .oneshot(req(
                "GET",
                "/m/api/v1/session-question?session_id=sess_ae",
                Some("mam_device=mm"),
                None,
            ))
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["available"], true);
    }

    /// **丁T1 回归锁（本批引入的新矛盾，任务书硬要求）**：codex / opencode 的问题
    /// 待决被状态链推出**语义红 Waiting** 之后，审批卡**不得**借机误出。
    ///
    /// 与既有 `question_mark_never_triggers_approve_card`（问题**标记** + detect 命中
    /// 的隔离）互补：本用例是**无标记 + detect 未命中**形态（问题标记只有 claude
    /// 钩子路径会写，codex/opencode 无钩子通道）——会话靠状态链的语义红满足 approve 的
    /// Waiting 门，此时拦截来自 detect 门（问题文本不含审批 marker → hit=false）。
    /// 锁住的正是「新红灯不会把审批卡带出来」这条边。
    ///
    /// **detect 命中形态另有用例**：`question_pending_red_beats_approval_marker_text`
    /// （丁T1 复评 F-2，模型把问题写成审批措辞时 detect 会命中，靠尾部待决问答判定拦）。
    ///
    /// 断言（GET = 卡的数据源，**「approve 不可用」的判定面**）：available=false
    /// （detect miss）+ options 空 + reason null（自隐契约，与严格档提示条区分）。
    ///
    /// **POST 面如实申报**：`session-approve` POST **不走 detect**（既有契约——
    /// 客户端只 POST 它从 GET 拿到的 option id，GET 已把 detect 门走过；见
    /// `audit_action_vocab`「POST 不走 detect，无需 last_message」与 F2 用例）。
    /// 因此 POST 对本用例的两会话仍会按映射出键（codex 有默认映射）——这是 **T1 之前
    /// 就存在**的契约面，不在本任务改动范围内（改它会让既有端点契约测试全红）。
    /// 用户可见面已由 GET 的 available=false 关死：前端 ApproveCard 只在 available=true
    /// 时渲染按钮，无按钮即无 POST 入口。残余面（旁路客户端直接 POST）记录在案。
    #[tokio::test]
    async fn question_pending_red_never_triggers_approve_card() {
        let fake = FakeInjector::ok();
        let state = question_state(fake.clone());
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state.clone());
        for sid in ["sess_al", "sess_am"] {
            // 审批 GET：不可用（问题文本不含审批 marker → detect miss）
            let r = app
                .clone()
                .oneshot(req(
                    "GET",
                    &format!("/m/api/v1/session-approve-options?session_id={sid}"),
                    Some("mam_device=mm"),
                    None,
                ))
                .await
                .unwrap();
            assert_eq!(r.status(), 200);
            let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
            assert_eq!(
                v["available"], false,
                "{sid}：问答待决的语义红不得触发审批卡（detect 门必须护住）"
            );
            assert!(
                v["options"].as_array().unwrap().is_empty(),
                "{sid}：不可用一律不下发选项"
            );
            assert_eq!(
                v["reason"],
                serde_json::Value::Null,
                "{sid}：非严格档不可批不得带 reason（ApproveCard 自隐契约）"
            );
        }
        assert!(fake.recorded_keys().is_empty(), "本用例不触任何注入路径");
        // 问答 GET：照常可用（问答行为零回归）
        let r = app
            .oneshot(req(
                "GET",
                "/m/api/v1/session-question?session_id=sess_u",
                Some("mam_device=mm"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
    }

    /// **丁T1 复评 F-2 主用例**：「问答在场 → 审批不可用」在 **detect 会命中**时也必须
    /// 成立（最严形态，评审 I-3）。
    ///
    /// 场景：模型把问题写成审批措辞（"Would you like to run the following command?"，
    /// 完全自然的问法）——`detect` 是**纯文本子串**命中，此时 `hit` 本会为 true，
    /// 审批卡借语义红误出。拦截只能来自「尾部存在待决问答」判定
    /// （`pending_question_tail_index`，与问答端点同一份判据）。
    ///
    /// 夹具构造：message_source 对 sess_an / sess_ao 返回「问答形态 tool-call 且其后
    /// 无 tool-result」的消息页（= 待决问答在场）。
    /// 断言：GET available=false + options 空 + reason null（与 detect-miss 形态同收敛，
    /// 不给存在性预言机）；问答 GET 照常 available=true（问答零回归）。
    #[tokio::test]
    async fn question_pending_red_beats_approval_marker_text() {
        let fake = FakeInjector::ok();
        // 待决问答页（AUQ 形态 tool-call，其后无 tool-result）
        let page = crate::remote::content::MessagesPage {
            messages: vec![user_msg(0), auq_tool_call(1, Q_SINGLE_PAYLOAD)],
            truncated: false,
        };
        let state = question_state_with_msgs(
            fake.clone(),
            Box::new(move |_, sid: &str, _| {
                if sid == "sess_an" || sid == "sess_ao" {
                    Ok(page.clone())
                } else {
                    Err("无消息".to_string())
                }
            }),
        );
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state.clone());
        for sid in ["sess_an", "sess_ao"] {
            // 审批 GET：detect 本会命中（last_message 是 codex marker 原文），但尾部
            // 待决问答 → 硬约束①（无标记分支）压为不可用
            let r = app
                .clone()
                .oneshot(req(
                    "GET",
                    &format!("/m/api/v1/session-approve-options?session_id={sid}"),
                    Some("mam_device=mm"),
                    None,
                ))
                .await
                .unwrap();
            assert_eq!(r.status(), 200);
            let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
            assert_eq!(
                v["available"], false,
                "{sid}：尾部待决问答在场时审批必须不可用（即使 last_message 命中审批 marker）"
            );
            assert!(
                v["options"].as_array().unwrap().is_empty(),
                "{sid}：不可用一律不下发选项"
            );
            assert_eq!(
                v["reason"],
                serde_json::Value::Null,
                "{sid}：硬约束① 的不可用不给 reason（与严格档提示条区分）"
            );
            // 问答 GET：照常可用（问答行为零回归；同一份判据在两处口径一致）
            let r = app
                .clone()
                .oneshot(req(
                    "GET",
                    &format!("/m/api/v1/session-question?session_id={sid}"),
                    Some("mam_device=mm"),
                    None,
                ))
                .await
                .unwrap();
            let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
            assert_eq!(v["available"], true, "{sid}：问答照常可用");
            assert_eq!(v["source"], "scan", "{sid}：通道 B 识别口径");
        }
        assert!(fake.recorded_keys().is_empty(), "本用例不触任何注入路径");
    }

    /// F-2 反向锁（防收窄过头）：**已答**的问答（tool-call 后随 tool-result）不再算
    /// 「问答在场」——此时 detect 命中的真审批会话必须照常可用（否则新判定会把真
    /// 审批卡压死）。夹具复用 sess_an 但消息页换成「tool-call + tool-result」。
    #[tokio::test]
    async fn answered_question_tail_does_not_block_approve() {
        let fake = FakeInjector::ok();
        let page = crate::remote::content::MessagesPage {
            messages: vec![
                auq_tool_call(0, Q_SINGLE_PAYLOAD),
                tool_result_msg(1, "Your questions have been answered: \"q\"=\"Tool demo\"."),
            ],
            truncated: false,
        };
        let state = question_state_with_msgs(
            fake.clone(),
            Box::new(move |_, sid: &str, _| {
                if sid == "sess_an" {
                    Ok(page.clone())
                } else {
                    Err("无消息".to_string())
                }
            }),
        );
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state.clone());
        // sess_an：Waiting + last_message 命中 codex marker + 已答问答尾部 → 审批照常可用
        let r = app
            .clone()
            .oneshot(req(
                "GET",
                "/m/api/v1/session-approve-options?session_id=sess_an",
                Some("mam_device=mm"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(
            v["available"], true,
            "已答问答不再构成「问答在场」——真审批会话照常可用（零回归）"
        );
        assert!(
            !v["options"].as_array().unwrap().is_empty(),
            "可批时下发选项"
        );
        // 问答 GET：已答 → 不可用（问答端点既有口径零回归）
        let r = app
            .oneshot(req(
                "GET",
                "/m/api/v1/session-question?session_id=sess_an",
                Some("mam_device=mm"),
                None,
            ))
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["available"], false, "已答 → 问答不可用（既有口径）");
    }

    /// **丁T1 复评 F2-2 反锁**：新门按工具收窄——claude **不**受尾部判据压制。
    ///
    /// 场景（评审 Important-2 的假阳性形态，取自真实库 `0c41365d-…`）：claude 的
    /// AUQ 被用户用**纯文本**作答，`tool_result` 永不落盘 → 消息尾部看起来像
    /// 「待决问答」，实则早已不在场。若新门对 claude 也生效，其后落在 40 条窗口内的
    /// **真审批**会被静默压成 `available=false, reason=null`（前端自隐：用户既看不到
    /// 按钮也看不到提示）——本用例锁住「claude 不受该门影响」。
    ///
    /// 夹具：sess_ar（claude，Waiting）+ last_message = claude 审批 marker 原文 +
    /// message_source 返回「AUQ tool-call 无 tool-result」的尾部。
    /// 断言：审批 GET **available=true**（照常出选项），问答 GET 仍走既有通道 B。
    #[tokio::test]
    async fn claude_tail_question_does_not_suppress_approve() {
        let fake = FakeInjector::ok();
        let page = crate::remote::content::MessagesPage {
            messages: vec![auq_tool_call(0, Q_SINGLE_PAYLOAD)],
            truncated: false,
        };
        let state = question_state_with_msgs(
            fake.clone(),
            Box::new(move |_, sid: &str, _| {
                if sid == "sess_ar" {
                    Ok(page.clone())
                } else {
                    Err("无消息".to_string())
                }
            }),
        );
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state.clone());
        let r = app
            .clone()
            .oneshot(req(
                "GET",
                "/m/api/v1/session-approve-options?session_id=sess_ar",
                Some("mam_device=mm"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(
            v["available"], true,
            "claude 走既有 question_marked 隔离路径：尾部 AUQ 形态（纯文本作答、无 result）\
             不得把真审批压成不可用（F2-2 收窄）"
        );
        assert!(
            !v["options"].as_array().unwrap().is_empty(),
            "claude 可批时必须下发选项（不被静默压制）"
        );
    }

    /// 通道 B（兜底）：sess_ai（Processing、无标记）message_source 注入「user 消息 +
    /// 已落盘 AUQ tool-call（无 tool-result）」→ GET available=true source="scan" +
    /// questions 可解析；POST select → (pid=44, "1") 出键。
    #[tokio::test]
    async fn question_channel_b_scan_available_and_answerable() {
        let fake = FakeInjector::ok();
        let page = crate::remote::content::MessagesPage {
            messages: vec![user_msg(0), auq_tool_call(1, Q_SINGLE_PAYLOAD)],
            truncated: false,
        };
        // 2026-10-03：select 走焦点守卫编排 → 需要屏读缝（焦点在选项行 → 守卫不触发）
        let probe_screen = screen_fixtures::multi_option_focus();
        let state = question_state_full(
            fake.clone(),
            Box::new(move |_, sid: &str, _| {
                if sid == "sess_ai" {
                    Ok(page.clone())
                } else {
                    Err("无消息".to_string())
                }
            }),
            std::sync::Arc::new(move |_, _| Some(probe_screen.clone())),
        );
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state.clone());
        let r = app
            .clone()
            .oneshot(req(
                "GET",
                "/m/api/v1/session-question?session_id=sess_ai",
                Some("mam_device=mm"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["available"], true, "未答 AUQ tool-call 在场 → 可用");
        assert_eq!(v["source"], "scan", "通道 B 识别口径");
        assert_eq!(v["questions"][0]["header"], "Next step");
        // POST select：通道 B 会话照常出手
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-question/answer",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_ai","action":"select","index":0}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        assert!(body_string(r).await.contains("\"status\":\"key_sent\""));
        assert_eq!(
            fake.recorded_keys(),
            vec![(44u32, "1".to_string())],
            "通道 B select 出键：{:?}",
            fake.recorded_keys()
        );
    }

    /// 通道 B 答完判据（可行口径）：tool-call 之后任何 tool-result 在场 → 不再可用。
    /// sess_aj 消息 = AUQ tool-call + tool_result → GET available=false、POST 409。
    #[tokio::test]
    async fn question_channel_b_not_available_after_tool_result() {
        let fake = FakeInjector::ok();
        let page = crate::remote::content::MessagesPage {
            messages: vec![
                auq_tool_call(0, Q_SINGLE_PAYLOAD),
                tool_result_msg(1, "Your questions have been answered: \"q\"=\"Tool demo\"."),
            ],
            truncated: false,
        };
        let state = question_state_with_msgs(
            fake.clone(),
            Box::new(move |_, sid: &str, _| {
                if sid == "sess_aj" {
                    Ok(page.clone())
                } else {
                    Err("无消息".to_string())
                }
            }),
        );
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state.clone());
        let r = app
            .clone()
            .oneshot(req(
                "GET",
                "/m/api/v1/session-question?session_id=sess_aj",
                Some("mam_device=mm"),
                None,
            ))
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(
            v["available"], false,
            "tool_result 在场 = 已答，不再可用（可行口径：任何后随 tool-result 即判答完）"
        );
        let r = app
            .oneshot(req(
                "POST",
                "/m/api/v1/session-question/answer",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_aj","action":"cancel"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 409);
        assert!(body_string(r).await.contains("no_question"));
        assert!(fake.recorded_keys().is_empty());
    }

    // ==== 丁T2：计划双卡 / 计划待确认 / kimi 审批互斥（问题 3/4/10/12）====
    // 夹具族：在 question_state_with_msgs 的会话清单上再加 sess_as..sess_ax（全测试集
    // 唯一 id，守卫 id 立规）。message_source 按 sid 分派返回各用例的消息页。

    /// 计划类消息条目（`kind="plan"`——claude/codex/kimi 三家的正文卡在消息层同构）
    fn plan_msg(seq: i64, content: &str) -> crate::remote::content::SessionMessage {
        crate::remote::content::SessionMessage {
            seq,
            role: "assistant".into(),
            kind: "plan".into(),
            content: content.into(),
            ts: None,
            tool_name: None,
            tool_args: None,
            collapsed: false,
        }
    }

    /// 计划文件卡条目（`kind="plan-file"`，content=路径）
    fn plan_file_msg(seq: i64, path: &str) -> crate::remote::content::SessionMessage {
        crate::remote::content::SessionMessage {
            seq,
            role: "assistant".into(),
            kind: "plan-file".into(),
            content: path.into(),
            ts: None,
            tool_name: None,
            tool_args: None,
            collapsed: false,
        }
    }

    /// codex 实机消息尾（17:19 rollout 缩录）：<proposed_plan> 升格而成的 plan 卡之后
    /// **无用户消息、无工具事件** = 计划待确认预期态在场。
    fn codex_plan_pending_page() -> crate::remote::content::MessagesPage {
        crate::remote::content::MessagesPage {
            messages: vec![
                user_msg(0),
                plan_msg(1, "# 《末班车》情感救赎版改写方案\n\n## Summary\n改写重点…"),
            ],
            truncated: false,
        }
    }

    /// 计划之后有用户消息（"Implement the plan." 实机形态）→ 预期态清除
    fn codex_plan_consumed_page() -> crate::remote::content::MessagesPage {
        crate::remote::content::MessagesPage {
            messages: vec![user_msg(0), plan_msg(1, "# 计划"), user_msg(2), user_msg(3)],
            truncated: false,
        }
    }

    /// 计划之后有工具事件（kimi 批准后 wire 立即落 ExitPlanMode 的 tool.call/result）
    fn plan_then_tool_event_page() -> crate::remote::content::MessagesPage {
        crate::remote::content::MessagesPage {
            messages: vec![
                user_msg(0),
                plan_msg(1, "# 计划"),
                plan_file_msg(2, "C:/u/.kimi-code/sessions/s/agents/main/plans/a.md"),
                crate::remote::content::SessionMessage {
                    seq: 3,
                    role: "assistant".into(),
                    kind: "tool-call".into(),
                    content: "ExitPlanMode".into(),
                    ts: None,
                    tool_name: Some("ExitPlanMode".into()),
                    tool_args: Some("{}".into()),
                    collapsed: true,
                },
                tool_result_msg(4, "Exited plan mode."),
            ],
            truncated: false,
        }
    }

    /// codex 计划待确认全链（问题 4）：**非 Waiting**（Processing，实机 codex 不落红）
    /// + 尾部计划提案 → 审批端点门放宽到「计划预期态」：
    /// - GET available=true、`planPending=true`、options 空（y/esc 是补丁审批键位，对
    ///   计划框未取证——不得下发）、plan 正文聚合（T8 机制，前端渲染计划全文）；
    /// - 计划之后有用户消息 → 预期态消失 → available=false（任务书「下一个用户消息
    ///   注入即清除」，无新存储）。
    #[tokio::test]
    async fn codex_plan_pending_opens_approve_gate() {
        let fake = FakeInjector::ok();
        let state = question_state_with_msgs(
            fake.clone(),
            Box::new(|_, sid: &str, _| match sid {
                "sess_as" => Ok(codex_plan_pending_page()),
                "sess_at" => Ok(codex_plan_consumed_page()),
                _ => Err("无消息".to_string()),
            }),
        );
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state.clone());
        // sess_as：Processing + 尾部计划提案 → 门放宽
        let r = app
            .clone()
            .oneshot(req(
                "GET",
                "/m/api/v1/session-approve-options?session_id=sess_as",
                Some("mam_device=mm"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(
            v["available"], true,
            "计划预期态必须打开审批端点门（否则详情页无从「检查终端对话框」）"
        );
        assert_eq!(
            v["planPending"], true,
            "预期态随载荷下发（前端提示条的数据源）"
        );
        assert!(
            v["options"].as_array().unwrap().is_empty(),
            "计划框未读到屏读选项时不得下发映射表键位（y/esc 是补丁审批键，未取证）"
        );
        assert_eq!(
            v["plan"]["content"], "# 《末班车》情感救赎版改写方案\n\n## Summary\n改写重点…",
            "计划正文聚合（T8 机制——点检查前用户先看到计划全文）"
        );
        assert_eq!(v["plan"]["isFile"], false);
        // sess_at：计划之后有用户消息 → 预期态清除
        let r = app
            .clone()
            .oneshot(req(
                "GET",
                "/m/api/v1/session-approve-options?session_id=sess_at",
                Some("mam_device=mm"),
                None,
            ))
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(
            v["available"], false,
            "计划后已有用户消息（Implement the plan.）→ 预期态清除（无新存储，尾部派生）"
        );
        assert_eq!(v["planPending"], false);
        assert!(fake.recorded_keys().is_empty(), "本用例不触任何注入路径");
    }

    /// kimi 计划审批卡（问题 3 主体）：
    /// - Waiting（实机 `interaction.request → Waiting` 红灯）+ 尾部 plan 卡（正文+文件卡）
    ///   → 门过、`planPending=true`；
    /// - **Windows 屏读拿到选项时**走 `dialog:<n>` N 选项（NavigateConfirm 键序）；
    /// - **屏读不到选项时**（CI / 非 Windows / 对话框未绘制）→ kimi 专属两态收敛：
    ///   **有计划预期态 → `available=true` + 零 options + `planPending=true`**（前端
    ///   「计划待确认」条 +「检查终端对话框」按钮——与 codex 同形态）；
    ///   无预期态（带标记的 Write/command 审批）→ `available=false` + reason 提示条。
    ///   **两态都绝不下发映射表键位**（R1-1 证伪 kimi 数字通道，默认表 options 恒空）。
    /// - **计划之后有工具事件**（批准后 wire 落 ExitPlanMode tool.call/result）→ 预期态
    ///   清除 → available=false（否则批准后卡片一直挂着）。
    ///
    /// 屏读依赖真实窗口（本测试进程无 conhost 目标）——故分支断言「屏读命中 vs 未命中」
    /// 两种合法形态，两者都必须满足「零映射键」这条硬约束。
    #[tokio::test]
    async fn kimi_plan_approval_card_never_emits_mapping_keys() {
        let fake = FakeInjector::ok();
        let state = question_state_with_msgs(
            fake.clone(),
            Box::new(|_, sid: &str, _| match sid {
                "sess_au" => Ok(crate::remote::content::MessagesPage {
                    messages: vec![
                        user_msg(0),
                        plan_msg(1, "# Plan: Create hi.txt\n\n## Goal\nCreate `hi.txt`."),
                        plan_file_msg(
                            2,
                            "C:/u/.kimi-code/sessions/wd_x/session_y/agents/main/plans/p.md",
                        ),
                    ],
                    truncated: false,
                }),
                "sess_av" => Ok(plan_then_tool_event_page()),
                _ => Err("无消息".to_string()),
            }),
        );
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state.clone());
        let r = app
            .clone()
            .oneshot(req(
                "GET",
                "/m/api/v1/session-approve-options?session_id=sess_au",
                Some("mam_device=mm"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(
            v["verifiedWith"], KIMI_NEVER_TESTED,
            "kimi 映射已入表（版本随实机）"
        );
        assert_eq!(v["planPending"], true, "尾部署名计划 ⇒ 预期态在场");
        // F3-4：**kimi 审批卡一律不带 plan 正文**（任务书成文要求，两种形态都成立）
        assert_eq!(
            v["plan"],
            serde_json::Value::Null,
            "kimi 审批卡不得下发 plan 正文（F3-4；正文归 §2.2 的 kimi 正文卡）"
        );
        let options = v["options"].as_array().unwrap();
        if v["dialog"] == true {
            // 形态一：屏读命中（有真实窗口的机器）——选项全是 dialog:<n>
            assert!(!options.is_empty(), "dialog 模式下选项非空");
            for o in options {
                assert!(
                    o["id"].as_str().unwrap().starts_with("dialog:"),
                    "选项只能来自屏读（dialog:<n>）：{o}"
                );
            }
        } else {
            // 形态二：屏读未命中（CI / 非 Windows / 对话框未绘制）——「计划待确认」条：
            // available=true（这是**可操作**的卡：点检查重试）+ 零 options（无键可发）。
            // available=false + reason 是**无计划预期态**的降级
            // （见 kimi_approval_without_plan_falls_back_to_hint），两态在此分界。
            assert!(
                options.is_empty(),
                "kimi 屏读失败不得下发任何映射键位（R1-1 数字通道不可依赖）"
            );
            assert!(
                v["reason"].is_null(),
                "计划待确认条不走严格档 reason 通道（reason 是 available=false 的）"
            );
        }
        // sess_av：计划之后有工具事件 → 预期态清除（批准后卡片不得挂死）
        let r = app
            .oneshot(req(
                "GET",
                "/m/api/v1/session-approve-options?session_id=sess_av",
                Some("mam_device=mm"),
                None,
            ))
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(
            v["planPending"], false,
            "计划之后已有工具事件（批准后 wire 落 ExitPlanMode 回执）→ 预期态清除"
        );
        assert!(fake.recorded_keys().is_empty(), "本用例不触任何注入路径");
    }

    /// kimi 审批的**另一态**：命中审批但**无计划预期态**（带标记的 Write/command 审批
    /// ——R1-3 实测 kimi 有 `▶ Write this file?` 四选项工具批准框）→ 屏读不到选项时
    /// 不给「计划待确认」条（那不是计划），走**降级提示条**（available=false + reason）：
    /// 无键可发（R1-1），指引去终端（§2.8）。
    ///
    /// 夹具：sess_ba = kimi Waiting + **审批等待标记**（走标记路径跳过 detect）+ 消息页
    /// 无计划（历史工具事件）→ 预期态不成立。
    #[tokio::test]
    async fn kimi_approval_without_plan_falls_back_to_hint() {
        let fake = FakeInjector::ok();
        let state = question_state_with_msgs(
            fake.clone(),
            Box::new(|_, sid: &str, _| match sid {
                // 无计划类消息（纯工具往返）→ 预期态不成立
                "sess_ba" => Ok(crate::remote::content::MessagesPage {
                    messages: vec![
                        user_msg(0),
                        crate::remote::content::SessionMessage {
                            seq: 1,
                            role: "assistant".into(),
                            kind: "tool-call".into(),
                            content: "Write".into(),
                            ts: None,
                            tool_name: Some("Write".into()),
                            tool_args: Some(r#"{"path":"hi.txt"}"#.into()),
                            collapsed: true,
                        },
                    ],
                    truncated: false,
                }),
                _ => Err("无消息".to_string()),
            }),
        );
        persist_named_device(&state, "mm", "测试设备");
        state.store.with(|conn| {
            crate::database::dao::approval_wait::mark(conn, "kimi", "sess_ba", 1_000, "工具审批")
        });
        let app = router(state.clone());
        let r = app
            .oneshot(req(
                "GET",
                "/m/api/v1/session-approve-options?session_id=sess_ba",
                Some("mam_device=mm"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        if v["available"] == true {
            // 屏读命中（真实窗口机器）：dialog 选项齐即可（本用例只锁「无计划不误导」）
            assert_eq!(v["dialog"], true);
            assert_eq!(v["planPending"], false, "无计划消息 → 预期态不成立");
            assert!(v["plan"].is_null(), "无计划消息 → 不下发 plan 主体");
        } else {
            assert_eq!(
                v["planPending"], false,
                "无计划预期态 → 不得渲染「计划待确认」条（那不是计划）"
            );
            assert!(
                v["reason"].as_str().is_some_and(|s| !s.is_empty()),
                "无计划预期态的降级必须给中文提示条（available=false + reason）"
            );
            assert!(
                v["options"].as_array().unwrap().is_empty(),
                "无键可发（R1-1）——提示条形态零选项"
            );
        }
        assert!(fake.recorded_keys().is_empty());
    }

    // ==== 丁T2 复评修复（F3-2/F3-3/F3-4/F3-5）====

    /// **F3-2（Critical）**：`POST /session-approve` 的门必须与 GET **同口径**——
    /// codex 计划待确认的真机状态是 **Idle + 无标记**，只放宽 GET 会让卡片挂上后
    /// **点 N 选项必回 409 not_waiting**（用户点了等于没点）。
    ///
    /// 夹具：sess_bb = codex **Idle** + 尾部计划提案（真机形态）。
    /// 断言：GET available=true + planPending；POST `dialog:1` 直达投递（200 key_sent
    /// + 注入器收到 VK 数字 '1'——本测试进程无真实窗口，屏读恒失败，故用**映射表键位
    /// 不可用**的 codex 计划形态……见下注）。
    ///
    /// **注（为什么断言「投递被尝试」而非具体键）**：计划框的真实交互依赖屏读
    /// （Windows 可见窗口），CI 无窗口 → `dialog_options=None` → 选项为空；故本用例
    /// 断言的是 **POST 越过了 not_waiting 门**（错误码不再是 not_waiting，而是走到
    /// 映射/选项层），这才是 F3-2 的缺陷面。键序列投递的成功路径由既有
    /// `approve_sends_key`（Waiting 会话）与 `dialog:<n>` 用例覆盖。
    #[tokio::test]
    async fn codex_plan_pending_opens_approve_post_gate() {
        let fake = FakeInjector::ok();
        let state = question_state_with_msgs(
            fake.clone(),
            Box::new(|_, sid: &str, _| match sid {
                "sess_bb" => Ok(codex_plan_pending_page()),
                "sess_bc" => Ok(codex_plan_consumed_page()),
                _ => Err("无消息".to_string()),
            }),
        );
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state.clone());
        // 前置：GET 可用（与既有 codex 用例一致）
        let r = app
            .clone()
            .oneshot(req(
                "GET",
                "/m/api/v1/session-approve-options?session_id=sess_bb",
                Some("mam_device=mm"),
                None,
            ))
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["available"], true, "GET 门（既有）");
        // **F3-2 核心断言 + N1 收口**：POST 映射表 id → 必须**越过 not_waiting 门**
        // （否则 F3-2 未修），但被 N1 拒为 no_mapping（404）——两个错误码的差异正是
        // 「门已放宽 ∧ 输入面已收窄」的判据：not_waiting 来自门之前，no_mapping 来自
        // 门之后。**零注入**是 N1 的核心（修前该组合注入 `y`）。
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-approve",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_bb","optionId":"approve"}"#),
            ))
            .await
            .unwrap();
        let status = r.status();
        let body = body_string(r).await;
        assert!(
            !body.contains("not_waiting"),
            "F3-2：计划预期态下 POST 必须越过 not_waiting 门（实得 {body}）"
        );
        assert_eq!(
            status, 404,
            "N1：越过门后映射表 id 必须被拒（no_mapping；修前是 200 key_sent + 注入 y）：{body}"
        );
        assert!(body.contains("no_mapping"), "{body}");
        assert!(
            fake.recorded_keys().is_empty(),
            "N1：计划预期态下映射键位零注入（修前实测注入 (63,\"y\")）：{:?}",
            fake.recorded_keys()
        );
        // 反向锁：尾无计划（计划已被用户消息消费）→ POST 照旧 409 not_waiting
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-approve",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_bc","optionId":"approve"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(
            r.status(),
            409,
            "反向锁：codex 的 Idle 本身不等于放行——尾部无可确认计划时 POST 仍须 409"
        );
        assert!(body_string(r).await.contains("not_waiting"));
        assert!(
            fake.recorded_keys().is_empty(),
            "反向锁：全用例零注入（409 与 404 两条路径都不出手）"
        );
    }

    /// **N1（复审 Important）**：计划预期态下 POST **只放行 `dialog:<n>`**。
    ///
    /// 端点上两种拒绝都收敛成 `no_mapping`，故本用例锁的是**「映射 id 被拒 + 零注入」**
    /// 这一半（可观测面）；另一半「`dialog:<n>` 仍被接受」由纯函数单测
    /// `plan_pending_post_accepts_dialog_options_only` 锁住——**CI 无法观测 dialog 的
    /// 通过路径**（屏读依赖真实可见窗口：非 Windows 恒 None，Windows 上假 pid 的
    /// AttachConsole 必失败），如实申报。
    ///
    /// 对照面（kimi 同锁）：kimi 的计划审批同样在预期态下——其映射表 options 本就为空，
    /// 但 KV 定制可能给出非空表，故 N1 的收敛点在 kimi 上同样成立（本用例用 codex，
    /// 因为它的默认表**有** `approve="y"`，正是插桩实测命中的键）。
    #[tokio::test]
    async fn plan_pending_post_rejects_mapped_option_ids() {
        let fake = FakeInjector::ok();
        let state = question_state_with_msgs(
            fake.clone(),
            Box::new(|_, sid: &str, _| match sid {
                "sess_bg" => Ok(codex_plan_pending_page()),
                _ => Err("无消息".to_string()),
            }),
        );
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state.clone());
        // 逐一遍历默认表全部 id + 若干域外 id：预期态下一律 no_mapping + 零注入
        for option_id in [
            "approve",
            "reject",
            "bogus",
            "y",
            "esc",
            "dialog",
            "dialogx:1",
        ] {
            let r = app
                .clone()
                .oneshot(req(
                    "POST",
                    "/m/api/v1/session-approve",
                    Some("mam_device=mm"),
                    Some(&format!(
                        r#"{{"sessionId":"sess_bg","optionId":"{option_id}"}}"#
                    )),
                ))
                .await
                .unwrap();
            assert_eq!(r.status(), 404, "{option_id}：预期态必须拒为 no_mapping");
            assert!(body_string(r).await.contains("no_mapping"), "{option_id}");
        }
        assert!(
            fake.recorded_keys().is_empty(),
            "N1：预期态下任何映射 id 都零注入：{:?}",
            fake.recorded_keys()
        );
    }

    /// **N1 反向锁**：`Waiting` 态的**既有映射键路径零回归**——同一端点、同一会话族，
    /// 状态换成 Waiting（或带审批标记）时映射表 id 照常投递。
    ///
    /// 用 sess_bh（claude Waiting + 审批标记）——它的映射表
    /// `approve="1"` 是 M8R 实证键位，POST 必须 200 key_sent 且注入 `1`。
    #[tokio::test]
    async fn waiting_state_post_still_delivers_mapped_keys() {
        let fake = FakeInjector::ok();
        let state = question_state_with_msgs(
            fake.clone(),
            Box::new(|_, sid: &str, _| match sid {
                "sess_bh" => Ok(crate::remote::content::MessagesPage {
                    messages: vec![user_msg(0), plan_msg(1, "# 执行计划")],
                    truncated: false,
                }),
                _ => Err("无消息".to_string()),
            }),
        );
        persist_named_device(&state, "mm", "测试设备");
        state.store.with(|conn| {
            crate::database::dao::approval_wait::mark(conn, "claude", "sess_bh", 1_000, "计划批准")
        });
        let app = router(state.clone());
        let r = app
            .oneshot(req(
                "POST",
                "/m/api/v1/session-approve",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_bh","optionId":"approve"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        assert!(body_string(r).await.contains("key_sent"));
        assert_eq!(
            fake.recorded_keys(),
            vec![(69u32, "1".to_string())],
            "N1 反向锁：非预期态（标记路径）的映射键位路径零回归"
        );
    }

    /// **N3（复审 Minor）**：F3-2 的**短路序/零 IO 口径**必须有回归锁——复审实测
    /// 「删掉 `&& !marked && status != Waiting` 两个守卫（变成无条件读页）」时**全套 lib
    /// 仍全绿**，即成本口径无保护（代码与注释都对，但将来有人调 `&&` 顺序不会红）。
    ///
    /// 修法：用**计数 message_source** 断言读页次数——这是本条口径唯一可观测的面。
    /// 五个场景覆盖全部短路臂与唯一的价值臂：
    /// - **Waiting ∧ 计划族**（sess_bi：codex Waiting）→ 门由状态满足 → **零读页**；
    /// - **标记态 ∧ 计划族**（sess_bj：codex Idle + 审批标记）→ 门由标记满足 → **零读页**；
    /// - **非计划族**（sess_bk：claude Idle + 尾部计划）→ 族收窄 → **零读页**；
    /// - **计划族 ∧ Idle ∧ 无标记 ∧ 尾部有计划**（sess_bb）→ 读**恰好一次**（价值臂）；
    /// - **计划族 ∧ Idle ∧ 无标记 ∧ 尾部无计划**（sess_bc）→ 读恰好一次后判 false
    ///   → 409（确认「读一次就够」）。
    ///
    /// **计数语义**：计数 `message_source` 的**调用次数**（不是成功次数）——桩返回 Err
    /// 也计数（`ok()` 的失败路径同样是一次调用尝试）。用 Err 桩让用例不依赖消息内容，
    /// 把断言收敛到「读没读」这一个自由度；需要「尾部有计划」的两例才给真页。
    #[tokio::test]
    async fn post_plan_pending_read_page_only_when_needed() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc as StdArc;

        let fake = FakeInjector::ok();
        let calls = StdArc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let state = question_state_with_msgs(
            fake.clone(),
            Box::new(move |_, sid: &str, _| {
                counter.fetch_add(1, Ordering::SeqCst);
                // 只有「尾部有计划」的两个 id 给真页，其余给 Err（Err = 判据不成立）
                match sid {
                    "sess_bb" => Ok(codex_plan_pending_page()),
                    "sess_bc" => Ok(codex_plan_consumed_page()),
                    _ => Err("计数桩：不提供内容".to_string()),
                }
            }),
        );
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state.clone());

        // ① Waiting ∧ 计划族（codex）→ 状态门满足，`||` 短路在读页之前
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-approve",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_bi","optionId":"approve"}"#),
            ))
            .await
            .unwrap();
        let s1 = r.status();
        assert_ne!(s1, 404, "Waiting 态映射存在 → 不该 no_mapping");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "N3：Waiting 态满足门 → 零读页（`status != Waiting` 守卫）"
        );

        // ② 标记态 ∧ 计划族（codex Idle + 审批标记）→ 门由标记满足
        state.store.with(|conn| {
            crate::database::dao::approval_wait::mark(conn, "codex", "sess_bj", 1_000, "工具审批")
        });
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-approve",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_bj","optionId":"approve"}"#),
            ))
            .await
            .unwrap();
        assert_ne!(r.status(), 409, "标记态不该 409 not_waiting");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "N3：标记态满足门 → 零读页（`!marked` 守卫）"
        );

        // ③ 非计划族（claude Idle + 尾部计划）→ 族收窄，**恒不读页**
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-approve",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_bk","optionId":"approve"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 409, "非计划族 + 非 Waiting → 既有 409 口径");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "N3：非计划族工具恒不读页（`plan_dialog_family` 守卫——claude 零额外 IO）"
        );

        // ④ **价值臂**：计划族 ∧ Idle ∧ 无标记 ∧ 尾部有计划 → 读**恰好一次**并放行
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-approve",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_bb","optionId":"approve"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(
            r.status(),
            404,
            "预期态下映射 id 被 N1 拒（此处只验读页次数）"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "N3 价值臂：唯一需要读页的形态 → 恰好一次（F3-2 的用武之地）"
        );

        // ⑤ 计划族 ∧ Idle ∧ 无标记 ∧ 尾部无计划 → 读一次后判 false → 409（不重读）
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-approve",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_bc","optionId":"approve"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 409);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "N3：尾部无计划的计划族会话 → 读一次即判 false（不重读）"
        );
    }

    /// **丁T2 互斥（无标记路径）**：kimi 的审批在场 → 问答卡不可用（硬约束① 扩到
    /// kimi 新卡）。
    ///
    /// **可达态说明（为什么不构造「计划 + 待决 AUQ」）**：kimi 的交互是**阻塞式**——
    /// 问答未答完时模型不可能提出计划，故「尾部计划提案 ∧ 尾部待决 AUQ」在真实 wire
    /// 里不可达（真实序列：AUQ tool.call → interaction.request → resolved → tool.result；
    /// 计划审批序列：Write plan → interaction.request(plan_review) → resolved →
    /// ExitPlanMode）。**真实可达的错位双卡**来自**陈旧问答标记**：kimi 的
    /// `question_wait_marks` 行靠清除事件删除，兔维斯 未运行/清除事件丢失时会残留
    /// （`tuvis.db` 现状即 `question_wait_marks` 表空、kimi 无标记通道的实证背景）——
    /// 此时**批准在等计划确认、问答卡却按陈旧标记冒出来**，正是本门要拦的形态。
    ///
    /// 夹具：kimi 会话 + 播种问答标记（通道 A）+ 尾部计划提案（预期态）→ 问答必须
    /// 不可用、审批照常可用；反向锁：尾部计划之后有工具事件（审批窗口已关）→ 问答
    /// 照常可用（陈旧标记仍走既有通道 A —— 本门**不**扩大压制面）。
    #[tokio::test]
    async fn kimi_plan_pending_blocks_question_card() {
        let fake = FakeInjector::ok();
        let state = question_state_with_msgs(
            fake.clone(),
            Box::new(|_, sid: &str, _| match sid {
                // 审批在场：尾部是计划提案（其前可能有历史工具事件，不影响判据）
                "sess_aw" => Ok(crate::remote::content::MessagesPage {
                    messages: vec![
                        user_msg(0),
                        plan_msg(1, "# 上一版计划"),
                        tool_result_msg(2, "Wrote 400 bytes to …/plans/a.md"),
                        plan_msg(3, "# 新版计划（审批中）"),
                    ],
                    truncated: false,
                }),
                // 审批已关：计划之后有工具事件（ExitPlanMode 回执）→ 预期态清除
                "sess_ax" => Ok(plan_then_tool_event_page()),
                _ => Err("无消息".to_string()),
            }),
        );
        persist_named_device(&state, "mm", "测试设备");
        // 陈旧问答标记（通道 A 形态）：两会话都播——sess_aw 应被预期态门拦下，
        // sess_ax 应照常出卡（反向锁：不扩大压制面）
        for sid in ["sess_aw", "sess_ax"] {
            state.store.with(|conn| {
                crate::database::dao::question_wait::mark(
                    conn,
                    "kimi",
                    sid,
                    1_000,
                    "等待回答",
                    Some(KIMI_Q_PAYLOAD),
                )
            });
        }
        let app = router(state.clone());
        // sess_aw：kimi 审批在场（尾部计划）→ 问答不可用（陈旧标记被压）
        let r = app
            .clone()
            .oneshot(req(
                "GET",
                "/m/api/v1/session-question?session_id=sess_aw",
                Some("mam_device=mm"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(
            v["available"], false,
            "kimi 审批在场（尾部计划提案）→ 问答卡不可用（硬约束① 扩面，无标记路径）"
        );
        assert!(v["questions"].as_array().unwrap().is_empty());
        // 问答 POST 同源拒绝（双卡错位的注入面同样关死）
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-question/answer",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_aw","action":"select","index":0}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 409);
        assert!(body_string(r).await.contains("no_question"));
        assert!(fake.recorded_keys().is_empty(), "问答键零出手");
        // **审批侧不在本用例断言（如实申报）**：播种的陈旧问答标记会先撞上既有
        // 硬约束①（`question_marked → 审批不可用`，批次乙 T8 的隔离规则），故本夹具下
        // 审批也判不可用。**该优先级未动**（本轮不改批次丙的隔离裁决）——代价是
        // 「陈旧标记 + 计划待确认」时两张卡都不出；受益面是「真问答在场时审批卡绝不
        // 误出」的安全面保持原样。审批侧可用性由
        // `kimi_plan_approval_card_never_emits_mapping_keys`（无标记形态）覆盖。
        //
        // sess_ax 反向锁：审批窗口已关 → 问答照常可用（陈旧标记走既有通道 A）
        let r = app
            .oneshot(req(
                "GET",
                "/m/api/v1/session-question?session_id=sess_ax",
                Some("mam_device=mm"),
                None,
            ))
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(
            v["available"], true,
            "审批窗口已关（计划后有工具事件）→ 问答照常可用（门不扩大压制面）"
        );
        assert_eq!(v["source"], "mark");
    }

    /// **丁T1 复评 F-3 端到端**：opencode 的问答销卡信号——走**真实 reader**
    /// （`read_opencode_messages_impl` + tempdir 内的 opencode.db），不复刻 reader 产物。
    ///
    /// 根因回顾：opencode reader 原先只产 tool-call 不产 tool-result → 问答端点的
    /// 「其后无 tool-result」判据恒真 → **已答完的问题仍 available=true，卡片不消失**。
    /// 修法：reader 对问答类部件的终态（completed/error）补一条 tool-result。
    ///
    /// 夹具（2026-09-21 本机 part 表实测形态）：sess_ap = running（待决）、
    /// sess_aq = completed + metadata.answers（已答）。message_source 经
    /// `read_session_messages_impl` 指向 tempdir home（零接触真实 ~/.local）。
    #[tokio::test]
    async fn opencode_question_availability_follows_part_status() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join(".local/share/opencode/opencode.db");
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, data TEXT);
             CREATE TABLE part (message_id TEXT, session_id TEXT, data TEXT, time_created INTEGER);",
        )
        .unwrap();
        let q_input = r#"{"questions":[{"header":"Build output folder","question":"Which folder?","options":[{"label":"dist","description":"d"},{"label":"out","description":"o"}]}]}"#;
        for (ses, mid, status, extra) in [
            ("sess_ap", "m_ap", "running", String::new()),
            (
                "sess_aq",
                "m_aq",
                "completed",
                r#","metadata":{"answers":[["out"]],"truncated":false},"output":"{\"answers\":[[\"out\"]]}""#
                    .to_string(),
            ),
        ] {
            conn.execute(
                "INSERT INTO message (id, session_id, time_created, data) VALUES (?1, ?2, 100, '{\"role\":\"assistant\"}')",
                rusqlite::params![mid, ses],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO part (message_id, session_id, data, time_created) VALUES (?1, ?2, ?3, 1783326720870)",
                rusqlite::params![
                    mid,
                    ses,
                    format!(
                        r#"{{"type":"tool","tool":"question","state":{{"status":"{status}","input":{q_input}{extra}}}}}"#
                    )
                ],
            )
            .unwrap();
        }
        drop(conn);

        // 会话夹具：question_state 已含 sess_ap/aq（Waiting），此处把 pid 对上；
        // message_source 走真实 reader（home = tempdir）
        let home = tmp.path().to_path_buf();
        let fake = FakeInjector::ok();
        let state = question_state_with_msgs(
            fake.clone(),
            Box::new(move |tool: &str, sid: &str, limit: usize| {
                crate::remote::content::read_session_messages_impl(
                    &home, None, None, tool, sid, limit,
                )
            }),
        );
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state.clone());

        // 待决（running）→ available=true（卡该出，且不得被自己误销）
        let r = app
            .clone()
            .oneshot(req(
                "GET",
                "/m/api/v1/session-question?session_id=sess_ap",
                Some("mam_device=mm"),
                None,
            ))
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(
            v["available"], true,
            "待决的 opencode 问答必须可用（待决窗口内不得被误销）"
        );
        assert_eq!(v["source"], "scan");

        // 已答（completed + answers）→ available=false（销卡信号生效）
        let r = app
            .clone()
            .oneshot(req(
                "GET",
                "/m/api/v1/session-question?session_id=sess_aq",
                Some("mam_device=mm"),
                None,
            ))
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(
            v["available"], false,
            "已答的 opencode 问答必须不可用（F-3：reader 补的 tool-result 销卡信号）"
        );
        // 已答 → 审批侧也不被问答压制（硬约束① 的反向：已答不再构成「问答在场」）
        let r = app
            .oneshot(req(
                "GET",
                "/m/api/v1/session-approve-options?session_id=sess_aq",
                Some("mam_device=mm"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        // opencode 不在默认映射表 → 无映射 → available=false（但不是被问答压的）；
        // 关键断言是它不 panic 且路径可达（映射缺失与问答隔离是两根轴）
        assert_eq!(v["reason"], serde_json::Value::Null);
        assert!(fake.recorded_keys().is_empty(), "本用例不触注入");
    }
    #[tokio::test]
    async fn question_mark_bad_payload_falls_through() {
        let fake = FakeInjector::ok();
        let state = question_state(fake.clone());
        persist_named_device(&state, "mm", "测试设备");
        state.store.with(|conn| {
            crate::database::dao::question_wait::mark(
                conn,
                "claude",
                "sess_ab",
                1_000,
                "等待回答",
                Some("not-json"),
            )
        });
        let app = router(state.clone());
        let r = app
            .oneshot(req(
                "GET",
                "/m/api/v1/session-question?session_id=sess_ab",
                Some("mam_device=mm"),
                None,
            ))
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(
            v["available"], false,
            "标记载荷损坏且通道 B 未命中 → 不可用（不误报）"
        );
    }

    /// P3 审计动作词表（Task 7 P3c）：KV 定制映射含域外 id=other 的选项 → POST
    /// session-approve 照发键位（x）→ 审计 action 收敛为 "key"（W5 词表 send|queue|
    /// flush|jump|retract|approve|reject|fail|key|open——open 已随 Task 11 一键
    /// resume 兑现，批次乙 T8 再追加 answer（问答端点，锁定见 question_answer_* 族），
    /// 批次丙 T6 追加 mode，**丁T3 追加 slash**（裁2 斜杠命令裸注入的溯源动作，
    /// 锁定见 `slash_message_bare_injects_and_audits_slash` / `slash_message_queued_keeps_bare_form_and_slash_action` 两用例）——
    /// 之外的域外 id 不得原样进审计
    /// action 列——key 是本次新增的收敛动作）且不 panic；域外 warn 在实现侧 log，
    /// 测试不断言日志。
    /// KV 经内存库 seed（DeviceStore 缝，零接触真实 ~/.tuvis）；sess_j 全测试集唯一
    /// id（守卫 id 立规）。前端 AuditLogSection「action 原样小写展示」契约不受影响。
    #[tokio::test]
    async fn audit_action_vocab() {
        let fake = FakeInjector::ok();
        let state = approve_state(fake.clone(), Some(APPROVE_HIT_MSG));
        // 定制映射 seed 进本测试自己的内存库（其他测试的 memory 库互不可见，零互染）
        state.store.with(|c| {
            crate::database::dao::settings::set_setting_conn(
                c,
                crate::inject::approve::KV_KEY,
                r#"[{"tool":"claude","verified_with":"2.1.251",
  "prompt_markers":["do you want to proceed"],
  "options":[{"id":"approve","label":"允许","key":"1"},
             {"id":"reject","label":"拒绝","key":"esc"},
             {"id":"other","label":"其他","key":"x"}]}]"#,
            )
        });
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state.clone());
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-approve",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_j","optionId":"other"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let body = body_string(r).await;
        assert!(
            body.contains("\"status\":\"key_sent\""),
            "域外 id 命中定制映射照发键位：{body}"
        );
        assert_eq!(
            fake.recorded_keys(),
            vec![(20u32, "x".to_string())],
            "域外 id 选项的键位照映射投递"
        );
        let audits = state
            .store
            .with(|c| crate::database::dao::write_audit::recent_conn(c, 10));
        assert_eq!(audits.len(), 1);
        assert_eq!(audits[0].action, "key", "域外 id 审计 action 收敛为 key");
        assert_eq!(audits[0].result, "ok");
        assert_eq!(audits[0].session_id, "sess_j");
    }

    /// 严格档（M9R Task 10 裁决：未取证不出键，probe-pending 恒判漂移）：KV seed
    /// verified_with="probe-pending" 的 codex 定制映射（复用 Task 7 audit_action_vocab 的
    /// store.with 内存库 seed 模式，零接触真实 ~/.tuvis）→
    /// - GET session-approve-options：available=false + options 空 + reason=「键位待实测确认，
    ///   请用普通发送」（前端 ApproveCard 契约：available=false 且带 reason → 只渲染提示条）
    ///   ——即使 sess_k 处 Waiting 且 last_message 命中 marker；
    /// - POST session-approve：404 no_mapping（未取证=映射缺失，降级走普通发送），键位
    ///   永不出手。
    /// sess_k 全测试集唯一 id（守卫 id 立规）。
    #[tokio::test]
    async fn probe_pending_strict_policy() {
        let fake = FakeInjector::ok();
        let state = approve_state(fake.clone(), Some(APPROVE_HIT_MSG));
        // probe-pending 定制映射 seed 进本测试自己的内存库（其他测试的 memory 库互不可见）
        state.store.with(|c| {
            crate::database::dao::settings::set_setting_conn(
                c,
                crate::inject::approve::KV_KEY,
                r#"[{"tool":"codex","verified_with":"probe-pending",
  "prompt_markers":["would you like to make the following"],
  "options":[{"id":"approve","label":"允许","key":"y"},
             {"id":"reject","label":"拒绝","key":"esc"}]}]"#,
            )
        });
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state.clone());
        // GET：Waiting + detect 命中（sess_k last_message 即 codex 弹框标题原文）仍压为不可批
        let r = app
            .clone()
            .oneshot(req(
                "GET",
                "/m/api/v1/session-approve-options?session_id=sess_k",
                Some("mam_device=mm"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["available"], false, "probe-pending 严格档恒不可批");
        assert!(
            v["options"].as_array().unwrap().is_empty(),
            "严格档不下发选项（键位与选项均不出键）"
        );
        assert_eq!(
            v["reason"], "键位待实测确认，请用普通发送",
            "严格档必须下发降级原因（前端提示条渲染契约）"
        );
        assert_eq!(v["verifiedWith"], "probe-pending");
        assert_eq!(v["drift"], true, "probe-pending 恒判漂移");
        // POST：404 no_mapping（未取证=映射缺失），零按键投递
        let r = app
            .oneshot(req(
                "POST",
                "/m/api/v1/session-approve",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_k","optionId":"approve"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 404);
        assert!(body_string(r).await.contains("no_mapping"));
        assert!(fake.recorded_keys().is_empty(), "严格档不得有任何按键投递");
    }

    /// 质量评审补锁：非严格不可批三形态（非 Waiting / detect 未命中 / 无映射）的
    /// reason 恒为 null——锁定前端 ApproveCard 契约「available=false 且无 reason →
    /// 卡自隐」（reason 只属严格档 probe-pending 语义，不得挪作普通不可批提示）。
    #[tokio::test]
    async fn non_strict_unavailable_reason_is_null() {
        let state_hit = approve_state(FakeInjector::ok(), Some(APPROVE_HIT_MSG));
        persist_named_device(&state_hit, "mm", "测试设备");
        let app_hit = router(state_hit);
        // 非 Waiting（sess_b Processing）→ reason null
        let r = app_hit
            .clone()
            .oneshot(req(
                "GET",
                "/m/api/v1/session-approve-options?session_id=sess_b",
                Some("mam_device=mm"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["available"], false);
        assert_eq!(
            v["reason"],
            serde_json::Value::Null,
            "非 Waiting 不得带 reason（自隐契约）"
        );
        // 无映射（sess_d zcode）→ reason null
        let r = app_hit
            .oneshot(req(
                "GET",
                "/m/api/v1/session-approve-options?session_id=sess_d",
                Some("mam_device=mm"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["available"], false);
        assert_eq!(
            v["reason"],
            serde_json::Value::Null,
            "无映射不得带 reason（自隐契约）"
        );
        // detect 未命中（state_miss：sess_a last_message="无关"）与无 last_message
        // （sess_e 夹具原样恒 None）→ reason null
        let state_miss = approve_state(FakeInjector::ok(), Some("无关"));
        persist_named_device(&state_miss, "mm", "测试设备");
        let app_miss = router(state_miss);
        for sid in ["sess_a", "sess_e"] {
            let r = app_miss
                .clone()
                .oneshot(req(
                    "GET",
                    &format!("/m/api/v1/session-approve-options?session_id={sid}"),
                    Some("mam_device=mm"),
                    None,
                ))
                .await
                .unwrap();
            assert_eq!(r.status(), 200, "{sid}");
            let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
            assert_eq!(v["available"], false, "{sid}");
            assert_eq!(
                v["reason"],
                serde_json::Value::Null,
                "{sid} 不可批不得带 reason（自隐契约）"
            );
        }
    }

    /// 质量评审补锁（现实现选定行为定格）：严格档 hint 与 detect 命中无关——probe-pending
    /// 映射 + last_message 与任何 marker 无关（sess_l）仍下发 reason。提示条语义=键位
    /// 取证状态（未取证），非「审批中」判定；防后人把严格档短路「修」到 detect 之后
    /// （那会让 detect-miss 的真审批会话完全无提示）。
    #[tokio::test]
    async fn probe_pending_hint_even_on_detect_miss() {
        let fake = FakeInjector::ok();
        let state = approve_state(fake.clone(), Some(APPROVE_HIT_MSG));
        // probe-pending 定制映射 seed 进本测试自己的内存库（与其他测试零互染）
        state.store.with(|c| {
            crate::database::dao::settings::set_setting_conn(
                c,
                crate::inject::approve::KV_KEY,
                r#"[{"tool":"codex","verified_with":"probe-pending",
  "prompt_markers":["would you like to make the following"],
  "options":[{"id":"approve","label":"允许","key":"y"}]}]"#,
            )
        });
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state);
        let r = app
            .oneshot(req(
                "GET",
                "/m/api/v1/session-approve-options?session_id=sess_l",
                Some("mam_device=mm"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["available"], false);
        assert!(
            v["options"].as_array().unwrap().is_empty(),
            "detect 未命中选项恒空"
        );
        assert_eq!(
            v["reason"], "键位待实测确认，请用普通发送",
            "detect 未命中严格档 hint 仍下发（短路序定格）"
        );
        assert!(fake.recorded_keys().is_empty(), "查询端点零按键投递");
    }

    // ==== M6R–M9R Task 11：session-open 端点（R5 一键 resume）====
    // 零污染 + 零真开窗：spawn 缝（RemoteState.resume_spawner）注入记录型假
    // spawner；会话快照/设备表/审计全走注入缝与内存库，不触真实 ~/.tuvis。

    /// Task 11 记录型假 spawner：克隆记录 SpawnSpec 不真开窗（R5 硬约束：
    /// spawner 缝使端点测试不真开窗；Windows 实开窗验证归用户手工/后续验收）
    struct RecordingSpawner(
        std::sync::Arc<std::sync::Mutex<Vec<crate::inject::resume::SpawnSpec>>>,
    );

    impl RecordingSpawner {
        fn new() -> Self {
            Self(std::sync::Arc::new(std::sync::Mutex::new(Vec::new())))
        }
        fn seam(&self) -> std::sync::Arc<crate::inject::resume::SpawnFn> {
            let log = self.0.clone();
            std::sync::Arc::new(move |spec: &crate::inject::resume::SpawnSpec| {
                log.lock().unwrap().push(spec.clone());
                Ok(())
            })
        }
        /// 评审 I3：恒败 spawner（照常记录后报 Err）——驱动 200-failed 分支测试
        fn seam_failing(
            &self,
            err: &'static str,
        ) -> std::sync::Arc<crate::inject::resume::SpawnFn> {
            let log = self.0.clone();
            std::sync::Arc::new(move |spec: &crate::inject::resume::SpawnSpec| {
                log.lock().unwrap().push(spec.clone());
                Err(err.to_string())
            })
        }
        fn recorded(&self) -> Vec<crate::inject::resume::SpawnSpec> {
            self.0.lock().unwrap().clone()
        }
    }

    /// macOS 端点路径固定接生产效果回查探针（open_session_terminal_with →
    /// open_macos_with → macos_effect_probe：轮询进程表 3s 找 resume 特征子串，
    /// resume_effect_in_snapshot 按 cmd 拼接串 contains 命中）。spawner 假体 Ok
    /// 后若探针未命中，双通道按「死窗」级联 failed——既有
    /// session_open_endpoint_opens_and_audits 预存失败即此根因（task-3-report）。
    /// 故出手时顺手种一个 argv 携 resume 特征的暗桩进程（`sh -c "sleep 5 # 特征"`，
    /// 首轮/次轮采样即命中；5s > 3s 回查窗自灭，非终端窗口——「零真开窗」约束
    /// 不破，~/.tuvis 零污染）。非 macOS 平台无回查探针，运行时 no-op。
    /// 共享作用域（原 nested 于 session_open_archive_fallback_tests，常红修复
    /// 时上提）：opens_and_audits 与归档回退两类 session-open 测试同用。
    fn spawn_effect_decoy(resume_cmd: &str) {
        if !cfg!(target_os = "macos") {
            return;
        }
        let _ = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("sleep 5 # {resume_cmd}"))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }

    /// Task 11 专用 state：会话夹具独占 id（守卫 id 立规的防串键纪律同源）——
    /// sess_m（claude Waiting 正常 cwd）/ sess_n（claude Waiting 空白 cwd）/
    /// sess_o（workbuddy Idle，未入 resume 命令表）；spawner 注入记录型假体。
    /// 其余缝与 inject_state 同口径（内存库，零接触真实 ~/.tuvis）。
    fn open_state(spawner: std::sync::Arc<crate::inject::resume::SpawnFn>) -> Arc<RemoteState> {
        open_state_with_home(spawner, None)
    }

    /// T3 带 home 注入的 state 变体：home_source 消费注入值——信任归一用例注入
    /// tempdir home（携假 ~/.claude.json，零接触真实主目录）；None = 既有用例
    /// 直跳过归一。调用方持 TempDir 存活于测试作用域即可（state 只存路径串）。
    fn open_state_with_home(
        spawner: std::sync::Arc<crate::inject::resume::SpawnFn>,
        home: Option<std::path::PathBuf>,
    ) -> Arc<RemoteState> {
        let mut sess_m = inj_sess(
            "sess_m",
            crate::session::AgentType::Claude,
            30,
            crate::session::SessionStatus::Waiting,
        );
        sess_m.project_path = "/tmp/proj-m".into();
        let mut sess_n = inj_sess(
            "sess_n",
            crate::session::AgentType::Claude,
            31,
            crate::session::SessionStatus::Waiting,
        );
        sess_n.project_path = "  ".into();
        let sess_o = inj_sess(
            "sess_o",
            crate::session::AgentType::WorkBuddy,
            32,
            crate::session::SessionStatus::Idle,
        );
        let sessions = vec![sess_m, sess_n, sess_o];
        Arc::new(RemoteState {
            ui_config_source: Box::new(|| None),
            subagent_source: std::collections::HashMap::new(),
            subagent_message_source: std::collections::HashMap::new(),
            target_evidence: no_target_evidence(),
            capability_table: crate::inject::capability::new_table(),
            session_source: Box::new(move || crate::session::SessionsResponse {
                sessions: sessions.clone(),
                total_count: sessions.len(),
                waiting_count: 0,
            }),
            store: crate::remote::pairing::DeviceStore::memory(),
            injector: std::sync::Arc::new(crate::inject::engine::RealInjector),
            resume_spawner: spawner,
            // C6：create 缝束缺省 stub（仅 session-create 用例就地覆盖）
            create_hub: create_hub_stub(),
            archive_source: Box::new(Vec::new),
            archive_delete: std::sync::Arc::new(|_: Option<&str>| 0usize),
            confirm_probe: std::sync::Arc::new(|_, _, _| true),
            // 丁T3：本组测试的对话框在场探针缺省「无法判定」（None）——控制类注入
            // 照常投递；「在场即拒」的用例就地建 state 覆盖为假体（见 mode_switch_* 用例）
            dialog_probe: std::sync::Arc::new(|_, _| None),
            screen_probe: std::sync::Arc::new(|_, _| None),
            host_source: Box::new(|| serde_json::Value::Null),
            message_source: Box::new(|_, _, _| Err("测试桩：未注入内容源".to_string())),
            path_source: Box::new(|_, _, _| (Vec::new(), false)),
            watcher_tx: tokio::sync::broadcast::channel(64).0,
            board_hidden_ids: Box::new(Vec::new),
            board_hidden_hide: std::sync::Arc::new(|_| 0usize),
            board_hidden_unhide: std::sync::Arc::new(|_| 0usize),
            unread_mark_read: std::sync::Arc::new(|_, _| ()),
            session_close: std::sync::Arc::new(|_| Ok(())),
            sse_registry: Arc::new(SseRegistry::default()),
            max_devices_source: Box::new(|| 3),
            pin_limiter: std::sync::Mutex::new(crate::remote::pin::PinRateLimiter::new()),
            global_pin_limiter: std::sync::Mutex::new(crate::remote::pin::PinRateLimiter::global()),
            pin_source: Box::new(|| Some("1234".to_string())),
            now_source: Box::new(|| chrono::Utc::now().timestamp_millis()),
            via_hosts_source: Box::new(|| None),
            rate_bucket_channels_source: Box::new(Vec::new),
            // T3：home_source 消费注入值（信任归一用例 = tempdir home；None = 跳过）
            home_source: Box::new(move || home.as_ref().map(|p| p.to_string_lossy().to_string())),
            // C7：配对计数缝缺省空表（既有用例零影响；配对打标/提示用例就地覆盖）
            pairing_counter: Box::new(Vec::new),
        })
    }

    /// 一键 resume 出手：200 opening + no-store + spawner 收到命令表产物 +
    /// 审计 action=open result=ok；无 cookie → 403（PIN gate 照旧覆盖）
    #[tokio::test]
    #[cfg_attr(
        not(any(windows, target_os = "macos")),
        ignore = "注入平台门（inject/routing.rs）：仅 Windows/macOS 可注入，本测走注入链"
    )]
    async fn session_open_endpoint_opens_and_audits() {
        let spawner_rec = RecordingSpawner::new();
        let state = open_state(spawner_rec.seam());
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state.clone());
        // 无 cookie → 403（nest 内层 gate 结构性覆盖新端点）
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-open",
                None,
                Some(r#"{"sessionId":"sess_m"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 403, "session-open 必须过 PIN 门禁");
        // 有 cookie → 200 opening + no-store。出手前种 argv 携特征的暗桩进程：
        // 生产效果回查探针（macos_effect_probe）要在进程表里找到 resume 特征才判
        // opening——无窗测试环境不种桩必级联 failed（本测试原常红根因之一，见
        // spawn_effect_decoy 注）。非 macOS 平台 no-op。
        spawn_effect_decoy("claude --resume sess_m");
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-open",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_m"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(
            r.headers()
                .get("cache-control")
                .and_then(|v| v.to_str().ok()),
            Some("no-store"),
            "打开回执是门禁下私有写路径，禁止中间层缓存"
        );
        let body = body_string(r).await;
        assert!(
            body.contains("\"status\":\"opening\""),
            "一键 resume 应回执 opening：{body}"
        );
        // spawner 恰被调用一次，携带命令表产物（wt/conhost 分支的平台差异不断言）
        let recorded = spawner_rec.recorded();
        assert_eq!(recorded.len(), 1, "spawner 恰被调用一次");
        match &recorded[0] {
            crate::inject::resume::SpawnSpec::Windows { args, cwd, .. } => {
                assert!(
                    args.iter().any(|a| a == "claude --resume sess_m"),
                    "spawn 计划必须携带 claude 的 resume 命令：{:?}",
                    recorded[0]
                );
                // 评审 I1：cwd 进 spec（conhost 分支的 current_dir 消费点）
                assert_eq!(cwd, "/tmp/proj-m", "spawn 计划必须携带项目目录");
            }
            crate::inject::resume::SpawnSpec::MacosApplescript { script } => {
                // 平台对等：macOS 断言脚本载荷（cwd + resume 命令，对齐
                // dead_session_opens_from_archive 的 macOS 断言先例），Windows 臂原样。
                // 原此处 panic!（「Windows 运行时不得派发 AppleScript 变体」）在 macOS
                // 运行时必炸——本测试常红第二层根因，同批清除。
                assert!(
                    script.contains("/tmp/proj-m") && script.contains("claude --resume sess_m"),
                    "spawn 计划必须携带项目目录与 claude resume 命令：{script}"
                );
            }
        }
        // 审计 action=open（Task 7 预留兑现）result=ok
        let audits = state
            .store
            .with(|c| crate::database::dao::write_audit::recent_conn(c, 10));
        assert_eq!(audits[0].action, "open");
        assert_eq!(audits[0].result, "ok");
        assert_eq!(audits[0].session_id, "sess_m");
        assert_eq!(audits[0].device_name, "测试设备");
        assert_eq!(audits[0].agent_type, "claude");
    }

    /// 无 cwd：404 no_cwd + spawner 不出手 + 不写审计（校验失败不落账口径）
    #[tokio::test]
    async fn session_open_endpoint_no_cwd() {
        let spawner_rec = RecordingSpawner::new();
        let state = open_state(spawner_rec.seam());
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state.clone());
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-open",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_n"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 404);
        assert_eq!(body_string(r).await, "{\"error\":\"no_cwd\"}");
        assert!(spawner_rec.recorded().is_empty(), "无 cwd 不得出手 spawn");
        let audits = state
            .store
            .with(|c| crate::database::dao::write_audit::recent_conn(c, 10));
        assert!(audits.is_empty(), "校验失败不写审计");
    }

    /// T3 未信任预检（远程/手机回执面）：命中 ~/.claude.json projects 条款但全部
    /// 未信任 → 200 opening 回执附 `trustPromptExpected:true`（会话页提示条消费）；
    /// 全 false 无可复用 casing → spawn cwd 保持原样。tempdir 假 home（假
    /// ~/.claude.json），零接触真实主目录。
    #[tokio::test]
    #[cfg_attr(
        not(any(windows, target_os = "macos")),
        ignore = "注入平台门（inject/routing.rs）：仅 Windows/macOS 可注入，本测走注入链"
    )]
    async fn session_open_endpoint_trust_prompt_expected_when_untrusted() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join(".claude.json"),
            r#"{"projects":{"/tmp/proj-m":{"hasTrustDialogAccepted":false}}}"#,
        )
        .unwrap();
        let spawner_rec = RecordingSpawner::new();
        let state = open_state_with_home(spawner_rec.seam(), Some(home.path().to_path_buf()));
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state.clone());
        // macOS 生产效果回查探针需要 argv 暗桩（opens_and_audits 同口径）；非 macOS no-op
        spawn_effect_decoy("claude --resume sess_m");
        let r = app
            .oneshot(req(
                "POST",
                "/m/api/v1/session-open",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_m"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let body = body_string(r).await;
        assert!(
            body.contains("\"status\":\"opening\""),
            "照常 opening：{body}"
        );
        assert!(
            body.contains("\"trustPromptExpected\":true"),
            "命中未信任条款必须附预检字段（会话页提示条）：{body}"
        );
        // 全 false：无可复用 casing，spawn cwd 原样（平台分臂断言同 opens_and_audits）
        let recorded = spawner_rec.recorded();
        assert_eq!(recorded.len(), 1, "spawner 恰被调用一次");
        match &recorded[0] {
            crate::inject::resume::SpawnSpec::Windows { cwd, .. } => {
                assert_eq!(cwd, "/tmp/proj-m", "全 false 不得改写 cwd");
            }
            crate::inject::resume::SpawnSpec::MacosApplescript { script } => {
                assert!(
                    script.contains("/tmp/proj-m"),
                    "全 false 不得改写 cwd：{script}"
                );
            }
        }
    }

    /// T3 归一端到端（远程路径，实证形态）：双 casing 条款一真一假并存 → spawn
    /// cwd **静默**复用真条款精确 casing（claude 查信任即命中、不弹窗），回执
    /// **不带** trustPromptExpected（已信任不得误报提醒）
    #[tokio::test]
    #[cfg_attr(
        not(any(windows, target_os = "macos")),
        ignore = "注入平台门（inject/routing.rs）：仅 Windows/macOS 可注入，本测走注入链"
    )]
    async fn session_open_endpoint_reuses_trusted_casing_silently() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join(".claude.json"),
            r#"{"projects":{"/tmp/proj-m":{"hasTrustDialogAccepted":false},"/TMP/PROJ-M":{"hasTrustDialogAccepted":true}}}"#,
        )
        .unwrap();
        let spawner_rec = RecordingSpawner::new();
        let state = open_state_with_home(spawner_rec.seam(), Some(home.path().to_path_buf()));
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state.clone());
        spawn_effect_decoy("claude --resume sess_m");
        let r = app
            .oneshot(req(
                "POST",
                "/m/api/v1/session-open",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_m"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let body = body_string(r).await;
        assert!(
            !body.contains("trustPromptExpected"),
            "复用真条款（已信任）不得误报预检提醒：{body}"
        );
        // spawn cwd = 真条款逐字 casing（平台分臂断言同 opens_and_audits）
        let recorded = spawner_rec.recorded();
        assert_eq!(recorded.len(), 1, "spawner 恰被调用一次");
        match &recorded[0] {
            crate::inject::resume::SpawnSpec::Windows { cwd, .. } => {
                assert_eq!(cwd, "/TMP/PROJ-M", "spawn cwd 必须是真条款的精确 casing");
            }
            crate::inject::resume::SpawnSpec::MacosApplescript { script } => {
                assert!(
                    script.contains("/TMP/PROJ-M"),
                    "spawn 载荷必须携带真条款精确 casing：{script}"
                );
            }
        }
    }

    /// 无映射（workbuddy 未入命令表）：404 no_resume_command + spawner 不出手
    #[tokio::test]
    async fn session_open_endpoint_no_resume_command() {
        let spawner_rec = RecordingSpawner::new();
        let state = open_state(spawner_rec.seam());
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state.clone());
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-open",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_o"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 404);
        assert_eq!(body_string(r).await, "{\"error\":\"no_resume_command\"}");
        assert!(
            spawner_rec.recorded().is_empty(),
            "未查证工具绝不出手 spawn"
        );
    }

    /// 会话不在快照：404 no_session；缺参：400（与 session-send 同口径）
    #[tokio::test]
    async fn session_open_endpoint_no_session_and_bad_request() {
        let spawner_rec = RecordingSpawner::new();
        let state = open_state(spawner_rec.seam());
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state.clone());
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-open",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_zzz"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 404);
        assert_eq!(body_string(r).await, "{\"error\":\"no_session\"}");
        // 缺参 → 400
        let r = app
            .oneshot(req(
                "POST",
                "/m/api/v1/session-open",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":""}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 400);
        assert!(spawner_rec.recorded().is_empty());
    }

    /// 评审 I3：spawn 出手失败 → 200 {"status":"failed","error"}（HTTP 200 恒定，
    /// 语义在 body——session-approve 同口径）+ 审计 action=open result=failed: 前缀。
    /// spawner 恒败（本机 wt/conhost 两分支皆败——降级链收口后的终态）
    #[tokio::test]
    #[cfg_attr(
        not(any(windows, target_os = "macos")),
        ignore = "注入平台门（inject/routing.rs）：仅 Windows/macOS 可注入，本测走注入链"
    )]
    async fn session_open_endpoint_spawn_failure_reports_failed() {
        let spawner_rec = RecordingSpawner::new();
        let state = open_state(spawner_rec.seam_failing("终端启动失败（模拟）"));
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state.clone());
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-open",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_m"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(
            r.status(),
            200,
            "失败回执走 200 语义分诊（session-approve 同口径）"
        );
        assert_eq!(
            r.headers()
                .get("cache-control")
                .and_then(|v| v.to_str().ok()),
            Some("no-store"),
            "失败回执同为门禁下私有写路径，禁止中间层缓存"
        );
        let body = body_string(r).await;
        assert!(
            body.contains("\"status\":\"failed\"") && body.contains("终端启动失败（模拟）"),
            "失败回执必须携带 status=failed 与后端错误文案：{body}"
        );
        // 出手了才谈失败：spawner 被调用（wt 在场时降级链两次、不在场一次——只断言非空）
        assert!(!spawner_rec.recorded().is_empty(), "失败回执前提是确有出手");
        // 审计 action=open result=failed: 前缀（出手失败照实落账）
        let audits = state
            .store
            .with(|c| crate::database::dao::write_audit::recent_conn(c, 10));
        assert_eq!(audits[0].action, "open");
        assert!(
            audits[0].result.starts_with("failed:"),
            "出手失败审计必须是 failed: 前缀：{}",
            audits[0].result
        );
        assert_eq!(audits[0].session_id, "sess_m");
    }

    /// M2（Mac 验收 D-5 根因①）：macOS osascript TCC 失败形态回执——缝注入假
    /// spawner 直接返回 [`classify_resume_error`] 的产物（分类函数本身跨平台单测
    /// 覆盖；端点在此只验回执契约）：200 failed 携带「自动化授权」指引 + 审计
    /// action=open result=failed: 前缀——账实一致，不再「open ok 但无窗」。
    /// 注：本测试在 Windows 上跑走 Windows 分支（open_session_terminal_with 按
    /// std::env::consts::OS 分派），但端点回执契约跨平台同形，与 OS 分派正交。
    #[tokio::test]
    #[cfg_attr(
        not(any(windows, target_os = "macos")),
        ignore = "注入平台门（inject/routing.rs）：仅 Windows/macOS 可注入，本测走注入链"
    )]
    async fn session_open_endpoint_macos_tcc_guidance_reports_failed() {
        let spawner_rec = RecordingSpawner::new();
        let state = open_state(spawner_rec.seam_failing(crate::inject::resume::MACOS_TCC_GUIDANCE));
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state.clone());
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-open",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_m"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(
            r.status(),
            200,
            "失败回执走 200 语义分诊（session-approve 同口径）"
        );
        let body = body_string(r).await;
        assert!(
            body.contains("\"status\":\"failed\"") && body.contains("自动化授权"),
            "失败回执必须携带 status=failed 与 TCC 授权指引：{body}"
        );
        assert!(!spawner_rec.recorded().is_empty(), "失败回执前提是确有出手");
        // 审计 action=open result=failed: 前缀且指引在账（出手失败照实落账）
        let audits = state
            .store
            .with(|c| crate::database::dao::write_audit::recent_conn(c, 10));
        assert_eq!(audits[0].action, "open");
        assert!(
            audits[0].result.starts_with("failed:") && audits[0].result.contains("自动化授权"),
            "审计必须 failed: 前缀且携带授权指引：{}",
            audits[0].result
        );
    }

    // ==== 历史会话区（spec 2026-09-20-mobile-archive-history §6.1）====
    mod archive_api_tests {
        use super::*;
        use crate::database::SessionArchiveRow;

        fn arch_row(id: &str, tool: &str, proj: &str, seen_secs_ago: i64) -> SessionArchiveRow {
            let seen = (chrono::Utc::now() - chrono::Duration::seconds(seen_secs_ago)).to_rfc3339();
            SessionArchiveRow {
                session_id: id.into(),
                agent_type: tool.into(),
                project_path: format!("/tmp/{proj}"),
                project_name: proj.into(),
                title: Some("标题".into()),
                last_status: "idle".into(),
                first_seen: seen.clone(),
                last_seen: seen,
                updated_at: String::new(),
            }
        }

        fn archive_state(rows: Vec<SessionArchiveRow>) -> axum::Router {
            // test_state() 返回 Arc<RemoteState>（引用计数 1、无他持）——Arc::get_mut
            // 就地换缝（比整份 RemoteState 字面量轻 30+ 行；本文件既有测试均为全字面量
            // 构造，此处引入 get_mut 模式属新写法，注释留痕）
            let mut st = test_state();
            let s = std::sync::Arc::get_mut(&mut st).expect("test_state 独占引用");
            s.archive_source = Box::new(move || rows.clone());
            s.archive_delete = std::sync::Arc::new(|_| 0);
            // 简报原始形态无凭据——新路由结构性在 PIN gate 之后（nest 内层），
            // 与全文件先例一致：persist_device 播种 + req 带 cookie 过闸（评审追记）
            persist_device(&st, "arch");
            crate::remote::server::router(st)
        }

        #[tokio::test]
        async fn days_window_and_order() {
            let app = archive_state(vec![
                arch_row("fresh", "codex", "a", 3600),     // 1h 前 → 1 天窗内
                arch_row("old2d", "kimi", "b", 2 * 86400), // 2 天前 → 仅 3/7 天窗
            ]);
            let r = app
                .clone()
                .oneshot(req(
                    "GET",
                    "/m/api/v1/sessions-archived?days=1",
                    Some("mam_device=arch"),
                    None,
                ))
                .await
                .unwrap();
            assert_eq!(r.status(), 200);
            let body = axum::body::to_bytes(r.into_body(), usize::MAX)
                .await
                .unwrap();
            let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
            let arr = v["archived"].as_array().unwrap();
            assert_eq!(arr.len(), 1);
            assert_eq!(arr[0]["sessionId"], "fresh");
            let projects = v["projects"].as_array().unwrap();
            assert_eq!(projects, &[serde_json::Value::from("a")]); // 窗口内项目聚合
        }

        #[tokio::test]
        async fn days_invalid_clamped_to_one() {
            let app = archive_state(vec![arch_row("old2d", "kimi", "b", 2 * 86400)]);
            let r = app
                .oneshot(req(
                    "GET",
                    "/m/api/v1/sessions-archived?days=999",
                    Some("mam_device=arch"),
                    None,
                ))
                .await
                .unwrap();
            let body = axum::body::to_bytes(r.into_body(), usize::MAX)
                .await
                .unwrap();
            let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(v["archived"].as_array().unwrap().len(), 0); // 夹到 1 天 → 排除
        }

        #[tokio::test]
        async fn live_session_excluded_from_archive() {
            let rows = vec![
                arch_row("live-1", "codex", "a", 60),
                arch_row("dead-1", "kimi", "b", 120),
            ];
            let mut st = test_state();
            let s = std::sync::Arc::get_mut(&mut st).expect("test_state 独占引用");
            s.archive_source = Box::new(move || rows.clone());
            s.archive_delete = std::sync::Arc::new(|_| 0);
            // 活板快照注入：session_source 类型 = Box<dyn Fn() -> SessionsResponse>
            // （server.rs:242）；inj_sess 四参夹具（server.rs:2760，id/agent_type/pid/status）
            s.session_source = Box::new(|| crate::session::SessionsResponse {
                sessions: vec![inj_sess(
                    "live-1",
                    crate::session::AgentType::Codex,
                    1,
                    crate::session::SessionStatus::Waiting,
                )],
                total_count: 1,
                waiting_count: 0,
            });
            persist_device(&st, "arch");
            let app = crate::remote::server::router(st);
            let r = app
                .oneshot(req(
                    "GET",
                    "/m/api/v1/sessions-archived",
                    Some("mam_device=arch"),
                    None,
                ))
                .await
                .unwrap();
            let body = axum::body::to_bytes(r.into_body(), usize::MAX)
                .await
                .unwrap();
            let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
            let arr = v["archived"].as_array().unwrap();
            assert_eq!(arr.len(), 1);
            assert_eq!(arr[0]["sessionId"], "dead-1");
        }

        #[tokio::test]
        async fn delete_requires_param() {
            let app = archive_state(vec![]);
            let r = app
                .oneshot(req(
                    "DELETE",
                    "/m/api/v1/sessions-archived",
                    Some("mam_device=arch"),
                    None,
                ))
                .await
                .unwrap();
            assert_eq!(r.status(), 400);
        }

        /// DELETE 参数→缝映射（终审 Important：破坏性端点成功路径零自动化覆盖）——
        /// 记录型假体（Arc<Mutex<Vec>> 收参 + 固定返回 7）锁定三件事：①`?session_id=s9`
        /// → 缝收到 `Some("s9")`；②`?all=1` → 收到 `None`；③**双参同在**
        /// （`?all=1&session_id=s9`）→ 仍收到 `None`（all 优先语义，防「target 映射
        /// 写反」一行回归）。响应形态 `{"ok":true,"deleted":<假体返回值>}` 一并断言。
        /// 换缝沿用本模块 Arc::get_mut 先例；过闸沿用 persist_device + cookie 先例。
        #[tokio::test]
        async fn delete_param_mapping_and_all_precedence() {
            let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Option<String>>::new()));
            let c2 = calls.clone();
            let mut st = test_state();
            let s = std::sync::Arc::get_mut(&mut st).expect("test_state 独占引用");
            s.archive_delete = std::sync::Arc::new(move |target: Option<&str>| {
                c2.lock()
                    .expect("测试单线程持锁")
                    .push(target.map(|t| t.to_string()));
                7 // 固定返回计数：响应 deleted 字段断言其来源是缝返回值
            });
            persist_device(&st, "arch");
            let app = crate::remote::server::router(st);

            // ① 单 session_id → Some("s9")
            let r = app
                .clone()
                .oneshot(req(
                    "DELETE",
                    "/m/api/v1/sessions-archived?session_id=s9",
                    Some("mam_device=arch"),
                    None,
                ))
                .await
                .unwrap();
            assert_eq!(r.status(), 200);
            let body = axum::body::to_bytes(r.into_body(), usize::MAX)
                .await
                .unwrap();
            let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(v["ok"], true);
            assert_eq!(v["deleted"], 7);

            // ② all=1 → None
            let r = app
                .clone()
                .oneshot(req(
                    "DELETE",
                    "/m/api/v1/sessions-archived?all=1",
                    Some("mam_device=arch"),
                    None,
                ))
                .await
                .unwrap();
            assert_eq!(r.status(), 200);
            let body = axum::body::to_bytes(r.into_body(), usize::MAX)
                .await
                .unwrap();
            let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(v["ok"], true);
            assert_eq!(v["deleted"], 7);

            // ③ 双参同在 → 仍 None（all 优先）
            let r = app
                .oneshot(req(
                    "DELETE",
                    "/m/api/v1/sessions-archived?all=1&session_id=s9",
                    Some("mam_device=arch"),
                    None,
                ))
                .await
                .unwrap();
            assert_eq!(r.status(), 200);
            let body = axum::body::to_bytes(r.into_body(), usize::MAX)
                .await
                .unwrap();
            let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(v["ok"], true);
            assert_eq!(v["deleted"], 7);

            let got = calls.lock().expect("测试单线程持锁");
            assert_eq!(got.len(), 3);
            assert_eq!(got[0].as_deref(), Some("s9"));
            assert_eq!(got[1], None);
            assert_eq!(got[2], None);
        }
    }

    // ==== 看板关闭/软归档（2026-09-20 体验批二）====
    mod board_close_hide_tests {
        use super::*;

        /// 固定活快照 + 过闸凭据的测试 state（缝默认假体，按需 Arc::get_mut 覆盖）
        fn state_with_sessions(sessions: Vec<crate::session::Session>) -> Arc<RemoteState> {
            let mut st = test_state();
            let s = std::sync::Arc::get_mut(&mut st).expect("test_state 独占引用");
            s.session_source = Box::new(move || crate::session::SessionsResponse {
                total_count: sessions.len(),
                waiting_count: 0,
                sessions: sessions.clone(),
            });
            persist_device(&st, "bh");
            st
        }

        /// App 形态会话夹具（inj_sess 默认 Cli，覆盖 form）
        fn app_session(id: &str, status: crate::session::SessionStatus) -> crate::session::Session {
            let mut s = inj_sess(id, crate::session::AgentType::Codex, 42, status);
            s.form = crate::session::ProcessForm::App;
            s
        }

        #[tokio::test]
        async fn sessions_hidden_green_is_filtered_and_counts_recomputed() {
            let sessions = vec![
                inj_sess(
                    "live-1",
                    crate::session::AgentType::Codex,
                    1,
                    crate::session::SessionStatus::Waiting,
                ),
                app_session("hidden-1", crate::session::SessionStatus::Idle),
            ];
            let mut st = state_with_sessions(sessions);
            {
                let s = std::sync::Arc::get_mut(&mut st).expect("独占");
                s.board_hidden_ids = Box::new(|| vec!["hidden-1".into()]);
            }
            let app = router(st);
            let r = app
                .oneshot(req(
                    "GET",
                    "/m/api/v1/sessions",
                    Some("mam_device=bh"),
                    None,
                ))
                .await
                .unwrap();
            assert_eq!(r.status(), 200);
            let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
            let arr = v["sessions"].as_array().unwrap();
            assert_eq!(arr.len(), 1, "绿态隐藏会话应被剔除");
            assert_eq!(arr[0]["id"], "live-1");
            assert_eq!(v["totalCount"], 1, "counts 按过滤后重算");
            assert_eq!(v["waitingCount"], 1);
        }

        #[tokio::test]
        async fn sessions_hidden_active_stays_hidden_and_no_unhide() {
            let sessions = vec![app_session(
                "busy-1",
                crate::session::SessionStatus::Processing,
            )];
            let mut st = state_with_sessions(sessions);
            let unhidden = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
            let u2 = unhidden.clone();
            {
                let s = std::sync::Arc::get_mut(&mut st).expect("独占");
                s.board_hidden_ids = Box::new(|| vec!["busy-1".into()]);
                s.board_hidden_unhide = std::sync::Arc::new(move |id| {
                    u2.lock().unwrap().push(id.to_string());
                    1
                });
            }
            let app = router(st);
            let r = app
                .oneshot(req(
                    "GET",
                    "/m/api/v1/sessions",
                    Some("mam_device=bh"),
                    None,
                ))
                .await
                .unwrap();
            assert_eq!(r.status(), 200);
            let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
            // 叉语义（体验批二修订）：非绿隐藏会话也保持剔除——初版「非绿∨未读
            // 即回归」被 W4 持久未读（转绿插行、24h 才过期）击穿，已废弃
            assert_eq!(v["sessions"].as_array().unwrap().len(), 0);
            assert_eq!(v["totalCount"], 0);
            assert!(
                unhidden.lock().unwrap().is_empty(),
                "无自动回归：解除隐藏缝不得被调用"
            );
        }

        #[tokio::test]
        async fn archived_includes_hidden_alive_first() {
            let sessions = vec![app_session(
                "alive-hidden",
                crate::session::SessionStatus::Idle,
            )];
            let mut st = test_state();
            {
                let s = std::sync::Arc::get_mut(&mut st).expect("独占");
                s.session_source = Box::new(move || crate::session::SessionsResponse {
                    total_count: 1,
                    waiting_count: 0,
                    sessions: sessions.clone(),
                });
                s.board_hidden_ids = Box::new(|| vec!["alive-hidden".into()]);
                let seen = (chrono::Utc::now() - chrono::Duration::seconds(3600)).to_rfc3339();
                s.archive_source = Box::new(move || {
                    vec![crate::database::SessionArchiveRow {
                        session_id: "dead-old".into(),
                        agent_type: "kimi".into(),
                        project_path: "/tmp/d".into(),
                        project_name: "dead-proj".into(),
                        title: None,
                        last_status: "idle".into(),
                        first_seen: seen.clone(),
                        last_seen: seen.clone(),
                        updated_at: String::new(),
                    }]
                });
                persist_device(&st, "bh");
            }
            let app = router(st);
            let r = app
                .oneshot(req(
                    "GET",
                    "/m/api/v1/sessions-archived?days=7",
                    Some("mam_device=bh"),
                    None,
                ))
                .await
                .unwrap();
            let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
            let arr = v["archived"].as_array().unwrap();
            assert_eq!(arr.len(), 2);
            assert_eq!(arr[0]["sessionId"], "alive-hidden", "hiddenAlive 排最前");
            assert_eq!(arr[0]["hiddenAlive"], true);
            assert_eq!(arr[1]["sessionId"], "dead-old");
            assert_eq!(arr[1]["hiddenAlive"], false);
        }

        #[tokio::test]
        async fn close_kills_cli_session_and_audits() {
            let sessions = vec![inj_sess(
                "cli-1",
                crate::session::AgentType::Claude,
                4242,
                crate::session::SessionStatus::Processing,
            )];
            let mut state = state_with_sessions(sessions);
            let killed = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u32>::new()));
            let k2 = killed.clone();
            {
                let s = std::sync::Arc::get_mut(&mut state).expect("独占");
                s.session_close = std::sync::Arc::new(move |pid| {
                    k2.lock().unwrap().push(pid);
                    Ok(())
                });
            }
            let app = router(state.clone());
            let r = app
                .oneshot(req(
                    "POST",
                    "/m/api/v1/session-close",
                    Some("mam_device=bh"),
                    Some(r#"{"sessionId":"cli-1"}"#),
                ))
                .await
                .unwrap();
            assert_eq!(r.status(), 200);
            assert_eq!(
                killed.lock().unwrap().as_slice(),
                [4242u32],
                "pid 取自活快照"
            );
            let audits = state
                .store
                .with(|c| crate::database::dao::write_audit::recent_conn(c, 10));
            assert_eq!(audits[0].action, "close");
            assert_eq!(audits[0].result, "ok");
            assert_eq!(audits[0].channel, "process");
            assert_eq!(audits[0].session_id, "cli-1");
        }

        #[tokio::test]
        async fn close_rejects_app_form() {
            let sessions = vec![app_session("app-1", crate::session::SessionStatus::Idle)];
            let state = state_with_sessions(sessions);
            let app = router(state);
            let r = app
                .oneshot(req(
                    "POST",
                    "/m/api/v1/session-close",
                    Some("mam_device=bh"),
                    Some(r#"{"sessionId":"app-1"}"#),
                ))
                .await
                .unwrap();
            assert_eq!(r.status(), 400, "App 形态杀不得（软归档走 /session-hide）");
        }

        #[tokio::test]
        async fn close_unknown_session_404() {
            let mut state = state_with_sessions(Vec::new());
            let killed = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u32>::new()));
            let k2 = killed.clone();
            {
                let s = std::sync::Arc::get_mut(&mut state).expect("独占");
                s.session_close = std::sync::Arc::new(move |pid| {
                    k2.lock().unwrap().push(pid);
                    Ok(())
                });
            }
            let app = router(state);
            let r = app
                .oneshot(req(
                    "POST",
                    "/m/api/v1/session-close",
                    Some("mam_device=bh"),
                    Some(r#"{"sessionId":"ghost"}"#),
                ))
                .await
                .unwrap();
            assert_eq!(r.status(), 404);
            assert!(killed.lock().unwrap().is_empty(), "未命中不出手");
        }

        #[tokio::test]
        async fn hide_app_session_marks_read_and_hides_any_status() {
            let sessions = vec![
                app_session("app-g", crate::session::SessionStatus::Idle),
                app_session("app-y", crate::session::SessionStatus::Processing),
            ];
            let mut state = state_with_sessions(sessions);
            let hid = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
            let h2 = hid.clone();
            let reads = std::sync::Arc::new(std::sync::Mutex::new(Vec::<(String, String)>::new()));
            let r2 = reads.clone();
            {
                let s = std::sync::Arc::get_mut(&mut state).expect("独占");
                s.board_hidden_hide = std::sync::Arc::new(move |id| {
                    h2.lock().unwrap().push(id.to_string());
                    1
                });
                s.unread_mark_read = std::sync::Arc::new(move |tool, id| {
                    r2.lock().unwrap().push((tool.to_string(), id.to_string()));
                });
            }
            let app = router(state);
            for sid in ["app-g", "app-y"] {
                let body = format!(r#"{{"sessionId":"{sid}"}}"#);
                let r = app
                    .clone()
                    .oneshot(req(
                        "POST",
                        "/m/api/v1/session-hide",
                        Some("mam_device=bh"),
                        Some(&body),
                    ))
                    .await
                    .unwrap();
                assert_eq!(r.status(), 200, "{sid} 任意状态可归档（叉不挑颜色）");
            }
            assert_eq!(
                hid.lock().unwrap().as_slice(),
                ["app-g".to_string(), "app-y".to_string()]
            );
            // 与桌面端「叉」同源：mark_read 删未读池行（tool, session_id）
            assert_eq!(
                reads.lock().unwrap().as_slice(),
                [
                    ("codex".to_string(), "app-g".to_string()),
                    ("codex".to_string(), "app-y".to_string())
                ]
            );
        }

        #[tokio::test]
        async fn hide_rejects_cli() {
            let sessions = vec![inj_sess(
                "cli-g",
                crate::session::AgentType::Claude,
                3,
                crate::session::SessionStatus::Idle,
            )];
            let state = state_with_sessions(sessions);
            let app = router(state);
            let r = app
                .oneshot(req(
                    "POST",
                    "/m/api/v1/session-hide",
                    Some("mam_device=bh"),
                    Some(r#"{"sessionId":"cli-g"}"#),
                ))
                .await
                .unwrap();
            assert_eq!(r.status(), 400, "CLI 会话走 /session-close，不得软归档");
        }

        #[tokio::test]
        async fn unhide_calls_seam_idempotent() {
            let mut state = state_with_sessions(Vec::new());
            let unhid = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
            let u2 = unhid.clone();
            {
                let s = std::sync::Arc::get_mut(&mut state).expect("独占");
                s.board_hidden_unhide = std::sync::Arc::new(move |id| {
                    u2.lock().unwrap().push(id.to_string());
                    1
                });
            }
            let app = router(state);
            let r = app
                .oneshot(req(
                    "POST",
                    "/m/api/v1/session-unhide",
                    Some("mam_device=bh"),
                    Some(r#"{"sessionId":"any-1"}"#),
                ))
                .await
                .unwrap();
            assert_eq!(r.status(), 200);
            assert_eq!(unhid.lock().unwrap().as_slice(), ["any-1".to_string()]);
        }
    }

    // ==== Task 4：session-open 归档回退（spec 2026-09-20-mobile-archive-history §6.2）====
    mod session_open_archive_fallback_tests {
        use super::*;
        use crate::database::SessionArchiveRow;

        /// 归档回退测试 state：活快照保持 test_state 默认（空会话集）——只换归档缝
        /// 与 spawn 缝。Arc::get_mut 就地换缝先例见 archive_api_tests::archive_state；
        /// 新路由结构性在 PIN gate 之后（nest 内层）：persist_device 播种 + req 带
        /// cookie 过闸（全文件先例；简报原始形态无凭据，同 Task 3 简报偏差修复 3）
        fn fallback_state(
            rows: Vec<SessionArchiveRow>,
            spawner: std::sync::Arc<crate::inject::resume::SpawnFn>,
        ) -> axum::Router {
            let mut st = test_state();
            let s = std::sync::Arc::get_mut(&mut st).expect("test_state 独占引用");
            s.archive_source = Box::new(move || rows.clone());
            s.resume_spawner = spawner;
            persist_device(&st, "arch");
            crate::remote::server::router(st)
        }

        /// 归档回退主路径：活快照未命中 + 登记表命中 → 构造 Session 走既有 resume
        /// 链——spawner 收到的载荷必须携带归档行 cwd（/tmp/proj-dead）与 resume 命令
        /// （codex resume dead-9，命令表 codex 条目产物），回执 200 opening，出手恰
        /// 一次（id 取自登记行而非远端输入的口径由实现侧保证：载荷命令里的是登记行
        /// session_id，若实现误回显远端输入，本测试输入与登记行同 id 无法区分——
        /// 双未命中 404 用例 + 实现注释守此口径）。
        /// 注：本测试在 Windows 上跑走 Windows 分支（open_session_terminal_with 按
        /// std::env::consts::OS 分派；macos_tcc 先例 :4614 同注），载荷断言按 cfg
        /// 分诊：macOS 断言 MacosApplescript 脚本全文、Windows 断言 SpawnSpec::Windows
        /// 的 cwd/args 字段——两分支断言语义同构（cwd + resume 命令落点），跨平台
        /// 均可编译（cfg! 运行时布尔，两分支全平台参与编译）。
        #[tokio::test]
        #[cfg_attr(
            not(any(windows, target_os = "macos")),
            ignore = "归档回退 resume 走注入/开窗链（platform 门）：仅 Windows/macOS"
        )]
        async fn dead_session_opens_from_archive() {
            let row = SessionArchiveRow {
                session_id: "dead-9".into(),
                agent_type: "codex".into(),
                project_path: "/tmp/proj-dead".into(),
                project_name: "proj-dead".into(),
                title: None,
                last_status: "idle".into(),
                first_seen: String::new(),
                last_seen: chrono::Utc::now().to_rfc3339(),
                updated_at: String::new(),
            };
            let fired = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let f2 = fired.clone();
            let spawner: std::sync::Arc<crate::inject::resume::SpawnFn> =
                std::sync::Arc::new(move |spec: &crate::inject::resume::SpawnSpec| {
                    f2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    if cfg!(windows) {
                        // Windows 分支：cwd 字段 = 归档行项目目录，args 携 resume 命令
                        // （wt 分支 `-d cwd cmd /k <resume>` / conhost 分支 `cmd /k`）
                        let crate::inject::resume::SpawnSpec::Windows { cwd, args, .. } = spec
                        else {
                            return Err("Windows 应收 Windows spec".into());
                        };
                        if cwd == "/tmp/proj-dead"
                            && args.iter().any(|a| a.contains("codex resume dead-9"))
                        {
                            Ok(())
                        } else {
                            Err(format!("payload 不含归档 cwd/resume 命令: {cwd} {args:?}"))
                        }
                    } else {
                        // macOS 分支：断言 AppleScript 脚本全文携带归档 cwd/resume 命令
                        let crate::inject::resume::SpawnSpec::MacosApplescript { script } = spec
                        else {
                            return Err("macOS 应收 MacosApplescript spec".into());
                        };
                        if !(script.contains("/tmp/proj-dead")
                            && script.contains("codex resume dead-9"))
                        {
                            return Err(format!("payload 不含归档 cwd/resume 命令: {script}"));
                        }
                        // 效果回查要真命中：种 argv 携特征的暗桩（见外层 spawn_effect_decoy 注）
                        super::spawn_effect_decoy("codex resume dead-9");
                        Ok(())
                    }
                });
            let app = fallback_state(vec![row], spawner);
            let r = app
                .oneshot(req(
                    "POST",
                    "/m/api/v1/session-open",
                    Some("mam_device=arch"),
                    Some(r#"{"sessionId":"dead-9"}"#),
                ))
                .await
                .unwrap();
            assert_eq!(r.status(), 200);
            let body = axum::body::to_bytes(r.into_body(), usize::MAX)
                .await
                .unwrap();
            let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(v["status"], "opening");
            assert_eq!(fired.load(std::sync::atomic::Ordering::SeqCst), 1);
        }

        /// 双未命中：活快照空 + 登记表空 → 既有 404 no_session 契约不破（归档回退
        /// 不得放宽未知 id 的拒绝口径）
        #[tokio::test]
        async fn neither_live_nor_archive_still_404() {
            let spawner: std::sync::Arc<crate::inject::resume::SpawnFn> =
                std::sync::Arc::new(|_: &crate::inject::resume::SpawnSpec| Ok(()));
            let app = fallback_state(Vec::new(), spawner);
            let r = app
                .oneshot(req(
                    "POST",
                    "/m/api/v1/session-open",
                    Some("mam_device=arch"),
                    Some(r#"{"sessionId":"ghost"}"#),
                ))
                .await
                .unwrap();
            assert_eq!(r.status(), 404);
        }
    }
    // ==== 移动端附件上传（2026-09-20）：落盘 <会话 cwd>/.tuvis-attachments/<会话>/ ====

    /// 附件端点测试状态：单会话、project_path 指向 tempdir（零真实目录污染）
    fn attach_state(project_path: std::path::PathBuf, empty_cwd: bool) -> Arc<RemoteState> {
        let mut session = inj_sess(
            "sess_att",
            crate::session::AgentType::Claude,
            21,
            crate::session::SessionStatus::Processing,
        );
        session.project_path = if empty_cwd {
            String::new()
        } else {
            project_path.to_string_lossy().into_owned()
        };
        Arc::new(RemoteState {
            ui_config_source: Box::new(|| None),
            subagent_source: std::collections::HashMap::new(),
            subagent_message_source: std::collections::HashMap::new(),
            target_evidence: no_target_evidence(),
            capability_table: crate::inject::capability::new_table(),
            session_source: Box::new(move || crate::session::SessionsResponse {
                sessions: vec![session.clone()],
                total_count: 1,
                waiting_count: 0,
            }),
            store: crate::remote::pairing::DeviceStore::memory(),
            injector: std::sync::Arc::new(crate::inject::engine::RealInjector),
            resume_spawner: std::sync::Arc::new(|_: &crate::inject::resume::SpawnSpec| Ok(())),
            // C6：create 缝束缺省 stub（任务簿+发现/pid/工具探测/步距缝），仅
            // session-create 用例就地覆盖
            create_hub: create_hub_stub(),
            confirm_probe: std::sync::Arc::new(|_, _, _| true),
            // 丁T3：本组测试的对话框在场探针缺省「无法判定」（None）——控制类注入
            // 照常投递；「在场即拒」的用例就地建 state 覆盖为假体（见 mode_switch_* 用例）
            dialog_probe: std::sync::Arc::new(|_, _| None),
            screen_probe: std::sync::Arc::new(|_, _| None),
            host_source: Box::new(|| {
                serde_json::json!({
                    "host": { "name": "t", "platform": "macos", "version": "0.0.0-test" },
                    "enabledTools": ["claude"]
                })
            }),
            message_source: Box::new(|_, _, _| Err("测试桩：未注入内容源".to_string())),
            path_source: Box::new(|_, _, _| (Vec::new(), false)),
            archive_source: Box::new(Vec::new),
            archive_delete: std::sync::Arc::new(|_: Option<&str>| 0usize),
            board_hidden_ids: Box::new(Vec::new),
            board_hidden_hide: std::sync::Arc::new(|_| 0usize),
            board_hidden_unhide: std::sync::Arc::new(|_| 0usize),
            unread_mark_read: std::sync::Arc::new(|_, _| ()),
            session_close: std::sync::Arc::new(|_| Ok(())),
            watcher_tx: tokio::sync::broadcast::channel(64).0,
            sse_registry: Arc::new(SseRegistry::default()),
            max_devices_source: Box::new(|| 3),
            pin_limiter: std::sync::Mutex::new(crate::remote::pin::PinRateLimiter::new()),
            global_pin_limiter: std::sync::Mutex::new(crate::remote::pin::PinRateLimiter::global()),
            pin_source: Box::new(|| Some("1234".to_string())),
            now_source: Box::new(|| chrono::Utc::now().timestamp_millis()),
            via_hosts_source: Box::new(|| None),
            rate_bucket_channels_source: Box::new(Vec::new),
            home_source: Box::new(|| None),
            // C7：配对计数缝缺省空表（既有用例零影响；配对打标/提示用例就地覆盖）
            pairing_counter: Box::new(Vec::new),
        })
    }

    fn attach_req(uri: &str, cookie: Option<&str>, body: Body) -> axum::http::Request<Body> {
        let mut b = axum::http::Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/octet-stream");
        if let Some(c) = cookie {
            b = b.header("cookie", c);
        }
        b.body(body).unwrap()
    }

    /// 附件 E2E 的项目目录：唯一性来自 `test_support`（pid + 进程内原子序号），
    /// **不再**用 `as_nanos()`——旧写法同一微秒内撞名，与同进程并行跑的
    /// `remote::attachments` 族互相 `remove_dir_all`（存量 flake 同根因，
    /// 见 `test_support` 模块文档）。
    fn attach_tempdir() -> crate::test_support::TestDir {
        let d = crate::test_support::temp_dir("mam-attach-e2e");
        std::fs::create_dir_all(d.join(".git").join("info")).unwrap();
        d
    }

    #[tokio::test]
    async fn attachment_upload_lands_in_project_dir_and_excludes_from_git() {
        let proj = attach_tempdir();
        // 单一 state 实例：设备注册与 router 必须同源（DeviceStore::memory 每个实例独立，
        // 分开构造会让注册的设备在 app 里不存在 → 403 假阴性）
        let state = attach_state(proj.path().to_path_buf(), false);
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state.clone());
        let r = app
            .oneshot(attach_req(
                "/m/api/v1/session-attachment?session_id=sess_att&name=shot.png",
                Some("mam_device=mm"),
                Body::from(b"\x89PNG fake".as_slice()),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let body = body_string(r).await;
        assert!(
            body.contains("\"path\"") && body.contains("\"size\":9"),
            "{body}"
        );
        // 落盘 = <cwd>/.tuvis-attachments/<session>/，**文件名保真**（5dc540a 用户
        // 裁决：移除纳秒+内容哈希前缀，保留原始名——文件池按名搜索、agent 识名
        // 依赖原名；仅同名才追加 (1)(2) 序号。本断言系 5dc540a 漏改，F4 订正）
        let dir = proj.join(".tuvis-attachments").join("sess_att");
        let entries: Vec<_> = std::fs::read_dir(&dir).unwrap().collect();
        assert_eq!(entries.len(), 1);
        let name = entries[0]
            .as_ref()
            .unwrap()
            .file_name()
            .to_string_lossy()
            .into_owned();
        assert_eq!(name, "shot.png", "原始文件名保真，无前缀污染");
        // git 本地排除：首份写入即幂等追加；二次上传不重复
        let exclude =
            std::fs::read_to_string(proj.join(".git").join("info").join("exclude")).unwrap();
        assert_eq!(exclude.matches(".tuvis-attachments/").count(), 1);
        // 二次上传：同一 state（设备注册仍有效），router 可重建
        let app = router(state.clone());
        let r = app
            .oneshot(attach_req(
                "/m/api/v1/session-attachment?session_id=sess_att&name=second.txt",
                Some("mam_device=mm"),
                Body::from("two"),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let exclude =
            std::fs::read_to_string(proj.join(".git").join("info").join("exclude")).unwrap();
        assert_eq!(exclude.matches(".tuvis-attachments/").count(), 1, "幂等");
    }

    #[tokio::test]
    async fn attachment_gate_and_error_contracts() {
        // 403：无设备 cookie（门禁防御）
        let proj = attach_tempdir();
        let state = attach_state(proj.path().to_path_buf(), false);
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state.clone());
        let r = app
            .oneshot(attach_req(
                "/m/api/v1/session-attachment?session_id=sess_att&name=a.png",
                None,
                Body::from("x"),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 403);
        // 404 no_session：未知会话
        let r = router(state.clone())
            .oneshot(attach_req(
                "/m/api/v1/session-attachment?session_id=nope&name=a.png",
                Some("mam_device=mm"),
                Body::from("x"),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 404);
        assert!(body_string(r).await.contains("no_session"));
        // 404 no_cwd：会话无项目目录（与 resume 同源口径）；empty-cwd state 需另注册设备
        let empty_state = attach_state(proj.path().to_path_buf(), true);
        persist_named_device(&empty_state, "mm", "测试设备");
        let r = router(empty_state)
            .oneshot(attach_req(
                "/m/api/v1/session-attachment?session_id=sess_att&name=a.png",
                Some("mam_device=mm"),
                Body::from("x"),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 404);
        assert!(body_string(r).await.contains("no_cwd"));
        // 413 too_large：超 20MB（DefaultBodyLimit 硬兜底）；复用已注册设备的 state
        let big = vec![0u8; crate::remote::attachments::MAX_ATTACHMENT_BYTES + 1];
        let r = router(state.clone())
            .oneshot(attach_req(
                "/m/api/v1/session-attachment?session_id=sess_att&name=a.bin",
                Some("mam_device=mm"),
                Body::from(big),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 413);
    }

    // ===== 丁T3 接入①：模式切换的对话框在场守卫（§2.7 裁8/9，问题 5）=====
    //
    // 端点守卫的落点是「投递前屏读」——真实屏读需要 conhost 目标（CI 不可观测），
    // 故经 `RemoteState.dialog_probe` 缝注入假体：在场形态用**真机屏幕原文**解析出的
    // 选项表（`inject::dialog` 夹具同源，来自 2026-09-21 探测档案 screen-t5-*），
    // 不在场/不可判定形态返回 None。四个用例把两路（shift+tab / slash）与三种探针
    // 形态都钉住。

    /// codex 计划批准框的**真机屏幕原文**（`screen-t5-codex-implement-before.txt`
    /// 行 24–29）→ 解析成选项表（在场假体的载荷）。解析失败即断言红——夹具不合法
    /// 时用例必须失败而不是静默放行。
    fn real_dialog_fixture() -> Vec<crate::inject::dialog::DialogOption> {
        let lines: Vec<String> = [
            "  Implement this plan?",
            "",
            "› 1. Yes, implement this plan          Switch to Default and start coding.",
            "  2. Yes, clear context and implement  Fresh thread. Context: 2% used.",
            "  3. No, stay in Plan mode             Continue planning with the model.",
            "",
            "  Press enter to confirm or esc to go back",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        crate::inject::dialog::parse_dialog_options(&lines)
            .expect("夹具必须是真机屏幕原文（能解析出编号选项簇）")
    }

    /// **Key 路（shift+tab）在场即拒**：claude 会话 + 假体报在场 → 409
    /// `blocked_by_dialog` + 中文回执；**零注入**（injector 两个记录表全空）+
    /// **零审计**（账只记真发生过的事）。
    #[tokio::test]
    async fn mode_switch_key_path_blocked_when_dialog_present() {
        let fake = FakeInjector::ok();
        let opts = real_dialog_fixture();
        let state = inject_state_with_dialog(
            fake.clone(),
            std::sync::Arc::new(move |_, _| Some(opts.clone())),
        );
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state.clone());
        let r = app
            .oneshot(req(
                "POST",
                "/m/api/v1/session-mode/switch",
                Some("mam_device=mm"),
                // sess_a = claude Waiting（shift+tab 路）
                Some(r#"{"sessionId":"sess_a","target":"plan"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(
            r.status(),
            409,
            "对话框在场 → 控制类注入必须被拒（409 与 no_mechanism 同用 CONFLICT）"
        );
        let body = body_string(r).await;
        assert!(
            body.contains("\"error\":\"blocked_by_dialog\""),
            "拒绝码须可程序分诊：{body}"
        );
        assert!(
            body.contains("终端有待决对话框，请先处理"),
            "中文回执须直给用户可读语义：{body}"
        );
        assert!(
            fake.recorded_keys().is_empty(),
            "拒绝必须零注入（shift+tab 未投递）：{:?}",
            fake.recorded_keys()
        );
        assert!(fake.recorded().is_empty(), "拒绝路径不得有任何文本注入");
        let audits = state
            .store
            .with(|c| crate::database::dao::write_audit::recent_conn(c, 10));
        assert!(
            audits.is_empty(),
            "拒绝必须零审计（校验失败不落账，与 approve/question 同口径）：{audits:?}"
        );
    }

    /// **Text 路（斜杠命令）同受此门**：codex 会话 + 假体报在场 → 同样 409 且零注入。
    /// 这条是「两路都在投递之前」的锁——若守卫被误挪到 Key 分支内，本用例即变红。
    #[tokio::test]
    async fn mode_switch_slash_path_blocked_when_dialog_present() {
        let fake = FakeInjector::ok();
        let opts = real_dialog_fixture();
        let state = inject_state_with_dialog(
            fake.clone(),
            std::sync::Arc::new(move |_, _| Some(opts.clone())),
        );
        persist_named_device(&state, "mm", "测试设备");
        // sess_q = codex Waiting（approve 族夹具已有；本组用 inject_state 的会话清单，
        // 故此处换用 sess_d：zcode → no_mechanism，不行；改走「会话快照里加 codex」）
        // ——为保持夹具单一来源，本用例直接复用 inject_state 的 sess_a 会话但**改工具**
        // 不可行（夹具是不可变的），故显式建造一个 codex 会话的 state。
        let codex_sess = inj_sess(
            "sess_t3b",
            crate::session::AgentType::Codex,
            71,
            crate::session::SessionStatus::Waiting,
        );
        let state2 = Arc::new(RemoteState {
            ui_config_source: Box::new(|| None),
            subagent_source: std::collections::HashMap::new(),
            subagent_message_source: std::collections::HashMap::new(),
            target_evidence: no_target_evidence(),
            capability_table: crate::inject::capability::new_table(),
            session_source: {
                let s = codex_sess.clone();
                Box::new(move || crate::session::SessionsResponse {
                    sessions: vec![s.clone()],
                    total_count: 1,
                    waiting_count: 0,
                })
            },
            store: crate::remote::pairing::DeviceStore::memory(),
            injector: fake.clone(),
            resume_spawner: std::sync::Arc::new(|_: &crate::inject::resume::SpawnSpec| Ok(())),
            // C6：create 缝束缺省 stub（任务簿+发现/pid/工具探测/步距缝），仅
            // session-create 用例就地覆盖
            create_hub: create_hub_stub(),
            confirm_probe: std::sync::Arc::new(|_, _, _| true),
            dialog_probe: {
                let opts = real_dialog_fixture();
                std::sync::Arc::new(move |_, _| Some(opts.clone()))
            },
            // 本用例只验控制类注入守卫（对话框在场即拒）——问答阶段机的屏读缝
            // 缺省「读不到屏」（该用例不走问答路径）
            screen_probe: std::sync::Arc::new(|_, _| None),
            host_source: Box::new(|| serde_json::Value::Null),
            message_source: Box::new(|_, _, _| Err("测试桩：未注入内容源".to_string())),
            path_source: Box::new(|_, _, _| (Vec::new(), false)),
            archive_source: Box::new(Vec::new),
            archive_delete: std::sync::Arc::new(|_: Option<&str>| 0usize),
            board_hidden_ids: Box::new(Vec::new),
            board_hidden_hide: std::sync::Arc::new(|_| 0usize),
            board_hidden_unhide: std::sync::Arc::new(|_| 0usize),
            unread_mark_read: std::sync::Arc::new(|_, _| ()),
            session_close: std::sync::Arc::new(|_| Ok(())),
            watcher_tx: tokio::sync::broadcast::channel(64).0,
            sse_registry: Arc::new(SseRegistry::default()),
            max_devices_source: Box::new(|| 3),
            pin_limiter: std::sync::Mutex::new(crate::remote::pin::PinRateLimiter::new()),
            global_pin_limiter: std::sync::Mutex::new(crate::remote::pin::PinRateLimiter::global()),
            pin_source: Box::new(|| Some("1234".to_string())),
            now_source: Box::new(|| chrono::Utc::now().timestamp_millis()),
            via_hosts_source: Box::new(|| None),
            rate_bucket_channels_source: Box::new(Vec::new),
            home_source: Box::new(|| None),
            // C7：配对计数缝缺省空表（既有用例零影响；配对打标/提示用例就地覆盖）
            pairing_counter: Box::new(Vec::new),
        });
        persist_named_device(&state2, "mm", "测试设备");
        let app = router(state2.clone());
        let r = app
            .oneshot(req(
                "POST",
                "/m/api/v1/session-mode/switch",
                Some("mam_device=mm"),
                // codex 的目标档 = 斜杠命令路（/plan）
                Some(r#"{"sessionId":"sess_t3b","target":"plan"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 409, "斜杠命令路同受对话框在场门");
        let body = body_string(r).await;
        assert!(body.contains("\"error\":\"blocked_by_dialog\""), "{body}");
        assert!(
            fake.recorded().is_empty() && fake.recorded_keys().is_empty(),
            "斜杠命令必须零注入（文本与回车都不发）：{:?}/{:?}",
            fake.recorded(),
            fake.recorded_keys()
        );
        let audits = state2
            .store
            .with(|c| crate::database::dao::write_audit::recent_conn(c, 10));
        assert!(audits.is_empty(), "零审计：{audits:?}");
    }

    /// **不在场正常投递**（回归锁：守卫不得把正常路径也拒掉）：假体返回 None →
    /// 既有行为原样（claude shift+tab 入 key_calls + 审计 action=mode result=ok）。
    /// 独占会话 `sess_t3c`（守卫 id 立规：INFLIGHT 按裸 id 全局占用，真投递的用例
    /// 必须各占唯一 id，否则并行跑会互相挤成「投递进行中」假红）。
    #[tokio::test]
    async fn mode_switch_proceeds_when_dialog_absent() {
        let fake = FakeInjector::ok();
        let (state, sid) = mode_guard_state(
            fake.clone(),
            "sess_t3c",
            crate::session::AgentType::Claude,
            72,
            std::sync::Arc::new(|_, _| None),
        );
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state.clone());
        let r = app
            .oneshot(req(
                "POST",
                "/m/api/v1/session-mode/switch",
                Some("mam_device=mm"),
                Some(&format!(r#"{{"sessionId":"{sid}","target":"plan"}}"#)),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "无对话框 → 照常投递");
        let body = body_string(r).await;
        assert!(body.contains("\"status\":\"key_sent\""), "{body}");
        // 丁T3：如实标注「本次是否真的检测过」——None ≠ 在场，回执不得暗示已检查
        assert!(
            body.contains("\"dialogChecked\":false"),
            "探针不可用时须如实标注未检测：{body}"
        );
        assert_eq!(
            fake.recorded_keys(),
            vec![(72u32, "shift+tab".to_string())],
            "shift+tab 键照常投递"
        );
        let audits = state
            .store
            .with(|c| crate::database::dao::write_audit::recent_conn(c, 10));
        assert_eq!(audits[0].action, "mode", "正常路径审计不变");
        assert_eq!(audits[0].result, "ok");
    }

    /// **在场检测成功 → 回执标注 dialogChecked:true**（与上一用例的 false 成对锁：
    /// 该字段必须真实反映探针是否给出结论，而不是恒 true/false 的装饰）。独占会话
    /// `sess_t3d`（守卫 id 立规，同上）。
    #[tokio::test]
    async fn mode_switch_reports_dialog_checked_when_probe_answered() {
        let fake = FakeInjector::ok();
        // 假体返回**空表**（构造上不可达——解析器下界 ≥2——但恰好用来表达「探针给出了
        // 答案（Some）且判不在场」这一格：blocks_control_injection(Some(&[])) == false）
        let (state, sid) = mode_guard_state(
            fake.clone(),
            "sess_t3d",
            crate::session::AgentType::Claude,
            73,
            std::sync::Arc::new(|_, _| Some(vec![])),
        );
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state.clone());
        let r = app
            .oneshot(req(
                "POST",
                "/m/api/v1/session-mode/switch",
                Some("mam_device=mm"),
                Some(&format!(r#"{{"sessionId":"{sid}","target":"plan"}}"#)),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let body = body_string(r).await;
        assert!(body.contains("\"dialogChecked\":true"), "{body}");
        assert_eq!(fake.recorded_keys(), vec![(73u32, "shift+tab".to_string())]);
    }

    // ===== 丁T3 F4-2：审批侧 dialog_probe 缝的两格自动化证据 =====
    //
    // 缺口（评审核实）：`read_dialog_options` 改经缝之后，`kimi_plan_approval_card_
    // never_emits_mapping_keys` 的 `v["dialog"]==true` 分支**只在有真实窗口的机器上
    // 才走得到**（CI 恒走 else）——缝的意义正是让屏读可测，故必须用假体把两格都钉住：
    // ① 缝给真机选项表 → GET 下发 `dialog:<n>` + `dialog=true`（T5 主路径）；
    // ② 缝给 None → 降级二元卡 + `degradedHint`（R1-3 防重警示，安全面）。
    // 两格都是真断言（键位不外泄 / 降级文案原文），不是「不 panic」。

    /// F4-2 格①：缝返回**真机屏幕原文**解析出的选项表（`real_dialog_fixture` =
    /// codex `Implement this plan?` 框，`screen-t5-codex-implement-before.txt` 行 24–29）
    /// → GET `/session-approve-options` 必须走 dialog 分支：`dialog=true` + 选项 id
    /// 全为 `dialog:<n>` + label 是屏上原文。
    #[tokio::test]
    async fn approve_dialog_branch_emits_dialog_options_via_probe() {
        let fake = FakeInjector::ok();
        let opts = real_dialog_fixture();
        let expected: Vec<(u32, String)> =
            opts.iter().map(|o| (o.number, o.label.clone())).collect();
        let state = approve_state_with_dialog(
            fake.clone(),
            Some(APPROVE_HIT_MSG),
            std::sync::Arc::new(move |_, _| Some(opts.clone())),
            None,
        );
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state.clone());
        let r = app
            .oneshot(req(
                "GET",
                "/m/api/v1/session-approve-options?session_id=sess_a",
                Some("mam_device=mm"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["available"], true);
        assert_eq!(v["dialog"], true, "缝给选项表 ⇒ dialog 分支必须成立");
        let options = v["options"].as_array().expect("options 应为数组");
        assert_eq!(
            options.len(),
            expected.len(),
            "选项数必须等于屏读解析出的编号项数"
        );
        for (o, (num, label)) in options.iter().zip(expected.iter()) {
            assert_eq!(
                o["id"].as_str().unwrap(),
                format!("dialog:{num}"),
                "dialog 分支的 id 形态（点按注入该数字）：{o}"
            );
            assert_eq!(
                o["label"].as_str().unwrap(),
                label.as_str(),
                "label 必须是屏上原文（T5 目标：把真实选项文本交给用户）：{o}"
            );
        }
        // 降级警示**不得**在 dialog 分支出现（R1-3 的警示条件是「命中审批但没读到
        // 对话框」——读到就不是降级态）
        assert_eq!(
            v["degradedHint"],
            serde_json::Value::Null,
            "读到选项时不叠加降级警示"
        );
    }

    /// 2026-10-04 计划批准卡：缝返回 **claude 计划批准框**选项表（带账本
    /// FEEDBACK_OPTION 锚的 "Tell Claude what to change"）→ GET 载荷必须下发
    /// `planDialog=true` + `feedbackOption="dialog:3"`（前端把该选项渲染为
    /// 「告诉 Claude 要改什么」反馈入口的依据）。
    #[tokio::test]
    async fn approve_dialog_plan_payload_flags_feedback_option() {
        let fake = FakeInjector::ok();
        let opts = vec![
            crate::inject::dialog::DialogOption {
                number: 1,
                label: "Yes, and use auto mode".into(),
                highlighted: true,
            },
            crate::inject::dialog::DialogOption {
                number: 2,
                label: "Yes, manually approve edits".into(),
                highlighted: false,
            },
            crate::inject::dialog::DialogOption {
                number: 3,
                label: "Tell Claude what to change".into(),
                highlighted: false,
            },
        ];
        let state = approve_state_with_dialog(
            fake.clone(),
            Some(APPROVE_HIT_MSG),
            std::sync::Arc::new(move |_, _| Some(opts.clone())),
            None,
        );
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state.clone());
        let r = app
            .oneshot(req(
                "GET",
                "/m/api/v1/session-approve-options?session_id=sess_a",
                Some("mam_device=mm"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["dialog"], true);
        assert_eq!(v["planDialog"], true, "带反馈锚的对话框 = 计划批准框");
        assert_eq!(
            v["feedbackOption"], "dialog:3",
            "反馈选项 id = 账本锚命中的屏上编号"
        );
    }

    /// 反向锁：缝给**非计划框**的普通对话框选项（无 FEEDBACK_OPTION 锚）→
    /// `planDialog=false` + `feedbackOption=null`（普通审批卡的渲染不受影响）。
    #[tokio::test]
    async fn approve_dialog_non_plan_payload_has_no_feedback_flags() {
        let fake = FakeInjector::ok();
        let opts = vec![
            crate::inject::dialog::DialogOption {
                number: 1,
                label: "允许".into(),
                highlighted: false,
            },
            crate::inject::dialog::DialogOption {
                number: 2,
                label: "拒绝".into(),
                highlighted: false,
            },
        ];
        let state = approve_state_with_dialog(
            fake.clone(),
            Some(APPROVE_HIT_MSG),
            std::sync::Arc::new(move |_, _| Some(opts.clone())),
            None,
        );
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state.clone());
        let r = app
            .oneshot(req(
                "GET",
                "/m/api/v1/session-approve-options?session_id=sess_a",
                Some("mam_device=mm"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["dialog"], true);
        assert_eq!(v["planDialog"], false);
        assert_eq!(v["feedbackOption"], serde_json::Value::Null);
    }

    /// 2026-10-04 审批卡不出修复：**claude 计划预期态 → 屏读出 1/2/3**。
    /// 实况取证（用户会话 bb92857b）：钩子标记 0 行、last_message 中文 prose
    /// （detect 恒 miss）、原生对话框标题不落 JSONL——三信号全灭致卡自隐。
    /// 修法后：尾部计划挂起（message_source 桩给 kind="plan" 尾页）即视为等审批
    /// → 屏读命中计划批准框 → available=true + dialog 选项 + planDialog/feedbackOption。
    #[tokio::test]
    async fn approve_card_appears_via_claude_plan_pending_with_dialog() {
        let fake = FakeInjector::ok();
        let opts = vec![
            crate::inject::dialog::DialogOption {
                number: 1,
                label: "Yes, and use auto mode".into(),
                highlighted: true,
            },
            crate::inject::dialog::DialogOption {
                number: 2,
                label: "Yes, manually approve edits".into(),
                highlighted: false,
            },
            crate::inject::dialog::DialogOption {
                number: 3,
                label: "Tell Claude what to change".into(),
                highlighted: false,
            },
        ];
        // last_message = 中文计划 prose（detect 不命中——实况形态）；标记表空
        let state = approve_state_with_dialog(
            fake.clone(),
            Some("我已经读完了《末班车》的全部版本。下面问你两道多选题，来确定优化方向："),
            std::sync::Arc::new(move |_, _| Some(opts.clone())),
            Some(vec![
                crate::remote::content::SessionMessage {
                    seq: 0,
                    role: "assistant".into(),
                    kind: "user".into(),
                    content: "继续优化计划".into(),
                    ts: Some(1000),
                    tool_name: None,
                    tool_args: None,
                    collapsed: false,
                },
                crate::remote::content::SessionMessage {
                    seq: 1,
                    role: "assistant".into(),
                    kind: "plan".into(),
                    content: "# 计划：创建 a.txt".into(),
                    ts: Some(2000),
                    tool_name: Some("ExitPlanMode".into()),
                    tool_args: None,
                    collapsed: false,
                },
            ]),
        );
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state.clone());
        let r = app
            .oneshot(req(
                "GET",
                "/m/api/v1/session-approve-options?session_id=sess_a",
                Some("mam_device=mm"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(
            v["available"], true,
            "计划预期态 = 等审批（修复前 false，卡自隐）"
        );
        assert_eq!(v["dialog"], true, "屏读命中计划批准框");
        assert_eq!(v["planDialog"], true);
        assert_eq!(v["feedbackOption"], "dialog:3");
        assert!(v["plan"].is_object(), "计划正文随卡聚合");
        let options = v["options"].as_array().unwrap();
        assert_eq!(options.len(), 3, "1/2/3 三个选项（用户诉求的选项卡）");
    }

    /// 反向档：计划预期态在场但**对话框没读到**（未绘制/非 Windows）→ 不给二元键
    /// （「允许='1'」对计划框是无效键，R1 实证），落「计划待确认」形态
    /// （available=true + planPending=true + 空 options + 计划正文）。
    #[tokio::test]
    async fn approve_card_claude_plan_pending_without_dialog_shows_pending_bar() {
        let fake = FakeInjector::ok();
        let state = approve_state_with_dialog(
            fake.clone(),
            Some("我已经读完了《末班车》的全部版本。"),
            std::sync::Arc::new(|_, _| None),
            Some(vec![crate::remote::content::SessionMessage {
                seq: 0,
                role: "assistant".into(),
                kind: "plan".into(),
                content: "# 计划：创建 a.txt".into(),
                ts: Some(2000),
                tool_name: Some("ExitPlanMode".into()),
                tool_args: None,
                collapsed: false,
            }]),
        );
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state.clone());
        let r = app
            .oneshot(req(
                "GET",
                "/m/api/v1/session-approve-options?session_id=sess_a",
                Some("mam_device=mm"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["available"], true);
        assert_eq!(v["planPending"], true);
        assert_eq!(v["dialog"], false);
        assert_eq!(
            v["degradedHint"],
            serde_json::Value::Null,
            "计划待确认形态不叠加二元错位警示"
        );
        let options = v["options"].as_array().unwrap();
        assert!(options.is_empty(), "零按钮——不给可能错位的二元键");
        assert!(v["plan"].is_object());
    }

    /// F4-2 格②：缝返回 `None`（CI/非 Windows/对话框未绘制）→ 降级二元卡 +
    /// `degradedHint`（R1-3 防重警示：二元键可能错位命中非预期选项）。
    /// 与格①成对：同一夹具、同一会话，只换探针结论。
    #[tokio::test]
    async fn approve_dialog_branch_degrades_with_hint_via_probe() {
        let fake = FakeInjector::ok();
        let state = approve_state_with_dialog(
            fake.clone(),
            Some(APPROVE_HIT_MSG),
            std::sync::Arc::new(|_, _| None),
            None,
        );
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state.clone());
        let r = app
            .oneshot(req(
                "GET",
                "/m/api/v1/session-approve-options?session_id=sess_a",
                Some("mam_device=mm"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["available"], true, "命中审批（映射存在且 detect 命中）");
        assert_eq!(v["dialog"], false, "缝给 None ⇒ 降级二元卡");
        let options = v["options"].as_array().unwrap();
        assert_eq!(options.len(), 2, "降级 = 映射表二元项（允许/拒绝）");
        assert!(
            options
                .iter()
                .all(|o| !o["id"].as_str().unwrap().starts_with("dialog:")),
            "降级路径不得夹带 dialog:<n>"
        );
        assert_eq!(
            v["degradedHint"],
            "未读到终端对话框选项——终端可能正显示多选项，二元键可能错位，建议到终端确认",
            "R1-3 防重警示必须下发（前端二元卡脚注）"
        );
    }

    /// 单会话 + 可注入对话框探针的 state（丁T3 模式守卫用例专用建造器：会话 id/工具/
    /// pid 与探针全部参数化——守卫 id 立规要求真投递用例各占唯一会话 id）。
    fn mode_guard_state(
        injector: std::sync::Arc<dyn crate::inject::engine::Injector>,
        sid: &str,
        tool: crate::session::AgentType,
        pid: u32,
        dialog_probe: std::sync::Arc<crate::remote::server::DialogProbeFn>,
    ) -> (Arc<RemoteState>, String) {
        let session = inj_sess(sid, tool, pid, crate::session::SessionStatus::Waiting);
        let sid_out = session.id.clone();
        let state = Arc::new(RemoteState {
            ui_config_source: Box::new(|| None),
            subagent_source: std::collections::HashMap::new(),
            subagent_message_source: std::collections::HashMap::new(),
            target_evidence: no_target_evidence(),
            capability_table: crate::inject::capability::new_table(),
            session_source: Box::new(move || crate::session::SessionsResponse {
                sessions: vec![session.clone()],
                total_count: 1,
                waiting_count: 0,
            }),
            store: crate::remote::pairing::DeviceStore::memory(),
            injector,
            resume_spawner: std::sync::Arc::new(|_: &crate::inject::resume::SpawnSpec| Ok(())),
            // C6：create 缝束缺省 stub（任务簿+发现/pid/工具探测/步距缝），仅
            // session-create 用例就地覆盖
            create_hub: create_hub_stub(),
            confirm_probe: std::sync::Arc::new(|_, _, _| true),
            dialog_probe,
            screen_probe: std::sync::Arc::new(|_, _| None),
            host_source: Box::new(|| serde_json::Value::Null),
            message_source: Box::new(|_, _, _| Err("测试桩：未注入内容源".to_string())),
            path_source: Box::new(|_, _, _| (Vec::new(), false)),
            archive_source: Box::new(Vec::new),
            archive_delete: std::sync::Arc::new(|_: Option<&str>| 0usize),
            board_hidden_ids: Box::new(Vec::new),
            board_hidden_hide: std::sync::Arc::new(|_| 0usize),
            board_hidden_unhide: std::sync::Arc::new(|_| 0usize),
            unread_mark_read: std::sync::Arc::new(|_, _| ()),
            session_close: std::sync::Arc::new(|_| Ok(())),
            watcher_tx: tokio::sync::broadcast::channel(64).0,
            sse_registry: Arc::new(SseRegistry::default()),
            max_devices_source: Box::new(|| 3),
            pin_limiter: std::sync::Mutex::new(crate::remote::pin::PinRateLimiter::new()),
            global_pin_limiter: std::sync::Mutex::new(crate::remote::pin::PinRateLimiter::global()),
            pin_source: Box::new(|| Some("1234".to_string())),
            now_source: Box::new(|| chrono::Utc::now().timestamp_millis()),
            via_hosts_source: Box::new(|| None),
            rate_bucket_channels_source: Box::new(Vec::new),
            home_source: Box::new(|| None),
            // C7：配对计数缝缺省空表（既有用例零影响；配对打标/提示用例就地覆盖）
            pairing_counter: Box::new(Vec::new),
        });
        (state, sid_out)
    }

    /// 丁T4 建造器：单会话 + **指定会话状态**（codex 运行中门用）+ 缺省探针。
    /// 与 [`mode_guard_state`] 同构，多一个 status 参数（其余缝同口径）。
    fn mode_state_with_status(
        sid: &str,
        tool: crate::session::AgentType,
        pid: u32,
        status: crate::session::SessionStatus,
    ) -> (Arc<RemoteState>, String) {
        let session = inj_sess(sid, tool, pid, status);
        let sid_out = session.id.clone();
        let state = Arc::new(RemoteState {
            ui_config_source: Box::new(|| None),
            subagent_source: std::collections::HashMap::new(),
            subagent_message_source: std::collections::HashMap::new(),
            target_evidence: no_target_evidence(),
            capability_table: crate::inject::capability::new_table(),
            session_source: Box::new(move || crate::session::SessionsResponse {
                sessions: vec![session.clone()],
                total_count: 1,
                waiting_count: 0,
            }),
            store: crate::remote::pairing::DeviceStore::memory(),
            injector: FakeInjector::ok(),
            resume_spawner: std::sync::Arc::new(|_: &crate::inject::resume::SpawnSpec| Ok(())),
            // C6：create 缝束缺省 stub（任务簿+发现/pid/工具探测/步距缝），仅
            // session-create 用例就地覆盖
            create_hub: create_hub_stub(),
            confirm_probe: std::sync::Arc::new(|_, _, _| true),
            dialog_probe: std::sync::Arc::new(|_, _| None),
            screen_probe: std::sync::Arc::new(|_, _| None),
            host_source: Box::new(|| serde_json::Value::Null),
            message_source: Box::new(|_, _, _| Err("测试桩：未注入内容源".to_string())),
            path_source: Box::new(|_, _, _| (Vec::new(), false)),
            archive_source: Box::new(Vec::new),
            archive_delete: std::sync::Arc::new(|_: Option<&str>| 0usize),
            board_hidden_ids: Box::new(Vec::new),
            board_hidden_hide: std::sync::Arc::new(|_| 0usize),
            board_hidden_unhide: std::sync::Arc::new(|_| 0usize),
            unread_mark_read: std::sync::Arc::new(|_, _| ()),
            session_close: std::sync::Arc::new(|_| Ok(())),
            watcher_tx: tokio::sync::broadcast::channel(64).0,
            sse_registry: Arc::new(SseRegistry::default()),
            max_devices_source: Box::new(|| 3),
            pin_limiter: std::sync::Mutex::new(crate::remote::pin::PinRateLimiter::new()),
            global_pin_limiter: std::sync::Mutex::new(crate::remote::pin::PinRateLimiter::global()),
            pin_source: Box::new(|| Some("1234".to_string())),
            now_source: Box::new(|| chrono::Utc::now().timestamp_millis()),
            via_hosts_source: Box::new(|| None),
            rate_bucket_channels_source: Box::new(Vec::new),
            home_source: Box::new(|| None),
            // C7：配对计数缝缺省空表（既有用例零影响；配对打标/提示用例就地覆盖）
            pairing_counter: Box::new(Vec::new),
        });
        (state, sid_out)
    }

    /// 丁T4 建造器：单会话（指定状态）+ **指定注入器**（两段式投递用例用）。
    fn mode_state_with_injector(
        injector: std::sync::Arc<dyn crate::inject::engine::Injector>,
        sid: &str,
        tool: crate::session::AgentType,
        pid: u32,
        status: crate::session::SessionStatus,
        dialog_probe: std::sync::Arc<crate::remote::server::DialogProbeFn>,
    ) -> (Arc<RemoteState>, String) {
        mode_state_with_screen(
            injector,
            sid,
            tool,
            pid,
            status,
            dialog_probe,
            // 缺省无屏读（CI/非 Windows 与既有用例的既有行为；D20 的屏读轮询窗需要
            // 它的用例走 mode_state_with_screen 显式注入脚本化屏序列）
            std::sync::Arc::new(|_, _| None),
        )
    }

    /// D20 建造器：在 [`mode_state_with_injector`] 之上多一个**屏读能力缝**
    /// （`RemoteState.screen_probe`）——回读轮询的脚本化屏序列从这里进。
    ///
    /// 为什么必须缝上：D20 之后**模式回读是一个轮询循环**（读几拍、命中即停、窗尽取
    /// 最后一次判定），它是本批的新判据；不缝屏读，这条控制流就只有真机能覆盖
    /// （本批已两次栽在这上面）。
    #[allow(clippy::too_many_arguments)] // 7 个（建造器的每个缝都要显式传；不再加）
    fn mode_state_with_screen(
        injector: std::sync::Arc<dyn crate::inject::engine::Injector>,
        sid: &str,
        tool: crate::session::AgentType,
        pid: u32,
        status: crate::session::SessionStatus,
        dialog_probe: std::sync::Arc<crate::remote::server::DialogProbeFn>,
        screen_probe: std::sync::Arc<crate::remote::server::ScreenProbeFn>,
    ) -> (Arc<RemoteState>, String) {
        let session = inj_sess(sid, tool, pid, status);
        let sid_out = session.id.clone();
        let state = Arc::new(RemoteState {
            ui_config_source: Box::new(|| None),
            subagent_source: std::collections::HashMap::new(),
            subagent_message_source: std::collections::HashMap::new(),
            target_evidence: no_target_evidence(),
            capability_table: crate::inject::capability::new_table(),
            session_source: Box::new(move || crate::session::SessionsResponse {
                sessions: vec![session.clone()],
                total_count: 1,
                waiting_count: 0,
            }),
            store: crate::remote::pairing::DeviceStore::memory(),
            injector,
            resume_spawner: std::sync::Arc::new(|_: &crate::inject::resume::SpawnSpec| Ok(())),
            // C6：create 缝束缺省 stub（任务簿+发现/pid/工具探测/步距缝），仅
            // session-create 用例就地覆盖
            create_hub: create_hub_stub(),
            confirm_probe: std::sync::Arc::new(|_, _, _| true),
            dialog_probe,
            screen_probe,
            host_source: Box::new(|| serde_json::Value::Null),
            message_source: Box::new(|_, _, _| Err("测试桩：未注入内容源".to_string())),
            path_source: Box::new(|_, _, _| (Vec::new(), false)),
            archive_source: Box::new(Vec::new),
            archive_delete: std::sync::Arc::new(|_: Option<&str>| 0usize),
            board_hidden_ids: Box::new(Vec::new),
            board_hidden_hide: std::sync::Arc::new(|_| 0usize),
            board_hidden_unhide: std::sync::Arc::new(|_| 0usize),
            unread_mark_read: std::sync::Arc::new(|_, _| ()),
            session_close: std::sync::Arc::new(|_| Ok(())),
            watcher_tx: tokio::sync::broadcast::channel(64).0,
            sse_registry: Arc::new(SseRegistry::default()),
            max_devices_source: Box::new(|| 3),
            pin_limiter: std::sync::Mutex::new(crate::remote::pin::PinRateLimiter::new()),
            global_pin_limiter: std::sync::Mutex::new(crate::remote::pin::PinRateLimiter::global()),
            pin_source: Box::new(|| Some("1234".to_string())),
            now_source: Box::new(|| chrono::Utc::now().timestamp_millis()),
            via_hosts_source: Box::new(|| None),
            rate_bucket_channels_source: Box::new(Vec::new),
            home_source: Box::new(|| None),
            // C7：配对计数缝缺省空表（既有用例零影响；配对打标/提示用例就地覆盖）
            pairing_counter: Box::new(Vec::new),
        });
        (state, sid_out)
    }

    /// D20：脚本化屏读缝（第 i 次读返回 `screens[i]`；用尽后**重复最后一屏**——
    /// 与 `inject::mode::tests::run_readback_script` 同语义：模拟「屏不再变」）。
    /// `None` 元素 = 那一拍读不到屏（读屏失败/平台无屏读）。
    fn scripted_screen_probe(
        screens: Vec<Option<Vec<String>>>,
    ) -> std::sync::Arc<crate::remote::server::ScreenProbeFn> {
        let pos = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        std::sync::Arc::new(move |_sid: &str, _pid: u32| {
            let i = pos.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let idx = i.min(screens.len().saturating_sub(1));
            screens.get(idx).and_then(|s| s.clone())
        })
    }

    fn screen_lines(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    // ===== 丁T4：模式栏二维结构 / 回读全开 / 组切换 / 运行中门 =====
    //
    // 本组用例的共同前提：**CI/非 Windows 下屏读恒 None** → GET 的 `current` 为 null。
    // 这对本组无碍：T4 的新面是**结构表与组路由**（纯表 + 请求面），屏读词表本身由
    // `inject::mode` 的单测覆盖（夹具 = T6 探测档案的真机屏幕原文逐字快照）。

    /// GET /session-mode 下发**结构表与两组**（裁5 的接口面）：codex 两组；模式组
    /// readback=true；权限组 2026-10-09 起回读源 = 最新事件行（readback=true），
    /// 屏读缝恒 None → 回落记忆、无记忆仍 current=null。裁7 的退役档也在载荷里
    /// （`legacy`）——**前端不渲染为按钮**，由 `tests/mobile/ModeBar.test.tsx` 锁住。
    #[tokio::test]
    async fn session_mode_reports_two_axis_structure_for_codex() {
        let (state, sid) = mode_state_with_status(
            "sess_t4_get_codex",
            crate::session::AgentType::Codex,
            81,
            crate::session::SessionStatus::Waiting,
        );
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state);
        let r = app
            .oneshot(req(
                "GET",
                &format!("/m/api/v1/session-mode?session_id={sid}"),
                Some("mam_device=mm"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["tool"], "codex");
        assert_eq!(v["structure"], "twoAxis");
        let groups = v["groups"].as_array().expect("groups 必须是数组");
        assert_eq!(groups.len(), 2, "二维家出两组（裁5）");
        assert_eq!(groups[0]["id"], "mode");
        assert_eq!(groups[1]["id"], "permission");
        // 权限组：2026-10-09 起回读源 = 最新事件行（readback=true）——本测试屏读缝
        // 恒 None（事件行不在屏/滚出）→ current=null 并回落记忆通道（无记忆仍 null，
        // 「屏读→记忆→未知」三级如实，spec §3.3）
        assert_eq!(groups[1]["readback"], true);
        assert!(groups[1]["current"].is_null());
        assert_eq!(groups[0]["readback"], true);
        // 档位：模式组 [操作(可选), 计划(可选)] = shift+tab toggle；权限组 [只读, 默认, 完全信任]
        assert_eq!(groups[0]["layout"], "toggle", "codex 模式组=单钮 toggle");
        assert_eq!(
            groups[1]["layout"], "picker",
            "codex 权限组=单选面板（2026-09-23 用户方案：读回终端菜单选项供用户点选）"
        );
        let mode_tiers = groups[0]["tiers"].as_array().unwrap();
        assert_eq!(mode_tiers[0]["mode"], "default");
        assert_eq!(mode_tiers[0]["label"], "操作");
        assert_eq!(mode_tiers[0]["selectable"], true);
        assert!(
            mode_tiers[0]["reason"].is_null(),
            "可选档不带 reason（如实回执的反面是不乱贴标签）"
        );
        assert_eq!(mode_tiers[1]["mode"], "plan");
        assert_eq!(mode_tiers[1]["selectable"], true);
        let perm_tiers = groups[1]["tiers"].as_array().unwrap();
        assert_eq!(
            perm_tiers.len(),
            4,
            "2026-09-23 起四档（含自动审批=Approve for me）"
        );
        assert_eq!(
            perm_tiers
                .iter()
                .map(|t| t["label"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["只读", "默认", "自动审批", "完全信任"]
        );
        // 裁7：退役档在 legacy 里如实登记，**不在 tiers 里**
        let legacy = groups[1]["legacy"]
            .as_array()
            .expect("codex 权限组带 legacy");
        assert_eq!(legacy.len(), 2);
        assert_eq!(legacy[0]["label"], "untrusted");
        assert_eq!(legacy[1]["label"], "on-failure");
        assert!(
            !perm_tiers
                .iter()
                .any(|t| matches!(t["mode"].as_str(), Some("untrusted") | Some("on-failure"))),
            "退役档不得作为可选档出现在 tiers 里（裁7）"
        );
        // 旧前端兼容视图仍在（顶层 current/switchKind）
        assert_eq!(v["switchKind"], "slashCommand");
        assert!(v["current"].is_null());
    }

    /// GET：权限组 current 来自「**上次切换**」记忆（2026-09-23 codex 模式切换改造
    /// 的显示面）——verified 切换写入 `PERMISSION_TIER_MEMORY` 后，GET 把它回放为
    /// 权限组的 current/currentLabel；无记录的会话权限组仍恒 null（模式未知，如实）。
    #[tokio::test]
    async fn session_mode_reports_permission_tier_from_memory() {
        let (state, sid) = mode_state_with_status(
            "sess_mc_get_mem",
            crate::session::AgentType::Codex,
            92,
            crate::session::SessionStatus::Waiting,
        );
        persist_named_device(&state, "mm", "测试设备");
        // 无记录：权限组恒 null（不假装知道）
        let app = router(state.clone());
        let r = app
            .oneshot(req(
                "GET",
                &format!("/m/api/v1/session-mode?session_id={sid}"),
                Some("mam_device=mm"),
                None,
            ))
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert!(v["groups"][1]["current"].is_null(), "无记忆 → 模式未知");
        // 终审 P1-2：来源随载荷下发——都无 = "null"（前端不标注「上次切换」）
        assert_eq!(v["groups"][1]["currentSource"], "null", "{v}");

        // 记忆后：GET 回放为 current/currentLabel（模拟一次 verified=true 的切换）
        crate::remote::api::remember_permission_tier(&state.store, &sid, "readOnly");
        let app = router(state);
        let r = app
            .oneshot(req(
                "GET",
                &format!("/m/api/v1/session-mode?session_id={sid}"),
                Some("mam_device=mm"),
                None,
            ))
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["groups"][1]["current"], "readOnly");
        assert_eq!(v["groups"][1]["currentLabel"], "只读");
        // 终审 P1-2：屏读缝 None（无屏）→ 记忆回落 → 来源 = "memory"
        // （前端「上次切换」标注的唯一判据）
        assert_eq!(v["groups"][1]["currentSource"], "memory", "{v}");
    }

    /// GET（终审 P1-2）：权限组屏读命中时 current 来源 = **"screen"**——
    /// codex 权限轴唯一屏读源 = 最新回执行（`parse_axis_from_screen`）；回执行在屏
    /// 时即使有记忆也以屏为准（屏读 → 记忆 → null 三级回落，spec §3.3），来源如实
    /// 随载荷下发。
    #[tokio::test]
    async fn session_mode_permission_current_source_screen_when_receipt_on_screen() {
        let fake = FakeInjector::ok();
        let probe = scripted_screen_probe(vec![Some(screen_lines(&[
            "  普通输出",
            "• Permission selection requested: Read Only",
            "› Ask Codex to do anything",
        ]))]);
        let (state, sid) = mode_state_with_screen(
            fake,
            "sess_fr1_perm_src",
            crate::session::AgentType::Codex,
            94,
            crate::session::SessionStatus::Waiting,
            std::sync::Arc::new(|_, _| None),
            probe,
        );
        persist_named_device(&state, "mm", "测试设备");
        // 记忆里是旧档（readOnly 之外再种一个 bypass）——屏读命中时**以屏为准**
        crate::remote::api::remember_permission_tier(&state.store, &sid, "bypass");
        let r = router(state)
            .oneshot(req(
                "GET",
                &format!("/m/api/v1/session-mode?session_id={sid}"),
                Some("mam_device=mm"),
                None,
            ))
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(
            v["groups"][1]["current"], "readOnly",
            "屏读命中（最新回执行）优先于记忆：{v}"
        );
        assert_eq!(v["groups"][1]["currentSource"], "screen", "{v}");
    }

    /// 终审 P0-1 回归锁：switch 端点的**权限档记忆写入**。
    ///
    /// 回归史：T4 重构把回执段拆成两臂后，Pending 臂（kimi 两段式 / codex 数字直达
    /// 等非闭环路）提前 return，丢掉了旧公共尾的 `remember_permission_tier` 写入
    /// （kimi 权限切换 verified 后 GET 无「上次切换」可回放）；唯一残存写入点在
    /// codex toggle 臂内、被 `group == Permission` 守卫恒 false（死码，toggle 臂只在
    /// group==Mode 时进入）。
    ///
    /// 为什么不是纯端点测试：菜单路投递要真 conhost 屏读（`menu_stages` 内的
    /// `read_screen_lines` 不走 screen_probe 缝），端点级单测驱动不到
    /// 「verified=true」——正向写入判定抽
    /// [`remember_permission_tier_if_verified`] 单点锁判定表，「写入 → GET 回放」
    /// 链走端点（不是直接种 PERMISSION_TIER_MEMORY——既有 GET 回落测试直接种数据，
    /// 掩盖了本回归）；端点级另锁「失败不写」（见下一个测试）。
    #[tokio::test]
    async fn session_mode_verified_permission_writes_memory_and_get_replays() {
        let (state, sid) = mode_state_with_status(
            "sess_fr1_perm_mem",
            crate::session::AgentType::Codex,
            93,
            crate::session::SessionStatus::Waiting,
        );
        persist_named_device(&state, "mm", "测试设备");
        // 判定表：verified=false × 权限组 → 不写（不假成功）
        crate::remote::api::remember_permission_tier_if_verified(
            false,
            crate::inject::mode::ModeGroupId::Permission,
            &state.store,
            &sid,
            crate::inject::mode::MamMode::Bypass,
        );
        assert!(
            crate::remote::api::recall_permission_tier(&state.store, &sid).is_none(),
            "verified=false 不得写权限档记忆"
        );
        // 判定表：verified=true × 模式组 → 不写（Mode 组不是 Permission 组，spec §3.1）
        crate::remote::api::remember_permission_tier_if_verified(
            true,
            crate::inject::mode::ModeGroupId::Mode,
            &state.store,
            &sid,
            crate::inject::mode::MamMode::Plan,
        );
        assert!(
            crate::remote::api::recall_permission_tier(&state.store, &sid).is_none(),
            "Mode 组零写入"
        );
        // 判定表：verified=true × 权限组 → 写；GET 端点回放「上次切换」（写入→回放链）
        crate::remote::api::remember_permission_tier_if_verified(
            true,
            crate::inject::mode::ModeGroupId::Permission,
            &state.store,
            &sid,
            crate::inject::mode::MamMode::Bypass,
        );
        let app = router(state);
        let r = app
            .oneshot(req(
                "GET",
                &format!("/m/api/v1/session-mode?session_id={sid}"),
                Some("mam_device=mm"),
                None,
            ))
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["groups"][1]["current"], "bypass", "GET 回放记忆档：{v}");
        assert_eq!(v["groups"][1]["currentLabel"], "完全信任", "{v}");
    }

    /// 终审 P0-1 配套：switch 端点**失败路零记忆写入**（不假成功的负向面）——
    /// kimi 权限组 Bypass 两段式在假体（无真 conhost）必然失败（同
    /// `..._kimi_permission_bypass_is_menu_two_stage_not_blind_confirm` 的形态），
    /// 回执 failed 后「上次切换」必须仍为空。还原动作（变异）：把写入从
    /// verified 判定里摘出来（失败也写）→ 本测试先红。
    #[tokio::test]
    async fn session_mode_switch_failed_permission_does_not_write_memory() {
        let fake = FakeInjector::ok();
        let (state, sid) = mode_state_with_injector(
            fake.clone(),
            "sess_fr1_perm_fail",
            crate::session::AgentType::Kimi,
            98,
            crate::session::SessionStatus::Idle,
            std::sync::Arc::new(|_, _| None),
        );
        persist_named_device(&state, "mm", "测试设备");
        let r = router(state.clone())
            .oneshot(req(
                "POST",
                "/m/api/v1/session-mode/switch",
                Some("mam_device=mm"),
                Some(&format!(
                    r#"{{"sessionId":"{sid}","target":"bypass","group":"permission"}}"#
                )),
            ))
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["status"], "failed", "假体下菜单路必然如实失败：{v}");
        assert!(
            crate::remote::api::recall_permission_tier(&state.store, &sid).is_none(),
            "failed 切换不得写权限档记忆：{v}"
        );
    }

    /// 终审 P0-1：picker Done 的权限档记忆**回填**判定（抽
    /// [`remember_permission_tier_from_screen`] 单点——pick 端点同样要真 conhost
    /// 读缝，端点级驱动不了 Done）。反查 = 屏面自底向上**最新可解析**回执行
    /// （0.160.0 词形）；0.154 旧锚行不进解析 → 不写（不猜档）。
    #[test]
    fn picker_done_backfill_takes_newest_parseable_event_line() {
        let store = crate::remote::pairing::DeviceStore::memory();
        // 底部 6 行窗：旧行（Read Only）在上、最新行（Full Access）在下 → 取最新
        let screen = screen_lines(&[
            "  普通输出",
            "• Permission selection requested: Read Only",
            "• Permission selection requested: Full Access",
        ]);
        crate::remote::api::remember_permission_tier_from_screen(
            &store,
            "sess_fr1_pick_done",
            &screen,
        );
        assert_eq!(
            crate::remote::api::recall_permission_tier(&store, "sess_fr1_pick_done"),
            Some(crate::inject::mode::MamMode::Bypass),
            "自底向上取最新回执行（不是首条）"
        );
        // 无回执行 / 认不出的档 → 不写（既有记忆不被冲掉）
        crate::remote::api::remember_permission_tier_from_screen(
            &store,
            "sess_fr1_pick_done",
            &screen_lines(&["  普通输出", "Update Model Permissions"]),
        );
        assert_eq!(
            crate::remote::api::recall_permission_tier(&store, "sess_fr1_pick_done"),
            Some(crate::inject::mode::MamMode::Bypass),
            "屏面无可解析回执行 → 保持既有记忆"
        );
        // 0.154 旧锚行（`permissions updated to`）不进事件行解析 → 不写（不猜档）
        crate::remote::api::remember_permission_tier_from_screen(
            &store,
            "sess_fr1_pick_done_old",
            &screen_lines(&["• Permissions updated to Full Access"]),
        );
        assert!(
            crate::remote::api::recall_permission_tier(&store, "sess_fr1_pick_done_old").is_none(),
            "0.154 旧锚行不在反查判据内 → 如实无记忆"
        );
    }

    /// GET：kimi 两组 + **权限组的屏显标签是工具自己的词**（§2.6 kimi 列）
    #[tokio::test]
    async fn session_mode_reports_kimi_permission_labels() {
        let (state, sid) = mode_state_with_status(
            "sess_t4_get_kimi",
            crate::session::AgentType::Kimi,
            82,
            crate::session::SessionStatus::Idle,
        );
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state);
        let r = app
            .oneshot(req(
                "GET",
                &format!("/m/api/v1/session-mode?session_id={sid}"),
                Some("mam_device=mm"),
                None,
            ))
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["structure"], "twoAxis");
        let perm = &v["groups"][1];
        assert_eq!(perm["id"], "permission");
        assert_eq!(perm["label"], "权限");
        assert_eq!(
            perm["tiers"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| t["label"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["总是询问", "按需询问", "永不询问"],
            "kimi 权限组屏显标签 = 该工具自己的词（不是 兔维斯 通用名）"
        );
        // 三档全部可选（「总是询问」走两段式；另两档有直达变体）
        assert!(perm["tiers"]
            .as_array()
            .unwrap()
            .iter()
            .all(|t| t["selectable"] == true));
        assert!(perm["legacy"].as_array().unwrap().is_empty());
    }

    /// GET：单轴家（opencode）只有一组，且档位标签已按裁6 术语对齐
    #[tokio::test]
    async fn session_mode_reports_single_axis_for_opencode() {
        let (state, sid) = mode_state_with_status(
            "sess_t4_get_oc",
            crate::session::AgentType::OpenCode,
            83,
            crate::session::SessionStatus::Idle,
        );
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state);
        let r = app
            .oneshot(req(
                "GET",
                &format!("/m/api/v1/session-mode?session_id={sid}"),
                Some("mam_device=mm"),
                None,
            ))
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["structure"], "singleAxis");
        assert_eq!(v["groups"].as_array().unwrap().len(), 1);
        assert_eq!(v["groups"][0]["step"], true, "shift+tab 一步一档");
        assert_eq!(
            v["groups"][0]["tiers"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| t["label"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["默认", "计划"],
            "裁6：Build 改显「默认」"
        );
    }

    /// GET：未实测工具 → structure="none" + 空 groups（前端不渲染）
    #[tokio::test]
    async fn session_mode_reports_none_for_untested_tool() {
        let (state, sid) = mode_state_with_status(
            "sess_t4_get_wb",
            crate::session::AgentType::WorkBuddy,
            84,
            crate::session::SessionStatus::Idle,
        );
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state);
        let r = app
            .oneshot(req(
                "GET",
                &format!("/m/api/v1/session-mode?session_id={sid}"),
                Some("mam_device=mm"),
                None,
            ))
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["structure"], "none");
        assert!(v["groups"].as_array().unwrap().is_empty());
        assert_eq!(v["switchKind"], "unsupported");
    }

    /// POST：**codex 模式组 shift+tab toggle**（2026-09-23 用户实测裁决；2026-10-10
    /// 用户指令改版——**零投递闸移除**）——显式 `group:"mode"` + `target:"plan"|"default"`
    /// → 无论 target 是什么**都只投递一次 `shift+tab` 键**（无斜杠命令、无额外回车；
    /// target 不再作前读闸，仅在前读不可判时作核验预期兜底）；核验预期 = **终端前读
    /// 真值的翻转**（本测前读 = Default → 预期 Plan），落点由**基线差分核验**
    /// （T4-F3：前读 → 发键 → 内容集差分轮询，`MODE_SWITCH_VERIFY_POLL_TOTAL_MS` 窗）
    /// 在投递闭包内闭环——屏序列用假缝脚本化（前读=Default 夹具、核验拍出现**新**
    /// Plan 事件行 → 命中）。两个 target 各打一轮 = 「卡面 target 过期也必然动作」
    /// 的新语义锁（实机日志 11:13-15 六次零投递正是旧「前读==target 零投递」闸的
    /// 根因——本测改写后该分支不复存在）。CI 无屏读 → 前读**零投递**如实拒
    /// （spec §3.1「读不到屏 → 如实拒」分支，由
    /// `..._no_screen_refuses_zero_delivery` 单独锁）。
    #[tokio::test]
    async fn session_mode_switch_codex_mode_group_sends_shift_tab() {
        // 屏脚本（槽序 = 读序）：① lookup 的 before 快照（既有纪律）→ ② toggle
        // 内核的 read_pre → ③ 核验拍。pre 屏 = Default 态（底 3 行 = 模型行+快捷键
        // 行+composer，` · ` 缺席推断 Default；事件行在 composer 区之上——真实
        // 终端形态，不进状态栏窗）；after 屏 = 状态栏翻到 Plan（模型行尾 Plan mode
        // 短语）+ **新** Plan 事件行（内容集差分双证据）。
        let pre = screen_lines(&[
            "  普通输出",
            "• Model changed to deepseek-v4.1-flash high for Default mode.",
            "  deepseek-v4.1-flash high · ~\\proj",
            "  ← for agents · ? for shortcuts",
            "› Ask Codex to do anything",
        ]);
        let after = screen_lines(&[
            "  普通输出",
            "• Model changed to glm-5.3-flash max for Plan mode.",
            "• Model changed to deepseek-v4.1-flash high for Default mode.",
            "  deepseek-v4.1-flash high · ~\\proj                    Plan mode",
            "  ← for agents · ? for shortcuts",
            "› Ask Codex to do anything",
        ]);
        // 两个 target 都必然发键（新语义）：target=plan（卡面与终端一致）与
        // target=default（卡面过期——旧实现这里会零投递，现在照发 shift+tab，
        // 预期 = 前读翻转 Plan，不受 target 影响）。
        for target in ["plan", "default"] {
            let fake = FakeInjector::ok();
            let probe = scripted_screen_probe(vec![
                Some(pre.clone()),
                Some(pre.clone()),
                Some(after.clone()),
            ]);
            let (state, sid) = mode_state_with_screen(
                fake.clone(),
                "sess_mc_mode_toggle",
                crate::session::AgentType::Codex,
                91,
                crate::session::SessionStatus::Waiting,
                std::sync::Arc::new(|_, _| None),
                probe,
            );
            persist_named_device(&state, "mm", "测试设备");
            let app = router(state.clone());
            let r = app
                .oneshot(req(
                    "POST",
                    "/m/api/v1/session-mode/switch",
                    Some("mam_device=mm"),
                    Some(&format!(
                        r#"{{"sessionId":"{sid}","target":"{target}","group":"mode"}}"#
                    )),
                ))
                .await
                .unwrap();
            assert_eq!(r.status(), 200);
            let body = body_string(r).await;
            let v: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert!(
                fake.recorded().is_empty(),
                "模式组 shift+tab 不投递任何文本：{body}"
            );
            assert_eq!(
                fake.recorded_keys(),
                vec![(91u32, "shift+tab".to_string())],
                "target={target} 也无条件发一次 shift+tab（零投递闸已移除）：{body}"
            );
            assert_eq!(
                v["verified"], true,
                "预期=前读翻转（Plan）且新事件行命中 → verified：{body}"
            );
            assert_eq!(
                v["observed"], "plan",
                "observed = 末拍屏读档（m-6 新字段）：{body}"
            );
            // 审计照记（action=mode；新语义无 zero-key 标注态）
            let audits = state
                .store
                .with(|c| crate::database::dao::write_audit::recent_conn(c, 10));
            assert_eq!(audits[0].action, "mode", "{body}");
            assert!(
                audits[0].summary.contains("切换模式至"),
                "审计摘要 = 切换动作本身：{:?}",
                audits[0].summary
            );
        }
    }

    /// POST：codex 模式组 **CI 无屏读 → 前读闸零投递如实拒**（spec §3.1 第一分支）：
    /// 无屏读探针 → Err「读不到屏」→ status=failed、零键零文本、审计如实记录失败。
    /// 还原动作（变异）：把前读闸删掉（读不到屏也盲发 shift+tab）→ 本测试先红。
    #[tokio::test]
    async fn session_mode_switch_codex_mode_group_no_screen_refuses_zero_delivery() {
        let fake = FakeInjector::ok();
        let (state, sid) = mode_state_with_injector(
            fake.clone(),
            "sess_mc_toggle_noscreen",
            crate::session::AgentType::Codex,
            911,
            crate::session::SessionStatus::Waiting,
            std::sync::Arc::new(|_, _| None),
        );
        persist_named_device(&state, "mm", "测试设备");
        let r = router(state)
            .oneshot(req(
                "POST",
                "/m/api/v1/session-mode/switch",
                Some("mam_device=mm"),
                Some(&format!(
                    r#"{{"sessionId":"{sid}","target":"plan","group":"mode"}}"#
                )),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let body = body_string(r).await;
        assert!(
            fake.recorded().is_empty() && fake.recorded_keys().is_empty(),
            "读不到屏零投递（不盲发）：{body}"
        );
        assert!(
            body.contains("\"status\":\"failed\"") && body.contains("读不到屏"),
            "如实拒：{body}"
        );
    }

    /// POST：**显式 group 路由**（丁T4 新增字段）——codex 权限组「完全信任」→ 走
    /// `/permissions` 两段式的**第一段**（文本 + tab——提交键 2026-10-10 3787fa7d
    /// 用户实测指令 enter→tab；第二段无真屏读 → 中止并如实回执）
    /// 平台门控（2026-10-07 存量债清理）：本测断言的是「两段式的**第一段照常投递**」，
    /// 而第一段之后的菜单定位/导航**必须屏读**，屏读是 **Windows 能力**——
    /// `remote/api.rs::menu_stages` 在 `not(windows)` 下按设计恒回
    /// Err「本平台无屏读，权限菜单无法定位」（如实回执，不盲发）。故该行为在非 Windows
    /// 上**不可能发生**，属平台不适用而非行为回归；门控取严 = 原注入平台门
    /// （inject/routing.rs：仅 Windows/macOS 可注入）∩ 屏读能力（仅 Windows）。
    #[tokio::test]
    #[cfg_attr(
        not(windows),
        ignore = "屏读是 Windows 能力：非 Windows 下 menu_stages 恒回 Err「本平台无屏读，权限菜单无法定位」，第一段投递不可发生（平台不适用）"
    )]
    async fn session_mode_switch_routes_explicit_permission_group() {
        let fake = FakeInjector::ok();
        let (state, sid) = mode_state_with_injector(
            fake.clone(),
            "sess_t4_perm",
            crate::session::AgentType::Codex,
            85,
            crate::session::SessionStatus::Waiting,
            std::sync::Arc::new(|_, _| None),
        );
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state);
        let r = app
            .oneshot(req(
                "POST",
                "/m/api/v1/session-mode/switch",
                Some("mam_device=mm"),
                Some(&format!(
                    r#"{{"sessionId":"{sid}","target":"bypass","group":"permission"}}"#
                )),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let body = body_string(r).await;
        // 第一段已投递：`/permissions` 文本 + tab 键
        assert_eq!(
            fake.recorded(),
            vec![(85u32, "/permissions".to_string())],
            "两段式的第一段必须投递开启命令：{body}"
        );
        assert_eq!(
            fake.recorded_keys(),
            vec![(85u32, "tab".to_string())],
            "斜杠命令提交键 = tab（3787fa7d 用户实测指令 enter→tab）"
        );
        // 第二段（CI 无屏读）→ 中止 + 如实回执（**不是**「已切换」）
        assert!(
            body.contains("\"status\":\"failed\""),
            "第二段读不到菜单必须如实失败：{body}"
        );
        assert!(
            body.contains("请人工核对终端"),
            "失败文案要讲清「命令已发、档位未切」：{body}"
        );
    }

    /// POST：**旧客户端不带 group** —— codex 的 `default` 由后端推断到模式组
    /// （2026-09-23 起 Default 两组都可选 → 歧义消解与 kimi 同规：取模式组，
    /// shift+tab toggle；见 `resolve_group` 文档与 `group_inference_rules`）。
    /// T4-F3 后模式组 Key 路 = codex toggle 闭环（前读闸需要屏读）——屏脚本：
    /// 前读 = Plan 态（`Plan mode` 短语在状态栏窗）≠ 目标 default → 发键 → 核验拍
    /// 状态栏翻 Default（` · ` 形态）→ verified（本测关注组推断与投递本身）。
    #[tokio::test]
    async fn session_mode_switch_infers_group_for_legacy_client() {
        let fake = FakeInjector::ok();
        // 前读屏 = Plan 态（底 3 行窗内模型行尾带 Plan mode 短语）；核验拍 = 状态栏
        // 翻 Default（` · ` 形态、无模式字样 → 缺席推断）。槽序：① before 快照
        // ② read_pre ③ 核验拍（命中即停）。
        let pre = screen_lines(&[
            "  普通输出",
            "• Model changed to deepseek-v4.1-flash high for Default mode.",
            "  deepseek-v4.1-flash high · ~\\proj                    Plan mode",
            "  ← for agents · ? for shortcuts",
            "› Ask Codex to do anything",
        ]);
        let after = screen_lines(&[
            "  普通输出",
            "• Model changed to deepseek-v4.1-flash high for Default mode.",
            "  deepseek-v4.1-flash high · ~\\proj",
            "  ← for agents · ? for shortcuts",
            "› Ask Codex to do anything",
        ]);
        let probe = scripted_screen_probe(vec![Some(pre.clone()), Some(pre), Some(after)]);
        let (state, sid) = mode_state_with_screen(
            fake.clone(),
            "sess_t4_infer",
            crate::session::AgentType::Codex,
            86,
            crate::session::SessionStatus::Waiting,
            std::sync::Arc::new(|_, _| None),
            probe,
        );
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state.clone());
        let r = app
            .oneshot(req(
                "POST",
                "/m/api/v1/session-mode/switch",
                Some("mam_device=mm"),
                Some(&format!(r#"{{"sessionId":"{sid}","target":"default"}}"#)),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let body = body_string(r).await;
        // 推断到模式组 → shift+tab 被投递（Key 路，无斜杠命令）
        assert!(fake.recorded().is_empty(), "{body}");
        assert_eq!(
            fake.recorded_keys(),
            vec![(86u32, "shift+tab".to_string())],
            "{body}"
        );
        // 审计摘要含**组名**（二维工具的组是语义的一部分）
        let audits = state
            .store
            .with(|c| crate::database::dao::write_audit::recent_conn(c, 10));
        assert_eq!(audits[0].action, "mode");
        assert!(
            audits[0].summary.contains("模式"),
            "二维家审计摘要要带组名（否则分不清是模式组还是权限组的默认）：{:?}",
            audits[0].summary
        );
    }

    /// POST：**退役档不可选**（裁7 的输入面）——POST `untrusted` → 400（零注入零审计）
    #[tokio::test]
    async fn session_mode_switch_rejects_legacy_enum() {
        let fake = FakeInjector::ok();
        let (state, sid) = mode_state_with_injector(
            fake.clone(),
            "sess_t4_legacy",
            crate::session::AgentType::Codex,
            87,
            crate::session::SessionStatus::Waiting,
            std::sync::Arc::new(|_, _| None),
        );
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state.clone());
        for target in ["untrusted", "on-failure"] {
            let r = app
                .clone()
                .oneshot(req(
                    "POST",
                    "/m/api/v1/session-mode/switch",
                    Some("mam_device=mm"),
                    Some(&format!(r#"{{"sessionId":"{sid}","target":"{target}"}}"#)),
                ))
                .await
                .unwrap();
            assert_eq!(r.status(), 400, "退役档不得作为可选档（裁7）：{target}");
        }
        assert!(fake.recorded().is_empty(), "拒绝路径零注入");
        assert!(fake.recorded_keys().is_empty());
        let audits = state
            .store
            .with(|c| crate::database::dao::write_audit::recent_conn(c, 10));
        assert!(audits.is_empty(), "拒绝路径零审计：{audits:?}");
    }

    /// POST：**组里没有的档** → 409 no_mechanism（kimi 模式组没有只读档）
    #[tokio::test]
    async fn session_mode_switch_rejects_tier_outside_group() {
        let fake = FakeInjector::ok();
        let (state, sid) = mode_state_with_injector(
            fake.clone(),
            "sess_t4_outgroup",
            crate::session::AgentType::Kimi,
            88,
            crate::session::SessionStatus::Waiting,
            std::sync::Arc::new(|_, _| None),
        );
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state);
        let r = app
            .oneshot(req(
                "POST",
                "/m/api/v1/session-mode/switch",
                Some("mam_device=mm"),
                Some(&format!(
                    r#"{{"sessionId":"{sid}","target":"readOnly","group":"mode"}}"#
                )),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 409);
        let body = body_string(r).await;
        assert!(body.contains("\"error\":\"no_mechanism\""), "{body}");
        assert!(fake.recorded().is_empty() && fake.recorded_keys().is_empty());
    }

    /// **codex `/plan` 运行中不可用 → 如实回执**（§2.6 表末）：会话 Processing →
    /// 200 failed + 中文说明，**零注入零审计**（codex 自己也会拒，兔维斯 提前拦）。
    /// 还原动作：删掉 `codex_plan_busy` 那道门 → 本断言先红（会变成投递 `/plan`）。
    #[tokio::test]
    async fn session_mode_switch_reports_codex_plan_busy() {
        let fake = FakeInjector::ok();
        let (state, sid) = mode_state_with_injector(
            fake.clone(),
            "sess_t4_busy",
            crate::session::AgentType::Codex,
            89,
            crate::session::SessionStatus::Processing,
            std::sync::Arc::new(|_, _| None),
        );
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state.clone());
        let r = app
            .oneshot(req(
                "POST",
                "/m/api/v1/session-mode/switch",
                Some("mam_device=mm"),
                Some(&format!(r#"{{"sessionId":"{sid}","target":"plan"}}"#)),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let body = body_string(r).await;
        assert!(body.contains("\"status\":\"failed\""), "{body}");
        assert!(
            body.contains("运行中不接受模式切换"),
            "回执要讲清「为什么没切」（如实，不是静默失败）：{body}"
        );
        assert!(
            fake.recorded().is_empty() && fake.recorded_keys().is_empty(),
            "运行中门必须在投递之前：{:?}/{:?}",
            fake.recorded(),
            fake.recorded_keys()
        );
        let audits = state
            .store
            .with(|c| crate::database::dao::write_audit::recent_conn(c, 10));
        assert!(
            audits.is_empty(),
            "零投递零审计（与忙让位同口径）：{audits:?}"
        );
    }

    /// codex 运行中门**只管 Plan 档**：同一 Processing 会话切权限组 → 照常出手
    /// （权限菜单的可用性与回合状态无关；未实测有同类限制 → 不扩张）。
    /// 平台门控（2026-10-07 存量债清理）：本测断言「运行中门只拦 Plan 档 → 权限组
    /// 照常出手」，而权限组出手 = 走两段式的第一段，第二段的菜单定位**必须屏读**
    /// （Windows 能力）——非 Windows 下 `menu_stages` 恒回 Err「本平台无屏读」，
    /// 记录里不会有 `/permissions`。属平台不适用而非行为回归。
    #[tokio::test]
    #[cfg_attr(
        not(windows),
        ignore = "屏读是 Windows 能力：非 Windows 下 menu_stages 恒回 Err「本平台无屏读，权限菜单无法定位」，权限组出手不可发生（平台不适用）"
    )]
    async fn session_mode_switch_codex_busy_only_blocks_plan() {
        let fake = FakeInjector::ok();
        let (state, sid) = mode_state_with_injector(
            fake.clone(),
            "sess_t4_busy_perm",
            crate::session::AgentType::Codex,
            90,
            crate::session::SessionStatus::Processing,
            std::sync::Arc::new(|_, _| None),
        );
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state);
        let r = app
            .oneshot(req(
                "POST",
                "/m/api/v1/session-mode/switch",
                Some("mam_device=mm"),
                Some(&format!(
                    r#"{{"sessionId":"{sid}","target":"bypass","group":"permission"}}"#
                )),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(
            fake.recorded(),
            vec![(90u32, "/permissions".to_string())],
            "运行中门只拦 Plan 档（权限组照常出手）"
        );
    }

    /// **两段式的第二段不重入守卫**（丁T4 的核心设计点）：第一段之前的守卫恰好调用
    /// **一次**，第一段照常投递——若第二段重入守卫，屏上刚打开的菜单会被判成
    /// 「待决对话框」→ 409 blocked_by_dialog（自相矛盾：权限档永远切不了）。
    ///
    /// **本用例能证明什么**：探针只在**第一段之前**被调用一次（守卫位）→ 断「调用
    /// 次数恰好 1」+「第一段照常投递」。若有人在第二段前再插一次探针，计数变 2 → 先红。
    /// 平台门控（2026-10-07 存量债清理）：本测断言「第一段照常投递 + 守卫恰好调用一次」，
    /// 而投递第一段的**前提**是菜单路径可行——菜单定位/导航必须屏读（Windows 能力），
    /// 非 Windows 下 `menu_stages` 恒回 Err「本平台无屏读，权限菜单无法定位」，
    /// 记录为空 ⇒ 断言不可满足。属平台不适用而非行为回归。
    #[tokio::test]
    #[cfg_attr(
        not(windows),
        ignore = "屏读是 Windows 能力：非 Windows 下 menu_stages 恒回 Err「本平台无屏读，权限菜单无法定位」，第一段投递不可发生（平台不适用）"
    )]
    async fn session_mode_switch_menu_guard_runs_once_before_first_stage() {
        let fake = FakeInjector::ok();
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let calls_probe = calls.clone();
        let (state, sid) = mode_state_with_injector(
            fake.clone(),
            "sess_t4_guard_once",
            crate::session::AgentType::Codex,
            91,
            crate::session::SessionStatus::Waiting,
            std::sync::Arc::new(move |_, _| {
                // 第一次（也是唯一一次）报**不在场**：用户点按钮时终端上没有别人的
                // 对话框；第一段之后出现的菜单**不经过探针**（这正是设计点）
                calls_probe.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                None
            }),
        );
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state);
        let r = app
            .oneshot(req(
                "POST",
                "/m/api/v1/session-mode/switch",
                Some("mam_device=mm"),
                Some(&format!(
                    r#"{{"sessionId":"{sid}","target":"readOnly","group":"permission"}}"#
                )),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "第一段不得被自己的菜单拦下");
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "守卫在位恰好调用一次（每请求一道门；第二段不重入）"
        );
        assert_eq!(
            fake.recorded(),
            vec![(91u32, "/permissions".to_string())],
            "第一段照常投递"
        );
        assert_eq!(
            fake.recorded_keys(),
            vec![(91u32, "tab".to_string())],
            "提交键 = tab（3787fa7d enter→tab）"
        );
    }

    /// **第一段之前有真对话框 → 仍被守卫拒**（回归锁：守卫位置不得因为两段式改造
    /// 而漂移）。用真机屏幕原文（codex `Implement this plan?`）作在场假体。
    #[tokio::test]
    async fn session_mode_switch_menu_blocked_when_foreign_dialog_present() {
        let fake = FakeInjector::ok();
        let opts = real_dialog_fixture();
        let (state, sid) = mode_state_with_injector(
            fake.clone(),
            "sess_t4_menu_blocked",
            crate::session::AgentType::Codex,
            92,
            crate::session::SessionStatus::Waiting,
            std::sync::Arc::new(move |_, _| Some(opts.clone())),
        );
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state.clone());
        let r = app
            .oneshot(req(
                "POST",
                "/m/api/v1/session-mode/switch",
                Some("mam_device=mm"),
                Some(&format!(
                    r#"{{"sessionId":"{sid}","target":"bypass","group":"permission"}}"#
                )),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 409, "两段式也要过第一段前的那道守卫");
        let body = body_string(r).await;
        assert!(body.contains("\"error\":\"blocked_by_dialog\""), "{body}");
        assert!(
            fake.recorded().is_empty() && fake.recorded_keys().is_empty(),
            "拒绝 = 零注入（含第一段的开启命令）"
        );
        let audits = state
            .store
            .with(|c| crate::database::dao::write_audit::recent_conn(c, 10));
        assert!(audits.is_empty(), "零审计：{audits:?}");
    }

    // ===== D20：模式回读的**动态轮询**（宪法 §5.(c) 第 8 条 / 计划 §2.9）=====
    //
    // 本组用例把**端点侧**的回读路径（含缝接线与回执合成）钉在门禁里：内核的轮询语义
    // 由 `inject::mode::tests::readback_poll_*` 覆盖，这里补的是「端点用不用它、用对
    // 没有、回执怎么念」。屏序列经 `RemoteState.screen_probe` 缝注入（零 conhost）。

    /// **用户实机观察①的回归锁（端点侧）**：claude 切档后**第一拍读到旧档**、后续拍读到
    /// 目标档 → 回执必须是 `verified=true`（而不是旧实现的「回读与预期不符」）。
    ///
    /// 夹具用 claude 真机底栏原文（T6 探测档案逐字）。会话切档前是**默认档**
    /// （`manual mode on`），目标 = `plan` → 机制是 shift+tab 一步，按实测环序
    /// `[AcceptEdits, Plan, Bypass, Default]` 推算应到档 = **接受编辑**
    /// （`⏵⏵ accept edits on`）——故重绘后的那一拍必须是这一条，才叫「命中」。
    ///
    /// 还原动作（变异①）：把端点改回「单次读」（不调 `poll_mode_readback`，只读一次屏）
    /// → 本测试先红（`verified` 会是 false、hint 会是「与预期不符」）。
    #[tokio::test]
    async fn session_mode_switch_readback_polls_until_target_mode_appears() {
        let fake = FakeInjector::ok();
        let stale = screen_lines(&["  ⏸ manual mode on · ? for shortuts ·←for agents"]);
        let fresh = screen_lines(&["  ⏵⏵ accept edits on (shift+tab to cycle) · ← for agents"]);
        // 屏序列：① 切档前的 before（注入前的单次快照，D20(c) 例外）读到默认档；
        // ② 投递后第一拍仍是旧档（重绘未及——观察① 的形态）；③ 第二拍拍到目标档
        let probe = scripted_screen_probe(vec![Some(stale.clone()), Some(stale), Some(fresh)]);
        let (state, sid) = mode_state_with_screen(
            fake.clone(),
            "sess_d20_readback_poll",
            crate::session::AgentType::Claude,
            93,
            crate::session::SessionStatus::Idle,
            std::sync::Arc::new(|_, _| None),
            probe,
        );
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state);
        let r = app
            .oneshot(req(
                "POST",
                "/m/api/v1/session-mode/switch",
                Some("mam_device=mm"),
                Some(&format!(r#"{{"sessionId":"{sid}","target":"plan"}}"#)),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(fake.recorded_keys(), vec![(93u32, "shift+tab".to_string())]);
        assert_eq!(
            v["verified"], true,
            "第一拍旧档、第二拍目标档 → 必须确认成功（观察①的修复形态）：{v}"
        );
        assert!(v["hint"].is_null(), "命中态不得带 hint（不假装失败）：{v}");
        assert_eq!(
            v["current"], "acceptEdits",
            "回执的 current 取**最后一拍**读数：{v}"
        );
    }

    /// **窗尽如实区分三态（端点侧文案）**：同一条端点、同样的投递，屏上「一直是旧档」
    /// （不符）与「一直读不到屏」（未及确认）必须给出**两句不同的话**——D20(b) 明令
    /// 不得退化成统一的「失败」。
    ///
    /// 还原动作（变异②）：把 `mode_verify_receipt` 的 `Mismatch` / `Unverifiable` 两支
    /// 合并成同一句 hint → 本测试的 `assert_ne!` 与两处关键词断言先红。
    #[tokio::test]
    async fn session_mode_switch_readback_window_end_keeps_three_states_distinct() {
        let fake = FakeInjector::ok();
        let stale = screen_lines(&["  ⏸ manual mode on · ? for shortuts ·←for agents"]);

        // ① 窗内一直是旧档 → 「不符」：必须报出预期/实际两边（预期 = 环序推算的 AcceptEdits）
        let probe = scripted_screen_probe(vec![Some(stale.clone())]); // 用尽后重复 = 屏不变
        let (state, sid) = mode_state_with_screen(
            fake.clone(),
            "sess_d20_mismatch",
            crate::session::AgentType::Claude,
            94,
            crate::session::SessionStatus::Idle,
            std::sync::Arc::new(|_, _| None),
            probe,
        );
        persist_named_device(&state, "mm", "测试设备");
        let r = router(state)
            .oneshot(req(
                "POST",
                "/m/api/v1/session-mode/switch",
                Some("mam_device=mm"),
                Some(&format!(r#"{{"sessionId":"{sid}","target":"plan"}}"#)),
            ))
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["verified"], false);
        let mismatch_hint = v["hint"].as_str().unwrap().to_string();
        assert!(
            mismatch_hint.contains("预期「接受编辑」") && mismatch_hint.contains("实际「默认」"),
            "「不符」要把两边都报出来（预期 = 按实测环序推算的应到档）：{mismatch_hint}"
        );
        assert_eq!(
            v["current"], "default",
            "窗尽时的 current = 最后一拍读到的档（不是 null）：{v}"
        );

        // ② 窗内始终读不到屏 → 「未及确认」：另一句话，且 current 必须是 null
        let probe = scripted_screen_probe(vec![None]);
        let (state2, sid2) = mode_state_with_screen(
            fake.clone(),
            "sess_d20_unverifiable",
            crate::session::AgentType::Claude,
            95,
            crate::session::SessionStatus::Idle,
            std::sync::Arc::new(|_, _| None),
            probe,
        );
        persist_named_device(&state2, "mm", "测试设备");
        let r = router(state2)
            .oneshot(req(
                "POST",
                "/m/api/v1/session-mode/switch",
                Some("mam_device=mm"),
                Some(&format!(r#"{{"sessionId":"{sid2}","target":"plan"}}"#)),
            ))
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["verified"], false);
        let unverifiable_hint = v["hint"].as_str().unwrap().to_string();
        assert!(
            unverifiable_hint.contains("无法自动确认"),
            "「未及确认」说的是「读不到」（另一句话）：{unverifiable_hint}"
        );
        assert!(
            !unverifiable_hint.contains("与预期不符"),
            "读不到 **不是**「不符」（D20(b)：两态不得混同）：{unverifiable_hint}"
        );
        assert!(
            v["current"].is_null(),
            "一格都没读到 → current 必须为 null（不给过期值）：{v}"
        );
        assert_ne!(
            mismatch_hint, unverifiable_hint,
            "两种超时的回执文案必须不同（D20(b)）"
        );
    }

    /// **命中即停、窗尽有界**（端点侧的量）：屏序列第 2 拍即读到目标档 → 缝的读数**恰好
    /// 3 次**（① before 快照 1 次 + 回读 2 拍），而不是把 15 拍的窗睡满。
    ///
    /// 这条同时钉住 D20(a) 的「禁止用固定睡眠替代轮询」在**端点侧**也成立：若有人把
    /// 轮询换回「睡满窗再读一次」，读数次数会退化到 1（且本断言先红）。
    #[tokio::test]
    async fn session_mode_switch_readback_stops_at_first_hit_within_window() {
        let fake = FakeInjector::ok();
        let stale = screen_lines(&["  ⏸ manual mode on · ? for shortuts ·←for agents"]);
        let fresh = screen_lines(&["  ⏵⏵ accept edits on (shift+tab to cycle) · ← for agents"]);
        let reads = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let reads_for_probe = reads.clone();
        let pos = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let probe: std::sync::Arc<crate::remote::server::ScreenProbeFn> =
            std::sync::Arc::new(move |_sid: &str, _pid: u32| {
                reads_for_probe.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let i = pos.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                // 0 = before 快照（旧档）；1 = 回读首拍（仍是旧档，重绘未及）；
                // 2 起 = 目标档（命中 → 停止轮询，不再读）
                if i <= 1 {
                    Some(stale.clone())
                } else {
                    Some(fresh.clone())
                }
            });
        let (state, sid) = mode_state_with_screen(
            fake.clone(),
            "sess_d20_readback_stop",
            crate::session::AgentType::Claude,
            96,
            crate::session::SessionStatus::Idle,
            std::sync::Arc::new(|_, _| None),
            probe,
        );
        persist_named_device(&state, "mm", "测试设备");
        let r = router(state)
            .oneshot(req(
                "POST",
                "/m/api/v1/session-mode/switch",
                Some("mam_device=mm"),
                Some(&format!(r#"{{"sessionId":"{sid}","target":"plan"}}"#)),
            ))
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["verified"], true, "{v}");
        assert_eq!(
            reads.load(std::sync::atomic::Ordering::SeqCst),
            3,
            "① before 快照 + ② 回读两拍（第二拍命中即停）——窗共 15 拍，不许睡满"
        );
    }

    /// **kimi 权限组全两段式**（2026-10-06 探针会话 2.1.1 定案）：`/auto`（Bypass，
    /// `/yolo` 同理）的第一段 = **文本 + 提交回车①**——2.1.1 实测：键入文本只出行内
    /// 自动补全，回车①执行命令后完整菜单才打开并停留；确认回车②由闭环导航发（屏读
    /// 确认高亮才发）。历史备注：曾误诊「回车消费 picker」改为纯文本首段（5744e45），
    /// 探针证实纯文本态菜单永不出现——已回退，本测试钉回退后的正确形态。
    ///
    /// 假体环境屏读不可用（菜单轮询的真 conhost 读失败）→ 恰好钉住**不盲发红线**：
    /// 注入动作 = `/auto` 文本 + 命令提交回车①，**到此为止**——无导航键、无第二次
    /// 确认回车；回执 `status=failed` 且说明「命令已发送」请人工核对。屏读探针读数
    /// = **1**（before 快照；投递未成功 → 回读段不启动）。
    ///
    /// 还原动作：把 kimi 第一段改回纯文本（不带回车）→ 本测试先红（keys 变空）。
    #[tokio::test]
    async fn session_mode_switch_kimi_permission_bypass_is_menu_two_stage_not_blind_confirm() {
        let fake = FakeInjector::ok();
        let reads = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let reads_for_probe = reads.clone();
        let probe: std::sync::Arc<crate::remote::server::ScreenProbeFn> =
            std::sync::Arc::new(move |_sid: &str, _pid: u32| {
                reads_for_probe.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                // 屏读可用，但该组没有任何东西构成回读判据
                Some(screen_lines(&[" GLM-5.3-Flash thinking: high  C:\\proj"]))
            });
        let (state, sid) = mode_state_with_screen(
            fake.clone(),
            "sess_d20_no_readback",
            crate::session::AgentType::Kimi,
            97,
            crate::session::SessionStatus::Idle,
            std::sync::Arc::new(|_, _| None),
            probe,
        );
        persist_named_device(&state, "mm", "测试设备");
        let r = router(state)
            .oneshot(req(
                "POST",
                "/m/api/v1/session-mode/switch",
                Some("mam_device=mm"),
                Some(&format!(
                    r#"{{"sessionId":"{sid}","target":"bypass","group":"permission"}}"#
                )),
            ))
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(
            fake.recorded(), // `/auto` 现在是 kimi 权限组 Bypass 的**开菜单命令**（全两段式）
            vec![(97u32, "/auto".to_string())],
            "kimi 权限组 Bypass = /auto 开菜单并预选：{v}"
        );
        // 屏读不可用 → 菜单轮询如实失败：除命令提交回车①外**零按键**（无导航键、
        // 无第二次确认回车——「不盲发」红线在端点级钉住）
        let keys: Vec<String> = fake
            .recorded_key_specs()
            .into_iter()
            .map(|(_, k, _)| k)
            .collect();
        assert_eq!(keys, vec!["enter".to_string()], "只有命令提交回车①：{v}");
        assert_eq!(
            v["status"].as_str(),
            Some("failed"),
            "菜单无法定位 = 如实失败（不假装成功）：{v}"
        );
        assert!(
            v["error"]
                .as_str()
                .unwrap_or_default()
                .contains("命令已发送"),
            "失败回执说明命令已投递、请人工核对终端：{v}"
        );
        assert_eq!(
            reads.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "before 快照 1 次；投递未成功 → 回读段不启动（旧 Text 路径的 2 次不再适用）：{v}"
        );
    }

    // ============================================================
    // C6 远程新建会话端点（spec §3/§5）：POST /session-create 三校验链 + 全局单飞
    // + 黄字信号 + status 快照 + create-projects 聚合 + 审计三类行。
    // 零污染：DB 依赖全走 DeviceStore::memory()（审计/设备/KV）；会话与归档走注入源；
    // create 缝束（发现/pid/工具探测/步距）全假体——全链端到端在单测内闭环，
    // 零真实 FS（mkdir 目标用 tempdir）、零真实睡眠、零真实 spawn。
    // ============================================================

    /// C6 专用 state：create 缝束 / 注入器 / 屏序 / host / 快照 / 归档六点可注入，
    /// 其余缝与 test_state 同口径（内存库，零接触真实 ~/.tuvis）。
    /// 屏序假体：按调用序弹出（耗尽后恒 None = 读屏失败轮）；弹屏次序驱动
    /// run_pipeline 走「处置 → idle → 注入首句 → waiting_materialize」。
    fn create_state(
        hub: std::sync::Arc<CreateTaskHub>,
        injector: std::sync::Arc<dyn crate::inject::engine::Injector>,
        screens: Vec<Vec<String>>,
        host: serde_json::Value,
        sessions: Vec<crate::session::Session>,
        archive: Vec<crate::database::SessionArchiveRow>,
    ) -> Arc<RemoteState> {
        let queue: std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<Vec<String>>>> =
            std::sync::Arc::new(std::sync::Mutex::new(screens.into()));
        let q = queue.clone();
        Arc::new(RemoteState {
            target_evidence: no_target_evidence(),
            session_source: Box::new(move || crate::session::SessionsResponse {
                sessions: sessions.clone(),
                total_count: sessions.len(),
                waiting_count: 0,
            }),
            store: crate::remote::pairing::DeviceStore::memory(),
            injector,
            resume_spawner: std::sync::Arc::new(|_: &crate::inject::resume::SpawnSpec| Ok(())),
            create_hub: hub,
            capability_table: crate::inject::capability::new_table(),
            archive_source: Box::new(move || archive.clone()),
            archive_delete: std::sync::Arc::new(|_: Option<&str>| 0usize),
            confirm_probe: std::sync::Arc::new(|_, _, _| true),
            dialog_probe: std::sync::Arc::new(|_, _| None),
            screen_probe: std::sync::Arc::new(move |_, _| q.lock().unwrap().pop_front()),
            host_source: Box::new(move || host.clone()),
            message_source: Box::new(|_, _, _| Err("测试桩：未注入内容源".to_string())),
            path_source: Box::new(|_, _, _| (Vec::new(), false)),
            watcher_tx: tokio::sync::broadcast::channel(64).0,
            board_hidden_ids: Box::new(Vec::new),
            board_hidden_hide: std::sync::Arc::new(|_| 0usize),
            board_hidden_unhide: std::sync::Arc::new(|_| 0usize),
            unread_mark_read: std::sync::Arc::new(|_, _| ()),
            session_close: std::sync::Arc::new(|_| Ok(())),
            sse_registry: Arc::new(SseRegistry::default()),
            max_devices_source: Box::new(|| 3),
            pin_limiter: std::sync::Mutex::new(crate::remote::pin::PinRateLimiter::new()),
            global_pin_limiter: std::sync::Mutex::new(crate::remote::pin::PinRateLimiter::global()),
            pin_source: Box::new(|| Some("1234".to_string())),
            now_source: Box::new(|| chrono::Utc::now().timestamp_millis()),
            // 配对限速通道来源缝（#119 取代 tunnel_hosts_source；空表 = fail-closed）
            rate_bucket_channels_source: Box::new(Vec::new),
            via_hosts_source: Box::new(|| None),
            home_source: Box::new(|| None),
            // C7：配对计数缝缺省空表（既有用例零影响；配对打标/提示用例就地覆盖）
            pairing_counter: Box::new(Vec::new),
            ui_config_source: Box::new(|| None),
            subagent_source: std::collections::HashMap::new(),
            subagent_message_source: std::collections::HashMap::new(),
        })
    }

    /// 缺省 host 载荷（enabledTools 含 claude——合法用例的工具门放行形态）
    fn create_host_default() -> serde_json::Value {
        serde_json::json!({
            "host": { "name": "test-host", "platform": "windows", "version": "0.0.0-test" },
            "enabledTools": ["claude"]
        })
    }

    /// 轮询至任务终态（测试侧协调；假缝下瞬时到达——3s 超时 = 管线卡死即断言失败）
    fn wait_terminal(hub: &CreateTaskHub, id: u64) -> CreateTaskShared {
        for _ in 0..600 {
            if let Some(t) = hub.snapshot(id) {
                if t.phase == "done" || t.phase == "failed" {
                    return t;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("任务 {id} 未在 3s 内到达终态");
    }

    /// 审计行过滤助手
    fn audit_rows_of(state: &Arc<RemoteState>) -> Vec<crate::database::dao::write_audit::AuditRow> {
        state
            .store
            .with(|c| crate::database::dao::write_audit::recent_conn(c, 100))
    }

    /// ① 校验链：非 ASCII 路径 400（reasonCode=non_ascii_path）+ trim 契约（合法
    /// 路径带首尾空白 → 校验按 trim 形态通过，create_dir_all 落在 trim 后路径）
    #[tokio::test]
    async fn create_rejects_non_ascii_path_and_honors_trim() {
        let state = create_state(
            std::sync::Arc::new(CreateTaskHub::stub()),
            std::sync::Arc::new(crate::inject::engine::RealInjector),
            vec![],
            create_host_default(),
            vec![],
            vec![],
        );
        persist_named_device(&state, "dev-c6a", "手机C6a");
        let app = router(state.clone());

        // CJK 路径 → 400 non_ascii_path（v1 纯 ASCII 同规）
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-create",
                Some("mam_device=dev-c6a"),
                Some(
                    &serde_json::json!({"tool": "claude", "projectPath": r"E:\项目\demo"})
                        .to_string(),
                ),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 400);
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["error"], "bad_request");
        assert_eq!(v["reasonCode"], "non_ascii_path", "{v}");

        // trim 契约（评审 I1）：首尾空白合法路径 → 通过校验且 mkdir 用 trim 后串。
        // 落盘点选构建 target 下唯一子目录——tempdir 位于 %TEMP%（AppData 段）会被
        // create_path::validate 的凭据黑名单**正确拒绝**（AppData ∈ SENSITIVE_DIRS），
        // 本用例要的是合法路径
        let unique = format!(
            "c6-trim-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let target = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(&unique);
        assert!(!target.exists(), "前置：目标目录尚未创建");
        let r = app
            .oneshot(req(
                "POST",
                "/m/api/v1/session-create",
                Some("mam_device=dev-c6a"),
                Some(
                    &serde_json::json!({
                        "tool": "claude",
                        "projectPath": format!("  {}  ", target.display())
                    })
                    .to_string(),
                ),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "首尾空白不得改变校验结论");
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert!(v["taskId"].is_u64(), "{v}");
        assert_eq!(v["hasActiveSession"], false);
        assert!(
            target.exists(),
            "create_dir_all 必须落在 trim 后路径（盘上不得出现带空格目录）"
        );
        // 注：不再探测 "name "/" name" 变体——Win32 路径层本就会剥尾部空白，
        // 该探测在 Windows 上恒解析回同一目录，无判别力（C6 报告登记）
        let _ = std::fs::remove_dir_all(&target);
    }

    /// ①d P1-2 canonicalize 真值复核（评审 2026-10-03）：junction 隐藏段名——
    /// 用户路径本身无任何命中段（第一道 validate 放行），mkdir 后取真值路径命中
    /// 系统目录表 → 400 blacklisted + reason 回显命中段。8.3 短名（C:\PROGRA~1）
    /// 同为 canonicalize 解真值机制覆盖，junction 免管理员权限可在测试环境构造，
    /// 故以 junction 为代表锚。仅 Windows（复核逻辑 cfg windows；mklink /J 亦然）
    #[cfg(windows)]
    #[tokio::test]
    async fn create_rejects_junction_into_blocked_target() {
        let state = create_state(
            std::sync::Arc::new(CreateTaskHub::stub()),
            std::sync::Arc::new(crate::inject::engine::RealInjector),
            vec![],
            create_host_default(),
            vec![],
            vec![],
        );
        persist_named_device(&state, "dev-p12", "手机P12");
        let app = router(state.clone());

        // 测试自建目录树：<base>\Windows（段名撞系统目录表，仅为复核判据载体，
        // 非真系统目录）+ junction <base>\lnk → 该目录；用户路径 <base>\lnk 无命中段。
        // 前置清场：上次中断残留的半建目录会让 mklink /J 拒绝访问（实测 flaky 根因）
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!(
                "p12-junction-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
        let _ = std::fs::remove_dir_all(&base);
        let blocked_name_dir = base.join("Windows");
        std::fs::create_dir_all(&blocked_name_dir).unwrap();
        let lnk = base.join("lnk");
        // mklink 是 cmd 内建命令：整串单参数会被 MSVCRT 引号转义打坏（实跑
        // 「目录名语法不正确」），分体传参由 Command 逐参引号包裹
        let mk = std::process::Command::new("cmd")
            .arg("/c")
            .arg("mklink")
            .arg("/J")
            .arg(&lnk)
            .arg(&blocked_name_dir)
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        assert!(
            mk.status.success(),
            "mklink /J 失败：{}{}",
            String::from_utf8_lossy(&mk.stdout),
            String::from_utf8_lossy(&mk.stderr)
        );

        let r = app
            .oneshot(req(
                "POST",
                "/m/api/v1/session-create",
                Some("mam_device=dev-p12"),
                Some(
                    &serde_json::json!({
                        "tool": "claude",
                        "projectPath": lnk.display().to_string()
                    })
                    .to_string(),
                ),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 400, "junction 真值命中黑名单必须拒绝");
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["reasonCode"], "blacklisted", "{v}");
        assert!(
            v["reason"].as_str().is_some_and(|r| r.contains("windows")),
            "复核命中段须回显（定位详情直达消费方，修复批 I1 同口径）：{v}"
        );

        // 清场：先删联接点（remove_dir 只摘链接不递归目标），再删整树
        let _ = std::fs::remove_dir(&lnk);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// ①c 黑名单拒绝的定位详情可达消费方（修复批 I1）：400 body 须带 reason
    /// （命中段 + 所属表），不能只在 create_path 单测里成立。跨平台：.ssh 段
    /// 双形态（凭据表，读侧同源表全局段匹配）
    #[tokio::test]
    async fn create_blacklisted_carries_reason_detail() {
        let state = create_state(
            std::sync::Arc::new(CreateTaskHub::stub()),
            std::sync::Arc::new(crate::inject::engine::RealInjector),
            vec![],
            create_host_default(),
            vec![],
            vec![],
        );
        persist_named_device(&state, "dev-c6i1", "手机C6i1");
        let app = router(state.clone());
        let bad_path = if cfg!(windows) {
            r"D:\keys\.ssh\k".to_string()
        } else {
            "/tmp/x/.ssh/k".to_string()
        };
        let r = app
            .oneshot(req(
                "POST",
                "/m/api/v1/session-create",
                Some("mam_device=dev-c6i1"),
                Some(&serde_json::json!({"tool": "claude", "projectPath": bad_path}).to_string()),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 400);
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["error"], "bad_request");
        assert_eq!(v["reasonCode"], "blacklisted", "{v}");
        let reason = v["reason"].as_str().unwrap_or_default();
        assert!(reason.contains(".ssh"), "reason 应含命中段：{v}");
        assert!(reason.contains("凭据表"), "reason 应含所属表名：{v}");
    }

    /// ①b 首句入参封顶（C6 评审 I1）：firstMessage 超 MAX_SEND_CHARS → 400
    /// （与 /session-send 同标尺；防无界注入文本进 compose/终端）
    #[tokio::test]
    async fn create_rejects_oversized_first_message() {
        let state = create_state(
            std::sync::Arc::new(CreateTaskHub::stub()),
            std::sync::Arc::new(crate::inject::engine::RealInjector),
            vec![],
            create_host_default(),
            vec![],
            vec![],
        );
        persist_named_device(&state, "dev-c6f", "手机C6f");
        let app = router(state.clone());
        // 落盘点：路径合法（tempdir_in 于 CARGO_MANIFEST_DIR）——让 400 只能来自封顶
        let td = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).expect("tempdir_in");
        let oversized = "x".repeat(crate::remote::api::MAX_SEND_CHARS + 1);
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-create",
                Some("mam_device=dev-c6f"),
                Some(
                    &serde_json::json!({
                        "tool": "claude",
                        "projectPath": td.path().join("proj").to_string_lossy(),
                        "firstMessage": oversized
                    })
                    .to_string(),
                ),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 400, "超长首句必须 400");
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["error"], "bad_request", "{v}");

        // 超长路径 → 400 reasonCode=path_too_long（评审修复⑥，移动端分診可辨）
        let oversized_path = format!(
            "E:\\long\\{}",
            "x".repeat(crate::remote::api::MAX_SEND_CHARS)
        );
        let r2 = app
            .oneshot(req(
                "POST",
                "/m/api/v1/session-create",
                Some("mam_device=dev-c6f"),
                Some(
                    &serde_json::json!({ "tool": "claude", "projectPath": oversized_path })
                        .to_string(),
                ),
            ))
            .await
            .unwrap();
        assert_eq!(r2.status(), 400, "超长路径必须 400");
        let v2: serde_json::Value = serde_json::from_str(&body_string(r2).await).unwrap();
        assert_eq!(v2["reasonCode"], "path_too_long", "{v2}");
    }

    /// ② 工具门三道：白名单外 / enabledTools 关 / 安装探测 false → 400
    /// reasonCode=tool_unavailable
    #[tokio::test]
    async fn create_tool_gate_requires_whitelist_enabled_and_installed() {
        // enabledTools 关（host_source 同源数据源）
        let host_off = serde_json::json!({
            "host": { "name": "t", "platform": "windows", "version": "0" },
            "enabledTools": []
        });
        let state = create_state(
            std::sync::Arc::new(CreateTaskHub::stub()),
            std::sync::Arc::new(crate::inject::engine::RealInjector),
            vec![],
            host_off,
            vec![],
            vec![],
        );
        persist_named_device(&state, "dev-c6b", "手机C6b");
        let app = router(state);
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-create",
                Some("mam_device=dev-c6b"),
                Some(r#"{"tool":"claude","projectPath":"E:\\gate\\proj"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 400);
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["reasonCode"], "tool_unavailable", "{v}");

        // 安装探测 false（缝注入——OnceLock 真探测在无该工具机器上会恒 false，
        // 端点契约必须确定，故探测缝可覆写，C6 报告登记）
        let mut hub = CreateTaskHub::stub();
        hub.tool_probe = Box::new(|_| false);
        let state2 = create_state(
            std::sync::Arc::new(hub),
            std::sync::Arc::new(crate::inject::engine::RealInjector),
            vec![],
            create_host_default(),
            vec![],
            vec![],
        );
        persist_named_device(&state2, "dev-c6b2", "手机C6b2");
        let r = router(state2)
            .oneshot(req(
                "POST",
                "/m/api/v1/session-create",
                Some("mam_device=dev-c6b2"),
                Some(r#"{"tool":"claude","projectPath":"E:\\gate\\proj"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 400, "enabledTools 开而未安装仍须拒");
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["reasonCode"], "tool_unavailable", "{v}");

        // 白名单外（workbuddy 不在 create 四家表）
        let host_wb = serde_json::json!({
            "host": { "name": "t", "platform": "windows", "version": "0" },
            "enabledTools": ["workbuddy"]
        });
        let state3 = create_state(
            std::sync::Arc::new(CreateTaskHub::stub()),
            std::sync::Arc::new(crate::inject::engine::RealInjector),
            vec![],
            host_wb,
            vec![],
            vec![],
        );
        persist_named_device(&state3, "dev-c6b3", "手机C6b3");
        let r = router(state3)
            .oneshot(req(
                "POST",
                "/m/api/v1/session-create",
                Some("mam_device=dev-c6b3"),
                Some(r#"{"tool":"workbuddy","projectPath":"E:\\gate\\proj"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 400, "白名单外工具不入 create 域");
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["reasonCode"], "tool_unavailable", "{v}");
    }

    /// ③ 全局单飞：合法 → 200 {taskId}；任务在飞（非终态，物化轮询被停）第二个
    /// 请求 → 409 conflict；终态（failed）后可再建 → 新 taskId。
    /// 顺带锁 status 在飞快照 phase=waiting_materialize（④ 在飞臂）。
    #[tokio::test]
    async fn create_single_flight_conflict_and_recreate_after_terminal() {
        // 发现缝：首轮进入即发信号并停（任务停在 waiting_materialize 在飞），
        // 释放后返回空表 → 15 轮 × no-op 步距 → materialize_timeout 终态。
        // Sender/Receiver 非 Sync（CreateDiscoverFn 要求 Sync）——Mutex 包装
        let (entered_tx, entered_rx) = std::sync::mpsc::channel::<()>();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let entered_tx = std::sync::Mutex::new(entered_tx);
        let release_rx = std::sync::Mutex::new(release_rx);
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let c2 = calls.clone();
        let disc: Box<CreateDiscoverFn> = Box::new(move |_t, _s, _d| {
            if c2.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                let _ = entered_tx.lock().unwrap().send(());
                // 释放前停在发现轮（in-flight 保持）；超时兜底放行防测试泄漏悬挂线程
                let _ = release_rx
                    .lock()
                    .unwrap()
                    .recv_timeout(std::time::Duration::from_secs(10));
            }
            Vec::new()
        });
        let state = create_state(
            std::sync::Arc::new(CreateTaskHub::stub_with(disc)),
            FakeInjector::ok(),
            // 屏序：直接 idle 两轮（双读确认，C8 定案 → 注入首句 → waiting_materialize）
            vec![
                vec!["manual mode on · ? for shortcuts".into()],
                vec!["manual mode on · ? for shortcuts".into()],
            ],
            create_host_default(),
            vec![],
            vec![],
        );
        persist_named_device(&state, "dev-c6c", "手机C6c");
        let app = router(state.clone());
        let hub = state.create_hub.clone();
        // 落盘点（C6 评审 I2）：tempdir_in 于 CARGO_MANIFEST_DIR——自动唯一+自动清理
        // （%TEMP% 在 AppData 段会被 SENSITIVE_DIRS 正确拒绝，不能用作合法路径）
        let td = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).expect("tempdir_in");
        let dir = td.path().join("proj").to_string_lossy().to_string();
        let dir_other = td.path().join("other").to_string_lossy().to_string();
        let payload =
            |p: &str| serde_json::json!({ "tool": "claude", "projectPath": p }).to_string();

        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-create",
                Some("mam_device=dev-c6c"),
                Some(&payload(&dir)),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["taskId"], 1, "任务 id 自 1 起自增：{v}");
        assert_eq!(v["hasActiveSession"], false);

        // 管线进入物化轮询（在飞）后第二个请求 → 409 conflict
        entered_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("管线未进入物化轮询");
        assert_eq!(
            hub.snapshot(1).map(|t| t.phase).as_deref(),
            Some("waiting_materialize"),
            "在飞相 = waiting_materialize（④ 在飞臂一并锁定）"
        );
        let r2 = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-create",
                Some("mam_device=dev-c6c"),
                Some(&payload(&dir_other)),
            ))
            .await
            .unwrap();
        assert_eq!(r2.status(), 409);
        let v2: serde_json::Value = serde_json::from_str(&body_string(r2).await).unwrap();
        assert_eq!(v2["error"], "conflict", "{v2}");

        // 释放 → 15 轮空发现 → materialize_timeout（终态可查，不占单飞额度）
        let _ = release_tx.send(());
        let t = wait_terminal(&hub, 1);
        assert_eq!(t.phase, "failed");
        assert!(t.detail.as_deref().unwrap_or_default().contains("物化超时"));

        let r3 = app
            .oneshot(req(
                "POST",
                "/m/api/v1/session-create",
                Some("mam_device=dev-c6c"),
                Some(&payload(&dir)),
            ))
            .await
            .unwrap();
        assert_eq!(r3.status(), 200, "终态任务不占单飞额度——可再建");
        let v3: serde_json::Value = serde_json::from_str(&body_string(r3).await).unwrap();
        assert_eq!(v3["taskId"], 2, "新任务拿到下一个 id：{v3}");
        wait_terminal(&hub, 2);
    }

    /// ④ status 端点：未知 taskId → 404；终态快照 {phase, sessionId, spawnedPid}
    /// 完整；done 后可再建（与 ③ 的终态不占单飞合并断言）。
    #[tokio::test]
    async fn create_status_endpoint_unknown_404_and_done_snapshot() {
        let state = create_state(
            std::sync::Arc::new(CreateTaskHub::stub_with(Box::new(|_, _, _| {
                vec!["sid-c6-ok".to_string()]
            }))),
            FakeInjector::ok(),
            // 屏序：信任框（处置 down,enter）→ idle 双读确认 → 注入首句 → waiting_materialize
            vec![
                vec![
                    "Quick safety check: Is this a project you created or one you trust?".into(),
                    "❯ No, exit".into(),
                ],
                vec!["manual mode on · ? for shortcuts".into()],
                vec!["manual mode on · ? for shortcuts".into()],
            ],
            create_host_default(),
            vec![],
            vec![],
        );
        persist_named_device(&state, "dev-c6d", "手机C6d");
        let app = router(state.clone());
        let hub = state.create_hub.clone();

        // 未知 taskId → 404（含畸形值）
        for q in ["?taskId=999", "?taskId=abc", ""] {
            let r = app
                .clone()
                .oneshot(req(
                    "GET",
                    &format!("/m/api/v1/session-create/status{q}"),
                    Some("mam_device=dev-c6d"),
                    None,
                ))
                .await
                .unwrap();
            assert_eq!(r.status(), 404, "未知/畸形 taskId 一律 404：{q}");
        }

        // 落盘点（C6 评审 I2）：tempdir_in 自动唯一+自动清理
        let td = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).expect("tempdir_in");
        let dir = td.path().join("proj").to_string_lossy().to_string();
        let r = app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-create",
                Some("mam_device=dev-c6d"),
                Some(&serde_json::json!({ "tool": "claude", "projectPath": dir }).to_string()),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["taskId"], 1);
        let t = wait_terminal(&hub, 1);
        assert_eq!(t.phase, "done");

        // 终态快照：sessionId = 物化回读、spawnedPid = 锚定假体 4242
        let r = app
            .oneshot(req(
                "GET",
                "/m/api/v1/session-create/status?taskId=1",
                Some("mam_device=dev-c6d"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["phase"], "done", "{v}");
        assert_eq!(v["sessionId"], "sid-c6-ok", "{v}");
        assert_eq!(v["spawnedPid"], 4242, "pid 锚定缝假体值透出：{v}");
    }

    /// ⑤ 审计三类行（内存库可查，设备名 = 发起设备 cookie 名）：
    /// 成功链 = dialog（场景+键序）+ send（首句原文）+ create（ok, session_id=物化者）；
    /// 失败链 = create（failed:<code>, session_id=""），无 dialog/send 行。
    #[tokio::test]
    async fn create_audit_rows_success_and_failure() {
        // ---- 成功链 ----
        let state = create_state(
            std::sync::Arc::new(CreateTaskHub::stub_with(Box::new(|_, _, _| {
                vec!["sid-audit-1".to_string()]
            }))),
            FakeInjector::ok(),
            vec![
                vec![
                    "Quick safety check: Is this a project you created or one you trust?".into(),
                    "❯ No, exit".into(),
                ],
                vec!["Quick safety check".into()],
                vec!["manual mode on · ? for shortcuts".into()],
                // idle 双读确认（C8 定案）
                vec!["manual mode on · ? for shortcuts".into()],
            ],
            create_host_default(),
            vec![],
            vec![],
        );
        persist_named_device(&state, "dev-c6e", "手机C6e");
        let app = router(state.clone());
        // 落盘点（C6 评审 I2）：tempdir_in 自动唯一+自动清理，路径随平台无盘符依赖
        let td = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).expect("tempdir_in");
        let dir = td.path().join("proj").to_string_lossy().to_string();
        let r = app
            .oneshot(req(
                "POST",
                "/m/api/v1/session-create",
                Some("mam_device=dev-c6e"),
                Some(&serde_json::json!({ "tool": "claude", "projectPath": dir }).to_string()),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        wait_terminal(state.create_hub.as_ref(), 1);

        let rows = audit_rows_of(&state);
        let by_action = |a: &str| {
            rows.iter()
                .filter(|r| r.action == a)
                .cloned()
                .collect::<Vec<_>>()
        };
        // dialog 行：summary = 场景+键序（claude 信任框红线键序），result ok，物化前 sid=""
        let dialogs = by_action("dialog");
        assert_eq!(dialogs.len(), 1, "恰好一条 dialog 行：{rows:?}");
        assert_eq!(dialogs[0].summary, "create_trust: down,enter", "{rows:?}");
        assert_eq!(dialogs[0].result, "ok");
        assert_eq!(dialogs[0].session_id, "", "物化前 dialog 行 sid 为空");
        assert_eq!(dialogs[0].device_name, "手机C6e");
        assert_eq!(dialogs[0].agent_type, "claude");
        // send 行：content = composed 原文（默认探针 hi；签名默认关=裸正文，2026-10-05），sid=""
        let sends = by_action("send");
        assert_eq!(sends.len(), 1, "恰好一条 send 行（首句）：{rows:?}");
        assert_eq!(sends[0].summary, "hi", "{rows:?}");
        assert_eq!(sends[0].result, "ok");
        assert_eq!(sends[0].session_id, "");
        assert_eq!(sends[0].device_name, "手机C6e");
        // create 行：ok + 物化 session_id + 目录+tool 摘要（不写首句原文）。
        // 摘要经 audit 摘要上限截断（Linux runner 的 tempdir 路径超限会带 … 尾）
        // ——期望值用同一 summarize 算，环境路径长短不改变结论
        let creates = by_action("create");
        assert_eq!(creates.len(), 1, "恰好一条 create 行：{rows:?}");
        assert_eq!(creates[0].result, "ok");
        assert_eq!(creates[0].session_id, "sid-audit-1");
        assert_eq!(
            creates[0].summary,
            crate::inject::normalize::summarize(
                &format!("create claude @ {}", dir),
                crate::inject::normalize::AUDIT_SUMMARY_CHARS
            ),
            "{rows:?}"
        );
        assert!(
            !creates[0].summary.contains("hi"),
            "create 行不得写首句原文：{rows:?}"
        );

        // ---- 失败链（未识别屏 → failed:unrecognized_screen，现场保留）----
        let state2 = create_state(
            std::sync::Arc::new(CreateTaskHub::stub()),
            FakeInjector::ok(),
            (0..16)
                .map(|_| vec!["某种未识别界面".to_string()])
                .collect(),
            create_host_default(),
            vec![],
            vec![],
        );
        persist_named_device(&state2, "dev-c6e2", "手机C6e2");
        // 落盘点（C6 评审 I2）：tempdir_in 自动唯一+自动清理
        let td2 = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).expect("tempdir_in");
        let dir2 = td2.path().join("fail").to_string_lossy().to_string();
        let r = router(state2.clone())
            .oneshot(req(
                "POST",
                "/m/api/v1/session-create",
                Some("mam_device=dev-c6e2"),
                Some(&serde_json::json!({ "tool": "claude", "projectPath": dir2 }).to_string()),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let t = wait_terminal(state2.create_hub.as_ref(), 1);
        assert_eq!(t.phase, "failed");
        assert!(
            t.detail
                .as_deref()
                .unwrap_or_default()
                .contains("终端窗口保留供查看现场"),
            "失败不自动清场——detail 注明现场保留：{:?}",
            t.detail
        );
        let rows2 = audit_rows_of(&state2);
        let creates2: Vec<_> = rows2.iter().filter(|r| r.action == "create").collect();
        assert_eq!(creates2.len(), 1, "{rows2:?}");
        assert_eq!(
            creates2[0].result, "failed:unrecognized_screen",
            "{rows2:?}"
        );
        assert_eq!(creates2[0].session_id, "", "失败/物化前 create 行 sid 为空");
        assert!(
            !rows2.iter().any(|r| r.action == "dialog"),
            "未识别失败无 dialog 行：{rows2:?}"
        );
        assert!(
            !rows2.iter().any(|r| r.action == "send"),
            "失败链无 send 行（首句未注入）：{rows2:?}"
        );
    }

    /// ⑥ create-projects：archive 三条（7 天内 / 8 天前 / 非 ASCII）+ 快照活跃一条
    /// → 只含 7 天内与活跃项；同目录去重（快照尾斜杠形态 ∪ archive）、lastActiveAt
    /// 取新、tools 并集、activeTools 仅快照、lastActiveAt 降序。
    #[tokio::test]
    async fn create_projects_window_dedupe_and_sort() {
        let now = chrono::Utc::now();
        let iso =
            |d: chrono::Duration| (now - d).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        // 夹具路径按运行平台取合法形态（M1 过滤器按 std::env::consts::OS 校验——
        // posix 形态路径在 windows 上会以 bad_windows_form 被正确滤除，夹具须与
        // 平台一致才能测到窗口/去重语义本身）
        let pa: &str = if cfg!(windows) {
            r"E:\proj\pa"
        } else {
            "/tmp/pa"
        };
        let pb: &str = if cfg!(windows) {
            r"E:\proj\pb"
        } else {
            "/tmp/pb"
        };
        let pc: &str = if cfg!(windows) {
            r"E:\proj\pc"
        } else {
            "/tmp/pc"
        };
        let pz: &str = if cfg!(windows) {
            r"E:\proj\pz"
        } else {
            "/tmp/pz"
        };
        let archive = vec![
            crate::database::SessionArchiveRow {
                session_id: "a1".into(),
                agent_type: "claude".into(),
                project_path: pa.into(),
                project_name: "pa".into(),
                title: None,
                last_status: "finished".into(),
                first_seen: iso(chrono::Duration::days(6)),
                last_seen: iso(chrono::Duration::days(2)),
                updated_at: iso(chrono::Duration::days(2)),
            },
            // 8 天前 → 出窗
            crate::database::SessionArchiveRow {
                session_id: "a2".into(),
                agent_type: "codex".into(),
                project_path: pb.into(),
                project_name: "pb".into(),
                title: None,
                last_status: "finished".into(),
                first_seen: iso(chrono::Duration::days(9)),
                last_seen: iso(chrono::Duration::days(8)),
                updated_at: iso(chrono::Duration::days(8)),
            },
            // 非 ASCII 路径 → 过滤（v1 同规）
            crate::database::SessionArchiveRow {
                session_id: "a3".into(),
                agent_type: "claude".into(),
                project_path: "E:\\项目".into(),
                project_name: "cjk".into(),
                title: None,
                last_status: "finished".into(),
                first_seen: iso(chrono::Duration::days(1)),
                last_seen: iso(chrono::Duration::days(1)),
                updated_at: iso(chrono::Duration::days(1)),
            },
            // 同目录另一工具（tools 并集来源）
            crate::database::SessionArchiveRow {
                session_id: "a5".into(),
                agent_type: "codex".into(),
                project_path: pa.into(),
                project_name: "pa".into(),
                title: None,
                last_status: "finished".into(),
                first_seen: iso(chrono::Duration::days(6)),
                last_seen: iso(chrono::Duration::days(2)),
                updated_at: iso(chrono::Duration::days(2)),
            },
            // 次新目录（排序第二位）
            crate::database::SessionArchiveRow {
                session_id: "a4".into(),
                agent_type: "kimi".into(),
                project_path: pc.into(),
                project_name: "pc".into(),
                title: None,
                last_status: "finished".into(),
                first_seen: iso(chrono::Duration::days(4)),
                last_seen: iso(chrono::Duration::days(3)),
                updated_at: iso(chrono::Duration::days(3)),
            },
        ];
        let mut live = inj_sess(
            "live-1",
            crate::session::AgentType::Claude,
            42,
            crate::session::SessionStatus::Waiting,
        );
        live.project_path = format!("{pa}/"); // 尾斜杠形态——归一后与 archive 同目录
        live.last_activity_at = iso(chrono::Duration::hours(1));

        let state = create_state(
            std::sync::Arc::new(CreateTaskHub::stub()),
            std::sync::Arc::new(crate::inject::engine::RealInjector),
            vec![],
            create_host_default(),
            vec![live],
            archive,
        );
        persist_named_device(&state, "dev-c6f", "手机C6f");
        let r = router(state)
            .oneshot(req(
                "GET",
                "/m/api/v1/create-projects",
                Some("mam_device=dev-c6f"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        let projects = v["projects"].as_array().expect("projects 数组");
        assert_eq!(
            projects.len(),
            2,
            "只含 7 天内与活跃项（pb 出窗、CJK 过滤）：{v}"
        );
        // 第一位 = pa：lastActiveAt 取新（快照 1h 前 > archive 2 天前）
        let pa_json = &projects[0];
        assert_eq!(pa_json["path"], pa, "去重展示形态取快照优先: {v}");
        assert_eq!(
            pa_json["lastActiveAt"],
            iso(chrono::Duration::hours(1)),
            "lastActiveAt = 最近活跃: {v}"
        );
        assert_eq!(
            pa_json["tools"],
            serde_json::json!(["claude", "codex"]),
            "tools = 快照 ∪ archive 工具并集: {v}"
        );
        assert_eq!(
            pa_json["activeTools"],
            serde_json::json!(["claude"]),
            "activeTools = 快照中该路径有活跃会话的工具: {v}"
        );
        // 第二位 = pc（3 天前归档，无活跃）
        let pc_json = &projects[1];
        assert_eq!(pc_json["path"], pc);
        assert_eq!(pc_json["tools"], serde_json::json!(["kimi"]));
        assert_eq!(pc_json["activeTools"], serde_json::json!([]));
        // days 窗生效：显式 days=1 → 只剩活跃快照项
        // （换新 state 避免与上一请求的断言耦合）
        let state2 = create_state(
            std::sync::Arc::new(CreateTaskHub::stub()),
            std::sync::Arc::new(crate::inject::engine::RealInjector),
            vec![],
            create_host_default(),
            vec![],
            vec![crate::database::SessionArchiveRow {
                session_id: "a9".into(),
                agent_type: "claude".into(),
                project_path: pz.into(),
                project_name: "pz".into(),
                title: None,
                last_status: "finished".into(),
                first_seen: iso(chrono::Duration::days(2)),
                last_seen: iso(chrono::Duration::days(2)),
                updated_at: iso(chrono::Duration::days(2)),
            }],
        );
        persist_named_device(&state2, "dev-c6f2", "手机C6f2");
        let r = router(state2)
            .oneshot(req(
                "GET",
                "/m/api/v1/create-projects?days=1",
                Some("mam_device=dev-c6f2"),
                None,
            ))
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(
            v["projects"].as_array().map(Vec::len),
            Some(0),
            "2 天前归档不出 1 天窗：{v}"
        );
    }

    // ==== 2026-10-08 子 agent 运行 chip：GET /m/api/v1/session-subagents（spec §5.2）====

    /// 假源装配：复制 test_state() 完整字面量（逐字段同值），仅 subagent_source
    /// 换成假源（仓内先例：inject/queue.rs 的 state()、server.rs 各用例组持有字面量）。
    /// claude 假源返回两条**故意倒序**的视图——端点必须按 spawnTs 升序输出、None 排尾。
    fn state_with_subagent_fakes() -> Arc<RemoteState> {
        let mut fakes: std::collections::HashMap<&'static str, Box<SubagentSourceFn>> =
            std::collections::HashMap::new();
        fakes.insert(
            "claude",
            Box::new(|_: &str, sid: &str| {
                vec![
                    crate::monitor::subagents::SubagentView {
                        id: format!("late-{sid}"),
                        name: "Plan".into(),
                        description: Some("设计新建会话两改动实现方案".into()),
                        spawn_ts: Some("2026-10-08T09:00:00.000+00:00".into()),
                        tokens: crate::monitor::subagents::TokenUsage {
                            input: 5124,
                            cache_read: 56448,
                            cache_creation: 0,
                            output: 9312,
                        },
                        status: crate::monitor::subagents::SubagentStatus::Running,
                        end_ts: None,
                    },
                    crate::monitor::subagents::SubagentView {
                        id: format!("early-{sid}"),
                        name: "Explore".into(),
                        description: None,
                        spawn_ts: None, // spawn 竞态：无时长
                        tokens: crate::monitor::subagents::TokenUsage::default(),
                        status: crate::monitor::subagents::SubagentStatus::Idle,
                        end_ts: Some("2026-10-08T08:59:00.000+00:00".into()),
                    },
                ]
            }),
        );
        Arc::new(RemoteState {
            ui_config_source: Box::new(|| None),
            target_evidence: no_target_evidence(),
            subagent_source: fakes,
            subagent_message_source: std::collections::HashMap::new(),
            capability_table: crate::inject::capability::new_table(),
            session_source: Box::new(|| crate::session::SessionsResponse {
                sessions: vec![],
                total_count: 7,
                waiting_count: 0,
            }),
            store: crate::remote::pairing::DeviceStore::memory(),
            injector: std::sync::Arc::new(crate::inject::engine::RealInjector),
            resume_spawner: std::sync::Arc::new(|_: &crate::inject::resume::SpawnSpec| Ok(())),
            create_hub: create_hub_stub(),
            archive_source: Box::new(Vec::new),
            archive_delete: std::sync::Arc::new(|_: Option<&str>| 0usize),
            confirm_probe: std::sync::Arc::new(|_, _, _| true),
            dialog_probe: std::sync::Arc::new(|_, _| None),
            screen_probe: std::sync::Arc::new(|_, _| None),
            host_source: Box::new(|| {
                serde_json::json!({
                    "host": { "name": "test-host", "platform": "macos", "version": "0.0.0-test" },
                    "enabledTools": ["claude"]
                })
            }),
            message_source: Box::new(|_, _, _| Err("测试桩：未注入内容源".to_string())),
            path_source: Box::new(|_, _, _| (Vec::new(), false)),
            watcher_tx: tokio::sync::broadcast::channel(64).0,
            board_hidden_ids: Box::new(Vec::new),
            board_hidden_hide: std::sync::Arc::new(|_| 0usize),
            board_hidden_unhide: std::sync::Arc::new(|_| 0usize),
            unread_mark_read: std::sync::Arc::new(|_, _| ()),
            session_close: std::sync::Arc::new(|_| Ok(())),
            sse_registry: Arc::new(SseRegistry::default()),
            max_devices_source: Box::new(|| 3),
            pin_limiter: std::sync::Mutex::new(crate::remote::pin::PinRateLimiter::new()),
            global_pin_limiter: std::sync::Mutex::new(crate::remote::pin::PinRateLimiter::global()),
            pin_source: Box::new(|| Some("1234".to_string())),
            now_source: Box::new(|| chrono::Utc::now().timestamp_millis()),
            via_hosts_source: Box::new(|| None),
            rate_bucket_channels_source: Box::new(Vec::new),
            home_source: Box::new(|| None),
            pairing_counter: Box::new(Vec::new),
        })
    }

    /// 端点契约矩阵（spec §5.2/§7）：缺参 400 / 空白 400 / 未知工具空态 / 假源
    /// camelCase 载荷 / no-store 头 / spawnTs 升序（None 排尾）/ 无 cookie 403
    /// （gate 结构性覆盖）
    #[tokio::test]
    async fn session_subagents_contract_matrix() {
        let st = state_with_subagent_fakes();
        let app = crate::remote::server::router(st.clone());
        // 直插设备表返回 cookie，不走 /pair/pin（零 PIN 依赖）
        let cookie = paired_cookie(&st, "sub-chips-1");
        // ① 缺 agent_type → 400
        let r = app
            .clone()
            .oneshot(req(
                "GET",
                "/m/api/v1/session-subagents?session_id=s1",
                Some(&cookie),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 400);
        // ② 缺 session_id → 400
        let r = app
            .clone()
            .oneshot(req(
                "GET",
                "/m/api/v1/session-subagents?agent_type=claude",
                Some(&cookie),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 400);
        // ③ 空白参（%20）→ 400
        let r = app
            .clone()
            .oneshot(req(
                "GET",
                "/m/api/v1/session-subagents?agent_type=%20&session_id=s1",
                Some(&cookie),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 400);
        // ④ 未知工具 → 200 空态（端点是空态唯一权威）
        let r = app
            .clone()
            .oneshot(req(
                "GET",
                "/m/api/v1/session-subagents?agent_type=zzz&session_id=s1",
                Some(&cookie),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(body_string(r).await, r#"{"subagents":[]}"#);
        // ⑤ 假源：camelCase 载荷 + spawnTs 升序（None 排尾）+ 四桶 + no-store
        let r = app
            .clone()
            .oneshot(req(
                "GET",
                "/m/api/v1/session-subagents?agent_type=claude&session_id=s1",
                Some(&cookie),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(
            r.headers().get("cache-control").unwrap(),
            "no-store",
            "门禁下私有数据禁中间层缓存"
        );
        let body = body_string(r).await;
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let arr = v["subagents"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["id"], "late-s1", "spawnTs 升序：有时戳在前");
        assert_eq!(arr[0]["name"], "Plan");
        assert_eq!(arr[0]["tokens"]["cacheRead"], 56448);
        assert_eq!(arr[0]["description"], "设计新建会话两改动实现方案");
        assert_eq!(arr[1]["id"], "early-s1", "无 spawnTs 排尾");
        assert_eq!(arr[1]["spawnTs"], serde_json::Value::Null);
        // ⑤b（观察台 §二）：全量名单——idle 也返回；status 小写单词；endTs camelCase
        assert_eq!(arr[0]["status"], "running", "活跃条目照常返回");
        assert_eq!(arr[0]["endTs"], serde_json::Value::Null);
        assert_eq!(
            arr[1]["status"], "idle",
            "终态条目不再被端点过滤（全量名单）"
        );
        assert_eq!(arr[1]["endTs"], "2026-10-08T08:59:00.000+00:00");
        // ⑥ 无 cookie → 403（gate 结构性覆盖）
        let r = app
            .clone()
            .oneshot(req(
                "GET",
                "/m/api/v1/session-subagents?agent_type=claude&session_id=s1",
                None,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 403);
    }

    // ==== 2026-10-09 观察台 T4：GET /m/api/v1/session-subagent-messages ====

    /// 假源装配（观察台 §三）：复制 state_with_subagent_fakes 完整字面量（逐字段
    /// 同值），subagent_source 置空缺省、subagent_message_source 换成入参假源
    /// （仓内「每用例组持有字面量」惯例同款）。
    fn state_with_subagent_msg_fakes(
        fakes: std::collections::HashMap<&'static str, Box<SubagentMessageSourceFn>>,
    ) -> Arc<RemoteState> {
        Arc::new(RemoteState {
            ui_config_source: Box::new(|| None),
            target_evidence: no_target_evidence(),
            subagent_source: std::collections::HashMap::new(),
            subagent_message_source: fakes,
            capability_table: crate::inject::capability::new_table(),
            session_source: Box::new(|| crate::session::SessionsResponse {
                sessions: vec![],
                total_count: 7,
                waiting_count: 0,
            }),
            store: crate::remote::pairing::DeviceStore::memory(),
            injector: std::sync::Arc::new(crate::inject::engine::RealInjector),
            resume_spawner: std::sync::Arc::new(|_: &crate::inject::resume::SpawnSpec| Ok(())),
            create_hub: create_hub_stub(),
            archive_source: Box::new(Vec::new),
            archive_delete: std::sync::Arc::new(|_: Option<&str>| 0usize),
            confirm_probe: std::sync::Arc::new(|_, _, _| true),
            dialog_probe: std::sync::Arc::new(|_, _| None),
            screen_probe: std::sync::Arc::new(|_, _| None),
            host_source: Box::new(|| {
                serde_json::json!({
                    "host": { "name": "test-host", "platform": "macos", "version": "0.0.0-test" },
                    "enabledTools": ["claude"]
                })
            }),
            message_source: Box::new(|_, _, _| Err("测试桩：未注入内容源".to_string())),
            path_source: Box::new(|_, _, _| (Vec::new(), false)),
            watcher_tx: tokio::sync::broadcast::channel(64).0,
            board_hidden_ids: Box::new(Vec::new),
            board_hidden_hide: std::sync::Arc::new(|_| 0usize),
            board_hidden_unhide: std::sync::Arc::new(|_| 0usize),
            unread_mark_read: std::sync::Arc::new(|_, _| ()),
            session_close: std::sync::Arc::new(|_| Ok(())),
            sse_registry: Arc::new(SseRegistry::default()),
            max_devices_source: Box::new(|| 3),
            pin_limiter: std::sync::Mutex::new(crate::remote::pin::PinRateLimiter::new()),
            global_pin_limiter: std::sync::Mutex::new(crate::remote::pin::PinRateLimiter::global()),
            pin_source: Box::new(|| Some("1234".to_string())),
            now_source: Box::new(|| chrono::Utc::now().timestamp_millis()),
            via_hosts_source: Box::new(|| None),
            rate_bucket_channels_source: Box::new(Vec::new),
            home_source: Box::new(|| None),
            pairing_counter: Box::new(Vec::new),
        })
    }

    /// GET /session-subagent-messages 契约矩阵（观察台 §三）：缺参 400 / 穿越 400 /
    /// 未装配工具 supported:false / claude 假源消息形状 / no-store / 无 cookie 403
    #[tokio::test]
    async fn session_subagent_messages_contract_matrix() {
        let mut fakes: std::collections::HashMap<&'static str, Box<SubagentMessageSourceFn>> =
            std::collections::HashMap::new();
        fakes.insert(
            "claude",
            Box::new(|_sid: &str, _sub: &str, _limit: usize| {
                Ok(crate::remote::content::MessagesPage {
                    messages: vec![crate::remote::content::SessionMessage {
                        seq: 0,
                        role: "user".into(),
                        kind: "user".into(),
                        content: "设计新方案".into(),
                        ts: Some(1760000000000),
                        tool_name: None,
                        tool_args: None,
                        collapsed: false,
                    }],
                    truncated: false,
                })
            }),
        );
        let st = state_with_subagent_msg_fakes(fakes);
        let app = crate::remote::server::router(st.clone());
        let cookie = paired_cookie(&st, "sub-det-1");
        // ① 缺 subagent_id → 400
        let r = app
            .clone()
            .oneshot(req(
                "GET",
                "/m/api/v1/session-subagent-messages?agent_type=claude&session_id=s1",
                Some(&cookie),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 400);
        // ② subagent_id 穿越 → 400（该 id 直接拼 subagents/agent-<id>.jsonl 文件名）
        let r = app
            .clone()
            .oneshot(req(
                "GET",
                "/m/api/v1/session-subagent-messages?agent_type=claude&session_id=s1&subagent_id=..%2F..%2Fsecret",
                Some(&cookie),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 400);
        // ③ 未装配工具（opencode）→ 200 supported:false 空表（「暂不支持」是正常态）
        let r = app
            .clone()
            .oneshot(req(
                "GET",
                "/m/api/v1/session-subagent-messages?agent_type=opencode&session_id=s1&subagent_id=x",
                Some(&cookie),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["supported"], false);
        assert_eq!(v["messages"].as_array().map(Vec::len), Some(0));
        // ④ claude 假源：消息形状 + supported:true + no-store
        let r = app
            .clone()
            .oneshot(req(
                "GET",
                "/m/api/v1/session-subagent-messages?agent_type=claude&session_id=s1&subagent_id=a1",
                Some(&cookie),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(r.headers().get("cache-control").unwrap(), "no-store");
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["supported"], true);
        assert_eq!(v["messages"][0]["kind"], "user");
        assert_eq!(v["messages"][0]["content"], "设计新方案");
        // ⑤ 无 cookie → 403（gate 结构性覆盖）
        let r = app
            .clone()
            .oneshot(req(
                "GET",
                "/m/api/v1/session-subagent-messages?agent_type=claude&session_id=s1&subagent_id=a1",
                None,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 403);
    }

    // ==== L13 靶向闸：**注入端点全覆盖**（C0-③ 复审补口）====
    //
    // 消息投递漏斗（inject::queue）之外，还有四条端点路径直接拿卡片 pid 按键/打字：
    // approve/reject、question answer、mode switch、mode menu。四条都改走同一靶向入口
    // [`crate::window::tty_map::resolve_session_target`]，本组用例逐条钉「≥2 候选 ⇒
    // 零注入拒绝 + 审计 ambiguous_target」与「恰 1 候选 ⇒ 行为不变」。
    // 证据经 `RemoteState.target_evidence` 缝注入（合成同 cwd 候选进程——单测不起真进程）。

    /// 合成证据的 cwd 与会话夹具一致（`inj_sess` 的 `project_path`）
    const FIXTURE_CWD: &str = "/tmp/proj";

    /// 审批（批准）：≥2 候选 → 200 failed{plan 文案} + **零按键** + 审计
    /// `approve/ambiguous_target`（回执契约与本端点既有失败臂同形）
    #[tokio::test]
    async fn approve_refuses_when_target_ambiguous() {
        let fake = FakeInjector::ok();
        let state = with_target_evidence(
            approve_state(fake.clone(), Some(APPROVE_HIT_MSG)),
            target_evidence_in_cwd(FIXTURE_CWD, &[81, 19]),
        );
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state.clone());
        let r = app
            .oneshot(req(
                "POST",
                "/m/api/v1/session-approve",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_l13ap","optionId":"approve"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let body = body_string(r).await;
        assert!(
            body.contains(&crate::window::tty_map::ambiguous_target_error(2)),
            "拒绝文案 = plan 定形：{body}"
        );
        assert!(body.contains("\"status\":\"failed\""), "{body}");
        assert!(fake.recorded_keys().is_empty(), "拒绝必须零按键（不猜）");
        assert!(fake.recorded().is_empty(), "拒绝必须零文本注入");
        let audits = state
            .store
            .with(|c| crate::database::dao::write_audit::recent_conn(c, 10));
        assert_eq!(audits.len(), 1);
        assert_eq!(audits[0].action, "approve");
        assert_eq!(audits[0].result, "ambiguous_target");
        assert_eq!(audits[0].session_id, "sess_l13ap");
    }

    /// 审批（拒绝）：同上——action 词表沿 optionId 记 `reject`
    #[tokio::test]
    async fn reject_refuses_when_target_ambiguous() {
        let fake = FakeInjector::ok();
        let state = with_target_evidence(
            approve_state(fake.clone(), Some(APPROVE_HIT_MSG)),
            target_evidence_in_cwd(FIXTURE_CWD, &[82, 19]),
        );
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state.clone());
        let r = app
            .oneshot(req(
                "POST",
                "/m/api/v1/session-approve",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_l13rj","optionId":"reject"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let body = body_string(r).await;
        assert!(
            body.contains(&crate::window::tty_map::ambiguous_target_error(2)),
            "{body}"
        );
        assert!(fake.recorded_keys().is_empty(), "拒绝必须零按键（不猜）");
        let audits = state
            .store
            .with(|c| crate::database::dao::write_audit::recent_conn(c, 10));
        assert_eq!(audits[0].action, "reject");
        assert_eq!(audits[0].result, "ambiguous_target");
    }

    /// 审批对照（恰 1 候选 ⇒ 行为不变）：照常发映射键 + 审计 ok
    #[tokio::test]
    async fn approve_allows_with_single_candidate() {
        let fake = FakeInjector::ok();
        let state = with_target_evidence(
            approve_state(fake.clone(), Some(APPROVE_HIT_MSG)),
            target_evidence_in_cwd(FIXTURE_CWD, &[81]),
        );
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state.clone());
        let r = app
            .oneshot(req(
                "POST",
                "/m/api/v1/session-approve",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_l13ap","optionId":"approve"}"#),
            ))
            .await
            .unwrap();
        let body = body_string(r).await;
        assert!(body.contains("\"status\":\"key_sent\""), "{body}");
        assert_eq!(fake.recorded_keys(), vec![(81u32, "1".to_string())]);
        let audits = state
            .store
            .with(|c| crate::database::dao::write_audit::recent_conn(c, 10));
        assert_eq!(audits[0].result, "ok", "单候选路径零漂移");
    }

    /// 问答作答（单选 select）：≥2 候选 → 200 failed{plan 文案} + 零按键 + 审计
    /// `answer/ambiguous_target`
    #[tokio::test]
    async fn question_answer_refuses_when_target_ambiguous() {
        let (state, fake, _script) = stage_rig(vec![], false);
        let state = with_target_evidence(state, target_evidence_in_cwd(FIXTURE_CWD, &[81, 82]));
        mark_question(&state, "claude", "sess_l13q", Q_SINGLE_PAYLOAD);
        let app = router(state.clone());
        let r = app
            .oneshot(req(
                "POST",
                "/m/api/v1/session-question/answer",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_l13q","action":"select","index":1}"#),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let body = body_string(r).await;
        assert!(
            body.contains(&crate::window::tty_map::ambiguous_target_error(2)),
            "{body}"
        );
        assert!(body.contains("\"status\":\"failed\""), "{body}");
        assert!(fake.recorded_keys().is_empty(), "拒绝必须零按键（不猜）");
        assert!(fake.recorded().is_empty(), "拒绝必须零文本注入");
        let audits = state
            .store
            .with(|c| crate::database::dao::write_audit::recent_conn(c, 10));
        assert_eq!(audits.len(), 1);
        assert_eq!(audits[0].action, "answer");
        assert_eq!(audits[0].result, "ambiguous_target");
        assert_eq!(audits[0].summary, "select#2", "摘要沿既有动作标签");
    }

    /// 问答作答对照（恰 1 候选 ⇒ 行为不变）：照常发数字键 + 审计 ok
    #[tokio::test]
    async fn question_answer_allows_with_single_candidate() {
        let (state, fake, _script) = stage_rig(
            vec![
                screen_fixtures::multi_option_focus(),
                screen_fixtures::multi_option_focus(),
            ],
            false,
        );
        let state = with_target_evidence(state, target_evidence_in_cwd(FIXTURE_CWD, &[82]));
        mark_question(&state, "claude", "sess_l13r", Q_SINGLE_PAYLOAD);
        let app = router(state.clone());
        let r = app
            .oneshot(req(
                "POST",
                "/m/api/v1/session-question/answer",
                Some("mam_device=mm"),
                Some(r#"{"sessionId":"sess_l13r","action":"select","index":1}"#),
            ))
            .await
            .unwrap();
        let body = body_string(r).await;
        assert!(body.contains("\"status\":\"key_sent\""), "{body}");
        assert_eq!(fake.recorded_keys(), vec![(82u32, "2".to_string())]);
        let audits = state
            .store
            .with(|c| crate::database::dao::write_audit::recent_conn(c, 10));
        assert_eq!(audits[0].result, "ok", "单候选路径零漂移");
    }

    /// 模式切换（键路 shift+tab）：≥2 候选 → 409 `ambiguous_target` + 零按键 + 审计
    /// `mode/ambiguous_target`（码形与本端点既有守卫拒绝同形：`error` + `reason`）
    #[tokio::test]
    async fn mode_switch_refuses_when_target_ambiguous() {
        let fake = FakeInjector::ok();
        let (state, sid) = mode_guard_state(
            fake.clone(),
            "sess_l13a",
            crate::session::AgentType::Claude,
            72,
            std::sync::Arc::new(|_, _| None),
        );
        let state = with_target_evidence(state, target_evidence_in_cwd(FIXTURE_CWD, &[72, 73]));
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state.clone());
        let r = app
            .oneshot(req(
                "POST",
                "/m/api/v1/session-mode/switch",
                Some("mam_device=mm"),
                Some(&format!(r#"{{"sessionId":"{sid}","target":"plan"}}"#)),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 409, "靶向歧义 = 守卫类拒绝 → 409");
        let body = body_string(r).await;
        assert!(body.contains("\"error\":\"ambiguous_target\""), "{body}");
        assert!(
            body.contains(&crate::window::tty_map::ambiguous_target_error(2)),
            "reason = plan 定形文案：{body}"
        );
        assert!(fake.recorded_keys().is_empty(), "拒绝必须零按键（不猜）");
        let audits = state
            .store
            .with(|c| crate::database::dao::write_audit::recent_conn(c, 10));
        assert_eq!(audits[0].action, "mode");
        assert_eq!(audits[0].result, "ambiguous_target");
    }

    /// 模式切换对照（恰 1 候选 ⇒ 行为不变）：照常发 shift+tab + 审计 ok
    #[tokio::test]
    async fn mode_switch_allows_with_single_candidate() {
        let fake = FakeInjector::ok();
        let (state, sid) = mode_guard_state(
            fake.clone(),
            "sess_l13b",
            crate::session::AgentType::Claude,
            72,
            std::sync::Arc::new(|_, _| None),
        );
        let state = with_target_evidence(state, target_evidence_in_cwd(FIXTURE_CWD, &[72]));
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state.clone());
        let r = app
            .oneshot(req(
                "POST",
                "/m/api/v1/session-mode/switch",
                Some("mam_device=mm"),
                Some(&format!(r#"{{"sessionId":"{sid}","target":"plan"}}"#)),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let body = body_string(r).await;
        assert!(body.contains("\"status\":\"key_sent\""), "{body}");
        assert_eq!(fake.recorded_keys(), vec![(72u32, "shift+tab".to_string())]);
        let audits = state
            .store
            .with(|c| crate::database::dao::write_audit::recent_conn(c, 10));
        assert_eq!(audits[0].result, "ok", "单候选路径零漂移");
    }

    /// 终端菜单单选（codex picker，action=open）：≥2 候选 → 409 `ambiguous_target` +
    /// **零投递零探测**（闸位在守卫探测之前）+ 审计 `mode/ambiguous_target`
    #[tokio::test]
    async fn mode_menu_refuses_when_target_ambiguous() {
        let fake = FakeInjector::ok();
        let (state, sid) = mode_guard_state(
            fake.clone(),
            "sess_l13c",
            crate::session::AgentType::Codex,
            81,
            std::sync::Arc::new(|_, _| None),
        );
        let state = with_target_evidence(state, target_evidence_in_cwd(FIXTURE_CWD, &[81, 82]));
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state.clone());
        let r = app
            .oneshot(req(
                "POST",
                "/m/api/v1/session-mode/menu",
                Some("mam_device=mm"),
                Some(&format!(r#"{{"sessionId":"{sid}","action":"open"}}"#)),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 409);
        let body = body_string(r).await;
        assert!(body.contains("\"error\":\"ambiguous_target\""), "{body}");
        assert!(
            body.contains(&crate::window::tty_map::ambiguous_target_error(2)),
            "{body}"
        );
        assert!(fake.recorded_keys().is_empty(), "拒绝必须零按键（不猜）");
        assert!(fake.recorded().is_empty(), "拒绝必须零文本注入");
        let audits = state
            .store
            .with(|c| crate::database::dao::write_audit::recent_conn(c, 10));
        assert_eq!(audits.len(), 1);
        assert_eq!(audits[0].action, "mode");
        assert_eq!(audits[0].result, "ambiguous_target");
    }

    /// 终端菜单对照（恰 1 候选 ⇒ 行为不变）：**不再报歧义**——落回本端点既有的
    /// 「屏读不可用/菜单读不到」失败面（本机假 pid 无真控制台；判据是「不是歧义拒绝」）
    #[tokio::test]
    async fn mode_menu_allows_with_single_candidate() {
        let fake = FakeInjector::ok();
        let (state, sid) = mode_guard_state(
            fake.clone(),
            "sess_l13d",
            crate::session::AgentType::Codex,
            81,
            std::sync::Arc::new(|_, _| None),
        );
        let state = with_target_evidence(state, target_evidence_in_cwd(FIXTURE_CWD, &[81]));
        persist_named_device(&state, "mm", "测试设备");
        let app = router(state.clone());
        let r = app
            .oneshot(req(
                "POST",
                "/m/api/v1/session-mode/menu",
                Some("mam_device=mm"),
                Some(&format!(r#"{{"sessionId":"{sid}","action":"open"}}"#)),
            ))
            .await
            .unwrap();
        let body = body_string(r).await;
        assert!(
            !body.contains("ambiguous_target"),
            "单候选不得报歧义：{body}"
        );
        assert!(
            !body.contains(&crate::window::tty_map::ambiguous_target_error(1)),
            "{body}"
        );
    }
}
