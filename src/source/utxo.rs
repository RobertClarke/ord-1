//! The fast source's in-memory UTXO value map: outpoint → sats.
//!
//! 256 hashbrown tables (std `HashMap`), sharded by txid byte 8, of 44-byte
//! buckets: the full 36-byte outpoint (exact, no truncated keys) and the
//! value as two `u32`s so the bucket needs no padding. With hashbrown's 7/8
//! load factor and power-of-two bucket counts that is 45 bytes per bucket
//! including the control byte: 12.1 GB (11.25 GiB) for 2^28 buckets, which
//! hold up to 234.9 M entries. Pre-size it (`utxo_capacity`).
//!
//! Why shards: a remove can leave a tombstone that only a rehash clears, and
//! once a hashbrown table is more than half full it rehashes by doubling. On
//! mainnet one table accumulated 37 M tombstones around height 886,000 (with
//! ~180 M entries), a few percent away from doubling to 33.75 GiB. Each shard
//! is instead rebuilt in place when tombstones eat its headroom (`drain`
//! keeps the allocation; ~32 MB transient), and a shard that really outgrows
//! its table doubles alone (+47 MB), not the whole map.
//!
//! Txids are uniformly distributed, so the hash is the txid's first eight
//! bytes and the vout, mixed with a per-process random seed (so nobody can
//! grind txids that collide in our table), and the shard is another byte.

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

/// Shards, selected by txid byte 8 (the hash uses bytes 0–7).
const SHARDS: usize = 256;

pub(crate) struct UtxoMap {
  shards: Box<[Shard]>,
  len: usize,
}

struct Shard {
  map: HashMap<Key, Value, SeededBuildHasher>,
  /// Entries the table holds when it has no tombstones.
  full_capacity: usize,
}

/// What `UtxoMap::maintain` did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Maintenance {
  /// Shards rebuilt in place to clear tombstones.
  pub(crate) compacted: usize,
  /// Shards hashbrown doubled since the last call.
  pub(crate) grown: usize,
}

impl UtxoMap {
  pub(crate) fn with_capacity(capacity: usize) -> Self {
    let seed = RandomState::new().hash_one(0x5eed_u64);
    let per_shard = capacity.div_ceil(SHARDS);
    Self {
      shards: (0..SHARDS)
        .map(|_| {
          let map = HashMap::with_capacity_and_hasher(per_shard, SeededBuildHasher { seed });
          Shard {
            full_capacity: map.capacity(),
            map,
          }
        })
        .collect(),
      len: 0,
    }
  }

  fn shard(&mut self, outpoint: &OutPoint) -> &mut Shard {
    &mut self.shards[usize::from(outpoint.txid.as_byte_array()[8])]
  }

  /// Returns the value of the entry it replaced, if any.
  pub(crate) fn insert(&mut self, outpoint: OutPoint, value: u64) -> Option<u64> {
    let replaced = self
      .shard(&outpoint)
      .map
      .insert(Key(outpoint), value.into())
      .map(u64::from);
    if replaced.is_none() {
      self.len += 1;
    }
    replaced
  }

  pub(crate) fn remove(&mut self, outpoint: &OutPoint) -> Option<u64> {
    let value = self
      .shard(outpoint)
      .map
      .remove(&Key(*outpoint))
      .map(u64::from);
    if value.is_some() {
      self.len -= 1;
    }
    value
  }

  pub(crate) fn len(&self) -> usize {
    self.len
  }

  /// Entries it holds before a shard has to grow (without tombstones).
  pub(crate) fn capacity(&self) -> usize {
    self.shards.iter().map(|shard| shard.full_capacity).sum()
  }

  /// Buckets freed by removes that new entries cannot use until the shard
  /// is rebuilt (hashbrown tombstones).
  pub(crate) fn tombstones(&self) -> usize {
    self
      .shards
      .iter()
      .map(|shard| shard.full_capacity.saturating_sub(shard.map.capacity()))
      .sum()
  }

  /// Size of the tables' allocations.
  pub(crate) fn allocated_bytes(&self) -> u64 {
    self
      .shards
      .iter()
      .map(|shard| {
        let buckets = buckets_for_capacity(shard.full_capacity) as u64;
        if buckets == 0 {
          0
        } else {
          buckets * (Self::BUCKET_BYTES as u64 + 1) + 16
        }
      })
      .sum()
  }

  /// Rebuilds, in place, shards whose tombstones have eaten most of their
  /// free buckets (before hashbrown would double them), and notes shards
  /// hashbrown grew. Cheap enough to call after every block.
  pub(crate) fn maintain(&mut self) -> Maintenance {
    let mut maintenance = Maintenance::default();
    for shard in &mut self.shards {
      let capacity = shard.map.capacity();
      if capacity > shard.full_capacity {
        shard.full_capacity = full_capacity_for(capacity);
        maintenance.grown += 1;
      }
      let free = capacity - shard.map.len();
      let tombstones = shard.full_capacity - capacity;
      if free < shard.full_capacity / 16 && tombstones >= (shard.full_capacity / 32).max(1) {
        let entries = shard.map.drain().collect::<Vec<(Key, Value)>>();
        shard.map.extend(entries);
        maintenance.compacted += 1;
      }
    }
    maintenance
  }

  pub(crate) const BUCKET_BYTES: usize = mem::size_of::<(Key, Value)>();
}

/// hashbrown's capacity for a power-of-two bucket count.
fn capacity_for_buckets(buckets: usize) -> usize {
  if buckets < 8 {
    buckets.saturating_sub(1)
  } else {
    buckets / 8 * 7
  }
}

/// The smallest hashbrown table holding `capacity` entries.
fn buckets_for_capacity(capacity: usize) -> usize {
  if capacity == 0 {
    return 0;
  }
  let mut buckets = 2;
  while capacity_for_buckets(buckets) < capacity {
    buckets *= 2;
  }
  buckets
}

/// The full capacity of the table whose `capacity()` (entries plus free
/// buckets, i.e. reduced by tombstones) is `capacity`, assuming fewer
/// tombstones than half the table.
fn full_capacity_for(capacity: usize) -> usize {
  capacity_for_buckets(buckets_for_capacity(capacity))
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
    let map = UtxoMap::with_capacity(256 * 1_000);
    // 1,000 per shard: 2,048 buckets, 1,792 entries.
    assert_eq!(map.capacity(), 256 * 1_792);
    assert_eq!(map.allocated_bytes(), 256 * (2048 * 45 + 16));
    assert_eq!(map.tombstones(), 0);

    // The recommended sizing: 2^28 buckets in all, 11.25 GiB.
    assert_eq!(
      buckets_for_capacity(200_000_000_usize.div_ceil(256)),
      1 << 20
    );
    assert_eq!(capacity_for_buckets(1 << 28), 234_881_024);
  }

  #[test]
  fn table_sizes() {
    for (capacity, buckets) in [
      (0, 0),
      (1, 2),
      (3, 4),
      (4, 8),
      (7, 8),
      (8, 16),
      (14, 16),
      (15, 32),
    ] {
      assert_eq!(buckets_for_capacity(capacity), buckets, "{capacity}");
    }
    assert_eq!(full_capacity_for(1_000), 1_792);
    assert_eq!(full_capacity_for(1_792), 1_792);
    assert_eq!(full_capacity_for(1_793), 3_584);
  }

  /// Outpoints that all land in shard 0.
  fn shard_zero(n: u64) -> OutPoint {
    let mut bytes = [0u8; 32];
    bytes[..8].copy_from_slice(&n.wrapping_mul(0x9e37_79b9_7f4a_7c15).to_le_bytes());
    bytes[9..17].copy_from_slice(&n.to_le_bytes());
    OutPoint {
      txid: Txid::from_byte_array(bytes),
      vout: (n % 3) as u32,
    }
  }

  #[test]
  fn maintenance_clears_tombstones_instead_of_growing() {
    let mut map = UtxoMap::with_capacity(256 * 100);
    let full = map.shards[0].full_capacity;
    assert_eq!(full, 112);

    let mut reference = std::collections::HashMap::new();
    let (mut next, mut oldest) = (0u64, 0u64);
    // Fill shard 0 to ~90%, then churn: remove the oldest, add a new one.
    while next < 100 {
      map.insert(shard_zero(next), next);
      reference.insert(next, ());
      next += 1;
    }
    let mut compacted = 0;
    for _ in 0..100_000 {
      assert_eq!(map.remove(&shard_zero(oldest)), Some(oldest));
      reference.remove(&oldest);
      oldest += 1;
      assert_eq!(map.insert(shard_zero(next), next), None);
      reference.insert(next, ());
      next += 1;

      let maintenance = map.maintain();
      assert_eq!(maintenance.grown, 0);
      compacted += maintenance.compacted;
      assert_eq!(map.shards[0].full_capacity, full);
    }
    assert!(compacted > 0);
    assert_eq!(map.len(), 100);
    assert_eq!(map.len(), reference.len());
    for n in oldest..next {
      assert_eq!(map.remove(&shard_zero(n)), Some(n));
    }
    assert_eq!(map.len(), 0);
  }

  #[test]
  fn shards_grow_individually() {
    let mut map = UtxoMap::with_capacity(256 * 100);
    for n in 0..500 {
      map.insert(shard_zero(n), n);
    }
    let maintenance = map.maintain();
    assert_eq!(maintenance.grown, 1);
    assert_eq!(map.shards[0].full_capacity, 896);
    assert_eq!(map.capacity(), 255 * 112 + 896);
    assert_eq!(map.len(), 500);
    for n in 0..500 {
      assert_eq!(map.remove(&shard_zero(n)), Some(n));
    }
  }
}
