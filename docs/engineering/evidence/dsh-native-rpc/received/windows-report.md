# Windows 官方 DSH Desktop 原生远控接入探测报告

> 范围声明：**本报告只覆盖 Windows 单平台探测**（机器 MateBook16s，2026-10-28），不是双平台结论，也不是兔维斯产品验收。Mac 侧结论未沿用，只在必要处标注「Mac 前轮」作对照。本轮**不进入正式功能开发**，只做取证与路线证据供给。

**结论标签口径**（全文统一）：

| 标签 | 含义 |
| --- | --- |
| 源码确认 | 在 Windows 本机安装包的源码/文档中直接读到，附文件与行号 |
| 实测通过 | 本轮在本机真实调用并拿到预期响应，附脱敏日志 |
| 推断 | 由源码与实测外推，未直接验证 |
| 未验证 / 未测 | 缺条件（未凑齐前置状态、需改动真实配置、需用户点击），**如实登记，不宣称** |

---

## 1. 环境、版本、安装包来源与源码摘要

### 1.0 已读总纲与既有档案（本轮不修改宪法）

按任务要求先读了项目宪法与既有 DSH 档案，本节登记「已读 + 与本任务的约束关系」：

| 已读资产 | 关键行 | 对本任务的约束 |
| --- | --- | --- |
| `docs/MASTER-PLAN.md` | L205 **D14**：dsh 纳入第八受管工具，一期只读监控（读其 `storages/sessions`，「读其 HTTP API 亦可评估」），「**写通道视其 web API/插件生态另评**」；L358 附 A 矩阵 dsh 行「◐（其 web API/插件生态另评）/ ◐（CLI headless 待探）/ ✅ 自带 web UI（dsh web）/ 自带 web + 插件生态」 | 本报告的定位就是给 D14「另评」补 Windows 证据；**不得**据此直接开工写通道 |
| 同上 | L64 一期明确不做一切写操作；L91 二期不做切模型/切模式与会话管理；L114 三期不做绕过各 harness 自身权限体系 | §4 矩阵里「模型切换 / 会话管理 / 审批」等行即使协议可达，**期次闸门仍未放行**；矩阵只报可得性，不报授权 |
| 同上 | L169 / L196 **D5**：配对状态机照抄 `dsh-remote-web-ui`（Apache-2.0）做 Rust 移植；L248 一次性配对 token + 设备 cookie（dsh 式） | 说明该社区插件的**配对模型**早已被列为兔维斯自身远程面板的参考，与本报告 §2 的拆解可直接复用 |
| `docs/superpowers/specs/2026-09-27-phase2-closure-app-injection-design.md` | L53（rc.2 时代：Electron 壳 + `dsh-desktop-host/lib/index.js` + `profiles/desktop`，host 监听 localhost 动态端口 **19387**，仓库 MIT 且 `packages/` 含 host/api/sdk/acp/webhook/jobs）；L186（ACP stdio + token→HMAC cookie 模型）；L187（桌面 host 的 HTTP API「401 鉴权」登记为备选、不主推）；L292/L388（19387 归属结案、桌面 cmdline 双特征 Mac 命中） | 本轮在同一判定式上复验 rc.3：判定式**仍成立**，端口改为 5214（见 §1.2）；并把当年「备选通道」从推断升级为**实测**（零插件跑通 RPC，§3.2） |
| `docs/superpowers/plans/2026-09-14-m1-dsh-monitoring-adapter.md` | L89/L679 `DSH_HOME`、L1644 默认端口 3080 | 本轮隔离实例正好落在 **3080**，与该档案一致 |
| `src-tauri/src/inject/routing.rs:94` | `"dsh" => Some(("blackbox", "dsh 写通道另评（web API/插件生态）"))`（`inject/resume.rs:102` 同类登记） | 现状确认：Tuvis 侧 dsh 写通道**仍是黑盒登记**，本报告不改动它 |
| `src-tauri/src/monitor/dsh/*`、`services/usage/collectors/dsh.rs`、`window/dsh_tab.rs:8` | 只读文件面；`dsh_tab.rs` 明写「lsof 只读查询，不触碰 `~/.dsh`、**不请求 dsh HTTP API**」；端口探测 `308[0-9]` 回落 3080 | 现状：**无任何 HTTP/WS 客户端**（C 路线）；`services/codex_desktop_ipc/transport/windows.rs` 是库内唯一的原生 IPC 先例（命名管道 + 服务端身份核验），可作为宿主身份核验的写法参照 |
| `docs/superpowers/specs/2026-09-27-…-app-injection-design.md` L90 | 桌面宿主判定单源核：**任一参数含 `dsh-desktop-host` 子串，或以 `\.dsh\profiles\desktop` / `/.dsh/profiles/desktop` 结尾**（`--expose-internals` 单独过弱，不作独立判据） | 本报告 §1.2 的「双特征」即此规则；本轮实测 rc.3 cmdline 两特征齐备，规则**继续适用** |
| 同上 L370 | 前轮登记：「`POST /api/session/prompt` 源码级存在（queue/steer + 冷会话自动 resume）；19387 全 401 实测、**POST 验证不可构造（无合法凭据，不做逆向）**」 | **本轮把它从「不可构造」推进为「已构造并跑通」**——在影子 `DSH_HOME` 上用官方 CLI 正常取得凭据，零插件完成 `POST /api/session/list → ok:true`（§3.2）；「不做逆向」这条边界仍然守住（§3.4 禁用手段清单） |
| `AGENTS.md` | 「采集域开发纪律」（改 `services/usage/**` 前先停 dev / 用影子 home）；「文档分工纪律」；「未经用户在场同意不得改 MASTER-PLAN」 | 本轮**未触碰采集代码与真实账本**；探针一律指向影子 `DSH_HOME`（§3.2）；未修改 MASTER-PLAN |
| `.agents/skills/win-console-inject-probe/SKILL.md` L81-87「边界与转介」 | 「本技能只管控制台会话（conhost/WT 宿主里的 TUI）。**GUI/APP 类工具不走控制台输入缓冲**……那是另一个工作流，勿用本流程硬套」 | DSH Desktop 属 Electron/协议通道 ⇒ **明确不执行**该技能的 conhost/WT 注入与屏读探测，也不得用模拟按键掩盖协议失败；本轮全程只用 HTTP/WS 与只读文件系统取证 |

**目录约定提示**：`docs/engineering/` 在本仓**原不存在**（既有证据档案集中在 `docs/superpowers/reviews/`，判定标签用 `STATIC / WIN-LIVE / MAC-RELAY / FIXTURE / DERIVED`）。本报告按任务指定路径新建该目录，标签体系改用本任务要求的四种（源码确认/实测通过/推断/未验证）；若主 Agent 要并入既有证据台账，需要做一次标签映射（源码确认↔STATIC、实测通过↔WIN-LIVE）。

### 1.1 安装包与宿主版本

| 项 | 实测值 | 取证方式 |
| --- | --- | --- |
| 操作系统 | Windows 10.0.26200 x64 | `[Environment]::OSVersion` |
| 安装路径 | `D:\Program Files\Deepseek-Harness\DeepSeek Harness.exe`（**非系统盘**） | `Win32_Process.ExecutablePath` |
| FileVersion | **0.2.1-rc.3** | `VersionInfo` |
| ProductVersion | 0.2.1.0 | 同上 |
| CompanyName | `GitHub, Inc.`（electron-builder 默认值，未改成 DeepSeek） | 同上 |
| 更新源 | `provider: generic`，`url: https://download.deepseek.com/dsh-desk/feeds/win-x64/`，**`channel: nightly`**，`publisherName: CN=Hangzhou DeepSeek Artificial Intelligence Co., Ltd.` | `resources/app-update.yml` |
| 桌面更新器缓存 | `~\AppData\Local\dsh-plugin-desktop-updater\installer.exe`，132,417,238 B（2026-08-25） | 目录枚举 |
| Electron 自定义 scheme | `dsh-app`（standard/secure/cors/fetch/streaming/code-cache 全部注册） | renderer 进程 cmdline |
| Chromium 用户数据目录 | `~\AppData\Roaming\@deepseek-ai/dsh-desktop`（**注意正斜杠混排**） | gpu/renderer cmdline |
| 内嵌运行时 | `resources/runtime/`，**node 24.21.0 / pnpm 11.28.5**（`versions.json`），目录 270 MB；`runtime/bin/node(.cmd)`、`runtime/cli/bin/dsh.cmd`(243 B) | 文件读取 |
| app.asar | 182,434,783 B，**18,188 个条目**，其中 `@deepseek-ai/**` 7,074 条、**307 个官方包** | 自研只读 asar 解析器 `probes/asar_probe.py` |

**与 Mac 的对照结论**：Windows 安装包版本串与 Mac 前轮所依据的 **0.2.1-rc.3 相同**，且 Mac 侧发现的机制在 Windows 包内**逐项存在**（见 §3），不是假定，是逐条读到的。

### 1.2 宿主进程身份与端口

```
主进程 PID 44172  "D:\Program Files\Deepseek-Harness\DeepSeek Harness.exe"      22:23:55
 ├─ 38348 gpu-process
 ├─ 51192 utility network.mojom.NetworkService
 ├─ 33940 renderer (--standard-schemes=dsh-app)
 ├─ 45768 utility audio.mojom.AudioService
 └─ 37096 ★ Desktop Host
      "…DeepSeek Harness.exe" --expose-internals
        "D:\…\resources\app.asar\dsh\node_modules\@deepseek-ai\dsh-desktop-host\lib\index.js"
        "D:\…\resources\app.asar\dsh"              ← argv[2] runtimeDir（安装包内的 dsh 根）
        "C:\Users\bunny\.dsh\profiles\desktop"     ← argv[3] projectDir（profile 根）
        "D:\…\resources\runtime\primary-runtime"   ← argv[4]
        "D:\…\runtime\primary-runtime\dependencies\pnpm\bin\pnpm.mjs"  ← argv[5] 包管理器
        "D:\…\resources\runtime\bin"               ← argv[6] PATH 前置
```

- **唯二双特征判定**（cmdline 含 `dsh-desktop-host\lib\index.js` **且** 含 `\.dsh\profiles\desktop`）：与仓库 `docs/superpowers/specs/2026-09-27-phase2-closure-app-injection-design.md:53` 在 v0.2.0-rc.2 上记录的判定式**跨版本仍然成立**；同文件 L388 记录 Mac 侧同判定式命中。
- **监听**：对全部 6 个 DSH 进程过滤 `Get-NetTCPConnection -State Listen`，**只有 1 条**：`127.0.0.1:5214`，属主 PID 37096。⇒ 当前机器无歧义实例。
- **端口是设计成动态的**：`dsh-desktop-host/lib/index.js:224-241` 固定传 `args: ["--no-open","--host","127.0.0.1","--port","0"]` —— `--port 0` = 由内核挑选临时端口。**源码确认**。
- **端口历史漂移实测**：仓库档案 rc.2 时代记录 **19387**（同 spec L53/L292，已「双结案」），本轮 rc.3 为 **5214**，我另起的隔离实例为 **3080**（`dsh web` 的默认端口，与 `plans/2026-09-14-m1-dsh-monitoring-adapter.md:1644` 一致）。⇒ **不能按默认端口或进程名认定 Host**，必须「cmdline 双特征 + 端口属主 PID」双校验。**实测通过**。

### 1.3 profile 隔离 / DSH_HOME / 数据面

| 层 | 位置 | 归属 | 备注 |
| --- | --- | --- | --- |
| profile 根（配置+插件） | `~/.dsh/profiles/desktop`（Host argv[3]）、`profiles/web`、`profiles/acp`、`profiles/headless` | 各 profile **独占** `cordis.yml` / `cordis.patch.yml` / `package.json` / `pnpm-lock.yaml` / `node_modules` | Desktop 只读自己的 `profiles/desktop` |
| 会话与用量数据 | `~/.dsh/sessions/<转义项目名>/…`、`~/.dsh/storages/session_projcache*`、`~/.dsh/dsh-usage` | 全局共享（`~/.dsh` 根） | 与 `monitor/dsh/mod.rs:1-3` 的「双数据源」一致 |
| 凭据 | `~/.dsh/.credentials.yaml`（以及 profile 内 credential store） | 秘密，**本轮未读取内容** | 见 §3.5 禁用手段 |
| 插件安装日志 | `~/.dsh/profiles/desktop/.plugin-manager/logs/{github-connection-*,operation-*}` | 桌面 profile 独有 | 第三方插件走 GitHub 拉取的证据面 |

**bundle 归属实测**（读各 profile 的 `package.json → dsh.profile.bundles`）：

- `desktop`：`@deepseek-ai/dsh-base`、`@deepseek-ai/dsh-web-app`、`dsh-foxbell-pet`、`@hytime/dsh-thinking-effort`
- `web`：官方两件 + `dsh-provider-model-configurator`、`dsh-payload-capture`、`dsh-foxbell-pet`、`@linxin666/dsh-web-all`、`@linxin666/dsh-i18n`、`dsh-better-sidebar`
- `acp`：`dsh-base` + `dsh-acp-app`；`headless`：`dsh-base` + `dsh-headless`

⇒ **关键 Windows 事实**：社区远控插件 `@linxin666/dsh-remote-web-ui` **0.3.18 已安装在共享 store**（`~/.dsh/profiles/node_modules/@linxin666/dsh-remote-web-ui`），但**没有任何 profile 的 bundles 声明它**（对 `profiles/**/*.json|*.yaml` 全文检索 `dsh-remote-web-ui` 命中 0 行）。**即：Windows 上该插件当前未启用，Desktop Host 里不存在 `/remote` 数据通道。** 这直接决定了 §4 的「插件实测」一列全部为未测。

---

## 2. 社区插件职责拆解表（`@linxin666/dsh-remote-web-ui` 0.3.18）

逐项判定「**仅转发** 还是 **另实现**」。源码位置 = 安装目录相对路径（版本 0.3.18 全表统一）。

| 编号 | 能力 | 是否仅转发 | 源码位置（v0.3.18） | 三态判定（旧注释 / 接口存在 / 实际可调用） |
| --- | --- | --- | --- | --- |
| 1a | 用户授权（扫码配对、一次性令牌、可撤销设备会话） | **另实现**（官方无此面） | `src/pairing.ts`（令牌过期 :46、默认 30 天闲置过期 :59、FIFO 上限 :272、持久化 :304-316 `mode 0o600`+临时文件 rename）；HTTP 面 `routes.ts:249 /pair-app`、`:336 /pair-accept`、`:343 /pair-app.sw.js`；客户端 `client/pair-api.ts`（`/api/pair/{status,issue,accept,stop,revoke,heartbeat,lan-bind}`）、`client/RemoteEntry.tsx:131 /api/pair/events` | 旧注释=插件自身注释钉在 **0.1.2-alpha.2**（过期）；接口存在=是；实际可调用=**未测**（未启用） |
| 1b | **内部凭据兑换（launch token → 浏览器鉴权 cookie）** | **另实现，且是本插件的不可替代核心** | `src/inner-auth.ts:1-10`（原话：authority-bound cookie「with no loopback exemption on this cohort line, so a cookie minted for the device's own authority can never satisfy the inner check」→「the process therefore **redeems its own launch token**」）；:26 前缀 `dsh-auth-`；:33-60 `createInnerAuth(launchUrl)` 用 `redirect:'manual'` 抓 303 的 `set-cookie` | 旧注释=同上过期；接口存在=是；实际可调用=**未测** |
| 1c | Host/Origin 适配 | **另实现** | `src/loopback.ts`、`src/loopback-proxy.ts:44`（内层 401 ⇒ 作废缓存凭据）、`gate.ts:66-70 isLoopbackClient`、`:94` 环回直接放行；`client/remote-channel.ts:49,64 isLoopbackHostname`；`client/index.ts:263 connection?.isLoopback ?? true`、`:210` 配对链接 origin-agnostic、`:428` 每个网卡独立 base URL | 同上 |
| 2a | HTTP 转发 | **仅转发**（带闸门的镜像） | `src/remote-methods.ts:18 REMOTE_PREFIX='/remote'`；`remote-channel-rules.ts:37-56` 规则表（`apiPrefix '/api/'`、`pairPrefix`、`updatePrefix`、`settingsBridgePrefix`、`sidebarPrefix`、`gitPrefix`、`petPrefix`）；`remote-channel-boot.ts` 把规则内联进 index.html（解析期打补丁） | 同上 |
| 2b | **WebSocket Mux 转发** | **仅转发 + 路径改写** | `remote-methods.ts:30-32 REMOTE_API_PATHS.mux='/remote/api/remote.mux'`；`:38-42 REMOTE_UPGRADE_PATHS`（mux + `/sidebar/ws/terminal` + `/sidebar/ws/agent-terminals` + `/api/dsh-ssh/terminal`，注释说明「SDK 按精确路径匹配 upgrade，不是前缀」）；无 cookie 时用 `x-dsh-remote-device` 头 / WS 用 `device` 查询串（`:56-58`） | **接口存在=实测通过**（官方 mux 在 Windows 存在，见 §3.1 行 4）；可调用=未测 |
| 2c | **文件上传路径改写** | **不存在——插件里没有任何上传/分段/receipt 代码** | 对 `src/` 全文检索 `upload|multipart|receipt`：**命中 0 行**。上传只是随 `apiPrefix '/api/'` 通用代理透传 | 判定：**仅转发**，非「另实现」 |
| 3 | 参数/响应/事件/错误转换 | **仅转发** | 插件内检索 `approval|answer|interactions` 的**功能性**命中为 0（仅注释散文命中，如 `firewall.ts:123,142,206`、`routes.ts:231`）；未自行解码或重写 Typert 载荷 | 判定：**审批/问答不是插件实现的**，属官方 RPC（`dsh-user-approval`、`dsh-user-questions` 官方包，见 §3） |
| 4 | 额外会话状态 / 审批缓存 / 重连恢复 / 幂等 / 持久化 | **另实现，但只限设备会话** | `pairing.ts:162` 设备表 JSON 持久化路径；`:201-203` 通知去重、`:222` lastSeenAt 脏标记 + sweep 节流落盘；`:239-241` 恢复上次进程的设备会话，注释原话「**persistence is an availability convenience, not a security boundary**」；`:266 evictIdle`、`:272` FIFO 截断 | **无审批缓存**（检索 approval 命中 0）。重连恢复=设备会话级，不含 RPC 游标 |
| 5 | 官方网页专用 Hook（独立客户端必需机制） | **插件承担了大量「仅浏览器成立」的胶水** | `package.json → dsh.client.platform: "web"`、`dsh.client.inject: [@deepseek-ai/dsh-client-connection, dsh-client-ui-settings, dsh-client-ui-sidebar]`、`dsh.bundle.patch: ./cordis.patch.yml`（patch 里 `insert` 一行 `- id: remote-web-ui`，同时挂 host 半与 browser 半）；`client/RemoteEntry.tsx`、`client/mobile-adapt.ts`、`client/settings-form.ts`、`client/deep-link.ts:4-6`（token → 接受 → 重载整页以带设备 cookie 启动 SPA）；`src/client/telemetry.ts` | 对**独立非浏览器客户端**：这些 Hook **全部不需要**——它只为「在浏览器里跑官方 WebUI」服务。见 §3.4 |
| 6 | 本机专用 / 禁止远程的接口 | **另实现（负向白名单）** | `remote-methods.ts:66-77 LOCAL_ONLY_PREFIXES = ['/api/pair','/api/update','/api/plugin-manager']`；`:10-16` 访问模型原话：「the host /api surface **has no per-method privilege pin** … 配对设备是 **full-control credential by design**，只有配对控制、自更新、插件装卸、主机电源这些控制面留在本机」；`src/update-api.ts:31,46` `/api/update/{status,run}`、`remote-methods.ts:46 /api/plugin-manager` | 判定：**风险申报关键项** —— 一旦按此模型配对，远端设备得到的是全权凭据 |
| — | 其它环境面 | 另实现 | `firewall.ts`（Windows 防火墙规则）、`lan-bind.ts`/`lan-bind-plan.ts`、`tunnel.ts`/`tunnel-plan.ts`（Cloudflare 隧道）、`dsh-home.ts`、`mount-once.ts`、`gate.ts`、`posture.ts:97 '/api/session.list'`、`update.ts`（家庭一键自更新）、`uuid-polyfill.ts` | — |

**版本漂移实证（回答「不要沿用旧注释」）**：插件注释把协议钉在 **0.1.2-alpha.2**（`remote-methods.ts:10,30`），而本机官方包是 **0.2.1-rc.3**；但注释所称「客户端只开一条常驻流套接字 = `/api/remote.mux`」在 0.2.1-rc.3 上**成立**（官方 `dsh-api-gateway/lib/index.js:133` 与 `client.node.js:7` 均写死该路径，实测 401 命中，见 §3）。⇒ 旧注释**描述已过期、结论仍对齐**，属于「注释要重写、路由要保留」的一类。

---

## 3. 官方接口核对与授权／直连步骤

### 3.1 Windows 安装包内官方接口核对（不假定与 Mac 相同）

`app.asar` 内 307 个官方包中与远控直接相关的 14 个：`dsh-api-remote-access`、`dsh-api-remotes`、`dsh-attachment`、`dsh-attachment-local`、`dsh-client-ui-approval`、`dsh-client-ui-attachment`、`dsh-client-ui-input-trigger`、`dsh-client-ui-user-questions`、`dsh-desktop-host`、`dsh-experimental-client-ui-voice-input`、`dsh-experimental-voice-input-bundle`、`dsh-host-webserver`、`dsh-user-approval`、`dsh-user-questions`。

| # | Mac 侧发现的机制 | Windows 是否存在 | 证据（文件:行） | 本轮是否真的调起来 |
| --- | --- | --- | --- | --- |
| 1 | `/connect` 路由 | **存在** | `dsh-client-connection/lib/index.js:19 const CONNECT_PATH="/connect"`；`:499-530 authorizeConnect`（JSDoc 原话「Exchange the launch token for an **independent API operator**; omitted interactions default to none」） | **实测通过**：对 Desktop 5214 `GET /connect?interactions=bogus` → **400** `interactions must be declared once as answer or none`（校验发生在鉴权之前 ⇒ 路由真的在执行）；`GET /connect`/错 token → **401**；对隔离实例带合法 token → **302 + 设备 cookie** |
| 2 | 独立 API Peer（`interactions` = `answer`/`none`） | **存在** | 同上 :507-529（`declarations.length>1` 或非 `answer/none` → 400；cookie `version:2` 携带 `id: publish(interactions)`）；能力闸在 `dsh-api-gateway/lib/index.js:1454`（`forwarded Remote events require interactions=answer`）、`client.node.js:1446,1452,1474,1497`（`$stream` 与转发事件都要求 `answer`，否则 `gateway/interactions-forbidden`，见 `index.js:969`） **实测通过**（隔离实例 `interactions=answer` 开出 API Peer；cookie `name=value` 283 字符 ⇒ 值 ≈231 字符，与源码编码 `v2.<b64u(json)>.<b64u(hmac-32B)>` 自洽；对照浏览器 cookie 171 字符 = v1 无 id）

    **答题闸已实测**：`interactions=none` 的 Peer 调 `POST /api/$events/result` → `gateway/interactions-forbidden: forwarded Remote events require interactions=answer`；`interactions=answer` 的 Peer 同一调用越过该闸、撞到 `api gateway: invalid Remote event result`（载荷形状）⇒ **`answer` 是答题的必要非充分条件**（还要事件归属与 owner 校验，见 `index.js:965-969`）。见 `logs/interactions_gate.txt` |
| 3 | 认证 HTTP（launch token → authority 绑定签名 cookie） | **存在** | `:294-300 processLaunchToken`（`encodeBase64Url(randomBytes(SECRET_BYTES))`，`SECRET_BYTES=32` :275；存 `WeakMap PROCESS_LAUNCH_TOKENS`，**不落盘**）；`:352 createHmac('sha256',secret)`；`:335-337 cookieName = 'dsh-auth-'+b64u(sha256(authority))`；`:306-312 requestAudience`；`:440-444 authenticatedUrl`；`:458-497 authorizeIndex`；`:330-334 tokenMatches`（`timingSafeEqual`） | **实测通过**（隔离实例 303 + `dsh-auth-<43位>=171字节`；authority 绑定与命名与源码完全一致） |
| 4 | Mux 事件流 `/api/remote.mux` | **存在** | `dsh-api-gateway/lib/index.js:132-133`（注释「Exact WebSocket route carrying every Typert Remote stream」）、`:820 webServer.registerUpgrade(route)`、`:777 RemoteStreamMuxServer` | **接口存在 + 传输层实测通过**：无凭据 `GET /api/remote.mux` → 401 `unauthorized`；裸 WS Upgrade → 401 + `Connection: close`（`logs/http_unauth_matrix.json`）；**带 API-Peer cookie 的 WS Upgrade → `HTTP/1.1 101 Switching Protocols`，`Sec-WebSocket-Accept` 校验通过**，且服务端**处理了 mux 帧**：`{type:open,streamId,endpoint,payload}` 送 `no/such` → 回 `{"type":"error","streamId":"s-bogus","error":{"code":"gateway/invocation-unavailable","message":"typert gateway: no/such: no active Remote method exports this endpoint"}}`（`logs/mux_ws_probe.txt`，帧键与 `client.node.js:225` / `index.js:244-250` 完全一致）。**真实会话流内容仍未测**（需运行中的会话） |
| 5 | 附件上传 / 文件 | **存在** | 官方包 `dsh-attachment`、`dsh-attachment-local`、`dsh-api-workspace-files`、`dsh-client-file-upload`；`dsh-client-connection/README.md`「Use this package」段落把 **multipart 结果附件**写成明文契约（`metadata` part + 附件表：路径/编解码/part id，客户端还原 `ArrayBuffer`；并明确「**Binary parameters, events, and streamed binary results are unsupported**」） | **未测**（隔离实例无凭据/模型，且需真实会话） |
| 6 | 模型 / 供应商 / 推理等级切换 | **存在** | `dsh-api-settings-controller` + `dsh-api-remotes/README.md` 挂载清单（settings、credentials、permissionPresets、agent preset 等命名空间）；本机 `profiles/desktop/cordis.patch.yml` 里可见多供应商 `baseURL` 行（:59 :121 :140 :156，**本轮未读取密钥**） | **未测**（切换会改真实默认设置，属禁改项；见 §7） |
| 7 | 会话管理（新建/恢复/重命名/分支/搜索/归档/置顶） | **存在** | `dsh-api-session-controller`（43 文件）、`dsh-api-remotes` 门面挂载 Session Controller；Typert 端点形如 `ns/method` 两段 —— `dsh-api-gateway/lib/index.js:876-882 claimsEndpoint` | **实测通过（部分）**：`session/list` 真实返回 `{ok:true,value:{items:[]}}`；`session/search` 走到业务校验（`session search query must not be empty`）⇒ **处理器真在跑**。其余动作 **未测**（缺参数形状/会话） |
| 8 | Skill / 命令 | **存在** | `commands/list` 端点在 `dsh-api-remotes` 挂载清单中；descriptor 要求 `agentId`（实测报 `missing "agentId"`） | **实测通过（仅到 descriptor 校验）**；真列表未取（隔离实例无 agent） |
| 9 | 原生审批 | **存在（官方侧）** | `dsh-user-approval`、`dsh-client-ui-approval` 官方包 | **未测** |
| 10 | 问答题型 | **存在（官方侧）** | `dsh-user-questions`、`dsh-client-ui-user-questions` | **未测** |
| 11 | 定向打断 / 队列编辑撤回 | **存在（官方侧，包面）** | `dsh-api-remotes/README.md` 提及 stream 与 cancellation（Gateway Client face 拥有 "streams, and cancellation"） | **未测** |
| 12 | **Peer 主体与非属主访问控制**（Mac 侧未特别点名，Windows 实为关键） | **存在** | `dsh-api-remote-access/README.md`：「Method-level Remote access control for **non-owner Peers**: role-based policy…」；`decide({principal,action,resources,args})` → `allow`/`deny(reason)`，Gateway 抛 `remote-access/denied`，被拒事件直接丢弃；「The owner (`{kind:'owner'}`) is **never checked**; `remoteAccess/follow` is always allowed」；`invalidate(principal?)` 在每次 grant/revocation/mode change/expiry 调用 | **源码确认**；策略实际生效 **未测** |

> **一处需要纠正的说法**：`dsh-authorization` 包**不是**宿主接入授权。它自己的 README 写明是「obtain credentials that configuration cannot supply, because getting one means a conversation with a human」（OAuth 式登录/一次性码/账号选择），「A headless or ACP composition can mount this package safely: it offers no flows of its own」。⇒ 远控接入的授权面在 `dsh-client-connection`（浏览器鉴权）+ `dsh-api-remote-access`（Peer 策略），选路讨论时不要混用这两个「authorization」。

---


### 3.2 已跑通的完整链路（隔离官方实例，零插件）

前置：`DSH_HOME` 指到本轮 run 目录下的隔离 home，用**安装包自带的 CLI** `resources/runtime/cli/bin/dsh.cmd`（内容：`ELECTRON_RUN_AS_NODE=1` + `DeepSeek Harness.exe --expose-internals …\dsh-desktop-host\lib\cli.js %*`），启动 `--profile tuvisprobe --from-default-profile web --no-open`。CLI **打印出带 token 的授权 URL**（`logs/boot2.log`，token 已脱敏为 `<REDACTED>`，只留长度 43 字符与 sha256 前 8 位 `17035d96`）：`dsh web: http://127.0.0.1:3080/?token=…`。**这一步就是「用户授权取得连接资料的可用入口」——官方自带，无需插件。**

随后逐步（`logs/independent_connect_exchange.json`、`logs/independent_rpc_probe.json`、`logs/independent_rpc_ok.json`）：

| 步 | 请求 | 响应 | 判定 |
| --- | --- | --- | --- |
| 1 | `GET /?token=<launch>` | **303**，`Set-Cookie: dsh-auth-VPhE…1sw=<171 B>` | 实测通过 |
| 2 | `GET /connect?token=<launch>&interactions=answer` | **302**，同名 cookie 换成 **321 B**（version 2 载荷携带 `id: publish(interactions)`） | 实测通过 = **独立 API Peer 在 Windows 可开出** |
| 2b | 两条重定向的**目标** | 源码：`authorizeIndex` 写 `location: "./"`（`:473-478`），`authorizeConnect` 写 `location: "./api/remote.mux"`（`:532-536`）。**本轮探针未捕获 Location 值**（证据文件里为 `null`），因此 Location 只记 **源码确认 / 未实测**，不冒充观测 | 源码确认 |
| 3 | `POST /api/session/list` `{}` | 200 `gateway/bad-request: invalid client-request message … path:["method"]` | 协议考古：信封缺 `method` |
| 4 | `POST /api/session/list` `{type,rpcId,payload:{}}` | 200 `gateway/internal: Remote payload must contain exactly one plain-object args field` | 缺 `args` |
| 5 | `payload:{args:{}}` | 200 `gateway/arguments-invalid: session/list: args fields do not match the descriptor: missing "_request"` | 缺 descriptor 字段 |
| 6 | `payload:{args:{"_request":{}}}` | **200 `{type:"server-response",rpcId:"ok",result:{ok:true,value:{items:[]}}}`** | **实测通过：纯官方接口、零插件、非浏览器环境完成一次真实 RPC** |
| 7 | `commands/list` | `missing "agentId"` | 端点被认领（`claimsEndpoint` 命中），参数形状已知 |
| 8 | `session/search {"request":{"query":""}}` | `session search query must not be empty` | **业务处理器真实执行** |

信封形状（`dsh-client-connection/lib/index.js:609-646`）：请求 `{type:"client-request", rpcId:string, method:string, payload:unknown}`；响应 `{type:"server-response", rpcId, result:{ok,…}}`；URL 前缀 `/api`（`API_PATH="/api"` :14）+ 路径即 `ns/method`（`channel !== "/api"` 会抛 `connection: invalid shared RPC channel` :874）。

### 3.3 「同一个 Desktop Host」的身份证据（本机真实实例，5214）

| 证据面 | 值 |
| --- | --- |
| 属主进程 | PID **37096**，父 PID 44172，创建 22:23:55，cmdline 双特征命中 |
| 唯一监听 | `127.0.0.1:5214`（6 个 DSH 进程里唯一 Listen 条目） |
| 401 文本 | `/`、`/connect` → `dsh web authentication required; reopen the URL printed by dsh web.` —— 该串**只**存在于 `dsh-client-connection/lib/index.js`（检索命中该文件） ⇒ 与 §3.2 的隔离实例**同一实现**，可断定 5214 就是官方 Desktop Host 的 web 层 |
| `/api`、`/api/health` | 401 `unauthorized`（另一realm，Gateway 的 `/api` 通道） |
| 未注册路径 | `404`（`/health`、`/version`、`/remote`、`/mux`、`/ws`、随机路径） ⇒ 401/404 之别可用于**区分「路由已注册但需鉴权」与「路由不存在」**，本轮据此断言 `/connect`、`/api` 存在 |
| `/remote` 前缀 | **404 ⇒ 实测确认 Desktop 实例上不存在 `/remote` 数据通道**，与 §1.3「插件已安装但未启用」互相印证 |

### 3.4 Desktop 实例上的授权缺口（本轮最重要的负面结论）

`dsh-desktop-host/lib/index.js:342-349`：

```js
/** Launch the Desktop profile through the Web application and report its URL to Electron. */  // :218
const url = ctx.connection.authenticatedUrl(`${ctx.webServer.protocol}//127.0.0.1:${String(ctx.webServer.port)}`);
if (process.connected) process.send?.({ type: "ready", url, injections: ctx.webServer.collectIndexInjections() }, …);
```

即：**Desktop 的 launch token 只经子进程 IPC 交给父 Electron 进程**，全仓（本机 307 包内）`authenticatedUrl` 的调用点只有两处（`:440` 定义、`dsh-desktop-host:342` 使用）。而 token 本身：

- `randomBytes(32)` 每次新造，存 `WeakMap`，**不写盘**（`:294-300`）；
- 我在 `~/.dsh`（含 `profiles/desktop` 全部文件）与 Chromium userData 下检索 `*port*`/`*token*`/`*host*json*`/`*web*json*`/`*.lock`，**没有任何宿主端口/令牌发布文件**（命中的只有无关的 `foxbell-pet/.import-staging`、`resolve.exports`、`task-board/ledger-v2.lock`）。

⇒ **文案面补强（源码确认）**：对安装包内 972 个 `*/locale/*.json` + `dsh-desktop-host` + `dsh-client-ui-brand-official` 文件做了检索，**「配对 / 二维码 / 分享 / remote-access / qrcode / qr-code / pairing / share-link」类用户可见文案命中 0 处**；「远程」仅 3 处且全部落在包元数据 `meta.title/description`（「远程调用网关」「Typert 类型注册表」等开发者文案）。对照组「设置」命中 15 处，证明 CJK 检索机制有效（`logs/ui_entry_search.md`）。⇒ 官方桌面壳**没有**面向用户的远控/配对入口文案面。

**源码确认 + 实测支持**：Windows 上「纯无插件、跨进程、经正常授权」地拿到 **Desktop 自己**的接入资料，**当前不存在可用入口**（`dsh web` 会另起一个进程、另一个 profile、另一个 token，因此**不是** Desktop 的同一宿主）。CLI 打印 URL 这条正常授权入口，只在**由 CLI 自己承载的宿主**上成立。
⇒ 若要走「直连官方 RPC 操控 Desktop 正在跑的会话」，必须让 Desktop 侧把 token 交出来：官方桌面 UI 是否有「复制访问地址/远程分享」入口 —— **未验证**（见 §7 待人工补验 ①）。

**本轮明确没有采用的手段**（避免歧义，逐条申报）：未提取签名密钥伪造 cookie；未读取 Chromium cookie jar / `Network/Cookies`；未附加调试器读父进程内存；未关闭任何认证开关来「跑通」；未修改 Desktop 的 profile、配置、凭据、防火墙或隧道。失败时先留证据（`logs/http_unauth_matrix.json` 即 401 原样）。

**插件不可替代性判定**（回答「若授权资料只能由插件提供，明确记录」）：授权资料本身由**官方** `ctx.connection` 服务在宿主进程内提供；插件 `inner-auth.ts` 只是**在宿主进程内读取该服务的 `authenticatedUrl()` 并代为兑换 cookie**（`:33-40`）。⇒ **不是只能由插件提供**，但「在宿主进程内读取」这一动作目前只有插件（或 Electron 父进程）做得到 —— 这正是路线 B 的立足点。

### 3.5 官方客户端组件在非浏览器环境的复用性

- **源码确认**：`dsh-client-connection/README.md`「Use this package」明写 —— `./client` 导入在 Node 下选中独立的 `lib/client.node.js` ESM 包，在浏览器下选中 `client.browser.js` 装载工厂；「Import the Client entry for a **non-page** Cordis tree」。`dsh-api-gateway/lib/client.node.js` 同样存在（含 `RemoteStreamMuxClient`，:7/:1446）。⇒ **官方自带 Node 侧客户端，不需要浏览器运行时**。
- 依赖形状：该 Node 客户端是 Cordis 图的一部分（`ctx.remote.$mount()`、`ctx.inject(["connection","webServer"],…)`），复用要引入 **Cordis + typert + zod + cosmokit** 等官方包（本机 307 包即完整闭包）；`README` 还要求宿主侧静态页提供 `__DSH_TRANSPORT__.streamBaseUrl`（"The desktop carrier owns authentication; setting the origin alone grants no access."）。
- **Windows 安装版分发成本实测**：`resources/runtime` **270 MB**（node 24.21.0 + pnpm 11.28.5 + python），`app.asar` **174 MB**。任何「复用官方 Node 客户端」的方案在 Windows 上都要么依赖用户已装的 Desktop 运行时（版本随 nightly 漂移），要么自带一份 node（≈270 MB 量级）。**推断**：若只做 §3.2 已证实的手工 HTTP 信封（`fetch` + `ws` + JSON 信封），可完全避开官方客户端包，代价是自付协议漂移风险 —— 见 §6。

---

## 4. 功能矩阵（官方接口 / 插件路径 / 独立 RPC）

列义：**插件源码支持** = 插件里是否有该能力面；**插件实测** = 启用插件后浏览器控制 Desktop 同会话（本轮 Windows **未测**，原因见每行）；**独立 RPC 实测** = 零插件、跨进程、经正常授权（本轮在**隔离官方实例**上做，故与 Desktop 实例的差别单列）。

| 功能 | 官方接口存在 | 插件源码支持 | 插件实测（Win） | 独立 RPC 实测（Win，隔离实例） | 未测原因 |
| --- | --- | --- | --- | --- | --- |
| 文本发送 | **源码确认**（session-controller + `dsh-api-remotes`） | 仅转发（`/api` 代理） | 未测 | **未测** | 插件未启用；隔离实例无模型凭据（不复制真实密钥） |
| 流式回复与状态 | **源码确认**（`/api/remote.mux` + `RemoteStreamMuxServer`） | 仅转发 + 精确 upgrade 路径改写 | 未测 | **实测通过（传输层）**：101 握手 + accept 校验 + 帧往返 + 结构化错误回帧（零插件、非浏览器） | 真实会话/工作区流的内容面未测（需运行中的会话，属 §7 ②） |
| 文件/图片上传 + receipt + 消息绑定 + 桌面可见性 | **源码确认**（`dsh-attachment*`、`dsh-client-file-upload`；multipart 结果附件契约写在 `dsh-client-connection/README.md`） | **插件无上传代码**（检索 0 命中，仅透传） | 未测 | **未测** | 需真实会话 + 模型；「桌面可见性」需 Desktop 同宿主，本轮拿不到 Desktop token |
| 模型/供应商/推理等级切换 + 下一次真实请求 + 默认设置副作用 | **源码确认**（`dsh-api-settings-controller`、`permissionPresets`「complete process-level catalog」） | 仅转发 | 未测 | **未测** | **禁改项**：会改真实默认设置（brief §6）；隔离实例无凭据 |
| 原生审批 允许一次 / 拒绝 | **源码确认**（`dsh-user-approval`、`dsh-client-ui-approval`）；非 answer Peer 不能答：**实测通过**（`interactions-forbidden`）（`gateway/interactions-forbidden` `index.js:1454,969`；`$events/result` 仅 owner Peer 可答 `:965-966`） | 插件不实现 | 未测 | **未测** | 需产生真实挂起审批 + 带凭据通道 |
| 实际可触发的问答题型 | **源码确认**（`dsh-user-questions` 等两包存在） | 插件不实现 | 未测 | **未测** | 同上 |
| 定向打断 / 队列编辑撤回 | **源码确认**（remotes README：streams + cancellation 归 Gateway Client face） | 仅转发 | 未测 | **未测** | 需运行中的 turn |
| 新建/恢复/搜索 | **源码确认** | 仅转发 | 未测 | **实测通过**：`session/list → {ok:true,value:{items:[]}}`、`session/search` 走到业务校验 | — |
| 重命名/分支/归档/恢复/置顶 | **源码确认**（session-controller 43 文件） | 仅转发 | 未测 | **未测** | 端点名与 descriptor 未逐一探到（§7 待人工补验 ③） |
| Skill/命令 | **源码确认** | 仅转发 | 未测 | **实测通过（到 descriptor）**：`commands/list` 报 `missing "agentId"` | 隔离实例无 agent |
| 设置/凭据/插件装卸接口盘点 | **源码确认**：`dsh-api-remotes` 挂载 Office 转换、Commands、credentials、settings、Goal、Schedule、dynamic Cordis、file 与 Session 引用、**只读宿主插件清单**、插件安装注册表响应比对、消息反馈、permissionPresets、Session Controller、subagents、Workspace Controller | 插件**本机专用**清单 `LOCAL_ONLY_PREFIXES=['/api/pair','/api/update','/api/plugin-manager']`（`remote-methods.ts:70-74`） | 未测 | **未测** | **不应直接开放**：credentials 面、plugin 装卸面、宿主电源/自更新面、dynamic Cordis（可注入运行代码）；`dsh-api-remote-access` 的意义正是给非 owner Peer 上方法级闸，但插件模型自述「配对设备 = 全权凭据」——**两处安全姿态不一致，必须在设计期裁决** |

## 5. Windows 特有差异与失败恢复案例（必须覆盖项）

| 场景 | Windows 结论 | 证据 |
| --- | --- | --- |
| 挂起审批后才连接 | **未测**（无法在 Desktop 侧产生挂起审批；隔离侧无凭据链） | — |
| 两端抢答 | **源码确认（部分）**：非 `answer` Peer 答题被拒 `gateway/interactions-forbidden`；`$events/result` 仅 owner Peer 可应答 ⇒ 「抢答」在官方侧有 Peer 归属校验 `index.js:969`（`client.peer !== peer` → 「Remote event result belongs to another Peer」） | 实测未做 |
| 重复请求 | **未测**；`rpcId` 为自由字符串（`rpcIdSchema = z.string()` `:609`），源码未见幂等表 ⇒ **推断**幂等由方法自身负责，不保证 | — |
| 连接中断 | **源码确认**：`dsh-client-connection/README.md` 声明「event-stream delivery with **reconnect**」、`ConnectionController` 按 retry policy、generation ready/failure/withdraw 语义；`client.node.js:176` `Remote stream reconnect requested` | 未实测 |
| Desktop 重启后端口/凭据变化 | **实测通过 + 源码确认**：端口 `--port 0` 动态（`dsh-desktop-host:224-241`；档案 19387 → 本轮 5214 → 隔离实例 3080）；token 每次 `randomBytes(32)` 新造（`:294-300`）⇒ **每次重启端口与凭据都必变**；cookie 是 HMAC 签名且 `authority` 绑定（`:306-312`、`:335-337`），跨端口不通用 | — |
| Windows 代理 | 本轮进程环境**无** `HTTP(S)_PROXY/NO_PROXY`（实测 env 命中 0 键）。⇒ 无代理相关结论可给；若用户环境设系统代理，`dsh.cmd` 的 CLI 路径未见代理绕过逻辑（**未验证**） | `os.environ` 过滤 |
| 路径编码 / 中文文件名 | **实测通过**：`~/.dsh/sessions` 下项目目录名为「转义名」，例 `--C-Users-bunny-Desktop-~4E0A~6D77~4EEA~7535~FF08~96C6~56E2~FF09~6709~9650~516C~53F8--` = `C:\Users\bunny\Desktop\上海人集宝有限公司…` 的 **UTF-16 码元十六进制转义**；另有 `--C-Users-bunny-AppData-Local-Temp-dsh-probe-t4--`（前轮探针残留）。⇒ Windows 特有：宿主把盘符/反斜杠/非 ASCII 一律转义进目录名，**任何远控侧要按项目名对齐会话时必须复用同一转义，或改用 `storages/session_projcache`**（与 `monitor/dsh/projcache.rs` 现策略一致） | 目录枚举 |
| 安装位置差异 | **实测通过**：本机在 `D:\Program Files\Deepseek-Harness`（非系统盘、带空格），且 `--user-data-dir` 用 `@deepseek-ai/dsh-desktop`（含斜杠的命名空间式目录名）。⇒ 探针/客户端**不得硬编码 `C:\Program Files`**；也不得硬编码 5214/3080 | cmdline + 版本信息 |
| **Git-Bash 参数改写导致的假阴性（本轮真实事故，已复现并纠正）** | 探针 `asar_probe.py` 的 `list`/`grep` 子命令以命令行接收检索子串。在 Git Bash 下，**以 `/` 开头的实参会被 MSYS 路径转换重写**（`/locale` → `C:/Program Files/Git/locale`；反过来设 `MSYS_NO_PATHCONV=1` 又会让 `/d/Program Files/…` 这类 asar 路径失配并 `FileNotFoundError`）。首轮「0 命中」正是这个原因，**当时差点被写成「官方无远控入口」的结论**。识别方式：**任何负面结论必须带一个已知能命中的对照组**（本轮用 `dsh web authentication required` 与「设置」两处对照）。纠正方式：改为在 Python 进程内以 Windows 反斜杠路径直接调用模块函数（`sys.path.insert` + `A.do_grep`），不经 shell 实参；结果与首轮不同且可信（972 文件、对照命中 15/1、目标项 0）。⇒ 见 `logs/ui_entry_search.md` | **实测通过（失败—识别—纠正全过程）** |
| 反复安装版本以填表 | **未做**（遵守 brief §5）；版本隔离验证仅有「同一安装包 0.2.1-rc.3」单点，跨版本矩阵 **未测**（无法在同一 DSH_HOME 隔离另一版本） | — |

---

## 6. 路线比较

| 维度 | A. 直连官方 RPC（零插件） | B. 官方 RPC + 最小授权插件 | C. 业务桥接（读盘 + ACP/headless） |
| --- | --- | --- | --- |
| 能否操控 **Desktop 正在跑的同一会话** | **不成立（本轮）**：Desktop 的 launch token 只经子进程 IPC 给 Electron 父进程，`authenticatedUrl` 全闭包仅 2 个调用点；无落盘发布点（§3.4）。除非官方桌面 UI 暴露分享入口或宿主被要求把 URL 交出来 | **成立**：插件在宿主进程内直接读 `ctx.connection`，`inner-auth.ts` 已实证「代为兑换 cookie」；只需一个**不含 WebUI** 的最小授权插件（把 Peer 信息经一次本地握手交给 Tuvis） | **不成立**：ACP/headless 另起进程（`profiles/acp` / `profiles/headless` bundle 各自独立），是**同一数据面、不同活会话** |
| 运行依赖 | 若复用官方 Node 客户端：Cordis + typert + zod 等（`dsh-client-connection/README.md` 明示 `./client` 走 Node bundle）→ 需 node；若只用本轮手工信封：任意语言 `fetch` + `ws`（**实测通过**，0 依赖） | Tuvis 侧同 A 的轻量客户端；宿主侧多一个 profile 插件（`cordis.patch.yml` insert 一行）；插件需 pnpm（安装包自带 11.28.5） | 已在今日主线实现（`monitor/dsh/*`、`services/usage/collectors/dsh.rs`，纯只读文件；写通道按仓库裁决走 ACP stdio） |
| 安装/分发成本（Windows） | 最低：无宿主侧改动；但**受 §3.4 缺口阻断** | 中：需把插件分发 + `dsh plugin --profile desktop add …`（或 `link:`）；GitHub 源插件另需网络（`.plugin-manager/logs/github-connection-*` 为证据）；官方 node/runtime ≈270 MB 若不自带 | 最低（现状） |
| 升级维护 | **最脆**：`/api` 信封、`ns/method` 端点名、descriptor 字段（本轮亲眼看到 `_request`/`agentId`/`request`）都是**内部契约**；`channel: nightly` 自动更新 ⇒ 漂移窗口持续存在 | **最可控**：宿主侧的 token 兑换由官方 `ctx.connection` 承担（版本随官方），Tuvis 只依赖 `/api` 信封 + mux；但**第三方插件**注释已钉 0.1.2-alpha.2 而实包 0.2.1-rc.3 ⇒ 社区插件会漂，B 的最小插件必须自研自控 | **最稳**：只依赖落盘代际（`session.v4.jsonl.zstd`）与 projcache，已有版本门与回归用例（AGENTS.md 采集域纪律） |
| 配置迁移 | 无 | 用户 profile 的 `package.json`/`cordis.patch.yml` 多一行；卸载需回滚两文件 + `node_modules` | 无 |
| 清理 | 无残留 | 需移除插件行并重跑 pnpm；设备会话文件（若最小插件沿用配对持久化）需清 | 无 |
| 安全姿态 | 直连 = 拿到 token 即全权（`BrowserAuth` 无 per-method 权限钉），Tuvis 需**自带**方法白名单 | 同 A，但官方已有 `dsh-api-remote-access` 的 Peer 角色策略可借用（**源码确认**：owner 永不校验、非 owner 走 `decide()`）——B 可让宿主侧策略做硬闸，Tuvis 只申请必要 action | 只读为主，写侧走 ACP 既有鉴权 |

---

## 7. 推荐路线、成立前提、反证与未解决问题

**推荐：B（官方 RPC + 最小授权插件）为主动线，C（业务桥接）为只读与降级基线，A 仅作为「官方若开放桌面分享入口」的备选。**

1. **B 成立前提**（缺一不可）：
   - 官方 `ctx.connection.authenticatedUrl()` 在 0.2.1-rc.3 上继续存在（本轮**源码确认**，且调用点仅 2 处，被删的可能性要盯）；
   - `/api` 信封 `{type,rpcId,method,payload:{args:{…}}}` + `ns/method` 端点命名继续稳定（本轮**实测通过**）；
   - 用户同意在其 `desktop` profile 安装一个**由兔维斯自研、不含 WebUI、只做授权转交**的最小插件（当前社区插件**已安装但未启用**，Windows 侧无任何 `/remote` 通道，§1.3/§3.3）；
   - 宿主侧用 `dsh-api-remote-access` 注册非 owner Peer 的角色策略，把 Tuvis 收口到必要 action 白名单（拒绝 credentials/plugin 装卸/dynamic Cordis 面）；
   - Tuvis 的远控 Peer **必须以 `interactions=answer` 开 `/connect`** —— `answer` 已实测为答题/事件流的必要条件（默认值是 `none`，`index.js:508`），漏传即只能读不能答。
2. **A 成立前提**：Desktop 出现**正常授权入口**把带 token 的 URL 交给用户/第三方（当前只有子 IPC 发给 Electron 父进程）。**本轮未找到该入口**（未验证，非不存在）。
3. **反证（会推翻推荐的观察）**：
   - 若官方在后续 rc 版本给 `/api` 加 per-method 权限钉或改信封 → B/C 的写侧需重做（关注 `dsh-api-remote-access` 是否从「Peer 策略」扩到「owner 也校验」）。
   - 若 `--port 0` 改为固定端口或官方发布 token 落盘文件 → A 直接可成立，优先级应重排。
   - **答题资格闸已实测**：`interactions=none` 的 Peer 调 `$events/result` 被 `gateway/interactions-forbidden` 拒绝，`answer` 的 Peer 越过该闸后仍受「事件归属 + owner」校验（`index.js:965-969`）约束 ⇒ B 的「远端答题」**前提确认了一半**（`answer` 为必要非充分条件）；**owner 关未实测**（隔离实例本身即以 owner 角色运行，在此无法区分 owner 与 `answer` 非 owner Peer）。若官方最终要求 owner 才能答题，B 的远端答题仍会落空。
4. **待人工补验（需要用户点击/授权，本轮不能自取）**：
   - ① 文案面已**源码确认不存在**官方远控/配对入口（972 文件 0 命中，见 §3.4）；仍需一次**目视核对**兜住「无文案的图标入口」这种残余可能（用户扫一眼桌面设置页即可判定，决定 A 是否复活）。
   - ② 经用户同意后，在 `desktop` profile 临时启用社区 `@linxin666/dsh-remote-web-ui` 0.3.18 或自研最小插件，补齐：`answer` Peer 审批允许一次/拒绝、问答各题型、定向打断、队列编辑撤回、附件上传 + receipt + 桌面可见性、以及「挂起审批后才连接」「两端抢答」「重复请求」三场景。**注**：带凭据的 `/api/remote.mux` **传输层**已在隔离实例实测通过（101 握手 + `Sec-WebSocket-Accept` 校验 + 帧往返 + 结构化错误回帧），此处待补的是**真实会话流内容**与在 Desktop 同宿主上的复现。
   - ③ 逐一枚举 `session/*`、`workspace/*`、`settings/*`、`attachment/*` 的 descriptor 必填字段（本轮只撞到 `_request`/`agentId`/`request` 三个），形成端点表。
   - ④ 模型/供应商/推理等级切换及其「下一次真实请求」与默认设置副作用 —— 需在**影子 profile** 上做（不能改真实默认值）。
   - ⑤ 版本矩阵（rc.2 vs rc.3）：同一 `DSH_HOME` 下无法隔离另一版本 ⇒ **未测**。

---

## 8. 清理恢复结果

| 项 | 结果 |
| --- | --- |
| 隔离官方宿主 | 两轮各一进程：PID 33636、PID 41316（监听 3080）**均已 `Stop-Process` 终止**，复核 `3080: no listener`；用户 Desktop `5214` 复核 `desktop OK pid=37096` |
| 影子 profile | 两轮分别在影子 `DSH_HOME` 下生成 `profiles/tuvisprobe`、`profiles/tuvisprobe2`（含各自新生成的签名密钥文件），**随 `isolated/` 整目录删除**；真实 `~/.dsh` 未被任何写入路径触达 |
| 隔离 DSH_HOME | `…/dsh-native-rpc-win-20261028/isolated/dshhome`（含新生成的 `.credentials.yaml` 签名密钥、profiles/tuvisprobe、storages）**整目录删除**，实测 `removed: True` |
| 令牌脱敏 | `raw/cli/boot2.log` 与入库副本 `logs/boot2.log` 中 `token=` 一律替换为 `<REDACTED:43-char-base64url-launch-token>`；报告只保留长度、sha256 前 8 位与 cookie 字节长度；入库前对 `token=[A-Za-z0-9_-]{8,}` 全量扫描，剩余命中仅为本轮**故意构造的假 token** `deadbeef` |
| 用户 Desktop | **未被触碰**：PID 37096 仍监听 `127.0.0.1:5214`；`profiles/desktop/{cordis.yml, cordis.patch.yml, package.json}` mtime 分别为 22:23 / 10-24 23:45 / 10-05 00:05，**全部早于本轮（22:27 起）** |
| 用户真实配置 | 未安装/未启用任何插件；未改 profile bundles、cordis.patch、模型/供应商、防火墙、LAN 绑定、隧道；未改凭据；未升级主应用；未重启 Desktop/CLI |
| 网络入口 | 未开隧道、未做设备配对、未改系统代理（环境本无 `HTTP(S)_PROXY`） |
| 新增文件 | 仅：run 目录 `C:\Users\bunny\Documents\tuvis-verify-logs\dsh-native-rpc-win-20261028\{probes,raw,sanitized}`；仓库内 `docs/engineering/evidence/dsh-native-rpc/{windows-report.md,probes/,logs/}` |
| 探针可复跑 | `probes/host_identity.ps1`（进程树 + 监听 + 已连回显，只读）、`probes/asar_probe.py`（只读 asar list/extract/grep）——**Windows 注意**：CLI 子串实参会被 Git-Bash 的 MSYS 路径转换改写，跨平台复跑请（a）用 Windows 风格路径传 asar，且检索子串不要以 `/` 开头，或（b）直接在 Python 里 `import asar_probe` 调用 `read_header/walk/extract_file/do_grep`（本轮采用 b）。文件头已写明输出必须走 `sys.stdout.buffer`（GBK 控制台下 `print()` 会在扫描中途抛 `UnicodeEncodeError` 并丢失缓冲输出，这也是首轮 0 命中的第二个成因）；隔离宿主启动命令记录在 §3.2（需自行设 `DSH_HOME` 到影子目录，避免污染 `~/.dsh`） |
| 仓库他人改动 | 工作区进入本轮前已有 `AGENTS.md` 未提交修改（5 行删除），**非本轮所为**；本报告只在**新分支** `docs/dsh-native-rpc-windows-probe` 上新增文件，未 stage、未提交、未覆盖该改动 |

### 交付物运行依赖与截图说明

- **探针依赖**：`probes/asar_probe.py` = 纯标准库 Python 3（无第三方包，只读打开 asar，支持 `list|extract|grep`）；`probes/host_identity.ps1` = Windows PowerShell 5+（`Get-CimInstance` / `Get-NetTCPConnection`，只读）。§3.2 的 HTTP/WS 探针用 Python 标准库 `http.client` 即可复跑（信封形状见 §3.2 表末段）。
- **隔离要求（复跑前必读）**：必须先 `set DSH_HOME=<影子目录>` 再跑 `dsh.cmd`，否则会落到真实 `~/.dsh` 与真实 Desktop 抢同一数据面（违反 AGENTS.md 采集域纪律）。本轮产物 `isolated/` 已整体删除。
- **截图**：本轮**未提供桌面截图**。原因：需要核对的都是桌面 UI 侧的「授权资料交付入口」是否存在（§7 待人工补验 ①），而该核对要在用户自己的 Desktop 会话里点原生入口；本轮不改动、不点击用户界面，因此没有可截的桌面结果，也不伪造截图占位。协议层证据一律以脱敏日志与源码行号交付。
- **未采集项**：无有效 token / cookie 值 / API Key 入库（cookie **名** 是 `dsh-auth-` + base64url(sha256(authority))，由 URL 明文可推导，不构成秘密；**值** 只登记字节长度）。

### 证据索引（本目录）

| 文件 | 内容 |
| --- | --- |
| `logs/boot2.log` | 隔离宿主 CLI 打印的授权 URL（token 已脱敏） |
| `logs/http_5214_probe.json` | Desktop Host 的 `/`、`/connect`、`/api*`、随机路径的状态码与响应体 |
| `logs/http_unauth_matrix.json` | `/connect` 的 interactions 校验 400 / 无 token 401 / 假 token 401 + 裸 WS Upgrade 401 原文 |
| `logs/independent_connect_exchange.json` | 303 索引 cookie 与 302 API-Peer cookie 的**名称与长度**（不含值） |
| `logs/independent_rpc_probe.json` | `/api/<ns>/<method>` 形状探测（404 vs 200+gateway 错误） |
| `logs/independent_rpc_ok.json` | `session/list → ok:true`、`session/search` 业务校验、`commands/list` descriptor 报错 |
| `logs/help.txt` | 官方 `dsh` CLI 帮助原文（`--profile/--from-default-profile/--patch/--dump-config/plugin add`） |
| `probes/host_identity.ps1`、`probes/asar_probe.py`、`probes/mux_ws_probe.py` | 可复跑只读探针（mux 探针含 RFC6455 客户端帧编解码，纯标准库） |
| `logs/mux_ws_probe.txt` | 101 握手 + `Sec-WebSocket-Accept` 校验 + mux 帧往返（结构化错误回帧） |
| `logs/interactions_gate.txt` | `none` vs `answer` Peer 的 `$events/result` 对照结果 |
| `logs/ui_entry_search.md` | 972 个官方文案文件的「远控/配对入口」检索，含 CJK 对照组 |

---

**报告作者视角的三句话结论**：
1. Windows 官方 0.2.1-rc.3 包里 `/connect`、独立 API Peer、`interactions=answer|none`、`/api` 桥、`/api/remote.mux`、附件/模型/会话管理面**逐项存在**，且零插件、非浏览器环境下用官方 HTTP 信封**真能跑通一次真实 RPC**（`session/list → ok:true`）；mux 的 WS 传输层（`101` + `Sec-WebSocket-Accept` 校验 + 帧往返）与 `interactions` 资格闸（`none` 被 `gateway/interactions-forbidden` 拒、`answer` 过关）也都已实测。**协议本身不构成障碍**。
2. 真正的闸门是**授权资料的交付**：Desktop 的 launch token 只经子进程 IPC 交给 Electron 父进程，不落盘、不打印、无可读入口；`dsh web` 的「打印 URL」是另一进程、另一 profile、另一 token，**不能**据此操控 Desktop 的同一会话。因此「纯无插件直连官方 Desktop」在 Windows 上**当前不成立**（未验证是否存在官方分享入口）。
3. 社区插件的价值不在转发聊天（它没有一行上传/审批代码），而在**宿主进程内代兑换官方 cookie + 配对/设备会话 + 局域网与隧道暴露**；且它在本机**已安装但未启用**。推荐「官方 RPC + 自研最小授权插件（B）」并借用官方 `dsh-api-remote-access` 的 Peer 策略做方法级白名单，只读面继续走已验证的 C 路线。
