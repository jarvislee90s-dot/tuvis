/**
 * Tauri API Mock for browser/Playwright environment
 *
 * When the app runs outside of Tauri WebView (e.g., in a browser via Vite dev server),
 * the @tauri-apps/api modules throw errors because __TAURI_INTERNALS__ is undefined.
 * This mock provides safe fallbacks so the UI can render for screenshot capture.
 */
// 用量域 case 体改用**同源夹具**（计划② Task 1）：与 tests/msw/tauriMocks.ts 共用
// src/lib/usage/mockFixtures.ts，双端形状从源头一致（门禁 tests/usage/usageMockParity.test.ts）
import {
  mockBillingCommit,
  mockBillingGet,
  mockBillingPreview,
  mockBillingUndo,
  mockChannelOptions,
  mockPricingCatalog,
  mockPricingGet,
  mockPricingOverrideDelete,
  mockPricingOverridesGet,
  mockPricingOverrideSet,
  mockPricingRefresh,
  mockPricingSetEnabled,
  mockProviderAliasDelete,
  mockProviderAliasesGet,
  mockProviderAliasUpsert,
  mockUsageCollect,
  mockUsageCostCsv,
  mockUsageCosts,
  mockUsageCsv,
  mockUsageDashboard,
  mockUsageDistribution,
  mockUsageIdentities,
  mockUsageMode,
  mockUsageRecords,
  mockUsageSettings,
  mockUsageTrend,
  mockModelDisplayNameSet,
  mockObservedEntriesGet,
  mockPricingAutoMatch,
  mockPricingCatalogSearch,
  mockPricingIdentitySet,
  mockPricingQuoteGet,
  mockTpsHeartbeat,
  mockTpsInstallApply,
  mockTpsInstallPreview,
  mockTpsPointsResult,
  mockTpsRefresh,
  mockTpsSnapshotResult,
  mockTpsUninstall,
  mockSpeedDashboard,
} from "@/lib/usage/mockFixtures";
import type { SpeedDashboardQuery } from "@/types/speed";
import type { TpsInstallChoice, TpsSelection, TpsViewRange } from "@/types/tps";
import type {
  UsageFilters,
  UsageGroupBy,
  UsageRange,
  UsageRecordsGroupBy,
  UsageSettingsPatch,
} from "@/types/usage";
import type {
  BillingCommit,
  BillingConfig,
  DistributionGroup,
  PriceOverrideRequest,
  ProviderAliasRule,
  TrendSelection,
  ModelDisplayNameRequest,
  PricingIdentityRequest,
} from "@/types/usageV2";

// Check if running in Tauri
const isTauri = typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

interface TauriInternalsMock {
  metadata: {
    currentWindow: { label: string };
    currentWebview: { label: string };
  };
  invoke: (cmd: string, args?: Record<string, unknown>) => Promise<unknown>;
  convertFileSrc: (path: string) => string;
  transformCallback: (callback: (payload: unknown) => void, once?: boolean) => string;
  unregisterCallback: (id: string) => void;
  postMessage: () => void;
}

if (!isTauri) {
  console.log("[tauri-mock] Running outside Tauri WebView — injecting API mocks");

  // 预设组 v2 fixture（1 通用 + 1 tool 私有；extensionId 对应上方资源样例的 id）
  const mockPresetsV2 = [
    {
      id: "preset-1",
      name: "Full Stack Dev",
      description: "全栈开发常用组合：需求梳理 + 系统化调试 + 文档检索",
      scope: "universal",
      boundTool: null,
      items: [
        { extensionId: "1", kind: "skill", extensionName: "brainstorming" },
        { extensionId: "2", kind: "skill", extensionName: "systematic-debugging" },
        { extensionId: "13", kind: "mcp", extensionName: "context7" },
        { extensionId: "14", kind: "mcp", extensionName: "firecrawl-mcp" },
      ],
    },
    {
      id: "preset-2",
      name: "Code Review",
      description: "Claude 专属中文审查流（工具私有预设样例）",
      scope: "tool",
      boundTool: "claude",
      items: [
        { extensionId: "10", kind: "skill", extensionName: "chinese-code-review" },
        { extensionId: "11", kind: "skill", extensionName: "chinese-commit-conventions" },
      ],
    },
  ];

  // 一致性体检 fixture（spec §13，T15）：含三类异常样例（漂移 L1-L4 / 不变量 / 残留暂存）+ 空健康。
  // 参数化：localStorage["mam-mock-health"] = "empty" 切空健康，便于目验折叠态（T19 手册）
  const mockLedgerDrift = [
    {
      toolId: "claude",
      kind: "L1",
      extensionId: "skill-brainstorming",
      path: "/Users/jarvis/.claude/skills/brainstorming",
    },
    {
      toolId: "claude",
      kind: "L3",
      extensionId: "skill-systematic-debugging",
      path: "/Users/jarvis/.claude/skills/systematic-debugging",
    },
    {
      toolId: "codex",
      kind: "L2",
      extensionId: "skill-chinese-code-review",
      path: "/Users/jarvis/.codex/skills/chinese-code-review",
    },
    {
      toolId: "codex",
      kind: "L4",
      extensionId: "skill-using-superpowers",
      path: "/Users/jarvis/.codex/skills/using-superpowers",
    },
  ];
  const mockPresetHealthIssues = {
    invariants: ["PRESET_SNAPSHOT_ORPHAN:codex: codex：快照存在但无激活预设"],
    stashPending: [
      {
        id: 1,
        toolId: "claude",
        skillName: "legacy-native-skill",
        stashedPath: "/Users/jarvis/.tuvis/stash/claude/skills/legacy-native-skill",
        originalPath: "/Users/jarvis/.claude/skills/legacy-native-skill",
        createdAt: new Date().toISOString(),
        restoredAt: null,
      },
    ],
    drift: mockLedgerDrift,
    // 空目录（wave33 Item D，与 tests/msw/tauriMocks.ts 同构）：mam 仓库 + 工具目录各一
    emptyDirs: [
      { owner: "mam", path: "/Users/jarvis/.tuvis/skills/empty-suite-dir" },
      { owner: "tool:claude", path: "/Users/jarvis/.claude/skills/empty-dir" },
    ],
  };
  const mockPresetHealthEmpty = { invariants: [], stashPending: [], drift: [], emptyDirs: [] };

  // frontmatter 存量建议 fixture（Task 17，spec §6）：extensionId 不在
  // list_resource_bindings 样例（仅 "13" 有绑定）→ 可出建议。参数化：
  // localStorage["mam-mock-fm"] = "empty" 切空建议（目验体检卡片无建议段）
  const mockFmSuggestion = {
    extensionId: "skill-systematic-debugging",
    tools: ["claude", "codex"],
  };
  const fmSuggestionOrDefault = () =>
    localStorage.getItem("mam-mock-fm") === "empty" ? null : mockFmSuggestion;

  // app_run_id（物理坠落按应用重启归位，2026-10-13）：浏览器预览下一次页面加载
  // 即"一轮运行"——mock 安装时取一次常量，整页生命周期内恒定，刷新即变
  const mockAppRunId = `mock-run-${Date.now()}`;

  // —— 事件回放（Task 7 新增）——
  // transformCallback 真的把回调存下来（此前只发号不存，事件无从回放）；
  // `plugin:event|listen` 把事件名 → 回调号登记进 mockEventListeners。
  // `workspace_request_navigation` 先存后"emit"（逐个回放注册的回调），与 Rust
  // 命令同语义——浏览器预览里点宠物/标题栏入口也能真实驱动四页导航 hook。
  type MockEventMessage = { event: string; id: number; payload: unknown };
  const mockEventCallbacks = new Map<string, (msg: MockEventMessage) => void>();
  const mockEventListeners = new Map<string, Set<string>>();
  let mockCallbackSeq = 0;
  let mockEventIdSeq = 1000;
  const dispatchMockEvent = (event: string, payload: unknown) => {
    const handlers = mockEventListeners.get(event);
    if (!handlers) return;
    for (const handlerId of handlers) {
      mockEventCallbacks.get(handlerId)?.({ event, id: mockEventIdSeq++, payload });
    }
  };

  // —— 四页工作区导航队列（Task 7）：与 Rust NavigationQueue 同语义的 in-memory 版 ——
  // request 换新 revision 并替换 pending；ack 仅在 revision 命中 pending 时清队。
  let mockNavRevision = 0;
  let mockNavPending: { revision: number; tab: string } | null = null;

  // Mock __TAURI_INTERNALS__
  (window as unknown as { __TAURI_INTERNALS__: TauriInternalsMock }).__TAURI_INTERNALS__ = {
    metadata: {
      currentWindow: { label: "main" },
      currentWebview: { label: "main" },
    },
    invoke: (cmd: string, args?: Record<string, unknown>) => {
      console.log(`[tauri-mock] invoke("${cmd}", ${JSON.stringify(args)}) — returning mock data`);
      return mockInvoke(cmd, args);
    },
    convertFileSrc: (path: string) => path,
    transformCallback: (callback, once) => {
      const id = `mock-cb-${++mockCallbackSeq}`;
      // 与真实 Tauri 同构：回调原样登记，事件到达时以 `{event, id, payload}` 调用；
      // 其他用途（Channel 等）无 dispatch 方，登记不副作用
      mockEventCallbacks.set(id, (msg) => {
        (callback as (msg: MockEventMessage) => void)(msg);
        if (once) mockEventCallbacks.delete(id);
      });
      return id;
    },
    unregisterCallback: (id) => {
      mockEventCallbacks.delete(id);
    },
    postMessage: () => {},
  };

  // Mock invoke responses for different commands
  function mockInvoke(cmd: string, args?: Record<string, unknown>): Promise<unknown> {
    switch (cmd) {
      case "app_run_id":
        return Promise.resolve(mockAppRunId);

      // P2-2：settings 页窗口关闭拦截改用 getCurrentWindow().onCloseRequested。
      // 该 API 依赖上方 __TAURI_INTERNALS__ 的 metadata/transformCallback/postMessage
      //（本 mock 已提供），浏览器渲染下注册为 no-op 监听、永不触发，页面不崩溃——
      // 无需额外 mock 模块；真实关闭拦截仅在 Tauri WebView 内生效
      case "list_enabled_tools":
        return Promise.resolve([]);
      case "get_all_sessions":
        return Promise.resolve({
          sessions: [
            {
              id: "mock-claude-1",
              agentType: "claude",
              projectName: "tuvis",
              projectPath: "/Users/jarvis/Documents/tuvis",
              title: "实现资源看板重构",
              gitBranch: "feat/resource-dashboard-redesign",
              githubUrl: "https://github.com/user/tuvis",
              status: "processing",
              lastMessage: "继续实现 Layer 2 目录管理...",
              lastMessageRole: "assistant",
              lastMessageSubagentReport: false,
              lastActivityAt: new Date().toISOString(),
              pid: 12345,
              cpuUsage: 2.3,
              activeSubagentCount: 1,
              form: "cli",
              jumpSupported: true,
            },
            {
              id: "mock-codex-1",
              agentType: "codex",
              projectName: "tuvis",
              projectPath: "/Users/jarvis/Documents/tuvis",
              title: "修复编译错误",
              gitBranch: "feat/resource-dashboard-redesign",
              githubUrl: null,
              status: "idle",
              lastMessage: "编译成功，0 errors",
              lastMessageRole: "assistant",
              lastMessageSubagentReport: false,
              lastActivityAt: new Date(Date.now() - 60000).toISOString(),
              pid: 12346,
              cpuUsage: 0.1,
              activeSubagentCount: 0,
              form: "cli",
              jumpSupported: true,
            },
            {
              id: "mock-openclaw-1",
              agentType: "codex",
              projectName: "OpenClaw",
              projectPath: "/Users/jarvis/.openclaw",
              title: "SkyComputing Agent",
              gitBranch: "main",
              githubUrl: null,
              status: "waiting",
              lastMessage: null,
              lastMessageRole: null,
              lastMessageSubagentReport: false,
              lastActivityAt: new Date(Date.now() - 120000).toISOString(),
              pid: 12347,
              cpuUsage: 0.0,
              activeSubagentCount: 0,
              form: "app",
              jumpSupported: false,
            },
            // 实机反馈轮 2026-10-27 #1 截图夹具：长项目名 + 长分支名卡——
            // 验证首行「项目名截断兜底、分支先让路」与会话名第二行形态（仅浏览器 mock）
            {
              id: "mock-kimi-long-1",
              agentType: "kimi",
              projectName: "MultiAgents-Manager",
              projectPath: "/Users/jarvis/Documents/MultiAgents-Manager",
              title: "（3）使用技能【win-console-inject-probe】探测注入规格",
              gitBranch: "feat/desktop-workspace-speed-v2-session-card-two-row-header",
              githubUrl: null,
              status: "processing",
              lastMessage: "正在执行 M6R 验证脚本套件，收集屏读证据...",
              lastMessageRole: "assistant",
              lastMessageSubagentReport: false,
              lastActivityAt: new Date().toISOString(),
              pid: 12348,
              cpuUsage: 1.7,
              activeSubagentCount: 2,
              form: "cli",
              jumpSupported: true,
            },
          ],
          totalCount: 4,
          waitingCount: 1,
        });

      case "list_extensions_with_assignments":
        return Promise.resolve([
          {
            id: "1",
            kind: "skill",
            name: "brainstorming",
            description: "将想法转化为设计",
            sourcePath: "~/.tuvis/skills/brainstorming",
            sourceTool: "claude",
            suite: "superpowers",
            tags: null,
            // 唯一 isNative:true 样例（sourceTool 已设）：浏览器 mock 下可目验编辑弹窗「原生技能」分组（T7）
            isNative: true,
            assignments: [
              { agentToolId: "claude", enabled: true, linkStatus: "linked" },
              { agentToolId: "codex", enabled: true, linkStatus: "linked" },
              { agentToolId: "openclaw", enabled: true, linkStatus: "linked" },
            ],
          },
          {
            id: "2",
            kind: "skill",
            name: "systematic-debugging",
            description: "系统化调试",
            sourcePath: "~/.tuvis/skills/systematic-debugging",
            sourceTool: "claude",
            suite: "superpowers",
            tags: null,
            isNative: false,
            assignments: [
              { agentToolId: "claude", enabled: true, linkStatus: "linked" },
              { agentToolId: "codex", enabled: true, linkStatus: "linked" },
            ],
          },
          {
            id: "3",
            kind: "skill",
            name: "test-driven-development",
            description: "测试驱动开发",
            sourcePath: "~/.tuvis/skills/test-driven-development",
            sourceTool: "claude",
            suite: "superpowers",
            tags: null,
            isNative: false,
            assignments: [{ agentToolId: "claude", enabled: true, linkStatus: "linked" }],
          },
          {
            id: "4",
            kind: "skill",
            name: "writing-plans",
            description: "编写实现计划",
            sourcePath: "~/.tuvis/skills/writing-plans",
            sourceTool: "claude",
            suite: "superpowers",
            tags: null,
            isNative: false,
            assignments: [{ agentToolId: "claude", enabled: false, linkStatus: "unlinked" }],
          },
          {
            id: "5",
            kind: "skill",
            name: "verification-before-completion",
            description: "完成前验证",
            sourcePath: "~/.tuvis/skills/verification-before-completion",
            sourceTool: "claude",
            suite: "superpowers",
            tags: null,
            isNative: false,
            assignments: [{ agentToolId: "claude", enabled: true, linkStatus: "linked" }],
          },
          {
            id: "6",
            kind: "skill",
            name: "requesting-code-review",
            description: "请求代码审查",
            sourcePath: "~/.tuvis/skills/requesting-code-review",
            sourceTool: "claude",
            suite: "superpowers",
            tags: null,
            isNative: false,
            assignments: [
              { agentToolId: "claude", enabled: true, linkStatus: "linked" },
              { agentToolId: "codex", enabled: true, linkStatus: "linked" },
            ],
          },
          {
            id: "7",
            kind: "skill",
            name: "subagent-driven-development",
            description: "子智能体驱动开发",
            sourcePath: "~/.tuvis/skills/subagent-driven-development",
            sourceTool: "claude",
            suite: "superpowers",
            tags: null,
            isNative: false,
            assignments: [{ agentToolId: "claude", enabled: true, linkStatus: "linked" }],
          },
          {
            id: "8",
            kind: "skill",
            name: "using-superpowers",
            description: "使用技能系统",
            sourcePath: "~/.tuvis/skills/using-superpowers",
            sourceTool: "claude",
            suite: "superpowers",
            tags: null,
            isNative: false,
            assignments: [
              { agentToolId: "claude", enabled: true, linkStatus: "linked" },
              { agentToolId: "codex", enabled: true, linkStatus: "linked" },
              { agentToolId: "openclaw", enabled: true, linkStatus: "linked" },
            ],
          },
          {
            id: "9",
            kind: "skill",
            name: "using-git-worktrees",
            description: "使用 Git 工作树",
            sourcePath: "~/.tuvis/skills/using-git-worktrees",
            sourceTool: "claude",
            suite: "superpowers",
            tags: null,
            isNative: false,
            assignments: [
              { agentToolId: "claude", enabled: true, linkStatus: "linked" },
              { agentToolId: "codex", enabled: true, linkStatus: "linked" },
            ],
          },
          {
            id: "10",
            kind: "skill",
            name: "chinese-code-review",
            description: "中文代码审查",
            sourcePath: "~/.tuvis/skills/chinese-code-review",
            sourceTool: "claude",
            suite: "superpowers",
            tags: null,
            isNative: false,
            assignments: [{ agentToolId: "claude", enabled: true, linkStatus: "linked" }],
          },
          {
            id: "11",
            kind: "skill",
            name: "chinese-commit-conventions",
            description: "中文提交规范",
            sourcePath: "~/.tuvis/skills/chinese-commit-conventions",
            sourceTool: "claude",
            suite: "superpowers",
            tags: null,
            isNative: false,
            assignments: [{ agentToolId: "claude", enabled: true, linkStatus: "linked" }],
          },
          {
            id: "12",
            kind: "skill",
            name: "mcp-builder",
            description: "构建 MCP 服务器",
            sourcePath: "~/.tuvis/skills/mcp-builder",
            sourceTool: "claude",
            suite: "superpowers",
            tags: null,
            isNative: false,
            assignments: [
              { agentToolId: "claude", enabled: false, linkStatus: "unlinked" },
              { agentToolId: "codex", enabled: false, linkStatus: "unlinked" },
            ],
          },
          {
            id: "13",
            kind: "mcp",
            name: "context7",
            description: "Library documentation lookup",
            sourcePath: "~/.tuvis/mcp/context7",
            sourceTool: "claude",
            suite: null,
            tags: null,
            isNative: false,
            assignments: [
              { agentToolId: "claude", enabled: true, linkStatus: "linked" },
              { agentToolId: "codex", enabled: true, linkStatus: "linked" },
            ],
          },
          {
            id: "14",
            kind: "mcp",
            name: "firecrawl-mcp",
            description: "Web search and scraping",
            sourcePath: "~/.tuvis/mcp/firecrawl",
            sourceTool: "claude",
            suite: null,
            tags: null,
            isNative: false,
            assignments: [{ agentToolId: "claude", enabled: true, linkStatus: "linked" }],
          },
          {
            id: "15",
            kind: "mcp",
            name: "playwright",
            description: "Browser automation",
            sourcePath: "~/.tuvis/mcp/playwright",
            sourceTool: "claude",
            suite: null,
            tags: null,
            isNative: false,
            assignments: [{ agentToolId: "claude", enabled: true, linkStatus: "linked" }],
          },
          {
            id: "16",
            kind: "mcp",
            name: "supabase",
            description: "Database management",
            sourcePath: "~/.tuvis/mcp/supabase",
            sourceTool: "claude",
            suite: null,
            tags: null,
            isNative: false,
            assignments: [{ agentToolId: "claude", enabled: false, linkStatus: "unlinked" }],
          },
          {
            id: "17",
            kind: "plugin",
            name: "statusline-setup",
            description: "Configure status line",
            sourcePath: "~/.tuvis/plugins/statusline-setup",
            sourceTool: "claude",
            suite: "superpowers",
            tags: null,
            isNative: false,
            assignments: [{ agentToolId: "claude", enabled: true, linkStatus: "linked" }],
          },
          {
            id: "18",
            kind: "plugin",
            name: "file-secretary",
            description: "Personal file management",
            sourcePath: "~/.tuvis/plugins/file-secretary",
            sourceTool: "claude",
            suite: null,
            tags: null,
            isNative: false,
            assignments: [{ agentToolId: "claude", enabled: false, linkStatus: "unlinked" }],
          },
        ]);

      // 预设组 v2 样例（1 通用 + 1 tool 私有）：与 Rust PresetRecord（serde camelCase）同构，
      // 并与 tests/msw/tauriMocks.ts 的 mockPresets 保持形状一致（mock parity 收口门禁）
      case "list_presets":
        return Promise.resolve(mockPresetsV2);

      case "get_preset":
        return Promise.resolve(
          mockPresetsV2.find((p) => p.id === (args?.presetId as string)) ?? null
        );

      case "get_active_preset":
        return Promise.resolve(args?.toolId === "claude" ? "preset-1" : null);

      case "list_active_presets":
        return Promise.resolve([{ toolId: "claude", presetId: "preset-1" }]);

      // (extensionId, kind, origin) 三元组，origin = "mam" | "native"（scan_tool_state 口径）
      case "get_tool_active_resources":
        return Promise.resolve([
          ["1", "skill", "mam"],
          ["2", "skill", "mam"],
          ["13", "mcp", "mam"],
          ["skill-brainstorming", "skill", "native"],
        ]);

      case "list_resource_bindings":
        return Promise.resolve([
          {
            extensionId: "13",
            exclusiveTools: "claude,codex",
            reason: "需要 Node 运行时，仅绑定前端工具链",
            updatedAt: new Date().toISOString(),
          },
        ]);

      case "list_tool_residents":
        return Promise.resolve(args?.toolId === "claude" ? ["17"] : []);

      // —— frontmatter 专属预填建议（Task 17，与 Rust 命令/ImportOutcome 同形）——
      // install_skill 返回 ImportOutcome{success, suggestion}；import_native_resources
      // 返回 ImportStats + suggestion；存量扫描读 fixture（空/有建议可参数化）
      case "list_frontmatter_suggestions":
        return Promise.resolve(
          localStorage.getItem("mam-mock-fm") === "empty" ? [] : [mockFmSuggestion]
        );
      case "install_skill":
        return Promise.resolve({ success: true, suggestion: fmSuggestionOrDefault() });
      case "import_native_resources":
        return Promise.resolve({
          imported: 1,
          newlyAdded: 1,
          skippedDup: 0,
          sourceCounts: [],
          suggestion: fmSuggestionOrDefault(),
        });

      // —— 一致性体检读命令（T15，与 tests/msw/tauriMocks.ts 形状一致）——
      case "get_preset_health":
        return Promise.resolve(
          localStorage.getItem("mam-mock-health") === "empty"
            ? mockPresetHealthEmpty
            : mockPresetHealthIssues
        );
      case "scan_ledger_drift":
        return Promise.resolve(mockLedgerDrift);
      // 空目录扫描（wave33 Item D）：读 fixture（与 get_preset_health 的 emptyDirs 同源）
      case "scan_empty_dirs":
        return Promise.resolve(mockPresetHealthIssues.emptyDirs);

      case "preview_apply_preset":
        return Promise.resolve({
          toEnable: ["1", "2"],
          filtered: [],
          toDisable: [],
          toStash: [],
          residentExempt: [],
        });

      // T6 开关 fixture：写命令须返回与真实命令同形的对象（undefined 会让 r.successCount / rr.restoredMam.length 崩）
      case "apply_preset":
        // PresetApplyResult（serde camelCase）：计数与上方 preview_apply_preset 的 toEnable 对齐
        return Promise.resolve({
          successCount: 2,
          failures: [],
          conflicts: [],
          stashed: [],
          disabled: [],
          restoredNative: [],
        });
      case "restore_preset":
        return Promise.resolve({ restoredMam: [], restoredNative: [], conflicts: [] });

      // 预设组写命令（浏览器 mock 一律视为成功；与 tests/msw/tauriMocks.ts 写分组对齐）。
      // deactivate_preset 前端路径已退役（T8），case 移除；子 Agent 变体保留
      case "create_preset":
      case "update_preset":
      case "delete_preset":
      case "apply_preset_to_subagent":
      case "deactivate_preset_from_subagent":
      case "set_resource_binding":
      case "delete_resource_binding":
      case "set_tool_resident":
        return Promise.resolve(undefined);

      // 暂存回移（T15 体检卡片 ③）：真实命令返回 Result<(), String>，mock 视为成功
      case "restore_stash_entry":
        return Promise.resolve(undefined);

      // 空目录清理（wave33 Item D）：真实命令返回 Result<usize>，mock 删无可删返回 0
      case "clean_empty_dirs":
        return Promise.resolve(0);

      // 快捷跳转（wave33 Item B）：homeDir()（@tauri-apps/api/path）在浏览器 mock
      // 下也走 __TAURI_INTERNALS__.invoke——提供固定 home，reveal_dir 本 mock 缺省 no-op
      case "plugin:path|resolve_directory":
        return Promise.resolve("/Users/jarvis");

      // reveal_dir（wave33 Item B）：浏览器 mock 下无法真开文件管理器，静默成功供目验
      case "reveal_dir":
        return Promise.resolve(undefined);

      // 体检对账写命令（T15）：须返回与 Rust ReconcileOutcome 同形对象
      //（undefined 会让 o.fixed / o.needsManual 读崩）；extensionId/toolId 结构化回带
      //（终审 Minor #4 起前端批量行映射靠结构化字段，message 只供人读不承载可解析格式）
      case "reconcile_item": {
        const item = args?.item as { extensionId?: string; toolId?: string } | undefined;
        return Promise.resolve({
          fixed: true,
          needsManual: false,
          extensionId: item?.extensionId ?? "",
          toolId: item?.toolId ?? "",
          message: "mock: 已按账本重建链接",
        });
      }
      case "reconcile_tool_batch": {
        const batchToolId = args?.toolId as string;
        return Promise.resolve(
          mockLedgerDrift
            .filter((d) => d.toolId === batchToolId)
            .map((d) => ({
              fixed: true,
              needsManual: false,
              extensionId: d.extensionId,
              toolId: batchToolId,
              message: "mock 已按账本重建链接",
            }))
        );
      }

      // 托盘统一重建（T16）：fire-and-forget，无返回值消费，显式 no-op 以闭合双 mock parity
      case "refresh_tray":
        return Promise.resolve(undefined);

      // R5 一键 resume（M6R–M9R Task 11）：浏览器 mock 下无法真开终端，视为成功
      //（按钮态目验链路：成功 toast「正在电脑上打开终端…」）。评审 M4 失败开关：
      // localStorage["mam-mock-session-open"] = "fail" → 拒绝（桌面失败 toast 目验）
      case "session_open":
        return localStorage.getItem("mam-mock-session-open") === "fail"
          ? Promise.reject("终端启动失败（mock 注入）")
          : Promise.resolve(undefined);

      // —— S5：Codex 自动适配（设置页「自动适配」卡的目验链路）——
      // codex_adapt_status = CodexAdaptStatus DTO（S1 登记，形状见
      // src/lib/api/remote.ts 的同名 interface）；toggle 恒成功（真实命令写 KV + 审计）。
      case "codex_adapt_status":
        return Promise.resolve({
          autoAdaptEnabled: true,
          autoAdaptExplicit: null,
          platform: "mac",
          desktopBuild: "0.55.0",
          probeKey: "mac|0.55.0|cli-mock|e1",
          connectionEpoch: 3,
          attemptable: { send: true, state: true },
          evidenced: { state: true, send: false, schema: false },
          schemaEvidence: null,
          lastFailure: null,
          probeStatus: { state: "idle", stage: null, reasonCode: null },
        });

      case "remote_toggle_codex_auto_adapt":
        return Promise.resolve(undefined);

      case "detect_tools":
        return Promise.resolve([
          { id: "claude", name: "Claude Code", available: true, path: "/usr/local/bin/claude" },
          { id: "codex", name: "Codex CLI", available: true, path: "/usr/local/bin/codex" },
          { id: "opencode", name: "OpenCode", available: false, path: "" },
          { id: "openclaw", name: "OpenClaw", available: true, path: "/usr/local/bin/openclaw" },
          { id: "kimi", name: "Kimi Code", available: false, path: "" },
          // 债修复（ZCode 接入轮）：原 mock 缺 workbuddy 行（与后端 6 工具不对齐）
          { id: "workbuddy", name: "WorkBuddy", available: true, path: "" },
          { id: "zcode", name: "ZCode", available: true, path: "" },
        ]);

      case "list_sub_agents":
        return Promise.resolve([
          {
            id: "sub-1",
            name: "frontend-dev",
            tool_id: "claude",
            skills: ["brainstorming", "test-driven-development"],
          },
          { id: "sub-2", name: "backend-dev", tool_id: "claude", skills: ["systematic-debugging"] },
        ]);

      // #66：SubAgentPresetActions 入口复活后浏览器/Playwright 渲染会真实 invoke——
      // 与 Rust SubagentDetection（serde camelCase）同构；空态走「支持但没有」分支
      case "detect_subagents":
        return Promise.resolve({ supported: true, agents: [] });

      // 写审计样例（M7 W5 桌面查看入口）：与 Rust AuditRow（serde camelCase）同构，
      // 最新在前；无 device_id 字段（与后端对外载荷一致，设备标识不外泄）
      case "inject_list_audit":
        return Promise.resolve({
          items: [
            {
              ts: 1758132000000,
              deviceName: "JARVIS 的 iPhone",
              agentType: "claude",
              sessionId: "sess-abc-1",
              channel: "tmux",
              action: "send",
              summary: "修复登录页空指针 [mobile 测试机]",
              result: "ok",
            },
            {
              ts: 1758128400000,
              deviceName: "iPad",
              agentType: "codex",
              sessionId: "sess-def-2",
              channel: "tmux",
              action: "queue",
              summary: "跑一遍回归测试 [mobile 测试机]",
              result: "ok",
            },
            {
              ts: 1758124800000,
              deviceName: "Desktop-A",
              agentType: "claude",
              sessionId: "sess-ghi-3",
              channel: "tmux",
              action: "retract",
              summary: "撤回上条消息 [mobile 测试机]",
              result: "ok",
            },
          ],
        });

      case "get_setting":
        if (args?.key === "notifications_enabled") return Promise.resolve(true);
        if (args?.key === "notify_subagent_report") return Promise.resolve(true);
        // F2b 静默开关：mock 缺省 true=静默（与真实端缺省语义一致）
        if (args?.key === "silence_subagent_activity_flap") return Promise.resolve(true);
        if (args?.key === "notification_sound") return Promise.resolve("default");
        if (args?.key === "ui_theme") return Promise.resolve(null);
        // 升级忽略版本（prerelease 渠道）：配合 localStorage["mam-mock-updater-skipped"] 手验忽略语义
        if (args?.key === "updater_skipped_version") {
          const skipped = localStorage.getItem("mam-mock-updater-skipped");
          return Promise.resolve(skipped ?? null);
        }
        return Promise.resolve(null);

      // 开机自动启动（2026-10-11）：mock 无法真写注册表/LaunchAgent——布尔镜像
      // localStorage["mam-mock-autostart"]，set/get 同 key 回读（手验闭环）
      case "get_autostart_enabled":
        return Promise.resolve(localStorage.getItem("mam-mock-autostart") === "true");

      case "set_autostart_enabled":
        localStorage.setItem("mam-mock-autostart", String(args?.enabled === true));
        return Promise.resolve(undefined);

      case "set_setting":
        // mock 无 DB：忽略版本镜像到 localStorage，get_setting 同 key 回读（手验忽略闭环）
        if (args?.key === "updater_skipped_version") {
          localStorage.setItem("mam-mock-updater-skipped", String(args?.value ?? ""));
        }
        return Promise.resolve(undefined);

      case "set_theme":
        return Promise.resolve(undefined);

      case "read_mcp_servers":
        return Promise.resolve([
          {
            name: "context7",
            command: "npx",
            args: ["-y", "@upstash/context7-mcp@latest"],
            tools: ["claude", "codex"],
          },
          {
            name: "firecrawl-mcp",
            command: "npx",
            args: ["-y", "firecrawl-mcp"],
            tools: ["claude"],
          },
          {
            name: "playwright",
            command: "npx",
            args: ["-y", "@playwright/mcp@latest"],
            tools: ["claude"],
          },
        ]);

      // 真实命令返回 NativeExtensionRecord 数组（serde camelCase，resource.rs）
      case "scan_native_resources":
        return Promise.resolve([
          {
            id: "skill-brainstorming",
            kind: "skill",
            name: "brainstorming",
            sourcePath: "/Users/jarvis/.claude/skills/brainstorming",
            sourceTool: "claude",
            description: null,
            detectedAt: new Date().toISOString(),
            imported: false,
          },
          {
            id: "skill-systematic-debugging",
            kind: "skill",
            name: "systematic-debugging",
            sourcePath: "/Users/jarvis/.claude/skills/systematic-debugging",
            sourceTool: "claude",
            description: null,
            detectedAt: new Date().toISOString(),
            imported: false,
          },
        ]);

      // ApplyResult 形状（tool_settings.rs，camelCase）；null 会让设置页
      // result.rebuildFailed.length 抛 TypeError（issue #36 review Minor）
      case "update_tool_settings":
        return Promise.resolve({
          restored: [],
          restoredMcps: [],
          rebuildFailed: [],
          skippedKept: [],
          skippedLost: [],
        });

      // 工具管理行（ToolSetting，camelCase）：七工具全量行，浏览器渲染下
      // 设置页「工具管理」分区可出数据
      case "get_tool_settings":
        return Promise.resolve([
          { toolId: "claude", name: "Claude Code", enabled: true, installed: true, managed: true },
          { toolId: "codex", name: "Codex CLI", enabled: true, installed: true, managed: true },
          { toolId: "workbuddy", name: "WorkBuddy", enabled: true, installed: true, managed: true },
          { toolId: "kimi", name: "Kimi Code", enabled: true, installed: true, managed: true },
          { toolId: "opencode", name: "OpenCode", enabled: true, installed: true, managed: false },
          { toolId: "openclaw", name: "OpenClaw", enabled: true, installed: false, managed: false },
          { toolId: "zcode", name: "ZCode", enabled: true, installed: true, managed: false },
        ]);

      case "list_repo_skills":
        return Promise.resolve([
          { name: "brainstorming", path: "/Users/jarvis/.tuvis/skills/brainstorming" },
          {
            name: "systematic-debugging",
            path: "/Users/jarvis/.tuvis/skills/systematic-debugging",
          },
        ]);

      // CompatibilityReport（serde camelCase）：条目为 {id, name, kind} / {id, name, kind, reason}
      case "check_preset_compatibility":
        return Promise.resolve({
          compatible: [
            { id: "1", name: "brainstorming", kind: "skill" },
            { id: "13", name: "context7", kind: "mcp" },
          ],
          incompatible: [
            { id: "16", name: "supabase", kind: "mcp", reason: "Not installed for this tool" },
          ],
        });

      // 遗留 codex 技能链接检测/迁移（spec §4.3）：浏览器模式视为无遗留、零报告
      case "detect_legacy_agents_links":
        return Promise.resolve([]);
      case "migrate_legacy_agents_links":
        return Promise.resolve([]);

      case "capture_window_screenshot":
        return Promise.resolve({ success: true, path: "/tmp/mock-screenshot.png" });

      case "list_screenshots":
        return Promise.resolve([]);

      // Event/Notification plugin commands — return safe mock values
      case "plugin:event|listen":
      case "plugin:event|once": {
        // Task 7：登记 事件名 → 回调号，供 workspace_request_navigation 回放事件。
        // 返回数值 eventId（与真实命令同形；旧 mock 返回 unlisten 函数是错形，无人消费）
        const listenArgs = args as { event?: string; handler?: unknown } | undefined;
        if (listenArgs?.event) {
          const handlerId = String(listenArgs.handler ?? "");
          const set = mockEventListeners.get(listenArgs.event) ?? new Set<string>();
          set.add(handlerId);
          mockEventListeners.set(listenArgs.event, set);
        }
        return Promise.resolve(mockEventIdSeq++);
      }
      case "plugin:event|unlisten": {
        const unlistenArgs = args as { event?: string; eventId?: unknown } | undefined;
        if (unlistenArgs?.event) {
          mockEventListeners.get(unlistenArgs.event)?.delete(String(unlistenArgs.eventId ?? ""));
        }
        return Promise.resolve(undefined);
      }
      case "plugin:notification|register_listener":
      case "plugin:notification|register_action_types":
        return Promise.resolve(() => {}); // unlisten function

      case "plugin:window|is_maximized":
        return Promise.resolve(false);

      case "plugin:window|show":
        return Promise.resolve(undefined);

      // —— 升级检查（prerelease 渠道）：浏览器渲染用 Rust GitHub 发现层的 mock 形状。
      // localStorage["mam-mock-updater"] = "available" 模拟有更新（示例 note 含
      // markdown 元素，便于目验渲染与限高滚动）；默认 up-to-date（弹窗/徽标不出现）。
      case "check_for_github_update": {
        if (localStorage.getItem("mam-mock-updater") === "available") {
          return Promise.resolve({
            status: "available",
            currentVersion: "0.4.1",
            update: {
              version: "0.5.0-beta.1",
              prerelease: true,
              notes:
                "## v0.5.0-beta.1 更新\n\n> 预发布：远程操控试验性开放。\n\n- **手机远程操控**：发消息/排队/撤回\n- **远程审批与问答**：屏读为准实时同步\n\n| 功能 | 状态 |\n| --- | --- |\n| 发消息 | ✅ |\n| 审批 | ✅ |\n",
              htmlUrl: "https://github.com/jarvislee90s-dot/tuvis/releases/tag/v0.5.0-beta.1",
              publishedAt: "2026-09-23T15:18:19Z",
              tag: "v0.5.0-beta.1",
              latestJsonUrl:
                "https://github.com/jarvislee90s-dot/tuvis/releases/download/v0.5.0-beta.1/latest.json",
            },
          });
        }
        return Promise.resolve({
          status: "up-to-date",
          currentVersion: "0.5.0-beta.1",
          latestVersion: "0.5.0-beta.1",
        });
      }
      // mock 不真装：静默成功（进度事件由真实 Rust 侧才有）
      case "install_github_update":
        return Promise.resolve(undefined);

      case "plugin:notification|is_permission_granted":
        return Promise.resolve(false);

      // M4 T4：新增 remoteOnText 第四参；2026-10-11 又并入 autostartText 第五参
      //（开机自启开关项标签）——mock 无状态托盘，多余参数忽略（无返回）
      case "update_tray_menu":
        return Promise.resolve(undefined);

      // —— 用量域（计划① Task 20 登记 case；② Task 1 换体）：case 行与位置不动，只换源——
      // 夹具收口到 `src/lib/usage/mockFixtures.ts`（与 tests/msw/tauriMocks.ts **同一构造器**，
      // 双端形状不可能漂移）。目验三态开关见 mockUsageMode：
      // localStorage["mam-mock-usage"] = "empty" → 空态（夹具内部分叉）；"error" → 用量命令 reject。
      case "usage_collect":
        return mockUsageMode() === "error"
          ? Promise.reject({ code: "usage-db-failed", detail: "mam-mock-usage=error" })
          : Promise.resolve(
              mockUsageCollect((args as { force?: boolean } | undefined)?.force ?? false)
            );
      case "usage_dashboard": {
        const range = (args as { range?: UsageRange } | undefined)?.range ?? { preset: "today" };
        const groupBy = (args as { groupBy?: UsageGroupBy } | undefined)?.groupBy ?? "tool";
        return mockUsageMode() === "error"
          ? Promise.reject({ code: "usage-db-failed", detail: "mam-mock-usage=error" })
          : Promise.resolve(mockUsageDashboard(range, groupBy));
      }
      case "usage_records": {
        const range = (args as { range?: UsageRange } | undefined)?.range ?? { preset: "today" };
        const groupBy = (args as { groupBy?: UsageRecordsGroupBy } | undefined)?.groupBy ?? "tool";
        const filters = (args as { filters?: UsageFilters } | undefined)?.filters ?? {};
        return mockUsageMode() === "error"
          ? Promise.reject({ code: "usage-db-failed", detail: "mam-mock-usage=error" })
          : Promise.resolve(mockUsageRecords(range, groupBy, filters));
      }
      case "usage_export_csv": {
        const range = (args as { range?: UsageRange } | undefined)?.range ?? { preset: "today" };
        const groupBy = (args as { groupBy?: UsageGroupBy } | undefined)?.groupBy ?? "tool";
        const filters = (args as { filters?: UsageFilters } | undefined)?.filters ?? {};
        return mockUsageMode() === "error"
          ? Promise.reject({ code: "usage-db-failed", detail: "mam-mock-usage=error" })
          : Promise.resolve(mockUsageCsv(range, groupBy, filters));
      }
      // 导出落盘（计划① Task 21，契约 §3 新增 2 条）：真实命令返回落盘绝对路径（string）；
      // mock 返回同形字符串，**必须带上请求的 name**（写死文件名会让文件名断言与分享图路径断言全红）
      case "export_save_text":
      case "export_save_bytes":
        return Promise.resolve(
          `/Users/jarvis/Downloads/${(args as { name?: string } | undefined)?.name ?? "mock-export.csv"}`
        );
      case "usage_get_settings":
        return mockUsageMode() === "error"
          ? Promise.reject({ code: "usage-db-failed", detail: "mam-mock-usage=error" })
          : Promise.resolve(mockUsageSettings());
      case "usage_set_settings": {
        const patch = ((args as { patch?: UsageSettingsPatch } | undefined)?.patch ??
          {}) as UsageSettingsPatch;
        return mockUsageMode() === "error"
          ? Promise.reject({ code: "usage-db-failed", detail: "mam-mock-usage=error" })
          : Promise.resolve(mockUsageSettings(patch));
      }

      // —— V2 身份 / Token（Task 5）：与 tests/msw/tauriMocks.ts 共用 mockFixtures ——
      case "usage_billing_get":
        return Promise.resolve(mockBillingGet());
      case "usage_billing_preview": {
        const candidate =
          (args as { candidate?: BillingConfig } | undefined)?.candidate ?? mockBillingGet();
        const range = (args as { range?: UsageRange } | undefined)?.range ?? { preset: "today" };
        return Promise.resolve(mockBillingPreview(candidate, range));
      }
      case "usage_billing_commit":
        return mockBillingCommit(
          (args as { request?: BillingCommit } | undefined)?.request ?? {
            expectedRevision: -1,
            candidate: mockBillingGet(),
          }
        );
      case "usage_billing_undo": {
        const a = args as { expectedRevision?: number; targetRevision?: number } | undefined;
        return mockBillingUndo(a?.expectedRevision ?? -1, a?.targetRevision ?? 0);
      }
      case "usage_identities": {
        const range = (args as { range?: UsageRange } | undefined)?.range ?? { preset: "today" };
        const filters = (args as { filters?: UsageFilters } | undefined)?.filters ?? {};
        return Promise.resolve(mockUsageIdentities(range, filters));
      }
      case "usage_trend": {
        const range = (args as { range?: UsageRange } | undefined)?.range ?? { preset: "today" };
        const selection = (args as { selection?: TrendSelection } | undefined)?.selection ?? {
          scope: "all",
        };
        return Promise.resolve(mockUsageTrend(range, selection));
      }
      case "usage_distribution": {
        const range = (args as { range?: UsageRange } | undefined)?.range ?? { preset: "today" };
        const groupBy = (args as { groupBy?: DistributionGroup } | undefined)?.groupBy ?? "tool";
        return Promise.resolve(mockUsageDistribution(range, groupBy));
      }
      case "usage_pricing_get":
        return mockUsageMode() === "error"
          ? Promise.reject({ code: "usage-db-failed", detail: "mam-mock-usage=error" })
          : Promise.resolve(mockPricingGet());
      case "usage_pricing_set_enabled":
        return mockUsageMode() === "error"
          ? Promise.reject({ code: "usage-db-failed", detail: "mam-mock-usage=error" })
          : Promise.resolve(
              mockPricingSetEnabled((args as { enabled?: boolean } | undefined)?.enabled ?? false)
            );
      case "usage_pricing_refresh":
        return mockUsageMode() === "error"
          ? Promise.reject({ code: "usage-db-failed", detail: "mam-mock-usage=error" })
          : Promise.resolve(mockPricingRefresh());
      case "usage_pricing_catalog": {
        const catalogArgs = args as { query?: string | null; limit?: number } | undefined;
        return mockUsageMode() === "error"
          ? Promise.reject({ code: "usage-db-failed", detail: "mam-mock-usage=error" })
          : Promise.resolve(mockPricingCatalog(catalogArgs?.query, catalogArgs?.limit));
      }
      case "usage_costs": {
        const range = (args as { range?: UsageRange } | undefined)?.range ?? { preset: "today" };
        return mockUsageMode() === "error"
          ? Promise.reject({ code: "usage-db-failed", detail: "mam-mock-usage=error" })
          : Promise.resolve(mockUsageCosts(range));
      }
      case "usage_cost_export_csv": {
        const range = (args as { range?: UsageRange } | undefined)?.range ?? { preset: "today" };
        return mockUsageMode() === "error"
          ? Promise.reject({ code: "usage-db-failed", detail: "mam-mock-usage=error" })
          : Promise.resolve(mockUsageCostCsv(range));
      }

      // —— Task 6：供应商别名 / 渠道选项 / 手动覆盖价 ——
      case "usage_provider_aliases_get":
        return Promise.resolve(mockProviderAliasesGet());
      case "usage_provider_alias_upsert": {
        const rule = (args as { rule?: ProviderAliasRule } | undefined)?.rule;
        return rule
          ? mockProviderAliasUpsert(rule)
          : Promise.reject({
              code: "usage-settings-invalid",
              detail: "provider alias id is empty",
            });
      }
      case "usage_provider_alias_delete": {
        const id = (args as { id?: string } | undefined)?.id ?? "";
        return mockProviderAliasDelete(id);
      }
      case "usage_channel_options": {
        const range = (args as { range?: UsageRange } | undefined)?.range ?? { preset: "today" };
        return Promise.resolve(mockChannelOptions(range));
      }
      case "usage_pricing_overrides_get":
        return Promise.resolve(mockPricingOverridesGet());
      case "usage_pricing_override_set": {
        const request = (args as { request?: PriceOverrideRequest } | undefined)?.request;
        return request
          ? mockPricingOverrideSet(request)
          : Promise.reject({
              code: "usage-settings-invalid",
              detail: "price override normalizedModelId is empty",
            });
      }
      case "usage_pricing_override_delete": {
        const normalizedModelId =
          (args as { normalizedModelId?: string } | undefined)?.normalizedModelId ?? "";
        return mockPricingOverrideDelete(normalizedModelId);
      }

      // —— 2026-10-20 计划 Task 5：观察条目 / 模型条目配置 / 精选价目目录 ——
      case "usage_observed_entries_get":
        return mockUsageMode() === "error"
          ? Promise.reject({ code: "usage-db-failed", detail: "mam-mock-usage=error" })
          : Promise.resolve(mockObservedEntriesGet());
      case "usage_model_display_name_set": {
        const displayRequest = (args as { request?: ModelDisplayNameRequest } | undefined)?.request;
        return displayRequest
          ? mockModelDisplayNameSet(displayRequest)
          : Promise.reject({
              code: "usage-settings-invalid",
              detail: "model config identity is empty",
            });
      }
      case "usage_pricing_identity_set": {
        const identityRequest = (args as { request?: PricingIdentityRequest } | undefined)?.request;
        return identityRequest
          ? mockPricingIdentitySet(identityRequest)
          : Promise.reject({
              code: "usage-settings-invalid",
              detail: "model config identity is empty",
            });
      }
      case "usage_pricing_catalog_search": {
        const searchArgs = args as
          { query?: string | null; limit?: number; scope?: string | null } | undefined;
        const scope = searchArgs?.scope ?? "curated";
        if (scope !== "curated" && scope !== "all") {
          return Promise.reject({
            code: "usage-settings-invalid",
            detail: "pricing catalog scope is invalid",
          });
        }
        return Promise.resolve(
          mockPricingCatalogSearch(searchArgs?.query, searchArgs?.limit, scope)
        );
      }
      case "usage_pricing_auto_match": {
        const matchArgs = args as { modelId?: string; fallbackRate?: string } | undefined;
        const modelId = matchArgs?.modelId ?? "";
        return Promise.resolve(mockPricingAutoMatch(modelId, matchArgs?.fallbackRate));
      }
      case "usage_pricing_quote_get": {
        const quoteArgs = args as
          { providerId?: string; modelId?: string; fallbackRate?: string } | undefined;
        return Promise.resolve(
          mockPricingQuoteGet(
            quoteArgs?.providerId ?? "",
            quoteArgs?.modelId ?? "",
            quoteArgs?.fallbackRate
          )
        );
      }

      // —— TPS 短时抽样七命令（Task 8；形状与 mockFixtures 同源）——
      case "tps_snapshot":
        return Promise.resolve(
          mockTpsSnapshotResult((args as { selection?: TpsSelection } | undefined)?.selection)
        );
      case "tps_points": {
        const tpsArgs = args as { selection?: TpsSelection; viewRange?: TpsViewRange } | undefined;
        return Promise.resolve(mockTpsPointsResult(tpsArgs?.selection, tpsArgs?.viewRange ?? "1h"));
      }
      case "tps_refresh":
        return Promise.resolve(mockTpsRefresh());
      case "tps_view_heartbeat":
        return Promise.resolve(mockTpsHeartbeat(args as { visible?: boolean } | undefined));
      case "tps_install_preview":
        return Promise.resolve(
          mockTpsInstallPreview((args as { tool?: string } | undefined)?.tool)
        );
      case "tps_install_apply": {
        const applyArgs = args as { previewId?: string; choice?: TpsInstallChoice } | undefined;
        return mockTpsInstallApply(applyArgs?.previewId, applyArgs?.choice);
      }
      case "tps_uninstall":
        return mockTpsUninstall();
      case "tps_speed_dashboard":
        // V2 速度 dashboard（Task 9）：只读快照；mock 同源夹具，形状与 Rust camelCase 一致
        return Promise.resolve(
          mockSpeedDashboard((args as { query?: SpeedDashboardQuery } | undefined)?.query)
        );

      // —— 四页工作区导航三命令（Task 7；先存后 emit，与 Rust 命令同语义）——
      case "workspace_request_navigation": {
        const tab = (args as { tab?: string } | undefined)?.tab ?? "messages";
        mockNavRevision += 1;
        const nav = { revision: mockNavRevision, tab };
        mockNavPending = nav; // ① 先存
        dispatchMockEvent("tuvis-workspace-navigation", nav); // ② 后 emit
        return Promise.resolve(nav);
      }
      case "workspace_peek_navigation":
        return Promise.resolve(mockNavPending);
      case "workspace_ack_navigation": {
        const revision = (args as { revision?: number } | undefined)?.revision;
        if (mockNavPending !== null && mockNavPending.revision === revision) {
          mockNavPending = null;
          return Promise.resolve(true);
        }
        return Promise.resolve(false);
      }

      // Default: return empty success
      default:
        console.log(`[tauri-mock] Unhandled command: ${cmd}`);
        return Promise.resolve(null);
    }
  }
}

export {};
