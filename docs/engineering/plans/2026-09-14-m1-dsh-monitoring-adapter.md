# M1 · dsh 监控底座（第 8 工具）Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** dsh（DeepSeek harness）成为 MAM 第 8 个受监控工具：会话卡片（含历史会话）、三色状态、消息预览、一键跳转，接入现有通知/未读链路。

**Architecture:** 新增 `monitor/dsh/` 目录模块（解码/日志/projcache/状态/预测五个内聚文件）+ `adapter/dsh.rs` + `window/dsh_tab.rs`（跳转）。双数据源：projcache（首选）+ zstd 事件日志（兜底）。全部只读，不写 `~/.dsh` 一个字节。

**Tech Stack:** Rust（Tauri 2 侧）、`zstd` crate 0.13（已在 Cargo.lock，加直接依赖）、AppleScript（跳转）、React/TS（前端注册）。

## Global Constraints

- 需求唯一来源：`docs/engineering/specs/2026-09-12-remote-access-level1-design.md` v4 §2（P1–P5）；技术事实唯一来源：`docs/research/dsh-probe-2026-09-13-report.md`（F1–F16）+ 评审修正。**禁止猜测 dsh 行为**——实现与探测事实冲突时停下报告，不得自行臆断。
- 只读红线：不安装 dsh 插件、不调用其 HTTP API、不写 `~/.dsh` 任何文件（水位等状态全部写 MAM 自己的 `~/.mam/mam.db`）。
- 代码注释用中文；标识符用英文；commit 遵循 conventional commits（仓库有 commitlint）。
- dsh 已测版本 `0.1.5-rc.2`；未知版本/格式一律降级不 panic（版本门）。
- 每个 Task 结束必须 `cargo test`（在 `src-tauri/` 下）全绿后 commit；涉及前端的 Task 加 `pnpm build` 通过。
- 证据样本路径：`docs/research/dsh-probe-2026-09-13-evidence/sample*.sanitized.jsonl`（5 个状态样本，作黄金测试夹具）。

---

### Task 1: 注册脊梁——AgentType、adapter 骨架、前后端登记

**Files:**
- Modify: `src-tauri/src/session/model.rs:6-14`（AgentType 枚举）
- Create: `src-tauri/src/adapter/dsh.rs`
- Create: `src-tauri/src/monitor/dsh/mod.rs`（本任务先放 stub）
- Modify: `src-tauri/src/adapter/mod.rs`（mod 声明 + TOOL_IDS + adapter_by_id）
- Modify: `src-tauri/src/monitor/mod.rs`（`pub mod dsh;`）
- Modify: `src/types/session.ts:4`、`src/config/constants.ts:20 附近`、`src/lib/agentBadge.tsx:31 附近`、`src/components/common/ToolIcon.tsx:204 附近`

**Interfaces:**
- Produces: `AgentType::Dsh`（serde 序列化为 `"dsh"`）；`adapter_by_id("dsh")` 可用；`monitor::dsh::{find_dsh_processes, get_dsh_sessions}` 两个 stub 函数签名（后续 Task 实装）：
  - `pub fn find_dsh_processes(system: &sysinfo::System) -> Vec<AgentProcess>`
  - `pub fn get_dsh_sessions(processes: &[AgentProcess]) -> Vec<Session>`

- [ ] **Step 1: 写失败测试（Rust 侧注册）**

在 `src-tauri/src/adapter/mod.rs` 文件末尾（现有 `mod tests` 之外或之内，跟随文件现有测试组织）加：

```rust
#[cfg(test)]
mod dsh_registration_tests {
    use super::*;

    #[test]
    fn dsh_adapter_registered() {
        assert!(adapter_by_id("dsh").is_some());
        assert_eq!(adapter_by_id("dsh").unwrap().name(), "dsh");
        // 注册表完整：8 个工具
        assert_eq!(all_adapters().len(), 8);
        assert!(TOOL_IDS.contains(&"dsh"));
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd src-tauri && cargo test dsh_adapter_registered`
Expected: FAIL（`adapter_by_id` 返回 None / `Dsh` 未定义编译错误）

- [ ] **Step 3: 最小实现**

`src-tauri/src/session/model.rs` 枚举加变体（放 `ZCode` 后）：

```rust
pub enum AgentType {
    Claude,
    Codex,
    OpenCode,
    OpenClaw,
    Kimi,
    WorkBuddy,
    ZCode,
    Dsh,
}
```

`src-tauri/src/monitor/dsh/mod.rs`（新建，本任务 stub）：

```rust
// dsh（DeepSeek harness）监控解析 — M1 只读底座
// 会话存储/事件语义全部依据 M0 探测报告（docs/research/dsh-probe-2026-09-13-report.md）
// 双数据源：storages/session_projcache（首选）+ sessions/<项目>/<会话>/session.vN.jsonl.zstd（兜底）

use crate::adapter::AgentProcess;
use crate::session::Session;

/// dsh 数据根：$DSH_HOME 覆盖（M0 F14 优先级：env > ~/.dsh），测试注入用
pub fn dsh_home() -> std::path::PathBuf {
    std::env::var("DSH_HOME")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".dsh"))
}

/// 进程发现：node 进程且 cmdline 令牌含 "dsh" 与 "web"（M0：进程名是 node，必须按 cmdline 判定）
pub fn find_dsh_processes(_system: &sysinfo::System) -> Vec<AgentProcess> {
    Vec::new() // Task 8 实装
}

/// 会话聚合：宿主进程在位时扫描全部会话目录出卡（Task 8 实装）
pub fn get_dsh_sessions(_processes: &[AgentProcess]) -> Vec<Session> {
    Vec::new()
}
```

`src-tauri/src/adapter/dsh.rs`（新建）：

```rust
// dsh（DeepSeek harness）adapter — 第 8 个受监控工具（宪法 D14，只读）
// 会话解析见 monitor::dsh（projcache + zstd 日志双源）
// 形态：本地 web 服务（进程形态 App）；MCP/Skill 管理非 M1 范围（写通道另评）

use super::*;
use crate::monitor;

pub struct DshAdapter;

impl AgentAdapter for DshAdapter {
    fn name(&self) -> &'static str {
        "dsh"
    }
    fn agent_type(&self) -> AgentType {
        AgentType::Dsh
    }
    fn process_names(&self) -> &'static [&'static str] {
        // 进程名是 node，通用名匹配不可用；发现走 find_dsh_processes 的 cmdline 判定
        //（zcode 同款形态：空切片 + 专用实现，detect 入口已防御空切片）
        &[]
    }
    fn find_processes(&self, system: &System) -> Vec<AgentProcess> {
        monitor::dsh::find_dsh_processes(system)
    }
    fn find_sessions(&self, processes: &[AgentProcess]) -> Vec<Session> {
        monitor::dsh::get_dsh_sessions(processes)
    }
    fn base_dir(&self) -> std::path::PathBuf {
        monitor::dsh::dsh_home()
    }
    // hook/MCP/plugin 均用 trait 默认值（hook_supported=false、McpFormat::Json 默认、
    // mcp_config_path=None、skill_dirs 默认空）——M1 只做监控
}
```

`src-tauri/src/adapter/mod.rs` 三处登记：模块声明区加 `pub mod dsh;`（跟随其他 `pub mod`）；`TOOL_IDS` 数组 `"zcode"` 后加 `"dsh",`；`adapter_by_id` 加分支：

```rust
        "zcode" => Some(Box::new(zcode::ZCodeAdapter)),
        "dsh" => Some(Box::new(dsh::DshAdapter)),
```

`src-tauri/src/monitor/mod.rs` 模块声明区加 `pub mod dsh;`。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd src-tauri && cargo test dsh_adapter_registered && cargo check`
Expected: PASS / 编译通过

- [ ] **Step 5: 前端登记（类型 + 徽章 + 图标）**

`src/types/session.ts:4` 的联合类型末尾加 `| "dsh"`：

```ts
export type AgentType =
  "claude" | "codex" | "opencode" | "openclaw" | "kimi" | "workbuddy" | "zcode" | "dsh";
```

`src/config/constants.ts`：找到含 `"workbuddy", "zcode"` 的工具 id 列表（约 13-21 行，`as const` 结尾），在 `"zcode",` 后加 `"dsh",`。

`src/lib/agentBadge.tsx`：`if (agentType === "zcode") return "ZCode";` 后加：

```tsx
  if (agentType === "dsh") return "DSH";
```

`src/components/common/ToolIcon.tsx`：图标表 `zcode: ZCodeIcon,` 后加（DeepSeek 品牌深蓝圆角方块 + 白色 "D"，几何重绘模式与 ZCodeIcon 同构）：

```tsx
// dsh（DeepSeek harness）— 品牌深蓝圆角方块 + 白色字母 D（本机无图标取样条件，几何近似）
function DshIcon({ size }: { size: number }) {
  return (
    <svg width={size} height={size} viewBox="0 0 24 24" fill="none">
      <rect width="24" height="24" rx="5" fill="#4D6BFE" />
      <path
        d="M8 7h4.2c2.6 0 4.3 1.7 4.3 5s-1.7 5-4.3 5H8V7zm2.3 2v6h1.8c1.4 0 2.2-1 2.2-3s-.8-3-2.2-3h-1.8z"
        fill="#fff"
      />
    </svg>
  );
}
```

并在图标映射表加 `dsh: DshIcon,`。

Run: `pnpm build`
Expected: TypeScript 编译通过（如 agentBadge/ToolIcon 有 exhaustive switch 报错，按提示补全 dsh 分支）

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/session/model.rs src-tauri/src/adapter/dsh.rs src-tauri/src/adapter/mod.rs src-tauri/src/monitor/dsh/mod.rs src-tauri/src/monitor/mod.rs src/types/session.ts src/config/constants.ts src/lib/agentBadge.tsx src/components/common/ToolIcon.tsx
git commit -m "feat(m1-dsh): 注册第 8 工具 dsh —— AgentType/adapter 骨架/前后端登记"
```

---

### Task 2: zstd 多帧解码器（torn 尾帧容错）

**Files:**
- Create: `src-tauri/src/monitor/dsh/decode.rs`
- Modify: `src-tauri/src/monitor/dsh/mod.rs`（`pub mod decode;`）
- Modify: `src-tauri/Cargo.toml`（`zstd = "0.13"` 直接依赖）

**Interfaces:**
- Produces:

```rust
pub struct DecodedFrames { pub text: String, pub torn_frames: usize }
pub fn decode_zstd_frames(bytes: &[u8]) -> Result<DecodedFrames, String>
```

- [ ] **Step 1: 写失败测试**

`src-tauri/src/monitor/dsh/decode.rs` 底部：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// 用 zstd crate 造多帧拼接文件（每批一帧，模拟 dsh 追加写入）
    fn make_frames(batches: &[&str]) -> Vec<u8> {
        let mut out = Vec::new();
        for b in batches {
            out.extend_from_slice(&zstd::stream::encode_all(b.as_bytes(), 3).unwrap());
        }
        out
    }

    #[test]
    fn decodes_all_frames() {
        let bytes = make_frames(&["line1\nline2\n", "line3\n", "line4\n"]);
        let out = decode_zstd_frames(&bytes).unwrap();
        assert_eq!(out.text, "line1\nline2\nline3\nline4\n");
        assert_eq!(out.torn_frames, 0);
    }

    #[test]
    fn tolerates_torn_tail_frame() {
        // M0 评审升级 #1：zstd 后端不写终帧，运行中会话尾帧 EOF 打断是常态
        let mut bytes = make_frames(&["line1\n", "line2\nline3\n"]);
        bytes.truncate(bytes.len() - 5); // 截断尾帧
        let out = decode_zstd_frames(&bytes).unwrap();
        assert_eq!(out.text, "line1\n");
        assert_eq!(out.torn_frames, 1);
    }

    #[test]
    fn rejects_no_magic() {
        assert!(decode_zstd_frames(b"not zstd at all").is_err());
    }

    #[test]
    fn decode_all_on_torn_data_documented_behavior() {
        // 开工实证（备忘 §A3）：记录 decode_all 整文件解压在 torn 数据上的行为
        let mut bytes = make_frames(&["a\n", "b\n"]);
        bytes.truncate(bytes.len() - 5);
        let whole = zstd::stream::decode_all(&bytes[..]);
        // 无论报错还是部分成功，我们的逐帧解码都必须给出确定结果：
        let ours = decode_zstd_frames(&bytes).unwrap();
        assert_eq!(ours.text, "a\n");
        let _ = whole; // 行为记录：当前实现返回 Err（观察于本测试运行时）
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd src-tauri && cargo test dsh::decode`
Expected: FAIL（`decode_zstd_frames` 未定义）

- [ ] **Step 3: 实现**

`src-tauri/Cargo.toml` `[dependencies]` 加 `zstd = "0.13"`。

`src-tauri/src/monitor/dsh/decode.rs`：

```rust
// dsh 会话日志解码：多 frame 拼接的 zstd 容器（每个追加批次独立一帧，M0 F1）
// torn 尾帧（EOF 打断）是设计内常态 —— 尾帧解码失败则前缀容错（评审升级 #1）

const ZSTD_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];

pub struct DecodedFrames {
    pub text: String,
    /// 被容错丢弃的帧数（诊断计数：正常 ≤1，只应是尾帧）
    pub torn_frames: usize,
}

fn frame_offsets(bytes: &[u8]) -> Vec<usize> {
    let mut offsets = Vec::new();
    let mut i = 0;
    while i + 4 <= bytes.len() {
        if bytes[i..i + 4] == ZSTD_MAGIC {
            offsets.push(i);
            i += 4;
        } else {
            i += 1;
        }
    }
    offsets
}

pub fn decode_zstd_frames(bytes: &[u8]) -> Result<DecodedFrames, String> {
    let offsets = frame_offsets(bytes);
    if offsets.is_empty() {
        return Err("dsh log: 未找到 zstd frame 魔数".into());
    }
    let mut text = String::new();
    let mut torn = 0usize;
    for (idx, &start) in offsets.iter().enumerate() {
        let end = offsets.get(idx + 1).copied().unwrap_or(bytes.len());
        match zstd::stream::decode_all(&bytes[start..end]) {
            Ok(part) => text.push_str(&String::from_utf8_lossy(&part)),
            Err(e) => {
                // 监控只读场景：任何解码失败的帧按丢失处理（尾帧 torn 常态；
                // 中间帧失败≈损坏，跳过并 warn——单帧丢失只影响增量精度，不阻塞出卡）
                torn += 1;
                log::warn!("dsh log: 第 {} 帧解码失败已跳过: {}", idx, e);
            }
        }
    }
    Ok(DecodedFrames { text, torn_frames: torn })
}
```

`src-tauri/src/monitor/dsh/mod.rs` 加 `pub mod decode;`。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd src-tauri && cargo test dsh::decode`
Expected: 4 个测试 PASS

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/monitor/dsh/decode.rs src-tauri/src/monitor/dsh/mod.rs src-tauri/Cargo.toml src-tauri/Cargo.lock
git commit -m "feat(m1-dsh): zstd 多帧解码器（torn 尾帧前缀容错）"
```

---

### Task 3: 会话日志读取层（三代际 + header + 事件解析 + 黄金夹具）

**Files:**
- Create: `src-tauri/src/monitor/dsh/log.rs`
- Create: `src-tauri/tests/fixtures/dsh/`（拷入 5 个证据样本）
- Modify: `src-tauri/src/monitor/dsh/mod.rs`（`pub mod log;`）

**Interfaces:**
- Consumes: `decode::decode_zstd_frames`
- Produces:

```rust
pub struct DshHeader { pub id: String, pub cwd: Option<String>, pub parent_session: Option<String>,
    pub origin: Option<String>, pub delegation_depth: i64, pub created_at: Option<i64>,
    pub is_seeded: Option<bool>, pub version: Option<i64> }
pub struct DshEvent { pub kind: String, pub seq: Option<i64>, pub time: Option<i64>, pub data: serde_json::Value }
pub fn is_subagent(h: &DshHeader) -> bool
pub fn generation_logs(dir: &Path) -> Vec<(i64, PathBuf)>          // (version, path) 升序
pub fn read_best_generation(dir: &Path) -> Option<GenerationRead>  // 取代际最大者
pub struct GenerationRead { pub version: i64, pub text: String, pub torn_frames: usize, pub mtime_ms: i64 }
pub fn parse_header(text: &str) -> Option<DshHeader>
pub fn parse_events(text: &str) -> Vec<DshEvent>
```

- [ ] **Step 1: 落夹具**

```bash
mkdir -p src-tauri/tests/fixtures/dsh
cp docs/research/dsh-probe-2026-09-13-evidence/sample*.sanitized.jsonl src-tauri/tests/fixtures/dsh/
ls src-tauri/tests/fixtures/dsh/   # 应有 5 个文件
```

- [ ] **Step 2: 写失败测试**

`src-tauri/src/monitor/dsh/log.rs` 底部：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> String {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/dsh").join(name);
        std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("读取夹具失败 {p:?}: {e}"))
    }

    #[test]
    fn golden_header_and_subagent_filter() {
        // 黄金样本 sample1：header 含 id/cwd；非子 Agent
        let text = fixture("sample1-completed.sanitized.jsonl");
        let h = parse_header(&text).expect("header 可解析");
        assert!(h.id.starts_with("session-"));
        assert!(h.cwd.as_deref().unwrap_or("").len() > 0);
        assert!(!is_subagent(&h));
        let events = parse_events(&text);
        assert_eq!(events.len(), 21); // M0 实测 21 事件
    }

    #[test]
    fn subagent_detected_by_origin_and_depth() {
        let mut h = DshHeader { id: "x".into(), cwd: None, parent_session: None,
            origin: None, delegation_depth: 0, created_at: None, is_seeded: None, version: None };
        assert!(!is_subagent(&h));
        h.origin = Some("subagent".into());
        assert!(is_subagent(&h));
        h.origin = None;
        h.delegation_depth = 1;
        assert!(is_subagent(&h));
    }

    #[test]
    fn picks_max_generation() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        // 未压缩 v0（旧存量）
        std::fs::write(d.join("session.jsonl"), "{\"type\":\"session\",\"version\":0,\"id\":\"a\"}\n").unwrap();
        // 压缩 v3（新）—— 用解码器测试同款编码
        let frame = zstd::stream::encode_all(
            b"{\"type\":\"session\",\"version\":3,\"id\":\"a\"}\n", 3).unwrap();
        std::fs::write(d.join("session.v3.jsonl.zstd"), &frame).unwrap();
        let read = read_best_generation(d).expect("应读到");
        assert_eq!(read.version, 3);
        let h = parse_header(&read.text).unwrap();
        assert_eq!(h.version, Some(3));
    }

    #[test]
    fn empty_dir_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_best_generation(dir.path()).is_none());
        assert!(generation_logs(dir.path()).is_empty());
    }
}
```

- [ ] **Step 3: 跑测试确认失败**

Run: `cd src-tauri && cargo test dsh::log`
Expected: FAIL（类型与函数未定义）

- [ ] **Step 4: 实现**

`src-tauri/src/monitor/dsh/log.rs`：

```rust
// dsh 会话日志读取层：三代际并存（v0/v2/v3，取代际最大者——M0 F2），
// header 必读（目录名 ~XXXX 转义不可反解 id，评审漏项 #3；header 兼得子 Agent 过滤字段）

use serde::Deserialize;
use std::path::{Path, PathBuf};

use super::decode::decode_zstd_frames;

#[derive(Debug, Clone, Deserialize)]
pub struct DshHeader {
    pub id: String,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(rename = "parentSession", default)]
    pub parent_session: Option<String>,
    #[serde(default)]
    pub origin: Option<String>,
    #[serde(rename = "delegationDepth", default)]
    pub delegation_depth: i64,
    #[serde(rename = "createdAt", default)]
    pub created_at: Option<i64>,
    #[serde(rename = "isSeeded", default)]
    pub is_seeded: Option<bool>,
    #[serde(default)]
    pub version: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DshEvent {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub seq: Option<i64>,
    #[serde(default)]
    pub time: Option<i64>,
    #[serde(default)]
    pub data: serde_json::Value,
}

/// 子 Agent 会话过滤（M0 F7：真实 home 33/76 是子 Agent，不出卡）
pub fn is_subagent(h: &DshHeader) -> bool {
    h.origin.as_deref() == Some("subagent") || h.delegation_depth > 0
}

/// 文件名 → 代际号；不识别的文件名忽略（版本门：未来 v4 只需放宽正则）
fn parse_generation(name: &str) -> Option<i64> {
    if name == "session.jsonl" || name == "session.jsonl.zstd" {
        return Some(0);
    }
    let rest = name.strip_prefix("session.v")?;
    let rest = rest.strip_suffix(".jsonl.zstd").or_else(|| rest.strip_suffix(".jsonl"))?;
    rest.parse::<i64>().ok()
}

pub fn generation_logs(dir: &Path) -> Vec<(i64, PathBuf)> {
    let mut out: Vec<(i64, PathBuf)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            parse_generation(&name).map(|v| (v, e.path()))
        })
        .collect();
    out.sort_by_key(|(v, _)| *v);
    out
}

pub struct GenerationRead {
    pub version: i64,
    pub text: String,
    pub torn_frames: usize,
    /// 日志 mtime（毫秒）：v0 兜底判定的静默时长输入
    pub mtime_ms: i64,
}

pub fn read_best_generation(dir: &Path) -> Option<GenerationRead> {
    let (version, path) = generation_logs(dir).pop()?;
    let bytes = std::fs::read(&path).ok()?;
    let mtime_ms = std::fs::metadata(&path).ok()?
        .modified().ok()?
        .duration_since(std::time::UNIX_EPOCH).ok()?
        .as_millis() as i64;
    let (text, torn_frames) = if path.extension().map(|e| e == "zstd").unwrap_or(false) {
        let d = decode_zstd_frames(&bytes).ok()?;
        (d.text, d.torn_frames)
    } else {
        (String::from_utf8_lossy(&bytes).to_string(), 0)
    };
    Some(GenerationRead { version, text, torn_frames, mtime_ms })
}

pub fn parse_header(text: &str) -> Option<DshHeader> {
    serde_json::from_str(text.lines().next()?).ok()
}

pub fn parse_events(text: &str) -> Vec<DshEvent> {
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}
```

`src-tauri/src/monitor/dsh/mod.rs` 加 `pub mod log;`。

- [ ] **Step 5: 跑测试确认通过**

Run: `cd src-tauri && cargo test dsh::log`
Expected: 4 个测试 PASS

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/monitor/dsh/log.rs src-tauri/src/monitor/dsh/mod.rs src-tauri/tests/fixtures/dsh/
git commit -m "feat(m1-dsh): 日志读取层（三代际取代际最大 + header 必读 + 子 Agent 过滤 + 黄金夹具）"
```

---

### Task 4: projcache 读取（identity 强校验）

**Files:**
- Create: `src-tauri/src/monitor/dsh/projcache.rs`
- Modify: `src-tauri/src/monitor/dsh/mod.rs`（`pub mod projcache;`）

**Interfaces:**
- Produces:

```rust
pub struct ProjcacheView { pub title: Option<String>, pub last_prompt_at: Option<i64>,
    pub open_turn: Option<bool> }   // open_turn=None 表示 turnBoundary 行缺失 ⇒ 无打开 turn
pub fn load(home: &Path, session_id: &str) -> Option<ProjcacheView>
pub fn identity_matches(identity: &serde_json::Value, header: &DshHeader, header_version: i64) -> bool
```

- [ ] **Step 1: 写失败测试**

`src-tauri/src/monitor/dsh/projcache.rs` 底部：

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitor::dsh::log::DshHeader;
    use serde_json::json;

    fn header() -> DshHeader {
        DshHeader { id: "session-1".into(), cwd: Some("/tmp/p".into()), parent_session: None,
            origin: None, delegation_depth: 0, created_at: Some(1000), is_seeded: Some(false),
            version: Some(3) }
    }

    #[test]
    fn identity_requires_format_version_match() {
        let h = header();
        // formatVersion 缺失 → 拒绝（M0 F5 陷阱：真实 home 12 条缺失）
        assert!(!identity_matches(&json!({ "createdAt": 1000, "cwd": "/tmp/p" }), &h, 3));
        // 不等 → 拒绝
        assert!(!identity_matches(&json!({ "formatVersion": 2, "createdAt": 1000, "cwd": "/tmp/p" }), &h, 3));
        // 全等 → 通过
        assert!(identity_matches(
            &json!({ "formatVersion": 3, "createdAt": 1000, "cwd": "/tmp/p", "isSeeded": false }),
            &h, 3));
        // createdAt/cwd 不一致 → 拒绝（张冠李戴防御）
        assert!(!identity_matches(
            &json!({ "formatVersion": 3, "createdAt": 9999, "cwd": "/tmp/p" }), &h, 3));
        assert!(!identity_matches(
            &json!({ "formatVersion": 3, "createdAt": 1000, "cwd": "/other" }), &h, 3));
        // isSeeded 缺省按 false（M0：isSeeded ?? false）
        assert!(identity_matches(&json!({ "formatVersion": 3, "createdAt": 1000, "cwd": "/tmp/p" }), &h, 3));
    }

    #[test]
    fn view_extracts_rows_and_missing_turn_boundary() {
        // rows.title.val / sessionListMetadata.val.lastPromptAt / turnBoundary.val.openTurnStartSeq
        let rec = json!({
            "record": { "identity": {}, "rows": {
                "title": { "ver": 1, "seq": 5, "val": "reply ok" },
                "sessionListMetadata": { "ver": 1, "seq": 6, "val": { "lastPromptAt": 123456 } },
                "turnBoundary": { "ver": 1, "seq": 6, "val": { "openTurnStartSeq": 4 } }
            }}
        });
        let v = view(&rec).unwrap();
        assert_eq!(v.title.as_deref(), Some("reply ok"));
        assert_eq!(v.last_prompt_at, Some(123456));
        assert_eq!(v.open_turn, Some(true));
        // 无 turnBoundary 行（空闲会话实测形态）⇒ open_turn=None ⇒ 视为无打开 turn
        let idle = json!({ "record": { "identity": {}, "rows": { "title": { "val": "t" } } } });
        assert_eq!(view(&idle).unwrap().open_turn, None);
    }

    #[test]
    fn load_reads_record_and_tolerates_missing() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let sess = home.join("storages/session_projcache/sessions");
        std::fs::create_dir_all(&sess).unwrap();
        std::fs::write(sess.join("session-1.json"), serde_json::to_string(&json!({
            "version": 7,
            "record": { "identity": {}, "rows": { "title": { "val": "hello" } } }
        })).unwrap()).unwrap();
        assert_eq!(load(home, "session-1").unwrap().title.as_deref(), Some("hello"));
        assert!(load(home, "session-missing").is_none());
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd src-tauri && cargo test dsh::projcache`
Expected: FAIL

- [ ] **Step 3: 实现**

`src-tauri/src/monitor/dsh/projcache.rs`：

```rust
// projcache（会话投影缓存）：$DSH_HOME/storages/session_projcache/sessions/<id>.json
// 首选数据源（标题/活跃度/运行信号已预计算，M0 F5）；identity 5 字段强校验防"张冠李戴"

use serde_json::Value;
use std::path::Path;

use super::log::DshHeader;

#[derive(Debug, Clone, Default)]
pub struct ProjcacheView {
    pub title: Option<String>,
    pub last_prompt_at: Option<i64>,
    /// None = turnBoundary 行缺失（空闲会话）⇒ 无打开 turn；Some(true)=运行中
    pub open_turn: Option<bool>,
}

/// identity 校验（M0 F5 + 评审修正 #5）：formatVersion 必须存在且 == 日志 header 版本；
/// createdAt/cwd 一致；isSeeded 缺省 false。inheritedEventCount 双方都无来源可比对（header 无此字段），跳过。
pub fn identity_matches(identity: &Value, header: &DshHeader, header_version: i64) -> bool {
    let fv = match identity.get("formatVersion").and_then(|v| v.as_i64()) {
        Some(v) => v,
        None => return false, // 缺失即拒绝（session-projection-cache 语义）
    };
    if fv != header_version {
        return false;
    }
    let created_ok = identity.get("createdAt").and_then(|v| v.as_i64())
        == header.created_at;
    let cwd_ok = identity.get("cwd").and_then(|v| v.as_str())
        == header.cwd.as_deref();
    let seeded = identity.get("isSeeded").and_then(|v| v.as_bool()).unwrap_or(false);
    let seeded_ok = seeded == header.is_seeded.unwrap_or(false);
    created_ok && cwd_ok && seeded_ok
}

/// 从 record JSON 提取视图（rows.<key>.val 动态形状）
pub fn view(record: &Value) -> Option<ProjcacheView> {
    let rows = record.get("record")?.get("rows")?;
    let row_val = |key: &str| rows.get(key).and_then(|r| r.get("val"));
    Some(ProjcacheView {
        title: row_val("title").and_then(|v| v.as_str()).map(String::from),
        last_prompt_at: row_val("sessionListMetadata")
            .and_then(|v| v.get("lastPromptAt"))
            .and_then(|v| v.as_i64()),
        open_turn: row_val("turnBoundary").map(|v| {
            v.get("openTurnStartSeq").map(|s| !s.is_null()).unwrap_or(false)
        }),
    })
}

/// 读取 projcache 记录。坏记录/缺文件 → None（降级走日志源，backup-and-skip 同语义）
pub fn load(home: &Path, session_id: &str) -> Option<ProjcacheView> {
    let path = home.join("storages/session_projcache/sessions").join(format!("{session_id}.json"));
    let text = std::fs::read_to_string(path).ok()?;
    let record: Value = serde_json::from_str(&text).ok()?;
    // domain version 门：[3,7] 之外弃缓存（M0：version 7 兼容 3–6）
    let ver = record.get("version").and_then(|v| v.as_i64())?;
    if !(3..=7).contains(&ver) {
        log::warn!("dsh projcache: 未知 version {}，弃缓存（session {}）", ver, session_id);
        return None;
    }
    view(&record)
}
```

`src-tauri/src/monitor/dsh/mod.rs` 加 `pub mod projcache;`。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd src-tauri && cargo test dsh::projcache`
Expected: 3 个测试 PASS

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/monitor/dsh/projcache.rs src-tauri/src/monitor/dsh/mod.rs
git commit -m "feat(m1-dsh): projcache 读取（identity 强校验 + turnBoundary 缺失语义）"
```

---

### Task 5: 三色状态判定（纯函数 + lock 交叉判定）

**Files:**
- Create: `src-tauri/src/monitor/dsh/status.rs`
- Modify: `src-tauri/src/monitor/dsh/mod.rs`（`pub mod status;`）

**Interfaces:**
- Consumes: `log::DshEvent`
- Produces:

```rust
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LockState { Held, Free, Unknown }
pub struct StatusInput<'a> { pub events: &'a [DshEvent], pub lock: LockState,
    pub silence_ms: Option<i64> }   // 日志 mtime 距今（仅 Unknown 兜底用）
pub struct StatusOutcome { pub status: SessionStatus, pub end_kind: Option<String>, pub last_end_seq: Option<i64> }
pub const V0_SILENCE_INTERRUPT_MS: i64 = 30 * 60 * 1000;
pub fn derive(input: &StatusInput) -> StatusOutcome
```

- [ ] **Step 1: 写失败测试**

`src-tauri/src/monitor/dsh/status.rs` 底部：

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitor::dsh::log::DshEvent;
    use crate::session::SessionStatus;
    use serde_json::json;

    fn fixture_events(name: &str) -> Vec<DshEvent> {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/dsh").join(name);
        crate::monitor::dsh::log::parse_events(&std::fs::read_to_string(&p).unwrap())
    }

    fn ev(kind: &str, seq: i64, data: serde_json::Value) -> DshEvent {
        DshEvent { kind: kind.into(), seq: Some(seq), time: None, data }
    }

    fn input(events: &[DshEvent], lock: LockState) -> StatusInput {
        StatusInput { events, lock, silence_ms: None }
    }

    #[test]
    fn golden_completed_is_finished() {
        let e = fixture_events("sample1-completed.sanitized.jsonl");
        let out = derive(&input(&e, LockState::Held));
        assert_eq!(out.status, SessionStatus::Finished);
        assert_eq!(out.end_kind.as_deref(), Some("completed"));
    }

    #[test]
    fn golden_tool_error_is_waiting() {
        let e = fixture_events("sample2-tool-error.sanitized.jsonl");
        assert_eq!(derive(&input(&e, LockState::Held)).status, SessionStatus::Waiting);
    }

    #[test]
    fn golden_approval_pending_is_waiting() {
        let e = fixture_events("sample5-approval-pending.sanitized.jsonl");
        // turn 打开 + 审批未决 → 红（优先于"运行中"）
        assert_eq!(derive(&input(&e, LockState::Held)).status, SessionStatus::Waiting);
    }

    #[test]
    fn golden_running_is_processing() {
        let e = fixture_events("sample4-running.sanitized.jsonl");
        assert_eq!(derive(&input(&e, LockState::Held)).status, SessionStatus::Processing);
    }

    #[test]
    fn open_turn_with_free_lock_is_interrupted_waiting() {
        // 评审升级 #2：turn 打开 + 写入者已死（锁无人持有）→ 红·中断
        let e = fixture_events("sample4-running.sanitized.jsonl");
        assert_eq!(derive(&input(&e, LockState::Free)).status, SessionStatus::Waiting);
    }

    #[test]
    fn open_turn_unknown_lock_uses_silence() {
        let e = fixture_events("sample4-running.sanitized.jsonl");
        // 静默 10 分钟 → 仍黄（长任务不误判）
        let short = StatusInput { events: &e, lock: LockState::Unknown, silence_ms: Some(10 * 60 * 1000) };
        assert_eq!(derive(&short).status, SessionStatus::Processing);
        // 静默 31 分钟 → 红·中断（v0 兜底阈值 30min）
        let long = StatusInput { events: &e, lock: LockState::Unknown, silence_ms: Some(31 * 60 * 1000) };
        assert_eq!(derive(&long).status, SessionStatus::Waiting);
    }

    #[test]
    fn end_kind_mapping_table() {
        let base = |kind: &str| vec![
            ev("turn/start", 4, json!({})),
            ev("turn/end", 6, json!({ "reason": { "kind": kind } })),
        ];
        // 设计 P2 定死映射：aborted→空闲（不点红）；blocked→黄；max-tokens→绿；interrupted→红
        assert_eq!(derive(&input(&base("aborted"), LockState::Held)).status, SessionStatus::Idle);
        assert_eq!(derive(&input(&base("blocked"), LockState::Held)).status, SessionStatus::Processing);
        assert_eq!(derive(&input(&base("max-tokens"), LockState::Held)).status, SessionStatus::Finished);
        assert_eq!(derive(&input(&base("interrupted"), LockState::Held)).status, SessionStatus::Waiting);
    }

    #[test]
    fn approval_open_without_decided_is_waiting() {
        // approval/asked 无同 id 的 decided → 红（turn 未闭合场景）
        let e = vec![
            ev("turn/start", 4, json!({})),
            ev("approval/asked", 5, json!({ "id": "appr-1" })),
            ev("approval/decided", 6, json!({ "id": "appr-x" })), // 决的是别的审批
        ];
        assert_eq!(derive(&input(&e, LockState::Held)).status, SessionStatus::Waiting);
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd src-tauri && cargo test dsh::status`
Expected: FAIL

- [ ] **Step 3: 实现**

`src-tauri/src/monitor/dsh/status.rs`：

```rust
// dsh 三色状态判定（设计 P2 定死映射）：
// 优先级 error > approval > running > done（foxbell 生产语义复刻，M0 §2）
// interrupted 判定 = "写入者是否存活"（lock 交叉判定，评审升级 #2），与任务时长无关

use crate::monitor::dsh::log::DshEvent;
use crate::session::SessionStatus;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LockState {
    /// 某个 dsh 进程持有该会话写租约（真运行）
    Held,
    /// 锁文件存在但无人持有（写入者已死 → 中断）
    Free,
    /// 无锁文件（v0 旧目录 / 探测失败）→ mtime 静默兜底
    Unknown,
}

/// v0 兜底：日志静默超此阈值 → 判中断（备忘 §A4）
pub const V0_SILENCE_INTERRUPT_MS: i64 = 30 * 60 * 1000;

pub struct StatusInput<'a> {
    pub events: &'a [DshEvent],
    pub lock: LockState,
    /// 日志 mtime 距今毫秒（仅 lock=Unknown 时消费）
    pub silence_ms: Option<i64>,
}

pub struct StatusOutcome {
    pub status: SessionStatus,
    /// 最近 turn/end 的 reason.kind（诊断/审计用）
    pub end_kind: Option<String>,
    /// 最近 turn/end 的 seq（水位/未读判定用）
    pub last_end_seq: Option<i64>,
}

pub fn derive(input: &StatusInput) -> StatusOutcome {
    let mut last_start: Option<i64> = None;
    let mut last_end: Option<(i64, String)> = None; // (seq, kind)
    let mut open_approvals: std::collections::HashSet<String> = std::collections::HashSet::new();

    for e in input.events {
        match e.kind.as_str() {
            "turn/start" => last_start = e.seq.or(last_start),
            "turn/end" => {
                if let Some(seq) = e.seq {
                    let kind = e.data.pointer("/reason/kind")
                        .and_then(|v| v.as_str()).unwrap_or("").to_string();
                    // 只保留最新（seq 单调）
                    if last_end.as_ref().map(|(s, _)| seq >= *s).unwrap_or(true) {
                        last_end = Some((seq, kind));
                    }
                }
            }
            "approval/asked" => {
                if let Some(id) = e.data.get("id").and_then(|v| v.as_str()) {
                    open_approvals.insert(id.to_string());
                }
            }
            "approval/decided" => {
                if let Some(id) = e.data.get("id").and_then(|v| v.as_str()) {
                    open_approvals.remove(id);
                }
            }
            _ => {}
        }
    }

    let open_turn = match (last_start, &last_end) {
        (Some(s), Some((e, _))) => s > *e,
        (Some(_), None) => true,
        _ => false,
    };

    let (status, end_kind) = if open_turn {
        if !open_approvals.is_empty() {
            (SessionStatus::Waiting, None) // 红 · 等待批准
        } else {
            match input.lock {
                LockState::Held => (SessionStatus::Processing, None),
                LockState::Free => (SessionStatus::Waiting, Some("interrupted".into())), // 写入者已死
                LockState::Unknown => {
                    let silence = input.silence_ms.unwrap_or(0);
                    if silence > V0_SILENCE_INTERRUPT_MS {
                        (SessionStatus::Waiting, Some("interrupted".into()))
                    } else {
                        (SessionStatus::Processing, None)
                    }
                }
            }
        }
    } else {
        let kind = last_end.as_ref().map(|(_, k)| k.clone());
        let st = match kind.as_deref() {
            Some("error") | Some("interrupted") => SessionStatus::Waiting,      // 红
            Some("blocked") => SessionStatus::Processing,                        // 黄 · 等待
            Some("max-tokens") | Some("completed") => SessionStatus::Finished,   // 绿
            Some("aborted") => SessionStatus::Idle,                              // 用户取消不点红
            _ => SessionStatus::Idle,
        };
        (st, kind)
    };

    StatusOutcome { status, end_kind, last_end_seq: last_end.map(|(s, _)| s) }
}
```

`src-tauri/src/monitor/dsh/mod.rs` 加 `pub mod status;`。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd src-tauri && cargo test dsh::status`
Expected: 8 个测试 PASS

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/monitor/dsh/status.rs src-tauri/src/monitor/dsh/mod.rs
git commit -m "feat(m1-dsh): 三色状态判定（lock 交叉判定中断 + P2 定死映射）"
```

---

### Task 6: 已读水位存储（dao + migration）

**Files:**
- Create: `src-tauri/src/database/dao/dsh_read.rs`
- Modify: `src-tauri/src/database/dao/mod.rs`（`pub mod dsh_read;`）
- Modify: `src-tauri/src/database/migration.rs`（建表）

**Interfaces:**
- Produces:

```rust
pub fn last_read_at(conn: &Connection, session_id: &str) -> Option<i64>
pub fn mark_read(conn: &Connection, session_id: &str, now_ms: i64) -> Result<(), String>
```

（调用方：Task 8 组装未读、Task 9 跳转标已读；连接获取方式跟随 dao 现有惯例——查 `dao/mod.rs` 里其他文件的取连接函数并在 Task 8 使用同名方式。）

- [ ] **Step 1: 写失败测试**

`src-tauri/src/database/dao/dsh_read.rs` 底部（连接构造方式抄同目录 `agent_tool.rs` 测试的现成模式——若其为内存库就用内存库，临时文件库就用 tempfile，以下按内存库写，执行时以现有模式为准对齐）：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn conn() -> rusqlite::Connection {
        let c = rusqlite::Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE IF NOT EXISTS dsh_session_read (
                 session_id TEXT PRIMARY KEY,
                 last_read_at INTEGER NOT NULL DEFAULT 0
             );",
        ).unwrap();
        c
    }

    #[test]
    fn mark_and_read_roundtrip() {
        let c = conn();
        assert_eq!(last_read_at(&c, "session-1"), None);
        mark_read(&c, "session-1", 1000).unwrap();
        assert_eq!(last_read_at(&c, "session-1"), Some(1000));
        mark_read(&c, "session-1", 2000).unwrap(); // 幂等覆盖
        assert_eq!(last_read_at(&c, "session-1"), Some(2000));
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd src-tauri && cargo test dsh_read`
Expected: FAIL

- [ ] **Step 3: 实现**

`src-tauri/src/database/dao/dsh_read.rs`：

```rust
// dsh 会话已读水位（MAM 自有库——绝不写 ~/.dsh）
// 未读判定：最近关键事件时间 > last_read_at ⇒ 未读（覆盖"刚完成"与"错误保持未读"两语义）

use rusqlite::Connection;

pub fn last_read_at(conn: &Connection, session_id: &str) -> Option<i64> {
    conn.query_row(
        "SELECT last_read_at FROM dsh_session_read WHERE session_id = ?1",
        [session_id],
        |row| row.get(0),
    )
    .ok()
}

pub fn mark_read(conn: &Connection, session_id: &str, now_ms: i64) -> Result<(), String> {
    conn.execute(
        "INSERT INTO dsh_session_read (session_id, last_read_at) VALUES (?1, ?2)
         ON CONFLICT(session_id) DO UPDATE SET last_read_at = ?2",
        rusqlite::params![session_id, now_ms],
    )
    .map_err(|e| format!("dsh mark_read 失败: {}", e))?;
    Ok(())
}
```

`migration.rs` 的 `migrate()` 末尾追加（幂等建表）：

```rust
    // M1（dsh 第 8 工具）：会话已读水位（未读判定的 MAM 侧持久化）
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS dsh_session_read (
             session_id TEXT PRIMARY KEY,
             last_read_at INTEGER NOT NULL DEFAULT 0
         );",
    )
    .map_err(|e| format!("建 dsh_session_read 失败: {}", e))?;
```

`dao/mod.rs` 加 `pub mod dsh_read;`。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd src-tauri && cargo test dsh_read && cargo test`
Expected: PASS（全量无回归）

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/database/dao/dsh_read.rs src-tauri/src/database/dao/mod.rs src-tauri/src/database/migration.rs
git commit -m "feat(m1-dsh): 已读水位存储（dao + migration 幂等建表）"
```

---

### Task 7: 标题与消息预览（注入过滤 + 工具帧回落）

**Files:**
- Create: `src-tauri/src/monitor/dsh/preview.rs`
- Modify: `src-tauri/src/monitor/dsh/mod.rs`（`pub mod preview;`）

**Interfaces:**
- Consumes: `log::DshEvent`、`projcache::ProjcacheView`
- Produces:

```rust
/// 返回 (role, text)：取真人输入与助手回复中 seq 更新的一方（设计 P3）
pub fn extract(events: &[DshEvent]) -> (Option<String>, Option<String>)
/// 标题：projcache 优先，回落 session/title 事件最新一条（M0 F6）
pub fn title(events: &[DshEvent], cache: Option<&ProjcacheView>) -> Option<String>
```

- [ ] **Step 1: 写失败测试**

`src-tauri/src/monitor/dsh/preview.rs` 底部：

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitor::dsh::log::DshEvent;
    use serde_json::json;

    fn ev(kind: &str, seq: i64, data: serde_json::Value) -> DshEvent {
        DshEvent { kind: kind.into(), seq: Some(seq), time: None, data }
    }

    #[test]
    fn filters_injected_user_messages() {
        // M0 F8：只有 source.kind=="user" 是真人输入；agent-instructions/skill-catalog 注入必须过滤
        let events = vec![
            ev("user/message", 8, json!({
                "content": [ { "type": "text", "text": "reply ok" } ],
                "source": { "kind": "user" } })),
            ev("user/message", 9, json!({
                "content": [ { "type": "text", "text": "<16913 chars AGENTS.md>" } ],
                "source": { "kind": "agent-instructions" } })),
            ev("user/message", 11, json!({
                "content": [ { "type": "text", "text": "<8894 chars skills>" } ],
                "source": { "kind": "skill-catalog" } })),
        ];
        let (role, text) = extract(&events);
        assert_eq!(role.as_deref(), Some("user"));
        assert_eq!(text.as_deref(), Some("reply ok"));
    }

    #[test]
    fn assistant_text_and_tool_fallback() {
        let mut events = vec![
            ev("user/message", 8, json!({
                "content": [ { "type": "text", "text": "run it" } ],
                "source": { "kind": "user" } })),
            ev("assistant/message", 15, json!({
                "message": { "content": [ { "type": "text", "text": "mock ok" } ] } })),
        ];
        let (role, text) = extract(&events);
        assert_eq!(role.as_deref(), Some("assistant"));
        assert_eq!(text.as_deref(), Some("mock ok"));
        // 纯 tool-call 帧 → 回落文案「执行 <工具名>」（M0 F8 边界）
        events.push(ev("assistant/message", 16, json!({
            "message": { "content": [ { "type": "tool-call", "name": "bash" } ] } })));
        let (role, text) = extract(&events);
        assert_eq!(role.as_deref(), Some("assistant"));
        assert_eq!(text.as_deref(), Some("执行 bash"));
    }

    #[test]
    fn golden_sample1_preview() {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/dsh/sample1-completed.sanitized.jsonl");
        let events = crate::monitor::dsh::log::parse_events(
            &std::fs::read_to_string(&p).unwrap());
        let (role, text) = extract(&events);
        assert_eq!(role.as_deref(), Some("assistant"));
        assert_eq!(text.as_deref(), Some("mock ok"));
        // 标题回落：session/title 事件（sanitized 样本内 fallback title 为 "reply ok"）
        let t = title(&events, None);
        assert!(t.is_some());
    }

    #[test]
    fn title_prefers_projcache() {
        let events = vec![ev("session/title", 3, json!({ "title": "from-log" }))];
        let cache = crate::monitor::dsh::projcache::ProjcacheView {
            title: Some("from-cache".into()), last_prompt_at: None, open_turn: None };
        assert_eq!(title(&events, Some(&cache)).as_deref(), Some("from-cache"));
        assert_eq!(title(&events, None).as_deref(), Some("from-log"));
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd src-tauri && cargo test dsh::preview`
Expected: FAIL

- [ ] **Step 3: 实现**

`src-tauri/src/monitor/dsh/preview.rs`：

```rust
// dsh 标题与消息预览（设计 P3）：
// 预览只取真人输入（source.kind=="user"）与助手回复；注入内容一律过滤；
// 助手纯 tool-call 帧回落「执行 <工具名>」

use crate::monitor::dsh::log::DshEvent;
use crate::monitor::dsh::projcache::ProjcacheView;

fn content_texts(value: &serde_json::Value) -> Vec<(String, String)> {
    // content[] → (type, text_or_name)
    value.as_array().map(|arr| {
        arr.iter().filter_map(|c| {
            let ty = c.get("type").and_then(|v| v.as_str()).unwrap_or("");
            let text = c.get("text").and_then(|v| v.as_str()).map(String::from);
            let name = c.get("name").and_then(|v| v.as_str()).map(String::from);
            match (ty, text, name) {
                ("text", Some(t), _) => Some((ty.to_string(), t)),
                ("tool-call", _, Some(n)) => Some((ty.to_string(), n)),
                _ => None,
            }
        }).collect()
    }).unwrap_or_default()
}

/// (role, text)：真人输入与助手回复取 seq 更新的一方
pub fn extract(events: &[DshEvent]) -> (Option<String>, Option<String>) {
    let mut last_user: Option<(i64, String)> = None;
    let mut last_assistant: Option<(i64, String)> = None;

    for e in events {
        let seq = e.seq.unwrap_or(0);
        match e.kind.as_str() {
            "user/message" => {
                // 注入过滤：只有 source.kind == "user" 是真人输入（M0 F8）
                if e.data.pointer("/source/kind").and_then(|v| v.as_str()) == Some("user") {
                    let text = content_texts(e.data.get("content").unwrap_or(&serde_json::Value::Null))
                        .into_iter().map(|(_, t)| t).collect::<Vec<_>>().join(" ");
                    if !text.is_empty() {
                        last_user = Some((seq, text));
                    }
                }
            }
            "assistant/message" => {
                let content = e.data.pointer("/message/content").unwrap_or(&serde_json::Value::Null);
                let parts = content_texts(content);
                let text = if parts.iter().any(|(ty, _)| ty == "text") {
                    parts.into_iter().filter(|(ty, _)| ty == "text")
                        .map(|(_, t)| t).collect::<Vec<_>>().join(" ")
                } else if let Some((_, name)) = parts.first() {
                    format!("执行 {}", name) // 纯工具帧回落（设计 P3）
                } else {
                    String::new()
                };
                if !text.is_empty() {
                    last_assistant = Some((seq, text));
                }
            }
            _ => {}
        }
    }

    match (last_user, last_assistant) {
        (Some(u), Some(a)) => if a.0 >= u.0 {
            (Some("assistant".into()), Some(a.1))
        } else {
            (Some("user".into()), Some(u.1))
        },
        (Some(u), None) => (Some("user".into()), Some(u.1)),
        (None, Some(a)) => (Some("assistant".into()), Some(a.1)),
        (None, None) => (None, None),
    }
}

/// 标题：projcache 优先 → session/title 最新事件（provider 生成覆盖 fallback，M0 F6）
pub fn title(events: &[DshEvent], cache: Option<&ProjcacheView>) -> Option<String> {
    if let Some(t) = cache.and_then(|c| c.title.clone()) {
        return Some(t);
    }
    events.iter()
        .filter(|e| e.kind == "session/title")
        .filter_map(|e| e.data.get("title").and_then(|v| v.as_str()).map(String::from))
        .last()
}
```

`src-tauri/src/monitor/dsh/mod.rs` 加 `pub mod preview;`。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd src-tauri && cargo test dsh::preview`
Expected: 4 个测试 PASS

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/monitor/dsh/preview.rs src-tauri/src/monitor/dsh/mod.rs
git commit -m "feat(m1-dsh): 标题与预览（注入过滤 + 工具帧回落 + projcache 优先）"
```

---

### Task 8: 进程发现与会话编排（get_dsh_sessions 实装）

**Files:**
- Modify: `src-tauri/src/monitor/dsh/mod.rs`（替换两个 stub）

**Interfaces:**
- Consumes: Task 2–7 全部（`log::*`、`projcache::*`、`status::*`、`preview::*`、`dao::dsh_read`）、`crate::database::connection`（取库连接，跟随现有 dao 调用惯例）
- Produces: 实装 `find_dsh_processes` / `get_dsh_sessions`（签名不变）

- [ ] **Step 1: 写失败测试**

在 `src-tauri/src/monitor/dsh/mod.rs` 底部：

```rust
#[cfg(test)]
mod integration_tests {
    use super::*;
    use crate::session::{SessionStatus, ProcessForm};

    /// 造一个隔离 dsh home：一个项目 + 一个会话（zstd 单帧事件）
    fn make_home(events: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let sess = dir.path().join("sessions/--tmp-proj--/session-abc");
        std::fs::create_dir_all(&sess).unwrap();
        let frame = zstd::stream::encode_all(
            format!("{\"type\":\"session\",\"version\":3,\"id\":\"session-abc\",\"cwd\":\"/tmp/proj\",\"createdAt\":1000,\"isSeeded\":false}\n{events}").as_bytes(), 3).unwrap();
        std::fs::write(sess.join("session.v3.jsonl.zstd"), &frame).unwrap();
        dir
    }

    fn fake_host() -> AgentProcess {
        AgentProcess { pid: 42, cpu_usage: 0.5, cwd: None, exe: None, form: ProcessForm::App }
    }

    #[test]
    fn emits_card_with_status_and_preview() {
        let home = make_home(
            "{\"type\":\"turn/start\",\"seq\":4,\"data\":{}}\n\
             {\"type\":\"user/message\",\"seq\":8,\"data\":{\"content\":[{\"type\":\"text\",\"text\":\"hi\"}],\"source\":{\"kind\":\"user\"}}}\n\
             {\"type\":\"assistant/message\",\"seq\":9,\"data\":{\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"done\"}]}}}\n\
             {\"type\":\"turn/end\",\"seq\":10,\"data\":{\"reason\":{\"kind\":\"completed\"}}}\n");
        let sessions = scan_sessions(home.path(), &fake_host());
        assert_eq!(sessions.len(), 1);
        let s = &sessions[0];
        assert_eq!(s.id, "session-abc");
        assert_eq!(s.agent_type, crate::session::AgentType::Dsh);
        assert_eq!(s.status, SessionStatus::Finished);
        assert_eq!(s.last_message.as_deref(), Some("done"));
        assert_eq!(s.last_message_role.as_deref(), Some("assistant"));
        assert_eq!(s.form, ProcessForm::App);
        assert_eq!(s.pid, 42);
        assert!(s.unread, "刚完成且从未读过 → 未读");
    }

    #[test]
    fn skips_subagent_and_missing_host() {
        // 子 Agent 会话不出卡
        let dir = tempfile::tempdir().unwrap();
        let sess = dir.path().join("sessions/--tmp-proj--/3b8a0933-0000-0000-0000-000000000000");
        std::fs::create_dir_all(&sess).unwrap();
        let frame = zstd::stream::encode_all(
            b"{\"type\":\"session\",\"version\":0,\"id\":\"sub-1\",\"cwd\":\"/tmp\",\"origin\":\"subagent\",\"delegationDepth\":1}\n",
            3).unwrap();
        std::fs::write(sess.join("session.jsonl.zstd"), &frame).unwrap();
        let with_host = scan_sessions(dir.path(), &fake_host());
        assert!(with_host.is_empty(), "子 Agent 过滤");
        // 宿主不在 → 无卡（与其他工具一致；不入扫描，无需 home）
        assert!(get_dsh_sessions(&[]).is_empty());
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd src-tauri && cargo test dsh::integration`
Expected: FAIL（stub 返回空）

- [ ] **Step 3: 实现（替换 mod.rs 中两个 stub）**

```rust
/// 进程发现：node 进程且 cmdline 含 "dsh" 与 "web" 令牌（M0 §5：进程名是 node，
/// 必须按 cmdline 判定；取命中的第一个作为宿主——esbuild 等子进程 cmdline 无此二令牌）
pub fn find_dsh_processes(system: &sysinfo::System) -> Vec<AgentProcess> {
    let mut out = Vec::new();
    for (pid, process) in system.processes() {
        let cmd = process.cmd();
        if cmd.is_empty() {
            continue;
        }
        let tokens: Vec<String> = cmd.iter()
            .map(|a| a.to_string_lossy().to_string()).collect();
        let has_dsh = tokens.iter().any(|t| t == "dsh" || t.ends_with("/dsh") || t.ends_with("\\dsh"));
        let has_web = tokens.iter().any(|t| t == "web");
        if has_dsh && has_web {
            out.push(AgentProcess {
                pid: pid.as_u32(),
                cpu_usage: process.cpu_usage(),
                cwd: process.cwd().map(|p| p.to_path_buf()),
                exe: process.exe().map(|p| p.to_path_buf()),
                form: ProcessForm::App,
            });
            if out.len() >= 1 {
                break; // 只需一个宿主（卡片 pid 用）
            }
        }
    }
    out
}

/// 会话锁持有探测（零干扰）：lock 文件不存在 → Unknown；有文件则问 lsof 是否有进程开着它
fn probe_lock_state(session_dir: &std::path::Path) -> LockState {
    let lock = session_dir.join("session.lock");
    if !lock.exists() {
        return LockState::Unknown; // v0 旧目录无锁文件（M0 F3）
    }
    let out = std::process::Command::new("lsof")
        .arg("-t").arg(&lock)
        .output();
    match out {
        Ok(o) if o.status.success() && !String::from_utf8_lossy(&o.stdout).trim().is_empty() =>
            LockState::Held,
        Ok(_) => LockState::Free, // 文件在但无人持有 → 写入者已死
        Err(_) => LockState::Unknown, // lsof 不可用（如 Windows）→ 静默兜底
    }
}

/// 会话聚合：宿主在位时扫描全部会话目录（含历史会话）出卡；未运行 → 无卡
pub fn get_dsh_sessions(processes: &[AgentProcess]) -> Vec<Session> {
    let Some(host) = processes.first() else {
        return Vec::new();
    };
    scan_sessions(&dsh_home(), host)
}

/// 内部扫描（home 注入，测试直调——避免 DSH_HOME 环境变量在并行测试中互踩）
fn scan_sessions(home: &std::path::Path, host: &AgentProcess) -> Vec<Session> {
    let sessions_root = home.join("sessions");
    let Ok(entries) = std::fs::read_dir(&sessions_root) else {
        return Vec::new();
    };
    let now_ms = chrono::Utc::now().timestamp_millis();
    let conn = crate::database::connection::get_connection();

    let mut sessions = Vec::new();
    for project_dir in entries.flatten() {
        let ppath = project_dir.path();
        if !ppath.is_dir() {
            continue;
        }
        let name = project_dir.file_name().to_string_lossy().to_string();
        if name.starts_with("session_projcache") || name == "workspace.json" {
            continue;
        }
        let Ok(session_dirs) = std::fs::read_dir(&ppath) else { continue };
        for sdir in session_dirs.flatten() {
            let spath = sdir.path();
            if !spath.is_dir() {
                continue;
            }
            // 版本门 + 读取（代际最大者；读不到/解不开 → 跳过该会话不影响其他）
            let Some(read) = log::read_best_generation(&spath) else { continue };
            let Some(header) = log::parse_header(&read.text) else { continue };
            if log::is_subagent(&header) {
                continue; // 子 Agent 不出卡（M0 F7）
            }
            // 版本门（设计 P5 + 备忘 A8）：header.version 超出已知集（0..=3）→
            // 降级卡"格式待适配"（未知语义不猜——探测红线），不影响其他会话
            if let Some(v) = header.version {
                if !(0..=3).contains(&v) {
                    log::warn!("dsh: 会话 {} 为未知代际 v{}，出降级卡", header.id, v);
                    sessions.push(Session {
                        id: header.id.clone(),
                        agent_type: AgentType::Dsh,
                        project_name: header.cwd.as_deref()
                            .map(crate::monitor::project::project_name_from_path)
                            .unwrap_or_else(|| name.clone()),
                        project_path: header.cwd.clone().unwrap_or_default(),
                        title: Some(format!("dsh 格式待适配（v{}）", v)),
                        git_branch: None,
                        github_url: None,
                        status: SessionStatus::Idle,
                        last_message: None,
                        last_message_role: None,
                        last_activity_at: chrono::DateTime::from_timestamp_millis(read.mtime_ms)
                            .map(|d| d.to_rfc3339()).unwrap_or_default(),
                        pid: host.pid,
                        cpu_usage: host.cpu_usage,
                        active_subagent_count: 0,
                        form: ProcessForm::App,
                        jump_supported: crate::session::jump_supported_for(ProcessForm::App),
                        unread: false,
                    });
                    continue;
                }
            }
            let events = log::parse_events(&read.text);

            // 双源：projcache（identity 过校验才可用）
            let cache = projcache::load(&home, &header.id)
                .filter(|_| projcache_identity_ok(&home, &header, read.version));

            // 状态：lock 交叉判定 + 静默兜底
            let lock = probe_lock_state(&spath);
            let silence_ms = now_ms - read.mtime_ms;
            // projcache 运行口径与日志口径一致时信任日志（审批只有日志有）
            let outcome = status::derive(&status::StatusInput {
                events: &events, lock, silence_ms: Some(silence_ms),
            });

            let (role, text) = preview::extract(&events);
            let title = preview::title(&events, cache.as_ref());
            let last_activity_ms = cache.as_ref()
                .and_then(|c| c.last_prompt_at)
                .or(events.iter().filter_map(|e| e.time).max())
                .unwrap_or(read.mtime_ms);

            // 未读（设计 P2 定死：等批准/出错/刚完成/回合被阻塞 → 未读）。
            // 锚点用事件流最大 time——勿用 lastPromptAt（完成晚于提问，
            // 用提问时刻会漏掉"读后完成"的刚完成未读）
            let unread_anchor_ms = events.iter().filter_map(|e| e.time).max()
                .unwrap_or(read.mtime_ms);
            let last_read = conn.as_ref()
                .and_then(|c| crate::database::dao::dsh_read::last_read_at(c, &header.id))
                .unwrap_or(0);
            let unread = (matches!(outcome.status, SessionStatus::Waiting | SessionStatus::Finished)
                    || outcome.end_kind.as_deref() == Some("blocked"))
                && unread_anchor_ms > last_read;

            sessions.push(Session {
                id: header.id.clone(),
                agent_type: AgentType::Dsh,
                project_name: header.cwd.as_deref()
                    .map(crate::monitor::project::project_name_from_path)
                    .unwrap_or_else(|| name.clone()),
                project_path: header.cwd.clone().unwrap_or_default(),
                title,
                git_branch: None,
                github_url: None,
                status: outcome.status,
                last_message: text,
                last_message_role: role,
                last_activity_at: chrono::DateTime::from_timestamp_millis(last_activity_ms)
                    .map(|d| d.to_rfc3339()).unwrap_or_default(),
                pid: host.pid,
                cpu_usage: host.cpu_usage,
                active_subagent_count: 0,
                form: ProcessForm::App,
                jump_supported: crate::session::jump_supported_for(ProcessForm::App),
                unread,
            });
        }
    }
    sessions
}

/// projcache identity 校验（5 字段；坏记录路径在此收敛）
fn projcache_identity_ok(home: &std::path::Path, header: &log::DshHeader, version: i64) -> bool {
    let path = home.join("storages/session_projcache/sessions")
        .join(format!("{}.json", header.id));
    let Ok(text) = std::fs::read_to_string(path) else { return false };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else { return false };
    v.get("record").and_then(|r| r.get("identity"))
        .map(|ident| projcache::identity_matches(ident, header, version))
        .unwrap_or(false)
}
```

注意两个随代码库对齐的点（执行时按现实调整，不许猜）：
1. `crate::database::connection::get_connection()` 的真实函数名/返回类型以 `src-tauri/src/database/connection.rs` 为准（dao 其他调用方的写法照抄）；
2. `crate::monitor::project::project_name_from_path` 的真实签名以 `monitor/project.rs` 为准（zcode_parser 有同款调用可抄）。`DshHeader` 字段全部 pub，统一用 `header.id` 字段直取（不引入 id() 方法）。

`mod.rs` 顶部 use 补：

```rust
use crate::session::{AgentType, ProcessForm, Session, SessionStatus};
pub use status::LockState;
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd src-tauri && cargo test dsh`
Expected: 全部 dsh 测试 PASS（含 Task 1 注册测试与全量回归）

- [ ] **Step 5: 手动冒烟（真机 ~/.dsh，只读）**

Run: `cd src-tauri && cargo test -- --ignored dsh_smoke 2>/dev/null; echo "---"; ls ~/.dsh/sessions | head -3`
说明：真机冒烟放在 M1 验收（Task 10），此处只需单测绿。

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/monitor/dsh/mod.rs
git commit -m "feat(m1-dsh): 进程发现与会话编排（双源聚合 + lock 探测 + 未读水位）"
```

---

### Task 9: 一键跳转（聚焦 dsh 浏览器标签）

**Files:**
- Create: `src-tauri/src/window/dsh_tab.rs`
- Modify: `src-tauri/src/window/mod.rs`（`pub mod dsh_tab;`）
- Modify: `src-tauri/src/commands/session.rs:166` 附近（macOS 分支最前插入 dsh 路由）

**Interfaces:**
- Produces: `pub fn focus_dsh_tab() -> Result<serde_json::Value, String>`（成功返回 `{"type":"focused","via":"dsh-tab"}` 或 `"dsh-open"`；失败返回 Err——设计 P4：聚焦失败降级为提示）

- [ ] **Step 1: 写实现 + 可测的探针模式**

`src-tauri/src/window/dsh_tab.rs`：

```rust
// dsh 跳转（设计 P4）：无 per-session URL（M0 F9 穷举确证）——只到应用级：
// 聚焦已打开的 dsh web 浏览器标签（URL 前缀匹配），没有则 open 新页。
// AppleScript 已在 M0 证据包 focus-tab.applescript 只读验证（Chrome/Safari，不抢焦点）

/// 找 dsh web 监听端口：优先 cmdline --port，否则探测默认 3080
fn dsh_port() -> u16 {
    if let Ok(out) = std::process::Command::new("sh")
        .arg("-c")
        .arg("lsof -nP -iTCP -sTCP:LISTEN | grep -i node | grep -E '308[0-9]' | head -1 | awk '{print $9}' | awk -F: '{print $NF}'")
        .output()
    {
        if let Ok(s) = String::from_utf8(out.stdout) {
            if let Ok(p) = s.trim().parse() {
                return p;
            }
        }
    }
    3080
}

/// 探测/聚焦标签（probe=true 只定位不抢焦点，供诊断）
fn applescript_focus(port: u16, activate: bool) -> Result<String, String> {
    let url_prefix = format!("http://127.0.0.1:{port}");
    let script = format!(
        r#"
on run
  set hits to ""
  tell application "Google Chrome"
    repeat with w in windows
      repeat with t in tabs of w
        if URL of t starts with "{url_prefix}" or URL of t starts with "http://localhost:{port}" then
          set hits to "found:Chrome"
          if {activate_flag} then
            set active tab index of w to index of t
            set index of w to 1
            activate
          end if
          return hits
        end if
      end repeat
    end repeat
  end tell
  tell application "Safari"
    repeat with w in windows
      repeat with t in tabs of w
        if URL of t starts with "{url_prefix}" or URL of t starts with "http://localhost:{port}" then
          set hits to "found:Safari"
          if {activate_flag} then
            set current tab of w to t
            set index of w to 1
            activate
          end if
          return hits
        end if
      end repeat
    end repeat
  end tell
  return hits
end run
"#,
        url_prefix = url_prefix,
        activate_flag = if activate { "true" } else { "false" },
    );
    let out = std::process::Command::new("osascript")
        .arg("-e").arg(&script)
        .output()
        .map_err(|e| format!("osascript 执行失败: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if stdout.starts_with("found:") {
        Ok(stdout)
    } else {
        Err("未找到 dsh 标签页".into())
    }
}

/// 卡片点击入口：聚焦标签 → 没有 open（cookie 持久 30 天免重登；绝不带 ?token=）
pub fn focus_dsh_tab() -> Result<serde_json::Value, String> {
    let port = dsh_port();
    match applescript_focus(port, true) {
        Ok(_) => Ok(serde_json::json!({ "type": "focused", "via": "dsh-tab" })),
        Err(_) => {
            let url = format!("http://127.0.0.1:{port}/");
            std::process::Command::new("open").arg(&url)
                .spawn().map_err(|e| format!("打开 dsh 失败: {e}"))?;
            Ok(serde_json::json!({ "type": "focused", "via": "dsh-open" }))
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn port_falls_back_to_default() {
        // 不可依赖真机端口；仅验证默认值路径不 panic
        assert_eq!(super::dsh_port() > 0, true);
    }
}
```

`src-tauri/src/window/mod.rs` 加 `pub mod dsh_tab;`。

- [ ] **Step 2: 接线 focus_session 的 macOS 分支**

`src-tauri/src/commands/session.rs` 找到 `#[cfg(target_os = "macos")]`（约 166 行）的 `activate_agent_app` 块，在其**之前**插入：

```rust
        // dsh（M1）：宿主是 node + 浏览器标签形态，不走 APP 激活链路——
        // 聚焦/打开 dsh web 标签页（无 per-session URL，设计 P4 定案）；失败给出提示
        #[cfg(target_os = "macos")]
        if agent_type.as_deref() == Some("dsh") {
            match crate::window::dsh_tab::focus_dsh_tab() {
                Ok(mut out) => {
                    mark_read_on_jump(&app, &session_id, &agent_type);
                    out["via"] = serde_json::Value::String("dsh".into());
                    return Ok(out);
                }
                Err(e) => return Err(format!("无法聚焦 dsh 页面：{e}（请手动打开 dsh web）")),
            }
        }
```

- [ ] **Step 3: 编译 + 现有测试回归**

Run: `cd src-tauri && cargo check && cargo test`
Expected: 编译通过、全量测试 PASS

- [ ] **Step 4: 手动验证（本机 dsh 正在运行）**

Run: `osascript -e 'tell application "System Events" to get name of first process' >/dev/null && echo ok`
然后手动：桌面 MAM 看板出现 dsh 卡片后点击（M1 验收场景一并做，见 Task 10）。此处只验证编译与单测。

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/window/dsh_tab.rs src-tauri/src/window/mod.rs src-tauri/src/commands/session.rs
git commit -m "feat(m1-dsh): 一键跳转——聚焦/打开 dsh web 标签页（macOS）"
```

---

### Task 10: M1 验收（五场景实测 + 全量回归）

**Files:**
- Modify: `docs/research/README.md`（登记验收结果行）

**Interfaces:** 无新代码——本任务是设计 §2 验收标准的执行与记录。

- [ ] **Step 1: 全量检查**

Run: `cd src-tauri && cargo test && cargo clippy -- -D warnings && cd .. && pnpm build && pnpm lint`
Expected: 全绿（clippy 告警清零）

- [ ] **Step 2: 启动真机验证**

Run: `pnpm tauri:dev`（保持 dsh web 运行）
预期：看板出现 dsh 卡片（含历史会话；无子 Agent 幽灵卡）。

- [ ] **Step 3: 五场景逐一验证（设计 §2 验收清单）**

| 场景 | 操作 | 预期 |
|---|---|---|
| 等批准（红） | 在 dsh 里发起一个会触发审批的操作 | dsh 卡变红·等待批准，预览为真人输入；桌面通知触发 |
| 运行中（黄） | 让会话跑长任务 | 卡黄·运行中，长跑 30 分钟不误判为中断 |
| 完成（绿+未读） | 任务结束 | 卡绿且未读，点击跳转后未读清除 |
| 用户取消（不点红） | 在 dsh 里手动取消回合 | 卡变空闲/绿，**不亮红** |
| 强杀（2026-09-14 裁决 D6） | ① `kill -9` dsh web 宿主 ② 另起一个正在跑 turn 的 dsh headless 进程后杀之（web 存活） | ① 卡片随宿主消失（与全工具统一语义，用户已确认）；② 卡红·中断（lock 交叉判定的可达场景），不恒黄 |

- [ ] **Step 4: 跳转验证**

点 dsh 卡片 → 聚焦浏览器 dsh 标签；关闭该标签再点 → 打开新页免登录；两路径都标已读。

- [ ] **Step 5: 回归确认**

其他七工具卡片/通知/未读行为无变化（宪法原则 5）；`~/.dsh` 零写入复核：`find ~/.dsh -newermt "$(date -v-30M '+%Y-%m-%d %H:%M:%S')" -not -path '*/node_modules/*' 2>/dev/null | grep -v -E 'sessions/|storages/|logs?/' | head`（MAM 除 `~/.dsh/skills` 的符号链接管理外不产生其他写入——2026-09-14 用户裁决 skill 管理纳入一期，见宪法 D14 补记）。

- [ ] **Step 6: 记录与收尾**

`docs/research/README.md` 的 dsh 报告行后追加一行验收记录（日期 + 五场景结果）。Commit：

```bash
git add docs/research/README.md
git commit -m "docs(m1-dsh): M1 验收记录（五场景 + 跳转 + 回归）"
```

---

## Self-Review 记录

- **Spec 覆盖**：P1（Task 8 发现/子Agent过滤/卡片）、P2（Task 5 映射+Task 8 未读/通知经 Session.status 自动接入）、P3（Task 7）、P4（Task 9）、P5（Task 3 版本门 + Task 8 读不到跳过降级）——全覆盖。P2 的"通知链路复用"无独立任务：SessionStatus 进入现有 get_all_sessions 管道即自动接入（现有机制），Task 10 场景 1 验证。
- **占位符扫描**：Task 8 有两处"以现实为准对齐"（connection 与 project_name 函数名）——这是对齐既有代码的明确指令（含抄哪里），非 TBD；无其他占位。
- **类型一致性**（2026-09-14 审计后修订）：`header.id` 全文统一为 pub 字段直取；`ProjcacheView` 三字段在 Task 4/7/8 一致；`LockState` 在 Task 5/8 一致；未读语义对齐设计 P2（含 blocked 计未读），未读锚点用事件流最大 time（审计修正，勿改回 lastPromptAt）。

---

## 合并前评审追记（2026-09-14，第 4 轮评审）

三路独立评审（dsh 核心 / 集成性能 / 前端对齐）+ CI 门禁复查后，本分支追加一轮修复（详细动机见各代码注释内「评审 R4/R5/R6」标记）：

- **R4**（dsh 核心 Important #1）：解析失败缓存为 `None` 的条目此前不参与 `live_logs` 保活，被 `retain_existing` 当轮逐出 → 窗内损坏/0 字节文件每轮重复读取解压。修复：保活先于 digest 判空，回归测试 `parse_failure_entry_survives_retain_and_is_not_reparsed`。
- **R5**（集成 Important #1）：skip-write 会让 `last_seen` 停在上次状态变化时刻，cleanup 的 24h TTL 以它计——长青卡离板第一轮即被清行，回退 #35-1 的离板保留语义。修复：`LAST_SEEN_REFRESH_MS`（1h）年龄上限，超龄仍刷新写库（不产生状态边沿）。
- **R6**（集成 Important #2）：`scan_flight_tests` 与 `tests::test_get_all_sessions` 并行互踩（快照/飞行位）→ CI 间歇红。修复：`SCAN_FLIGHT_TEST_LOCK` 提升至模块级跨模块共享。
- **CI 机械修复**：`cargo fmt`（adapter/mod.rs、migration.rs、dsh/mod.rs）+ `prettier`（ResourceByKindView/SessionCard/home）+ `dsh/mod.rs` 测试计数器宏上的 `///` 注释改 `//`（clippy `--all-targets -D warnings`，Windows 交叉门禁口径）。

### 已裁决接受的已知限制：冷启动超窗「开回合」会话不上板

**场景**：dsh 存在开回合会话（红·中断 / 红·等待批准）空闲超 24h 后 MAM 重启——冷缓存 + mtime 预过滤直接跳过，且 dsh web 宿主无法映射进程→会话、无从实现 codex 式 `fill_uncached` 回退，该卡永不上板（MAM 持续运行期间有缓存例外保卡，不受影响）。

**裁决（2026-09-14 用户拍板）**：接受为已知限制、落档不改代码；后续与 I1（强杀中断卡可达性——宿主死=卡片消失 vs 红·中断离线卡）一并裁决是否补轻量 rescue（`session.lock` 存在 / projcache `openTurnStartSeq` 非空则豁免预过滤）。AGENTS.md L3-4 已同步加注。
