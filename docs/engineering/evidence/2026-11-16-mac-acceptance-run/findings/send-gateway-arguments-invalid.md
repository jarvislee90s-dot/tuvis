# 发送面真机缺陷：宿主网关拒 gateway/arguments-invalid（2026-11-16）

## 链路证明（消息已到达宿主网关）
- 正确会话 id（session-aed8892b…）+ instanceId + actionId 的 send：
  回执 `{"state":"failed","reasonCode":"gateway/arguments-invalid"}`（HTTP 200，账本 Failed+reasonCode）。
- 链路逐环 ✓：设备 cookie 归属 ✓（correct sid+instanceId 不再 403）→ action reserve ✓ → 送达宿主网关 ✓ → **宿主拒参数**。
- 早期误诊修正：①`session_ownership_unproven` 是探针用错 sessionId（项目名 "test" ≠ 会话 id）——快照中
  DSH 会话的 dsh_instance_id 字段完全正确；②`dsh:failed` 与会话活/完结无关（活 turn 下同样拒）。
- 早期 `session_ownership_unproven`（06:00 窗口）是快照降级窗的真实表现（ensure_fresh 空窗），已由
  扫描死线+快照保留旧数据缓解（degraded_snapshot 保留 data）——后续 send 在降级窗内也能走完链路拿到宿主回执。

## 缺陷定性
Tuvis 注入载荷与宿主网关（DeepSeek Harness app.asar 内 dsh-desktop-host lib）当前版本的
inject 参数契约不匹配 → `gateway/arguments-invalid`。宿主应用版本可能已更新（Remote 侧显示
⬆️新版本可用）。待比对：Tuvis 注入载荷构造（dsh_control 注入路径）vs 宿主网关 inject handler
的参数校验（app.asar 内 lib/index.js）。

## 已验证非缺陷面
- 手机面配对/cookie/审计/动作账本（reserve→failed 状态机）全链 ✓。
- 完结会话 vs 活 turn：两者同样拒 → 拒因在载荷不在会话态。

## 根因实锤（asar 实读）：session/prompt 契约漂移
- 已装宿主（DeepSeek Harness app.asar 实读，occurrence @11049994 test-support 印证 + AGENT_METHODS 形态）：
  现行 `session/prompt` 请求形状 = `{sessionId, prompt: [{type:"text",text:...}, ...]}`（ACP 形态；
  无 requestId/mode/content 字段名）。
- Tuvis 发送（actions.rs dispatch_message，契约钉 0.2.1-rc.3 实读）：`{requestId, sessionId, mode:"queue",
  content:[...]}` 四字段。
- 宿主网关 assertExactArguments 精确匹配（多/缺皆拒）→ 多 requestId/mode/content、缺 prompt →
  `gateway/arguments-invalid`。**宿主 app 已升级**（Remote 侧 ⬆️新版本按钮同证），schema 漂移实锤。

## 修复方向（互操作轮，待开工）
1. actions.rs dispatch_message 按宿主版本/契约窗口分发两种形状：旧（0.2.1-rc.3 四字段）/新
   （{sessionId, prompt:[内容块]}，文本+附件 receipt 块迁入 prompt 数组）。
2. contract.rs 重钉新宿主的 session/prompt schema（asar/安装实读），版本窗口机制已有（Window::Method+版本）。
3. requestId 缺位的影响：Tuvis 动作幂等仍在 Tuvis 侧账本（actionId 客户端持有）；宿主侧 rpc-id 由
   传输层自带，无去重语义损失——需在报告中明示。
4. 回归：contract 镜像测试（新形状逐字段）+ dispatch 单测（新旧两形状矩阵）+ 真机复测 M1-M3。

## 修复闭环（2026-11-16 晚间窗口）
- 修复提交链：5e32601b（ACP 形状分发初版）→ 8f77d8cb（文案对齐）→ 396619cc（**request 包装**：
  真机 asar typert.host.js 实读 = session/prompt wire 参数为单一 `request` 对象
  {requestId,sessionId,mode,content}，顶层平铺被 assertExactArguments 拒）+ 测试断言更新。
- **真机终验**：396619cc 包换启后 M1/M2 = accepted（nativeId 回填）、M3 = queued（宿主按序处理）；
  宿主转写 user/message 帧按序各恰 1 次（M1→M2→M3，零重复），ACC-RUN 标记全量落盘。
- 附件/控制台新页/A06 活体观察：活会话已就绪，待下一窗口继续。
