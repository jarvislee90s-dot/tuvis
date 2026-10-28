# Release 机制 + 自动更新 + CI 验证 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 建立 tag-push 驱动的 GitHub 发布流水线（CI 自动构建 Windows NSIS + macOS universal DMG 并挂载到 GitHub Release），打通 Tauri 自动更新，增强 CI 验证。

**Architecture:** 采用 cc-switch 结构——`release.yml` 三阶段：矩阵构建（Windows/macOS，Tauri minisign 签名）→ `publish-release`（softprops 挂载 + 自动拼 body：下载清单 + 仓库 notes 文件）→ `assemble-latest-json`（从 release 资产组装 Tauri 格式 manifest 挂回）。`ci.yml` 加路径过滤与 PR 真构建验证。release notes 存仓库 `docs/release-notes/`。

**Tech Stack:** GitHub Actions、Tauri 2、Rust、pnpm、Node 20、`softprops/action-gh-release@v2`、`dorny/paths-filter`。

**Spec:** `docs/engineering/specs/2026-08-13-release-system-design.md`（本计划从 spec 立论；执行者需同时读 spec）

## Global Constraints

- 仓库：`jarvislee90s-dot/MultiAgents-Manager`（public）
- 包管理器 pnpm，release 流水线用 `pnpm install --frozen-lockfile`
- Node 20（`actions/setup-node` + `cache: pnpm`）；Rust `dtolnay/rust-toolchain@stable`
- `pnpm tauri build` 自带前端构建（`beforeBuildCommand: pnpm build` 已配）
- **不发布 Linux** 产物；PR 构建验证用 ubuntu runner，产物仅作 artifact 不进 release
- Tauri minisign 签名**始终开启**（release 构建硬依赖 Secret `TAURI_SIGNING_PRIVATE_KEY`，缺失则构建失败）
- Apple 签名**不做**：macOS job 只检测并警告跳过；完整流程仅在 `docs/operations/RELEASE.md` 记录
- Release notes 单路径：`docs/release-notes/v{ver}.md`（中文）
- `prerelease: false`（updater 端点依赖 `releases/latest`）
- updater 端点单通道：`https://github.com/jarvislee90s-dot/MultiAgents-Manager/releases/latest/download/latest.json`
- 产物命名固定：`MultiAgents-Manager-{ver}-macOS.dmg` / `-macOS.tar.gz` / `-Windows-x64-setup.exe`（+ 各 `.sig`）
- 执行用指令（验证 YAML）：`ruby -e "require 'yaml'; YAML.load_file('<path>')"`

---

### Task 1: 生成 Tauri 签名密钥并写入 tauri.conf.json

**Files:**
- Modify: `src-tauri/tauri.conf.json`

**Interfaces:**
- Consumes: 用户手动生成的密钥（Task 1 内完成）
- Produces: `tauri.conf.json` 中真实的 `plugins.updater.pubkey` 与 `endpoints`；后续 Task 2-4 的流水线依赖该配置语义

- [ ] **Step 1: 手动生成签名密钥（用户在终端执行）**

Run:
```bash
mkdir -p ~/.mam
pnpm tauri signer generate -w ~/.mam/tauri.key
```
Expected: 终端提示输入密码时**直接回车（留空）**；生成 `~/.mam/tauri.key`（私钥）与 `~/.mam/tauri.key.pub`（公钥），并打印一行 `public key:` 开头的公钥。**把打印的公钥字符串复制下来**（它是 base64，以 `dW50cnVzdGVkIGNvbW1lbnQ6` 开头）。
> 若交互式卡住：`pnpm tauri signer generate --help` 查看该版本是否支持 `--password` 参数；不支持就留空密码。私钥文件勿提交进 git、勿外发。

- [ ] **Step 2: 更新 tauri.conf.json 的 pubkey 与 endpoints**

将 `src-tauri/tauri.conf.json` 的 `plugins.updater` 段改为（把 `REPLACE_WITH_REAL_PUBKEY` 换成 Step 1 复制的公钥字符串）：

```json
    "updater": {
      "pubkey": "REPLACE_WITH_REAL_PUBKEY",
      "endpoints": [
        "https://github.com/jarvislee90s-dot/MultiAgents-Manager/releases/latest/download/latest.json"
      ],
      "windows": {
        "installMode": "passive"
      }
    }
```

- [ ] **Step 3: 校验配置合法**

Run: `python3 -c "import json; d=json.load(open('src-tauri/tauri.conf.json')); print(d['plugins']['updater']['pubkey'][:20], d['plugins']['updater']['endpoints'])"`
Expected: 打印真实 pubkey 前缀（非 `__TAURI_UPDATER_PUBKEY__`）与 GitHub endpoints 数组。

- [ ] **Step 4: 提交**

```bash
git add src-tauri/tauri.conf.json
git commit -m "chore: wire tauri updater pubkey and GitHub endpoint"
```

---

### Task 2: 重写 `.github/workflows/release.yml`（cc-switch 三段式）

**Files:**
- Modify (overwrite): `.github/workflows/release.yml`

**Interfaces:**
- Consumes: Task 1 的签名密钥 Secret；`docs/release-notes/v{ver}.md`（Task 4 建立）
- Produces: 端到端发布流水线；`latest.json` 资产（Tauri 格式，平台键 `darwin-aarch64` / `darwin-x86_64` / `windows-x86_64`），供应用 updater 消费

- [ ] **Step 1: 完整重写 release.yml**

将 `.github/workflows/release.yml` 整个替换为以下内容：

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

jobs:
  release:
    runs-on: ${{ matrix.os }}
    environment: release
    strategy:
      fail-fast: false
      matrix:
        include:
          - os: windows-latest
          - os: macos-latest
    steps:
      - name: Checkout
        uses: actions/checkout@v4

      - name: Setup pnpm
        uses: pnpm/action-setup@v4

      - name: Setup Node.js
        uses: actions/setup-node@v4
        with:
          node-version: "20"
          cache: pnpm

      - name: Setup Rust
        uses: dtolnay/rust-toolchain@stable

      - name: Add macOS universal targets
        if: runner.os == 'macOS'
        run: rustup target add aarch64-apple-darwin x86_64-apple-darwin

      - name: Install frontend dependencies
        run: pnpm install --frozen-lockfile

      - name: Prepare Tauri signing key
        shell: bash
        run: |
          RAW="${{ secrets.TAURI_SIGNING_PRIVATE_KEY }}"
          if [ -z "$RAW" ]; then
            echo "::error::TAURI_SIGNING_PRIVATE_KEY secret is not set. Add it under Settings > Secrets and variables > Actions." >&2
            exit 1
          fi
          KEY_PATH="$RUNNER_TEMP/tauri_signing.key"
          if printf '%s' "$RAW" | head -n1 | grep -q '^untrusted comment:'; then
            printf '%s\n' "$RAW" > "$KEY_PATH"
          elif DECODED=$(printf '%s' "$RAW" | base64 --decode 2>/dev/null) \
               && echo "$DECODED" | head -n1 | grep -q '^untrusted comment:'; then
            printf '%s\n' "$DECODED" > "$KEY_PATH"
          else
            printf '%s\n%s\n' "untrusted comment: tauri signing key" "$RAW" > "$KEY_PATH"
          fi
          echo "TAURI_SIGNING_PRIVATE_KEY=$(base64 < "$KEY_PATH" | tr -d '\r\n')" >> "$GITHUB_ENV"
          if [ -n "${{ secrets.TAURI_SIGNING_PRIVATE_KEY_PASSWORD }}" ]; then
            echo "TAURI_SIGNING_PRIVATE_KEY_PASSWORD=${{ secrets.TAURI_SIGNING_PRIVATE_KEY_PASSWORD }}" >> "$GITHUB_ENV"
          fi

      - name: Apple signing readiness check
        if: runner.os == 'macOS'
        shell: bash
        run: |
          if [ -n "${{ secrets.APPLE_CERTIFICATE }}" ]; then
            echo "::warning::Apple signing secrets detected but the signing pipeline is not enabled yet. Building unsigned DMG. See docs/operations/RELEASE.md."
          else
            echo "Building unsigned DMG (Apple signing secrets not configured, expected)."
          fi

      - name: Build Tauri App (macOS)
        if: runner.os == 'macOS'
        shell: bash
        run: pnpm tauri build --bundles dmg --target universal-apple-darwin

      - name: Build Tauri App (Windows)
        if: runner.os == 'Windows'
        shell: pwsh
        run: pnpm tauri build --bundles nsis

      - name: Prepare macOS Assets
        if: runner.os == 'macOS'
        shell: bash
        run: |
          set -euo pipefail
          mkdir -p release-assets
          VERSION="${GITHUB_REF_NAME#v}"
          DMG=$(find src-tauri/target -name "*.dmg" -type f | head -1 || true)
          TAR=$(find src-tauri/target -name "*.app.tar.gz" -type f | head -1 || true)
          if [ -z "$DMG" ]; then
            echo "::error::No macOS DMG found" >&2
            exit 1
          fi
          cp "$DMG" "release-assets/MultiAgents-Manager-${VERSION}-macOS.dmg"
          [ -f "${DMG}.sig" ] && cp "${DMG}.sig" "release-assets/MultiAgents-Manager-${VERSION}-macOS.dmg.sig" || true
          if [ -n "$TAR" ]; then
            cp "$TAR" "release-assets/MultiAgents-Manager-${VERSION}-macOS.tar.gz"
            [ -f "${TAR}.sig" ] && cp "${TAR}.sig" "release-assets/MultiAgents-Manager-${VERSION}-macOS.tar.gz.sig" || true
          fi
          ls -la release-assets/

      - name: Prepare Windows Assets
        if: runner.os == 'Windows'
        shell: pwsh
        run: |
          $ErrorActionPreference = 'Stop'
          New-Item -ItemType Directory -Force -Path release-assets | Out-Null
          $VERSION = $env:GITHUB_REF_NAME.Substring(1)
          $setup = Get-ChildItem -Path 'src-tauri/target/release/bundle/nsis' -Recurse -Include *.exe -ErrorAction SilentlyContinue | Select-Object -First 1
          if ($null -eq $setup) { throw 'No NSIS setup.exe found' }
          Copy-Item $setup.FullName "release-assets/MultiAgents-Manager-$VERSION-Windows-x64-setup.exe"
          if (Test-Path "$($setup.FullName).sig") {
            Copy-Item "$($setup.FullName).sig" "release-assets/MultiAgents-Manager-$VERSION-Windows-x64-setup.exe.sig"
          }
          Get-ChildItem release-assets | Format-Table Name, Length

      - name: Upload release artifacts
        uses: actions/upload-artifact@v4
        with:
          name: release-assets-${{ runner.os }}
          path: release-assets/*
          if-no-files-found: error

  publish-release:
    name: Publish GitHub Release
    runs-on: ubuntu-latest
    needs: release
    permissions:
      contents: write
    steps:
      - name: Checkout
        uses: actions/checkout@v4
        with:
          ref: ${{ github.ref }}

      - name: Download built artifacts
        uses: actions/download-artifact@v4
        with:
          pattern: release-assets-*
          path: release-assets
          merge-multiple: true

      - name: Build release body
        shell: bash
        run: |
          set -euo pipefail
          VERSION="${GITHUB_REF_NAME#v}"
          BODY="$RUNNER_TEMP/body.md"
          {
            echo "## MultiAgents Manager ${GITHUB_REF_NAME}"
            echo ""
            echo "### 下载"
            echo ""
            echo "- **macOS**: \`MultiAgents-Manager-${VERSION}-macOS.dmg\`"
            echo "- **macOS (updater)**: \`MultiAgents-Manager-${VERSION}-macOS.tar.gz\`（自动更新专用，无需手动下载）"
            echo "- **Windows (x86_64)**: \`MultiAgents-Manager-${VERSION}-Windows-x64-setup.exe\`"
            echo ""
            echo "---"
            echo ""
            echo "### 更新内容"
            echo ""
            if [ -f "docs/release-notes/v${VERSION}.md" ]; then
              cat "docs/release-notes/v${VERSION}.md"
            else
              echo "_本次更新内容见上方下载清单。_"
            fi
          } > "$BODY"
          echo "RELEASE_BODY_PATH=$BODY" >> "$GITHUB_ENV"

      - name: Upload Release Assets
        uses: softprops/action-gh-release@v2
        with:
          tag_name: ${{ github.ref_name }}
          body_path: ${{ env.RELEASE_BODY_PATH }}
          prerelease: false
          files: release-assets/*
        env:
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}

  assemble-latest-json:
    name: Assemble latest.json
    runs-on: ubuntu-latest
    needs: publish-release
    permissions:
      contents: write
    steps:
      - name: Checkout
        uses: actions/checkout@v4
        with:
          ref: ${{ github.ref }}

      - name: Download release assets
        env:
          GH_TOKEN: ${{ secrets.GITHUB_TOKEN }}
        run: |
          set -euo pipefail
          mkdir -p dl
          gh release download "$GITHUB_REF_NAME" --dir dl --repo "$GITHUB_REPOSITORY"
          ls -la dl/

      - name: Generate latest.json
        env:
          GH_TOKEN: ${{ secrets.GITHUB_TOKEN }}
          TAG: ${{ github.ref_name }}
          REPO: ${{ github.repository }}
        run: |
          node <<'NODE'
          const fs = require("fs"), path = require("path");
          const tag = process.env.TAG;
          const repo = process.env.REPO;
          const base = `https://github.com/${repo}/releases/download/${tag}`;
          const version = tag.replace(/^v/i, "");
          const sigFiles = fs.readdirSync("dl").filter((f) => f.endsWith(".sig"));
          const platforms = {};
          for (const f of sigFiles) {
            const asset = f.slice(0, -4);
            const url = `${base}/${encodeURIComponent(asset)}`;
            const signature = fs.readFileSync(path.join("dl", f), "utf8").trim();
            if (asset.endsWith("-macOS.tar.gz")) {
              platforms["darwin-aarch64"] = { signature, url };
              platforms["darwin-x86_64"] = { signature, url };
            } else if (asset.endsWith("-Windows-x64-setup.exe")) {
              platforms["windows-x86_64"] = { signature, url };
            }
          }
          let notes = `Release ${tag}`;
          const notesFile = `docs/release-notes/v${version}.md`;
          if (fs.existsSync(notesFile)) {
            notes = fs.readFileSync(notesFile, "utf8");
          }
          const pubDate = new Date().toISOString().replace(/\.\d{3}Z$/, "Z");
          const payload = { version, notes, pub_date: pubDate, platforms };
          fs.writeFileSync("latest.json", JSON.stringify(payload, null, 2) + "\n");
          console.log(fs.readFileSync("latest.json", "utf8"));
          NODE

      - name: Upload latest.json to release
        env:
          GH_TOKEN: ${{ secrets.GITHUB_TOKEN }}
        run: gh release upload "$GITHUB_REF_NAME" latest.json --clobber --repo "$GITHUB_REPOSITORY"
```

- [ ] **Step 2: 校验 YAML 语法**

Run: `ruby -e "require 'yaml'; YAML.load_file('.github/workflows/release.yml'); puts 'YAML OK'"`
Expected: 打印 `YAML OK`。

- [ ] **Step 3: 人工核对关键点（逐条确认，不跳过）**
  - [ ] 触发 `push.tags: ["v*"]` + `permissions.contents: write` + `concurrency`
  - [ ] 两个矩阵条目 `windows-latest` / `macos-latest`；macOS 先加 universal targets
  - [ ] 签名密钥 step 在两种 Secret 格式下都能产出 `TAURI_SIGNING_PRIVATE_KEY` 环境变量
  - [ ] `publish-release` 的 `body_path` 使用 env 变量 `RELEASE_BODY_PATH`
  - [ ] `assemble-latest-json` 的平台映射只认 `*-macOS.tar.gz` 与 `*-Windows-x64-setup.exe`
  - [ ] 无 `prerelease: true`

- [ ] **Step 4: 提交**

```bash
git add .github/workflows/release.yml
git commit -m "ci: rewrite release pipeline (build -> publish -> latest.json)"
```

---

### Task 3: 增强 `.github/workflows/ci.yml`（路径过滤 + PR 真构建验证）

**Files:**
- Modify (overwrite): `.github/workflows/ci.yml`

**Interfaces:**
- Consumes: 无（独立于 release 流水线）
- Produces: `changes` job 输出 `frontend`/`backend` 布尔值；`build` job 的验证 artifact

- [ ] **Step 1: 完整重写 ci.yml**

将 `.github/workflows/ci.yml` 整个替换为：

```yaml
name: CI

on:
  push:
    branches: [main, develop]
  pull_request:
    branches: [main]

concurrency:
  group: ci-${{ github.ref }}
  cancel-in-progress: true

jobs:
  changes:
    name: Detect changed areas
    runs-on: ubuntu-latest
    outputs:
      frontend: ${{ steps.filter.outputs.frontend }}
      backend: ${{ steps.filter.outputs.backend }}
    steps:
      - name: Checkout
        if: github.event_name == 'push'
        uses: actions/checkout@v4

      - name: Detect changed paths
        id: filter
        uses: dorny/paths-filter@v3
        with:
          filters: |
            frontend:
              - "src/**"
              - "tests/**"
              - "index.html"
              - "package.json"
              - "pnpm-lock.yaml"
              - "pnpm-workspace.yaml"
              - "tsconfig.json"
              - "tsconfig.node.json"
              - "vite.config.ts"
              - "vitest.config.ts"
              - "components.json"
              - ".github/workflows/**"
            backend:
              - "src-tauri/**"
              - "rust-toolchain.toml"
              - ".github/workflows/**"

  frontend:
    name: Frontend Checks
    runs-on: ubuntu-latest
    needs: changes
    if: github.event_name != 'pull_request' || needs.changes.outputs.frontend == 'true'
    steps:
      - uses: actions/checkout@v4
      - uses: pnpm/action-setup@v4
      - uses: actions/setup-node@v4
        with:
          node-version: 20
          cache: pnpm
      - run: pnpm install --frozen-lockfile
      - run: pnpm lint
      - run: pnpm format:check
      - run: pnpm build
      - run: pnpm test

  backend:
    name: Backend Checks
    runs-on: ubuntu-latest
    needs: changes
    if: github.event_name != 'pull_request' || needs.changes.outputs.backend == 'true'
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - run: cd src-tauri && cargo check
      - run: cd src-tauri && cargo clippy -- -D warnings
      - run: cd src-tauri && cargo test

  build:
    name: PR build verification
    runs-on: ubuntu-latest
    needs: changes
    if: github.event_name != 'pull_request' || needs.changes.outputs.frontend == 'true' || needs.changes.outputs.backend == 'true'
    steps:
      - uses: actions/checkout@v4
      - uses: pnpm/action-setup@v4
      - uses: actions/setup-node@v4
        with:
          node-version: 20
          cache: pnpm
      - uses: dtolnay/rust-toolchain@stable

      - name: Install Linux system dependencies
        run: |
          sudo apt-get update
          sudo apt-get install -y --no-install-recommends \
            build-essential pkg-config file libssl-dev patchelf xdg-utils \
            libgtk-3-dev librsvg2-dev libayatana-appindicator3-dev \
            libwebkit2gtk-4.1-dev libsoup-3.0-dev

      - name: Build Tauri App
        run: pnpm tauri build --no-bundle

      - name: Upload binaries
        uses: actions/upload-artifact@v4
        with:
          name: pr-build-binaries
          path: src-tauri/target/release/multi-agents-manager
          if-no-files-found: error
```

- [ ] **Step 2: 校验 YAML 语法**

Run: `ruby -e "require 'yaml'; YAML.load_file('.github/workflows/ci.yml'); puts 'YAML OK'"`
Expected: 打印 `YAML OK`。

- [ ] **Step 3: 确认二进制路径**

Run: `grep -n "mainBinaryName" src-tauri/tauri.conf.json`
Expected: `"mainBinaryName": "multi-agents-manager"`（与 `build` job 的 `path: src-tauri/target/release/multi-agents-manager` 一致）。

- [ ] **Step 4: 提交**

```bash
git add .github/workflows/ci.yml
git commit -m "ci: add path filtering and PR build verification"
```

---

### Task 4: 建立 release notes 目录约定 + 对齐 release.sh

**Files:**
- Create: `docs/release-notes/README.md`
- Modify: `scripts/release.sh`

**Interfaces:**
- Consumes: 无
- Produces: `docs/release-notes/`（Task 2 的 `publish-release` / `assemble-latest-json` 读取 `docs/release-notes/v{ver}.md`）

- [ ] **Step 1: 创建 notes 目录说明**

创建 `docs/release-notes/README.md`：

```markdown
# Release Notes

每次发布一个版本，在仓库里维护一个该版本的更新说明文件。

- 文件名：`v<版本号>.md`，例如 `v0.2.3.md`
- 内容只写"本次更新内容"（变更/修复/优化），**不要**写下载清单（发布流水线会自动生成下载段）
- 发布流程：
  1. 写 `vX.Y.Z.md` 并单独提交（`docs: add release notes vX.Y.Z`）
  2. 运行 `pnpm release:version`，选择同一版本号（它会更新版本文件、打 tag、push）
  3. CI 自动构建并发布到 GitHub Releases（正文含本文件内容 + 下载清单）
```

- [ ] **Step 2: 修改 release.sh 的 notes 模板位置**

打开 `scripts/release.sh`，把：

```bash
NOTES_FILE="$RELEASE_DIR/release-notes-v${VERSION}.md"
```

改为：

```bash
NOTES_DIR="$PROJECT_DIR/docs/release-notes"
mkdir -p "$NOTES_DIR"
NOTES_FILE="$NOTES_DIR/v${VERSION}.md"
```

并把模板文件头部的 `# Release Notes` 标题改为 `## v${VERSION}`（与 release body 拼接后的层级一致）。

- [ ] **Step 3: 校验 bash 语法 + 核对路径**

Run: `bash -n scripts/release.sh && grep -n "docs/release-notes" scripts/release.sh`
Expected: 无语法错误；grep 命中 `NOTES_DIR="$PROJECT_DIR/docs/release-notes"` 与 `NOTES_FILE="$NOTES_DIR/v${VERSION}.md"`。

- [ ] **Step 4: 核对更新对话框已渲染 notes（spec §7，预计零改动）**

Run: `grep -n "update?.body\|update\.body" src/components/common/updater-dialog.tsx`
Expected: 命中第 73-78 行附近 `{update?.body && (...)}` 渲染块。若命中则前端无需改动，仅记录确认即可；若未命中，追加一个 `{update?.body && ...}` 渲染段（参照同一文件现有样式）。

- [ ] **Step 5: 提交**

```bash
git add docs/release-notes/README.md scripts/release.sh
git commit -m "docs: add release notes convention and align release.sh template path"
```

---

### Task 5: 编写 RELEASE 指南

**Files:**
- Create: `docs/operations/RELEASE.md`

**Interfaces:**
- Consumes: Task 1-4 的产物与约定
- Produces: 用户操作手册（一次性配置、发布步骤、兑底上传、错误恢复、Apple 启用指引）

- [ ] **Step 1: 创建 `docs/operations/RELEASE.md`**

内容必须覆盖以下全部小节（直接写完整内容，无占位）：

```markdown
# Release 发布指南

## 一次性配置（首次发布前，只做一次）

### 1. 生成签名密钥
```bash
mkdir -p ~/.mam
pnpm tauri signer generate -w ~/.mam/tauri.key   # 密码直接回车留空
```
把打印的 `public key:` 公钥填入 `src-tauri/tauri.conf.json` 的 `plugins.updater.pubkey`。

### 2. 配置 GitHub Secrets
仓库 Settings → Secrets and variables → Actions → New repository secret：
- `TAURI_SIGNING_PRIVATE_KEY`：`~/.mam/tauri.key` 文件内容（base64）→ 填之前先 base64 编码：
  ```bash
  base64 < ~/.mam/tauri.key | tr -d '\n' | pbcopy
  ```
- `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`：仅当生成时设了密码才需要

（Apple 签名 Secrets 暂不需要；见下文"Apple 签名"）

## 每次发版步骤

1. 写更新说明：`docs/release-notes/vX.Y.Z.md`（只写更新内容）
2. 提交：`git add docs/release-notes/vX.Y.Z.md && git commit -m "docs: add release notes vX.Y.Z"`
3. 发版：`pnpm release:version` → 选择同一版本号 → 确认后自动提交版本文件、打 tag、push
4. 到 GitHub 仓库 Actions 页看 Release 流水线进度（构建 → 发布 → latest.json 三段）
5. 完成后到 Releases 页核对：正文含更新内容 + 下载清单，产物全部挂载

## 用户如何下载/更新
- GitHub Releases 页面（`https://github.com/jarvislee90s-dot/MultiAgents-Manager/releases`）：下载对应平台的 dmg / exe
- 已安装旧版的应用：启动时自动检测更新并提示（自动更新走 `latest.json`，无需手动下载）
- 自动更新端点：`https://github.com/jarvislee90s-dot/MultiAgents-Manager/releases/latest/download/latest.json`

## 手动补挂产物（个别平台构建失败时）
```bash
gh release upload vX.Y.Z ./path/to/installer.exe --repo jarvislee90s-dot/MultiAgents-Manager
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
```

- [ ] **Step 2: 校验 markdown 完整性**

Run: `grep -c "^## " docs/operations/RELEASE.md`
Expected: 输出大于等于 6（一次性配置/每次发版步骤/用户如何下载/手动补挂/失败恢复/Apple 签名 均有 `## ` 标题）。

- [ ] **Step 3: 提交**

```bash
git add docs/operations/RELEASE.md
git commit -m "docs: add release guide"
```

---

### Task 6: 端到端发布演练（手动验收，需用户配合）

**Files:**
- 无代码改动；仅执行验证

**Interfaces:**
- Consumes: Task 1-5 全部产物

- [ ] **Step 1: 配置 Secrets（用户操作）**

在 GitHub 仓库 Settings → Secrets and variables → Actions 新建 `TAURI_SIGNING_PRIVATE_KEY`（值：`base64 < ~/.mam/tauri.key | tr -d '\n'` 的输出）。确认 Secret 已保存（`gh secret list` 可见）。

- [ ] **Step 2: 确认签名可本地产出（在 mac 上验证）**

Run:
```bash
TAURI_SIGNING_PRIVATE_KEY="$(base64 < ~/.mam/tauri.key | tr -d '\n')" pnpm tauri build --bundles dmg --target universal-apple-darwin
find src-tauri/target/universal-apple-darwin/release/bundle -name "*.sig" | head
```
Expected: 构建成功；至少一个 `.sig` 文件存在（证明 createUpdaterArtifacts + 签名生效）。此步耗时较长（universal 编译），可后台运行。
> 若机器未装 universal 目标：先 `rustup target add aarch64-apple-darwin x86_64-apple-darwin`。

- [ ] **Step 3: 发布 v0.2.3（用户确认后执行）**

```bash
# 1. 写 notes 文件（编辑内容）
cat > docs/release-notes/v0.2.3.md <<'EOF'
## v0.2.3

### 🚀 新增
- 首个通过 CI 自动构建发布的版本

### 📦 打包
- 引入 release 机制与自动更新（本版本起生效）
EOF
git add docs/release-notes/v0.2.3.md
git commit -m "docs: add release notes v0.2.3"

# 2. 发版
pnpm release:version   # 选 0.2.3 或 patch，确认 push
```

- [ ] **Step 4: 观察流水线三段**

在 GitHub Actions 页确认 `Release` workflow 三个 job 依次成功：`release`（构建/签名）→ `publish-release`（挂载）→ `assemble-latest-json`。任一失败按 `docs/operations/RELEASE.md` 的"失败恢复"处理。

- [ ] **Step 5: 核对 release 页 + updater 端点**

Run:
```bash
gh release view v0.2.3 --repo jarvislee90s-dot/MultiAgents-Manager --json name,assets --jq '{name, assets: [.assets[].name]}'
curl -fsS https://github.com/jarvislee90s-dot/MultiAgents-Manager/releases/latest/download/latest.json | python3 -m json.tool
```
Expected: release 包含 `MultiAgents-Manager-0.2.3-macOS.dmg`、`-macOS.tar.gz`、`-Windows-x64-setup.exe`、`latest.json`；`latest.json` 的 `platforms` 含 `darwin-aarch64`/`darwin-x86_64`/`windows-x86_64` 且各自 `signature` 非空。

- [ ] **Step 6: 应用内验证自动更新（用户操作，选做）**

在装有旧版的 mac 上打开应用，或改动 `latest.json` 版本号后手动检查更新（设置页/关于页的检查更新入口）。Expected: 检测到新版本，更新对话框显示 `docs/release-notes/v0.2.3.md` 的更新内容。

---

### 范围说明（不在本计划内）
- Linux 发布产物、Apple 签名/公证落地、应用内发布工作台、updater 双通道 CDN、多语种 release notes——均为后续独立任务。
