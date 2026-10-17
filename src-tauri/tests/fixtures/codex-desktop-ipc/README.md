# Codex Desktop IPC 契约夹具（脱敏）

供 Rust 集成测试使用的最小协议夹具。每个文件标注来源级别：

- **OBSERVED**：Mac 探针（本仓 `probe/mac-ipc-probe-report.md`）实机捕获的结构/字段，
  内容替换为合成占位值。
- **CODE**：从 Desktop bundle 静态读到的结构（Mac 报告标注 `CODE` 的部分）。
- **SYNTHETIC**：合成兼容样本——协议字段依据计划/探测记录，但该具体取值未实机出现。
- **NOT_TRIGGERED**：Windows probe 观察到、Mac 未触发的方法面，字段按计划记录合成。

不携带任何真实提示词、会话内容、凭据、家目录路径；thread/owner id 一律占位 UUID
（`00000000-…` 形态）。

| 文件 | 来源 | 说明 |
| --- | --- | --- |
| `macos-snapshot.json` | OBSERVED 骨架 + SYNTHETIC 内容 | `thread-stream-state-changed` v11 snapshot：`turns=[]`（实机事实）+ `turnHistory.history.entitiesByKey/islands` 正史 + permissions pending（整数 request id，OBSERVED id=12 形态） |
| `windows-snapshot.json` | NOT_TRIGGERED/合成 | Windows 侧同构 snapshot（双端共用协议）；pending 为 command 审批（整数 id） |
| `patches.json` | OBSERVED 结构（reviewer 2026-10-17 实读 Windows 原始日志，脱敏） | `{baseRevision,revision,patches[]}`，数组路径，**path 直接对 conversationState 本体寻址（无 `conversationState` 前缀——真实形状，R1 返工定案）**；含 add/replace/remove、流式同 item replace 与 `path=["threadRuntimeStatus","type"]` 的 reviewer 复现样本 |
| `command.json` | SYNTHETIC（Mac NOT_TRIGGERED；表面 `thread-follower-command-approval-decision` 来自 CODE） | `item/commandExecution/requestApproval` 请求项 |
| `permissions.json` | OBSERVED（Mac POC-5，无 `availableDecisions`） | `item/permissions/requestApproval` 请求项 |
| `user-input.json` | CODE + SYNTHETIC | blocking `item/tool/requestUserInput` 请求项（含 isOther/isSecret 双问题；request id 字符串形态为 SYNTHETIC——Mac 实测 requestId 为整数） |

**实机缺口（诚实申报）**：Mac 的 command 变体与 blocking input、双端 Desktop 重启恢复
未实测；夹具只保证协议字段契约，不构成对应能力已验证的证据（生产 profile 的
capability flag 相应保持 false）。
