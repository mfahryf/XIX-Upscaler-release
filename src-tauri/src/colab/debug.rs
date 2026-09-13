//! Opt-in terminal diagnostics for the desktop ↔ Google Drive ↔ Colab path.
//!
//! The logger is deliberately small and only accepts caller-provided stage
//! messages. Callers must never pass tokens, OAuth codes, email addresses,
//! local paths, or Drive session URLs here.

use std::{fmt::Display, sync::OnceLock, time::Instant};

const ENV_NAME: &str = "XIX_COLAB_DEBUG";
static START: OnceLock<Instant> = OnceLock::new();

pub(super) fn event(stage: &str, message: impl Display) {
    if !enabled() {
        return;
    }
    let elapsed = START.get_or_init(Instant::now).elapsed().as_millis();
    eprintln!("[COLAB-DEBUG +{elapsed}ms] {stage}: {message}");
}

fn enabled() -> bool {
    matches!(
        std::env::var(ENV_NAME).ok().as_deref(),
        Some("1" | "true" | "TRUE" | "yes" | "YES" | "on" | "ON")
    )
}
