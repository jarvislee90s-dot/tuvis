import { createHash } from "node:crypto";
import { existsSync, readdirSync, readFileSync } from "node:fs";
import { resolve } from "node:path";
import { parseManifest } from "@/components/pet/petVoices";
import { BUILTIN_PETS } from "@/components/pet/petRuntime";

const PET_ROOT = resolve(__dirname, "../../public/pet");
/** public/pet/ 下的直接子目录 = 内置宠物静态目录（README 等说明文档不入库，已被 gitignore） */
const assetDirs = readdirSync(PET_ROOT, { withFileTypes: true })
  .filter((d) => d.isDirectory())
  .map((d) => d.name)
  .sort();

describe("内置宠物统一目录结构", () => {
  it("注册表与磁盘目录一一对应（新增内置宠物两侧都要落）", () => {
    expect(assetDirs).toEqual(BUILTIN_PETS.map((p) => p.id.replace(/^builtin-/, "")).sort());
  });

  it("每个内置目录同构：manifest + 图集 + voice 清单逐条落盘", () => {
    for (const dir of assetDirs) {
      const root = resolve(PET_ROOT, dir);
      expect(existsSync(resolve(root, "manifest.json"))).toBe(true);
      expect(existsSync(resolve(root, "spritesheet.webp"))).toBe(true);
      const entries = parseManifest(
        JSON.parse(readFileSync(resolve(root, "manifest.json"), "utf8"))
      );
      expect(entries.length).toBeGreaterThan(0);
      // 四个语音组齐全（VoicePlayer 的组契约）
      expect(new Set(entries.map((e) => e.group))).toEqual(
        new Set(["general", "approval", "done", "error"])
      );
      for (const entry of entries) {
        expect(entry.file).toBe(`${entry.group}/${entry.name}.m4a`);
        expect(existsSync(resolve(root, "voice", entry.file))).toBe(true);
      }
    }
  });
});

describe("Foxbell 首发素材", () => {
  const root = resolve(PET_ROOT, "foxbell");
  const raw = JSON.parse(readFileSync(resolve(root, "manifest.json"), "utf-8")) as {
    index: number;
    group: string;
    name: string;
    file: string;
  }[];

  it("manifest 含四个组且原始索引连续", () => {
    expect(new Set(raw.map((v) => v.group))).toEqual(
      new Set(["general", "approval", "done", "error"])
    );
    expect(raw.map((v) => v.index)).toEqual(raw.map((_, i) => i));
  });

  it("31 条语音文件真实存在且文件名去扩展即 name", () => {
    expect(raw).toHaveLength(31);
    for (const v of raw) {
      expect(existsSync(resolve(root, "voice", v.file))).toBe(true);
      expect(v.name).toBe(
        v.file
          .replace(/\.(m4a|mp4)$/i, "")
          .split("/")
          .pop()
      );
    }
  });
});

describe("tuvis-3D 第一版素材", () => {
  const root = resolve(PET_ROOT, "tuvis-3d");

  it("使用已确认的 running 缩小 20% 图集", () => {
    const sheet = readFileSync(resolve(root, "spritesheet.webp"));
    expect(createHash("sha256").update(sheet).digest("hex")).toBe(
      "b9a49dfef97624686877a559d6fe988ca76cab8f83a8d05bb54c8ab1f24c5ef8"
    );
  });

  it("四组各四句，字幕与文件名一致，全部音频存在且为 m4a", () => {
    const entries = parseManifest(JSON.parse(readFileSync(resolve(root, "manifest.json"), "utf8")));
    expect(entries).toHaveLength(16);
    for (const group of ["general", "approval", "done", "error"]) {
      expect(entries.filter((entry) => entry.group === group)).toHaveLength(4);
    }
    for (const entry of entries) {
      expect(entry.file).toBe(`${entry.group}/${entry.name}.m4a`);
      expect(existsSync(resolve(root, "voice", entry.file))).toBe(true);
    }
  });
});
