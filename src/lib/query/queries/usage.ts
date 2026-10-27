// 用量域数据层（React Query；计划② Task 5）——把「读账本」与「触发扫描」分成两件事：
//
//  * 读侧三条查询（dashboard / records / settings）**只读本地账本，绝不触发扫描**（§3.15；
//    `tests/pet/usage-queries.test.tsx` 有「采集命令不出现在查询路径」的缺席断言）。
//  * 扫描侧只有 `useUsageCollect`：挂载按需采一次（`force=false`，最小间隔与并发单飞由后端
//    采集命令保证，前端**不做二次节流**），外加一条兜底 ticker。本文件不 import 任何会话查询，
//    采集命令也从不进 3 秒会话轮询路径。
//
// 查询键一律 `["usage", <域>, ...参数]` 形状：写侧一次 `invalidateQueries({ queryKey: USAGE_KEY })`
// 即可覆盖看板 / 记录页 / 设置。设置查询**只此一处**（`useUsageSettingsQuery`），别处（Task 6/9/13/14）
// 一律复用它，否则失效与它失联。
//
// ⚠️ 源码锁约束（改注释前先读这一段）：本文件含 `refetchInterval`（三处读查询恒 `false`），会落进
// ① 的静态锁 `src-tauri/src/commands/usage.rs` 的 `polling_hot_path_*` 用例的扫描面 —— 它按**纯文本**
// 扫描「含 `refetchInterval` / `POLL_INTERVAL` 的前端文件」，断言其中不含四条用量命令的 snake_case
// 字面量（采集 / 看板 / 记录 / 导出 csv，命令名见 `src/lib/api/usage.ts`）。故本文件**连注释都不能**
// 写出这四个字面量（要引用就写中文名或 camelCase 包装名）；`tests/**` 不在扫描面内，测试文件不受限。
import { useCallback, useEffect, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { usageCollect, usageDashboard, usageGetSettings, usageRecords } from "@/lib/api/usage";
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

/**
 * 兜底 ticker 取不到设置时的默认间隔（分钟）。与 Rust `UsageSettings::default()` 的
 * `collect_interval_min: 10` 同值（`src-tauri/src/services/usage/model.rs` 的 Default impl）。
 */
const DEFAULT_COLLECT_INTERVAL_MIN = 10;

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
 * 采集触发：挂载即按需采一次（`force` 恒 `false`），另设一条兜底 ticker。
 *
 * * 前端**不做二次节流**：最小间隔与并发单飞由后端采集命令保证（§3.16）——重复调用
 *   只会拿到上次结果，代价可忽略。挂载采集**不受 `enabled` 约束**（设置可能还没回来）：后端
 *   总开关关闭时在读游标（全局 DB 入口）之前就早退，零扫描零落库（`services/usage/collect.rs`
 *   的 `collect_sources`），故白调一次也不扫。
 * * `active`（默认 true，Task 7 新增）：主体嵌入四页宿主后页面隐藏期间传 false——
 *   兜底 ticker 停摆且**不设 UI 兜底**（隐藏页不采集），重新可见后按原 interval 重建 ticker；
 *   内部 settings 查询同样按 active 门控（评审 Important 2：queryClient 未禁 focus refetch，
 *   隐藏页重聚焦不得发任何 usage 读）；挂载即采的一次性增量**不受 active 约束**
 *   （挂载即「打开了一次看板」，旧调用者行为不变）。
 * * 失败**不抛给 UI**（错误态由查询命令负责，看板三态读的是 `useUsageDashboardQuery`），只 `console.warn`。
 */
export function useUsageCollect(active = true) {
  const queryClient = useQueryClient();
  const [lastResult, setLastResult] = useState<UsageCollectResult | null>(null);
  const { data: settings } = useUsageSettingsQuery({ enabled: active });

  const collect = useCallback(async (): Promise<UsageCollectResult | null> => {
    try {
      const result = await usageCollect(false);
      setLastResult(result);
      // 采集写了账本 → 一次失效整个用量域，下一次渲染即重查（环比不需要第二个查询，§3.17）
      await queryClient.invalidateQueries({ queryKey: USAGE_KEY });
      return result;
    } catch (error) {
      console.warn("[usage] 采集失败（错误态由查询命令负责）：", error);
      return null;
    }
  }, [queryClient]);

  // 挂载即按需采集一次（看板 / 浮窗打开时拉一次增量）。`collect` 引用稳定 → 只在挂载时跑一次。
  useEffect(() => {
    void collect();
  }, [collect]);

  const enabled = settings?.enabled ?? true;
  const intervalMin = settings?.collectIntervalMin ?? DEFAULT_COLLECT_INTERVAL_MIN;

  // 兜底 ticker（**必须保留**）：后端只有启动后一次性采集 + `collectIntervalMin` 最小间隔复用，
  // **没有周期调度**（§3.16）——删掉它，长时间开着的看板就不再出新数据。
  // 只在窗口可见时真正触发（后台 / 被遮挡的窗口不采集）；总开关关闭或间隔非正 → 不设 ticker。
  // `active=false`（用量页隐藏，Task 7）→ 整条 ticker 不设（无 UI 兜底），
  // active 回 true 时本 effect 重跑、按原 interval 重建。
  useEffect(() => {
    if (!active || !enabled || intervalMin <= 0) return;
    const id = setInterval(() => {
      if (document.visibilityState !== "visible") return;
      void collect();
    }, intervalMin * 60_000);
    return () => clearInterval(id);
  }, [active, enabled, intervalMin, collect]);

  return { collect, lastResult };
}
