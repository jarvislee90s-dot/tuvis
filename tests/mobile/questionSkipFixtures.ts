import type { CodexAsyncQuestionView } from "@/mobile/codexIpcApi";
import type { GroupDisplayView } from "@/mobile/codexQuestionSkipApi";
import type { Session } from "@/types/session";

export function questionSkipViews(
  threadId: string,
  contentGeneration: string,
  sourceQuestionCount?: number
) {
  const group = { threadId, sourceTurnId: "turn", sourceItemId: "item", contentGeneration };
  const active = {
    group,
    revision: 0,
    decision: "active" as const,
    latestNativeEffect: null,
    reasonCode: null,
    ...(sourceQuestionCount === undefined ? {} : { sourceComplete: true, sourceQuestionCount }),
  } satisfies GroupDisplayView;
  const skipped = {
    ...active,
    revision: 1,
    decision: "skipped" as const,
    latestNativeEffect: "unresolved" as const,
  };
  return { group, active, skipped };
}

export const response = (body: unknown, status = 200) =>
  new Response(JSON.stringify(body), { status });

export function questionSkipDetailFixture(
  group: GroupDisplayView["group"],
  body: Pick<CodexAsyncQuestionView, "questionKey" | "title" | "options">
) {
  const question: CodexAsyncQuestionView = {
    ...body,
    contextToken: "token",
    sourceTurnId: group.sourceTurnId,
    sourceItemId: group.sourceItemId,
    questionIndex: 0,
    state: "pending",
    actionable: true,
    disabledReasonCode: null,
  };
  const session = (id: string): Session => ({
    id,
    agentType: "codex",
    projectName: "proj",
    projectPath: "/tmp/proj",
    title: "codex",
    gitBranch: null,
    githubUrl: null,
    status: "idle",
    lastMessage: null,
    lastMessageRole: null,
    lastMessageSubagentReport: false,
    flapFromSubagentActivity: false,
    lastActivityAt: "2026-10-20T00:00:00Z",
    pid: 1,
    cpuUsage: 0,
    activeSubagentCount: 0,
    form: "app",
    jumpSupported: false,
    unread: false,
  });
  const projection = (extra: Record<string, unknown> = {}) => ({
    threadId: group.threadId,
    revision: 5,
    stale: false,
    availability: "controllable",
    capabilities: {
      send: true,
      state: true,
      command: true,
      permissions: false,
      blockingInput: true,
    },
    runtimeStatus: "idle",
    contentState: "ready",
    messages: [],
    truncated: false,
    pending: [],
    asyncQuestions: [question],
    ...extra,
  });
  function detailFetch(input: RequestInfo | URL) {
    const url = String(input);
    const body = url.includes("/codex/thread")
      ? projection()
      : url.includes("/codex/queue")
        ? { actions: [], blockedReasonCode: null }
        : url.includes("/session-messages")
          ? { messages: [], truncated: false }
          : url.includes("/session-subagents")
            ? { subagents: [] }
            : url.includes("/session-files")
              ? { files: [], truncated: false }
              : url.includes("/session-queue")
                ? { items: [] }
                : url.includes("/session-send-info")
                  ? { injectable: true, channels: [], visibility: "realtime" }
                  : {};
    return response(body);
  }
  return { question, session, projection, detailFetch };
}
