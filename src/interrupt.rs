//! Cooperative Ctrl-C: a flag the signal handler sets and the interpreter polls.
//!
//! While `rustyline` is reading a line the terminal is in raw mode, so Ctrl-C
//! never becomes a signal there. While a statement *runs* it does, and (with no
//! handler) it kills the whole process. `main.rs` installs a handler that calls
//! [`Interrupt::on_ctrl_c`]; the interpreter calls [`Interrupt::check`] at the
//! points listed in `CONTROL_FLOW_PLAN.md` (each `while` iteration, function
//! entry, statement entry, either side of a Polars `collect`, IPC waits), which
//! turns a requested interrupt into [`QplError::Interrupted`].
//!
//! The flag lives on each `Vm` rather than in a global `static`, so parallel
//! tests can't interrupt each other. It is a pair of atomics behind an `Arc`
//! — the only state `Vm` shares with another thread, and never a lock.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::errors::QplError;

#[derive(Default)]
struct State {
    /// A Ctrl-C arrived while a statement was running.
    requested: AtomicBool,
    /// A top-level statement is in flight (see [`Interrupt::statement`]).
    running: AtomicBool,
}

#[derive(Clone, Default)]
pub struct Interrupt(Arc<State>);

/// What the Ctrl-C handler should do, as decided by [`Interrupt::on_ctrl_c`].
#[derive(Debug, PartialEq, Eq)]
pub enum CtrlC {
    /// Nothing is running, or this is the second Ctrl-C for the same
    /// statement: leave the process (exit code 130).
    Exit,
    /// First Ctrl-C for a running statement: it has been flagged and will stop
    /// at its next check point.
    Interrupting,
}

impl Interrupt {
    /// `Err(QplError::Interrupted)` once a Ctrl-C has been requested.
    pub fn check(&self) -> Result<(), QplError> {
        if self.0.requested.load(Ordering::Relaxed) {
            Err(QplError::Interrupted)
        } else {
            Ok(())
        }
    }

    /// Mark the start of one top-level statement. Entering the outermost
    /// statement clears any stale request, and so does leaving it; a nested one
    /// (`\l` running a script from a statement) just restores the previous
    /// `running` on drop.
    pub fn statement(&self) -> StatementGuard {
        let prev = self.0.running.swap(true, Ordering::Relaxed);
        if !prev {
            self.0.requested.store(false, Ordering::Relaxed);
        }
        StatementGuard { state: self.0.clone(), prev }
    }

    /// Called from the signal-handler thread.
    pub fn on_ctrl_c(&self) -> CtrlC {
        if !self.0.running.load(Ordering::Relaxed) || self.0.requested.swap(true, Ordering::Relaxed) {
            CtrlC::Exit
        } else {
            CtrlC::Interrupting
        }
    }

    /// Request an interrupt directly (tests, embedders).
    pub fn request(&self) {
        self.0.requested.store(true, Ordering::Relaxed);
    }
}

/// RAII guard returned by [`Interrupt::statement`].
pub struct StatementGuard {
    state: Arc<State>,
    prev: bool,
}

impl Drop for StatementGuard {
    fn drop(&mut self) {
        if !self.prev {
            self.state.requested.store(false, Ordering::Relaxed);
        }
        self.state.running.store(self.prev, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_is_ok_until_requested() {
        let i = Interrupt::default();
        assert!(i.check().is_ok());
        i.request();
        assert!(matches!(i.check(), Err(QplError::Interrupted)));
    }

    #[test]
    fn statement_guard_clears_stale_request_and_nests() {
        let i = Interrupt::default();
        i.request();
        let outer = i.statement();
        assert!(i.check().is_ok(), "entering the outermost statement clears a stale request");
        {
            let _inner = i.statement();
            i.request();
        }
        // the inner guard must not have cleared it, nor marked us not-running
        assert!(i.check().is_err());
        assert_eq!(i.on_ctrl_c(), CtrlC::Exit);
        drop(outer);
        assert!(i.check().is_ok(), "leaving the outermost statement clears the request");
    }

    #[test]
    fn ctrl_c_escalates_only_while_running() {
        let i = Interrupt::default();
        assert_eq!(i.on_ctrl_c(), CtrlC::Exit, "idle: exit like an unhandled SIGINT");
        let _g = i.statement();
        assert_eq!(i.on_ctrl_c(), CtrlC::Interrupting);
        assert_eq!(i.on_ctrl_c(), CtrlC::Exit, "second Ctrl-C forces quit");
    }

    #[test]
    fn a_guard_dropped_during_a_panic_still_releases_the_statement() {
        let i = Interrupt::default();
        let i2 = i.clone();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _g = i2.statement();
            i2.request();
            panic!("boom");
        }));
        assert!(i.check().is_ok());
        assert_eq!(i.on_ctrl_c(), CtrlC::Exit, "nothing is running any more");
    }

    #[test]
    fn interrupts_are_per_vm_not_global() {
        let a = Interrupt::default();
        let b = Interrupt::default();
        a.request();
        assert!(a.check().is_err());
        assert!(b.check().is_ok());
    }

    #[test]
    fn a_clone_shares_the_flag() {
        let a = Interrupt::default();
        let handler_side = a.clone();
        let _g = a.statement();
        assert_eq!(handler_side.on_ctrl_c(), CtrlC::Interrupting);
        assert!(a.check().is_err());
    }
}
