# Codex APP build `26.1016.51333` 复探取证（2026-10-22）

> 背景：本机 Desktop（`/Applications/ChatGPT.app`，bundle id `com.openai.codex`）自动升级
> `26.1013.40934` → `26.1016.51333`（2026-10-22 发现），`profile_for` 精确匹配失效 →
> `closed()` 能力全关。按冻结 spec §3.5「未经验证的 Desktop 版本或方法不得盲目启用」，
> 本轮按 `2026-10-22-codex-interrupt-probe-results.md` 的同一方法论重取证，结论：
> **方法表、版本协商、interrupt handler、安全阀四项静态证据逐字符零漂移**，
> 据此与 40934 同面登记（`profile.rs` `macos_51333_profile`）。
>
> 证据分层沿用本仓口径：**STATIC**（打包产物静态核对）、**MAC-LIVE**（真机 IPC 直连原始帧）。

## 一、环境

| 项 | 值 |
| --- | --- |
| Desktop | `/Applications/ChatGPT.app`，`CFBundleShortVersionString = 26.1016.51333`（2026-10-22 `defaults read` 实读） |
| 打包产物 | `Contents/Resources/app.asar`（597 MB，2026-10-21 22:57 构建时间戳） |
| 传输 | Unix socket `~/.codex/ipc/ipc.sock`（探针时活跃） |
| 帧格式 | 4 字节 LE 长度 + UTF-8 JSON（与 40934 一致） |

## 二、STATIC：方法表逐项 diff —— 零漂移

单遍扫描 app.asar 提取全部 `"thread-follower-*": N` 对（各恰好出现一次）。tuvis 在册方法与 51333 实测值：

| 方法（tuvis 登记表） | 40934 登记 | 51333 实测 | 结论 |
| --- | --- | --- | --- |
| `thread-follower-start-turn` | 2 | 2 | 一致 |
| `thread-follower-load-complete-history` | 1 | 1 | 一致 |
| `thread-follower-command-approval-decision` | 1 | 1 | 一致 |
| `thread-follower-permissions-request-approval-response` | 1 | 1 | 一致 |
| `thread-follower-submit-user-input` | 1 | 1 | 一致 |
| `thread-follower-interrupt-turn` | **4** | **4** | 一致 |
| `thread-owner-discovery` | 1 | 1 | 一致 |
| `thread-stream-following-changed` | 1 | 1 | 一致 |
| `thread-stream-following-status-requested` | 1 | 1 | 一致 |
| `thread-stream-state-changed` | 11 | 11 | 一致 |
| `ipc-connection-reset` | 1 | 1 | 一致 |
| `client-status-changed` | 0（无数字条目） | 无数字条目 | 一致 |

51333 另有 11 个 tuvis 未使用的方法（`steer-turn`=1、`edit-last-user-turn`=2、`update-thread-settings`=2、
queued-messages 族=1、`compact-thread`=1、`read-model-settings`=1、`submit-mcp-server-elicitation-response`=1、
`update-daybreak`=1）——不影响 tuvis（只派发在册方法），仅登记备查。

## 三、STATIC：版本协商 / interrupt handler / 安全阀 —— 逐字符等价

1. **版本协商**（40934 取证中的 `Ae`/`je`/`Me`，51333 中仅表变量更名 `Oe`→`ke`，逻辑逐字符同）：

```js
function Ae(e){let t=ke[e];return t===void 0?0:t}
function je(e,t,n){if(n!=null&&e.startsWith(`thread-follower-`))return(ke[e]??0)+1;
  if(e===`thread-follower-interrupt-turn`&&t!=null&&(!(`expectedTurnId`in t)||t.expectedTurnId==null))return 3;
  let r=ke[e];return r===void 0?0:r}
function Me(e,t,n){return t===je(e,void 0,n)||n==null&&e===`thread-follower-interrupt-turn`&&t===3}
```

2. **interrupt handler**（返回形态与 `goalPauseError` 分支同构）：

```js
case`thread-follower-interrupt-turn`:try{
  let n=await e.interruptConversation(t.params.conversationId,t.params.mode,t.params.expectedTurnId);
  return{method:t.method,result:{interruptedTurnId:n,ok:!0}}
}catch(e){ if(t.params.mode!==`user-stop`||!(e instanceof Error)||!(`interruptedTurnId`in e)
  ||typeof e.interruptedTurnId!=`string`)throw e;
  return{method:t.method,result:{interruptedTurnId:e.interruptedTurnId,goalPauseError:e.message,ok:!0}}}
```

3. **安全阀**（40934 取证中的 `ox`，51333 为 `fx`/`Etn` 两份同构拷贝）：

```js
if(s!=null&&(s!==f||d?.status!==`inProgress`))return null;   // expectedTurnId 不符 → 静默不动作
...
u(a,e=>{let t=px(e,f);t!=null&&(t.status=`interrupted`)})     // 打断后状态仍置 interrupted
```

**推论（实现契约不变）**：始终带 `expectedTurnId`、固定基值 4；`turn_is_terminal` 认
`interrupted` 的前提继续成立。

## 四、MAC-LIVE

### 4.1 `initialize` 握手（2026-10-22 21:49）

```text
→ {"type":"request","requestId":…,"method":"initialize","params":{"clientType":"tuvis"}}
← {"type":"response","requestId":…,"resultType":"success","method":"initialize",
   "handledByClientId":"c34e6c8f-…","result":{"clientId":"c34e6c8f-…"}}
```

回执形态与 40934 记录同构（`resultType: success` + `result.clientId`）。

### 4.2 owner 发现扫面（2026-10-22 21:50）

`thread-owner-discovery`（v1，`{hostId:"local", conversationId}`）对近 3 天 40 个本地
会话 id 逐一探测：全部 `{"resultType":"error","error":"no-client-found"}`——错误契约与
档案一致（探测方法在、路由在），**当时桌面未持有任何本地线程**，属环境状态而非协议漂移。

### 4.3 安全探针（不打断任何回合，2026-10-22 21:57）

桌面持有线程后（用户在 Desktop 打开会话 `01a163fb-7872-7a63-b0f0-11d5b04fbf2e`），
直连 socket 发一发 `thread-follower-interrupt-turn`（**v4**，`mode=user-stop`，
`expectedTurnId=00000000-0000-7000-8000-000000000000`——构造为必不存在的值）：

```text
thread-owner-discovery (v1)
  → {"resultType":"success","handledByClientId":"57f3a9de-…",
     "result":{"supportsUntrustedAppInput":true}}
thread-follower-interrupt-turn (v4, targetClientId=57f3a9de-…)
  → {"resultType":"success","method":"thread-follower-interrupt-turn",
     "result":{"interruptedTurnId": null, "ok": true}}
```

**与 40934 报告 §3.3 逐项同构**：方法名 / 版本 4 / params 形态 / owner 路由全部正确；
`expectedTurnId` 与活动回合不符 → 安全阀生效，未打断任何回合（目标会话全程未受影响）。
探针脚本为一次性诊断工具（`/tmp/tuvis-51333-probe.py`），不入仓；仓内可复跑的等价物是
`src-tauri/tests/codex_ipc_remote.rs` 的打断用例（假 Desktop，零真实副作用）。

## 五、登记决定与回滚

- **登记**：`profile_for` 增 `("macos","26.1016.51333")`，能力位与 40934 同面
  （send/state/interrupt 开，command/permissions/blocking_input 关——未实测不跨 build 迁移）。
- **send 闭环**：由登记后的收官验收提供（§4.3 of 设计文档同款链路）；如验收发现漂移，
  **回滚 = 删 `profile.rs` 登记分支 + 构造器 + 测试断言三处**，无其他耦合面。
- **未验证项**：permissions / command / blocking_input 在 51333 未实测，保持 false；
  steer 仍不做（设计文档 §2.2 边界不变）。
