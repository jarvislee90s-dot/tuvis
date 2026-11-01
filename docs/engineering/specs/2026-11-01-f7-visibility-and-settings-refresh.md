# F7 用量恢复可见采集与设置刷新

## 现状与问题

`UsageDashboardBody` 把内部页签状态传入 `useUsageCollect(active)`。窗口最小化时页签仍为 active，恢复窗口后 `active` 没有变化，因此不会触发可见采集。设置页的 `UsageStatusSection` 同时启动今日数据读取、可见采集和异步 `usage-collected` 监听；如果采集先完成，完成事件会先于监听注册，今日数字可能一直停留在旧账本。

## 设计

### 主用量页的可见采集入口

将触发条件改成「页面 active 且文档可见」。文档 `visibilitychange` 令窗口隐藏时退出可见态，恢复可见后重新进入同一个边沿采集入口。Tauri `Window.onFocusChanged` 与浏览器 `window.focus` 也调用同一个入口；因此窗口已可见但从其它应用切回时同样可以补采。

现有模块级 `lastForcedCollectAt` 继续作为跨 hook 实例的 60 秒去抖；页签激活、可见恢复、窗口重新聚焦全部走这一个入口。同拍 focus 与 visibility 通知合并为一次。窗口隐藏期间 `active=true` 不触发采集。桌宠迷你条仍走相同 hook 和模块级门闩。

React Query 的查询启用条件、手动采集命令和后端 60 秒门闩保持原行为；本组只扩充可见入口信号，不加前端周期采集。

### 设置分区的冷启动刷新

`UsageStatusSection` 的可见采集 Promise 成功返回后，直接调用现有 `loadToday()`。此回读兜住组件首次挂载时「采集快于事件 listener 注册」的窗口。`usage-collected` 监听继续处理监听就绪后的后台/其它 WebView 采集；手动采集仍由现有路径直接 `loadToday()`。分区卸载后不再为已完成的回执更新组件状态。其它设置分区不新增采集入口。

## 错误与生命周期

- 隐藏、焦点事件监听均在 effect cleanup 中移除；Tauri 异步 `onFocusChanged` 返回的 unlisten 若晚于卸载到达，立即调用。
- 非 Tauri 浏览器与测试环境使用 DOM visibility/focus 事件；窗口 API 初始化失败不阻断文档可见入口。
- 可见采集失败继续走原错误记录，不执行「采集成功后刷新」回读；手动刷新按钮仍可重试。
- 模块级 60 秒去抖继续共享，不因新增事件监听拆成多个时间戳。

## 验收

1. `active=true` 时文档 hidden→visible，触发一次可见采集；`active=false` 时不采集。
2. 页面 active 与窗口焦点不变时，Tauri/browser 窗口重新聚焦会调用同一入口；若 visibility 与 focus 同拍，只发一次请求。
3. 模块级 60 秒去抖对页签、恢复可见、重新聚焦与跨实例重挂载继续生效。
4. 设置分区挂载后若可见采集先于 `usage-collected` listener 注册完成，成功 Promise 仍直接触发 `loadToday()`。
5. 设置分区监听到后续后台采集事件仍刷新；其它设置分区不开启新的采集触发。

## 修改范围

- `src/lib/query/queries/usage.ts`：文档可见状态、Tauri/window focus 监听、共用可见入口。
- `src/components/settings/UsageStatusSection.tsx`：可见采集成功回执直接触发今日回读。
- `tests/usage/collectTrigger.test.tsx` 与 `tests/settings/UsageStatusSection.test.tsx`：时序回归。

不修改 `docs/MASTER-PLAN.md`、后端采集、账本、错误码、文案或其他设置分区。
