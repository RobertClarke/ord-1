ord — rare-sat tracker fork
===========================

This fork of [ord](https://github.com/ordinals/ord) tracks, block by block,
where the rare sats that matter are: every uncommon and black sat, the
extractable alphas, omegas and palindromes, bounty3 names, rare-sat combos
inside pizza / jpeg / hitman / Silk Road / Nakamoto blocks, and every sat of
the vintage blocks 0–999 (which include block 9 and its 450x first BTC). It
does not index inscriptions, runes or every sat range, so it fits a 64 GiB
RAM / 1.6 TiB box and syncs in hours.

* Design: [docs/DESIGN.md](docs/DESIGN.md)
* HTTP API: [docs/API.md](docs/API.md)

It keeps upstream's `ordinals` crate (sat math) and `mockcore` (mock bitcoind
for tests). Everything under `src/` is new.
