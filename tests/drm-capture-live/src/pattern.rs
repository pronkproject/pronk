//! Fullscreen changing content; run only on the disposable test compositor.

extern "C" {
    fn capture_pattern_client();
}

fn main() {
    // SAFETY: The helper initializes GTK and runs its loop on the main thread.
    unsafe { capture_pattern_client() };
}
