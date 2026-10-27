// TPS 查询数据层（Task 8）——queryKey 前缀恒 `["tps"]`，**绝不**与 Token/费用域
// （`["usage", …]`）共享任何键或区间状态（Spec §7「Token 区间与 TPS 区间分离」）。
//
// 可见性纪律（Spec §2）：前端**仅可见时**轮询——snapshot 每 5 秒、points 每 60 秒；
// 隐藏后两者 refetchInterval=false（停止前端轮询，后台调度不受影响）。
// 查询是纯只读，**不触发采集**（采集节奏由后端调度器与视图租约决定）。
//
// 心跳租约（Spec §2）：每个挂载的 TPS 区块一个稳定 view_id；窗口可见**且**区块在
// 视口内才续租（30 秒一拍）；隐藏/滚出/卸载立即 visible=false 释放。
import { useCallback, useEffect, useId, useRef, useState, type RefObject } from "react";
import { useQuery } from "@tanstack/react-query";
import { tpsSnapshot, tpsPoints, tpsViewHeartbeat } from "@/lib/api/tps";
import type { TpsSelection, TpsSnapshotResult, TpsPointsResult, TpsViewRange } from "@/types/tps";

/** TPS 域 queryKey 前缀（与 Token/费用域隔离的唯一前缀）。 */
export const TPS_KEY = ["tps"] as const;

/** 可见时 snapshot 轮询间隔（Spec §2「每5秒查询snapshot」）。 */
export const TPS_SNAPSHOT_INTERVAL_MS = 5_000;
/** 可见时 points 轮询间隔（Spec §2「每60秒查询points」）。 */
export const TPS_POINTS_INTERVAL_MS = 60_000;
/** 心跳间隔（Spec §2「心跳30秒、租约90秒」）。 */
export const TPS_HEARTBEAT_INTERVAL_MS = 30_000;

/** 选择器入键：显式 null 化，保证同选择器同键（undefined 序列化不稳定）。 */
function selectionKey(selection: TpsSelection) {
  return {
    toolId: selection.toolId ?? null,
    modelId: selection.modelId ?? null,
    channelKey: selection.channelKey ?? null,
    method: selection.method ?? null,
  };
}

/** snapshot 查询键（TpsCard 的 refresh 用 setQueryData 直写同一键）。 */
export function tpsSnapshotKey(selection: TpsSelection) {
  return [...TPS_KEY, "snapshot", selectionKey(selection)] as const;
}

/** snapshot：可见时每 5 秒 refetch；隐藏停止轮询（数据保留）。 */
export function useTpsSnapshotQuery(
  selection: TpsSelection,
  visible: boolean,
  opts?: { enabled?: boolean }
) {
  return useQuery<TpsSnapshotResult>({
    queryKey: tpsSnapshotKey(selection),
    queryFn: () => tpsSnapshot(selection),
    refetchInterval: visible ? TPS_SNAPSHOT_INTERVAL_MS : false,
    enabled: opts?.enabled ?? true,
  });
}

/** points：可见时每 60 秒 refetch；viewRange 进键（与 Token range 完全无关）。 */
export function useTpsPointsQuery(
  selection: TpsSelection,
  viewRange: TpsViewRange,
  visible: boolean,
  opts?: { enabled?: boolean }
) {
  return useQuery<TpsPointsResult>({
    queryKey: [...TPS_KEY, "points", selectionKey(selection), viewRange],
    queryFn: () => tpsPoints(selection, viewRange),
    refetchInterval: visible ? TPS_POINTS_INTERVAL_MS : false,
    enabled: opts?.enabled ?? true,
  });
}

/**
 * 「窗口可见 **且** TPS 区块在视口内」的合成可见性。
 * - 文档侧：`document.visibilitychange` + `visibilityState`；
 * - 区块侧：IntersectionObserver（jsdom/老环境无此 API → 视为在视口内，只受文档侧约束）。
 */
export function useTpsVisible(blockRef: RefObject<HTMLElement | null>): boolean {
  const [docVisible, setDocVisible] = useState(
    () => typeof document === "undefined" || document.visibilityState !== "hidden"
  );
  useEffect(() => {
    const sync = () => setDocVisible(document.visibilityState !== "hidden");
    sync();
    document.addEventListener("visibilitychange", sync);
    return () => document.removeEventListener("visibilitychange", sync);
  }, []);

  const [blockVisible, setBlockVisible] = useState(
    () => typeof IntersectionObserver === "undefined"
  );
  useEffect(() => {
    const el = blockRef.current;
    if (!el || typeof IntersectionObserver === "undefined") return undefined;
    const io = new IntersectionObserver((entries) => {
      setBlockVisible(entries.some((e) => e.isIntersecting));
    });
    io.observe(el);
    return () => io.disconnect();
  }, [blockRef]);

  return docVisible && blockVisible;
}

/**
 * 跨窗口不重复的租约 id 主体：`crypto.randomUUID()` 优先（桌面 Tauri WebView 均支持），
 * 缺失时回退 Math.random + 时间戳拼接。**刻意包一层不透明函数**——id 只要求
 * 「每实例一次、之后稳定」，生成动作不在渲染路径上可重入。
 */
function randomLeaseId(): string {
  try {
    if (typeof crypto !== "undefined" && typeof crypto.randomUUID === "function") {
      return crypto.randomUUID();
    }
  } catch {
    // 某些环境访问 crypto 即抛错：落到回退分支
  }
  return `${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 10)}`;
}

/**
 * 视图租约（Spec §2「多窗口 view_id 租约、心跳30秒、租约90秒」）：
 * - 挂载的每个区块一个稳定 view_id。**主体是每实例一次性生成的随机值**（useState 惰性
 *   初始化，重渲染不变）：`useId` 只在单个 React 树/document 内唯一——两个窗口各自从
 *   `:r0:` 起计数必然同串，而后端按 view_id 整条释放租约，A 窗隐藏/卸载会撤销 B 可见窗
 *   的租约（多窗口租约计数直接失效）。`useId` 仅保留作可读前缀。
 * - `visible` 变真：立即续租 + 30 秒一拍；
 * - `visible` 变假、`enabled` 变假或卸载：立即 `visible=false` 释放（幂等，未租过不发）。
 *   `enabled`（Task 7 起由 TpsCard 传「页签是否当前」）：false = 停心跳并**先释放**——
 *   visibility:hidden 的盒子仍在视口内，IO 判不出页签切换，释放必须挂在 enabled 上。
 * 心跳失败静默（后端租约 90 秒自然过期，不弹错）。
 */
export function useTpsViewLease(blockRef: RefObject<HTMLElement | null>, enabled = true): void {
  const [randomPart] = useState(() => randomLeaseId());
  const viewId = `tps-view-${useId().replace(/[^a-zA-Z0-9_-]/g, "")}-${randomPart}`;
  const visible = useTpsVisible(blockRef);
  const lastSentRef = useRef(false);

  const send = useCallback(
    (value: boolean) => {
      lastSentRef.current = value;
      void tpsViewHeartbeat(viewId, value).catch(() => {});
    },
    [viewId]
  );

  useEffect(() => {
    if (!enabled) {
      // 页签切走（组件仍挂载）：与「变不可见」同权——立即释放已持有的租约再停拍
      if (lastSentRef.current) send(false);
      return undefined;
    }
    if (visible) {
      send(true);
      const iv = setInterval(() => send(true), TPS_HEARTBEAT_INTERVAL_MS);
      return () => clearInterval(iv);
    }
    if (lastSentRef.current) send(false);
    return undefined;
  }, [enabled, visible, send]);

  // 卸载兜底：无论当前可见性如何都再发一次释放——心跳是 fire-and-forget，前端无法确认
  // 上一次续租是否到达后端，释放在后端是幂等操作，多发一次只会把租约收紧不会误续。
  useEffect(
    () => () => {
      lastSentRef.current = false;
      void tpsViewHeartbeat(viewId, false).catch(() => {});
    },
    [viewId]
  );
}
