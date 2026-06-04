# Battle trace format (v1)

A *trace* is one fully-recorded, deterministic battle sampled from the real Pokémon
Showdown simulator. It is the ground truth the Rust engine is verified against.

A trace is reproducible: given the same `seed`, `format`, teams, and the recorded
`choices`, replaying it in PS yields byte-identical states. The Rust engine replays the
same start state + choices and must match every per-turn snapshot.

```jsonc
{
  "format": "gen9customgame",
  "seed": "sodium,0123...",         // PS PRNG seed; makes the JS battle reproducible
  "players": {
    "p1": { "name": "...", "team": "<packed team string>" },
    "p2": { "name": "...", "team": "<packed team string>" }
  },

  // The full machine state BEFORE any choice is made (after team preview / lead send-out).
  // Same shape as each `snapshots[i].state`. The Rust side parses this into its State.
  "start": { "turn": 1, "state": { /* see State shape below */ } },

  // One entry per decision point, in order. A "turn" here is a request→choice→resolution.
  "snapshots": [
    {
      "turn": 1,
      "choices": { "p1": "move 1", "p2": "switch 2" },  // exact >pN strings replayed
      "outcomes": [                                       // injected random outcomes (v2; see below)
        // { "event": "damageRoll", "value": 11 },
        // { "event": "crit", "value": false },
        // { "event": "hit", "value": true },
        // { "event": "secondary", "value": true }
      ],
      "state": { /* full normalized Battle.toJSON() projected to the State shape below */ }
    }
    // ...
  ],

  "result": { "winner": "p1" | "p2" | "tie" | null }
}
```

## State shape

A normalized projection of `Battle.toJSON()` keeping only fields the Rust engine
models, so the two can be compared directly. `State.normalize()` strips PS's
nondeterministic `|t:|` timestamps first.

```jsonc
{
  "turn": 7,
  "weather": "sand", "weatherTurns": 3,
  "terrain": "none", "terrainTurns": 0,
  "trickRoom": false, "trickRoomTurns": 0,
  "sides": [
    {
      "activeIndex": 0,
      "boosts": { "atk": 0, "def": 0, "spa": 0, "spd": 0, "spe": 0, "accuracy": 0, "evasion": 0 },
      "volatiles": ["substitute", "leechseed"],
      "substituteHp": 75,
      "sideConditions": { "stealthRock": true, "spikes": 1, "toxicSpikes": 0, "stickyWeb": false,
                          "reflect": 0, "lightScreen": 0, "auroraVeil": 0, "tailwind": 0 },
      "pokemon": [
        {
          "species": "greattusk", "level": 100,
          "types": ["ground","fighting"],
          "hp": 230, "maxHp": 330,
          "status": "brn", "statusCounter": 0,
          "ability": "protosynthesis", "item": "boosterenergy",
          "terastallized": false, "teraType": "ground",
          "moves": [ { "id": "earthquake", "pp": 15, "maxPp": 16, "disabled": false }, ... ]
        }
        // ... up to 6
      ]
    }
    // ... 2 sides
  ]
}
```

## Outcome injection (v2)

The `outcomes` array is the list of concrete random results PS produced while resolving
that turn, in consumption order. The Rust engine, run in verification mode, consumes
these instead of rolling its own RNG, so the comparison tests *mechanics* not *RNG
reproduction*. Emitted by a small PS patch (`battle.debug(...)`) at the roll sites:
damage roll (0–15), crit, accuracy hit/miss, secondary proc, speed-tie winner.
Until that patch lands, `outcomes` is empty and the Rust side infers the branch by
matching the resulting HP (sufficient for deterministic moves; the patch makes it exact).
```
