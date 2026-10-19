//! `reply_len` and `ResolversOnHeap` against rkyv itself: the contract sizes
//! its reply with the first and encodes it with the second.
use dusk_multicall::{ARGBUF_LEN, Aggregate, CallError, MAX_CALLS, out_of_line, reply_len};
use rkyv::ser::Serializer;
use rkyv::ser::serializers::{BufferScratch, BufferSerializer, CompositeSerializer};
use rkyv::{Archive, Infallible, Serialize};

/// `Aggregate` without `ResolversOnHeap`.
#[derive(Archive, Serialize)]
struct Plain {
    height: u64,
    results: Vec<Result<Vec<u8>, CallError>>,
}

/// Results of every kind and many lengths, so that the paddings vary.
fn results(n: usize, seed: usize) -> Vec<Result<Vec<u8>, CallError>> {
    (0..n)
        .map(|i| match (i + seed) % 7 {
            0 => Ok(vec![7; (i * 31 + seed) % 23]),
            1 => Ok(vec![]),
            2 => Err(CallError::Panic("p".repeat((i + seed) % 12))),
            3 => Err(CallError::Panic("é".repeat((i + seed) % 9))),
            4 => Err(CallError::OutOfGas),
            5 => Err(CallError::DoesNotExist),
            _ => Err(CallError::Unknown),
        })
        .collect()
}

#[test]
fn reply_len_is_the_archive_length() {
    for n in (0..40).chain([255, 256]) {
        for seed in 0..7 {
            let results = results(n, seed);
            let extra = results.iter().map(out_of_line).sum();
            let aggregate = Aggregate { height: 9, results };
            let bytes = rkyv::to_bytes::<_, 1024>(&aggregate).unwrap();
            assert_eq!(bytes.len(), reply_len(n, extra), "{n} results, seed {seed}");
        }
    }
    // Out-of-line sizes that are not a multiple of the slot alignment, so the
    // padding before the slot array is load-bearing.
    for extra in [1usize, 2, 3, 5, 6, 7, 9, 201, 203] {
        let results = vec![Ok(vec![0u8; extra])];
        assert_eq!(out_of_line(&results[0]), extra);
        let bytes = rkyv::to_bytes::<_, 1024>(&Aggregate { height: 9, results }).unwrap();
        assert_eq!(bytes.len(), reply_len(1, extra), "one {extra}-byte reply");
    }
}

#[test]
fn resolvers_on_heap_keep_the_archive_unchanged() {
    for n in [0, 1, 2, 3, 100, 256] {
        let results = results(n, n);
        let plain = rkyv::to_bytes::<_, 1024>(&Plain { height: 5, results: results.clone() });
        let ours = rkyv::to_bytes::<_, 1024>(&Aggregate { height: 5, results });
        assert_eq!(plain.unwrap().as_slice(), ours.unwrap().as_slice(), "{n} results");
    }
}

/// Encodes into a 64 KiB buffer with a fixed 1 KiB scratch space and no
/// fallback, as uplink's `wrap_call` encodes what an export returns.
fn encode_like_wrap_call<T>(value: &T) -> Result<usize, String>
where
    T: for<'a> Serialize<
        CompositeSerializer<BufferSerializer<&'a mut [u8]>, BufferScratch<&'a mut [u8; 1024]>>,
    >,
{
    let mut buf = vec![0u8; ARGBUF_LEN];
    let mut scratch = [0u8; 1024];
    let mut serializer = CompositeSerializer::new(
        BufferSerializer::new(&mut buf[..]),
        BufferScratch::new(&mut scratch),
        Infallible,
    );
    serializer.serialize_value(value).map_err(|e| format!("{e:?}"))?;
    Ok(serializer.pos())
}

#[test]
fn a_full_reply_needs_no_scratch_space() {
    let results = vec![Err(CallError::OutOfGas); MAX_CALLS];
    let ours = encode_like_wrap_call(&Aggregate { height: 1, results: results.clone() });
    assert_eq!(ours, Ok(reply_len(MAX_CALLS, 0)));
    // rkyv's own `Vec` encoding keeps one resolver per result in the scratch
    // space: 16 bytes each here, 8 in a wasm32 contract.
    let plain = encode_like_wrap_call(&Plain { height: 1, results }).unwrap_err();
    assert!(plain.contains("OutOfScratch"), "{plain}");
}
