// @vitest-environment node
// B4 合同层测试（纯逻辑）：模型/思考强度的读取、目录校验、CAS 命令、恢复与错误分类。
// 源码依据见 scripts/zcode-bridge-probe/selectionBridge.ts 头注与 b4-source-notes.md。

import { describe, expect, it } from "vitest";

import {
  assertSwitchable,
  buildSwitchModelConfig,
  classifySelectionFailure,
  planRestore,
  readConfigFromSnapshot,
  resolveCatalogFromSnapshot,
  resolveCatalogFromView,
  type CatalogEntry,
} from "../../scripts/zcode-bridge-probe/selectionBridge";

const SNAPSHOT = {
  config: {
    provider: "bigmodel",
    model: "glm-5.3-flash",
    thought: "medium",
    thoughtLevels: ["low", "medium", "high"],
  },
};

const VIEW = {
  providers: [
    {
      id: "bigmodel",
      models: [
        {
          id: "glm-5.3-flash",
          config: { optionSpecs: { reasoningLevel: { values: ["low", "medium", "high"] } } },
        },
        {
          id: "glm-5.3-pro",
          config: { optionSpecs: { reasoningLevel: { values: ["low", "medium", "high", "max"] } } },
        },
      ],
    },
  ],
};

const CATALOG: CatalogEntry[] = [
  {
    provider: "bigmodel",
    models: ["glm-5.3-flash", "glm-5.3-pro"],
    thoughtLevelsByModel: {
      "glm-5.3-flash": ["low", "medium", "high"],
      "glm-5.3-pro": ["low", "medium", "high", "max"],
    },
  },
];

describe("B4 selection contract", () => {
  it("read_config_from_snapshot_is_fail_closed", () => {
    expect(readConfigFromSnapshot(SNAPSHOT)).toEqual({
      provider: "bigmodel",
      model: "glm-5.3-flash",
      thought: "medium",
      thoughtLevels: ["low", "medium", "high"],
    });
    expect(readConfigFromSnapshot({})).toBeNull();
    expect(readConfigFromSnapshot({ config: { provider: "x" } })).toBeNull();
    expect(resolveCatalogFromSnapshot(SNAPSHOT)).toEqual([
      {
        provider: "bigmodel",
        models: ["glm-5.3-flash"],
        thoughtLevelsByModel: { "glm-5.3-flash": ["low", "medium", "high"] },
      },
    ]);
    expect(resolveCatalogFromView(VIEW)).toEqual(CATALOG);
  });

  it("thought_levels_are_validated_per_model", () => {
    // flash 不支持 max（max 只属于 pro）——provider 级并集校验会误放行，逐模型校验必须拒绝
    expect(() =>
      assertSwitchable(CATALOG, { provider: "bigmodel", model: "glm-5.3-flash", thought: "max" })
    ).toThrow(/unsupported-thought-level/);
    expect(() =>
      assertSwitchable(CATALOG, { provider: "bigmodel", model: "glm-5.3-pro", thought: "max" })
    ).not.toThrow();
  });

  it("empty_level_list_rejects_any_thought", () => {
    const empty: CatalogEntry[] = [
      {
        provider: "bigmodel",
        models: ["glm-5.3-empty"],
        thoughtLevelsByModel: { "glm-5.3-empty": [] },
      },
    ];
    expect(() =>
      assertSwitchable(empty, { provider: "bigmodel", model: "glm-5.3-empty", thought: "low" })
    ).toThrow(/declared no levels/);
  });

  it("restore_rejects_missing_original_thought", () => {
    const original = readConfigFromSnapshot({
      config: { provider: "bigmodel", model: "glm-5.3-flash", thought: "", thoughtLevels: ["low"] },
    })!;
    expect(original.thought).toBeNull();
    expect(() =>
      planRestore({
        original,
        baseRevision: 1,
        commandId: "cmd-empty",
        clientId: "client-1",
        sessionId: "sess-x",
        issuedAt: 1,
      })
    ).toThrow(/restore-unavailable/);
  });

  it("only_server_supported_ids_accepted", () => {
    expect(() =>
      assertSwitchable(CATALOG, { provider: "bigmodel", model: "glm-5.3-flash", thought: "high" })
    ).not.toThrow();
    // 显示名不是 wire 值
    expect(() =>
      assertSwitchable(CATALOG, {
        provider: "bigmodel",
        model: "GLM-5.3-Flash(个人套餐)",
        thought: "high",
      })
    ).toThrow(/unsupported-model/);
    expect(() =>
      assertSwitchable(CATALOG, { provider: "bigmodel", model: "glm-5.3-flash", thought: "ultra" })
    ).toThrow(/unsupported-thought-level/);
    expect(() =>
      assertSwitchable(CATALOG, { provider: "other", model: "glm-5.3-flash", thought: "high" })
    ).toThrow(/unsupported-provider/);
  });

  it("switch_command_requires_base_revision_and_has_exact_payload", () => {
    const command = buildSwitchModelConfig({
      provider: "bigmodel",
      model: "glm-5.3-pro",
      thought: "max",
      baseRevision: 7,
      commandId: "cmd-1",
      clientId: "client-1",
      sessionId: "sess-x",
      issuedAt: 123,
    });
    expect(command.type).toBe("switchModelConfig");
    expect(command.payload).toEqual({ provider: "bigmodel", model: "glm-5.3-pro", thought: "max" });
    expect(command.baseRevision).toBe(7);
    expect(Object.keys(command)).not.toContain("baseLogEpoch");
    expect(() =>
      buildSwitchModelConfig({
        provider: "bigmodel",
        model: "glm-5.3-pro",
        thought: "max",
        baseRevision: -1,
        commandId: "c",
        clientId: "c",
        sessionId: "s",
        issuedAt: 1,
      })
    ).toThrow(/base-revision-required/);
  });

  it("restore_plan_uses_original_values", () => {
    const original = readConfigFromSnapshot(SNAPSHOT)!;
    const restore = planRestore({
      original,
      baseRevision: 9,
      commandId: "cmd-2",
      clientId: "client-1",
      sessionId: "sess-x",
      issuedAt: 456,
    });
    expect(restore.payload).toEqual({
      provider: "bigmodel",
      model: "glm-5.3-flash",
      thought: "medium",
    });
    expect(restore.baseRevision).toBe(9);
  });

  it("error_taxonomy_maps_distinctly", () => {
    expect(classifySelectionFailure("DEVICE_OFFLINE")).toBe("offline");
    expect(classifySelectionFailure("AUTH_FAILED")).toBe("needs_auth");
    expect(classifySelectionFailure({ message: "rejected/proto.sessionNotFound" })).toBe(
      "needs_config"
    );
    expect(classifySelectionFailure({ message: "v4 command session not found: sess-x" })).toBe(
      "needs_config"
    );
    expect(classifySelectionFailure("KICKED")).toBe("conflict");
    expect(classifySelectionFailure("something else")).toBe("unknown");
  });

  it("readonly_queries_survive_control_failure", () => {
    // 合同断言：控制失败只产生分类，不改变读取层状态（读取由本地数据源独立提供）。
    const failure = classifySelectionFailure("DEVICE_OFFLINE");
    expect(failure).toBe("offline");
    expect(readConfigFromSnapshot(SNAPSHOT)?.model).toBe("glm-5.3-flash");
  });
});
