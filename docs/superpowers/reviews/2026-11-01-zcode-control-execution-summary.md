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

Q5/Q6 代码已提交；Q7、C1–C2、M1–M2、K1–K2 与 V0 的收口见下。R01–R10 的最终覆盖与实机缺口另列。- 接手收口：`queue_host_stop_delegates_only_with_current_target` 定向 1 passed；复核 `hub.rs` 的最终停止写前查重与 `store.rs` 的 `priority_stop_attachments_valid`，确认错误 `expectedExecutionId`、错误代际、缺失/已判 unknown 的停止认领均在调用停止端口前拒绝，且不产生停止帧。该检查不把“已写停止”误当成可复用许可。

- Q5 主会话整体复核：`request_input_hash` 只覆盖设备、会话、原始草稿/附件与 `editedFrom`，刻意排除服务端派生的 workspace/prepared/签名设置；同 ID 重放遇签名设置变化仍返回原回执，旧行无新指纹时回退原 `payload_hash`。HTTP 路由已覆盖未配对/隐藏会话/未就绪、设备与会话归属、旧 `/actions` Stop 拒绝、同 ID 重放与冲突、编辑快照、SSE 首帧/设备过滤/积压 resync/读库失败 resync；附件只接受当前会话附件目录内现存普通文件。`CARGO_INCREMENTAL=0 cargo test --locked --lib services::zcode_control:: --quiet` 830 passed；`cargo test --locked --test zcode_control_remote --features zcode-bridge-evidence --quiet` 38 passed；两套 `cargo clippy --all-targets ... -D warnings` 与 `cargo fmt --check` 通过。Q7 仍须补生产路由到假 wire 的完整忙态／取消链；真实宿主、真实附件删除窗口和手机端接线 NOT RUN。此复审为主会话限定检查，不冒充新上下文独立 reviewer。

### Q6：手机即时列表、再编辑与统一写槽

- 新增 `src/mobile/zcode-local-queue-state.ts`：20 字素摘要、按会话/版本单调合并、recoverableEdits 查找。严格 RED：模块缺失导致 `zcodeLocalQueueState.test.ts` 无法解析（0 test）→ GREEN 3 passed。
- `useZcodeControl` 在同一 SSE 流消费裸 `local_queue` 帧及 `snapshot.localQueue`，`GET /local-queue` 首读失败不污染连接/会话原因面；版本旧帧不倒退。新增 hook 反例 RED（`localQueue` 为 undefined）→ GREEN；全 hook 26 passed。
- 新增 `ZcodeLocalQueuePanel`：默认展开，位次 + 20 字素摘要 + 「插队/再编辑/撤回」，暂停显示继续队列，unknown 项保留并标注待核对，recoverableEdits 提供「恢复编辑」。组件缺失 RED → GREEN；面板 5 passed。
- 主面板接线：运行中/本地已有待发/暂停时普通发送只 `POST /local-queue/enqueue`，不直接 `POST /actions send`；空闲且无待发仍走原直接发送兼容路径。入队回执只清本次提交的草稿/附件，回执期间续写保留；网络未知重试复用原 requestId。`enqueue` 前反例 RED（无本地行）→ GREEN，另补续写、network 重试、插队、撤回、编辑冲突、继续队列用例；本地队列 flow 6 passed。
- 插队仅在运行中弹确认，确认后只发一个本地 `prioritize`（带唯一 `expectedExecutionId`、`confirmStop=true`）；空闲直接单条处理，不要求预先暂停。再编辑取回全文/附件，已有草稿时弹替换/合并/取消，默认取消；快照请求 ID 保留给新入队消费。所有本地写动作与主按钮共用 `inFlightRef` 单槽。
- 门禁：`pnpm build`（tsc + vite）通过；`pnpm check:i18n` 2660 键通过；目标 ESLint 通过；ZCode 移动端 13 个相关文件 193 passed（含新增 5 + 6 + 3 + 1）。过程偏差：部分交互测试在对应实现后补写，未全部取得严格先行 RED，不冒充完整严格 TDD。
- Q6 未实机；Q7 仍需生产路由→假 wire 的全链、取消/重建与旧官方队列 CAS；手册/变更账本在 Q7/V0 更新。

### Q7：生产路由 → 生产队列 worker → 假 wire

- 新增全链回归 `outbox_http_priority_stop_and_send_cross_production_wire`：真实 router（配对 gate）→ `POST /local-queue/enqueue` B/C → 生产 `ZcodeQueueService` worker → `POST /local-queue/prioritize` C → 生产 Hub/CommandSender → 回环假官方 WS。断言忙态入队零上游帧、C 插队只产生一次 stop + 一次 C、B 留队且 `autoDrain=false`、stop/send 各一次 ACK；同 requestId 重放逐字段相同且零第二停/发。
- 首次运行 RED：priority 返回 `no_session`。定位为入队写入的 canonicalize 路径（macOS `/tmp` → `/private/tmp`）与绑定目标字符串不等；在 `local_queue/store.rs` 增加 `workspace_matches`（字符串相等或两端 canonicalize 相等），覆盖 priority/claim/head 四处比较。修复后同一用例 GREEN。
- 假宿主只在收到 `type=stop` 时清 `running` 并推下一水位空 control，使生产停止终态判据可满足；不把 ACK 当终态。另把旧 `/actions` Stop 的 e2e 改为断言 409 `queue_refresh_required`、零上游帧、零审计、零动作行，避免旧测试与新安全拒绝策略冲突。
- 结果：该用例 1 passed；evidence 路由套件 39 passed。仍未跑真实宿主、真实附件删除窗口、真实取消/重建；`prioritize` 的 HTTP 取消由 Q4 任务级有界收口覆盖，未在本用例重造。
- 过程偏差：全链测试首轮因真实路径比较缺陷 RED，属生产缺陷；不是先写好测试再实现，但失败与修复均有实际命令证据。

### C1 / C2：跨供应商目录与两级选择

- C1 后端：`ProviderChoice`、`SelectionView.providers`、`ControlAction::Selection.provider`、`build_selection_command_for_provider`；目录解析保留全部合法 provider 分组，模型/档位只取目标组自己的列表，未知/他组/空档位拒绝；动作指纹对 `Some(provider)` 生效、`None` 保留旧形状；Hub 确认目标 provider 与 Session 写锁。新增 RED：provider 函数不存在（选择测试无法编译）→ 实现后 3 passed；命令级 `selection_uses_target_provider_and_waits_for_it` 1 passed；HTTP `selection_provider_reaches_backend_through_action_whitelist` 1 passed。
- C2 前端：新增 `ProviderChoice`/`ModeView` 类型与 `resolveModelChoice` 纯函数；面板增加供应商下拉，切供应商零 POST，选模型后一次提交完整三元组。纯函数缺失 RED → GREEN 3 passed；UI `provider_change_alone_emits_zero_post_then_model_sends_one_full_triple` 1 passed。不可用 provider 禁用、旧载荷仅当前 provider 兼容。
- **评审修正（2026-11-02）**：原实现只在显式 `available=false` 时关闭，缺字段按可用，属于候选级乐观假设；已收紧为**当前 provider 缺省 true、非当前 provider 仅显式 `available=true` 才开放**，并补 `provider_availability_defaults_only_for_current_provider`。生产合并出口原先只复制当前 `models`、不复制 `providers`，已补 `selection_load_exposes_provider_groups_through_session_view`（RED：生产读路径返回空数组 → GREEN）。真实两个已配置 provider、目录缓存联动、上下文保留仍 NOT RUN；目录成员本身仍不等于“当前工作区可用配置”的证据。

### M1 / M2：R10 协作模式

- 后端新增 `CollaborationMode`/`ModeView`、`switchCollaborationMode` 纯构造（payload 只含 mode + baseRevision；`edit` 只读、yolo 必须显式确认）、`normalize_mode`（`planEnabled=true` → Plan，`auto` → Build，未知不猜）、`ControlAction::Mode` 与 HTTP 白名单；`Capability::Mode` 作为第 12 项独立能力，已知 build 才走静态判据，未知 build 不再凭 switch 方法 + config.mode 形状开放，绝不借 send/approval/queue。
- `CommandSender::execute_mode`：新鲜 ready/idle/no pending 读 → 恰一帧 → ACK 只落 accepted → 有界等 revision 前进且规范化 effectiveMode 精确相等才 confirmed；复用既有 `StopPort` 的只读 session（未另造 ModePort，属实现偏离）；Hub 的同会话写锁覆盖 Selection/Mode。
- 新增 `mode.rs` 3 tests、命令级模式测试 1 passed、HTTP mode 白名单正向/多余字段 400 1 passed、resolver 模式独立性 1 passed、DAO 12 能力词可逆 1 passed。M2 面板：四标签、应用/取消、完全访问额外确认、ACK 不乐观改标签；UI 2 passed。真实四模式效果、原生界面同步、`planEnabled` 实机序列化、手机发送实际审批策略 NOT RUN。
- **偏离**：`ModeView` 放在 `SelectionView.mode`，而非计划写的 `SessionView.mode`；为避免 51 处 SessionView 字面量扩散而采取该形状，API 已随 selection 下发，前端消费一致。
- **评审修正（2026-11-02）**：Mode 已进入 Hub 确认能力集合，新增 `ConfirmationTarget::Mode`、`ModeObservation`、`confirm_mode`（规范化 effectiveMode + revision 前进）、回执 confirmed/degraded 收口与 `mode_matched` 证据词；未知 build 只凭共享方法/形状不再开放，已知 build 才走候选。新增纯判据、pump 落账、回执失败降级、动作级登记测试。仍无真实模式效果/实机 evidence 账本，不能据此声称 R10 实机通过。

### K1 / K2：签名入口与凭据回归

- K1 新增 `scripts/zcode-signed-build.mjs` + `build:macos:signed-dev`：参数数组、`shell:false`、缺身份/非 macOS 抛错且零构建、单条 `tauri build --debug --bundles app --config`，JSON 仅含 `bundle.macOS.signingIdentity`；不装证书/不读私钥/不改 ACL/不启动 App。测试缺失脚本 RED → GREEN 4 passed；`pnpm exec tauri build --help` 确认 `--config/--debug/--bundles` 存在。未用真实证书。
- K2：`auth` 51、`credential_store` 20、`auto_connect` 22 passed；未发现新缺陷，不伪造新 RED；手册中英补齐系统“允许/始终允许”、四类签名/重编译结果和“脚本单测不等于系统提示已过”。真实 Keychain 冷启动 3 次、同身份升级、开发热重载、Windows/Linux 均 NOT RUN，R02 仍是外部条件未闭合。

### V0 门禁与剩余缺口（2026-11-02）

- 前端：`pnpm test` 207 files / **2539 passed**、5 skipped；`pnpm check`（Prettier、ESLint、i18n 2673 键、错误码 328 码、tsc、vite）通过；`pnpm build:mobile` 通过。ESLint 仅 1 条既有 `uploadCtrlsRef` warning，无 error。
- Rust：`cargo check --locked`（src-tauri 下）通过；`cargo test --locked --features marker-helper` **4359 passed**、0 failed；`cargo test --locked --tests --features marker-helper,zcode-bridge-evidence` **4382 passed**、0 failed；`cargo test --locked --lib --features marker-helper,zcode-bridge-evidence` **3852 passed**、0 failed、38 ignored；两套 `cargo clippy --all-targets ... -D warnings` 与 `cargo fmt --check` 通过。
- 真实宿主/真实凭据/真实发送停止/审批模式效果/真实 Keychain 四类/两 provider 上下文/移动真机：全部 NOT RUN。自动假链只能证明接线与守卫，不证明 R09/R10 的宿主效果或平台提示次数。
- 仍关闭/未证：非当前 provider 缺 `available` 时保守禁用（已按评审收紧）；Mode 已有确认账本接线但真实模式效果未跑；真实未知 build 的模型/thought 首用确认；真实附件删除窗口；官方队列 W0 写能力仍按原门；R02 证书条件。未发现新的持久秘密/正文/链接泄漏。
- 报告复跑以最终 HEAD 为准；执行者不再自判“可合并”。以上为单会话执行与限定复审，最终仍需原聊天/独立 reviewer 复核。

## R01–R10 覆盖表（最终代码；真实宿主状态单列）

| 需求                     | 代码/自动证据                                                                                       | 真实宿主                                  | 结论                                       |
| ------------------------ | --------------------------------------------------------------------------------------------------- | ----------------------------------------- | ------------------------------------------ |
| R01 授权复用             | auth 51、auto_connect 22、凭据存储 20 passed；启动只恢复不 OAuth                                    | 真实保险箱/真实重启 NOT RUN               | 代码与假体闭合，实机未跑                   |
| R02 钥匙串提示           | K1 signed-build 4 tests；手册四类场景；未安装证书                                                   | 三次冷启动/同身份升级/开发重编译 NOT RUN  | **外部条件未闭合**，不得称已解决           |
| R03 队列立即显示         | Q5 路由 39、Q6 UI/flow、Q7 全链；入队零上游，刷新/重启由业务表保留                                  | 真机忙态 B/C NOT RUN                      | 自动假链闭合，实机未跑                     |
| R04 只暂存不先发         | Q2/Q3 FIFO、Q5/Q7 enqueue、附件失效与路径复核；忙态零上游帧                                         | 真实附件删除窗口 NOT RUN                  | 代码闭合，实机未跑                         |
| R05 指定项停止后只发一条 | Q4 stop 编排、Q5/Q7 priority；全链断言 1 stop + 1 send，B 留队                                      | 真实停止/取消 NOT RUN                     | 自动假链闭合，实机未跑                     |
| R06 再编辑/保护草稿      | Q6 edit 快照、冲突替换/合并/取消、24h 清理；Q5 路由                                                 | 响应丢失/刷新真机 NOT RUN                 | 代码闭合，实机未跑                         |
| R07 撤回/暂停/显式恢复   | Q2/Q3/Q4 本地 CAS、Q6 resume、旧 Stop 409；不自动恢复整队                                           | 真机并发/重启 NOT RUN                     | 代码闭合，实机未跑                         |
| R08 同供应商模型/档位    | 既有 selection tests + C1 回归；命令仍单帧 CAS                                                      | 原有实机认可保留                          | 保持，未重写                               |
| R09 跨供应商两级选择     | C1 parser/command/route tests；C2 provider UI；同模型不同 provider 可分                             | 两 provider 真实切换/上下文 NOT RUN       | 代码与假链闭合，**可用性字段仍为候选假设** |
| R10 四模式               | M1 `mode.rs` 3、命令 1、HTTP 1、resolver 1、DAO 1；M2 UI 2；四标签/planEnabled/yolo 确认/ACK 不乐观 | 四模式真实效果、原生 UI、实际审批 NOT RUN | 代码与假链闭合，**实机未跑**               |

继承八能力、问答/审批/正文/附件回归由 4359 项普通测试与 4382 项 evidence 集成回归覆盖；没有用“全绿”替代上表的实机未跑结论。

## 2026-11-02 独立评审修正轮

独立评审在 `b9708e8a` 上指出 6 项代码缺口；本轮逐项以失败回归/根因修复，未连接真实宿主。

1. **P1 供应商目录未下发**：`view_with_catalog` 只复制 `models`，`providers` 保持投影空数组。新增生产读路径测试，RED 为 `left: [] / right: [A,B]`；修复为同一合并出口复制 `catalog.providers`。GET 与实时帧共用该出口（`on_data_outcome` 前后快照同函数）。
2. **P1 再编辑取消误删快照**：原流程先 `POST /edit` 撤回、再询问冲突，取消调用 `finish`。改为**已知草稿先 preflight**，取消零 `POST /edit`；请求期间才出现的冲突，取消只关对话框、不调用 `finish`，保留 `recoverableEdits` 入口。新增两条 flow 测试（preflight 零 edit/零 finish；请求期冲突取消零 finish）。
3. **P1 Unknown 人工核对未接线**：面板新增 `getZcodeQueueRecovery` + `acknowledgeZcodeQueueRecovery` 四类按钮（priority/manual stop/dispatch/unconfirmed），核对只解除屏障，不重发/重停；核对后独立“继续队列”。新增四按钮 API 断言与“核对后出现 resume”测试。
4. **P2 运行态守卫**：Selection 现在要求 `ready && pending.empty && control=Some(empty activeWorks)`；Mode 的 `control=None` 不再当作空闲。新增零帧拒绝测试（Selection unknown/running、Mode unknown）。
5. **P2 同名模型档位串组**：`provider_from_record` 的扁平档位回退增加 `provider_id == config.provider`；新增 A/B 同名模型的失败回归，B 未声明档位保持空。
6. **P2 Mode 证据/账本**：未知 build 的 Mode 在 resolver 中保守关闭（`needs_evidence`），仅已知 build 静态判据可开；Mode 进入 Hub 首次确认集合，新增 `ConfirmationTarget::Mode`、`ModeObservation`、`confirm_mode`、`mode_matched` 证据词与回执 degraded 收口。新增纯判据、pump 确认、回执失败降级、动作级登记测试。另将“完全访问只有 UI 检查”的评审表述更正为：服务端 `execute_mode` 也会拒绝 `confirmFullAccess=false`。

复跑（本轮修改后，最终计数）：普通 Rust **4359 passed / 0 failed**；evidence integration **4382 passed / 0 failed**；evidence lib **3852 passed / 38 ignored**（一次 `pong_write_is_bounded_on_half_open_socket` 在满载下偶发失败，单跑及重跑全绿，记录为已知 flaky，不记为产品修复）；移动端全量 **2539 passed / 5 skipped**；`pnpm check`、`build:mobile`、两套 clippy、`cargo fmt --check` 通过。

## 2026-11-02 第二轮评审修正

独立复审又指出两项时序缺口；本轮均先做失败反例，再修根因。

1. **P1 替换确认后的回执竞态**：原 `runLocalEdit("replace")` 只信 preflight 时刻的意图，请求期间用户继续输入会被回执覆盖。现在确认替换时记录**正文 + 附件版本戳**（id/status/path/name），回执到达时逐项比较；任一项变化即转 `editConflict` 再询问，不覆盖新草稿。新增 `replace_during_edit_waits_for_a_stable_draft_and_reasks`。
2. **P2 A→B 实时帧丢目录**：原 `view_with_catalog` 只在 `catalog.provider == current.provider` 时复制 `providers`；桌面切到 B 而缓存仍是 A 时，实时帧的 providers 为空。现在 `on_data_outcome` 检测 provider 变化后**清空旧缓存**，锁外调用 `ensure_selection_catalog` 补读新目录，再重算并发布本帧视图。新增 `provider_switch_refreshes_catalog_before_publishing_live_frame`；修复前断言 `providers.len() == 2` 得到 0，修复后通过，且不借用 A 的模型列表。

最终复跑：普通 Rust **4359 passed / 0 failed**；evidence integration **4382 passed / 0 failed**；evidence lib **3852 passed / 38 ignored**；移动端 **2539 passed / 5 skipped**；`pnpm check`、`build:mobile`、两套 clippy、`cargo fmt --check` 通过。真实宿主未验，不改变此前的实机 NOT RUN 结论。

## 2026-11-02 第三轮评审修正

### P1：收帧等待目录 RPC 的自等待

根因：生产 Hub 串行消费 `Data` → `bridge.on_data_outcome` 内同步 `await ensure_selection_catalog`；目录 `getView` 的应答也必须由同一条 Data 循环处理。这不是锁问题，而是同一条串行泵等待自己的应答，生产约 10 秒超时。

修正：

- `on_data_outcome` 只完成投影、ACK、必要变化视图，**绝不 await 目录 RPC**；
- 新增 `DataOutcome.catalog_refresh`，Hub 泵为每个切换会话启动独立任务；
- 任务完成刷新后重新核对 Hub 代际、绑定、revision、当前 provider，再发布最新视图；
- 刷新失败有界重试一次（首次 + 一次，100ms），不无限循环；
- `BridgePump` 新增 `refresh_catalog`/`session_view`，生产 `ProjectionBridge` 委托 `BridgeManager`。

回归：

- `provider_switch_refreshes_catalog_before_publishing_live_frame`：断言收帧期间 `getView` 数量不增加，`catalog_refresh` 含会话；手动刷新后 B 两组目录在场。
- `provider_switch_catalog_refresh_is_async_and_publishes_latest_view`：经生产 Hub 泵消费，先发布切换帧，再由独立任务发布 providers=2 的第二帧。
- `late_provider_catalog_result_does_not_overwrite_newer_provider`：A→B→C 时 B 的迟到 RPC 结果被丢弃，C 仍可重试并安装。
- `provider_catalog_refresh_retries_once_then_stops`：失败恒定时只调用两次，随后停止。
- `failed_catalog_read_clears_attempt_for_same_revision_retry`：同一 revision 的失败尝试位被清除，下一次仍会读。

### P2：失败重试承诺

`ensure_selection_catalog` 现在返回是否安装，并在 RPC/解析失败或结果过期时清掉对应 revision 的 `catalog_attempted_at`；同时用 `catalog_refresh_provider` 记住缓存清空后的待刷新 provider，保证 A→B→C 的 C 帧仍会排队刷新，而不是被 `catalog=None` 跳过。刷新任务最多重试一次，之后由读路径按需再试。

最终复跑：普通 Rust **4359 passed / 0 failed**；evidence integration **4382 passed / 0 failed**；evidence lib **3852 passed / 38 ignored**；移动端 **2539 passed / 5 skipped**；`pnpm check`、`build:mobile`、两套 clippy、`cargo fmt --check` 通过。真实宿主未验，不改变此前的实机 NOT RUN 结论。

## 2026-11-03 手机可用性修补（队列/供应商/计划批准）

本轮针对用户实机反馈的四个问题，按修补文档
`docs/superpowers/plans/2026-11-03-zcode-mobile-usability-patch.md` 执行，子代理 A/B/C 顺序实施，
主会话逐项复审、提交并跑全量门禁。

### 队列恢复

- 根因：重启恢复 `accepted + startNow` 无终态被标 `restart unconfirmed` / `target unconfirmed`，
  `auto_drain=0` 且有 `has_delivery_barrier`；旧 `resume` 只置 `auto_drain=1`，不清 barrier；
  `prioritize` 在无前台回合时提前返回，造成“继续/插队成功但不派发”。
- 修正：新增 `queue_review_required`（409）；`resume` 和 `prioritize` 在未核对 barrier 时拒绝且零
  stop/send；普通 running accepted **不算** review barrier，仍由 `has_delivery_barrier` 管自动派发
  互斥；“确认并继续队列”ack 后自动 resume，旧 unknown 不重发。
- 回归：unconfirmed 下 resume/priority 拒绝、版本不变、零发送；核对后只发 B 一次；正常 running
  accepted 的插队仍可 stop+send；移动端未核对禁用+原因、确认后一次 ack+一次 resume。
- 提交 `d91389c2`。
- 2026-11-03 实机追补：核对入口原先按设备过滤，旧设备留下的 unconfirmed 结果会让新设备
  看到“已暂停”但没有核对按钮，`resume` 又因会话级屏障拒绝。已改为会话级可见/可核对
  （只清屏障、不重发），新增跨设备回归；`local_queue` 160 tests 通过。

### 供应商三态

- `ProviderChoice.availabilityKnown`：显式 `available` 为 known；缺字段为未知；显式 false 恒禁用。
- `Selection.confirmUnknownProvider`：unknown 未确认拒绝，确认后允许一次完整三元组；provider
  继续入指纹，确认标志仅在 true 时入指纹。
- UI：不可用仍禁用；unknown 可选中并显示原因，选模型/档位提交前二次确认；确认前零 selection
  POST；失败不乐观改当前组合。
- 回归：selection 21、action 21、command selection 11、移动端 8、HTTP 白名单 1；同名模型档位
  与 provider 指纹不回归。
- 提交 `6fa1b8b4`。真实 provider 切换/账号状态仍 NOT RUN。

### 计划批准（C2 P0 代码完成，真实验收待跑）

- `PendingView.planApproval` 仅在 `kind=userInput ∧ toolName=ExitPlanMode ∧
schema=plan_approval` 时置位；`answerable` 仍 false，普通 `Answer` 仍拒绝。
- C2 已按官方投影实现原生批准：官方唯一 option 原值为 `approve`；新增独立
  `ControlAction::PlanApproval`、`build_plan_approval_answer`（唯一问题 + 唯一 approve
  才返回 `{optionId:"approve"}`）、pending 授权/写前预留/snooze/恰一帧 resolve；
  Hub 使用 `single_select` 能力闸，不借 permission 的 `approval_reply`。
- 移动端计划卡提供“批准并开始实施”按钮；点击前零 POST，点击后恰一次；能力关闭/卡形状不精确
  时只读并说明原因；普通不可答子型保持通用只读。
- 回归：pending 4、command 2、计划卡/面板 93、zcode_control lib 870、全量 Rust 4375、
  移动端 2549 通过。提交 `85c04164 feat(zcode): resolve plan approval natively`。
- **真实宿主批准验收 NOT RUN**：需要重建 Tuvis 后，在真实 `plan_approval` 卡点击一次并观察
  宿主撤卡/退出 plan mode；未过此前不得宣称 P0 完成，也不得用 fake 冒充真实验收。
- 提交 `d26b1083` 是 C1；C2 代码待本批提交。

### 最终门禁

- 移动端：207 files，**2549 passed / 5 skipped**；`pnpm check`、`pnpm build:mobile` 通过。
- Rust：普通 **4375 passed / 0 failed**；evidence integration **4398 passed / 0 failed**；
  evidence lib **3864 passed / 38 ignored**；两套 clippy、`cargo fmt --check`、`cargo check`、
  `git diff --check` 通过。
- 真实 ZCode 宿主、真实凭据、真实发送/停止/审批/权限、真实 provider 切换、真实 Keychain：NOT RUN。
- 未 push/merge/tag；C2 需真实 payload 后另开任务。
