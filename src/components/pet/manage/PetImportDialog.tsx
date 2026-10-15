// PetImportDialog — 导入向导：来源（codex/本地/petdex）→ 配置确认 → 完成（spec §8）
import { useCallback, useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import { convertFileSrc, invoke } from "@tauri-apps/api/core";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { openUrl } from "@tauri-apps/plugin-opener";
import { Dialog, DialogContent, DialogHeader, DialogTitle } from "@/components/ui/dialog";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Switch } from "@/components/ui/switch";
import { Loader2 } from "lucide-react";
import { RESERVED_PET_IDS, probeSheetRows, type PetRows } from "../petRuntime";
import {
  judgeVoiceTier,
  petNameProblem,
  petNameProblemKey,
  voiceRowProblem,
  type VoiceRow,
} from "../petValidation";
import { VoiceGroupEditor } from "./VoiceGroupEditor";
import { useVoiceDurationProbe } from "./useVoiceDurationProbe";
import { petErrMsg } from "../petErrors";

interface StagedPetDto {
  stagingId: string;
  dir: string;
  suggestedName: string;
  suggestedDisplayName: string;
  spriteVersionNumber: number;
  spritesheetSize: number;
  voiceFiles: { group: string; name: string; file: string; sizeBytes: number }[];
}

interface CodexPetDto {
  id: string;
  displayName: string;
  spriteVersionNumber: number;
  imported: boolean;
}

type Step = "source" | "config" | "done";

export function PetImportDialog(props: { open: boolean; onOpenChange: (v: boolean) => void }) {
  const { t } = useTranslation();
  const [step, setStep] = useState<Step>("source");
  const [tab, setTab] = useState<"codex" | "local" | "petdex">("codex");
  const [codexList, setCodexList] = useState<CodexPetDto[]>([]);
  const [petdexUrl, setPetdexUrl] = useState("");
  const [staged, setStaged] = useState<StagedPetDto | null>(null);
  const [rows, setRows] = useState<PetRows | null>(null);
  const [name, setName] = useState("");
  const [displayName, setDisplayName] = useState("");
  const [description, setDescription] = useState("");
  const [voiceRows, setVoiceRows] = useState<VoiceRow[]>([]);
  const [subtitle, setSubtitle] = useState(true);
  const [busy, setBusy] = useState(false);
  const [importedId, setImportedId] = useState("");
  // 已导入宠物 id（重名实时预检，issue #33-7）；foxbell 为内置保留名
  const [existingIds, setExistingIds] = useState<string[]>([]);
  // 图集探测代数（issue #33-3）：跨次暂存时旧探测后到不得覆盖新结果
  const probeGen = useRef(0);

  useEffect(() => {
    if (!props.open) return;
    setStep("source");
    setStaged(null);
    setRows(null);
    setVoiceRows([]);
    invoke<{ id: string }[]>("pet_list_pets")
      .then((l) => setExistingIds(Array.isArray(l) ? l.map((p) => p.id) : []))
      .catch(() => setExistingIds([]));
    // codex 列表由下方 [open, step, tab] effect 统一拉取
  }, [props.open]);

  // codex 列表按需刷新
  useEffect(() => {
    if (props.open && step === "source" && tab === "codex") {
      invoke<CodexPetDto[]>("pet_list_codex_pets")
        .then((l) => setCodexList(Array.isArray(l) ? l : []))
        .catch(() => setCodexList([]));
    }
  }, [props.open, step, tab]);

  const enterConfig = useCallback((s: StagedPetDto) => {
    const gen = ++probeGen.current;
    setStaged(s);
    setName(s.suggestedName);
    setDisplayName(s.suggestedDisplayName || s.suggestedName);
    setVoiceRows(s.voiceFiles.map((v) => ({ ...v, durationMs: null })));
    setRows(null); // 清旧值：新暂存未探测完成前不得沿用上一次的行数
    setStep("config");
    probeSheetRows(convertFileSrc(`${s.dir}/spritesheet.webp`))
      .then((r) => {
        if (gen === probeGen.current) setRows(r);
      })
      .catch(() => {
        if (gen === probeGen.current) setRows(null);
      });
  }, []);

  // 未探测时长的文件补探测（并行）：失败延迟自动重试（封顶），徽标可点击重测（#2）——共享 hook 封装
  const reprobe = useVoiceDurationProbe(voiceRows, setVoiceRows, staged?.dir ?? null);

  const stageFrom = async (fn: () => Promise<StagedPetDto>) => {
    setBusy(true);
    try {
      enterConfig(await fn());
    } catch (e) {
      toast.error(t("pet.import.errorStage", { msg: petErrMsg(e, t) }));
    } finally {
      setBusy(false);
    }
  };

  const cancelAll = async () => {
    if (staged) await invoke("pet_cancel_import", { stagingId: staged.stagingId }).catch(() => {});
    setStaged(null);
    setStep("source");
    props.onOpenChange(false);
  };

  const execute = async () => {
    if (!staged || !rows) return;
    setBusy(true);
    try {
      const valid = voiceRows.filter((r) => !voiceRowProblem(r));
      const hasVoice = judgeVoiceTier(
        valid.map((r) => ({ rel: r.file, size: r.sizeBytes, durationMs: r.durationMs }))
      ).hasVoice;
      const manifest = {
        schemaVersion: 1,
        id: name,
        displayName,
        description,
        source: tab === "codex" ? "codex" : tab === "petdex" ? "petdex" : "folder",
        spriteVersionNumber: rows === 9 ? 1 : 2,
        spritesheetSizeBytes: staged.spritesheetSize,
        hasVoice,
        hasSubtitle: hasVoice && subtitle,
        voices: valid.map((r) => ({
          group: r.group,
          name: r.name,
          file: r.file,
          sizeBytes: r.sizeBytes,
          durationMs: r.durationMs ?? 0,
        })),
      };
      const sum = await invoke<{ id: string }>("pet_finalize_import", {
        stagingId: staged.stagingId,
        name,
        manifest,
      });
      setImportedId(sum.id);
      setStaged(null);
      setStep("done");
    } catch (e) {
      toast.error(t("pet.import.errorFinalize", { msg: petErrMsg(e, t) }));
    } finally {
      setBusy(false);
    }
  };

  // 名称实时校验（issue #33-7，spec §8.4-1）：字符集/保留设备名/长度/重名（含内置 foxbell）
  const nameProblem = petNameProblem(name, {
    existingIds: [...existingIds, ...RESERVED_PET_IDS],
  });
  const nameOk = nameProblem === null;
  const validCount = voiceRows.filter((r) => !voiceRowProblem(r)).length;

  return (
    <Dialog
      open={props.open}
      onOpenChange={(v) => (v ? props.onOpenChange(true) : void cancelAll())}
    >
      <DialogContent className="max-w-xl">
        <DialogHeader>
          <DialogTitle>
            {step === "source"
              ? t("pet.import.title")
              : step === "config"
                ? t("pet.import.configTitle")
                : t("pet.import.doneTitle")}
          </DialogTitle>
        </DialogHeader>

        {step === "source" && (
          <div className="space-y-3" data-testid="import-source">
            <div className="flex gap-1">
              {(["codex", "local", "petdex"] as const).map((k) => (
                <Button
                  key={k}
                  size="sm"
                  variant={tab === k ? "default" : "outline"}
                  data-testid={`import-tab-${k}`}
                  onClick={() => setTab(k)}
                >
                  {t(
                    `pet.import.tab${k === "codex" ? "Codex" : k === "local" ? "Local" : "Petdex"}`
                  )}
                </Button>
              ))}
            </div>
            {tab === "codex" && (
              <div className="max-h-64 space-y-1 overflow-auto" data-testid="import-codex-list">
                {codexList.length === 0 && (
                  <p className="text-muted-foreground text-sm">{t("pet.import.codexEmpty")}</p>
                )}
                {codexList.map((c) => (
                  <div
                    key={c.id}
                    className="flex items-center justify-between rounded border px-2 py-1 text-sm"
                  >
                    <span>
                      {c.id}
                      {c.displayName && (
                        <span className="text-muted-foreground ml-1">{c.displayName}</span>
                      )}
                      {c.spriteVersionNumber > 0 && (
                        <span className="bg-muted ml-1 rounded px-1 text-[10px]">
                          v{c.spriteVersionNumber}
                        </span>
                      )}
                    </span>
                    {c.imported ? (
                      <span className="text-muted-foreground text-xs">
                        {t("pet.import.codexImported")}
                      </span>
                    ) : (
                      <Button
                        size="sm"
                        variant="outline"
                        disabled={busy}
                        data-testid={`import-stage-${c.id}`}
                        onClick={() =>
                          void stageFrom(() =>
                            invoke<StagedPetDto>("pet_stage_from_codex", { codexId: c.id })
                          )
                        }
                      >
                        {t("pet.import.stage")}
                      </Button>
                    )}
                  </div>
                ))}
              </div>
            )}
            {tab === "local" && (
              <div className="flex gap-2">
                <Button
                  size="sm"
                  variant="outline"
                  disabled={busy}
                  data-testid="import-pick-folder"
                  onClick={() =>
                    void openDialog({ directory: true }).then((p) => {
                      if (p)
                        void stageFrom(() =>
                          invoke<StagedPetDto>("pet_stage_from_folder", { path: p as string })
                        );
                    })
                  }
                >
                  {t("pet.import.pickFolder")}
                </Button>
                <Button
                  size="sm"
                  variant="outline"
                  disabled={busy}
                  onClick={() =>
                    void openDialog({ filters: [{ name: "ZIP", extensions: ["zip"] }] }).then(
                      (p) => {
                        if (p)
                          void stageFrom(() =>
                            invoke<StagedPetDto>("pet_stage_from_zip", { path: p as string })
                          );
                      }
                    )
                  }
                >
                  {t("pet.import.pickZip")}
                </Button>
              </div>
            )}
            {tab === "petdex" && (
              <div className="space-y-2">
                <p className="text-muted-foreground text-xs">{t("pet.import.petdexHint")}</p>
                <div className="flex items-center gap-2">
                  <Input
                    data-testid="import-petdex-url"
                    placeholder="https://petdex.dev/pets/..."
                    value={petdexUrl}
                    onChange={(e) => setPetdexUrl(e.target.value)}
                  />
                  <Button
                    size="sm"
                    variant="outline"
                    data-testid="import-petdex-browse"
                    onClick={() => void openUrl("https://petdex.dev")}
                  >
                    {t("pet.import.petdexBrowse")}
                  </Button>
                </div>
                <Button
                  size="sm"
                  disabled={busy || !petdexUrl}
                  data-testid="import-petdex-download"
                  onClick={() =>
                    void stageFrom(() =>
                      invoke<StagedPetDto>("pet_stage_from_petdex", { url: petdexUrl })
                    )
                  }
                >
                  {busy && <Loader2 className="animate-spin" />}
                  {busy ? t("pet.import.petdexDownloading") : t("pet.import.petdexDownload")}
                </Button>
              </div>
            )}
          </div>
        )}

        {step === "config" && staged && (
          <div className="space-y-3" data-testid="import-config">
            {/* 内容区限高滚动（与修改面板同款）：音频行多时配置页不再无限增高，
                取消/执行按钮固定在滚动区外始终可见 */}
            <div className="max-h-[55vh] space-y-3 overflow-y-auto pr-1">
              <div className="flex gap-4">
                <div className="flex-none">
                  <div
                    className="bg-muted/40 mb-1 h-[104px] w-[96px] rounded"
                    style={{
                      backgroundImage: `url(${convertFileSrc(`${staged.dir}/spritesheet.webp`)})`,
                      backgroundPosition: "0 0",
                      // 半倍显示比例（帧 192×208 → 96×104）：v1 全高 936，v2 1144；未探测时按 v2 兜底
                      backgroundSize: rows === 9 ? "768px 936px" : "768px 1144px",
                    }}
                    data-testid="import-preview"
                    title={t("pet.import.preview")}
                  />
                  <div className="text-center">
                    {rows ? (
                      <span
                        data-testid="import-sheet-badge"
                        className="bg-muted rounded px-1 text-[10px]"
                      >
                        v{rows === 9 ? 1 : 2}
                      </span>
                    ) : (
                      <span
                        data-testid="import-sheet-badge"
                        className="text-destructive text-[10px]"
                      >
                        {t("pet.import.sheetInvalid")}
                      </span>
                    )}
                  </div>
                </div>
                <div className="flex-1 space-y-2">
                  <div>
                    <label className="text-sm" title={t("pet.import.nameHint")}>
                      {t("pet.import.name")}
                    </label>
                    <Input value={name} onChange={(e) => setName(e.target.value)} />
                    {nameProblem && (
                      <p className="text-destructive text-xs" data-testid="import-name-problem">
                        {t(petNameProblemKey(nameProblem))}
                      </p>
                    )}
                  </div>
                  <div>
                    <label className="text-sm">{t("pet.import.displayName")}</label>
                    <Input value={displayName} onChange={(e) => setDisplayName(e.target.value)} />
                  </div>
                  <div>
                    <label className="text-sm">{t("pet.import.description")}</label>
                    <Input value={description} onChange={(e) => setDescription(e.target.value)} />
                  </div>
                </div>
              </div>
              <VoiceGroupEditor
                rows={voiceRows}
                busy={busy}
                onAdd={async (group, paths) => {
                  const added = await invoke<StagedPetDto["voiceFiles"]>("pet_stage_audio", {
                    stagingId: staged.stagingId,
                    srcPaths: paths,
                    group,
                  });
                  setVoiceRows((prev) => [
                    ...prev,
                    ...added.map((a) => ({ ...a, durationMs: null })),
                  ]);
                }}
                onRemove={async (rel) => {
                  await invoke("pet_remove_staged_audio", { stagingId: staged.stagingId, rel });
                  setVoiceRows((prev) => prev.filter((r) => r.file !== rel));
                }}
                onReprobe={reprobe}
              />
              <div className="flex items-center gap-2" title={t("pet.import.subtitle")}>
                <Switch
                  checked={subtitle}
                  disabled={validCount === 0}
                  onCheckedChange={setSubtitle}
                />
                <span className="text-sm">{t("pet.import.subtitle")}</span>
              </div>
            </div>
            <div className="flex justify-end gap-2">
              <Button
                size="sm"
                variant="ghost"
                data-testid="import-cancel"
                onClick={() => void cancelAll()}
              >
                {t("pet.import.cancelImport")}
              </Button>
              <Button
                size="sm"
                disabled={!nameOk || !rows || busy}
                data-testid="import-execute"
                onClick={() => void execute()}
              >
                {t("pet.import.execute")}
              </Button>
            </div>
          </div>
        )}

        {step === "done" && (
          <div className="space-y-3" data-testid="import-done">
            <p className="text-sm">{importedId}</p>
            <div className="flex justify-end gap-2">
              <Button
                size="sm"
                onClick={async () => {
                  const { activatePet } = await import("../petActivation");
                  const r = await activatePet(importedId, async () => "update");
                  if (r.status === "activated")
                    toast.success(t("pet.switch.activated", { name: importedId }));
                  props.onOpenChange(false);
                }}
              >
                {t("pet.import.activateNow")}
              </Button>
              <Button size="sm" variant="outline" onClick={() => props.onOpenChange(false)}>
                {t("pet.import.finish")}
              </Button>
            </div>
          </div>
        )}
      </DialogContent>
    </Dialog>
  );
}
