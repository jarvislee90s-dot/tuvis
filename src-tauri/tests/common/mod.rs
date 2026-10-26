//! 集成测试共享夹具（T8.1 起）。
//!
//! 每个集成测试二进制只使用其中一部分辅助，未使用项在 `--all-targets -D warnings`
//! 下不应报 dead_code（与 `tests/support.rs` 同规）。

#![allow(dead_code)]

pub mod zcode;
