# Windows Agent 首轮提示词

完整复制下文发送。提示词可独立执行首轮 P0；如能提供同目录手册与模板则优先读取，缺文件如实说明，不声称已读。

---

请做 ZCode 的 Windows 首轮探测，只做 P0 源码对照与只读取证，不修产品代码。目标是为 Tuvis 看板恢复、消息发送、审批／问答、打断、新建确定依据。报告后停止，由用户决定下一阶段。

先读项目 AGENTS.md、docs/MASTER-PLAN.md。按当前 Windows 项目发现实际仓库路径，不照抄 Mac 路径；核对分支和脏状态，不切换用户在用分支。若有 docs/superpowers/plans/2026-10-23-zcode-probe/README.md 和 report-template.md，读后执行；若无，按本提示词首轮契约执行，标记未取得手册包。不修改宪法、不启动 Tuvis dev、不碰 usage 采集。

官方源码 https://github.com/zai-org/ZCode ，固定提交 3c5b227649371507ddc4eb46ac5133afe6db1cab。可只读克隆到独立参考目录，不安装依赖、不 build、不运行仓库脚本。同版本号不证明同构建。先读 apps/zcode-cli/docs/usage/{README,05-permissions,06-sessions}.md、packages/cli/src/{resume,protocol-lifecycle}.ts、packages/core/src/permission/service.ts（后三者均相对 apps/zcode-cli），继续追实现调用与测试。

六项逐一执行：

- P0-01：Windows build/架构、ZCode 安装版 ProductVersion/FileVersion、实际 exe/随包 CLI 路径别名与 SHA256、配置存在性、Tuvis installed version/source SHA；独立 PATH CLI 分列。记录源码与安装包对应性证据。
- P0-02：只读有界查 CLI DB、任务索引、session schema、ID 形态、task_type/parent_id、24h 主会话及归档数、旧 v0.6.1 ID 门接受/拒绝数；新旧ID各抽一例，无旧样本写缺测；消息/part顺序、合成条目、后代关联、附件只回传结构。不能把 Mac 的13/0统计复制成 Windows 实测。
- P0-03：DB候选→parser→adapter→get_all_sessions→本机/远程读取逐层取证；工具启用、宿主/解析数、隐藏/已读过滤、日志和界面分列。无现有认证接口写缺测，模拟SQL不是产品出卡，不绕过配对。
- P0-04：源码/安装包核对参数→Electron/CLI→provider配置→resume→加载执行→stdout/DB，Windows argv 引号、空格和非ASCII路径的静态处理一并记录。只运行经证明无副作用的元信息命令。
- P0-05：审批/问答请求生成→传输→session/turn/request标识→决策→消费；取消入口→实例归属→目标turn→子任务。对比prompt、另起协议进程和APP原生通道，不启动服务、不发送决策。记录后续需要验证的 Job Object/子进程树行为。
- P0-06：审计主干与#117留存的zcode/create/gate/routing和L13/L14，分类已交付/待保留/过时/冲突，列下一最小实验、预期、负例、隔离与副作用。

ZCode桌面是APP/无头面，不套 WriteConsoleInput/按键/屏读方法；若发现独立TUI任务，登记并另按项目win-console-inject-probe技能立项，不混入本轮。

禁止prompt（包括hi/空串）、新建/发送/审批/打断、app-server握手、改权限/recentProjects、重启用户APP。doctor/help/version先从源码确认无副作用，不确定则跳过。库只读，副本用SQLite backup API，不只拷贝主库遗漏WAL、不强制checkpoint。主会话最多100，每会话消息最多200，不全库扫正文。Get-CimInstance/进程参数可能泄露token，只采白名单；不输出环境/配置值。正文、ID和真实工作路径用结构及别名回报。新起协议服务≠附着APP；kill子进程≠APP原生打断；yolo≠审批支持。

run_id＝windows＋带时区时间＋短随机码。材料放 $env:USERPROFILE\Documents\tuvis-verify-logs\zcode\<run_id>\，内含raw、sanitized、report.md。raw留机，不上传。

固定回报 v1：
A 身份：schema_version=1、run_id、platform=windows、phase=P0、时间、OS/架构、APP version/build、随包/PATH CLI version/hash、Tuvis installed/source/分支/脏状态、ZCode source SHA、源码包对应性 PROVEN/PARTIAL/UNPROVEN、数据落点别名、是否发生修改/模型调用/写入/进程终止。
B 表格：ID｜对象/作用域｜预期｜实际与最小反例｜结论｜证据类型/编号｜功能/限制。P0-01至06每项一行。结论仅PASS/FAIL/PARTIAL/BLOCKED/NOT_TESTED/NOT_APPLICABLE；证据SOURCE/PACKAGE/OBSERVED/FIXTURE/RELAY单列，纯源码不能算运行PASS。
C 每项证据卡：case_id/attempt_id、前提、假设/反例、步骤/脱敏命令、输出结构/退出码/耗时、计数、固定SHA文件行号链接/调用链/测试、安装包差异、前后对照、raw编号/hash、sanitized附件/hash、尚不能证明什么。
D 能力表：看板状态、正文附件、已有会话发送、无头取消、APP打断、审批展示、审批应答、结构化问答、在册新建。各填 平台/入口/指纹｜GO/GO_WITH_LIMITS/NO_GO/NEEDS_EVIDENCE｜用例证据｜缺口。源码推断填NEEDS_EVIDENCE。P1-P4全未开放。
E 最多五条发现、源码/发行差异、#117移植分类、下一实验及副作用、用户决策项、临时产物/进程收尾、sanitized索引。

回传完整report.md及必要sanitized附件，不传raw。不替Mac填结果，不自行开放后续阶段/实现/推送，不创建GitHub工单、不向其他对话发消息。
