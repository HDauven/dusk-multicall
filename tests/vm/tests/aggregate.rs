//! What `aggregate` returns and refuses.
use multicall_vm_tests::*;

const REFUSED: &str = "multicall: direct queries only";
/// How the exports refuse a bad argument, in the words of uplink's `wrap_call`.
const INVALID: &str = "Argument should correctly deserialize";

/// Calls that fail, with the error each one reports.
fn failing() -> Vec<(Call, CallError)> {
    vec![
        (call(PROBE, "fail", b"boom".to_vec()), CallError::Panic("boom".into())),
        (capped(call(PROBE, "burn", vec![]), 1_000_000), CallError::OutOfGas),
        (call(MISSING, "echo", vec![]), CallError::DoesNotExist),
        (call(PROBE, "no_such_fn", vec![]), CallError::Unknown),
        (call(PROBE, "init", vec![]), CallError::Unknown),
        (call(PROBE, "bump", vec![1, 2, 3]), CallError::Panic("probe: invalid argument".into())),
    ]
}

fn forward(target: ContractId, fn_name: &str, arg: Vec<u8>) -> Vec<u8> {
    encode(&(target, fn_name.to_string(), arg))
}

#[test]
fn replies_are_the_direct_replies_in_call_order() {
    let w = World::new();
    let calls = [
        call(PROBE, "echo", b"first".to_vec()),
        call(PROBE, "height", vec![]),
        call(PROBE, "big", encode(&1000u32)),
        call(PROBE, "counter", vec![]),
        call(PROBE, "echo", vec![]),
        call(PROBE, "echo", b"last".to_vec()),
    ];
    let (aggregate, _) = w.aggregate(&calls).unwrap();
    assert_eq!(aggregate.height, HEIGHT);
    assert_eq!(aggregate.results.len(), calls.len());
    for (call, result) in calls.iter().zip(&aggregate.results) {
        let direct = w.reply(call.contract, &call.fn_name, &call.fn_args);
        assert_eq!(result.as_ref().unwrap(), &direct, "{}", call.fn_name);
    }
    assert_eq!(decode::<u64>(aggregate.results[1].as_ref().unwrap()), HEIGHT);
}

#[test]
fn every_call_gets_exactly_its_argument_bytes() {
    let w = World::new();
    let args: Vec<Vec<u8>> = [0usize, 1, 3, 8, 255, 4096, 30_000]
        .iter()
        .map(|&n| (0..n).map(|i| (i * 7 + n) as u8).collect())
        .collect();
    let calls: Vec<Call> = args.iter().map(|a| call(PROBE, "echo", a.clone())).collect();
    for (arg, result) in args.iter().zip(w.results(&calls)) {
        assert_eq!(&result.unwrap(), arg);
    }
}

#[test]
fn an_empty_batch_returns_only_the_height() {
    let w = World::new();
    let (aggregate, _) = w.aggregate(&[]).unwrap();
    assert_eq!(aggregate, Aggregate { height: HEIGHT, results: vec![] });
}

#[test]
fn version_is_one_whatever_the_argument() {
    let w = World::new();
    for arg in [vec![], vec![0xff; 33], vec![0x5a; ARGBUF_LEN]] {
        assert_eq!(decode::<u32>(&w.reply(MULTICALL, "version", &arg)), 1, "{} bytes", arg.len());
    }
}

#[test]
fn callees_see_the_multicall_as_their_caller() {
    let w = World::new();
    assert_eq!(w.reply(PROBE, "caller_id", &[]), Vec::<u8>::new(), "a query has no caller");
    let results = w.results(&[call(PROBE, "caller_id", vec![])]);
    assert_eq!(results[0].as_ref().unwrap(), &MULTICALL.to_bytes().to_vec());
}

#[test]
fn every_failure_is_reported_and_the_batch_goes_on() {
    let w = World::new();
    let mut calls = vec![call(PROBE, "echo", b"before".to_vec())];
    for (failing, _) in failing() {
        calls.push(failing);
        calls.push(call(PROBE, "echo", b"after".to_vec()));
    }
    let results = w.results(&calls);
    assert_eq!(results.len(), calls.len());
    assert_eq!(results[0].as_ref().unwrap(), b"before");
    for (i, (_, error)) in failing().into_iter().enumerate() {
        assert_eq!(results[1 + 2 * i], Err(error));
        assert_eq!(results[2 + 2 * i].as_ref().unwrap(), b"after");
    }
}

#[test]
fn a_failed_call_leaves_no_state_for_the_calls_after_it() {
    let w = World::new();
    let results =
        w.results(&[call(PROBE, "bump_then_fail", encode(&5u64)), call(PROBE, "counter", vec![])]);
    assert_eq!(results[0], Err(CallError::Panic("probe: reverted at 5".into())));
    assert_eq!(decode::<u64>(results[1].as_ref().unwrap()), 0);
}

#[test]
fn writes_reach_later_calls_but_never_outlive_the_query() {
    let w = World::new();
    let bump = call(PROBE, "bump", encode(&5u64));
    let results = w.results(&[bump.clone(), call(PROBE, "counter", vec![])]);
    assert_eq!(decode::<u64>(results[0].as_ref().unwrap()), 5);
    assert_eq!(decode::<u64>(results[1].as_ref().unwrap()), 5);
    assert_eq!(decode::<u64>(&w.reply(PROBE, "counter", &[])), 0);
    assert_eq!(decode::<u64>(w.results(&[bump])[0].as_ref().unwrap()), 5);
}

#[test]
fn contract_callers_are_refused() {
    let w = World::new();
    let envelope = encode(&vec![call(PROBE, "echo", b"x".to_vec())]);
    let refused = [&[1][..], format!("Panic: {REFUSED}").as_bytes()].concat();
    // The probe at the transfer contract's ID stands in for a transaction.
    for caller in [PROBE, TRANSFER] {
        for (fn_name, arg) in [("aggregate", envelope.clone()), ("version", vec![])] {
            let reply = w.reply(caller, "forward", &forward(MULTICALL, fn_name, arg));
            assert_eq!(reply, refused, "{fn_name} called by {caller:?}");
        }
    }
}

#[test]
fn nested_multicalls_are_refused() {
    let w = World::new();
    let inner = encode(&vec![call(PROBE, "echo", b"x".to_vec())]);
    let nested = call(MULTICALL, "aggregate", inner.clone());
    let version = call(MULTICALL, "version", vec![]);
    let results = w.results(&[nested, version, call(PROBE, "echo", b"next".to_vec())]);
    assert_eq!(results[..2], vec![Err(CallError::Panic(REFUSED.into())); 2]);
    assert_eq!(results[2].as_ref().unwrap(), b"next");
    // Nor through another contract.
    let indirect = call(PROBE, "forward", forward(MULTICALL, "aggregate", inner));
    let reply = w.results(&[indirect]).remove(0).unwrap();
    assert_eq!(reply, [&[1][..], format!("Panic: {REFUSED}").as_bytes()].concat());
    // A bad argument is refused as one, before the caller is checked.
    let results = w.results(&[call(MULTICALL, "aggregate", vec![0xff; 12])]);
    assert!(
        matches!(&results[0], Err(CallError::Panic(m)) if m.starts_with(INVALID)),
        "{results:?}"
    );
}

#[test]
fn malformed_envelopes_are_refused() {
    let w = World::new();
    let valid = encode(&vec![call(PROBE, "echo_a_long_name", b"abc".to_vec())]);
    let (bytes, _) = w.aggregate_raw(&valid).unwrap();
    assert_eq!(decode::<Aggregate>(&bytes).results, vec![Err(CallError::Unknown)]);

    let mut not_utf8 = valid.clone();
    let name = valid.windows(16).position(|w| w == b"echo_a_long_name").unwrap();
    not_utf8[name] = 0xff;
    let mut too_long = valid.clone();
    let n = too_long.len();
    too_long[n - 4..].copy_from_slice(&1000u32.to_le_bytes());

    for (case, bytes) in [
        ("empty", vec![]),
        ("too short", vec![0xff; 3]),
        ("garbage", vec![0xff; 64]),
        ("truncated", valid[..valid.len() - 1].to_vec()),
        ("name not UTF-8", not_utf8),
        ("list past the end", too_long),
    ] {
        // The export validates the argument before the method runs.
        let error = w.aggregate_raw(&bytes).unwrap_err();
        assert!(error.starts_with(INVALID), "{case}: {error}");
    }
}

#[test]
fn at_most_max_calls() {
    let w = World::new();
    let echo = call(PROBE, "echo", vec![]);
    assert_eq!(w.results(&vec![echo.clone(); MAX_CALLS]).len(), MAX_CALLS);
    let error = w.aggregate(&vec![echo; MAX_CALLS + 1]).unwrap_err();
    assert_eq!(error, format!("multicall: {} calls, at most {MAX_CALLS}", MAX_CALLS + 1));
}

#[test]
fn an_envelope_over_the_argument_buffer_never_reaches_the_contract() {
    let w = World::new();
    let envelope = |n: usize| encode(&vec![call(PROBE, "echo", vec![7u8; n])]);
    let largest = (ARGBUF_LEN - 128..ARGBUF_LEN)
        .take_while(|&n| envelope(n).len() <= ARGBUF_LEN)
        .last()
        .unwrap();
    let results = w.results(&[call(PROBE, "echo", vec![7u8; largest])]);
    assert_eq!(results[0].as_ref().unwrap().len(), largest);
    let error = w.aggregate_raw(&envelope(largest + 1)).unwrap_err();
    assert!(error.contains("out of bounds"), "{error}");
}

#[test]
fn a_reply_over_the_argument_buffer_fails_cleanly() {
    let w = World::new();
    let big = |n: u32| call(PROBE, "big", encode(&n));
    let error = w.aggregate(&[big(40_000), big(30_000), big(1)]).unwrap_err();
    assert_eq!(error, format!("multicall: reply exceeds {ARGBUF_LEN} bytes at call 1"));
    // A reply that fits alone but not inside the envelope.
    let largest = (ARGBUF_LEN as u32 - 128..=ARGBUF_LEN as u32)
        .take_while(|&n| w.aggregate(&[big(n)]).is_ok())
        .last()
        .unwrap();
    assert_eq!(largest as usize, ARGBUF_LEN - 32);
    let error = w.aggregate(&[big(largest + 1)]).unwrap_err();
    assert_eq!(error, format!("multicall: reply exceeds {ARGBUF_LEN} bytes at call 0"));
    let results = w.results(&[big(largest)]);
    assert_eq!(results[0].as_ref().unwrap(), &w.reply(PROBE, "big", &encode(&largest)));
    println!("largest single reply: {largest} bytes");
}

/// `shout(n)` panics with `n` bytes: `x` if `n` is odd, `xx` if even, then
/// two-byte `é`s. Byte 200 falls inside an `é` when `n` is odd.
fn shout(n: u32) -> Call {
    call(PROBE, "shout", encode(&n))
}

#[test]
fn a_panic_message_is_cut_to_200_bytes_at_a_character_boundary() {
    let w = World::new();
    let ok = call(PROBE, "echo", b"ok".to_vec());
    for n in [0u32, 1, 8, 9, 199, 200, 201, 202, 60_001, 65_531, 65_532] {
        let message = message(w.query(PROBE, "shout", &encode(&n)).unwrap_err());
        let n = n as usize;
        let kept = if n <= MAX_PANIC_BYTES { n } else { MAX_PANIC_BYTES - n % 2 };
        assert_eq!(message.len(), n);
        let results = w.results(&[ok.clone(), shout(n as u32), ok.clone()]);
        assert_eq!(results[1], Err(CallError::Panic(message[..kept].into())), "{n}");
        assert_eq!(
            (results[0].as_deref(), results[2].as_deref()),
            (Ok(&b"ok"[..]), Ok(&b"ok"[..]))
        );
    }
    // An argument-encoded message too, with three-byte characters.
    let euros = "€".repeat(400);
    let results = w.results(&[call(PROBE, "fail", euros.clone().into_bytes())]);
    assert_eq!(results, vec![Err(CallError::Panic(euros[..198].into()))]);
}

#[test]
fn the_results_of_max_calls_failures_always_fit() {
    let w = World::new();
    let results = w.results(&vec![shout(65_532); MAX_CALLS]);
    assert!(results.iter().all(|r| matches!(r, Err(CallError::Panic(m)) if m.len() == 200)));
    let (bytes, _) = w.aggregate_raw(&encode(&vec![shout(65_532); MAX_CALLS])).unwrap();
    assert_eq!(bytes.len(), reply_len(MAX_CALLS, MAX_CALLS * MAX_PANIC_BYTES));
    println!("{MAX_CALLS} failures at the longest message: a {}-byte reply", bytes.len());
}

#[test]
fn a_feeder_query_streams_callee_chunks_outside_the_reply() {
    let w = World::new();
    let calls = [call(PROBE, "feed_back", b"chunk".to_vec()), call(PROBE, "echo", b"ok".to_vec())];
    // A plain query has no feeder: the feeding call traps, the next one runs.
    let results = w.results(&calls);
    assert_eq!(results, [Err(CallError::Unknown), Ok(b"ok".to_vec())]);
    // A feeder query has no caller either, so the multicall runs, and the
    // callee's chunk goes to the client beside the reply, which Rusk drops.
    let (reply, chunks) = w.feeder_query(MULTICALL, "aggregate", &encode(&calls.to_vec()));
    let results = decode::<Aggregate>(&reply.unwrap()).results;
    assert_eq!(results, [Ok(vec![]), Ok(b"ok".to_vec())]);
    assert_eq!(chunks, [b"chunk".to_vec()]);
}

#[test]
fn a_query_that_runs_out_of_gas_fails_whole() {
    let w = World::new();
    let batch = encode(&vec![call(PROBE, "echo", vec![]); MAX_CALLS]);
    let error = w.query_with_gas(MULTICALL, "aggregate", &batch, 50_000).unwrap_err();
    assert_eq!(message(error), "OutOfGas");
}
