# F7 恢复可见采集与设置刷新实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:executing-plans` to implement this plan task-by-task. Steps use checkbox syntax for tracking.

**Goal:** 在主用量页恢复可见/聚焦时触发去抖采集，并保证设置分区的可见采集返回后刷新今日数据。

**Architecture:** `useUsageCollect` 把页签激活、文档 visibility、DOM/Tauri window focus 收敛到现有模块级 60 秒可见采集入口。设置分区的可见采集成功 Promise 直接调用 `loadToday()`；后台事件监听继续刷新后续真实采集。

**Tech Stack:** React 19 Hooks、Tauri 2 `@tauri-apps/api/window`、Vitest、Testing Library。

---

## 任务 1：隐藏页签不采集，恢复可见进入同一入口

**文件：**

- 修改 `src/lib/query/queries/usage.ts`
- 测试 `tests/usage/collectTrigger.test.tsx`

- [ ] **Step 1：写 hidden→visible 失败用例**

设置 `document.visibilityState` 为 `hidden` 后挂载 `useUsageCollect(true)`，断言没有可见采集；将状态改为 `visible` 并 dispatch `visibilitychange`，断言 `usageCollectVisible` 恰好调用一次。保留 `active=true` 不变。

- [ ] **Step 2：写窗口聚焦回归**

mock `getCurrentWindow().onFocusChanged` 并挂载 active hook；在 visibility 保持 visible 时发送 `focused=true`。先把模块时钟推进超过 60 秒，断言同一个可见入口调用一次；同拍将文档从 hidden 切为 visible 并发送 focus/visibility 事件，断言总调用仍为一次。另让 Tauri 监听 Promise 延迟到卸载后才 resolve，断言 late focus 回调不采集且 unlisten 被调用；拒绝 Tauri 监听时 DOM focus 仍可触发。

- [ ] **Step 3：运行测试确认失败**

运行 `pnpm vitest run tests/usage/collectTrigger.test.tsx`。预期旧实现无法在 active 不变时响应 visibility/focus。

- [ ] **Step 4：合并可见条件与事件入口**

在 `usage.ts` 中监听文档 `visibilitychange`，并订阅 Tauri `Window.onFocusChanged` 与 DOM `focus`。只在 `active && document.visibilityState === "visible"` 时进入公共 `requestVisibleCollect()`，并由其检查同一模块级时间戳。清理所有同步/异步监听。

- [ ] **Step 5：重跑用量触发测试**

再次运行 `pnpm vitest run tests/usage/collectTrigger.test.tsx`。预期旧边沿去抖、手动采集、remount 去抖、hidden→visible 与 focus 用例全部通过。

## 任务 2：设置可见采集回执直接刷新今日数据

**文件：**

- 修改 `src/components/settings/UsageStatusSection.tsx`
- 测试 `tests/settings/UsageStatusSection.test.tsx`

- [ ] **Step 1：写 listener 延迟失败用例**

让 `listen("usage-collected")` 返回尚未 resolve 的 Promise，让 `usage_collect_visible` 先 resolve；在没有完成事件回调的情况下断言 `usage_dashboard` 发生第二次读取。最后 resolve listener 并卸载组件。

- [ ] **Step 2：运行测试确认失败**

运行 `pnpm vitest run tests/settings/UsageStatusSection.test.tsx`。预期旧实现只有首次 dashboard 读取。

- [ ] **Step 3：增加 Promise 成功回执兜底**

把可见采集 effect 改为：成功后且组件仍挂载时调用 `loadToday()`；失败继续记日志。保留现有 `usage-collected` 监听和手动采集后 `loadToday()`。

- [ ] **Step 4：重跑设置分区测试**

运行 `pnpm vitest run tests/settings/UsageStatusSection.test.tsx`。预期首读竞态、手动采集回读、后续完成事件刷新均通过。

## 任务 3：最终验证与提交

- [ ] **Step 1：运行定向回归**

运行 `pnpm vitest run tests/usage/collectTrigger.test.tsx tests/settings/UsageStatusSection.test.tsx`。

- [ ] **Step 2：运行前端质量门**

运行 `pnpm check`。预期格式、lint、i18n、错误码对账和构建通过。

- [ ] **Step 3：提交本组**

运行 `git diff --check` 后，仅暂存实现与这两份测试，提交 `fix(usage): collect when the active window returns`。

## 规格覆盖核对

- active 不变的 hidden→visible：任务 1。
- Tauri/browser focus 入口与同拍 visibility 去重：任务 1。
- 模块级 60 秒门闩和非 Tauri cleanup：任务 1。
- 设置监听迟注册时采集成功回读：任务 2。
- 后台事件、手动采集和其它设置分区行为：任务 2、3。
