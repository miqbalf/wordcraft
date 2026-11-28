//! udinCraft: this build updates from the fork's own GitHub releases, never from upstream's.
//!
//! [`crate::UPDATE_APP`] names the fork (`miqbalf/wordcraft`), and [`source`] is the check the
//! updater runs at start and from Help ▸ Check for Updates. It reads the fork's release list and
//! counts only the udinCraft release workflow's tags, `udincraft-v<version>-<YYYYMMDD>`: an
//! upstream-style tag is never offered, and with no udinCraft release yet there is simply no
//! update. The release's files are then checked against its `SHA256SUMS.txt`: each file takes
//! its checksum from that list (GitHub's own digest, when it has one, must agree), and a file the
//! list doesn't name is dropped, so the update installs only what the list vouches for.
//!
//! Trust: the release is read over HTTPS from github.com/miqbalf and its files are checked
//! against its sums file. The builds carry no signature (macOS ad hoc, Windows unsigned), as the
//! release notes say.

use craft_update::Release;
use craft_update::Version;

/// The tag prefix of the fork's releases.
pub const TAG_PREFIX: &str = "udincraft-v";

/// Reads a udinCraft release tag, `udincraft-v<version>-<YYYYMMDD>`: the version (the one this app
/// reports) and the build date, which only breaks a tie between two builds of one version. `None`
/// for any other tag.
pub fn tag_version(tag: &str) -> Option<(Version, u32)> {
    let rest = tag.strip_prefix(TAG_PREFIX)?;
    let (version, date) = rest.rsplit_once('-')?;
    let plausible = date.len() == 8
        && date.bytes().all(|b| b.is_ascii_digit())
        && matches!(&date[4..6], "01" | "02" | "03" | "04" | "05" | "06" | "07" | "08" | "09" | "10" | "11" | "12")
        && (1..=31).contains(&date[6..].parse::<u8>().ok()?);
    if !plausible || version.len() > 64 {
        return None;
    }
    let version = Version::parse(version).ok().filter(|v| v.build.is_empty())?;
    Some((version, date.parse().ok()?))
}

/// The release in the fork's release list `json` (`GET /repos/<repo>/releases`) to offer a copy
/// running `running`: the newest stable udinCraft release that publishes `SHA256SUMS.txt`, when it
/// is newer than `running`, with its files checked against that list (`sums` fetches it: `Ok(None)`
/// when it isn't there).
///
/// # Errors
///
/// When the list can't be read, the sums file can't be fetched or is missing, or a file's GitHub
/// digest disagrees with the sums file.
pub fn offered_release(
    json: &str,
    repo: &str,
    running: &str,
    sums: impl FnOnce(&str) -> Result<Option<String>, String>,
) -> Result<Option<Release>, String> {
    let list: serde_json::Value = serde_json::from_str(json).map_err(|e| format!("unreadable answer from GitHub ({e})"))?;
    let list = list.as_array().ok_or("unexpected answer from GitHub")?;
    let mut best: Option<(Release, Version, u32)> = None;
    for item in list {
        if item["draft"].as_bool().unwrap_or(false) || item["prerelease"].as_bool().unwrap_or(false) {
            continue;
        }
        let Some((version, date)) = item["tag_name"].as_str().and_then(tag_version) else { continue };
        if !version.pre.is_empty() {
            continue;
        }
        let Ok(mut release) = craft_update::parse_github_release(&item.to_string(), repo) else { continue };
        if release.sums_asset().is_none() {
            continue;
        }
        release.version = Some(version.clone());
        let newer = best.as_ref().is_none_or(|(_, v, d)| match version.cmp_precedence(v) {
            std::cmp::Ordering::Equal => date > *d,
            order => order == std::cmp::Ordering::Greater,
        });
        if newer {
            best = Some((release, version, date));
        }
    }
    let Some((mut release, version, _)) = best else { return Ok(None) };
    let Some(running) = craft_update::version::parse(running) else { return Ok(None) };
    if !craft_update::version::offers_update(&version, &running) {
        return Ok(None);
    }
    let sums_url = release.sums_asset().map(|a| a.url.clone()).unwrap_or_default();
    let text = sums(&sums_url)?.ok_or_else(|| format!("{} publishes no SHA256SUMS.txt", release.tag))?;
    let listed = craft_update::release::parse_sha256sums(&text);
    for asset in release.assets.iter_mut().filter(|a| a.name != "SHA256SUMS.txt") {
        let Some((hash, _)) = listed.iter().find(|(_, name)| *name == asset.name) else { continue };
        if asset.sha256.as_ref().is_some_and(|digest| digest != hash) {
            return Err(format!("{} in {} doesn't match its line in SHA256SUMS.txt", asset.name, release.tag));
        }
        asset.sha256 = Some(hash.clone());
    }
    release.assets.retain(|a| a.name == "SHA256SUMS.txt" || listed.iter().any(|(_, name)| *name == a.name));
    Ok(Some(release))
}

/// The update check: the fork's release list on GitHub, read by [`offered_release`] for the
/// version this copy counts as (the `*_PRETEND_VERSION` override included).
#[must_use]
pub fn source() -> craft_update::updater::CheckSource {
    std::sync::Arc::new(|| {
        let app = &crate::UPDATE_APP;
        let running = craft_update::self_update::current_version(app, env!("CARGO_PKG_VERSION"))
            .map_or_else(|| env!("CARGO_PKG_VERSION").to_string(), |v| v.to_string());
        let http = craft_update::Http::new(&format!("{}/Update", app.name))?;
        let url = format!("https://api.github.com/repos/{}/releases?per_page=30", app.repo);
        match http.get_api(&url, None)? {
            craft_update::http::Fetched::Body { text, .. } => offered_release(&text, app.repo, &running, |sums| http.get_text(sums)),
            craft_update::http::Fetched::RateLimited { .. } => Err("GitHub's request limit was reached".into()),
            craft_update::http::Fetched::NotModified | craft_update::http::Fetched::NotFound => Ok(None),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPO: &str = "miqbalf/wordcraft";
    const HASH: &str = "8a5edab282632443219e051e4ade2d1d5bbc671c781051bf1437897cbdfea0f1";

    /// One release in GitHub's list, with the files the udinCraft release workflow publishes.
    fn item(tag: &str, version: &str, extra: &str) -> String {
        let dl = format!("https://github.com/{REPO}/releases/download/{tag}");
        format!(
            r#"{{"tag_name":"{tag}","name":"{tag}","html_url":"https://github.com/{REPO}/releases/tag/{tag}","draft":false,"prerelease":false{extra},
            "assets":[{{"name":"wordcraft-{version}-macos-universal.dmg","size":1,"browser_download_url":"{dl}/wordcraft-{version}-macos-universal.dmg","digest":"sha256:{HASH}"}},
                      {{"name":"wordcraft-{version}-linux-x86_64.AppImage","size":1,"browser_download_url":"{dl}/wordcraft-{version}-linux-x86_64.AppImage"}},
                      {{"name":"wordcraft-{version}-linux-x86_64.AppImage.zsync","size":1,"browser_download_url":"{dl}/wordcraft-{version}-linux-x86_64.AppImage.zsync"}},
                      {{"name":"SHA256SUMS.txt","size":1,"browser_download_url":"{dl}/SHA256SUMS.txt"}}]}}"#
        )
    }

    /// A sums file that lists the DMG and the AppImage of `version` (not the zsync file).
    fn sums(version: &str) -> String {
        format!("{HASH}  wordcraft-{version}-macos-universal.dmg\n{}  wordcraft-{version}-linux-x86_64.AppImage\n", "1".repeat(64))
    }

    fn offered(json: &str, running: &str) -> Option<String> {
        offered_release(json, REPO, running, |_| Ok(Some(sums("9.1.0")))).unwrap().map(|r| r.tag)
    }

    #[test]
    fn reads_udincraft_tags_only() {
        assert_eq!(tag_version("udincraft-v0.18.1-20261127"), Some((Version::new(0, 18, 1), 20_261_127)));
        for bad in ["v0.19.1", "udincraft-v0.18.1", "udincraft-v0.18.1-20261327", "udincraft-v0.18-20261127", "udincraft-0.18.1-20261127"] {
            assert_eq!(tag_version(bad), None, "{bad}");
        }
    }

    #[test]
    fn the_fork_offers_only_its_newest_udincraft_release() {
        let json = format!(
            "[{},{},{},{},{}]",
            item("v99.0.0", "99.0.0", ""),
            item("udincraft-v9.1.0-20261128", "9.1.0", ""),
            item("udincraft-v9.1.0-20261127", "9.1.0", ""),
            item("udincraft-v9.2.0-20261130", "9.2.0", r#","draft":true"#),
            item("udincraft-v9.3.0-rc.1-20261130", "9.3.0-rc.1", r#","prerelease":true"#),
        );
        // The newest stable udinCraft build; a same-version rebuild wins on its date.
        assert_eq!(offered(&json, "0.1.0").as_deref(), Some("udincraft-v9.1.0-20261128"));
        // Already running it, or something newer: nothing to offer.
        assert_eq!(offered(&json, "9.1.0"), None);
        assert_eq!(offered(&json, "9.2.0"), None);
        // An upstream-style release alone is never offered, and no release yet is no update.
        assert_eq!(offered(&format!("[{}]", item("v99.0.0", "99.0.0", "")), "0.1.0"), None);
        assert_eq!(offered("[]", "0.1.0"), None);
        assert!(offered_release("{}", REPO, "0.1.0", |_| Ok(None)).is_err());
    }

    #[test]
    fn the_offered_files_are_the_ones_its_sums_file_vouches_for() {
        let json = format!("[{}]", item("udincraft-v9.1.0-20261128", "9.1.0", ""));
        let mut asked = String::new();
        let r = offered_release(&json, REPO, "0.1.0", |url| {
            asked = url.to_string();
            Ok(Some(sums("9.1.0")))
        })
        .unwrap()
        .unwrap();
        assert_eq!(asked, format!("https://github.com/{REPO}/releases/download/udincraft-v9.1.0-20261128/SHA256SUMS.txt"));
        assert_eq!(r.version, Some(Version::new(9, 1, 0)));
        let names: Vec<_> = r.assets.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(
            names,
            ["wordcraft-9.1.0-macos-universal.dmg", "wordcraft-9.1.0-linux-x86_64.AppImage", "SHA256SUMS.txt"],
            "the unlisted zsync is dropped"
        );
        assert_eq!(r.assets[0].sha256.as_deref(), Some(HASH));
        assert_eq!(r.assets[1].sha256, Some("1".repeat(64)), "a file without a GitHub digest takes the list's");
        assert!(r.assets.iter().all(|a| a.url.starts_with(&format!("https://github.com/{REPO}/releases/download/"))));
        // A digest that disagrees with the list, or no list at all, offers nothing installable.
        let wrong = format!("{}  wordcraft-9.1.0-macos-universal.dmg\n", "2".repeat(64));
        assert!(offered_release(&json, REPO, "0.1.0", |_| Ok(Some(wrong))).unwrap_err().contains("doesn't match"));
        assert!(offered_release(&json, REPO, "0.1.0", |_| Ok(None)).unwrap_err().contains("no SHA256SUMS.txt"));
        assert!(offered_release(&json, REPO, "0.1.0", |_| Err("offline".into())).unwrap_err().contains("offline"));
    }

    #[test]
    fn the_update_windows_name_the_fork() {
        assert_eq!(crate::UPDATE_APP.developer, "udinCraft");
        assert_eq!(crate::UPDATE_APP.repo, REPO);
    }

    /// Read-only, against the real GitHub: `cargo test -p wordcraft-ui-egui --lib -- --ignored live_`.
    /// The fork answers with its udinCraft release or, before the first one, with no update.
    #[test]
    #[ignore = "asks the real GitHub API"]
    fn live_the_fork_answers_with_its_release_or_none_yet() {
        match (source())() {
            Ok(None) => println!("{REPO}: no udinCraft release newer than this build yet"),
            Ok(Some(r)) => {
                println!("{REPO}: {}", r.tag);
                assert!(tag_version(&r.tag).is_some(), "{}", r.tag);
            }
            Err(why) => panic!("{REPO}: {why}"),
        }
    }
}
