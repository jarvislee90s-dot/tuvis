// PetStartupGuard — 主窗口启动校验弹窗（EP2）：素材异常时确认处理，宠物窗口自身永不弹窗
import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import { convertFileSrc, invoke } from "@tauri-apps/api/core";
import { emit } from "@tauri-apps/api/event";
import { Dialog, DialogContent, DialogHeader, DialogTitle } from "@/components/ui/dialog";
import { Button } from "@/components/ui/button";
import { loadActiveId, probeSheetRows, saveActiveId, type PetRows } from "./petRuntime";
import { buildManifestFromScan, repairManifest } from "./petActivation";
import {
  diffManifestVsScan,
  type PetManifestView,
  type PetScan,
  type ValidationIssue,
} from "./petValidation";
import { saveConfig, saveVisible } from "./petConfig";
import { isPetRpcError, petErrMsg, PetError } from "./petErrors";

function toFoxbell(msgKey: string) {
  saveActiveId("foxbell", true, "Foxbell");
  emit("pet-active-changed", {}).catch(() => {});
  toast.success(msgKey);
}

export function PetStartupGuard() {
  const { t } = useTranslation();
  // fatal 存原始异常（PetError/普通 Error/字符串），渲染时经 petErrMsg 按语言翻译（P3-6）
  const [fatal, setFatal] = useState<unknown | null>(null);
  const [issues, setIssues] = useState<ValidationIssue[] | null>(null);
  const [ctx, setCtx] = useState<{ scan: PetScan; manifest: PetManifestView | null } | null>(null);
  // 图集探测到的行数（doUpdate 优先用探测值，探测失败才回退 manifest 记录，FIX-4）
  const [probedRows, setProbedRows] = useState<PetRows | null>(null);

  useEffect(() => {
    let disposed = false;
    (async () => {
      const id = loadActiveId();
      if (id === "foxbell") return;
      try {
        const scan = await invoke<PetScan>("pet_scan", { id });
        if (!scan.spritesheet.exists) {
          if (!disposed) setFatal(new PetError("sheet-missing"));
          return;
        }
        const manifest = await invoke<PetManifestView | null>("pet_read_manifest", { id });
        // 图集尺寸校验：大小与缓存一致时信任记录，否则探测（spec §6.1）
        const sizeChanged =
          !manifest ||
          manifest.spritesheetSizeBytes !== scan.spritesheet.size ||
          manifest.spriteVersionNumber === 0;
        if (sizeChanged) {
          try {
            const rows = await probeSheetRows(convertFileSrc(`${scan.dir}/spritesheet.webp`));
            if (!disposed) setProbedRows(rows);
          } catch (e) {
            if (!disposed) setFatal(e);
            return;
          }
        } else if (manifest) {
          // 大小未变：按 manifest 记录还原行数（校验时信任缓存，spec §4.2）
          if (!disposed) setProbedRows(manifest.spriteVersionNumber === 2 ? 11 : 9);
        }
        if (!manifest) {
          // 直投场景：manifest 缺失是独立 issue kind，不再借用 voice-extra（P3-6）
          if (!disposed) setIssues([{ kind: "manifest-missing", detail: "manifest.json" }]);
          if (!disposed) setCtx({ scan, manifest: null });
          return;
        }
        const list = diffManifestVsScan(manifest, scan);
        if (list.length > 0 && !disposed) {
          setIssues(list);
          setCtx({ scan, manifest });
        }
      } catch (e) {
        // 扫描失败（如宠物目录被整体删除）：宠物窗口自行降级渲染，但主窗口仍需弹窗确认（EP2，FIX-4）。
        // RpcError/普通 Error 原样存（petErrMsg 分流翻译）；字符串/其它形态收敛 scan-fail（第六轮）
        if (!disposed)
          setFatal(isPetRpcError(e) || e instanceof Error ? e : new PetError("scan-fail"));
      }
    })();
    return () => {
      disposed = true;
    };
  }, []);

  const doUpdate = async () => {
    if (!ctx) return;
    try {
      if (!ctx.manifest) {
        // 直投（manifest 缺失）：走与切换一致的生成路径——字幕默认=有语音即有字幕（spec §6-2，FIX-4）
        const rows: PetRows = probedRows ?? 9;
        const built = await buildManifestFromScan(ctx.scan.id, ctx.scan, rows, "folder", true);
        await invoke("pet_update_manifest", { id: ctx.scan.id, manifest: built, backup: false });
        saveActiveId(ctx.scan.id, built.hasVoice, built.displayName);
        emit("pet-active-changed", {}).catch(() => {});
        toast.success(t("pet.startup.updated"));
        setIssues(null);
        return;
      }
      const manifest = ctx.manifest;
      // rows 优先用探测值；未探测过才回退 manifest 记录（FIX-4）
      const rows: PetRows = probedRows ?? (manifest.spriteVersionNumber === 2 ? 11 : 9);
      const repaired = await repairManifest(manifest, ctx.scan, rows);
      await invoke("pet_update_manifest", { id: ctx.scan.id, manifest: repaired, backup: true });
      saveActiveId(ctx.scan.id, repaired.hasVoice, repaired.displayName);
      emit("pet-active-changed", {}).catch(() => {});
      toast.success(t("pet.startup.updated"));
      setIssues(null);
    } catch (e) {
      // 更新失败不静默（issue #33-1）：petErrMsg toast 可见、弹窗保持、issues 不清理，可重试
      toast.error(petErrMsg(e, t));
    }
  };

  const hidePet = () => {
    saveVisible(false);
    // 启动跟随顺带关（2026-10-11 用户裁决）：素材损坏场景下「隐藏」是收走的意思，
    // 不关跟随的话下次启动坏宠还会跟出来把守卫弹窗再弹一遍
    saveConfig({ followStartup: false });
    void invoke("set_pet_visible", { visible: false }).catch(() => {});
    toast.info(t("pet.startup.petHidden"));
    setFatal(null);
  };

  const open = fatal !== null || issues !== null;
  return (
    <Dialog open={open}>
      <DialogContent
        data-testid="pet-startup-dialog"
        className="max-w-md"
        onInteractOutside={(e) => e.preventDefault()}
      >
        <DialogHeader>
          <DialogTitle>{t("pet.startup.title")}</DialogTitle>
        </DialogHeader>
        {fatal !== null ? (
          <div className="space-y-3">
            {/* 前缀拆分（第七轮）：rpc（扫描类）走 fatalScan，PetError（图集类）走原 fatal，
                消除"图集缺失：Pet not found"的自相矛盾 */}
            <p className="text-sm">
              {isPetRpcError(fatal)
                ? t("pet.startup.fatalScan", { msg: petErrMsg(fatal, t) })
                : t("pet.startup.fatal", { msg: petErrMsg(fatal, t) })}
            </p>
            <div className="flex gap-2">
              <Button
                size="sm"
                data-testid="pet-startup-foxbell"
                onClick={() => {
                  toFoxbell(t("pet.startup.switched"));
                  setFatal(null);
                }}
              >
                {t("pet.startup.foxbell")}
              </Button>
              <Button size="sm" variant="outline" onClick={hidePet}>
                {t("pet.startup.hidePet")}
              </Button>
            </div>
          </div>
        ) : (
          <div className="space-y-3">
            <p className="text-sm">{t("pet.startup.issuesTitle")}</p>
            <ul
              className="text-muted-foreground max-h-40 list-disc overflow-auto pl-5 text-xs"
              data-testid="pet-startup-issues"
            >
              {issues?.map((i) => (
                <li key={i.kind + i.detail}>
                  {t(`pet.issue.${i.kind}`)}: {i.detail}
                </li>
              ))}
            </ul>
            <div className="flex flex-wrap gap-2">
              <Button size="sm" data-testid="pet-startup-update" onClick={() => void doUpdate()}>
                {t("pet.startup.update")}
              </Button>
              <Button
                size="sm"
                variant="outline"
                data-testid="pet-startup-foxbell"
                onClick={() => {
                  toFoxbell(t("pet.startup.switched"));
                  setIssues(null);
                }}
              >
                {t("pet.startup.foxbell")}
              </Button>
              <Button size="sm" variant="ghost" onClick={() => setIssues(null)}>
                {t("pet.startup.ignore")}
              </Button>
            </div>
          </div>
        )}
      </DialogContent>
    </Dialog>
  );
}
