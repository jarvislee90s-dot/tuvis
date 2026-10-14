# M4 远程接入修复批：隧道地址可达性 + TLS 弹窗化 + Token 面板 + 下载器加固 · 实施计划

<!-- 历史标注（2026-10-14 · #131）：本文档成文于「命名隧道 → 自有域名」改名前；文中旧卡名「命名隧道」即现行「自有域名」卡，属历史记录，按裁决不改原文。 -->

> 分支基线：`feat/m4-external-fullchain`（PR #68）。本计划的所有修复针对 PR #68 引入/暴露的问题，实施时直接在该分支追加 commit。
> 根因定位已完成（2026-09-17，systematic-debugging 流程），本文「关键发现」即为定位结论；「用户已拍板决策」为最终结论，实施时不得重议。

---

## 一、背景与关键发现（根因定位结论）

### F1 · 隧道地址缺 `/m` → 打开报 "not found"（bug）
- 服务端只伺服 `/m` 前缀；根路径 `/` 明确返回 404，响应体就是小写 `"not found"`（`src-tauri/src/remote/server.rs:116` static_fallback）。
- `parse_quick_url` 从 cloudflared stderr 截取的是**裸域名**（`tunnel.rs:182`），快照 `url`、地址表、toast、托盘复制全用它；全代码库唯一补 `/m` 的地方是配对链接 `pair_url_with_tunnel`（`mod.rs:817`）。
- 链路本身通（`tunnel.rs:286` 转发 `http://127.0.0.1:{port}` 正确），用户点 toast 里的裸地址 → axum 404 "not found"。

### F2 · 设置页无隧道状态刷新订阅 → 常驻位永远空，只剩 toast（bug）
- 设计上地址表会常驻：`host_payload` 把隧道地址插 `addresses` 首位（`mod.rs:789` address_entries_with_tunnel，kind=tunnel 带徽标），设置页也有「当前隧道地址」行（`RemoteSection.tsx:482`）。
- 但 `remote_status` 只在进面板/开关/改配置动作时读一次；**无轮询、无事件订阅**（`remote-changed` 只有托盘在听 `home.tsx:91`；`remote-tunnel-address` 只弹 toast `useRemoteEvents.ts:42`）。隧道拉起异步（数秒），地址到手时只有 toast 知道，页面不知道。

### F3 · 命名隧道 Token 输入框死锁（bug）
- 输入框存在但只在 `channel === "named"` 时渲染（`RemoteSection.tsx:456`）；后端 `remote_set_channel` 在 Token 空时**先 return Err、后写 channel**（`mod.rs:592-599`）→ channel 永远变不成 named → 输入框永远不出现 → 无处填 Token。鸡生蛋死锁。

### F4 · cloudflared 下载失败（网络 + 下载器缺陷）
- 下载 URL 本身正确（GitHub 官方 latest 跳转）；`error sending request` 是 reqwest **连接层错误**（连不上 github.com），非 404。
- 直接放大器：`src-tauri/Cargo.toml:64` `reqwest = { default-features = false, features = ["json", "rustls-tls"] }`——关掉了默认特性 **`system-proxy`**（已查 docs.rs reqwest 0.12.28 证实为默认特性）→ 下载器不读 Windows/macOS 系统代理；"浏览器能下、MAM 不能" 与此吻合。
- 次要缺陷：无超时、无镜像回退、失败文案只给 `~/.mam/bin/` 相对写法（Windows 用户不知道是哪）、无完整性校验。
- 手动放置旁路（`~/.mam/bin/cloudflared.exe`，`tunnel.rs:34` + `binary_ready` 短路下载）工作正常，用户实测通过。

### F5 · TLS 勾选框 / Tailscale 提示现状
- TLS 勾选框仅在 `bind === 0.0.0.0` 时渲染（`RemoteSection.tsx:492-512`）；后端 P7 门在 `mod.rs:214`（`is_external_bind && ack != "true"` → Err），`remote_confirm_public` 只写 KV（`mod.rs:577`）。**后端语义本轮零改动**。
- Tailscale 提示为常驻一行文案（`RemoteSection.tsx:684` + `settings.remote.tailscaleHint`）；已核实测试无断言引用，删除无牵连。

---

## 二、用户已拍板决策（2026-09-17）

1. **TLS 确认 → 方案 A**：删常驻勾选框；点「开启远程接入」且绑定=0.0.0.0 且未确认过时，弹一次性 Dialog（"将明文 HTTP 向局域网开放会话内容"），点「已知晓并开启」→ 确认落库并继续开启。后端 P7 门与 `remote.public_ack` 语义不变。
2. **Tailscale 提示：整行删除**（UI + 中英文案）。理由：受众过窄。
3. **cloudflared 分发：修下载器**（系统代理 + 超时 + 固定版本 + sha256 校验 + 镜像回退 + 完整错误指引），**不**打包进安装包、**不**进源码仓库。
4. **隧道地址常驻且可直达**（F1/F2 的修复目标，用户报障即视为要求）：地址统一带 `/m`，设置页 ≤3s 自动出现。
5. **Token 有地方填 + 轻量引导**（F3 修复目标，用户原话「修补完 能做个轻量引导」）。
6. **本轮不做**：接入方式四选一单选重排（涉及 spec「绑定与通道独立并存」裁决修订，用户未拍板，挂起待议）。

---

## 三、文件结构

```
src-tauri/src/remote/tunnel.rs              改：board_url 归一（F1）；固定版本 + sha256 + 镜像下载内核（F4）
src-tauri/src/remote/mod.rs                 改：pair_url_with_tunnel 去重 /m（F1）
src-tauri/Cargo.toml                        改：reqwest += "system-proxy"；新增直依 sha2（F4）
src/components/settings/RemoteSection.tsx   改：3s 状态轮询（F2）/ Token 面板（F3）/ TLS Dialog（决策1）/ 删 Tailscale 行（决策2）
src/i18n/locales/zh.json、en.json           改：增 tunnelTokenGuide*/tlsDialog*，删 tailscaleHint/tlsAck*（保留仍被引用键）
tests/settings/remoteSection.test.tsx       改：F2/F3/决策1/决策2 的前端用例（增删）
src-tauri/src/remote/tunnel.rs（tests 模块） 改：F1/F4 纯函数用例
docs/superpowers/specs/2026-09-16-m4-external-fullchain-design.md  改：2026-09-17 裁决追记
research/README.md                          改：登记 cloudflared 固定版本与升级流程
```

测试参照：`tests/settings/remoteSection.test.tsx` 已有 `remote_status` invoke mock 模式（`if (cmd === "remote_status") return {...}`），新增用例沿用。

---

## 四、核心设计

### 设计 1 · 隧道地址单点归一为看板地址（修 F1）

- `tunnel.rs` 新增纯函数 `board_url(base: &str) -> String`：去尾 `/` → 追加 `/m`；已以 `/m` 结尾则幂等不重复。
- **归一在摄取点**：stderr 解析任务 `set_snapshot(|s| s.url = ...)` 前（quick 与 named 同一路径）先过 `board_url()`——快照 `url` 从此恒为看板完整地址，所有消费方自然修正：
  - `addresses` 表首位条目（设置页地址区，带「外部通道」徽标）
  - `remote-tunnel-address` toast 载荷
  - 托盘「复制远程地址」（`tray_display`）
  - 设置页「当前隧道地址」行（status.tunnelUrl）
- `pair_url_with_tunnel` 同步改契约：入参已是看板地址，去掉手工 `"/m"` 拼接 → `format!("{}#token={token}", t.trim_end_matches('/'))`，防 `/m/m`；LAN 分支（base_url 本就含 `/m`）不变。其单测（`mod.rs:1306-1314`）随契约更新。

### 设计 2 · 设置页 3s 状态轮询（修 F2）

- 既有 3s interval（`enabled` 门控，现只刷待审批 + 花名册）顺带刷新隧道状态。新增轻量 `refreshStatus()`（只 `setStatus(await remoteStatus())`，**不**回读 ack/hostName——不 clobber 编辑中的输入框；失败静默，下一拍自愈），并入该 interval。
- 效果：隧道地址到手后 ≤3s 地址区与「当前隧道地址」自动出现；toast 保留不变（地址经设计 1 已带 `/m`）。

### 设计 3 · Token 面板死锁解除（修 F3）——前端编排，后端零改动

- `changeChannel("named")` 时若 `token.trim()` 为空：**不调后端**，本地置 `tokenPanelOpen = true`，展开 Token 面板（当前通道不变、隧道不受影响）。
- 面板渲染条件：`channel === "named" || tokenPanelOpen`（已切 named 的老用户照常回填显示）。
- 面板内容：password 输入框（沿用既有 `remote-tunnel-token`）+ 保存 / 取消按钮 + **轻量引导**：
  - 三步文案（i18n）：① 登录 Cloudflare Dashboard → Zero Trust → Networks → Tunnels；② Create a tunnel → 选 Cloudflared → 命名；③ 复制安装页的 `eyJh...` Token 粘贴到此处
  - 「打开 Cloudflare 文档」外链（`https://developers.cloudflare.com/cloudflare-one/connections/connect-networks/`，实施时核实最终 URL）
  - 警示行：Token 等价于隧道凭据，勿外传
- 保存 → `remoteSetChannel("named", token)`（后端既有校验/落库/热切换路径原样）；取消 → 关面板，通道保持原值。
- 后端 `remote_set_channel` 的空 Token 拒绝路径**保留**为兜底（防止其他入口写入），前端不再把它当首次入口。

### 设计 4 · TLS 确认弹窗化（决策 1）——后端零改动

- 删常驻勾选框区块（`RemoteSection.tsx:489-512`）及取消方向 toast 逻辑（`tlsAckNoRevoke`/`tlsAckRevokeByRebind`）。
- 开关 `onCheckedChange(true)` 分流：`bind === 0.0.0.0 && !acked` → 开 Dialog；否则直接 `enable()`（127.0.0.1 与已确认路径无感）。
- Dialog：标题「对外绑定安全确认」；正文「将以明文 HTTP 向局域网开放全部会话内容。地址即钥匙，请确保当前网络可信；确认后长期生效，撤销需改回仅本机模式。」；按钮「取消」/「已知晓并开启」。
- 确认动作：`await remoteConfirmPublic()` → `setAcked(true)` → `enable()`（enable 既有 busy 互斥与失败 toast 原样）。
- 后端门（`mod.rs:214`）不动——弹窗只是把「何时调 remote_confirm_public」从勾选框换到开启动作。

### 设计 5 · 删 Tailscale 提示（决策 2）

- 删 `RemoteSection.tsx:684` 一行 + zh/en 的 `settings.remote.tailscaleHint`。无其他引用（已核实）。

### 设计 6 · cloudflared 下载器加固（决策 3，修 F4）

1. **系统代理**：`Cargo.toml` reqwest features 增 `"system-proxy"`（恢复读取 Windows 注册表 / macOS 系统代理——本次失败的直接放大器）。
2. **版本固定**：新常量 `CLOUDFLARED_VERSION = "2026.9.1"`（用户实测版本）；下载 URL 从 `releases/latest/download/{asset}` 改 `releases/download/{版本}/{asset}`。固定版本 + 固定 sha256 是镜像回退不引入供应链风险的前提；升级流程 = 改常量 + 更新四平台 sha256 表（登记在 `research/README.md`，符合 AGENTS「持续跟踪外部依赖版本」惯例）。
3. **sha256 校验**：新增直依 `sha2 = "0.10"`（lock 树已有 0.10.9，零新增传递依赖）。各平台期望值在实施 Task 6 时从该 release 页面取全（Windows amd64 用户已提供 `2837888cc0f5…`，**实施时须与 release 页核对**，不盲信会话转述）；macOS `.tgz` 校验对象为下载的压缩包本身（解压前）。
4. **镜像回退**：候选序 = 官方 → `https://ghfast.top/` 前缀 → `https://gh-proxy.com/` 前缀（前缀 + 完整原始 URL 形态）。每个候选下载后校验 sha256，不匹配即弃用换下一个；全失败才报错。实施时先实测镜像可用性，不可用的从候选表剔除。镜像失效的最坏情形 = 回落到官方 + 手动放置旁路，sha256 兜底完整性。
5. **超时**：`Client::builder().connect_timeout(15s).timeout(600s)`（60MB 级二进制容忍慢网，同时杜绝永久挂起）。
6. **错误指引**：失败文案给出**完整绝对路径**（如 `C:\Users\<user>\.mam\bin\cloudflared.exe`，由 `cloudflared_path().display()` 生成，注意 `bin_path` 已被 move 进 spawn_blocking 闭包，需在 move 前留显示串）+ 官方下载页链接 `https://github.com/cloudflare/cloudflared/releases`。
7. **零网络红线**：`Downloader` 注入缝保持；单测只测编排纯函数——候选 URL 序、sha 不符回落下一候选、终态错误文案内容。

---

## 五、实施顺序（TDD，7 个 commit，只 commit 不 push）

### Task 1 · 隧道地址 `/m` 归一（backend，修 F1）
- 红：`board_url` 用例（裸域名补 `/m`、已含不重复、尾 `/` 去重）；`pair_url_with_tunnel` 新契约（tunnel 入参含 `/m` + `#token`；LAN 分支不变；**不得出现 /m/m**）。
- 绿：实现 `board_url` + stderr 摄取点归一 + `pair_url_with_tunnel` 简化。
- 门禁全绿 → `fix(m4-tunnel): 隧道地址统一归一为看板地址——修裸域名 404 not found`

### Task 2 · 设置页 3s 状态轮询（frontend，修 F2）
- 红（tests/settings/remoteSection.test.tsx，沿用 remote_status mock 模式）：enabled 且隧道地址延迟到手时，interval 触发后地址区自动出现；refreshStatus 不回读 hostName/ack（编辑不 clobber）。
- 绿：`refreshStatus` 拆分 + 并入既有 3s interval。
- 门禁全绿 → `fix(m4-settings): 设置页随 3s 轮询刷新隧道状态——地址常驻不再只靠 toast`

### Task 3 · Token 面板 + 轻量引导（frontend，修 F3）
- 红：空 Token 点「命名隧道」→ 不调 `remote_set_channel`、面板出现、引导三步与外链渲染；填 Token 保存 → `remoteSetChannel("named", token)` 被调；取消 → 面板关、后端未调；已切 named 的回填显示不回归。
- 绿：`tokenPanelOpen` 状态 + 面板 JSX + i18n（zh/en 增 `tunnelTokenGuide*`/`tunnelTokenDoc`/`tunnelTokenWarning`）。
- 门禁全绿 → `fix(m4-settings): 解除命名隧道 Token 死锁——选 named 即展开输入面板与三步引导`

### Task 4 · TLS 确认弹窗化（frontend，决策 1）
- 红：0.0.0.0 未确认点开 → Dialog 出现且 `remote_toggle` 未调；确认 → `remote_confirm_public` 后 `remote_toggle(true)` 依序被调；取消 → 两者皆未调；127.0.0.1 → 无 Dialog 直开；已确认 → 无 Dialog 直开；**勾选框不再渲染**。
- 绿：删勾选框区块、接 Dialog；i18n 增 `tlsDialog*` 删 `tlsAck`/`tlsAckHint`/`tlsAckNoRevoke`/`tlsAckRevokeByRebind`（先 grep 确认无残留引用）。
- 门禁全绿 → `feat(m4-settings): TLS 对外绑定确认改一次性 Dialog——常驻勾选框退场`

### Task 5 · 删 Tailscale 提示（frontend，决策 2，tiny）
- 红：断言 `tailscaleHint` 文案不渲染。
- 绿：删行 + 删两 locale 键。
- 门禁全绿 → `chore(m4-settings): 移除 Tailscale 指引文案（用户裁决：受众过窄）`

### Task 6 · 下载器加固（backend，决策 3，修 F4）
- 子步骤：先 WebFetch `releases#release-2026.9.1` 页取四平台 sha256（并核对用户提供的 Windows 值）+ 实测镜像前缀可用性。
- 红：固定版本 URL 形态；镜像候选序；sha256 校验（正确通过 / 错误弃用回落）；`ensure_with` 失败错误文案含绝对路径与下载页链接；既有 `download_url_matches_platform` 用例随固定版本更新。
- 绿：Cargo.toml（reqwest += system-proxy；sha2 直依）；`download_to` 重构（builder 超时 + 候选循环 + 校验）；错误文案（bin_path move 前留显示串）。
- 门禁全绿 → `fix(m4-t1d): cloudflared 下载器加固——系统代理/固定版本+sha256/镜像回退/超时/完整指引`

### Task 7 · 文档收口（docs）
- spec `2026-09-16-m4-external-fullchain-design.md` 追记 2026-09-17 裁决：TLS 弹窗化（A）/ Tailscale 提示删除 / Token 面板交互 / cloudflared 固定版本策略。
- `research/README.md` 登记 cloudflared 2026.9.1 与升级流程。
- 门禁（fmt/check 若涉及）→ `docs(m4): 三项用户裁决追记与 cloudflared 版本登记`

（2026-09-17 实施追记：research/README 登记经用户裁决改道 M4 spec §12.5——research/ 不入库；sha256 实取五平台值。）

---

## 六、门禁（每个 commit 前全绿）

```bash
cd src-tauri && cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --check
pnpm test && pnpm build:mobile && pnpm build && pnpm format:check && pnpm lint
```

教训沿用（M3 批次）：**必须显式跑 `pnpm build`**（tsc 门在其中）；命令逐段执行，管道 `| grep/tail` 会吞退出码。涉及 Rust 依赖变更的 Task 6 之后首次构建会拉取 `system-proxy` 的传递依赖，属预期。

---

## 七、不做（本轮）

- 接入方式四选一单选重排（第 (5) 项）与 127/0.0.0.0 概念隐藏（第 (6) 项）：用户未拍板，且涉及 spec「绑定与通道独立并存」裁决修订，挂起待议。
- cloudflared 打包进安装包 / 进源码仓库：已裁决不采用。
- 根路径 `/` 302 → `/m`：设计 1 的最小修复已解决可达性；302 可作后续池（收益：手敲裸域名也能进）。
- 二进制预览回落、dsh 大库性能等既有遗留：与本轮无关，维持原台账。

---

## 八、已知边界

- 镜像域名是第三方公共服务，可能失效或变更：sha256 校验兜底完整性；失效时自动回落官方源 + 手动放置旁路；候选表随 research/README 维护。
- 版本固定意味着 cloudflared 安全更新滞后：升级 = 改 `CLOUDFLARED_VERSION` + 更新 sha256 表（research/README 流程），不追 latest。
- `tokenPanelOpen` 展开期间后端 channel 未变：面板开着时隧道仍按旧通道运行，保存才切换——预期行为。
- 已手动放置 cloudflared 的机器（含当前用户）走 `binary_ready` 短路，Task 6 对其无感；加固收益体现在全新安装与 macOS 用户。
- TLS Dialog 的「已确认」状态无在线撤销入口（沿用 M4 T0b 语义）：撤销 = 改绑仅本机（后端门只对外部绑定生效），弹窗正文已说明。
