// 扫描 — 单宠物 stat 快扫、仓库清单、codex 目录清单（spec §6-1/§8.1）
use super::error::PetRpcError;
use super::{manifest, pet_dir};
use serde::Serialize;
use std::path::Path;

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct FileStat {
    pub rel: String,
    pub exists: bool,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PetScan {
    pub id: String,
    pub dir: String,
    pub spritesheet: FileStat,
    pub voice_files: Vec<FileStat>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PetSummary {
    pub id: String,
    pub display_name: String,
    pub description: String,
    pub source: String,
    pub sprite_version_number: u8,
    pub has_voice: bool,
    pub has_subtitle: bool,
    pub manifest_exists: bool,
    pub spritesheet_exists: bool,
    pub dir: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexPetInfo {
    pub id: String,
    pub display_name: String,
    pub sprite_version_number: u8,
    pub imported: bool,
}

fn stat_rel(root: &Path, rel: &str) -> FileStat {
    match std::fs::metadata(root.join(rel)) {
        Ok(md) => FileStat {
            rel: rel.to_string(),
            exists: true,
            size: md.len(),
        },
        Err(_) => FileStat {
            rel: rel.to_string(),
            exists: false,
            size: 0,
        },
    }
}

/// 递归收集 voice/ 下合规文件（仅 voice/<group>/<file> 三段，issue #32-7，
/// 规则与暂存/remove 同源走 manifest::is_voice_rel；深层子目录与非分组目录的
/// 文件对宠物系统不可见）。entry.file_type() 不跟随符号链接（issue #32-5）：
/// 目录型链接不入栈防环，链接文件一律跳过
fn walk_voice(root: &Path) -> Vec<FileStat> {
    let mut out = Vec::new();
    let mut stack = vec![root.join("voice")];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_symlink() {
                continue;
            }
            let p = entry.path();
            if ft.is_dir() {
                stack.push(p);
                continue;
            }
            let Ok(rel) = p.strip_prefix(root) else {
                continue;
            };
            let rel = rel.to_string_lossy().replace('\\', "/");
            if !manifest::is_voice_rel(&rel) {
                continue;
            }
            out.push(FileStat {
                rel,
                exists: true,
                size: entry.metadata().map(|m| m.len()).unwrap_or(0),
            });
        }
    }
    out.sort_by(|a, b| a.rel.cmp(&b.rel));
    out
}

/// 单宠物 stat 快扫（统一校验算法的 Rust 侧输入，spec §6-1）
pub fn scan_pet_in(root: &Path, id: &str) -> Result<PetScan, PetRpcError> {
    let dir = pet_dir(root, id);
    if !dir.is_dir() {
        return Err(
            PetRpcError::new("pet-not-found", format!("宠物不存在: {}", id))
                .with("id", id.to_string()),
        );
    }
    Ok(PetScan {
        id: id.to_string(),
        dir: dir.to_string_lossy().to_string(),
        spritesheet: stat_rel(&dir, "spritesheet.webp"),
        voice_files: walk_voice(&dir),
    })
}

/// 仓库清单（跳过点开头的隐藏目录如 .import-staging，spec §9）
pub fn list_pets_in(root: &Path) -> Vec<PetSummary> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(root) else {
        return out;
    };
    for entry in rd.flatten() {
        let p = entry.path();
        if !p.is_dir() {
            continue;
        }
        let Some(id) = p.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if id.starts_with('.') || super::is_builtin_pet(id) {
            continue;
        }
        let m = manifest::load(&p);
        out.push(PetSummary {
            id: id.to_string(),
            display_name: m
                .as_ref()
                .map(|m| m.display_name.clone())
                .unwrap_or_else(|| id.to_string()),
            description: m
                .as_ref()
                .map(|m| m.description.clone())
                .unwrap_or_default(),
            source: m.as_ref().map(|m| m.source.clone()).unwrap_or_default(),
            sprite_version_number: m.as_ref().map(|m| m.sprite_version_number).unwrap_or(0),
            has_voice: m.as_ref().map(|m| m.has_voice).unwrap_or(false),
            has_subtitle: m.as_ref().map(|m| m.has_subtitle).unwrap_or(false),
            manifest_exists: m.is_some(),
            spritesheet_exists: p.join("spritesheet.webp").exists(),
            dir: p.to_string_lossy().to_string(),
        });
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct CodexPetJson {
    #[serde(default)]
    display_name: String,
    #[serde(default)]
    sprite_version_number: u8,
}

/// codex 宠物清单（导入向导渠道 A，spec §8.1）：仅返回含 spritesheet.webp 的宠物，并标注是否已导入
pub fn list_codex_pets_in(codex_root: &Path, mam_root: &Path) -> Vec<CodexPetInfo> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(codex_root) else {
        return out;
    };
    for entry in rd.flatten() {
        let p = entry.path();
        if !p.is_dir() {
            continue;
        }
        let Some(id) = p.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !p.join("spritesheet.webp").is_file() {
            continue;
        }
        let meta = std::fs::read_to_string(p.join("pet.json"))
            .ok()
            .and_then(|t| serde_json::from_str::<CodexPetJson>(&t).ok());
        out.push(CodexPetInfo {
            id: id.to_string(),
            display_name: meta
                .as_ref()
                .map(|m| m.display_name.clone())
                .unwrap_or_default(),
            sprite_version_number: meta.as_ref().map(|m| m.sprite_version_number).unwrap_or(0),
            imported: pet_dir(mam_root, id).exists(),
        });
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造临时宠物仓库：pets/<id>/{spritesheet.webp, voice/<group>/<file>}
    fn fixture(root: &Path, id: &str, with_manifest: bool) {
        let dir = pet_dir(root, id);
        std::fs::create_dir_all(dir.join("voice/general")).unwrap();
        std::fs::write(dir.join("spritesheet.webp"), b"sheet").unwrap();
        std::fs::write(dir.join("voice/general/a.m4a"), b"audio-a").unwrap();
        if with_manifest {
            let m = manifest::PetManifest {
                schema_version: 1,
                id: id.into(),
                display_name: format!("{}-disp", id),
                description: String::new(),
                source: "folder".into(),
                sprite_version_number: 1,
                spritesheet_size_bytes: 5,
                has_voice: true,
                has_subtitle: true,
                voices: vec![],
            };
            manifest::write_with_backup(&dir, &m, false).unwrap();
        }
    }

    #[test]
    fn scan_pet_reports_stats() {
        let root = tempfile::tempdir().unwrap();
        fixture(root.path(), "p1", false);
        let s = scan_pet_in(root.path(), "p1").unwrap();
        assert!(s.spritesheet.exists);
        assert_eq!(s.spritesheet.size, 5);
        assert_eq!(s.voice_files.len(), 1);
        assert_eq!(s.voice_files[0].rel, "voice/general/a.m4a");
        assert_eq!(s.voice_files[0].size, 7);
    }

    #[test]
    fn scan_missing_pet_errs() {
        let root = tempfile::tempdir().unwrap();
        assert!(scan_pet_in(root.path(), "nope").is_err());
    }

    /// issue #32-7：scan 只认 voice/<group>/<file> 三段——深层子目录与非法分组
    /// 不进 stat 清单（与暂存/复制/remove 的三段规则同源，避免 diff 误报）
    #[test]
    fn scan_voice_only_three_segments() {
        let root = tempfile::tempdir().unwrap();
        fixture(root.path(), "p1", false);
        let dir = pet_dir(root.path(), "p1");
        std::fs::create_dir_all(dir.join("voice/general/sub")).unwrap();
        std::fs::write(dir.join("voice/general/sub/deep.mp3"), b"d").unwrap();
        std::fs::create_dir_all(dir.join("voice/backup")).unwrap();
        std::fs::write(dir.join("voice/backup/x.m4a"), b"x").unwrap();
        let s = scan_pet_in(root.path(), "p1").unwrap();
        assert_eq!(s.voice_files.len(), 1, "仅三段合规文件应收录");
        assert_eq!(s.voice_files[0].rel, "voice/general/a.m4a");
    }

    #[test]
    fn list_pets_skips_hidden_and_sorts() {
        let root = tempfile::tempdir().unwrap();
        fixture(root.path(), "b-pet", true);
        fixture(root.path(), "a-pet", false);
        std::fs::create_dir_all(super::super::staging_root(root.path()).join("x")).unwrap();
        let list = list_pets_in(root.path());
        assert_eq!(
            list.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            ["a-pet", "b-pet"]
        );
        assert!(!list[0].manifest_exists);
        assert_eq!(list[0].display_name, "a-pet"); // 无 manifest 用 id
        assert!(list[1].manifest_exists);
        assert_eq!(list[1].display_name, "b-pet-disp");
        assert!(list[1].has_voice);
    }

    #[test]
    fn list_pets_hides_builtin_directories_without_deleting_them() {
        let root = tempfile::tempdir().unwrap();
        fixture(root.path(), "builtin-foxbell", true);
        fixture(root.path(), "builtin-tuvis-3d", true);
        fixture(root.path(), "FoxBell", true);
        fixture(root.path(), "external", true);
        fixture(root.path(), "tuvis-3d", true);
        let list = list_pets_in(root.path());
        assert_eq!(
            list.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(),
            ["external", "tuvis-3d"]
        );
        assert!(pet_dir(root.path(), "builtin-tuvis-3d")
            .join("spritesheet.webp")
            .exists());
    }

    #[test]
    fn list_codex_pets_filters_and_marks() {
        let codex = tempfile::tempdir().unwrap();
        let mam = tempfile::tempdir().unwrap();
        // 有图集的
        std::fs::create_dir_all(codex.path().join("alpha")).unwrap();
        std::fs::write(codex.path().join("alpha/spritesheet.webp"), b"s").unwrap();
        std::fs::write(
            codex.path().join("alpha/pet.json"),
            r#"{"displayName":"阿尔法","spriteVersionNumber":2}"#,
        )
        .unwrap();
        // 没图集的（应被过滤）
        std::fs::create_dir_all(codex.path().join("beta")).unwrap();
        let list = list_codex_pets_in(codex.path(), mam.path());
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, "alpha");
        assert_eq!(list[0].display_name, "阿尔法");
        assert_eq!(list[0].sprite_version_number, 2);
        assert!(!list[0].imported);
        // 导入后标记
        std::fs::create_dir_all(mam.path().join("alpha")).unwrap();
        assert!(list_codex_pets_in(codex.path(), mam.path())[0].imported);
    }
}
