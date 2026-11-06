use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum TestPoint {
    AfterSqlPreparedBeforePermit,
    DbContended,
}

pub(crate) struct TestProbe {
    point: TestPoint,
    reached: AtomicBool,
    command: AtomicU8,
    timeout: Duration,
}
impl TestProbe {
    pub(crate) fn new(point: TestPoint) -> Self {
        Self {
            point,
            reached: AtomicBool::new(false),
            command: AtomicU8::new(0),
            timeout: Duration::from_secs(10),
        }
    }
    pub(crate) fn hit(&self, point: TestPoint) -> Result<(), ()> {
        if self.point != point {
            return Ok(());
        }
        self.reached.store(true, Ordering::Release);
        let deadline = Instant::now() + self.timeout;
        loop {
            match self.command.load(Ordering::Acquire) {
                1 => return Ok(()),
                2 => return Err(()),
                _ => {}
            }
            if Instant::now() >= deadline {
                self.abort();
                return Err(());
            }
            std::thread::yield_now();
        }
    }
    pub(crate) fn reached(&self) -> bool {
        self.reached.load(Ordering::Acquire)
    }
    pub(crate) fn resume(&self) -> bool {
        self.command
            .compare_exchange(0, 1, Ordering::Release, Ordering::Acquire)
            .is_ok()
    }
    pub(crate) fn abort(&self) {
        self.command.store(2, Ordering::Release);
    }
}
