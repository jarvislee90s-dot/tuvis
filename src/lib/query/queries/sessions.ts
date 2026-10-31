import { useQuery } from "@tanstack/react-query";
import { getAllSessions } from "@/lib/api/session";
import { SESSIONS_QUERY_KEY } from "@/lib/query/optimisticSessions";
import { useScanMode } from "@/lib/query/queries/scanBus";
export function useSessionsQuery() {
  // 轮询间隔跟随后端扫描档位（Task 9）：后端扫多快前端就拉多快，不再钉死 3s。
  // Task 2：主路径是 `scan-bus-snapshot` 事件直接回写缓存（见 useScanSnapshotSync）；
  // 本轮询保留为**兜底**（事件无重放），间隔跟随档位，不得恢复 3s 高频轮询。
  const interval = useScanMode();
  return useQuery({
    queryKey: SESSIONS_QUERY_KEY,
    queryFn: getAllSessions,
    refetchInterval: interval,
    refetchIntervalInBackground: false,
    staleTime: 1000,
  });
}
