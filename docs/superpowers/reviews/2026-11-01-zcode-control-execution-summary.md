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

Q2–Q7、C1–C2、M1–M2、K1–K2、V0 尚待执行。R01–R10 的最终覆盖、逐任务 RED/GREEN 与独立复审将在施工中逐项记入。
