// PetSwitchDialog — 切换宠物：卡片列表 + 统一校验激活（spec §9）
import { useCallback, useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import { convertFileSrc, invoke } from "@tauri-apps/api/core";
import { Dialog, DialogContent, DialogHeader, DialogTitle } from "@/components/ui/dialog";
import { Button } from "@/components/ui/button";
import { BUILTIN_PETS, builtinPet, loadActiveId } from "../petRuntime";
import { activatePet, type MismatchChoice } from "../petActivation";
import { petErrMsg } from "../petErrors";
import type { ValidationIssue } from "../petValidation";

export interface PetCardInfo {
  id: string;
  displayName: string;
  spriteVersionNumber: number; // 0=未知（直投未激活）
  hasVoice: boolean;
  hasSubtitle: boolean;
  manifestExists: boolean;
  dir?: string; // foxbell 无
}

export function PetSwitchDialog(props: { open: boolean; onOpenChange: (v: boolean) => void }) {
  const { t } = useTranslation();
  const [pets, setPets] = useState<PetCardInfo[]>([]);
  const [activeId, setActiveId] = useState(loadActiveId());
  const [mismatch, setMismatch] = useState<{ id: string; issues: ValidationIssue[] } | null>(null);
  const [busy, setBusy] = useState(false);

  const reload = useCallback(async () => {
    try {
      const list = await invoke<PetCardInfo[]>("pet_list_pets");
      setPets(list);
      setActiveId(loadActiveId());
    } catch {
      setPets([]);
    }
  }, []);

  useEffect(() => {
    if (props.open) void reload();
  }, [props.open, reload]);

  // mismatch 三选的 resolver 存 ref（let 变量会在 re-render 后产生 stale closure）
  const mismatchResolveRef = useRef<(c: MismatchChoice) => void>(() => {});

  const doActivate = async (id: string) => {
    setBusy(true);
    try {
      const r = await activatePet(id, async (issues) => {
        setMismatch({ id, issues });
        // 三选 UI：返回由 mismatch 面板按钮 resolve 的 promise
        return new Promise<MismatchChoice>((resolve) => {
          mismatchResolveRef.current = resolve;
        });
      });
      setMismatch(null);
      if (r.status === "activated") {
        toast.success(
          r.repaired
            ? t("pet.switch.updated")
            : r.ignoredDiff
              ? t("pet.switch.ignoredDiff")
              : t("pet.switch.activated", { name: id })
        );
        setActiveId(loadActiveId());
        if (typeof r.voiceCap === "boolean") {
          // 所见即所得（spec §9，issue #33-8）：activatePet 实际写入指针的能力即时反映到卡片徽标
          // （ignore 降级不重写 manifest，重拉列表反而回滚；以结果为准本地修补）
          setPets((prev) =>
            prev.map((p) =>
              p.id === id
                ? { ...p, hasVoice: r.voiceCap!, hasSubtitle: p.hasSubtitle && r.voiceCap! }
                : p
            )
          );
        }
        if (r.manifestBuilt) void reload(); // 直投首激活后徽标刷新
      } else if (r.status === "invalid-sheet") {
        toast.error(t("pet.switch.invalidSheet"));
      } else if (r.status === "error") {
        toast.error(petErrMsg(r.err ?? undefined, t));
      }
    } finally {
      setBusy(false);
    }
  };

  // 对话框关闭时放弃待决三选（issue #33-4）：按 cancel 结算 activatePet、复位 busy。
  // 走 effect 而非仅 onOpenChange：父组件直接置 open=false（Esc/遮罩/其它入口）同样覆盖；
  // resolve 幂等，与 doActivate 的 setMismatch(null) 双路径无冲突
  useEffect(() => {
    if (!props.open && mismatch) {
      mismatchResolveRef.current("cancel");
      setMismatch(null);
    }
  }, [props.open, mismatch]);

  return (
    <Dialog open={props.open} onOpenChange={props.onOpenChange}>
      <DialogContent className="max-w-lg">
        <DialogHeader>
          <DialogTitle>{t("pet.switch.title")}</DialogTitle>
        </DialogHeader>
        {mismatch ? (
          <div className="space-y-3" data-testid="pet-switch-mismatch">
            <div className="text-sm font-medium">{t("pet.switch.mismatchTitle")}</div>
            <ul className="text-muted-foreground max-h-40 list-disc overflow-auto pl-5 text-xs">
              {mismatch.issues.map((i) => (
                <li key={i.detail}>
                  {t(`pet.issue.${i.kind}`)}: {i.detail}
                </li>
              ))}
            </ul>
            <div className="flex flex-wrap gap-2">
              <Button size="sm" onClick={() => mismatchResolveRef.current("update")}>
                {t("pet.switch.mismatchUpdate")}
              </Button>
              <Button
                size="sm"
                variant="outline"
                onClick={() => mismatchResolveRef.current("ignore")}
              >
                {t("pet.switch.mismatchIgnore")}
              </Button>
              <Button
                size="sm"
                variant="ghost"
                onClick={() => mismatchResolveRef.current("cancel")}
              >
                {t("pet.switch.mismatchCancel")}
              </Button>
            </div>
          </div>
        ) : (
          <div className="grid grid-cols-2 gap-2" data-testid="pet-switch-list">
            {BUILTIN_PETS.map((pet) => (
              <PetCard
                key={pet.id}
                info={{
                  id: pet.id,
                  displayName: pet.displayName,
                  spriteVersionNumber: 2,
                  hasVoice: true,
                  hasSubtitle: true,
                  manifestExists: true,
                }}
                active={activeId === pet.id}
                disabled={busy}
                builtin
                onClick={() => void doActivate(pet.id)}
                t={t}
              />
            ))}
            {pets
              .filter((p) => !builtinPet(p.id))
              .map((p) => (
                <PetCard
                  key={p.id}
                  info={p}
                  active={activeId === p.id}
                  disabled={busy}
                  onClick={() => void doActivate(p.id)}
                  t={t}
                />
              ))}
          </div>
        )}
      </DialogContent>
    </Dialog>
  );
}

function PetCard(props: {
  info: PetCardInfo;
  active: boolean;
  disabled: boolean;
  builtin?: boolean;
  onClick: () => void;
  t: (k: string) => string;
}) {
  const { info, t } = props;
  const thumb = props.builtin
    ? `url(${builtinPet(info.id)?.spritesheetUrl})`
    : info.dir
      ? `url(${convertFileSrc(`${info.dir}/spritesheet.webp`)})`
      : undefined;
  return (
    <button
      data-testid={`pet-card-${info.id}`}
      disabled={props.disabled}
      onClick={props.onClick}
      className={`flex flex-col items-start gap-1 rounded-lg border p-3 text-left transition-colors ${
        props.active ? "border-primary bg-accent" : "border-border hover:bg-accent/50"
      }`}
    >
      <div
        className="mb-1 h-[52px] w-[48px] rounded bg-contain"
        style={
          thumb
            ? {
                backgroundImage: thumb,
                backgroundPosition: "0 0",
                // 1/4 显示比例（帧 192×208 → 48×52）：v1 全高 468，v2 572（issue #33-5）
                backgroundSize: info.spriteVersionNumber === 1 ? "384px 468px" : "384px 572px",
              }
            : undefined
        }
      />
      <div className="flex items-center gap-1 text-sm font-medium">
        {info.displayName}
        {props.builtin && (
          <span className="bg-muted rounded px-1 text-[10px]">{t("pet.switch.builtin")}</span>
        )}
        {info.spriteVersionNumber > 0 ? (
          <span className="bg-muted rounded px-1 text-[10px]">v{info.spriteVersionNumber}</span>
        ) : (
          <span
            className="bg-muted rounded px-1 text-[10px]"
            title={t("pet.switch.pendingFirstCheck")}
          >
            v?
          </span>
        )}
      </div>
      <div className="text-muted-foreground flex gap-1 text-[10px]">
        <span
          title={t("pet.menu.soundNoCap")}
          className={info.hasVoice ? "text-primary" : "opacity-40"}
        >
          🔊
        </span>
        <span
          title={t("pet.menu.subtitleNoCap")}
          className={info.hasSubtitle ? "text-primary" : "opacity-40"}
        >
          💬
        </span>
      </div>
    </button>
  );
}
