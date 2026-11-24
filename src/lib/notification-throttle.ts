/** 会话通知节流共享口径（F2a，终审发现 C）：桌面 useNotification 与移动端 Board
 *  双端同源——纯 TS 零依赖（Tauri 依赖不可进移动 bundle，故抽独立模块而非放在
 *  hooks/useNotification）。同会话同一 from→to 颜色对 60s 内不重复提醒
 *  （黄↔绿两个边都算；红/waiting 豁免——等待提醒不延迟）。 */

export const SAME_DIRECTION_NOTIFY_MS = 60_000;

/** F2a 方向记账键：from→to 颜色对（桌面/移动端同一口径） */
export function directionKey(from: string, to: string): string {
  return `${from}>${to}`;
}

/** 会话通知记账键：id 只在工具内唯一、跨工具可撞 id（types/session.ts 唯一性
 *  约定），跨表记账必须取 (agentType, sessionId) 二元组 */
/** 会话通知身份键（H04 审查 I4②）：多实例下 DSH 同名/同 ID 会话不串——
 *  dshInstanceId 在场时纳入键；缺席（非 DSH / 未核验）回落 (agentType, sessionId)
 *  既有口径。 */
export function sessionNotifyKey(
  agentType: string,
  sessionId: string,
  dshInstanceId?: string
): string {
  return `${agentType}:${dshInstanceId ?? ""}:${sessionId}`;
}

/** F2a 同方向节流判定：该会话该方向上次实际提醒距今 < 60s → 节流。
 *  红/waiting 双向豁免（等待提醒不延迟，既有语义）；fromColor 未知（首见未读
 *  补发）不节流。dirMap 缺该方向键（首见该方向 / 反方向）= 不节流 */
export function isSameDirectionThrottled(
  dirMap: Map<string, number> | undefined,
  fromColor: string,
  toColor: string,
  nowMs: number = Date.now()
): boolean {
  if (!dirMap) return false;
  if (fromColor === "red" || toColor === "red") return false;
  if (!fromColor) return false;
  const lastAt = dirMap.get(directionKey(fromColor, toColor));
  if (lastAt === undefined) return false;
  return nowMs - lastAt < SAME_DIRECTION_NOTIFY_MS;
}
