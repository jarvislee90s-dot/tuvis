# TPS抽样版执行提示词

在Tuvis仓库先读当前AGENTS.md、MASTER-PLAN.md、新spec `docs/superpowers/specs/2026-10-24-tps-sampling-design.md`及新计划`docs/superpowers/plans/2026-10-24-tps-sampling-plan.md`。旧model-speed-statistics计划和提示词已被替代，不交叉执行。

使用superpowers按Task1—9及其依赖实现。用户已明确Token全量区间与TPS机会抽样分开；后台600秒、查看60秒采点/5秒增量，点回看120秒且每series最多20个有效完成调用，保留24小时，gap不补0，不追求所有调用或生成中精确瞬时。

不再把七工具全面实测作为开工条件。已有原生ZCode/DSH/Kimi/Mac WorkBuddy及Claude近似计时用脱敏fixture回放。OpenCode修迭代/SDK/关联键是Task6正常调试，可参考开源订阅和事件状态机但保留真实许可证；无正例不能标支持，不拖住原生来源交付。Codex当前无合格生成窗，明确缺口，不用turn−TTFT凑值。

修改源码前同步main、保留用户改动，并停dev或隔离全部源根与账本。统计设置只能点击授权后维护自己的部分，不改API/Key/provider/网关，不主动重启、不向工作会话注入，不增系统守护进程。总授权复用usage.enabled；Token账本、费用和原游标不改。不要先安装整套Grafana或反复要求用户重跑所有Harness。

每Task红绿验证、独立提交、如实报告未通过来源。Task9用假时钟和影子账本验采样节奏/独立区间，两个系统各最多一轮窄范围自然任务验收，无法运行就写缺口，不宣称全部支持。交付`docs/superpowers/reports/2026-10-24-tps-sampling-implementation.md`，列HEAD/提交/命令/实际结果/平台源支持与限制，返回原设计会话review；不发布、不合并。
