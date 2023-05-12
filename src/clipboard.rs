use std::{
    collections::hash_map::DefaultHasher,
    error::Error,
    fmt,
    hash::{Hash, Hasher},
    io, mem,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use arboard::ImageData;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    sync::Mutex,
    time::sleep,
};
use tracing::trace;

pub struct Clipboard {
    clipboard: Mutex<arboard::Clipboard>,
    current_text: AtomicU64,
    current_image: AtomicU64,
}

impl fmt::Debug for Clipboard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Clipboard")
            .field("current_text", &self.current_text)
            .field("current_image", &self.current_image)
            .finish()
    }
}

impl Clipboard {
    pub fn new() -> Self {
        let mut clipboard = arboard::Clipboard::new().unwrap();
        let current_text = AtomicU64::new(clipboard.get_text().map(hash).unwrap_or_default());
        let current_image = AtomicU64::new(
            clipboard
                .get_image()
                .map(|img| hash(img.bytes))
                .unwrap_or_default(),
        );
        Self {
            clipboard: Mutex::new(clipboard),
            current_text,
            current_image,
        }
    }

    pub async fn copy(
        &self,
        obj: impl Into<ClipboardObject>,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        let obj = obj.into();
        let hashed = hash(&obj);

        match obj {
            ClipboardObject::Text(text) => {
                if self.current_text.load(Ordering::SeqCst) != hashed {
                    self.clipboard.lock().await.set_text(text)?;
                    self.current_text.store(hashed, Ordering::SeqCst);
                }
            }
            ClipboardObject::Image(img) => {
                if self.current_image.load(Ordering::SeqCst) != hashed {
                    self.clipboard.lock().await.set_image(img)?;
                    self.current_image.store(hashed, Ordering::SeqCst);
                }
            }
        };
        Ok(())
    }

    pub async fn paste(&self) -> Result<ClipboardObject, Box<dyn Error + Send + Sync>> {
        loop {
            let mut clip = self.clipboard.lock().await;

            let paste = clip.get_text().unwrap_or_default();
            let hashed = hash(&paste);
            if !paste.is_empty() && hashed != self.current_text.load(Ordering::SeqCst) {
                self.current_text.store(hashed, Ordering::SeqCst);
                break Ok(ClipboardObject::Text(paste));
            }

            if let Ok(paste) = clip.get_image() {
                let hashed = hash(&paste.bytes);
                if !paste.bytes.is_empty() && hashed != self.current_image.load(Ordering::SeqCst) {
                    self.current_text.store(hashed, Ordering::SeqCst);
                    break Ok(ClipboardObject::Image(paste));
                }
            }
            sleep(Duration::from_secs(1)).await;
        }
    }
}

#[derive(Debug)]
pub enum ClipboardObject {
    Text(String),
    Image(ImageData<'static>),
}

impl AsRef<[u8]> for ClipboardObject {
    fn as_ref(&self) -> &[u8] {
        match self {
            Self::Text(txt) => txt.as_ref(),
            Self::Image(img) => img.bytes.as_ref(),
        }
    }
}

impl ClipboardObject {
    pub async fn from_reader(
        mut reader: impl AsyncRead + Send + Unpin,
    ) -> Result<Self, Box<dyn Error + Send + Sync>> {
        let mut buf = [0; mem::size_of::<usize>()];
        if reader.read(&mut buf).await? == 0 {
            trace!("Stream closed");
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "stream closed").into());
        }
        let len = usize::from_be_bytes(buf);
        let mut buf = vec![0; len];
        reader.read_exact(&mut buf).await?;

        let text = std::str::from_utf8(&buf)?;
        Ok(Self::Text(text.to_string()))
    }

    pub async fn write(
        self,
        mut writer: impl AsyncWrite + Send + Unpin,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        match self {
            Self::Text(text) => {
                let bytes = text.as_bytes();
                let buf = [&bytes.len().to_be_bytes(), bytes].concat();
                trace!(text, "Sent text");
                if writer.write(&buf).await? == 0 {
                    trace!("Stream closed");
                    return Err(io::Error::new(io::ErrorKind::BrokenPipe, "stream closed").into());
                }
            }
            Self::Image(img) => {
                let bytes = img.bytes;
                let buf = [&bytes.len().to_be_bytes(), bytes.as_ref()].concat();
                if writer.write(&buf).await? == 0 {
                    trace!("Stream closed");
                    return Err(io::Error::new(io::ErrorKind::BrokenPipe, "stream closed").into());
                }
            },
        };
        Ok(())
    }
}

fn hash(val: impl AsRef<[u8]>) -> u64 {
    let mut hasher = DefaultHasher::new();
    val.as_ref().hash(&mut hasher);
    hasher.finish()
}
