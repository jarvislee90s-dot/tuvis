// 阶段② · 「部分覆盖**不在界面表达**」+ 过期「19/99」的**源码/文档锁**（2026-10-06 用户裁决）。
//
// 为什么需要这条锁：
//  * 那个比例（spec 探测期记的「原始日志仅 19/99 会话存在 → available=true + reason=部分覆盖」）
//    **已经过期**——覆盖率随机器与会话清理策略变化。它当时散落在 4 个 Rust 文件 + spec 的 6 处，
//    只靠人工清理**必然腐烂**（本仓既有教训逐字写在 `collect.rs` 的 `scan_face` 钉子注释里：
//    「只在注释里写数字 ⇒ 每轮都会腐烂 …… 且**没有任何用例会发现**」）。
//  * 裁决本身也要可发现：**「部分覆盖」不新增界面档位**（可得性模型是布尔两态，加一档要动冻结
//    契约；且前端不直渲后端中文 `reason`）。裁决被删掉、与比例被写回来，一样是回归。
// ⇒ 本锁钉住两头：**比例不许回来** + **登记必须在位**（spec §9.5.1 / `availability.ts` 口径 ④ /
//    验收清单 §0+§6 / README——2026-10-22 英文化后为中英两份：`README.zh-CN.md` + `README.md`）。
//
// ⚠️ **范围（别以为它覆盖全仓）**：本锁只扫下面这几个文件 —— 判据面（`services/usage/**`）的 4 个
//    Rust 文件 + spec + **验收清单**（`ACCEPT`）+ **双 README**（`README.zh-CN.md` + `README.md`，
//    共 8 个被扫文件）。**用例 1 的 `files` 数组 =
//    `RUST_FILES + SPEC + ACCEPT + README + README_EN`**（2026-10-06 第三轮把 ACCEPT 补进来：此前文件头声明
//    含验收清单、而 `files` 与黑名单都不含它 ⇒ 那份文件里的 `19/99` 其实**不受本锁保护**，
//    它当时那两处恰巧都带「已作废」标记，属侥幸而非设计）。**① 的历史计划文件
//    `docs/superpowers/plans/2026-10-03-usage-collector-and-storage.md` 不在范围内**：它是
//    **gitignored 的本地存档**（记的是"当时怎么定的"），里头的 19/99 属历史记录，**不清**。
//
// 2026-10-22（README 英文化后）：锁面扩为中英双 README —— 默认 `README.md` 已是英文版，其作废标记为
//    `obsolete`。英文版无「不在界面表达」对应句（翻译缺口，待另行补译），该面暂无英文锁。
//
// ⚠️ **判据强度（如实标注，别当成行为锁）** —— 逐条给**单点硬钉 / 存在性判据**的分类与
//    **该串在目标文件里的实际出现次数**（第三轮按实测数字改写；早先把用例 2 / 3 一并写成
//    「存在性判据」是**低估**了用例 2）：
//  * **用例 1** = **逐行判据**：出现 `19/99` 的行必须自带「作废 / 不写死 / 过期 / obsolete」之一。
//    含 `19/99` 的**被锁文件是七个**（8 个被扫文件里只有 `query.rs` 一处都没有），
//    **逐文件行数 = 3 / 2 / 2 / 1 / 1 / 1 / 1** —— 这组数字的**唯一出处是下面那张 `EXPECTED_COUNTS`
//    表**，用例 1 会逐文件 `match(/19\/99/g)` 数一遍并与它 `toEqual` **互锁**（第五轮新增：
//    手写计数已腐烂过一次——第四轮前写「三个文件 3 / 2 / 1」，实测是六文件 3/2/2/1/1/1）。
//    **黑名单那两条是全文包含判据**——任一被锁文件里写回那两种断言式写法即红（单点硬钉）。
//    它挡的是「把过期比例当现行事实写回来」。
//  * **用例 2** = **混合**：
//      - **单点硬钉**（该串在目标文件里**只此一处** ⇒ 删掉它本用例即红）：
//        spec 的 `布尔两态`（1 处）、`availability.ts` 的三个串（`不在界面上表达` / `布尔两态` /
//        `EM_DASH` **各 1 处**）、README 中文版的 `不在界面表达`（1 处）、README 英文版的三个
//        对应串（`no ratio is…hard-coded` 跨行正则 / `coverage varies…` / `the earlier "19/99"
//        is obsolete` **各 1 处**；英文版无「不在界面表达」对应句，该面暂无英文锁）；
//      - **存在性判据**（多处 ⇒ 删掉其中任一处仍绿）：spec 的 `9.5.1`（2 处）、
//        spec 的 `不在界面表达`（4 处）、`ACCEPT` 的 `部分覆盖`（4 处）与 `不在界面表达`（2 处）。
//  * **用例 3** = **几乎全是存在性判据**：`一部分会话`（spec 9 处）、`部分覆盖`（spec 11 处）、
//    `available=true`（spec 5 处）—— 删掉其中任一处登记它们都还是绿的。这是**刻意**的：这几条锁的是
//    "裁决还写着、结论没被一起删掉"，不是"防单点删除"（那要逐处钉死，维护成本不值）。
//    **例外（单点硬钉）**：`query.rs` 的 `覆盖率随机器与清理策略变化`（生产字符串，**只此一处**，
//    且它在自省锁的判据面内 ⇒ 改它还会动面尺寸钉子）。
//
// 纯文本扫描（零 DOM API、零新增依赖），与主题门禁 `usage-theme-tokens.test.ts` 同法。
import { readFileSync } from "node:fs";
import path from "node:path";
import { describe, expect, it } from "vitest";

const read = (p: string) => readFileSync(path.join(process.cwd(), p), "utf8");

/** 判据面（`services/usage/**`）里曾经写死过 19/99 的四个文件（逐个具名，供下面的计数表引用） */
const CAPS = "src-tauri/src/services/usage/caps.rs";
const QUERY = "src-tauri/src/services/usage/query.rs";
const DSH = "src-tauri/src/services/usage/collectors/dsh.rs";
const COLLECT = "src-tauri/src/services/usage/collect.rs";
const RUST_FILES = [CAPS, QUERY, DSH, COLLECT];
const SPEC = "docs/superpowers/specs/2026-10-03-pet-token-usage-dashboard-design.md";
const ACCEPT = "docs/release-notes/usage-dashboard-acceptance.md";
const README = "README.zh-CN.md"; // 中文主笔版：锁钉的是中文裁决串（不在界面表达/作废标记）
const README_EN = "README.md"; // 默认英文版：锁对应英文裁决句（作废标记是 obsolete）——2026-10-22 英文化后入锁

/**
 * **含 `19/99` 的被锁文件 × 逐文件行数** —— 文件头那句「七个文件、3 / 2 / 2 / 1 / 1 / 1 / 1」的
 * **唯一出处**。这张表**不是注释**：用例 1 会逐文件 `match(/19\/99/g)` 数一遍，与本表
 * `toEqual` 互锁 ⇒ **数字漂了就红**（第五轮新增）。
 *
 * 为什么必须机械钉：这类手写计数**已经腐烂过一次** —— 第四轮之前文件头写「三个被锁文件里分别
 * 出现 3 / 2 / 1 处」，实测是**六个**文件 3 / 2 / 2 / 1 / 1 / 1（`grep -c` 复核：spec 3、
 * `collect.rs` 2、验收清单 2、`caps.rs` 1、`dsh.rs` 1、README 1；第 7 个被扫文件 `query.rs` 为 0）。
 */
const EXPECTED_COUNTS: Record<string, number> = {
  [SPEC]: 3,
  [COLLECT]: 2,
  [ACCEPT]: 2,
  [CAPS]: 1,
  [DSH]: 1,
  [README]: 1,
  [README_EN]: 1,
};

/**
 * 「旧比例」出现的每一行都必须**自带作废标记**（作废 / 不写死 / 过期 / obsolete）。
 *
 * 为什么不是「一律不许出现这四个字符」：清理过期数字时**必然要在登记与钉子注释里引用它**
 * （「探测期的 19/99 已作废」，`collect.rs` 的判据面钉子注释还要靠它说明面为什么变）。
 * 真正的不变量是**这个数字不得再被当作现行事实** —— 故判据按**行**做：出现即须同行为
 * 「已作废 / 不写死 / 过期」的语境，否则红。
 *
 * ⚠️ **标记里刻意不含 `→`**（2026-10-06 第二轮堵后门）：`→` 太容易当连接词用，
 * 「本机仅 19/99 会话留有原始日志 → 说明」这种**断言式**写法会同时骗过本函数与下面的黑名单。
 * 去掉它之后被锁文件里含 19/99 的每一行都仍带真标记（作废 / 不写死 / 过期，当时仅此三标记），不误红。
 *
 * 2026-10-22：README 英文化后英文版（`README.md`）也入锁，其作废标记为 `obsolete`
 * （语言不限，语义同为「声明已作废」），故判据正则兼收它。
 */
function assertOnlyRetiredMentions(file: string, src: string) {
  for (const [i, line] of src.split("\n").entries()) {
    if (!line.includes("19/99")) continue;
    expect(
      /作废|不写死|过期|obsolete/.test(line),
      `${file}:${i + 1} 出现了 19/99 但没写明它已作废（该比例随机器与清理策略变化，不得当作现行事实）`
    ).toBe(true);
  }
}

describe("usage-partial-coverage-lock（「部分覆盖不在界面表达」+ 过期比例清理）", () => {
  it("1. 旧比例不得再被当作现行事实：出现处必须自带「已作废 / 不写死」语境（写回来即红）", () => {
    // **含 ACCEPT**（2026-10-06 第三轮补齐）：文件头声明的扫描面里有验收清单，`files` 也必须真有它，
    // 否则那份文件里的 19/99 不受保护（当时靠"两处恰巧都带标记"过关，不是设计）。
    // **含双 README**（2026-10-22 补齐英文侧）：README 英文化后 `README.md` 一并入锁。
    const files = [...RUST_FILES, SPEC, ACCEPT, README, README_EN];
    for (const f of files) assertOnlyRetiredMentions(f, read(f));
    // **计数互锁（第五轮新增）**：逐文件数 `19/99`，与文件头那张 `EXPECTED_COUNTS` **逐字比对** ——
    // 手写数字从此不可能静默漂（本表上一轮已腐烂过一次：写「三个文件 3 / 2 / 1」，实测六文件
    // 3 / 2 / 2 / 1 / 1 / 1）。`query.rs` 一处都没有 ⇒ 表里不列它，但下面单独钉住「它是 0」。
    const actualCounts: Record<string, number> = {};
    for (const f of files) {
      const n = (read(f).match(/19\/99/g) ?? []).length;
      if (n > 0) actualCounts[f] = n;
    }
    expect(actualCounts).toEqual(EXPECTED_COUNTS);
    expect(actualCounts[QUERY] ?? 0).toBe(0);
    // 表里的键必须是**被扫文件**的子集（防把路径写错成「永远数不到的幽灵文件」）
    for (const f of Object.keys(EXPECTED_COUNTS)) expect(files).toContain(f);
    // 当年那几种**断言式**写法逐字清零（它们才是缺陷本身）。
    // ⚠️ 黑名单里**不放 `本机 19/99`**：那正是"合法的退化引用"的形态（「（本机 19/99）→ 改成…」、
    // 「旧注释里的『本机 19/99』已作废」），禁它会把**正确的清理写法**也判红（洞一）。
    for (const f of files) {
      const src = read(f);
      for (const stale of ["仅 19/99 会话存在", "原始日志仅 19/99"]) {
        expect(src.includes(stale), `${f} 仍留着过期断言「${stale}」`).toBe(false);
      }
    }
  });

  it("2. 裁决登记齐：spec §9.5.1 / availability.ts 口径 ④ / 验收清单 §0+§6 / 双 README", () => {
    const spec = read(SPEC);
    expect(spec).toContain("9.5.1"); // 登记小节
    expect(spec).toContain("不在界面表达");
    expect(spec).toContain("布尔两态"); // 理由 ①：没有「部分」这一档

    const av = read("src/lib/usage/availability.ts");
    expect(av).toContain("不在界面上表达");
    expect(av).toContain("布尔两态");
    expect(av).toContain("EM_DASH"); // 落地：给不出的位次走空态，而不是新造一个「部分」档

    const acc = read(ACCEPT);
    expect(acc).toContain("部分覆盖");
    expect(acc).toContain("不在界面表达"); // §0 的第 9 行

    expect(read(README)).toContain("不在界面表达"); // README 中文版的可得性小节同步登记

    // 英文默认版对应句入锁（各恰 1 处 = 单点硬钉）；「不在界面表达」句英文版缺失（翻译缺口，见文件头标注）。
    const en = read(README_EN);
    expect(en).toMatch(/no ratio is\s+hard-coded/); // 比例不写死的对应句（中间断行，须用正则）
    expect(en).toContain("coverage varies with the machine and cleanup policy");
    expect(en).toContain('the earlier "19/99" is obsolete'); // 「已作废」的直接对应
  });

  it("3. 清理比例 ≠ 删掉结论：定性的「一部分会话 / 部分覆盖 / available=true」仍在位", () => {
    const spec = read(SPEC);
    expect(spec).toContain("一部分会话");
    expect(spec).toContain("部分覆盖");
    expect(spec).toContain("available=true");
    // 后端 reason 里那条口径也必须是**定性**的（生产字符串，改动会进判据面钉子）
    expect(read("src-tauri/src/services/usage/query.rs")).toContain("覆盖率随机器与清理策略变化");
  });
});
