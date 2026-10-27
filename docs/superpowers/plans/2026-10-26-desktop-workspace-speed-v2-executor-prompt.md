# 桌面四页与输出速度 V2 执行提示词

请在 `/Users/jarvis/Documents/tuvis` 实施桌面四页导航与输出速度 V2。先读现行 AGENTS.md、docs/MASTER-PLAN.md，以及：

- `docs/superpowers/specs/2026-10-26-desktop-workspace-speed-v2-design.md`
- `docs/superpowers/plans/2026-10-26-desktop-workspace-speed-v2-plan.md`
- `docs/superpowers/evidence/2026-10-26-codex-response-observation.md`
- `docs/superpowers/mockups/2026-10-26-desktop-workspace-v2.html`

使用 superpowers:subagent-driven-development，按 Task 1—13 的依赖与默认顺序执行，每个任务反例先行、实际验证、独立评审、修复和提交后再继续；子代理工具不可用时采用 superpowers:executing-plans。任务实现和评审都读取同一份公共类型/Interfaces，不能各自重命名。最后做一次整分支审查。

实施前检查 Git 与开发应用源根，保留未跟踪评审报告和 tuvis-setup-preview.html。新功能分支 `codex/desktop-workspace-speed-v2` 必须包含已审 TPS 产品基线 `6b9fc9b5` 及当前定稿设计/计划。同步 main 时若上期 TPS 还未合并，保留其实现作为依赖并在报告说明，不能从缺少 TPS 的 main 重新开始或丢弃本树改动。worktree 只在 `/Users/jarvis/Documents/` 下。

配色已经结束讨论：浅色纯白底加原蓝紫强调，深色雾银，轻玻璃只导航/浮层。保留模型用量原排版与折线。草图是结构参考和演示数据，不是应复制的生产代码或数值；优先完成可验证功能，不扩成逐页视觉重造。

核心功能是 WorkBuddy 数值字符串兼容及旧EOF有界恢复、六源双指标、Codex含等待响应吞吐、同窗四页、区间/桶查询及可读图表。Codex一级明确“含等待 · 日志近似”，流式速度/逐响应TTFT不伪造；原生response_id与伴随token_count只记一次，不能用整轮duration−TTFT凑值。

全部测试使用显式影子home和私有库。TUVIS_HOME只隔离账本，不代表所有源根/Hook配置已经隔离；不能据此启动原生dev应用做测试。修改usage共用Rust模块前遵守AGENTS的停dev/完整隔离纪律，不杀其他源根的用户应用，不写真实账本，不改变API/认证/provider/网关，不主动重启Harness，不自动发送测速或正常模型请求。

不重跑暂停的旧全区间计划，不把用户七工具全面探测当开工条件。使用已有真实结构脱敏重放，不能为测试过关把字符串改数字、改掉事件层级、删不变量断言或把后台600秒改成同一时刻。依计划核对旧库/旧游标、重复身份、延迟封口、工具间隔、边缘桶、跨窗导航及隐藏轮询。

完成后写 `docs/superpowers/reports/2026-10-26-desktop-workspace-speed-v2-implementation.md`，返回分支、最终HEAD、逐Task提交、实际命令/退出码/计数、浏览器交互与截图、各来源两种指标的真实支持范围，以及原生Mac/Windows实机、ACL等未运行事项。不得把browser mock、编译或离线结构回放扩大为真实平台全通过。结果交原设计会话review；不要自行合并、推送、发布或打tag。
