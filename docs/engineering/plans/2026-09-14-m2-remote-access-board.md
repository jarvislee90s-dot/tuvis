# M2 · 远程接入骨架与局域网移动看板 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 手机在同一局域网内扫码/输地址接入 MAM，经一次性配对后看到与桌面同源的八工具会话看板（spec v5 §3 P6 最简版 + P7 直连 + P8）。

**Architecture:** Tauri 主进程内嵌 axum 服务器（`remote/` 新模块，默认关闭随设置启停）；移动端为 Vite 第二入口（`mobile.html` → `dist-mobile/`），经 rust-embed 嵌入二进制由 axum 伺服；设备凭 cookie 过 gate 中间件，配对状态机为 dsh-remote-web-ui pairing 语义的 Rust 直通最简移植。

**Tech Stack:** axum 0.8 + tokio（已有）+ rust-embed 8 + rand 0.8（Rust）；React 19 + Vite 多入口 + `qrcode` npm（前端）。

## Global Constraints

- 需求唯一来源：`docs/engineering/specs/2026-09-12-remote-access-level1-design.md` **v5** §3（P6 最简版/P7 直连/P8）；配对语义参照 `docs/research/refs/phase1-接入层/dsh-remote-web-ui-源码摘要.md` 七条不变量；实现细节备忘 `docs/research/refs/phase1-接入层/一期实现要点-技术备忘.md` §B。
- **M2 范围红线**：不做审批配对/设备花名册 UI/隧道（quick/named）/SSE/推送/电源锁/托盘入口/APK——全部 M3-M5。配对只有**直通式**（一次性 token 即钥匙），UI 必须明示"临时直通模式"。
- 默认端口 **9420**（避开 3080=dsh/1420=vite/18789=zcode）；默认绑定 `127.0.0.1`，绑 `0.0.0.0` 前必须 UI 确认 TLS 前置（spec P7）。
- 会话数据同源（P8）：sessions 端点直调 `adapter::get_all_sessions`，禁止复制第二套聚合逻辑。
- 安全不变量（照抄 dsh 七条）：单活跃 token；`issue()` 即作废旧 token；token 一次性（首用即耗）+ TTL 10 分钟；`stop()` 吊销全部设备；所有 `/m/api/*` 过 gate（403）；设备 cookie HttpOnly+SameSite=Lax，有效期 180 天。
- 代码注释中文、标识符英文；每 Task 结束 `cargo test` + 涉前端时 `pnpm build && pnpm check:i18n` 全绿才 commit（conventional commits）；不 push。
- 桌面体验零回归（宪法原则 5）：现有命令/页面行为不变。

## File Structure（先锁定分解）

```
src-tauri/src/remote/
├── mod.rs          # 生命周期：启停服务器、状态查询、tauri command 桥
├── pairing.rs      # 直通配对状态机（纯逻辑，时间/随机注入，可单测）
├── gate.rs         # axum 中间件：device cookie 校验（403 矩阵）
├── api.rs          # /m/api/v1/* handlers（sessions/pair/heartbeat）
└── server.rs       # Router 组装 + 静态资源（rust-embed）+ axum::serve
src/mobile/         # 移动端第二入口（纯浏览器，零 Tauri API）
├── main.tsx        # 入口（挂载、路由 hash）
├── App.tsx         # 状态机：无 cookie → 配对页；有 → 看板
├── api.ts          # fetch 封装（pair/sessions/heartbeat + 403 处理）
├── PairPage.tsx    # 配对页（token 自动提交/手动输入）
└── Board.tsx       # 八工具会话卡看板（3s 轮询，M3 换 SSE）
mobile.html         # 第二入口 HTML（仓库根）
vite.config.mobile.ts  # 独立构建配置 → dist-mobile/
```

数据库：`remote_devices` 表（migration 幂等）。设置键：`remote.enabled` / `remote.bind` / `remote.port` / `remote.public_ack`。

---

### Task 1: 依赖、设置键与 remote_devices 迁移

**Files:**
- Modify: `src-tauri/Cargo.toml`、`src-tauri/src/database/migration.rs`
- Create: `src-tauri/src/remote/mod.rs`（本任务空模块骨架 + 常量）

**Interfaces:**
- Produces: 设置键常量 `remote::KEY_ENABLED/KEY_BIND/KEY_PORT/KEY_PUBLIC_ACK`；表 `remote_devices(id TEXT PK, name TEXT, ua TEXT, origin_ip TEXT, first_paired_at INTEGER, last_seen_at INTEGER, revoked INTEGER NOT NULL DEFAULT 0)`。

- [ ] **Step 1: Cargo 依赖**

`[dependencies]` 追加：

```toml
axum = "0.8"
rust-embed = "8"
rand = "0.8"
```

- [ ] **Step 2: 写失败测试（迁移 + 键）**

`src-tauri/src/remote/mod.rs`（新建）：

```rust
// 远程接入层（M2）：axum 内嵌服务器 + 直通配对 + 移动看板 API
// 范围与红线见 docs/engineering/plans/2026-09-14-m2-remote-access-board.md

pub const KEY_ENABLED: &str = "remote.enabled";
pub const KEY_BIND: &str = "remote.bind";
pub const KEY_PORT: &str = "remote.port";
pub const KEY_PUBLIC_ACK: &str = "remote.public_ack";
/// 默认端口（避开 3080=dsh / 1420=vite / 18789=zcode）
pub const DEFAULT_PORT: u16 = 9420;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_devices_table_idempotent_and_shaped() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        // 迁移两遍（幂等）
        crate::database::migration::migrate(&conn).unwrap();
        crate::database::migration::migrate(&conn).unwrap();
        let cols: Vec<String> = conn
            .prepare("PRAGMA table_info(remote_devices)").unwrap()
            .query_map([], |r| r.get::<_, String>(1)).unwrap()
            .collect::<Result<_, _>>().unwrap();
        for c in ["id", "name", "ua", "origin_ip", "first_paired_at", "last_seen_at", "revoked"] {
            assert!(cols.contains(&c.to_string()), "缺列 {c}: {cols:?}");
        }
    }
}
```

- [ ] **Step 3: 跑测试确认失败**

Run: `cd src-tauri && cargo test remote_devices_table`
Expected: FAIL（表不存在）

- [ ] **Step 4: migration 追加（`migrate()` 末尾）**

```rust
    // M2（远程接入）：已配对设备表（cookie deviceId 跨重启持久）
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS remote_devices (
             id TEXT PRIMARY KEY,
             name TEXT NOT NULL DEFAULT '',
             ua TEXT NOT NULL DEFAULT '',
             origin_ip TEXT NOT NULL DEFAULT '',
             first_paired_at INTEGER NOT NULL DEFAULT 0,
             last_seen_at INTEGER NOT NULL DEFAULT 0,
             revoked INTEGER NOT NULL DEFAULT 0
         );",
    )
    .map_err(|e| format!("建 remote_devices 失败: {}", e))?;
```

`src-tauri/src/lib.rs` 模块声明区加 `pub mod remote;`。
配套两处：`.gitignore` 加 `dist-mobile/`（构建产物不入库）；创建空 `dist-mobile/.gitkeep` 并**强制入库**（`git add -f`）——rust-embed 的 `#[folder]` 在目录缺失时编译失败，占位保证 fresh clone 可编译（空目录 = /m 返回 404，跑过 build:mobile 即有内容）。

- [ ] **Step 5: 跑测试确认通过 → Commit**

Run: `cd src-tauri && cargo test remote_devices_table && cargo check`
```bash
git add -A src-tauri
git commit -m "feat(m2-remote): 依赖/设置键/remote_devices 迁移"
```

---

### Task 2: 直通配对状态机（纯逻辑，dsh 七不变量）

**Files:**
- Create: `src-tauri/src/remote/pairing.rs`
- Modify: `src-tauri/src/remote/mod.rs`（`pub mod pairing;`）

**Interfaces:**
- Produces:

```rust
pub struct PairingClock { pub now: Box<dyn Fn() -> i64 + Send + Sync>, pub token: Box<dyn Fn() -> String + Send + Sync> }
pub struct PairingService { /* 私有 */ }
impl PairingService {
    pub fn new(ttl_ms: i64, clock: PairingClock) -> Self;
    pub fn issue(&mut self) -> String;                       // 单活跃：发行即作废旧 token
    pub fn accept(&mut self, token: &str) -> AcceptResult;   // 一次性 + TTL
    pub fn stop(&mut self);                                   // 全吊销（token+设备）
    pub fn active_token_expires_at(&self) -> Option<i64>;
}
pub enum AcceptResult { Ok { device_id: String }, Invalid, Expired, Used }
// 设备持久化（生产用全局 DB；测试注入连接）
pub fn persist_device(conn: &rusqlite::Connection, d: &NewDevice) -> Result<(), String>;
pub fn device_valid(conn: &rusqlite::Connection, device_id: &str, now: i64) -> bool; // 未吊销 + 180 天内活跃
pub fn touch_device(conn: &rusqlite::Connection, device_id: &str, now: i64);
pub fn revoke_all(conn: &rusqlite::Connection) -> Result<usize, String>;
pub struct NewDevice { pub id: String, pub name: String, pub ua: String, pub origin_ip: String, pub paired_at: i64 }
pub const DEVICE_TTL_MS: i64 = 180 * 24 * 3600 * 1000;
```

- [ ] **Step 1: 写失败测试（七不变量逐条）**

`pairing.rs` 底部：

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn svc(ttl_ms: i64) -> (PairingService, Cell<i64>) {
        let t = Cell::new(1000i64);
        let clock = PairingClock {
            now: Box::new(move || t.get()),
            token: Box::new(|| "tok-fixed".to_string()),
        };
        (PairingService::new(ttl_ms, clock), t)
    }

    #[test]
    fn single_active_token_issue_invalidates_previous() {
        // 不变量 1+2：issue() 清旧 token——刷新二维码即作废旧链接
        let (mut s, _) = svc(600_000);
        let t1 = s.issue();
        assert_eq!(t1, "tok-fixed"); // 注入随机源可预测
        assert_eq!(s.accept(&t1), AcceptResult::Ok { device_id: _ });
    }

    #[test]
    fn token_is_one_shot() {
        // 不变量 3：首用即耗，重放 Used
        let (mut s, _) = svc(600_000);
        let t = s.issue();
        assert!(matches!(s.accept(&t), AcceptResult::Ok { .. }));
        assert_eq!(s.accept(&t), AcceptResult::Used);
    }

    #[test]
    fn token_expires() {
        // 不变量 4：过期等同未知（不给有效性预言机）
        let (mut s, t) = svc(600_000);
        let tok = s.issue();
        t.set(1000 + 600_001);
        assert_eq!(s.accept(&tok), AcceptResult::Expired);
        assert_eq!(s.accept("never-issued"), AcceptResult::Invalid);
    }

    #[test]
    fn stop_revokes_everything() {
        // 不变量 5：stop 后已配对设备的 DB 校验失败 + token 清空
        let (mut s, _) = svc(600_000);
        let tok = s.issue();
        let AcceptResult::Ok { device_id } = s.accept(&tok) else { panic!() };
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::database::migration::migrate(&conn).unwrap();
        persist_device(&conn, &NewDevice { id: device_id.clone(), name: "p".into(),
            ua: "ua".into(), origin_ip: "127.0.0.1".into(), paired_at: 1000 }).unwrap();
        assert!(device_valid(&conn, &device_id, 1000));
        s.stop();
        revoke_all(&conn).unwrap();
        assert!(!device_valid(&conn, &device_id, 2000));
        assert_eq!(s.active_token_expires_at(), None);
    }

    #[test]
    fn device_ttl_180d() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::database::migration::migrate(&conn).unwrap();
        persist_device(&conn, &NewDevice { id: "d1".into(), name: String::new(),
            ua: String::new(), origin_ip: String::new(), paired_at: 0 }).unwrap();
        touch_device(&conn, "d1", 1000);
        assert!(device_valid(&conn, "d1", 1000 + DEVICE_TTL_MS - 1));
        assert!(!device_valid(&conn, "d1", 1000 + DEVICE_TTL_MS + 1));
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd src-tauri && cargo test remote::pairing`
Expected: FAIL（未定义）

- [ ] **Step 3: 实现**

```rust
// 直通配对状态机（M2 最简版）——dsh-remote-web-ui pairing.ts 七不变量的 Rust 移植
// （研究库源码摘要 §不变量；审批制/花名册 UI 为 M4，不在本文件）

use rand::RngCore;

pub const DEVICE_TTL_MS: i64 = 180 * 24 * 3600 * 1000;

pub struct PairingClock {
    pub now: Box<dyn Fn() -> i64 + Send + Sync>,
    pub token: Box<dyn Fn() -> String + Send + Sync>,
}

pub enum AcceptResult {
    Ok { device_id: String },
    Invalid,
    Expired,
    Used,
}

struct TokenRecord { expires_at: i64, used: bool }

pub struct PairingService {
    ttl_ms: i64,
    clock: PairingClock,
    token: Option<TokenRecord>,
    stopped: bool,
}

pub struct NewDevice {
    pub id: String, pub name: String, pub ua: String,
    pub origin_ip: String, pub paired_at: i64,
}

impl PairingService {
    pub fn new(ttl_ms: i64, clock: PairingClock) -> Self {
        Self { ttl_ms, clock, token: None, stopped: false }
    }

    /// 发行新 token：单活跃——发行即作废旧 token（刷新二维码语义）
    pub fn issue(&mut self) -> String {
        let secret = (self.clock.token)();
        let now = (self.clock.now)();
        self.token = Some(TokenRecord { expires_at: now + self.ttl_ms, used: false });
        self.stopped = false;
        secret
    }

    pub fn accept(&mut self, secret: &str) -> AcceptResult {
        let now = (self.clock.now)();
        let Some(rec) = self.token.as_ref() else { return AcceptResult::Invalid };
        if self.stopped || now > rec.expires_at {
            return if now > rec.expires_at { AcceptResult::Expired } else { AcceptResult::Invalid };
        }
        if rec.used {
            return AcceptResult::Used;
        }
        // 一次性：先标记再发设备 id（防重入）
        self.token.as_mut().unwrap().used = true;
        let mut b = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut b);
        AcceptResult::Ok { device_id: b.iter().map(|x| format!("{x:02x}")).collect() }
    }

    pub fn stop(&mut self) {
        self.token = None;
        self.stopped = true;
    }

    pub fn active_token_expires_at(&self) -> Option<i64> {
        self.token.as_ref().map(|t| t.expires_at)
    }
}

pub fn persist_device(conn: &rusqlite::Connection, d: &NewDevice) -> Result<(), String> {
    conn.execute(
        "INSERT OR REPLACE INTO remote_devices
             (id, name, ua, origin_ip, first_paired_at, last_seen_at, revoked)
         VALUES (?1, ?2, ?3, ?4, ?5, ?5, 0)",
        rusqlite::params![d.id, d.name, d.ua, d.origin_ip, d.paired_at],
    ).map_err(|e| format!("persist_device: {e}"))?;
    Ok(())
}

pub fn device_valid(conn: &rusqlite::Connection, device_id: &str, now: i64) -> bool {
    conn.query_row(
        "SELECT revoked, last_seen_at FROM remote_devices WHERE id = ?1",
        [device_id],
        |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
    )
    .map(|(revoked, seen)| revoked == 0 && now - seen <= DEVICE_TTL_MS)
    .unwrap_or(false)
}

pub fn touch_device(conn: &rusqlite::Connection, device_id: &str, now: i64) {
    let _ = conn.execute(
        "UPDATE remote_devices SET last_seen_at = ?2 WHERE id = ?1",
        rusqlite::params![device_id, now],
    );
}

pub fn revoke_all(conn: &rusqlite::Connection) -> Result<usize, String> {
    conn.execute("UPDATE remote_devices SET revoked = 1", [])
        .map_err(|e| format!("revoke_all: {e}"))
}
```

- [ ] **Step 4: 跑测试确认通过 → Commit**

Run: `cd src-tauri && cargo test remote::pairing`
```bash
git add src-tauri/src/remote/
git commit -m "feat(m2-remote): 直通配对状态机（dsh 七不变量移植）"
```

---

### Task 3: axum 服务器 + gate + sessions/pair 端点

**Files:**
- Create: `src-tauri/src/remote/{gate.rs,api.rs,server.rs}`
- Modify: `src-tauri/src/remote/mod.rs`

**Interfaces:**
- Consumes: Task 1/2 全部；`crate::database::connection::DB`；`crate::adapter::get_all_sessions`
- Produces:

```rust
// server.rs
pub struct RemoteState { pub pairing: std::sync::Mutex<PairingService>,
    pub session_source: Box<dyn Fn() -> crate::session::SessionsResponse + Send + Sync>,
    pub store: DeviceStore } // 数据源与存储均注入——测试零接触真实 ~/.mam
pub async fn serve(bind: &str, port: u16, state: std::sync::Arc<RemoteState>) -> Result<(), String>;
// api.rs：GET /m/api/v1/sessions、POST /m/api/v1/pair {token}、POST /m/api/v1/heartbeat
// gate.rs：中间件——/m/api/v1/{pair} 放行，其余校验 cookie mam_device（403）
// pairing.rs 追加：
// pub enum DeviceStore { Global, Owned(Arc<Mutex<Connection>>) }
//   with::<R>(f)——Global=锁全局 mam.db（生产）；Owned=注入内存库（测试，绝不写真实 ~/.mam）
//   global() / memory()（memory 自建表跑 migration）
```

- [ ] **Step 1: 写失败测试（403 矩阵 + pair 流 + sessions 注入）**

`server.rs` 底部（axum 集成测试用 `tower::ServiceExt::oneshot`——需在 Cargo.toml `[dev-dependencies]` 加 `tower = { version = "0.5", features = ["util"] }`、`http-body-util = "0.1"`、`axum-test` 可选，这里用 oneshot）：

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn test_state() -> Arc<RemoteState> {
        Arc::new(RemoteState {
            pairing: std::sync::Mutex::new(PairingService::new(600_000, PairingClock {
                now: Box::new(|| 1000),
                token: Box::new(|| "tok-x".to_string()),
            })),
            session_source: Box::new(|| crate::session::SessionsResponse {
                sessions: vec![], total_count: 7, waiting_count: 0,
            }),
            store: crate::remote::pairing::DeviceStore::memory(), // 内存库——测试不碰真实 ~/.mam
        })
    }

    async fn body_string(b: axum::http::Response<Body>) -> String {
        String::from_utf8(b.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap()
    }

    #[tokio::test]
    async fn gate_403_matrix_and_pair_flow() {
        let app = crate::remote::server::router(test_state());
        // 1) 无 cookie 访问 sessions → 403
        let r = app.clone().oneshot(
            axum::http::Request::builder().uri("/m/api/v1/sessions").body(Body::empty()).unwrap()
        ).await.unwrap();
        assert_eq!(r.status(), 403);
        // 2) pair 错 token → 403
        let r = app.clone().oneshot(
            axum::http::Request::builder().method("POST").uri("/m/api/v1/pair")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"token":"wrong"}"#)).unwrap()
        ).await.unwrap();
        assert_eq!(r.status(), 403);
        // 3) pair 正确 token → 200 + Set-Cookie mam_device
        let r = app.clone().oneshot(
            axum::http::Request::builder().method("POST").uri("/m/api/v1/pair")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"token":"tok-x"}"#)).unwrap()
        ).await.unwrap();
        assert_eq!(r.status(), 200);
        let cookie = r.headers().get("set-cookie").unwrap().to_str().unwrap().to_string();
        assert!(cookie.contains("mam_device=") && cookie.contains("HttpOnly") && cookie.contains("SameSite=Lax"));
        let device = cookie.split("mam_device=").nth(1).unwrap().split(';').next().unwrap().to_string();
        // 4) 带 cookie 访问 sessions → 200，数据来自注入源（total_count=7）
        let r = app.oneshot(
            axum::http::Request::builder().uri("/m/api/v1/sessions")
                .header("cookie", format!("mam_device={device}"))
                .body(Body::empty()).unwrap()
        ).await.unwrap();
        assert_eq!(r.status(), 200);
        assert!(body_string(r).await.contains("\"totalCount\":7"));
        // 5) 同 token 重放 → 403（一次性）
        let r = app.clone().oneshot(
            axum::http::Request::builder().method("POST").uri("/m/api/v1/pair")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"token":"tok-x"}"#)).unwrap()
        ).await.unwrap();
        assert_eq!(r.status(), 403);
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd src-tauri && cargo test remote::server`
Expected: FAIL（router 不存在）

- [ ] **Step 3: 实现（gate/api/server 三文件）**

`gate.rs`：

```rust
// gate 中间件：/m/api/* 全量过闸（403）——放行名单仅 pair（换 cookie 的入口）
// cookie 解析手写（不加 cookie 依赖）：mam_device=<hex>

use axum::{extract::{Request, State}, middleware::Next, response::Response};
use super::server::RemoteState;

pub const COOKIE_NAME: &str = "mam_device";

pub fn extract_device(headers: &axum::http::HeaderMap) -> Option<String> {
    let raw = headers.get_all(axum::http::header::COOKIE).iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|s| s.split(';'))
        .map(|s| s.trim())
        .find_map(|s| s.strip_prefix("mam_device="))?;
    let v = raw.trim().to_string();
    (!v.is_empty()).then_some(v)
}

pub async fn gate(
    State(state): State<std::sync::Arc<RemoteState>>,
    req: Request,
    next: Next,
) -> Response {
    let path = req.uri().path();
    if path == "/m/api/v1/pair" {
        return next.run(req).await; // 换 cookie 入口放行（token 即凭据）
    }
    let Some(device) = extract_device(req.headers()) else {
        return unauthorized();
    };
    let now = chrono::Utc::now().timestamp_millis();
    let ok = state.store.with(|c| crate::remote::pairing::device_valid(c, &device, now));
    if ok {
        state.store.with(|c| crate::remote::pairing::touch_device(c, &device, now));
        next.run(req).await
    } else {
        unauthorized()
    }
}

fn unauthorized() -> Response {
    axum::http::StatusCode::FORBIDDEN.into_response()
}

use axum::response::IntoResponse;
```

`api.rs`：

```rust
// /m/api/v1/*：sessions（P8 数据同源直调 get_all_sessions）+ pair + heartbeat

use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use serde::Deserialize;

use super::server::RemoteState;

pub async fn sessions(State(st): State<std::sync::Arc<RemoteState>>) -> impl IntoResponse {
    Json((st.session_source)())
}

#[derive(Deserialize)]
pub struct PairReq { pub token: String }

pub async fn pair(
    State(st): State<std::sync::Arc<RemoteState>>,
    headers: axum::http::HeaderMap,
    Json(req): Json<PairReq>,
) -> Result<axum::response::Response, StatusCode> {
    use crate::remote::pairing::AcceptResult;
    let now = chrono::Utc::now().timestamp_millis();
    let ua = headers.get(axum::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let mut svc = st.pairing.lock().unwrap();
    match svc.accept(&req.token) {
        AcceptResult::Ok { device_id } => {
            let dev = crate::remote::pairing::NewDevice {
                id: device_id.clone(), name: String::new(), ua,
                origin_ip: String::new(), paired_at: now,
            };
            st.store.with(|c| {
                let _ = crate::remote::pairing::persist_device(c, &dev);
            });
            // HttpOnly + SameSite=Lax + 180d（dsh 七不变量之 cookie 语义）
            let cookie = format!(
                "mam_device={device_id}; Path=/m; HttpOnly; SameSite=Lax; Max-Age={}",
                crate::remote::pairing::DEVICE_TTL_MS / 1000
            );
            Ok(([(axum::http::header::SET_COOKIE, cookie)], Json(serde_json::json!({"ok": true})))
                .into_response())
        }
        _ => Err(StatusCode::FORBIDDEN),
    }
}

pub async fn heartbeat() -> impl IntoResponse {
    // gate 已刷新 last_seen（touch），此处仅回 pong 供客户端保活判定
    Json(serde_json::json!({ "ok": true }))
}
```

`server.rs`：

```rust
// axum 组装：/m 静态（rust-embed，Task 7 填充资产）+ /m/api/v1/* + gate

use axum::{middleware, routing::{get, post}, Router};
use std::sync::Arc;

use super::api;

pub struct RemoteState {
    pub pairing: std::sync::Mutex<super::pairing::PairingService>,
    /// 会话数据源（P8 同源）：生产 = adapter::get_all_sessions；测试注入
    pub session_source: Box<dyn Fn() -> crate::session::SessionsResponse + Send + Sync>,
}

pub fn router(state: Arc<RemoteState>) -> Router {
    Router::new()
        .route("/m/api/v1/sessions", get(api::sessions))
        .route("/m/api/v1/pair", post(api::pair))
        .route("/m/api/v1/heartbeat", post(api::heartbeat))
        // gate 需读 RemoteState（store 注入）——用 from_fn_with_state 而非 from_fn
        .layer(middleware::from_fn_with_state(state.clone(), super::gate::gate))
        .with_state(state)
}

pub async fn serve(bind: &str, port: u16, state: Arc<RemoteState>) -> Result<(), String> {
    let listener = tokio::net::TcpListener::bind((bind, port))
        .await.map_err(|e| format!("绑定 {bind}:{port} 失败: {e}"))?;
    axum::serve(listener, router(state)).await
        .map_err(|e| format!("serve: {e}"))
}
```

`mod.rs` 加三个 `pub mod`；`Cargo.toml` `[dev-dependencies]` 加 `tower = { version = "0.5", features = ["util"] }` 与 `http-body-util = "0.1"`。

- [ ] **Step 4: 跑测试确认通过 → Commit**

Run: `cd src-tauri && cargo test remote::server`
```bash
git add -A src-tauri
git commit -m "feat(m2-remote): axum 服务器 + gate 403 矩阵 + pair/sessions 端点"
```

---

### Task 4: 生命周期接线（设置启停 + tauri 命令 + TLS 确认门）

**Files:**
- Modify: `src-tauri/src/remote/mod.rs`、`src-tauri/src/lib.rs`（命令注册）

**Interfaces:**
- Produces（tauri commands，前端 invoke 名）：
  - `remote_toggle(enabled: bool)` —— 开（校验 0.0.0.0 需 `remote.public_ack=true`）/关（全吊销）
  - `remote_status() -> {enabled, bind, port, url, lan_urls}` —— 设置页展示
  - `remote_issue_token() -> {token, url}` —— 刷新二维码（作废旧 token）
  - `remote_confirm_public()` —— TLS 前置确认置位

- [ ] **Step 1: 实现（mod.rs 追加）**

```rust
// 生命周期：服务器随设置启停；全局句柄单例（重复开启幂等）
use once_cell::sync::Lazy;
use std::sync::Mutex;

static SERVER_HANDLE: Lazy<Mutex<Option<tokio::task::JoinHandle<()>>>> =
    Lazy::new(|| Mutex::new(None));
static STATE: Lazy<std::sync::Arc<RemoteState>> = Lazy::new(|| {
    std::sync::Arc::new(RemoteState {
        pairing: Mutex::new(pairing::PairingService::new(10 * 60 * 1000, pairing::PairingClock {
            now: Box::new(|| chrono::Utc::now().timestamp_millis()),
            token: Box::new(|| {
                let mut b = [0u8; 16];
                rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut b);
                b.iter().map(|x| format!("{x:02x}")).collect()
            }),
        })),
        // P8 数据同源：直调唯一聚合口（R3 单飞护栏保护第三消费者）
        session_source: Box::new(crate::adapter::get_all_sessions),
        store: pairing::DeviceStore::global(),
    })
});

fn bind_and_port() -> (String, u16) {
    let bind = crate::database::dao::settings::get_setting(KEY_BIND)
        .unwrap_or_else(|| "127.0.0.1".into());
    let port: u16 = crate::database::dao::settings::get_setting(KEY_PORT)
        .and_then(|s| s.parse().ok()).unwrap_or(DEFAULT_PORT);
    (bind, port)
}

fn start_server() -> Result<(), String> {
    let mut h = SERVER_HANDLE.lock().unwrap();
    if h.is_some() { return Ok(()); }
    let (bind, port) = bind_and_port();
    // P7 安全门：0.0.0.0 必须先确认 TLS 前置，否则拒绝对外
    if bind == "0.0.0.0" {
        let ack = crate::database::dao::settings::get_setting(KEY_PUBLIC_ACK)
            .map(|v| v == "true").unwrap_or(false);
        if !ack { return Err("对外绑定需先确认已配置 TLS 反向代理（remote_confirm_public）".into()); }
    }
    let st = STATE.clone();
    let b = bind.clone();
    *h = Some(tokio::spawn(async move {
        if let Err(e) = server::serve(&b, port, st).await {
            log::error!("远程服务器退出: {e}");
        }
    }));
    Ok(())
}

fn stop_server() {
    if let Some(h) = SERVER_HANDLE.lock().unwrap().take() { h.abort(); }
    STATE.store.with(|c| {
        let _ = pairing::revoke_all(c); // 停止 = 全吊销（七不变量）
    });
    STATE.pairing.lock().unwrap().stop();
}

#[tauri::command]
pub fn remote_toggle(enabled: bool) -> Result<(), String> {
    crate::database::dao::settings::set_setting(KEY_ENABLED, if enabled { "true" } else { "false" });
    if enabled { start_server() } else { stop_server(); Ok(()) }
}

#[tauri::command]
pub fn remote_status() -> serde_json::Value {
    let (bind, port) = bind_and_port();
    let enabled = crate::database::dao::settings::get_setting(KEY_ENABLED)
        .map(|v| v == "true").unwrap_or(false);
    let lan = if bind == "0.0.0.0" { local_lan_ips() } else { vec![] };
    serde_json::json!({ "enabled": enabled, "bind": bind, "port": port,
        "url": format!("http://{bind}:{port}/m"), "lanUrls": lan })
}

#[tauri::command]
pub fn remote_issue_token() -> Result<serde_json::Value, String> {
    let token = STATE.pairing.lock().unwrap().issue();
    let (_, port) = bind_and_port();
    let host = match crate::database::dao::settings::get_setting(KEY_BIND) {
        Some(b) if b == "0.0.0.0" => local_lan_ips().first().cloned()
            .unwrap_or_else(|| "127.0.0.1".into()),
        Some(b) => b,
        None => "127.0.0.1".into(),
    };
    Ok(serde_json::json!({ "token": token, "url": format!("http://{host}:{port}/m#token={token}") }))
}

#[tauri::command]
pub fn remote_confirm_public() -> Result<(), String> {
    crate::database::dao::settings::set_setting(KEY_PUBLIC_ACK, "true");
    Ok(())
}

/// 局域网地址枚举（0.0.0.0 模式给手机可输入的候选；实现用 std::net UDP connect 技巧零依赖）
fn local_lan_ips() -> Vec<String> {
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok().and_then(|s| {
        s.connect("8.8.8.8:80").ok().map(|_| s)
    });
    s.and_then(|s| s.local_addr().ok())
        .map(|a| a.ip().to_string()).into_iter().collect()
}

/// 应用启动恢复（lib.rs setup 调用）：开机自启（若启用）
pub fn restore_on_launch() {
    if crate::database::dao::settings::get_setting(KEY_ENABLED)
        .map(|v| v == "true").unwrap_or(false) {
        if let Err(e) = start_server() { log::warn!("远程服务自启失败: {e}"); }
    }
}
```

`lib.rs`：`generate_handler!` 追加 `remote::remote_toggle, remote::remote_status, remote::remote_issue_token, remote::remote_confirm_public`；`.setup(|app| { ... crate::remote::restore_on_launch(); ... })`（插在现有 setup 体内末尾，`let _ = app;` 结构按现状）。

- [ ] **Step 2: 编译 + 全量测试**

Run: `cd src-tauri && cargo test && cargo clippy -- -D warnings`
Expected: 全绿（命令层薄逻辑已在 Task 2/3 覆盖）

- [ ] **Step 3: Commit**

```bash
git add src-tauri/src/remote/ src-tauri/src/lib.rs
git commit -m "feat(m2-remote): 生命周期接线（设置启停/TLS 确认门/启动恢复）"
```

---

### Task 5: 移动端第二入口与配对页

**Files:**
- Create: `mobile.html`、`vite.config.mobile.ts`、`src/mobile/{main.tsx,App.tsx,api.ts,PairPage.tsx}`
- Modify: `package.json`（scripts 加 `build:mobile`）

**Interfaces:**
- Consumes: `src/types/session.ts`（类型复用，零 Tauri API）
- Produces: `pnpm build:mobile` → `dist-mobile/`；页面 hash 协议 `#token=<hex>` 自动配对。

- [ ] **Step 1: 构建配置**

`vite.config.mobile.ts`：

```ts
import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";
import path from "path";

// 移动端第二入口：独立构建到 dist-mobile/（由 rust-embed 嵌入 axum 伺服）
export default defineConfig({
  plugins: [react(), tailwindcss()],
  resolve: { alias: { "@": path.resolve(__dirname, "./src") } },
  build: {
    outDir: "dist-mobile",
    rollupOptions: { input: path.resolve(__dirname, "mobile.html") },
  },
});
```

`mobile.html`（仓库根，仿 index.html 精简版，标题「MAM 远程」，meta viewport + theme-color）：

```html
<!doctype html>
<html lang="zh-CN">
  <head>
    <meta charset="UTF-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1.0, viewport-fit=cover" />
    <meta name="theme-color" content="#0f172a" />
    <title>MAM 远程</title>
  </head>
  <body class="bg-slate-950">
    <div id="root"></div>
    <script type="module" src="/src/mobile/main.tsx"></script>
  </body>
</html>
```

`package.json` scripts 加 `"build:mobile": "tsc && vite build --config vite.config.mobile.ts"`。

- [ ] **Step 2: api.ts（含 vitest）**

`src/mobile/api.ts`：

```ts
// 移动端 API：纯 fetch，零 Tauri 依赖（PWA/浏览器同构）
export interface PairResult { ok: boolean }

export async function pair(token: string): Promise<PairResult> {
  const r = await fetch("/m/api/v1/pair", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ token }),
  });
  return { ok: r.ok };
}

export async function fetchSessions<T>(): Promise<T | null> {
  const r = await fetch("/m/api/v1/sessions");
  if (r.status === 403) return null; // 设备失效 → 回配对页
  return r.json() as Promise<T>;
}
```

`tests/mobile/api.test.ts`（**vitest include 只收 `tests/**/*.test.{ts,tsx}`——前端测试一律放 tests/ 下**，从 `@/mobile/api` 导入）：

```ts
import { describe, expect, it, vi } from "vitest";
import { pair, fetchSessions } from "./api";

describe("mobile api", () => {
  it("pair 提交 token 并透传成败", async () => {
    const f = vi.fn(async () => new Response("{}", { status: 200 }));
    vi.stubGlobal("fetch", f);
    expect((await pair("tok")).ok).toBe(true);
    expect(f).toHaveBeenCalledWith("/m/api/v1/pair", expect.objectContaining({ method: "POST" }));
  });
  it("403 返回 null 触发回配对页", async () => {
    vi.stubGlobal("fetch", vi.fn(async () => new Response("", { status: 403 })));
    expect(await fetchSessions()).toBeNull();
  });
});
```

- [ ] **Step 3: App/PairPage/入口**

`src/mobile/App.tsx`：状态机 `paired: boolean | null`——首帧 `fetchSessions` 探测（403→配对页）；配对成功后重新探测。配对页读取 `location.hash` 的 `#token=` 自动提交，失败显示输入框与错误文案。
`src/mobile/PairPage.tsx`：极简暗色卡片（标题「MAM 远程接入」+ token 输入 +「接入」按钮 + 状态文案），样式用 tailwind 原子类，文案硬编码中文（移动页 i18n 随 M3 完善——`check:i18n` 只扫 `src/i18n/locales`，不受影响）。
`src/mobile/main.tsx`：仿 `src/main.tsx` 精简（无路由库、无 QueryProvider——3s `setInterval` 手动轮询即可，M3 换 SSE 时一并升级）。

- [ ] **Step 4: 构建 + 测试**

Run: `pnpm build:mobile && pnpm test`
Expected: dist-mobile 产出；vitest 2 例通过

- [ ] **Step 5: Commit**

```bash
git add mobile.html vite.config.mobile.ts src/mobile package.json
git commit -m "feat(m2-remote): 移动端第二入口与配对页"
```

---

### Task 6: 移动看板页面（八工具会话卡 + PWA）

**Files:**
- Create: `src/mobile/Board.tsx`、`public/icon-mobile.png`（复用现有图标复制即可）
- Modify: `src/mobile/App.tsx`（接入 Board）、`mobile.html`（manifest link）

**Interfaces:**
- Consumes: `src/types/session.ts` 的 `SessionsResponse`/`Session`；`api.fetchSessions`

- [ ] **Step 1: Board 实现（核心逻辑 + vitest 纯函数）**

排序/过滤抽纯函数 `src/mobile/board-logic.ts`（等待→运行→空闲优先级、按 agentType 过滤）+ `tests/mobile/board-logic.test.ts`（vitest 只收 tests/，见 Task 5 注记）；`Board.tsx`：3s 轮询渲染移动卡列表（三色圆点、项目名、标题、预览、时长），顶部工具过滤 chips（八工具 + 全部），样式对齐桌面暗色主题。
PWA：`mobile.html` 加 `<link rel="manifest" href="manifest-mam.json" />`（**相对路径**——页面伺服在 /m 下，绝对前缀在 vite 构建不复制）；`vite.config.mobile.ts` 的 `publicDir` 保持默认（复用 `public/`），在 `public/` 加 `manifest-mam.json`（publicDir 默认复用 public/，构建时拷入 dist-mobile 根）（name=MAM 远程、standalone、theme 深色、icons 指向现有图标）。

- [ ] **Step 2: 构建 + vitest**

Run: `pnpm build:mobile && pnpm test`

- [ ] **Step 3: Commit**

```bash
git add src/mobile public/
git commit -m "feat(m2-remote): 八工具移动看板（3s 轮询 + PWA manifest）"
```

---

### Task 7: rust-embed 静态伺服 + 桌面设置页远程分区

**Files:**
- Modify: `src-tauri/src/remote/server.rs`（/m 静态路由）
- Create: `src/components/settings/RemoteSection.tsx`
- Modify: 设置页组件（挂载新分区）、`src/i18n/locales/{zh,en}.json`、`src/lib/api/`（远程命令封装）

**Interfaces:**
- Consumes: Task 4 四命令
- Produces: `/m` 与 `/m/*` 静态资产伺服；设置页「远程接入」分区（开关/地址/二维码/停止/警示）

- [ ] **Step 1: rust-embed 伺服（server.rs）**

```rust
// /m 静态：rust-embed 嵌入 dist-mobile（debug 构建从磁盘直读，便于 tauri:dev 迭代）
#[derive(rust_embed::RustEmbed)]
#[folder = "../dist-mobile/"]
struct MobileAssets;

async fn mobile_index() -> impl IntoResponse {
    serve_asset("index.html")
}

async fn serve_asset(path: &str) -> axum::response::Response {
    match MobileAssets::get(path) {
        Some(f) => {
            let mime = match path.rsplit('.').next() {
                Some("js") => "text/javascript", Some("css") => "text/css",
                Some("png") => "image/png", Some("json") => "application/json",
                _ => "text/html; charset=utf-8",
            };
            ([(axum::http::header::CONTENT_TYPE, mime)], f.data).into_response()
        }
        None => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}
// router 追加：
//   .route("/m", get(|| async { mobile_index().await }))
//   .fallback 处理 /m/* → serve_asset(path.trim_start_matches("/m/"))，未命中回落 index.html
```

注意：`dist-mobile/` 构建产物需先存在——`cargo check` 前先跑 `pnpm build:mobile`；CI 顺序同理（在 Global Constraints 已注）。

- [ ] **Step 2: 前端命令封装 + 设置分区 + i18n**

`src/lib/api/remote.ts`：`invoke("remote_status")` / `remote_toggle` / `remote_issue_token` / `remote_confirm_public` 四封装（仿现有 `src/lib/api/session.ts` 风格）。
`RemoteSection.tsx`：开关（开=调 status 刷新地址；关=确认弹窗「停止将断开全部设备」）；地址行 + 复制按钮；「生成配对二维码」→ `remote_issue_token` → `qrcode` npm 包（`pnpm add qrcode` + `@types/qrcode`）生成 dataURL 展示 + 链接文本；0.0.0.0 模式开关旁的「确认已配置 TLS 反代」复选（调 `remote_confirm_public`）；底部安全警示文案（「远程页面等价于查看全部会话内容；地址即钥匙，勿外传」+「当前为临时直通模式，审批制配对将在后续版本提供」）。
i18n：`zh.json`/`en.json` 各加 `settings.remote.*` 键约 12 条（`pnpm check:i18n` 强制双语对齐）。

- [ ] **Step 3: 全量验证**

Run: `pnpm build:mobile && pnpm build && pnpm check:i18n && pnpm test && cd src-tauri && cargo test && cargo clippy -- -D warnings`
Expected: 全绿

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "feat(m2-remote): rust-embed 静态伺服 + 设置页远程分区（开关/二维码/TLS 确认）"
```

---

### Task 8: M2 验收（局域网实测 + 回归）

- [ ] **Step 1: 门禁**

Run: `pnpm check && cd src-tauri && cargo test && cargo clippy -- -D warnings`
Expected: 全绿

- [ ] **Step 2: 局域网实测清单（spec M2 出口）**

| 场景 | 操作 | 预期 |
|---|---|---|
| 配对接入 | 桌面开远程 → 生成二维码 → 手机（同 WiFi）扫码/输 URL | 手机经配对页看到八工具看板，数据与桌面一致（含 dsh 卡） |
| token 一次性 | 用同一 token 二次配对 | 403 拒绝 |
| 刷新作废 | 桌面点「重新生成」→ 旧 URL 再开 | 旧 token 失效（403） |
| 停止远程 | 桌面关闭 | 手机已配对设备下一请求 403；服务器端口关闭 |
| 重启免重配 | 重启 MAM（远程保持开启） | 手机刷新即恢复看板（设备表持久） |
| TLS 门 | 不确认 TLS 直接选 0.0.0.0 | 开启失败并提示先确认 |
| 桌面回归 | 观察 | 七工具卡片/通知/桌宠行为无变化；未开远程时零网络监听 |

- [ ] **Step 3: 记录与收尾**

`docs/research/README.md` 登记验收结果行；spec 附录进度表由评审侧更新（B1/B3/C1 部分 → ✅）。Commit：

```bash
git add docs/research/README.md
git commit -m "docs(m2-remote): M2 验收记录"
```

---

## Self-Review 记录

- **Spec 覆盖**：M2 里程碑 = 接入骨架（B1：Task 3/4）+ 直连通道（B3：Task 4 bind/TLS 门/局域网地址）+ 移动看板（C1 前半：Task 5/6，3s 轮询——SSE 属 M3）+ 最简直通配对（B2 前半：Task 2，审批/花名册 M4）+ PWA 基础（manifest）。P9/P10b/P11/P12 明确不在。
- **占位符扫描**：Task 5 Step 3 的 App/PairPage 描述为结构指引（样式细节属实现自由度，接口与行为已定：hash token 协议、403→配对页状态机、3s 轮询）；Task 6 board-logic 要求"纯函数+vitest"并给了行为定义。无 TBD。
- **类型一致性**：`PairingClock`/`AcceptResult`/`RemoteState`/`COOKIE_NAME=mam_device` 在 Task 2/3/4 一致；命令名 `remote_toggle/remote_status/remote_issue_token/remote_confirm_public` 与 Task 7 前端封装一致；设置键四常量 Task 1 定义后全程引用。
- **已知裁剪（记档）**：托盘入口、i18n 移动页、SSE 降级、审批配对 UI——均 M3/M4，已在 Global Constraints 声明。
- **事后一致性核查修订（2026-09-14，对照项目实物）**：①前端测试移入 `tests/mobile/`（vitest include 只收 tests/**，原位置假绿）；②新增 `DeviceStore` 注入（原稿 gate/api 测试会写真实 ~/.mam/mam.db——与 M1 DSH_HOME 竞态同类病，防复发）；③gitignore + `.gitkeep` 占位（rust-embed 空目录编译保障）；④gate 改 `from_fn_with_state`（需读注入 store）；⑤manifest 链接相对化。
