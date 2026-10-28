# MultiAgents-Manager Release 机制 + 自动更新 + CI 验证 设计

日期：2026-08-13
状态：已与用户逐节确认

## 1. 背景与目标

当前应用（Tauri 2 + React 19 + pnpm，`com.jarvis.multiagents-manager`，v0.2.2）已有：
- 本地打包脚本 `scripts/release.sh`（macOS aarch64 DMG + tar.gz → `release/`）、`scripts/release-version.mjs`（升版本 + 提交 + 打 tag + push）
- CI `ci.yml`（前端 lint/format/build/test + Rust check/clippy/test）、`release.yml`（tag push 时 tauri-action 三平台构建）、`stale.yml`
- updater 已接 `tauri-plugin-updater`，但 `tauri.conf.json` 的 pubkey/endpoint 是占位符
- 前端更新对话框已存在（`updater-dialog.tsx` / `use-updater.ts` / `lib/updater.ts`）

目标：
1. 建立专业 release 机制：tag push 触发 CI 自动构建 Windows exe 与 macOS DMG 并挂载到 GitHub Release，生成 Tauri 格式 `latest.json` 支撑自动更新
2. 打通自动更新（免费，用 Tauri minisign 密钥，不依赖 Apple 账号）
3. 增强 CI 验证流水线（路径过滤 + PR 真构建验证）
4. Release notes 沿用参考项目（cc-switch / CodexPlusPlus）惯例存仓库

参考项目（均已核对其实际实现）：
- **cc-switch**（farion1231/cc-switch）：Tauri 应用，tag push 触发；显式分 job 构建 → `softprops/action-gh-release` 发布 → 独立 `assemble-latest-json` job 组装 Tauri 格式 `latest.json`；release notes 存 `docs/release-notes/vX.Y.Z-zh.md`（多语种）；updater 端点双通道（自家 CDN + GitHub `releases/latest/download/latest.json`）；macOS 完整 Apple 签名 + 公证。
- **CodexPlusPlus**（BigPizzaV3/CodexPlusPlus）：`pr-build.yml` 在 PR 时真构建产物上传 artifact；`release-assets.yml` 在 `release: published` 后补构建挂载；同样有独立 latest.json job（electron 格式）。

本设计与参考项目的一致点：**不设应用内发布工作台**；发布机制全部在 git + GitHub 上完成；应用内只有更新检查对话框。

## 2. 关键决策（已与用户确认）

| 维度 | 决策 |
|---|---|
| 方案 | 方案二：cc-switch 结构（显式构建 → softprops 发布 → 独立 latest.json job），tag push 触发 |
| 平台 | Windows（x64，NSIS）+ macOS（universal，aarch64 + x86_64 一包） |
| Linux | 不发布（无本地机器验证；ci.yml 后端检查仍在 ubuntu 跑）；PR 构建验证 job 用 ubuntu runner（仅验证，产物不进 release） |
| macOS Apple 签名 | 不做（用户暂无 Developer 账号/不想付费）。流水线留**条件闸门**：配了 `APPLE_*` Secrets 就走完整签名+公证，否则跳过 |
| Tauri 自动更新 | 打通。minisign 密钥，免费；端点用 GitHub 单通道 |
| Release notes | 存仓库 `docs/release-notes/vX.Y.Z.md`（中文先上），CI 写入 Release body + latest.json |
| 应用内 | 不新增发布工作台；增强现有更新对话框（展示 notes，基本零改动） |
| 凭证 | 无应用内 API 调用需求（工作台已砍），发布全部走 git push + GitHub Secrets |
| 手动补挂产物 | 不写代码，用 `gh release upload` / GitHub 网页，文档指引 |

## 3. CI 发布流水线（重写 `.github/workflows/release.yml`）

### 3.1 触发与守卫

```yaml
name: Release
on:
  push:
    tags: ["v*"]
permissions:
  contents: write
concurrency:
  group: release-${{ github.ref_name }}
  cancel-in-progress: true
```

- `environment: release`（可选，未来可在此加人工审批门）

### 3.2 构建 job（matrix）

| runner | 目标 | 命令 | 产物（重命名规整后） |
|---|---|---|---|
| `windows-latest` | 默认 x86_64 | `pnpm tauri build --bundles nsis` | `MultiAgents-Manager-{v}-Windows-x64-setup.exe` + `.sig` |
| `macos-latest` | `universal-apple-darwin` | 先 `rustup target add aarch64-apple-darwin x86_64-apple-darwin`，再 `pnpm tauri build --bundles dmg --target universal-apple-darwin` | `MultiAgents-Manager-{v}-macOS.dmg` + `.tar.gz`（updater 用）+ `.sig` |

通用步骤：checkout → pnpm/node/rust 设置 → `pnpm install --frozen-lockfile` → 构建 → 规整产物 → `upload-artifact`。

**Tauri minisign 签名（始终开启）**：构建时注入 `TAURI_SIGNING_PRIVATE_KEY` / `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`（Secret）。签名密钥格式兼容处理参考 cc-switch 的 "Prepare Tauri signing key" step（原始两行 / base64 包裹 / 单行 base64 三种格式都可识别）。无密钥时构建失败（自动更新依赖签名，属预期硬约束）。

**Apple 签名（条件闸门，当前为占位）**：macOS job 首步检查 `APPLE_CERTIFICATE` 等 Secret；当前未配置 → 打印 warning 并跳过，构建未签名 DMG（Gatekeeper 首次打开提醒为预期行为）。**完整签名+公证流程（导入证书到临时 keychain → 签名 → notarize → 三重验证）暂不作为代码落地**，写入 `docs/operations/RELEASE.md` 作为将来启用指引——避免把未经实测的签名代码上线；有账号时再作为独立小任务激活并用真实 release 实测。

### 3.3 发布 job（`publish-release`）

```
needs: release
```
- `actions/download-artifact`（merge-multiple）合并各平台产物
- `softprops/action-gh-release`：`tag_name: ${{ github.ref_name }}`，上传全部产物
- **Release body 自动拼接**（cc-switch 风格）：模板头部 + 下载清单 + 更新内容
  - 下载清单固定生成（产物文件名确定，模板化即可）：`macOS: MultiAgents-Manager-{v}-macOS.dmg（推荐）`、`Windows: MultiAgents-Manager-{v}-Windows-x64-setup.exe` 等，带一句说明
  - 更新内容从 `docs/release-notes/v{ver}.md` 读取（`body_path` 拼接在下载清单之后）
  - 效果：GitHub Release 页面上方是"本次更新内容"，下方是各平台下载包
- `prerelease: false`（保证 `releases/latest` 指向它，updater 端点依赖）

### 3.4 manifest job（`assemble-latest-json`）

```
needs: publish-release
```
- `gh release download "$TAG" --dir dl` 拉回刚发布的产物
- 遍历 `dl/*.sig`，按文件名匹配平台，组装 Tauri 格式：

```json
{
  "version": "0.2.3",
  "notes": "<docs/release-notes/v0.2.3.md 的内容>",
  "pub_date": "<ISO 8601>",
  "platforms": {
    "darwin-aarch64": { "signature": "<...>", "url": "https://github.com/jarvislee90s-dot/MultiAgents-Manager/releases/download/v0.2.3/MultiAgents-Manager-0.2.3-macOS.tar.gz" },
    "darwin-x86_64": { "signature": "<...>", "url": "<同上 universal 包>" },
    "windows-x86_64": { "signature": "<...>", "url": "https://github.com/jarvislee90s-dot/MultiAgents-Manager/releases/download/v0.2.3/MultiAgents-Manager-0.2.3-Windows-x64-setup.exe" }
  }
}
```
- `darwin-aarch64` 与 `darwin-x86_64` 指向同一 universal `.tar.gz`（cc-switch 同款做法）
- `gh release upload "$TAG" latest.json --clobber`

## 4. 自动更新落地

1. 一次性生成密钥：`pnpm tauri signer generate -w ~/.mam/tauri.key`（可设密码或留空）→ 得到私钥文件 + 公钥字符串
2. `src-tauri/tauri.conf.json`：
   - `plugins.updater.pubkey` → 真实公钥
   - `plugins.updater.endpoints` → `["https://github.com/jarvislee90s-dot/MultiAgents-Manager/releases/latest/download/latest.json"]`
3. GitHub Secrets（发布前配一次）：`TAURI_SIGNING_PRIVATE_KEY`（私钥 base64）、`TAURI_SIGNING_PRIVATE_KEY_PASSWORD`（若生成时设了密码）
4. `lib.rs` 已处理"无签名密钥时不注册 updater"，本地 `tauri dev` 不受影响
5. 验证闭环：CI 签名 → `.sig` 进 latest.json → 应用 updater 检查端点 → 校验签名 → 更新；对话框展示 notes

## 5. Release notes 约定

- 新目录 `docs/release-notes/`，每版 `vX.Y.Z.md`（中文先上；未来如需双语仿 cc-switch 加 `-en.md` 等）
- **该文件只写"更新内容"**；下载清单由 `publish-release` job 模板化自动生成（见 §3.3），发布人无需手写
- 发布流程（顺序由 `release-version.mjs` 约束：要求工作树干净、只提交版本 4 文件）：
  1. 写 `docs/release-notes/vX.Y.Z.md`（本次更新内容），单独提交
  2. 跑 `pnpm release:version`，选择同一版本号 → 它负责改版本号、提交 4 个版本文件、打 tag、push
  3. CI 自动构建发布（tag 指向的提交已含 notes 文件）
- `publish-release` job 把该文件内容拼进 Release body；`assemble-latest-json` 把内容写入 latest.json 的 `notes`
- 现有 `scripts/release.sh` 生成的 notes 模板位置从 `release/release-notes-v{ver}.md` 对齐到 `docs/release-notes/v{ver}.md`（小改，保持单一路径）

## 6. CI 验证流水线增强（修改 `.github/workflows/ci.yml`）

1. **路径过滤**：`dorny/paths-filter`。前端 job 仅在 `src/**`、`tests/**`、`index.html`、`package.json`、`pnpm-lock.yaml`、`vite.config.ts`、`.github/workflows/**` 等变化时运行；后端 job 仅在 `src-tauri/**`、`rust-toolchain.toml`、`.github/workflows/**` 变化时运行（push 到 main 时仍全跑，保缓存）
2. **PR 真构建验证 job**（对齐 CodexPlusPlus 的 pr-build）：在 `pull_request` 与 `push` 到 main 时，`ubuntu-latest` 上装好 Linux 系统依赖（libwebkit2gtk-4.1 / libgtk-3 / librsvg2 等，参考 cc-switch 的依赖清单）后跑完整 `pnpm tauri build`，上传 release 二进制为 artifact。验证"干净环境能完整打包"在合并前完成
   - 该 job 的产物是验证用途，不进 GitHub Release，不发布 Linux

## 7. 应用内改动清单

- `src-tauri/tauri.conf.json`：pubkey + endpoint（唯一必要改动）
- 前端：核对 `updater-dialog.tsx` 已渲染 `latest.json` 的 `notes`；如已渲染则零改动
- 无新增 tab、无新增 Rust 命令（发布工作台已砍）
- 新增 `docs/` release 指南：一次性密钥设置、Secrets 配置、`gh release upload` 手动补挂用法、发布步骤清单

## 8. 错误处理与回滚

- **发布失败**：修复后需重打新 tag（Git tag 不可变，避免强制重打同 tag）；补丁版即可
- **latest.json 与 release 更新**：`--clobber` 支持重复上传；`assemble-latest-json` 可在发布后手动重跑（`workflow_dispatch` 建议加上）
- **手动补挂产物**：`gh release upload <tag> <files>` 或 GitHub 网页
- **Apple 签名缺失**：预期行为，文档注明 Gatekeeper 首次打开提醒
- **密钥泄露**：`pnpm tauri signer generate` 重新生成，替换 Secret 与 pubkey，旧版本将无法自动升级（需引导重新下载）

## 9. 验证策略

1. **本地 macOS**：`pnpm tauri build --bundles dmg --target universal-apple-darwin`，确认 DMG + tar.gz + `.sig` 产出、`createUpdaterArtifacts` 生效
2. **本地 Windows**：`pnpm tauri build --bundles nsis`，确认 setup.exe + `.sig`
3. **CI**：PR 构建验证 job 通过
4. **发布后**：访问 `https://github.com/jarvislee90s-dot/MultiAgents-Manager/releases/latest/download/latest.json` 返回合法 JSON 且签名/URL 正确；一台 macOS 上装旧版 → 新版发布后 updater 检测到并更新；更新对话框显示 notes

## 10. 一次性设置清单（首次发布前）

1. `pnpm tauri signer generate -w ~/.mam/tauri.key` → 公钥写进 `tauri.conf.json`，私钥 base64 配 Secret
2. 配 Secrets：`TAURI_SIGNING_PRIVATE_KEY`（必填）、`TAURI_SIGNING_PRIVATE_KEY_PASSWORD`（可选）、`APPLE_*`（暂空，将来补）
3. 建 `docs/release-notes/` 目录
4. 推送一次 `v0.2.3` tag 走通端到端

## 11. 范围外（Out of scope）

- Linux 发布产物（保留 ci.yml ubuntu 检查；PR 验证用 ubuntu 仅验证不进 release）
- Apple 签名/公证（条件闸门已就位，当前关闭）
- 应用内发布工作台 / 手动上传界面（已砍，与参考项目一致）
- updater 端点双通道 CDN（当前 GitHub 单通道即可，未来加 `https://...latest.json` 另一条）
- 多语种 release notes（先中文，结构支持后加）
