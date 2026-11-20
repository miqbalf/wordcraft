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
use wordcraft_live::{Access, LiveEditing, LiveStatus, SERVICE_UID, SaveEvent, document_from_state};

use super::{Rng, plain, random_off, texts};

const ROOM: &str = "doc:inst:42";

/// One connection as the fake relay sees it.
struct Conn {
    pid: String,
    display: String,
    may_edit: bool,
    service: bool,
    connected: bool,
    /// Refuse this peer's edits, as a relay that could not order them.
    refuse_edits: bool,
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
}

type Shared = Rc<RefCell<Relay>>;

fn relay(rng: Option<u64>) -> Shared {
    Rc::new(RefCell::new(Relay { seq: 0, saved: 0, log: Vec::new(), conns: Vec::new(), next_space: 1, ephemeral: Vec::new(), rng: rng.map(Rng) }))
}

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
        let replay: Vec<ServerMsg> = self.log.iter().filter(|m| op_seq(m) > self.saved).cloned().collect();
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
                    Some("read-only access: this ticket may only read")
                } else if is_snap(&params) && !self.conns[at].service {
                    Some("only the document service may checkpoint; ask the saver")
                } else if self.conns[at].refuse_edits {
                    Some("the relay could not order that edit")
                } else {
                    None
                };
                if let Some(detail) = refusal {
                    self.conns[at].inbox.push_back(ServerMsg::Error { doc: ROOM.into(), detail: detail.into(), local: Some(local) });
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
        let (id, params) = random_command(&mut rng);
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
