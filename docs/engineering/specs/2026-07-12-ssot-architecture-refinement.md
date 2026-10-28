# SSOT 集中仓库架构完善

## 背景与问题

当前 MAM 已实现三层符号链接架构（SSOT → Tool → SubAgent），但存在三个关键缺口：

1. **Skill 导入后不清理原始文件**：各工具的 `~/.claude/skills/xxx` 等目录在导入到 SSOT 后仍然保留，导致用户看到重复 skill
2. **UI 不展示 SSOT 仓库内容**：按工具视图只展示了各工具的原生资源扫描结果，不展示 `~/.mam/skills/` 等 SSOT 仓库中已有的资源清单
3. **MCP/Plugin 缺乏 SSOT 机制**：MCP 和 Plugin 当前直接读写各工具的配置文件，没有经过 SSOT 仓库统一管理

## 设计目标

- 建立统一的资源生命周期：SSOT 仓库作为唯一真实来源
- 支持"清理"操作：将已导入的原始文件替换为符号链接
- UI 展示 SSOT 仓库全貌：用户能直观看到所有已管理的资源
- MCP 和 Plugin 走 SSOT 管理路径，与 Skill 做清晰区分

---

## 第 0 部分：SSOT 仓库目录结构

```
~/.mam/
├── skills/              # Skill SSOT 仓库（已有）
│   ├── brainstorming/
│   │   └── SKILL.md
│   ├── systematic-debugging/
│   │   └── SKILL.md
│   └── ...
├── mcp/                 # MCP 配置 SSOT 仓库（新增）
│   ├── firecrawl/
│   │   └── mcp.json     # { "command": "...", "args": [...], "env": {...} }
│   └── ...
├── plugins/             # Plugin SSOT 仓库（已有目录，需规范）
│   ├── custom-snippets/  # 文件型（目录）
│   ├── my-hook.json     # 配置型（文件）
│   └── ...
└── active/              # Layer 2/3 激活目录（已有）
    ├── claude/
    ├── codex/
    ├── opencode/
    └── openclaw/
```

### 各工具的固定路径（硬编码，不可移）

| 工具 | Skill 目录 | MCP 配置路径 | Plugin 目录 | Plugin 配置路径 |
|------|-----------|-------------|------------|---------------|
| Claude Code | `~/.claude/skills/` | `~/.claude.json` | `~/.claude/plugins/` | `~/.claude/settings.json` |
| Codex CLI | `~/.agents/skills/` | `~/.codex/config.toml` | `~/.codex/plugins/` | `~/.codex/config.toml` |
| OpenCode | `~/.config/opencode/skills/` | `~/.config/opencode/opencode.json` | `~/.config/opencode/plugins/` | `~/.config/opencode/opencode.json` |
| OpenClaw | `~/.openclaw/skills/` | `~/.openclaw/openclaw.json` | `~/.openclaw/plugins/` | `~/.openclaw/openclaw.json` |

### MCP/Plugin 能否移入 `~/.mam/` 管理？

**MCP：可以。** MCP 服务器在工具配置文件中仅存储配置项（command、args、env），command 通常指向全局安装的二进制（npm 包或系统路径）。将 MCP 配置存储在 `~/.mam/mcp/<name>/mcp.json` 作为 SSOT，然后写入各工具配置文件，不影响工具运行。

**文件型 Plugin：不能移原始文件，但可以通过符号链接接管。** 各工具的 Plugin 读取路径是硬编码的（如 `~/.claude/plugins/`），必须由符号链接在原位置指向 SSOT。这和 Skill 的清理逻辑一致——原始目录变为符号链接，指向 `~/.mam/plugins/`。`enable_file_plugin` 已在此机制，`~/.mam/plugins/` 作为 SSOT 仓库即可。

**配置型 Plugin：可以。** 配置型 Plugin 只是 JSON/TOML 文件中的配置段，与 MCP 同理。

---

## 第 1 部分：SSOT 仓库概览（UI 展示）

### 现状

`~/.mam/` 目录下已有 skills、plugins 等内容，但 UI 没有任何地方展示这个仓库的完整内容。

### 设计

在"资源"页面顶部展示 SSOT 仓库概览，Skill、MCP、Plugin 三区块：

```
┌─ MAM 仓库 ─────────────────────────────────────────┐
│ Skills (12)    MCP (3)    Plugins (5)              │
├─ Skills ───────────────────────────────────────────┤
│ systematic-debugging   ✓ 已接入 Claude, Codex       │
│ brainstorming           ✓ 已接入 Claude              │
│ frontend-design         ⚠ 未启用                    │
├─ MCP ─────────────────────────────────────────────┤
│ firecrawl               ✓ 已接入 Claude, Codex      │
│ playwright              ✓ 已接入 Claude              │
├─ Plugins ─────────────────────────────────────────┤
│ custom-snippets         ✓ 已接入 Claude              │
│ my-hook                 ⚠ 未启用                    │
└─────────────────────────────────────────────────────┘
```

**数据源**：`~/.mam/skills/`、`~/.mam/mcp/`、`~/.mam/plugins/` 目录内容结合 `extensions` + `extension_assignments` 表。

**Rust 后端新增命令**：
- `list_ssot_resources()` → 返回 `{ skills: [...], mcp: [...], plugins: [...] }`

**前端改动**：
- `ExtensionList` 上方新增 `SsotRepoOverview` 组件
- 展示三个资源类型的总数和列表
- 列表项显示名称、已在哪些工具中启用

---

## 第 2 部分：Skill 的符号链接托管 + 清理机制

### 2.1 数据流：导入 = 复制 → SSOT；启用 = 符号链接 → 工具目录

```
导入（~/.claude/skills/xxx → SSOT）:
  ~/.claude/skills/xxx/SKILL.md  ──复制──→  ~/.mam/skills/xxx/SKILL.md

启用（SSOT → 工具原始路径）:
  ~/.mam/skills/xxx  ──L2链接──→  ~/.mam/active/claude/skills/xxx
  ~/.mam/active/claude/skills/xxx  ──符号链接──→  ~/.claude/skills/xxx
```

关键：`install_to_repo`（`linker/mod.rs:87`）已经走 `copy_dir_recursive`，复制整个文件夹。`enable_skill_for_tool`（`services/skill/mod.rs:46`）创建符号链接时也指向整个文件夹路径。**导入和链接都是整个 skill 文件夹级别的操作，不只是 SKILL.md 文件。**

### 2.2 清理流程

**触发位置**：按工具视图，每个工具的 skill 列表中，对于在 SSOT 和该工具原始目录中都存在的 skill，显示"清理"标签。

**清理逻辑**：

```
用户点击"清理"（单个）或"全部清理"（该工具下所有重复的）:

对每个需要清理的 skill:
  1. 确认原始目录存在且不是符号链接（安全校验）
  2. 删除原始 skill 目录
  3. 在原始位置创建符号链接 → ~/.mam/skills/<name>/
  4. 更新 DB extension_assignments 的 link_status = "symlinked"

"全部清理" = 遍历该工具下所有有重复的 skill，执行同样操作。
```

**效果**：
```
清理前:
  ~/.claude/skills/brainstorming/  ← 实际目录
  ~/.mam/skills/brainstorming/     ← SSOT（复制品）

清理后:
  ~/.claude/skills/brainstorming/  ← 符号链接 → ~/.mam/skills/brainstorming/
  ~/.mam/skills/brainstorming/     ← SSOT（唯一真实版本）
```

### 2.3 重复检测逻辑

一个新函数判定某个 skill 是否有重复：

```rust
fn has_duplicate_in_tool(skill_name: &str, tool_id: &str) -> bool {
    // SSOT 仓库中有
    repo.exists() &&
    // 该工具原始 skill 目录中也有一个非符号链接的同名目录
    tool_skill_dir.join(skill_name).exists() &&
    !tool_skill_dir.join(skill_name).is_symlink()
}
```

### 2.4 与 import 的关系

- 导入：走 `import_native_resources` 或 `auto_import_extensions`，复制到 SSOT，写入 DB
- 导入后**不自动清理**，也不自动启用
- 用户自主选择：逐个启用、逐个清理、或者一键清理该工具下的所有重复

---

## 第 3 部分：MCP 和 Plugin 的 SSOT 机制

### 3.1 结论

MCP 和 Plugin **不需要也不能**把原始文件移动到 `~/.mam/` 下面，因为它们有自己的安装路径（npm 全局、二进制文件等）。但它们的**配置信息**可以 SSOT 化管理：

| 资源类型 | SSOT 存储 | 同步方式 |
|---------|----------|---------|
| Skill | `~/.mam/skills/<name>/` 全目录 | 符号链接替换原始位置 |
| MCP | `~/.mam/mcp/<name>/mcp.json`（仅配置） | 写入各工具配置文件的 mcpServers 段 |
| 文件型 Plugin | `~/.mam/plugins/<name>/` 全目录 | 符号链接替换原始位置 |
| 配置型 Plugin | `~/.mam/plugins/<name>.json`（仅配置） | 写入各工具配置文件的 plugins 段 |

### 3.2 MCP SSOT 仓库

新增 `~/.mam/mcp/` 目录结构：

```
~/.mam/mcp/
├── firecrawl/
│   └── mcp.json        # { "command": "npx", "args": ["-y", "firecrawl-mcp"], "env": {...} }
├── playwright/
│   └── mcp.json        # { "command": "npx", "args": ["-y", "@anthropic/mcp-playwright"], "env": {...} }
└── ...
```

**关键**：MCP 只存配置，不存资源包或二进制文件。工具中的 MCP 配置指向的是全局安装的 npm 包/二进制路径，`~/.mam/mcp/` 只存这个配置的 SSOT 版本。

**导入流程**：
```
1. 从工具配置文件读取已有 MCP 服务器列表
2. 将每项的配置写入 ~/.mam/mcp/<name>/mcp.json
3. 写入 DB extensions（kind = "mcp"）
```

**启用/禁用流程**（已有 `write_mcp` / `remove_mcp` 在 `services/mcp/mod.rs`）：
```
1. 从 ~/.mam/mcp/<name>/mcp.json 读取配置
2. 按目标工具的格式（JSON/TOML/JSONC）写入其配置文件
3. 更新 DB assignment
```

### 3.3 Plugin SSOT 仓库

已有 `~/.mam/plugins/` 目录，但需规范：

- `install_plugin_to_repo` 用 `~/.mam/plugins/` ✓ 正确
- `enable_file_plugin` 用 `linker::ensure_repo_dir()` 即 `~/.mam/skills/` ✗ **Bug**，应改为 `~/.mam/plugins/`

---

## 第 4 部分：数据库字段扩展

### extension_assignments 表

新增 link_status 值：
- `"duplicate"` — SSOT 和目标工具原始目录中都存在同一 skill（检测到重复但用户未清理）
- `"symlinked"` — 原始目录已被替换为符号链接（用户已点"清理"）

现有值不变：`"valid"`、`"missing"`、`"ui-only"`

---

## 第 5 部分：Rust 新增命令

| 命令 | 功能 |
|------|------|
| `list_ssot_resources()` | 扫描 `~/.mam/skills/`、`~/.mam/mcp/`、`~/.mam/plugins/` 目录，返回三类资源的清单 |
| `detect_duplicate_skills(tool_id)` | 返回指定工具下所有在 SSOT 和原始目录中都存在的 skill 列表 |
| `cleanup_duplicate_skills(tool_id, names: Vec<String>)` | 清理指定工具下的重复 skill（可单个或批量），删除原始目录，创建符号链接 |
| `import_mcp_to_ssot(tool_id)` | 从指定工具的配置文件导入 MCP 服务器列表到 `~/.mam/mcp/`（Phase 3） |

---

## 第 6 部分：实施计划（三阶段）

### Phase 1：SSOT 仓库展示 + Plugin 路径修正

1. 修复 `enable_file_plugin` 的路径 bug（skills → plugins）
2. 新增 `list_ssot_resources` 命令
3. 前端新增 `SsotRepoOverview` 组件
4. Skill、MCP、Plugin 三区块独立展示

### Phase 2：Skill 清理机制

1. 新增 `detect_duplicate_skills` 命令
2. 新增 `cleanup_duplicate_skills` 命令
3. 新增 `linker::replace_with_symlink` 函数
4. 在按工具视图中加入"清理"按钮
5. 支持单个清理和"一键全部清理"

### Phase 3：MCP SSOT 导入/导出（独立 plan，不在本 spec 中实施）

1. 新增 `import_mcp_to_ssot` 命令
2. MCP 启用/禁用统一走 SSOT 读取配置 → 写入工具配置文件
3. `~/.mam/mcp/` 目录初始化
4. `SsotRepoOverview` 展示 MCP 资源