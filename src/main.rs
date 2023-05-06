use std::{
    collections::hash_map::DefaultHasher,
    error::Error,
    hash::{Hash, Hasher},
    io, mem,
    process::exit,
    time::Duration,
};

use arboard::{Clipboard, Error as ClipboardError};
use clap::{command, Parser};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream, UdpSocket},
    select,
    time::sleep,
};
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
    let Ok((_, addr)) = socket.recv_from(&mut buf).await else {
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

#[instrument(skip(stream))]
async fn send_clipboard(
    mut stream: impl AsyncWrite + Send + Unpin,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let mut clipboard = Clipboard::new().unwrap();
    let mut curr_paste = hash(&clipboard.get_text().unwrap_or_default());
    loop {
        let paste = clipboard.get_text().unwrap_or_default();
        let hashed = hash(&paste);
        if hashed != curr_paste {
            if !paste.is_empty() {
                let text = paste.as_bytes();
                let buf = [&text.len().to_be_bytes(), text].concat();
                trace!(text = paste, "Sent text");
                if stream.write(&buf).await? == 0 {
                    trace!("Stream closed");
                    break Ok(());
                }
            }
            curr_paste = hashed;
        }
        sleep(Duration::from_secs(1)).await;
    }
}

#[instrument(skip(stream))]
async fn recv_clipboard(
    mut stream: impl AsyncRead + Send + Unpin,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let mut clipboard = Clipboard::new().unwrap();
    loop {
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
}

fn hash(val: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    val.hash(&mut hasher);
    hasher.finish()
}
