//! A cut panic message keeps only the bytes it reports, in an allocation of
//! that size, so a batch of long panics holds no more memory than its reply.
use dusk_multicall::{CallError, MAX_PANIC_BYTES};
use piecrust_uplink::ContractError;

fn kept(message: String) -> String {
    match CallError::from(ContractError::Panic(message)) {
        CallError::Panic(kept) => kept,
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_cut_panic_keeps_only_its_kept_bytes() {
    for message in ["x".repeat(10_000), "é".repeat(5_000), "€".repeat(4_000)] {
        let original = message.clone();
        let cut = kept(message);
        assert!(original.starts_with(&cut), "the cut is a prefix");
        // At most 200 bytes, cut at a character boundary.
        assert!(cut.len() <= MAX_PANIC_BYTES && cut.len() > MAX_PANIC_BYTES - 4, "{}", cut.len());
        assert_eq!(cut.capacity(), cut.len(), "a {}-byte message", original.len());
    }
    assert_eq!(kept("boom".into()), "boom");
}
