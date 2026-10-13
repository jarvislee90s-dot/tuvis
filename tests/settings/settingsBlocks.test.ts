// 设置页两级导航的**可达性锁**（2026-10-06 用户裁决：按 6 大块切换、块内分小卡片）。
//
// 为什么需要它：重构把「每个分区一个菜单项」换成了「6 大块 × 各自的 `sections` 列表」，
// 于是出现一个**全新的、静默的**回归面 —— 某个分区**没被挂进任何块**（或挂漏了），
// 它在界面上就**永远看不见**：编译过、lint 过、其余用例全绿，没人会知道。
// 这条锁把「13 个分区各挂且只挂一次」变成可判定的事实。
//
// **为什么是源码锁而不是渲染断言**（与 `UsageStatusSection.test.tsx` 的接线锁同法）：渲染整个
// `SettingsPage` 要备齐 5 处窗口/主题/查询 mock，而本锁要判的只是一张**静态表**与 **13 处守卫**
// 的对应关系——静态扫描更稳、也更难被「恰好渲染出来了」蒙混（仓库里已有同款先例）。
import { readFileSync } from "node:fs";
import path from "node:path";
import { describe, expect, it } from "vitest";

const SRC = readFileSync(path.join(process.cwd(), "src/pages/settings.tsx"), "utf8");

/**
 * **13 个分区**（联合类型的唯一出处就是这张表；与 `SettingSection` 逐字对齐）。
 * 2026-10-07 D4：`shortcut`（全局快捷键）随功能一起移除 ⇒ 13 → 12，块表里也不再挂它。
 * 2026-10-11：`autostart`（开机自动启动，桌面行为块第三卡）随功能加入 ⇒ 12 → 13。
 * 本锁的判据（各挂且只挂一次 / 联合类型逐字对齐 / 每分区一处守卫）**一个字都没改**，
 * 只是清单随分区增删同步——这正是它该有的行为：加/删分区必须同时改三处，少改一处就红。
 */
const SECTIONS = [
  "appearance",
  "skin",
  "notifications",
  "pet",
  "autostart",
  "tools",
  "health",
  "signal",
  "remote",
  "audit",
  "data",
  "usage",
  "usageStatus",
] as const;

/** 6 个块（顺序即侧栏顺序；2026-10-11 顺序重派：桌面 → 外观 → 远程 → 用量 → 工具 → 数据） */
const BLOCKS = ["desktop", "appearance", "remote", "usage", "tools", "data"] as const;

/** 从 `SETTINGS_BLOCKS` 的每个 `sections: [...]` 里抽出分区 id（按出现顺序） */
function declaredSections(): string[] {
  const out: string[] = [];
  for (const m of SRC.matchAll(/sections: \[([^\]]*)\]/g)) {
    for (const raw of m[1].split(",")) {
      const id = raw.trim().replace(/^"|"$/g, "");
      if (id) out.push(id);
    }
  }
  return out;
}

describe("设置页两级导航：块表与渲染守卫的可达性锁", () => {
  it("① 13 个分区**各挂且只挂一个块**（漏挂 ⇒ 该分区在界面上永远看不见）", () => {
    const declared = declaredSections();
    // 逐分区计数：重复挂载（同一个分区出现在两个块里）与漏挂都是回归
    for (const id of SECTIONS) {
      expect(
        declared.filter((d) => d === id),
        `分区 ${id} 的挂载次数`
      ).toHaveLength(1);
    }
    // 反向：块表里不得出现 `SettingSection` 之外的 id（拼错的分区名同样是「永远看不见」）
    expect([...declared].sort()).toEqual([...SECTIONS].sort());
  });

  it("② 联合类型恰好就是这 13 个分区（加/删分区必须同时改块表）", () => {
    const m = /type SettingSection =([\s\S]*?);/.exec(SRC);
    expect(m, "找不到 `type SettingSection`").toBeTruthy();
    const union = [...(m as RegExpExecArray)[1].matchAll(/"(\w+)"/g)].map((x) => x[1]);
    expect([...union].sort()).toEqual([...SECTIONS].sort());
  });

  it("③ 每个分区都有**一处** `{visibleIds.has(...)}` 渲染守卫（漏一处 ⇒ 该卡不渲染）", () => {
    for (const id of SECTIONS) {
      const guards = SRC.match(new RegExp(`\\{visibleIds\\.has\\("${id}"\\)`, "g")) ?? [];
      expect(guards, `分区 ${id} 的渲染守卫数`).toHaveLength(1);
    }
  });

  it("④ 侧栏恰好 6 个块，且每个块带可点 testid（切块靠它，测试也靠它）", () => {
    for (const b of BLOCKS) {
      expect(SRC).toContain(`data-testid={\`settings-nav-\${block.id}\`}`);
      expect(SRC).toContain(`"${b}"`);
    }
    // 侧栏 map 的是块表（不是已被删掉的 `menuItems`）
    expect(SRC).toContain("SETTINGS_BLOCKS.map((block) => {");
    expect(SRC).not.toContain("menuItems");
    // 块内多卡并置 ⇒ 不能再有「当前分区」这一层**状态**（留着会造出「工具卡脏但守卫不触发」的空档）。
    // 判据只禁**状态本身**（声明与 setter）：注释里对 `activeSection === "tools"` 的历史引用是
    // **刻意留的**（说明为什么换成 `visibleIds.has("tools")` 更准），删掉反而更容易被改回去；
    // 而「JSX 里不再拿它当守卫」这件事由上面的 ③ 全覆盖（13 个分区必须各有一处 `visibleIds.has`）。
    expect(SRC).not.toContain("[activeSection,");
    expect(SRC).not.toContain("setActiveSection(");
    expect(SRC).toContain("[activeBlock, setActiveBlock]");
  });

  it("⑤ i18n：6 个块的标签与说明键在 zh / en 双语齐备", () => {
    const root = process.cwd();
    for (const loc of ["zh", "en"]) {
      const j = JSON.parse(readFileSync(path.join(root, `src/i18n/locales/${loc}.json`), "utf8"));
      const nav = j.settings.nav as Record<string, string>;
      for (const b of BLOCKS) {
        expect(typeof nav[b], `${loc}: settings.nav.${b}`).toBe("string");
        expect(typeof nav[`${b}Desc`], `${loc}: settings.nav.${b}Desc`).toBe("string");
      }
      // 逐 locale 的键集必须相等（半语种漏键是最常见的漂移）
      expect(Object.keys(nav).sort()).toEqual(Object.keys(j.settings.nav).sort());
    }
    const zh = JSON.parse(readFileSync(path.join(root, "src/i18n/locales/zh.json"), "utf8"));
    const en = JSON.parse(readFileSync(path.join(root, "src/i18n/locales/en.json"), "utf8"));
    expect(Object.keys(zh.settings.nav).sort()).toEqual(Object.keys(en.settings.nav).sort());
  });
});
