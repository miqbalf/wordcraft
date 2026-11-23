//! Live co-editing cost: the time `Session::run` takes for a keystroke and a caret move in a
//! long document, without and with an [`EditObserver`] installed (layout not included).
//!
//! `cargo run --release -p wordcraft-engine --example live_bench -- [paragraphs] [comments] [keys]`

use std::time::Instant;

use serde_json::{Value, json};
use wordcraft_engine::doc::{Document, StoryRef};
use wordcraft_engine::{EditObserver, Session};

/// An observer that does nothing, so only the engine's side is timed.
struct Quiet;

impl EditObserver for Quiet {
    fn edited(&mut self, _: &Document, _: &Document, _: &str) {}
}

/// Median and 95th percentile, in milliseconds, of running `id` `n` times.
fn time(s: &mut Session, id: &str, params: &Value, n: usize) -> Result<(f64, f64), String> {
    let mut ms = Vec::with_capacity(n);
    for _ in 0..n {
        let t = Instant::now();
        s.run(id, params).map_err(|e| e.to_string())?;
        ms.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    ms.sort_by(f64::total_cmp);
    let at = |q: f64| ms.get(((ms.len() as f64 - 1.0) * q) as usize).copied().unwrap_or(0.0);
    Ok((at(0.5), at(0.95)))
}

fn main() -> Result<(), String> {
    let args: Vec<usize> = std::env::args().skip(1).filter_map(|a| a.parse().ok()).collect();
    let paras = args.first().copied().unwrap_or(2000);
    let comments = args.get(1).copied().unwrap_or(200);
    let keys = args.get(2).copied().unwrap_or(200);
    let text: String = (0..paras).map(|i| format!("Paragraph {i} of a long shared document, with some words in it.\n")).collect();
    let mut s = Session::new(Document::from_text(&text));
    for i in 0..comments {
        let path = s.doc.para_paths(StoryRef::Body).get(i * paras / comments.max(1)).cloned().ok_or("no paragraph")?;
        s.run("caret.set", &json!({"pos": {"story": "body", "path": path.0, "off": 0}})).map_err(|e| e.to_string())?;
        s.run("select.word", &json!({})).map_err(|e| e.to_string())?;
        s.run("review.newComment", &json!({"text": format!("Comment {i}")})).map_err(|e| e.to_string())?;
    }
    let mid = s.doc.para_paths(StoryRef::Body).get(paras / 2).cloned().ok_or("no paragraph")?;
    s.run("caret.set", &json!({"pos": {"story": "body", "path": mid.0, "off": 0}})).map_err(|e| e.to_string())?;
    for observed in [false, true] {
        if observed {
            s.set_observer(Some(Box::new(Quiet)));
        }
        let (k50, k95) = time(&mut s, "text.insert", &json!({"text": "a"}), keys)?;
        let (c50, c95) = time(&mut s, "caret.left", &json!({}), keys)?;
        println!(
            "paragraphs {paras}, comments {}, observer {observed}: keystroke median {k50:.3} ms p95 {k95:.3} ms; caret move median {c50:.3} ms p95 {c95:.3} ms",
            s.doc.comments.len()
        );
    }
    Ok(())
}
