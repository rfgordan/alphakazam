# gen9randombattle team pool

Reproducible sample of the **pinned** Pokémon Showdown (`b9dc987d`) `gen9randombattle` team
generator, used to draw real training matchups instead of a single fixed matchup. We do **not**
reimplement PS's generator — we drive it (`Teams.generate('gen9randombattle', {seed})`), so the
pool is distribution-exact by construction and later doubles as an exact belief-prior label source.

## Files

| file | tracked | description |
|------|:---:|-------------|
| `gen9randombattle-2k.jsonl.gz` | ✅ | 2 000-team sample (kept light for the repo; used by the loader test) |
| `gen9randombattle-100k.jsonl.gz` | ❌ (gitignored) | full 100 000-team pool; regenerate locally |
| `stats.json` | ✅ | species frequency + per-species move marginals (P(move \| species)) over the full 100k pool, for belief priors |

## Format

Gzipped JSONL, one team per line: `{"team": [ <6 members> ]}`. Each member is in PS `toID` form,
matching the engine loader (`pybridge::team_from_pool_line`):

```json
{
  "species": "vikavolt", "level": 83,
  "ability": "levitate", "item": "heavydutyboots",
  "nature": "serious", "tera": "electric", "gender": "M",
  "evs": [85, 0, 85, 85, 85, 85],
  "ivs": [31, 0, 31, 31, 31, 31],
  "moves": ["stickyweb", "discharge", "bugbuzz", "voltswitch"]
}
```

Notes:
- `evs`/`ivs` are `[hp, atk, def, spa, spd, spe]` (engine `StatIndex` order). Special attackers
  ship a 0 Atk IV, so IVs are honoured per-stat (not assumed 31).
- gen9 randombattle never assigns a nature → recorded as neutral `serious`.
- `item` is `""` for the rare itemless set (e.g. Acrobatics users) → `Item::None`.
- `gender` is recorded for completeness; the engine has no gender field and ignores it.

## Regeneration

Run from `showdown-rs/harness` (the PS clone is resolved relative to the script):

```sh
# Full 100k pool (also (re)writes stats.json over the full pool):
node gen-team-pool.mjs 100000 team-pool/gen9randombattle-100k.jsonl.gz

# Committed 2k sample (regenerate stats.json from 100k afterwards if you run this last):
node gen-team-pool.mjs 2000 team-pool/gen9randombattle-2k.jsonl.gz
```

Team `i` uses PRNG seed derived deterministically from `i`, so regeneration is byte-identical.

## Consumption

`FlowVec` / `BattleVec` in `crates/pybridge` take an optional `team_pool=<path to .jsonl.gz>`; when
set, every env draws two random pool teams per reset (seeded per-env). `pool_size` reports the
loaded team count. Unknown ids are **loud** errors (no silent defaults).
