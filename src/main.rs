use std::{
    collections::hash_map::DefaultHasher,
    error::Error,
    hash::{Hash, Hasher},
    io::{self, Read, Write},
    mem,
    net::{TcpListener, TcpStream, UdpSocket},
    process::exit,
    thread::{self, sleep},
    time::Duration,
};

use arboard::{Clipboard, Error as ClipboardError};
use clap::{command, Parser};
use tracing::{debug, error_span, instrument, metadata::LevelFilter, trace};
use tracing_error::{ErrorLayer, InstrumentResult};
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

const HANDSHAKE: &[u8; 9] = b"clipshare";

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// Clipboard id to connect to
    clipboard: Option<u16>,
}

fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
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
        Some(port) => start_client(port),
        None => start_server(),
    }
}

#[instrument]
fn start_server() -> Result<(), Box<dyn Error + Send + Sync>> {
    let socket = UdpSocket::bind("0.0.0.0:0")?;
    let port = socket.local_addr()?.port();

    thread::spawn(move || {
        let span = error_span!("Port publishing", port).entered();
        socket.set_broadcast(true)?;
        loop {
            if socket.send_to(HANDSHAKE, ("255.255.255.255", port))? == 0 {
                debug!("Failed to send UDP packet");
                break;
            }
            sleep(Duration::from_secs(3));
        }
        span.exit();
        io::Result::Ok(())
    });

    let listener = TcpListener::bind(("0.0.0.0", port))?;
    eprintln!("Run `clipshare {port}` on another machine of your network");

    for stream in listener.incoming() {
        trace!("New connection arrived");
        thread::spawn(move || {
            let reader = stream.in_current_span()?;
            let ip = reader.peer_addr()?.ip();
            let span = error_span!("Connection", %ip).entered();
            let writer = reader.try_clone().in_current_span()?;
            thread::spawn(move || send_clipboard(writer));
            recv_clipboard(reader)?;
            trace!("Finishing server connection");
            span.exit();
            Ok::<_, Box<dyn Error + Send + Sync>>(())
        });
    }

    Ok(())
}

#[instrument]
fn start_client(clipboard_port: u16) -> Result<(), Box<dyn Error + Send + Sync>> {
    let socket = UdpSocket::bind(("0.0.0.0", clipboard_port))?;
    socket.set_read_timeout(Some(Duration::from_secs(5)))?;
    eprintln!("Connecting to clipboard {clipboard_port}...");
    let mut buf = [0_u8; 9];
    let Ok((_, addr)) = socket.recv_from(&mut buf) else {
        eprintln!("Timeout trying to connect to clipboard {clipboard_port}");
        exit(1);
    };
    if &buf == HANDSHAKE {
        trace!("Begin client connection");
        let reader = TcpStream::connect(addr).in_current_span()?;
        let ip = reader.peer_addr()?.ip();
        let span = error_span!("Connection", %ip).entered();
        let writer = reader.try_clone().in_current_span()?;
        eprintln!("Clipboards connected");
        thread::spawn(move || send_clipboard(writer));
        recv_clipboard(reader)?;
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
fn send_clipboard(mut stream: impl Write) -> Result<(), Box<dyn Error + Send + Sync>> {
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
                if stream.write(&buf).in_current_span()? == 0 {
                    trace!("Stream closed");
                    break Ok(());
                }
            }
            curr_paste = hashed;
        }
        sleep(Duration::from_secs(1));
    }
}

#[instrument(skip(stream))]
fn recv_clipboard(mut stream: impl Read) -> Result<(), Box<dyn Error + Send + Sync>> {
    let mut clipboard = Clipboard::new().unwrap();
    loop {
        let mut buf = [0; mem::size_of::<usize>()];
        if stream.read(&mut buf)? == 0 {
            trace!("Stream closed");
            break Ok(());
        }
        let len = usize::from_be_bytes(buf);
        let mut buf = vec![0; len];
        stream.read_exact(&mut buf).in_current_span()?;

        if let Ok(text) = std::str::from_utf8(&buf) {
            trace!(text = text, "Received text");
            while let Err(ClipboardError::ClipboardOccupied) = clipboard.set_text(text) {
                sleep(Duration::from_secs(1));
            }
        }
    }
}

fn hash(val: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    val.hash(&mut hasher);
    hasher.finish()
}
