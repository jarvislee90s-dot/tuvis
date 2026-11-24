# macOS 官方 DSH Desktop 原生远控接入探测报告

> 任务：以源码 + 实机调用证据，判断兔维斯应「直接连接官方 RPC」「官方 RPC ＋ 最小授权插件」还是「需要业务桥接」。
> 本轮为可得性探测，不进入正式功能开发，不宣布任何产品验收。
> 证据等级标注：**源码确认** / **实测通过** / **推断** / **未验证**。

## 0. 一页结论

| 问题 | 结论 | 等级 |
| --- | --- | --- |
| 官方是否提供独立 API 客户端通道 | 有：`GET /connect?token=&lt;启动令牌&gt;&interactions=answer\|none` → 302 + 签名 API cookie + Mux 目标；随后 `POST /api/{ns}/{method}` 一元、`/api/remote.mux` 逻辑流 | 源码确认＋实测通过 |
| 独立客户端（无浏览器、无插件）能否跑通协议 | 能。本轮用手写 Node 探针（fetch + 裸 WebSocket 帧）完成鉴权、事件订阅、建会话、发 prompt、会话管理、原始字节上传、模型目录/切换 | 实测通过 |
| 能否连上**正在运行的 DSH Desktop 宿主** | **不能（现状）**：Desktop 宿主以 `--host 127.0.0.1 --port 0` 启动，随机端口与 per-process 启动令牌只经 child→Electron 母进程 IPC 传递，不落盘、不进日志、也没有任何桌面导出入口（四路穷举） | 源码确认＋实测旁证（401／日志 0 命中／文案 0 命中）＋目视待补 |
| 社区插件是否不可替代 | 对**独立 RPC 客户端**而言：其转发层与浏览器垫片不可需；其价值集中在「配对/设备凭据授权层 + LAN/异源接入」 | 源码确认 |
| 推荐路线 | **官方 RPC ＋ 自建最小授权层**，落地形态二选一：① 兔维斯自启官方宿主（`dsh --profile <web 形态>`，端口/令牌天然可得，已全量实测）；② 等官方在桌面端加「授权/显示连接资料」入口后做纯直连。**不建议**把插件 `/remote` 当数据通道 | 见 §7 |
| 真实回合（模型/审批/问答）能否由独立客户端闭环 | **能**：换用可用的供应商档位（`opencode-go`）后，独立客户端收到审批与问答 waterfall 实帧、作答后宿主按我的取值裁决、命令实际执行/被拒 | 实测通过 |
| 测试纪律纠偏 | `~/.dsh/profiles/web` 里装了 8+ 第三方包，其 client 会在宿主进程内抢答 `approval/request`；**审批类用例必须在只含官方两包的纯净 profile 上测**（配方见 `probe/clean-profile.md`） | 实测通过 |
| 安全红线 | `/connect` 出来的 Peer principal 恒为 **owner**，不受 `dsh-api-remote-access` 方法级门限，等于全权（含 `terminal/*`、`credentials/set`、`settings/mutate`）。兔维斯远控面必须按 §4 白名单收敛 | 源码确认 |

## 1. 环境、版本、包来源与源码摘要

| 项 | 值 | 取证方式 | 等级 |
| --- | --- | --- | --- |
| 宿主 App | `/Applications/DeepSeek Harness.app`，bundle id `com.deepseek.dsh` | `PlistBuddy` | 实测通过 |
| 版本 | **0.2.1-rc.3**（`CFBundleShortVersionString` 与 `CFBundleVersion` 同值）；CLI `dsh -V` 亦为 `0.2.1-rc.3` | Plist + CLI | 实测通过 |
| 运行形态 | Electron 主进程 pid 4628；宿主子进程 pid 5011 = `--expose-internals app.asar/dsh/node_modules/@deepseek-ai/dsh-desktop-host/lib/index.js` + profile `/Users/jarvis/.dsh/profiles/desktop` | `ps` argv | 实测通过 |
| Desktop 宿主监听 | `127.0.0.1:63026`（LISTEN，IPv4） | `lsof -iTCP -sTCP:LISTEN` | 实测通过 |
| 包来源 | 官方包全部来自 `app.asar/dsh/node_modules/@deepseek-ai/*`（306 个包）；本轮把 `dsh/` 子树抽出为静态源码树用于取证（6196 文件 / 110 MiB） | 自研 asar 读取器（偏移：`CONTENT=8+u32LE(4)`，header JSON 在 16，长 `u32LE(12)`=4748698） | 实测通过 |
| 官方包版本一致性 | `dsh-api-remote-access` / `dsh-client-connection` 等 `package.json` version 均为 `0.2.1-rc.3` | 逐包读取 | 源码确认 |
| 关键包（协议面） | `dsh-client-connection`（浏览器/API 认证、`/connect`、Peer、fetch bridge）、`dsh-api-gateway`（Typert RPC + `/api/remote.mux`）、`dsh-api-remote-access`（非 owner Peer 方法级 ACL）、`dsh-api-remotes`（Remote 装配与转发事件白名单）、`dsh-host-webserver`（绑定/TLS/trustedHosts）、`dsh-desktop-host`（桌面宿主引导）、`dsh-web-app`（CLI 打印带 token 的 URL） | 各包 `README.zh.md` + `lib/*.js` | 源码确认 |
| 社区插件 | `@linxin666/dsh-remote-web-ui@0.4.11`（装在 **desktop** profile；`package.json` → `dsh.profile.bundles`），peer 声明 `@deepseek-ai/dsh >= 0.2.1-rc.3` | 插件目录 | 源码确认 |
| 探针运行环境 | macOS（arm64）、Node v24.18.0（探针只用 `node:net`/`node:crypto`/全局 `fetch`，**零依赖**） | 实测 | 实测通过 |

**官方宿主启动参数（决定性）**：`dsh-desktop-host/lib/index.js` 用 `["--no-open","--host","127.0.0.1","--port","0"]` 拉起 web 形态宿主；绑定后 `const url = ctx.connection.authenticatedUrl(\`//127.0.0.1:${port}\`)`，再 `process.send({type:"ready",url,injections})`；Electron 母进程 `main.js` 把它 `readyResolve({url, injections})` 用于渲染文档。**全链路没有落盘、没有 UI 导出**（源码确认；本轮未读到任何菜单/剪贴板导出该 URL 的代码路径）。

## 2. 插件职责拆解表（它到底隔离了什么）

插件 cordis 名 `remote-web-ui`，`inject = ['webServer','typertGateway','connection']`，浏览器半通过 `dsh.client.inject` 注入官方 `dsh-client-connection/ui-settings/ui-sidebar`。路径前缀省略：插件源码 `~/.dsh/profiles/desktop/node_modules/@linxin666/dsh-remote-web-ui/src/`，官方抽取树 `$R/`。

| # | 职责 | 关键实现（文件:行） | 分类 | 独立客户端是否必需 |
| --- | --- | --- | --- | --- |
| 1 | 配对令牌签发（一次性、同时仅一枚、TTL 10 分钟），二维码内容 `<base>/pair-accept?pair=<token>` | `pairing.ts:224`（`randomBytes(16).hex`）、`routes.ts:843`（`loopbackFence` 才允许 `POST /api/pair/issue`）、`routes.ts:880` | **授权适配** | 语义必需，但官方已有等价物：启动令牌→`/connect` 换 cookie |
| 2 | 设备 cookie 与会话判定（`dsh_pair=<deviceId>`，`SameSite=Lax`，1 年） | `routes.ts:1335`、`gate.ts:143-145`（`touchDevice`） | **授权适配** | 同上（兔维斯可自建设备表） |
| 3 | 设备/注册表持久化 | `~/.dsh/remote-web-ui-devices.json`＝`{<deviceId>:{createdAt,lastSeenAt,userAgent?,name?}}`（`pairing.ts:74-90,337-350`，0600 原子改名）；`~/.dsh/remote-web-ui-registry/<profile>.json`＝`{id, secret}`（**中继租据凭据**，值已脱敏，`relay-registry.ts:56,90`） | **状态补偿** | 否 |
| 4 | Host/Origin 可信判定与环回判定 | `routes.ts:76-95`（`isTrustedApiRequest`：Origin.host===Host.host 或命中 trustedHosts）、`gate.ts:95-101`（`isLoopbackClient`：环回 socket + 环回 Host 且**无** XFF/CF-Connecting-IP） | **授权适配** | 官方栅栏更严（见 §3） |
| 5 | **内部凭据兑换**：进程内调官方 `ctx.connection.authenticatedUrl()` 拿带 token 的 URL，`fetch(..., redirect:'manual')` 抓 `dsh-auth-*` cookie 并缓存，附给所有内发请求 | `inner-auth.ts:44-58`、`index.ts:845-862`（`ctx.connection`/`ctx.webServer`）、`index.ts:847` | **授权适配** | **必需**——这正是独立进程拿不到的那一步 |
| 6 | `/remote` 前缀全量反代（`kind:'prefix'`，逐字节到 127.0.0.1 同端口） | `remote-methods.ts:16`、`remote-api.ts:190` | **转发** | 否（兔维斯同机直连官方路径） |
| 7 | WebSocket Mux 代理：手工重写升级行与 4 个 `sec-websocket-*` 头、注入内层 cookie、`head` 之后双向 `pipe` 原样字节；无帧级处理、无应用层心跳，仅双腿 `setKeepAlive(20_000)` | `remote-methods.ts:38-45`、`loopback-proxy.ts:229-274` | **转发** | 否 |
| 8 | 上传通道专项适配：官方 file-upload 的后台 Worker 裸 XHR 会逃出门控→401，插件用 boot 补丁发布 `__DSH_FILE_UPLOAD__.fetch` 把它拉回被改写的主线程 fetch | `remote-channel-boot.ts:166-188`（自述对应上游 issue #1580） | **纯网页适配** | 否 |
| 9 | 业务转换 | 核心**逐字节透传**；仅两处合成：门控 403 复用官方信封形状 `{type:'server-response',rpcId,result:{ok:false,error:{code:'unpaired'\|'forbidden'}}}`（`remote-api.ts:94-99`）；header 面改写（剥 origin/referer/cookie/x-forwarded-*/sec-*，塞合成 `sec-fetch-site: same-origin`，`loopback-proxy.ts:49-92,120-128`） | **转发** | 否 |
| 10 | 额外状态：一次性 grant 表（60s，消费即删）、`/api/pair/events` SSE（打开即快照，**无历史重放**）、10s 心跳刷 `lastSeenAt`、隧道/中继租约状态 | `pair-grant.ts:99-113`、`routes.ts:1073-1083`、`client/index.ts:176` | **状态补偿** | 否（官方 `ConnectionController` 自带 500ms→10s 抖动退避与 generation 重建） |
| 11 | 刻意本机专用 | `LOCAL_ONLY_PREFIXES = ['/api/pair','/api/update','/api/plugin-manager']`（`remote-methods.ts:71-77`，对已配对设备也拒绝）；`loopbackFence` 把守 issue/stop/revoke/rename/events/lan-bind（`routes.ts:741,843,942,957,982,1007,1075,1087`）；status 对未配对远端只给 `{phase,lanAvailable}`，设备名册不出环回（`routes.ts:1064-1070`） | **业务隔离** | 兔维斯需自建同类门（见 §4 风险面） |
| 12 | 注意：终端 WS、设置写入**不在**插件的本机专属表内；其 README 安全模型自述「配对设备＝完全控制凭据」 | 插件 README 安全模型段 | **业务隔离（缺项）** | — |

**一句话**：该插件 ≈ 「配对/设备授权层 ＋ 异源浏览器 URL 改写垫片 ＋ LAN/隧道接入」。它**不转换业务语义**，也**不维护与官方对齐的业务状态**；`/connect` **未被复用**（全库无引用，内层走 `GET /?token`）。它不可替代之处只有第 5 行：**进程内取宿主自己的启动令牌**。

## 3. 授权与直连步骤，及「同一 Desktop Host」的身份证据

### 3.1 官方授权链（源码确认）

1. 宿主每个进程生成随机启动令牌（`dsh-client-connection/lib/index.js:275 SECRET_BYTES=32`、`:294-300 processLaunchToken`，仅内存 WeakMap，**不落盘**）。
2. `dsh-web-app` 打印并打开带 `?token=` 的 URL（CLI 帮助原文：「advertise this HTTP(S) root in the printed, opened, runtime-context, and **DSH_WEB_URL** forms」）。
3. 浏览器路线：`frontend-static` 把根路径交给 `ctx.connection.authorizeIndex`，**只在 `GET /` 接受 token**，写 host-only 签名 cookie（名＝`dsh-auth-` + base64url(sha256(authority))，`Path=/`、`HttpOnly`、`SameSite=Strict`、默认 30 天，TLS 时加 `Secure`），再 302 去掉 token。
4. API 路线（兔维斯该走的）：`GET /connect?token=&lt;启动令牌&gt;&interactions=answer|none` → **302 + `location: ./api/remote.mux` + 签名 API cookie**；principal 恒为 `owner`；缺省 `none`，非法/重复声明 → 400；一条 API socket 独占清理：重复关联 → **409**，socket 关闭即移除 Peer，此后该 cookie 的 HTTP 被拒。
5. 信任栅栏先于认证：`Host` 必须是 loopback / 等于监听器绑定 IP 字面量 / 命中 `trustedHosts`；`Origin` 若存在必须同 authority；`sec-fetch-site: cross-site` 一律拒。栅栏失败 403，可信未认证 401。**`dsh web --host 0.0.0.0` 官方明确不支持**。
6. 令牌只在根路径交换与 `/connect` 接受，**不接受 Authorization 头 token**。

### 3.2 本轮实跑的直连步骤（实测通过，宿主＝官方 `dsh --profile web`，无插件、无浏览器）

```bash
# 1) 由我们自己启动官方宿主 → stdout 打印带令牌的 URL（授权材料合法来源）
"/Applications/DeepSeek Harness.app/Contents/Resources/runtime/cli/bin/dsh" --profile web --no-open --port 37411
#    → http://127.0.0.1:37411/?token=&lt;43 字符&gt;

# 2) 换 API Peer 凭据
curl -i "http://127.0.0.1:37411/connect?token=&lt;令牌&gt;&interactions=answer"
#    → 302 / location: ./api/remote.mux / set-cookie: dsh-auth-4vYVr…=&lt;签名&gt;; Max-Age=2592000; Path=/; HttpOnly; SameSite=Strict

# 3) 一元 RPC
curl -X POST http://127.0.0.1:37411/api/session/modelCatalog \
  -H 'content-type: application/json' -H "cookie: dsh-auth-…=&lt;签名&gt;" \
  -d '{"type":"client-request","rpcId":"p1","method":"session/modelCatalog","payload":{"args":{}}}'

# 4) 事件流：裸 WebSocket 升级 /api/remote.mux（Node 全局 WebSocket 不能带 cookie，故手写握手）
{ "type":"open", "streamId":"ev", "endpoint":"$events", "payload":{"args":{}} }
→ { "type":"item","streamId":"ev","value":{"type":"ready","clientId":"<uuid>","host":{"home":"/Users/jarvis"}}}
```

同一命令序列的完整可复跑脚本见 `probe/`（`probe.mjs` 连接与一元、`rest.mjs` 语义/会话管理/上传/模型、`final.mjs` 事件流与 follow；令牌只从环境变量读，日志已掩码）。

### 3.3 「目标是官方 Desktop 当前会话」的证据与缺口

| 证据 | 观测 | 等级 |
| --- | --- | --- |
| Desktop 宿主确实在监听并可被外部 HTTP 触达 | `lsof` → pid 5011 `127.0.0.1:63026 LISTEN`；`GET /` 与 `GET /connect` → **401 `dsh web authentication required; reopen the URL printed by dsh web.`**（与源码字面量一致） | 实测通过 |
| Desktop 宿主已挂载插件（同源进程） | `GET /remote/` → **403 `{"type":"server-response","rpcId":"invalid-request","result":{"ok":false,"error":{"code":"unpaired",…}}}`**（插件自有信封） | 实测通过 |
| Desktop 宿主与 web 形态宿主是同一套协议代码 | 同一 asar 内的同一包树；桌面启动参数即 `--profile web` 形态 + `--port 0` | 源码确认 |
| **连上 Desktop 宿主当前会话** | **未达成**：端口可用 `lsof` 观测，但启动令牌只在 Electron 母进程内存（`process.send` → `readyResolve({url})`），官方无导出入口；本轮**没有**从 Electron `Cookies` 库/凭据文件提取 cookie，也**没有**用调试器读进程内存（任务明令禁止） | 实测（受阻）＋未验证（GUI 是否有入口：Computer Use 权限未授予，见 §8） |
| 若必须借插件取授权 | 记为**依赖项**：插件只在进程内用 `ctx.connection.authenticatedUrl()` 取令牌，未对外暴露；因此「插件给授权材料」也需要官方/插件再加一个导出面。**当前不存在纯无插件连 Desktop 宿主的授权路径** | 源码确认 |

**第四路穷举＝桌面端 UI 文案（源码确认）**：Electron 母进程 `app.asar/lib/main.js` 里「远程 / 局域网 / 配对 / 令牌」命中数 **0**（`Pair`×30 全是 YAML `pairs` 内部件、`Remote`×46 全是 `RemoteError` 之类标识符，`title:"…"` 菜单字面量 0 个）；306 个官方包的 `locale/zh.json` 里同样只有「远程调用网关」「遮挡…令牌」等词，**没有任何「远程访问/设备配对/显示地址」类入口文案**。⇒ 判断：0.2.1-rc.3 官方桌面端**不存在**把连接资料交给用户的入口（目视核对仍作为最终背书，见 §9-1）。

**授权材料不可得性的三路穷举（实测通过）**：① 文件系统——`~/.dsh` 全树无任何含端口/令牌的落盘件（`grep -rlE "127\.0\.0\.1:6[0-9]{4}" ~/.dsh` → 空）；② 统一日志——`log show --predicate processImagePath CONTAINS "DeepSeek Harness" --last 20m` 中 `token=`/`authenticatedUrl` 命中数为 **0**；③ 官方导出面——桌面母进程收到 `{type:"ready",url}` 后只用于渲染文档（`readyResolve({url,injections})`），未见菜单/剪贴板/文件导出路径（源码确认）。

**跨进程边界（重要，源码确认 + 实测旁证）**：宿主是「会话的持有进程」。`session/list` 行携带 `agentAvailable/running/blank/parentSessionId`，`session/prompt` 文档语义是「显式 resume 其 Session 后准入」，并有 `session/conflict`、`session/agent-busy`、`session/writer-held` 等锁码；另在 `dsh-api-gateway/lib/index.js` 看到 `ownership fence` 抛独立 `RemoteError`。→ 由兔维斯自启的宿主**能看见** Desktop 持有的会话行，但**不能替代** Desktop 去驱动其正在运行的 Agent（**推断**，本轮无并发持有样本可实测，见 §4 未测项）。

## 4. 功能矩阵

列定义：**官方接口**＝源码/类型确认存在；**插件源码支持**＝插件是否转发该路径（`/remote` 全量反代 ⇒ 一律「是」，个别前缀本机专属）；**插件实测**＝前轮 Chrome 经局域网（用户已确认）；**独立 RPC 实测**＝本轮无浏览器无插件探针；**未测原因**＝明确写出。

| 能力 | 官方接口（ns/method） | 插件源码 | 插件实测 | 独立 RPC 实测 | 未测原因 |
| --- | --- | --- | --- | --- | --- |
| 文本发送（准入） | `session/prompt {requestId,sessionId,mode:'queue'\|'steer',content:[text\|image\|file],clientTimeZone}` | 是 | 是（前轮发消息） | **实测通过**：`ok:true {accepted:true}` ×3 次 | — |
| 流式状态/事件 | `session/follow {request:{address:{kind:'session',sessionId}}}`；`$events` 转发；事件名 `turn/start,end`、`step/start,end`、`user/message`、`assistant/attempt`、`agent/inbox/spliced`、`tool/call,result` | 是 | 是 | **实测通过（真实回合）**：snapshot（format v4）→ `agent/inbox/spliced` → `turn/start` → `step/start` → `system/message` → `assistant/attempt` → **`assistant/message` 终值文本 `DSH-PROBE-PONG`** → `step/end` → `turn/end`；`$events` 同步 emit `api-session/{status,activity}`（`runlogs/live-text.txt`） | 逐 token 的 delta 帧粒度未量化（只见 `assistant/attempt.stream` 结构，未统计每帧条数）
| 会话投影/控制 | `session/control`、`session/projections`、`session/page` | 是 | 未测 | **未测**：`session/control` 参数名未知（`unexpected "request"` / `unexpected "agent"`，见 `raw/final-frames.log`） | 未继续穷举形状（时间预算）；下一轮可按 `typert.host.js:1505` 描述符逐字段试 |
| 文件/图片上传 | 精确路由 `POST /api/session/uploadFileBinary?sessionId=&name=`（`application/octet-stream`，非 multipart）；类型面 `fileUploads/upload`；绑定用 `content:[{type:'file',receiptId}]`；读回 `session/attachment` | 是（逐字节管道 + 浏览器 Worker 垫片） | 是（前轮写入测试文件） | **实测通过**：`{ok:true,value:{receiptId:<uuid>,file:{attachmentId:"sha256:c5749…",name:"probe-sentinel.txt",bytes:33}}}` | 桌面显示与「消息绑定后 Agent 真读到内容」未实测（402） |
| 图片归一化限制 | `imageLimits.maxImageBytes/maxImagesPerMessage/…` | 是 | 未测 | **未测** | 需要真实回合 |
| 模型目录 | `session/modelCatalog` | 是 | 未测 | **实测通过**：`default{provider:deepseek-official,model:deepseek-flash}`、`routableProviders[official,account]`、`groups[].models[].reasoning.efforts[off,low,high,max]`+`defaultEffort:high` | — |
| 模型/推理等级切换＋默认副作用 | `session/selectModel {provider,model,reasoningEffort}` | 是 | 未测 | **实测通过**（会话级生效）＋**副作用确认**：selectModel 会写「默认模型 profile patch」（`dsh-agent-default-model`，实测把 `provider/model` 落进 `~/.dsh/profiles/web/cordis.patch.yml:39-43`）；且 `modelCatalog.default` 读取滞后一次调用（观察到 restore 后 default 仍显示上一次值） | 「下一次真实请求是否采用所选模型」未实测（402） |
| 审批：允许一次 / 拒绝 | 转发 waterfall `approval/request`（`mode:'waterfall'`，仅 owner＋`interactions:'answer'` Peer 收/答）；outcome `'allowed-once'|'rejected'|'cancelled'|'unavailable'`；**官方无「永久允许」取值**；作答走 `POST /api/$events/result`，args `\{clientId,eventId,outcome\}`（`outcome.kind='next'` 放行给下一个 / `'result'` 带值） | 是（前轮「允许一次」实测） | 是 | **实测通过（闭环，纯净宿主）**：`/permission read-only` 降档（`commands/execute {agentId,line,submittedAttachments}` → `result.kind=success "preset read-only"`）→ bash 升级请求触发 waterfall 实帧 `{type:"waterfall",event:"approval/request",eventId,agentId,request:{toolName:"bash",callId,reason,displayReason}}` → 我发 `POST /api/$events/result {clientId,eventId,outcome:{kind:"result",value:"allowed-once"}}` → 宿主 `approval/decided outcome:"allowed-once"` → **哨兵文件真创建**；改答 `"rejected"` → 224ms 后 `decided:"rejected"` → **哨兵未创建**（`runlogs/live-approve-allowed-once.txt`、`runlogs/live-approve-rejected.txt`） | 「永久允许」确实不存在（全树无 allow-always 词）；未测 `cancelled`/`unavailable` 两值
| 问答题型 | `user-questions/request`（waterfall）；`AskUserQuestionItem{id,question,detail?,header?,options?,multiSelect?,intent?}`；`intent` 仅 `plan-review`；开放期在事件里答，过后 `userQuestions/answer`；跳过＝`selected:[]`；重复回复＝`REPLY_QUEUED`；**无秘密输入题型** | 是 | 未测 | **实测通过**：实帧 `{type:"waterfall",event:"user-questions/request",eventId,agentId,request:{questions:[{id:"fruit_choice",question:"选苹果还是香蕉？",header:"水果选择",options:[{label,description}×2]}]}}` → 作答 `outcome.value={answers:[{id,selected:["苹果"]}]}` → `ok:true`，答案进入会话日志（`runlogs/live-ask-x.txt`） | 多题、`multiSelect`、自由文本 `custom`、`intent:plan-review` 未穷举；本轮实帧内**没有** isSecret/isOther 字段
| 打断/取消 | `session/cancel {sessionId}` → `{accepted:true}`；只停当前回合、**保留 pending inbox**；`subagents/interruptByParent`、`job/kill` | 是 | 未测 | **实测通过**：`ok:true {accepted:true}` | — |
| 队列编辑/撤回 | `session/updateQueue {sessionId,itemId,action:{kind:'edit'\|'remove'\|'steer'}}`；**无重排/移动接口**（顺序变化只由 `agent/inbox/spliced` 回显） | 是 | 未测 | **部分实测**：非法 itemId → `session/queue-item-not-found "queued item is no longer pending"`（说明动作被解析、只缺真实 pending 项）；另观察到 `agent/inbox/spliced` 的 removed/inserted 序列 | 真实队列项需成功回合（402） |
| 新建会话 | `session/create {request:{workspaceId\|cwd,sessionId?,agentPreset?}}` | 是 | 未测 | **实测通过**：`{sessionId,agentPreset:"standard"}` ×2 | — |
| 恢复/打开 | **无独立 resume 方法**：由 `scope/lookup` 在 prompt 等调用里隐式恢复 | 是 | 未测 | **实测通过（间接）**：对既有 sessionId 直接 prompt 成功 | 「跨宿主接管 Desktop 持有会话」未测（无并发持有样本） |
| 重命名 | `session/rename {sessionId,title}` → `{title,seq}` | 是 | 未测 | **实测通过**：`{title:"dsh-probe-renamed",seq:7}` | — |
| 分支 | `session/fork {sessionId,atSeq?,allowMigration?}` → `{sessionId}` | 是 | 未测 | **实测通过**：新 `session-56aa978a…`，`parentSessionId` 正确，`$events` 同步 emit `api-session/added` | — |
| 搜索 | `session/search {query}` | 是 | 未测 | **实测通过（返回不支持）**：`session search is disabled: this deployment configures the session-query index …` | 该部署未启用索引（属**配置不支持**，不是协议缺失） |
| 归档/取消归档 | `workspace/archiveSession` / `unarchiveSession` → `{archivedSessionIds:[…]}` | 是 | 未测 | **实测通过**（返回值是**整个归档集合**而非本次变更集：两次调用都原样带回 5+ 条**用户既有**归档会话；对照落盘的 `~/.dsh/storages/workspace.json` → `global.archivedSessionIds` 里**没有**我的探针会话） | — |
| 置顶/取消置顶 | `workspace/pinSession` / `unpinSession` → `{pinnedSessionIds:[…]}` | 是 | 未测 | **实测通过**：置顶后 `[我的会话]`，取消后 `[]` | — |
| 工作区 | `workspace/{create,rename,delete,insertBefore,insertSessionBefore,follow,initializeDefault}` | 是 | 未测 | **未测**（避免动用户工作区） | `workspace/delete` 风险高，主动不测 |
| 文件浏览/读 | `workspaceFiles/{list,read,readBytes,stat,changes}`、精确路由 `GET\|HEAD /api/file?path=<绝对路径>` | 是 | 未测 | **未测** | 可读工作区外文件 ⇒ 属风险面，不主动开放 |
| Skill | `skills/list {request:{sessionId}}`（**只读**）；skill 安装/启停**无 Remote 面** | 是 | 未测 | **实测通过**：返回真实技能（含绝对路径与中文描述，来自 `~/.mam/skills`） | — |
| Slash 命令 | `commands/{list,execute(agent,name,args,attachments?)}`；内置 `compact,feedback,goal,permission,plan,export` | 是 | 未测 | **实测通过（list）**；`execute` 未测 | execute 会改权限/触发压缩，风险与真实回合依赖 |
| 设置读写 | `settings/{describe,update,mutate,replace,openSettingsDocument}`；`credentials/set\|unset` | 是 | 未测 | **实测通过（describe）**：`{writable:true,hasDocument:true,namespaces[…]}` | **写路径主动不测**（能改 provider/模型 key/任意配置） |
| 权限预设 | `permissionPresets/catalog`（只读目录）＋ `commands/execute '/permission <preset>'` 应用 | 是 | 未测 | **实测通过**：目录 `read-only / workspace-write / danger-full-access` | 应用＝改档位，未在用户宿主上试 |
| 终端 | `terminal/{create,attach,close,list,rename,retain,shells,environment}`——描述明写「without Agent sandbox or approval restrictions」 | 是（**不在**插件本机专属表内） | 未测 | **主动不测** | 远控面禁止开放 |
| 账号/凭据 | `account/{startSignIn,cancelSignIn,signOut,getProfile,getState,getBalance,…}` | 是 | 未测 | **未测** | 凭据相邻，风险面 |
| 插件装卸 | `pluginManager/{installBundle,removeBundle,setBundleEnabled,…}`、`pluginMarket/*` | 是 | 未测 | **主动不测** | 等价于远程代码执行 |
| 官方 SDK（另一套） | `dsh-sdk-protocol` stdio NDJSON-RPC（7 请求＋4 通知） | 不适用 | 不适用 | **未测** | 它是「自带执行器的 SDK 运行时」，不连运行中的宿主 |
| 官方 Node 客户端复用 | `dsh-client-connection` `exports['./client'].node = lib/client.node.js`（24 KB，依赖 `globalThis.fetch/crypto`，WebSocket 走可注入 factory + `streamBaseUrl`）；`dsh-api-gateway/lib/client.node.js`（72 KB） | 不适用 | 不适用 | **源码确认**（本轮探针未使用它，改用手写帧以保证可带 cookie） | 需真实宿主上跑一遍官方 client.node 才算实测 |

### 4.1 关键负面/恢复用例（本轮实跑）

| 用例 | 结果 | 等级 |
| --- | --- | --- |
| 无凭据 HTTP | `GET /`、`GET /connect` → 401 明文提示；`/api/*` → 401 `unauthorized`；未知路径 → 404（先路由后认证） | 实测通过 |
| `interactions=none` 的 Peer 订阅事件流 | `{"type":"error",…,"code":"gateway/interactions-forbidden","message":"$events: forwarded Remote events require interactions=answer"}` | 实测通过 |
| `$events` 传非空 args | `gateway/arguments-invalid … requires an empty args object` | 实测通过 |
| 不存在的 endpoint | `gateway/invocation-unavailable … no active Remote method exports this endpoint` | 实测通过 |
| 流式方法走一元通道 | `workspace/follow` 经 HTTP 一元 → `gateway/signature-invalid … stream Remote methods must be opened through the stream carrier` | 实测通过 |
| 同一 Peer 第二条物理 socket | 升级直接 **409 Conflict** | 实测通过 |
| 宿主重启后地址/凭据轮换 | 旧端口消失；新端口 `60214`、新令牌（43 字符，与旧值不同）；**旧 cookie 打新宿主 → 401 `unauthorized`**；新 `/connect` → 302 | 实测通过 |
| 参数校验 | 字段名不符 → `gateway/arguments-invalid`（列出 missing/unexpected）；形状不符 → `gateway/input-invalid`（strict codec） | 实测通过 |
| `$events/result` 良构 vs 畸形 args | 良构 → `gateway/internal: Remote event result identifies no active event stream`（说明校验已过、只差关联）；`{eventId}`/`{}`/`outcome.kind:'bogus'` → `api gateway: invalid Remote event result`；`clientId:'not-a-uuid'` 仍被判良构（clientId 形态校验不是 UUID） | 实测通过 |
| 幂等（同一 `requestId` 重放） | 两次都返回 `accepted:true`，**未见去重信号**；会话日志里只落地一条 `user/message`（观察自 follow 事件序列） | 实测（结果待定，见 §7 反证） |
| 断线重连补偿 | 未实测（官方 `ConnectionController` 自带 500ms→10s 抖动退避与 generation 重建；插件不做应用层心跳） | 未验证 |
| 审批已挂起后才连接 / 两端抢答 | 未实测。**原因已更新**：造挂起审批现在可复现（`probe/clean-profile.md` 的降档配方），本轮时间未覆盖 → 列为下轮首项 | 未测 |

## 5. 失败与恢复案例（本轮实录）

1. **`/connect` 无 token** → 401；**带 token** → 302＋cookie。恢复方式：正确取用官方打印的带令牌 URL（我们自启宿主 ⇒ 合法）。**实测通过**。
2. **asar 头偏移读错**（把 JSON 起点当 12，实际 16；`CONTENT=8+u32LE(4)`）→ 解析失败并刷屏 4.5 MB。修正后抽取 6196 文件成功。**实测通过**。
3. **判定「requests 已清」时读了 patches 帧**（旧教训沿用）：本轮一律以 snapshot 为准；follow 流首个 snapshot 给出 `cursor` 作为后续读取基线。**实测通过**。
4. **`session/list` 报 missing `_request`**：该 descriptor 的保留参数名就是 `_request`（不是 `request`）；错误信息直接给出字段名，据此修正。**实测通过**。
5. **`session/prompt/rename/fork/cancel` 首次全失败**：根因是 shell 里把 `SID=$SID` 写在命令**之后**，`process.env.SID` 为 undefined ⇒ 请求体字段缺失。**实测通过（失败原因自查）**。
6. **`session/follow` 三次形状失败 → 第四次成功**：正确形状 `payload.args.request.address.{kind:'session',sessionId}`；`kind:'session'` 之外的候选被 `input-invalid` 拒。**实测通过**。
7. **模型选择副作用**：`selectModel` 落盘写 `profiles/web/cordis.patch.yml` 的 `agent-default-model` 段，并且 `modelCatalog.default` 读值滞后一拍；已按 §8 恢复为 `deepseek-official/deepseek-flash`。**实测通过**。
8. **真实回合失败**：`assistant/attempt → chunk finish {kind:'error', failure:{message:"Insufficient Balance"}, code:'QUOTA', status:402}`，`turn/end` 带同一错误，`$events` 同步 emit `api-session/error`。事件通路正常、额度受阻 ⇒ 所有需要模型产出的格子标未测。**实测通过（失败取证）**。
9. **插件 `task-board`（web profile 里的第三方插件）在宿主 stdout 持续抛错**：`assertPeerLive` 读 `signal` of undefined ⇒ 第三方插件在进程内调 `ctx.remote` 未带 Peer，官方网关直接崩它的轮询。旁证「网关从不提供默认 Peer」。**实测通过（旁证）**。

## 6. 路线比较

| 维度 | A. 纯无插件直连 Desktop 宿主 | B. 官方 RPC ＋ 最小授权层（自建或借插件授权面） | C. 业务桥接（插件 `/remote` 当数据通道） | D. 兔维斯自启官方宿主 |
| --- | --- | --- | --- | --- |
| 依赖 | 只需官方 App 在跑；**缺官方授权入口** | 官方 App ＋ 兔维斯侧授权表（设备/令牌）；可选第三方插件仅用于取连接资料 | 官方 App ＋ **社区插件**（版本随上游漂移，peer 锚 0.2.1-rc.3） | 官方 App 内的 `dsh` CLI ＋ **Node 运行时**（或随应用打包） |
| 初始化成本 | 最低（但**当前不可得**） | 中：配对 UI＋cookie 安全存储＋`/api`+`/api/remote.mux` 客户端（本轮探针 ≈ 200 行零依赖） | 低（复用插件），但多一条转发腿 | 中：起进程、读 stdout 令牌、健康探测；端口占用/多实例治理 |
| 升级维护 | 最优（只跟官方协议） | 优（只跟官方协议） | 差（插件为第三方，改动不受控；且它自己也锚版本） | 中（跟官方 CLI flag，`--host/--port/--no-open` 已稳定；`--host 0.0.0.0` 官方拒绝） |
| 能否操控 **Desktop 当前会话** | 是（若官方给授权） | **否**——B 仍受 §3.3 授权缺口限制，除非官方/插件导出连接资料 | 是（前轮实测：局域网 Chrome 操作同一桌面会话） | **否**（新宿主＝新执行器；只能看行、不能接管 Desktop 持有的 Agent） |
| 配置迁移 | 无 | 需自建配对/设备表（语义可参照 dsh 式一次性令牌＋设备 cookie，MASTER-PLAN D5/D11 已有裁决） | 迁移即受制于插件状态文件（`remote-web-ui-*`） | 需管理 profile/工作区与凭据复用（用户已登录态可共享，本轮实测能直接用其账号） |
| 退出清理 | 干净 | 干净（撤设备、删自建表） | 需卸载/关闭插件入口并撤销设备 | 杀进程即可（本轮验证端口/令牌随之轮换、旧 cookie 失效） |
| 安全面 | owner 全权 ⇒ 兔维斯侧必须白名单 | 同 A | 同 A ＋ 插件把终端/设置写入也转发（**不在其本机专属表**） | 同 A，且宿主由我们启动 ⇒ 令牌暴露面更小 |

## 7. 推荐路线、成立前提、反证、未解决问题

**推荐：官方 RPC ＋ 最小授权层（B 形态），产品落地先做 D（自启宿主），并行争取 A（官方桌面授权入口）。**

理由（各条注明等级）：

1. 协议层**不需要业务桥接**：业务语义逐字节透传，官方网关自己提供方法校验、资源解析、ACL、幂等/取消、上行缓冲与错误码；插件也承认自己「零业务转换」。**源码确认＋实测通过**。
2. 独立客户端的全部必需件＝①连接资料（端口＋启动令牌）②`/connect` 换 cookie ③`POST /api/{ns}/{method}` ④`/api/remote.mux` 逻辑流 ⑤重连与 generation。除 ① 之外全部官方自备、本轮实测可用。**实测通过**。
3. 因此兔维斯要写的「最小层」只剩**授权与设备治理**（一次性配对码＋设备表＋cookie 安全存储），与 MASTER-PLAN D5/D11 既有裁决同构，不必移植插件的转发/垫片。**推断（基于源码分工）**。
4. 先做 D 的原因：Desktop 的 ① 当前不可得（源码确认），而 D 的 ① 天然可得且能全量实测；D 的语义是「另开一个宿主」，对**全新会话的远程操控**完全成立。**实测通过**。
5. 若产品硬要求「操控 Desktop 里那条正在跑的会话」，则 C（借插件通道）是唯一**已被证明可行**的路（前轮局域网实测），但它把安全边界交给第三方插件（终端/设置写入不过其本机专属门），且版本随上游漂移——**不推荐作为长期形态**。**源码确认＋前轮实测**。

成立前提：

- P1 官方保持 `0.2.1-rc.3` 的 wire 约定（`{type:'client-request',rpcId,method,payload:{args}}` / `server-response{rpcId,result}` / mux `open|item|end|cancel`）。破坏性改动会打到我们——需要版本闸门与探针复跑脚本（已在 `probe/`）。
- P2 兔维斯宿主与 DSH 同机或经隧道到 loopback；跨网必须走 `--trusted-host`/TLS（官方只允许具体接口字面量，拒绝通配）。
- P3 兔维斯侧白名单只放 `session/*`、`workspace/{pin,archive,follow}`、`skills/list`、`commands/list`、`userQuestions/answer`、审批作答；**默认禁** `terminal/*`、`credentials/*`、`settings/{mutate,update,replace}`、`pluginManager/*`、`account/*`、`workspaceFiles/read*`、`/api/file?path=`、`directoryPicker/*`。

反证（如果以下任一成立，结论要改）：

- F1 若官方桌面端**已有**用户可见的「显示/复制连接资料」入口，则 A 立刻成立，B 只剩「兔维斯自建配对表 vs 直接用桌面 cookie」的取舍。→ 源码侧已四路穷举（落盘/日志/导出路径/UI 文案）指向「无入口」；本轮 GUI 核对因 Computer Use 权限未获批而**未做最终目视**，仍是最高优先补测项。
- F2 若 `assistant/live-chunk` 等流式事件只能在浏览器 generation 下产出（本轮因 402 未证），D 的「流式」面要重评。
- F3 若「同 `requestId` 重放」其实产生了两次用户消息（§4.1 第 9 行观察不充分），则幂等语义需重估，兔维斯侧必须自建去重。

未解决（诚实清单）：

- U1 Desktop 宿主的授权材料可及性：源码四路穷举已指向「不可得」，仅差 GUI 目视最终背书（F1）。
- U2 waterfall **实帧形状**（`approval/request` / `user-questions/request` 下发到 `$events` 时携带哪些字段、eventId 从哪来）仍未取到样本；作答通道本身已实测可达（见 §4 审批行）。
- U3 `session/control` 参数形状、`assistant` delta 帧、附件绑定后被 Agent 真读到、`updateQueue` 三动作、`commands/execute`。
- U4 断线重连、两端抢答、挂起后才连接、结果未知（幂等/重复）。
- U5 跨宿主接管 Desktop 持有会话的**实测**（当前只有源码旁证）。
- U6 官方 `client.node.js` 在 Node 下的真跑（源码确认可用，未实测）。

## 8. 清理恢复结果

| 项 | 动作 | 核验 | 等级 |
| --- | --- | --- | --- |
| 自启探针宿主（本轮共启动 4 次：固定端口 37411、`--port 0` 随机端口、37422、37423） | 每个都 `SIGTERM` | 收尾核验 `lsof -iTCP -sTCP:LISTEN`：DeepSeek 只剩用户 Desktop 的 `127.0.0.1:63026`（pid 5011）；探针端口全部消失 | 实测通过 |
| 模型默认档位 | 按探针前读到的 `deepseek-official/deepseek-flash` 复位 `selectModel`；`profiles/web/cordis.patch.yml:39-43` 现为 `provider: deepseek-official / model: deepseek-flash` | 实测通过（注意：该 patch 段是探针期间新写入的，**值与探针前生效默认一致**；如需彻底回到「无显式 patch」状态，可删除该段——留给用户裁决） | 实测通过 |
| 网络暴露 | 全程未开启 LAN/隧道；插件设备表本轮开始前即为 `{}`（无配对设备），未新增设备、未点 `lan-bind` | `lsof`：仅 127.0.0.1；`remote-web-ui-devices.json` 未变 | 实测通过 |
| 会话与文件 | 探针专用会话：`session-00f8af65…`、`session-b2f3dd10…`（标题 `dsh-probe-renamed`）及其 fork `session-56aa978a…`；工作目录 `/Users/jarvis/Documents/dsh-probe-workspace`（空，未被 Agent 写入，402 未跑工具） | **保留**待用户裁决（不静默删用户库内记录）；目录已确认空 | 实测通过 |
| 凭据纪律 | 未读 `~/.dsh/.credentials.yaml`、未读 `remote-web-ui-registry/*` 的值、未碰 Electron `Cookies` 库、未用调试器读进程内存；所有入库日志对 token/cookie 一律 `<TOKEN>`/`<COOKIE>` 掩码，令牌/cookie 明文只留在本机私有 `raw/`（0600）且**不入库** | 见 `logs/`（已掩码）与本报告 | 实测通过 |
| 归档/置顶 | 探针调用 `pinSession/unpinSession/archiveSession/unarchiveSession` 之后，读落盘状态 `~/.dsh/storages/workspace.json`：`global.pinnedSessionIds = []`；三条探针会话 id（…b2f3dd10 / …56aa978a / …00f8af65）在全文件中 **0 次出现**；`global.archivedSessionIds` 只剩用户既有归档项 | **净零副作用**（`archiveSession` 返回值是「整个归档集合」，不是本次变更集，故必须这样核对而不是看返回值） | 实测通过 |
| GUI 核对 | Computer Use 权限（Accessibility / Screen Recording）弹窗未获批准 ⇒ 「桌面是否有官方远程访问入口」未看 | 待用户在弹窗授权后补测 | 未验证 |

## 9. 需要人工配合的补测项（集中列）

1. **批准 Computer Use 权限**，之后只需 3 步：DSH 桌面 → 设置/菜单里找「远程/Web/API/配对」类入口 → 截图。这直接判定 F1（能否纯无插件直连 Desktop 宿主）。
2. **给宿主一次可用额度**（充值或切到已配 key 的 provider），之后可一次性补齐 U2–U4 与矩阵里全部「需真实回合」的格子（审批允许一次/拒绝、问答题型、assistant delta、队列 edit/remove/steer、附件被 Agent 读到、两端抢答、挂起后才连接）。
3. 裁决：探针会话与空目录是否删除；`profiles/web/cordis.patch.yml` 的 `agent-default-model` 段是保留（值已正确）还是删除。
4. 若产品要「操控 Desktop 正在跑的那条会话」，需要向官方提功能请求（桌面端导出连接资料 / 桌面端 API Peer 配对入口），或在自有授权层里做一次性配对。

## 10. 附录：证据文件与哈希

入库件令牌/cookie 已掩码（全目录二次扫描：无泄漏）。私有原始记录（含真实令牌与 cookie）只在本机 `~/Documents/tuvis-verify-logs/dsh-native-rpc-macos-20261028T223500/`，权限 0600，**不入库**。

> 注：`macos-report.md` 一行的哈希是「写入本表之前的正文」哈希；复核时排除该行即可，其余文件哈希与内容严格一致。

| 文件 | SHA-256 |
| --- | --- |
| macos-report.md | `5442ddeaf965d0304e11049433d16a7505345694978abe461531626551f25356` |
| probe/README.md | `fadb8fc524aff4251991a9f033da9947e521b09c67a4b3639adceca3256dd819` |
| probe/clean-profile.md | `355b7b3a5aa8cbe4f471df89fe739a8439809731598db1cae29a4ac08495c348` |
| probe/probe.mjs | `df8575eddb4cd9c81521034c05ad2442b9066bb14dc1d8d9ebb2f046413eaf81` |
| probe/rest.mjs | `8e110ebcc9afe42ce9f3342f05783d4389937c6bbc1f662b3f32372a4290867f` |
| probe/final.mjs | `ba6b22f1b81cb41249ebd20d63d4ee843c5df39018e0cba2ac05f210e4c0969e` |
| probe/live.mjs | `eb2bcb07a624d725b4b46a1bdc09ad48a1f64ae8247017e6ab18d0c6c1ad752f` |
| probe/matrix.mjs | `854c2d735ba121f8a35be67a59bc846035ee25cc900ed4617c6c95e9b1e88669` |
| runlogs/e2e-frames.txt | `f3ff50fc5791907f29a356072a1761b2b6fc1c94173e8b920c420488c564a835` |
| runlogs/events-result-probe.txt | `35fb249b19e451b0b4d039c2bb039e0180cca8f22e8b155c7ce3bb936a7d8052` |
| runlogs/final-frames.txt | `3de4e7b13c5e11a4ae980c217339e1a80d1a0aa8bf9c3512704cf1de172c79f3` |
| runlogs/live-approve-allowed-once.txt | `cc68dc6d273656056c3a11dc41886337b8f9e5319d61179063512c4ba03dab47` |
| runlogs/live-approve-rejected.txt | `113e44f5f6dcef9a0597e1c8910ecc5a4b487ee3c52ba7b17d86980bb806a21f` |
| runlogs/live-ask-x.txt | `a79341e86ab1f2d373f37bfe7e2725e0f6cbbd3b5db4a14012fad45401d6e2a8` |
| runlogs/live-text.txt | `2db01b4a468898b9d75c07aae0e6ee9639204bed97a1b31c062ae1f09645649c` |
| runlogs/matrix-run1.txt | `0cbbb41b12f2b8225cc48442cb10b2b54646ba5859762b7cc198680751f78ae1` |
| runlogs/probe-inventory.json | `9ee2e69431a10f739af3c69e3240439906da5becb7a797b9e52b3385ba9c388d` |
| runlogs/rest-frames.txt | `2c4e625a84b6f4d38e3eddf13c9fa6517983f717678e1c02ab83ca5c5b5a1c0c` |

## 11. 第二轮补测（同日深夜）：真实回合闭环与纯净宿主纠偏

触发点：用户指出实际供应商是 **OpenCode**（官方账号未存额度）。核对 `~/.dsh/profiles/desktop/cordis.patch.yml:8-52` 后确认桌面档位由 `@deepseek-ai/dsh-llm-pi-ai` 声明 `opencode-go`（`apiKeyEnv` + `baseURL https://opencode.ai/zen/go/v1`）与 `zai-coding-cn`，并由 `agent-default-model` 定档 `opencode-go / deepseek-v4.1-flash / max`；密钥本身在 `~/.dsh/.credentials.yaml`（全局凭据库，**探针未读取其内容**）。把这两段作为 `--patch` 注入宿主即可路由：`session/modelCatalog` 的 `routableProviders` 由 2 个变 4 个，`groups` 多出 `opencode-go:[deepseek-v4.1-flash, muse-spark-1.3-contributor]` 与 `zai-coding-cn:[glm-5.3, glm-5.3-flash]`。**真实回合跑通**，之前因 402 标未测的格子现已补上（见 §4 三行）。

三条新结论：

1. **独立客户端可以答掉真实审批**（实测通过）。`ApprovalService.decide()` 用 `ctx.waterfall(target,'approval/request',req,()=>'unavailable')`，即 listener 的返回值**就是** ApprovalOutcome 字符串；因此 `{clientId,eventId,outcome:{kind:'result',value:'allowed-once'}}` 是被官方语义接受的合法作答（`dsh-user-approval/lib/index.js:176-182` 源码确认，且两轮实测的 `approval/decided` 与落盘效果都与我的取值一致）。
2. **第三方插件会污染审批测试**（实测通过，重要纪律）。第一轮在 `--profile web` 上跑，两次出现「我答 allowed-once → 宿主记 rejected」「我答 rejected → 哨兵仍创建」的反直觉结果；根因是该 profile 的 client 侧第三方包（remote-web-ui / task-board / web-ui-settings…）也注册了 `approval/request` listener 并抢先作答。改在只含 `@deepseek-ai/dsh-base` + `@deepseek-ai/dsh-web-app` 的 `probe-clean` profile 上，结果立刻稳定可复现。配方与残留（全局 `~/.dsh/cordis.patch.yml` 会注入一个 skin 包，不注册审批 listener）见 [probe/clean-profile.md](probe/clean-profile.md)。
3. **目视背书已完成**（实测通过）。Computer Use 权限生效后读取 DSH Desktop 无障碍树：侧边栏底部确有 `button 远程访问`，但该文案与实现全部来自 `@linxin666/dsh-remote-web-ui`（其 `lib/types/client/locales.js`、`README.zh.md` 命中「远程访问」；官方 306 个包 `locale/zh.json` 与 `lib/main.js` 对「远程/局域网/配对/令牌」**0 命中**）。⇒ §0/§3.3 的判定不变：官方桌面端没有导出连接资料的入口，用户看到的入口是插件贡献的；兔维斯若走 Desktop 直连，必须让官方新增入口或接受「授权层依赖插件」。

仍未闭合（下轮顺序）：① 审批已挂起后才连接（`hasLiveClient()` 与 pending waterfall 重放语义）；② 两端抢答（两个 answer Peer 同一 eventId 先后作答）；③ 断线重连与 generation 重建；④ `session/updateQueue` 三动作（现在可造真实队列项）；⑤ 附件经 `receiptId` 绑定后被 Agent 真读到 + 桌面显示核对；⑥ `session/control` 参数形状；⑦ 官方 `lib/client.node.js` 在 Node 下真跑；⑧ 跨宿主接管 Desktop 持有会话的实测。

清理增量：`probe-clean` profile 已删除（配方保留在 `probe/clean-profile.md`）；两个探针宿主（37431、37441）已 `SIGTERM`，`lsof` 复核仅剩用户 Desktop 的 `127.0.0.1:63026`（pid 5011）；`/tmp/dsh-probe-*.txt` 哨兵与探针会话仍保留待用户裁决；模型档位 patch 未写入用户任何 profile（只用了 `--patch` 临时叠加）。
