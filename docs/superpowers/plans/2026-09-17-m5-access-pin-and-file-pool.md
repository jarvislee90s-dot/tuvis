# M5 远程接入重设计（访问密码制）+ 文件预览增强 · 实施计划

<!-- 历史标注（2026-10-14 · #131）：本文档成文于「命名隧道 → 自有域名」改名前；文中旧卡名「命名隧道」即现行「自有域名」卡，属历史记录，按裁决不改原文。 -->

> 状态：**已批准并实施完毕**（2026-09-17 用户多轮对齐后批准）。阶段 A（A1–A9）按「subagent-driven-development」（A1–A6，实现子代理 + spec/质量双评审 + 修复回环）与「executing-plans」（A7 起，用户改裁：单轮自检评审）执行完毕，commit 范围 `f2db12f..30103e4`；阶段 B（B1–B5）同日实施完毕（`75d9097..`），B1 调研结论见 §九。手工验收清单见 §八（用户执行）。
> UI 唯一契约：`docs/superpowers/wireframes/2026-09-17-remote-settings-redesign.html`（**v5 定稿候选**，localhost 8940 可预览）。实现与线稿不一致即缺陷。
> 分支策略：建议在 `feat/m4-external-fullchain`（PR #68，tip f2db12f）上续作——密码制直接替代该 PR 中的审批制，避免审批流以废弃形态合入 main；PR #68 描述随改动更新。备选（不推荐）：M4 先合并再开替换 PR。

---

## 一、背景（为什么改）

用户实测暴露四类问题（根因已定位）：
1. **刷新掉线**：`stop_server`「停止=全吊销」不变量把改绑定/开关循环变成全设备下线；临时隧道每重启换域名，cookie 按域名隔离。
2. **设备数膨胀**：每次配对新建设备记录，换域名/重新申请即累加（1 浏览器 3 设备）。
3. **审批流体验差**：一次性接入码 / 请求审批 + 4 位码 / 同 IP 占位 429，入口过多且易锁死。
4. **设置窗口 600×500 过窄**，M4 内容折行严重。

用户裁决（2026-09-17，多轮对齐）：以**常驻访问密码（4 位数字）**替代审批制/直通码/扫码 token 三入口；四通道独立开关可同开；吊销收窄；文件池三来源 + 搜索 + HTML 源码/渲染双态。

### 与宪法的冲突裁决记录（重要）

MASTER-PLAN P6「审批配对 + 直通配对」的**手段**修订为「访问密码 + 设备指纹」——目标（可信设备接入、设备可吊销可管理）不变。此为用户在场明确裁决（2026-09-17 多轮），MASTER-PLAN 本体不动，M4 spec 以追记方式记录（实施 Task A9）。若评审发现与 MASTER-PLAN 目标层冲突，停下上报用户。

---

## 二、用户已拍板决策（实施不得重议）

1. **访问密码**：仅允许 4 位数字，可自填或随机生成；输入后点「保存」生效；仅对 局域网/临时隧道/命名隧道 生效（本机无需）；连续输错 5 次锁 10 分钟（按来源 IP）；新设备输一次绑定 180 天；重置密码 = 全部设备下线。
2. **登录状态常驻**：绑定后 180 天滑动有效；切通道开关 / 改设置 / 改绑定 / 重启 MAM 不掉线；只有显性关闭「开启远程接入」或重置密码才要求重新绑定。（**2026-09-18 修订**：显性关闭也不再吊销——吊销触发器收敛为「重置设备」与「修改访问密码」两个显式动作，开关切换只是停/起服务）
3. **设备指纹去重**：同一浏览器同一网络重新绑定 → 覆盖原设备记录，不新增。
4. **设备名称**：默认取设备自报（首次配对），桌面端可重命名保存；重命名后设备重连不丢名。
5. **四通道独立开关可同开**：本机 / 局域网 / 临时隧道 / 命名隧道各带开关；本机常驻锁死（局域网监听天然包含回环，物理上不可单独关）。小卡片一排，点击卡片展开**唯一**详情区。
6. **隧道生命周期**：通道开关 = 唯一启停入口；开 = 常驻（断线自动重连，切设置页/其它开关不影响）；临时隧道每次开关换新地址（卡片明示）；总开关关闭全停，重开按各卡状态恢复。
7. **三通道二维码** = 通道地址 + 密码参数（`#pin=`），扫码自动填入直接进；手输地址需输密码。
8. **命名隧道教程**：点击展开的信息框，分步详细（6 步），可保持展开边看边操作。
9. **设置窗口默认 880×640**（现 600×500）。
10. **文件来源三池**：我上传的 / 工具读取 / 工具读写；筛选片 + 全部；文件名搜索框**点「搜索」（或回车）才执行**，与追溯范围/来源筛选叠加。
11. **源码/渲染切换在文件预览页内**：仅 .md（现有渲染显式化）与 .html（新增，沙箱 iframe）出现；其余类型原样，图片直接显示图片。
12. **上传附件入池按工具逐步支持**：先调研 kimi / zcode，其余跟进。
13. 本机名称默认填系统用户名，去掉灰字提示。
14. **二维码 = 常驻地址 + 密码**（扫码自动填直接进；密码更换/重置设备时随之失效）——替代旧「一次性 10 分钟直通码」，安全语义变化已由用户确认。
15. **TLS 明文确认 Dialog 触发点 = 开启局域网开关时**（0.0.0.0 由局域网开关驱动；后端 P7 门随 `remote_toggle_channel("lan", true)` 生效）。
16. **托盘维持总开关口径**：仅「远程接入」勾选（关 = 吊销全部设备）+ 复制地址；通道管理只在设置页。
17. **设备上限默认 3 → 10**（KV 未设置时的默认值；已存值不迁移；范围仍 1-10 可改）。

---

## 三、核心设计

### 3.1 认证流（替代审批制）

- 新端点 `POST /m/api/v1/pair/pin`，body `{ pin: string, name?: string }`：校验 PIN（限速门）→ 设备指纹 upsert → 下发 180 天 cookie（属性不变：`Path=/m; HttpOnly; SameSite=Lax`）。
- **限速状态机**（内存态，重启即清）：按来源 IP 记连错计数，5 次错 → 锁 10 分钟；锁定期直接 429（不泄露剩余次数预言机以外的信息；响应带 `retryAfter` 供文案）。
- **旧端点全部下线**：`/pair`（直通码）、`/pair/request`、`/pair/poll`、`/pair/confirm` 移除；gate 放行名单收口为 `/pair/pin`（结构与既有相对路径契约一致）。
- **本机免密（安全关键设计，评审必查）**：豁免条件 = **来源 IP 为回环 且 Host 头非隧道域名**。仅查回环**不够**——cloudflared 在本机把隧道流量从回环转发进来，只查回环会把全部公网隧道流量免密放进（穿透）。Host 校验：不匹配隧道快照域名（quick/named）即视为本地。实现为纯函数 + 测试锁定。

> **追记（2026-10-14 · #131 历史文档失效标注）**：本条「本机免密」豁免**已失效**——
> 2026-10-06 门禁零豁免裁决（#119）将其撤销：任何中继都会把公网流量从回环转进来，
> 「从回环来」不再等于「人在本机」；鉴权只认设备凭据，本机浏览器访问同样需要访问
> 密码。现行权威口径：`docs/superpowers/specs/2026-10-06-remote-fixed-address-and-gate-design.md`（§1.5 ②）。
> 本文档为历史记录，按裁决不改原文。
- PIN 存储：KV `remote.access_pin`；开启 局域网/任一隧道 时若未设置 → 后端自动生成并随 status 返回（UI 展示，可改）。

### 3.2 设备模型

- `remote_devices` 迁移加两列：`fingerprint`（sha256(UA + "|" + origin_ip)，hex）、`via`（本机/局域网/临时隧道/命名隧道）。
- **upsert 语义**：配对成功时按 fingerprint 查未吊销行 → 命中则覆盖（保留原 id 与**用户重命名过的名称**；名称仅在首次建行时取设备自报）→ 刷新 cookie；未命中或命中已吊销行 → 新建（吊销行不复活）。
- `via` 判定（配对时刻）：回环 + 本地 Host → 本机；Host = quick 域名 → 临时隧道；Host = named 域名 → 命名隧道；其余 → 局域网。
- 新命令 `remote_rename_device(id, name)`；花名册行渲染 via 徽标。

### 3.3 吊销收窄与重启语义

- `stop_server(revoke: bool)`：显性关闭远程（`remote_toggle(false)`）与重置密码 → `revoke=true`；**改绑定为自动热重启**（后端内部重启监听，不再要求用户手动停/开）→ `revoke=false`；MAM 重启本来就不吊销（维持）。
- 移除 approval 模块接线（approval.rs 及其命令/端点/桌面待审批面板/移动端请求 UI/相关 i18n）；保留花名册（列表/在线/踢下线/上限）。
- 设备上限语义保留（PIN 配对同样过上限门，满员拒绝并提示）；**默认值 3 → 10**（`max_devices_from` 的 None/乱串回落值改 10，已存 KV 不迁移；前端占位符同步）。

### 3.4 通道开关（后端）

- 三个布尔 KV：`remote.chan_lan` / `remote.chan_quick` / `remote.chan_named`；`bind` 不再是独立用户键——由 `chan_lan` 派生（on=0.0.0.0，off=127.0.0.1），迁移时旧 `remote.bind` 值映射进 `chan_lan`。
- **隧道多进程（quick 与 named 可同时开）**：现有 `tunnel.rs` 是单句柄模型（`TUNNEL: Mutex<Option<TunnelHandle>>`），改造为**每通道槽位**（`quick` / `named` 各自 supervisor 句柄），`tunnel::stop()` 拆为 `stop_channel(mode)` / `stop_all()`；`supervise` 按通道独立守护，快照聚合两通道地址（`addresses` 各带 kind）。同开两个公网入口属合法状态（用户裁决可同开）。
- 总开关开启 → 按各卡状态自动恢复：bind 监听 + 各开着的隧道；总开关关闭 → `stop_all` + 断电（既有联动）。单通道切换命令 `remote_toggle_channel(channel, on)`：幂等；隧道启停仅由本命令与总开关驱动（切设置页/其它设置不再触碰隧道）。
- `remote_status` 载荷扩展：`channels: { local, lan, quick, named }`（各开关与运行态）、`pin`（当前 PIN，设置页展示用）、各通道地址。前端 3s 轮询沿用。

### 3.5 桌面 UI（严格按线稿 v5）

- RemoteSection 重写三段式：① 通用（总开关[关闭=吊销语义文案] / 电源保活 / 本机名称默认系统用户名）；② 通道四小卡一排（本机卡开关锁死，局域网/临时/命名各带开关）+ 点击卡片展开唯一详情区（地址/复制/二维码/临时隧道换址警告/命名隧道 Token + 点击展开教程）；③ 访问与安全（PIN 输入 + 随机 + 保存 + 重置设备；已接入设备列表：在线点、via 徽标、重命名、踢下线、上限）。
- TLS 确认 Dialog 保留，**触发点 = 开启局域网开关时**（`remote_toggle_channel("lan", true)` 未过确认 → 后端 P7 门 Err，前端弹 Dialog，确认后重试）；语义与后端门零改动，仅触发载体随结构映射。
- 设置窗口创建参数 600×500 → 880×640（`main-title-bar.tsx`）。
- 托盘维持总开关口径（勾选 = `remote_toggle`，关 = 吊销全部 + 全停；地址复制随隧道/绑定口径既有逻辑）；通道管理只在设置页。
- i18n zh/en 同步；托盘「复制远程地址」等回归不动。

### 3.6 移动端

- PairPage 重写为密码单页：PIN 输入 + 进入；`#pin=` hash 自动填并自动提交（扫码路径）；错误文案（密码不正确 + 剩余次数 / 锁定提示）；旧「请求接入 / 4 位确认码」UI 移除。
- Board/SessionDetail/文件面板不涉及认证改动（gate 不变）。

### 3.7 文件来源三池（后端）

- `FileEntry` 增 `origin` 字段：`user` / `tool_read` / `tool_write`。tool_* 由现有 `modified` 推导（读类白名单既有判定）；`user` 来自新抽取源。
- 附件抽取（新函数，逐工具）：先 kimi / zcode——调研其会话文件中用户上传附件的存放形态（Task B1 产出黄金夹具），抽取为 `{path, ts}` 纳入合并管线（去重、mtime 排序沿用既有）；若工具附件不落盘（无路径），降级仅展示文件名（不可预览），计划内明确标注。
- `/session-files` 载荷加 `origin`；**筛选与搜索在客户端实现**（列表 ≤1000 条，无后端改动面）。

### 3.8 删除与新增清单（接口一致性对照，2026-09-17 已对现库核实）

**保留沿用（名称不变）**：`remote_toggle` / `remote_status` / `remote_devices` / `remote_revoke_device` / `remote_revoke_all_devices` / `remote_confirm_public` / `KEY_ENABLED` / `KEY_PORT` / `KEY_PUBLIC_ACK` / `KEY_HOST_NAME` / `KEY_TUNNEL_TOKEN` / `KEY_MAX_DEVICES`；pairing.rs 的 `NewDevice` / `persist_device`（改造为 upsert）/ `device_valid` / `touch_device` / `revoke_all` / `DEVICE_TTL_MS` / `device_cookie`；tunnel.rs 的 `snapshot` / `board_url` / `parsed_board_url` / `tunnel_address_entry` / `parse_quick_url` / `parse_named_url` / `cloudflared_path` / `ensure_with` / 下载器全套；gate.rs 的 `extract_device` / `gate`（放行名单改 `/pair/pin`）；api.rs 的 `persist_and_cookie`（内部改 upsert）；server.rs 路由结构契约（仅替换 pair 路由）；files.rs 的 `FileEntry`（加 `origin`）/ `PathSourceFn` / 合并管线；`tray_display_from` 与托盘事件；`SseRegistry` 全套。

**删除**：
- 命令：`remote_issue_token`（二维码改前端拼 `地址#pin=`，qrcode 库已有）、`remote_pending_requests`、`remote_approve_request`、`remote_set_channel`（被 `remote_toggle_channel` 替代）
- 端点：`GET` 相关 token 逻辑随 `remote_issue_token` 移除；`POST /pair`、`/pair/request`、`/pair/poll`、`/pair/confirm`
- 模块/类型：`approval.rs` 整个（ApprovalService / CreateRejection / ConfirmOutcome 等）、pairing.rs 的 `PairingService` / `PairingClock` / `issue()` / `stop()`
- 事件：`remote-pair-request`（useRemoteEvents 监听与 i18n 文案随删）
- KV：`KEY_CHANNEL`（"remote.channel"，迁移至 chan_quick/chan_named）、`KEY_BIND`（迁移至 chan_lan 后废弃）
- 前端封装/类型：`PairingToken` / `PendingRequest` / `pair()` / `requestPairing()` / `pollPairing()` / `confirmPairing()`（`src/lib/api/remote.ts` 与 `src/mobile/api.ts` 各自清理）
- i18n：审批/请求接入/直通码相关键（zh/en 同步）

**新增**：KV `remote.access_pin` / `remote.chan_lan` / `remote.chan_quick` / `remote.chan_named`；命令 `remote_toggle_channel(channel, on)` / `remote_rename_device(id, name)` / `remote_set_pin(pin)`（含重置语义拆分：`remote_reset_devices`）；端点 `POST /pair/pin`；`FileEntry.origin`。

### 3.9 移动端文件面板 / 预览


- FilePanel：追溯范围行右侧加来源筛选片（全部来源/我上传的/工具读取/工具读写）+ 搜索框（点「搜索」或回车才执行，按文件名模糊匹配，与范围/来源叠加）。
- FilePreview：预览页头部（返回箭头旁）加「源码/渲染」seg——仅 `.md` 与 `.html` 显示；.md 渲染态 = 现有 markdown 排版；.html 渲染态 = `/file` 取文本 → `sandbox="allow-scripts"`（**无** same-origin，与看板数据隔离）iframe srcdoc；其余类型不显示 seg。

---

## 四、实施顺序（TDD，两阶段 14 Task，每 Task 一 commit，门禁全绿）

### 阶段 A · 远程接入重设计

| Task | 内容（红→绿重点） | commit |
|---|---|---|
| A1 | 设备表迁移（fingerprint/via 列）+ upsert-by-fingerprint（覆盖保留 id/重命名；吊销行不复活）+ rename DAO + 测试 | `feat(m5-pin): 设备指纹 upsert 与 via 字段——同浏览器重绑不再新增` |
| A2 | PIN 内核：KV、校验纯函数、per-IP 限速状态机（注入时钟测试：5 错锁 10 分、到期解锁） | `feat(m5-pin): 访问密码内核与 per-IP 限速` |
| A3 | `POST /pair/pin` 端点（校验→upsert→cookie→via 判定）+ gate 回环豁免（**回环 + 本地 Host 双条件**，穿透测试锁定）+ 旧四端点下线与放行名单收口 + api 测试 | `feat(m5-pin): /pair/pin 认证端点——旧配对端点下线` |
| A4 | 吊销收窄：`stop_server(revoke)`、改绑自动热重启（不吊销）、重置密码命令、rename 命令、命令/事件接线 + 测试 | `feat(m5-pin): 吊销收窄——仅显性关闭与重置密码吊销` |
| A5 | 三通道开关后端：KV、bind 派生与迁移映射、总开关恢复逻辑、`remote_toggle_channel`、`remote_status` 载荷扩展 + 测试 | `feat(m5-pin): 三通道独立开关后端` |
| A6 | 桌面 RemoteSection 按线稿 v5 重写（四卡/唯一展开/教程 popover/PIN/设备列表 via+重命名）+ 窗口 880×640 + i18n + 组件测试 | `feat(m5-ui): 设置页远程接入按线稿 v5 重写` |
| A7 | 移动端 PairPage 密码单页 + `#pin=` 自动填 + 旧 UI 移除 + 测试 | `feat(m5-ui): 移动端配对页改访问密码单入口` |
| A8 | 托盘/审计/事件回归适配 + 全量回归修补（SSE 注册表、审计事件改名） | `chore(m5): 托盘与事件适配、回归修补` |
| A9 | 文档收口 A：M4 spec 追记（审批制→密码制裁决）+ 本计划入库 | `docs(m5): 访问密码制裁决追记` |

### 阶段 B · 文件预览增强

| Task | 内容 | commit |
|---|---|---|
| B1 | 附件调研：kimi/zcode 会话文件附件存放形态 + 黄金夹具（合成数据，零真实用户目录） | `chore(m5-files): 附件调研与黄金夹具（kimi/zcode）` |
| B2 | 抽取扩展：user 来源抽取 + `FileEntry.origin` + `/session-files` 载荷 + 测试 | `feat(m5-files): 上传附件入池与 origin 三源字段` |
| B3 | FilePanel：来源筛选（三源+全部）+ 文件名搜索（点搜索才执行）+ 测试 | `feat(m5-files): 文件池来源筛选与文件名搜索` |
| B4 | FilePreview：源码/渲染 seg（.md 显式化 / .html 沙箱 iframe）+ 测试 | `feat(m5-files): 预览页源码/渲染双态` |
| B5 | 文档收口 B + 手工验收清单；**用 `gh issue create` 挂追踪 issue**「文件池附件来源逐工具支持」：勾选 kimi / zcode（本批完成），列出 claude / codex / opencode / openclaw / workbuddy / dsh 待调研（含本计划 B1 的调研方法引用），后续每支持一个工具勾一项 | `docs(m5): 文件预览增强文档与验收清单` |

依赖关系：A1→A2→A3→A4→A5→（A6、A7 可并行评审串行实施）→A8→A9；B1→B2→（B3、B4）→B5。阶段 A/B 相互独立，可分开交付验收。

## 五、门禁（每 commit 全绿）

```bash
cd src-tauri && cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --check
pnpm test && pnpm build:mobile && pnpm build && pnpm format:check && pnpm lint
```

红线沿用：测试零网络、零真实用户数据目录（tempdir/合成夹具）；`cargo test` 前先 `pnpm build:mobile`（rust-embed debug 直读）；命令逐段执行防管道吞退出码。

## 六、不做（v2 池）

- 访问密码升级为更长/字母数字混合（现固定 4 位数字）；跨设备密码同步
- 其余工具（claude/codex/…）附件入池（先 kimi/zcode）
- 命名隧道多主机名、根路径 `/` 302 → `/m`
- saveToken 空值清 Token 等 M4 终审 6 条 Minor（随本批相关 Task 顺带修除外）

## 七、已知边界

- 4 位 PIN 强度依赖限速 + 地址保密；隧道场景公网可达，靠「地址随机 + PIN + 限速」三层。
- 指纹 = UA + 来源 IP：同机换浏览器 / 浏览器清 cookie / 运营商级 NAT 变化会新增设备行（预期行为，可踢下线）。
- 临时隧道地址仅存续于 cloudflared 进程生命周期；总开关关闭再开 = 新地址（线稿已明示）。
- 附件调研结论可能为「某工具附件不落盘」→ 该工具降级仅展示文件名。
- 本机免密的双条件豁免是安全敏感点：Task A3 评审必须专项验证「隧道流量无法借回环穿透」。

   > **追记（#131）**：本处「本机免密」已随 2026-10-06 零豁免裁决失效（依据与现行口径见本文件上方首条追记）。

## 八、手工验收清单（批准后随 Task 展开，先给用户级摘要）

1. 开局域网 → 手机输一次密码进看板 → 改绑定/切隧道开关/重启 MAM → 刷新仍在线；显性关闭远程 → 手机回配对页。
2. 同一浏览器重复绑定 → 花名册不新增；重命名后重连名字不变；踢下线 → 该设备需重输密码。
3. 密码输错 5 次 → 锁 10 分钟；重置密码 → 所有设备下线。
4. 临时隧道开关 → 地址变化且卡片有警告；命名隧道按教程操作可跑通（需 CF 域名）。
5. 文件池三个来源筛选正确；搜索点按钮才执行；HTML 点「渲染」出页面且脚本被沙箱隔离。

---

## 九、B1 附件调研结论（2026-09-17 实测，实施期追记）

> 调研方法：只读实测本机真实会话数据（kimi wire.jsonl 多会话 + zcode cli/db/db.sqlite），抽取**结构形态**；黄金夹具一律合成数据（`tests/fixtures/attachments/` + `remote/attachment_fixtures.rs`），零真实路径入库。

### 9.1 kimi（wire.jsonl 事件流）

- **用户输入为纯文本**：`turn.steer.input[*].text` 与 `context.append_message.message.content[*].text`（role=user）——未发现独立的"附件对象"结构。
- **原始路径以内联标记出现在文本里**：`<image path="E:/demo/fig.png">…</image>`、`<file path="…">`（HTML 风格属性，路径含正斜杠/反斜杠双形态）。
- **贴图走内容寻址 blob**：消息内容块 `{"type":"image_url","imageUrl":{"url":"blobref:image/png;<sha256>"}}`，实体在会话目录 `agents/main/blobs/<sha256>`（无扩展名、无原始名）——**不可按原始路径预览**，降级口径 = 不入池（文件名无从谈起）或仅展示 blob 引用（实施取不入池，避免噪音）。
- **抽取口径（B2）**：从 user 消息文本中提取 `<image path="X">` / `<file path="X">` / `<image src="X">` 属性值 → origin=user。

### 9.2 zcode（cli/db/db.sqlite，message/part 表——content.rs 同源）

- **用户附件在 part 表**：`{"type":"file","mime":"image/png","filename":"cover.png","url":"zcode-artifact://<session_id>/tool-result-<uuid>","source":{"type":"file","path":"<原始绝对路径>"}}`。
- **本地路径型**：有 `source.path`（如微信临时目录里的 jpg）→ 原始文件落盘可直接预览 → origin=user。
- **artifact 型**（粘贴截图）：无 `source`，`url` 为 `zcode-artifact://` URI → 磁盘实体在 `~/.zcode/cli/artifacts/<session_id>/*<tool-result-<uuid>>*`（文件名尾段内嵌 uuid，glob 尾段匹配可解析）→ 解析命中则按该路径入池预览，未命中降级仅文件名。
- **`input_history.attachments` 辅助表**（`[{"type":"image","path":"…"}]` / `{"path":"image.png","content":"zcode-artifact://…"}`）与 part 表形态互证；B2 取 part 表为主（会话消息流同源，input_history 跨会话共享、以 session_id 关联亦可作补充源——实施只取 part 表，保持数据同源铁律）。

### 9.3 其余六工具

claude/codex/opencode/openclaw/workbuddy/dsh 未在本批调研范围——追踪 issue 按 B5 落档（kimi/zcode 勾选完成，其余待调研）。

---

## 十、手工验收清单（M5 全批，用户执行；√ = 自动化已覆盖，手工复核交互观感）

### 阶段 A · 远程接入重设计

1. **登录常驻**：桌面开「开启远程接入」+ 局域网 → 手机输一次密码进看板 → 改绑定（局域网开关）/切隧道开关/重启 MAM → 手机刷新**仍在线**；桌面显性关闭远程 → 手机刷新回配对页。
2. **设备去重与花名册**：同一浏览器重复配对 → 花名册条数不变（不新增）；重命名设备后该设备重连 → 名字保持；踢下线 → 该设备刷新需重输密码；上限徽标 N / 10。
3. **限速与重置**：手机连续输错密码 → 第 5 次起提示锁定，10 分钟内即使密码正确也拒；桌面「重置设备」→ 所有设备下线；改 PIN 保存 → 所有设备需重新输入（首次设置不吊销）。
4. **通道四卡**：本机卡开关锁死（随总开关）；局域网开 → 触发 TLS 明文确认 Dialog（首次），确认后 0.0.0.0 生效；临时隧道开 → 地址显示且换址警告在场；命名隧道按六步教程操作 → 状态变「运行中」（需 CF 域名）；quick 与 named 同开 → 两卡各自运行态与二维码独立。
5. **本机免密**：本机浏览器直接访问 http://127.0.0.1:9420/m → 无需配对直达看板（隧道关闭态）。

   > **追记（#131）**：本处「本机免密」已随 2026-10-06 零豁免裁决失效（依据与现行口径见本文件上方首条追记）。
6. **二维码**：三通道二维码内容 = 地址 + `#pin=<密码>`；手机扫码 → 自动填入直接进入。
7. **托盘**：托盘勾选 = 总开关（关 = 全设备下线）；复制地址随当前通道口径。

### 阶段 B · 文件预览增强

8. **来源三池**：kimi 会话（有 `<image path=>` 引用）→ 文件池「我上传的」可见原始文件；zcode 会话（粘贴截图）→ artifact 实体入池可预览；来源筛选片 全部来源/我上传的/工具读取/工具读写 各自过滤正确。
9. **文件名搜索**：输入后**不点搜索不生效**；点「搜索」或回车 → 按文件名模糊匹配（大小写不敏感）；与追溯范围/类型档/来源筛选叠加；无匹配显示「无匹配文件」。
10. **源码/渲染双态**：.md → seg 默认「渲染」（现有排版），切「源码」看原文；.html → seg 默认「源码」，点「渲染」出沙箱页面（脚本可跑、地址栏与看板数据隔离——iframe 内访问 parent 应受限）；其余类型与图片无 seg。
11. **行内来源徽标**：文件行次行显示 我上传的/工具读取/工具读写 徽标（旧载荷条目无徽标）。

### 已知边界（预期行为，非缺陷）

- 微信临时目录等**会话工作区之外**的上传文件：入池展示，但点击预览会被 `/file` 安全边界（会话 cwd ∪ 用户主目录）拒绝（403「无法预览该文件」）——边界收紧属安全设计，跨盘预览能力如需放开另立裁决。
  - **（2026-09-18 追记）上一条已失效**：`3d22e2e` 按用户裁决把预览**全盘放开**——删除 cwd ∪ home 越界判定，主目录外不再 403；403 语义改为结构化原因码（`sensitive`/`too_large`/`not_found`/`not_file`/`io`）。同 commit 引入一处生产回归（端点传 `None` 致主目录内敏感黑名单整段失效），修复计划见 `docs/superpowers/plans/2026-09-18-file-preview-sensitive-blacklist-fix.md`。
- kimi 贴图（blobs 内容寻址）无原始路径，不入池。
- 临时隧道每次开关换新地址；隧道错误态下本机访问也需配对（fail-closed）。
