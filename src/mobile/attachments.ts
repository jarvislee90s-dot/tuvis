/**
 * 附件上传的跨面板共享件（三轮 C）：MessageComposer 与 ZcodeControlPanel 共用
 * 同一上传管线（api.uploadAttachment → /m/api/v1/session-attachment）、同一附件
 * 条目形状与同一正文引用约定。只抽「形状 + 纯函数」，各面板保留自己的编排
 * （MessageComposer 有速率估算/排队语义，ZCode 面板面小，不强行合一）。
 */

/** 在途附件条目（上传中/就绪/失败；取消=移除 chip 并中断 XHR，无独立状态）。 */
export type PendingAttachment = {
  id: string;
  name: string;
  isImage: boolean;
  status: "uploading" | "ready" | "failed";
  /** 上传成功后的服务端落盘绝对路径（ready 才有）。 */
  path?: string;
  /** 失败原因（服务端错误码或本地化文案；failed 才有）。 */
  error?: string;
  loaded?: number;
  total?: number | null;
  /** 瞬时速率估算（字节/秒；仅 MessageComposer 使用，ZCode 面板不带）。 */
  rateBps?: number | null;
};

/** 图片判定：与 MessageComposer/服务端 read_zcode_attachment_refs 同一口径。 */
export const isImageFile = (file: File): boolean => file.type.startsWith("image/");

/** 新附件条目 id（时间基 + 随机尾巴，进程内唯一即可）。 */
export const newAttachmentId = (): string =>
  `${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 8)}`;

/**
 * 正文引用约定（唯一实现）：就绪附件按类型拼 `<image path>` / `<file path>`，
 * 换行连接；空列表 → 空串。发送侧负责把 markup 拼进正文（`text\nmarkup`）。
 */
export const attachmentMarkup = (attachments: PendingAttachment[]): string =>
  attachments
    .filter((a) => a.status === "ready" && a.path)
    .map((a) => (a.isImage ? `<image path="${a.path}">` : `<file path="${a.path}">`))
    .join("\n");
