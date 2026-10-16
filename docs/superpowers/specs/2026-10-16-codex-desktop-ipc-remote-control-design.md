# Codex Desktop IPC 远程控制 · 需求说明书

> 日期：2026-10-16  
> 状态：待用户评审  
> 性质：需求与架构设计（实现细节由后续 `writing-plans` 另行确定）  
> 主路径：Codex Desktop Owner IPC  
> 兜底路径：PR #117 Codex APP Headless Injection  
> 关联项目：`jarvislee90s-dot/tuvis`

---

## 1. 背景与目标

当前 Tuvis 已通过 PR #117 建立 Codex APP 的无头消息注入能力，但该路径本质上是独立的 Codex execution / queue 通道，并不能完整控制官方 Codex Desktop 当前打开的原生会话。

本设计新增 **Codex Desktop IPC 主控制通道**，直接连接官方 Codex Desktop 当前持有 Thread 的本地 IPC，使 Tuvis 能够在不改变用户继续使用官方 Desktop App 的前提下，实现：

- 查看官方 Desktop Thread 的实时状态；
- 远程发送消息；
- 接收并处理命令审批；
- 接收并回答结构化用户输入；
- 在 Desktop 重启、owner 变化后自动恢复控制关系。

同时保留 PR #117 作为普通消息的兼容兜底。

最终形成：

```text
                    Tuvis Remote
                         │
                         ▼
                Codex Control Router
                   │            │
             Primary A       Fallback
             Desktop IPC     PR #117
                   │            │
                   ▼            ▼
             Official APP    Headless
             Thread          Execution
```

**核心原则：A 是官方 Desktop Thread 的真正控制面；PR #117 是降级通道，两者不混淆产品语义。**

---

## 2. 功能需求

### F1 · 官方 Desktop Thread 接入

Tuvis 能发现并连接当前运行中的官方 Codex Desktop，并识别指定 Thread 当前是否由 Desktop 持有。

要求：

- 一个 Desktop owner 可以持有多个 Thread；
- Thread owner 是临时状态，不是 Thread 的永久属性；
- Desktop 重启或 owner 消失后，Tuvis 必须重新发现；
- 未被 Desktop 当前持有的历史 Thread 不视为可直接 IPC 控制。

---

### F2 · Thread 实时状态

Tuvis 能订阅官方 Desktop Thread 的实时状态，并将其转换为统一的 Tuvis Thread 状态。

至少支持：

- 当前 Turn 状态；
- streaming 内容；
- Turn 历史；
- pending request；
- 完成、失败等终态；
- Desktop 连接/owner 状态变化。

移动端不直接消费 Codex 原始 IPC state，而使用 Tuvis 自己的状态投影。

---

### F3 · 远程发送消息

当 Thread 存在有效 Desktop owner 时：

```text
手机
 → Tuvis
 → Desktop IPC
 → 原 Desktop Thread
```

发送消息必须继续由官方 Codex Desktop 执行，而不是创建第二个独立 execution session。

同一 Thread 的远程操作必须保持有序，避免多个 Turn 或交互请求互相覆盖。

---

### F4 · 远程命令与权限审批

当官方 Codex Desktop 产生 command approval 或 permissions approval request 时，Tuvis 应将其识别为待处理请求并提供远程审批。

本期同时覆盖 `item/commandExecution/requestApproval` 与 `item/permissions/requestApproval`；由协议适配层将批准/拒绝映射到各自的原生应答，不混用 wire payload。（2026-10-16 用户裁决：按会话纳入两类审批，修正原第 5 节范围矛盾。）

支持的产品行为：

- 批准；
- 拒绝；
- 审批结果返回原始 Thread；
- 原 Turn 在审批后继续执行；
- 审批动作可审计。

**审批不是普通消息发送的替代品。**

如果当前环境不具备 Desktop IPC 审批能力，Tuvis 必须明确显示“不支持远程审批”，不得伪造一个普通消息作为审批。

---

### F5 · 结构化用户输入

当 Codex Desktop 请求用户进行结构化回答时，Tuvis 应展示对应的问题、选项和回答状态，并将用户选择提交回原始 Turn。

当前确认范围：

- `request_user_input`；
- Plan collaboration mode。

Default 等不产生该请求的模式，不应伪造能力。

---

### F6 · 连接恢复与状态恢复

Desktop IPC 是易失连接。

当发生：

- ChatGPT.exe 重启；
- Desktop owner 变化；
- IPC 连接断开；
- Thread owner 失效；

Tuvis 应进入恢复流程：

```text
断开
 → 重连
 → 重新初始化
 → 重新发现 owner
 → 重新订阅 Thread
 → 恢复状态
```

旧 owner 身份不得继续用于后续控制。

---

### F7 · 主路径 / 兜底路径路由

建立统一 `CodexControlRouter`。

对于普通消息：

```text
Desktop IPC + 有效 owner
        ↓
      IPC

否则
        ↓
     PR #117
```

对于审批、结构化用户输入等 Desktop IPC 专属能力：

```text
Desktop IPC
    ↓
支持 → 执行
不支持 → 明确告知用户
```

**禁止在“不确定请求是否已经送达”的情况下自动切换到 PR #117。**

这是为了避免一次用户操作被执行两次。

---

### F8 · 能力与失败状态可见

Tuvis 应根据当前 Desktop 版本、owner、Thread 状态和已知协议能力判断：

```text
可控制
可发送但不支持交互
仅可使用兼容模式
当前不可控制
```

用户看到的必须是实际能力，而不是“理论上支持”。

例如：

- Thread 未被 Desktop 打开：普通消息可以考虑走兜底；审批不能假装支持；
- Desktop 版本未知：关闭未经验证的 IPC 能力；
- 审批没有产生：不应认为审批链路异常；
- IPC 请求结果未知：不得自动重发。

---

## 3. 技术架构

### 3.1 控制层

新增独立的 Codex Desktop IPC Adapter，与现有 Headless Adapter 平行存在：

```text
CodexControlRouter
├── DesktopIpcAdapter
│   ├── IPC Connection
│   ├── Owner Registry
│   ├── Thread State
│   └── Pending Requests
│
└── HeadlessAdapter
    └── PR #117
```

Desktop IPC **不归入 Headless**，因为它控制的是已经运行的官方 Desktop，而不是自行启动的无头执行进程。

---

### 3.2 Owner Registry

核心关系：

```text
(hostId, threadId)
        ↓
 current ownerClientId
```

Owner 是临时身份，需要记录其生命周期和失效状态。

同时支持：

```text
ownerClientId
    ↓
多个 Thread
```

Owner 失效后，旧映射全部失效并重新 discovery。

---

### 3.3 Thread State 层

IPC 原始 state 不直接进入远程 API。

采用：

```text
IPC State
    ↓
Thread State Mirror
    ↓
Tuvis State Projection
    ↓
Remote API / Mobile UI
```

负责统一处理 snapshot、增量状态以及 pending requests。

---

### 3.4 Pending Request 层

审批和结构化用户输入统一视为：

```text
Thread
  └── pending requests
```

而不是设计成两个完全独立的消息系统。

每个请求必须至少能够唯一关联：

```text
threadId
requestId
owner
```

其中 `requestId` 不假设为 UUID，必须按照协议实际类型处理。

---

### 3.5 跨平台 IPC 与协议基线

经 Windows/macOS 独立探测，Architecture A 的核心控制链路已在两个平台验证。

**平台接入：**

- Windows：Named Pipe，`\\.\pipe\codex-ipc`
- macOS：Unix Domain Socket，`$CODEX_HOME/ipc/ipc.sock`，默认 `~/.codex/ipc/ipc.sock`
- 两端共用相同的 IPC 帧格式、请求信封、Owner Discovery、Thread Following 和 Follower Control 模型。

**协议约束：**

1. IPC 使用按方法区分的版本门禁；版本不匹配可能表现为 `no-client-found`，不能直接判定 owner 不存在。
2. Thread 状态需要先声明 following；Turn 内容以 `turnHistory` 为重要数据源，待处理交互来自 `conversationState.requests`。
3. Pending Request ID 必须按不透明标识处理，兼容整数和字符串；不得假设 `availableDecisions` 始终存在。
4. Owner 身份随连接生命周期变化，断开或 Desktop 重启后必须重新发现。
5. Approval 存在不同协议类型，控制层应统一产品语义，并由协议适配层处理差异。
6. 未经验证的 Desktop 版本或方法不得盲目启用对应能力。

已验证版本：

| 平台    | Codex Desktop  |
| ------- | -------------- |
| Windows | 26.1013.3656.0 |
| macOS   | 26.1013.31256  |

以上能力属于**当前协议基线**；Codex Desktop 升级后需要通过版本能力门控避免盲目使用未经验证的协议。

---

## 4. 安全与可靠性约束

### S1 · IPC 不作为远程网络接口

`codex-ipc` 只允许 Tuvis 本机访问。

不得将 Named Pipe 直接暴露给：

- Internet；
- WebSocket；
- HTTP；
- VPN / tunnel；
- 其他远程客户端。

---

### S2 · IPC 自身不提供安全边界

Codex Desktop IPC 不具备足够的认证/授权能力。

因此真正的远程权限边界必须位于：

```text
Remote Client
    ↓
Tuvis Authentication / Authorization
    ↓
CodexControlRouter
    ↓
Desktop IPC
```

`clientType`、`clientId` 均不得作为 Tuvis 的认证依据。

---

### S3 · target owner 不由客户端指定

远程客户端只能指定：

```text
threadId
```

实际：

```text
ownerClientId
targetClientId
```

必须由 Tuvis 根据当前 Owner Registry 解析。

---

### S4 · 审批必须绑定当前上下文

审批动作必须绑定：

```text
threadId
+
requestId
+
current owner
```

上下文不匹配时拒绝执行。

---

### S5 · 未知投递结果不得自动重试

对于可能改变 Codex 状态的操作：

```text
start turn
approval decision
user input
```

如果请求已经发出但结果未知：

```text
UNKNOWN_DELIVERY
```

不得自动重试，也不得自动切换 PR #117。

---

### S6 · 审计

远程发送、审批、结构化回答以及路由降级均应留下 Tuvis 审计记录。

审计重点记录：

- 操作；
- Thread；
- 设备；
- 路由；
- 请求关联 ID；
- 结果。

不得记录 API key、cookie 等敏感凭据。

---

## 5. 不在本 Spec 范围

以下能力暂不作为本次功能承诺：

- interrupt / steer；
- update-thread-settings；
- 远程切换 collaboration mode；
- file approval；
- 更复杂的 approval decision；
- 多 Desktop Window 的高级 owner 协同；
- 将 Codex IPC 代理到网络；
- 用 PR #117 模拟 Desktop IPC 专属交互。

这些能力可以在后续专项 Probe / Spec 中独立增加。

---

## 6. 实施分期

本 Spec 只冻结功能和架构，不冻结代码文件、函数、具体类型或测试实现。

后续实施计划建议按以下能力阶段展开：

```text
Phase 1
IPC 基础连接 + 协议能力门控

        ↓

Phase 2
Owner Discovery + Thread State

        ↓

Phase 3
Remote Start Turn

        ↓

Phase 4
Command / Permissions Approval + Structured User Input

        ↓

Phase 5
CodexControlRouter + PR #117 Fallback

        ↓

Phase 6
安全、审计、恢复与完整 E2E
```

具体文件、模块边界、接口、测试用例、提交粒度和实施顺序由后续 `superpowers:writing-plans` 决定。

---

## 7. 验收标准

本功能达到以下状态时视为完成：

1. Tuvis 可以发现并控制当前官方 Codex Desktop 持有的 Thread；
2. 手机发送消息能够进入原 Desktop Thread 并正常继续执行；
3. 手机能够看到 Thread 实时状态；
4. Command / Permissions Approval 可以远程批准/拒绝并影响原 Turn；
5. Plan 模式下 Structured User Input 可以远程回答并继续原 Turn；
6. Desktop 重启后可以重新发现 owner 并恢复状态；
7. IPC 不可用时，普通消息可以安全降级到 PR #117；
8. 审批 / 结构化输入不会被错误降级成普通消息；
9. 未知投递结果不会造成重复执行；
10. 远程控制不会直接暴露 Codex IPC；
11. 所有关键控制操作具备可追踪审计记录。

验收须分别记录 Windows 与 macOS 的实机证据。macOS Plan 模式阻塞式 Structured User Input、Desktop 重启恢复及 command approval 变体在本次 Probe 中尚未实测；未验前不得宣称对应平台能力完成，Default 模式异步提问不替代阻塞式问答验收。

---

## 8. 设计结论

本项目正式采用：

> **Architecture A：Codex Desktop Owner IPC 作为官方 Desktop Thread 的主远程控制面；PR #117 Headless Injection 作为普通消息的兼容兜底。**

两者通过统一的 `CodexControlRouter` 编排，但保持：

- 独立控制面；
- 独立能力模型；
- 独立失败语义；
- 独立安全边界。

**本 Spec 的功能需求一旦评审通过即冻结；后续 `writing-plans` 不重新讨论上述产品功能和架构决策，只负责将其转化为可执行的工程计划。**
