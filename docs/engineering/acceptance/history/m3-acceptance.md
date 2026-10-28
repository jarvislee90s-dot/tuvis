# M3 验收记录（看板实时内容 · board-realtime-content）

- 分支：`feat/m3-board-realtime-content`（基线 339f7d5 = M2 合并 #62）
- 计划：`docs/engineering/plans/2026-09-15-m3-board-realtime-content.md`
- 验收日期：2026-09-15
- 结论先行：**自动化验收 ✅ 全绿 / 真机验收 ⬜ 待用户**（清单见第三节，照做即可）

---

## 一、交付范围摘要（逐条对应 commit）

| 交付项 | 内容 | Commit |
|---|---|---|
| P8a/P8b | 移动看板页头：MAM 品牌 + 版本号 + 本机名 | `d8d540d` |
| P7 修正 + P8c | 0.0.0.0 绑定时主显示改局域网 IP；卡片主行重排（工具名 + 项目名 + 右侧状态点） | `11fd2ff` |
| P8d/P8e | chips 仅列受管工具、品牌配色、多行折叠、活跃会话排序 | `20bbbb1` + 评审修复 `40c02bb` |
| P8f | 日/夜双皮肤（跟随系统 + 手动切换持久化） | `155f0df` |
| A3 SessionWatcher | 后端事件桥：2s diff → broadcast（camelCase 契约、零订阅者免扫描、句柄自愈） | `47dd283` + 评审修复 `d3642c6` |
| C1 实时 + 降级 | `/api/events` SSE 端点 + 移动端实时提醒（横幅/提醒音/振动）+ 连续 2 次失败降级 3s 轮询 | `8b9fb12` + 测试修正 `f744171` |
| C2 消息 API | 八工具（claude/codex/dsh/kimi/opencode/openclaw/workbuddy/zcode）会话内容读取；dsh 数据根 DSH_HOME 同源 + 同 id 去重 | `21327ba` + 评审修复 `692a86f` |
| C2 前端 + 文件预览 | ZCode 式会话详情（完整对话）+ 文件预览（安全读取：越界/symlink/双阈值防护；markdown 渲染 / 图片 / 代码高亮） | `b8b3294` |
| 文件路径提取 | 七工具会话内容中文件路径提取（泛化提取器 + fixture 兜底；kimi `file` / opencode `filePath` 实证） | `4ba40f0` |
| 终审修复三连 | 终审（全分支评审）三项必修：/session-files 数据源与 /session-messages 同源（DSH_HOME/KIMI_CODE_HOME 单源归口）；file 端点图片响应加 CSP + nosniff 安全头；移动端主题切换在 localStorage 写失败时自愈 | `67c8565` |
| 终验评审修复 R1 | SSE 模式 30s 低频对账（`Board.tsx` SSE effect 内 `RECONCILE_MS=30s` tick）：transition 只更新已存在卡（watcher 对新增会话不发事件）、SSE 又无周期快照，新会话成员资格原本冻结——30s 一次全量拉取兜底后，新会话上卡/下卡最大延迟 30s，不再需要刷新页面 | `56a9040` |

评审记录：每个 commit 均通过独立评审（`.superpowers/sdd/2026-09-15-m3-board-realtime-content/review-*.diff`），评审发现的问题全部在对应 fix commit 内 ADDRESSED（台账见同目录 `progress.md`）。

## 二、自动化门禁结果（2026-09-15 实测，按序执行）

| # | 命令 | 结果 | 数字 |
|---|---|---|---|
| 1 | `pnpm build:mobile` | ✅ exit 0 | 2323 modules；产物 JS 728.55 kB（gzip 220.71 kB）+ CSS 48.91 kB（gzip 9.50 kB）；vite 对 >500 kB chunk 有体积告警（见第四节 #7） |
| 2 | `cargo test`（src-tauri/） | ✅ exit 0 | 575 单元 + 4 dao + 8 linker = **587 passed / 0 failed**（Doc-tests 0；终审修复后终数） |
| 3 | `cargo clippy --all-targets -- -D warnings` | ✅ exit 0 | 0 warning |
| 4 | `cargo fmt --check` | ✅ exit 0 | 无差异 |
| 5 | `pnpm check`（format:check + lint + check:i18n + tsc + vite build） | ✅ exit 0 | prettier / eslint / i18n / tsc 全过；桌面 bundle 构建成功 |
| 6 | `pnpm test`（vitest） | ✅ exit 0 | **59 个测试文件 / 357 个用例全过**（终审修复后终数） |

## 三、手动验收清单（⬜ = 待用户真机）

**桌面准备（一次性）**：启动应用（开发模式 `pnpm tauri:dev`，或安装版直接打开）→ 打开 **设置页 → 远程接入** 区块 → 打开「启用远程接入」开关 → 绑定选 **局域网 (0.0.0.0)**（真机必须在同一 Wi-Fi；仅本机 127.0.0.1 手机连不上）→ 点「生成二维码」→ 用手机相机/浏览器扫码（二维码内容即带配对 token 的 `http://<局域网IP>:<端口>/m` 地址），在手机浏览器打开即进入移动看板。

| 场景 | 操作步骤 | 预期 | 状态 |
|---|---|---|---|
| P8a/P8b 页头 | 打开看板，看顶部品牌行 | 显示 MAM 品牌、应用版本号、本机名 | ⬜ 待用户 |
| P7 修正 | 桌面绑定切到 0.0.0.0，重开服务后刷新看板 / 看设置页主地址 | 主显示是局域网 IP（如 `http://192.168.x.x:PORT`），不是 `0.0.0.0` | ⬜ 待用户 |
| P8c 卡片结构 | 看任一会话卡片 | 主行 = 工具名 + 项目名 + 右侧状态点；副行 = 会话标题 + 最新消息摘要 | ⬜ 待用户 |
| P8d/P8e chips | 看顶部工具筛选 chips；点选/取消；对比活跃会话排序 | 仅列出受管（已安装接入）的工具；各工具品牌色；多行时折叠为「+N」可展开；活跃会话的工具/会话排前 | ⬜ 待用户 |
| P8f 日/夜皮肤 | 手机系统切深色/浅色；或在看板内手动切换后刷新 | 跟随系统变化；手动选择刷新后保持 | ⬜ 待用户 |
| C1 实时 | 看板留在手机前台，在桌面终端让某个 agent 会话产生状态变化（如提交新消息、等待批准） | ≤2 秒内手机收到横幅 + 提醒音 + 振动（首次需在页面上有一次交互以解锁音频/振动权限） | ⬜ 待用户 |
| C1 降级 | 看板打开后，手机断 Wi-Fi 数秒（让 SSE 连续 2 次重连失败，1s 线性退避）再恢复网络 | 恢复后看板自动切 3s 轮询、数据仍刷新（变化提示延迟最多 ~3s，弱实时但不断流） | ⬜ 待用户 |
| C2 消息 | 点开任一卡片进入会话详情 | ZCode 式完整对话视图（用户/助手消息、工具调用步骤） | ⬜ 待用户 |
| C2 文件 | 在会话详情中点开文件路径链接（如 Claude Read/Write 的目标文件、ZCode 会话中被改文件） | 文件可预览：markdown 渲染、图片直接显示、代码语法高亮；拒绝越界/超大文件 | ⬜ 待用户 |
| 桌面回归 | 以第二节全量门禁为代理指标（587 Rust + 357 前端用例 + 双端构建全绿，M3 未触碰桌面 UI 组件），另请用户日常使用复核 | 现有桌面功能零变化 | ⬜ 用户复核（代理指标绿：587+357 全绿） |

## 四、已知限制 / 边界登记（如实转写，均已在评审台账备案）

1. **revoke 后已建立的 SSE 连接持续收事件**——设备判废时效从 ≤3s 退化为∞（连接存续期内）。Task 6 评审裁决：接受为 M3 已知限制；修复需连接注册表 + revoke 广播断连，列入 **M4 候选**。关闭看板页 / 断网重连后新连接即被拒。
2. **TLS 反代确认复选框单向不可撤回且无文案**——M2 终审留办（「M3 前修」）项，**M3 未做，属实**：`src/components/settings/RemoteSection.tsx` 的 Checkbox 仅处理勾选（`v === true` → confirmPublic），取消勾选为静默 no-op，无禁用态、无撤回说明文案；`remote.public_ack` 一旦置位永久 true。登记为 **M4 候选**（修法 = 禁用取消方向 + 文案）。
3. **0.0.0.0 绑定主显示**——P7 已修（`11fd2ff`，`display_url_for` 纯函数）：主地址显示局域网 IP；`lanUrls` 行为不变（仍并列展示全部网卡局域网地址供复制）。
4. **stop_server 后已开 SSE 连接存续**——axum per-connection task 不被 abort，服务已停但手机端旧连接不主动断（连接失败后才走降级）。Task 5 评审备案，随第 1 条一并列入 M4 连接注册表候选。
5. **resubscribe 后首 tick 陈旧基线可能一次假跃迁**——重订阅首拍用陈旧快照作基线，可能产生一次假状态变化提醒；下一拍（≤2s）快照校正自愈。Task 5/6 评审备案。
6. **OpenCode filePath / OpenClaw 键名置信度为文档级证据**——opencode `filePath` 键证据口径为「上游源码」（非实机会话样本）；OpenClaw `tool_call_update` 不做合并（无实机样本）；OpenCode tool part 形态待实机验证。已有泛化提取器 + fixture 兜底（`4ba40f0`），实机不符时补 fixture 即可。
7. **移动 bundle 728.55 kB 原始 / 220.71 kB gzip**——vite 对 >500 kB chunk 有构建告警；功能不受影响。**M4 候选**：manualChunks 代码分割（react-markdown/highlight.js 为大头）。
8. **M2 遗留台账清点**（`m2-progress.md` 终审留办四项）：① TLS 复选撤回文案——**未修**（见第 2 条）；② 0.0.0.0 地址行——已修（P7）；③ stop_server 尾补 revoke_all——已修（`remote/mod.rs` stop 即全吊销）；④ `/m/.gitkeep` SPA 兜底 200——已修（rust-embed 占位入库）。即：M2 留办仅剩 TLS 文案一项未清，已登记。

## 五、结论

- **自动化验收 ✅**：六项门禁全绿（Rust 587 用例、前端 357 用例、双端构建、clippy -D warnings、rustfmt、prettier/eslint/i18n）。
- **真机验收 ⬜ 待用户**：第三节清单共 10 项，需用户 + 手机真机按步骤操作确认；桌面准备步骤已写明，照做即可。
- 已知限制 8 条已如实登记，其中 M4 候选：SSE 连接注册表 + revoke 广播断连、TLS 撤回文案、manualChunks 分包。
