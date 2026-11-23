import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
const { invoke, listen } = vi.hoisted(() => ({ invoke: vi.fn(), listen: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke }));
vi.mock("@tauri-apps/api/event", () => ({ listen }));
import i18n from "@/i18n";
import { OpenCodeNativeSection } from "@/components/settings/OpenCodeNativeSection";

const t = (key: string, options?: Record<string, unknown>) => i18n.t(key, options);

type PolicyStatus = {
  policy: "native_only" | "legacy_compatible";
  legacyConfigurable: boolean;
};
let policy: PolicyStatus["policy"] = "native_only";
let legacyConfigurable = true;
const policyStatus = (): PolicyStatus => ({ policy, legacyConfigurable });
let installed = false;
let attested = false;
let observerStatus: "absent" | "current" | "outdated" | "conflict" = "absent";
/** 安装回执特例：仍返回 conflict（模拟外来文件/上次中断），不改本地状态。 */
let observerInstallReturnsConflict = false;
let failObserverInstall = false;
let failBridgeInstall = false;
let bridgeLastError: string | null = null;

const bridgeView = () => ({
  install: {
    pluginPath: "/tmp/plugin",
    configPath: "/tmp/opencode.json",
    pluginInstalled: installed,
    configRegistered: installed,
    backupPath: "/tmp/opencode.json.bak",
  },
  bridge: attested
    ? {
        attested: true,
        runtime: { serviceId: "svc", pid: 4242, startedAtMs: 1 },
        incarnation: "inc",
        location: "/tmp/project",
        peerPid: 4242,
        lastError: null,
      }
    : bridgeLastError === null
      ? null
      : {
          attested: false,
          runtime: null,
          incarnation: null,
          location: null,
          peerPid: null,
          lastError: bridgeLastError,
        },
  reloadTriggered: false,
  reloadError: null,
});

beforeEach(() => {
  policy = "native_only";
  legacyConfigurable = true;
  installed = false;
  attested = false;
  observerStatus = "absent";
  observerInstallReturnsConflict = false;
  failObserverInstall = false;
  failBridgeInstall = false;
  bridgeLastError = null;
  invoke.mockReset();
  listen.mockReset();
  listen.mockResolvedValue(() => {});
  invoke.mockImplementation(async (command, args) => {
    if (command === "opencode_interaction_policy_status") return policyStatus();
    if (command === "opencode_interaction_policy_get") return policy;
    if (command === "opencode_interaction_policy_set") {
      policy = args.policy;
      return undefined;
    }
    if (command === "opencode_tui_observer_status") return observerStatus;
    if (command === "opencode_tui_observer_install") {
      if (failObserverInstall) throw new Error("observer disk full");
      if (observerInstallReturnsConflict) return "conflict";
      observerStatus = "current";
      return observerStatus;
    }
    if (command === "opencode_tui_observer_uninstall") {
      observerStatus = "absent";
      return observerStatus;
    }
    if (command === "opencode_bridge_status") return bridgeView();
    if (command === "opencode_bridge_install") {
      if (failBridgeInstall) throw new Error("bridge config locked");
      installed = true;
      return bridgeView();
    }
    if (command === "opencode_bridge_uninstall") {
      installed = false;
      attested = false;
      return bridgeView();
    }
    if (command === "opencode_bridge_reload") return bridgeView();
    return undefined;
  });
});
afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

/** 等总状态行进入某个判定态（data-state 是映射单点的可观测面）。 */
async function waitForState(state: string) {
  await waitFor(() =>
    expect(screen.getByTestId("opencode-native-status")).toHaveAttribute("data-state", state)
  );
}

async function expandDetails() {
  fireEvent.click(screen.getByTestId("opencode-native-details-toggle"));
  return await screen.findByTestId("opencode-native-details");
}

/** 折叠态总开关不在 DOM 里：凡是要动总开关的用例都必须先展开卡面。 */
async function expandAndGetMaster() {
  const details = await expandDetails();
  return { details, master: within(details).getByTestId("opencode-native-switch") };
}

// ---------------------------------------------------------------------------
// 总开关（本轮新增语义）：一次开关 = 两个插件各自命令的组合，不是第三套安装器。
// ---------------------------------------------------------------------------

it("turning the master switch on installs both plugins in one step", async () => {
  render(<OpenCodeNativeSection />);
  await waitForState("off");
  const { master } = await expandAndGetMaster();
  expect(master).toHaveAttribute("aria-checked", "false");

  fireEvent.click(master);

  await waitFor(() => expect(invoke).toHaveBeenCalledWith("opencode_tui_observer_install"));
  await waitFor(() => expect(invoke).toHaveBeenCalledWith("opencode_bridge_install"));
  // 每个插件仍只调用各自的安装命令，各一次（总开关不引入新命令/重复调用）。
  expect(invoke.mock.calls.filter(([c]) => c === "opencode_tui_observer_install")).toHaveLength(1);
  expect(invoke.mock.calls.filter(([c]) => c === "opencode_bridge_install")).toHaveLength(1);

  await waitFor(() => expect(master).toHaveAttribute("aria-checked", "true"));
  // 两个插件都装了但服务尚未加载：如实说「等待加载」，不冒充已就绪。
  await waitForState("pending");
});

it("turning the master switch off uninstalls both plugins", async () => {
  observerStatus = "current";
  installed = true;
  attested = true;
  render(<OpenCodeNativeSection />);
  await waitForState("ready");
  const { master } = await expandAndGetMaster();
  await waitFor(() => expect(master).toHaveAttribute("aria-checked", "true"));

  fireEvent.click(master);

  await waitFor(() => expect(invoke).toHaveBeenCalledWith("opencode_tui_observer_uninstall"));
  await waitFor(() => expect(invoke).toHaveBeenCalledWith("opencode_bridge_uninstall"));
  expect(invoke.mock.calls.filter(([c]) => c === "opencode_tui_observer_uninstall")).toHaveLength(
    1
  );
  expect(invoke.mock.calls.filter(([c]) => c === "opencode_bridge_uninstall")).toHaveLength(1);
  await waitFor(() => expect(master).toHaveAttribute("aria-checked", "false"));
  await waitForState("off");
});

it("a failed half reports the real reason, keeps the successful half, and offers repair", async () => {
  failBridgeInstall = true;
  render(<OpenCodeNativeSection />);
  await waitForState("off");
  const { details, master } = await expandAndGetMaster();
  fireEvent.click(master);

  await waitFor(() =>
    expect(screen.getByTestId("opencode-native-status")).toHaveTextContent(/bridge config locked/)
  );
  const status = screen.getByTestId("opencode-native-status");
  expect(status).not.toHaveTextContent(t("settings.openCodeNative.statusReady"));
  expect(status).toHaveAttribute("role", "alert");

  // 不谎报也不回滚：观察插件已装好，不回退、不触发反向卸载。
  expect(invoke).toHaveBeenCalledWith("opencode_tui_observer_install");
  expect(invoke).not.toHaveBeenCalledWith("opencode_tui_observer_uninstall");
  expect(invoke).not.toHaveBeenCalledWith("opencode_bridge_uninstall");

  // 修复入口（重跑幂等安装）在失败态常驻（展开内容内）。
  const repair = within(details).getByTestId("opencode-native-repair");
  expect(repair).toHaveTextContent(t("settings.openCodeNative.repair"));
  expect(repair).toBeEnabled();

  // 详情里如实分层：观察插件已装、桥接操作有告警。
  expect(within(details).getByTestId("opencode-observer-status")).toHaveTextContent(
    t("settings.openCodeNative.observer.current")
  );
  expect(within(details).getByTestId("opencode-bridge-error")).toBeInTheDocument();

  // 后续一次成功的状态刷新不得把失败谎报成已就绪。
  fireEvent.click(within(details).getByTestId("opencode-native-recheck"));
  await waitFor(() =>
    expect(screen.getByTestId("opencode-native-status")).toHaveTextContent(/bridge config locked/)
  );
  await waitForState("failed");
});

it("a successful repair clears the failure line instead of staying red", async () => {
  failBridgeInstall = true;
  render(<OpenCodeNativeSection />);
  await waitForState("off");
  const { details, master } = await expandAndGetMaster();
  fireEvent.click(master);
  await waitForState("failed");
  const repair = within(details).getByTestId("opencode-native-repair");
  await waitFor(() =>
    expect(screen.getByTestId("opencode-native-status")).toHaveTextContent(/bridge config locked/)
  );
  expect(repair).toBeEnabled();

  // 失败原因消失（例如配置锁释放）后重跑修复：桥接补装成功，失败原因必须被清掉。
  failBridgeInstall = false;
  fireEvent.click(repair);

  await waitFor(() =>
    expect(screen.getByTestId("opencode-native-status")).not.toHaveTextContent(
      /bridge config locked/
    )
  );
  await waitForState("pending");
  expect(screen.getByTestId("opencode-native-status")).not.toHaveTextContent(
    t("settings.openCodeNative.statusFailed", { reason: "bridge config locked" })
  );
});

it("shows a single status line in the normal state and keeps details collapsed", async () => {
  observerStatus = "current";
  installed = true;
  attested = true;
  render(<OpenCodeNativeSection />);
  const status = screen.getByTestId("opencode-native-status");
  await waitFor(() => expect(status).toHaveAttribute("data-state", "ready"));
  // 正常态只显示一句结论。
  expect(status.textContent).toBe(t("settings.openCodeNative.statusReady"));

  // 折叠态 = 只有一行：详情面、总开关、动作与兼容控件都不在 DOM 里。
  expect(screen.queryByTestId("opencode-native-details")).toBeNull();
  expect(screen.queryByTestId("opencode-observer")).toBeNull();
  expect(screen.queryByTestId("opencode-bridge")).toBeNull();
  expect(screen.queryByTestId("opencode-native-switch")).toBeNull();
  expect(screen.queryByTestId("opencode-native-repair")).toBeNull();
  expect(screen.queryByTestId("opencode-native-remove")).toBeNull();
  expect(screen.queryByTestId("opencode-legacy-switch")).toBeNull();
  expect(screen.getByTestId("opencode-native-details-toggle")).toHaveAttribute(
    "aria-expanded",
    "false"
  );

  const details = await expandDetails();
  expect(screen.getByTestId("opencode-native-details-toggle")).toHaveAttribute(
    "aria-expanded",
    "true"
  );
  // 正常态不出现「安装/修复」，只留总开关 + 卸载。
  const master = within(details).getByTestId("opencode-native-switch");
  expect(master).toHaveAttribute("aria-checked", "true");
  expect(within(details).queryByTestId("opencode-native-repair")).toBeNull();
  expect(within(details).getByTestId("opencode-native-remove")).toBeInTheDocument();
  expect(within(details).getByTestId("opencode-observer-status")).toHaveTextContent(
    t("settings.openCodeNative.observer.current")
  );
  expect(within(details).getByTestId("opencode-bridge-status")).toHaveTextContent("PID 4242");
  expect(details).toHaveTextContent(t("settings.openCodeNative.bridge.impact"));
  expect(details).toHaveTextContent(t("settings.openCodeNative.bridge.sessionWriteStatus"));
  expect(within(details).getByTestId("opencode-bridge-reload")).toBeInTheDocument();
});

it("uses the card header as the only collapse toggle and nests no interactive control in it", async () => {
  observerStatus = "current";
  installed = true;
  attested = true;
  render(<OpenCodeNativeSection />);
  await waitForState("ready");

  const header = screen.getByTestId("opencode-native-details-toggle");
  expect(header).toHaveAttribute("aria-controls", "opencode-native-details-panel");
  expect(header).toHaveAttribute("aria-expanded", "false");
  // 卡头是唯一展开入口：展开/收起都只由它切换。
  fireEvent.click(header);
  expect(header).toHaveAttribute("aria-expanded", "true");
  const panel = await screen.findByTestId("opencode-native-details");
  expect(panel).toHaveAttribute("id", "opencode-native-details-panel");
  // 卡头内不得嵌套任何可交互元素（开关/按钮都在展开内容里）。
  expect(header.querySelectorAll("button, [role='switch'], input, a").length).toBe(0);
  fireEvent.click(header);
  expect(header).toHaveAttribute("aria-expanded", "false");
  expect(screen.queryByTestId("opencode-native-details")).toBeNull();
  // 收起后交互控件随之消失，不会浮在卡头外。
  expect(screen.queryByTestId("opencode-native-switch")).toBeNull();
});

it("keeps repair, remove and the recovery actions inside the expanded panel", async () => {
  observerStatus = "conflict";
  installed = true;
  render(<OpenCodeNativeSection />);
  await waitForState("conflict");
  // 冲突态折叠时只显示状态与箭头，恢复动作不得常驻在卡头下方。
  expect(screen.queryByTestId("opencode-native-repair")).toBeNull();
  expect(screen.queryByTestId("opencode-native-remove")).toBeNull();

  const details = await expandDetails();
  expect(within(details).getByTestId("opencode-native-repair")).toBeInTheDocument();
  expect(within(details).getByTestId("opencode-native-remove")).toBeInTheDocument();
});

it.each([
  ["opencode-native-repair", "retryInstall", "opencode_tui_observer_install"],
  ["opencode-native-remove", "finishUninstall", "opencode_tui_observer_uninstall"],
] as const)(
  "observer conflict keeps an explicit recovery action (%s)",
  async (testId, key, command) => {
    observerStatus = "conflict";
    installed = true;
    render(<OpenCodeNativeSection />);
    await waitForState("conflict");
    expect(screen.getByTestId("opencode-native-status")).not.toHaveTextContent(
      t("settings.openCodeNative.statusReady")
    );
    const details = await expandDetails();
    const button = within(details).getByTestId(testId);
    expect(button).toHaveTextContent(t(`settings.openCodeNative.observer.${key}`));
    expect(button).toBeEnabled();
    fireEvent.click(button);
    await waitFor(() => expect(invoke).toHaveBeenCalledWith(command));
  }
);

// ---------------------------------------------------------------------------
// 「允许旧终端交互」：由桌面状态决定是否可配置。
// ---------------------------------------------------------------------------

it("macOS hides legacy configuration while native controls remain usable", async () => {
  legacyConfigurable = false;
  render(<OpenCodeNativeSection />);
  await waitForState("off");

  const { details, master } = await expandAndGetMaster();
  expect(master).toBeEnabled();
  fireEvent.click(master);
  await waitForState("pending");
  expect(within(details).getByTestId("opencode-observer-status")).toBeInTheDocument();
  expect(within(details).getByTestId("opencode-bridge-status")).toBeInTheDocument();
  expect(invoke.mock.calls.map(([command]) => command)).not.toContain(
    "opencode_interaction_policy_set"
  );
  expect(within(details).queryByTestId("opencode-legacy-switch")).not.toBeInTheDocument();
  expect(invoke).toHaveBeenCalledWith("opencode_interaction_policy_status");
});

it("Windows requires explicit legacy opt-in and persists it across remount", async () => {
  const first = render(<OpenCodeNativeSection />);
  await waitForState("off");
  const details = await expandDetails();
  const legacy = within(details).getByTestId("opencode-legacy-switch");
  await waitFor(() => expect(legacy).toBeEnabled());
  expect(legacy).toHaveAttribute("aria-checked", "false");
  fireEvent.click(legacy);
  await waitFor(() => expect(legacy).toHaveAttribute("aria-checked", "true"));
  expect(invoke).toHaveBeenCalledWith("opencode_interaction_policy_set", {
    policy: "legacy_compatible",
  });
  expect(policy).toBe("legacy_compatible");
  first.unmount();
  render(<OpenCodeNativeSection />);
  const reopened = await expandDetails();
  await waitFor(() =>
    expect(within(reopened).getByTestId("opencode-legacy-switch")).toHaveAttribute(
      "aria-checked",
      "true"
    )
  );
});

it("failed legacy save retains the authoritative setting", async () => {
  render(<OpenCodeNativeSection />);
  await waitForState("off");
  const details = await expandDetails();
  const legacy = within(details).getByTestId("opencode-legacy-switch");
  await waitFor(() => expect(legacy).toBeEnabled());
  const fallback = invoke.getMockImplementation()!;
  invoke.mockImplementation(async (command, args) => {
    if (command === "opencode_interaction_policy_set") throw new Error("busy");
    return fallback(command, args);
  });
  fireEvent.click(legacy);
  await within(details).findByRole("alert");
  expect(legacy).toHaveAttribute("aria-checked", "false");
});

it("legacy policy read failure offers read-only retry and recovers the authoritative policy", async () => {
  let failedOnce = false;
  const fallback = invoke.getMockImplementation()!;
  invoke.mockImplementation(async (command, args) => {
    if (command === "opencode_interaction_policy_status" && !failedOnce) {
      failedOnce = true;
      throw new Error("offline");
    }
    return fallback(command, args);
  });
  render(<OpenCodeNativeSection />);
  await waitForState("off");
  // 首拍读取失败：重试入口只在展开内容里（不挂在折叠卡头下方）。
  const details = await expandDetails();
  await within(details).findByTestId("opencode-policy-retry");
  expect(within(details).queryByTestId("opencode-legacy-switch")).not.toBeInTheDocument();
  fireEvent.click(within(details).getByTestId("opencode-policy-retry"));
  expect(await within(details).findByTestId("opencode-legacy-switch")).toBeEnabled();
  expect(
    invoke.mock.calls.every(
      ([command]) =>
        command === "opencode_interaction_policy_status" ||
        command === "opencode_bridge_status" ||
        command === "opencode_tui_observer_status"
    )
  ).toBe(true);
});

it("a late policy read cannot overwrite a newer device policy notification", async () => {
  let release: ((value: PolicyStatus) => void) | undefined;
  let delayed = false;
  const fallback = invoke.getMockImplementation()!;
  invoke.mockImplementation(async (command, args) => {
    if (command === "opencode_interaction_policy_status") {
      if (!delayed) {
        delayed = true;
        return new Promise<PolicyStatus>((resolve) => {
          release = resolve;
        });
      }
      return policyStatus();
    }
    return fallback(command, args);
  });
  render(<OpenCodeNativeSection />);
  await waitFor(() => expect(release).toBeTypeOf("function"));
  const details = await expandDetails();
  policy = "legacy_compatible";
  act(() =>
    listen.mock.calls.find(([event]) => event === "opencode-interaction-policy-changed")![1]()
  );
  const legacy = await within(details).findByTestId("opencode-legacy-switch");
  await waitFor(() => expect(legacy).toHaveAttribute("aria-checked", "true"));
  await act(async () => {
    release!({ policy: "native_only", legacyConfigurable: true });
  });
  expect(legacy).toHaveAttribute("aria-checked", "true");
});

// ---------------------------------------------------------------------------
// 详情面：独立状态、失败不谎报、刷新只读。
// ---------------------------------------------------------------------------

it("shows the observer as installed independently of an absent service bridge", async () => {
  observerStatus = "current";
  render(<OpenCodeNativeSection />);
  await waitForState("partial");
  const details = await expandDetails();
  expect(within(details).getByTestId("opencode-observer-status")).toHaveTextContent(
    t("settings.openCodeNative.observer.current")
  );
  expect(within(details).getByTestId("opencode-bridge-status")).toHaveTextContent(
    t("settings.openCodeNative.bridge.notInstalled")
  );
  expect(invoke).toHaveBeenCalledWith("opencode_tui_observer_status");
});

it("refreshes both plugin statuses without changing their installations", async () => {
  observerStatus = "current";
  installed = true;
  render(<OpenCodeNativeSection />);
  await waitForState("pending");
  const details = await expandDetails();
  const count = (command: string) => invoke.mock.calls.filter(([c]) => c === command).length;
  const observerReads = count("opencode_tui_observer_status");
  const bridgeReads = count("opencode_bridge_status");

  fireEvent.click(within(details).getByTestId("opencode-native-recheck"));

  await waitFor(() => expect(count("opencode_tui_observer_status")).toBe(observerReads + 1));
  expect(count("opencode_bridge_status")).toBe(bridgeReads + 1);
  expect(invoke).not.toHaveBeenCalledWith("opencode_tui_observer_install");
  expect(invoke).not.toHaveBeenCalledWith("opencode_tui_observer_uninstall");
  expect(invoke).not.toHaveBeenCalledWith("opencode_bridge_install");
  expect(invoke).not.toHaveBeenCalledWith("opencode_bridge_uninstall");
});

it("keeps an observer install failure visible even when the following status query succeeds", async () => {
  failObserverInstall = true;
  render(<OpenCodeNativeSection />);
  await waitForState("off");
  const { details, master } = await expandAndGetMaster();
  fireEvent.click(master);
  await waitFor(() =>
    expect(screen.getByTestId("opencode-native-status")).toHaveTextContent(/observer disk full/)
  );
  // 前一步失败不阻断后一步（组合动作朝一致状态推进，不做来回装卸）。
  expect(invoke).toHaveBeenCalledWith("opencode_bridge_install");
  expect(within(details).getByRole("alert")).toBeInTheDocument();
  expect(within(details).queryByTestId("opencode-observer-restart")).toBeNull();
});

it("does not claim readiness when the observer install returns conflict", async () => {
  observerInstallReturnsConflict = true;
  render(<OpenCodeNativeSection />);
  await waitForState("off");
  const { details, master } = await expandAndGetMaster();
  fireEvent.click(master);
  await waitFor(() =>
    expect(screen.getByTestId("opencode-native-status")).toHaveTextContent(
      t("settings.openCodeNative.statusConflict")
    )
  );
  expect(screen.getByTestId("opencode-native-status")).not.toHaveTextContent(
    t("settings.openCodeNative.statusReady")
  );
  expect(within(details).getByTestId("opencode-observer-status")).toHaveTextContent(
    t("settings.openCodeNative.observer.conflict")
  );
  expect(within(details).queryByTestId("opencode-observer-restart")).toBeNull();
});

it("reports a dropped bridge connection on the single status line with its reason", async () => {
  observerStatus = "current";
  installed = true;
  bridgeLastError = "bridge confirm timeout";
  render(<OpenCodeNativeSection />);
  await waitForState("failed");
  const status = screen.getByTestId("opencode-native-status");
  expect(status).toHaveTextContent("bridge confirm timeout");
  expect(status).not.toHaveTextContent(t("settings.openCodeNative.statusReady"));
  expect(status).toHaveAttribute("role", "alert");
  // 桥接已装但连接失败 → 展开后给出可重装的修复入口。
  const details = await expandDetails();
  expect(within(details).getByTestId("opencode-native-repair")).toBeEnabled();
});

it("keeps the master switch disabled while plugin state is unknown", async () => {
  const fallback = invoke.getMockImplementation()!;
  invoke.mockImplementation(async (command, args) => {
    if (command === "opencode_tui_observer_status") throw new Error("observer offline");
    return fallback(command, args);
  });
  render(<OpenCodeNativeSection />);
  await waitForState("failed");
  const { master } = await expandAndGetMaster();
  expect(master).toBeDisabled();
  // 读取失败也不得冒充「未开启」：状态行如实说读不到。
  expect(screen.getByTestId("opencode-native-status")).not.toHaveTextContent(
    t("settings.openCodeNative.statusOff")
  );
});
