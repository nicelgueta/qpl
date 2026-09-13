//! IPC: `hopen` / `dispatch` / `async dispatch` / `await` on the client side,
//! `\port` on the server side. `ipc` feature only.
//!
//! Transport is a single REQ/REP pair per connection (`zeromq`, pure Rust, no
//! libzmq dependency). Everything async is confined to one dedicated OS
//! thread per connection (client) or one thread for the whole listener
//! (server) — the rest of the VM stays fully synchronous, single-threaded,
//! and untouched: a connection's worker thread only ever exchanges owned
//! `String`/`Vec<u8>` values over `std::sync::mpsc` channels, never a
//! reference into `Vm`.
//!
//! Wire format: a dispatched *command* is shipped as plain UTF-8 source text
//! (see `Expr::Dispatch`, which reconstructs it from tokens at parse time) —
//! the server tokenises/parses/evaluates it exactly like a REPL line. The
//! *response* mirrors `vm::EvalResult`: a one-byte tag followed by an
//! encoding specific to that variant (tables go over as Parquet bytes, reusing
//! the same format `load`/`sink` already use — no new Polars feature needed;
//! scalars use a small hand-rolled tag+payload encoding for `ast::Value`).

use std::io::Cursor;
use std::sync::mpsc;
use std::thread;

use polars::prelude::*;

use crate::ast;
use crate::errors::QplError;
use crate::vm::EvalResult;

fn rt<E: std::fmt::Display>(e: E) -> QplError {
    QplError::Runtime(e.to_string())
}

/// The inner message, without `QplError::Display`'s per-variant decoration
/// (e.g. `Runtime`'s own leading `'`) — encoded on the wire and reconstructed
/// as a plain `Runtime` error on the other side (the original variant doesn't
/// survive the trip, matching every other cross-boundary error in this
/// codebase, which already collapses to `Runtime` via the `rt`/`map_err(rt)` idiom).
fn error_message(e: &QplError) -> String {
    match e {
        QplError::Lex(m) | QplError::Parse(m) | QplError::Compile(m) | QplError::Runtime(m) => m.clone(),
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

/// Render a `PeerIdentity` (an opaque per-socket UUID) as a short hex tag for
/// the connect/disconnect log lines — just enough to tell two concurrently
/// connected clients apart, not a meaningful identity on its own.
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

/// Per-connection permission, decided by the client at `hopen` time and
/// enforced by the server on every request dispatched from that connection —
/// never on the server's own local/interactive input (see
/// `vm::Vm::with_request_permission`). Bare `hopen` is `Read` (the default);
/// `` `w!hopen `` asks for `Write`. Carried on the wire as a single tag byte
/// prepended to the command text (`tag`/`from_tag`).
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

/// A connection opened by `hopen`. Cheap to store (just a channel handle) —
/// the actual `ReqSocket` lives on the dedicated worker thread spawned by
/// `hopen`, which processes one request at a time for the connection's
/// lifetime, matching REQ's strict lock-step request/reply protocol.
pub struct ClientConn {
    tx: mpsc::Sender<ConnRequest>,
    pub mode: HandleMode,
}

/// `hopen <addr>` — connect and spawn the connection's worker thread. Blocks
/// until the connection either succeeds or fails, so a bad address/unreachable
/// host is reported immediately rather than on the first `dispatch`. `mode`
/// (from the client's `hopen` / `` `w!hopen `` spelling) is tagged onto every
/// request this connection ever sends, for the server to enforce.
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
        Err(_) => Err(QplError::Runtime(format!("hopen '{addr}': connection thread died"))),
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
    let bytes: Vec<u8> = msg.try_into().map_err(|e: &str| QplError::Runtime(e.into()))?;
    decode_response(&bytes)
}

/// Enqueue `command` on `conn`'s worker thread and return the reply channel —
/// shared by sync and async dispatch. Sync dispatch blocks on `reply_rx.recv()`
/// immediately; async dispatch stashes `reply_rx` in `Vm.pending` and returns
/// right away, to be collected later by `await`.
pub fn enqueue(conn: &ClientConn, command: String) -> Result<ReplyRx, QplError> {
    let (reply_tx, reply_rx) = mpsc::channel();
    conn.tx
        .send((command, reply_tx))
        .map_err(|_| QplError::Runtime("connection is closed".into()))?;
    Ok(reply_rx)
}

/// Blocking `dispatch`: enqueue and wait for the reply inline.
pub fn dispatch_blocking(conn: &ClientConn, command: String) -> Result<EvalResult, QplError> {
    enqueue(conn, command)?
        .recv()
        .map_err(|_| QplError::Runtime("connection closed before replying".into()))?
}

/// `await`: block on a reply channel previously stashed by an async dispatch.
pub fn await_reply(rx: ReplyRx) -> Result<EvalResult, QplError> {
    rx.recv().map_err(|_| QplError::Runtime("connection closed before replying".into()))?
}

// ---------------------------------------------------------------------------
// server: `\port`
// ---------------------------------------------------------------------------

/// One request received on the listening socket, forwarded to the main
/// thread (which owns the one and only `Vm`) for evaluation. `mode` is that
/// connection's permission (decoded from the wire tag `dispatch_once`
/// prepends), applied only for the duration of this one request. `reply_tx`
/// is how the encoded response bytes get back to the listener thread to send.
pub type PortRequest = (HandleMode, String, mpsc::Sender<Vec<u8>>);

/// A running `\port` listener. Dropping it (or calling `close`) signals the
/// listener thread to stop accepting new requests and joins it — any request
/// already being processed is allowed to finish first (REP can't abandon an
/// in-flight reply mid-flight anyway).
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

/// `\port <n>` — bind and start the listener thread. Blocks until the bind
/// either succeeds or fails, so a busy port is reported immediately.
pub fn start_server(port: u16, main_tx: mpsc::Sender<PortRequest>) -> Result<ServerHandle, QplError> {
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
            // registered before `bind` so no `Accepted` event can be missed —
            // this is just an mpsc channel handle, nothing to race with the bind.
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
                    // TCP-level accept/close on the listening socket — printed
                    // for operator visibility only, no effect on `Vm` state
                    // (that's `HandleMode`, decided per request, not per socket).
                    // Caveat: `Accepted` fires reliably, but this version of the
                    // `zeromq` crate only emits `Disconnected` when a peer's
                    // stream ends with a protocol-level error — a clean close
                    // (the common case: the client process just exits) is
                    // dropped silently a layer down (`FairQueue::poll_next`,
                    // the `Poll::Ready(None)` arm) without notifying `monitor()`.
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
                        // hand off to the main thread and block this (otherwise
                        // idle) listener thread for the reply — fine, since REP
                        // can't accept another request until this one replies
                        // anyway.
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
        Ok(Ok(())) => Ok(ServerHandle { shutdown_tx: Some(shutdown_tx), thread: Some(thread) }),
        Ok(Err(e)) => Err(QplError::Runtime(format!("\\port {port}: {e}"))),
        Err(_) => Err(QplError::Runtime(format!("\\port {port}: listener thread died"))),
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

/// Encode a dispatched command's outcome for the wire. Called on the server,
/// right after evaluating the command — errors are encoded too (rather than
/// left to the transport), so the client always gets a reply and can surface
/// a remote error exactly like a local one.
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
            v.encode(&mut out);
            out
        }
        Ok(EvalResult::Table(df)) => {
            let mut out = vec![TAG_TABLE];
            let mut df = df.clone();
            // Parquet-in-memory: reuses the same writer `sink`/`load` already
            // link (see the `parquet` polars feature in Cargo.toml) instead of
            // adding a dedicated wire-serialisation format.
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
    let (&tag, rest) = bytes.split_first().ok_or_else(|| rt("empty dispatch response"))?;
    match tag {
        TAG_ERROR => Err(QplError::Runtime(String::from_utf8_lossy(rest).into_owned())),
        TAG_STORED => Ok(EvalResult::Stored),
        TAG_LAZY => Ok(EvalResult::Lazy(String::from_utf8_lossy(rest).into_owned())),
        TAG_SCALAR => Ok(EvalResult::Scalar(ast::Value::decode(&mut Reader::new(rest))?)),
        TAG_TABLE => {
            let df = ParquetReader::new(Cursor::new(rest.to_vec()))
                .finish()
                .map_err(rt)?;
            Ok(EvalResult::Table(df))
        }
        other => Err(QplError::Runtime(format!("unknown dispatch response tag {other}"))),
    }
}

/// The one-byte tag identifying which `ast::Value` variant follows on the
/// wire. Explicit discriminants so the encoding is stable across builds.
/// Kept as an enum (rather than loose `u8` constants) specifically so that
/// adding a new `Value` variant forces a decision here too: `Value::encode`'s
/// match is exhaustive over `ValueTag`, so the compiler catches a forgotten
/// wire-format update the moment a new tag is added but not handled.
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum ValueTag {
    Int = 0,
    Float = 1,
    Str = 2,
    Sym = 3,
    Bool = 4,
    Date = 5,
    Month = 6,
    Time = 7,
    Minute = 8,
    Second = 9,
    Timestamp = 10,
    Timespan = 11,
    IntVec = 12,
    FloatVec = 13,
    SymVec = 14,
    StrVec = 15,
    BoolVec = 16,
}

impl TryFrom<u8> for ValueTag {
    type Error = QplError;
    fn try_from(b: u8) -> Result<Self, QplError> {
        use ValueTag::*;
        Ok(match b {
            0 => Int, 1 => Float, 2 => Str, 3 => Sym, 4 => Bool,
            5 => Date, 6 => Month, 7 => Time, 8 => Minute, 9 => Second,
            10 => Timestamp, 11 => Timespan, 12 => IntVec, 13 => FloatVec,
            14 => SymVec, 15 => StrVec, 16 => BoolVec,
            other => return Err(QplError::Runtime(format!("unknown dispatch value tag {other}"))),
        })
    }
}

fn push_str(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u32).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
}

impl ast::Value {
    /// Wire-encode this value, tagged with its `ValueTag` — see `decode`.
    fn encode(&self, out: &mut Vec<u8>) {
        use ast::Value::*;
        match self {
            Int(n) => { out.push(ValueTag::Int as u8); out.extend_from_slice(&n.to_le_bytes()); }
            Float(n) => { out.push(ValueTag::Float as u8); out.extend_from_slice(&n.to_le_bytes()); }
            Str(s) => { out.push(ValueTag::Str as u8); push_str(out, s); }
            Sym(s) => { out.push(ValueTag::Sym as u8); push_str(out, s); }
            Bool(b) => { out.push(ValueTag::Bool as u8); out.push(*b as u8); }
            Date(n) => { out.push(ValueTag::Date as u8); out.extend_from_slice(&n.to_le_bytes()); }
            Month(n) => { out.push(ValueTag::Month as u8); out.extend_from_slice(&n.to_le_bytes()); }
            Time(n) => { out.push(ValueTag::Time as u8); out.extend_from_slice(&n.to_le_bytes()); }
            Minute(n) => { out.push(ValueTag::Minute as u8); out.extend_from_slice(&n.to_le_bytes()); }
            Second(n) => { out.push(ValueTag::Second as u8); out.extend_from_slice(&n.to_le_bytes()); }
            Timestamp(n) => { out.push(ValueTag::Timestamp as u8); out.extend_from_slice(&n.to_le_bytes()); }
            Timespan(n) => { out.push(ValueTag::Timespan as u8); out.extend_from_slice(&n.to_le_bytes()); }
            IntVec(v) => {
                out.push(ValueTag::IntVec as u8);
                out.extend_from_slice(&(v.len() as u32).to_le_bytes());
                for n in v { out.extend_from_slice(&n.to_le_bytes()); }
            }
            FloatVec(v) => {
                out.push(ValueTag::FloatVec as u8);
                out.extend_from_slice(&(v.len() as u32).to_le_bytes());
                for n in v { out.extend_from_slice(&n.to_le_bytes()); }
            }
            SymVec(v) => {
                out.push(ValueTag::SymVec as u8);
                out.extend_from_slice(&(v.len() as u32).to_le_bytes());
                for s in v { push_str(out, s); }
            }
            StrVec(v) => {
                out.push(ValueTag::StrVec as u8);
                out.extend_from_slice(&(v.len() as u32).to_le_bytes());
                for s in v { push_str(out, s); }
            }
            BoolVec(v) => {
                out.push(ValueTag::BoolVec as u8);
                out.extend_from_slice(&(v.len() as u32).to_le_bytes());
                for b in v { out.push(*b as u8); }
            }
            // connection/future handles never need to cross the wire themselves
            Handle(_) | Future(_) => {
                out.push(ValueTag::Str as u8);
                push_str(out, "<unrepresentable>");
            }
        }
    }

    /// Inverse of `encode` — see `Reader` below.
    fn decode(r: &mut Reader) -> Result<ast::Value, QplError> {
        use ValueTag::*;
        Ok(match ValueTag::try_from(r.u8()?)? {
            Int => ast::Value::Int(r.i64()?),
            Float => ast::Value::Float(r.f64()?),
            Str => ast::Value::Str(r.string()?),
            Sym => ast::Value::Sym(r.string()?),
            Bool => ast::Value::Bool(r.u8()? != 0),
            Date => ast::Value::Date(r.i32()?),
            Month => ast::Value::Month(r.i32()?),
            Time => ast::Value::Time(r.i64()?),
            Minute => ast::Value::Minute(r.i32()?),
            Second => ast::Value::Second(r.i32()?),
            Timestamp => ast::Value::Timestamp(r.i64()?),
            Timespan => ast::Value::Timespan(r.i64()?),
            IntVec => {
                let n = r.u32()?;
                ast::Value::IntVec((0..n).map(|_| r.i64()).collect::<Result<_, _>>()?)
            }
            FloatVec => {
                let n = r.u32()?;
                ast::Value::FloatVec((0..n).map(|_| r.f64()).collect::<Result<_, _>>()?)
            }
            SymVec => {
                let n = r.u32()?;
                ast::Value::SymVec((0..n).map(|_| r.string()).collect::<Result<_, _>>()?)
            }
            StrVec => {
                let n = r.u32()?;
                ast::Value::StrVec((0..n).map(|_| r.string()).collect::<Result<_, _>>()?)
            }
            BoolVec => {
                let n = r.u32()?;
                ast::Value::BoolVec((0..n).map(|_| r.u8().map(|b| b != 0)).collect::<Result<_, _>>()?)
            }
        })
    }
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8], QplError> {
        let end = self.pos + n;
        let slice = self.buf.get(self.pos..end).ok_or_else(|| rt("truncated dispatch payload"))?;
        self.pos = end;
        Ok(slice)
    }
    fn u8(&mut self) -> Result<u8, QplError> {
        Ok(self.take(1)?[0])
    }
    fn i32(&mut self) -> Result<i32, QplError> {
        Ok(i32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn i64(&mut self) -> Result<i64, QplError> {
        Ok(i64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn f64(&mut self) -> Result<f64, QplError> {
        Ok(f64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> Result<u32, QplError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn string(&mut self) -> Result<String, QplError> {
        let len = self.u32()? as usize;
        Ok(String::from_utf8_lossy(self.take(len)?).into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip_value(v: ast::Value) -> ast::Value {
        let mut buf = Vec::new();
        v.encode(&mut buf);
        ast::Value::decode(&mut Reader::new(&buf)).expect("decode")
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
            ast::Value::IntVec(vec![1, 2, 3]),
            ast::Value::FloatVec(vec![1.5, 2.5]),
            ast::Value::SymVec(vec!["a".into(), "b".into()]),
            ast::Value::StrVec(vec!["x".into(), "y".into()]),
            ast::Value::BoolVec(vec![true, false, true]),
        ];
        for v in cases {
            assert_eq!(roundtrip_value(v.clone()), v);
        }
    }

    #[test]
    fn value_round_trip_handles_empty_vectors_and_strings() {
        assert_eq!(roundtrip_value(ast::Value::Str("".into())), ast::Value::Str("".into()));
        assert_eq!(roundtrip_value(ast::Value::IntVec(vec![])), ast::Value::IntVec(vec![]));
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

    /// Runs a tiny "server": for every request the listener forwards, replies
    /// with `respond(command)`'s encoded result. Mimics the REPL's polling
    /// loop (`PortSession::poll` + `eval_for_dispatch`) without needing the
    /// REPL itself.
    fn spawn_stub_server(port: u16, respond: impl Fn(&str) -> Result<EvalResult, QplError> + Send + 'static) -> ServerHandle {
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
        match dispatch_blocking(&conn, "1+1".into()) {
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
        match dispatch_blocking(&conn, "select from t".into()) {
            Ok(EvalResult::Table(df)) => assert_eq!(df, df!["a" => [1i64, 2]].unwrap()),
            other => panic!("expected a table, got {other:?}"),
        }
        server.close();
    }

    #[test]
    fn dispatch_surfaces_a_remote_error_locally() {
        let server = spawn_stub_server(28903, |_cmd| Err(QplError::Runtime("nope".into())));
        let conn = hopen("28903", HandleMode::Read).expect("hopen");
        let err = dispatch_blocking(&conn, "bad".into()).expect_err("expected an error");
        assert_eq!(err.to_string(), QplError::Runtime("nope".into()).to_string());
        server.close();
    }

    #[test]
    fn async_dispatch_then_await_resolves_the_same_reply() {
        let server = spawn_stub_server(28904, |_cmd| Ok(EvalResult::Scalar(ast::Value::Int(99))));
        let conn = hopen("28904", HandleMode::Read).expect("hopen");
        let rx = enqueue(&conn, "slow query".into()).expect("enqueue");
        // the request is already in flight; await just waits for it
        match await_reply(rx) {
            Ok(EvalResult::Scalar(ast::Value::Int(99))) => {}
            other => panic!("expected Scalar(99), got {other:?}"),
        }
        server.close();
    }

    #[test]
    fn two_connections_to_the_same_server_are_independent() {
        let server = spawn_stub_server(28905, |cmd| Ok(EvalResult::Scalar(ast::Value::Str(cmd.to_string()))));
        let a = hopen("28905", HandleMode::Read).expect("hopen a");
        let b = hopen("28905", HandleMode::Write).expect("hopen b");
        match dispatch_blocking(&a, "from-a".into()) {
            Ok(EvalResult::Scalar(ast::Value::Str(s))) => assert_eq!(s, "from-a"),
            other => panic!("unexpected: {other:?}"),
        }
        match dispatch_blocking(&b, "from-b".into()) {
            Ok(EvalResult::Scalar(ast::Value::Str(s))) => assert_eq!(s, "from-b"),
            other => panic!("unexpected: {other:?}"),
        }
        server.close();
    }

    /// The wire mode tag (prepended by `dispatch_once`) must actually reach the
    /// listener thread's decoding, not just get ignored by a stub that skips
    /// straight to `respond` — this is the one piece `spawn_stub_server`-based
    /// tests above don't exercise, since their `respond` closures never look at
    /// the connection's mode.
    #[test]
    fn mode_tag_round_trips_through_the_real_listener() {
        let (tx, rx) = mpsc::channel::<PortRequest>();
        let server = start_server(28907, tx).expect("bind");
        thread::spawn(move || {
            while let Ok((mode, command, reply_tx)) = rx.recv() {
                let echoed = format!("{mode:?}:{command}");
                let _ = reply_tx.send(encode_result(&Ok(EvalResult::Scalar(ast::Value::Str(echoed)))));
            }
        });

        let read_conn = hopen("28907", HandleMode::Read).expect("hopen read");
        match dispatch_blocking(&read_conn, "cmd".into()) {
            Ok(EvalResult::Scalar(ast::Value::Str(s))) => assert_eq!(s, "Read:cmd"),
            other => panic!("unexpected: {other:?}"),
        }

        let write_conn = hopen("28907", HandleMode::Write).expect("hopen write");
        match dispatch_blocking(&write_conn, "cmd".into()) {
            Ok(EvalResult::Scalar(ast::Value::Str(s))) => assert_eq!(s, "Write:cmd"),
            other => panic!("unexpected: {other:?}"),
        }

        server.close();
    }
}
