//! 固定参考分词配置。
use crate::services::usage::error::UsageError;

use sha2::{Digest, Sha256};
use std::sync::{LazyLock, OnceLock};

const RESOURCE_SHA256: &str = "446a9538cb6c348e3516120d7c08b09f57c36495e2acfffe59a5bf8b0cfb1a2d";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReferenceProfile {
    pub id: String,
    pub encoding: &'static str,
    pub rule_version: u32,
    pub resource_sha256: &'static str,
}

fn profile_id(encoding: &str, rule_version: u32, resource_sha256: &str) -> String {
    let payload = format!("{encoding}\0{rule_version}\0{resource_sha256}");
    Sha256::digest(payload.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub fn reference_profile() -> &'static ReferenceProfile {
    static PROFILE: LazyLock<ReferenceProfile> = LazyLock::new(|| ReferenceProfile {
        id: profile_id("o200k_base", 1, RESOURCE_SHA256),
        encoding: "o200k_base",
        rule_version: 1,
        resource_sha256: RESOURCE_SHA256,
    });
    &PROFILE
}

pub fn count_reference(text: &str) -> Result<u64, UsageError> {
    static BPE: OnceLock<Result<tiktoken_rs::CoreBPE, String>> = OnceLock::new();
    let tokenizer = BPE.get_or_init(|| tiktoken_rs::o200k_base().map_err(|e| e.to_string()));
    let tokenizer = tokenizer
        .as_ref()
        .map_err(|e| UsageError::new("usage-internal", e.clone()))?;
    u64::try_from(tokenizer.encode_ordinary(text).len())
        .map_err(|_| UsageError::new("usage-internal", "reference token count overflow"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn reference_ordinary_text_matches_golden() {
        let cases: Vec<Value> = serde_json::from_str(include_str!(
            "../../../tests/fixtures/tps-estimate/reference-v1.json"
        ))
        .unwrap();
        for case in cases {
            let text = case["text"].as_str().unwrap();
            assert_eq!(
                count_reference(text).unwrap(),
                case["tokens"].as_u64().unwrap(),
                "参考分词不符"
            );
        }
    }

    #[test]
    fn reference_profile_changes_with_rules_or_resources() {
        let base = profile_id("o200k_base", 1, "a");
        assert_ne!(base, profile_id("o200k_base", 2, "a"));
        assert_ne!(base, profile_id("o200k_base", 1, "b"));
    }
}
