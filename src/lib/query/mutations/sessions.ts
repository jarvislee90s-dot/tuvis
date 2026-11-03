import { useMutation, useQueryClient } from "@tanstack/react-query";
import { focusSession } from "@/lib/api/session";
import { SESSIONS_QUERY_KEY } from "@/lib/query/optimisticSessions";
export function useFocusSessionMutation() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (pid: number) => focusSession(pid),
    onSuccess: () => qc.invalidateQueries({ queryKey: SESSIONS_QUERY_KEY }),
  });
}
