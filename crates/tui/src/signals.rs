//! Scoped Unix exit handlers. The OS dispositions belong to the caller again
//! after the session, including when setup fails partway through.

use std::{
    io,
    sync::atomic::{AtomicBool, Ordering},
};

static ACTIVE: AtomicBool = AtomicBool::new(false);
static REQUESTED: AtomicBool = AtomicBool::new(false);

extern "C" fn request_exit(_: libc::c_int) {
    // Only a lock-free atomic store runs in signal context. Cleanup happens in
    // the event loop, and this static remains valid after a handler is removed.
    REQUESTED.store(true, Ordering::SeqCst);
}

pub(crate) struct ExitSignals {
    previous: Vec<(libc::c_int, libc::sigaction)>,
}

impl ExitSignals {
    pub(crate) fn new() -> io::Result<Self> {
        Self::install(&[libc::SIGINT, libc::SIGTERM, libc::SIGHUP])
    }

    fn install(signals: &[libc::c_int]) -> io::Result<Self> {
        // Signal dispositions and panic hooks are process-global. Reject an
        // overlapping session instead of overwriting another guard's state.
        let previous = Vec::with_capacity(signals.len());
        ACTIVE
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .map_err(|_| io::Error::other("a TUI session is already active"))?;
        let mut guard = Self { previous };
        REQUESTED.store(false, Ordering::SeqCst);
        let action = exit_action()?;
        for &signal in signals {
            let previous = replace_action(signal, &action)?;
            guard.previous.push((signal, previous));
        }
        Ok(guard)
    }

    pub(crate) fn requested(&self) -> bool {
        REQUESTED.load(Ordering::SeqCst)
    }
}

impl Drop for ExitSignals {
    fn drop(&mut self) {
        for (signal, previous) in self.previous.drain(..).rev() {
            // These are valid signals and complete dispositions returned by
            // sigaction, so restoration does not depend on allocation or I/O.
            let _ = replace_action(signal, &previous);
        }
        ACTIVE.store(false, Ordering::SeqCst);
    }
}

#[allow(unsafe_code)]
fn exit_action() -> io::Result<libc::sigaction> {
    // SAFETY: sigaction is a C struct of integer fields and a signal set, for
    // which zero initialization is valid. sigemptyset gets a valid writable set.
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    if unsafe { libc::sigemptyset(&mut action.sa_mask) } != 0 {
        return Err(io::Error::last_os_error());
    }
    action.sa_sigaction = request_exit as *const () as usize;
    action.sa_flags = libc::SA_RESTART;
    Ok(action)
}

#[allow(unsafe_code)]
fn replace_action(signal: libc::c_int, action: &libc::sigaction) -> io::Result<libc::sigaction> {
    let mut previous = std::mem::MaybeUninit::uninit();
    // SAFETY: action is initialized and previous points to writable storage.
    // On success sigaction initializes previous; on failure we never read it.
    if unsafe { libc::sigaction(signal, action, previous.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { previous.assume_init() })
}

#[cfg(test)]
mod tests {
    use super::*;

    static CALLER_REQUESTED: AtomicBool = AtomicBool::new(false);

    extern "C" fn caller_handler(_: libc::c_int) {
        CALLER_REQUESTED.store(true, Ordering::SeqCst);
    }

    #[allow(unsafe_code)]
    fn current_action(signal: libc::c_int) -> libc::sigaction {
        let mut action = std::mem::MaybeUninit::uninit();
        // SAFETY: a null new action only queries the disposition and the output
        // points to writable storage, read only after sigaction succeeds.
        assert_eq!(
            unsafe { libc::sigaction(signal, std::ptr::null(), action.as_mut_ptr()) },
            0
        );
        unsafe { action.assume_init() }
    }

    #[test]
    #[allow(unsafe_code)]
    fn restores_caller_dispositions_and_rolls_back_partial_install() {
        // Signal dispositions are process-global. Keep this test isolated from
        // the test harness and other terminal guards.
        const CHILD: &str = "KYORA_TEST_SIGNAL_DISPOSITION_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "signals::tests::restores_caller_dispositions_and_rolls_back_partial_install",
                ])
                .env(CHILD, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        for handler in [libc::SIG_IGN, caller_handler as *const () as usize] {
            let mut action = exit_action().unwrap();
            action.sa_sigaction = handler;
            // SAFETY: sa_mask is initialized and SIGUSR1 is a valid signal.
            assert_eq!(
                unsafe { libc::sigaddset(&mut action.sa_mask, libc::SIGUSR1) },
                0
            );
            for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
                let original = replace_action(signal, &action).unwrap();
                let before = current_action(signal);
                for _ in 0..2 {
                    CALLER_REQUESTED.store(false, Ordering::SeqCst);
                    let guard = ExitSignals::new().unwrap();
                    assert!(!guard.requested());
                    assert!(ExitSignals::new().is_err(), "overlapping guards must fail");
                    // SAFETY: the installed handler only sets a static flag.
                    assert_eq!(unsafe { libc::raise(signal) }, 0);
                    assert!(guard.requested());
                    assert!(!CALLER_REQUESTED.load(Ordering::SeqCst));
                    drop(guard);

                    let after = current_action(signal);
                    assert_eq!(after.sa_sigaction, before.sa_sigaction);
                    assert_eq!(after.sa_flags, before.sa_flags);
                    // SAFETY: both sets were initialized by sigaction.
                    assert_eq!(
                        unsafe { libc::sigismember(&after.sa_mask, libc::SIGUSR1) },
                        1
                    );
                    // SAFETY: the restored disposition is SIG_IGN or our handler.
                    assert_eq!(unsafe { libc::raise(signal) }, 0);
                    assert_eq!(
                        CALLER_REQUESTED.load(Ordering::SeqCst),
                        handler != libc::SIG_IGN
                    );
                }

                assert!(ExitSignals::install(&[signal, -1]).is_err());
                let after = current_action(signal);
                assert_eq!(after.sa_sigaction, before.sa_sigaction);
                assert_eq!(after.sa_flags, before.sa_flags);
                // Rollback also releases the session so installation can retry.
                drop(ExitSignals::new().unwrap());
                replace_action(signal, &original).unwrap();
            }
        }
    }
}
