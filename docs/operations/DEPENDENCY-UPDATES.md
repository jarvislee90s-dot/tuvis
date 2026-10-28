# 依赖升级与月度批处理指南

> 适用对象：需要处理 dependabot PR、或改动依赖相关配置的人（含 AI agent）。
> 相关文件：`.github/dependabot.yml`、`.github/workflows/ci.yml`、`scripts/check-tauri-parity.mjs`、`package.json`（`engines`）、`src-tauri/Cargo.toml`（`rust-version`）。

## 1. 更新是怎么来的

| 事实 | 说明 |
|---|---|
| 节奏 | `schedule.interval: monthly` = **每月 1 日**运行一次 |
| 改配置会立刻触发 | 修改 `.github/dependabot.yml` 并合并后，dependabot 会立即重新检查一次（2026-10-03 实证：16:45 合并配置，16:47–16:52 就开出了 #102–#105） |
| 安全更新不受节奏约束 | `schedule`、`open-pull-requests-limit`、`multi-ecosystem-groups` 都只作用于 **version updates**；安全更新随时单独开 PR，需要及时处理 |
| 一 PR 覆盖两个生态 | `multi-ecosystem-groups.deps` 把 npm 与 cargo 合进同一个 PR —— 这是为了让 tauri 的 Rust crate 与 `@tauri-apps/*` NPM 包天然同批（见 §4） |
| 手动触发检查 | Insights → Dependency graph → Dependabot → **Check for updates**（改完配置想立刻验证时用） |

## 2. 门禁与分支保护

`ci.yml` 共五个 job，其中 **4 个是必需检查**（main 的分支保护）：

| 检查 | 内容 | 必需 |
|---|---|---|
| `Detect changed areas` | paths-filter 判定跑哪些 job | 否 |
| `Tauri version parity` | 只读两个锁文件，约 10 秒；Rust crate ↔ NPM 包 major.minor 必须相等 | ✅ |
| `Frontend Checks` | `pnpm install --frozen-lockfile` / lint / format:check / build / test | ✅ |
| `Backend Checks` | cargo fmt / check / clippy `-D warnings` / test + `x86_64-pc-windows-gnu --all-targets --features marker-helper` 交叉 check+clippy | ✅ |
| `Build verification` | `pnpm tauri build --no-bundle` | ✅ |

关键语义（均出自 GitHub 官方文档）：

- **`skipped` 也算通过**：必需检查的状态只要是 `successful` / `skipped` / `neutral` 就不阻塞合并。所以只改文档的 PR（前端/后端 job 全 skipped）照样能合。
- **`strict`（Require branches to be up to date）已开启**：分支必须与 main 同步才能合。含义是**每次合的都是「CI 在当时的 main 上验过的合并结果」**——这正是让"月度批处理"安全的机制，代价是每合一个 PR，其余 PR 会变成 `behind`，需要先更新分支。
- `enforce_admins: false`：仓库所有者保留人工绿色通道（真需要时可绕过门禁）。

## 3. 月度批处理流程

```bash
# 1. 列出待处理 PR，按 CI 结论分类
gh pr list --state open --json number,title,statusCheckRollup

# 2. 全绿的：逐个合并；每合一个，其余会变 behind，先更新分支再等新 head 绿
gh pr merge <N> --squash
gh api -X PUT repos/jarvislee90s-dot/tuvis/pulls/<下一个N>/update-branch
#    等新 head 的必需检查全绿（约 10–15 分钟）后再合下一个

# 3. tauri 两半（若 parity 报「成对不一致」）：必须同批合，见 §4

# 4. 红的：按 §4 的症状表判断——需要写迁移的单独排期，不要在批处理里现场写

# 5. 收尾：验证 main 自身
gh workflow run ci.yml --ref main

# 6. 在本文档 §7「批次记录」追加一行
```

## 4. 症状 → 处置

| 症状 | 原因 | 处置 |
|---|---|---|
| `Tauri version parity` 红，输出里成对出现「Rust 侧 ≠ NPM 侧」 | 同一批 tauri 升级被拆成两个 PR（npm 侧 / cargo 侧各一半），单独合任何一个都必红 | 两个 PR **同批合**：先合一个 → 对另一个 `update-branch` → 等 parity 转绿 → 再合 |
| `Tauri version parity` 红，只有一对不一致且是 patch 之外的漂移（如 `opener 2.6.0 ≠ 2.7.0`） | `@tauri-apps/*` 的 `^2` 这类松范围在 rebase/重解析锁文件时漂到了更新的 minor | 要么把 NPM 侧锁回与 Rust 相同的 minor，要么把 Rust 侧升到同一 minor（两边必须一致） |
| `Backend Checks` 红：`error[E…]` 编译错误 | major 升级带来的 API 破坏（历史案例：`rand 0.10` 删除 `thread_rng`/`RngCore`；`windows 0.61` 句柄指针化；`sysinfo 0.39` 删除 `RefreshKind::new`） | 立"迁移任务"单独做；E0599 这类"方法找不到"会**掩盖后续类型错误**（windows 那次 CI 报 16 个、实际 21 个），必须本地 `cargo check` 迭代到干净 |
| `Frontend Checks` 红：`typescript-eslint does not support TS X` | ESLint 工具链尚未支持该 TS 大版本（上游阻塞） | 对该依赖 `@dependabot ignore this major version`，等上游支持后 `unignore` |
| `Build verification` 红：`tsconfig.json(...) TS5102: Option 'baseUrl' has been removed` | TS 7 移除 `baseUrl` | 迁移 `tsconfig.json` 的 `baseUrl` + `paths` 写法（后端/前端构建都要验） |
| PR 显示 `behind` / 合并按钮灰 | `strict` 要求分支最新 | `gh api -X PUT repos/jarvislee90s-dot/tuvis/pulls/<N>/update-branch` |
| 改完 `dependabot.yml` 后**一个 PR 都不来** | 配置不合法（会让**所有**更新静默停摆） | Insights → Dependency graph → Dependabot 看有没有报错；确认写错就立刻回滚该文件 |

## 5. dependabot 评论命令（在 PR 里发评论）

| 命令 | 作用 |
|---|---|
| `@dependabot rebase` | 重新基于最新 main 生成分支（会重跑 CI） |
| `@dependabot recreate` | 重建 PR（**覆盖**对该 PR 分支的任何手工改动） |
| `@dependabot ignore this major version` | 关闭本 PR，并且不再提这个大版本（直到你手动升或 `unignore`） |
| `@dependabot ignore this minor version` / `… patch version` | 同上，作用于更小粒度 |
| `@dependabot ignore this dependency` | 完全不再为这个依赖开 PR |
| `@dependabot ignore <依赖名> major version` | 只忽略指定依赖的该大版本（组 PR 里按包裁剪时用这个） |
| `@dependabot unignore <依赖名>` / `@dependabot unignore *` | 清除已记录的 ignore 条件并重开 PR |
| `@dependabot show <依赖名> ignore conditions` | 列出该依赖当前的全部 ignore 条件 |

> 组 PR 里想"先拿一部分"：对不打算现在做的包逐个 `@dependabot ignore <依赖名> major version`，dependabot 会重开一个不含它们的 PR。

> ⚠️ **0.x 包的陷阱**：semver 里 `0.x` 的 major 就是 `0`，所以对 0.x 依赖发
> `@dependabot ignore <包> major version` 等于**忽略该依赖的全部更新**（不只是当前这个次版本）。
> 只想忽略某个次版本（例如 `sysinfo 0.39`）应当用 `@dependabot ignore <包> minor version`。
> 已经这样忽略过的，等对应升级自己落地后记得 `@dependabot unignore <包>` 恢复提醒。

## 6. 环境下限（不要凭感觉写）

| 下限 | 声明位置 | 为什么是这个值 |
|---|---|---|
| Node.js **≥ 22.22.2** | `package.json` → `engines.node` | `jsdom 30` 的 engines 要求（测试环境）；CI 用 `node-version: 22` 浮动到最新 22.x 才恰好满足 |
| Rust **≥ 1.89** | `src-tauri/Cargo.toml` → `rust-version` | 代码实际用到 `File::unlock`（1.89 稳定，`src/linker/mod.rs`）与 `u64::is_multiple_of`（1.87 稳定，`src/services/pet/import.rs`）；依赖侧底线更低（`sha2 0.11` 要求 1.85） |
| pnpm **11** | CI `pnpm/action-setup` | 与锁文件版本一致 |

声明 MSRV 的附带收益：会激活 clippy 的 `incompatible_msrv`（默认 allow，一旦声明即 warn），**以后再误用超版本 API 会在 CI 直接暴露**。2026-10-03 就是这么发现真实 MSRV 是 1.89 而不是 1.85 的。

## 7. 批次记录

- **2026-10-03**：季度改月度 + 加 `Tauri version parity` 门禁 + 删自动合并工作流（PR #101）；#86+#91（tauri 2.12 两半）成对合并；#87 zstd / #88 sha2 / #92 @types/node / #93 eslint / #94 react-i18next / #95 jsdom 逐个合并；#89 rand 与 #90 windows 因需迁移代码，改由 #99 / #100 带迁移重做后合并。当日 main 五项门禁全绿。
- **2026-10-04（同日续）**：把 dependabot 改成"单一多生态组"（PR #107）后，配置改动**立刻**触发了重新分组：原先的
  #102–#105 被自动关闭，改开成一个跨生态的组 PR——**实测证明了结构性修复有效**（组 PR 里 tauri 两半同批，
  `Tauri version parity` 直接绿，不再出现"两半拆分、单独必红"）。该组 PR 的处置链：
  ① `typescript 7.x` 被 upstream（typescript-eslint）卡住 → ignore；
  ② 组 PR 三处红的真正根因是 `tsconfig.json` 的 `baseUrl`（TS 6 弃用 TS5101 / TS 7 移除）→ 单独 PR #109 修掉；
  ③ 前端工具链 9 个 major（TS 6、vite 8、vitest 5、msw 3、jest-dom 7、@vitejs/plugin-react 6、@eslint/js 10 等）
  **无需改代码**即通过；
  ④ cargo 侧 9 个 major 里 8 个（windows 0.62 / rusqlite 0.40 / zip 8 / toml 1.1 / toml_edit 0.25 / dirs 7 /
  winreg 0.55 / junction 2.1）同样无需改代码；
  ⑤ 只有 `sysinfo 0.32→0.39` 需要迁移（0.33+ 删除 `RefreshKind::new`/`ProcessRefreshKind::new`）→ 因 **0.32 没有
  `nothing()`**，无法"先迁移后 bump"，故按 #99/#100 的模式拆成独立 PR #111（bump + 6 文件 9 处改名同批）。
- **2026-10-04（收口）**：组 PR #110 在处置链后与 main 冲突，且 dependabot 对其 rebase/recreate **连续 35 分钟无响应**
  （PR 停在 `dirty`）→ 按其内容在当前 main 上重建为 **PR #112** 并合并（cargo 8 个 major + 前端 13 项），#110 随后关闭。
  当日最终态：`Tauri version parity` 门禁在组 PR 上直接绿（两半同批），前端与 cargo 的 major 全部落地，
  tsconfig 的 `baseUrl` 已移除（为 TS 7 铺路），sysinfo 迁移随 #111 落地。
- **待办**：见 §8。

## 8. 已知缺口

- **`release.yml` 的平台矩阵**：PR 阶段只验 Ubuntu + `windows-gnu` **只编译不链接**；macOS universal 与 Windows MSVC 只在打 tag 时构建。批处理后建议 `gh workflow run release.yml` 干跑一次。
- **majors 无法再用 PR 隔离**：多生态组把大版本与小版本放进同一个 PR（换来"两半天然同批"）。代价用 §5 的 ignore 命令补偿；若将来想把 majors 拆回独立 PR，需要先验证"同一 ecosystem+directory 拆两条条目 + 条目级 `exclude-patterns`"是否被 GitHub 侧校验接受（当前无公开先例）。
- **安全更新不参与分组与节奏**，需要随时单独处理。
- 本仓库不使用自动合并（`allow_auto_merge: false`，每次合并都需人工知情）。
- **已知 flaky 测试**：`tests/mobile/SessionDetail.test.tsx` 的「点加载更早后保持阅读位置（顶部插入量补偿）」
  在新前端工具链（vitest 5 / @vitejs/plugin-react 6 / jest-dom 7）下**偶发失败**：
  `AssertionError: expected 8000 to be 4000`（同一份代码重跑即绿；2026-10-04 实测同一 sha 一次失败、一次通过）。
  碰到时先 `gh run rerun <run-id> --failed` 再判断，别急着回滚依赖；根治方向是让滚动补偿的 effect 幂等。
- 组 PR 里若含当天吃不下的大版本，**会把同批的小版本一起压住**（一个更新只能属于一个组）；用 §5 的 ignore 命令裁剪，
  或在月度会话里直接排期迁移。
- `sysinfo` 曾被以 `ignore ... major version` 忽略（对 0.x 等于全忽略）；它的迁移由 PR #111 落地后，
  应在对应 PR 下 `@dependabot unignore sysinfo` 恢复后续更新提醒。
