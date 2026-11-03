//! 连续 IPC 正文窗口观察；正文仅在内存保存。
use super::reference::reference_profile;
use crate::services::codex_desktop_ipc::observation::{BodyChange, BodyFrame, CaptureScope};
use std::collections::{HashMap, HashSet};

const ITEM_BUDGET: usize = 1024 * 1024;
const GLOBAL_BUDGET: usize = 16 * 1024 * 1024;

pub struct FinishedBody {
    pub scope: CaptureScope,
    pub turn_id: String,
    pub item_id: String,
    pub model: Option<String>,
    pub channel: Option<String>,
    pub text: String,
    pub first_mono_ns: u64,
    pub last_mono_ns: u64,
    pub duration_ns: u64,
    pub last_wall_ms: i64,
    pub reference_profile_id: String,
    pub short_window: bool,
}
struct Candidate {
    text: String,
    model: Option<String>,
    channel: Option<String>,
    first: Option<u64>,
    last: u64,
    wall: i64,
    growths: usize,
}
#[derive(Default)]
struct Stream {
    revision: Option<u64>,
    items: HashMap<(String, String), Candidate>,
    invalid: HashSet<(String, String)>,
    terminal: HashSet<String>,
}
#[derive(Default)]
pub struct BodyObserver {
    streams: HashMap<CaptureScope, Stream>,
    bytes: usize,
}
impl BodyObserver {
    pub fn invalidate(&mut self, scope: &CaptureScope) {
        if let Some(old) = self.streams.remove(scope) {
            self.bytes = self
                .bytes
                .saturating_sub(old.items.values().map(|c| c.text.len()).sum::<usize>());
        }
    }
    pub fn on_frame(&mut self, frame: BodyFrame) -> Vec<FinishedBody> {
        let incoming = (
            frame.scope.capture_generation,
            frame.scope.owner_epoch,
            frame.scope.owner_generation,
        );
        if self.streams.keys().any(|s| {
            s.host_key == frame.scope.host_key
                && s.thread_key == frame.scope.thread_key
                && (s.capture_generation, s.owner_epoch, s.owner_generation) > incoming
        }) {
            return Vec::new();
        }
        let stale: Vec<_> = self
            .streams
            .keys()
            .filter(|s| {
                s.host_key == frame.scope.host_key
                    && s.thread_key == frame.scope.thread_key
                    && *s != &frame.scope
            })
            .cloned()
            .collect();
        for scope in stale {
            self.invalidate(&scope);
        }
        let stream = self.streams.entry(frame.scope.clone()).or_default();
        if stream
            .revision
            .is_some_and(|r| frame.revision != r.saturating_add(1))
        {
            self.bytes = self
                .bytes
                .saturating_sub(stream.items.values().map(|c| c.text.len()).sum::<usize>());
            stream.items.clear();
            stream.invalid.clear();
            stream.terminal.clear();
            stream.revision = None;
        }
        let baseline = frame
            .changes
            .iter()
            .any(|c| matches!(c, BodyChange::Baseline));
        if baseline {
            self.bytes = self
                .bytes
                .saturating_sub(stream.items.values().map(|c| c.text.len()).sum::<usize>());
            stream.items.clear();
            stream.invalid.clear();
            stream.terminal.clear();
            stream.revision = Some(frame.revision);
            return Vec::new();
        }
        if stream.revision.is_none() {
            return Vec::new();
        }
        stream.revision = Some(frame.revision);
        let mut finished = Vec::new();
        for change in frame.changes {
            match change {
                BodyChange::Baseline => unreachable!(),
                BodyChange::EmptyItem {
                    turn_id,
                    item_id,
                    model,
                    channel,
                } => {
                    let key = (turn_id, item_id);
                    if !stream.invalid.contains(&key) && !stream.terminal.contains(&key.0) {
                        stream.items.entry(key).or_insert(Candidate {
                            text: String::new(),
                            model,
                            channel,
                            first: None,
                            last: 0,
                            wall: 0,
                            growths: 0,
                        });
                    }
                }
                BodyChange::Append {
                    turn_id,
                    item_id,
                    text,
                } => {
                    let key = (turn_id, item_id);
                    if text.is_empty() {
                        continue;
                    }
                    let Some(candidate) = stream.items.get_mut(&key) else {
                        stream.invalid.insert(key);
                        continue;
                    };
                    if candidate.text.len().saturating_add(text.len()) > ITEM_BUDGET
                        || self.bytes.saturating_add(text.len()) > GLOBAL_BUDGET
                    {
                        self.bytes = self.bytes.saturating_sub(candidate.text.len());
                        stream.items.remove(&key);
                        stream.invalid.insert(key);
                        continue;
                    }
                    candidate.first.get_or_insert(frame.clock.mono_ns);
                    candidate.last = frame.clock.mono_ns;
                    candidate.wall = frame.clock.wall_ms;
                    candidate.growths += 1;
                    self.bytes += text.len();
                    candidate.text.push_str(&text);
                }
                BodyChange::InvalidatedItem { turn_id, item_id } => {
                    let keys: Vec<_> = stream
                        .items
                        .keys()
                        .filter(|(t, i)| t == &turn_id && item_id.as_ref().is_none_or(|id| id == i))
                        .cloned()
                        .collect();
                    for key in keys {
                        if let Some(c) = stream.items.remove(&key) {
                            self.bytes = self.bytes.saturating_sub(c.text.len());
                        }
                        stream.invalid.insert(key);
                    }
                    if let Some(id) = item_id {
                        stream.invalid.insert((turn_id, id));
                    } else {
                        stream.terminal.insert(turn_id);
                    }
                }
                BodyChange::TurnTerminal { turn_id, completed } => {
                    let keys: Vec<_> = stream
                        .items
                        .keys()
                        .filter(|(t, _)| t == &turn_id)
                        .cloned()
                        .collect();
                    for key in keys {
                        let c = stream.items.remove(&key).expect("key exists");
                        self.bytes = self.bytes.saturating_sub(c.text.len());
                        if let Some(first) = c.first {
                            let control_content = c.text.contains(
                                crate::services::codex_desktop_ipc::async_question::ENVELOPE_OPEN,
                            ) || c.text.contains(
                                crate::services::codex_desktop_ipc::async_question::ENVELOPE_CLOSE,
                            );
                            if completed && !control_content && c.growths >= 2 && c.last > first {
                                let duration_ns = c.last - first;
                                finished.push(FinishedBody {
                                    scope: frame.scope.clone(),
                                    turn_id: key.0,
                                    item_id: key.1,
                                    model: c.model,
                                    channel: c.channel,
                                    text: c.text,
                                    first_mono_ns: first,
                                    last_mono_ns: c.last,
                                    duration_ns,
                                    last_wall_ms: c.wall,
                                    reference_profile_id: reference_profile().id.clone(),
                                    short_window: duration_ns < 500_000_000,
                                });
                            }
                        }
                    }
                    stream.terminal.insert(turn_id);
                }
            }
        }
        finished
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::codex_desktop_ipc::observation::{BodyChange, CaptureClock};
    fn scope() -> CaptureScope {
        CaptureScope {
            capture_generation: 1,
            owner_epoch: 1,
            owner_generation: 1,
            host_key: "host".into(),
            thread_key: "thread".into(),
        }
    }
    fn frame(revision: u64, ms: u64, change: BodyChange) -> BodyFrame {
        BodyFrame {
            scope: scope(),
            revision,
            clock: CaptureClock {
                mono_ns: ms * 1_000_000,
                wall_ms: ms as i64,
            },
            changes: vec![change],
        }
    }
    fn empty(item: &str) -> BodyChange {
        BodyChange::EmptyItem {
            turn_id: "turn".into(),
            item_id: item.into(),
            model: None,
            channel: None,
        }
    }
    fn append(item: &str, text: &str) -> BodyChange {
        BodyChange::Append {
            turn_id: "turn".into(),
            item_id: item.into(),
            text: text.into(),
        }
    }
    fn done() -> BodyChange {
        BodyChange::TurnTerminal {
            turn_id: "turn".into(),
            completed: true,
        }
    }
    #[test]
    fn body_window_uses_growth_end_not_turn_completion() {
        let mut o = BodyObserver::default();
        assert!(o.on_frame(frame(1, 0, BodyChange::Baseline)).is_empty());
        assert!(o.on_frame(frame(2, 10, empty("i"))).is_empty());
        assert!(o.on_frame(frame(3, 1000, append("i", "a"))).is_empty());
        assert!(o.on_frame(frame(4, 5000, append("i", "b"))).is_empty());
        let result = o.on_frame(frame(5, 5682, done()));
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].duration_ns, 4_000_000_000);
        assert_eq!(result[0].last_wall_ms, 5000);
        assert_eq!(result[0].text, "ab");
    }
    #[test]
    fn two_bodies_exclude_tool_gap_and_freeze_model() {
        let mut o = BodyObserver::default();
        for f in [
            frame(1, 0, BodyChange::Baseline),
            frame(
                2,
                10,
                BodyChange::EmptyItem {
                    turn_id: "turn".into(),
                    item_id: "a".into(),
                    model: Some("model-a".into()),
                    channel: Some("channel-a".into()),
                },
            ),
            frame(3, 100, append("a", "a")),
            frame(4, 200, append("a", "b")),
            frame(5, 10000, empty("b")),
            frame(6, 10100, append("b", "c")),
            frame(7, 10300, append("b", "d")),
        ] {
            assert!(o.on_frame(f).is_empty());
        }
        let out = o.on_frame(frame(8, 12000, done()));
        assert_eq!(out.len(), 2);
        let first = out.iter().find(|v| v.item_id == "a").unwrap();
        let second = out.iter().find(|v| v.item_id == "b").unwrap();
        assert_eq!(first.model.as_deref(), Some("model-a"));
        assert_eq!(first.channel.as_deref(), Some("channel-a"));
        assert!(second.model.is_none());
        let mut durations: Vec<_> = out.iter().map(|b| b.duration_ns).collect();
        durations.sort();
        assert_eq!(durations, vec![100_000_000, 200_000_000]);
    }
    #[test]
    fn old_owner_late_frame_cannot_destroy_new_owner_candidate() {
        let mut o = BodyObserver::default();
        let old = scope();
        let mut new = scope();
        new.owner_generation = 2;
        let mk = |scope: CaptureScope, revision: u64, ms: u64, change| BodyFrame {
            scope,
            revision,
            clock: CaptureClock {
                mono_ns: ms * 1_000_000,
                wall_ms: ms as i64,
            },
            changes: vec![change],
        };
        o.on_frame(mk(old.clone(), 1, 0, BodyChange::Baseline));
        o.on_frame(mk(new.clone(), 1, 1, BodyChange::Baseline));
        o.on_frame(mk(new.clone(), 2, 2, empty("i")));
        o.on_frame(mk(new.clone(), 3, 3, append("i", "a")));
        o.on_frame(mk(old, 2, 4, append("i", "old")));
        o.on_frame(mk(new.clone(), 4, 5, append("i", "b")));
        let out = o.on_frame(mk(new, 5, 6, done()));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "ab");
    }
    #[test]
    fn incomplete_and_rewritten_bodies_never_finish() {
        for case in 0..7 {
            let mut o = BodyObserver::default();
            o.on_frame(frame(1, 0, BodyChange::Baseline));
            if case != 0 {
                o.on_frame(frame(2, 1, empty("i")));
            } else {
                o.on_frame(frame(2, 1, append("i", "missed")));
            }
            o.on_frame(frame(3, 2, append("i", "a")));
            match case {
                0 => {
                    o.on_frame(frame(4, 3, append("i", "b")));
                }
                1 => {
                    o.on_frame(frame(4, 3, append("i", "")));
                }
                2 => {
                    o.on_frame(frame(4, 2, append("i", "b")));
                }
                3 => {
                    o.on_frame(frame(
                        4,
                        3,
                        BodyChange::InvalidatedItem {
                            turn_id: "turn".into(),
                            item_id: Some("i".into()),
                        },
                    ));
                }
                4 => {
                    o.on_frame(frame(4, 3, BodyChange::Baseline));
                }
                5 => {
                    o.on_frame(frame(5, 3, append("i", "b")));
                }
                6 => {
                    o.on_frame(frame(4, 3, append("i", "b")));
                }
                _ => unreachable!(),
            }
            let end = if case == 5 { 6 } else { 5 };
            let terminal = if case == 6 {
                BodyChange::TurnTerminal {
                    turn_id: "turn".into(),
                    completed: false,
                }
            } else {
                done()
            };
            assert!(
                o.on_frame(frame(end, 10, terminal)).is_empty(),
                "case {case}"
            );
        }
    }
    #[test]
    fn body_budget_is_global_and_released() {
        let mut o = BodyObserver::default();
        o.on_frame(frame(1, 0, BodyChange::Baseline));
        for i in 0..16 {
            let name = format!("{i}");
            o.on_frame(frame(2 + i * 2, 1, empty(&name)));
            o.on_frame(frame(3 + i * 2, 2, append(&name, &"a".repeat(ITEM_BUDGET))));
        }
        assert_eq!(o.bytes, GLOBAL_BUDGET);
        o.on_frame(frame(34, 3, empty("overflow")));
        o.on_frame(frame(35, 4, append("overflow", "x")));
        assert_eq!(o.bytes, GLOBAL_BUDGET);
        o.on_frame(frame(
            36,
            5,
            BodyChange::InvalidatedItem {
                turn_id: "turn".into(),
                item_id: None,
            },
        ));
        assert_eq!(o.bytes, 0);
    }
    #[test]
    fn chunk_boundaries_preserve_final_reference_count() {
        fn run(chunks: &[&str]) -> FinishedBody {
            let mut o = BodyObserver::default();
            o.on_frame(frame(1, 0, BodyChange::Baseline));
            o.on_frame(frame(2, 1, empty("i")));
            for (i, part) in chunks.iter().enumerate() {
                o.on_frame(frame(i as u64 + 3, i as u64 + 2, append("i", part)));
            }
            o.on_frame(frame(chunks.len() as u64 + 3, 100, done()))
                .pop()
                .unwrap()
        }
        let a = run(&["hello ", "世界"]);
        let b = run(&["he", "llo", " 世", "界"]);
        assert_eq!(a.text, b.text);
        assert_eq!(
            super::super::reference::count_reference(&a.text).unwrap(),
            super::super::reference::count_reference(&b.text).unwrap()
        );
    }
    #[test]
    fn oversized_single_body_is_discarded_and_releases_budget() {
        let mut o = BodyObserver::default();
        o.on_frame(frame(1, 0, BodyChange::Baseline));
        o.on_frame(frame(2, 1, empty("i")));
        o.on_frame(frame(3, 2, append("i", &"a".repeat(ITEM_BUDGET))));
        o.on_frame(frame(4, 3, append("i", "x")));
        assert_eq!(o.bytes, 0);
        assert!(o.on_frame(frame(5, 4, done())).is_empty());
    }
    #[test]
    fn official_answer_envelope_is_not_visible_body() {
        let envelope = crate::services::codex_desktop_ipc::async_question::compose_async_answer(
            "call-a", 0, "question", "answer",
        );
        let split = envelope.find('\n').unwrap();
        let mut o = BodyObserver::default();
        o.on_frame(frame(1, 0, BodyChange::Baseline));
        o.on_frame(frame(2, 1, empty("i")));
        o.on_frame(frame(3, 2, append("i", &envelope[..split])));
        o.on_frame(frame(4, 3, append("i", &envelope[split..])));
        assert!(o.on_frame(frame(5, 4, done())).is_empty());
    }
}
