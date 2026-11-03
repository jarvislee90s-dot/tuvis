# 状态修复 + 跳转准确性 + 提示音与标识 Implementation Plan（精简版）

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 实施三份已批准 spec：`specs/010-status-accuracy-and-notification`、`specs/011-jump-accuracy-and-notification-jump`、`specs/012-custom-sounds-and-tool-identity`。

**本轮约定（用户明确要求）**：执行从简，不复杂化，确保功能完成；**每任务有明确验收条件**（各任务内 + Task 9 汇总人工清单）；除 Task 1 的纯函数测试外不新增自动化测试；每任务一 commit。

**环境**：Windows（Git Bash），cargo 在 `src-tauri/` 下；TLS 报错时后台跑 `python C:\Users\bunny\AppData\Local\Temp\dsh-cargo-http-mirror.py 8765`。**macOS 零回归**硬约束（`window/mod.rs` 的 macOS 分支不动）。

---

## Part A — 状态判定与通知触发（spec 010）

### Task 1: 完成信号优先 + OpenCode CPU 防抖

**Files:**
- Modify: `src-tauri/src/monitor/status.rs:126-135`（`determine_status` 的 assistant 分支）
- Modify: `src-tauri/src/monitor/opencode_parser.rs:343-362`（`determine_opencode_status`）

- [ ] **Step 1: 写失败测试（纯函数，先改断言）**

`status.rs` 文件末尾追加：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assistant_text_is_idle_even_if_file_recent() {
        // 明确完成信号优先：assistant 纯文本 + 文件仍在年龄窗口内 → Idle（不再被拉回 Processing）
        assert_eq!(
            determine_status(Some("assistant"), false, false, false, false, false, true),
            SessionStatus::Idle
        );
        assert_eq!(
            determine_status(Some("assistant"), false, false, false, false, false, false),
            SessionStatus::Idle
        );
    }

    #[test]
    fn assistant_tool_use_is_processing() {
        assert_eq!(
            determine_status(Some("assistant"), true, false, false, false, false, false),
            SessionStatus::Processing
        );
        // 用户输入类工具（AskUserQuestion）→ Waiting
        assert_eq!(
            determine_status(Some("assistant"), true, false, false, false, true, false),
            SessionStatus::Waiting
        );
    }

    #[test]
    fn fallback_branch_still_uses_file_age() {
        // 兜底分支保留 file_recently_modified 语义
        assert_eq!(determine_status(None, false, false, false, false, false, true), SessionStatus::Processing);
        assert_eq!(determine_status(None, false, false, false, false, false, false), SessionStatus::Waiting);
    }
}
```

`opencode_parser.rs` 文件末尾追加：

```rust
#[cfg(test)]
mod status_tests {
    use super::*;

    #[test]
    fn cpu_spike_after_assistant_reply_is_not_processing() {
        // assistant 已回复完：CPU 抖动（后台 GC/索引）不得把状态拉回 Processing
        let now = chrono::Utc::now().timestamp_millis();
        assert_eq!(
            determine_opencode_status(50.0, Some("assistant"), now, now),
            SessionStatus::Idle
        );
    }

    #[test]
    fn cpu_still_marks_processing_before_reply() {
        // 尚未回复完（最后消息是 user）：高 CPU 正常判 Processing
        let now = chrono::Utc::now().timestamp_millis();
        assert_eq!(
            determine_opencode_status(50.0, Some("user"), now, now),
            SessionStatus::Processing
        );
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd src-tauri && cargo test --lib determine_status && cargo test --lib status_tests
```

预期：`assistant_text_is_idle_even_if_file_recent` 与两个 opencode 测试 FAIL。

- [ ] **Step 3: 实现**

`status.rs` 的 assistant 分支将：

```rust
            if has_tool_use && is_user_input_tool {
                SessionStatus::Waiting
            } else if has_tool_use || file_recently_modified {
                SessionStatus::Processing
            } else {
```

改为（完成信号优先，file_recently_modified 不再否决）：

```rust
            if has_tool_use && is_user_input_tool {
                SessionStatus::Waiting
            } else if has_tool_use {
                SessionStatus::Processing
            } else {
```

`opencode_parser.rs` 的 `determine_opencode_status` 将：

```rust
    if cpu > 5.0 {
        SessionStatus::Processing
    } else {
```

改为：

```rust
    // CPU 为瞬时采样噪声大：仅当会话不是"assistant 已回复完"且 CPU 明显高（阈值提高至 15%）
    // 才升级为 Processing，避免任务结束后后台活动（GC/索引）导致绿黄横跳
    if cpu > 15.0 && last_role != Some("assistant") {
        SessionStatus::Processing
    } else {
```

（其余分支不动。）

- [ ] **Step 4: 测试通过 + 全量回归 + Commit**

```bash
cd src-tauri && cargo test && cargo clippy --all-targets -- -D warnings
git add src-tauri/src/monitor/status.rs src-tauri/src/monitor/opencode_parser.rs
git commit -m "fix(monitor): completion signal takes priority over file age and cpu noise"
```

**验收条件**：新测试全过；既有测试无回归。

### Task 2: 通知时间去重

**Files:**
- Modify: `src/hooks/useNotification.ts`（prevStatuses 循环，当前 112-135 行附近）

- [ ] **Step 1: 实现**

① `prevStatuses` 的类型（当前 `useRef<Map<string, string>>`）改为：

```ts
  const prevStatuses = useRef<Map<string, { status: string; color: string; at: number }>>(
    new Map()
  );
```

② 循环内比较段（当前 `const prevStatus = ...` 到颜色未变 `continue`）替换为：

```ts
      const prev = prevStatuses.current.get(session.id);
      // 首次加载不通知
      if (!prev) {
        prevStatuses.current.set(session.id, {
          status: session.status,
          color: statusToColor(session.status),
          at: Date.now(),
        });
        continue;
      }

      const currColor = statusToColor(session.status);
      prevStatuses.current.set(session.id, {
        status: session.status,
        color: currColor,
        at: Date.now(),
      });

      // 颜色未变 → 不通知
      if (prev.color === currColor) continue;
      // 时间去重（5 秒内同目标颜色不重复弹，兜底状态抖动）
      if (prev.color !== currColor && currColor === prev.lastNotifiedColor) {
        if (Date.now() - prev.lastNotifiedAt < 5000) continue;
      }
```

（`lastNotifiedColor`/`lastNotifiedAt` 需并入存储结构——简化实现：直接在记录里加两个字段，未通知过时初始化为 `""`/`0`；弹窗执行点更新它们。若该结构造成实现别扭，允许用等效的平行 `Map<string, {color, at}>` 记录"上次通知"实现同一语义，以语义为准。）

- [ ] **Step 2: 验证 + Commit**

```bash
cd "E:\LLMproject\Github\MultiAgents-Manager" && pnpm lint && pnpm build
git add src/hooks/useNotification.ts
git commit -m "fix(notification): 5s color-dedup for session notifications"
```

**验收条件**：lint/build 通过；语义为"同会话 5 秒内重复翻转到同一颜色只弹一次"。

---

## Part B — 跳转准确性与通知窗联动（spec 011）

### Task 3: hook 注册修复 + marker 注入启用

**Files:**
- Modify: `src-tauri/src/monitor/hooks.rs`（HOOK_SCRIPT、ensure_hook_script、register_hooks_for_tool、register_all_hooks）
- Modify: `docs/engineering/plans/2026-08-24-jump-v2-notification-i18n.md`（spike 结论更新）

- [ ] **Step 1: HOOK_SCRIPT 注入 marker + ensure 总是重写**

HOOK_SCRIPT 的 `echo ... > "$EVENTS_DIR/$PPID.json"` 行之后追加：

```bash
# 注入窗口标题 marker（MAM:<session_id 前 8 位>），供 MultiAgents Manager 跳转精确定位
printf '\\033]0;MAM:%s\\007' "$(printf '%s' "$SESSION_ID" | cut -c1-8)" > /dev/tty 2>/dev/null || true
```

`ensure_hook_script` 将 `if !script_path.exists() { ...写+权限+log... }` 改为无条件执行写+权限段（删掉 if 包裹与"已创建"log；脚本由应用托管，幂等重写保证升级后新 marker 生效）。

- [ ] **Step 2: register_hooks_for_tool 合并式追加 + Windows 命令形态**

函数内 `let script_path_str = ...` 之后加：

```rust
    // Windows 无法直接执行 .sh，hook 命令经 bash 调用（Git Bash 随开发/使用环境存在）
    let command_str = if cfg!(windows) {
        format!("bash \"{}\"", script_path_str)
    } else {
        script_path_str.clone()
    };
```

"添加 Hook 条目"段（`let hook_entry = serde_json::json!([...]); hooks_obj.insert(&event_name, hook_entry); added += 1;`）替换为合并式（保留用户同事件已有条目，仅追加我们的）：

```rust
        // 合并式追加：用户已有同事件 hooks 时保留其条目，仅追加我们的（不整组替换）
        let our_entry = serde_json::json!({
            "matcher": "",
            "hooks": [{
                "type": "command",
                "command": &command_str
            }]
        });
        match hooks_obj.get_mut(&event_name) {
            Some(arr) if arr.is_array() => {
                arr.as_array_mut().unwrap().push(our_entry);
            }
            _ => {
                hooks_obj.insert(event_name, serde_json::json!([our_entry]));
            }
        }
        added += 1;
```

- [ ] **Step 3: register_all_hooks 改为按工具核验**

整个 `register_all_hooks` 替换为：

```rust
/// 为所有支持 Hook 的工具注册 Hook（在应用启动时调用）
/// 核验实际配置状态而非信任 DB 标志：修复"全局单标志 + 永不核验"导致的假阳性
/// （此前 claude 注册失败后因 codex 成功置位而永不重试）
pub fn register_all_hooks() {
    use crate::adapter::{claude::ClaudeAdapter, codex::CodexAdapter};
    use crate::adapter::{AgentAdapter, HookEventCase};

    let adapters: Vec<Box<dyn AgentAdapter>> = vec![Box::new(ClaudeAdapter), Box::new(CodexAdapter)];
    let script_path = ensure_hook_script();

    for adapter in &adapters {
        if !adapter.hook_supported() {
            continue;
        }
        let Some(config_path) = adapter.hook_config_path() else { continue };
        let tool_key = format!(
            "hooks_registered_{}",
            format!("{:?}", adapter.agent_type()).to_lowercase()
        );

        // 启动核验：配置文件实际包含 status-hook 引用且脚本存在才跳过
        let verified = fs::read_to_string(&config_path)
            .map(|c| c.contains("status-hook.sh"))
            .unwrap_or(false)
            && script_path.exists();
        if verified {
            crate::database::set_setting(&tool_key, "true");
            debug!("{} Hook 已确认: {:?}", adapter.name(), config_path);
            continue;
        }

        let events = adapter.hook_events();
        let is_pascal = matches!(adapter.hook_event_case(), HookEventCase::PascalCase);
        match register_hooks_for_tool(&config_path, &events, is_pascal) {
            Ok(()) => {
                info!("Hook 注册成功: {} → {:?}", adapter.name(), config_path);
                crate::database::set_setting(&tool_key, "true");
            }
            Err(e) => warn!("Hook 注册失败 {} → {:?}: {}", adapter.name(), config_path, e),
        }
    }
}
```

（旧全局 `hooks_registered` 键不再读取，自然废弃。注意修一处笔误：`Err(e) => warn!` 参数里的 `{:?}` 对齐 config_path。）

- [ ] **Step 4: 编译 + 现场验证（go/no-go 决策点）+ Commit**

```bash
cd src-tauri && cargo clippy --all-targets -- -D warnings && cargo test
```

**现场验证（必做）**：跑 `pnpm tauri:dev` 启动一次（触发注册）→ 检查 `~/.claude/settings.json` 出现 hooks 段且含 `bash ...status-hook.sh` → 在终端跑一个 claude 会话发条消息 → 检查 `~/.mam/events/` 出现新事件文件 **且终端标题出现 `MAM:<8 位>`**。若事件文件出现但标题无 marker → 记录现象（`/dev/tty` 在该环境不可达）并在 marker 行保留、依赖 Task 4 的候选过滤兜底；若事件文件都不出现（bash 形态不被 claude 执行）→ 记录结论，注册命令回退为直接路径。

```bash
git add src-tauri/src/monitor/hooks.rs
git commit -m "fix(hooks): per-tool verification, merge-style registration, and title marker"
```

- [ ] **Step 5: 更新 spike 结论文档**

在 `docs/engineering/plans/2026-08-24-jump-v2-notification-i18n.md` 的 spike 结论引用块下追加一行：

```
  > **2026-08-25 更新**：前提 A 根因（注册假阳性）已修复，marker 已启用；实际生效状态见本日执行记录。
```

（按 Step 4 实测结果如实填写，随代码一起 `git add` 提交。）

**验收条件**：`~/.claude/settings.json` 含 hooks 段（且用户已有配置未被删除）；claude 会话触发后 events 目录出现新事件；终端标题含 `MAM:<8 位>`（或如实记录 no-go 原因）。

### Task 4: 跳转候选按打分过滤

**Files:**
- Modify: `src-tauri/src/window/win32.rs`（Ambiguous 分支 + `WindowCandidate`）

- [ ] **Step 1: WindowCandidate 加 score 字段**

```rust
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WindowCandidate {
    pub hwnd: isize,
    pub title: String,
    pub process: String,
    pub score: i32,
}
```

- [ ] **Step 2: Ambiguous 分支过滤**

将（当前 199-207 行附近）：

```rust
        let candidates = cands
            .iter()
            .map(|(hwnd, title)| WindowCandidate {
                hwnd: *hwnd,
                title: title.clone(),
                process: proc_name.clone(),
            })
            .collect();
        return Ok(FocusOutcome::Ambiguous(candidates));
```

替换为（`scored` 已按分数降序）：

```rust
        // 候选过滤：优先仅返回打分 > 0（标题含项目名/工具名）的窗口，避免无关终端混入；
        // 全零时回退全量候选（按分数降序），保证"永远有得选"
        let mut candidates: Vec<WindowCandidate> = scored
            .iter()
            .filter(|(s, _)| *s > 0)
            .map(|(s, (hwnd, title))| WindowCandidate {
                hwnd: **hwnd,
                title: (*title).clone(),
                process: proc_name.clone(),
                score: *s,
            })
            .collect();
        if candidates.is_empty() {
            candidates = scored
                .iter()
                .map(|(s, (hwnd, title))| WindowCandidate {
                    hwnd: **hwnd,
                    title: (*title).clone(),
                    process: proc_name.clone(),
                    score: *s,
                })
                .collect();
        }
        return Ok(FocusOutcome::Ambiguous(candidates));
```

前端两处候选类型（`SessionCard.tsx` 与 `notification.tsx` 的 `windows?` 类型）加 `score?: number`（不改变渲染，可后续用于排序展示）。

- [ ] **Step 3: 验证 + Commit**

```bash
cd src-tauri && cargo test && cargo clippy --all-targets -- -D warnings
cd "E:\LLMproject\Github\MultiAgents-Manager" && pnpm build
git add src-tauri/src/window/win32.rs src/components/sessions/SessionCard.tsx src/pages/notification.tsx
git commit -m "fix(window): filter ambiguous candidates by title score"
```

**验收条件**：cargo/build 全过（既有测试无回归）。

### Task 5: 共享跳转 hook + 通知 10 秒

**Files:**
- Create: `src/hooks/useSessionJump.ts`
- Modify: `src/components/sessions/SessionCard.tsx`、`src/pages/notification.tsx`

- [ ] **Step 1: 新建共享 hook**

```ts
// 会话跳转共享逻辑 — 主界面卡片与通知浮窗复用同一实现（含歧义候选结果）
import { useState } from "react";
import { invoke } from "@tauri-apps/api/core";

export interface JumpWindowCandidate {
  hwnd: number;
  title: string;
  process: string;
  score?: number;
}

export interface JumpTarget {
  pid: number;
  id: string;
  agentType: string;
  projectName: string;
}

export function useSessionJump() {
  const [candidates, setCandidates] = useState<JumpWindowCandidate[] | null>(null);

  const focus = async (target: JumpTarget): Promise<JumpWindowCandidate[] | null> => {
    const result = await invoke<{ type: string; windows?: JumpWindowCandidate[] }>(
      "focus_session",
      {
        pid: target.pid,
        sessionId: target.id,
        agentType: target.agentType,
        projectName: target.projectName,
      }
    );
    const ambiguous = result.type === "ambiguous" && result.windows ? result.windows : null;
    setCandidates(ambiguous && ambiguous.length > 0 ? ambiguous : null);
    return ambiguous;
  };

  const focusHwnd = async (hwnd: number) => {
    await invoke("focus_hwnd", { hwnd });
  };

  return { candidates, setCandidates, focus, focusHwnd };
}
```

- [ ] **Step 2: SessionCard 接入**

删除组件内 `pendingWindows` state 与 `handleClick` 中的 invoke 逻辑，改用 hook：

```tsx
  const { candidates, setCandidates, focus, focusHwnd } = useSessionJump();

  const handleClick = async () => {
    if (!session.jumpSupported) {
      toast.info(t("sessions.jumpUnsupported"));
      return;
    }
    try {
      await focus({
        pid: session.pid,
        id: session.id,
        agentType: session.agentType,
        projectName: session.projectName,
      });
    } catch (e) {
      toast.error(t("sessions.jumpFailed", { error: e }));
    }
  };
```

（选择器弹层 JSX 中的 `pendingWindows` 改名 `candidates`，按钮点击改 `await focusHwnd(w.hwnd);`，其余不变；本地不再需要 `invoke` import 时移除。）

- [ ] **Step 3: notification.tsx 接入 + 10 秒**

`jump()` 改用共享 hook：`const ambiguous = await focus({ pid: payload.pid, id: payload.sessionId, agentType: payload.agentType, projectName: payload.projectName }); if (ambiguous) getCurrentWindow().show();`（候选渲染用 hook 的 `candidates` state，内联列表保持现有 JSX）；`armTimer(6000)` → `armTimer(10000)`；卡片 `onMouseLeave` 的 `armTimer(3000)` → `armTimer(5000)`。

- [ ] **Step 4: 验证 + Commit**

```bash
cd "E:\LLMproject\Github\MultiAgents-Manager" && pnpm lint && pnpm build
git add src/hooks/useSessionJump.ts src/components/sessions/SessionCard.tsx src/pages/notification.tsx
git commit -m "refactor(ui): shared session jump hook and 10s notification timeout"
```

**验收条件**：主界面点击与通知点击走同一实现（grep 确认无第二处 `invoke("focus_session")` 散落——useNotification 的系统通知 onAction 处理器除外）；通知 10 秒自动隐藏。

---

## Part C — 提示音与工具标识（spec 012）

### Task 6: 音效系统 + 设置页（合成音彻底移除）

**Files:**
- Create: `public/sounds/`（12 个 wav 拷入）
- Modify: `src/lib/audio.ts`（整体重写）
- Modify: `src/hooks/useNotification.ts`（触发规则）
- Modify: `src/pages/settings.tsx`（音频区重做）
- Modify: `src/i18n/locales/zh.json`、`en.json`

- [ ] **Step 1: 素材入构建**

```bash
cd "E:\LLMproject\Github\MultiAgents-Manager" && mkdir -p public/sounds && cp assets/NoticeSound/*.wav public/sounds/ && ls public/sounds | wc -l
```

预期输出 `12`。

- [ ] **Step 2: 重写 audio.ts**

整文件替换为：

```ts
// 文件音效系统 — 12 个内置音效，全局默认 + 每工具覆盖（localStorage: mam-sound-config）

export interface SoundConfig {
  default: string; // 音效 id 或 "mute"
  tools: Partial<Record<"claude" | "codex" | "opencode" | "openclaw", string>>; // 音效 id 或 "mute"
}

export const SOUND_IDS = [
  "notification_accomplished_04",
  "notification_accomplished_06",
  "notification_activated_05",
  "notification_message_02",
  "notification_message_04",
  "notification_operation_failed_03",
  "notification_operation_succeed_01",
  "notification_operation_succeed_03",
  "notification_operation_succeed_06",
  "notification_operation_succeed_09",
  "notification_searching_03",
  "notification_wrong_02",
] as const;

const STORAGE_KEY = "mam-sound-config";
// 旧合成音配置键，读取时忽略并清理
const LEGACY_KEY = "mam-audio-frequencies";

export function getSoundConfig(): SoundConfig {
  try {
    localStorage.removeItem(LEGACY_KEY);
    const saved = localStorage.getItem(STORAGE_KEY);
    if (saved) return { default: "notification_operation_succeed_01", tools: {}, ...JSON.parse(saved) };
  } catch {
    // ignore
  }
  return { default: "notification_operation_succeed_01", tools: {} };
}

export function saveSoundConfig(config: SoundConfig) {
  localStorage.setItem(STORAGE_KEY, JSON.stringify(config));
}

// === 播放引擎（解码缓存） ===
let audioCtx: AudioContext | null = null;
const bufferCache = new Map<string, AudioBuffer>();

function getContext(): AudioContext | null {
  if (typeof window === "undefined") return null;
  if (!audioCtx) audioCtx = new AudioContext();
  if (audioCtx.state === "suspended") audioCtx.resume();
  return audioCtx;
}

async function loadBuffer(id: string): Promise<AudioBuffer | null> {
  if (bufferCache.has(id)) return bufferCache.get(id)!;
  try {
    const res = await fetch(`/sounds/${id}.wav`);
    const data = await res.arrayBuffer();
    const ctx = getContext();
    if (!ctx) return null;
    const buf = await ctx.decodeAudioData(data);
    bufferCache.set(id, buf);
    return buf;
  } catch {
    return null;
  }
}

/** 播放指定音效（试听与实际触发共用） */
export async function playSound(id: string) {
  if (!SOUND_IDS.includes(id as (typeof SOUND_IDS)[number])) return;
  const ctx = getContext();
  const buf = await loadBuffer(id);
  if (!ctx || !buf) return;
  const source = ctx.createBufferSource();
  source.buffer = buf;
  source.connect(ctx.destination);
  source.start();
}

/** 任务完成（→绿）时按工具播放：专属覆盖 → 全局默认；mute 跳过 */
export function playCompletionSound(agentType: string) {
  const cfg = getSoundConfig();
  const id = cfg.tools[agentType as keyof SoundConfig["tools"]] ?? cfg.default;
  if (id && id !== "mute") playSound(id);
}
```

- [ ] **Step 3: 触发规则（useNotification.ts）**

当前 `playSoundForStatus(session.status);` 调用处替换为（方向过滤：仅变为绿时响）：

```ts
        if (currColor === "green") playCompletionSound(session.agentType);
```

import 行同步替换（`playSoundForStatus` → `playCompletionSound`）。

- [ ] **Step 4: 设置页音频区重做（settings.tsx）**

删除 Hz 配置 UI（`frequencyConfig`/`waitingStatus`/`finishedStatus` 三段及 `audioConfig` state、`updateAudioConfig`、对 `getAudioConfig`/`saveUserFrequencies` 的 import），替换为：

- 「全局完成音」：Select（选项 = 12 音效 + 静音）+ 试听按钮（`playSound(id)`）
- 四个工具行（Claude/Codex/OpenCode/OpenClaw）：默认「跟随全局」，可改为任一音效或静音 + 试听
- 状态用 `getSoundConfig()`/`saveSoundConfig()`（沿用本文件现有表单模式；试听按钮可直接 `onClick={() => playSound(x)}`）

i18n：`settings.notifications` 命名空间删除不再使用的键（frequencyConfig/waitingStatus/finishedStatus 等），新增（zh/en 同步）：`soundGlobalDefault`（全局完成音）、`soundFollowGlobal`（跟随全局）、`soundMute`（静音）、`soundTest`（试听）、`soundToolOverride`（工具专属音）。

- [ ] **Step 5: 验证 + Commit**

```bash
cd "E:\LLMproject\Github\MultiAgents-Manager" && pnpm check
git add public/sounds src/lib/audio.ts src/hooks/useNotification.ts src/pages/settings.tsx src/i18n/locales/zh.json src/i18n/locales/en.json
git commit -m "feat(audio): file-based per-tool completion sounds replacing synth tones"
```

**验收条件**：`pnpm check` 全过（含 check:i18n 键对齐）；设置页无 Hz UI；`grep -rn "playTone\|playSoundForStatus\|mam-audio-frequencies" src/` 仅剩 audio.ts 中 LEGACY_KEY 清理一处。

### Task 7: 工具标识（图标 + 配色 + openclaw + 通知窗）

**Files:**
- Create: `src/components/icons/BrandIcons.tsx`（path 数据从 `assets/icons/*.svg` 提取）
- Modify: `src/components/sessions/SessionCard.tsx`（AGENT_BADGE 迁移+改造）
- Create: `src/lib/agentBadge.tsx`（共享映射）
- Modify: `src/pages/notification.tsx`（头部加图标与工具色）
- `git mv assets/icons → 删除`（素材迁至组件内嵌后清理）

- [ ] **Step 1: 品牌图标组件**

创建 `src/components/icons/BrandIcons.tsx`：

```tsx
// 品牌图标 — path 取自 assets/icons/*.svg（simple-icons / opencode 官网），currentColor 随文字色

export function ClaudeIcon({ className }: { className?: string }) {
  return (
    <svg className={className} viewBox="0 0 24 24" fill="currentColor" role="img" aria-label="Claude">
      <path d={/* 从 assets/icons/claude.svg 的 <path d="..."> 复制 */ ""} />
    </svg>
  );
}

export function OpenAIIcon({ className }: { className?: string }) {
  return (
    <svg className={className} viewBox="0 0 24 24" fill="currentColor" role="img" aria-label="OpenAI">
      <path d={/* 从 assets/icons/openai.svg 复制 */ ""} />
    </svg>
  );
}

export function OpenCodeIcon({ className }: { className?: string }) {
  return (
    <svg className={className} viewBox="0 0 512 512" role="img" aria-label="OpenCode">
      {/* 从 assets/icons/opencode.svg 复制内部元素（该文件为嵌套 svg 结构，整体内联） */}
    </svg>
  );
}
```

（执行时把三个 SVG 文件的内容实际粘入——保留各自 viewBox 与路径，颜色统一 currentColor；opencode 图标自带配色则原样保留其填充。）

- [ ] **Step 2: 共享映射 agentBadge**

创建 `src/lib/agentBadge.tsx`：

```tsx
// 工具徽标映射 — SessionCard 与通知浮窗共用（配色：codex 紫 / claude 橙 / opencode 灰白 / openclaw 灰）
import type { ComponentType } from "react";
import { ClaudeIcon, OpenAIIcon, OpenCodeIcon } from "@/components/icons/BrandIcons";

export interface AgentBadge {
  label: string;
  className: string;
  Icon: ComponentType<{ className?: string }>;
}

export const AGENT_BADGE: Record<string, AgentBadge> = {
  claude: {
    label: "Claude",
    className: "border-orange-500/30 bg-orange-500/15 text-orange-400",
    Icon: ClaudeIcon,
  },
  codex: {
    label: "Codex",
    className: "border-purple-500/30 bg-purple-500/15 text-purple-400",
    Icon: OpenAIIcon,
  },
  opencode: {
    label: "OpenCode",
    className: "border-zinc-500/40 bg-zinc-800/80 text-zinc-100",
    Icon: OpenCodeIcon,
  },
  openclaw: {
    label: "OpenClaw",
    className: "border-gray-500/30 bg-gray-500/15 text-gray-300",
    Icon: OpenCodeIcon, // 无品牌素材，暂用占位图标，后续替换
  },
};
```

- [ ] **Step 3: SessionCard 接入**

删除文件内的 `AGENT_BADGE` 定义与 lucide 相关 import（`Bot`/`Terminal`/`FolderGit2` 若无其他使用一并清理），改 `import { AGENT_BADGE } from "@/lib/agentBadge";`。卡片头部结构保持现状（顺序已为 图标+工具名 → 项目名 → session 8 位 → git 分支，符合目标布局），仅徽标渲染换新映射（`badge.Icon` 用法不变）。

- [ ] **Step 4: 通知窗头部**

`notification.tsx` 标题行的 `{payload.agentLabel}` 替换为徽标形式：

```tsx
          {(() => {
            const badge = AGENT_BADGE[payload.agentType];
            return badge ? (
              <span className={cn("inline-flex items-center gap-1 rounded border px-1.5 py-0.5", badge.className)}>
                <badge.Icon className="h-3 w-3" />
                {payload.agentLabel}
              </span>
            ) : (
              payload.agentLabel
            );
          })()}
```

（`cn` 与 `AGENT_BADGE` 按现有 import 方式引入。）

- [ ] **Step 5: 清理素材双份 + 验证 + Commit**

```bash
git rm -r assets/icons
cd "E:\LLMproject\Github\MultiAgents-Manager" && pnpm lint && pnpm build && pnpm check:i18n
git add src/components/icons/BrandIcons.tsx src/lib/agentBadge.tsx src/components/sessions/SessionCard.tsx src/pages/notification.tsx
git commit -m "feat(ui): brand icons and tool colors unified across card and notification"
```

**验收条件**：构建通过；`assets/icons/` 已删（path 已内联组件）；暗色/亮色主题下四个工具徽标均清晰可见。

### Task 8: 全量门禁 + 人工验收清单

- [ ] **Step 1: 自动门禁**

```bash
cd src-tauri && cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --check
cd "E:\LLMproject\Github\MultiAgents-Manager" && pnpm check
```

- [ ] **Step 2: 人工验收清单（明确验收条件，逐项记录 ✅/❌/待用户）**

**状态与通知（spec 010）**：

| # | 操作 | 预期（判定标准） |
|---|------|------------------|
| 1 | Codex App 跑一个任务至结束 | 结束后 ≤2 个轮询周期（目测 ≤10 秒）卡片变绿，且 5 分钟内不回跳黄、无重复弹窗 |
| 2 | 终端 claude/codex 任务结束 | 同上，≤10 秒变绿且稳定 |
| 3 | OpenCode 任务结束后静置 10 分钟 | 状态保持绿，无任何弹窗（可接受最多 1 次结束弹窗） |
| 4 | 空闲会话发起新任务 | 变红/黄并弹窗（开始提示不受影响） |

**跳转（spec 011）**：

| # | 操作 | 预期 |
|---|------|------|
| 5 | 启动应用后查 `~/.claude/settings.json` | 含 hooks 段且命令为 `bash ...status-hook.sh`；原有的其他配置键完整保留 |
| 6 | 跑 claude 会话发消息 | `~/.mam/events/` 出现新事件文件；终端标题出现 `MAM:<8 位>` 且与卡片前缀一致（若 no-go 如实记录） |
| 7 | 开两个终端各跑一个 claude（不同项目）+ 一个 opencode + 一个空 PowerShell，依次点两张 claude 卡 | 各自精确聚焦（marker 命中，无选择器） |
| 8 | 制造歧义（如 marker 未生效时重复 7）点 claude 卡 | 选择器只列 claude 相关窗口；全零匹配才出现全部窗口 |
| 9 | 通知浮窗弹出后不操作 | 10 秒消失；悬停保留；移开 5 秒消失 |
| 10 | 点击通知卡 | 跳转行为与主界面点同一会话卡片完全一致 |

**提示音与标识（spec 012）**：

| # | 操作 | 预期 |
|---|------|------|
| 11 | 设置页试听各音效 | 每个选项立即出声 |
| 12 | claude 配专属音、codex 跟随全局，两任务先后结束 | 分别播放对应音效；音效互不相同 |
| 13 | 某工具设静音后任务结束 | 无声但弹窗照常 |
| 14 | 任务开始（绿→黄/红） | 无任何提示音 |
| 15 | 查看卡片与通知窗 | 图标+工具名+项目名+8 位标识顺序正确；codex 紫/claude 橙/opencode 灰白/openclaw 灰；明暗主题均清晰 |
| 16 | 切换语言 | 新增文案跟随 i18n |

- [ ] **Step 3: 汇报**

各 Task 状态（含 Task 3 Step 4 的实测结论）、门禁结果、16 项验收逐项结果（不能实机验证的标"待用户验证"）、`git log --oneline 5994d5d..HEAD`。

---

## 范围外

- 等待音配置（用户决策：仅完成音）
- openclaw 品牌图标素材（文字+占位图标）
- 音频压缩转码、UIA 内容匹配、WT 标签级定位（延续既往裁剪）
- macOS ChatGPT 聚焦、Linux/Wayland
