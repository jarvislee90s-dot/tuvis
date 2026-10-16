// 应用确认弹窗 v2（M2 Phase B / spec §7.3 + 2026-09-15 白板裁决）：
// open 时 dry-run 拉 preview_apply_preset，渲染五段清单（空段隐藏）；
// 白板（无可启用项但会停用/暂存）→ 顶部强警示 + 确认按钮 destructive，仍允许继续；
// 确认动作经 onConfirm 委托调用方（T6 开关接线：applyPreset + toast），本组件只做预览与意图收集。
import { useEffect, useState, type ReactNode } from "react";
import { useTranslation } from "react-i18next";
import {
  Archive,
  CheckCircle,
  Filter,
  Loader2,
  ShieldCheck,
  TriangleAlert,
  XCircle,
} from "lucide-react";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { formatInvokeError } from "@/lib/invokeError";
import { previewApplyPreset } from "@/lib/api/preset";
import type { ApplyPreview } from "@/types/preset";

interface ApplyConfirmDialogProps {
  open: boolean;
  presetId: string;
  toolId: string;
  toolName: string;
  onClose: () => void;
  /** 确认回调（进行中驱动按钮 loading 态）；支持 Promise，T6 传 async 函数 */
  onConfirm: () => void | Promise<void>;
}

export function ApplyConfirmDialog({
  open,
  presetId,
  toolId,
  toolName,
  onClose,
  onConfirm,
}: ApplyConfirmDialogProps) {
  const { t } = useTranslation();
  const [preview, setPreview] = useState<ApplyPreview | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [confirming, setConfirming] = useState(false);

  // 白板裁决：不启用任何资源、但会停用/暂存现有资源 → 强警示 + destructive 确认（不阻断）
  const isWhiteboard =
    preview !== null &&
    preview.toEnable.length === 0 &&
    (preview.toDisable.length > 0 || preview.toStash.length > 0);

  // open 触发拉取（沿用旧版确认弹窗的 loading 模式），另加 stale 守卫与状态复位
  useEffect(() => {
    if (!open) return;
    let stale = false;
    setPreview(null);
    setError(null);
    setConfirming(false);
    setLoading(true);
    (async () => {
      try {
        const data = await previewApplyPreset(presetId, toolId);
        if (!stale) setPreview(data);
      } catch (e) {
        console.error("Failed to load apply preview:", e);
        if (!stale) setError(formatInvokeError(e, t));
      } finally {
        if (!stale) setLoading(false);
      }
    })();
    return () => {
      stale = true;
    };
  }, [open, presetId, toolId, t]);

  const handleConfirm = async () => {
    try {
      setConfirming(true);
      await onConfirm();
    } finally {
      setConfirming(false);
    }
  };

  return (
    <Dialog open={open} onOpenChange={(v) => !v && onClose()}>
      <DialogContent aria-describedby={undefined} className="max-w-md">
        <DialogHeader>
          <DialogTitle className="text-sm">{t("presets.applyTo", { tool: toolName })}</DialogTitle>
        </DialogHeader>

        {loading ? (
          <div className="text-muted-foreground py-4 text-center text-xs">
            {t("common.loading")}
          </div>
        ) : error ? (
          // 拉取失败：展示错误（本地化透出），仅保留取消入口
          <p className="text-destructive py-2 text-xs">{formatInvokeError(error, t)}</p>
        ) : preview ? (
          <div className="space-y-3">
            {/* 白板强警示：顶部 destructive 块 */}
            {isWhiteboard && (
              <div className="text-destructive border-destructive/30 bg-destructive/10 flex items-start gap-1.5 rounded border p-2 text-xs">
                <TriangleAlert className="mt-0.5 h-3.5 w-3.5 shrink-0" />
                <span>{t("presets.whiteboardWarning")}</span>
              </div>
            )}

            {/* 将启用（沿用旧弹窗 compatibleCount 表头——本任务禁新增 i18n key） */}
            {preview.toEnable.length > 0 && (
              <PreviewSection
                icon={<CheckCircle className="h-3 w-3" />}
                title={t("presets.compatibleCount", { n: preview.toEnable.length })}
                titleClass="text-green-600"
                items={preview.toEnable}
              />
            )}

            {/* 被过滤（后端每条已是 "id: 原因"，原样渲染并弱化） */}
            {preview.filtered.length > 0 && (
              <PreviewSection
                icon={<Filter className="h-3 w-3" />}
                title={t("presets.filteredCount", { n: preview.filtered.length })}
                titleClass="text-orange-600"
                items={preview.filtered}
                muted
              />
            )}

            {/* 将停用 */}
            {preview.toDisable.length > 0 && (
              <PreviewSection
                icon={<XCircle className="h-3 w-3" />}
                title={t("presets.toDisableCount", { n: preview.toDisable.length })}
                titleClass="text-orange-600"
                items={preview.toDisable}
              />
            )}

            {/* 将暂存（被清扫的原生技能点名） */}
            {preview.toStash.length > 0 && (
              <PreviewSection
                icon={<Archive className="h-3 w-3" />}
                title={t("presets.toStashCount", { n: preview.toStash.length })}
                titleClass="text-amber-500"
                items={preview.toStash}
              />
            )}

            {/* 常驻豁免：仅计数一行 */}
            {preview.residentExempt.length > 0 && (
              <div className="text-muted-foreground flex items-center gap-1 text-xs">
                <ShieldCheck className="h-3 w-3" />
                {t("presets.residentExemptCount", { n: preview.residentExempt.length })}
              </div>
            )}
          </div>
        ) : null}

        <DialogFooter>
          {/* 取消恒可用（loading / confirming 中也可关） */}
          <Button size="sm" variant="outline" onClick={onClose}>
            {t("common.cancel")}
          </Button>
          {/* 预览就绪才出确认键；白板时 destructive */}
          {preview && !error && (
            <Button
              size="sm"
              variant={isWhiteboard ? "destructive" : "default"}
              onClick={handleConfirm}
              disabled={confirming}
            >
              {confirming && <Loader2 className="h-3 w-3 animate-spin" />}
              {t("presets.confirmApply", { n: preview.toEnable.length })}
            </Button>
          )}
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

/** 预览分节：图标 + 计数表头 + 点名列表 */
function PreviewSection({
  icon,
  title,
  titleClass,
  items,
  muted = false,
}: {
  icon: ReactNode;
  title: string;
  titleClass: string;
  items: string[];
  /** 条目弱化色（被过滤的原因串） */
  muted?: boolean;
}) {
  return (
    <div>
      <h4 className={`flex items-center gap-1 text-xs font-medium ${titleClass}`}>
        {icon}
        {title}
      </h4>
      <div className="mt-1 space-y-0.5">
        {items.map((item, i) => (
          <div key={`${i}-${item}`} className={`text-xs ${muted ? "text-muted-foreground" : ""}`}>
            {item}
          </div>
        ))}
      </div>
    </div>
  );
}
