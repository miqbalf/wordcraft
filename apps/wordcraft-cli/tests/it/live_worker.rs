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
    Clock, Config, Flow, Grant, NOTICE_KIND, Nextcloud, QUIET_MS, Room, RoomFiles, Rooms, Secret, Spawn, router, service_ticket,
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
        };
        self.conns[at].inbox.push_back(joined);
        self.conns[at].inbox.extend(replay);
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
                    self.conns[at].inbox.push_back(ServerMsg::Error { doc: DOC.into(), detail: detail.into(), local: Some(local) });
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
    ([(header::ETAG, format!("\"{}\"", etag_of(&bytes))), (header::CONTENT_DISPOSITION, format!("attachment; filename=\"{FILE}\""))], bytes)
        .into_response()
}

async fn nc_put(State(nc): State<NcState>, Path(room): Path<String>, h: HeaderMap, body: Bytes) -> Response {
    if !signed(&h, &room) {
        return deny(&nc);
    }
    let mut n = nc.lock().unwrap();
    let if_match = h.get(header::IF_MATCH).and_then(|v| v.to_str().ok()).unwrap_or("").trim_matches('"').to_owned();
    if if_match != etag_of(&n.files[FILE]) {
        n.calls.push("put412");
        return (StatusCode::PRECONDITION_FAILED, "etag mismatch").into_response();
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
    let mut w = World { relay, nc, clock, worker, eds };
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

#[tokio::test]
async fn saved_file_reopens_equal() {
    let md = "# Project Plan\n\nThis is the shared plan with **bold** text.\n\n- Item one\n- Item two\n";
    let bytes = save_bytes(FILE, &open_bytes("seed.md", md.as_bytes()).unwrap()).unwrap();
    let mut w = world(bytes, 2).await;
    w.eds[0].type_at_end(1, " Edited by one.");
    w.eds[1].run_at_end(0, "text.newParagraph", json!({}));
    w.eds[1].type_at_end(1, "Added by two.");
    w.eds[1].type_at_end(0, " (draft)");
    w.run_for(10_000, 1_000).await;
    assert_eq!(w.count("put"), 1);
    let saved = open_bytes(FILE, &w.file(FILE)).unwrap();
    for e in &w.eds {
        assert!(saved.same_content(&e.s.doc), "saved {:?} vs live {:?}", texts(&saved), texts(&e.s.doc));
    }
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
