//! The fair share a call with cap 0 gets, one share kept back, and its floor.
use dusk_multicall::fair_share;

#[test]
fn a_share_is_the_gas_left_over_the_calls_left_plus_one() {
    assert_eq!(fair_share(3_000_000_000, 256), 11_673_151);
    assert_eq!(fair_share(3_000_000_000, 3), 750_000_000);
    assert_eq!(fair_share(3_000_000_000, 1), 1_500_000_000, "a lone call gets half");
    assert_eq!(fair_share(14, 6), 2);
    assert_eq!(fair_share(u64::MAX, 1), u64::MAX / 2);
    assert_eq!(fair_share(10, 0), 10);
}

#[test]
fn a_share_is_never_zero() {
    // The VM reads a limit of 0 as its default, 93% of the gas left.
    for (left, calls) in [(0, 1), (0, 256), (5, 6), (256, 256), (1, 1)] {
        assert_eq!(fair_share(left, calls), 1, "{left} over {calls}");
    }
}
