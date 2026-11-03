# Windows平台收口验收提示词（不是新一轮全面探测）

读取仓库TPS抽样新spec和计划：docs/engineering/specs/2026-10-24-tps-sampling-design.md、docs/engineering/plans/2026-10-24-tps-sampling-plan.md。用于实现后的Windows验收；若尚未实现，只据既有报告整理可用样本，不重跑七源库存，不复用旧暂停计划。Windows/WSL与CLI/桌面宿主分清。

1. 用假时钟验证后台600秒采点（1小时6计划点）、可见60秒采点/5秒有界读取、lease过期恢复后台；Token区间和TPS2m/10m/1h/today状态互不影响，不等真实1小时。
2. 每点前120秒完成的有效调用最多20条；ΣToken/Σ生成时间。无样本gap，不画0或旧速度；n/来源/age可见。跨开始边界可整条入样，不按比例切Token；失败取消不进速度。
3. 复用现成DSH/ZCode/Kimi真实结构样本。ZCode缺F行排除；DSH取首非空思考/正文/参数；Kimi原生806/776ms合法窗不套1秒阈值。原Token账本不变。
4. Claude只验原provider正常路径的统计入口，不换API/Key获取数字；近似first_content方法需标明，fallback实验不代表正常路线。Codex管道已通但无合格生成窗时显示缺口，不使用turn duration−turn TTFT或不对齐histogram画假TPS。
5. WorkBuddy正常桌面只有时刻无Token时返回insufficient-evidence，不借旧CLI样本、不过度承诺所有路由；同请求联到最终usage可在对应Task补齐。OpenCode修观察器/SDK适配属于Task6，不要求用户另跑全面探测；只有loaded不算事件闭环。
6. 新真实操作最多观察用户一次正常任务、一次自然取消；其余用影子账本和脱敏fixture验收。配置仅自有统计项、用户点击确认、自然重启，API/provider/认证不动。不主动关闭工具、不注入、不写真实账本，不覆盖用户配置。

返回≤1000字收口结果：版本/宿主/构建HEAD，哪些源通过或保持缺口，采样和独立区间断言结果，一次n/Token/生成ms/TPS/age，未完成事项。未测不伪装通过，也不再把未测变成七工具重测前置。
