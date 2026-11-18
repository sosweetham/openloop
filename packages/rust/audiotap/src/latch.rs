//! The capture block. Once anything suggests that a System Audio Recording prompt may be
//! pending, or that the permission is missing, no HAL call is made again until a person acts:
//! a tap operation issued while a prompt was pending once wedged coreaudiod, and with it audio
//! for every app on the Mac, for hours (docs/ARCHITECTURE.md § Audio). A marker file carries the
//! block across process restarts, so a relaunched service cannot retry either.

use std::fs;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};

use crate::events::BlockReason;

pub(crate) struct Latch {
    blocked: Mutex<Option<BlockReason>>,
    granted: AtomicBool,
}

pub(crate) static PROCESS: Latch = Latch::new();

impl Latch {
    pub const fn new() -> Latch {
        Latch {
            blocked: Mutex::new(None),
            granted: AtomicBool::new(false),
        }
    }

    pub fn blocked(&self) -> Option<BlockReason> {
        self.blocked
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The first reason stays. The marker records it for later processes.
    pub fn block(&self, reason: &BlockReason, marker: Option<&Path>) -> io::Result<()> {
        let mut blocked = self.blocked.lock().unwrap_or_else(PoisonError::into_inner);
        if blocked.is_none() {
            *blocked = Some(reason.clone());
        }
        match marker {
            Some(path) if !path.exists() => fs::write(path, format!("{reason}\n")),
            _ => Ok(()),
        }
    }

    pub fn load_marker(&self, marker: &Path) {
        if let Ok(text) = fs::read_to_string(marker) {
            let mut blocked = self.blocked.lock().unwrap_or_else(PoisonError::into_inner);
            blocked.get_or_insert(BlockReason::Recorded(text));
        }
    }

    pub fn clear(&self, marker: Option<&Path>) -> io::Result<()> {
        if let Some(path) = marker {
            match fs::remove_file(path) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
                _ => {}
            }
        }
        *self.blocked.lock().unwrap_or_else(PoisonError::into_inner) = None;
        Ok(())
    }

    pub fn grant(&self) {
        self.granted.store(true, Ordering::Relaxed);
    }

    /// Whether an IOProc in this process has received audio, which proves the permission.
    pub fn granted(&self) -> bool {
        self.granted.load(Ordering::Relaxed)
    }
}

/// Why capture is blocked in this process, if it is.
pub fn blocked() -> Option<BlockReason> {
    PROCESS.blocked()
}

/// Lifts the block and deletes `marker`. Call it only for an explicit action by the person at
/// the Mac, after they have answered the prompt or changed the setting in System Settings.
pub fn clear_block(marker: Option<&Path>) -> io::Result<()> {
    PROCESS.clear(marker)
}

/// Whether capture has received audio in this process, which proves the permission.
pub fn permission_proven() -> bool {
    PROCESS.granted()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_reason_wins_and_the_marker_outlives_the_process() {
        let dir = std::env::temp_dir().join(format!("audiotap-latch-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("blocked");
        let _ = fs::remove_file(&marker);

        let latch = Latch::new();
        assert_eq!(latch.blocked(), None);
        let first = BlockReason::NoCallbacks { waited_ms: 5000 };
        latch.block(&first, Some(&marker)).unwrap();
        latch
            .block(&BlockReason::Recorded("later".into()), Some(&marker))
            .unwrap();
        assert_eq!(latch.blocked(), Some(first.clone()));

        let next_process = Latch::new();
        next_process.load_marker(&marker);
        let Some(BlockReason::Recorded(text)) = next_process.blocked() else {
            panic!()
        };
        assert_eq!(text.trim(), first.to_string());

        next_process.clear(Some(&marker)).unwrap();
        assert_eq!(next_process.blocked(), None);
        assert!(!marker.exists());
        next_process.clear(Some(&marker)).unwrap();
        fs::remove_dir_all(&dir).unwrap();
    }
}
