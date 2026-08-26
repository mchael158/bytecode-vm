//! Unforgeable flow capabilities (security phase 2 — FlowCap).
//!
//! # Why Caps exist
//!
//! After authenticated sender (S1), bytecode still needed an *address* to
//! deliver hops. Using raw [`FlowId`] / [`crate::Value::Pid`] as that address
//! meant any module that could learn or guess a numeric id could talk to
//! that flow — Pid was an ambient authority token.
//!
//! A [`CapId`] is an opaque handle minted only by the runtime. Guessing the
//! next integer does not grant `Send` / `Ask`: resolution goes through
//! [`CapTable`] under the same fail-closed mutex policy as the rest of the
//! scheduler. Rights are attenuated at mint time (`SEND`, `ASK`, or both).
//!
//! # Split with Pid
//!
//! | Value | Role |
//! |-------|------|
//! | [`crate::Value::Cap`] | Address for bytecode `Send` / `Ask` |
//! | [`crate::Value::Pid`] | Identity inside `Message.sender` / `msg_sender` |
//!
//! Reply path: stamped hops carry `Message.reply_cap` (SEND-only Cap back to
//! the sender). Receivers answer with `msg_reply_cap`, never by treating
//! `msg_sender` as a delivery address.
//!
//! See `docs/security.md` (S6) and `docs/atomic-hop.md`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use super::error::RuntimeError;
use super::process::FlowId;
use super::sync_lock;

/// Opaque capability identifier carried in [`crate::Value::Cap`].
///
/// Never equal to a [`FlowId`] by construction (independent counter). Do not
/// compare CapIds to FlowIds; Ask correlation uses the **resolved** FlowId.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CapId(pub(crate) u64);

impl CapId {
    #[inline]
    pub fn as_u64(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for CapId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "cap#{}", self.0)
    }
}

/// Cap ids start at 1 so `0` can mean “no grant” on `Message.reply_cap`
/// (host-injected hops / unauthenticated `make_msg` placeholders).
static NEXT_CAP_ID: AtomicU64 = AtomicU64::new(1);

/// Rights attached to a capability (bitflags as `u8`).
///
/// Attenuation is mint-time only in this revision: there is no runtime
/// `attenuate()` that narrows an existing Cap in place. Mint a new Cap with
/// fewer bits instead (e.g. reply grants use [`CapRights::SEND`] alone).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CapRights(u8);

impl CapRights {
    pub const SEND: CapRights = CapRights(0b01);
    pub const ASK: CapRights = CapRights(0b10);
    pub const SEND_ASK: CapRights = CapRights(0b11);

    #[inline]
    pub fn contains(self, other: CapRights) -> bool {
        self.0 & other.0 == other.0
    }

    #[inline]
    pub fn bits(self) -> u8 {
        self.0
    }
}

/// One live capability entry.
#[derive(Clone, Copy, Debug)]
pub struct CapEntry {
    /// Flow this Cap authorizes delivery to.
    pub flow: FlowId,
    pub rights: CapRights,
}

/// Registry `CapId → { FlowId, CapRights }`.
///
/// Lives on [`super::runtime::Shared`] next to [`super::directory::Directory`]:
/// Directory answers “where is this flow’s mailbox?”; CapTable answers
/// “does this Cap authorize Send/Ask to some flow?”.
///
/// On flow exit the worker calls [`CapTable::revoke_target`] so Caps that
/// pointed at the dead flow stop resolving (fail-closed delivery).
pub struct CapTable {
    inner: Mutex<HashMap<CapId, CapEntry>>,
}

impl CapTable {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// Mint a new capability targeting `flow` with `rights`.
    ///
    /// Used for: self Cap (`SelfPid`), child Cap (`Spawn`), and per-hop
    /// `reply_cap` (SEND-only back to the sender).
    pub fn mint(&self, flow: FlowId, rights: CapRights) -> Result<CapId, RuntimeError> {
        let id = CapId(NEXT_CAP_ID.fetch_add(1, Ordering::Relaxed));
        sync_lock::lock(&self.inner, "CapTable::mint")?.insert(
            id,
            CapEntry { flow, rights },
        );
        Ok(id)
    }

    /// Resolve a capability. `None` if unknown / revoked.
    pub fn resolve(&self, id: CapId) -> Result<Option<CapEntry>, RuntimeError> {
        Ok(sync_lock::lock(&self.inner, "CapTable::resolve")?
            .get(&id)
            .copied())
    }

    /// Drop every capability whose **target** is `flow` (flow exited).
    ///
    /// Caps *held by* other flows that pointed here become dead; Caps this
    /// flow held to *other* targets are not swept here (they die when those
    /// targets exit, or leak until then — acceptable for the MVP table).
    pub fn revoke_target(&self, flow: FlowId) -> Result<(), RuntimeError> {
        let mut g = sync_lock::lock(&self.inner, "CapTable::revoke_target")?;
        g.retain(|_, e| e.flow != flow);
        Ok(())
    }
}

impl Default for CapTable {
    fn default() -> Self {
        Self::new()
    }
}
