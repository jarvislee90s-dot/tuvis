import { useQuery } from "@tanstack/react-query";
import { getSessionsSnapshot } from "@/lib/api/session";
import {
  arbitrateFetchedSessionSnapshot,
  currentScanWindowLabel,
  registerFetchedSessionSnapshot,
  SESSIONS_QUERY_KEY,
} from "@/lib/query/optimisticSessions";
import { useScanMode } from "@/lib/query/queries/scanBus";
import type { SessionsResponse } from "@/types/session";

export function useSessionsQuery() {
  // 轮询间隔跟随后端扫描档位（Task 9）：后端扫多快前端就拉多快，不再钉死 3s。
  // Task 2：主路径是 `scan-bus-snapshot` 事件直接回写缓存（见 useScanSnapshotSync）；
  // 本轮询保留为**兜底**（事件无重放），间隔跟随档位，不得恢复 3s 高频轮询。
  const interval = useScanMode();
  const windowLabel = currentScanWindowLabel();
  return useQuery<SessionsResponse>({
    queryKey: SESSIONS_QUERY_KEY,
    queryFn: async () => {
      const snapshot = await getSessionsSnapshot();
      if (snapshot.degraded) throw new Error("snapshot degraded; refusing stale response");
      return registerFetchedSessionSnapshot(snapshot.data, snapshot.version, snapshot.epoch);
    },
    // 必须在 React Query 最终写入缓存的同步点仲裁；queryFn 返回前的水位检查仍有微任务竞态。
    structuralSharing: (oldData, newData) =>
      arbitrateFetchedSessionSnapshot(
        oldData as SessionsResponse | undefined,
        newData as SessionsResponse,
        windowLabel
      ),
    refetchInterval: interval,
    refetchIntervalInBackground: false,
    staleTime: 1000,
  });
}
