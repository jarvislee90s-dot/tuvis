// invoke 错误的本地化渲染（issue #36-3 起，#123/#150/#157 全量迁移后为通用解析）。
//
// 后端错误串形态（约定全文见 src-tauri/src/errcode.rs，互为镜像）：
//   形态 A：`CODE: 原中文句`——命中已知码 → 整句换 i18n 文案（en 纯英文语义句）；
//   形态 B：`CODE:机器参数: 原中文句`——模板 {{param}} 插值；
//   未知码 / 无码 → 原样返回（不静默吞，未迁移域走原文）。
// locale 键 = `errors.` + camelCase(码)，由 scripts/check-error-codes.mjs 对账
// （Rust 用的码必须有 zh/en 双键；locale 孤儿键报错）。
// 既有三码 W5_* 与 PRESET_* 以 legacy 别名映射到历史键，保持零回归。

type TranslateFn = (key: string, options?: Record<string, unknown>) => string;

// legacy（迁移前已在用，键名与码不同名，显式别名；正则保持原口径）
const LEGACY: { re: RegExp; key: string; param?: "m1" }[] = [
  { re: /^W5_TOOL_DISABLED:(.+)$/, key: "errors.toolDisabled", param: "m1" },
  { re: /^W5_APPLY_IN_PROGRESS/, key: "errors.applyInProgress" },
  { re: /^W5_APPLY_TASK_FAILED/, key: "errors.applyTaskFailed" },
  { re: /^PRESET_ACTIVE:(.+)$/, key: "errors.presetActive", param: "m1" },
  { re: /^PRESET_NATIVE_IN_UNIVERSAL:(.+)$/, key: "errors.presetNativeInUniversal", param: "m1" },
];

// 形态 A/B 的统一前缀解析：码字符集 [A-Z][A-Z0-9_]+，B 形的参数段不含空格与冒号
const CODED = /^([A-Z][A-Z0-9_]{2,})(?::([^:\s]+))?(?::\s?(.*))?$/;

/** 码 → errors 段键名：REMOTE_NO_TUNNEL_TOKEN → remoteNoTunnelToken */
const camel = (code: string) =>
  code
    .toLowerCase()
    .split("_")
    .map((w, i) => (i === 0 ? w : w.charAt(0).toUpperCase() + w.slice(1)))
    .join("");

export function formatInvokeError(e: unknown, t: TranslateFn): string {
  const raw = typeof e === "string" ? e : String(e);
  for (const l of LEGACY) {
    const m = raw.match(l.re);
    if (m) {
      const translated = t(
        l.key,
        l.param === "m1" ? { param: m[1], tools: m[1], id: m[1] } : undefined
      );
      // 键尚未登记（域迁移中间态）→ 原文回退，与未迁移域行为一致
      if (translated !== l.key) return translated;
      return raw;
    }
  }
  const m = raw.match(CODED);
  if (m) {
    const key = `errors.${camel(m[1])}`;
    // 模板存在与否由 check-error-codes.mjs 门禁保证（Rust 码 ⊆ locale 双键）；
    // 未登记的码（迁移中间态/第三方）走原文回退。
    const translated = t(key, m[2] ? { param: m[2] } : undefined);
    if (translated !== key) return translated;
  }
  return raw;
}
