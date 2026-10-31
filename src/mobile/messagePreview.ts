// 移动端消息正文预览的共享词法（P4 抽出为单源）：来源标记签名与附件内联标记的
// 正则、剥离、占位替换只在此定义一份，MessageComposer（发送区队列条预览、「修改」
// 放回正文回填）与 CodexQueuePanel（待发队列行正文预览）共用——禁止复制两份正则
// （两处口径漂移会导致同一正文在发送区与队列里剥出不同结果）。
import i18n from "@/i18n";

/** W1 来源标记（服务端 compose_injection 注入，非用户正文）：预览与「修改」放回正文
 *  时剥离——不剥则修改重发会二次叠加签名。
 *
 *  **丁T3 裁2 起签名在尾部**（原名在头部）：正则从 `^\[mobile…\]` 改为 `\s*\[mobile…\]$`
 *  ——尾部形态（`{正文} [mobile 设备名]`）。不改为尾部就会在预览里残留签名、在
 *  「修改」回填时把签名当正文带回输入框（重发即二次叠加）。
 *  容错：正文里偶然出现 `[mobile …]` 字样但**不在尾部**时不剥（与 Rust 侧
 *  `normalize::strip_mobile_signature` 同口径：只剥形态完整的尾签名）。 */
export const MOBILE_SIGNATURE_RE = /\s*\[mobile[^\]]*\]$/;

/** 附件内联标记（文件池既有约定）：<image|file path="…"> → 预览中替换为 [附件]。
 *  展示标记文案走 i18n 键（M5；仅展示用，「修改」放回正文不受影响）。
 *  g 旗标仅供 String.replace 使用（replace 对全局正则每次从 0 起扫并复位
 *  lastIndex，跨模块共享同一实例安全）。 */
export const ATTACH_MARKUP_RE = /<(?:image|file)\s+path="[^"]*">/g;

/** 剥离尾部 `[mobile 设备名]` 签名（正则不命中则原样返回）：预览与修改放回共用。
 *  前后空白一并剥（服务端产出形态为 `{正文} [mobile X]`）。 */
export function stripMobileSignature(content: string): string {
  return content.replace(MOBILE_SIGNATURE_RE, "");
}

/** 预览主体（共用前处理）：剥尾部移动端签名 → 附件标记替换为可读占位
 *  （mobile.composer.attachMark）。截断长度与换行折叠按展示场景由调用方处理
 *  （composer 队列条截 12 字符；队列面板行预览截 40 字符 + 单行归一）。 */
export function previewBody(content: string): string {
  return stripMobileSignature(content).replace(
    ATTACH_MARKUP_RE,
    i18n.t("mobile.composer.attachMark")
  );
}
