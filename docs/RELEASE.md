# Release 发布指南

## 一次性配置（首次发布前，只做一次）

### 1. 生成签名密钥
```bash
mkdir -p ~/.tuvis
pnpm tauri signer generate -w ~/.tuvis/tauri.key   # 密码直接回车留空
```
把打印的 `public key:` 公钥填入 `src-tauri/tauri.conf.json` 的 `plugins.updater.pubkey`。

### 2. 配置 GitHub Secrets
仓库 Settings → Secrets and variables → Actions → New repository secret：
- `TAURI_SIGNING_PRIVATE_KEY`：`~/.tuvis/tauri.key` 文件内容（base64）→ 填之前先 base64 编码：
  ```bash
  base64 < ~/.tuvis/tauri.key | tr -d '\n' | pbcopy
  ```
- `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`：仅当生成时设了密码才需要

（Apple 签名 Secrets 暂不需要；见下文"Apple 签名"）

## 每次发版步骤

0. **中英对照检查**：按 `AGENTS.md`「发版中英对照纪律」核对全部双语资产（前端 locale、双语
   README、用户手册 zh/en），本轮中文改动的英文侧未对齐先补齐，再进入下面步骤
1. 写更新说明：`docs/release-notes/vX.Y.Z.md`（只写更新内容）
2. 提交：`git add docs/release-notes/vX.Y.Z.md && git commit -m "docs: add release notes vX.Y.Z"`
3. 发版：`pnpm release:version` → 选择同一版本号 → 确认后自动提交版本文件、打 tag、push
4. 到 GitHub 仓库 Actions 页看 Release 流水线进度（构建 → 发布 → latest.json 三段）
5. 完成后到 Releases 页核对：正文含更新内容 + 下载清单，产物全部挂载

## 用户如何下载/更新
- GitHub Releases 页面（`https://github.com/jarvislee90s-dot/tuvis/releases`）：下载对应平台的 dmg / exe
- 已安装旧版的应用：启动时自动检测更新并提示（自动更新走 `latest.json`，无需手动下载）
- 自动更新端点：`https://github.com/jarvislee90s-dot/tuvis/releases/latest/download/latest.json`

## 手动补挂产物（个别平台构建失败时）
```bash
gh release upload vX.Y.Z ./path/to/installer.exe --repo jarvislee90s-dot/tuvis
```

## 失败恢复
- 构建/发布失败：修复后**打一个新 tag**（如 v0.2.4），不要重打同 tag
- `latest.json` 没生成或要重跑：Actions 页对 `assemble-latest-json` job 点 "Re-run"

## Apple 签名（将来启用）
当前发布的是未签名 DMG，macOS 首次打开有 Gatekeeper 安全提示（预期）。
启用完整签名需要 Apple Developer Program（$99/年）。启用时参考 cc-switch 的 release.yml：
1. 在 `release.yml` 的 macOS job 加入：把 `APPLE_CERTIFICATE`（.p12 base64）导入临时 keychain → `security set-key-partition-list` → 导出 `APPLE_SIGNING_IDENTITY`
2. `pnpm tauri build` 的 macOS 分支用 `--config src-tauri/tauri.conf.json`（保持签名环境变量），对 `universal-apple-darwin` 产物签名
3. `xcrun notarytool submit` 公证 DMG → `xcrun stapler staple`
4. 验证：`codesign --verify --deep --strict` + `spctl -a -t exec`
配置 Secrets：`APPLE_CERTIFICATE`、`APPLE_CERTIFICATE_PASSWORD`、`APPLE_ID`、`APPLE_PASSWORD`（App-specific password）、`APPLE_TEAM_ID`、`KEYCHAIN_PASSWORD`。
建议启用时作为独立任务并在真实 release 上实测。
