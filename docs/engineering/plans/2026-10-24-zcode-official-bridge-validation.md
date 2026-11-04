# ZCode 官方控制接入定案与真实验收计划（工作包 B）

> **执行 Agent：** 使用 superpowers:executing-plans，按 task 执行并记录结果；仅在用户选择子 agent 执行后使用 superpowers:subagent-driven-development。本计划先交付可验证的接入合同，不凭未知认证接口编造完整生产施工单。未通过前置门不得开放产品写操作。

**目标：** 证明 Tuvis 可作为受授权的官方远控客户端，取得正确 APP／session／pending 的状态和逐动作结果，再产出可直接施工的统一控制实现计划。

**架构：** 单台 Tuvis 主机一条上游 terminal 连接，现有配对手机复用本机控制 API；读取仍走原有本地数据。授权、短期 token、relay、bridge 和动作层逐段取证；禁用 DOM 自动点击或新起私有 app-server 冒充附着原 APP。

**技术栈：** 项目现有 Rust/TypeScript 测试工具、固定提交官方源码／协议测试、独立 fake relay 和受控测试会话。生产 WS／凭据存储依赖在 B1 定案，不能擅自增加平台或 SDK。

**设计：** [ZCode 统一远控设计 v3](../specs/2026-10-24-zcode-unified-remote-design.md)；[用户与双平台能力证据](2026-10-23-zcode-probe/2026-10-24-capability-status.md)。

## 全局约束

- A 包可独立交付；A 的 Saved 链接状态不是 Connected。B 没过门时保持控制区不可用、正文文件可用。
- 官方网页已被用户验证，不再做“网页能否发送”的重复实验；本包对象是独立客户端身份／连接／动作映射。
- 链接不是 JWT，Tuvis 配对 cookie 不是官方登录。官方登录／扫码／账号授权由用户本人完成；不读取其他应用账号文件、Cookie DB、进程内存或日志中的凭据。
- 本机手册和私有证据目录只存必要结构，真实凭据进入本轮临时受保护存储并在结束按授权清理；不提交真实 SID／JWT／token／正文。
- 不使用 full-access／永久授权选项作审批成功捷径；不扩大用户权限。多选／问答原题 ID 和选项不能自造。
- 官方同 SID／role 单 owner；发现 KICKED／冲突就停自动重连，不能与用户官方浏览器无限互踢。
- 发送结果未知不重发；只终止自己创建的测试客户端，不 kill 用户 APP。
- 用户指定专用工作区／会话并逐批授权真实动作。模型切换／思考强度记录原值和测试值，结束恢复测试会话设置；不改变全局默认。
- MASTER-PLAN 不改；协议接入归三期，不声称完成 CLI F2.3。#117 hi gate 不加入本包。

## 评审重点

1. 看见官方页面成功≠独立客户端能拿身份；无合法授权路径必须停止，不以爬 cookie 补洞（B1）。
2. 同角色 owner 替换与两台 Tuvis 手机不能产生两条抢占连接（B2）。
3. session／workspace／request 的错误匹配会误投递／误审批，必须用兄弟会话和错 ID 负例（B2、B3）。
4. 超时／迟到／重复事件不得引发重发或跨代际消费（B3、B4）。
5. 协议可用与产品 HTTP/UI 完成分开，不伪造能力 flag；最终计划须覆盖只读独立可用（B5）。

## 固定产物与接口

- 新建研究决策文件 docs/superpowers/decisions/2026-10-24-zcode-official-bridge-contract.md；B1 先写实际身份／wire，B2—B4 追加各自证据；不填 TBD，不用伪造 endpoint。
- 私有 raw：用户 Documents/tuvis-verify-logs/zcode/<platform-B-run_id>/；入库仅 sanitized fixture。
- 新测试工具目录 scripts/zcode-bridge-probe/，只在明确执行本计划时创建；不挂进应用启动／扫描。
- 契约 JSON 文件 tests/fixtures/zcode-official-bridge/contract.json，schemaVersion=1：
  identity{platform,appBuild,bundledCliFingerprint,sourceSha};
  auth{status:proven|unsupported,flowName,tokenSourceKind,credentialPersistence};
  route{role:terminal,ownerScope,workspaceBindingFields,sessionBindingFields};
  capabilities{send,approval,userInput,interrupt,model,thoughtLevel};
  operations 每项记录实际 wireMethod、requestSchema、responseSchema、terminalEvidence、unknownRule、sourceLines、fixtureRefs。
- 上述 JSON schema 是本计划的产物结构，不是杜撰官方 wire。operations 的具体方法／字段必须来自实现＋真实脱敏帧；未取得就 capability=false，给出明确不支持原因并停对应任务。
- 后续生产实现者只消费已验证合同，不去猜某个网页按钮最终发了什么字段。

## B0：准备独立取证与源码范围

**文件：** 决策文件头部、证据索引；不编辑产品运行链。

**输入：** A4 的配置 URL 格式合同、用户官方页面长期使用证据、双平台已知版本。
**输出：** 本轮安装指纹／源码 SHA／run_id、官方模块与许可证清单、测试目标别名。

- [ ] 核对本机 APP/随包CLI/运行版本（无安全启动条件则沿用PACKAGE并标未运行）、记录哈希；取固定官方源码 SHA 3c5b227649371507ddc4eb46ac5133afe6db1cab，若升级只记录新fingerprint，不改写旧证据。
- [ ] 以 Mac P0-R 的 host／services／rpc／relay链和实际 broker/server-operations 为入口，不再引用旧 v1“未来才做交互”作为当前事实。
- [ ] 只下载实现所需文件与测试，记录许可证／复用方式。禁止复制本轮用户完整链接到决策文档。
- [ ] 用户指定测试工作区／会话；记录哪些动作已授权、哪些要现场确认。未完成这步可继续源码／fake测试，不执行真实写。
- [ ] 提交来源和合同结构，停止 review。

## B1：官方身份与最小只读连接定案门（必须早于生产桥接）

**文件：** scripts/zcode-bridge-probe/auth-check.ts；tests/remote/zcodeBridgeRelay.test.ts；contract.json 的 auth/route 部分；决策文件。

**输入：** B0 指纹与官方认证实现。
**输出：** proven 的身份取得方式、所需最小依赖、凭据生命周期／保护存储选择；或明确 unsupported 结论。没有 proven auth，B2—B5 不执行写实验。

- [ ] 红测 fake_terminal_requires_owned_identity：未认证不能申请／消费token或发送data；合法凭据才可申请role=terminal；过期／重复消费token被拒，不把SID当token。
- [ ] 源码核对官方允许的终端登录／授权方式，写出实际URL／回调／身份来源／刷新机制与注销动作。不能凭习惯假设官方支持任意第三方OAuth client。
- [ ] 用所选客户端库和官方测试契约实现fake relay连接探针，测试文件标注 Vitest node environment，执行 pnpm exec vitest run tests/remote/zcodeBridgeRelay.test.ts；证明认证失败不发后续帧。不为运行测试额外引入 tsx。
- [ ] 用户本人完成官方登录／必要配对。Agent 仅通过已定案的授权返回通路持有本轮凭据；如没有可用授权通路，输出 auth.status=unsupported、原因和合法备选，停止。不得要求用户复制JWT或读取Safari/ZCode凭据文件。
- [ ] 在批准的链接和测试实例上建立一次terminal连接，仅读设备／workspace／session状态，不发送、审批或切模型；保存脱敏握手、权限范围和退出证据。
- [ ] 验证token短期性、重新连接需新token、owner冲突处理，断开本测试连接。不自动重新注册设备或夺取owner。
- [ ] 填合同auth／route、锁定真实依赖及凭据保护方案（如确需OS凭据库，此处比较项目可用库并确定依赖，不使用明文settings）。提交后review。

**门：** “proven且实际只读连到正确测试APP”才能进入B2；只有源码字符串命中不算。

## B2：单连接、会话映射与发送闭环

**文件：** 新 scripts/zcode-bridge-probe/session-probe.ts；tests/remote/zcodeBridgeSession.test.ts；sanitized fixtures；合同 send 项。

**输入：** B1 proven auth/route。
**输出：** session与workspace绑定、send实际wire、受理／完成／未知证据。

- [ ] 红测同一Tuvis主机多手机请求只用一条上游连接；错workspace／错session／旧代际映射拒绝，不能按标题或cwd选一个。
- [ ] 红测peer受理后丢响应，客户端只记unknown，不重发；重复事件去重不生成第二条发送。
- [ ] 在测试会话发送一次唯一合成marker，核官方snapshot中的user消息与turn关联、回复及只读DB最终可见性；保留兄弟会话，证明没有串投。
- [ ] 明确同一台电脑多工作区／窗口的能力边界：绑定证据不充分的卡只能读取，不能开放发送。
- [ ] 记录官方页面同时打开时的owner替换行为；冲突不循环重连，不把另页被替换当连接成功无影响。
- [ ] 执行 pnpm exec vitest run tests/remote/zcodeBridgeSession.test.ts；真实实验仅按批准次数执行。contract.capabilities.send仅在精准归属与结果证据均成立时true。
- [ ] 提交合同／脱敏帧和结果，review后进入B3。

## B3：审批、问答、打断的逐能力合同

**文件：** scripts/zcode-bridge-probe/interaction-probe.ts；tests/remote/zcodeBridgeInteraction.test.ts；合同 approval/userInput/interrupt。

**输入：** B2 目标绑定、B1代际与认证。
**输出：** 三项能力各自实际request／decision／terminal证据及不可用原因。

- [ ] 红测pending绑定session/turn/interaction和连接epoch；错ID／过期／已消费／重复应答不改其他请求。
- [ ] 红测退出连接、换SID后迟到响应不能在新epoch上释放pending；用户已有草稿／只读视图不丢。
- [ ] 测试实例触发无害文件操作，独立两轮分别批准／拒绝，核产物；不得用会话全量allow或full-access取代单次批准。
- [ ] 测单选／多选／自由文本问答，以原题ID／选项／字段回传；不支持形态写capability=false而非发送猜测字段。
- [ ] 打断APP自己启动的测试turn，保留兄弟turn；记录pending失效与工具／后台任务真实后果，不能用kill自起子进程冒充。
- [ ] 执行 pnpm exec vitest run tests/remote/zcodeBridgeInteraction.test.ts，覆盖各能力fake正反例；真实帧脱敏入库；合同区分“可显示pending”和“可应答”。
- [ ] 提交并review。缺某能力不自动拖累读取和其他已验证能力，也不谎称全部控制已通过。

## B4：模型／思考强度与恢复规则

**文件：** scripts/zcode-bridge-probe/selection-probe.ts；tests/remote/zcodeBridgeSelection.test.ts；合同 model/thoughtLevel/恢复规则。

**输入：** B1/B2已定案身份和目标。
**输出：** 模型与强度选项来源、设置作用域、忙态规则与回显证据。

- [ ] 红测只接受服务端当前支持的模型／强度ID，UI显示名不能当wire值；旧代际／错目标／未知值拒绝。
- [ ] 用户批准的测试会话记录原模型／强度，设置一个支持值，读新鲜snapshot确认作用范围和生效时机；结束恢复原值。无修改全局默认授权。
- [ ] 假服务覆盖离线、Not Found等结构错误、认证失效、超时、KICKED、迟到结果；分别映射needs_config/needs_auth/offline/conflict/unknown，不把全部失败称链接过期。
- [ ] 验证发送／控制失效时只读查询仍可用；保存URL成功不返回connected。
- [ ] 执行 pnpm exec vitest run tests/remote/zcodeBridgeSelection.test.ts；提交完整合同，附每项能力的来源／scope／失败规则／fixture与双平台差异。

## B5：形成生产统一控制实施计划与末期人机验收清单

**文件：** 合同与决策文件；新 docs/superpowers/plans/<实际日期>-zcode-official-bridge-implementation.md（仅B1—B4通过后创建）。

**输入：** 已review的实际wire／授权／依赖，不是今天尚未知的假设。
**输出：** 可直接写生产代码的接口、任务、测试、文件和真实验收。

- [ ] 按合同冻结本机适配器trait、action状态、pending投影、事件归一、REST/SSE以及凭据保护接口；消费同一份已验证schema，不在前端复制协议字段。
- [ ] 排生产实现：官方授权／连接→动作与代际→send→pending应答／interrupt→模型强度→Tuvis控制区→全链验收；保持H3解耦和只读独立。
- [ ] 每任务写红测／实现签名／命令／通过条件／提交点，不能只写“接一下API”。未取证能力明确延后，不做默认true。
- [ ] 末期由用户完成必要登录／配对、指定专用会话与手机体验确认；Agent自动跑剩余批准动作。读取／文件、PIN遮罩、链接更新、两手机、断线与审计均纳入。
- [ ] 将产品控制动作与结果记入既有write_audit，账号和正文不入日志；单纯打开官方链接不冒充发送审计。
- [ ] 更新准确中英操作文档／能力矩阵／升级说明，保持官方外链辅助入口与统一控制实现边界。
- [ ] 交付实施计划供用户确认；不要在本包里顺手实施。若B1过不了，交付结论与可选路线，保留A已交付功能，不把外跳当作已完成统一UI。

## 本计划的诚实边界

A包已有完整产品施工任务；本包是可执行的接入定案任务，含明确停止门。官方控制生产施工单由B5基于真实合同产出。不能在身份／wire未确定时承诺今天已经写出可照抄实现的全功能生产计划。
