//! Gas: explicit caps, the fair share a cap of 0 gets, what failures take, the
//! share kept back for the reply, and the numbers the README quotes. Run with
//! `--nocapture` to see the report.
use multicall_vm_tests::*;

/// A cap the VM always replaces with its default, 93% of the gas left.
const DEFAULT: u64 = u64::MAX;

/// Loop rounds that need more gas than a 256-call batch's fair share.
const SPINS: u64 = 1_000_000;

fn gas_limit() -> Call {
    call(PROBE, "gas_limit", vec![])
}

/// The gas limit the probe's `gas_limit` call at `i` was given.
fn given(results: &[Result<Vec<u8>, CallError>], i: usize) -> u64 {
    decode::<u64>(results[i].as_ref().unwrap())
}

/// 93% of `left`, as the VM computes its default limit.
fn default_of(left: u64) -> u64 {
    left / 100 * 93 + left % 100 * 93 / 100
}

#[test]
fn a_cap_below_the_gas_left_is_the_callee_limit() {
    let w = World::new();
    for cap in [100_000, 2_000_000, 1_000_000_000, QUERY_GAS - 10_000_000] {
        assert_eq!(given(&w.results(&[capped(gas_limit(), cap)]), 0), cap);
    }
}

#[test]
fn a_cap_at_or_over_the_gas_left_gets_93_percent_of_it() {
    let w = World::new();
    let results = w.results(&[capped(gas_limit(), DEFAULT), capped(gas_limit(), QUERY_GAS)]);
    let first = given(&results, 0);
    // Setting up the batch takes well under a million gas.
    assert!(first <= default_of(QUERY_GAS) && first > default_of(QUERY_GAS - 1_000_000));
    let second = given(&results, 1);
    assert!(second < first && first - second < 50_000, "{first} then {second}");
}

#[test]
fn a_cap_of_0_shares_the_gas_left_over_the_calls_left_plus_one() {
    let w = World::new();
    let results = w.results(&vec![gas_limit(); 4]);
    let shares: Vec<u64> = (0..4).map(|i| given(&results, i)).collect();
    // The gas left when call i starts is its share times the calls left plus
    // one. Each probe spends little, so the gas left barely moves and the
    // shares grow, the last to half of it.
    let left: Vec<u64> = (0..4).map(|i| shares[i] * (5 - i as u64)).collect();
    assert!(left[0] <= QUERY_GAS && left[0] > QUERY_GAS - 1_000_000, "{shares:?}");
    for i in 1..4 {
        assert!(left[i] < left[i - 1] && left[i - 1] - left[i] < 50_000, "{shares:?}");
    }
    // A lone call gets half the query's gas.
    let lone = given(&w.results(&[gas_limit()]), 0);
    assert!(lone <= QUERY_GAS / 2 && lone > (QUERY_GAS - 1_000_000) / 2, "{lone}");
    println!("shares of a 4-call batch: {shares:?}; a lone call: {lone}");
}

#[test]
fn a_failed_call_costs_its_whole_limit() {
    let w = World::new();
    let fail = call(PROBE, "fail", b"x".to_vec());
    let after = |first: Call| given(&w.results(&[first, capped(gas_limit(), DEFAULT)]), 1);
    let fresh = given(&w.results(&[capped(gas_limit(), DEFAULT)]), 0);

    // At the VM's default a cheap panic takes 93% of the gas left.
    let left = QUERY_GAS - default_of(QUERY_GAS);
    let got = after(capped(fail.clone(), DEFAULT));
    assert!(got <= default_of(left) && got > default_of(left - 1_000_000), "{got}");
    // With a cap, the cap.
    let got = after(capped(fail.clone(), 1_000_000));
    assert!(got < fresh - default_of(1_000_000) && got > fresh - 2_000_000, "{got}");
    // With a fair share, its share: a third of the gas left, with one call
    // after it and one share kept back.
    let left = QUERY_GAS / 3 * 2;
    let got = after(fail);
    assert!(got <= default_of(left) && got > default_of(left - 1_000_000), "{got}");
    // A missing contract is refused before the VM hands over any gas.
    let got = after(call(MISSING, "x", vec![]));
    assert!(fresh - got < 50_000, "{fresh} then {got}");
}

#[test]
fn fair_shares_keep_failures_from_starving_later_calls() {
    let w = World::new();
    let fail = call(PROBE, "fail", b"x".to_vec());
    let echo = call(PROBE, "echo", b"ok".to_vec());
    for failures in [1, 5, 50, 255] {
        let calls = [vec![fail.clone(); failures], vec![echo.clone()]].concat();
        let results = w.results(&calls);
        assert!(results[..failures].iter().all(Result::is_err));
        assert_eq!(results[failures].as_ref().unwrap(), b"ok", "after {failures} failures");
    }
    // A probe after the failures gets about the query's gas over the calls
    // plus one.
    for failures in [5, 50, 254] {
        let calls = [vec![fail.clone(); failures], vec![gas_limit(), echo.clone()]].concat();
        let n = calls.len() as u64 + 1;
        let got = given(&w.results(&calls), failures);
        assert!(got <= QUERY_GAS / n && got > (QUERY_GAS - 1_000_000) / n - 50_000, "{got}");
        println!("after {failures} failures, call {} of {} got {got}", failures + 1, n - 1);
    }
}

#[test]
fn a_heavy_call_in_a_large_batch_needs_an_explicit_cap() {
    let w = World::new();
    let heavy = call(PROBE, "spin", encode(&SPINS));
    let (_, direct) = w.query(PROBE, "spin", &encode(&SPINS)).unwrap();
    let share = QUERY_GAS / (MAX_CALLS as u64 + 1);
    assert!(direct > share && direct < QUERY_GAS / 10, "{direct}");
    let mut calls = vec![heavy.clone()];
    calls.extend(vec![call(PROBE, "echo", b"ok".to_vec()); MAX_CALLS - 1]);
    let results = w.results(&calls);
    assert_eq!(results[0], Err(CallError::OutOfGas));
    assert!(results[1..].iter().all(|r| r.as_deref() == Ok(&b"ok"[..])));
    calls[0] = capped(heavy, 2 * direct);
    let results = w.results(&calls);
    assert_eq!(decode::<u64>(results[0].as_ref().unwrap()), SPINS);
    assert!(results[1..].iter().all(|r| r.as_deref() == Ok(&b"ok"[..])));
    println!("spin({SPINS}) needs {direct} gas; a 256-call share is {share}");
}

#[test]
fn the_reserve_covers_the_largest_reply() {
    let w = World::new();
    let fail = call(PROBE, "fail", b"x".to_vec());
    let big = |n: u32| call(PROBE, "big", encode(&n));
    // The largest reply the buffer holds next to 255 failed results.
    let size = |n: usize| {
        let mut results = vec![Ok(vec![0; n])];
        results.extend(vec![Err(CallError::Panic("x".into())); 255]);
        encode(&Aggregate { height: HEIGHT, results }).len()
    };
    let (mut largest, mut over) = (0, ARGBUF_LEN);
    while largest + 1 < over {
        let mid = (largest + over) / 2;
        if size(mid) <= ARGBUF_LEN { largest = mid } else { over = mid }
    }
    let largest = largest as u32;
    // With the big call first, the last of 255 failures ends the batch, takes
    // its share and leaves only the share kept back for the encoding.
    for calls in [
        [vec![big(largest)], vec![fail.clone(); 255]].concat(),
        [vec![fail.clone(); 255], vec![big(largest)]].concat(),
    ] {
        let results = w.results(&calls);
        assert_eq!(results.iter().filter(|r| r.is_err()).count(), 255);
        let reply = results.iter().find_map(|r| r.as_ref().ok()).unwrap();
        assert_eq!(reply, &w.reply(PROBE, "big", &encode(&largest)));
    }
    // One byte more and the buffer, not the gas, refuses the reply.
    let calls = [vec![big(largest + 1)], vec![fail; 255]].concat();
    let error = w.aggregate(&calls).unwrap_err();
    assert_eq!(error, format!("multicall: reply exceeds {ARGBUF_LEN} bytes at call 0"));
    println!("largest reply next to 255 failures: {largest} bytes");
}

#[test]
fn caps_at_or_over_the_gas_left_can_still_starve_later_calls() {
    let w = World::new();
    let fail = capped(call(PROBE, "fail", b"x".to_vec()), DEFAULT);
    let echo = call(PROBE, "echo", b"ok".to_vec());
    let batch = |n: usize| [vec![fail.clone(); n], vec![echo.clone()]].concat();
    // Each failure leaves 7% of the gas: 3e9, 210M, 14.7M, 1.03M, 72k, 5k.
    assert_eq!(w.results(&batch(4))[4].as_ref().unwrap(), b"ok");
    assert_eq!(w.aggregate(&batch(5)).unwrap_err(), "OutOfGas");
}

#[test]
fn gas_report() {
    let w = World::new();
    let echo = call(PROBE, "echo", b"abc".to_vec());
    let (_, direct) = w.query(PROBE, "echo", b"abc").unwrap();
    let batch = |n: usize| w.aggregate(&vec![echo.clone(); n]).unwrap().1;
    let empty = batch(0);
    let (one, full) = (batch(1), batch(MAX_CALLS));
    let per_call = (full - one) / (MAX_CALLS as u64 - 1);
    let (_, version) = w.query(MULTICALL, "version", &[]).unwrap();
    let big = |n: u32| w.aggregate(&[call(PROBE, "big", encode(&n))]).unwrap().1;
    let (_, direct_big) = w.query(PROBE, "big", &encode(&30_000u32)).unwrap();
    println!("version query: {version} gas");
    println!("direct echo query: {direct} gas");
    println!("aggregate([]): {empty} gas");
    println!("aggregate([echo]): {one} gas");
    println!("aggregate([echo; {MAX_CALLS}]): {full} gas, {per_call} per call");
    println!(
        "30,000-byte reply: direct {direct_big}, in a batch {} ({} more)",
        big(30_000),
        big(30_000) - empty - direct_big
    );
    println!(
        "{MAX_CALLS} echo calls take {:.4}% of the query bound",
        full as f64 * 100.0 / QUERY_GAS as f64
    );
    // The worst case for the reserve: 255 failures at their shares, then the
    // last call. Its share is what a failing last call would leave behind.
    let fail = call(PROBE, "fail", b"x".to_vec());
    let last =
        |probe: Call| given(&w.results(&[vec![fail.clone(); 255], vec![probe]].concat()), 255);
    let share = last(gas_limit());
    let left = last(capped(gas_limit(), DEFAULT)) * 100 / 93;
    println!(
        "after 255 failures: {left} gas left, the last call's share {share}, the reserve {}",
        left - share
    );
    assert!(empty < 1_000_000 && per_call < 1_000_000);
}
