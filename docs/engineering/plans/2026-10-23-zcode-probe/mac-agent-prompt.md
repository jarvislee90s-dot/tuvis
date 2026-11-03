# Mac Agent 首轮提示词

完整复制下文发送。提示词可独立执行首轮 P0；如能提供同目录手册与模板则优先读取，缺文件如实说明，不声称已读。

---

请做 ZCode 的 Mac 首轮探测，只做 P0 源码对照与只读取证，不修产品代码。目标是为 Tuvis 的看板恢复、消息发送、审批／问答、打断、新建确定依据。报告后停止，由用户决定下一阶段。

先读项目 AGENTS.md、docs/MASTER-PLAN.md。通常仓库在 /Users/jarvis/Documents/tuvis；核对实际路径、分支、脏状态，不切换用户在用分支。若有 docs/superpowers/plans/2026-10-23-zcode-probe/README.md 和 report-template.md，读后执行；若无，按本提示词首轮契约执行，报告标记未取得手册包。不修改宪法，不启动 Tuvis dev，不碰 usage 采集。

官方源码 https://github.com/zai-org/ZCode ，固定提交 3c5b227649371507ddc4eb46ac5133afe6db1cab。可只读克隆到独立参考目录，不安装依赖、不 build、不运行仓库脚本。README 虽标 3.16.2，同版本不证明同构建。源码先查 apps/zcode-cli/docs/usage/{README,05-permissions,06-sessions}.md、packages/cli/src/{resume,protocol-lifecycle}.ts 和 packages/core/src/permission/service.ts（后三者均相对 apps/zcode-cli）；沿调用点继续找实现与测试，不止读文档。

执行六项，每项必须报告：

- P0-01：macOS／架构、ZCode.app version/build、实际 exe／随包 CLI 路径别名与 SHA256、配置存在性、Tuvis installed version/source SHA；独立 PATH CLI 分列。记录源码与安装包对应证据。
- P0-02：只读有界查 CLI DB 与任务索引，session schema、ID 形态、task_type/parent_id、24h 主会话和归档数、旧 v0.6.1 ID 门接受／拒绝数；新旧 ID 各抽一例，无旧样本写缺测；检查 message/part 顺序、合成条目、后代关系、附件结构，只回传脱敏结构。
- P0-03：沿 DB 候选→parser→adapter→get_all_sessions→本机／远程读取定位边界。记录工具启用、宿主／解析数、隐藏／已读过滤、已有日志和界面。此前 Mac 的 13 候选／0 接受只是历史线索，必须重新采集。模拟 SQL 不是产品出卡，无认证接口则标缺测，不绕过配对。
- P0-04：从源码／包核对参数解析→Electron/CLI 启动→provider 配置→resume→加载→执行→stdout/DB，列失败／退出条件。只运行已从实现确认无副作用的元信息命令。
- P0-05：审批／问答的请求生成→传输→session/turn/request ID→决策→消费，取消入口→实例归属→目标 turn→子任务，分别画调用链。对比 prompt、另起协议服务、APP 原生接口能覆盖什么，不启动服务、不发送决策。
- P0-06：审计主干与 PR #117 留存的 zcode/create/gate/routing 及 L13/L14，分类已交付／待保留／过时／冲突，提出下一最小实验的步骤、预期、负例、隔离和副作用。

禁止 prompt（包括 hi/空串）、新建、发送、审批、打断、app-server 握手、修改权限/recentProjects、重启用户 APP。doctor/help/version 先证实无副作用再考虑；不确认就跳过。真实库只读，需要副本用 SQLite backup API，不能只拷贝主库遗漏 WAL、不强制 checkpoint。近期主会话最多100，每会话消息最多200，不全库扫正文。不输出全量进程参数、环境、配置或凭据。报告用 ID/路径别名，正文不回传。新起协议服务不等于附着 APP；kill 子进程不等于打断 APP；yolo 不等于审批支持。

新建 run_id＝macos＋带时区时间＋短随机码，私有目录 ~/Documents/tuvis-verify-logs/zcode/<run_id>/，内含 raw/、sanitized/、report.md。原始材料留机，不上传。

固定回报 v1，必须包含：
A 身份：schema_version=1、run_id、platform=macos、phase=P0、时间、OS/架构、APP version/build、随包与PATH CLI version/hash、Tuvis installed/source/分支/脏状态、ZCode source SHA、源码包对应性 PROVEN/PARTIAL/UNPROVEN、数据落点别名、是否发生修改/模型调用/写入/进程终止。
B 表格：ID｜对象/作用域｜预期｜实际与最小反例｜结论｜证据类型/编号｜功能/限制。P0-01至06每项一行。结论仅 PASS/FAIL/PARTIAL/BLOCKED/NOT_TESTED/NOT_APPLICABLE。证据 SOURCE/PACKAGE/OBSERVED/FIXTURE/RELAY 单列，纯源码不能报运行PASS。
C 每项证据卡：case_id/attempt_id、前提、假设/反例、可复现步骤/脱敏命令、输出结构/退出码/耗时、计数、固定SHA文件行号链接/调用链/测试、安装包差异、前后对照、raw引用/hash、sanitized附件/hash、尚不能证明什么。
D 能力表：看板状态、正文附件、已有会话发送、无头取消、APP打断、审批展示、审批应答、结构化问答、在册新建，逐项填 平台/入口/指纹｜GO/GO_WITH_LIMITS/NO_GO/NEEDS_EVIDENCE｜用例证据｜缺口。源码推断只填NEEDS_EVIDENCE。P1-P4全是未开放。
E 最多五条主要发现、源码/发行差异、#117移植分类、下一实验及副作用、用户决策项、临时产物/进程收尾、sanitized附件索引。

回传完整 report.md 与必要 sanitized 附件，不传 raw。不替 Windows 填结果，不自行开始后续阶段／实现／推送，不创建 GitHub 工单、不向其他对话发消息。
