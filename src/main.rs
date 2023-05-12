use std::{error::Error, io, process::exit, sync::Arc, time::Duration};

use async_compression::tokio::{bufread::GzipDecoder, write::GzipEncoder};
use clap::{command, Parser};
use clipboard::ClipboardObject;
use rustls::{client::ServerCertVerifier, Certificate, PrivateKey, ServerName};
use tokio::{
    io::{AsyncRead, AsyncWrite, BufReader},
    net::{TcpListener, TcpStream, UdpSocket},
    select,
    time::{sleep, timeout},
};
use tokio_rustls::{TlsAcceptor, TlsConnector};
use tracing::{debug, error_span, instrument, metadata::LevelFilter, trace, Instrument};
use tracing_error::ErrorLayer;
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

use crate::clipboard::Clipboard;

mod clipboard;

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

    let clipboard = Arc::new(Clipboard::new());
    match Cli::parse().clipboard {
        Some(port) => start_client(clipboard, port).await,
        None => start_server(clipboard).await,
    }
}

#[instrument(skip(clipboard))]
async fn start_server(clipboard: Arc<Clipboard>) -> Result<(), Box<dyn Error + Send + Sync>> {
    let socket = UdpSocket::bind("0.0.0.0:0").await?;
    socket.set_broadcast(true)?;
    let port = socket.local_addr()?.port();

    let cert = rcgen::generate_simple_self_signed([])?;
    let public_key = cert.serialize_der()?;
    let private_key = cert.serialize_private_key_der();

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

    let tls_acceptor = {
        let config = rustls::ServerConfig::builder()
            .with_safe_defaults()
            .with_no_client_auth()
            .with_single_cert(vec![Certificate(public_key)], PrivateKey(private_key))?;
        TlsAcceptor::from(Arc::new(config))
    };

    let listener = TcpListener::bind(("0.0.0.0", port)).await?;
    eprintln!("Run `clipshare {port}` on another machine of your network");

    while let Ok((stream, addr)) = listener.accept().await {
        let stream = tls_acceptor.accept(stream).await?;
        trace!("New connection arrived");
        let ip = addr.ip();
        let clipboard = clipboard.clone();
        tokio::spawn(
            async move {
                let (reader, writer) = tokio::io::split(stream);

                let reader = GzipDecoder::new(BufReader::new(reader));
                let writer = GzipEncoder::new(writer);

                if let Err(err) = select! {
                    result = recv_clipboard(clipboard.clone(), reader) => result,
                    result = send_clipboard(clipboard.clone(), writer) => result,
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

#[instrument(skip(clipboard))]
async fn start_client(
    clipboard: Arc<Clipboard>,
    clipboard_port: u16,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let socket = UdpSocket::bind(("0.0.0.0", clipboard_port)).await?;
    eprintln!("Connecting to clipboard {clipboard_port}...");
    let mut buf = [0_u8; 9];

    let Ok(Ok((_, addr))) = timeout(Duration::from_secs(5), socket.recv_from(&mut buf)).await else {
        eprintln!("Timeout trying to connect to clipboard {clipboard_port}");
        exit(1);
    };

    if &buf == HANDSHAKE {
        let tls_connector = {
            let config = rustls::ClientConfig::builder()
                .with_safe_defaults()
                .with_custom_certificate_verifier(Arc::new(NoCa))
                .with_no_client_auth();
            TlsConnector::from(Arc::new(config))
        };

        trace!("Begin client connection");
        let stream = TcpStream::connect(addr).await?;
        let ip = stream.peer_addr()?.ip();
        let stream = tls_connector
            .connect(ServerName::IpAddress(ip), stream)
            .await?;

        let (reader, writer) = tokio::io::split(stream);
        let span = error_span!("Connection", %ip).entered();
        eprintln!("Clipboards connected");

        let reader = GzipDecoder::new(BufReader::new(reader));
        let writer = GzipEncoder::new(writer);

        if let Err(err) = select! {
            result = recv_clipboard(clipboard.clone(), reader).in_current_span() => result,
            result = send_clipboard(clipboard.clone(), writer).in_current_span() => result,
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

#[instrument(skip(clipboard, stream))]
async fn send_clipboard(
    clipboard: Arc<Clipboard>,
    mut stream: impl AsyncWrite + Send + Unpin,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    loop {
        clipboard
            .paste()
            .in_current_span()
            .await?
            .write(&mut stream)
            .in_current_span()
            .await?;
    }
}

#[instrument(skip(clipboard, stream))]
async fn recv_clipboard(
    clipboard: Arc<Clipboard>,
    mut stream: impl AsyncRead + Send + Unpin,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    loop {
        let obj = ClipboardObject::from_reader(&mut stream)
            .in_current_span()
            .await?;
        clipboard.copy(obj).in_current_span().await?;
    }
}

struct NoCa;

impl ServerCertVerifier for NoCa {
    fn verify_server_cert(
        &self,
        _end_entity: &Certificate,
        _intermediates: &[Certificate],
        _server_name: &rustls::ServerName,
        _scts: &mut dyn Iterator<Item = &[u8]>,
        _ocsp_response: &[u8],
        _now: std::time::SystemTime,
    ) -> Result<rustls::client::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::ServerCertVerified::assertion())
    }
}
