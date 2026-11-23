//! The live session pump: real sessions joined through `LiveEditing` over in-memory hubs and a
//! fake relay that numbers ops, retains them for joiners and acknowledges saves.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use craft_collab::{ClientMsg, Hub, Presence, ServerMsg};
use serde_json::{Value, json};
use wordcraft_doc::{Block, Document, Pos};
use wordcraft_engine::io::save_bytes;
use wordcraft_engine::{Selection, Session};
use wordcraft_live::{Access, ENDED_KIND, LiveEditing, LiveStatus, NOTICE_KIND, SERVICE_UID, SaveEvent, document_from_state};

use super::{Rng, plain, random_off, texts};

const ROOM: &str = "doc:inst:42";

/// One connection as the fake relay sees it.
struct Conn {
    pid: String,
    display: String,
    may_edit: bool,
    service: bool,
    connected: bool,
    /// Refuse this peer's edits for good, as a relay that will not carry them (a refusal that
    /// may pass is sent again by `Live`, so only a final one breaks the replica).
    refuse_edits: bool,
    /// The hub fails to send, as a transport that lost the message.
    fail_sends: bool,
    inbox: VecDeque<ServerMsg>,
    /// Document submits this connection made, snapshots included.
    submits: usize,
}

/// A relay in memory: numbers every submit, broadcasts it to the room (sender included, with
/// its counter), keeps the ops after the last save for joiners, grants each join a fresh
/// space, keeps the newest ephemeral per peer and kind, and answers save reports. It does not
/// filter `saved` markers, so the client's own check is what is tested.
struct Relay {
    seq: u64,
    saved: u64,
    log: Vec<ServerMsg>,
    conns: Vec<Conn>,
    next_space: u32,
    ephemeral: Vec<(String, String, Value)>,
    /// Deliver a random prefix of each inbox per poll instead of all of it.
    rng: Option<Rng>,
    /// Deliver one message per poll.
    one_per_poll: bool,
    /// The relay restarted and lost the room's history: joins say so and replay nothing.
    lost_history: bool,
}

type Shared = Rc<RefCell<Relay>>;

fn relay(rng: Option<u64>) -> Shared {
    Rc::new(RefCell::new(Relay {
        seq: 0,
        saved: 0,
        log: Vec::new(),
        conns: Vec::new(),
        next_space: 1,
        ephemeral: Vec::new(),
        rng: rng.map(Rng),
        one_per_poll: false,
        lost_history: false,
    }))
}

#[derive(Clone)]
struct TestHub {
    relay: Shared,
    at: usize,
}

impl Hub for TestHub {
    fn send(&mut self, msg: ClientMsg) -> craft_collab::Result<()> {
        if self.relay.borrow().conns[self.at].fail_sends {
            return Err(craft_collab::Error::Hub("the link lost it".into()));
        }
        self.relay.borrow_mut().handle(self.at, msg);
        Ok(())
    }
    fn poll(&mut self) -> Vec<ServerMsg> {
        self.relay.borrow_mut().take(self.at)
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
    fn connect(relay: &Shared, uid: &str, may_edit: bool) -> TestHub {
        let at = {
            let mut r = relay.borrow_mut();
            let at = r.conns.len();
            r.conns.push(Conn {
                pid: format!("{uid}~{at:08x}"),
                display: uid.to_uppercase(),
                may_edit,
                service: uid == SERVICE_UID,
                connected: true,
                refuse_edits: false,
                fail_sends: false,
                inbox: VecDeque::new(),
                submits: 0,
            });
            r.join(at);
            at
        };
        TestHub { relay: relay.clone(), at }
    }

    fn roster(&self) -> Vec<Presence> {
        self.conns.iter().filter(|c| c.connected).map(|c| Presence { peer: c.pid.clone(), display: c.display.clone() }).collect()
    }

    fn join(&mut self, at: usize) {
        let space = self.next_space;
        self.next_space += 1;
        let replay: Vec<ServerMsg> =
            if self.lost_history { Vec::new() } else { self.log.iter().filter(|m| op_seq(m) > self.saved).cloned().collect() };
        let until = replay.last().map_or(self.saved, op_seq);
        let joined = ServerMsg::Joined {
            doc: ROOM.into(),
            peer: self.conns[at].pid.clone(),
            seq: self.saved,
            space,
            roster: self.roster(),
            locks: Vec::new(),
            replay_until: Some(until),
            fallback: false,
            text_cap: None,
            heartbeat: false,
            history_lost: self.lost_history,
        };
        let pid = self.conns[at].pid.clone();
        let others: Vec<ServerMsg> = self
            .ephemeral
            .iter()
            .filter(|(o, _, _)| *o != pid)
            .map(|(origin, kind, payload)| ServerMsg::Ephemeral {
                doc: ROOM.into(),
                origin: origin.clone(),
                kind: kind.clone(),
                payload: payload.clone(),
            })
            .collect();
        let conn = &mut self.conns[at];
        conn.inbox.push_back(joined);
        conn.inbox.extend(replay);
        conn.inbox.extend(others);
        let presence = ServerMsg::Presence { doc: ROOM.into(), roster: self.roster() };
        for (j, c) in self.conns.iter_mut().enumerate() {
            if j != at && c.connected {
                c.inbox.push_back(presence.clone());
            }
        }
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
                self.conns[at].submits += 1;
                let refusal = if !self.conns[at].may_edit {
                    Some("read-only access to this file")
                } else if is_snap(&params) && !self.conns[at].service {
                    Some("only the document service may checkpoint; ask the saver")
                } else if self.conns[at].refuse_edits {
                    Some("that edit is 99 bytes, more than this relay will carry")
                } else {
                    None
                };
                if let Some(detail) = refusal {
                    self.conns[at].inbox.push_back(ServerMsg::Error { doc: ROOM.into(), detail: detail.into(), local: Some(local), code: None });
                    return;
                }
                self.seq += 1;
                let op = ServerMsg::Op {
                    doc: ROOM.into(),
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
                    ServerMsg::SavedAck { doc: ROOM.into(), seq }
                } else {
                    ServerMsg::SavedRefused { doc: ROOM.into(), seq, reason: "no checkpoint follows that sequence".into() }
                };
                self.conns[at].inbox.push_back(answer);
            }
            ClientMsg::Ephemeral { kind, payload, .. } => {
                let origin = self.conns[at].pid.clone();
                self.ephemeral.retain(|(o, k, _)| !(*o == origin && *k == kind));
                self.ephemeral.push((origin.clone(), kind.clone(), payload.clone()));
                self.broadcast(&ServerMsg::Ephemeral { doc: ROOM.into(), origin, kind, payload }, Some(at));
            }
            _ => {}
        }
    }

    fn take(&mut self, at: usize) -> Vec<ServerMsg> {
        if !self.conns[at].connected {
            return Vec::new();
        }
        let len = self.conns[at].inbox.len();
        let n = match self.rng.as_mut() {
            _ if self.one_per_poll => len.min(1),
            Some(rng) => rng.below(len + 1),
            None => len,
        };
        self.conns[at].inbox.drain(..n).collect()
    }

    fn quiet(&self) -> bool {
        self.conns.iter().all(|c| c.inbox.is_empty() || !c.connected)
    }

    fn drop_link(&mut self, at: usize) {
        self.conns[at].connected = false;
        self.conns[at].inbox.clear();
        let presence = ServerMsg::Presence { doc: ROOM.into(), roster: self.roster() };
        self.broadcast(&presence, None);
    }

    /// The hub reconnecting with its original auth: a fresh join from the save baseline.
    fn restore_link(&mut self, at: usize) {
        self.conns[at].connected = true;
        self.join(at);
    }
}

/// One editor: a session and its pump.
struct Ed {
    s: Session,
    le: LiveEditing<TestHub>,
}

impl Ed {
    fn open(relay: &Shared, uid: &str, access: Access, bytes: &[u8]) -> Ed {
        let hub = Relay::connect(relay, uid, access == Access::Edit);
        let mut s = Session::new(Document::new());
        let le = LiveEditing::start_as(hub, ROOM, bytes, "plan.docx", &mut s, access).unwrap();
        Ed { s, le }
    }

    fn pump(&mut self) {
        self.le.pump(&mut self.s);
    }

    fn run(&mut self, id: &str, params: Value) -> bool {
        self.le.run(&mut self.s, id, &params).is_ok()
    }

    fn caret(&mut self, block: usize, off: usize) {
        self.s.sel = Selection { anchor: Pos::body(block, off), focus: Pos::body(block, off) };
    }

    fn end_of(&mut self, block: usize) {
        let len = self.s.doc.body[block].as_para().unwrap().text.len();
        self.caret(block, len);
    }
}

fn file(paras: &[&str]) -> Vec<u8> {
    save_bytes("plan.docx", &plain(paras)).unwrap()
}

fn settle(relay: &Shared, eds: &mut [&mut Ed]) {
    relay.borrow_mut().rng = None;
    relay.borrow_mut().one_per_poll = false;
    for _ in 0..50 {
        for e in eds.iter_mut() {
            e.pump();
        }
        if relay.borrow().quiet() {
            for e in eds.iter_mut() {
                e.pump();
            }
            return;
        }
    }
    panic!("the room never went quiet");
}

fn pids(d: &Document) -> Vec<u64> {
    d.body.iter().filter_map(|b| b.as_para()).map(|p| p.pid).collect()
}

fn assert_same(a: &Ed, b: &Ed, what: &str) {
    assert!(a.s.doc.same_content(&b.s.doc), "{what}: {:?} vs {:?}", texts(&a.s.doc), texts(&b.s.doc));
    assert_eq!(pids(&a.s.doc), pids(&b.s.doc), "{what}: the same paragraphs");
}

fn random_selection(s: &mut Session, rng: &mut Rng) {
    let paras: Vec<usize> = s.doc.body.iter().enumerate().filter(|(_, b)| matches!(***b, Block::Para(_))).map(|(i, _)| i).collect();
    let i = paras[rng.below(paras.len())];
    let from = random_off(&s.doc, i, rng);
    let (j, to) = match rng.below(4) {
        0 => (i, from),
        1 | 2 => (i, random_off(&s.doc, i, rng)),
        _ => {
            let j = paras[rng.below(paras.len())];
            (j, random_off(&s.doc, j, rng))
        }
    };
    let (lo, hi) = if (i, from) <= (j, to) { ((i, from), (j, to)) } else { ((j, to), (i, from)) };
    s.sel = Selection { anchor: Pos::body(lo.0, lo.1), focus: Pos::body(hi.0, hi.1) };
}

fn random_command(rng: &mut Rng) -> (&'static str, Value) {
    const WORDS: [&str; 5] = ["a", "b ", "é", "😀", "xyz "];
    match rng.below(9) {
        0..=2 => ("text.insert", json!({"text": WORDS[rng.below(WORDS.len())]})),
        3 => ("text.delete", json!({})),
        4 => ("text.backspace", json!({})),
        5 => ("text.newParagraph", json!({})),
        6 => ("format.bold", json!({})),
        7 => ("edit.undo", json!({})),
        _ => (if rng.below(2) == 0 { "para.moveUp" } else { "para.moveDown" }, json!({})),
    }
}

/// `n` editors, `steps` commands, pumps and partial deliveries interleaved at random. A
/// quarter of the commands go straight to the session, so the pump's flush picks them up.
fn converge_through_pump(n: usize, seed: u64, steps: usize) {
    converge_with(n, seed, steps, random_command);
}

/// [`converge_through_pump`] with commands drawn by `command`.
fn converge_with(n: usize, seed: u64, steps: usize, command: fn(&mut Rng) -> (&'static str, Value)) {
    let relay = relay(Some(seed ^ 0x5eed));
    let bytes = file(&["Alpha beta", "Gamma", "Delta epsilon zeta", ""]);
    let mut eds: Vec<Ed> = (0..n).map(|k| Ed::open(&relay, &format!("u{k}"), Access::Edit, &bytes)).collect();
    for e in &mut eds {
        e.pump();
    }
    let mut rng = Rng(seed);
    let (mut ran, mut tries) = (0, 0);
    while ran < steps && tries < steps * 20 {
        tries += 1;
        let k = rng.below(n);
        let e = &mut eds[k];
        if rng.below(3) == 0 || !e.le.editable() {
            e.pump();
            continue;
        }
        random_selection(&mut e.s, &mut rng);
        let (id, params) = command(&mut rng);
        if rng.below(4) == 0 {
            let _ = e.s.run(id, &params);
        } else {
            let _ = e.le.run(&mut e.s, id, &params);
        }
        ran += 1;
    }
    assert!(ran >= steps / 2, "seed {seed}: the editors went live and ran commands ({ran})");
    let mut refs: Vec<&mut Ed> = eds.iter_mut().collect();
    settle(&relay, &mut refs);
    for e in &eds[1..] {
        assert_same(&eds[0], e, &format!("seed {seed}, {n} editors"));
    }
    assert_ne!(texts(&eds[0].s.doc), ["Alpha beta", "Gamma", "Delta epsilon zeta", ""], "seed {seed}: the edits landed");
    for e in &eds {
        assert!(matches!(e.le.status(), LiveStatus::Live { .. }), "seed {seed}: still live: {:?}", e.le.status());
    }
}

#[test]
fn two_editors_converge_through_the_pump() {
    for seed in 0..8 {
        converge_through_pump(2, seed, 150);
    }
}

#[test]
fn three_editors_converge() {
    for seed in 100..106 {
        converge_through_pump(3, seed, 150);
    }
}

#[test]
fn joiner_after_save_matches() {
    let relay = relay(None);
    let bytes = file(&["One", "Two", "Three"]);
    let mut a = Ed::open(&relay, "alice", Access::Edit, &bytes);
    let mut b = Ed::open(&relay, "bob", Access::Edit, &bytes);
    let mut saver = Ed::open(&relay, SERVICE_UID, Access::Edit, &bytes);
    settle(&relay, &mut [&mut a, &mut b, &mut saver]);
    a.end_of(0);
    assert!(a.run("text.insert", json!({"text": " fish"})));
    b.end_of(2);
    assert!(b.run("text.newParagraph", json!({})));
    assert!(b.run("text.insert", json!({"text": "Four"})));
    settle(&relay, &mut [&mut a, &mut b, &mut saver]);

    let state = saver.le.checkpoint().unwrap();
    settle(&relay, &mut [&mut a, &mut b, &mut saver]);
    let events = saver.le.take_save_events();
    let Some(SaveEvent::Checkpointed { seq }) = events.first().cloned() else { panic!("checkpointed: {events:?}") };
    let saved = document_from_state(&saver.s.doc, &state).unwrap();
    assert!(saved.same_content(&a.s.doc), "the saved document is the room's: {:?}", texts(&saved));
    saver.le.announce_saved(seq, "etag-1").unwrap();
    settle(&relay, &mut [&mut a, &mut b, &mut saver]);
    assert_eq!(a.le.status(), &LiveStatus::Live { saved_at: Some("etag-1".into()) }, "editors see the save");

    // The relay dropped what the checkpoint covers; edits continue after it.
    a.caret(1, 0);
    assert!(a.run("text.insert", json!({"text": "Big "})));
    settle(&relay, &mut [&mut a, &mut b, &mut saver]);

    // A late joiner opens the original file and still ends up with the room's document.
    let mut c = Ed::open(&relay, "carol", Access::Edit, &bytes);
    settle(&relay, &mut [&mut a, &mut b, &mut saver, &mut c]);
    assert_same(&a, &c, "joiner after save");
    assert!(texts(&c.s.doc).iter().any(|t| t == "Big Two"), "{:?}", texts(&c.s.doc));
    assert_eq!(c.le.status(), &LiveStatus::Live { saved_at: Some("etag-1".into()) }, "the joiner is told of the save");
    c.end_of(0);
    assert!(c.run("text.insert", json!({"text": "!"})));
    settle(&relay, &mut [&mut a, &mut b, &mut saver, &mut c]);
    assert_same(&a, &c, "the joiner's edit lands everywhere");
    assert_same(&b, &c, "the joiner's edit lands everywhere");
}

#[test]
fn read_ticket_cannot_send() {
    let relay = relay(None);
    let bytes = file(&["Shared text"]);
    let mut a = Ed::open(&relay, "alice", Access::Edit, &bytes);
    let mut r = Ed::open(&relay, "bob", Access::Read, &bytes);
    settle(&relay, &mut [&mut a, &mut r]);
    assert_eq!(r.le.status(), &LiveStatus::ReadOnly);
    assert!(!r.le.editable() && !r.le.allows("text.insert"));
    assert!(r.le.allows("caret.right"), "moving around is still allowed");
    r.end_of(0);
    assert!(!r.run("text.insert", json!({"text": " nope"})), "the pump refuses the command");
    // An edit that reaches the session anyway is put back, never sent.
    let _ = r.s.run("text.insert", &json!({"text": " nope"}));
    r.pump();
    assert_eq!(texts(&r.s.doc), ["Shared text"]);
    a.end_of(0);
    assert!(a.run("text.insert", json!({"text": "!"})));
    settle(&relay, &mut [&mut a, &mut r]);
    assert_eq!(texts(&r.s.doc), ["Shared text!"], "the reader follows the editors");
    assert_eq!(relay.borrow().conns[1].submits, 0, "nothing was submitted by the reader");
    assert_eq!(r.le.status(), &LiveStatus::ReadOnly);
}

#[test]
fn edits_are_refused_until_live() {
    let relay = relay(None);
    let bytes = file(&["Hello"]);
    let mut a = Ed::open(&relay, "alice", Access::Edit, &bytes);
    let mut b = Ed::open(&relay, "bob", Access::Edit, &bytes);
    // Before the first pump nothing has been granted.
    assert_eq!(a.le.status(), &LiveStatus::Connecting);
    assert!(!a.le.editable() && !a.le.allows("text.insert"));
    a.end_of(0);
    assert!(!a.run("text.insert", json!({"text": " early"})));
    let _ = a.s.run("text.insert", &json!({"text": " early"}));
    a.pump();
    assert_eq!(texts(&a.s.doc), ["Hello"], "the early edit was put back");
    assert!(!a.le.take_notices().is_empty(), "and the user was told");
    settle(&relay, &mut [&mut a, &mut b]);
    assert!(a.le.editable());

    // A dropped link pauses editing until the rejoin.
    relay.borrow_mut().drop_link(0);
    a.pump();
    assert_eq!(a.le.status(), &LiveStatus::Reconnecting);
    a.end_of(0);
    assert!(!a.run("text.insert", json!({"text": " offline"})));
    let _ = a.s.run("text.insert", &json!({"text": " offline"}));
    a.pump();
    assert_eq!(texts(&a.s.doc), ["Hello"]);
    b.end_of(0);
    assert!(b.run("text.insert", json!({"text": " world"})));
    relay.borrow_mut().restore_link(0);
    settle(&relay, &mut [&mut a, &mut b]);
    assert!(a.le.editable(), "live again: {:?}", a.le.status());
    a.end_of(0);
    assert!(a.run("text.insert", json!({"text": "!"})));
    settle(&relay, &mut [&mut a, &mut b]);
    assert_same(&a, &b, "after the rejoin");
    assert_eq!(texts(&b.s.doc), ["Hello world!"]);
}

#[test]
fn a_non_shareable_edit_is_put_back_with_a_notice() {
    let relay = relay(None);
    let bytes = file(&["Hello", "World"]);
    let mut a = Ed::open(&relay, "alice", Access::Edit, &bytes);
    let mut b = Ed::open(&relay, "bob", Access::Edit, &bytes);
    settle(&relay, &mut [&mut a, &mut b]);
    assert!(a.le.allows("text.insert") && !a.le.allows("insert.picture"));
    a.end_of(0);
    assert!(!a.run("layout.orientation", json!({"value": "landscape"})), "the pump refuses it");
    let said = a.le.take_notices();
    assert!(said.iter().any(|n| n.contains("Page setup")), "{said:?}");
    // The same edit arriving through the session directly is put back.
    let before = a.s.doc.clone();
    let _ = a.s.run("layout.orientation", &json!({"value": "landscape"}));
    assert!(!before.same_content(&a.s.doc), "the command changed the document");
    a.pump();
    assert!(before.same_content(&a.s.doc), "put back");
    let said = a.le.take_notices();
    assert!(said.iter().any(|n| n.contains("Page setup")), "{said:?}");
    let _ = a.s.run("insert.bookmark", &json!({"name": "here"}));
    a.pump();
    assert!(before.same_content(&a.s.doc), "put back");
    let said = a.le.take_notices();
    assert!(said.iter().any(|n| n.contains("link or bookmark")), "{said:?}");
    assert!(a.run("text.insert", json!({"text": "!"})), "shareable editing carries on");
    settle(&relay, &mut [&mut a, &mut b]);
    assert_same(&a, &b, "only the shared edit landed");
}

#[test]
fn undo_keeps_remote_edits() {
    let relay = relay(None);
    let bytes = file(&["One", "Two"]);
    let mut a = Ed::open(&relay, "alice", Access::Edit, &bytes);
    let mut b = Ed::open(&relay, "bob", Access::Edit, &bytes);
    settle(&relay, &mut [&mut a, &mut b]);
    a.end_of(0);
    assert!(a.run("text.insert", json!({"text": " mine"})));
    settle(&relay, &mut [&mut a, &mut b]);
    b.end_of(1);
    assert!(b.run("text.insert", json!({"text": " theirs"})));
    settle(&relay, &mut [&mut a, &mut b]);
    assert_eq!(texts(&a.s.doc), ["One mine", "Two theirs"]);
    assert!(a.run("edit.undo", json!({})));
    settle(&relay, &mut [&mut a, &mut b]);
    assert_eq!(texts(&a.s.doc), ["One", "Two theirs"], "undo took back only the local edit");
    assert_same(&a, &b, "after undo");
    assert!(a.run("edit.redo", json!({})));
    settle(&relay, &mut [&mut a, &mut b]);
    assert_eq!(texts(&b.s.doc), ["One mine", "Two theirs"], "redo puts it back everywhere");
    assert_same(&a, &b, "after redo");
}

#[test]
fn cursor_rate_is_capped() {
    let relay = relay(None);
    let bytes = file(&["Hello world"]);
    let mut a = Ed::open(&relay, "alice", Access::Edit, &bytes);
    let mut b = Ed::open(&relay, "bob", Access::Edit, &bytes);
    settle(&relay, &mut [&mut a, &mut b]);
    let now = std::sync::Arc::new(std::sync::Mutex::new(0.0_f64));
    let clock = now.clone();
    a.le.set_clock(Box::new(move || *clock.lock().unwrap()));
    let pid = a.s.doc.body[0].as_para().unwrap().pid;
    let sent =
        |relay: &Shared| relay.borrow().conns[1].inbox.iter().filter(|m| matches!(m, ServerMsg::Ephemeral { kind, .. } if kind == "cursor")).count();
    // One simulated second of a caret moving at 100 Hz, pumped each step.
    for step in 0..100 {
        *now.lock().unwrap() = f64::from(step) * 10.0;
        a.le.send_cursor(pid, step as usize % 11, None);
        a.pump();
    }
    let n = sent(&relay);
    assert!((2..=6).contains(&n), "at most five a second, not none: {n}");
    // The newest position still arrives once the interval has passed.
    *now.lock().unwrap() = 5_000.0;
    a.le.send_cursor(pid, 4, None);
    a.pump();
    let peers = b.le.pump(&mut b.s);
    let alice = peers.iter().find(|p| p.id.starts_with("alice")).unwrap();
    assert_eq!(alice.caret, Some((pid, 4)));
}

#[test]
fn peer_carets_follow_remote_edits() {
    let relay = relay(None);
    let bytes = file(&["Hello world", "Second"]);
    let mut a = Ed::open(&relay, "alice", Access::Edit, &bytes);
    let mut b = Ed::open(&relay, "bob", Access::Edit, &bytes);
    settle(&relay, &mut [&mut a, &mut b]);
    let first = a.s.doc.body[0].as_para().unwrap().pid;
    let second = a.s.doc.body[1].as_para().unwrap().pid;
    a.le.send_cursor(first, 6, None);
    a.pump();
    b.le.send_cursor(second, 6, None);
    b.pump();
    settle(&relay, &mut [&mut a, &mut b]);
    // Alice types before her own caret; Bob deletes the paragraph his own caret is in.
    a.caret(0, 0);
    assert!(a.run("text.insert", json!({"text": ">> "})));
    b.caret(0, 11);
    assert!(b.run("text.delete", json!({})), "join Second onto the first paragraph");
    settle(&relay, &mut [&mut a, &mut b]);
    let on_b = b.le.pump(&mut b.s);
    let alice = on_b.iter().find(|p| p.id.starts_with("alice")).unwrap();
    assert_eq!(alice.caret, Some((first, 9)), "shifted by the text typed before it");
    let on_a = a.le.pump(&mut a.s);
    let bob = on_a.iter().find(|p| p.id.starts_with("bob")).unwrap();
    assert_eq!(bob.caret, None, "dropped with its paragraph");
}

#[test]
fn saved_from_a_peer_that_is_not_the_service_is_ignored() {
    let relay = relay(None);
    let bytes = file(&["Hello"]);
    let mut a = Ed::open(&relay, "alice", Access::Edit, &bytes);
    let mut b = Ed::open(&relay, "mallory", Access::Edit, &bytes);
    let mut saver = Ed::open(&relay, SERVICE_UID, Access::Edit, &bytes);
    settle(&relay, &mut [&mut a, &mut b, &mut saver]);
    b.le.announce_saved(99, "forged").unwrap();
    settle(&relay, &mut [&mut a, &mut b, &mut saver]);
    assert_eq!(a.le.status(), &LiveStatus::Live { saved_at: None }, "a forged marker changes nothing");
    saver.le.announce_saved(3, "real").unwrap();
    settle(&relay, &mut [&mut a, &mut b, &mut saver]);
    assert_eq!(a.le.status(), &LiveStatus::Live { saved_at: Some("real".into()) });
    // Malformed and older markers from the service are ignored too.
    let service = relay.borrow().conns[2].pid.clone();
    let malformed = ServerMsg::Ephemeral { doc: ROOM.into(), origin: service, kind: "saved".into(), payload: json!({"seq": 9}) };
    relay.borrow_mut().conns[0].inbox.push_back(malformed);
    saver.le.announce_saved(2, "older").unwrap();
    settle(&relay, &mut [&mut a, &mut b, &mut saver]);
    assert_eq!(a.le.status(), &LiveStatus::Live { saved_at: Some("real".into()) });
    // A peer leaving does not forget it, and the service is not shown as an editor.
    relay.borrow_mut().drop_link(1);
    let peers = a.le.pump(&mut a.s);
    assert_eq!(a.le.status(), &LiveStatus::Live { saved_at: Some("real".into()) });
    assert!(peers.iter().all(|p| !p.id.starts_with(SERVICE_UID) && !p.id.starts_with("mallory")), "{peers:?}");
}

#[test]
fn a_broken_replica_stops_editing() {
    let relay = relay(None);
    let bytes = file(&["Hello"]);
    let mut a = Ed::open(&relay, "alice", Access::Edit, &bytes);
    let mut b = Ed::open(&relay, "bob", Access::Edit, &bytes);
    settle(&relay, &mut [&mut a, &mut b]);
    relay.borrow_mut().conns[0].refuse_edits = true;
    a.end_of(0);
    assert!(a.run("text.insert", json!({"text": " lost"})));
    settle(&relay, &mut [&mut a, &mut b]);
    let LiveStatus::Broken(msg) = a.le.status().clone() else { panic!("broken: {:?}", a.le.status()) };
    assert!(msg.contains("reopen") || msg.contains("Reopen"), "{msg}");
    assert!(!a.le.editable() && !a.le.allows("text.insert"));
    assert!(!a.run("text.insert", json!({"text": "x"})));
    assert_eq!(texts(&b.s.doc), ["Hello"], "the refused edit never reached the room");
}

#[test]
fn typing_into_a_paragraph_joined_away_is_recovered_through_the_pump() {
    let relay = relay(None);
    let bytes = file(&["One", "Two", "Three"]);
    let mut a = Ed::open(&relay, "alice", Access::Edit, &bytes);
    let mut b = Ed::open(&relay, "bob", Access::Edit, &bytes);
    settle(&relay, &mut [&mut a, &mut b]);
    // Alice joins "Two" onto "One" while Bob, not having seen it, types into "Two".
    a.end_of(0);
    assert!(a.run("text.delete", json!({})));
    b.caret(1, 2);
    assert!(b.run("text.insert", json!({"text": "XYZ"})));
    settle(&relay, &mut [&mut a, &mut b]);
    assert_same(&a, &b, "after the race");
    assert!(texts(&a.s.doc).iter().any(|t| t.contains("XYZ")), "the typing survives: {:?}", texts(&a.s.doc));
}

/// The saver checkpoints and announces the save: pumping every editor, retrying on a race.
fn save(eds: &mut [&mut Ed], saver: usize, etag: &str) {
    for _ in 0..200 {
        if eds[saver].le.checkpoint().is_ok() {
            for _ in 0..50 {
                for e in eds.iter_mut() {
                    e.pump();
                }
                let events = eds[saver].le.take_save_events();
                if let Some(SaveEvent::Checkpointed { seq }) = events.first().cloned() {
                    eds[saver].le.announce_saved(seq, etag).unwrap();
                    return;
                }
                if !events.is_empty() {
                    break;
                }
            }
        }
        for e in eds.iter_mut() {
            e.pump();
        }
    }
    panic!("the checkpoint never went through");
}

#[test]
fn live_waits_for_the_whole_catch_up() {
    let relay = relay(None);
    let bytes = file(&["One", "Two"]);
    let mut a = Ed::open(&relay, "alice", Access::Edit, &bytes);
    let mut saver = Ed::open(&relay, SERVICE_UID, Access::Edit, &bytes);
    settle(&relay, &mut [&mut a, &mut saver]);
    a.end_of(0);
    assert!(a.run("text.insert", json!({"text": " fish"})));
    settle(&relay, &mut [&mut a, &mut saver]);
    save(&mut [&mut a, &mut saver], 1, "e1");
    a.end_of(1);
    assert!(a.run("text.insert", json!({"text": " more"})));
    settle(&relay, &mut [&mut a, &mut saver]);

    // A late joiner whose catch-up (snapshot first) comes one message per poll.
    relay.borrow_mut().one_per_poll = true;
    let mut c = Ed::open(&relay, "carol", Access::Edit, &bytes);
    c.pump();
    assert_eq!(c.le.status(), &LiveStatus::Connecting, "joined, the catch-up not here yet");
    c.end_of(0);
    assert!(!c.run("text.insert", json!({"text": " early"})), "refused while catching up");
    let _ = c.s.run("text.insert", &json!({"text": " early"}));
    c.pump();
    assert!(!texts(&c.s.doc).iter().any(|t| t.contains("early")), "put back: {:?}", texts(&c.s.doc));
    for _ in 0..20 {
        c.pump();
    }
    assert!(c.le.editable(), "live once the catch-up arrived: {:?}", c.le.status());
    assert_same(&a, &c, "the joiner has the room's document");
    c.end_of(0);
    assert!(c.run("text.insert", json!({"text": "!"})));
    settle(&relay, &mut [&mut a, &mut saver, &mut c]);
    assert_same(&a, &c, "the joiner's edit lands");
    assert_eq!(texts(&a.s.doc), ["One fish!", "Two more"]);
}

#[test]
fn falling_too_far_behind_stops_editing() {
    let relay = relay(None);
    let bytes = file(&["Hello"]);
    let mut a = Ed::open(&relay, "alice", Access::Edit, &bytes);
    settle(&relay, &mut [&mut a]);
    relay.borrow_mut().conns[0].inbox.push_back(ServerMsg::Snapshot { doc: ROOM.into(), seq: 40, document: Value::Null });
    a.pump();
    let LiveStatus::Broken(msg) = a.le.status().clone() else { panic!("broken: {:?}", a.le.status()) };
    assert!(msg.contains("Reopen"), "{msg}");
    assert!(!a.le.editable());
}

#[test]
fn an_incomplete_catch_up_stops_editing() {
    let relay = relay(None);
    let bytes = file(&["Hello"]);
    let mut a = Ed::open(&relay, "alice", Access::Edit, &bytes);
    settle(&relay, &mut [&mut a]);
    let lost = ServerMsg::TextOps { doc: ROOM.into(), scope: "doc".into(), ops: json!([]), complete: false, snap: false, more: false };
    relay.borrow_mut().conns[0].inbox.push_back(lost);
    a.pump();
    let LiveStatus::Broken(msg) = a.le.status().clone() else { panic!("broken: {:?}", a.le.status()) };
    assert!(msg.contains("Reopen"), "{msg}");
}

#[test]
fn an_edit_that_cannot_be_sent_stops_editing() {
    let relay = relay(None);
    let bytes = file(&["Hello"]);
    let mut a = Ed::open(&relay, "alice", Access::Edit, &bytes);
    let mut b = Ed::open(&relay, "bob", Access::Edit, &bytes);
    settle(&relay, &mut [&mut a, &mut b]);
    relay.borrow_mut().conns[0].fail_sends = true;
    a.end_of(0);
    let _ = a.run("text.insert", json!({"text": " lost"}));
    a.pump();
    assert!(matches!(a.le.status(), LiveStatus::Broken(_)), "{:?}", a.le.status());
    assert!(a.le.take_notices().iter().any(|n| n.contains("could not be sent")));
    assert!(!a.le.editable());
}

#[test]
fn a_read_ticket_opened_for_editing_says_it_is_read_only() {
    let relay = relay(None);
    let bytes = file(&["Hello"]);
    let hub = Relay::connect(&relay, "bob", false);
    let mut s = Session::new(Document::new());
    let mut le = LiveEditing::start(hub, ROOM, &bytes, "plan.docx", &mut s).unwrap();
    le.pump(&mut s);
    assert!(le.editable(), "the relay has not said yet");
    s.sel = Selection { anchor: Pos::body(0, 5), focus: Pos::body(0, 5) };
    let _ = le.run(&mut s, "text.insert", &json!({"text": "!"}));
    le.pump(&mut s);
    let LiveStatus::Broken(msg) = le.status().clone() else { panic!("broken: {:?}", le.status()) };
    assert!(msg.contains("only read"), "{msg}");
}

#[test]
fn undo_after_recovery_keeps_the_recovered_text() {
    let relay = relay(None);
    let bytes = file(&["One", "Two", "Three"]);
    let mut a = Ed::open(&relay, "alice", Access::Edit, &bytes);
    let mut b = Ed::open(&relay, "bob", Access::Edit, &bytes);
    settle(&relay, &mut [&mut a, &mut b]);
    // Bob joins "Two" onto "One" while Alice types into "Two"; Bob recovers her typing.
    b.end_of(0);
    assert!(b.run("text.delete", json!({})));
    a.caret(1, 2);
    assert!(a.run("text.insert", json!({"text": "XYZ"})));
    settle(&relay, &mut [&mut a, &mut b]);
    assert!(texts(&b.s.doc).iter().any(|t| t.contains("XYZ")), "recovered: {:?}", texts(&b.s.doc));
    // Bob's Undo takes back his join, never the recovered text.
    assert!(b.run("edit.undo", json!({})));
    settle(&relay, &mut [&mut a, &mut b]);
    assert_same(&a, &b, "after the undo");
    assert!(texts(&a.s.doc).iter().any(|t| t.contains("XYZ")), "Alice's typing survives: {:?}", texts(&a.s.doc));
    assert!(texts(&b.s.doc).iter().any(|t| t == "One"), "the join was undone: {:?}", texts(&b.s.doc));
}

#[test]
fn undo_inside_text_another_editor_typed_into() {
    let relay = relay(None);
    let bytes = file(&["One"]);
    let mut a = Ed::open(&relay, "alice", Access::Edit, &bytes);
    let mut b = Ed::open(&relay, "bob", Access::Edit, &bytes);
    settle(&relay, &mut [&mut a, &mut b]);
    a.end_of(0);
    assert!(a.run("text.insert", json!({"text": "abcd"})));
    settle(&relay, &mut [&mut a, &mut b]);
    b.caret(0, 5);
    assert!(b.run("text.insert", json!({"text": "XY"})));
    settle(&relay, &mut [&mut a, &mut b]);
    assert_eq!(texts(&a.s.doc), ["OneabXYcd"]);
    assert!(a.run("edit.undo", json!({})));
    settle(&relay, &mut [&mut a, &mut b]);
    assert_same(&a, &b, "after the undo");
    assert_eq!(texts(&b.s.doc), ["OneXY"], "only Alice's characters went");
}

/// Random commands and pumps by every editor but the saver (index 2), which only pumps.
fn random_phase(eds: &mut [Ed], rng: &mut Rng, steps: usize) {
    let editors: Vec<usize> = (0..eds.len()).filter(|k| *k != 2).collect();
    for _ in 0..steps * 3 {
        let k = editors[rng.below(editors.len())];
        if rng.below(3) == 0 || !eds[k].le.editable() {
            eds[k].pump();
            eds[2].pump();
            continue;
        }
        random_selection(&mut eds[k].s, rng);
        let (id, params) = random_command(rng);
        let e = &mut eds[k];
        let _ = e.le.run(&mut e.s, id, &params);
    }
}

#[test]
fn editors_converge_across_a_save_and_a_late_joiner() {
    for seed in 200..204 {
        let relay = relay(Some(seed));
        let bytes = file(&["Alpha beta", "Gamma", "Delta epsilon zeta", ""]);
        let mut eds: Vec<Ed> = ["u0", "u1", SERVICE_UID].iter().map(|u| Ed::open(&relay, u, Access::Edit, &bytes)).collect();
        let mut rng = Rng(seed);
        random_phase(&mut eds, &mut rng, 60);
        {
            let mut refs: Vec<&mut Ed> = eds.iter_mut().collect();
            save(&mut refs, 2, "e1");
        }
        // Editing continues under the random relay while a late joiner comes in.
        relay.borrow_mut().rng = Some(Rng(seed ^ 0xfeed));
        eds.push(Ed::open(&relay, "late", Access::Edit, &bytes));
        random_phase(&mut eds, &mut rng, 60);
        let mut refs: Vec<&mut Ed> = eds.iter_mut().collect();
        settle(&relay, &mut refs);
        for e in &eds[1..] {
            assert_same(&eds[0], e, &format!("seed {seed}"));
        }
        assert!(eds.iter().all(|e| matches!(e.le.status(), LiveStatus::Live { saved_at: Some(_) })), "seed {seed}: all live and saved");
    }
}

#[test]
fn an_undo_that_cannot_be_sent_stops_editing() {
    let relay = relay(None);
    let bytes = file(&["Hello"]);
    let mut a = Ed::open(&relay, "alice", Access::Edit, &bytes);
    let mut b = Ed::open(&relay, "bob", Access::Edit, &bytes);
    settle(&relay, &mut [&mut a, &mut b]);
    a.end_of(0);
    assert!(a.run("text.insert", json!({"text": " world"})));
    settle(&relay, &mut [&mut a, &mut b]);
    relay.borrow_mut().conns[0].fail_sends = true;
    let _ = a.run("edit.undo", json!({}));
    a.pump();
    assert!(matches!(a.le.status(), LiveStatus::Broken(_)), "{:?}", a.le.status());
    assert!(a.le.take_notices().iter().any(|n| n.contains("could not be sent")));
    assert_eq!(texts(&b.s.doc), ["Hello world"], "the room never saw the undo");
}

#[test]
fn a_catch_up_that_never_arrives_ends_in_reopen() {
    let relay = relay(None);
    let bytes = file(&["Hello"]);
    let mut a = Ed::open(&relay, "alice", Access::Edit, &bytes);
    settle(&relay, &mut [&mut a]);
    a.end_of(0);
    assert!(a.run("text.insert", json!({"text": "!"})));
    settle(&relay, &mut [&mut a]);
    // A joiner gets its join, then the replay is lost.
    let mut c = Ed::open(&relay, "carol", Access::Edit, &bytes);
    let now = std::sync::Arc::new(std::sync::Mutex::new(0.0_f64));
    let clock = now.clone();
    c.le.set_clock(Box::new(move || *clock.lock().unwrap()));
    relay.borrow_mut().one_per_poll = true;
    c.pump();
    relay.borrow_mut().conns[1].inbox.clear();
    assert_eq!(c.le.status(), &LiveStatus::Connecting);
    *now.lock().unwrap() = 11_000.0;
    c.pump();
    assert_eq!(c.le.status(), &LiveStatus::Connecting, "late, not given up yet");
    assert!(c.le.take_notices().iter().any(|n| n.contains("catching up")));
    *now.lock().unwrap() = 21_000.0;
    c.pump();
    let LiveStatus::Broken(msg) = c.le.status().clone() else { panic!("broken: {:?}", c.le.status()) };
    assert!(msg.contains("Reopen"), "{msg}");
    *now.lock().unwrap() = 60_000.0;
    c.pump();
    assert!(c.le.take_notices().is_empty(), "no more catch-up notices once broken");
}

/// `peer` (a connection of the fake relay) sends ephemeral `kind` with `payload` to the room.
fn tell(relay: &Shared, peer: usize, kind: &str, payload: Value) {
    let msg = ClientMsg::Ephemeral { doc: ROOM.into(), kind: kind.into(), payload };
    relay.borrow_mut().handle(peer, msg);
}

#[test]
fn a_service_notice_reaches_the_editors_once() {
    let relay = relay(None);
    let bytes = file(&["Hello"]);
    let mut a = Ed::open(&relay, "alice", Access::Edit, &bytes);
    let mut m = Ed::open(&relay, "mallory", Access::Edit, &bytes);
    let mut saver = Ed::open(&relay, SERVICE_UID, Access::Edit, &bytes);
    settle(&relay, &mut [&mut a, &mut m, &mut saver]);
    a.le.take_notices();
    tell(&relay, 1, NOTICE_KIND, json!({"text": "Forged notice."}));
    tell(&relay, 2, NOTICE_KIND, json!({"text": "Their version was kept as Plan (conflict).docx."}));
    settle(&relay, &mut [&mut a, &mut m, &mut saver]);
    assert_eq!(a.le.take_notices(), ["Their version was kept as Plan (conflict).docx."], "the service's notice only");
    // A rejoin's catch-up sends the newest notice again: it is not told twice.
    relay.borrow_mut().drop_link(0);
    a.pump();
    relay.borrow_mut().restore_link(0);
    settle(&relay, &mut [&mut a, &mut m, &mut saver]);
    assert!(a.le.take_notices().iter().all(|n| !n.contains("conflict")), "told once");
    assert!(matches!(a.le.status(), LiveStatus::Live { .. }), "a notice does not end the link");
}

#[test]
fn the_service_ending_the_session_ends_the_link() {
    let relay = relay(None);
    let bytes = file(&["Hello"]);
    let mut a = Ed::open(&relay, "alice", Access::Edit, &bytes);
    let mut m = Ed::open(&relay, "mallory", Access::Edit, &bytes);
    let mut saver = Ed::open(&relay, SERVICE_UID, Access::Edit, &bytes);
    settle(&relay, &mut [&mut a, &mut m, &mut saver]);
    tell(&relay, 1, ENDED_KIND, json!({"reason": "forged"}));
    settle(&relay, &mut [&mut a, &mut m, &mut saver]);
    assert!(a.le.editable(), "only the service ends the session");
    tell(&relay, 2, ENDED_KIND, json!({"reason": "Saving has stopped. Download a copy, then reopen the file."}));
    settle(&relay, &mut [&mut a, &mut m, &mut saver]);
    assert_eq!(a.le.status(), &LiveStatus::Broken("Saving has stopped. Download a copy, then reopen the file.".into()));
    assert!(!a.le.editable());
    a.end_of(0);
    assert!(!a.run("text.insert", json!({"text": "x"})), "no edits once ended");
}

#[test]
fn an_editor_rejoining_a_room_the_service_closed_stops_editing() {
    let relay = relay(None);
    let bytes = file(&["Hello"]);
    let mut a = Ed::open(&relay, "alice", Access::Edit, &bytes);
    let mut saver = Ed::open(&relay, SERVICE_UID, Access::Edit, &bytes);
    settle(&relay, &mut [&mut a, &mut saver]);
    save(&mut [&mut a, &mut saver], 1, "etag-1");
    settle(&relay, &mut [&mut a, &mut saver]);
    // Alice's link is down when the service closes the room and leaves.
    relay.borrow_mut().drop_link(0);
    a.pump();
    let (seq, etag) = saver.le.saved().map(|(s, e)| (s, e.to_owned())).unwrap();
    saver.le.announce_closed(seq, &etag).unwrap();
    relay.borrow_mut().drop_link(1);
    assert!(!matches!(a.le.status(), LiveStatus::Broken(_)), "not told yet: {:?}", a.le.status());
    // The relay still holds the room; her reconnect gets the closed marker in its catch-up.
    relay.borrow_mut().restore_link(0);
    settle(&relay, &mut [&mut a]);
    let LiveStatus::Broken(why) = a.le.status().clone() else { panic!("broken: {:?}", a.le.status()) };
    assert!(why.contains("Reopen"), "{why}");
}

#[test]
fn the_host_can_end_the_link() {
    let relay = relay(None);
    let bytes = file(&["Hello"]);
    let mut a = Ed::open(&relay, "alice", Access::Edit, &bytes);
    settle(&relay, &mut [&mut a]);
    a.le.end("The live session was restarted. Reopen the file.");
    assert_eq!(a.le.status(), &LiveStatus::Broken("The live session was restarted. Reopen the file.".into()));
    settle(&relay, &mut [&mut a]);
    assert!(matches!(a.le.status(), LiveStatus::Broken(_)), "for good");
}

/// The relay restarts and loses the room's history: nothing to replay, joins say so.
fn lose_history(relay: &Shared) {
    let mut r = relay.borrow_mut();
    r.lost_history = true;
    r.log.clear();
    r.ephemeral.clear();
    // The relay keeps the room's sequence: joins start at it, with nothing before it to replay.
    r.saved = r.seq;
}

#[test]
fn a_fresh_window_waits_for_the_snapshot_after_the_room_lost_its_history() {
    let relay = relay(None);
    let bytes = file(&["Hello"]);
    let mut a = Ed::open(&relay, "alice", Access::Edit, &bytes);
    let mut saver = Ed::open(&relay, SERVICE_UID, Access::Edit, &bytes);
    settle(&relay, &mut [&mut a, &mut saver]);
    a.end_of(0);
    assert!(a.run("text.insert", json!({"text": " world"})));
    settle(&relay, &mut [&mut a, &mut saver]);
    lose_history(&relay);
    // A window opened now seeds from a file that may not be the room's document.
    let mut c = Ed::open(&relay, "carol", Access::Edit, &bytes);
    let now = std::sync::Arc::new(std::sync::Mutex::new(0.0_f64));
    let clock = now.clone();
    c.le.set_clock(Box::new(move || *clock.lock().unwrap()));
    settle(&relay, &mut [&mut a, &mut saver, &mut c]);
    assert_eq!(c.le.status(), &LiveStatus::Connecting, "it waits");
    assert!(!c.le.editable());
    // The service, holding the document, checkpoints it: the waiting window restarts on it.
    save(&mut [&mut a, &mut saver, &mut c], 1, "etag-2");
    settle(&relay, &mut [&mut a, &mut saver, &mut c]);
    assert!(c.le.editable(), "{:?}", c.le.status());
    assert_eq!(texts(&c.s.doc), ["Hello world"], "the room's document, not its own file");
}

#[test]
fn a_fresh_window_with_no_snapshot_coming_ends() {
    let relay = relay(None);
    let bytes = file(&["Hello"]);
    let mut a = Ed::open(&relay, "alice", Access::Edit, &bytes);
    settle(&relay, &mut [&mut a]);
    lose_history(&relay);
    let mut c = Ed::open(&relay, "carol", Access::Edit, &bytes);
    let now = std::sync::Arc::new(std::sync::Mutex::new(0.0_f64));
    let clock = now.clone();
    c.le.set_clock(Box::new(move || *clock.lock().unwrap()));
    settle(&relay, &mut [&mut a, &mut c]);
    *now.lock().unwrap() = 10_000.0;
    c.pump();
    assert_eq!(c.le.status(), &LiveStatus::Connecting, "still waiting");
    *now.lock().unwrap() = 21_000.0;
    c.pump();
    let LiveStatus::Broken(why) = c.le.status().clone() else { panic!("ended: {:?}", c.le.status()) };
    assert!(why.contains("lost its history") && why.contains("Download a copy"), "{why}");
}

#[test]
fn a_settled_window_goes_on_after_the_room_lost_its_history() {
    let relay = relay(None);
    let bytes = file(&["Hello"]);
    let mut a = Ed::open(&relay, "alice", Access::Edit, &bytes);
    settle(&relay, &mut [&mut a]);
    // Settled on the room's history: an edit of its own was ordered and came back.
    a.end_of(0);
    assert!(a.run("text.insert", json!({"text": "!"})));
    settle(&relay, &mut [&mut a]);
    relay.borrow_mut().drop_link(0);
    a.pump();
    lose_history(&relay);
    relay.borrow_mut().restore_link(0);
    settle(&relay, &mut [&mut a]);
    assert!(a.le.take_history_lost(), "told the room lost its history");
    assert!(a.le.editable(), "holding a settled document, it does not wait: {:?}", a.le.status());
}

/// Body block `i`, a paragraph.
fn para(ed: &Ed, i: usize) -> &wordcraft_doc::Paragraph {
    ed.s.doc.body[i].as_para().unwrap()
}

/// Whether the char at byte `at` of paragraph `i` is bold.
fn bold_at(ed: &Ed, i: usize, at: usize) -> bool {
    let p = para(ed, i);
    let mut start = 0;
    for r in &p.runs {
        if at < start + r.len {
            return r.props.bold == Some(true);
        }
        start += r.len;
    }
    false
}

fn objects(d: &Document) -> usize {
    d.body.iter().filter_map(|b| b.as_para()).map(|p| p.objects.len()).sum()
}

fn select(ed: &mut Ed, block: usize, from: usize, to: usize) {
    ed.s.sel = Selection { anchor: Pos::body(block, from), focus: Pos::body(block, to) };
}

#[test]
fn cut_and_paste_are_shared() {
    let relay = relay(None);
    let bytes = file(&["Alpha beta gamma", "Second"]);
    let mut a = Ed::open(&relay, "alice", Access::Edit, &bytes);
    let mut b = Ed::open(&relay, "bob", Access::Edit, &bytes);
    settle(&relay, &mut [&mut a, &mut b]);
    assert!(a.le.allows("edit.cut") && a.le.allows("edit.paste") && a.le.allows("edit.pasteText"));
    assert!(!a.le.allows("edit.pasteSpecial"), "Paste Special stays off");
    select(&mut a, 0, 6, 11);
    assert!(a.run("edit.cut", json!({})), "cut runs live");
    a.end_of(1);
    assert!(a.run("edit.paste", json!({})), "paste runs live");
    assert!(a.le.take_notices().is_empty(), "nothing refused");
    settle(&relay, &mut [&mut a, &mut b]);
    assert_eq!(texts(&b.s.doc), ["Alpha gamma", "Secondbeta "], "the other editor sees the cut and the paste");
    assert_same(&a, &b, "after cut and paste");
    // Text of several lines makes paragraphs on every side.
    a.end_of(0);
    assert!(a.run("edit.pasteText", json!({"text": " one\ntwo"})));
    settle(&relay, &mut [&mut a, &mut b]);
    assert_eq!(texts(&b.s.doc), ["Alpha gamma one", "two", "Secondbeta "]);
    assert_same(&a, &b, "after a paste of two lines");
}

#[test]
fn a_formatted_paste_keeps_its_runs_on_every_side_and_in_the_save() {
    let relay = relay(None);
    let bytes = file(&["Hello world"]);
    let mut a = Ed::open(&relay, "alice", Access::Edit, &bytes);
    let mut b = Ed::open(&relay, "bob", Access::Edit, &bytes);
    let mut saver = Ed::open(&relay, SERVICE_UID, Access::Edit, &bytes);
    settle(&relay, &mut [&mut a, &mut b, &mut saver]);
    select(&mut a, 0, 0, 5);
    assert!(a.run("format.bold", json!({})));
    assert!(a.run("edit.copy", json!({})));
    a.end_of(0);
    assert!(a.run("edit.paste", json!({})));
    assert!(a.le.take_notices().is_empty(), "a paste of formatted text with the document's own styles is shared as it is");
    settle(&relay, &mut [&mut a, &mut b, &mut saver]);
    assert_same(&a, &b, "after the formatted paste");
    assert_eq!(texts(&b.s.doc), ["Hello worldHello"]);
    let end = para(&b, 0).text.len() - 1;
    assert!(bold_at(&b, 0, end) && !bold_at(&b, 0, 6), "the pasted text is bold on the other side too");

    // The document service saves it: the checkpoint's document, and the .docx it writes.
    let state = saver.le.checkpoint().unwrap();
    settle(&relay, &mut [&mut a, &mut b, &mut saver]);
    let saved = document_from_state(&saver.s.doc, &state).unwrap();
    assert!(saved.same_content(&a.s.doc), "the saved document holds the paste: {:?}", texts(&saved));
    let reopened = wordcraft_engine::io::open_bytes("plan.docx", &save_bytes("plan.docx", &saved).unwrap()).unwrap();
    let p = reopened.body[0].as_para().unwrap();
    assert_eq!(p.text, "Hello worldHello");
    let last = p.runs.last().unwrap();
    assert_eq!(last.props.bold, Some(true), "the file keeps the pasted formatting");
}

#[test]
fn concurrent_cut_and_paste_in_one_paragraph_converge() {
    let relay = relay(None);
    let bytes = file(&["The quick brown fox", "jumps"]);
    let mut a = Ed::open(&relay, "alice", Access::Edit, &bytes);
    let mut b = Ed::open(&relay, "bob", Access::Edit, &bytes);
    settle(&relay, &mut [&mut a, &mut b]);
    // Neither has seen the other's edits when they make their own.
    select(&mut a, 0, 4, 10);
    assert!(a.run("edit.cut", json!({})));
    a.caret(0, 0);
    assert!(a.run("edit.paste", json!({})));
    select(&mut b, 0, 16, 19);
    assert!(b.run("edit.cut", json!({})));
    b.caret(0, 4);
    assert!(b.run("edit.paste", json!({})));
    settle(&relay, &mut [&mut a, &mut b]);
    assert_same(&a, &b, "concurrent cut and paste");
    let all = texts(&a.s.doc).join("\n");
    assert_eq!(all.matches("quick").count(), 1, "{all:?}");
    assert_eq!(all.matches("fox").count(), 1, "{all:?}");
    assert!(all.contains("jumps"), "{all:?}");
    // A paste of several lines splits the paragraph while the other editor cuts from it: they
    // converge (the split carries its tail as it was, as Enter does).
    select(&mut a, 0, 0, 3);
    assert!(a.run("edit.cut", json!({})));
    b.caret(0, 1);
    assert!(b.run("edit.pasteText", json!({"text": "A\nB"})));
    settle(&relay, &mut [&mut a, &mut b]);
    assert_same(&a, &b, "a cut against a split");
}

fn clipboard_command(rng: &mut Rng) -> (&'static str, Value) {
    match rng.below(14) {
        0 | 1 => ("edit.cut", json!({})),
        2 => ("edit.copy", json!({})),
        3 | 4 => ("edit.paste", json!({})),
        5 => ("edit.pasteText", json!({"text": "p\nq"})),
        6 => ("edit.pasteMerge", json!({})),
        7 => ("text.insert", json!({"text": "ab"})),
        8 => ("format.bold", json!({})),
        // Another program's content: formatted lines, a table, a picture, a link.
        9 => ("edit.paste", json!({"html": "<p><b>h1</b> x</p><p>h2</p>", "text": "h1 x\nh2"})),
        10 => ("edit.paste", json!({"html": "<table><tr><td>t1</td><td>t2</td></tr></table>", "text": "t1\tt2"})),
        11 => ("edit.paste", json!({"html": format!("<p>i<img src=\"data:image/png;base64,{PNG}\" width=\"4\" height=\"4\">j</p>"), "text": "ij"})),
        12 => ("edit.paste", json!({"html": "<p>see <a href=\"https://example.com/\">here</a></p>", "text": "see here"})),
        _ => ("edit.undo", json!({})),
    }
}

#[test]
fn cut_copy_and_paste_converge_through_the_pump() {
    for seed in 200..248 {
        converge_with(2, seed, 120, clipboard_command);
    }
    for seed in 300..316 {
        converge_with(3, seed, 120, clipboard_command);
    }
}

#[test]
fn undo_of_a_paste_takes_back_only_the_paste() {
    let relay = relay(None);
    let bytes = file(&["One"]);
    let mut a = Ed::open(&relay, "alice", Access::Edit, &bytes);
    let mut b = Ed::open(&relay, "bob", Access::Edit, &bytes);
    settle(&relay, &mut [&mut a, &mut b]);
    a.end_of(0);
    assert!(a.run("edit.pasteText", json!({"text": " pasted\nmore"})));
    b.caret(0, 0);
    assert!(b.run("text.insert", json!({"text": "X"})));
    settle(&relay, &mut [&mut a, &mut b]);
    assert_eq!(texts(&a.s.doc), ["XOne pasted", "more"]);
    assert!(a.run("edit.undo", json!({})));
    settle(&relay, &mut [&mut a, &mut b]);
    assert_eq!(texts(&b.s.doc), ["XOne"], "the paste is gone, the other editor's typing stays");
    assert_same(&a, &b, "after undo");
    // Bob's undo takes back his typing, not anything of Alice's.
    assert!(b.run("edit.undo", json!({})));
    settle(&relay, &mut [&mut a, &mut b]);
    assert_eq!(texts(&a.s.doc), ["One"]);
    assert_same(&a, &b, "after both undo");
}

/// A 1×1 PNG.
const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";

#[test]
fn a_paste_with_a_picture_goes_in_as_text_on_every_side() {
    let relay = relay(None);
    let bytes = file(&["Start"]);
    let mut a = Ed::open(&relay, "alice", Access::Edit, &bytes);
    let mut b = Ed::open(&relay, "bob", Access::Edit, &bytes);
    settle(&relay, &mut [&mut a, &mut b]);
    let media = a.s.doc.media.len();
    a.end_of(0);
    let html = format!("<p>Hi <img src=\"data:image/png;base64,{PNG}\" width=\"10\" height=\"10\"> there</p>");
    assert!(a.run("edit.paste", json!({"html": html, "text": " Hi there"})), "the paste goes in");
    let said = a.le.take_notices();
    assert!(said.iter().any(|n| n.contains("Pasted as text")), "the user is told: {said:?}");
    assert!(!said.iter().any(|n| n.contains("link or bookmark")), "and not the refusal: {said:?}");
    settle(&relay, &mut [&mut a, &mut b]);
    assert_same(&a, &b, "the text-only paste");
    let got = texts(&b.s.doc).concat();
    assert!(got.starts_with("Start") && got.contains("Hi") && got.contains("there"), "the HTML's own text: {got:?}");
    assert_eq!(objects(&a.s.doc), 0, "no picture");
    assert_eq!(a.s.doc.media.len(), media, "and no picture data");

    // Without any text to fall back on, nothing changes and the user is told why.
    let before = a.s.doc.clone();
    let html = format!("<p><img src=\"data:image/png;base64,{PNG}\" width=\"10\" height=\"10\"></p>");
    let _ = a.run("edit.paste", json!({"html": html}));
    assert!(before.same_content(&a.s.doc), "nothing pasted");
    let said = a.le.take_notices();
    assert!(said.iter().any(|n| n.contains("paste")), "{said:?}");
    // One undo step for the text paste: Undo takes it back whole.
    assert!(a.run("edit.undo", json!({})));
    settle(&relay, &mut [&mut a, &mut b]);
    assert_eq!(texts(&b.s.doc), ["Start"]);
}

#[test]
fn a_paste_bringing_a_new_list_definition_goes_in_as_text() {
    // Another program's list brings its own list definition: the numbering is not shared.
    let relay = relay(None);
    let bytes = file(&["Start"]);
    let mut a = Ed::open(&relay, "alice", Access::Edit, &bytes);
    let mut b = Ed::open(&relay, "bob", Access::Edit, &bytes);
    settle(&relay, &mut [&mut a, &mut b]);
    let numbering = a.s.doc.numbering.clone();
    a.end_of(0);
    assert!(a.run("edit.paste", json!({"html": "<ul><li>one</li><li>two</li></ul>", "text": "one\ntwo"})));
    let said = a.le.take_notices();
    assert!(said.iter().any(|n| n.contains("Pasted as text")), "{said:?}");
    assert!(a.s.doc.numbering == numbering, "no list definition was added");
    settle(&relay, &mut [&mut a, &mut b]);
    assert_same(&a, &b, "the text-only paste");
    assert_eq!(texts(&b.s.doc), ["Startone", "two"]);
}

#[test]
fn a_cut_of_a_picture_is_put_back() {
    // A picture in the file, cut with its text: removing an inline object is not shared.
    let bytes = picture_file();
    let relay = relay(None);
    let mut a = Ed::open(&relay, "alice", Access::Edit, &bytes);
    let mut b = Ed::open(&relay, "bob", Access::Edit, &bytes);
    settle(&relay, &mut [&mut a, &mut b]);
    let before = a.s.doc.clone();
    let len = para(&a, 0).text.len();
    select(&mut a, 0, 0, len);
    let _ = a.run("edit.cut", json!({}));
    assert!(before.same_content(&a.s.doc), "put back");
    let said = a.le.take_notices();
    assert!(said.iter().any(|n| n.contains("cut")), "{said:?}");
    assert!(a.s.clipboard.is_some(), "the copy is still made");
    settle(&relay, &mut [&mut a, &mut b]);
    assert_same(&a, &b, "nothing was shared");
}

#[test]
fn a_pasted_table_reaches_every_side() {
    let relay = relay(None);
    let bytes = file(&["Start", "End"]);
    let mut a = Ed::open(&relay, "alice", Access::Edit, &bytes);
    let mut b = Ed::open(&relay, "bob", Access::Edit, &bytes);
    settle(&relay, &mut [&mut a, &mut b]);
    a.end_of(0);
    let html = "<table><tr><td>a1</td><td>b1</td></tr><tr><td>a2</td><td>b2</td></tr></table>";
    assert!(a.run("edit.paste", json!({"html": html, "text": "a1\tb1\na2\tb2"})));
    assert!(a.le.take_notices().is_empty(), "the table itself is shared");
    settle(&relay, &mut [&mut a, &mut b]);
    assert_same(&a, &b, "the pasted table");
    assert_eq!(b.s.doc.body.iter().filter(|b| b.as_table().is_some()).count(), 1, "the other editor has the table");
    assert!(b.s.doc.plain_text(wordcraft_doc::StoryRef::Body).contains("b2"));
}

/// A file whose first paragraph holds a picture, between "Before" and "x…y".
fn picture_file() -> Vec<u8> {
    let html = format!("<p>x<img src=\"data:image/png;base64,{PNG}\" width=\"10\" height=\"10\">y</p>");
    let mut s = Session::new(plain(&["Before", "After"]));
    s.sel = Selection { anchor: Pos::body(0, 6), focus: Pos::body(0, 6) };
    s.run("edit.paste", &json!({"html": html})).unwrap();
    assert!(objects(&s.doc) > 0, "the file has a picture");
    save_bytes("plan.docx", &s.doc).unwrap()
}

fn two(bytes: &[u8]) -> (Shared, Ed, Ed) {
    let relay = relay(None);
    let mut a = Ed::open(&relay, "alice", Access::Edit, bytes);
    let mut b = Ed::open(&relay, "bob", Access::Edit, bytes);
    settle(&relay, &mut [&mut a, &mut b]);
    (relay, a, b)
}

#[test]
fn a_paste_too_large_to_share_is_refused_and_editing_goes_on() {
    let (relay, mut a, mut b) = two(&file(&["Start"]));
    let before = a.s.doc.clone();
    let huge = "x".repeat(20 << 20);
    a.end_of(0);
    assert!(!a.run("edit.pasteText", json!({"text": huge})), "a 20 MB paste is refused");
    assert!(before.same_content(&a.s.doc), "and put back");
    let said = a.le.take_notices();
    assert!(said.iter().any(|n| n.contains("too large")), "{said:?}");
    // Through Paste too: its text is as large, so there is nothing to fall back on.
    assert!(!a.run("edit.paste", json!({"text": huge})));
    assert!(before.same_content(&a.s.doc));
    assert!(a.le.take_notices().iter().any(|n| n.contains("too large")));
    // So is a table of ten thousand rows, and its ten thousand lines of text.
    let rows: String = (0..10_000).map(|i| format!("<tr><td>r{i}</td><td>c</td></tr>")).collect();
    let lines: String = (0..10_000).map(|i| format!("r{i}\tc\n")).collect();
    assert!(!a.run("edit.paste", json!({"html": format!("<table>{rows}</table>"), "text": lines})));
    assert!(before.same_content(&a.s.doc));
    assert!(a.le.take_notices().iter().any(|n| n.contains("too large")));
    // The link is still live and editing goes on.
    assert!(a.le.editable() && matches!(a.le.status(), LiveStatus::Live { .. }), "{:?}", a.le.status());
    assert!(a.run("text.insert", json!({"text": "!"})));
    settle(&relay, &mut [&mut a, &mut b]);
    assert_eq!(texts(&b.s.doc), ["Start!"]);
    assert_same(&a, &b, "after the refused pastes");
}

#[test]
fn a_table_too_large_to_share_goes_in_as_text() {
    let (relay, mut a, mut b) = two(&file(&["Start"]));
    let cells: String = (0..10).map(|_| "<td>x</td>").collect();
    let rows: String = (0..600).map(|_| format!("<tr>{cells}</tr>")).collect();
    let text: String = (0..600).map(|_| "x\tx\tx\tx\tx\tx\tx\tx\tx\tx\n").collect();
    a.end_of(0);
    assert!(a.run("edit.paste", json!({"html": format!("<table>{rows}</table>"), "text": text})));
    assert!(a.le.take_notices().iter().any(|n| n.contains("Pasted as text")));
    settle(&relay, &mut [&mut a, &mut b]);
    assert_same(&a, &b, "the table's text");
    assert_eq!(b.s.doc.body.iter().filter(|b| b.as_table().is_some()).count(), 0, "no table");
}

#[test]
fn an_html_only_paste_falls_back_to_its_own_text() {
    // Our clipboard holds an older copy; the paste is another program's HTML with a picture.
    let (relay, mut a, mut b) = two(&file(&["Start"]));
    select(&mut a, 0, 0, 5);
    assert!(a.run("edit.copy", json!({})));
    a.end_of(0);
    let html = format!("<p>Fresh <img src=\"data:image/png;base64,{PNG}\" width=\"10\" height=\"10\">words</p>");
    assert!(a.run("edit.paste", json!({"html": html})));
    assert!(a.le.take_notices().iter().any(|n| n.contains("Pasted as text")));
    settle(&relay, &mut [&mut a, &mut b]);
    let got = texts(&b.s.doc).concat();
    assert!(got.contains("Fresh") && got.contains("words"), "the paste's own text: {got:?}");
    assert!(!got.contains("StartStart"), "not the old copy: {got:?}");
    assert_same(&a, &b, "after the fallback");
}

#[test]
fn a_clipboard_pane_item_with_a_picture_goes_in_as_text() {
    let (relay, mut a, mut b) = two(&picture_file());
    let len = para(&a, 0).text.len();
    select(&mut a, 0, 0, len);
    assert!(a.run("edit.copy", json!({})));
    a.end_of(1);
    assert!(a.run("edit.pasteClipboardItem", json!({"index": 0})));
    assert!(a.le.take_notices().iter().any(|n| n.contains("Pasted as text")));
    settle(&relay, &mut [&mut a, &mut b]);
    assert_same(&a, &b, "the item's text");
    let last = texts(&b.s.doc).last().cloned().unwrap();
    assert!(last.starts_with("After") && last.contains("Before") && last.contains("xy"), "{last:?}");
    assert_eq!(objects(&b.s.doc), objects(&a.s.doc), "no second picture");
}

#[test]
fn a_table_that_would_not_read_back_goes_in_as_text() {
    // A table whose column width is not a number: its shared form would not read back.
    let mut s = Session::new(plain(&["x"]));
    s.run("edit.paste", &json!({"html": "<table><tr><td>t1</td><td>t2</td></tr></table>"})).unwrap();
    let mut table = s.doc.body.iter().find_map(|b| b.as_table()).cloned().unwrap();
    table.grid = vec![f32::INFINITY; table.grid.len().max(1)];
    let (relay, mut a, mut b) = two(&file(&["Start"]));
    a.s.clipboard = Some(wordcraft_doc::edit::Fragment { blocks: vec![Block::Table(table)], ..Default::default() });
    a.end_of(0);
    assert!(a.run("edit.paste", json!({})));
    assert!(a.le.take_notices().iter().any(|n| n.contains("Pasted as text")));
    settle(&relay, &mut [&mut a, &mut b]);
    assert_same(&a, &b, "the table's text");
    assert!(texts(&b.s.doc).concat().contains("t1"));
}

#[test]
fn a_pasted_table_holding_a_bookmark_goes_in_as_text() {
    let mut s = Session::new(plain(&["x"]));
    s.run("edit.paste", &json!({"html": "<table><tr><td>site</td></tr></table>"})).unwrap();
    let mut table = s.doc.body.iter().find_map(|b| b.as_table()).cloned().unwrap();
    let cell = std::sync::Arc::make_mut(&mut table.rows[0].cells[0].blocks[0]);
    let Block::Para(p) = cell else { panic!("a paragraph") };
    p.insert_object(0, wordcraft_doc::InlineObject::BookmarkStart { name: "here".into() }, &Default::default()).unwrap();
    let (relay, mut a, mut b) = two(&file(&["Start"]));
    a.s.clipboard = Some(wordcraft_doc::edit::Fragment { blocks: vec![Block::Table(table)], ..Default::default() });
    a.end_of(0);
    assert!(a.run("edit.paste", json!({})));
    assert!(a.le.take_notices().iter().any(|n| n.contains("Pasted as text")), "objects inside a table are not shared");
    settle(&relay, &mut [&mut a, &mut b]);
    assert_same(&a, &b, "the table's text");
    assert_eq!(objects(&b.s.doc), 0);
}

#[test]
fn edits_made_before_a_paste_are_kept_when_it_goes_in_as_text() {
    let (relay, mut a, mut b) = two(&file(&["Start"]));
    a.end_of(0);
    // Typing that reached the session without a flush yet.
    a.s.run("text.insert", &json!({"text": " typed"})).unwrap();
    let html = format!("<p>p<img src=\"data:image/png;base64,{PNG}\" width=\"10\" height=\"10\">q</p>");
    assert!(a.run("edit.paste", json!({"html": html, "text": " pq"})));
    settle(&relay, &mut [&mut a, &mut b]);
    assert_eq!(texts(&b.s.doc), ["Start typedpq"], "the typing, then the HTML's text");
    assert_same(&a, &b, "both edits");
}

#[test]
fn a_paste_over_a_picture_says_it_would_replace_it() {
    let (_relay, mut a, _b) = two(&picture_file());
    let before = a.s.doc.clone();
    let len = para(&a, 0).text.len();
    select(&mut a, 0, 0, len);
    assert!(!a.run("edit.paste", json!({"text": "z"})));
    assert!(before.same_content(&a.s.doc));
    let said = a.le.take_notices();
    assert!(said.iter().any(|n| n.contains("replace")), "{said:?}");
    assert!(!said.iter().any(|n| n.contains("Pasted as text") || n.contains("holds")), "{said:?}");
}
