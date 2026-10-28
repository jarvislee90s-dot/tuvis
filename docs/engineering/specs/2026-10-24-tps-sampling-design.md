# TPS短时抽样设计（收口版）

> 2026-10-24。用户已明确：Token区间与TPS区间分离；TPS是短时有效样本，不追求区间全覆盖；查看时提高采样密度，不查看时降低开销。本文取代暂停的全区间性能设计，不授权执行其旧计划。
> 上位依据：docs/MASTER-PLAN.md，不修改项目宪法。证据：[两平台补测复核](../reports/2026-10-24-tps-supplement-assessment.md)。本文是可进入实现的边界，剩余适配调试在对应Task内完成，不再以七源全面重测作为开工条件。

## 1. 产品范围

> **被取代说明（2026-10-26）**：本节的「同窗入口与 TPS 旧规则」（独立「近期TPS」区块、近2分钟/10分钟/1小时/当天查看范围、120 秒滚动点等）已由 [桌面四页导航与输出速度 V2 设计](2026-10-26-desktop-workspace-speed-v2-design.md) 取代——入口收编为主窗口「输出速度」页签，范围/分桶改按 V2 的四档区间与 `(from,to]` 完成时刻归属；本文其余采集/有效性原则仅在与 V2 不冲突处继续有效。

1. Token、费用及原工作活动累计完全沿用已有区间和账本，不因TPS筛选改变。
2. 新增独立“近期TPS”区块，查看范围为近2分钟/10分钟/1小时/当天，默认1小时；不提供多天TPS。横轴是实际采样时间，不是假定该区间每个Token都被测过。
3. 显示最近一次有效完成调用TPS、当前回合有效样本TPS及曲线。四项详情为样本模型用时、样本工具调用用时、样本平均首token、样本TPS；未知时长为空，不借用Token区间的累计冒充样本值。
4. 当前回合按原生turn/interaction/trace/conversation-request身份分组，只有session身份时不称“当前回合”。模型/工具/渠道分别保留，不合成跨模型TPS。
5. 当前回合只统计已取得的有效完成调用。生成中的精确瞬时TPS不在本版承诺内；不将字符估算、回合墙钟吞吐或旧采样值伪装为瞬时速度。
6. 不自动发送测速请求。刷新按钮只读取已有事实；用户正常使用Harness才产生样本。

## 2. 固定采样规则

- Tuvis运行且无可见TPS视图：每600秒读取一次并生成一个采样点；启动后先采一次。
- 复用既有usage.enabled作为用量采集总授权，不新增第二个总开关。关闭时停止TPS调度/接收、返回disabled但不删历史；重新开启启动新runId采样段，不能跨关闭期连线。原生只读来源随总授权开启，插件/遥测仍须用户一次点击安装。
- 至少一个TPS视图可见：每5秒做有界增量读取，每60秒生成曲线点。立即刷新读取并生成一个manual点，最小间隔5秒。
- 前端仅可见时每5秒查询snapshot、每60秒查询points；查询不触发采集。隐藏后两者停止轮询，原生后台调度继续600秒，不与Token/费用queryKey联动。
- 每个点固定选择该点时间之前120秒内**完成**的有效调用，每工具/模型/渠道/计时方法最多取最近20条。单次原生SQL每宿主最多尾200行；文件只读新增部分，不全库COUNT轮询。
- 调用在窗口之前开始但在窗口内完成时，整条纳入。点表示“最近完成调用的抽样吞吐”，不是这120秒内逐Token速率；不裁切、不分摊。这替代旧“整个调用完整落窗”的规则。
- 无有效样本时保存gap状态，不画0，不沿用旧速度制造新点。曲线不填补Tuvis退出期间不存在的采样点，不在缺样本处连成连续实测线。
- 展示最新样本结束时间、距今年龄、有效n和计时来源。最新读数超过10分钟不再当当前值，显示暂无近期样本；历史点仍可查看。
- 点内TPS=`1000×Σ输出Token/Σ生成毫秒`，不平均单请求TPS。不再将相邻采样点均值继续平均为“全量TPS”；各点可能包含重叠调用。
- 每次对近24小时已知series及当前选中series分别产点，包括空窗gap。每次Tuvis启动生成runId；不同runId、显式gap、缺失的计划采样点均断线。后台同runId稀疏点可以相连，但不能插值生成新实测点。
- 视图显隐与采样解耦于费用区间；多窗口通过view_id租约计数，心跳30秒、租约90秒，只有窗口与TPS区块实际可见才续约。后台不保留5秒轮询。
- 事实与点只保留最近24小时，按结束时间/采样时间清理；不作90天性能回填，不变更旧用量保留设置。原生落盘可恢复最近事实，恢复后只从当前时间产生新采样点。

## 3. 有效性与口径

请求成功、输出>0、生成时间>0、计数和时间同次调用，才入TPS。失败/取消不入速度；有可靠起止仍可进入样本耗时详情。能识别而不能对齐的重试排除；不知道的内部重试明确标来源限制，不能谎称零重试。

每点TPS取候选中最近20个有效调用；耗时详情另按同一读取批次、同120秒结束窗口内有可靠时间的候选汇总，包含失败/取消。candidateCount、timedModelCount、timedToolCount、failedCount、cancelledCount、missingTimingCount分别保存并显示；不得把它们都叫TPS有效n，也不声称样本模型时间等于TPS分母或全区间模型时间。失败缺计时为缺失，不填0。

不设置全局1秒最小生成窗。合法原生小窗不因短而剔除；少量Token标小样本，辅助标题/自动摘要与主任务分别标purpose，默认不进入主任务曲线。只收到一块完整响应或无可靠生成阶段时不使用整个请求时间代替。

供应商输出已含思考时不再相加。互斥output/reasoning仅在生成窗覆盖两者时相加；无单列思考时保留provider-reported及“思考计数未单独声明”，不按字符估思考Token，也不宣称已测得隐藏思考的独立速度。

计时方法固定stream-observed/client-native/native-event-approx。不同方法不合并成一条连续精确曲线；tooltip显示方法，原生事件近似须有“原生近似”标记。全部都是端侧体验/原生阶段采样，不称服务器纯解码基准。

## 4. 首版来源与明确缺口

| 来源      | 采用路径及规则                                                                                                                                                                                                             | 首版交付                                                                                                               |
| --------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------- |
| ZCode     | 原生model_usage首点至completed，成功/retry_count=0/正窗；缺F的工具参数响应跳过。标首点仅正文/思考，不推造参数首点                                                                                                          | 读取有效子集；最新/当前回合/采样点；不承诺生成中数据                                                                   |
| DSH       | 原始step+最终成功消息，展开紧凑流首非空思考/正文/参数至结算，同消息outputTokens。可定位最后成功尝试则用其起点，否则剔除重试速度；缓存只对账                                                                                | 现有落盘路径，跨平台；不是只认旧稿的单一replay profile                                                                 |
| Kimi      | 同step原生usage+llmFirstTokenLatencyMs/llmStreamDurationMs；支持嵌套事件，排除已知失败/取消/无法对齐重试，记录SDK内部重试限制                                                                                              | client-native近似，provider-reported输出；不因缺逐chunk轨迹整体禁用                                                    |
| WorkBuddy | qua.end同请求completion_tokens/(total_cost_ms−ttft_ms)，核对成功finish、model/request身份；不加内容侧估算Token                                                                                                             | Mac5.8.3正常路线已证。Windows正常路线需联到同请求数值usage，缺失时该路线返回insufficient-evidence，不能借旧CLI数据凑值 |
| Claude    | 官方llm_request span、attempt=1、success及output齐全；首点采用first_content_ms（内容块开始）并标native-event-approx；不得称首非空delta。生成窗duration−first_content，保留原生ttft_ms作诊断；缺first_content不借回合值补齐 | 一次统计配置后接入本机OTLP JSON。按interaction/trace分回合；Windows fallback结果不代表原provider速度                   |
| OpenCode  | 最小server观察插件。Task内按实际宿主SDK确定Effect Stream或AsyncIterable消费，修复迭代/静默吞错/丢关联ID，分别保存session/message/ordinal/event/attempt键。开始、非空delta、结束tokens同键，累计usage不重复入账             | 正式扩展Task，借鉴开源事件分派/生命周期。至少一条真实正例通过才标支持；验证失败仅降级此源，不阻塞原生来源交付          |
| Codex     | metrics管道已通，但当前没有同调用可靠生成窗                                                                                                                                                                                | 本版明确不提供TPS；不得用turn duration−turn TTFT、item窗口或不对齐histogram凑值。仍保留原用量能力，来源状态说明缺口    |

首批有依据的路径是DSH/ZCode/Kimi、Mac WorkBuddy及Claude原生近似。不是宣称七家全支持。OpenCode适配是正常实现调试，不另开用户全面探测；Codex缺口单列，不拖住整个版本，也不暗改公式。

## 5. 一次配置与运行

用户一次点击启用，Tuvis自动完成自有统计项，原生信任界面由用户点击；提示“下次启动生效”，不主动关闭Harness，不要求手写配置。API、认证、provider、网关不变。

保留现有Hook/插件/统计出口。已有相同信号目标则显示冲突，默认保留，不覆盖；用户明确点击转交统计目标后只更新该统计键。卸载仅处理仍匹配安装值的自有项：新增键删除，转交替换键恢复旧统计值；用户后改项保留，禁止整文件备份覆盖恢复。

转交必须由apply的choice=transfer-statistics-target显式表达，默认choice=append-only。安装记录逐键保存原先是否存在、原统计值、安装值及指纹；卸载时当前值仍匹配安装值才恢复原统计值，原先不存在才删除；用户后改则保留并报冲突。记录放用户私有目录，Unix 0600/Windows用户私有ACL，不进日志/报告；preview对敏感统计header只给脱敏值/指纹。

Claude接收器只bind127.0.0.1，默认固定端口47181，首次占用则在47182—47190中选择并持久保存；以后端口冲突报状态，不杀进程或擅自换端口使旧配置失效。OTLP仅JSON、路径/v1/traces，1MiB上限、独立随机Bearer凭证，允许清单字段；不为此启用正文、raw body或工具参数上报。Tuvis真正退出就停止接收，不增系统守护进程。

OpenCode插件仅观察、取数字后丢正文；不必TUI插件，不引入整套监控平台。可在Harness内落本地最小事实，Tuvis退出后也能回填；采集端离线或磁盘失败必须有界失败且不阻塞Harness。

## 6. 固定工程契约

新增services/tps域，不改变旧usage Token账本、水位、采集间隔或费用计算。旧区间继续工作；TPS有独立状态和查询。

三张表：tps_observation（主键source+host_instance+stable_call_key，原生S/F/end、native duration、generation_ms、output、reasoning包含关系、model/channel/session/turn/purpose/outcome/method、失败原因）；tps_point（主键tick_id+series_key，sampled_at/window_ms=120000、n、sum_output/sum_generation_ms/ttft_sum/count、latest_ended_at、quality、sampled_keys）；tps_state（key+JSON，源游标/安装记录/调度元数据）。不要保存每token正文，也不复用旧usage_cursor。

series_key包含tool、原始model/provider、channel、method。原生数据只读并尊重WAL；事实UPSERT，点引用稳定键不重复计数，同一调用进入不同点允许，但一个点内只一次。单批最多200条。查询不发模型请求，不运行完整旧采集。采样与旧读取同时发生时复用已有解析/缓存，禁止为新图重复全量解压历史。

IPC：tps_snapshot(selection)、tps_points(selection,view_range)、tps_refresh(selection)、tps_view_heartbeat(view_id,visible)、tps_install_preview(tool)、tps_install_apply(preview_id,choice)、tps_uninstall(tool)。selection为tool/model/channel/method过滤，view_range仅2m/10m/1h/today；安装preview含目标项/原值指纹/自有项，apply时比较当前值与预览指纹，预览后再变更则拒绝；用户已选transfer且指纹未变时仅替换已授权统计键。

返回sourceStates、method、sampleCount、latestEndedAt、ageMs及空值原因。当前回合必须有原生turn/trace，缺键返回unknown-turn；不能按session全部请求当一轮。模型/工具用时只显示样本累计，不宣称全区间完整；工具归属不唯一为null，不填0。

公共类型固定如下，Rust snake_case/IPC camelCase镜像：TpsSelection={toolId?,modelId?,channelKey?,method?}；TpsSnapshot={series:TpsSeriesSnapshot[],sourceStates:SourceState[]}；TpsSeriesSnapshot={seriesKey,lastCall:TpsMetrics|null,currentTurn:TpsMetrics|null}；TpsMetrics={modelMs?,toolMs?,avgFirstTokenMs?,tps?,sampleCount,ttftSampleCount,candidateCount,timedModelCount,timedToolCount,failedCount,cancelledCount,missingTimingCount,latestEndedAt?,ageMs?,method,outputScope,quality}（问号字段响应显式null）。TTFT只平均成功且首点可对齐的样本，分母ttftSampleCount独立于TPS的sampleCount。TpsPoint包含上述TpsMetrics以及sumOutputTokens/sumGenerationMs/ttftSumMs/pointId/seriesKey/sampledAt/windowMs/runId/trigger(startup|scheduled|manual)/status(value|gap)。最近一轮取同series最近有效调用的原生turnKey及其已采样完成部分；没有进行中状态证据时称“最近一轮”，不自称正在生成。

来源读取统一返回ReadBatch={observations,sourceState,diagnostics,nextCursor}，入参cursor只读；sourceState始终返回，即使无数据或缺计时。事实与nextCursor在同事务提交后才推进；读取失败不改变水位。collector预算失败只影响该来源，不抹去其他series。

## 7. 验收与收口

- Token区间从当日切到30天，TPS曲线/观察范围不变；TPS范围改变不触发Token/费用重算。
- 后台1小时最多6个计划点（启动/manual点另有类型），可见状态60秒点、5秒有界读取；用假时钟测试，不真等1小时。视图租约过期恢复后台频率。
- 每点最多20个有效调用，Σ/Σ正确；失败/取消/无F不贡献Token和生成时间。空窗gap，旧点不可冒充当前；同调用重复读取不翻倍。
- 一次调用跨窗口起点仍可在其完成后被采样；不切分Token。退出期间没有实时点，恢复只补可得事实。
- 原生来源用已取得真实结构样本做离线回放和影子存储验收，无需用户重跑七工具。WorkBuddyWindows/OC的缺口只阻断对应适配，不阻断其他源。
- OpenCode：合法AsyncIterable无next也能消费；Effect/Promise接口按锁定宿主核对；订阅错误有诊断，两会话交错不串键；真实正常宿主一条开始→delta→结束为该适配验收，不以loaded充数。复制开源代码锁commit并保留真实许可证。
- 两平台最后只验Token/TPS解耦、短窗出点/空窗、成功/取消、窗口显隐开销、一次配置确认及自然重启，不再开展全面字段搜寻。
- 不修改MASTER-PLAN、不发布、不打tag、不自动合并。实现前同步主分支；同步失败停止代码实施但保留文档，不悄悄在旧基线上开发。
