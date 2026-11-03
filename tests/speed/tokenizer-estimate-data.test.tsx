import { describe, expect, it, vi } from "vitest";
import { mockSpeedDashboard, setMockTokenizerEstimateEnabled } from "@/lib/usage/mockFixtures";
import { seriesKeyId } from "@/lib/speed/view";
import { speedDashboardKey } from "@/lib/query/queries/speed";
import type { SeriesKey, SpeedDashboardQuery } from "@/types/speed";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));

describe("tokenizer estimate data identity", () => {
  const key = {
    tool: "codex",
    model: "gpt",
    channel: null,
    method: "ipc-body-reference",
    referenceProfileId: "p1",
  } as unknown as SeriesKey;
  const query = {
    range: "1h",
    selection: key,
    metricMode: "tokenizer-estimate",
    toolId: null,
    search: "",
    sort: "tool",
  } as unknown as SpeedDashboardQuery;
  it("does not reuse native series in an empty estimate preview", () => {
    const result = mockSpeedDashboard({ ...query, selection: null });
    expect(result.series).toEqual([]);
    expect(result.buckets).toEqual([]);
  });
  it("mock source control changes only the estimate source", () => {
    setMockTokenizerEstimateEnabled(false);
    const codex = mockSpeedDashboard().sourceStates.find((source) => source.source === "codex")!;
    expect(codex.estimateState.enabled).toBe(false);
    expect(codex.capabilities.response).toBe(true);
    setMockTokenizerEstimateEnabled(true);
  });
  it("keeps reference profiles and modes in separate query identities", () => {
    const other = { ...key, referenceProfileId: "p2" };
    expect(seriesKeyId(key)).not.toBe(seriesKeyId(other));
    expect(speedDashboardKey(query)).not.toEqual(speedDashboardKey({ ...query, selection: other }));
    expect(speedDashboardKey(query)).not.toEqual(
      speedDashboardKey({ ...query, metricMode: "response" })
    );
  });
});
