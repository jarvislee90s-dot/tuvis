// scripts/copy-pet-assets.mjs — 从原插件仓库搬运素材并生成 manifest（一次性，spec §6.1）
// 用法: node scripts/copy-pet-assets.mjs <源assets目录>
import { cpSync, mkdirSync, readdirSync, statSync, writeFileSync } from "node:fs";
import { join, relative, resolve } from "node:path";

const SRC = resolve(process.argv[2] ?? "");
const DEST = resolve("public/pet");
const GROUPS = ["general", "approval", "done", "error"]; // spec §6.1 映射表顺序

if (!statSync(join(SRC, "spritesheet.webp")).isFile()) {
  throw new Error(`源目录无效: ${SRC}`);
}
mkdirSync(DEST, { recursive: true });
cpSync(join(SRC, "spritesheet.webp"), join(DEST, "spritesheet.webp"));

const manifest = [];
let index = 0;
for (const group of GROUPS) {
  const dir = join(SRC, "voice", group);
  const files = readdirSync(dir)
    .filter((f) => /\.(m4a|mp4)$/i.test(f))
    .sort((a, b) => a.localeCompare(b, "zh"));
  for (const f of files) {
    cpSync(join(dir, f), join(DEST, "voice", group, f));
    manifest.push({
      index: index++,
      group,
      name: f.replace(/\.(m4a|mp4)$/i, ""),
      file: `${group}/${f}`,
    });
  }
}
writeFileSync(join(DEST, "manifest.json"), JSON.stringify(manifest, null, 2) + "\n");
console.log(`copied ${manifest.length} voices + spritesheet -> ${relative(".", DEST)}`);
