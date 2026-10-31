// 会话类型定义 — 与 Rust Session 结构（camelCase 序列化）对应

export type AgentType =
  "claude" | "codex" | "opencode" | "openclaw" | "kimi" | "workbuddy" | "zcode" | "dsh";

export type SessionStatus =
  "waiting" | "processing" | "thinking" | "compacting" | "idle" | "finished";

export type ProcessForm = "cli" | "app";

export interface Session {
  id: string;
  agentType: AgentType;
  projectName: string;
  projectPath: string;
  title: string | null;
  gitBranch: string | null;
  githubUrl: string | null;
  status: SessionStatus;
  lastMessage: string | null;
  lastMessageRole: string | null;
  /** lastMessage 是否为「子 agent 回报」触发（观察台 §四 提醒开关判定信号；
   *  后端判据层打标，claude 专属、其余工具恒 false = 未区分） */
  lastMessageSubagentReport: boolean;
  /** 「本次状态由子 agent 活动引发」打标（终审发现 C / 决策 4）：活跃子 agent 在场
   *  ∧ 最新 hook 事件 ∈ PostToolUse 族 ∧ < 30s TTL。后端判定、前端只消费布尔，
   *  永不匹配文案；桌面/移动通知入口按该字段默认静默 */
  flapFromSubagentActivity: boolean;
  lastActivityAt: string;
  pid: number;
  cpuUsage: number;
  activeSubagentCount: number;
  form: ProcessForm;
  jumpSupported: boolean;
  /** 未读标记（W4）：绿色已完成且用户未查看的持久未读卡（APP 类专用） */
  unread: boolean;
  /** 未读池首次转绿时间（epoch ms）；通知新鲜度用它，不用业务 lastActivityAt */
  unreadTurnedGreenAtMs?: number | null;
}

export interface SessionsResponse {
  sessions: Session[];
  totalCount: number;
  waitingCount: number;
}

/** 状态跃迁事件（SSE `transition` 帧的 data 载荷，M3 Task 6）— 与 Rust
 * `remote::watcher::TransitionEvent`（camelCase 序列化）逐字段对应。
 * 语义：**只含边沿**（watcher 是唯一去重点，铁律 4），消费者收到一条即处理一条，
 * 不得在客户端再做 diff/去重。key 口径同后端 diff：id 只在工具内唯一，
 * 跨工具可撞 id，故唯一键必须取 (agentType, sessionId) 二元组 */
export interface TransitionEvent {
  sessionId: string;
  agentType: AgentType;
  /** 变化前的状态（wire 字符串，与 Session.status 同一联合） */
  from: SessionStatus;
  /** 变化后的状态 */
  to: SessionStatus;
  projectName: string;
  /** 变化时刻该会话的最新消息预览（可为 null：无消息） */
  lastMessage: string | null;
  /** 「跃迁由子 agent 活动引发」打标（T4 F2b / 决策 4，取 curr 会话同名会话字段）：
   *  移动端横幅/提示音/振动入口按本字段默认静默 */
  flapFromSubagentActivity: boolean;
  /** 事件产生时刻（服务端毫秒时间戳） */
  ts: number;
}
