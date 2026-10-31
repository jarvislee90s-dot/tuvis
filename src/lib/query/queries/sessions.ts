import { useQuery } from "@tanstack/react-query";
import { getAllSessions } from "@/lib/api/session";
import { SESSIONS_QUERY_KEY } from "@/lib/query/optimisticSessions";
import { useScanMode } from "@/lib/query/queries/scanBus";
export function useSessionsQuery() {
  // 轮询间隔跟随后端扫描档位（Task 9）：后端扫多快前端就拉多快，不再钉死 3s——
  // 后端快照是唯一数据源，前端比扫描更快只会重复读同一份快照
  const interval = useScanMode();
  return useQuery({
    queryKey: SESSIONS_QUERY_KEY,
    queryFn: getAllSessions,
    refetchInterval: interval,
    refetchIntervalInBackground: false,
    staleTime: 1000,
  });
}
