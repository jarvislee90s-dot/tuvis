// 设置页「③ 工具特殊配置」的工具卡装配段（A5，设计 §4.1）。
//
// **本段只挂 ZCode 卡**：Codex APP 卡由 RemoteSection 用同一 `ToolConfigCard` 装配。
// 不给没有特殊配置的工具渲染空卡，也不预建「工具 × 配置」通用框架。分组标题与
// `data-group="tools"` 锚仍由 RemoteSection 持有（四段顺序锁的定位面），本段只负责把卡挂上。
import { ZcodeRemoteSettingsCard } from "@/components/settings/ZcodeRemoteSettingsCard";

interface ToolRemoteSettingsSectionProps {
  /** 透传给 ZCode 卡的连接态轮询间隔（毫秒）；省略 = 卡片默认 1.5s。 */
  zcodeStatusPollMs?: number;
}

export function ToolRemoteSettingsSection({
  zcodeStatusPollMs,
}: ToolRemoteSettingsSectionProps = {}) {
  return <ZcodeRemoteSettingsCard statusPollMs={zcodeStatusPollMs} />;
}
