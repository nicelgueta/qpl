//! What an action is allowed to change. Every action that changes state is
//! classified by its [`Effect`], and [`Vm::authorize`](crate::vm::Vm::authorize)
//! decides whether the session may perform it:
//!
//! - a read-only session (every session not started with `qpl -w`) refuses
//!   [`Effect::Write`], from every source;
//! - a request over a read-only IPC handle also refuses [`Effect::Session`].
//!
//! User-facing docs only talk about read and write: `Session` is a read as far
//! as a session's own permission goes, and only matters for IPC handles.

/// The most an action can change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    /// Changes nothing: queries, `load`, `hopen`, `log`.
    Read,
    /// Changes the session only: assignment, `.qpl.cfg key=value`.
    Session,
    /// Changes state outside the session: `sink`, `\1 <path>`, a write handle.
    Write,
}
