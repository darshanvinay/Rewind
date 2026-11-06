//! Disposable-copy instrumentation only; never installed in the engine checkout.
use std::{cell::RefCell, collections::BTreeMap, io::{Read, Write}, time::Instant};
use serde::Serialize;
#[derive(Default, Serialize)]
struct Timing { calls: u64, inclusive_ns: u64, exclusive_ns: u64 }
struct Frame { name: String, start: Instant, children: u64 }
#[derive(Default)]
struct State { stack: Vec<Frame>, timings: BTreeMap<String, Timing>, counters: BTreeMap<String, u64> }
thread_local! { static STATE: RefCell<Option<State>> = RefCell::new(std::env::var_os("REWIND_PROFILE_OUT").map(|_| State::default())); }
pub struct Scope(bool);
impl Scope {
    pub fn new(name: impl Into<String>) -> Self {
        let active = STATE.with(|cell| {
            let mut state = cell.borrow_mut();
            if let Some(state) = state.as_mut() {
                state.stack.push(Frame { name: name.into(), start: Instant::now(), children: 0 });
                true
            } else { false }
        });
        Self(active)
    }
}
impl Drop for Scope {
    fn drop(&mut self) {
        if !self.0 { return; }
        let now = Instant::now();
        STATE.with(|cell| {
            if let Some(state) = cell.borrow_mut().as_mut() {
                if let Some(frame) = state.stack.pop() {
                    let ns = now.duration_since(frame.start).as_nanos() as u64;
                    if let Some(parent) = state.stack.last_mut() { parent.children += ns; }
                    let stat = state.timings.entry(frame.name).or_default();
                    stat.calls += 1; stat.inclusive_ns += ns;
                    stat.exclusive_ns += ns.saturating_sub(frame.children);
                }
            }
        });
    }
}
pub fn timed<T>(name: impl Into<String>, f: impl FnOnce() -> T) -> T {
    let _scope = Scope::new(name); f()
}
pub fn event(name: impl Into<String>, amount: u64) {
    STATE.with(|cell| { if let Some(state) = cell.borrow_mut().as_mut() { *state.counters.entry(name.into()).or_default() += amount; } });
}
pub fn kind() -> &'static str {
    STATE.with(|cell| {
        if let Some(state) = cell.borrow().as_ref() {
            for frame in state.stack.iter().rev() {
                match frame.name.as_str() {
                    "chunk_store" => return "chunk",
                    "manifest_persistence" => return "manifest",
                    "merkle_persistence" => return "tree",
                    "snapshot_publication" => return "snapshot",
                    _ => (),
                }
            }
        }
        "other"
    })
}
pub fn tag(name: &str) -> String { format!("{name}.{}", kind()) }
pub struct TimedRead<R>(R);
impl<R> TimedRead<R> { pub fn new(read: R) -> Self { Self(read) } }
impl<R: Read> Read for TimedRead<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let _scope = Scope::new("source_read");
        let result = self.0.read(buf);
        if let Ok(n) = result { event("source_bytes_read", n as u64); }
        result
    }
}
pub struct ChunkIter<I>(I);
impl<I> ChunkIter<I> { pub fn new(iter: I) -> Self { Self(iter) } }
impl<I: Iterator> Iterator for ChunkIter<I> {
    type Item = I::Item;
    fn next(&mut self) -> Option<Self::Item> { timed("fastcdc", || self.0.next()) }
}
struct TimedWriter<'a, W>(&'a mut W);
impl<W: Write> Write for TimedWriter<'_, W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let _scope = Scope::new(tag("data_write"));
        let result = self.0.write(bytes);
        if let Ok(n) = result { event(tag("data_bytes_written"), n as u64); }
        result
    }
    fn flush(&mut self) -> std::io::Result<()> { self.0.flush() }
}
pub fn write_all(out: &mut impl Write, bytes: &[u8]) -> std::io::Result<()> { TimedWriter(out).write_all(bytes) }
pub fn finish() -> std::io::Result<()> {
    let Some(path) = std::env::var_os("REWIND_PROFILE_OUT") else { return Ok(()); };
    let value = STATE.with(|cell| cell.borrow_mut().take().map(|state| serde_json::json!({"timings":state.timings,"counters":state.counters,"unclosed_scopes":state.stack.len()})));
    let bytes = serde_json::to_vec_pretty(&value).map_err(std::io::Error::other)?;
    std::fs::write(path, bytes)
}
