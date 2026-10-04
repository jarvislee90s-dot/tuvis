# H1–H12 实施计划 · APP 类注入与无头通道（二期收尾第一部分）

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 交付 spec H1–H12：四件前置修复（dsh 读侧诊断修复 / WB 发现双源 / TTY 靶向消歧 / 插队诚实化）+ 无头底座（开关/生命周期/回执/门控）+ zcode、codex APP、WorkBuddy、三家 CLI 的无头注入 + zcode 无头新建。

**Architecture:** 一次性进程 per turn（裁决 8）：每条消息 spawn 一次目标工具命令，回执归一后退出；三族通道（spawn 型 / WB 的 HTTP ACP / 在产终端注入）经路由表分派，共用一套底座（总开关默认关、按会话串行、watchdog 600s、取消、版本门控、审计）。素材库 = spec 附录 E（AionCore 源码级结论，claude argv 全集 / control_response 构造器 / codex 重放面等直接采用）。

**Tech Stack:** Rust（sysinfo 进程扫描 / tokio 异步 spawn / rusqlite 只读副本）· Tauri IPC · React 19 + TS（移动端 `/m` 与设置页）· ACP JSON-RPC over HTTP（WB）· 各家无头 CLI。

**上位 spec：** `docs/superpowers/specs/2026-09-27-phase2-closure-app-injection-design.md`（H 节为需求权威；裁决 1–19 不得重议；附录 D 探测定案 = 通道参数事实源；附录 E = 实现素材索引）。

**范围注记（审阅时裁决）：** 本计划覆盖 **H1–H12**（用户 2026-10-04 指令）；spec 裁决 19 现文为 H1–H13（H13 = dsh 写侧 ACP stdio）。若审定要并入 H13 → 在 Task 11 后追加 11b（结构同 Task 11，ACP over stdio）。

**分支与门禁（用户 2026-10-05 裁决）**：工作分支 `feat/h1-h12-headless`（off `origin/main` `88b43ae`）；**全部任务完成并通过 Task 15 统一手工测试前，一律不 push 到远端**——所有 commit 落本地分支；每 Task 一 commit，全门禁绿才 commit：`cd src-tauri && cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --check`；涉前端另跑 `pnpm test && pnpm build && pnpm format:check && pnpm lint`。`git add` 只加任务列明文件，**严禁 `-u`/`-A`/`.`**。

**已知实机环境（USER-ASSIST 点会标注）：** 本机 MAM dev 在跑（改后端需重启验证）；ZCode / DeepSeek Harness / WorkBuddy / ChatGPT.app(codex) 均已装；zcode CLI 0.16.9（`ELECTRON_RUN_AS_NODE=1 "D:/Program Files/ZCode/ZCode.exe" "D:/Program Files/ZCode/resources/glm/zcode.cjs"`）；codex 0.160.0；WB 5.7.3（Win 端点未启用——Task 11 有前置检查）；探测定案的证据目录 `~/mam-probe-closure/20261003-022408/`。

---

## 复用清单（现有实现与探测结果——执行者从这里取，不要重造）

**在产函数/模块（直接调用或同构仿写）**：

| 复用物 | 位置 | 用于任务 |
|---|---|---|
| `find_dsh_desktop_host_pid`（宿主进程扫描形态） | `monitor/dsh/mod.rs`（H1 交付） | Task 8 的 zcode APP 活跃探测仿其「遍历进程 → cmdline 特征匹配」骨架 |
| `mangle_project_path` / `session_jsonl_path` / `find_session_jsonl` / `derive_status_from_tail` / `title_from_db` | `monitor/workbuddy_parser.rs:130-259` | Task 2（db 源的状态映射）、Task 11（`projects/` 转写补扫与落盘佐证）直接调用 |
| 注入三端点骨架与审计双行口径 | `remote/api.rs:569+`（`一次 session-send 最多落两类审计行` 既有架构注释） | Task 5/8/9 的端点接线与 `action=headless` 审计照抄同构 |
| settings 键模式 | `remote/mod.rs` `KEY_ENABLED` 族 + `database::get/set_setting` | Task 5/6 的 `remote.headless_enabled/_timeout_ms/_concurrency` 照抄键命名 |
| codex 会话文件路径推导 | `monitor/codex_parser.rs`（`sessions/<Y/M/D>/rollout-*`） | Task 9 thread id = adapter 已有路径取 basename，不重写扫描 |
| `projcache::load` / `identity_matches` | `monitor/dsh/projcache.rs:19,55` | Task 1 修复对象本体（version 白名单在此） |
| `win32::collect_ancestor_pids` / 进程组口径 | `window/win32.rs:31` | Task 6 kill 树的进程侧查询 |
| 按会话串行 + W5 内容摘要口径 | `inject/queue.rs`（在产） | Task 6 串行层与审计摘要对齐，不另造口径 |

**探测定案事实（附录 D 直译，测试断言以此为真值）**：zcode 争用锁 1s 失败形态（Task 8 `classify_exit`）；codex `-C` 前置与单写者锁 -32600（Task 9）；WB ACP wire 序列与已结束会话静默挂（Task 11）；claude argv 全集与 control_response 规则（Task 13，附录 E ①②）；stdout 前缀污染样本（Task 6 receipt 测试）；`workbuddy.db` sessions 表 40 列结构（Task 2）；`recentProjects` 位置 `~/.zcode/v2/setting.json`（Task 12）。

---

## 文件结构总览

| 文件 | 职责 | 动作 |
|---|---|---|
| `src-tauri/src/monitor/dsh/log.rs` / `projcache.rs` | dsh 读侧（代际/projcache） | Task 1 诊断修复 |
| `src-tauri/src/monitor/workbuddy_parser.rs` | WB 发现（心跳） | Task 2 扩 db 双源 |
| `src-tauri/src/window/tty_map.rs`（新） | TTY↔终端窗口映射（L13） | Task 3 新建 |
| `src-tauri/src/inject/engine.rs` / `confirm.rs` / `queue.rs` | 靶向与回执 | Task 3/4 修改 |
| `src-tauri/src/remote/mod.rs` + `commands/settings.rs` + `RemoteSection.tsx` | H3 总开关 | Task 5 |
| `src-tauri/src/inject/headless/mod.rs`（新，含 `runner.rs`/`receipt.rs`/`gate.rs`） | H4/H6 底座 | Task 6 新建 |
| `src-tauri/src/inject/routing.rs` | 路由表扩展 | Task 7 |
| `src-tauri/src/inject/headless/zcode.rs` / `codex.rs` / `wb_acp.rs` / `cli_three.rs`（新） | 四组通道适配器 | Task 8/9/11/13 |
| `src-tauri/src/remote/api.rs` | session-send 接线 + 新建端点 | Task 8/12 |
| `src/mobile/*` + `tests/mobile/*` | 回执卡/置灰/新建表单 | Task 5/8/12 |
| `src-tauri/tests/headless_e2e.rs`（新） | `#[ignore]` 实机 E2E | Task 14 |

---

### Task 1: C0-① dsh 读侧修复（诊断驱动——v4 会话「无消息」根因定位与修复）

**背景（重要更正）**：`parse_generation`（`log.rs:46`）是通用解析，`session.v4.jsonl.zstd` **能过**代际门——此前「版本门挡 v4」的判断不成立。真实嫌疑（按序排查）：① `projcache.rs:65` 的 record wrapper version 白名单（rc.2 写 **`"version": 7`**，今晚实测文件即此——若 MAM 白名单只到 6 则整缓存被弃）；② v4 日志内容的 header/事件 schema 演进（`DshHeader` 解析或 identity 校验 `formatVersion==日志版本` 失败连锁）；③ zstd 多帧解码对 v4 新帧形态。

**Files:**
- Modify: `src-tauri/src/monitor/dsh/projcache.rs`（version 白名单 + 测试）
- Modify: `src-tauri/src/monitor/dsh/log.rs` 或 `decode.rs`（若诊断命中 ②/③）
- Test: 同文件 tests 模块 + 新夹具 `src-tauri/tests/fixtures/dsh-v4/`

- [ ] **Step 1: 制备真实夹具**——把今晚 MinerU 会话的真文件拷进测试夹具（只读复制，不碰用户数据）：

```bash
mkdir -p src-tauri/tests/fixtures/dsh-v4/session-1b6c5c45-probe
cp ~/.dsh/sessions/--E-LLMproject-MinerU_Convert--/session-1b6c5c45-2ef3-4033-8355-eacb066c7e6b/session.v4.jsonl.zstd src-tauri/tests/fixtures/dsh-v4/session-1b6c5c45-probe/
cp ~/.dsh/storages/session_projcache/sessions/session-1b6c5c45-2ef3-4033-8355-eacb066c7e6b.json src-tauri/tests/fixtures/dsh-v4/projcache-v7.json
```

- [ ] **Step 2: 写失败测试（诊断三连，先定位卡点）**——`projcache.rs` tests 模块追加：

```rust
    #[test]
    fn v7_projcache_and_v4_log_parse_end_to_end() {
        let fx = concat!(env!("CARGO_MANIFEST_DIR"), r"\tests\fixtures\dsh-v4");
        // ① 代际门：v4 应被识别为代际 4
        let logs = super::super::log::generation_logs(std::path::Path::new(fx).join("session-1b6c5c45-probe").as_path());
        assert!(logs.iter().any(|(v, _)| *v == 4), "v4 应过代际门: {logs:?}");
        // ② projcache v7 应可加载（当前疑似被 version 白名单拒绝）
        let pc = super::load(std::path::Path::new(fx), "projcache-v7") // 注：load 按 <id>.json 命名，夹具名对齐
            ;
        assert!(pc.is_some(), "projcache version 7 不应被弃缓存");
        // ③ v4 日志应能解码出 header 与正文行
        let best = super::super::log::read_best_generation(std::path::Path::new(fx).join("session-1b6c5c45-probe").as_path());
        assert!(best.is_some(), "v4 日志应可读");
    }
```

（夹具文件名与 `load` 的 `<session_id>.json` 约定对齐：`projcache-v7.json` + `load(fx, "projcache-v7")`。）

- [ ] **Step 3: 跑测试定位** `cd src-tauri && cargo test v7_projcache_and_v4_log`——**断言失败处即根因层**：② 失败 → 白名单问题（走 Step 4a）；③ 失败 → 解码/schema 问题（走 Step 4b）；全过 → 根因在更上层（会话目录发现/窗口过滤，转 Step 4c 加打印诊断）。

- [ ] **Step 4a: 修白名单**——`projcache.rs` 的 version 校验处（~L60-70）把已知版本集扩到 7：

```rust
// 已知 record wrapper version：1..=7（7 = 桌面端 rc.2 起，2026-10-04 实测夹具定案）
fn known_wrapper_version(v: i64) -> bool {
    (1..=7).contains(&v)
}
```

（以实际代码结构为准——若现状是 `match v { 1..=6 => .., _ => 弃 }` 就扩上界；保持「未知 version 弃缓存（backup-and-skip）」语义不弃。）

- [ ] **Step 4b: 修 v4 schema/解码**——按 Step 3 命中处补：`DshHeader` 未知字段容忍（serde `#[serde(default)]` 已有则查缺）、事件行新 type 的 fallthrough；**未知事件类型不猜、跳过计行**（对齐「未知帧不猜」纪律）。
- [ ] **Step 4c: 上层诊断**——在 `get_dsh_sessions` 加临时 eprintln 跑一次 `cargo test -- --nocapture dsh`，对照 `~/mam-probe-closure/` 的今晚文件路径确认目录/窗口过滤哪层丢弃。

- [ ] **Step 5: 门禁 + 实机核验**——`cargo test dsh && cargo clippy --all-targets -- -D warnings && cargo fmt --check`；USER-ASSIST：重启 dev MAM → dsh 卡正文应恢复（MinerU 今晚会话有标题有正文）、在 dsh APP 里发一条新消息 → 卡片刷新。
- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/monitor/dsh/ src-tauri/tests/fixtures/dsh-v4/
git commit -m "fix(dsh): rc.2 会话读侧修复——真实 v4/v7 夹具诊断驱动（代际✓/projcache 白名单/或 schema 层，按实测命中）"
```

### Task 2: C0-② H12 WorkBuddy 发现双源（心跳 ∪ workbuddy.db）

**Files:**
- Modify: `src-tauri/src/monitor/workbuddy_parser.rs`（`discover_workbuddy_processes` L366 + `get_workbuddy_sessions` L382）
- 新增纯核函数 + tests 同文件

- [ ] **Step 1: 写失败测试**——tests 模块追加（tempdir 造 workbuddy.db 副本形态）：

```rust
    #[test]
    fn db_source_surfaces_session_without_heartbeat() {
        // WB 5.7.3：交互会话无心跳，只在 workbuddy.db sessions 表——db 源应能独立发现
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("workbuddy.db");
        {
            let con = rusqlite::Connection::open(&db).unwrap();
            con.execute_batch(
                "CREATE TABLE sessions (id TEXT PRIMARY KEY, cwd TEXT, title TEXT,
                 status TEXT, updated_at INTEGER, transport TEXT, source_mode TEXT);
                 INSERT INTO sessions VALUES ('3f12ca20','E:/t2','你是什么模型',
                 'completed',1791121748142,'local','craft');",
            ).unwrap();
        }
        let rows = super::read_db_sessions(&db).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "3f12ca20");
        assert_eq!(rows[0].cwd, "E:/t2");
    }

    #[test]
    fn union_dedup_heartbeat_wins_on_conflict() {
        // 心跳在场的会话以心跳为准（活跃态更准），db 行只补无心跳会话
        let hb = super::parse_heartbeat(r#"{"pid":1,"lastHeartbeat":9,"sessionId":"a","cwd":"C:/x","kind":"interactive"}"#).unwrap();
        let db_rows = vec![super::DbSession { id: "a".into(), cwd: "C:/x".into(), title: "t".into(), status: "completed".into(), updated_at: 9 }];
        let merged = super::merge_sources(vec![hb], db_rows);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].source, super::Source::Heartbeat);
    }
```

- [ ] **Step 2: 确认失败** `cd src-tauri && cargo test db_source_surfaces`——`read_db_sessions`/`DbSession`/`merge_sources` 未定义。
- [ ] **Step 3: 实现**（workbuddy_parser.rs 追加；读取走**副本**，活库不直查——拷到 tempdir 再 open，对齐项目 SQLite 纪律）：

```rust
/// workbuddy.db sessions 表行（5.7.3+ 真相源；列语义按 2026-10-04 实测）
pub struct DbSession {
    pub id: String,
    pub cwd: String,
    pub title: String,
    pub status: String,      // completed/terminated/…
    pub updated_at: i64,     // ms epoch
}

/// 读 db 副本的 sessions 表（按 updated_at 倒序，限 24h 活动窗——对齐三层预算 L3）
pub fn read_db_sessions(db_copy: &Path) -> rusqlite::Result<Vec<DbSession>> {
    let con = rusqlite::Connection::open_with_flags(
        db_copy, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let mut st = con.prepare(
        "SELECT id, cwd, title, status, updated_at FROM sessions
         ORDER BY updated_at DESC",
    )?;
    let rows = st.query_map([], |r| Ok(DbSession {
        id: r.get(0)?, cwd: r.get(1)?, title: r.get(2)?,
        status: r.get(3)?, updated_at: r.get(4)?,
    }))?;
    rows.collect()
}

/// db mtime 变化才重拷重读（轮询预算：~/.workbuddy/workbuddy.db mtime 对比）
pub fn db_snapshot_fresh(home: &Path, last_mtime: &mut Option<SystemTime>) -> Option<Vec<DbSession>> { /* 拷副本→read_db_sessions；mtime 未变返回 None 跳过 */ }
```

`discover_workbuddy_processes`/`get_workbuddy_sessions` 接线：心跳路径原样保留；db 源经 `merge_sources`（心跳优先去重）补 AgentProcess/Session；`status` 映射三色（`completed`→绿、`terminated`→红·中断、运行中判据沿用转写 mtime 心跳口径）。`Source` 枚举标注来源供测试。
- [ ] **Step 4: 过测 + 门禁** `cargo test workbuddy && clippy && fmt`
- [ ] **Step 5: 实机核验（USER-ASSIST）**：重启 MAM → Test2 的 WB 会话（3f12ca20）上板。
- [ ] **Step 6: Commit** `git add src-tauri/src/monitor/workbuddy_parser.rs && git commit -m "feat(workbuddy): H12 发现双源——心跳 ∪ workbuddy.db sessions 表（5.7.3 心跳废弃适配）"`

### Task 3: C0-③ L13 注入靶向 TTY 消歧（同 cwd 多实例不再乱窜）

**Files:**
- Create: `src-tauri/src/window/tty_map.rs`（TTY↔终端窗口映射纯核 + macOS 采数）
- Modify: `src-tauri/src/inject/engine.rs`（靶向收窄入口）、`src-tauri/src/window/mod.rs`（`pub mod tty_map;`）
- Test: `tty_map.rs` tests + engine 侧多候选拒绝测试

- [ ] **Step 1: 写失败测试**（tty_map.rs）：

```rust
    #[test]
    fn exact_tty_match_beats_cwd_ambiguity() {
        // 三胞胎同 cwd：候选 = [(pid 44161, ttys000), (pid 44638, ttys006), (pid 45543, ttys007)]
        // 目标会话进程 tty = ttys006 → 唯一命中 pid 44638
        let cands = vec![(44161u32, "ttys000"), (44638, "ttys006"), (45543, "ttys007")];
        assert_eq!(super::resolve_by_tty(&cands, "ttys006"), Some(44638));
        assert_eq!(super::resolve_by_tty(&cands, "ttys999"), None);
    }

    #[test]
    fn no_tty_multi_candidate_rejects() {
        // 取不到 TTY：单候选放行；多候选必须拒绝（不猜）
        assert_eq!(super::fallback_cwd_policy(1), super::CwdFallback::Allow(1));
        assert_eq!(super::fallback_cwd_policy(3), super::CwdFallback::Reject(3));
    }
```

- [ ] **Step 2: 确认失败** → **Step 3: 实现**（探测定案的三家采数法）：

```rust
/// agent 进程 TTY：sysinfo 无直接口，经 `ps -o tty= -p <pid>`（macOS）；
/// Windows 本任务不采（走 fallback_cwd_policy 的拒绝规则同样生效）
pub fn agent_tty(pid: u32) -> Option<String> { /* std::process::Command ps，输出 trim */ }

/// 终端侧 TTY 清单：tmux `list-panes -a -F '#{pane_pid} #{pane_tty}'`；
/// Terminal.app / iTerm 经 AppleScript `tty of tab/session`（复用 window/ 既有脚本基建拼装）
pub fn terminal_ttys() -> Vec<(u32, String)> { /* ... */ }

pub fn resolve_by_tty(cands: &[(u32, &str)], target: &str) -> Option<u32> {
    cands.iter().find(|(_, t)| *t == target).map(|(p, _)| *p)
}

pub enum CwdFallback { Allow(u32), Reject(usize) }
pub fn fallback_cwd_policy(n: usize) -> CwdFallback {
    if n == 1 { CwdFallback::Allow/* 取唯一 pid */ } else { CwdFallback::Reject(n) }
}
```

engine 靶向入口接线：候选集 >1 且能取 TTY → `resolve_by_tty`；取不到 → `fallback_cwd_policy`，Reject 时返回 `Err("同目录存在 N 个候选会话，无法确定投递目标（已知缺口 L13 修复）：请关闭多余窗口或改用无头通道")`，审计 `action='fail' reason='ambiguous_target'`。Windows 侧 agent_tty 返回 None → 拒绝规则同样生效（Windows 等价键〔窗口-进程链〕登记后续优化）。
- [ ] **Step 4: 过测 + 门禁**；**Step 5: Commit** `git add src-tauri/src/window/tty_map.rs src-tauri/src/window/mod.rs src-tauri/src/inject/engine.rs && git commit -m "fix(inject): L13 靶向 TTY 消歧——精确匹配优先，多候选无 TTY 时拒绝并报错（不猜）"`

### Task 4: C0-④ L14 插队诚实化（macOS 假成功 → 中性 submitted）

**Files:**
- Modify: `src-tauri/src/inject/confirm.rs`（~L818 macOS 直接 Ok 分支）
- Modify: `src-tauri/src/inject/queue.rs` + `src-tauri/src/remote/api.rs`（jump 回执状态字段）
- Modify: `src/mobile/*`（「已送达终端」→「已投递未确认」文案，仅 submitted 态）
- Test: confirm.rs / queue.rs tests

- [ ] **Step 1: 写失败测试**（confirm.rs tests——翻转语义）：

```rust
    #[test]
    fn macos_unverifiable_jump_reports_submitted_not_delivered() {
        // L14：确认面不可达（非 Windows 无屏读）→ 不得返回 delivered；
        // 应返回 Submitted（中性：已投递未确认）——比照 codex 诚实失败范式
        let r = super::confirm_jump(ConfirmCtx::mock_platform("macos"));
        assert!(matches!(r, JumpConfirm::Submitted { note: _ }));
    }
```

（`ConfirmCtx`/`JumpConfirm` 按现有类型接缝 mock；核心断言 = **不再是 Delivered**。）
- [ ] **Step 2: 确认失败** → **Step 3: 实现**：`JumpConfirm` 枚举加 `Submitted { note: String }` 变体；confirm.rs macOS 分支由 `Ok(→Delivered)` 改返回 `Submitted { note: "macOS 无占用/屏读确认面，键已投递未验证消费（L14 诚实化）" }`；api.rs jump 回执 `status` 透传 `submitted`；queue.rs `DELIVERY_TIMEOUT_MSG` 防重口径不动（那是 Windows 排空超时语义）。移动端按 `submitted` 渲染「已投递未确认」黄色态（区别于 delivered 绿）。
- [ ] **Step 4: 过测 + 全门禁（含 pnpm）**；**Step 5: Commit** `git add src-tauri/src/inject/confirm.rs src-tauri/src/inject/queue.rs src-tauri/src/remote/api.rs src/mobile/ tests/mobile/ && git commit -m "fix(inject): L14 插队回执诚实化——确认面不可达报 submitted 不报 delivered（macOS 假成功根修）"`

### Task 5: C1-① H3 无头通道总开关（默认关）

**Files:**
- Modify: `src-tauri/src/remote/mod.rs`（`pub const KEY_HEADLESS: &str = "remote.headless_enabled";` 纳入 status 组装）
- Modify: `src-tauri/src/remote/api.rs`（session-send 路由到无头通道前的开关校验）
- Modify: `src/components/settings/RemoteSection.tsx` + `src/i18n/locales/{zh,en}.json`（「无头注入」开关 + 一次性安全说明）
- Test: `src-tauri/src/remote/api.rs` tests 模块（既有远程端点测试同位追加）+ `tests/`（RemoteSection 前端用例，沿用既有 settings 区测试文件）

- [ ] **Step 1: 写失败测试**（后端）：开关关闭时对 zcode 会话 session-send → `403 {"error":"headless_disabled"}`；开启后放行到路由层。前端：RemoteSection 渲染开关、默认 off、点开弹安全说明（i18n 双语键 `remote.headless.title/hint/confirm`）。
- [ ] **Step 2: 确认失败** → **Step 3: 实现**：`database::get_setting(KEY_HEADLESS)` 默认 `"false"`；`remote_status` JSON 加 `headlessEnabled`；RemoteSection 三件套之第一件（本任务只放开关，超时/并发上限控件随 Task 6 的配置落点一起加）；开关翻转写审计 `action='setting' detail='headless=<v>'`。安全说明文案（zh）：「无头注入将在不经过终端可视确认的情况下直接驱动 Agent 执行消息（含工具调用）。zcode 通道默认 yolo 档（不弹审批）。开启即表示知悉。」
- [ ] **Step 4: 全门禁**；**Step 5: Commit** `git commit -m "feat(remote): H3 无头总开关——默认关 + 状态下发 + 开启安全说明 + 审计"`

### Task 6: C1-② H4+H6 无头底座（runner / receipt / 版本门控）

**Files:**
- Create: `src-tauri/src/inject/headless/mod.rs`（`pub mod runner; pub mod receipt; pub mod gate;` + 公共类型）
- Create: `src-tauri/src/inject/headless/receipt.rs`、`runner.rs`、`gate.rs`
- Modify: `src-tauri/src/inject/mod.rs`（挂模块）、`src-tauri/src/database/`（审计 action 枚举扩 `headless`/`headless_cancel`——migration 若为 CHECK 约束则随批）
- Test: 三文件 tests（纯核为主，spawn 用 `cmd /c echo` 类无害命令做缝测试）

- [ ] **Step 1: receipt.rs 失败测试 + 实现**（归一结构 + 前缀跳过解析）：

```rust
    #[test]
    fn parses_json_after_noise_prefix_lines() {
        // Mac 实测：JSON 前有 "ZCode Built-in missing" 等非 JSON 行——跳过前缀取首个 '{' 起
        let raw = "ZCode Built-in skipped (not-due)\n{\"sessionId\":\"s1\",\"response\":\"hi\"}";
        let v = super::parse_json_skipping_prefix(raw).unwrap();
        assert_eq!(v["sessionId"], "s1");
    }

    #[test]
    fn receipt_normalization_maps_stages() {
        let r = super::Receipt::failed(Stage::Timeout, "watchdog 600s");
        assert_eq!(r.status, ReceiptStatus::Failed);
        assert!(matches!(r.stage, Some(Stage::Timeout)));
    }
```

`Receipt { status: Ok|Queued|Failed|Cancelled, session_id, last_assistant(截断 200 字), tokens: Option<u64>, duration_ms, stage: Option<Stage>, reason: Option<String> }`；`Stage { Spawn, VersionGate, Timeout, Crash, ChannelError, Dialog, WorkspaceBusy }`（WorkspaceBusy = zcode 争用锁专档，Task 8 用）。
- [ ] **Step 2: runner.rs 失败测试 + 实现**（生命周期纯核 + tokio 缝）：

```rust
    #[test]
    fn watchdog_fires_and_kills_tree_stub() {
        // 用注入的 wait 闭包模拟超时（不真 sleep 600s）
        let mut r = super::RunnerCfg::for_test().timeout_ms(50);
        let out = r.run_once(|_| std::thread::sleep(std::time::Duration::from_millis(500)));
        assert!(matches!(out.stage, Some(super::super::receipt::Stage::Timeout)));
    }

    #[test]
    fn concurrency_cap_queues_second_turn_globally() {
        // 全局上限 2（裁决 15 语义为 watchdog；并发上限 = H4 默认 2 可配）
        let sem = super::GlobalSem::new(2);
        let _a = sem.acquire(); let _b = sem.acquire();
        assert!(!sem.try_acquire(), "第三个应排队");
    }
```

runner 生产路径：`tokio::process::Command::spawn` → `Stdio::piped` 读 stdout 流 → 前缀跳过 + 增量 JSON 行解析 → 进程 `wait().await` 与 `tokio::time::timeout(600s)` select（超时侧 `kill_tree(pid)`：Windows 优先 Job Object〔附录 E-⑥〕、保底 `taskkill /T /F`；POSIX 杀进程组）→ 归一 Receipt；`cancel()` 句柄供移动端取消（与 watchdog 先到者生效，回执注明终止方）；审计落账 `action=headless`（内容摘要对齐 W5 口径）+ `headless_cancel`。
- [ ] **Step 3: gate.rs 失败测试 + 实现**（版本门控探针，缓存 + 漂移提示）：

```rust
    #[test]
    fn zcode_probe_uses_prompt_dryrun_not_version() {
        // 附录 E/探测定案：--version 不需要 provider config 会漏判——探针必须 --prompt 干跑
        let spec = super::ProbeSpec::Zcode { exe: "D:/Program Files/ZCode/ZCode.exe".into(), cjs: "D:/Program Files/ZCode/resources/glm/zcode.cjs".into() };
        let argv = super::probe_argv(&spec);
        assert!(argv.iter().any(|a| a == "--prompt"), "探针须 --prompt 干跑: {argv:?}");
        assert!(!argv.iter().any(|a| a == "--version"), "探针不得用 --version（会漏判 provider 缺失）: {argv:?}");
    }
```

探针：zcode = `--prompt "" --mode yolo --json` 干跑（超时 15s，能出 JSON 即过；Mac 缺 env var 时报 VersionGate 并附修复提示）；codex = `queue --help` 子命令在场；结果缓存（进程存在性 + mtime 键）。
- [ ] **Step 4: 设置页「无头」子区三件套收齐**——Task 5 只放了总开关，本步补另两件：`remote.headless_timeout_ms`（默认 600000）与 `remote.headless_concurrency`（默认 2）的 RemoteSection 控件 + `remote_status` 下发 + runner 启动时读取（spec H4 配置落点）。前端用例：三控件渲染 + 默认值断言。
- [ ] **Step 5: 全门禁**；**Step 6: Commit** `git commit -m "feat(headless): H4/H6 底座——runner(600s watchdog/kill树/取消/并发2) + receipt 归一(前缀跳过) + 版本门控探针 + 无头子区三件套 + 审计"`

### Task 7: C1-③ 路由表扩展（H 系通道入路由）

**Files:**
- Modify: `src-tauri/src/inject/routing.rs`（`Channel`/`Visibility`/`tool_gate`/`route`）
- Test: 同文件 tests（翻转既有 app_form 断言）

- [ ] **Step 1: 写失败测试**：

```rust
    #[test]
    fn app_tools_route_to_headless_channels() {
        // 裁决 1/12/16：四家 APP 形态入无头通道（不再一律 app_form 拒绝）
        let r = route("zcode", app(), 100, "windows");
        assert!(matches!(r, RouteOutcome::Injectable { candidates, visibility }
            if candidates == vec![Channel::Headless(HeadlessKind::Zcode)] && visibility == Visibility::AfterRestart));
        let r = route("codex", app(), 100, "windows");
        assert!(matches!(r, RouteOutcome::Injectable { candidates, .. }
            if candidates.contains(&Channel::Headless(HeadlessKind::CodexQueue))));
        let r = route("workbuddy", app(), 100, "windows");
        assert!(matches!(r, RouteOutcome::Injectable { candidates, .. }
            if candidates.contains(&Channel::Headless(HeadlessKind::WbAcp))));
    }

    #[test]
    fn cli_forms_keep_terminal_channels() {
        // 终端形态不动（W3：无头对已开 TUI 不路由）
        let r = route("claude", cli(), 100, "macos");
        assert!(matches!(r, RouteOutcome::Injectable { .. }));
    }
```

- [ ] **Step 2: 确认失败** → **Step 3: 实现**：`Channel` 加 `Headless(HeadlessKind)`；`HeadlessKind { Zcode, CodexQueue, CodexExec, WbAcp, ClaudeP, KimiP, OpencodeRun }`；`Visibility` 加 `AfterRestart`（zcode 已信任）/ `MamOnly`（zcode 未信任）——携元数据 `visibility_note: &'static str`（移动端提示文案键）；`tool_gate` 重写：workbuddy/dsh/zcode 不再一律黑盒——zcode→Headless(Zcode)、workbuddy→Headless(WbAcp)、dsh CLI 形态→终端注入保留 + APP 形态→NotInjectable(reason_code="dsh_headless_pending")〔**范围注记：若审阅裁 H13 并入 → 此分支改路由 Headless(DshAcp) 并在 HeadlessKind 加变体**〕；codex 按形态分派（cli→终端 / app→CodexQueue）。既有 `app_form` 断言测试翻转语义。
- [ ] **Step 4: 全门禁**；**Step 5: Commit** `git commit -m "feat(routing): H 系无头通道入路由——四家 APP 形态分派 + 可见性元数据（AfterRestart/MamOnly）"`

### Task 8: C1-④ H7 zcode 无头适配器 + session-send 接线 + 回执卡

**Files:**
- Create: `src-tauri/src/inject/headless/zcode.rs`
- Modify: `src-tauri/src/remote/api.rs`（session-send 路由分派到 zcode 通道）
- Modify: `src/mobile/SessionDetail.tsx`（无头回执卡：发送中/回执/失败分診/取消按钮）
- Test: zcode.rs tests + `tests/mobile/SessionDetail.test.tsx` 追加

- [ ] **Step 1: 写失败测试**（zcode.rs 纯核——命令构造与争用锁判定）：

```rust
    #[test]
    fn argv_windows_and_mac_diverge() {
        // 探测定案 D1：Mac 必须补 ZCODE_BUILTIN_PROVIDER_CONFIG_FILE
        let w = super::build_argv(&super::ZcodeSpec::win("D:/Program Files/ZCode"), "你好 [mobile iPad]", "sess_1", "E:/p", None);
        assert!(w.iter().any(|a| a.contains("zcode.cjs")));
        assert!(w.windows(2).any(|p| p[0] == "--mode" && p[1] == "yolo")); // 裁决 14
        let m = super::build_argv(&super::ZcodeSpec::mac("/Applications/ZCode.app"), "hi", "sess_1", "/tmp/p", None);
        assert!(m.env.contains_key("ZCODE_BUILTIN_PROVIDER_CONFIG_FILE"));
        assert!(m.env.contains_key("ELECTRON_RUN_AS_NODE"));
    }

    #[test]
    fn workspace_busy_maps_to_retryable_stage() {
        // 探测定案：APP 活跃工作区 → "Model creation failed" 1s 退出 → Stage::WorkspaceBusy（探活重试）
        let r = super::classify_exit(&super::ExitObs { code: 0, stderr_head: "", stdout_head: "Error: Model creation failed", duration_ms: 1000 });
        assert!(matches!(r.stage, Some(Stage::WorkspaceBusy)));
        assert!(r.retryable);
    }
```

- [ ] **Step 2: 确认失败** → **Step 3: 实现**：`build_argv`（`--prompt <text> --resume <sess> --cwd <proj> --mode yolo --json`；Win/Mac 两形态 + env 集）；执行走 Task 6 runner；`WorkspaceBusy` → 探活重试（APP 工作区活跃探测 = `find_dsh_desktop_host_pid` 同款扫描法的 zcode 版：查 ZCode APP 是否活跃于该项目——按 app-server 进程 cwd 扫描，探测定案口径）最多 2 次（间隔 5s），仍忙 → 回执失败 +「工作区忙」原因；成功 → Receipt + 可见性提示文案（已信任=「重启 ZCode 应用后可见」/未信任=「仅 MAM 可见」，信任判定 = recentProjects 含该项目路径——读 `~/.zcode/v2/setting.json` 只读）。api.rs：`Headless(Zcode)` 分派 → 会话串行锁（per-session MutexMap）→ runner → 回执 + 审计。移动端回执卡三态 + 取消按钮（调 `/session-headless-cancel`）。
- [ ] **Step 4: 全门禁**；**Step 5: USER-ASSIST 实机**：手机对探测留下的自建 zcode 会话（`sess_4d37f1f3` 等）发一条 → 回执卡出（末条 assistant + token + 耗时）→ 重启 ZCode APP 后 Test2 工作区可见该回合。
- [ ] **Step 6: Commit** `git commit -m "feat(zcode): H7 无头发消息——平台分叉 argv/争用锁探活重试/yolo/回执卡 + session-send 接线"`

### Task 9: C1-⑤ H8 codex APP 适配器（queue 主 / exec resume 兜底）

**Files:**
- Create: `src-tauri/src/inject/headless/codex.rs`
- Modify: `src-tauri/src/remote/api.rs`（分派）
- Test: codex.rs tests

- [ ] **Step 1: 写失败测试**：

```rust
    #[test]
    fn dispatch_by_app_presence() {
        // 探测定案：APP 开（thread 被打开）→ queue；关 → exec resume（-C 前置！）
        let d = super::dispatch(super::AppPresence::Open, "01a10735-…", "E:/t2", "hi [mobile]");
        assert!(matches!(d, super::Plan::Queue { .. }));
        let d = super::dispatch(super::AppPresence::Closed, "01a10735-…", "E:/t2", "hi");
        assert!(matches!(d, super::Plan::ExecResume { argv, .. } if argv[1] == "-C")); // codex -C <dir> exec resume …
    }

    #[test]
    fn thread_uuid_from_rollout_filename() {
        assert_eq!(super::thread_id_of("rollout-2026-10-04T21-58-01-01a10735-1354-7d10-822a-f3bd9e041c12.jsonl"),
            Some("01a10735-1354-7d10-822a-f3bd9e041c12"));
    }
```

- [ ] **Step 2: 确认失败** → **Step 3: 实现**：AppPresence 判定 = 会话宿主 form + ChatGPT.app 进程在场扫描；`Plan::Queue` → `codex queue --thread <UUID> --message <text>`（回执 = message-id + exit 0 只证入队）→ **消费确认自建**：轮询目标 rollout 文件追加（命中消息文本 = 已消费；60s 未消费且 `queue_1.sqlite` 副本查到该 thread 滞留 → 回执「已入队未消费（thread 未被 APP 打开），建议在 APP 打开该会话或改走 exec」）；`Plan::ExecResume` → `codex -C <dir> exec resume <UUID> "<text>" --skip-git-repo-check`（runner 跑，stdout 尾行=末条回复）；单写者锁报错（`already has an active writer`）→ 自动改道 Queue 并如实注明。id 映射：MAM codex 会话 → rollout 文件名 UUID（adapter 读链已有文件路径，取 basename）。
- [ ] **Step 4: 全门禁**；**Step 5: USER-ASSIST 实机**：手机对 Test2 的 codex APP 会话发消息 → APP 内 ~1min 出现并执行。
- [ ] **Step 6: Commit** `git commit -m "feat(codex): H8 APP 托管会话注入——queue 主/exec resume 兜底按 APP 在场分派 + 消费确认自建 + UUID 唯一"`

### Task 10: C1-⑥ H5 审批与权限档（参数化 + 边界落码）

**Files:**
- Modify: `src-tauri/src/inject/headless/mod.rs`（`PermissionSpec` 参数面）+ zcode.rs/codex.rs 接线
- Modify: `src/mobile/SessionDetail.tsx`（审批卡数据接口预留——本任务仅类型与占位渲染）
- Test: mod.rs tests

- [ ] **Step 1: 失败测试 + 实现**：`PermissionSpec { Zcode(Yolo 固定——裁决 14), CodexExec(policy: "on-request" 默认), ClaudeP(Stdio /* C4 用 */, KimiDefault, OpencodeDefault }`——spawn 构造器按 spec 注入旗子；断言 zcode argv 恒 `--mode yolo`、codex exec 恒带 `-c approval_policy=...`；移动端审批卡接口 `HeadlessApprovalRequest`（type 定义 + 渲染占位「无头通道审批将在 claude 通道（C4）启用」）。边界注释落码：codex queue 无审批面（H8 定案）、zcode yolo 无审批（裁决 14 + H3 知情文案兜底）。
- [ ] **Step 2: 全门禁**；**Step 3: Commit** `git commit -m "feat(headless): H5 权限档参数化——zcode yolo 固定/策略驱动旗子/审批卡接口预留(C4)"`

### Task 11: C2 H9 WorkBuddy ACP 适配器（HTTP 型）

**Files:**
- Create: `src-tauri/src/inject/headless/wb_acp.rs`（`MockHttp` 为本任务内定义的注入缝：`Box<dyn Fn(HttpReq) -> HttpResp>` 封装，非外部依赖）
- Modify: `src-tauri/src/monitor/workbuddy_parser.rs`（读链路补扫 `~/.workbuddy/projects/<munged-cwd>/*.jsonl`——ACP 会话不进 db，Task 2 的 db 源扫不到）
- Test: wb_acp.rs tests（wire 纯核用 MockHttp）

- [ ] **Step 1: 写失败测试**（协议序列纯核——Mac 实测 wire 为准）：

```rust
    #[test]
    fn acp_handshake_and_prompt_sequence() {
        let mut http = MockHttp::new();
        http.expect_post("/api/v1/acp/connect", None).respond_json(r#"{"connectionId":"c1","sessionToken":"t1"}"#);
        http.expect_post("/api/v1/acp", headers(&["acp-connection-id","acp-session-token","Accept: application/json, text/event-stream"]))
             .respond_sse("session/update: agentPhase=model_requesting\n:ok\n");
        let client = super::AcpClient::new("http://127.0.0.1:63928", http);
        let sess = client.handshake().unwrap();
        assert_eq!(sess.token, "t1");
        let ev = client.prompt(&sess, "probe [mobile]", SessionSel::New { cwd: "E:/t2" }).unwrap();
        assert_eq!(ev.first_phase, "model_requesting");
    }

    #[test]
    fn ended_session_prompt_reports_revival_needed() {
        // 探测定案：load 已结束会话 → 200+心跳但回合不推进 → 不许静默挂：60s 无 agentPhase 事件即判「已结束需复活」
        // → Receipt failed(stage=ChannelError, reason="会话已结束（end_turn），需复活语义——引导用户在 APP 重开或走新建")
    }
```

- [ ] **Step 2: 确认失败** → **Step 3: 实现**：endpoint 发现双路：Mac/旧版 = 心跳 `endpoint` 字段；Win 5.7.3 = **前置检查**——无心跳无端点 → 直接回执 `channel_unavailable`（文案：「WorkBuddy 远程控制端点未启用——请在 WorkBuddy 设置中开启远程控制（风险 16 跟进项）」），不盲扫；启用后（USER-ASSIST 确认）补端口发现（`Get-NetTCPConnection` 过滤 WB pid → `/` 返回 CodeBuddy Remote Control 标题指纹 + `/health` `{"status":"UP"}` 双确认）。协议流：connect → initialize（能力协商）→ 目标语义二分（活跃 = `session/load`+prompt；新建 = `session/new`+prompt）→ SSE 事件归一（agentPhase/session_update → Receipt）；转写佐证（projects/ 落盘侦测）作二次确认。
- [ ] **Step 4: 全门禁**；**Step 5: USER-ASSIST（Mac 或 Win 启用后）实机**：对活跃 WB 会话发消息 → APP 内出现并执行。
- [ ] **Step 6: Commit** `git commit -m "feat(workbuddy): H9 ACP 注入——免鉴权握手/活跃-新建二分/已结束复活提示/读链路补扫 projects"`

### Task 12: C3 H10 zcode 无头新建

**Files:**
- Create: `src-tauri/src/inject/headless/zcode_create.rs`
- Modify: `src-tauri/src/remote/api.rs`（`POST /m/api/v1/session-create-zcode`——任务态内存态复用 session-create spec §3 语义但仅 zcode 单工具）
- Modify: `src/mobile/NewSessionForm.tsx`（或既有新建入口——加 zcode 分组）
- Test: zcode_create.rs tests + mobile 用例

- [ ] **Step 1: 失败测试 + 实现**：候选列表纯核 = `recentProjects`（读 `~/.zcode/v2/setting.json` 只读，过滤存在性）∪ 看板快照项目；手填校验 = 路径合法 + 盘符存在 + **黑名单同源文件预览黑名单**（`remote/files.rs` 的 SENSITIVE 口径）+ 不存在递归创建；spawn = zcode argv 无 `--resume` + `--cwd <项目>` + 首句（默认 `hi`）→ 新 sess_id 回执 → 可见性提示（已信任=重启可见/未信任=仅 MAM——同 H7 判定）；同项目已有活跃 zcode 会话 → 黄字信号复用配对不确定门。移动端：新建表单工具选择器加 zcode 项（独立交付，UI 融合留 session-create Phase C）。
- [ ] **Step 2: 全门禁**；**Step 3: USER-ASSIST 实机**：手机无头新建一个 Test2 的 zcode 会话 → 回执 sess_id → 上板 → 重启 ZCode APP 后可见。
- [ ] **Step 4: Commit** `git commit -m "feat(zcode): H10 无头新建——recentProjects 候选/手填黑名单/首句注入/可见性分层提示"`

### Task 13: C4 H11 三家 CLI 无头（claude / kimi / opencode）

**Files:**
- Create: `src-tauri/src/inject/headless/cli_three.rs`（claude 主体 + kimi/opencode 薄适配）
- Modify: `src-tauri/src/inject/headless/mod.rs`（claude 审批双向桥）、`src/mobile/SessionDetail.tsx`（审批卡激活）
- Test: cli_three.rs tests（wire 纯核 mock stdin/stdout 帧）

- [ ] **Step 1: 写失败测试**（附录 E ①② 的 wire 规格直译）：

```rust
    #[test]
    fn claude_argv_full_set_e_permission_mode_always_present() {
        // 附录 E-①：恒带 --permission-mode（fail-closed：省略=bypassPermissions）
        let a = super::claude_argv("hi [mobile]", Some("sess_9"), None);
        for f in ["--print","--input-format","stream-json","--output-format","stream-json",
                  "--verbose","--include-partial-messages","--replay-user-messages",
                  "--permission-prompt-tool","stdio","--permission-mode"] {
            assert!(a.iter().any(|x| x.contains(f)), "缺 {f}");
        }
        assert!(a.iter().any(|x| x == "--resume")); // 与 --session-id 互斥（E-① 实机）
    }

    #[test]
    fn control_response_allow_must_echo_updated_input() {
        // 附录 E-②：allow 必带 updatedInput=原 input 回显；deny 不带；dismiss→deny
        let req = super::parse_control_request(r#"{"subtype":"can_use_tool","toolUseID":"t1","tool_name":"Bash","input":{"command":"ls"}}"#).unwrap();
        let allow = super::build_control_response(&req, super::Decision::Allow);
        assert!(allow.contains("updatedInput") && allow.contains(r#""command":"ls""#));
        let deny = super::build_control_response(&req, super::Decision::Deny);
        assert!(deny.contains(r#""behavior":"deny""#) && !deny.contains("updatedInput"));
    }

    #[test]
    fn ask_answers_keyed_by_question_text_multiselect_array() {
        // 附录 E-②/③：answers 键=题面文本；多选=数组；未答全 → 不允许提交（防静默丢题）
        let answers = super::build_ask_answers(&[super::Q::multi("选框架", vec!["a","b"], vec!["a"]), super::Q::single("确认?", vec!["y","n"], "y")]);
        assert!(answers.contains(r#""选框架":["a"]"#) && answers.contains(r#""确认?":"y""#));
    }
```

- [ ] **Step 2: 确认失败** → **Step 3: 实现**：claude = runner 长驻变体（进程存活至 turn 结束——裁决 8 特例）：stdout 双流解析（stream-json 事件流 + `control_request` 控制面分离）→ `control_request{can_use_tool}` 投影成移动端审批卡/问答卡（复用 H5 接口与 Task 10 类型）→ 用户选择 → stdin 写 `control_response`；turn 终点判据 = `stream_event{message_delta{stop_reason}}`（E-①，勿等 result 帧）；kimi = `kimi -p -S <id> "<text>"`；opencode = `opencode run <text>`（两家的会话续接参数属 B/C 级证据——**本任务实机首步**先各自无头跑一条探针定案续接形态，再落适配器）；回执解析各一（末条 assistant + token）。
- [ ] **Step 4: 全门禁**；**Step 5: USER-ASSIST 实机**：对一个无窗 claude 会话发消息 → 审批卡弹出 → 批准 → 工具执行 → 回执。
- [ ] **Step 6: Commit** `git commit -m "feat(cli-three): H11 claude/kimi/opencode 无头——argv 全集/审批双向 wire/问答 answers 映射（附录 E 规格）"`

### Task 14: 收尾——E2E 骨架 + 全门禁 + spec 进度回填

**Files:**
- Create: `src-tauri/tests/headless_e2e.rs`（跨平台 `#[ignore]` 实机套件——无头通道本就双平台，**不做 windows-only 编译门**，单机跑不了的用例按平台条件 skip 并登记）
- Modify: spec 附录 B（状态翻 🔄/✅）

- [ ] **Step 1: E2E 用例**（全 `#[ignore]`，实机跑）：`zcode_send_roundtrip`（发→回执→rollout 落盘核验）/ `codex_queue_consumed` / `wb_acp_prompt_landed` / `claude_approval_roundtrip`。空跑验证编译 + `cargo test -- --ignored --list` 列出。
- [ ] **Step 2: 全门禁总跑**（cargo test/clippy/fmt + pnpm test/build/format/lint）+ 既有 `#[ignore]` 套件不回归。
- [ ] **Step 3: spec 附录 B 回填 commit + push** `git commit -m "docs(spec): H1-H12 实施进度回填（本计划执行完毕）"`

### Task 15: 统一手工测试用例（用户执行——仅限 agent 无法操作的项）

> 时机：**Task 1–14 全部完成、代码 review 通过之后，用户一次性统一执行**（用户 2026-10-05 裁决）。凡 agent 能用电脑直接操作的（命令行验证、DB/文件核对、API 调用、进程检查）都已在前序任务的实机核验步覆盖——本清单**只收**手机操作、GUI 目视、APP 内交互、账号态四类。每用例带通过判据；发现不符记录现象回主线。

**前置准备（一次）**：MAM dev 以本分支最新代码重启；手机连同一局域网打开 `/m` 并 PIN 配对；ZCode / WorkBuddy / ChatGPT.app / DeepSeek Harness 保持安装可用。

| # | 功能点 | 手工步骤（只有你能做的部分） | 通过判据 |
|---|---|---|---|
| M1 | H3 总开关·关闭态 | 设置页确认「无头注入」默认关 → 手机打开任一 zcode 会话详情 | 发送入口置灰 + 提示「无头通道未开启，请在电脑端 MAM 设置中开启」 |
| M2 | H3 总开关·开启 | 电脑端开启开关（读安全说明并确认）→ 回手机刷新 | 入口恢复可用 |
| M3 | H7 zcode 发消息 | 手机对 Test2 的 zcode 会话发「hi [测试]」 | 回执卡出现：末条 assistant 摘要 + token + 耗时；下方灰字「重启 ZCode 应用后可见」 |
| M4 | H7 可见性兑现 | 重启 ZCode APP → 打开 Test2 工作区 | M3 的回合出现在会话里 |
| M5 | H4 取消 | 手机对任一会话发一条长任务（如「数到 100」）→ 回执进行中点「取消」 | 回执变「已取消（用户终止）」，进程消失（电脑任务管理器无残留 zcode.cjs） |
| M6 | H8 codex APP | 手机对 Test2 的 codex 会话发「ok? [测试]」→ 切到 ChatGPT.app 看 | APP 内 ~1 分钟出现该消息并开始回复 |
| M7 | H9 WB 前置 | （WB 未开远程控制时）手机对 WB 会话发消息 | 如实收到「WorkBuddy 远程控制端点未启用」提示（不谎报成功） |
| M8 | H9 WB 全链 | 在 WorkBuddy 设置里开启远程控制类开关（找到与否都告知主线）→ 手机对活跃 WB 会话发「hi」 | WB APP 内出现消息并执行；找不到开关 = 记录后跳过（风险 16 活账） |
| M9 | H10 zcode 新建 | 手机「+ 新建会话」→ 工具选 zcode → 项目选 Test2 → 首句默认 → 提交 | 回执出新 sess_id → 看板出现新卡 → 重启 ZCode APP 后 Test2 里可见 |
| M10 | H11 claude 审批 | 手机对一个无窗 claude 会话发「列出本目录文件」 | 手机弹出审批卡（Bash 工具 + 命令原文）→ 点批准 → 工具执行 → 回执含结果 |
| M11 | H11 问答卡 | 手机发一条会触发 claude AskUserQuestion 的消息（如「问我一个单选题」） | 问答卡出现：单选/多选/Other 自由文本可用；**不全答无法提交**；提交后 claude 收到答案 |
| M12 | H1+T1 dsh 正文 | 在 DeepSeek Harness 里随便一个项目发一条消息 | MAM 看板 dsh 卡正文/预览随之更新（不再「无消息」） |
| M13 | H12 WB 上板 | 在 WorkBuddy 里新建一个会话说一句 | MAM 看板出现该 WB 卡（无心跳场景） |
| M14 | L13 拒绝歧义 | 电脑开两个终端、同目录各起一个 claude → 手机对其中一张卡发消息 | 收到明确拒绝提示「同目录存在多个候选会话…」（而不是打进错误窗口） |
| M15 | 回执诚实性抽查 | 手机随便发 2–3 条到不同工具 | 每条要么明确成功（有消费证据）要么明确失败原因——**没有任何一条谎报成功** |

（L14 的 macOS 假成功修复属 Mac 侧验收——下次 Mac 有空时按 Mac 报告 ③-5 场景复测 Esc 生效即可，不阻塞本批。）

- [ ] **Step 1: 用户按表统一执行，逐条记录 PASS/FAIL/现象**
- [ ] **Step 2: 主线汇总结果回填 spec 附录 B + 修复 FAIL 项（若有）**
- [ ] **Step 3: 全部 PASS 后：主线征得用户同意再 push 分支与合流**

---

## 自审记录（Self-Review，含 2026-10-05 复审轮）

1. **Spec 覆盖**：H1（Task 1，诊断驱动——含对既有交付代码的 v4 修复）/ H2（✅ 已完成探测，无实现任务——结论供 Task 9/11 用）/ H3（5）/ H4+H6（6）/ H5（10+13）/ H7（8）/ H8（9）/ H9（11）/ H10（12）/ H11（13）/ H12（2）/ L13（3）/ L14（4）。C0 四件 = Task 1–4 ✓。**H13（dsh 写侧）不在本计划**（范围注记，审阅裁决）。**Task 15 覆盖全部 H 节的手工验收面**（M1–M15）。
2. **占位符扫描**：实现要点均给出核心代码或明确规格表；`db_snapshot_fresh`/`agent_tty` 等给签名+行为契约（内部逻辑为直白 IO，执行者按契约落码）；无 TBD。
3. **类型一致性**：`Receipt/Stage/HeadlessKind/Channel::Headless/PermissionSpec/Decision/Q` 在 Task 6/7/8/10/13 间交叉引用已对齐；`codex -C` 前置（Task 9 argv[1] 断言）与附录 E-④ 一致。
4. **实测对齐**：Task 1 的诊断三连源自「版本门结论被代码事实推翻」的更正；Task 8 争用锁/Task 9 分派/Task 11 复活语义均为两端探测定案直译。
5. **复审轮修订（2026-10-05，用户指令内审）**：① 执行纪律改为「本地分支 `feat/h1-h12-headless`、全任务完成且 Task 15 通过前不 push」；② 新增复用清单节（在产函数 8 项 + 探测定案真值 7 项，标注用于哪个任务）；③ gate.rs 伪码测试改真码（`--prompt` 在场断言 + `--version` 缺席断言）；④ Task 6 补「无头子区三件套收齐」步（超时/并发控件原漏排）；⑤ Task 11 MockHttp 定义为任务内注入缝；⑥ Task 13「C4 实机首任务」自指措辞改「本任务实机首步」；⑦ Task 14 E2E 去 windows-only 编译门（无头通道双平台）；⑧ 新增 Task 15（15 个手工用例，仅收手机/GUI 目视/APP 交互/账号态四类不可自动化项，统一于 review 通过后执行）。
