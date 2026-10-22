// 分时价格的时间判定（2026-10-21）。
//
// 口径（用户裁决）：
//  * 星期固定周一至周五；不处理法定节假日；
//  * 高峰窗口允许跨午夜（`start > end` 表示跨到次日）；
//  * 只支持整点边界（落库小时桶是 `YYYY-MM-DDTHH`，分钟级窗口无法如实拆分）；
//  * 时区固定预设：`local` 直接用宿主本地 `hour_key`；`asiaShanghai` / `utc` 先按
//    宿主本地时区得到时间戳，再换算到目标固定偏移。
use chrono::{Datelike, FixedOffset, Local, NaiveDate, NaiveDateTime, TimeZone, Timelike, Weekday};

use crate::services::usage::v2::model::{TimePriceTimezone, TimePriceWindow};
use crate::services::usage::UsageError;

const MINUTES_PER_DAY: i32 = 24 * 60;

/// 校验后的窗口（分钟边界；`start > end` 表示跨午夜）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NormalizedWindow {
    pub start_min: i32,
    pub end_min: i32,
}

fn invalid(detail: impl Into<String>) -> UsageError {
    UsageError::new("usage-settings-invalid", detail.into())
}

fn parse_hour(text: &str) -> Result<i32, UsageError> {
    let Some(hour) = text.strip_suffix(":00") else {
        return Err(invalid(format!(
            "time price window must use whole hours: {text}"
        )));
    };
    if hour.len() != 2 || !hour.bytes().all(|b| b.is_ascii_digit()) {
        return Err(invalid(format!(
            "time price window hour is invalid: {text}"
        )));
    }
    let hour: i32 = hour
        .parse()
        .map_err(|_| invalid(format!("time price window hour is invalid: {text}")))?;
    if !(0..24).contains(&hour) {
        return Err(invalid(format!(
            "time price window hour must be 00-23: {text}"
        )));
    }
    Ok(hour * 60)
}

/// 解析并校验窗口：非空、整点、起止不同、同一天内不重叠。
pub fn normalize_windows(windows: &[TimePriceWindow]) -> Result<Vec<NormalizedWindow>, UsageError> {
    if windows.is_empty() {
        return Err(invalid("time price rule requires at least one window"));
    }
    let mut out = Vec::with_capacity(windows.len());
    for window in windows {
        let start = parse_hour(&window.start)?;
        let end = parse_hour(&window.end)?;
        if start == end {
            return Err(invalid("time price window start and end must differ"));
        }
        out.push(NormalizedWindow {
            start_min: start,
            end_min: end,
        });
    }
    // 跨午夜窗口拆成当天两段后做重叠检查；宁可拒绝含混配置，也不重复计高峰。
    let segments: Vec<(i32, i32)> = out
        .iter()
        .flat_map(|window| {
            if window.start_min < window.end_min {
                vec![(window.start_min, window.end_min)]
            } else {
                vec![(window.start_min, MINUTES_PER_DAY), (0, window.end_min)]
            }
        })
        .collect();
    for (index, left) in segments.iter().enumerate() {
        for right in &segments[index + 1..] {
            if left.0 < right.1 && right.0 < left.1 {
                return Err(invalid("time price windows must not overlap"));
            }
        }
    }
    Ok(out)
}

fn is_workday(day: Weekday) -> bool {
    matches!(
        day,
        Weekday::Mon | Weekday::Tue | Weekday::Wed | Weekday::Thu | Weekday::Fri
    )
}

fn previous_day(day: Weekday) -> Weekday {
    match day {
        Weekday::Mon => Weekday::Sun,
        Weekday::Tue => Weekday::Mon,
        Weekday::Wed => Weekday::Tue,
        Weekday::Thu => Weekday::Wed,
        Weekday::Fri => Weekday::Thu,
        Weekday::Sat => Weekday::Fri,
        Weekday::Sun => Weekday::Sat,
    }
}

/// 宿主本地 `hour_key` → 目标固定偏移时区。夏令时歧义取更早的本地时刻；无法表示时返回 None。
fn host_to_offset(local: NaiveDateTime, offset_seconds: i32) -> Option<NaiveDateTime> {
    let local = Local
        .from_local_datetime(&local)
        .earliest()
        .or_else(|| Local.from_local_datetime(&local).latest())?;
    FixedOffset::east_opt(offset_seconds)?
        .timestamp_opt(local.timestamp(), 0)
        .single()
        .map(|value| value.naive_local())
}

fn target_local(local: NaiveDateTime, timezone: TimePriceTimezone) -> Option<NaiveDateTime> {
    match timezone {
        TimePriceTimezone::Local => Some(local),
        TimePriceTimezone::AsiaShanghai => host_to_offset(local, 8 * 3600),
        TimePriceTimezone::Utc => host_to_offset(local, 0),
    }
}

/// 纯时间判定（已换算到规则时区），便于单测覆盖跨午夜与工作日边界。
pub fn is_peak_at(target: NaiveDateTime, windows: &[NormalizedWindow]) -> bool {
    let minute = target.hour() as i32 * 60 + target.minute() as i32;
    let workday = is_workday(target.weekday());
    let previous_day_is_workday = is_workday(previous_day(target.weekday()));
    windows.iter().any(|window| {
        if window.start_min < window.end_min {
            workday && minute >= window.start_min && minute < window.end_min
        } else {
            // 当天 [start, 24:00) 或前一天跨午夜窗口的 [00:00, end)
            (workday && minute >= window.start_min)
                || (previous_day_is_workday && minute < window.end_min)
        }
    })
}

/// 判断小时桶是否落在高峰时段。`bucket_key` 为宿主本地 `YYYY-MM-DDTHH`。
pub fn is_peak(
    bucket_key: &str,
    timezone: TimePriceTimezone,
    windows: &[NormalizedWindow],
) -> bool {
    // `hour_key` 形态是 `YYYY-MM-DDTHH`（没有分钟），NaiveDateTime 的
    // `%Y-%m-%dT%H` 解析会因缺少 minute 字段失败，必须按日期 + 小时拆开。
    let Some((date, hour)) = bucket_key.split_once('T') else {
        return false;
    };
    let Ok(date) = NaiveDate::parse_from_str(date, "%Y-%m-%d") else {
        return false;
    };
    if hour.len() != 2 || !hour.bytes().all(|byte| byte.is_ascii_digit()) {
        return false;
    }
    let Ok(hour) = hour.parse::<u32>() else {
        return false;
    };
    let Some(local) = date.and_hms_opt(hour, 0, 0) else {
        return false;
    };
    target_local(local, timezone).is_some_and(|target| is_peak_at(target, windows))
}

// ==== usage 自省锁：以下为排除区（测试代码），勿删勿复制 ====
#[cfg(test)]
mod tests {
    use super::*;

    fn windows(pairs: &[(&str, &str)]) -> Vec<NormalizedWindow> {
        normalize_windows(
            &pairs
                .iter()
                .map(|(start, end)| TimePriceWindow {
                    start: (*start).to_string(),
                    end: (*end).to_string(),
                })
                .collect::<Vec<_>>(),
        )
        .unwrap()
    }

    fn at(value: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M").unwrap()
    }

    #[test]
    fn deepseek_windows_cover_workdays_and_exclude_lunch_and_weekends() {
        let windows = windows(&[("09:00", "12:00"), ("14:00", "18:00")]);
        assert!(is_peak_at(at("2026-10-19T09:00"), &windows)); // Monday
        assert!(is_peak_at(at("2026-10-19T11:00"), &windows));
        assert!(!is_peak_at(at("2026-10-19T12:00"), &windows));
        assert!(is_peak_at(at("2026-10-19T14:00"), &windows));
        assert!(is_peak_at(at("2026-10-19T17:00"), &windows));
        assert!(!is_peak_at(at("2026-10-19T18:00"), &windows));
        assert!(!is_peak_at(at("2026-10-17T10:00"), &windows)); // Saturday
    }

    #[test]
    fn cross_midnight_window_spills_into_next_day_only_from_workday() {
        let windows = windows(&[("22:00", "02:00")]);
        assert!(is_peak_at(at("2026-10-19T23:00"), &windows)); // Monday
        assert!(is_peak_at(at("2026-10-20T01:00"), &windows)); // Tuesday, from Monday
        assert!(is_peak_at(at("2026-10-16T23:00"), &windows)); // Friday
        assert!(is_peak_at(at("2026-10-17T01:00"), &windows)); // Saturday, from Friday
        assert!(!is_peak_at(at("2026-10-19T01:00"), &windows)); // Monday, from Sunday
        assert!(!is_peak_at(at("2026-10-17T23:00"), &windows)); // Saturday
        assert!(!is_peak_at(at("2026-10-18T01:00"), &windows)); // Sunday, from Saturday
    }

    #[test]
    fn windows_reject_invalid_or_overlapping_hours() {
        for (start, end) in [("09:30", "12:00"), ("24:00", "02:00"), ("09:00", "09:00")] {
            assert!(
                normalize_windows(&[TimePriceWindow {
                    start: start.to_string(),
                    end: end.to_string(),
                }])
                .is_err(),
                "{start}-{end} should be rejected"
            );
        }
        assert!(normalize_windows(&[
            TimePriceWindow {
                start: "09:00".into(),
                end: "12:00".into()
            },
            TimePriceWindow {
                start: "11:00".into(),
                end: "13:00".into()
            }
        ])
        .is_err());
        assert!(normalize_windows(&[
            TimePriceWindow {
                start: "22:00".into(),
                end: "02:00".into()
            },
            TimePriceWindow {
                start: "01:00".into(),
                end: "03:00".into()
            }
        ])
        .is_err());
    }

    #[test]
    fn windows_require_whole_hours_and_non_empty_list() {
        for (start, end) in [
            ("9:00", "12:00"),
            ("09:0", "12:00"),
            ("09:00", "12"),
            ("09:00", "24:00"),
            ("-1:00", "02:00"),
            ("ab:00", "02:00"),
        ] {
            assert!(
                normalize_windows(&[TimePriceWindow {
                    start: start.to_string(),
                    end: end.to_string(),
                }])
                .is_err(),
                "{start}-{end} 必须因非整点/越界被拒绝"
            );
        }
        let error = normalize_windows(&[]).unwrap_err();
        assert_eq!(error.code, "usage-settings-invalid");
        assert_eq!(error.detail, "time price rule requires at least one window");
    }

    #[test]
    fn reverse_window_is_accepted_as_cross_midnight() {
        let windows = windows(&[("22:00", "02:00")]);
        assert_eq!(
            windows,
            vec![NormalizedWindow {
                start_min: 22 * 60,
                end_min: 2 * 60,
            }]
        );
        // 跨午夜与白天窗口可以共存，但不能重叠。
        assert!(normalize_windows(&[
            TimePriceWindow {
                start: "22:00".into(),
                end: "02:00".into()
            },
            TimePriceWindow {
                start: "09:00".into(),
                end: "12:00".into()
            }
        ])
        .is_ok());
    }

    #[test]
    fn weekend_boundaries_follow_the_previous_workday_rule() {
        let cross_midnight = windows(&[("22:00", "02:00")]);
        // 周五深夜 + 周六凌晨属于周五高峰；周六深夜 + 周日凌晨不算。
        assert!(is_peak_at(at("2026-10-16T23:30"), &cross_midnight)); // Friday
        assert!(is_peak_at(at("2026-10-17T01:00"), &cross_midnight)); // Saturday, from Friday
        assert!(!is_peak_at(at("2026-10-17T23:30"), &cross_midnight)); // Saturday
        assert!(!is_peak_at(at("2026-10-18T01:00"), &cross_midnight)); // Sunday, from Saturday
        assert!(!is_peak_at(at("2026-10-18T23:30"), &cross_midnight)); // Sunday
        assert!(!is_peak_at(at("2026-10-19T01:00"), &cross_midnight)); // Monday, from Sunday

        // 白天窗口在周末整天关闭。
        let daytime = windows(&[("09:00", "18:00")]);
        assert!(is_peak_at(at("2026-10-16T10:00"), &daytime)); // Friday
        assert!(!is_peak_at(at("2026-10-17T10:00"), &daytime)); // Saturday
        assert!(!is_peak_at(at("2026-10-18T10:00"), &daytime)); // Sunday
    }

    #[test]
    fn is_peak_handles_bucket_keys_and_timezone_conversion() {
        let windows = windows(&[("09:00", "12:00")]);
        // 非法 bucket key 一律按非高峰（不 panic）。
        for key in ["", "2026-10-19", "2026-10-19T9", "not-a-time"] {
            assert!(!is_peak(key, TimePriceTimezone::Local, &windows));
        }
        // local 直接按 hour_key 解释。
        assert!(is_peak("2026-10-19T10", TimePriceTimezone::Local, &windows));
        assert!(!is_peak(
            "2026-10-19T12",
            TimePriceTimezone::Local,
            &windows
        ));

        // 固定偏移口径：把宿主本地时间换算到目标时区后判定，结果与手工换算一致。
        use chrono::{Local, TimeZone, Utc};
        let host = at("2026-10-20T01:00");
        let instant = Local
            .from_local_datetime(&host)
            .earliest()
            .expect("宿主本地时间必须可表示");
        let utc = instant.with_timezone(&Utc).naive_local();
        let shanghai = utc + chrono::Duration::hours(8);
        assert_eq!(
            is_peak("2026-10-20T01", TimePriceTimezone::Utc, &windows),
            is_peak_at(utc, &windows)
        );
        assert_eq!(
            is_peak("2026-10-20T01", TimePriceTimezone::AsiaShanghai, &windows),
            is_peak_at(shanghai, &windows)
        );
    }
}
// ==== usage 自省锁：排除区结束，勿删勿复制 ====
