use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;

use super::*;

const TOKEN: &str = "cGF5bG9hZA.c2lnbmF0dXJl";

/// What the fake server saw of one request.
struct Seen {
    request_line: String,
    headers: Vec<(String, String)>,
    body: String,
}

/// A fake craft_office on 127.0.0.1 that answers each request with the next of `answers`
/// (status, body) and hands back what it saw.
fn server(answers: Vec<(u16, String)>) -> (String, std::thread::JoinHandle<Vec<Seen>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let handle = std::thread::spawn(move || {
        let mut seen = Vec::new();
        for (status, body) in answers {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request_line = String::new();
            reader.read_line(&mut request_line).unwrap();
            let mut headers = Vec::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let line = line.trim_end().to_string();
                if line.is_empty() {
                    break;
                }
                let (k, v) = line.split_once(':').unwrap();
                headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
            }
            let len: usize = headers.iter().find(|(k, _)| k == "content-length").map_or(0, |(_, v)| v.parse().unwrap());
            let mut buf = vec![0; len];
            reader.read_exact(&mut buf).unwrap();
            let extra = if status == 302 { "Location: http://127.0.0.1:9/elsewhere\r\n" } else { "" };
            let mut stream = stream;
            write!(
                stream,
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
            seen.push(Seen { request_line, headers, body: String::from_utf8(buf).unwrap() });
        }
        seen
    });
    (base, handle)
}

fn creds(server: &str) -> Creds {
    Creds { server: server.to_string(), token: TOKEN.to_string(), proof: true, ai: true }
}

fn check(id: &str, text: &str) -> AssistOut {
    AssistOut::ProofCheck { id: id.into(), language: "auto".into(), text: text.into() }
}

fn session_json(server: &str, token: &str, expires: chrono::DateTime<chrono::Utc>) -> String {
    json!({"v": 1, "server": server, "fileId": 7, "ticket": "t", "assist": {"token": token, "expiresAt": expires.to_rfc3339(), "proof": true, "ai": false}}).to_string()
}

#[test]
fn a_check_goes_to_the_token_route_with_the_token_in_a_header_and_reads_the_routes_answer() {
    let body = json!({"matches": [{"offset": 4, "length": 2, "kind": "grammar", "message": "agreement", "replacements": ["goes"], "rule": "R"}], "indonesian": []});
    let (base, seen) = server(vec![(200, body.to_string())]);
    let answer = ask(&creds(&base), &check("p1", "She go home."));
    let AssistIn::ProofResult { id, reply } = answer else { panic!("a result") };
    assert_eq!(id, "p1");
    assert_eq!((reply.matches[0].offset, reply.matches[0].length), (4, 2));
    let seen = seen.join().unwrap();
    let r = &seen[0];
    assert!(r.request_line.starts_with(&format!("POST {PROOF_PATH} ")), "{}", r.request_line);
    assert!(!r.request_line.contains(TOKEN), "never in the URL");
    assert!(r.headers.contains(&(TOKEN_HEADER.to_ascii_lowercase(), TOKEN.to_string())));
    assert!(!r.headers.iter().any(|(k, _)| k == "authorization" || k == "cookie"), "no other credential");
    let sent: Value = serde_json::from_str(&r.body).unwrap();
    assert_eq!(sent, json!({"language": "auto", "text": "She go home."}), "the web route's body");
}

#[test]
fn improve_and_summarize_use_their_routes() {
    let (base, seen) = server(vec![(200, json!({"text": "went"}).to_string()), (200, json!({"text": "- x"}).to_string())]);
    let c = creds(&base);
    let improve = ask(&c, &AssistOut::Ai { id: "a1".into(), task: AiTask::Improve, language: "en".into(), text: "go".into() });
    assert_eq!(improve, AssistIn::AiResult { id: "a1".into(), text: "went".into() });
    let summary = ask(&c, &AssistOut::Ai { id: "a2".into(), task: AiTask::Summarize, language: "en".into(), text: "doc".into() });
    assert_eq!(summary, AssistIn::AiResult { id: "a2".into(), text: "- x".into() });
    let seen = seen.join().unwrap();
    assert!(seen[0].request_line.contains(IMPROVE_PATH) && seen[1].request_line.contains(SUMMARIZE_PATH));
}

#[test]
fn failures_become_the_pages_codes_and_a_redirect_is_not_followed() {
    let answers = vec![
        (401, json!({"error": "signed_out"}).to_string()),
        (429, String::new()),
        (413, json!({"error": "too_large"}).to_string()),
        (503, json!({"error": "unavailable"}).to_string()),
        (302, String::new()),
        (200, "not json".to_string()),
    ];
    let (base, seen) = server(answers);
    let c = creds(&base);
    let codes: Vec<String> = (0..6)
        .map(|i| match ask(&c, &check(&format!("p{i}"), "x")) {
            AssistIn::ProofFailed { error, .. } => error,
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(codes, ["signed_out", "busy", "too_large", "unavailable", "unavailable", "unavailable"]);
    assert_eq!(seen.join().unwrap().len(), 6, "the redirect's target was never asked");
}

#[test]
fn a_down_server_is_unavailable() {
    let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let answer = ask(
        &creds(&format!("http://127.0.0.1:{port}")),
        &AssistOut::Ai { id: "a".into(), task: AiTask::Improve, language: "en".into(), text: "x".into() },
    );
    assert_eq!(answer, AssistIn::AiFailed { id: "a".into(), error: "unavailable".into() });
}

#[test]
fn only_an_unexpired_token_for_a_safe_server_is_used() {
    let now = chrono::Utc::now();
    let later = now + chrono::Duration::minutes(5);
    let c = creds_from(&session_json("https://cloud.example.com/", TOKEN, later), now).unwrap();
    assert_eq!(c.server, "https://cloud.example.com");
    assert!(c.proof && !c.ai);
    assert!(creds_from(&session_json("https://cloud.example.com", TOKEN, now), now).is_none(), "expired");
    for bad in ["http://cloud.example.com", "ftp://x", "https://u:p@cloud.example.com", "https://cloud.example.com?x=1", "https://", "file:///etc"] {
        assert!(creds_from(&session_json(bad, TOKEN, later), now).is_none(), "{bad}");
    }
    for ok in ["http://localhost:8035", "http://127.0.0.1:1", "https://[2001:db8::1]:8443"] {
        assert!(creds_from(&session_json(ok, TOKEN, later), now).is_some(), "{ok}");
    }
    assert!(creds_from(&session_json("https://a.example", "", later), now).is_none());
    assert!(creds_from(&session_json("https://a.example", "tok\nen", later), now).is_none());
    assert!(creds_from(&json!({"v": 1, "server": "https://a.example", "ticket": "t"}).to_string(), now).is_none(), "a session file without assist");
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("wc-assist-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn told(net: &mut AssistNet, at: Instant) -> Vec<AssistIn> {
    net.frame(at);
    std::mem::take(&mut *net.inbox.borrow_mut())
}

#[test]
fn the_window_is_told_signed_out_then_on_then_signed_out_after_a_401_until_a_new_token() {
    let dir = scratch("states");
    let session = dir.join("live.json");
    let account = dir.join("assist.json");
    let later = chrono::Utc::now() + chrono::Duration::minutes(5);
    let (base, seen) = server(vec![(401, String::new())]);
    let mut net = AssistNet::new(vec![session.clone(), account.clone()], egui::Context::default());
    let host = net.host();
    let t0 = Instant::now();
    assert_eq!(told(&mut net, t0), vec![AssistIn::SignedOut], "no file: sign in");
    assert!((host.send)(&check("p0", "x")).is_err(), "nothing is sent without a token");
    // Only the account file (a local or checked-out file): it is used.
    std::fs::write(&account, session_json(&base, TOKEN, later)).unwrap();
    assert_eq!(told(&mut net, t0 + RELOAD), vec![AssistIn::Enabled { proof: true, ai: false }]);
    assert_eq!(told(&mut net, t0 + RELOAD * 2), vec![], "told once");
    (host.send)(&check("p1", "x")).unwrap();
    seen.join().unwrap();
    let mut got = Vec::new();
    for _ in 0..200 {
        got.extend(told(&mut net, t0 + RELOAD * 2));
        if got.len() >= 2 {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(got, vec![AssistIn::ProofFailed { id: "p1".into(), error: "signed_out".into() }, AssistIn::SignedOut]);
    assert_eq!(told(&mut net, t0 + RELOAD * 3), vec![], "the refused token in the file is not taken again");
    // The launcher renewed it; the session file comes first.
    std::fs::write(&session, session_json(&base, "bmV3.dG9rZW4", later)).unwrap();
    assert_eq!(told(&mut net, t0 + RELOAD * 4), vec![AssistIn::Enabled { proof: true, ai: false }]);
    let _ = std::fs::remove_dir_all(&dir);
}
