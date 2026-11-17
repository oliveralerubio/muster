//! Process death for the development-only crash-injection hooks.
//!
//! Injected crashes must look like an abrupt kill to the rest of the system.
//! `abort()` does that but raises SIGABRT, so systemd-coredump writes a core
//! dump and the desktop shows a "Process crashed" notification on every test
//! run. SIGKILL is the same abrupt death (no destructors, no flushing), but
//! the kernel never writes a core dump for it. Never use `abort()` for
//! injected crashes.
#![allow(unsafe_code)]

/// Print `message` to stderr, then kill this process with SIGKILL.
pub fn die(message: std::fmt::Arguments<'_>) -> ! {
    eprintln!("{message}");
    // SAFETY: `getpid` and `kill` take and return plain integers and touch no
    // Rust memory; signalling our own pid is always valid.
    unsafe {
        libc::kill(libc::getpid(), libc::SIGKILL);
    }
    // Unreachable in practice: SIGKILL cannot be caught or blocked.
    std::process::exit(137)
}
