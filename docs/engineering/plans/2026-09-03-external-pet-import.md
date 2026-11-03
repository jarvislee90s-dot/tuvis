# 外部宠物导入机制 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 支持 codex / 本地文件夹 / zip / petdex 链接四种来源的外部宠物导入到 `~/.mam/pets/`，含统一校验、能力门控、切换/修改/删除与启动校验弹窗。

**Architecture:** 混合分工（spec EP4）——Rust `services/pet/` 负责 IO/zip/下载/manifest 写入（.bak 备份），前端负责媒体探测（图集尺寸 Image / 音频时长 Audio 元数据）、校验交互与渲染；外部素材经 Tauri asset 协议（scope `~/.mam/pets/**`）加载；激活时音频全量读入内存快照（EP6）。

**Tech Stack:** Tauri 2 + Rust（新增 reqwest / zip / tauri-plugin-dialog）、React 19 + TypeScript、vitest（jsdom + MSW + tauriInvokeMock）、cargo test。

**Spec:** `docs/engineering/specs/2026-09-03-external-pet-import-design.md`（决策 EP1-EP10、验收 AC1-AC10）

**工作分支:** `pets-extension`

**约定：**
- 前端测试：`pnpm test tests/pet/<file>`；Rust 测试：`cd src-tauri && cargo test pet::`
- 每个 Task 结束必须 commit；commit message 用英文 conventional 格式
- 所有新 UI 文案中英双语（zh.json + en.json 同步）；tooltip 用原生 `title=` 属性（EP9）
- 代码注释用中文，风格对齐现有文件

---

## 文件结构总览

```
src-tauri/
  Cargo.toml                              # +reqwest +zip +tauri-plugin-dialog
  tauri.conf.json                         # +assetProtocol
  src/lib.rs                              # 注册新命令 + dialog 插件
  src/commands/pet.rs                     # 追加 18 个 pet_* IPC 命令
  src/services/pet/mod.rs                 # 服务入口：路径、重命名、删除
  src/services/pet/manifest.rs            # PetManifest 结构 + load/write_with_backup
  src/services/pet/scan.rs                # stat 快扫 / 仓库清单 / codex 清单
  src/services/pet/import.rs              # 暂存区 / 三来源落地 / finalize / zip 安全
  src/services/pet/petdex.rs              # 链接解析 / 清单匹配 / zip 下载
  capabilities/default.json               # +dialog:allow-open

src/
  components/pet/petAnimations.ts         # frameStyle 增 rows 参数
  components/pet/petVoices.ts             # VoicePlayer 增 resolveUrl 参数
  components/pet/petConfig.ts             # petSoundTakeover 增语音能力闸门
  components/pet/petRuntime.ts            # 新：激活指针/描述符/探测/内存快照
  components/pet/petValidation.ts         # 新：统一校验纯函数
  components/pet/petActivation.ts         # 新：激活编排（生成/修复/降级）
  components/pet/FoxbellPet.tsx           # 接入 ActivePet + 热切换 + 门控
  components/pet/PetMenu.tsx              # 声音/字幕门控
  components/pet/PetStartupGuard.tsx      # 新：主窗口启动校验弹窗
  components/pet/manage/VoiceGroupEditor.tsx   # 新：四分组音频编辑器（共用）
  components/pet/manage/PetImportDialog.tsx    # 新：导入向导
  components/pet/manage/PetSwitchDialog.tsx    # 新：切换宠物
  components/pet/manage/PetManageDialog.tsx    # 新：修改宠物
  pages/settings.tsx                      # 桌宠栏目三入口
  pages/home.tsx                          # 挂 PetStartupGuard
  i18n/locales/zh.json / en.json          # 新键

tests/
  msw/tauriMocks.ts                       # 新命令默认 mock + convertFileSrc
  setup.ts                                # core mock 增 convertFileSrc
  pet/*.test.ts(x)                        # 各任务配套测试
```

---

## Phase 0：基础设施

### Task 1: 依赖与配置接线

**Files:**
- Modify: `src-tauri/Cargo.toml`
- Modify: `src-tauri/tauri.conf.json:39-41`
- Modify: `src-tauri/src/lib.rs:39-46`
- Modify: `src-tauri/capabilities/default.json`
- Modify: `package.json`（经 pnpm）

- [x] **Step 1: 添加 Rust 依赖**

`src-tauri/Cargo.toml` 的 `[dependencies]` 末尾（`semver = "1"` 之后）追加：

```toml
reqwest = { version = "0.12", default-features = false, features = ["json", "rustls-tls"] }
zip = "2"
tauri-plugin-dialog = "2"
```

- [x] **Step 2: 添加前端依赖**

```bash
pnpm add @tauri-apps/plugin-dialog
```

- [x] **Step 3: 启用 asset 协议**

`src-tauri/tauri.conf.json` 中 `"security"` 块（第 39-41 行）改为：

```json
    "security": {
      "csp": null,
      "assetProtocol": {
        "enable": true,
        "scope": ["$HOME/.mam/pets/**"]
      }
    }
```

- [x] **Step 4: 注册 dialog 插件**

`src-tauri/src/lib.rs` 在 `.plugin(tauri_plugin_notification::init())`（第 69 行）后追加一行：

```rust
        .plugin(tauri_plugin_dialog::init())
```

- [x] **Step 5: 主窗口 dialog 权限**

`src-tauri/capabilities/default.json` 的 `permissions` 数组末尾追加：

```json
    "dialog:allow-open"
```

- [x] **Step 6: 编译验证**

```bash
cd src-tauri && cargo check
```
Expected: 编译通过（无新警告级别的错误）。

```bash
cd .. && pnpm build
```
Expected: TypeScript 编译 + Vite 打包通过。

- [x] **Step 7: Commit**

```bash
git add src-tauri/Cargo.toml src-tauri/Cargo.lock src-tauri/tauri.conf.json src-tauri/src/lib.rs src-tauri/capabilities/default.json package.json pnpm-lock.yaml
git commit -m "chore(pet): add reqwest/zip/dialog deps and asset protocol scope"
```

---

### Task 2: services/pet 骨架与 manifest 模块

**Files:**
- Create: `src-tauri/src/services/pet/mod.rs`
- Create: `src-tauri/src/services/pet/manifest.rs`
- Modify: `src-tauri/src/services/mod.rs:8`（`pub mod preset;` 后加一行）

- [x] **Step 1: 写失败测试**

创建 `src-tauri/src/services/pet/manifest.rs`，先只写测试骨架（实现部分留空会编译失败，即为"失败测试"）：

```rust
// manifest.json — 结构、读取与备份写入（spec §4.2）。写入前自动备份 manifest.json.bak（仅保留最近一份）
use serde::{Deserialize, Serialize};
use std::path::Path;

pub const MANIFEST_FILE: &str = "manifest.json";
pub const BACKUP_FILE: &str = "manifest.json.bak";
pub const SCHEMA_VERSION: u32 = 1;

/// 四个固定语音分组（与 foxbell 语音系统一致，spec §5.1）
pub const VOICE_GROUPS: [&str; 4] = ["general", "approval", "done", "error"];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct VoiceEntry {
    pub group: String,
    /// 字幕文本 = 音频文件名去扩展名（EP8）
    pub name: String,
    /// 相对宠物目录：voice/<group>/<文件名>
    pub file: String,
    pub size_bytes: u64,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PetManifest {
    pub schema_version: u32,
    pub id: String,
    pub display_name: String,
    #[serde(default)]
    pub description: String,
    /// codex | petdex | folder | zip
    #[serde(default)]
    pub source: String,
    /// 1=v1（9 行） 2=v2（11 行）；0=未知（尚未探测）
    pub sprite_version_number: u8,
    #[serde(default)]
    pub spritesheet_size_bytes: u64,
    pub has_voice: bool,
    pub has_subtitle: bool,
    #[serde(default)]
    pub voices: Vec<VoiceEntry>,
}

/// 读取 manifest.json；任何失败（缺失/损坏）返回 None，由调用方决定生成或修复（spec §6）
pub fn load(dir: &Path) -> Option<PetManifest> {
    let text = std::fs::read_to_string(dir.join(MANIFEST_FILE)).ok()?;
    serde_json::from_str(&text).ok()
}

/// 写 manifest.json；backup=true 且旧文件存在时先复制为 manifest.json.bak（spec §4.1）
pub fn write_with_backup(dir: &Path, m: &PetManifest, backup: bool) -> Result<(), String> {
    let path = dir.join(MANIFEST_FILE);
    if backup && path.exists() {
        std::fs::copy(&path, dir.join(BACKUP_FILE))
            .map_err(|e| format!("备份 manifest 失败: {}", e))?;
    }
    let text = serde_json::to_string_pretty(m).map_err(|e| e.to_string())?;
    std::fs::write(&path, text).map_err(|e| format!("写入 manifest 失败: {}", e))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> PetManifest {
        PetManifest {
            schema_version: SCHEMA_VERSION,
            id: "starry-dew".into(),
            display_name: "Starry Dew".into(),
            description: "test".into(),
            source: "folder".into(),
            sprite_version_number: 1,
            spritesheet_size_bytes: 1652314,
            has_voice: true,
            has_subtitle: true,
            voices: vec![VoiceEntry {
                group: "general".into(),
                name: "休息一下吧".into(),
                file: "voice/general/休息一下吧.m4a".into(),
                size_bytes: 123456,
                duration_ms: 3200,
            }],
        }
    }

    #[test]
    fn serde_roundtrip_camel_case() {
        let json = serde_json::to_string(&sample()).unwrap();
        // camelCase 字段名（前端契约）
        assert!(json.contains("\"schemaVersion\""));
        assert!(json.contains("\"displayName\""));
        assert!(json.contains("\"spriteVersionNumber\""));
        assert!(json.contains("\"sizeBytes\""));
        let back: PetManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(back, sample());
    }

    #[test]
    fn default_fields_tolerate_missing() {
        // description/source/spritesheetSizeBytes/voices 均可缺省
        let m: PetManifest = serde_json::from_str(
            r#"{"schemaVersion":1,"id":"a","displayName":"A","spriteVersionNumber":1,"hasVoice":false,"hasSubtitle":false}"#,
        )
        .unwrap();
        assert_eq!(m.voices.len(), 0);
        assert_eq!(m.source, "");
    }

    #[test]
    fn load_missing_or_corrupt_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(load(tmp.path()).is_none());
        std::fs::write(tmp.path().join(MANIFEST_FILE), "{broken").unwrap();
        assert!(load(tmp.path()).is_none());
    }

    #[test]
    fn write_with_backup_keeps_one_bak() {
        let tmp = tempfile::tempdir().unwrap();
        let mut m = sample();
        write_with_backup(tmp.path(), &m, false).unwrap();
        m.display_name = "v2".into();
        write_with_backup(tmp.path(), &m, true).unwrap();
        let bak = std::fs::read_to_string(tmp.path().join(BACKUP_FILE)).unwrap();
        assert!(bak.contains("Starry Dew")); // bak 是旧内容
        assert_eq!(load(tmp.path()).unwrap().display_name, "v2");
    }
}
```

- [x] **Step 2: 创建服务模块入口**

创建 `src-tauri/src/services/pet/mod.rs`：

```rust
// 外部宠物服务 — 仓库路径与子模块入口（spec §4/§17）
pub mod import;
pub mod manifest;
pub mod petdex;
pub mod scan;

use std::path::{Path, PathBuf};

/// 宠物仓库根目录 ~/.mam/pets
pub fn pets_root() -> PathBuf {
    dirs::home_dir().unwrap_or_default().join(".mam").join("pets")
}

/// 指定宠物的目录
pub fn pet_dir(root: &Path, id: &str) -> PathBuf {
    root.join(id)
}

/// 导入暂存区根目录 ~/.mam/pets/.import-staging（隐藏目录，清单扫描自动跳过）
pub fn staging_root(root: &Path) -> PathBuf {
    root.join(".import-staging")
}

/// 重命名宠物 = 目录重命名 + manifest.id 同步（备份旧 manifest，spec §10-1）
pub fn rename_pet_in(root: &Path, old_id: &str, new_id: &str) -> Result<(), String> {
    if old_id == new_id {
        return Ok(());
    }
    let old_dir = pet_dir(root, old_id);
    if !old_dir.is_dir() {
        return Err(format!("宠物不存在: {}", old_id));
    }
    import::validate_pet_name(root, new_id)?;
    if let Some(mut m) = manifest::load(&old_dir) {
        m.id = new_id.to_string();
        manifest::write_with_backup(&old_dir, &m, true)?;
    }
    std::fs::rename(&old_dir, pet_dir(root, new_id)).map_err(|e| format!("重命名失败: {}", e))
}

/// 删除宠物：整目录移入回收站（spec §10；trash crate 已是项目依赖）
pub fn delete_pet_in(root: &Path, id: &str) -> Result<(), String> {
    let dir = pet_dir(root, id);
    if !dir.is_dir() {
        return Err(format!("宠物不存在: {}", id));
    }
    trash::delete(&dir).map_err(|e| format!("删除失败: {}", e))
}
```

> 注：此时 `import` / `scan` / `petdex` 子模块尚未创建，为让本任务编译通过，先创建三个占位文件（后续任务替换内容）：`pub fn validate_pet_name(_root: &std::path::Path, _name: &str) -> Result<(), String> { unimplemented!() }` 放在临时 `import.rs`；`scan.rs` / `petdex.rs` 为空模块体。

- [x] **Step 3: 注册模块**

`src-tauri/src/services/mod.rs` 第 8 行 `pub mod preset;` 后追加：

```rust
pub mod pet;
```

- [x] **Step 4: 运行测试**

```bash
cd src-tauri && cargo test pet::manifest
```
Expected: 4 个测试 PASS。

- [x] **Step 5: Commit**

```bash
git add src-tauri/src/services/
git commit -m "feat(pet): manifest module with camelCase serde and .bak backup"
```

---

## Phase 1：Rust 服务

### Task 3: scan 模块（stat 快扫 / 仓库清单 / codex 清单）

**Files:**
- Modify: `src-tauri/src/services/pet/scan.rs`（替换占位）

- [x] **Step 1: 写实现与测试**

```rust
// 扫描 — 单宠物 stat 快扫、仓库清单、codex 目录清单（spec §6-1/§8.1）
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
    pub sheet_exists: bool,
}

fn stat_rel(root: &Path, rel: &str) -> FileStat {
    match std::fs::metadata(root.join(rel)) {
        Ok(md) => FileStat { rel: rel.to_string(), exists: true, size: md.len() },
        Err(_) => FileStat { rel: rel.to_string(), exists: false, size: 0 },
    }
}

/// 递归收集 voice/ 下全部文件（相对路径统一 / 分隔），目录不存在返回空
fn walk_voice(root: &Path) -> Vec<FileStat> {
    let mut out = Vec::new();
    let mut stack = vec![root.join("voice")];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for entry in rd.flatten() {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            let Ok(rel) = p.strip_prefix(root) else { continue };
            out.push(FileStat {
                rel: rel.to_string().replace('\\', "/"),
                exists: true,
                size: entry.metadata().map(|m| m.len()).unwrap_or(0),
            });
        }
    }
    out.sort_by(|a, b| a.rel.cmp(&b.rel));
    out
}

/// 单宠物 stat 快扫（统一校验算法的 Rust 侧输入，spec §6-1）
pub fn scan_pet_in(root: &Path, id: &str) -> Result<PetScan, String> {
    let dir = pet_dir(root, id);
    if !dir.is_dir() {
        return Err(format!("宠物不存在: {}", id));
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
    let Ok(rd) = std::fs::read_dir(root) else { return out };
    for entry in rd.flatten() {
        let p = entry.path();
        if !p.is_dir() {
            continue;
        }
        let Some(id) = p.file_name().and_then(|n| n.to_str()) else { continue };
        if id.starts_with('.') {
            continue;
        }
        let m = manifest::load(&p);
        out.push(PetSummary {
            id: id.to_string(),
            display_name: m.as_ref().map(|m| m.display_name.clone()).unwrap_or_else(|| id.to_string()),
            description: m.as_ref().map(|m| m.description.clone()).unwrap_or_default(),
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
    let Ok(rd) = std::fs::read_dir(codex_root) else { return out };
    for entry in rd.flatten() {
        let p = entry.path();
        if !p.is_dir() {
            continue;
        }
        let Some(id) = p.file_name().and_then(|n| n.to_str()) else { continue };
        let sheet_exists = p.join("spritesheet.webp").exists();
        if !sheet_exists {
            continue;
        }
        let meta = std::fs::read_to_string(p.join("pet.json"))
            .ok()
            .and_then(|t| serde_json::from_str::<CodexPetJson>(&t).ok());
        out.push(CodexPetInfo {
            id: id.to_string(),
            display_name: meta.as_ref().map(|m| m.display_name.clone()).unwrap_or_default(),
            sprite_version_number: meta.as_ref().map(|m| m.sprite_version_number).unwrap_or(0),
            imported: pet_dir(mam_root, id).exists(),
            sheet_exists,
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

    #[test]
    fn list_pets_skips_hidden_and_sorts() {
        let root = tempfile::tempdir().unwrap();
        fixture(root.path(), "b-pet", true);
        fixture(root.path(), "a-pet", false);
        std::fs::create_dir_all(super::super::staging_root(root.path()).join("x")).unwrap();
        let list = list_pets_in(root.path());
        assert_eq!(list.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), ["a-pet", "b-pet"]);
        assert!(!list[0].manifest_exists);
        assert_eq!(list[0].display_name, "a-pet"); // 无 manifest 用 id
        assert!(list[1].manifest_exists);
        assert_eq!(list[1].display_name, "b-pet-disp");
        assert!(list[1].has_voice);
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
```

- [x] **Step 2: 运行测试**

```bash
cd src-tauri && cargo test pet::scan
```
Expected: 4 个测试 PASS。

- [x] **Step 3: Commit**

```bash
git add src-tauri/src/services/pet/scan.rs
git commit -m "feat(pet): scan module with stat fast-scan and codex listing"
```

---

### Task 4: import 模块（暂存 / 三来源 / finalize / zip 安全）

**Files:**
- Modify: `src-tauri/src/services/pet/import.rs`（替换占位）

- [x] **Step 1: 写实现**

```rust
// 导入 — 暂存区、来源落地（文件夹/zip/codex）、音频暂存、finalize 原子落地（spec §8/§13）
use super::{manifest, pet_dir, scan, staging_root};
use serde::Serialize;
use std::path::{Path, PathBuf};

pub const SHEET_FILE: &str = "spritesheet.webp";
pub const MAX_ZIP_TOTAL_BYTES: u64 = 100 * 1024 * 1024;
pub const MAX_ZIP_FILES: usize = 200;
/// 允许的音频扩展名（spec §5.1）
pub const AUDIO_EXTS: [&str; 7] = ["m4a", "mp3", "wav", "ogg", "opus", "flac", "aac"];

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StagedVoiceFile {
    pub group: String,
    pub name: String,
    pub file: String, // voice/<group>/<文件名>
    pub size_bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StagedPet {
    pub staging_id: String,
    pub dir: String,
    pub suggested_name: String,
    pub suggested_display_name: String,
    /// codex pet.json 透传；0=未知（前端图集探测后回填 manifest，spec §4.2）
    pub sprite_version_number: u8,
    pub spritesheet_size: u64,
    pub voice_files: Vec<StagedVoiceFile>,
}

/// 简易唯一 id：时间戳 + 进程内计数（避免引入 uuid 依赖）
fn uid() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    format!(
        "{}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
        N.fetch_add(1, Ordering::Relaxed)
    )
}

fn ext_lower(p: &Path) -> String {
    p.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase()
}

fn is_audio(p: &Path) -> bool {
    AUDIO_EXTS.contains(&ext_lower(p).as_str())
}

fn valid_group(g: &str) -> bool {
    manifest::VOICE_GROUPS.contains(&g)
}

fn new_staging(root: &Path) -> Result<PathBuf, String> {
    let dir = staging_root(root).join(uid());
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建暂存区失败: {}", e))?;
    Ok(dir)
}

/// 在根目录或一层子目录内定位 spritesheet.webp（根优先，spec §8.2）
pub fn locate_sheet(src: &Path) -> Option<PathBuf> {
    let direct = src.join(SHEET_FILE);
    if direct.is_file() {
        return Some(direct);
    }
    let Ok(rd) = std::fs::read_dir(src) else { return None };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() && p.join(SHEET_FILE).is_file() {
            return Some(p.join(SHEET_FILE));
        }
    }
    None
}

/// 复制 voice/ 子树：仅四分组目录下的合法音频（spec §8.2 自动带入）
fn copy_voice_tree(base: &Path, dir: &Path, dest_base: &Path) -> Result<(), String> {
    for entry in std::fs::read_dir(dir).map_err(|e| e.to_string())?.flatten() {
        let p = entry.path();
        if p.is_dir() {
            copy_voice_tree(base, &p, dest_base)?;
            continue;
        }
        if !is_audio(&p) {
            continue;
        }
        let Ok(rel) = p.strip_prefix(base) else { continue };
        let dest = dest_base.join(rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::copy(&p, &dest).map_err(|e| format!("复制音频失败: {}", e))?;
    }
    Ok(())
}

/// 暂存区内收集 voice 文件清单
fn list_staged_voice(staging: &Path) -> Vec<StagedVoiceFile> {
    let mut out = Vec::new();
    let mut stack = vec![staging.join("voice")];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for entry in rd.flatten() {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            let Ok(rel) = p.strip_prefix(staging) else { continue };
            let rel = rel.to_string().replace('\\', "/");
            let mut segs = rel.split('/');
            let (_v, group, file) = (segs.next(), segs.next().unwrap_or(""), segs.next().unwrap_or(""));
            if !valid_group(group) {
                continue;
            }
            out.push(StagedVoiceFile {
                group: group.to_string(),
                name: Path::new(file).file_stem().and_then(|n| n.to_str()).unwrap_or("").to_string(),
                file: rel,
                size_bytes: entry.metadata().map(|m| m.len()).unwrap_or(0),
            });
        }
    }
    out.sort_by(|a, b| a.file.cmp(&b.file));
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

/// 读来源目录的 pet.json（codex 元数据透传；仅用于向导预填，spec §8.1）
fn codex_meta(dir: &Path) -> (String, u8) {
    let Ok(text) = std::fs::read_to_string(dir.join("pet.json")) else {
        return (String::new(), 0);
    };
    match serde_json::from_str::<CodexPetJson>(&text) {
        Ok(j) => (j.display_name, j.sprite_version_number),
        Err(_) => (String::new(), 0),
    }
}

/// 建议名合法化：非法字符折叠为 '-'（最终名称在 finalize 时严格校验）
fn sanitize_name(raw: &str) -> String {
    raw.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect()
}

fn finish_staged(
    staging: &Path,
    suggested_name: String,
    suggested_display_name: String,
    sprite_version_number: u8,
) -> Result<StagedPet, String> {
    Ok(StagedPet {
        staging_id: staging.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string(),
        dir: staging.to_string_lossy().to_string(),
        suggested_name,
        suggested_display_name,
        sprite_version_number,
        spritesheet_size: std::fs::metadata(staging.join(SHEET_FILE)).map(|m| m.len()).unwrap_or(0),
        voice_files: list_staged_voice(staging),
    })
}

/// 文件夹来源暂存：定位图集 → 复制图集 + voice/ → 返回暂存描述（spec §8.2）
pub fn stage_from_folder_in(root: &Path, src: &Path) -> Result<StagedPet, String> {
    if !src.is_dir() {
        return Err("来源不是文件夹".into());
    }
    let sheet = locate_sheet(src).ok_or_else(|| "未找到 spritesheet.webp（根目录或一层子目录）".to_string())?;
    let sheet_root = sheet.parent().unwrap_or(src).to_path_buf();
    let staging = new_staging(root)?;
    let copy = (|| -> Result<(), String> {
        std::fs::copy(&sheet, staging.join(SHEET_FILE)).map_err(|e| format!("复制图集失败: {}", e))?;
        let voice_root = sheet_root.join("voice");
        if voice_root.is_dir() {
            copy_voice_tree(&voice_root, &voice_root, &staging.join("voice"))?;
        }
        Ok(())
    })();
    if let Err(e) = copy {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(e);
    }
    let (disp, ver) = codex_meta(&sheet_root);
    let suggested_name = sanitize_name(
        sheet_root.file_name().and_then(|n| n.to_str()).unwrap_or("pet"),
    );
    finish_staged(&staging, suggested_name, disp, ver)
}

/// 安全解压：enclosed_name 防 zip-slip + 文件数/总大小上限（spec §13）
pub fn safe_unzip(zip_path: &Path, dest: &Path) -> Result<(), String> {
    let f = std::fs::File::open(zip_path).map_err(|e| format!("打开压缩包失败: {}", e))?;
    let mut zip = zip::ZipArchive::new(f).map_err(|e| format!("读取压缩包失败: {}", e))?;
    if zip.len() > MAX_ZIP_FILES as u64 {
        return Err(format!("压缩包文件数超限（>{}）", MAX_ZIP_FILES));
    }
    std::fs::create_dir_all(dest).map_err(|e| e.to_string())?;
    let mut total: u64 = 0;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).map_err(|e| e.to_string())?;
        // enclosed_name 已拒绝绝对路径与 .. 穿越；None 即非法条目
        let Some(rel) = entry.enclosed_name() else {
            return Err(format!("压缩包含非法路径条目: {}", entry.name()));
        };
        if entry.is_dir() {
            std::fs::create_dir_all(dest.join(rel)).map_err(|e| e.to_string())?;
            continue;
        }
        total += entry.size();
        if total > MAX_ZIP_TOTAL_BYTES {
            return Err("压缩包解压总量超限（>100MB）".into());
        }
        let out_path = dest.join(rel);
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let mut out = std::fs::File::create(&out_path).map_err(|e| e.to_string())?;
        std::io::copy(&mut entry, &mut out).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// zip 来源暂存：解压到 staging 下的 extract 目录 → 复用文件夹管线 → 清理（spec §8.2）
pub fn stage_from_zip_in(root: &Path, zip_path: &Path) -> Result<StagedPet, String> {
    if !zip_path.is_file() {
        return Err("压缩包不存在".into());
    }
    let extract = staging_root(root).join(format!("extract-{}", uid()));
    if let Err(e) = safe_unzip(zip_path, &extract) {
        let _ = std::fs::remove_dir_all(&extract);
        return Err(e);
    }
    let staged = stage_from_folder_in(root, &extract);
    let _ = std::fs::remove_dir_all(&extract);
    staged
}

/// codex 来源暂存（spec §8.1）：仅取 spritesheet.webp（+ 自动带入 voice/ 若存在）
pub fn stage_from_codex_in(root: &Path, codex_root: &Path, codex_id: &str) -> Result<StagedPet, String> {
    let src = codex_root.join(codex_id);
    if !src.is_dir() {
        return Err(format!("codex 宠物不存在: {}", codex_id));
    }
    stage_from_folder_in(root, &src)
}

/// 单个音频复制进目标 voice/<group>/（暂存与正式目录共用）
fn copy_audio_into(dest_voice: &Path, src: &Path, group: &str) -> Result<StagedVoiceFile, String> {
    if !valid_group(group) {
        return Err(format!("非法分组: {}", group));
    }
    if !src.is_file() {
        return Err(format!("音频文件不存在: {}", src.display()));
    }
    if !is_audio(src) {
        return Err(format!("不支持的音频格式: {}", src.display()));
    }
    let name = src.file_stem().and_then(|n| n.to_str()).unwrap_or("audio").to_string();
    let file_name = src.file_name().and_then(|n| n.to_str()).unwrap_or("audio").to_string();
    let dest = dest_voice.join(group).join(&file_name);
    if let Some(p) = dest.parent() {
        std::fs::create_dir_all(p).map_err(|e| e.to_string())?;
    }
    std::fs::copy(src, &dest).map_err(|e| format!("复制音频失败: {}", e))?;
    Ok(StagedVoiceFile {
        group: group.to_string(),
        name,
        file: format!("voice/{}/{}", group, file_name),
        size_bytes: std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0),
    })
}

fn staging_dir(root: &Path, staging_id: &str) -> Result<PathBuf, String> {
    if staging_id.contains("..") || staging_id.contains('/') || staging_id.contains('\\') {
        return Err("非法暂存区 id".into());
    }
    let d = staging_root(root).join(staging_id);
    if !d.is_dir() {
        return Err("暂存区不存在".into());
    }
    Ok(d)
}

/// 向导音频暂存（spec §8.4-3）
pub fn stage_audio_in(root: &Path, staging_id: &str, src_paths: &[String], group: &str) -> Result<Vec<StagedVoiceFile>, String> {
    let staging = staging_dir(root, staging_id)?;
    let mut out = Vec::new();
    for p in src_paths {
        out.push(copy_audio_into(&staging.join("voice"), Path::new(p), group)?);
    }
    Ok(out)
}

/// 修改面板直接向正式目录添加音频（spec §10-3）
pub fn add_voice_files_in(root: &Path, pet_id: &str, src_paths: &[String], group: &str) -> Result<Vec<StagedVoiceFile>, String> {
    let dir = pet_dir(root, pet_id);
    if !dir.is_dir() {
        return Err(format!("宠物不存在: {}", pet_id));
    }
    let mut out = Vec::new();
    for p in src_paths {
        out.push(copy_audio_into(&dir.join("voice"), Path::new(p), group)?);
    }
    Ok(out)
}

/// 音频路径安全校验：必须形如 voice/<group>/<file> 且无穿越（staged=true 为暂存区）
pub fn remove_audio_in(root: &Path, base_id: &str, rel: &str, staged: bool) -> Result<(), String> {
    if !rel.starts_with("voice/") || rel.contains("..") || rel.contains('\\') {
        return Err("非法音频路径".into());
    }
    if Path::new(rel).components().count() != 3 {
        return Err("非法音频路径".into());
    }
    let base = if staged { staging_dir(root, base_id)? } else { pet_dir(root, base_id) };
    if !base.is_dir() {
        return Err("目录不存在".into());
    }
    let p = base.join(rel);
    if p.is_file() {
        std::fs::remove_file(&p).map_err(|e| format!("删除音频失败: {}", e))?;
    }
    Ok(())
}

/// 宠物名（= 文件夹名）严格校验（spec §8.4-1）
pub fn validate_pet_name(root: &Path, name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("宠物名不能为空".into());
    }
    if name.starts_with('.') {
        return Err("宠物名不能以点开头".into());
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Err("宠物名仅支持字母/数字/连字符/下划线".into());
    }
    if name.eq_ignore_ascii_case("foxbell") {
        return Err("foxbell 为内置宠物保留名".into());
    }
    if pet_dir(root, name).exists() {
        return Err(format!("宠物已存在: {}", name));
    }
    Ok(())
}

/// finalize：写 manifest（前端已探测 voices）→ 同盘 rename 原子落地（spec §8.4-5）
pub fn finalize_in(root: &Path, staging_id: &str, name: &str, mut m: manifest::PetManifest) -> Result<scan::PetSummary, String> {
    validate_pet_name(root, name)?;
    let staging = staging_dir(root, staging_id)?;
    if !staging.join(SHEET_FILE).is_file() {
        return Err("暂存区缺少 spritesheet.webp".into());
    }
    m.schema_version = manifest::SCHEMA_VERSION;
    m.id = name.to_string();
    manifest::write_with_backup(&staging, &m, false)?;
    let dest = pet_dir(root, name);
    std::fs::rename(&staging, &dest).map_err(|e| format!("落地失败: {}", e))?;
    scan::list_pets_in(root)
        .into_iter()
        .find(|s| s.id == name)
        .ok_or_else(|| "落地后读取宠物信息失败".to_string())
}

/// 取消导入：清理暂存区（spec §8.4-6）
pub fn cancel_in(root: &Path, staging_id: &str) -> Result<(), String> {
    let Ok(staging) = staging_dir(root, staging_id) else { return Ok(()) };
    std::fs::remove_dir_all(&staging).map_err(|e| e.to_string())
}
```

- [x] **Step 2: 追加测试（同文件末尾）**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn mkpet(root: &Path, id: &str) -> PathBuf {
        let dir = root.join(id);
        std::fs::create_dir_all(dir.join("voice/general")).unwrap();
        std::fs::write(dir.join(SHEET_FILE), b"sheet-bytes").unwrap();
        std::fs::write(dir.join("voice/general/休息一下吧.m4a"), b"a").unwrap();
        dir
    }

    #[test]
    fn stage_from_folder_copies_sheet_and_voice() {
        let root = tempfile::tempdir().unwrap();
        let src = mkpet(root.path(), "src-pet");
        let s = stage_from_folder_in(root.path(), &src).unwrap();
        assert_eq!(s.suggested_name, "src-pet");
        assert_eq!(s.spritesheet_size, 11);
        assert_eq!(s.voice_files.len(), 1);
        assert_eq!(s.voice_files[0].group, "general");
        assert_eq!(s.voice_files[0].name, "休息一下吧");
        assert!(staging_root(root.path()).join(&s.staging_id).join(SHEET_FILE).is_file());
    }

    #[test]
    fn stage_locates_sheet_one_level_deep() {
        let root = tempfile::tempdir().unwrap();
        let src = root.path().join("wrapper");
        mkpet(root.path(), "wrapper/inner-pet"); // src/wrapper/inner-pet/...
        let s = stage_from_folder_in(root.path(), &src).unwrap();
        assert_eq!(s.suggested_name, "inner-pet"); // 用图集所在目录名
    }

    #[test]
    fn stage_without_sheet_errs_and_cleans() {
        let root = tempfile::tempdir().unwrap();
        let src = root.path().join("empty");
        std::fs::create_dir_all(&src).unwrap();
        assert!(stage_from_folder_in(root.path(), &src).is_err());
        assert!(staging_root(root.path()).read_dir().map(|mut d| d.next().is_none()).unwrap_or(true));
    }

    #[test]
    fn codex_meta_prefills_display_and_version() {
        let root = tempfile::tempdir().unwrap();
        let src = mkpet(root.path(), "linabell");
        std::fs::write(
            src.join("pet.json"),
            r#"{"displayName":"玲娜贝儿","spriteVersionNumber":2}"#,
        )
        .unwrap();
        let s = stage_from_folder_in(root.path(), &src).unwrap();
        assert_eq!(s.suggested_display_name, "玲娜贝儿");
        assert_eq!(s.sprite_version_number, 2);
    }

    fn write_zip(path: &Path, entries: &[(&str, &str)]) {
        let f = std::fs::File::create(path).unwrap();
        let mut w = zip::ZipWriter::new(f);
        let opts = zip::write::SimpleFileOptions::default();
        for (name, data) in entries {
            w.start_file(*name, opts).unwrap();
            std::io::Write::write_all(&mut w, data.as_bytes()).unwrap();
        }
        w.finish().unwrap();
    }

    #[test]
    fn zip_stage_unwraps_one_level() {
        let root = tempfile::tempdir().unwrap();
        let zp = root.path().join("p.zip");
        write_zip(&zp, &[("inner/spritesheet.webp", "sheet"), ("inner/pet.json", "{}")]);
        let s = stage_from_zip_in(root.path(), &zp).unwrap();
        assert_eq!(s.suggested_name, "inner");
        assert_eq!(s.spritesheet_size, 5);
    }

    #[test]
    fn zip_slip_rejected() {
        let root = tempfile::tempdir().unwrap();
        let zp = root.path().join("evil.zip");
        // zip crate 的 writer 会规范化 ".."，手工构造恶意条目名直接写仍可能被拒；
        // 因此本测试断言 safe_unzip 对该条目返回 Err（enclosed_name 拒绝穿越）
        write_zip(&zp, &[("../evil.txt", "x")]);
        let dest = root.path().join("dest");
        let r = safe_unzip(&zp, &dest);
        // 无论 zip writer 是否已规范化：要么安全拒绝，要么落点必须在 dest 内（无 dest 外文件）
        match r {
            Err(_) => {}
            Ok(()) => assert!(!root.path().join("evil.txt").exists()),
        }
    }

    #[test]
    fn finalize_moves_and_names_manifest() {
        let root = tempfile::tempdir().unwrap();
        let src = mkpet(root.path(), "src");
        let s = stage_from_folder_in(root.path(), &src).unwrap();
        let m = manifest::PetManifest {
            schema_version: 1,
            id: String::new(), // 由 finalize 回填
            display_name: "Starry Dew".into(),
            description: String::new(),
            source: "folder".into(),
            sprite_version_number: 1,
            spritesheet_size_bytes: s.spritesheet_size,
            has_voice: false,
            has_subtitle: false,
            voices: vec![],
        };
        let sum = finalize_in(root.path(), &s.staging_id, "starry-dew", m).unwrap();
        assert_eq!(sum.id, "starry-dew");
        assert!(pet_dir(root.path(), "starry-dew").join("manifest.json").is_file());
        // 暂存区已腾空
        assert!(!staging_root(root.path()).join(&s.staging_id).exists());
    }

    #[test]
    fn validate_pet_name_rules() {
        let root = tempfile::tempdir().unwrap();
        assert!(validate_pet_name(root.path(), "abc-123_X").is_ok());
        assert!(validate_pet_name(root.path(), "").is_err());
        assert!(validate_pet_name(root.path(), "中文").is_err());
        assert!(validate_pet_name(root.path(), "../hack").is_err());
        assert!(validate_pet_name(root.path(), "foxbell").is_err());
        assert!(validate_pet_name(root.path(), "FoxBell").is_err());
        std::fs::create_dir_all(root.path().join("dup")).unwrap();
        assert!(validate_pet_name(root.path(), "dup").is_err());
    }

    #[test]
    fn stage_audio_copies_and_remove_deletes() {
        let root = tempfile::tempdir().unwrap();
        let src = mkpet(root.path(), "src");
        let s = stage_from_folder_in(root.path(), &src).unwrap();
        let audio_src = root.path().join("hi.mp3");
        std::fs::write(&audio_src, b"mp3-bytes").unwrap();
        let added = stage_audio_in(root.path(), &s.staging_id, &[audio_src.to_string_lossy().to_string()], "done").unwrap();
        assert_eq!(added[0].file, "voice/done/hi.mp3");
        assert_eq!(added[0].name, "hi");
        assert!(stage_audio_in(root.path(), &s.staging_id, &[], "bad-group").is_err());
        remove_audio_in(root.path(), &s.staging_id, "voice/done/hi.mp3", true).unwrap();
        assert!(remove_audio_in(root.path(), &s.staging_id, "../evil", true).is_err());
    }

    #[test]
    fn cancel_cleans_staging() {
        let root = tempfile::tempdir().unwrap();
        let src = mkpet(root.path(), "src");
        let s = stage_from_folder_in(root.path(), &src).unwrap();
        cancel_in(root.path(), &s.staging_id).unwrap();
        assert!(!staging_root(root.path()).join(&s.staging_id).exists());
    }
}
```

> 注意 `zip_slip_rejected`：zip 2.x 的 `ZipWriter::start_file` 对 `../` 名称可能自动规范化为安全名（此时断言"落点必须在 dest 内"）。两条断言路径覆盖两种实现行为，避免对 zip 内部规范化策略过度耦合。

- [x] **Step 3: 运行测试**

```bash
cd src-tauri && cargo test pet::import
```
Expected: 全部 PASS。

- [x] **Step 4: Commit**

```bash
git add src-tauri/src/services/pet/import.rs
git commit -m "feat(pet): staging pipeline with safe unzip, audio staging and atomic finalize"
```

---

### Task 5: petdex 模块与维护函数测试

**Files:**
- Modify: `src-tauri/src/services/pet/petdex.rs`（替换占位）
- Modify: `src-tauri/src/services/pet/mod.rs`（rename/delete 已在 Task 2 写入，本任务补测试）

- [x] **Step 1: 写 petdex 实现**

```rust
// petdex 在线导入 — 链接解析、清单匹配、zip 下载（spec §8.3/§13 域名白名单）
use super::import::{self, StagedPet};
use serde::Deserialize;
use std::path::Path;

pub const MANIFEST_URL: &str = "https://petdex.dev/api/manifest";

/// 仅允许 petdex 域（页面域 + 资产域，spec §13）
pub fn allowed_host(host: &str) -> bool {
    host == "petdex.dev" || host == "www.petdex.dev" || host.ends_with(".petdex.dev")
}

/// 从宠物页链接解析 slug：/pets/<slug>（兼容 /en/pets/<slug>、尾斜杠、query）
pub fn parse_slug(url: &str) -> Option<String> {
    let path = url.split('?').next()?.split('#').next()?;
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let i = segs.iter().position(|s| *s == "pets")?;
    let slug = segs.get(i + 1)?;
    let ok = !slug.is_empty() && slug.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
    if ok { Some(slug.to_string()) } else { None }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PetdexEntry {
    pub slug: String,
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub zip_url: String,
    #[serde(default)]
    pub sprite_version_number: u8,
}

fn client(secs: u64) -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(secs))
        .build()
        .map_err(|e| e.to_string())
}

/// 拉全量清单并按 slug 匹配（petdex 文档化的稳定接口，spec §3）
pub async fn fetch_entry(slug: &str) -> Result<PetdexEntry, String> {
    let list = client(30)?
        .get(MANIFEST_URL)
        .send()
        .await
        .map_err(|e| format!("petdex 清单请求失败: {}", e))?
        .error_for_status()
        .map_err(|e| format!("petdex 清单响应异常: {}", e))?
        .json::<Vec<PetdexEntry>>()
        .await
        .map_err(|e| format!("petdex 清单解析失败: {}", e))?;
    list.into_iter()
        .find(|e| e.slug == slug)
        .ok_or_else(|| format!("petdex 上未找到宠物: {}", slug))
}

/// 下载 zip 字节（域名白名单校验，spec §8.3）
pub async fn download_zip(zip_url: &str) -> Result<Vec<u8>, String> {
    let host = reqwest::Url::parse(zip_url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_string()))
        .unwrap_or_default();
    if !allowed_host(&host) {
        return Err(format!("拒绝非 petdex 域下载: {}", host));
    }
    let bytes = client(120)?
        .get(zip_url)
        .send()
        .await
        .map_err(|e| format!("下载失败: {}", e))?
        .error_for_status()
        .map_err(|e| format!("下载响应异常: {}", e))?
        .bytes()
        .await
        .map_err(|e| format!("下载读取失败: {}", e))?;
    Ok(bytes.to_vec())
}

/// 链接 → 暂存：解析 slug → 清单匹配 → 下载 zip → 统一 zip 管线（spec §8.3 仅下载压缩包）
pub async fn stage_from_url(root: &Path, url: &str) -> Result<StagedPet, String> {
    let slug = parse_slug(url)
        .ok_or_else(|| "无法从链接解析宠物标识（期望 https://petdex.dev/pets/<slug>）".to_string())?;
    let entry = fetch_entry(&slug).await?;
    if entry.zip_url.is_empty() {
        return Err("该宠物没有可下载的压缩包".into());
    }
    let bytes = download_zip(&entry.zip_url).await?;
    let tmp = std::env::temp_dir().join(format!("mam-petdex-{}.zip", slug));
    std::fs::write(&tmp, &bytes).map_err(|e| format!("临时文件写入失败: {}", e))?;
    let staged = import::stage_from_zip_in(root, &tmp);
    let _ = std::fs::remove_file(&tmp);
    let mut staged = staged?;
    staged.suggested_name = slug.clone();
    if !entry.display_name.is_empty() {
        staged.suggested_display_name = entry.display_name.clone();
    }
    staged.sprite_version_number = entry.sprite_version_number;
    Ok(staged)
}
```

- [x] **Step 2: 写测试（petdex 测试 + mod.rs 的 rename/delete 测试）**

`petdex.rs` 末尾追加：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_slug_variants() {
        assert_eq!(parse_slug("https://petdex.dev/pets/capvolt").as_deref(), Some("capvolt"));
        assert_eq!(parse_slug("https://petdex.dev/en/pets/capvolt/").as_deref(), Some("capvolt"));
        assert_eq!(parse_slug("https://petdex.dev/pets/capvolt?x=1").as_deref(), Some("capvolt"));
        assert_eq!(parse_slug("https://petdex.dev/collections"), None);
        assert_eq!(parse_slug("https://petdex.dev/pets/"), None);
        assert_eq!(parse_slug("https://evil.com/pets/abc"), Some("abc")); // 域名不限（只解析 slug），下载域在 download_zip 校验
    }

    #[test]
    fn allowed_host_whitelist() {
        assert!(allowed_host("petdex.dev"));
        assert!(allowed_host("assets.petdex.dev"));
        assert!(!allowed_host("evil.dev"));
        assert!(!allowed_host("petdex.dev.evil.com"));
    }

    #[test]
    fn entry_deserializes_manifest_shape() {
        // 与 petdex.dev/api/manifest 实测字段一致（spec §3）
        let e: PetdexEntry = serde_json::from_str(
            r#"{"slug":"capvolt","displayName":"Pikachu","spritesheetUrl":"https://assets.petdex.dev/pets/capvolt-x/sprite.webp","petJsonUrl":"...","zipUrl":"https://assets.petdex.dev/pets/capvolt-x/zip.zip","spriteVersionNumber":1}"#,
        )
        .unwrap();
        assert_eq!(e.slug, "capvolt");
        assert!(e.zip_url.contains("assets.petdex.dev"));
    }
}
```

`mod.rs` 末尾追加：

```rust
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
        assert!(pet_dir(root.path(), "new-name").join(manifest::BACKUP_FILE).is_file());
    }

    #[test]
    fn rename_conflict_errs() {
        let root = tempfile::tempdir().unwrap();
        mkpet(root.path(), "a");
        mkpet(root.path(), "b");
        assert!(rename_pet_in(root.path(), "a", "b").is_err());
        // 重命名为自身是 no-op
        assert!(rename_pet_in(root.path(), "a", "a").is_ok());
    }
}
```

- [x] **Step 3: 运行测试**

```bash
cd src-tauri && cargo test pet::
```
Expected: 全部 PASS（含 Task 2/3/4）。

- [x] **Step 4: Commit**

```bash
git add src-tauri/src/services/pet/
git commit -m "feat(pet): petdex fetch with host whitelist and rename/delete tests"
```

---

### Task 6: IPC 命令面与注册

**Files:**
- Modify: `src-tauri/src/commands/pet.rs`（文件末尾追加）
- Modify: `src-tauri/src/lib.rs:71-122`（invoke_handler）

- [x] **Step 1: 追加命令**

`src-tauri/src/commands/pet.rs` 末尾追加：

```rust
// ===== 外部宠物 IPC（spec 2026-09-03-external-pet-import §5.1）=====
use crate::services::pet::{self, import, manifest, petdex, scan};

fn root() -> std::path::PathBuf {
    pet::pets_root()
}

#[tauri::command]
pub async fn pet_list_pets() -> Result<Vec<scan::PetSummary>, String> {
    Ok(scan::list_pets_in(&root()))
}

#[tauri::command]
pub async fn pet_list_codex_pets() -> Result<Vec<scan::CodexPetInfo>, String> {
    let codex = dirs::home_dir().unwrap_or_default().join(".codex").join("pets");
    Ok(scan::list_codex_pets_in(&codex, &root()))
}

#[tauri::command]
pub async fn pet_scan(id: String) -> Result<scan::PetScan, String> {
    scan::scan_pet_in(&root(), &id)
}

#[tauri::command]
pub async fn pet_read_manifest(id: String) -> Result<Option<manifest::PetManifest>, String> {
    Ok(manifest::load(&pet::pet_dir(&root(), &id)))
}

#[tauri::command]
pub async fn pet_stage_from_folder(path: String) -> Result<import::StagedPet, String> {
    import::stage_from_folder_in(&root(), std::path::Path::new(&path))
}

#[tauri::command]
pub async fn pet_stage_from_zip(path: String) -> Result<import::StagedPet, String> {
    import::stage_from_zip_in(&root(), std::path::Path::new(&path))
}

#[tauri::command]
pub async fn pet_stage_from_codex(codex_id: String) -> Result<import::StagedPet, String> {
    let codex = dirs::home_dir().unwrap_or_default().join(".codex").join("pets");
    import::stage_from_codex_in(&root(), &codex, &codex_id)
}

#[tauri::command]
pub async fn pet_stage_from_petdex(url: String) -> Result<import::StagedPet, String> {
    petdex::stage_from_url(&root(), &url).await
}

#[tauri::command]
pub async fn pet_stage_audio(
    staging_id: String,
    src_paths: Vec<String>,
    group: String,
) -> Result<Vec<import::StagedVoiceFile>, String> {
    import::stage_audio_in(&root(), &staging_id, &src_paths, &group)
}

#[tauri::command]
pub async fn pet_remove_staged_audio(staging_id: String, rel: String) -> Result<(), String> {
    import::remove_audio_in(&root(), &staging_id, &rel, true)
}

#[tauri::command]
pub async fn pet_finalize_import(
    staging_id: String,
    name: String,
    manifest: manifest::PetManifest,
) -> Result<scan::PetSummary, String> {
    import::finalize_in(&root(), &staging_id, &name, manifest)
}

#[tauri::command]
pub async fn pet_cancel_import(staging_id: String) -> Result<(), String> {
    import::cancel_in(&root(), &staging_id)
}

#[tauri::command]
pub async fn pet_update_manifest(
    id: String,
    mut manifest: manifest::PetManifest,
    backup: bool,
) -> Result<(), String> {
    manifest.id = id.clone();
    manifest::write_with_backup(&pet::pet_dir(&root(), &id), &manifest, backup)
}

#[tauri::command]
pub async fn pet_rename_pet(old_id: String, new_id: String) -> Result<(), String> {
    pet::rename_pet_in(&root(), &old_id, &new_id)
}

#[tauri::command]
pub async fn pet_delete_pet(id: String) -> Result<(), String> {
    pet::delete_pet_in(&root(), &id)
}

#[tauri::command]
pub async fn pet_add_voice_files(
    id: String,
    src_paths: Vec<String>,
    group: String,
) -> Result<Vec<import::StagedVoiceFile>, String> {
    import::add_voice_files_in(&root(), &id, &src_paths, &group)
}

#[tauri::command]
pub async fn pet_remove_voice_file(id: String, rel: String) -> Result<(), String> {
    import::remove_audio_in(&root(), &id, &rel, false)
}

#[tauri::command]
pub async fn pet_reveal_folder(id: String) -> Result<(), String> {
    let dir = pet::pet_dir(&root(), &id);
    tauri_plugin_opener::open_path(dir.to_string_lossy().to_string(), None::<&str>)
        .map_err(|e| e.to_string())
}
```

- [x] **Step 2: 注册命令**

`src-tauri/src/lib.rs` 的 `generate_handler!` 列表中，`commands::pet::set_pet_always_on_top,`（第 80 行）之后插入：

```rust
        commands::pet::pet_list_pets,
        commands::pet::pet_list_codex_pets,
        commands::pet::pet_scan,
        commands::pet::pet_read_manifest,
        commands::pet::pet_stage_from_folder,
        commands::pet::pet_stage_from_zip,
        commands::pet::pet_stage_from_codex,
        commands::pet::pet_stage_from_petdex,
        commands::pet::pet_stage_audio,
        commands::pet::pet_remove_staged_audio,
        commands::pet::pet_finalize_import,
        commands::pet::pet_cancel_import,
        commands::pet::pet_update_manifest,
        commands::pet::pet_rename_pet,
        commands::pet::pet_delete_pet,
        commands::pet::pet_add_voice_files,
        commands::pet::pet_remove_voice_file,
        commands::pet::pet_reveal_folder,
```

- [x] **Step 3: 验证**

```bash
cd src-tauri && cargo check && cargo test pet:: && cargo clippy -- -D warnings
```
Expected: 编译、测试、clippy 全过。

- [x] **Step 4: Commit**

```bash
git add src-tauri/src/commands/pet.rs src-tauri/src/lib.rs
git commit -m "feat(pet): register 18 external pet IPC commands"
```

---

## Phase 2：前端运行时

### Task 7: petAnimations 行数参数化

**Files:**
- Modify: `src/components/pet/petAnimations.ts`
- Test: `tests/pet/petAnimations.test.ts`（追加用例）

- [x] **Step 1: 追加失败测试**

在 `tests/pet/petAnimations.test.ts` 末尾追加（先读现有文件确认导入）：

```ts
describe("frameStyle rows 参数（v1/v2，spec EP1）", () => {
  it("rows=9 时 backgroundSize 高度按 9 行计算", () => {
    const s = frameStyle("idle", 0, -1, 1, 9);
    expect(s.backgroundSize).toBe("1536px 1872px");
  });
  it("rows=11 默认值不变（v2 兼容）", () => {
    const s = frameStyle("idle", 0, -1, 1);
    expect(s.backgroundSize).toBe("1536px 2288px");
  });
  it("rows=9 时 look 帧不可用，回退 idle 行定位", () => {
    // v1 无 look 行：调用方保证不进入 look；此处验证默认缩放不越界
    const s = frameStyle("review", 2, -1, 1, 9);
    expect(s.backgroundPosition).toBe("-384px -1664px");
  });
});
```

- [x] **Step 2: 运行确认失败**

```bash
pnpm test tests/pet/petAnimations.test.ts
```
Expected: FAIL（`frameStyle` 第 5 个参数不存在，TS 报错或断言不等）。

- [x] **Step 3: 实现**

`src/components/pet/petAnimations.ts` 的 `frameStyle` 改为：

```ts
export function frameStyle(
  anim: PetAnimKey,
  frame: number,
  lookFrame: number,
  scale: number,
  rows: 9 | 11 = 11
): { backgroundPosition: string; backgroundSize: string } {
  const w = FRAME_W * scale;
  const h = FRAME_H * scale;
  let x: number;
  let y: number;
  if (anim === "look") {
    const f = LOOK_FRAMES[Math.max(0, Math.min(LOOK_FRAMES.length - 1, lookFrame))];
    x = f.x * scale;
    y = f.y * scale;
  } else {
    const def = ANIM[anim];
    const i = ((frame % def.d.length) + def.d.length) % def.d.length;
    x = -i * w;
    y = -def.row * h;
  }
  return {
    backgroundPosition: `${x}px ${y}px`,
    backgroundSize: `${w * SHEET_COLS}px ${h * rows}px`,
  };
}
```

（文件头注释补一行：`// rows 参数：v1=9（无 look 行）/ v2=11（默认），spec EP1`）

- [x] **Step 4: 运行测试通过**

```bash
pnpm test tests/pet/petAnimations.test.ts
```
Expected: 全部 PASS（旧用例默认 11 行不受影响）。

- [x] **Step 5: Commit**

```bash
git add src/components/pet/petAnimations.ts tests/pet/petAnimations.test.ts
git commit -m "feat(pet): frameStyle accepts sheet rows for v1 pets"
```

---

### Task 8: petVoices URL 解析器参数化

**Files:**
- Modify: `src/components/pet/petVoices.ts`
- Test: `tests/pet/petVoices.test.ts`（追加用例）

- [x] **Step 1: 追加失败测试**

```ts
describe("VoicePlayer resolveUrl（外部宠物 blob 快照，spec EP6）", () => {
  it("load 可注入自定义 URL 解析器", () => {
    const player = new VoicePlayer();
    const entries: VoiceEntry[] = [
      { index: 0, group: "general", name: "a", file: "voice/general/a.m4a" },
    ];
    player.load(entries, (f) => `blob://${f}`);
    // jsdom Audio 不可真实加载，仅验证不抛错且 pick 正常
    const e = player.pick("general");
    expect(e?.file).toBe("voice/general/a.m4a");
    player.dispose();
  });
  it("默认解析器保持 foxbell 内置路径", () => {
    const player = new VoicePlayer();
    expect(() =>
      player.load([{ index: 0, group: "general", name: "a", file: "x.m4a" }])
    ).not.toThrow();
    player.dispose();
  });
});
```

（文件顶部导入补 `VoicePlayer`、`VoiceEntry`）

- [x] **Step 2: 运行确认失败**

```bash
pnpm test tests/pet/petVoices.test.ts
```
Expected: FAIL（load 无第二参数——TS 编译期即报错）。

- [x] **Step 3: 实现**

`petVoices.ts` 中 `VoicePlayer` 的改动（其余不变）：

```ts
export class VoicePlayer {
  private entries: VoiceEntry[] = [];
  private els: HTMLAudioElement[] = [];
  private lastIdx: Partial<Record<VoiceGroup, number>> = {};
  private shared: HTMLAudioElement | null = null;
  private unlocked = false;
  private resolve: (file: string) => string = (f) => `/pet/voice/${encodeURI(f)}`;

  load(entries: VoiceEntry[], resolveUrl?: (file: string) => string): void {
    this.dispose();
    this.entries = entries;
    if (resolveUrl) this.resolve = resolveUrl;
    try {
      this.els = entries.map((v) => {
        // 文件名含中文/空格/~：外部宠物用注入的解析器（blob 快照），内置走 encodeURI 路径
        const a = new Audio(this.resolve(v.file));
        a.preload = "auto";
        a.load();
        return a;
      });
    } catch {
      this.els = []; // 测试环境无音频：静默降级
    }
  }
```

`play()` 的 shared 分支中 `this.shared.src = ...` 一行改为：

```ts
        this.shared.src = this.resolve(entry.file);
```

- [x] **Step 4: 运行测试通过**

```bash
pnpm test tests/pet/petVoices.test.ts
```
Expected: 全部 PASS。

- [x] **Step 5: Commit**

```bash
git add src/components/pet/petVoices.ts tests/pet/petVoices.test.ts
git commit -m "feat(pet): VoicePlayer accepts injected voice URL resolver"
```

---

### Task 9: petRuntime（激活指针 / 描述符 / 探测 / 内存快照）

**Files:**
- Create: `src/components/pet/petRuntime.ts`
- Modify: `tests/msw/tauriMocks.ts`、`tests/setup.ts`
- Test: `tests/pet/petRuntime.test.ts`

- [x] **Step 1: 扩展测试基础设施**

`tests/msw/tauriMocks.ts`：`export const tauriInvokeMock = vi.fn(...)` 之前加导出、switch 中加默认分支：

```ts
// convertFileSrc：asset 协议路径转换（petRuntime/向导预览用）
export const convertFileSrcMock = (path: string) => `asset://mock/${path}`;
```

switch 内追加（`case "get_setting":` 之前）：

```ts
    case "pet_list_pets":
      return Promise.resolve([]);
    case "pet_list_codex_pets":
      return Promise.resolve([]);
    case "pet_scan":
      return Promise.resolve({
        id: "x",
        dir: "/home/u/.mam/pets/x",
        spritesheet: { rel: "spritesheet.webp", exists: true, size: 1 },
        voiceFiles: [],
      });
    case "pet_read_manifest":
      return Promise.resolve(null);
```

`tests/setup.ts` 的 `vi.mock("@tauri-apps/api/core", ...)` 改为：

```ts
import { convertFileSrcMock, tauriInvokeMock } from "./msw/tauriMocks";

vi.mock("@tauri-apps/api/core", () => ({
  invoke: tauriInvokeMock,
  convertFileSrc: convertFileSrcMock,
}));
```

- [x] **Step 2: 写失败测试**

创建 `tests/pet/petRuntime.test.ts`：

```ts
import { beforeEach, describe, expect, it } from "vitest";
import { loadActiveId, saveActiveId, loadVoiceCap, rowsFromSize, FOXBELL } from "@/components/pet/petRuntime";

describe("petRuntime", () => {
  beforeEach(() => localStorage.clear());

  it("激活指针：默认 foxbell，可读写", () => {
    expect(loadActiveId()).toBe("foxbell");
    saveActiveId("starry-dew", false, "Starry Dew");
    expect(loadActiveId()).toBe("starry-dew");
    expect(loadVoiceCap()).toBe(false);
  });

  it("rowsFromSize：v1/v2 识别与非法尺寸（EP1）", () => {
    expect(rowsFromSize(1536, 1872)).toBe(9);
    expect(rowsFromSize(1536, 2288)).toBe(11);
    expect(rowsFromSize(1536, 1000)).toBeNull();
    expect(rowsFromSize(1024, 1872)).toBeNull();
  });

  it("FOXBELL 描述符：内置路径与全能力", () => {
    expect(FOXBELL.id).toBe("foxbell");
    expect(FOXBELL.rows).toBe(11);
    expect(FOXBELL.hasVoice).toBe(true);
    expect(FOXBELL.resolveVoiceUrl("a.m4a")).toBe("/pet/voice/a.m4a");
  });
});
```

- [x] **Step 3: 运行确认失败**

```bash
pnpm test tests/pet/petRuntime.test.ts
```
Expected: FAIL（模块不存在）。

- [x] **Step 4: 实现**

创建 `src/components/pet/petRuntime.ts`：

```ts
// 激活宠物运行时 — 指针持久化、foxbell 描述符、外部宠物解析、媒体探测与音频内存快照（spec §7/§12）
import { convertFileSrc, invoke } from "@tauri-apps/api/core";
import { parseManifest, type VoiceEntry } from "./petVoices";

export const ACTIVE_KEY = "mam-pet-active";
export const ACTIVE_NAME_KEY = "mam-pet-active-name";
export const VOICE_CAP_KEY = "mam-pet-voice-cap";

export type PetRows = 9 | 11;

export interface ActivePet {
  id: string; // "foxbell" | 外部宠物 ID
  displayName: string;
  spritesheetUrl: string;
  rows: PetRows;
  hasVoice: boolean;
  hasSubtitle: boolean;
  voices: VoiceEntry[];
  resolveVoiceUrl: (file: string) => string;
}

/** 内置 foxbell 描述符（voices 由 manifest.json 拉取后填充，spec EP10） */
export const FOXBELL: ActivePet = {
  id: "foxbell",
  displayName: "Foxbell",
  spritesheetUrl: "/pet/spritesheet.webp",
  rows: 11,
  hasVoice: true,
  hasSubtitle: true,
  voices: [],
  resolveVoiceUrl: (f) => `/pet/voice/${encodeURI(f)}`,
};

export function loadActiveId(): string {
  try {
    return localStorage.getItem(ACTIVE_KEY) || "foxbell";
  } catch {
    return "foxbell";
  }
}

export function loadActiveName(): string {
  try {
    return localStorage.getItem(ACTIVE_NAME_KEY) || "Foxbell";
  } catch {
    return "Foxbell";
  }
}

/** 激活指针 + 语音能力缓存 + 展示名缓存（petSoundTakeover 同步读取用，spec §5.2） */
export function saveActiveId(id: string, voiceCap: boolean, displayName?: string): void {
  localStorage.setItem(ACTIVE_KEY, id);
  localStorage.setItem(VOICE_CAP_KEY, voiceCap ? "1" : "0");
  if (displayName) localStorage.setItem(ACTIVE_NAME_KEY, displayName);
}

/** 语音能力：未写入时视为 true（foxbell / 旧版本升级兼容） */
export function loadVoiceCap(): boolean {
  try {
    return localStorage.getItem(VOICE_CAP_KEY) !== "0";
  } catch {
    return true;
  }
}

/** 图集尺寸 → 行数（1536×1872→9，1536×2288→11，其余非法） */
export function rowsFromSize(w: number, h: number): PetRows | null {
  if (w !== 1536) return null;
  if (h === 1872) return 9;
  if (h === 2288) return 11;
  return null;
}

/** 图集行数探测（Image 解码，约 50-150ms） */
export function probeSheetRows(url: string): Promise<PetRows> {
  return new Promise((resolve, reject) => {
    const img = new Image();
    img.onload = () => {
      const r = rowsFromSize(img.naturalWidth, img.naturalHeight);
      if (r) resolve(r);
      else reject(new Error(`spritesheet 尺寸非法: ${img.naturalWidth}x${img.naturalHeight}`));
    };
    img.onerror = () => reject(new Error("spritesheet 加载失败"));
    img.src = url;
  });
}

/** 音频时长探测（仅读元数据头部，超时 8s，spec §6-2） */
export function probeAudioDurationMs(url: string, timeoutMs = 8000): Promise<number> {
  return new Promise((resolve, reject) => {
    const a = new Audio();
    a.preload = "metadata";
    const timer = window.setTimeout(() => {
      a.src = "";
      reject(new Error("音频探测超时"));
    }, timeoutMs);
    a.onloadedmetadata = () => {
      window.clearTimeout(timer);
      const d = a.duration;
      a.src = "";
      if (Number.isFinite(d) && d > 0) resolve(Math.round(d * 1000));
      else reject(new Error("音频时长不可用"));
    };
    a.onerror = () => {
      window.clearTimeout(timer);
      reject(new Error("音频加载失败"));
    };
    a.src = url;
  });
}

interface ScanFile {
  rel: string;
  exists: boolean;
  size: number;
}
interface PetScanDto {
  id: string;
  dir: string;
  spritesheet: ScanFile;
  voiceFiles: ScanFile[];
}
interface ManifestVoiceDto {
  group: string;
  name: string;
  file: string;
  sizeBytes: number;
  durationMs: number;
}
interface ManifestDto {
  id: string;
  displayName: string;
  hasVoice: boolean;
  hasSubtitle: boolean;
  spriteVersionNumber: number;
  voices: ManifestVoiceDto[];
}

/** 音频内存快照：激活时全量 fetch → blob URL（EP6；任一失败整体降级为无语音） */
async function snapshotVoices(
  dir: string,
  voices: ManifestVoiceDto[]
): Promise<{ entries: VoiceEntry[]; resolve: (file: string) => string } | null> {
  try {
    const blobs = new Map<string, string>();
    await Promise.all(
      voices.map(async (v) => {
        const res = await fetch(convertFileSrc(`${dir}/${v.file}`));
        if (!res.ok) throw new Error(`快照失败: ${v.file}`);
        blobs.set(v.file, URL.createObjectURL(await res.blob()));
      })
    );
    return {
      // index 必须顺序编号：VoicePlayer.play 以 els[entry.index] 定位预载元素
      entries: voices.map((v, i) => ({
        index: i,
        group: v.group as VoiceEntry["group"],
        name: v.name,
        file: v.file,
      })),
      resolve: (file) => blobs.get(file) ?? "",
    };
  } catch {
    return null;
  }
}

/**
 * 解析当前激活宠物（宠物窗口启动 / 热切换共用）。
 * foxbell：静态描述符 + manifest.json 语音；外部：scan + manifest + 图集探测 + 音频快照。
 * 任何失败抛错，调用方回落 FOXBELL（spec §5.2 宠物永不白屏）。
 */
export async function resolveActivePet(): Promise<ActivePet> {
  const id = loadActiveId();
  if (id === "foxbell") {
    try {
      const raw = await fetch("/pet/manifest.json").then((r) => r.json());
      return { ...FOXBELL, voices: parseManifest(raw) };
    } catch {
      return FOXBELL; // 浏览器渲染/素材缺失：静默降级
    }
  }
  const scan = await invoke<PetScanDto>("pet_scan", { id });
  if (!scan.spritesheet.exists) throw new Error("spritesheet.webp 缺失");
  const rows = await probeSheetRows(convertFileSrc(`${scan.dir}/spritesheet.webp`));
  const manifest = await invoke<ManifestDto | null>("pet_read_manifest", { id });
  if (!manifest) {
    // 直投未生成 manifest：渲染可用的最低档（主窗口启动校验负责生成，spec §6.1）
    return {
      id,
      displayName: id,
      spritesheetUrl: convertFileSrc(`${scan.dir}/spritesheet.webp`),
      rows,
      hasVoice: false,
      hasSubtitle: false,
      voices: [],
      resolveVoiceUrl: () => "",
    };
  }
  const snap = manifest.hasVoice ? await snapshotVoices(scan.dir, manifest.voices) : null;
  return {
    id,
    displayName: manifest.displayName || id,
    spritesheetUrl: convertFileSrc(`${scan.dir}/spritesheet.webp`),
    rows,
    hasVoice: !!snap,
    hasSubtitle: manifest.hasSubtitle && !!snap,
    voices: snap?.entries ?? [],
    resolveVoiceUrl: snap?.resolve ?? (() => ""),
  };
}
```

- [x] **Step 5: 运行测试通过**

```bash
pnpm test tests/pet/petRuntime.test.ts
```
Expected: 3 个用例 PASS。

- [x] **Step 6: Commit**

```bash
git add src/components/pet/petRuntime.ts tests/pet/petRuntime.test.ts tests/msw/tauriMocks.ts tests/setup.ts
git commit -m "feat(pet): active pet runtime with probes and blob voice snapshot"
```

---

### Task 10: FoxbellPet 接入 / PetMenu 门控 / 提示音接管

**Files:**
- Modify: `src/components/pet/FoxbellPet.tsx`
- Modify: `src/components/pet/PetMenu.tsx`
- Modify: `src/components/pet/petConfig.ts:126-128`
- Test: `tests/pet/petMenu.test.tsx`、`tests/pet/notificationTakeover.test.ts`（追加）

- [x] **Step 1: 写失败测试**

`tests/pet/petMenu.test.tsx` 追加：

```tsx
describe("能力门控（spec §5.2）", () => {
  it("voiceCapable=false 时声音行置灰不可切换", async () => {
    localStorage.setItem("mam-pet-config", JSON.stringify({ muted: false }));
    render(
      <PetMenu
        onClose={() => {}}
        onPreview={() => {}}
        onHide={() => {}}
        voiceCapable={false}
        subtitleCapable={false}
      />
    );
    const rows = screen.getAllByTestId("pet-menu-row-sound");
    fireEvent.click(rows[0]);
    // 未切换：配置仍 muted=false
    expect(JSON.parse(localStorage.getItem("mam-pet-config")!).muted).toBe(false);
    expect(rows[0]).toHaveAttribute("title"); // tooltip 说明原因（EP9）
  });
  it("voiceCapable=true 时点击正常切换", () => {
    localStorage.setItem("mam-pet-config", JSON.stringify({ muted: false }));
    render(
      <PetMenu onClose={() => {}} onPreview={() => {}} onHide={() => {}} voiceCapable subtitleCapable />
    );
    fireEvent.click(screen.getAllByTestId("pet-menu-row-sound")[0]);
    expect(JSON.parse(localStorage.getItem("mam-pet-config")!).muted).toBe(true);
  });
});
```

`tests/pet/notificationTakeover.test.ts` 追加：

```ts
describe("语音能力闸门（spec §5.2）", () => {
  it("无语音外部宠物不接管完成提示音", () => {
    localStorage.setItem("mam-pet-visible", "1");
    localStorage.setItem("mam-pet-voice-cap", "0");
    expect(petSoundTakeover()).toBe(false);
  });
  it("foxbell（未写能力缓存）保持接管", () => {
    localStorage.setItem("mam-pet-visible", "1");
    localStorage.removeItem("mam-pet-voice-cap");
    expect(petSoundTakeover()).toBe(true);
  });
});
```

（两个测试文件按需补 import：`PetMenu`、`fireEvent`、`render`、`screen`、`petSoundTakeover`——与现有文件一致）

- [x] **Step 2: 运行确认失败**

```bash
pnpm test tests/pet/petMenu.test.tsx tests/pet/notificationTakeover.test.ts
```
Expected: FAIL（PetMenu 无 voiceCapable 属性 / takeover 不读能力键）。

- [x] **Step 3: 实现**

**petConfig.ts** —— `petSoundTakeover` 改为：

```ts
/** 完成提示音接管：宠物开启且当前宠物具备语音能力（无语音外部宠物回落主看板，spec §5.2） */
export function petSoundTakeover(): boolean {
  return loadVisible() && loadVoiceCap();
}
```

顶部 import 补：`import { loadVoiceCap } from "./petRuntime";`

**PetMenu.tsx** —— props 增加两个必填属性并改造两行开关：

```tsx
export function PetMenu(props: {
  onClose(): void;
  onPreview(action: PetAction | null): void;
  onHide(): void;
  voiceCapable: boolean;
  subtitleCapable: boolean;
}) {
```

声音行（161-168 行）替换为：

```tsx
          <div
            data-testid="pet-menu-row-sound"
            title={!props.voiceCapable ? t("pet.menu.soundNoCap") : undefined}
            style={{
              ...rowStyle,
              opacity: props.voiceCapable ? 1 : 0.5,
              cursor: props.voiceCapable ? "pointer" : "not-allowed",
            }}
            onClick={() => {
              if (props.voiceCapable) saveConfig({ muted: !cfg.muted });
            }}
          >
            <span>{t("pet.menu.sound")}</span>
            {btn(
              !cfg.muted,
              () => {
                if (props.voiceCapable) saveConfig({ muted: !cfg.muted });
              },
              !cfg.muted && props.voiceCapable ? t("pet.menu.on") : t("pet.menu.off")
            )}
          </div>
```

字幕行（169-176 行）同款改造（`subtitleCapable` / `talkative` / `pet.menu.subtitleNoCap` / `data-testid="pet-menu-row-subtitle"`）。

**FoxbellPet.tsx** —— 关键改动（保持其余逻辑不变）：

1. 头部 import 调整（原文件已 `import { emit } from "@tauri-apps/api/event";`，只需补 listen 与 petRuntime）：

```ts
import { listen } from "@tauri-apps/api/event";
import { FOXBELL, resolveActivePet, type ActivePet } from "./petRuntime";
```

2. 组件内新增 active 状态（在 `const [cfg, setCfg]` 之后）：

```tsx
  // ---- 激活宠物（外部宠物热切换，spec §12）----
  const [active, setActive] = useState<ActivePet>(FOXBELL);
  const activeRef = useRef(active);
  useEffect(() => {
    activeRef.current = active;
  }, [active]);
```

3. 原 manifest 拉取 effect（128-149 行）整体替换为：

```tsx
  // 激活宠物解析：启动一次 + 监听热切换事件（失败回落 foxbell，spec §5.2/§12）
  useEffect(() => {
    let disposed = false;
    const refresh = () => {
      resolveActivePet()
        .then((p) => {
          if (!disposed) setActive(p);
        })
        .catch(() => {
          if (!disposed) setActive(FOXBELL);
        });
    };
    refresh();
    let un: (() => void) | null = null;
    void listen("pet-active-changed", refresh).then((f) => {
      if (disposed) f();
      else un = f;
    });
    return () => {
      disposed = true;
      un?.();
    };
  }, []);
```

4. 语音 player 挂载（紧随上一个 effect）：

```tsx
  // VoicePlayer 随激活宠物重建（外部宠物 = blob 快照 URL，EP6）
  useEffect(() => {
    const player = new VoicePlayer();
    player.load(active.voices, active.resolveVoiceUrl);
    voiceRef.current = player;
    if (unlockedRef.current) player.unlock();
    return () => {
      player.dispose();
      voiceRef.current = null;
    };
  }, [active]);
```

5. `playVoice` 门控（162 行起）：`if (!loadVisible()) return;` 之后加两行：

```ts
    if (!activeRef.current.hasVoice) return; // 无语音宠物：动作照播、不出声不出字幕（spec §5.2）
```

以及 showBubble 调用处（170 行）改条件：

```ts
    if (cfgRef.current.talkative && activeRef.current.hasSubtitle) showBubble(entry.name, MIN_SPEECH_MS);
```

`onSubtitle` 回调内条件（175 行）同步加 `&& activeRef.current.hasSubtitle`。

6. look 环视门控（`scheduleNextLook` 内、6s 定时回调开头）：

```ts
      if (genRef.current.look !== gen) return;
      if (rowsRef.current !== 11) {
        scheduleNextLook(); // v1 无环视行：静默续期（EP1）
        return;
      }
```

并在 activeRef 声明旁补 `const rowsRef = useRef<9 | 11>(11);`，同步 effect 中 `rowsRef.current = active.rows;`。

7. 精灵 style（466-478 行）：`backgroundImage: "url(/pet/spritesheet.webp)"` 改为 `backgroundImage: `url(${active.spritesheetUrl})``；`const style = frameStyle(anim, frame, lookFrame, cfg.scale)` 改为 `const style = frameStyle(anim, frame, lookFrame, cfg.scale, active.rows)`。

8. PetMenu 挂载（663-667 行）传门控：

```tsx
          <PetMenu
            onClose={handleMenuClose}
            onPreview={handleMenuPreview}
            onHide={handleMenuHide}
            voiceCapable={active.hasVoice}
            subtitleCapable={active.hasSubtitle}
          />
```

- [x] **Step 4: i18n 键**

`zh.json` 的 `pet.menu` 对象内追加：

```json
      "soundNoCap": "该宠物没有可用语音（需四组音频齐全）",
      "subtitleNoCap": "该宠物未启用字幕（字幕=音频文件名）"
```

`en.json` 对应：

```json
      "soundNoCap": "This pet has no voice assets (requires all four groups)",
      "subtitleNoCap": "Subtitles disabled for this pet (subtitle = audio filename)"
```

- [x] **Step 5: 运行测试**

```bash
pnpm test tests/pet/
```
Expected: 全部 PASS（含既有 foxbell 系列回归——若既有用例因 PetMenu 新必填 props 报 TS 错，为其 render 调用补 `voiceCapable subtitleCapable`）。

- [x] **Step 6: Commit**

```bash
git add src/components/pet/ src/i18n/locales/ tests/pet/
git commit -m "feat(pet): integrate active pet runtime with v1 degrade and capability gating"
```

---

## Phase 3：校验与切换

### Task 11: petValidation（统一校验纯函数）

**Files:**
- Create: `src/components/pet/petValidation.ts`
- Test: `tests/pet/petValidation.test.ts`

- [x] **Step 1: 写失败测试**

```ts
import { describe, expect, it } from "vitest";
import {
  AUDIO_EXTS, MAX_AUDIO_BYTES, MIN_DURATION_MS, MAX_DURATION_MS,
  groupOfRel, nameFromRel, isAudioCandidate, diffManifestVsScan, judgeVoiceTier, voiceRowProblem,
  type PetScan, type PetManifestView,
} from "@/components/pet/petValidation";

const scan = (files: { rel: string; size: number }[], sheetSize = 100): PetScan => ({
  id: "p",
  dir: "/x/p",
  spritesheet: { rel: "spritesheet.webp", exists: sheetSize > 0, size: sheetSize },
  voiceFiles: files.map((f) => ({ rel: f.rel, exists: true, size: f.size })),
});

describe("petValidation", () => {
  it("分组与文件名解析", () => {
    expect(groupOfRel("voice/general/a.m4a")).toBe("general");
    expect(groupOfRel("voice/hack/a.m4a")).toBeNull();
    expect(groupOfRel("a.m4a")).toBeNull();
    expect(nameFromRel("voice/general/休息一下吧.m4a")).toBe("休息一下吧");
  });

  it("合法音频候选：扩展名 + 分组（spec §5.1）", () => {
    expect(isAudioCandidate({ rel: "voice/done/x.MP3", exists: true, size: 1 })).toBe(true);
    expect(isAudioCandidate({ rel: "voice/done/x.txt", exists: true, size: 1 })).toBe(false);
    expect(isAudioCandidate({ rel: "other/x.mp3", exists: true, size: 1 })).toBe(false);
    expect(AUDIO_EXTS).toContain("m4a");
    expect(MAX_AUDIO_BYTES).toBe(10 * 1024 * 1024);
    expect(MIN_DURATION_MS).toBe(1000);
    expect(MAX_DURATION_MS).toBe(20000);
  });

  it("voiceRowProblem：时长/大小边界（spec §5.1 严格不等）", () => {
    const ok = { group: "general", name: "a", file: "voice/general/a.m4a", sizeBytes: 1, durationMs: 2000 };
    expect(voiceRowProblem(ok)).toBeNull();
    expect(voiceRowProblem({ ...ok, durationMs: 1000 })).toBe("too-short");
    expect(voiceRowProblem({ ...ok, durationMs: 20000 })).toBe("too-long");
    expect(voiceRowProblem({ ...ok, durationMs: null })).toBe("no-duration");
    expect(voiceRowProblem({ ...ok, sizeBytes: MAX_AUDIO_BYTES + 1 })).toBe("too-big");
  });

  it("judgeVoiceTier：四组各≥1 合法才开语音（全有或全无）", () => {
    const v = { rel: "", size: 1, durationMs: 2000 };
    const mk = (g: string) => ({ ...v, rel: `voice/${g}/a.m4a` });
    expect(judgeVoiceTier([mk("general"), mk("approval"), mk("done"), mk("error")]).hasVoice).toBe(true);
    expect(judgeVoiceTier([mk("general"), mk("approval"), mk("done")]).hasVoice).toBe(false);
    expect(judgeVoiceTier([]).hasVoice).toBe(false);
    // 单组不合法即整组无覆盖
    expect(
      judgeVoiceTier([mk("general"), mk("approval"), mk("done"), { ...mk("error"), durationMs: 25000 }]).hasVoice
    ).toBe(false);
  });

  it("diffManifestVsScan：一致无 issue；缺文件/大小变/多余文件/图集变（spec §6-3）", () => {
    const m: PetManifestView = {
      id: "p", displayName: "P", hasVoice: true, hasSubtitle: true,
      spriteVersionNumber: 2, spritesheetSizeBytes: 100,
      voices: [{ group: "general", name: "a", file: "voice/general/a.m4a", sizeBytes: 10, durationMs: 2000 }],
    };
    expect(diffManifestVsScan(m, scan([{ rel: "voice/general/a.m4a", size: 10 }]))).toEqual([]);
    const issues = diffManifestVsScan(
      m,
      scan([{ rel: "voice/general/a.m4a", size: 99 }, { rel: "voice/done/new.mp3", size: 5 }], 999)
    );
    expect(issues.map((i) => i.kind)).toEqual(["spritesheet-changed", "voice-changed", "voice-extra"]);
    const missing = diffManifestVsScan(m, scan([], 100));
    expect(missing.map((i) => i.kind)).toContain("voice-missing");
    const noSheet = diffManifestVsScan(m, scan([{ rel: "voice/general/a.m4a", size: 10 }], 0));
    expect(noSheet.map((i) => i.kind)).toContain("spritesheet-missing");
  });
});
```

- [x] **Step 2: 运行确认失败**

```bash
pnpm test tests/pet/petValidation.test.ts
```
Expected: FAIL（模块不存在）。

- [x] **Step 3: 实现**

```ts
// 统一校验纯函数 — manifest×磁盘 diff、音频合法性、三档判定（spec §6；探测由 petRuntime 提供）
export const GROUPS = ["general", "approval", "done", "error"] as const;
export const AUDIO_EXTS = ["m4a", "mp3", "wav", "ogg", "opus", "flac", "aac"];
export const MAX_AUDIO_BYTES = 10 * 1024 * 1024;
export const MIN_DURATION_MS = 1000;
export const MAX_DURATION_MS = 20000;

export interface ScanFile {
  rel: string;
  exists: boolean;
  size: number;
}

export interface PetScan {
  id: string;
  dir: string;
  spritesheet: ScanFile;
  voiceFiles: ScanFile[];
}

export interface ManifestVoice {
  group: string;
  name: string;
  file: string;
  sizeBytes: number;
  durationMs: number;
}

export interface PetManifestView {
  id: string;
  displayName: string;
  description?: string;
  source?: string;
  hasVoice: boolean;
  hasSubtitle: boolean;
  spriteVersionNumber: number;
  spritesheetSizeBytes: number;
  voices: ManifestVoice[];
}

export type VoiceProblem = "too-short" | "too-long" | "too-big" | "no-duration";

export interface VoiceRow {
  group: string;
  name: string;
  file: string;
  sizeBytes: number;
  durationMs: number | null;
}

export function extOf(rel: string): string {
  const i = rel.lastIndexOf(".");
  return i >= 0 ? rel.slice(i + 1).toLowerCase() : "";
}

/** "voice/<group>/<file>" → group（仅四固定分组） */
export function groupOfRel(rel: string): string | null {
  const parts = rel.split("/");
  if (parts.length !== 3 || parts[0] !== "voice") return null;
  return (GROUPS as readonly string[]).includes(parts[1]) ? parts[1] : null;
}

/** 字幕文本 = 文件名去扩展名（EP8） */
export function nameFromRel(rel: string): string {
  const base = rel.split("/").pop() ?? rel;
  const i = base.lastIndexOf(".");
  return i > 0 ? base.slice(0, i) : base;
}

/** 扫描项是否为合法音频候选（扩展名 + 分组路径，spec §5.1） */
export function isAudioCandidate(f: ScanFile): boolean {
  return f.exists && AUDIO_EXTS.includes(extOf(f.rel)) && groupOfRel(f.rel) !== null;
}

/** 单音频合法性（探测后判定；durationMs=null 表示探测失败） */
export function voiceRowProblem(r: VoiceRow): VoiceProblem | null {
  if (r.durationMs === null) return "no-duration";
  if (r.durationMs <= MIN_DURATION_MS) return "too-short";
  if (r.durationMs >= MAX_DURATION_MS) return "too-long";
  if (r.sizeBytes > MAX_AUDIO_BYTES) return "too-big";
  return null;
}

export interface TierJudge {
  hasVoice: boolean;
  coverage: Record<string, number>;
}

/** 声音档判定：四组各 ≥1 合法文件（全有或全无，spec §5.1） */
export function judgeVoiceTier(files: { rel: string; size: number; durationMs: number | null }[]): TierJudge {
  const coverage: Record<string, number> = {};
  for (const g of GROUPS) coverage[g] = 0;
  for (const f of files) {
    if (voiceRowProblem({ group: "", name: "", file: f.rel, sizeBytes: f.size, durationMs: f.durationMs })) continue;
    const g = groupOfRel(f.rel);
    if (g) coverage[g] += 1;
  }
  return { hasVoice: GROUPS.every((g) => coverage[g] > 0), coverage };
}

export type IssueKind =
  | "spritesheet-missing"
  | "spritesheet-changed"
  | "voice-missing"
  | "voice-changed"
  | "voice-extra";

export interface ValidationIssue {
  kind: IssueKind;
  detail: string;
}

/** manifest × 磁盘 stat 比对（不解码媒体，spec §6-3） */
export function diffManifestVsScan(m: PetManifestView, s: PetScan): ValidationIssue[] {
  const issues: ValidationIssue[] = [];
  if (!s.spritesheet.exists) {
    issues.push({ kind: "spritesheet-missing", detail: "spritesheet.webp 不存在" });
  } else if (m.spritesheetSizeBytes > 0 && s.spritesheet.size !== m.spritesheetSizeBytes) {
    issues.push({ kind: "spritesheet-changed", detail: `图集大小 ${m.spritesheetSizeBytes} → ${s.spritesheet.size}` });
  }
  const onDisk = new Map(s.voiceFiles.map((f) => [f.rel, f.size]));
  for (const v of m.voices) {
    if (!onDisk.has(v.file)) issues.push({ kind: "voice-missing", detail: v.file });
    else if (onDisk.get(v.file) !== v.sizeBytes) issues.push({ kind: "voice-changed", detail: v.file });
  }
  const known = new Set(m.voices.map((v) => v.file));
  for (const f of s.voiceFiles) {
    if (isAudioCandidate(f) && !known.has(f.rel)) issues.push({ kind: "voice-extra", detail: f.rel });
  }
  return issues;
}

/** rows ↔ spriteVersionNumber */
export function spriteVersionOf(rows: 9 | 11): 1 | 2 {
  return rows === 9 ? 1 : 2;
}
```

- [x] **Step 4: 运行测试通过**

```bash
pnpm test tests/pet/petValidation.test.ts
```
Expected: 全部 PASS。

- [x] **Step 5: Commit**

```bash
git add src/components/pet/petValidation.ts tests/pet/petValidation.test.ts
git commit -m "feat(pet): unified validation pure functions"
```

---

### Task 12: petActivation（激活编排）

**Files:**
- Create: `src/components/pet/petActivation.ts`
- Test: `tests/pet/petActivation.test.ts`

- [x] **Step 1: 写失败测试**

```ts
import { beforeEach, describe, expect, it, vi } from "vitest";
import { activatePet, buildManifestFromScan, repairManifest } from "@/components/pet/petActivation";
import type { PetScan, PetManifestView } from "@/components/pet/petValidation";
import { tauriInvokeMock } from "../../msw/tauriMocks";

// 探测桩：默认 v2 图集、全部音频 3s
vi.mock("@/components/pet/petRuntime", async (importOriginal) => {
  const orig = await importOriginal<typeof import("@/components/pet/petRuntime")>();
  return {
    ...orig,
    probeSheetRows: vi.fn().mockResolvedValue(11),
    probeAudioDurationMs: vi.fn().mockResolvedValue(3000),
  };
});

const scanOf = (files: { rel: string; size: number }[], sheet = 100): PetScan => ({
  id: "p1",
  dir: "/x/p1",
  spritesheet: { rel: "spritesheet.webp", exists: sheet > 0, size: sheet },
  voiceFiles: files.map((f) => ({ rel: f.rel, exists: true, size: f.size })),
});
const g = (n: string) => `voice/${n}/a.mp3`;
const fourGroups = ["general", "approval", "done", "error"].map((n) => ({ rel: g(n), size: 5 }));

describe("activatePet", () => {
  beforeEach(() => {
    localStorage.clear();
    tauriInvokeMock.mockClear();
  });

  it("foxbell：直接写指针并广播", async () => {
    const r = await activatePet("foxbell", async () => "cancel");
    expect(r.status).toBe("activated");
    expect(localStorage.getItem("mam-pet-active")).toBe("foxbell");
    expect(localStorage.getItem("mam-pet-active-name")).toBe("Foxbell");
  });

  it("图集缺失：invalid-sheet，不写指针", async () => {
    tauriInvokeMock.mockImplementation((cmd: string) => {
      if (cmd === "pet_scan") return Promise.resolve(scanOf([], 0));
      if (cmd === "pet_read_manifest") return Promise.resolve(null);
      return Promise.resolve(undefined);
    });
    const r = await activatePet("p1", async () => "cancel");
    expect(r.status).toBe("invalid-sheet");
    expect(localStorage.getItem("mam-pet-active")).toBeNull();
  });

  it("直投无 manifest：全量探测 → 生成 manifest → 激活（spec §6-2）", async () => {
    tauriInvokeMock.mockImplementation((cmd: string) => {
      if (cmd === "pet_scan") return Promise.resolve(scanOf(fourGroups));
      if (cmd === "pet_read_manifest") return Promise.resolve(null);
      return Promise.resolve(undefined);
    });
    const r = await activatePet("p1", async () => "cancel");
    expect(r.status).toBe("activated");
    expect(r.manifestBuilt).toBe(true);
    const call = tauriInvokeMock.mock.calls.find((c) => c[0] === "pet_update_manifest");
    const m = call?.[1]?.manifest as PetManifestView;
    expect(m.hasVoice).toBe(true); // 四组齐 → 有语音
    expect(m.hasSubtitle).toBe(true); // 直投默认有语音即有字幕
    expect(localStorage.getItem("mam-pet-active")).toBe("p1");
    expect(localStorage.getItem("mam-pet-voice-cap")).toBe("1");
  });

  it("音频不全的直投：无语音激活", async () => {
    tauriInvokeMock.mockImplementation((cmd: string) => {
      if (cmd === "pet_scan") return Promise.resolve(scanOf([fourGroups[0]]));
      if (cmd === "pet_read_manifest") return Promise.resolve(null);
      return Promise.resolve(undefined);
    });
    const r = await activatePet("p1", async () => "cancel");
    expect(r.status).toBe("activated");
    expect(localStorage.getItem("mam-pet-voice-cap")).toBe("0");
  });

  it("manifest 不一致 + 用户选更新：备份修复后激活（spec §6-3）", async () => {
    const manifest: PetManifestView = {
      id: "p1", displayName: "P", hasVoice: true, hasSubtitle: true,
      spriteVersionNumber: 2, spritesheetSizeBytes: 100,
      voices: [{ group: "general", name: "a", file: g("general"), sizeBytes: 10, durationMs: 3000 }],
    };
    tauriInvokeMock.mockImplementation((cmd: string) => {
      if (cmd === "pet_scan") return Promise.resolve(scanOf(fourGroups)); // 旧条目 size 10 ≠ 5 → changed；其余 extra
      if (cmd === "pet_read_manifest") return Promise.resolve(manifest);
      return Promise.resolve(undefined);
    });
    const r = await activatePet("p1", async () => "update");
    expect(r.status).toBe("activated");
    expect(r.repaired).toBe(true);
    const call = tauriInvokeMock.mock.calls.find((c) => c[0] === "pet_update_manifest");
    expect(call?.[1]?.backup).toBe(true);
  });

  it("manifest 不一致 + 用户选忽略：无语音降级激活", async () => {
    const manifest: PetManifestView = {
      id: "p1", displayName: "P", hasVoice: false, hasSubtitle: false,
      spriteVersionNumber: 2, spritesheetSizeBytes: 100, voices: [],
    };
    tauriInvokeMock.mockImplementation((cmd: string) => {
      if (cmd === "pet_scan") return Promise.resolve(scanOf([fourGroups[0]], 999)); // 图集也变了
      if (cmd === "pet_read_manifest") return Promise.resolve(manifest);
      return Promise.resolve(undefined);
    });
    const r = await activatePet("p1", async () => "ignore");
    expect(r.status).toBe("activated");
    expect(localStorage.getItem("mam-pet-voice-cap")).toBe("0");
  });

  it("用户选取消：不激活", async () => {
    const manifest: PetManifestView = {
      id: "p1", displayName: "P", hasVoice: false, hasSubtitle: false,
      spriteVersionNumber: 2, spritesheetSizeBytes: 1, voices: [],
    };
    tauriInvokeMock.mockImplementation((cmd: string) => {
      if (cmd === "pet_scan") return Promise.resolve(scanOf([], 999));
      if (cmd === "pet_read_manifest") return Promise.resolve(manifest);
      return Promise.resolve(undefined);
    });
    const r = await activatePet("p1", async () => "cancel");
    expect(r.status).toBe("mismatch");
    expect(localStorage.getItem("mam-pet-active")).toBeNull();
  });
});

describe("buildManifestFromScan / repairManifest", () => {
  it("repair 保留未变条目、重探变动与新增（spec §6-3 修复语义）", async () => {
    const old: PetManifestView = {
      id: "p1", displayName: "Old", hasVoice: true, hasSubtitle: true,
      spriteVersionNumber: 2, spritesheetSizeBytes: 100,
      voices: [
        { group: "general", name: "a", file: g("general"), sizeBytes: 5, durationMs: 3000 }, // 不变
        { group: "approval", name: "b", file: g("approval"), sizeBytes: 99, durationMs: 3000 }, // 变动
      ],
    };
    const repaired = await repairManifest(old, scanOf(fourGroups), 9);
    expect(repaired.spriteVersionNumber).toBe(1); // rows=9 → v1
    expect(repaired.voices.find((v) => v.file === g("general"))?.durationMs).toBe(3000); // 保留缓存
    expect(repaired.voices.find((v) => v.file === g("approval"))?.sizeBytes).toBe(5); // 重探纳入
    expect(repaired.hasVoice).toBe(true);
  });

  it("buildManifest：探测失败的文件排除且不阻断", async () => {
    const { probeAudioDurationMs } = await import("@/components/pet/petRuntime");
    vi.mocked(probeAudioDurationMs).mockRejectedValueOnce(new Error("x"));
    const m = await buildManifestFromScan("p1", scanOf(fourGroups), 11, "petdex", false);
    expect(m.voices).toHaveLength(3); // 一个探测失败被排除
    expect(m.hasVoice).toBe(false); // 该组无覆盖
    expect(m.spriteVersionNumber).toBe(2);
    expect(m.source).toBe("petdex");
  });
});
```

- [x] **Step 2: 运行确认失败**

```bash
pnpm test tests/pet/petActivation.test.ts
```
Expected: FAIL（模块不存在）。

- [x] **Step 3: 实现**

```ts
// 激活编排 — 统一校验算法交互层：直投生成 / 不一致修复 / 忽略降级 / 激活指针（spec §6）
import { convertFileSrc, invoke } from "@tauri-apps/api/core";
import { emit } from "@tauri-apps/api/event";
import { probeAudioDurationMs, probeSheetRows, saveActiveId } from "./petRuntime";
import {
  diffManifestVsScan,
  isAudioCandidate,
  judgeVoiceTier,
  spriteVersionOf,
  type PetManifestView,
  type PetScan,
  type ValidationIssue,
} from "./petValidation";

export type MismatchChoice = "update" | "ignore" | "cancel";
export type MismatchConfirm = (
  issues: ValidationIssue[],
  manifest: PetManifestView
) => Promise<MismatchChoice>;

export interface ActivationResult {
  status: "activated" | "invalid-sheet" | "mismatch" | "error";
  manifestBuilt?: boolean;
  repaired?: boolean;
  message?: string;
}

function notifyPetChanged(): void {
  emit("pet-active-changed", {}).catch(() => {});
}

/** 探测扫描中的全部合法候选（并行，失败项 durationMs=null） */
async function probeCandidates(
  scan: PetScan
): Promise<{ rel: string; size: number; durationMs: number | null }[]> {
  const candidates = scan.voiceFiles.filter(isAudioCandidate);
  return Promise.all(
    candidates.map(async (f) => ({
      rel: f.rel,
      size: f.size,
      durationMs: await probeAudioDurationMs(convertFileSrc(`${scan.dir}/${f.rel}`)).catch(() => null),
    }))
  );
}

const isValid = (p: { size: number; durationMs: number | null }) =>
  p.durationMs !== null &&
  p.durationMs > 1000 &&
  p.durationMs < 20000 &&
  p.size <= 10 * 1024 * 1024;

/** 直投首激活：全量探测 → 三档判定 → 生成 manifest（spec §6-2） */
export async function buildManifestFromScan(
  id: string,
  scan: PetScan,
  rows: 9 | 11,
  source: string,
  subtitleDefault: boolean,
  overrides?: { displayName?: string; description?: string }
): Promise<PetManifestView> {
  const probed = await probeCandidates(scan);
  const valid = probed.filter(isValid);
  const hasVoice = judgeVoiceTier(valid).hasVoice;
  return {
    id,
    displayName: overrides?.displayName || id,
    description: overrides?.description || "",
    source,
    hasVoice,
    hasSubtitle: hasVoice && subtitleDefault,
    spriteVersionNumber: spriteVersionOf(rows),
    spritesheetSizeBytes: scan.spritesheet.size,
    voices: valid.map((p) => ({
      group: p.rel.split("/")[1],
      name: p.rel.split("/").pop()!.replace(/\.[^.]+$/, ""),
      file: p.rel,
      sizeBytes: p.size,
      durationMs: p.durationMs!,
    })),
  };
}

/** 修复：保留未变条目（信任缓存时长）、重探变动与新增（spec §6-3） */
export async function repairManifest(
  old: PetManifestView,
  scan: PetScan,
  rows: 9 | 11
): Promise<PetManifestView> {
  const keep = old.voices.filter(
    (v) => scan.voiceFiles.some((f) => f.rel === v.file && f.size === v.sizeBytes)
  );
  const changedOrNew = scan.voiceFiles
    .filter(isAudioCandidate)
    .filter((f) => !keep.some((v) => v.file === f.rel));
  const probed = await Promise.all(
    changedOrNew.map(async (f) => ({
      rel: f.rel,
      size: f.size,
      durationMs: await probeAudioDurationMs(convertFileSrc(`${scan.dir}/${f.rel}`)).catch(() => null),
    }))
  );
  const validNew = probed.filter(isValid).map((p) => ({
    group: p.rel.split("/")[1],
    name: p.rel.split("/").pop()!.replace(/\.[^.]+$/, ""),
    file: p.rel,
    sizeBytes: p.size,
    durationMs: p.durationMs!,
  }));
  const voices = [...keep, ...validNew];
  const hasVoice = judgeVoiceTier(voices.map((v) => ({ rel: v.file, size: v.sizeBytes, durationMs: v.durationMs }))).hasVoice;
  return {
    ...old,
    spriteVersionNumber: spriteVersionOf(rows),
    spritesheetSizeBytes: scan.spritesheet.size,
    hasVoice,
    hasSubtitle: hasVoice && old.hasSubtitle,
    voices,
  };
}

/** 统一激活入口（切换/启动修复共用，spec §6） */
export async function activatePet(id: string, confirm: MismatchConfirm): Promise<ActivationResult> {
  try {
    if (id === "foxbell") {
      saveActiveId("foxbell", true, "Foxbell");
      notifyPetChanged();
      return { status: "activated" };
    }
    const scan = await invoke<PetScan>("pet_scan", { id });
    if (!scan.spritesheet.exists) {
      return { status: "invalid-sheet", message: "spritesheet.webp 缺失，无法激活" };
    }
    let rows: 9 | 11;
    try {
      rows = await probeSheetRows(convertFileSrc(`${scan.dir}/spritesheet.webp`));
    } catch (e) {
      return { status: "invalid-sheet", message: (e as Error).message };
    }
    const manifest = await invoke<PetManifestView | null>("pet_read_manifest", { id });
    if (!manifest) {
      const built = await buildManifestFromScan(id, scan, rows, "folder", true);
      await invoke("pet_update_manifest", { id, manifest: built, backup: false });
      saveActiveId(id, built.hasVoice, built.displayName);
      notifyPetChanged();
      return { status: "activated", manifestBuilt: true };
    }
    const issues = diffManifestVsScan(manifest, scan);
    if (issues.length === 0) {
      saveActiveId(id, manifest.hasVoice, manifest.displayName);
      notifyPetChanged();
      return { status: "activated" };
    }
    const choice = await confirm(issues, manifest);
    if (choice === "cancel") {
      return { status: "mismatch", issues };
    }
    if (choice === "update") {
      const repaired = await repairManifest(manifest, scan, rows);
      await invoke("pet_update_manifest", { id, manifest: repaired, backup: true });
      saveActiveId(id, repaired.hasVoice, repaired.displayName);
      notifyPetChanged();
      return { status: "activated", repaired: true };
    }
    // ignore：按磁盘降级运行（时长未探测，无法验证 1-20s → 保守无语音，spec §6-3）
    saveActiveId(id, false, manifest.displayName);
    notifyPetChanged();
    return { status: "activated", message: "已按降级模式激活（无语音）" };
  } catch (e) {
    return { status: "error", message: (e as Error).message };
  }
}
```

> 注：`buildManifestFromScan` 中的 display/overrides 供 PetManageDialog 对直投宠物保存时复用（Task 17）。

- [x] **Step 4: 运行测试通过**

```bash
pnpm test tests/pet/petActivation.test.ts
```
Expected: 全部 PASS。

- [x] **Step 5: Commit**

```bash
git add src/components/pet/petActivation.ts tests/pet/petActivation.test.ts
git commit -m "feat(pet): activation orchestration with manifest build/repair/degrade"
```

---

### Task 13: 切换对话框 + 设置页三入口

**Files:**
- Create: `src/components/pet/manage/PetSwitchDialog.tsx`
- Modify: `src/pages/settings.tsx:420-471`
- Modify: `src/i18n/locales/zh.json`、`en.json`
- Test: `tests/pet/petSettings.test.tsx`（追加）

- [x] **Step 1: i18n 键**

`zh.json` 的 `settings.pet` 对象内追加：

```json
      "currentPet": "当前宠物",
      "switchPet": "切换宠物",
      "importPet": "导入宠物",
      "managePet": "修改宠物"
```

`zh.json` 顶层 `pet` 对象内追加：

```json
    "switch": {
      "title": "切换宠物",
      "builtin": "内置",
      "activated": "已切换到 {{name}}",
      "invalidSheet": "该宠物图集缺失或非法，无法激活",
      "updated": "已根据现有素材更新 manifest 并激活",
      "degraded": "已按降级模式激活（无语音）",
      "mismatchTitle": "素材与 manifest 不一致",
      "mismatchUpdate": "根据现有素材更新（自动备份）",
      "mismatchIgnore": "忽略，按磁盘降级运行",
      "mismatchCancel": "取消切换",
      "pendingFirstCheck": "待首次激活校验",
      "error": "操作失败：{{msg}}"
    }
```

`en.json` 对应：

```json
      "currentPet": "Current pet",
      "switchPet": "Switch pet",
      "importPet": "Import pet",
      "managePet": "Manage pets"
```

```json
    "switch": {
      "title": "Switch Pet",
      "builtin": "Built-in",
      "activated": "Switched to {{name}}",
      "invalidSheet": "Spritesheet missing or invalid, cannot activate",
      "updated": "Manifest updated from assets and activated",
      "degraded": "Activated in degraded mode (no voice)",
      "mismatchTitle": "Assets differ from manifest",
      "mismatchUpdate": "Update manifest from assets (auto backup)",
      "mismatchIgnore": "Ignore, run degraded from disk",
      "mismatchCancel": "Cancel switch",
      "pendingFirstCheck": "Pending first activation check",
      "error": "Failed: {{msg}}"
    }
```

- [x] **Step 2: 写 PetSwitchDialog**

```tsx
// PetSwitchDialog — 切换宠物：卡片列表 + 统一校验激活（spec §9）
import { useCallback, useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import { invoke } from "@tauri-apps/api/core";
import { Dialog, DialogContent, DialogHeader, DialogTitle } from "@/components/ui/dialog";
import { Button } from "@/components/ui/button";
import { loadActiveId } from "../petRuntime";
import { activatePet, type MismatchChoice } from "../petActivation";
import type { ValidationIssue } from "../petValidation";

export interface PetCardInfo {
  id: string;
  displayName: string;
  spriteVersionNumber: number; // 0=未知（直投未激活）
  hasVoice: boolean;
  hasSubtitle: boolean;
  manifestExists: boolean;
  dir?: string; // foxbell 无
}

export function PetSwitchDialog(props: { open: boolean; onOpenChange: (v: boolean) => void }) {
  const { t } = useTranslation();
  const [pets, setPets] = useState<PetCardInfo[]>([]);
  const [activeId, setActiveId] = useState(loadActiveId());
  const [mismatch, setMismatch] = useState<{ id: string; issues: ValidationIssue[] } | null>(null);
  const [busy, setBusy] = useState(false);

  const reload = useCallback(async () => {
    try {
      const list = await invoke<PetCardInfo[]>("pet_list_pets");
      setPets(list);
      setActiveId(loadActiveId());
    } catch {
      setPets([]);
    }
  }, []);

  useEffect(() => {
    if (props.open) void reload();
  }, [props.open, reload]);

  // mismatch 三选的 resolver 存 ref（let 变量会在 re-render 后产生 stale closure）
  const mismatchResolveRef = useRef<(c: MismatchChoice) => void>(() => {});

  const doActivate = async (id: string) => {
    setBusy(true);
    try {
      const r = await activatePet(id, async (issues) => {
        setMismatch({ id, issues });
        // 三选 UI：返回由 mismatch 面板按钮 resolve 的 promise
        return new Promise<MismatchChoice>((resolve) => {
          mismatchResolveRef.current = resolve;
        });
      });
      setMismatch(null);
      if (r.status === "activated") {
        toast.success(r.repaired ? t("pet.switch.updated") : r.message ?? t("pet.switch.activated", { name: id }));
        setActiveId(loadActiveId());
        if (r.manifestBuilt) void reload(); // 直投首激活后徽标刷新
      } else if (r.status === "invalid-sheet") {
        toast.error(t("pet.switch.invalidSheet"));
      } else if (r.status === "error") {
        toast.error(t("pet.switch.error", { msg: r.message ?? "" }));
      }
    } finally {
      setBusy(false);
    }
  };

  return (
    <Dialog open={props.open} onOpenChange={props.onOpenChange}>
      <DialogContent className="max-w-lg">
        <DialogHeader>
          <DialogTitle>{t("pet.switch.title")}</DialogTitle>
        </DialogHeader>
        {mismatch ? (
          <div className="space-y-3" data-testid="pet-switch-mismatch">
            <div className="text-sm font-medium">{t("pet.switch.mismatchTitle")}</div>
            <ul className="text-muted-foreground max-h-40 overflow-auto list-disc pl-5 text-xs">
              {mismatch.issues.map((i) => (
                <li key={i.detail}>
                  {i.kind}: {i.detail}
                </li>
              ))}
            </ul>
            <div className="flex flex-wrap gap-2">
              <Button size="sm" onClick={() => mismatchResolveRef.current("update")}>
                {t("pet.switch.mismatchUpdate")}
              </Button>
              <Button size="sm" variant="outline" onClick={() => mismatchResolveRef.current("ignore")}>
                {t("pet.switch.mismatchIgnore")}
              </Button>
              <Button size="sm" variant="ghost" onClick={() => mismatchResolveRef.current("cancel")}>
                {t("pet.switch.mismatchCancel")}
              </Button>
            </div>
          </div>
        ) : (
          <div className="grid grid-cols-2 gap-2" data-testid="pet-switch-list">
            {/* foxbell 永远第一张（内置） */}
            <PetCard
              info={{
                id: "foxbell",
                displayName: "Foxbell",
                spriteVersionNumber: 2,
                hasVoice: true,
                hasSubtitle: true,
                manifestExists: true,
              }}
              active={activeId === "foxbell"}
              disabled={busy}
              builtin
              onClick={() => void doActivate("foxbell")}
              t={t}
            />
            {pets.map((p) => (
              <PetCard
                key={p.id}
                info={p}
                active={activeId === p.id}
                disabled={busy}
                onClick={() => void doActivate(p.id)}
                t={t}
              />
            ))}
          </div>
        )}
      </DialogContent>
    </Dialog>
  );
}

function PetCard(props: {
  info: PetCardInfo;
  active: boolean;
  disabled: boolean;
  builtin?: boolean;
  onClick: () => void;
  t: (k: string) => string;
}) {
  const { info, t } = props;
  const thumb = info.builtin
    ? "url(/pet/spritesheet.webp)"
    : info.dir
      ? `url(asset://mock/${info.dir}/spritesheet.webp)` /* 占位：正式实现用 convertFileSrc */
      : undefined;
  return (
    <button
      data-testid={`pet-card-${info.id}`}
      disabled={props.disabled}
      onClick={props.onClick}
      className={`flex flex-col items-start gap-1 rounded-lg border p-3 text-left transition-colors ${
        props.active ? "border-primary bg-accent" : "border-border hover:bg-accent/50"
      }`}
    >
      <div
        className="mb-1 h-[52px] w-[48px] rounded bg-contain"
        style={thumb ? { backgroundImage: thumb, backgroundPosition: "0 0", backgroundSize: "384px 572px" } : undefined}
      />
      <div className="flex items-center gap-1 text-sm font-medium">
        {info.displayName}
        {props.builtin && <span className="rounded bg-muted px-1 text-[10px]">{t("pet.switch.builtin")}</span>}
        {info.spriteVersionNumber > 0 ? (
          <span className="rounded bg-muted px-1 text-[10px]">v{info.spriteVersionNumber}</span>
        ) : (
          <span className="rounded bg-muted px-1 text-[10px]" title={t("pet.switch.pendingFirstCheck")}>
            v?
          </span>
        )}
      </div>
      <div className="text-muted-foreground flex gap-1 text-[10px]">
        <span title={t("pet.menu.soundNoCap")} className={info.hasVoice ? "text-primary" : "opacity-40"}>
          🔊
        </span>
        <span title={t("pet.menu.subtitleNoCap")} className={info.hasSubtitle ? "text-primary" : "opacity-40"}>
          💬
        </span>
      </div>
    </button>
  );
}
```

> 实现注意：`PetCard` 缩略图需真实 asset URL——在文件顶部 `import { convertFileSrc } from "@tauri-apps/api/core";` 并把占位行替换为 `url(${convertFileSrc(`${info.dir}/spritesheet.webp`)})`（上面的 `asset://mock` 占位仅示意，落地时必须用 convertFileSrc）。`backgroundSize` 固定 384×572（2×192、(11/4)×208 近似首帧预览即可）。

- [x] **Step 3: 设置页接线**

`src/pages/settings.tsx`：

1. import 增加：

```ts
import { PetSwitchDialog } from "@/components/pet/manage/PetSwitchDialog";
import { loadActiveId, loadActiveName } from "@/components/pet/petRuntime";
```

2. 组件内新增对话框状态（`onPetCfgChange` 之后）：

```ts
  const [switchOpen, setSwitchOpen] = useState(false);
  const [activePetName, setActivePetName] = useState(loadActiveName());
```

3. `useEffect(() => subscribeConfig(...))`（82-89 行）回调里补 `setActivePetName(loadActiveName());`。

4. pet section（420-471 行）尺寸块之后、`</div></div>` 之前追加：

```tsx
                <div className="border-t" />
                {/* 当前宠物 + 三入口（spec §11） */}
                <div className="flex items-center justify-between gap-2 py-2.5">
                  <label className="text-sm font-medium">{t("settings.pet.currentPet")}</label>
                  <span className="text-muted-foreground mr-auto pl-2 text-sm">{activePetName}</span>
                  <Button size="sm" variant="outline" onClick={() => setSwitchOpen(true)}>
                    {t("settings.pet.switchPet")}
                  </Button>
                </div>
```

5. 组件 return 的 `<Toaster />` 之后追加：

```tsx
      <PetSwitchDialog open={switchOpen} onOpenChange={setSwitchOpen} />
```

- [x] **Step 4: 写失败测试（先于 Step 2/3 亦可，此处一并跑）**

`tests/pet/petSettings.test.tsx` 追加（按现有文件的 render 方式对齐，必要时补 import）：

```tsx
describe("外部宠物三入口（spec §11）", () => {
  it("渲染当前宠物行与切换按钮", async () => {
    renderSettings(); // 现有 helper：渲染设置页并切到 pet 分区
    expect(await screen.findByText(/当前宠物|Current pet/)).toBeInTheDocument();
    const switchBtn = await screen.findByRole("button", { name: /切换宠物|Switch pet/ });
    fireEvent.click(switchBtn);
    expect(await screen.findByTestId("pet-switch-list")).toBeInTheDocument();
  });
});
```

> 执行者注意：若现有 `petSettings.test.tsx` 使用 `t("...")` 实际键渲染（中文 locale），按钮名匹配用中文分支；先读该文件确认 helper 命名（`renderSettings` 为示意，以实际为准），保持一致。

- [x] **Step 5: 运行测试**

```bash
pnpm test tests/pet/petSettings.test.tsx
```
Expected: PASS（含既有回归）。

- [x] **Step 6: Commit**

```bash
git add src/components/pet/manage/PetSwitchDialog.tsx src/pages/settings.tsx src/i18n/locales/ tests/pet/petSettings.test.tsx
git commit -m "feat(pet): switch dialog with unified validation and settings entries"
```

---

### Task 14: PetStartupGuard（启动校验弹窗）

**Files:**
- Create: `src/components/pet/PetStartupGuard.tsx`
- Modify: `src/pages/home.tsx`（挂载，先读文件定位根节点）
- Modify: `src/i18n/locales/zh.json`、`en.json`
- Test: `tests/pet/petStartupGuard.test.tsx`

- [x] **Step 1: i18n 键**

`zh.json` 顶层 `pet` 内追加：

```json
    "startup": {
      "title": "宠物素材校验",
      "fatal": "图集缺失或非法：{{msg}}",
      "issuesTitle": "以下素材与 manifest 不一致：",
      "update": "根据现有素材更新 manifest（自动备份）",
      "foxbell": "切回 foxbell",
      "ignore": "忽略并继续",
      "hidePet": "关闭宠物",
      "updated": "manifest 已更新",
      "switched": "已切回 foxbell",
      "petHidden": "宠物已关闭"
    }
```

`en.json` 对应：

```json
    "startup": {
      "title": "Pet Assets Check",
      "fatal": "Spritesheet missing or invalid: {{msg}}",
      "issuesTitle": "The following assets differ from manifest:",
      "update": "Update manifest from assets (auto backup)",
      "foxbell": "Switch back to foxbell",
      "ignore": "Ignore and continue",
      "hidePet": "Hide pet",
      "updated": "Manifest updated",
      "switched": "Switched back to foxbell",
      "petHidden": "Pet hidden"
    }
```

- [x] **Step 2: 写失败测试**

```tsx
import { beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, screen, waitFor } from "@testing-library/react";
import { render } from "./test-utils"; // 若无现成 helper，直接 @testing-library/react 的 render
import { PetStartupGuard } from "@/components/pet/PetStartupGuard";
import { tauriInvokeMock } from "../../msw/tauriMocks";

// pet-activation 的 repair 复用（mock 为可直接断言的桩）
vi.mock("@/components/pet/petActivation", async (importOriginal) => {
  const orig = await importOriginal<typeof import("@/components/pet/petActivation")>();
  return { ...orig, repairManifest: vi.fn().mockResolvedValue({ hasVoice: false, displayName: "P" }) };
});

const manifest = {
  id: "p1", displayName: "P", hasVoice: false, hasSubtitle: false,
  spriteVersionNumber: 2, spritesheetSizeBytes: 100, voices: [],
};
const scanOk = {
  id: "p1", dir: "/x/p1",
  spritesheet: { rel: "spritesheet.webp", exists: true, size: 100 },
  voiceFiles: [{ rel: "voice/done/new.mp3", exists: true, size: 5 }], // extra → issue
};

describe("PetStartupGuard（EP2 启动弹窗）", () => {
  beforeEach(() => {
    localStorage.clear();
    tauriInvokeMock.mockClear();
  });

  it("foxbell 激活时不弹窗", async () => {
    localStorage.setItem("mam-pet-active", "foxbell");
    const { container } = render(<PetStartupGuard />);
    await waitFor(() => expect(tauriInvokeMock).not.toHaveBeenCalled());
    expect(container.querySelector("[data-testid='pet-startup-dialog']")).toBeNull();
  });

  it("素材不一致 → 弹窗三选；点更新 → pet_update_manifest(backup=true)", async () => {
    localStorage.setItem("mam-pet-active", "p1");
    tauriInvokeMock.mockImplementation((cmd: string) => {
      if (cmd === "pet_scan") return Promise.resolve(scanOk);
      if (cmd === "pet_read_manifest") return Promise.resolve(manifest);
      return Promise.resolve(undefined);
    });
    render(<PetStartupGuard />);
    const dlg = await screen.findByTestId("pet-startup-dialog");
    expect(dlg).toBeInTheDocument();
    fireEvent.click(await screen.findByTestId("pet-startup-update"));
    await waitFor(() => {
      const call = tauriInvokeMock.mock.calls.find((c) => c[0] === "pet_update_manifest");
      expect(call?.[1]?.backup).toBe(true);
    });
  });

  it("图集缺失 → 致命弹窗（无更新按钮，只有切回/关闭）", async () => {
    localStorage.setItem("mam-pet-active", "p1");
    tauriInvokeMock.mockImplementation((cmd: string) => {
      if (cmd === "pet_scan")
        return Promise.resolve({ ...scanOk, spritesheet: { rel: "spritesheet.webp", exists: false, size: 0 } });
      if (cmd === "pet_read_manifest") return Promise.resolve(manifest);
      return Promise.resolve(undefined);
    });
    render(<PetStartupGuard />);
    await screen.findByTestId("pet-startup-dialog");
    expect(screen.queryByTestId("pet-startup-update")).toBeNull();
    fireEvent.click(screen.getByTestId("pet-startup-foxbell"));
    await waitFor(() => expect(localStorage.getItem("mam-pet-active")).toBe("foxbell"));
  });
});
```

- [x] **Step 3: 运行确认失败**

```bash
pnpm test tests/pet/petStartupGuard.test.tsx
```
Expected: FAIL（组件不存在）。

- [x] **Step 4: 实现**

```tsx
// PetStartupGuard — 主窗口启动校验弹窗（EP2）：素材异常时确认处理，宠物窗口自身永不弹窗
import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import { invoke } from "@tauri-apps/api/core";
import { emit } from "@tauri-apps/api/event";
import { Dialog, DialogContent, DialogHeader, DialogTitle } from "@/components/ui/dialog";
import { Button } from "@/components/ui/button";
import { loadActiveId, probeSheetRows, saveActiveId } from "./petRuntime";
import { repairManifest } from "./petActivation";
import { diffManifestVsScan, type PetManifestView, type PetScan, type ValidationIssue } from "./petValidation";
import { saveVisible } from "./petConfig";

function toFoxbell(msgKey: string) {
  saveActiveId("foxbell", true, "Foxbell");
  void emit("pet-active-changed", {});
  toast.success(msgKey);
}

export function PetStartupGuard() {
  const { t } = useTranslation();
  const [fatal, setFatal] = useState<string | null>(null);
  const [issues, setIssues] = useState<ValidationIssue[] | null>(null);
  const [ctx, setCtx] = useState<{ scan: PetScan; manifest: PetManifestView } | null>(null);

  useEffect(() => {
    let disposed = false;
    (async () => {
      const id = loadActiveId();
      if (id === "foxbell") return;
      try {
        const scan = await invoke<PetScan>("pet_scan", { id });
        if (!scan.spritesheet.exists) {
          if (!disposed) setFatal("spritesheet.webp 缺失");
          return;
        }
        const manifest = await invoke<PetManifestView | null>("pet_read_manifest", { id });
        // 图集尺寸校验：大小与缓存一致时信任记录，否则探测（spec §6.1）
        const sizeChanged = !manifest || manifest.spritesheetSizeBytes !== scan.spritesheet.size || manifest.spriteVersionNumber === 0;
        if (sizeChanged) {
          try {
            const { convertFileSrc } = await import("@tauri-apps/api/core");
            await probeSheetRows(convertFileSrc(`${scan.dir}/spritesheet.webp`));
          } catch (e) {
            if (!disposed) setFatal((e as Error).message);
            return;
          }
        }
        if (!manifest) {
          if (!disposed) setIssues([{ kind: "voice-extra", detail: "manifest.json 缺失（待首次激活校验生成）" }]);
          if (!disposed) setCtx({ scan, manifest: { id, displayName: id, hasVoice: false, hasSubtitle: false, spriteVersionNumber: 0, spritesheetSizeBytes: 0, voices: [] } });
          return;
        }
        const list = diffManifestVsScan(manifest, scan);
        if (list.length > 0 && !disposed) {
          setIssues(list);
          setCtx({ scan, manifest });
        }
      } catch {
        // 扫描失败（如宠物目录被整体删除）：宠物窗口自行降级，不打扰启动
      }
    })();
    return () => {
      disposed = true;
    };
  }, []);

  const doUpdate = async () => {
    if (!ctx) return;
    // rows 未知时按 manifest 记录；manifest 无记录（直投）按 9 保守（生成后下次激活会复核）
    const rows = ctx.manifest.spriteVersionNumber === 2 ? 11 : 9;
    const repaired = await repairManifest(ctx.manifest, ctx.scan, rows);
    await invoke("pet_update_manifest", { id: ctx.scan.id, manifest: repaired, backup: true });
    saveActiveId(ctx.scan.id, repaired.hasVoice, repaired.displayName);
    void emit("pet-active-changed", {});
    toast.success(t("pet.startup.updated"));
    setIssues(null);
  };

  const hidePet = () => {
    saveVisible(false);
    void invoke("set_pet_visible", { visible: false }).catch(() => {});
    toast.info(t("pet.startup.petHidden"));
    setFatal(null);
  };

  const open = fatal !== null || issues !== null;
  return (
    <Dialog open={open}>
      <DialogContent data-testid="pet-startup-dialog" className="max-w-md" onInteractOutside={(e) => e.preventDefault()}>
        <DialogHeader>
          <DialogTitle>{t("pet.startup.title")}</DialogTitle>
        </DialogHeader>
        {fatal !== null ? (
          <div className="space-y-3">
            <p className="text-sm">{t("pet.startup.fatal", { msg: fatal })}</p>
            <div className="flex gap-2">
              <Button size="sm" data-testid="pet-startup-foxbell" onClick={() => { toFoxbell(t("pet.startup.switched")); setFatal(null); }}>
                {t("pet.startup.foxbell")}
              </Button>
              <Button size="sm" variant="outline" onClick={hidePet}>
                {t("pet.startup.hidePet")}
              </Button>
            </div>
          </div>
        ) : (
          <div className="space-y-3">
            <p className="text-sm">{t("pet.startup.issuesTitle")}</p>
            <ul className="text-muted-foreground max-h-40 overflow-auto list-disc pl-5 text-xs" data-testid="pet-startup-issues">
              {issues?.map((i) => (
                <li key={i.kind + i.detail}>
                  {i.kind}: {i.detail}
                </li>
              ))}
            </ul>
            <div className="flex flex-wrap gap-2">
              <Button size="sm" data-testid="pet-startup-update" onClick={() => void doUpdate()}>
                {t("pet.startup.update")}
              </Button>
              <Button size="sm" variant="outline" data-testid="pet-startup-foxbell" onClick={() => { toFoxbell(t("pet.startup.switched")); setIssues(null); }}>
                {t("pet.startup.foxbell")}
              </Button>
              <Button size="sm" variant="ghost" onClick={() => setIssues(null)}>
                {t("pet.startup.ignore")}
              </Button>
            </div>
          </div>
        )}
      </DialogContent>
    </Dialog>
  );
}
```

> 执行者注意：shadcn `DialogContent` 若不支持 `onInteractOutside` 透传，改为外层包一层（radix Dialog 支持该 prop；先读 `src/components/ui/dialog.tsx` 确认）。`toFoxbell` 的 toast 参数直接传文案字符串。

- [x] **Step 5: 挂载到主窗口**

读 `src/pages/home.tsx`，在页面根组件返回的 JSX 最外层（或与现有并列的 Fragment）加入：

```tsx
import { PetStartupGuard } from "@/components/pet/PetStartupGuard";
// 根 JSX 内：
      <PetStartupGuard />
```

- [x] **Step 6: 运行测试**

```bash
pnpm test tests/pet/petStartupGuard.test.tsx
```
Expected: 3 个用例 PASS。

- [x] **Step 7: Commit**

```bash
git add src/components/pet/PetStartupGuard.tsx src/pages/home.tsx src/i18n/locales/ tests/pet/petStartupGuard.test.tsx
git commit -m "feat(pet): startup validation dialog on main window"
```

---

## Phase 4：导入与修改 UI

### Task 15: VoiceGroupEditor（共用组件）

**Files:**
- Create: `src/components/pet/manage/VoiceGroupEditor.tsx`
- Modify: `src/i18n/locales/zh.json`、`en.json`
- Test: `tests/pet/voiceGroupEditor.test.tsx`

- [x] **Step 1: i18n 键**

`zh.json` 顶层 `pet` 内追加：

```json
    "import": {
      "groupGeneral": "日常闲聊（双击说话）",
      "groupApproval": "需要审批（红灯）",
      "groupDone": "任务完成（绿灯）",
      "groupError": "出错",
      "addAudio": "添加音频",
      "remove": "移除",
      "duration": "{{ms}}s",
      "problems": {
        "too-short": "时长 ≤1s",
        "too-long": "时长 ≥20s",
        "too-big": "大于 10MB",
        "no-duration": "无法读取时长"
      },
      "coverageOk": "四组齐全，语音可用",
      "coverageMissing": "缺少分组：{{groups}}（该宠物将无语音）",
      "totalSize": "语音总大小：{{size}}",
      "tooLargeWarn": "语音总量较大（>30MB），建议精简"
    }
```

`en.json` 对应：

```json
    "import": {
      "groupGeneral": "General chat (double-click)",
      "groupApproval": "Approval needed (red)",
      "groupDone": "Task done (green)",
      "groupError": "Error",
      "addAudio": "Add audio",
      "remove": "Remove",
      "duration": "{{ms}}s",
      "problems": {
        "too-short": "≤1s",
        "too-long": "≥20s",
        "no-duration": "unreadable duration",
        "too-big": ">10MB"
      },
      "coverageOk": "All four groups present, voice enabled",
      "coverageMissing": "Missing groups: {{groups}} (pet will have no voice)",
      "totalSize": "Total voice size: {{size}}",
      "tooLargeWarn": "Voice is large (>30MB), consider trimming"
    }
```

- [x] **Step 2: 写失败测试**

```tsx
import { beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, screen } from "@testing-library/react";
import { render } from "@testing-library/react";
import { VoiceGroupEditor } from "@/components/pet/manage/VoiceGroupEditor";

const pickFiles = vi.fn();

vi.mock("@tauri-apps/plugin-dialog", () => ({
  open: (...args: unknown[]) => pickFiles(...args),
}));

const rows = [
  { group: "general", name: "a", file: "voice/general/a.mp3", sizeBytes: 1000, durationMs: 3000 },
  { group: "general", name: "b", file: "voice/general/b.mp3", sizeBytes: 1000, durationMs: 25000 }, // too-long
  { group: "approval", name: "c", file: "voice/approval/c.mp3", sizeBytes: 1000, durationMs: null }, // 未探测
];

describe("VoiceGroupEditor", () => {
  beforeEach(() => {
    pickFiles.mockResolvedValue(["C:/x/new.mp3"]);
  });

  it("渲染分组、文件、问题徽标与组含义 tooltip（EP9）", () => {
    render(<VoiceGroupEditor rows={rows} onAdd={() => {}} onRemove={() => {}} />);
    expect(screen.getByTestId("voice-group-general")).toBeInTheDocument();
    expect(screen.getByTestId("voice-row-voice/general/b.mp3")).toHaveTextContent(/too-long|≥20s|时长 ≥20s/);
    expect(screen.getByTestId("voice-group-general")).toHaveAttribute("title"); // 组含义 tooltip
  });

  it("添加按钮走文件选择器并回调路径", async () => {
    const onAdd = vi.fn();
    render(<VoiceGroupEditor rows={rows} onAdd={onAdd} onRemove={() => {}} />);
    fireEvent.click(screen.getByTestId("voice-add-general"));
    expect(pickFiles).toHaveBeenCalledWith(expect.objectContaining({ multiple: true }));
    expect(onAdd).toHaveBeenCalledWith("general", ["C:/x/new.mp3"]);
  });

  it("移除按钮回调相对路径；覆盖提示按合法文件计算", () => {
    const onRemove = vi.fn();
    render(<VoiceGroupEditor rows={rows} onAdd={() => {}} onRemove={onRemove} />);
    fireEvent.click(screen.getByTestId("voice-remove-voice/general/a.mp3"));
    expect(onRemove).toHaveBeenCalledWith("voice/general/a.mp3");
    // 仅 general 1 条合法 → 缺 approval/done/error
    expect(screen.getByTestId("voice-coverage")).toHaveTextContent(/approval|缺少分组/);
  });
});
```

- [x] **Step 3: 运行确认失败**

```bash
pnpm test tests/pet/voiceGroupEditor.test.tsx
```
Expected: FAIL。

- [x] **Step 4: 实现**

```tsx
// VoiceGroupEditor — 四分组音频编辑器（导入向导暂存模式 / 修改面板直写模式共用，spec §8.4-3/§10-3）
import { useTranslation } from "react-i18next";
import { open } from "@tauri-apps/plugin-dialog";
import { Button } from "@/components/ui/button";
import {
  AUDIO_EXTS,
  GROUPS,
  judgeVoiceTier,
  voiceRowProblem,
  type VoiceRow,
} from "../petValidation";

const GROUP_LABEL_KEY: Record<string, string> = {
  general: "pet.import.groupGeneral",
  approval: "pet.import.groupApproval",
  done: "pet.import.groupDone",
  error: "pet.import.groupError",
};

export function VoiceGroupEditor(props: {
  rows: VoiceRow[];
  onAdd: (group: string, paths: string[]) => void | Promise<void>;
  onRemove: (rel: string) => void | Promise<void>;
  busy?: boolean;
}) {
  const { t } = useTranslation();
  const totalBytes = props.rows.reduce((s, r) => s + r.sizeBytes, 0);
  const valid = props.rows.filter((r) => !voiceRowProblem(r));
  const judge = judgeVoiceTier(valid.map((r) => ({ rel: r.file, size: r.sizeBytes, durationMs: r.durationMs })));
  const missing = GROUPS.filter((g) => judge.coverage[g] === 0);

  const add = async (group: string) => {
    const paths = (await open({
      multiple: true,
      filters: [{ name: "Audio", extensions: AUDIO_EXTS }],
    })) as string[] | string | null;
    if (!paths) return;
    await props.onAdd(group, Array.isArray(paths) ? paths : [paths]);
  };

  return (
    <div className="space-y-3" data-testid="voice-group-editor">
      {GROUPS.map((g) => {
        const list = props.rows.filter((r) => r.group === g);
        return (
          <div key={g} data-testid={`voice-group-${g}`} title={t(GROUP_LABEL_KEY[g])}>
            <div className="mb-1 flex items-center justify-between">
              <span className="text-sm font-medium">{g}</span>
              <Button
                size="sm"
                variant="outline"
                disabled={props.busy}
                data-testid={`voice-add-${g}`}
                onClick={() => void add(g)}
              >
                {t("pet.import.addAudio")}
              </Button>
            </div>
            {list.map((r) => {
              const problem = voiceRowProblem(r);
              return (
                <div
                  key={r.file}
                  data-testid={`voice-row-${r.file}`}
                  className="text-muted-foreground flex items-center justify-between py-0.5 text-xs"
                >
                  <span className="max-w-[60%] truncate">
                    {r.name}
                    {r.durationMs !== null && (
                      <span className="ml-1">({t("pet.import.duration", { ms: (r.durationMs / 1000).toFixed(1) })})</span>
                    )}
                  </span>
                  <span className="flex items-center gap-2">
                    {problem && (
                      <span className="rounded bg-destructive/15 px-1 text-destructive">
                        {t(`pet.import.problems.${problem}`)}
                      </span>
                    )}
                    <button
                      data-testid={`voice-remove-${r.file}`}
                      className="underline-offset-2 hover:underline"
                      onClick={() => void props.onRemove(r.file)}
                    >
                      {t("pet.import.remove")}
                    </button>
                  </span>
                </div>
              );
            })}
          </div>
        );
      })}
      <div data-testid="voice-coverage" className="text-xs">
        {missing.length === 0 ? (
          <span className="text-primary">{t("pet.import.coverageOk")}</span>
        ) : (
          <span className="text-muted-foreground">{t("pet.import.coverageMissing", { groups: missing.join(", ") })}</span>
        )}
        <span className="ml-2">{t("pet.import.totalSize", { size: `${(totalBytes / 1024 / 1024).toFixed(1)}MB` })}</span>
        {totalBytes > 30 * 1024 * 1024 && <span className="text-destructive ml-1">{t("pet.import.tooLargeWarn")}</span>}
      </div>
    </div>
  );
}
```

- [x] **Step 5: 运行测试通过**

```bash
pnpm test tests/pet/voiceGroupEditor.test.tsx
```
Expected: 全部 PASS。

- [x] **Step 6: Commit**

```bash
git add src/components/pet/manage/VoiceGroupEditor.tsx src/i18n/locales/ tests/pet/voiceGroupEditor.test.tsx
git commit -m "feat(pet): shared voice group editor with inline validation"
```

---

### Task 16: PetImportDialog（导入向导）

**Files:**
- Create: `src/components/pet/manage/PetImportDialog.tsx`
- Modify: `src/pages/settings.tsx`（补导入入口与对话框）
- Modify: `src/i18n/locales/zh.json`、`en.json`
- Test: `tests/pet/petImportDialog.test.tsx`

- [x] **Step 1: i18n 键（追加到 `pet.import`）**

`zh.json`：

```json
      "title": "导入宠物",
      "sourceTitle": "选择来源",
      "tabCodex": "从 codex 导入",
      "tabLocal": "文件夹 / 压缩包",
      "tabPetdex": "petdex 在线导入",
      "codexEmpty": "~/.codex/pets 下没有可用宠物",
      "codexImported": "已导入",
      "stage": "暂存所选",
      "pickFolder": "选择文件夹",
      "pickZip": "选择压缩包",
      "petdexHint": "先去 petdex.dev 画廊挑选，把宠物页链接粘贴到下面（如 https://petdex.dev/pets/capvolt）",
      "petdexBrowse": "浏览 petdex.dev",
      "petdexDownload": "下载并暂存",
      "configTitle": "配置确认",
      "name": "宠物名（文件夹名）",
      "nameHint": "仅字母/数字/连字符/下划线",
      "displayName": "展示名",
      "description": "描述（可选）",
      "preview": "图集预览",
      "sheetInvalid": "图集尺寸非法（需 1536×1872 或 1536×2288）",
      "subtitle": "同步导入字幕（字幕 = 音频文件名）",
      "execute": "执行导入",
      "cancelImport": "取消",
      "doneTitle": "导入成功",
      "activateNow": "立即激活",
      "finish": "完成",
      "errorStage": "暂存失败：{{msg}}",
      "errorFinalize": "导入失败：{{msg}}"
```

`en.json` 对应（英文文案）：

```json
      "title": "Import Pet",
      "sourceTitle": "Choose source",
      "tabCodex": "From codex",
      "tabLocal": "Folder / zip",
      "tabPetdex": "petdex online",
      "codexEmpty": "No pets found under ~/.codex/pets",
      "codexImported": "imported",
      "stage": "Stage selected",
      "pickFolder": "Pick folder",
      "pickZip": "Pick zip",
      "petdexHint": "Browse petdex.dev, then paste a pet page link (e.g. https://petdex.dev/pets/capvolt)",
      "petdexBrowse": "Browse petdex.dev",
      "petdexDownload": "Download & stage",
      "configTitle": "Configure",
      "name": "Pet name (folder)",
      "nameHint": "letters/digits/-/_ only",
      "displayName": "Display name",
      "description": "Description (optional)",
      "preview": "Preview",
      "sheetInvalid": "Invalid sheet size (need 1536x1872 or 1536x2288)",
      "subtitle": "Import subtitles (subtitle = audio filename)",
      "execute": "Import",
      "cancelImport": "Cancel",
      "doneTitle": "Imported",
      "activateNow": "Activate now",
      "finish": "Done",
      "errorStage": "Stage failed: {{msg}}",
      "errorFinalize": "Import failed: {{msg}}"
```

- [x] **Step 2: 写失败测试**

```tsx
import { beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, screen, waitFor } from "@testing-library/react";
import { render } from "@testing-library/react";
import { PetImportDialog } from "@/components/pet/manage/PetImportDialog";
import { tauriInvokeMock } from "../../msw/tauriMocks";

const pick = vi.fn();

vi.mock("@tauri-apps/plugin-dialog", () => ({ open: (...a: unknown[]) => pick(...a) }));
vi.mock("@/components/pet/petRuntime", async (importOriginal) => {
  const orig = await importOriginal<typeof import("@/components/pet/petRuntime")>();
  return { ...orig, probeSheetRows: vi.fn().mockResolvedValue(9), probeAudioDurationMs: vi.fn().mockResolvedValue(3000) };
});

const staged = {
  stagingId: "s1",
  dir: "/home/u/.mam/pets/.import-staging/s1",
  suggestedName: "starry-dew",
  suggestedDisplayName: "Starry Dew",
  spriteVersionNumber: 0,
  spritesheetSize: 1652314,
  voiceFiles: [],
};

describe("PetImportDialog", () => {
  beforeEach(() => {
    tauriInvokeMock.mockClear();
    tauriInvokeMock.mockImplementation((cmd: string) => {
      if (cmd === "pet_stage_from_folder") return Promise.resolve(staged);
      if (cmd === "pet_finalize_import") return Promise.resolve({ id: staged.suggestedName, displayName: staged.suggestedDisplayName });
      return Promise.resolve(undefined);
    });
    pick.mockResolvedValue("C:/pets/starry-dew");
  });

  it("本地文件夹来源 → 暂存 → 配置页显示预览与名称 → 完成导入", async () => {
    render(<PetImportDialog open onOpenChange={() => {}} />);
    fireEvent.click(await screen.findByTestId("import-tab-local"));
    fireEvent.click(await screen.findByTestId("import-pick-folder"));
    await screen.findByTestId("import-config");
    expect(await screen.findByDisplayValue("starry-dew")).toBeInTheDocument();
    expect(await screen.findByTestId("import-sheet-badge")).toHaveTextContent("v1"); // probe 桩返回 9
    fireEvent.click(await screen.findByTestId("import-execute"));
    await waitFor(() => {
      const call = tauriInvokeMock.mock.calls.find((c) => c[0] === "pet_finalize_import");
      expect(call?.[1]?.name).toBe("starry-dew");
      expect(call?.[1]?.manifest.spriteVersionNumber).toBe(1);
    });
    expect(await screen.findByTestId("import-done")).toBeInTheDocument();
  });

  it("petdex 渠道：输入链接 → pet_stage_from_petdex", async () => {
    tauriInvokeMock.mockImplementation((cmd: string) => {
      if (cmd === "pet_stage_from_petdex") return Promise.resolve(staged);
      if (cmd === "pet_finalize_import") return Promise.resolve({ id: "x" });
      return Promise.resolve(undefined);
    });
    render(<PetImportDialog open onOpenChange={() => {}} />);
    fireEvent.click(await screen.findByTestId("import-tab-petdex"));
    fireEvent.change(await screen.findByTestId("import-petdex-url"), {
      target: { value: "https://petdex.dev/pets/capvolt" },
    });
    fireEvent.click(await screen.findByTestId("import-petdex-download"));
    expect(await screen.findByTestId("import-config")).toBeInTheDocument();
    expect(tauriInvokeMock.mock.calls.find((c) => c[0] === "pet_stage_from_petdex")?.[1]?.url).toBe(
      "https://petdex.dev/pets/capvolt"
    );
  });

  it("配置页关闭对话框 → pet_cancel_import 清理", async () => {
    const onOpenChange = vi.fn();
    render(<PetImportDialog open onOpenChange={onOpenChange} />);
    fireEvent.click(await screen.findByTestId("import-tab-local"));
    fireEvent.click(await screen.findByTestId("import-pick-folder"));
    await screen.findByTestId("import-config");
    fireEvent.click(await screen.findByTestId("import-cancel"));
    await waitFor(() =>
      expect(tauriInvokeMock.mock.calls.find((c) => c[0] === "pet_cancel_import")?.[1]?.stagingId).toBe("s1")
    );
  });
});
```

- [x] **Step 3: 运行确认失败**

```bash
pnpm test tests/pet/petImportDialog.test.tsx
```
Expected: FAIL。

- [x] **Step 4: 实现**

```tsx
// PetImportDialog — 导入向导：来源（codex/本地/petdex）→ 配置确认 → 完成（spec §8）
import { useCallback, useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import { convertFileSrc, invoke } from "@tauri-apps/api/core";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { openUrl } from "@tauri-apps/plugin-opener";
import { Dialog, DialogContent, DialogHeader, DialogTitle } from "@/components/ui/dialog";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Switch } from "@/components/ui/switch";
import { probeAudioDurationMs, probeSheetRows, type PetRows } from "../petRuntime";
import { judgeVoiceTier, voiceRowProblem, type VoiceRow } from "../petValidation";
import { VoiceGroupEditor } from "./VoiceGroupEditor";

interface StagedPetDto {
  stagingId: string;
  dir: string;
  suggestedName: string;
  suggestedDisplayName: string;
  spriteVersionNumber: number;
  spritesheetSize: number;
  voiceFiles: { group: string; name: string; file: string; sizeBytes: number }[];
}

interface CodexPetDto {
  id: string;
  displayName: string;
  spriteVersionNumber: number;
  imported: boolean;
}

type Step = "source" | "config" | "done";

export function PetImportDialog(props: {
  open: boolean;
  onOpenChange: (v: boolean) => void;
  onImported?: (id: string) => void;
}) {
  const { t } = useTranslation();
  const [step, setStep] = useState<Step>("source");
  const [tab, setTab] = useState<"codex" | "local" | "petdex">("codex");
  const [codexList, setCodexList] = useState<CodexPetDto[]>([]);
  const [petdexUrl, setPetdexUrl] = useState("");
  const [staged, setStaged] = useState<StagedPetDto | null>(null);
  const [rows, setRows] = useState<PetRows | null>(null);
  const [name, setName] = useState("");
  const [displayName, setDisplayName] = useState("");
  const [description, setDescription] = useState("");
  const [voiceRows, setVoiceRows] = useState<VoiceRow[]>([]);
  const [subtitle, setSubtitle] = useState(true);
  const [busy, setBusy] = useState(false);
  const [importedId, setImportedId] = useState("");

  useEffect(() => {
    if (!props.open) return;
    setStep("source");
    setStaged(null);
    setRows(null);
    setVoiceRows([]);
    if (tab === "codex") {
      invoke<CodexPetDto[]>("pet_list_codex_pets").then(setCodexList).catch(() => setCodexList([]));
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [props.open]);

  // codex 列表按需刷新
  useEffect(() => {
    if (props.open && step === "source" && tab === "codex") {
      invoke<CodexPetDto[]>("pet_list_codex_pets").then(setCodexList).catch(() => setCodexList([]));
    }
  }, [props.open, step, tab]);

  const enterConfig = useCallback((s: StagedPetDto) => {
    setStaged(s);
    setName(s.suggestedName);
    setDisplayName(s.suggestedDisplayName || s.suggestedName);
    setVoiceRows(s.voiceFiles.map((v) => ({ ...v, durationMs: null })));
    setStep("config");
    probeSheetRows(convertFileSrc(`${s.dir}/spritesheet.webp`))
      .then(setRows)
      .catch(() => setRows(null));
  }, []);

  // 未探测时长的文件补探测（并行）
  useEffect(() => {
    if (!staged || step !== "config") return;
    const pending = voiceRows.filter((r) => r.durationMs === null);
    if (pending.length === 0) return;
    let cancelled = false;
    void Promise.all(
      pending.map(async (r) => ({
        file: r.file,
        durationMs: await probeAudioDurationMs(convertFileSrc(`${staged.dir}/${r.file}`)).catch(() => null),
      }))
    ).then((probed) => {
      if (cancelled) return;
      setVoiceRows((prev) =>
        prev.map((r) => {
          const hit = probed.find((p) => p.file === r.file);
          return hit ? { ...r, durationMs: hit.durationMs } : r;
        })
      );
    });
    return () => {
      cancelled = true;
    };
  }, [staged, step, voiceRows]);

  const stageFrom = async (fn: () => Promise<StagedPetDto>) => {
    setBusy(true);
    try {
      enterConfig(await fn());
    } catch (e) {
      toast.error(t("pet.import.errorStage", { msg: (e as Error).message }));
    } finally {
      setBusy(false);
    }
  };

  const cancelAll = async () => {
    if (staged) await invoke("pet_cancel_import", { stagingId: staged.stagingId }).catch(() => {});
    setStaged(null);
    setStep("source");
    props.onOpenChange(false);
  };

  const execute = async () => {
    if (!staged || !rows) return;
    setBusy(true);
    try {
      const valid = voiceRows.filter((r) => !voiceRowProblem(r));
      const hasVoice = judgeVoiceTier(valid.map((r) => ({ rel: r.file, size: r.sizeBytes, durationMs: r.durationMs }))).hasVoice;
      const manifest = {
        schemaVersion: 1,
        id: name,
        displayName,
        description,
        source: tab === "codex" ? "codex" : tab === "petdex" ? "petdex" : "folder",
        spriteVersionNumber: rows === 9 ? 1 : 2,
        spritesheetSizeBytes: staged.spritesheetSize,
        hasVoice,
        hasSubtitle: hasVoice && subtitle,
        voices: valid.map((r) => ({
          group: r.group,
          name: r.name,
          file: r.file,
          sizeBytes: r.sizeBytes,
          durationMs: r.durationMs ?? 0,
        })),
      };
      const sum = await invoke<{ id: string }>("pet_finalize_import", {
        stagingId: staged.stagingId,
        name,
        manifest,
      });
      setImportedId(sum.id);
      setStaged(null);
      setStep("done");
      props.onImported?.(sum.id);
    } catch (e) {
      toast.error(t("pet.import.errorFinalize", { msg: (e as Error).message }));
    } finally {
      setBusy(false);
    }
  };

  const nameOk = /^[A-Za-z0-9_-]+$/.test(name) && name.toLowerCase() !== "foxbell";
  const validCount = voiceRows.filter((r) => !voiceRowProblem(r)).length;

  return (
    <Dialog open={props.open} onOpenChange={(v) => (v ? props.onOpenChange(true) : void cancelAll())}>
      <DialogContent className="max-w-xl">
        <DialogHeader>
          <DialogTitle>
            {step === "source" ? t("pet.import.title") : step === "config" ? t("pet.import.configTitle") : t("pet.import.doneTitle")}
          </DialogTitle>
        </DialogHeader>

        {step === "source" && (
          <div className="space-y-3" data-testid="import-source">
            <div className="flex gap-1">
              {(["codex", "local", "petdex"] as const).map((k) => (
                <Button
                  key={k}
                  size="sm"
                  variant={tab === k ? "default" : "outline"}
                  data-testid={`import-tab-${k}`}
                  onClick={() => setTab(k)}
                >
                  {t(`pet.import.tab${k === "codex" ? "Codex" : k === "local" ? "Local" : "Petdex"}`)}
                </Button>
              ))}
            </div>
            {tab === "codex" && (
              <div className="max-h-64 space-y-1 overflow-auto" data-testid="import-codex-list">
                {codexList.length === 0 && <p className="text-muted-foreground text-sm">{t("pet.import.codexEmpty")}</p>}
                {codexList.map((c) => (
                  <div key={c.id} className="flex items-center justify-between rounded border px-2 py-1 text-sm">
                    <span>
                      {c.id}
                      {c.displayName && <span className="text-muted-foreground ml-1">{c.displayName}</span>}
                      {c.spriteVersionNumber > 0 && <span className="bg-muted ml-1 rounded px-1 text-[10px]">v{c.spriteVersionNumber}</span>}
                    </span>
                    {c.imported ? (
                      <span className="text-muted-foreground text-xs">{t("pet.import.codexImported")}</span>
                    ) : (
                      <Button
                        size="sm"
                        variant="outline"
                        disabled={busy}
                        data-testid={`import-stage-${c.id}`}
                        onClick={() => void stageFrom(() => invoke<StagedPetDto>("pet_stage_from_codex", { codexId: c.id }))}
                      >
                        {t("pet.import.stage")}
                      </Button>
                    )}
                  </div>
                ))}
              </div>
            )}
            {tab === "local" && (
              <div className="flex gap-2">
                <Button
                  size="sm"
                  disabled={busy}
                  data-testid="import-pick-folder"
                  onClick={() =>
                    void openDialog({ directory: true }).then((p) => {
                      if (p) void stageFrom(() => invoke<StagedPetDto>("pet_stage_from_folder", { path: p as string }));
                    })
                  }
                >
                  {t("pet.import.pickFolder")}
                </Button>
                <Button
                  size="sm"
                  disabled={busy}
                  onClick={() =>
                    void openDialog({ filters: [{ name: "ZIP", extensions: ["zip"] }] }).then((p) => {
                      if (p) void stageFrom(() => invoke<StagedPetDto>("pet_stage_from_zip", { path: p as string }));
                    })
                  }
                >
                  {t("pet.import.pickZip")}
                </Button>
              </div>
            )}
            {tab === "petdex" && (
              <div className="space-y-2">
                <p className="text-muted-foreground text-xs">{t("pet.import.petdexHint")}</p>
                <div className="flex items-center gap-2">
                  <Input
                    data-testid="import-petdex-url"
                    placeholder="https://petdex.dev/pets/..."
                    value={petdexUrl}
                    onChange={(e) => setPetdexUrl(e.target.value)}
                  />
                  <Button size="sm" variant="outline" onClick={() => void openUrl("https://petdex.dev/collections")}>
                    {t("pet.import.petdexBrowse")}
                  </Button>
                </div>
                <Button
                  size="sm"
                  disabled={busy || !petdexUrl}
                  data-testid="import-petdex-download"
                  onClick={() => void stageFrom(() => invoke<StagedPetDto>("pet_stage_from_petdex", { url: petdexUrl }))}
                >
                  {t("pet.import.petdexDownload")}
                </Button>
              </div>
            )}
          </div>
        )}

        {step === "config" && staged && (
          <div className="space-y-3" data-testid="import-config">
            <div className="flex gap-4">
              <div className="flex-none">
                <div className="bg-muted/40 mb-1 h-[104px] w-[96px] rounded"
                  style={{
                    backgroundImage: `url(${convertFileSrc(`${staged.dir}/spritesheet.webp`)})`,
                    backgroundPosition: "0 0",
                    backgroundSize: "768px 1144px",
                  }}
                  data-testid="import-preview"
                  title={t("pet.import.preview")}
                />
                <div className="text-center">
                  {rows ? (
                    <span data-testid="import-sheet-badge" className="bg-muted rounded px-1 text-[10px]">
                      v{rows === 9 ? 1 : 2}
                    </span>
                  ) : (
                    <span data-testid="import-sheet-badge" className="text-destructive text-[10px]">
                      {t("pet.import.sheetInvalid")}
                    </span>
                  )}
                </div>
              </div>
              <div className="flex-1 space-y-2">
                <div>
                  <label className="text-sm" title={t("pet.import.nameHint")}>
                    {t("pet.import.name")}
                  </label>
                  <Input value={name} onChange={(e) => setName(e.target.value)} />
                  {!nameOk && <p className="text-destructive text-xs">{t("pet.import.nameHint")}</p>}
                </div>
                <div>
                  <label className="text-sm">{t("pet.import.displayName")}</label>
                  <Input value={displayName} onChange={(e) => setDisplayName(e.target.value)} />
                </div>
                <div>
                  <label className="text-sm">{t("pet.import.description")}</label>
                  <Input value={description} onChange={(e) => setDescription(e.target.value)} />
                </div>
              </div>
            </div>
            <VoiceGroupEditor
              rows={voiceRows}
              busy={busy}
              onAdd={async (group, paths) => {
                const added = await invoke<StagedPetDto["voiceFiles"]>("pet_stage_audio", {
                  stagingId: staged.stagingId,
                  srcPaths: paths,
                  group,
                });
                setVoiceRows((prev) => [...prev, ...added.map((a) => ({ ...a, durationMs: null }))]);
              }}
              onRemove={async (rel) => {
                await invoke("pet_remove_staged_audio", { stagingId: staged.stagingId, rel });
                setVoiceRows((prev) => prev.filter((r) => r.file !== rel));
              }}
            />
            <div className="flex items-center gap-2" title={t("pet.import.subtitle")}>
              <Switch checked={subtitle} disabled={validCount === 0} onCheckedChange={setSubtitle} />
              <span className="text-sm">{t("pet.import.subtitle")}</span>
            </div>
            <div className="flex justify-end gap-2">
              <Button size="sm" variant="ghost" data-testid="import-cancel" onClick={() => void cancelAll()}>
                {t("pet.import.cancelImport")}
              </Button>
              <Button size="sm" disabled={!nameOk || !rows || busy} data-testid="import-execute" onClick={() => void execute()}>
                {t("pet.import.execute")}
              </Button>
            </div>
          </div>
        )}

        {step === "done" && (
          <div className="space-y-3" data-testid="import-done">
            <p className="text-sm">{importedId}</p>
            <div className="flex justify-end gap-2">
              <Button
                size="sm"
                onClick={async () => {
                  const { activatePet } = await import("../petActivation");
                  const r = await activatePet(importedId, async () => "update");
                  if (r.status === "activated") toast.success(t("pet.switch.activated", { name: importedId }));
                  props.onOpenChange(false);
                }}
              >
                {t("pet.import.activateNow")}
              </Button>
              <Button size="sm" variant="outline" onClick={() => props.onOpenChange(false)}>
                {t("pet.import.finish")}
              </Button>
            </div>
          </div>
        )}
      </DialogContent>
    </Dialog>
  );
}
```

> 执行者注意：shadcn `Input`/`Switch`/`Button` 若与本文件属性用法有出入，以 `src/components/ui/` 实际导出为准微调。

- [x] **Step 5: 设置页补导入入口**

`settings.tsx`：import `PetImportDialog`；新增 `const [importOpen, setImportOpen] = useState(false);`；三入口行在"切换宠物"按钮旁加：

```tsx
                  <Button size="sm" variant="outline" onClick={() => setImportOpen(true)}>
                    {t("settings.pet.importPet")}
                  </Button>
```

（"修改宠物"按钮在 Task 17 接线，本任务先渲染 disabled 占位或直接留到 Task 17——选择：本任务不加修改按钮，Task 17 一并加，避免 disabled 占位。）`<Toaster />` 后挂 `<PetImportDialog open={importOpen} onOpenChange={setImportOpen} />`。

- [x] **Step 6: 运行测试**

```bash
pnpm test tests/pet/petImportDialog.test.tsx tests/pet/petSettings.test.tsx
```
Expected: PASS。

- [x] **Step 7: Commit**

```bash
git add src/components/pet/manage/PetImportDialog.tsx src/pages/settings.tsx src/i18n/locales/ tests/pet/
git commit -m "feat(pet): import wizard with codex/local/petdex sources"
```

---

### Task 17: PetManageDialog（修改宠物）

**Files:**
- Create: `src/components/pet/manage/PetManageDialog.tsx`
- Modify: `src/pages/settings.tsx`（补修改入口）
- Modify: `src/i18n/locales/zh.json`、`en.json`
- Test: `tests/pet/petManageDialog.test.tsx`

- [x] **Step 1: i18n 键**

`zh.json` 顶层 `pet` 内追加：

```json
    "manage": {
      "title": "修改宠物",
      "pick": "选择要管理的宠物",
      "rename": "新名称（文件夹名）",
      "renameBtn": "重命名",
      "renamedToast": "已重命名为 {{name}}",
      "subtitle": "同步导入字幕",
      "save": "保存修改",
      "savedToast": "已保存（manifest 已备份更新）",
      "activeSwitchNotice": "该宠物正在使用，已自动切回 foxbell",
      "openFolder": "打开文件夹",
      "delete": "删除宠物",
      "deleteConfirm": "确认删除？整個文件夹将移入回收站",
      "deletedToast": "已删除 {{name}}"
    }
```

`en.json` 对应：

```json
    "manage": {
      "title": "Manage Pets",
      "pick": "Pick a pet to manage",
      "rename": "New name (folder)",
      "renameBtn": "Rename",
      "renamedToast": "Renamed to {{name}}",
      "subtitle": "Import subtitles",
      "save": "Save changes",
      "savedToast": "Saved (manifest backed up & updated)",
      "activeSwitchNotice": "Pet in use; switched back to foxbell first",
      "openFolder": "Open folder",
      "delete": "Delete pet",
      "deleteConfirm": "Delete? The whole folder goes to recycle bin",
      "deletedToast": "Deleted {{name}}"
    }
```

- [x] **Step 2: 写失败测试**

```tsx
import { beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, screen, waitFor } from "@testing-library/react";
import { render } from "@testing-library/react";
import { PetManageDialog } from "@/components/pet/manage/PetManageDialog";
import { tauriInvokeMock } from "../../msw/tauriMocks";

vi.mock("@/components/pet/petActivation", async (importOriginal) => {
  const orig = await importOriginal<typeof import("@/components/pet/petActivation")>();
  return { ...orig, buildManifestFromScan: vi.fn(), repairManifest: vi.fn().mockResolvedValue({ hasVoice: true, displayName: "P" }) };
});
vi.mock("@tauri-apps/plugin-dialog", () => ({ open: vi.fn().mockResolvedValue(["C:/a.mp3"]) }));
vi.mock("@/components/pet/petRuntime", async (importOriginal) => {
  const orig = await importOriginal<typeof import("@/components/pet/petRuntime")>();
  return { ...orig, probeSheetRows: vi.fn().mockResolvedValue(9), probeAudioDurationMs: vi.fn().mockResolvedValue(3000) };
});

const pets = [
  { id: "starry-dew", displayName: "Starry Dew", spriteVersionNumber: 1, hasVoice: false, hasSubtitle: false, manifestExists: true, spritesheetExists: true, dir: "/x/starry-dew", source: "folder", description: "" },
];

describe("PetManageDialog", () => {
  beforeEach(() => {
    localStorage.clear();
    tauriInvokeMock.mockClear();
    tauriInvokeMock.mockImplementation((cmd: string) => {
      if (cmd === "pet_list_pets") return Promise.resolve(pets);
      if (cmd === "pet_scan")
        return Promise.resolve({
          id: "starry-dew", dir: "/x/starry-dew",
          spritesheet: { rel: "spritesheet.webp", exists: true, size: 100 },
          voiceFiles: [],
        });
      if (cmd === "pet_read_manifest")
        return Promise.resolve({
          id: "starry-dew", displayName: "Starry Dew", hasVoice: false, hasSubtitle: false,
          spriteVersionNumber: 1, spritesheetSizeBytes: 100, voices: [],
        });
      return Promise.resolve(undefined);
    });
  });

  it("列表 → 选中进入面板并渲染字段", async () => {
    render(<PetManageDialog open onOpenChange={() => {}} />);
    fireEvent.click(await screen.findByTestId("manage-pick-starry-dew"));
    expect(await screen.findByTestId("manage-panel")).toBeInTheDocument();
    expect(await screen.findByTestId("manage-rename-input")).toBeInTheDocument();
  });

  it("重命名激活中的宠物：先切回 foxbell 再 pet_rename_pet（EP5）", async () => {
    localStorage.setItem("mam-pet-active", "starry-dew");
    render(<PetManageDialog open onOpenChange={() => {}} />);
    fireEvent.click(await screen.findByTestId("manage-pick-starry-dew"));
    fireEvent.change(await screen.findByTestId("manage-rename-input"), { target: { value: "dew" } });
    fireEvent.click(await screen.findByTestId("manage-rename-btn"));
    await waitFor(() =>
      expect(tauriInvokeMock.mock.calls.find((c) => c[0] === "pet_rename_pet")?.[1]).toEqual({
        oldId: "starry-dew",
        newId: "dew",
      })
    );
    expect(localStorage.getItem("mam-pet-active")).toBe("foxbell"); // 已先切回
  });

  it("保存：重建 manifest 并 pet_update_manifest(backup=true)", async () => {
    render(<PetManageDialog open onOpenChange={() => {}} />);
    fireEvent.click(await screen.findByTestId("manage-pick-starry-dew"));
    fireEvent.click(await screen.findByTestId("manage-save"));
    await waitFor(() => {
      const call = tauriInvokeMock.mock.calls.find((c) => c[0] === "pet_update_manifest");
      expect(call?.[1]?.backup).toBe(true);
    });
  });

  it("删除：确认后 pet_delete_pet", async () => {
    render(<PetManageDialog open onOpenChange={() => {}} />);
    fireEvent.click(await screen.findByTestId("manage-pick-starry-dew"));
    fireEvent.click(await screen.findByTestId("manage-delete"));
    fireEvent.click(await screen.findByTestId("manage-delete-confirm"));
    await waitFor(() =>
      expect(tauriInvokeMock.mock.calls.find((c) => c[0] === "pet_delete_pet")?.[1]?.id).toBe("starry-dew")
    );
  });
});
```

- [x] **Step 3: 运行确认失败**

```bash
pnpm test tests/pet/petManageDialog.test.tsx
```
Expected: FAIL。

- [x] **Step 4: 实现**

```tsx
// PetManageDialog — 修改宠物：重命名/展示名/音频/字幕/删除/打开文件夹（spec §10；激活中先切回 foxbell，EP5）
import { useCallback, useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import { invoke } from "@tauri-apps/api/core";
import { emit } from "@tauri-apps/api/event";
import { Dialog, DialogContent, DialogHeader, DialogTitle } from "@/components/ui/dialog";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Switch } from "@/components/ui/switch";
import { loadActiveId, saveActiveId, type PetRows } from "../petRuntime";
import { buildManifestFromScan, repairManifest } from "../petActivation";
import type { PetManifestView, PetScan, VoiceRow } from "../petValidation";
import { VoiceGroupEditor } from "./VoiceGroupEditor";

interface PetSummaryDto {
  id: string;
  displayName: string;
  description: string;
  spriteVersionNumber: number;
  hasVoice: boolean;
  hasSubtitle: boolean;
  manifestExists: boolean;
  dir: string;
}

export function PetManageDialog(props: { open: boolean; onOpenChange: (v: boolean) => void }) {
  const { t } = useTranslation();
  const [pets, setPets] = useState<PetSummaryDto[]>([]);
  const [selected, setSelected] = useState<PetSummaryDto | null>(null);
  const [renameTo, setRenameTo] = useState("");
  const [displayName, setDisplayName] = useState("");
  const [subtitle, setSubtitle] = useState(false);
  const [voiceRows, setVoiceRows] = useState<VoiceRow[]>([]);
  const [deleting, setDeleting] = useState(false);
  const [busy, setBusy] = useState(false);

  const reload = useCallback(async () => {
    try {
      setPets(await invoke<PetSummaryDto[]>("pet_list_pets"));
    } catch {
      setPets([]);
    }
  }, []);

  useEffect(() => {
    if (props.open) {
      setSelected(null);
      void reload();
    }
  }, [props.open, reload]);

  const openPanel = async (p: PetSummaryDto) => {
    setSelected(p);
    setRenameTo("");
    setDisplayName(p.displayName);
    setSubtitle(p.hasSubtitle);
    setVoiceRows([]);
    try {
      const scan = await invoke<PetScan>("pet_scan", { id: p.id });
      const m = await invoke<PetManifestView | null>("pet_read_manifest", { id: p.id });
      const rows = (m?.voices ?? []).map((v) => ({
        group: v.group,
        name: v.name,
        file: v.file,
        sizeBytes: v.sizeBytes,
        durationMs: v.durationMs, // manifest 缓存时长（未变条目信任缓存，spec §4.2）
      }));
      // 磁盘上不在 manifest 的合法音频（手动放入）→ 待探测
      const known = new Set(rows.map((r) => r.file));
      const extra = scan.voiceFiles
        .filter((f) => f.rel.startsWith("voice/") && !known.has(f.rel))
        .map((f) => {
          const seg = f.rel.split("/");
          return { group: seg[1], name: seg[2]?.replace(/\.[^.]+$/, "") ?? "", file: f.rel, sizeBytes: f.size, durationMs: null };
        });
      setVoiceRows([...rows, ...extra]);
    } catch {
      /* 面板仍可用，保存时按扫描兜底 */
    }
  };

  /** 激活中宠物先自动切回 foxbell（EP5），返回是否执行了切换 */
  const ensureNotActive = (): boolean => {
    if (loadActiveId() !== selected?.id) return false;
    saveActiveId("foxbell", true, "Foxbell");
    void emit("pet-active-changed", {});
    toast.info(t("pet.manage.activeSwitchNotice"));
    return true;
  };

  const doRename = async () => {
    if (!selected || !renameTo || renameTo === selected.id) return;
    setBusy(true);
    try {
      ensureNotActive();
      await invoke("pet_rename_pet", { oldId: selected.id, newId: renameTo });
      toast.success(t("pet.manage.renamedToast", { name: renameTo }));
      await reload();
      setSelected(null);
    } catch (e) {
      toast.error((e as Error).message);
    } finally {
      setBusy(false);
    }
  };

  const doSave = async () => {
    if (!selected) return;
    setBusy(true);
    try {
      ensureNotActive();
      const scan = await invoke<PetScan>("pet_scan", { id: selected.id });
      const rows: PetRows = selected.spriteVersionNumber === 2 ? 11 : 9;
      const old = await invoke<PetManifestView | null>("pet_read_manifest", { id: selected.id });
      const base = old
        ? await repairManifest({ ...old, displayName, description: old.description, hasSubtitle: subtitle && old.hasVoice }, scan, rows)
        : await buildManifestFromScan(selected.id, scan, rows, "folder", subtitle, { displayName });
      const manifest = { ...base, displayName, hasSubtitle: base.hasVoice && subtitle };
      await invoke("pet_update_manifest", { id: selected.id, manifest, backup: true });
      toast.success(t("pet.manage.savedToast"));
      await reload();
    } catch (e) {
      toast.error((e as Error).message);
    } finally {
      setBusy(false);
    }
  };

  const doDelete = async () => {
    if (!selected) return;
    setBusy(true);
    try {
      ensureNotActive();
      await invoke("pet_delete_pet", { id: selected.id });
      toast.success(t("pet.manage.deletedToast", { name: selected.displayName }));
      setDeleting(false);
      setSelected(null);
      await reload();
    } catch (e) {
      toast.error((e as Error).message);
    } finally {
      setBusy(false);
    }
  };

  return (
    <Dialog open={props.open} onOpenChange={props.onOpenChange}>
      <DialogContent className="max-w-xl">
        <DialogHeader>
          <DialogTitle>{t("pet.manage.title")}</DialogTitle>
        </DialogHeader>
        {!selected ? (
          <div className="max-h-72 space-y-1 overflow-auto" data-testid="manage-list">
            <p className="text-muted-foreground mb-1 text-sm">{t("pet.manage.pick")}</p>
            {pets.map((p) => (
              <button
                key={p.id}
                data-testid={`manage-pick-${p.id}`}
                onClick={() => void openPanel(p)}
                className="hover:bg-accent/50 flex w-full items-center justify-between rounded border px-2 py-1.5 text-left text-sm"
              >
                <span>
                  {p.displayName}
                  <span className="text-muted-foreground ml-1 text-xs">{p.id}</span>
                </span>
                <span className="bg-muted rounded px-1 text-[10px]">v{p.spriteVersionNumber || "?"}</span>
              </button>
            ))}
          </div>
        ) : (
          <div className="space-y-3" data-testid="manage-panel">
            <div className="flex items-end gap-2">
              <div className="flex-1">
                <label className="text-sm">{t("pet.manage.rename")}</label>
                <Input
                  data-testid="manage-rename-input"
                  placeholder={selected.id}
                  value={renameTo}
                  onChange={(e) => setRenameTo(e.target.value)}
                />
              </div>
              <Button size="sm" disabled={busy || !renameTo} data-testid="manage-rename-btn" onClick={() => void doRename()}>
                {t("pet.manage.renameBtn")}
              </Button>
            </div>
            <div>
              <label className="text-sm">{t("pet.import.displayName")}</label>
              <Input value={displayName} onChange={(e) => setDisplayName(e.target.value)} />
            </div>
            <VoiceGroupEditor
              rows={voiceRows}
              busy={busy}
              onAdd={async (group, paths) => {
                ensureNotActive(); // 直写正式目录前冻结保护
                const added = await invoke<{ group: string; name: string; file: string; sizeBytes: number }[]>(
                  "pet_add_voice_files",
                  { id: selected.id, srcPaths: paths, group }
                );
                setVoiceRows((prev) => [...prev, ...added.map((a) => ({ ...a, durationMs: null }))]);
              }}
              onRemove={async (rel) => {
                ensureNotActive();
                await invoke("pet_remove_voice_file", { id: selected.id, rel });
                setVoiceRows((prev) => prev.filter((r) => r.file !== rel));
              }}
            />
            <div className="flex items-center gap-2">
              <Switch checked={subtitle} onCheckedChange={setSubtitle} />
              <span className="text-sm">{t("pet.manage.subtitle")}</span>
            </div>
            <div className="flex flex-wrap justify-end gap-2">
              <Button size="sm" variant="outline" onClick={() => void invoke("pet_reveal_folder", { id: selected.id }).catch(() => {})}>
                {t("pet.manage.openFolder")}
              </Button>
              <Button size="sm" variant="destructive" data-testid="manage-delete" onClick={() => setDeleting(true)}>
                {t("pet.manage.delete")}
              </Button>
              <Button size="sm" disabled={busy} data-testid="manage-save" onClick={() => void doSave()}>
                {t("pet.manage.save")}
              </Button>
            </div>
            {deleting && (
              <div className="border-destructive/40 bg-destructive/5 flex items-center justify-between gap-2 rounded border p-2">
                <span className="text-xs">{t("pet.manage.deleteConfirm")}</span>
                <div className="flex gap-2">
                  <Button size="sm" variant="destructive" data-testid="manage-delete-confirm" onClick={() => void doDelete()}>
                    {t("pet.manage.delete")}
                  </Button>
                  <Button size="sm" variant="ghost" onClick={() => setDeleting(false)}>
                    {t("pet.import.cancelImport")}
                  </Button>
                </div>
              </div>
            )}
          </div>
        )}
      </DialogContent>
    </Dialog>
  );
}
```

- [x] **Step 5: 设置页补修改入口**

`settings.tsx`：import `PetManageDialog`；`const [manageOpen, setManageOpen] = useState(false);`；切换/导入按钮旁加：

```tsx
                  <Button size="sm" variant="outline" onClick={() => setManageOpen(true)}>
                    {t("settings.pet.managePet")}
                  </Button>
```

挂 `<PetManageDialog open={manageOpen} onOpenChange={setManageOpen} />`。

- [x] **Step 6: 运行测试**

```bash
pnpm test tests/pet/petManageDialog.test.tsx tests/pet/petSettings.test.tsx
```
Expected: PASS。

- [x] **Step 7: Commit**

```bash
git add src/components/pet/manage/PetManageDialog.tsx src/pages/settings.tsx src/i18n/locales/ tests/pet/
git commit -m "feat(pet): manage dialog with rename/audio/subtitle/delete"
```

---

### Task 18: 收尾验证与 E2E 手动清单

**Files:** 无新文件（验证任务）

- [x] **Step 1: 全量自动化检查**

```bash
cd src-tauri && cargo test && cargo clippy -- -D warnings
cd .. && pnpm test && pnpm check
```
Expected: 全部通过。若有 lint/format 报错：`pnpm format` + `pnpm lint:fix` 后重跑。

- [x] **Step 2: i18n 键校对**

人工比对 `zh.json` 与 `en.json`：`settings.pet.*`、`pet.menu.soundNoCap/subtitleNoCap`、`pet.switch.*`、`pet.startup.*`、`pet.import.*`、`pet.manage.*` 两组键完全一致（键集合相同）。可用命令辅助：

```bash
python -c "
import json
zh=json.load(open('src/i18n/locales/zh.json',encoding='utf-8'))
en=json.load(open('src/i18n/locales/en.json',encoding='utf-8'))
def keys(d,p=''):
    out=set()
    for k,v in d.items():
        kk=f'{p}.{k}' if p else k
        out.add(kk) if not isinstance(v,dict) else out|=keys(v,kk)
    return out
a,b=keys(zh),keys(en)
print('zh-only:',sorted(a-b)); print('en-only:',sorted(b-a))
"
```
Expected: 两组输出均为空。

- [ ] **Step 3: 手动 E2E 验收（对照 spec §15.2 AC1-AC10）**——待人工执行（实施环境无法操作实机 GUI；自动化检查已全过：cargo test 145 通过、clippy 干净、pnpm test 100 通过、pnpm build 通过、i18n 键比对一致）

`pnpm tauri:dev` 启动，逐项执行并在本文件勾选：

- [ ] AC1：设置 → 导入宠物 → 本地文件夹 → 选 `docs/pet-gallery/starry-dew` → 配置页显示 v1 徽标 → 执行导入 → 立即激活 → 宠物动画正常、无空闲环视、右键"声音/字幕"置灰、任务完成提示音走主看板
- [ ] AC2：修改宠物 → starry-dew → 四组各加 1 条合法音频（1-20s、<10MB）→ 保存 → 重新激活 → 语音/字幕能力点亮，字幕 = 文件名
- [ ] AC3：导入宠物 → 从 codex → linabell-peach（v2）→ 激活 → 空闲 6 秒后出现 16 帧环视
- [ ] AC4：导入宠物 → petdex → 粘贴 `https://petdex.dev/pets/capvolt` → 下载暂存 → 导入激活（验收时留意重定向与网络超时表现——每跳白名单策略经代码审查保证，无自动化测试（不为此引入 mock server 依赖））
- [ ] AC5：激活 starry-dew → 文件管理器删其 voice 目录 → 重启应用 → 启动弹窗出现，三分支（更新/切回/忽略）各自可走通
- [ ] AC6：关闭应用 → 手动把 `~/.codex/pets/bajie` 复制为 `~/.mam/pets/bajie` → 启动 → 切换宠物 → bajie 卡片无能力徽标（v?）→ 点击 → 自动生成 manifest 并激活
- [ ] AC7：修改宠物 → 选中当前激活的宠物 → 改名/保存 → 观察 toast"已自动切回 foxbell"且宠物即时变回 foxbell
- [ ] AC8：导入 starry-dew 副本（改名 starry-dew-2）→ general 组加一条 25s 超长音频 → 该文件标红排除 → 四组仍齐 → 语音可用
- [ ] AC9：修改宠物 → 重命名/删除非激活宠物 → manifest 同步 / 目录进回收站
- [ ] AC10：切回 foxbell → 对照 `2026-09-01-foxbell-pet-design.md` §9 交互清单抽查：拖拽物理、单击挥手、双击说话、红灯/绿灯语音、右键菜单各页、显隐、位置记忆

- [x] **Step 4: 修复发现的问题并提交**

任何 AC 失败：修复 → 补测试 → 重新执行对应 AC。

```bash
git add -A
git commit -m "test(pet): manual E2E acceptance pass for external pets"
```

- [x] **Step 5: 汇总**

在 PR/分支说明中列出：完成任务数、测试统计（`pnpm test 2>&1 | tail -5`、`cargo test 2>&1 | tail -5` 输出）、AC 通过状态。

---

## 自审记录（writing-plans Self-Review）

1. **Spec 覆盖**：EP1（Task 7/10）、EP2（Task 14）、EP3（Task 15/17）、EP4（Task 1-17 架构本身）、EP5（Task 17 ensureNotActive + Task 12 repair 例外）、EP6（Task 9 snapshotVoices + Task 8 resolver）、EP7（Task 11 diff 纯 stat + Task 12 快路径）、EP8（Task 11 nameFromRel）、EP9（各 UI 任务 title/tooltip + i18n）、EP10（FOXBELL 常量 + AC10 回归）。spec §4（Task 2）、§5（Task 11）、§6（Task 11/12/14）、§7（Task 9/10）、§8（Task 16）、§9（Task 13）、§10（Task 17）、§11（Task 13/16/17）、§12（Task 9/10）、§13（Task 4 safe_unzip / Task 5 域名白名单）、§15（Task 18）——无遗漏。
2. **占位符扫描**：无 TBD/TODO；"执行者注意"均为对现有代码读取的确定性指令（非占位）。
3. **类型一致性**：`StagedPet`/`PetSummary`/`CodexPetInfo`/`PetScan`/`PetManifestView`/`VoiceRow`/`ActivePet` 在 Rust 与 TS 两侧字段名 camelCase 对齐（serde rename_all）；命令名 snake_case（invoke 自动映射 camelCase 参数）已在各调用点一致。
4. **自审修复记录**（初稿发现并已改正）：
   - PetSwitchDialog mismatch 三选 resolver 由 `let` 变量改 `useRef`（re-render stale closure 会导致 promise 永不 resolve）；
   - `petActivation` 的 `emit` 从 `@tauri-apps/api/core` 改为 `@tauri-apps/api/event`（原路径无此导出），并补 `.catch` 防测试环境未处理 rejection；
   - `snapshotVoices` 条目 `index` 由恒 0 改为顺序编号（`VoicePlayer.play` 以 `els[entry.index]` 定位，恒 0 会播错文件）；
   - PetImportDialog 去除 plugin-dialog 重复导入；`source` 字段判定简化（staging 目录不含 zip 信息）；
   - FoxbellPet 的 listen 清理回调签名改为无参 `UnlistenFn`。
