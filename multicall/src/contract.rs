//! The contract. Forge's `#[contract]` turns each `pub fn` into a Wasm export
//! that decodes its argument before the method runs and encodes what the
//! method returns, and, in a `data-driver` build, generates the data driver.
//!
//! Each method runs only as a direct query. A query calls its contract with no
//! caller; a transaction reaches a contract through the transfer contract, and
//! any calling contract is a caller too.

#[dusk_forge::contract]
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
