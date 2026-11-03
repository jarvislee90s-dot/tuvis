# 通知链路缺陷修复 + marker spike 补录 Implementation Plan（小修复）

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 修复代码审查（e1ed533..2e4e55d）发现的 Important 1-5：通知链路三处缺陷、hook marker spike 结论补录（含按结果实施）、通知浮窗状态标签 i18n。顺带两个一行修（通知跳转传原始 agentType、SessionCard 前缀兜底 8 位）。

**约定：** 延续精简模式——不新增自动化测试，人工验证为主，每任务一 commit。

**环境：** Windows（Git Bash）；cargo 在 `src-tauri/` 下；TLS 报错时后台跑 `python C:\Users\bunny\AppData\Local\Temp\dsh-cargo-http-mirror.py 8765`。

---

### Task 1: Rust 侧 — 槽位时间戳占用 + payload 字段调整

**Files:**
- Modify: `src-tauri/src/commands/notification.rs`

- [ ] **Step 1: 槽位占用改为时间戳判定（修审查 Important 2：is_visible 竞态丢通知）**

在 `const MARGIN` 之后追加：

```rust
/// 槽位占用表：值 = 占用时刻。
/// 不用 is_visible() 判忙：窗口要等事件送达 + 页面 show() 之后才可见（约 300ms+），
/// 同批第二条通知会误判空闲而覆盖第一条。时间戳占用 10 秒自动过期（通知只活 6 秒）。
static SLOT_OCCUPANCY: once_cell::sync::Lazy<std::sync::Mutex<std::collections::HashMap<usize, std::time::Instant>>> =
    once_cell::sync::Lazy::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

const SLOT_TTL: std::time::Duration = std::time::Duration::from_secs(10);
```

将 `show_notification_window` 开头的槽位选择段（`let mut slot = 0; for i in 0..SLOTS {...}` 整段）替换为：

```rust
    // 槽位选择：空闲优先；全忙则顶替最早占用的（时间戳判定，规避可见性竞态）
    let now = std::time::Instant::now();
    let slot = {
        let mut occupancy = SLOT_OCCUPANCY.lock().unwrap();
        occupancy.retain(|_, at| now.duration_since(*at) < SLOT_TTL);
        let s = (0..SLOTS)
            .find(|i| !occupancy.contains_key(i))
            .unwrap_or_else(|| {
                occupancy
                    .iter()
                    .min_by_key(|(_, at)| **at)
                    .map(|(i, _)| *i)
                    .unwrap_or(0)
            });
        occupancy.insert(s, now);
        s
    };
```

- [ ] **Step 2: payload 字段调整（为 Important 1/5 与打分修正服务）**

`NotificationPayload` 结构体替换为（`status_label` 删除，新增原始 `status` 与显示名 `agent_label`）：

```rust
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NotificationPayload {
    pub agent_type: String,  // 原始类型（claude/codex/...），跳转标题打分用
    pub agent_label: String, // 显示名（Claude Code 等专有名词，不翻译）
    pub project_name: String,
    pub status_color: String,
    pub status: String, // 原始状态枚举，由通知页 i18n 翻译
    pub last_message: String,
    pub pid: u32,
    pub session_id: String,
}
```

- [ ] **Step 3: 编译与 Commit**

```bash
cd src-tauri && cargo clippy --all-targets -- -D warnings
git add src-tauri/src/commands/notification.rs
git commit -m "fix(notification): timestamp-based slot occupancy and raw status payload"
```

（前端此刻尚未适配新字段，编译仅 Rust 侧；Task 2 完成前不要运行应用做通知验证。）

---

### Task 2: 前端 — 失败降级、歧义处理、i18n 状态标签

**Files:**
- Modify: `src/hooks/useNotification.ts`
- Modify: `src/pages/notification.tsx`
- Modify: `src/i18n/locales/zh.json`、`en.json`

- [ ] **Step 1: `useNotification.ts` 浮窗失败降级 + payload 构造（修 Important 3 与 Minor 1）**

浮窗路径（当前 `} else {` 分支内的 `await invoke(...)` 整块）替换为：

```ts
          } else {
            // 应用内浮窗路径（无需系统权限，不夺焦点）
            try {
              await invoke("show_notification_window", {
                payload: {
                  agentType: session.agentType,
                  agentLabel: toolLabel,
                  projectName: session.projectName,
                  statusColor: statusToColor(session.status),
                  status: session.status,
                  lastMessage: session.lastMessage ?? "",
                  pid: session.pid,
                  sessionId: session.id,
                },
              });
            } catch (e) {
              // 浮窗失败降级系统 toast，保证通知不丢（spec 008 错误处理要求）
              console.error("show_notification_window failed:", e);
              if (permissionGranted.current) {
                sendNotification({
                  title: `${toolLabel}${formTag} — ${session.projectName}`,
                  body: `${statusLabel}${session.lastMessage ? ": " + session.lastMessage.slice(0, 80) : ""}`,
                  actionTypeId: "focus-session",
                  extra: {
                    pid: session.pid,
                    sessionId: session.id,
                    agentType: session.agentType,
                    projectName: session.projectName,
                  },
                });
              }
            }
          }
```

（`STATUS_LABELS` 保留——仅系统 toast 路径使用；浮窗不再使用它。）

系统通知主路径的 `sendNotification`（`if (useSystemToast)` 分支）的 `extra` 同步补两个字段：`agentType: session.agentType, projectName: session.projectName`。

`onAction` 处理器（当前只传 pid）替换为：

```ts
        await onAction(async (notification) => {
          if (notification.actionTypeId !== "focus-session") return;
          const pid = (notification.extra?.pid as number) ?? 0;
          if (pid > 0) {
            try {
              await invoke("focus_session", {
                pid,
                sessionId: (notification.extra?.sessionId as string) ?? undefined,
                agentType: (notification.extra?.agentType as string) ?? undefined,
                projectName: (notification.extra?.projectName as string) ?? undefined,
              });
            } catch (e) {
              console.error("focus_session failed:", e);
            }
          }
        });
```

- [ ] **Step 2: `notification.tsx` 歧义候选内联 + i18n（修 Important 1 与 5）**

import 区追加 `import { useTranslation } from "react-i18next";`。interface 替换为：

```tsx
interface NotificationPayload {
  agentType: string;
  agentLabel: string;
  projectName: string;
  statusColor: string;
  status: string;
  lastMessage: string;
  pid: number;
  sessionId: string;
}

interface WindowCandidate {
  hwnd: number;
  title: string;
  process: string;
}
```

组件内（`const [payload, ...]` 之后）追加状态与 t：`const [candidates, setCandidates] = useState<WindowCandidate[] | null>(null);`、`const { t } = useTranslation();`

`jump()` 替换为：

```tsx
  const jump = async () => {
    getCurrentWindow().hide();
    try {
      const result = await invoke<{ type: string; windows?: WindowCandidate[] }>(
        "focus_session",
        {
          pid: payload.pid,
          sessionId: payload.sessionId,
          agentType: payload.agentType,
          projectName: payload.projectName,
        },
      );
      // 多窗口歧义：在通知窗内联渲染候选，避免静默失败
      if (result.type === "ambiguous" && result.windows && result.windows.length > 0) {
        setCandidates(result.windows);
        getCurrentWindow().show();
      }
    } catch {
      // 跳转失败不弹新提示（通知窗口环境无 toast 容器）
    }
  };
```

标题行中 `{payload.statusLabel}` 替换为 `{t(`sessions.statusLabels.${payload.status}`, payload.status)}`；`{payload.agentType}` 替换为 `{payload.agentLabel}`。

组件 JSX 末尾（主卡片 div 之后）追加候选列表：

```tsx
      {candidates && (
        <div className="bg-card flex h-screen w-screen flex-col gap-1 rounded-lg border p-3 shadow-2xl">
          <p className="text-xs font-semibold">{t("sessions.pickWindow")}</p>
          {candidates.map((w) => (
            <button
              key={w.hwnd}
              className="truncate rounded border px-2 py-1.5 text-left text-[11px] hover:bg-accent"
              title={w.title}
              onClick={() => {
                setCandidates(null);
                getCurrentWindow().hide();
                invoke("focus_hwnd", { hwnd: w.hwnd }).catch(() => {});
              }}
            >
              {w.title || t("sessions.untitledWindow")} — {w.process}
            </button>
          ))}
        </div>
      )}
```

注意：主卡片外层需要改为条件渲染 `{payload && !candidates && ( ...原卡片... )}`，避免候选列表与卡片同时出现。

- [ ] **Step 3: i18n 键**

`zh.json` 的 `sessions` 命名空间内追加：

```json
"statusLabels": {
  "waiting": "等待操作",
  "processing": "运行中",
  "thinking": "思考中",
  "compacting": "压缩中",
  "idle": "空闲",
  "finished": "已结束"
}
```

`en.json` 对应追加：

```json
"statusLabels": {
  "waiting": "Waiting for input",
  "processing": "Running",
  "thinking": "Thinking",
  "compacting": "Compacting",
  "idle": "Idle",
  "finished": "Finished"
}
```

- [ ] **Step 4: 验证与 Commit**

```bash
cd "E:\LLMproject\Github\MultiAgents-Manager" && pnpm check:i18n && pnpm lint && pnpm build
git add src/hooks/useNotification.ts src/pages/notification.tsx src/i18n/locales/zh.json src/i18n/locales/en.json
git commit -m "fix(notification): system toast fallback, inline window picker, i18n status labels"
```

---

### Task 3: marker spike 补做与结论记录（+ go 则实现注入）

**Files:**
- Modify: `docs/engineering/plans/2026-08-24-jump-v2-notification-i18n.md`（Task 3 Step 1 勾选框处补记结论）
- Modify（仅 go）: `src-tauri/src/monitor/hooks.rs`
- Modify: `src/components/sessions/SessionCard.tsx:106`（Minor 2 一行修）

- [ ] **Step 1: 补做 spike（上轮未做未记录）**

前提 A（hook 在本机生效）：跑一个 claude 会话发条消息，检查 `~/.mam/events/` 出现新事件文件。
前提 B（子进程写标题可达）：Git Bash 执行 `printf '\033]0;MAM:test1234\007' > /dev/tty`，标题栏出现 `MAM:test1234`。

- [ ] **Step 2: 记录结论（无条件执行）**

在计划文件 `2026-08-24-jump-v2-notification-i18n.md` 的 Task 3 Step 1 勾选框 `- [ ] **Step 1: Spike...**` 下方缩进补记一行结论，格式示例：

```
  > **spike 结论（2026-08-24 补录）**：前提 A 不成立（~/.mam/events 无新事件，hook 未触发）/ 前提 B 成立。判定 no-go，marker 层保持未启用，窗口歧义由标题打分 + 选择器兜底。
```

（按实际结果填写；两项都成立则判定 go 并继续 Step 3。）

- [ ] **Step 3:（仅 go）实施注入**

`hooks.rs` 的 `HOOK_SCRIPT` 常量中、写事件文件的 `echo ... > "$EVENTS_DIR/$PPID.json"` 行之后追加：

```bash
# 注入窗口标题 marker（MAM:<session_id 前 8 位>），供 MultiAgents Manager 跳转精确定位
printf '\\033]0;MAM:%s\\007' "$(printf '%s' "$SESSION_ID" | cut -c1-8)" > /dev/tty 2>/dev/null || true
```

（转义以 HOOK_SCRIPT 现有定界符风格为准。）并将 `ensure_hook_script` 的 `if !script_path.exists()` 外层条件去掉、改为总是 `fs::write`（保留 unix 权限段），使新脚本能覆盖旧版。

- [ ] **Step 4: SessionCard 前缀兜底一行修**

`src/components/sessions/SessionCard.tsx:106`：`session.id.slice(0, 12)` → `session.id.slice(0, 8)`。

- [ ] **Step 5: 验证与 Commit**

```bash
cd src-tauri && cargo clippy --all-targets -- -D warnings
cd "E:\LLMproject\Github\MultiAgents-Manager" && pnpm lint
git add docs/engineering/plans/2026-08-24-jump-v2-notification-i18n.md src-tauri/src/monitor/hooks.rs src/components/sessions/SessionCard.tsx
git commit -m "docs(plan): record marker spike conclusion (implement injection if go)"
```

（no-go 时 hooks.rs 无改动，git add 它不会产生变更，不影响提交。）

---

### Task 4: 门禁与人工验证

- [ ] **Step 1: 自动门禁**

```bash
cd src-tauri && cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --check
cd "E:\LLMproject\Github\MultiAgents-Manager" && pnpm check
```

- [ ] **Step 2: 人工验证（`pnpm tauri:dev`，多会话并行时验证最佳）**

1. 两个会话同轮触发通知 → 两条浮窗都出现且纵向堆叠（不再互相覆盖）
2. WT 多窗口下点击通知卡（打分未命中时）→ 通知窗内出现候选列表，点选后聚焦对应窗口（不再静默无反应）
3. 英文界面下浮窗状态词为英文（"Running" 等）；中文界面为中文
4. 打开"使用系统通知"开关 → 系统 toast 正常；点击 toast 的"查看会话"动作 → 正常跳转（不报错）
5. （若 Task 3 判定 go）跑 claude 会话 → 终端标题出现 `MAM:<8 位>` 且与看板卡片前缀一致；点击卡片直接精确聚焦
6. 回归：卡片点击跳转、6 秒自动消失、悬停保留均正常

- [ ] **Step 3: 汇报**

各 Task 状态（含 spike 结论 go/no-go 及依据）、门禁结果、人工验证逐项结果、`git log --oneline 2e4e55d..HEAD`。

---

## 范围外

- Minor 3-6（准死代码文案、StrictMode 监听器、300ms 固定延迟改 ack 机制、check-i18n 数组防御）——记录在审查报告，后续按需
- UIA 内容匹配、Windows Terminal 标签定位（延续上轮裁剪）
