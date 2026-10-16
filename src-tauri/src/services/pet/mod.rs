// 外部宠物服务 — 仓库路径与子模块入口（spec §4/§17）
pub mod error;
pub mod import;
pub mod manifest;
pub mod petdex;
pub mod scan;

use std::path::{Path, PathBuf};

use self::error::PetRpcError;

/// 宠物仓库根目录 ~/.tuvis/pets
pub fn pets_root() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(".tuvis")
        .join("pets")
}

/// 指定宠物的目录
pub fn pet_dir(root: &Path, id: &str) -> PathBuf {
    root.join(id)
}

/// 宠物 id（=文件夹名）长度上限：64 已远超实际 slug 形态，并为路径深度留足余量
/// （issue #32-1，原实现无上限）
pub const MAX_PET_ID_LEN: usize = 64;

/// Windows 保留设备名（大小写不敏感，任意扩展名形态均保留）。宠物 id 白名单
/// 字符集不含点，仅需全名匹配（issue #32-1）
const WINDOWS_RESERVED_DEVICES: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// IPC 传入的宠物 id 统一白名单校验：id 即文件夹名，`pet_dir` 是裸 join，
/// 不设卡则 `../`、`..\`、绝对路径均可逃逸出仓库（如 pet_delete_pet("..") 会把
/// ~/.tuvis 整目录送回收站）。静态规则与 validate_pet_name 一致（复用 pet-name-* 错误码，
/// 前端码表/i18n 无需新增）；存在性检查留给各命令自身的语义。
pub fn validate_pet_id(id: &str) -> Result<(), PetRpcError> {
    if id.is_empty() {
        return Err(PetRpcError::new("pet-name-empty", "宠物名不能为空"));
    }
    if id.starts_with('.') {
        return Err(PetRpcError::new(
            "pet-name-dot-prefix",
            "宠物名不能以点开头",
        ));
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(PetRpcError::new(
            "pet-name-illegal",
            "宠物名仅支持字母/数字/连字符/下划线",
        ));
    }
    if id.len() > MAX_PET_ID_LEN {
        return Err(PetRpcError::new(
            "pet-name-too-long",
            format!("宠物名过长（≤{MAX_PET_ID_LEN} 字符）"),
        )
        .with("max", MAX_PET_ID_LEN.to_string()));
    }
    if WINDOWS_RESERVED_DEVICES
        .iter()
        .any(|d| id.eq_ignore_ascii_case(d))
    {
        return Err(PetRpcError::new(
            "pet-name-reserved-device",
            "宠物名与 Windows 保留设备名冲突",
        ));
    }
    if is_builtin_pet(id) {
        return Err(PetRpcError::new(
            "pet-name-reserved",
            "该名称为内置宠物保留名",
        ));
    }
    Ok(())
}

/// 内置名称由静态资源提供，不允许外部目录覆盖（大小写不敏感）。
/// 与前端注册表对齐：src/components/pet/petRuntime.ts 的 BUILTIN_PET_DEFS
/// （id 形如 builtin-<目录名>）；"foxbell" 是 0.5.x 无前缀旧 id，保留禁用防旧目录冲突。
pub fn is_builtin_pet(id: &str) -> bool {
    ["builtin-foxbell", "builtin-tuvis-3d", "foxbell"]
        .iter()
        .any(|builtin| id.eq_ignore_ascii_case(builtin))
}

/// 启动清扫 .import-staging 残留（崩溃/强杀留下的半截导入与 petdex 解压中间产物，
/// issue #32-3，spec §8.4-6 要求取消/失败无残留）。仅清暂存区不触碰宠物目录；
/// 调用时机=应用启动后台线程，此时不存在运行中的导入，清扫是安全的
pub fn sweep_staging_in(root: &Path) -> std::io::Result<()> {
    let sroot = staging_root(root);
    let Ok(rd) = std::fs::read_dir(&sroot) else {
        return Ok(());
    };
    for e in rd.flatten() {
        let p = e.path();
        let res = if p.is_dir() {
            std::fs::remove_dir_all(&p)
        } else {
            std::fs::remove_file(&p)
        };
        if let Err(err) = res {
            log::warn!("清扫暂存区残留失败 {}: {err}", p.display());
        }
    }
    Ok(())
}

/// sweep_staging_in 的全局仓库包装（lib.rs 启动线程调用）
pub fn sweep_staging() {
    if let Err(err) = sweep_staging_in(&pets_root()) {
        log::warn!("清扫导入暂存区失败: {err}");
    }
}

/// 导入暂存区根目录 ~/.tuvis/pets/.import-staging（隐藏目录，清单扫描自动跳过）
pub fn staging_root(root: &Path) -> PathBuf {
    root.join(".import-staging")
}

/// 重命名宠物 = 目录重命名 + manifest.id 同步（备份旧 manifest，spec §10-1）。
/// 顺序：先 rename 目录再写 manifest——rename 失败零副作用。
/// manifest 写失败为非致命：manifest.id 仅是展示字段，宠物身份以文件夹名为准
/// （校验/匹配/激活均用文件夹名），目录已改名即主操作已完成；返回 Err 会让 UI 把
/// 已成功的改名报成失败，且用户重试会命中"宠物不存在: old_id"的死路。
pub fn rename_pet_in(root: &Path, old_id: &str, new_id: &str) -> Result<(), PetRpcError> {
    if old_id == new_id {
        return Ok(());
    }
    let old_dir = pet_dir(root, old_id);
    if !old_dir.is_dir() {
        return Err(
            PetRpcError::new("pet-not-found", format!("宠物不存在: {}", old_id))
                .with("id", old_id.to_string()),
        );
    }
    import::validate_pet_name(root, new_id)?;
    let new_dir = pet_dir(root, new_id);
    std::fs::rename(&old_dir, &new_dir).map_err(|e| {
        PetRpcError::new("rename-failed", format!("重命名失败: {}", e)).with("err", e.to_string())
    })?;
    if let Some(mut m) = manifest::load(&new_dir) {
        m.id = new_id.to_string();
        if let Err(e) = manifest::write_with_backup(&new_dir, &m, true) {
            // 目录已改名（主操作完成），仅 id 展示字段未同步：记日志、不判失败（下次激活/修复会兜底）
            log::warn!(
                "manifest.id 同步失败（目录已改名 {} → {}）: {:?}",
                old_id,
                new_id,
                e
            );
        }
    }
    Ok(())
}

/// 删除宠物：整目录移入回收站（spec §10；trash crate 已是项目依赖）
pub fn delete_pet_in(root: &Path, id: &str) -> Result<(), PetRpcError> {
    let dir = pet_dir(root, id);
    if !dir.is_dir() {
        return Err(
            PetRpcError::new("pet-not-found", format!("宠物不存在: {}", id))
                .with("id", id.to_string()),
        );
    }
    trash::delete(&dir).map_err(|e| {
        PetRpcError::new("delete-failed", format!("删除失败: {}", e)).with("err", e.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mkpet(root: &std::path::Path, id: &str) {
        let dir = pet_dir(root, id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("spritesheet.webp"), b"s").unwrap();
    }

    #[test]
    fn rename_updates_dir_and_manifest_id() {
        let root = tempfile::tempdir().unwrap();
        mkpet(root.path(), "old-name");
        let m = manifest::PetManifest {
            schema_version: 1,
            id: "old-name".into(),
            display_name: "D".into(),
            description: String::new(),
            source: "folder".into(),
            sprite_version_number: 1,
            spritesheet_size_bytes: 1,
            has_voice: false,
            has_subtitle: false,
            voices: vec![],
        };
        manifest::write_with_backup(&pet_dir(root.path(), "old-name"), &m, false).unwrap();
        rename_pet_in(root.path(), "old-name", "new-name").unwrap();
        assert!(pet_dir(root.path(), "new-name").is_dir());
        assert!(!pet_dir(root.path(), "old-name").exists());
        let m2 = manifest::load(&pet_dir(root.path(), "new-name")).unwrap();
        assert_eq!(m2.id, "new-name");
        // 备份存在且记录旧 id
        assert!(pet_dir(root.path(), "new-name")
            .join(manifest::BACKUP_FILE)
            .is_file());
    }

    #[test]
    fn rename_conflict_errs() {
        let root = tempfile::tempdir().unwrap();
        mkpet(root.path(), "a");
        mkpet(root.path(), "b");
        assert!(rename_pet_in(root.path(), "a", "b").is_err());
        // 重命名为自身是 no-op
        assert!(rename_pet_in(root.path(), "a", "a").is_ok());
        // FIX-5：rename 失败（目标已存在）时旧目录 manifest 未被改写（零副作用）
        let old_manifest = manifest::load(&pet_dir(root.path(), "a")).unwrap_or_else(|| {
            let m = manifest::PetManifest {
                schema_version: 1,
                id: "a".into(),
                display_name: "A".into(),
                description: String::new(),
                source: "folder".into(),
                sprite_version_number: 1,
                spritesheet_size_bytes: 1,
                has_voice: false,
                has_subtitle: false,
                voices: vec![],
            };
            manifest::write_with_backup(&pet_dir(root.path(), "a"), &m, false).unwrap();
            m
        });
        assert!(rename_pet_in(root.path(), "a", "b").is_err());
        let after = manifest::load(&pet_dir(root.path(), "a")).unwrap();
        assert_eq!(after.id, old_manifest.id, "rename 失败不应改写旧 manifest");
        assert_eq!(after.id, "a");
    }

    /// P0-1：id 白名单码级断言（../、..\、绝对路径、空串、点、点前缀、foxbell 变体）
    #[test]
    fn validate_pet_id_rejects_escape_and_reserved() {
        let cases: &[(&str, &str)] = &[
            ("", "pet-name-empty"),
            (".", "pet-name-dot-prefix"),
            ("..", "pet-name-dot-prefix"),
            (".hidden", "pet-name-dot-prefix"),
            // 点前缀规则先于字符白名单命中（拒绝顺序）
            ("../skills", "pet-name-dot-prefix"),
            ("..\\skills", "pet-name-dot-prefix"),
            ("/etc", "pet-name-illegal"),
            ("C:\\Windows", "pet-name-illegal"),
            ("a/b", "pet-name-illegal"),
            ("foxbell", "pet-name-reserved"),
            ("FoxBell", "pet-name-reserved"),
            ("builtin-foxbell", "pet-name-reserved"),
            ("Builtin-FoxBell", "pet-name-reserved"),
            ("builtin-tuvis-3d", "pet-name-reserved"),
            ("Builtin-Tuvis-3D", "pet-name-reserved"),
        ];
        for (id, code) in cases {
            match validate_pet_id(id) {
                Err(e) => assert_eq!(&e.code, code, "id {id:?} 应命中 {code}"),
                Ok(()) => panic!("id {id:?} 应被拒绝"),
            }
        }
        for ok in ["starry-dew", "abc_123-X", "A9", "tuvis-3d"] {
            validate_pet_id(ok).unwrap_or_else(|e| panic!("合法 id {ok:?} 被误拒: {:?}", e.code));
        }
    }

    /// issue #32-1：Windows 保留设备名拒绝（大小写不敏感；白名单字符集不含点，
    /// 故无需考虑 con.txt 形态）+ 长度上限（MAX_PET_ID_LEN，预留路径深度余量）
    #[test]
    fn validate_pet_id_rejects_windows_reserved_device_and_overlong() {
        for bad in [
            "con", "CON", "Nul", "aux", "PRN", "nul", "com1", "Com9", "lpt1", "LPT9",
        ] {
            match validate_pet_id(bad) {
                Err(e) => assert_eq!(e.code, "pet-name-reserved-device", "id {bad:?}"),
                Ok(()) => panic!("保留设备名 {bad:?} 应被拒绝"),
            }
        }
        let long = "a".repeat(super::MAX_PET_ID_LEN + 1);
        match validate_pet_id(&long) {
            Err(e) => {
                assert_eq!(e.code, "pet-name-too-long");
                assert_eq!(e.params.get("max").map(String::as_str), Some("64"));
            }
            Ok(()) => panic!("超长 id 应被拒绝"),
        }
        assert!(validate_pet_id(&"a".repeat(super::MAX_PET_ID_LEN)).is_ok());
    }

    /// issue #32-3：启动清扫 .import-staging 残留（崩溃/强杀遗留），不触碰宠物目录
    #[test]
    fn sweep_staging_clears_leftovers_only() {
        let root = tempfile::tempdir().unwrap();
        let pet = root.path().join("real-pet");
        std::fs::create_dir_all(pet.join("voice/general")).unwrap();
        std::fs::write(pet.join("spritesheet.webp"), b"s").unwrap();
        let sroot = staging_root(root.path());
        for leftover in ["leftover-1", "extract-abc"] {
            std::fs::create_dir_all(sroot.join(leftover).join("voice/general")).unwrap();
            std::fs::write(sroot.join(leftover).join("spritesheet.webp"), b"s").unwrap();
        }
        sweep_staging_in(root.path()).unwrap();
        assert!(!sroot.join("leftover-1").exists(), "暂存残留应被清扫");
        assert!(
            !sroot.join("extract-abc").exists(),
            "petdex 解压残留应被清扫"
        );
        assert!(pet.is_dir(), "正常宠物目录不得被触碰");
        assert!(pet.join("voice/general").is_dir());
    }
}
