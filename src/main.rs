use std::{
    collections::hash_map::DefaultHasher,
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

const HANDSHAKE: &[u8; 9] = b"clipshare";

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// Clipboard id to connect to
    clipboard: Option<u16>,
}

fn main() -> io::Result<()> {
    match Cli::parse().clipboard {
        Some(port) => start_client(port),
        None => start_server(),
    }
}

fn start_server() -> io::Result<()> {
    let socket = UdpSocket::bind("0.0.0.0:0")?;
    let port = socket.local_addr()?.port();

    thread::spawn(move || {
        socket.set_broadcast(true)?;
        loop {
            if socket.send_to(HANDSHAKE, ("255.255.255.255", port))? == 0 {
                break;
            }
            sleep(Duration::from_secs(3));
        }
        io::Result::Ok(())
    });

    let listener = TcpListener::bind(("0.0.0.0", port))?;
    eprintln!("Run `clipshare {port}` on another machine of your network");

    for stream in listener.incoming() {
        thread::spawn(move || {
            let reader = stream?;
            let writer = reader.try_clone()?;
            thread::spawn(move || send_clipboard(writer));
            recv_clipboard(reader)?;
            io::Result::Ok(())
        });
    }

    Ok(())
}

fn start_client(clipboard_port: u16) -> io::Result<()> {
    let socket = UdpSocket::bind(("0.0.0.0", clipboard_port))?;
    socket.set_read_timeout(Some(Duration::from_secs(5)))?;
    eprintln!("Connecting to clipboard {clipboard_port}...");
    let mut buf = [0_u8; 9];
    let Ok((_, addr)) = socket.recv_from(&mut buf) else {
        eprintln!("Timeout trying to connect to clipboard {clipboard_port}");
        exit(1);
    };
    if &buf == HANDSHAKE {
        let reader = TcpStream::connect(addr)?;
        let writer = reader.try_clone()?;
        eprintln!("Clipboards connected");
        thread::spawn(move || send_clipboard(writer));
        recv_clipboard(reader)?;
        eprintln!("Clipboard closed");
        Ok(())
    } else {
        eprintln!("Clipboard {clipboard_port} not found");
        exit(1);
    }
}

fn send_clipboard(mut stream: impl Write) -> io::Result<()> {
    let mut clipboard = Clipboard::new().unwrap();
    let mut curr_paste = hash(&clipboard.get_text().unwrap_or_default());
    loop {
        let paste = clipboard.get_text().unwrap_or_default();
        let hashed = hash(&paste);
        if hashed != curr_paste {
            if !paste.is_empty() {
                let text = paste.as_bytes();
                let buf = [&text.len().to_be_bytes(), text].concat();
                if stream.write(&buf)? == 0 {
                    break Ok(());
                }
            }
            curr_paste = hashed;
        }
        sleep(Duration::from_secs(1));
    }
}

fn recv_clipboard(mut stream: impl Read) -> io::Result<()> {
    let mut clipboard = Clipboard::new().unwrap();
    loop {
        let mut buf = [0; mem::size_of::<usize>()];
        if stream.read(&mut buf)? == 0 {
            break Ok(());
        }
        let len = usize::from_be_bytes(buf);
        let mut buf = vec![0; len];
        stream.read_exact(&mut buf)?;

        if let Ok(text) = std::str::from_utf8(&buf) {
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
