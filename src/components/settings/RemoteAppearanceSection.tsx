import { useCallback, useEffect, useState, type CSSProperties } from "react";
import { invoke } from "@tauri-apps/api/core";
import { useTranslation } from "react-i18next";
import { SETTINGS_CARD_TITLE } from "@/components/settings/typography";
import { TOOL_BRAND_COLORS, TOOL_BRAND_COLORS_DARK } from "@/mobile/board-logic";
import type { AgentType } from "@/types/session";

// 远程端外观配置器（2026-10-05 UI 改版，spec §5；参考稿
// docs/design/remote-ui/04-外观配置-设置节-定稿.html）：桌面端保存 → settings KV
//（remote_ui_config）→ /m/api/v1/ui-config 下发 → 移动端三级回退应用。
// 视觉 Token 与 src/mobile/mobile.css 的六皮肤表逐字同源（改一处必须同步另一处）

interface UiConfig {
  daySkin: string;
  nightSkin: string;
  font: string;
  radius: number;
  accent: string;
}

const DEFAULTS: UiConfig = {
  daySkin: "lpaper",
  nightSkin: "npaper",
  font: "std",
  radius: 12,
  accent: "edge",
};

// 六皮肤 Token 表（与 mobile.css :root/.dark 及 data-skin 覆盖表一致）
const SKINS: Record<
  string,
  {
    pg: string;
    cbg: string;
    cb: string;
    tx: string;
    mut: string;
    sh: string;
    bub: string;
    btnp: string;
    btnpt: string;
  }
> = {
  lpaper: {
    pg: "#F8F3E9",
    cbg: "#FFFDF8",
    cb: "#EAE2CF",
    tx: "#2B2417",
    mut: "#97907E",
    sh: "0 1px 2px rgba(80,60,20,.06),0 3px 10px rgba(80,60,20,.05)",
    bub: "#EFE8D8",
    btnp: "#1A1915",
    btnpt: "#FFFFFF",
  },
  lpure: {
    pg: "#FFFFFF",
    cbg: "#FFFFFF",
    cb: "#E3E3E1",
    tx: "#141414",
    mut: "#8F8F8B",
    sh: "none",
    bub: "#F0F0EE",
    btnp: "#1A1915",
    btnpt: "#FFFFFF",
  },
  lmist: {
    pg: "#EFF3F2",
    cbg: "#FFFFFF",
    cb: "#DFE7E5",
    tx: "#1D2422",
    mut: "#87928F",
    sh: "0 1px 2px rgba(30,60,55,.05),0 3px 10px rgba(30,60,55,.04)",
    bub: "#E2EBE8",
    btnp: "#14201D",
    btnpt: "#FFFFFF",
  },
  npaper: {
    pg: "#271D0F",
    cbg: "#3A2C1A",
    cb: "#6E5836",
    tx: "#F0E9DC",
    mut: "#B0A896",
    sh: "0 1px 3px rgba(0,0,0,.35)",
    bub: "#564427",
    btnp: "#EFE9DC",
    btnpt: "#241B0E",
  },
  npure: {
    pg: "#0A0A0A",
    cbg: "#1C1C1C",
    cb: "#4D4D4D",
    tx: "#F5F5F5",
    mut: "#9A9A9A",
    sh: "none",
    bub: "#2E2E2E",
    btnp: "#F5F5F5",
    btnpt: "#111111",
  },
  ndeep: {
    pg: "#0C201B",
    cbg: "#173229",
    cb: "#3B6353",
    tx: "#E8F0EC",
    mut: "#8AA39A",
    sh: "0 1px 3px rgba(0,0,0,.4)",
    bub: "#24453A",
    btnp: "#DDEBE3",
    btnpt: "#0C1A14",
  },
};

const FONTS: Record<string, { ui: string; key: string }> = {
  std: { ui: "-apple-system,'PingFang SC',sans-serif", key: "fontStd" },
  term: { ui: "ui-monospace,'SF Mono',Menlo,monospace", key: "fontTerm" },
  round: { ui: "ui-rounded,'Yuanti SC','PingFang SC',sans-serif", key: "fontRound" },
  serif: { ui: "ui-serif,'New York','Songti SC',serif", key: "fontSerif" },
};

const RADII = ["8", "12", "18", "22"];
const ACCENTS = ["edge", "top", "tint", "none"];

function normalize(raw: string | null): UiConfig {
  if (!raw) return { ...DEFAULTS };
  try {
    const v = JSON.parse(raw) as Partial<UiConfig>;
    return {
      daySkin: typeof v.daySkin === "string" ? v.daySkin : DEFAULTS.daySkin,
      nightSkin: typeof v.nightSkin === "string" ? v.nightSkin : DEFAULTS.nightSkin,
      font: typeof v.font === "string" ? v.font : DEFAULTS.font,
      radius: typeof v.radius === "number" ? v.radius : DEFAULTS.radius,
      accent: typeof v.accent === "string" ? v.accent : DEFAULTS.accent,
    };
  } catch {
    return { ...DEFAULTS };
  }
}

/** 预览盒公共样式：按 (皮肤, 字体, 圆角) 注入 Token 变量 */
function resVars(skinKey: string): CSSProperties {
  const s = SKINS[skinKey] ?? SKINS.lpaper;
  return {
    background: s.pg,
    ["--cbg" as string]: s.cbg,
    ["--cb" as string]: s.cb,
    ["--tx" as string]: s.tx,
    ["--mut" as string]: s.mut,
    ["--sh" as string]: s.sh,
    ["--bub" as string]: s.bub,
    ["--btnp" as string]: s.btnp,
    ["--btnpt" as string]: s.btnpt,
  };
}

export default function RemoteAppearanceSection() {
  const { t } = useTranslation();
  const [loadFailed, setLoadFailed] = useState(false);
  const [config, setConfig] = useState<UiConfig>({ ...DEFAULTS });
  const [savedConfig, setSavedConfig] = useState<UiConfig>({ ...DEFAULTS });
  const [justSaved, setJustSaved] = useState(false);
  const [saving, setSaving] = useState(false);

  useEffect(() => {
    invoke<string | null>("get_setting", { key: "remote_ui_config" })
      .then((raw) => {
        const cfg = normalize(raw);
        setConfig(cfg);
        setSavedConfig(cfg);
      })
      .catch(() => setLoadFailed(true));
  }, []);

  const dirty = JSON.stringify(config) !== JSON.stringify(savedConfig);
  const patch = (p: Partial<UiConfig>) => {
    setConfig((c) => ({ ...c, ...p }));
    setJustSaved(false);
  };

  const save = useCallback(async () => {
    setSaving(true);
    try {
      await invoke("set_setting", { key: "remote_ui_config", value: JSON.stringify(config) });
      setSavedConfig({ ...config });
      setJustSaved(true);
    } finally {
      setSaving(false);
    }
  }, [config]);

  const day = SKINS[config.daySkin] ?? SKINS.lpaper;
  const night = SKINS[config.nightSkin] ?? SKINS.npaper;

  /** 看板预览卡（边缘点缀四式与移动端 Board 同构）。檐色 = board-logic 台账表
   *  TOOL_BRAND_COLORS(_DARK)，与移动看板真实渲染**同源**——2026-10-14 评审修正：
   *  旧硬编码按显示名（"Codex"）比对 tool id，大小写失配从未命中，预览檐色一直
   *  落灰；本批顺带修活并按台账对标（预览什么色，移动端就是什么色） */
  const BoardCard = ({
    skin,
    night,
    toolId,
    toolLabel,
    title,
    msg,
    badge,
    badgeCls,
  }: {
    skin: typeof day;
    night: boolean;
    toolId: AgentType;
    toolLabel: string;
    title: string;
    msg: string;
    badge: string;
    badgeCls: string;
  }) => {
    const edge = (night ? TOOL_BRAND_COLORS_DARK[toolId] : TOOL_BRAND_COLORS[toolId]) ?? "#8A94A6";
    return (
      <div
        className="relative overflow-hidden border p-2.5"
        style={{
          borderRadius: "var(--rr)",
          background:
            config.accent === "tint" ? `color-mix(in srgb, ${edge} 7%, ${skin.cbg})` : skin.cbg,
          borderColor:
            config.accent === "tint" ? `color-mix(in srgb, ${edge} 28%, ${skin.cb})` : skin.cb,
          boxShadow: skin.sh,
          paddingLeft: config.accent === "edge" ? 12 : 10,
        }}
      >
        {config.accent === "edge" && (
          <span className="absolute inset-y-0 left-0 w-[3px]" style={{ background: edge }} />
        )}
        {config.accent === "top" && (
          <span className="absolute inset-x-0 top-0 h-[2px]" style={{ background: edge }} />
        )}
        <div className="flex items-center gap-1.5">
          <span className="h-3 w-3 shrink-0 rounded" style={{ background: edge }} />
          <span className="text-[11px] font-semibold" style={{ color: skin.tx }}>
            {toolLabel}
          </span>
          <span className="ml-auto font-mono text-[9px]" style={{ color: skin.mut }}>
            2 min
          </span>
        </div>
        <div className="mt-1 truncate text-[11px] font-semibold" style={{ color: skin.tx }}>
          {title}
        </div>
        <div className="truncate text-[10px]" style={{ color: skin.mut }}>
          {msg}
        </div>
        <span
          className={`mt-1.5 inline-flex items-center gap-1 rounded-full px-2 py-0.5 text-[9px] font-medium ${badgeCls}`}
        >
          <span className="h-1 w-1 rounded-full bg-current" />
          {badge}
        </span>
      </div>
    );
  };

  const previewBox = (mode: "day" | "night") => {
    const skin = mode === "day" ? day : night;
    const fontK = FONTS[config.font]?.ui ?? FONTS.std.ui;
    const rr = `${config.radius}px`;
    return (
      <div
        className="flex-1 space-y-2 rounded-2xl border p-3"
        style={{
          ...resVars(mode === "day" ? config.daySkin : config.nightSkin),
          borderRadius: 18,
          fontFamily: fontK,
          ["--rr" as string]: rr,
        }}
      >
        <div className="text-[12px] font-bold" style={{ color: skin.tx }}>
          {mode === "day"
            ? t("settings.remoteAppearance.day")
            : t("settings.remoteAppearance.night")}
        </div>
        {/* 看板 */}
        <div className="text-[10px]" style={{ color: skin.mut }}>
          {t("settings.remoteAppearance.board")}
        </div>
        <BoardCard
          skin={skin}
          night={mode === "night"}
          toolId="codex"
          toolLabel="Codex"
          title={t("settings.remoteAppearance.demoTitle")}
          msg={t("settings.remoteAppearance.demoMsg")}
          badge={t("settings.remoteAppearance.statusWaiting")}
          badgeCls="bg-[#FEE9E5] text-[#B3261E]"
        />
        {/* 对话流 */}
        <div className="text-[10px]" style={{ color: skin.mut }}>
          {t("settings.remoteAppearance.chat")}
        </div>
        <div className="space-y-1.5">
          <div
            className="ml-auto max-w-[80%] rounded-[13px_13px_4px_13px] px-2.5 py-1.5 text-[10px]"
            style={{ background: skin.bub, color: skin.tx }}
          >
            {t("settings.remoteAppearance.demoUser")}
          </div>
          <div className="px-0.5 text-[10px]" style={{ color: skin.tx }}>
            {t("settings.remoteAppearance.demoAssistant")}
          </div>
          <div
            className="flex items-center gap-1.5 rounded-lg border px-2 py-1 text-[9px]"
            style={{ background: skin.cbg, borderColor: skin.cb, color: skin.mut }}
          >
            <span className="truncate font-mono" style={{ color: skin.tx }}>
              grep -n "rounded" src/mobile/Board.tsx
            </span>
          </div>
          <div className="flex items-center gap-1.5 pt-1">
            <span
              className="flex h-5 w-5 items-center justify-center rounded-full border text-[9px]"
              style={{ borderColor: skin.cb, color: skin.mut }}
            >
              ＋
            </span>
            <span
              className="flex-1 rounded-lg border px-2 py-1 text-[9px]"
              style={{ background: skin.cbg, borderColor: skin.cb, color: skin.mut }}
            >
              {t("settings.remoteAppearance.demoPlaceholder")}
            </span>
            <span
              className="rounded-lg px-2.5 py-1 text-[9px] font-semibold"
              style={{ background: skin.btnp, color: skin.btnpt }}
            >
              {t("settings.remoteAppearance.demoSend")}
            </span>
          </div>
        </div>
        {/* 审批卡 */}
        <div className="text-[10px]" style={{ color: skin.mut }}>
          {t("settings.remoteAppearance.approve")}
        </div>
        <div
          className="rounded-xl border-[1.5px] border-[#E2A79E] p-2"
          style={{ background: skin.cbg }}
        >
          <div className="flex items-center gap-1 text-[10px] font-bold text-[#B3261E]">
            <span className="h-1 w-1 rounded-full bg-[#B3261E]" />
            {t("settings.remoteAppearance.statusWaiting")}
          </div>
          <div
            className="mt-1.5 rounded-md border px-2 py-1 font-mono text-[9px]"
            style={{ background: skin.pg, borderColor: skin.cb, color: skin.tx }}
          >
            $ pnpm install
          </div>
          <div className="mt-1.5 flex gap-1.5">
            <span
              className="flex-1 rounded-lg py-1 text-center text-[9px] font-semibold"
              style={{ background: skin.btnp, color: skin.btnpt }}
            >
              {t("settings.remoteAppearance.allow")}
            </span>
            <span
              className="flex-1 rounded-lg border py-1 text-center text-[9px]"
              style={{ borderColor: skin.cb, color: skin.mut }}
            >
              {t("settings.remoteAppearance.deny")}
            </span>
          </div>
        </div>
        {/* 文件面板 */}
        <div className="text-[10px]" style={{ color: skin.mut }}>
          {t("settings.remoteAppearance.files")}
        </div>
        <div
          className="rounded-xl border p-2"
          style={{ background: skin.cbg, borderColor: skin.cb }}
        >
          {["src/mobile/Board.tsx", "src/index.css"].map((p2, i) => (
            <div
              key={p2}
              className="flex items-center gap-1.5 border-b py-1 text-[9px] last:border-b-0"
              style={{ borderColor: `${skin.cb}80` }}
            >
              <span className="truncate font-mono" style={{ color: skin.tx }}>
                {p2}
              </span>
              {i === 0 && (
                <span className="rounded bg-[#FEE9E5] px-1 text-[8px] text-[#B3261E]">改</span>
              )}
              <span className="ml-auto font-mono text-[8px]" style={{ color: skin.mut }}>
                {i === 0 ? "12:04" : "11:12"}
              </span>
            </div>
          ))}
        </div>
      </div>
    );
  };

  const optBtn = (on: boolean) =>
    `cursor-pointer rounded-lg border px-2 py-1.5 text-center text-xs transition-colors ${
      on
        ? "border-primary bg-primary/5 font-semibold text-foreground ring-1 ring-primary"
        : "border-border bg-card text-muted-foreground hover:border-primary/40"
    }`;

  return (
    <div className="overflow-hidden rounded-xl border">
      {/* 卡头条（2026-10-11 两级标题拉开）：折叠交互按用户裁决**移除**（设置页为独立
          窗口，空间足够，配置器常展开）；h2 语义补位与其它卡头条一致 */}
      <div className="bg-muted/40 flex items-baseline gap-2 border-b px-4 py-2.5">
        <h2 className={SETTINGS_CARD_TITLE}>{t("settings.remoteAppearance.title")}</h2>
        <span className="text-muted-foreground text-xs">{t("settings.remoteAppearance.hint")}</span>
        <span className="ml-auto rounded-full bg-amber-600/10 px-2 py-0.5 text-[11px] font-medium text-amber-700 dark:text-amber-400">
          {t("settings.remoteAppearance.scope")}
        </span>
      </div>

      <div className="space-y-4 px-4 py-4">
        {loadFailed && (
          <p className="text-xs text-amber-700 dark:text-amber-400">
            {t("settings.remoteAppearance.loadFail")}
          </p>
        )}

        {/* ① 字体（自证样式：文字即其字体本样） */}
        <div className="border-b pb-4">
          <div className="text-muted-foreground mb-1.5 text-[11px] font-bold tracking-wide">
            {t("settings.remoteAppearance.fontLabel")}
          </div>
          <div className="grid grid-cols-4 gap-2">
            {Object.entries(FONTS).map(([k, f]) => (
              <button
                key={k}
                type="button"
                style={{ fontFamily: f.ui }}
                className={optBtn(config.font === k)}
                onClick={() => patch({ font: k })}
              >
                {t(`settings.remoteAppearance.${f.key}`)}
                <span className="mt-0.5 block text-[10px] font-normal opacity-70">Aa 123</span>
              </button>
            ))}
          </div>
        </div>

        {/* ② 皮肤（白天/夜间各选其一） */}
        <div className="border-b pb-4">
          <div className="text-muted-foreground mb-1.5 text-[11px] font-bold tracking-wide">
            {t("settings.remoteAppearance.skinLabel")}
          </div>
          <div className="grid grid-cols-6 gap-2">
            {Object.entries(SKINS).map(([k, s]) => {
              const isDay = k.startsWith("l");
              const on = config.daySkin === k || config.nightSkin === k;
              return (
                <button
                  key={k}
                  type="button"
                  className={optBtn(on)}
                  onClick={() => patch(isDay ? { daySkin: k } : { nightSkin: k })}
                >
                  <span
                    className="mx-auto mb-1 flex h-7 w-7 items-center justify-center rounded-full border"
                    style={{ background: s.pg, borderColor: "rgba(0,0,0,.12)" }}
                  >
                    <span
                      className="block h-3 w-2.5 rounded-sm border"
                      style={{ background: s.cbg, borderColor: "rgba(255,255,255,.4)" }}
                    />
                  </span>
                  <span className="block text-[10px] font-semibold">
                    {t(`settings.remoteAppearance.skin_${k}`)}
                  </span>
                  <span className="block text-[9px] opacity-70">
                    {isDay
                      ? t("settings.remoteAppearance.day")
                      : t("settings.remoteAppearance.night")}
                  </span>
                </button>
              );
            })}
          </div>
        </div>

        {/* ③ 圆角 */}
        <div className="border-b pb-4">
          <div className="text-muted-foreground mb-1.5 text-[11px] font-bold tracking-wide">
            {t("settings.remoteAppearance.radiusLabel")}
          </div>
          <div className="grid grid-cols-4 gap-2">
            {RADII.map((r) => (
              <button
                key={r}
                type="button"
                className={optBtn(String(config.radius) === r)}
                onClick={() => patch({ radius: Number(r) })}
              >
                {t(`settings.remoteAppearance.r_${r}`)}
                <span className="mt-0.5 block text-[10px] font-normal opacity-70">{r}px</span>
              </button>
            ))}
          </div>
        </div>

        {/* ④ 品牌点缀 */}
        <div className="border-b pb-4">
          <div className="text-muted-foreground mb-1.5 text-[11px] font-bold tracking-wide">
            {t("settings.remoteAppearance.accentLabel")}
          </div>
          <div className="grid grid-cols-4 gap-2">
            {ACCENTS.map((a) => (
              <button
                key={a}
                type="button"
                className={optBtn(config.accent === a)}
                onClick={() => patch({ accent: a })}
              >
                {t(`settings.remoteAppearance.a_${a}`)}
              </button>
            ))}
          </div>
        </div>

        {/* 预览：左白天右夜间 × 四排（看板/对话流/审批卡/文件面板） */}
        <div>
          <div className="text-foreground mb-1.5 text-xs font-bold">
            {t("settings.remoteAppearance.preview")}
          </div>
          <div className="grid grid-cols-2 gap-3">
            {previewBox("day")}
            {previewBox("night")}
          </div>
        </div>

        {/* 保存条：改动不自动生效（用户裁决），显式「保存并下发」 */}
        <div className="flex min-h-9 items-center gap-2.5 border-t pt-3">
          {!dirty && !justSaved && (
            <span className="text-muted-foreground text-xs">
              {t("settings.remoteAppearance.clean")}
            </span>
          )}
          {dirty && (
            <span className="rounded-full bg-amber-600/10 px-2.5 py-1 text-xs text-amber-700 dark:text-amber-400">
              {t("settings.remoteAppearance.dirty")}
            </span>
          )}
          {justSaved && (
            <span className="text-xs font-medium text-emerald-700 dark:text-emerald-400">
              {t("settings.remoteAppearance.saved")}
            </span>
          )}
          <button
            type="button"
            disabled={!dirty || saving}
            onClick={() => void save()}
            className="bg-primary text-primary-foreground ml-auto rounded-lg px-5 py-2 text-xs font-semibold disabled:opacity-40"
          >
            {t("settings.remoteAppearance.save")}
          </button>
        </div>
      </div>
    </div>
  );
}
