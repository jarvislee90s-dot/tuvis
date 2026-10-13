//! 项目归属（D21 四条规则；**取代**契约 §4 与旧版说明书 §5.1 规则②的
//! 「realpath + 大小写规范化项目键」——那条已作废，本模块不得再实现第二套命名）。
//!
//! 1. 分组键 = `projectName.toLowerCase()`；显示名 = `projectName` 原文
//!    （对齐首页会话卡片与 `pairing.ts` 的 pairingKey 小写约定；**禁止**用 projectPath 作键
//!    ——它在各源有三种形态：claude 小写正斜杠 / zcode 原生反斜杠 / workbuddy 空串）；
//! 2. 命名**必须复用**既有 `monitor::project::project_name_from_path`（basename，
//!    不做 realpath / 大小写规范化 / trim / 软链解析）；
//! 3. 一律取**记录内** cwd/目录字段，禁止用日志目录名反推；
//! 4. 一个会话文件可跨多个项目 → 归属按**记录级** cwd 判定，多值各自计入。
//!    **落地方式**：采集器把 `record_project(cwd).key` 填进 `DetailKey.project_key`（明细行键，
//!    进明细表主键），日聚合按同一个键折叠 → 小时档与日档口径一致；会话维度表的项目列只是
//!    **会话级近似**（会话首个带 cwd 的记录），不参与任何分组（R2/GC 18）。
//!    **禁止**在 `DeltaBuilder::detail()` 里回退查会话维度表取项目——那会让两档悄悄分叉。
//!
//! 另存备用：会话维度表同时保存 realpath 规范化路径（**不作 UI 用途**），
//! 以便日后若要修归类时无需重扫。
//!
//! **已知代价（如实接受，不掩盖）**：Windows 上 claude 的路径被 `normalize_cwd_for_match`
//! 转成小写，而 zcode / workbuddy 保留原生大小写 → 同一目录可能裂成两组；
//! 无 cwd 的会话全部落 `Unknown` 桶；basename 相同而实际不同的目录会合并成一组。
use crate::monitor::project::project_name_from_path;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecordProject {
    /// 分组键（D21 规则①）
    pub key: String,
    /// 显示名（D21 规则①）
    pub label: String,
    /// 记录内 cwd 原文（D21 规则③；各源形态原样保留）
    pub path_raw: String,
    /// realpath 规范化路径（仅留档，不作 UI 用途）
    pub realpath: String,
}

/// 项目名 = 既有 basename 函数的输出（缺 cwd → "Unknown"）
pub fn project_name_of(cwd: Option<&str>) -> String {
    match cwd.map(str::trim).filter(|s| !s.is_empty()) {
        Some(p) => project_name_from_path(p),
        None => project_name_from_path(""),
    }
}

/// 分组键 = 项目名小写（D21 规则①）
pub fn project_key_of(cwd: Option<&str>) -> String {
    project_name_of(cwd).to_lowercase()
}

/// realpath 规范化路径（**只写进 RecordProject.realpath**）：
/// 解析软链与相对段（macOS `/tmp` → `/private/tmp` 一类），失败则回退「统一分隔符的原文」。
/// 采集侧**不得**用它生成分组键或显示名。
pub fn project_realpath_of(cwd: &str) -> String {
    let unified = cwd.trim().replace('\\', "/");
    std::fs::canonicalize(std::path::Path::new(&unified))
        .ok()
        .and_then(|p| p.to_str().map(|s| s.to_string()))
        .map(|s| s.replace('\\', "/"))
        .unwrap_or(unified)
}

/// 采集器构造会话维度项目归属的唯一入口（记录级调用）
pub fn record_project(cwd: Option<&str>) -> RecordProject {
    let raw = cwd.map(str::trim).unwrap_or("");
    RecordProject {
        key: project_key_of(cwd),
        label: project_name_of(cwd),
        path_raw: raw.to_string(),
        realpath: if raw.is_empty() {
            String::new()
        } else {
            project_realpath_of(raw)
        },
    }
}

// ==== usage 自省锁：以下为排除区（测试代码），勿删勿复制 ====
#[cfg(test)]
mod tests {
    use super::*;

    /// D21 规则①：**分组键 = `projectName.toLowerCase()`，显示名 = `projectName` 原文**。
    /// 本机铁证：kimi 把 `codexplusplus` 与 `CodexPlusPlus`（同 inode 552403）按字面路径
    /// 拆成两个 workspace，13 个会话被算成两个项目——按小写 projectName 作键后归一个。
    #[test]
    fn project_key_is_lowercased_name_and_label_keeps_original() {
        assert_eq!(
            project_key_of(Some("/Users/jarvis/Documents/CodexPlusPlus")),
            "codexplusplus"
        );
        assert_eq!(
            project_key_of(Some("/Users/jarvis/Documents/codexplusplus")),
            "codexplusplus"
        );
        assert_eq!(
            project_name_of(Some("/Users/jarvis/Documents/CodexPlusPlus")),
            "CodexPlusPlus"
        );
        // 展示名保留原文（首页会话卡片直接渲染 projectName，看板必须同名）
        assert_eq!(
            record_project(Some("/Users/jarvis/Documents/CodexPlusPlus")).label,
            "CodexPlusPlus"
        );
    }

    /// D21 规则②：命名**必须复用** `monitor::project::project_name_from_path`（basename）。
    /// 本用例直接与该既有函数逐例对齐——若将来有人另造一套命名，这条会红。
    #[test]
    fn naming_function_is_the_existing_one() {
        for path in [
            "/Users/x/proj",
            "E:\\LLMproject\\0807",
            "C:\\Users\\bunny\\Desktop",
            "/",
            "",
        ] {
            assert_eq!(
                project_name_of(Some(path)),
                crate::monitor::project::project_name_from_path(path),
                "路径 {path} 的命名必须与既有函数一致"
            );
        }
    }

    /// D21 规则③ + 另存 realpath：cwd **原文原样保留**（各源形态不同，禁止在采集侧统一），
    /// realpath 只写进 `realpath` 字段备用（不作 UI 用途）。
    /// `E:\LLMproject\0807` 的原生反斜杠形态是 zcode 既有测试显式断言的约定，不得被归一化。
    #[test]
    fn raw_path_is_preserved_and_realpath_is_stored_separately() {
        let r = record_project(Some("E:\\LLMproject\\0807"));
        assert_eq!(r.path_raw, "E:\\LLMproject\\0807", "cwd 原文不得被归一化");
        assert_eq!(r.label, "0807");
        assert_eq!(r.key, "0807");
        // 另存 realpath 的**接线**也必须锁住：`record_project` 不填 realpath 既不报错、
        // 也不影响任何分组口径，只会让 D21「日后若要修归类时无需重扫」的承诺静默落空
        assert_eq!(
            r.realpath, "E:/LLMproject/0807",
            "realpath 字段必须由 project_realpath_of 填充（失败回退统一分隔符原文）"
        );
        // 软链：realpath 解析（macOS 上 /tmp → /private/tmp 的同一类问题），平台无关写法
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real-proj");
        std::fs::create_dir_all(&real).unwrap();
        let link = dir.path().join("link-proj");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&real, &link).unwrap();
            assert_eq!(
                project_realpath_of(&link.to_string_lossy()),
                project_realpath_of(&real.to_string_lossy())
            );
        }
        #[cfg(windows)]
        {
            // Windows symlink_dir 需要开发者模式 / SeCreateSymbolicLinkPrivilege（无特权
            // 机器上 os error 1314 必失败）；junction **不需要特权**且 realpath 解析同样
            // 生效（linker 既有先例 junction::create）——用 junction 让断言在全部
            // Windows 机器上真跑，而不是无开发者模式的机器上永久静默跳过
            junction::create(&real, &link).expect("创建 junction 失败");
            assert_eq!(
                project_realpath_of(&link.to_string_lossy()),
                project_realpath_of(&real.to_string_lossy())
            );
        }
        // 路径不存在 → 回退统一分隔符后的原文（不 panic、不返回空）
        assert_eq!(project_realpath_of("E:\\no\\such\\dir"), "E:/no/such/dir");
    }

    /// D21 规则④ + 已知代价（**如实钉住，不得掩盖**）：
    /// * 无 cwd / 根路径 → `Unknown` 桶（键 `unknown`）；
    /// * basename 口径的代价：不同目录同名会合并成一组。
    #[test]
    fn unknown_bucket_and_basename_collision_are_accepted_costs() {
        assert_eq!(project_name_of(None), "Unknown");
        assert_eq!(project_key_of(None), "unknown");
        assert_eq!(project_name_of(Some("/")), "Unknown");
        assert_eq!(project_key_of(Some("   ")), "unknown");
        // 已知代价：两个不同目录、同名 → 合并（basename 口径的必然结果）
        assert_eq!(
            project_key_of(Some("/a/Repo")),
            project_key_of(Some("/b/repo"))
        );
        // 不同名 → 不同键（记录级归属的前提）
        assert_ne!(project_key_of(Some("/x/A")), project_key_of(Some("/x/B")));
    }
}
// ==== usage 自省锁：排除区结束，勿删勿复制 ====
