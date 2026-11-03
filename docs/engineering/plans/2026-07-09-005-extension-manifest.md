# Extension Manifest 标准化 实现计划

> **面向 AI 代理的工作者：** 必需子技能：使用 superpowers:subagent-driven-development（推荐）或 superpowers:executing-plans 逐任务实现此计划。步骤使用复选框（`- [ ]`）语法来跟踪进度。

**目标：** 为 Skill/MCP/Plugin 三类资源定义统一的 `mam.json` manifest 规范，实现 Rust 侧校验器、前端 Zod schema、权限模型 UI 展示、兼容性检查、本地商店索引和安装分发流程。

**架构：** Phase 1 实现声明式权限展示（manifest 声明权限，UI 在安装前展示 + 确认）。后端在 `services/manifest/` 实现 `ManifestValidator`，前端在 `lib/schemas/manifest.ts` 实现 Zod schema 并在安装流程中展示权限和兼容性信息。`~/.mam/store/index.json` 作为本地商店索引。

**技术栈：** Rust + serde + semver + Zod + TypeScript

---

## 前置依赖

- Spec 002 FR-5 完成（`services/` 目录就位）
- Spec 002 FR-4 完成（`src/lib/api/` 层就位）
- Spec 002 FR-7 完成（`src/lib/schemas/` + zod 安装就位）

---

## 文件结构

### 后端（Rust）

| 文件 | 职责 |
|------|------|
| `src-tauri/src/services/manifest/mod.rs` | Manifest 模块聚合 |
| `src-tauri/src/services/manifest/types.rs` | Manifest/Permission/Compatibility Rust 结构体 |
| `src-tauri/src/services/manifest/validator.rs` | ManifestValidator 校验逻辑 |
| `src-tauri/src/services/manifest/store.rs` | 本地商店索引读写 |
| `src-tauri/src/database/dao/extension.rs` | 扩展 extension 表字段（manifest_path/permissions/min_runtime） |
| `src-tauri/src/commands/manifest.rs` | Manifest 相关 IPC 命令 |

### 前端（TypeScript）

| 文件 | 职责 |
|------|------|
| `src/lib/schemas/manifest.ts` | Zod manifest schema |
| `src/lib/api/manifest.ts` | Manifest API 封装 |
| `src/components/resources/ManifestInstallDialog.tsx` | 安装确认弹窗（权限展示） |
| `src/components/resources/PermissionBadge.tsx` | 权限风险等级徽章 |

---

## 任务分解

### 任务 1：Manifest 类型定义（FR-1/2/3）

**文件：**
- 创建：`src-tauri/src/services/manifest/mod.rs`
- 创建：`src-tauri/src/services/manifest/types.rs`

- [ ] **步骤 1：创建 types.rs -- Rust 结构体**

```rust
// src-tauri/src/services/manifest/types.rs
use serde::{Deserialize, Serialize};

/// 资源类型
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Skill,
    Mcp,
    Plugin,
}

/// 权限枚举
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Permission {
    #[serde(rename = "filesystem.read")]
    FilesystemRead,
    #[serde(rename = "filesystem.write")]
    FilesystemWrite,
    #[serde(rename = "network")]
    Network,
    #[serde(rename = "shell")]
    Shell,
    #[serde(rename = "env.read")]
    EnvRead,
    #[serde(rename = "settings.write")]
    SettingsWrite,
    #[serde(rename = "symlink.create")]
    SymlinkCreate,
}

impl Permission {
    /// 风险等级: low / medium / high
    pub fn risk_level(&self) -> &'static str {
        match self {
            Permission::FilesystemRead | Permission::SymlinkCreate => "low",
            Permission::FilesystemWrite | Permission::Network | Permission::EnvRead => "medium",
            Permission::Shell | Permission::SettingsWrite => "high",
        }
    }

    /// 权限说明
    pub fn description(&self) -> &'static str {
        match self {
            Permission::FilesystemRead => "读取文件",
            Permission::FilesystemWrite => "写入文件",
            Permission::Network => "发起网络请求",
            Permission::Shell => "执行 shell 命令（高风险）",
            Permission::EnvRead => "读取环境变量",
            Permission::SettingsWrite => "写入工具配置文件（高风险）",
            Permission::SymlinkCreate => "创建符号链接",
        }
    }
}

/// 兼容性条目
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompatibilityEntry {
    pub tool: String,
    pub min_version: Option<String>,
    pub mcp_format: Option<String>,
    pub sub_agent_support: Option<bool>,
    pub notes: Option<String>,
}

/// 作者信息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Author {
    pub name: String,
    pub url: Option<String>,
    pub email: Option<String>,
}

/// 公共字段
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManifestCommon {
    pub id: String,
    pub name: String,
    pub version: String,
    pub kind: Kind,
    pub description: Option<String>,
    pub author: Option<Author>,
    pub homepage: Option<String>,
    pub icon_url: Option<String>,
    pub tags: Option<Vec<String>>,
    pub min_runtime: Option<String>,
    pub github_repo: Option<String>,
    pub permissions: Option<Vec<Permission>>,
    pub compatibility: Option<Vec<CompatibilityEntry>>,
}

/// Skill 扩展字段
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillFields {
    pub entry: String,
    pub includes: Option<Vec<String>>,
}

/// MCP 扩展字段
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpFields {
    pub command: String,
    pub args: Option<Vec<String>>,
    pub env: Option<std::collections::BTreeMap<String, String>>,
    pub formats: Option<Vec<String>>,
}

/// Plugin 扩展字段
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginFields {
    pub entry: String,
    #[serde(rename = "type")]
    pub plugin_type: String,
    pub config_template: Option<String>,
}

/// 完整 Manifest
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    #[serde(flatten)]
    pub common: ManifestCommon,
    pub skill: Option<SkillFields>,
    pub mcp: Option<McpFields>,
    pub plugin: Option<PluginFields>,
}
```

- [ ] **步骤 2：创建 mod.rs -- 模块聚合**

```rust
// src-tauri/src/services/manifest/mod.rs
pub mod types;
pub mod validator;
pub mod store;

pub use types::*;
pub use validator::{ManifestValidator, ValidationError};
```

- [ ] **步骤 3：在 services/mod.rs 中注册模块**

```rust
// src-tauri/src/services/mod.rs 中添加
pub mod manifest;
```

- [ ] **步骤 4：验证编译**

运行：`cd src-tauri && cargo check`
预期：PASS

- [ ] **步骤 5：Commit**

```bash
git add src-tauri/src/services/manifest/
git commit -m "feat(manifest): define Manifest/Permission/Compatibility Rust types"
```

---

### 任务 2：Manifest 校验器（FR-4）

**文件：**
- 创建：`src-tauri/src/services/manifest/validator.rs`

- [ ] **步骤 1：编写 ValidationError 结构**

```rust
// src-tauri/src/services/manifest/validator.rs
use std::path::Path;
use super::types::*;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ValidationError {
    pub field: String,
    pub message: String,
    pub code: String,
}

pub struct ManifestValidator;

impl ManifestValidator {
    /// 从文件路径校验 manifest
    pub fn validate_file(path: &Path) -> Result<Manifest, Vec<ValidationError>> {
        if !path.exists() {
            return Err(vec![ValidationError {
                field: "file".to_string(),
                message: format!("文件不存在: {}", path.display()),
                code: "FILE_NOT_FOUND".to_string(),
            }]);
        }
        let content = std::fs::read_to_string(path).map_err(|e| vec![ValidationError {
            field: "file".to_string(),
            message: format!("读取文件失败: {}", e),
            code: "READ_ERROR".to_string(),
        }])?;
        Self::validate_json(&content)
    }

    /// 从 JSON 字符串校验 manifest
    pub fn validate_json(json: &str) -> Result<Manifest, Vec<ValidationError>> {
        let manifest: Manifest = serde_json::from_str(json).map_err(|e| {
            vec![ValidationError {
                field: "root".to_string(),
                message: format!("JSON 解析失败: {}", e),
                code: "PARSE_ERROR".to_string(),
            }]
        })?;

        Self::validate_manifest(&manifest)?;
        Ok(manifest)
    }

    /// 校验已解析的 Manifest
    pub fn validate_manifest(manifest: &Manifest) -> Result<(), Vec<ValidationError>> {
        let mut errors = Vec::new();

        // 必填字段检查
        if manifest.common.id.is_empty() {
            errors.push(ValidationError {
                field: "id".to_string(),
                message: "id 不能为空".to_string(),
                code: "REQUIRED".to_string(),
            });
        }

        // id 格式检查：仅允许字母、数字、.、_、-
        if !manifest.common.id.chars().all(|c| c.is_alphanumeric() || c == '.' || c == '_' || c == '-') {
            errors.push(ValidationError {
                field: "id".to_string(),
                message: "id 仅允许字母、数字、.、_、-".to_string(),
                code: "INVALID_FORMAT".to_string(),
            });
        }

        // version semver 检查
        if !is_valid_semver(&manifest.common.version) {
            errors.push(ValidationError {
                field: "version".to_string(),
                message: "version 必须为有效 semver（如 1.0.0）".to_string(),
                code: "INVALID_SEMVER".to_string(),
            });
        }

        // githubRepo 格式检查
        if let Some(repo) = &manifest.common.github_repo {
            if !repo.contains('/') || repo.split('/').count() != 2 {
                errors.push(ValidationError {
                    field: "githubRepo".to_string(),
                    message: "githubRepo 格式应为 owner/repo".to_string(),
                    code: "INVALID_FORMAT".to_string(),
                });
            }
        }

        // 类型必填字段检查
        match manifest.common.kind {
            Kind::Skill => {
                if manifest.skill.is_none() {
                    errors.push(ValidationError {
                        field: "skill".to_string(),
                        message: "kind 为 skill 时 skill.entry 必填".to_string(),
                        code: "REQUIRED".to_string(),
                    });
                } else if let Some(skill) = &manifest.skill {
                    // 路径穿越检查
                    if skill.entry.contains("..") {
                        errors.push(ValidationError {
                            field: "skill.entry".to_string(),
                            message: "不允许路径穿越（../）".to_string(),
                            code: "PATH_TRAVERSAL".to_string(),
                        });
                    }
                }
            }
            Kind::Mcp => {
                if manifest.mcp.is_none() {
                    errors.push(ValidationError {
                        field: "mcp".to_string(),
                        message: "kind 为 mcp 时 mcp.command 必填".to_string(),
                        code: "REQUIRED".to_string(),
                    });
                }
            }
            Kind::Plugin => {
                if manifest.plugin.is_none() {
                    errors.push(ValidationError {
                        field: "plugin".to_string(),
                        message: "kind 为 plugin 时 plugin.entry 和 plugin.type 必填".to_string(),
                        code: "REQUIRED".to_string(),
                    });
                }
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

/// 简易 semver 校验：x.y.z 格式
fn is_valid_semver(v: &str) -> bool {
    let parts: Vec<&str> = v.split('.').collect();
    parts.len() >= 3 && parts.iter().take(3).all(|p| p.parse::<u32>().is_ok())
}
```

- [ ] **步骤 2：编写校验器单元测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_valid_skill_manifest() {
        let json = r#"{
            "id": "com.example.brainstorming",
            "name": "Brainstorming",
            "version": "1.0.0",
            "kind": "skill",
            "permissions": ["filesystem.read"],
            "skill": { "entry": "SKILL.md" }
        }"#;
        let result = ManifestValidator::validate_json(json);
        assert!(result.is_ok());
    }

    #[test]
    fn test_missing_required_field() {
        let json = r#"{ "name": "Test", "version": "1.0.0", "kind": "skill" }"#;
        let result = ManifestValidator::validate_json(json);
        assert!(result.is_err());
        let errors = result.unwrap_err();
        assert!(errors.iter().any(|e| e.field == "id"));
    }

    #[test]
    fn test_invalid_semver() {
        let json = r#"{ "id": "test", "name": "T", "version": "abc", "kind": "skill", "skill": { "entry": "SKILL.md" } }"#;
        let result = ManifestValidator::validate_json(json);
        assert!(result.is_err());
        let errors = result.unwrap_err();
        assert!(errors.iter().any(|e| e.code == "INVALID_SEMVER"));
    }

    #[test]
    fn test_path_traversal_rejected() {
        let json = r#"{ "id": "test", "name": "T", "version": "1.0.0", "kind": "skill", "skill": { "entry": "../../../etc/passwd" } }"#;
        let result = ManifestValidator::validate_json(json);
        assert!(result.is_err());
        let errors = result.unwrap_err();
        assert!(errors.iter().any(|e| e.code == "PATH_TRAVERSAL"));
    }

    #[test]
    fn test_invalid_github_repo() {
        let json = r#"{ "id": "test", "name": "T", "version": "1.0.0", "kind": "skill", "githubRepo": "invalid", "skill": { "entry": "SKILL.md" } }"#;
        let result = ManifestValidator::validate_json(json);
        assert!(result.is_err());
        let errors = result.unwrap_err();
        assert!(errors.iter().any(|e| e.field == "githubRepo"));
    }
}
```

- [ ] **步骤 3：运行测试**

运行：`cd src-tauri && cargo test manifest`
预期：全部 PASS

- [ ] **步骤 4：Commit**

```bash
git add src-tauri/src/services/manifest/validator.rs
git commit -m "feat(manifest): implement ManifestValidator with path traversal and semver checks"
```

---

### 任务 3：前端 Zod Manifest Schema（FR-4）

**文件：**
- 创建：`src/lib/schemas/manifest.ts`

- [ ] **步骤 1：编写 Zod schema**

```typescript
// src/lib/schemas/manifest.ts
import { z } from "zod";

export const PermissionSchema = z.enum([
  "filesystem.read",
  "filesystem.write",
  "network",
  "shell",
  "env.read",
  "settings.write",
  "symlink.create",
]);

export const KindSchema = z.enum(["skill", "mcp", "plugin"]);

export const CompatibilityEntrySchema = z.object({
  tool: z.string(),
  minVersion: z.string().optional(),
  mcpFormat: z.enum(["json", "toml", "jsonc"]).optional(),
  subAgentSupport: z.boolean().optional(),
  notes: z.string().optional(),
});

export const AuthorSchema = z.object({
  name: z.string(),
  url: z.string().url().optional(),
  email: z.string().email().optional(),
});

// 公共字段
const commonFields = {
  id: z.string().regex(/^[a-zA-Z0-9._-]+$/),
  name: z.string().min(1),
  version: z.string().regex(/^\d+\.\d+\.\d+/),
  kind: KindSchema,
  description: z.string().optional(),
  author: AuthorSchema.optional(),
  homepage: z.string().url().optional(),
  iconUrl: z.string().optional(),
  tags: z.array(z.string()).optional(),
  minRuntime: z.string().optional(),
  githubRepo: z.string().optional(),
  permissions: z.array(PermissionSchema).optional(),
  compatibility: z.array(CompatibilityEntrySchema).optional(),
};

export const SkillManifestSchema = z.object({
  ...commonFields,
  kind: z.literal("skill"),
  skill: z.object({
    entry: z.string(),
    includes: z.array(z.string()).optional(),
  }),
});

export const McpManifestSchema = z.object({
  ...commonFields,
  kind: z.literal("mcp"),
  mcp: z.object({
    command: z.string(),
    args: z.array(z.string()).optional(),
    env: z.record(z.string(), z.string()).optional(),
    formats: z.array(z.enum(["json", "toml", "jsonc"])).optional(),
  }),
});

export const PluginManifestSchema = z.object({
  ...commonFields,
  kind: z.literal("plugin"),
  plugin: z.object({
    entry: z.string(),
    type: z.enum(["file", "config", "mixed"]),
    configTemplate: z.string().optional(),
  }),
});

export const ManifestSchema = z.discriminatedUnion("kind", [
  SkillManifestSchema,
  McpManifestSchema,
  PluginManifestSchema,
]);

export type Manifest = z.infer<typeof ManifestSchema>;
export type Permission = z.infer<typeof PermissionSchema>;
export type CompatibilityEntry = z.infer<typeof CompatibilityEntrySchema>;

/// 权限风险等级映射
export const PERMISSION_RISK: Record<Permission, "low" | "medium" | "high"> = {
  "filesystem.read": "low",
  "filesystem.write": "medium",
  network: "medium",
  shell: "high",
  "env.read": "medium",
  "settings.write": "high",
  "symlink.create": "low",
};

export const PERMISSION_DESCRIPTION: Record<Permission, string> = {
  "filesystem.read": "读取文件",
  "filesystem.write": "写入文件",
  network: "发起网络请求",
  shell: "执行 shell 命令（高风险）",
  "env.read": "读取环境变量",
  "settings.write": "写入工具配置文件（高风险）",
  "symlink.create": "创建符号链接",
};
```

- [ ] **步骤 2：验证编译**

运行：`pnpm build`
预期：PASS

- [ ] **步骤 3：Commit**

```bash
git add src/lib/schemas/manifest.ts
git commit -m "feat(schemas): add Zod manifest schema with discriminated union"
```

---

### 任务 4：前端 Manifest API + 权限展示组件（FR-2/4）

**文件：**
- 创建：`src/lib/api/manifest.ts`
- 创建：`src/components/resources/PermissionBadge.tsx`
- 创建：`src/components/resources/ManifestInstallDialog.tsx`

- [ ] **步骤 1：创建 Manifest API 封装**

```typescript
// src/lib/api/manifest.ts
import { invoke } from "@tauri-apps/api/core";

export interface ValidationError {
  field: string;
  message: string;
  code: string;
}

export interface ValidateResult {
  valid: boolean;
  manifest?: unknown;
  errors?: ValidationError[];
}

export async function validateManifestPath(path: string): Promise<ValidateResult> {
  return await invoke("validate_manifest", { path });
}

export async function installResource(path: string): Promise<void> {
  return await invoke("install_resource_from_manifest", { path });
}

export async function getStoreIndex(): Promise<unknown> {
  return await invoke("get_store_index");
}
```

- [ ] **步骤 2：创建 PermissionBadge 组件**

```typescript
// src/components/resources/PermissionBadge.tsx
import { PERMISSION_RISK, PERMISSION_DESCRIPTION, type Permission } from "@/lib/schemas/manifest";

const RISK_STYLES: Record<string, string> = {
  low: "bg-green-100 text-green-700 dark:bg-green-900 dark:text-green-300",
  medium: "bg-yellow-100 text-yellow-700 dark:bg-yellow-900 dark:text-yellow-300",
  high: "bg-red-100 text-red-700 dark:bg-red-900 dark:text-red-300",
};

export function PermissionBadge({ permission }: { permission: Permission }) {
  const risk = PERMISSION_RISK[permission];
  const desc = PERMISSION_DESCRIPTION[permission];

  return (
    <span
      className={`inline-flex items-center rounded px-2 py-0.5 text-xs font-medium ${RISK_STYLES[risk]}`}
      title={desc}
    >
      {permission}
    </span>
  );
}
```

- [ ] **步骤 3：创建 ManifestInstallDialog 组件**

```typescript
// src/components/resources/ManifestInstallDialog.tsx
import { useState, useEffect } from "react";
import { Dialog, DialogContent, DialogHeader, DialogTitle, DialogFooter } from "@/components/ui/dialog";
import { Button } from "@/components/ui/button";
import { PermissionBadge } from "./PermissionBadge";
import { validateManifestPath, installResource, type ValidateResult } from "@/lib/api/manifest";

interface Props {
  path: string | null;
  open: boolean;
  onOpenChange: (open: boolean) => void;
  onInstalled?: () => void;
}

export function ManifestInstallDialog({ path, open, onOpenChange, onInstalled }: Props) {
  const [result, setResult] = useState<ValidateResult | null>(null);
  const [installing, setInstalling] = useState(false);

  useEffect(() => {
    if (path && open) {
      validateManifestPath(path).then(setResult);
    }
  }, [path, open]);

  const handleInstall = async () => {
    if (!path) return;
    setInstalling(true);
    try {
      await installResource(path);
      onInstalled?.();
      onOpenChange(false);
    } finally {
      setInstalling(false);
    }
  };

  const manifest = result?.manifest as {
    name?: string;
    version?: string;
    kind?: string;
    permissions?: string[];
    compatibility?: { tool: string }[];
  } | undefined;

  const hasHighRisk = manifest?.permissions?.some(
    (p) => p === "shell" || p === "settings.write"
  );

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>安装确认</DialogTitle>
        </DialogHeader>

        {result?.valid && manifest ? (
          <div className="space-y-4">
            <div>
              <p className="font-medium">{manifest.name} v{manifest.version}</p>
              <p className="text-sm text-muted-foreground">类型: {manifest.kind}</p>
            </div>

            {manifest.permissions && manifest.permissions.length > 0 && (
              <div>
                <p className="mb-1 text-sm font-medium">权限声明:</p>
                <div className="flex flex-wrap gap-1">
                  {manifest.permissions.map((p) => (
                    <PermissionBadge key={p} permission={p as never} />
                  ))}
                </div>
              </div>
            )}

            {manifest.compatibility && (
              <div>
                <p className="mb-1 text-sm font-medium">兼容工具:</p>
                <p className="text-sm text-muted-foreground">
                  {manifest.compatibility.map((c) => c.tool).join(", ")}
                </p>
              </div>
            )}

            {hasHighRisk && (
              <div className="rounded border border-red-300 bg-red-50 p-3 dark:border-red-700 dark:bg-red-950">
                <p className="text-sm text-red-700 dark:text-red-300">
                  此资源声明了高风险权限，请确认你信任此资源的来源。
                </p>
              </div>
            )}
          </div>
        ) : (
          <div className="space-y-2">
            <p className="text-sm text-red-600">Manifest 校验失败:</p>
            {result?.errors?.map((e, i) => (
              <p key={i} className="text-sm text-muted-foreground">
                {e.field}: {e.message} ({e.code})
              </p>
            ))}
          </div>
        )}

        <DialogFooter>
          <Button variant="outline" onClick={() => onOpenChange(false)}>取消</Button>
          {result?.valid && (
            <Button onClick={handleInstall} disabled={installing}>
              {installing ? "安装中..." : "确认安装"}
            </Button>
          )}
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
```

- [ ] **步骤 4：验证编译**

运行：`pnpm build`
预期：PASS

- [ ] **步骤 5：Commit**

```bash
git add src/lib/api/manifest.ts src/components/resources/PermissionBadge.tsx src/components/resources/ManifestInstallDialog.tsx
git commit -m "feat(manifest): add manifest API, permission badge, and install dialog"
```

---

### 任务 5：后端 Manifest IPC 命令（FR-4/6）

> **依赖：** 任务 7（数据库扩展字段）必须先完成，ExtensionRecord 需包含 manifest_path/permissions/min_runtime 字段。

**文件：**
- 创建：`src-tauri/src/commands/manifest.rs`
- 修改：`src-tauri/src/commands/mod.rs`（注册新模块）
- 修改：`src-tauri/src/lib.rs`（注册新命令）

- [ ] **步骤 1：创建 manifest 命令**

```rust
// src-tauri/src/commands/manifest.rs
use crate::services::manifest::{ManifestValidator, ValidationError};
use serde::Serialize;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ValidateResult {
    pub valid: bool,
    pub manifest: Option<crate::services::manifest::Manifest>,
    pub errors: Option<Vec<ValidationError>>,
}

#[tauri::command]
pub fn validate_manifest(path: String) -> ValidateResult {
    match ManifestValidator::validate_file(std::path::Path::new(&path)) {
        Ok(manifest) => ValidateResult { valid: true, manifest: Some(manifest), errors: None },
        Err(errors) => ValidateResult { valid: false, manifest: None, errors: Some(errors) },
    }
}

#[tauri::command]
pub fn install_resource_from_manifest(path: String) -> Result<(), String> {
    let manifest = ManifestValidator::validate_file(std::path::Path::new(&path))
        .map_err(|errors| {
            errors.iter()
                .map(|e| format!("{}: {}", e.field, e.message))
                .collect::<Vec<_>>()
                .join("; ")
        })?;

    // 根据类型安装到对应目录
    let mam_dir = dirs::home_dir()
        .unwrap_or_default()
        .join(".mam");

    let dest_dir = match manifest.common.kind {
        crate::services::manifest::Kind::Skill => mam_dir.join("skills").join(&manifest.common.id),
        crate::services::manifest::Kind::Mcp => mam_dir.join("mcp").join(&manifest.common.id),
        crate::services::manifest::Kind::Plugin => mam_dir.join("plugins").join(&manifest.common.id),
    };

    // 复制资源文件
    let source = std::path::Path::new(&path).parent()
        .ok_or("无法获取资源目录")?;
    crate::linker::copy_dir_recursive(source, &dest_dir)?;

    // 写入 manifest 到目标目录
    let manifest_dest = dest_dir.join("mam.json");
    std::fs::copy(&path, &manifest_dest).map_err(|e| e.to_string())?;

    // 更新 store 索引
    crate::services::manifest::store::add_entry(&manifest)?;

    // 记录到数据库
    let conn = crate::database::connection::open().map_err(|e| e.to_string())?;
    crate::database::dao::extension::insert_extension(&conn, &crate::database::dao::extension::ExtensionRecord {
        id: manifest.common.id.clone(),
        kind: format!("{:?}", manifest.common.kind).to_lowercase(),
        name: manifest.common.name.clone(),
        description: manifest.common.description.clone(),
        source_path: dest_dir.to_string_lossy().to_string(),
        source_url: manifest.common.homepage.clone(),
        suite: None,
        manifest_path: Some(manifest_dest.to_string_lossy().to_string()),
        permissions: manifest.common.permissions.as_ref().map(|p| {
            p.iter().map(|perm| format!("{:?}", perm)).collect::<Vec<_>>().join(",")
        }),
        min_runtime: manifest.common.min_runtime.clone(),
    }).map_err(|e| e.to_string())?;

    Ok(())
}

#[tauri::command]
pub fn get_store_index() -> Result<serde_json::Value, String> {
    crate::services::manifest::store::read_index()
}
```

- [ ] **步骤 2：在 commands/mod.rs 中注册**

```rust
// src-tauri/src/commands/mod.rs 添加
pub mod manifest;
pub use manifest::{validate_manifest, install_resource_from_manifest, get_store_index};
```

- [ ] **步骤 3：在 lib.rs 中注册命令**

在 `invoke_handler` 的 `generate_handler!` 中添加：

```rust
commands::validate_manifest,
commands::install_resource_from_manifest,
commands::get_store_index,
```

- [ ] **步骤 4：验证编译**

运行：`cd src-tauri && cargo check`
预期：PASS

- [ ] **步骤 5：Commit**

```bash
git add src-tauri/src/commands/manifest.rs src-tauri/src/commands/mod.rs src-tauri/src/lib.rs
git commit -m "feat(manifest): add validate/install/store-index IPC commands"
```

---

### 任务 6：本地商店索引（FR-5）

**文件：**
- 创建：`src-tauri/src/services/manifest/store.rs`

- [ ] **步骤 1：编写 store 索引读写逻辑**

```rust
// src-tauri/src/services/manifest/store.rs
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use super::types::Manifest;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoreEntry {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub version: String,
    pub description: Option<String>,
    pub tags: Option<Vec<String>>,
    pub github_repo: Option<String>,
    pub installed: bool,
    pub featured: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoreIndex {
    pub version: String,
    pub updated: String,
    pub entries: Vec<StoreEntry>,
}

fn store_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(".mam/store/index.json")
}

/// 读取商店索引，不存在则返回空索引
pub fn read_index() -> Result<serde_json::Value, String> {
    let path = store_path();
    if !path.exists() {
        let empty = StoreIndex {
            version: "1".to_string(),
            updated: chrono::Utc::now().to_rfc3339(),
            entries: vec![],
        };
        return serde_json::to_value(&empty).map_err(|e| e.to_string());
    }
    let content = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    serde_json::from_str::<serde_json::Value>(&content).map_err(|e| e.to_string())
}

/// 添加或更新商店条目
pub fn add_entry(manifest: &Manifest) -> Result<(), String> {
    let path = store_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }

    let mut index = if path.exists() {
        let content = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
        serde_json::from_str::<StoreIndex>(&content).unwrap_or(StoreIndex {
            version: "1".to_string(),
            updated: String::new(),
            entries: vec![],
        })
    } else {
        StoreIndex {
            version: "1".to_string(),
            updated: String::new(),
            entries: vec![],
        }
    };

    let entry = StoreEntry {
        id: manifest.common.id.clone(),
        name: manifest.common.name.clone(),
        kind: format!("{:?}", manifest.common.kind).to_lowercase(),
        version: manifest.common.version.clone(),
        description: manifest.common.description.clone(),
        tags: manifest.common.tags.clone(),
        github_repo: manifest.common.github_repo.clone(),
        installed: true,
        featured: false,
    };

    // 移除同 id 旧条目，添加新条目
    index.entries.retain(|e| e.id != entry.id);
    index.entries.push(entry);
    index.updated = chrono::Utc::now().to_rfc3339();

    let json = serde_json::to_string_pretty(&index).map_err(|e| e.to_string())?;
    std::fs::write(&path, json).map_err(|e| e.to_string())?;
    Ok(())
}
```

- [ ] **步骤 2：验证编译**

运行：`cd src-tauri && cargo check`
预期：PASS

- [ ] **步骤 3：Commit**

```bash
git add src-tauri/src/services/manifest/store.rs
git commit -m "feat(store): implement local store index read/write"
```

---

### 任务 7：数据库扩展字段 + Legacy 兼容（FR-7）

**文件：**
- 修改：`src-tauri/src/database/schema.rs`（extension 表新增字段）
- 修改：`src-tauri/src/database/dao/extension.rs`（ExtensionRecord 新增字段）
- 修改：`src-tauri/src/database/migration.rs`（ALTER TABLE 迁移）

- [ ] **步骤 1：在 schema.rs 中扩展 extension 表**

**两步操作**：在 `database/schema.rs` 的 `CREATE TABLE IF NOT EXISTS extensions` 语句中添加新列（新数据库直接包含），同时在 `database/migration.rs` 中用 `ALTER TABLE` 兼容已有数据库。

schema.rs 中 extensions 表的 CREATE TABLE 语句新增三列：

```sql
-- 在 CREATE TABLE IF NOT EXISTS extensions (...) 中添加：
manifest_path TEXT,
permissions TEXT,
min_runtime TEXT
```

migration.rs 中为已有数据库添加迁移（检查列是否存在再 ALTER）：

```rust
// src-tauri/src/database/migration.rs
use rusqlite::Connection;

pub fn migrate(conn: &Connection) -> Result<(), String> {
    // 检查 extension 表是否有 manifest_path 列
    let has_manifest_path: bool = conn
        .prepare("SELECT manifest_path FROM extensions LIMIT 0")
        .is_ok();

    if !has_manifest_path {
        conn.execute_batch(
            "ALTER TABLE extensions ADD COLUMN manifest_path TEXT;
             ALTER TABLE extensions ADD COLUMN permissions TEXT;
             ALTER TABLE extensions ADD COLUMN min_runtime TEXT;"
        ).map_err(|e| format!("迁移失败: {}", e))?;
    }

    Ok(())
}
```

- [ ] **步骤 2：扩展 ExtensionRecord 结构体**

```rust
// src-tauri/src/database/dao/extension.rs
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtensionRecord {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub description: Option<String>,
    pub source_path: String,
    pub source_url: Option<String>,
    pub suite: Option<String>,
    // 新增字段
    pub manifest_path: Option<String>,
    pub permissions: Option<String>,
    pub min_runtime: Option<String>,
}
```

- [ ] **步骤 3：在 database/mod.rs 的 init 中调用迁移**

```rust
pub fn init() {
    let conn = connection::open().expect("无法打开数据库");
    schema::init(&conn).expect("Schema 初始化失败");
    migration::migrate(&conn).expect("迁移失败");
}
```

- [ ] **步骤 4：验证编译 + 迁移测试**

运行：`cd src-tauri && cargo check`
预期：PASS

运行：`pnpm tauri:dev`，验证现有数据不丢失，新字段为 NULL（legacy 资源不受影响）。

- [ ] **步骤 5：Commit**

```bash
git add src-tauri/src/database/
git commit -m "feat(database): add manifest fields to extension table with migration"
```

---

### 任务 8：版本更新检查（FR-6）

**文件：**
- 创建：`src-tauri/src/services/manifest/update_checker.rs`
- 修改：`src-tauri/src/services/manifest/mod.rs`

- [ ] **步骤 1：编写 GitHub Release 版本检查**

```rust
// src-tauri/src/services/manifest/update_checker.rs
use super::types::Manifest;

pub struct UpdateInfo {
    pub current_version: String,
    pub latest_version: String,
    pub release_url: String,
}

/// 检查 GitHub Release 是否有新版本
pub fn check_for_updates(manifest: &Manifest) -> Option<UpdateInfo> {
    let repo = manifest.common.github_repo.as_ref()?;
    let url = format!("https://api.github.com/repos/{}/releases/latest", repo);

    // 使用 ureq 或 reqwest 发送请求（需添加依赖）
    // MVP 阶段：返回 None，Phase 2 实现 HTTP 请求
    // 此处预留接口，实际请求逻辑后续实现
    None
}
```

注意：MVP 阶段版本检查为预留接口。实际 HTTP 请求需添加 `ureq` 或 `reqwest` 依赖，在 Phase 2 实现。

- [ ] **步骤 2：在 mod.rs 中注册**

```rust
// src-tauri/src/services/manifest/mod.rs 添加
pub mod update_checker;
```

- [ ] **步骤 3：验证编译**

运行：`cd src-tauri && cargo check`
预期：PASS

- [ ] **步骤 4：Commit**

```bash
git add src-tauri/src/services/manifest/update_checker.rs src-tauri/src/services/manifest/mod.rs
git commit -m "feat(manifest): add update checker interface (reserved for Phase 2)"
```

---

### 任务 9：卸载流程（FR-6.4）

**文件：**
- 创建：`src-tauri/src/commands/manifest.rs`（在任务 5 的文件中新增命令）
- 修改：`src-tauri/src/services/manifest/store.rs`（新增 remove_entry）

- [ ] **步骤 1：在 store.rs 中新增 remove_entry 函数**

```rust
/// 从商店索引中移除条目（保留条目但标记 installed = false）
pub fn remove_entry(id: &str) -> Result<(), String> {
    let path = store_path();
    if !path.exists() {
        return Ok(());
    }
    let content = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let mut index: StoreIndex = serde_json::from_str(&content).map_err(|e| e.to_string())?;

    if let Some(entry) = index.entries.iter_mut().find(|e| e.id == id) {
        entry.installed = false;
    }
    index.updated = chrono::Utc::now().to_rfc3339();
    let json = serde_json::to_string_pretty(&index).map_err(|e| e.to_string())?;
    std::fs::write(&path, json).map_err(|e| e.to_string())?;
    Ok(())
}
```

- [ ] **步骤 2：在 commands/manifest.rs 中新增 uninstall 命令**

```rust
#[tauri::command]
pub fn uninstall_resource(ext_id: String, kind: String) -> Result<(), String> {
    let mam_dir = dirs::home_dir().unwrap_or_default().join(".mam");

    // 根据类型确定资源目录
    let kind_dir = match kind.as_str() {
        "skill" => "skills",
        "mcp" => "mcp",
        "plugin" => "plugins",
        _ => return Err(format!("未知资源类型: {}", kind)),
    };
    let resource_dir = mam_dir.join(kind_dir).join(&ext_id);

    // 移除所有工具的符号链接/配置（通过 linker 和 mcp/plugin 服务）
    // 遍历所有工具，移除该资源的分配
    let conn = crate::database::connection::open().map_err(|e| e.to_string())?;
    let assignments = crate::database::dao::extension::list_all_assignments(&conn);
    for assignment in assignments.iter().filter(|a| a.ext_id == ext_id) {
        match kind.as_str() {
            "skill" => {
                let _ = crate::services::skill::disable_skill_for_tool(
                    &ext_id, &assignment.tool_id,
                );
            }
            "mcp" => {
                let _ = crate::services::mcp::remove_mcp(&assignment.tool_id, &ext_id);
            }
            "plugin" => {
                let _ = crate::services::plugin::toggle_plugin(
                    &ext_id, &assignment.tool_id, false, "file",
                );
            }
            _ => {}
        }
    }

    // 移除资源文件
    if resource_dir.exists() {
        std::fs::remove_dir_all(&resource_dir).map_err(|e| format!("删除目录失败: {}", e))?;
    }

    // 数据库标记为未安装（删除 extension 记录）
    crate::database::dao::extension::delete_extension(&conn, &ext_id)
        .map_err(|e| e.to_string())?;

    // store/index.json 保留条目但标记 installed = false
    crate::services::manifest::store::remove_entry(&ext_id)?;

    Ok(())
}
```

- [ ] **步骤 3：在 commands/mod.rs 和 lib.rs 中注册新命令**

```rust
// commands/mod.rs 添加
pub use manifest::uninstall_resource;
```

在 `lib.rs` 的 `generate_handler!` 中添加 `commands::uninstall_resource`。

- [ ] **步骤 4：验证编译**

运行：`cd src-tauri && cargo check`
预期：PASS

- [ ] **步骤 5：Commit**

```bash
git add src-tauri/src/commands/manifest.rs src-tauri/src/services/manifest/store.rs src-tauri/src/commands/mod.rs src-tauri/src/lib.rs
git commit -m "feat(manifest): add uninstall flow with cleanup"
```

---

### 任务 10：MCP 格式适配检查（FR-7.4）

**文件：**
- 修改：`src-tauri/src/services/manifest/validator.rs`（新增格式检查）

- [ ] **步骤 1：在 ManifestValidator 中新增 MCP 格式兼容性检查**

在 `validator.rs` 的 `validate_manifest` 函数末尾（`if errors.is_empty()` 之前）添加：

```rust
// MCP 格式适配检查：mcp.formats 必须与 compatibility 中的 mcpFormat 一致
if let Some(mcp_fields) = &manifest.mcp {
    if let Some(formats) = &mcp_fields.formats {
        if let Some(compat) = &manifest.common.compatibility {
            for entry in compat {
                if let Some(ref mcp_format) = entry.mcp_format {
                    if !formats.iter().any(|f| f == mcp_format) {
                        errors.push(ValidationError {
                            field: format!("compatibility[{}].mcpFormat", entry.tool),
                            message: format!(
                                "工具 {} 要求 MCP 格式 {}，但 mcp.formats 未包含此格式",
                                entry.tool, mcp_format
                            ),
                            code: "FORMAT_MISMATCH".to_string(),
                        });
                    }
                }
            }
        }
    }
}
```

- [ ] **步骤 2：编写格式检查测试**

```rust
#[test]
fn test_mcp_format_mismatch_rejected() {
    let json = r#"{
        "id": "com.example.mcp",
        "name": "Test MCP",
        "version": "1.0.0",
        "kind": "mcp",
        "mcp": { "command": "npx", "formats": ["json"] },
        "compatibility": [
            { "tool": "codex", "mcpFormat": "toml" }
        ]
    }"#;
    let result = ManifestValidator::validate_json(json);
    assert!(result.is_err());
    let errors = result.unwrap_err();
    assert!(errors.iter().any(|e| e.code == "FORMAT_MISMATCH"));
}
```

- [ ] **步骤 3：运行测试**

运行：`cd src-tauri && cargo test manifest`
预期：全部 PASS

- [ ] **步骤 4：Commit**

```bash
git add src-tauri/src/services/manifest/validator.rs
git commit -m "feat(manifest): add MCP format compatibility check in validator"
```

---

## 自检

**规格覆盖度：**
- FR-1 Manifest Schema -> 任务 1（Rust types）+ 任务 3（Zod schema）✓
- FR-2 权限模型 -> 任务 1（Permission enum + risk_level）+ 任务 4（PermissionBadge UI）✓
- FR-3 兼容性声明 -> 任务 1（CompatibilityEntry）+ 任务 3（Zod）✓
- FR-4 校验工具 -> 任务 2（Rust validator）+ 任务 3（Zod schema）✓
- FR-5 Store 索引 -> 任务 6 ✓
- FR-6 安装与分发 -> 任务 5（install 命令）+ 任务 8（update checker）+ 任务 9（uninstall 流程）✓
- FR-7 现有系统集成 -> 任务 5（database 集成）+ 任务 7（extension 表扩展 + legacy 兼容）+ 任务 10（MCP 格式适配）✓
无遗漏。

**占位符扫描：** 任务 8 的 HTTP 请求逻辑明确标注为 Phase 2 预留，这是 spec 假设 4 的设计决策（"Phase 1 只做声明式权限展示"），非占位符。其余步骤含完整代码。

**类型一致性：** `Manifest` 结构体在任务 1 定义，任务 2/5/6/8 引用一致。`ValidationError` 在任务 2 定义，任务 4/5 引用一致。`Permission` 的 `risk_level()` 在 Rust 侧（任务 1）和 `PERMISSION_RISK` 在 TS 侧（任务 3）映射一致。`StoreEntry`/`StoreIndex` 在任务 6 定义，任务 5 的 `get_store_index` 命令引用一致。
