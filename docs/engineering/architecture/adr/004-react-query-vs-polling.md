# ADR 004: React Query 替代手动轮询

**状态**: 已接受 | **日期**: 2026-07-08

## 背景
前端使用手动 setInterval 轮询，存在竞态条件、无缓存、无自动刷新控制等问题。

## 决策
引入 TanStack React Query：useQuery + refetchInterval 实现自动轮询，内置缓存/去重/后台刷新。

## 后果
- 正面：消除竞态条件，缓存命中时瞬时渲染
- 负面：引入新依赖（~12KB gzip）

## 状态更新（2026-08-25）

资源页（ResourceByKindView）已接入 React Query：`useSsotResourcesQuery`（queryKey `ssotResources`）+ `useToggleMcpMutation`，写操作后按 key invalidate。会话轮询仍为自定义 hook，维持原结论"会话场景保留轮询"。
