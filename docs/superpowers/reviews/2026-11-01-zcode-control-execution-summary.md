# ZCode 远程控制施工账本

日期：2026-11-01。施工进行中；本文不代表完整验收。

## G0 继承现场

- 原 HEAD：`4c4050ec868e390a42bd7cd54fb338eca8f5ad0b`；分支 `codex/zcode-unified-remote`。原现场有 20 个已跟踪文件改动和 10 个未跟踪文档；`git diff --stat` 为 951 行新增、512 行删除（不含未跟踪文件）。
- `8034e646` 单独冻结已审 W6 四项修正、相关测试和手册，以及此前三个设计文档；精确文件清单见 `git show --stat 8034e646`。不将其称作本轮功能实现。
- `9f93d629` 冻结本轮索引、三个子计划、需求矩阵、设计和 R10 静态证据；文件清单见 `git show --stat 9f93d629`。
- 继承定向：`cargo test --locked --lib services::zcode_control:: --quiet`：641 通过；`pnpm test tests/mobile/ZcodeControlPanel.test.tsx tests/mobile/ZcodeQueuePanel.test.tsx tests/mobile/ZcodePendingCard.test.tsx tests/mobile/zcodeControlHook.test.tsx`：4 文件、143 通过。Vite 输出配置加载器未来兼容性提示。
- 静态结论：R10 **GO_STATIC**；供应商可用性字段及输入到执行的必然关联 **PARTIAL**。来源、安装包 SHA-256、字段与规范化定位见 [G0 静态证据](./2026-11-01-zcode-g0-static-evidence.md)；合成夹具为 `tests/fixtures/zcode-official-bridge/control-extensions.json`。
- 真实宿主、真实凭据及真实写入：NOT RUN；GO_STATIC 不等于 LIVE_PASS。

## 任务与验收

### Q1：发送受理与冻结正文

- 代码提交 `721014bc`，基点 `1bde45ce`；受理字段与动作账本迁移、按设备读取旧代回执、冻结正文发送及新旧回执兼容。
- RED：Rust 全域命令在新增测试引用尚不存在的接口时退出 101；关键缺失为 `receipt_for_device`、`freeze_send_text`、`PreparedSend`。GREEN：同命令 647 通过；`pnpm exec vitest run tests/mobile/zcodeControlApi.test.ts --pool=threads` 6 通过；`cargo test --locked --test zcode_control_remote --quiet` 27 通过。
- 独立新上下文复审：功能与接口符合，代码质量静态审查未发现确认的阻塞缺陷。复审未重复运行实现者的测试。实机 NOT RUN。
- **流程偏差**：仅旧行与冻结签名测试先写并观察接口缺失 RED；其余新增 Rust 回归和 TS 兼容测试在实现后补写。旧版 TS 类型事后对照因缺 `sendAdmission` 失败、新版通过，但不能改写成先红。Q1 不得标称严格 TDD 全通过；后续任务须先取得各新增行为的真实红灯。
- 本阶段 prepared 生产接线尚待 Q2/Q3，普通库出现 dead_code 警告；不得放宽 V0 的 clippy 门禁。后续假链应补实际 admission=queue/guide 的贯通回放验证。

### Q2：本地队列事务

- 实现者定向 50 项及域内 697 项通过；尚未提交。独立复审发现优先编排无法安全收口、24 小时快照仅启动时清理、按 requestId 取快照及存储错误码丢失四项缺口。
- 裁决：以设计的可恢复性和 24 小时上限为准，扩展 Q2 内部接口供 Q3–Q5 调用：提供安全失败收口及用户明确核对后的继续路径、独立的快照过期清理、按 requestId 授权读取快照，并保留冻结错误码。Q3/Q4/Q5 负责相应生命周期与交互接线；未知结果仍不得自动重停或重发。该补充修正计划接口未覆盖的链路，不降低原保护。
- 第一轮修正复审：2 项解决、2 项未解决。独立清理和按请求 ID 取快照的存储接口已补；提前 resume 后再核对会绕过暂停（P1），跨类型请求和已清理墓碑仍有错误码遗漏（P2）。
- 第二轮以行为断言复现上述两类缺陷并修正：核对、安全失败与启动恢复首次收口保持暂停，旧核对重放不撤销之后的显式恢复；请求按 owner/kind 先分流，墓碑和超界版本不误报存储故障。最终定向 76 项、ZCode 域 723 项通过；独立限定复审两项均 ADDRESSED，无新增 P1/P2。代码提交 `e289ffdc`。
- Q3 必须接通最早到期定时清理、执行屏障及生产宿主；Q4 接通优先操作安全收口和人工核对；Q5 将 GET 编辑结果接入 `edit_snapshot` 并映射冻结错误码。以上尚未完成；全文预览仅向属主提供，Q6 负责 20 字素显示。实机 NOT RUN。

### Q3：FIFO 与生产接线

- 实现者定向和回环结果、逐组 RED/GREEN、静态终态链、白名单扩展及 Q5/Q6 接线合同记于施工报告；最终控制域 762、远程模块 16、两套集成 9 + 27 通过。首次整域有 `pong_write_is_bounded_on_half_open_socket` 失败，单项与整域复跑通过，原因未证实；回环测试首次即绿不冒称 RED。实机 NOT RUN。
- 设计裁决：进程重启后不能凭复用的代际数字解除旧屏障；保留回执、暂停并要求属主核对。未观察到可关联终态时，显式核对不重发原项且仍需独立 resume。为与旧发送共锁，enqueue 改为 async，Q5 必须 await；Q6 必须接刷新冲突码与提示。
- 独立复审发现 paired 激活重试阻塞事件消费者、认领前临时读取失败无后续唤醒、确定零写错误误记 Unknown 且丢失原因码，第一轮修正后三项均解决。第二轮修复重连换代激活请求丢失，复审仍发现 Bridge 失效前预占代际可能把旧绑定误记为新代成功。第三轮以忠实假桥重现该窗口（103 通过、1 失败），成功连接完成时强制重验后 104 通过；限定复审判定解决，无新增 P1/P2。代码提交 `78c7bec7`。
- 第三轮最终控制域 773、远程模块 16、集成 9 + 27 通过；格式和差异检查通过。一次链接因磁盘不足失败，仅清理本 worktree 可重建的 Rust 增量缓存后重跑通过。先前回环 TCP 测试的首次失败和未证实根因仍保留。报告原“变化期间没有事件”的边界已扩大为 Bridge 已观察 controller 变化但队列未采样即恢复的情况；监听句柄意外死亡仍为未覆盖的运行限制。
- Q5 必须强制授权会话订阅、等待恢复完成、await enqueue、接入属主核对及显式 resume；Q6 补刷新错误码及中英文提示。Q4 须正确消费写前/可能已写出阶段。实机 NOT RUN。

### Q4：指定项停止后发送

- 实现者定向及冻结控制域 807、远程 16、集成 9 + 27 通过；真实宿主 NOT RUN。停止阶段扩展到 host/Hub/CommandSender，写后未知不重停；核对与 resume 分开。截止窗口曾在共享树临时移除守卫进行变异验证，已恢复，该过程违反计划要求使用外部副本，不冒称先实现 RED。
- 独立复审发现所选项原工作区在停止之后才核验（P1）、纯停止认领事务失败可留下无任务的 in-flight（P2）、停止阶段错误码未持久保留（P2）。第一轮限定复审前两项已解决；第二轮补入认领审计失败的持久 `before_write/storage_failed`，定向测试先 RED（phase 为 null）后 GREEN；主会话限定检查确认查询、同请求重放一致且零帧。当前工具未提供新约定要求的 `reviewer_senior`，第二轮由主会话检查，未冒称新上下文独立复审。代码提交 `1eaecea7`。
- 第二轮冻结验证：定向两项各 1、控制域 824、远程 16、集成 9 + 27 通过；格式和差异检查通过。连续拒写且进程退出或有限内存缓存满时，无法保证失败原因已落盘；恢复保持暂停、不伪造原因。
- 新代理约定生效后，Q4 剩余修正由 `worker`（配置预期 gpt-6.1-terra / medium；工具未返回实际运行字段，实际未核验）接手。此前执行者创建时未显式固定角色，不能核验其运行组合，已停止续派。最终和高风险审查所需 `reviewer_senior` 当前工具未暴露。
- Q5 必须将旧 HTTP Stop 安全转交 durable stop_current 或明确拒绝，确保旧路径未知结果也不能换 ID 重停；核对候选须按设备与会话授权，Q6 区分停止未知和消息投递未知。

### Q5：生产路由（代码完成，待 Q7 全链与实机）

- 恢复时 HEAD 为 `3972e63d`，工作树干净；Q5 尚无源码或测试。现有 `/zcode/session` 已在 `remote/server.rs` 注册，`ZcodeHub::queue_service` 在 `hub.rs` 提供现有服务入口；本地队列 HTTP/SSE 尚须按 Q5 合同实现。
- 本次尝试按固定 `worker` 角色分派时，代理工具两次返回并发名额已满；列表中三个旧代理处于 `pending_init`，中断后仍未释放名额。未因此将 Q5 改成无独立实现者的交付，也未声称取得 RED。
- 用户随后授权在主会话执行。只读入口的早期施工：配对但服务未就绪反例 RED（403，预期 503）→ GREEN；隐藏会话反例 RED（503，预期 404）→ GREEN。两个 `local_queue` 定向测试最终 2 passed；组合运行曾发现未就绪误映射 409，修正后通过。编译期间补写过测试，相关旧进程结果未作为最终证据。当前仅有未提交的只读入口；已安装服务的多设备隔离、写入、SSE 和 Q5 其余合同尚未验证或完成，不能视为 Q5 交付。
- 入队白名单开始施工：客户端自报 workspacePath RED（403，预期 400）→ GREEN；附件嵌套多余字段 RED（409，预期 400）→ GREEN。当时 POST 仅检查字段并返回未就绪；后续接线见下。所有改动仍未提交。
- 后续已将 POST 接入同一队列服务，真实 router 假服务用例 RED（409，预期 200）→ GREEN，验证回执、GET 立即见一项和零后端写；该夹具未启动 worker，不能证明运行中忙态的全链。空 requestId RED（200，预期 400）→ GREEN；隐藏会话 POST RED（409，预期 404）→ GREEN。附件临时目录校验 RED→GREEN，另有会话目录符号链接绕过 RED→GREEN（2 项通过）。上述入队仍缺完整回环、附件及身份边界复审；Q5 未完成。
- 编辑、快照 GET、撤回、放弃快照已接真实 router：入队→编辑→按请求 ID 重取 RED（403，预期 200）→ GREEN；撤回 RED（403，预期 200）→ GREEN；放弃及同请求重放 RED（403，预期 200）→ GREEN。其他设备取快照回归通过；首次因合成设备过期被 gate 403，修正夹具时间后为预期 404。假服务未启动 worker；上述改动均未提交。
- 继续施工：旧 `/actions` Stop 的刷新拒绝 RED（needs_evidence，预期 queue_refresh_required）→ GREEN；插队额外正文 RED（403，预期 400）→ GREEN，外设备项拒绝为首次 GREEN 回归；恢复、持久 stop 及原请求操作查询的路由测试依次 RED（403）→ GREEN。完整 `CARGO_INCREMENTAL=0 cargo test --locked --test zcode_control_remote --features zcode-bridge-evidence --quiet` 35 passed；该测试不覆盖真实宿主或 worker 忙态。旧 Stop 当前被拒绝，新 stop 入口已接但手机尚未适配；未知核对和 SSE 仍未接入，不能声称 Q5 完成。
- 再次施工：属主未知操作候选和显式核对的假路由 RED（403）→ GREEN，核对后仍暂停，其他设备核对被拒；SSE 首帧附本设备队列、编辑后 `local_queue` 事件 RED（超时）→ GREEN，第二设备的首帧和事件不含他人正文/编辑 ID（首次 GREEN 回归）。当时使用独立广播并尚未引入 `ControlEventKind::LocalQueue`，后续整改见下；不得据此称 SSE 完成。该阶段远程路由套件 35 passed；一次误在仓库根运行 cargo 因缺 Cargo.toml 失败，随后在 src-tauri 运行通过。
- 普通 FIFO 的派发认领事件追加失败驱动测试：假宿主阻塞 ACK 时事件接收超时 RED → 认领事务后发布事件 GREEN；验证当时队列状态为 dispatching。指定项认领、后台各收口、事件积压和恢复的完整传播仍待验证；格式及 `git diff --check` 通过，未提交。
- 指定项认领的 ACK 前事件 RED（第二个通知超时）→ GREEN；已增加 Hub `LocalQueue` 标记并复用原 Hub 广播，SSE 对每个接收设备重新读取过滤视图。Hub 事件反向唤醒 worker 的循环 RED → GREEN；先订阅 SSE、后装配队列服务的漏事件 RED → GREEN。70 次入队造成积压后发 resync 的假路由回归通过。当前广播仍须对全部后台事务、数据读取失败及 seq/重连语义做完整复审；未实机。一次错误地在仓库根运行 cargo 失败后，改在 src-tauri 执行通过。
- 同一总线接入后 `CARGO_INCREMENTAL=0 cargo test --locked --lib services::zcode_control:: --quiet` 825 passed；真实路由假服务的 SSE 首帧、属主过滤、积压 resync 及服务迟装配用例分别通过。格式与 diff 检查通过。Q5 仍未通过整体复审，不提交。
- SSE 接收队列事件时数据库暂不可读：真实假路由反例 RED（静默跳过、超时）→ GREEN（发送 queue_read_failed resync）；恢复表后后续操作通过。该次 `zcode_control_remote` evidence 套件 36 passed，格式及 diff 检查通过。仍需对全部后台状态通知、真实路由到假 wire 的完整忙态链、幂等和独立复审逐项验收。
- 按施工单修正编辑快照 GET 与放弃入口：仅 requestId 的两项路由反例均 RED（400）→ GREEN，服务端从属主记录推导会话并核验可见性。手机 API 路径/载荷三组 RED（函数缺失）→ GREEN，Vitest 9 passed，`pnpm build` 通过（有 Vite 包体提示）。
- 入队同 ID 重放遇签名设置变化 RED（400）→ GREEN：新增可空 `request_input_hash` 迁移，按设备和原始草稿/附件/会话/编辑来源校验同请求，返回原回执、保留第一次冻结正文；不再把服务端当前签名设置当成客户端重放参数。不同正文仍冲突；原材料清理后的同 ID 重放回归通过。旧库加列及重复迁移保留旧行的回归首次 GREEN；旧行无新指纹沿用原 hash 判据。ZCode 控制域 826 passed。该服务端派生参数与 Q2 原完整指纹契约的关系需整体复审，不能以单项测试视为最终裁决。
- 完整 evidence 远程路由回归在上述修改后 36 passed；仍只证明假服务链，未启动该用例的 worker，不能代替真实路由到假 wire 的忙态测试。新加列/原始请求指纹、后台 SSE 传播和旧 Stop 拒绝策略仍须独立审查。所有改动未提交。
- 新增真实 router + 生产队列 worker + 假 Hub 忙态回归：宿主有发送资格、官方队列已知且未暂停、A 运行；B/C 入队后立即可见，等待 worker 完成一次检查后均保持 queued/autoDrain，零 fake 后端写。此用例首次 GREEN，不伪称 RED；不是生产 CommandSender 到回环 wire 的全链。当前 evidence 远程路由套件 37 passed，格式与 diff 检查通过。
- 追加假路由附件回归：会话附件目录内文件入队并编辑后，可仅凭 requestId 重取原附件结构；工作区内但目录外的文件被拒绝；未配对 POST 不触发绑定或落库。两项均首次 GREEN，不伪称 RED。当前 evidence 远程路由套件 38 passed；一次在 src-tauri 下误用仓库相对路径执行 rustfmt 失败，改在仓库根执行后通过。全链 wire 和独立审查仍待完成。
- 发现入队后附件被清理时会发送失效引用：普通 FIFO 反例 RED→GREEN（留队暂停、零消息）；指定项附件失效反例 RED→GREEN（停止前拒绝、零停止/消息）；指定项认领后附件被移除反例 RED→GREEN（零消息、原项留队）。复用同一附件目录校验，并以 `attachment_missing` 暂停原因表示；控制域 827 passed。当时 Hub 重新绑定期间仍有窗口，后续修正见下。
- Hub 最终重绑定期间删除附件的假后端反例 RED（错误到达发送器）→ GREEN（零帧、留队暂停）；Hub 在最终委托前按原 action/设备/会话回查并复核附件。文件仍可能在最后一次检查后由外部删除，无法靠路径检查锁住，须实机如实验收。`attachment_missing` 已加入 Rust/TS 原因码与中英文文案；手机 API 测试 10 passed、`pnpm build`、i18n 和错误码脚本通过。误运行不存在的 `pnpm check:error-codes` 失败，随后以 `node scripts/check-error-codes.mjs` 正确运行通过。旧直调发送端口的假测试增加对应持久认领记录后通过。
- 最终重绑定复核后 ZCode 控制域 828 passed；此项使用内存库及临时附件，未连接真实宿主。指定项停止端口自身重新绑定期间附件被外部删除的窄窗口尚未单独覆盖；不得把发送端口校验写成对停止端口的证明。
- 指定项停止认领后、端口调用前删除附件 RED（错误停止）→ GREEN；Hub 假后端最终停止重绑定期间删除附件 RED（错误进入停止发送器）→ GREEN（零停止帧）。控制域 829 passed；当时无匹配 priority 记录按纯 stop 放行，后续收紧见下。末次文件检查之后仍存在外部删除窗口；未实机。一次在仓库根误运行 cargo 失败，随后在 src-tauri 执行通过。
- 停止最终写前收紧为必须匹配当前设备、会话、actionId、代际和已认领 in-flight 的 priority 或纯 stop；缺失记录 RED（误放行）→ GREEN，纯 stop 有效认领可通过、错误代际及 unknown 状态拒绝。旧直接端口假测试补入持久认领记录；控制域 830 passed。末次文件检查后仍可能被外部删除；该收口及测试夹具变更仍待整体复审。

Q6–Q7、C1–C2、M1–M2、K1–K2、V0 尚待执行；Q5 代码已提交，但完整生产路由到假 wire 归 Q7。R01–R10 的最终覆盖、逐任务 RED/GREEN 与独立复审将在施工中逐项记入。
- 接手收口：`queue_host_stop_delegates_only_with_current_target` 定向 1 passed；复核 `hub.rs` 的最终停止写前查重与 `store.rs` 的 `priority_stop_attachments_valid`，确认错误 `expectedExecutionId`、错误代际、缺失/已判 unknown 的停止认领均在调用停止端口前拒绝，且不产生停止帧。该检查不把“已写停止”误当成可复用许可。
- Q5 主会话整体复核：`request_input_hash` 只覆盖设备、会话、原始草稿/附件与 `editedFrom`，刻意排除服务端派生的 workspace/prepared/签名设置；同 ID 重放遇签名设置变化仍返回原回执，旧行无新指纹时回退原 `payload_hash`。HTTP 路由已覆盖未配对/隐藏会话/未就绪、设备与会话归属、旧 `/actions` Stop 拒绝、同 ID 重放与冲突、编辑快照、SSE 首帧/设备过滤/积压 resync/读库失败 resync；附件只接受当前会话附件目录内现存普通文件。`CARGO_INCREMENTAL=0 cargo test --locked --lib services::zcode_control:: --quiet` 830 passed；`cargo test --locked --test zcode_control_remote --features zcode-bridge-evidence --quiet` 38 passed；两套 `cargo clippy --all-targets ... -D warnings` 与 `cargo fmt --check` 通过。Q7 仍须补生产路由到假 wire 的完整忙态／取消链；真实宿主、真实附件删除窗口和手机端接线 NOT RUN。此复审为主会话限定检查，不冒充新上下文独立 reviewer。
