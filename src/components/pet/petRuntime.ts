// 激活宠物运行时 — 指针持久化、内置宠物注册表、外部宠物解析、媒体探测与音频内存快照（spec §7/§12）
import { convertFileSrc, invoke } from "@tauri-apps/api/core";
import { parseManifest, type VoiceEntry } from "./petVoices";
import { PetError } from "./petErrors";

export const ACTIVE_KEY = "mam-pet-active";
export const ACTIVE_NAME_KEY = "mam-pet-active-name";
export const VOICE_CAP_KEY = "mam-pet-voice-cap";
/** 闪切持久标记：管理对话框因编辑激活中宠物而闪切回默认宠物时记录原宠物 id。
 *  存 localStorage 而非组件 state——跨对话框关闭/重开仍生效（P1-4 修订，回环修复） */
export const FLASH_SWITCHED_KEY = "mam-pet-flash-switched";
/** 同窗口激活变更通知：storage 事件只发给其它窗口，本窗口写入方（切换/管理对话框）
 *  经此 DOM 事件让同窗口订阅者（设置页"当前宠物"标签等）即时刷新（P1-6） */
export const ACTIVE_LOCAL_EVENT = "mam-pet-active-local-changed";

export type PetRows = 9 | 11;

export interface ActivePet {
  id: string; // 内置宠物或外部宠物 ID
  displayName: string;
  spritesheetUrl: string;
  rows: PetRows;
  hasVoice: boolean;
  hasSubtitle: boolean;
  voices: VoiceEntry[];
  resolveVoiceUrl: (file: string) => string;
  /** 释放运行时资源（外部宠物 = 逐个 revoke 音频 blob URL）；foxbell 无快照故无 dispose */
  dispose?: () => void;
}

/**
 * 内置宠物注册表：每个内置宠物一个同构静态目录 public/pet/<dirName>/
 * （manifest.json 语音清单 + spritesheet.webp 图集 + voice/<group>/*.m4a）。
 * 新增内置宠物 = 建同构目录 + 在此追加一行；数组顺序即切换对话框中的内置区顺序。
 */
interface BuiltinPetDef {
  /** 静态资源目录名（public/pet/ 下），同时决定宠物 id：builtin-<dirName> */
  dirName: string;
  displayName: string;
  rows: PetRows;
}

const BUILTIN_PET_DEFS: BuiltinPetDef[] = [
  { dirName: "tuvis-3d", displayName: "tuvis-3D", rows: 11 },
  { dirName: "foxbell", displayName: "Foxbell", rows: 11 },
];

/** 默认宠物（内部参数）：无激活记录 / 解析失败回落 / 管理对话框闪切都回到它，换默认只改这里 */
const DEFAULT_BUILTIN_DIR_NAME = "tuvis-3d";

/** 旧版本激活指针兼容：0.5.x 仅内置 foxbell 且 id 无 builtin- 前缀（读取时归一，写入恒为新 id） */
const LEGACY_BUILTIN_IDS: Record<string, string> = { foxbell: "builtin-foxbell" };

function toActivePet(def: BuiltinPetDef): ActivePet {
  const dir = `/pet/${def.dirName}`;
  return {
    id: `builtin-${def.dirName}`,
    displayName: def.displayName,
    spritesheetUrl: `${dir}/spritesheet.webp`,
    rows: def.rows,
    hasVoice: true,
    hasSubtitle: true,
    voices: [],
    resolveVoiceUrl: (f) => `${dir}/voice/${encodeURI(f)}`,
  };
}

export const BUILTIN_PETS: ActivePet[] = BUILTIN_PET_DEFS.map(toActivePet);
export const DEFAULT_PET: ActivePet =
  BUILTIN_PETS.find((pet) => pet.id === `builtin-${DEFAULT_BUILTIN_DIR_NAME}`) ?? BUILTIN_PETS[0]!;

export function builtinPet(id: string): ActivePet | undefined {
  return BUILTIN_PETS.find((pet) => pet.id === id);
}

/** 内置保留名全集（含旧 id）：外部宠物命名/重命名禁用，与 src-tauri is_builtin_pet 保持对齐 */
export const RESERVED_PET_IDS: string[] = [
  ...BUILTIN_PETS.map((pet) => pet.id),
  ...Object.keys(LEGACY_BUILTIN_IDS),
];

export function loadActiveId(): string {
  try {
    const raw = localStorage.getItem(ACTIVE_KEY);
    if (!raw) return DEFAULT_PET.id;
    return LEGACY_BUILTIN_IDS[raw] ?? raw;
  } catch {
    return DEFAULT_PET.id;
  }
}

export function loadActiveName(): string {
  try {
    return (
      localStorage.getItem(ACTIVE_NAME_KEY) ||
      builtinPet(loadActiveId())?.displayName ||
      loadActiveId()
    );
  } catch {
    return DEFAULT_PET.displayName;
  }
}

/** 激活指针 + 语音能力缓存 + 展示名缓存（petSoundTakeover 同步读取用，spec §5.2） */
export function saveActiveId(id: string, voiceCap: boolean, displayName?: string): void {
  localStorage.setItem(ACTIVE_KEY, id);
  localStorage.setItem(VOICE_CAP_KEY, voiceCap ? "1" : "0");
  if (displayName) localStorage.setItem(ACTIVE_NAME_KEY, displayName);
  window.dispatchEvent(new Event(ACTIVE_LOCAL_EVENT));
}

/** 仅回写语音能力缓存（不动激活指针/展示名）：运行时实际能力与激活时缓存背离时，
 *  让 petSoundTakeover 同步看到真实值（issue #33-10，spec §5.2 回落契约）。
 *  双向同步（评审 N1）：丢失方向降 0，文件恢复方向也回升 1；已一致时免写入与事件派发 */
export function persistVoiceCap(cap: boolean): void {
  if (loadVoiceCap() === cap) return;
  try {
    localStorage.setItem(VOICE_CAP_KEY, cap ? "1" : "0");
  } catch {
    /* ignore */
  }
  window.dispatchEvent(new Event(ACTIVE_LOCAL_EVENT));
}

/** 读取闪切持久标记：被闪切的原宠物 id；无标记返回 null（含 localStorage 不可用） */
export function loadFlashSwitched(): string | null {
  try {
    return localStorage.getItem(FLASH_SWITCHED_KEY);
  } catch {
    return null;
  }
}

/** 写入闪切持久标记（记录被闪切的原宠物 id） */
export function saveFlashSwitched(id: string): void {
  try {
    localStorage.setItem(FLASH_SWITCHED_KEY, id);
  } catch {
    /* ignore */
  }
}

/** 清除闪切持久标记（保存成功/重命名成功/对话框关闭三个出口统一消费） */
export function clearFlashSwitched(): void {
  try {
    localStorage.removeItem(FLASH_SWITCHED_KEY);
  } catch {
    /* ignore */
  }
}

/** 语音能力：未写入时视为 true（foxbell / 旧版本升级兼容） */
export function loadVoiceCap(): boolean {
  try {
    return localStorage.getItem(VOICE_CAP_KEY) !== "0";
  } catch {
    return true;
  }
}

/** 图集尺寸 → 行数（1536×1872→9，1536×2288→11，其余非法） */
export function rowsFromSize(w: number, h: number): PetRows | null {
  if (w !== 1536) return null;
  if (h === 1872) return 9;
  if (h === 2288) return 11;
  return null;
}

/** 图集行数探测（Image 解码，约 50-150ms；8s 超时与音频探测对齐，spec §13——
 *  图集挂起时 activatePet 不得永久 busy，issue #33-9） */
export function probeSheetRows(url: string, timeoutMs = 8000): Promise<PetRows> {
  return new Promise((resolve, reject) => {
    const img = new Image();
    const timer = window.setTimeout(() => {
      img.src = "";
      reject(new PetError("sheet-timeout"));
    }, timeoutMs);
    img.onload = () => {
      window.clearTimeout(timer);
      const r = rowsFromSize(img.naturalWidth, img.naturalHeight);
      if (r) resolve(r);
      else reject(new PetError("sheet-bad-size", { w: img.naturalWidth, h: img.naturalHeight }));
    };
    img.onerror = () => {
      window.clearTimeout(timer);
      reject(new PetError("sheet-load-fail"));
    };
    img.src = url;
  });
}

/** 音频时长探测（仅读元数据头部，超时 8s，spec §6-2） */
export function probeAudioDurationMs(url: string, timeoutMs = 8000): Promise<number> {
  return new Promise((resolve, reject) => {
    const a = new Audio();
    a.preload = "metadata";
    const timer = window.setTimeout(() => {
      a.src = "";
      reject(new PetError("audio-timeout"));
    }, timeoutMs);
    a.onloadedmetadata = () => {
      window.clearTimeout(timer);
      const d = a.duration;
      a.src = "";
      if (Number.isFinite(d) && d > 0) resolve(Math.round(d * 1000));
      else reject(new PetError("audio-bad-duration"));
    };
    a.onerror = () => {
      window.clearTimeout(timer);
      reject(new PetError("audio-load-fail"));
    };
    a.src = url;
  });
}

interface ScanFile {
  rel: string;
  exists: boolean;
  size: number;
}
interface PetScanDto {
  id: string;
  dir: string;
  spritesheet: ScanFile;
  voiceFiles: ScanFile[];
}
interface ManifestVoiceDto {
  group: string;
  name: string;
  file: string;
  sizeBytes: number;
  durationMs: number;
}
interface ManifestDto {
  id: string;
  displayName: string;
  hasVoice: boolean;
  hasSubtitle: boolean;
  spriteVersionNumber: number;
  voices: ManifestVoiceDto[];
}

/** 音频内存快照：激活时全量 fetch → blob URL（EP6；任一失败整体降级为无语音）。
 *  返回的 dispose 逐个 revoke 创建的 objectURL（FIX-2：切换/卸载时防泄漏） */
async function snapshotVoices(
  dir: string,
  voices: ManifestVoiceDto[]
): Promise<{
  entries: VoiceEntry[];
  resolve: (file: string) => string;
  dispose: () => void;
} | null> {
  const blobs = new Map<string, string>();
  const created: string[] = [];
  try {
    await Promise.all(
      voices.map(async (v) => {
        const res = await fetch(convertFileSrc(`${dir}/${v.file}`));
        if (!res.ok) throw new PetError("audio-load-fail");
        const url = URL.createObjectURL(await res.blob());
        created.push(url);
        blobs.set(v.file, url);
      })
    );
    return {
      // index 必须顺序编号：VoicePlayer.play 以 els[entry.index] 定位预载元素
      entries: voices.map((v, i) => ({
        index: i,
        group: v.group as VoiceEntry["group"],
        name: v.name,
        file: v.file,
      })),
      resolve: (file) => blobs.get(file) ?? "",
      dispose: () => {
        for (const url of created) {
          try {
            URL.revokeObjectURL(url);
          } catch {
            /* ignore */
          }
        }
        created.length = 0;
      },
    };
  } catch {
    // 快照中途失败：撤销已创建的 URL，不留泄漏
    for (const url of created) {
      try {
        URL.revokeObjectURL(url);
      } catch {
        /* ignore */
      }
    }
    return null;
  }
}

/**
 * 解析当前激活宠物（宠物窗口启动 / 热切换共用）。
 * 内置宠物：静态描述符 + 各自 manifest.json 语音；外部：scan + manifest + 图集探测 + 音频快照。
 * 任何失败抛错，调用方回落 DEFAULT_PET（spec §5.2 宠物永不白屏）。
 */
export async function resolveActivePet(): Promise<ActivePet> {
  const id = loadActiveId();
  const builtin = builtinPet(id);
  if (builtin) {
    try {
      const manifestUrl = builtin.spritesheetUrl.replace("spritesheet.webp", "manifest.json");
      const raw = await fetch(manifestUrl).then((r) => {
        if (!r.ok) throw new PetError("audio-load-fail");
        return r.json();
      });
      return { ...builtin, voices: parseManifest(raw) };
    } catch {
      return { ...builtin, hasVoice: false, hasSubtitle: false }; // 图集仍可显示
    }
  }
  const scan = await invoke<PetScanDto>("pet_scan", { id });
  if (!scan.spritesheet.exists) throw new PetError("sheet-missing");
  const rows = await probeSheetRows(convertFileSrc(`${scan.dir}/spritesheet.webp`));
  const manifest = await invoke<ManifestDto | null>("pet_read_manifest", { id });
  if (!manifest) {
    // 直投未生成 manifest：渲染可用的最低档（主窗口启动校验负责生成，spec §6.1）
    return {
      id,
      displayName: id,
      spritesheetUrl: convertFileSrc(`${scan.dir}/spritesheet.webp`),
      rows,
      hasVoice: false,
      hasSubtitle: false,
      voices: [],
      resolveVoiceUrl: () => "",
    };
  }
  const snap = manifest.hasVoice ? await snapshotVoices(scan.dir, manifest.voices) : null;
  if (manifest.hasVoice) persistVoiceCap(!!snap); // 双向：失败降 0，恢复回升 1
  return {
    id,
    displayName: manifest.displayName || id,
    spritesheetUrl: convertFileSrc(`${scan.dir}/spritesheet.webp`),
    rows,
    hasVoice: !!snap,
    hasSubtitle: manifest.hasSubtitle && !!snap,
    voices: snap?.entries ?? [],
    resolveVoiceUrl: snap?.resolve ?? (() => ""),
    dispose: snap?.dispose,
  };
}
