# Codex APP 远程停止回合 · Probe 取证记录（2026-10-22）

> 本文件是 `docs/engineering/specs/2026-10-22-codex-remote-interrupt-design.md` 的证据附录：
> 记录 `thread-follower-interrupt-turn` 的协议事实与真机验收结果。
> **不改写**既有探测报告（`2026-10-19-codex-desktop-ipc-windows-4894-probe-results.md` 等），
> 也不把未测的平台/build 说成已测。
>
> 证据分层（沿用本仓既有口径）：
> - **MAC-LIVE**：macOS 桌面真机直连官方 Desktop IPC 通道的原始帧 / 原始回包；
> - **STATIC**：上游打包产物的静态核对（不替代本机实测，仅用于定位契约）；
> - **FIXTURE**：仓库内脱敏夹具。

## 1. 目标

冻结 spec（提交 `e39d7665`）§5 把 interrupt / steer 列为 out-of-scope，要求「在后续专项 Probe / Spec 中独立增加」。本轮 Probe 只回答一件事：

> 官方 Desktop 是否提供「停止当前回合」的方法？若提供，方法名 / 版本 / 参数 / 返回 / 打断后的回合状态各是什么？打断是否可被安全阀约束？

## 2. 环境

| 项 | 值 |
| --- | --- |
| Desktop | `/Applications/ChatGPT.app`（Codex APP），`CFBundleShortVersionString = 26.1013.40934` |
| 传输 | Unix domain socket `~/.codex/ipc/ipc.sock` |
| 帧格式 | 4 字节 LE 长度 + UTF-8 JSON |
| 宿主 | macOS（darwin arm64） |
| 对端 thread | `01a159d5-b979-77b0-8242-e006e13bf266`（用户在跑的会话；**探针全程未打断它**） |

## 3. 协议事实

### 3.1 STATIC：Desktop 自带方法表

打包产物内嵌的方法版本表（节选）：

```text
"thread-follower-start-turn": 2
"thread-follower-steer-turn": 1
"thread-follower-interrupt-turn": 4
"thread-follower-submit-user-input": 1
...
```

版本协商函数（同产物）：

```js
function Ae(e, t, n) {                      // (method, params, envelopeHost)
  if (n != null && e.startsWith(`thread-follower-`)) return (Oe[e] ?? 0) + 1;
  if (e === `thread-follower-interrupt-turn` && t != null &&
      (!(`expectedTurnId` in t) || t.expectedTurnId == null)) return 3;
  let r = Oe[e]; return r === void 0 ? 0 : r;
}
function je(e, t, n) {                      // (method, version, envelopeHost) → 是否接受
  return t === Ae(e, void 0, n) ||
         (n == null && e === `thread-follower-interrupt-turn` && t === 3);
}
```

**推论（实现必须遵守）**：本地 follower 请求（envelope 不发 hostId）→ 期望版本 **4**；`params` 缺 `expectedTurnId` 时 Desktop 期望版本 3 —— 故实现**始终带上 `expectedTurnId`**，固定走基值 4。

### 3.2 STATIC：请求处理与打断状态机

```js
case `thread-follower-interrupt-turn`:
  try {
    let n = await e.interruptConversation(t.params.conversationId,
                                          t.params.mode, t.params.expectedTurnId);
    return {method: t.method, result: {interruptedTurnId: n, ok: true}};
  } catch (e) { /* mode=user-stop 且带 interruptedTurnId 时仍回 ok */ }
```

```js
async function ox({..., expectedTurnId: s, ...}) {
  let d = <当前活动回合>, f = d?.turnId ?? null;
  if (s != null && (s !== f || d?.status !== `inProgress`)) return null;   // ← 安全阀
  ...
  u(a, e => { let t = <按 turnId 认领的回合>; t != null && (t.status = `interrupted`) });
}
```

**安全阀**：`expectedTurnId` 与当前活动回合不符 → 直接返回 null，**不做任何打断**（且 `interruptConversation` 的 `finally` 分支在此情形下不触发子代理清理——`n == null || r != null` 为假）。

**打断后状态**：回合 status 被置 **`interrupted`**（这是 tuvis 终态门必须同批放行的原因）。

### 3.3 MAC-LIVE：安全探针（不打断任何回合）

探针直连 `~/.codex/ipc/ipc.sock`，走 `initialize` → `thread-owner-discovery` → `thread-follower-interrupt-turn`，
**故意传一个不存在的 `expectedTurnId`**：

```text
initialize  → {resultType: success, result: {clientId: 2ba11614-…}}
discovery   → {handledByClientId: 45674868-…, result: {supportsUntrustedAppInput: true}}
interrupt   → {resultType: success, method: thread-follower-interrupt-turn,
                result: {interruptedTurnId: null, ok: true}}
```

**结论**：方法名 / 版本 4 / params 形态 / owner 路由全部正确；因 `expectedTurnId` 与活动回合不符，Desktop 未打断任何回合（安全阀生效）。**用户当时正在跑的长回合未受影响。**

## 4. MAC-LIVE：端到端验收

在**另一个空闲会话**（`01a15f01-aab5-7910-8f60-917d5ace7a61`）上跑，用户主线程 `01a159d5-…` 全程未触碰：

| 步 | 观测 |
| --- | --- |
| 发送消息 | `POST /m/api/v1/session-send` → 200 `{status: desktop_ipc, receipt: {delivery: accepted, route: desktop_ipc, turnId: 01a15f27-1438-76c3-8330-628ebf91471f}}` |
| 屏障在册 | `GET /m/api/v1/codex/queue` → `{blockedReasonCode: "native_turn_pending", barrierActionIds: [<该 action>]}`；DB 在册 turnId = `01a15f27-…` |
| 打断 | `POST /m/api/v1/codex/turn/interrupt` → 200 `{ok: true, interruptedTurnId: "01a15f27-1438-76c3-8330-628ebf91471f", expectedTurnId: "01a15f27-1438-76c3-8330-628ebf91471f"}` |
| 屏障释放 | 队列视图 → `{actions: [], blockedReasonCode: null, barrierActionIds: []}`；DB `compat_outstanding = 0` |
| 日志 | `codex_control: Turn 终态证据齐备，释放顺序限制 action=… turn=Some("01a15f27-…")` |

**闭环成立**：打断 → 回合终态（`interrupted`）→ 在册屏障释放 → 队列可接力。

## 5. 未验证项（如实登记，不宣称）

| 项 | 状态 | 后果 |
| --- | --- | --- |
| Windows `26.1013.4894.0` / `26.1013.3656.0` 的 `interrupt` | **未实测** | 能力位保持 false → 端点 503 拒发、零 IPC 写入 |
| macOS `26.1013.31256` 的 `interrupt` | **未实测** | 同上 |
| `thread-follower-steer-turn`（回合中追加指令） | **未实现**（同属冻结 spec §5 out-of-scope） | 不在本期范围 |
| `AgentMessage.questions[]` 形态的提问在手机端应答 | **未实现**（非协议 request，pending 通道拿不到） | 见专项 spec §5.3 |

## 6. 复跑方式

- 安全探针（不打断任何回合）：`python3 /tmp/tuvis-interrupt-probe.py <conversationId> [bogusTurnId]`
- 端到端（会真打断目标会话的当前回合，仅限空闲测试会话）：`python3 /tmp/tuvis-interrupt-verify.py [sessionId]`

两个脚本不入仓（一次性诊断工具）；仓内可复跑的等价物是 `src-tauri/tests/codex_ipc_remote.rs` 的三个打断用例（假 Desktop，零真实副作用）。
