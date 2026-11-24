// DSH 管理查询数据层（T05）——queryKey 前缀恒 `["dsh"]`，状态键**恰为**
// `["dsh","management"]`、作业键 `["dsh","job",jobId]`；**绝不**借 usage/tps 键
// （域隔离纪律，tps.ts 同款）。
//
// 轮询纪律（tps.ts 可见性同款）：仅文档可见时轮询——管理状态每 3 秒（与
// RemoteSection 轮询同节奏）、活跃作业每 1.2 秒（进度反馈）；隐藏即停。
// 状态变化失效：作业抵达终态后失效管理状态（恰好一拍），管理状态刷新会把
// 终态作业从 operation 摘掉，作业查询随之停轮询（无死循环）。
import { useEffect, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import {
  dshInstanceList,
  dshInstanceStatus,
  dshPluginJob,
  dshRemoteStatus,
  dshSwitchGet,
} from "@/lib/api/dsh";
import type {
  DshInstanceCandidate,
  DshManagementJob,
  DshManagementPhase,
  DshManagementStatus,
  DshSwitchView,
} from "@/types/dsh";

/** DSH 域 queryKey 前缀（独占前缀，不与他域共享）。 */
export const DSH_KEY = ["dsh"] as const;

/** 管理状态键（恰为 ["dsh","management"]；契约键，测试锁）。 */
export const DSH_MANAGEMENT_KEY = [...DSH_KEY, "management"] as const;

/** 候选实例列表键（发现事实；不含已选状态）。 */
export const DSH_INSTANCES_KEY = [...DSH_KEY, "instances"] as const;

/** 按实例的状态键：显式实例查询与「当前选择」分开，避免互相覆盖。 */
export function dshInstanceStatusKey(instanceId: string | null) {
  return [...DSH_KEY, "instance", instanceId ?? "selected"] as const;
}

/** 作业键（jobId 作键：离开页面再回仍能按同一 id 续查真实作业）。 */
export function dshJobKey(jobId: string) {
  return [...DSH_KEY, "job", jobId] as const;
}

/** 单开关状态键（Task 4：按实例查询后端持久状态机；关闭/失败/待卸载都是后端事实）。 */
export function dshSwitchKey(instanceId: string | null) {
  return [...DSH_KEY, "switch", instanceId ?? "none"] as const;
}

/** 可见时管理状态轮询间隔（对齐 RemoteSection 的 3s 节奏）。 */
export const DSH_STATUS_INTERVAL_MS = 3_000;
/** 可见时活跃作业轮询间隔（进度反馈优先于状态节拍）。 */
export const DSH_JOB_INTERVAL_MS = 1_200;

/** 活跃阶段集（未达终态；终态后停轮询并失效管理状态）。 */
const ACTIVE_PHASES: ReadonlySet<DshManagementPhase> = new Set([
  "queued",
  "waiting_for_desktop_exit",
  "applying",
  "verifying",
]);

/** 作业是否活跃（未达终态；组件 busy 闸与轮询判定同一单点）。 */
export function isDshJobActive(phase: DshManagementPhase): boolean {
  return ACTIVE_PHASES.has(phase);
}

/** 文档可见性（jsdom/测试环境缺事件时按可见处理，只受轮询间隔约束）。 */
export function useDocumentVisible(): boolean {
  const [visible, setVisible] = useState(
    () => typeof document === "undefined" || document.visibilityState !== "hidden"
  );
  useEffect(() => {
    if (typeof document === "undefined") return undefined;
    const sync = () => setVisible(document.visibilityState !== "hidden");
    document.addEventListener("visibilitychange", sync);
    return () => document.removeEventListener("visibilitychange", sync);
  }, []);
  return visible;
}

/** 管理状态：可见时每 3 秒 refetch；隐藏停止轮询（数据保留）。
 *  `staleTime: 0`：管理卡状态必须「回页面即重取真值」——共享 queryClient 的
 *  全局 staleTime(1s) 会让快速重挂吃到旧缓存而跳过 refetch（验收⑤要求看到
 *  的是真实作业，不是缓存残影）。 */
export function useDshManagementQuery(visible: boolean) {
  return useQuery<DshManagementStatus>({
    queryKey: DSH_MANAGEMENT_KEY,
    queryFn: dshRemoteStatus,
    // 同上：折叠/隐藏时不得发起首次查询。
    enabled: visible,
    staleTime: 0,
    refetchInterval: visible ? DSH_STATUS_INTERVAL_MS : false,
  });
}

/** 候选实例列表（只报事实；不自动选择）。 */
export function useDshInstancesQuery(visible: boolean) {
  return useQuery<DshInstanceCandidate[]>({
    queryKey: DSH_INSTANCES_KEY,
    queryFn: dshInstanceList,
    // 折叠时必须**完全不查**：候选发现会扫进程与 profile，属重活；
    // 只控制轮询间隔不够——首次查询仍会发出去。
    enabled: visible,
    staleTime: 0,
    refetchInterval: visible ? DSH_STATUS_INTERVAL_MS : false,
  });
}

/** 按实例（或持久选择）的管理状态；实例消失时错误原样暴露，不回落。 */
export function useDshInstanceStatusQuery(instanceId: string | null, visible: boolean) {
  return useQuery<DshManagementStatus>({
    queryKey: dshInstanceStatusKey(instanceId),
    queryFn: () => dshInstanceStatus(instanceId ?? undefined),
    // 未选定实例、或卡片已折叠/页面隐藏时都不发请求：与同文件「折叠时完全不查」
    // 的注释保持一致（否则选中后再折叠，focus/invalidate 仍会发一次）。
    enabled: instanceId !== null && visible,
    staleTime: 0,
    refetchInterval: visible ? DSH_STATUS_INTERVAL_MS : false,
  });
}

/**
 * 单开关状态（Task 4）：可见时每 3 秒轮询后端真实状态机（线性推进、等待退出、
 * 就绪结果都是后端事实）；未选定实例或不可见时不查询。
 */
export function useDshSwitchQuery(instanceId: string | null, visible: boolean) {
  return useQuery<DshSwitchView>({
    queryKey: dshSwitchKey(instanceId),
    queryFn: () => dshSwitchGet(instanceId ?? undefined),
    enabled: instanceId !== null && visible,
    staleTime: 0,
    refetchInterval: visible ? DSH_STATUS_INTERVAL_MS : false,
  });
}

/**
 * 作业查询：jobId 作键（非 null 才启用）；活跃阶段可见时快轮询，终态即停并
 * **失效管理状态**（状态变化失效；effect 依赖 phase，终态期间只发一次）。
 */
export function useDshJobQuery(jobId: string | null, visible: boolean) {
  const queryClient = useQueryClient();
  const query = useQuery<DshManagementJob>({
    queryKey: dshJobKey(jobId ?? "none"),
    queryFn: () => dshPluginJob(jobId as string),
    enabled: jobId !== null,
    staleTime: 0,
    refetchInterval: (q) => {
      const phase = q.state.data?.phase;
      return visible && phase !== undefined && isDshJobActive(phase) ? DSH_JOB_INTERVAL_MS : false;
    },
  });
  const phase = query.data?.phase ?? null;
  useEffect(() => {
    if (phase !== null && !isDshJobActive(phase)) {
      void queryClient.invalidateQueries({ queryKey: DSH_MANAGEMENT_KEY });
    }
  }, [phase, queryClient]);
  return query;
}
