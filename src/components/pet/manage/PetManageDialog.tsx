// PetManageDialog — 修改宠物：重命名/展示名/音频/字幕/删除/打开文件夹（spec §10；激活中先切回默认宠物，EP5）
import { useCallback, useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import { convertFileSrc, invoke } from "@tauri-apps/api/core";
import { emit } from "@tauri-apps/api/event";
import { Dialog, DialogContent, DialogHeader, DialogTitle } from "@/components/ui/dialog";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Switch } from "@/components/ui/switch";
import {
  DEFAULT_PET,
  RESERVED_PET_IDS,
  clearFlashSwitched,
  loadActiveId,
  loadFlashSwitched,
  probeSheetRows,
  saveActiveId,
  saveFlashSwitched,
  type PetRows,
} from "../petRuntime";
import { petErrMsg } from "../petErrors";
import { buildManifestFromScan, repairManifest } from "../petActivation";
import {
  manifestVoiceCapOnDisk,
  nameFromRel,
  petNameProblem,
  petNameProblemKey,
  type PetManifestView,
  type PetScan,
  type VoiceRow,
} from "../petValidation";
import { VoiceGroupEditor } from "./VoiceGroupEditor";
import { useVoiceDurationProbe } from "./useVoiceDurationProbe";

interface PetSummaryDto {
  id: string;
  displayName: string;
  description: string;
  spriteVersionNumber: number;
  hasVoice: boolean;
  hasSubtitle: boolean;
  manifestExists: boolean;
  dir: string;
}

export function PetManageDialog(props: { open: boolean; onOpenChange: (v: boolean) => void }) {
  const { t } = useTranslation();
  const [pets, setPets] = useState<PetSummaryDto[]>([]);
  const [selected, setSelected] = useState<PetSummaryDto | null>(null);
  const [renameTo, setRenameTo] = useState("");
  const [displayName, setDisplayName] = useState("");
  const [description, setDescription] = useState("");
  const [subtitle, setSubtitle] = useState(false);
  const [voiceRows, setVoiceRows] = useState<VoiceRow[]>([]);
  const [petDir, setPetDir] = useState<string | null>(null);
  const [deleting, setDeleting] = useState(false);
  const [busy, setBusy] = useState(false);
  // 闪切持久标记（localStorage，跨对话框关闭/重开仍生效）：增删音频闪切回默认宠物时记录
  // 原宠物 id；保存成功/重命名成功/对话框关闭三个出口统一消费并清除（回环修复，P1-4 修订）

  const reload = useCallback(async () => {
    try {
      const list = await invoke<PetSummaryDto[]>("pet_list_pets");
      setPets(Array.isArray(list) ? list : []);
    } catch {
      setPets([]);
    }
  }, []);

  useEffect(() => {
    if (props.open) {
      setSelected(null);
      setPetDir(null);
      void reload();
    }
  }, [props.open, reload]);

  const openPanel = async (p: PetSummaryDto) => {
    setSelected(p);
    setRenameTo("");
    setDisplayName(p.displayName);
    setDescription(p.description ?? "");
    setSubtitle(p.hasSubtitle);
    setVoiceRows([]);
    setPetDir(null);
    try {
      const scan = await invoke<PetScan>("pet_scan", { id: p.id });
      setPetDir(scan.dir);
      const m = await invoke<PetManifestView | null>("pet_read_manifest", { id: p.id });
      const rows = (m?.voices ?? []).map((v) => ({
        group: v.group,
        name: v.name,
        file: v.file,
        sizeBytes: v.sizeBytes,
        durationMs: v.durationMs, // manifest 缓存时长（未变条目信任缓存，spec §4.2）
      }));
      // 磁盘上不在 manifest 的合法音频（手动放入）→ 待探测
      const known = new Set(rows.map((r) => r.file));
      const extra = scan.voiceFiles
        .filter((f) => f.rel.startsWith("voice/") && !known.has(f.rel))
        .map((f) => {
          const seg = f.rel.split("/");
          return {
            group: seg[1],
            name: nameFromRel(f.rel),
            file: f.rel,
            sizeBytes: f.size,
            durationMs: null,
          };
        });
      setVoiceRows([...rows, ...extra]);
    } catch {
      /* 面板仍可用，保存时按扫描兜底 */
    }
  };

  // 未探测时长（manifest 无缓存 / 磁盘新文件）并行探测回填：失败延迟自动重试（封顶），
  // 徽标可点击重测（返回的 reprobe 清除该行记账重新探测，#2）
  const reprobe = useVoiceDurationProbe(voiceRows, setVoiceRows, petDir);

  /** 激活中宠物先自动切回默认宠物（EP5），返回是否执行了切换 */
  const ensureNotActive = (): boolean => {
    if (loadActiveId() !== selected?.id) return false;
    saveActiveId(DEFAULT_PET.id, DEFAULT_PET.hasVoice, DEFAULT_PET.displayName);
    emit("pet-active-changed", {}).catch(() => {});
    saveFlashSwitched(selected.id); // 持久标记：随后的保存/重命名/关对话框据此自动切回（回环修复）
    toast.info(t("pet.manage.activeSwitchNotice", { pet: DEFAULT_PET.displayName }));
    return true;
  };

  /** 消费闪切持久标记：把激活指针切回被闪切的原宠物并广播（与"保存完成自动切回"同语义）。
   *  返回是否执行了切回；无标记（未闪切过）时不动指针 */
  const restoreFlashSwitched = (): boolean => {
    const id = loadFlashSwitched();
    if (!id) return false;
    clearFlashSwitched();
    saveActiveId(id, true, undefined); // 能力/展示名未知：仅回指针，不覆盖缓存
    emit("pet-active-changed", {}).catch(() => {});
    return true;
  };

  const doRename = async () => {
    if (!selected || !renameTo || renameTo === selected.id) return;
    // 实时校验兜底（issue #33-7）：非法字符/保留名/超长/重名不发给后端（按钮态由 renameProblem 驱动）
    if (renameProblem) return;
    // 捕获须在 ensureNotActive 翻指针之前（EP5 修订：编辑后自动切回，Bug3）；
    // 持久标记兜底增删音频先行触发过闪切的场景（P1-4 修订，跨对话框重开仍生效）
    const wasActive = loadActiveId() === selected.id || !!loadFlashSwitched();
    setBusy(true);
    try {
      ensureNotActive();
      await invoke("pet_rename_pet", { oldId: selected.id, newId: renameTo });
      if (wasActive) {
        // 重命名切回：manifest 未随重命名重写，且面板可能有未保存的直写增删，
        // 能力按磁盘现状重判（issue #33-12）；扫描失败才退回面板快照值
        let cap = selected.hasVoice;
        try {
          const scan = await invoke<PetScan>("pet_scan", { id: renameTo });
          const m = await invoke<PetManifestView | null>("pet_read_manifest", { id: renameTo });
          cap = m ? manifestVoiceCapOnDisk(m, scan) : false;
        } catch {
          /* 维持面板快照值 */
        }
        // 展示名用输入框现值（未保存也仅是展示层缓存）
        saveActiveId(renameTo, cap, displayName || selected.displayName);
        emit("pet-active-changed", {}).catch(() => {});
      }
      toast.success(t("pet.manage.renamedToast", { name: renameTo }));
      clearFlashSwitched(); // 重命名成功出口：消费并清除闪切标记
      await reload();
      setSelected(null);
      setPetDir(null);
    } catch (e) {
      toast.error(petErrMsg(e, t));
      // 失败出口：指针仍停在默认宠物时按标记恢复原宠物（回环修复）
      if (wasActive) restoreFlashSwitched();
    } finally {
      setBusy(false);
    }
  };

  const doSave = async () => {
    if (!selected) return;
    // 捕获须在 ensureNotActive 翻指针之前（EP5 修订：编辑后自动切回，Bug3）；
    // 持久标记兜底增删音频先行触发过闪切的场景（P1-4 修订，跨对话框重开仍生效）
    const wasActive = loadActiveId() === selected.id || !!loadFlashSwitched();
    setBusy(true);
    try {
      ensureNotActive();
      const scan = await invoke<PetScan>("pet_scan", { id: selected.id });
      let rows: PetRows;
      if (selected.spriteVersionNumber === 2) rows = 11;
      else if (selected.spriteVersionNumber === 1) rows = 9;
      else {
        // 版本未知（直投未激活 = 0）先探测图集再定 rows（issue #33-2）：0 → 恒 9 会把 v2 写成 v1；
        // 探测失败（尺寸非法/挂起超时）按错误退出，不得猜 9
        rows = await probeSheetRows(convertFileSrc(`${scan.dir}/spritesheet.webp`));
      }
      const old = await invoke<PetManifestView | null>("pet_read_manifest", { id: selected.id });
      const base = old
        ? await repairManifest(
            {
              ...old,
              displayName,
              description,
              hasSubtitle: subtitle && old.hasVoice,
            },
            scan,
            rows
          )
        : await buildManifestFromScan(selected.id, scan, rows, "folder", subtitle, {
            displayName,
            description,
          });
      const manifest = { ...base, displayName, hasSubtitle: base.hasVoice && subtitle };
      await invoke("pet_update_manifest", { id: selected.id, manifest, backup: true });
      if (wasActive) {
        // 保存完成自动切回原宠物（经默认宠物 闪切 + 热重载同宠物的全新素材快照）
        saveActiveId(selected.id, manifest.hasVoice, displayName);
        emit("pet-active-changed", {}).catch(() => {});
      }
      toast.success(t("pet.manage.savedToast"));
      clearFlashSwitched(); // 保存成功出口：消费并清除闪切标记
      await reload();
    } catch (e) {
      toast.error(petErrMsg(e, t));
      // 失败出口：指针仍停在默认宠物时按标记恢复原宠物（回环修复）
      if (wasActive) restoreFlashSwitched();
    } finally {
      setBusy(false);
    }
  };

  const doDelete = async () => {
    if (!selected) return;
    setBusy(true);
    try {
      ensureNotActive();
      await invoke("pet_delete_pet", { id: selected.id });
      toast.success(t("pet.manage.deletedToast", { name: selected.displayName }));
      clearFlashSwitched(); // 宠物已删除：闪切标记失去意义，一并清除
      setDeleting(false);
      setSelected(null);
      setPetDir(null);
      await reload();
    } catch (e) {
      toast.error(petErrMsg(e, t));
    } finally {
      setBusy(false);
    }
  };

  // 重命名实时校验（issue #33-7，spec §10-1）：非法字符/保留名/超长/重名即时提示并禁用按钮
  // （重名为前端预检，含内置 foxbell；selfId 豁免保持自身名）
  const renameProblem =
    selected && renameTo
      ? petNameProblem(renameTo, {
          existingIds: [...pets.map((p) => p.id), ...RESERVED_PET_IDS],
          selfId: selected.id,
        })
      : null;

  /** 对话框关闭出口：闪切标记在场 → 自动切回原宠物（不点保存直接关对话框也切回，回环修复） */
  const handleOpenChange = (v: boolean) => {
    if (!v) restoreFlashSwitched();
    props.onOpenChange(v);
  };

  return (
    <Dialog open={props.open} onOpenChange={handleOpenChange}>
      <DialogContent className="max-w-xl">
        <DialogHeader>
          <DialogTitle>{t("pet.manage.title")}</DialogTitle>
        </DialogHeader>
        {!selected ? (
          <div className="max-h-72 space-y-1 overflow-auto" data-testid="manage-list">
            <p className="text-muted-foreground mb-1 text-sm">{t("pet.manage.pick")}</p>
            {pets.map((p) => (
              <button
                key={p.id}
                data-testid={`manage-pick-${p.id}`}
                onClick={() => void openPanel(p)}
                className="hover:bg-accent/50 flex w-full items-center justify-between rounded border px-2 py-1.5 text-left text-sm"
              >
                <span>
                  {p.displayName}
                  <span className="text-muted-foreground ml-1 text-xs">{p.id}</span>
                </span>
                <span className="bg-muted rounded px-1 text-[10px]">
                  v{p.spriteVersionNumber || "?"}
                </span>
              </button>
            ))}
          </div>
        ) : (
          <div className="space-y-3" data-testid="manage-panel">
            {/* 内容区限高滚动（UI 反馈修复）：音频行多时面板不再无限增高，
                保存/删除按钮与删除确认条固定在滚动区外，始终可见可点 */}
            <div className="max-h-[55vh] space-y-3 overflow-y-auto pr-1">
              <div className="flex items-end gap-2">
                <div className="flex-1">
                  <label className="text-sm">{t("pet.manage.rename")}</label>
                  <Input
                    data-testid="manage-rename-input"
                    placeholder={selected.id}
                    value={renameTo}
                    onChange={(e) => setRenameTo(e.target.value)}
                  />
                  {renameProblem && (
                    <p className="text-destructive text-xs" data-testid="manage-rename-problem">
                      {t(petNameProblemKey(renameProblem))}
                    </p>
                  )}
                </div>
                <Button
                  size="sm"
                  disabled={busy || !renameTo || renameProblem !== null}
                  data-testid="manage-rename-btn"
                  onClick={() => void doRename()}
                >
                  {t("pet.manage.renameBtn")}
                </Button>
              </div>
              <div>
                <label className="text-sm">{t("pet.import.displayName")}</label>
                <Input value={displayName} onChange={(e) => setDisplayName(e.target.value)} />
              </div>
              <div>
                <label className="text-sm">{t("pet.import.description")}</label>
                <Input
                  data-testid="manage-desc-input"
                  value={description}
                  onChange={(e) => setDescription(e.target.value)}
                />
              </div>
              <VoiceGroupEditor
                rows={voiceRows}
                busy={busy}
                onAdd={async (group, paths) => {
                  ensureNotActive(); // 直写正式目录前冻结保护
                  const added = await invoke<
                    { group: string; name: string; file: string; sizeBytes: number }[]
                  >("pet_add_voice_files", { id: selected.id, srcPaths: paths, group });
                  setVoiceRows((prev) => [
                    ...prev,
                    ...added.map((a) => ({ ...a, durationMs: null })),
                  ]);
                }}
                onRemove={async (rel) => {
                  ensureNotActive();
                  await invoke("pet_remove_voice_file", { id: selected.id, rel });
                  setVoiceRows((prev) => prev.filter((r) => r.file !== rel));
                }}
                onReprobe={reprobe}
              />
              <div className="flex items-center gap-2">
                <Switch checked={subtitle} onCheckedChange={setSubtitle} />
                <span className="text-sm">{t("pet.manage.subtitle")}</span>
              </div>
            </div>
            <div className="flex flex-wrap justify-end gap-2">
              <Button
                size="sm"
                variant="outline"
                onClick={() =>
                  void invoke("pet_reveal_folder", { id: selected.id }).catch(() => {})
                }
              >
                {t("pet.manage.openFolder")}
              </Button>
              <Button
                size="sm"
                variant="destructive"
                data-testid="manage-delete"
                onClick={() => setDeleting(true)}
              >
                {t("pet.manage.delete")}
              </Button>
              <Button
                size="sm"
                disabled={busy}
                data-testid="manage-save"
                onClick={() => void doSave()}
              >
                {t("pet.manage.save")}
              </Button>
            </div>
            {deleting && (
              <div className="border-destructive/40 bg-destructive/5 flex items-center justify-between gap-2 rounded border p-2">
                <span className="text-xs">{t("pet.manage.deleteConfirm")}</span>
                <div className="flex gap-2">
                  <Button
                    size="sm"
                    variant="destructive"
                    data-testid="manage-delete-confirm"
                    onClick={() => void doDelete()}
                  >
                    {t("pet.manage.delete")}
                  </Button>
                  <Button size="sm" variant="ghost" onClick={() => setDeleting(false)}>
                    {t("pet.import.cancelImport")}
                  </Button>
                </div>
              </div>
            )}
          </div>
        )}
      </DialogContent>
    </Dialog>
  );
}
