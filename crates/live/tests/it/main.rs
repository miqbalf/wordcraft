//! The mapping between a WordCraft document and the shared block document: seeding, local
//! edits going out, remote edits coming in, and two real sessions converging.
use std::sync::{Arc, Mutex};

use craft_collab::{BlockChange, LiveDoc};
use serde_json::{Value, json};
use wordcraft_doc::{Block, Document, Paragraph, Pos, para_block};
use wordcraft_engine::io::{open_bytes, save_bytes};
use wordcraft_engine::{EditObserver, Selection, Session};
use wordcraft_live::{LiveMap, RECOVERED_STYLE, RECOVERED_STYLE_ID, assign_seed_pids, seed_blocks};

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
type Edits = Arc<Mutex<Vec<(Document, Document)>>>;

struct Watch(Edits);

impl EditObserver for Watch {
    fn edited(&mut self, before: &Document, after: &Document, _command: &str) {
        self.0.lock().unwrap().push((before.clone(), after.clone()));
    }
}

/// One participant: an engine session, its replica of the shared document and the map between
/// them, wired the way the app wires them.
struct Peer {
    s: Session,
    live: LiveDoc,
    map: LiveMap,
    edits: Edits,
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
        Peer { s, live, map, edits }
    }

    /// Run a command and return the updates it sends. A refused command sends nothing.
    fn run(&mut self, id: &str, params: Value) -> Vec<Vec<u8>> {
        let _ = self.s.run(id, &params);
        let seen: Vec<_> = std::mem::take(&mut *self.edits.lock().unwrap());
        let mut out = Vec::new();
        for (before, after) in seen {
            let edit = self.map.local_updates(&mut self.live, &before, &after).unwrap();
            assert!(edit.failed.is_empty(), "{id}: {:?}", edit.failed);
            wordcraft_live::share_pids(&mut self.s, &edit).unwrap();
            out.extend(edit.updates);
        }
        out
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
    match rng.below(7) {
        0 | 1 => ("text.insert", json!({"text": WORDS[rng.below(WORDS.len())]})),
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
    converge_on(&plain(&["Alpha beta", "Gamma", "Delta epsilon zeta", ""]), seed, steps)
}

fn converge_on(file: &Document, seed: u64, steps: usize) -> usize {
    let file = file.clone();
    // Styles the file already has, so applying one changes the paragraph only.
    let styles: Vec<String> =
        ["Normal", "Heading1", "Heading2", "Title", "Quote"].into_iter().filter(|id| file.styles.get(id).is_some()).map(String::from).collect();
    assert!(styles.len() >= 2, "the default sheet has paragraph styles");
    let mut rng = Rng(seed);
    let mut peers = [Peer::join(&file, 1), Peer::join(&file, 2)];
    // inbox[k] holds updates waiting for peer k.
    let mut inbox: [Vec<Vec<u8>>; 2] = [Vec::new(), Vec::new()];
    let mut commands = 0;
    while commands < steps {
        let k = rng.below(2);
        if !inbox[k].is_empty() && rng.below(3) == 0 {
            // Deliver some of what is waiting, in a random order.
            for _ in 0..=rng.below(inbox[k].len()) {
                let u = inbox[k].swap_remove(rng.below(inbox[k].len()));
                let answer = peers[k].receive(&u);
                inbox[1 - k].extend(answer);
            }
            continue;
        }
        random_selection(&mut peers[k], &mut rng);
        let (id, params) = random_command(&mut rng, &styles);
        let out = peers[k].run(id, params);
        inbox[1 - k].extend(out);
        commands += 1;
    }
    while inbox.iter().any(|q| !q.is_empty()) {
        for k in 0..2 {
            while !inbox[k].is_empty() {
                let u = inbox[k].swap_remove(rng.below(inbox[k].len()));
                let answer = peers[k].receive(&u);
                inbox[1 - k].extend(answer);
            }
        }
    }
    let [a, b] = &peers;
    assert_converged(a, b, &format!("seed {seed}"));
    let bytes = |p: &Peer| save_bytes("x.docx", &p.s.doc).unwrap();
    assert!(bytes(a) == bytes(b), "seed {seed}: both save the same file");
    assert!(!a.s.doc.same_content(&file), "seed {seed}: the edits landed");
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
        converge_on(&wordcraft_engine::sample::sample_document(), seed, 300);
    }
}
