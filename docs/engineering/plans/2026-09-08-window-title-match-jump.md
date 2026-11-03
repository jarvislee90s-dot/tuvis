# 终端窗口标题匹配跳转（kimi + opencode）+ codex id 前缀加长 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在 Windows 跳转判定链的"marker 匹配"之后新增"窗口标题 ≡ 会话标题"演绎锁定层，使 kimi 与 opencode 在同一 WindowsTerminal 进程双开时能自动锁定目标窗口，不再弹人工选择器；同时修复 codex 会话 id 前 8 位截断在同一分钟启动时会撞车的问题（截断加长到 12 位）。

**Architecture:** 数据流是"会话侧已有的 title"经 IPC 透传到后端 `resolve_and_focus`，在候选窗口 ≥2 时先于硬排除洋葱做正向身份匹配（归一化后相等或前缀关系 + 唯一性守卫 + 他工具认领守卫）。kimi 的 title 来自 `state.json.title`（实测与终端标题同源），opencode 的 title 来自 SQLite `session.title`（窗口标题 = `OC | ` + title）。纯加法：匹配不命中时完全落回现有 ②-⑤ 层，零回归。

**Tech Stack:** Rust（Tauri 2 后端，windows crate 的 Win32 枚举/UIA）、TypeScript（React 19 前端，Tauri IPC invoke）、cargo test 单测。

**背景实测证据（2026-09-08 本机探测，勿在执行时重测）：**

- kimi 双开：两窗口标题 `（1）使用技能【convert-excel-report】，把…` / `（0）使用技能【ratingdog-report】，下载【苏州市…`，与各自 `state.json.title` 的开头逐字吻合（终端侧截断到 ~30 字符显示）。state.json title 截断到 200 字符存储。
- opencode 双开：两窗口标题 `OC | 公司资产查询` / `OC | 公司股东查询`，与 opencode.db `session.title`（`公司资产查询` / `公司股东查询`）逐字吻合。
- codex 同项目双开：两窗口标题均为项目目录名（无会话区分度），且最近 4 个 rollout 中出现两个不同会话 id 前 8 位相同（`01a08083-5ca0-…` 与 `01a08083-2260-…`，UUIDv7 时间戳前缀 8 hex 字符只编码到 65.5 秒精度）。
- 现有决策树（`win32.rs::resolve_and_focus`）：① marker（死通道，保留）→ ② 硬排除洋葱 L1/L2/L3 → ③ UIA 壳排除 L4 → ④ lastMessage 尾串包含 → ⑤ Ambiguous 选择器。祖先链扫描在 `WindowsTerminal.exe`（单进程多窗口）处拿到全部窗口进入消歧。

---

## File Structure

- Modify: `src-tauri/src/window/win32.rs` — 新增标题匹配纯函数 + `resolve_and_focus` 增参 + 兼容入口适配
- Modify: `src-tauri/src/commands/session.rs` — `focus_session` IPC 契约加 `title` 参数并透传
- Modify: `src-tauri/src/monitor/codex_parser.rs` — id 截断 8→12 位
- Modify: `src/hooks/useSessionJump.ts` — `JumpTarget` 加 `title?` 字段并透传给后端
- Modify: `src/components/sessions/SessionCard.tsx`、`src/components/pet/FoxbellPet.tsx`、`src/components/notifications/NotificationBell.tsx`、`src/pages/notification.tsx` — 跳转调用点补传 title
- Modify: `src/lib/notificationHistory.ts`、`src/hooks/useNotification.ts` — 通知历史链路补 title（使铃铛历史跳转也能用标题匹配）
- Modify: `src-tauri/src/commands/notification.rs` — `NotificationPayload` 加 `title` 字段（通知浮窗跳转链路）
- Modify: `src/pages/settings.tsx` — 测试通知 payload 补 `title`（新必填字段）

注意：`kimi_parser.rs` 与 `opencode_parser.rs` **不需要改**——两者已正确生产 title（`state.json.title` / DB `session.title`），本次只做消费端透传。

## 关键设计决定（执行者必读）

1. **归一化函数复用** `normalize_title_for_project`（剥盲文 spinner U+2800–U+28FF、trim、小写），再叠加 `collapse_ws`（连续空白压单空格）与尾部省略号剥离（`…`/`...`）。不新造归一化函数。
2. **前缀方向只有一种**：`窗口标题是键的前缀`（截断发生在终端侧，键=完整 title 更长）。反向（键是窗口标题前缀）不做——无实际场景，YAGNI。
3. **守卫两道**：(a) 窗口被其他工具认领（`claim_owner` 返回非本工具）时不参与匹配——他工具窗口绝不跳；(b) 命中数必须恰好 1，0 或 ≥2 都不锁定、落回下一层（P2-2 哲学：宁弹选择器不跳错）。
4. **opencode 前缀剥离**：窗口标题形态 `OC | <title>`，匹配时对窗口标题剥掉 `oc |` 前缀（小写归一化后比较）。kimi 无前缀，剥前缀函数对不含该前缀的标题必须原样返回。
5. **空键守卫**：title 为 None/空串时跳过本层（避免空串与一切标题"相等"的假命中）。
6. **插入位置**：在 marker 匹配之后、硬排除洋葱之前。命中时完全不触发 UIA 读取（省最多 800ms）。
7. **参数膨胀**：`resolve_and_focus` 已有 7 参数，再加 title 触发 clippy `too_many_arguments`（阈值 7）。方案：把 `session_marker / agent_keyword / project_name / last_message / title` 收拢为 `JumpHints` 结构体，`running_projects` 保持独立参数（它是扫描产物，语义不同）。两个调用点同步改。
8. **title 的消费范围**：仅用于标题匹配（第 ②′ 层）。选择器打分（原第 ⑤ 层）也可顺手加 title 命中分，但为控制改动面，本期**不加**——选择器已有 UIA 前缀排序兜底。

---

### Task 1: win32.rs — 标题匹配纯函数（TDD）

**Files:**
- Modify: `src-tauri/src/window/win32.rs`

- [ ] **Step 1: 写失败测试**

在 `win32.rs` 底部现有 `mod tests` 内追加（`use super::` 行已有 `hard_survivors` 等，追加 `title_match_lock`、`JumpHints` 相关导入——实际同模块直接用 `super::` 前缀即可）：

```rust
    // ---- 标题匹配层（②′）：窗口标题 ≡ 会话标题演绎锁定 ----

    #[test]
    fn title_match_equal_after_normalize() {
        // 完全相等（归一化后）：spinner 前缀、空白差异、大小写均被归一化吸收
        let cands = vec![
            (10isize, "（1）使用技能【convert-excel-report】，把".to_string()),
            (20, "（0）使用技能【ratingdog-report】，下载【苏州市".to_string()),
        ];
        let keys = vec!["（0）使用技能【ratingdog-report】，下载【苏州市农业发展集团有限公司】的 YY评级报告到项目本级目录".to_string()];
        let hit = title_match_lock(&cands, "kimi", &keys);
        assert_eq!(hit, Some(20));
    }

    #[test]
    fn title_match_window_is_prefix_of_key() {
        // 窗口标题是键的前缀（终端截断方向）
        let cands = vec![(30isize, "OC | 公司资产查询".to_string())];
        let keys = vec!["公司资产查询".to_string()];
        let hit = title_match_lock(&cands, "opencode", &keys);
        assert_eq!(hit, Some(30));
    }

    #[test]
    fn title_match_strips_oc_prefix() {
        // "OC | " 前缀被剥离后才比较；无前缀的标题不受影响
        let cands = vec![
            (40isize, "OC | 公司股东查询".to_string()),
            (41, "（1）使用技能".to_string()),
        ];
        let keys = vec!["公司股东查询".to_string()];
        let hit = title_match_lock(&cands, "opencode", &keys);
        assert_eq!(hit, Some(40));
    }

    #[test]
    fn title_match_two_hits_never_locks() {
        // 唯一性守卫：两个窗口都命中 → 不锁（宁弹选择器）
        let cands = vec![
            (50isize, "（5）使用技能【信评-处理修改意见生成终稿】".to_string()),
            (51, "（5）使用技能【信评-处理修改意见生成终稿】".to_string()),
        ];
        let keys = vec!["（5）使用技能【信评-处理修改意见生成终稿】 更新完之后股东、行业".to_string()];
        // 归一化后两窗口标题都与键的前 22 字构成前缀关系 → 双命中
        assert_eq!(title_match_lock(&cands, "kimi", &keys), None);
    }

    #[test]
    fn title_match_zero_hits_returns_none() {
        let cands = vec![(60isize, "完全不相关标题".to_string())];
        let keys = vec!["（0）使用技能【ratingdog-report】".to_string()];
        assert_eq!(title_match_lock(&cands, "kimi", &keys), None);
    }

    #[test]
    fn title_match_empty_key_skipped() {
        // 空键守卫：title 缺失时不产生假命中
        let cands = vec![(70isize, "任何标题".to_string())];
        let keys = vec![String::new()];
        assert_eq!(title_match_lock(&cands, "kimi", &keys), None);
    }

    #[test]
    fn title_match_other_tool_claim_never_hits() {
        // 他工具认领守卫：kimi 的键不能锁进 claude 认领的窗口
        let cands = vec![(80isize, "✳ Claude Code".to_string())];
        // 构造一个恰含 "claude" 字样的 kimi title——键命中但窗口被 claude 认领
        let keys = vec!["claude code 使用记录".to_string()];
        assert_eq!(title_match_lock(&cands, "kimi", &keys), None);
    }

    #[test]
    fn title_match_key_prefix_of_window_not_matched() {
        // 反向前缀（键是窗口标题前缀）不匹配：截断只在终端侧发生
        // 键="公司资产查询" 而窗口="OC | 公司资产查询的更多内容"——不现实场景，不锁定
        let cands = vec![(90isize, "OC | 公司资产查询的更多内容".to_string())];
        let keys = vec!["公司资产查询".to_string()];
        assert_eq!(title_match_lock(&cands, "opencode", &keys), None);
    }

    #[test]
    fn title_match_strips_ellipsis() {
        // 终端侧截断后可能带省略号尾巴
        let cands = vec![(100isize, "（2）使用技能【extract-report技能】…".to_string())];
        let keys = vec!["（2）使用技能【extract-report技能】，把二级目录下 full.md 提取经营数据".to_string()];
        assert_eq!(title_match_lock(&cands, "kimi", &keys), Some(100));
    }
```

- [ ] **Step 2: 运行测试确认失败**

```bash
cd src-tauri && cargo test title_match
```

Expected: 编译失败 `cannot find function title_match_lock in this scope`（函数未实现）。

- [ ] **Step 3: 实现 `JumpHints` 结构与 `title_match_lock` 纯函数**

在 `win32.rs` 顶部（`TOOL_CLAIM_KEYWORDS` 附近、`normalize_title_for_project` 之后）加：

```rust
/// 跳转消歧的会话侧身份线索包（由 focus_session IPC 透传组装）。
/// 收拢为结构体避免 resolve_and_focus 参数超过 clippy too_many_arguments 阈值（7）
pub struct JumpHints<'a> {
    /// hook 注入的标题标记（如 "MAM:1ba8e2f7"），通道 no-go 但代码保留
    pub session_marker: Option<&'a str>,
    /// 工具 id 小写（"kimi"/"opencode"…），认领判定与打分用
    pub agent_keyword: Option<&'a str>,
    /// 项目目录名，选择器打分用
    pub project_name: Option<&'a str>,
    /// 最近一条消息，UIA 尾串匹配用
    pub last_message: Option<&'a str>,
    /// 会话标题（kimi=state.json.title / opencode=DB session.title / claude、codex=id 前缀），
    /// 标题匹配层（②′）的键
    pub title: Option<&'a str>,
}

/// 剥窗口标题的 "OC | " 工具前缀（opencode 终端标题形态 "OC | <会话标题>"）。
/// 不含前缀的标题原样返回。大小写不敏感（实测 "OC | "，防御其他壳的大小写变体）
fn strip_oc_prefix(title: &str) -> &str {
    let t = title.trim_start();
    if t.len() >= 5 && t[..5].eq_ignore_ascii_case("oc | ") {
        &t[5..]
    } else {
        title
    }
}

/// 归一化标题用于匹配：剥 "OC | " 前缀 → 剥盲文 spinner（normalize_title_for_project）
/// → 压空白 → 剥尾部省略号 → 小写
fn normalize_window_title(title: &str) -> String {
    let stripped = strip_oc_prefix(title);
    let n = normalize_title_for_project(stripped);
    let n = n.trim_end_matches('…');
    let n = n.trim_end_matches('.').trim_end_matches('.').trim_end_matches('.');
    // trim_end_matches('.') 会把 "..." 全剥掉；再补一次省略号点号组合的稳妥剥离
    let n = collapse_ws(n);
    n.trim_end_matches('…').to_string()
}
```

⚠️ 上面的省略号剥离写得别扭（`trim_end_matches('.')` 连普通句点也会剥掉，会把以句点结尾的正常标题误伤）。按下面这个更精确的版本实现（二选一，以本版本为准）：

```rust
/// 归一化标题用于匹配：剥 "OC | " 前缀 → 剥盲文 spinner（normalize_title_for_project）
/// → 剥尾部省略号（… / ...）→ 压空白 → 小写
fn normalize_window_title(title: &str) -> String {
    let stripped = strip_oc_prefix(title);
    let n = normalize_title_for_project(stripped);
    let n = collapse_ws(n);
    n.trim_end_matches("…")
        .trim_end_matches("...")
        .trim()
        .to_string()
}
```

然后实现匹配函数（放在 `single_survivor` 附近）：

```rust
/// 标题匹配层（②′）：窗口标题（归一化）与会话标题键（归一化）相等或
/// "窗口标题是键的前缀"（终端截断方向），且候选中恰好 1 个命中 → 锁定。
/// 守卫：他工具认领的窗口绝不参与；空键跳过；命中 0 或 ≥2 都返回 None（落回下层）。
/// 键列表由调用方组装（opencode 需同时提供 DB title 等；kimi 传 state.title 一项即可）
fn title_match_lock(
    cands: &[(isize, String)],
    agent: &str,
    keys: &[String],
) -> Option<isize> {
    let keys_norm: Vec<String> = keys
        .iter()
        .filter(|k| !k.trim().is_empty())
        .map(|k| normalize_window_title(k))
        .collect();
    if keys_norm.is_empty() {
        return None;
    }
    let hits: Vec<isize> = cands
        .iter()
        .filter(|(_, t)| {
            // 他工具认领的窗口绝不参与命中
            if let Some(owner) = claim_owner(t) {
                if owner != agent {
                    return false;
                }
            }
            let wt = normalize_window_title(t);
            keys_norm
                .iter()
                .any(|k| wt == *k || k.starts_with(&wt))
        })
        .map(|(h, _)| *h)
        .collect();
    if hits.len() == 1 {
        Some(hits[0])
    } else {
        None
    }
}
```

- [ ] **Step 4: 运行测试确认通过**

```bash
cd src-tauri && cargo test title_match
```

Expected: 9 个 `title_match_*` 测试全部 PASS。

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/window/win32.rs
git commit -m "feat(jump): add title-match disambiguation primitive in win32"
```

---

### Task 2: win32.rs — `JumpHints` 接入 `resolve_and_focus`（TDD）

**Files:**
- Modify: `src-tauri/src/window/win32.rs`

- [ ] **Step 1: 写失败测试**

在 `mod tests` 追加（不依赖真实 Win32，只测纯函数层的键组装）：

```rust
    #[test]
    fn title_keys_kimi_uses_state_title_only() {
        // kimi：键 = state.title 一项；空/None → 空列表（层跳过）
        assert_eq!(title_keys_for(Some("（0）使用技能"), "kimi"), vec!["（0）使用技能【ratingdog-report】，下载".to_string()].len(), "占位断言见 Step 3 实现");
    }
```

⚠️ 这个测试依赖 `title_keys` 的实现形态，先别写死断言。**按以下方式写（确定性断言，无占位）：**

```rust
    #[test]
    fn title_keys_kimi_single_key() {
        // kimi：键 = [state.title]；title 缺失 → 空列表（层自然跳过）
        assert_eq!(
            title_keys("kimi", Some("（0）使用技能【ratingdog-report】，下载")),
            vec!["（0）使用技能【ratingdog-report】，下载".to_string()]
        );
        assert!(title_keys("kimi", None).is_empty());
    }

    #[test]
    fn title_keys_opencode_single_key() {
        // opencode：键 = [session.title]（前缀剥离在窗口侧做，键保持原样）
        assert_eq!(
            title_keys("opencode", Some("公司资产查询")),
            vec!["公司资产查询".to_string()]
        );
        assert!(title_keys("opencode", None).is_empty());
    }

    #[test]
    fn title_keys_id_prefix_tools_get_empty_keys() {
        // claude/codex 的 title 是 id 前 8/12 位，与终端标题无前缀关系；
        // 传入也是无害 miss，但为省事直接不给键
        assert!(title_keys("claude", Some("01a08083")).is_empty());
        assert!(title_keys("codex", Some("01a08083")).is_empty());
        assert!(title_keys("workbuddy", Some("WorkBuddy")).is_empty());
        assert!(title_keys("openclaw", Some("my-agent")).is_empty());
    }
```

- [ ] **Step 2: 运行测试确认失败**

```bash
cd src-tauri && cargo test title_keys
```

Expected: 编译失败 `cannot find function title_keys`。

- [ ] **Step 3: 实现键组装 + `resolve_and_focus` 改签名**

键组装（放在 `title_match_lock` 旁）：

```rust
/// 会话标题键组装：仅标题里携带会话区分度信息的工具接入（kimi/opencode 已实测同源）。
/// claude/codex/openclaw/workbuddy 返回空列表——本层对其自然跳过
fn title_keys(agent: &str, title: Option<&str>) -> Vec<String> {
    match agent {
        "kimi" | "opencode" => title
            .filter(|t| !t.trim().is_empty())
            .map(|t| vec![t.to_string()])
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}
```

`resolve_and_focus` 签名改造（`win32.rs:405` 起）。把 `session_marker/agent_keyword/project_name/last_message` 四个独立参数收拢进 `JumpHints`（加 `title`）：

```rust
/// 解析并聚焦（CLI 与 App 统一入口，路径差异见模块头注释）
/// hints: 会话侧身份线索（marker/工具/项目名/lastMessage/title）
/// running_projects: 当前运行会话的 (工具id, 项目名) 列表——用于"面板反推"排除
pub fn resolve_and_focus(
    system: &sysinfo::System,
    pid: u32,
    hints: &JumpHints<'_>,
    running_projects: &[(String, String)],
) -> Result<FocusOutcome, String> {
```

函数体内部改动点（保持其余逻辑逐行不动）：

1. 函数开头（`let windows = all_windows();` 之前）组装键：

```rust
    let agent = hints.agent_keyword.unwrap_or_default().to_lowercase();
    let keys = title_keys(&agent, hints.title);
```

2. 原第 ① 层 marker 匹配改用 `hints.session_marker`：

```rust
        if let Some(marker) = hints.session_marker {
```

3. **在 marker 层之后、`hard_survivors` 之前插入标题匹配层**（对每个祖先的多窗口候选生效）：

```rust
        // ①′ 标题匹配：窗口标题（归一化，剥 "OC | " 前缀）≡ 会话标题键且唯一 → 锁定。
        // 命中即返回，不触发 UIA 读取（省最多 800ms）；不命中完全落回下层，零回归
        if !keys.is_empty() {
            if let Some(hwnd) = title_match_lock(cands, &agent, &keys) {
                return try_lock(hwnd);
            }
        }
```

4. 原 ② 层的 `let agent = agent_keyword.unwrap_or_default().to_lowercase();` 删除（已上移到函数开头）。
5. ⑤ 选择器打分里 `project_name` 改 `hints.project_name`；`uia_prefix`/`last_message` 相关改用 `hints.last_message`。
6. 每处原 `agent_keyword`/`project_name`/`last_message`/`session_marker` 引用改为 `hints.xxx` 字段访问。

兼容入口 `focus_window_for_pid`（`win32.rs:540`）同步改：

```rust
pub fn focus_window_for_pid(pid: u32) -> Result<(), String> {
    let system = sysinfo::System::new_all();
    let hints = JumpHints {
        session_marker: None,
        agent_keyword: None,
        project_name: None,
        last_message: None,
        title: None,
    };
    match resolve_and_focus(&system, pid, &hints, &[]) {
        Ok(FocusOutcome::Focused) => Ok(()),
        Ok(FocusOutcome::Ambiguous(_)) => Err("存在多个候选窗口，请重试以打开选择器".to_string()),
        Err(e) => Err(e),
    }
}
```

- [ ] **Step 4: 运行全量 Rust 测试确认通过**

```bash
cd src-tauri && cargo test
```

Expected: 全部 PASS（编译错误集中修：`resolve_and_focus` 调用点共 2 处——`commands/session.rs:119`、`win32.rs::focus_window_for_pid`。Task 3 才改 commands，此步先用 `JumpHints` 空参占位让 `focus_window_for_pid` 编译通过，`commands/session.rs` 会编译失败——因此本步只运行 `cargo test --lib window` 验证 win32 模块自身；全量留到 Task 3）。

实际执行：`cargo test -p multi-agents-manager --lib window::` 或直接 `cargo check` 确认 win32.rs 自身无错即可，commands 的编译错误在 Task 3 修复。

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/window/win32.rs
git commit -m "feat(jump): wire title-match layer into resolve_and_focus via JumpHints"
```

---

### Task 3: commands/session.rs — IPC 契约加 title 并透传

**Files:**
- Modify: `src-tauri/src/commands/session.rs:57-127`

- [ ] **Step 1: 修改 `focus_session` 签名与调用**

签名加 `title`（放在 `last_message` 后，与前端字段顺序无关）：

```rust
pub fn focus_session(
    app: tauri::AppHandle,
    pid: u32,
    session_id: Option<String>,
    agent_type: Option<String>,
    project_name: Option<String>,
    last_message: Option<String>,
    title: Option<String>,
    form: Option<String>,
    unread: Option<bool>,
) -> Result<serde_json::Value, String> {
```

组装 `JumpHints`（替换原 `resolve_and_focus` 的 7 参调用）：

```rust
        let hints = crate::window::win32::JumpHints {
            session_marker: marker.as_deref(),
            agent_keyword: agent_type.as_deref(),
            project_name: project_name.as_deref(),
            last_message: last_message.as_deref(),
            title: title.as_deref(),
        };
        match crate::window::win32::resolve_and_focus(&system, pid, &hints, &running_projects) {
```

注意：`marker` 的构造（`session.rs:113-115`）保持不动。

- [ ] **Step 2: 编译验证**

```bash
cd src-tauri && cargo check
```

Expected: 无错误（此时前端还没传 title，运行时该参数为 None，行为与现状一致）。

- [ ] **Step 3: 运行 Rust 全量测试**

```bash
cd src-tauri && cargo test && cargo clippy
```

Expected: 全 PASS、clippy 无告警。

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/commands/session.rs
git commit -m "feat(ipc): pass session title through focus_session for title-match jump"
```

---

### Task 4: codex id 前缀 8→12 位

**Files:**
- Modify: `src-tauri/src/monitor/codex_parser.rs:399`（截断处）与 `:523-532`（title_tests）

背景：codex 用 UUIDv7，前 8 hex 字符只编码 65.5 秒粒度，同一分钟启动的会话卡片标题撞车（实测 `01a08083-5ca0-…` 与 `01a08083-2260-…` 同分钟双开）。12 hex 字符 ≈ 4.7 天粒度 + 随机位，实际不撞车。claude 是 UUIDv4（随机），8 位不撞车，**不改**（保持与 hook marker `MAM:<id8>` 的口径一致）。

- [ ] **Step 1: 写失败测试**

在 `codex_parser.rs` 的 `mod title_tests` 内追加：

```rust
    /// 同分钟启动的两个 UUIDv7 会话（前 8 位相同）必须靠更长前缀区分
    #[test]
    fn same_minute_uuid7_sessions_get_distinct_titles() {
        let tmp = tempfile::tempdir().unwrap();
        let mk = |name: &str, id: &str| {
            let p = tmp.path().join(name);
            std::fs::write(
                &p,
                format!(
                    r#"{{"timestamp":"2026-01-01T00:00:00Z","type":"session_meta","payload":{{"id":"{}","cwd":"/work/demo"}}}}"#,
                    id
                ),
            )
            .unwrap();
            p
        };
        let a = mk(
            "a.jsonl",
            "01a08083-5ca0-74c2-97bf-6dfdd149fac5",
        );
        let b = mk(
            "b.jsonl",
            "01a08083-2260-7462-beff-2212cffec36d",
        );
        let sa = parse_codex_jsonl(&a, ProcessForm::Cli).unwrap();
        let sb = parse_codex_jsonl(&b, ProcessForm::Cli).unwrap();
        assert_ne!(sa.title, sb.title, "同分钟双开的 codex 卡片标题不得相同");
        assert_eq!(sa.title.as_deref(), Some("01a080835ca0"));
    }
```

注意：测试用文件名不同（mtime 相同也可能），`parse_codex_jsonl` 是单文件解析、与 mtime 无关，直接可测。

- [ ] **Step 2: 运行测试确认失败**

```bash
cd src-tauri && cargo test same_minute_uuid7
```

Expected: FAIL——`sa.title == sb.title`（都是 `01a08083`）。

- [ ] **Step 3: 修改截断长度**

`codex_parser.rs:399`：

```rust
    // 卡片前缀统一 12 位：UUIDv7 前 8 hex 只编码 65.5s 粒度，同分钟启动的会话撞车
    // （实测 01a08083-5ca0 与 01a08083-2260）；12 位编码到 ~4.7 天粒度 + 随机位，实际不撞。
    // 注意与 hook marker（MAM:<id 前 8 位>）口径解耦：marker 通道尚未启用，未来启用时
    // 应同步改为 12 位（issue 见 marker 复活提案）
    let codex_title = session_id.chars().take(12).collect::<String>();
```

- [ ] **Step 4: 运行测试确认通过 + 修既有断言**

```bash
cd src-tauri && cargo test codex
```

Expected: 新测试 PASS。既有 `multibyte_session_id_title_does_not_panic` 断言 `Some("会话🔥x")`——该 id 只有 4 个字符，`take(12)` 后仍为 `"会话🔥x"`，断言**不需要改**，应保持 PASS。

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/monitor/codex_parser.rs
git commit -m "fix(codex): widen session title prefix to 12 chars to avoid same-minute collision"
```

---

### Task 5: 前端 — JumpTarget 加 title 并全线透传

**Files:**
- Modify: `src/hooks/useSessionJump.ts`
- Modify: `src/components/sessions/SessionCard.tsx:65-71`
- Modify: `src/components/pet/FoxbellPet.tsx:355-363`
- Modify: `src/components/notifications/NotificationBell.tsx:89-98` + `src/lib/notificationHistory.ts` + `src/hooks/useNotification.ts:149-158`
- Modify: `src/pages/notification.tsx:16-28,80-86`
- Modify: `src/pages/settings.tsx:589-601`

- [ ] **Step 1: `useSessionJump.ts` 加字段**

`JumpTarget` 接口（`useSessionJump.ts:15-25`）加一行：

```typescript
export interface JumpTarget {
  pid: number;
  id: string;
  agentType: string;
  projectName: string;
  lastMessage?: string;
  // 会话标题（kimi/opencode 终端窗口标题同源）：标题匹配层（②′）的正向身份键
  title?: string;
  unread?: boolean;
  form?: "cli" | "app";
}
```

`focus()` 的 invoke 参数（`useSessionJump.ts:36-45`）加一行 `title: target.title,`：

```typescript
    const result = await invoke<{ type: string; via?: string; windows?: JumpWindowCandidate[] }>(
      "focus_session",
      {
        pid: target.pid,
        sessionId: target.id,
        agentType: target.agentType,
        projectName: target.projectName,
        lastMessage: target.lastMessage,
        title: target.title,
        form: target.form,
        unread: target.unread,
      }
    );
```

- [ ] **Step 2: SessionCard 补传**

`SessionCard.tsx:65-71` 的 `focus({...})` 里 `lastMessage` 行后加：

```typescript
        title: session.title ?? undefined,
```

- [ ] **Step 3: FoxbellPet 补传**

`FoxbellPet.tsx:355-363`（`jump` 函数内 `sessionJumpFocus({...})`）同样在 `lastMessage` 行后加：

```typescript
        title: s.title ?? undefined,
```

（`s` 来自 `sessionIndexRef`，其值是 `Session` 类型，`title` 字段已存在。）

- [ ] **Step 4: 通知浮窗链路补 title（三处）**

4a. `useNotification.ts:149-158` 的 `addHistory({...})` 加一行（`session.title` 在作用域内可用）：

```typescript
        addHistory({
          agentType: session.agentType,
          form: session.form,
          projectName: session.projectName,
          status: session.status,
          lastMessage: session.lastMessage ?? "",
          title: session.title ?? undefined,
          pid: session.pid,
          sessionId: session.id,
          at: Date.now(),
        });
```

4b. `useNotification.ts:172-182` 的 `show_notification_window` payload 加一行：

```typescript
                title: session.title ?? "",
```

4c. `src-tauri/src/commands/notification.rs:9-18` 的 `NotificationPayload` 加字段（serde camelCase 自动映射）：

```rust
pub struct NotificationPayload {
    pub agent_type: String,
    pub agent_label: String,
    pub project_name: String,
    pub status_color: String,
    pub status: String,
    pub last_message: String,
    /// 会话标题：通知卡跳转走标题匹配层用（kimi/opencode）
    pub title: String,
    pub pid: u32,
    pub session_id: String,
}
```

4d. `src/pages/settings.tsx:589-601` 测试通知 payload 加一行（`title` 成为必填字段后必须补，否则 settings 测试浮窗报错）：

```typescript
                            title: "",
```

4e. `src/lib/notificationHistory.ts:2-12` 的 `HistoryEntry` 加字段：

```typescript
export interface HistoryEntry {
  agentType: string;
  form?: string;
  projectName: string;
  status: string;
  lastMessage: string;
  title?: string;
  pid: number;
  sessionId: string;
  at: number;
  read: boolean;
}
```

（`addHistory`/`getHistory` 是整体对象存取（`Omit<HistoryEntry,"read">`），字段加进接口即自动随对象存取，函数体无需改。）

4f. `src/pages/notification.tsx:16-28` 的 `NotificationPayload` 接口加 `title: string;`；`jump()`（`notification.tsx:78-86`）的 `focus({...})` 加一行：

```typescript
        title: payload.title || undefined,
```

4g. `NotificationBell.tsx:89-98` 的 `jumpTo` 的 `focus({...})` 加一行：

```typescript
        title: e.title,
```

- [ ] **Step 5: 类型检查与 lint**

```bash
pnpm check
```

Expected: TypeScript 编译、ESLint、Prettier 全通过。若 `pnpm format:check` 报格式问题，先 `pnpm format` 再提交。

- [ ] **Step 6: Commit**

```bash
git add src/hooks/useSessionJump.ts src/components/sessions/SessionCard.tsx src/components/pet/FoxbellPet.tsx src/components/notifications/NotificationBell.tsx src/pages/notification.tsx src/pages/settings.tsx src/lib/notificationHistory.ts src/hooks/useNotification.ts src-tauri/src/commands/notification.rs
git commit -m "feat(jump): propagate session title across all jump entry points for title-match"
```

---

### Task 6: 全量验证 + 真机验收

**Files:** 无新改动，验证任务。

- [ ] **Step 1: 后端全量**

```bash
cd src-tauri && cargo test && cargo clippy
```

Expected: 全 PASS，无新告警。

- [ ] **Step 2: 前端全量**

```bash
pnpm check
```

Expected: 通过。

- [ ] **Step 3: 真机验收（用户场景复现）**

前置：kimi 双开（两个不同项目目录）+ opencode 双开，均在 WindowsTerminal（单进程多窗口）下。

1. `pnpm tauri:dev` 启动应用。
2. 点击 kimi 会话 A 卡片的跳转按钮 → 预期：**不弹选择器**，对应的 WT 窗口置前。
3. 点击 kimi 会话 B 卡片 → 同上，窗口 B 置前。
4. 点击 opencode 会话 A/B 卡片 → 同上，各自 `OC | …` 窗口置前。
5. 反向校验零回归：单窗口工具（如 codex 单开）跳转仍直达；claude 会话跳转行为不变。
6. 验收 kimi 卡片标题显示：卡片头部应显示 `state.json.title` 的可读文本（title 生产端未动，应与 v0.3.0 一致）。

- [ ] **Step 4: 收尾**

全部通过后按仓库惯例推送分支并开 PR（或按用户指示合并）。commit 信息用英文（AGENTS.md 允许）。

---

## Self-Review 记录

- **Spec 覆盖**：kimi/opencode 标题匹配（Task 1-3、5）；codex id 撞车（Task 4）；通知浮窗/铃铛/宠物/看板全部跳转入口透传（Task 5）；settings 测试通知的必填字段补偿（Task 5 Step 4d）。macOS 链路不受影响（改动全部在 Windows cfg 内或前端透传）。
- **占位符扫描**：Task 2 Step 1 初稿测试含占位断言，已在该步内标注"按确定性版本实现"并给出完整替换代码；无其他 TBD/TODO。
- **类型一致性**：`JumpHints<'a>`（win32.rs）字段名与 Task 3 组装处一致；前端 `title?: string` 与 IPC `title: Option<String>` 对应（undefined → None）；`NotificationPayload.title: String` 必填与 useNotification/settings 两处构造点均补齐；`HistoryEntry.title?: string` 可选向后兼容旧 localStorage 数据。
- **已知取舍**：`title_keys` 白名单只接 kimi/opencode（已实证）；codex/claude/openclaw/workbuddy 显式返回空键，本层对其 no-op。codex 同项目双开与 marker 复活绑定，另行提 issue。