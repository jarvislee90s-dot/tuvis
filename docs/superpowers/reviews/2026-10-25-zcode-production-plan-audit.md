# ZCode 生产计划接口、依据与 spec 覆盖核查

复核日期：2026-10-26。代码基线 `8e663f09`。范围为重写生产计划，不改产品、不真实登录/连接/发消息/审批，不改 `docs/MASTER-PLAN.md`。

核查依据：统一远控 spec v3、决策记录 §6–§8.10、contract.json、scripts/zcode-bridge-probe 下现有探针及测试、固定官方源码 SHA `3c5b227649371507ddc4eb46ac5133afe6db1cab`。源码读取自本机 `tuvis-verify-logs/zcode/pinned-src-3c5b2276/` 及 Mac P0R 固定源码副本；补读同 SHA heartbeat 与 snapshot 源文件。本文不重复传播真实链接或私有 transcript。

## 结论

旧计划方向成立，但**不能作为直接交给陌生 Agent 施工的完整 TDD 计划**：缺实际生产接线、wire 层、明确接口、失败期望、逐形态证据门，且把部分探针实验保护误当产品语义。已原位重写 [生产实施计划](../plans/2026-10-25-zcode-official-bridge-implementation.md)，保留 T1–T8 编号族，拆为 13 个独立可审查子任务；所有实现步骤尚未执行。

新版覆盖 spec 全部功能点的施工或既有回归，也把补证纳入本批末期；**不等于现在能保证全部功能开放**。发送催卡有已知负结果，问答/打断终态、多选/自由文本、Windows/ZAI 仍有未闭合证据。本批的 spec_complete 必须以对应实机门通过为条件，不能拿“按设计禁用了按钮”冒充该功能交付。

## 逐项发现与修订

| 旧计划问题                                                        | 核查依据                                                                                                                             | 新计划处理                                                                                                  |
| ----------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------ | ----------------------------------------------------------------------------------------------------------- |
| T1 只写 auth/relay，没有长驻 WS 依赖、心跳/过期/取消接口          | Cargo.toml 无客户端 WS；官方 web transport 使用 Heartbeat；短探针不含完整长驻行为                                                    | T1.1/T1.2 分拆，明确授权epoch、30s路由窗、5s±1s心跳/15s超时、断开清理；新增已核文档的tokio-tungstenite 0.28 |
| “projection”跨过了二进制RPC和工作区桥                             | sessionBridge.ts 的 encodeChannelRequest/FrameAssembler/ControllerTaskIndexReader；官方 channelClient/serialization/workspace bridge | T2.1 wire独立验收，T2.2桥/任务索引/hello/订阅独立验收；不发明JSON-RPC文本直连                               |
| tasks-index 被写成类似普通只读请求，实际入口不清                  | window-controller频道、onDynamicControllerFrame监听、subscribeControllerV4({topic:"controller/tasks-index",visibility:"foreground"}) | 明确先打开workspace bridge并等Initialize，再监听/订阅；不是tasks-index-request                              |
| 会话subscribe→activate容易误写成新的官方RPC                       | agentConversationTransport.ts 的 activate(subscriptionId) 是本地barrier；sessionBridge.ts:1345起注释及实现                           | 仅ACK后本地放行；没有activateConversationV4方法。收到ACK不等于已有初始snapshot/control                      |
| 只列POST路由，没有handler、RemoteState/应用实例/IPC/退出接线      | 当前services/zcode_control/mod.rs只导出config；commands/zcode_remote.rs只读写链接                                                    | T7.1定义Hub、同一个Arc、IPC/HTTP注册、配对门、SSE吊销、保存删除回调、应用与服务生命周期                     |
| 未明确M11旧门如何变更，可能新UI仍走旧session-send                 | inject/routing.rs:96固定headless_only；mobile/api.ts:540起恒needs_connection                                                         | T7.1旧门改为official_control_required并继续保守拒绝；T7.2新UI走/zcode/actions，测试断言零legacy/CLI回退     |
| 只写“单帧/不重发”，没有多手机/重启去重接口或审计失败策略          | spec §5.2不双发；现有write_audit::record_conn_checked与Codex动作账本模式                                                             | T3预留账本+起始审计同事务；payload hash；发送前持久保守状态，恢复不重放；跨设备ID不泄漏回执                 |
| 将“触发新执行后排除旧id”直接写入生产打断                          | interactionBridge.observeStopTarget和session-probe此限制用于实验只停自身新任务；官方stop payload仅expectedForegroundExecutionId      | T4.2产品按钮停止用户指定的已观察既有执行，不另发trigger；探针保护保留不动；产品语义补fake和实跑             |
| “原thought不存在不得切”误用探针恢复要求                           | selectionBridge目标校验；官方switchModelConfig只要求目标provider/model/thought+CAS；探针原档位检查为恢复清理                         | T6不要求产品恢复原值，允许宿主合法目标；仍拒绝空目录/未知目标。source+fake与live支持范围分别记              |
| 审批把kind与optionId混用                                          | session-probe pickPermissionOption按kind选，返回原optionId；broker按原选项精确查找                                                   | T5按kind=allowOnce/deny白名单，原optionId回传；fullAccess硬拒，不能硬编码按钮标题或ID                       |
| 问答只有模糊“单选”，多选/自由文本不在施工且自动结束snooze时机不清 | buildUserInputAnswer、command.ts:222–258、interaction-broker.ts:623–650,766–780                                                      | T4.1包含三形态代码与分别开闸；首次有效编辑snooze、提交前复核，host撤卡而非本地消费作为终态                  |
| “断线即需重新授权”混淆网络与凭据过期                              | 官方transport网络掉线与AUTH_FAILED/401不同；JWT仅内存                                                                                | T1/T7分离offline/needs_auth；有效内存凭据可显式重连，退出/换链接才清；KICKED零自动重连                      |
| 只有文档中接口表，无方法签名和验收输出                            | writing-plans要求Files/Interfaces/RED/GREEN/Commit                                                                                   | 每任务列确切输入输出、断言代码、命令、PASS条件；补共享DTO、HTTP状态和对外端点合同                           |
| “九项true”可能直接用于生产全开放                                  | contract.capabilityScopes明确ACK-only、审批催卡false、多选/自由文本false                                                             | 新CapabilityView细分证据；T8分code_complete/platform_validated/spec_complete；未证能力零写                  |
| 关闭能力后无法从产品UI补证，可能先填true再测试                    | 原计划没有定义候选能力的验收入口                                                                                                     | T8.2仅debug feature、专用session/代际、120s单次许可；release+feature编译拒绝；通过才更新生产policy          |

补读snapshot.ts:242–297后，还明确了DTO字段：question保留header和options原label/value，freeText来自payload级，permission有summary/toolName；sensitive问答使用密码输入且不入草稿/历史；plan_approval/MCP等未建模子类不得被当普通问答误批准。这里是明确支持边界，不将一个userInput=true泛化为所有交互。

## 官方接口逐项核对

以下路径均相对固定官方仓库；Tuvis 新的Rust/HTTP方法不是官方接口，不以源码存在为名虚构来源。

| 协议/功能               | 源码支撑                                                                                                                                                 | 探针/实跑支撑与边界                                                                                                                     |
| ----------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------- |
| OAuth两通道/交换        | packages/web/src/auth/zaiWebOAuthProvider.ts:168–238；webZaiOAuthConfig.ts:29–66                                                                         | oauthFlow.ts；BigModel回环B1已证，ZAI仅源码/fake，未证则UI禁用                                                                          |
| terminal token/WS/路由  | shared/web-remote-relay/contract.ts:96–133；web transport:104–143,317–331                                                                                | relayAuth.ts；TTL60s、mid带入后matched已证；一次性消费未重放实验                                                                        |
| 心跳                    | shared/web-remote-relay/adapters/heartbeat.ts；domain/heartbeatTiming.ts:1–3；web transport sendPairStatusQuery                                          | 来源为源码，不是历史live新增结论；pair_status_query/client_ts秒，15s无ACK失联；Tuvis选择不自动重连                                      |
| RPC二进制编码与桥       | packages/rpc/src/{serialization,channelClient,channels.shared}.ts；shared/web-remote-control-rpc-transport.ts；webRemoteControlWorkspaceBridgeSession.ts | sessionBridge.ts的合成编码/CRC/ACK/分片及B2已连；rpc identity为bridgeSessionId/可选bridgeGeneration,recoveryId，不是workspaceKey/taskId |
| hello/初始化            | services/zcode-agent/zcodeAgent.ts:881–883；shared/zcode-protocol-v4/transport.ts:42–92                                                                  | helloConversationV4()、initializeConversationV4(clientHello)；wire protocolVersion=3，不能因名v4填4                                     |
| tasks-index             | services/window-controller/windowController.ts:75–78；shared/zcode-protocol-v4/controller.ts；ui/v4/windowControllerTaskListRegistry.ts                  | ControllerTaskIndexReader，主动监听后订阅，B2读119行；标题只展示，精确taskId绑定                                                        |
| 会话帧                  | zcodeAgent.ts subscribeConversationV4/onDynamicConversationFrame；agentConversationTransport.ts:162–291                                                  | ConversationTopicSubscription，ACK前暂存/ACK后本地activate，按topic/subscription/generation过滤                                         |
| 发送                    | zcodeAgent.ts:937；shared/zcode-protocol-v4/command.ts:95–135,343–356                                                                                    | SendOnceLoop；B2 APP可见；sendText免CAS，payload text，不凭“活动帧”断言已完成                                                           |
| 应答/snooze             | command.ts:222–253；CLI interaction-broker.ts:623–650,766–780；官方ElicitationDialog实现的content映射见决策§8                                            | InteractionCommandLoop/buildUserInputAnswer；单选ACK-only；多选/文本source+fake；snooze必须先受理                                       |
| stop                    | command.ts:161–164；snapshot.activeWorks；CLI session-flow.ts:292–360                                                                                    | buildStopPayload；官方可缺守卫，Tuvis收紧为必填。历史live ACK-only，产品打断既有任务需新实跑                                            |
| 模型/档位               | command.ts:254–258,315–340；services/model-provider/providerFacadeServices.ts:95–106；provider/facades.ts:183–205                                        | getView频道model-selection；目标模型自己的reasoningLevel；B4快照已证，CAS必需，仅此等指定命令带baseRevision                             |
| 批准/拒绝               | interaction-broker.ts:185–243；snapshot permission options；command.resolveInteraction                                                                   | 既有卡有批准/拒绝产物；terminal发送催卡负结果；不把guarded称为Tuvis发送保护                                                             |
| 本机持久化/审计/手机API | Tuvis自有设施：database/dao/write_audit.rs；remote/gate/server/SseRegistry；Codex既有装配模式                                                            | 不是ZCode服务端能力；T3/T7/T8必须用fake和生产实机证明集成，开源代码不能代证                                                             |

WebSocket客户端接口另核 [tokio-tungstenite 0.28 connect_async 官方API文档](https://docs.rs/tokio-tungstenite/0.28.0/tokio_tungstenite/fn.connect_async.html)，TLS/connect feature使用该固定版本。本项不宣称最新版本；执行时需cargo编译验证MSRV和锁文件。

## Spec 覆盖和缺口判定

新版计划末尾已逐节映射 §1–§8.1。A包设置、默认值、PIN和读取修复作为现有基线回归；本批补齐官方授权、长驻连接、wire、精确映射、发送、三类问答、审批、打断、模型/档位、统一UI、失败/并发/SSE、审计与文档。没有把生产链再延后为另一个只交计划的批次。

以下是**有任务覆盖但目前不能保证通过**的外部能力门，不是漏排：

- Tuvis/terminal发送触发权限卡：仍有负结果，T5/T8.2保留停止门；未通过不能称完整审批体验交付。
- 单选与打断的宿主终态：历史ACK-only，T4/T8.2需生产实跑确认，不复用旧结论。
- 多选/自由文本、Windows、ZAI、双手机、断线/失效/互踢：源码/部分fake支持或待平台验证，T8.2逐行记录。

这些门不过时，执行者必须列受限功能、保持能力关闭、保留阅读，不自行放宽spec或改MASTER-PLAN。用户可据结果另行裁决缩范围；本计划本身不作该裁决。

## 本次自审与验证范围

已完成writing-plans的五项自审：spec覆盖、步骤粒度、接口/类型一致性、Review Focus归属、篇幅与实现边界。只更改计划和本核查记录；静态检查Markdown、任务结构、路径引用、git diff；不将文档核查说成生产代码或自动门禁已通过。
