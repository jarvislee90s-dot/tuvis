# ZCode 探测回报模板 v1

每轮一份。中文填写，缺测项不得删除。真实 ID／路径用别名，原值映射留本机。

## A. 轮次与身份

- schema_version: 1
- run_id:
- platform: macos / windows
- phase: P0（后续须有明确开放指令）
- 开放指令与范围:
- 起止时间（含时区）:
- OS／架构:
- APP version／build:
- 随包 CLI version／SHA256（不能安全运行版本命令则写未测）:
- 独立 PATH CLI version／SHA256（没有则写无）:
- 入口、配置路径别名及存在性:
- Tuvis installed version／source SHA／分支／脏状态:
- ZCode source SHA: 3c5b227649371507ddc4eb46ac5133afe6db1cab
- 源码与安装包对应性: PROVEN / PARTIAL / UNPROVEN（理由与证据）
- 隔离状态／私有材料目录别名:
- 是否改源码／配置、调用模型、写真实数据、终止进程:

## B. 用例汇总

结论仅 PASS／FAIL／PARTIAL／BLOCKED／NOT_TESTED／NOT_APPLICABLE。另记 SOURCE／PACKAGE／OBSERVED／FIXTURE／RELAY；纯源码不能算运行 PASS。

| ID    | 对象／作用域 | 预期 | 实际与最小反例 | 结论       | 证据类型／编号 | 功能／限制 |
| ----- | ------------ | ---- | -------------- | ---------- | -------------- | ---------- |
| P0-01 |              |      |                | NOT_TESTED |                |            |
| P0-02 |              |      |                | NOT_TESTED |                |            |
| P0-03 |              |      |                | NOT_TESTED |                |            |
| P0-04 |              |      |                | NOT_TESTED |                |            |
| P0-05 |              |      |                | NOT_TESTED |                |            |
| P0-06 |              |      |                | NOT_TESTED |                |            |

P1／P2／P3／P4：未开放，NOT_TESTED。后续轮次按手册替换本表用例行。

## C. 逐项证据卡（每个执行 ID 一张）

- case_id／attempt_id:
- 假设与预期反例:
- 前置状态、目标别名、数据落点:
- 可复现步骤及白名单命令骨架（秘密用占位符）:
- 输出结构、退出码、耗时（不含正文／凭据）:
- 查询条件、候选数、接受数、拒绝原因数（如适用）:
- 源码固定 SHA 链接＋行号、调用链、测试位置:
- 发行包核对与差异:
- 运行前后对照（P0 无主动写入，外部并发更新注明）:
- 私有 raw 引用编号／hash:
- sanitized 附件相对路径／hash／脱敏项:
- 不能证明什么、下一最小实验:

## D. 能力结论

逐入口／平台／指纹填写 GO／GO_WITH_LIMITS／NO_GO／NEEDS_EVIDENCE。源码推断填 NEEDS_EVIDENCE。

| 能力             | 平台／入口／指纹 | 结论           | 用例证据 | 缺口／限制 |
| ---------------- | ---------------- | -------------- | -------- | ---------- |
| 看板与状态读取   |                  | NEEDS_EVIDENCE |          |            |
| 正文／附件读取   |                  | NEEDS_EVIDENCE |          |            |
| 已有会话发送     |                  | NEEDS_EVIDENCE |          |            |
| 自建无头回合取消 |                  | NEEDS_EVIDENCE |          |            |
| APP 原生回合打断 |                  | NEEDS_EVIDENCE |          |            |
| 审批请求展示     |                  | NEEDS_EVIDENCE |          |            |
| 审批批准／拒绝   |                  | NEEDS_EVIDENCE |          |            |
| 结构化问答       |                  | NEEDS_EVIDENCE |          |            |
| 在册工作区新建   |                  | NEEDS_EVIDENCE |          |            |

## E. 回传与收尾

- 最多五条主要发现:
- 源码与发行版差异:
- 主干／#117 的已交付／待保留／过时／冲突清单:
- 下一阶段最小步骤、预期、负例、隔离、副作用:
- 需要用户决定的事项（尤其协议范围／测试项目注册）:
- 临时产物、本轮子进程及收尾状态（没有也写无）:
- sanitized 附件索引（raw 留机）:

回报后停止，不自行实现、开放后续阶段、改全局权限或发云端工单。
