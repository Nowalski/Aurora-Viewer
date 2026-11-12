//! The hosting viewer's TCP server of a shared instance. It accepts up to
//! `MAX_GUESTS` connections, relays each guest's messages to the other guests
//! and to the host, and keeps the last state of every peer, so that a new
//! guest sees everyone at once. Threads: one accept loop, then per connection
//! a reader and a writer.

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, TrySendError, bounded, unbounded};
use parking_lot::Mutex;
use uuid::Uuid;

use super::wire::{self, Msg};

/// Connections accepted at once, Hello or not.
const MAX_GUESTS: usize = 32;
/// Lines queued for one guest before it is considered too slow.
const GUEST_QUEUE: usize = 512;
/// How often the accept loop checks the stop flag.
const POLL: Duration = Duration::from_millis(100);

/// One encoded line, shared by every guest it is sent to.
type Line = Arc<[u8]>;

pub struct Hub {
    port: u16,
    shared: Arc<Shared>,
    host_rx: Receiver<Msg>,
    accept: Option<JoinHandle<()>>,
}

struct Shared {
    stop: AtomicBool,
    /// Guests' messages and their Bye, for the host. Unbounded: at most
    /// 32 guests send about 10 states a second, and the host drains it each frame.
    host_tx: Sender<Msg>,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    next_id: u64,
    conns: HashMap<u64, Conn>,
    cache: HashMap<Uuid, PeerCache>,
}

struct Conn {
    /// Second handle on the socket, to shut it down from another thread.
    stream: TcpStream,
    /// Queue of the writer thread. Set once the guest has said Hello, so
    /// only identified guests receive the relayed messages.
    tx: Option<Sender<Line>>,
    addr: SocketAddr,
}

/// What a new guest needs to see a peer: its last Hello, its last State
/// carrying an appearance, and its latest State.
#[derive(Default)]
struct PeerCache {
    hello: Option<Msg>,
    appearance: Option<Msg>,
    last: Option<Msg>,
}

impl Hub {
    /// Binds `bind:port` (port 0 picks a free one) and starts accepting.
    pub fn start(bind: &str, port: u16) -> io::Result<Hub> {
        let listener = TcpListener::bind((bind, port))?;
        let port = listener.local_addr()?.port();
        // Non-blocking, so the accept loop can notice the stop flag.
        listener.set_nonblocking(true)?;
        let (host_tx, host_rx) = unbounded();
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            host_tx,
            state: Mutex::new(State::default()),
        });
        let accept_shared = shared.clone();
        let accept = thread::Builder::new()
            .name("instance-hub".into())
            .spawn(move || accept_loop(&listener, &accept_shared))?;
        log::info!("instance: hosting on port {port}");
        Ok(Hub {
            port,
            shared,
            host_rx,
            accept: Some(accept),
        })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// The host's own message: cached for the guests that come later, then
    /// sent to every guest.
    pub fn send(&self, msg: Msg) {
        let line: Line = wire::encode(&msg).into();
        let mut st = self.shared.state.lock();
        remember(&mut st.cache, &msg);
        fanout(&mut st, &line, None);
    }

    /// The next message of a guest, or the Bye of a guest that left.
    pub fn try_recv(&self) -> Option<Msg> {
        self.host_rx.try_recv().ok()
    }

    /// Guests that have said Hello.
    #[cfg(test)]
    pub fn guests(&self) -> usize {
        self.shared.state.lock().conns.values().filter(|c| c.tx.is_some()).count()
    }
}

impl Drop for Hub {
    fn drop(&mut self) {
        // Set before taking the lock: a connection accepted meanwhile sees it
        // and is refused, so none escapes the shutdown below.
        self.shared.stop.store(true, Ordering::Release);
        for c in self.shared.state.lock().conns.values() {
            let _ = c.stream.shutdown(Shutdown::Both);
        }
        if let Some(accept) = self.accept.take() {
            let _ = accept.join();
        }
    }
}

fn accept_loop(listener: &TcpListener, shared: &Arc<Shared>) {
    while !shared.stop.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((stream, addr)) => accept_one(shared, stream, addr),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => thread::sleep(POLL),
            Err(e) => {
                log::warn!("instance: accept failed: {e}");
                thread::sleep(POLL);
            }
        }
    }
}

fn accept_one(shared: &Arc<Shared>, stream: TcpStream, addr: SocketAddr) {
    // An accepted socket may inherit the listener's non-blocking mode.
    let _ = stream.set_nonblocking(false);
    if let Err(e) = stream.set_nodelay(true) {
        log::warn!("instance: no TCP_NODELAY for {addr}: {e}");
    }
    let (Ok(kill), Ok(writer_stream)) = (stream.try_clone(), stream.try_clone()) else {
        log::warn!("instance: cannot duplicate the socket of {addr}");
        return;
    };
    let (tx, rx) = bounded::<Line>(GUEST_QUEUE);
    let conn = {
        let mut st = shared.state.lock();
        if shared.stop.load(Ordering::Acquire) || st.conns.len() >= MAX_GUESTS {
            log::info!("instance: connection from {addr} refused, the hub is full or closing");
            return;
        }
        let id = st.next_id;
        st.next_id += 1;
        st.conns.insert(
            id,
            Conn {
                stream: kill,
                tx: None,
                addr,
            },
        );
        id
    };
    log::info!("instance: connection from {addr}");
    let shared_reader = shared.clone();
    let spawned = thread::Builder::new()
        .name("instance-guest-writer".into())
        .spawn(move || write_loop(writer_stream, rx))
        .and_then(|_| {
            thread::Builder::new()
                .name("instance-guest".into())
                .spawn(move || serve(shared_reader, conn, stream, tx, addr))
        });
    if let Err(e) = spawned {
        log::warn!("instance: no thread for {addr}: {e}");
        shared.state.lock().conns.remove(&conn);
    }
}

/// Writer of one guest: sends its queue in order, then shuts the socket so
/// the reader ends too.
fn write_loop(mut stream: TcpStream, rx: Receiver<Line>) {
    for line in rx {
        if stream.write_all(&line).is_err() {
            break;
        }
    }
    let _ = stream.shutdown(Shutdown::Both);
}

/// Reader of one guest: validates its lines, relays them, and cleans up
/// when it leaves.
fn serve(shared: Arc<Shared>, conn: u64, stream: TcpStream, tx: Sender<Line>, addr: SocketAddr) {
    let mut reader = BufReader::new(stream);
    let mut line = Vec::new();
    let mut peer: Option<Uuid> = None;
    let mut said_bye = false;
    loop {
        match read_line_bounded(&mut reader, &mut line) {
            Ok(true) => {}
            Ok(false) => break,
            Err(e) => {
                log::info!("instance: guest {addr} dropped: {e}");
                break;
            }
        }
        let Some(msg) = wire::decode(&line) else {
            log::debug!("instance: invalid message from {addr} ignored");
            continue;
        };
        match peer {
            None => {
                // The first valid message must be the Hello.
                let id = match msg {
                    Msg::Hello { id, .. } => id,
                    _ => {
                        log::info!("instance: guest {addr} did not start with Hello");
                        break;
                    }
                };
                let mut st = shared.state.lock();
                // Queued before this guest is added to the relay, so the
                // snapshot always comes first in its queue.
                for m in snapshot(&st.cache, id) {
                    // At most three messages per peer: far below the queue size.
                    let _ = tx.try_send(wire::encode(&m).into());
                }
                let arc: Line = wire::encode(&msg).into();
                fanout(&mut st, &arc, None);
                remember(&mut st.cache, &msg);
                if let Some(c) = st.conns.get_mut(&conn) {
                    c.tx = Some(tx.clone());
                }
                drop(st);
                let _ = shared.host_tx.send(msg);
                peer = Some(id);
            }
            Some(pid) => {
                // A guest speaks only for its own id, and says Hello once.
                if msg.id() != pid || matches!(msg, Msg::Hello { .. } | Msg::Present { .. }) {
                    continue;
                }
                let bye = matches!(msg, Msg::Bye { .. });
                let arc: Line = wire::encode(&msg).into();
                let mut st = shared.state.lock();
                fanout(&mut st, &arc, Some(conn));
                remember(&mut st.cache, &msg);
                drop(st);
                let _ = shared.host_tx.send(msg);
                if bye {
                    said_bye = true;
                    break;
                }
            }
        }
    }

    let mut st = shared.state.lock();
    st.conns.remove(&conn);
    if let Some(id) = peer {
        st.cache.remove(&id);
        // A guest that said Bye already had it relayed.
        if !said_bye {
            let bye = Msg::Bye { id };
            let arc: Line = wire::encode(&bye).into();
            fanout(&mut st, &arc, None);
            let _ = shared.host_tx.send(bye);
        }
    }
    log::info!("instance: guest {addr} disconnected");
}

/// Reads up to the next newline into `out` (newline excluded). `Ok(false)` at
/// end of stream. A line past `MAX_LINE` is an error and is never buffered
/// beyond that limit.
fn read_line_bounded(r: &mut impl BufRead, out: &mut Vec<u8>) -> io::Result<bool> {
    out.clear();
    loop {
        let buf = r.fill_buf()?;
        if buf.is_empty() {
            return Ok(false);
        }
        let newline = buf.iter().position(|&b| b == b'\n');
        let take = newline.unwrap_or(buf.len());
        if out.len() + take > wire::MAX_LINE {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "line too long"));
        }
        out.extend_from_slice(&buf[..take]);
        r.consume(take + usize::from(newline.is_some()));
        if newline.is_some() {
            return Ok(true);
        }
    }
}

/// Queues `line` to every connected guest except `except`. A guest whose
/// queue is full is too slow: it is cut off, and its reader cleans up.
fn fanout(st: &mut State, line: &Line, except: Option<u64>) {
    for (id, c) in st.conns.iter_mut() {
        if Some(*id) == except {
            continue;
        }
        let Some(tx) = &c.tx else { continue };
        match tx.try_send(line.clone()) {
            Ok(()) | Err(TrySendError::Disconnected(_)) => {}
            Err(TrySendError::Full(_)) => {
                log::warn!("instance: guest {} is too slow, disconnected", c.addr);
                c.tx = None;
                let _ = c.stream.shutdown(Shutdown::Both);
            }
        }
    }
}

/// Keeps what a new guest needs to see a peer. A Bye forgets the peer.
fn remember(cache: &mut HashMap<Uuid, PeerCache>, msg: &Msg) {
    match msg {
        Msg::Hello { id, .. } | Msg::Present { id, .. } => cache.entry(*id).or_default().hello = Some(msg.clone()),
        Msg::State(s) => {
            let entry = cache.entry(s.id).or_default();
            if s.appearance.is_some() {
                entry.appearance = Some(msg.clone());
            }
            entry.last = Some(msg.clone());
        }
        Msg::Chat { .. } => {}
        Msg::Bye { id } => {
            cache.remove(id);
        }
    }
}

/// What a guest with id `joining` is sent first: the Hello of every other
/// peer, so each is known, then its appearance and its latest state.
fn snapshot(cache: &HashMap<Uuid, PeerCache>, joining: Uuid) -> Vec<Msg> {
    let others = || cache.iter().filter(|(id, _)| **id != joining);
    let mut out: Vec<Msg> = others().filter_map(|(_, p)| p.hello.clone()).collect();
    for (_, p) in others() {
        out.extend(p.appearance.clone());
        out.extend(p.last.clone().filter(|l| Some(l) != p.appearance.as_ref()));
    }
    out
}
