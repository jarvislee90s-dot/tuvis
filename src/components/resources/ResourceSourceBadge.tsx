// 来源徽章（资源管理页 UI 改版 C9）：按工具视图每行右侧 / 预设预览每项佩戴。
// 三态互斥、优先级 原生 > 全局仓库 > .agents 可用（优先级裁决在调用方做行合并，本组件只渲染）。
// 样式与旧「✓ 全局仓库」文字 tag 同族：小圆角 + 低饱和底色；agents 态附说明 title。
import { useTranslation } from "react-i18next";

export type ResourceSource = "native" | "repo" | "agents";

const BADGE_CLS: Record<ResourceSource, string> = {
  native: "bg-orange-500/15 text-orange-600",
  repo: "bg-emerald-500/10 text-emerald-700",
  agents: "bg-blue-500/10 text-blue-600",
};

const LABEL_KEY: Record<ResourceSource, string> = {
  native: "resources.sourceNative",
  repo: "resources.sourceRepo",
  agents: "resources.sourceAgents",
};

export function ResourceSourceBadge({ source }: { source: ResourceSource }) {
  const { t } = useTranslation();
  return (
    <span
      className={`shrink-0 rounded px-1.5 py-0.5 text-[10px] leading-none ${BADGE_CLS[source]}`}
      title={source === "agents" ? t("resources.sourceAgentsHint") : undefined}
    >
      {t(LABEL_KEY[source])}
    </span>
  );
}
