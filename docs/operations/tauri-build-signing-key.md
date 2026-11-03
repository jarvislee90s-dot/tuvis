# 本地打包报错：A public key has been found, but no private key

> 本文记录 2026-08-24 在本地执行 `pnpm tauri:build` 时遇到的更新器签名报错：**问题背景 → 根因定位 → 解决方案**。
> 自动更新的完整配置流程见 [`docs/operations/AUTO_UPDATE.zh-CN.md`](AUTO_UPDATE.zh-CN.md)，本文只聚焦"本地打包 + 签名密钥缺失"这个具体问题。

## 问题背景

在本地（Windows / PowerShell）执行：

```powershell
pnpm tauri:build   # 等价于 tauri build --bundles nsis
```

编译到 NSIS 打包完成、`setup.exe` 已经生成之后，**最后一步**报错退出：

```text
Running makensis to produce ...\bundle\nsis\兔维斯_0.1.0_x64-setup.exe
Finished 1 bundle at: ...\兔维斯_0.1.0_x64-setup.exe
A public key has been found, but no private key. Make sure to set `TAURI_SIGNING_PRIVATE_KEY` environment variable.
       Error A public key has been found, but no private key. ...
[ELIFECYCLE] Command failed with exit code 1.
```

关键事实：

- **安装包其实已经成功产出**：`src-tauri/target/release/bundle/nsis/兔维斯_0.1.0_x64-setup.exe`
- 报错只发生在**更新器（updater）产物签名**环节，导致命令整体以 exit 1 收尾
- 不是代码问题、不是编译失败，是**密钥 / 环境变量缺失**的配置问题

## 根因定位

Tauri 自动更新的签名机制决定：

1. `src-tauri/tauri.conf.json` 中两处配置共同触发"需要生成更新签名"：
   - `bundle.createUpdaterArtifacts: true` → 要求为更新产物（`latest.json` / 各安装包的 `.sig`）签名
   - `updater.pubkey` → 已配置公钥
2. Tauri 在打包最后一步校验：**配置了公钥 → 必须有对应私钥**来签名；私钥通过环境变量 `TAURI_SIGNING_PRIVATE_KEY`（及可选的 `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`）提供。
3. 本机排查结果：
   - `TAURI_SIGNING_PRIVATE_KEY` 环境变量：**未设置**
   - 本地私钥文件（`*.key`）：**不存在**
   - 私钥实际只在 GitHub 仓库的 Actions Secrets 中（`release.yml` 从 `secrets.TAURI_SIGNING_PRIVATE_KEY` 读取）

于是 Tauri 判定"有公钥、无私钥"，无法签名，直接报错退出。

CI 的处理方式（`.github/workflows/release.yml` 第 48-73 行）：当 `TAURI_SIGNING_PRIVATE_KEY` secret 存在时签名；不存在时用 `jq` 把 `createUpdaterArtifacts` 临时置为 `false`，构建照常通过（只是不出更新签名）。

## 解决方案

### 现在（本地自用测试）：一次性参数，不改仓库

```powershell
# 项目根目录执行
pnpm exec tauri build --bundles nsis --config '{"bundle":{"createUpdaterArtifacts":false}}'
```

要点：

- 与 CI 无私钥时的处理方式一致（`createUpdaterArtifacts=false`）
- **只对本次构建生效**，`tauri.conf.json` 一字不改，CI 正式发版不受影响
- 产物手动安装、单机自用完全正常；仅没有自动更新签名文件

> ⚠️ 不要直接把 `tauri.conf.json` 的 `createUpdaterArtifacts` 改成 `false` 并提交：那会**永久禁用仓库的更新产物**，即使以后配好私钥，CI 也不再生成签名，反而破坏正式发版。

### 以后（正式发版 + 自动更新）：配置签名密钥

完整流程见 `docs/operations/AUTO_UPDATE.zh-CN.md`，核心三步：

1. 生成密钥对（一次性）：
   ```bash
   pnpm tauri signer generate -w ~/.tauri/multiagents-manager.key
   # 建议加密码：pnpm tauri signer generate -w ~/.tauri/multiagents-manager.key -p <密码>
   ```
2. 把生成的**公钥**填回 `src-tauri/tauri.conf.json` 的 `updater.pubkey`；
3. 把**私钥**（及密码，若有）加入 GitHub 仓库 Secrets：
   - `TAURI_SIGNING_PRIVATE_KEY`
   - `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`

之后 CI 发版会自动签名并生成 `latest.json` + `.sig`；本地若需完整签名，设置同样的环境变量后再 `pnpm tauri:build`。

### ⚠️ 密钥轮换注意事项

仓库里现有 `updater.pubkey` 对应一把私钥（应在 GitHub Secrets 中，本地没有）。**重新生成密钥对时，公钥和私钥必须成对同步更新**（`tauri.conf.json` 的 pubkey + Secrets 的 private key 一起换）。否则：

- 已发布版本（如 v0.1.0）内置的是**旧公钥**
- 用**新私钥**签名的更新，旧版本校验不过 → 老用户收不到自动更新，只能手动安装新安装包

## 相关文件

- `src-tauri/tauri.conf.json` — `updater.pubkey` / `bundle.createUpdaterArtifacts`
- `.github/workflows/release.yml` — CI 签名逻辑（无 key 时第 71-72 行临时置 `createUpdaterArtifacts=false`）
- `docs/operations/AUTO_UPDATE.zh-CN.md` — 自动更新完整配置流程
