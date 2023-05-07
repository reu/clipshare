use std::{
    collections::hash_map::DefaultHasher,
    error::Error,
    fmt,
    hash::{Hash, Hasher},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use arboard::Error as ClipboardError;
use tokio::{sync::Mutex, time::sleep};

pub struct Clipboard {
    clipboard: Mutex<arboard::Clipboard>,
    current: AtomicU64,
}

impl fmt::Debug for Clipboard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Clipboard")
            .field("current", &self.current)
            .finish()
    }
}

impl Clipboard {
    pub fn new() -> Self {
        let mut clipboard = arboard::Clipboard::new().unwrap();
        let current = AtomicU64::new(hash(&clipboard.get_text().unwrap_or_default()));
        Self {
            clipboard: Mutex::new(clipboard),
            current,
        }
    }

    pub async fn copy(&self, text: &str) -> Result<(), Box<dyn Error + Send + Sync>> {
        let hashed = hash(text);
        if self.current.load(Ordering::SeqCst) != hashed {
            while let Err(ClipboardError::ClipboardOccupied) =
                self.clipboard.lock().await.set_text(text)
            {
                sleep(Duration::from_secs(1)).await;
            }
            self.current.store(hashed, Ordering::SeqCst);
        }
        Ok(())
    }

    pub async fn paste(&self) -> Result<String, Box<dyn Error + Send + Sync>> {
        loop {
            let paste = self.clipboard.lock().await.get_text().unwrap_or_default();
            let hashed = hash(&paste);
            if !paste.is_empty() && hashed != self.current.load(Ordering::SeqCst) {
                self.current.store(hashed, Ordering::SeqCst);
                break Ok(paste);
            }
            sleep(Duration::from_secs(1)).await;
        }
    }
}

fn hash(val: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    val.hash(&mut hasher);
    hasher.finish()
}
