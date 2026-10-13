// 桌宠配置 — localStorage 单后端，跨窗口 storage 事件同步（spec §10）
import { loadVoiceCap, ACTIVE_KEY, ACTIVE_NAME_KEY, ACTIVE_LOCAL_EVENT } from "./petRuntime";

export type PetAction = "jumping" | "waving" | "failed" | "waiting" | "review" | "running";
export type PetScale = 0.75 | 1 | 1.25;

export interface PetConfig {
  alwaysOnTop: boolean;
  muted: boolean;
  talkative: boolean;
  gravity: boolean;
  scale: PetScale;
  dblAction: PetAction;
  approvalAction: PetAction;
  errorAction: PetAction; // 占位：v1 无触发场景（spec D9/D14）
  doneAction: PetAction;
  /** 启动跟随（2026-10-11 用户裁决）：开 = 软件每次启动宠物自动跟出（哪怕上次
   *  隐身——显隐开关只管本会话中途）；关 = 启动后先不出，托盘/标题栏随时可开。
   *  未设置过 = 默认开（DEFAULT 兜底）。消费点 = pet 窗口挂载靴（pets 页） */
  followStartup: boolean;
}

export const CONFIG_KEY = "mam-pet-config";
export const VISIBLE_KEY = "mam-pet-visible";
export const POSITION_KEY = "mam-pet-position";

export const PET_ACTIONS: PetAction[] = [
  "jumping",
  "waving",
  "failed",
  "waiting",
  "review",
  "running",
];
export const PET_SCALES: PetScale[] = [0.75, 1, 1.25];

const DEFAULT_CONFIG: PetConfig = {
  alwaysOnTop: true,
  muted: false,
  talkative: true,
  gravity: true,
  scale: 1,
  dblAction: "waving",
  approvalAction: "waiting",
  errorAction: "failed",
  doneAction: "jumping",
  followStartup: true,
};

const ACTION_KEYS = ["dblAction", "approvalAction", "errorAction", "doneAction"] as const;
// followStartup 在 BOOL_KEYS 里一起享受 sanitize 白名单（旧存档无该键 → DEFAULT true 兜底）
const BOOL_KEYS = ["alwaysOnTop", "muted", "talkative", "gravity", "followStartup"] as const;

function sanitize(raw: unknown): PetConfig {
  const out = { ...DEFAULT_CONFIG };
  if (raw && typeof raw === "object") {
    const p = raw as Record<string, unknown>;
    for (const k of BOOL_KEYS) if (typeof p[k] === "boolean") out[k] = p[k] as boolean;
    if (PET_SCALES.includes(p.scale as PetScale)) out.scale = p.scale as PetScale;
    for (const k of ACTION_KEYS)
      if (PET_ACTIONS.includes(p[k] as PetAction)) out[k] = p[k] as PetAction;
  }
  return out;
}

export function loadConfig(): PetConfig {
  try {
    const raw = localStorage.getItem(CONFIG_KEY);
    return raw ? sanitize(JSON.parse(raw)) : { ...DEFAULT_CONFIG };
  } catch {
    return { ...DEFAULT_CONFIG };
  }
}

const listeners = new Set<() => void>();
const emit = () => listeners.forEach((fn) => fn());

export function saveConfig(patch: Partial<PetConfig>): void {
  localStorage.setItem(CONFIG_KEY, JSON.stringify(sanitize({ ...loadConfig(), ...patch })));
  emit();
}

export function subscribeConfig(fn: () => void): () => void {
  listeners.add(fn);
  // storage 事件只在"其它窗口"修改时触发；本窗口修改靠 emit。
  // 激活指针/展示名键纳入监听（P1-6）：同窗口切换/管理对话框改指针后，设置页
  // "当前宠物"标签经 ACTIVE_LOCAL_EVENT 即时刷新，跨窗口改动则经 storage 键过滤触达
  const onStorage = (e: StorageEvent) => {
    if (
      e.key === null ||
      e.key === CONFIG_KEY ||
      e.key === VISIBLE_KEY ||
      e.key === ACTIVE_KEY ||
      e.key === ACTIVE_NAME_KEY
    )
      fn();
  };
  const onActiveLocal = () => fn();
  window.addEventListener("storage", onStorage);
  window.addEventListener(ACTIVE_LOCAL_EVENT, onActiveLocal);
  return () => {
    listeners.delete(fn);
    window.removeEventListener("storage", onStorage);
    window.removeEventListener(ACTIVE_LOCAL_EVENT, onActiveLocal);
  };
}

export function loadVisible(): boolean {
  try {
    return localStorage.getItem(VISIBLE_KEY) === "1";
  } catch {
    return false;
  }
}

export function saveVisible(v: boolean): void {
  localStorage.setItem(VISIBLE_KEY, v ? "1" : "0");
  emit();
}

export interface PetPosition {
  x: number;
  y: number;
}

export function loadPosition(): PetPosition | null {
  try {
    const raw = localStorage.getItem(POSITION_KEY);
    if (!raw) return null;
    const p = JSON.parse(raw);
    if (Number.isFinite(p?.x) && Number.isFinite(p?.y)) return { x: p.x, y: p.y };
  } catch {
    // ignore
  }
  return null;
}

export function savePosition(pos: PetPosition): void {
  localStorage.setItem(
    POSITION_KEY,
    JSON.stringify({ x: Math.round(pos.x), y: Math.round(pos.y) })
  );
}

/** 完成提示音接管：宠物开启且当前宠物具备语音能力（无语音外部宠物回落主看板，spec §5.2） */
export function petSoundTakeover(): boolean {
  return loadVisible() && loadVoiceCap();
}

/** 通知浮窗抑制：宠物开启即抑制——气泡是唯一通知面（spec W1，浮窗与系统通知降级路径
 *  均在 useNotification 的同一 if 内，天然一并静默） */
export function petSuppressPopup(): boolean {
  return loadVisible();
}
