// 统一报告入口（S5 · R5 前身）：一个可接线的事件/回调，R5（诊断上报）实现时经
// `setReportIssueSink` 接上真实上报。在 R5 落地前 sink 恒为 null——界面如实显示
// 「诊断服务未开启」并提供复制摘要出口，**绝不伪造上传成功**。
//
// **安全引用纪律（设计 §11.4）**：payload 只含安全引用（工具标识、任务/动作 ID、
// 错误码枚举、适配状态机器词摘要），**不含**消息正文、日志、路径、凭据或本地业务
// 敏感 ID；`reportIssueSummary` 是给用户复制的一行摘要，同样只由这些引用拼成。

/** 一次报告的安全引用（字段全部是引用或机器词，禁止塞入正文/日志/路径） */
export interface ReportIssueRef {
  /** 工具标识（如 "codex"） */
  tool: string;
  /** 入口位置（如 "composer_send" / "settings_adapt" / "pending_card"） */
  surface: string;
  /** 任务/会话 ID（可空——设置页入口无会话语境） */
  sessionId: string | null;
  /** 动作 ID（可空） */
  actionId: string | null;
  /** 错误码枚举（机器码，如 S2 理由码；可空） */
  errorCode: string | null;
  /** 适配状态机器词摘要（可空；见 reportIssue 适配面） */
  adaptSummary: string | null;
}

/** R5 上报出口签名。R5 注册后 `reportIssue` 即把引用交给它。 */
export type ReportIssueSink = (ref: ReportIssueRef) => void;

let sink: ReportIssueSink | null = null;

/** R5 接线点：注册上报出口；传 null = 注销（回到「诊断服务未开启」态）。 */
export function setReportIssueSink(s: ReportIssueSink | null): void {
  sink = s;
}

/** 诊断出口是否已接线（R5 注册后为 true）。 */
export function hasReportIssueSink(): boolean {
  return sink !== null;
}

/**
 * 提交一次报告。返回 false = 出口未接线（诊断服务未开启）——调用方必须如实呈现
 * 「不可用 + 复制摘要」出口，不得显示提交成功。
 */
export function reportIssue(ref: ReportIssueRef): boolean {
  if (sink === null) return false;
  sink(ref);
  return true;
}

/** 一行安全摘要（复制出口的唯一内容；只由引用与机器码拼成）。 */
export function reportIssueSummary(ref: ReportIssueRef): string {
  const parts = [
    `tool=${ref.tool}`,
    `surface=${ref.surface}`,
    ref.sessionId ? `session=${ref.sessionId}` : null,
    ref.actionId ? `action=${ref.actionId}` : null,
    ref.errorCode ? `error=${ref.errorCode}` : null,
    ref.adaptSummary ? `adapt=${ref.adaptSummary}` : null,
  ].filter((p): p is string => p !== null);
  return `tuvis-report ${parts.join(" ")}`;
}
