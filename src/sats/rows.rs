//! The default row set (DESIGN.md §3). Row ids are the API contract with the
//! dashboard: renaming one is a breaking change, and any change here changes
//! the catalog fingerprint (so the store asks for a reindex).

use super::{Row, SatType, SatType::*};

/// Heights of 2009's blocks (the last 2009 block is 32489).
pub const HEIGHTS_2009: (u32, u32) = (0, 32489);

fn row(id: &str, label: &str, all_of: &[SatType]) -> Row {
  Row {
    id: id.into(),
    label: label.into(),
    all_of: all_of.to_vec(),
    any_of: Vec::new(),
    heights: None,
    range: false,
  }
}

fn range(id: &str, label: &str, t: SatType) -> Row {
  Row {
    range: true,
    ..row(id, label, &[t])
  }
}

fn in_2009(id: &str, label: &str, all_of: &[SatType]) -> Row {
  Row {
    heights: Some(HEIGHTS_2009),
    ..row(id, label, all_of)
  }
}

pub fn default_rows() -> Vec<Row> {
  vec![
    row("uncommon", "Uncommon", &[Uncommon]),
    row("rare", "Rare", &[Rare]),
    row("epic", "Epic", &[Epic]),
    row("legendary", "Legendary", &[Legendary]),
    row("mythic", "Mythic", &[Mythic]),
    row("black", "Black", &[Black]),
    row("black_rare", "Black rare", &[BlackRare]),
    row("black_epic", "Black epic", &[BlackEpic]),
    row("paliblock", "Paliblock", &[Paliblock]),
    row(
      "alpha_jan_feb_2009",
      "Alpha, Jan–Feb 2009",
      &[AlphaJanFeb2009],
    ),
    row("kilosat_alpha", "Kilosat alpha", &[KilosatAlpha]),
    row("decasat_alpha", "Decasat alpha", &[DecasatAlpha]),
    row("centasat_alpha", "Centasat alpha", &[CentasatAlpha]),
    row("megasat_alpha", "Megasat alpha", &[MegasatAlpha]),
    row("kilosat_omega", "Kilosat omega", &[KilosatOmega]),
    row("decasat_omega", "Decasat omega", &[DecasatOmega]),
    row("centasat_omega", "Centasat omega", &[CentasatOmega]),
    row("megasat_omega", "Megasat omega", &[MegasatOmega]),
    row("palindrome_1d", "1D palindrome", &[PalindromesInteger1d]),
    row("palindrome_2d", "2D palindrome", &[PalindromesInteger2d]),
    row(
      "palindrome_perfect_palinception",
      "Perfect palinception",
      &[PalindromesPerfectPalinception],
    ),
    row("bounty3_name", "Bounty3 name", &[Bounty3Name]),
    row("vintage_uncommon", "Vintage uncommon", &[Vintage, Uncommon]),
    row("vintage_black", "Vintage black", &[Vintage, Black]),
    row("vintage_alpha", "Vintage alpha", &[Vintage, Alpha]),
    row("vintage_omega", "Vintage omega", &[Vintage, Omega]),
    range("block9", "Block 9", Block9),
    range("450x", "450x", Block450x),
    row("block9_alpha", "Block 9 alpha", &[Block9, Alpha]),
    row("block9_omega", "Block 9 omega", &[Block9, Omega]),
    row(
      "block9_palindrome",
      "Block 9 palindrome",
      &[Block9, PalindromesInteger],
    ),
    row("block78_alpha", "Block 78 alpha", &[Block78, Alpha]),
    row("block78_omega", "Block 78 omega", &[Block78, Omega]),
    row(
      "block78_palindrome",
      "Block 78 palindrome",
      &[Block78, PalindromesInteger],
    ),
    row("block286_alpha", "Block 286 alpha", &[Block286, Alpha]),
    row("block286_omega", "Block 286 omega", &[Block286, Omega]),
    row(
      "block286_palindrome",
      "Block 286 palindrome",
      &[Block286, PalindromesInteger],
    ),
    row("block_666_alpha", "Block 666 alpha", &[Block666, Alpha]),
    row("block_666_omega", "Block 666 omega", &[Block666, Omega]),
    row(
      "block_666_palindrome",
      "Block 666 palindrome",
      &[Block666, PalindromesInteger],
    ),
    row(
      "nakamoto_uncommon",
      "Nakamoto uncommon",
      &[Nakamoto, Uncommon],
    ),
    row("nakamoto_black", "Nakamoto black", &[Nakamoto, Black]),
    row("nakamoto_alpha", "Nakamoto alpha", &[Nakamoto, Alpha]),
    row("nakamoto_omega", "Nakamoto omega", &[Nakamoto, Omega]),
    row(
      "nakamoto_palindrome",
      "Nakamoto palindrome",
      &[Nakamoto, PalindromesInteger],
    ),
    row("pizza_alpha", "Pizza alpha", &[Pizza, Alpha]),
    row("pizza_omega", "Pizza omega", &[Pizza, Omega]),
    row(
      "pizza_palindrome",
      "Pizza palindrome",
      &[Pizza, PalindromesInteger],
    ),
    row("jpeg_alpha", "JPEG alpha", &[Jpeg, Alpha]),
    row("jpeg_omega", "JPEG omega", &[Jpeg, Omega]),
    row(
      "jpeg_palindrome",
      "JPEG palindrome",
      &[Jpeg, PalindromesInteger],
    ),
    row("hitman_alpha", "Hitman alpha", &[Hitman, Alpha]),
    row("hitman_omega", "Hitman omega", &[Hitman, Omega]),
    row(
      "hitman_palindrome",
      "Hitman palindrome",
      &[Hitman, PalindromesInteger],
    ),
    row("sr_alpha", "Silk Road alpha", &[Sr1stAuction, Alpha]),
    row("sr_omega", "Silk Road omega", &[Sr1stAuction, Omega]),
    row("sr_black", "Silk Road black", &[Sr1stAuction, Black]),
    row(
      "sr_palindrome_3d",
      "Silk Road 3D palindrome",
      &[Sr1stAuction, PalindromesInteger3d, PalindromesInteger],
    ),
    in_2009("uncommon_2009", "Uncommon 2009", &[Uncommon]),
    in_2009("rare_2009", "Rare 2009", &[Rare]),
    in_2009("black_2009", "Black 2009", &[Black]),
    in_2009(
      "palindrome_2d_2009",
      "2D palindrome 2009",
      &[PalindromesInteger2d],
    ),
  ]
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn ids_are_the_api_contract() {
    let ids: Vec<String> = default_rows().into_iter().map(|row| row.id).collect();
    assert_eq!(
      ids.join(" "),
      "uncommon rare epic legendary mythic black black_rare black_epic paliblock \
       alpha_jan_feb_2009 kilosat_alpha decasat_alpha centasat_alpha megasat_alpha \
       kilosat_omega decasat_omega centasat_omega megasat_omega \
       palindrome_1d palindrome_2d palindrome_perfect_palinception bounty3_name \
       vintage_uncommon vintage_black vintage_alpha vintage_omega \
       block9 450x block9_alpha block9_omega block9_palindrome \
       block78_alpha block78_omega block78_palindrome \
       block286_alpha block286_omega block286_palindrome \
       block_666_alpha block_666_omega block_666_palindrome \
       nakamoto_uncommon nakamoto_black nakamoto_alpha nakamoto_omega nakamoto_palindrome \
       pizza_alpha pizza_omega pizza_palindrome jpeg_alpha jpeg_omega jpeg_palindrome \
       hitman_alpha hitman_omega hitman_palindrome \
       sr_alpha sr_omega sr_black sr_palindrome_3d \
       uncommon_2009 rare_2009 black_2009 palindrome_2d_2009"
    );
  }
}
