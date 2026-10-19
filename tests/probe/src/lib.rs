//! Test target for the multicall: views that echo, report their caller, height
//! or gas limit, panic, burn gas, return large data or feed it; a counter that
//! writes; and a forwarder that calls another contract. For tests only.
#![cfg(target_family = "wasm")]
#![no_std]

extern crate alloc;

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::hint::black_box;

use bytecheck::CheckBytes;
use piecrust_uplink::{self as uplink, ContractId, arg_buf::with_arg_buf};
use rkyv::ser::serializers::AllocSerializer;
use rkyv::validation::validators::DefaultValidator;
use rkyv::{Archive, Deserialize, Infallible};

static mut COUNTER: u64 = 0;

fn argument<T: Archive>(len: u32) -> T
where
    T::Archived: Deserialize<T, Infallible> + for<'a> CheckBytes<DefaultValidator<'a>>,
{
    with_arg_buf(|buf| match rkyv::check_archived_root::<T>(&buf[..len as usize]) {
        Ok(archived) => archived.deserialize(&mut Infallible).ok(),
        Err(_) => None,
    })
    .unwrap_or_else(|| panic!("probe: invalid argument"))
}

fn reply(bytes: &[u8]) -> u32 {
    with_arg_buf(|buf| buf[..bytes.len()].copy_from_slice(bytes));
    bytes.len() as u32
}

fn output<T: rkyv::Serialize<AllocSerializer<256>>>(value: &T) -> u32 {
    reply(&rkyv::to_bytes::<_, 256>(value).expect("probe: encoding"))
}

/// Returns its argument unchanged.
#[unsafe(no_mangle)]
pub extern "C" fn echo(len: u32) -> u32 {
    len
}

// Export names must differ from the host imports (`caller`, `limit`, ...):
// the linker would bind the import to the export of the same name.

/// The caller's 32-byte ID, or nothing when no contract called.
#[unsafe(no_mangle)]
pub extern "C" fn caller_id(_: u32) -> u32 {
    match uplink::caller() {
        Some(id) => reply(&id.to_bytes()),
        None => 0,
    }
}

static mut REMEMBERED: Option<ContractId> = None;

/// Stores the caller's ID, for a transaction to leave behind.
#[unsafe(no_mangle)]
pub extern "C" fn remember_caller(_: u32) -> u32 {
    unsafe { REMEMBERED = uplink::caller() };
    0
}

/// The ID `remember_caller` stored, or nothing.
#[unsafe(no_mangle)]
pub extern "C" fn remembered_caller(_: u32) -> u32 {
    match unsafe { REMEMBERED } {
        Some(id) => reply(&id.to_bytes()),
        None => 0,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn height(_: u32) -> u32 {
    output(&uplink::meta_data::<u64>("block_height").unwrap_or_default())
}

/// The gas limit this call was given.
#[unsafe(no_mangle)]
pub extern "C" fn gas_limit(_: u32) -> u32 {
    output(&uplink::limit())
}

/// Panics with its argument as the message.
#[unsafe(no_mangle)]
pub extern "C" fn fail(len: u32) -> u32 {
    let message = with_arg_buf(|buf| String::from_utf8_lossy(&buf[..len as usize]).to_string());
    panic!("{message}")
}

/// Panics with a message of `n` bytes, `n` an archived u32: one `x` if `n` is
/// odd or two if even, then `é`s, two bytes each.
#[unsafe(no_mangle)]
pub extern "C" fn shout(len: u32) -> u32 {
    let n = argument::<u32>(len) as usize;
    let mut message = String::from(&"xx"[..n.min(2 - n % 2)]);
    while message.len() < n {
        message.push('é');
    }
    panic!("{message}")
}

unsafe extern "C" {
    /// The host's feed: sends the first `arg_len` bytes of the argument
    /// buffer to a feeder query's client, and traps in any other query.
    fn feed(arg_len: u32);
}

/// Feeds its argument bytes back as one chunk and returns nothing.
#[unsafe(no_mangle)]
pub extern "C" fn feed_back(len: u32) -> u32 {
    // The argument is already where `feed` reads.
    unsafe { feed(len) };
    0
}

/// Loops until it runs out of gas.
#[unsafe(no_mangle)]
pub extern "C" fn burn(_: u32) -> u32 {
    let mut i = 0u64;
    loop {
        i = black_box(i).wrapping_add(1);
    }
}

/// Loops `n` times, `n` an archived u64, and returns `n`: a heavy view.
#[unsafe(no_mangle)]
pub extern "C" fn spin(len: u32) -> u32 {
    let n: u64 = argument(len);
    let mut i = 0u64;
    while black_box(i) < n {
        i += 1;
    }
    output(&i)
}

/// Returns `n` bytes, `n` an archived u32: byte `i` is `i % 251`.
#[unsafe(no_mangle)]
pub extern "C" fn big(len: u32) -> u32 {
    let n: u32 = argument(len);
    let bytes: Vec<u8> = (0..n).map(|i| (i % 251) as u8).collect();
    reply(&bytes)
}

/// Adds `n`, an archived u64, to the counter and returns the new value.
#[unsafe(no_mangle)]
pub extern "C" fn bump(len: u32) -> u32 {
    let n: u64 = argument(len);
    output(&add(n))
}

/// Adds `n` to the counter, then panics.
#[unsafe(no_mangle)]
pub extern "C" fn bump_then_fail(len: u32) -> u32 {
    let n: u64 = argument(len);
    panic!("probe: reverted at {}", add(n))
}

#[unsafe(no_mangle)]
pub extern "C" fn counter(_: u32) -> u32 {
    output(&unsafe { COUNTER })
}

/// Calls `(contract, fn_name, fn_args)`. Replies `0` and the reply bytes, or
/// `1` and the error.
#[unsafe(no_mangle)]
pub extern "C" fn forward(len: u32) -> u32 {
    let (contract, fn_name, fn_args): (ContractId, String, Vec<u8>) = argument(len);
    let (tag, bytes) = match uplink::call_raw(contract, &fn_name, &fn_args) {
        Ok(bytes) => (0, bytes),
        Err(error) => (1, error.to_string().into_bytes()),
    };
    reply(&[&[tag][..], &bytes].concat())
}

fn add(n: u64) -> u64 {
    // Wasm runs one call at a time; nothing else holds this reference.
    unsafe {
        COUNTER += n;
        COUNTER
    }
}
