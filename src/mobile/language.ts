// 移动端语言决策模块（三终端语言自适应 C-6 的**唯一实现处**，需求书 M1 / M2 / M7）。
//
// 决策链（C-6，优先级从高到低，逐字对齐需求书 §1 统一原则）：
//   ① 手动记录 —— 桌面账本里本设备（cookie 身份）的手动语言（/host 的 `manualLanguage`），
//      或本次会话内的手动切换（switchMobileLanguage 钉住）；手动后自动跟随不再覆盖；
//   ② 桌面当前语言 —— /host 的 `desktopLanguage`（仅认 "zh"/"en"，其余值含 null/缺席
//      一律「桌面语言不可得」，控制者裁决）；
//   ③ 手机浏览器语言 —— 复用桌面同款归流判定 detectSystemLanguage（zh* 系 → zh、
//      en* → en、其余跳过；全局约束 1：不另造第二份分类器）；
//   ④ 英文兜底 —— 归流无可解析候选 → en（detectSystemLanguage 内建，与 fallbackLng 同口径）。
//
// 重评估时机（U2：连接时跟随，不做实时推送）：
//   - 启动：读手机本地手动缓存（若在）→ 直接定案（第①级，重连免闪免等待）；
//     否则以浏览器归流为**临时值**，等连接（/host）定案；
//   - 连接：Board 挂载拉 /host（C-4 载荷）→ applyConnectLanguage 重评估；
//   - 手动切换：switchMobileLanguage 立即生效 + 钉住 + 桌面记账（C-5 写接口）。
//
// 首帧闸（C-7）：首个决策未定前不渲染任何内容（main.tsx 渲染前 await），
// 防「先浏览器语言渲染、/host 到达后翻桌面语言」的可见闪变；/host 失败 / 超时
// 按浏览器语言 fail-open（M2 既定兜底，不无限白屏）。
//
// 手机本地持久化（控制者移交的实现裁量，全局约束 2）：**只持久化手动值**——
// 复用桌面 `tuvis.language.manual` 键（language-prefs 的读写函数），手机与桌面是
// 不同设备/浏览器，互不干扰。收益：手动用户重开 PWA / 刷新时第①级在启动即定案，
// 零闪变零等待。自动跟随结果**绝不**写入该键；账本（SSOT）显式无记录时清键，
// 保证「重新配对清账 → 下次连接回到跟随」的收敛不残留本地旧值（L2）。
import i18n from "@/i18n";
import {
  clearManualLanguage,
  detectSystemLanguage,
  navigatorLanguageCandidates,
  readManualLanguage,
  writeManualLanguage,
  type UiLanguage,
} from "@/i18n/language-prefs";
import { toast } from "sonner";
import { formatInvokeError } from "@/lib/invokeError";
import { fetchHost, pushDeviceLanguage } from "./api";

/** 首帧闸超时（毫秒）：/host 迟迟不回（慢隧道）时的 fail-open 上限——超时后按
 *  浏览器语言放行首帧（M2），迟到的 /host 结果仍会照常应用（其后到的大型翻转为
 *  fail-open 的既定代价，不为此无限期扣住首帧）。 */
export const FIRST_DECISION_TIMEOUT_MS = 4000;

/** /host 载荷中与本模块相关的 C-4 两字段（HostPayload 的子集；单独提出便于测试构造）。 */
export interface LanguageHostFields {
  /** 桌面当前语言（"zh"|"en"；null/缺席 = 未推送或不可得，控制者裁决一律按不可得） */
  desktopLanguage?: unknown;
  /** 本设备手动语言记录（"zh"|"en"；null = 无记录；缺席 = 旧版后端不知情） */
  manualLanguage?: unknown;
}

/** 会话内手动钉住（第①级的内存半边；null = 无手动，走自动跟随）。模块级单例：
 *  移动页是单页应用，Board 重挂载（403 重配）不换模块实例。 */
let pinnedManual: UiLanguage | null = null;

/** 连接探测**代数**（I1，评审 fix round 1）：每次手动切换自增。连接响应由
 *  fetchConnectPayload 在**请求发出时**钉住代数、applyConnectLanguage 在落地时对账——
 *  代数已前进 = 响应是先于最近一次手动切换串行化的**过期账面**（慢隧道在途窗口），
 *  其 manualLanguage 无论 null 与否都不得当账本真值落状态（否则刚做的手动选择会被
 *  迟到的旧响应可见回翻 / 刚写的手动缓存被旧账冲掉）。账本真值由下一条新鲜连接
 *  （Board 重挂载 / 页面重开）收敛；重配后的连接发出于切换之后，代数相同 = 新鲜，
 *  L2 清账语义不受守卫误伤。 */
let connectGeneration = 0;

/**
 * C-4 字段归一（控制者裁决：非 "zh"/"en" 的任何值——null / 数字 / 其他串——
 * 一律按「不可得 / 无记录」处理，Rust 只存不判，判定全在移动端）。
 * 返回 undefined 表示字段**缺席**（旧版后端），与显式 null（新后端明确「无」）区分：
 * 缺席时不动会话钉住与本地缓存（不知情不妄动），显式 null 才触发清账收敛。
 */
function normalizeLanguageField(raw: unknown): UiLanguage | null | undefined {
  if (raw === undefined) return undefined;
  return raw === "zh" || raw === "en" ? raw : null;
}

/**
 * C-6 决策链（纯函数，单一出处；表驱动测试直接锁四级优先序）。
 * ① manual > ② desktop > ③ browserCandidates 归流（detectSystemLanguage，其内建
 * ④ en 兜底）。不读任何全局态，可独立复测。
 */
export function resolveMobileLanguage(input: {
  manual: UiLanguage | null;
  desktop: UiLanguage | null;
  browserCandidates?: readonly (string | undefined | null)[];
}): UiLanguage {
  if (input.manual) return input.manual;
  if (input.desktop) return input.desktop;
  return detectSystemLanguage(input.browserCandidates);
}

/**
 * 生效语言应用（C-9 / M7 单点）：i18n 切语言 + `<html lang>` 同步为二值规范形
 * （zh / en），保障读屏器发音正确。幂等，可重复调用。
 */
export function applyMobileLanguage(lang: UiLanguage): void {
  if (i18n.language !== lang) void i18n.changeLanguage(lang);
  document.documentElement.lang = lang;
}

/**
 * 启动初始化（同步，main.tsx 渲染前调用一次）：读手机本地手动缓存定会话钉住，
 * 按第①级或浏览器归流定启动语言并应用（i18n 初值已由 @/i18n 同一来源算出，
 * 这里是权威对账 + html lang 应用点）。返回启动生效语言。
 */
export function initMobileLanguage(
  candidates?: readonly (string | undefined | null)[]
): UiLanguage {
  pinnedManual = readManualLanguage();
  const lang = pinnedManual ?? detectSystemLanguage(candidates ?? navigatorLanguageCandidates());
  applyMobileLanguage(lang);
  return lang;
}

/**
 * 连接探测一次（供首帧闸与 Board 挂载钩子共用）：在请求**发出时**钉住代数，响应落地时
 * 原样带回——applyConnectLanguage 据此判定响应是否先于最近一次手动切换（过期账面，I1）。
 * 拒绝（网络失败）原样上抛，由调用方按各自既有口径兜底（闸 fail-open / Board 静默）。
 */
export function fetchConnectPayload<T extends LanguageHostFields>(): Promise<{
  generation: number;
  payload: T | null;
}> {
  const generation = connectGeneration;
  return fetchHost<T>().then((payload) => ({ generation, payload }));
}

/**
 * 连接时语言重评估（Board 挂载拉 /host 后调用；C-4 两字段在此消费）。
 * 账本显式无记录（manualLanguage = null，含重新配对清账）→ 会话钉住与本地缓存
 * 一并清零（L2：重配 = 回到自动跟随）；账本有记录 → 本地缓存镜像之（仅手动值落盘）。
 * 返回重评估后的生效语言。幂等：首帧闸与 Board 各调一次互不冲突。
 *
 * `generation`：响应的探测代数（fetchConnectPayload 带回）。缺省 = 视为当前代
 * （未经代数钉住的直接调用面，如测试 / 旧调用方）。
 */
export function applyConnectLanguage(
  payload: LanguageHostFields | null,
  generation: number = connectGeneration
): UiLanguage {
  // I1：过期账面（响应在途期间发生了手动切换）整体忽略——钉住让决策仍取手动值，
  // 故本响应既不该改语言、更不该清钉住/缓存或镜像旧账（防可见回翻，见 connectGeneration 注）
  if (generation !== connectGeneration) return i18n.language as UiLanguage;
  const manualField = normalizeLanguageField(payload?.manualLanguage);
  if (manualField === null) {
    // 账本明确「无手动记录」：清会话钉住 + 清本地缓存（收敛不残留，见模块头注）
    pinnedManual = null;
    clearManualLanguage();
  } else if (manualField !== undefined) {
    // 账本有记录：缓存镜像账本（读写函数自带脏值/异常降级）。钉住优先级不变——
    // 会话内的手动切换比这条只读下发更新（正常两者一致；记账失败时不让旧账回翻本会话）
    writeManualLanguage(manualField);
  }
  const lang = resolveMobileLanguage({
    manual: pinnedManual ?? manualField ?? null,
    desktop: normalizeLanguageField(payload?.desktopLanguage) ?? null,
    browserCandidates: navigatorLanguageCandidates(),
  });
  applyMobileLanguage(lang);
  return lang;
}

/**
 * 首帧闸（C-7）：等首个语言决策定案后返回。调用方（main.tsx）在此之后才首次
 * render，保证「先英文后中文」类可见闪变不可能出现在首帧。
 *
 * - 有本地手动缓存（第①级启动已定案）→ 立即返回，不发请求（重连零等待）；
 * - 否则拉一次 /host 定案：载荷 → applyConnectLanguage；null（403 未配对，L1）/
 *   网络失败 → 第③级浏览器语言（fail-open，M2）；
 * - 超时（FIRST_DECISION_TIMEOUT_MS）→ 先按浏览器语言放行；迟到的 /host 结果
 *   照常应用（其后到的大型翻转是 fail-open 的既定代价）。
 *
 * 本函数**不拒绝**（全路径内部兜底），调用方无需 catch。
 */
export async function resolveFirstLanguage(opts?: { timeoutMs?: number }): Promise<UiLanguage> {
  // 第①级已定案（手动缓存）：决策已知，闸直接放行
  if (pinnedManual !== null) return i18n.language as UiLanguage;
  const timeoutMs = opts?.timeoutMs ?? FIRST_DECISION_TIMEOUT_MS;
  const decision = fetchConnectPayload<LanguageHostFields>()
    .then(({ generation, payload }) => applyConnectLanguage(payload, generation)) // null（403 未配对）→ 两字段归一 null → 浏览器归流
    .catch(() => applyConnectLanguage(null)); // 网络失败 → 第③级 fail-open（M2）
  // 决策先到时及时清掉超时定时器（竞速结束后让它空等 4s 只是资源微漏）
  let timerId: ReturnType<typeof setTimeout> | undefined;
  const timer = new Promise<void>((resolve) => {
    timerId = setTimeout(resolve, timeoutMs);
  });
  try {
    await Promise.race([decision, timer]);
  } finally {
    clearTimeout(timerId);
  }
  return i18n.language as UiLanguage;
}

/**
 * 切换生效入口（任务 4 的语言按钮消费；本任务只保证可调用且语义正确，不建 UI）：
 *  ① 立即生效——applyMobileLanguage（i18n + html lang，与 C-9 同一机制）；
 *  ② 钉住本设备决策为手动（C-6 第①级对本次会话余下时间成立，自动跟随不再覆盖），
 *     并写手机本地缓存（仅手动值落盘，全局约束 2）；
 *  ③ 桌面记账——POST /device-language（C-5 写接口，桌面按配对设备落账）。
 *
 * ③ 失败不得静默假成功：经 formatInvokeError 双语错误体系（g5）+ App 根部
 * Toaster 如实暴露；本会话内①②已生效的切换不回滚（用户看到的选择保持，
 * 账本会在下次连接时以其真值收敛）。
 */
export async function switchMobileLanguage(lang: UiLanguage): Promise<void> {
  applyMobileLanguage(lang);
  pinnedManual = lang;
  writeManualLanguage(lang);
  // 代数自增（I1）：在途连接响应自此视为过期账面——不得用切换前串行化的旧账
  // 清本钉住 / 冲缓存（见 connectGeneration 注）
  connectGeneration += 1;
  try {
    await pushDeviceLanguage(lang);
  } catch (e) {
    // ApiError 按 ModeBar / MessageComposer 既有惯例取 message（Error 对象直接
    // String 化会带 "Error: " 前缀，码解析正则会失配）
    toast.error(formatInvokeError(e instanceof Error ? e.message : String(e), i18n.t));
  }
}
