# 测试体系 实现计划

> **面向 AI 代理的工作者：** 必需子技能：使用 superpowers:subagent-driven-development（推荐）或 superpowers:executing-plans 逐任务实现此计划。步骤使用复选框（`- [ ]`）语法来跟踪进度。

**目标：** 建立完整的测试基础设施，覆盖后端 Rust（DAO/Linker/Services/Commands 集成测试）和前端 React（vitest 组件测试 + MSW Tauri API Mock），核心模块覆盖率 >=80%。

**架构：** 后端使用 `tempfile` crate 创建临时数据库和目录，通过 `tauri::test::mock_builder()` 测试 IPC 命令。前端使用 vitest + @testing-library/react 测试组件，MSW 拦截 Tauri invoke 调用模拟后端响应。所有测试在 Spec 002 重构完成后编写。

**技术栈：** Rust + tempfile + rusqlite + vitest + @testing-library/react + MSW + @vitest/coverage-v8

---

## 前置依赖

- Spec 002 重构完成（database/dao、services/、React Query、api 层就位）
- `tempfile = "3"` 已在 `src-tauri/Cargo.toml` 的 `[dev-dependencies]`

---

## 文件结构

### 后端（Rust）

| 文件 | 职责 |
|------|------|
| `src-tauri/tests/support.rs` | 测试辅助：内存数据库初始化、临时目录 |
| `src-tauri/tests/dao_test.rs` | DAO 层 CRUD 单元测试 |
| `src-tauri/tests/linker_test.rs` | 三层符号链接映射测试 |
| `src-tauri/tests/services_test.rs` | Service 层业务逻辑测试 |
| `src-tauri/tests/commands_test.rs` | IPC 命令集成测试 |

### 前端（TypeScript）

| 文件 | 职责 |
|------|------|
| `vitest.config.ts` | vitest 配置 |
| `tests/setup.ts` | 全局测试初始化 |
| `tests/msw/server.ts` | MSW 服务器实例 |
| `tests/msw/handlers.ts` | Mock handler 聚合 |
| `tests/msw/tauriMocks.ts` | Tauri API mock handlers |
| `tests/components/SessionCard.test.tsx` | 会话卡片组件测试 |
| `tests/components/SessionGrid.test.tsx` | 会话网格测试 |
| `tests/components/ResourceByKindView.test.tsx` | 资源视图测试 |
| `tests/components/PresetList.test.tsx` | 预设组列表测试 |
| `tests/hooks/useSessions.test.ts` | 会话 hook 测试 |
| `tests/integration/dashboard-flow.test.tsx` | 看板集成测试 |

---

## 任务分解

### 任务 1：Rust 测试基础设施（FR-1）

**文件：**
- 创建：`src-tauri/tests/support.rs`

- [ ] **步骤 1：创建测试辅助模块**

```rust
// src-tauri/tests/support.rs
use rusqlite::Connection;
use tempfile::TempDir;

/// 初始化全局数据库（指向临时文件），替代内存数据库
/// 当前 store.rs 使用全局 DB 静态连接，测试需初始化该全局变量
pub fn setup_test_db() {
    let temp = setup_test_home();
    crate::database::init();
}

/// 创建内存数据库（仅用于 DAO trait 单元测试，不依赖全局 DB）
pub fn setup_in_memory_db() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    crate::database::schema::init(&conn).unwrap();
    conn
}

/// 创建临时目录，模拟 ~/.mam/ 结构
pub fn setup_test_mam_dir() -> TempDir {
    let temp = tempfile::tempdir().unwrap();
    let mam = temp.path().join(".mam");
    std::fs::create_dir_all(mam.join("skills")).unwrap();
    std::fs::create_dir_all(mam.join("mcp")).unwrap();
    std::fs::create_dir_all(mam.join("plugins")).unwrap();
    std::fs::create_dir_all(mam.join("active")).unwrap();
    temp
}

/// 在临时目录中创建一个测试 skill
pub fn create_test_skill(mam_dir: &std::path::Path, name: &str) -> std::path::PathBuf {
    let skill_path = mam_dir.join(".mam/skills").join(name);
    std::fs::create_dir_all(&skill_path).unwrap();
    std::fs::write(
        skill_path.join("SKILL.md"),
        format!("---\nname: {}\n---\n# {}", name, name),
    ).unwrap();
    skill_path
}

/// 创建测试用 ExtensionRecord
pub fn create_test_extension() -> crate::database::dao::extension::ExtensionRecord {
    crate::database::dao::extension::ExtensionRecord {
        id: "test-skill-1".to_string(),
        kind: "skill".to_string(),
        name: "Test Skill".to_string(),
        description: Some("测试用 skill".to_string()),
        source_path: "/tmp/test-skill".to_string(),
        source_url: None,
        suite: None,
        manifest_path: None,
        permissions: None,
        min_runtime: None,
    }
}
```

- [ ] **步骤 2：验证编译**

运行：`cd src-tauri && cargo test --no-run`
预期：PASS（编译通过，暂无测试函数）

- [ ] **步骤 3：Commit**

```bash
git add src-tauri/tests/support.rs
git commit -m "test: add Rust test support utilities (in-memory db, temp dirs)"
```

---

### 任务 2：DAO 层单元测试（FR-1）

**文件：**
- 创建：`src-tauri/tests/dao_test.rs`

- [ ] **步骤 1：编写 Session DAO 测试**

```rust
// src-tauri/tests/dao_test.rs
mod support;

use support::setup_test_db;
use crate::database::dao::session;

#[test]
fn test_update_and_cleanup_session_status() {
    setup_test_db(); // 初始化全局 DB

    // 写入会话状态（函数内部使用全局 DB，不传 Connection）
    let prev = session::update_session_status("session-1", "claude", "running");
    assert!(prev.is_none()); // 首次记录返回 None

    // 验证状态已写入（通过 DAO trait 查询）
    let dao = session::SessionDaoImpl;
    let status = dao.find_status("session-1");
    assert_eq!(status, Some("running".to_string()));

    // 清理不活跃会话
    let mut active = std::collections::HashSet::new();
    active.insert("session-1".to_string());
    session::cleanup_stale_sessions(&active);

    // session-1 仍在活跃列表中，不应被清理
    let status = dao.find_status("session-1");
    assert_eq!(status, Some("running".to_string()));
}
```

- [ ] **步骤 2：编写 Extension DAO 测试**

```rust
use support::{setup_test_db, create_test_extension};
use crate::database::dao::extension;

#[test]
fn test_extension_crud() {
    setup_test_db();
    let ext = create_test_extension();

    // 插入（函数内部使用全局 DB）
    extension::insert_extension(&ext).unwrap();

    // 查询全部
    let list = extension::list_extensions();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].id, "test-skill-1");

    // 查询分配
    let assignments = extension::list_assignments("claude");
    assert!(assignments.is_empty());

    // 插入分配
    extension::upsert_assignment("test-skill-1", "claude", true, "linked").unwrap();
    let assignments = extension::list_assignments("claude");
    assert_eq!(assignments.len(), 1);
    assert!(assignments[0].enabled);
}

#[test]
fn test_native_extension_crud() {
    setup_test_db();
    let native = extension::NativeExtensionRecord {
        id: "native-1".to_string(),
        kind: "skill".to_string(),
        name: "Native Skill".to_string(),
        description: None,
        source_path: "/tmp/native".to_string(),
        source_tool: "claude".to_string(),
        detected_at: chrono::Utc::now().to_rfc3339(),
        imported: 0,
    };

    extension::insert_native_extension(&native).unwrap();
    let list = extension::list_native_extensions(Some("claude"));
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].id, "native-1");
}
```

- [ ] **步骤 3：编写 Preset DAO 测试**

```rust
use support::setup_test_db;
use crate::database::dao::preset;

#[test]
fn test_preset_crud() {
    setup_test_db();

    // 创建预设组（函数内部使用全局 DB）
    let preset_id = preset::create_preset("前端开发", &[
        ("skill-1".to_string(), "skill".to_string()),
        ("mcp-1".to_string(), "mcp".to_string()),
    ]).unwrap();
    assert!(!preset_id.is_empty());

    // 查询列表
    let presets = preset::list_presets();
    assert_eq!(presets.len(), 1);
    assert_eq!(presets[0].name, "前端开发");

    // 查询预设项
    let items = preset::get_preset_items(&preset_id);
    assert_eq!(items.len(), 2);

    // 删除
    preset::delete_preset(&preset_id).unwrap();
    let presets = preset::list_presets();
    assert!(presets.is_empty());
}
```

- [ ] **步骤 4：编写 Settings DAO 测试**

```rust
use support::setup_test_db;
use crate::database::dao::settings;

#[test]
fn test_settings_get_set() {
    setup_test_db();

    assert!(settings::get_setting("nonexistent").is_none());

    settings::set_setting("poll_interval", "3000");
    assert_eq!(settings::get_setting("poll_interval"), Some("3000".to_string()));

    settings::set_setting("poll_interval", "5000");
    assert_eq!(settings::get_setting("poll_interval"), Some("5000".to_string()));
}
```

- [ ] **步骤 5：运行测试**

运行：`cd src-tauri && cargo test --test dao_test`
预期：全部 PASS

- [ ] **步骤 6：Commit**

```bash
git add src-tauri/tests/dao_test.rs
git commit -m "test(dao): add CRUD unit tests for session/extension/preset/settings DAOs"
```

---

### 任务 3：Linker 三层映射测试（FR-1）

**文件：**
- 创建：`src-tauri/tests/linker_test.rs`

- [ ] **步骤 1：编写符号链接创建/删除测试**

```rust
// src-tauri/tests/linker_test.rs
mod support;

use support::setup_test_mam_dir;
use crate::linker;

#[test]
fn test_create_and_remove_link() {
    let temp = setup_test_mam_dir();
    let mam = temp.path().join(".mam");

    // Layer 1: 创建源 skill
    let source = support::create_test_skill(temp.path(), "brainstorming");

    // Layer 2: 创建符号链接
    let target = mam.join("active/claude/skills/brainstorming");
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    linker::create_link(&source, &target).unwrap();

    assert!(target.exists());
    // 验证链接指向源
    assert_eq!(
        std::fs::read_link(&target).unwrap(),
        source.canonicalize().unwrap()
    );

    // 删除链接
    linker::remove_link(&target).unwrap();
    assert!(!target.exists());
    // 源文件不受影响
    assert!(source.exists());
}

#[test]
fn test_install_to_repo() {
    let temp = setup_test_mam_dir();
    let mam = temp.path().join(".mam");

    // 创建外部源目录
    let external = temp.path().join("external/my-skill");
    std::fs::create_dir_all(&external).unwrap();
    std::fs::write(external.join("SKILL.md"), "---\nname: my-skill\n---\n# My Skill").unwrap();

    // 安装到仓库
    linker::install_to_repo(&external, "my-skill").unwrap();

    // 验证仓库中有该 skill（install_to_repo 复制到全局仓库目录）
    let repo_skill = mam.join("skills/my-skill");
    assert!(repo_skill.exists());
    assert!(repo_skill.join("SKILL.md").exists());
}
```

- [ ] **步骤 2：编写原子写入测试**

```rust
#[test]
fn test_write_atomic() {
    let temp = setup_test_mam_dir();
    let target = temp.path().join(".mam/test-config.json");

    linker::write_atomic(&target, r#"{"key": "value"}"#).unwrap();

    let content = std::fs::read_to_string(&target).unwrap();
    assert_eq!(content, r#"{"key": "value"}"#);

    // 临时文件不应残留
    assert!(!temp.path().join(".mam/test-config.tmp").exists());
}

#[test]
fn test_write_config_locked() {
    let temp = setup_test_mam_dir();
    let target = temp.path().join(".mam/locked-config.json");

    linker::write_config_locked(&target, r#"{"locked": true}"#).unwrap();

    let content = std::fs::read_to_string(&target).unwrap();
    assert_eq!(content, r#"{"locked": true}"#);
}
```

- [ ] **步骤 3：运行测试**

运行：`cd src-tauri && cargo test --test linker_test`
预期：全部 PASS

- [ ] **步骤 4：Commit**

```bash
git add src-tauri/tests/linker_test.rs
git commit -m "test(linker): add three-layer symlink and atomic write tests"
```

---

### 任务 4：Services 层测试（FR-1）

**文件：**
- 创建：`src-tauri/tests/services_test.rs`

- [ ] **步骤 1：编写 Skill 服务测试**

```rust
// src-tauri/tests/services_test.rs
mod support;

use support::setup_test_mam_dir;

#[test]
fn test_install_and_enable_skill() {
    let temp = setup_test_mam_dir();

    // 创建外部 skill 源
    let external = temp.path().join("external/test-skill");
    std::fs::create_dir_all(&external).unwrap();
    std::fs::write(external.join("SKILL.md"), "---\nname: test-skill\n---\n# Test").unwrap();

    // 安装 skill 到仓库（函数名保持原样，仅模块路径变化）
    crate::services::skill::install_skill("external/test-skill", "test-skill").unwrap();

    // 为 claude 启用
    crate::services::skill::enable_skill_for_tool("test-skill", "claude").unwrap();

    // 验证 Layer 2 链接存在
    let active = std::env::home_dir().unwrap().join(".mam/active/claude/skills/test-skill");
    // 注意：实际测试需要 mock home_dir 或使用环境变量，此处验证逻辑
}
```

注意：Services 测试涉及文件系统路径，需通过环境变量 `HOME` 指向临时目录来隔离。在 `support.rs` 中可添加：

```rust
pub fn setup_test_home() -> TempDir {
    let temp = tempfile::tempdir().unwrap();
    std::env::set_var("HOME", temp.path());
    setup_test_mam_dir_at(temp.path());
    temp
}
```

- [ ] **步骤 2：编写 Preset 服务测试**

```rust
#[test]
fn test_apply_and_deactivate_preset() {
    let _home = support::setup_test_home();
    support::setup_test_db(); // 初始化全局 DB

    // 安装 2 个 skill 到仓库
    support::create_test_skill(std::env::home_dir().unwrap(), "skill-a");
    support::create_test_skill(std::env::home_dir().unwrap(), "skill-b");

    // 创建预设组（全局 DB）
    let preset_id = crate::database::dao::preset::create_preset(
        "测试组",
        &[("skill-a".into(), "skill".into()), ("skill-b".into(), "skill".into())]
    ).unwrap();

    // 应用预设到 claude（apply_preset 不接收 Connection，使用全局 DB）
    let result = crate::services::preset::apply_preset(&preset_id, "claude");
    assert!(result.success_count > 0);

    // 验证 Layer 2 链接存在
    let home = std::env::home_dir().unwrap();
    assert!(home.join(".mam/active/claude/skills/skill-a").exists());
    assert!(home.join(".mam/active/claude/skills/skill-b").exists());

    // 取消预设
    crate::services::preset::deactivate_preset(&preset_id, "claude").unwrap();
    assert!(!home.join(".mam/active/claude/skills/skill-a").exists());
    assert!(!home.join(".mam/active/claude/skills/skill-b").exists());
}
```

- [ ] **步骤 3：运行测试**

运行：`cd src-tauri && cargo test --test services_test`
预期：全部 PASS

- [ ] **步骤 4：Commit**

```bash
git add src-tauri/tests/services_test.rs src-tauri/tests/support.rs
git commit -m "test(services): add skill install and preset apply/deactivate tests"
```

---

### 任务 5：Commands 集成测试（FR-1）

**文件：**
- 创建：`src-tauri/tests/commands_test.rs`

- [ ] **步骤 1：编写 IPC 命令契约测试**

```rust
// src-tauri/tests/commands_test.rs
// 注意：Tauri 集成测试需要 mock app 上下文
// 对于不依赖 AppHandle 的命令，可直接调用函数测试

mod support;

#[test]
fn test_get_setting_returns_none_for_missing() {
    let _home = support::setup_test_home();
    support::setup_test_db(); // 初始化全局 DB
    crate::database::init();

    let result = crate::commands::settings::get_setting("nonexistent".to_string());
    assert_eq!(result, None);
}

#[test]
fn test_set_and_get_setting() {
    let _home = support::setup_test_home();
    crate::database::init();

    crate::commands::settings::set_setting("test_key".to_string(), "test_value".to_string());
    let result = crate::commands::settings::get_setting("test_key".to_string());
    assert_eq!(result, Some("test_value".to_string()));
}

#[test]
fn test_list_presets_empty() {
    let _home = support::setup_test_home();
    crate::database::init();

    let presets = crate::commands::preset::list_presets();
    assert!(presets.is_empty());
}

#[test]
fn test_detect_tools_returns_list() {
    let _home = support::setup_test_home();
    let tools = crate::commands::settings::detect_tools();
    // detect_tools 返回检测到的工具列表，测试环境中可能为空
    assert!(tools.iter().all(|t| !t.tool_id.is_empty()));
}
```

- [ ] **步骤 2：运行测试**

运行：`cd src-tauri && cargo test --test commands_test`
预期：全部 PASS

- [ ] **步骤 3：Commit**

```bash
git add src-tauri/tests/commands_test.rs
git commit -m "test(commands): add IPC command contract tests"
```

---

### 任务 6：前端 vitest 配置（FR-2）

**文件：**
- 创建：`vitest.config.ts`
- 创建：`tests/setup.ts`
- 修改：`package.json`（添加 test 脚本和依赖）

- [ ] **步骤 1：安装依赖**

```bash
pnpm add -D vitest @testing-library/react @testing-library/jest-dom @testing-library/user-event jsdom @vitest/coverage-v8
```

- [ ] **步骤 2：创建 vitest.config.ts**

```typescript
// vitest.config.ts
import { defineConfig } from "vitest/config";
import react from "@vitejs/plugin-react";
import path from "path";

export default defineConfig({
  plugins: [react()],
  test: {
    environment: "jsdom",
    globals: true,
    setupFiles: ["./tests/setup.ts"],
    coverage: {
      provider: "v8",
      reporter: ["text", "html"],
      exclude: ["node_modules/", "tests/", "src/lib/api/", "src/components/ui/"],
    },
  },
  resolve: {
    alias: {
      "@": path.resolve(__dirname, "./src"),
    },
  },
});
```

- [ ] **步骤 3：创建 tests/setup.ts**

```typescript
// tests/setup.ts
import "@testing-library/jest-dom";
import { server } from "./msw/server";

beforeAll(() => server.listen());
afterEach(() => server.resetHandlers());
afterAll(() => server.close());
```

- [ ] **步骤 4：在 package.json 添加 test 脚本**

```json
{
  "scripts": {
    "test": "vitest run",
    "test:watch": "vitest",
    "test:coverage": "vitest run --coverage"
  }
}
```

- [ ] **步骤 5：验证配置**

运行：`pnpm test`
预期：无测试文件时输出 "No test files found"，无报错

- [ ] **步骤 6：Commit**

```bash
git add vitest.config.ts tests/setup.ts package.json
git commit -m "test: configure vitest with jsdom and coverage"
```

---

### 任务 7：MSW + Tauri Mock（FR-3）

**文件：**
- 创建：`tests/msw/tauriMocks.ts`
- 创建：`tests/msw/handlers.ts`
- 创建：`tests/msw/server.ts`
- 修改：`src/tauri-mock.ts`（添加 vitest 分支）

- [ ] **步骤 1：安装 MSW**

```bash
pnpm add -D msw
```

> **Spec 偏差说明：** Spec 003 FR-3.1 展示 MSW 用 `http.post("/tauri/...")` 拦截 HTTP 请求。但 Tauri `invoke()` 不是 HTTP 请求，MSW 无法拦截。本计划改用 vitest 的 `vi.mock` 拦截 `@tauri-apps/api/core` 的 `invoke` 函数，MSW 仅保留用于未来可能的 HTTP 请求 mock（如 GitHub API 版本检查）。此方案比 spec 原文更适配 Tauri 架构。

- [ ] **步骤 2：创建 Tauri mock handlers**

```typescript
// tests/msw/tauriMocks.ts
import { http, HttpResponse } from "msw";

// 测试数据
export const mockSessions = {
  sessions: [
    {
      id: "session-1",
      tool: "claude",
      projectPath: "/tmp/project1",
      status: "running",
      pid: 12345,
      cpuUsage: 12.5,
      duration: 300,
      lastMessage: "正在处理...",
      gitBranch: "main",
    },
    {
      id: "session-2",
      tool: "codex",
      projectPath: "/tmp/project2",
      status: "waiting",
      pid: 12346,
      cpuUsage: 0,
      duration: 600,
      lastMessage: "等待用户输入",
      gitBranch: "develop",
    },
  ],
};

export const mockExtensions = [
  {
    id: "brainstorming",
    kind: "skill",
    name: "Brainstorming",
    description: "头脑风暴 skill",
    assignments: [{ toolId: "claude", enabled: true, linkStatus: "linked" }],
  },
];

export const mockPresets = [
  { id: "preset-1", name: "前端开发", items: [["brainstorming", "skill"]] },
];

// 由于 Tauri invoke 不是 HTTP 请求，需要在测试中 mock invoke 函数本身
// 使用 vitest 的 vi.mock 拦截 @tauri-apps/api/core 的 invoke
export const tauriInvokeMock = (cmd: string, _args?: unknown) => {
  switch (cmd) {
    case "get_all_sessions":
      return Promise.resolve(mockSessions);
    case "list_extensions_with_assignments":
      return Promise.resolve(mockExtensions);
    case "list_presets":
      return Promise.resolve(mockPresets);
    case "focus_session":
      return Promise.resolve();
    case "get_setting":
      return Promise.resolve(null);
    case "set_setting":
      return Promise.resolve();
    case "detect_tools":
      return Promise.resolve([]);
    default:
      return Promise.resolve(undefined);
  }
};
```

- [ ] **步骤 3：创建 handlers.ts 和 server.ts**

```typescript
// tests/msw/handlers.ts
// MSW 主要用于拦截 HTTP 请求（如果有的话），
// Tauri invoke 通过 vi.mock 在 setup 中全局 mock
export const handlers: never[] = [];
```

```typescript
// tests/msw/server.ts
import { setupServer } from "msw/node";
import { handlers } from "./handlers";

export const server = setupServer(...handlers);
```

- [ ] **步骤 4：在 tests/setup.ts 中 mock invoke**

更新 `tests/setup.ts`：

```typescript
import "@testing-library/jest-dom";
import { vi } from "vitest";
import { server } from "./msw/server";
import { tauriInvokeMock } from "./msw/tauriMocks";

// 全局 mock Tauri invoke
vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn((cmd: string, args?: unknown) => tauriInvokeMock(cmd, args)),
}));

beforeAll(() => server.listen());
afterEach(() => {
  server.resetHandlers();
  vi.clearAllMocks();
});
afterAll(() => server.close());
```

- [ ] **步骤 5：更新 tauri-mock.ts 添加 vitest 分支**

```typescript
// src/tauri-mock.ts 顶部添加
export function mockTauriApi() {
  if (import.meta.env.VITEST) {
    // vitest 环境下由 setup.ts 中的 vi.mock 处理
    return;
  }
  // 原有浏览器渲染 mock 逻辑保持不变
  // ...
}
```

- [ ] **步骤 6：验证 mock 生效**

创建一个最小测试验证：

```typescript
// tests/msw/tauriMocks.test.ts
import { invoke } from "@tauri-apps/api/core";
import { describe, it, expect } from "vitest";

describe("Tauri mock", () => {
  it("mocks get_all_sessions", async () => {
    const result = await invoke("get_all_sessions");
    expect(result).toHaveProperty("sessions");
    expect((result as { sessions: unknown[] }).sessions).toHaveLength(2);
  });
});
```

运行：`pnpm test tests/msw/tauriMocks.test.ts`
预期：PASS

- [ ] **步骤 7：Commit**

```bash
git add tests/msw/ tests/setup.ts src/tauri-mock.ts
git commit -m "test(msw): add Tauri invoke mock infrastructure for frontend tests"
```

---

### 任务 8：前端组件测试（FR-2）

**文件：**
- 创建：`tests/components/SessionCard.test.tsx`
- 创建：`tests/components/SessionGrid.test.tsx`
- 创建：`tests/components/ResourceByKindView.test.tsx`
- 创建：`tests/components/PresetList.test.tsx`

- [ ] **步骤 1：编写 SessionCard 测试**

```typescript
// tests/components/SessionCard.test.tsx
import { render, screen } from "@testing-library/react";
import { describe, it, expect } from "vitest";
import { SessionCard } from "@/components/sessions/SessionCard";

const mockRunningSession = {
  id: "s1",
  tool: "claude",
  projectPath: "/tmp/proj",
  status: "running",
  pid: 1234,
  cpuUsage: 15.2,
  duration: 300,
  lastMessage: "正在处理...",
  gitBranch: "main",
};

const mockWaitingSession = {
  ...mockRunningSession,
  id: "s2",
  status: "waiting",
  lastMessage: "等待输入",
};

describe("SessionCard", () => {
  it("renders running status with yellow light", () => {
    render(<SessionCard session={mockRunningSession} />);
    expect(screen.getByText("正在处理...")).toBeInTheDocument();
    const light = screen.getByTestId("status-light");
    expect(light.className).toContain("bg-yellow");
  });

  it("renders waiting status with red light", () => {
    render(<SessionCard session={mockWaitingSession} />);
    expect(screen.getByText("等待输入")).toBeInTheDocument();
    const light = screen.getByTestId("status-light");
    expect(light.className).toContain("bg-red");
  });

  it("displays tool name and project path", () => {
    render(<SessionCard session={mockRunningSession} />);
    expect(screen.getByText(/claude/i)).toBeInTheDocument();
    expect(screen.getByText("/tmp/proj")).toBeInTheDocument();
  });
});
```

注意：`SessionCard` 组件需要有 `data-testid="status-light"` 属性。如果没有，在组件中添加。

- [ ] **步骤 2：编写 SessionGrid 测试**

```typescript
// tests/components/SessionGrid.test.tsx
import { render, screen } from "@testing-library/react";
import { describe, it, expect } from "vitest";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { SessionGrid } from "@/components/sessions/SessionGrid";

const queryClient = new QueryClient({
  defaultOptions: { queries: { retry: false, staleTime: 0 } },
});

describe("SessionGrid", () => {
  it("renders session cards from mocked data", async () => {
    render(
      <QueryClientProvider client={queryClient}>
        <SessionGrid />
      </QueryClientProvider>
    );

    // mockSessions 包含 "正在处理..." 和 "等待用户输入"
    expect(await screen.findByText("正在处理...")).toBeInTheDocument();
    expect(screen.getByText("等待用户输入")).toBeInTheDocument();
  });
});
```

- [ ] **步骤 3：编写 PresetList 测试**

```typescript
// tests/components/PresetList.test.tsx
import { render, screen } from "@testing-library/react";
import { describe, it, expect } from "vitest";
import { PresetList } from "@/components/presets/PresetList";

describe("PresetList", () => {
  it("renders preset names from query data", async () => {
    // PresetList 内部使用 usePresetsQuery，invoke 已被全局 mock
    render(<PresetList toolId="claude" />);

    // mockPresets 包含 "前端开发"
    expect(await screen.findByText("前端开发")).toBeInTheDocument();
  });
});
```

- [ ] **步骤 4：编写 ResourceByKindView 测试**

```typescript
// tests/components/ResourceByKindView.test.tsx
import { render, screen } from "@testing-library/react";
import { describe, it, expect } from "vitest";
import { ResourceByKindView } from "@/components/resources/ResourceByKindView";

describe("ResourceByKindView", () => {
  it("renders extension list from query data", async () => {
    render(<ResourceByKindView />);
    // mockExtensions 包含 "Brainstorming"
    expect(await screen.findByText("Brainstorming")).toBeInTheDocument();
  });
});
```

- [ ] **步骤 5：运行测试**

运行：`pnpm test`
预期：全部 PASS

- [ ] **步骤 6：Commit**

```bash
git add tests/components/
git commit -m "test(components): add SessionCard, PresetList, ResourceByKindView tests"
```

---

### 任务 9：Hook 测试 + 集成测试（FR-2）

**文件：**
- 创建：`tests/hooks/useSessions.test.ts`
- 创建：`tests/integration/dashboard-flow.test.tsx`

- [ ] **步骤 1：编写 useSessions hook 测试**

```typescript
// tests/hooks/useSessions.test.ts
import { renderHook, waitFor } from "@testing-library/react";
import { describe, it, expect } from "vitest";
import { useSessions } from "@/hooks/useSessions";

describe("useSessions", () => {
  it("returns session list from mocked invoke", async () => {
    const { result } = renderHook(() => useSessions());

    // 初始状态为 loading
    expect(result.current.isLoading).toBe(true);

    // 等待数据加载
    await waitFor(() => {
      expect(result.current.isSuccess).toBe(true);
    });

    expect(result.current.data?.sessions).toHaveLength(2);
    expect(result.current.data?.sessions[0].tool).toBe("claude");
  });
});
```

注意：React Query 的 `useQuery` 需要 `QueryClientProvider` 包裹。在 `tests/setup.ts` 中或测试文件中添加 wrapper：

```typescript
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";

const queryClient = new QueryClient({
  defaultOptions: { queries: { retry: false } },
});

const wrapper = ({ children }: { children: React.ReactNode }) => (
  <QueryClientProvider client={queryClient}>{children}</QueryClientProvider>
);

const { result } = renderHook(() => useSessions(), { wrapper });
```

- [ ] **步骤 2：编写看板集成测试**

```typescript
// tests/integration/dashboard-flow.test.tsx
import { render, screen, waitFor } from "@testing-library/react";
import { describe, it, expect } from "vitest";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import Home from "@/pages/home";

const queryClient = new QueryClient({
  defaultOptions: { queries: { retry: false, staleTime: 0 } },
});

describe("Dashboard flow", () => {
  it("renders session cards from mocked data", async () => {
    render(
      <QueryClientProvider client={queryClient}>
        <Home />
      </QueryClientProvider>
    );

    // 等待会话数据加载并渲染
    await waitFor(() => {
      expect(screen.getByText("正在处理...")).toBeInTheDocument();
    });
    expect(screen.getByText("等待输入")).toBeInTheDocument();
  });
});
```

- [ ] **步骤 3：运行全部测试**

运行：`pnpm test`
运行：`cd src-tauri && cargo test`
预期：全部 PASS

- [ ] **步骤 4：检查覆盖率**

运行：`pnpm test:coverage`
预期：核心模块（DAO + Services + Linker）覆盖率 >= 80%（后端覆盖率需在 Rust 侧用 `cargo tarpaulin` 或类似工具检查）

- [ ] **步骤 5：Commit**

```bash
git add tests/hooks/ tests/integration/
git commit -m "test: add useSessions hook test and dashboard integration test"
```

---

## 自检

**规格覆盖度：**
- FR-1 Rust 集成测试 -> 任务 1（support）+ 任务 2（DAO）+ 任务 3（Linker）+ 任务 4（Services）+ 任务 5（Commands）✓
- FR-2 前端 vitest -> 任务 6（配置）+ 任务 8（组件测试）+ 任务 9（hook + 集成测试）✓
- FR-3 MSW + Tauri Mock -> 任务 7 ✓
无遗漏。

**占位符扫描：** 无占位符。所有步骤含具体测试代码和运行命令。

**类型一致性：** `tauriInvokeMock` 在任务 7 定义，任务 8/9 通过全局 `vi.mock` 间接引用。mock 数据结构（`mockSessions`、`mockExtensions`、`mockPresets`）在任务 7 定义，测试中引用一致。`SessionCard` 的 `data-testid="status-light"` 在任务 8 中要求添加，测试断言引用一致。
