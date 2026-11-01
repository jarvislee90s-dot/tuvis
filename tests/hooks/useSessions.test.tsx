import { renderHook, waitFor } from "@testing-library/react";
import { describe, it, expect } from "vitest";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { ReactNode } from "react";
import { useSessions } from "@/hooks/useSessions";
import { tauriInvokeMock } from "../msw/tauriMocks";

function createWrapper() {
  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false, staleTime: 0 } },
  });
  return ({ children }: { children: ReactNode }) => (
    <QueryClientProvider client={queryClient}>{children}</QueryClientProvider>
  );
}

describe("useSessions", () => {
  it("returns session list from mocked invoke", async () => {
    const { result } = renderHook(() => useSessions(), { wrapper: createWrapper() });

    await waitFor(() => {
      expect(result.current.isSuccess).toBe(true);
    });

    expect(result.current.data?.sessions).toHaveLength(2);
    expect(result.current.data?.sessions[0].agentType).toBe("claude");
    expect(result.current.data?.totalCount).toBe(2);
    expect(tauriInvokeMock).toHaveBeenCalledWith("get_sessions_snapshot");
  });
});
