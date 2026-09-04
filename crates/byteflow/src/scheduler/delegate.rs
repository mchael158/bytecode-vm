//! Opcode DELEGATE — attenuation as a first-class ISA operation.
//!
//! `DELEGATE dst, src_cap, rights_mask, native_mask_cap?`
//!
//! The entire semantics are guaranteed inside [`Cap::attenuate`]: the result
//! is `min(src.rights, rights_mask)` — bitwise AND. Hostile operands cannot
//! escalate because there is no separate "grant"; only intersection.

use crate::bytecode::{Cap, CapRights, NativeMask, RevocationCell};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DelegateError {
    SourceRevoked,
    /// Caller asked to narrow a native mask but `src` has no `NATIVE` right.
    SourceLacksNative,
}

impl std::fmt::Display for DelegateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DelegateError::SourceRevoked => f.write_str("delegate: source capability revoked"),
            DelegateError::SourceLacksNative => {
                f.write_str("delegate: source lacks NATIVE right")
            }
        }
    }
}

impl std::error::Error for DelegateError {}

pub fn exec_delegate(
    src: &Cap,
    src_cell: &RevocationCell,
    want_rights: CapRights,
    want_native: Option<&NativeMask>,
) -> Result<Cap, DelegateError> {
    if !src.is_valid(src_cell) {
        return Err(DelegateError::SourceRevoked);
    }
    if want_native.is_some() && !src.rights.contains(CapRights::NATIVE) {
        return Err(DelegateError::SourceLacksNative);
    }
    Ok(src.attenuate(want_rights, want_native))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytecode::{CapTarget, RevocationCell};

    #[test]
    fn cannot_escalate_via_delegate_regardless_of_requested_mask() {
        let cell = RevocationCell::new();
        let src = Cap::root(CapTarget::Flow(1), CapRights::SEND, None, &cell);
        let out = match exec_delegate(
            &src,
            &cell,
            CapRights::SEND.union(CapRights::ADMIN).union(CapRights::NATIVE),
            None,
        ) {
            Ok(c) => c,
            Err(_) => {
                assert!(false, "delegate of live SEND cap must succeed");
                return;
            }
        };
        assert!(out.rights.contains(CapRights::SEND));
        assert!(!out.rights.contains(CapRights::ADMIN));
        assert!(!out.rights.contains(CapRights::NATIVE));
    }

    #[test]
    fn revoked_source_cannot_delegate() {
        let cell = RevocationCell::new();
        let src = Cap::root(CapTarget::Flow(1), CapRights::SEND, None, &cell);
        cell.revoke();
        assert!(matches!(
            exec_delegate(&src, &cell, CapRights::SEND, None),
            Err(DelegateError::SourceRevoked)
        ));
    }
}
