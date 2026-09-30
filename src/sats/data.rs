//! The embedded data exported from bitcoin-monitoring by
//! `tools/export-monitoring-data.js` (formats documented there).

use {
  super::{SatType, TypeSet},
  anyhow::{Context, bail, ensure},
};

pub(crate) static EXOTIC_RANGES_BIN: &[u8] = include_bytes!("../../data/exotic-ranges.bin");
pub(crate) static BOUNTY3_NAMES_BIN: &[u8] = include_bytes!("../../data/bounty3-names.bin");

/// One exotic type's sorted, merged, half-open ranges.
pub(crate) type TypeRanges = (SatType, Vec<(u64, u64)>);

struct Reader<'a> {
  bytes: &'a [u8],
  pos: usize,
}

impl<'a> Reader<'a> {
  fn take(&mut self, n: usize) -> anyhow::Result<&'a [u8]> {
    let end = self.pos.checked_add(n).context("length overflow")?;
    ensure!(end <= self.bytes.len(), "truncated at byte {}", self.pos);
    let slice = &self.bytes[self.pos..end];
    self.pos = end;
    Ok(slice)
  }

  fn u8(&mut self) -> anyhow::Result<u8> {
    Ok(self.take(1)?[0])
  }

  fn u32(&mut self) -> anyhow::Result<u32> {
    Ok(u32::from_le_bytes(self.take(4)?.try_into()?))
  }

  fn u64(&mut self) -> anyhow::Result<u64> {
    Ok(u64::from_le_bytes(self.take(8)?.try_into()?))
  }

  fn done(&self) -> anyhow::Result<()> {
    ensure!(
      self.pos == self.bytes.len(),
      "{} trailing bytes",
      self.bytes.len() - self.pos
    );
    Ok(())
  }
}

/// Per exotic type, its sorted, merged, half-open ranges. `requests` (never
/// tracked) is skipped; an unknown type name is an error so a new JS type
/// cannot be silently ignored.
pub(crate) fn parse_exotic_ranges(bytes: &[u8]) -> anyhow::Result<Vec<TypeRanges>> {
  let mut r = Reader { bytes, pos: 0 };
  ensure!(r.take(8)? == b"ORDEXRNG", "bad exotic-ranges magic");
  ensure!(r.u32()? == 1, "unsupported exotic-ranges version");
  let types = r.u32()?;
  let mut out: Vec<TypeRanges> = Vec::new();
  for _ in 0..types {
    let len = r.u8()?.into();
    let name = std::str::from_utf8(r.take(len)?)?.to_owned();
    let count = r.u32()?;
    let mut ranges = Vec::with_capacity(count as usize);
    for _ in 0..count {
      let start = r.u64()?;
      let end = r.u64()?;
      ensure!(start < end, "{name}: empty range [{start}, {end})");
      if let Some(&(_, last_end)) = ranges.last() {
        ensure!(
          last_end < start,
          "{name}: ranges not sorted and merged at {start}"
        );
      }
      ranges.push((start, end));
    }
    if name == "requests" {
      continue;
    }
    let Some(t) = SatType::from_name(&name) else {
      bail!("unknown exotic type `{name}`: update src/sats for the new JS type");
    };
    ensure!(t.is_exotic(), "`{name}` is not an exotic range type");
    ensure!(
      out.iter().all(|(seen, _)| *seen != t),
      "duplicate exotic type `{name}`"
    );
    out.push((t, ranges));
  }
  r.done()?;
  for t in SatType::EXOTIC {
    ensure!(
      out.iter().any(|(seen, _)| *seen == t),
      "exotic type `{t}` missing from the data"
    );
  }
  Ok(out)
}

pub(crate) struct Bounty3 {
  pub sats: Vec<u64>,
  names: Vec<Box<str>>,
}

impl Bounty3 {
  pub fn parse(bytes: &[u8]) -> anyhow::Result<Self> {
    let mut r = Reader { bytes, pos: 0 };
    ensure!(r.take(8)? == b"ORDB3NAM", "bad bounty3-names magic");
    ensure!(r.u32()? == 1, "unsupported bounty3-names version");
    let count = r.u32()? as usize;
    let mut sats = Vec::with_capacity(count);
    for _ in 0..count {
      let sat = r.u64()?;
      if let Some(&last) = sats.last() {
        ensure!(last < sat, "bounty3 sats not sorted at {sat}");
      }
      sats.push(sat);
    }
    let mut names = Vec::with_capacity(count);
    for _ in 0..count {
      let len = r.u8()?.into();
      names.push(std::str::from_utf8(r.take(len)?)?.into());
    }
    r.done()?;
    Ok(Self { sats, names })
  }

  pub fn contains(&self, sat: u64) -> bool {
    self.sats.binary_search(&sat).is_ok()
  }

  pub fn name(&self, sat: u64) -> Option<&str> {
    self.sats.binary_search(&sat).ok().map(|i| &*self.names[i])
  }
}

/// Every exotic type's ranges folded into one sorted list of boundary points,
/// with the set of types covering each segment between consecutive points,
/// plus a coarse bucket index so a lookup touches a handful of points.
pub(crate) struct ExoticIndex {
  /// Per `SatType` (indexed by discriminant), the type's ranges; empty for
  /// non-exotic types.
  ranges: Vec<Vec<(u64, u64)>>,
  points: Vec<u64>,
  /// `masks[i]` covers `[points[i], points[i + 1])`; the last is empty.
  masks: Vec<u64>,
  /// `bucket_first[b]`: index of the first point `>= b << BUCKET_SHIFT`.
  bucket_first: Vec<u32>,
}

const BUCKET_SHIFT: u32 = 34;

impl ExoticIndex {
  pub fn new(types: Vec<TypeRanges>) -> Self {
    let mut ranges = vec![Vec::new(); SatType::ALL.len()];
    let mut events = Vec::new();
    for (t, type_ranges) in types {
      for &(start, end) in &type_ranges {
        events.push((start, t.bit()));
        events.push((end, t.bit()));
      }
      ranges[t as usize] = type_ranges;
    }
    // Each type's ranges are disjoint and non-adjacent, so a type toggles at
    // most once per point and XOR gives the covering set.
    events.sort_unstable();
    let mut points = Vec::new();
    let mut masks = Vec::new();
    let mut mask = 0u64;
    let mut i = 0;
    while i < events.len() {
      let point = events[i].0;
      while i < events.len() && events[i].0 == point {
        mask ^= events[i].1;
        i += 1;
      }
      points.push(point);
      masks.push(mask);
    }
    let buckets = points
      .last()
      .map_or(0, |&p| (p >> BUCKET_SHIFT) as usize + 1);
    let mut bucket_first = Vec::with_capacity(buckets + 1);
    let mut next = 0usize;
    for b in 0..=buckets {
      let start = (b as u64) << BUCKET_SHIFT;
      while next < points.len() && points[next] < start {
        next += 1;
      }
      bucket_first.push(next as u32);
    }
    Self {
      ranges,
      points,
      masks,
      bucket_first,
    }
  }

  pub fn ranges(&self, t: SatType) -> &[(u64, u64)] {
    &self.ranges[t as usize]
  }

  /// Number of points `<= sat`.
  fn rank(&self, sat: u64) -> usize {
    let bucket = (sat >> BUCKET_SHIFT) as usize;
    let last = self.bucket_first.len().saturating_sub(1);
    if bucket >= last {
      return self.points.len();
    }
    let (lo, hi) = (
      self.bucket_first[bucket] as usize,
      self.bucket_first[bucket + 1] as usize,
    );
    lo + self.points[lo..hi].partition_point(|&p| p <= sat)
  }

  /// The exotic types whose ranges contain `sat`.
  pub fn types_at(&self, sat: u64) -> TypeSet {
    match self.rank(sat) {
      0 => TypeSet::EMPTY,
      i => TypeSet(self.masks[i - 1]),
    }
  }

  /// Calls `f(start, end, types)` for every maximal segment of `[a, b)` with
  /// a constant, non-empty set of exotic types, ascending.
  pub fn for_each_segment(&self, a: u64, b: u64, mut f: impl FnMut(u64, u64, TypeSet)) {
    if a >= b {
      return;
    }
    let mut i = self.rank(a);
    let mut start = a;
    let mut mask = if i == 0 { 0 } else { self.masks[i - 1] };
    loop {
      let end = self.points.get(i).copied().unwrap_or(u64::MAX).min(b);
      if mask != 0 && start < end {
        f(start, end, TypeSet(mask));
      }
      if end >= b {
        break;
      }
      start = end;
      mask = self.masks[i];
      i += 1;
    }
  }
}

/// Number of sats of `[a, b)` inside the sorted, disjoint `ranges`.
pub(crate) fn overlap_len(ranges: &[(u64, u64)], a: u64, b: u64) -> u64 {
  let mut total = 0;
  for_each_overlap(ranges, a, b, |s, e| total += e - s);
  total
}

/// Calls `f(start, end)` for each non-empty intersection of `[a, b)` with the
/// sorted, disjoint `ranges`, ascending.
pub(crate) fn for_each_overlap(ranges: &[(u64, u64)], a: u64, b: u64, mut f: impl FnMut(u64, u64)) {
  if a >= b {
    return;
  }
  let first = ranges.partition_point(|&(_, end)| end <= a);
  for &(start, end) in &ranges[first..] {
    if start >= b {
      break;
    }
    f(start.max(a), end.min(b));
  }
}

/// Sorts and merges overlapping or adjacent half-open ranges.
pub(crate) fn merge_ranges(mut ranges: Vec<(u64, u64)>) -> Vec<(u64, u64)> {
  ranges.retain(|&(start, end)| start < end);
  ranges.sort_unstable();
  let mut merged: Vec<(u64, u64)> = Vec::with_capacity(ranges.len());
  for (start, end) in ranges {
    match merged.last_mut() {
      Some(last) if start <= last.1 => last.1 = last.1.max(end),
      _ => merged.push((start, end)),
    }
  }
  merged
}

/// Intersection of two sorted, disjoint range lists.
pub(crate) fn intersect_ranges(a: &[(u64, u64)], b: &[(u64, u64)]) -> Vec<(u64, u64)> {
  let mut out = Vec::new();
  let (mut i, mut j) = (0, 0);
  while i < a.len() && j < b.len() {
    let start = a[i].0.max(b[j].0);
    let end = a[i].1.min(b[j].1);
    if start < end {
      out.push((start, end));
    }
    if a[i].1 < b[j].1 {
      i += 1;
    } else {
      j += 1;
    }
  }
  out
}

#[cfg(test)]
mod tests {
  use {super::*, bitcoin::hashes::Hash};

  fn manifest(name: &str) -> serde_json::Value {
    let path = format!("{}/data/{name}", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
  }

  #[test]
  fn exotic_ranges_match_manifest() {
    let manifest = manifest("exotic-ranges.json");
    assert_eq!(
      manifest["bin"]["sha256"].as_str().unwrap(),
      bitcoin::hashes::sha256::Hash::hash(EXOTIC_RANGES_BIN).to_string()
    );
    assert_eq!(
      manifest["bin"]["bytes"].as_u64().unwrap(),
      EXOTIC_RANGES_BIN.len() as u64
    );
    let parsed = parse_exotic_ranges(EXOTIC_RANGES_BIN).unwrap();
    let mut total = 0;
    for entry in manifest["types"].as_array().unwrap() {
      let name = entry["name"].as_str().unwrap();
      let sats = entry["sats"].as_u64().unwrap();
      total += sats;
      if name == "requests" {
        continue;
      }
      let t = SatType::from_name(name).unwrap();
      let ranges = &parsed.iter().find(|(seen, _)| *seen == t).unwrap().1;
      assert_eq!(
        ranges.len() as u64,
        entry["ranges"].as_u64().unwrap(),
        "{name}"
      );
      assert_eq!(
        ranges.iter().map(|(s, e)| e - s).sum::<u64>(),
        sats,
        "{name}"
      );
    }
    assert_eq!(total, manifest["total_sats"].as_u64().unwrap());
    let get = |t: SatType| {
      parsed
        .iter()
        .find(|(seen, _)| *seen == t)
        .unwrap()
        .1
        .clone()
    };
    assert_eq!(get(SatType::Vintage), [(0, 5_000_000_000_000)]);
    assert_eq!(get(SatType::Block9), [(45_000_000_000, 50_000_000_000)]);
    assert_eq!(get(SatType::Block450x), [(45_000_000_000, 45_100_000_000)]);
    assert_eq!(get(SatType::Block78), [(390_000_000_000, 395_000_000_000)]);
    assert_eq!(
      get(SatType::Block286),
      [(1_430_000_000_000, 1_435_000_000_000)]
    );
    assert_eq!(
      get(SatType::Block666),
      [(3_330_000_000_000, 3_335_000_000_000)]
    );
    assert_eq!(get(SatType::Nakamoto).len(), 19);
  }

  #[test]
  fn bounty3_matches_manifest() {
    let manifest = manifest("bounty3-names.json");
    assert_eq!(
      manifest["bin"]["sha256"].as_str().unwrap(),
      bitcoin::hashes::sha256::Hash::hash(BOUNTY3_NAMES_BIN).to_string()
    );
    let bounty3 = Bounty3::parse(BOUNTY3_NAMES_BIN).unwrap();
    assert_eq!(
      bounty3.sats.len() as u64,
      manifest["count"].as_u64().unwrap()
    );
    assert_eq!(bounty3.sats[0], manifest["first"].as_u64().unwrap());
    assert_eq!(
      *bounty3.sats.last().unwrap(),
      manifest["last"].as_u64().unwrap()
    );
    assert!(bounty3.contains(1_860_605_502_586_555));
    assert_eq!(bounty3.name(1_860_605_502_586_555), Some("arbitraires"));
    assert_eq!(bounty3.name(1_860_605_502_586_556), None);
  }

  #[test]
  fn corrupt_data_is_rejected() {
    assert!(parse_exotic_ranges(&EXOTIC_RANGES_BIN[..100]).is_err());
    assert!(parse_exotic_ranges(b"NOTMAGIC").is_err());
    let mut unknown = b"ORDEXRNG".to_vec();
    unknown.extend(1u32.to_le_bytes());
    unknown.extend(1u32.to_le_bytes());
    unknown.push(3);
    unknown.extend(b"foo");
    unknown.extend(0u32.to_le_bytes());
    assert!(
      parse_exotic_ranges(&unknown)
        .unwrap_err()
        .to_string()
        .contains("unknown exotic type `foo`")
    );
    assert!(Bounty3::parse(&BOUNTY3_NAMES_BIN[..BOUNTY3_NAMES_BIN.len() - 1]).is_err());
  }

  #[test]
  fn index_lookups() {
    let index = ExoticIndex::new(vec![
      (SatType::Vintage, vec![(0, 100)]),
      (SatType::Block9, vec![(10, 20), (30, 40)]),
      (SatType::Pizza, vec![(20, 30), (1 << 40, (1 << 40) + 5)]),
    ]);
    let vintage = TypeSet::of(&[SatType::Vintage]);
    assert_eq!(index.types_at(0), vintage);
    assert_eq!(index.types_at(10), vintage.with(SatType::Block9));
    assert_eq!(index.types_at(19), vintage.with(SatType::Block9));
    assert_eq!(index.types_at(20), vintage.with(SatType::Pizza));
    assert_eq!(index.types_at(99), vintage);
    assert_eq!(index.types_at(100), TypeSet::EMPTY);
    assert_eq!(index.types_at(1 << 40), TypeSet::of(&[SatType::Pizza]));
    assert_eq!(index.types_at((1 << 40) + 5), TypeSet::EMPTY);
    assert_eq!(index.types_at(u64::MAX), TypeSet::EMPTY);
    let mut segments = Vec::new();
    index.for_each_segment(5, 1 << 41, |s, e, t| segments.push((s, e, t)));
    assert_eq!(
      segments,
      [
        (5, 10, vintage),
        (10, 20, vintage.with(SatType::Block9)),
        (20, 30, vintage.with(SatType::Pizza)),
        (30, 40, vintage.with(SatType::Block9)),
        (40, 100, vintage),
        (1 << 40, (1 << 40) + 5, TypeSet::of(&[SatType::Pizza])),
      ]
    );
    segments.clear();
    index.for_each_segment(12, 13, |s, e, t| segments.push((s, e, t)));
    assert_eq!(segments, [(12, 13, vintage.with(SatType::Block9))]);
  }

  #[test]
  fn range_helpers() {
    let ranges = merge_ranges(vec![(5, 10), (0, 3), (3, 4), (9, 12), (20, 20)]);
    assert_eq!(ranges, [(0, 4), (5, 12)]);
    assert_eq!(overlap_len(&ranges, 2, 6), 3);
    assert_eq!(overlap_len(&ranges, 12, 100), 0);
    assert_eq!(
      intersect_ranges(&ranges, &[(3, 6), (11, 30)]),
      [(3, 4), (5, 6), (11, 12)]
    );
  }
}
