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
      availabilityKnown: true,
      models: [{ modelId: "same", thoughtLevels: ["low"] }],
    },
    {
      providerId: "b",
      label: "B",
      available: true,
      availabilityKnown: true,
      models: [{ modelId: "same", thoughtLevels: ["high"] }],
    },
    {
      providerId: "off",
      label: "Off",
      available: false,
      availabilityKnown: true,
      models: [{ modelId: "x", thoughtLevels: ["low"] }],
    },
    {
      // 缺 available 字段：可用性未知，UI 仍可选（提交前必须二次确认）。
      providerId: "u",
      label: "Unknown",
      available: false,
      availabilityKnown: false,
      models: [{ modelId: "um", thoughtLevels: ["low"] }],
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
              availabilityKnown: true,
              models: [{ modelId: "m", thoughtLevels: [] }],
            },
          ],
        },
        "b",
        "m"
      )
    ).toBeNull();
  });

  it("unknown availability is selectable but explicit unavailable is not", () => {
    expect(resolveModelChoice(view, "u", "um")).toEqual({
      provider: "u",
      model: "um",
      thought: "low",
    });
    expect(resolveModelChoice(view, "off", "x")).toBeNull();
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
