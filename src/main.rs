use std::{
    borrow::Cow,
    collections::hash_map::DefaultHasher,
    error::Error,
    hash::{Hash, Hasher},
    io, mem,
    pin::Pin,
    process::exit,
    task,
    time::Duration,
};

use arboard::{Clipboard, Error as ClipboardError, ImageData};
use clap::{command, Parser};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream, UdpSocket},
    select,
    sync::mpsc,
    time::{sleep, timeout},
};
use tokio_stream::{wrappers::ReceiverStream, Stream, StreamExt};
use tracing::{debug, error_span, instrument, metadata::LevelFilter, trace, Instrument};
use tracing_error::ErrorLayer;
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

const HANDSHAKE: &[u8; 9] = b"clipshare";

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// Clipboard id to connect to
    clipboard: Option<u16>,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(
            EnvFilter::builder()
                .with_default_directive(LevelFilter::ERROR.into())
                .from_env_lossy(),
        )
        .with(ErrorLayer::default())
        .init();

    match Cli::parse().clipboard {
        Some(port) => start_client(port).await,
        None => start_server().await,
    }
}

#[instrument]
async fn start_server() -> Result<(), Box<dyn Error + Send + Sync>> {
    let socket = UdpSocket::bind("0.0.0.0:0").await?;
    socket.set_broadcast(true)?;
    let port = socket.local_addr()?.port();

    tokio::spawn(
        async move {
            loop {
                if socket.send_to(HANDSHAKE, ("255.255.255.255", port)).await? == 0 {
                    debug!("Failed to send UDP packet");
                    break;
                }
                sleep(Duration::from_secs(3)).await;
            }
            io::Result::Ok(())
        }
        .instrument(error_span!("Port publishing", port)),
    );

    let listener = TcpListener::bind(("0.0.0.0", port)).await?;
    eprintln!("Run `clipshare {port}` on another machine of your network");

    while let Ok((mut stream, addr)) = listener.accept().await {
        trace!("New connection arrived");
        let ip = addr.ip();
        tokio::spawn(
            async move {
                let (reader, writer) = stream.split();
                if let Err(err) = select! {
                    result = recv_clipboard(reader) => result,
                    result = send_clipboard(writer) => result,
                } {
                    debug!(error = %err, "Server error");
                }
                trace!("Finishing server connection");
                Ok::<_, Box<dyn Error + Send + Sync>>(())
            }
            .instrument(error_span!("Connection", %ip)),
        );
    }

    Ok(())
}

#[instrument]
async fn start_client(clipboard_port: u16) -> Result<(), Box<dyn Error + Send + Sync>> {
    let socket = UdpSocket::bind(("0.0.0.0", clipboard_port)).await?;
    eprintln!("Connecting to clipboard {clipboard_port}...");
    let mut buf = [0_u8; 9];

    let Ok(Ok((_, addr))) = timeout(Duration::from_secs(5), socket.recv_from(&mut buf)).await else {
        eprintln!("Timeout trying to connect to clipboard {clipboard_port}");
        exit(1);
    };

    if &buf == HANDSHAKE {
        trace!("Begin client connection");
        let mut stream = TcpStream::connect(addr).await?;
        let (reader, writer) = stream.split();
        let ip = reader.peer_addr()?.ip();
        let span = error_span!("Connection", %ip).entered();
        eprintln!("Clipboards connected");

        if let Err(err) = select! {
            result = recv_clipboard(reader).in_current_span() => result,
            result = send_clipboard(writer).in_current_span() => result,
        } {
            debug!(error = %err, "Client error");
        }

        trace!("Finish client connection");
        span.exit();
        eprintln!("Clipboard closed");
        Ok(())
    } else {
        eprintln!("Clipboard {clipboard_port} not found");
        exit(1);
    }
}

#[repr(u8)]
enum ClipboardObjectType {
    Text = 1,
    Image = 2,
}

#[instrument(skip(stream))]
async fn send_clipboard(
    mut stream: impl AsyncWrite + Send + Unpin,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let text_changes = ClipboardStream::new(Clipboard::new().unwrap()).map(|text| {
        [
            &[ClipboardObjectType::Text as u8][..],
            &text.len().to_be_bytes()[..],
            text.as_bytes(),
        ]
        .concat()
    });

    let image_changes = ClipboardImageStream::new(Clipboard::new().unwrap()).map(|image| {
        [
            &[ClipboardObjectType::Image as u8][..],
            &image.width.to_be_bytes()[..],
            &image.height.to_be_bytes()[..],
            &image.bytes.len().to_be_bytes()[..],
            &image.bytes,
        ]
        .concat()
    });

    let mut changes = text_changes.merge(image_changes);

    while let Some(data) = changes.next().await {
        if stream.write(&data).await? == 0 {
            trace!("Stream closed");
            return Ok(());
        }
    }

    Ok(())
}

#[instrument(skip(stream))]
async fn recv_clipboard(
    mut stream: impl AsyncRead + Send + Unpin,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let mut clipboard = Clipboard::new().unwrap();
    loop {
        let mut buf = [0; 1];
        if stream.read(&mut buf).await? == 0 {
            trace!("Stream closed");
            break Ok(());
        }
        let kind = match buf[0] {
            1 => ClipboardObjectType::Text,
            2 => ClipboardObjectType::Image,
            n => break Err(format!("Invalid clipboard object type {n}").into()),
        };

        match kind {
            ClipboardObjectType::Text => {
                let mut buf = [0; mem::size_of::<usize>()];
                if stream.read(&mut buf).await? == 0 {
                    trace!("Stream closed");
                    break Ok(());
                }
                let len = usize::from_be_bytes(buf);
                let mut buf = vec![0; len];
                stream.read_exact(&mut buf).await?;

                if let Ok(text) = std::str::from_utf8(&buf) {
                    trace!(text = text, "Received text");
                    while let Err(ClipboardError::ClipboardOccupied) = clipboard.set_text(text) {
                        sleep(Duration::from_secs(1)).await;
                    }
                }
            }

            ClipboardObjectType::Image => {
                let mut buf = [0; mem::size_of::<usize>()];
                if stream.read(&mut buf).await? == 0 {
                    trace!("Stream closed");
                    break Ok(());
                }
                let width = usize::from_be_bytes(buf);

                let mut buf = [0; mem::size_of::<usize>()];
                if stream.read(&mut buf).await? == 0 {
                    trace!("Stream closed");
                    break Ok(());
                }
                let height = usize::from_be_bytes(buf);

                let mut buf = [0; mem::size_of::<usize>()];
                if stream.read(&mut buf).await? == 0 {
                    trace!("Stream closed");
                    break Ok(());
                }
                let len = usize::from_be_bytes(buf);
                let mut buf = vec![0; len];
                stream.read_exact(&mut buf).await?;

                let img = ImageData {
                    width,
                    height,
                    bytes: Cow::from(buf),
                };

                trace!(width, height, "Received image");
                clipboard.set_image(img).unwrap();
            }
        }
    }
}

fn hash(val: impl AsRef<[u8]>) -> u64 {
    let mut hasher = DefaultHasher::new();
    val.as_ref().hash(&mut hasher);
    hasher.finish()
}

struct ClipboardStream {
    stream: ReceiverStream<String>,
}

impl ClipboardStream {
    pub fn new(mut clipboard: Clipboard) -> Self {
        let (tx, rx) = mpsc::channel(5);
        tokio::spawn(async move {
            let mut curr_paste = hash(&clipboard.get_text().unwrap_or_default());
            loop {
                let paste = clipboard.get_text().unwrap_or_default();
                let hashed = hash(&paste);
                if hashed != curr_paste {
                    if !paste.is_empty() {
                        trace!(text = paste, "Sent text");
                        if tx.send(paste).await.is_err() {
                            break Ok::<_, Box<dyn Error + Send + Sync>>(());
                        }
                    }
                    curr_paste = hashed;
                }
                sleep(Duration::from_secs(1)).await;
            }
        });

        Self {
            stream: ReceiverStream::new(rx),
        }
    }
}

impl Stream for ClipboardStream {
    type Item = String;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
    ) -> task::Poll<Option<Self::Item>> {
        self.stream.as_mut().poll_recv(cx)
    }
}

struct ClipboardImageStream {
    stream: ReceiverStream<ImageData<'static>>,
}

impl ClipboardImageStream {
    pub fn new(mut clipboard: Clipboard) -> Self {
        let (tx, rx) = mpsc::channel(5);
        tokio::spawn(async move {
            let data = clipboard.get_image().unwrap();
            let mut curr_paste = hash(&data.bytes);
            loop {
                let paste = clipboard.get_image().unwrap();
                let hashed = hash(&paste.bytes);
                if hashed != curr_paste {
                    if !paste.bytes.is_empty() {
                        trace!("Sent image");
                        if tx.send(paste).await.is_err() {
                            break Ok::<_, Box<dyn Error + Send + Sync>>(());
                        }
                    }
                    curr_paste = hashed;
                }
                sleep(Duration::from_secs(1)).await;
            }
        });

        Self {
            stream: ReceiverStream::new(rx),
        }
    }
}

impl Stream for ClipboardImageStream {
    type Item = ImageData<'static>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
    ) -> task::Poll<Option<Self::Item>> {
        self.stream.as_mut().poll_recv(cx)
    }
}
