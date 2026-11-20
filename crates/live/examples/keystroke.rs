//! Times the mapping for one keystroke in a long document: the local diff on the typing peer,
//! and the remote merge and rebuild on the other. Run with
//! `cargo run --release -p wordcraft-live --example keystroke [paragraphs]`.
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use craft_collab::LiveDoc;
use serde_json::json;
use wordcraft_doc::{Document, Paragraph, Pos, para_block};
use wordcraft_engine::{EditObserver, Selection, Session};
use wordcraft_live::{LiveMap, assign_seed_pids, seed_blocks, share_pids};

type Edits = Arc<Mutex<Vec<(Document, Document, String)>>>;

struct Watch(Edits);

impl EditObserver for Watch {
    fn edited(&mut self, before: &Document, after: &Document, command: &str) {
        if let Ok(mut v) = self.0.lock() {
            v.push((before.clone(), after.clone(), command.to_owned()));
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let n: usize = std::env::args().nth(1).and_then(|a| a.parse().ok()).unwrap_or(2000);
    let mut file = Document::new();
    file.body =
        (0..n).map(|i| para_block(Paragraph::with_text(&format!("Paragraph {i} with some ordinary text in it."), Default::default()))).collect();
    let join = |space: u32| -> Result<(Session, LiveDoc, LiveMap), Box<dyn std::error::Error>> {
        let mut s = Session::new(file.clone());
        s.assign_pids(assign_seed_pids)?;
        let mut live = LiveDoc::seeded(&seed_blocks(&s.doc))?;
        live.set_client(space)?;
        let map = LiveMap::new(&s.doc, space);
        Ok((s, live, map))
    };
    let (mut a, mut a_live, mut a_map) = join(1)?;
    let (mut b, mut b_live, mut b_map) = join(2)?;
    let edits = Edits::default();
    a.set_observer(Some(Box::new(Watch(edits.clone()))));
    let (mut local, mut merge, mut rebuild, mut remote, mut command) =
        (Duration::ZERO, Duration::ZERO, Duration::ZERO, Duration::ZERO, Duration::ZERO);
    let keys = 200u32;
    for k in 0..keys {
        let at = Pos::body(n / 2, 5 + k as usize);
        a.sel = Selection { anchor: at.clone(), focus: at };
        let t = Instant::now();
        a.run("text.insert", &json!({"text": "x"}))?;
        command += t.elapsed();
        let seen: Vec<_> = edits.lock().map(|mut v| std::mem::take(&mut *v)).unwrap_or_default();
        for (before, after, id) in seen {
            let t = Instant::now();
            let edit = a_map.local_updates(&mut a_live, &before, &after, &id)?;
            share_pids(&mut a, &edit)?;
            local += t.elapsed();
            for u in &edit.updates {
                let t = Instant::now();
                let r = b_map.receive(&mut b_live, u)?;
                merge += t.elapsed();
                let mut inner = Duration::ZERO;
                let t = Instant::now();
                b.apply_remote(|d| {
                    let t = Instant::now();
                    b_map.apply_changes(d, &b_live, &r.changes);
                    inner = t.elapsed();
                })?;
                remote += t.elapsed();
                rebuild += inner;
            }
        }
    }
    let per = |d: Duration| d.as_secs_f64() * 1e6 / f64::from(keys);
    println!("{n} paragraphs, {keys} keystrokes, mean each:");
    println!("  typing peer: engine command {:.1} µs, local_updates + share_pids {:.1} µs", per(command), per(local));
    println!(
        "  other peer: receive (yrs apply) {:.1} µs, apply_changes {:.1} µs, the whole Session::apply_remote {:.1} µs",
        per(merge),
        per(rebuild),
        per(remote)
    );
    println!("converged: {}", a.doc.same_content(&b.doc));
    Ok(())
}
