# Codex Desktop IPC 契约夹具（脱敏）

供 Rust 集成测试使用的最小协议夹具。每个文件标注来源级别：

- **OBSERVED**：Mac 探针（本仓 `probe/mac-ipc-probe-report.md`）实机捕获的结构/字段，
  内容替换为合成占位值。
- **CODE**：从 Desktop bundle 静态读到的结构（Mac 报告标注 `CODE` 的部分）。
- **SYNTHETIC**：合成兼容样本——协议字段依据计划/探测记录，但该具体取值未实机出现。
- **NOT_TRIGGERED**：Windows probe 观察到、Mac 未触发的方法面，字段按计划记录合成。

不携带任何真实提示词、会话内容、凭据、家目录路径；thread/owner id 一律占位 UUID
（`00000000-…` 形态）。

| 文件                    | 来源                                                                                       | 说明                                                                                                                                                                                                                                                      |
| ----------------------- | ------------------------------------------------------------------------------------------ | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `macos-snapshot.json`   | OBSERVED 骨架 + SYNTHETIC 内容                                                             | `thread-stream-state-changed` v11 snapshot：`turns=[]`（实机事实）+ `turnHistory.history.entitiesByKey/islands` 正史 + permissions pending（整数 request id，OBSERVED id=12 形态）                                                                        |
| `windows-snapshot.json` | NOT_TRIGGERED/合成                                                                         | Windows 侧同构 snapshot（双端共用协议）；pending 为 command 审批（整数 id）                                                                                                                                                                               |
| `patches.json`          | OBSERVED 结构（reviewer 2026-10-17 实读 Windows 原始日志，脱敏）                           | `{baseRevision,revision,patches[]}`，数组路径，**path 直接对 conversationState 本体寻址（无 `conversationState` 前缀——真实形状，R1 返工定案）**；含 add/replace/remove、流式同 item replace 与 `path=["threadRuntimeStatus","type"]` 的 reviewer 复现样本 |
| `command.json`          | SYNTHETIC（Mac NOT_TRIGGERED；表面 `thread-follower-command-approval-decision` 来自 CODE） | `item/commandExecution/requestApproval` 请求项                                                                                                                                                                                                            |
| `permissions.json`      | OBSERVED（Mac POC-5，无 `availableDecisions`）                                             | `item/permissions/requestApproval` 请求项                                                                                                                                                                                                                 |
| `user-input.json`       | CODE + SYNTHETIC                                                                           | blocking `item/tool/requestUserInput` 请求项（含 isOther/isSecret 双问题；request id 字符串形态为 SYNTHETIC——Mac 实测 requestId 为整数）                                                                                                                  |

## 目录索引（路径保持现状）

| 目录                                | 内容                                                     | 构建/能力                                                |
| ----------------------------------- | -------------------------------------------------------- | -------------------------------------------------------- |
| 根目录 JSON                         | command/permissions/user-input/patches/snapshot 兼容夹具 | 合成或 OBSERVED 骨架；不单独作为能力证据                 |
| `artifacts/`                        | 正文产物关联的合成案例                                   | SYNTHETIC                                                |
| `observed/`（根）                   | canonical 金样、Windows 4894/6573 证据                   | macOS 26.1013.40934；Windows 26.1013.4894 / 26.1016.6573 |
| `observed/20261026-macos/`          | F01—F08 Mac 主探测 + `contract-fragments/`               | macOS 26.1021.31345                                      |
| `observed/20261026-macos-followup/` | F03 自由回答/异步答题追加样本                            | macOS 26.1021.31345（同一指纹）                          |
| `schema/`                           | 随包 CLI 生成的官方 approval schema                      | 审批 decision 枚举                                       |
| `observed/README.md`                | 金样来源、采集方式与缺口                                 | 事实索引；不替代能力验收                                 |

测试使用方：`src-tauri/tests/codex_desktop_ipc.rs`（协议/投影）、`codex_ipc_remote.rs`（HTTP 与竞态）、以及各模块内联夹具测试。路径保持现状，新增 build 时在对应 `observed/` 目录加样本并在本表登记。

**实机缺口（诚实申报）**：Mac 的 command 变体与 blocking input、双端 Desktop 重启恢复
未实测；夹具只保证协议字段契约，不构成对应能力已验证的证据（生产 profile 的
capability flag 相应保持 false）。
