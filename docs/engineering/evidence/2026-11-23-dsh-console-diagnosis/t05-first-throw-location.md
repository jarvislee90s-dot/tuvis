# T05 · 官方控制台首个抛错点定位

## 结论

**首个抛错点 = 沙箱文档访问 `localStorage` 时抛 `SecurityError`**，与任务给出的最强线索一致，
本轮在真实浏览器中直接复现并固定了语句级证据。

```
SecurityError: Failed to read the 'localStorage' property from 'Window':
The document is sandboxed and lacks the 'allow-same-origin' flag.
```

成因链（与代码一致）：

1. `console_mount::SANDBOX_CSP` 以 `sandbox allow-scripts allow-forms allow-downloads` 送出控制台文档
   （[`src-tauri/src/remote/console_mount.rs:49`](../../../../src-tauri/src/remote/console_mount.rs)），
   **刻意为隔离而不含 `allow-same-origin`**；
2. 该策略让文档获得 **opaque origin（`null`）**；opaque origin 下任何对
   `window.localStorage` 的**属性读取本身**即抛 `SecurityError`（不是返回 null，也不是写入时才抛）；
3. 官方客户端把工作区/侧栏视图状态存在共享 Web Storage（`uiWorkspace` 等）。该模块一加载即触达
   `localStorage`，于是**首个异常**在这里抛出；侧栏与聊天随后因依赖它而连带失败——这解释了
   「页面能到、业务不可用」的现象。

## 真实浏览器实测（Chrome 157.0.8103.80，headless=new）

同一份 HTML 里并列两个 `srcdoc` iframe，唯一差异是 sandbox 属性：

| sandbox 属性 | `window.localStorage.getItem('k')` 结果 |
| --- | --- |
| `allow-scripts`（= 当前控制台策略，无 same-origin） | **`SecurityError: Failed to read the 'localStorage' property from 'Window': The document is sandboxed and lacks the 'allow-same-origin' flag.`** |
| `allow-scripts allow-same-origin` | `NO_THROW (value=null)` |

复现脚本随证据归档：[`sandbox-probe.html`](./sandbox-probe.html)。
命令：

```sh
"/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" \
  --headless=new --disable-gpu --no-sandbox --virtual-time-budget=8000 \
  --dump-dom "file://$PWD/sandbox-probe.html"
```

## 三阶段划分（本轮取证范围）

| 阶段 | 状态 | 证据 |
| --- | --- | --- |
| **凭据签发** | 未在本轮重跑（需启动应用并生成测试配对） | 既有 H09 端点测试覆盖签发/兑换/撤权，本轮未新增 |
| **页面启动** | **首个抛错点已定位** | 上文 `SecurityError` 语句级复现；opaque origin 是直接成因 |
| **业务可操作** | 未达（被启动阶段阻断） | 侧栏/聊天依赖失败的前置模块；未取得业务证据 |

## 明确未取证的部分（不得当作通过）

- **未启动 Tuvis 应用、未做测试配对、未在 Safari 打开真实控制台页**：本轮无 GUI 会话可用，
  且不得触碰用户真实配对/凭据。上表「业务可操作」因此为空。
- **官方包内 `uiWorkspace` 的确切调用栈未在真实页面中抓取**：本轮证明的是**沙箱策略必然导致
  该 `SecurityError`**（真实浏览器），以及官方客户端确实使用共享存储（既有证据），但
  「首个失败插件是哪个、其栈帧是什么」需要在真实控制台页里抓 `console`/`pageerror`。
  该项留给有 GUI 会话的下一轮，或由 T06 的方案验证顺带取得。
- **Safari 未测**：Safari 27.0.1 在场但未启动对照。

## 对 T06 的直接含义

任何「让官方页可用」的方案都必须解决 opaque origin 与 Web Storage 的矛盾。三条候选路径：

1. **上游存储降级**（优先）：若官方客户端有「存储不可用则退化为内存态」的分支，只需启用它，
   零 origin 拓扑改动；
2. **受限页面级存储适配**：仅在已核验版本的已识别启动资源上注入一层内存/受限存储 shim，
   让官方代码在 opaque origin 下不抛错；范围、生命周期、受影响功能须逐项说明；
3. **改 origin 拓扑**（给控制台独立来源）：会破坏「单入口」约束，需用户裁决。

推荐 1 → 2，且都不放宽 `apply_sandbox_headers` 的共享入口隔离。
