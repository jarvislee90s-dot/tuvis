// 用量配置弹窗（2026-10-20 第三轮修订）：设置页与 D 卡共用同一个弹窗。
//
// 口径（用户 2026-10-20 晚裁决）：把「显示与计价配置」与「高级覆盖」放回**同一个弹窗**；
// 弹窗内仍保留可折叠的高级区（默认收起，展开才挂载四价覆盖管理器）。
//
// 壳纪律：
//  * 宽度与主看板窗口一致：初始 1040px、最小 840px（受当前窗口宽度约束）；只纵向滚动；
//  * 弹窗内容只在打开时挂载 ⇒ observed entries / 覆盖价查询随打开态门控；
//  * 设置页用自带触发按钮；D 卡用受控 `open`（「管理配置」按钮打开）。
import { useState, type ReactNode } from "react";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
  DialogTrigger,
} from "@/components/ui/dialog";
import { UsageConfigPanel } from "@/components/usage/UsageConfigPanel";
import { UsageInfoTip } from "@/components/usage/UsageInfoTip";
import type { TFn } from "@/lib/usage/range";

export interface UsageConfigDialogProps {
  t: TFn;
  /** 生成唯一 testid（设置页 / D 卡两个独立 WebView） */
  instanceId: string;
  /** 受控打开态；不传则自持 */
  open?: boolean;
  onOpenChange?(open: boolean): void;
  /** 设置页的触发按钮；不传则没有内置触发（D 卡由「管理配置」控制） */
  trigger?: ReactNode;
}

export function UsageConfigDialog({
  t,
  instanceId,
  open,
  onOpenChange,
  trigger,
}: UsageConfigDialogProps) {
  const [internalOpen, setInternalOpen] = useState(false);
  const controlled = open !== undefined;
  const dialogOpen = controlled ? open : internalOpen;
  const setDialogOpen = (next: boolean) => {
    if (controlled) onOpenChange?.(next);
    else setInternalOpen(next);
  };

  return (
    <Dialog open={dialogOpen} onOpenChange={setDialogOpen}>
      {trigger ? <DialogTrigger asChild>{trigger}</DialogTrigger> : null}
      <DialogContent
        data-testid={`usage-config-dialog-${instanceId}`}
        className="max-h-[85vh] w-[1040px] max-w-[calc(100vw-2rem)] min-w-[min(840px,calc(100vw-2rem))] overflow-x-hidden overflow-y-auto sm:max-w-[calc(100vw-2rem)]"
        onOpenAutoFocus={(event) => {
          // Radix 默认聚焦第一个可 tab 元素（标题旁的 InfoTip 按钮），聚焦会立刻打开
          // Tooltip，而 Tooltip 的 DismissableLayer 会先消费掉第一次 Esc。
          // 把初始焦点交给弹窗标题（tabIndex=-1），Esc 即可直接关闭弹窗。
          event.preventDefault();
          const content = event.currentTarget as HTMLElement | null;
          content?.querySelector<HTMLElement>('[data-slot="dialog-title"]')?.focus();
        }}
      >
        <DialogHeader>
          <div className="flex items-start gap-1.5 pr-6">
            <div className="min-w-0 flex-1">
              <DialogTitle tabIndex={-1} className="outline-none">
                {t("usage.config.title")}
              </DialogTitle>
              <DialogDescription>{t("usage.config.desc")}</DialogDescription>
            </div>
            <UsageInfoTip
              label={t("usage.config.help.panelLabel")}
              testId={`usage-config-info-${instanceId}`}
            >
              {t("usage.config.help.panel")}
            </UsageInfoTip>
          </div>
        </DialogHeader>

        {/* 打开才挂载：关闭态不拉观察条目 / 覆盖价 */}
        <UsageConfigPanel t={t} instanceId={instanceId} />
      </DialogContent>
    </Dialog>
  );
}
