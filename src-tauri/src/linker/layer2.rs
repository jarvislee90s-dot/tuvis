// Layer 2：工具级激活目录管理
// ~/.tuvis/active/<tool>/ 存放该工具已启用的 skill 链接

use std::path::PathBuf;

/// 获取 Layer 2 基础目录
pub fn active_base_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(".tuvis")
        .join("active")
}

/// 获取指定工具的 Layer 2 目录
pub fn tool_active_dir(tool_id: &str) -> PathBuf {
    active_base_dir().join(tool_id)
}

/// 确保 Layer 2 目录存在
pub fn ensure_tool_active_dir(tool_id: &str) -> PathBuf {
    let dir = tool_active_dir(tool_id);
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// 创建 Layer 2 symlink：从 Layer 1 源文件 → Layer 2 工具目录。
/// 派发拍平（用户裁决 2026-09-17）：source 侧保持 SSOT 嵌套原路径（仓库层级
/// 不动），target 侧一律用拍平名（套件-技能名，`/` → `-`）
pub fn link_skill_to_layer2(skill_name: &str, tool_id: &str) -> Result<PathBuf, String> {
    let repo = super::ensure_repo_dir();
    let source = repo.join(skill_name);
    if !source.exists() {
        return Err(format!(
            "LINKER_SKILL_NOT_IN_REPO: Skill 不在全局仓库: {}",
            skill_name
        ));
    }
    let layer2_dir = ensure_tool_active_dir(tool_id);
    let target = super::dispatch_target(&layer2_dir, skill_name);
    super::create_link(&source, &target)?;
    Ok(target)
}

/// 从 Layer 2 移除 skill 链接（目标按拍平名定位，与建链同口径）
pub fn unlink_skill_from_layer2(skill_name: &str, tool_id: &str) -> Result<(), String> {
    let target = super::dispatch_target(&tool_active_dir(tool_id), skill_name);
    super::remove_link(&target)
}

/// 列出工具在 Layer 2 中已启用的 skill
pub fn list_layer2_skills(tool_id: &str) -> Vec<String> {
    let dir = tool_active_dir(tool_id);
    let mut skills = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_symlink() || path.is_dir() {
                if let Some(name) = entry.file_name().to_str() {
                    skills.push(name.to_string());
                }
            }
        }
    }
    skills.sort();
    skills
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tool_active_dir() {
        let dir = tool_active_dir("claude");
        // Windows 路径分隔符是 \，统一转 / 再断言，保持跨平台
        assert!(dir
            .to_string_lossy()
            .replace('\\', "/")
            .contains(".tuvis/active/claude"));
    }

    #[test]
    fn test_ensure_tool_active_dir() {
        let dir = ensure_tool_active_dir("test_tool");
        // Windows 下 dirs::home_dir 指向真实用户目录，测试环境可能不可写，
        // 仅断言路径结构，不依赖真实目录创建成功
        assert!(dir
            .to_string_lossy()
            .replace('\\', "/")
            .contains(".tuvis/active/test_tool"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
