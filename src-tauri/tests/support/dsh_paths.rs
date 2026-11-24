#![allow(dead_code)]

use std::path::PathBuf;

pub(super) fn lexical_parent(path: &str) -> Option<String> {
    let trimmed = path.trim_end_matches(['/', '\\']);
    let idx = trimmed.rfind(['/', '\\'])?;
    let parent = &trimmed[..idx];
    Some(if parent.is_empty() {
        "/".to_string()
    } else {
        parent.to_string()
    })
}

pub(super) fn join_lexical(base: &str, rel: &[&str]) -> PathBuf {
    let sep = if base.contains('\\') { "\\" } else { "/" };
    let trimmed = base.trim_end_matches(['/', '\\']);
    PathBuf::from(format!("{trimmed}{sep}{}", rel.join(sep)))
}
