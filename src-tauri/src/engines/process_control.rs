use crate::engines::EngineError;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[derive(Clone)]
pub struct ProcessControl {
    cancel: Arc<AtomicBool>,
    pause: Arc<AtomicBool>,
}

impl ProcessControl {
    pub fn new(cancel: Arc<AtomicBool>, pause: Arc<AtomicBool>) -> Self {
        Self { cancel, pause }
    }
    pub fn checkpoint_blocking(&self) -> Result<(), EngineError> {
        if self.cancel.load(Ordering::Relaxed) {
            return Err(EngineError::Cancelled);
        }
        while self.pause.load(Ordering::Relaxed) {
            if self.cancel.load(Ordering::Relaxed) {
                return Err(EngineError::Cancelled);
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cancellation_wins_over_pause() {
        let control = ProcessControl::new(
            Arc::new(AtomicBool::new(true)),
            Arc::new(AtomicBool::new(true)),
        );
        assert!(matches!(
            control.checkpoint_blocking(),
            Err(EngineError::Cancelled)
        ));
    }
}
