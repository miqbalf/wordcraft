//! `live-worker`: the document service saving a live room back to Nextcloud. The worker's room
//! runs against a fake Nextcloud (the app's worker routes, served by axum in the test, checking
//! the worker's signature on every call) and an in-memory relay shared with real editing
//! sessions, on a clock the test moves.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use craft_collab::{ClientMsg, Hub, Presence, ServerMsg};
use hmac::digest::KeyInit;
use hmac::{Hmac, Mac};
use serde_json::{Value, json};
use sha1::{Digest, Sha1};
use sha2::Sha256;
use tokio::sync::watch;
use wordcraft_cli::live_worker::{
    Clock, Config, ENDED_KIND, FileGot, Flow, Grant, HubTicket, NOTICE_KIND, Nextcloud, OpenError, QUIET_MS, Room, RoomFiles, Rooms, SAVE_GIVE_UP_MS,
    Secret, Spawn, listen, router, serve_grant, service_ticket,
};
use wordcraft_doc::{Document, Paragraph, Pos, para_block};
use wordcraft_engine::io::{open_bytes, save_bytes};
use wordcraft_engine::{Selection, Session};
use wordcraft_live::{LiveEditing, LiveStatus, SERVICE_UID};

const ROOM: &str = "inst42:155:abcdef012345";
const DOC: &str = "doc:inst42:155:abcdef012345";
const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const WORKER_SECRET: &str = "worker-secret-for-tests-only-0123456789";
const RELAY_SECRET: &str = "relay-secret-for-tests-only-0123456789ab";
const FILE: &str = "Plan.docx";
/// 2026-11-21 14:03:00 UTC.
const START_UNIX_MS: i64 = 1_795_269_780_000;

// ---- the relay -------------------------------------------------------------------------------

struct Conn {
    pid: String,
    service: bool,
    connected: bool,
    /// Hold this connection's inbox (deliver nothing) while set.
    paused: bool,
    inbox: VecDeque<ServerMsg>,
}

/// A relay in memory: numbers submits and broadcasts them (sender included), keeps the ops
/// after the last save for joiners, answers save reports, and keeps every ephemeral.
#[derive(Default)]
struct Relay {
    seq: u64,
    saved: u64,
    log: Vec<ServerMsg>,
    conns: Vec<Conn>,
    next_space: u32,
    /// Every ephemeral sent: `(origin, kind, payload)`.
    ephemeral: Vec<(String, String, Value)>,
    /// Keep save acknowledgements back until released.
    hold_acks: bool,
    held: Vec<(usize, ServerMsg)>,
    /// Refuse every snapshot submit with these words.
    refuse_snapshots: Option<&'static str>,
    /// Every `Hub::set_token`: `(connection, ticket)`.
    tokens: Vec<(usize, String)>,
    /// The newest `saved` marker, kept after its sender left and sent to every joiner, as the
    /// relay does: `(origin, payload)`.
    saved_marker: Option<(String, Value)>,
}

type Shared = Rc<RefCell<Relay>>;

#[derive(Clone)]
struct TestHub {
    relay: Shared,
    at: usize,
}

impl Hub for TestHub {
    fn send(&mut self, msg: ClientMsg) -> craft_collab::Result<()> {
        self.relay.borrow_mut().handle(self.at, msg);
        Ok(())
    }
    fn poll(&mut self) -> Vec<ServerMsg> {
        let mut r = self.relay.borrow_mut();
        let c = &mut r.conns[self.at];
        if !c.connected || c.paused { Vec::new() } else { c.inbox.drain(..).collect() }
    }
    fn set_token(&mut self, token: String) {
        let at = self.at;
        self.relay.borrow_mut().tokens.push((at, token));
    }
    fn connected(&self) -> bool {
        self.relay.borrow().conns[self.at].connected
    }
}

fn op_seq(m: &ServerMsg) -> u64 {
    match m {
        ServerMsg::Op { seq, .. } => *seq,
        _ => 0,
    }
}

fn is_snap(params: &Value) -> bool {
    params.get("snap").and_then(Value::as_bool).unwrap_or(false)
}

impl Relay {
    fn connect(relay: &Shared, uid: &str) -> TestHub {
        let at = {
            let mut r = relay.borrow_mut();
            let at = r.conns.len();
            r.conns.push(Conn {
                pid: format!("{uid}~{at:08x}"),
                service: uid == SERVICE_UID,
                connected: true,
                paused: false,
                inbox: VecDeque::new(),
            });
            r.join(at);
            at
        };
        TestHub { relay: relay.clone(), at }
    }

    fn roster(&self) -> Vec<Presence> {
        self.conns.iter().filter(|c| c.connected).map(|c| Presence { peer: c.pid.clone(), display: c.pid.clone() }).collect()
    }

    fn join(&mut self, at: usize) {
        self.next_space += 1;
        let replay: Vec<ServerMsg> = self.log.iter().filter(|m| op_seq(m) > self.saved).cloned().collect();
        let until = replay.last().map_or(self.saved, op_seq);
        let joined = ServerMsg::Joined {
            doc: DOC.into(),
            peer: self.conns[at].pid.clone(),
            seq: self.saved,
            space: self.next_space,
            roster: self.roster(),
            locks: Vec::new(),
            replay_until: Some(until),
            fallback: false,
            text_cap: None,
            heartbeat: false,
        };
        self.conns[at].inbox.push_back(joined);
        self.conns[at].inbox.extend(replay);
        if let Some((origin, payload)) = self.saved_marker.clone() {
            self.conns[at].inbox.push_back(ServerMsg::Ephemeral { doc: DOC.into(), origin, kind: "saved".into(), payload });
        }
        let presence = ServerMsg::Presence { doc: DOC.into(), roster: self.roster() };
        self.broadcast(&presence, Some(at));
    }

    fn broadcast(&mut self, msg: &ServerMsg, except: Option<usize>) {
        for (j, c) in self.conns.iter_mut().enumerate() {
            if Some(j) != except && c.connected {
                c.inbox.push_back(msg.clone());
            }
        }
    }

    fn handle(&mut self, at: usize, msg: ClientMsg) {
        if !self.conns[at].connected {
            return;
        }
        match msg {
            ClientMsg::Submit { local, command, params, .. } => {
                let refusal = match self.refuse_snapshots {
                    Some(words) if is_snap(&params) => Some(words),
                    _ if is_snap(&params) && !self.conns[at].service => Some("only the document service may checkpoint; ask the saver"),
                    _ => None,
                };
                if let Some(detail) = refusal {
                    self.conns[at].inbox.push_back(ServerMsg::Error { doc: DOC.into(), detail: detail.into(), local: Some(local), code: None });
                    return;
                }
                self.seq += 1;
                let op = ServerMsg::Op {
                    doc: DOC.into(),
                    seq: self.seq,
                    origin: self.conns[at].pid.clone(),
                    local: Some(local),
                    command,
                    params,
                    scope: None,
                };
                self.log.push(op.clone());
                self.broadcast(&op, None);
            }
            ClientMsg::Saved { seq, .. } => {
                let next_is_snap = self.log.iter().any(|m| matches!(m, ServerMsg::Op { seq: s, params, .. } if *s == seq + 1 && is_snap(params)));
                let answer = if self.conns[at].service && next_is_snap {
                    self.saved = self.saved.max(seq);
                    self.log.retain(|m| op_seq(m) > seq);
                    ServerMsg::SavedAck { doc: DOC.into(), seq }
                } else {
                    ServerMsg::SavedRefused { doc: DOC.into(), seq, reason: "no checkpoint follows that sequence".into() }
                };
                if self.hold_acks {
                    self.held.push((at, answer));
                } else {
                    self.conns[at].inbox.push_back(answer);
                }
            }
            ClientMsg::Ephemeral { kind, payload, .. } => {
                let origin = self.conns[at].pid.clone();
                if kind == "saved" {
                    self.saved_marker = Some((origin.clone(), payload.clone()));
                }
                self.ephemeral.push((origin.clone(), kind.clone(), payload.clone()));
                self.broadcast(&ServerMsg::Ephemeral { doc: DOC.into(), origin, kind, payload }, Some(at));
            }
            _ => {}
        }
    }

    fn release_acks(&mut self) {
        self.hold_acks = false;
        for (at, m) in std::mem::take(&mut self.held) {
            self.conns[at].inbox.push_back(m);
        }
    }

    fn leave(&mut self, at: usize) {
        self.conns[at].connected = false;
        self.conns[at].inbox.clear();
        let presence = ServerMsg::Presence { doc: DOC.into(), roster: self.roster() };
        self.broadcast(&presence, None);
    }

    /// A connection that left comes back and joins again.
    fn rejoin(&mut self, at: usize) {
        self.conns[at].connected = true;
        self.join(at);
    }

    /// The reasons the worker ended the editors' session with, in order.
    fn ended(&self) -> Vec<String> {
        self.ephemeral
            .iter()
            .filter(|(origin, kind, _)| origin.starts_with(SERVICE_UID) && kind == ENDED_KIND)
            .filter_map(|(_, _, p)| p.get("reason").and_then(Value::as_str).map(str::to_owned))
            .collect()
    }

    /// The worker's notices, in order.
    fn notices(&self) -> Vec<String> {
        self.ephemeral
            .iter()
            .filter(|(origin, kind, _)| origin.starts_with(SERVICE_UID) && kind == NOTICE_KIND)
            .filter_map(|(_, _, p)| p.get("text").and_then(Value::as_str).map(str::to_owned))
            .collect()
    }
}

// ---- the fake Nextcloud ----------------------------------------------------------------------

#[derive(Default)]
struct Nc {
    /// The files in the room file's folder, by name.
    files: HashMap<String, Vec<u8>>,
    /// Every call that got through, in order: "get", "put", "put412", "copy", "copy409",
    /// "heartbeat", "close".
    calls: Vec<&'static str>,
    /// Calls refused for a bad signature.
    forbidden: usize,
    /// Answer every copy with this status instead.
    copy_status: Option<StatusCode>,
    /// Take this long over every write.
    put_delay: Option<std::time::Duration>,
    /// Answer every write with this status once a conflict copy was made.
    put_status_after_copy: Option<StatusCode>,
    /// Answer every write with this status.
    put_status: Option<StatusCode>,
    /// The name the file is served under, when not [`FILE`] (renamed).
    name: Option<String>,
}

type NcState = Arc<Mutex<Nc>>;

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn etag_of(b: &[u8]) -> String {
    hex(&Sha1::digest(b))
}

fn hmac_hex(secret: &str, msg: &str) -> String {
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(msg.as_bytes());
    hex(&mac.finalize().into_bytes())
}

fn signed(h: &HeaderMap, room: &str) -> bool {
    h.get("X-Craft-Worker").and_then(|v| v.to_str().ok()) == Some(hmac_hex(WORKER_SECRET, &format!("{room}:{TOKEN}")).as_str())
}

fn deny(nc: &NcState) -> Response {
    nc.lock().unwrap().forbidden += 1;
    (StatusCode::FORBIDDEN, "forbidden").into_response()
}

async fn nc_get(State(nc): State<NcState>, Path(room): Path<String>, h: HeaderMap) -> Response {
    if !signed(&h, &room) {
        return deny(&nc);
    }
    let mut n = nc.lock().unwrap();
    n.calls.push("get");
    let bytes = n.files[FILE].clone();
    let name = n.name.clone().unwrap_or_else(|| FILE.to_owned());
    ([(header::ETAG, format!("\"{}\"", etag_of(&bytes))), (header::CONTENT_DISPOSITION, format!("attachment; filename=\"{name}\""))], bytes)
        .into_response()
}

async fn nc_put(State(nc): State<NcState>, Path(room): Path<String>, h: HeaderMap, body: Bytes) -> Response {
    if !signed(&h, &room) {
        return deny(&nc);
    }
    let slow = nc.lock().unwrap().put_delay;
    if let Some(d) = slow {
        tokio::time::sleep(d).await;
    }
    let mut n = nc.lock().unwrap();
    let if_match = h.get(header::IF_MATCH).and_then(|v| v.to_str().ok()).unwrap_or("").trim_matches('"').to_owned();
    if if_match != etag_of(&n.files[FILE]) {
        n.calls.push("put412");
        return (StatusCode::PRECONDITION_FAILED, "etag mismatch").into_response();
    }
    if let Some(status) = n.put_status.or(n.put_status_after_copy.filter(|_| n.calls.contains(&"copy"))) {
        n.calls.push("putfail");
        return (status, "no").into_response();
    }
    n.calls.push("put");
    n.files.insert(FILE.into(), body.to_vec());
    json_response(json!({"etag": etag_of(&body)}))
}

async fn nc_copy(State(nc): State<NcState>, Path(room): Path<String>, h: HeaderMap, body: Bytes) -> Response {
    if !signed(&h, &room) {
        return deny(&nc);
    }
    let name = serde_json::from_slice::<Value>(&body).ok().and_then(|v| v["name"].as_str().map(str::to_owned)).unwrap_or_default();
    let mut n = nc.lock().unwrap();
    if let Some(status) = n.copy_status {
        n.calls.push("copyfail");
        return (status, "no").into_response();
    }
    if n.files.contains_key(&name) {
        n.calls.push("copy409");
        return (StatusCode::CONFLICT, "name exists").into_response();
    }
    n.calls.push("copy");
    let current = n.files[FILE].clone();
    n.files.insert(name.clone(), current);
    json_response(json!({"name": name}))
}

async fn nc_room(State(nc): State<NcState>, Path((room, what)): Path<(String, String)>, h: HeaderMap) -> Response {
    if !signed(&h, &room) {
        return deny(&nc);
    }
    nc.lock().unwrap().calls.push(if what == "close" { "close" } else { "heartbeat" });
    json_response(json!({"ok": true}))
}

fn json_response(v: Value) -> Response {
    ([(header::CONTENT_TYPE, "application/json")], v.to_string()).into_response()
}

async fn serve(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

async fn nextcloud(bytes: Vec<u8>) -> (NcState, String) {
    let nc: NcState = Arc::default();
    nc.lock().unwrap().files.insert(FILE.into(), bytes);
    let api = "/index.php/apps/craft_office/api/v1";
    let app = Router::new()
        .route(&format!("{api}/file/{{room}}"), get(nc_get).put(nc_put))
        .route(&format!("{api}/file/{{room}}/copy"), post(nc_copy))
        .route(&format!("{api}/room/{{room}}/{{what}}"), post(nc_room))
        .with_state(nc.clone());
    let base = serve(app).await;
    (nc, base)
}

// ---- the clock, the editors, the world -------------------------------------------------------

struct TestClock(AtomicU64);

impl Clock for TestClock {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
    fn unix_ms(&self) -> i64 {
        START_UNIX_MS + i64::try_from(self.now_ms()).unwrap()
    }
}

struct Ed {
    s: Session,
    le: LiveEditing<TestHub>,
    at: usize,
}

impl Ed {
    fn pump(&mut self) {
        self.le.pump(&mut self.s);
    }

    fn type_at_end(&mut self, block: usize, text: &str) {
        self.run_at_end(block, "text.insert", json!({ "text": text }));
    }

    fn run_at_end(&mut self, block: usize, id: &str, params: Value) {
        let len = self.s.doc.body[block].as_para().unwrap().text.len();
        self.s.sel = Selection { anchor: Pos::body(block, len), focus: Pos::body(block, len) };
        assert!(self.le.run(&mut self.s, id, &params).is_ok(), "{id}: {:?}", self.le.status());
    }

    fn saved_at(&self) -> Option<String> {
        match self.le.status() {
            LiveStatus::Live { saved_at } => saved_at.clone(),
            other => panic!("not live: {other:?}"),
        }
    }
}

fn file(paras: &[&str]) -> Vec<u8> {
    let mut doc = Document::new();
    doc.body = paras.iter().map(|t| para_block(Paragraph::with_text(t, Default::default()))).collect();
    save_bytes(FILE, &doc).unwrap()
}

fn texts(doc: &Document) -> Vec<String> {
    doc.body.iter().filter_map(|b| b.as_para()).map(|p| p.text.clone()).collect()
}

struct World {
    relay: Shared,
    nc: NcState,
    clock: Arc<TestClock>,
    worker: Room<TestHub>,
    eds: Vec<Ed>,
    /// The fake Nextcloud's base URL.
    base: String,
}

fn worker_secret() -> Arc<Secret> {
    Arc::new(Secret::new("CRAFT_WORKER_SECRET", Some(WORKER_SECRET)).unwrap())
}

async fn world(bytes: Vec<u8>, editors: usize) -> World {
    let (nc, base) = nextcloud(bytes.clone()).await;
    let relay: Shared = Rc::default();
    let clock = Arc::new(TestClock(AtomicU64::new(0)));
    let files = RoomFiles::new(Arc::new(Nextcloud::new(&base, worker_secret()).unwrap()), ROOM.into(), watch::channel(TOKEN.to_owned()).1);
    let worker = Room::open(Relay::connect(&relay, SERVICE_UID), files, clock.clone()).await.unwrap();
    let eds = (0..editors)
        .map(|k| {
            let hub = Relay::connect(&relay, &format!("editor{k}"));
            let at = hub.at;
            let mut s = Session::new(Document::new());
            let le = LiveEditing::start(hub, DOC, &bytes, FILE, &mut s).unwrap();
            Ed { s, le, at }
        })
        .collect();
    let mut w = World { relay, nc, clock, worker, eds, base };
    for _ in 0..3 {
        w.tick(0).await;
    }
    for e in &w.eds {
        assert!(e.le.editable(), "editor live: {:?}", e.le.status());
    }
    w
}

impl World {
    /// Move the clock by `ms`, then one round: the editors, the worker, the editors again.
    async fn tick(&mut self, ms: u64) -> Flow {
        self.clock.0.fetch_add(ms, Ordering::Relaxed);
        for e in &mut self.eds {
            e.pump();
        }
        let flow = self.worker.step().await;
        for e in &mut self.eds {
            e.pump();
        }
        flow
    }

    /// Tick every `step` ms for `ms` ms, or until the worker is done.
    async fn run_for(&mut self, ms: u64, step: u64) -> Flow {
        let mut gone = 0;
        while gone < ms {
            gone += step;
            if self.tick(step).await == Flow::Done {
                return Flow::Done;
            }
        }
        Flow::Continue
    }

    fn count(&self, call: &str) -> usize {
        self.nc.lock().unwrap().calls.iter().filter(|c| **c == call).count()
    }

    fn file(&self, name: &str) -> Vec<u8> {
        self.nc.lock().unwrap().files[name].clone()
    }

    fn file_texts(&self) -> Vec<String> {
        texts(&open_bytes(FILE, &self.file(FILE)).unwrap())
    }
}

// ---- the tests -------------------------------------------------------------------------------

#[tokio::test]
async fn debounce_saves_once_after_quiet() {
    let mut w = world(file(&["One", "Two"]), 1).await;
    w.eds[0].type_at_end(0, " fish");
    w.tick(1_000).await;
    w.eds[0].type_at_end(0, "!");
    // Typing keeps the save back until the room has been quiet.
    w.run_for(QUIET_MS - 1_000, 1_000).await;
    assert_eq!(w.count("put"), 0, "no save while the last edit is recent");
    w.run_for(5_000, 1_000).await;
    assert_eq!(w.count("put"), 1, "one save once quiet");
    w.run_for(30_000, 1_000).await;
    assert_eq!(w.count("put"), 1, "nothing new, nothing saved");
    assert_eq!(w.file_texts(), ["One fish!", "Two"]);
    assert_eq!(w.eds[0].saved_at().as_deref(), Some(etag_of(&w.file(FILE)).as_str()), "the editor was told which file is saved");
    assert_eq!(w.nc.lock().unwrap().forbidden, 0, "every call was signed for the room's grant");
}

#[tokio::test]
async fn saves_at_least_every_60s_while_typing() {
    let mut w = world(file(&["Notes"]), 1).await;
    let mut saves_at = Vec::new();
    let mut t = 0;
    while t < 150_000 {
        if t % 2_000 == 0 {
            w.eds[0].type_at_end(0, "x");
        }
        let before = w.count("put");
        w.tick(1_000).await;
        t += 1_000;
        if w.count("put") > before {
            saves_at.push(t);
        }
    }
    assert!(saves_at.len() >= 2, "saves while typing never pauses: {saves_at:?}");
    assert!(saves_at[0] <= 65_000, "the first save within about a minute: {saves_at:?}");
    for pair in saves_at.windows(2) {
        assert!(pair[1] - pair[0] <= 65_000, "saves at most about a minute apart: {saves_at:?}");
    }
    // Whatever was typed up to the last save is in the file.
    assert!(w.file_texts()[0].starts_with("Notesxx"), "{:?}", w.file_texts());
}

#[tokio::test]
async fn external_change_makes_conflict_copy() {
    let mut w = world(file(&["One", "Two"]), 1).await;
    w.eds[0].type_at_end(0, " fish");
    w.run_for(10_000, 1_000).await;
    assert_eq!(w.count("put"), 1);

    // Someone writes the file outside the session, and the first conflict name is taken.
    let outside = file(&["Changed outside"]);
    let taken = "Plan (conflict 2026-11-21 1403).docx";
    {
        let mut n = w.nc.lock().unwrap();
        n.files.insert(FILE.into(), outside.clone());
        n.files.insert(taken.into(), b"someone else's".to_vec());
    }
    w.eds[0].type_at_end(1, " more");
    w.run_for(10_000, 1_000).await;

    let copy = "Plan (conflict 2026-11-21 1403) 2.docx";
    assert_eq!(w.count("put412"), 1, "the save found the file changed");
    assert_eq!(w.file(copy), outside, "the version found there is kept beside the file");
    assert_eq!(w.file(taken), b"someone else's", "a taken name is never overwritten");
    assert_eq!(w.relay.borrow().notices(), [format!("Someone changed this file outside the live session. Their version was kept as {copy}.")]);
    assert_eq!(w.file_texts(), ["One fish", "Two more"], "the session went on saving over that version");
    assert_eq!(w.eds[0].saved_at().as_deref(), Some(etag_of(&w.file(FILE)).as_str()));

    // And keeps saving normally from there.
    w.eds[0].type_at_end(1, "!");
    w.run_for(10_000, 1_000).await;
    assert_eq!(w.count("put412"), 1, "no second conflict");
    assert_eq!(w.file_texts(), ["One fish", "Two more!"]);
}

#[tokio::test]
async fn last_peer_leaving_still_saves() {
    let mut w = world(file(&["One", "Two"]), 1).await;
    w.eds[0].type_at_end(1, " and three");
    w.tick(500).await;
    let at = w.eds[0].at;
    w.relay.borrow_mut().leave(at);
    w.eds.clear();
    // The last save is still waiting for the relay when the room has been empty long enough
    // to close: the room stays open for it.
    w.relay.borrow_mut().hold_acks = true;
    assert_eq!(w.run_for(20_000, 1_000).await, Flow::Continue, "not closed while the last save is open");
    assert_eq!(w.count("close"), 0);
    w.relay.borrow_mut().release_acks();
    let flow = w.run_for(60_000, 1_000).await;
    assert_eq!(flow, Flow::Done, "the worker leaves an empty room");
    assert_eq!(w.file_texts(), ["One", "Two and three"], "the last edit was saved");
    let calls = w.nc.lock().unwrap().calls.clone();
    let put = calls.iter().position(|c| *c == "put").unwrap();
    assert_eq!(calls.last(), Some(&"close"), "the room is closed last: {calls:?}");
    assert!(put < calls.len() - 1, "saved before closing: {calls:?}");
}

/// The dev stack's seeds (craft-office-nextcloud `dev/seed/Plan.md` and `Spec.md`, our own
/// text, copied here verbatim), which it converts to `.docx` and opens live.
const SEEDS: [(&str, &str); 2] = [
    ("Plan", "# Project Plan\n\nThis is the shared plan.\n\n- Item one\n- Item two\n"),
    ("Spec", "# Spec\n\nA spec document with **bold** and *italic* text.\n"),
];

#[tokio::test]
async fn saved_file_reopens_equal() {
    for (seed, md) in SEEDS {
        let bytes = save_bytes(FILE, &open_bytes("seed.md", md.as_bytes()).unwrap()).unwrap();
        let mut w = world(bytes, 2).await;
        w.eds[0].type_at_end(1, " Edited by one.");
        w.eds[1].run_at_end(0, "text.newParagraph", json!({}));
        w.eds[1].type_at_end(1, "Added by two.");
        w.eds[1].type_at_end(0, " (draft)");
        w.run_for(10_000, 1_000).await;
        assert_eq!(w.count("put"), 1, "{seed}");
        let saved = open_bytes(FILE, &w.file(FILE)).unwrap();
        for e in &w.eds {
            assert!(saved.same_content(&e.s.doc), "{seed}: saved {:?} vs live {:?}", texts(&saved), texts(&e.s.doc));
        }
    }
}

#[tokio::test]
async fn catch_up_edits_are_saved() {
    // An editor typed while no worker was in the room (a worker restart). The next worker gets
    // those edits only in its join's catch-up, handed over across several turns, and the
    // catch-up ends with a resent copy of the last edit (the editor's link dropped before the
    // relay confirmed it), which changes nothing. The editor types nothing more.
    let bytes = file(&["One", "Two"]);
    let (nc, base) = nextcloud(bytes.clone()).await;
    let relay: Shared = Rc::default();
    let ed_hub = Relay::connect(&relay, "editor0");
    let at = ed_hub.at;
    let mut s = Session::new(Document::new());
    let le = LiveEditing::start(ed_hub, DOC, &bytes, FILE, &mut s).unwrap();
    let mut ed = Ed { s, le, at };
    ed.pump();
    ed.type_at_end(0, " fish");
    ed.type_at_end(1, " more");
    ed.pump();
    {
        let mut r = relay.borrow_mut();
        r.seq += 1;
        let seq = r.seq;
        let Some(ServerMsg::Op { doc, origin, local, command, params, scope, .. }) = r.log.last().cloned() else { panic!("no edit logged") };
        r.log.push(ServerMsg::Op { doc, seq, origin, local, command, params, scope });
    }

    let clock = Arc::new(TestClock(AtomicU64::new(0)));
    let files = RoomFiles::new(Arc::new(Nextcloud::new(&base, worker_secret()).unwrap()), ROOM.into(), watch::channel(TOKEN.to_owned()).1);
    let hub = Relay::connect(&relay, SERVICE_UID);
    let me = hub.at;
    // The join's answer and replay, handed over one message a turn.
    let mut catch_up = std::mem::take(&mut relay.borrow_mut().conns[me].inbox);
    assert!(catch_up.len() > 2, "a replay to catch up on: {catch_up:?}");
    let worker = Room::open(hub, files, clock.clone()).await.unwrap();
    let mut w = World { relay, nc, clock, worker, eds: vec![ed], base };
    while let Some(m) = catch_up.pop_front() {
        w.relay.borrow_mut().conns[me].inbox.push_back(m);
        w.tick(100).await;
    }
    w.run_for(5_000, 1_000).await;
    // The editor leaves; the room empties.
    w.relay.borrow_mut().leave(at);
    w.eds.clear();
    assert_eq!(w.run_for(60_000, 1_000).await, Flow::Done);
    assert_eq!(w.file_texts(), ["One fish", "Two more"], "the caught-up edits were saved before the close");
}

/// A save over an outside change, with `prepare` making keeping that version fail. Returns the
/// world after five minutes, and whether saving had stopped after the first minute.
async fn conflicted(prepare: impl FnOnce(&mut Nc)) -> (World, bool) {
    let mut w = world(file(&["One"]), 1).await;
    {
        let mut n = w.nc.lock().unwrap();
        n.files.insert(FILE.into(), file(&["Changed outside"]));
        prepare(&mut n);
    }
    w.eds[0].type_at_end(0, " fish");
    w.run_for(60_000, 1_000).await;
    let early = w.worker.saving_stopped().is_some();
    w.run_for(240_000, 1_000).await;
    (w, early)
}

#[tokio::test]
async fn failed_conflict_copy_stops_saving() {
    let outside = file(&["Changed outside"]);
    // (case, prepare, the reason given, whether it may pass: retried over minutes first)
    let cases: [(&str, Box<dyn FnOnce(&mut Nc)>, &str, bool); 6] = [
        (
            "every name taken",
            Box::new(|n: &mut Nc| {
                for k in 1..=9 {
                    let nth = if k > 1 { format!(" {k}") } else { String::new() };
                    n.files.insert(format!("Plan (conflict 2026-11-21 1403){nth}.docx"), b"taken".to_vec());
                }
            }),
            "could not keep the outside version: all 9 conflict names are taken",
            false,
        ),
        (
            "the copy keeps failing",
            Box::new(|n: &mut Nc| n.copy_status = Some(StatusCode::SERVICE_UNAVAILABLE)),
            "could not keep the outside version: the server answered 503",
            true,
        ),
        ("no right to create the copy", Box::new(|n: &mut Nc| n.copy_status = Some(StatusCode::FORBIDDEN)), "may not create files", false),
        (
            "the write after the copy keeps failing",
            Box::new(|n: &mut Nc| n.put_status_after_copy = Some(StatusCode::INSUFFICIENT_STORAGE)),
            "could not keep the outside version: the server answered 507",
            true,
        ),
        (
            "the write after the copy is refused",
            Box::new(|n: &mut Nc| n.put_status_after_copy = Some(StatusCode::FORBIDDEN)),
            "no longer lets this session write",
            false,
        ),
        (
            "the file keeps changing",
            Box::new(|n: &mut Nc| n.put_status_after_copy = Some(StatusCode::PRECONDITION_FAILED)),
            "could not keep the outside version",
            true,
        ),
    ];
    for (case, prepare, why, may_pass) in cases {
        let (w, early) = conflicted(prepare).await;
        let stopped = w.worker.saving_stopped().map(str::to_owned).unwrap_or_default();
        assert!(stopped.contains(why), "{case}: {stopped:?}");
        assert_eq!(early, !may_pass, "{case}: a failure that may pass is retried for minutes; one that cannot stops at once");
        assert_eq!(w.file(FILE), outside, "{case}: the outside version was not written over");
        assert_eq!(w.count("put"), 0, "{case}");
        let notices = w.relay.borrow().notices();
        assert_eq!(notices.len(), 1, "{case}: one notice, the stop, no conflict notice: {notices:?}");
        assert!(notices[0].starts_with("Saving has stopped"), "{case}: {notices:?}");
        // Every outside version copied aside is named in the stop notice, since no conflict
        // notice told of it.
        let copies: Vec<String> =
            w.nc.lock().unwrap().files.iter().filter(|(k, v)| *k != FILE && v.as_slice() != b"taken").map(|(k, _)| k.clone()).collect();
        for copy in &copies {
            assert!(notices[0].contains(copy.as_str()), "{case}: {copy} named in {notices:?}");
        }
    }
}

#[tokio::test]
async fn restart_rejoins_and_saves_on_renotify() {
    // The worker ends (a restart) after the editors typed and before it saved. The app's stale
    // job re-notifies the same grant; the new worker joins the same room, and the relay's replay
    // since the last save brings it the edits, which it saves.
    let mut w = world(file(&["One", "Two"]), 1).await;
    w.eds[0].type_at_end(0, " fish");
    w.tick(1_000).await;
    let gone = w.relay.borrow().conns.iter().position(|c| c.service && c.connected).unwrap();
    w.relay.borrow_mut().leave(gone);
    assert_eq!(w.count("put"), 0, "nothing saved before the restart");

    let files = RoomFiles::new(Arc::new(Nextcloud::new(&w.base, worker_secret()).unwrap()), ROOM.into(), watch::channel(TOKEN.to_owned()).1);
    w.worker = Room::open(Relay::connect(&w.relay, SERVICE_UID), files, w.clock.clone()).await.unwrap();
    w.run_for(10_000, 1_000).await;
    assert_eq!(w.file_texts(), ["One fish", "Two"], "the edits made before the restart are saved");
}

#[tokio::test]
async fn an_old_notify_never_reaches_the_relay() {
    let (nc, base) = nextcloud(file(&["One"])).await;
    let old = "e".repeat(64);
    let files = RoomFiles::new(Arc::new(Nextcloud::new(&base, worker_secret()).unwrap()), ROOM.into(), watch::channel(old).1);
    let relay_secret = Secret::new("CRAFT_RELAY_SECRET", Some(RELAY_SECRET)).unwrap();
    let connects = Rc::new(RefCell::new(0));
    let tried = connects.clone();
    let connect = move |_ticket: String| {
        *tried.borrow_mut() += 1;
        async { Err::<TestHub, String>("the test never connects".into()) }
    };
    let clock: Arc<dyn Clock> = Arc::new(TestClock(AtomicU64::new(0)));
    serve_grant(&relay_secret, files, clock, connect).await;
    assert_eq!(*connects.borrow(), 0, "a grant that is not live never joins the relay");
    assert!(nc.lock().unwrap().forbidden >= 1, "the app refused the old grant");
}

#[tokio::test]
async fn no_file_write_before_ack() {
    // The relay holds its acknowledgement: nothing is written until it comes.
    let mut w = world(file(&["One"]), 1).await;
    w.relay.borrow_mut().hold_acks = true;
    w.eds[0].type_at_end(0, " held");
    w.run_for(15_000, 1_000).await;
    assert!(!w.relay.borrow().held.is_empty(), "a save report is waiting for its answer");
    assert_eq!(w.count("put"), 0, "no write before the checkpoint is acknowledged");
    w.relay.borrow_mut().release_acks();
    w.run_for(3_000, 1_000).await;
    assert_eq!(w.count("put"), 1);

    // A raced checkpoint (an edit ordered before it that it does not hold) is never written.
    let mut w = world(file(&["One"]), 1).await;
    w.eds[0].type_at_end(0, " a");
    w.tick(1_000).await;
    let worker_at = 0;
    w.relay.borrow_mut().conns[worker_at].paused = true;
    w.eds[0].type_at_end(0, " b");
    w.run_for(QUIET_MS, 1_000).await;
    w.relay.borrow_mut().conns[worker_at].paused = false;
    w.tick(1_000).await;
    assert_eq!(w.count("put"), 0, "the raced checkpoint was not written");
    w.run_for(15_000, 1_000).await;
    assert_eq!(w.count("put"), 1, "the retry saved");
    assert_eq!(w.file_texts(), ["One a b"], "with the edit the raced one missed");

    // A refused checkpoint is never written, and saving stops with a notice.
    let mut w = world(file(&["One"]), 1).await;
    w.relay.borrow_mut().refuse_snapshots = Some("that checkpoint is 99 bytes, too large to retain for joining editors");
    w.eds[0].type_at_end(0, " big");
    assert_eq!(w.run_for(30_000, 1_000).await, Flow::Continue, "the room stays open");
    assert_eq!(w.count("put"), 0);
    assert!(w.worker.saving_stopped().is_some());
    let notices = w.relay.borrow().notices();
    assert_eq!(notices.len(), 1, "the editors were told once: {notices:?}");
    assert!(notices[0].starts_with("Saving has stopped"), "{notices:?}");
}

/// Sign a notify as the app does.
fn notify_sig(room: &str, token: &str) -> String {
    hmac_hex(WORKER_SECRET, &format!("notify:{room}:{token}"))
}

#[tokio::test]
async fn notify_with_bad_hmac_is_rejected() {
    let started: Arc<Mutex<Vec<Grant>>> = Arc::default();
    let rooms = Arc::new(Rooms::default());
    let spawn: Spawn = {
        let started = started.clone();
        Arc::new(move |g| started.lock().unwrap().push(g))
    };
    let base = serve(router(worker_secret(), rooms.clone(), spawn)).await;
    let http = reqwest::Client::new();
    let post = |sig: Option<String>, token: &str| {
        let mut req = http.post(format!("{base}/rooms")).body(json!({"room": ROOM, "fileId": 155, "token": token}).to_string());
        if let Some(sig) = sig {
            req = req.header("X-Craft-Worker", sig);
        }
        req.send()
    };
    let other = "f".repeat(64);
    for (sig, why) in [
        (None, "no signature"),
        (Some("00".repeat(32)), "a wrong signature"),
        (Some(notify_sig(ROOM, &other)), "another grant's signature"),
        (Some(hmac_hex(WORKER_SECRET, &format!("{ROOM}:{TOKEN}"))), "the file-route signature"),
    ] {
        assert_eq!(post(sig, TOKEN).await.unwrap().status(), 403, "{why}");
    }
    assert!(started.lock().unwrap().is_empty() && rooms.is_empty(), "nothing started");

    assert!(post(Some(notify_sig(ROOM, TOKEN)), TOKEN).await.unwrap().status().is_success());
    assert_eq!(started.lock().unwrap().len(), 1);
    // The app notifies again (a refreshed ticket, a race): the room keeps one task, with the
    // newest token.
    assert!(post(Some(notify_sig(ROOM, &other)), &other).await.unwrap().status().is_success());
    let started = started.lock().unwrap();
    assert_eq!(started.len(), 1, "one task per room");
    assert_eq!(*started[0].token.borrow(), other);
}

#[test]
fn service_ticket_is_signed_for_the_relay() {
    let secret = Secret::new("CRAFT_RELAY_SECRET", Some(RELAY_SECRET)).unwrap();
    let now = 1_800_000_000;
    let ticket = service_ticket(&secret, ROOM, now).unwrap();
    let (payload, sig) = ticket.split_once('.').unwrap();
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(RELAY_SECRET.as_bytes()).unwrap();
    mac.update(payload.as_bytes());
    mac.verify_slice(&URL_SAFE_NO_PAD.decode(sig).unwrap()).expect("signed with the relay secret");
    let claims: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).unwrap()).unwrap();
    assert_eq!(claims["rm"], ROOM);
    assert_eq!(claims["uid"], SERVICE_UID);
    assert_eq!(claims["perm"].as_u64().map(|p| p & 3), Some(3), "may read and edit");
    let exp = claims["exp"].as_i64().unwrap();
    assert!(exp > now + 480 && exp <= now + 600, "outlives the refresh, not long: {exp}");
    assert!(claims["pid"].as_str().unwrap().starts_with(&format!("{SERVICE_UID}~")));
}

#[test]
fn config_refuses_weak_secrets_and_plain_http() {
    let ok = Some(RELAY_SECRET);
    assert!(Config::new("0.0.0.0:9555", "https://h/craft", "https://h", false, ok, ok).is_ok());
    assert!(Config::new("0.0.0.0:9555", "https://h/craft", "https://h", false, Some("  short  "), ok).is_err(), "short");
    assert!(Config::new("0.0.0.0:9555", "https://h/craft", "https://h", false, ok, None).is_err(), "missing");
    assert!(Config::new("0.0.0.0:9555", "https://h/craft", "http://h", false, ok, ok).is_err(), "plain http");
    assert!(Config::new("0.0.0.0:9555", "http://relay:9444/craft", "http://nextcloud", true, ok, ok).is_ok(), "dev");
    let padded = format!("  {RELAY_SECRET}\n");
    assert!(Config::new("0.0.0.0:9555", "https://h/craft", "https://h", false, Some(&padded), ok).is_ok(), "trimmed");
}

#[tokio::test]
async fn notify_past_the_room_cap_is_refused() {
    let rooms = Arc::new(Rooms::with_cap(1));
    let spawn: Spawn = Arc::new(|_| {});
    let base = serve(router(worker_secret(), rooms.clone(), spawn)).await;
    let http = reqwest::Client::new();
    let post = |room: &'static str| {
        http.post(format!("{base}/rooms"))
            .header("X-Craft-Worker", notify_sig(room, TOKEN))
            .body(json!({"room": room, "fileId": 1, "token": TOKEN}).to_string())
            .send()
    };
    assert!(post("inst42:1:aaaaaaaaaaaa").await.unwrap().status().is_success());
    assert_eq!(post("inst42:2:bbbbbbbbbbbb").await.unwrap().status(), 503, "a second room past the cap");
    assert!(post("inst42:1:aaaaaaaaaaaa").await.unwrap().status().is_success(), "the served room still takes its notify");
    assert_eq!(rooms.len(), 1);
}

#[tokio::test]
async fn slow_header_client_is_dropped() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let spawn: Spawn = Arc::new(|_| {});
    tokio::spawn(listen(listener, router(worker_secret(), Arc::new(Rooms::default()), spawn)));
    let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
    // A request line and one header, then nothing: the headers never end.
    client.write_all(b"POST /rooms HTTP/1.1\r\nHost: worker\r\n").await.unwrap();
    let began = std::time::Instant::now();
    let mut answer = Vec::new();
    let closed = tokio::time::timeout(std::time::Duration::from_secs(10), client.read_to_end(&mut answer)).await;
    assert!(closed.is_ok(), "the server drops a client whose headers never end");
    assert!(began.elapsed() >= std::time::Duration::from_secs(2), "but gives it time to send them");
}

#[tokio::test]
async fn hub_gets_every_reminted_ticket() {
    let relay: Shared = Rc::default();
    let mut hub = Relay::connect(&relay, SERVICE_UID);
    let clock = TestClock(AtomicU64::new(0));
    let secret = Secret::new("CRAFT_RELAY_SECRET", Some(RELAY_SECRET)).unwrap();
    let mut ticket = HubTicket::new(&secret, ROOM, &clock).unwrap();
    let first = ticket.current().to_owned();
    let live = LiveStatus::Live { saved_at: None };
    let given = |relay: &Shared| relay.borrow().tokens.iter().map(|(_, t)| t.clone()).collect::<Vec<_>>();

    ticket.keep(&mut hub, &live, &secret, ROOM, &clock);
    assert!(given(&relay).is_empty(), "a young ticket stays");
    // Each re-mint goes to the hub, for its next reconnect: no new connection.
    clock.0.fetch_add(480_000, Ordering::Relaxed);
    ticket.keep(&mut hub, &live, &secret, ROOM, &clock);
    assert_eq!(given(&relay), [ticket.current().to_owned()]);
    assert_ne!(ticket.current(), first, "a new ticket");
    clock.0.fetch_add(480_000, Ordering::Relaxed);
    ticket.keep(&mut hub, &live, &secret, ROOM, &clock);
    assert_eq!(given(&relay).len(), 2);

    // The relay refused the connection: a fresh ticket at once, then no more than one a minute.
    let refused = LiveStatus::Refused("the ticket expired".into());
    clock.0.fetch_add(1_000, Ordering::Relaxed);
    ticket.keep(&mut hub, &refused, &secret, ROOM, &clock);
    assert_eq!(given(&relay).len(), 3);
    assert_eq!(given(&relay)[2], ticket.current());
    for _ in 0..10 {
        clock.0.fetch_add(1_000, Ordering::Relaxed);
        ticket.keep(&mut hub, &refused, &secret, ROOM, &clock);
    }
    assert_eq!(given(&relay).len(), 3, "not a ticket a step");
    clock.0.fetch_add(60_000, Ordering::Relaxed);
    ticket.keep(&mut hub, &refused, &secret, ROOM, &clock);
    assert_eq!(given(&relay).len(), 4);
    assert_eq!(relay.borrow().conns.len(), 1, "the hub is kept, never replaced");
}

#[tokio::test]
async fn heartbeats_while_a_step_is_blocked() {
    // A step that waits far past the app's stale limit (a write that hangs) still heartbeats.
    // Scaled down: heartbeats every 100 ms, a write that takes 2 s.
    let mut w = world(file(&["One"]), 1).await;
    w.worker.heartbeat_every(std::time::Duration::from_millis(100)).unwrap();
    w.nc.lock().unwrap().put_delay = Some(std::time::Duration::from_secs(2));
    w.eds[0].type_at_end(0, " fish");
    w.run_for(QUIET_MS - 1_000, 1_000).await;
    let before = w.count("heartbeat");
    let began = std::time::Instant::now();
    while w.count("put") == 0 {
        w.tick(1_000).await;
    }
    assert!(began.elapsed() >= std::time::Duration::from_secs(2), "the step was blocked on the write");
    let during = w.count("heartbeat") - before;
    assert!(during >= 10, "heartbeats went on while the step was blocked ({during})");
}

#[tokio::test]
async fn a_no_op_op_still_advances_saved() {
    let saved = |w: &World| -> Vec<(u64, String)> {
        w.relay
            .borrow()
            .ephemeral
            .iter()
            .filter(|(_, kind, _)| kind == "saved")
            .map(|(_, _, p)| (p["seq"].as_u64().unwrap(), p["etag"].as_str().unwrap().to_owned()))
            .collect()
    };
    let mut w = world(file(&["One"]), 1).await;
    w.eds[0].type_at_end(0, " fish");
    w.tick(0).await;
    let edit =
        w.relay.borrow().log.iter().rev().find(|m| matches!(m, ServerMsg::Op { origin, .. } if origin.starts_with("editor"))).cloned().unwrap();
    w.run_for(10_000, 1_000).await;
    assert_eq!(w.count("put"), 1);
    let first = saved(&w);
    assert_eq!(first.len(), 1, "one save, one marker: {first:?}");
    w.run_for(20_000, 1_000).await;
    assert_eq!(saved(&w), first, "nothing new: no second marker");

    // The editor's edit comes again after a reconnect: it takes a sequence and changes nothing.
    let resent = {
        let mut r = w.relay.borrow_mut();
        r.seq += 1;
        let seq = r.seq;
        let ServerMsg::Op { doc, origin, local, command, params, scope, .. } = edit else { unreachable!() };
        let op = ServerMsg::Op { doc, seq, origin, local, command, params, scope };
        r.log.push(op.clone());
        r.broadcast(&op, None);
        seq
    };
    w.tick(1_000).await;
    assert_eq!(saved(&w).len(), 1, "not before quiet");
    w.run_for(10_000, 1_000).await;
    let after = saved(&w);
    assert_eq!(after.len(), 2, "{after:?}");
    assert_eq!(after[1], (resent, first[0].1.clone()), "saved up to the resend, the same file");
    assert!(after[1].0 > first[0].0);
    assert_eq!(w.count("put"), 1, "no write");
    assert_eq!(w.eds[0].saved_at().as_deref(), Some(first[0].1.as_str()));
}

#[tokio::test]
async fn saved_never_claims_an_op_held_for_a_gap() {
    let saved = |w: &World| -> Vec<u64> {
        w.relay.borrow().ephemeral.iter().filter(|(_, kind, _)| kind == "saved").map(|(_, _, p)| p["seq"].as_u64().unwrap()).collect()
    };
    let mut w = world(file(&["One"]), 1).await;
    w.eds[0].type_at_end(0, " fish");
    w.tick(0).await;
    let edit =
        w.relay.borrow().log.iter().rev().find(|m| matches!(m, ServerMsg::Op { origin, .. } if origin.starts_with("editor"))).cloned().unwrap();
    w.run_for(10_000, 1_000).await;
    let first = saved(&w);
    assert_eq!(first.len(), 1);

    // Two resends of the edit (they change nothing), which reach the worker out of order:
    // the later one alone first.
    let me = w.relay.borrow().conns.iter().position(|c| c.service && c.connected).unwrap();
    let [a, b] = {
        let mut r = w.relay.borrow_mut();
        let ops = [1, 2].map(|_| {
            r.seq += 1;
            let ServerMsg::Op { doc, origin, local, command, params, scope, .. } = edit.clone() else { unreachable!() };
            ServerMsg::Op { doc, seq: r.seq, origin, local, command, params, scope }
        });
        for op in &ops {
            r.log.push(op.clone());
            r.broadcast(op, Some(me));
        }
        ops
    };
    w.relay.borrow_mut().conns[me].inbox.push_back(b.clone());
    w.run_for(10_000, 1_000).await;
    assert_eq!(saved(&w), first, "no marker may claim the op held for the gap");
    w.relay.borrow_mut().conns[me].inbox.push_back(a);
    w.run_for(10_000, 1_000).await;
    let after = saved(&w);
    assert_eq!(after.len(), 2, "{after:?}");
    assert_eq!(after[1], op_seq(&b), "saved up to both, once the gap filled");
    assert_eq!(w.count("put"), 1, "no write");
}

/// A new editor joining the room with its own copy of the file, `bytes`.
fn editor(relay: &Shared, uid: &str, bytes: &[u8]) -> Ed {
    let hub = Relay::connect(relay, uid);
    let at = hub.at;
    let mut s = Session::new(Document::new());
    let le = LiveEditing::start(hub, DOC, bytes, FILE, &mut s).unwrap();
    Ed { s, le, at }
}

fn room_files(base: &str) -> RoomFiles {
    RoomFiles::new(Arc::new(Nextcloud::new(base, worker_secret()).unwrap()), ROOM.into(), watch::channel(TOKEN.to_owned()).1)
}

#[tokio::test]
async fn saving_stopping_ends_the_editors_session_and_the_heartbeat() {
    let mut w = world(file(&["One"]), 1).await;
    w.worker.heartbeat_every(std::time::Duration::from_millis(20)).unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(w.count("heartbeat") > 0, "heartbeating while saving");
    w.relay.borrow_mut().refuse_snapshots = Some("that checkpoint is too large to retain for joining editors");
    w.eds[0].type_at_end(0, " big");
    w.run_for(10_000, 1_000).await;
    assert!(w.worker.saving_stopped().is_some());
    // The editor is told, and its link ends: nothing would save what it types now.
    assert!(w.eds[0].le.take_notices().iter().any(|n| n.starts_with("Saving has stopped")), "the notice reaches the editor");
    let LiveStatus::Broken(why) = w.eds[0].le.status().clone() else { panic!("ended: {:?}", w.eds[0].le.status()) };
    assert!(why.starts_with("Saving has stopped"), "{why}");
    assert!(!w.eds[0].le.editable());
    assert_eq!(w.relay.borrow().ended().len(), 1, "ended once");
    // And the app stops hearing from the room, so its stale-room recovery takes it.
    let beats = w.count("heartbeat");
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    w.tick(1_000).await;
    assert_eq!(w.count("heartbeat"), beats, "no heartbeat once saving stopped");
}

#[tokio::test]
async fn an_editor_rejoining_after_the_worker_left_stops_editing() {
    let mut w = world(file(&["One", "Two"]), 1).await;
    w.eds[0].type_at_end(1, " and three");
    w.run_for(10_000, 1_000).await;
    assert_eq!(w.count("put"), 1);
    // The editor's link drops; the room empties and the worker closes it and leaves.
    let at = w.eds[0].at;
    w.relay.borrow_mut().leave(at);
    assert_eq!(w.run_for(60_000, 1_000).await, Flow::Done);
    assert!(matches!(w.eds[0].le.status(), LiveStatus::Reconnecting), "not told while away: {:?}", w.eds[0].le.status());
    // Its reconnect finds the room the relay still holds, with no saver in it.
    w.relay.borrow_mut().rejoin(at);
    for _ in 0..3 {
        w.eds[0].pump();
    }
    let LiveStatus::Broken(why) = w.eds[0].le.status().clone() else { panic!("ended: {:?}", w.eds[0].le.status()) };
    assert!(why.contains("Reopen"), "{why}");
    assert!(!w.eds[0].le.editable(), "nothing would save its edits");
}

#[tokio::test]
async fn the_first_join_checkpoints_the_room() {
    let mut w = world(file(&["One", "Two"]), 0).await;
    w.run_for(5_000, 1_000).await;
    let snapshot = w.relay.borrow().log.iter().any(|m| matches!(m, ServerMsg::Op { params, .. } if is_snap(params)));
    assert!(snapshot, "the room was checkpointed once it was caught up");
    assert!(w.relay.borrow().saved > 0, "and has a join baseline past the start (0 reads as never saved)");
    assert_eq!(w.count("put"), 0, "the file already holds it: no write");
    // An editor whose copy of the file is not the room's base (it changed since the worker read
    // it) restores the room's document from that checkpoint instead of seeding from its copy.
    let mut late = editor(&w.relay, "editor9", &file(&["Something else"]));
    for _ in 0..3 {
        late.pump();
        w.tick(1_000).await;
        late.pump();
    }
    assert!(late.le.editable(), "{:?}", late.le.status());
    assert_eq!(texts(&late.s.doc), ["One", "Two"], "the room's document, not the stale copy");
}

#[tokio::test]
async fn a_change_while_no_worker_served_the_room_is_kept_aside() {
    // The worker saves, then ends (a restart). While no worker serves the room, someone writes
    // the file outside it, and the editor keeps typing. The next worker reads the changed file:
    // its save must not write over that version unseen.
    let mut w = world(file(&["One", "Two"]), 1).await;
    w.eds[0].type_at_end(0, " fish");
    w.run_for(10_000, 1_000).await;
    assert_eq!(w.count("put"), 1);
    let gone = w.relay.borrow().conns.iter().position(|c| c.service && c.connected).unwrap();
    w.relay.borrow_mut().leave(gone);
    let outside = file(&["Changed outside"]);
    w.nc.lock().unwrap().files.insert(FILE.into(), outside.clone());
    w.eds[0].type_at_end(1, " more");
    w.tick(1_000).await;

    w.worker = Room::open(Relay::connect(&w.relay, SERVICE_UID), room_files(&w.base), w.clock.clone()).await.unwrap();
    w.run_for(20_000, 1_000).await;
    assert_eq!(w.count("put412"), 1, "the save found the file changed");
    let copy = "Plan (conflict 2026-11-21 1403).docx";
    assert_eq!(w.file(copy), outside, "the outside version is kept beside the file");
    assert_eq!(w.file_texts(), ["One fish", "Two more"], "the room's document is saved");
    assert!(w.relay.borrow().notices().iter().any(|n| n.contains(copy)), "the editors are told where it went");
}

#[tokio::test]
async fn writes_that_keep_failing_are_given_up() {
    let mut w = world(file(&["One"]), 1).await;
    w.nc.lock().unwrap().put_status = Some(StatusCode::SERVICE_UNAVAILABLE);
    w.eds[0].type_at_end(0, " fish");
    w.run_for(SAVE_GIVE_UP_MS - 30_000, 1_000).await;
    assert!(w.worker.saving_stopped().is_none(), "retried for minutes first");
    assert!(w.count("putfail") > 10, "and retried: {}", w.count("putfail"));
    w.run_for(60_000, 1_000).await;
    let stopped = w.worker.saving_stopped().map(str::to_owned).unwrap_or_default();
    assert!(stopped.contains("503"), "{stopped:?}");
    assert!(matches!(w.eds[0].le.status(), LiveStatus::Broken(_)), "the editor's session ended");
    let fails = w.count("putfail");
    w.run_for(60_000, 1_000).await;
    assert_eq!(w.count("putfail"), fails, "no more tries");
}

#[tokio::test]
async fn only_word_documents_are_served() {
    let (_nc, base) = nextcloud(file(&["One"])).await;
    let relay: Shared = Rc::default();
    let clock = Arc::new(TestClock(AtomicU64::new(0)));
    let got = FileGot { bytes: b"plain text".to_vec(), name: "Notes.txt".into(), etag: "e1".into() };
    let started = Room::start(Relay::connect(&relay, SERVICE_UID), room_files(&base), got, clock);
    assert!(matches!(started, Err(OpenError::Format(ref name)) if name == "Notes.txt"), "{:?}", started.err());
}

#[tokio::test]
async fn a_renamed_file_names_its_conflict_copy() {
    let mut w = world(file(&["One"]), 1).await;
    {
        let mut n = w.nc.lock().unwrap();
        n.name = Some("Budget.docx".into());
        n.files.insert(FILE.into(), file(&["Changed outside"]));
    }
    w.eds[0].type_at_end(0, " fish");
    w.run_for(10_000, 1_000).await;
    assert_eq!(w.file("Budget (conflict 2026-11-21 1403).docx"), file(&["Changed outside"]), "named after the file as it is now");
}
