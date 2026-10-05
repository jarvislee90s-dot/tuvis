// 文件安全读取与路径提取（M3 Task 8，C2 后端）
//
// 两条职责：
// - `extract_file_paths`：从会话消息流（复用 content::read_session_messages，八工具
//   统一出口）的 tool-call 条目里提取涉及的文件路径——**泛化实现，不做 per-tool
//   提取器**（控制者裁决 2026-09-15）：递归走 toolArgs JSON 树，收集
//   file_path / path / filename / abs_path（+ Task 9 实测补键 filePath / file，
//   证据见 task-9-report.md）键下的字符串值（须含路径分隔符或 `.扩展名` 才算
//   路径候选），去重保序。对八工具统一生效；各工具真实 toolArgs 形态由
//   extract_chain_* 系列 fixture 测试锁定。
// - `read_file_safe`：文件预览的安全读取内核——canonicalize（含 macOS firmlink
//   前缀折叠）后按敏感目录黑名单拒凭据，只读、双阈值大小上限（图片扩展名 5MB /
//   其余 500KB，按 Global Constraints 的 mime 分支）、按扩展名给 MIME。
//   注意：2026-09-18 起**全盘放开**——不再限定会话 cwd（越界拒绝已退役），
//   路径级防线只剩主目录内的敏感黑名单。
//
// Windows 注意：`canonicalize` 返回带 `\\?\`（及 `\\?\UNC\`）前缀的 verbatim 路径，
// 与字面 cwd 直接 starts_with 会因前缀不匹配误判越界——比较前统一剥前缀、统一
// 分隔符；Windows 文件系统大小写不敏感，语义分支下整体转小写（linker::detector
// 的 strip_verbatim_prefix / path_starts_with_ci 同款口径，因彼处私有且属 linker
// 域，此处以内聚小助手复刻，见 path_within_semantics）。
//
// 测试约束（宪法级）：一律 tempdir，绝不触真实 ~/.zcode ~/.claude 等数据目录；
// Windows 语义分支以 `windows: bool` 参数注入，darwin 上也可全分支测试。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use super::content::SessionMessage;

/// 文件路径源函数形态（RemoteState 注入缝的类型别名，生产 = extract_file_paths）
pub type PathSourceFn = dyn Fn(&str, &str, usize) -> (Vec<FileEntry>, bool) + Send + Sync;

/// 文本类文件大小上限：500KB（Global Constraints）
const MAX_TEXT_BYTES: u64 = 500 * 1024;
/// 图片类文件大小上限：5MB（Global Constraints，mime 分支）
const MAX_IMAGE_BYTES: u64 = 5 * 1024 * 1024;
/// 递归收集的参数键集合（小写精确匹配）。基础四键 `file_path`/`path`/`filename`/
/// `abs_path` 为控制者裁决；Task 9 按各工具真实 toolArgs 形态探测补两键（证据见
/// task-9-report.md）：`filePath` 驼峰——OpenCode 官方工具 schema（edit/write/read，
/// 本机库无 tool part 样本，文档级证据）；`file`——Kimi 旧版 Read 参数（本机
/// wire.jsonl 实测 3 例绝对路径）。键名单对八工具统一生效，不做 per-tool 分支；
/// 误收风险由 is_path_candidate 候选判定（URL/纯词/换行排除）兜底
const PATH_KEYS: &[&str] = &[
    "file_path",
    "path",
    "filename",
    "abs_path",
    "filePath",
    "file",
];
/// 单个路径候选的长度上限（超长串不是可预览文件，纯防御）
const MAX_PATH_LEN: usize = 4096;

// ============================================================
// 安全读取（read_file_safe）
// ============================================================

/// macOS APFS 数据卷 firmlink 前缀折叠：`/System/Volumes/Data/Users/x` 与
/// `/Users/x` 是同一 inode 的两个字面形态，而 `canonicalize` **不解析 firmlink**
/// （firmlink 非 symlink）——不折叠会被误判为「主目录外」而整段跳过黑名单
/// （2026-09-18 复核实锤：该别名可读 `~/.ssh/known_hosts`，194 字节泄露）。
/// 纯函数、平台中立（非 macOS 上该前缀不出现，调用为恒等变换）。
fn fold_data_volume_alias(p: &Path) -> PathBuf {
    const PREFIX: &str = "/System/Volumes/Data/";
    let s = p.to_string_lossy();
    match s.strip_prefix(PREFIX) {
        Some(rest) => PathBuf::from(format!("/{rest}")),
        None if s == "/System/Volumes/Data" => PathBuf::from("/"),
        None => p.to_path_buf(),
    }
}

/// 敏感黑名单命中判定（纯函数，无 IO——跨平台 CI 锁定用）：
/// 先对 child/base 两侧折叠 macOS Data 卷 firmlink 前缀（同 inode 双字面归一），
/// 再判「child 在主目录基准内 ∧ 相对段命中 SENSITIVE_DIRS」。
/// `windows` = 平台语义注入（路径分隔符双态 + Windows 语义大小写不敏感）。
fn sensitive_under_home(child: &Path, home_base: &Path, windows: bool) -> bool {
    let child = fold_data_volume_alias(child);
    let base = fold_data_volume_alias(home_base);
    path_within(&child, &base)
        && is_sensitive_path(&child.to_string_lossy(), &base.to_string_lossy(), windows)
}

/// 敏感目录拒绝清单（2026-09-16 用户裁决）：主目录放宽后，这些目录下的文件
/// 对**已配对设备**一律不可读——密钥/凭据/浏览器与会话数据。按路径段精确匹配
/// （`.ssh2` 这类前缀相似目录不误伤），平台语义可注入便于测试
pub(crate) const SENSITIVE_DIRS: &[&str] = &[
    ".ssh",
    ".aws",
    ".gnupg",
    ".tuvis",
    ".claude",
    ".codex",
    ".kimi-code",
    ".zcode",
    ".dsh",
    ".config",
    ".docker",
    ".kube",
    ".npmrc",
    ".netrc",
    ".git-credentials",
    "AppData", // Windows 应用数据（含浏览器 profile / 凭据库）
    "Library", // macOS 应用支持与凭据（~/Library/Keychains）
               // 黑名单仅作用于**用户主目录范围内**（凭据高价值区）；主目录之外不设
               // 路径级防线（M5 P2-a 全盘放开——系统目录本就低敏，且全路径黑名单会
               // 误伤 AppData 下的临时文件）
];

/// 子路径豁免表（T6/D9，手工验收修复批）：SENSITIVE_DIRS 整目录拦截下的「agent
/// 写给用户看的纯 markdown 产物」豁免——命中黑名单的路径若落在豁免子路径**子树**
/// 内（段精确：`.claude/plans-x` 不误豁免 `.claude/plans`）则放行预览。
/// 首例 `.claude/plans`（用户 C-10 发现计划文件「受安全策略保护」）；`.codex/plans`
/// 为 T0 盘点第二例（<turn>/<uuid>/PLAN.md 子树，纯 md）。凭据/会话原始数据照拦
/// （豁免只放产物目录；WorkBuddy 等其余候选待黑名单盲区补拦裁决，T0 盘点只记录）。
///
/// **批次丙 T7 · 尾段序列匹配**：kimi 的计划产物在
/// `~/.kimi-code/sessions/wd_<proj>_<hash>/session_<uuid>/agents/main/plans/*.md`
/// ——前缀含**变量段**（会话 id、工作目录 hash），段精确的静态前缀表永远接不住。
/// 故 `exempt_subpath_under_home` 增补第二条判据：豁免项作为**连续尾段序列**出现即
/// 命中（`agents/main/plans` 在 rel 中连续同序出现）。段精确性保持不变（`plans-x`
/// 不等于 `plans`；非连续同序不命中），凭据面照拦（`.kimi-code` 其余子路径仍被
/// SENSITIVE_DIRS 拦截，只有明确列出的产物目录放行）。
const EXEMPT_SUBPATHS: &[&str] = &[
    ".claude/plans",
    ".codex/plans",
    // T7 第三例：kimi 计划文件（深路径，含会话 id 变量段 → 靠尾段序列匹配；
    // 实测样本 `~/.kimi-code/sessions/wd_test_.../session_<uuid>/agents/main/plans/
    // miss-martian-she-hulk-beast.md`）
    "agents/main/plans",
];

/// fail-closed 全段匹配面（基准不可用分支专用）：仅凭据类目录。AppData/Library
/// 是「主目录内」语义段，不进全段面——Windows 的 TEMP 本就在 AppData 之下，
/// 全局段匹配会误伤一切临时文件（含测试 tempdir，feat/phase2-injection 合并
/// main 后 Windows 实机回归抓获；与本文件上方"全路径黑名单误伤 AppData 临时
/// 文件"实测教训同源——那条教训只落在了正常分支，fail-closed 分支漏了）
const CREDENTIAL_DIRS: &[&str] = &[
    ".ssh",
    ".aws",
    ".gnupg",
    ".tuvis",
    ".claude",
    ".codex",
    ".kimi-code",
    ".zcode",
    ".dsh",
    ".config",
    ".docker",
    ".kube",
    ".npmrc",
    ".netrc",
    ".git-credentials",
];

/// 预览拒绝原因（M5 P2-a：403 细分——原因仅暴露给已过闸设备，便于用户自助排障；
/// 原设计「一律空体 403 防探测」随边界放开退役：PIN + cookie 已是门槛，原因文案
/// 对已认证用户是排障信息而非预言机）
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FileRejectReason {
    /// 命中敏感目录黑名单（基准可用时仅主目录内路径参与判定；基准不可用时
    /// 退化为全段匹配——见 `read_file_safe` 的 fail-closed 分支）
    Sensitive,
    /// 超过预览大小上限
    TooLarge,
    /// 路径不存在 / 不可解析
    NotFound,
    /// 非常规文件（目录等）
    NotFile,
    /// 其它读取失败
    Io,
}

impl FileRejectReason {
    /// 日志用中文明细（reason 字段本身走 snake_case 序列化）
    pub fn message(&self) -> String {
        match self {
            FileRejectReason::Sensitive => "路径命中敏感目录黑名单，不可预览".into(),
            FileRejectReason::TooLarge => format!(
                "文件超过预览上限（文本 {}KB / 图片 {}MB）",
                MAX_TEXT_BYTES / 1024,
                MAX_IMAGE_BYTES / 1024 / 1024
            ),
            FileRejectReason::NotFound => "文件不存在或路径不可解析".into(),
            FileRejectReason::NotFile => "目标不是常规文件".into(),
            FileRejectReason::Io => "文件读取失败".into(),
        }
    }
}

/// 安全读取文件（M5 P2-a 全盘语义：任意路径可读，仅两道约束——① 主目录内的
/// 敏感目录黑名单（凭据防护，威胁主体是隧道/局域网上的配对设备读密钥）；②
/// 只读 + 双阈值大小上限。主目录之外不再设路径级防线：系统目录等本就任何
/// 用户可读，访问门槛 = PIN + 设备 cookie）。返回 (字节, mime)；拒绝一律
/// Err(FileRejectReason)（file 端点统一 403 并把 reason 序列化进响应体）。
/// `path` 支持绝对路径与相对路径（相对者按会话 cwd 解析——部分工具 toolArgs
/// 记录项目内相对路径）。
///
/// 边界沿革：2026-09-16 放宽为「cwd ∪ 主目录」；2026-09-18 用户裁决**全盘放开**
/// ——经常需要查看/参考项目外文件（微信目录附件等）。取舍记录：黑名单仅作用
/// 于主目录内（凭据高价值区）；主目录外连 Windows 系统目录都不设防（低价值 +
/// 避免误伤 AppData 下的临时文件/测试目录——全路径黑名单会让 Windows 临时目录
/// 全部不可读，实测教训）。
///
/// `home` = 敏感黑名单的**主目录基准**（非边界）：基准可用时仅主目录内路径过
/// 段匹配；基准不可用（None / 无法 canonicalize）时退化为**全段保守匹配**
/// （fail-closed，见 2026-09-18 追记：生产曾传 None 致黑名单整段失效）。
/// **调用方必须传真实 home**（端点经 `RemoteState.home_source`）。
pub fn read_file_safe(
    session_cwd: &str,
    path: &str,
    home: Option<&str>,
) -> Result<(Vec<u8>, String), FileRejectReason> {
    if path.trim().is_empty() {
        return Err(FileRejectReason::NotFound);
    }
    // 相对路径仍按会话 cwd 解析（cwd 本身不构成边界——全盘语义下仅作解析基准）
    let mut full = PathBuf::from(path.trim());
    if full.is_relative() {
        let cwd = Path::new(session_cwd)
            .canonicalize()
            .map_err(|_| FileRejectReason::NotFound)?;
        full = cwd.join(full);
    }
    let canon = full
        .canonicalize()
        .map_err(|_| FileRejectReason::NotFound)?;
    // 敏感黑名单（凭据防护，威胁主体 = 隧道/局域网上的配对设备读密钥）：
    // - 主目录基准可用 → 仅主目录内的路径过段匹配（相对主目录段；主目录之外
    //   不设路径级防线——M5 P2-a 2026-09-18 裁决原样保留）；
    // - 基准不可用（未注入 / 无法 canonicalize）→ **全段保守匹配**（fail-closed：
    //   宁可少读一个文件，凭据防护不因基准缺失而失效；也不让一个坏基准把全部
    //   预览打成 NotFound——根因回归锁，生产曾在 3d22e2e 传 None 致黑名单整段跳过）
    //
    // 判定内核已抽出为 [`sensitive_rejected`]（H10 Task 12 复用点：项目路径选择
    // 必须与文件预览**同一份黑名单**，不得另造第二份）——本处行为逐字不变。
    let home_canon = home.and_then(|h| Path::new(h).canonicalize().ok());
    if sensitive_rejected(&canon, home_canon.as_deref(), cfg!(windows)) {
        return Err(FileRejectReason::Sensitive);
    }
    let meta = canon.metadata().map_err(|_| FileRejectReason::NotFound)?;
    if !meta.is_file() {
        return Err(FileRejectReason::NotFile);
    }
    let ext = canon.extension().and_then(|e| e.to_str()).unwrap_or("");
    let mime = mime_from_ext(ext);
    // 双阈值（进度台账 #11 裁决）：图片扩展名 5MB / 其余 500KB
    let max = if mime.starts_with("image/") {
        MAX_IMAGE_BYTES
    } else {
        MAX_TEXT_BYTES
    };
    if meta.len() > max {
        return Err(FileRejectReason::TooLarge);
    }
    let bytes = std::fs::read(&canon).map_err(|_| FileRejectReason::Io)?;
    Ok((bytes, mime))
}

/// 敏感路径判定内核（纯函数，平台语义可注入）：路径**相对主目录**的每一段
/// 命中 SENSITIVE_DIRS 即拒（含嵌套 `.config/gh/hosts.yml`）。分隔符双态
/// （`/` 与 `\`）、Windows 语义大小写不敏感、剥 verbatim 前缀——macOS 形态
/// `/Users/x/.ssh/id_rsa` 与 Windows 形态 `C:\Users\x\.ssh\id_rsa` 一并覆盖
fn is_sensitive_path(child: &str, home: &str, windows: bool) -> bool {
    any_segment_hit(child, home, SENSITIVE_DIRS, windows)
}

/// fail-closed 全段面（基准不可用）：仅凭据目录参与（见 CREDENTIAL_DIRS 注释）
fn is_credential_path(child: &str, windows: bool) -> bool {
    any_segment_hit(child, "/", CREDENTIAL_DIRS, windows)
}

/// 黑名单判定**内核**（纯函数，平台语义注入；H10 Task 12 自 [`read_file_safe`] 抽出，
/// 行为逐字不变——抽出理由 = 项目路径选择必须与文件预览**同一份黑名单**，不得另造）：
/// - 基准可用：路径在主目录内 ∧ 命中 [`SENSITIVE_DIRS`] ∧ **不在**豁免产物子树
///   （[`EXEMPT_SUBPATHS`]）⇒ 拒；
/// - 基准不可用：fail-closed 全段匹配（仅凭据目录，见 [`CREDENTIAL_DIRS`] 注释）。
pub(crate) fn sensitive_rejected(child: &Path, home_canon: Option<&Path>, windows: bool) -> bool {
    match home_canon {
        Some(home) => {
            // firmlink 折叠与归属/段匹配收敛在 sensitive_under_home（纯函数、
            // 跨平台 CI 锁定——CI 无 macOS runner，平台门控的端到端锁永不执行）
            // T6：黑名单命中但落在豁免产物子树（.claude/plans 等）→ 放行预览
            sensitive_under_home(child, home, windows)
                && !exempt_subpath_under_home(child, home, windows)
        }
        None => is_credential_path(&child.to_string_lossy(), windows),
    }
}

/// **项目路径**黑名单判定（H10 Task 12 的**唯一复用口**）：与文件预览同源同口径
/// （[`sensitive_rejected`]）。
///
/// 与 [`read_file_safe`] 的差别**只有解析深度**（复审 Minor 1 修正）：预览判的是
/// `canonicalize` 后的真实 inode；本函数面对的是「可能还不存在」的目录，故按
/// **尽力解 symlink** 的顺序解析（[`resolve_for_blacklist`]）——字面路径在非敏感处、
/// 却**指向**敏感目录的链接必须照拦（与文件预览同结论），叶节点不存在时至少解掉父级链接。
/// `home` 基准仍经 canonicalize（与预览同源，含 macOS firmlink 折叠）；读不到即
/// **fail-closed 全段匹配**——基准缺失不得让黑名单整段失效。
pub(crate) fn project_path_rejected(child: &Path, home: Option<&Path>, windows: bool) -> bool {
    let home_canon = home.and_then(|h| h.canonicalize().ok());
    let resolved = resolve_for_blacklist(child);
    sensitive_rejected(&resolved, home_canon.as_deref(), windows)
}

/// 待判路径的**尽力解析**（只解链接，不改变 fail-closed 方向）：
/// ① 路径在场 → 直接 `canonicalize`（解掉链上全部 symlink/junction，与文件预览同结论）；
/// ② 叶节点不在场 → `canonicalize` 其**父目录**再拼回叶名（父级是链接时同样解掉）；
/// ③ 都解不了（父目录也不在场/无权限）→ 原样返回字面路径（段匹配本就基于字面路径，
/// 且此形态与修复前行为一致，不会把可读目录误拦）。
fn resolve_for_blacklist(child: &Path) -> PathBuf {
    if let Ok(c) = child.canonicalize() {
        return c;
    }
    if let (Some(parent), Some(name)) = (child.parent(), child.file_name()) {
        if let Ok(p) = parent.canonicalize() {
            return p.join(name);
        }
    }
    child.to_path_buf()
}

/// 豁免子路径判定（T6 纯函数，平台语义可注入）：路径相对主目录的 rel 前缀命中
/// EXEMPT_SUBPATHS 且段边界精确（`{entry}/` 前缀——`plans-x` 不吃 `plans` 的豁免）。
/// 仅主目录内生效（fail-closed 基准缺失分支不豁免——无法验证落点时保守照拦）；
/// 归一口径与 any_segment_hit 同源（双分隔符/verbatim 剥离/Windows 大小写）
fn exempt_subpath_under_home(child: &Path, home_base: &Path, windows: bool) -> bool {
    let child = fold_data_volume_alias(child);
    let base = fold_data_volume_alias(home_base);
    if !path_within(&child, &base) {
        return false;
    }
    let norm = |s: &str| -> String {
        let s = if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
            format!(r"\\{rest}")
        } else if let Some(rest) = s.strip_prefix(r"\\?\") {
            rest.to_string()
        } else {
            s.to_string()
        };
        let mut s = s.replace('\\', "/");
        while s.len() > 1 && s.ends_with('/') {
            s.pop();
        }
        if windows {
            s.to_lowercase()
        } else {
            s.to_string()
        }
    };
    let rel = norm(&child.to_string_lossy())
        .strip_prefix(&norm(&base.to_string_lossy()))
        .map(|r| r.trim_start_matches('/').to_string())
        .unwrap_or_default();
    let rel_segs: Vec<&str> = rel.split('/').filter(|s| !s.is_empty()).collect();
    EXEMPT_SUBPATHS.iter().any(|e| {
        let e = if windows {
            e.to_lowercase()
        } else {
            e.to_string()
        };
        // ① 前缀形态（既有）：`.claude/plans/...` —— rel 以该子路径开头
        if rel.starts_with(&format!("{e}/")) {
            return true;
        }
        // ② 尾段序列形态（批次丙 T7）：`.kimi-code/…/agents/main/plans/<f>.md`
        // 这类**路径前缀含变量段**（会话 id）的产物目录接不住前缀形态，改用
        // **尾段序列**匹配：rel 的**连续段序列**与豁免项段序列全等。
        //
        // 段精确不放松：比对的是完整段序列（`plans-x` 不等于 `plans`；
        // `agents/main/plans` 只在**连续同序**出现时命中）。凭据面照拦——
        // 豁免项本身写死为产物目录（见 EXEMPT_SUBPATHS 注释），不含会话/凭据目录。
        let pat: Vec<&str> = e.split('/').filter(|s| !s.is_empty()).collect();
        if pat.is_empty() || pat.len() > rel_segs.len() {
            return false;
        }
        rel_segs
            .windows(pat.len())
            .any(|w| w.iter().zip(&pat).all(|(a, b)| a == b))
    })
}

/// 段匹配内核：child 相对 home 的路径段（home 之下取相对段，否则全段）与给定
/// 目录清单精确比对（平台语义大小写注入）
fn any_segment_hit(child: &str, home: &str, dirs: &[&str], windows: bool) -> bool {
    let norm = |s: &str| -> String {
        let s = if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
            format!(r"\\{rest}")
        } else if let Some(rest) = s.strip_prefix(r"\\?\") {
            rest.to_string()
        } else {
            s.to_string()
        };
        let mut s = s.replace('\\', "/");
        while s.len() > 1 && s.ends_with('/') {
            s.pop();
        }
        if windows {
            s.to_lowercase()
        } else {
            s
        }
    };
    let c = norm(child);
    let h = norm(home);
    // 相对主目录的部分（不在 home 之下时——cwd 内路径——用全段匹配，
    // 项目目录里叫 .config 的子目录同样按敏感处理，宁可少读一个文件）
    let rel = c
        .strip_prefix(&h)
        .map(|r| r.trim_start_matches('/').to_string())
        .unwrap_or_else(|| c.trim_start_matches('/').to_string());
    rel.split('/').any(|seg| {
        let seg = if windows {
            seg.to_lowercase()
        } else {
            seg.to_string()
        };
        dirs.iter().any(|d| {
            let d = if windows {
                d.to_lowercase()
            } else {
                (*d).to_string()
            };
            seg == d
        })
    })
}

/// 路径包含判定（canonicalize 之后的双方）：Windows 语义自动分派
fn path_within(child: &Path, ancestor: &Path) -> bool {
    path_within_semantics(
        &child.to_string_lossy(),
        &ancestor.to_string_lossy(),
        cfg!(windows),
    )
}

/// 路径包含判定内核（字符串级、平台语义可注入——darwin 上可测全分支）：
/// - 剥 Windows verbatim 前缀（`\\?\UNC\` → `\\`、`\\?\` → 空），统一分隔符为 `/`，
///   去尾部分隔符；Windows 语义整体转小写（文件系统大小写不敏感）；
/// - 前缀命中后剩余必须为空或以 `/` 开头（`/ab` 不在 `/a` 之下——分隔符边界）；
/// - 祖先为根（`/`）时任意绝对路径都包含（根即一切）。
fn path_within_semantics(child: &str, ancestor: &str, windows: bool) -> bool {
    let norm = |s: &str| -> String {
        // 剥 verbatim 前缀（UNC 形态还原为 `\\server\share` 常规形态再统一分隔符）
        let s = if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
            format!(r"\\{rest}")
        } else if let Some(rest) = s.strip_prefix(r"\\?\") {
            rest.to_string()
        } else {
            s.to_string()
        };
        let mut s = s.replace('\\', "/");
        while s.len() > 1 && s.ends_with('/') {
            s.pop();
        }
        if windows {
            s.to_lowercase()
        } else {
            s
        }
    };
    let c = norm(child);
    let a = norm(ancestor);
    if a.is_empty() {
        return false;
    }
    if a == "/" {
        return c.starts_with('/');
    }
    match c.strip_prefix(&a) {
        Some(rest) => rest.is_empty() || rest.starts_with('/'),
        None => false,
    }
}

/// 按扩展名给 MIME（预览渲染分支依据；未知扩展回落 text/plain——按纯文本
/// 渲染比误标 image/html 安全）
fn mime_from_ext(ext: &str) -> String {
    let lower = ext.to_ascii_lowercase();
    match lower.as_str() {
        "md" => "text/markdown",
        "rs" => "text/rust",
        "ts" | "tsx" => "text/typescript",
        "js" | "mjs" | "cjs" => "text/javascript",
        "jsx" => "text/jsx",
        "py" => "text/python",
        "go" => "text/go",
        "java" => "text/java",
        "c" | "h" => "text/c",
        "cpp" | "cc" | "hpp" => "text/cpp",
        "css" => "text/css",
        "sh" => "text/shell",
        "toml" => "text/toml",
        "yaml" | "yml" => "text/yaml",
        "json" => "application/json",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        _ => "text/plain",
    }
    .to_string()
}

// ============================================================
// 路径提取（extract_file_paths）
// ============================================================

/// 生产薄壳：真实 home + env 重定向 → 注入核（zcode_home_with 先例）。
/// env 双参（DSH_HOME / KIMI_CODE_HOME）与 /session-messages 生产薄壳**同一归口**
/// （content::read_env_homes，终审 Important 1）——设了 env 的机器上文件面板与
/// 详情页消息同源，不再静默空表。limit 为追溯档位（默认值由 API 层给）。
/// 提取失败（存储不可读 / 会话不存在）一律返回空表——文件面板是增强能力，
/// 不因提取失败阻塞详情页
pub fn extract_file_paths(
    agent_type: &str,
    session_id: &str,
    limit: usize,
) -> (Vec<FileEntry>, bool) {
    let Some(home) = dirs::home_dir() else {
        return (Vec::new(), false);
    };
    let (dsh_env, kimi_env) = super::content::read_env_homes();
    extract_file_paths_with_env(
        &home,
        dsh_env.as_deref(),
        kimi_env.as_deref(),
        agent_type,
        session_id,
        limit,
    )
}

/// 注入核（测试直调 tempdir home，零真实数据目录接触；env 重定向恒 None）
pub fn extract_file_paths_with(
    home: &Path,
    agent_type: &str,
    session_id: &str,
    limit: usize,
) -> (Vec<FileEntry>, bool) {
    extract_file_paths_with_env(home, None, None, agent_type, session_id, limit)
}

/// env 注入核（终审 Important 1）：与 content::read_session_messages_impl 同款 env
/// 双参——dsh/kimi 数据根重定向可注入（env 值作参，测试锁重定向链路，零真实 env
/// 接触）。生产薄壳不走 None：extract_file_paths 经 content::read_env_homes 读真实值。
///
/// M3+（用户裁决 2）：limit 直接**透传** read_session_messages_impl——条数窗、
/// 字节窗（512KB×⌈limit/200⌉ 封顶 4MB）与 truncated 全部复用主会话同一套机制，
/// 本层零新增预算代码；返回值第二项即该 truncated（前端据此提示「还有更早文件」）
pub(crate) fn extract_file_paths_with_env(
    home: &Path,
    dsh_env_home: Option<&str>,
    kimi_env_home: Option<&str>,
    agent_type: &str,
    session_id: &str,
    limit: usize,
) -> (Vec<FileEntry>, bool) {
    // 复用 Task 7 的八工具统一出口（数据同源：与详情页读的是同一份消息流），
    // 不另立 per-tool 查询（控制者裁决）；派发核 pub(crate) 同源复用
    match super::content::read_session_messages_impl(
        home,
        dsh_env_home,
        kimi_env_home,
        agent_type,
        session_id,
        limit,
    ) {
        Ok(pg) => {
            let mut merger = EntryMerger::new(cfg!(windows));
            merger.absorb_messages(&pg.messages);
            // M5 B2：zcode 用户附件第二抽取源——part 表 type=file（B1 调研）。
            // 该形态不在统一消息流的文本里（content.rs 只拼 text parts），故走
            // content 层专用查询拿「(窗口序号, 路径或 artifact URI, ts)」，序号与
            // SessionMessage.seq 同刻度（同一窗口查询），合并排序语义一致。
            // 其它工具用户附件已随 user 正文内联标记覆盖（absorb_messages）。
            if agent_type == "zcode" {
                for (seq, r#ref, ts) in
                    super::content::read_zcode_attachment_refs_with(home, session_id, limit)
                {
                    // artifact URI（粘贴截图，无原始路径）→ 解析 artifacts 目录磁盘实体；
                    // 解析不到（实体已清理）→ 降级跳过（不可预览的条目无展示价值）
                    let path = match r#ref.strip_prefix("zcode-artifact://") {
                        Some(tail) => match resolve_zcode_artifact(home, tail) {
                            Some(p) => p,
                            None => continue,
                        },
                        None => r#ref,
                    };
                    merger.merge(&path, seq, ts, FileOrigin::User);
                }
            }
            (merger.finish(), pg.truncated)
        }
        Err(_) => (Vec::new(), false),
    }
}

/// zcode artifact URI（`<session_id>/<tool-result-<uuid>>` 尾段）→ artifacts 目录
/// 磁盘实体路径。实测落盘形态 `~/.zcode/cli/artifacts/<session>/…-<tool-result-uuid>.<ext>`
/// （文件名尾段内嵌 uuid，B1 调研），故按「文件名包含尾段」匹配；多命中取字典序
/// 最小（保证确定性；uuid 全局唯一，碰撞纯理论）。目录不可读 / 无命中 → None
/// （调用方降级跳过）
fn resolve_zcode_artifact(home: &Path, tail: &str) -> Option<String> {
    let (sess, name) = tail.split_once('/')?;
    if sess.is_empty() || name.is_empty() {
        return None;
    }
    let dir = home.join(".zcode").join("cli").join("artifacts").join(sess);
    let mut hit: Option<String> = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let fname = entry.file_name();
        if fname.to_string_lossy().contains(name) {
            let p = entry.path().to_string_lossy().into_owned();
            if hit.as_ref().is_none_or(|h| p < *h) {
                hit = Some(p);
            }
        }
    }
    hit
}

/// 文件来源三池（M5 决策 10 / 线稿来源筛选）：user=用户上传/引用（附件抽取，
/// B1 调研口径）；tool_read=工具只读；tool_write=工具改写。
/// 同一文件多来源命中取信息量最大者（tool_write > tool_read > user）
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FileOrigin {
    User,
    ToolRead,
    ToolWrite,
}

impl FileOrigin {
    /// 合并优先级（数值大者胜）：上传后被工具改写 → 显示「工具读写」
    fn rank(self) -> u8 {
        match self {
            FileOrigin::User => 0,
            FileOrigin::ToolRead => 1,
            FileOrigin::ToolWrite => 2,
        }
    }
}

/// 文件条目（M3+ 文件面板后端富化契约，camelCase 序列化 = 移动端字段名，勿改）
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileEntry {
    /// 文件路径：**最后一次出现**的原始形态（归一化只用于去重比较，用户裁决 7）
    pub path: String,
    /// 最后出现条目的 seq（finalize 重排后的数组内序，稳定可排序）
    pub last_seq: i64,
    /// 最后出现条目的时间戳（原生存储无时间戳则为 None，前端显示 `—`）
    pub last_ts: Option<i64>,
    /// 该文件在窗口内出现的次数（hits=1 前端不显示徽标）
    pub hits: usize,
    /// 是否**被改写**过（2026-09-16 用户裁决）：窗口内只要有一次写类工具
    /// （Edit/Write/apply_patch 等）触达即为 true；仅被读类工具（Read/Grep/
    /// Glob/view_image…）触达为 false。前端据此把「仅读过」的文件名渲成
    /// 常规色（仍是超链接，只是视觉上区分「只是看过」与「动过手」）。
    /// M5 B2 起与 origin 同步：modified == (origin == ToolWrite)
    pub modified: bool,
    /// 来源三池（M5 B2）：user / tool_read / tool_write（snake_case 序列化）
    pub origin: FileOrigin,
}

/// 读类工具白名单（**小写**比较）：只读取文件、不修改内容。
/// 判定读写的唯一依据是工具名——不在白名单内的（含工具名缺失）一律按
/// 已改写处理（fail-safe：宁可把只读文件标成改写，也不要让真正被改过的
/// 文件看起来「只是读过」）。名单按各工具实测形态收集：
/// claude/codex/workbuddy 的 Read/Grep/Glob/view_image、kimi 的 Read/
/// ReadMediaFile、zcode/opencode/openclaw 的 read 类工具名
const READ_ONLY_TOOLS: &[&str] = &[
    "read",
    "readfile",
    "readmediafile",
    "read_image",
    "view_image",
    "view",
    "cat",
    "grep",
    "glob",
    "search",
    "find",
    "list",
    "ls",
    "webfetch",
    "fetch",
];

/// 工具名是否为读类（大小写不敏感；None → false = 按已改写处理）
fn is_read_only_tool(name: Option<&str>) -> bool {
    match name {
        Some(n) => {
            let lower = n.to_lowercase();
            READ_ONLY_TOOLS.contains(&lower.as_str())
        }
        None => false,
    }
}

/// 归一化比较键（去重用，用户裁决 7）：`\`→`/`；Windows 语义再 to_lowercase。
/// 平台语义以参数注入便于跨平台测试（path_within_semantics 先例），生产 cfg!(windows)
fn normalize_key(path: &str, windows: bool) -> String {
    let unified = path.replace('\\', "/");
    if windows {
        unified.to_lowercase()
    } else {
        unified
    }
}

/// 从统一消息流提取文件条目（纯函数，M3+ 富化 + M5 B2 来源三池）：
/// - kind=="tool-call"：toolArgs JSON（PATH_KEYS 递归收集，既有逻辑不变）→
///   按读写白名单定 origin（tool_write / tool_read）；
/// - kind=="user"：正文里的内联附件标记（B1 调研 kimi 实测形态
///   `<image path="X">` / `<file path="X">`，通用扫描对其它工具无害）→ origin=user。
///
/// 按归一化键去重，记录最后出现 seq/ts 与 hits，输出按 last_seq 降序（最近出现的在上）。
/// 生产入口（平台语义取编译目标）
pub fn extract_paths_from_messages(msgs: &[SessionMessage]) -> Vec<FileEntry> {
    extract_paths_from_messages_with(msgs, cfg!(windows))
}

/// 平台语义注入核（darwin 上可测全分支）
pub fn extract_paths_from_messages_with(msgs: &[SessionMessage], windows: bool) -> Vec<FileEntry> {
    let mut merger = EntryMerger::new(windows);
    merger.absorb_messages(msgs);
    merger.finish()
}

/// 合并管线状态（M5 B2 重构为结构体）：去重索引 + 条目集 + 平台语义。
/// 消息流抽取与 zcode 附件第二抽取源共用同一实例——跨来源去重/优先级合并
/// 才能全局生效（同一文件被上传又被工具改写 → 单条目 origin=tool_write）
pub(crate) struct EntryMerger {
    index: HashMap<String, usize>,
    out: Vec<FileEntry>,
    windows: bool,
}

impl EntryMerger {
    pub(crate) fn new(windows: bool) -> Self {
        Self {
            index: HashMap::new(),
            out: Vec::new(),
            windows,
        }
    }

    /// 消息流吸收（tool-call 参数路径 + user 正文内联附件标记）
    pub(crate) fn absorb_messages(&mut self, msgs: &[SessionMessage]) {
        for m in msgs {
            match m.kind.as_str() {
                "tool-call" => {
                    let Some(args) = &m.tool_args else { continue };
                    let Ok(v) = serde_json::from_str::<serde_json::Value>(args) else {
                        continue; // 参数损坏 → 跳过该条（防御）
                    };
                    let mut paths: Vec<String> = Vec::new();
                    let mut seen_in_msg: HashSet<String> = HashSet::new();
                    collect_path_values(&v, &mut paths, &mut seen_in_msg);
                    // 同一消息内同一文件多次命中只计一次（一条 tool-call 里重复键不算多次使用）
                    let origin = if is_read_only_tool(m.tool_name.as_deref()) {
                        FileOrigin::ToolRead
                    } else {
                        FileOrigin::ToolWrite
                    };
                    for path in paths {
                        self.merge(&path, m.seq, m.ts, origin);
                    }
                }
                "user" => {
                    // M5 B2：用户消息内联附件标记（kimi 贴图/引用文件形态）
                    for path in extract_inline_markup_paths(&m.content) {
                        self.merge(&path, m.seq, m.ts, FileOrigin::User);
                    }
                }
                _ => {}
            }
        }
    }

    /// 单路径并入：命中更新 last_seq/ts/hits 与来源优先级（优先级 tool_write
    /// 最高，其次 tool_read，user 最低——B2）；未命中建行。modified 与
    /// origin==ToolWrite 同步（字段保留为前端既有消费面）。
    pub(crate) fn merge(&mut self, path: &str, seq: i64, ts: Option<i64>, origin: FileOrigin) {
        let key = normalize_key(path, self.windows);
        match self.index.get(&key) {
            Some(&i) => {
                let e = &mut self.out[i];
                e.hits += 1;
                e.last_seq = seq;
                e.last_ts = ts;
                e.path = path.to_string(); // 展示保留最后一次出现的原始形态
                if origin.rank() > e.origin.rank() {
                    e.origin = origin;
                }
                // 写优先：写过之后再读也不撤销「已改写」
                e.modified = e.origin == FileOrigin::ToolWrite;
            }
            None => {
                self.index.insert(key, self.out.len());
                self.out.push(FileEntry {
                    path: path.to_string(),
                    last_seq: seq,
                    last_ts: ts,
                    hits: 1,
                    modified: origin == FileOrigin::ToolWrite,
                    origin,
                });
            }
        }
    }

    /// 收官：排序键 = 会话出现序（lastSeq 降序），用户裁决 1；同 seq 保持插入序（稳定）
    pub(crate) fn finish(self) -> Vec<FileEntry> {
        let mut out = self.out;
        out.sort_by_key(|b| std::cmp::Reverse(b.last_seq));
        out
    }
}

/// kimi 实测内联附件标记（B1 调研）：`<image path="X">` / `<file path="X">` /
/// `src="X"` 变体。双引号属性（实测唯一定界形态）；逐标记扫描、无正则依赖。
/// 通用应用到全部工具的 user 正文——普通文本不含 `<image `/`<file ` 前缀，零误伤面
fn extract_inline_markup_paths(content: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for tag in ["<image ", "<file "] {
        let mut from = 0;
        while let Some(rel) = content[from..].find(tag) {
            let inner_start = from + rel + tag.len();
            let rest = &content[inner_start..];
            let inner_end = rest.find('>').unwrap_or(rest.len());
            let inner = &rest[..inner_end];
            for attr in ["path=\"", "src=\""] {
                if let Some(a) = inner.find(attr) {
                    let v = &inner[a + attr.len()..];
                    if let Some(q) = v.find('"') {
                        let p = v[..q].trim();
                        // URI 判别跳过（blobref:/zcode-artifact: 等）：冒号前缀 >1 字符
                        // 即协议名（Windows 盘符恒单字符，不受影响）——内容寻址引用
                        // 不可按路径预览，不入池（B1 调研口径）
                        let is_uri = match p.find(':') {
                            Some(i) => i > 1,
                            None => false,
                        };
                        if !p.is_empty()
                            && !is_uri
                            && is_path_candidate(p)
                            && seen.insert(p.to_string())
                        {
                            out.push(p.to_string());
                        }
                    }
                }
            }
            from = inner_start + inner_end;
        }
    }
    out
}

/// 递归走 JSON 树：对象键命中 PATH_KEYS 时收字符串值，其余结构下钻。
/// `seen` 为**单条消息内**的判重集（跨消息去重由 extract_paths_from_messages_with
/// 按归一化键完成——同一条 tool-call 里同路径重复键不该计成多次使用）
pub(crate) fn collect_path_values(
    v: &serde_json::Value,
    out: &mut Vec<String>,
    seen: &mut HashSet<String>,
) {
    match v {
        serde_json::Value::Object(map) => {
            for (k, val) in map {
                if PATH_KEYS.contains(&k.as_str()) {
                    if let Some(s) = val.as_str() {
                        if is_path_candidate(s) && seen.insert(s.to_string()) {
                            out.push(s.to_string());
                        }
                        continue; // 命中键的字符串值已收，不再下钻
                    }
                    // 命中键的非字符串值（对象/数组包裹形态）继续下钻
                }
                collect_path_values(val, out, seen);
            }
        }
        serde_json::Value::Array(items) => {
            for it in items {
                collect_path_values(it, out, seen);
            }
        }
        _ => {}
    }
}

/// 路径候选判定（控制者裁决口径）：含路径分隔符**或**带 `.扩展名`；排除空串 /
/// 超长 / 含换行与 NUL / URL（`://`，MCP 参数常见，非本地文件）。
fn is_path_candidate(s: &str) -> bool {
    let s = s.trim();
    if s.is_empty() || s.len() > MAX_PATH_LEN {
        return false;
    }
    if s.contains(['\n', '\r', '\0']) {
        return false;
    }
    if s.contains("://") {
        return false;
    }
    s.contains('/') || s.contains('\\') || has_ext_like(s)
}

/// 形如带扩展名的文件名：末段含 `.字母数字后缀`（1-12 位）。
/// 让 "README.md" / "Cargo.toml" 这类裸文件名也算候选（read_file_safe 会按
/// 会话 cwd 解析相对路径）
fn has_ext_like(s: &str) -> bool {
    let last = s.rsplit(['/', '\\']).next().unwrap_or(s);
    match last.rsplit_once('.') {
        Some((stem, ext)) => {
            !stem.is_empty()
                && !ext.is_empty()
                && ext.len() <= 12
                && ext.chars().all(|c| c.is_ascii_alphanumeric())
        }
        None => false,
    }
}

// ============================================================
// 测试（Task 8 Step 1）：一律 tempdir / 合成数据，零真实数据目录接触。
// Windows 语义以参数注入，darwin 上全分支可测。
// ============================================================
#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::content::SessionMessage;

    /// content.rs 的构造器是私有的——测试本地直接按字段构造（字段全 pub，勿改生产可见性）
    fn tool_call(args: &str) -> SessionMessage {
        SessionMessage {
            seq: 1,
            role: "assistant".into(),
            kind: "tool-call".into(),
            content: "调用 Write".into(),
            ts: Some(1),
            tool_name: Some("Write".into()),
            tool_args: Some(args.into()),
            collapsed: true,
        }
    }

    fn text_msg(kind: &str, content: &str) -> SessionMessage {
        SessionMessage {
            seq: 0,
            role: if kind == "user" { "user" } else { "assistant" }.into(),
            kind: kind.into(),
            content: content.into(),
            ts: Some(1),
            tool_name: None,
            tool_args: None,
            collapsed: kind == "thinking",
        }
    }

    // ==== read_file_safe ====

    #[test]
    fn read_file_safe_allows_file_inside_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("hello.txt");
        std::fs::write(&f, "hello mam").unwrap();
        let (bytes, mime) = read_file_safe(
            tmp.path().to_str().unwrap(),
            f.to_str().unwrap(),
            Some(tmp.path().to_str().unwrap()),
        )
        .unwrap();
        assert_eq!(bytes, b"hello mam");
        assert_eq!(mime, "text/plain");
    }

    #[test]
    fn read_file_safe_resolves_relative_path_against_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(tmp.path().join("src").join("m.rs"), "fn main() {}").unwrap();
        let (bytes, mime) = read_file_safe(
            tmp.path().to_str().unwrap(),
            "src/m.rs",
            Some(tmp.path().to_str().unwrap()),
        )
        .unwrap();
        assert_eq!(bytes, b"fn main() {}");
        assert_eq!(mime, "text/rust");
    }

    #[test]
    fn read_file_safe_rejects_missing_and_dir() {
        // M5 P2-a 全盘语义：cwd 外真实文件**放行**（见 allows_outside_project 测试）；
        // 本测试聚焦「不存在 / 目录 / 空参」三类结构性拒绝
        let outer = tempfile::tempdir().unwrap();
        let proj = outer.path().join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        let cwd = proj.to_str().unwrap();
        // (1) 不存在（../ 相对形态与绝对形态一致：NotFound）
        let r = read_file_safe(cwd, "../nope.txt", None);
        assert_eq!(r.unwrap_err(), FileRejectReason::NotFound);
        // (2) 目录（canonicalize 成功但非常规文件）
        let r = read_file_safe(cwd, ".", Some(cwd));
        assert_eq!(r.unwrap_err(), FileRejectReason::NotFile);
        // (3) 空参
        assert_eq!(
            read_file_safe(cwd, "  ", Some(cwd)).unwrap_err(),
            FileRejectReason::NotFound
        );
    }

    /// 双阈值（进度台账 #11）：文本 500KB 上限、图片 5MB 上限——
    /// 各造阈值两侧样本，图片在「文本会拒」的体量下必须放行
    #[test]
    fn read_file_safe_enforces_dual_size_thresholds() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        // 文本：500KB 恰好放行、+1 字节拒绝
        let txt = tmp.path().join("big.txt");
        std::fs::write(&txt, vec![b'a'; 500 * 1024]).unwrap();
        assert!(
            read_file_safe(cwd, txt.to_str().unwrap(), Some(cwd)).is_ok(),
            "恰 500KB 放行"
        );
        std::fs::write(&txt, vec![b'a'; 500 * 1024 + 1]).unwrap();
        assert!(
            read_file_safe(cwd, txt.to_str().unwrap(), Some(cwd)).is_err(),
            "超 500KB 拒"
        );
        // 图片：600KB（超文本上限、低于图片上限）必须放行；>5MB 拒
        let png = tmp.path().join("pic.png");
        std::fs::write(&png, vec![0u8; 600 * 1024]).unwrap();
        let (bytes, mime) = read_file_safe(cwd, png.to_str().unwrap(), Some(cwd)).unwrap();
        assert_eq!(mime, "image/png");
        assert_eq!(bytes.len(), 600 * 1024);
        std::fs::write(&png, vec![0u8; 5 * 1024 * 1024 + 1]).unwrap();
        assert!(
            read_file_safe(cwd, png.to_str().unwrap(), Some(cwd)).is_err(),
            "超 5MB 拒"
        );
    }

    /// 全盘放开（2026-09-18 用户裁决）：项目外 / 主目录外的文件**放行**——
    /// 经常需要查看/参考项目目录外文件（微信目录附件等实测场景）；
    /// 访问门槛 = PIN + 设备 cookie，路径级防线收敛为敏感目录黑名单
    #[test]
    fn read_file_safe_allows_outside_project_and_home() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let proj = home.join("Desktop").join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        let dl = home.join("Downloads");
        std::fs::create_dir_all(&dl).unwrap();
        let pic = dl.join("trip-share.png");
        std::fs::write(&pic, b"\x89PNG").unwrap();
        let cwd = proj.to_str().unwrap();

        // 主目录内、项目目录外（Downloads）→ 放行
        let (bytes, mime) =
            read_file_safe(cwd, pic.to_str().unwrap(), Some(home.to_str().unwrap()))
                .unwrap_or_else(|e| panic!("主目录内文件必须放行: {e:?}"));
        assert_eq!(bytes, b"\x89PNG");
        assert_eq!(mime, "image/png");

        // 反斜杠形态路径（Windows 会话里 toolArgs 记录的形态）同样放行。
        // 仅 Windows 跑：Linux 下反斜杠是合法文件名字符，此检查不适用
        if cfg!(windows) {
            let win_style = pic.to_string_lossy().replace('/', "\\");
            assert!(
                read_file_safe(cwd, &win_style, Some(home.to_str().unwrap())).is_ok(),
                "反斜杠路径必须与正斜杠同判"
            );
        }

        // 主目录之外（同级另一棵树的文件）→ **同样放行**（全盘语义）
        let outside = tmp.path().join("outside.txt");
        std::fs::write(&outside, "x").unwrap();
        let (bytes, _) =
            read_file_safe(cwd, outside.to_str().unwrap(), Some(home.to_str().unwrap()))
                .unwrap_or_else(|e| panic!("全盘语义下主目录外文件必须放行: {e:?}"));
        assert_eq!(bytes, b"x");
        // home=None ⇒ 基准不可用 → 全段保守匹配分支：outside.txt 不含敏感段，
        // 故仍可读（全盘语义不变；黑名单对含敏感段路径照拒——见
        // sensitive_check_still_applies_when_home_base_unusable）
        let r = read_file_safe(cwd, outside.to_str().unwrap(), None);
        assert!(r.is_ok(), "home=None 不再构成边界（全盘语义）");
    }

    /// 敏感目录拒绝清单（2026-09-16 用户裁决）：主目录内但这些目录下的文件
    /// 一律拒绝——配对手机不得读取密钥与凭据
    #[test]
    fn read_file_safe_rejects_sensitive_dirs_inside_home() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let proj = home.join("Desktop").join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        let cwd = proj.to_str().unwrap();
        let home_s = home.to_str().unwrap();

        for dir in [
            ".ssh",
            ".aws",
            ".gnupg",
            ".tuvis",
            ".claude",
            ".codex",
            ".kimi-code",
            ".zcode",
            ".dsh",
            ".config",
            ".docker",
            ".kube",
            ".npmrc",
        ] {
            let d = home.join(dir);
            std::fs::create_dir_all(&d).unwrap();
            let f = d.join("id_rsa");
            std::fs::write(&f, "PRIVATE").unwrap();
            let r = read_file_safe(cwd, f.to_str().unwrap(), Some(home_s));
            assert!(
                r.is_err(),
                "敏感目录 {dir} 下的文件必须拒绝（配对设备不得读凭据）"
            );
        }
        // 敏感目录的**同级前缀**目录不得被误伤（.ssh2 不是 .ssh）
        let lookalike = home.join(".ssh2");
        std::fs::create_dir_all(&lookalike).unwrap();
        let f = lookalike.join("notes.txt");
        std::fs::write(&f, "ok").unwrap();
        assert!(
            read_file_safe(cwd, f.to_str().unwrap(), Some(home_s)).is_ok(),
            "前缀相似目录（.ssh2）不得被误拒"
        );
    }

    /// 黑名单基准不可用时的保守语义：敏感段照拒——凭据防护不得因基准缺失而
    /// 失效（M5 P2-a 追记回归：生产曾传 None 致黑名单整段跳过，配对设备可读
    /// ~/.ssh）；且坏基准不得把一个普通文件读成 NotFound 错误（不打死预览）
    #[test]
    fn sensitive_check_still_applies_when_home_base_unusable() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path().join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        let cwd = proj.to_str().unwrap();

        // tmpdir 下造敏感段路径（不触真实主目录）
        let secret = tmp.path().join(".ssh").join("id_rsa");
        std::fs::create_dir_all(secret.parent().unwrap()).unwrap();
        std::fs::write(&secret, "PRIVATE").unwrap();
        let ok = tmp.path().join("notes.txt");
        std::fs::write(&ok, "ok").unwrap();

        // (1) home 未注入 → 敏感段必须拒（fail-closed）
        assert_eq!(
            read_file_safe(cwd, secret.to_str().unwrap(), None).unwrap_err(),
            FileRejectReason::Sensitive,
            "home 未知时敏感段必须照拒（fail-closed）"
        );
        // (2) home 给了但不存在（canonicalize 失败）→ 同样拒，且不得误报 NotFound
        assert_eq!(
            read_file_safe(cwd, secret.to_str().unwrap(), Some("/no-such-mam-home")).unwrap_err(),
            FileRejectReason::Sensitive,
            "坏基准下敏感段仍须拒（不得退化为 NotFound 跳过检查）"
        );
        // (3) 两者下普通文件仍可读（不误伤全盘语义、不打死预览）
        assert!(
            read_file_safe(cwd, ok.to_str().unwrap(), None).is_ok(),
            "基准不可用不得妨碍普通文件（全盘语义不变）"
        );
        assert!(
            read_file_safe(cwd, ok.to_str().unwrap(), Some("/no-such-mam-home")).is_ok(),
            "坏基准不得让普通文件读取整体失败"
        );
    }

    /// macOS firmlink 别名旁路回归锁（2026-09-18 复核阻塞项）：
    /// `canonicalize` 不解析 APFS firmlink（firmlink 非 symlink），
    /// `/System/Volumes/Data/<home>` 与 `<home>` 是**同 inode** 的两个字面形态——
    /// 只做字符串前缀比较会把主目录内路径误判为「主目录外」，按全盘裁决整段跳过
    /// 黑名单（2026-09-18 实测：别名可读 ~/.ssh/known_hosts，194 字节泄露）。
    /// 夹具走 tmpdir + 前缀合成（零真实主目录接触，M5 红线）。
    ///
    /// 夹具配方要点：tmpdir 原始形态是 `/var/...`，而 `/var` **不在** firmlink
    /// 清单（`/usr/share/firmlinks` 只有 `/private`）；必须先 `canonicalize()`
    /// 拿到 `/private/var/...` 再加前缀，别名才真实存在且同 inode（已实测）。
    #[cfg(target_os = "macos")]
    #[test]
    fn firmlink_alias_still_hits_sensitive_blacklist() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        std::fs::write(home.join(".ssh").join("id_rsa"), "PRIVATE").unwrap();
        std::fs::write(home.join("ok.txt"), "ok").unwrap();
        let h = home.to_str().unwrap();
        let alias = |tail: &str| format!("/System/Volumes/Data{h}/{tail}");

        // 夹具自检：别名必须真实存在（否则本锁是空转——把「存在性」也锁住）
        assert!(
            std::path::Path::new(&alias(".ssh/id_rsa")).exists(),
            "夹具别名必须真实存在（先 canonicalize 再加前缀，见本测试文档）"
        );

        // (1) 别名形态的凭据必须拒（本锁的靶心；修复前此处 Ok = 泄露）
        assert_eq!(
            read_file_safe(h, &alias(".ssh/id_rsa"), Some(h)).unwrap_err(),
            FileRejectReason::Sensitive,
            "firmlink 别名不得绕过敏感黑名单（2026-09-18 复核阻塞项）"
        );
        // (2) 别名形态的普通文件仍可读（不得靠全盘拒绝蒙混过关）
        assert!(
            read_file_safe(h, &alias("ok.txt"), Some(h)).is_ok(),
            "别名形态普通文件必须放行（黑名单不得误伤）"
        );
        // (3) 主目录**外**别名路径不设路径级防线（2026-09-18 裁决未收窄）
        let out = tempfile::tempdir().unwrap();
        let op = out.path().canonicalize().unwrap().join(".ssh");
        std::fs::create_dir_all(&op).unwrap();
        std::fs::write(op.join("n.txt"), "x").unwrap();
        let outside = format!("/System/Volumes/Data{}", op.join("n.txt").display());
        assert!(
            read_file_safe(h, &outside, Some(h)).is_ok(),
            "主目录外路径不设防线（裁决未被收窄）"
        );
    }

    /// firmlink 前缀折叠纯函数锁（跨平台——CI 只有 ubuntu 跑 `cargo test`，
    /// `#[cfg(target_os = "macos")]` 的端到端锁在 CI 永不执行，2026-09-18 复核
    /// Important）：别名形态折叠、恒等形态原样、挂载点本身归根。
    #[test]
    fn fold_data_volume_alias_folds_prefix_and_keeps_others() {
        // 别名形态 → 折叠为主目录字面
        assert_eq!(
            fold_data_volume_alias(Path::new("/System/Volumes/Data/Users/u/.ssh/id_rsa")),
            Path::new("/Users/u/.ssh/id_rsa")
        );
        // 恒等：非前缀路径原样返回
        assert_eq!(
            fold_data_volume_alias(Path::new("/Users/u/.ssh/id_rsa")),
            Path::new("/Users/u/.ssh/id_rsa")
        );
        // 挂载点本身 → 根
        assert_eq!(
            fold_data_volume_alias(Path::new("/System/Volumes/Data")),
            Path::new("/")
        );
    }

    /// 折叠 + 归属 + 段匹配的组合判定锁（跨平台，纯字符串无 IO）：
    /// 覆盖 CI 不可达的 firmlink 别名形态——折叠被还原为恒等时，别名用例必红；
    /// 同时锁定裁决边界（主目录外含敏感段名不设防线、主目录内非敏感不误伤）。
    /// T6（D9）：豁免子路径边界锁——`.claude/plans` 与 `.codex/plans` 子树放行
    /// （黑名单命中但豁免），同工具其余路径（会话/凭据）照拦；段精确不误伤
    /// （`plans-x` 不吃 `plans` 的豁免）。组合判定：sensitive 命中 ∧ 豁免成立
    /// ⇒ 调用方放行（files.rs 主路径的 `&& !exempt` 形态）
    #[test]
    fn exempt_subpaths_allow_plan_artifacts_only() {
        let home = Path::new("/Users/u");
        // 豁免产物：黑名单命中 ∧ 豁免成立 → 放行
        for p in [
            "/Users/u/.claude/plans/shiny-words.md",
            "/Users/u/.codex/plans/turn-1/018f-uuid/PLAN.md", // 子树嵌套（T0 盘点形态）
        ] {
            assert!(
                sensitive_under_home(Path::new(p), home, false),
                "{p} 应命中黑名单（否则豁免无从谈起）"
            );
            assert!(
                exempt_subpath_under_home(Path::new(p), home, false),
                "{p} 应豁免"
            );
        }
        // 照拦：同工具非豁免路径（会话/凭据/记忆）
        for p in [
            "/Users/u/.claude/projects/abc.jsonl",
            "/Users/u/.claude/credentials.yaml",
            "/Users/u/.codex/auth.json",
            "/Users/u/.codex/sessions/2026/09/20/rollout-x.jsonl",
        ] {
            assert!(
                sensitive_under_home(Path::new(p), home, false),
                "{p} 应照拦"
            );
            assert!(
                !exempt_subpath_under_home(Path::new(p), home, false),
                "{p} 不在豁免子树"
            );
        }
        // 段精确：`plans-x` 目录不吃 `plans` 的豁免（前后缀相似不误伤，黑名单
        // 同款裁决口径）
        let p = "/Users/u/.claude/plans-x/secret.md";
        assert!(sensitive_under_home(Path::new(p), home, false));
        assert!(
            !exempt_subpath_under_home(Path::new(p), home, false),
            "段精确：plans-x 不豁免"
        );
        // Windows 语义（大小写不敏感 + 反斜杠）
        assert!(exempt_subpath_under_home(
            Path::new("C:/Users/u/.Claude/Plans/a.md"),
            Path::new("C:/Users/u"),
            true
        ));
    }

    /// 批次丙 T7：**尾段序列匹配**——kimi 计划产物在深路径（前缀含会话 id 变量段），
    /// 前缀形态接不住，靠「豁免项作为连续尾段序列出现」命中；段精确性与凭据面
    /// 照旧（`plans-x` 不豁免、`.kimi-code` 其余路径照拦）
    #[test]
    fn exempt_tail_sequence_matches_kimi_plan_paths() {
        let home = Path::new("/Users/u");
        // 实机路径形态（本机实测样本）：wd_<proj>_<hash>/session_<uuid>/agents/main/plans/x.md
        let hit = "/Users/u/.kimi-code/sessions/wd_test_72c4040eb71a/session_46b9cd5f-c817-4b84-a7ef-ee1ff8a4c59d/agents/main/plans/miss-martian-she-hulk-beast.md";
        assert!(
            sensitive_under_home(Path::new(hit), home, false),
            "kimi 计划路径命中 .kimi-code 黑名单（否则豁免无从谈起）"
        );
        assert!(
            exempt_subpath_under_home(Path::new(hit), home, false),
            "T7：kimi 深路径计划文件应豁免（尾段序列 agents/main/plans）"
        );
        // 多级 plans 子目录同样命中（尾段序列不要求是叶子目录）
        assert!(exempt_subpath_under_home(
            Path::new("/Users/u/.kimi-code/x/y/agents/main/plans/sub/p.md"),
            home,
            false
        ));
        // 照拦：kimi 其余路径（会话数据/配置——凭据面不放松）
        for p in [
            "/Users/u/.kimi-code/config.toml",
            "/Users/u/.kimi-code/sessions/wd_x/session_y/agents/main/wire.jsonl",
            "/Users/u/.kimi-code/sessions/wd_x/session_y/agents/main/plans-x/secret.md",
            // 段序列不连续 → 不命中（`agents/plans/main` 不等于 `agents/main/plans`）
            "/Users/u/.kimi-code/sessions/x/agents/plans/main/f.md",
        ] {
            assert!(
                !exempt_subpath_under_home(Path::new(p), home, false),
                "{p} 应照拦"
            );
        }
        // Windows 形态（反斜杠 + 大写盘符）
        assert!(exempt_subpath_under_home(
            Path::new(r"C:\Users\u\.kimi-code\sessions\wd_x\session_y\agents\main\plans\p.md"),
            Path::new(r"C:\Users\u"),
            true
        ));
    }

    #[test]
    fn sensitive_home_hit_covers_firmlink_alias_and_keeps_ruling_boundary() {
        let home = Path::new("/Users/u");

        // firmlink 别名形态（主目录内凭据）→ 命中
        assert!(sensitive_under_home(
            Path::new("/System/Volumes/Data/Users/u/.ssh/id_rsa"),
            home,
            false
        ));
        // 直接形态（主目录内凭据）→ 命中
        assert!(sensitive_under_home(
            Path::new("/Users/u/.ssh/id_rsa"),
            home,
            false
        ));
        // Windows 盘符形态 → 命中（段匹配的 Windows 语义）
        assert!(sensitive_under_home(
            Path::new("C:\\Users\\u\\.ssh\\id_rsa"),
            Path::new("C:/Users/u"),
            true
        ));
        // 主目录外含敏感段名 → 不命中（2026-09-18 裁决：主目录外不设防线）
        assert!(!sensitive_under_home(
            Path::new("/System/Volumes/Data/Users/other/.ssh/id_rsa"),
            home,
            false
        ));
        // 主目录内非敏感 → 不命中（不误伤）
        assert!(!sensitive_under_home(
            Path::new("/Users/u/notes.txt"),
            home,
            false
        ));
    }

    /// 敏感清单匹配内核（纯函数，平台语义可注入）：
    /// - 命中清单任一段即拒（含嵌套 .config/xxx）；
    /// - macOS 形态（/Users/x/.ssh/id_rsa）与 Windows 形态（C:\Users\x\.ssh\id_rsa）
    ///   必须一并覆盖；大小写不敏感（Windows 语义）
    #[test]
    fn sensitive_path_check_covers_mac_and_windows_forms() {
        // macOS 形态（正斜杠）
        assert!(is_sensitive_path(
            "/Users/alice/.ssh/id_rsa",
            "/Users/alice",
            true
        ));
        assert!(is_sensitive_path(
            "/Users/alice/.aws/credentials",
            "/Users/alice",
            true
        ));
        // Windows 形态（反斜杠，大小写混合）
        assert!(is_sensitive_path(
            r"C:\Users\bunny\.SSH\id_rsa",
            r"C:\Users\bunny",
            true
        ));
        assert!(is_sensitive_path(
            r"C:\Users\bunny\.gnupg\secring.gpg",
            r"C:\Users\bunny",
            true
        ));
        // 嵌套命中：清单项出现在路径中段也算（~/.config/gh/hosts.yml）
        assert!(is_sensitive_path(
            "/Users/alice/.config/gh/hosts.yml",
            "/Users/alice",
            true
        ));
        // 普通文件不误拒
        assert!(!is_sensitive_path(
            "/Users/alice/Downloads/trip.png",
            "/Users/alice",
            true
        ));
        assert!(!is_sensitive_path(
            r"C:\Users\bunny\Desktop\proj\src\main.rs",
            r"C:\Users\bunny",
            true
        ));
        // 前缀相似不误拒（.ssh2 / .awsome）
        assert!(!is_sensitive_path(
            "/Users/alice/.ssh2/notes.txt",
            "/Users/alice",
            true
        ));
        assert!(!is_sensitive_path(
            "/Users/alice/.awsome/x.txt",
            "/Users/alice",
            true
        ));
        // 目录名恰好等于清单项本身（非其内文件）仍拒（目录不可预览，语义一致）
        assert!(is_sensitive_path("/Users/alice/.ssh", "/Users/alice", true));
    }

    #[test]
    fn mime_from_ext_covers_brief_list_and_falls_back() {
        assert_eq!(mime_from_ext("md"), "text/markdown");
        assert_eq!(mime_from_ext("rs"), "text/rust");
        assert_eq!(mime_from_ext("ts"), "text/typescript");
        assert_eq!(mime_from_ext("tsx"), "text/typescript");
        assert_eq!(mime_from_ext("js"), "text/javascript");
        assert_eq!(mime_from_ext("py"), "text/python");
        assert_eq!(mime_from_ext("png"), "image/png");
        assert_eq!(mime_from_ext("jpg"), "image/jpeg");
        assert_eq!(mime_from_ext("gif"), "image/gif");
        assert_eq!(mime_from_ext("svg"), "image/svg+xml");
        assert_eq!(mime_from_ext("json"), "application/json");
        // Task 8 评审 Minor①：yaml/yml 从 toml 拆出，不再误标 text/toml
        assert_eq!(mime_from_ext("toml"), "text/toml");
        assert_eq!(mime_from_ext("yaml"), "text/yaml");
        assert_eq!(mime_from_ext("yml"), "text/yaml");
        assert_eq!(mime_from_ext(""), "text/plain");
        assert_eq!(mime_from_ext("unknownxyz"), "text/plain");
        // 大写扩展名（IMG.PNG 实测形态）不误判为文本
        assert_eq!(mime_from_ext("PNG"), "image/png");
    }

    // ==== path_within_semantics（Windows 语义注入，darwin 全分支可测）====

    #[test]
    fn path_within_unix_semantics_with_boundary() {
        assert!(path_within_semantics("/a/b/c", "/a/b", false));
        assert!(
            path_within_semantics("/a/b", "/a/b", false),
            "自身包含自身（目录拒绝由 is_file 兜）"
        );
        assert!(
            path_within_semantics("/a/x", "/", false),
            "根包含一切绝对路径"
        );
        // 分隔符边界：/ab 不在 /a 之下
        assert!(!path_within_semantics("/ab", "/a", false));
        assert!(!path_within_semantics("/a/b", "/a/c", false));
        assert!(
            !path_within_semantics("/a/b", "", false),
            "空祖先不含任何路径"
        );
        // 相对路径形态不误判（canonicalize 后不会出现，防御性只判字面）
        assert!(!path_within_semantics("a/b", "/a", false));
    }

    #[test]
    fn path_within_windows_semantics_strips_verbatim_and_case_insensitive() {
        // verbatim 前缀剥离 + 大小写不敏感（canonicalize 恒返 \\?\ 形态 vs 字面 cwd）
        assert!(path_within_semantics(
            r"\\?\C:\proj\src\m.rs",
            r"C:\proj",
            true
        ));
        assert!(path_within_semantics(
            r"C:\proj\src\m.rs",
            r"\\?\C:\PROJ",
            true
        ));
        // UNC 形态还原
        assert!(path_within_semantics(
            r"\\?\UNC\server\share\x.txt",
            r"\\server\share",
            true
        ));
        // 边界：C:\proj2 不在 C:\proj 之下
        assert!(!path_within_semantics(r"\\?\C:\proj2\a", r"C:\proj", true));
        assert!(!path_within_semantics(r"\\?\D:\proj\a", r"C:\proj", true));
        // 尾部分隔符的祖先
        assert!(path_within_semantics(r"\\?\C:\proj\a", r"C:\proj\", true));
    }

    // ==== extract_paths_from_messages ====

    #[test]
    fn extract_collects_known_keys_recursively_and_dedups_in_order() {
        let msgs = vec![
            text_msg("user", "帮我改"),
            tool_call(r#"{"file_path":"/tmp/proj/src/a.rs","content":"x"}"#),
            tool_call(r#"{"command":"cat","path":"/tmp/proj/src/a.rs"}"#), // 同路径去重
            tool_call(
                r#"{"edits":[{"filename":"/tmp/proj/src/b.rs"},{"path":{"file_path":"/tmp/proj/lib/c.py"}}]}"#,
            ),
            tool_call(r#"{"abs_path":"/tmp/proj/README.md"}"#),
            text_msg("assistant", "done"), // 非 tool-call 忽略
        ];
        // 全部同 seq（tool_call 夹具默认 seq=1）→ 稳定排序下保持插入序
        assert_eq!(
            paths_of(&extract_paths_from_messages(&msgs)),
            vec![
                "/tmp/proj/src/a.rs".to_string(),
                "/tmp/proj/src/b.rs".to_string(),
                "/tmp/proj/lib/c.py".to_string(),
                "/tmp/proj/README.md".to_string(),
            ],
            "命中键递归收集 + 去重（按最后出现序稳定）"
        );
    }

    #[test]
    fn extract_skips_non_path_values() {
        let msgs = vec![
            // 命中键但值不是路径候选：URL / 纯词 / 换行串 / 空
            tool_call(r#"{"path":"https://example.com/x.rs"}"#),
            tool_call(r#"{"path":"main"}"#),
            tool_call(r#"{"path":"line1\nline2.rs"}"#),
            tool_call(r#"{"path":""}"#),
            tool_call(r#"{"path":42}"#),
            tool_call(r#"{"cmd":"ls -la /etc"}"#), // 非命中键不下手
            tool_call("not-json"),                 // 参数损坏
            tool_call(r#"{}"#),                    // 无参数对象
        ];
        // "main" 无分隔符无扩展名 → 排除；其余全部排除 → 空表
        assert!(extract_paths_from_messages(&msgs).is_empty());
    }

    /// T1 计划一等消息（kind="plan"）：落 `absorb_messages` 的 `_ => {}` 兜底分支——
    /// 不进文件面板、不 panic。计划 markdown 正文里即使出现路径样串（改计划常引用
    /// 待改文件）也不得误收——计划不是文件操作；同流 tool-call 照常提取（行为面无
    /// 回归的对照：旧 ExitPlanMode tool-call 的 plan 参数本就抽不出路径，见
    /// PATH_KEYS 无 plan 键）
    #[test]
    fn plan_messages_neither_panic_nor_produce_file_entries() {
        let plan = SessionMessage {
            seq: 2,
            role: "assistant".into(),
            kind: "plan".into(),
            content: "# 计划\n\n- 修改 /tmp/proj/src/plan-target.rs\n- 新建 /tmp/proj/new.rs"
                .into(),
            ts: Some(1),
            tool_name: Some("ExitPlanMode".into()),
            tool_args: None,
            collapsed: false,
        };
        // 纯 plan 流：无条目、不 panic
        assert!(extract_paths_from_messages(std::slice::from_ref(&plan)).is_empty());
        // 混合流：tool-call 照常提取，plan 不干扰（顺序、条数不受影响）
        let msgs = vec![tool_call(r#"{"file_path":"/tmp/proj/src/a.rs"}"#), plan];
        assert_eq!(
            paths_of(&extract_paths_from_messages(&msgs)),
            vec!["/tmp/proj/src/a.rs".to_string()]
        );
    }

    #[test]
    fn extract_accepts_relative_paths_and_bare_filenames_with_ext() {
        let msgs = vec![
            tool_call(r#"{"file_path":"src/lib/util.ts"}"#),
            tool_call(r#"{"path":"Cargo.toml"}"#),
        ];
        assert_eq!(
            paths_of(&extract_paths_from_messages(&msgs)),
            vec!["src/lib/util.ts".to_string(), "Cargo.toml".to_string()],
            "相对路径与裸扩展名文件名都是候选（端点侧按 cwd 解析相对路径）"
        );
    }

    /// FileEntry 列表 → 路径列表（既有用例断言适配助手）
    fn paths_of(entries: &[FileEntry]) -> Vec<String> {
        entries.iter().map(|e| e.path.clone()).collect()
    }

    // ==== extract_paths_from_messages（M3+ 文件面板：FileEntry 富化）====

    /// M3+ 文件面板：出现序/hits/最后原始形态
    /// - 同一文件 3 次（seq 2/7/9）→ hits=3、last_seq=9、path=最后一次的原始形态；
    /// - 多文件按 last_seq 降序（最近出现的文件在上）
    #[test]
    fn extract_entries_track_hits_last_seq_and_last_shape() {
        let mut msgs: Vec<SessionMessage> = Vec::new();
        let mut push = |seq: i64, args: &str| {
            let mut m = tool_call(args);
            m.seq = seq;
            m.ts = Some(seq * 100);
            msgs.push(m);
        };
        push(2, r#"{"path":"/p/a.rs"}"#);
        push(4, r#"{"path":"/p/b.md"}"#);
        push(7, r#"{"file_path":"/p/a.rs"}"#);
        push(9, r#"{"abs_path":"/p/a.rs","other":"x"}"#);

        let entries = extract_paths_from_messages(&msgs);
        assert_eq!(
            entries.iter().map(|e| e.path.as_str()).collect::<Vec<_>>(),
            vec!["/p/a.rs", "/p/b.md"],
            "按 last_seq 降序（a 最后出现于 9，b 于 4）"
        );
        let a = &entries[0];
        assert_eq!(a.hits, 3, "同一文件三次出现");
        assert_eq!(a.last_seq, 9, "取最后出现的 seq");
        assert_eq!(a.last_ts, Some(900), "取最后出现的 ts");
        let b = &entries[1];
        assert_eq!(b.hits, 1);
        assert_eq!(b.last_seq, 4);
        assert_eq!(b.last_ts, Some(400));
    }

    /// M3+ 文件面板（2026-09-16 用户裁决）：区分「已改写」与「仅读过」。
    /// 判定依据 = 触达该文件的工具名是否属于读类白名单；只要有一次
    /// 写类工具（Edit/Write/apply_patch…）触达即 modified=true。
    /// 前端据此把仅读过的文件名渲成常规色（仍是超链接）
    #[test]
    fn extract_entries_track_modified_vs_readonly() {
        // (1) 仅 Read → modified=false（tool_call 夹具默认名是 Write，须显式改）
        let mut read_msg = tool_call(r#"{"file_path":"/p/read.rs"}"#);
        read_msg.tool_name = Some("Read".into());
        let read_only = vec![read_msg];
        let entries = extract_paths_from_messages(&read_only);
        assert_eq!(entries.len(), 1);
        assert!(!entries[0].modified, "只有 Read 触达的文件应标为未改写");

        // (2) Edit → modified=true
        let mut edit = tool_call(r#"{"file_path":"/p/edited.rs"}"#);
        edit.tool_name = Some("Edit".into());
        let entries = extract_paths_from_messages(&[edit]);
        assert!(entries[0].modified, "Edit 触达的文件应标为已改写");

        // (3) 先读后写 → modified=true（写优先，只要写过就算改写）
        let mut r = tool_call(r#"{"file_path":"/p/both.rs"}"#);
        r.seq = 1;
        r.tool_name = Some("Read".into());
        let mut w = tool_call(r#"{"file_path":"/p/both.rs"}"#);
        w.seq = 2;
        w.tool_name = Some("Write".into());
        let entries = extract_paths_from_messages(&[r, w]);
        assert_eq!(entries.len(), 1);
        assert!(entries[0].modified, "先读后写应判为已改写");
        // 反过来（先写后读）仍是已改写——读过不撤销写过的结论
        let mut r2 = tool_call(r#"{"file_path":"/p/both2.rs"}"#);
        r2.seq = 1;
        r2.tool_name = Some("Read".into());
        let mut w2 = tool_call(r#"{"file_path":"/p/both2.rs"}"#);
        w2.seq = 2;
        w2.tool_name = Some("Write".into());
        let mut r3 = tool_call(r#"{"file_path":"/p/both2.rs"}"#);
        r3.seq = 3;
        r3.tool_name = Some("Read".into());
        assert!(
            extract_paths_from_messages(&[r2, w2, r3])[0].modified,
            "写过之后的读不撤销已改写标记"
        );

        // (4) 工具名缺失（未知形态）→ 保守判为已改写？不——按读类白名单判定，
        //     不在白名单内即视为可能改写（fail-safe：宁可标成改写也不误导用户
        //     以为「只是读过」的安全文件其实是改过的）
        let mut unknown = tool_call(r#"{"file_path":"/p/unknown.rs"}"#);
        unknown.tool_name = None;
        assert!(
            extract_paths_from_messages(&[unknown])[0].modified,
            "工具名缺失时按已改写处理（保守）"
        );

        // (5) 读类白名单：Read / Grep / Glob / view_image / ReadMediaFile 等
        for read_tool in ["Read", "Grep", "Glob", "view_image", "ReadMediaFile"] {
            let mut m = tool_call(r#"{"file_path":"/p/x.rs"}"#);
            m.tool_name = Some(read_tool.to_string());
            assert!(
                !extract_paths_from_messages(&[m])[0].modified,
                "{read_tool} 属读类工具，不得标为已改写"
            );
        }
    }

    /// 评审 I1 回归锁（M5 P2-a 全盘语义下语义不变）：会话 cwd == 用户主目录时，
    /// ~/.ssh/id_rsa 必须仍被拒绝（黑名单全路径一致生效，无 cwd 豁免），而
    /// ~/Downloads 下的普通文件仍可读
    #[test]
    fn read_file_safe_cwd_equals_home_still_enforces_sensitive_list() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        std::fs::create_dir_all(home.join("Downloads")).unwrap();
        std::fs::write(home.join(".ssh").join("id_rsa"), "PRIVATE").unwrap();
        std::fs::write(home.join("Downloads").join("trip.png"), b"\x89PNG").unwrap();
        let home_s = home.to_str().unwrap();
        // 会话 cwd = 主目录本身（在 ~ 下启动 agent 的常见场景）
        assert_eq!(
            read_file_safe(
                home_s,
                home.join(".ssh").join("id_rsa").to_str().unwrap(),
                Some(home_s)
            )
            .unwrap_err(),
            FileRejectReason::Sensitive,
            "cwd==home 时 ~/.ssh 必须仍被敏感清单拒绝"
        );
        assert!(
            read_file_safe(
                home_s,
                home.join("Downloads").join("trip.png").to_str().unwrap(),
                Some(home_s)
            )
            .is_ok(),
            "cwd==home 时 Downloads 普通文件仍可读"
        );
    }

    /// FileEntry 新增 modified 的 camelCase 序列化契约 + M5 B2 origin snake_case
    #[test]
    fn file_entry_serializes_modified_flag() {
        let e = FileEntry {
            path: "/p/a.rs".into(),
            last_seq: 1,
            last_ts: None,
            hits: 1,
            modified: true,
            origin: FileOrigin::ToolWrite,
        };
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v.get("modified").and_then(|b| b.as_bool()), Some(true));
        assert_eq!(
            v.get("origin").and_then(|s| s.as_str()),
            Some("tool_write"),
            "origin 序列化为 snake_case（移动端来源筛选值域）"
        );
    }

    /// M3+ 归一化比较键去重：`\`→`/`；Windows 语义（参数注入）再小写。
    /// 展示保留**最后一次出现**的原始形态（用户裁决 7）
    #[test]
    fn extract_dedups_via_normalized_key_and_keeps_last_shape() {
        let mut m1 = tool_call(r#"{"path":"C:\\A\\b.md"}"#);
        m1.seq = 1;
        let mut m2 = tool_call(r#"{"path":"C:/a/b.md"}"#);
        m2.seq = 2;
        let msgs = vec![m1, m2];

        // windows=true：反斜杠/大小写归一 → 一行，hits=2，形态取最后一次
        let win = extract_paths_from_messages_with(&msgs, true);
        assert_eq!(win.len(), 1, "Windows 语义下两种形态归一为一个文件");
        assert_eq!(win[0].hits, 2);
        assert_eq!(win[0].path, "C:/a/b.md", "保留最后一次出现的原始形态");

        // windows=false（macOS/Linux）：大小写敏感 → 两行
        let unix = extract_paths_from_messages_with(&msgs, false);
        assert_eq!(unix.len(), 2, "大小写敏感平台不得归一小写差异");
    }

    /// M3+ 归一化键纯函数：分隔符统一 + 平台大小写语义
    #[test]
    fn normalize_key_unifies_separators_and_platform_case() {
        assert_eq!(normalize_key(r"C:\A\b.md", true), "c:/a/b.md");
        assert_eq!(normalize_key("C:/a/b.md", true), "c:/a/b.md");
        assert_eq!(
            normalize_key(r"C:\A\b.md", true),
            normalize_key("C:/a/b.md", true),
            "同一文件两种形态必须同键"
        );
        assert_eq!(normalize_key(r"C:\A\b.md", false), "C:/A/b.md");
        assert_ne!(
            normalize_key("C:/a/b.md", false),
            normalize_key("C:/A/b.md", false),
            "非 Windows 语义大小写敏感"
        );
        // 尾部分隔符与空串（防御）
        assert_eq!(normalize_key("", true), "");
    }

    /// M3+ lastTs 可为 None（原生存储无时间戳条目）——不得 panic
    #[test]
    fn extract_entries_tolerate_missing_timestamp() {
        let mut m = tool_call(r#"{"path":"/p/n.ts"}"#);
        m.ts = None;
        let entries = extract_paths_from_messages(&[m]);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].last_ts, None);
    }

    /// camelCase 序列化契约（移动端字段名，勿漂移）：lastSeq/lastTs + origin
    #[test]
    fn file_entry_serializes_camel_case() {
        let e = FileEntry {
            path: "/p/a.rs".into(),
            last_seq: 7,
            last_ts: Some(700),
            hits: 2,
            modified: false,
            origin: FileOrigin::ToolRead,
        };
        let v = serde_json::to_value(&e).unwrap();
        assert!(v.get("path").is_some());
        assert!(v.get("lastSeq").is_some(), "lastSeq 字段（camelCase）");
        assert!(v.get("lastTs").is_some(), "lastTs 字段（camelCase）");
        assert!(v.get("hits").is_some());
        assert_eq!(v.get("origin").and_then(|s| s.as_str()), Some("tool_read"));
    }

    // ==== M5 B2：来源三池（user / tool_read / tool_write）====

    /// 工具读写 → origin 推导：写类工具 ToolWrite+modified、读类工具 ToolRead
    #[test]
    fn tool_call_origin_derivation_read_vs_write() {
        let mut write = tool_call(r#"{"path":"/p/w.txt"}"#);
        write.tool_name = Some("Write".into());
        let mut read = tool_call(r#"{"path":"/p/r.txt"}"#);
        read.tool_name = Some("Read".into());
        let entries = extract_paths_from_messages(&[write, read]);
        let w = entries.iter().find(|e| e.path == "/p/w.txt").unwrap();
        let r = entries.iter().find(|e| e.path == "/p/r.txt").unwrap();
        assert_eq!(w.origin, FileOrigin::ToolWrite);
        assert!(w.modified);
        assert_eq!(r.origin, FileOrigin::ToolRead);
        assert!(!r.modified);
    }

    /// kimi 内联标记（B1 黄金夹具形态）→ user 来源；blobref 不入池
    #[test]
    fn user_inline_markup_extracts_user_origin() {
        let m = text_msg(
            "user",
            "先看 <image path=\"C:/fixt/demo/brand-a.png\"> 与 <file path=\"C:/fixt/demo/report-draft.md\">",
        );
        let entries = extract_paths_from_messages(&[m]);
        assert_eq!(entries.len(), 2);
        assert!(entries.iter().all(|e| e.origin == FileOrigin::User));
        assert!(entries.iter().any(|e| e.path == "C:/fixt/demo/brand-a.png"));
        // blobref / 无路径标记不出条目
        let blob = text_msg("user", "<image src=\"blobref:image/png;abc\"></image>");
        assert!(
            extract_paths_from_messages(&[blob]).is_empty(),
            "blobref 内容寻址不可预览，不入池"
        );
    }

    /// 同一文件多来源：上传（user）后被工具改写（tool_write）→ 取信息量最大者
    #[test]
    fn merged_origin_takes_highest_priority() {
        let user = text_msg("user", "<image path=\"/p/both.png\"></image>");
        let mut write = tool_call(r#"{"path":"/p/both.png"}"#);
        write.seq = 3;
        // 反序喂入（先工具后用户）也一样：user 不得降级 tool_write
        let entries = extract_paths_from_messages(&[user, write]);
        assert_eq!(entries.len(), 1, "归一键去重 → 单条目");
        assert_eq!(entries[0].origin, FileOrigin::ToolWrite);
        assert!(entries[0].modified);
        assert_eq!(entries[0].hits, 2);
    }

    /// kimi 黄金夹具全链：wire 行 → map_kimi_lines → 抽取 → user 来源条目
    #[test]
    fn kimi_golden_fixture_full_chain_user_attachments() {
        let lines = crate::remote::attachment_fixtures::kimi_fixture_lines();
        let msgs = super::super::content::map_kimi_lines(&lines);
        let entries = extract_paths_from_messages(&msgs);
        let paths: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
        assert!(
            paths.contains(&"C:/fixt/demo/brand-a.png"),
            "用户消息内联 <image path> 必须出条目：{paths:?}"
        );
        assert!(
            paths.contains(&"C:/fixt/demo/report-draft.md"),
            "用户输入内联 <file path> 必须出条目"
        );
        assert!(
            entries.iter().all(|e| e.origin == FileOrigin::User),
            "内联标记全部来自用户消息 → user 来源"
        );
        assert!(
            !paths
                .iter()
                .any(|p| p.contains("blobref") || p.contains("chart-b.png")),
            "工具读取（tool.result blobref）不属用户附件来源"
        );
    }

    /// zcode 黄金夹具全链：tmp sqlite 种入 → 注入核抽取 → source.path 与
    /// artifact 磁盘实体双条目，origin=user
    #[test]
    fn zcode_golden_fixture_full_chain_user_attachments() {
        let tmp = tempfile::tempdir().unwrap();
        let (source_path, artifact) =
            crate::remote::attachment_fixtures::seed_zcode_attachment_db(tmp.path()).unwrap();
        let (entries, truncated) = extract_file_paths_with(
            tmp.path(),
            "zcode",
            crate::remote::attachment_fixtures::ZCODE_SESSION_ID,
            200,
        );
        assert!(!truncated);
        let by_path = |p: &str| entries.iter().find(|e| e.path == p);
        let src = by_path(&source_path).expect("source.path 型附件必须入池");
        assert_eq!(src.origin, FileOrigin::User);
        let art =
            by_path(&artifact.to_string_lossy()).expect("artifact URI 必须解析到磁盘实体并入池");
        assert_eq!(art.origin, FileOrigin::User);
        assert_eq!(entries.len(), 2, "恰两附件：本地路径型 + artifact 型");
    }

    /// artifact 实体缺失（artifacts 目录被清理）→ 降级跳过，不出死链接条目
    #[test]
    fn zcode_artifact_missing_entity_degrades_to_skip() {
        let tmp = tempfile::tempdir().unwrap();
        let (source_path, artifact) =
            crate::remote::attachment_fixtures::seed_zcode_attachment_db(tmp.path()).unwrap();
        std::fs::remove_file(&artifact).unwrap();
        let (entries, _) = extract_file_paths_with(
            tmp.path(),
            "zcode",
            crate::remote::attachment_fixtures::ZCODE_SESSION_ID,
            200,
        );
        assert_eq!(entries.len(), 1, "仅剩 source.path 型附件");
        assert_eq!(entries[0].path, source_path);
    }

    /// artifact URI 解析纯函数：尾段匹配 + 多命中取字典序最小 + 越界会话空尾段拒绝
    #[test]
    fn resolve_zcode_artifact_matches_tail_and_prefers_deterministic() {
        let tmp = tempfile::tempdir().unwrap();
        // 真实目录布局：<home>/.zcode/cli/artifacts/<session>/…
        let sess = tmp.path().join(".zcode/cli/artifacts/sess_x");
        std::fs::create_dir_all(&sess).unwrap();
        std::fs::write(sess.join("prompt-a-tool-result-u1.png"), b"x").unwrap();
        std::fs::write(sess.join("prompt-b-tool-result-u1.png"), b"x").unwrap();
        let hit = resolve_zcode_artifact(tmp.path(), "sess_x/tool-result-u1").unwrap();
        assert!(
            hit.ends_with("prompt-a-tool-result-u1.png"),
            "字典序最小：{hit}"
        );
        assert!(resolve_zcode_artifact(tmp.path(), "sess_x/tool-result-missing").is_none());
        assert!(
            resolve_zcode_artifact(tmp.path(), "sess_x").is_none(),
            "无尾段拒绝"
        );
    }

    /// limit 透传：更大 limit 取到更早的工具调用（窗口机制零新增，透传既有管线）
    #[test]
    fn extract_with_env_passes_limit_through() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".claude/projects/-tmp-proj");
        std::fs::create_dir_all(&dir).unwrap();
        // 早期行（首条）+ 大量后续行：小 limit 取不到首条，大 limit 取得到
        let mut lines = vec![
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"t0","name":"Write","input":{"file_path":"/tmp/proj/early.rs"}}]}}"#.to_string(),
        ];
        for i in 0..60 {
            lines.push(format!(
                r#"{{"type":"assistant","message":{{"role":"assistant","content":[{{"type":"tool_use","id":"t{i}","name":"Write","input":{{"file_path":"/tmp/proj/late{i}.rs"}}}}]}}}}"#
            ));
        }
        std::fs::write(
            dir.join("1f2e3d4c-5b6a-4948-8276-9a0b8c7d6e5f.jsonl"),
            format!("{}\n", lines.join("\n")),
        )
        .unwrap();
        let sid = "1f2e3d4c-5b6a-4948-8276-9a0b8c7d6e5f";

        // limit=1：窗口只覆盖尾条消息（其工具调用出条目，早期文件不在窗内）
        let (small, _) = extract_file_paths_with_env(tmp.path(), None, None, "claude", sid, 1);
        assert!(
            small.iter().all(|e| !e.path.ends_with("early.rs")),
            "limit=1 只覆盖尾条（早期工具调用不在窗内），实得 {small:?}"
        );
        assert!(
            small.iter().any(|e| e.path.ends_with("late59.rs")),
            "尾条消息的文件必须在窗内，实得 {small:?}"
        );
        // limit=200：窗口覆盖全量 → 首条也可见
        let (big, _) = extract_file_paths_with_env(tmp.path(), None, None, "claude", sid, 200);
        assert!(
            big.iter().any(|e| e.path.ends_with("early.rs")),
            "放大 limit 后早期文件必须可见（窗口透传生效）"
        );
    }

    // ==== Task 9：七工具提取链路 fixture（真实 toolArgs 形态口径，逐工具探测证据
    // 见 task-9-report.md）。全链路 = tempdir home → content 层统一出口 → 泛化提取。
    // claude 已有 extract_file_paths_with_reads_through_content_layer（Task 8 实机验证）。====

    const T9_UUID: &str = "1f2e3d4c-5b6a-4948-8276-9a0b8c7d6e5f";

    /// ZCode tmp sqlite（message + part 表，content.rs 测试同款 schema）
    fn t9_zcode_db(home: &Path) -> rusqlite::Connection {
        let db = home.join(".zcode/cli/db/db.sqlite");
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE message (
                id TEXT PRIMARY KEY, session_id TEXT, sequence INTEGER,
                time_created INTEGER, data TEXT
             );
             CREATE TABLE part (
                id TEXT PRIMARY KEY, message_id TEXT, sequence INTEGER, data TEXT
             );",
        )
        .unwrap();
        conn
    }

    /// 实测形态（本机 ~/.zcode 真实库，4646+758+3370 命中）：Edit/Write/Read 的
    /// state.input 用 file_path 键；Bash 的 command 键不是路径参数——不得误收
    #[test]
    fn extract_chain_zcode_tool_part_input_file_path() {
        let tmp = tempfile::tempdir().unwrap();
        let conn = t9_zcode_db(tmp.path());
        conn.execute(
            "INSERT INTO message VALUES ('m1', 'sess-t9', 1, 100, '{\"role\":\"assistant\",\"semantics\":{\"kind\":\"assistant\"}}')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO part VALUES ('p1', 'm1', 1, '{\"type\":\"tool\",\"tool\":\"Edit\",\"state\":{\"input\":{\"file_path\":\"/tmp/proj/src/main.rs\",\"old_string\":\"a\",\"new_string\":\"b\"}}}')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO part VALUES ('p2', 'm1', 2, '{\"type\":\"tool\",\"tool\":\"Bash\",\"state\":{\"input\":{\"command\":\"cat /etc/hostname\"}}}')",
            [],
        )
        .unwrap();
        assert_eq!(
            paths_of(&extract_file_paths_with(tmp.path(), "zcode", "sess-t9", 200).0),
            vec!["/tmp/proj/src/main.rs".to_string()],
            "zcode state.input.file_path 收集；bash command 不收"
        );
    }

    /// 实测形态（本机 ~/.codex 真实 rollout 抽样 40 份）：view_image 的 arguments.path
    /// 是唯一成规模的文件路径键；apply_patch 的 command 是补丁全文（路径只是嵌在
    /// 补丁字符串里，不是路径参数键）——嵌在串内的路径不得被误收
    #[test]
    fn extract_chain_codex_rollout_function_call_arguments() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".codex/sessions/2026/09/15");
        std::fs::create_dir_all(&dir).unwrap();
        let rollout = dir.join(format!("rollout-2026-09-15T00-00-00-{T9_UUID}.jsonl"));
        let meta = format!(
            r#"{{"timestamp":"2026-09-15T00:00:00.000Z","type":"session_meta","payload":{{"id":"{T9_UUID}","cwd":"/tmp/proj"}}}}"#
        );
        let view_image = r#"{"timestamp":"2026-09-15T00:00:10.000Z","type":"response_item","payload":{"type":"function_call","name":"view_image","arguments":"{\"path\":\"/tmp/shots/page1.png\"}"}}"#;
        let apply_patch = r#"{"timestamp":"2026-09-15T00:00:20.000Z","type":"response_item","payload":{"type":"function_call","name":"apply_patch","arguments":"{\"command\":\"*** Add File: /tmp/proj/embedded.rs\"}"}}"#;
        std::fs::write(&rollout, format!("{meta}\n{view_image}\n{apply_patch}\n")).unwrap();
        assert_eq!(
            paths_of(&extract_file_paths_with(tmp.path(), "codex", T9_UUID, 200).0),
            vec!["/tmp/shots/page1.png".to_string()],
            "codex arguments.path 收集；补丁串内嵌路径不收"
        );
    }

    /// dsh tmp home zstd 代际（content.rs 测试同款 header/events 编码）。
    /// 实测形态（本机 ~/.dsh 21 份代际日志）：read/write/edit/read_image 的
    /// arguments.file_path；present 的 files[].path（数组嵌套递归）；bash 的
    /// command 是命令串——不得误收
    #[test]
    fn extract_chain_dsh_tool_call_arguments() {
        let tmp = tempfile::tempdir().unwrap();
        let header = r#"{"type":"session","version":3,"id":"session-t9","cwd":"/tmp/proj","createdAt":1000,"isSeeded":false}"#;
        let events = concat!(
            r#"{"type":"tool/call","seq":10,"time":1112,"data":{"name":"edit","callId":"c1","arguments":"{\"file_path\":\"/tmp/proj/src/a.rs\",\"old_string\":\"x\",\"new_string\":\"y\"}"}}"#,
            "\n",
            r#"{"type":"tool/call","seq":11,"time":1113,"data":{"name":"bash","callId":"c2","arguments":"{\"command\":\"ls /etc\"}"}}"#,
            "\n",
            r#"{"type":"tool/call","seq":12,"time":1114,"data":{"name":"present","callId":"c3","arguments":"{\"files\":[{\"path\":\"/tmp/proj/docs/report.md\",\"description\":\"d\"}]}"}}"#,
            "\n",
        );
        let frame = zstd::stream::encode_all(format!("{header}\n{events}").as_bytes(), 3).unwrap();
        let sess = tmp.path().join(".dsh/sessions/--proj--/escaped~dir");
        std::fs::create_dir_all(&sess).unwrap();
        std::fs::write(sess.join("session.v3.jsonl.zstd"), &frame).unwrap();
        assert_eq!(
            paths_of(&extract_file_paths_with(tmp.path(), "dsh", "session-t9", 200).0),
            vec![
                // lastSeq 降序：report.md 出现于 seq 12、a.rs 于 seq 10（用户裁决 1）
                "/tmp/proj/docs/report.md".to_string(),
                "/tmp/proj/src/a.rs".to_string(),
            ],
            "dsh arguments.file_path 收集 + files[].path 嵌套收集；bash command 不收"
        );
    }

    /// Kimi tmp home（session_index 定位 + wire.jsonl）。实测形态（本机
    /// ~/.kimi-code 35 份 wire）：Read/Edit/Write/Grep 的 args.path；另有旧版
    /// Read 用 args.file（3 实测绝对路径样本）——file 键须在收集名单内
    #[test]
    fn extract_chain_kimi_wire_tool_call_args() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join(".kimi-code");
        let session_dir = home
            .join("sessions/wd_t9")
            .join(format!("session_{T9_UUID}"));
        std::fs::create_dir_all(session_dir.join("agents/main")).unwrap();
        let lines = concat!(
            r#"{"type":"context.append_loop_event","event":{"type":"tool.call","toolCallId":"c1","name":"Edit","args":{"path":"/tmp/proj/src/m.rs","old_string":"a","new_string":"b"}},"time":100}"#,
            "\n",
            r#"{"type":"context.append_loop_event","event":{"type":"tool.call","toolCallId":"c2","name":"Read","args":{"file":"/tmp/proj/README.md","offset":10,"limit":80}},"time":200}"#,
            "\n",
            r#"{"type":"context.append_loop_event","event":{"type":"tool.call","toolCallId":"c3","name":"Bash","args":{"command":"ls /tmp"}},"time":300}"#,
            "\n",
        );
        std::fs::write(session_dir.join("agents/main/wire.jsonl"), lines).unwrap();
        let index = serde_json::json!({
            "sessionId": T9_UUID,
            "sessionDir": session_dir.to_string_lossy(),
            "workDir": "/tmp/proj",
        })
        .to_string();
        std::fs::write(home.join("session_index.jsonl"), format!("{index}\n")).unwrap();
        assert_eq!(
            paths_of(&extract_file_paths_with(tmp.path(), "kimi", T9_UUID, 200).0),
            vec![
                // lastSeq 降序：README.md 行后于 m.rs 行（用户裁决 1）
                "/tmp/proj/README.md".to_string(),
                "/tmp/proj/src/m.rs".to_string(),
            ],
            "kimi args.path 与旧版 args.file 均收集；bash command 不收"
        );
    }

    /// WorkBuddy tmp home（projects 扫描定位）。实测形态（本机 ~/.workbuddy）：
    /// OpenAI 风格 function_call 的 arguments.file_path（Read/Write/Edit）
    #[test]
    fn extract_chain_workbuddy_function_call_arguments() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".workbuddy/projects/-tmp-proj");
        std::fs::create_dir_all(&dir).unwrap();
        let lines = concat!(
            r#"{"type":"message","role":"user","content":[{"type":"text","text":"改一下"}]}"#,
            "\n",
            r#"{"type":"function_call","name":"Edit","arguments":"{\"file_path\":\"/tmp/proj/src/wb.rs\",\"old_string\":\"a\",\"new_string\":\"b\"}"}"#,
            "\n",
            r#"{"type":"function_call","name":"Bash","arguments":"{\"command\":\"cat /etc/hosts\"}"}"#,
            "\n",
        );
        std::fs::write(dir.join(format!("{T9_UUID}.jsonl")), lines).unwrap();
        assert_eq!(
            paths_of(&extract_file_paths_with(tmp.path(), "workbuddy", T9_UUID, 200).0),
            vec!["/tmp/proj/src/wb.rs".to_string()],
            "workbuddy arguments.file_path 收集；bash command 不收"
        );
    }

    /// OpenCode tmp sqlite。本机库无 tool part 样本（实测只有 text/patch）——
    /// 形态按官方文档合成：edit/write/read 的 state.input 用 filePath 驼峰键
    /// （opencode.ai/docs/tools 与 issue #729 交叉确认），须在收集名单内
    #[test]
    fn extract_chain_opencode_tool_part_state_input() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join(".local/share/opencode/opencode.db");
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, data TEXT);
             CREATE TABLE part (message_id TEXT, data TEXT, time_created INTEGER);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message VALUES ('m1', ?1, 100, '{\"role\":\"assistant\"}')",
            ["sess-t9-oc"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO part VALUES ('m1', '{\"type\":\"tool\",\"tool\":\"edit\",\"state\":{\"input\":{\"filePath\":\"/tmp/proj/src/oc.ts\",\"oldString\":\"a\",\"newString\":\"b\"}}}', 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO part VALUES ('m1', '{\"type\":\"tool\",\"tool\":\"bash\",\"state\":{\"input\":{\"command\":\"ls\"}}}', 2)",
            [],
        )
        .unwrap();
        assert_eq!(
            paths_of(&extract_file_paths_with(tmp.path(), "opencode", "sess-t9-oc", 200).0),
            vec!["/tmp/proj/src/oc.ts".to_string()],
            "opencode state.input.filePath（驼峰）收集；bash command 不收"
        );
    }

    /// OpenClaw tmp sqlite（acp_replay_events）。协议形态（sessionUpdate/rawInput）
    /// 为 2026-09-15 实机探测确认，但本机库无 tool_call 事件样本、rawInput 内的
    /// 路径键名无实测依据——合成用 file_path 锁「replay → toolArgs → 提取」链路，
    /// 真实键名若不同，泛化名单（path/file/filePath 等）大概率兜住（低置信度备案）
    #[test]
    fn extract_chain_openclaw_acp_tool_call_raw_input() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join(".openclaw/state/openclaw.sqlite");
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE acp_replay_events (
                session_id TEXT NOT NULL, seq INTEGER NOT NULL, at INTEGER NOT NULL,
                session_key TEXT NOT NULL, run_id TEXT, update_json TEXT NOT NULL,
                PRIMARY KEY (session_id, seq)
             );",
        )
        .unwrap();
        let ins = |seq: i64, update: &str| {
            conn.execute(
                "INSERT INTO acp_replay_events (session_id, seq, at, session_key, update_json) VALUES ('s-t9', ?1, ?2, 'k', ?3)",
                rusqlite::params![seq, seq * 100, update],
            )
            .unwrap();
        };
        ins(
            1,
            r#"{"sessionUpdate":"tool_call","toolCallId":"c1","title":"write","rawInput":{"file_path":"/tmp/proj/src/oclaw.rs","content":"x"}}"#,
        );
        ins(
            2,
            r#"{"sessionUpdate":"tool_call","toolCallId":"c2","title":"bash","rawInput":{"command":"ls /tmp"}}"#,
        );
        assert_eq!(
            paths_of(&extract_file_paths_with(tmp.path(), "openclaw", "s-t9", 200).0),
            vec!["/tmp/proj/src/oclaw.rs".to_string()],
            "openclaw rawInput 经 ACP 映射后收集；bash command 不收"
        );
    }

    /// 计划测试名保留：注入核全链路（tempdir home 的 claude fixture → 统一出口 → 提取）。
    /// Task 9 已按同款为其余七工具补 fixture（上方 extract_chain_* 系列）。
    #[test]
    fn extract_file_paths_with_reads_through_content_layer() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp
            .path()
            .join(".claude")
            .join("projects")
            .join("-tmp-proj");
        std::fs::create_dir_all(&proj).unwrap();
        let sid = "0f1e2d3c-4b5a-4948-8276-9a0b8c7d6e5f";
        let line = r#"{"type":"assistant","timestamp":"2026-09-15T00:00:00Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Write","input":{"file_path":"/tmp/proj/src/main.rs","content":"fn main(){}"}}]}}"#;
        std::fs::write(proj.join(format!("{sid}.jsonl")), format!("{line}\n")).unwrap();
        assert_eq!(
            paths_of(&extract_file_paths_with(tmp.path(), "claude", sid, 200).0),
            vec!["/tmp/proj/src/main.rs".to_string()],
            "全链路：content 层读取 → tool-call 参数提取"
        );
        // 会话不存在 → 空表（不 Err，文件面板是增强能力）
        assert!(
            extract_file_paths_with(tmp.path(), "claude", "missing", 200)
                .0
                .is_empty()
        );
    }

    /// 终审 Important 1：/session-files 与 /session-messages 数据同源——DSH_HOME /
    /// KIMI_CODE_HOME 数据源重定向必须同样作用到文件面板提取。env 值以**参数**注入
    /// （dsh_env_home_redirects_data_root 同款模式，测试零真实 env 接触）。旧实现恒走
    /// None env（read_session_messages_with）——设了 env 的机器上详情页消息正常、
    /// 文件面板静默空表，本测试在旧实现下必红
    #[test]
    fn extract_file_paths_env_redirect_reaches_file_panel() {
        // ---- dsh：会话数据只在 DSH_HOME 根下（home 下无任何 dsh 数据）----
        let tmp = tempfile::tempdir().unwrap();
        let dsh_env_dir = tempfile::tempdir().unwrap();
        let header = r#"{"type":"session","version":3,"id":"session-env","cwd":"/tmp/proj","createdAt":1000,"isSeeded":false}"#;
        let events = r#"{"type":"tool/call","seq":10,"time":1112,"data":{"name":"edit","callId":"c1","arguments":"{\"file_path\":\"/tmp/proj/src/env.rs\",\"old_string\":\"x\",\"new_string\":\"y\"}"}}"#;
        let frame =
            zstd::stream::encode_all(format!("{header}\n{events}\n").as_bytes(), 3).unwrap();
        let sess = dsh_env_dir.path().join("sessions/--proj--/escaped~dir");
        std::fs::create_dir_all(&sess).unwrap();
        std::fs::write(sess.join("session.v3.jsonl.zstd"), &frame).unwrap();

        let dsh_env = dsh_env_dir.path().to_str().unwrap().to_string();
        assert_eq!(
            paths_of(
                &extract_file_paths_with_env(
                    tmp.path(),
                    Some(dsh_env.as_str()),
                    None,
                    "dsh",
                    "session-env",
                    200,
                )
                .0
            ),
            vec!["/tmp/proj/src/env.rs".to_string()],
            "DSH_HOME 重定向必须作用到文件面板提取（与详情页消息同源）"
        );
        // env 未设形态（None）→ 回落 home/.dsh → 无数据 → 空表
        assert!(
            extract_file_paths_with_env(tmp.path(), None, None, "dsh", "session-env", 200)
                .0
                .is_empty()
        );

        // ---- kimi：会话数据只在 KIMI_CODE_HOME 根下 ----
        let kimi_env_dir = tempfile::tempdir().unwrap();
        let session_dir = kimi_env_dir
            .path()
            .join("sessions/wd_env")
            .join(format!("session_{T9_UUID}"));
        std::fs::create_dir_all(session_dir.join("agents/main")).unwrap();
        let wire = concat!(
            r#"{"type":"context.append_loop_event","event":{"type":"tool.call","toolCallId":"c1","name":"Edit","args":{"path":"/tmp/proj/src/kimi-env.rs","old_string":"a","new_string":"b"}},"time":100}"#,
            "\n",
        );
        std::fs::write(session_dir.join("agents/main/wire.jsonl"), wire).unwrap();
        let index = serde_json::json!({
            "sessionId": T9_UUID,
            "sessionDir": session_dir.to_string_lossy(),
            "workDir": "/tmp/proj",
        })
        .to_string();
        std::fs::write(
            kimi_env_dir.path().join("session_index.jsonl"),
            format!("{index}\n"),
        )
        .unwrap();

        let kimi_env = kimi_env_dir.path().to_str().unwrap().to_string();
        assert_eq!(
            paths_of(
                &extract_file_paths_with_env(
                    tmp.path(),
                    None,
                    Some(kimi_env.as_str()),
                    "kimi",
                    T9_UUID,
                    200
                )
                .0
            ),
            vec!["/tmp/proj/src/kimi-env.rs".to_string()],
            "KIMI_CODE_HOME 重定向必须作用到文件面板提取（与详情页消息同源）"
        );
        assert!(
            extract_file_paths_with_env(tmp.path(), None, None, "kimi", T9_UUID, 200)
                .0
                .is_empty(),
            "env 未设形态回落 ~/.kimi-code → 无数据 → 空表"
        );
    }
}
