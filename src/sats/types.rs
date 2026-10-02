//! `SatType` names and `TypeSet` helpers.

use super::{SatType, TypeSet};

impl SatType {
  /// Every type, in declaration (bit) order.
  pub const ALL: [SatType; 36] = [
    SatType::Mythic,
    SatType::Legendary,
    SatType::Epic,
    SatType::Rare,
    SatType::Uncommon,
    SatType::Paliblock,
    SatType::Black,
    SatType::BlackRare,
    SatType::BlackEpic,
    SatType::Alpha,
    SatType::MegasatAlpha,
    SatType::CentasatAlpha,
    SatType::DecasatAlpha,
    SatType::KilosatAlpha,
    SatType::Omega,
    SatType::MegasatOmega,
    SatType::CentasatOmega,
    SatType::DecasatOmega,
    SatType::KilosatOmega,
    SatType::PalindromesInteger,
    SatType::PalindromesInteger1d,
    SatType::PalindromesInteger2d,
    SatType::PalindromesInteger3d,
    SatType::PalindromesPerfectPalinception,
    SatType::Vintage,
    SatType::Block9,
    SatType::Block450x,
    SatType::Block78,
    SatType::Block286,
    SatType::Block666,
    SatType::Nakamoto,
    SatType::Jpeg,
    SatType::Pizza,
    SatType::Hitman,
    SatType::Sr1stAuction,
    SatType::Bounty3Name,
  ];

  /// The `EXOTIC_RANGES` types: pure sat-range membership.
  pub const EXOTIC: [SatType; 11] = [
    SatType::Vintage,
    SatType::Block9,
    SatType::Block450x,
    SatType::Block78,
    SatType::Block286,
    SatType::Block666,
    SatType::Nakamoto,
    SatType::Jpeg,
    SatType::Pizza,
    SatType::Hitman,
    SatType::Sr1stAuction,
  ];

  /// The JS type string.
  pub fn name(self) -> &'static str {
    match self {
      SatType::Mythic => "mythic",
      SatType::Legendary => "legendary",
      SatType::Epic => "epic",
      SatType::Rare => "rare",
      SatType::Uncommon => "uncommon",
      SatType::Paliblock => "paliblock",
      SatType::Black => "black",
      SatType::BlackRare => "black_rare",
      SatType::BlackEpic => "black_epic",
      SatType::Alpha => "alpha",
      SatType::MegasatAlpha => "megasat_alpha",
      SatType::CentasatAlpha => "centasat_alpha",
      SatType::DecasatAlpha => "decasat_alpha",
      SatType::KilosatAlpha => "kilosat_alpha",
      SatType::Omega => "omega",
      SatType::MegasatOmega => "megasat_omega",
      SatType::CentasatOmega => "centasat_omega",
      SatType::DecasatOmega => "decasat_omega",
      SatType::KilosatOmega => "kilosat_omega",
      SatType::PalindromesInteger => "palindromes_integer",
      SatType::PalindromesInteger1d => "palindromes_integer_1d",
      SatType::PalindromesInteger2d => "palindromes_integer_2d",
      SatType::PalindromesInteger3d => "palindromes_integer_3d",
      SatType::PalindromesPerfectPalinception => "palindromes_perfect_palinception",
      SatType::Vintage => "vintage",
      SatType::Block9 => "block9",
      SatType::Block450x => "450x",
      SatType::Block78 => "block78",
      SatType::Block286 => "block286",
      SatType::Block666 => "block_666",
      SatType::Nakamoto => "nakamoto",
      SatType::Jpeg => "jpeg",
      SatType::Pizza => "pizza",
      SatType::Hitman => "hitman",
      SatType::Sr1stAuction => "sr_1st_auction",
      SatType::Bounty3Name => "bounty3_name",
    }
  }

  pub fn from_name(name: &str) -> Option<Self> {
    SatType::ALL.into_iter().find(|t| t.name() == name)
  }

  /// Pure range membership (an `EXOTIC_RANGES` type).
  pub fn is_exotic(self) -> bool {
    SatType::EXOTIC.contains(&self)
  }

  pub fn bit(self) -> u64 {
    1 << self as u64
  }
}

impl std::fmt::Display for SatType {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str(self.name())
  }
}

impl TypeSet {
  pub const EMPTY: TypeSet = TypeSet(0);

  pub fn of(types: &[SatType]) -> Self {
    types.iter().copied().collect()
  }

  pub fn exotic() -> Self {
    TypeSet::of(&SatType::EXOTIC)
  }

  pub fn remove(&mut self, t: SatType) {
    self.0 &= !t.bit();
  }

  pub fn with(mut self, t: SatType) -> Self {
    self.insert(t);
    self
  }

  pub fn union(self, other: TypeSet) -> TypeSet {
    TypeSet(self.0 | other.0)
  }

  pub fn intersection(self, other: TypeSet) -> TypeSet {
    TypeSet(self.0 & other.0)
  }

  pub fn difference(self, other: TypeSet) -> TypeSet {
    TypeSet(self.0 & !other.0)
  }

  /// Every type of `other` is in `self`.
  pub fn contains_all(self, other: TypeSet) -> bool {
    self.0 & other.0 == other.0
  }

  pub fn intersects(self, other: TypeSet) -> bool {
    self.0 & other.0 != 0
  }

  pub fn len(self) -> usize {
    self.0.count_ones() as usize
  }

  /// Types in declaration order.
  pub fn iter(self) -> impl Iterator<Item = SatType> {
    SatType::ALL.into_iter().filter(move |t| self.contains(*t))
  }

  /// JS type strings, in declaration order.
  pub fn names(self) -> Vec<&'static str> {
    self.iter().map(SatType::name).collect()
  }
}

impl FromIterator<SatType> for TypeSet {
  fn from_iter<I: IntoIterator<Item = SatType>>(iter: I) -> Self {
    let mut set = TypeSet::EMPTY;
    for t in iter {
      set.insert(t);
    }
    set
  }
}

impl std::fmt::Display for TypeSet {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    write!(f, "[{}]", self.names().join(", "))
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn names_round_trip() {
    for (i, t) in SatType::ALL.into_iter().enumerate() {
      assert_eq!(t as usize, i);
      assert_eq!(SatType::from_name(t.name()), Some(t));
    }
    assert_eq!(SatType::from_name("450x"), Some(SatType::Block450x));
    assert_eq!(SatType::from_name("block_666"), Some(SatType::Block666));
    assert_eq!(
      SatType::from_name("sr_1st_auction"),
      Some(SatType::Sr1stAuction)
    );
    // The runtime's extraction criterion is no type here (rules version 4).
    assert_eq!(SatType::from_name("alpha_jan_feb_2009"), None);
    assert_eq!(SatType::from_name("requests"), None);
    assert_eq!(SatType::from_name("Uncommon"), None);
    let mut names: Vec<&str> = SatType::ALL.iter().map(|t| t.name()).collect();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), SatType::ALL.len());
  }

  #[test]
  fn type_set() {
    let mut set = TypeSet::of(&[SatType::Uncommon, SatType::Alpha]);
    assert_eq!(set.len(), 2);
    assert!(set.contains(SatType::Alpha));
    assert!(!set.contains(SatType::Omega));
    assert_eq!(set.names(), ["uncommon", "alpha"]);
    set.remove(SatType::Alpha);
    assert_eq!(set.iter().collect::<Vec<_>>(), [SatType::Uncommon]);
    assert!(set.with(SatType::Black).contains_all(set));
    assert!(!set.contains_all(set.with(SatType::Black)));
    assert!(TypeSet::exotic().contains(SatType::Sr1stAuction));
    assert!(!TypeSet::exotic().contains(SatType::Bounty3Name));
    assert_eq!(TypeSet::EMPTY.to_string(), "[]");
    assert_eq!(
      serde_json::to_string(&TypeSet::of(&[SatType::Mythic])).unwrap(),
      "1"
    );
  }
}
