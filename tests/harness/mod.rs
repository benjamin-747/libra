//! Cross-process test harness for Local TUI Automation Control, plus the
//! Task A6.5 local agent-capture smoke driver.

// Unix-only: the smoke driver uses std::os::unix (process groups, file
// modes); gating here keeps every `mod harness;` consumer compiling on
// non-unix targets (its sole consumer test file is `#![cfg(unix)]`).
#[cfg(unix)]
pub mod agent_local_capture;
mod code_session;
pub mod event_stream;
pub mod matrix;
mod scenario;

pub use code_session::{CodeSession, CodeSessionOptions};
pub use event_stream::{EventStream, SseEvent};
#[allow(unused_imports)]
pub use scenario::Scenario;
