# Windows Agent 补证提示词（P0-R）

完整转发以下正文；可独立执行，Windows 没有本机手册包也可按下面契约执行。不要重跑首轮 P0，不开始 P1。

---

请做 ZCode Windows 只读补证 P0-R。先读项目 AGENTS.md、docs/MASTER-PLAN.md，核对实际仓库和分支，不切换用户在用分支。禁止产品改码、prompt（包括 hi/空串）、发送、审批、打断、app-server握手、重启用户APP、改权限/recentProjects。原报告 run_id 为 windows-20261023T2247+0800-1rn60k。

协调 Agent 已确认两个修正：

1. #117 最终 head=289b20db4d6bdfba50ecfdeb67f1ef6f47ab583f，inject/headless/gate.rs:130–147 已在 Windows 推导配置路径存在时设置 provider env。你上轮用 d5cc28b1 得到的“Windows未处理”不能作为最终版结论。无需重复设计该修复。
2. 官方固定源码 3c5b227649371507ddc4eb46ac5133afe6db1cab 存在审批／问答和 stop 实现：
   apps/zcode-cli/packages/bootstrap/src/zcode-protocol/server-operations.ts:3513–3516 注入交互broker；
   同目录 interaction-broker.ts:105 权限请求、:306 输入请求；
   bootstrap/src/zcode-protocol-v4/interaction-registry.ts 及 commands/handlers/interaction-background.ts 处理 resolveInteraction；
   zcode-protocol/server.ts:677 → server-operations.ts:2702 的 session/stop。
   不要把 Tuvis 尚未接入或 prompt 无审批客户端推导成 ZCode 整体不支持。

本轮只补：
R-01：源码完整通路由 Mac 为主研究，你重点核 Windows 安装包和桌面启动链与上述入口的对应：交互方法、v4 resolveInteraction、stop 的结构证据；APP host/source-server/RPC 的实际安装组件、连接归属与多客户端限制的静态证据。不可启动协议服务、猜管道、取token或连接用户活跃回合。字符串命中只报PACKAGE，不报运行PASS。找不到外部附着入口标NEEDS_EVIDENCE，不宣称不存在。
R-02（主要）：按#117最终head核对 Windows argv/env/provider文件存在性，空格/非ASCII路径处理，以及影子数据目录是否贯穿 CLI DB、tasks-index、配置、插件和工具输出；静态核对 Job Object 应只覆盖 Tuvis 自起子树。给下一轮隔离启动骨架、取消测试对象与安全边界，不运行prompt或kill。元信息命令只有确认无副作用后才可跑。不要重复下载完整仓库，也不为“对应性”编译全项目。
R-03：修正报告“Windows 10 / 25H2 / 26200”的命名矛盾，保留可核验 OS build；分开APP版本、包内CLI字符串、DB迁移版本。正文/附件仅补现有结构与读取实现的映射；已有授权只读接口可用才测，无则留给修复后验收。看板ID根因已确认，不重跑整库和首轮全部用例，可复用旧副本。

新 run_id=windows-P0R＋带时区时间＋短码；材料放用户 Documents/tuvis-verify-logs/zcode/<run_id>/，raw/sanitized/report.md 分开。原始值留机，不回传正文、真实ID/工作路径/配置值/token，不输出全量进程参数。P1-P4均未开放。

固定回报v1：A 身份、run_id、platform=windows、phase=P0-R、时间、APP/随包CLI/hash/Tuvis源码身份及变化；B R-01/02/03各一行，字段为对象/预期/实际与反例/结论/证据类型编号/限制；C 每项固定SHA文件行号、步骤、安装包差异、附件/hash、尚不能证明什么；D 按入口区分“协议自持实例”和“已有APP回合”的发送/审批问答/取消打断能力，填 GO/GO_WITH_LIMITS/NO_GO/NEEDS_EVIDENCE；E 下一最小真实实验、隔离、副作用及用户决策项。
结论仅PASS/FAIL/PARTIAL/BLOCKED/NOT_TESTED/NOT_APPLICABLE；证据SOURCE/PACKAGE/OBSERVED单列，源码不能冒充运行PASS。回传report.md与必要sanitized附件后停止，不实现、不开始P1、不push、不发工单或跨对话消息。
