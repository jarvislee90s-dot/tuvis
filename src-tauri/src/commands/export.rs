//! 导出落盘（契约 §3 新增两条命令，前端 2026-10-03 实测后定案）。
//!
//! **为什么不走 `<a download>` + Blob**：本仓 webview（wry/tauri 2.x）未注册 download handler 时
//! 对 download 类导航直接 Cancel 且**失败静默**；未标成 download 时会把窗口导航到 blob URL
//! （界面跑飞）。**为什么不走 `dialog.save` + plugin-fs**：plugin-fs 未安装、`dialog:allow-save`
//! 未声明、新窗口 `usage-dashboard` 不在任何 capability 的 windows 白名单里（插件命令会被 ACL 拒）；
//! 而**应用自定义命令不受 ACL 影响** → Rust 落盘是零新插件、零新权限、零 capability 改动的路径。
//!
//! **CSV 的 UTF-8 BOM**：Excel（尤其中文 Windows 版）在无 BOM 时按本地代码页解码 CSV，
//! 中文列名会乱码。本仓此前无 CSV 导出先例，故在此显式前置 `EF BB BF`——这是刻意的
//! 兼容性妥协，不是数据内容（导出内容本身仍是 UTF-8）。**与 Task 19 的分工**：
//! `services/usage/query.rs::export_csv` 出的文本**本层不带 BOM**（该文件有专门用例锁住），
//! BOM 只在这一层加；`with_bom_if_csv` 对已带 BOM 的输入幂等（上游哪天补了也不会出现双 BOM）。
//!
//! **GC 10（隐私白名单）**：本层只做「清洗文件名 → 前置 BOM → 原字节落盘」，
//! **不加工、不追加**任何内容（不夹带路径、时间戳、元数据），错误信息也只含字节数/静态文案。
//!
//! **落盘定位（GC 16 ④）**：复用既有 `commands/resource.rs::ensure_reveal_allowed`
//! （白名单含 `~/.tuvis`），**不新写打开逻辑**——定位由前端调既有 `reveal_dir` 完成。
//!
//! ## ⚠️ 锁序（W-26）
//! 本层**不取 `DB` 锁、也不调任何采集入口**（`collect()` / `collect_with()` / `run_collection()`）：
//! 那会与在飞扫描的「飞行锁 → DB」构成 ABBA 死锁，挂死整个 app 的 DB 访问。
//! 锁：本文件 `mod tests` 的 `export_layer_never_locks_db_swallows_errors_or_reimplements_reveal`。
//!
//! ## ⚠️ 写失败必须响亮（Task 20 裁决 F）
//! 落盘失败一律 `Err`（静态中文文案，含失败原因），**绝不 `let _ =` 吞错**、绝不返回成功
//! （否则前端会显示"导出成功"而磁盘上没有文件）。契约 §3 未给这两条命令定义结构化错误码
//! （9 个 `USAGE_CODES` 全属采集/查询域，含 A-2 的 `usage-filter-unavailable`）
//! → 本层不新造码，按计划用 `Result<_, String>`。
use base64::Engine;

use crate::commands::resource::ensure_reveal_allowed;

/// 文本上限（32 MB）：CSV 是聚合账本，正常远小于此；超限说明调用方传错了东西
pub const MAX_TEXT_BYTES: usize = 32 * 1024 * 1024;
/// 二进制上限（解码后 16 MB）：分享图 PNG 典型 0.2–2 MB
pub const MAX_BINARY_BYTES: usize = 16 * 1024 * 1024;
const MAX_NAME_LEN: usize = 128;

/// 文件名白名单（防路径穿越）：只允许 `[A-Za-z0-9._-]`，且拒绝 `..`、前导点、空名、超长。
/// **只取文件名，不接受任何目录成分**。
pub fn sanitize_export_name(name: &str) -> Result<String, String> {
    let n = name.trim();
    if n.is_empty() {
        return Err("EXPORT_NAME_EMPTY: 导出文件名不得为空".into());
    }
    if n.len() > MAX_NAME_LEN {
        return Err(format!(
            "EXPORT_NAME_TOO_LONG: 导出文件名过长（>{} 字符）",
            MAX_NAME_LEN
        ));
    }
    if n.starts_with('.') || n.contains("..") {
        return Err("EXPORT_NAME_DOT: 导出文件名不得以点开头或包含 ..".into());
    }
    if !n
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        return Err("EXPORT_NAME_CHARSET: 导出文件名只允许字母、数字、点、下划线、连字符".into());
    }
    Ok(n.to_string())
}

/// 导出目录：**系统「下载」文件夹**（2026-10-07 用户裁决 A2 —— 原为 `~/.tuvis/exports/`）。
///
/// **为什么改**：用户的心智模型是「我导出的东西在下载里」。`~/.tuvis/exports/` 藏在应用数据目录
/// 深处，导出成功后只能靠行内那个「打开所在目录」才找得到；而这个目录在 Finder / 资源管理器里
/// **不在任何常见入口下**。契约与需求说明书已同步（见 §3 的 2026-10-07 注记）。
///
/// **跨平台同一套**：`dirs::download_dir()` 在 macOS 给 `~/Downloads`、Windows 给
/// `%USERPROFILE%\Downloads`（以注册表 Shell Folders 为准，**可能是被 OneDrive 重定向过的路径**）
/// ⇒ 优先用它；取不到（极少见）才回落到 `<home>/Downloads`。
///
/// 与 `database/connection.rs::app_data_home()` 同一条约定——**仅在 debug/test 构建**下认
/// `TUVIS_HOME` 重定向（集成测试 `tests/support.rs` 靠它把数据目录指到 tempdir），此时导出目录 =
/// `$TUVIS_HOME/Downloads`：**保住测试隔离**，不让 `cargo test` 往开发机真实下载目录写文件。
/// release 生产构建一律用真实用户目录，防环境变量误设导致导出落到别处。
///
/// ⚠️ **改这里必须同时看 `commands/resource.rs::reveal_allowed_roots()`**：那个白名单根里就有
/// 本函数（否则导出成功却点不开「打开所在目录」）。两处由 `export.rs` 的单测钉住同源。
pub(crate) fn exports_dir() -> std::path::PathBuf {
    if cfg!(debug_assertions) {
        if let Some(h) = std::env::var_os("TUVIS_HOME").filter(|h| !h.is_empty()) {
            return std::path::PathBuf::from(h).join("Downloads");
        }
    }
    dirs::download_dir().unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join("Downloads"))
}

/// `.csv` 前置 UTF-8 BOM（幂等：已有则不重复加）
pub fn with_bom_if_csv(name: &str, content: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(content.len() + 3);
    if name.to_ascii_lowercase().ends_with(".csv") && !content.starts_with('\u{feff}') {
        out.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
    }
    out.extend_from_slice(content.as_bytes());
    out
}

pub fn check_text_size(content: &str) -> Result<(), String> {
    if content.len() > MAX_TEXT_BYTES {
        return Err(format!(
            "导出内容过大（{} 字节 > {} 上限）",
            content.len(),
            MAX_TEXT_BYTES
        ));
    }
    Ok(())
}

/// base64 宽松解码：容忍 `data:*;base64,` 前缀与换行/空白（canvas `toDataURL()` 形态）
pub fn decode_base64_loose(s: &str) -> Result<Vec<u8>, String> {
    let body = match s.split_once("base64,") {
        Some((_, rest)) => rest,
        None => s,
    };
    let cleaned: String = body.chars().filter(|c| !c.is_whitespace()).collect();
    base64::engine::general_purpose::STANDARD
        .decode(cleaned.as_bytes())
        .map_err(|e| format!("EXPORT_B64_DECODE: base64 解码失败: {e}"))
}

/// base64 **载荷长度**（剥掉 `data:*;base64,` 前缀与全部空白后的字符数）。
/// 只扫描、**不构造缓冲**——fix round 1 裁决 B 的长度预判要靠它避开 4/3 的大分配。
fn base64_payload_len(s: &str) -> usize {
    let body = match s.split_once("base64,") {
        Some((_, rest)) => rest,
        None => s,
    };
    body.chars().filter(|c| !c.is_whitespace()).count()
}

/// 载荷长度是否**可能**解出 > `MAX_BINARY_BYTES`（裁决 B 的**纯长度预判**）：
/// base64 每 4 个字符最多出 3 字节 ⇒ 载荷 > `4 * ceil(MAX / 3)` 时**必然**超限，可直接拒。
/// 反向不成立（恰好等于该长度的无填充串仍可能解出略超限的字节数）→ 落盘写手那道
/// `bytes.len() > MAX_BINARY_BYTES` 检查**保留**，两层都在。
/// 按**载荷**计数（不是原始串长度），故 `data:*;base64,` 前缀与折行换行不会被误杀。
fn base64_may_exceed_binary_limit(s: &str) -> bool {
    base64_payload_len(s) > MAX_BINARY_BYTES.div_ceil(3) * 4
}

/// 落盘核心（**目录注入版**）：建目录 → 写文件 → 返回绝对路径。
/// `dir` 由调用方给——生产传 `exports_dir()`，单测传 `tempfile::tempdir()`：
/// **这是为了不让 `cargo test`（lib 单测）往开发机真实导出目录（下载文件夹）写文件**
/// （§3.2.3 FIX-6 同类）。与全域 `*_conn` 形态同一个思路：依赖显式注入，不做隐式全局。
/// 为什么不用 `TUVIS_HOME` 环境变量重定向：env 是**进程级**的，lib 单测并行跑，
/// 一个用例 set/remove 会踩到同二进制里的其它用例（导出目录只在 `debug_assertions` 下认它）。
///
/// 写入语义：同名文件**覆盖**（`fs::write` 截断重写），不追加——追加会让重试产出两份表头。
pub fn save_text_file_in(
    dir: &std::path::Path,
    name: &str,
    content: &str,
) -> Result<String, String> {
    let safe = sanitize_export_name(name)?;
    check_text_size(content)?;
    std::fs::create_dir_all(dir)
        .map_err(|e| format!("EXPORT_MKDIR_FAIL: 创建导出目录失败: {e}"))?;
    let path = dir.join(&safe);
    std::fs::write(&path, with_bom_if_csv(&safe, content))
        .map_err(|e| format!("EXPORT_WRITE_FAIL: 写入失败: {e}"))?;
    Ok(path.to_string_lossy().to_string())
}

pub fn save_bytes_file_in(
    dir: &std::path::Path,
    name: &str,
    bytes: &[u8],
) -> Result<String, String> {
    let safe = sanitize_export_name(name)?;
    if bytes.len() > MAX_BINARY_BYTES {
        return Err(format!(
            "导出文件过大（{} 字节 > {} 上限）",
            bytes.len(),
            MAX_BINARY_BYTES
        ));
    }
    std::fs::create_dir_all(dir)
        .map_err(|e| format!("EXPORT_MKDIR_FAIL: 创建导出目录失败: {e}"))?;
    let path = dir.join(&safe);
    std::fs::write(&path, bytes).map_err(|e| format!("EXPORT_WRITE_FAIL: 写入失败: {e}"))?;
    Ok(path.to_string_lossy().to_string())
}

/// 生产入口（目录 = 导出目录，A2 起 = 系统下载目录）：薄包装，签名与既有调用方（`export_save_*`）不变
pub fn save_text_file(name: &str, content: &str) -> Result<String, String> {
    save_text_file_in(&exports_dir(), name, content)
}

pub fn save_bytes_file(name: &str, bytes: &[u8]) -> Result<String, String> {
    save_bytes_file_in(&exports_dir(), name, bytes)
}

/// 文本导出（CSV 用）。返回**落盘绝对路径**（前端据此提示，并可调既有 `reveal_dir` 定位）。
#[tauri::command]
pub fn export_save_text(name: String, content: String) -> Result<String, String> {
    let path = save_text_file(&name, &content)?;
    // 复用既有白名单校验（~/.tuvis 内），失败只 warn——文件已落盘，定位失败不该让导出算失败
    if let Err(e) = ensure_reveal_allowed(&path) {
        log::warn!("export: 落盘路径未通过 reveal 白名单（不该发生）: {}", e);
    }
    Ok(path)
}

/// 二进制导出（分享图 PNG 用）。base64 入参：JSON 数组会放大数倍体积，base64 仅 +33%。
#[tauri::command]
pub fn export_save_bytes(name: String, base64: String) -> Result<String, String> {
    // 裁决 B（fix round 1）：base64 是 4/3 放大——**先按载荷长度预判**，超限就不进 decode，
    // 免得 400 MB 入参先吃 ~300 MB 解码缓冲（上限口径仍是「解码后 16 MB」，此处只拒「必然超限」）。
    if base64_may_exceed_binary_limit(&base64) {
        return Err(format!(
            "导出内容过大（base64 载荷 {} 字符 > 解码后 {} 字节上限）",
            base64_payload_len(&base64),
            MAX_BINARY_BYTES
        ));
    }
    let bytes = decode_base64_loose(&base64)?;
    let path = save_bytes_file(&name, &bytes)?;
    if let Err(e) = ensure_reveal_allowed(&path) {
        log::warn!("export: 落盘路径未通过 reveal 白名单（不该发生）: {}", e);
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 本文件源码（自省锁的输入）
    const SELF_SRC: &str = include_str!("export.rs");
    /// 命令模块声明处（三处登记之一）
    const MOD_SRC: &str = include_str!("mod.rs");
    /// 命令注册表（`generate_handler!`）
    const LIB_SRC: &str = include_str!("../lib.rs");

    /// 读仓内文本文件（前端 mock 等）；读不到直接 panic（防「文件挪走 → 静默跳过」的假绿）
    fn repo_file(rel: &str) -> String {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
        std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("无法读取 {}: {}", path.display(), e))
    }

    /// 去掉注释行与全部空白后的「代码骨架」：自省锁只看代码、不看注释——
    /// 否则把文档注释里的签名抄一遍就能骗过签名锁（假绿）。
    /// 顺带把参数表尾逗号 `,)` 归一成 `)`，于是「一行写」与「多行写」等价（fmt 不制造假红）。
    ///
    /// ⚠️ 注意：本函数会**删掉全部空白**，故期望值必须写成 `code_only("pub mod export;")`
    /// 这种经过同一变换的形态，不能直接写字面量。
    fn code_only(src: &str) -> String {
        src.lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect::<String>()
            .replace(",)", ")")
    }

    /// 本文件的**生产半边**（`#[cfg(test)] mod tests` 之前）。
    /// 自省锁必须只看生产代码，否则断言里写的字面量（`"#[tauri::command]"`、`"DB.lock("`……）
    /// 会被自己数进去 → 反向锁恒红 / 正向锁恒假绿。
    fn prod_only(src: &str) -> String {
        let marker = concat!("#[cfg", "(test)]");
        match src.find(marker) {
            Some(i) => src[..i].to_string(),
            None => src.to_string(),
        }
    }

    /// 契约 §3 的两条导出命令签名（**逐字冻结**）。参数名即 Tauri 的 JS 侧键名
    /// （默认 camelCase 映射）：`base64` → `base64`、`content` → `content`……改一个字母，
    /// 前端 `invoke` 就会**静默 reject**（mock 会掩盖它）。
    const FROZEN_SIGNATURES: [&str; 2] = [
        "pub fn export_save_text(name: String, content: String) -> Result<String, String>",
        "pub fn export_save_bytes(name: String, base64: String) -> Result<String, String>",
    ];

    const COMMAND_NAMES: [&str; 2] = ["export_save_text", "export_save_bytes"];

    #[test]
    fn name_whitelist_blocks_path_traversal() {
        for bad in [
            "../evil.csv",
            "a/b.csv",
            "a\\b.csv",
            ".hidden",
            "",
            "   ",
            "a b.csv",
            "名前.csv",
            "..",
            "a..b.csv",
        ] {
            assert!(sanitize_export_name(bad).is_err(), "必须拒绝: {bad}");
        }
        assert_eq!(
            sanitize_export_name("usage-2026-10-03.csv").unwrap(),
            "usage-2026-10-03.csv"
        );
        assert_eq!(
            sanitize_export_name("share_2026.10.03.png").unwrap(),
            "share_2026.10.03.png"
        );
        // 超长名（>128 字符）拒绝
        assert!(sanitize_export_name(&"a".repeat(129)).is_err());
        // 边界：恰好 128 字符**允许**（否则「上限」会被实现成 127）
        assert!(sanitize_export_name(&"a".repeat(128)).is_ok());
        // 首尾空白被 trim（不是拒绝整名）——与「空格在中间要拒绝」配对
        assert_eq!(sanitize_export_name("  ok.csv  ").unwrap(), "ok.csv");
    }

    /// CSV 必须带 UTF-8 BOM（Excel 兼容；本仓无先例，故显式断言前 3 字节）
    #[test]
    fn csv_gets_utf8_bom_prefix() {
        let body = with_bom_if_csv("usage.csv", "a,b\n1,2\n");
        assert_eq!(&body[..3], &[0xEF, 0xBB, 0xBF], "CSV 必须前置 UTF-8 BOM");
        assert_eq!(body.len(), "a,b\n1,2\n".len() + 3, "只多 3 字节 BOM");
        // 非 csv 不加 BOM
        let txt = with_bom_if_csv("note.txt", "a\n");
        assert_ne!(&txt[..3.min(txt.len())], &[0xEF, 0xBB, 0xBF]);
        // 大小写不敏感（.CSV 也要加）
        assert_eq!(
            &with_bom_if_csv("USAGE.CSV", "a\n")[..3],
            &[0xEF, 0xBB, 0xBF],
            ".CSV 同样要加 BOM"
        );
        // 已有 BOM 不重复加
        let twice = with_bom_if_csv("x.csv", "\u{feff}a\n");
        assert_eq!(twice.iter().filter(|b| **b == 0xEF).count(), 1);
    }

    #[test]
    fn size_limits_are_enforced_before_writing() {
        let big = "x".repeat(MAX_TEXT_BYTES + 1);
        assert!(check_text_size(&big).is_err());
        // 边界：恰好等于上限**放行**（不是 MAX-1）
        assert!(check_text_size(&"x".repeat(MAX_TEXT_BYTES)).is_ok());
        assert!(check_text_size("a,b\n").is_ok());
        let b64 = "A".repeat(((MAX_BINARY_BYTES + 1) / 3 + 1) * 4);
        assert!(
            decode_base64_loose(&b64)
                .map(|v| v.len())
                .unwrap_or(usize::MAX)
                > MAX_BINARY_BYTES
        );
    }

    #[test]
    fn base64_decoder_accepts_data_url_and_whitespace() {
        // canvas.toDataURL() 的形态：data:image/png;base64,....
        let png_head = "data:image/png;base64,iVBORw0KGgo=";
        assert_eq!(
            &decode_base64_loose(png_head).unwrap()[..4],
            &[0x89, b'P', b'N', b'G']
        );
        // 换行容忍（前端可能按 76 列折行）
        assert_eq!(decode_base64_loose("aGVs\nbG8=").unwrap(), b"hello");
        // 前缀 + 折行同时出现（真实的 toDataURL + 折行形态）
        assert_eq!(
            decode_base64_loose("data:image/png;base64,aGVs\nbG8=").unwrap(),
            b"hello"
        );
        // 边界：空前缀体 / 纯空白 → 空字节（不 panic、不造垃圾）
        assert!(
            decode_base64_loose("data:image/png;base64,")
                .unwrap()
                .is_empty(),
            "空前缀体必须解出空字节"
        );
        assert!(
            decode_base64_loose("  \n\t ").unwrap().is_empty(),
            "纯空白必须解出空字节"
        );
        // 非法字符 → 报错（不静默截断）
        assert!(decode_base64_loose("!!!!").is_err());
    }

    /// **裁决 B**（fix round 1）：`export_save_bytes` 在 decode **之前**按**载荷长度**预判。
    /// 本用例锁三件事：① 阈值两侧各一条（恰好等于上限的载荷不得被误杀）；② 判的是**载荷**
    /// 而不是原始串长度（`data:*;base64,` 前缀 + 折行会让整体长度超阈值）；③ 空串/纯空白不 panic。
    #[test]
    fn base64_precheck_counts_payload_only_and_does_not_over_reject() {
        assert_eq!(base64_payload_len("data:image/png;base64,aGVs\nbG8="), 8);
        assert_eq!(base64_payload_len("  aGVsbG8=  "), 8);
        assert_eq!(base64_payload_len(""), 0);
        assert_eq!(base64_payload_len("   \n\t "), 0);
        // 「解码后恰好 == MAX_BINARY_BYTES」对应的 base64 载荷长度
        let at_limit = MAX_BINARY_BYTES.div_ceil(3) * 4;
        assert!(
            !base64_may_exceed_binary_limit(&"A".repeat(at_limit)),
            "恰好等于上限的载荷长度不得被预判拒绝（防误杀）"
        );
        assert!(
            base64_may_exceed_binary_limit(&"A".repeat(at_limit + 4)),
            "多一个 base64 量子的载荷必须被预判拒绝"
        );
        // 前缀 + 折行：整体长度**超过**阈值，但载荷恰好等于阈值 → 不得拒
        let folded = format!("data:image/png;base64,\n{}\n", "A".repeat(at_limit));
        assert!(
            folded.len() > at_limit,
            "前提：整体长度确实超过阈值（否则本断言测不到「按载荷计数」）"
        );
        assert!(
            !base64_may_exceed_binary_limit(&folded),
            "预判必须按载荷计数：data URL 前缀与折行不得导致误杀"
        );
    }

    /// 落盘端到端：建目录 → 写文件 → 返回**绝对路径**（目录注入版，写 tempdir）。
    /// **本用例不碰开发机真实 `~/.tuvis/exports/`**（§3.2.3 FIX-6 同类）：目录由测试注入，
    /// 断言"目录不存在会自动建 + 返回的是该目录下的绝对路径 + 文件真的在"。
    #[test]
    fn writes_into_injected_dir_and_returns_absolute_path() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("exports"); // 故意给一个**还不存在**的目录
        let content = "a,b\n1,2\n";
        let path = save_text_file_in(&nested, "unit-test-export.csv", content).unwrap();
        assert!(
            path.starts_with(&nested.to_string_lossy().to_string()),
            "必须落在注入目录内：{path}"
        );
        assert!(
            std::path::Path::new(&path).is_absolute(),
            "契约 §3 要求返回**绝对路径**：{path}"
        );
        assert_eq!(
            std::path::Path::new(&path)
                .file_name()
                .and_then(|n| n.to_str()),
            Some("unit-test-export.csv"),
            "返回路径的文件名必须是清洗后的入参名"
        );
        assert!(nested.is_dir(), "目录不存在时必须自动创建");
        assert!(std::path::Path::new(&path).exists(), "文件必须真的落盘");
        // 「真落盘」而不是「只返回了字符串」：磁盘上的**字节数**必须对得上
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            (content.len() + 3) as u64,
            "磁盘上的字节数必须 == BOM(3) + 内容"
        );
        // CSV 必须带 BOM（与 `csv_gets_utf8_bom_prefix` 呼应：这里验的是真写进磁盘的字节）
        let raw = std::fs::read(&path).unwrap();
        assert_eq!(&raw[..3], &[0xEF, 0xBB, 0xBF], "落盘内容必须前置 UTF-8 BOM");
        assert_eq!(&raw[3..], content.as_bytes(), "BOM 之后逐字节等于入参");
    }

    /// 生产目录形态：`exports_dir()` 必须是**系统下载目录**且是**绝对路径**
    /// （契约 §3：返回落盘绝对路径）。**纯路径断言、零 IO**（旧版这条用例真的往导出目录
    /// 写了一个文件再自删，见 §3.2.3 FIX-6 同类）。
    ///
    /// 2026-10-07 A2：期望值由「以 `.tuvis/exports` 结尾」改为「以 `Downloads` 结尾」。
    /// 断言用**后缀**而不是 `dirs::download_dir()` 逐字比对 —— 后者在 Windows 上可能是被
    /// OneDrive 重定向过的路径，而本仓 CI 只跑 ubuntu + Windows 交叉**编译**（不跑 Windows 测试）
    /// ⇒ 拿本机 `download_dir()` 当期望值等于把「本机恰好没重定向」写成契约。
    ///
    /// **本用例故意不调 `ensure_reveal_allowed`**（旧版调了）：它对不存在的路径 `canonicalize`
    /// 会报「路径不存在」，而 lib 单测里那目录未必存在（`TUVIS_HOME` 下的 tempdir）。
    /// 「落盘路径能过 reveal 白名单」这条不变量改由下面 `reveal_whitelist_contains_exports_dir`
    /// **结构性**钉住（白名单根里就有本函数），零 IO、零环境变量副作用。
    #[test]
    fn production_exports_dir_shape() {
        let dir = exports_dir();
        assert!(
            dir.ends_with("Downloads")
                || dir.ends_with("Downloads/")
                || dir.ends_with("Downloads\\"),
            "导出目录必须是系统下载目录（A2），实际 {dir:?}"
        );
        assert!(
            dir.is_absolute(),
            "导出目录必须是绝对路径（否则返回给前端的落盘路径不是契约要求的绝对路径）：{dir:?}"
        );
    }

    /// **A2 的配套不变量（2026-10-07）**：`reveal_dir` 的白名单根必须**包含导出目录本身**。
    ///
    /// 为什么单独钉一条：导出目录从 `~/.tuvis/exports/` 改到系统下载目录后，**最容易漏的就是白名单**
    /// ——落盘成功、行内提示「已保存：…」，用户点「打开所在目录」却报「路径不在允许打开的范围」。
    /// 这条不变量是**同一函数**的两处引用（白名单根 ↔ 导出目录），故零 IO、不依赖目录是否已存在、
    /// 也不受 `TUVIS_HOME` 是否设置影响，任何平台都稳定。
    #[test]
    fn reveal_whitelist_contains_exports_dir() {
        let roots = crate::commands::resource::reveal_allowed_roots();
        let dir = exports_dir();
        assert!(
            roots.contains(&dir),
            "reveal 白名单根必须包含导出目录 {dir:?}，实际根 = {roots:?}\
             （漏了它 ⇒ 导出成功但「打开所在目录」必然失败）"
        );
    }

    /// 二进制落盘核：**逐字节**等于入参（不夹带 BOM/元数据）、空内容写 0 字节、
    /// 超限**先报错再动手**（不得留下文件）。
    #[test]
    fn bytes_writer_writes_exact_bytes_without_bom_and_rejects_oversize() {
        let dir = tempfile::tempdir().unwrap();
        let png = [0x89u8, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        let path = save_bytes_file_in(dir.path(), "share_2026.10.03.png", &png).unwrap();
        assert!(
            std::path::Path::new(&path).is_absolute(),
            "二进制落盘同样返回绝对路径：{path}"
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            png.len() as u64,
            "磁盘字节数必须等于入参长度"
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            png,
            "二进制落盘必须逐字节等于入参（不得夹带 BOM/元数据）"
        );
        // 即便是 .csv 名，二进制入口也不加 BOM（BOM 只属 export_save_text 的文本路径）
        let p2 = save_bytes_file_in(dir.path(), "binary-named.csv", &png).unwrap();
        assert_eq!(
            std::fs::read(&p2).unwrap(),
            png,
            "二进制入口不得前置 BOM（否则 Task 19 的文本路径会与它口径打架）"
        );
        // 空内容边界：写 0 字节文件（不是报错、也不是写一半）
        let p3 = save_bytes_file_in(dir.path(), "empty.png", &[]).unwrap();
        assert_eq!(std::fs::metadata(&p3).unwrap().len(), 0);
        // 超限：**落盘前**报错，且不得留下任何文件
        let oversize = vec![0u8; MAX_BINARY_BYTES + 1];
        let before = std::fs::read_dir(dir.path()).unwrap().count();
        let err = save_bytes_file_in(dir.path(), "huge.png", &oversize).unwrap_err();
        assert!(err.contains("过大"), "超限必须报错：{err}");
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            before,
            "超限不得留下文件"
        );
        assert!(!dir.path().join("huge.png").exists());
    }

    /// 写失败必须**响亮**（Task 20 裁决 F 的教训：`let _ =` 吞错 → 前端显示成功、重启回旧值）；
    /// 而三类校验/建目录失败必须发生在**动手之前**（不得留下目录或半截文件）。
    #[test]
    fn failed_writes_are_loud_and_validation_precedes_disk_touch() {
        // ① 目标目录名被一个**普通文件**占用 → create_dir_all 必败（跨平台、不依赖 chmod）
        let root = tempfile::tempdir().unwrap();
        let occupied = root.path().join("exports");
        std::fs::write(&occupied, b"not a dir").unwrap();
        let err = save_text_file_in(&occupied, "a.csv", "a,b\n1,2\n").unwrap_err();
        assert!(
            err.contains("创建导出目录失败"),
            "目录建不出来必须响亮报错：{err}"
        );
        assert_eq!(
            std::fs::read(&occupied).unwrap(),
            b"not a dir",
            "不得破坏占位文件"
        );

        // ② 目标路径被**同名目录**占用 → fs::write 必败（可写目录里的写失败）
        let out = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(out.path().join("a.csv")).unwrap();
        std::fs::create_dir_all(out.path().join("a.png")).unwrap();
        let err = save_text_file_in(out.path(), "a.csv", "a,b\n").unwrap_err();
        assert!(
            err.contains("写入失败"),
            "写失败必须响亮（不得静默报成功）：{err}"
        );
        let err = save_bytes_file_in(out.path(), "a.png", &[1, 2, 3]).unwrap_err();
        assert!(err.contains("写入失败"), "二进制路径同样要响亮：{err}");

        // ③ 校验失败（路径穿越 / 超长 / 体积）必须**先于**建目录：注入目录不得被创建
        let dir = tempfile::tempdir().unwrap();
        let never = dir.path().join("never-created");
        assert!(save_text_file_in(&never, "../evil.csv", "x").is_err());
        assert!(!never.exists(), "路径穿越被拒时不得创建目录");
        assert!(save_text_file_in(&never, &"a".repeat(129), "x").is_err());
        assert!(!never.exists(), "超长名被拒时不得创建目录");
        assert!(save_text_file_in(&never, "big.csv", &"x".repeat(MAX_TEXT_BYTES + 1)).is_err());
        assert!(!never.exists(), "文本超限被拒时不得创建目录");
        assert!(save_bytes_file_in(&never, "ok.png", &vec![0u8; MAX_BINARY_BYTES + 1]).is_err());
        assert!(!never.exists(), "二进制超限被拒时不得创建目录");
    }

    /// 只读目录里的写失败同样是**响亮**的（不能返回 Ok）。root 身份下本用例无法判定 → 前提跳过。
    #[cfg(unix)]
    #[test]
    fn readonly_dir_write_fails_loudly() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let ro = dir.path().join("ro");
        std::fs::create_dir_all(&ro).unwrap();
        std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o555)).unwrap();
        // **前提断言**：只读目录必须真的写不进去（root/特权身份下会写进去 → 本用例不可判）
        if std::fs::write(ro.join("probe"), b"x").is_ok() {
            eprintln!("跳过 readonly_dir_write_fails_loudly：当前身份可写只读目录（疑似 root）");
            std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o755)).unwrap();
            return;
        }
        let err = save_text_file_in(&ro, "a.csv", "a,b\n").unwrap_err();
        assert!(
            err.contains("写入失败"),
            "只读目录写入必须响亮报错（不得静默报成功）：{err}"
        );
        assert!(!ro.join("a.csv").exists(), "失败时不得留下半截文件");
        let err = save_bytes_file_in(&ro, "a.png", &[1, 2, 3]).unwrap_err();
        assert!(err.contains("写入失败"), "二进制路径同样要响亮：{err}");
        assert!(!ro.join("a.png").exists(), "失败时不得留下半截文件");
        std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// **GC 10 哨兵**：落盘内容只允许「结构化数字 + 静态文本」——
    /// 导出层既**不得**夹带任何额外内容（路径 / 时间戳 / 元数据 / `path_raw` 之流），
    /// 也**不得**把内容回显进路径与错误信息（那会随日志/提示语外泄）。
    #[test]
    fn written_text_is_exactly_bom_plus_content_with_no_extra_payload() {
        const SENTINEL: &str = "PROMPT-SENTINEL-绝密原文-SECRET";
        let content = format!("groupKey,label\n{SENTINEL},1\n");
        let dir = tempfile::tempdir().unwrap();
        let path = save_text_file_in(dir.path(), "s.csv", &content).unwrap();
        let raw = std::fs::read(&path).unwrap();
        let mut want = vec![0xEF, 0xBB, 0xBF];
        want.extend_from_slice(content.as_bytes());
        assert_eq!(
            raw, want,
            "落盘内容必须逐字节 == BOM + 入参（不得夹带路径/元数据/额外列）"
        );
        assert!(!path.contains(SENTINEL), "返回路径不得夹带内容：{path}");
        // 超限错误的构成只有字节数与上限，不得回显内容
        let big = format!("{SENTINEL}{}", "x".repeat(MAX_TEXT_BYTES + 1));
        let err = check_text_size(&big).unwrap_err();
        assert!(!err.contains(SENTINEL), "错误信息不得回显内容：{err}");
        assert!(err.contains(&MAX_TEXT_BYTES.to_string()), "错误应给出上限");
        // 非 CSV 文本：不加 BOM
        let p2 = save_text_file_in(dir.path(), "note.txt", "a\n").unwrap();
        assert_eq!(std::fs::read(&p2).unwrap(), b"a\n");
        // 已带 BOM 的 CSV：不重复加（Task 19 的文本本层不带 BOM；上游哪天补了，这里也只能有一个）
        let p3 = save_text_file_in(dir.path(), "bom.csv", "\u{feff}a,b\n").unwrap();
        let raw3 = std::fs::read(&p3).unwrap();
        assert_eq!(
            raw3.iter().filter(|b| **b == 0xEF).count(),
            1,
            "不得出现双 BOM"
        );
        assert_eq!(raw3, "\u{feff}a,b\n".as_bytes());
    }

    /// 目标文件已存在时的语义：**覆盖**（truncate 重写），不是追加——
    /// 追加会让「同一次导出重试」产出两份表头/两份数据（静默坏文件）。
    #[test]
    fn existing_file_is_overwritten_not_appended() {
        let dir = tempfile::tempdir().unwrap();
        let p1 = save_text_file_in(dir.path(), "again.csv", "old\n").unwrap();
        let p2 = save_text_file_in(dir.path(), "again.csv", "new\n").unwrap();
        assert_eq!(p1, p2, "同名导出必须落同一路径");
        let raw = std::fs::read(&p2).unwrap();
        assert_eq!(
            raw, b"\xEF\xBB\xBFnew\n",
            "必须是覆盖写（旧内容不得残留/追加）"
        );
        let b1 = save_bytes_file_in(dir.path(), "again.png", &[1, 2, 3]).unwrap();
        let b2 = save_bytes_file_in(dir.path(), "again.png", &[9]).unwrap();
        assert_eq!(b1, b2);
        assert_eq!(std::fs::read(&b2).unwrap(), vec![9u8], "二进制同样覆盖写");
    }

    /// **GC 1 契约冻结 + 三处登记**（契约 §1 / §3）：命令名、入参、返回类型逐字；
    /// 命令名/参数名/`rename_all` 的漂移**不会有编译错**，只让前端**静默 reject**（mock 会掩盖）。
    #[test]
    fn two_commands_keep_frozen_signatures_and_three_place_registration() {
        let code = code_only(&prod_only(SELF_SRC));
        assert_eq!(
            code.matches("#[tauri::command").count(),
            2,
            "导出域必须恰好 2 条 `#[tauri::command]`（契约 §3）"
        );
        for sig in FROZEN_SIGNATURES {
            let want = code_only(sig);
            assert!(
                code.contains(&want),
                "契约 §3 的命令签名被改动或缺失：`{sig}`\n（参数名 = 前端 invoke 的键名，改它前端会静默 reject）"
            );
        }
        for name in COMMAND_NAMES {
            assert_eq!(
                code.matches(&format!("fn{name}(")).count(),
                1,
                "命令 `{name}` 必须恰好声明一次（契约 §3）"
            );
        }
        assert!(
            !code.contains("rename_all"),
            "2 条命令不得使用 `rename_all`：前端按默认 camelCase 传 name/content/base64（契约 §1）"
        );

        // 登记处 ②：`lib.rs` 的 `generate_handler!` 逐条登记一次（漏登记 = 静默 reject）
        let lib = code_only(LIB_SRC);
        let handler_start = lib
            .find("tauri::generate_handler![")
            .expect("lib.rs 必须存在 generate_handler! 块");
        let handler = &lib[handler_start..];
        for name in COMMAND_NAMES {
            assert_eq!(
                handler
                    .matches(&format!("commands::export::{name},"))
                    .count(),
                1,
                "`generate_handler!` 必须登记 `commands::export::{name},`（契约 §1：三处登记）"
            );
        }
        assert_eq!(
            lib.matches("commands::export::").count(),
            2,
            "lib.rs 全文件只应有 2 处 `commands::export::` 引用（逐条登记、无重复）"
        );

        // 登记处 ①：`commands/mod.rs` 的模块声明
        let mods = code_only(MOD_SRC);
        assert!(
            mods.contains(&code_only("pub mod export;")),
            "commands/mod.rs 必须声明 `pub mod export;`"
        );

        // 登记处 ③：前端**双端 mock** 各恰好一个 case（重复 case 会静默遮蔽前者）
        for rel in ["../src/tauri-mock.ts", "../tests/msw/tauriMocks.ts"] {
            let text = repo_file(rel);
            for name in COMMAND_NAMES {
                let pat = format!("case \"{name}\"");
                assert_eq!(
                    text.split(&pat).count() - 1,
                    1,
                    "{rel} 里 `{pat}` 必须恰好一次（漏 = 前端拿到 null；重复 = 静默遮蔽）"
                );
            }
        }
    }

    /// **W-26**：本层**不得**取 `DB` 锁、也不得调采集入口（持 `DB` 锁再调采集 = ABBA 死锁，
    /// `std::sync::Mutex` 不可重入 → 整个 app 的 DB 访问挂死）。
    /// **W-53 / 裁决 F**：不得用 `let _ =` 吞掉写失败。
    /// **GC 16 ④ / 契约 §3**：定位一律复用既有 `ensure_reveal_allowed`，不得新写打开逻辑。
    #[test]
    fn export_layer_never_locks_db_swallows_errors_or_reimplements_reveal() {
        let code = code_only(&prod_only(SELF_SRC));
        for forbidden in [
            "DB.lock(",
            "cached_result(",
            "run_collection(",
            "collect_with(",
            "collect::collect(",
            "load_ledger_rows(",
            "settings::load(",
            "provider::resolve_provider(",
            "let_=",
            "std::process::Command",
            "open_file_in_system(",
            "open_dir_in_system(",
        ] {
            assert!(
                !code.contains(forbidden),
                "导出层不得出现 `{forbidden}`（W-26 锁序 / W-53 吞错 / GC 16 复用 reveal）"
            );
        }
        assert_eq!(
            code.matches("ensure_reveal_allowed(").count(),
            2,
            "两条命令都必须复用既有 `ensure_reveal_allowed` 校验（各一次；GC 16 ④）"
        );
    }
}
