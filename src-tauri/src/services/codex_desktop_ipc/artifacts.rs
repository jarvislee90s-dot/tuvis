//! 结构化附件抽取：只读任务 1 已核验的字段，不读文件、不解析工具输出、不解码或
//! 复制 base64／`fileId`。
//!
//! 字段证据：macOS `26.1013.40934` 静态映射（见
//! `docs/engineering/evidence/codex-desktop-ipc/2026-10-20-codex-artifact-field-evidence.md`）。
//! 状态枚举、`change.kind` 取值与 Windows 附件形态均未实测，因此
//! `sourceStatus`／`changeKind` 只保留原始字符串标量，不映射为「完成／成功」，
//! 也绝不把缺失、失败或进行中状态描述成已写盘。
//!
//! 有界纪律：
//! * 编号在完整 canonical 回合上统计（先于尾窗裁剪），翻页／重建不重编号；
//! * `imageIndexInTurn` 只给有可展示路径的本地图片卡；`file_change` 文件 chip 与
//!   外链不占图片编号，也不进入每回合 12 图限额；
//! * 每消息最多 [`MAX_ARTIFACTS`] 个附件；超出只累计省略计数，不复制任何字段；
//! * 摘要 ≤ [`MAX_SUMMARY_CHARS`] 个 Unicode 字符，路径／身份键／外链 URL 各
//!   ≤ [`MAX_PATH_BYTES`] 字节；超长摘要/原始标量只做有界截断或丢弃，**不**写进
//!   `unavailableReason`——该字段按已批准契约只表达「引用不可预览/打开」。

use super::types::{ArtifactKind, ArtifactSource, ProjectedArtifact};
use crate::file_identity::file_identity_key;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;

/// 每消息附件上限（服务端预算；与 UI 的 12 图／6 chip 展示限额无关）。
const MAX_ARTIFACTS: usize = 128;
/// 路径、文件身份键与外链 URL 的字节上限（16 KiB）。
const MAX_PATH_BYTES: usize = 16 * 1024;
/// 附件摘要的 Unicode 字符上限。
const MAX_SUMMARY_CHARS: usize = 1024;
/// 状态／变更类型原始标量的字符上限；超限丢弃（不回写为不可用原因）。
const MAX_SCALAR_CHARS: usize = 1024;

/// 不可预览/打开的原因（机器码；`unavailableReason` 只承载这一组）。
const MISSING_PATH: &str = "missing_path";
const PATH_TOO_LONG: &str = "path_too_long";
const FILE_KEY_TOO_LONG: &str = "file_key_too_long";
const MISSING_URL: &str = "missing_url";
const URL_TOO_LONG: &str = "url_too_long";
const DATA_URL: &str = "data_url";
const UNRESOLVED_FILE_ID: &str = "unresolved_file_id";
const UNSUPPORTED_URL_SCHEME: &str = "unsupported_url_scheme";

/// `mention` 可显示为图片卡的扩展名（与 `remote/files.rs` 的 mime 表同口径）。
/// 只决定卡片形态，不据此推导路径或断言文件存在。
const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "bmp", "svg", "ico"];

/// 单回合的编号上下文：在完整 canonical 序列上推进，尾窗只切消息。
#[derive(Default)]
pub(super) struct TurnArtifacts {
    /// 来源内序号（每个来源独立计数）。
    ordinals: HashMap<ArtifactSource, usize>,
    /// 完整回合内可展示的本地图片卡数。
    pub image_count: usize,
}

/// 单条消息的抽取结果。
#[derive(Default)]
pub(super) struct ItemArtifacts {
    pub artifacts: Vec<ProjectedArtifact>,
    /// 服务端每消息预算省略的数量（与 UI 展示限额分开表达）。
    pub omitted: usize,
}

/// 单条消息的共享上下文（消息身份、相对路径基准、来源摘要）。
struct MessageContext<'a> {
    message_id: &'a str,
    cwd: Option<&'a str>,
    summary: Option<&'a str>,
}

/// 投影层缓存的“最近用户消息”有界副本：只多留 1 个字符用于判截断。
pub(super) fn bounded_summary_source(text: &str) -> String {
    text.chars().take(MAX_SUMMARY_CHARS + 1).collect()
}

/// 保留常规 ID；超长身份改为稳定摘要，避免单条元数据突破尾窗保底预算。
pub(super) fn bounded_id(id: &str) -> String {
    if id.len() <= MAX_PATH_BYTES {
        return id.to_owned();
    }
    let hex: String = Sha256::digest(id.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("sha256:{hex}")
}

/// 用户输入块里是否出现本期支持的附件类型（text 之外的唯一“可保留”依据）。
pub(super) fn has_input_artifacts(blocks: &[Value]) -> bool {
    blocks.iter().any(|block| {
        matches!(
            block.get("type").and_then(Value::as_str),
            Some("localImage" | "mention" | "image")
        )
    })
}

/// 按已核验的 item 字段分派；`position` 始终使用原始块／change 索引。
pub(super) fn project_item_artifacts(
    item: &Value,
    message_id: &str,
    cwd: Option<&str>,
    summary: Option<&str>,
    turn: &mut TurnArtifacts,
) -> ItemArtifacts {
    let ctx = MessageContext {
        message_id,
        cwd,
        summary,
    };
    match item.get("type").and_then(Value::as_str) {
        Some("userMessage") => {
            let blocks = item
                .get("content")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            project_input_artifacts(blocks, message_id, cwd, summary, turn)
        }
        Some("fileChange") => {
            let mut out = ItemArtifacts::default();
            // 每个有路径的 change 形成文件引用；非对象 change 无可读形态，不猜。
            if let Some(changes) = item.get("changes").and_then(Value::as_array) {
                for (position, change) in changes.iter().enumerate() {
                    if !change.is_object() {
                        continue;
                    }
                    add_artifact(
                        &mut out,
                        change,
                        Some(item),
                        ArtifactSource::FileChange,
                        position,
                        &ctx,
                        turn,
                    );
                }
            }
            out
        }
        Some("imageGeneration") => {
            project_single_item(item, ArtifactSource::ImageGeneration, &ctx, turn)
        }
        Some("imageView") => project_single_item(item, ArtifactSource::ImageView, &ctx, turn),
        _ => ItemArtifacts::default(),
    }
}

/// 用户输入附件（正式 `content[]` 或开场 `params.input[]`）。
pub(super) fn project_input_artifacts(
    blocks: &[Value],
    message_id: &str,
    cwd: Option<&str>,
    summary: Option<&str>,
    turn: &mut TurnArtifacts,
) -> ItemArtifacts {
    let ctx = MessageContext {
        message_id,
        cwd,
        summary,
    };
    let mut out = ItemArtifacts::default();
    for (position, block) in blocks.iter().enumerate() {
        let source = match block.get("type").and_then(Value::as_str) {
            Some("localImage") => ArtifactSource::LocalImage,
            Some("mention") => ArtifactSource::Mention,
            Some("image") => ArtifactSource::ImageUrl,
            // 文本块与 `skill` 等其他块不进附件（`skill` 不因与 mention 共用字段而扩张）。
            _ => continue,
        };
        add_artifact(&mut out, block, None, source, position, &ctx, turn);
    }
    out
}

fn project_single_item(
    item: &Value,
    source: ArtifactSource,
    ctx: &MessageContext<'_>,
    turn: &mut TurnArtifacts,
) -> ItemArtifacts {
    let mut out = ItemArtifacts::default();
    add_artifact(&mut out, item, Some(item), source, 0, ctx, turn);
    out
}

fn add_artifact(
    out: &mut ItemArtifacts,
    value: &Value,
    status_item: Option<&Value>,
    source: ArtifactSource,
    position: usize,
    ctx: &MessageContext<'_>,
    turn: &mut TurnArtifacts,
) {
    // 来源序号在完整 canonical 顺序上推进：本条即使因预算未保留也不回退。
    let ordinal = {
        let counter = turn.ordinals.entry(source).or_default();
        *counter += 1;
        *counter
    };

    let mut path: Option<String> = None;
    let mut url: Option<String> = None;
    let mut file_key: Option<String> = None;
    let mut kind = match source {
        // `fileChange` 无论扩展名都是文件 chip；`mention` 先按文档，图片扩展名再晋级。
        ArtifactSource::FileChange | ArtifactSource::Mention => ArtifactKind::Document,
        ArtifactSource::ImageUrl => ArtifactKind::ExternalImage,
        _ => ArtifactKind::Image,
    };
    let mut reason: Option<&'static str> = None;

    match source {
        ArtifactSource::ImageUrl => {
            match value.get("url").and_then(Value::as_str) {
                // data URL 不解码、不复制：只在原始串上判前缀。
                Some(raw)
                    if raw
                        .get(..5)
                        .is_some_and(|p| p.eq_ignore_ascii_case("data:")) =>
                {
                    reason = Some(DATA_URL);
                }
                Some(raw) if raw.len() > MAX_PATH_BYTES => {
                    reason = Some(URL_TOO_LONG);
                }
                Some(raw) => match reqwest::Url::parse(raw) {
                    Ok(parsed)
                        if matches!(parsed.scheme(), "http" | "https")
                            && parsed.host_str().is_some() =>
                    {
                        url = Some(raw.to_owned());
                    }
                    _ => reason = Some(UNSUPPORTED_URL_SCHEME),
                },
                // 只有 `fileId` 时不推测本地路径，也不代理下载。
                None if value.get("fileId").is_some() => reason = Some(UNRESOLVED_FILE_ID),
                None => reason = Some(MISSING_URL),
            }
        }
        _ => {
            let field = if source == ArtifactSource::ImageGeneration {
                "savedPath"
            } else {
                "path"
            };
            match value.get(field).and_then(Value::as_str) {
                Some(raw) if raw.trim().is_empty() => reason = Some(MISSING_PATH),
                Some(raw) if raw.len() > MAX_PATH_BYTES => reason = Some(PATH_TOO_LONG),
                Some(raw) => {
                    // 相对路径交给共享身份规则（不猜 cwd、不查文件系统）。
                    let key = file_identity_key(raw, ctx.cwd, cfg!(windows));
                    path = Some(raw.to_owned());
                    if key.len() <= MAX_PATH_BYTES {
                        file_key = Some(key);
                    } else {
                        reason = Some(FILE_KEY_TOO_LONG);
                    }
                }
                None => reason = Some(MISSING_PATH),
            }
            if source == ArtifactSource::Mention && mention_is_image(value, path.as_deref()) {
                kind = ArtifactKind::Image;
            }
        }
    }

    // 摘要：同回合最近一条非空用户消息 → 已核验附件摘要（`revisedPrompt`／`name`）。
    // 只做有界截断；摘要超限不把引用变成不可用（`unavailableReason` 不承载字段原因）。
    let raw_summary = ctx
        .summary
        .filter(|text| !text.trim().is_empty())
        .map(str::to_owned)
        .or_else(|| match source {
            ArtifactSource::ImageGeneration => value
                .get("revisedPrompt")
                .and_then(Value::as_str)
                .map(str::to_owned),
            ArtifactSource::Mention => value.get("name").and_then(Value::as_str).map(str::to_owned),
            _ => None,
        });
    let summary = raw_summary.map(|raw| raw.chars().take(MAX_SUMMARY_CHARS).collect());

    // 原始状态／变更类型：只保留字符串标量，不猜枚举语义；超长时丢弃而不是
    // 截断伪造标量，也不据此把可预览引用标成不可用。
    let mut source_status = None;
    if matches!(
        source,
        ArtifactSource::ImageGeneration | ArtifactSource::FileChange
    ) {
        if let Some(raw) = status_item
            .and_then(|item| item.get("status"))
            .and_then(Value::as_str)
        {
            if raw.chars().count() <= MAX_SCALAR_CHARS {
                source_status = Some(raw.to_owned());
            }
        }
    }
    let mut change_kind = None;
    if source == ArtifactSource::FileChange {
        if let Some(raw) = value.get("kind").and_then(Value::as_str) {
            if raw.chars().count() <= MAX_SCALAR_CHARS {
                change_kind = Some(raw.to_owned());
            }
        }
    }

    // 图片卡编号只给“有可展示路径的本地图片”；外链与文件 chip 不占 12 图限额。
    let image_index_in_turn = if kind == ArtifactKind::Image && path.is_some() {
        turn.image_count += 1;
        Some(turn.image_count)
    } else {
        None
    };

    // 数量预算省略：计数已推进，但不复制任何字段（含 base64／fileId 原文）。
    if out.artifacts.len() >= MAX_ARTIFACTS {
        out.omitted += 1;
        return;
    }

    // `unavailableReason` 只表达「该引用不可预览/打开」；有界截断不进入此字段。
    let unavailable_reason = reason.map(str::to_owned);

    out.artifacts.push(ProjectedArtifact {
        artifact_id: bounded_id(&format!("{}/artifact/{position}", ctx.message_id)),
        kind,
        source,
        path,
        url,
        file_key,
        ordinal,
        image_index_in_turn,
        summary,
        source_status,
        change_kind,
        unavailable_reason,
    });
}

/// `mention` 的卡片形态：优先路径扩展名，缺路径时退回 `name`（文件名原文）。
fn mention_is_image(value: &Value, path: Option<&str>) -> bool {
    let ext = path.and_then(last_extension).or_else(|| {
        value
            .get("name")
            .and_then(Value::as_str)
            .and_then(last_extension)
    });
    ext.is_some_and(|ext| IMAGE_EXTENSIONS.contains(&ext.as_str()))
}

/// 末段扩展名（小写）；无扩展名、空扩展名或扩展名里再含路径分隔符 → None。
fn last_extension(path: &str) -> Option<String> {
    let (_, ext) = path.rsplit_once('.')?;
    let ext = ext.trim();
    (!ext.is_empty() && !ext.contains(['/', '\\'])).then(|| ext.to_ascii_lowercase())
}
