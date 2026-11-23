//! The mapping between a WordCraft document and the shared block document: seeding, local
//! edits going out, remote edits coming in, and two real sessions converging.
use std::sync::{Arc, Mutex};

use craft_collab::{BlockChange, LiveDoc};
use serde_json::{Value, json};
use wordcraft_doc::{Block, Document, Paragraph, Pos, para_block};
use wordcraft_engine::io::{open_bytes, save_bytes};
use wordcraft_engine::{EditObserver, Selection, Session};
use wordcraft_live::{LiveMap, LocalEdit, MapError, RECOVERED_STYLE, RECOVERED_STYLE_ID, Refusal, assign_seed_pids, is_shareable, seed_blocks};

mod pump;
#[cfg(feature = "native-hub")]
mod relay;

/// The dev stack's seed files (craft-office-nextcloud `dev/seed/*.md`, our own text), which it
/// converts to `.docx` with `wordcraft-cli convert`.
const SEEDS: [(&str, &str); 4] = [
    ("Notes", "# Notes\n\nScratch notes for editing tests.\n"),
    ("Plan", "# Project Plan\n\nThis is the shared plan.\n\n- Item one\n- Item two\n"),
    ("ReadOnly", "# Read Only\n\nBob can read this but not edit.\n"),
    ("Spec", "# Spec\n\nA spec document with **bold** and *italic* text.\n"),
];

/// A seed file as the dev stack uploads it: Markdown converted to `.docx`, then opened.
fn seed_docx(md: &str) -> Document {
    let doc = open_bytes("seed.md", md.as_bytes()).unwrap();
    let bytes = save_bytes("seed.docx", &doc).unwrap();
    open_bytes("seed.docx", &bytes).unwrap()
}

/// A document of plain paragraphs, as `open` would leave it before sharing.
fn plain(paras: &[&str]) -> Document {
    let mut doc = Document::new();
    doc.body = paras.iter().map(|t| para_block(Paragraph::with_text(t, Default::default()))).collect();
    doc
}

fn texts(doc: &Document) -> Vec<String> {
    doc.body.iter().filter_map(|b| b.as_para()).map(|p| p.text.clone()).collect()
}

/// Every `edited` call, for the peer to turn into live updates after the command returns.
type Edits = Arc<Mutex<Vec<(Document, Document, String)>>>;

struct Watch(Edits);

impl EditObserver for Watch {
    fn edited(&mut self, before: &Document, after: &Document, command: &str) {
        self.0.lock().unwrap().push((before.clone(), after.clone(), command.to_owned()));
    }
}

/// One participant: an engine session, its replica of the shared document and the map between
/// them, wired the way the app wires them.
struct Peer {
    s: Session,
    live: LiveDoc,
    map: LiveMap,
    edits: Edits,
    /// Commands refused as not shareable, and put back.
    refused: usize,
}

impl Peer {
    /// Join a document every peer opened from the same file. `space` is the relay's grant.
    fn join(file: &Document, space: u32) -> Peer {
        let mut s = Session::new(file.clone());
        s.assign_pids(assign_seed_pids).unwrap();
        let mut live = LiveDoc::seeded(&seed_blocks(&s.doc)).unwrap();
        live.set_client(space).unwrap();
        let map = LiveMap::new(&s.doc, space);
        let edits = Edits::default();
        s.set_observer(Some(Box::new(Watch(edits.clone()))));
        Peer { s, live, map, edits, refused: 0 }
    }

    /// Run a command and return the updates it sends. A refused command sends nothing.
    fn run(&mut self, id: &str, params: Value) -> Vec<Vec<u8>> {
        let _ = self.s.run(id, &params);
        let seen: Vec<_> = std::mem::take(&mut *self.edits.lock().unwrap());
        let mut out = Vec::new();
        for (before, after, command) in seen {
            match self.map.local_updates(&mut self.live, &before, &after, &command) {
                Ok(edit) => out.extend(self.sent(edit)),
                // Refused (not shared, or no space yet): the app puts the document back.
                Err(e) if e.refused().is_some() => {
                    assert_eq!(e.refused(), Some(command.as_str()));
                    self.s.apply_remote(|d| *d = before.clone()).unwrap();
                    self.refused += 1;
                }
                Err(e) => panic!("{id}: {e}"),
            }
        }
        out
    }

    fn sent(&mut self, edit: LocalEdit) -> Vec<Vec<u8>> {
        assert!(edit.failed.is_empty(), "{:?}", edit.failed);
        wordcraft_live::share_pids(&mut self.s, &edit).unwrap();
        edit.updates
    }

    /// Take one update from a peer; returns what this peer sends in answer (recovered text).
    fn receive(&mut self, update: &[u8]) -> Vec<Vec<u8>> {
        let remote = self.map.receive(&mut self.live, update).unwrap();
        let (map, live) = (&mut self.map, &self.live);
        self.s.apply_remote(|d| map.apply_changes(d, live, &remote.changes)).unwrap();
        remote.updates
    }

    fn select(&mut self, block: usize, from: usize, to: usize) {
        self.s.sel = Selection { anchor: Pos::body(block, from), focus: Pos::body(block, to) };
    }

    fn caret(&mut self, block: usize, off: usize) {
        self.select(block, off, off);
    }
}

/// Deliver every update in `out` to `to`, and whatever it answers back to `from`, until quiet.
fn exchange(a: &mut Peer, b: &mut Peer, mut to_b: Vec<Vec<u8>>, mut to_a: Vec<Vec<u8>>) {
    while !to_a.is_empty() || !to_b.is_empty() {
        for u in std::mem::take(&mut to_b) {
            to_a.extend(b.receive(&u));
        }
        for u in std::mem::take(&mut to_a) {
            to_b.extend(a.receive(&u));
        }
    }
}

fn assert_converged(a: &Peer, b: &Peer, what: &str) {
    assert!(a.s.doc.same_content(&b.s.doc), "{what}: {:?} vs {:?}", texts(&a.s.doc), texts(&b.s.doc));
    let pids = |d: &Document| d.body.iter().filter_map(|b| b.as_para()).map(|p| p.pid).collect::<Vec<_>>();
    assert_eq!(pids(&a.s.doc), pids(&b.s.doc), "{what}: the same paragraphs on both sides");
}

#[test]
fn seed_then_rebuild_is_identity() {
    let mut files: Vec<(String, Document)> = SEEDS.iter().map(|(n, md)| ((*n).to_string(), seed_docx(md))).collect();
    // The sample document adds a table, a list and inline objects.
    files.push(("sample".into(), wordcraft_engine::sample::sample_document()));
    for (name, mut doc) in files {
        assign_seed_pids(&mut doc);
        let live = LiveDoc::seeded(&seed_blocks(&doc)).unwrap();
        let mut rebuilt = doc.clone();
        rebuilt.body.clear();
        let mut map = LiveMap::new(&rebuilt, 1);
        let changes: Vec<BlockChange> = live.order().into_iter().enumerate().map(|(at, id)| BlockChange::Inserted { at, id }).collect();
        map.apply_changes(&mut rebuilt, &live, &changes);
        assert!(rebuilt.same_content(&doc), "{name}: {:?} vs {:?}", texts(&rebuilt), texts(&doc));
        let pids = |d: &Document| d.body.iter().filter_map(|b| b.as_para()).map(|p| p.pid).collect::<Vec<_>>();
        assert_eq!(pids(&rebuilt), pids(&doc), "{name}: paragraphs keep their ids");
        assert!(pids(&doc).iter().all(|p| *p != 0), "{name}: every body paragraph is shared");
    }
}

#[test]
fn typing_bold_in_middle_round_trips() {
    let file = plain(&["Hello brave world", "Second"]);
    let (mut a, mut b) = (Peer::join(&file, 1), Peer::join(&file, 2));
    a.select(0, 6, 11);
    let out = a.run("format.bold", json!({}));
    assert!(!out.is_empty(), "bold sends an update");
    a.caret(0, 8);
    let mut more = a.run("text.insert", json!({"text": "é😀"}));
    more.splice(0..0, out);
    exchange(&mut a, &mut b, more, Vec::new());
    assert_converged(&a, &b, "bold then typing");
    let p = b.s.doc.body[0].as_para().unwrap();
    assert_eq!(p.text, "Hello bré😀ave world", "the typing landed where it was typed");
    let bold: Vec<bool> = p.runs.iter().map(|r| r.props.bold == Some(true)).collect();
    assert!(bold.len() >= 3 && bold[0] != bold[1], "the middle is bold on the other side: {:?}", p.runs);
    assert_eq!(b.s.doc.body[1].as_para().unwrap().text, "Second", "the other paragraph is untouched");
}

#[test]
fn split_and_merge_round_trip() {
    let file = plain(&["Hello world", "Next"]);
    let (mut a, mut b) = (Peer::join(&file, 1), Peer::join(&file, 2));
    a.caret(0, 5);
    let out = a.run("text.newParagraph", json!({}));
    exchange(&mut a, &mut b, out, Vec::new());
    assert_converged(&a, &b, "split");
    assert_eq!(texts(&b.s.doc), ["Hello", " world", "Next"]);
    let tail = b.s.doc.body[1].as_para().unwrap().pid;
    assert!(tail != 0 && tail >> 32 == 1, "the new paragraph got an id in the splitter's space: {tail:#x}");

    // Merge it back from the other side: Delete at the end of the first paragraph.
    b.caret(0, 5);
    let out = b.run("text.delete", json!({}));
    exchange(&mut a, &mut b, Vec::new(), out);
    assert_converged(&a, &b, "merge");
    assert_eq!(texts(&a.s.doc), ["Hello world", "Next"]);
    assert!(!a.live.order().contains(&tail), "the merged paragraph left the order");
}

#[test]
fn delete_beats_concurrent_typing_keeps_recovered_text() {
    let file = plain(&["One", "Two", "Three"]);
    let (mut a, mut b) = (Peer::join(&file, 1), Peer::join(&file, 2));
    // A joins "Two" onto "One" while B, not yet seeing it, types into "Two".
    a.caret(0, 3);
    let from_a = a.run("text.delete", json!({}));
    assert_eq!(texts(&a.s.doc), ["OneTwo", "Three"]);
    b.caret(1, 2);
    let from_b = b.run("text.insert", json!({"text": "XYZ"}));
    exchange(&mut a, &mut b, from_a, from_b);
    assert_converged(&a, &b, "delete against typing");
    for (who, p) in [("a", &a), ("b", &b)] {
        let recovered: Vec<&Paragraph> =
            p.s.doc.body.iter().filter_map(|b| b.as_para()).filter(|q| q.props.style.as_deref() == Some(RECOVERED_STYLE_ID)).collect();
        assert_eq!(recovered.len(), 1, "{who}: one recovered paragraph: {:?}", texts(&p.s.doc));
        assert!(recovered[0].text.contains("XYZ"), "{who}: the typing survives: {:?}", recovered[0].text);
        let style = p.s.doc.styles.get(RECOVERED_STYLE_ID).map(|s| s.name.as_str());
        assert_eq!(style, Some(RECOVERED_STYLE), "{who}: the style exists");
    }
    let at = texts(&a.s.doc).iter().position(|t| t.contains("XYZ")).unwrap();
    assert_eq!(at, 1, "recovered right after the paragraph it was joined to: {:?}", texts(&a.s.doc));
}

/// A small deterministic generator (SplitMix64), so every case replays.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: usize) -> usize {
        if n == 0 { 0 } else { (self.next() % n as u64) as usize }
    }
}

/// Byte offset of a random character boundary in paragraph `block`.
fn random_off(doc: &Document, block: usize, rng: &mut Rng) -> usize {
    let text = doc.body.get(block).and_then(|b| b.as_para()).map(|p| p.text.as_str()).unwrap_or("");
    let bounds: Vec<usize> = text.char_indices().map(|(i, _)| i).chain(std::iter::once(text.len())).collect();
    bounds[rng.below(bounds.len())]
}

/// Put the selection somewhere random in the body's paragraphs: a caret or a range in one
/// paragraph, now and then a range over two.
fn random_selection(p: &mut Peer, rng: &mut Rng) {
    let paras: Vec<usize> = p.s.doc.body.iter().enumerate().filter(|(_, b)| matches!(***b, Block::Para(_))).map(|(i, _)| i).collect();
    let i = paras[rng.below(paras.len())];
    let from = random_off(&p.s.doc, i, rng);
    let (j, to) = match rng.below(4) {
        0 => (i, from),
        1 | 2 => (i, random_off(&p.s.doc, i, rng)),
        _ => {
            let j = paras[rng.below(paras.len())];
            (j, random_off(&p.s.doc, j, rng))
        }
    };
    let (lo, hi) = if (i, from) <= (j, to) { ((i, from), (j, to)) } else { ((j, to), (i, from)) };
    p.s.sel = Selection { anchor: Pos::body(lo.0, lo.1), focus: Pos::body(hi.0, hi.1) };
}

/// One random command of the shareable set.
fn random_command(rng: &mut Rng, styles: &[String]) -> (&'static str, Value) {
    const WORDS: [&str; 6] = ["a", "b ", "cd", "é", "😀", "xyz "];
    match rng.below(8) {
        0 | 1 => ("text.insert", json!({"text": WORDS[rng.below(WORDS.len())]})),
        6 => (if rng.below(2) == 0 { "para.moveUp" } else { "para.moveDown" }, json!({})),
        2 => ("text.delete", json!({})),
        3 => ("text.newParagraph", json!({})),
        4 => ("format.bold", json!({})),
        5 => ("format.italic", json!({})),
        _ => ("para.style", json!({"style": styles[rng.below(styles.len())]})),
    }
}

/// Run one case: `steps` random commands on two peers (deliveries between them not counted), updates delivered in random order.
/// Returns how many recovered paragraphs the result holds.
fn converge_case(seed: u64, steps: usize) -> usize {
    converge_on(&plain(&["Alpha beta", "Gamma", "Delta epsilon zeta", ""]), seed, steps, 2)
}

/// Deliver up to `n` of peer `k`'s waiting updates, picked at random, and queue its answers for
/// every other peer.
fn deliver(peers: &mut [Peer], inbox: &mut [Vec<Vec<u8>>], k: usize, n: usize, rng: &mut Rng) {
    for _ in 0..n {
        if inbox[k].is_empty() {
            return;
        }
        let u = inbox[k].swap_remove(rng.below(inbox[k].len()));
        let answer = peers[k].receive(&u);
        broadcast(inbox, k, answer);
    }
}

fn broadcast(inbox: &mut [Vec<Vec<u8>>], from: usize, updates: Vec<Vec<u8>>) {
    for (j, q) in inbox.iter_mut().enumerate() {
        if j != from {
            q.extend(updates.iter().cloned());
        }
    }
}

fn converge_on(file: &Document, seed: u64, steps: usize, n: usize) -> usize {
    let file = file.clone();
    // Styles the file already has, so applying one changes the paragraph only.
    let styles: Vec<String> =
        ["Normal", "Heading1", "Heading2", "Title", "Quote"].into_iter().filter(|id| file.styles.get(id).is_some()).map(String::from).collect();
    assert!(styles.len() >= 2, "the default sheet has paragraph styles");
    let mut rng = Rng(seed);
    let mut peers: Vec<Peer> = (1..=n as u32).map(|space| Peer::join(&file, space)).collect();
    // inbox[k] holds updates waiting for peer k.
    let mut inbox: Vec<Vec<Vec<u8>>> = vec![Vec::new(); n];
    let mut commands = 0;
    while commands < steps {
        let k = rng.below(n);
        if !inbox[k].is_empty() && rng.below(3) == 0 {
            // Deliver some of what is waiting, in a random order.
            let count = 1 + rng.below(inbox[k].len());
            deliver(&mut peers, &mut inbox, k, count, &mut rng);
            continue;
        }
        random_selection(&mut peers[k], &mut rng);
        let (id, params) = random_command(&mut rng, &styles);
        let out = peers[k].run(id, params);
        broadcast(&mut inbox, k, out);
        commands += 1;
    }
    while inbox.iter().any(|q| !q.is_empty()) {
        for k in 0..n {
            deliver(&mut peers, &mut inbox, k, usize::MAX, &mut rng);
        }
    }
    let a = &peers[0];
    for (j, b) in peers.iter().enumerate().skip(1) {
        assert_converged(a, b, &format!("seed {seed}, peer {j}"));
        let bytes = |p: &Peer| save_bytes("x.docx", &p.s.doc).unwrap();
        assert!(bytes(a) == bytes(b), "seed {seed}: peer {j} saves the same file");
    }
    assert!(!a.s.doc.same_content(&file), "seed {seed}: the edits landed");
    let refused: usize = peers.iter().map(|p| p.refused).sum();
    assert!(refused * 2 < steps, "seed {seed}: most commands were shared ({refused} of {steps} refused)");
    a.s.doc.body.iter().filter_map(|b| b.as_para()).filter(|q| q.props.style.as_deref() == Some(RECOVERED_STYLE_ID)).count()
}

#[test]
fn two_sessions_converge() {
    let recovered: usize = (0..32).map(|seed| converge_case(seed, 300)).sum();
    assert!(recovered > 0, "some cases raced a removal against typing");
}

#[test]
fn a_document_without_ids_is_shared_whole() {
    // Ids already given by an earlier share are kept; duplicates and zeros get fresh ones.
    let mut doc = plain(&["a", "b", "c"]);
    if let Block::Para(p) = std::sync::Arc::make_mut(&mut doc.body[2]) {
        p.pid = 2;
    }
    assign_seed_pids(&mut doc);
    let pids: Vec<u64> = doc.body.iter().filter_map(|b| b.as_para()).map(|p| p.pid).collect();
    let unique: std::collections::BTreeSet<u64> = pids.iter().copied().collect();
    assert_eq!(unique.len(), 3, "{pids:?}");
    assert_eq!(pids[2], 2, "an id the paragraph had is kept");
    assert!(LiveDoc::seeded(&seed_blocks(&doc)).is_ok());
}

/// The same on the sample document: a table (shared whole), lists and inline objects.
#[test]
fn two_sessions_converge_on_the_sample() {
    for seed in 0..8 {
        converge_on(&wordcraft_engine::sample::sample_document(), seed, 300, 2);
    }
}

#[test]
fn three_sessions_converge() {
    for seed in 100..108 {
        converge_on(&plain(&["Alpha beta", "Gamma", "Delta epsilon zeta"]), seed, 200, 3);
    }
}

#[test]
fn bold_at_a_caret_then_typing_is_bold_everywhere() {
    let file = plain(&["Hello world"]);
    let (mut a, mut b) = (Peer::join(&file, 1), Peer::join(&file, 2));
    a.caret(0, 5);
    // Bold at a collapsed caret changes nothing yet; the next typing carries it.
    assert!(a.run("format.bold", json!({})).is_empty());
    let out = a.run("text.insert", json!({"text": "XY"}));
    exchange(&mut a, &mut b, out, Vec::new());
    assert_converged(&a, &b, "pending bold");
    let p = b.s.doc.body[0].as_para().unwrap();
    let mut at = 0;
    let bold: Vec<&str> = p
        .runs
        .iter()
        .filter_map(|r| {
            let t = &p.text[at..at + r.len];
            at += r.len;
            (r.props.bold == Some(true)).then_some(t)
        })
        .collect();
    assert_eq!(bold, ["XY"], "only the typed text is bold: {:?}", p.runs);
}

#[test]
fn a_move_keeps_concurrent_typing_in_the_moved_paragraph() {
    let file = plain(&["One", "Two", "Three"]);
    let (mut a, mut b) = (Peer::join(&file, 1), Peer::join(&file, 2));
    // A moves "Two" above "One" while B types into both.
    a.caret(1, 1);
    let from_a = a.run("para.moveUp", json!({}));
    assert_eq!(texts(&a.s.doc), ["Two", "One", "Three"]);
    b.caret(0, 3);
    let mut from_b = b.run("text.insert", json!({"text": "!"}));
    b.caret(1, 3);
    from_b.extend(b.run("text.insert", json!({"text": "?"})));
    exchange(&mut a, &mut b, from_a, from_b);
    assert_converged(&a, &b, "move against typing");
    assert_eq!(texts(&a.s.doc), ["Two?", "One!", "Three"], "both typings stay in their paragraphs");
    assert!(a.s.doc.styles.get(RECOVERED_STYLE_ID).is_none(), "a move recovers nothing");
}

#[test]
fn an_edit_before_the_space_is_granted_is_refused_and_put_back() {
    // The sample has a table after the edited paragraphs, so the table ids must stay paired.
    let file = wordcraft_engine::sample::sample_document();
    let mut b = Peer::join(&file, 2);
    let mut a = {
        let mut s = Session::new(file.clone());
        s.assign_pids(assign_seed_pids).unwrap();
        let live = LiveDoc::seeded(&seed_blocks(&s.doc)).unwrap();
        let map = LiveMap::new(&s.doc, 0);
        let edits = Edits::default();
        s.set_observer(Some(Box::new(Watch(edits.clone()))));
        Peer { s, live, map, edits, refused: 0 }
    };
    let opened = a.s.doc.clone();
    a.caret(0, 0);
    assert!(a.run("text.insert", json!({"text": "Early "})).is_empty(), "nothing goes out without a space");
    assert!(a.run("text.newParagraph", json!({})).is_empty());
    assert_eq!(a.refused, 2, "both were refused");
    assert!(a.s.doc.same_content(&opened), "and put back");
    // B edits meanwhile; A takes it in.
    let last = b.s.doc.body.len() - 1;
    b.caret(last, 0);
    let from_b = b.run("text.insert", json!({"text": "Late "}));
    for u in &from_b {
        a.receive(u);
    }
    a.live.set_client(1).unwrap();
    assert!(a.map.grant(&a.live, &a.s.doc, 1), "the session and the replica agree at the grant");
    // From now on A's edits go out, the tables included.
    a.caret(0, 0);
    let mut out = a.run("text.insert", json!({"text": "Now "}));
    let t = a.s.doc.body.iter().position(|x| x.as_table().is_some()).unwrap();
    a.caret(t + 1, 0);
    out.extend(a.run("text.insert", json!({"text": "After "})));
    exchange(&mut a, &mut b, out, Vec::new());
    assert_converged(&a, &b, "after the grant");
    let all = texts(&a.s.doc).join("|");
    assert!(all.contains("Now ") && all.contains("Late ") && all.contains("After ") && !all.contains("Early "), "{all}");
    let tables = |d: &Document| d.body.iter().filter(|x| x.as_table().is_some()).count();
    assert_eq!(tables(&a.s.doc), tables(&file), "the tables are still there, once each");
}

#[test]
fn a_command_is_reported_once_and_put_back_whole() {
    let file = plain(&["Hello"]);
    let mut a = Peer::join(&file, 1);
    // A macro runs its steps inside one command: one edit, whose `before` is from before it.
    a.s.macros.insert("mark".into(), vec![("text.insert".into(), json!({"text": "typed "})), ("insert.bookmark".into(), json!({"name": "here"}))]);
    a.caret(0, 0);
    let out = a.run("tools.macros", json!({"run": "mark"}));
    assert!(out.is_empty() && a.refused == 1, "refused once, nothing sent");
    assert!(
        a.s.doc.same_content(&{
            let mut d = file.clone();
            assign_seed_pids(&mut d);
            d
        }),
        "the typing inside the macro is put back too: {:?}",
        texts(&a.s.doc)
    );
}

#[test]
fn objects_are_not_shared() {
    assert!(!is_shareable("insert.picture"), "pictures are not shared");
    assert!(!is_shareable("edit.paste"), "paste may carry objects");
    assert!(is_shareable("text.insert") && is_shareable("para.moveUp"));
    let commands = wordcraft_engine::cmd::registry();
    for id in wordcraft_live::SHAREABLE {
        assert!(commands.get(id).is_some(), "{id} is a command");
    }

    // The safety net: an edit that adds an inline object is refused and sends nothing.
    let file = plain(&["Text"]);
    let mut a = Peer::join(&file, 1);
    let before = a.s.doc.clone();
    let mut after = before.clone();
    if let Block::Para(p) = std::sync::Arc::make_mut(&mut after.body[0]) {
        p.text.push(wordcraft_doc::para::OBJ);
        p.objects.push(wordcraft_doc::InlineObject::FieldEnd);
        p.normalize();
    }
    let r = a.map.local_updates(&mut a.live, &before, &after, "insert.field");
    assert_eq!(r.unwrap_err(), MapError::NotShareable { command: "insert.field".into(), reason: Refusal::InlineObject });
    assert_eq!(a.live.block(a.live.order()[0]).unwrap().text, "Text", "the replica is untouched");
    // So is one that adds a style definition.
    let mut after = before.clone();
    std::sync::Arc::make_mut(&mut after.styles).styles.push(wordcraft_doc::Style { id: "Mine".into(), ..Default::default() });
    let r = a.map.local_updates(&mut a.live, &before, &after, "style.new");
    assert!(matches!(r, Err(MapError::NotShareable { reason: Refusal::OutsideBody("styles"), .. })), "{r:?}");

    // Real commands that change only the settings or the last section's setup.
    for (id, params, part) in
        [("review.trackChanges", json!({"value": true}), "settings"), ("layout.orientation", json!({"value": "landscape"}), "last_section")]
    {
        let kept = a.s.doc.clone();
        a.s.run(id, &params).unwrap();
        let (before, after, command) = a.edits.lock().unwrap().pop().unwrap();
        let r = a.map.local_updates(&mut a.live, &before, &after, &command);
        assert!(matches!(&r, Err(MapError::NotShareable { reason: Refusal::OutsideBody(p), .. }) if *p == part), "{id}: {r:?}");
        a.s.apply_remote(|d| *d = before.clone()).unwrap();
        assert!(a.s.doc.same_content(&kept));
    }
}
