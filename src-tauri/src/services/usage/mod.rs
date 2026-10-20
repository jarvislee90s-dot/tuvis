// 用量采集与存储底座（计划①）：契约 §2 值对象 + 口径层 + 7 采集器 + 查询 + 账本落库。
// 纪律：本域的一切文件读取只经 stream.rs::read_incremental；一切口径计算只在
// semantics / dedup / project / provider / range 五个纯函数模块里（采集器只上报原始字段）。
// 模块声明随任务增量补齐（每个任务在 Files 里写明自己要 append 的那一行），
// 避免出现「声明了但文件还没建」的中间态编译失败。
pub mod caps;
pub mod collect;
pub mod collectors;
pub mod cursor;
pub mod dedup;
pub mod delta;
pub mod error;
pub mod ledger;
pub mod model;
pub mod project;
pub mod provider;
pub mod query;
pub mod range;
pub mod semantics;
pub mod settings;
pub mod stream;
pub mod v2;

pub use error::UsageError;
pub use model::*;

/// 供应商不可得时的行标签 = **i18n 键**（zh/en 同父同键，前端 `t(label)` 渲染）。
/// **不得**在 Rust 侧硬编码任何中文文案（W5）；CSV 导出不落 i18n 键（该列留空，见 Task 19）。
pub const UNKNOWN_PROVIDER_LABEL_KEY: &str = "usage.label.unknownProvider";

/// 当前毫秒（全域唯一时钟入口，便于测试注入）
pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// **源码级自省锁的共享判据面**（第 5 轮：用**显式哨兵标记**取代全部「结构推断」；
/// 第 6 轮补「**排除区形态**」断言 —— 堵掉「往排除区里写生产代码」这条第三条致盲路）。
///
/// 五轮教训一句话：判据先前一直用**文本启发式去推断 Rust 结构**（首个 `#[cfg(test)]` 字面量 →
/// 前缀匹配的 `mod tests` → 大括号「配平到 EOF」），每一轮都被**更早的假锚点 / 字符串字面量 /
/// 块注释**骗过；而且**盲区永远是 `[锚点, EOF)`**——「收口校验」管不到锚点与收口点之间装了什么
/// （生产代码可以写在测试模块**之后**，Rust 不看声明顺序）。
/// 第 5 轮把「结构推断」**整个删掉**：面由**一对显式标记**划出，判据只做「计数 + 顺序 + 非空」
/// 这类**无条件断言**，不再猜任何 Rust 结构。**两份手抄判据也随之下岗**（改为一处共享实现，
/// 见下）——从此不存在「只改一处不会变红」的问题：只有一处可改。
/// 第 6 轮补上**排除区形态断言**：排除区**只允许**是 `#[cfg(test)] mod tests { … }` 这一种形状
/// （首行 `#[cfg(test)]`、次行 `mod ` 开头、末行 `}`、**其余非空行必须缩进**）——因为「往排除区里
/// 写生产代码」曾经是第三条致盲路（**一个标记都不用搬**）。
#[cfg(test)]
pub(crate) mod self_lock {
    /// 排除区**开始**标记（每个文件**恰好 1 行**）。用 `concat!` 拆开写：判据自己的源码也在
    /// 扫描面内，写成整行字面量会让「恰好 1 次」变成 2 次 ⇒ 响亮失败。
    pub(crate) const EXCLUDE_BEGIN: &str = concat!(
        "// ==== usage 自省锁：",
        "以下为排除区（测试代码），勿删勿复制 ===="
    );

    /// 排除区**结束**标记（每个文件**恰好 1 行**）。位置：有测试模块的文件 = 其测试模块的收口
    /// `}` 之后（本域 30 个文件里它就是文件最后一行）；**无测试模块的 6 个文件**
    /// （`caps.rs` / `delta.rs` / `model.rs` / `settings.rs` / `v2/model.rs` / `v2/mod.rs`）
    /// = **紧邻 BEGIN 之后**（排除区为空 ⇒ 整文件都在面内）。**分类由
    /// `collect::tests::usage_source_scan_covers_every_file_on_disk`
    /// 机械钉住（30/6）**——给空区文件加测试模块而不同步这里，用例会红。
    pub(crate) const EXCLUDE_END: &str =
        concat!("// ==== usage 自省锁：", "排除区结束，勿删勿复制 ====");

    /// 定位两个标记（**计数 + 顺序**断言都在这儿，任何一条不满足 ⇒ panic）。
    fn markers(name: &str, src: &str) -> (usize, usize) {
        let raw: Vec<&str> = src.lines().collect();
        let begins = raw.iter().filter(|l| l.trim() == EXCLUDE_BEGIN).count();
        let ends = raw.iter().filter(|l| l.trim() == EXCLUDE_END).count();
        assert!(
            begins == 1 && ends == 1,
            "`{name}` 的排除区标记必须**各恰好 1 行**，实际 BEGIN={begins} / END={ends}：\
             0 行 = 标记被删（面不可信）、≥2 行 = 标记被复制或被他处整行引用（**含独占一行的\
             多行字符串**）⇒ **一律响亮失败，不许静默退化**。修法：两行标记按原样放回——\
             BEGIN 在测试模块 `#[cfg(test)]` 前一行，END 在其收口 `}}` 之后\
             （无测试模块的文件：两行**相邻**放在文件末尾）"
        );
        let b = raw
            .iter()
            .position(|l| l.trim() == EXCLUDE_BEGIN)
            .expect("BEGIN 计数已断言为 1");
        let e = raw
            .iter()
            .position(|l| l.trim() == EXCLUDE_END)
            .expect("END 计数已断言为 1");
        assert!(
            b < e,
            "`{name}` 的排除区标记顺序反了（BEGIN 在源码第 {} 行、END 在第 {} 行）：\
             顺序错了「面」就没有意义，**直接红**，不许猜",
            b + 1,
            e + 1
        );
        (b, e)
    }

    /// 排除区里的**代码行**（剥掉空行与**整行** `//` 注释后的原样行）。
    /// **空区** = 这个列表为空（两标记之间只有空行 / 注释）——与 `exclusion_is_empty` **同源同义**。
    fn exclude_region<'a>(raw: &[&'a str], b: usize, e: usize) -> Vec<&'a str> {
        raw[b + 1..e]
            .iter()
            .copied()
            .filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with("//"))
            .collect()
    }

    /// **排除区形态断言（第 6 轮 P1-A）**：排除区只允许两种形态 ——
    ///
    /// * **空区**（两标记之间没有**非注释**行）：`caps.rs` / `delta.rs` / `model.rs` / `settings.rs`
    ///   / `v2/model.rs` / `v2/mod.rs`；
    /// * **测试模块区**：① 首个非空行 == `#[cfg(test)]`；② 次个非空行以 `mod ` 开头；
    ///   ③ 末个非空行 == `}`；④ **除这三行外，区内其余非空行一律必须缩进**。
    ///
    /// **四条断言各自抓什么（第 7 轮按实测更正归因，别再笼统归给 ④）**：
    /// * 复审 **H1**（在 END 上一行顶格插 `pub fn day_key_of(..)`）⇒ 命中 **③**（末个非空行不再是 `}`）；
    /// * 复审 **H5**（在两条相邻标记之间插同一函数）⇒ 命中 **①**（首个非空行不再是 `#[cfg(test)]`）；
    /// * **④** 抓的是**朴素追加**那一类（把顶格代码塞进区内、或把测试模块收口的 `}` 挤成区内顶格行）。
    ///
    /// 正常测试代码全在 `mod tests { … }` 里、被 rustfmt 缩进 ⇒ 不会顶格；
    /// **④ 因此是「`cargo fmt` 代理」**：见下面 P1-2 的收窄声明。
    /// **④ 的边界（第 7 轮 P1-2 收窄，如实登记，不加新判据）**：
    /// * **抓得住**：「顶格朴素形态」——顶格插代码（H1 型）、或把区内容器挤成顶格行（朴素追加型）。
    /// * **抓不住**：**缩进 + 花括号重排**——在区内已有顶格 `}` 之前插 `    }` + 缩进的
    ///   `    mod … { … }`（四条断言全过；`cargo check --lib` 会报 E0308 ⇒ 证明区内确是生产代码），
    ///   **唯一防线是门禁的 `cargo fmt --check`**（rustfmt 把顶格项归一化后才红于 `mod.rs:147`）。
    ///   ⇒ 该形态需要**两个条件同时成立**（刻意重排 **且** fmt 未跑），**登记即为诚实**；
    ///   宿主裁定**不为此重引入结构推断 / 大括号深度判据**（那会引入「字符串与注释里的括号误判」
    ///   一整类新代价，收益不抵）。
    /// * **已知误报面**：排除区内的**多行字符串字面量**若某行顶格（如 `r#"…"#` 里顶格的 JSON 夹具），
    ///   会被当成顶格代码行 ⇒ **假红**（响亮、安全方向；当前 36 个文件实测 0 例）。
    ///   修法是把该行缩进（rustfmt **不会**重排字符串内容）。
    fn assert_exclude_shape(name: &str, region: &[&str]) {
        if region.is_empty() {
            return; // 空区：由 `markers` 的调用方另行断言「无非注释行」（见 scan_face）
        }
        assert!(
            region[0].trim() == concat!("#[cfg", "(test)]"),
            "`{name}` 排除区的**首个非空行**必须是 `#[cfg(test)]`，实际 `{}`：\
             排除区只允许放测试模块（`#[cfg(test)] mod tests {{ … }}`）",
            region[0].trim()
        );
        assert!(
            region.len() >= 3 && region[1].trim_start().starts_with(concat!("mod", " ")),
            "`{name}` 排除区的**次个非空行**必须以 `mod ` 开头（测试模块声明），实际 `{}`",
            region.get(1).unwrap_or(&"（不存在）").trim()
        );
        assert_eq!(
            region.last().map(|l| l.trim()),
            Some("}"),
            "`{name}` 排除区的**末个非空行**必须是 `}}`（测试模块的收口），实际 `{}`",
            region.last().unwrap_or(&"（不存在）").trim()
        );
        let sanctioned = [0usize, 1, region.len() - 1];
        let stray: Vec<&str> = region
            .iter()
            .enumerate()
            .filter(|(i, l)| !sanctioned.contains(i) && !l.starts_with(' ') && !l.starts_with('\t'))
            .map(|(_, l)| *l)
            .collect();
        assert!(
            stray.is_empty(),
            "`{name}` **排除区里出现了非缩进的代码行 —— 那是生产代码，必须放到 BEGIN 之前或 \
             END 之后**（第 6 轮 P1-A：这是「一个标记都不用搬」的第三条致盲路）。命中行：{:?}",
            stray
        );
    }

    /// 该文件的排除区是否为空（空区 = 两标记之间没有**非注释**行）。供 30/6 普查用例使用。
    pub(crate) fn exclusion_is_empty(name: &str, src: &str) -> bool {
        let raw: Vec<&str> = src.lines().collect();
        let (b, e) = markers(name, src);
        exclude_region(&raw, b, e).is_empty()
    }

    /// 取一个文件的**判据面**：`[文件头, BEGIN)` ∪ `(END, 文件尾]`，去行内空白、剥**整行** `//` 注释。
    ///
    /// `(END, 文件尾]` **必须在面内**：那是第 4 轮两条致盲构造（`R8_COLLECT` / `R8_STREAM`，
    /// 回归追加在文件末尾、落在旧「收口点」之外）的修法。
    ///
    /// **断言（任何一条不满足都 panic = 响亮失败，绝不静默退化）**：
    /// ① 两个标记各**恰好 1 行**（0 行 = 标记被删；≥2 行 = 被复制或被他处整行引用）；
    /// ② BEGIN 在 END **之前**；③ 排除区**形态**合法（见 `assert_exclude_shape`）；
    /// ④ 判据面**非空**。
    ///
    /// **威胁模型（如实声明，别再宣称「结构上不可能」）**：本锁防的是**顺手回归**与**无意的
    /// 习惯性改回去**，**不防蓄意改写本文件内的锁面标记**——对抗性攻击者本来就可以直接不写回归。
    /// 具体地：删标记 / 复制标记 / 调换顺序 ⇒ **响亮失败**；把 BEGIN **下移**或把 END **上移**
    /// （= 排除区缩小）⇒ **多扫**（安全方向，最坏是假阳性）；**在「不往排除区里写代码、也不搬动
    /// 标记」的前提下**，唯一能重新造出盲区的形态是**把 BEGIN 上移 / 把 END 下移（= 排除区扩大、
    /// 盖住生产代码）**，而那需要把标记**搬到**生产代码两侧——属上面那句「蓄意改写锁面标记」，
    /// **不在威胁模型内**，且 diff 里一眼可见。**而「往排除区里写入生产代码」是第三条路**
    /// （H1 / H5 实测：旧判据全绿）：**顶格朴素形态**已由 `assert_exclude_shape` 打红；
    /// **缩进 + 花括号重排**仍可过，**由门禁的 `cargo fmt --check` 兜住**（rustfmt 归一化顶格项后才红）。
    pub(crate) fn scan_face(name: &str, src: &str) -> Vec<(usize, String)> {
        let raw: Vec<&str> = src.lines().collect();
        let (b, e) = markers(name, src);
        let region = exclude_region(&raw, b, e);
        // 空区（无代码行）⇒ 整文件都在面内；非空 ⇒ 必须是**测试模块**那一种形状。
        if !region.is_empty() {
            assert_exclude_shape(name, &region);
        }
        let face: Vec<(usize, String)> = raw
            .iter()
            .enumerate()
            .filter(|(i, _)| *i < b || *i > e)
            .filter(|(_, l)| !l.trim_start().starts_with("//"))
            .map(|(i, l)| {
                (
                    i + 1,
                    l.chars().filter(|c| !c.is_whitespace()).collect::<String>(),
                )
            })
            .collect();
        assert!(
            !face.is_empty(),
            "`{name}` 的判据面为空（BEGIN 在第 {} 行、END 在第 {} 行）：面被清空 = 该文件的\
             反向锁彻底失效，**直接红**（这正是「哨兵写错位置把整个文件变成盲区」那条）",
            b + 1,
            e + 1
        );
        face
    }
}

/// `self_lock::scan_face` 自身的**表驱动用例**（第 6 轮 B-4）：三条断言的失败分支（0/1/2 标记、
/// 顺序反、面空）此前在仓内**没有任何用例覆盖**（复审只能用变异间接打响前两条）。
///
/// **本模块的存在会改变 `mod.rs` 的分类**：它从「相邻空区」变成「有测试模块」——
/// `collect::tests::usage_source_scan_covers_every_file_on_disk` 的 **30/6** 断言与两处声明
/// 已同步（这正是 B-3 要的机械钉住）。
// ==== usage 自省锁：以下为排除区（测试代码），勿删勿复制 ====
#[cfg(test)]
mod self_lock_tests {
    use super::self_lock::{exclusion_is_empty, scan_face, EXCLUDE_BEGIN as B, EXCLUDE_END as E};

    /// 合成源码（`B` / `E` 都是 `concat!` 拼出来的常量，故**本文件源码里没有整行标记**）。
    fn src_of(parts: &[&str]) -> String {
        parts.join("\n")
    }

    /// 断言判据**响亮失败**且文案含 `needle`。
    fn expect_panic(name: &str, src: &str, needle: &str) {
        let got = std::panic::catch_unwind(|| scan_face(name, src));
        let msg = match got {
            Ok(_) => panic!("`{name}` 的合成源码本该被拒绝，判据却通过了"),
            Err(e) => e
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| e.downcast_ref::<&str>().map(|s| (*s).to_string()))
                .unwrap_or_default(),
        };
        assert!(
            msg.contains(needle),
            "`{name}` 的失败文案应当含 `{needle}`，实际：{msg}"
        );
    }

    const HEALTHY_REGION: &[&str] = &[
        "#[cfg(test)]",
        "mod tests {",
        "    #[test]",
        "    fn t() {",
        "        assert!(true);",
        "    }",
        "}",
    ];

    #[test]
    fn rejects_zero_or_one_marker() {
        expect_panic(
            "zero",
            &src_of(&["pub fn a() {}", "pub fn b() {}"]),
            "各恰好 1 行",
        );
        expect_panic("only_begin", &src_of(&[B, "pub fn a() {}"]), "各恰好 1 行");
        expect_panic("only_end", &src_of(&["pub fn a() {}", E]), "各恰好 1 行");
        expect_panic(
            "twice",
            &src_of(&[B, E, "pub fn a() {}", B, E]),
            "各恰好 1 行",
        );
    }

    #[test]
    fn rejects_reversed_markers() {
        expect_panic(
            "reversed",
            &src_of(&[E, "pub fn a() {}", B, "pub fn b() {}"]),
            "顺序反了",
        );
    }

    #[test]
    fn rejects_empty_face() {
        // 排除区合法（健康测试模块），但面 = BEGIN 之前 ∪ END 之后 = 空
        let mut parts = vec![B];
        parts.extend_from_slice(HEALTHY_REGION);
        parts.push(E);
        expect_panic("face_empty", &src_of(&parts), "判据面为空");
    }

    #[test]
    fn rejects_marker_text_inside_a_multiline_string() {
        // 独占一行的标记文本（多行字符串里）⇒ 计数变 2 ⇒ 响亮失败（**安全方向**）
        let fake = src_of(&["pub fn a() {}", "const S: &str = r#\"", B, "\"#;"]);
        expect_panic("str_marker", &src_of(&[&fake, B, E]), "各恰好 1 行");
    }

    #[test]
    fn rejects_a_top_level_line_inside_the_exclusion_region() {
        // H1 / H5：往排除区里顶格插生产代码（标记零改动）
        let mut parts = vec![B, "#[cfg(test)]", "mod tests {"];
        parts.push(
            r#"pub fn day_key_of(_ts_ms: i64, _tz_name: &str) -> String { _tz_name.to_string() }"#,
        );
        parts.push("}");
        parts.push(E);
        expect_panic("top_level_in_region", &src_of(&parts), "非缩进的代码行");
        // H5 变体：两标记之间插同一行（空区被填成非空）⇒ 命中「首个非空行必须是 #[cfg(test)]」
        expect_panic(
            "code_between_adjacent_markers",
            &src_of(&["pub fn a() {}", B, "pub fn day_key_of() {}", E]),
            "首个非空行",
        );
    }

    #[test]
    fn rejects_a_region_that_is_not_a_test_module() {
        expect_panic(
            "first_line_not_cfg_test",
            &src_of(&[B, "mod tests {", "}", E, "pub fn a() {}"]),
            "首个非空行",
        );
        expect_panic(
            "second_line_not_mod",
            &src_of(&[B, "#[cfg(test)]", "fn helper() {", "}", E, "pub fn a() {}"]),
            "次个非空行",
        );
        expect_panic(
            "last_line_not_brace",
            &src_of(&[
                B,
                "#[cfg(test)]",
                "mod tests {",
                "    fn t() {}",
                E,
                "pub fn a() {}",
            ]),
            "末个非空行",
        );
    }

    #[test]
    fn accepts_the_two_legal_shapes_and_keeps_the_tail_in_the_face() {
        // ① 健康测试模块区：面 = 头 + END 之后（**尾部必须在面内** —— R8 两条的修法）
        let mut parts = vec!["pub fn prod() {}"];
        parts.push(B);
        parts.extend_from_slice(HEALTHY_REGION);
        parts.push(E);
        parts.push("pub fn appended_after_end() {}");
        let face: String = scan_face("healthy", &src_of(&parts))
            .into_iter()
            .map(|(_, l)| l)
            .collect();
        assert!(face.contains("pubfnprod(){}"));
        assert!(
            face.contains("pubfnappended_after_end(){}"),
            "END 之后的生产代码**必须在面内**（第 4 轮 R8 的修法），实际面：{face}"
        );
        assert!(!face.contains("assert!(true)"), "测试模块内部不得进面");
        assert!(!exclusion_is_empty("healthy", &src_of(&parts)));
        // ② 空区（相邻两行）：整文件都在面内
        let mut parts = vec!["pub fn prod() {}"];
        parts.push(B);
        parts.push(E);
        parts.push("pub fn tail() {}");
        let face: String = scan_face("empty_region", &src_of(&parts))
            .into_iter()
            .map(|(_, l)| l)
            .collect();
        assert!(face.contains("pubfnprod(){}") && face.contains("pubfntail(){}"));
        assert!(exclusion_is_empty("empty_region", &src_of(&parts)));
        // ③ 只有注释/空行的区**也算空区**（与 exclusion_is_empty 同义）
        let parts = vec![
            "pub fn prod() {}",
            B,
            "// 这里只允许注释",
            "",
            E,
            "pub fn tail() {}",
        ];
        let face: String = scan_face("comment_only_region", &src_of(&parts))
            .into_iter()
            .map(|(_, l)| l)
            .collect();
        assert!(face.contains("pubfntail(){}"));
        assert!(exclusion_is_empty("comment_only_region", &src_of(&parts)));
    }
}
// ==== usage 自省锁：排除区结束，勿删勿复制 ====
