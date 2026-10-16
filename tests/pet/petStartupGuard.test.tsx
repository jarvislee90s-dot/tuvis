import { beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { toast } from "sonner";
import { PetStartupGuard } from "@/components/pet/PetStartupGuard";
import { tauriInvokeMock } from "../msw/tauriMocks";

// pet-activation 的 repair 复用（mock 为可直接断言的桩）；buildManifestFromScan 同样桩化供直投用例断言
vi.mock("@/components/pet/petActivation", async (importOriginal) => {
  const orig = await importOriginal<typeof import("@/components/pet/petActivation")>();
  return {
    ...orig,
    repairManifest: vi.fn().mockResolvedValue({ hasVoice: false, displayName: "P" }),
    buildManifestFromScan: vi.fn().mockResolvedValue({ hasVoice: false, displayName: "P" }),
  };
});

const manifest = {
  id: "p1",
  displayName: "P",
  hasVoice: false,
  hasSubtitle: false,
  spriteVersionNumber: 2,
  spritesheetSizeBytes: 100,
  voices: [],
};
const scanOk = {
  id: "p1",
  dir: "/x/p1",
  spritesheet: { rel: "spritesheet.webp", exists: true, size: 100 },
  voiceFiles: [{ rel: "voice/done/new.mp3", exists: true, size: 5 }], // extra → issue
};

describe("PetStartupGuard（EP2 启动弹窗）", () => {
  beforeEach(() => {
    localStorage.clear();
    tauriInvokeMock.mockClear();
  });

  it("旧版无前缀 foxbell 指针（读取归一为内置）不弹窗", async () => {
    localStorage.setItem("mam-pet-active", "foxbell");
    const { container } = render(<PetStartupGuard />);
    await waitFor(() => expect(tauriInvokeMock).not.toHaveBeenCalled());
    expect(container.querySelector("[data-testid='pet-startup-dialog']")).toBeNull();
  });

  it("素材不一致 → 弹窗三选；点更新 → pet_update_manifest(backup=true)", async () => {
    localStorage.setItem("mam-pet-active", "p1");
    tauriInvokeMock.mockImplementation((cmd: string) => {
      if (cmd === "pet_scan") return Promise.resolve(scanOk);
      if (cmd === "pet_read_manifest") return Promise.resolve(manifest);
      return Promise.resolve(undefined);
    });
    render(<PetStartupGuard />);
    const dlg = await screen.findByTestId("pet-startup-dialog");
    expect(dlg).toBeInTheDocument();
    fireEvent.click(await screen.findByTestId("pet-startup-update"));
    await waitFor(() => {
      const call = tauriInvokeMock.mock.calls.find((c) => c[0] === "pet_update_manifest");
      expect(call?.[1]?.backup).toBe(true);
    });
  });

  it("图集缺失 → 致命弹窗（无更新按钮，只有切回/关闭）", async () => {
    localStorage.setItem("mam-pet-active", "p1");
    tauriInvokeMock.mockImplementation((cmd: string) => {
      if (cmd === "pet_scan")
        return Promise.resolve({
          ...scanOk,
          spritesheet: { rel: "spritesheet.webp", exists: false, size: 0 },
        });
      if (cmd === "pet_read_manifest") return Promise.resolve(manifest);
      return Promise.resolve(undefined);
    });
    render(<PetStartupGuard />);
    await screen.findByTestId("pet-startup-dialog");
    expect(screen.queryByTestId("pet-startup-update")).toBeNull();
    fireEvent.click(screen.getByTestId("pet-startup-foxbell"));
    await waitFor(() => expect(localStorage.getItem("mam-pet-active")).toBe("builtin-tuvis-3d"));
  });

  it("pet_scan reject（结构化 RpcError）→ 致命弹窗走 pet.rpc 映射文案（第六轮）", async () => {
    localStorage.setItem("mam-pet-active", "p1");
    // i18n 未初始化时 t() 返回键名：断言 fatal 行走映射键 pet.startup.fatalScan + pet.rpc.pet-not-found
    tauriInvokeMock.mockImplementation((cmd: string) => {
      if (cmd === "pet_scan")
        return Promise.reject({
          code: "pet-not-found",
          params: { id: "p1" },
          detail: "宠物不存在: p1",
        });
      return Promise.resolve(undefined);
    });
    render(<PetStartupGuard />);
    await screen.findByTestId("pet-startup-dialog");
    expect(screen.queryByTestId("pet-startup-update")).toBeNull(); // 致命分支无更新按钮
    expect(await screen.findByTestId("pet-startup-foxbell")).toBeInTheDocument();
    // 映射链路（第七轮前缀拆分）：RpcError 走 pet.startup.fatalScan 前缀，msg 内含 pet.rpc.pet-not-found。
    // 测试环境 i18next 未初始化时渲染为键名（插值未展开），映射语义由 petErrors.test.ts 钉死
    expect(screen.getByTestId("pet-startup-dialog").textContent).toContain("pet.startup.fatalScan");
  });

  it("图集缺失（PetError）→ 致命弹窗走原 pet.startup.fatal 前缀（第八轮分流）", async () => {
    localStorage.setItem("mam-pet-active", "p1");
    tauriInvokeMock.mockImplementation((cmd: string) => {
      if (cmd === "pet_scan")
        return Promise.resolve({
          id: "p1",
          dir: "/x/p1",
          spritesheet: { rel: "spritesheet.webp", exists: false, size: 0 },
          voiceFiles: [],
        });
      return Promise.resolve(undefined);
    });
    render(<PetStartupGuard />);
    await screen.findByTestId("pet-startup-dialog");
    expect(screen.queryByTestId("pet-startup-update")).toBeNull(); // 致命分支无更新按钮
    // PetError（sheet-missing）经 petErrMsg → pet.err.sheet-missing，前缀走原 fatal 而非 fatalScan
    expect(screen.getByTestId("pet-startup-dialog").textContent).toContain("pet.startup.fatal");
    expect(screen.getByTestId("pet-startup-dialog").textContent).not.toContain(
      "pet.startup.fatalScan"
    );
  });

  it("直投（无 manifest）更新 → buildManifestFromScan，字幕默认跟随 hasVoice（FIX-4）", async () => {
    localStorage.setItem("mam-pet-active", "p1");
    const { buildManifestFromScan } = await import("@/components/pet/petActivation");
    vi.mocked(buildManifestFromScan).mockClear();
    vi.mocked(buildManifestFromScan).mockResolvedValue({
      id: "p1",
      displayName: "p1",
      hasVoice: false,
      hasSubtitle: false, // 四组不全 → 无语音 → 无字幕
      spriteVersionNumber: 2,
      spritesheetSizeBytes: 100,
      voices: [],
    });
    tauriInvokeMock.mockImplementation((cmd: string) => {
      if (cmd === "pet_scan") return Promise.resolve(scanOk);
      if (cmd === "pet_read_manifest") return Promise.resolve(null); // 直投：manifest 缺失
      return Promise.resolve(undefined);
    });
    // stub Image：直投必走 probeSheetRows（manifest 无记录），jsdom 无法解码 → 直接回 v2 尺寸
    class FakeImage {
      onload: (() => void) | null = null;
      onerror: (() => void) | null = null;
      naturalWidth = 1536;
      naturalHeight = 2288;
      set src(_v: string) {
        queueMicrotask(() => this.onload?.());
      }
    }
    vi.stubGlobal("Image", FakeImage);
    try {
      render(<PetStartupGuard />);
      fireEvent.click(await screen.findByTestId("pet-startup-update"));
      await waitFor(() => {
        // buildManifestFromScan 以探测 rows / "folder" source / 字幕默认 true 被调用
        expect(buildManifestFromScan).toHaveBeenCalledWith("p1", scanOk, 11, "folder", true);
        const call = tauriInvokeMock.mock.calls.find((c) => c[0] === "pet_update_manifest");
        expect(call?.[1]?.backup).toBe(false); // 直投首写不备份
      });
    } finally {
      vi.unstubAllGlobals();
    }
  });
});
describe("PetStartupGuard 更新失败处理（issue #33-1）", () => {
  beforeEach(() => {
    localStorage.clear();
    tauriInvokeMock.mockClear();
  });

  it("pet_update_manifest reject → toast 错误、弹窗保持、issues 不清理（不再静默吞错）", async () => {
    localStorage.setItem("mam-pet-active", "p1");
    tauriInvokeMock.mockImplementation((cmd: string) => {
      if (cmd === "pet_scan") return Promise.resolve(scanOk);
      if (cmd === "pet_read_manifest") return Promise.resolve(manifest);
      if (cmd === "pet_update_manifest") return Promise.reject({ code: "manifest-write-failed" });
      return Promise.resolve(undefined);
    });
    const errSpy = vi.spyOn(toast, "error").mockImplementation(() => {});
    try {
      render(<PetStartupGuard />);
      fireEvent.click(await screen.findByTestId("pet-startup-update"));
      await waitFor(() => expect(errSpy).toHaveBeenCalled());
      // 弹窗保持打开且更新按钮仍在（issues 未被清理，用户可重试或改选其它处理）
      expect(screen.getByTestId("pet-startup-dialog")).toBeInTheDocument();
      expect(screen.getByTestId("pet-startup-update")).toBeInTheDocument();
    } finally {
      errSpy.mockRestore();
    }
  });
});
