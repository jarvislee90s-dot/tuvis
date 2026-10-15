import { beforeEach, describe, expect, it, vi } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
import {
  loadActiveId,
  loadActiveName,
  resolveActivePet,
  saveActiveId,
} from "@/components/pet/petRuntime";
import { activatePet } from "@/components/pet/petActivation";
import { PetSwitchDialog } from "@/components/pet/manage/PetSwitchDialog";
import { PetStartupGuard } from "@/components/pet/PetStartupGuard";
import { tauriInvokeMock } from "../msw/tauriMocks";
import { activePetSheet } from "@/lib/usage/sheet";

describe("tuvis 内置默认宠物", () => {
  beforeEach(() => {
    localStorage.clear();
    tauriInvokeMock.mockReset();
    tauriInvokeMock.mockResolvedValue([]);
  });

  it("首次使用默认 tuvis-3D，显式选择 Foxbell 仍保留", () => {
    expect(loadActiveId()).toBe("builtin-tuvis-3d");
    expect(loadActiveName()).toBe("tuvis-3D");
    saveActiveId("builtin-foxbell", true, "Foxbell");
    expect(loadActiveId()).toBe("builtin-foxbell");
    expect(loadActiveName()).toBe("Foxbell");
  });

  it("激活 tuvis 不扫描外部目录", async () => {
    const confirm = vi.fn();
    expect((await activatePet("builtin-tuvis-3d", confirm)).status).toBe("activated");
    expect(loadActiveId()).toBe("builtin-tuvis-3d");
    expect(loadActiveName()).toBe("tuvis-3D");
    expect(tauriInvokeMock).not.toHaveBeenCalled();
    expect(confirm).not.toHaveBeenCalled();
  });

  it("解析 tuvis 使用独立静态素材与语音清单", async () => {
    saveActiveId("builtin-tuvis-3d", true, "tuvis-3D");
    const fetcher = vi.fn().mockResolvedValue({
      ok: true,
      json: async () => [{ index: 0, group: "general", name: "你好", file: "general/你好.m4a" }],
    });
    vi.stubGlobal("fetch", fetcher);
    try {
      const pet = await resolveActivePet();
      expect(pet.spritesheetUrl).toBe("/pet/tuvis-3d/spritesheet.webp");
      expect(pet.voices).toHaveLength(1);
      expect(pet.resolveVoiceUrl("general/你好.m4a")).toBe(
        `/pet/tuvis-3d/voice/${encodeURI("general/你好.m4a")}`
      );
      expect(fetcher).toHaveBeenCalledWith("/pet/tuvis-3d/manifest.json");
      expect(tauriInvokeMock).not.toHaveBeenCalled();
    } finally {
      vi.unstubAllGlobals();
    }
  });

  it("切换列表 tuvis 第一、Foxbell 第二，缩略图分别指向自己的图集", async () => {
    render(<PetSwitchDialog open onOpenChange={() => {}} />);
    const tuvis = await screen.findByTestId("pet-card-builtin-tuvis-3d");
    const foxbell = screen.getByTestId("pet-card-builtin-foxbell");
    const cards = screen.getByTestId("pet-switch-list").querySelectorAll("button");
    expect(cards[0]).toBe(tuvis);
    expect(cards[1]).toBe(foxbell);
    expect(tuvis.querySelector("[style]")?.getAttribute("style")).toContain(
      "/pet/tuvis-3d/spritesheet.webp"
    );
    expect(foxbell.querySelector("[style]")?.getAttribute("style")).toContain(
      "/pet/foxbell/spritesheet.webp"
    );
  });

  it("默认内置宠物启动时不触发外部素材扫描", async () => {
    render(<PetStartupGuard />);
    await waitFor(() => expect(tauriInvokeMock).not.toHaveBeenCalled());
    expect(screen.queryByTestId("pet-startup-dialog")).toBeNull();
  });

  it("用量图导出使用激活内置宠物的图集，不扫描外部目录", async () => {
    expect(await activePetSheet()).toEqual({ url: "/pet/tuvis-3d/spritesheet.webp", rows: 11 });
    saveActiveId("builtin-foxbell", true, "Foxbell");
    expect(await activePetSheet()).toEqual({ url: "/pet/foxbell/spritesheet.webp", rows: 11 });
    expect(tauriInvokeMock).not.toHaveBeenCalled();
  });

  it("旧版导入的 tuvis-3d 仍按外部宠物解析，不被新内置覆盖", async () => {
    saveActiveId("tuvis-3d", false, "旧版兔维斯");
    class FakeImage {
      onload: (() => void) | null = null;
      naturalWidth = 1536;
      naturalHeight = 2288;
      set src(_value: string) {
        queueMicrotask(() => this.onload?.());
      }
    }
    vi.stubGlobal("Image", FakeImage);
    tauriInvokeMock.mockImplementation(async (cmd: string) =>
      cmd === "pet_scan"
        ? { id: "tuvis-3d", dir: "/old/tuvis-3d", spritesheet: { exists: true }, voiceFiles: [] }
        : null
    );
    try {
      const pet = await resolveActivePet();
      expect(pet.id).toBe("tuvis-3d");
      expect(pet.spritesheetUrl).toContain("/old/tuvis-3d/spritesheet.webp");
      expect(loadActiveId()).toBe("tuvis-3d");
      expect(tauriInvokeMock).toHaveBeenCalledWith("pet_scan", { id: "tuvis-3d" });
    } finally {
      vi.unstubAllGlobals();
    }
  });
});
