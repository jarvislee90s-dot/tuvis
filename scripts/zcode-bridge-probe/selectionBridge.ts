// B4 模型/思考强度的**合同层**（纯逻辑，无网络/无 IO）。
//
// 源码依据（固定基线 zai-org/ZCode @ 3c5b227649371507ddc4eb46ac5133afe6db1cab，
// 逐条行号见 .superpowers/sdd/2026-10-24-zcode-official-bridge-validation/b4-source-notes.md）：
//   - READ：`snapshot.config.{provider, model, thought, thoughtLevels}`
//     （packages/shared/src/zcode-protocol-v4/snapshot.ts:506 → session-config.ts:5-28；
//      deltas 同键 patch delta.ts:49；备用 `snapshot.modelTransition` snapshot.ts:508）。
//   - CHANGE：命令 `type:"switchModelConfig"`，payload 恰为 `{provider, model, thought}`
//     （command.ts:254-258）；CAS：**必须**带 `baseRevision`、**禁止** `baseLogEpoch`
//     （command.ts:315-331,333-340；官方调用方 zcodeTaskServiceAdapter.ts:2822-2845）。
//   - 值源：`model-selection.getView()`（channels.ts:133；providerFacadeServices.ts:95-106）
//     → providers[].models[].config.optionSpecs.reasoningLevel.values；会话内等价
//     `snapshot.config.thoughtLevels`（session-facade.ts:374-379）。**显示名不得作 wire 值**。
//   - 错误分类：DEVICE_OFFLINE（web-remote-relay/contract.ts:18）；rejected/proto.sessionNotFound
//     （command-inbox.ts:316-326）；failed/fault.command.executionFailed + "v4 command
//     session not found:"（v4-gateway.ts:2687-2694）；AUTH_FAILED→device-not-owned
//     （webRemoteControlTransport.ts:402-407）；无命令超时码 → 装配超时
//     （remote.rpcFrame.assemblyTimeout）；KICKED 首次可重试、60s 内第二次终态冲突
//     （domain/retry.ts:26-30）；迟到/未知 = 字面 "unknown"（command.ts:488-494）。
//
// 纪律：只接受宿主目录里存在的 id；未知/显示名一律拒绝（零帧）；恢复原值用 fresh baseRevision。

export type SessionConfigView = {
  provider: string;
  model: string;
  thought: string | null;
  thoughtLevels: string[];
};

export type CatalogEntry = {
  provider: string;
  models: string[];
  thoughtLevels: string[];
};

export type SwitchModelConfigCommand = {
  type: "switchModelConfig";
  payload: { provider: string; model: string; thought: string };
  baseRevision: number;
  commandId: string;
  clientId: string;
  sessionId: string;
  issuedAt: number;
};

export type SelectionFailureClass =
  "needs_config" | "needs_auth" | "offline" | "conflict" | "unknown";

const asString = (value: unknown): string | null =>
  typeof value === "string" && value.length > 0 ? value : null;

/** 从会话快照的 `config` 读当前值；形状不符 → null（fail-closed，不猜）。 */
export function readConfigFromSnapshot(snapshot: unknown): SessionConfigView | null {
  if (!snapshot || typeof snapshot !== "object") return null;
  const config = (snapshot as { config?: unknown }).config;
  if (!config || typeof config !== "object") return null;
  const record = config as Record<string, unknown>;
  const provider = asString(record.provider);
  const model = asString(record.model);
  if (!provider || !model) return null;
  const thought = asString(record.thought);
  const levels = Array.isArray(record.thoughtLevels)
    ? record.thoughtLevels.filter((item): item is string => typeof item === "string")
    : [];
  return { provider, model, thought, thoughtLevels: levels };
}

/** 从 model-selection.getView() 应答解析目录（宿主值 only）。 */
export function resolveCatalogFromView(raw: unknown): CatalogEntry[] | null {
  if (!raw || typeof raw !== "object") return null;
  // 官方 channel 应答可能带 {code,msg,data} 或 {result} 包装：逐层解包后找 providers。
  const seen = new Set<unknown>();
  let view: Record<string, unknown> | null = raw as Record<string, unknown>;
  for (let depth = 0; view && depth < 3; depth += 1) {
    if ("providers" in view && Array.isArray(view.providers)) break;
    const next =
      (view.data && typeof view.data === "object" ? view.data : null) ??
      (view.result && typeof view.result === "object" ? view.result : null);
    if (!next || seen.has(next)) {
      view = null;
      break;
    }
    seen.add(next);
    view = next as Record<string, unknown>;
  }
  if (!view) return null;
  const providers = view.providers;
  if (!Array.isArray(providers) || providers.length === 0) return null;
  const out: CatalogEntry[] = [];
  for (const provider of providers) {
    if (!provider || typeof provider !== "object") return null;
    const record = provider as Record<string, unknown>;
    const providerId = asString(record.id) ?? asString(record.provider);
    const models = Array.isArray(record.models) ? record.models : [];
    if (!providerId || models.length === 0) return null;
    const modelIds: string[] = [];
    const thoughtLevels = new Set<string>();
    for (const model of models) {
      if (!model || typeof model !== "object") return null;
      const modelRecord = model as Record<string, unknown>;
      const modelId = asString(modelRecord.id) ?? asString(modelRecord.model);
      if (!modelId) return null;
      modelIds.push(modelId);
      const levels =
        (modelRecord.config as { optionSpecs?: { reasoningLevel?: { values?: unknown } } })
          ?.optionSpecs?.reasoningLevel?.values ?? modelRecord.thoughtLevels;
      if (Array.isArray(levels)) {
        for (const level of levels) {
          const id =
            typeof level === "string"
              ? level
              : level && typeof level === "object"
                ? asString((level as { value?: unknown }).value)
                : null;
          if (id) thoughtLevels.add(id);
        }
      }
    }
    out.push({ provider: providerId, models: modelIds, thoughtLevels: [...thoughtLevels] });
  }
  return out;
}

/** 会话内等价目录：provider/model 来自快照，强度来自 config.thoughtLevels。 */
export function resolveCatalogFromSnapshot(snapshot: unknown): CatalogEntry[] | null {
  const config = readConfigFromSnapshot(snapshot);
  if (!config) return null;
  return [
    { provider: config.provider, models: [config.model], thoughtLevels: config.thoughtLevels },
  ];
}

/** 目标必须在宿主目录里（未知 id / 显示名 → 抛错，零帧）。 */
export function assertSwitchable(
  catalog: CatalogEntry[],
  target: { provider: string; model: string; thought: string }
): CatalogEntry {
  const entry = catalog.find((item) => item.provider === target.provider);
  if (!entry) {
    throw new Error(`unsupported-provider: ${target.provider} is not in the host catalog`);
  }
  if (!entry.models.includes(target.model)) {
    throw new Error(
      `unsupported-model: ${target.model} is not a host model id (display names rejected)`
    );
  }
  if (entry.thoughtLevels.length > 0 && !entry.thoughtLevels.includes(target.thought)) {
    throw new Error(`unsupported-thought-level: ${target.thought} is not a host level id`);
  }
  return entry;
}

/**
 * switchModelConfig 命令构造（唯一构造点）：CAS 只带 baseRevision，
 * payload 恰为 {provider, model, thought}（command.ts:254-258,315-340）。
 */
export function buildSwitchModelConfig(args: {
  provider: string;
  model: string;
  thought: string;
  baseRevision: number;
  commandId: string;
  clientId: string;
  sessionId: string;
  issuedAt: number;
}): SwitchModelConfigCommand {
  if (!Number.isInteger(args.baseRevision) || args.baseRevision < 0) {
    throw new Error("cas-base-revision-required: baseRevision must be a non-negative integer");
  }
  for (const key of [
    "provider",
    "model",
    "thought",
    "commandId",
    "clientId",
    "sessionId",
  ] as const) {
    if (typeof args[key] !== "string" || args[key].length === 0) {
      throw new Error(`invalid-command-field: ${key} must be a non-empty string`);
    }
  }
  return {
    type: "switchModelConfig",
    payload: { provider: args.provider, model: args.model, thought: args.thought },
    baseRevision: args.baseRevision,
    commandId: args.commandId,
    clientId: args.clientId,
    sessionId: args.sessionId,
    issuedAt: args.issuedAt,
  };
}

/** 恢复原值：同一构造器 + 调用方提供的新 baseRevision（不改全局默认）。 */
export function planRestore(args: {
  original: SessionConfigView;
  baseRevision: number;
  commandId: string;
  clientId: string;
  sessionId: string;
  issuedAt: number;
}): SwitchModelConfigCommand {
  if (!args.original.thought) {
    throw new Error("restore-unavailable: original thought level was not observed");
  }
  return buildSwitchModelConfig({
    provider: args.original.provider,
    model: args.original.model,
    thought: args.original.thought,
    baseRevision: args.baseRevision,
    commandId: args.commandId,
    clientId: args.clientId,
    sessionId: args.sessionId,
    issuedAt: args.issuedAt,
  });
}

/** 失败分类（不把一切失败都称「链接过期」；来源行号见文件头）。 */
export function classifySelectionFailure(signal: unknown): SelectionFailureClass {
  const code = typeof signal === "string" ? signal : "";
  const record = signal && typeof signal === "object" ? (signal as Record<string, unknown>) : {};
  const message = typeof record.message === "string" ? record.message : "";
  const kind = `${code} ${message}`;
  if (/DEVICE_OFFLINE|device.offline|offline/i.test(kind)) return "offline";
  if (/AUTH_FAILED|device-not-owned|auth/i.test(kind)) return "needs_auth";
  if (/sessionNotFound|session not found|proto\.session|workspace.*not.*found/i.test(kind)) {
    return "needs_config";
  }
  if (/KICKED|session-conflict|conflict/i.test(kind)) return "conflict";
  return "unknown";
}
