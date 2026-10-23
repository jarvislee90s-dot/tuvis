// 「在桌面端打开」可用性门（R5 Task 11 原生于 SessionDetail，2026-09-20 归档区
// 复用迁出为独立模块）。SSOT = src-tauri/src/inject/resume.rs 的 RESUME_TABLE——
// 后端查证新工具回填后**两处必须同步**（本镜像仅驱动按钮禁用态）。
// M5 文案双语化（Task 7）：禁用原因经 mobile.resume.* 键渲染期现取（中英随语言切换）。
import i18n from "@/i18n";

export const RESUME_SUPPORTED_TOOLS: ReadonlySet<string> = new Set([
  "claude",
  "codex",
  "kimi",
  "opencode",
]);

/** 禁用原因（M5 Task 7 前为中文内联常量，现经 i18n 现取；独立导出供 ArchiveDetail
 *  openFailed 插值复用）：null = 可用 */
export function resumeUnsupportedReason(): string {
  return i18n.t("mobile.resume.unsupportedTool");
}

export function resumeUnavailableReason(session: {
  projectPath?: string | null;
  agentType: string;
}): string | null {
  if (!session.projectPath || !session.projectPath.trim()) {
    return i18n.t("mobile.resume.noProjectDir");
  }
  if (!RESUME_SUPPORTED_TOOLS.has(session.agentType)) {
    return resumeUnsupportedReason();
  }
  return null;
}
