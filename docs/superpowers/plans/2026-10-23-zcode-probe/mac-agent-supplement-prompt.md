# Mac Agent 补证提示词（P0-R）

完整转发以下正文；可独立执行。不要重跑首轮 P0，也不要开始 P1。

---

请做 ZCode Mac 只读补证 P0-R，不改产品代码，不发送、不审批、不打断、不启动 app-server、不重启用户 APP、不修改权限或 recentProjects。先读项目 AGENTS.md、docs/MASTER-PLAN.md。

本机手册在 /Users/jarvis/Documents/tuvis-worktree2/docs/superpowers/plans/2026-10-23-zcode-probe/，读 README.md、2026-10-24-p0-review.md、report-template.md。原报告 run_id 为 macos-20261023T2246+0800-acdb。不要切换用户正在工作的仓库分支。

首轮两点已被协调 Agent 修正：

1. 固定官方源码 SHA 3c5b227649371507ddc4eb46ac5133afe6db1cab 已实现审批／问答，不能再依据旧 v1 设计文档判定不存在。入口：
   apps/zcode-cli/packages/bootstrap/src/zcode-protocol/server-operations.ts:3513–3516 注入 createProtocolInteractionBroker；
   同目录 interaction-broker.ts:105 起发送权限请求，:306 有 interactionRequestUserInput；
   bootstrap/src/zcode-protocol-v4/interaction-registry.ts、commands/handlers/interaction-background.ts 实现 v4 resolveInteraction；
   zcode-protocol/server.ts:677 和 server-operations.ts:2702 实现 session/stop。
   这些只证明实现存在，不证明外部能附着 APP 当前回合。
2. #117 最终 head 是 289b20db4d6bdfba50ecfdeb67f1ef6f47ab583f。gate.rs:130–147 已处理 Windows provider env，不能沿用 d5cc28b1 的旧结论。真实 hi 模型回合作自动预检仍不得移植。

本轮集中补三项：
R-01（主要）：沿上述实际实现继续追 APP 的 host/source-server/RPC 如何创建、连接并持有协议实例。找外部客户端能否接入的明确入口、身份认证、单／多客户端限制、session/turn/request 归属及断线生命周期；分别说明“控制 Tuvis 自起协议实例”和“控制已有 APP 活跃回合”。给固定 SHA 调用链和测试。查不到写 NEEDS_EVIDENCE，不宣称不存在；不要发握手或获取凭据。源码研究由 Mac 为主，Windows 只补平台差异，不要求重复完整下载。
R-02：按最终 #117 核对 Mac argv/env/provider 配置及数据目录重定向的完整传播；CLI DB、tasks-index、配置、插件和工具输出是否都隔离。只提出下一轮安全启动骨架及最小测试步骤，不运行 prompt，不假定设置一个环境变量就已隔离。元信息命令只有静态确认无副作用后才可运行，不确定则留缺口。
R-03：现有材料中正文／附件结构与 Tuvis 读取实现是否对齐，列修复后的验收项；无现有授权只读接口就保留未测。看板 ID 根因已确认，不重做计数、不修改 ID、不追溯其来源作为修复前置。可复用旧副本，不再复制 GB 级库。

新 run_id 使用 macos-P0R＋带时区时间＋短码，放 ~/Documents/tuvis-verify-logs/zcode/<run_id>/。raw 留机，只回传脱敏报告及必要附件，不含正文、真实 ID、工作路径、token、配置值。文件名 report.md。
按 v1 模板回报：A 身份与新旧指纹变化；B R-01/R-02/R-03 各一行（预期、实际、反例、结论、SOURCE/PACKAGE/OBSERVED 证据）；C 每项固定 SHA 链接行号、复现步骤、附件/hash、不能证明什么；D 按平台/入口分别填发送、自持实例审批问答/取消、已有APP审批问答/打断的 GO/GO_WITH_LIMITS/NO_GO/NEEDS_EVIDENCE；E 下一最小真实实验及副作用。
源码支持不等于安装版运行通过。报告后停止，不开始实现或 P1，不推送、不发云端工单或跨对话消息。
