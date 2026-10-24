//! Traces: how a key's value was computed.

use crate::keys::KeyId;
use skycodec::{Decode, DecodeError, Decoder, Encode, Encoder};
use skystore::ValueRef;

/// The result was an error.
pub(crate) const ERROR: u8 = 1;

/// The result mustn't be reused by a later evaluation, because it was a
/// transient error or depended on one.
pub(crate) const TRANSIENT: u8 = 2;

/// A record of one computation of a key: the value it produced and, in the
/// order they were asked for, the dependencies it read and their values.
///
/// A trace is still valid wherever its dependencies have the same values,
/// since a key's value depends only on those. Each dependency group was asked
/// for knowing only the values of the groups before it, so checking a trace
/// group by group asks for exactly the keys the computation would.
///
/// Traces are stored, compressed and deduplicated, in the value store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Trace {
    pub key: KeyId,
    pub flags: u8,
    pub value: ValueRef,
    pub deps: Vec<Dep>,
    /// Where each group of `deps` ends.
    pub group_ends: Vec<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Dep {
    pub key: KeyId,
    pub value: ValueRef,
}

impl Trace {
    pub(crate) fn groups(&self) -> impl Iterator<Item = &[Dep]> {
        let mut start = 0;
        self.group_ends.iter().map(move |&end| {
            let group = &self.deps[start..end as usize];
            start = end as usize;
            group
        })
    }
}

impl Encode for Trace {
    fn encode(&self, e: &mut Encoder) {
        e.u32(self.key.0);
        e.u8(self.flags);
        e.u32(self.value.id());
        e.usize(self.group_ends.len());
        for group in self.groups() {
            e.usize(group.len());
            for dep in group {
                e.u32(dep.key.0);
                e.u32(dep.value.id());
            }
        }
    }
}

impl Decode for Trace {
    fn decode(d: &mut Decoder<'_>) -> Result<Trace, DecodeError> {
        let key = KeyId(d.u32()?);
        let flags = d.u8()?;
        let value = ValueRef::from_id(d.u32()?);
        let groups = d.usize()?;
        let mut deps = Vec::new();
        let mut group_ends = Vec::with_capacity(groups.min(1024));
        for _ in 0..groups {
            let len = d.usize()?;
            for _ in 0..len {
                let key = KeyId(d.u32()?);
                let value = ValueRef::from_id(d.u32()?);
                deps.push(Dep { key, value });
            }
            group_ends.push(deps.len() as u32);
        }
        Ok(Trace {
            key,
            flags,
            value,
            deps,
            group_ends,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn traces_round_trip() {
        let dep = |key, value| Dep {
            key: KeyId(key),
            value: ValueRef::from_id(value),
        };
        let trace = Trace {
            key: KeyId(7),
            flags: ERROR,
            value: ValueRef::from_id(99),
            deps: vec![dep(1, 2), dep(3, 4), dep(5, 6)],
            group_ends: vec![1, 3],
        };
        let bytes = skycodec::to_bytes(&trace);
        assert_eq!(skycodec::from_bytes::<Trace>(&bytes).unwrap(), trace);
        let groups: Vec<usize> = trace.groups().map(<[Dep]>::len).collect();
        assert_eq!(groups, [1, 2]);
    }
}
