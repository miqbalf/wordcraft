//! Native clients converging through a real craft-collab relay. Ignored by default: they need a
//! relay and its secret, `CRAFT_TEST_RELAY` (for example `http://localhost:9444`) and
//! `CRAFT_TEST_SECRET`, and run with `--features native-hub -- --ignored relay`.
//! `relay_restart_resumes` also needs `CRAFT_TEST_RESTART_CMD`, a shell command that restarts
//! that relay (for example `docker restart t9-relay`). `five_clients_typing_remote_apply_p95`
//! is the phase 1 load check; point `CRAFT_TEST_RELAY` at the deployed relay as clients reach it
//! (for the dev stack, `http://<lan address>:8035/craft`, through Apache).

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use craft_collab::Hub;
use craft_collab::native::NativeHub;
use hmac::digest::KeyInit;
use hmac::{Hmac, Mac};
use serde_json::{Value, json};
use sha2::Sha256;
use wordcraft_doc::{Document, Pos};
use wordcraft_engine::io::save_bytes;
use wordcraft_engine::{Selection, Session};
use wordcraft_live::{LiveEditing, LiveStatus};

use super::{Rng, plain, texts};

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} is not set"))
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

/// A join ticket as the relay's signer makes it: `base64url(claims).base64url(HMAC-SHA256)`.
fn ticket(secret: &str, room: &str, uid: &str, pid: &str) -> String {
    let claims = json!({"rm": room, "uid": uid, "dsp": uid, "perm": 3, "exp": now() + 900, "pid": pid});
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap());
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(payload.as_bytes());
    format!("{payload}.{}", URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes()))
}

/// A room no earlier run has used, so the relay starts the document from the file.
fn fresh_room() -> String {
    format!("t9-{}", SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos())
}

struct Client {
    name: String,
    s: Session,
    le: LiveEditing<NativeHub>,
}

impl Client {
    async fn join(relay: &str, secret: &str, room: &str, uid: &str, file: &[u8]) -> Client {
        let token = ticket(secret, room, uid, &format!("{uid}-{}", now()));
        let hub = NativeHub::connect(relay, &token, None).await.unwrap();
        let mut s = Session::new(Document::new());
        let le = LiveEditing::start(hub, &format!("doc:{room}"), file, "plan.docx", &mut s).unwrap();
        Client { name: uid.to_owned(), s, le }
    }

    fn pump(&mut self) {
        self.le.pump(&mut self.s);
    }

    fn live(&self) -> bool {
        matches!(self.le.status(), LiveStatus::Live { .. }) && self.le.editable()
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

fn pump_all(cs: &mut [Client]) {
    for c in cs.iter_mut() {
        c.pump();
    }
}

fn state(cs: &[Client]) -> String {
    cs.iter()
        .map(|c| {
            let chars: usize = texts(&c.s.doc).iter().map(String::len).sum();
            format!(
                "{}: {:?}, hub connected {}, editable {}, {} paragraphs, {} bytes",
                c.name,
                c.le.status(),
                c.le.hub().connected(),
                c.le.editable(),
                c.s.doc.body.len(),
                chars
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Pump every client until `done` holds, or fail after `secs` with where each one stands.
async fn until(cs: &mut [Client], secs: u64, what: &str, done: impl Fn(&[Client]) -> bool) {
    let end = Instant::now() + Duration::from_secs(secs);
    loop {
        pump_all(cs);
        if done(cs) {
            return;
        }
        assert!(Instant::now() < end, "timed out waiting for {what}:\n{}", state(cs));
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn all_live(cs: &[Client]) -> bool {
    cs.iter().all(Client::live)
}

fn equal(cs: &[Client]) -> bool {
    cs.windows(2).all(|w| w[0].s.doc.same_content(&w[1].s.doc))
}

/// Everyone is Live and equal, and stays so for `quiet`: nothing is still on its way.
async fn converge(cs: &mut [Client], what: &str) {
    let quiet = Duration::from_millis(600);
    let mut since: Option<Instant> = None;
    let end = Instant::now() + Duration::from_secs(30);
    loop {
        pump_all(cs);
        if all_live(cs) && equal(cs) {
            let t = *since.get_or_insert_with(Instant::now);
            if t.elapsed() >= quiet {
                return;
            }
        } else {
            since = None;
        }
        assert!(Instant::now() < end, "{what}: no convergence:\n{}", state(cs));
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn setup() -> (String, String, String, Vec<u8>) {
    let file = save_bytes("plan.docx", &plain(&["Alpha beta", "Gamma", "Delta epsilon zeta", ""])).unwrap();
    (env("CRAFT_TEST_RELAY"), env("CRAFT_TEST_SECRET"), fresh_room(), file)
}

/// One random edit by a client, in a random paragraph.
fn type_somewhere(c: &mut Client, rng: &mut Rng) {
    const WORDS: [&str; 5] = ["a", "b ", "cd", "é", "xyz "];
    let paras = c.s.doc.body.len();
    let i = rng.below(paras);
    let len = c.s.doc.body[i].as_para().map_or(0, |p| p.text.len());
    // A character boundary: ASCII and `é` only in the file, so walk to one.
    let text = c.s.doc.body[i].as_para().map(|p| p.text.clone()).unwrap_or_default();
    let mut at = if len == 0 { 0 } else { rng.below(len + 1) };
    while !text.is_char_boundary(at) {
        at += 1;
    }
    c.caret(i, at);
    match rng.below(6) {
        0 => {
            c.run("text.delete", json!({}));
        }
        1 => {
            c.run("text.newParagraph", json!({}));
        }
        _ => {
            c.run("text.insert", json!({"text": WORDS[rng.below(WORDS.len())]}));
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs relay"]
async fn three_native_clients_converge_through_relay() {
    let (relay, secret, room, file) = setup();
    let mut cs = vec![
        Client::join(&relay, &secret, &room, "alice", &file).await,
        Client::join(&relay, &secret, &room, "bob", &file).await,
        Client::join(&relay, &secret, &room, "carol", &file).await,
    ];
    until(&mut cs, 20, "every client Live", all_live).await;

    // Concurrent random edits, a few pumps between them so some overlap and some do not.
    let mut rng = Rng(7);
    for step in 0..150 {
        let k = rng.below(cs.len());
        type_somewhere(&mut cs[k], &mut rng);
        if step % 3 == 0 {
            pump_all(&mut cs);
        }
        if step % 10 == 0 {
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
    }
    converge(&mut cs, "after the random edits").await;
    let first = texts(&cs[0].s.doc);
    assert!(first.iter().any(|t| !t.is_empty()), "the edits landed: {first:?}");
    for c in &cs {
        assert!(c.le.editable(), "{} is still editable", c.name);
    }
    let bytes = |c: &Client| save_bytes("x.docx", &c.s.doc).unwrap();
    assert!(bytes(&cs[0]) == bytes(&cs[1]) && bytes(&cs[1]) == bytes(&cs[2]), "all three save the same file");

    // A late joiner reaches the same document from the relay's catch-up.
    let late = Client::join(&relay, &secret, &room, "dave", &file).await;
    cs.push(late);
    until(&mut cs, 20, "the late joiner Live", all_live).await;
    converge(&mut cs, "with the late joiner").await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs relay and CRAFT_TEST_RESTART_CMD"]
async fn relay_restart_resumes() {
    let (relay, secret, room, file) = setup();
    let restart = env("CRAFT_TEST_RESTART_CMD");
    let mut cs = vec![
        Client::join(&relay, &secret, &room, "alice", &file).await,
        Client::join(&relay, &secret, &room, "bob", &file).await,
        Client::join(&relay, &secret, &room, "carol", &file).await,
    ];
    until(&mut cs, 20, "every client Live", all_live).await;
    let mut rng = Rng(11);
    for _ in 0..30 {
        let k = rng.below(cs.len());
        type_somewhere(&mut cs[k], &mut rng);
        pump_all(&mut cs);
    }
    converge(&mut cs, "before the restart").await;

    // Restart the relay while clients keep editing; edits made while not live are refused.
    let child = std::thread::spawn(move || std::process::Command::new("sh").arg("-c").arg(restart).output());
    let mut typed = 0;
    let mut held_back = 0;
    while !child.is_finished() {
        for c in cs.iter_mut() {
            let live = c.live();
            type_somewhere(c, &mut rng);
            if live { typed += 1 } else { held_back += 1 }
        }
        pump_all(&mut cs);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let out = child.join().unwrap().unwrap();
    assert!(out.status.success(), "the restart command failed");
    eprintln!("during the restart: {typed} edit attempts while live, {held_back} while not live");
    eprintln!("after the restart:\n{}", state(&cs));

    for c in &cs {
        assert!(!matches!(c.le.status(), LiveStatus::Broken(_)), "{} is Broken after the restart: {:?}", c.name, c.le.status());
    }
    until(&mut cs, 60, "every client Live again", all_live).await;
    converge(&mut cs, "after the restart").await;

    // Editing carries on, in both directions.
    for (k, c) in cs.iter_mut().enumerate() {
        c.end_of(1);
        assert!(c.run("text.insert", json!({"text": format!("[{k}]")})), "client {k} can edit again");
    }
    converge(&mut cs, "after editing again").await;
    let joined = texts(&cs[0].s.doc).join("|");
    for k in 0..3 {
        assert!(joined.contains(&format!("[{k}]")), "{joined}");
    }
}

/// The spec target is a 95th percentile under 200 ms. Printed, not asserted: AGENTS.md rule 28.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs relay"]
async fn op_round_trip_p95() {
    let (relay, secret, room, file) = setup();
    let mut cs = vec![Client::join(&relay, &secret, &room, "alice", &file).await, Client::join(&relay, &secret, &room, "bob", &file).await];
    until(&mut cs, 20, "both Live", all_live).await;
    let mut ms = Vec::new();
    for n in 0..300 {
        let want = cs[0].s.doc.body[0].as_para().unwrap().text.len() + 1;
        cs[0].end_of(0);
        let t = Instant::now();
        assert!(cs[0].run("text.insert", json!({"text": "x"})), "sample {n}: the edit ran");
        let end = t + Duration::from_secs(5);
        loop {
            cs[0].pump();
            cs[1].pump();
            if cs[1].s.doc.body[0].as_para().unwrap().text.len() >= want {
                break;
            }
            assert!(Instant::now() < end, "sample {n}: the other client never saw it:\n{}", state(&cs));
            tokio::time::sleep(Duration::from_micros(500)).await;
        }
        ms.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    converge(&mut cs, "after the samples").await;
    ms.sort_by(f64::total_cmp);
    let at = |p: f64| ms[((ms.len() as f64 * p).ceil() as usize).saturating_sub(1).min(ms.len() - 1)];
    eprintln!(
        "round trip over {} samples: min {:.1} ms, p50 {:.1} ms, p95 {:.1} ms, p99 {:.1} ms, max {:.1} ms",
        ms.len(),
        ms[0],
        at(0.5),
        at(0.95),
        at(0.99),
        ms[ms.len() - 1]
    );
}

/// The phase 1 load check: five native clients each typing 8 characters a second for 60 s
/// (`CRAFT_TEST_LOAD_SECS` changes the length), every one into its own paragraph. Each client
/// times every other client's keystrokes, from just before the typist's `run` until the
/// keystroke is in its own document (remote apply). The spec target is a 95th percentile under
/// 200 ms on the LAN. Printed, not asserted: AGENTS.md rule 28.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs relay"]
async fn five_clients_typing_remote_apply_p95() {
    const N: usize = 5;
    const PERIOD: Duration = Duration::from_millis(125);
    let secs: u64 = std::env::var("CRAFT_TEST_LOAD_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(60);
    let (relay, secret, room) = (env("CRAFT_TEST_RELAY"), env("CRAFT_TEST_SECRET"), fresh_room());
    let file = save_bytes("plan.docx", &plain(&[""; N])).unwrap();
    let mut cs = Vec::new();
    for k in 0..N {
        cs.push(Client::join(&relay, &secret, &room, &format!("typist{k}"), &file).await);
    }
    until(&mut cs, 30, "every client Live", all_live).await;

    let keystrokes = usize::try_from(secs * 8).unwrap();
    let para_len = |c: &Client, k: usize| c.s.doc.body[k].as_para().unwrap().text.len();
    // sent[k][i]: when typist k started its keystroke i (its paragraph then holds i + 1 chars).
    let mut sent: Vec<Vec<Instant>> = (0..N).map(|_| Vec::with_capacity(keystrokes)).collect();
    // seen[j][k]: how many of typist k's keystrokes client j has been timed on.
    let mut seen = [[0usize; N]; N];
    let mut ms = Vec::with_capacity(N * (N - 1) * keystrokes);
    let mut not_live_runs = 0usize;
    let mut slowest_loop = Duration::ZERO;
    let start = Instant::now() + Duration::from_millis(200);
    let give_up = start + Duration::from_secs(secs + 30);
    loop {
        let lap = Instant::now();
        for k in 0..N {
            // Typists spread over the period, so not all five type in the same instant.
            let due = start + PERIOD * u32::try_from(sent[k].len()).unwrap() + Duration::from_millis(25 * k as u64);
            if sent[k].len() < keystrokes && Instant::now() >= due {
                if !cs[k].live() {
                    not_live_runs += 1;
                }
                cs[k].end_of(k);
                let t = Instant::now();
                assert!(cs[k].run("text.insert", json!({"text": "x"})), "typist {k}, keystroke {}: the edit ran:\n{}", sent[k].len(), state(&cs));
                sent[k].push(t);
            }
        }
        pump_all(&mut cs);
        let now = Instant::now();
        for j in 0..N {
            for k in (0..N).filter(|&k| k != j) {
                let have = para_len(&cs[j], k).min(sent[k].len());
                while seen[j][k] < have {
                    ms.push((now - sent[k][seen[j][k]]).as_secs_f64() * 1000.0);
                    seen[j][k] += 1;
                }
            }
        }
        slowest_loop = slowest_loop.max(lap.elapsed());
        let typed = sent.iter().all(|s| s.len() == keystrokes);
        if typed && (0..N).all(|j| (0..N).all(|k| j == k || seen[j][k] == keystrokes)) {
            break;
        }
        assert!(Instant::now() < give_up, "not every keystroke arrived:\n{}", state(&cs));
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    let typing = sent.iter().map(|s| s[s.len() - 1] - s[0]).max().unwrap();
    converge(&mut cs, "after the load").await;
    for (k, c) in cs.iter().enumerate() {
        assert!(c.le.editable(), "typist {k} is still editable");
        assert_eq!(texts(&c.s.doc), vec!["x".repeat(keystrokes); N], "typist {k} holds every keystroke once");
    }

    ms.sort_by(f64::total_cmp);
    let at = |p: f64| ms[((ms.len() as f64 * p).ceil() as usize).saturating_sub(1).min(ms.len() - 1)];
    eprintln!(
        "load: {N} clients x {keystrokes} keystrokes over {:.1} s via {relay}; {} remote applies: min {:.1} ms, p50 {:.1} ms, p95 {:.1} ms, p99 {:.1} ms, max {:.1} ms; runs while not live {not_live_runs}; slowest pump lap {:.1} ms",
        typing.as_secs_f64(),
        ms.len(),
        ms[0],
        at(0.5),
        at(0.95),
        at(0.99),
        ms[ms.len() - 1],
        slowest_loop.as_secs_f64() * 1000.0
    );
}
