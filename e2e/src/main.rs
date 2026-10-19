//! End-to-end checks of the multicall on a local Rusk 1.7.1 node. `run.sh`
//! starts the node, runs this and stops it. Every check prints PASS or FAIL
//! with what it saw; the run writes `report.json` and fails if a check failed.
mod node;
mod wallet;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{scope, sleep};
use std::time::{Duration, Instant};

use dusk_bytes::Serializable;
use dusk_core::stake::STAKE_CONTRACT;
use dusk_core::transfer::TRANSFER_CONTRACT;
use dusk_core::transfer::data::gen_contract_id;
use dusk_data_driver::ConvertibleContract;
use dusk_multicall::driver::Driver;
use dusk_multicall::{
    ARGBUF_LEN, Aggregate, Call, CallError, ContractId, MAX_CALLS, MAX_PANIC_BYTES, reply_len,
};
use rkyv::ser::serializers::AllocSerializer;
use rkyv::{AlignedVec, Archive, Deserialize, Infallible};
use serde_json::{Value, json};
use sha3::Digest;

use node::{Node, Refusal};
use wallet::{Account, Chain};

const QUERY_GAS: u64 = 3_000_000_000;
const REFUSED: &str = "multicall: direct queries only";
/// How Forge's export, through uplink's `wrap_call`, refuses a bad argument.
const INVALID: &str = "Panic: Argument should correctly deserialize: ";
const CALL_GAS: u64 = 20_000_000;
/// Rusk 1.7.1 answers every failed query with this; the reason goes to its log.
const GENERIC: &str = r#"{"error":"Internal server error"}"#;

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

macro_rules! ensure {
    ($cond:expr, $($message:tt)+) => {
        if !$cond {
            return Err(format!($($message)+));
        }
    };
}

#[derive(Default)]
struct Report {
    checks: Vec<Value>,
    numbers: BTreeMap<String, Value>,
    failed: usize,
}

impl Report {
    fn check(&mut self, name: &str, outcome: Result<String, String>) {
        let (pass, detail) = match outcome {
            Ok(detail) => (true, detail),
            Err(detail) => (false, detail),
        };
        println!("{} {name}: {detail}", if pass { "PASS" } else { "FAIL" });
        self.failed += usize::from(!pass);
        self.checks.push(json!({ "check": name, "pass": pass, "detail": detail }));
    }

    fn number(&mut self, name: &str, value: impl Into<Value>) {
        let value = value.into();
        println!("     {name} = {value}");
        self.numbers.insert(name.into(), value);
    }
}

/// A call with a fair share of the gas.
fn call(contract: ContractId, fn_name: &str, fn_args: Vec<u8>) -> Call {
    Call { contract, fn_name: fn_name.into(), fn_args, gas_limit: 0 }
}

fn capped(call: Call, gas_limit: u64) -> Call {
    Call { gas_limit, ..call }
}

/// A call at the VM's default limit, 93% of the gas left.
fn at_default(call: Call) -> Call {
    capped(call, u64::MAX)
}

/// 93% of `left`, as the VM computes its default limit.
fn default_of(left: u64) -> u64 {
    left / 100 * 93 + left % 100 * 93 / 100
}

/// The probe's `forward` argument.
fn forward(target: ContractId, fn_name: &str, arg: Vec<u8>) -> Vec<u8> {
    encode(&(target, fn_name.to_string(), arg))
}

/// The gas left when a `gas_limit` probe call at the VM's default got `limit`.
fn left_before(limit: u64) -> u64 {
    limit * 100 / 93
}

struct Env {
    node: Node,
    log: String,
    /// Measurements the checks record for the report.
    notes: Mutex<BTreeMap<String, Value>>,
    chain: Chain,
    a: Account,
    b: Account,
    multicall: ContractId,
    probe: ContractId,
}

impl Env {
    fn note(&self, name: &str, value: impl Into<Value>) {
        self.notes.lock().unwrap().insert(name.into(), value.into());
    }

    fn aggregate(&self, calls: &[Call]) -> Result<Aggregate, Refusal> {
        self.node
            .query(&self.multicall, "aggregate", &encode(&calls.to_vec()))
            .map(|bytes| decode(&bytes))
    }

    fn results(&self, calls: &[Call]) -> Result<Vec<Result<Vec<u8>, CallError>>, String> {
        self.aggregate(calls).map(|a| a.results).map_err(|r| format!("{r:?}"))
    }

    /// An `aggregate` query the node must refuse with VM error `reason`. The
    /// client gets only HTTP 500 and the generic body; the node logs the reason.
    fn refused(&self, arg: &[u8], reason: &str) -> Result<Refusal, String> {
        self.refused_where(arg, reason, |line, wanted| line.ends_with(wanted))
    }

    /// The same for a reason that starts with `prefix`.
    fn refused_like(&self, arg: &[u8], prefix: &str) -> Result<Refusal, String> {
        self.refused_where(arg, prefix, |line, wanted| line.contains(wanted))
    }

    fn refused_where(
        &self,
        arg: &[u8],
        reason: &str,
        matches: fn(&str, &str) -> bool,
    ) -> Result<Refusal, String> {
        let line = format!("VM Error: {reason}");
        let logged = || {
            let log = std::fs::read_to_string(&self.log).unwrap_or_default();
            log.lines().filter(|l| matches(l.trim_end(), &line)).count()
        };
        let before = logged();
        let refusal = match self.node.query(&self.multicall, "aggregate", arg) {
            Ok(reply) => return Err(format!("accepted, {} bytes back", reply.len())),
            Err(refusal) => refusal,
        };
        ensure!(refusal.status == 500 && refusal.body == GENERIC, "{refusal:?}");
        let start = Instant::now();
        while logged() <= before {
            ensure!(start.elapsed() < Duration::from_secs(5), "the node never logged {line:?}");
            sleep(Duration::from_millis(50));
        }
        Ok(refusal)
    }

    fn refused_calls(&self, calls: &[Call], reason: &str) -> Result<Refusal, String> {
        self.refused(&encode(&calls.to_vec()), reason)
    }

    fn direct(&self, c: &Call) -> Result<Vec<u8>, String> {
        self.node
            .query(&c.contract, &c.fn_name, &c.fn_args)
            .map_err(|r| format!("{} {r:?}", c.fn_name))
    }

    fn probe(&self, fn_name: &str, arg: Vec<u8>) -> Call {
        call(self.probe, fn_name, arg)
    }

    fn height(&self) -> u64 {
        decode(&self.node.query(&self.probe, "height", &[]).expect("height"))
    }

    fn counter(&self) -> u64 {
        decode(&self.node.query(&self.probe, "counter", &[]).expect("counter"))
    }

    fn gas_given(&self, results: &[Result<Vec<u8>, CallError>], i: usize) -> u64 {
        decode(results[i].as_ref().expect("gas_limit reply"))
    }
}

fn main() {
    let arg = |flag: &str| {
        let args: Vec<String> = std::env::args().collect();
        let i = args.iter().position(|a| a == flag).unwrap_or_else(|| panic!("missing {flag}"));
        args[i + 1].clone()
    };
    let (rusk, wasm, out, log) =
        (arg("--rusk-dir"), arg("--wasm"), arg("--out"), arg("--node-log"));
    let read = |path: String| std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    let text = |path: String| String::from_utf8(read(path)).expect("UTF-8");
    let node = Node::new(&arg("--node"));
    let mut report = Report::default();

    let info = node.info();
    let vm = &info["vm_config"];
    let number = |key: &str| vm[key].as_u64().unwrap_or_else(|| panic!("vm_config.{key}"));
    report.number("node.version", info["version"].clone());
    report.number("node.version_build", info["version_build"].clone());
    for key in
        ["block_gas_limit", "gas_per_deploy_byte", "min_deploy_points", "min_deployment_gas_price"]
    {
        report.number(&format!("vm.{key}"), number(key));
    }
    // The node's info leaves `chain_id` empty on a local chain; the transfer
    // contract holds the one transactions are signed for.
    let chain_id: u8 = decode(&node.query(&TRANSFER_CONTRACT, "chain_id", &[]).expect("chain_id"));
    report.number("node.chain_id", chain_id);
    let chain =
        Chain { node: node.clone(), chain_id, deploy_price: number("min_deployment_gas_price") };

    let a = Account::from_phrase(&text(format!("{rusk}/examples/recovery-phrase.txt")), 0);
    let b = Account::from_phrase(&text(format!("{rusk}/examples/recovery-phrase-unstaked.txt")), 0);
    let genesis = text(format!("{rusk}/examples/genesis.toml"));
    report.check("the signing accounts are the genesis accounts", {
        let funded = |acct: &Account| genesis.contains(&format!("address = '{}'", acct.address()));
        if funded(&a) && funded(&b) {
            Ok(format!("A {}…, B {}…", &a.address()[..12], &b.address()[..12]))
        } else {
            Err("a derived key is not in genesis.toml".into())
        }
    });

    // Deployments, with the gas Dusk Domains' deploy tooling uses: the
    // bytecode charge plus 10M, at the node's minimum deployment price.
    let multicall_wasm = read(format!("{wasm}/dusk_multicall.wasm"));
    let probe_wasm = read(format!("{wasm}/multicall_probe.wasm"));
    let driver_wasm = read(format!("{wasm}/dusk_multicall_driver.wasm"));
    let charge =
        |len: usize| (len as u64 * number("gas_per_deploy_byte")).max(number("min_deploy_points"));
    let deploy_gas = |len: usize| charge(len) + 10_000_000;
    let multicall = gen_contract_id(&multicall_wasm, 0, a.owner());
    let probe = gen_contract_id(&probe_wasm, 0, a.owner());
    let deployed =
        chain.deploy(&a, &multicall_wasm, a.owner(), 0, deploy_gas(multicall_wasm.len()));
    report.check(
        "the multicall deploys at BLAKE2b-256(wasm ‖ nonce ‖ owner)",
        match &deployed {
            Ok(r)
                if r.err.is_none() && r.contract_id == Some(hex::encode(multicall.as_bytes())) =>
            {
                Ok(format!("{} at height {}", hex::encode(multicall.as_bytes()), r.height))
            }
            Ok(r) => Err(format!("err {:?}, contract {:?}", r.err, r.contract_id)),
            Err(refusal) => Err(format!("{refusal:?}")),
        },
    );
    let Ok(deployed) = deployed else { std::process::exit(1) };
    // A second chain only shows where the same deployment lands there.
    if std::env::args().any(|a| a == "--deploy-only") {
        finish(&report, &out, multicall, None);
    }
    let price = chain.deploy_price;
    report.number("deploy.wasm_bytes", multicall_wasm.len());
    report.number("deploy.bytecode_charge", charge(multicall_wasm.len()));
    report.number("deploy.gas_limit", deployed.gas_limit);
    report.number("deploy.gas_spent", deployed.gas_spent);
    report.number("deploy.price_lux", price);
    report.number("deploy.cost_dusk", deployed.gas_spent as f64 * price as f64 / 1e9);
    report.number("deploy.reserve_dusk", deployed.gas_limit as f64 * price as f64 / 1e9);
    let probe_deployed = chain.deploy(&a, &probe_wasm, a.owner(), 0, deploy_gas(probe_wasm.len()));
    report.check(
        "the probe deploys",
        match probe_deployed {
            Ok(r) if r.err.is_none() && r.contract_id == Some(hex::encode(probe.as_bytes())) => {
                Ok(hex::encode(probe.as_bytes()))
            }
            other => Err(format!("{:?}", other.map(|r| (r.err, r.contract_id)))),
        },
    );

    let env = Env { node, log, notes: Mutex::default(), chain, a, b, multicall, probe };
    report.check(
        "contract IDs ignore the signer and the chain, not the nonce",
        ids(&env, &probe_wasm, &multicall_wasm, deploy_gas(probe_wasm.len())),
    );
    report.check("only the owner registers the data driver", driver_upload(&env, &driver_wasm));
    report.check(
        "version is 1",
        env.direct(&call(multicall, "version", vec![])).and_then(|b| {
            let version: u32 = decode(&b);
            if version == 1 { Ok("1".into()) } else { Err(format!("{version}")) }
        }),
    );
    report.check("a direct query has no caller; a batched call has the multicall", callers(&env));
    report.check("in a transaction the transfer contract is the caller", transaction_caller(&env));
    report.check("replies equal direct queries at the same height", byte_equality(&env));
    report.check("every result comes from one height, in call order", one_height(&env));
    report.check("every failing call is reported and the batch goes on", failures(&env));
    report.check("panic messages are cut to 200 bytes at a character boundary", long_panics(&env));
    report.check("a feeder query streams callee chunks and drops the reply", feeder(&env));
    report.check("malformed envelopes, the empty list and the call cap", envelopes(&env));
    report.check("input and output overflow of the 64 KiB buffer", overflow(&env));
    report.check("explicit caps pass through; a cap of 0 gets a fair share", gas(&env));
    report.check("fair shares keep failures from starving later calls", fair_shares(&env));
    report.check("the share kept back encodes the largest reply", reserve(&env));
    report.check("writes in a query reach later calls and never persist", writes(&env));
    report
        .check("a transaction calling the multicall fails and changes nothing", transactions(&env));
    report.check("other contracts and nested multicalls are refused", nesting(&env));
    report.check("the node encodes and decodes JSON with the uploaded driver", json_mode(&env));
    report.check("hex text in, hex text out, as plain curl sends it", hex_mode(&env));
    report.check("how many typical calls fit one query", capacity(&env));
    report.check("concurrent queries while blocks are produced", concurrency(&env));
    for (name, value) in env.notes.lock().unwrap().clone() {
        report.number(&name, value);
    }

    finish(&report, &out, multicall, Some(probe));
}

fn finish(report: &Report, out: &str, multicall: ContractId, probe: Option<ContractId>) -> ! {
    let summary = json!({
        "checks": report.checks,
        "numbers": report.numbers,
        "multicall": hex::encode(multicall.as_bytes()),
        "probe": probe.map(|p| hex::encode(p.as_bytes())),
    });
    std::fs::write(format!("{out}/report.json"), serde_json::to_string_pretty(&summary).unwrap())
        .expect("report");
    println!(
        "{} checks, {} failed; report in {out}/report.json",
        report.checks.len(),
        report.failed
    );
    std::process::exit(i32::from(report.failed > 0));
}

fn ids(env: &Env, probe_wasm: &[u8], multicall_wasm: &[u8], gas: u64) -> Result<String, String> {
    let deploy = |from: &Account, wasm: &[u8], owner: Vec<u8>, nonce| {
        env.chain.deploy(from, wasm, owner, nonce, gas).map_err(|r| format!("{r:?}"))
    };
    // B signs a deployment whose owner is A: the ID follows the owner bytes.
    let r = deploy(&env.b, probe_wasm, env.a.owner(), 5)?;
    let at_5 = gen_contract_id(probe_wasm, 5, env.a.owner());
    ensure!(
        r.err.is_none() && r.contract_id == Some(hex::encode(at_5.as_bytes())),
        "B's deployment: {:?}",
        r.err
    );
    let owner = env.node.metadata(&at_5)["contract_owner"].as_str().unwrap_or_default().to_string();
    ensure!(owner == hex::encode(env.a.owner()), "owner {owner}");
    // Another nonce, another ID.
    let r = deploy(&env.a, probe_wasm, env.a.owner(), 6)?;
    let at_6 = gen_contract_id(probe_wasm, 6, env.a.owner());
    ensure!(
        r.contract_id == Some(hex::encode(at_6.as_bytes())) && at_6 != at_5,
        "nonce 6: {:?}",
        r.contract_id
    );
    // The multicall's own triple again, from B: the ID is taken.
    let r = deploy(&env.b, multicall_wasm, env.a.owner(), 0)?;
    let refused = r.err.clone().unwrap_or_default();
    ensure!(!refused.is_empty() && r.gas_spent == r.gas_limit, "redeploy: {:?}", r.err);
    ensure!(env.node.query(&env.multicall, "version", &[]).is_ok(), "the multicall is gone");
    Ok(format!(
        "signer B with owner A lands at the ID of owner A; nonce 5 vs 6 differ; the same triple again fails ({}) and costs its full limit",
        refused.chars().take(90).collect::<String>()
    ))
}

fn driver_upload(env: &Env, driver: &[u8]) -> Result<String, String> {
    let sign =
        |acct: &Account| hex::encode(acct.sk.sign(&sha3::Sha3_256::digest(driver)).to_bytes());
    let by_b = env.node.upload_driver(&env.multicall, &sign(&env.b), driver);
    ensure!(by_b.is_err(), "a driver signed by B was accepted");
    let by_a = env
        .node
        .upload_driver(&env.multicall, &sign(&env.a), driver)
        .map_err(|r| format!("{r:?}"))?;
    let meta = env.node.metadata(&env.multicall);
    ensure!(meta["driver_available"] == true, "metadata {meta}");
    Ok(format!("B: {}; A: {by_a}", by_b.unwrap_err().body.chars().take(80).collect::<String>()))
}

fn callers(env: &Env) -> Result<String, String> {
    let direct = env.direct(&env.probe("caller_id", vec![]))?;
    ensure!(direct.is_empty(), "a direct query saw caller {}", hex::encode(&direct));
    let batched =
        env.results(&[env.probe("caller_id", vec![])])?.remove(0).map_err(|e| format!("{e:?}"))?;
    ensure!(batched == env.multicall.to_bytes(), "batched caller {}", hex::encode(&batched));
    Ok("direct: none; in a batch: the multicall".into())
}

fn transaction_caller(env: &Env) -> Result<String, String> {
    let r = env
        .chain
        .call(&env.a, env.probe, "remember_caller", vec![], CALL_GAS)
        .map_err(|r| format!("{r:?}"))?;
    ensure!(r.err.is_none(), "{:?}", r.err);
    let remembered = env.direct(&env.probe("remembered_caller", vec![]))?;
    ensure!(remembered == TRANSFER_CONTRACT.to_bytes(), "remembered {}", hex::encode(&remembered));
    Ok(format!("caller() in a transaction was {}", hex::encode(&remembered)))
}

/// Views of the genesis contracts and the probe.
fn views(env: &Env) -> Vec<Call> {
    vec![
        call(TRANSFER_CONTRACT, "account", encode(&env.a.pk)),
        call(TRANSFER_CONTRACT, "account", encode(&env.b.pk)),
        call(TRANSFER_CONTRACT, "chain_id", vec![]),
        call(TRANSFER_CONTRACT, "root", vec![]),
        call(TRANSFER_CONTRACT, "num_notes", vec![]),
        call(TRANSFER_CONTRACT, "contract_balance", encode(&env.multicall)),
        call(STAKE_CONTRACT, "get_stake", encode(&env.a.pk)),
        call(STAKE_CONTRACT, "get_stake_keys", encode(&env.a.pk)),
        call(STAKE_CONTRACT, "get_version", vec![]),
        call(STAKE_CONTRACT, "get_config", vec![]),
        call(STAKE_CONTRACT, "burnt_amount", vec![]),
        env.probe("height", vec![]),
        env.probe("counter", vec![]),
        env.probe("echo", b"the same bytes".to_vec()),
        env.probe("big", encode(&2000u32)),
    ]
}

fn byte_equality(env: &Env) -> Result<String, String> {
    let views = views(env);
    for attempt in 1..=30 {
        let batch = env.aggregate(&views).map_err(|r| format!("{r:?}"))?;
        // The probe's height bracket proves every direct query ran at one tip.
        let before = env.height();
        let direct = views.iter().map(|c| env.direct(c)).collect::<Result<Vec<_>, _>>()?;
        let after = env.height();
        if before != batch.height || after != batch.height {
            continue;
        }
        for (i, (got, want)) in batch.results.iter().zip(&direct).enumerate() {
            ensure!(
                got.as_ref() == Ok(want),
                "{} differs at height {}",
                views[i].fn_name,
                batch.height
            );
        }
        let tip = env.node.tip();
        ensure!(
            tip >= batch.height && tip <= batch.height + 1,
            "tip {tip}, batch {}",
            batch.height
        );
        return Ok(format!(
            "{} views equal at height {} (attempt {attempt}, GraphQL tip {tip})",
            views.len(),
            batch.height
        ));
    }
    Err("the tip never held still for a whole comparison".into())
}

fn one_height(env: &Env) -> Result<String, String> {
    let stake = call(STAKE_CONTRACT, "get_stake", encode(&env.a.pk));
    let height = env.probe("height", vec![]);
    let echo = |n: u8| env.probe("echo", vec![n; n as usize]);
    let calls =
        [height.clone(), echo(1), stake.clone(), echo(2), height.clone(), stake, echo(3), height];
    let (start, mut heights) = (Instant::now(), BTreeMap::new());
    while start.elapsed() < Duration::from_secs(6) {
        let batch = env.aggregate(&calls).map_err(|r| format!("{r:?}"))?;
        let r: Vec<Vec<u8>> = batch.results.iter().map(|r| r.clone().unwrap()).collect();
        for i in [0, 4, 7] {
            ensure!(
                decode::<u64>(&r[i]) == batch.height,
                "call {i} saw height {}",
                decode::<u64>(&r[i])
            );
        }
        ensure!(r[2] == r[5], "two stake reads differ within one batch");
        ensure!(r[1] == [1] && r[3] == [2, 2] && r[6] == [3, 3, 3], "echoes out of order");
        let stake = heights.entry(batch.height).or_insert_with(|| r[2].clone());
        ensure!(*stake == r[2], "stake differs between batches at height {}", batch.height);
        sleep(Duration::from_millis(100));
    }
    ensure!(heights.len() >= 2, "only saw {} height", heights.len());
    let distinct = heights.values().collect::<BTreeSet<_>>().len();
    Ok(format!(
        "{} heights, {distinct} distinct stake values, each batch at one height",
        heights.len()
    ))
}

fn failures(env: &Env) -> Result<String, String> {
    let missing = ContractId::from_bytes([0xcc; 32]);
    let cases = [
        ("panic", env.probe("fail", b"boom".to_vec()), Some(CallError::Panic("boom".into()))),
        ("out of gas", capped(env.probe("burn", vec![]), 1_000_000), Some(CallError::OutOfGas)),
        ("missing contract", call(missing, "x", vec![]), Some(CallError::DoesNotExist)),
        ("unknown function", env.probe("no_such_fn", vec![]), Some(CallError::Unknown)),
        (
            "malformed arguments",
            env.probe("bump", vec![1, 2, 3]),
            Some(CallError::Panic("probe: invalid argument".into())),
        ),
        (
            "unknown genesis function",
            call(TRANSFER_CONTRACT, "no_such_fn", vec![]),
            Some(CallError::Unknown),
        ),
        // Whatever the transfer contract makes of it, it must fail.
        ("malformed genesis arguments", call(TRANSFER_CONTRACT, "account", vec![1, 2, 3]), None),
        // A function that streams with `feed` has nowhere to write in a batch.
        ("streaming function", call(STAKE_CONTRACT, "stakes", vec![]), None),
    ];
    let mut calls = vec![env.probe("echo", b"first".to_vec())];
    for (_, failing, _) in &cases {
        calls.extend([failing.clone(), env.probe("echo", b"after".to_vec())]);
    }
    let results = env.results(&calls)?;
    ensure!(results.len() == calls.len() && results[0] == Ok(b"first".to_vec()), "{results:?}");
    let mut seen = Vec::new();
    for (i, (name, _, expected)) in cases.iter().enumerate() {
        let error = results[1 + 2 * i].clone().err().ok_or(format!("{name} succeeded"))?;
        ensure!(expected.as_ref().is_none_or(|e| *e == error), "{name}: {error:?}");
        ensure!(results[2 + 2 * i] == Ok(b"after".to_vec()), "the call after {name}");
        seen.push(format!("{name}: {error:?}"));
    }
    Ok(format!("one batch answered all of them: {}", seen.join("; ")))
}

/// The message the probe's `shout(n)` panics with: `x` if `n` is odd, `xx` if
/// even, then two-byte `é`s.
fn shout_message(n: usize) -> String {
    let mut message = String::from(&"xx"[..n.min(2 - n % 2)]);
    while message.len() < n {
        message.push('é');
    }
    message
}

fn long_panics(env: &Env) -> Result<String, String> {
    let shout = |n: u32| env.probe("shout", encode(&n));
    let ok = env.probe("echo", b"ok".to_vec());
    let mut kept = Vec::new();
    for n in [200usize, 201, 202, 65_531, 65_532] {
        let results = env.results(&[ok.clone(), shout(n as u32), ok.clone()])?;
        let cut = if n <= MAX_PANIC_BYTES { n } else { MAX_PANIC_BYTES - n % 2 };
        let want = CallError::Panic(shout_message(n)[..cut].into());
        ensure!(results[1] == Err(want), "shout({n}): {:?}", results[1].as_ref().err());
        ensure!(results[0] == Ok(b"ok".to_vec()) && results[2] == results[0], "around shout({n})");
        kept.push(format!("{n} → {cut}"));
    }
    let all = vec![shout(65_532); MAX_CALLS];
    let reply =
        env.node.query(&env.multicall, "aggregate", &encode(&all)).map_err(|r| format!("{r:?}"))?;
    let failed = decode::<Aggregate>(&reply).results;
    ensure!(
        failed.iter().all(|r| matches!(r, Err(CallError::Panic(m)) if m.len() == MAX_PANIC_BYTES)),
        "{MAX_CALLS} shouts"
    );
    ensure!(
        reply.len() == reply_len(MAX_CALLS, MAX_CALLS * MAX_PANIC_BYTES),
        "{} bytes",
        reply.len()
    );
    Ok(format!(
        "bytes kept of an n-byte panic, the calls around it fine: {}; {MAX_CALLS} panics of 65,532 bytes come back in a {}-byte reply",
        kept.join(", "),
        reply.len()
    ))
}

fn feeder(env: &Env) -> Result<String, String> {
    let calls = [env.probe("feed_back", b"chunk".to_vec()), env.probe("echo", b"ok".to_vec())];
    let plain = env.results(&calls)?;
    ensure!(plain == [Err(CallError::Unknown), Ok(b"ok".to_vec())], "plain query: {plain:?}");
    let fed = env
        .node
        .feeder_query(&env.multicall, "aggregate", &encode(&calls.to_vec()))
        .map_err(|r| format!("{r:?}"))?;
    ensure!(fed == b"chunk", "feeder query answered {:?}", String::from_utf8_lossy(&fed));
    let version =
        env.node.feeder_query(&env.multicall, "version", &[]).map_err(|r| format!("{r:?}"))?;
    ensure!(version.is_empty(), "feeder version answered {version:?}");
    Ok("a plain query reports the feeding call as Unknown; a feeder query streams only the callee's chunk and no Aggregate; a feeder version streams nothing".into())
}

fn envelopes(env: &Env) -> Result<String, String> {
    let valid = encode(&vec![env.probe("echo", b"x".to_vec())]);
    for (case, bytes) in [
        ("empty", vec![]),
        ("garbage", vec![0xff; 64]),
        ("truncated", valid[..valid.len() - 1].to_vec()),
    ] {
        env.refused_like(&bytes, INVALID).map_err(|e| format!("{case}: {e}"))?;
    }
    let empty = env.aggregate(&[]).map_err(|r| format!("{r:?}"))?;
    ensure!(empty.results.is_empty() && empty.height > 0, "empty: {empty:?}");
    let echo = env.probe("echo", vec![]);
    ensure!(env.results(&vec![echo.clone(); MAX_CALLS])?.len() == MAX_CALLS, "{MAX_CALLS} calls");
    let cap = format!("Panic: multicall: {} calls, at most {MAX_CALLS}", MAX_CALLS + 1);
    env.refused_calls(&vec![echo; MAX_CALLS + 1], &cap)?;
    Ok(format!(
        "empty, garbage and truncated envelopes refused ({INVALID:?}…); [] answers height {}; {MAX_CALLS} calls pass; {} logged {cap:?}",
        empty.height,
        MAX_CALLS + 1
    ))
}

fn overflow(env: &Env) -> Result<String, String> {
    let envelope = |n: usize| encode(&vec![env.probe("echo", vec![7u8; n])]);
    let largest = (ARGBUF_LEN - 128..ARGBUF_LEN)
        .take_while(|&n| envelope(n).len() <= ARGBUF_LEN)
        .last()
        .unwrap();
    let echoed = env
        .results(&[env.probe("echo", vec![7u8; largest])])?
        .remove(0)
        .map_err(|e| format!("{e:?}"))?;
    ensure!(echoed.len() == largest, "largest echo");
    let over = envelope(largest + 1);
    let host = format!(
        "Memory access out of bounds: offset 0, length {}, memory length {ARGBUF_LEN}",
        over.len()
    );
    env.refused(&over, &host)?;
    let big = |n: u32| env.probe("big", encode(&n));
    env.refused_calls(
        &[big(40_000), big(30_000)],
        &format!("Panic: multicall: reply exceeds {ARGBUF_LEN} bytes at call 1"),
    )?;
    env.refused_calls(
        &[big(65_505)],
        &format!("Panic: multicall: reply exceeds {ARGBUF_LEN} bytes at call 0"),
    )?;
    let fits = env.results(&[big(65_504)])?.remove(0).map_err(|e| format!("{e:?}"))?;
    ensure!(fits == env.direct(&big(65_504))?, "the largest reply differs from its direct query");
    Ok(format!(
        "a {largest}-byte argument works; a {}-byte envelope is refused by the VM before the contract runs ({host:?}); a 65,504-byte reply fits, 65,505 or 40,000 + 30,000 do not",
        over.len()
    ))
}

fn gas(env: &Env) -> Result<String, String> {
    let given = env.probe("gas_limit", vec![]);
    for cap in [100_000u64, 1_000_000, 1_000_000_000] {
        let results = env.results(&[capped(given.clone(), cap)])?;
        ensure!(env.gas_given(&results, 0) == cap, "cap {cap} gave {}", env.gas_given(&results, 0));
    }
    // A cap at or over the gas left gets the VM's default.
    let results = env.results(&[at_default(given.clone()), capped(given.clone(), QUERY_GAS)])?;
    let (first, second) = (env.gas_given(&results, 0), env.gas_given(&results, 1));
    let setup = QUERY_GAS - left_before(first);
    ensure!(setup < 1_000_000, "first call was given {first}");
    ensure!(second < first && first - second < 50_000, "{first} then {second}");
    env.note("gas.setup_before_first_call", setup);
    // A cap of 0: the gas left over the calls left plus one, so one share
    // stays back for the reply and a lone call gets half.
    let results = env.results(&vec![given.clone(); 4])?;
    let shares: Vec<u64> = (0..4).map(|i| env.gas_given(&results, i)).collect();
    let left: Vec<u64> = (0..4).map(|i| shares[i] * (5 - i as u64)).collect();
    ensure!(left[0] <= QUERY_GAS && left[0] > QUERY_GAS - 1_000_000, "{shares:?}");
    for i in 1..4 {
        ensure!(left[i] < left[i - 1] && left[i - 1] - left[i] < 50_000, "{shares:?}");
    }
    let lone = env.gas_given(&env.results(std::slice::from_ref(&given))?, 0);
    ensure!(lone <= QUERY_GAS / 2 && lone > (QUERY_GAS - 1_000_000) / 2, "lone call: {lone}");
    env.note("gas.shares_of_4_calls", json!(shares));
    env.note("gas.share_of_a_lone_call", lone);
    // What each view takes inside a batch: the gas left before and after it.
    let pair = env.results(&[at_default(given.clone()), at_default(given.clone())])?;
    let empty_slot = left_before(env.gas_given(&pair, 0)) - left_before(env.gas_given(&pair, 1));
    for (name, view) in [
        ("probe.echo", env.probe("echo", b"abc".to_vec())),
        ("transfer.account", call(TRANSFER_CONTRACT, "account", encode(&env.a.pk))),
        ("stake.get_stake", call(STAKE_CONTRACT, "get_stake", encode(&env.a.pk))),
        ("transfer.root", call(TRANSFER_CONTRACT, "root", vec![])),
    ] {
        let r = env.results(&[at_default(given.clone()), view, at_default(given.clone())])?;
        let cost =
            left_before(env.gas_given(&r, 0)) - left_before(env.gas_given(&r, 2)) - empty_slot;
        env.note(&format!("gas.in_batch.{name}"), cost);
    }
    Ok(format!(
        "caps passed through, caps at or over the gas left got 93%, a 4-call batch's shares were {shares:?}, a lone call's {lone}"
    ))
}

fn fair_shares(env: &Env) -> Result<String, String> {
    let given = env.probe("gas_limit", vec![]);
    let fail = env.probe("fail", b"x".to_vec());
    let echo = env.probe("echo", b"ok".to_vec());
    // What a failure takes from the call after it.
    let after = |first: Call| -> Result<u64, String> {
        Ok(env.gas_given(&env.results(&[first, at_default(given.clone())])?, 1))
    };
    let fresh = env.gas_given(&env.results(&[at_default(given.clone())])?, 0);
    let at_93 = after(at_default(fail.clone()))?;
    let at_cap = after(capped(fail.clone(), 1_000_000))?;
    let at_share = after(fail.clone())?;
    let at_missing = after(call(ContractId::from_bytes([0xcc; 32]), "x", vec![]))?;
    ensure!(at_93 < fresh / 10, "after a failure at the default: {at_93}");
    ensure!(fresh - at_cap < 1_100_000, "a 1M cap took {}", fresh - at_cap);
    // A failure at a fair share takes a third, with one call after it and one
    // share kept back.
    let two_thirds = QUERY_GAS / 3 * 2;
    ensure!(
        at_share <= default_of(two_thirds) && at_share > default_of(two_thirds - 1_000_000),
        "{at_share}"
    );
    ensure!(fresh - at_missing < 50_000, "a missing contract took {}", fresh - at_missing);
    // Failures at a fair share leave every later call its own share.
    for failures in [5, 50, 255] {
        let results = env.results(&[vec![fail.clone(); failures], vec![echo.clone()]].concat())?;
        ensure!(results[..failures].iter().all(Result::is_err), "{failures} failures");
        ensure!(results[failures] == Ok(b"ok".to_vec()), "the call after {failures} failures");
    }
    let mut shares = Vec::new();
    for failures in [5, 50, 254] {
        let calls = [vec![fail.clone(); failures], vec![given.clone(), echo.clone()]].concat();
        let (n, parts) = (calls.len(), calls.len() as u64 + 1);
        let share = env.gas_given(&env.results(&calls)?, failures);
        let ok = share <= QUERY_GAS / parts && share > (QUERY_GAS - 1_000_000) / parts - 50_000;
        ensure!(ok, "{share}");
        env.note(&format!("gas.share_after_{failures}_failures_of_{n}_calls"), share);
        shares.push(format!("{share} after {failures} of {n}"));
    }
    // A call heavier than its share needs an explicit cap.
    let spins = 1_000_000u64;
    let heavy = env.probe("spin", encode(&spins));
    let mut calls = vec![heavy.clone()];
    calls.extend(vec![echo.clone(); MAX_CALLS - 1]);
    let r = env.results(&calls)?;
    let rest_ok = r[1..].iter().all(|r| *r == Ok(b"ok".to_vec()));
    ensure!(r[0] == Err(CallError::OutOfGas) && rest_ok, "uncapped heavy call: {:?}", r[0]);
    calls[0] = capped(heavy, 200_000_000);
    let r = env.results(&calls)?;
    let rest_ok = r[1..].iter().all(|r| *r == Ok(b"ok".to_vec()));
    let spun = r[0].as_ref().map(|b| decode::<u64>(b)).ok();
    ensure!(spun == Some(spins) && rest_ok, "capped heavy call: {:?}", r[0]);
    // Caps at or over the gas left still take 93% each.
    let batch = |n: usize| [vec![at_default(fail.clone()); n], vec![echo.clone()]].concat();
    ensure!(env.results(&batch(4))?[4] == Ok(b"ok".to_vec()), "four failures at the default");
    env.refused_calls(&batch(5), "OutOfGas").map_err(|e| format!("five at the default: {e}"))?;
    Ok(format!(
        "after 5, 50 and 255 failures the next call succeeds; a probe's share was {}; spin({spins}) fails in a 256-call batch at cap 0 and succeeds at a 200M cap; five failures at the default still exhaust the query",
        shares.join(", ")
    ))
}

/// The share kept back covers the largest reply in the worst case: 255
/// failures at their shares, the last of them ending the batch.
fn reserve(env: &Env) -> Result<String, String> {
    let fail = env.probe("fail", b"x".to_vec());
    let given = env.probe("gas_limit", vec![]);
    let last = |probe: Call| -> Result<u64, String> {
        Ok(env.gas_given(&env.results(&[vec![fail.clone(); 255], vec![probe]].concat())?, 255))
    };
    let share = last(given.clone())?;
    let left = left_before(last(at_default(given))?);
    env.note("gas.left_before_the_last_of_256_calls_after_255_failures", left);
    env.note("gas.share_of_that_last_call", share);
    env.note("gas.reserve_after_it_fails", left - share);
    // The largest reply the buffer holds next to 255 failed results.
    let size = |n: usize| {
        let mut results = vec![Ok(vec![0u8; n])];
        results.extend(vec![Err(CallError::Panic("x".into())); 255]);
        encode(&Aggregate { height: u64::MAX, results }).len()
    };
    let (mut largest, mut over) = (0, ARGBUF_LEN);
    while largest + 1 < over {
        let mid = (largest + over) / 2;
        if size(mid) <= ARGBUF_LEN { largest = mid } else { over = mid }
    }
    let big = |n: usize| env.probe("big", encode(&(n as u32)));
    let direct = env.direct(&big(largest))?;
    for (order, calls) in [
        ("first", [vec![big(largest)], vec![fail.clone(); 255]].concat()),
        ("last", [vec![fail.clone(); 255], vec![big(largest)]].concat()),
    ] {
        let results = env.results(&calls)?;
        let failed = results.iter().filter(|r| r.is_err()).count();
        let reply = results.iter().find_map(|r| r.as_ref().ok());
        ensure!(failed == 255 && reply == Some(&direct), "the big reply {order}: {failed} failed");
    }
    let one_more = [vec![big(largest + 1)], vec![fail; 255]].concat();
    let over = format!("Panic: multicall: reply exceeds {ARGBUF_LEN} bytes at call 0");
    env.refused_calls(&one_more, &over)?;
    env.note("gas.largest_reply_next_to_255_failures", largest);
    Ok(format!(
        "after 255 failures {left} gas was left; the last call's share was {share} and a failing one would leave {}; a {largest}-byte reply came back with 255 failures before or after it, and one byte more hit the buffer",
        left - share
    ))
}

fn writes(env: &Env) -> Result<String, String> {
    let before = env.counter();
    let bump = env.probe("bump", encode(&5u64));
    let r = env.results(&[bump.clone(), env.probe("counter", vec![])])?;
    let seen: Vec<u64> = r.iter().map(|r| decode(r.as_ref().unwrap())).collect();
    ensure!(seen == [before + 5, before + 5], "within the batch: {seen:?}");
    ensure!(env.counter() == before, "the write persisted");
    let again: u64 = decode(env.results(&[bump])?[0].as_ref().unwrap());
    ensure!(again == before + 5, "second query started from {again}");
    let r =
        env.results(&[env.probe("bump_then_fail", encode(&5u64)), env.probe("counter", vec![])])?;
    ensure!(decode::<u64>(r[1].as_ref().unwrap()) == before, "a failed call's write survived");
    Ok(format!("counter {before}: {} inside the batch, {before} after it", before + 5))
}

fn transactions(env: &Env) -> Result<String, String> {
    let send = |contract, fn_name: &str, arg: Vec<u8>| {
        env.chain.call(&env.a, contract, fn_name, arg, CALL_GAS).map_err(|r| format!("{r:?}"))
    };
    let start = env.counter();
    let control = send(env.probe, "bump", encode(&1u64))?;
    ensure!(
        control.err.is_none() && env.counter() == start + 1,
        "a direct bump transaction: {:?}",
        control.err
    );
    let batch = encode(&vec![env.probe("bump", encode(&10u64))]);
    let balance = env.chain.account(&env.a).balance;
    let refused = send(env.multicall, "aggregate", batch.clone())?;
    let err = refused.err.clone().unwrap_or_default();
    ensure!(err.contains(REFUSED), "aggregate in a transaction: {err:?}");
    ensure!(
        refused.gas_spent == refused.gas_limit,
        "spent {} of {}",
        refused.gas_spent,
        refused.gas_limit
    );
    ensure!(
        balance - env.chain.account(&env.a).balance == refused.gas_spent,
        "balance moved by more than the gas"
    );
    ensure!(env.counter() == start + 1, "the batched bump persisted");
    let version = send(env.multicall, "version", vec![])?;
    ensure!(
        version.err.as_deref().unwrap_or_default().contains(REFUSED),
        "version in a transaction: {:?}",
        version.err
    );
    let indirect = send(env.probe, "forward", forward(env.multicall, "aggregate", batch))?;
    ensure!(
        indirect.err.is_none() && env.counter() == start + 1,
        "through the probe: {:?}",
        indirect.err
    );
    Ok(format!(
        "err {err:?}; charged {} gas (the whole limit); counter {} before and after",
        refused.gas_spent,
        start + 1
    ))
}

fn nesting(env: &Env) -> Result<String, String> {
    let inner = encode(&vec![env.probe("echo", b"x".to_vec())]);
    let via_probe =
        env.direct(&env.probe("forward", forward(env.multicall, "aggregate", inner.clone())))?;
    ensure!(
        via_probe == [&[1][..], format!("Panic: {REFUSED}").as_bytes()].concat(),
        "probe → multicall: {}",
        String::from_utf8_lossy(&via_probe)
    );
    let nested = call(env.multicall, "aggregate", inner.clone());
    let version = call(env.multicall, "version", vec![]);
    let results = env.results(&[nested, version, env.probe("echo", b"next".to_vec())])?;
    ensure!(results[..2] == vec![Err(CallError::Panic(REFUSED.into())); 2], "{results:?}");
    ensure!(results[2] == Ok(b"next".to_vec()), "the call after them: {:?}", results[2]);
    let indirect =
        env.results(&[env.probe("forward", forward(env.multicall, "aggregate", inner))])?;
    ensure!(indirect[0] == Ok(via_probe), "multicall → probe → multicall: {indirect:?}");
    Ok("probe → multicall, multicall → multicall and multicall → probe → multicall all refused"
        .into())
}

fn json_mode(env: &Env) -> Result<String, String> {
    let calls = json!([
        { "contract": hex::encode(TRANSFER_CONTRACT.as_bytes()), "fn_name": "chain_id", "fn_args": "", "gas_limit": "0" },
        { "contract": hex::encode(env.probe.as_bytes()), "fn_name": "fail", "fn_args": hex::encode("boom"), "gas_limit": 5_000_000 },
    ]);
    let reply = env
        .node
        .query_json(&env.multicall, "aggregate", &calls.to_string())
        .map_err(|r| format!("{r:?}"))?;
    let chain_id = format!("{:02x}", env.chain.chain_id);
    ensure!(
        reply["results"] == json!([{ "Ok": chain_id }, { "Err": { "Panic": "boom" } }]),
        "{reply}"
    );
    ensure!(
        reply["height"].as_str().and_then(|h| h.parse::<u64>().ok()).is_some(),
        "height {}",
        reply["height"]
    );
    // The same envelope as raw bytes, decoded locally with the same driver.
    let raw = env
        .node
        .query(
            &env.multicall,
            "aggregate",
            &Driver.encode_input_fn("aggregate", &calls.to_string()).unwrap(),
        )
        .map_err(|r| format!("{r:?}"))?;
    let local = Driver.decode_output_fn("aggregate", &raw).unwrap();
    ensure!(local["results"] == reply["results"], "local {local}");
    let version =
        env.node.query_json(&env.multicall, "version", "null").map_err(|r| format!("{r:?}"))?;
    ensure!(version == 1, "version {version}");
    let bad = env
        .node
        .query_json(&env.multicall, "aggregate", "[{\"contract\":\"aa\"}]")
        .err()
        .ok_or("bad JSON accepted")?;
    Ok(format!(
        "{reply}; bad JSON: HTTP {} {}",
        bad.status,
        bad.body.chars().take(100).collect::<String>()
    ))
}

/// The README's curl form: no content type, a `0x`-hex body, a hex reply.
fn hex_mode(env: &Env) -> Result<String, String> {
    let batch = encode(&vec![call(TRANSFER_CONTRACT, "chain_id", vec![])]);
    let reply = env
        .node
        .query_text(&env.multicall, "aggregate", &format!("0x{}", hex::encode(&batch)))
        .map_err(|r| format!("{r:?}"))?;
    let bytes = hex::decode(&reply).map_err(|e| format!("not hex: {e}: {reply}"))?;
    let aggregate: Aggregate = decode(&bytes);
    ensure!(aggregate.results == vec![Ok(vec![env.chain.chain_id])], "{aggregate:?}");
    let version =
        env.node.query_text(&env.multicall, "version", "").map_err(|r| format!("{r:?}"))?;
    ensure!(version == "01000000", "version {version}");
    Ok(format!("aggregate 0x{} answered {reply}; version answered {version}", hex::encode(&batch)))
}

fn capacity(env: &Env) -> Result<String, String> {
    let account = call(TRANSFER_CONTRACT, "account", encode(&env.a.pk));
    let stake = call(STAKE_CONTRACT, "get_stake", encode(&env.a.pk));
    // The most copies that encode within the buffer: alignment padding makes
    // the size per call vary, so count by encoding.
    let most = |size: &dyn Fn(usize) -> usize| {
        (0..2048).take_while(|&n| size(n) <= ARGBUF_LEN).last().unwrap()
    };
    let mut lines = Vec::new();
    for (name, view) in [("transfer.account", account), ("stake.get_stake", stake)] {
        let reply = env.direct(&view)?;
        let input = |n: usize| encode(&vec![view.clone(); n]).len();
        let output = |n: usize| {
            encode(&Aggregate { height: u64::MAX, results: vec![Ok(reply.clone()); n] }).len()
        };
        let (by_input, by_output) = (most(&input), most(&output));
        let cost = env.notes.lock().unwrap()[&format!("gas.in_batch.{name}")].as_u64().unwrap();
        let by_gas = QUERY_GAS / cost;
        let fit = by_input.min(by_output).min(MAX_CALLS).min(by_gas as usize);
        let results = env.results(&vec![view.clone(); fit])?;
        ensure!(
            results.len() == fit && results.iter().all(Result::is_ok),
            "{fit} {name} calls failed"
        );
        let per_call = |size: &dyn Fn(usize) -> usize| (size(100) - size(0)) as f64 / 100.0;
        env.note(&format!("capacity.{name}.input_bytes_per_call"), per_call(&input));
        env.note(&format!("capacity.{name}.output_bytes_per_call"), per_call(&output));
        env.note(&format!("capacity.{name}.reply_bytes"), reply.len());
        env.note(&format!("capacity.{name}.fit_by_input"), by_input);
        env.note(&format!("capacity.{name}.fit_by_output"), by_output);
        env.note(&format!("capacity.{name}.fit_by_gas"), by_gas);
        lines.push(format!("{name}: {fit} per query ({by_input} by input, {by_output} by output, {by_gas} by gas, cap {MAX_CALLS})"));
    }
    Ok(lines.join("; "))
}

fn concurrency(env: &Env) -> Result<String, String> {
    let batch = [
        env.probe("height", vec![]),
        env.probe("counter", vec![]),
        call(STAKE_CONTRACT, "get_stake", encode(&env.a.pk)),
        call(TRANSFER_CONTRACT, "account", encode(&env.b.pk)),
        env.probe("height", vec![]),
    ];
    let stop = AtomicBool::new(false);
    let (replies, throttled, writes) = scope(|s| {
        // B bumps the counter in transactions, so state moves between blocks.
        let writer = s.spawn(|| {
            let mut writes = 0;
            while !stop.load(Ordering::Relaxed) {
                let r = env.chain.call(&env.b, env.probe, "bump", encode(&1u64), CALL_GAS);
                writes += usize::from(matches!(r, Ok(ref r) if r.err.is_none()));
            }
            writes
        });
        let workers: Vec<_> = (0..8)
            .map(|_| {
                s.spawn(|| {
                    let (mut replies, mut throttled, start) = (Vec::new(), 0, Instant::now());
                    while start.elapsed() < Duration::from_secs(10) {
                        match env.aggregate(&batch) {
                            Ok(reply) => replies.push(reply),
                            Err(r) if r.status == 429 => throttled += 1,
                            Err(r) => panic!("query failed: {r:?}"),
                        }
                        sleep(Duration::from_millis(100));
                    }
                    (replies, throttled)
                })
            })
            .collect();
        let (mut all, mut throttled) = (Vec::new(), 0);
        for worker in workers {
            let (replies, t) = worker.join().expect("worker");
            all.extend(replies);
            throttled += t;
        }
        stop.store(true, Ordering::Relaxed);
        (all, throttled, writer.join().expect("writer"))
    });
    let mut by_height: BTreeMap<u64, Vec<Result<Vec<u8>, CallError>>> = BTreeMap::new();
    for reply in &replies {
        let r = &reply.results;
        let (h0, h4): (u64, u64) = (decode(r[0].as_ref().unwrap()), decode(r[4].as_ref().unwrap()));
        ensure!(
            h0 == reply.height && h4 == reply.height,
            "a reply mixed heights {h0}, {h4}, {}",
            reply.height
        );
        let first = by_height.entry(reply.height).or_insert_with(|| r.clone());
        ensure!(first == r, "two replies at height {} differ", reply.height);
    }
    let counters: BTreeSet<u64> =
        by_height.values().map(|r| decode(r[1].as_ref().unwrap())).collect();
    ensure!(
        by_height.len() >= 3 && counters.len() >= 2,
        "{} heights, {} counter values",
        by_height.len(),
        counters.len()
    );
    env.note("concurrency.queries", replies.len());
    env.note("concurrency.throttled", throttled);
    env.note("concurrency.heights", by_height.len());
    env.note("concurrency.writes", writes);
    Ok(format!(
        "{} replies over {} heights from 8 threads ({throttled} throttled), {writes} write transactions, {} counter values; every reply at one height, equal replies per height",
        replies.len(),
        by_height.len(),
        counters.len()
    ))
}
