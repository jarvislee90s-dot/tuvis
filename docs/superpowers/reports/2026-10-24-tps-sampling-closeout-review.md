# TPS抽样收口版一致性检查

2026-10-24。用户最新目标是Token区间与TPS抽样分离，允许一小时6个稀疏点、按需增加读取，取有效数据即可。此次不是新一轮七源探测，也没有运行产品代码/模型请求。

- 当前spec：../specs/2026-10-24-tps-sampling-design.md
- 当前计划：../plans/2026-10-24-tps-sampling-plan.md
- 当前执行交接：../plans/2026-10-24-tps-sampling-executor-prompt.md
- Mac/Windows：../probes/2026-10-24-tps-closeout-macos.md、../probes/2026-10-24-tps-closeout-windows.md（实现后验收，非编码前置）

## 收口范围

固定120秒机会样本，每series最多20个有效完成调用；后台600秒点、可见60秒点/5秒读取。范围只近2m/10m/1h/today、默认1h；24h保留。点稀疏、不补0或假实测，不宣称全区间覆盖和服务端基准。

来源以已有证据推进DSH/ZCode/Kimi/Mac WorkBuddy/Claude原生近似；Windows WorkBuddy正常路由缺数值usage、OpenCode适配不通过、Codex无合格生成窗均如实列状态。未知思考计数/原生首事件近似留方法标签，不为凑值混入回合时长。

## 独立审查与修复

子代理首审提出三项工程缺口，全部已修复并复核通过：

1. ReadBatch统一携带observations/sourceState/diagnostics/nextCursor；入参cursor只读，事实及nextCursor原子落库后推进。
2. build_point接已知/选中series及runId；无数据也有gap，新runId/缺失计划点断线，不能跳过空窗后伪连。
3. 安装apply显式携带append-only/transfer-statistics-target选择；收据逐键保存oldExists/oldValue/installedValue；卸载只对仍匹配安装值的键恢复旧统计值或删除新增值，用户后改保留，原模型配置不动。

## 作者自查

- Task1定义模型、ReadBatch、SeriesKey、gap与快照，Task2提供三表原子持久化，Task3/4提供原生来源，Task5/6对应增强入口，Task7调度IPC，Task8独立图表，Task9验收与文档。
- DAG无环；Task6未通过不成为原生调度的编译前置，不能因此要求用户再全面探测。Task6是否真实支持必须有正例，单纯loaded不算。
- 时间指标的有效n独立：TPS最近20有效，耗时详情可以包含同读取批次可靠失败/取消；candidate/TTFT/TPS/时长计数分别给出，不声称与费用Token同范围。
- 复用usage.enabled总授权；关闭停调度和receiver，重开新runId；快照/点查询不触发模型请求或旧用量采集。
- 私有统计配置与卸载、端口冲突、空窗和样本年龄都有确定规则，不把旧暂停文档交给执行者。
- 每Task含功能、Files、Interfaces、红绿检查与独立验收；公式、去重、资源节奏、身份、缺口、双语和旧账不变均有归属测试。

文档审查通过不等于产品实现或所有来源验收通过。当前可以进入实现；剩余版本适配在任务中调试，不再以用户提交新一批全平台报告为前提。
