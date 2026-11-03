// 出图模块选择（2026-10-27 实机反馈轮 #3a）纯函数层：分享图「画哪些段」的选择 + localStorage 记忆。
//
// 纪律：
//  * **零 React / 零 Tauri 依赖**（对齐 `lib/toolFilter` 的先例）：偏好记在**本机浏览器
//    localStorage**，不进 Rust 设置（`usage_get_settings` / `usage_set_settings` 不接这两个字段，
//    评语/姿态那条 Rust 通路保持原样）——分享图是本机导出的产物，跨设备同步没有需求。
//  * 总览（hero + 指标行）+ 趋势 mini **必选不可关**：它们是「这张图是什么」的底座，不在类型里
//    （`ExportModules` 只收可关的段）；布局层也按「恒画」处理。
//  * 坏值 / 缺值一律回落默认；`distribution` 只接受四个合法 `UsageGroupBy`，其余值静默丢弃
//    （localStorage 脏数据不能把导出卡死，同 `loadToolFilter` 的防御）。
import type { UsageGroupBy } from "@/types/usage";

/** 分享图可开关的模块；总览 + 趋势必选，**不在**这个类型里（永远绘制） */
export interface ExportModules {
  /** 分组分布：四种分组多选；`[]` = 不出分组段 */
  distribution: UsageGroupBy[];
  /** API 等效费用段（复用费用卡数据：总额 + Top 行） */
  cost: boolean;
  /** 工作小结（工具格）+ 最后桌宠发言（立绘 + 气泡）——一个开关管两段 */
  work: boolean;
}

/** 与「今日出图」完全一致：一段按工具的分组 + 工具格 + 立绘，无费用段 */
export const DEFAULT_EXPORT_MODULES: ExportModules = {
  distribution: ["tool"],
  cost: false,
  work: true,
};

const STORAGE_KEY = "tuvis.usage.exportModules";

/** 合法的分组维度（`UsageGroupBy` 恰好四个；别处不得另立清单） */
const GROUP_BY_VALUES: readonly UsageGroupBy[] = ["tool", "project", "provider", "model"];

/** 深拷贝默认值（数组字段共享引用会被调用方改脏） */
function defaultModules(): ExportModules {
  return { distribution: [...DEFAULT_EXPORT_MODULES.distribution], cost: false, work: true };
}

/**
 * 持久化读：缺值 / 坏 JSON / 字段类型不对 → 整体或单字段回落默认；
 * `distribution` 逐项过滤到四个合法值并去重（保序：按用户勾选顺序出段）。
 */
export function loadExportModules(): ExportModules {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (!raw) return defaultModules();
    const parsed: unknown = JSON.parse(raw);
    if (typeof parsed !== "object" || parsed === null) return defaultModules();
    const o = parsed as Record<string, unknown>;
    const dist = Array.isArray(o.distribution)
      ? [
          ...new Set(
            o.distribution.filter(
              (g): g is UsageGroupBy =>
                typeof g === "string" && (GROUP_BY_VALUES as readonly string[]).includes(g)
            )
          ),
        ]
      : [...DEFAULT_EXPORT_MODULES.distribution];
    return {
      distribution: dist,
      cost: typeof o.cost === "boolean" ? o.cost : DEFAULT_EXPORT_MODULES.cost,
      work: typeof o.work === "boolean" ? o.work : DEFAULT_EXPORT_MODULES.work,
    };
  } catch {
    /* localStorage 不可用（隐私模式等）或 JSON 坏值：回落默认 */
    return defaultModules();
  }
}

/** 持久化写：失败静默（记忆是体验增强，不阻塞导出本身，同 `saveToolFilter`） */
export function saveExportModules(m: ExportModules): void {
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify(m));
  } catch {
    /* 忽略 */
  }
}
