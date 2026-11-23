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
/// quarter of the commands go straight to the session, so the pump's flush picks them up. The
/// safety net is off, so the mapping alone must keep every window equal to its own replica (what
/// the saver writes) and to the others.
fn converge_through_pump(n: usize, seed: u64, steps: usize) {
    let relay = relay(Some(seed ^ 0x5eed));
    let bytes = file(&["Alpha beta", "Gamma", "Delta epsilon zeta", ""]);
    let mut eds: Vec<Ed> = (0..n).map(|k| Ed::open(&relay, &format!("u{k}"), Access::Edit, &bytes)).collect();
    for e in &mut eds {
        e.le.set_verify_every(0);
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
    for (k, e) in eds.iter().enumerate() {
        assert_eq!(e.le.drift(&e.s), None, "seed {seed}, {n} editors: editor {k} shows what its replica holds");
        assert_eq!(e.le.repairs(), 0, "seed {seed}: the safety net never ran");
    }
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

/// Seeds that once left the editors apart for good: a swap of two equal paragraphs that was
/// never shared (149), an id whose block an undo took away (572, 917) and the rest found by
/// the sweep below.
#[test]
fn editors_converge_on_seeds_that_diverged() {
    for seed in [149, 488, 572, 820, 917, 1950] {
        converge_through_pump(3, seed, 150);
    }
    for seed in [187, 250, 543, 555, 715, 717, 770] {
        converge_through_pump(2, seed, 150);
    }
}

/// A long sweep of [`converge_through_pump`], for a run by hand: `LIVE_SWEEP=from..to` (and
/// `LIVE_SWEEP_EDITORS`, 3 by default). Without it the test checks nothing.
#[test]
fn converge_sweep_from_the_environment() {
    let Some((from, to)) = std::env::var("LIVE_SWEEP").ok().and_then(|r| {
        let (a, b) = r.split_once("..")?;
        Some((a.parse::<u64>().ok()?, b.parse::<u64>().ok()?))
    }) else {
        return;
    };
    let n = std::env::var("LIVE_SWEEP_EDITORS").ok().and_then(|n| n.parse().ok()).unwrap_or(3);
    let mut failed = Vec::new();
    for seed in from..to {
        if std::panic::catch_unwind(|| converge_through_pump(n, seed, 150)).is_err() {
            failed.push(seed);
        }
    }
    assert!(failed.is_empty(), "{n} editors, seeds {from}..{to}: {} failed: {failed:?}", failed.len());
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

/// Select bytes `from..to` of body block `block`.
fn select_range(ed: &mut Ed, block: usize, from: usize, to: usize) {
    ed.s.sel = Selection { anchor: Pos::body(block, from), focus: Pos::body(block, to) };
}

/// Whether every char of `text` in body paragraph `i` of `ed` is bold.
fn all_bold(ed: &Ed, i: usize, text: &str) -> bool {
    let p = ed.s.doc.body[i].as_para().unwrap();
    let Some(from) = p.text.find(text) else { return false };
    let mut start = 0;
    p.runs.iter().all(|r| {
        let (lo, hi) = (start, start + r.len);
        start = hi;
        hi <= from || lo >= from + text.len() || r.props.bold == Some(true)
    })
}

/// Enter splits a paragraph while another editor edits the part that moves to the new
/// paragraph: their delete, formatting and typing land in the new paragraph, as if made after.
/// (A paragraph that starts in lower case is avoided: Enter's sentence capital changes the head
/// in the same edit, which is then not seen as a split.)
#[test]
fn an_edit_in_the_tail_of_a_split_lands_in_the_new_paragraph() {
    let relay = relay(None);
    let bytes = file(&["Alpha beta gamma delta", "End of the line"]);
    let mut a = Ed::open(&relay, "alice", Access::Edit, &bytes);
    let mut b = Ed::open(&relay, "bob", Access::Edit, &bytes);
    settle(&relay, &mut [&mut a, &mut b]);

    // Neither has seen the other's edit when making their own.
    a.caret(0, 5);
    assert!(a.run("text.newParagraph", json!({})));
    select_range(&mut b, 0, 11, 17);
    assert!(b.run("text.delete", json!({})), "bob deletes \"gamma \"");
    settle(&relay, &mut [&mut a, &mut b]);
    assert_same(&a, &b, "a delete against a split");
    assert_eq!(texts(&a.s.doc), ["Alpha", " beta delta", "End of the line"], "the delete wins");

    a.caret(1, 1);
    assert!(a.run("text.newParagraph", json!({})));
    select_range(&mut b, 1, 1, 5);
    assert!(b.run("format.bold", json!({})), "bob bolds \"beta\"");
    b.end_of(1);
    assert!(b.run("text.insert", json!({"text": "!"})), "bob types at the end");
    settle(&relay, &mut [&mut a, &mut b]);
    assert_same(&a, &b, "formatting and typing against a split");
    assert_eq!(texts(&a.s.doc), ["Alpha", " ", "beta delta!", "End of the line"], "the typing lands where it was typed");
    assert!(all_bold(&a, 2, "beta") && all_bold(&b, 2, "beta"), "the formatting is kept");

    // The new paragraph's own typing since the split is followed.
    a.caret(3, 3);
    assert!(a.run("text.insert", json!({"text": "X"})));
    settle(&relay, &mut [&mut a, &mut b]);
    a.caret(3, 2);
    assert!(a.run("text.newParagraph", json!({})));
    a.caret(4, 0);
    assert!(a.run("text.insert", json!({"text": "Y"})));
    select_range(&mut b, 3, 8, 16);
    assert!(b.run("text.delete", json!({})), "bob deletes \"the line\"");
    settle(&relay, &mut [&mut a, &mut b]);
    assert_same(&a, &b, "a delete against a split and typing");
    assert_eq!(texts(&a.s.doc), ["Alpha", " ", "beta delta!", "En", "YdX of "]);
}

/// The browser e2e's steps: Enter mid-paragraph while the other editor types at its end, then
/// Enter at the start of that tail while the other editor goes on typing there. The typing
/// stays at the end of the tail.
#[test]
fn typing_at_the_end_follows_two_splits() {
    let relay = relay(None);
    let bytes = file(&["T7 test report", "This is a small test document for the share proof."]);
    let mut a = Ed::open(&relay, "alice", Access::Edit, &bytes);
    let mut b = Ed::open(&relay, "bob", Access::Edit, &bytes);
    settle(&relay, &mut [&mut a, &mut b]);

    a.caret(1, 10);
    assert!(a.run("text.newParagraph", json!({})));
    b.end_of(1);
    assert!(b.run("text.insert", json!({"text": " TAILONE"})));
    // Bob gets the split before Alice forwards his typing into the tail.
    b.pump();
    settle(&relay, &mut [&mut a, &mut b]);
    assert_eq!(texts(&b.s.doc), ["T7 test report", "This is a ", "small test document for the share proof. TAILONE"]);

    a.caret(2, 0);
    assert!(a.run("text.newParagraph", json!({})));
    assert!(b.run("text.insert", json!({"text": " T"})));
    settle(&relay, &mut [&mut a, &mut b]);
    assert!(b.run("text.insert", json!({"text": "AILTWO"})));
    settle(&relay, &mut [&mut a, &mut b]);
    assert_same(&a, &b, "two splits and typing");
    assert_eq!(texts(&a.s.doc), ["T7 test report", "This is a ", "", "small test document for the share proof. TAILONE TAILTWO"]);
}

/// Two separate edits racing a split, one in the head and one in the tail, reach the splitter
/// together; each lands in its own part, and the split stays where it was made.
#[test]
fn two_edits_racing_a_split_land_in_their_own_parts() {
    let relay = relay(None);
    let bytes = file(&["Alpha beta", "End"]);
    let mut a = Ed::open(&relay, "alice", Access::Edit, &bytes);
    let mut b = Ed::open(&relay, "bob", Access::Edit, &bytes);
    settle(&relay, &mut [&mut a, &mut b]);

    a.caret(0, 5);
    assert!(a.run("text.newParagraph", json!({})));
    select_range(&mut b, 0, 1, 2);
    assert!(b.run("text.delete", json!({})), "bob deletes the \"l\"");
    b.end_of(0);
    assert!(b.run("text.insert", json!({"text": "!"})), "bob types at the end");
    settle(&relay, &mut [&mut a, &mut b]);
    assert_same(&a, &b, "two edits against a split");
    assert_eq!(texts(&a.s.doc), ["Apha", " beta!", "End"]);
}

/// A paragraph's own formatting set while another editor splits it reaches both its head and
/// its tail.
#[test]
fn centring_a_paragraph_while_it_is_split_centres_both_parts() {
    let relay = relay(None);
    let bytes = file(&["Alpha beta gamma delta", "End of the line"]);
    let mut a = Ed::open(&relay, "alice", Access::Edit, &bytes);
    let mut b = Ed::open(&relay, "bob", Access::Edit, &bytes);
    settle(&relay, &mut [&mut a, &mut b]);

    a.caret(0, 5);
    assert!(a.run("text.newParagraph", json!({})));
    b.caret(0, 2);
    assert!(b.run("para.alignCenter", json!({})), "bob centres the paragraph");
    settle(&relay, &mut [&mut a, &mut b]);
    assert_same(&a, &b, "centring against a split");
    assert_eq!(texts(&a.s.doc), ["Alpha", " beta gamma delta", "End of the line"]);
    for e in [&a, &b] {
        let align: Vec<_> = e.s.doc.body.iter().filter_map(|b| b.as_para()).map(|p| p.props.align).collect();
        assert_eq!(align[..2], [Some(wordcraft_doc::Align::Center); 2], "both parts are centred");
        assert_ne!(align[2], Some(wordcraft_doc::Align::Center), "the next paragraph is not");
    }
}

/// The safety net: a window whose body drifted from the shared copy (here, by an edit made
/// behind the mapping's back) is found out on the periodic check and rebuilt from the shared
/// copy; a window that did not drift is left alone.
#[test]
fn a_drifted_window_is_rebuilt_from_the_shared_copy() {
    let relay = relay(None);
    let bytes = file(&["One", "Two"]);
    let mut a = Ed::open(&relay, "alice", Access::Edit, &bytes);
    let mut b = Ed::open(&relay, "bob", Access::Edit, &bytes);
    settle(&relay, &mut [&mut a, &mut b]);
    a.end_of(0);
    assert!(a.run("text.insert", json!({"text": "!"})));
    settle(&relay, &mut [&mut a, &mut b]);
    assert_eq!(a.le.drift(&a.s), None, "in step");
    assert!(!a.le.verify(&mut a.s), "nothing to repair");

    // Behind the mapping's back: an extra paragraph, and other text in a shared one.
    a.s.apply_remote(|d| {
        d.body.push(wordcraft_doc::para_block(wordcraft_doc::Paragraph::new()));
        if let Some(wordcraft_doc::Block::Para(p)) = d.body.get_mut(1).map(std::sync::Arc::make_mut) {
            p.text = "Changed".into();
            p.runs.clear();
        }
    })
    .unwrap();
    assert!(a.le.drift(&a.s).is_some(), "the drift is seen");
    for _ in 0..wordcraft_live::session::VERIFY_EVERY {
        a.pump();
    }
    assert_eq!(a.le.drift(&a.s), None, "the periodic check rebuilt the body");
    assert_eq!(a.le.repairs(), 1, "and counted it");
    assert_same(&a, &b, "the window shows the shared document again");
    assert_eq!(texts(&a.s.doc), ["One!", "Two"]);

    // A due check waits while the window is being typed in, and runs at the first pause.
    a.le.set_verify_every(1);
    a.s.apply_remote(|d| {
        if let Some(wordcraft_doc::Block::Para(p)) = d.body.get_mut(1).map(std::sync::Arc::make_mut) {
            p.text = "Behind".into();
            p.runs.clear();
        }
    })
    .unwrap();
    for _ in 0..2 * wordcraft_live::session::VERIFY_QUIET {
        a.end_of(0);
        assert!(a.run("text.insert", json!({"text": "x"})));
        a.pump();
    }
    assert_eq!(a.le.repairs(), 1, "not while typing");
    for _ in 0..=wordcraft_live::session::VERIFY_QUIET {
        a.pump();
    }
    assert_eq!(a.le.repairs(), 2, "at the pause");
    settle(&relay, &mut [&mut a, &mut b]);
    assert_same(&a, &b, "rebuilt again");
    assert_eq!(texts(&a.s.doc)[1], "Two");
}
