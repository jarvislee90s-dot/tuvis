// 设置页「④ 工具特殊配置」的工具卡装配段（A5，设计 §4.1）。
//
// **首版只有 ZCode 一张卡**：不给没有特殊配置的工具渲染空卡，也不预建「工具 × 配置」的
// 通用框架（真有第二个工具要配时再谈列表/键表）。分组标题与 `data-group="tools"` 锚仍由
// RemoteSection 持有（它是六段顺序锁的定位面，A2 就是这么定义的），本段只负责把卡挂上。
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
