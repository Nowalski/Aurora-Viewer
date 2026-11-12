//! A guest's connection to a Hub (experimental, AURORA_INSTANCE_JOIN). A reader
//! and a writer thread own the socket, so the frame loop never waits on the
//! network: it only drains an inbox and fills a bounded outbox.

use std::io::{self, BufRead, BufReader, Write};
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, TrySendError, bounded, unbounded};

use super::wire::{self, Msg};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Messages waiting for the writer. A full outbox drops the newest message
/// rather than stalling the frame: the next state update replaces it anyway.
const OUTBOX: usize = 512;

pub struct Client {
    /// Kept only to shut the socket down on drop.
    socket: TcpStream,
    outbox: Sender<Msg>,
    inbox: Receiver<Msg>,
    connected: Arc<AtomicBool>,
}

impl Client {
    /// Connects to the first address of `addr` that accepts within 5 s.
    pub fn connect(addr: &str) -> io::Result<Client> {
        let mut last_err = None;
        let mut stream = None;
        for target in addr.to_socket_addrs()? {
            match TcpStream::connect_timeout(&target, CONNECT_TIMEOUT) {
                Ok(s) => {
                    stream = Some(s);
                    break;
                }
                Err(e) => last_err = Some(e),
            }
        }
        let stream =
            stream.ok_or_else(|| last_err.unwrap_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no address to connect to")))?;
        // Small state messages must not wait for Nagle's delay.
        stream.set_nodelay(true)?;

        let reader_stream = stream.try_clone()?;
        let writer_stream = stream.try_clone()?;
        let (inbox_tx, inbox) = unbounded();
        let (outbox, outbox_rx) = bounded(OUTBOX);
        let connected = Arc::new(AtomicBool::new(true));

        let flag = Arc::clone(&connected);
        std::thread::Builder::new()
            .name("instance-client-rx".into())
            .spawn(move || read_messages(reader_stream, inbox_tx, &flag))?;
        let flag = Arc::clone(&connected);
        std::thread::Builder::new()
            .name("instance-client-tx".into())
            .spawn(move || write_messages(writer_stream, outbox_rx, &flag))?;

        Ok(Client {
            socket: stream,
            outbox,
            inbox,
            connected,
        })
    }

    /// Queues a message for the host. Never blocks: it is dropped when the
    /// outbox is full or the link is gone.
    pub fn send(&self, msg: Msg) {
        match self.outbox.try_send(msg) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => log::warn!("instance: outbox full, message dropped"),
            Err(TrySendError::Disconnected(_)) => {}
        }
    }

    /// The next valid message from the host, if any.
    pub fn try_recv(&self) -> Option<Msg> {
        self.inbox.try_recv().ok()
    }

    pub fn connected(&self) -> bool {
        self.connected.load(Ordering::Acquire)
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        // The reader gets EOF and the writer a failed write, so both threads end.
        // The outbox sender is dropped with the struct, which also ends the writer.
        let _ = self.socket.shutdown(Shutdown::Both);
    }
}

fn read_messages(stream: TcpStream, inbox: Sender<Msg>, connected: &AtomicBool) {
    let mut reader = BufReader::new(stream);
    let mut line = Vec::new();
    loop {
        line.clear();
        match read_bounded_line(&mut reader, &mut line) {
            Ok(true) => {}
            Ok(false) => break,
            Err(e) => {
                log::warn!("instance: closing the link: {e}");
                break;
            }
        }
        match wire::decode(&line) {
            Some(msg) => {
                if inbox.send(msg).is_err() {
                    break;
                }
            }
            // Never log the contents: they come from the network.
            None => log::warn!("instance: invalid message from the host dropped"),
        }
    }
    mark_lost(connected, &"the host closed the link");
}

fn write_messages(mut stream: TcpStream, outbox: Receiver<Msg>, connected: &AtomicBool) {
    for msg in outbox {
        if let Err(e) = stream.write_all(&wire::encode(&msg)) {
            mark_lost(connected, &e);
            return;
        }
    }
}

/// Reads up to the next newline into `line` (newline removed). `Ok(false)`
/// at end of stream. A line longer than `MAX_LINE` is an error, so a peer
/// cannot make this viewer allocate without bound.
fn read_bounded_line<R: BufRead>(reader: &mut R, line: &mut Vec<u8>) -> io::Result<bool> {
    loop {
        let buf = reader.fill_buf()?;
        if buf.is_empty() {
            return Ok(false);
        }
        let (take, done) = match buf.iter().position(|&b| b == b'\n') {
            Some(i) => (i, true),
            None => (buf.len(), false),
        };
        if line.len() + take > wire::MAX_LINE {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "line too long"));
        }
        line.extend_from_slice(&buf[..take]);
        reader.consume(if done { take + 1 } else { take });
        if done {
            return Ok(true);
        }
    }
}

/// Clears the flag and logs once, whichever thread notices first.
fn mark_lost(connected: &AtomicBool, why: &dyn std::fmt::Display) {
    if connected.swap(false, Ordering::AcqRel) {
        log::info!("instance: connection to the host lost ({why})");
    }
}
