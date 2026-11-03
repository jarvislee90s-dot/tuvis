# 文档与流程 实现计划

> **面向 AI 代理的工作者：** 必需子技能：使用 superpowers:subagent-driven-development（推荐）或 superpowers:executing-plans 逐任务实现此计划。步骤使用复选框（`- [ ]`）语法来跟踪进度。

**目标：** 建立完整的文档体系和开发流程规范，包括 GitHub CI/CD、Issue/PR 模板、多语言用户指南和手册、CHANGELOG、CONTRIBUTING、SECURITY、ADR、commitlint、Dependabot、CODEOWNERS。

**架构：** 所有文件为文档和配置，不涉及代码逻辑变更。CI/CD 流水线依赖 Spec 003 的测试基础设施（cargo test + pnpm test）。文档内容在功能稳定后编写，中英文双语。

**技术栈：** GitHub Actions + YAML + Markdown + commitlint + Dependabot

---

## 前置依赖

- Spec 002 重构完成（CI 中 `cargo check` / `tsc --noEmit` 需要新结构）
- Spec 003 测试体系完成（CI 中 `cargo test` / `pnpm test` 需要测试文件存在）

---

## 文件结构

| 文件 | 职责 |
|------|------|
| `.github/workflows/ci.yml` | CI 流水线：前端 lint + 后端 cargo check/test |
| `.github/workflows/release.yml` | 发布构建：多平台打包 |
| `.github/workflows/stale.yml` | 自动关闭 stale issue |
| `.github/ISSUE_TEMPLATE/*.yml` | 4 个 Issue 模板 + config |
| `.github/dependabot.yml` | 依赖自动更新 |
| `.github/CODEOWNERS` | 代码所有权 |
| `.github/PULL_REQUEST_TEMPLATE.md` | PR 模板 |
| `docs/product/guides/{en,zh}/*.md` | 多语言快速指南（5 主题 x 2 语言） |
| `docs/product/manual/{en,zh}/*.md` | 多语言用户手册（11 章 x 2 语言） |
| `docs/release-notes/*.md` | 版本发布说明 |
| `docs/engineering/architecture/adr/*.md` | 架构决策记录 |
| `CHANGELOG.md` | 变更日志 |
| `CONTRIBUTING.md` | 贡献指南 |
| `SECURITY.md` | 安全策略 |
| `commitlint.config.js` | commit 规范配置 |

---

## 任务分解

### 任务 1：GitHub CI/CD 流水线（FR-1）

**文件：**
- 创建：`.github/workflows/ci.yml`
- 创建：`.github/workflows/release.yml`
- 创建：`.github/workflows/stale.yml`

- [ ] **步骤 1：创建 CI 工作流**

```yaml
# .github/workflows/ci.yml
name: CI
on:
  push:
    branches: [main, develop]
  pull_request:
    branches: [main]
jobs:
  frontend:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: pnpm/action-setup@v4
      - uses: actions/setup-node@v4
        with:
          node-version: 20
          cache: pnpm
      - run: pnpm install
      - run: pnpm lint
      - run: pnpm format:check
      - run: pnpm build
      - run: pnpm test
  backend:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - run: cd src-tauri && cargo check
      - run: cd src-tauri && cargo clippy -- -D warnings
      - run: cd src-tauri && cargo test
```

- [ ] **步骤 2：创建 Release 工作流**

```yaml
# .github/workflows/release.yml
name: Release
on:
  push:
    tags: ["v*"]
jobs:
  build:
    strategy:
      matrix:
        platform: [macos-latest, ubuntu-latest, windows-latest]
    runs-on: ${{ matrix.platform }}
    steps:
      - uses: actions/checkout@v4
      - uses: pnpm/action-setup@v4
      - uses: actions/setup-node@v4
        with:
          node-version: 20
          cache: pnpm
      - run: pnpm install
      - run: pnpm tauri build
      - uses: tauri-apps/tauri-action@v0
        env:
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
```

- [ ] **步骤 3：创建 Stale 工作流**

```yaml
# .github/workflows/stale.yml
name: Close Stale Issues
on:
  schedule:
    - cron: "0 0 * * *"
jobs:
  stale:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/stale@v9
        with:
          stale-issue-message: "此 issue 已闲置 60 天，如仍需处理请评论。"
          days-before-stale: 60
          days-before-close: 7
```

- [ ] **步骤 4：Commit**

```bash
git add .github/workflows/
git commit -m "ci: add CI, release, and stale issue workflows"
```

---

### 任务 2：Issue 模板（FR-2）

**文件：**
- 创建：`.github/ISSUE_TEMPLATE/bug_report.yml`
- 创建：`.github/ISSUE_TEMPLATE/feature_request.yml`
- 创建：`.github/ISSUE_TEMPLATE/documentation.yml`
- 创建：`.github/ISSUE_TEMPLATE/question.yml`
- 创建：`.github/ISSUE_TEMPLATE/config.yml`

- [ ] **步骤 1：创建 Bug 报告模板**

```yaml
# .github/ISSUE_TEMPLATE/bug_report.yml
name: Bug 报告
description: 报告一个 bug
labels: ["bug"]
body:
  - type: textarea
    id: description
    attributes:
      label: 问题描述
      description: 清晰描述 bug 现象
    validations:
      required: true
  - type: textarea
    id: steps
    attributes:
      label: 复现步骤
      description: 详细描述复现步骤
    validations:
      required: true
  - type: textarea
    id: expected
    attributes:
      label: 期望行为
      description: 描述期望的正确行为
    validations:
      required: true
  - type: textarea
    id: environment
    attributes:
      label: 环境信息
      description: 操作系统、应用版本等
      placeholder: "OS: macOS 14 / App: v0.2.2"
    validations:
      required: true
```

- [ ] **步骤 2：创建功能请求、文档改进、问题咨询模板**

```yaml
# .github/ISSUE_TEMPLATE/feature_request.yml
name: 功能请求
description: 建议新功能
labels: ["enhancement"]
body:
  - type: textarea
    id: description
    attributes:
      label: 功能描述
    validations:
      required: true
  - type: textarea
    id: usecase
    attributes:
      label: 使用场景
    validations:
      required: true
  - type: textarea
    id: alternatives
    attributes:
      label: 替代方案
```

```yaml
# .github/ISSUE_TEMPLATE/documentation.yml
name: 文档改进
description: 报告文档问题或建议
labels: ["documentation"]
body:
  - type: textarea
    id: description
    attributes:
      label: 文档问题
    validations:
      required: true
  - type: input
    id: location
    attributes:
      label: 文档位置
      placeholder: "docs/product/manual/zh/03-dashboard.md"
```

```yaml
# .github/ISSUE_TEMPLATE/question.yml
name: 问题咨询
description: 提问或寻求帮助
labels: ["question"]
body:
  - type: textarea
    id: question
    attributes:
      label: 你的问题
    validations:
      required: true
```

- [ ] **步骤 3：创建模板配置（禁用空白 issue）**

```yaml
# .github/ISSUE_TEMPLATE/config.yml
blank_issues_enabled: false
contact_links:
  - name: 讨论
    url: https://github.com/jarvis/MultiAgents-Manager/discussions
    about: 一般性讨论请使用 Discussions
```

- [ ] **步骤 4：Commit**

```bash
git add .github/ISSUE_TEMPLATE/
git commit -m "docs: add GitHub issue templates (bug, feature, doc, question)"
```

---

### 任务 3：CHANGELOG + CONTRIBUTING + SECURITY（FR-6/7/8）

**文件：**
- 创建：`CHANGELOG.md`
- 创建：`CONTRIBUTING.md`
- 创建：`SECURITY.md`

- [ ] **步骤 1：创建 CHANGELOG.md**

```markdown
# Changelog

本文件记录 MultiAgents Manager 的所有版本变更，遵循 [Keep a Changelog](https://keepachangelog.com/) 格式。

## [Unreleased]

## [0.2.2] - 2026-07-08
### Added
- 资源管理看板双视图（按类型/按工具）
- 预设组一键应用/取消功能
- 兼容性检查对话框
- OpenClaw 第四工具支持
### Fixed
- 修复 TypeScript 未使用变量错误

## [0.1.0] - 2026-07-01
### Added
- 多 Agent 工具统一监控看板
- Skill/MCP/Plugin 三层映射架构
- 系统托盘预设菜单
- 状态变更桌面通知
- 终端快速跳转
```

- [ ] **步骤 2：创建 CONTRIBUTING.md**

```markdown
# 贡献指南

感谢你对 MultiAgents Manager 的关注！本文档帮助你快速参与开发。

## 开发环境

### 前置要求
- Node.js 20+
- pnpm 9+
- Rust 1.75+
- macOS / Linux / Windows

### 搭建步骤

\`\`\`bash
git clone https://github.com/jarvis/MultiAgents-Manager.git
cd MultiAgents-Manager
pnpm install
pnpm tauri:dev
\`\`\`

## 代码规范

- Rust: 遵循 `rustfmt` 默认格式，`cargo clippy` 无警告
- TypeScript: 遵循 Prettier + ESLint 配置
- 提交信息: 遵循 [Conventional Commits](https://www.conventionalcommits.org/)

### 提交信息格式

\`\`\`
<type>(<scope>): <description>

feat: 新功能
fix: Bug 修复
docs: 文档更新
refactor: 代码重构
test: 测试相关
chore: 构建/工具链
\`\`\`

## PR 流程

1. Fork 仓库并创建分支: `git checkout -b feat/your-feature`
2. 编写代码并确保测试通过: `cargo test && pnpm test`
3. 提交 PR，填写 PR 模板
4. 等待 CI 通过和代码审查

## 本地测试

\`\`\`bash
# 前端
pnpm check          # format + lint + build
pnpm test           # vitest

# 后端
cd src-tauri && cargo check
cd src-tauri && cargo test
cd src-tauri && cargo clippy -- -D warnings
\`\`\`
```

- [ ] **步骤 3：创建 SECURITY.md**

```markdown
# 安全策略

## 支持的版本

| 版本 | 支持状态 |
|------|---------|
| 0.2.x | ✅ 支持 |
| < 0.2 | ❌ 不支持 |

## 报告漏洞

如果你发现安全漏洞，请不要公开提交 Issue。

1. 发送邮件至: [维护者邮箱]
2. 或使用 GitHub Security Advisories: Settings -> Security -> Advisories

请在报告中包含：
- 漏洞描述和影响范围
- 复现步骤
- 建议的修复方案（如有）

## 披露时间线

- 收到报告后 48 小时内确认
- 7 天内评估并制定修复计划
- 修复发布后 90 天公开披露

## 安全原则

- 所有监控在本地完成，不向外传输数据
- 敏感路径（SSH 密钥、.env、凭据）不得被读取或暴露
- 安全扫描检查 skill 文件路径是否落在允许目录内，防止路径穿越
```

- [ ] **步骤 4：Commit**

```bash
git add CHANGELOG.md CONTRIBUTING.md SECURITY.md
git commit -m "docs: add CHANGELOG, CONTRIBUTING, and SECURITY"
```

---

### 任务 4：多语言用户指南（FR-3）

**文件：**
- 创建：`docs/product/guides/zh/getting-started.md` 等 5 篇 x 2 语言

- [ ] **步骤 1：创建中文指南**

每篇指南 >=500 字，覆盖一个主题。创建以下文件：

- `docs/product/guides/zh/getting-started.md` -- 安装、首次启动、基本界面
- `docs/product/guides/zh/session-monitoring.md` -- 会话看板使用、状态含义、通知配置
- `docs/product/guides/zh/resource-management.md` -- Skill/MCP/Plugin 安装、启用、禁用
- `docs/product/guides/zh/preset-groups.md` -- 预设组创建、应用、管理
- `docs/product/guides/zh/troubleshooting.md` -- 常见问题排查

每篇指南内容结构：

```markdown
# 快速开始

## 系统要求

- macOS 12+ / Ubuntu 20.04+ / Windows 10+
- 已安装至少一种支持的 AI 编程工具

## 安装

### macOS
1. 下载最新 DMG 文件
2. 拖拽到 Applications 目录
3. 首次启动时允许辅助功能权限（用于终端跳转）

## 首次启动

启动应用后，看板会自动扫描已安装的 AI 编程工具...
（继续扩展至 500+ 字）
```

- [ ] **步骤 2：创建英文指南**

将中文指南翻译为英文，内容保持一致：
- `docs/product/guides/en/getting-started.md`
- `docs/product/guides/en/session-monitoring.md`
- `docs/product/guides/en/resource-management.md`
- `docs/product/guides/en/preset-groups.md`
- `docs/product/guides/en/troubleshooting.md`

- [ ] **步骤 3：Commit**

```bash
git add docs/product/guides/
git commit -m "docs: add multilingual quick guides (zh/en, 5 topics)"
```

---

### 任务 5：用户手册（FR-5）

**文件：**
- 创建：`docs/product/manual/zh/01-introduction.md` 等 11 章 x 2 语言

- [ ] **步骤 1：创建中文用户手册（11 章）**

每章 >=500 字，含截图占位符 `![截图](../../assets/screenshots/zh/xxx.png)`：

1. `01-introduction.md` -- 项目介绍、功能概览
2. `02-installation.md` -- 安装步骤（三平台）
3. `03-dashboard.md` -- 看板界面详解
4. `04-sessions.md` -- 会话管理、状态说明
5. `05-resources.md` -- Skill/MCP/Plugin 管理
6. `06-presets.md` -- 预设组功能
7. `07-settings.md` -- 设置选项
8. `08-notifications.md` -- 通知配置
9. `09-shortcuts.md` -- 快捷键
10. `10-troubleshooting.md` -- 故障排查
11. `11-faq.md` -- 常见问题

- [ ] **步骤 2：创建英文用户手册**

将中文手册翻译为英文：`docs/product/manual/en/01-introduction.md` 等 11 章。

- [ ] **步骤 3：Commit**

```bash
git add docs/product/manual/
git commit -m "docs: add multilingual user manual (zh/en, 11 chapters)"
```

---

### 任务 6：Release notes + Screenshots 目录（FR-4/9）

**文件：**
- 创建：`docs/release-notes/v0.1.0.md`、`v0.2.0.md`、`v0.2.2.md`
- 创建：`assets/screenshots/zh/`、`assets/screenshots/en/` 目录

- [ ] **步骤 1：创建版本发布说明**

每个版本文档包含版本号、日期、新增/改进/修复/已知问题。

```markdown
<!-- docs/release-notes/v0.2.2.md -->
# v0.2.2 - 2026-07-08

## 新增
- 资源管理看板双视图（按类型/按工具）
- 预设组一键应用/取消
- 兼容性检查对话框
- OpenClaw 第四工具支持

## 修复
- TypeScript 未使用变量错误

## 已知问题
- Linux Wayland 不支持终端跳转
```

- [ ] **步骤 2：创建截图目录结构**

```bash
mkdir -p assets/screenshots/zh assets/screenshots/en
```

在目录中创建 `.gitkeep` 文件占位。实际截图待 UI 稳定后生成。

注意：`TEST/` 目录维持 `.gitignore` 忽略状态，不删除条目，不做处理。

- [ ] **步骤 3：Commit**

```bash
git add docs/release-notes/ assets/screenshots/
git commit -m "docs: add release notes and screenshot directory structure"
```

---

### 任务 7：ADR 架构决策记录（FR-10）

**文件：**
- 创建：`docs/engineering/architecture/adr/README.md`
- 创建：`docs/engineering/architecture/adr/001-adapter-pattern.md`
- 创建：`docs/engineering/architecture/adr/002-three-layer-symlink.md`
- 创建：`docs/engineering/architecture/adr/003-hook-priority-status.md`
- 创建：`docs/engineering/architecture/adr/004-react-query-vs-polling.md`

- [ ] **步骤 1：创建 ADR 索引**

```markdown
<!-- docs/engineering/architecture/adr/README.md -->
# 架构决策记录 (ADR)

本目录记录 MultiAgents Manager 的关键架构决策。

| 编号 | 标题 | 状态 |
|------|------|------|
| 001 | Adapter 模式实现多工具支持 | 已接受 |
| 002 | 三层符号链接映射架构 | 已接受 |
| 003 | Hook 优先 + notify + 轮询三重策略 | 已接受 |
| 004 | React Query 替代手动轮询 | 已接受 |
```

- [ ] **步骤 2：创建 4 个 ADR 文件**

每个 ADR 包含：状态、背景、决策、后果、替代方案。

```markdown
<!-- docs/engineering/architecture/adr/001-adapter-pattern.md -->
# ADR 001: Adapter 模式实现多工具支持

**状态**: 已接受
**日期**: 2026-07-05

## 背景
需要支持多个 AI 编程工具（Claude Code、Codex CLI、OpenCode、OpenClaw），
每个工具的进程发现、配置格式、会话解析方式不同。

## 决策
采用 Adapter 模式：每个工具实现 `AgentAdapter` trait，核心模块
通过 trait 接口与工具交互，不感知具体工具实现。

## 后果
- 正面：新增工具只需实现 trait，不改核心代码
- 负面：trait 设计需足够抽象，可能限制工具特有功能

## 替代方案
- 硬编码 if-else：代码膨胀快，拒绝
- 插件系统：过于复杂，MVP 阶段过度设计，拒绝
```

```markdown
<!-- docs/engineering/architecture/adr/002-three-layer-symlink.md -->
# ADR 002: 三层符号链接映射架构

**状态**: 已接受
**日期**: 2026-07-05

## 背景
Skill/MCP/Plugin 需要在统一仓库（SSOT）中管理，同时映射到各工具的配置目录。
需要支持工具级和子 Agent 级两层分配，且工具级禁用要自动断开子 Agent 链接。

## 决策
采用三层符号链接：Layer 1 (SSOT) -> Layer 2 (工具级) -> Layer 3 (子 Agent 级)。
Layer 3 指向 Layer 2 而非 Layer 1，工具级禁用自动断开所有子 Agent。

## 后果
- 正面：工具级禁用自动传播到子 Agent，无需额外清理逻辑
- 负面：Windows 不支持符号链接，需 Junction/copy 降级

## 替代方案
- 直接复制文件：版本不同步，浪费空间
- 单层映射：无法支持子 Agent 级独立分配
```

```markdown
<!-- docs/engineering/architecture/adr/003-hook-priority-status.md -->
# ADR 003: Hook 优先 + notify + 轮询三重状态检测策略

**状态**: 已接受
**日期**: 2026-07-05

## 背景
AI 编程工具的会话状态变更需要被及时捕获。不同工具的能力不同：
- Claude Code / Codex CLI 支持 Hook 事件（会话开始/结束/工具调用）
- OpenCode / OpenClaw 无 Hook，只能通过进程扫描 + 数据文件解析

单一策略（纯轮询）延迟高（3s+），纯 Hook 覆盖不全（无 Hook 的工具漏检）。

## 决策
采用三重策略，按优先级组合：
1. **Hook 事件**（最高优先级）：有 Hook 的工具，事件文件新鲜（<30s）时以事件为准
2. **notify 文件监听**：notify-rs 监听 Hook/进程事件文件变化，变化时立即触发刷新
3. **定时轮询**（兜底）：30s 轮询周期，捕获遗漏的状态变更

无 Hook 的工具降级为 notify + 进程扫描双策略。

## 后果
- 正面：状态变更延迟从 3s 降至 <1s（有 Hook 的工具），无 Hook 工具仍有兜底
- 负面：三重策略增加复杂度，需处理事件去重和新鲜度判断

## 替代方案
- 纯轮询：延迟高，用户体验差
- 纯 Hook：无法覆盖无 Hook 的工具
- WebSocket 推送：需要工具配合，MVP 阶段不现实
```

```markdown
<!-- docs/engineering/architecture/adr/004-react-query-vs-polling.md -->
# ADR 004: React Query 替代手动 setInterval 轮询

**状态**: 已接受
**日期**: 2026-07-08

## 背景
前端会话数据获取使用手动 `setInterval` 轮询（`useSessions` hook），存在以下问题：
- 竞态条件：多个轮询并发返回时可能覆盖较新数据
- 无缓存：页面切换后重新请求，无缓存命中
- 无自动刷新控制：后台标签页仍在轮询，浪费资源
- 样板代码多：loading/error 状态需手动管理

## 决策
引入 TanStack React Query 替代手动轮询：
- `useQuery` + `refetchInterval` 实现自动轮询
- 内置缓存/去重/后台刷新机制
- `refetchIntervalInBackground: false` 避免后台轮询
- Zustand 保留 UI 状态管理（侧边栏、主题），React Query 管理服务端数据

## 后果
- 正面：消除竞态条件，缓存命中时瞬时渲染，减少样板代码
- 负面：引入新依赖（~12KB gzip），需学习 React Query 概念
- 迁移策略：初期 Zustand + React Query 共存，逐步迁移

## 替代方案
- SWR：功能类似，但 React Query 生态更活跃，社区更大
- 手动优化 setInterval：治标不治本，竞态问题仍存在
- GraphQL 订阅：过度设计，Tauri IPC 不需要
```

- [ ] **步骤 3：Commit**

```bash
git add docs/engineering/architecture/adr/
git commit -m "docs: add architecture decision records (ADR 001-004)"
```

---

### 任务 8：commitlint + Dependabot + CODEOWNERS + PR 模板（FR-11/12/13/14）

**文件：**
- 创建：`commitlint.config.js`
- 创建：`.github/dependabot.yml`
- 创建：`.github/CODEOWNERS`
- 创建：`.github/PULL_REQUEST_TEMPLATE.md`
- 创建：`.husky/commit-msg`

- [ ] **步骤 1：安装 commitlint**

```bash
pnpm add -D @commitlint/cli @commitlint/config-conventional
```

- [ ] **步骤 2：创建 commitlint 配置**

```javascript
// commitlint.config.js
export default {
  extends: ["@commitlint/config-conventional"],
  rules: {
    "type-enum": [
      2,
      "always",
      ["feat", "fix", "docs", "style", "refactor", "perf", "test", "chore", "ci", "build", "revert"],
    ],
    "scope-case": [2, "always", "lower-case"],
  },
};
```

- [ ] **步骤 3：配置 commit-msg hook**

```bash
# .husky/commit-msg
npx --no -- commitlint --edit $1
```

- [ ] **步骤 4：创建 Dependabot 配置**

```yaml
# .github/dependabot.yml
version: 2
updates:
  - package-ecosystem: npm
    directory: /
    schedule:
      interval: weekly
      day: monday
    open-pull-requests-limit: 10
    labels: [dependencies, frontend]
    groups:
      minor-patch:
        update-types: [minor, patch]
  - package-ecosystem: cargo
    directory: /src-tauri/
    schedule:
      interval: weekly
      day: monday
    open-pull-requests-limit: 10
    labels: [dependencies, backend]
```

- [ ] **步骤 5：创建 CODEOWNERS**

```
# .github/CODEOWNERS
# 后端 Rust 代码
/src-tauri/  @backend-maintainers

# 前端 React 代码
/src/        @frontend-maintainers

# 文档与规格
/docs/       @docs-maintainers
/specs/      @docs-maintainers

# CI/CD 配置
/.github/    @devops-maintainers
```

注意：`@backend-maintainers` 等团队名需在 GitHub 仓库 Settings -> Teams 中先创建。个人项目可替换为 GitHub 用户名。

- [ ] **步骤 6：创建 PR 模板**

```markdown
<!-- .github/PULL_REQUEST_TEMPLATE.md -->
## 变更描述

<!-- 简要描述此 PR 做了什么 -->

## 变更类型

- [ ] 新功能（feat）
- [ ] Bug 修复（fix）
- [ ] 文档更新（docs）
- [ ] 代码重构（refactor）
- [ ] 测试（test）
- [ ] 工程化（chore）

## 测试

- [ ] `cargo test` 通过
- [ ] `pnpm test` 通过
- [ ] `pnpm check` 通过
- [ ] 手动验证通过

## 检查清单

- [ ] 代码已自审
- [ ] 相关文档已更新
- [ ] CHANGELOG 已更新（如有用户可见变更）
- [ ] 无安全风险

## 截图（如涉及 UI 变更）

<!-- 粘贴截图 -->
```

- [ ] **步骤 7：验证 commitlint 生效**

运行：`echo "bad message" | npx commitlint` 
预期：FAIL（不符合 Conventional Commits）

运行：`echo "feat: add new feature" | npx commitlint`
预期：PASS

- [ ] **步骤 8：Commit**

```bash
git add commitlint.config.js .husky/commit-msg .github/dependabot.yml .github/CODEOWNERS .github/PULL_REQUEST_TEMPLATE.md package.json
git commit -m "chore: add commitlint, dependabot, codeowners, and PR template"
```

---

### 任务 9：开发环境标准化（FR-15，可选）

**文件：**
- 创建：`.devcontainer/devcontainer.json`

- [ ] **步骤 1：创建 devcontainer 配置**

```json
{
  "name": "MultiAgents Manager",
  "image": "mcr.microsoft.com/devcontainers/rust:1",
  "features": {
    "ghcr.io/devcontainers/features/node:1": { "version": "20" }
  },
  "postCreateCommand": "pnpm install && cd src-tauri && cargo build",
  "customizations": {
    "vscode": {
      "extensions": [
        "rust-lang.rust-analyzer",
        "bradlc.vscode-tailwindcss",
        "esbenp.prettier-vscode"
      ]
    }
  }
}
```

- [ ] **步骤 2：Commit**

```bash
git add .devcontainer/
git commit -m "chore: add devcontainer config for standardized dev environment"
```

---

## 自检

**规格覆盖度：**
- FR-1 CI/CD -> 任务 1 ✓
- FR-2 Issue 模板 -> 任务 2 ✓
- FR-3 多语言指南 -> 任务 4 ✓
- FR-4 Release notes -> 任务 6 ✓
- FR-5 用户手册 -> 任务 5 ✓
- FR-6 CHANGELOG -> 任务 3 ✓
- FR-7 CONTRIBUTING -> 任务 3 ✓
- FR-8 SECURITY -> 任务 3 ✓
- FR-9 Screenshots -> 任务 6 ✓
- FR-10 ADR -> 任务 7 ✓
- FR-11 commitlint -> 任务 8 ✓
- FR-12 Dependabot -> 任务 8 ✓
- FR-13 CODEOWNERS -> 任务 8 ✓
- FR-14 PR 模板 -> 任务 8 ✓
- FR-15 devcontainer -> 任务 9 ✓
无遗漏。

**占位符扫描：** 指南和手册内容在步骤中给出了结构和示例，实际文字在执行时编写。配置文件含完整内容，无占位符。

**类型一致性：** commitlint 的 type-enum 与 CONTRIBUTING 中的提交信息格式一致。CODEOWNERS 路径与项目结构一致。PR 模板的测试项与 CI 工作流步骤一致。
