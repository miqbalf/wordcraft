//! How a browser shell wires its save hooks (Save As = Export, [`crate::save_as`]).
//!
//! The shells (`apps/wordcraft-web`: the plain page, a `data-host="page"` page and a
//! `?host=parent` frame before and after the host says it stores saves) call [`wire`] instead of
//! setting the hooks themselves, so the one rule is tested here, natively:
//!
//! - Save As and Export copies always go to the browser download the shell gives
//!   (`craft_web_host::web::download`), in every mode. A copy is never posted to a host page.
//! - Only plain Save goes to a host page, and only where the page stores saves.
//!
//! The copy hook is not optional: a shell can't leave it out, and a window without one refuses a
//! copy with an error ([`crate::WordApp::save_as_copy`]) instead of posting it.

use std::rc::Rc;

use crate::Services;

/// The browser download of a Save As or Export copy: name, MIME type, bytes.
pub type CopyHook = Box<dyn Fn(&str, &str, &[u8]) -> Result<(), String>>;
/// A browser download of a plain Save: name, bytes.
pub type DownloadHook = Box<dyn Fn(&str, &[u8]) -> Result<(), String>>;
/// A plain Save posted to the host page: name, bytes, whether it was a Save As.
pub type HostPost = Rc<dyn Fn(&str, &[u8], bool) -> Result<(), String>>;

/// What frames the browser window.
pub enum BrowserHost {
    /// No host page: Save downloads (or uses the save picker).
    Plain,
    /// A page that stores saves itself (`data-host="page"`): Save and every other plain
    /// download go to it.
    Page(HostPost),
    /// A framing page (`?host=parent`) that stores nothing (yet): Save downloads a copy.
    Parent,
    /// A framing page that said it stores saves (`saves: true`): plain Save goes to it with an
    /// id, Save As copies still download.
    ParentSaves(HostPost),
}

/// Set `services`' save hooks for `host`: `copy` takes every Save As and Export copy whatever
/// frames the window, `download` the plain downloads where no page takes them.
pub fn wire(services: &mut Services, host: BrowserHost, copy: CopyHook, download: DownloadHook) {
    services.download_copy = Some(copy);
    match host {
        BrowserHost::Plain | BrowserHost::Parent => {
            services.host_save = None;
            services.download = Some(download);
        }
        BrowserHost::Page(post) => {
            let save = post.clone();
            services.host_save = Some(Box::new(move |name: &str, bytes: &[u8], save_as: bool| save(name, bytes, save_as)));
            // Anything else that would download goes to the page too; copies don't.
            services.download = Some(Box::new(move |name: &str, bytes: &[u8]| post(name, bytes, false)));
        }
        BrowserHost::ParentSaves(post) => {
            services.host_save = Some(Box::new(move |name: &str, bytes: &[u8], save_as: bool| post(name, bytes, save_as)));
            services.download = Some(download);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use serde_json::json;

    use super::*;
    use crate::WordApp;

    #[derive(Default)]
    struct Seen {
        copies: Vec<String>,
        downloads: Vec<String>,
        posts: Vec<String>,
    }

    fn hooks(seen: &Rc<RefCell<Seen>>) -> (CopyHook, DownloadHook, HostPost) {
        let (c, d, p) = (seen.clone(), seen.clone(), seen.clone());
        (
            Box::new(move |name: &str, _: &str, _: &[u8]| {
                c.borrow_mut().copies.push(name.to_string());
                Ok(())
            }),
            Box::new(move |name: &str, _: &[u8]| {
                d.borrow_mut().downloads.push(name.to_string());
                Ok(())
            }),
            Rc::new(move |name: &str, _: &[u8], _: bool| {
                p.borrow_mut().posts.push(name.to_string());
                Ok(())
            }),
        )
    }

    /// In every mode a Save As copy and an export go to the copy hook (the browser download),
    /// never to the host page; plain Save goes to the page only where it stores saves.
    #[test]
    fn copies_download_and_only_plain_save_reaches_a_host() {
        for mode in ["plain", "page", "parent", "parent saves"] {
            let seen = Rc::new(RefCell::new(Seen::default()));
            let (copy, download, post) = hooks(&seen);
            let host = match mode {
                "plain" => BrowserHost::Plain,
                "page" => BrowserHost::Page(post),
                "parent" => BrowserHost::Parent,
                _ => BrowserHost::ParentSaves(post),
            };
            let mut services = Services { hosted: mode.starts_with("parent"), saves_are_copies: mode == "parent", ..Services::default() };
            wire(&mut services, host, copy, download);
            let mut a = WordApp::new(wordcraft_engine::Session::new(wordcraft_doc::Document::new()), services);
            a.run("text.insert", json!({"text": "Plan"})).unwrap();
            for id in ["docx", "odt", "pdf"] {
                a.save_as_copy(id).unwrap();
            }
            a.run("file.exportPng", json!({})).unwrap();
            a.run("file.printPdf", json!({})).unwrap();
            let seen = seen.borrow();
            assert_eq!(seen.copies.len(), 5, "{mode}: every copy downloads: {:?}", seen.copies);
            assert!(seen.posts.is_empty() && seen.downloads.is_empty(), "{mode}: no copy posted or saved: {:?} {:?}", seen.posts, seen.downloads);
        }
    }

    /// A window wired without its copy hook refuses a copy loudly; it never falls back to the
    /// host page or the save download.
    #[test]
    fn a_window_without_the_copy_hook_refuses_copies() {
        let seen = Rc::new(RefCell::new(Seen::default()));
        let (copy, download, post) = hooks(&seen);
        let mut services = Services::default();
        wire(&mut services, BrowserHost::Page(post), copy, download);
        services.download_copy = None;
        let mut a = WordApp::new(wordcraft_engine::Session::new(wordcraft_doc::Document::new()), services);
        assert!(a.save_as_copy("pdf").is_err());
        assert!(a.run("file.printPdf", json!({})).is_err());
        assert!(!a.export_error.is_empty(), "the reason is shown");
        let seen = seen.borrow();
        assert!(seen.posts.is_empty() && seen.downloads.is_empty() && seen.copies.is_empty(), "nothing went anywhere");
    }
}
