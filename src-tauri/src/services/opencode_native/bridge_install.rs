//! OpenCode 服务插件桥的安装/卸载（B1 可信首次绑定的一次性配置）。
//!
//! 只改 `~/.config/opencode/opencode.json` 的 `plugins` 数组，并写入 Tuvis 私有的
//! `plugin.mjs` 与 `bridge.json`；不改 provider、模型、权限、agent 或用户会话。
//! 安装前保留一份 JSON 备份，卸载只移除 Tuvis 自己的路径。真实执行前由 GUI 显示
//! 影响面并取得用户确认（用户已接受“首次插件/hook 配置”原则）。

use std::path::{Path, PathBuf};

use super::bridge::BridgePaths;

const PLUGIN_SOURCE: &str = include_str!("plugin.mjs");
const PLUGIN_ENTRY_PREFIX: &str = "file://";

#[derive(Clone, Debug)]
pub struct BridgeInstall {
    paths: BridgePaths,
    opencode_config: PathBuf,
}

#[derive(Clone, Debug, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BridgeInstallStatus {
    pub plugin_path: String,
    pub config_path: String,
    pub plugin_installed: bool,
    pub config_registered: bool,
    pub backup_path: String,
}

impl BridgeInstall {
    pub fn new(paths: BridgePaths, opencode_config: PathBuf) -> Self {
        Self {
            paths,
            opencode_config,
        }
    }

    /// 生产布局：`~/.tuvis/opencode-bridge` + `$XDG_CONFIG_HOME/opencode/opencode.json`
    /// （缺省 `~/.config/opencode/opencode.json`）。
    pub fn production(home: &Path, xdg_config_home: Option<&Path>) -> Self {
        let config_root = xdg_config_home
            .filter(|path| !path.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| home.join(".config"));
        Self::new(
            BridgePaths::production(home),
            config_root.join("opencode").join("opencode.json"),
        )
    }

    pub fn paths(&self) -> &BridgePaths {
        &self.paths
    }

    pub fn opencode_config(&self) -> &Path {
        &self.opencode_config
    }

    fn plugin_entry(&self) -> String {
        format!(
            "{}{}",
            PLUGIN_ENTRY_PREFIX,
            self.paths.plugin_dir.to_string_lossy()
        )
    }

    fn backup_path(&self) -> PathBuf {
        self.paths.dir.join("opencode.json.bak")
    }

    /// 写入插件与配置。重复调用幂等；不删除用户已有的其它插件，且用 JSONC 无损编辑
    /// 保留注释/尾逗号/原始排版（OpenCode 把该文件当 JSONC）。
    pub fn install(&self) -> Result<BridgeInstallStatus, String> {
        std::fs::create_dir_all(&self.paths.dir).map_err(|e| format!("bridge dir: {e}"))?;
        set_mode(&self.paths.dir, 0o700)?;
        std::fs::create_dir_all(&self.paths.plugin_dir)
            .map_err(|e| format!("bridge plugin dir: {e}"))?;
        set_mode(&self.paths.plugin_dir, 0o700)?;
        std::fs::write(&self.paths.plugin_file, PLUGIN_SOURCE)
            .map_err(|e| format!("write bridge plugin: {e}"))?;
        set_mode(&self.paths.plugin_file, 0o600)?;
        let original = read_config_text(&self.opencode_config)?;
        let backup = self.backup_path();
        // 只在首次安装前备份：重复安装/升级不能把「已含 Tuvis 插件」的中间态
        // 覆盖掉真正的安装前配置，否则回滚会回到半安装状态。
        if !backup.exists() {
            std::fs::write(&backup, &original).map_err(|e| format!("write config backup: {e}"))?;
            set_mode(&backup, 0o600)?;
        }
        let next = crate::services::mcp::jsonc::upsert_string_array(
            &original,
            "plugins",
            &self.plugin_entry(),
        )?;
        if next != original {
            write_config_text(&self.opencode_config, &next)?;
        }
        Ok(self.status())
    }

    /// 移除 Tuvis 自己的插件条目，保留其它条目、注释与插件文件（便于复核/重装）。
    pub fn uninstall(&self) -> Result<BridgeInstallStatus, String> {
        if !self.opencode_config.exists() {
            return Ok(self.status());
        }
        let original = read_config_text(&self.opencode_config)?;
        let next = crate::services::mcp::jsonc::remove_string_array(
            &original,
            "plugins",
            &self.plugin_entry(),
        )?;
        if next != original {
            write_config_text(&self.opencode_config, &next)?;
        }
        Ok(self.status())
    }

    pub fn status(&self) -> BridgeInstallStatus {
        let config_registered = read_config_text(&self.opencode_config)
            .ok()
            .and_then(|text| {
                crate::services::mcp::jsonc::string_array_contains(
                    &text,
                    "plugins",
                    &self.plugin_entry(),
                )
                .ok()
            })
            .unwrap_or(false);
        BridgeInstallStatus {
            plugin_path: self.paths.plugin_file.to_string_lossy().into_owned(),
            config_path: self.opencode_config.to_string_lossy().into_owned(),
            plugin_installed: self.paths.plugin_file.exists(),
            config_registered,
            backup_path: self.backup_path().to_string_lossy().into_owned(),
        }
    }
}

fn read_config_text(path: &Path) -> Result<String, String> {
    if !path.exists() {
        return Ok("{}".to_string());
    }
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    if text.trim().is_empty() {
        Ok("{}".to_string())
    } else {
        Ok(text)
    }
}

fn write_config_text(path: &Path, text: &str) -> Result<(), String> {
    // 跟随符号链接到真实文件，保留原文件权限；temp+rename 不能把软链接替换成普通文件，
    // 也不能把用户的 0644 配置改成 umask 默认。
    let target = if path.exists() {
        std::fs::canonicalize(path).map_err(|e| format!("resolve {}: {e}", path.display()))?
    } else {
        path.to_path_buf()
    };
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("config dir: {e}"))?;
    }
    #[cfg(unix)]
    let mode = {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(&target)
            .ok()
            .map(|metadata| metadata.permissions().mode())
    };
    #[cfg(not(unix))]
    let mode: Option<u32> = None;
    let temp = target.with_extension("tuvis-tmp");
    std::fs::write(&temp, text).map_err(|e| format!("write temp config: {e}"))?;
    if let Some(mode) = mode {
        set_mode(&temp, mode)?;
    }
    std::fs::rename(&temp, &target).map_err(|e| format!("rename config: {e}"))?;
    Ok(())
}

fn set_mode(path: &Path, mode: u32) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .map_err(|e| format!("chmod {}: {e}", path.display()))?;
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn install_merges_plugins_and_uninstall_removes_only_ours() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let config = home.join(".config/opencode/opencode.json");
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::fs::write(
            &config,
            br#"{"plugins":["existing-plugin"],"provider":{"keep":true}}"#,
        )
        .unwrap();
        let installer = BridgeInstall::production(&home, None);
        let status = installer.install().unwrap();
        assert!(status.plugin_installed);
        assert!(status.config_registered);
        let value: Value = serde_json::from_slice(&std::fs::read(&config).unwrap()).unwrap();
        let plugins = value["plugins"].as_array().unwrap();
        assert_eq!(plugins.len(), 2);
        assert_eq!(plugins[0], "existing-plugin");
        assert!(plugins[1]
            .as_str()
            .unwrap()
            .ends_with("/.tuvis/opencode-bridge/plugin"));
        assert_eq!(value["provider"]["keep"], true);
        let status = installer.uninstall().unwrap();
        assert!(!status.config_registered);
        let value: Value = serde_json::from_slice(&std::fs::read(&config).unwrap()).unwrap();
        assert_eq!(value["plugins"].as_array().unwrap().len(), 1);
        assert_eq!(value["plugins"][0], "existing-plugin");
    }

    #[test]
    fn install_and_uninstall_preserve_jsonc_comments_and_trailing_commas() {
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join("opencode.json");
        std::fs::write(
            &config,
            "{\n  // keep this comment\n  \"plugins\": [\"existing\",], // trailing\n  \"x\": 1\n}\n",
        )
        .unwrap();
        let installer = BridgeInstall::new(
            BridgePaths::in_dir(temp.path().join("bridge")),
            config.clone(),
        );
        installer.install().unwrap();
        let installed = std::fs::read_to_string(&config).unwrap();
        assert!(installed.contains("// keep this comment"));
        assert!(installed.contains("// trailing"));
        assert!(installed.contains("existing"));
        assert!(installed.contains("plugin"));
        installer.uninstall().unwrap();
        let uninstalled = std::fs::read_to_string(&config).unwrap();
        assert!(uninstalled.contains("// keep this comment"));
        assert!(uninstalled.contains("existing"));
        assert!(!uninstalled.contains("opencode-bridge/plugin"));
    }

    #[test]
    fn install_is_idempotent_and_creates_missing_config() {
        let temp = tempfile::tempdir().unwrap();
        let installer = BridgeInstall::new(
            BridgePaths::in_dir(temp.path().join("bridge")),
            temp.path().join("config/opencode.json"),
        );
        installer.install().unwrap();
        let backup = installer.paths.dir.join("opencode.json.bak");
        let first_backup = std::fs::read(&backup).unwrap();
        installer.install().unwrap();
        let value: Value =
            serde_json::from_slice(&std::fs::read(installer.opencode_config()).unwrap()).unwrap();
        assert_eq!(value["plugins"].as_array().unwrap().len(), 1);
        // 重复安装不得把首次安装前的备份覆盖成「已含插件」的中间态。
        assert_eq!(std::fs::read(&backup).unwrap(), first_backup);
    }
}
