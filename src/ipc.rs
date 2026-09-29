//! IPC: `hopen`/`dispatch`/`async dispatch`/`await` (client) and `\port`
//! (server). `ipc` feature only.
//!
//! One zeromq REQ/REP pair per connection (pure Rust, no libzmq). Async code
//! is confined to one thread per client connection and one for the listener;
//! they exchange only owned `String`/`Vec<u8>` values with the main thread
//! over `mpsc`, never a reference into `Vm`.
//!
//! A command travels as source text and is evaluated like a REPL line. The
//! response mirrors `vm::EvalResult`: a tag byte, then Parquet bytes for a
//! table or a [`crate::codec`] value for a scalar.

use std::io::Cursor;
use std::sync::mpsc;
use std::thread;

use polars::prelude::*;

use crate::ast;
use crate::errors::QplError;
use crate::interrupt::Interrupt;
use crate::vm::EvalResult;

fn rt<E: std::fmt::Display>(e: E) -> QplError {
    QplError::Runtime(e.to_string())
}

/// The error's bare message, sent on the wire and rebuilt as a `Runtime`
/// error on the other side.
fn error_message(e: &QplError) -> String {
    match e {
        QplError::Lex(m) | QplError::Parse(m) | QplError::Compile(m) | QplError::Runtime(m) => {
            m.clone()
        }
        QplError::Interrupted => "interrupted".to_string(),
    }
}

/// `hopen 5001` → `tcp://127.0.0.1:5001`; `hopen "host:5001"` → `tcp://host:5001`.
fn to_tcp_uri(addr: &str) -> String {
    if addr.contains("://") {
        addr.to_string()
    } else if addr.contains(':') {
        format!("tcp://{addr}")
    } else {
        format!("tcp://127.0.0.1:{addr}")
    }
}

/// A short hex tag for a peer, to tell clients apart in connect/disconnect logs.
fn short_peer_id(id: &zeromq::util::PeerIdentity) -> String {
    id.iter().take(4).map(|b| format!("{b:02x}")).collect()
}

fn build_runtime() -> Result<tokio::runtime::Runtime, QplError> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(rt)
}

// ---------------------------------------------------------------------------
// client: hopen / dispatch / async dispatch / await
// ---------------------------------------------------------------------------

/// A connection's permission, chosen by the client (`hopen` = `Read`,
/// `` `w!hopen `` = `Write`) and enforced by the server per request. Sent as a
/// tag byte before the command text. Never applies to local input.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HandleMode {
    Read,
    Write,
}

impl HandleMode {
    fn tag(self) -> u8 {
        match self {
            HandleMode::Read => b'R',
            HandleMode::Write => b'W',
        }
    }

    fn from_tag(b: u8) -> Option<Self> {
        match b {
            b'R' => Some(HandleMode::Read),
            b'W' => Some(HandleMode::Write),
            _ => None,
        }
    }
}

type ReplyTx = mpsc::Sender<Result<EvalResult, QplError>>;
pub type ReplyRx = mpsc::Receiver<Result<EvalResult, QplError>>;
type ConnRequest = (String, ReplyTx);

/// A `hopen` connection: a channel to its worker thread, which owns the
/// `ReqSocket` and handles one request at a time (as REQ requires).
pub struct ClientConn {
    tx: mpsc::Sender<ConnRequest>,
    pub mode: HandleMode,
}

/// `hopen <addr>`: connect and spawn the worker thread. Blocks until the
/// connection succeeds or fails, so a bad address is reported immediately.
pub fn hopen(addr: &str, mode: HandleMode) -> Result<ClientConn, QplError> {
    let uri = to_tcp_uri(addr);
    let (tx, rx) = mpsc::channel::<ConnRequest>();
    let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();

    thread::spawn(move || {
        let rt = match build_runtime() {
            Ok(rt) => rt,
            Err(e) => {
                let _ = ready_tx.send(Err(e.to_string()));
                return;
            }
        };
        rt.block_on(async move {
            use zeromq::Socket;
            let mut req = zeromq::ReqSocket::new();
            if let Err(e) = req.connect(&uri).await {
                let _ = ready_tx.send(Err(e.to_string()));
                return;
            }
            let _ = ready_tx.send(Ok(()));
            while let Ok((command, reply_tx)) = rx.recv() {
                let result = dispatch_once(&mut req, mode, &command).await;
                let _ = reply_tx.send(result);
            }
        });
    });

    match ready_rx.recv() {
        Ok(Ok(())) => Ok(ClientConn { tx, mode }),
        Ok(Err(e)) => Err(QplError::Runtime(format!("hopen '{addr}': {e}"))),
        Err(_) => Err(QplError::Runtime(format!(
            "hopen '{addr}': connection thread died"
        ))),
    }
}

async fn dispatch_once(
    req: &mut zeromq::ReqSocket,
    mode: HandleMode,
    command: &str,
) -> Result<EvalResult, QplError> {
    use zeromq::{SocketRecv, SocketSend};
    let mut payload = vec![mode.tag()];
    payload.extend_from_slice(command.as_bytes());
    req.send(payload.into()).await.map_err(rt)?;
    let msg = req.recv().await.map_err(rt)?;
    let bytes: Vec<u8> = msg
        .try_into()
        .map_err(|e: &str| QplError::Runtime(e.into()))?;
    decode_response(&bytes)
}

/// Queue `command` on `conn`'s worker and return the reply channel. Sync
/// dispatch waits on it; async stores it for `await`.
pub fn enqueue(conn: &ClientConn, command: String) -> Result<ReplyRx, QplError> {
    let (reply_tx, reply_rx) = mpsc::channel();
    conn.tx
        .send((command, reply_tx))
        .map_err(|_| QplError::Runtime("connection is closed".into()))?;
    Ok(reply_rx)
}

/// Wait for a reply, waking every 50 ms to check for Ctrl-C. An abandoned
/// reply is discarded by the worker, so the connection stays usable.
fn recv_interruptible(rx: &ReplyRx, interrupt: &Interrupt) -> Result<EvalResult, QplError> {
    loop {
        interrupt.check()?;
        match rx.recv_timeout(std::time::Duration::from_millis(50)) {
            Ok(reply) => return reply,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(QplError::Runtime(
                    "connection closed before replying".into(),
                ));
            }
        }
    }
}

/// Blocking `dispatch`: enqueue and wait for the reply inline.
pub fn dispatch_blocking(
    conn: &ClientConn,
    command: String,
    interrupt: &Interrupt,
) -> Result<EvalResult, QplError> {
    recv_interruptible(&enqueue(conn, command)?, interrupt)
}

/// `await`: wait on an async dispatch's reply channel. Borrows it so an
/// interrupted wait can be retried.
pub fn await_reply(rx: &ReplyRx, interrupt: &Interrupt) -> Result<EvalResult, QplError> {
    recv_interruptible(rx, interrupt)
}

// ---------------------------------------------------------------------------
// server: `\port`
// ---------------------------------------------------------------------------

/// A request forwarded from the listener to the main thread: the
/// connection's mode, the command text, and where to send the encoded reply.
pub type PortRequest = (HandleMode, String, mpsc::Sender<Vec<u8>>);

/// An open `\port` listener, owned by `Vm`: the listener thread, the channel
/// requests arrive on, and the port (for display).
pub struct PortState {
    pub handle: ServerHandle,
    pub rx: mpsc::Receiver<PortRequest>,
    pub port: u16,
}

impl PortState {
    /// Bind and start a listener on `port`.
    pub fn open(port: u16) -> Result<Self, QplError> {
        let (tx, rx) = mpsc::channel();
        let handle = start_server(port, tx)?;
        Ok(Self { handle, rx, port })
    }
}

/// A running listener. Dropping it (or `close`) stops accepting requests and
/// joins the thread; a request already in flight finishes first.
pub struct ServerHandle {
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl ServerHandle {
    pub fn close(mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Bind and start the listener thread. Blocks until the bind succeeds or
/// fails, so a busy port is reported immediately.
pub fn start_server(
    port: u16,
    main_tx: mpsc::Sender<PortRequest>,
) -> Result<ServerHandle, QplError> {
    let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();

    let thread = thread::spawn(move || {
        let rt = match build_runtime() {
            Ok(rt) => rt,
            Err(e) => {
                let _ = ready_tx.send(Err(e.to_string()));
                return;
            }
        };
        rt.block_on(async move {
            use zeromq::Socket;
            let mut rep = zeromq::RepSocket::new();
            // registered before `bind` so no `Accepted` event is missed
            let mut events = rep.monitor();
            if let Err(e) = rep.bind(&format!("tcp://127.0.0.1:{port}")).await {
                let _ = ready_tx.send(Err(e.to_string()));
                return;
            }
            let _ = ready_tx.send(Ok(()));
            loop {
                use zeromq::{SocketRecv, SocketSend};
                tokio::select! {
                    _ = &mut shutdown_rx => break,
                    // accept/close events, logged for the operator only.
                    // This zeromq version emits `Disconnected` only on a
                    // protocol error, so a clean client exit goes unlogged.
                    Ok(event) = events.recv() => {
                        match event {
                            zeromq::SocketEvent::Accepted(endpoint, peer_id) => {
                                println!("qpl: client connected from {endpoint} ({})", short_peer_id(&peer_id));
                            }
                            zeromq::SocketEvent::Disconnected(peer_id) => {
                                println!("qpl: client disconnected ({})", short_peer_id(&peer_id));
                            }
                            _ => {}
                        }
                    }
                    recv = rep.recv() => {
                        let msg = match recv {
                            Ok(msg) => msg,
                            Err(_) => continue,
                        };
                        let bytes: Vec<u8> = match msg.try_into() {
                            Ok(bytes) => bytes,
                            Err(_) => continue,
                        };
                        let (mode, command) = match bytes.split_first() {
                            Some((&tag, rest)) => match HandleMode::from_tag(tag) {
                                Some(mode) => (mode, String::from_utf8_lossy(rest).into_owned()),
                                None => continue,
                            },
                            None => continue,
                        };
                        // hand off to the main thread and wait; REP can't take
                        // another request until this one replies anyway
                        let (reply_tx, reply_rx) = mpsc::channel();
                        if main_tx.send((mode, command, reply_tx)).is_err() {
                            break;
                        }
                        let response = reply_rx.recv().unwrap_or_default();
                        let _ = rep.send(response.into()).await;
                    }
                }
            }
        });
    });

    match ready_rx.recv() {
        Ok(Ok(())) => Ok(ServerHandle {
            shutdown_tx: Some(shutdown_tx),
            thread: Some(thread),
        }),
        Ok(Err(e)) => Err(QplError::Runtime(format!("\\port {port}: {e}"))),
        Err(_) => Err(QplError::Runtime(format!(
            "\\port {port}: listener thread died"
        ))),
    }
}

// ---------------------------------------------------------------------------
// wire format
// ---------------------------------------------------------------------------

const TAG_ERROR: u8 = 0;
const TAG_STORED: u8 = 1;
const TAG_LAZY: u8 = 2;
const TAG_SCALAR: u8 = 3;
const TAG_TABLE: u8 = 4;

/// Encode a command's outcome for the wire. Errors are encoded too, so the
/// client always gets a reply.
pub fn encode_result(result: &Result<EvalResult, QplError>) -> Vec<u8> {
    match result {
        Err(e) => {
            let mut out = vec![TAG_ERROR];
            out.extend_from_slice(error_message(e).as_bytes());
            out
        }
        Ok(EvalResult::Stored) => vec![TAG_STORED],
        Ok(EvalResult::Lazy(plan)) => {
            let mut out = vec![TAG_LAZY];
            out.extend_from_slice(plan.as_bytes());
            out
        }
        Ok(EvalResult::Scalar(v)) => {
            let mut out = vec![TAG_SCALAR];
            encode_scalar(v, &mut out);
            out
        }
        Ok(EvalResult::Table(df)) => {
            let mut out = vec![TAG_TABLE];
            let mut df = df.clone();
            // Parquet in memory, reusing the writer `sink` already links
            if ParquetWriter::new(&mut out).finish(&mut df).is_err() {
                return encode_result(&Err(QplError::Runtime(
                    "failed to serialise table for dispatch".into(),
                )));
            }
            out
        }
    }
}

fn decode_response(bytes: &[u8]) -> Result<EvalResult, QplError> {
    let (&tag, rest) = bytes
        .split_first()
        .ok_or_else(|| rt("empty dispatch response"))?;
    match tag {
        TAG_ERROR => Err(QplError::Runtime(
            String::from_utf8_lossy(rest).into_owned(),
        )),
        TAG_STORED => Ok(EvalResult::Stored),
        TAG_LAZY => Ok(EvalResult::Lazy(String::from_utf8_lossy(rest).into_owned())),
        TAG_SCALAR => Ok(EvalResult::Scalar(crate::codec::decode_value(
            &mut crate::codec::Reader::new(rest),
        )?)),
        TAG_TABLE => {
            let df = ParquetReader::new(Cursor::new(rest.to_vec()))
                .finish()
                .map_err(rt)?;
            Ok(EvalResult::Table(df))
        }
        other => Err(QplError::Runtime(format!(
            "unknown dispatch response tag {other}"
        ))),
    }
}

/// Encode a scalar response. Values with no codec encoding (handles,
/// closures, frames) are sent as the text `"<unrepresentable>"`.
fn encode_scalar(v: &ast::Value, out: &mut Vec<u8>) {
    if crate::codec::encode_value(v, out).is_err() {
        crate::codec::encode_value(&ast::Value::Str("<unrepresentable>".into()), out)
            .expect("a Str value always encodes");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip_value(v: ast::Value) -> ast::Value {
        let mut buf = Vec::new();
        encode_scalar(&v, &mut buf);
        crate::codec::decode_value(&mut crate::codec::Reader::new(&buf)).expect("decode")
    }

    #[test]
    fn value_round_trip_covers_every_scalar_kind() {
        let cases = vec![
            ast::Value::Int(-42),
            ast::Value::Float(3.5),
            ast::Value::Str("hello".into()),
            ast::Value::Sym("sym".into()),
            ast::Value::Bool(true),
            ast::Value::Bool(false),
            ast::Value::Date(123),
            ast::Value::Month(-7),
            ast::Value::Time(456),
            ast::Value::Minute(90),
            ast::Value::Second(3600),
            ast::Value::Timestamp(789),
            ast::Value::Timespan(-1000),
            ast::int_vec(vec![1, 2, 3]),
            ast::float_vec(vec![1.5, 2.5]),
            ast::sym_vec(vec!["a".into(), "b".into()]),
            ast::str_vec(vec!["x".into(), "y".into()]),
            ast::bool_vec(vec![true, false, true]),
        ];
        for v in cases {
            assert_eq!(roundtrip_value(v.clone()), v);
        }
    }

    #[test]
    fn value_round_trip_handles_empty_vectors_and_strings() {
        assert_eq!(
            roundtrip_value(ast::Value::Str("".into())),
            ast::Value::Str("".into())
        );
        assert_eq!(roundtrip_value(ast::int_vec(vec![])), ast::int_vec(vec![]));
    }

    #[test]
    fn response_round_trip_stored_lazy_scalar() {
        for result in [
            Ok(EvalResult::Stored),
            Ok(EvalResult::Lazy("PLAN".into())),
            Ok(EvalResult::Scalar(ast::Value::Int(7))),
            Err(QplError::Runtime("boom".into())),
        ] {
            let bytes = encode_result(&result);
            let decoded = decode_response(&bytes);
            match (&result, &decoded) {
                (Ok(EvalResult::Stored), Ok(EvalResult::Stored)) => {}
                (Ok(EvalResult::Lazy(a)), Ok(EvalResult::Lazy(b))) => assert_eq!(a, b),
                (Ok(EvalResult::Scalar(a)), Ok(EvalResult::Scalar(b))) => assert_eq!(a, b),
                (Err(a), Err(b)) => assert_eq!(a.to_string(), b.to_string()),
                other => panic!("mismatched round trip: {other:?}"),
            }
        }
    }

    #[test]
    fn response_round_trip_table() {
        let df = df!["a" => [1i64, 2, 3], "b" => ["x", "y", "z"]].unwrap();
        let bytes = encode_result(&Ok(EvalResult::Table(df.clone())));
        match decode_response(&bytes) {
            Ok(EvalResult::Table(got)) => assert_eq!(got, df),
            other => panic!("expected a table, got {other:?}"),
        }
    }

    #[test]
    fn to_tcp_uri_forms() {
        assert_eq!(to_tcp_uri("5001"), "tcp://127.0.0.1:5001");
        assert_eq!(to_tcp_uri("example.com:5001"), "tcp://example.com:5001");
        assert_eq!(to_tcp_uri("tcp://1.2.3.4:5001"), "tcp://1.2.3.4:5001");
    }

    // --- end-to-end: a real listener + a real hopen'd connection over TCP ---

    /// A stub server replying with `respond(command)` for every request,
    /// standing in for the REPL's polling loop.
    fn spawn_stub_server(
        port: u16,
        respond: impl Fn(&str) -> Result<EvalResult, QplError> + Send + 'static,
    ) -> ServerHandle {
        let (tx, rx) = mpsc::channel::<PortRequest>();
        let handle = start_server(port, tx).expect("bind");
        thread::spawn(move || {
            while let Ok((_mode, command, reply_tx)) = rx.recv() {
                let _ = reply_tx.send(encode_result(&respond(&command)));
            }
        });
        handle
    }

    #[test]
    fn hopen_dispatch_round_trips_a_scalar_over_a_real_socket() {
        let server = spawn_stub_server(28901, |cmd| {
            assert_eq!(cmd, "1+1");
            Ok(EvalResult::Scalar(ast::Value::Int(2)))
        });
        let conn = hopen("28901", HandleMode::Read).expect("hopen");
        match dispatch_blocking(&conn, "1+1".into(), &Interrupt::default()) {
            Ok(EvalResult::Scalar(ast::Value::Int(2))) => {}
            other => panic!("expected Scalar(2), got {other:?}"),
        }
        server.close();
    }

    #[test]
    fn hopen_dispatch_round_trips_a_table_over_a_real_socket() {
        let server = spawn_stub_server(28902, |_cmd| {
            Ok(EvalResult::Table(df!["a" => [1i64, 2]].unwrap()))
        });
        let conn = hopen("28902", HandleMode::Read).expect("hopen");
        match dispatch_blocking(&conn, "select from t".into(), &Interrupt::default()) {
            Ok(EvalResult::Table(df)) => assert_eq!(df, df!["a" => [1i64, 2]].unwrap()),
            other => panic!("expected a table, got {other:?}"),
        }
        server.close();
    }

    #[test]
    fn dispatch_surfaces_a_remote_error_locally() {
        let server = spawn_stub_server(28903, |_cmd| Err(QplError::Runtime("nope".into())));
        let conn = hopen("28903", HandleMode::Read).expect("hopen");
        let err = dispatch_blocking(&conn, "bad".into(), &Interrupt::default())
            .expect_err("expected an error");
        assert_eq!(
            err.to_string(),
            QplError::Runtime("nope".into()).to_string()
        );
        server.close();
    }

    #[test]
    fn async_dispatch_then_await_resolves_the_same_reply() {
        let server = spawn_stub_server(28904, |_cmd| Ok(EvalResult::Scalar(ast::Value::Int(99))));
        let conn = hopen("28904", HandleMode::Read).expect("hopen");
        let rx = enqueue(&conn, "slow query".into()).expect("enqueue");
        // the request is already in flight; await just waits for it
        match await_reply(&rx, &Interrupt::default()) {
            Ok(EvalResult::Scalar(ast::Value::Int(99))) => {}
            other => panic!("expected Scalar(99), got {other:?}"),
        }
        server.close();
    }

    #[test]
    fn two_connections_to_the_same_server_are_independent() {
        let server = spawn_stub_server(28905, |cmd| {
            Ok(EvalResult::Scalar(ast::Value::Str(cmd.to_string())))
        });
        let a = hopen("28905", HandleMode::Read).expect("hopen a");
        let b = hopen("28905", HandleMode::Write).expect("hopen b");
        match dispatch_blocking(&a, "from-a".into(), &Interrupt::default()) {
            Ok(EvalResult::Scalar(ast::Value::Str(s))) => assert_eq!(s, "from-a"),
            other => panic!("unexpected: {other:?}"),
        }
        match dispatch_blocking(&b, "from-b".into(), &Interrupt::default()) {
            Ok(EvalResult::Scalar(ast::Value::Str(s))) => assert_eq!(s, "from-b"),
            other => panic!("unexpected: {other:?}"),
        }
        server.close();
    }

    /// The mode tag must reach the listener's decoding (the stub-server tests
    /// never look at it).
    #[test]
    fn mode_tag_round_trips_through_the_real_listener() {
        let (tx, rx) = mpsc::channel::<PortRequest>();
        let server = start_server(28907, tx).expect("bind");
        thread::spawn(move || {
            while let Ok((mode, command, reply_tx)) = rx.recv() {
                let echoed = format!("{mode:?}:{command}");
                let _ = reply_tx.send(encode_result(&Ok(EvalResult::Scalar(ast::Value::Str(
                    echoed,
                )))));
            }
        });

        let read_conn = hopen("28907", HandleMode::Read).expect("hopen read");
        match dispatch_blocking(&read_conn, "cmd".into(), &Interrupt::default()) {
            Ok(EvalResult::Scalar(ast::Value::Str(s))) => assert_eq!(s, "Read:cmd"),
            other => panic!("unexpected: {other:?}"),
        }

        let write_conn = hopen("28907", HandleMode::Write).expect("hopen write");
        match dispatch_blocking(&write_conn, "cmd".into(), &Interrupt::default()) {
            Ok(EvalResult::Scalar(ast::Value::Str(s))) => assert_eq!(s, "Write:cmd"),
            other => panic!("unexpected: {other:?}"),
        }

        server.close();
    }

    /// A server that answers `"slow"` after `delay`, everything else at once.
    fn spawn_slow_server(port: u16, delay: std::time::Duration) -> ServerHandle {
        spawn_stub_server(port, move |cmd| {
            if cmd == "slow" {
                thread::sleep(delay);
            }
            Ok(EvalResult::Scalar(ast::Value::Str(cmd.to_string())))
        })
    }

    fn interrupt_after(ms: u64) -> (Interrupt, thread::JoinHandle<()>) {
        let interrupt = Interrupt::default();
        let i2 = interrupt.clone();
        let t = thread::spawn(move || {
            thread::sleep(std::time::Duration::from_millis(ms));
            i2.request();
        });
        (interrupt, t)
    }

    #[test]
    fn an_interrupted_dispatch_leaves_the_connection_usable() {
        let server = spawn_slow_server(28910, std::time::Duration::from_millis(400));
        let conn = hopen("28910", HandleMode::Read).expect("hopen");
        let (interrupt, t) = interrupt_after(50);
        let started = std::time::Instant::now();
        let err = dispatch_blocking(&conn, "slow".into(), &interrupt).expect_err("interrupted");
        t.join().unwrap();
        assert!(matches!(err, QplError::Interrupted), "{err:?}");
        assert!(
            started.elapsed() < std::time::Duration::from_millis(350),
            "returned before the server replied"
        );
        // the worker drains the abandoned reply; the next round trip is in step
        match dispatch_blocking(&conn, "fast".into(), &Interrupt::default()) {
            Ok(EvalResult::Scalar(ast::Value::Str(s))) => assert_eq!(s, "fast"),
            other => panic!("expected the reply to `fast`, got {other:?}"),
        }
        server.close();
    }

    #[test]
    fn an_interrupted_await_can_be_awaited_again() {
        let server = spawn_slow_server(28911, std::time::Duration::from_millis(300));
        let conn = hopen("28911", HandleMode::Read).expect("hopen");
        let rx = enqueue(&conn, "slow".into()).expect("enqueue");
        let (interrupt, t) = interrupt_after(50);
        assert!(matches!(
            await_reply(&rx, &interrupt),
            Err(QplError::Interrupted)
        ));
        t.join().unwrap();
        match await_reply(&rx, &Interrupt::default()) {
            Ok(EvalResult::Scalar(ast::Value::Str(s))) => assert_eq!(s, "slow"),
            other => panic!("expected the original reply, got {other:?}"),
        }
        server.close();
    }

    #[test]
    fn an_interrupt_requested_up_front_never_blocks() {
        let server = spawn_slow_server(28912, std::time::Duration::from_millis(300));
        let conn = hopen("28912", HandleMode::Read).expect("hopen");
        let interrupt = Interrupt::default();
        interrupt.request();
        assert!(matches!(
            dispatch_blocking(&conn, "slow".into(), &interrupt),
            Err(QplError::Interrupted)
        ));
        server.close();
    }

    #[test]
    fn an_interrupted_error_crosses_the_wire_as_a_plain_error() {
        let server = spawn_stub_server(28913, |_cmd| Err(QplError::Interrupted));
        let conn = hopen("28913", HandleMode::Read).expect("hopen");
        let err = dispatch_blocking(&conn, "x".into(), &Interrupt::default()).expect_err("error");
        assert_eq!(err.to_string(), "'interrupted");
        server.close();
    }
}
