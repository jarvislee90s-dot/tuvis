import { readFile, writeFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

const MODULE_FILE = fileURLToPath(import.meta.url);
const ROOT = path.resolve(path.dirname(MODULE_FILE), "..");
const MARK = { yes: "✅", partial: "◐", experimental: "🧪", no: "❌" };
const escapeCell = (value) => String(value).replaceAll("|", "\\|").replace(/\r?\n/g, "<br>");
const isText = (value) => typeof value === "string" && value.trim().length > 0;
function validateBilingual(value, label) {
  if (!isText(value?.zh) || !isText(value?.en)) throw new Error(`缺少双语文本：${label}`);
}

export function validateMatrix(data) {
  if (
    !Array.isArray(data.tools) ||
    !data.tools.length ||
    new Set(data.tools).size !== data.tools.length
  ) {
    throw new Error("能力矩阵工具列表为空或重复");
  }
  if (!Array.isArray(data.groups) || !data.groups.length) throw new Error("能力矩阵缺少分组");
  const keys = new Set();
  for (const tool of data.tools) {
    if (
      typeof tool !== "string" ||
      !Object.hasOwn(data.toolNames ?? {}, tool) ||
      typeof data.toolNames[tool] !== "string"
    )
      throw new Error(`缺少工具名称：${tool}`);
  }
  validateBilingual(data.notes?.codex, "Codex 说明");
  for (const group of data.groups) {
    if (!Array.isArray(group.rows) || !group.rows.length) throw new Error("能力矩阵分组为空");
    for (const row of group.rows) {
      if (!isText(row.key) || keys.has(row.key) || !isText(row.name) || !isText(row.nameEn))
        throw new Error("矩阵行缺少双语名称或键重复");
      keys.add(row.key);
      if (row.note !== undefined || row.noteEn !== undefined) {
        validateBilingual({ zh: row.note, en: row.noteEn }, row.key);
      }
      if (Object.keys(row.support ?? {}).length !== data.tools.length)
        throw new Error(`支持列不完整：${row.key}`);
      for (const tool of data.tools) {
        if (!Object.hasOwn(MARK, row.support?.[tool]))
          throw new Error(`无效支持状态：${row.key}/${tool}`);
      }
    }
  }
  if (
    data.cellDetails !== undefined &&
    (data.cellDetails === null ||
      typeof data.cellDetails !== "object" ||
      Array.isArray(data.cellDetails))
  ) {
    throw new Error("单元格说明必须为对象");
  }
  for (const [rowKey, details] of Object.entries(data.cellDetails ?? {})) {
    if (!keys.has(rowKey)) throw new Error(`说明引用未知能力：${rowKey}`);
    if (details === null || typeof details !== "object" || Array.isArray(details))
      throw new Error(`无效单元格说明：${rowKey}`);
    for (const [tool, text] of Object.entries(details)) {
      if (!data.tools.includes(tool)) throw new Error(`说明引用未知工具：${tool}`);
      validateBilingual(text, `${rowKey}/${tool}`);
    }
  }
  return data;
}

export function renderMatrix(data, language) {
  validateMatrix(data);
  if (!["zh", "en"].includes(language)) throw new Error("仅支持 zh/en");
  const rows = data.groups.flatMap((group) => group.rows);
  const zh = language === "zh";
  const intro = zh
    ? `${rows.length} 行能力 × ${data.tools.length} 个工具（✅ 支持 / ◐ 部分 / 🧪 试验性 / ❌ 不支持）。`
    : `${rows.length} capability rows × ${data.tools.length} tools (✅ supported / ◐ partial / 🧪 experimental / ❌ not supported).`;
  const table = [
    [zh ? "能力" : "Capability", ...data.tools.map((tool) => data.toolNames[tool])],
    data.tools.map(() => "---").concat("---"),
    ...rows.map((row) => [
      zh ? row.name : row.nameEn,
      ...data.tools.map((tool) => {
        const detail = data.cellDetails?.[row.key]?.[tool]?.[language];
        return `${MARK[row.support[tool]]}${detail ? ` ${detail}` : ""}`;
      }),
    ]),
  ]
    .map((cells) => `| ${cells.map(escapeCell).join(" | ")} |`)
    .join("\n");
  const source = zh
    ? "数据唯一维护源：[`docs/capabilities.json`](docs/capabilities.json)，与官网同源。"
    : "Single maintained source: [`docs/capabilities.json`](docs/capabilities.json), shared with the website.";
  return `<!-- capabilities:start (generated; run pnpm sync:capabilities) -->\n\n${intro}\n\n${source}\n\n${table}\n\n${data.notes.codex[language]}\n\n<!-- capabilities:end -->`;
}

export function replaceMatrix(readme, data, language) {
  const heading = language === "zh" ? "## 能力矩阵" : "## Capability Matrix";
  const start = readme.indexOf(`${heading}\n`);
  if (start < 0) throw new Error(`README 缺少矩阵章节：${heading}`);
  const body = start + heading.length + 1;
  const end = readme.indexOf("\n## ", body);
  if (end < 0) throw new Error("README 矩阵之后缺少章节边界");
  return readme.slice(0, body) + `\n${renderMatrix(data, language)}\n` + readme.slice(end);
}

export async function syncReadmes({ check = false } = {}) {
  const data = JSON.parse(await readFile(path.join(ROOT, "docs/capabilities.json"), "utf8"));
  const outputs = await Promise.all(
    ["zh", "en"].map(async (language) => {
      const filename = language === "zh" ? "README.zh-CN.md" : "README.md";
      const file = path.join(ROOT, filename);
      const before = (await readFile(file, "utf8")).replaceAll("\r\n", "\n");
      return { file, filename, before, after: replaceMatrix(before, data, language) };
    })
  );
  const changed = outputs.filter(({ before, after }) => before !== after);
  if (check && changed.length)
    throw new Error(
      `README 矩阵已漂移：${changed.map((o) => o.filename).join(", ")}；运行 pnpm sync:capabilities`
    );
  if (!check) for (const { file, after } of changed) await writeFile(file, after);
  return changed.map(({ filename }) => filename);
}

if (process.argv[1] && path.resolve(process.argv[1]) === MODULE_FILE) {
  syncReadmes({ check: process.argv.includes("--check") })
    .then(() => {
      console.log("能力矩阵与唯一数据源一致");
    })
    .catch((error) => {
      console.error(error.message);
      process.exitCode = 1;
    });
}
