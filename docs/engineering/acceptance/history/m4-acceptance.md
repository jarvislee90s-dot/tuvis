# M4 验收记录（外网全链路 · external-fullchain）

- 分支：`feat/m4-external-fullchain`（基线 bf6f8fd = M3 合并后的 main；计划/设计文档先行提交）
- 计划：`docs/engineering/plans/2026-09-16-m4-external-fullchain.md`（v3）
- 设计：`docs/engineering/specs/2026-09-16-m4-external-fullchain-design.md`（v1.1）
- 验收日期：2026-09-17
- 结论先行：**Task 1–11 开发与逐任务评审 ✅ 全绿 / 全量门禁 ✅ 六件套 / 端到端 S1–S14：自动化可执行项 ✅ 12 项、人工项 ⬜ 4 项**（外网蜂窝 / 彻夜保活 / Windows 实机 / named tunnel 真 Token，见第四节）

---

## 一、交付范围摘要（逐条对应 commit）

| 交付项 | 内容 | Commit |
|---|---|---|
| T0a · SSE 连接注册表 | 设备→活跃连接映射；吊销/停止即时断连（take_until + Drop 反注册），修复 M3 已知限制 | `6203f93` |
| T0b · TLS 确认撤回文案 | 取消勾选回弹 + toast + 常驻说明；toast 分隔符收进 i18n（评审修复） | `0e30262` + `4fcf7ce` |
| 基建 · 全局 AppHandle | events::emit_ui / events::audit 事件出口（隧道/配对/托盘通知共用） | `8faedef` |
| T1d · cloudflared 获取 | 三值通道解析 / 按平台下载源 / ensure 注入式内核 + tar 解压 + 手动放置旁路 | `d90882e` |
| T1b/c · 隧道进程管理 | quick/named 双模式 spawn + stderr 地址解析 + 连续失败退避守护（60s 重置）+ 快照 | `7a5e6d1` |
| T1a · 设置页通道区块 | 外部通道三选一 + Token 输入 + 隧道状态 + 地址「外部通道」徽标 + Tailscale 指引 | `84a709d` |
| T2 后端 · 审批配对 | 内存队列状态机（TTL 5 分钟/队列 3/同 IP 1/限试 3）+ 三端点 + 设备上限三入口同门 + 吊销/花名册命令 + 审计留痕；吊销清审批残留（评审修复） | `6487f53` + `df4752f` |
| T2 桌面 · 配对面板+花名册 | 待审批（4 位码可见+批准）与花名册（在线点/吊销/上限）+ 配对请求系统通知 + useRemoteEvents；i18n 键名对齐（评审修复） | `2496728` + `04859ba` |
| T2 移动 · 请求接入流 | 设备名 + 请求/轮询/4 位码确认三态（满员/限试/过期文案） | `825b30f` |
| T3 · 电源保活 | caffeinate/执行状态 + Windows 磁盘休眠代设还原（注入式状态机 + 崩溃恢复）+ 默认开 + 边界文案 | `e5d7866` |
| T4 · 托盘远程入口 | 开关勾选项 + 地址展示复制（三菜单重建路径统一 + remote-changed 回环刷新） | `83b8804` |
| E2E 修复 ×3 | S7 实测：pair_poll 满员复核（封死轮询幂等路径绕过三入口同门的 TOCTOU）；同修复的重入死锁修正（with 持锁不可重入）；通道切换显式停旧启新（off→quick 隧道永不拉起） | `5f136a2` + `8de69c1` + `24dc04f` |

评审记录：每个任务均经独立评审（spec 合规 + 代码质量双裁决），3 个任务触发 fix loop 并复审通过（台账：`.superpowers/sdd/2026-09-16-m4-external-fullchain/progress.md`，评审包 `review-*.diff` 同目录）。

## 二、自动化门禁结果（2026-09-17 实测，按序执行）

| # | 命令 | 结果 | 数字 |
|---|---|---|---|
| 1 | `cargo fmt --check`（src-tauri/） | ✅ | 无差异 |
| 2 | `cargo clippy --all-targets -- -D warnings` | ✅ | 0 warning |
| 3 | `cargo test` | ✅ | **633 passed / 0 failed**（621 单元 + 4 dao + 8 linker） |
| 4 | `pnpm build:mobile` | ✅ | 构建成功（>500 kB chunk 体积告警为已知二期项） |
| 5 | `pnpm check`（prettier/eslint/check:i18n/tsc/build） | ✅ | 全过 |
| 6 | `pnpm test`（vitest） | ✅ | **65 文件 / 457 用例全过** |

## 三、端到端场景结果（S1–S14）

驱动方式（照计划）：桌面 = 截图定位 + 坐标点击；移动 = Playwright 直连真实服务器（多 context = 多设备）；OS 真值 = bash 探针。验收实例 = 本分支 `pnpm tauri:dev`（含 E2E 期两处代码修复）。

**重要环境事实**：本机存在 co-tenant（用户另一 worktree 的 preset-v2 开发实例 + 单实例插件互斥 + 用户自有 named tunnel ×2）——E2E 以临时端口（vite 1430 / remote 9430，经 KV `remote.port`）隔离完成，未触碰用户自有隧道。

| # | 场景 | 结果 | 证据 / 数字 |
|---|---|---|---|
| S1 | 远程开关 + TLS 门 | ✅ | 绑 0.0.0.0 未确认时开启 → toast「对外绑定需先确认已配置 TLS 反向代理」且开关回弹、9430 无监听；改绑仅本机后开启成功 → `*:9430 LISTEN`（e2e-m4/s1-tls-gate-toast.png、s1-enabled.png） |
| S2 | T0b 撤回文案 | ✅ | 勾选 TLS 确认后取消 → 复选框回弹保持勾选 + toast「安全确认不可在线撤回。 如需撤销，请将绑定改回「仅本机」后重新启用」+ 常驻说明「确认一次性生效，不可在线撤回…」（s2-uncheck-toast.png、s2-hint.png） |
| S3 | 直通扫码 + 首屏 + Cookie | ✅ | 桌面生成配对二维码 → Playwright 打开 `#token=` URL 自动提交 → 看板上板；`mam_device` Cookie TTL=15,551,928s（=Max-Age 15552000 即 180 天，HttpOnly，path=/m）；首屏 domContentLoaded **97ms** / FCP 200ms（≤1s 达标）（s3-board.png） |
| S4 | 审批配对 · 路径一（桌面批准） | ✅ | 新 context 请求「JARVIS-E2E-S4」→ 桌面待审批面板出现（名称/IP/UA/4 位码/剩余秒）→ 点批准 → ≤4s 上板；刷新页面仍已配对（poll 幂等） |
| S5 | 审批配对 · 路径二（4 位码） | ✅ | 新 context 请求「JARVIS-E2E-S5」→ 桌面面板读码 **7035** → 输码确认 → 上板（s5-code-7035.png） |
| S6 | 限试 3 次 | ✅ | 新请求连输 3 次错码：第 1/2 次「确认码错误，剩余 N 次机会」→ 第 3 次「错误次数过多，请重新发起请求」+ 桌面待审批项消失（作废） |
| S7 | 设备满员 · 三入口同门 | ✅* | 3 台在册时第 4 台：①桌面批准 → toast「设备已满，请先在花名册吊销腾位」；②正确 4 位码 → cap_full「设备已满，请在桌面端花名册腾位后重试」；③直通扫码 → 403（审计 `pair_rejected_cap max=3`）；腾位一台后批准 → 上板。*首跑暴露 poll 幂等路径 TOCTOU（第 4 台经重 poll 落库）→ 已修复（`5f136a2` + 重入死锁修正 `8de69c1`，新增回归测试）后复验通过（s7-capfull-toast.png） |
| S8 | 吊销即时断连 | ✅ | S8 设备页面在线（SSE）时花名册点「吊销」→ 页面**立即**回配对页（≪5s）；其余设备不受影响；审计 `device_revoked id=… closed_sse=…` |
| S9 | 停止远程一键断开 | ✅ | 设置页开关关闭（确认弹窗「停止将断开全部设备」→ 停止）→ `enabled=false`、9430 无监听、已配对设备全部吊销（DB 实证）、无孤儿 caffeinate |
| S10 | 电源保活往返 | ✅ | 开启远程 → `caffeinate -ims` 子进程出现 + `pmset -g assertions` 出现 `PreventUserIdleSystemSleep`（pid=caffeinate）；关闭远程 → 子进程退出、断言消失。Windows 磁盘代设还原⬜【人工：Win 实机 `powercfg /q` 前后对照】 |
| S11 | 临时隧道全链路 | ✅* | 通道切「临时隧道」→ 应用自动下载（受限网络停滞，走 T1d 手动放置旁路放入 `~/.mam/bin/`）→ quick tunnel 拉起（`tunnel --url http://127.0.0.1:9430`）→ 设置页显示「当前隧道地址 https://…trycloudflare.com」+ 地址区块首条「外部通道·推荐」→ Playwright 经隧道 URL 请求「JARVIS-E2E-S11」→ 桌面批准上板 → SSE 空闲 **155s** 不断连（CF ~100s 超时 × 内建 15s 心跳 ✓）→ 通道切回关闭 → 应用 cloudflared 进程退出。地址变化桌面通知：首次获取地址 toast/通知已实测 ✓ |
| S12 | 实时提醒（2s 口径） | ⚠️ 部分 | 传输层实证：S11 隧道 + S3/S13 回环 SSE 存活（155s 空闲不断连）；M3 跃迁→横幅逻辑有单测回归锁。真机 2s 横幅时序⬜【人工：真机蜂窝 + 真实会话跃迁复核】（自动化窗口期与并发会话互相干扰，未取得稳定横幅录像） |
| S13 | 重启免重连 | ✅ | 应用进程重启（KV enabled=true → 自动恢复服务 + 绑定）→ 已配对 context 刷新 → 免重连直接上板；设备行 DB 存活实证 |
| S14 | 托盘入口 | ⚠️ 部分 | 托盘菜单项/开关/地址复制已实现且代码评审通过；菜单数据源（tray_display 隧道优先、勾选态）纯核测试覆盖。NSStatusItem 菜单对本自动化不可达（合成事件不打开菜单）⬜【人工：点一次托盘 → 验证勾选态与地址复制（`pbpaste`）】 |

记录契约：每场景一行（结果 / 证据索引 / 数字），失败场景修复后重跑（S7 重跑两轮、S11 重跑一轮，均通过）。

## 四、人工待办清单（⬜ = 待用户）

1. **外网蜂窝全链路**：手机蜂窝网络访问 named tunnel 固定子域（需用户提供 Tunnel Token 后走 S11 半自动流程）→ 看板与内容可用、2s 提醒、断网自愈。
2. **彻夜保活**：Windows 实机远程彻夜开启不失联；`powercfg /q` 磁盘休眠代设/还原对照（macOS 侧 caffeinate 断言已实证）。
3. **named tunnel 真 Token 流程**：填 Token → 固定子域接入（临时隧道 quick 模式已实测，named 流程同路径）。
4. **S12 真机复核**：真机蜂窝 + 页面开 + 真实会话跃迁 → 横幅 ≤2s + 提醒音。
5. **S14 托盘点测**：点托盘图标 → 菜单「远程接入」勾选态与设置页一致；点地址项 → 剪贴板（`pbpaste` 验证）；0.0.0.0 未确认时开关失败有系统通知（AX 对 NSStatusItem 不可达，计划已预授权人工兜底）。
6. **PWA 主屏安装**（v6 §5 体验项，真机 Safari 操作）。

## 五、已知限制（E2E 实测登记）

1. **审批队列同 IP 占位含已消费项**：批准后 5 分钟内，同来源 IP 的新请求被拒（「请求过多」）——安全方向 fail-safe，多设备同 NAT 场景需等 TTL。
2. **stop/通道关闭后 cloudflared 孤儿进程**：`kill_on_drop` 在 supervisor abort 路径未稳定生效（实测 1 例孤儿，手动清理）；边缘连接已断（530），无流量泄露。
3. **异常退出遗留 caffeinate 孤儿**（macOS）：SIGKILL 级退出后 `caffeinate -ims` 存活，阻止空闲睡眠直至手动清理——Task 10 已知限制，崩溃恢复仅覆盖 Windows 磁盘代设（spec §8 范围）。
4. **check:i18n 只验中英成对**，不校验「代码引用的键在 locales 中存在」——Task 8 缺陷漏网根因，已由 useRemoteEvents 专项测试补哨兵。
5. **pair_poll 满员时维持 pending**（修复后语义）：批准后满员，手机轮询停留在等待态而非报错——腾位后下次 poll 自动补上凭证。
6. **审批面板数据 3s 轮询**：桌面待审批/花名册为轮询刷新（非推送），极端时序下批准按钮位置随列表刷新轻微移动。
7. **>500 kB chunk 体积告警**（build:mobile）：manualChunks 分包移二期（D17 附注）。
8. **快速隧道 URL 随重启变化**：quick 模式地址不持久（spec T1c 既定），固定地址需 named tunnel + 真 Token（人工项）。

## 六、E2E 期间代码修复（全部带回归测试，commit 见第一节）

1. `5f136a2` — pair_poll Approved 落库前复核设备上限（TOCTOU 封堵）+ 回归测试 `pair_poll_defers_when_cap_full`（满员 poll → pending 且无 Set-Cookie；腾位后重 poll → 放行）。
2. `8de69c1` — 上式重入死锁修正：`max_devices_source()` 求值移出 `store.with` 闭包（with 持 DB 锁不可重入，AGENTS.md 红线；E2E 实测进程冻结根因，sample 栈实锤主线程阻塞于 rusqlite Connection lock）。
3. `24dc04f` — remote_set_channel 显式停旧启新（off→quick/named 隧道永不拉起的计划级缺口）。

## 七、遗留问题

1. named tunnel 真 Token 流程未实测（人工项 3）；`parse_named_url` 解析不到子域时设置页显示「地址以 Cloudflare 面板为准」。
2. macOS 崩溃孤儿 caffeinate 无自动恢复（spec §8 崩溃恢复仅承诺 Windows 磁盘代设）。
3. 推送网关（ntfy/Bark）随 D17 移二期；APK 壳同步移二期。
4. M3/M2 既有行为零回归：全量门禁绿 + E2E 复验（S1 绑定/TLS 门、S3 直通配对、S13 免重连、降级横幅）未见回归。

## 八、终审（全分支评审）与修复波

- 终审判定：Needs fixes → 3 项 Important 全部修复 → 范围复审 3/3 ADDRESSED → **Ready to merge**。
- 终审修复波（`20e3035`，4 文件 +15/−6）：
  1. 远程关闭时通道切换不再拉起隧道——`remote_set_channel` 的 `start_if_configured` 前加 `handle_is_live` 存活门（此前远程关着点「临时隧道」会下载并 spawn 指向死端口的公网 URL）；
  2. `notice2` 安全警示文案更新——「临时直通模式/审批制后续提供」与已交付的审批配对自相矛盾，改为如实描述（审批配对已可用 + 直通码保留一次性分享）；`remote.desc` 同步去掉「与本机直连」排斥措辞；
  3. spec T2e 审计补全——`pair_confirm` 的 Wrong 分支补 `pair_confirm_wrong` 留痕（敌意试码前两次失败此前无痕）；`stop_server` purge 计数并入 `events::audit`。
- 终审其余结论：无 Critical；全部 32 条 deferred minors 裁定可遗留（triage 表见台账）；跨任务一致性（事件名/wire camelCase/KV 键/cookie 语义/i18n）与 spec 覆盖逐项核查通过；两处形态偏差如实记录（T2c 批准按钮以 toast 报满替代禁用态——服务端门更强；T2e 剩余 NotFound/Expired 试码无痕）。
