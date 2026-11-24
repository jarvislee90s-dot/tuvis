import type { Session } from "@/types/session";

export function dshSession(
  overrides: Omit<
    Session,
    | "agentType"
    | "gitBranch"
    | "githubUrl"
    | "lastMessage"
    | "lastMessageRole"
    | "pid"
    | "cpuUsage"
    | "jumpSupported"
    | "unread"
  > &
    Partial<
      Pick<
        Session,
        | "agentType"
        | "gitBranch"
        | "githubUrl"
        | "lastMessage"
        | "lastMessageRole"
        | "pid"
        | "cpuUsage"
        | "jumpSupported"
        | "unread"
      >
    >
): Session {
  return {
    agentType: "dsh",
    gitBranch: null,
    githubUrl: null,
    lastMessage: null,
    lastMessageRole: null,
    pid: 1,
    cpuUsage: 0,
    jumpSupported: false,
    unread: false,
    ...overrides,
  };
}

export function json(body: unknown, status = 200, headers?: HeadersInit): Response {
  return new Response(JSON.stringify(body), { status, headers });
}
