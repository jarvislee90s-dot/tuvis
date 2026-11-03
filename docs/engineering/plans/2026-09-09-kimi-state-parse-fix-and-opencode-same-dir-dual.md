# kimi state.json 解析修复（title 兜底 "session_"）+ opencode 同目录双开遮蔽 修复计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 修复两个会话检测层缺陷，使 `feat/window-title-match-jump` 的标题匹配跳转真正生效：
1. **kimi**：`state.json` 的 `updatedAt` 实为 **int 毫秒时间戳**，而 `KimiState` 声明为 `Option<String>`——serde 类型不匹配导致**整个 state.json 反序列化失败**，`title` 兜底成 `"session_"`（kimi 会话 id 全部以 `session_` 开头，前 8 字符恰好是它）。标题匹配层的 key 恒为 `"session_"`，双开跳转永远落回选择器。
2. **opencode**：两个终端在**同一目录**双开时，主匹配对每个进程 `find()` 只取 `time_updated` 最新的**同一条**会话行，另一个会话永远不上报（面板只见一个会话）。

**Architecture:** 纯后端修复，改动集中在 `monitor/` 两个解析器。kimi 侧把 `updated_at` 改为 `serde_json::Value` 双形态容忍（int→`ms_to_iso`、ISO 字符串→原样）；opencode 侧照搬 `kimi_parser` Phase 1 已验证的"已匹配行集合"模式，同目录多会话与多进程按 `time_updated DESC` 一一配对，并补 DB 路径注入缝隙使主匹配可单测。前端与 `win32.rs` 跳转链路**零改动**——数据修好后既有标题匹配层自然生效。

**Tech Stack:** Rust（rusqlite、serde、tempfile）、cargo test 单测。

**背景实测证据（2026-09-09 本机探测，勿在执行时重测）：**

- kimi 全部 8 个近期会话的 `state.json` 中 `updatedAt` 均为 int（如 `1788856769331`），`title` 均为 str；`KimiState` 自 `2080bb8`（09-01）引入起就声明 `Option<String>`。
- 现有测试夹具（`kimi_parser.rs::fixture_session`）写的 state.json 用 ISO 字符串 `"updatedAt":"2026-08-01T00:01:00.000Z"`——**夹具编码了错误假设，测试全绿掩盖了生产全挂**。
- 实测跳转失败现场：面板 kimi 卡 title=`"session_"`（OCR 截图 "SSS" 乱码佐证），选择器候选 `[（1）kimi窗, （0）kimi窗, cmd]` 与 L1/L2 幸存者完全吻合。
- opencode.db 两个活跃会话同目录 `C:/Users/bunny/Desktop/苏州市农业发展集团有限公司`，标题 `公司股东查询` / `查看公司2025年末总资产`；`get_opencode_sessions` 主匹配（`opencode_parser.rs:88-108`）对两个同 cwd 进程各 `find()` 到同一条最新行。
- 该目录匹配逻辑 `1eec409`（08-24）引入，非 `feat/window-title-match-jump` 分支引入（该分支未改 opencode_parser）。
- kimi_parser Phase 1（`kimi_parser.rs:183-205`）已有正确的防重用实现（`matched: HashSet<usize>`），可直接借鉴。

---

## File Structure

- Modify: `src-tauri/src/monitor/kimi_parser.rs` — `KimiState.updated_at` 改型 + 兜底转换 + 双形态测试 + 夹具改真实格式
- Modify: `src-tauri/src/monitor/opencode_parser.rs` — DB 路径注入缝隙 + 主匹配防重用 + 临时 DB 测试

## 关键设计决定（执行者必读）

1. **`updated_at` 用 `Option<serde_json::Value>` 双形态容忍，而非 `Option<i64>`**：测试夹具里的 ISO 字符串形态说明旧版本 kimi CLI（或写夹具时的观察）可能真写过字符串；kimi CLI 版本演进不可控，双形态取值（Number→`ms_to_iso`、String→原样、其他→None）一次性消灭这一类事故。**不**做整体 `Value`-dig 防御性重构（最小改动；`title` 实测恒为 str）。
2. **`updated_at` 只在 `last_activity_at` 兜底消费**（wire 的 `last_ts` 优先），改型影响面仅一行消费点 + 一个辅助函数；`title` 提取路径不感知此字段。
3. **opencode 主匹配防重用照搬 kimi_parser Phase 1**：`recent` 已按 `time_updated DESC` 排序，同目录最新会话配第一个进程、次新配第二个——产出两张卡、session id 各异。
4. **卡片↔终端配对可能互换（已知局限，接受）**：进程侧没有会话级身份，同目录双开时哪张卡对应哪个终端无法确知。**跳转不受影响**：两个 opencode pid 共享同一 WT 祖先进程 → 同一候选窗口池 → 标题匹配层按窗口标题锁定，与卡片绑定的 pid 无关（实测模拟验证：`OC | 公司股东查询` / `OC | 查看公司2025年末总资产` 各自唯一锁定）。卡片 status/lastMessage 可能互串，彻底解决需 hook marker 复活（已有独立提案，本期不做）。
5. **DB 注入缝隙用参数注入（`_with_db`）而非环境变量**：kimi 侧 env 注入需要 `HOME_LOCK` 串行化互斥，参数注入无此负担且更明确。公开入口 `get_opencode_sessions` 签名不变（adapter 调用点零改动）。
6. **测试夹具 DB 用最小 schema**：仅建 4 张表、仅含查询实际引用的列（见 Task 4 Step 1 的 DDL）；`message`/`part` 允许为空（build_session_from_row 对空表查询自然得到 None，不报错）。

---

### Task 1: kimi — 失败测试（真实 int 格式 state.json）

**Files:**
- Modify: `src-tauri/src/monitor/kimi_parser.rs`（`mod tests`）

- [ ] **Step 1: 写失败测试**

在 `mod tests` 内追加（用 2026-09-09 真实事故数据）：

```rust
    /// 2026-09-09 事故回归锁：真实 kimi CLI 的 state.json updatedAt 是 int 毫秒时间戳，
    /// 曾因声明 Option<String> 整体反序列化失败 → title 兜底 "session_" → 双开跳转失效
    #[test]
    fn parse_kimi_session_reads_title_with_int_updated_at() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("session_44554114-366e-4c61-9a57-733a3f3b79d0");
        fs::create_dir_all(session_dir.join("agents").join("main")).unwrap();
        // 真实形态：updatedAt 为 int（ms 时间戳）
        fs::write(
            session_dir.join("state.json"),
            r#"{"id":"session_44554114","version":2,"cwd":"C:/x","createdAt":1788851359520,"updatedAt":1788851359520,"archived":false,"title":"（0）使用技能【ratingdog-report】，下载【贵州铁路投资集团有限责任公司】的 YY评级报告"}"#,
        )
        .unwrap();
        fs::write(
            session_dir.join("agents").join("main").join("wire.jsonl"),
            r#"{"type":"turn.prompt","input":[{"type":"text","text":"hi"}],"time":1788851359000}"#,
        )
        .unwrap();
        let entry = IndexedSession {
            session_id: "session_44554114-366e-4c61-9a57-733a3f3b79d0".to_string(),
            work_dir: "C:/Users/bunny/Desktop/贵州铁路投资集团有限责任公司".to_string(),
            session_dir: session_dir.clone(),
            wire_mtime: SystemTime::now(),
        };
        let session = parse_kimi_session(&entry, &fake_process(1, "C:/x")).unwrap();
        // 修复前红：title 兜底 "session_"（id 前 8 字符）
        assert_eq!(
            session.title.as_deref(),
            Some("（0）使用技能【ratingdog-report】，下载【贵州铁路投资集团有限责任公司】的 YY评级报告")
        );
    }

    /// updatedAt 双形态：ISO 字符串（旧版本/夹具假设形态）必须继续可用
    #[test]
    fn parse_kimi_session_tolerates_string_updated_at() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("session_22222222-2222-2222-2222-222222222222");
        fs::create_dir_all(session_dir.join("agents").join("main")).unwrap();
        fs::write(
            session_dir.join("state.json"),
            r#"{"title":"Legacy String","updatedAt":"2026-08-01T00:01:00.000Z"}"#,
        )
        .unwrap();
        fs::write(
            session_dir.join("agents").join("main").join("wire.jsonl"),
            r#"{"type":"turn.prompt","input":[{"type":"text","text":"hi"}],"time":1782300900000}"#,
        )
        .unwrap();
        let entry = IndexedSession {
            session_id: "session_22222222-2222-2222-2222-222222222222".to_string(),
            work_dir: "/work".to_string(),
            session_dir,
            wire_mtime: SystemTime::now(),
        };
        let session = parse_kimi_session(&entry, &fake_process(1, "/work")).unwrap();
        assert_eq!(session.title.as_deref(), Some("Legacy String"));
    }
```

注意：`IndexedSession` 与 `parse_kimi_session` 为模块私有，`mod tests` 同文件内可直接构造（现有测试同模式）。

- [ ] **Step 2: 运行确认失败**

```bash
cd src-tauri && cargo test parse_kimi_session_reads_title_with_int
```

Expected: FAIL——`title` 实际为 `Some("session_")`。

### Task 2: kimi — `KimiState.updated_at` 改型 + 兜底转换

**Files:**
- Modify: `src-tauri/src/monitor/kimi_parser.rs`

- [ ] **Step 1: 改结构体与消费点**

```rust
#[derive(Deserialize)]
struct KimiState {
    title: Option<String>,
    /// 实测 kimi CLI 写 int 毫秒时间戳（2026-09 样本全量）；旧版本可能为 ISO 字符串。
    /// 曾声明 Option<String>：int 形态类型不匹配 → 整个 state.json 反序列化失败 →
    /// title 兜底 "session_"（kimi id 统一前缀），标题匹配跳转因此失效（2026-09-09 事故）
    #[serde(rename = "updatedAt")]
    updated_at: Option<serde_json::Value>,
}

/// updatedAt 双形态（int 毫秒 / ISO 字符串）转显示字符串；其他形态 None
/// （仅影响 last_activity_at 兜底显示，不影响 title 提取）
fn updated_at_to_string(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::Number(n) => n.as_i64().map(ms_to_iso),
        serde_json::Value::String(s) => Some(s.clone()),
        _ => None,
    }
}
```

`last_activity_at` 消费点（`kimi_parser.rs:351-354`）改为：

```rust
    let last_activity_at = last_ts
        .map(ms_to_iso)
        .or_else(|| {
            state
                .as_ref()
                .and_then(|s| s.updated_at.as_ref())
                .and_then(updated_at_to_string)
        })
        .unwrap_or_else(|| "Unknown".to_string());
```

- [ ] **Step 2: 夹具改真实格式**

`fixture_session`（`kimi_parser.rs:530-533`）的 state.json 改为 int 形态：

```rust
        fs::write(
            session_dir.join("state.json"),
            r#"{"title":"Demo Session","createdAt":1754000000000,"updatedAt":1754000060000}"#,
        )
        .unwrap();
```

同时检查既有断言：若现有测试断言 `last_activity_at == "2026-08-01T00:01:00.000Z"`（字符串兜底路径），改为 `ms_to_iso(1754000060000)` 的期望值（wire 无 time 的用例才走兜底；有 wire time 的用例期望值不变）。

- [ ] **Step 3: 运行确认通过 + 全量**

```bash
cd src-tauri && cargo test kimi && cargo test
```

Expected: 新增 2 测试 PASS；夹具改动后既有测试全 PASS（如有断言按 Step 2 说明同步修正）。

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/monitor/kimi_parser.rs
git commit -m "fix(kimi): tolerate int-millis updatedAt in state.json so session title is read"
```

---

### Task 3: opencode — DB 路径注入缝隙

**Files:**
- Modify: `src-tauri/src/monitor/opencode_parser.rs`

- [ ] **Step 1: 拆注入函数**

`get_opencode_sessions` 主体移入 `get_opencode_sessions_with_db(db_path: &Path, processes: &[AgentProcess]) -> Vec<Session>`（逻辑逐行不动，仅 `db_path` 由参数传入）；原函数变薄封装：

```rust
/// 获取 OpenCode 会话（生产入口：DB 固定在 ~/.local/share/opencode/opencode.db）
pub fn get_opencode_sessions(processes: &[AgentProcess]) -> Vec<Session> {
    let Some(h) = dirs::home_dir() else { return Vec::new() };
    get_opencode_sessions_with_db(
        &h.join(".local").join("share").join("opencode").join("opencode.db"),
        processes,
    )
}

/// DB 路径注入版（单测用）：与生产入口同逻辑
fn get_opencode_sessions_with_db(db_path: &Path, processes: &[AgentProcess]) -> Vec<Session> { … }
```

（`use std::path::Path;` 按需补充。`if processes.is_empty()` 早退保留在注入函数内。）

- [ ] **Step 2: 编译验证**

```bash
cd src-tauri && cargo check
```

### Task 4: opencode — 失败测试（同目录双开）

**Files:**
- Modify: `src-tauri/src/monitor/opencode_parser.rs`（新增 `mod matching_tests`）

- [ ] **Step 1: 写失败测试**

新增测试模块与临时 DB 夹具（schema 仅含解析器 4 条 SQL 实际引用的列）：

```rust
#[cfg(test)]
mod matching_tests {
    use super::*;
    use crate::session::ProcessForm;
    use std::path::Path;

    fn fake_process(pid: u32, cwd: &str) -> AgentProcess {
        AgentProcess {
            pid,
            cpu_usage: 0.0,
            cwd: Some(std::path::PathBuf::from(cwd)),
            form: ProcessForm::Cli,
            exe: None,
        }
    }

    /// 最小 schema 夹具 DB：仅建解析器 SQL 引用的表/列；message/part 允许为空
    fn fixture_db(path: &Path, sessions: &[(&str, &str, &str, i64)]) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE session (id TEXT PRIMARY KEY, project_id TEXT, directory TEXT, title TEXT, time_updated INTEGER);
             CREATE TABLE project (id TEXT PRIMARY KEY, worktree TEXT, name TEXT);
             CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, data TEXT);
             CREATE TABLE part (message_id TEXT, data TEXT, time_created INTEGER);",
        )
        .unwrap();
        for (id, dir, title, ts) in sessions {
            conn.execute(
                "INSERT INTO session (id, project_id, directory, title, time_updated) VALUES (?1,'p1',?2,?3,?4)",
                rusqlite::params![id, dir, title, ts],
            )
            .unwrap();
        }
    }

    /// 2026-09-09 事故回归锁：同目录双开两会话，两进程必须各得一张卡（id 各异）。
    /// 修复前红：两进程 find() 命中同一条最新行，另一会话被遮蔽
    #[test]
    fn same_dir_dual_sessions_both_surfaced() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("opencode.db");
        fixture_db(
            &db,
            &[
                ("ses_new", "C:/Users/x/Desktop/苏州", "公司股东查询", 2000),
                ("ses_old", "C:/Users/x/Desktop/苏州", "查看公司2025年末总资产", 1000),
            ],
        );
        let procs = vec![
            fake_process(11, "C:\\Users\\x\\Desktop\\苏州"),
            fake_process(22, "C:\\Users\\x\\Desktop\\苏州"),
        ];
        let sessions = get_opencode_sessions_with_db(&db, &procs);
        assert_eq!(sessions.len(), 2, "两个终端两张卡");
        let ids: HashSet<&str> = sessions.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["ses_new", "ses_old"].into_iter().collect(), "两会话都出现，不再互相遮蔽");
    }

    /// 回归：不同目录双开各配各的（既有行为不回退）
    #[test]
    fn distinct_dirs_each_matched() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("opencode.db");
        fixture_db(
            &db,
            &[
                ("ses_a", "C:/Users/x/A", "会话A", 2000),
                ("ses_b", "C:/Users/x/B", "会话B", 1000),
            ],
        );
        let procs = vec![
            fake_process(11, "C:\\Users\\x\\A"),
            fake_process(22, "C:\\Users\\x\\B"),
        ];
        let sessions = get_opencode_sessions_with_db(&db, &procs);
        assert_eq!(sessions.len(), 2);
        assert!(sessions.iter().any(|s| s.id == "ses_a"));
        assert!(sessions.iter().any(|s| s.id == "ses_b"));
    }

    /// 回归：单进程 + 同目录多会话 → 取最新（配对顺序语义）
    #[test]
    fn single_process_gets_newest_same_dir_session() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("opencode.db");
        fixture_db(
            &db,
            &[
                ("ses_new", "C:/Users/x/Desktop/苏州", "公司股东查询", 2000),
                ("ses_old", "C:/Users/x/Desktop/苏州", "查看公司2025年末总资产", 1000),
            ],
        );
        let sessions = get_opencode_sessions_with_db(&db, &[fake_process(11, "C:\\Users\\x\\Desktop\\苏州")]);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "ses_new");
    }
}
```

注意：`same_dir_dual_sessions_both_surfaced` 在修复前的实际行为是**两进程产出同一 `ses_new`**（两张同 id 卡），`ids` 集合断言红。

- [ ] **Step 2: 运行确认失败**

```bash
cd src-tauri && cargo test same_dir_dual
```

Expected: FAIL——ids 集合为 `{"ses_new"}`（`ses_old` 被遮蔽）。

### Task 5: opencode — 主匹配防重用（照搬 kimi Phase 1 模式）

**Files:**
- Modify: `src-tauri/src/monitor/opencode_parser.rs`（主匹配循环，现 `:82-108`）

- [ ] **Step 1: 改主匹配循环**

```rust
    let mut sessions = Vec::new();
    let mut matched_pids: HashSet<u32> = HashSet::new();
    // 已被某进程认领的 session 行下标（同目录双开防遮蔽：recent 按 time_updated DESC，
    // 最新行配第一个进程、次新行配第二个——借鉴 kimi_parser Phase 1 的 matched 集合）
    let mut matched_rows: HashSet<usize> = HashSet::new();

    for process in processes {
        let Some(cwd) = &process.cwd else { continue };
        let cwd_str = cwd.to_string_lossy();
        if let Some((row_idx, row)) = recent
            .iter()
            .enumerate()
            .find(|(i, (_, dir, _, _))| !matched_rows.contains(i) && cwd_equivalent(dir, &cwd_str))
        {
            matched_rows.insert(row_idx);
            matched_pids.insert(process.pid);
            let (session_id, directory, title, time_updated) = row;
            if let Some(session) = build_session_from_row(
                &conn,
                session_id,
                directory,
                title.as_deref(),
                None,
                *time_updated,
                process,
            ) {
                sessions.push(session);
            }
        }
    }
```

（`project` 回退匹配段不动——其 `!matched_pids.contains(&proc.pid)` 过滤语义不受影响。）

- [ ] **Step 2: 运行确认通过**

```bash
cd src-tauri && cargo test matching_tests && cargo test
```

Expected: 3 个新测试全 PASS，全量回归 PASS。

- [ ] **Step 3: Commit**

```bash
git add src-tauri/src/monitor/opencode_parser.rs
git commit -m "fix(opencode): pair same-directory sessions to separate processes instead of shadowing"
```

---

### Task 6: 全量验证 + 真机验收

- [ ] **Step 1: 后端全量**

```bash
cd src-tauri && cargo test && cargo clippy
```

Expected: 全 PASS，无新告警。（前端零改动，`pnpm check` 非必需，跑一次更稳。）

- [ ] **Step 2: 真机验收（重启加载新构建）**

前置：kimi 双开（两个不同项目目录）+ opencode 同目录双开，均在 WindowsTerminal。

1. **重启 `pnpm tauri:dev`**（旧进程持有 exe 锁，必须先退出再启动，否则新构建链接失败、跑的还是旧代码）。
2. kimi 卡片标题应显示任务文本（如 `（0）使用技能【ratingdog-report】…`），**不再是 `session_`**——此为 Bug 1 修复的最直接可见症状。
3. 点击 kimi 双开卡片跳转 → **不弹选择器**，对应窗口置前。
4. opencode 同目录双开 → 面板出现**两张**卡（`公司股东查询` / `查看公司2025年末总资产`），各自跳转锁定各自 `OC | …` 窗口。
5. 反向零回归：单开 kimi / opencode 跳转直达；claude、codex 行为不变。

- [ ] **Step 3: 收尾**

按仓库惯例推送分支（可并入 `feat/window-title-match-jump` 或续开 fix 分支，以用户指示为准）。

---

## Self-Review 记录

- **Spec 覆盖**：Bug 1（Task 1-2：int updatedAt 解析 + 双形态容忍 + 夹具真实化）；Bug 2（Task 3-5：注入缝隙 + 同目录防遮蔽 + 三向回归）。跳转链路零改动，依赖既有标题匹配层自然生效。
- **占位符扫描**：Task 3 Step 1 的 `…` 为主体平移指示（非待写逻辑）；无其他 TBD。
- **类型一致性**：`updated_at: Option<serde_json::Value>` 与 `updated_at_to_string(&Value) -> Option<String>` 对应；`matched_rows: HashSet<usize>` 与 `recent.iter().enumerate()` 下标对应（kimi 侧同模式已验证）。
- **已知取舍**：① 同目录双开卡片↔终端配对可能互换（status/lastMessage 互串），跳转不受影响（标题匹配按窗口标题锁定，与卡片 pid 无关），彻底解决绑定 hook marker 复活提案；② kimi 不做整体 Value-dig 防御性重构，`title` 字段实测恒为 str；③ 选择器混入全路径 `C:\WINDOWS\system32\cmd.exe` 空壳窗口（`IDLE_TERMINAL_TITLES` 名单只有裸名）不在本期范围，另行记 issue。
