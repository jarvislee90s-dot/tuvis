# 模型速度统计：设计与计划一致性检查

日期：2026-10-24。范围仅文档；未实施产品代码，未运行产品测试或主动模型实测。

## 交付

- [正式设计](../specs/2026-10-24-model-speed-statistics-design.md)
- [逐任务计划](../plans/2026-10-24-model-speed-statistics-plan.md)
- [执行提示词](../plans/2026-10-24-model-speed-executor-prompt.md)
- [逐源证据](../evidence/2026-10-24-model-speed/README.md)

## 作者自查

1. Spec覆盖：§1—7逐项映射到Task1—10，矩阵位于计划末尾；产品范围、公式、源门控、区间、保留期、UI与双语均有归属任务。
2. 步骤：每Task有Files、Interfaces、失败测试名/断言、红绿命令、实现约束和独立验收/提交。未将测试骨架写成完整实现。
3. 类型：Task1定义公共值对象及PerformanceAccumulator，Task7复用；Task8的PerformanceToolModel完整携带源/回填/查询态，Task9消费同一类型；camelCase遵守后端序列化。
4. Review Focus：回填旧账重放、DSH profile、范围/保留、未知/无费用、工具边界五类风险均在归属Task明确测试。
5. 比例：计划用接口和断言固定决定，不复制产品函数体；单一功能域采用一份计划，无需独立发布的多子项目拆分。
6. 依赖：1→2，1→3/4/5，2+3+4+5→6，1+2+6→7，7→8，8→9，6+7+8+9→10。无环，默认序号顺序满足前置条件。
7. 路径/命令：核对已有usage测试target、support::open_ledger_db、查询/API/UI路径。计划中的新文件是待创建交付，不误称已存在；实际产品测试只在执行阶段运行。

## 独立审查与修复

设计复审发现DSH TPS缺少明确总输出scope门槛。已补安装包mapUsage与持久replay profile证据，固定只准入pi-ai/2/openai-completions且source一致的记录；其他profile为空。独立设计复审通过。

计划审查发现两处遗漏：

- 保留期扩大后EOF游标不会自动重扫：Spec及Task6增加coveredFromMs/backfillRevision/unrecoverableRanges状态迁移，30→7→30包含源存在/源已删两支验收，仅回放性能。
- attachPerformance/Grid丢失sourceStatuses：Task8改为消费完整PerformanceResult及queryState，Task9接PerformanceToolModel；增加rows=[]下不支持、失败、无源、回填中及查询失败测试。

独立计划复审已确认两项修复，未发现仍须修改的spec/接口/依赖问题。用户后续收到执行结果后，由原设计会话检查真实diff、提交与测试证据；此处通过不等于产品实现或兼容认证通过。
