//! Anchors: the sparse sat families every individually tracked sat and every
//! per-sat row sat belongs to (block boundaries, alphas/omegas, palindromes,
//! listed sats), with ascending enumeration and counting over sat ranges.

use super::{
  SatType, TypeSet,
  math::{
    COIN, LAST_SUBSIDY_HEIGHT, SUPPLY, block_of, count_palindromes, first_sat, for_each_palindrome,
    subsidy,
  },
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ListId {
  OneDigit,
  TwoDigit,
  Palinception,
  Bounty3,
}

/// Precomputed sorted sat lists.
pub(crate) struct Lists {
  pub one_digit: Vec<u64>,
  pub two_digit: Vec<u64>,
  pub palinception: Vec<u64>,
  pub bounty3: Vec<u64>,
}

impl Lists {
  pub fn get(&self, id: ListId) -> &[u64] {
    match id {
      ListId::OneDigit => &self.one_digit,
      ListId::TwoDigit => &self.two_digit,
      ListId::Palinception => &self.palinception,
      ListId::Bounty3 => &self.bounty3,
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Anchor {
  /// First sats of blocks with `height % modulus == residue`.
  BlockFirst {
    modulus: u64,
    residue: u64,
  },
  /// Last sats of blocks with `height % modulus == residue`.
  BlockLast {
    modulus: u64,
    residue: u64,
  },
  /// Both boundary sats of every palindromic height.
  PalindromicBlocks,
  /// Sats with `sat % modulus == residue`.
  Multiple {
    modulus: u64,
    residue: u64,
  },
  /// Every palindrome except 0.
  Palindromes,
  List(ListId),
}

const MEGASAT: u64 = COIN * 1_000_000;
const CENTASAT: u64 = COIN * 100_000;
const DECASAT: u64 = COIN * 10_000;
const KILOSAT: u64 = COIN * 1_000;

/// Heights `h` with `first_sat(h)` in `[a, b)`, as `lo..hi`.
fn heights_first_in(a: u64, b: u64) -> (u64, u64) {
  let ceil = |sat: u64| {
    if sat >= SUPPLY {
      return LAST_SUBSIDY_HEIGHT + 1;
    }
    let pos = block_of(sat);
    if pos.offset == 0 {
      pos.height
    } else {
      pos.height + 1
    }
  };
  if a >= b { (0, 0) } else { (ceil(a), ceil(b)) }
}

/// Heights `h` with the last sat of `h` in `[a, b)`, as `lo..hi`.
fn heights_last_in(a: u64, b: u64) -> (u64, u64) {
  let floor = |sat: u64| {
    if sat >= SUPPLY {
      LAST_SUBSIDY_HEIGHT + 1
    } else {
      block_of(sat).height
    }
  };
  if a >= b { (0, 0) } else { (floor(a), floor(b)) }
}

fn last_sat(height: u64) -> u64 {
  first_sat(height) + subsidy(height) - 1
}

/// First `x >= start` with `x % modulus == residue`, if it fits in `u64`.
fn first_congruent(start: u64, modulus: u64, residue: u64) -> Option<u64> {
  let r = start % modulus;
  if r <= residue {
    start.checked_add(residue - r)
  } else {
    start.checked_add(modulus - r)?.checked_add(residue)
  }
}

/// Number of `x` in `[lo, hi)` with `x % modulus == residue`.
fn count_congruent(lo: u64, hi: u64, modulus: u64, residue: u64) -> u64 {
  match first_congruent(lo, modulus, residue) {
    Some(first) if first < hi => (hi - 1 - first) / modulus + 1,
    _ => 0,
  }
}

/// The rarity tiers of a block's first sat, highest first, each with the
/// height modulus of its blocks (`u64::MAX`: block 0 only). Since rules
/// version 4 a sat keeps only the highest of them (`TypeSet::exclusive`).
pub(crate) const FIRST_TIERS: [(SatType, u64); 5] = [
  (SatType::Mythic, u64::MAX),
  (SatType::Legendary, 1_260_000),
  (SatType::Epic, 210_000),
  (SatType::Rare, 2016),
  (SatType::Uncommon, 1),
];

/// The tiers of a block's last sat, highest first: height `h` with
/// `(h + 1) % modulus == 0`.
pub(crate) const LAST_TIERS: [(SatType, u64); 3] = [
  (SatType::BlackEpic, 210_000),
  (SatType::BlackRare, 2016),
  (SatType::Black, 1),
];

fn gcd(a: u64, b: u64) -> u64 {
  if b == 0 { a } else { gcd(b, a % b) }
}

/// Least common multiple of two tier moduli; `u64::MAX` (block 0 only)
/// absorbs any other.
fn lcm(a: u64, b: u64) -> u64 {
  if a == u64::MAX || b == u64::MAX {
    u64::MAX
  } else {
    a / gcd(a, b) * b
  }
}

impl Anchor {
  /// Candidates in `[a, b)` that have a tier of `tier`'s family above
  /// `tier`, so that, tiers being exclusive, they do not have `tier`
  /// (inclusion-exclusion over the higher tiers' block moduli). `None`
  /// unless the anchor is that family's block boundary.
  pub fn superseded(self, tier: SatType, lists: &Lists, a: u64, b: u64) -> Option<u64> {
    let (family, first) = if let Some(i) = FIRST_TIERS.iter().position(|&(t, _)| t == tier) {
      (&FIRST_TIERS[..i], true)
    } else {
      let i = LAST_TIERS.iter().position(|&(t, _)| t == tier)?;
      (&LAST_TIERS[..i], false)
    };
    let modulus = match self {
      Anchor::BlockFirst {
        modulus,
        residue: 0,
      } if first => modulus,
      Anchor::BlockLast { modulus, residue } if !first && residue == modulus - 1 => modulus,
      _ => return None,
    };
    let mut total: i128 = 0;
    for subset in 1u32..(1 << family.len()) {
      let mut m = modulus;
      for (i, &(_, tier_modulus)) in family.iter().enumerate() {
        if subset >> i & 1 == 1 {
          m = lcm(m, tier_modulus);
        }
      }
      let anchor = if first {
        Anchor::BlockFirst {
          modulus: m,
          residue: 0,
        }
      } else {
        Anchor::BlockLast {
          modulus: m,
          residue: m - 1,
        }
      };
      let n = i128::from(anchor.count(lists, a, b));
      total += if subset.count_ones() % 2 == 1 { n } else { -n };
    }
    Some(u64::try_from(total).unwrap())
  }

  /// The anchor that enumerates every sat having `t`, if `t` is sparse.
  pub fn of(t: SatType) -> Option<Anchor> {
    use SatType::*;
    let first = |modulus| Anchor::BlockFirst {
      modulus,
      residue: 0,
    };
    let last = |modulus: u64| Anchor::BlockLast {
      modulus,
      residue: modulus - 1,
    };
    let alpha = |modulus| Anchor::Multiple {
      modulus,
      residue: 0,
    };
    let omega = |modulus: u64| Anchor::Multiple {
      modulus,
      residue: modulus - 1,
    };
    Some(match t {
      Mythic => first(u64::MAX),
      Legendary => first(1_260_000),
      Epic => first(210_000),
      Rare => first(2016),
      Uncommon => first(1),
      Paliblock => Anchor::PalindromicBlocks,
      Black => Anchor::BlockLast {
        modulus: 1,
        residue: 0,
      },
      BlackRare => last(2016),
      BlackEpic => last(210_000),
      Alpha => alpha(COIN),
      MegasatAlpha => alpha(MEGASAT),
      CentasatAlpha => alpha(CENTASAT),
      DecasatAlpha => alpha(DECASAT),
      KilosatAlpha => alpha(KILOSAT),
      Omega => omega(COIN),
      MegasatOmega => omega(MEGASAT),
      CentasatOmega => omega(CENTASAT),
      DecasatOmega => omega(DECASAT),
      KilosatOmega => omega(KILOSAT),
      PalindromesInteger | PalindromesInteger3d => Anchor::Palindromes,
      PalindromesInteger1d => Anchor::List(ListId::OneDigit),
      PalindromesInteger2d => Anchor::List(ListId::TwoDigit),
      PalindromesPerfectPalinception => Anchor::List(ListId::Palinception),
      Bounty3Name => Anchor::List(ListId::Bounty3),
      Vintage | Block9 | Block450x | Block78 | Block286 | Block666 | Nakamoto | Jpeg | Pizza
      | Hitman | Sr1stAuction => return None,
    })
  }

  /// Types every candidate has (independent of the domain).
  pub fn implied(self) -> TypeSet {
    use SatType::*;
    match self {
      Anchor::BlockFirst { modulus, residue } => {
        let mut set = TypeSet::of(&[Uncommon]);
        if residue == 0 {
          for (m, t) in [(2016, Rare), (210_000, Epic), (1_260_000, Legendary)] {
            if modulus % m == 0 {
              set.insert(t);
            }
          }
          if modulus == u64::MAX {
            set = set.union(TypeSet::of(&[Mythic, Legendary, Epic, Rare]));
          }
        }
        set
      }
      Anchor::BlockLast { modulus, residue } => {
        let mut set = TypeSet::of(&[Black]);
        for (m, t) in [(2016, BlackRare), (210_000, BlackEpic)] {
          if modulus % m == 0 && residue % m == m - 1 {
            set.insert(t);
          }
        }
        set
      }
      Anchor::PalindromicBlocks => TypeSet::EMPTY,
      Anchor::Multiple { modulus, residue } => {
        let (base, mega) = if modulus % COIN != 0 {
          return TypeSet::EMPTY;
        } else if residue == 0 {
          (Alpha, MegasatAlpha)
        } else if residue == modulus - 1 {
          (Omega, MegasatOmega)
        } else {
          return TypeSet::EMPTY;
        };
        let mut set = TypeSet::of(&[base]);
        if modulus % MEGASAT == 0 {
          set.insert(mega);
        }
        set
      }
      Anchor::Palindromes => TypeSet::of(&[PalindromesInteger]),
      Anchor::List(ListId::OneDigit) => TypeSet::of(&[PalindromesInteger, PalindromesInteger1d]),
      Anchor::List(ListId::TwoDigit) => TypeSet::of(&[PalindromesInteger, PalindromesInteger2d]),
      Anchor::List(ListId::Palinception) => {
        TypeSet::of(&[PalindromesInteger, PalindromesPerfectPalinception])
      }
      Anchor::List(ListId::Bounty3) => TypeSet::of(&[Bounty3Name]),
    }
  }

  /// Rough density rank: lower enumerates fewer candidates.
  pub fn rank(self) -> u32 {
    match self {
      Anchor::BlockFirst { modulus, .. } | Anchor::BlockLast { modulus, .. } => match modulus {
        u64::MAX => 0,
        1 => 50,
        m if m >= 210_000 => 10,
        _ => 30,
      },
      Anchor::List(ListId::OneDigit) => 0,
      Anchor::List(ListId::Palinception | ListId::Bounty3) => 20,
      Anchor::List(ListId::TwoDigit) => 25,
      Anchor::Multiple { modulus, .. } => match modulus {
        m if m >= MEGASAT => 5,
        m if m >= CENTASAT => 10,
        m if m >= DECASAT => 20,
        m if m >= KILOSAT => 30,
        _ => 60,
      },
      Anchor::PalindromicBlocks => 40,
      Anchor::Palindromes => 70,
    }
  }

  /// Calls `f` for every candidate in `[a, b)` (clamped to the supply),
  /// ascending.
  pub fn for_each(self, lists: &Lists, a: u64, b: u64, mut f: impl FnMut(u64)) {
    let b = b.min(SUPPLY);
    if a >= b {
      return;
    }
    match self {
      Anchor::BlockFirst { modulus, residue } => {
        let (lo, hi) = heights_first_in(a, b);
        let mut h = first_congruent(lo, modulus, residue);
        while let Some(height) = h.filter(|&h| h < hi) {
          f(first_sat(height));
          h = height.checked_add(modulus);
        }
      }
      Anchor::BlockLast { modulus, residue } => {
        let (lo, hi) = heights_last_in(a, b);
        let mut h = first_congruent(lo, modulus, residue);
        while let Some(height) = h.filter(|&h| h < hi) {
          f(last_sat(height));
          h = height.checked_add(modulus);
        }
      }
      Anchor::PalindromicBlocks => {
        let lo = heights_last_in(a, b).0;
        let hi = heights_first_in(a, b).1;
        for_each_palindrome(lo, hi, |height| {
          let (first, last) = (first_sat(height), last_sat(height));
          if first >= a && first < b {
            f(first);
          }
          if last != first && last >= a && last < b {
            f(last);
          }
        });
      }
      Anchor::Multiple { modulus, residue } => {
        let mut x = first_congruent(a, modulus, residue);
        while let Some(sat) = x.filter(|&x| x < b) {
          f(sat);
          x = sat.checked_add(modulus);
        }
      }
      Anchor::Palindromes => for_each_palindrome(a, b, f),
      Anchor::List(id) => {
        let list = lists.get(id);
        let from = list.partition_point(|&sat| sat < a);
        for &sat in &list[from..] {
          if sat >= b {
            break;
          }
          f(sat);
        }
      }
    }
  }

  /// Number of candidates in `[a, b)` (clamped to the supply).
  pub fn count(self, lists: &Lists, a: u64, b: u64) -> u64 {
    let b = b.min(SUPPLY);
    if a >= b {
      return 0;
    }
    match self {
      Anchor::BlockFirst { modulus, residue } => {
        let (lo, hi) = heights_first_in(a, b);
        count_congruent(lo, hi, modulus, residue)
      }
      Anchor::BlockLast { modulus, residue } => {
        let (lo, hi) = heights_last_in(a, b);
        count_congruent(lo, hi, modulus, residue)
      }
      Anchor::Multiple { modulus, residue } => count_congruent(a, b, modulus, residue),
      Anchor::Palindromes => count_palindromes(a, b),
      Anchor::List(id) => {
        let list = lists.get(id);
        (list.partition_point(|&sat| sat < b) - list.partition_point(|&sat| sat < a)) as u64
      }
      Anchor::PalindromicBlocks => {
        let mut n = 0;
        self.for_each(lists, a, b, |_| n += 1);
        n
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use {super::*, crate::sats::math::is_palindrome};

  fn lists() -> Lists {
    Lists {
      one_digit: vec![1, 2, 11, 999],
      two_digit: vec![],
      palinception: vec![],
      bounty3: vec![5, 5_000_000_000, 10_000_000_000],
    }
  }

  fn brute(anchor: Anchor, a: u64, b: u64) -> Vec<u64> {
    let lists = lists();
    let mut out = Vec::new();
    // Brute force over the candidate definition, sat by sat, for small
    // windows around interesting points.
    for sat in a..b.min(SUPPLY) {
      let pos = block_of(sat);
      let is = match anchor {
        Anchor::BlockFirst { modulus, residue } => {
          pos.offset == 0 && pos.height % modulus == residue
        }
        Anchor::BlockLast { modulus, residue } => {
          pos.offset == pos.subsidy - 1 && pos.height % modulus == residue
        }
        Anchor::PalindromicBlocks => {
          (pos.offset == 0 || pos.offset == pos.subsidy - 1)
            && pos.height != 0
            && is_palindrome(pos.height)
        }
        Anchor::Multiple { modulus, residue } => sat % modulus == residue,
        Anchor::Palindromes => sat != 0 && is_palindrome(sat),
        Anchor::List(id) => lists.get(id).contains(&sat),
      };
      if is {
        out.push(sat);
      }
    }
    out
  }

  #[test]
  fn enumeration_matches_brute_force() {
    let lists = lists();
    let windows = [
      (0, 3000),
      (4_999_999_000, 5_000_001_000),
      (45_000_000_000 - 500, 45_000_000_000 + 500),
      (1_050_000_000_000_000 - 700, 1_050_000_000_000_000 + 700),
      (SUPPLY - 2000, SUPPLY + 5),
      (2_099_999_997_480_000 - 50, 2_099_999_997_480_000 + 1000),
      (99_999_900, 100_000_100),
    ];
    let anchors = [
      Anchor::of(SatType::Uncommon).unwrap(),
      Anchor::of(SatType::Rare).unwrap(),
      Anchor::of(SatType::Epic).unwrap(),
      Anchor::of(SatType::Mythic).unwrap(),
      Anchor::of(SatType::Black).unwrap(),
      Anchor::of(SatType::BlackRare).unwrap(),
      Anchor::of(SatType::BlackEpic).unwrap(),
      Anchor::PalindromicBlocks,
      Anchor::of(SatType::Alpha).unwrap(),
      Anchor::of(SatType::Omega).unwrap(),
      Anchor::Multiple {
        modulus: 7,
        residue: 3,
      },
      Anchor::Palindromes,
      Anchor::List(ListId::OneDigit),
      Anchor::List(ListId::Bounty3),
    ];
    for anchor in anchors {
      for (a, b) in windows {
        let mut got = Vec::new();
        anchor.for_each(&lists, a, b, |sat| got.push(sat));
        let want = brute(anchor, a, b);
        assert_eq!(got, want, "{anchor:?} [{a}, {b})");
        assert_eq!(
          anchor.count(&lists, a, b),
          want.len() as u64,
          "{anchor:?} [{a}, {b})"
        );
      }
    }
  }

  #[test]
  fn whole_chain_counts() {
    let lists = lists();
    let count = |t: SatType| Anchor::of(t).unwrap().count(&lists, 0, u64::MAX);
    assert_eq!(count(SatType::Uncommon), 6_930_000);
    assert_eq!(count(SatType::Black), 6_930_000);
    assert_eq!(count(SatType::Rare), 3438);
    assert_eq!(count(SatType::BlackRare), 3437);
    assert_eq!(count(SatType::Epic), 33);
    assert_eq!(count(SatType::BlackEpic), 33);
    assert_eq!(count(SatType::Legendary), 6);
    assert_eq!(count(SatType::Mythic), 1);
    assert_eq!(count(SatType::Alpha), 20_999_999 + 1);
    assert_eq!(count(SatType::Omega), 20_999_999);
    assert_eq!(count(SatType::MegasatAlpha), 21);
    assert_eq!(count(SatType::MegasatOmega), 20);
  }

  #[test]
  fn implied_types() {
    use SatType::*;
    assert_eq!(
      Anchor::of(Mythic).unwrap().implied(),
      TypeSet::of(&[Mythic, Legendary, Epic, Rare, Uncommon])
    );
    assert_eq!(
      Anchor::of(Legendary).unwrap().implied(),
      TypeSet::of(&[Legendary, Epic, Rare, Uncommon])
    );
    assert_eq!(
      Anchor::of(Epic).unwrap().implied(),
      TypeSet::of(&[Epic, Uncommon])
    );
    assert_eq!(
      Anchor::of(BlackEpic).unwrap().implied(),
      TypeSet::of(&[Black, BlackEpic])
    );
    assert_eq!(
      Anchor::of(MegasatOmega).unwrap().implied(),
      TypeSet::of(&[Omega, MegasatOmega])
    );
    assert_eq!(
      Anchor::of(KilosatAlpha).unwrap().implied(),
      TypeSet::of(&[Alpha])
    );
    for t in SatType::ALL {
      if let Some(anchor) = Anchor::of(t) {
        let exact = !matches!(
          t,
          Paliblock
            | PalindromesInteger3d
            | CentasatAlpha
            | DecasatAlpha
            | KilosatAlpha
            | CentasatOmega
            | DecasatOmega
            | KilosatOmega
        );
        assert_eq!(anchor.implied().contains(t), exact, "{t}");
      } else {
        assert!(t.is_exotic());
      }
    }
  }
}
