import { describe, expect, it } from "vitest";
import { resolveModelChoice } from "@/mobile/zcode-selection-state";
import type { SelectionView } from "@/lib/api/zcodeControl";

const view: SelectionView = {
  provider: "a",
  model: "same",
  thought: "low",
  models: [{ modelId: "same", thoughtLevels: ["low"] }],
  providers: [
    {
      providerId: "a",
      label: "A",
      available: true,
      models: [{ modelId: "same", thoughtLevels: ["low"] }],
    },
    {
      providerId: "b",
      label: "B",
      available: true,
      models: [{ modelId: "same", thoughtLevels: ["high"] }],
    },
    {
      providerId: "off",
      label: "Off",
      available: false,
      models: [{ modelId: "x", thoughtLevels: ["low"] }],
    },
  ],
};

describe("zcode selection state", () => {
  it("same model under two providers resolves to its own levels", () => {
    expect(resolveModelChoice(view, "a", "same")).toEqual({
      provider: "a",
      model: "same",
      thought: "low",
    });
    expect(resolveModelChoice(view, "b", "same")).toEqual({
      provider: "b",
      model: "same",
      thought: "high",
    });
  });

  it("unavailable provider and empty levels are not faked", () => {
    expect(resolveModelChoice(view, "off", "x")).toBeNull();
    expect(
      resolveModelChoice(
        {
          ...view,
          providers: [
            {
              providerId: "b",
              label: "B",
              available: true,
              models: [{ modelId: "m", thoughtLevels: [] }],
            },
          ],
        },
        "b",
        "m"
      )
    ).toBeNull();
  });

  it("legacy view without providers keeps current provider scope", () => {
    const legacy: SelectionView = {
      provider: "a",
      model: "same",
      thought: null,
      models: view.models,
    };
    expect(resolveModelChoice(legacy, "a", "same")).toEqual({
      provider: "a",
      model: "same",
      thought: "low",
    });
    expect(resolveModelChoice(legacy, "b", "same")).toBeNull();
  });
});
