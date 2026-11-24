//! 直接接口测试的日志捕获和合成 canary；不实现环境解析、身份比较或可信状态。
//! 同步接口的日志只捕获当前测试线程，避免并行测试串扰；不声称覆盖异步消费者。

use std::cell::RefCell;
use std::sync::OnceLock;

pub const SECRET_KEY: &str = "R01_PRIVATE_CANARY";
pub const SECRET_VALUE: &str = "synthetic-r01-secret-never-export-7c29";

thread_local! {
    static LOGS: RefCell<Option<Vec<String>>> = const { RefCell::new(None) };
}

struct CaptureLogger;
static LOGGER: CaptureLogger = CaptureLogger;
static LOGGER_READY: OnceLock<()> = OnceLock::new();

impl log::Log for CaptureLogger {
    fn enabled(&self, _: &log::Metadata<'_>) -> bool {
        true
    }

    fn log(&self, record: &log::Record<'_>) {
        LOGS.with(|slot| {
            if let Some(lines) = slot.borrow_mut().as_mut() {
                lines.push(format!("{} {}", record.target(), record.args()));
            }
        });
    }

    fn flush(&self) {}
}

struct CaptureGuard;
impl Drop for CaptureGuard {
    fn drop(&mut self) {
        LOGS.with(|slot| {
            slot.borrow_mut().take();
        });
    }
}

pub fn capture_logs<T>(call: impl FnOnce() -> T) -> (T, Vec<String>) {
    LOGGER_READY.get_or_init(|| {
        log::set_logger(&LOGGER).expect("日志捕获未接入：不能以空日志假装通过");
        log::set_max_level(log::LevelFilter::Trace);
    });
    LOGS.with(|slot| {
        let mut current = slot.borrow_mut();
        assert!(current.is_none(), "不允许嵌套日志捕获");
        *current = Some(Vec::new());
    });
    let _guard = CaptureGuard;
    let result = call();
    let logs = LOGS.with(|slot| slot.borrow_mut().take().unwrap());
    (result, logs)
}

pub fn assert_redacted(text: &str) {
    for canary in [SECRET_KEY, SECRET_VALUE] {
        // 同时拦住字符串和被 Debug/JSON 输出的原始字节数组。
        let decimal: Vec<String> = canary.bytes().map(|byte| byte.to_string()).collect();
        for representation in [canary.to_owned(), decimal.join(", "), decimal.join(",")] {
            assert!(
                !text.contains(&representation),
                "合成环境 canary 泄漏；禁止在断言中回显原文"
            );
        }
    }
}

pub fn assert_logs_redacted(logs: &[String]) {
    for line in logs {
        assert_redacted(line);
    }
}
