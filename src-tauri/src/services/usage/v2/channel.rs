//! 供应商别名配置与显示渠道解析（用量看板 V2 Task 2）。
//!
//! 本模块是**纯函数 + 配置服务**：不读账本、不联网。
//! * `ChannelResolver` 在查询层把「原始 provider / 空 provider 的（工具 + 模型）条目」
//!   解析为用户显示名；
//! * 解析结果**不回写账本**，原始 `observedProvider` 永远保留在身份里；
//! * 优先级严格按 Spec §2.4：显式 `billingProfileId` 绑定 → `ambiguous` → 非空 provider
//!   （别名命中 = `alias`，否则 = `provider`）→ 空 provider 的 `(sourceId, modelId)`
//!   指派（命中 = `alias`，否则 = `unassigned`）；
//! * 工具不进入渠道键：只有「空 provider 未指派」的行在展示层额外附加工具上下文，
//!   渠道维度仍合并为一行。

use std::collections::{HashMap, HashSet};

use rusqlite::Connection;

use crate::database::connection::DB;
use crate::database::dao::usage_alias as dao;
use crate::services::usage::model::UsageSourceId;
use crate::services::usage::v2::model::{
    Attribution, AttributionKind, BillingConfig, ChannelKind, IdentityKey, ProviderAliasConfig,
    ProviderAliasKind, ProviderAliasRule,
};
use crate::services::usage::UsageError;

/// 空 provider 未指派时的渠道稳定键（渠道维度合并为一行）。
pub(crate) const UNASSIGNED_CHANNEL_KEY: &str = "unassigned";
/// 规则冲突/混合、不能声称属于某个渠道时的稳定键。
pub(crate) const AMBIGUOUS_CHANNEL_KEY: &str = "ambiguous";

fn invalid(detail: impl Into<String>) -> UsageError {
    UsageError::new("usage-settings-invalid", detail)
}

fn db_failed(context: &str, err: impl std::fmt::Display) -> UsageError {
    UsageError::new("usage-db-failed", format!("{context}: {err}"))
}

fn poisoned() -> UsageError {
    UsageError::new("usage-db-failed", "db poisoned lock")
}

/// `bp:<profileId>` / `provider:<displayName>` / `unassigned` / `ambiguous`。
fn bp_key(profile_id: &str) -> String {
    format!("bp:{profile_id}")
}

fn provider_key(display_name: &str) -> String {
    format!("provider:{display_name}")
}

fn ambiguous_channel() -> ResolvedChannel {
    ResolvedChannel {
        key: AMBIGUOUS_CHANNEL_KEY.to_string(),
        kind: ChannelKind::Ambiguous,
        display_provider: None,
        billing_profile_id: None,
    }
}

/// 长度前缀序列化：任何分组/身份键都不得用 `/` 等裸拼接（会碰撞）。
///
/// 该实现与旧 `v2/query.rs` 的私有实现逐字一致（Task 2 提升为共享工具）：
/// 既有查询期 key 的语义与字节形态不变。
pub(crate) fn join_key(parts: &[&str]) -> String {
    let mut out = String::new();
    for part in parts {
        out.push_str(&part.len().to_string());
        out.push(':');
        out.push_str(part);
    }
    out
}

/// 校验单条别名规则；detail 前缀按计划 §2.5 固定，错误码统一 `usage-settings-invalid`。
fn validate_rule(rule: &ProviderAliasRule) -> Result<(), UsageError> {
    if rule.id.trim().is_empty() {
        return Err(invalid("provider alias id is empty"));
    }
    if rule.display_name.trim().is_empty() {
        return Err(invalid(format!(
            "provider alias display name is empty: {}",
            rule.id
        )));
    }
    match rule.kind {
        ProviderAliasKind::Provider => {
            if rule
                .provider
                .as_deref()
                .is_none_or(|raw| raw.trim().is_empty())
            {
                return Err(invalid(format!(
                    "provider alias provider is empty: {}",
                    rule.id
                )));
            }
            if rule.source_id.is_some() || rule.model_id.is_some() {
                return Err(invalid(format!(
                    "provider alias source/model key is invalid: {}",
                    rule.id
                )));
            }
        }
        ProviderAliasKind::ToolModel => {
            if rule.provider.is_some() || rule.source_id.is_none() || rule.model_id.is_none() {
                return Err(invalid(format!(
                    "provider alias source/model key is invalid: {}",
                    rule.id
                )));
            }
        }
    }
    Ok(())
}

/// 校验完整配置：id 唯一、单条形状合法、目标键不得重复。
///
/// 用于「配置级」校验（测试/未来批量导入）；单条保存走 [`upsert_with_conn`]，
/// 其键冲突在事务内对照数据库检查。
pub fn validate_config(config: &ProviderAliasConfig) -> Result<(), UsageError> {
    let mut ids: HashSet<&str> = HashSet::new();
    let mut providers: HashSet<&str> = HashSet::new();
    let mut tool_models: HashSet<(&str, &str)> = HashSet::new();
    for rule in &config.aliases {
        if rule.id.trim().is_empty() {
            return Err(invalid("provider alias id is empty"));
        }
        if !ids.insert(rule.id.as_str()) {
            return Err(invalid(format!("duplicate provider alias id: {}", rule.id)));
        }
        validate_rule(rule)?;
        match rule.kind {
            ProviderAliasKind::Provider => {
                let provider = rule.provider.as_deref().unwrap_or_default();
                if !providers.insert(provider) {
                    return Err(invalid(format!(
                        "provider alias key already exists: provider:{provider}"
                    )));
                }
            }
            ProviderAliasKind::ToolModel => {
                let source = rule
                    .source_id
                    .map(|source| source.db_id())
                    .unwrap_or_default();
                let model = rule.model_id.as_deref().unwrap_or_default();
                if !tool_models.insert((source, model)) {
                    return Err(invalid(format!(
                        "provider alias key already exists: toolModel:{source}:{model}"
                    )));
                }
            }
        }
    }
    Ok(())
}

/// 一条身份解析出的显示渠道。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedChannel {
    /// `bp:<profileId>` / `provider:<displayName>` / `unassigned` / `ambiguous`。
    pub key: String,
    pub kind: ChannelKind,
    pub display_provider: Option<String>,
    pub billing_profile_id: Option<String>,
}

/// 查询期渠道解析器：配置一次性加载，逐行 O(1) 查表。
pub(crate) struct ChannelResolver<'a> {
    /// profile id -> 显示名
    profiles: HashMap<&'a str, &'a str>,
    /// 原始 provider -> 显示名
    by_provider: HashMap<&'a str, &'a str>,
    /// (工具, 原始模型) -> 显示名
    by_tool_model: HashMap<(&'a str, &'a str), &'a str>,
}

impl<'a> ChannelResolver<'a> {
    pub(crate) fn new(config: &'a BillingConfig, aliases: &'a ProviderAliasConfig) -> Self {
        let profiles = config
            .profiles
            .iter()
            .map(|profile| (profile.id.as_str(), profile.name.as_str()))
            .collect();
        let mut by_provider = HashMap::new();
        let mut by_tool_model = HashMap::new();
        for alias in &aliases.aliases {
            match alias.kind {
                ProviderAliasKind::Provider => {
                    if let Some(provider) = alias.provider.as_deref() {
                        by_provider.insert(provider, alias.display_name.as_str());
                    }
                }
                ProviderAliasKind::ToolModel => {
                    if let (Some(source), Some(model)) =
                        (alias.source_id, alias.model_id.as_deref())
                    {
                        by_tool_model.insert((source.db_id(), model), alias.display_name.as_str());
                    }
                }
            }
        }
        Self {
            profiles,
            by_provider,
            by_tool_model,
        }
    }

    /// 观察条目的显示供应商（2026-10-20 计划 §3.1）：只走别名映射——非空 provider
    /// 精确命中、空 provider 走 `(工具, 原始模型)` 指派——**不触 billing attribution**
    /// （配置弹窗没有归因上下文）。非空 provider 无别名时显示原名（与
    /// [`resolve_channel`] 的 Provider 分支一致）；空 provider 未指派时 `None`
    /// （未指定渠道）。
    pub(crate) fn display_provider_of(
        &self,
        source_id: UsageSourceId,
        provider: Option<&str>,
        model_id: &str,
    ) -> Option<String> {
        if let Some(observed) = provider.filter(|raw| !raw.is_empty()) {
            return Some(
                self.by_provider
                    .get(observed)
                    .map(|display| (*display).to_string())
                    .unwrap_or_else(|| observed.to_string()),
            );
        }
        self.by_tool_model
            .get(&(source_id.db_id(), model_id))
            .map(|display| (*display).to_string())
    }
}

/// 解析一条身份/归因的显示渠道（严格按 Spec §2.4 的优先级）。
pub(crate) fn resolve_channel(
    identity: &IdentityKey,
    attribution: &Attribution,
    resolver: &ChannelResolver<'_>,
) -> ResolvedChannel {
    // 1. 显式计费绑定（覆盖层，优先级最高）；profile 缺失保守按 ambiguous。
    if attribution.attribution_kind == AttributionKind::UserAssigned {
        if let Some(profile_id) = attribution.billing_profile_id.as_deref() {
            return match resolver.profiles.get(profile_id) {
                Some(name) => ResolvedChannel {
                    key: bp_key(profile_id),
                    kind: ChannelKind::BillingProfile,
                    display_provider: Some((*name).to_string()),
                    billing_profile_id: Some(profile_id.to_string()),
                },
                None => ambiguous_channel(),
            };
        }
    }

    // 2. 冲突/混合归因不得套用任何别名。
    if attribution.attribution_kind == AttributionKind::Ambiguous {
        return ambiguous_channel();
    }

    // 3. 非空 provider：原始串精确、区分大小写。
    if let Some(observed) = identity
        .observed_provider
        .as_deref()
        .filter(|raw| !raw.is_empty())
    {
        return match resolver.by_provider.get(observed) {
            Some(display) => ResolvedChannel {
                key: provider_key(display),
                kind: ChannelKind::Alias,
                display_provider: Some((*display).to_string()),
                billing_profile_id: None,
            },
            None => ResolvedChannel {
                key: provider_key(observed),
                kind: ChannelKind::Provider,
                display_provider: Some(observed.to_string()),
                billing_profile_id: None,
            },
        };
    }

    // 4. 空 provider：只查（工具 + 原始模型）逐条指派；未指派合并 unassigned。
    match resolver
        .by_tool_model
        .get(&(identity.source_id.db_id(), identity.model_id.as_str()))
    {
        Some(display) => ResolvedChannel {
            key: provider_key(display),
            kind: ChannelKind::Alias,
            display_provider: Some((*display).to_string()),
            billing_profile_id: None,
        },
        None => ResolvedChannel {
            key: UNASSIGNED_CHANNEL_KEY.to_string(),
            kind: ChannelKind::Unassigned,
            display_provider: None,
            billing_profile_id: None,
        },
    }
}

// ── 服务入口（`*_with_conn` 供测试；生产入口只负责取 DB 锁）────────────────────

pub fn load_with_conn(conn: &Connection) -> Result<ProviderAliasConfig, UsageError> {
    Ok(ProviderAliasConfig {
        aliases: dao::load_aliases_conn(conn)?,
    })
}

/// 服务边界归一化：`provider` 与 `displayName` 去首尾空白后入库；
/// `toolModel` 的 `sourceId` / `modelId` 保持原样（模型名可能含合法空格）。
fn trimmed_rule(rule: &ProviderAliasRule) -> ProviderAliasRule {
    let mut trimmed = rule.clone();
    trimmed.provider = rule.provider.as_deref().map(str::trim).map(str::to_string);
    trimmed.display_name = rule.display_name.trim().to_string();
    trimmed
}

/// 保存一条规则：按 id 更新，不存在则插入；目标键被其他规则占用时拒绝且不写半条。
pub fn upsert_with_conn(
    conn: &mut Connection,
    rule: &ProviderAliasRule,
    now_ms: i64,
) -> Result<ProviderAliasConfig, UsageError> {
    let rule = trimmed_rule(rule);
    validate_rule(&rule)?;
    let tx = conn
        .transaction()
        .map_err(|e| db_failed("provider alias transaction begin failed", e))?;
    if let Some(key) = dao::conflicting_alias_key_conn(&tx, &rule)? {
        return Err(invalid(format!("provider alias key already exists: {key}")));
    }
    dao::write_alias_conn(&tx, &rule, now_ms)?;
    tx.commit()
        .map_err(|e| db_failed("provider alias transaction commit failed", e))?;
    load_with_conn(conn)
}

/// 删除一条规则；不存在的 id 是幂等空操作。
pub fn delete_with_conn(
    conn: &mut Connection,
    id: &str,
) -> Result<ProviderAliasConfig, UsageError> {
    dao::delete_alias_conn(conn, id)?;
    load_with_conn(conn)
}

pub fn load() -> Result<ProviderAliasConfig, UsageError> {
    let conn = DB.lock().map_err(|_| poisoned())?;
    load_with_conn(&conn)
}

pub fn upsert(rule: &ProviderAliasRule, now_ms: i64) -> Result<ProviderAliasConfig, UsageError> {
    let mut conn = DB.lock().map_err(|_| poisoned())?;
    upsert_with_conn(&mut conn, rule, now_ms)
}

pub fn delete(id: &str) -> Result<ProviderAliasConfig, UsageError> {
    let mut conn = DB.lock().map_err(|_| poisoned())?;
    delete_with_conn(&mut conn, id)
}

// ==== usage 自省锁：以下为排除区（测试代码），勿删勿复制 ====
#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::usage::model::UsageSourceId;
    use crate::services::usage::v2::model::{
        AttributionReason, BillingProfile, BillingProfileKind, ProviderAliasConfig,
        ProviderAliasRule,
    };

    fn billing_config(profiles: &[(&str, &str, bool)]) -> BillingConfig {
        BillingConfig {
            revision: 0,
            profiles: profiles
                .iter()
                .map(|(id, name, archived)| BillingProfile {
                    id: (*id).to_string(),
                    name: (*name).to_string(),
                    kind: BillingProfileKind::Api,
                    note: String::new(),
                    archived: *archived,
                })
                .collect(),
            rules: Vec::new(),
            pricing_rules: Vec::new(),
        }
    }

    fn provider_alias(id: &str, provider: &str, display: &str) -> ProviderAliasRule {
        ProviderAliasRule {
            id: id.to_string(),
            kind: ProviderAliasKind::Provider,
            provider: Some(provider.to_string()),
            source_id: None,
            model_id: None,
            display_name: display.to_string(),
        }
    }

    fn tool_alias(
        id: &str,
        source: UsageSourceId,
        model: &str,
        display: &str,
    ) -> ProviderAliasRule {
        ProviderAliasRule {
            id: id.to_string(),
            kind: ProviderAliasKind::ToolModel,
            provider: None,
            source_id: Some(source),
            model_id: Some(model.to_string()),
            display_name: display.to_string(),
        }
    }

    fn aliases(rules: Vec<ProviderAliasRule>) -> ProviderAliasConfig {
        ProviderAliasConfig { aliases: rules }
    }

    fn identity(source: UsageSourceId, provider: Option<&str>, model: &str) -> IdentityKey {
        IdentityKey {
            source_id: source,
            observed_provider: provider.map(str::to_string),
            model_id: model.to_string(),
        }
    }

    fn unknown() -> Attribution {
        Attribution {
            billing_profile_id: None,
            attribution_kind: AttributionKind::Unknown,
            rule_ids: Vec::new(),
            reason: Some(AttributionReason::Unassigned),
        }
    }

    fn ambiguous() -> Attribution {
        Attribution {
            billing_profile_id: None,
            attribution_kind: AttributionKind::Ambiguous,
            rule_ids: vec!["r".to_string()],
            reason: Some(AttributionReason::Conflict),
        }
    }

    fn assigned(profile_id: Option<&str>) -> Attribution {
        Attribution {
            billing_profile_id: profile_id.map(str::to_string),
            attribution_kind: AttributionKind::UserAssigned,
            rule_ids: vec!["r".to_string()],
            reason: None,
        }
    }

    #[test]
    fn join_key_is_length_prefixed_and_collision_free() {
        assert_eq!(join_key(&["provider", "x"]), "8:provider1:x");
        assert_ne!(join_key(&["ab", "c"]), join_key(&["a", "bc"]));
        assert_eq!(join_key(&[]), "");
    }

    #[test]
    fn provider_alias_matches_exact_raw_string_case_sensitively() {
        let config = billing_config(&[]);
        let aliases = aliases(vec![provider_alias("p1", "openai", "OpenAI 订阅")]);
        let resolver = ChannelResolver::new(&config, &aliases);

        let hit = resolve_channel(
            &identity(UsageSourceId::Codex, Some("openai"), "gpt-5"),
            &unknown(),
            &resolver,
        );
        assert_eq!(hit.kind, ChannelKind::Alias);
        assert_eq!(hit.key, "provider:OpenAI 订阅");
        assert_eq!(hit.display_provider.as_deref(), Some("OpenAI 订阅"));
        assert_eq!(hit.billing_profile_id, None);

        let case_miss = resolve_channel(
            &identity(UsageSourceId::Codex, Some("OpenAI"), "gpt-5"),
            &unknown(),
            &resolver,
        );
        assert_eq!(case_miss.kind, ChannelKind::Provider);
        assert_eq!(case_miss.key, "provider:OpenAI");
        assert_eq!(case_miss.display_provider.as_deref(), Some("OpenAI"));
    }

    #[test]
    fn different_raw_providers_with_same_display_name_merge() {
        let config = billing_config(&[]);
        let aliases = aliases(vec![
            provider_alias("p1", "openai", "OpenAI"),
            provider_alias("p2", "azure-openai", "OpenAI"),
        ]);
        let resolver = ChannelResolver::new(&config, &aliases);

        let first = resolve_channel(
            &identity(UsageSourceId::Codex, Some("openai"), "gpt-5"),
            &unknown(),
            &resolver,
        );
        let second = resolve_channel(
            &identity(UsageSourceId::Claude, Some("azure-openai"), "gpt-5"),
            &unknown(),
            &resolver,
        );
        assert_eq!(first.key, second.key);
        assert_eq!(first.key, "provider:OpenAI");
        assert_eq!(first.kind, ChannelKind::Alias);
        assert_eq!(second.kind, ChannelKind::Alias);
    }

    #[test]
    fn same_raw_provider_merges_across_tools() {
        let config = billing_config(&[]);
        let aliases = aliases(vec![provider_alias("p1", "openai", "OpenAI 订阅")]);
        let resolver = ChannelResolver::new(&config, &aliases);

        let codex = resolve_channel(
            &identity(UsageSourceId::Codex, Some("openai"), "gpt-5"),
            &unknown(),
            &resolver,
        );
        let claude = resolve_channel(
            &identity(UsageSourceId::Claude, Some("openai"), "claude-x"),
            &unknown(),
            &resolver,
        );
        assert_eq!(codex.key, "provider:OpenAI 订阅");
        assert_eq!(codex.key, claude.key, "工具不进入渠道键");
    }

    #[test]
    fn empty_provider_uses_tool_model_assignment_including_empty_model() {
        let config = billing_config(&[]);
        let aliases = aliases(vec![
            tool_alias("t1", UsageSourceId::Codex, "gpt-5", "Codex GPT-5"),
            tool_alias("t2", UsageSourceId::Codex, "", "Codex 未知模型"),
        ]);
        let resolver = ChannelResolver::new(&config, &aliases);

        let model = resolve_channel(
            &identity(UsageSourceId::Codex, None, "gpt-5"),
            &unknown(),
            &resolver,
        );
        assert_eq!(model.kind, ChannelKind::Alias);
        assert_eq!(model.key, "provider:Codex GPT-5");

        let unknown_model = resolve_channel(
            &identity(UsageSourceId::Codex, None, ""),
            &unknown(),
            &resolver,
        );
        assert_eq!(unknown_model.kind, ChannelKind::Alias);
        assert_eq!(unknown_model.key, "provider:Codex 未知模型");

        // 同模型但不同工具不命中：toolModel 指派按工具隔离。
        let other_tool = resolve_channel(
            &identity(UsageSourceId::Claude, None, "gpt-5"),
            &unknown(),
            &resolver,
        );
        assert_eq!(other_tool.kind, ChannelKind::Unassigned);
    }

    #[test]
    fn unassigned_empty_provider_merges_to_one_key() {
        let config = billing_config(&[]);
        let aliases = aliases(Vec::new());
        let resolver = ChannelResolver::new(&config, &aliases);

        let first = resolve_channel(
            &identity(UsageSourceId::Codex, None, "gpt-5"),
            &unknown(),
            &resolver,
        );
        let second = resolve_channel(
            &identity(UsageSourceId::Claude, Some(""), "claude-x"),
            &unknown(),
            &resolver,
        );
        assert_eq!(first.kind, ChannelKind::Unassigned);
        assert_eq!(second.kind, ChannelKind::Unassigned);
        assert_eq!(first.key, UNASSIGNED_CHANNEL_KEY);
        assert_eq!(first.key, second.key);
        assert_eq!(first.display_provider, None);
        assert_eq!(first.billing_profile_id, None);
    }

    #[test]
    fn ambiguous_attribution_ignores_aliases() {
        let config = billing_config(&[]);
        let aliases = aliases(vec![provider_alias("p1", "openai", "OpenAI")]);
        let resolver = ChannelResolver::new(&config, &aliases);

        let got = resolve_channel(
            &identity(UsageSourceId::Codex, Some("openai"), "gpt-5"),
            &ambiguous(),
            &resolver,
        );
        assert_eq!(got.kind, ChannelKind::Ambiguous);
        assert_eq!(got.key, AMBIGUOUS_CHANNEL_KEY);
        assert_eq!(got.display_provider, None);
        assert_eq!(got.billing_profile_id, None);
    }

    #[test]
    fn explicit_binding_wins_over_provider_alias() {
        let config = billing_config(&[("bp1", "工作套餐", false)]);
        let aliases = aliases(vec![provider_alias("p1", "openai", "OpenAI")]);
        let resolver = ChannelResolver::new(&config, &aliases);

        let got = resolve_channel(
            &identity(UsageSourceId::Codex, Some("openai"), "gpt-5"),
            &assigned(Some("bp1")),
            &resolver,
        );
        assert_eq!(got.kind, ChannelKind::BillingProfile);
        assert_eq!(got.key, "bp:bp1");
        assert_eq!(got.display_provider.as_deref(), Some("工作套餐"));
        assert_eq!(got.billing_profile_id.as_deref(), Some("bp1"));
    }

    #[test]
    fn archived_profile_still_resolves() {
        let config = billing_config(&[("bp1", "归档套餐", true)]);
        let aliases = aliases(Vec::new());
        let resolver = ChannelResolver::new(&config, &aliases);

        let got = resolve_channel(
            &identity(UsageSourceId::Codex, Some("openai"), "gpt-5"),
            &assigned(Some("bp1")),
            &resolver,
        );
        assert_eq!(
            got.kind,
            ChannelKind::BillingProfile,
            "归档渠道仍要能显示历史行（选项排除由查询层负责）"
        );
        assert_eq!(got.key, "bp:bp1");
        assert_eq!(got.display_provider.as_deref(), Some("归档套餐"));
    }

    #[test]
    fn missing_profile_is_ambiguous() {
        let config = billing_config(&[]);
        let aliases = aliases(Vec::new());
        let resolver = ChannelResolver::new(&config, &aliases);

        let got = resolve_channel(
            &identity(UsageSourceId::Codex, Some("openai"), "gpt-5"),
            &assigned(Some("ghost")),
            &resolver,
        );
        assert_eq!(got.kind, ChannelKind::Ambiguous);
        assert_eq!(got.key, AMBIGUOUS_CHANNEL_KEY);
        assert_eq!(got.billing_profile_id, None);
    }

    #[test]
    fn user_assigned_without_profile_id_falls_through_to_provider() {
        let config = billing_config(&[]);
        let aliases = aliases(vec![provider_alias("p1", "openai", "OpenAI")]);
        let resolver = ChannelResolver::new(&config, &aliases);

        let got = resolve_channel(
            &identity(UsageSourceId::Codex, Some("openai"), "gpt-5"),
            &assigned(None),
            &resolver,
        );
        assert_eq!(got.kind, ChannelKind::Alias);
        assert_eq!(got.key, "provider:OpenAI");
    }

    #[test]
    fn provider_without_alias_is_provider_kind() {
        let config = billing_config(&[]);
        let aliases = aliases(Vec::new());
        let resolver = ChannelResolver::new(&config, &aliases);

        let got = resolve_channel(
            &identity(UsageSourceId::Codex, Some("openai"), "gpt-5"),
            &unknown(),
            &resolver,
        );
        assert_eq!(got.kind, ChannelKind::Provider);
        assert_eq!(got.key, "provider:openai");
        assert_eq!(got.display_provider.as_deref(), Some("openai"));
    }

    #[test]
    fn config_validation_rejects_duplicate_ids_and_keys() {
        let duplicate_id = aliases(vec![
            provider_alias("dup", "openai", "A"),
            provider_alias("dup", "azure", "B"),
        ]);
        let err = validate_config(&duplicate_id).unwrap_err();
        assert_eq!(err.code, "usage-settings-invalid");
        assert_eq!(err.detail, "duplicate provider alias id: dup");

        let duplicate_provider = aliases(vec![
            provider_alias("p1", "openai", "A"),
            provider_alias("p2", "openai", "B"),
        ]);
        let err = validate_config(&duplicate_provider).unwrap_err();
        assert_eq!(
            err.detail,
            "provider alias key already exists: provider:openai"
        );

        let duplicate_tool_model = aliases(vec![
            tool_alias("t1", UsageSourceId::Codex, "gpt-5", "A"),
            tool_alias("t2", UsageSourceId::Codex, "gpt-5", "B"),
        ]);
        let err = validate_config(&duplicate_tool_model).unwrap_err();
        assert_eq!(
            err.detail,
            "provider alias key already exists: toolModel:codex:gpt-5"
        );
    }

    #[test]
    fn config_validation_rejects_bad_shapes_with_fixed_details() {
        let empty_id = aliases(vec![provider_alias("", "openai", "A")]);
        assert_eq!(
            validate_config(&empty_id).unwrap_err().detail,
            "provider alias id is empty"
        );

        let mut blank_display = provider_alias("p1", "openai", "A");
        blank_display.display_name = "   ".to_string();
        assert_eq!(
            validate_config(&aliases(vec![blank_display]))
                .unwrap_err()
                .detail,
            "provider alias display name is empty: p1"
        );

        let mut empty_provider = provider_alias("p1", "openai", "A");
        empty_provider.provider = Some(String::new());
        assert_eq!(
            validate_config(&aliases(vec![empty_provider]))
                .unwrap_err()
                .detail,
            "provider alias provider is empty: p1"
        );

        let mut provider_with_source = provider_alias("p1", "openai", "A");
        provider_with_source.source_id = Some(UsageSourceId::Codex);
        assert_eq!(
            validate_config(&aliases(vec![provider_with_source]))
                .unwrap_err()
                .detail,
            "provider alias source/model key is invalid: p1"
        );

        let mut tool_with_provider = tool_alias("t1", UsageSourceId::Codex, "", "A");
        tool_with_provider.provider = Some("openai".to_string());
        assert_eq!(
            validate_config(&aliases(vec![tool_with_provider]))
                .unwrap_err()
                .detail,
            "provider alias source/model key is invalid: t1"
        );

        // 空 model 是合法的「未知模型」指派。
        validate_config(&aliases(vec![tool_alias(
            "t1",
            UsageSourceId::Codex,
            "",
            "未知模型",
        )]))
        .expect("空 modelId 的 toolModel 指派必须合法");
    }
}
// ==== usage 自省锁：排除区结束，勿删勿复制 ====
