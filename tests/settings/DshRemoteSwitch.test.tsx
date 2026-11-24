// tests/settings/DshRemoteSwitch.test.tsx — Task 4：「远程控制 DSH」单开关卡。
//
// 覆盖（brief 验收映射）：
//   collapsed_by_default_and_single_instance_targets_directly —— 默认折叠；
//       单实例展开后自动登记选择（开关行直接展示明确目标）；
//   first_enable_confirm_cancel_starts_nothing —— 可选首次确认（最多一次）；
//       取消时不发起任何作业；
//   enable_confirm_ok_calls_switch_enable —— 确认后按选定实例发起开启意图；
//   linear_progress_states —— 线性推进：等待退出指引 / 重开按钮 / 就绪结果；
//   waiting_uninstall_banner —— 「远控已关闭 · 等待卸载」+ 卸载作业阶段；
//   failed_state_shows_step_and_retry —— 失败保持意图，失败步骤 + 重试入口
//       （开启失败 → 重试=再开启；关闭失败 → 重试=再关闭）；
//   rebind_required_shows_trusted_rebind_panel —— 身份变化 → 可信重绑面板；
//   repair_entry_is_collapsible_and_instance_scoped —— 修复插件可展开、只对
//       当前实例发起 dsh_switch_repair，修复中开关锁定；
//   F09 —— 关闭未完成时禁止开启及修复；作业结束后可重试关闭。
//   closable_during_install_wait —— 安装作业等待退出期间开关可关闭；
//   disable_calls_switch_disable —— 点击关闭发起 dsh_switch_disable；
//   i18n zh/en 键集对账（卡片源码引用键双 locale 齐备）。
//
// 旧四件套（DshRemoteSection*.test.tsx）随独立安装/绑定/撤权/卸载按钮一并退役：
// 其后端行为由 src-tauri switch_tests.rs 的状态机回归承接。
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { readFileSync } from "node:fs";
import path from "node:path";

const { invokeMock } = vi.hoisted(() => ({
  invokeMock: vi.fn(),
}));
vi.mock("@tauri-apps/api/core", () => ({ invoke: invokeMock }));
// useAppTranslation 内部 listen("@tauri-apps/api/event") 在 jsdom 无 Tauri 内核，须 mock
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(async () => () => {}),
  emit: vi.fn(async () => {}),
}));
vi.mock("sonner", () => ({
  toast: Object.assign(vi.fn(), { info: vi.fn(), success: vi.fn(), error: vi.fn() }),
}));

// jsdom navigator.language=en：brief 断言面是中文文案，显式切 zh
import i18n from "@/i18n";
import { queryClient } from "@/lib/query/queryClient";
import { DshRemoteSection } from "@/components/settings/DshRemoteSection";
import type {
  DshActivationState,
  DshAuthorizationState,
  DshInstallationState,
  DshSwitchView,
} from "@/types/dsh";

void i18n;

// ---- mock 载荷工厂（形状契约 = src/types/dsh.ts 的 DshSwitchView 镜像）----
const INSTANCE_ID = "6d96b3036ae0024d0b4d08a33c94a005";
const INSTANCE_B_ID = "aaaa0000aaaa0000aaaa0000aaaa0001";

const instanceOf = (over: Record<string, unknown> = {}) => ({
  instanceId: INSTANCE_ID,
  kind: "desktop",
  displayName: "工作机 DSH",
  profileDir: "/tmp/profile",
  live: true,
  selected: true,
  hostEpoch: "1",
  ...over,
});

// F09 扩充的是同一开关外表；旧载荷可不含插件事实，不向 legacy 状态借值。
type SwitchFixture = DshSwitchView & {
  pluginStatus?: {
    installation: DshInstallationState;
    activation: DshActivationState;
    authorization: DshAuthorizationState;
    version: string | null;
    reasonCode: string | null;
  };
};

const pluginOf = (
  over: Partial<NonNullable<SwitchFixture["pluginStatus"]>> = {}
): NonNullable<SwitchFixture["pluginStatus"]> => ({
  installation: "installed",
  activation: "restart_required",
  authorization: "unbound",
  version: "1.27.4",
  reasonCode: null,
  ...over,
});

const switchOf = (over: Partial<SwitchFixture> = {}): SwitchFixture => ({
  instanceId: INSTANCE_ID,
  enabled: false,
  phase: "off",
  failedStep: null,
  reasonCode: null,
  job: null,
  send: null,
  console: null,
  rebindRequired: false,
  ...over,
});

const jobOf = (over: Record<string, unknown> = {}) => ({
  jobId: "mgmt-1760000000000-0",
  kind: "install",
  phase: "queued",
  reasonCode: null,
  ...over,
});

let switchPayload: SwitchFixture = switchOf();
let instancesPayload: Record<string, unknown>[] = [instanceOf()];
let legacyStatusPayload: Record<string, unknown> | null = {
  installation: "missing",
  activation: "disabled",
  authorization: "unbound",
  operation: null,
  desktopVersion: null,
  pluginVersion: null,
  reasonCode: null,
  autoAdaptEnabled: true,
  bindingId: null,
};

/** 默认 invoke：开关/实例/legacy 状态走可变载荷；动作命令记录调用。 */
function installDefaultInvoke() {
  invokeMock.mockImplementation(async (cmd: string, args?: Record<string, unknown>) => {
    if (cmd === "dsh_switch_get") return { ...switchPayload };
    if (cmd === "dsh_instance_list") return instancesPayload.map((i) => ({ ...i }));
    if (cmd === "dsh_remote_status")
      return legacyStatusPayload === null ? null : { ...legacyStatusPayload };
    if (cmd === "dsh_instance_select") {
      instancesPayload = instancesPayload.map((i) => ({
        ...i,
        selected: i.instanceId === (args as { request: { instanceId: string } }).request.instanceId,
      }));
      return null;
    }
    if (
      cmd === "dsh_switch_enable" ||
      cmd === "dsh_switch_disable" ||
      cmd === "dsh_switch_repair"
    ) {
      return {
        ...((args as { request: { instanceId: string } }).request.instanceId === INSTANCE_B_ID
          ? switchOf({ instanceId: INSTANCE_B_ID })
          : switchPayload),
      };
    }
    if (cmd === "dsh_restart_host") return null;
    if (cmd === "dsh_set_auto_adapt") return true;
    if (cmd === "dsh_binding_start") {
      return {
        bindingId: "binding-1",
        displayCode: "AB12CD",
        expiresAtMs: 1,
        clientExecutable: "DeepSeek Harness",
        clientPid: 1,
        rustFingerprint: "rustfp",
        instanceId: INSTANCE_ID,
        oldFingerprint: "oldfp",
        newFingerprint: "newfp",
      };
    }
    if (cmd === "dsh_binding_rebind") return null;
    return null;
  });
}

beforeAll(async () => {
  await i18n.changeLanguage("zh");
});

beforeEach(() => {
  invokeMock.mockReset();
  queryClient.clear();
  switchPayload = switchOf();
  instancesPayload = [instanceOf()];
  legacyStatusPayload = {
    installation: "missing",
    activation: "disabled",
    authorization: "unbound",
    operation: null,
    desktopVersion: null,
    pluginVersion: null,
    reasonCode: null,
    autoAdaptEnabled: true,
    bindingId: null,
  };
});

afterEach(() => {
  cleanup();
  queryClient.clear();
});

/** 展开卡片（展开本身是行为；默认折叠）。 */
function renderExpanded() {
  const view = render(<DshRemoteSection />);
  fireEvent.click(screen.getByTestId("tool-config-header-dsh"));
  return view;
}

const switchEl = () => screen.getByTestId("dsh-switch");
const callsOf = (cmd: string) => invokeMock.mock.calls.filter((c) => c[0] === cmd);

// ============================================================
// brief 指定验收
// ============================================================

describe("DshRemoteSwitch 单开关卡", () => {
  it("collapsed_by_default_and_single_instance_targets_directly：默认折叠；单实例展开即自动登记并展示明确目标", async () => {
    installDefaultInvoke();
    instancesPayload = [instanceOf({ selected: false })];
    const { unmount } = render(<DshRemoteSection />);
    expect(screen.queryByTestId("dsh-switch")).toBeNull();
    fireEvent.click(screen.getByTestId("tool-config-header-dsh"));
    await waitFor(() => {
      expect(callsOf("dsh_instance_select")).toHaveLength(1);
    });
    expect(callsOf("dsh_instance_select")[0][1]).toEqual({
      request: { instanceId: INSTANCE_ID },
    });
    await waitFor(() => {
      expect(switchEl()).toBeEnabled();
    });
    expect(document.body.textContent ?? "").toContain("工作机 DSH");
    unmount();
  });

  it("first_enable_confirm_cancel_starts_nothing：首次开启最多一次确认；取消不开始作业", async () => {
    installDefaultInvoke();
    renderExpanded();
    await waitFor(() => expect(switchEl()).toBeEnabled());
    fireEvent.click(switchEl());
    const dialog = await screen.findByTestId("dsh-enable-confirm-cancel");
    expect(dialog).toBeInTheDocument();
    fireEvent.click(dialog);
    await waitFor(() => expect(screen.queryByTestId("dsh-enable-confirm-ok")).toBeNull());
    expect(callsOf("dsh_switch_enable")).toHaveLength(0);
  });

  it("enable_confirm_ok_calls_switch_enable：确认后按选定实例发起开启意图", async () => {
    installDefaultInvoke();
    renderExpanded();
    await waitFor(() => expect(switchEl()).toBeEnabled());
    fireEvent.click(switchEl());
    fireEvent.click(await screen.findByTestId("dsh-enable-confirm-ok"));
    await waitFor(() => expect(callsOf("dsh_switch_enable")).toHaveLength(1));
    expect(callsOf("dsh_switch_enable")[0][1]).toEqual({
      request: { instanceId: INSTANCE_ID },
    });
  });

  it("linear_progress_states：等待退出指引 / 重开按钮 / 就绪各自就绪结果", async () => {
    installDefaultInvoke();
    // ① 安装中 + 作业等待退出
    switchPayload = switchOf({
      enabled: true,
      phase: "installing",
      job: jobOf({ kind: "install", phase: "waiting_for_desktop_exit" }),
    });
    const { unmount } = renderExpanded();
    await waitFor(() => expect(screen.getByTestId("dsh-waiting-exit")).toBeInTheDocument());
    expect(screen.getByTestId("dsh-step-install").dataset.state).toBe("active");
    expect(screen.getByTestId("dsh-step-instance_check").dataset.state).toBe("done");
    unmount();

    // ② 等待重开：给重开入口
    queryClient.clear();
    switchPayload = switchOf({ enabled: true, phase: "awaiting_reopen" });
    renderExpanded();
    const reopen = await screen.findByTestId("dsh-reopen-host");
    fireEvent.click(reopen);
    await waitFor(() => expect(callsOf("dsh_restart_host")).toHaveLength(1));
    expect(callsOf("dsh_restart_host")[0][1]).toEqual({ request: { instanceId: INSTANCE_ID } });
    cleanup();

    // ③ 就绪：发送/控制台各自就绪结果分开呈现
    queryClient.clear();
    switchPayload = switchOf({
      enabled: true,
      phase: "ready",
      send: "ready",
      console: "recovering",
    });
    renderExpanded();
    await waitFor(() => expect(screen.getByTestId("dsh-readiness")).toBeInTheDocument());
    expect(screen.getByTestId("dsh-send-readiness").dataset.state).toBe("ready");
    expect(screen.getByTestId("dsh-console-readiness").dataset.state).toBe("recovering");
    cleanup();
  });

  describe("F09 设置卡独立事实", () => {
    it("等待重开时已经显示所选实例的已安装状态和插件版本", async () => {
      installDefaultInvoke();
      switchPayload = switchOf({
        enabled: true,
        phase: "awaiting_reopen",
        pluginStatus: pluginOf(),
      });
      renderExpanded();
      await screen.findByTestId("dsh-await-reopen");
      expect(screen.getByTestId("dsh-installation-state")).toHaveAttribute(
        "data-state",
        "installed"
      );
      expect(screen.getByText(/1\.27\.4/)).toBeVisible();
    });

    it.each(["restart_required", "running"] as const)(
      "独立显示加载事实 %s，不把已安装或开关开启等同于加载",
      async (activation) => {
        installDefaultInvoke();
        switchPayload = switchOf({
          enabled: true,
          phase: "verifying_plugin",
          pluginStatus: pluginOf({ activation }),
        });
        renderExpanded();
        expect(await screen.findByTestId("dsh-activation-state")).toHaveAttribute(
          "data-state",
          activation
        );
      }
    );

    it("通道尚未恢复时仍显示所选实例保存的授权", async () => {
      installDefaultInvoke();
      switchPayload = switchOf({
        enabled: true,
        phase: "authorizing",
        pluginStatus: pluginOf({ activation: "running", authorization: "bound" }),
        send: "recovering",
        console: "recovering",
      });
      renderExpanded();
      expect(await screen.findByTestId("dsh-authorization-state")).toHaveAttribute(
        "data-state",
        "bound"
      );
    });

    it.each([
      [
        "推进中",
        switchOf({
          enabled: true,
          phase: "authorizing",
          send: "recovering",
          console: "recovering",
        }),
      ],
      [
        "开启失败",
        switchOf({
          enabled: true,
          phase: "failed",
          failedStep: "install",
          reasonCode: "DSH_CLI_FAILED",
          send: "not_authorized",
          console: "not_authorized",
        }),
      ],
    ] as const)("%s时分别显示后端通道状态，不因开启意图显示就绪", async (_, view) => {
      installDefaultInvoke();
      switchPayload = view;
      renderExpanded();
      await waitFor(() => expect(switchEl()).toBeChecked());
      expect(screen.getByTestId("dsh-send-readiness")).toHaveAttribute("data-state", view.send);
      expect(screen.getByTestId("dsh-console-readiness")).toHaveAttribute(
        "data-state",
        view.console
      );
    });

    it.each([
      [
        "等待卸载",
        switchOf({ phase: "waiting_uninstall", send: "ready", console: "ready" }),
        "dsh-waiting-uninstall",
      ],
      [
        "关闭失败",
        switchOf({ phase: "failed", failedStep: "revoke", send: "ready", console: "ready" }),
        "dsh-failed",
      ],
    ] as const)("%s时分别显示通道关闭，不沿用残留的就绪结果", async (_, view, phaseTestId) => {
      installDefaultInvoke();
      switchPayload = view;
      renderExpanded();
      await screen.findByTestId(phaseTestId);
      expect(screen.getByTestId("dsh-send-readiness")).toHaveAttribute("data-state", "off");
      expect(screen.getByTestId("dsh-console-readiness")).toHaveAttribute("data-state", "off");
    });

    it("已授权且凭据已交付时，官方网页未验证的控制台仍不显示可用", async () => {
      installDefaultInvoke();
      switchPayload = switchOf({
        enabled: true,
        phase: "ready",
        pluginStatus: pluginOf({ activation: "running", authorization: "bound" }),
        send: "ready",
        console: "ready",
      });
      renderExpanded();
      expect(await screen.findByTestId("dsh-send-readiness")).toHaveAttribute(
        "data-state",
        "ready"
      );
      const consoleState = screen.getByTestId("dsh-console-readiness");
      expect(consoleState).toHaveAttribute("data-state", "unverified");
      expect(consoleState).not.toHaveTextContent(/已就绪|已可用/);
    });
  });

  it("waiting_uninstall_banner：远控已关闭 · 等待卸载，不能重新开启", async () => {
    installDefaultInvoke();
    switchPayload = switchOf({
      enabled: false,
      phase: "waiting_uninstall",
      job: jobOf({ kind: "uninstall", phase: "waiting_for_desktop_exit" }),
    });
    renderExpanded();
    await waitFor(() => expect(screen.getByTestId("dsh-waiting-uninstall")).toBeInTheDocument());
    expect(document.body.textContent ?? "").toContain("远控已关闭 · 等待卸载");
    expect(switchEl()).not.toBeChecked();
    expect(switchEl()).toBeDisabled();
    fireEvent.click(switchEl());
    expect(callsOf("dsh_switch_enable")).toHaveLength(0);
  });

  describe("F09 关闭失败恢复", () => {
    it("failed_close_retry_is_enabled_and_calls_disable_once", async () => {
      installDefaultInvoke();
      switchPayload = switchOf({
        enabled: false,
        phase: "failed",
        failedStep: "uninstall",
        reasonCode: "DSH_CLI_FAILED",
        job: jobOf({ kind: "uninstall", phase: "failed", reasonCode: "DSH_CLI_FAILED" }),
      });
      renderExpanded();
      expect(await screen.findByTestId("dsh-failed")).toBeInTheDocument();
      const retry = screen.getByTestId("dsh-retry");
      expect(retry).toBeEnabled();
      fireEvent.click(retry);
      fireEvent.click(retry);
      await waitFor(() => expect(callsOf("dsh_switch_disable")).toHaveLength(1));
      expect(callsOf("dsh_switch_disable")[0][1]).toEqual({ request: { instanceId: INSTANCE_ID } });
      expect(callsOf("dsh_switch_enable")).toHaveLength(0);
      expect(screen.getByTestId("dsh-retry")).toHaveTextContent(/^重试关闭$/);
    });

    it.each(["waiting_for_desktop_exit", "queued", "applying", "verifying"] as const)(
      "active_uninstall_job_disables_retry_switch_and_repair (%s)",
      async (phase) => {
        installDefaultInvoke();
        switchPayload = switchOf({
          enabled: false,
          phase: "failed",
          failedStep: "uninstall",
          job: jobOf({ kind: "uninstall", phase }),
        });
        renderExpanded();
        expect(await screen.findByTestId("dsh-retry")).toBeDisabled();
        fireEvent.click(screen.getByTestId("dsh-repair-toggle"));
        expect(await screen.findByTestId("dsh-repair-start")).toBeDisabled();
        expect(switchEl()).toBeDisabled();
        fireEvent.click(switchEl());
        expect(callsOf("dsh_switch_enable")).toHaveLength(0);
      }
    );

    it("waiting_uninstall_disables_enable_and_repair_and_shows_quit_hint", async () => {
      installDefaultInvoke();
      switchPayload = switchOf({
        enabled: false,
        phase: "waiting_uninstall",
        job: jobOf({ kind: "uninstall", phase: "waiting_for_desktop_exit" }),
      });
      renderExpanded();
      const banner = await screen.findByTestId("dsh-waiting-uninstall");
      fireEvent.click(screen.getByTestId("dsh-repair-toggle"));
      expect.soft(await screen.findByTestId("dsh-repair-start")).toBeDisabled();
      expect.soft(switchEl()).toBeDisabled();
      expect
        .soft(banner)
        .toHaveTextContent("请完全退出 DeepSeek Harness 桌面端；卸载完成后可重新开启。");
      expect(document.body).not.toHaveTextContent("请通过开关操作");
    });

    it("off_phase_restores_switch", async () => {
      installDefaultInvoke();
      switchPayload = switchOf({ enabled: false, phase: "off" });
      renderExpanded();
      await waitFor(() => expect(switchEl()).toBeEnabled());
    });

    it.each(["uninstall", "revoke", "home_evidence"] as const)(
      "failed_close_blocks_direct_enable_even_if_clicked (%s)",
      async (failedStep) => {
        installDefaultInvoke();
        switchPayload = switchOf({
          enabled: false,
          phase: "failed",
          failedStep,
          reasonCode: "DSH_CLI_FAILED",
          job: jobOf({ kind: "uninstall", phase: "failed", reasonCode: "DSH_CLI_FAILED" }),
        });
        renderExpanded();
        expect(await screen.findByTestId("dsh-failed")).toBeInTheDocument();
        fireEvent.click(screen.getByTestId("dsh-repair-toggle"));
        expect.soft(await screen.findByTestId("dsh-repair-start")).toBeDisabled();
        expect.soft(switchEl()).toBeDisabled();
        await act(async () => {
          fireEvent.click(switchEl());
        });
        expect(callsOf("dsh_switch_enable")).toHaveLength(0);
      }
    );
  });

  it("closable_during_install_wait：安装作业等待退出期间开关可关闭", async () => {
    installDefaultInvoke();
    switchPayload = switchOf({
      enabled: true,
      phase: "installing",
      job: jobOf({ kind: "install", phase: "waiting_for_desktop_exit" }),
    });
    renderExpanded();
    await waitFor(() => expect(screen.getByTestId("dsh-waiting-exit")).toBeInTheDocument());
    expect(switchEl()).toBeEnabled();
    expect(switchEl()).toBeChecked();
    fireEvent.click(switchEl());
    await waitFor(() => expect(callsOf("dsh_switch_disable")).toHaveLength(1));
    expect(callsOf("dsh_switch_disable")[0][1]).toEqual({
      request: { instanceId: INSTANCE_ID },
    });
  });

  it("failed_state_shows_step_and_retry：失败保持意图 + 失败步骤 + 重试入口", async () => {
    installDefaultInvoke();
    // ① 开启失败：重试 = 再发起开启
    switchPayload = switchOf({
      enabled: true,
      phase: "failed",
      failedStep: "install",
      reasonCode: "DSH_CLI_FAILED",
    });
    const { unmount } = renderExpanded();
    await waitFor(() => expect(screen.getByTestId("dsh-failed")).toBeInTheDocument());
    expect(document.body.textContent ?? "").toContain("安装兔维斯插件");
    fireEvent.click(screen.getByTestId("dsh-retry"));
    await waitFor(() => expect(callsOf("dsh_switch_enable")).toHaveLength(1));
    unmount();

    // ② 关闭失败（卸载失败）：重试 = 再次关闭
    queryClient.clear();
    switchPayload = switchOf({
      enabled: false,
      phase: "failed",
      failedStep: "uninstall",
      reasonCode: "DSH_VERIFY_FAILED",
    });
    renderExpanded();
    await waitFor(() => expect(screen.getByTestId("dsh-failed")).toBeInTheDocument());
    fireEvent.click(screen.getByTestId("dsh-retry"));
    await waitFor(() => expect(callsOf("dsh_switch_disable")).toHaveLength(1));
    cleanup();
  });

  it("rebind_required_shows_trusted_rebind_panel：身份变化进可信重绑面板", async () => {
    installDefaultInvoke();
    switchPayload = switchOf({
      enabled: true,
      phase: "rebind_required",
      rebindRequired: true,
    });
    renderExpanded();
    const panel = await screen.findByTestId("dsh-binding-panel");
    expect(panel).toBeInTheDocument();
    expect(screen.getByTestId("dsh-display-code").textContent).toBe("AB12CD");
    fireEvent.click(screen.getByTestId("dsh-confirm-rebind"));
    await waitFor(() => expect(callsOf("dsh_binding_rebind")).toHaveLength(1));
    expect(callsOf("dsh_binding_rebind")[0][1]).toEqual({
      instanceId: INSTANCE_ID,
      bindingId: "binding-1",
      expectedOldFingerprint: "oldfp",
    });
  });

  it("repair_entry_is_collapsible_and_instance_scoped：修复插件可展开、只对当前实例、修复中锁定开关", async () => {
    installDefaultInvoke();
    const { unmount } = renderExpanded();
    await waitFor(() => expect(switchEl()).toBeEnabled());
    expect(screen.getAllByRole("switch", { name: "远程控制 DSH" })).toEqual([switchEl()]);
    // 折叠态不出现修复按钮
    expect(screen.queryByTestId("dsh-repair-start")).toBeNull();
    fireEvent.click(screen.getByTestId("dsh-repair-toggle"));
    fireEvent.click(await screen.findByTestId("dsh-repair-start"));
    await waitFor(() => expect(callsOf("dsh_switch_repair")).toHaveLength(1));
    expect(callsOf("dsh_switch_repair")[0][1]).toEqual({
      request: { instanceId: INSTANCE_ID },
    });
    unmount();

    // 修复中：开关锁定 + 修复中标记
    queryClient.clear();
    switchPayload = switchOf({ enabled: true, phase: "repairing" });
    renderExpanded();
    fireEvent.click(await screen.findByTestId("dsh-repair-toggle"));
    await waitFor(() => expect(screen.getByTestId("dsh-repair-active")).toBeInTheDocument());
    expect(switchEl()).toBeDisabled();
    cleanup();
  });

  it("disable_calls_switch_disable：点击关闭发起 dsh_switch_disable", async () => {
    installDefaultInvoke();
    switchPayload = switchOf({ enabled: true, phase: "ready", send: "ready", console: "ready" });
    renderExpanded();
    await waitFor(() => expect(switchEl()).toBeEnabled());
    fireEvent.click(switchEl());
    await waitFor(() => expect(callsOf("dsh_switch_disable")).toHaveLength(1));
    expect(callsOf("dsh_switch_disable")[0][1]).toEqual({
      request: { instanceId: INSTANCE_ID },
    });
  });

  it("multi_instance_requires_explicit_selection：多实例要求先选择；开关查询按所选实例", async () => {
    installDefaultInvoke();
    instancesPayload = [
      instanceOf({ selected: false }),
      instanceOf({ instanceId: INSTANCE_B_ID, displayName: "第二台", selected: false }),
    ];
    switchPayload = switchOf({ instanceId: INSTANCE_B_ID, enabled: true, phase: "ready" });
    renderExpanded();
    await waitFor(() => expect(screen.getByTestId("dsh-instance-picker")).toBeInTheDocument());
    // 未选择前不查询开关
    expect(callsOf("dsh_switch_get")).toHaveLength(0);
    fireEvent.click(screen.getByTestId(`dsh-instance-${INSTANCE_B_ID}`));
    await waitFor(() => expect(callsOf("dsh_instance_select")).toHaveLength(1));
    await waitFor(() => {
      expect(
        callsOf("dsh_switch_get").some(
          (c) =>
            (c[1] as { request: { instanceId: string } } | undefined)?.request?.instanceId ===
            INSTANCE_B_ID
        )
      ).toBe(true);
    });
  });

  it("i18n zh/en 键集对账：卡片源码引用键双 locale 齐备", () => {
    const read = (p: string) =>
      JSON.parse(readFileSync(path.resolve(__dirname, p), "utf8")) as Record<string, unknown>;
    const zh = read("../../src/i18n/locales/zh.json");
    const en = read("../../src/i18n/locales/en.json");
    const panelSrc = readFileSync(
      path.resolve(__dirname, "../../src/components/settings/DshRemoteSwitchPanel.tsx"),
      "utf8"
    );
    const containerSrc = readFileSync(
      path.resolve(__dirname, "../../src/components/settings/DshRemoteSection.tsx"),
      "utf8"
    );
    const pick = (source: string) => {
      const keys = new Set<string>();
      for (const m of source.matchAll(/(?:settings\.remote\.)((?:dsh|errors)[A-Za-z0-9_]*)/g)) {
        keys.add(m[1]);
      }
      for (const m of source.matchAll(/"(dshStep_[a-z_]+)"/g)) keys.add(m[1]);
      return keys;
    };
    const errorsKeys = new Set<string>();
    for (const source of [panelSrc, containerSrc]) {
      for (const key of pick(source)) {
        const full =
          key.startsWith("dsh") && key.includes("_") && key.startsWith("dshStep_") ? key : key;
        for (const locale of [zh, en]) {
          const remote = (locale as Record<string, Record<string, unknown>>).settings.remote;
          expect(remote[full]).toBeDefined();
        }
      }
      for (const m of source.matchAll(/errors\.(dsh[A-Za-z0-9_]*)/g)) errorsKeys.add(m[1]);
    }
    for (const key of errorsKeys) {
      expect((zh as Record<string, Record<string, unknown>>).errors[key]).toBeDefined();
      expect((en as Record<string, Record<string, unknown>>).errors[key]).toBeDefined();
    }
  });
});

// ============================================================
// T01：与 Codex APP 同款工具卡（共用卡头 / 键盘 / 无障碍）
// ============================================================
describe("T01 DSH 工具卡", () => {
  it("默认收起，卡头是唯一的展开入口；鼠标与键盘都能展开", async () => {
    installDefaultInvoke();
    render(<DshRemoteSection />);
    const header = screen.getByTestId("tool-config-header-dsh");
    expect(header).toHaveAttribute("aria-expanded", "false");
    expect(header).toHaveAttribute("aria-controls", "tool-config-panel-dsh");
    // 未展开 = 正文不挂载
    expect(screen.queryByTestId("tool-config-panel-dsh")).toBeNull();
    expect(screen.queryByTestId("dsh-switch")).toBeNull();

    // 键盘：卡头是原生 button，Enter/Space 触发 click
    header.focus();
    fireEvent.keyDown(header, { key: "Enter", code: "Enter" });
    fireEvent.click(header); // jsdom 不合成原生 key->click，显式补上等价激活
    await waitFor(() => expect(header).toHaveAttribute("aria-expanded", "true"));
    expect(await screen.findByTestId("tool-config-panel-dsh")).toBeInTheDocument();
    expect(await screen.findByTestId("dsh-switch")).toBeInTheDocument();
  });

  it("收起时不产生选择/安装/作业副作用，再展开仍显示原作业状态", async () => {
    installDefaultInvoke();
    // 让卡内状态带一个失败作业，验证收起再展开不丢
    switchPayload = switchOf({
      enabled: false,
      phase: "failed",
      failedStep: "uninstall",
      reasonCode: "DSH_CLI_FAILED",
      job: jobOf({ kind: "uninstall", phase: "failed", reasonCode: "DSH_CLI_FAILED" }),
    });
    render(<DshRemoteSection />);
    const header = screen.getByTestId("tool-config-header-dsh");

    fireEvent.click(header);
    await waitFor(() => expect(header).toHaveAttribute("aria-expanded", "true"));
    expect(await screen.findByTestId("dsh-failed")).toBeInTheDocument();
    const callsBeforeCollapse = invokeMock.mock.calls.length;

    // 收起：正文卸载，且不得触发任何新命令
    fireEvent.click(header);
    await waitFor(() => expect(header).toHaveAttribute("aria-expanded", "false"));
    expect(screen.queryByTestId("tool-config-panel-dsh")).toBeNull();
    const afterCollapse = invokeMock.mock.calls.slice(callsBeforeCollapse);
    for (const cmd of [
      "dsh_switch_enable",
      "dsh_switch_disable",
      "dsh_switch_repair",
      "dsh_instance_select",
      "dsh_set_auto_adapt",
    ]) {
      expect(afterCollapse.filter((c) => c[0] === cmd)).toHaveLength(0);
    }

    // 再展开：原作业状态照常显示
    fireEvent.click(header);
    await waitFor(() => expect(header).toHaveAttribute("aria-expanded", "true"));
    expect(await screen.findByTestId("dsh-failed")).toBeInTheDocument();
  });

  it("页面上只有一张 DSH 卡，且保留单开关与卡内修复入口", async () => {
    installDefaultInvoke();
    render(<DshRemoteSection />);
    fireEvent.click(screen.getByTestId("tool-config-header-dsh"));
    await screen.findByTestId("dsh-switch");
    // 一张卡
    expect(document.querySelectorAll('[data-tool-card="dsh"]')).toHaveLength(1);
    // 单开关（没有第二个月于安装/卸载的主按钮）
    expect(screen.getAllByTestId("dsh-switch")).toHaveLength(1);
    // 修复入口仍在卡内
    expect(screen.getByTestId("dsh-repair-toggle")).toBeInTheDocument();
    // 发送就绪与控制台就绪分开显示（等开关查询落地后渲染）
    expect(await screen.findByTestId("dsh-send-readiness")).toBeInTheDocument();
    expect(await screen.findByTestId("dsh-console-readiness")).toBeInTheDocument();
  });
});
