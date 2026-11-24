//! Safety-net cost: the time [`LiveMap::drift`] takes to compare a long window's body with the
//! shared copy. `LiveEditing::verify` runs it only when the session or the shared copy changed
//! since the last clean check; otherwise it is two integer compares.
//!
//! `cargo run --release -p wordcraft-live --example verify_bench -- [paragraphs] [runs]`

use std::time::Instant;

use craft_collab::LiveDoc;
use serde_json::json;
use wordcraft_engine::doc::{Document, Pos};
use wordcraft_engine::{Selection, Session};
use wordcraft_live::{LiveMap, assign_seed_pids, seed_blocks};

fn main() -> Result<(), String> {
    let args: Vec<usize> = std::env::args().skip(1).filter_map(|a| a.parse().ok()).collect();
    let paras = args.first().copied().unwrap_or(2000);
    let runs = args.get(1).copied().unwrap_or(50);
    let text: String = (0..paras).map(|i| format!("Paragraph {i} of a long shared document, with some words in it.\n")).collect();
    let mut s = Session::new(Document::from_text(&text));
    // Some formatting, so the runs are compared too.
    for i in (0..paras).step_by(3) {
        s.sel = Selection { anchor: Pos::body(i, 0), focus: Pos::body(i, 9) };
        s.run("format.bold", &json!({})).map_err(|e| e.to_string())?;
    }
    s.assign_pids(assign_seed_pids).map_err(|e| e.to_string())?;
    let live = LiveDoc::seeded(&seed_blocks(&s.doc)).map_err(|e| e.to_string())?;
    let map = LiveMap::new(&s.doc, 1);
    if let Some(d) = map.drift(&s.doc, &live) {
        return Err(format!("the seeded window drifted: {d}"));
    }
    let mut ms = Vec::with_capacity(runs);
    for _ in 0..runs {
        let t = Instant::now();
        std::hint::black_box(map.drift(&s.doc, &live));
        ms.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    ms.sort_by(f64::total_cmp);
    let at = |q: f64| ms.get(((ms.len() as f64 - 1.0) * q) as usize).copied().unwrap_or(0.0);
    println!("{paras} paragraphs, {runs} checks: drift median {:.2} ms, p95 {:.2} ms", at(0.5), at(0.95));
    Ok(())
}
