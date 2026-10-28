# Codex APP build `26.1016.61520` 复探取证（2026-10-22 深夜）

> 背景：本机 Desktop 当日**第二次自动升级** `26.1016.51333` → `26.1016.61520`
> （app.asar 落地时间 22:58；运行进程在用户 23:07 真机打断成功之后某次重启切换到新
> build）。`profile_for` 精确匹配再次失效 → `closed()` 写全关 → 手机发送掉 headless
> 被设置拒（`headless_disabled`）、打断按钮按 P2-2 隐藏。按同一方法论重取证，结论：
> **方法表 / 版本协商 / 安全阀 / 打断后状态四项静态证据零漂移 + 活体探针同构**，
> 与 40934/51333 同面登记（`profile.rs` `macos_61520_profile`）。
>
> 当日两连跳（40934 → 51333 → 61520）实证「按 build 精确门控」在自动升级节奏下的
> 锁死代价——系统性解法见 PR #178（L0–L5 分级 + 探测账本），本登记仅为即时解除。

## 一、环境

| 项 | 值 |
| --- | --- |
| Desktop | `/Applications/ChatGPT.app`，`CFBundleShortVersionString = 26.1016.61520`（2026-10-22 23:4x `defaults read` 实读） |
| 打包产物 | `Contents/Resources/app.asar`（597 MB，Oct 22 22:58 构建时间戳） |
| 传输 | Unix socket `~/.codex/ipc/ipc.sock`（活跃） |

## 二、STATIC：四项核对 —— 零漂移

1. **方法表**：tuvis 在册 11 项逐一实测同值（`interrupt-turn`=4、`start-turn`=2、
   `state-changed`=11、`owner-discovery`=1、`ipc-connection-reset`=1 等；
   `client-status-changed` 无数字条目——与 40934/51333 同形态，tuvis 登记 0）。
2. **版本协商**：`je` 函数逐字符同（变量名未变更，仍 `ke`）——缺 `expectedTurnId`
   期望版本 3、带上走基值 4、envelope hostId +1 三个分叉原样。
3. **安全阀**：`if(s!=null&&(s!==f||d?.status!==`inProgress`))return null` 正则逐字符命中。
4. **打断后状态**：`t.status=`interrupted`` 在场。

## 三、MAC-LIVE（2026-10-22 23:4x）

用户桌面持有 3 个线程（`01a1642e` / `01a16439` / `01a1643c`）。对 `01a1643c`：

```text
initialize            → success + clientId（形态同构）
thread-owner-discovery → success + handledByClientId + supportsUntrustedAppInput:true
thread-follower-interrupt-turn (v4, bogus expectedTurnId)
                      → { "interruptedTurnId": null, "ok": true }   // 安全阀生效，零误伤
```

与 40934/51333 档案逐项同构。探针脚本一次性工具（`/tmp/tuvis-51333-probe.py`），不入仓。

## 四、登记决定

- `profile_for` 增 `("macos","26.1016.61520")`，能力面与 51333/40934 同
  （send/state/interrupt 开，command/permissions/blocking_input 关）。
- 相邻 build 负例断言同步（`61521` → None）；51333 登记保留（历史证据）。
- 回滚 = 删 profile.rs 分支 + 构造器 + 测试断言。
