import { beforeEach, describe, expect, it, vi } from "vitest";
import {
  loadActiveId,
  saveActiveId,
  loadVoiceCap,
  rowsFromSize,
  BUILTIN_PETS,
  DEFAULT_PET,
  builtinPet,
  loadFlashSwitched,
  saveFlashSwitched,
  clearFlashSwitched,
} from "@/components/pet/petRuntime";

describe("petRuntime", () => {
  beforeEach(() => localStorage.clear());

  it("激活指针：默认 tuvis-3D，可读写", () => {
    expect(loadActiveId()).toBe("builtin-tuvis-3d");
    saveActiveId("starry-dew", false, "Starry Dew");
    expect(loadActiveId()).toBe("starry-dew");
    expect(loadVoiceCap()).toBe(false);
  });

  it("闪切持久标记：默认无标记，可读写与清除（回环修复）", () => {
    expect(loadFlashSwitched()).toBeNull();
    saveFlashSwitched("starry-dew");
    expect(loadFlashSwitched()).toBe("starry-dew");
    clearFlashSwitched();
    expect(loadFlashSwitched()).toBeNull();
  });

  it("rowsFromSize：v1/v2 识别与非法尺寸（EP1）", () => {
    expect(rowsFromSize(1536, 1872)).toBe(9);
    expect(rowsFromSize(1536, 2288)).toBe(11);
    expect(rowsFromSize(1536, 1000)).toBeNull();
    expect(rowsFromSize(1024, 1872)).toBeNull();
  });

  it("内置注册表：id=builtin-<目录名>，各指向自己的同构目录，全能力无快照", () => {
    expect(BUILTIN_PETS.map((p) => p.id)).toEqual(["builtin-tuvis-3d", "builtin-foxbell"]);
    for (const pet of BUILTIN_PETS) {
      const dir = `/pet/${pet.id.replace(/^builtin-/, "")}`;
      expect(pet.spritesheetUrl).toBe(`${dir}/spritesheet.webp`);
      expect(pet.resolveVoiceUrl("a.m4a")).toBe(`${dir}/voice/${encodeURI("a.m4a")}`);
      expect(pet.rows).toBe(11);
      expect(pet.hasVoice).toBe(true);
      expect(pet.dispose).toBeUndefined(); // 内置无 blob 快照，无需回收
    }
    expect(DEFAULT_PET.id).toBe("builtin-tuvis-3d"); // 默认宠物由注册表参数指定
    expect(builtinPet("starry-dew")).toBeUndefined(); // 外部 id 不命中
  });

  it("旧版激活指针兼容：无前缀 foxbell 读取时归一为 builtin-foxbell（写入恒为新 id）", () => {
    localStorage.setItem("mam-pet-active", "foxbell");
    expect(loadActiveId()).toBe("builtin-foxbell");
    expect(builtinPet(loadActiveId())?.displayName).toBe("Foxbell");
    saveActiveId("builtin-foxbell", true, "Foxbell");
    expect(localStorage.getItem("mam-pet-active")).toBe("builtin-foxbell");
  });

  describe("音频快照 blob URL 生命周期（spec §7/EP6）", () => {
    beforeEach(() => {
      // fetch blob 快照链路 mock：ok + blob()
      vi.stubGlobal(
        "fetch",
        vi.fn(async () => ({ ok: true, blob: async () => ({}) }))
      );
    });
    afterEach(() => {
      vi.unstubAllGlobals();
      vi.restoreAllMocks();
    });

    it("resolveActivePet：dispose 撤销全部创建的 objectURL", async () => {
      // stub Image：probeSheetRows 走 new Image().src 加载，jsdom 无解码能力 → 直接回 1536×2288
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
      // jsdom（Node 后端）的 createObjectURL 要求真 Blob：spy 拦截并返回合成 URL
      let seq = 0;
      vi.spyOn(URL, "createObjectURL").mockImplementation(() => `blob:mock-${++seq}`);
      const revokeSpy = vi.spyOn(URL, "revokeObjectURL").mockImplementation(() => {});
      const { resolveActivePet, saveActiveId: save } = await import("@/components/pet/petRuntime");
      save("p1", true, "P1");
      const { tauriInvokeMock } = await import("../msw/tauriMocks");
      tauriInvokeMock.mockImplementation((cmd: string) => {
        if (cmd === "pet_scan")
          return Promise.resolve({
            id: "p1",
            dir: "/x/p1",
            spritesheet: { rel: "spritesheet.webp", exists: true, size: 1 },
            voiceFiles: [],
          });
        if (cmd === "pet_read_manifest")
          return Promise.resolve({
            id: "p1",
            displayName: "P",
            hasVoice: true,
            hasSubtitle: true,
            spriteVersionNumber: 2,
            voices: [
              {
                group: "general",
                name: "a",
                file: "voice/general/a.m4a",
                sizeBytes: 1,
                durationMs: 3000,
              },
              {
                group: "done",
                name: "b",
                file: "voice/done/b.m4a",
                sizeBytes: 1,
                durationMs: 3000,
              },
            ],
          });
        return Promise.resolve(undefined);
      });
      const pet = await resolveActivePet();
      expect(pet.rows).toBe(11); // FakeImage 1536×2288 → v2
      expect(pet.voices).toHaveLength(2); // 两条语音 → 两个 blob URL
      expect(pet.dispose).toBeTypeOf("function");
      pet.dispose!();
      expect(revokeSpy).toHaveBeenCalledTimes(2);
      tauriInvokeMock.mockRestore();
    });
  });

  describe("探测超时与能力回写（issue #33-9/#33-10）", () => {
    afterEach(() => {
      vi.unstubAllGlobals();
      vi.restoreAllMocks();
    });

    it("probeSheetRows：图集永不加载 → 超时拒绝（sheet-timeout），activatePet 不再永久 busy", async () => {
      class StalledImage {
        onload: (() => void) | null = null;
        onerror: (() => void) | null = null;
        set src(_v: string) {
          /* 永不回调 */
        }
      }
      vi.stubGlobal("Image", StalledImage);
      const { probeSheetRows } = await import("@/components/pet/petRuntime");
      await expect(probeSheetRows("x://sheet", 20)).rejects.toMatchObject({
        code: "sheet-timeout",
      });
    });

    it("resolveActivePet：快照整体失败 → 运行时无语音且回写 voice-cap=0（spec §5.2 回落契约）", async () => {
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
      // fetch blob 失败（文件锁定/同大小损坏）→ snapshotVoices 整体降级 null
      vi.stubGlobal(
        "fetch",
        vi.fn(async () => ({ ok: false, blob: async () => ({}) }))
      );
      const { resolveActivePet, loadVoiceCap } = await import("@/components/pet/petRuntime");
      const { tauriInvokeMock: mock } = await import("../msw/tauriMocks");
      mock.mockReset();
      saveActiveId("p1", true, "P1"); // 激活时缓存 voice-cap=1
      mock.mockImplementation((cmd: string) => {
        if (cmd === "pet_scan")
          return Promise.resolve({
            id: "p1",
            dir: "/x/p1",
            spritesheet: { rel: "spritesheet.webp", exists: true, size: 1 },
            voiceFiles: [],
          });
        if (cmd === "pet_read_manifest")
          return Promise.resolve({
            id: "p1",
            displayName: "P",
            hasVoice: true,
            hasSubtitle: true,
            spriteVersionNumber: 2,
            voices: [
              {
                group: "general",
                name: "a",
                file: "voice/general/a.m4a",
                sizeBytes: 1,
                durationMs: 3000,
              },
            ],
          });
        return Promise.resolve(undefined);
      });
      const pet = await resolveActivePet();
      expect(pet.hasVoice).toBe(false); // 快照失败降级
      expect(loadVoiceCap()).toBe(false); // 缓存同步回写，petSoundTakeover 不再误判
    });

    it("resolveActivePet：语音文件恢复后快照成功 → voice-cap 回升 1（双向同步，评审 N1）", async () => {
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
      vi.stubGlobal(
        "fetch",
        vi.fn(async () => ({ ok: true, blob: async () => ({}) }))
      );
      vi.spyOn(URL, "createObjectURL").mockImplementation(() => "blob:mock-ok");
      vi.spyOn(URL, "revokeObjectURL").mockImplementation(() => {});
      const { resolveActivePet, loadVoiceCap } = await import("@/components/pet/petRuntime");
      const { tauriInvokeMock: mock } = await import("../msw/tauriMocks");
      mock.mockReset();
      saveActiveId("p1", false, "P1"); // 之前 ignore 降级缓存了 voice-cap=0
      mock.mockImplementation((cmd: string) => {
        if (cmd === "pet_scan")
          return Promise.resolve({
            id: "p1",
            dir: "/x/p1",
            spritesheet: { rel: "spritesheet.webp", exists: true, size: 1 },
            voiceFiles: [],
          });
        if (cmd === "pet_read_manifest")
          return Promise.resolve({
            id: "p1",
            displayName: "P",
            hasVoice: true,
            hasSubtitle: true,
            spriteVersionNumber: 2,
            voices: [
              {
                group: "general",
                name: "a",
                file: "voice/general/a.m4a",
                sizeBytes: 1,
                durationMs: 3000,
              },
            ],
          });
        return Promise.resolve(undefined);
      });
      const pet = await resolveActivePet();
      expect(pet.hasVoice).toBe(true); // 快照成功
      expect(loadVoiceCap()).toBe(true); // 缓存回升，主看板提示音接管恢复
    });
  });
});
