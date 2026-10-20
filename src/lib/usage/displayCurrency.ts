// 显示币种（USD / CNY）——费用展示的唯一换算层。
//
// 纪律（用户 2026-10-19 手工测试裁决）：
//  * 默认 USD、默认汇率 7.2；设置持久化在 localStorage（跨窗口用 storage 事件、同窗口用自定义事件）；
//  * **只做展示换算**：账本、价格源、手动覆盖价与 CSV 导出保持 USD，不在这里改数据；
//  * 只接受非负十进制字符串（`^\d+(\.\d+)?$`），科学计数 / 负号 / 千分位 / 空白一律视为非法；
//  * 金额 ≥0.01 保留 2 位小数；<0.01 最多 6 位（避免把 $0.000529 印成 $0.00）；
//  * 用 BigInt 十进制缩放做换算与四舍五入，避免浮点误差与大数科学计数法。
import { useCallback, useEffect, useState } from "react";

export type DisplayCurrency = "usd" | "cny";

export const DISPLAY_CURRENCY_STORAGE_KEY = "usage.displayCurrency";
export const USD_CNY_RATE_STORAGE_KEY = "usage.usdCnyRate";
export const DEFAULT_USD_CNY_RATE = "7.2";
/** 同窗口变更事件（localStorage 的 `storage` 事件只在其它窗口触发） */
export const DISPLAY_CURRENCY_CHANGED_EVENT = "usage-display-currency-changed";

const DECIMAL_RE = /^\d+(?:\.\d+)?$/;

interface ScaledDecimal {
  /** 去掉小数点后的非负整数（`0.001` → `1n`，scale=3） */
  value: bigint;
  /** 小数位数 */
  scale: number;
}

function parseDecimal(raw: string | null | undefined): ScaledDecimal | null {
  if (raw === null || raw === undefined) return null;
  if (!DECIMAL_RE.test(raw)) return null;
  const [whole, fraction = ""] = raw.split(".");
  return { value: BigInt(`${whole}${fraction}` || "0"), scale: fraction.length };
}

function pow10(exponent: number): bigint {
  return 10n ** BigInt(exponent);
}

/** 四舍五入（half-up）到指定小数位，返回 `目标值 × 10^decimals`。 */
function roundTo(value: bigint, scale: number, decimals: number): bigint {
  if (scale <= decimals) return value * pow10(decimals - scale);
  const divisor = pow10(scale - decimals);
  const quotient = value / divisor;
  const remainder = value % divisor;
  return remainder * 2n >= divisor ? quotient + 1n : quotient;
}

function groupWhole(whole: string): string {
  return whole.replace(/\B(?=(\d{3})+(?!\d))/g, ",");
}

/** 已缩放的非负整数 → 展示数字串（不含币种符号）。 */
function formatScaled(value: bigint, scale: number): string {
  // 阈值 0.01：value / 10^scale >= 1/100 ⇔ value * 100 >= 10^scale
  const isCentOrMore = value * 100n >= pow10(scale);
  const decimals = value === 0n ? 2 : isCentOrMore ? 2 : 6;
  const rounded = roundTo(value, scale, decimals);
  // 小于 0.0000005 的值在「最多 6 位」下四舍五入为 0：仍按 0.00 显示（不出现 0.0）
  if (rounded === 0n) return "0.00";
  const digits = rounded.toString().padStart(decimals + 1, "0");
  const whole = digits.slice(0, digits.length - decimals);
  let fraction = digits.slice(digits.length - decimals);
  if (!isCentOrMore && value !== 0n) {
    // 小额：最多 6 位，去掉无意义的尾零（0.0010 → 0.001）
    fraction = fraction.replace(/0+$/, "");
    if (fraction === "") fraction = "0";
  }
  return `${groupWhole(whole)}.${fraction}`;
}

function convert(
  value: string | null,
  currency: DisplayCurrency,
  rate: string | number
): { scaled: ScaledDecimal; symbol: string } | null {
  const amount = parseDecimal(value);
  if (!amount) return null;
  if (currency === "usd") return { scaled: amount, symbol: "$" };

  const rateText = typeof rate === "number" ? String(rate) : rate.trim();
  const parsedRate = parseDecimal(rateText);
  if (!parsedRate || parsedRate.value <= 0n) return null;
  return {
    scaled: {
      value: amount.value * parsedRate.value,
      scale: amount.scale + parsedRate.scale,
    },
    symbol: "¥",
  };
}

/**
 * 金额显示：USD → `$`，CNY → `¥`。
 * `null` / 非法十进制串 / 非法汇率 → `null`（由调用方显示「未估价」或 `—`，绝不假装 0）。
 */
export function formatMoney(
  value: string | null,
  currency: DisplayCurrency = "usd",
  rate: string | number = DEFAULT_USD_CNY_RATE
): string | null {
  const converted = convert(value, currency, rate);
  if (!converted) return null;
  return `${converted.symbol}${formatScaled(converted.scaled.value, converted.scaled.scale)}`;
}

/**
 * 原生币种金额显示（后端已按目标币种算好，不再乘汇率）：
 * USD → `$`，CNY → `¥`；非法/空值 → null。
 */
export function formatNativeMoney(value: string | null, currency: DisplayCurrency): string | null {
  const amount = parseDecimal(value);
  if (!amount) return null;
  const symbol = currency === "cny" ? "¥" : "$";
  return `${symbol}${formatScaled(amount.value, amount.scale)}`;
}

/** 原生币种单价显示（不含币种符号）；调用方补 `{{value}} {{currency}} / 1M tokens`。 */
export function formatNativeUnitPrice(
  value: string | null,
  _currency: DisplayCurrency
): string | null {
  const amount = parseDecimal(value);
  if (!amount) return null;
  return formatScaled(amount.value, amount.scale);
}

/**
 * 单价显示（不含币种符号）：USD / CNY 数值同样换算；
 * 调用方再用 `{{value}} {{currency}} / 1M tokens` 补单位。
 */
export function formatUnitPrice(
  value: string | null,
  currency: DisplayCurrency = "usd",
  rate: string | number = DEFAULT_USD_CNY_RATE
): string | null {
  const converted = convert(value, currency, rate);
  if (!converted) return null;
  return formatScaled(converted.scaled.value, converted.scaled.scale);
}

/** 读取本地持久化设置；非法值回退默认（坏数据不进入格式化层）。 */
export function readDisplayCurrency(): { currency: DisplayCurrency; rate: string } {
  if (typeof localStorage === "undefined") {
    return { currency: "usd", rate: DEFAULT_USD_CNY_RATE };
  }
  const storedCurrency = localStorage.getItem(DISPLAY_CURRENCY_STORAGE_KEY);
  const storedRate = localStorage.getItem(USD_CNY_RATE_STORAGE_KEY);
  const rateValid = storedRate !== null && DECIMAL_RE.test(storedRate) && Number(storedRate) > 0;
  return {
    currency: storedCurrency === "cny" ? "cny" : "usd",
    rate: rateValid ? (storedRate as string) : DEFAULT_USD_CNY_RATE,
  };
}

function notifyDisplayCurrencyChanged(): void {
  if (typeof window !== "undefined") {
    window.dispatchEvent(new Event(DISPLAY_CURRENCY_CHANGED_EVENT));
  }
}

/**
 * 显示币种 Hook：localStorage 持久化 + 同窗口自定义事件 / 跨窗口 storage 事件同步。
 * 汇率只接受正十进制字符串；非法输入被忽略（保留上一个有效值）。
 */
export function useDisplayCurrency() {
  const [state, setState] = useState(readDisplayCurrency);

  useEffect(() => {
    const sync = () => setState(readDisplayCurrency());
    window.addEventListener(DISPLAY_CURRENCY_CHANGED_EVENT, sync);
    window.addEventListener("storage", sync);
    return () => {
      window.removeEventListener(DISPLAY_CURRENCY_CHANGED_EVENT, sync);
      window.removeEventListener("storage", sync);
    };
  }, []);

  const setCurrency = useCallback((currency: DisplayCurrency) => {
    localStorage.setItem(DISPLAY_CURRENCY_STORAGE_KEY, currency);
    setState((prev) => ({ ...prev, currency }));
    notifyDisplayCurrencyChanged();
  }, []);

  const setRate = useCallback((raw: string) => {
    const next = raw.trim();
    if (!DECIMAL_RE.test(next) || Number(next) <= 0) return;
    localStorage.setItem(USD_CNY_RATE_STORAGE_KEY, next);
    setState((prev) => ({ ...prev, rate: next }));
    notifyDisplayCurrencyChanged();
  }, []);

  return { ...state, setCurrency, setRate };
}
