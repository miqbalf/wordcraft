//! An in-memory relay for host tests (feature `test-util`): the interface's tests join a real
//! [`LiveEditing`](crate::LiveEditing) through it, with no socket and no server.
//!
//! It does what a host test needs and no more: numbers every submit and broadcasts it to the
//! room (sender included), refuses edits from a read-only connection with the relay's wording,
//! grants each join a fresh client space, replays the ops so far to a joiner, keeps the newest
//! ephemeral (caret) per peer and kind, and can drop and restore a connection. It does not save:
//! the crate's own pump tests cover the saver. Not for production use.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use craft_collab::{ClientMsg, Hub, Presence, ServerMsg};
use serde_json::Value;

/// The room every connection of a [`MemRelay`] is in.
pub const ROOM: &str = "test:room:1";

struct Conn {
    pid: String,
    display: String,
    may_edit: bool,
    connected: bool,
    inbox: VecDeque<ServerMsg>,
    /// The tickets the host handed this connection with [`Hub::set_token`], oldest first.
    tokens: Vec<String>,
}

#[derive(Default)]
struct Relay {
    seq: u64,
    log: Vec<ServerMsg>,
    conns: Vec<Conn>,
    next_space: u32,
    ephemeral: Vec<(String, String, Value)>,
}

/// The relay, shared by its hubs.
#[derive(Clone, Default)]
pub struct MemRelay(Rc<RefCell<Relay>>);

/// One connection to a [`MemRelay`].
#[derive(Clone)]
pub struct MemHub {
    relay: MemRelay,
    at: usize,
}

impl MemRelay {
    pub fn new() -> MemRelay {
        MemRelay::default()
    }

    /// Connect `uid` (shown as `display`); `may_edit` false is a read ticket. The relay joins
    /// it at once.
    pub fn connect(&self, uid: &str, display: &str, may_edit: bool) -> MemHub {
        let mut r = self.0.borrow_mut();
        let at = r.conns.len();
        r.conns.push(Conn {
            pid: format!("{uid}~{at:08x}"),
            display: display.to_owned(),
            may_edit,
            connected: true,
            inbox: VecDeque::new(),
            tokens: Vec::new(),
        });
        r.join(at);
        MemHub { relay: self.clone(), at }
    }

    /// The link of `hub` drops, as a network outage: nothing reaches it or comes from it.
    pub fn drop_link(&self, hub: &MemHub) {
        let mut r = self.0.borrow_mut();
        if let Some(c) = r.conns.get_mut(hub.at) {
            c.connected = false;
            c.inbox.clear();
        }
        let presence = ServerMsg::Presence { doc: ROOM.into(), roster: r.roster() };
        r.broadcast(&presence, None);
    }

    /// The link of `hub` comes back and rejoins, as the hub's own reconnect does.
    pub fn restore_link(&self, hub: &MemHub) {
        let mut r = self.0.borrow_mut();
        if let Some(c) = r.conns.get_mut(hub.at) {
            c.connected = true;
        }
        r.join(hub.at);
    }

    /// The relay refuses the connection of `hub` itself (`code` as `auth` for an expired
    /// ticket), with `detail` in its words. The link stays up until [`MemRelay::drop_link`].
    pub fn refuse(&self, hub: &MemHub, code: &str, detail: &str) {
        if let Some(c) = self.0.borrow_mut().conns.get_mut(hub.at) {
            c.inbox.push_back(ServerMsg::Error { doc: ROOM.into(), detail: detail.into(), local: None, code: Some(code.into()) });
        }
    }

    /// The relay's sequence: the last op it ordered.
    pub fn seq(&self) -> u64 {
        self.0.borrow().seq
    }

    /// The relay gives up catching `hub` up and sends the room's snapshot instead (a client
    /// that fell too far behind), which breaks a window's link.
    pub fn fall_behind(&self, hub: &MemHub) {
        let mut r = self.0.borrow_mut();
        let seq = r.seq;
        if let Some(c) = r.conns.get_mut(hub.at) {
            c.inbox.push_back(ServerMsg::Snapshot { doc: ROOM.into(), seq, document: Value::Null });
        }
    }

    /// The tickets the host handed `hub` with [`Hub::set_token`], oldest first.
    pub fn tokens(&self, hub: &MemHub) -> Vec<String> {
        self.0.borrow().conns.get(hub.at).map(|c| c.tokens.clone()).unwrap_or_default()
    }

    /// Nothing is waiting for any connected peer.
    pub fn quiet(&self) -> bool {
        self.0.borrow().conns.iter().all(|c| c.inbox.is_empty() || !c.connected)
    }
}

impl Hub for MemHub {
    fn send(&mut self, msg: ClientMsg) -> craft_collab::Result<()> {
        self.relay.0.borrow_mut().handle(self.at, msg);
        Ok(())
    }

    fn poll(&mut self) -> Vec<ServerMsg> {
        let mut r = self.relay.0.borrow_mut();
        match r.conns.get_mut(self.at) {
            Some(c) if c.connected => c.inbox.drain(..).collect(),
            _ => Vec::new(),
        }
    }

    fn connected(&self) -> bool {
        self.relay.0.borrow().conns.get(self.at).is_some_and(|c| c.connected)
    }

    fn set_token(&mut self, token: String) {
        if let Some(c) = self.relay.0.borrow_mut().conns.get_mut(self.at) {
            c.tokens.push(token);
        }
    }
}

impl Relay {
    fn roster(&self) -> Vec<Presence> {
        self.conns.iter().filter(|c| c.connected).map(|c| Presence { peer: c.pid.clone(), display: c.display.clone() }).collect()
    }

    fn join(&mut self, at: usize) {
        self.next_space += 1;
        let space = self.next_space;
        let roster = self.roster();
        let Some(pid) = self.conns.get(at).map(|c| c.pid.clone()) else { return };
        let joined = ServerMsg::Joined {
            doc: ROOM.into(),
            peer: pid.clone(),
            seq: 0,
            space,
            roster: roster.clone(),
            locks: Vec::new(),
            replay_until: Some(self.seq),
            fallback: false,
            text_cap: None,
            heartbeat: false,
        };
        let replay = self.log.clone();
        let others: Vec<ServerMsg> = self
            .ephemeral
            .iter()
            .filter(|(o, _, _)| *o != pid)
            .map(|(origin, kind, payload)| ServerMsg::Ephemeral {
                doc: ROOM.into(),
                origin: origin.clone(),
                kind: kind.clone(),
                payload: payload.clone(),
            })
            .collect();
        if let Some(conn) = self.conns.get_mut(at) {
            conn.inbox.push_back(joined);
            conn.inbox.extend(replay);
            conn.inbox.extend(others);
        }
        let presence = ServerMsg::Presence { doc: ROOM.into(), roster };
        self.broadcast(&presence, Some(at));
    }

    fn broadcast(&mut self, msg: &ServerMsg, except: Option<usize>) {
        for (j, c) in self.conns.iter_mut().enumerate() {
            if Some(j) != except && c.connected {
                c.inbox.push_back(msg.clone());
            }
        }
    }

    fn handle(&mut self, at: usize, msg: ClientMsg) {
        let Some(conn) = self.conns.get_mut(at).filter(|c| c.connected) else { return };
        match msg {
            ClientMsg::Submit { local, command, params, .. } => {
                if !conn.may_edit {
                    conn.inbox.push_back(ServerMsg::Error {
                        doc: ROOM.into(),
                        detail: "read-only access to this file".into(),
                        local: Some(local),
                        code: None,
                    });
                    return;
                }
                let origin = conn.pid.clone();
                self.seq += 1;
                let op = ServerMsg::Op { doc: ROOM.into(), seq: self.seq, origin, local: Some(local), command, params, scope: None };
                self.log.push(op.clone());
                self.broadcast(&op, None);
            }
            ClientMsg::Ephemeral { kind, payload, .. } => {
                let origin = conn.pid.clone();
                self.ephemeral.retain(|(o, k, _)| !(*o == origin && *k == kind));
                self.ephemeral.push((origin.clone(), kind.clone(), payload.clone()));
                self.broadcast(&ServerMsg::Ephemeral { doc: ROOM.into(), origin, kind, payload }, Some(at));
            }
            _ => {}
        }
    }
}
