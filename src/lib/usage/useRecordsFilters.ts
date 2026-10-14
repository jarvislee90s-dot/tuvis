// 记录页筛选状态与**三层日档守卫**的唯一出处（计划② Task 9 步骤 4；2026-10-06 用户裁决扩展）。
//
// ## 为什么上提到 hook（而不是留在 `UsageRecordsPanel` 里）
// 用户裁决原文：「**CSV 需要接收记录页的筛选条件**……那最后导出的 CSV 应该也是这么一个口径。」
// 导出与屏幕必须是**同一个口径**：若页面与面板各持一份筛选状态，两者必然漂移，而且**漂移在界面上
// 看不出来**（用户以为导出的就是他筛出来的那批）。故「卡片维度 / 工具 chips / 子代理开关」三样
// **只有一个出处** = 本 hook；`UsageRecordsPanel`（渲染与查询）与 `UsageExportActions`（导出）
// 都读它这一份。
//
// ## 三层日档守卫（契约 §3 要点 4「预防优于报错」）——语义原样保留，只换了存放位置
//   ① **判据**：`dayTier = preset !== "last5h" && preset !== "today"`（表达式逐字取自计划，
//      **不得**改写成别的等价形式，否则与后端 `query.rs` 的档位判据各说各话）；
//   ② **复位**：进入日档即把 `parentsOnly` 拨回 `false`——用户从小时档带着「开」切回来时，
//      不该留下一个按不动的开关；
//   ③ **双保险**：`filters` 侧 `...(parentsOnly && !dayTier ? …)`——即使复位那一帧还没落地，
//      日档也**绝不**发出 `parentsOnly`（后端会以第 9 码 `usage-filter-unavailable` 拒收；
//      静默放行会产出「数字含子代理、而页脚文案说不含」的界面谎言）。
//   三层都留着：不靠后端报错兜底。
//
// 纪律：本文件不做任何取数（查询在 `UsageRecordsPanel`）、不发任何 IPC。
import { useEffect, useState } from "react";
import type { UsageFilters, UsageRange, UsageRecordsGroupBy, UsageSourceId } from "@/types/usage";

/**
 * 工具 chips = **D1 的 7 个采集源**。数组顺序即 chips 顺序（同时是选区的确定顺序：
 * `toggleTool` 按本表重排，同一次选择永远得到同一个查询键）。
 * ⚠️ 不是夹具那 4 个工具（`MOCK_USAGE_TOOL_IDS` 只是夹具数据、不是筛选清单），也不是
 * `SUPPORTED_TOOLS`（那是看板筛选清单，与用量 7 源口径不同）。
 */
export const FILTER_TOOL_IDS: readonly UsageSourceId[] = [
  "claude",
  "codex",
  "kimi",
  "opencode",
  "workbuddy",
  "zcode",
  "dsh",
];

/** 卡片分组两值（D7）：`provider` / `model` 只属看板的行维度，记录页不接受、不展示 */
export const RECORD_DIMS: readonly UsageRecordsGroupBy[] = ["tool", "project"];

export interface RecordsFiltersState {
  /** 卡片维度（D7）：只接受 `tool` | `project`，默认「按工具」 */
  cardDim: UsageRecordsGroupBy;
  setCardDim(v: UsageRecordsGroupBy): void;
  /** 工具筛选（D1 七源，多选；空 = 不筛 ⇒ `filters` 里**不出现** `toolIds` 键） */
  toolIds: readonly UsageSourceId[];
  toggleTool(id: UsageSourceId): void;
  /** 子代理模式（`true` = 仅父会话）；日档下恒为 `false`（见三层守卫 ②③） */
  parentsOnly: boolean;
  setParentsOnly(v: boolean): void;
  /** **第一层**的判据（日档 = 非小时档）；面板用它出 `disabled` + 常驻原因 */
  dayTier: boolean;
  /**
   * **第三层**的产物：喂给 `usage_records` / `usage_export_csv` 的那一个对象。
   * 屏幕与导出**必须**用它（不得各自现拼一份）。
   */
  filters: UsageFilters;
}

/**
 * 记录页筛选状态（唯一出处）。`range` 由页面持有（两页签共用同一个 range），本 hook 只读它判断档位。
 */
export function useRecordsFilters(range: UsageRange): RecordsFiltersState {
  const [cardDim, setCardDim] = useState<UsageRecordsGroupBy>("tool");
  const [toolIds, setToolIds] = useState<readonly UsageSourceId[]>([]);
  const [parentsOnly, setParentsOnly] = useState(false);

  // ① 判据：表达式逐字取自计划，**不得**改写成别的等价形式
  const dayTier = range.preset !== "last5h" && range.preset !== "today";

  // ② 复位：进入日档即把开关拨回 `false`
  useEffect(() => {
    if (dayTier) setParentsOnly(false);
  }, [dayTier]);

  // ③ 双保险：日档绝不发出 `parentsOnly`
  const filters: UsageFilters = {
    ...(toolIds.length > 0 ? { toolIds: [...toolIds] } : {}),
    ...(parentsOnly && !dayTier ? { subagentMode: "parentsOnly" } : {}),
  };

  const toggleTool = (id: UsageSourceId) =>
    setToolIds((prev) =>
      prev.includes(id)
        ? prev.filter((x) => x !== id)
        : // 保持 D1 顺序：选区顺序 = chips 顺序，查询键因此确定（同一次选择永远同一个键）
          FILTER_TOOL_IDS.filter((x) => x === id || prev.includes(x))
    );

  return {
    cardDim,
    setCardDim,
    toolIds,
    toggleTool,
    parentsOnly,
    setParentsOnly,
    dayTier,
    filters,
  };
}
