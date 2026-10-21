//! 文件路径身份键。仅做词法处理，不查询文件系统；展示路径仍由调用方保留。

use std::path::{Component, Path, PathBuf};

/// 既有三池的比较键：统一分隔符，Windows 语义下折叠大小写。
pub(crate) fn normalize_key(path: &str, windows: bool) -> String {
    let unified = path.replace('\\', "/");
    if windows {
        unified.to_lowercase()
    } else {
        unified
    }
}

/// 文件身份：有 cwd 时把相对路径接到 cwd，丢弃 `.` 与重复分隔符，保留 `..`。
/// cwd 缺失或为空时不猜绝对基准，仍做词法归一；退化为空的路径保留原文键。
/// 绝对路径判定沿用宿主 `Path` 语义，与原会话产物去重规则一致。
pub(crate) fn file_identity_key(path: &str, cwd: Option<&str>, windows: bool) -> String {
    let p = Path::new(path.trim());
    let joined: PathBuf = match cwd {
        Some(c) if !c.trim().is_empty() && !p.is_absolute() => Path::new(c.trim()).join(p),
        _ => p.to_path_buf(),
    };
    let lexically_normal: PathBuf = joined
        .components()
        .filter(|c| !matches!(c, Component::CurDir))
        .collect();
    let normalized = lexically_normal.to_string_lossy();
    if normalized.is_empty() {
        return normalize_key(path.trim(), windows);
    }
    normalize_key(&normalized, windows)
}
