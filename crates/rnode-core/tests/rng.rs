//! The node's seeded pseudo-random number generator.

use rnode_core::rng::*;

#[test]
fn same_seed_same_sequence() {
    let mut a = Rng::new(42);
    let mut b = Rng::new(42);
    for _ in 0..100 {
        assert_eq!(a.next_u64(), b.next_u64());
    }
}

#[test]
fn different_seeds_differ() {
    let mut a = Rng::new(1);
    let mut b = Rng::new(2);
    assert_ne!(a.next_u64(), b.next_u64());
}

#[test]
fn range_stays_in_bounds_and_covers_it() {
    let mut rng = Rng::new(7);
    let mut seen = [false; 15];
    for _ in 0..10_000 {
        let v = rng.range(15, 30);
        assert!((15..30).contains(&v));
        seen[(v - 15) as usize] = true;
    }
    assert!(seen.iter().all(|&s| s));
}

#[test]
fn empty_range_returns_min() {
    let mut rng = Rng::new(7);
    assert_eq!(rng.range(5, 5), 5);
    assert_eq!(rng.range(9, 3), 9);
}
