# Live co-editing: dependency licences

The licence check for the dependencies live co-editing added (AGENTS.md rule 26). It covers
every package that is new in `Cargo.lock` since `49171f11`, the base of `feat/live-coediting`,
and the direct dependency edges the branch added. Licences come from `cargo metadata --locked`
(2026-11-21). They were checked against the allowed list in
[storytold/craft-contrib `standards/licences.md`](https://github.com/storytold/craft-contrib/blob/main/standards/licences.md).
That list allows an expression only when every identifier in it is on the list. The older
`MIT/Apache-2.0` form means `MIT OR Apache-2.0`.

**Result: every licence is on the list.** Nothing was removed. The flags are two unmaintained
crates and the git dependencies from the miqbalf forks; see [Flags](#flags).

## Packages new in Cargo.lock

| Package | Version | Licence | On the list | Pulled in by |
|---|---|---|---|---|
| adler32 | 1.2.0 | Zlib | yes | yrs |
| arc-swap | 1.9.5 | MIT OR Apache-2.0 | yes | yrs |
| async-stream, async-stream-impl | 0.3.6 | MIT | yes | rust_socketio (craft-collab `native-hub`) |
| axum, axum-core | 0.8.9, 0.5.6 | MIT | yes | wordcraft-cli (worker notify routes) |
| backoff | 0.4.0 | MIT OR Apache-2.0 | yes (**unmaintained**) | rust_engineio |
| base64 | 0.21.7 (another version) | MIT OR Apache-2.0 | yes | rust_engineio |
| byteorder | 1.5.0 | Unlicense OR MIT | yes | rust_engineio |
| craft-collab | 0.1.0 (git) | MIT OR Apache-2.0 | yes (**git, miqbalf**) | wordcraft-live, -cli, -ui-egui, -web |
| craft-web-host | 0.1.0 (git, new rev) | MIT OR Apache-2.0 | yes (**git, miqbalf**) | wordcraft-web |
| dashmap | 6.2.1 | MIT | yes | yrs |
| data-encoding | 2.11.1 | MIT | yes | tungstenite |
| foreign-types, foreign-types-shared | 0.3.2, 0.1.1 (other versions) | MIT OR Apache-2.0 | yes | openssl |
| hashbrown | 0.14.5 (another version) | MIT OR Apache-2.0 | yes | dashmap |
| httpdate | 1.0.3 | MIT OR Apache-2.0 | yes | hyper (server) |
| hyper-tls | 0.6.0 | MIT OR Apache-2.0 | yes | reqwest `native-tls` |
| instant | 0.1.13 | BSD-3-Clause | yes (**unmaintained**) | backoff |
| matchit | 0.8.4 | MIT AND BSD-3-Clause | yes (both are on the list) | axum |
| native-tls | 0.2.18 | MIT OR Apache-2.0 | yes | reqwest, rust_engineio |
| openssl | 0.10.82 | Apache-2.0 | yes | native-tls (Linux) |
| openssl-macros | 0.1.1 | MIT OR Apache-2.0 | yes | openssl |
| openssl-sys | 0.9.118 | MIT | yes | openssl |
| ppv-lite86 | 0.2.21 | MIT OR Apache-2.0 | yes | rand_chacha |
| rand | 0.8.8 (another version) | MIT OR Apache-2.0 | yes | tungstenite, rust_engineio |
| rand_chacha | 0.3.1 | MIT OR Apache-2.0 | yes | rand 0.8 |
| reqwest | 0.12.28 (another version) | MIT OR Apache-2.0 | yes | wordcraft-cli (worker file routes), rust_engineio |
| rust_engineio, rust_socketio | 0.6.0 | MIT | yes | craft-collab `native-hub` |
| smallstr | 0.3.1 | MIT OR Apache-2.0 | yes | yrs |
| tokio-macros | 2.7.2 | MIT | yes | tokio `macros` (tests) |
| tokio-native-tls | 0.3.1 | MIT | yes | hyper-tls |
| tokio-tungstenite, tungstenite | 0.21.0 | MIT; MIT OR Apache-2.0 | yes | rust_engineio |
| utf-8 | 0.7.6 | MIT OR Apache-2.0 | yes | tungstenite |
| vcpkg | 0.2.15 | MIT OR Apache-2.0 | yes | openssl-sys (build, Windows) |
| wasm-streams | 0.4.2 | MIT OR Apache-2.0 | yes | reqwest (wasm) |
| yrs | 0.28.2 | MIT | yes | craft-collab |
| wordcraft-live | 0.18.1 (path) | MIT OR Apache-2.0 | yes | ours |

## Direct dependency edges added (packages already in the lock)

| Dependency | Licence | Used by |
|---|---|---|
| tokio 1.53 | MIT | wordcraft-cli (worker), wordcraft-live (dev) |
| hyper 1.12, hyper-util 0.1.21 | MIT | wordcraft-cli (notify listener) |
| hmac 0.13, sha2 0.11 | MIT OR Apache-2.0 | wordcraft-cli (signatures, tickets), wordcraft-live (dev, test tickets) |
| base64 0.23 | MIT OR Apache-2.0 | wordcraft-cli, wordcraft-web (ticket payload), wordcraft-live (dev) |
| getrandom, chrono, serde, log | MIT OR Apache-2.0 | wordcraft-cli |
| sha1 | MIT OR Apache-2.0 | wordcraft-cli (dev: the fake Nextcloud's ETags) |

## Flags

- **Unmaintained: `backoff` 0.4.0 and `instant` 0.1.13.** RustSec lists both as unmaintained
  (informational advisories, not vulnerabilities). They reach the build only through
  `rust_engineio`, the Socket.IO client under craft-collab's `native-hub` feature. Only the
  native worker (`wordcraft-cli live-worker`) and the relay tests use that feature; the browser
  and desktop windows do not. The fix belongs in craft-collab: a maintained client, or
  `rust_engineio` dropping `backoff`.
- **Git dependencies outside storytold (rule 10).** `craft-collab` comes from
  `miqbalf/collab-craft` (pinned by rev, workspace `Cargo.toml`). `craft-web-host` comes from
  `miqbalf/craft-libs` `feat/web-host-live` (pinned by rev, `apps/wordcraft-web/Cargo.toml`),
  while the other craft-libs crates stay on storytold `93dac78c`. Rule 10 says git dependencies
  SHOULD come only from storytold, with all craft-libs crates at one merged rev. This is a
  deliberate departure for a personal phase 1 branch. The owner must decide before anything
  goes upstream: merge both forks' branches into storytold, then move the pins back to one
  storytold rev, in their own PR.
- **openssl (Apache-2.0) and the system OpenSSL.** `native-tls` links the platform TLS library:
  OpenSSL on Linux. The crate's licence is on the list. The linked library is the system's own
  and is not part of the worker binary. The worker image (craft-office-nextcloud
  `dev/worker.Dockerfile`) installs it from Ubuntu as `libssl3t64` (OpenSSL 3, Apache-2.0, on the
  list), so that image ships it. NOTICE's general line on permissively licensed
  dependencies (MIT/Apache-2.0/BSD/ISC/Zlib) covers the crates above, Zlib, BSD-3-Clause and
  Unlicense-or-MIT included.
