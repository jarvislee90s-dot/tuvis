import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";
import {
  renderMatrix,
  replaceMatrix,
  syncReadmes,
  validateMatrix,
} from "../../scripts/sync-capabilities.mjs";

const source = JSON.parse(readFileSync("docs/capabilities.json", "utf8"));
const rowCount = source.groups.reduce(
  (sum: number, group: { rows: unknown[] }) => sum + group.rows.length,
  0
);

describe("能力矩阵唯一维护源", () => {
  it("单元格双语缺失、未知行引用与对象备注不能通过校验", () => {
    const missingTranslation = structuredClone(source);
    delete missingTranslation.cellDetails.skill.dsh.en;
    expect(() => validateMatrix(missingTranslation)).toThrow();
    const invalidReference = structuredClone(source);
    invalidReference.cellDetails.missingRow = { claude: { zh: "说明", en: "detail" } };
    expect(() => validateMatrix(invalidReference)).toThrow();
    const invalidNote = structuredClone(source);
    invalidNote.notes.codex.en = { text: "not a string" };
    expect(() => validateMatrix(invalidNote)).toThrow();
  });
  it("中文 README 的行数声明来自实际矩阵", () => {
    expect(readFileSync("README.zh-CN.md", "utf8")).toContain(`${rowCount} 行能力`);
  });
  it("英文 README 的行数声明来自实际矩阵", () => {
    expect(readFileSync("README.md", "utf8")).toContain(`${rowCount} capability rows`);
  });
  it("实际 README 与源文件生成结果一致", async () => {
    await expect(syncReadmes({ check: true })).resolves.toEqual([]);
  });
  it("英文生成区没有中文回落", () => {
    expect(renderMatrix(source, "en")).not.toMatch(/\p{Script=Han}/u);
  });
  it("修改源支持状态时，输出随之改变", () => {
    const changed = structuredClone(source);
    changed.groups[0].rows[0].support.claude = "no";
    expect(renderMatrix(changed, "en")).not.toBe(renderMatrix(source, "en"));
  });
  it("未知状态和重复工具不能写入生成产物", () => {
    const changed = structuredClone(source);
    changed.groups[0].rows[0].support.claude = "constructor";
    expect(() => validateMatrix(changed)).toThrow("无效支持状态");
    changed.tools.push(changed.tools[0]);
    expect(() => validateMatrix(changed)).toThrow("重复");
  });
  it("只替换矩阵章节，重复生成保持一致，并转义表格分隔符", () => {
    const changed = structuredClone(source);
    changed.groups[0].rows[0].nameEn = "Status | lights";
    const input = "before\n## Capability Matrix\nold\n## Next\nafter\n";
    const result = replaceMatrix(input, changed, "en");
    expect(result.startsWith("before\n")).toBe(true);
    expect(result.endsWith("\n## Next\nafter\n")).toBe(true);
    expect(result).toContain("Status \\| lights");
    expect(replaceMatrix(result, changed, "en")).toBe(result);
  });
});
