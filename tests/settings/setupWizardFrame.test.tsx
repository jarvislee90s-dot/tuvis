import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";

const { invokeMock, successMock } = vi.hoisted(() => ({
  invokeMock: vi.fn(),
  successMock: vi.fn(),
}));
vi.mock("@tauri-apps/api/core", () => ({ invoke: invokeMock }));
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(async () => () => {}),
  emit: vi.fn(async () => {}),
}));
vi.mock("sonner", () => ({ toast: { success: successMock, error: vi.fn() } }));
import "@/i18n";
import { SetupWizardFrame } from "@/components/settings/SetupWizardFrame";

const history = JSON.stringify({ configKey: "saved", address: "https://saved.example.com/m" });
const frame = (configKey: string | null, storageKey: string, complete = false) => (
  <SetupWizardFrame
    configKey={configKey}
    storageKey={storageKey}
    complete={complete}
    address={complete ? "https://saved.example.com/m" : null}
    issue={false}
  >
    <p>configuration steps</p>
  </SetupWizardFrame>
);

beforeEach(() => {
  invokeMock.mockReset();
  successMock.mockReset();
  invokeMock.mockImplementation(async (cmd: string) => (cmd === "get_setting" ? history : null));
});

describe("完成摘要的历史与配置边界", () => {
  it("重启探测尚未返回配置标识时保持历史折叠，并明确地址暂不可用", async () => {
    render(frame(null, "test-history-unknown"));
    await screen.findByText("https://saved.example.com/m");
    expect(screen.queryByText("configuration steps")).toBeNull();
    expect(screen.getByText(/temporarily unavailable/i)).toBeTruthy();
    expect(screen.queryByRole("link", { name: "Open board" })).toBeNull();
  });

  it("未知配置期间主动展开，随后确认仍为相同配置时不折叠", async () => {
    const view = render(frame(null, "test-history-manual"));
    fireEvent.click(await screen.findByRole("button", { name: "View setup steps" }));
    view.rerender(frame("saved", "test-history-manual"));
    await waitFor(() => expect(screen.getByText("configuration steps")).toBeTruthy());
    expect(screen.getByRole("button", { name: "Hide details" })).toHaveAttribute(
      "aria-expanded",
      "true"
    );
  });

  it("历史配置与当前配置不同时展开新配置，不能保留旧地址", async () => {
    const view = render(frame(null, "test-history-changed"));
    await screen.findByText("https://saved.example.com/m");
    view.rerender(frame("new-config", "test-history-changed"));
    expect(await screen.findByText("configuration steps")).toBeTruthy();
    expect(screen.queryByText("https://saved.example.com/m")).toBeNull();
  });

  it("首次完成只通知一次地址位置，手动展开后状态刷新不收起", async () => {
    invokeMock.mockImplementation(async () => null);
    const view = render(frame("saved", "test-history-success", true));
    await screen.findByText("Setup complete, your fixed address is ready");
    await waitFor(() => expect(successMock).toHaveBeenCalledTimes(1));
    expect(successMock).toHaveBeenCalledWith(
      "Setup complete, the address is shown on this channel card"
    );
    fireEvent.click(screen.getByRole("button", { name: "View setup steps" }));
    view.rerender(frame("saved", "test-history-success", true));
    expect(screen.getByText("configuration steps")).toBeTruthy();
    expect(successMock).toHaveBeenCalledTimes(1);
  });

  it("复验在途时摘要给出过程提示；完成或需要排查时不显示（过程≠状态）", async () => {
    const pending = (
      <SetupWizardFrame
        configKey="saved"
        storageKey="test-reverify"
        complete={false}
        address={null}
        issue={false}
        reverifyPending
      >
        <p>configuration steps</p>
      </SetupWizardFrame>
    );
    const view = render(pending);
    await screen.findByText("https://saved.example.com/m");
    expect(screen.getByTestId("setup-reverify-hint").textContent).toContain(
      "automatically re-checking"
    );
    // 完成 → 复验结束，过程提示退场（状态行已变绿色可用）
    view.rerender(
      <SetupWizardFrame
        configKey="saved"
        storageKey="test-reverify"
        complete
        address="https://saved.example.com/m"
        issue={false}
        reverifyPending
      >
        <p>configuration steps</p>
      </SetupWizardFrame>
    );
    expect(screen.queryByTestId("setup-reverify-hint")).toBeNull();
    // 需要排查 → 有更具体的诊断行，不再叠加复验提示
    view.rerender(
      <SetupWizardFrame
        configKey="saved"
        storageKey="test-reverify"
        complete={false}
        address={null}
        issue
        reverifyPending
      >
        <p>configuration steps</p>
      </SetupWizardFrame>
    );
    expect(screen.queryByTestId("setup-reverify-hint")).toBeNull();
  });
});
