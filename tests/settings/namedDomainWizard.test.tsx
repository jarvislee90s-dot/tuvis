import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";

const { invokeMock } = vi.hoisted(() => ({ invokeMock: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: invokeMock }));
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(async () => () => {}),
  emit: vi.fn(async () => {}),
}));

import i18n from "@/i18n";
import { NamedDomainWizard } from "@/components/settings/NamedDomainWizard";
void i18n;

const named = (over: Record<string, unknown> = {}) => ({
  configKey: "named-config-a",
  hasToken: true,
  enabled: true,
  running: true,
  connected: false,
  originUrl: "http://127.0.0.1:9420",
  address: "https://remote.example.com/m",
  lastAddress: null,
  reach: { state: "unverified" },
  waiting: null,
  ...over,
});

beforeEach(() => {
  invokeMock.mockReset();
  invokeMock.mockImplementation(async (cmd: string) => {
    if (cmd === "get_setting") return null;
    if (cmd === "remote_named_probe") return named();
    return null;
  });
});

describe("NamedDomainWizard Cloudflare timeline", () => {
  it("an older probe cannot replace the current configuration", async () => {
    let resolveOld: (value: unknown) => void = () => {};
    let reads = 0;
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd !== "remote_named_probe") return null;
      reads += 1;
      if (reads === 1)
        return await new Promise((resolve) => {
          resolveOld = resolve;
        });
      return named({ configKey: "named-new", address: "https://new.example.com/m" });
    });
    const view = render(<NamedDomainWizard hasToken refreshKey="old" />);
    view.rerender(<NamedDomainWizard hasToken refreshKey="new" />);
    const input = await screen.findByRole("textbox", { name: "Public address" });
    await waitFor(() => expect(input).toHaveValue("https://new.example.com/m"));
    await act(async () =>
      resolveOld(named({ configKey: "named-old", address: "https://old.example.com/m" }))
    );
    expect(input).toHaveValue("https://new.example.com/m");
  });

  it("an old verify receipt cannot make a different configuration available", async () => {
    let resolveVerify: (value: unknown) => void = () => {};
    let current = named({ configKey: "named-action-old", connected: true });
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "remote_named_probe") return current;
      if (cmd === "remote_named_verify")
        return await new Promise((resolve) => {
          resolveVerify = resolve;
        });
      return null;
    });
    const view = render(<NamedDomainWizard hasToken refreshKey="old" />);
    fireEvent.click(await screen.findByRole("button", { name: "Verify address" }));
    current = named({
      configKey: "named-action-new",
      connected: true,
      address: "https://new.example.com/m",
    });
    view.rerender(<NamedDomainWizard hasToken refreshKey="new" />);
    await waitFor(() =>
      expect(screen.getByRole("textbox", { name: "Public address" })).toHaveValue(
        "https://new.example.com/m"
      )
    );
    await act(async () =>
      resolveVerify(
        named({ configKey: "named-action-old", connected: true, reach: { state: "verified" } })
      )
    );
    expect(screen.queryByRole("link", { name: "Open board" })).toBeNull();
    expect(screen.getByRole("textbox", { name: "Public address" })).toHaveValue(
      "https://new.example.com/m"
    );
  });
  it("connector running without registered connection cannot become available or verify", async () => {
    render(<NamedDomainWizard hasToken refreshKey="a" />);
    await screen.findByTestId("named-wizard");
    expect(screen.queryByText("Setup complete, your fixed address is ready")).toBeNull();
    expect(screen.getByRole("button", { name: "Verify address" })).toBeDisabled();
    expect(invokeMock).not.toHaveBeenCalledWith("remote_named_verify");
  });

  it("only actual connected, running and verified state stores successful config history", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "get_setting") return null;
      if (cmd === "remote_named_probe")
        return named({ connected: true, reach: { state: "verified" } });
      return null;
    });
    render(<NamedDomainWizard hasToken refreshKey="b" />);
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith("set_setting", {
        key: "remote.setup.named",
        value: JSON.stringify({
          configKey: "named-config-a",
          address: "https://remote.example.com/m",
        }),
      })
    );
    await waitFor(() => expect(screen.queryByTestId("named-wizard")).toBeNull());
    expect(await screen.findByText("Setup complete, your fixed address is ready")).toBeTruthy();
    expect(screen.getByRole("link", { name: "Open board" })).toBeTruthy();
  });

  it("renders the current origin URL for copying after the external binding step", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "get_setting") return null;
      if (cmd === "remote_named_probe") return named({ configKey: `named-origin-${Date.now()}` });
      return null;
    });
    render(<NamedDomainWizard hasToken refreshKey="c" />);
    expect(await screen.findByText("http://127.0.0.1:9420")).toBeTruthy();
    expect(screen.getByRole("button", { name: /Copy origin URL/i })).toBeTruthy();
  });

  it("DNS propagation waiting hides the verify action and shows backend wait age", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "get_setting") return null;
      if (cmd === "remote_named_probe")
        return named({
          configKey: "named-dns-config",
          connected: true,
          reach: { state: "record_pending" },
          waiting: { startedAtMs: Date.now() - 120_000, expectedSeconds: null },
        });
      return null;
    });
    render(<NamedDomainWizard hasToken refreshKey="pending" />);
    const waiting = await screen.findByTestId("named-waiting");
    expect(waiting.textContent).toMatch(/Waiting [12] minutes/i);
    expect(screen.queryByRole("button", { name: "Verify address" })).toBeNull();
  });

  it("a background verification cycle keeps DNS waiting and hides another verify action", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "get_setting") return null;
      if (cmd === "remote_named_probe")
        return named({
          configKey: "named-verifying-config",
          connected: true,
          reach: { state: "verifying" },
          waiting: { startedAtMs: Date.now() - 120_000, expectedSeconds: null },
        });
      return null;
    });
    render(<NamedDomainWizard hasToken refreshKey="named-verifying" />);
    expect(await screen.findByTestId("named-waiting")).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Verify address" })).toBeNull();
  });

  it("a status refresh does not replace the hostname while the user is editing it", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "get_setting") return null;
      if (cmd === "remote_named_probe") return named({ configKey: "named-edit-config" });
      return null;
    });
    const view = render(<NamedDomainWizard hasToken refreshKey="edit-1" />);
    const input = (await screen.findByRole("textbox", {
      name: "Public address",
    })) as HTMLInputElement;
    fireEvent.change(input, { target: { value: "https://draft.example.com/m" } });
    view.rerender(<NamedDomainWizard hasToken refreshKey="edit-2" />);
    await waitFor(() =>
      expect(
        invokeMock.mock.calls.filter((call) => call[0] === "remote_named_probe").length
      ).toBeGreaterThan(1)
    );
    expect(input.value).toBe("https://draft.example.com/m");
  });
});

describe("NamedDomainWizard 复验过程提示（状态≠过程）", () => {
  const historyValue = JSON.stringify({
    configKey: "named-config-a",
    address: "https://remote.example.com/m",
  });

  it("已连接且运行、只差校验证据 → 摘要给自动复验提示；未连接不给", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "get_setting") return historyValue;
      if (cmd === "remote_named_probe") return named({ connected: true });
      return null;
    });
    const view = render(<NamedDomainWizard hasToken refreshKey="reverify-1" />);
    await screen.findByTestId("setup-reverify-hint");
    // 连接器未注册连接（connected=false）→ 不承诺「正在复验」
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "get_setting") return historyValue;
      if (cmd === "remote_named_probe") return named({ connected: false });
      return null;
    });
    view.rerender(<NamedDomainWizard hasToken refreshKey="reverify-2" />);
    await waitFor(() => expect(screen.queryByTestId("setup-reverify-hint")).toBeNull());
  });
});
