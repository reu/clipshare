use std::{
    borrow::Cow,
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
                    self.current_image.store(hashed, Ordering::SeqCst);
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

#[repr(u8)]
enum ClipboardObjectType {
    Text = 1,
    Image = 2,
}

impl ClipboardObject {
    pub async fn from_reader(
        mut reader: impl AsyncRead + Send + Unpin,
    ) -> Result<Self, Box<dyn Error + Send + Sync>> {
        let mut buf = [0; 1];
        if reader.read(&mut buf).await? == 0 {
            trace!("Stream closed");
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "stream closed").into());
        }

        trace!("Read kind {buf:?}");
        let kind = match buf[0] {
            1 => ClipboardObjectType::Text,
            2 => ClipboardObjectType::Image,
            n => return Err(format!("Invalid clipboard object type {n}").into()),
        };

        match kind {
            ClipboardObjectType::Text => {
                let mut buf = [0; mem::size_of::<usize>()];
                reader.read_exact(&mut buf).await?;
                let len = usize::from_be_bytes(buf);

                let mut buf = vec![0; len];
                reader.read_exact(&mut buf).await?;

                let text = std::str::from_utf8(&buf)?;
                Ok(Self::Text(text.to_string()))
            }

            ClipboardObjectType::Image => {
                let mut buf = [0; mem::size_of::<usize>()];
                reader.read_exact(&mut buf).await?;
                let width = usize::from_be_bytes(buf);

                let mut buf = [0; mem::size_of::<usize>()];
                reader.read_exact(&mut buf).await?;
                let height = usize::from_be_bytes(buf);

                let mut buf = [0; mem::size_of::<usize>()];
                reader.read_exact(&mut buf).await?;
                let len = usize::from_be_bytes(buf);
                trace!(width, height, len, "Received image");

                let mut buf = vec![0; len];
                reader.read_exact(&mut buf).await?;
                trace!(width, height, len, "Read image");

                let img = ImageData {
                    width,
                    height,
                    bytes: Cow::from(buf),
                };

                Ok(Self::Image(img))
            }
        }
    }

    pub async fn write(
        self,
        mut writer: impl AsyncWrite + Send + Unpin,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        let buf = match self {
            Self::Text(ref text) => {
                trace!(len = text.as_bytes().len(), "Sending text");

                [
                    &[ClipboardObjectType::Text as u8][..],
                    &text.as_bytes().len().to_be_bytes()[..],
                ]
                .concat()
            }

            Self::Image(ref img) => {
                trace!(
                    width = img.width,
                    height = img.height,
                    len = img.bytes.len(),
                    "Sending image"
                );

                [
                    &[ClipboardObjectType::Image as u8][..],
                    &img.width.to_be_bytes()[..],
                    &img.height.to_be_bytes()[..],
                    &img.bytes.len().to_be_bytes()[..],
                ]
                .concat()
            }
        };

        writer.write_all(&buf).await?;

        let buf = match self {
            Self::Text(ref text) => text.as_bytes(),
            Self::Image(ref img) => &img.bytes,
        };

        writer.write_all(buf).await?;
        trace!(len = buf.len(), "Clipboard sent");

        Ok(())
    }
}

fn hash(val: impl AsRef<[u8]>) -> u64 {
    let mut hasher = DefaultHasher::new();
    val.as_ref().hash(&mut hasher);
    hasher.finish()
}
