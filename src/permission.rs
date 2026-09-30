//! What an action is allowed to change. Every action that changes state is
//! classified by its [`Effect`], and [`Vm::authorize`](crate::vm::Vm::authorize)
//! decides whether the session may perform it:
//!
//! - a read-only session (every session not started with `qpl -w`) refuses
//!   [`Effect::Write`], from every source;
//! - a request over a read-only IPC handle also refuses [`Effect::IRead`]
//!   and [`Effect::Write`].
//!
//! User-facing docs call the three levels `read`, `iread` and `write`.

/// The most an action can change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    /// Reads session data only: queries, maths, `.qpl.dt`. Allowed
    /// everywhere, including over a read-only IPC handle.
    Read,
    /// Reads outside the session, or changes the session: `load`,
    /// assignment, `.qpl.cfg key=value`, `\1`. Refused over a read-only IPC
    /// handle, allowed everywhere else.
    IRead,
    /// Changes state outside the session: `sink`, `\1 <path>`, a write
    /// handle. Refused in a read-only session and over a read-only IPC
    /// handle.
    Write,
}
