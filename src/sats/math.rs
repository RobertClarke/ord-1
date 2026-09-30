//! Sat/block arithmetic and decimal-digit helpers, written to match the JS
//! (`satRarityRules.js`) bit for bit.

pub const COIN: u64 = 100_000_000;

/// One past the last sat that will ever exist (`ordinals::Sat::SUPPLY`).
pub const SUPPLY: u64 = 2_099_999_997_690_000;

pub const HALVING_INTERVAL: u64 = 210_000;
pub const DIFFCHANGE_INTERVAL: u64 = 2016;
pub const CYCLE_INTERVAL: u64 = 1_260_000;

/// Epochs with a non-zero subsidy (0..=32).
pub const SUBSIDY_EPOCHS: usize = 33;

/// Last height with a subsidy.
pub const LAST_SUBSIDY_HEIGHT: u64 = SUBSIDY_EPOCHS as u64 * HALVING_INTERVAL - 1;

/// Block 5924 is the first block of March 2009 (`FIRST_MARCH_2009_BLOCK`).
pub const FIRST_MARCH_2009_HEIGHT: u64 = 5924;

/// First sat of block 5924: alphas below it are `alpha_jan_feb_2009`.
pub const FIRST_MARCH_2009_SAT: u64 = FIRST_MARCH_2009_HEIGHT * 5_000_000_000;

const fn epoch_starts() -> [u64; SUBSIDY_EPOCHS + 1] {
  let mut starts = [0; SUBSIDY_EPOCHS + 1];
  let mut i = 1;
  while i <= SUBSIDY_EPOCHS {
    starts[i] = starts[i - 1] + HALVING_INTERVAL * (5_000_000_000 >> (i - 1));
    i += 1;
  }
  starts
}

/// First sat of each epoch; `EPOCH_STARTS[33] == SUPPLY`.
pub const EPOCH_STARTS: [u64; SUBSIDY_EPOCHS + 1] = epoch_starts();

pub const fn epoch_subsidy(epoch: usize) -> u64 {
  if epoch < SUBSIDY_EPOCHS {
    5_000_000_000 >> epoch
  } else {
    0
  }
}

/// Where a sat sits: its block, its offset in the block's subsidy and that
/// subsidy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockPos {
  pub height: u64,
  pub offset: u64,
  pub subsidy: u64,
}

/// The JS `getBlockForSat`. Sats past the supply keep the last epoch's
/// one-sat subsidy, as the JS does; they never exist on chain.
pub fn block_of(sat: u64) -> BlockPos {
  let epoch = EPOCH_STARTS[..SUBSIDY_EPOCHS].partition_point(|&start| start <= sat) - 1;
  let subsidy = epoch_subsidy(epoch);
  let into = sat - EPOCH_STARTS[epoch];
  BlockPos {
    height: epoch as u64 * HALVING_INTERVAL + into / subsidy,
    offset: into % subsidy,
    subsidy,
  }
}

pub fn height_of(sat: u64) -> u64 {
  block_of(sat).height
}

fn epoch_of_height(height: u64) -> usize {
  usize::try_from(height / HALVING_INTERVAL)
    .unwrap_or(usize::MAX)
    .min(SUBSIDY_EPOCHS)
}

/// Block subsidy; 0 from height 6,930,000 on.
pub fn subsidy(height: u64) -> u64 {
  epoch_subsidy(epoch_of_height(height))
}

/// First sat of a block (`SUPPLY` for every block without a subsidy).
pub fn first_sat(height: u64) -> u64 {
  let epoch = epoch_of_height(height);
  if epoch >= SUBSIDY_EPOCHS {
    return SUPPLY;
  }
  EPOCH_STARTS[epoch] + (height - epoch as u64 * HALVING_INTERVAL) * epoch_subsidy(epoch)
}

/// Decimal digits, most significant first.
#[derive(Clone, Copy)]
pub struct Digits {
  buf: [u8; 20],
  len: usize,
}

impl Digits {
  pub fn new(mut n: u64) -> Self {
    let mut buf = [0; 20];
    let mut len = 0;
    loop {
      buf[len] = (n % 10) as u8;
      len += 1;
      n /= 10;
      if n == 0 {
        break;
      }
    }
    buf[..len].reverse();
    Self { buf, len }
  }

  pub fn as_slice(&self) -> &[u8] {
    &self.buf[..self.len]
  }
}

pub fn is_palindrome_digits(digits: &[u8]) -> bool {
  let len = digits.len();
  (0..len / 2).all(|i| digits[i] == digits[len - 1 - i])
}

pub fn is_palindrome(n: u64) -> bool {
  is_palindrome_digits(Digits::new(n).as_slice())
}

pub fn distinct_digits(digits: &[u8]) -> u32 {
  digits
    .iter()
    .fold(0u16, |mask, &d| mask | 1 << d)
    .count_ones()
}

/// The JS `isPerfectPalinception`: the digit string is some palindromic
/// substring with at least two distinct digits, repeated at least twice.
pub fn is_perfect_palinception(digits: &[u8]) -> bool {
  let len = digits.len();
  if len < 4 {
    return false;
  }
  for sub_len in 1..=len / 2 {
    if !len.is_multiple_of(sub_len) {
      continue;
    }
    let sub = &digits[..sub_len];
    if digits.chunks(sub_len).all(|chunk| chunk == sub)
      && is_palindrome_digits(sub)
      && distinct_digits(sub) >= 2
    {
      return true;
    }
  }
  false
}

pub const POW10: [u64; 20] = {
  let mut p = [1u64; 20];
  let mut i = 1;
  while i < 20 {
    p[i] = p[i - 1] * 10;
    i += 1;
  }
  p
};

pub fn num_digits(n: u64) -> usize {
  let mut len = 1;
  while len < 20 && n >= POW10[len] {
    len += 1;
  }
  len
}

fn reverse_digits(mut n: u64) -> u64 {
  let mut r = 0;
  while n > 0 {
    r = r * 10 + n % 10;
    n /= 10;
  }
  r
}

/// The palindrome of `len` digits whose first `ceil(len / 2)` digits are
/// `half`.
fn mirror(half: u64, len: usize) -> u64 {
  let half_len = len.div_ceil(2);
  if len.is_multiple_of(2) {
    half * POW10[half_len] + reverse_digits(half)
  } else {
    half * POW10[half_len - 1] + reverse_digits(half / 10)
  }
}

/// Calls `f` for every palindrome in `[a, b)` (clamped to `[1, SUPPLY)`),
/// ascending. Like the JS `generatePalindromesInRange`, never yields 0.
pub fn for_each_palindrome(a: u64, b: u64, mut f: impl FnMut(u64)) {
  let a = a.max(1);
  let b = b.min(SUPPLY);
  if a >= b {
    return;
  }
  let last = b - 1;
  let (min_len, max_len) = (num_digits(a), num_digits(last));
  for len in min_len..=max_len {
    let half_len = len.div_ceil(2);
    let mut lo = POW10[half_len - 1];
    let mut hi = POW10[half_len] - 1;
    if len == min_len {
      lo = lo.max(a / POW10[len - half_len]);
    }
    if len == max_len {
      hi = hi.min(last / POW10[len - half_len]);
    }
    let mut half = lo;
    while half <= hi {
      let p = mirror(half, len);
      if p > last {
        break;
      }
      if p >= a {
        f(p);
      }
      half += 1;
    }
  }
}

/// Number of palindromes in `[a, b)` (clamped like `for_each_palindrome`).
pub fn count_palindromes(a: u64, b: u64) -> u64 {
  let a = a.max(1);
  let b = b.min(SUPPLY);
  if a >= b {
    return 0;
  }
  let last = b - 1;
  let (min_len, max_len) = (num_digits(a), num_digits(last));
  let mut count = 0;
  for len in min_len..=max_len {
    let half_len = len.div_ceil(2);
    let mut lo = POW10[half_len - 1];
    let mut hi = POW10[half_len] - 1;
    if len == min_len {
      lo = lo.max(a / POW10[len - half_len]);
    }
    if len == max_len {
      hi = hi.min(last / POW10[len - half_len]);
    }
    if lo > hi {
      continue;
    }
    count += hi - lo + 1;
    // Only the end halves can mirror outside [a, last].
    if mirror(lo, len) < a {
      count -= 1;
      if lo == hi {
        continue;
      }
    }
    if mirror(hi, len) > last {
      count -= 1;
    }
  }
  count
}

fn from_digits(digits: &[u8]) -> Option<u64> {
  digits
    .iter()
    .try_fold(0u64, |n, &d| n.checked_mul(10)?.checked_add(u64::from(d)))
}

/// Every palindrome below the supply with exactly 1 (`distinct == 1`) or
/// exactly 2 distinct digits, ascending, without 0.
pub fn palindromes_with_distinct_digits(distinct: u32) -> Vec<u64> {
  assert!(distinct == 1 || distinct == 2);
  let mut out = Vec::new();
  let mut digits = [0u8; 20];
  for len in 1..=num_digits(SUPPLY - 1) {
    let half_len = len.div_ceil(2);
    for x in 0..=9u8 {
      for y in x..=9u8 {
        if (distinct == 1) != (x == y) {
          continue;
        }
        let patterns: u32 = if x == y { 1 } else { 1 << half_len };
        for pattern in 0..patterns {
          for (i, digit) in digits.iter_mut().enumerate().take(half_len) {
            *digit = if (pattern >> (half_len - 1 - i)) & 1 == 1 {
              y
            } else {
              x
            };
          }
          if digits[0] == 0 {
            continue;
          }
          for i in half_len..len {
            digits[i] = digits[len - 1 - i];
          }
          let number = &digits[..len];
          if distinct_digits(number) != distinct {
            continue;
          }
          if let Some(n) = from_digits(number)
            && n < SUPPLY
          {
            out.push(n);
          }
        }
      }
    }
  }
  out.sort_unstable();
  out.dedup();
  out
}

/// Every perfect palinception below the supply, ascending.
pub fn perfect_palinceptions() -> Vec<u64> {
  let mut out = Vec::new();
  let max_len = num_digits(SUPPLY - 1);
  for len in 4..=max_len {
    for sub_len in 1..=len / 2 {
      if len % sub_len != 0 {
        continue;
      }
      let half_len = sub_len.div_ceil(2);
      for half in POW10[half_len - 1]..POW10[half_len] {
        let sub = Digits::new(mirror(half, sub_len));
        let sub = sub.as_slice();
        if sub.len() != sub_len || distinct_digits(sub) < 2 {
          continue;
        }
        let repeated = sub.repeat(len / sub_len);
        if let Some(n) = from_digits(&repeated)
          && n < SUPPLY
        {
          out.push(n);
        }
      }
    }
  }
  out.sort_unstable();
  out.dedup();
  out
}

#[cfg(test)]
mod tests {
  use {super::*, ordinals::Height};

  #[test]
  fn epoch_starts_match_ordinals() {
    for (i, start) in EPOCH_STARTS.iter().enumerate() {
      assert_eq!(*start, ordinals::Epoch::STARTING_SATS[i].n(), "epoch {i}");
    }
    assert_eq!(EPOCH_STARTS[SUBSIDY_EPOCHS], SUPPLY);
    assert_eq!(SUPPLY, ordinals::Sat::SUPPLY);
    assert_eq!(LAST_SUBSIDY_HEIGHT, 6_929_999);
  }

  #[test]
  fn block_math_matches_ordinals() {
    let heights = [
      0u32, 1, 9, 209_999, 210_000, 419_999, 420_000, 840_000, 6_719_999, 6_720_000, 6_929_999,
    ];
    for h in heights {
      let first = Height(h).starting_sat().n();
      let subsidy = Height(h).subsidy();
      assert_eq!(first_sat(h.into()), first);
      assert_eq!(super::subsidy(h.into()), subsidy);
      let pos = block_of(first);
      assert_eq!(
        (pos.height, pos.offset, pos.subsidy),
        (h.into(), 0, subsidy)
      );
      let pos = block_of(first + subsidy - 1);
      assert_eq!(
        (pos.height, pos.offset, pos.subsidy),
        (h.into(), subsidy - 1, subsidy)
      );
    }
    assert_eq!(first_sat(6_930_000), SUPPLY);
    assert_eq!(super::subsidy(6_930_000), 0);
    assert_eq!(super::subsidy(u64::MAX), 0);
    assert_eq!(first_sat(u64::MAX), SUPPLY);
    // Past the supply the JS keeps one-sat blocks.
    assert_eq!(block_of(SUPPLY).height, 6_930_000);
    assert_eq!(block_of(SUPPLY + 5).height, 6_930_005);
  }

  #[test]
  fn palindromes() {
    assert!(is_palindrome(0));
    assert!(is_palindrome(7));
    assert!(is_palindrome(1_234_321));
    assert!(!is_palindrome(10));
    assert!(is_palindrome(12_345_678_987_654_321));
    assert_eq!(distinct_digits(Digits::new(1_234_321).as_slice()), 4);

    for (a, b) in [
      (0u64, 2000u64),
      (1, 10),
      (9, 12),
      (95, 1_000_101),
      (123_456, 1_234_567),
      (999_999_990, 1_000_000_010),
      (44_999_999_000, 45_000_100_000),
    ] {
      let mut generated = Vec::new();
      for_each_palindrome(a, b, |p| generated.push(p));
      let brute: Vec<u64> = (a.max(1)..b).filter(|&n| is_palindrome(n)).collect();
      assert_eq!(generated, brute, "[{a}, {b})");
      assert_eq!(count_palindromes(a, b), brute.len() as u64, "[{a}, {b})");
    }
    assert_eq!(count_palindromes(45_000_000_000, 50_000_000_000), 50_000);
    let mut n = 0;
    for_each_palindrome(45_000_000_000, 50_000_000_000, |_| n += 1);
    assert_eq!(n, 50_000);
    // Clamped at the supply.
    let mut last = 0;
    for_each_palindrome(SUPPLY - 10_000_000_000, u64::MAX, |p| last = p);
    assert!(last < SUPPLY && is_palindrome(last));
  }

  #[test]
  fn palinception() {
    let p = |n: u64| is_perfect_palinception(Digits::new(n).as_slice());
    assert!(p(121_121));
    assert!(p(12_211_221));
    assert!(p(123_321_123_321));
    assert!(!p(111_111));
    assert!(!p(1_111));
    assert!(!p(12_121)); // 5 digits: no divisor repeats
    assert!(!p(1_221));
    assert!(p(121_121_121));
  }

  #[test]
  fn digit_class_lists() {
    let one = palindromes_with_distinct_digits(1);
    assert_eq!(one.len(), 9 * 15 + 1); // 16-digit repdigits below 2.1e15: only 1…1
    assert!(one.iter().all(|&n| n > 0 && is_palindrome(n)));
    let two = palindromes_with_distinct_digits(2);
    let brute: Vec<u64> = (1..2_000_000u64)
      .filter(|&n| is_palindrome(n) && distinct_digits(Digits::new(n).as_slice()) == 2)
      .collect();
    assert_eq!(
      two
        .iter()
        .copied()
        .filter(|&n| n < 2_000_000)
        .collect::<Vec<_>>(),
      brute
    );
    let palinceptions = perfect_palinceptions();
    let brute: Vec<u64> = (1..20_000_000u64)
      .filter(|&n| is_palindrome(n) && is_perfect_palinception(Digits::new(n).as_slice()))
      .collect();
    assert_eq!(
      palinceptions
        .iter()
        .copied()
        .filter(|&n| n < 20_000_000)
        .collect::<Vec<_>>(),
      brute
    );
    assert!(palinceptions.windows(2).all(|w| w[0] < w[1]));
    assert!(*palinceptions.last().unwrap() < SUPPLY);
  }
}
