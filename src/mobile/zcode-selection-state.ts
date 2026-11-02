// 供应商 / 模型 / 思考强度的纯选择规则（C2）：只从官方目录选合法组合。
// provider 变更本身零写入；选定模型后才生成一次完整三元组。
import type { SelectionView } from "@/lib/api/zcodeControl";

export interface PendingModelChoice {
  provider: string;
  model: string;
  thought: string;
}

/**
 * 解析目标 provider 下的合法三元组：
 * - provider 必须在目录分组里（旧载荷无 providers 时只接受当前 provider）；
 * - model 必须属于该 provider；缺省取该组第一个模型；
 * - thought 只取该模型自己的档位，旧值不合法时取首档；空档位 → null（不伪造）。
 */
export function resolveModelChoice(
  view: SelectionView,
  provider: string,
  model?: string
): PendingModelChoice | null {
  const groups = view.providers ?? [];
  const group =
    groups.length === 0
      ? provider === view.provider
        ? { providerId: view.provider, label: view.provider, models: view.models, available: true }
        : null
      : groups.find((candidate) => candidate.providerId === provider && candidate.available);
  if (group === null || group === undefined || group.models.length === 0) return null;
  const choice =
    (model === undefined ? undefined : group.models.find((item) => item.modelId === model)) ??
    (provider === view.provider
      ? group.models.find((item) => item.modelId === view.model)
      : undefined) ??
    group.models[0];
  if (choice.thoughtLevels.length === 0) return null;
  const thought =
    provider === view.provider &&
    view.thought !== null &&
    choice.thoughtLevels.includes(view.thought)
      ? view.thought
      : choice.thoughtLevels[0];
  return { provider, model: choice.modelId, thought };
}
