# Rare-sat sales on chain: Magisat shapes and detection rules

Research input for the sales detector (`market::analyze`, `Sale` in
`src/model.rs`, docs/API.md "Sales and inscriptions"). Every claim below was
checked against Magisat's own sales history (all of it, not a sample) and the
transactions on chain; the numbers are exact counts over that dataset unless
stated otherwise. Dataset and scripts: §9.

## 1. The rules in short

| # | Rule | Coverage on Magisat's history |
|---|---|---|
| a | **Seller input** of a listing sale: an input that spends a tracked UTXO and whose (first) signature has sighash `0x83` (SINGLE\|ANYONECANPAY). The seller is that input's prevout script. | 35,221 / 35,221 listing sales; prevout address = API `sellerAddress` in 35,221 |
| b | **Seller payment**: output `vout == vin` of that input (SIGHASH_SINGLE pairing). It must exist. | address = `sellerReceiveAddress` 35,220 / 35,221; value = `price − makerFee` 35,217 / 35,221 |
| c | **Price** (Magisat's `price`, our `price`): `P = value(vout[vin])`. Before block **861,287** (2024-09-14 13:16 UTC): `price = P`, `makerFee = 0`. From 861,287: `makerFee = ⌊P/199⌋`, `price = P + ⌊P/199⌋` (the largest `p` with `p − ⌊p/200⌋ = P`). | exact for 34,377 / 35,221 (97.6 %): 16,134 / 16,134 before, 18,243 / 19,087 (95.6 %) after |
| d | **Magisat**: the tx has an output to `bc1pan37k3yyvpnazh4zsz284hsyvsrucxzjgs5td8h22regpfzhgu9sg4xlc6` (F1) or `3Ke21osfhEbEryUeqdwAuAY8VKxm5B9uB2` (F2) and neither address funds the tx. One fee output per tx (Σ maker fees + taker fee); **omitted when below 546 sats**. | 34,247 / 35,221 listing sales (15,398 txs) and 124 / 129 offers carry it; 797 sales in 506 txs have none; 177 sales in 12 txs were bought by F1 itself. In the last 4,032 blocks it never marked a non-sale except Magisat's own tool txs (§6) |
| e | **Accepted offer** (no `0x83`): seller input at **vin 0** signed ALL (`0x01`), buyer inputs signed ALL; `vout0` = the sold UTXO's sats to the buyer (same value), `vout1` = seller proceeds, `vout2` = Magisat fee = maker + taker fee. Offer price = `vout1 + takerFee`; since Aug 2024 `takerFee = makerFee = vout2 / 2`. | 129 / 129 on every check |
| f | **Bulk buys**: one Sale per `0x83` tracked input; seller inputs are contiguous at `vin k+1 … 2k`, payments at the same indices, one aggregated fee output at `2k+1`. | standard layout in 15,671 / 15,916 txs; the other 245 are an Apr–Jul 2024 variant |
| g | **Sold sats** go where FIFO puts them (the tracker's move): `vout = vin − k` in the standard layout, `vout 0` in the variant and in offers. | 35,221 / 35,221 computed; 121 / 121 identical to the fork's recorded moves |

False positives and other venues are in §6; the short version: `0x83` on a
tracked input was a sale in every case seen, but 18 of 70 such txs in the
last 4,032 blocks were not Magisat; `0x81` (ALL|ANYONECANPAY) is both a
2-party PSBT sale (84 txs, mostly 450x) and exchange consolidation (94 txs);
F1/F2 outputs also appear in Magisat's non-sale tools (transfers, isolation)
and in txs where Magisat itself buys (F1 is its trading wallet too).

## 2. Data

* Tag catalog: `GET https://api.magisat.io/tag` (no key) returns the 74
  rare-sat tags (`isVirtual: false`); `?includeIsVirtual=true` adds 2,765
  virtual tags, which are all inscription collections. There is no
  `/tags`, `/categories` or public `/sattribute` list; `/tag/by-slug?slug=`
  returns one tag. Catalog, mapping and gaps: `data/magisat-tags.json` (§8).
* Sales: `GET /activity/tag/{tagId}?additionalSattributes=&offset=&limit=50&type=PURCHASE,OFFER_PURCHASED`
  for each of the 73 tags (the `inscription` tag was skipped: 72,148
  activities, mostly inscription collections, not a rare-sat category).
  A tag's feed lists every sale whose UTXO contains a sat of that tag, not
  only those whose `mainTag` it is, so the 94,136 activities collapse to
  **37,749 unique sales** by `(buyerTxId, utxo)` (the `tags` field keeps
  every feed a sale appeared in).
* 37,620 `PURCHASE` + 129 `OFFER_PURCHASED`, `purchasedAt`
  2023-08-23 → 2026-09-30, blocks 804,528 → 969,352. By year: 2023 5,188,
  2024 15,006, 2025 11,127, 2026 6,428. Per tag per year:
  `data/magisat-sales-summary.json`.
* Not decodable: 630 records without `buyerTxId` (all `isExternal: true`,
  629 without `purchasedAt`; status `UNLISTED` 383, `DECORATION_PENDING`
  137, `DECORATED` 95, `UNDECORATED` 15), and 269 whose `buyerTxId` never
  confirmed. Of the latter, 66 carry `spentAtBlockNumber`; 59 of those are
  `RBF` listings (below), and in 17 of the 18 checked the UTXO was really
  bought by the original other-marketplace transaction: same input, same
  paired output, no Magisat fee (e.g. Magisat's
  `0647a0c568f220125ad047c570d5fdcc684a6daf6a7eb3e33743fad5c5b9cf8e` never
  confirmed; the UTXO went in
  `e90cef67efd58269cb448d3faba37392fa13f58fdd479ed2ce84949c506707ff`, vin 2,
  `0x83`, 1,443,080 sats to the same seller address).
* Not sales: 1,500 `GIVEAWAY` "purchases" (Trump sats, price 1 sat,
  2025-01-20, seller `bc1pllxtnvjs…`, signed ALL), e.g.
  `979adde9cd63751a03e1eb0429fc366e0ffc700ce082441d8b41363d59d375ab`.
* Listing `type` in the snapshot: `OWNER` 23,058, `RBF` 2,193 (Magisat
  imports a pending purchase from another marketplace, whose seller signature
  is public in the mempool, and lets users replace it), `OFFER` 41,
  `GIVEAWAY` 1,500, absent 10,957 (2023 – early 2024 records). `RBF`
  listings have the same on-chain shape as `OWNER` ones; their seller signed
  on the other marketplace.
* Decoded: 35,221 listing sales in 15,916 transactions and 129 offers,
  every transaction fetched with `getrawtransaction <txid> 2`.
* For ongoing validation: `GET /activity/global?offset=&limit=50&type=PURCHASE,OFFER_PURCHASED`
  is the newest-first feed across all tags (no `count`), and
  `/activity/owner/{address}` one wallet's.

API fields that matter: `price` (string sats), `makerFee`, `utxoValue`,
`relativeUnitPrice` = ⌊(price − utxoValue) / mainSatoshiAmount⌋ (exact in
35,623 sales, floored in 286, absent in 211), `signatureIndex`, `sellerAddress`,
`sellerReceiveAddress`, `mainSatoshi.{rangeStart, rangeEnd, count, offset,
sattributes}` (`offset` absent in 2023 records), and for offers
`offerSnapshot.{price, makerFee, takerFee, makerAddress, takerAddress,
makerReceiveAddress, takerReceiveAddress}`. The listing `price` of an
offer sale is the ask, not the sale price (equal in 95 / 129).
`affiliateId` (5,913 sales), `referralAddress`, `referralCommission` exist
but are settled off chain: `referralAddress` never receives a commission
output (it appears among the outputs in 1 of 498, as the buyer).

## 3. Listing purchases

Standard layout of a Magisat buy of `k` listings (15,671 of 15,916 txs):

```
inputs                                     outputs
0 .. k      buyer padding: k+1 UTXOs       0          Σ padding → buyer
            (all 600 sats in 93.7 %)       1 .. k     sold UTXOs, each = its seller input's
                                                      value, → buyer receive address
k+1 .. 2k   seller UTXOs, sighash 0x83     k+1 .. 2k  seller payments (paired with the
                                                      inputs at the same index)
2k+1 ..     buyer funding                  2k+1       Magisat fee (F1/F2), one for all k
                                           …          k+1 new 600-sat padding outputs
                                                      → buyer (97.9 %)
                                           last       buyer change
```

Single listing (`k = 1`): `[pad, pad, seller, funding…] →
[2×pad, sold UTXO, payment, fee, 600, 600, change]`, e.g.
`3854c0ba0ea36ac5993be75f366395a21bc77c5621887f0a9e59a77d07e5c500`
(2023-10, 35,000 sats, fee 875 = 2.5 %) or the API example's
`09d718c6d49d20d2014359374248de91586ddf8454663df425a1ea803b7eb4f2`
(`k = 2`, two 45,546-sat alpha uncommons: payments 45,319 at vout 3 and 4 =
vin 3 and 4, fee 3,254 = 2 × 227 maker + 2,800 taker).

Measured on the 15,671 standard txs: output 0 = Σ padding inputs 100 %;
outputs `1..k` = seller input values in order 100 %; fee at `2k+1` 15,153
(506 have none, 12 elsewhere). The variant (245 txs, 2024-04-16 → mostly
2024-07, a few later) has **one** padding input, sellers at `vin 1..k`, and
merges the padding, every sold UTXO and part of the funding into output 0:
`844265bdfba89e3fc21058807c778ec0ab5d227b35b94927a82043f26212b746`
(`k = 5`), `5d028ecd0872348c1316ace2821bc11ae6134df60f50f479a1a4828e414ce171`.

### a. Seller input

* Input spending the API's `utxo`: sighash `0x83` in **35,221 / 35,221**,
  across all years, listing types and categories (table §7). Seller script
  types: P2TR 29,704 (65-byte Schnorr sig ending `0x83`), P2WPKH 5,487 and
  P2SH-P2WPKH 30 (DER sig ending `0x83`).
* Prevout address = `sellerAddress`: 35,221 / 35,221.
* `signatureIndex` is **not** the vin (equal in 573 / 35,221, by chance;
  values go up to 220). It is the input's index in the PSBT the seller
  signed when listing: bulk listings sign many inputs in one PSBT (listings
  a seller created in the same second carry distinct indices in 1,873 of
  2,534 such groups). The vin is chosen later by the buyer's layout.
* Buyer inputs in listing txs never use `0x83`: P2SH-P2WPKH and P2WPKH sign
  `0x01`, P2TR signs `0x00` (64-byte).

### b. Payment output

* `vout[vin]` exists and pays `sellerReceiveAddress` in 35,220 / 35,221;
  its value is exactly `price − makerFee` in 35,217. The 4 exceptions
  (`bca278278e7a…`, `3c56078410c8…`, `c48679b8e77f…`, `d94ee5f88a0c…`, all
  2025) pay a different amount (one also a different address) than the
  API's listing records; the `0x83` pairing itself held.
* `sellerReceiveAddress` ≠ the seller's input address in 22,648 sales
  (64 %): wallets with separate ordinals and payment addresses. Never use
  "output to the seller's input address" to find the payment.

### c. Price

* `makerFee > 0` in 18,398 sales and always equal to ⌊price × 0.5 %⌋
  (`⌊price/200⌋`, 18,398 / 18,398).
* The first sale with a maker fee is block 861,287 (2024-09-14 13:16 UTC,
  `c4d60e298dcc8b3c59606c82a46a2dbd9f3a93a57f9032b2e28812e1e4147bbf`).
  Before it every sale has `makerFee = 0` and `price = P` (16,134 / 16,134).
  After it, listings signed before the launch still sell without a maker fee
  (689 of 19,087 sales, the last at block 963,683 on 2026-08-23).
* Inverting `P = p − ⌊p/200⌋`: `p = P + ⌊P/199⌋` is always a solution, and
  when `P % 199 == 0` (a price that is a multiple of 200, so common) `p − 1`
  is one too. The larger root was right in 3,527 of 3,681 such cases.
* Rule exactness after the launch: 18,243 / 19,087 (95.6 %). Misses: 689
  pre-launch listings (true `makerFee = 0`, rule says 0.5 % too much), 151
  prices ending in …99 (smaller root), 4 API errors (b).
* Optional refinement, +186 exact (96.6 %): in a single-listing tx, if the
  fee output `F` is a plausible taker fee for `price = P` (a schedule
  minimum × discount, or ⌊1 % × P⌋ × discount, §5) and `F − ⌊P/199⌋` is not,
  take `makerFee = 0`.
* Our `Sale` fields: `price` = Magisat price; `seller_proceeds = P`;
  `fees = price − P` (the maker fee; the buyer's taker fee is extra and not
  part of `price`); `premium = price − seller.value`; `unit.price =
  premium / count` = Magisat `relativeUnitPrice` (floored).

### f. Bulk buys

* Txs by number of `0x83` inputs `k`: 1: 12,213; 2: 1,305; 3: 597; 4: 371;
  5: 351; 6: 159; 7: 104; 8: 114; 9: 73; ≥10: 629 (up to 60). In 15,856 of
  them every listing is in the API.
* Fees are not split on chain: the one fee output = Σ ⌊price_i/200⌋ +
  taker fee on Σ price_i (with the bulk minimum, §5); attribute the maker
  fee per listing by rule c and, if needed, the taker fee pro rata.
* Magisat sometimes is the buyer: F1 also funds purchases (e.g.
  `53f61c21954cb7ca60a1f58af512b38725bd3b72fba2bf11244d5a1160d867c0`, 50
  uncommons at ~1,000 sats bought by F1; 104 outputs to F1 are the bought
  sats, padding and change, not fees). 12 such txs; skip "fee" outputs to an
  address that also funds the tx.

### g. Where the sold sats go

FIFO over the tx (input sats in input order, outputs in order). In the
standard layout the UTXO of seller input `i` lands whole in output `i − k`
(34,836 sales); in the variant everything lands in output 0 (385). Checked
against the fork's own moves for the 121 sales in the retained window whose
sats are tracked: 121 / 121 identical. The detector should take the
destination from the tracker's `Move` (pieces per output) rather than
recompute it. Self-purchases (sats return to the seller's script): 8 /
35,221 — flag, do not drop.

## 4. Accepted offers (`OFFER_PURCHASED`)

```
inputs                                   outputs
0     seller UTXO, sighash 0x01 (ALL)    0  sold sats → makerReceiveAddress (= seller input value)
1 ..  buyer funding, sighash 0x01        1  offer price − takerFee → takerReceiveAddress (seller)
                                         2  Magisat fee = makerFee + takerFee (F1/F2), absent if < 546
                                         3  buyer change
```

* The buyer (maker) signs the whole transaction when making the offer (the
  UTXO is known: offers are made on a listing), the seller (taker) signs
  vin 0 when accepting. Both sign ALL; taproot inputs use an explicit
  `0x01` (65-byte), never `0x00` (58 / 58 taproot sellers, 265 / 265 taproot
  buyer inputs), unlike the buy flow above.
* 129 / 129: seller at vin 0, seller sighash `0x01`, buyer inputs `0x01`
  (544 / 544), sats to vout 0, vout 0 value = seller input value, vout 0 =
  `makerReceiveAddress`, vout 1 = `takerReceiveAddress` and = `price −
  takerFee`, fee outputs = `makerFee + takerFee`. Fee output at vout 2 in
  124 (5 zero-fee offers).
* Fees: 2024-03 maker 2.5 %, taker 0 (`39a640d232ebcab2326d6f4b3f295c3ead0808545a0e74c1117bc82633f08b6f`:
  2,500,000 to the seller, 62,500 fee); 2024-04 → 2024-10 percentages with
  15,000 minimums on each side (`750c5e1e1f180823f4a267c5e7b27685d0e3e2d7ac8bc7c467ac3d0a44dc42e3`:
  985,000 + 30,000); since 2024-09 1 % + 1 % (`19df449751a6bc87d75195f282a3886d14f256541e052d4239d7e3ca590097dc`:
  4,000,000 offer, 3,960,000 + 80,000); zero-fee offers since 2025-11
  (`b1a71ed6e428c95c8d35f3a899b4665758477738dd7c90a86df6417cb0360b74`).
  `makerFee == takerFee` in 125 / 129, so `price = vout1 + vout2/2`; with no
  fee output, `price = vout1`.
* Detection without `0x83`: vin 0 tracked, vout 0 value = vin 0 value and
  vout 0 script ≠ vin 0 script, an F1/F2 output that no input funds, and the
  other inputs from other scripts. The API misses some: an offer-shaped
  F2 sale of a rare sat for 5,200,000 sats,
  `82d5b938c1d5019a73f33d39f9f0bcede7e8be24de1f7cf2ee3eae4bed935c20`
  (block 965,591), is in no tag feed (probably a collection offer, which
  the feeds may not list).

## 5. Fees and fee addresses

**Addresses** (also returned by `GET /info/platform-fee-percentage`,
`/info/isolation-fee`, `/info/safe-transfer-fees`, and hard-coded in the
web bundle):

| address | role | first / last seen in sales |
|---|---|---|
| F1 `bc1pan37k3yyvpnazh4zsz284hsyvsrucxzjgs5td8h22regpfzhgu9sg4xlc6` | `platformFeeAddress` (and Magisat's own buying wallet) | 804,700 (2023-08-24) → 969,352 (2026-09-30) |
| F2 `3Ke21osfhEbEryUeqdwAuAY8VKxm5B9uB2` | `secondPlatformFeeAddress` | 831,297 (2024-02-20) → 967,176 (2026-09-15) |

Which one a tx uses (one fee output per tx, by the total fee):

* ≤ block 831,279 (2024-02-20 17:10 UTC): F1 always.
* 831,297 → 852,668 (2024-02-20 → 2024-07-17): F2 always.
* ≥ 852,779 (2024-07-18): F2 if 10,000 ≤ fee ≤ 100,000,000, else F1 (the
  bundle's `choosePlatformFeeAddress` with `secondAddressMinThreshold`
  10,000 / `secondAddressMaxThreshold` 100,000,000): 0 exceptions in 7,681
  listing txs.
* No fee output when the total is below 546 sats (`calculateOfferPlatformFee`
  zeroes fees under dust; listings behave the same): 797 listing sales in
  506 txs have none, 171 txs in 2023-08/09 (2.5 % of small prices, no
  minimum yet) and 242 in 2025-07/08 (minimum dropped to 0). In 395 of the
  506 the maker fees plus the bare percentage are below 546; the other 111
  (mostly 2025-01 → 06) had their minimum waived, presumably by a wallet
  discount, e.g.
  `0cdb671fb9367b008a97512a40708d0467ad7de96c4217b0b7f88e76331fc612`.
  A Magisat sale without a fee output is indistinguishable from another
  `0x83` venue (§6): attribute `marketplace: "unknown"`.

**Maker fee** (seller, deducted from the paired output): 0 until block
861,287, then 0.5 % floored, no minimum.

**Taker fee** (buyer, on top of the price, in the fee output only), as fitted
on 12,213 single-listing txs:

| period | rate | minimum (before discounts) |
|---|---|---|
| 2023-08 → 2023-09 | 2.5 % | none (sometimes raised to 546) |
| 2023-10 → 2023-11 | 2.5 % | 600 |
| 2023-12 | 2.5 % | 6,000 |
| 2024-01 → 2024-08 | 2.5 % | 15,000 |
| 2024-09-14 → 2024-10 | 1 % | 15,000 |
| 2024-11 → 2025-03 | 1 % | 12,000 |
| 2025-03 → 2025-06 | 1 % | 10,000 |
| 2025-07 | 1 % | 0 |
| 2025-08 → now | 1 % | 2,800 (= current `platformFeeListingTakerBulkMinValue`) |

Bulk buys mostly apply one minimum to the summed price (some 2023-10 and
2024-03 buys charged it per listing). Wallet-specific
"collaborator discounts" (`/wallet/discount`, multiplied `feeModifier`s)
scale the fee: observed multiples 0.8, 0.7, 0.56, 0.5, 0.49, 0.35, 0.25,
0.175 of the minimum (e.g. 12,000 / 8,400 / 6,000 / 4,200 / 3,000 in
2024-11, 2,800 / 2,240 / 1,959 / 1,400 / 979 / 700 in 2025-26). The taker fee
therefore cannot be predicted exactly, which is why rule c does not use it.
Current values (`/info/platform-fee-percentage`): listing taker 1 %, maker
0.5 %, offer taker 1 % and maker 1 %, no minimums except bulk taker 2,800
and collection-offer taker 2,800.

## 6. False positives and other venues

All transactions in the fork's retained moves (blocks 965,413 → 969,444)
that spend a tracked UTXO worth ≤ 0.1 BTC: 15,008 txs, classified by the
sighash of the tracked input (`fp.json` on gp):

| class | txs | what it is |
|---|---|---|
| `0x01` / `0x00` only, no Magisat fee | 14,748 | transfers, consolidations, pool payouts |
| `0x83`, Magisat fee output | 52 | Magisat listing sales: 52 / 52 in the API (120 sales) |
| `0x83`, no Magisat fee | 18 | other venues and OTC: sellers using a Magisat-like layout without a fee, 450x listings with a 1,000-sat fee to `33HToBLb5xGhkoHbYCR7KZP7hy57ZnrCJW` (`4496989b62716ff0df954a29b43fe9ac96a153658801392f5ff25fe2d705aa21`), a 10,000-sat fee to `bc1qxma2dwmutht4vxyl6u395slew5ectfpn35ug9l` (tx.dev), fee-less 2-output swaps (`ce4dc92baaba478327bbd6b7e182576366f79334c72b40e46f5c54551df7c0cb`). Every one pays its paired output to a third party and moves the sats elsewhere: sales, `marketplace: "unknown"` |
| `0x81` seller input + buyer inputs | 84 | 2-party PSBT sales (seller signs ALL\|ANYONECANPAY over `[sats → buyer, payment → seller]`, buyer adds funding), mostly 450x / block 9: `0c47e0acc1b4f2de35cab4a8deaf278fe47f0d9a707135dd8e2488a228a8ec86`. **A sale without `0x83`** |
| `0x81` on every input, one owner | 78 | exchange withdrawals / batches (`cf061ead6b1288ecaab6512ea1b074504844fbe63506d9d410a3a8ad1463e935`): not sales |
| `0x81` on every input, many owners | 16 | consolidations of many P2PKH deposits into one output (`06d243eb5e28a746c93d81fe1d365fb80d9d98b46e4d483702a5cf9664e93104`): not sales |
| `0x82` (NONE\|ANYONECANPAY) | 6 | sweeps of dozens of unrelated inputs (`f810a3252a4bccf33f3a4552a6d72970995cd3918b9682d02639dee63023358a`): not sales |
| Magisat fee, ALL-signed, in the API | 1 | accepted offer |
| Magisat fee, offer-shaped, not in the API | 1 | offer `82d5b938…` (§4) |
| Magisat fee, one owner / tools | 4 | safe transfer (`c421d418dc00729c7f0dad77dcbf36b222771014b7f72b55c5d862e633f23471`, fee 7,000 = 5,000 + 1,000 per input), isolation of block 9 sats (`6de0f2f080d5a7b97c4262c00e78b09aa7a2e408899195eb2ca438aad920b2d4`), sends (`8c4d350491d327282f2b212d01f2202c89ba03d86021d5268bb5900fc219f39b`): not sales |

Every API sale in that window whose sats the fork tracks was found (121 /
121; the other 134 sold untracked sats).

Rules for the detector:

1. A tracked input signed `0x83` whose paired output exists is a listing
   sale (precision 70 / 70 here). Magisat iff an F1/F2 output that no input
   funds is present; otherwise `marketplace: "unknown"` (or another venue's
   fee address). `vin ≥ outputs` (the SIGHASH_SINGLE bug) is not a sale.
2. `0x81` on a tracked input is a sale only when the other inputs are not
   `0x81` and come from other scripts, and an output pays the seller.
   All-`0x81` transactions are not sales. `0x82` never.
3. An F1/F2 output alone is not a sale: require the offer shape (§4) and a
   counterparty; single-owner txs (Magisat transfer, isolation, split,
   inscribe tools) and txs funded by F1 (Magisat buying) are not Magisat
   fee events.
4. Not sales in Magisat's own data: `GIVEAWAY` records; records without
   `buyerTxId`; `buyerTxId`s that never confirmed (failed RBF snipes, the
   real sale is the other venue's tx with the same paired output).
5. Sales of inscribed sats are real `0x83` sales but priced as inscriptions:
   keep them, flagged `inscribed` (API.md).

### 6.1 Offers below the sold UTXO's value (rules of 2026-10)

Over the fork's whole sales history (292,912 sales to block 969,592), 18
had a price below the sold UTXO's value: 17 offers (shape 4, `offer`) and
one listing. Every offer was a misreading of the transaction, in three
classes, each now ruled out by a shape rule (replayed over all 78,222
offer sales: these 22 removed, 9 genuine sales re-priced, nothing else
changed):

| class | sales | what it is | rule |
|---|---|---|---|
| funding taken for a seller | 11 (Magic Eden, 2023-2024) | `[item vin 0] → [item vout 0, payment, fee, change]` where the buyer's funding at vin 1 holds tracked sats: FIFO puts them in the payment, so the funding reads as a seller paid by the item output (10,000 sats for a 0.3 BTC UTXO), e.g. `7b2a675a67dd0fd8a643c2673d48a2bf4bbb94aca83210bc5def23277f1e7462`. In 9 of the 11 the item at vin 0 held tracked sats too and is the real sale (now with the whole fee) | an output taking exactly the sats of an earlier input, whole, at another script, is that input's item, never a payment (no other offer was paid by one) |
| rune bid fills | 4 below value, 5 more above it | Magic Eden rune bids: a runestone at vout 0, bidders' funds in 1-of-2 P2WSH escrows, each escrow's change back to it, the seller's 546-sat rune UTXOs; a tracked sat in one escrow lands in another bidder's change, e.g. `0409b5475cabb8a67ed1f84eb4252476a713f78363c0cc4eff74bbdfa5fa5cd3` | no offer in a transaction with a runestone (no offer outside these 9 carries one) |
| exchange batch | 2 (Satflow) | a 43 BTC payout of five 10 BTC inputs, 32 outputs, one of them Satflow's new fee address (its first payment): the two tracked sats land in the batch's change and the largest payout reads as the payment, 3.08 BTC for 10 BTC each, `774a0494884ff37ed2ed9481b7ac936e7869a661d5ebb7a46ecb5f9a1b73921a` | the item output must be fed by the seller and at most one other party (only these 2 and 2 of the rune fills had more) |

A last-line guard drops an offer that hands a whole UTXO over (no sat of it
returns to the seller's script) for less than its value; it removes nothing
the rules above leave. It is not applied to split sales (`seller.value` is
the whole UTXO while the seller keeps most of it) nor to listings: the
remaining listing below value, a black omega in a 10,000-sat UTXO sold for
1,000 sats by a SINGLE|ANYONECANPAY signature at block 847,609
(`67ff982d06614379d66800bb3b7e17011246ffe59df17160a947af6e85ad595c`), is the
seller's signed price. Two more listings sold at exactly the UTXO's value
(`premium` 0), also signed.

## 7. Coverage by stratum

Listing sales (rules a–c, d presence, g) and offers (rule e), by period, buy
size and category group (the full 58-stratum table with two example txids
each is `strata.json` on gp):

| kind | buy | period | n | 0x83 | paired = receive | paired = price − mf | price rule exact | Magisat fee output | sats located |
|---|---|---|---|---|---|---|---|---|---|
| listing | single | 2023 | 2,753 | 2,753 | 2,753 | 2,753 | 2,753 | 2,582 | 2,753 |
| listing | bulk | 2023 | 2,435 | 2,435 | 2,435 | 2,435 | 2,435 | 2,435 | 2,435 |
| listing | single | 2024 | 5,255 | 5,255 | 5,255 | 5,255 | 5,094 | 5,243 | 5,255 |
| listing | bulk | 2024 | 9,663 | 9,663 | 9,663 | 9,663 | 9,223 | 9,656 | 9,663 |
| listing | single | 2025–26 | 4,205 | 4,205 | 4,204 | 4,201 | 4,122 | 3,945 | 4,205 |
| listing | bulk | 2025–26 | 10,910 | 10,910 | 10,910 | 10,910 | 10,750 | 10,563 | 10,910 |

| kind | period | n | seller vin 0 | seller ALL | vout1 = price − taker fee | Magisat fee output |
|---|---|---|---|---|---|---|
| offer | 2024 | 22 | 22 | 22 | 22 | 22 |
| offer | 2025–26 | 107 | 107 | 107 | 107 | 102 |

Category groups (by the sale's Magisat tags) are uncommon/black/rare,
alpha/omega, palindromes, block 9/450x, vintage/b78/b286/b666/Nakamoto,
pizza/JPEG/hitman/Silk Road, names, and Trump/Labitbu/legacy/parasite;
`0x83` and the pairing hold at 100 % in every one of them.

Examples: 2024 single, F2, 15,000 minimum:
`d2d35be449ec5b76f451f8c4ac4f8902473e5013fdc36aff0901ed1b3a3485bd`;
2024-10 with maker fee: `72cb8faa3f6806fad40fff7e2bfadfc701d1d320796d5b67932d7adb67c1b954`
(490,000, paired 487,550, F2 12,950 = 2,450 + 10,500 discounted taker);
2025 F1: `d52437e0272fe3fcb91b8bae542c93836aa3e9b7cfed170363996690a6c5508c`
(31,500, paired 31,343, fee 2,957 = 157 + 2,800); a 0.1 BTC rare sat:
`a67fb7f7a12730a47d43902e9388856c928c4e791167cb193931db61963d95b5`
(10,000,546, paired 9,950,544, F2 150,007 = 50,002 + 100,005); `RBF`
listings: `e4a8fa8996d4b298a40c94c99535542a8c21795a49144d7a05446604fffcb09b`,
`69f503b63eac73ccce4f7675ab8d2e41618f6ff6d4066c3d13a0e86e4222eff3` (4
listings of one seller in one buy); no fee output in 2023-09 (15,000 sats, 2.5 % < 546):
`90c55758af26e17c646a146adce237f92b70fb9228eef485ba213c61b0c4fd7d`.

## 8. Categories: Magisat tags vs our rows

`data/magisat-tags.json` has every tag (id, slug, label, sattributes,
`additionalSattributes` filters, Magisat stats, sales in the dataset), its
relation to our rows, our stats now and at block 932,293, proposals and the
coverage analysis. Of the 74 tags: 32 **exact**, 15 **ours_superset** (our
row strictly contains the tag), 5 **ours_subset** (we track only part: alpha,
omega, Nakamoto, 3d and Silk Road palindromes), 17 **gap**, 5 out of scope
(`trump`, `parasite`, `legacy`, `labitbu-official`, `inscription`).

Definition differences behind "superset":

* Magisat's `uncommon`/`rare`/`epic` are ord's exclusive rarities (uncommon
  6,926,535 = 6,930,000 − 3,465; rare and epic exclude the 6 legendary
  blocks), and `black-uncommon`/`black-rare`/`black-epic` likewise exclude
  the higher black ranks. Our rows include them.
* **Off-by-one in our exotic ranges.** The JS `isInRanges()` treats the
  exported `[start, end)` pizza/JPEG/hitman/Silk Road ranges as inclusive
  (`value <= end`), so every range adds the first sat after it, which is an
  uncommon and/or alpha whenever the range ends on a block or BTC boundary:
  pizza 1,000,000,000,825 sats for 825 ranges, JPEG +10, hitman +5,918, Silk
  Road +405,534 (vs Magisat's exact 1e12, 5e10, 1.67e11, 2,965,651,306,529).
  Inflated rows: `pizza_uncommon` 378 vs 201, `pizza_alpha` 10,192 vs 10,002,
  `hitman_uncommon` 56 vs 25, `hitman_alpha` 1,699 vs 1,662, `sr_uncommon`
  1,699 vs 829, `sr_alpha` 30,537 vs 29,542, `jpeg_uncommon` 20 (10 real),
  `jpeg_alpha` 510 (500 real); omega, black and palindrome combos match to
  ±1. It shows up in real sales: 6 sold uncommons are "pizza"/"Silk
  Road"/"hitman" uncommons for us but plain uncommons for Magisat, e.g. sat
  282,610,000,000,000 (first sat of block 56,522; the real last pizza sat is
  the black 282,609,999,999,999) sold in
  `8ee7bd8152bedc4b783714d556bd43215b7aae9835a958f0e4ed04698ccf284e`. A
  detector pricing by row would value these as pizza uncommons. This is
  inherited from bitcoin-monitoring and worth fixing there.
* `1st-tx` = block 9 sats `[45e9, 46e9)` (by FIFO through block 170's
  `f4184fc596403b9d638783cf57adfe4c75c605f6356fbc91338530e9831e9e16`, whose
  output 0 sent those 10 BTC to Hal Finney); the other 40 BTC never moved,
  so the `b9` and `1st-tx` sales lists are identical (5,873, every sold sat
  below 45,589,677,779).
* Magisat has no year tags: "2009 uncommon" etc. are its `uncommon`,
  `rare`, `black-uncommon` and `2d-pali` tags with the UI's year filter
  (`mainSatoshi.blockTimestamp`). Our `uncommon_2009`, `rare_2009`,
  `black_2009`, `palindrome_2d_2009` rows are those filters (heights
  0..=32,489), so they map exactly up to the rarity exclusivity above.
* Palindrome definitions, checked on all 8,596 sold palindromic sats after
  each tag appeared: `1d`/`2d` = exactly 1/2 distinct digits (Magisat counts
  sat 0); `3d-pali` = 2 **or** 3 distinct digits per sat (its totalSupply
  1,249,047 is the exactly-3 count); `seq-pali` = a run of ≥ 3 identical
  digits (8,591 / 8,591); `uniform-palinception` = ≥ 2 equal blocks of length
  ≥ 3, each a palindrome (6,879 / 6,880); `pali-block` = a palindromic sat in
  a block whose height is a palindrome (7,927 / 7,927; not our `paliblock`,
  which is the uncommon/black sat of such a block); `perfect-palinception`
  = ours (7,428 / 7,428). Totals for seq (16,896,268 vs 16,796,355) and
  uniform (1,830,989 vs 1,850,789) still differ by ~1 %, so an edge case of
  Magisat's rules is unknown.

**Gaps ranked by sales** (sales = sales in the tag's feed; untracked = the
listing's main sat is not tracked by the fork today):

| Magisat tag | sales | untracked | proposal (our type vocabulary) | tracking cost |
|---|---|---|---|---|
| `vintage` | 9,502 | 3,326 | `vintage` range row, all_of [vintage] | range [0, 5e12) in `TrackerConfig::ranges` (reindex) |
| `pali` | 9,275 | 8,203 | `palindrome`, all_of [palindromes_integer] | ~121M individual sats: impractical |
| `nakamoto` | 6,998 | 996 | `nakamoto` range row | 19 blocks as ranges (block 9 already) |
| `alpha` | 6,874 | 2,299 | `alpha`, all_of [alpha] | ~21M individual sats |
| `omega` | 4,503 | 2,136 | `omega`, all_of [omega] | ~21M individual sats |
| `alpha-uncommon` | 4,251 | 3 | `uncommon_alpha`, all_of [uncommon, alpha] | none (all tracked) |
| `seq-pali` | 2,919 | 2,566 | `palindrome_sequence`, new type `palindromes_sequence` | ~16.9M sats |
| `pizza` | 2,689 | 1,994 | `pizza` range row (fix the off-by-one first) | 825 ranges |
| `hal-finney` | 2,457 | 898 | `finney` range row, new exotic type | needs Magisat's 127-block list |
| `3d-pali` | 2,191 | 1,895 | `palindrome_3d`, any_of [palindromes_integer_2d, palindromes_integer_3d] | ~1.25M new sats (track palindromes_integer_3d) |
| `omega-black-uncommon` | 2,069 | 3 | `black_omega`, all_of [black, omega] | none |
| `b78` | 1,352 | 1,197 | `block78` range row | 1 range |
| `vintage-pali` | 1,206 | 966 | `vintage_palindrome`, all_of [vintage, palindromes_integer] | ~6.0M sats |
| `uniform-palinception` | 1,056 | 933 | new type `palindromes_uniform_palinception` | ~1.83M sats |
| `silkroad` | 973 | 802 | `sr_1st_auction` range row | 405k ranges |
| `pali-block` | 804 | 716 | `paliblock_palindrome`, all_of [palindromes_integer, palindromic_height] (new type) | ~1.74M sats |
| `b286` | 401 | 383 | `block286` range row | 1 range |
| `hitman` | 400 | 226 | `hitman` range row | 5,914 ranges |
| `jpeg` | 388 | 324 | `jpeg` range row | 10 ranges |
| `b666` | 352 | 261 | `block_666` range row | 1 range |
| `silkroad-pali` | 330 | 325 | `sr_palindrome`, all_of [sr_1st_auction, palindromes_integer] | ~197k sats |
| `1st-tx` | 5,873 | 1 | `block9_1st_tx`, sub-range [45e9, 46e9) | none |
| `pizza-black-uncommon` | 1 | 0 | `pizza_black`, all_of [pizza, black] | none |
| `hitman-black-uncommon` | 0 | 0 | `hitman_black`, all_of [hitman, black] | none |

Of 35,462 market sales, the fork tracks the main sat of 18,578; 825 are
out of scope and **16,059** are rare-sat sales it cannot see. Greedily, the
range rows plus the palindrome sub-types (vintage, seq, pizza, 3d, Silk
Road, pali-block, Nakamoto, uniform, JPEG, hitman, Finney) would cover
10,318 of them; what remains is plain palindromes (3,067), plain alphas
(2,219) and plain omegas (448). The free rows (`uncommon_alpha`, 4,251
sales; `black_omega`, 2,069; `pizza_black`; `hitman_black`;
`block9_1st_tx`) only label sales whose sats we already track.

**Sales per our row** (market sales whose main sat is in the row, all
years; `sales_by_our_row` in the JSON has them per year): uncommon 7,737,
block9 5,818, 450x 5,209, black 3,455, pizza_palindrome 312, bounty3_name
239, palindrome_2d 184, kilosat_alpha 182, alpha_jan_feb_2009 166 (a row until rules version 4),
nakamoto_palindrome 129, block78_palindrome 121, hitman_palindrome 117,
uncommon_2009 113, sr_alpha 86, palindrome_perfect_palinception 77,
block_666_palindrome 68, vintage_alpha 66, kilosat_omega 53, black_2009 50,
then ≤ 41 each (rare 28, black_rare 5, epic/legendary/mythic 0).

**Supply vs Magisat** (full table in the JSON):

* Magisat's `circulatingSupply` is its mined count **as of block ≈ 932,293
  (2026-01-14)**: it equals our `mined` at that height for uncommon
  (931,827 = 932,294 minus the rare and epic blocks, block 0 counted once),
  black-uncommon (931,828), black-rare (462), epic (4), 2d palindromes
  (30,854) and perfect palinception (13,202 = 13,065 + 137). Magisat's tag
  stats are stale by ~37,000 blocks; compare with our history at 932,293
  (`/supply/{row}/history`), not with `/supply`.
* `sequestered` ≈ our `extracted` at 932,293 within ~3 %: uncommon 223,476
  vs 220,911, black 112,167 vs 111,068, rare 141 vs 140, 2d 1,210 vs 1,208,
  pizza palindromes 5,593 vs 5,602, bounty3 names 1,256 vs 1,225. It
  diverges where our budget counts sats inside large extracted block 9
  chunks: block 9 palindromes 45 vs 571, Nakamoto palindromes 1,500 vs
  2,022; and in the off-by-one rows.
* `totalHolders` counts addresses, so P2PK coinbase outputs (no address)
  are missing: vintage uncommon 76 vs our 980 holders, perfect palinception
  2,723 vs 9,892; for modern rows it is close (pizza palindromes 14,242 vs
  14,241). Uncommon 222,571 vs 256,840.
* `inscribed` has no counterpart yet (API.md `RowStats.inscribed`).

## 9. Reproduction

On gp, `~/work/magisat/`:

* `sales.jsonl` (113 MB, 37,749 lines, sha256 `1b4a8704…f22d`), also in the
  session scratchpad `magisat/sales.jsonl`; raw per-tag pages in `dl/raw/`,
  resumable state in `dl/state.json`; `tags.json` (catalog at download time).
* `magisat_dl.py` (download, ≤ 1 request/s, backoff; `--merge` dedupes),
  `shapes.py` (decode every buyer tx via `getrawtransaction 2`, cached in
  `txcache.jsonl.gz`), `analyze_all.py` → `analysis.json` (rules a–f),
  `stratify.py` → `strata.json`, `movescan.py` + `an_fp.py` → `fp.json`
  (§6), `satcheck.py` + `tagstats.py` (main sat vs `/sat`), `palicheck2.py`,
  `fifo_check.py`, `build_tags.py`, `build_summary.py`.
* Signatures are not stored (the API already redacts them).
