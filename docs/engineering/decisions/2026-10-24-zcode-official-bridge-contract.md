# ZCode 官方桥接接入合同（研究决策文件）

> 本文档是工作包 B 的固定产物（见
> `docs/superpowers/plans/2026-10-24-zcode-official-bridge-validation.md`「固定产物与接口」）。
> 当前仅含 **B0 部分**：头部身份与安装指纹、证据索引、官方模块与许可证清单、测试目标等待项。
> auth／route／operations 的实际身份与 wire 值**不在本文档预设**——它们由 B1—B4 以真实取证逐一追加，
> 未取得即 `capability=false` 并写明原因，绝不以 TBD 或伪造 endpoint 占位。

---

## 1. 头部（B0）

### 1.1 run_id 与平台

- **run_id（本轮 B0 取证）**：`macos-B20261024T142609+0800-k0zat1`
  （命名规则沿用探测手册 §4.2：平台＋含时区时间＋随机短码）。
- **platform**：macOS（ProductVersion 27.0.1，BuildVersion 26A434，arch arm64；本轮 `sw_vers`/`uname -m` 直读）。
- **私有证据目录（预留）**：`~/Documents/tuvis-verify-logs/zcode/macos-B20261024T142609+0800-k0zat1/`
  （内含 `raw/`、`sanitized/`、`report.md`；追加轮次不覆盖旧记录；B0 为仓库文档内只读取证，未创建该目录、未落任何 raw 文件）。
- **执行方式声明**：本轮全部为本机文件系统**只读静态检查**与 GitHub API 对固定提交的元数据核对；
  **未启动 ZCode APP／CLI、未运行 Tuvis、未执行任何构建或测试套件、未连接任何官方远端服务**
  （并行代理持有产品构建目录与提交权，见工作包 B 约束）。

### 1.2 本机安装指纹（本轮直取 + 沿用记录）

本轮**不启动任何进程**，因此运行版本沿用既有 PACKAGE 证据并显式标注；静态事实为本轮新测，不覆盖旧记录：

| 项                          | 值                                                                                                                                   | 证据级别                | 来源与本轮状态                                                                                                                                                                                                            |
| --------------------------- | ------------------------------------------------------------------------------------------------------------------------------------ | ----------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| ZCode.app 版本／build       | CFBundleShortVersionString `3.16.2`，CFBundleVersion `3.16.2.8769`                                                                   | PACKAGE（本轮直读）     | `/Applications/ZCode.app/Contents/Info.plist`（PlistBuddy 只读）；与 Mac P0 记录（`macos-20261023T2246+0800-acdb`，APP 3.16.2.8769）**一致，未升级**                                                                      |
| 随包 CLI 入口文件           | `/Applications/ZCode.app/Contents/Resources/glm/zcode.cjs`，15,243,891 B，mtime 2026-10-19 20:37                                     | PACKAGE（本轮直读）     | 同目录 `.node-bundle-meta.json`：`entry=zcode.cjs`、`runtime=electron-node`、`platform=darwin-arm64`、`source=apps/zcode-cli/packages/cli/dist/zcode.cjs`——随包文件自述其源码路径位于本固定 SHA 仓库内                    |
| 随包 CLI SHA256（本轮新增） | `a055234a6ca3da0172616d3987cf3c617b6a26deb1332d69035300c2a0ae0de1`                                                                   | PACKAGE（本轮直取）     | `shasum -a 256`；旧探测文档未记录该哈希，属**追加**新指纹，不改写旧证据                                                                                                                                                   |
| 随包 CLI 静态版本串         | `0.16.9`                                                                                                                             | PACKAGE（本轮复测命中） | 对 zcode.cjs 的静态 grep 命中；与 P0 复核「随包 CLI 静态串 0.16.9」一致，文件未变                                                                                                                                         |
| 随包 CLI 运行版本           | **未运行本轮**                                                                                                                       | UNPROVEN（Mac 侧）      | 沿用 `2026-10-24-capability-status.md` §4.1 口径：Windows 运行 help 为 `0.17.0`、DB 迁移版本 `0.16.9`、静态包字符串 `0.16.9`、APP `3.16.3.8992` **四者不可互推**；Mac 的 `0.16.9` 目前只是静态指纹，不能推断 Mac 运行版本 |
| 独立 PATH CLI               | 无（`zcode` 不在 PATH）                                                                                                              | PACKAGE（本轮直查）     | `which zcode` 未命中                                                                                                                                                                                                      |
| Tuvis.app 已安装版本        | `0.6.1`（CFBundleShortVersionString/CFBundleVersion）                                                                                | PACKAGE（本轮直读）     | `/Applications/Tuvis.app/Contents/Info.plist`；与 P0 记录（Tuvis 0.6.1）一致                                                                                                                                              |
| Tuvis 源码身份              | 分支 `codex/zcode-unified-remote`，HEAD `131346083f0f1112d4b53843d0905d72826ed430`，工作树 `/Users/jarvis/Documents/tuvis-worktree2` | SOURCE（本轮直查）      | `git rev-parse HEAD`（只读）；A 包由并行代理续提交，本文件只钉住 B0 取证时点                                                                                                                                              |
| Windows 安装身份（沿用）    | APP `3.16.3.8992`、随包静态串 `0.16.9`、run_id `windows-20261023T2247+0800-1rn60k`、P0-R+E1—E4 包 `zcode-windows-P0R-E1-E4-20261024` | PACKAGE/RELAY           | 沿用 P0 复核 §1 与 capability-status §1／§4.1；**本轮未在 Windows 机器重测**                                                                                                                                              |

历史安装指纹（Mac P0：3.16.2.8769／静态串 0.16.9；Windows P0：3.16.3.8992）保持原文不变；
若未来版本升级，只**追加**新 fingerprint 行，不改写本节旧记录。

### 1.3 固定官方源码 SHA 及钉固方式

- **官方源码基线**：`zai-org/ZCode` @ `3c5b227649371507ddc4eb46ac5133afe6db1cab`（提交日期 2026-10-20T06:23:05Z）。
- **钉固方式**：
  1. 由探测手册 §2（`docs/superpowers/plans/2026-10-23-zcode-probe/README.md`）以树链接首次钉固，
     P0 复核 §2.2 与 Mac 首轮／P0-R 提示词全部引用同一 SHA；
  2. 本轮经 GitHub API `GET /repos/zai-org/ZCode/commits/3c5b2276…` 复核该提交存在，
     committer 为「ZCode Source Sync Bot」，commit message 注明外部同步源
     `Source-Commit: 5a082039a1a3b1edfe8364a33c8978689c0d68bf`（即公开仓库为同步镜像，
     固定 SHA 指向同步提交本身；B1 引用官方实现时一律以本 SHA 的 blob 为准）；
  3. 安装版对应性判定沿用 P0 复核结论：**PARTIAL**——`glm/.node-bundle-meta.json` 的
     `source` 字段把随包 zcode.cjs 自述为本仓库 `apps/zcode-cli/packages/cli/dist/zcode.cjs`，
     且 README 标注 2026-10-19 更新到 v3.16.2、本机 APP 即 3.16.2.8769；
     但「同版本号不等于同构建」，无字节级构建对应证据，不升级为 PROVEN。
- **升级规则**（B0 简报第 1 条）：若本机安装版升级或官方发新提交，只记录新 fingerprint／新 SHA 行，
  本节的 3c5b2276 基线及其历史结论**不改写**。

---

## 2. 证据索引（claim → 支撑材料）

本轮**不重跑 P0 探测**；下表把决策文档后续各节将依赖的每一类论断钉到既有条目上。

| #   | 论断（桥接依赖）                                                                                                                                                                                                                       | 支撑材料与路径                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                         | 证据级别                                                    |
| --- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------- |
| E1  | 旧 v1 设计「协议未支持审批／问答」的结论**已被同提交实现反证**，不得再引用 v1 措辞当作当前事实                                                                                                                                         | `docs/superpowers/plans/2026-10-23-zcode-probe/2026-10-24-p0-review.md` §2.2（撤销定论及修正表）                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                       | SOURCE（固定 SHA 行号，见 E2）                              |
| E2  | 固定 SHA 上的审批／问答／stop 实现入口（桥接模块边界依据）                                                                                                                                                                             | 同 §2.2 表：`apps/zcode-cli/packages/bootstrap/src/zcode-protocol/server-operations.ts:3513`（注入 `createProtocolInteractionBroker`）、`interaction-broker.ts:105`（requestPermission 反向请求，含 requestId/sessionId/turnId/toolCallId）、`interaction-broker.ts:306`（interactionRequestUserInput）、`zcode-protocol-v4/interaction-registry.ts:1`（broker 与 v4 resolveInteraction 汇合）、`zcode-protocol-v4/commands/handlers/interaction-background.ts:45`（answer 消费；存在 full-access 特权分支，禁止探测误触）、`zcode-protocol/server.ts:677` 与 `server-operations.ts:2702`（session/stop 路由、`requireSession` 后中止 `record.activeAbortController`） | SOURCE                                                      |
| E3  | 上述方法在**本机安装版随包 CLI** 中存在（PACKAGE，不升级运行 PASS）                                                                                                                                                                    | P0 复核 §2.2 末段静态计数：interaction/requestPermission×1、interaction/requestUserInput×1、resolveInteraction×5、session/stop×4；本轮确认 zcode.cjs 哈希（§1.2）使该计数对应当前文件。注意：哈希↔静态计数的对应关系目前依托 P0 以来该文件的 (mtime,size) 同一性支撑（15,243,891 B / 2026-10-19 20:37），下次指纹复核须以内容哈希重算再确认                                                                                                                                                                                                                                                                                                                            | PACKAGE                                                     |
| E4  | Mac P0-R 已追出「APP host 独占本地 stdio；官方 Web 远控经账号→SID→短期 token→relay→RPC bridge 到 APP」的链路与 broker/server-operations 入口                                                                                           | `mac-agent-supplement-prompt.md`（R-01 定义）、`mac-agent-prompt.md`（P0-05 定义）；回报本体在私有目录 `~/Documents/tuvis-verify-logs/zcode/macos-P0R20261024T002051+0800-5n1lm/`（留机不入库）；仓库内摘要见 `2026-10-24-capability-status.md` §3「已有 APP 回合审批／问答／打断」行与 §4.5                                                                                                                                                                                                                                                                                                                                                                           | RELAY（私有原件）＋SOURCE                                   |
| E5  | 官方远控（网页面）的发送／审批／打断／切模型／链接复用有用户约两个月长期实测；本包**不重复证明**，对象是 Tuvis 独立客户端身份／连接／逐动作映射                                                                                        | `2026-10-24-capability-status.md` §7；设计 v3 §1「官方页面这些功能已有用户约两个月的真实使用证据」                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                     | 用户直接陈述（登记于 capability-status）                    |
| E6  | 读取断点根因（新旧 ID 双形态）与修复归属 A 包、不属桥接证据                                                                                                                                                                            | P0 复核 §2.1；设计 v3 §6                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                               | OBSERVED（两平台）                                          |
| E7  | 配置 URL 格式合同（A4，输入给 B）：`https`＋主机 `zcode.z.ai`＋`/remote/vN` 版本化路径＋非空唯一 `sid`（≤128 UTF-8 字节）、无 userinfo／非默认端口、≤4096、未知 query 保留；存储 key `remote.zcode_official_url`；`ConfigState=Missing | Saved                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                  | Invalid`，**Saved≠Connected**；完整链接不入日志／文档／夹具 | A 包计划 §A4（`docs/superpowers/plans/2026-10-24-remote-settings-and-zcode-read-recovery.md`）＋设计 v3 §4.3；实现 commit `ae3e3353 feat(zcode): persist and validate official remote configuration` | SOURCE（本仓库实现） |
| E8  | #117 最终 head 与其 provider env 处理结论（发送线历史素材的审计边界）                                                                                                                                                                  | `2026-10-24-p0-review.md` §2.3：head `289b20db4d6bdfba50ecfdeb67f1ef6f47ab583f`，gate.rs:130–147；旧合流点 `d5cc28b1` 结论作废                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                         | SOURCE                                                      |
| E9  | 「真实 `hi` 自动探针不进入本路线」                                                                                                                                                                                                     | `2026-10-24-capability-status.md` §4.3；工作包 B 计划全局约束「#117 hi gate 不加入本包」；设计 v3 §2                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                   | 决定（用户裁决）                                            |
| E10 | Windows 运行证据（E2 无头 marker、E4 取消）仅覆盖 Windows 特定构建＋provider 组合，不解锁 Mac、不解锁手机闭环                                                                                                                          | capability-status §1／§3 矩阵各行                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                      | OBSERVED（Windows 单机）                                    |
| E11 | 同 SID／role 单 owner、KICKED／冲突须停自动重连的约束                                                                                                                                                                                  | 设计 v3 §5.2；capability-status §6（mobile allowlist／设备列表／多 SID／每 SID-role 单 owner 的开源证据）                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                              | SOURCE＋待 B1 实连核验                                      |

---

## 3. 官方模块与许可证清单（桥接所需范围）

### 3.1 许可证

- 仓库 `zai-org/ZCode` @ 固定 SHA：根 `LICENSE` 为 **Apache License 2.0**（11,344 B，blob
  `550d8df4cfc74878663a511caa97852f15fd9592`；本轮经 GitHub API 取文核对首 200 行）。
- **无 `NOTICE` 文件**（本轮对该 SHA 查询返回 404）。Apache-2.0 复用义务相应简化为：保留 LICENSE 副本、
  对修改处加注明（state changes）、不得使用其商标背书衍生作品；无额外 NOTICE 传播义务。
- 随包 `ZCode.app/Contents/Resources/licenses/` 仅含第三方 `electron` 许可，与本清单的官方源码模块许可无关；
  `Contents/Resources/THIRD-PARTY-NOTICES.md` 为发行包第三方声明，仅在**再分发安装介质**场景相关，本项目不重新分发该包。
- 结论：**可以按 Apache-2.0 参考实现并选择性复用代码／类型**（引用而非复制整包是默认方式，见 3.3）；
  该许可不授予任何账号、token 或远端服务访问权——认证仍必须走 B1 定案的官方授权流程。

### 3.2 模块边界（相对固定 SHA `3c5b2276…`）

| 模块                                                                                                                      | 边界与桥接角色                                                                                                                                                                                                                                                                                | 现有证据                      |
| ------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------- |
| `apps/zcode-cli/packages/bootstrap/src/zcode-protocol/`（协议服务端面）                                                   | `server.ts`、`server-operations.ts`、`interaction-broker.ts`、`transport.ts`（传输层实现，B1 需通读双面）、`session-residency.ts`／`session-resident-pool.ts`（实例持有）、`official-mcp-auth-port.ts`（名称指向外部 MCP 认证端口，**是否涉及远控授权未验证，留给 B1，不预设为登录通路**）    | E2、E3                        |
| `apps/zcode-cli/packages/bootstrap/src/zcode-protocol-v4/`                                                                | `interaction-registry.ts`、`commands/handlers/interaction-background.ts`、`v4-gateway.ts`、`replay.ts`、`cold-session-resume.ts`（v4 应答／事件／恢复面；具体命令 wire 值必须来自实现＋真实脱敏帧）                                                                                           | E2                            |
| `apps/zcode-cli/packages/contracts/`                                                                                      | 跨端类型契约目录（`commands/`、`events/`、`interfaces/`、`network/` 等），B1 评估为 wire schema 的**参考源**；引用≠复制（见 3.3）                                                                                                                                                             | 本轮目录列表（GitHub API）    |
| `apps/zcode-cli/packages/cli/`（`src/resume.ts`、`src/protocol-lifecycle.ts`）、`packages/core/src/permission/service.ts` | P0 已读发送／恢复／权限面文档；CLI prompt 通道**没有**双向审批客户端，不能据此推导协议面能力                                                                                                                                                                                                  | P0 复核 §2.2 结论、手册 §2 表 |
| 官方 Web 远控前端与云 relay 服务                                                                                          | **不在本仓库**：固定 SHA 的 `apps/` 仅含 `zcode-cli` 与 `dev-docs`（本轮 API 列目录核实）。即 `zcode.z.ai` 远端与 relay 是托管服务，仓库只有其**对端实现**（协议服务端与随包 CLI）；B1 的 relay wire 证据只能来自客户端侧实现、授权流程返回与真实脱敏帧，**不得以“读官方服务端源码”补齐合同** | 本轮目录核实                  |

### 3.3 复用方式（登记；逐模块裁决留 B1）

1. **默认方式——协议参照实现最小客户端**：按固定 SHA 的 `zcode-protocol`／`zcode-protocol-v4`／`contracts`
   实现语义，在 Tuvis 侧写最小桥接客户端；不整体嵌入官网、不 clone 全仓进产品树、不引入官方 workspace 依赖。
2. **可选方式——类型契约引用**：仅当 `contracts` 中某 schema 与真实脱敏帧双向核对一致时，
   按 Apache-2.0 摘录最小类型并在文件头注明来源仓库、SHA、修改说明；每个被摘录文件在决策文档登记来源行号。
3. **禁止方式**：复制官方服务端模块进 Tuvis 冒充 host、把 `ZCode.app` 内打包产物（zcode.cjs）作为运行时依赖加载、
   以 DOM 自动点击或新起私有 app-server 附着原 APP（工作包 B 架构约束、D7）。
4. 本轮**下载范围**仅限许可证核验与目录清单（GitHub API 元数据 + LICENSE 文本），未 clone 仓库、
   未取任何实现文件入本地——实现文件与测试的按需下载在 B1 定案依赖时执行并逐文件登记许可证／复用方式。

---

## 4. 测试目标状态与等待项（B0 第 4 条）

- **等待项**：用户尚未为工作包 B 指定专用测试工作区／会话；本执行上下文中**未确认任何有效官方远控链接**
  （A 包 A4/A5 已交付配置入口，但 Saved≠Connected，且链接本体按合同不进入本文档、日志或夹具）。
- **用户需要的最小动作**（按阶段，不必一次做完）：
  1. （B1 只读连之前）在 ZCode APP 内开启移动远控→复制链接，经 Tuvis 设置页 ZCode 卡粘贴保存；
     官方登录／扫码／账号授权**由用户本人完成**，Agent 不读凭据文件、不要求粘贴 JWT／SID 原文。
  2. （任何真实写之前）指定一个专用测试工作区／会话（本机别名登记到本文档 §4 追加小节，
     真实路径与 session ID 只留私有目录映射），并**逐批授权**该会话上的动作（发送／审批／问答／打断／模型切换）。
- **因等待而未执行的 B0 子项**：「测试目标别名」尚无值可登记（不猜测、不占位）；
  「哪些动作已授权」清单为空。B0 其余子项（指纹、SHA 钉固、许可证／模块清单、证据索引）不依赖测试目标，已全部完成。
- 该等待**不阻塞**后续源码／fake 测试面（B1 红测、fake relay 探针可先行），但 B1 的实连步骤与
  B2—B4 的真实写实验在用户动作完成前一律不执行。

### 4.1 测试目标登记（2026-10-24 追加，B1 实连半场；别名制，真实 ID/路径只存私有映射）

| 项                    | 登记值（别名）                                                  | 说明                                                           |
| --------------------- | --------------------------------------------------------------- | -------------------------------------------------------------- |
| 测试 workspace 别名   | `Travel`                                                        | 用户指定的专用测试工作区；真实路径/ID 不入本文档，只存私有映射 |
| 测试 session 别名     | `hi`                                                            | 该 workspace 内的专用测试会话；真实 session ID 同上            |
| 已授权批次（batch 1） | 在会话 `hi` 中**发送消息**                                      | 唯一获批的真实写动作                                           |
| 未授权批次            | 审批 / 问答（自由作答与选项） / 打断（session/stop） / 模型切换 | 逐批授权制：未列即禁止，探针与后续批次不得触碰                 |

- 本登记只解除「测试目标未指定」的等待项（§4 第 2 条）；B1 实连仍受 §6.7 其余停等项
  （有效链接、凭据通路探测授权、同 role 互踢知情确认）约束。
- 别名 → 真实值的映射由用户保存在私有目录（不入仓库/日志/夹具）；后续批次引用一律用别名。

### 4.2 等待项收官登记（2026-10-24 追加，B1 实连半场；按 §6.7 编号逐项销账，不改写上文）

| §6.7 等待项                  | 状态                       | 日期与依据                                                                                                                                                                                                                                                    |
| ---------------------------- | -------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| ① 有效官方链接（阻塞实连）   | **DONE（2026-10-24）**     | 用户本人在 ZCode APP 开启移动远控并经 Tuvis A4 设置卡保存当前有效链接；探针自 `~/.tuvis/tuvis.db` 只读读取成功并解析 sid/mid（`linkSource=settings`，见 §6.9 run 引用）                                                                                       |
| ② 专用测试目标（阻塞真实写） | **部分完成（2026-10-24）** | 测试目标已指定并登记 §4.1（workspace `Travel` / session `hi`），batch 1 = 发送消息已授权；审批／问答／打断／切模型批次仍未授权，B2 起逐批申请                                                                                                                 |
| ③ 账号凭据通路探测授权       | **DONE（2026-10-24）**     | 用户在场完成浏览器 OAuth 通路探测：bigmodel 通道回环 redirect 被接受、交换端点签发有效账号 JWT → `auth.status=proven` 定案（详见 §6.9 第 1 条）                                                                                                               |
| ④ 同 role 互踢知情确认       | **已消解（2026-10-24）**   | 实连证实官方流程**不存在 APP 侧配对确认**（waiting = 路由不可达/设备离线态，§6.9 第 3/4 条）；互踢语义维持原判——同一链接第二条 terminal 连接会把第一条踢成 KICKED（官方源码注释 + fake 锁形维持），探针 KICKED 即停口径不变；实连时段用户知情且未依赖网页会话 |

## 5. 纪律声明（B 包约束登记）

- 旧 v1 设计文档中「交互（审批／问答）为未来工作」的措辞**已被同固定提交的实现反证**（E1／E2），
  任何后续引用把其当作当前事实都是错误。
- #117 的真实 `hi` 自动探针**不在本路线**（E9）：运行时只做只读预检，真实回合实验仅在获授权的专用测试会话内做。
- 全局约束重申：官方登录／扫码／配对归用户本人；不读其他应用凭据文件、Cookie DB、进程内存或日志取 token；
  不提交真实 SID／JWT／token／会话正文；不修改 `docs/MASTER-PLAN.md`；本包不编辑产品运行链；
  本轮及本文件生成过程中未调用任何官方远端服务（仅 GitHub API 公开仓库元数据）。
- 后续追加区（B1：identity/auth/wire 实况；B2—B4：各自 operations／fixtures）在此线以下按轮次追加，不改写上节记录。

---

# 6. B1 部分（2026-10-24 追加）：官方身份与最小认证合同（source + fake 阶段）

> 本节只做两件事：①从固定官方源码核对「官方允许的终端登录／授权流」，逐条给出行号证据；
> ②用 in-test fake relay 锁死 Tuvis 客户端侧行为（红测→绿测）。**本轮未连接任何真实远端**；
> 实连半场被 §6.7 停等项阻塞。所有凭据/标识均为合成值，不含真实 SID/JWT/token/链接。

## 6.0 执行方式声明

- GitHub 只读取文（`gh api contents` at 固定 SHA `3c5b2276…`）共 15 个实现文件 + 树清单；未 clone 仓库。
- 本机未启动 ZCode APP／CLI／Tuvis；未执行任何真实网络连接（除 GitHub API 公开端点）。
- 测试执行：`pnpm exec vitest run tests/remote/zcodeBridgeRelay.test.ts`（RED 与 GREEN 证据见 §6.5）。
- 新增文件：`tests/remote/zcodeBridgeRelay.test.ts`、`scripts/zcode-bridge-probe/relayAuth.ts`、
  `scripts/zcode-bridge-probe/auth-check.ts`（骨架，拒绝连接）、`tests/fixtures/zcode-official-bridge/contract.json`。
  探针目录不挂应用启动/扫描（工作包 B「固定产物与接口」）。

## 6.1 对 B0 §3.2 模块清单的勘误补充（追加，不改写 B0 原文）

B0 §3.2 末行「官方 Web 远控前端与云 relay 服务**不在本仓库**」的 **apps/ 目录陈述为真**
（固定 SHA 的 `apps/` 确实只有 `zcode-cli` 与 `dev-docs`），但仓库级判断不完整。本轮树清查
（`git trees?recursive=1` @ 同 SHA）证实以下三组**客户端侧实现就在本仓库**：

| 目录                                                 | 角色                                                                       | B1 用途                           |
| ---------------------------------------------------- | -------------------------------------------------------------------------- | --------------------------------- |
| `packages/shared/src/web-remote-relay/`              | relay V2 wire 契约（zod schema：HTTP 路径表、WS 消息、token 形状、错误码） | 本轮 wire 引用的第一权威          |
| `packages/web/src/web-remote-control/`（含 `auth/`） | 官方网页远控前端（终端角色）+ 其 OAuth 登录模块                            | 终端授权流与账号 JWT 来源的证据源 |
| `packages/desktop/src/main/web-remote-relay/`        | 官方桌面端（设备角色）relay 客户端                                         | 设备注册/注销与凭据持久化对照面   |

**仍然不在仓库**：云 relay/hub 服务端本体与 `zcode.z.ai` 站点部署配置。因此 B0 的结论
「relay wire 证据只能来自客户端侧实现 + 授权返回 + 真实脱敏帧」**维持成立**，且本轮起
客户端侧证据已实际在手。

## 6.2 官方允许的终端登录／授权流（逐条行号证据，文件均 @ `3c5b2276…`）

缩写：`shared/contract.ts` = `packages/shared/src/web-remote-relay/contract.ts`；
`web/transport.ts` = `packages/web/src/web-remote-control/adapters/webRemoteControlTransport.ts`；
`web/v2token.ts` = `packages/web/src/web-remote-control/adapters/webRemoteControlV2Token.ts`；
`entry.ts` = `packages/web/src/web-remote-control/app/webRemoteControlEntry.ts`。

**A. 账号身份（OAuth → ZCode JWT）**

1. 授权入口（ZAI 通道）：`{chat.z.ai}/api/oauth/authorize?redirect_uri=<回调>&response_type=code&client_id=<公开client_id>&state=<state>`
   （`packages/web/src/auth/zaiWebOAuthProvider.ts:184-191`；默认 origin `https://chat.z.ai`，
   `webZaiOAuthConfig.ts:30`）。BigModel 通道变体：`{bigmodel}/login?redirect=<回调>&appId=zcode&state=<state>`
   （`zaiWebOAuthProvider.ts:175-181`；`webZaiOAuthConfig.ts:40-43,60-61`，appId 默认 `zcode` 即「桌面端在用的」）。
2. `client_id` 默认 `client_P8X5CMWmlaRO9gyO-KSqtg`，源码注释明示「client_id 会出现在授权 URL 中，
   属于公开配置」（`webZaiOAuthConfig.ts:57-58`）——它是**官方网页 app 的第一方公开客户端**，
   不是第三方客户端发放通道。
3. 回调 redirect_uri **钉死**官方网页自身：`webRemoteCallbackUrl = {zcodeOrigin}/remote/v4`
   （`webZaiOAuthConfig.ts:62` + `packages/shared/src/zcodeEndpoint.ts:378`；callback 判定
   `/remote/v4` + `state` + `code|authCode|error`，`entry.ts:17-26`）。exchange 时回传同一
   redirect_uri（`zaiWebOAuthProvider.ts:225`）。dev 环境另有 `VITE_WEB_REMOTE_ALLOW_DEV_RETURN_TO`
   门控的 devOrigin 回调（`webZaiOAuthConfig.ts:64-65,74-81`）——构建期开关，不是产品第三方通路。
4. token 交换：`POST {zcodeOrigin}/api/v1/oauth/token`，body `{provider, code, redirect_uri, state}`
   → `{code:0, data:{token:<ZCode账号JWT>, zai|bigmodel:{access_token}, expires_in, user}}`
   （`zaiWebOAuthProvider.ts:211-239`；`data.token` 即 `zcodeJwtToken`，121-124,151-156）。
5. JWT 持久化在网页 localStorage，读用时校验过期，**过期即 clearAll 并要求重新 OAuth**；
   logout = `clearAll()`（`packages/web/src/auth/browserOAuthCredentialRepo.ts:145-179`；
   `webAuthService.ts:205-207`；`entry.ts:250-255`）。**没有 refresh token 流**——「刷新」即重新走 OAuth。

**B. 终端角色接入（relay token → WS 握手）**

6. 终端一次性 token：`POST /api/v1/remote/token`，`Authorization: Bearer <ZCode JWT>`，
   body `{role:"terminal"}`（路径表 `shared/contract.ts:96-101`；请求形状 `web/v2token.ts:13-22`）。
   响应 `{code:0, msg, data:{token, expires_in}}`；token = 16 字节无填充 Base64URL 的 22 字符、
   末字符∈`[AQgw]`（schema `shared/contract.ts:119-123`，注释明示 16 字节语义）。
7. **一次性、不缓存**：「一次性 token 不缓存；401 交给入口恢复登录」（`web/v2token.ts:8`）；
   终端传输**每次连接尝试都现取新 token**（`web/transport.ts:104`）；401 → `auth-required`
   终态交上层回登录（`web/v2token.ts:23`；`entry.ts:236`）。→「重连需新 token」由实现结构保证。
8. WS 连接：`{relayWsUrl}` = `wss://<relay>/api/v1/remote/ws2`（`zcodeEndpoint.ts:380`；路径常量
   `shared/contract.ts:102`），URL 带 `?token=<一次性token>`（可选 `mid`）（`web/transport.ts:106-110`）。
9. 握手：open 后发 `auth_init{device_sid, meta:{platform:"web",version,name:"mobile-browser"}, client_ts}`
   （`shared/contract.ts:19-24`；`web/transport.ts:119-143`），服务端回
   `auth_ack{device_sid, terminal_sid?, pair_status: waiting|matched, server_ts?}`
   （`shared/contract.ts:53-59`）；客户端要求 `device_sid` 回显匹配才认账（`web/transport.ts:318`）。
   `waiting` 为合法配对前态（心跳 `pair_status_query` 维持，30s 未 matched → desktop-disconnected 终态，
   `web/transport.ts:460-473`）。
10. 数据面门禁：`data` 帧**仅 authenticated 且 paired** 才处理（`web/transport.ts:329-331`）；
    发送侧未 paired 直接返回 unavailable——不发送、不排队（`web/transport.ts:216-222`）。
    出站/入站 data 形状 `{type:"data", payload, client_ts?|server_ts?}`（`shared/contract.ts:36-44,66-70`）。
11. 错误帧 `error{code: AUTH_FAILED|WRONG_PARAM|KICKED|INTERNAL|DEVICE_OFFLINE, message?, server_ts?}`
    （`shared/contract.ts:18,71-76`）。终端侧处置：`KICKED` → `kickedDelay()`：60 秒窗口内已有
    一次 KICK 则返回 null → **终态失败 session-conflict（state=kicked，停止）**，否则 2–5 秒后
    重连一次（`domain/retry.ts:26-30`；`web/transport.ts:389-394`）；`AUTH_FAILED` → 终态
    `device-not-owned`、`WRONG_PARAM` → 终态 `invalid-mobile-connection`（`web/transport.ts:402-407`）。
12. **单 owner 的直接实现证据**：官方生命周期注释「同一个远控链接建立第二条 terminal socket
    并被 relay 踢成 KICKED」（`packages/web/src/web-remote-control/app/webRemoteControlLifecycle.ts:29-33`）
    ——同链接（同 device_sid/role）的第二个终端连接会踢掉第一个；终端重试参数
    initial 500ms / max 10s（`web/transport.ts:45-49`），设备侧 initial 1s / max 30s
    （`domain/retry.ts:9-12` + 桌面 `packages/desktop/src/main/web-remote-relay/webRemoteControlTransport.ts:51`）。

**C. 对照面（设备角色与注销）**

13. 设备注册：`POST /api/v1/remote/device {device_mid}` → `device_sid`；注销：
    `POST /api/v1/remote/device/delete {device_sid}`；列表 `GET /api/v1/remote/device`
    （`shared/contract.ts:96-101,105-118`；`packages/desktop/src/main/web-remote-relay/webRemoteControlV2Http.ts:47-61`）。
14. 设备侧凭据持久化 = 桌面 App settings 的 `webRemoteControlRelayV2Device` 字段
    （`packages/desktop/src/main/web-remote-relay/webRemoteControlDeviceStorage.ts:11-24`）——
    官方桌面端同样**没有** OS keychain，注册记录（非凭据）落 settings；legacy 凭据有显式清除迁移。
15. 链接形状：`{zcodeOrigin}/remote/vN?sid=<device_sid>&mid=<device_mid>&name=<name>&app_version=<v>`
    （构造 `packages/shared/src/web-remote-control.ts:163-189`；解析 191-207：**带 legacy `hash`/`t`
    参数直接判 null 拒绝**；选择设备后 URL 才带 sid/mid，`entry.ts:186-199`）。与 A4 合同
    （E7：`https` + `zcode.z.ai` + `/remote/vN` + 非空唯一 `sid`）一致，且补充：vN 当前取
    v3/v4（按 appVersion 门控，`zcodeEndpoint.ts:371-377`）。

## 6.3 第三方可复用授权路径核查（B1 关键问题）

**结论：本仓库源码没有展示任何第三方可复用的授权返回通路。** 证据：

- 唯一的账号凭据获取流是官方网页自己的 OAuth：client_id 为第一方公开客户端（§6.2-2），
  redirect_uri 钉死官方 `/remote/v4` 回调（§6.2-3），exchange 回传同一 redirect_uri；
  仓库内没有第三方 client 注册、动态 redirect、PAT/设备码（device flow）或任何
  「把账号凭据交给非官方客户端」的机制（全仓树清查 + `packages/web/src/auth/` 全量阅读）。
- 设备角色（桌面端）的注册凭据是桌面 App 自己的账号 JWT，属**其他应用凭据**，按工作包 B
  约束禁止读取。
- 因此：`auth.status = interim`（不是 proven，也**尚不能**判 unsupported——缺失的不是流程知识，
  而是「官方授权服务器是否接受 Tuvis 控制的 redirect_uri」这一服务器侧行为，仓库无法证明或证伪）。
  **禁止**以习惯假设官方支持任意第三方 OAuth client；该问题只能由 §6.7-③ 的用户在场浏览器
  探测或官方渠道问答定案。

## 6.4 contract.json（auth/route 填写与证据类别）

- 文件：`tests/fixtures/zcode-official-bridge/contract.json`（schemaVersion=1，本轮新建）。
- `auth.status = "interim"`（计划枚举 proven|unsupported 之外的显式过渡值，附
  `interimMissing` 说明缺失项）；`flowName`/`tokenSourceKind`/`credentialPersistence`
  全部来自 §6.2 的 source 证据，`evidence[]` 逐条标 `class: source | fake`。
- `route.role = "terminal"`；`ownerScope` = 同 device_sid 单 owner、KICKED 冲突、探针即停口径；
  `workspaceBindingFields`/`sessionBindingFields` = 空数组 + `bindingNote`（relay 层只有
  `device_sid` 绑定；workspace/session 绑定属 RPC 层，**留 B2 真实帧取证，不猜测字段**）。
- `capabilities` 全 false（未实连，无真实帧）；`operations` 空数组（同因）。

## 6.5 fake relay 红测证据（tests/remote/zcodeBridgeRelay.test.ts）

- 文件首行 `// @vitest-environment node`（仓库既有约定，同 `tests/lib/errorCodesGate.test.ts:1`）；
  未新增任何依赖、未引入 tsx；WS 客户端用 Node ≥22 全局 `WebSocket`，fake 服务端为本文件内
  ~120 行的最小 RFC6455 实现（`node:http` upgrade + SHA1 accept + masked 帧解析）。
- **RED**（先写测试、`scripts/zcode-bridge-probe/relayAuth.ts` 尚不存在）：
  `pnpm exec vitest run tests/remote/zcodeBridgeRelay.test.ts` →
  `Error: Cannot find module '../../scripts/zcode-bridge-probe/relayAuth'`（1 failed suite）。
- **GREEN**（实现 `relayAuth.ts` 后同命令）：**5 passed / 0 failed**，无 MSW 干预
  （文件级 `beforeAll` 关闭全局 MSW，仅本文件作用域）：
  1. 未认证（错/空凭据）申请 token → 401×2、`tokenMinted=0`（没有任何可消费 token 产出）；
     裸 WS 无 token：`auth_init` 即 `error{AUTH_FAILED}`，其后 data 帧被丢弃不计数（`dataAccepted=0`）。
  2. 合成合法凭据：`role:"terminal"` 申请成功、token 合官方形状、`expires_in>0`；两次申请
     token 不同（一次性）；WS `auth_init→auth_ack(matched)` → paired；paired 后 data 被接受。
  3. 过期 token（60ms TTL）连接 → `rejected`；已消费 token 重连 → `rejected`；两者
     `sendData` 均 false（认证失败不发后续帧），`wsAuthFailed` 计数精确 +1/+1。
  4. SID 形态字符串冒充 Bearer → 401；冒充 WS token → `rejected`；`dataAccepted` 恒 0。
  5. KICKED 注入 → 客户端终态 `kicked`、不再发送；等待 100ms 后 `wsUpgrades` 仍为 1
     （**结构性无重连**：客户端没有重连代码路径，`relayAuth.ts` 无任何计时器）。
- 被测客户端 = `scripts/zcode-bridge-probe/relayAuth.ts`（Tuvis 参照实现）：单连接、无自动重连、
  token 一次性不落盘、未 paired 不发送、KICKED/拒绝即终态；每个常量与分支注释都标了
  官方源码行号。fake 侧「FAKE 策略」注释明确区分了本测试自定行为（如升级必成、auth_init 后
  才校验 token、未认证 data 丢弃并断开）与官方取证结论。

## 6.6 实连半场探针骨架（scripts/zcode-bridge-probe/auth-check.ts）

- 骨架仅声明 6 步（取链接→账号凭据通路定案→申请 terminal token→单连接握手→只读观察→
  断开落证）与硬安全闸（单连接、`reconnectPolicy:"never"`、KICKED 即停、只读白名单、
  60s 硬超时、凭据只入内存）。`main()` 在解锁旗标缺席时**一律抛错拒绝执行**；
  本轮不连接任何远端；不挂产品启动/扫描。

## 6.7 停等项（用户动作；全部完成前不执行任何真实连接/写实验）

1. **有效官方链接（阻塞实连）**：现存的两个月前链接已显示 Not Found，**禁止复用**。
   请用户本人在 ZCode APP 内开启移动远控→复制**当前有效**链接→经 Tuvis 设置页 ZCode 卡
   （A4）粘贴保存；链接本体按合同不入本文档/日志/夹具。
2. **专用测试目标（阻塞任何真实写实验，B2 前置）**：指定专用测试工作区/会话，并在本文档
   §4 追加小节登记别名与逐批授权动作清单（真实路径/ID 只留私有目录映射）。
3. **账号凭据通路探测授权（决定 interim→proven/unsupported，实连半场第一闸）**：
   官方终端 token 需要**账号 JWT**（§6.2-6），而 JWT 的官方获取流是浏览器 OAuth（§6.2-1—5），
   其 redirect_uri 钉死官方回调、仓库未见第三方通路（§6.3）。需要用户**在场**二选一：
   (a) 授权做一次浏览器 OAuth 通路探测（用户本人操作浏览器完成官方授权，验证官方授权服务器
   是否接受 Tuvis 本地回环 redirect——不接受即 `auth.status=unsupported` 定案）；
   (b) 或直接裁决走官方渠道询问/其他合法备选。Agent 不代登、不读浏览器/其他应用凭据、
   不要求用户粘贴 JWT 原文。
4. **同 role 互踢知情确认（实连前）**：单 owner 语义（§6.2-12）意味着探针连接可能踢掉用户
   正在使用的官方网页会话；实连时段需用户确认不依赖网页会话，且探针 KICKED 即停不互踢。

## 6.8 本轮遗留与风险登记

- `expires_in` 的真实时长是服务器侧值，仓库不可见；「token 短期性」只能实连观测（§6.7 全部
  完成后于实连半场验证）。
- 官方服务端对「未认证 data 帧」的处置未取证（fake 取保守丢弃+断开策略并标注 FAKE 策略）；
  Tuvis 客户端行为不受影响（发送侧门禁使未 paired 从不发出 data）。
- `terminal_sid`（auth_ack 可选字段）的确切语义/稳定性待实连帧确认，暂不入合同字段。
- B1 实连半场的验收门维持计划原文：「proven 且实际只读连到正确测试 APP」才进 B2。

## 6.9 B1 实连判定（live connection verdict，2026-10-24 收官追加）

> 任务书原称「§6.4 live connection verdict」；因 §6.4 已被「contract.json 填写与证据类别」
> 占用，按「追加不改写」纪律落为本节 §6.9。B1 live 验收门判定：**PASSED**（proven 且实际
> 只读连到正确测试 APP，controller 记录）。私有脱敏证据目录
> `~/Documents/tuvis-verify-logs/zcode/`（0700，不入仓库）；本节只引用 run id，无任何真实值。
> 证据级别标注：**live-sanitized** = 实连脱敏私有证据；**user-testimony** = 用户直接陈述；
> **source** = 固定 SHA 官方源码行号。

实连 run 索引（私有，只读引用）：

| run id                            | oauth                           | connect              | 说明                                                                       |
| --------------------------------- | ------------------------------- | -------------------- | -------------------------------------------------------------------------- |
| `macos-B120261024T102434Z-SbxvWA` | UNSUPPORTED（callback-timeout） | SKIPPED              | zai 通道——用户账号在 BigModel 侧，通道不符；inconclusive，**非**服务器拒绝 |
| `macos-B120261024T104607Z-iykCdg` | PROVEN（bigmodel）              | STOPPED:pair-timeout | 探针缺 mid → pair_status 恒 waiting → 30s 超时                             |
| `macos-B120261024T104720Z-OMksKw` | PROVEN（bigmodel）              | STOPPED:pair-timeout | 同上（复跑）                                                               |
| `macos-B120261024T105940Z-NarmSw` | PROVEN（bigmodel）              | **COMPLETED**        | mid 修复后收官 run：paired + 两只读查询 ok                                 |

六项判定（逐条挂证据级别）：

1. **auth.status：interim → proven。**【live-sanitized】官方授权服务器（bigmodel.cn 通道，
   国内智谱账号）接受 Tuvis 本地回环 redirect_uri（127.0.0.1 随机端口回调），共用交换端点
   `POST /api/v1/oauth/token` 签发有效账号 JWT。三次实连 OAuth 事件：①zai 通道
   callback-timeout（用户账号在 BigModel 侧、通道不符，inconclusive——**不是**服务器拒绝）
   【live-sanitized】；②bigmodel PROVEN【live-sanitized】；③bigmodel PROVEN（mid 修复后复跑）
   【live-sanitized】。
2. **终端 token 实测。**【live-sanitized】`POST /api/v1/remote/token` Bearer JWT
   `{role:"terminal"}` → 22 字符 token、`expires_in=60s`；同一 run 两次申请值不同（一次性由
   source+fake 约束，未做重放拒绝）、每次连接现取新 token；两条独立 run 复现。
3. **mid 绑定发现。**【live-sanitized + source】relay 以 ws2 升级 URL 的 `mid` 查询参数把终端
   路由到目标设备：官方 web 终端在链接 mid 非空时**条件性**附加（`webRemoteControlTransport.ts:107-110`）
   【source】；APP（设备角色）**恒发**自身 mid（desktop `webRemoteControlTransport.ts:117-119`）
   【source】。探针无 mid → pair_status 恒 waiting → 30s pair-timeout（run `…iykCdg` /
   `…OMksKw`）；带 mid → **auth 后约 131ms 即 matched**——全程无 APP 侧人工介入
   【live-sanitized】。
4. **用户观察证据。**【user-testimony】用户陈述：任何浏览器打开官方远控链接只出现浏览器
   登录/授权一步，ZCode APP 从不弹出任何配对确认——与探针自动 matched 一致。结论：官方流程
   **不存在 APP 侧配对确认步骤**；`waiting` = 终端不可路由/设备离线态，**不是**「等待用户
   批准」。早前合同里「官方 30s 配对窗口（等待用户）」的假设就此勘正为「**30s 路由窗口**」
   （探针 `auth-check.ts` 的等待提示文案已随本批次修正）。
5. **只读连接实测。**【live-sanitized】单条 WS：`auth_init{device_sid,meta,client_ts}` →
   `auth_ack` → matched；白名单两个只读 RPC 查询 `bootstrap-request` /
   `workspace-list-request` 均返回 ok；探针侧主动 close 收尾；无重连路径（结构性与 fake 锁形
   维持）。脱敏证据在私有 run 目录（按 run id 引用，无真实值）。
6. **route 面定案。** role=terminal 实连成立【live-sanitized】；relay 层绑定字段 =
   **mid（设备路由）+ device_sid（auth_init 回显校验）**【live-sanitized + source】；
   workspace/session 绑定仍在 relay 之上的 RPC 层——留 B2 以真实帧取证，不猜测。

合同落点：`tests/fixtures/zcode-official-bridge/contract.json` 的 `auth.status=proven`、
`interimMissing` 改记解除过程、`auth.evidence`/`route.evidence` 追加 live 条目、
route `ownerScope`/`bindingNote` 勘正如上；`capabilities` 维持全 false、`operations` 维持
空数组（send 等属 B2+，未取得各自真实帧证据不得前移）。**历史状态；最终能力状态以 §8.10 与
contract.json 为准。**

---

# 7. B2 部分（2026-10-24 追加）：单连接、会话映射与单次发送的准备定案

> 本节登记 B2 的**准备阶段**产物：发送链 wire 定案（source 级，逐条行号）、会话映射方案、
> 合成 marker 设计与 fake 合同。**send 能力未前移**：`contract.capabilities.send` 维持 false，
> 须待 B2 实连半场（controller 与用户在场按批准次数执行）取得受理/归属/结果证据后再裁。
> **历史状态；send 已由 §7.9 前移为 true。**
> 本节不含任何真实会话 id / 路径 / SID / token；私有证据目录
> `~/Documents/tuvis-verify-logs/zcode/macos-B2<UTC>-<短码>/`（0700/0600，脱敏 fail-closed）。

## 7.1 新增资产

| 文件                                          | 职责                                                                                                                                                                                                             |
| --------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `scripts/zcode-bridge-probe/sessionBridge.ts` | Tuvis 侧参照实现：二进制 Channel 序列化、CRC32、rpc-frame 编解码/组装器、SingleConnectionGate、SessionTargetMapper（精确归属+代际绑定）、SendOnceLoop（单帧发送闭环）。不在产品运行链上。                        |
| `scripts/zcode-bridge-probe/session-probe.ts` | B2 实连半场探针（OAuth → token → ws2+mid 单连接 → 只读映射 → designate → 受闸单次发送 → 脱敏落证）。`--i-know-this-connects` 解锁连接；`--send-authorized` 单独解锁发送（缺省 map-only，打印 WOULD-send 即停）。 |
| `tests/remote/zcodeBridgeSession.test.ts`     | B2 fake 合同六用例（见 §7.4）。                                                                                                                                                                                  |
| `scripts/zcode-bridge-probe/sanitize.ts`      | `createEvidenceRunDir` 增加 batchPrefix（B2 运行目录 `macos-B2…`）。                                                                                                                                             |

## 7.2 发送链 wire 定案（source 级；全部行号 @ 固定 SHA `3c5b2276…`）

发送一条消息的完整链（官方 web 远控客户端同款）：

1. **app payload**：`workspace-bridge-open{requestId, bridgeSessionId, workspaceKey,
taskId}`（形状 `packages/shared/src/web-remote-control.ts:435-447`）→ 宿主
   workspace-bridge-ready（`bridgeSessionId`/`bridgeGeneration` 回显，同文件 :449-456）。
   workspaceKey 口径 = `resolveWebRemoteControlWorkspaceKey` =
   `workspaceIdentity?.trim() || workspacePath`（同文件 :743-747）。
2. **rpc-frame 通道握手**（Channel = `"zcode-agent"`，`packages/shared/src/channels.ts:114`；
   顺序钉 `packages/ui/src/v4/agentV4ConnectionHandshake.ts:30-46`）：
   `helloConversationV4()` → `HelloMessage{kind:"hello", protocolVersion, connectionId,
clientMode:"desktop-continuous"|"web-remote-replayable", deliveryProfile, serverTime,
capabilities, auth}`（`packages/shared/src/zcode-protocol-v4/transport.ts:42-65`）→
   `initializeConversationV4(clientHello)`，clientHello =
   `{kind:"clientHello", protocolVersion:3, clientId, clientKind:"desktop"|"web",
appVersion, capabilities?}`（同文件 :69-92，capabilities 可选；clientKind 映射见
   handshake 文件 :43）。
3. **订阅**（仅为打开活动帧证据流；官方发送本身只依赖握手）：
   `subscribeConversationV4{workspacePath, workspaceIdentity?, sessionId}`——
   参数面钉 `packages/ui/src/v4/agentConversationTransport.ts:207-214`（`{...workspace,
sessionId, base?, visibility?, retryInitialization?}`；workspace = `{workspacePath,
workspaceIdentity?}` 同文件 :94-97）+ `packages/services/src/zcode-agent/zcodeAgent.ts:400-405`
   （`ZCodeAgentConversationSubscribeParams extends ZCodeAgentSessionTarget`）；
   sessionId = `"conversation/<sessionId>"` topic 的精确后缀
   （`packages/shared/src/zcode-protocol-v4/transport.ts:113,205`）。
   **勘误记录**：准备稿曾按 `{topic, connectionId, clientMode, workspace:{…}}` 发订阅——
   该形状在固定源码中不存在，已按上行钉源修正（无 guess 留存）。
4. **发送**：`sendConversationCommandV4`（`packages/services/src/zcode-agent/zcodeAgent.ts:937`），
   args = `{workspacePath, workspaceIdentity?, projectId?, envelope}`
   （`ZCodeAgentConversationCommandParams` 同文件 :498-505；workspace target
   `zcodeAgentPluginParams.ts:9-14`；官方 UI 调用面
   `agentConversationTransport.ts:354-372`）。envelope =
   `{commandId, clientId, sessionId, baseRevision?, baseLogEpoch?, type, payload, issuedAt}`
   （`packages/shared/src/zcode-protocol-v4/command.ts:343-356`）；sendText payload 只**必带**
   `text`，model/mode/plan 全部 optional=缺省沿用会话现值（同文件 :95-135）；
   sendText **不在** `COMMANDS_REQUIRING_BASE_REVISION`（同文件 :315-331）→ 免 baseRevision。
5. **承载**：Channel 消息字节 → 单分片 rpc-frame（CRC32）→ relay data 信封
   `{type:"data", payload, client_ts}`（`packages/shared/src/web-remote-control-rpc-transport.ts:60-107,133-143`；
   请求字节 wire = `serialize([100, id, channel, method]) + serialize([args])`
   `packages/rpc/src/channelClient.ts:182-190` + `serialization.ts:169-175`；
   类型码 100/201/202/204 `packages/rpc/src/channels.shared.ts:13-23`）。

## 7.3 会话映射方案（精确归属，绝不按标题发送）

- **listing 双源**：workspace 条目来自 bootstrap/workspace-list response 的
  `WebRemoteControlWorkspaceTarget{workspacePath, workspaceIdentity?, label, kind,…}`
  （`web-remote-control.ts:628-635`，B1 实连已验）；task 行来自宿主推送
  `controller/tasks-index` 帧的 `WindowHostControllerTaskRow{address:{workspacePath, taskId},
meta:{title,…}}`（`packages/shared/src/zcode-protocol-v4/controller.ts:17-35,93-123`；
  B1 实连观察到同形状增量推送）。
- **designate**：操作员指称（`--workspace` 默认 Travel、`--task-title` 默认 hi）只用于
  在 listing 里**精确唯一**匹配出真实 id；不唯一/不命中一律中止。**发送授权只认
  `resolveTarget` 产物**（taskId 在当前代际 listing 里精确存在 + workspacePath 严格相等）；
  本参照实现没有任何按标题/cwd 查找的 API（测试以反射断言缺席）。
- **代际绑定**：映射绑定解析时的连接代际；`authorizeSend(target, liveGeneration)` 拒绝
  旧代际映射（stale-epoch），SendOnceLoop 构造即二次校验。探针单连接无重连，代际恒 1。
- **能力边界（计划 §B2 第 4 条）**：绑定证据不充分的卡（taskId/workspaces 对不上唯一行）
  只读不发送——mapper 的 ambiguous/not-in-listing 拒绝即该边界的强制点。

## 7.4 fake 合同（tests/remote/zcodeBridgeSession.test.ts，RED→GREEN）

六用例（fake relay 127.0.0.1，无真实服务；合成 id `b2-fake-*` / `fake-bridge-*`）：

1. `single_upstream_connection`——已有 paired 连接时第二个请求本地即拒（factory 不调用、
   relay wsUpgrades 保持 1）；
2. `wrong_target_rejected`——未知/错 taskId、错 workspacePath、以标题当 id 全部客户端侧拒绝，
   零 data 帧发出；按标题查找 API 反射断言不存在；
3. `unknown_not_resent`——peer 受理（rpc-frame-ack）后丢响应 → 结果只记 unknown，
   恰好 1 个发送帧，多等一拍仍无重发（结构性无重发计时器）；
4. `duplicate_events_deduped`——重复 ack×3 + 重复应答×2 + 重复事件×2 全部去重，
   发送帧恒 1，结果 completed；
5. `stale_epoch_rejected`——旧代际映射在新代际上 authorizeSend 即抛、构造 SendOnceLoop 即抛，
   零 data 帧；新代际必须用新 listing 重新绑定；
6. wire 形状自检——CRC32 往返、rpc-frame 字段、Channel 请求头数组序列化。

## 7.5 合成 marker 设计

正文 = `[tuvis-b2-marker:<UTC ISO 时间戳>]`（唯一、合成、不含任何秘密）；一次运行
**至多一条**（`--send-authorized` 显式解锁；SendOnceLoop 结构性恒 1 帧）；模型/思考强度/
模式全缺省=沿用会话现值；不审批、不打断。结果三态：completed（受理+窗口内观察到该会话
活动帧，去重后）/ accepted（受理但活动窗静默）/ unknown（应答超窗——**绝不重发**）。
验证核对面（官方 snapshot 的 user 消息与 turn 关联、只读 DB 最终可见性、兄弟会话不串投）
留实连半场按批准次数执行后入账。

## 7.6 contract.json 落点（B2 准备阶段）

`operations` 追加且仅追加两条 live 已验**只读**操作（`bootstrap-request` /
`workspace-list-request`，B1 run `macos-B120261024T105940Z-NarmSw`）；`capabilities.send`
维持 false——实发取证前不得前移；schema 其余不动。**历史状态；send 已由 §7.9 前移为 true。**

## 7.7 追加与勘误（2026-10-24，B2 发送前评审修复；不改写上文）

- **live-verification 显式项（新增）**：`envelope.sessionId ≡ listing address.taskId`
  **同命名空间**是发送结果可解释的前提——`sendConversationCommandV4` envelope 携带的
  sessionId 必须与 tasks listing `address.taskId` 属同一 id 空间；实连半场须把该等同性
  列为显式核验项（拿 marker 落点回对 listing 行归因）。判错即 fail-closed（不发送／
  结果作废），但 COMPLETED/ACCEPTED/UNKNOWN 的 verdict 解读依赖它成立。
- **勘误（§7.3「SendOnceLoop 构造即二次校验」与 §7.4 用例 5 同款措辞）**：构造**不再**
  比较 `identity.bridgeGeneration` vs `target.generation`——bridgeGeneration 是宿主
  workspace-bridge-ready 回显的 **bridge 通道代际**，与探针连接代际不同命名空间，拿两者
  比较属类别错误（宿主回显 ≠1 时构造曾在 try/catch 之外抛出，探针裸 crash 丢证据）。
  stale-epoch 闸唯一住在 `SessionTargetMapper.authorizeSend`（映射代际 vs 存活连接代际，
  like-for-like）；bridgeGeneration 仅信息性（随帧回传 + 落证据，不作闸）。配套加固：
  session-probe 主体加故障围栏——任何阶段未处理异常一律记 `STOPPED:fault-<阶段>`
  verdict 并照常落证，绝不裸 crash。

## 7.8 追加：B2 实连 map-only 修复（2026-10-24；不改写上文）

实连 run `macos-B220261024T121929Z-SnWWVA`（map-only，`--send-authorized` 已挂但未走到）：
bootstrap-response / workspace-list-response 均 OK，28 条 workspace 条目；
designate 在 workspace 阶段 `workspace-ambiguous` fail-closed（**未发送任何写向**）；
被动 8s 收帧窗 0 条 rpc-frame / 0 条 bridge 消息（taskRows=0）。归因与修复：

- **双 listing 重复（根因一）**：bootstrap-response 与 workspace-list-response 携带
  **同一份** `workspaces[]`（实连 14 条 ×2 = 28 条原样合并），任何 label 的精确匹配都
  至少翻倍、「精确唯一」永假。修复 = `dedupeWorkspaceEntries` 按全部采集字段精确去重
  （sessionBridge.ts；证据记 `workspaceEntriesRaw` vs `workspaceEntries` 计数）。
- **同 label 双条目（根因二）**：workspace 条目含 `workspacePurpose`
  （`web-remote-control.ts:633`；`WorkspacePurpose = "project" | "conversation"`，
  `packages/shared/src/workspacePurpose.ts`），同一人读 label 会以 project/conversation
  各挂一条 → 单靠 label 唯一匹配不成立。修复 = 指称唯一化阶梯
  `resolveDesignatedWorkspace`（sessionBridge.ts；探针 designate 步改调它）：
  label 精确匹配 → 多候选时优先 `workspacePurpose === "project"` → 仍多候选时优先
  `workspacePath === activeWorkspaceKey` → 仍不唯一即 `workspace-ambiguous` 中止；
  **purpose 收窄为空集时保留原候选集**继续走 activeWorkspaceKey（"prefer" 语义）。
  每步收窄只以 sanitized 计数进证据（`labelMatches` / `afterPurposeFilter` /
  `afterActiveKeyFilter` / `resolvedStep`），绝不输出值。
- **activeWorkspaceKey 的权威源 = bootstrap response**（只读回读，不是探针记忆）：
  `bootstrap-response.result.mobileViewState.activeWorkspaceKey`
  （`web-remote-control.ts:694-698` MobileViewState + `:726-735` bootstrap result；
  实连 run 已实证该字段存在且为 workspacePath 口径）；bootstrap 缺失时回落
  workspace-list-response 的 `result.activeWorkspaceKey`（同文件 `:672-676`）。
  解析产物 workspacePath 与 activeWorkspaceKey 相等时证据记
  `activeWorkspaceConfirmed: true`（APP 当前打开工作区的交叉确认）。
- **任务索引主动枚举（根因三的修复）**：被动收帧窗不再作为任务行来源。改为：
  `workspace-bridge-open`（taskId 可选：`web-remote-control.ts:435-447`；枚举期省略）
  打开读面桥拿到 bridge 身份 → 在 channel **`"window-controller"`**
  （`packages/shared/src/channels.ts:112` ServiceChannels.WindowController；该服务只在
  workspace bridge 的 relay 通道上发布——`packages/client/src/remoteServiceAccess.ts
:181-185` + `websocket.ts:102-108` connectViaProtocol +
  `packages/web/src/web-remote-control/app/webRemoteControlWorkspaceBridgeSession.ts
:39-74`）上：
  1. `EventListen(102)` 事件 `"onDynamicControllerFrame"`
     （`packages/services/src/window-controller/windowController.ts:78`；wire =
     `serialize([102, id, channel, event]) + serialize(undefined)`，
     `packages/rpc/src/channelClient.ts:150-177,179-194` +
     `channels.shared.ts:21`；动态事件名即属性名本体
     `packages/rpc/src/proxy-channel.ts:126-127`；后续帧以 EventFire(204) 按该 id 回推
     `channelClient.ts:210-231`）——官方 registry 同序：先挂帧监听再订阅
     （`packages/ui/src/v4/windowControllerTaskListRegistry.ts:145` → `:220-224`；
     手机任务首页 `packages/ui/src/hooks/useGlobalTaskList.ts:45-47` 消费同一 registry）；
  2. `subscribeControllerV4{topic:"controller/tasks-index", visibility:"foreground"}`
     （`windowController.ts:75`；params
     `packages/shared/src/zcode-protocol-v4/controller.ts:258-271`；ack
     `{subscriptionId, mode, logEpoch}` = `transport.ts:125-131` +
     `controller.ts:273-274`）；
  3. 有界收帧窗（8s）内收 `WindowHostControllerTaskFrame`
     （`controller.ts:251-256`）：`payload.kind="snapshot"` → `snapshot.tasks`
     （`:155-164`）；`kind="deltas"` → `task.upserted`/`task.removed`
     （`:187-191`）；行形状 `WindowHostControllerTaskRow`
     `:17-35,93-123`（`address{workspacePath, taskId, workspaceIdentity?}` +
     `meta.title`）。行按 `workspacePath\0taskId` upsert 去重，上限 1024 行防巨库。
     实现 = `ControllerTaskIndexReader`（sessionBridge.ts）+ session-probe 新 s5b 阶段；
     全部出站帧为**读面控制帧**（监听/订阅/退订），`maxSendFrames=1` 闸仍只数会话发送帧，
     发送路径零改动。`unsubscribeControllerV4{subscriptionId}`（`windowController.ts:77`）
     在窗闭后尽力退订（失败不影响结果）。subscribe 失败/超窗 = 结构性失败
     （`ABORTED:designate-task-subscribe-*`），零行 = `task-not-found`，
     多行 = `task-ambiguous`（沿用原中止语义，绝不按标题发送）。
- **fake 合同追加（tests/remote/zcodeBridgeSession.test.ts）**：阶梯七断言（去重复现
  28→14、bootstrap 优先/回落、purpose 收窄、activeWorkspaceKey 收窄 + confirmed、
  空集回退、不唯一/零命中中止、exact-label）+ 枚举三断言（subscribe→snapshot/deltas→
  title 精确唯一匹配且全程只发读面控制帧、零行、subscribe 超时不重发）；
  原六用例全绿。

### 7.9 workspace-bridge-open 身份/Initialize/回执勘误（2026-10-24 实连 124743Z 后二修）

实连 run `macos-B220261024T124743Z-gz7zRQ`（tasks-bridge-open=ok、subscribe-timeout、
peerAckedFrames=0、bridge-messages 出现 6 字节"乱码"帧）对照钉源复盘，修正三点
（行号 @ 3c5b2276）：

1. **ready 无 success 字段**：`workspace-bridge-ready`
   {requestId, bridgeSessionId, bridgeGeneration?, recoveryId?, bridge}
   （web-remote-control.ts:449-456）不携带 success；desktop 只在失败时发
   `workspace-bridge-error{reason, error}`（webRemoteControlManager.ts:1175-1191）。
   实连 transcript 的 `payload.success=false` 是探针旧 summary `success === true` 的
   误报——bridge-open 实际成功。summary 改三态（true/false/null）。
2. **帧身份 = 请求声明字段的原样回显，绝不虚构**：ready 的 bridgeGeneration/recoveryId
   只是请求回显（manager.ts:1156-1165 仅在 request 存在时写）；APP 接收侧
   `rawIdentityMatches` 对身份做严格相等（含 undefined，
   webRemoteControlAcknowledgedRelayProtocolTypes.ts:65-76）。探针旧实现给身份补默认
   `bridgeGeneration: 1` → 每条出站 rpc-frame 被静默丢弃且不回 ack = subscribe-timeout
   真因。规则：open 请求不声明 → 帧/ack 身份同样缺席。
3. **Initialize 门 + 接收侧回执**：桥 ChannelServer 构造即发
   Initialize=200 = `serialize([200])+serialize(undefined)` = `04 01 06 C8 01 00`
   （channelServer.ts:29-36,45-47,57-66；实连 bridge-messages 里的"乱码"帧即此）；
   官方 ChannelClient 把一切请求排在 whenInitialized 之后（channelClient.ts:124-128,
   159-163）→ 探针枚举与 send 都加有界 Initialize 门（超窗 =
   `initialize-timeout` / `STOPPED:bridge-init-timeout`，零控制帧出站）。接收侧按
   ackRelayProtocol.ts:293-304 对每条完整组装的入站消息回累计 rpc-frame-ack（读面控制
   帧，不占 maxSendFrames；不回执 → 对端按重放期限反复重发并最终降级桥，实连 3×
   Initialize 即此）。
4. **无前台前置**：workspace-bridge-open 不校验 `mobileViewState.activeWorkspaceKey`
   （manager.ts:1000-1054 只要求目标 workspace 在当前桌面窗口里）；activeWorkspaceKey
   只是手机页初始选区导航默认（web-remote-control.ts:758-783）→ 重跑无需把 APP 切到
   Travel 前台；拒绝情形以 `bridge-open-refused` verdict + 脱敏 reason/error 落证。

## 7.10 B2 发送闭环定案（2026-10-24 收官追加；不改写上文）

> 本节登记 B2 实连半场收官判定与合同落点。证据级别沿用 §6.9 口径：**live-sanitized**
> = 实连脱敏私有证据；**user-testimony** = 用户直接陈述；**source** = 固定 SHA 行号；
> **fake** = tests/remote/zcodeBridgeSession.test.ts 锁形。私有证据目录
> `~/Documents/tuvis-verify-logs/zcode/macos-B220261024T132316Z-prItvw/`（0700/0600，
> 不入仓库；本节只引用 run id 与 §4.1 别名，无任何真实值）。

六项事实（逐条挂证据级别）：

1. **实连 run 主链**【live-sanitized】：`macos-B220261024T132316Z-prItvw`——oauth
   PROVEN → connect PAIRED → mapping MAPPED（bootstrap / workspace-list ok，
   workspace 条目 28 原始去重为 14；主动任务枚举 snapshot×1 + deltas×19 = 119 行）→
   目标锁定（指称 Travel / hi 按 §7.3 阶梯精确唯一解析出真实 id，证据只见别名）→
   workspace-bridge-ready → 桥 Initialize ok → `sendConversationCommandV4` 受理
   outcome=**accepted**、framesSent=1、peerAck=true → 探针主动 close 收尾。全程恰一个
   会话发送帧、无重发（unknownRule 未触发——本轮受理即终态，见第 4 条合同落点）。
2. **marker 落点与用户可见性**【user-testimony】：用户确认
   `[tuvis-b2-marker:2026-10-24T13:26:59.709Z]`（与 transcript 的 issuedUtc 一致）以
   **用户消息**形态出现在指定测试会话（§4.1 别名 `hi`）中，agent 正常处理该消息并回复
   （turn 约 31 秒；回复明确识别其为测试 marker）。截图证据由用户本人持有，不入仓库。
   「只读 DB 最终可见性」不再作为本批次的强制追加项：用户在 APP 侧的直接可见性已覆盖
   §7.5 登记的核对面主体。
3. **精确归属**【live-sanitized + user-testimony】：marker 只落在指定会话；单帧单 run，
   无任何其他会话收到写向（transcript 发送帧计数恒 1；用户侧未见串投）——§7.7 的
   「envelope.sessionId ≡ listing address.taskId 同命名空间」显式核验项由该落点成立。
4. **能力前移**：`contract.capabilities.send = true`。第 1/2/3 条为 live/user 级证据，
   wire 形状基础 = §7.2 的 source 行号（未改写）；operations 追加
   sendConversationCommandV4 行（kind=write），含 unknownRule（受理但无结果 → unknown、
   绝不重发——实现于 SendOnceLoop、fake 锁形 `unknown_not_resent` 用例）。其余能力
   （approval / userInput / interrupt / model / thoughtLevel）维持 false，等各自批次
   授权后取证，本节不解锁。**历史状态；B3/B4 已由 §8.5–§8.7 逐项前移，最终以 §8.10 与
   contract.json 为准。**
5. **stdout 泄漏缺口（本轮发现并修复）**：收官 run 的「目标已锁定」一行把**真实**
   taskId/workspaceKey 打上了 stdout——证据文件经 SecretRedactor fail-closed 落盘闸，
   但 stdout 不在其内（别名制只护了文件面）。修复 = 该行改经
   `formatTargetLockedLine`（session-probe.ts）先登记取别名再拼行、故障围栏行的原始
   message 同样过 sanitizeText，人类可读 verdict 措辞不变；fake 断言追加锁定「真实值
   不上 stdout、别名与 mapping 阶段登记稳定复用」。已落仓库的收官 run 证据文件本身
   经查无泄漏（fail-closed 闸有效），缺口只在 stdout 控制台面。
6. **B2 范围内遗留（评审 deferred minors，如实登记）**：①`wrong_target_rejected` 用例
   对 SendOnceLoop 分支的断言为 spy 同义反复（该分支实际由 mapper/gate 前置拦截，
   测试面只锁「唯一入口是 mapper 解析产物」的纪律）；②`isTurnActivity` 判定宽松
   （`rawJson.includes(taskId)` 子串判据，可能与无关帧误配）；③`summarizePayload` 把
   app payload 原样嵌入证据结构（依赖 writeEvidence 前 sanitizeDeep 兜底，可再走
   redactPayload 加固）。三项不影响本节判定，留后续批次跟进。

## 8. B3 准备：审批 / 问答 / 打断的接入链（source 取证 + fake 合同）

> 追加于 2026-10-24。本节只登记**源码取证结论与 fake 合同**；真实动作证据在 B3 实跑后追加到 §8.x。固定源码基线 3c5b227649371507ddc4eb46ac5133afe6db1cab（逐条行号见 `scripts/zcode-bridge-probe/interactionBridge.ts` 头注与类注释）。

### 8.1 三条链（客户端侧，全部经 bridge 的 rpc-frame 通道）

1. **审批（permission pending）**：pending 卡由会话 topic 的 snapshot/deltas 下发（`transport.ts:163-191`；`snapshot.ts:511`；`delta.ts:40,120`）、控制态 `control.activeWorks` 同帧携带（`snapshot.ts:53-64,133-148`）。应答 = `resolveInteraction` 命令，payload `{interactionId, answer}`，answer 形状 `{optionId, freeText?}`（`V4InteractionDialogs.tsx:358-363`；`command.ts:221-234`）。**探针红线**：`fullAccess` 特权项在客户端硬闸排除（`snapshot.ts:234`；宿主侧 `interaction-background.ts:50-54`）。
2. **问答（userInput pending）**：应答 = `resolveInteraction` + `{action:"accept", content}`，content 形状同 `buildElicitationResponseContent`（`ElicitationDialog.tsx:273-303`）：`answers{[题干全文]: 选中 value ", " 连接}` + `answer_<i>` + 单题兼容 `answer`；题干/选项一律回传卡上原值（`interaction-broker.ts:505-517,622-650,766-780`）；空答案不伪造偏好（同文件 :277-278）。
3. **打断（stop）**：命令 `stop`，payload `{expectedForegroundExecutionId}`（`command.ts:161-164`）。守卫 id **必须**来自 `control.activeWorks` 的观察（`snapshot.ts:53-64`），缺省 = 盲停 → 探针 fail-closed 拒绝构造（`session-flow.ts:292-360` 守卫语义）。

### 8.2 客户端侧绑定与幂等（fake 合同）

- pending 册（`PendingBook`）只收 `conversation/<本会话>` topic 帧；四元组绑定 session/generation/anchor；`authorizePendingResolution` 三查闸（存在性 → 会话一致 → 代际一致）过闸即消费，重复应答给 `already-consumed`，错会话/旧代际**不会消费**别的 pending（`interaction-registry.ts:155-167` 的服务端幂等只是最后防线）。
- 每个交互 run：恰 1 个触发帧（复用 B2 已验证 send 链）+ 至多 1 个 resolve/stop 帧（autoResolution 在场时前置至多 1 个 snoozeInteractionAutoResolution 帧）；CommandAck 分类 accepted/duplicate/noop/rejected/stale/failed，超窗 = unknown，**结构性无重发路径**。
- fake 合同（`tests/remote/zcodeBridgeInteraction.test.ts`）：pending 绑定与代际、错/已消费拒绝、迟到响应不释放新代际、打断只作用于目标 turn、只读面不因交互流程被改写。

### 8.3 实跑入口与授权（未执行；待用户在场）

```
node scripts/zcode-bridge-probe/interaction-probe.ts approve \
  --i-know-this-connects --interaction-authorized --provider=bigmodel --open
# 其余模式：reject / answer / interrupt
```

- 前置：用户本人在浏览器完成一次官方 OAuth（每 run 一次）；实跑期间不依赖官方网页会话（同 role 互踢）；目标固定为 Travel 工作区「hi」会话（batch-2 已授权）。
- 触发指令为合成文本（`[tuvis-b3-probe:<UTC>]` + 无害文件操作 / 提问卡 / 逐行写文件），不含真实会话内容。
- 如实申报：若会话为自动批准模式（无 pending 卡）→ 审批能力记 `UNAVAILABLE:no-pending-observed`；问答卡形状不可解或非 AskUserQuestion（含计划审批卡）→ `UNAVAILABLE:answer-shape-unsupported`；无前台执行 id → `UNAVAILABLE:no-foreground-execution`（旧口径 `NOOP:guard.stopTargetChanged` 仅在守卫在执行中变化时出现）；均不重试、不猜测。

### 8.4 追加：conversation topic 事件流的 channel 订阅链勘误（2026-10-25；不改写上文）

> 触发证据：B3 实连 `macos-B320261024T230311Z-aTrTIg` / `macos-B320261024T230432Z-Vmt1pg`。两次 run 在 `bridge-subscribeConversationV4 {outcome:"ok"}` 之后只收到 request/response 帧，`unsolicited-frames: 0`、无任何 EVENT_FIRE/topic 帧 → `PendingBook` 恒空。固定源码基线仍为 `3c5b227649371507ddc4eb46ac5133afe6db1cab`（行号 @ 该 SHA）。

**根因（superseded 方案）**：此前只发 `bridgeCall("subscribeConversationV4", {workspacePath, sessionId})`（普通 Promise 请求）。该调用只让 CLI 建立订阅；CLI 的 conversation 帧走 `V4_NOTIFICATIONS.conversationFrame` 通知 → `zcodeAgentService` 按 workspace 投递给 `getConversationFrameEmitter(workspace)`（`zcodeAgentService.ts:2360-2395` 分发、`:1771-1780` 建 emitter、`:6563-6565` 暴露事件）。emitter 只有在 channel **事件监听**挂上时才有订阅者；官方 UI 的监听点是 `agentService.onDynamicConversationFrame(workspace)(...)`（`agentConversationTransport.ts:539-554`）。只发 RPC 不 EventListen 时该 emitter 无订阅者，桥侧只有 request/response 返程，因此 topic 帧永远不会变成 EVENT_FIRE/`unsolicited` 帧。B2 controller reader 之所以能收到帧，正是因为它先 `EventListen(onDynamicControllerFrame)` 再 subscribe。

**官方客户端订阅链（逐段行号）**：

1. **帧监听先行**：`agentService.onDynamicConversationFrame(workspace)`（`agentConversationTransport.ts:552-554`；`zcodeAgent.ts:956-960` 的签名为 `onDynamicConversationFrame(params: ZCodeAgentWorkspaceTarget): Event<ConversationTopicWireCandidate>`；`workspace = {workspacePath, workspaceIdentity?}` 同文件 `:94-97`）。动态事件客户端 = `channel.listen(propKey, arg)`（`proxy-channel.ts:126-127`），服务端 = 动态事件 `target.call(handler, arg)`（`proxy-channel.ts:61-74`）；wire = `EventListen(102) + serialize(arg)`（`channelClient.ts:150-177,179-194`）。
2. **topic 订阅**：`subscribeConversationV4({...workspace, sessionId, ...(base?), ...(visibility?), ...(retryInitialization?)})`（`agentConversationTransport.ts:187-215`；`sessionId` 来自 topic 后缀 `:188`，topic 规则 `transport.ts:113,205`）。ack = `{ack:{subscriptionId, mode, logEpoch}}`（`v4-transport.ts:125-131,158-161,453-457`），`agentConversationTransport.ts:233,247` 取 `subscriptionId`。
3. **ACK 激活屏障**：`barrier.begin(topic)` 在 RPC 前登记（`:202-205`）、`barrier.bind(pending, result.ack.subscriptionId)` 绑定 ack（`:232-247`）、`activate(subscriptionId)` 后只放行同 subscriptionId 的暂存帧（`:276-291`；`ackActivationBarrier.ts`：notification 可先于 RPC response，ack 前按 topic 暂存）。
4. **帧体**：EVENT_FIRE body = physical wire candidate（`wire.ts:19-75`：`{wireVersion:3, kind:"complete"|"fragment", deliveryKind?, logicalFrameId, logicalFrameOrdinal, topic, subscriptionId, frame?|fragment 字段}`）；logical frame = `{topic,subscriptionId,fromSeq,toSeq,sentAt,payload}`（`v4-transport.ts:163-191`）。超 1 MiB 逻辑帧按 UTF-8 字节分片、CRC32 校验（`wire-codec.ts`）。

**探针修正**：新增 `ConversationTopicSubscription`（`scripts/zcode-bridge-probe/sessionBridge.ts`，与 `ControllerTaskIndexReader` 同族：同一 `encodeChannelEventListen`/`encodeChannelRequest`/`RpcFrameAssembler`/`decodeChannelMessage` 与有界等待循环）。`run()` 顺序 = EventListen(`onDynamicConversationFrame`, workspace) → subscribe → 有界等 ack → 激活；ack 前帧暂存，ack 后按 `subscriptionId` 放行；兄弟 topic / 旧 subscriptionId / 坏 wire 帧只计数不交付；complete 直通、fragment 按 CRC32 重组。`session-probe.ts` 在 hello/clientHello 之后、触发帧之前建立该订阅，单槽帧汇先接钩子再 `run()`；topic 帧直入 `PendingBook`（不再把 EventFire body 当作 topic frame 直接喂册）。发送语义不变：仍恰 1 个触发帧 + 至多 1 个 resolve/stop（+ 至多 1 个 snooze），无重发。

**fake 合同**：`tests/remote/zcodeBridgeInteraction.test.ts` 新增 2 条——①EventListen 帧先行（channel `zcode-agent`、事件名、workspace 参数）→ subscribe 参数 → ack 前暂存 → ack 激活/按 subscriptionId 放行 → 兄弟 topic 与旧订阅帧拒收 → deltas 进册；②fragment 两片 CRC32 重组进册、坏 checksum fail-closed。

### 8.5 B3 实连结果（2026-10-25 收官追加；不改写上文）

> 本节登记 B3（审批 / 问答 / 打断）实连半场的三项判定与合同落点。证据级别沿用 §6.9 口径：
> **live-sanitized** = 实连脱敏私有证据（`~/Documents/tuvis-verify-logs/zcode/<run_id>/`，
> 0700/0600，不入仓库；本节只引用 run id 与 §4.1 别名，无任何真实值 / 链接 / token）。

三项判定（逐条挂 run id 与证据级别）：

1. **审批 = UNAVAILABLE:no-pending-observed（`capabilities.approval` 维持 false）。**
   【live-sanitized】run `macos-B320261024T230311Z-aTrTIg`：触发帧受理 accepted、
   peerAck=true，30s 待卡窗内**零 pending 卡**（topicFramesSeen=0、pendingSize=0，
   resolution 未尝试）；同时代理直接创建了 `tuvis-b3-permission-test.txt`——根因 =
   **指定测试会话为自动批准模式**，不存在 pending 卡。结论原因记入合同：
   "session auto-approves; no pending card exists in the designated test session"。
   早前 run `macos-B320261024T151206Z-JWXiGw`（pre-fix，trigger unknown、未发任何应答帧）
   标 **preliminary/invalid**，不计入裁决。
2. **问答 = 受理 accepted（`capabilities.userInput = true`，仅限单选；终态未证）。**【live-sanitized】run
   `macos-B320261025T012945Z-aMtnqA`：触发 completed → userInput pending 观察到
   （interactionId / 题干 / 选项 value 全部取自卡上原值）→ resolveInteraction 应答 →
   outcome accepted（ackStatus accepted、framesSent=1、peerAck=true）→ 历史 verdict RESOLVED。
   **2026-10-25 复审勘正**：该 run 在 ACK 后约 5ms teardown，未收到宿主撤卡帧；
   "RESOLVED" 仅按 ACK 得出，不能作为终态证据（见 §8.10）。
3. **打断 = 受理 accepted（`capabilities.interrupt = true`；执行停止未证）。**【live-sanitized】run
   `macos-B320261025T013128Z-X8HkPg`：触发 completed → `control.activeWorks` 观察到
   守卫 id（kind=primaryTurn）→ stop 帧受理 accepted（framesSent=1、peerAck=true）→
   历史 verdict STOPPED:interrupt-accepted。**2026-10-25 复审勘正**：该 run 在 ACK 后约
   4ms teardown，未观察守卫执行从 control 消失（见 §8.10）。

跨项事实：

- **reject 轮跳过的裁决**：approve run 已证明该会话自动批准（无 pending 卡可拒绝）→
  reject 轮必然同一结论，按裁决跳过、不额外消耗一次用户 OAuth；以本节登记代替空跑
  （不伪造 reject 证据）。
- **bridge-init 抖动 + 窗口加宽**：更早的打断尝试 run `macos-B320261025T013037Z-TT8HoQ`
  在建桥 Initialize 门超时（verdict STOPPED:bridge-init-timeout，零帧出站、未发任何 stop，
  结构性安全）；commit e34f3fe9 加宽 Initialize 等待窗后复跑通过（run `…X8HkPg`）。
- **conversation 事件信封修复确认**：B3 事件流必须走 channel 事件信封——EventListen
  （`onDynamicConversationFrame`）+ 动态 conversation 帧 + subscribe + ACK 激活
  （§8.4 根因），fix commit 27bc9dd3。answer run 实证该链生效：监听先行
  （eventListenRequestId=3 → subscribeRequestId=4）、subscribe ack（ackMode=snapshot）
  后 topic 帧入册（eventFramesSeen=6、snapshotsApplied=1、deltasApplied=2），pending
  才出现在 `PendingBook`——该 run 即修正后订阅链在真实环境的首证。

合同落点（2026-10-25 复审后）：`tests/fixtures/zcode-official-bridge/contract.json`——
`capabilities.userInput=true`（仅单选 AskUserQuestion，ACK-only）、`capabilities.interrupt=true`
（有守卫 stop，ACK-only）、`capabilities.approval=true`（仅应答既有 permission 卡；
`approvalForTerminalSend=false`）、`userInputMultiSelect=false`、`userInputFreeText=false`；
`operations` 含 `resolveInteraction` / `stop` / `switchModelConfig`，terminalEvidence 逐条注明
ack-only 或新鲜快照确认。`model` / `thoughtLevel` 由 §8.7 补全为 true；恢复原值仅探针清理。

## 10. 生产实施计划指针（B5）

- 生产统一控制施工单：[2026-10-25-zcode-official-bridge-implementation.md](../plans/2026-10-25-zcode-official-bridge-implementation.md)。基于本文件 §6–§8.10 的已取证合同：发送、只读、单选问答、有守卫打断、模型/档位已实跑；审批仅"应答既有卡"已证，Tuvis 发送→弹卡未证（negative evidence），生产门保持关闭；问答/打断的 ACK 不等于终态，生产 UI 必须区分 Accepted / Confirmed。

### 8.6 B4 实跑（thoughtLevel 已证 / model 未证）

- run `macos-B320261025T025928Z-3WyTFA`（脱敏证据私有目录）：读取 `snapshot.config` = provider `account:bigmodel-individual-coding-plan` / model `GLM-5.3-Flash` / thought `max`；`availability.switchModelConfig.allowed=true`。
- 切换 thought `max → low`：`switchModelConfig` 命令 ack `accepted`；等待新快照后 `config.thought` 确为 `low`（**ACK≠投影生效，必须等 snapshot/delta**，与 §9 笔记一致）。
- 恢复原值 `low → max`：ack `accepted`，新快照确认恢复；全程未改全局默认、未影响其他会话。
  **边界**：恢复原值只是探针清理，不是生产行为；生产必须保留用户刚选的档位。
- **结论**：`capabilities.thoughtLevel=true`；`capabilities.model=false`（本次目录只暴露当前模型 id，模型切换需 `model-selection.getView()` 目录且其 mobileRemote 角色可见性未验证，未尝试——UI 保持关闭）。**该 model=false 已被 §8.7 补全为 true。**

### 8.7 B4 模型切换实跑（补全）

- run `macos-B320261025T030637Z-bS5dOg`：`model-selection.getView()` 对 terminal 角色**可用**（返回 `{revision, providers[], preferredSelection}`；providers 用 `providerId`/`models[].modelId` 字段，含 4 个 provider）。
- 目标选择限定在同一 provider：`GLM-5.3-Flash → GLM-5.3`（`modelChanged:true`，thought 保持 `max`）；`switchModelConfig` ack `accepted`；新快照确认 `model=GLM-5.3, thought=max`；随后恢复 `GLM-5.3-Flash`（ack `accepted`）并确认恢复。
  **边界**：恢复只是探针清理；生产保留用户选择。
- **结论**：`capabilities.model=true`、`capabilities.thoughtLevel=true`。跨 provider 切换未尝试（不属本轮授权范围，且目录暴露了其他账号 provider——生产实现必须限定用户当前账号 provider）。
- **复审补强（2026-10-25）**：目录里同一 provider 下各模型的档位可以不同；校验必须按**目标模型自己的** `reasoningLevel.values`，不得把同 provider 所有模型并成一份（`selectionBridge.ts` 已改为 `thoughtLevelsByModel`；探针选择目标模型时也只从该模型档位里挑）。

### 8.8 审批卡负结果（2026-10-25，两轮）

- run `macos-B320261024T230311Z-aTrTIg`（文件写入触发）：无 pending；临时文件被**直接创建**。
- run `macos-B320261025T031402Z-QjC2hA`（shell 触发：`pwd && echo tuvis-b3-shell-test`）：无 pending；turn 正常完成。
- 结论：**该会话权限策略为全部放行**（文件写入与 exec 均不产生 `permission` 卡），探针实现无缺陷（`pendingSize` 摄入正常，userInput 卡可收可答）。
- 取证前提（用户动作）：在 ZCode APP 把测试会话切到「需要确认 / Default / Ask」模式后复跑 approve/reject 两轮；若该构建无此模式，则 `capabilities.approval=false` 定案（原因：宿主不产生权限卡）。
  **该推论已被 §8.9 修正**：本构建没有"切模式"入口，但用户亲发的「变更前确认」提交可以弹卡；因此 approval 的已证范围仅限"应答既有卡"，不是"发送会弹卡"。

### 8.9 审批卡双向实跑（approve / reject 均通过）

- **催卡前提（实跑结论）**：本构建下，仅**用户在 APP 输入框亲发**的提交（模式「变更前确认/guarded」）会拦截并弹 `kind="permission"` 卡；探针替会话合成的 sendText（默认或 `mode:"guarded"`）均直接执行不弹卡（4 轮负结果见 §8.8）。
- **approve**（run `macos-B320261025T032425Z-K7I6ZA`）：用户 APP 发「删除 /tmp/tuvis-b3-approval-test.txt」→ 卡 3 选项（含 fullAccess，客户端硬闸排除）→ 探针 `--existing-pending` 选 allow-once → CommandAck `accepted` → **文件被删除**（产物核对）。
- **reject**（run `macos-B320261025T034803Z-noorSA`）：同法再开一卡 → 选 deny → `accepted` → **文件仍在**（拒绝生效）。
- **结论**：`capabilities.approval=true`，但**只表示"应答既有 permission 卡"**；卡形状/选项/回执与源码取证一致；探针从不选择 fullAccess、不改会话权限模式。
- **关键边界（2026-10-25 复审确认）**：`approvalForTerminalSend=false`。Tuvis/探针走的是 terminal 发起的 sendText，本构建实测（默认与 `mode:"guarded"`）**不会弹卡、直接执行**（§8.8 四轮负结果）。因此生产不能把"guarded 模式"宣传为对手机发送的审批保护；在"Tuvis 发送→弹卡→应答"整链取证前，审批 UI 只能应答外部（APP 亲发）产生的卡，或保持关闭。
- **终态证据**：approve/reject 的产物（文件删除/保留）由用户侧核对，强于 ACK；探针 transcript 本身仍在 ACK 后立即 teardown，未观察撤卡帧。

### 8.10 独立复审后的证据分层与修正（2026-10-25）

> 触发：对 HEAD `90236b12` 的独立只读复审。结论：不按"A/B 全部完成、九项能力全部闭环"合并。
> 以下为逐项核对后的定案；全部可在源码/私有 transcript 复核。

1. **ACK ≠ 终态（Critical）**：answer run `…T012945Z-aMtnqA` 在 ACK 后约 5ms、interrupt
   run `…T013128Z-X8HkPg` 在 ACK 后约 4ms 即 teardown；没有宿主撤卡/执行停止的后续帧。
   历史 verdict `RESOLVED` / `STOPPED:interrupt-accepted` 降级为**命令受理已证**。
   探针已加终态观察窗（`INTERACTION_TERMINAL_WAIT_MS=10s`）：answer/approval 必须等
   `hostPendingRevision` 前进且 interactionId 不在宿主集合；interrupt 必须等
   `controlRevision` 前进且守卫 id 不在 `activeWorks`。否则只报
   `ACCEPTED:pending-unconfirmed` / `ACCEPTED:interrupt-unconfirmed`。
2. **发送→审批未证（Critical）**：审批卡只由用户 APP 亲发（guarded）触发；terminal
   sendText 默认与 guarded 均直接执行。已证的是"应答既有卡"，不是"Tuvis 发送→弹卡→应答"。
   合同新增 `approvalForTerminalSend=false`；B5 T5 加生产停止门。
3. **问答形态**：实跑只覆盖单选 AskUserQuestion；多选与自由文本仅有 fake/合同层。
   合同新增 `userInputMultiSelect=false`、`userInputFreeText=false`；B3 不能据此宣称全部形态闭合。
4. **token 一次性**：实跑只证 `expires_in=60` 与两次申请值不同；"一次性"来自源码/fake
   （过期/已消费 token 被拒），未做重放拒绝实验。合同措辞已改为 source+fake 依据。
5. **模型档位归属**：原 `selectionBridge` 把同 provider 各模型档位并成一份，可能放行
   "模型 A + 仅模型 B 支持的档位"；已改为逐模型 `thoughtLevelsByModel`，探针目标选择只取
   目标模型自己的档位，且只使用当前会话 provider 的目录。
6. **恢复原值**：只是探针清理；生产保留用户选择（B5 T6 已改）。
7. **证据脱敏边界**：OAuth URL 连同 state 故意打印到 **stdout** 供用户点击，不写证据文件
   （state/link 已登记 redactor）；此前桥消息 `bodyPreview` 会把普通帧正文原样写入证据，
   已改为只记结构头、Initialize 仅记 hex；CLI 参数错误不再回显原值。
   JSON 证据文件仍经 fail-closed 断言，但"所有输出都已脱敏"只应理解为"证据落盘面"。
8. **A8**：代码与自动门禁完成 ≠ 平台验收完成；Windows/真机/链接生命周期仍待验。
9. **打断必须绑定本次触发（第二轮复审 Critical）**：原实现只要求 `controlRevision` 前进；
   若触发前已有执行 E、触发被拒绝或排队，后续 control 帧仍报告 E 时会误停 E。修正：
   触发前记录 `foregroundExecutionIds()` 基线，`observeStopTarget` 排除基线 id；触发结果
   非 `accepted`/`completed` 不进入 stop；`--existing-pending + interrupt` 直接拒绝。
   回归：`stop_target_excludes_pre_trigger_execution`（新帧仍带旧 id → null；旧+新并存只取新）。
10. **无可恢复档位不得切换**：`thought` 空串读作 null；原实现仍可切模型但跳过恢复，
    却能返回 `COMPLETED:selection-switch-restore-unconfirmed`。修正：select 预检
    `original.thought === null` → `UNAVAILABLE:no-restorable-thought`（零写帧）；
    `planRestore` 继续 fail-closed；`sendSwitch` 内同时校验**原选择**在目录里可恢复，
    任一校验不过都零帧。回归：`restore_rejects_missing_original_thought`。
11. **空档位列表不得放行**：`assertSwitchable` 原条件只在列表非空时校验，`[]` 接受任意值。
    修正：空列表直接拒绝（宿主未宣告 = 无法证明目标合法）。回归：
    `empty_level_list_rejects_any_thought`。
12. **CLI 错误出口去原值**：未知参数 / 未知交互模式不再回显输入（auth-check /
    session-probe / interaction-probe）；以合成值实测，输出不含该值。
13. **合同文档同步**：`route` 的 workspace/session 绑定字段与 B2 实连一致
    （`workspacePath` + 可选 `workspaceIdentity`；listing `address.taskId` ≡
    `envelope.sessionId`），`credentialPersistence` 与 B5 §2.3 对齐（JWT 仅内存，
    本轮不落 OS keychain）；补 B2 binding live evidence。复核期间出现的未提交
    `session-probe.ts` 回退（撤掉终态观察/逐模型校验、恢复 `bodyPreview`）已保存为
    补丁并从工作树移除，不进入任何提交。
14. **初始 control 未确认不得触发 interrupt（第三轮复审 Critical）**：订阅只等 ACK，
    初始 snapshot/control 可能延迟；若在 control 到达前取基线（空集合），延迟首快照
    里的旧 E 会被误判为新执行。修正：interrupt 触发前等待
    `hasConfirmedControl(book.control)`（`control.activeWorks` 为数组；`[]` = 已确认
    无执行，null/缺字段 = 尚未知），超时 `INITIAL_CONTROL_WAIT_MS=10s` →
    `UNAVAILABLE:no-initial-control`，**零触发、零 stop**；基线只在确认后取。回归：
    `initial_control_must_be_confirmed_before_baseline`（ACK-only → pending-only
    snapshot → control 缺 activeWorks → 延迟 control 含 E → 基线含 E → 后续 E 被排除）。

### 8.11 生产硬化状态与剩余缺口（T8.2，2026-10-26，HEAD `981ee02b`）

> 触发：T8.2 代码完成后的验收收口。基线 `8e663f09`，分支
> `codex/zcode-unified-remote`。完整逐项结果见
> [生产验收报告](../reviews/2026-10-25-zcode-production-acceptance.md)。

1. **能力闸默认全关（已落地）**：`CapabilityPolicy::for_host(appVersion, protocolVersion,
   schemaKnown, helloKnown)` 只在 APP build `3.16.2.8769` + 协议 v3 + schema/hello 全部
   识别时才认为宿主已取证；即便如此仍返回 `production_unverified()`（八项全关，
   `needs_evidence`）。未知 build/协议/schema/hello 同样全关。生产装配当前直接使用
   `CapabilityView::default()`；`for_host` 尚无运行时宿主元数据来源，开闸前必须接线。
2. **证据许可仅 debug（已落地）**：`evidence.rs` 只在
   `all(debug_assertions, feature="zcode-bridge-evidence")` 编译；release + 该 feature
   由 `mod.rs` 的 `compile_error!` 拒绝。桌面专用 IPC
   `zcode_control_arm_evidence(session, generation, capability)` 发放内存许可：
   120,000ms 有效、一次动作消费、换代/关服务失效、snooze 首次绑题不消费、其他题不得
   借用；目标/代际/pending/`fullAccess`/option kind 守卫全部原样执行。手机无发放入口，
   许可不落 settings/DB/日志。回归覆盖单次与过期、换代失效、`fullAccess`/错目标拒绝、
   snooze 绑题、release 编译门。
3. **T8.1 loopback 缺口仍在（Important，未闭合）**：brief 要求的 loopback HTTP+WS
   假官方端未交付；`PartsBackend` 的 `RelayClient ↔ BridgeManager ↔ CommandSender`
   组合只有构造冒烟，进程内假体停在 `HubBackend`/`CommandPort` 边界。不得把 T8.1
   描述为覆盖“整条生产链”；需补 loopback fake 或书面移交，二者必居其一。
4. **T8.2 产品验收行全部 blocked/not_run**：当前无用户本人官方登录的专用测试会话、
   无用户指定目标、无 Windows 主机，macOS 产品 UI 行与 Windows 全部行均未执行。
   探针证据 B1–B4（B1 `…T105940Z-NarmSw`；B2 `…T132316Z-prItvw`；B3
   `…T012945Z-aMtnqA`、`…T013128Z-X8HkPg`、approve `…T032425Z-K7I6ZA`、reject
   `…T034803Z-noorSA`；B4 `…T025928Z-3WyTFA`、`…T030637Z-bS5dOg`）只证明该形态，
   不替代产品验收：单选用例与 interrupt 仅 ACK（未观察宿主撤卡/执行停止），多选与
   自由文本只有 fake/合同层，审批只证“应答既有卡”，`approvalForTerminalSend=false`
   （terminal sendText 默认与 guarded 均不弹卡，4 轮负结果）。
5. **开闸规则（本批定案）**：**任何能力都不得在没有“该宿主 + 该动作 + 该 APP build”
   的实机产品证据时开闸**。合同 `capabilities` 的宽泛 `true`、探针 run、fake/合同层
   结论均不能作为生产开闸依据；只能先经 debug 证据许可跑出产品证据，再最小更新
   policy 并保留失败行的 `blocked`/`not_run`。`T5` 显示催卡负结果未消除前，
   `approvalReply` 保持关闭；问答/打断终态、多选/自由文本、Windows、双手机、断线/
   失效/互踢未跑前，整批只能标“部分实现/能力受限”，不得称 `spec_complete`。
6. **边界**：本记录与验收报告不改 `docs/MASTER-PLAN.md`；不 push、不合并、不打 tag、
   不发版；未开放任何生产能力，普通构建八项控制能力仍全部 `enabled=false`。

### 8.12 凭据持久化分界（2026-10-27，凭据复用批次）

> 本节为**追加段**，不改写 §7.6 / §8.11 的历史口径：旧探针与旧 run 的
> 「账号 JWT 仅内存、进程退出即弃」仍是其当时实现与取证事实，旧 run 记录原样保留；
> 本批生产装配改用方案 A（系统保险箱持久化 + 重启恢复）。

1. **旧探针仅内存（历史保留）**：B1–B4 的 `scripts/zcode-bridge-probe` 与旧产品构建
   只把账号 JWT 放在进程内存，进程退出即弃；当时「不落 OS keychain（后续单列评估）」
   的结论只适用于该实现与取证批次。历史报告、run id 与合同原文不追改。
2. **本批生产采用方案 A**：OAuth 成功后账号 JWT 经 `CredentialStore` 写入系统凭据
   保险箱（macOS Keychain / Windows Credential Manager / Linux Secret Service），
   单条目 `{schemaVersion, provider, jwt}`；启动时 `restore` 只读该记录，不打开浏览器、
   不自动 OAuth。terminal token 仍每次连接现取、仅内存、用后即弃，不持久化。
3. **首次升级需授权一次**：旧版本没有持久记录，升级后第一次启动只会得到
   「无记录 → 需要授权」；用户完成一次官方授权后 JWT 才落保险箱，后续进程重启
   自动恢复，不再要求重新授权（过期/撤销/忘记除外）。这不是回退，而是首次迁移的
   预期行为，用户手册与更新日志同批说明。
4. **断开与忘记的区别**：`disconnect_requested` 持久写 `auto=false` 但**保留**凭据与
   链接；`forget_authorization` 持久写 `auto=false`、**删除**凭据、**保留**链接。
   换/删链接只断连接、保留账号（不重新 OAuth）；删除失败（`delete_failed`）与保存
   失败（`storage_failed`）如实上报，不假报已删除/已记住。
5. **JWT 边界**：仅系统保险箱 + `AuthService` 受控内存；不写 settings/SQLite/文件，
   不进日志/错误文本/IPC/手机响应；`AuthStatus` 仅桌面 IPC，手机只读既有
   `ConnectionView`，**永远不拿 JWT 或 terminal token**。本地回环 HTML
   与设置页也不展示 JWT；探针证据只见别名与掩码。
6. **合同夹具同步**：`tests/fixtures/zcode-official-bridge/contract.json` 的
   `auth.credentialPersistence` 同批注明「旧探针仅内存 / 本批生产采用 A」，并保留
   旧 run 历史；`generatedBy` 不重写，只追加本次分界说明，避免把新裁决冒充旧证据。
7. **自动化与平台边界**：本批 `wire_e2e` 用共享 fake 持久存储 + 重新构造
   AuthService/Hub 证明「OAuth 一次、恢复后重新取 terminal token/WS、断开与忘记后
   重启零上游、恢复不放开能力闸」；真实 Keychain/Credential Manager/Secret Service、
   真实 OAuth 与真实进程重启仍需 macOS/Windows/Linux 各自实机验收，未跑行保持
   `awaiting_platform_validation` / `not_run`，不以 fake 结果标 PASS。
