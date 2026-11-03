# 通知审查问题修复（oneshot 回收 / 最小权限 / 候选自动隐藏）Implementation Plan（小修复）

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 修复代码审查（2e4e55d..824edab）发现的 2 个 Important + 1 个 Minor：建房失败被吞导致降级失效、通知窗权限过大、窗口选择器无限驻留；顺带统一计划文件入库。

**约定：** 精简模式，不新增自动化测试，人工验证为主，每任务一 commit。

**环境：** Windows（Git Bash）；cargo 在 `src-tauri/` 下；TLS 报错时后台跑 `python C:\Users\bunny\AppData\Local\Temp\dsh-cargo-http-mirror.py 8765`。

---

### Task 1: 建房失败回收（修 Important 1 — 降级路径实质失效）

**Files:**
- Modify: `src-tauri/src/commands/notification.rs`（`show_notification_window`）

- [ ] **Step 1: 替换命令实现**

将 `show_notification_window` 整个函数（含建房注释块与 `std::thread::spawn`）替换为：

```rust
#[tauri::command]
pub async fn show_notification_window(
    app: AppHandle,
    payload: NotificationPayload,
) -> Result<(), String> {
    // 建房必须放独立线程（Windows 上在命令内直接创建 webview 会死锁，wry#583）；
    // 结果经 oneshot 回收向上传播：失败时前端 catch 到 Err、降级系统 toast，通知不丢。
    // await 发生在异步运行时线程，不阻塞主循环，不会复活死锁。
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let _ = tx.send(create_notification_window(&app, payload));
    });
    match rx.await {
        Ok(result) => result,
        Err(_) => Err("创建通知窗口线程异常".to_string()),
    }
}
```

（`create_notification_window` 及其内部的 300ms emit 脱离线程保持不变；tokio 已是全 feature 依赖，无需改 Cargo.toml。若建房线程 panic，tx 被 drop，`rx.await` 走 `Err(_)` 分支。）

- [ ] **Step 2: 编译与 Commit**

```bash
cd src-tauri && cargo clippy --all-targets -- -D warnings
git add src-tauri/src/commands/notification.rs
git commit -m "fix(notification): propagate window creation failure for toast fallback"
```

---

### Task 2: 通知窗最小权限（修 Important 2 — capability 通配过权）

**Files:**
- Create: `src-tauri/capabilities/notification.json`
- Modify: `src-tauri/capabilities/default.json:5`

- [ ] **Step 1: 新建最小权限 capability**

创建 `src-tauri/capabilities/notification.json`：

```json
{
  "$schema": "../gen/schemas/desktop-schema.json",
  "identifier": "notification",
  "description": "通知浮窗最小权限（仅显示/隐藏与事件监听）",
  "windows": ["notification-*"],
  "permissions": [
    "core:default",
    "core:window:allow-show",
    "core:window:allow-hide"
  ]
}
```

（`core:default` 已含 `core:event:default`（listen/unlisten）与窗口 getter；通知页只调用 `getCurrentWindow().show()/hide()`、`listen`、应用自有命令 `focus_session`/`focus_hwnd`（无 ACL 管控）——此前已核实。）

- [ ] **Step 2: 收回默认 capability 的通配**

`src-tauri/capabilities/default.json` 第 5 行：

```json
  "windows": ["main", "about", "settings", "notification-*"],
```

改为：

```json
  "windows": ["main", "about", "settings"],
```

- [ ] **Step 3: 编译与 Commit**

```bash
cd src-tauri && cargo clippy --all-targets -- -D warnings
git add src-tauri/capabilities/notification.json src-tauri/capabilities/default.json
git commit -m "fix(capabilities): least-privilege permissions for notification windows"
```

---

### Task 3: 窗口选择器自动隐藏（修 Minor 1 — 候选列表无限驻留）

**Files:**
- Modify: `src/pages/notification.tsx`

- [ ] **Step 1: `armTimer` 提升为组件级参数化函数**

将 `useEffect` 内的局部 `armTimer` 定义删除，在 `const timerRef = ...` 之后（组件级）新增：

```tsx
  // 自动隐藏计时器（组件级，参数化时长）：通知卡片与候选列表复用
  const armTimer = (ms: number) => {
    if (timerRef.current) window.clearTimeout(timerRef.current);
    timerRef.current = window.setTimeout(() => getCurrentWindow().hide(), ms);
  };
```

`useEffect` 的 listen 回调中 `armTimer();` 改为 `armTimer(6000);`。

- [ ] **Step 2: `jump()` 弹出候选时计时**

`jump()` 中 `setCandidates(result.windows); getCurrentWindow().show();` 之后追加一行：

```tsx
        armTimer(15000);
```

- [ ] **Step 3: 通知卡片 mouseLeave 统一 + 候选列表加悬停计时**

通知卡片 div 的 `onMouseLeave` 替换为：

```tsx
          onMouseLeave={() => armTimer(3000)}
```

候选列表 div（`{candidates && (` 的那层）加悬停属性，变为：

```tsx
      {candidates && (
        <div
          className="bg-card flex h-screen w-screen flex-col gap-1 rounded-lg border p-3 shadow-2xl"
          onMouseEnter={() => timerRef.current && window.clearTimeout(timerRef.current)}
          onMouseLeave={() => armTimer(5000)}
        >
```

（卡片 `onMouseEnter` 保持不变。新通知到达时现有的 `setCandidates(null)` 兜底维持。）

- [ ] **Step 4: 验证与 Commit**

```bash
cd "E:\LLMproject\Github\MultiAgents-Manager" && pnpm lint && pnpm build
git add src/pages/notification.tsx
git commit -m "fix(notification): auto-hide window picker with hover pause"
```

---

### Task 4: 计划文件统一入库（卫生项）

- [ ] **Step 1: 入库 untracked 计划文件**

```bash
cd "E:\LLMproject\Github\MultiAgents-Manager" && git status --short docs/engineering/plans/
git add docs/engineering/plans/
git commit -m "docs(plans): track implementation plan files"
```

（**禁止** `git add -A`——仓库内有 `.pnpm-store/` 等 untracked 杂项不得带入。若 `git status` 显示本计划文件之外还有其他 untracked 的 `.md`，一并入库。）

---

### Task 5: 门禁与人工验证

- [ ] **Step 1: 自动门禁**

```bash
cd src-tauri && cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --check
cd "E:\LLMproject\Github\MultiAgents-Manager" && pnpm check
```

- [ ] **Step 2: 人工验证（`pnpm tauri:dev`）**

1. **权限拆分回归**：触发一次通知（跑个会话任务或临时改状态），浮窗正常显示、6 秒隐藏、悬停保留——不得出现 permission denied（打开 devtools 控制台确认无权限报错）
2. **降级路径**：临时在 `create_notification_window` 函数首行插入 `return Err("test".into());` → 触发通知 → 应弹出**系统 toast**（而非静默）→ 还原插入并确认浮窗路径恢复
3. **候选自动隐藏**：WT 多窗口下点击通知卡（打分未命中时）→ 候选列表出现 → 不操作 15 秒自动隐藏；悬停不隐藏、移开 5 秒隐藏
4. 回归：卡片点击跳转、双通知堆叠正常

- [ ] **Step 3: 汇报**

各 Task 状态、门禁结果、人工验证逐项结果（不能实机验证的标注"待用户验证"）、`git log --oneline 824edab..HEAD`。

---

## 范围外（后续另立）

- 300ms 固定延迟改 ready-ack 机制（首条通知偶发丢失）
- claude hook 注册假阳性（DB `hooks_registered=true` 但 `settings.json` 无 hooks 段——spike 挖出的存量缺陷）
- `csp: null` 基础内容安全防护
