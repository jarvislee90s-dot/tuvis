// 用量域数据层（React Query；计划② Task 5 / 扫描总线计划 Task 10）——把「读账本」与「触发扫描」
// 分成两件事：
//
//  * 读侧三条查询（dashboard / records / settings）**只读本地账本，绝不触发扫描**（§3.15；
//    `tests/pet/usage-queries.test.tsx` 有「采集命令不出现在查询路径」的缺席断言）。
//  * 扫描侧只有 `useUsageCollect`：
//    ① **可见即采**（active 的 false→true 边沿，含挂载即 active=true）走**可见性命令**
//       （F7：后端 60s 门闩跨所有 WebView 统一去抖 + 单飞；窗口内直接返回缓存、不扫描）。
//       前端仍保留模块级 60s 时间戳作为**第一道**去抖（同一 WebView 内连 IPC 都省掉；宠物
//       迷你条悬停/菜单会频繁挂载卸载，按实例记账等于每次重挂载都发一次请求）。
//    ② **手动**路径 `collect(force)` 仍走原采集命令：最小间隔与并发单飞由后端保证（§3.16）。
//    ③ 后端**任何真实采集完成**（可见 / 手动 / ticker / 启动）都会 emit 同一个 `usage-collected`
//       事件（0 条新记录也发）；事件到达即失效整个用量域，与 newRecords 无关。
//    前端周期 ticker 已删除，兜底采集由后端 ticker（Task 8）负责。
//    本文件不 import 任何会话查询，采集命令也从不进 3 秒会话轮询路径。
//
// 查询键一律 `["usage", <域>, ...参数]` 形状：写侧一次 `invalidateQueries({ queryKey: USAGE_KEY })`
// 即可覆盖看板 / 记录页 / 设置。设置查询**只此一处**（`useUsageSettingsQuery`），别处（Task 6/9/13/14）
// 一律复用它，否则失效与它失联。
//
// ⚠️ 源码锁约束（改注释前先读这一段）：本文件含 `refetchInterval`（三处读查询恒 `false`），会落进
// ① 的静态锁 `src-tauri/src/commands/usage.rs` 的 `polling_hot_path_*` 用例的扫描面 —— 它按**纯文本**
// 扫描「含 `refetchInterval` 的前端文件」，断言其中不含四条用量命令的 snake_case
// 字面量（采集 / 看板 / 记录 / 导出 csv，命令名见 `src/lib/api/usage.ts`）。故本文件**连注释都不能**
// 写出这四个字面量（要引用就写中文名或 camelCase 包装名）；`tests/**` 不在扫描面内，测试文件不受限。
import { useCallback, useEffect, useRef, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { listen } from "@tauri-apps/api/event";
import {
  usageCollect,
  usageCollectVisible,
  usageDashboard,
  usageGetSettings,
  usageRecords,
} from "@/lib/api/usage";
import type {
  UsageCollectResult,
  UsageDashboard,
  UsageFilters,
  UsageGroupBy,
  UsageRange,
  UsageRecords,
  UsageRecordsGroupBy,
  UsageSettings,
} from "@/types/usage";

/** 用量域查询键前缀：采集成功 / 设置保存后一律失效整个前缀（`["usage", …]` 全被覆盖） */
export const USAGE_KEY = ["usage"] as const;

// 读命令只读账本、不扫描（§3.15），账本又只在采集后才变 → 30s 短 staleTime 防抖即可，
// 不做轮询（`refetchInterval: false`；新数据的来源是采集成功后的那次失效）。惯例照 bindings.ts / health.ts。
// `opts.enabled`（默认 true）＝ Task 7 的 active 门控：主体嵌入四页宿主后页面隐藏期间不发请求。
export function useUsageDashboardQuery(
  range: UsageRange,
  groupBy: UsageGroupBy,
  opts?: { enabled?: boolean }
) {
  return useQuery<UsageDashboard>({
    queryKey: ["usage", "dashboard", range, groupBy],
    queryFn: () => usageDashboard(range, groupBy),
    staleTime: 30_000,
    refetchInterval: false,
    enabled: opts?.enabled ?? true,
  });
}

/** 记录页查询。`groupBy` 是**卡片**维度（tool | project）；卡内行恒为「供应商 / 模型」（D6/D7） */
export function useUsageRecordsQuery(
  range: UsageRange,
  groupBy: UsageRecordsGroupBy,
  filters: UsageFilters,
  opts?: { enabled?: boolean }
) {
  return useQuery<UsageRecords>({
    queryKey: ["usage", "records", range, groupBy, filters],
    queryFn: () => usageRecords(range, groupBy, filters),
    staleTime: 30_000,
    refetchInterval: false,
    enabled: opts?.enabled ?? true,
  });
}

/** 用量设置的**唯一**查询入口（Task 6/9/13/14 一律复用；键在 `USAGE_KEY` 前缀下） */
export function useUsageSettingsQuery(opts?: { enabled?: boolean }) {
  return useQuery<UsageSettings>({
    queryKey: ["usage", "settings"],
    queryFn: usageGetSettings,
    staleTime: 30_000,
    refetchInterval: false,
    enabled: opts?.enabled ?? true,
  });
}

/**
 * 可见即采的强制去抖时间戳：**模块作用域**，同一 WebView 内所有 `useUsageCollect` 实例共享。
 *
 * 定义在这里（而不是 hook 内的 ref）是因为宠物迷你条 `UsageMiniBar` 会随悬停/托盘菜单频繁
 * 挂载卸载；按实例记账时每次重挂载都是一枚全新时间戳，等于绕过后端最小间隔反复强制全量扫描
 * （最坏每 ~2.5s 一次）。F7 要求的是**前端 60s 去抖**，跨实例共享才成立。
 *
 * `null` = 本模块尚未强制采集过（区别于时间戳 0：假定时器下 Date.now() 可为 0，首次激活仍须必发）
 */
let lastForcedCollectAt: number | null = null;

/**
 * 采集触发：**可见即采**（active 的 false→true 边沿，含挂载即 active=true）+ 后端兜底 ticker。
 *
 * * 可见边沿走**可见性命令**：后端 60s 门闩跨所有 WebView 去抖、同时到达的调用合并为一次
 *   重型采集；窗口内直接返回缓存（不扫描）。前端模块级时间戳只是**第一道**去抖（同一 WebView
 *   内省掉 IPC），首次激活必发。
 * * 手动路径 `collect(force = false)` 保持原语义：前端**不做二次节流**，最小间隔与并发单飞由
 *   后端采集命令保证（§3.16）——重复调用只会拿到上次结果，代价可忽略。后端总开关关闭时在读
 *   游标（全局 DB 入口）之前就早退，零扫描零落库（`services/usage/collect.rs` 的
 *   `collect_sources`），故白调一次也不扫。
 * * 前端周期 ticker **已删除**：兜底采集移到后端 ticker（Task 8，不依赖任何页面）；
 *   它 emit 的 `usage-collected` 到达时失效整个用量域，让看板 / 记录页 / 设置重查
 *   （与 `newRecords` 无关：0 条新记录的成功采集也会发）。
 * * 失败**不抛给 UI**（错误态由查询命令负责，看板三态读的是 `useUsageDashboardQuery`），只 `console.warn`。
 */
export function useUsageCollect(active = true) {
  const queryClient = useQueryClient();
  const [lastResult, setLastResult] = useState<UsageCollectResult | null>(null);
  const prevActive = useRef(false);

  const runCollect = useCallback(
    async (run: () => Promise<UsageCollectResult>): Promise<UsageCollectResult | null> => {
      try {
        const result = await run();
        setLastResult(result);
        // 采集写了账本 → 一次失效整个用量域，下一次渲染即重查（环比不需要第二个查询，§3.17）
        await queryClient.invalidateQueries({ queryKey: USAGE_KEY });
        return result;
      } catch (error) {
        console.warn("[usage] 采集失败（错误态由查询命令负责）：", error);
        return null;
      }
    },
    [queryClient]
  );

  const collect = useCallback(
    (force = false) => runCollect(() => usageCollect(force)),
    [runCollect]
  );

  /** 可见性触发（F7）：失败不抛给 UI；60s 去抖由前端模块时间戳 + 后端门闩双层保证 */
  const collectVisible = useCallback(() => runCollect(usageCollectVisible), [runCollect]);

  // 可见边沿（含挂载即 active=true；prevActive 初值 false → 挂载算一次边沿）。
  // 去抖按**模块**记账（跨实例共享）：60s 内重复可见（快速切页 / 宠物浮窗反复重挂载）不重复
  // 发请求；首次激活恒发。真正「会不会重扫」由后端跨 WebView 的 60s 门闩裁定。
  useEffect(() => {
    const becameActive = active && !prevActive.current;
    prevActive.current = active;
    if (!becameActive) return;

    const now = Date.now();
    if (lastForcedCollectAt === null || now - lastForcedCollectAt >= 60_000) {
      lastForcedCollectAt = now;
      void collectVisible();
    }
  }, [active, collectVisible]);

  // 后端**真实采集完成**（可见 / 手动 / ticker / 启动）→ 失效用量域。
  // 前后端两步必须同批合并，否则会出现「没有任何周期采集」的中间态；前端不再自设 interval。
  // `.catch(() => () => {})`：非 Tauri 环境（浏览器 / Playwright）里 `listen` 是 rejected
  // promise，吞掉并退回空解除函数，避免 unhandled rejection（同 `scanBus.ts`）。
  useEffect(() => {
    const un = listen("usage-collected", () => {
      void queryClient.invalidateQueries({ queryKey: USAGE_KEY });
    }).catch(() => () => {});
    return () => {
      void un.then((unlisten) => unlisten());
    };
  }, [queryClient]);

  return { collect, collectVisible, lastResult };
}
