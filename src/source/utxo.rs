//! The fast source's in-memory UTXO value map: outpoint → sats.
//!
//! A hashbrown table (std `HashMap`) of 44-byte buckets: the full 36-byte
//! outpoint (exact, no truncated keys) and the value as two `u32`s so the
//! bucket needs no padding. With hashbrown's 7/8 load factor and power-of-two
//! bucket counts that is 45 bytes per bucket including the control byte:
//! 12.1 GB (11.3 GiB) for the 2^28 buckets that hold up to 234.9 M entries.
//! Pre-size it (`utxo_capacity`) so it never resizes near the peak: a resize
//! holds the old and the new table at once.
//!
//! Txids are uniformly distributed, so the hash is the txid's first eight
//! bytes and the vout, mixed with a per-process random seed (so nobody can
//! grind txids that collide in our table).

use {
  bitcoin::{OutPoint, hashes::Hash as _},
  std::{
    collections::HashMap,
    hash::{BuildHasher, Hash, Hasher, RandomState},
    mem,
  },
};

#[derive(Clone, Copy, PartialEq, Eq)]
struct Key(OutPoint);

impl Hash for Key {
  fn hash<H: Hasher>(&self, state: &mut H) {
    let bytes = self.0.txid.as_byte_array();
    let prefix = u64::from_le_bytes(bytes[..8].try_into().unwrap());
    state.write_u64(prefix ^ u64::from(self.0.vout).wrapping_mul(0x9e37_79b9_7f4a_7c15));
  }
}

#[derive(Clone, Copy)]
struct Value([u32; 2]);

impl From<u64> for Value {
  fn from(value: u64) -> Self {
    Self([value as u32, (value >> 32) as u32])
  }
}

impl From<Value> for u64 {
  fn from(Value([lo, hi]): Value) -> Self {
    u64::from(lo) | u64::from(hi) << 32
  }
}

#[derive(Clone, Copy)]
struct SeededBuildHasher {
  seed: u64,
}

impl BuildHasher for SeededBuildHasher {
  type Hasher = SeededHasher;

  fn build_hasher(&self) -> SeededHasher {
    SeededHasher { state: self.seed }
  }
}

struct SeededHasher {
  state: u64,
}

impl Hasher for SeededHasher {
  fn write(&mut self, bytes: &[u8]) {
    // Only `Key` is hashed with this hasher, and it writes one u64; keep a
    // correct (if slow) fallback anyway.
    for &byte in bytes {
      self.write_u64(u64::from(byte));
    }
  }

  fn write_u64(&mut self, value: u64) {
    self.state = fmix64(self.state ^ value);
  }

  fn finish(&self) -> u64 {
    self.state
  }
}

/// MurmurHash3's 64-bit finalizer.
fn fmix64(mut h: u64) -> u64 {
  h ^= h >> 33;
  h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
  h ^= h >> 33;
  h = h.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
  h ^ (h >> 33)
}

pub(crate) struct UtxoMap {
  map: HashMap<Key, Value, SeededBuildHasher>,
}

impl UtxoMap {
  pub(crate) fn with_capacity(capacity: usize) -> Self {
    let seed = RandomState::new().hash_one(0x5eed_u64);
    Self {
      map: HashMap::with_capacity_and_hasher(capacity, SeededBuildHasher { seed }),
    }
  }

  /// Returns the value of the entry it replaced, if any.
  pub(crate) fn insert(&mut self, outpoint: OutPoint, value: u64) -> Option<u64> {
    self.map.insert(Key(outpoint), value.into()).map(u64::from)
  }

  pub(crate) fn remove(&mut self, outpoint: &OutPoint) -> Option<u64> {
    self.map.remove(&Key(*outpoint)).map(u64::from)
  }

  pub(crate) fn len(&self) -> usize {
    self.map.len()
  }

  /// Entries it holds before it has to grow.
  pub(crate) fn capacity(&self) -> usize {
    self.map.capacity()
  }

  /// Approximate size of the table's allocation.
  pub(crate) fn allocated_bytes(&self) -> u64 {
    let capacity = self.map.capacity() as u64;
    if capacity == 0 {
      return 0;
    }
    // hashbrown keeps 1/8 of the buckets free (small tables aside).
    let buckets = (capacity * 8).div_ceil(7).next_power_of_two();
    buckets * (Self::BUCKET_BYTES as u64 + 1) + 16
  }

  pub(crate) const BUCKET_BYTES: usize = mem::size_of::<(Key, Value)>();
}

#[cfg(test)]
mod tests {
  use {super::*, bitcoin::Txid};

  fn outpoint(n: u64, vout: u32) -> OutPoint {
    let mut bytes = [0u8; 32];
    bytes[..8].copy_from_slice(&n.to_le_bytes());
    bytes[31] = 0xaa;
    OutPoint {
      txid: Txid::from_byte_array(bytes),
      vout,
    }
  }

  #[test]
  fn bucket_is_44_bytes() {
    assert_eq!(UtxoMap::BUCKET_BYTES, 44);
  }

  #[test]
  fn insert_remove_overwrite() {
    let mut map = UtxoMap::with_capacity(0);
    assert_eq!(map.insert(outpoint(1, 0), 5), None);
    assert_eq!(map.insert(outpoint(1, 1), 0), None);
    assert_eq!(map.insert(outpoint(2, 0), u64::MAX), None);
    assert_eq!(map.len(), 3);
    assert_eq!(map.insert(outpoint(1, 0), 7), Some(5));
    assert_eq!(map.len(), 3);
    assert_eq!(map.remove(&outpoint(1, 0)), Some(7));
    assert_eq!(map.remove(&outpoint(1, 0)), None);
    assert_eq!(map.remove(&outpoint(1, 1)), Some(0));
    assert_eq!(map.remove(&outpoint(2, 0)), Some(u64::MAX));
    assert_eq!(map.len(), 0);
  }

  #[test]
  fn keys_differing_only_outside_the_hashed_prefix_are_distinct() {
    let mut map = UtxoMap::with_capacity(16);
    let a = outpoint(9, 0);
    let mut b = a;
    let mut bytes = *b.txid.as_byte_array();
    bytes[20] ^= 1;
    b.txid = Txid::from_byte_array(bytes);
    map.insert(a, 1);
    map.insert(b, 2);
    assert_eq!(map.len(), 2);
    assert_eq!(map.remove(&a), Some(1));
    assert_eq!(map.remove(&b), Some(2));
  }

  #[test]
  fn capacity_and_allocation() {
    let map = UtxoMap::with_capacity(1_000);
    assert!(map.capacity() >= 1_000);
    assert_eq!(map.allocated_bytes(), 2048 * 45 + 16);
  }
}
