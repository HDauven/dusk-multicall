//! The contract. Its Wasm exports are written by hand, in `exports`; in a
//! `data-driver` build, Forge's `#[contract]` generates the data driver from
//! the methods below.
//!
//! Each method runs only as a direct query. A query calls its contract with no
//! caller; a transaction reaches a contract through the transfer contract, and
//! any calling contract is a caller too.

#[cfg_attr(feature = "data-driver", dusk_forge::contract)]
mod multicall {
    use alloc::vec::Vec;

    use dusk_multicall::{Aggregate, Call};

    /// Batches read-only contract calls into one query. It holds no state.
    pub struct Multicall;

    impl Multicall {
        /// The contract holds no state.
        pub const fn new() -> Self {
            Self
        }

        /// Runs each call in order inside this query and returns the height
        /// of the block whose state the query ran on and, per call, its reply
        /// bytes or why it failed.
        pub fn aggregate(&self, calls: Vec<Call>) -> Aggregate {
            use dusk_multicall::{ARGBUF_LEN, CallError, MAX_CALLS};
            direct_query_only();
            assert!(
                calls.len() <= MAX_CALLS,
                "multicall: {} calls, at most {MAX_CALLS}",
                calls.len()
            );
            let height = block_height();

            let mut results = Vec::with_capacity(calls.len());
            let mut out_of_line = 0;
            for (i, call) in calls.iter().enumerate() {
                let limit = match call.gas_limit {
                    0 => dusk_multicall::fair_share(
                        dusk_core::abi::limit() - dusk_core::abi::spent(),
                        calls.len() - i,
                    ),
                    cap => cap,
                };
                let result = dusk_core::abi::call_raw_with_limit(
                    call.contract,
                    &call.fn_name,
                    &call.fn_args,
                    limit,
                )
                .map_err(CallError::from);
                // The export encodes the reply into the argument buffer, so
                // stop as soon as the results can no longer fit in it.
                out_of_line += dusk_multicall::out_of_line(&result);
                assert!(
                    dusk_multicall::reply_len(calls.len(), out_of_line) <= ARGBUF_LEN,
                    "multicall: reply exceeds {ARGBUF_LEN} bytes at call {i}"
                );
                results.push(result);
            }
            Aggregate { height, results }
        }

        /// The ABI version, 1.
        pub fn version(&self) -> u32 {
            direct_query_only();
            dusk_multicall::VERSION
        }
    }

    fn direct_query_only() {
        assert!(dusk_core::abi::caller().is_none(), "multicall: direct queries only");
    }

    unsafe extern "C" {
        /// The host's metadata read: writes the value to the argument buffer
        /// and returns its length.
        fn hd(name: *const u8, name_len: u32) -> u32;
    }

    /// Reads the height as `dusk_core::abi::block_height` does, without the
    /// `tracing` call in uplink's `meta_data`, which links float formatting
    /// and more than doubles the Wasm.
    fn block_height() -> u64 {
        // `dusk_core::abi::Metadata::BLOCK_HEIGHT`
        const BLOCK_HEIGHT: &str = "block_height";
        let len = unsafe { hd(BLOCK_HEIGHT.as_ptr(), BLOCK_HEIGHT.len() as u32) } as usize;
        piecrust_uplink::arg_buf::with_arg_buf(|buf| rkyv::from_bytes::<u64>(&buf[..len]).ok())
            .unwrap_or_else(|| panic!("multicall: no block height"))
    }
}

/// The Wasm exports. Each decodes its argument into owned values, runs its
/// method and encodes the reply.
#[cfg(not(feature = "data-driver"))]
mod exports {
    use core::fmt::Debug;

    use bytecheck::CheckBytes;
    use piecrust_uplink::arg_buf::with_arg_buf;
    use piecrust_uplink::{SCRATCH_BUF_BYTES, StandardBufSerializer};
    use rkyv::ser::Serializer;
    use rkyv::ser::serializers::{BufferScratch, BufferSerializer, CompositeSerializer};
    use rkyv::validation::validators::DefaultValidator;
    use rkyv::{Archive, Deserialize, Infallible, Serialize};

    use super::multicall::Multicall;

    #[unsafe(no_mangle)]
    extern "C" fn aggregate(arg_len: u32) -> u32 {
        let calls = argument(arg_len);
        reply(&Multicall::new().aggregate(calls))
    }

    #[unsafe(no_mangle)]
    extern "C" fn version(arg_len: u32) -> u32 {
        let () = argument(arg_len);
        reply(&Multicall::new().version())
    }

    /// The argument, checked and decoded as uplink's `wrap_call` does it, with
    /// the same message for a bad one.
    fn argument<A, E>(arg_len: u32) -> A
    where
        A: Archive,
        A::Archived:
            Deserialize<A, Infallible> + for<'a> CheckBytes<DefaultValidator<'a>, Error = E>,
        E: Debug,
    {
        let checked = with_arg_buf(|buf| {
            // The VM never passes more than the buffer holds.
            let arg = &buf[..buf.len().min(arg_len as usize)];
            rkyv::check_archived_root::<A>(arg)
                .map(|archived| archived.deserialize(&mut Infallible))
        });
        let Ok(argument) = checked.expect("Argument should correctly deserialize");
        argument
    }

    /// Encodes the reply into the argument buffer, where the VM reads it, and
    /// returns its length. `aggregate` has checked that its reply fits.
    fn reply<R>(value: &R) -> u32
    where
        R: for<'a> Serialize<StandardBufSerializer<'a>>,
    {
        let encoded = with_arg_buf(|buf| {
            let mut scratch = [0u8; SCRATCH_BUF_BYTES];
            let mut serializer = CompositeSerializer::new(
                BufferSerializer::new(buf),
                BufferScratch::new(&mut scratch),
                Infallible,
            );
            serializer.serialize_value(value).map(|_| serializer.pos()).map_err(|_| ())
        });
        encoded.unwrap_or_else(|()| panic!("multicall: cannot encode the reply")) as u32
    }
}
