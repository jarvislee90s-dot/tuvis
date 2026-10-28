# 2026-10-26 Codex Desktop F01—F08 观测夹具（Mac）

来源运行（本机仓外目录，**不上传、不提交 raw**）：

| 运行         | 目录                                                                         | 用途                                                                                                                              |
| ------------ | ---------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------- |
| Mac 主探测   | `~/Documents/tuvis-verify-logs/codex-f01-f08-mac-20261026T174302+0800/`      | 阻塞问答（单/多题、多值、自由文本）、命令/文件审批、打断、异步题与信封、跨端与 UI 缺口                                            |
| Mac 追加     | `~/Documents/tuvis-verify-logs/codex-f03-mac-followup-20261026T203938+0800/` | 空闲异步选答/自由回答、紧凑 `questionItemId` 配对、超时已落地、`declined` 终态                                                    |
| Windows 追加 | 报告记录 `codex-f03-win-followup-20261026T1745+0800`                         | **未归档**：该压缩包缺少索引引用的脱敏样本，本轮不据其声称任何 Windows 闭环；Windows 能力在 T01 只做「当前契约未核对 → 保持关闭」 |

运行身份（主探测 `manifest.json`）：Desktop `26.1021.31345`（bundle
`/Applications/ChatGPT.app`，`CFBundleIdentifier=com.openai.codex`），随包 CLI
`0.164.0-alpha.11.1`，探测键 `macos|26.1021.31345|0.164.0-alpha.11.1|1`。

## 契约结构片段的取法与等级

`contract-fragments/` 是**静态结构**（`app.asar` 主进程打包产物里与协议相关的
最小片段），不是运行日志；不含会话正文、命令、路径或凭据：

| 片段                                  | 来源（包内）                                                             | 用途                                                                      |
| ------------------------------------- | ------------------------------------------------------------------------ | ------------------------------------------------------------------------- |
| `version-table.js`                    | `/.vite/build/src-Dl21kCYQ.js`（方法版本表 `Oe` 与版本协商 `Ae`/`je`）   | start-turn v2、interrupt v4、decision v1、submit-user-input v1 的版本判据 |
| `start-turn-handler.js`               | `/.vite/build/main-0FVbxCQU.js`（`thread-follower-start-turn` 分发分支） | Send 的 handler 形状（`conversationId+turnStart` → `result.result`）      |
| `decision-and-user-input-handlers.js` | 同上（command/file decision、submit-user-input 分支）                    | 审批与阻塞问答的 handler 形状（`requestId+decision/response` → `ok`）     |
| `interrupt-handler.js`                | 同上（`thread-follower-interrupt-turn` 分支）                            | Interrupt 的 v4 形状（`mode+expectedTurnId` → `interruptedTurnId+ok`）    |
| `async-envelope.js`                   | 同上（`uxe`/`dxe`/`Ay`/`fxe`/`pxe`/`mxe`）                               | 异步题信封标签、`questionItemId` 合成、单条 `min(1)` 解析                 |
| `start-turn-request.js`               | 同上（start-turn 请求构造）                                              | 请求侧 `conversationId+turnStart` 形状                                    |

片段等级：**CODE（静态结构）**。它们是"结构匹配"规则的来源，**不是**"能力已验"
的证据——能力是否放行仍由 T02 的逐操作策略按当前包重新匹配决定。

复现方法（只读，不执行包内 JS）：

```bash
# 1) 读 ASAR 目录头（[4][header_size][..][json_size]），2) 只取 /.vite/build/*.js，
# 3) 在片段内按方法版本表 + handler 形状 + 语义规则做有限词法匹配。
cargo test --lib f08p_t01_local_installed_package_smoke -- --ignored --nocapture
```

本机实测（2026-10-27，`/Applications/ChatGPT.app`，包 SHA-256
`130145706a912db74e4d260ad7538f10f1e05e88af955f9afba42cdf01e64606`）：
`Send v2 / CommandApproval v1 / FileChangeApproval v1 / BlockingInput v1 /
AsyncAnswerIdle v2 / Interrupt v4` 六项全部匹配。

## 行为样本（脱敏）

| 文件                                   | 等级                    | 说明                                                                                                                                                                                                     |
| -------------------------------------- | ----------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `blocking-multi-answer.sanitized.json` | OBSERVED（ID/文本占位） | 阻塞三题（`m1`/`m2`/`m3`）+ 多值（`m1=["甲","丙"]`）+ 自由文本（`m3`）；`userInputResponse.completed=true`、`requests=[]`、`runtime=idle`；发送形态 `answers[qid].answers[]`、存储形态 `answers[qid]=[]` |
| `async-question-ledger.sanitized.json` | OBSERVED                | 异步题：无 `answered`/`resolved`/`isBlocking` 位；`questionItemId = JSON.stringify(["request_user_input_async", itemId, index])`；普通正文对照不清卡                                                     |
| `async-answer-envelope.sanitized.json` | OBSERVED                | 正式信封 `<send_user_message_question_reply>\n[{questionItemId,question,answer}]\n</…>` 经 start-turn v2 一次提交，回执 `Accepted`                                                                       |
| `interrupt.sanitized.json`             | OBSERVED                | `thread-follower-interrupt-turn` v4 + `expectedTurnId` → `{interruptedTurnId, ok:true}`                                                                                                                  |
| `command-file-decision.sanitized.json` | OBSERVED                | 手机端 `pending-response` → command decision v1 `accept` → 上游执行（哨兵文件），`requests=0`、turn completed                                                                                            |
| `multi-device-sync.sanitized.json`     | OBSERVED                | 跨端处理/收敛的只读事实（revision、requests、items 变化）                                                                                                                                                |
| `method-version-ledger.sanitized.json` | OBSERVED                | 15 份写回执的方法/版本实跑计数                                                                                                                                                                           |

## 追加运行样本（`../20261026-macos-followup/`）

| 文件                                | 说明                                                      |
| ----------------------------------- | --------------------------------------------------------- |
| `h-freecompact.json`                | 空闲异步自由回答闭环（含超时回执形态）                    |
| `f03-answer-route-static.json`      | 答题路由的静态取证（信封合成规则）                        |
| `questionitemid-format-hazard.json` | `questionItemId` 紧凑/带空格配对差异（格式必须精确一致）  |
| `pairing-exact-vs-spaced.json`      | 精确配对 vs 带空格配对的差异样本                          |
| `j-decline.json`                    | 用户拒绝的终态样本（`declined` 是真实命令终态）           |
| `f04-decline-and-permissions.json`  | `decline` 与「请求权限」标题不等于 permissions 协议的证据 |

## 纪律

- 不提交 `raw/`、截图、真实 thread/owner/turn ID、真实题库文本或家目录路径。
- 无题形样本（如"无 options 真实题"）**不得**在这里拼成 LIVE；若需要，另放
  `synthetic/` 并标明 SYNTHETIC。
- Windows 当前无可用脱敏归档 → 本目录不产出 Windows 契约样本；Windows 能力在
  实现侧按"当前契约未核对"保持关闭（见 spec §13.2）。
