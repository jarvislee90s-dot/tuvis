//! M3 review 纯夹具（review 9e11-M3，windows-only）：顶层枚举 257 截断守卫 +
//! 包查询错误分类与候选窗口步骤顺序。不触真实 OS/COM/EnumWindows——分类缝为
//! 保守 stub（恒 FailWhole、零后续调用），因此「已证无关早排除」「拒绝+独立
//! 证据可排除」为 RED，「未知错误整体失败」「257 截断无 partial」为既有正确
//! 行为的回归守卫。GREEN 提取真实实现后，discover_windows 生产路径必须调用
//! 同一 `evaluate_candidate_steps` / `collect_top_level`，不留镜像副本。

use super::*;

/// 顺带守卫（同一回调纯夹具）：第 257 个顶层窗口主动停，truncated 置位、
/// 不保存第 257 个——不返回截短候选集。
#[test]
fn top_level_collector_stops_at_the_257th_window_without_partial() {
    let mut collected = TopLevelWindows {
        hwnds: Vec::new(),
        truncated: false,
    };
    for i in 0isize..=256isize {
        let pointer = i as usize as *mut core::ffi::c_void;
        // SAFETY：纯存活 fixture 指针——`collected` 在整个循环内存活，回调仅解引用
        // 该指针复制 HWND；不触任何 OS/COM（无真实 EnumWindows，HWND 是合成的
        // 非 null 标记值，仅用于计数与存储，从不被解引用）。
        let continued = unsafe {
            collect_top_level(
                HWND(pointer),
                LPARAM(&mut collected as *mut TopLevelWindows as isize),
            )
        };
        let _ = continued;
    }
    assert!(
        collected.truncated,
        "the 257th top-level window must mark truncation"
    );
    assert_eq!(
        collected.hwnds.len(),
        MAX_DISCOVERY_TOP_LEVEL,
        "truncation must not leave a partial candidate set"
    );
    assert_eq!(
        collected.hwnds.last().copied(),
        Some((MAX_DISCOVERY_TOP_LEVEL - 1) as isize),
        "the 257th window must not be stored"
    );
}

/// M3：APPMODEL_ERROR_NO_PACKAGE（已证未打包）与外来包名 = 已证无关，必须在
/// image/creation-time 之前早排除（后续步骤调用次数 0）。RED = 保守 stub 恒
/// FailWhole；GREEN 后由真实分类函数满足。
#[test]
fn proven_unrelated_packages_exclude_before_image_and_time_steps() {
    for outcome in [
        PackageQueryOutcome::ProvenUnpackaged,
        PackageQueryOutcome::Named("com.example.unrelated".into()),
    ] {
        let mut image_calls = 0usize;
        let mut time_calls = 0usize;
        let mut package = || outcome.clone();
        let mut image = || -> Result<String, UiError> {
            image_calls += 1;
            Err(UiError::Unavailable)
        };
        let mut time = || -> Result<u64, UiError> {
            time_calls += 1;
            Err(UiError::Unavailable)
        };
        let (verdict, image_used, time_used) = evaluate_candidate_steps(
            PINNED_PACKAGE_FULL_NAME,
            false,
            &mut package,
            &mut image,
            &mut time,
        );
        assert_eq!(
            verdict,
            CandidateVerdict::ExcludeUnrelated,
            "proven-unrelated windows must be excluded early: {outcome:?}"
        );
        assert_eq!(
            (image_used, time_used),
            (0, 0),
            "later read steps must not run for proven-unrelated windows: {outcome:?}"
        );
    }
}

/// M3 fail-closed 守卫：未知包查询错误与 OpenProcess 被拒（无独立权威无关证据）
/// 都不可当无关——整体失败，且后续读取步骤不得运行。
#[test]
fn unknown_package_errors_and_denied_process_fail_whole_without_evidence() {
    for outcome in [
        PackageQueryOutcome::UnknownError,
        PackageQueryOutcome::ProcessDenied,
    ] {
        let mut image_calls = 0usize;
        let mut time_calls = 0usize;
        let mut package = || outcome.clone();
        let mut image = || -> Result<String, UiError> {
            image_calls += 1;
            Err(UiError::Unavailable)
        };
        let mut time = || -> Result<u64, UiError> {
            time_calls += 1;
            Err(UiError::Unavailable)
        };
        let (verdict, image_used, time_used) = evaluate_candidate_steps(
            PINNED_PACKAGE_FULL_NAME,
            false,
            &mut package,
            &mut image,
            &mut time,
        );
        assert_eq!(
            verdict,
            CandidateVerdict::FailWhole,
            "unclassifiable windows must fail the whole discovery: {outcome:?}"
        );
        assert_eq!(
            (image_used, time_used),
            (0, 0),
            "no later step may run for an unclassifiable window: {outcome:?}"
        );
    }
}

/// M3：OpenProcess 被拒只有在具备独立权威无关证据时才可排除；有证据时同样必须
/// 在后续读取步骤之前排除。RED = 保守 stub 恒 FailWhole。
#[test]
fn denied_process_with_independent_unrelated_evidence_excludes_without_later_steps() {
    let mut image_calls = 0usize;
    let mut time_calls = 0usize;
    let mut package = || PackageQueryOutcome::ProcessDenied;
    let mut image = || -> Result<String, UiError> {
        image_calls += 1;
        Err(UiError::Unavailable)
    };
    let mut time = || -> Result<u64, UiError> {
        time_calls += 1;
        Err(UiError::Unavailable)
    };
    let (verdict, image_used, time_used) = evaluate_candidate_steps(
        PINNED_PACKAGE_FULL_NAME,
        true,
        &mut package,
        &mut image,
        &mut time,
    );
    assert_eq!(
        verdict,
        CandidateVerdict::ExcludeUnrelated,
        "independent authoritative unrelated evidence may exclude a denied process"
    );
    assert_eq!(
        (image_used, time_used),
        (0, 0),
        "exclusion by independent evidence must skip all later read steps"
    );
}
