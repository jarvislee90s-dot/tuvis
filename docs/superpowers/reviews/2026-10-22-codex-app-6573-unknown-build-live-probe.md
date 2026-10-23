# 真机证据：未知 build `26.1016.6573.0` 的能力探测（2026-10-22）

> 关联：#172、`feat/codex-ipc-unknown-build-compat`
> 探针：`src-tauri/tests/codex_ipc_live_probe.rs`（`#[ignore]`，只读）
> 主机：Windows / Codex Desktop `26.1016.6573.0`（**不在 shipped 表 → 走未知 build 路径**）

## 0. 结论

**未知 build 的只读能力探测链路（P0–P3）在真实 Desktop 上用生产代码路径跑通**；
官方 schema 探针也取到了本 build 的决策枚举与内容指纹。随后补做了 **send 真机闭环**
（只发一帧，回执 `Accepted` + 回执内 `turn.status="inProgress"`，事后核对状态真的前进），
四条证据齐备 → **`26.1016.6573.0` 已登记进 shipped 表**（`state`/`send`/`command` = true；
`permissions` / `blocking_input` / Plan / collaboration 无证据，保持关闭）。

## 1. 探测输出（原样）

```json
{
  "platform": "windows",
  "build": "26.1016.6573.0",
  "cliId": "0.163.0-alpha.11",
  "probeKey": "windows|26.1016.6573.0|0.163.0-alpha.11|1",
  "epoch": 1,
  "schema": {
    "known": true,
    "fingerprint": "eff66fde123bdbeddaad14c5b36a86749eee9846f9ab355f33393ead4ca6343c",
    "commandDecisions": ["accept", "acceptForSession", "decline", "cancel"],
    "fileDecisions": ["accept", "acceptForSession", "decline", "cancel"]
  },
  "readProbe": {
    "threadId": "01a153d7-78d4-7d60-9709-5500cd0aed0f",
    "state": "idle",
    "stage": null,
    "reasonCode": null,
    "capabilities": {
      "send": false,
      "state": true,
      "command": false,
      "permissions": false,
      "blockingInput": false
    }
  }
}
```

## 2. 逐项解读

| 事实 | 含义 |
| --- | --- |
| `cliId = 0.163.0-alpha.11` | **CLI 随 Desktop 一起升了**（旧 build `26.1013.4894.0` 上是 `0.163.0-alpha.10.2`）→ **第二指纹不是多余的**：只看 build 会把「同 build、不同 CLI」的证据混用 |
| `probeKey = windows\|26.1016.6573.0\|0.163.0-alpha.11\|1` | 账本键把 platform/build/CLI/epoch 四段都带上 → CLI 漂移自动换键，旧结论不会被误复用 |
| schema `commandDecisions` / `fileDecisions` = `accept, acceptForSession, decline, cancel` | 本 build 的**官方 schema 明确支持 decline**；但 pending 自带的 `availableDecisions` 只列 `accept` / `cancel` → 未知 build 仍按「只发 pending 自己列出的词」给按钮（accept + cancel），decline 不给 |
| `capabilities.state = true` | **只读探测 P0–P3 全通过**（真实 IPC：owner discovery → 临时 following → 首个 snapshot 判据） |
| `capabilities.send = false` | send 未取证（未真发消息）→ 未知 build 走兼容发送开关（默认关） |
| `capabilities.command = false` | 能力位仍由 shipped 表控制；兼容审批走 **schema 档**（决策词来自 pending/schema），不靠能力位 |
| `readProbe.state = "idle"`、`reasonCode = null` | 本探针**未注入账本** → 读侧「取不到」如实报 idle + 无码（不猜「未验证」）——与设计 §4.11「取不到不猜」一致 |

## 3. 6573 的证据总表

| 能力 | 证据 | 等级 |
| --- | --- | --- |
| 读链（P0–P3） | 真机只读探测通过（本次，生产代码路径） | ✅ 可提升 |
| 官方 schema 枚举 | 真机 schema 探针（本次，含内容指纹） | ✅ 可提升 |
| 命令审批 decision | 真机闭环（2026-10-21，accept，`result.ok=true`） | ✅ 可提升 |
| **send（真发消息）** | 真机闭环：只发一帧 → `Accepted`（674ms）+ 回执内 `turn.status="inProgress"` + 状态真的前进 | ✅ 已取证 |
| **unknown build 的审批决策闭环** | 真机：`thread-follower-command-approval-decision` v1 / `decision=accept` → `resultType=success`、`result.ok=true` | ✅ 已取证 |

→ **已办**：`26.1016.6573.0` 写入 shipped 表
（`state=true, send=true, command=true, permissions=false, blocking_input=false`），
该 build 走「已验证」路径，不再付「未知 build 探测 + 兼容发送开关」的代价；
其余未取证 build 继续走未知 build 兼容路径（只读探测 → schema 审批 → 显式兼容发送）。

## 3.1 send 闭环原始输出

```json
{
  "platform": "windows",
  "build": "26.1016.6573.0",
  "delivery": "Accepted",
  "accepted": true,
  "elapsedMs": 674,
  "revisionBefore": 24,
  "messagesBefore": 26,
  "stateAdvanced": true,
  "textLen": 40
}
```

回执原文（截取）：`result.turn = {id:"01a16194-…", status:"inProgress", itemsView:"notLoaded"}`
——回合真的起来了；事后又核对到状态前进（revision/消息数），**不是**只有 `Accepted` 就报成功。

## 4. 复现

```powershell
cd src-tauri
$env:TUVIS_CAPTURE_THREAD = "<thread-uuid>"   # 可选：不设则自动枚举最近会话
cargo test --features marker-helper --test codex_ipc_live_probe -- --ignored --nocapture
```

探针只读：只发 `thread-owner-discovery` / `thread-stream-following-changed`（含撤销）/
`thread-stream-following-status-requested`；**不发** start-turn / 审批决策 / 问答作答。
