//! Opt-in, bounded thread lifecycle diagnostics, without thread arguments.
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

pub(super) fn event(action: &str, thread: u64, result: i32) {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    static COUNT: AtomicUsize = AtomicUsize::new(0);
    if !*ENABLED.get_or_init(|| std::env::var_os("ROSEMARY_TRACE_PTHREAD").is_some()) {
        return;
    }
    #[allow(deprecated)] // try_update is not available on older supported Rust.
    let Ok(sequence) = COUNT.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
        (n < 256).then_some(n + 1)
    }) else {
        return;
    };
    eprintln!(
        "rosemary: pthread sequence={} action={action} handle={thread:#x} result={result}",
        sequence + 1
    );
    // Stack traces contain code locations, never the guest's start argument or
    // returned value. Only failures need the more expensive unwind.
    if result != 0 {
        eprintln!(
            "rosemary: pthread failure stack: {}",
            std::backtrace::Backtrace::force_capture()
        );
    }
}
