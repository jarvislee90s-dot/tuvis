# worktree2 子代理执行提示词

将以下正文完整发给负责执行的主 Agent。本提示词授权按现有设计／计划实施，不授权绕过官方认证、向未指定生产会话发送／审批、推送／合并／发版。主 Agent 负责编排，使用新鲜子代理完成单 task，实现后由独立子代理审阅。

---

你负责在 /Users/jarvis/Documents/tuvis-worktree2 执行远程设置与 ZCode 项目，执行方式已经指定：superpowers:subagent-driven-development。不要把全部任务塞给单个子代理，不让主会话承担全部实现上下文。配合 test-driven-development、verification-before-completion、requesting-code-review；遵守本机 AGENTS.md。这里的工作目录是用户明确指定，覆盖项目默认 /Users/jarvis/Documents/tuvis。

一、先安全建立执行分支

1. 先记录 pwd、git status --short、当前branch、HEAD，并冻结文档引用：git rev-parse codex/zcode-probe-handbook。该分支的最新文档提交包含本提示词、设计和A/B计划。
2. git fetch origin main。源码必须从此时最新 origin/main 开始，不直接从旧研究分支开代码分支。
3. 工作树有未提交内容时先识别归属；保留用户已有改动。禁止 reset --hard、clean -fdx、丢弃／覆盖未跟踪文档；不要擅自stash他人的工作。若文档已作为提交保存，进入下一步；否则只归档本任务文档或先报告具体冲突，不能带着不明脏文件切分支。
4. 在此目录建 codex/zcode-unified-remote 功能分支（若同名已存在，先读其log和进度，不重建／覆盖）。从最新origin/main创建后，cherry-pick第1步冻结的文档提交；发生文档冲突时三方合并，保留新main与本设计，不整文件倒退。记录 base SHA、docs SHA 和最终HEAD。
5. 不新建 ~/.codex/worktrees，不切到主目录，不在main直接写代码。此目录已经由用户指定为执行区，不删除／归档这个checkout。

二、必须读取的当前文件

- docs/MASTER-PLAN.md：最高依据，不修改。
- docs/superpowers/specs/2026-10-24-zcode-unified-remote-design.md
- docs/superpowers/plans/2026-10-24-zcode-unified-remote-master-plan.md
- docs/superpowers/plans/2026-10-24-remote-settings-and-zcode-read-recovery.md
- docs/superpowers/plans/2026-10-24-zcode-official-bridge-validation.md
- docs/superpowers/reviews/2026-10-24-zcode-unified-remote-review.md

旧“仅打开官方页面”和旧ZCode CLI原生移植方案不是本轮路线。共享多工具历史文档仅追溯，不重新执行。不要重复全部P0，也不重测用户已经长期验证的官方页面功能。

三、子代理组织与上下文恢复

- 按技能脚本为每份实际执行的计划创建自己的 SDD workspace／progress.md；总协调日志另记录当前task、子计划ledger路径、依赖、SHA和等待项。
- 主 Agent 每次只给子代理该task全文、相关设计条款、必要文件／接口、当前base/head。使用不继承历史的上下文（fork_turns=none），不要转发整个对话／真实DB／全量原始日志。
- 一个task一个新实现者；完成后独立新审阅者检查规格符合与代码质量，问题修好再进入依赖task。模型选择按技能和当前可用模型执行，不虚构模型。
- 默认串行实现，不并行改RemoteSection、remote/mod.rs、api.rs等共享文件；无共享状态的只读研究才可并行。
- 每task先有针对性红测、最小实现、相关验证、自检和提交；审阅包带本task base/head和准确测试结果，不把上一task结果当当前结果。
- 完成后更新ledger再勾计划。不要每task问用户“是否继续”；技术细节可按设计自行决定并记Ruling。未解决的官方授权／破坏性动作／工作区外敏感副作用按规定停，不以自主裁决绕过。
- 主会话压缩后，先读ledger、git log和剩余task，再派发；禁止从A0重新做一遍。完成状态应区分 implemented／locally_verified／awaiting_platform_validation／awaiting_auth／complete，不把待验项勾成完成。
- 全分支结束再做一次独立整体review。保留源码checkout和有用ledger，不清理用户工作区。

四、首次执行范围与连续推进规则

首先完成A0—A7的编码／相关测试，执行A8的本机检查及可用平台／手机验收。同时尽早推进B0和B1的源码／fake／身份路径核对，避免等UI完工才发现授权不能接入。B0需要的测试目标未指定时，先做不涉及真实写的部分。

若A8要求的Windows／真实手机验收在当前机器无法完成，明确留下等待项；可以继续独立的B0/B1源码／fake任务，不能伪造另一平台PASS或擅自开新对话联系其他Agent。

B1认证门：

- 用户已经提供过官方远控链接，可用于本机配置与用户授权范围内的只读连接准备；先确认当前链接仍有效。历史材料里有两个月旧链接Not Found，不能直接拿那条当有效值。
- 不在新提示词、仓库、日志重复抄写真实链接。优先读取用户已经保存的本机ZCode配置；未保存且执行Agent上下文没有有效链接时，只请求用户粘贴一次新链接。
- 链接SID不是JWT，Tuvis配对cookie不是ZCode授权。若有官方允许且已授权的身份复用路径，直接复用，不让用户无意义重复扫码。
- 若实际流程仍要求本人登录／扫码／设备批准，只在该实际节点给用户一个具体操作说明并停等；不能提取其他应用Cookie库、ZCode账号文件、进程内存或日志里的token来跳过。
- 不默认假设官方提供第三方OAuth。找不到合法授权通路时，按B1输出unsupported与证据；保留A包成果，不把外跳冒充统一控制已完成。
- 同SID/role owner可能冲突，不能自动无限互踢／重连。

B1通过、测试目标及真实动作授权明确后，连续执行B2—B4，按实际合同完成B5，不再逐task请求是否继续。若尚无用户指定的测试会话或动作范围，先完成fake／源码部分，只在真实动作前集中请求一次必要授权。

B2—B4涉及真实发送、审批、问答、APP原生打断、模型／思考强度切换，只在用户指定的专用测试会话。链接授权不自动包含操作任意生产会话。Unknown不重发，不用full-access替代单次审批，不kill用户APP或其他Agent。

B5的交付是“基于实际认证／wire合同的生产统一控制实施计划”，不是全套生产控制已经编码完成。B5完成后提交该计划和证据交用户确认；本提示词不授权自动执行B5新生成的后续计划。不要把A0—A8+B0—B5全打勾解释成发送／审批UI已全部上线。

五、不可改变的产品合同

- 设置顺序：通用→四通道→访问与安全（含设备列表）→工具特殊配置→无头配置→注入审计。
- PIN初始遮罩，眼睛只控制显示，失焦／重入隐藏，不改实际密码／设备配对。
- H3和Codex兼容发送缺键默认true；显式false保留；非法值false；资格守卫／目标／pending和Unknown规则不放开。
- 工具特殊配置只有ZCode折叠卡；链接按Tuvis主机保存；未联机／失效只在链接框与发送控制区提示，不影响正文／文件。
- 读取接受 sess_＋严格UUID 和 session_＋32hex，保留原ID；子代理、parent、归档／删除、窗口、隐藏／已读不被绕过。
- 统一控制走官方协议适配；不做隐藏浏览器DOM自动点击，不移植#117真实hi自动探针。
- 用户已验证官方页面功能，新的验证是独立客户端／精确归属／回执闭环，不再泛化重测官方网页。
- 新文案中英同步；行为变化写升级须知；不提前打tag、不改MASTER-PLAN。

六、结束／需要用户时的回报

正常结束给：分支、base/docs/head SHA；A/B每task实现／审阅／测试／待验表；变更文件、已提交列表、真实实验授权与结果、剩余工作、后续计划位置。

阻塞时给：已完成task与SHA、唯一实际阻塞点、用户此刻要做的最少动作、做完从哪个task/step恢复。不要只说“需要扫码”或“等用户确认”，也不要抛弃可独立完成的A包工作。

不push、不合并、不发版、不发GitHub工单、不向其他聊天发消息，除非用户另行明确授权。
