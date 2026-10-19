//! Dusk Multicall batches read-only contract calls into one contract query.
//!
//! `aggregate` takes a list of [`Call`]s and returns an [`Aggregate`]: the
//! height of the block whose state the query ran on and, in call order, each
//! call's reply bytes or its error. All calls run inside that one query, so
//! every reply comes from the same state, and a failing call never stops the
//! others. The contract holds no state and refuses to run unless a query calls
//! it directly. The README covers the ABI, the limits and the gas behaviour.
#![cfg_attr(all(target_family = "wasm", not(feature = "data-driver")), no_std)]

extern crate alloc;
// The Forge module names this crate's types by absolute paths.
extern crate self as dusk_multicall;

use alloc::string::String;
use alloc::vec::Vec;
use core::mem::{align_of, size_of};

use bytecheck::CheckBytes;
pub use piecrust_uplink::{ARGBUF_LEN, ContractId};
use rkyv::string::repr::INLINE_CAPACITY;
use rkyv::{Archive, Archived, Deserialize, Serialize};
#[cfg(feature = "serde")]
use serde_with::{As, DisplayFromStr, PickFirst, Same, hex::Hex};

/// The ABI version `version` returns.
pub const VERSION: u32 = 1;

/// The most calls one `aggregate` takes.
pub const MAX_CALLS: usize = 256;

/// The most bytes of a panic message a result keeps. With this cap, the
/// results of 256 failed calls always fit in the reply.
pub const MAX_PANIC_BYTES: usize = 200;

/// One call in a batch.
#[derive(Debug, Clone, PartialEq, Eq, Archive, Serialize, Deserialize)]
#[archive_attr(derive(CheckBytes))]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(deny_unknown_fields)
)]
pub struct Call {
    /// The contract to call.
    pub contract: ContractId,
    /// The function to call.
    pub fn_name: String,
    /// The argument, in the callee's own encoding.
    #[cfg_attr(feature = "serde", serde(with = "As::<Hex>"))]
    pub fn_args: Vec<u8>,
    /// The most gas the call may spend, or `0` for a [`fair_share`]. A limit
    /// at or above the gas left gets the VM's default instead, 93% of the gas
    /// left. A failed call is charged its whole limit.
    #[cfg_attr(feature = "serde", serde(with = "As::<PickFirst<(DisplayFromStr, Same)>>"))]
    pub gas_limit: u64,
}

/// The gas limit a call with cap `0` gets: the gas left when it starts,
/// divided by the calls left, itself included, plus one. A failing call then
/// takes at most its share, so it can't starve the calls after it, and one
/// share always stays back for encoding the reply. The VM reads a limit of `0`
/// as its default, so a share that rounds down to `0` is `1`, and the call
/// runs out of gas.
#[must_use]
pub fn fair_share(gas_left: u64, calls_left: usize) -> u64 {
    (gas_left / (calls_left as u64).saturating_add(1)).max(1)
}

/// What `aggregate` returns.
#[derive(Debug, Clone, PartialEq, Eq, Archive, Serialize, Deserialize)]
#[archive_attr(derive(CheckBytes))]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(deny_unknown_fields)
)]
pub struct Aggregate {
    /// The height of the block whose state the query ran on.
    #[cfg_attr(feature = "serde", serde(with = "As::<PickFirst<(DisplayFromStr, Same)>>"))]
    pub height: u64,
    /// One result per call, in call order: exactly the bytes the callee
    /// returned to the multicall, or why the call failed. For a view that
    /// doesn't branch on its caller, they are the bytes a direct query
    /// returns.
    #[with(ResolversOnHeap)]
    #[cfg_attr(feature = "serde", serde(with = "As::<Vec<Result<Hex, Same>>>"))]
    pub results: Vec<Result<Vec<u8>, CallError>>,
}

/// Why a call failed, as the VM reports it to the calling contract.
#[derive(Debug, Clone, PartialEq, Eq, Archive, Serialize, Deserialize)]
#[archive_attr(derive(CheckBytes))]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum CallError {
    /// The callee panicked with this message, cut to [`MAX_PANIC_BYTES`] at a
    /// character boundary.
    Panic(String),
    /// The callee ran out of gas.
    OutOfGas,
    /// No contract has this ID.
    DoesNotExist,
    /// Any other failure, such as a missing function or a trap.
    Unknown,
}

impl From<piecrust_uplink::ContractError> for CallError {
    fn from(error: piecrust_uplink::ContractError) -> Self {
        use piecrust_uplink::ContractError as E;
        match error {
            E::Panic(message) if message.len() <= MAX_PANIC_BYTES => Self::Panic(message),
            E::Panic(message) => {
                let mut end = MAX_PANIC_BYTES;
                while !message.is_char_boundary(end) {
                    end -= 1;
                }
                // A new string holding only the kept bytes; truncating in
                // place would keep the whole message allocated until the
                // reply is encoded.
                Self::Panic(String::from(&message[..end]))
            }
            E::OutOfGas => Self::OutOfGas,
            E::DoesNotExist => Self::DoesNotExist,
            E::Unknown => Self::Unknown,
        }
    }
}

/// The bytes a result adds to the reply beside its fixed slot: its reply, or
/// a panic message too long to sit inside the slot.
#[must_use]
pub fn out_of_line(result: &Result<Vec<u8>, CallError>) -> usize {
    match result {
        Ok(bytes) => bytes.len(),
        Err(CallError::Panic(message)) if message.len() > INLINE_CAPACITY => message.len(),
        Err(_) => 0,
    }
}

/// The length of the reply to `calls` calls whose results add `out_of_line`
/// bytes beside their slots. rkyv writes those bytes first, then the slots,
/// then the root, each aligned for its type.
#[must_use]
pub fn reply_len(calls: usize, out_of_line: usize) -> usize {
    type Slot = Archived<Result<Vec<u8>, CallError>>;
    let slots = out_of_line.next_multiple_of(align_of::<Slot>()) + calls * size_of::<Slot>();
    slots.next_multiple_of(align_of::<ArchivedAggregate>()) + size_of::<ArchivedAggregate>()
}

/// Archives a `Vec` exactly as rkyv does, but keeps the resolvers of its
/// elements in a heap `Vec` instead of the serializer's scratch space. The
/// exports encode a reply as uplink's `wrap_call` does, with a fixed 1 KiB of
/// scratch space: too small for the resolvers of a few hundred results.
#[derive(Debug)]
pub struct ResolversOnHeap;

impl<T: Archive> rkyv::with::ArchiveWith<Vec<T>> for ResolversOnHeap {
    type Archived = rkyv::vec::ArchivedVec<T::Archived>;
    type Resolver = rkyv::vec::VecResolver;

    unsafe fn resolve_with(
        field: &Vec<T>,
        pos: usize,
        resolver: Self::Resolver,
        out: *mut Self::Archived,
    ) {
        // SAFETY: the caller's guarantees for `pos` and `out` carry over, and
        // `resolver` comes from `serialize_with` on this field.
        unsafe { rkyv::vec::ArchivedVec::resolve_from_len(field.len(), pos, resolver, out) }
    }
}

impl<T, S> rkyv::with::SerializeWith<Vec<T>, S> for ResolversOnHeap
where
    T: Serialize<S>,
    S: rkyv::ser::Serializer + ?Sized,
{
    fn serialize_with(field: &Vec<T>, serializer: &mut S) -> Result<Self::Resolver, S::Error> {
        let resolvers =
            field.iter().map(|value| value.serialize(serializer)).collect::<Result<Vec<_>, _>>()?;
        // SAFETY: copying an empty slice writes nothing: it aligns the
        // position for the elements and returns a resolver pointing there.
        // Each element's size is a multiple of its alignment, so the position
        // stays aligned for every element written after it.
        unsafe {
            let resolver =
                rkyv::vec::ArchivedVec::serialize_copy_from_slice(&field[..0], serializer)?;
            for (value, value_resolver) in field.iter().zip(resolvers) {
                serializer.resolve_aligned(value, value_resolver)?;
            }
            Ok(resolver)
        }
    }
}

impl<T, D> rkyv::with::DeserializeWith<rkyv::vec::ArchivedVec<T::Archived>, Vec<T>, D>
    for ResolversOnHeap
where
    T: Archive,
    T::Archived: Deserialize<T, D>,
    D: rkyv::Fallible + ?Sized,
{
    fn deserialize_with(
        field: &rkyv::vec::ArchivedVec<T::Archived>,
        deserializer: &mut D,
    ) -> Result<Vec<T>, D::Error> {
        field.iter().map(|value| value.deserialize(deserializer)).collect()
    }
}

#[cfg(any(feature = "data-driver", all(feature = "contract", target_family = "wasm")))]
mod contract;

/// The data driver Forge generates from the contract's methods.
#[cfg(feature = "data-driver")]
pub use contract::data_driver as driver;
