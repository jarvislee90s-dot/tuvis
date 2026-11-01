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

### Q3：FIFO 与生产接线，复审修正中

- 实现者定向和回环结果、逐组 RED/GREEN、静态终态链、白名单扩展及 Q5/Q6 接线合同记于施工报告；最终控制域 762、远程模块 16、两套集成 9 + 27 通过。首次整域有 `pong_write_is_bounded_on_half_open_socket` 失败，单项与整域复跑通过，原因未证实；回环测试首次即绿不冒称 RED。实机 NOT RUN。
- 设计裁决：进程重启后不能凭复用的代际数字解除旧屏障；保留回执、暂停并要求属主核对。未观察到可关联终态时，显式核对不重发原项且仍需独立 resume。为与旧发送共锁，enqueue 改为 async，Q5 必须 await；Q6 必须接刷新冲突码与提示。
- 独立复审未通过：paired 激活重试阻塞事件消费者、认领前临时读取失败无后续唤醒、确定零写错误误记 Unknown 且丢失原因码；源码未提交，第一轮修正中。报告原“变化期间没有事件”的边界应扩大为 Bridge 已观察 controller 变化但队列未采样即恢复的情况；监听句柄意外死亡仍为未覆盖的运行限制。

Q4–Q7、C1–C2、M1–M2、K1–K2、V0 尚待执行。R01–R10 的最终覆盖、逐任务 RED/GREEN 与独立复审将在施工中逐项记入。
