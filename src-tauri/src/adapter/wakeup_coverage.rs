use once_cell::sync::Lazy;
use std::collections::BTreeSet;
use std::sync::RwLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WakeupSource {
    RuntimeHook,
    FixedFiles,
    Uncovered,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ToolWakeupCoverage {
    pub tool_id: &'static str,
    pub source: WakeupSource,
}

/// 必须与 `adapter::TOOL_IDS` 一一对应。未知工具按 `Uncovered` 处理。
pub(crate) const TOOL_WAKEUP_COVERAGE: &[ToolWakeupCoverage] = &[
    ToolWakeupCoverage {
        tool_id: "claude",
        source: WakeupSource::RuntimeHook,
    },
    // Codex hook 有 TUI trust 门，但应用无法读取 trust 状态，保持 Idle 兜底。
    ToolWakeupCoverage {
        tool_id: "codex",
        source: WakeupSource::Uncovered,
    },
    ToolWakeupCoverage {
        tool_id: "opencode",
        source: WakeupSource::FixedFiles,
    },
    ToolWakeupCoverage {
        tool_id: "openclaw",
        source: WakeupSource::Uncovered,
    },
    ToolWakeupCoverage {
        tool_id: "kimi",
        source: WakeupSource::RuntimeHook,
    },
    ToolWakeupCoverage {
        tool_id: "workbuddy",
        source: WakeupSource::Uncovered,
    },
    ToolWakeupCoverage {
        tool_id: "zcode",
        source: WakeupSource::Uncovered,
    },
    ToolWakeupCoverage {
        tool_id: "dsh",
        source: WakeupSource::Uncovered,
    },
];

#[derive(Debug, Clone, Default)]
pub(crate) struct RuntimeHookCoverage {
    covered: BTreeSet<&'static str>,
}

impl RuntimeHookCoverage {
    pub(crate) fn set(&mut self, tool_id: &'static str, covered: bool) {
        if !matches!(source_for(tool_id), Some(WakeupSource::RuntimeHook)) {
            return;
        }
        if covered {
            self.covered.insert(tool_id);
        } else {
            self.covered.remove(tool_id);
        }
    }

    fn contains(&self, tool_id: &str) -> bool {
        self.covered.contains(tool_id)
    }
}

fn source_for(tool_id: &str) -> Option<WakeupSource> {
    TOOL_WAKEUP_COVERAGE
        .iter()
        .find(|entry| entry.tool_id == tool_id)
        .map(|entry| entry.source)
}

pub(crate) fn is_tool_uncovered(tool_id: &str, runtime: &RuntimeHookCoverage) -> bool {
    match source_for(tool_id) {
        Some(WakeupSource::RuntimeHook) => !runtime.contains(tool_id),
        Some(WakeupSource::FixedFiles) => false,
        Some(WakeupSource::Uncovered) | None => true,
    }
}

/// 仅当 hook 当前可执行且配置已核验或成功注册时才覆盖；Codex trust 不可读，永不由
/// hook 注册结果推断为覆盖。
pub(crate) fn hook_registration_covers(
    tool_id: &str,
    script_current: bool,
    helper_available: bool,
    helper_effective: bool,
    config_ready: bool,
) -> bool {
    if !config_ready {
        return false;
    }
    match tool_id {
        "claude" => {
            if helper_effective {
                helper_available
            } else {
                script_current
            }
        }
        "kimi" => helper_effective && helper_available,
        _ => false,
    }
}

static RUNTIME_HOOK_COVERAGE: Lazy<RwLock<RuntimeHookCoverage>> =
    Lazy::new(|| RwLock::new(RuntimeHookCoverage::default()));

pub(crate) fn reset_runtime_hook_coverage() {
    *RUNTIME_HOOK_COVERAGE
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = RuntimeHookCoverage::default();
}

pub(crate) fn set_runtime_hook_coverage(tool_id: &'static str, covered: bool) {
    RUNTIME_HOOK_COVERAGE
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .set(tool_id, covered);
}

pub(crate) fn runtime_hook_coverage_snapshot() -> RuntimeHookCoverage {
    RUNTIME_HOOK_COVERAGE
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::TOOL_IDS;

    #[test]
    fn coverage_table_matches_registered_tool_ids() {
        let mut registered = TOOL_IDS.to_vec();
        let mut covered: Vec<_> = TOOL_WAKEUP_COVERAGE
            .iter()
            .map(|entry| entry.tool_id)
            .collect();
        registered.sort_unstable();
        covered.sort_unstable();
        assert_eq!(covered, registered);
    }

    #[test]
    fn runtime_hook_requires_current_success() {
        let mut runtime = RuntimeHookCoverage::default();
        assert!(is_tool_uncovered("claude", &runtime));
        assert!(is_tool_uncovered("kimi", &runtime));

        runtime.set(
            "claude",
            hook_registration_covers("claude", true, false, false, true),
        );
        runtime.set(
            "kimi",
            hook_registration_covers("kimi", false, true, true, true),
        );
        assert!(!is_tool_uncovered("claude", &runtime));
        assert!(!is_tool_uncovered("kimi", &runtime));

        runtime.set(
            "kimi",
            hook_registration_covers("kimi", false, false, true, true),
        );
        assert!(
            is_tool_uncovered("kimi", &runtime),
            "helper 缺席时不能沿用之前的覆盖状态"
        );

        runtime = RuntimeHookCoverage::default();
        assert!(is_tool_uncovered("claude", &runtime));
        assert!(is_tool_uncovered("kimi", &runtime));
    }

    #[test]
    fn codex_and_no_signal_tools_stay_uncovered() {
        let runtime = RuntimeHookCoverage::default();
        for tool in ["codex", "dsh", "openclaw", "workbuddy", "zcode"] {
            assert!(is_tool_uncovered(tool, &runtime), "{tool}");
        }
        assert!(!is_tool_uncovered("opencode", &runtime));
        assert!(!hook_registration_covers("codex", true, true, true, true));
    }

    #[test]
    fn hook_coverage_requires_artifact_and_verified_or_registered_config() {
        assert!(hook_registration_covers("claude", true, false, false, true));
        assert!(!hook_registration_covers(
            "claude", false, false, false, true
        ));
        assert!(!hook_registration_covers(
            "claude", true, false, false, false
        ));
        assert!(hook_registration_covers("claude", false, true, true, true));
        assert!(!hook_registration_covers(
            "claude", false, true, false, true
        ));

        assert!(hook_registration_covers("kimi", false, true, true, true));
        assert!(!hook_registration_covers("kimi", false, false, true, true));
        assert!(!hook_registration_covers("kimi", false, true, false, true));
        assert!(!hook_registration_covers("unknown", true, true, true, true));
    }

    #[test]
    fn unknown_tool_is_uncovered() {
        assert!(is_tool_uncovered(
            "future-tool",
            &RuntimeHookCoverage::default()
        ));
    }
}
