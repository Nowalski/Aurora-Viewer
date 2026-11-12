//! Integration tests of the hub and the guest client over real loopback TCP.
//! A guest counts, and hears the others, once it has said Hello.
//! Each test starts its own hub on a free port and only uses the public API.

use std::io::{ErrorKind, Read, Write};
use std::net::TcpStream;
use std::thread;
use std::time::{Duration, Instant};

use uuid::Uuid;

use super::client::Client;
use super::hub::Hub;
use super::wire::{self, Msg, PeerAppearance, PeerState};

/// Polls `f` every 5 ms until it returns `Some`, and gives up after 3 s.
fn wait_for<T>(mut f: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(value) = f() {
            return value;
        }
        if Instant::now() >= deadline {
            panic!("timed out after 3 s waiting for a message or a state");
        }
        thread::sleep(Duration::from_millis(5));
    }
}

/// Every message currently queued on `next`.
fn drain(next: impl Fn() -> Option<Msg>) -> Vec<Msg> {
    std::iter::from_fn(next).collect()
}

/// Collects what `next` yields until `want` shows up; returns all of it, so a
/// test can also check what came before `want`.
fn await_msg(next: impl Fn() -> Option<Msg>, want: &Msg) -> Vec<Msg> {
    let mut seen = Vec::new();
    wait_for(|| {
        seen.extend(drain(&next));
        seen.contains(want).then_some(())
    });
    seen
}

fn addr(hub: &Hub) -> String {
    format!("127.0.0.1:{}", hub.port())
}

fn hello(id: Uuid, name: &str) -> Msg {
    Msg::Hello { id, name: name.into() }
}

fn chat(id: Uuid, text: &str) -> Msg {
    Msg::Chat {
        id,
        text: text.into(),
        chat_type: 1,
    }
}

fn appearance() -> PeerAppearance {
    PeerAppearance {
        te: "AAECAw==".into(),
        visual_params: "AQID".into(),
        hover: 0.1,
        cof_version: 3,
    }
}

fn peer_state(id: Uuid, x: f32, appearance: Option<PeerAppearance>) -> PeerState {
    PeerState {
        id,
        pos: [x, 128.0, 20.0],
        rot: [0.0, 0.0, 0.0, 1.0],
        vel: [0.5, 0.0, 0.0],
        flying: false,
        anims: vec![(Uuid::new_v4(), 1)],
        appearance,
        body_te: None,
    }
}

/// Reads until the hub closes the connection (0 bytes or an error). Data sent
/// before that is ignored; a connection still open after 3 s fails the test.
fn expect_closed(stream: &mut TcpStream) {
    stream.set_read_timeout(Some(Duration::from_millis(100))).expect("set read timeout");
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut buf = [0u8; 4096];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => return,
            Ok(_) => {}
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(_) => return,
        }
        assert!(Instant::now() < deadline, "the hub kept the connection open for more than 3 s");
    }
}

#[test]
fn guest_hello_and_state_reach_the_host() {
    let hub = Hub::start("127.0.0.1", 0).expect("start the hub");
    let guest = Client::connect(&addr(&hub)).expect("connect the guest");
    let id = Uuid::new_v4();
    let hello = hello(id, "Ada");
    let state = Msg::State(peer_state(id, 10.0, None));
    guest.send(hello.clone());
    guest.send(state.clone());

    assert_eq!(wait_for(|| hub.try_recv()), hello);
    assert_eq!(wait_for(|| hub.try_recv()), state);
    wait_for(|| (hub.guests() == 1).then_some(()));
}

#[test]
fn chat_reaches_the_other_guests_and_the_host_but_is_not_echoed() {
    let hub = Hub::start("127.0.0.1", 0).expect("start the hub");
    let (a_id, b_id) = (Uuid::new_v4(), Uuid::new_v4());
    let a = Client::connect(&addr(&hub)).expect("connect guest A");
    let b = Client::connect(&addr(&hub)).expect("connect guest B");
    a.send(hello(a_id, "A"));
    b.send(hello(b_id, "B"));
    wait_for(|| (hub.guests() == 2).then_some(()));

    let chat_b = chat(b_id, "hello from b");
    let chat_a = chat(a_id, "hello from a");
    b.send(chat_b.clone());
    // The hub has relayed B's chat before A speaks, so any echo to B would be
    // queued ahead of A's chat.
    await_msg(|| hub.try_recv(), &chat_b);
    a.send(chat_a.clone());

    let b_seen = await_msg(|| b.try_recv(), &chat_a);
    assert!(!b_seen.contains(&chat_b), "B received its own chat back");
    await_msg(|| hub.try_recv(), &chat_a);
    let a_seen = drain(|| a.try_recv());
    assert!(!a_seen.contains(&chat_a), "A received its own chat back");
}

#[test]
fn late_guest_receives_the_host_hello_and_state_first() {
    let hub = Hub::start("127.0.0.1", 0).expect("start the hub");
    let host_id = Uuid::new_v4();
    let hello = hello(host_id, "Host");
    let state = Msg::State(peer_state(host_id, 20.0, Some(appearance())));
    hub.send(hello.clone());
    hub.send(state.clone());

    let late = Client::connect(&addr(&hub)).expect("connect the late guest");
    // the hub answers a guest's Hello with what it already knows
    late.send(Msg::Hello {
        id: Uuid::new_v4(),
        name: "Late".into(),
    });
    assert_eq!(wait_for(|| late.try_recv()), hello);
    assert_eq!(wait_for(|| late.try_recv()), state);
}

#[test]
fn guest_cannot_speak_for_another_id() {
    let hub = Hub::start("127.0.0.1", 0).expect("start the hub");
    let (x_id, y_id, b_id) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let a = Client::connect(&addr(&hub)).expect("connect guest A");
    let b = Client::connect(&addr(&hub)).expect("connect guest B");
    a.send(hello(x_id, "A"));
    b.send(hello(b_id, "B"));
    wait_for(|| (hub.guests() == 2).then_some(()));

    // Spoofed chat, then a real one from the same connection: the spoof would
    // be queued ahead of the real chat if it had been relayed.
    a.send(chat(y_id, "spoofed"));
    let real = chat(x_id, "real");
    a.send(real.clone());

    let host_seen = await_msg(|| hub.try_recv(), &real);
    assert!(host_seen.iter().all(|m| m.id() != y_id), "the host received a spoofed message");
    let b_seen = await_msg(|| b.try_recv(), &real);
    assert!(b_seen.iter().all(|m| m.id() != y_id), "B received a spoofed message");
}

#[test]
fn dropped_guest_is_announced_with_bye() {
    let hub = Hub::start("127.0.0.1", 0).expect("start the hub");
    let x_id = Uuid::new_v4();
    let a = Client::connect(&addr(&hub)).expect("connect guest A");
    let b = Client::connect(&addr(&hub)).expect("connect guest B");
    a.send(hello(x_id, "A"));
    b.send(hello(Uuid::new_v4(), "B"));
    wait_for(|| (hub.guests() == 2).then_some(()));
    await_msg(|| hub.try_recv(), &hello(x_id, "A"));
    await_msg(|| b.try_recv(), &hello(x_id, "A"));

    drop(a);
    let bye = Msg::Bye { id: x_id };
    await_msg(|| hub.try_recv(), &bye);
    await_msg(|| b.try_recv(), &bye);
    wait_for(|| (hub.guests() == 1).then_some(()));
}

#[test]
fn first_message_that_is_not_hello_disconnects_the_guest() {
    let hub = Hub::start("127.0.0.1", 0).expect("start the hub");
    let mut stream = TcpStream::connect(addr(&hub)).expect("connect raw stream");
    let msg = chat(Uuid::new_v4(), "said before hello");
    stream.write_all(&wire::encode(&msg)).expect("write chat");

    expect_closed(&mut stream);
}

#[test]
fn line_longer_than_max_line_disconnects_the_guest() {
    let hub = Hub::start("127.0.0.1", 0).expect("start the hub");
    let mut stream = TcpStream::connect(addr(&hub)).expect("connect raw stream");
    stream.write_all(&wire::encode(&hello(Uuid::new_v4(), "Raw"))).expect("write hello");

    // Looks like a chat, but the line alone exceeds MAX_LINE. The hub may close
    // the socket while this is still being written, which also counts as closed.
    let line = format!(
        "{{\"t\":\"chat\",\"id\":\"{}\",\"text\":\"{}\",\"chat_type\":1}}\n",
        Uuid::new_v4(),
        "a".repeat(wire::MAX_LINE)
    );
    let _ = stream.write_all(line.as_bytes());

    expect_closed(&mut stream);
}
