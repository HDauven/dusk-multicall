//! In-VM harness: the multicall and the probe on piecrust 0.31, the VM in
//! Rusk 1.7.1. Every query runs as Rusk runs one: a fresh session on the
//! committed state, the block height as metadata and the block gas limit,
//! then dropped without a commit. `make wasm` builds the contracts first.
use std::path::Path;

pub use dusk_multicall::{
    ARGBUF_LEN, Aggregate, Call, CallError, ContractId, MAX_CALLS, MAX_PANIC_BYTES, reply_len,
};
use piecrust::{Error, SessionData, VM};
use rkyv::ser::serializers::AllocSerializer;
use rkyv::{AlignedVec, Archive, Deserialize, Infallible};

/// Rusk 1.7.1 gives a query the block gas limit.
pub const QUERY_GAS: u64 = 3_000_000_000;
pub const HEIGHT: u64 = 4242;

pub const MULTICALL: ContractId = id(0xaa);
pub const PROBE: ContractId = id(0xbb);
/// The transfer contract's ID, through which every transaction calls; here a
/// probe stands in for it.
pub const TRANSFER: ContractId = ContractId::from_bytes({
    let mut b = [0; 32];
    b[0] = 1;
    b
});
pub const MISSING: ContractId = id(0xcc);

pub const fn id(byte: u8) -> ContractId {
    ContractId::from_bytes([byte; 32])
}

pub struct World {
    vm: VM,
    root: [u8; 32],
    // Dropped after the VM. `VM::ephemeral` would leave its directory behind.
    _dir: tempfile::TempDir,
}

impl World {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().expect("VM directory");
        let vm = VM::new(dir.path()).expect("VM");
        let mut session = vm.session(SessionData::builder()).expect("session");
        for (id, name) in [
            (MULTICALL, "dusk_multicall"),
            (PROBE, "multicall_probe"),
            (TRANSFER, "multicall_probe"),
        ] {
            session
                .deploy_raw(Some(id), &wasm(name), None, vec![0; 96], QUERY_GAS)
                .expect("deploy");
        }
        let root = session.commit().expect("commit");
        Self { vm, root, _dir: dir }
    }

    /// A direct query: its data and gas spent, or the VM's error.
    pub fn query(
        &self,
        contract: ContractId,
        fn_name: &str,
        arg: &[u8],
    ) -> Result<(Vec<u8>, u64), Error> {
        self.query_with_gas(contract, fn_name, arg, QUERY_GAS)
    }

    /// The same with a gas limit other than Rusk's.
    pub fn query_with_gas(
        &self,
        contract: ContractId,
        fn_name: &str,
        arg: &[u8],
        gas: u64,
    ) -> Result<(Vec<u8>, u64), Error> {
        let data = SessionData::builder()
            .base(self.root)
            .insert("block_height", HEIGHT)
            .expect("metadata");
        let mut session = self.vm.session(data).expect("session");
        session
            .call_raw(contract, fn_name, arg.to_vec(), gas)
            .map(|receipt| (receipt.data, receipt.gas_spent))
    }

    /// A feeder query, as Rusk runs one for a `Rusk-Feeder` request: its
    /// reply, which Rusk drops, and the chunks fed to the client.
    pub fn feeder_query(
        &self,
        contract: ContractId,
        fn_name: &str,
        arg: &[u8],
    ) -> (Result<Vec<u8>, Error>, Vec<Vec<u8>>) {
        let data = SessionData::builder()
            .base(self.root)
            .insert("block_height", HEIGHT)
            .expect("metadata");
        let mut session = self.vm.session(data).expect("session");
        let (sender, receiver) = std::sync::mpsc::channel();
        let reply = session
            .feeder_call_raw(contract, fn_name, arg.to_vec(), QUERY_GAS, sender)
            .map(|receipt| receipt.data);
        (reply, receiver.try_iter().collect())
    }

    pub fn reply(&self, contract: ContractId, fn_name: &str, arg: &[u8]) -> Vec<u8> {
        self.query(contract, fn_name, arg).expect("query").0
    }

    /// `aggregate` over these calls, with the gas the query spent.
    pub fn aggregate(&self, calls: &[Call]) -> Result<(Aggregate, u64), String> {
        self.aggregate_raw(&encode(&calls.to_vec())).map(|(bytes, gas)| (decode(&bytes), gas))
    }

    pub fn aggregate_raw(&self, arg: &[u8]) -> Result<(Vec<u8>, u64), String> {
        self.query(MULTICALL, "aggregate", arg).map_err(message)
    }

    pub fn results(&self, calls: &[Call]) -> Vec<Result<Vec<u8>, CallError>> {
        self.aggregate(calls).expect("aggregate").0.results
    }
}

impl Default for World {
    fn default() -> Self {
        Self::new()
    }
}

/// A call with a fair share of the gas.
pub fn call(contract: ContractId, fn_name: &str, arg: impl Into<Vec<u8>>) -> Call {
    Call { contract, fn_name: fn_name.into(), fn_args: arg.into(), gas_limit: 0 }
}

pub fn capped(call: Call, gas_limit: u64) -> Call {
    Call { gas_limit, ..call }
}

pub fn encode<T: rkyv::Serialize<AllocSerializer<1024>>>(value: &T) -> Vec<u8> {
    rkyv::to_bytes::<_, 1024>(value).expect("encode").to_vec()
}

pub fn decode<T: Archive>(bytes: &[u8]) -> T
where
    T::Archived: Deserialize<T, Infallible>
        + for<'a> bytecheck::CheckBytes<rkyv::validation::validators::DefaultValidator<'a>>,
{
    let mut aligned = AlignedVec::new();
    aligned.extend_from_slice(bytes);
    let archived = rkyv::check_archived_root::<T>(&aligned).expect("valid archive");
    archived.deserialize(&mut Infallible).expect("infallible")
}

/// The message a failed query reports.
pub fn message(error: Error) -> String {
    match error {
        Error::Panic(message) => message,
        error => error.to_string(),
    }
}

pub fn wasm(name: &str) -> Vec<u8> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("../../target/wasm/{name}.wasm"));
    std::fs::read(&path)
        .unwrap_or_else(|e| panic!("{}: {e}; run `make wasm` first", path.display()))
}
