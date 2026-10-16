import { describe, expect, it, vi } from "vitest";
import {
  parseManifest,
  pickIndex,
  subtitleMs,
  VoicePlayer,
  type VoiceEntry,
} from "@/components/pet/petVoices";

describe("petVoices", () => {
  it("parseManifest：组序重排索引、组内 zh 排序、忽略非法项", () => {
    const raw = [
      { group: "done", name: "b", file: "done/b.m4a" },
      { group: "general", name: "乙", file: "general/乙.m4a" },
      { group: "general", name: "甲", file: "general/甲.m4a" },
      { group: "hack", name: "x", file: "hack/x.m4a" },
      null,
    ];
    const out = parseManifest(raw);
    expect(out.map((v) => v.index)).toEqual([0, 1, 2]);
    expect(out[0].group).toBe("general");
    expect(out[0].name).toBe("甲"); // zh 排序：甲 < 乙
    expect(out[2].group).toBe("done");
  });

  it("pickIndex：不连续重复；边界", () => {
    expect(pickIndex(0, -1)).toBe(-1);
    expect(pickIndex(1, 0)).toBe(0);
    for (let i = 0; i < 50; i++) {
      const idx = pickIndex(3, 1);
      expect(idx).not.toBe(1);
      expect(idx).toBeGreaterThanOrEqual(0);
      expect(idx).toBeLessThanOrEqual(2);
    }
  });

  it("subtitleMs：与音频时长对齐，最短 2.5s（spec E4）", () => {
    expect(subtitleMs(0)).toBe(2500);
    expect(subtitleMs(NaN)).toBe(2500);
    expect(subtitleMs(1.2)).toBe(2500); // 1200+250=1450 < 2500
    expect(subtitleMs(4)).toBe(4250);
  });
});

describe("VoicePlayer resolveUrl（外部宠物 blob 快照，spec EP6）", () => {
  it("load 可注入自定义 URL 解析器", () => {
    const player = new VoicePlayer();
    const entries: VoiceEntry[] = [
      { index: 0, group: "general", name: "a", file: "voice/general/a.m4a" },
    ];
    player.load(entries, (f) => `blob://${f}`);
    // jsdom Audio 不可真实加载，仅验证不抛错且 pick 正常
    const e = player.pick("general");
    expect(e?.file).toBe("voice/general/a.m4a");
    player.dispose();
  });
  it("load 必须注入解析器：预载 Audio 的 src 由注入值决定", () => {
    const made: string[] = [];
    class FakeAudio {
      preload = "";
      pause() {}
      load() {}
      constructor(src: string) {
        made.push(src);
      }
    }
    vi.stubGlobal("Audio", FakeAudio);
    try {
      const player = new VoicePlayer();
      player.load([{ index: 0, group: "general", name: "a", file: "x.m4a" }], (f) => `blob://${f}`);
      expect(made).toEqual(["blob://x.m4a"]);
      player.dispose();
    } finally {
      vi.unstubAllGlobals();
    }
  });
});
