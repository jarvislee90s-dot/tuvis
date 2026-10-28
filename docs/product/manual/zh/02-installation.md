# 下载与安装

**全新安装默认值**：没有历史数据库（含旧品牌数据）时，远程接入总开关默认开启，开机启动在正式版首次启动时自动注册，远程消息设备签名默认关闭。各远程通道仍需按需手动开启；升级用户保留原状态。关闭开关后，后续启动不会重新打开；开机启动以系统实际注册结果为准，失败可在设置中重试。

兔维斯提供 Windows 与 macOS 两种安装包，从 GitHub Releases 下载最新版本，安装约一分钟。

## 下载

1. 打开发布页：`https://github.com/jarvislee90s-dot/tuvis/releases`；
2. 在最新版本的 Assets 列表里，按你的系统下载对应安装包：
   - **Windows**：`Tuvis-*-Windows-x64-setup.exe`
   - **macOS**：`Tuvis-*-macOS.dmg`

> 💡 只认本仓库的 Releases 页。其他任何网站或渠道提供的下载均为第三方分发，请自行甄别风险。

## 系统要求

- Windows 10 及以上（64 位）；
- macOS 14 及以上。

## 安装步骤

**Windows**：双击安装包，按安装向导点击「下一步」直至完成。

![安装向导](../img/install-wizard.png)

**macOS**：打开 `.dmg`，将兔维斯拖入 Applications（应用程序）文件夹，再从启动台或访达打开。

## 安装注意

- **从 0.5.2 及更早版本升级**：这些旧版（应用名还是 MultiAgents Manager）的应用内自动更新已失效，请直接手动下载新版安装包；新旧是两个应用登记，**先卸载旧版再装新版**，避免出现两个托盘图标。旧数据会在新版首次启动时自动从 `~/.mam` 迁移为 `~/.tuvis`，无需手动搬。
- **从 0.5.5 及之后版本升级**：直接用新安装包覆盖安装即可，hook 配置由应用启动时自动迁移。
- **数据都在本地**：兔维斯的所有数据（数据库、设置、日志、技能仓库）存放在 `~/.tuvis/` 目录，备份这个目录即备份全部数据。
- **macOS 提示「应用已损坏」或「无法验证开发者」**：这是系统对未公证安装包的通用拦截。在「系统设置」的隐私与安全性区域找到关于兔维斯的放行提示并点击允许；或对应用右键（Ctrl+点按）选「打开」启动一次，之后即可正常双击打开。

## 数据目录

应用数据统一存放在 `~/.tuvis/`：

| 路径                            | 用途                                        |
| ------------------------------- | ------------------------------------------- |
| `~/.tuvis/tuvis.db`             | SQLite 数据库（设置、扩展、预设、会话缓存） |
| `~/.tuvis/skills/`              | 全局 Skill 仓库                             |
| `~/.tuvis/mcp/`                 | 全局 MCP 服务器配置                         |
| `~/.tuvis/hooks/status-hook.sh` | 共享 Hook 脚本（状态事件）                  |
| `~/.tuvis/events/`              | Hook 事件文件（自动清理，30 秒 TTL）        |

## 开发构建（从源码运行）

**macOS 开发版提示**：开发版（未签名打包）首次使用「一键恢复会话」开窗前，需在
**系统设置 > 隐私与安全性 > 自动化** 中手动允许兔维斯控制
Terminal / iTerm2 一次（TCC 不会自动弹窗）。未授权时 osascript 以 -1743 失败，
resume 会回执「macOS 自动化授权缺失」并给出该指引。

**helper 构建门（批次丙 T2 固化）**：`tuvis-hook-listener`（hook 事件监听 helper）
与 `tuvis-marker`（窗口标题标记）都挂在 Cargo `required-features` 门后——**不带
feature 时根本不构建**，应用启动的 `ensure_hook_script` 就找不到同目录 helper、
跳过安装，`~/.tuvis/bin/` 里那份永远是旧构建（实测故障：事件缺 `tool_name` 载荷 →
问答卡通道 A 永不识别）。

- `pnpm tauri:dev` / `pnpm tauri:build` **已内置 feature**（`hook-listener` 与
  `marker-helper`，后者蕴含前者），照常用即可；
- 单独构建 helper（如跑实机 `#[ignore]` 测试）必须显式带上：
  `cd src-tauri && cargo build --bin tuvis-hook-listener --features hook-listener`；
- 跑 bin 测试同理：`cargo test --bin tuvis-hook-listener --features hook-listener`；
- macOS 打包（`release:macos` / release.yml 的 macOS job）走的是裸
  `pnpm tauri build` 显式参数，**不受**上述 npm script 影响——额外 bin 会令
  universal 打包失败，故 macOS 侧有意不带 feature（helper 为 Windows 通道）。
