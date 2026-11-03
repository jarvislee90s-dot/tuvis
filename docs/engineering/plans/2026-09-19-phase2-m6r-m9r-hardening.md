# M6R–M9R 终端注入加固与增强 · 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 按批次设计 R1–R7 与 §8 参数定案，重写 Windows 注入引擎（四家 CLI、自适应节流、A1 分层确认、冻结自愈）、修复首批全部 P1/P2 与 P3 子集、补全审批取证（严格档）、交付一键 resume 窗口，收官含明确化的 E2E/计算机辅助/人工三段验收。

**Architecture:** 纯核先行（族规格/事件构造/确认戳为跨平台可测纯函数 + KeyLayout 缝注入 Windows FFI）；windows_console 重写为「全局互斥 + RAII 附加守卫 + 自适应节流 + 真总预算 + 部分写续写」；队列侧落裁决 19（停服冻结 + 启动对账 + 周期兜底）与裁决 A1（FlushOutcome 上抛 + 确认分层回执）；端点/前端/审批/resume 各自成任务。

**Tech Stack:** Rust（axum/rusqlite/tokio；新增 1 个依赖 `wait-timeout`；windows crate 增补两个 feature 位）+ React 19 mobile（中文内联）+ 桌面 i18n zh/en。

**Spec（执行前必读，裁决不得重议）：** 批次设计 `docs/engineering/specs/2026-09-18-phase2-m6r-m9r-injection-hardening-design.md`（R1–R7、§8 参数定案、裁决 18/19/A1/B1、灰色项三项保留）→ 二期总 spec v1.5.1 → 宪法（不动）。参数证据：`docs/research/refs/phase2-消息注入/`（M6R 报告 v2 §3、独立评审、opencode 规格）。

## Global Constraints（每任务隐含遵守）

1. 六门禁全绿才算任务完：`cargo fmt --check` / `cargo clippy --all-targets -- -D warnings` / `cargo test`（**合并 main@e6fda1c 后基线〔2026-09-19〕：lib 835/835 全绿 + 集成 preset_v2 34 过/5 败=360 环境基线名单恒定；lib 并行负载下个别用例偶发干扰、复跑即绿**）/ `pnpm check` / `pnpm test`（544/544）/ `pnpm build:mobile`。
2. 红线：不 push、不建 PR、不动 main、不改宪法与设计文档（发现冲突停下上台账）；裁决 1–19 + A1/B1 不重议。
3. 平台分层：cfg 只包执行层；纯核（族表/事件构造/节流判定/确认戳/映射）无 cfg、Windows 全绿可测。macOS 单测 `#[cfg(all(test, target_os = "macos"))]` 照旧。
4. 测试零接触真实 `~/.mam`（dao 内存库；端点测试假 session_source/假注入器）；**实机测试一律 `#[ignore]`**（`cargo test -- --ignored` 显式跑，Task 12）。
5. `store.with` 持锁不可重入（M4 教训）：锁内只 SQL；确认轮询/注入全在锁外。
6. 数据同源铁律：会话查找只经 `(st.session_source)()`。
7. 移动端文案中文内联；桌面新 UI 走 i18n zh/en 成对（check:i18n 卡关）。
8. 探测/取证涉及真实 CLI 会话时，用 `.agents/skills/win-console-inject-probe` 的脚本套件（`%USERPROFILE%\mam-probe-m6r\` 为基座目录），纪律按其八条铁律。

## 关键复用点（已存在，直接用）

- `inject/windows_console.rs`：`FlatKeyRecord`（20 字节平铺+编译期断言）、`attach/open_conin/write_chunk/is_insufficient_buffer/resolve_target` 保留；`write_all/inject_via/inject_text/inject_key/single_key_records` 本计划重写。
- `inject/queue.rs`：`FlushOutcome{Sent,Failed,Suspended,Deferred}`/`try_flush/settle/try_acquire_inflight/INFLIGHT/FLUSH_LOOP_HANDLE/spawn_flush_loop`。
- `remote/api.rs`：`session_send`（直发分支 756-775 一带）、`session_queue_jump`（守卫先取先例 968-979）、`session_queue_retract`（1014-1069，无守卫）、`session_approve*`、`endpoint_audit/device_identity/find_session_sync/MAX_SEND_CHARS`。
- `remote/mod.rs`：`stop_server_core`@511（SERVER_HANDLE abort@520 先例；`stop_server_with`@538 薄壳）；`window/tmux.rs`（list-panes 输出解析）、`window/applescript.rs::execute_applescript`、`window/win32.rs::collect_ancestor_pids`。
- `inject/approve.rs`：`DEFAULT_MAPPINGS_JSON/load_mappings_from/detect/is_version_drift/cached_cli_version/VERSION_CACHE`。
- 移动端：`MessageComposer.tsx`（队列轮询 effect 66-86）、`ApproveCard.tsx`、`api.ts`、`SessionDetail.tsx:939/950` 挂载点。

## 跨任务接口契约（先读这里）

```rust
// inject/families.rs（Task 1）——族规格与全部脆弱常量（宪法横切 6 的集中落点）
pub enum TuiFamily { RawVt, Crossterm }
pub struct FamilySpec { pub family: TuiFamily, pub verified_with: &'static str,
    pub slow_consumer: bool, pub confirm_timeout_ms: u64 }
pub const CHUNK_CHARS: usize = 80;  pub const CHUNK_GAP_MS: u64 = 50;
pub const SUBMIT_DELAY_MS: u64 = 150;  pub const LONG_MSG_CHARS: usize = 2000;
pub const DRAIN_TO: u32 = 40;  pub const OCC_ABNORMAL_MS: u64 = 5_000;
pub const BASE_BUDGET_MS: u64 = 10_000;  pub const BACKPRESSURE_MS_PER_CHAR: u64 = 45;
pub fn family_for(tool: &str) -> Option<FamilySpec>;
pub fn use_backpressure(spec: &FamilySpec, chars: usize) -> bool;
pub fn inject_budget_ms(spec: &FamilySpec, chars: usize) -> u64;
pub struct ChunkPlan { pub text_chunks: usize, pub backpressure: bool } // 纯构造：分块/背压计划（Task 3 执行层消费）
pub fn chunk_plan(text: &str, spec: &FamilySpec) -> ChunkPlan;

// inject/engine.rs 纯核（Task 2）——KeyLayout 缝：Windows 真 FFI、测试假实现
pub struct KeyRecordSpec { pub vk: u16, pub scan: u16, pub ch: u16, pub down: bool }
pub trait KeyLayout { fn vk_of(&self, c: char) -> u16; fn scan_of(&self, vk: u16) -> u16; }
pub fn text_records(text: &str, layout: &dyn KeyLayout) -> Vec<KeyRecordSpec>;      // 成对 down/up
pub fn enter_records(layout: &dyn KeyLayout) -> Vec<KeyRecordSpec>;                 // VK 形态（三家统一）
pub fn control_records(key: &str, layout: &dyn KeyLayout) -> Option<Vec<KeyRecordSpec>>; // enter/esc/tab
pub fn vt_seq_records(seq: &str) -> Vec<KeyRecordSpec>;   // A 族方向键：vk=0/scan=0 字符流（单批原子写）
pub fn vk_arrow_records(seq: &str, layout: &dyn KeyLayout) -> Option<Vec<KeyRecordSpec>>; // B 族方向键

// windows_console.rs（Task 3/4）——重写后的执行层
pub(crate) struct InjectStats { pub written: usize, pub backpressure: bool, pub unfroze: bool }
pub(crate) fn inject_text_spec(pid: u32, text: &str, spec: &FamilySpec) -> Result<InjectStats, String>;
pub(crate) fn inject_key_spec(pid: u32, key: &str, spec: &FamilySpec) -> Result<(), String>; // 域外 Err
pub(crate) fn read_input_tail(pid: u32, n: usize) -> Result<String, String>;  // CONOUT$ 屏读（A1 插队确认/滞留回查）

// inject/confirm.rs（Task 5）——A1 确认层
pub fn stamp_of(content: &str) -> &str;                 // 截尾 24 字符（先 trim_end——F8 尾空格修剪）
pub fn stamp_in_messages<T: AsRef<str>>(msgs: &[T], stamp: &str) -> bool;  // 纯核
pub fn session_stamp_hit(st: &RemoteState, tool: &str, sid: &str, stamp: &str) -> bool; // 复用会话消息读路径

// queue.rs（Task 6）——签名演进
pub fn flush_one(st: &RemoteState, session_id: &str, jump: bool) -> FlushOutcome; // Result→FlushOutcome（P1-4）
pub(crate) fn reconcile_once(state: &Arc<RemoteState>);  // 启动/周期对账（P2-5+灰2）
```

---

### Task 1: 族规格层 `inject/families.rs`（纯核）

**Files:** Create `src-tauri/src/inject/families.rs`；Modify `inject/mod.rs` 追加 `pub mod families;`

- [ ] **Step 1: 失败测试**（文件尾 `#[cfg(test)] mod tests`）

```rust
#[test] fn family_table_matches_probe() {
    let c = families::family_for("claude").unwrap();
    assert_eq!(c.family, TuiFamily::RawVt); assert!(!c.slow_consumer); assert_eq!(c.verified_with, "2.1.251");
    let o = families::family_for("opencode").unwrap(); assert!(o.slow_consumer); assert_eq!(o.verified_with, "1.18.31");
    let x = families::family_for("codex").unwrap();
    assert_eq!(x.family, TuiFamily::Crossterm); assert_eq!(x.verified_with, "0.154.0");
    assert!(families::family_for("kimi").is_some());
    assert!(families::family_for("workbuddy").is_none()); // 路由层已拦，此处纵深防御
}
#[test] fn backpressure_rules() { // R2-1：慢消费者或长文切背压
    let fast = family_for("claude").unwrap(); let slow = family_for("opencode").unwrap();
    assert!(!families::use_backpressure(&fast, 80)); assert!(families::use_backpressure(&fast, 2001));
    assert!(families::use_backpressure(&slow, 39)); assert!(!families::use_backpressure(&fast, 2000));
}
#[test] fn budget_scales_for_backpressure() { // 真总预算：背压按斜率放宽（opencode 实测 10k≈110-183s）
    let slow = family_for("opencode").unwrap();
    assert_eq!(families::inject_budget_ms(&slow, 80), 10_000 + 80 * 45);
    assert!(families::inject_budget_ms(&slow, 10_000) >= 183_000);
    assert_eq!(families::inject_budget_ms(&family_for("claude").unwrap(), 500), 10_000);
}
```

- [ ] **Step 2:** `cargo test families` 确认编译失败 → **Step 3: 实现**（模块头注释：M6R 定案 2026-09-18/19 + 版本指纹复验走项目技能；代码按上方契约全量落地：四个族的静态表 + 8 个常量 + 两个判定函数）→ **Step 4:** 过测 + fmt → **Step 5:** Commit `feat(m9r): 注入族规格层——A/B 族按家分支与全部脆弱常量集中（宪法横切6）`

### Task 2: 事件构造纯核重写 + 键域校验

**Files:** Modify `src-tauri/src/inject/engine.rs`（纯函数区：`KeyRecordSpec`（engine.rs:44）由旧版三字段 {vk,ch,down} **扩为四字段加 scan**，同步 `windows_console.rs` 的 `FlatKeyRecord::from`；旧 `text_to_key_records`/`key_to_windows_vk`/`single_key_records` 中被新构造完全取代者**同批删除**——防死代码）；Modify `src-tauri/Cargo.toml`（windows crate features 追加 `"Win32_UI_Input_KeyboardAndMouse"`, `"Win32_UI_WindowsAndMessaging"`——仅 feature 位，非新 crate）。

- [ ] **Step 1: 失败测试**（engine.rs tests，跨平台；用假布局 `struct FakeLayout;` 实现 `vk_of(c)=c as u16、scan_of(vk)=vk+1`）

```rust
#[test] fn text_records_pair_with_vk_scan() {
    let r = text_records("ab", &FakeLayout);
    assert_eq!(r.len(), 4);
    assert_eq!((r[0].vk, r[0].scan, r[0].ch, r[0].down), (b'a' as u16, b'a' as u16 + 1, b'a' as u16, true));
    assert!(!r[1].down); // 成对；keyup 由各家过滤/忽略（M6R F4 无双写）
}
#[test] fn enter_is_vk_form() { // 三家统一 VK 回车（B 族丢 vk=0 控制字符，M6R F3）
    let r = enter_records(&FakeLayout);
    assert_eq!(r.len(), 2); assert_eq!(r[0].vk, FakeLayout.vk_of('\r')); assert_eq!(r[0].ch, 0x0D); assert!(r[0].scan > 0);
}
#[test] fn control_keys_and_domain() {
    assert!(control_records("esc", &FakeLayout).is_some()); assert!(control_records("tab", &FakeLayout).is_some());
    assert!(control_records("我们", &FakeLayout).is_none());  // 域外 None——域校验（P2-2）
}
#[test] fn vt_seq_is_char_stream() { // A 族方向键：整条单批原子写的字符流
    let r = vt_seq_records("\x1b[A");
    assert!(r.iter().all(|k| k.vk == 0 && k.scan == 0));
    assert_eq!(r[0].ch, 0x1B as u16);
}
#[test] fn vk_arrow_for_crossterm() { // B 族方向键：VK+scan、char=0
    let r = vk_arrow_records("up", &FakeLayout).unwrap();
    assert_eq!(r[0].ch, 0); assert!(r[0].vk > 0 && r[0].scan > 0);
}
```

- [ ] **Step 2:** 确认失败 → **Step 3: 实现**（`KeyLayout` trait + 五个构造函数按契约；方向键表 `up/down/left/right → VK 0x25..0x28`；`control_records` 域=enter/esc/tab + 单字符 a-z0-9）→ **Step 4:** 过测（同步修 windows_console 编译：`FlatKeyRecord` 三字段对齐平铺布局并保住 20 字节断言）→ **Step 5:** Commit `feat(m9r): 事件构造按族规格重写——VK 回车/VT-VK 方向键分族/键域校验（§8.1）`

### Task 3: Windows 通道执行层重写（互斥 + RAII + 自适应节流 + 真总预算）

**Files:** Modify `src-tauri/src/inject/windows_console.rs`（重写 `write_all/inject_via/inject_text/inject_key`；新增 `CONSOLE_OP` 互斥、`AttachGuard`、`paced_write`、Windows 真 `KeyLayout`——`VkKeyScanW(c)&0xFF` / `MapVirtualKeyW(vk, MAPVK_VK_TO_VSC)`）；Modify `inject/engine.rs` 的 `RealInjector` windows impl 委托新签名。

- [ ] **Step 1: 失败测试**（节流计划纯函数 `chunk_plan` **归属 families.rs**、测试并入 Task 1 清单跨平台跑；此处仅域外键一项，`#[cfg(windows)]`）

```rust
#[test] fn chunk_plan_splits_text_and_enter() { // 正文块 160 事件 + 回车独立块（150ms 后单批）
    let spec = families::family_for("claude").unwrap();
    let plan = chunk_plan("[mobile test] hello", &spec); // 纯函数：返回 (正文块数, backpressure, enter 块)
    assert_eq!(plan.text_chunks, 1); assert!(!plan.backpressure);
    let spec_op = families::family_for("opencode").unwrap();
    assert!(chunk_plan(&"x".repeat(3000), &spec_op).backpressure);
}
#[cfg(windows)][test] fn unknown_key_rejected() { // P2-2：域外键报错，不回退文本+回车
    let err = inject_key_spec(424242, "bad!", &families::family_for("codex").unwrap()).unwrap_err();
    assert!(err.contains("不支持的按键"));
}
```

- [ ] **Step 2:** 确认失败 → **Step 3: 实现**，骨架（纪律注释写明依据）：

```rust
static CONSOLE_OP: once_cell::sync::Lazy<std::sync::Mutex<()>> = Lazy::new(|| Mutex::new(()));
struct AttachGuard; // P1-1：任何错误/panic 路径 Drop → FreeConsole 复位（CTRL_CLOSE_EVENT 防连带终止）
impl Drop for AttachGuard { fn drop(&mut self) { unsafe { let _ = FreeConsole(); } } }

fn inject_text_spec(pid: u32, text: &str, spec: &FamilySpec) -> Result<InjectStats, String> {
    let _lock = CONSOLE_OP.lock().unwrap_or_else(|e| e.into_inner()); // P1-2：进程级串行（附加态全局唯一）
    attach(pid).map_err(...)?; let _g = AttachGuard;
    let handle = unsafe { open_conin().map_err(|e| format!("打开 CONIN$ 失败：{e}"))? };
    let layout = WinKeyLayout;
    let r = (|| { // 闭包内不早退出 guard 作用域
        let chars = text.chars().count();
        let bp = families::use_backpressure(spec, chars);
        let deadline = Instant::now() + Duration::from_millis(families::inject_budget_ms(spec, chars));
        let mut stats = paced_write(handle, &text_records(text, &layout), bp, deadline)?; // 每轮检查 deadline（真总预算）
        std::thread::sleep(Duration::from_millis(SUBMIT_DELAY_MS));
        paced_write(handle, &enter_records(&layout), false, deadline)?; // 提交回车 150ms 后单批
        Ok(stats)
    })();
    unsafe { let _ = CloseHandle(handle); }
    r
}
```

`paced_write(handle, records, backpressure, deadline)`：按 160 事件/块写（`write_chunk` 保留）；**每轮循环顶部查 deadline**（超 → `Err("注入超时（总预算耗尽），请重试")`）；`ok=true 且 written<n` → 从偏移续写（M6R 纪律）；`ok=false` → 读错误码（**仅此时读**，F7），122 走 100ms 重试同块；背压模式每块后轮询 `GetNumberOfConsoleInputEvents ≤ DRAIN_TO`（15ms 间隔，同受 deadline）；`inject_key_spec` = `control_records`/`vk_arrow_records`（按族）→ 同链路单批。`resolve_target` 探测路径同样先取 `CONSOLE_OP` 锁（P1-1 修复面全覆盖）。
- [ ] **Step 4:** 全测 + clippy（FFI 部分编译过、真跑归 Task 12 的 `#[ignore]`）→ **Step 5:** Commit `feat(m9r): Windows 通道重写——进程级互斥+RAII 复位+自适应节流+真总预算+部分写续写（P1-1/P1-2/P2-1/P2-2 闭环）`

### Task 4: 占用率监控 + 冻结自愈 + 屏读层

**Files:** Modify `src-tauri/src/inject/windows_console.rs`（新增 `read_input_tail`（CONOUT$ + `ReadConsoleOutputCharacterW` 读输入行尾部）、`try_unfreeze`（`GetConsoleWindow()` 取 hwnd → `PostMessageW` WM_KEYDOWN/WM_KEYUP VK_ESCAPE → 等 drain ≤2s））；`paced_write` 内嵌冻结检测。

- [ ] **Step 1: 失败测试**（`#[cfg(windows)]` 单测用无人读控制台子进程；断言可跑）
  - `occ_stuck_triggers_unfreeze_probe`：向挂起控制台连写制造占用 → 调 `try_unfreeze` → 断言占用归零或返回带中文错误（二选一断言放宽，实机冻结态依赖选择模式，真值验证归 Task 12）。
  - `read_input_tail_returns_recent_chars`：conhost cmd 会话打入 `echo tail123` 前先 `read_input_tail(pid, 16)` 断言含刚注入文本（复用 Task 3 通道）。
- [ ] **Step 2:** 确认失败 → **Step 3: 实现**：`paced_write` 记录占用时间戳，`占用>0 且持续 ≥ OCC_ABNORMAL_MS` → 调 `try_unfreeze`（解冻成功 → 补一次 drain 等待后继续；失败 → `Err("目标终端输入冻结（选择模式），自动解冻失败——请点一下该终端窗口后重试")`）；`read_input_tail` 全程持 `CONSOLE_OP` + AttachGuard（与注入互斥）。WT 宿主恢复未验证 = 已知限制（设计 §8.3），错误文案覆盖。
- [ ] **Step 4:** 门禁 → **Step 5:** Commit `feat(m9r): 占用率监控+冻结自愈(PostMessage)+CONOUT$ 屏读层（M6R F1/§8.1）`

### Task 5: A1 写入确认层 `inject/confirm.rs`

**Files:** Create `src-tauri/src/inject/confirm.rs`；Modify `remote/api.rs`（把 session-messages 端点内部的「按 (agent, session) 读消息」聚合抽为 `pub(crate) fn read_session_messages_core(...)`，端点与确认层共用——数据同源）；Modify `inject/queue.rs`（flush_one 内嵌确认）；Modify `remote/server.rs` + `remote/mod.rs`（RemoteState 新增 `pub confirm_probe: Arc<dyn Fn(&str,&str,&str)->bool + Send + Sync>` 缝并在 STATE/测试态装配——生产=真实现，测试=恒 true，与 injector 缝同模式；**本任务定义并装配，Task 6 仅复用**）。

- [ ] **Step 1: 失败测试**

```rust
#[test] fn stamp_logic() { // 纯核：截尾 24 字符 + 先 trim_end（F8 尾空格修剪）
    let c = "[mobile iPhone] ".to_string() + &"a".repeat(40) + "  ";
    let s = stamp_of(&c);
    assert_eq!(s.chars().count(), 24); assert!(!s.ends_with(' '));
    assert_eq!(stamp_of("短消息"), "短消息");
}
#[test] fn stamp_found_in_user_messages() {
    let msgs = vec!["历史消息".to_string(), format!("reply just OK {}", stamp_of("[mobile t] body…"))];
    assert!(stamp_in_messages(&msgs, stamp_of("[mobile t] body…")));
    assert!(!stamp_in_messages(&msgs, "不存在戳"));
}
```

- [ ] **Step 2:** 确认失败 → **Step 3: 实现**：`session_stamp_hit` = `read_session_messages_core`（取最近 20 条，user 侧文本）内查戳（JSONL 家直接命中；opencode 走其 SQLite 读路径——同一核心函数天然覆盖，确认器"可插拔"由此实现，不另建 per-tool 确认器）。
- [ ] **Step 4: flush_one 内嵌 A1 分层**（`Sent` 之后、`settle` 之前，阻塞轮询——调用方均在 spawn_blocking）：
  - **直发/常规 flush**（jump=false）：以 `stamp_of(item.content)` 轮询 `session_stamp_hit`（500ms 间隔，超时=`spec.confirm_timeout_ms`）；未中 → 屏读 `read_input_tail` 见正文尾（滞留输入行）→ 补发一次 `enter_records` → 再轮询 3s；仍未中 → `FlushOutcome::Failed("已注入未确认（未见会话记录），请检查终端后重试")`（设计 R2-4：确认失败才回失败回执；重试由用户判断，终端可见已打内容）。
  - **插队**（jump=true，会话运行中）：写后等占用 drain ≤2s + `read_input_tail` 见草稿尾（best-effort，屏读失败则以"写入成功+排空"为准）→ `Sent`；排空超时 → `Failed("投递超时")`。
  - 超时值取 `families::family_for(&item.agent_type)`；无族（理论不可达）按快消费者默认。
- [ ] **Step 5:** 门禁（queue 既有测试按新断言更新：Sent 路径现在要求假确认——**给 queue 测试加确认缝**：`flush_one` 的确认调用经 `st.confirm_probe`（RemoteState 新增 `pub confirm_probe: Arc<dyn Fn(&str,&str,&str)->bool + Send + Sync>`，生产装配真实现、测试装 `|_,_,_| true`；与 injector 缝同模式，在 Task 6 一并接线 STATE））→ **Step 6:** Commit `feat(m9r): A1 分层写入确认——会话文件戳为主回执+屏读滞留回查补按回车（§8.1/E10 实证）`

### Task 6: 队列生命周期（裁决 19 + P1-4 + P2-5/6 + 灰2）

**Files:** Modify `src-tauri/src/inject/queue.rs`、`remote/mod.rs`（`stop_server_core`@511 在 SERVER_HANDLE abort@520 之后追加 `abort_flush_loop()`——裁决 19 冻结队列）、`remote/api.rs`（直发/插队端点回执映射；confirm_probe 缝已由 Task 5 装配，此处仅复用）。

- [ ] **Step 1: 失败测试**（queue 内存库 + 假注入器 + confirm_probe）
  - `stop_freezes_flush_loop`：spawn 循环 → 调 stop 路径的 abort 段（抽 `pub(crate) fn abort_flush_loop()`）→ 断言 `FLUSH_LOOP_HANDLE` 槽空。
  - `reconcile_flushes_pending_on_restart_like_state`：预置 pending（假会话 Waiting）→ `reconcile_once` → 断言已 sent + 审计 flush。
  - `periodic_sweep_gated_by_pending_count`：pending=0 时不触发任何 flush（假 session_source 计数为 0 断言）。
  - `flush_outcome_passthrough`：`flush_one` 返回 `Deferred`（黄态）与 `Suspended`（会话消失）——端点映射测试在 api 层。
- [ ] **Step 2:** 确认失败 → **Step 3: 实现**：
  1. `stop_server_core` 内 SERVER_HANDLE abort 之后追加：`crate::inject::queue::abort_flush_loop();`（注释：裁决 19 冻结队列；abort 硬停的「已注入未落账」窗口由启动对账兜底）。
  2. `spawn_flush_loop` 重构为 `select!` 双臂：事件臂（保留既有 burst 去重/守卫/JoinError warn）+ `tokio::time::interval(60s)` 臂（**灰2 周期兜底**：先 `store.with` 纯 SQL `SELECT COUNT(DISTINCT session_id) ... pending`，0 直接跳过——守宪法扫描预算）；订阅建立后**立即 `reconcile_once`**（P2-5 主场景：重启补投）。
  3. `flush_one`/`flush_given` 签名 `Result<(),String>` → `FlushOutcome`（P1-4）；`session_send` 直发分支与 `session_queue_jump` 改精确映射：`Sent→delivered`、`Deferred|Suspended→{"status":"queued",itemId,position}`（回查 pending 取 position；Suspended 亦 queued——行保持 pending 等会话回来，语义即排队）、`Failed(e)→failed{error}`。**删除** api.rs 756-775 的 `Ok(())→delivered` 旧映射与注释。
  4. `session_queue_retract` 开头加守卫（照抄 jump 先例）：`let Some(_guard) = try_acquire_inflight(&sid) else { return failed("投递进行中，请稍后重试"); }`（P2-6）。
- [ ] **Step 4:** 全量门禁（server 端点测试按新回执语义更新断言）→ **Step 5:** Commit `feat(m9r): 队列生命周期闭环——停服冻结(裁决19)+启动对账+周期兜底(灰2)+FlushOutcome上抛(P1-4)+撤回守卫(P2-6)`

### Task 7: 版本探测超时/垫片（灰1）+ P3 后端清理

**Files:** Modify `src-tauri/Cargo.toml`（`wait-timeout = "0.2"`——**本计划唯一新依赖**）、`inject/approve.rs`、`database/schema.rs`、`database/dao/inject_queue.rs`、`inject/queue.rs`、`inject/mod.rs`、`remote/api.rs`、`window/tmux.rs` + `inject/engine.rs`（复用提取）。

- [ ] **Step 1: 失败测试**
  - `probe_version_times_out_and_caches_none`：`cached_cli_version("mam-fake-hang-cli")`——配合一个 PATH 里的假挂死脚本（测试内写 `%TEMP%\mam-fake-hang-cli.cmd`：`@ping -n 30 127.0.0.1 >nul`，进程环境 PATH 前插该目录）→ 断言 ≤5s 返回 None 且二次调用不再 spawn（用锁内时间断言）。
  - `probe_version_via_cmd_shim`：`mam-fake-fast-cli.cmd`（`@echo mam-fake-fast-cli 9.9.9`）→ 断言解析出 `9.9.9`（灰1 垫片路径实证）。
  - `audit_action_vocab`（api 测试）：自定义 KV 映射含 `{"id":"other",...}` 选项 → 审计 action 记 `"key"` 且 log warn（非 approve/reject 不进词表）。
- [ ] **Step 2:** 确认失败 → **Step 3: 实现**：
  1. `probe_cli_version`：`Command::new(cli).arg("--version").stdout(piped).spawn()` 失败 → 回退 `Command::new("cmd").args(["/c", cli, "--version"])`（npm `.cmd` 垫片，灰1）；`wait_timeout(3s)` → `None` 则 `kill()+wait()` 并缓存 None（超时入缓存防重试刷屏）。
  2. P3 清单逐项：schema.rs 删 `jumped` 列（表未随任何发布版出库，直接改定义免迁移）+ dao 注释对齐；`queue.rs` INFLIGHT 两处与 `inject/mod.rs` 审计查询的 `.lock().unwrap()` → `.unwrap_or_else(|e| e.into_inner())`（锁自愈统一）；`session_approve` 审计动作 `match option.id { "approve"=>"approve","reject"=>"reject",other=>{warn; "key"} }`；`window/tmux.rs` 抽 `pub(crate) fn list_panes_lines() -> Option<Vec<String>>`，`engine.rs::find_tmux_pane` 与聚焦侧共用（消除双份格式解析）；陈旧注释清理（approve.rs 模块头「一律 probe-pending」、server.rs 测试注释）。
- [ ] **Step 4:** 全量门禁 → **Step 5:** Commit `fix(m9r): 版本探测超时+cmd垫片解析(灰1)；P3 后端清理——死列/锁自愈/审计词表/tmux解析复用/注释对齐`

### Task 8: 前端对齐（P2-7/10 + 灰3 + 严格档文案 + P3 前端）

**Files:** Modify `src/mobile/MessageComposer.tsx`、`src/mobile/ApproveCard.tsx`、`src/mobile/api.ts`、`src/mobile/SessionDetail.tsx`、`src/components/settings/AuditLogSection.tsx`（失败态测试）、`tests/mobile/*`、`tests/settings/AuditLogSection.test.tsx`。

- [ ] **Step 1: 失败测试**（vitest，要点断言）
  - `jump_busy_restores_queued_view`：jump 返回 failed(投递进行中) → 组件 fetchQueue 复核 → itemId 仍在 pending → chip 回「排队中 第N位」+ 按钮保留（P2-7）；真不在队 → failed 终态。
  - `retract_failure_same_reconcile`：retract 网络错 → 同上复核恢复。
  - `maxlength_guard`：textarea `maxLength=10000`；超限输入被截断。
  - `delivering_chip_during_await`（灰3）：点发送进入 await 期间显示「投递中…」chip（慢消费者长文 2-3 分钟不空白）。
  - `probe_pending_options_show_hint_only`：approve-options 返回 `available:false, reason:"键位待实测确认，请用普通发送"` → 卡片只渲染提示条不渲染按键（严格档 UI）。
  - `send_403_reason_chip`：session-send 403 not_injectable → 失败 chip 含 reason 原文（P2-10 补锁）。
  - `approve_404_no_session_copy`（P2-10 补锁）；`audit_load_error_state`（P3 补锁）。
- [ ] **Step 2:** 确认失败 → **Step 3: 实现**：MessageComposer 的 `handleJump/handleRetract` 失败分支先 `fetchQueue` 复核（itemId 在队→`setReceipt({kind:"queued",...})` 刷新 position；不在→failed）；`sending` 态渲染「投递中…」chip；`<textarea maxLength={10000}>`；`api.ts`：`SendInfo.channels/visibility` 改 optional（NotInjectable 分支不返回）；`ApproveCard`：options 载荷新增 `reason` 字段消费；`SessionDetail` 两组件挂载加 `key={session.id}`。
- [ ] **Step 4:** `pnpm test` + `pnpm check` → **Step 5:** Commit `fix(m9r): 移动端对账恢复(P2-7)+投递中chip(灰3)+严格档文案+10000上限+三条补锁测试(P2-10)`

### Task 9: macOS 匹配修复（P2-3/P2-4，构造层 Windows 可测）

**Files:** Modify `src-tauri/src/inject/engine.rs`（iTerm2 两脚本、Terminal 两脚本、**tmux 匹配拆纯函数 `parse_panes_find(lines:&[String], tty:&str)->Option<String>` 置跨平台区——`find_tmux_pane` 本体是 cfg(macos) 执行层薄壳**）、`src-tauri/src/window/terminal_app.rs`（聚焦侧 contains→is）。

- [ ] **Step 1: 失败测试**（跨平台构造断言）
  - `iterm_scripts_use_exact_tty`：`iterm_write_script/iterm_send_key_script` 产出含 `tty of s is "/dev/ttys005"`（不再 contains）。
  - `tmux_pane_match_is_full_path`：`parse_panes_find`（纯函数，跨平台）比较改全路径相等——构造 list 输出含 `/dev/ttys100` 与 `/dev/ttys1000`，喂 `/dev/ttys100` 只命中前者（回归锁前缀撞号）。
  - `terminal_scripts_traverse_tabs`：两脚本含 `repeat with t in tabs of w` + `tty of t is "/dev/..."`；单键脚本先 `set selected tab of w to t` 再 keystroke（先选后发）。
- [ ] **Step 2:** 确认失败 → **Step 3:** 实现（`terminal_do_script` 命中后 `do script ... in t`；`terminal_send_key_script` 命中后选 tab → activate → keystroke；`terminal_app.rs:26` contains 同步改 is）→ **Step 4:** 门禁 → **Step 5:** Commit `fix(m9r): macOS 终端定位精确匹配+Terminal.app 标签遍历（P2-3/P2-4；实机验证入回传清单）`

### Task 10: R4 审批取证回填 + 严格档后端（实机取证步骤内嵌）

**Files:** Modify `src-tauri/src/inject/approve.rs`、`remote/api.rs`（approve-options 严格档）、`tests`（server 端点测试）；取证材料归 `%USERPROFILE%\mam-probe-m6r\evidence\M8R-*`。

- [ ] **Step 1: 实机取证（本机 Windows，用探测技能脚本套件）**：
  1. codex（手册 C1 实验1）：conhost 起 codex → 注入 `/permissions`+回车 → 注入 ↓+Enter 选 **Read Only** → 注入 `create hello.txt with content hi` + 回车 → `probe-shot-win.ps1` 截图审批框 → 抄录标题/三选项/footer（预期对照源码快照：`Would you like to make the following edits?` / `1. Yes, proceed (y)` / `3. No, and tell Codex what to do differently (esc)`）。
  2. 键位验证：同场景再触发两次，分别注入 `y`（批准生效：文件落盘）与 `Esc`（拒绝生效：未落盘+对话内有拒绝记录）。
  3. claude（手册 C2）：plan 模式（注入 VT 序列 `\x1b[Z` 即 shift+tab 字符流——claude 为 RAW_VT 族）→ 触发改文件计划 → 截图批准交互与选项文案 → 记录数字直选键位。
  4. `codex --version`/`claude --version` 记录取证版本；全部截图+会话文件归档 evidence。
- [ ] **Step 2: 失败测试**（先写断言再回填）
  - `codex_markers_are_source_phrases`：默认表 codex markers = `["would you like to run the following","would you like to make the following","do you want to approve network"]`；`verified_with = "<Step1 实测版本>"`；detect 命中 Step1 抄录原文。
  - `claude_plan_marker_added`：claude markers 含 Step3 实测标题短语。
  - `probe_pending_strict_policy`（server 测试）：构造 KV 映射 `verified_with:"probe-pending"` → approve-options 返回 `available:false` + `reason:"键位待实测确认，请用普通发送"`；session-approve 对其 404 no_mapping（严格档：未取证=映射缺失）。
- [ ] **Step 3:** 实现：`DEFAULT_MAPPINGS_JSON` 按 Step2 回填（codex approve=`y` reject=`esc`——源码键位矩阵 B4 实证）；api.rs approve-options 判 `verified_with=="probe-pending"` → available=false + reason（与 Task 8 UI 契约对齐）。
- [ ] **Step 4:** 门禁 → **Step 5:** Commit `feat(m8r): 审批取证回填（codex Read Only 档实测+claude plan 模式）+严格档后端（未取证不出键）`

### Task 11: R5 一键 resume 窗口（双端）

**Files:** Create `src-tauri/src/inject/resume.rs`；Modify `inject/mod.rs`、`remote/api.rs`+`server.rs`（POST `/m/api/v1/session-open`）、`remote/mod.rs`、`lib.rs`（Tauri command `session_open`）、`src/mobile/SessionDetail.tsx`+`api.ts`、桌面会话卡按钮 + i18n zh/en、`src/tauri-mock.ts`、测试。

- [ ] **Step 1: resume 命令映射探测（小项）**：`claude --help`（已知 `--resume <id>`）、`codex --help | grep resume`（`codex resume <id>`? 以实测为准）、`kimi --help`、`opencode --help`——查证通过的入表，未查证/不存在的记 None（按钮禁用+原因「该工具 resume 命令待查证」）。结果写入 `resume.rs` 静态表注释（含版本）。
- [ ] **Step 2: 失败测试**
  - `resume_command_table`：claude → `claude --resume abc`；codex 按 Step1 实测断言；未知工具 None。
  - `build_spawn_command_windows`：有 wt → `wt -d <cwd> cmd /k <resume>`；无 wt → conhost+CREATE_NEW_CONSOLE（纯构造断言，不真 spawn）。
  - `session_open_endpoint`（server 测试，spawn 缝注入假 spawner）：POST session-open → 200 `{"status":"opening"}` + 审计 action=`"open"`；无 cwd → 404 `{"error":"no_cwd"}`；无映射 → 404 `{"error":"no_resume_command"}`；PIN gate 403 照旧覆盖。
- [ ] **Step 3: 实现**：`resume.rs` = 命令表 + `open_session_terminal(session) -> Result<(),String>`（核心接受 spawner 缝便于测试；Windows：`where wt` 探测；macOS cfg：iTerm2 `create window with default profile command "cd '<cwd>' && <resume>"` 优先、Terminal `do script` 次选——复用 execute_applescript；完成后置前聚焦）；端点 + Tauri 命令同核心；审计动作词表加 `open`（W5 注释同步）；移动端详情页「在电脑上打开」按钮（中文内联）+ `api.ts::sessionOpen`；桌面会话卡同款（i18n zh/en 各 +5 键：`resume.open/resume.noCwd/resume.noCmd/resume.opening/resume.failed`）；tauri-mock +1 case。
- [ ] **Step 4:** 门禁（i18n 成对卡关）→ **Step 5:** Commit `feat(w13): 一键 resume 窗口——双端触发+本机 spawn 终端+聚焦+审计（B 兜底可见性半部）`

### Task 12: E2E 集成测试（`#[ignore]` 实机显式跑）

**Files:** Create `src-tauri/tests/m9r_e2e.rs`；复用探测技能脚本起会话（`launch-session.ps1` 拓扑）。

- [ ] **Step 1: 编写四个 E2E（均 `#[ignore]`，`cargo test --test m9r_e2e -- --ignored` 显式跑；文档注明前置：本机已装四家 CLI + 探测目录就绪）**
  1. `e2e_engine_matrix_short`：对 claude/codex/kimi/opencode 各起 conhost 会话 → `inject_text_spec` 200 字符 → 确认层 `session_stamp_hit` 命中 → taskkill。断言：全量 stamp 命中 + `InjectStats.backpressure` 仅 opencode false-family 快送时为 false（200 字符 claude 不背压、opencode 慢消费者背压）。
  2. `e2e_engine_matrix_long`：claude 10000（固定节流，断言耗时 <15s）+ opencode 10000（背压，断言命中且允许 ≤460s 预算）。**本机通过后把两行耗时数字写进测试注释留档。**
  3. `e2e_http_full_chain`：conhost cmd 会话（复用 ffi_hop 拓扑）+ 内存库测试服务器（RealInjector + 真 confirm_probe）+ 假 session_source 指向该 cmd → POST `/m/api/v1/session-send`（注入 `echo <stamp> >> hop.log`）→ 断言 `delivered` + hop.log 行命中 + 审计 send/flush 两行落库——**HTTP→路由→队列→引擎→确认→审计全链闭环**（补齐首批登记的"组合缝"增强）。
  4. `e2e_key_domain_and_enter`：codex 会话注入短文本+回车（VK 形态）断言提交生效（会话文件命中）；`inject_key_spec(pid,"bad!",..)` 断言域外拒绝。
- [ ] **Step 2:** 本机实跑四例全绿（记录耗时与证据路径进测试文件头注释）→ **Step 3:** Commit `test(m9r): E2E 四例实机——引擎矩阵/长文预算/HTTP全链闭环/键域（#[ignore] 显式跑）`

### Task 13: 收官——门禁 + 验收测试清单 + Mac 回传 + handover

**Files:** Create `docs/release-notes/m6r-m9r-acceptance-checklist.md`；Modify `docs/release-notes/m7-m8-macos-live-checklist.md`（追加）；台账/handover 更新。

- [ ] **Step 1: 全六门禁终跑**（数字记台账；cargo 基线 5 败名单核对恒定）。
- [ ] **Step 2: 写验收测试清单**（`m6r-m9r-acceptance-checklist.md`，结构四段，逐条对应设计 R1–R7 与需求点）：
  - **A. 自动化已覆盖**（免人工）：单测/集成清单（族表、事件构造、节流判定、确认戳、队列生命周期、端点回执四态、严格档、resume 构造、macOS 脚本构造）+ E2E 四例（Task 12，附实跑命令与前置）。
  - **B. computer-use 可辅助项**（agent 可代看）：①四家终端注入后截图核验 `[mobile 设备名]` 行在框（WT+conhost 各一）；②resume 点击后窗口打开且前台聚焦核验；③移动端页面（浏览器过 PIN）发送/排队/插队/撤回/审批卡走查截图；④审计设置页逐条截图。
  - **C. 人工主场景清单**（用户真机，主力场景优先）：手机→四家会话各发一条短消息（送达回执+桌面终端可见）；长消息 2000 字（claude 即时 / opencode「投递中」→ 命中）；黄排队→转闲自动 flush；立即发送（claude 运行中入草稿）；撤回；**关闭远程→队列冻结→重开续跑**；MAM 重启→待发自动补投；审批：codex（Read Only 档）批准/拒绝、claude 计划模式批准执行；resume 双端各一次（≥2 家工具）；审计页逐条可查。
  - **D. Mac 回传清单追加**：三通道长消息+确认机制；相等匹配（ttys 前缀相近双会话互不串扰）；Terminal.app 后台标签页注入；审批 codex/claude 复验；resume（iTerm2/Terminal）。
- [ ] **Step 3: handover**（台账目录）：任务×提交×门禁总表、偏离与自裁决、E2E 证据路径、已知限制（WT 冻结恢复未验证/单实例假设/慢消费者耗时/DefTerm 不可达）。**不 push。**
- [ ] **Step 4:** Commit `docs(m6r-m9r): 收官——验收测试清单（自动化/computer-use/人工三段）+Mac 回传追加+handover`

---

## 自审记录（计划侧）

- **Spec 覆盖矩阵**：R1 探测=已完成（批外）；R2 → Task 1/2/3/4/5 + 12（引擎规格九条硬性要求逐一落：自适应节流 T1/T3、按家分支 T2、生命周期 T3、确认双层 T4/T5、真总预算 T3、键域 T2、保留件 T3 不动件、监控解冻 T4、常量可配+指纹 T1+T10）；R3 → Task 6/7/8/9（P1-3/A2、P1-4、P2-5/6/7/8/10、P2-3/4、P3 子集）+ 灰1(T7)/灰2(T6)/灰3(T8)；R4 → Task 10；R5 → Task 11；R6 → Task 9；R7 → Task 7/8；B1 四家矩阵 → T1 族表/T12 E2E；A1 分层 → T5/T6。**无缺口。**
- **占位符**：无 TBD；resume 命令表以「Step 1 实测定案 + 断言按实测写」闭环（探测即任务，不留悬空）；codex 映射键位同理（源码已证 y/esc，实测确认）。
- **类型一致性**：`KeyRecordSpec` 三字段贯穿 T2→T3；`FlushOutcome` 上抛贯穿 T5→T6→api；`confirm_probe` 缝 T5 定义 T6 装配；`InjectStats` T3 定义 T12 断言。
- **裁决不重议**：18/19/A1/B1 全部落为实现条款；宪法 D19 措辞级修订已按用户在席批准落档（本计划约束第 2 条"不改宪法"以 D19 已完成的措辞修订为界，后续执行不再触碰）。
- **复核修订（2026-09-19，标识符与代码库逐一核对后）**：核对命中——`MAX_SEND_CHARS=10_000`（api.rs:530）、`endpoint_audit/device_identity/find_session_sync`（api.rs:576/587/614）、`stop_server_core@511/abort@520/stop_server_with@538`、`FlatKeyRecord/attach/open_conin/write_chunk/is_insufficient_buffer/single_key_records/resolve_target`（windows_console.rs）、`KeyRecordSpec{vk,ch,down}@engine.rs:44`/`key_to_windows_vk@22`/`text_to_key_records@53`、`DEFAULT_MAPPINGS_JSON/load_mappings_from/cached_cli_version/VERSION_CACHE/probe_cli_version`（approve.rs）、`handleJump/handleRetract`（MessageComposer.tsx:118/135）、`FlushOutcome/try_acquire_inflight/INFLIGHT/FLUSH_LOOP_HANDLE/flush_one/flush_given/try_flush/settle`（queue.rs）——全部存在且签名相符；`FlushOutcome` 现为 `pub(crate)`，同 crate 内 api.rs 可见，无需升 pub。据此修正五处：①`chunk_plan` 归属 families.rs（原落 Windows 层导致非 Windows 测试编不过）；②Task 2 字段措辞（旧版三字段非二字段）+ 被取代旧函数删除注记（防死代码）；③Task 5 空字符字面量笔误；④confirm_probe 缝接线从 Task 6 归位 Task 5（消除跨任务接缝含糊）；⑤Task 9 tmux 匹配拆跨平台纯函数（`find_tmux_pane` 是 cfg(macos) 执行层，纯核测试原打不到）。**多路线未落定扫描**：无——abort vs CancellationToken（设计已定 abort+对账兜底）、确认主次（裁决 A1）、互斥 vs actor（设计已定全局互斥）均已定案；resume 命令与 codex 键位属"探测即定案"闭环（Step 1 实测→断言按实测写），非多路线并存。
