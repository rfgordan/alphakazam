# Ruleset spec — making format rules configurable (target: TRUE `[Gen 9] Random Battle`)

**Status:** IMPLEMENTED, 2026-07-27. See `DRAW_EXACT_SCOREBOARD.md`'s RULESET TRANCHE section
for what landed and how it is gated. Two corrections to this document, both load-bearing:

> **§9's worked example is WRONG.** "504 → +6 2016 → Scarf 3024 → Tailwind 6048 → Swift Swim
> 12096 → wraps to 3904" skips the `stat > 10000` cap at `sim/pokemon.ts:638`, which carries the
> SAME `!this.battle.format.battle?.trunc` guard as the truncation and therefore fires in exactly
> the formats that truncate. 12096 caps to 10000, then truncs to **1808**. The reachable
> action-speed range is `[0, 8191]` for raw ≤ 8191, `[0, 1808]` for raw in `[8192, 10000]`, and
> the single value 1808 above that — **`(1808, 8191]` is unreachable by wrapping**, and the
> practical wrap window is 1809 wide, not "everything past 8192".

> **§11.5 step 3's re-stamping is NOT what was done.** Re-stamping 801 gzipped fixtures is a large
> binary diff with a silent failure mode. Instead the trace and fixture gained an explicit,
> optional `ruleset` field naming the formatid handed to `new Battle`, and `ruleset_for` treats
> its ABSENCE as `gen9customgame`. Zero churn, and the two stamps cannot disagree.

**Ground truth:** pinned PS commit `b9dc987d344635789116ae46c48f8e2480e0ddc2` (`showdown-rs/ps.lock`,
date 2026-06-03). All `file:line` citations below are relative to the PS clone root and refer to the
TypeScript sources (`config/`, `data/`, `sim/`), never `dist/`.

> **Clone note (blocker hit while writing this):** `engines/pokemon-showdown` in this checkout is a
> self-referential symlink (`engines -> /Users/…/deep-showdown/engines`) created 2026-07-27 14:00, so
> the pinned clone is **not on disk**. Every worktree's `engines` symlink points at that same broken
> path. This spec was written against a fresh `--depth 1` fetch of the exact pin into a scratchpad.
> Restore the clone (`git clone` + `git checkout b9dc987d`, then `node build` for the harness) before
> any recording work; `harness/check-ps-pin.mjs` will fail until then.

---

## 0. The format definition, verbatim

`config/formats.ts:28-35`:

```ts
	{
		name: "[Gen 9] Random Battle",
		desc: `Randomized teams of Pok&eacute;mon with sets that are generated to be competitively viable.`,
		mod: 'gen9',
		team: 'random',
		bestOfDefault: true,
		ruleset: ['PotD', 'Obtainable', 'Species Clause', 'HP Percentage Mod', 'Cancel Mod', 'Sleep Clause Mod', 'Illusion Level Mod'],
	},
```

There is **no `banlist`**, **no `Team Preview`**, and — contrary to the task brief's assumption —
**no `Endless Battle Clause`** (§4). `bestOfDefault: true` is a server/tournament default (Bo3
challenge default), not a sim rule.

For comparison, the format our whole corpus was recorded under, `config/formats.ts:148-156`:

```ts
	{
		name: "[Gen 9] Custom Game",
		mod: 'gen9',
		searchShow: false,
		debug: true,
		battle: { trunc: Math.trunc },
		// no restrictions, for serious (other than team preview)
		ruleset: ['Team Preview', 'Cancel Mod', 'Max Team Size = 24', 'Max Move Count = 24', 'Max Level = 9999', 'Default Level = 100'],
	},
```

`debug: true` and `battle: { trunc: Math.trunc }` are **not ruleset entries** but they are
format-driven behaviour, and both differ from randbats. They are the two highest-risk findings in
this document — see §9 and §10.

### Resolved RuleTable for `gen9randombattle`

`getRuleTable` (`sim/dex-formats.ts:738-991`) walks `format.ruleset` in order, `set()`s each rule id,
then splices in that rule's own sub-RuleTable. Insertion order (this order is load-bearing: it is the
order `onBegin` handlers fire and the order pseudo-weathers are registered):

| # | rule id | effectType | source |
|---|---------|-----------|--------|
| 1 | `potd` | Rule | ruleset[0] |
| 2 | `obtainable` | ValidatorRule | ruleset[1] |
| 3 | `obtainablemoves` | ValidatorRule | from Obtainable (`data/rulesets.ts:219`) |
| 4 | `obtainableabilities` | ValidatorRule | from Obtainable (`:225`) |
| 5 | `obtainableformes` | ValidatorRule | from Obtainable (`:231`) |
| 6 | `evlimit` (= `Auto`) | ValidatorRule | from Obtainable (`:2076`) |
| 7 | `obtainablemisc` | ValidatorRule | from Obtainable (`:237`) |
| 8 | `-pokemontag:unreleased`, `-pokemontag:unobtainable`, `-nonexistent` | bans | Obtainable banlist |
| 9 | `speciesclause` | ValidatorRule | ruleset[2] |
| 10 | `hppercentagemod` | Rule | ruleset[3] |
| 11 | `cancelmod` | Rule | ruleset[4] |
| 12 | `sleepclausemod` | Rule | ruleset[5] (+ complexBan `Hypnosis + Gengarite`) |
| 13 | `illusionlevelmod` | Rule | ruleset[6] |
| 14 | tag rules appended by `ruleTable.getTagRules()` (`:966`) | | |

`megarayquazaclause` is **not** added: the implicit-add guard at `sim/dex-formats.ts:969` requires
`gen <= 7 || ruleTable.has('+pokemontag:past')`, and randbats is gen 9 without `+Past`.

`resolveNumbers` (`sim/dex-formats.ts:207-...`) then yields, for randbats: `minTeamSize 0`,
`maxTeamSize 6`, `pickedTeamSize null`, `maxMoveCount 4`, `minLevel 1`, `maxLevel 100`,
`defaultLevel 0`, `adjustLevel null`, `evLimit` from `EV Limit = Auto`, `minSourceGen 9`
(gen ≥ 9 + `obtainable` + no natdex). Customgame differs: `maxTeamSize 24`, `maxMoveCount 24`,
`maxLevel 9999`, `defaultLevel 100`, `pickedTeamSize null`.

### How a ruleset entry becomes an event handler

Two distinct mechanisms:

**(a) Lifecycle hooks — called directly, in RuleTable order, never via the event system:**

- `onBegin` — `sim/battle.ts:1941-1946`, inside `start()`, after `|gen|`/`|tier|`/`|rated|`:
  ```ts
  format.onBegin?.call(this);
  for (const rule of this.ruleTable.keys()) {
      if ('+*-!'.includes(rule.charAt(0))) continue;
      const subFormat = this.dex.formats.get(rule);
      subFormat.onBegin?.call(this);
  }
  ```
- `onTeamPreview` — `sim/battle.ts:1975-1982` (`runPickTeam`), same loop shape.
- `onBattleStart` — `sim/battle.ts:2703-2708`, inside the `'start'` queue action, after
  `|teamsize|`/`|start|` and the per-species `BattleStart` singleEvents, before the leads switch in.
- `onValidateRule` — `sim/dex-formats.ts:978-986`, at RuleTable build time.
- `onValidateTeam` / `onChangeSet` / `onValidateSet` — team validator only; the sim never calls them.

**(b) Everything else — registered as a FIELD PSEUDO-WEATHER at construction time.**
`sim/battle.ts:295-308`, in the `Battle` constructor, immediately after `this.add('gametype', …)` and
**before** any `setPlayer` call:

```ts
// timing is early enough to hook into ModifySpecies event
for (const rule of this.ruleTable.keys()) {
    if ('+*-!'.includes(rule.charAt(0))) continue;
    const subFormat = this.dex.formats.get(rule);
    if (subFormat.exists) {
        const hasEventHandler = Object.keys(subFormat).some(
            // skip event handlers that are handled elsewhere
            val => val.startsWith('on') && ![
                'onBegin', 'onTeamPreview', 'onBattleStart', 'onValidateRule', 'onValidateTeam', 'onChangeSet', 'onValidateSet',
            ].includes(val)
        );
        if (hasEventHandler) this.field.addPseudoWeather(rule);
    }
}
```

`dex.conditions.getByID(id)` short-circuits to the Format object when the id is in `data.Rulesets`
(`sim/dex-conditions.ts:685-689`), so the pseudo-weather's "condition" **is** the rule, with
`effectType: 'Rule'`.

**For `gen9randombattle`, exactly one rule qualifies: `sleepclausemod`.** Verified by inspection of
each rule in the table: `potd`/`speciesclause`/`hppercentagemod`/`cancelmod`/`illusionlevelmod` have
only `onBegin`; `obtainable` has only `onValidateTeam`; `obtainablemoves`/`obtainableabilities`/
`obtainableformes`/`evlimit` have no `on*` at all; `obtainablemisc` has only `onChangeSet`. So
`field.pseudoWeather` for a randbats battle is exactly `{ sleepclausemod: {…} }` from turn 0.

`addPseudoWeather` (`sim/field.ts:186-216`) fires `singleEvent('FieldStart')` (no handler → no
output, returns truthy) and `runEvent('PseudoWeatherChange', null, null, status)`. At construction
time `this.sides` is still `[null, null]` and no rule defines `onPseudoWeatherChange`, so this
resolves to an empty handler list: **no protocol output and no PRNG draw.** It does consume nothing
from `battle.effectOrder` either — `initEffectState` (`sim/battle.ts:3343-3353`) only increments
`effectOrder` when the state has both an `id` **and** a `target`, and `addPseudoWeather` passes no
`target`. So the pseudo-weather registration is completely inert except for handler discovery.

---

## 1. `Sleep Clause Mod`

`data/rulesets.ts:1378-1401`, verbatim:

```ts
	sleepclausemod: {
		effectType: 'Rule',
		name: 'Sleep Clause Mod',
		desc: "Prevents players from putting more than one of their opponent's Pok&eacute;mon to sleep at a time, and bans Mega Gengar from using Hypnosis",
		banlist: ['Hypnosis + Gengarite'],
		onBegin() {
			this.add('rule', 'Sleep Clause Mod: Limit one foe put to sleep');
		},
		onSetStatus(status, target, source) {
			if (source?.isAlly(target)) {
				return;
			}
			if (status.id === 'slp') {
				for (const pokemon of target.side.pokemon) {
					if (pokemon.hp && pokemon.status === 'slp') {
						if (!pokemon.statusState.source?.isAlly(pokemon)) {
							this.add('-message', 'Sleep Clause Mod activated.');
							this.hint("Sleep Clause Mod prevents players from putting more than one of their opponent's Pokémon to sleep at a time");
							return false;
						}
					}
				}
			}
		},
	},
```

### Exact failure semantics

- **Event:** `SetStatus`, run from `Pokemon#setStatus` (`sim/pokemon.ts:1722-1728`):
  ```ts
  if (status.id) {
      const result: boolean = this.battle.runEvent('SetStatus', this, source, sourceEffect, status);
      if (!result) {
          this.battle.debug('set status [' + status.id + '] interrupted');
          return result;
      }
  }
  ```
  It runs **after** the "already has this status" short-circuit (`:1698-1706`) and **after**
  `runStatusImmunity` (`:1709-1719`), and **before** `statusState` is created and before the status
  condition's `onStart`.
- **Guard 1 — `source?.isAlly(target)` returns early.** Self-inflicted sleep (Rest — source is the
  user itself) and ally-inflicted sleep are never blocked. This is the Rest *inflicting* exclusion.
- **Guard 2 — "what counts as asleep":** iterate `target.side.pokemon` (**the whole party, not just
  actives**), require `pokemon.hp` (alive; fainted sleepers don't count) and
  `pokemon.status === 'slp'`, and then require `!pokemon.statusState.source?.isAlly(pokemon)` — i.e.
  the existing sleeper must have been slept by a **foe**. A party member asleep from its own Rest
  does **not** occupy the clause slot. This is the Rest *counting* exclusion, and it is why the flag
  must be stored per-Pokemon (our `Pokemon.slept_by_foe` already does this).
- **The target itself is included in the scan** — but it cannot already be `slp` (the earlier
  same-status short-circuit at `:1698` would have returned). Irrelevant in practice.
- **Message emission, on every activation:**
  ```
  |-message|Sleep Clause Mod activated.
  |-hint|Sleep Clause Mod prevents players from putting more than one of their opponent's Pokémon to sleep at a time
  ```
  Note `hint()` (`sim/battle.ts:3092-3102`) is called **without** the `once` argument, so the hint is
  never added to `battle.hints` and is re-emitted on **every** block, not once per battle. (Contrast
  the Illusion Level Mod hint, §3, which passes `once = true`.)
- **No `|-fail|`.** `moveHit` (`sim/battle-actions.ts:1244-1252`):
  ```ts
  if (moveData.status) {
      hitResult = target.trySetStatus(moveData.status, source, moveData.ability ? moveData.ability : move);
      if (!hitResult && move.status) {
          damage[i] = this.combineResults(damage[i], false);
          didAnything = this.combineResults(didAnything, null);
          continue;
      }
      …
  ```
  `didAnything` becomes `null`, not `false`, and the tail block at `:1323-1330` only emits
  `|-fail|` when `didAnything === false`. So a Sleep-Clause-blocked Spore prints the move line, the
  two clause lines, and nothing else.

### Ordering among `SetStatus` handlers

`resolvePriority` (`sim/battle.ts:950-999`) assigns `subOrder` from `effectTypeOrder`
(`Condition: 2, Weather: 5, Format: 5, Rule: 5, Ruleset: 5, Ability: 7, Item: 8`), overridden for
`Condition` by the state's holder (`Side` → 4 / slot 3, `Field` → 5). `comparePriority`
(`sim/battle.ts:403-411`) sorts: order asc → priority desc → **speed desc** → subOrder asc →
effectOrder asc.

The full gen-9 `onSetStatus` inventory and where Sleep Clause Mod sits:

| handler | effectType | holder | speed | subOrder |
|---|---|---|---|---|
| 12 abilities (Immunity, Insomnia, Vital Spirit, Comatose, Leaf Guard, Purifying Salt, Shields Down, Thermal Exchange, Water Veil/Bubble, Sweet Veil, Flower Veil …) — `data/abilities.ts:582, 2058, 2130, 2247, 2330, 3162, 3574, 4221, 4959, 5279, 5363, 5393` | Ability | Pokemon | `pokemon.speed` | 7 |
| Electric Terrain (`data/moves.ts:4516`), Misty Terrain (`:12176`) | Condition | Field (`terrainState`, **no `target` key**) | 0 | **2** |
| Safeguard (`data/moves.ts:15600`) | Condition | Side (`target: this` at `sim/side.ts:409-415`) | 0 | 4 |
| **Sleep Clause Mod** (`data/rulesets.ts:1386`) | **Rule** | Field pseudoWeather | 0 | **5** |
| Stadium Sleep Clause (`:1410`), Sleep Clause (`:1458`), Freeze Clause Mod (`:1479`) | Rule | — | — | not in this format |

So: every ability handler runs first (speed > 0 beats speed 0), then the terrains (subOrder 2), then
Safeguard (4), then **Sleep Clause Mod last (5)**. That matters: Safeguard's own `false` return
pre-empts the clause, so a Safeguard block never emits the clause message; and Misty Terrain's block
likewise short-circuits before it.

### PRNG interaction — the important part

- **Blocking suppresses the sleep-duration draw.** `data/conditions.ts:47-59`:
  ```ts
  	slp: {
  		…
  		onStart(target, source, sourceEffect) { … this.effectState.startTime = this.random(2, 5); … }
  ```
  `onStart` runs at `sim/pokemon.ts:1738` — **after** the `SetStatus` event. A blocked sleep therefore
  consumes **zero** draws at the status site, whereas the same battle under customgame consumes one
  `random(2,5)`. This is the single biggest draw-shape difference between the two formats.
- **The move still pays for everything before `moveHit`.** `moveSteps`
  (`sim/battle-actions.ts:552-572`): TryHit → TypeImmunity → TryImmunity → **Accuracy (step 4)** →
  BreakProtect → StealBoosts → **MoveHitLoop (step 7)**. So a blocked Hypnosis still consumes its
  accuracy `randomChance(60,100)`; a blocked secondary-chance sleep (e.g. Relic Song) still consumes
  the `random(100)` at `sim/battle-actions.ts:1361`; a blocked `sample()`-selected secondary (Tri
  Attack, Dire Claw) still consumes the sample draw.
- **The extra handler does NOT add a speed-tie shuffle here.** `runEvent`'s `speedSort`
  (`sim/battle.ts:794`, sorter at `:428-459`) calls `this.prng.shuffle(list, start, end)`
  (`sim/prng.ts:150-158`, one `random(start,end)` per element past the first) for every tie group of
  size ≥ 2. Sleep Clause Mod's tuple is `(order ∞, priority 0, speed 0, subOrder 5, effectOrder 0)`
  and in `gen9randombattle` **nothing else in a `SetStatus` handler list shares it**: the format
  itself has no `onSetStatus` (subOrder 5, Format), there is no second Rule pseudo-weather, and no
  weather condition defines `onSetStatus`. So adding the clause is draw-neutral apart from the
  suppressed duration roll. **Two edge cases to keep in mind:**
  1. If a Pokemon's `getActionSpeed()` returns exactly 0 it ties with the field handlers. That can
     happen because `Pokemon#getActionSpeed` ends with `return this.battle.trunc(speed, 13)`
     (`sim/pokemon.ts:649`) — speed exactly 8192 truncates to 0. See §10.
  2. Any future rule we add with an `on*` handler that collides at subOrder 5 (e.g. Freeze Clause
     Mod + Sleep Clause Mod in the same format) **would** introduce a shuffle. Assert against it.

---

## 2. `HP Percentage Mod` — observation-only, and a no-op in gen 9

`data/rulesets.ts:1352-1360`:

```ts
	hppercentagemod: {
		effectType: 'Rule',
		name: 'HP Percentage Mod',
		desc: "Shows the HP of Pok&eacute;mon in percentages",
		onBegin() {
			this.add('rule', 'HP Percentage Mod: HP is shown in percentages');
			this.reportPercentages = true;
		},
	},
```

The only consumer is `Pokemon#getHealth` (`sim/pokemon.ts:2060-2100`):

```ts
	getHealth = () => {
		if (!this.hp) return { side: this.side.id, secret: '0 fnt', shared: '0 fnt' };
		let secret = `${this.hp}/${this.maxhp}`;
		let shared;
		if (this.battle.reportExactHP) {
			shared = secret;
		} else if (this.battle.dex.currentMod === 'champions') {
			…
		} else if (this.battle.reportPercentages || this.battle.gen >= 7) {
			// HP Percentage Mod mechanics
			let percentage = Math.ceil(100 * this.hp / this.maxhp);
			if (percentage === 100 && this.hp < this.maxhp) {
				percentage = 99;
			}
			shared = `${percentage}/100`;
		} else { /* 48-pixel form */ }
		if (this.status) { secret += ` ${this.status}`; … }
```

Findings:

1. **`reportPercentages || gen >= 7`** — in gen 9 the percentage branch is taken whether or not the
   rule is present. **`HP Percentage Mod` is mechanically and observationally inert in gen 9 except
   for its `|rule|` line.** The thing that actually made our customgame corpus show exact HP is
   `format.debug: true` → `this.reportExactHP = !!format.debug` (`sim/battle.ts:225`), which takes
   the earlier branch.
2. **Rounding:** `ceil(100·hp/maxhp)`, then **clamped down to 99 whenever `hp < maxhp`**. Our
   `hp_frac` (`crates/engine/src/protocol.rs:296-308`) implements the ceil but **not** the 99 clamp —
   it does `.clamp(1, 100)`. That is a live bug the moment we emit percent HP: e.g. `403/404` →
   PS `99/100`, ours `100/100`. Reachable for any `maxhp > 100` at `hp = maxhp - 1`.
3. **`secret` vs `shared`:** `getHealth` returns both; `Battle.addSplit`/the protocol splitter sends
   `secret` to the owner and `shared` to everyone else. Affected lines: `|switch|`/`|drag|`
   (via `getFullDetails`, `sim/battle-actions.ts:146-148`), `|-damage|`, `|-heal|`, `|-sethp|`, and
   `side.getRequestData()`'s per-Pokemon `condition` field, which uses **`getHealth().secret`**
   (`sim/pokemon.ts:1158`) — i.e. **the request JSON always carries exact HP for your own team**,
   regardless of this rule. Only the spectator/foe-visible stream is quantized.
4. **No mechanics.** `reportPercentages` has exactly one reader (`getHealth`). Confirmed
   observation-layer.

---

## 3. `Illusion Level Mod` — observation-only

`data/rulesets.ts:2916-2925`:

```ts
	illusionlevelmod: {
		effectType: 'Rule',
		name: "Illusion Level Mod",
		desc: `Changes the Illusion ability to disguise the Pok&eacute;mon's level instead of leaking it.`,
		onBegin() {
			this.add('rule', "Illusion Level Mod: Illusion disguises the Pokémon's true level");
		},
		// Implemented in Pokemon#getDetails
	},
```

Exactly two readers of `ruleTable.has('illusionlevelmod')` in the whole tree:

1. `sim/pokemon.ts:545-554` — `getFullDetails`:
   ```ts
   	getFullDetails = () => {
   		const health = this.getHealth();
   		let details = this.details;
   		if (this.illusion) {
   			details = this.illusion.getUpdatedDetails(
   				this.battle.ruleTable.has('illusionlevelmod') ? this.illusion.level : this.level
   			);
   		}
   		if (this.terastallized) details += `, tera:${this.terastallized}`;
   		return { side: health.side, secret: `${details}|${health.secret}`, shared: `${details}|${health.shared}` };
   	};
   ```
   Without the mod the disguised Pokemon shows the **disguiser's real level** (leaking that it is not
   who it claims to be); with the mod it shows the **copied Pokemon's level**. `getUpdatedDetails`
   (`:539-544`) renders `Name` + (`, L{level}` iff level ≠ 100) + gender + shiny. Since randbats
   levels are per-set and frequently ≠ 100, this changes `|switch|`/`|drag|`/`|replace|` details
   strings in real games.
2. `data/abilities.ts:2032-2043` — Illusion `onEnd`, after the `|replace|`/`|-end|` pair:
   ```ts
   				if (this.ruleTable.has('illusionlevelmod')) {
   					this.hint("Illusion Level Mod is active, so this Pokémon's true level was hidden.", true);
   				}
   ```
   `once = true` here, so this `|-hint|` is emitted **at most once per battle**.

No mechanics, no PRNG. Pure protocol.

---

## 4. `Endless Battle Clause` — NOT in this format (but the turn limit is)

`data/rulesets.ts:1060-1068`:

```ts
	endlessbattleclause: {
		effectType: 'Rule',
		name: 'Endless Battle Clause',
		desc: "Prevents players from forcing a battle which their opponent cannot end except by forfeit",
		// implemented in sim/battle.js, see https://dex.pokemonshowdown.com/articles/battlerules#endlessbattleclause for the specification.
		onBegin() {
			this.add('rule', 'Endless Battle Clause: Forcing endless battles is banned');
		},
	},
```

It is pulled in by `Standard AG` (`data/rulesets.ts:12-18`) → `Standard` → OU/Ubers/etc.
**`[Gen 9] Random Battle` does not include it.** So for our target format the clause never fires and
we do not need staleness tracking for *mechanics*. The bits that DO apply unconditionally are in
`Battle#maybeTriggerEndlessBattleClause` (`sim/battle.ts:1800-1900`), called once per turn from
`nextTurn` (`sim/battle.ts:1770`) **before** `this.add('turn', …)`:

```ts
		if (this.turn <= 100) return;

		// the turn limit is not a part of Endless Battle Clause
		if (this.turn > 1000) {
			this.add('message', `It is turn 1000. You have hit the turn limit!`);
			this.tie();
			return true;
		}
		if (
			(this.turn >= 500 && this.turn % 100 === 0) || // every 100 turns past turn 500,
			(this.turn >= 900 && this.turn % 10 === 0) || // every 10 turns past turn 900,
			this.turn >= 990 // every turn past turn 990
		) {
			const turnsLeft = 1000 - this.turn;
			const turnsLeftText = (turnsLeft === 1 ? `1 turn` : `${turnsLeft} turns`);
			this.add('bigerror', `You will auto-tie if the battle doesn't end in ${turnsLeftText} (on turn 1000).`);
		}

		if (!this.ruleTable.has('endlessbattleclause')) return;
```

So, format-independent: turns 500/600/…/900, then 910/920/…/980, then every turn 990-999 emit a
`|bigerror|`; **turn > 1000 forces `tie()`**. (Also format-independent: the gen ≤ 1 pre-check at
`:1806-1831`, irrelevant to us.)

For completeness, the clause proper (gated at `:1851`, and additionally skipped for `freeforall` at
`:1853`) triggers when **all** of:
- every side is "stale" (`stalenessBySide.every(s => !!s)`) and **at least one** side is
  `'external'` (`:1856`);
- **not** every side can switch to a non-stale, non-fainted Pokemon (`:1859-1872`) — a trapped side
  short-circuits to `canSwitch[i] = false`.

Staleness is computed in `nextTurn` (`sim/battle.ts:1758-1763`) as
`pokemon.volatileStaleness || pokemon.staleness`, `'external'` dominating `'internal'`. Sources:
`Pokemon#eatItem` promotes `pendingStaleness` when a `RESTORATIVE_BERRIES` berry is eaten
(`sim/pokemon.ts:1788-1798`; set = `leppaberry, aguavberry, enigmaberry, figyberry, iapapaberry,
magoberry, sitrusberry, wikiberry, oranberry`, `sim/pokemon.ts:43-45`), plus explicit
`target.staleness = 'external'` at `data/moves.ts:1926, 5756, 5835, 8418, 13460` (Bug Bite/Pluck
line, Trick/Switcheroo, Bestow) and `data/rulesets.ts:2498`.

Forced outcome (`sim/battle.ts:1875-1899`): each side is a "loser" if some member's **set** has a
restorative berry **and** some member's set has Harvest/Pickup/Recycle. One loser → `win(loser.foe)`
with `|-message|{name}'s team started with the rudimentary means to perform restorative
berry-cycling and thus loses.`; all sides losers → an extra `|-message|Each side's team started with
the rudimentary means…` then `tie()`; otherwise plain `tie()`.

**Recommendation:** implement the turn-limit/bigerror path (needed for any format); put the clause
proper behind a flag that stays `false` for both presets and is not implemented yet.

---

## 5. Team Preview: what actually differs

`data/rulesets.ts:634-665` — the `teampreview` rule's `onTeamPreview` emits `|clearpoke|`, one
`|poke|SIDE|DETAILS|` per Pokemon (with `, shiny` stripped and Zacian/Zamazenta/Greninja/Gourgeist/
Pumpkaboo/Xerneas/Silvally/Urshifu/Dudunsparce forme-masked to `-*` unless `speciesrevealclause`),
optionally the Tera-type block, and then `this.makeRequest('teampreview')`.

`Battle#runPickTeam` (`sim/battle.ts:1971-2000`):

```ts
	runPickTeam() {
		this.format.onTeamPreview?.call(this);
		for (const rule of this.ruleTable.keys()) { … subFormat.onTeamPreview?.call(this); }

		if (this.requestState === 'teampreview') {
			return;
		}

		if (this.ruleTable.pickedTeamSize) {
			// There was no onTeamPreview handler (e.g. Team Preview rule missing).
			…
			this.makeRequest('teampreview');
		}
	}
```

For `gen9randombattle`: no rule defines `onTeamPreview`, and `pickedTeamSize` is `null` (§0), so
**`runPickTeam` is a complete no-op**. `start()` then does
`this.queue.addChoice({ choice: 'start' }); this.midTurn = true; if (!this.requestState) this.turnLoop();`
(`sim/battle.ts:1965-1968`).

Concretely, the two flows:

| | `gen9customgame` (our corpus) | `gen9randombattle` (target) |
|---|---|---|
| after `start()` | `|clearpoke|`, 12×`|poke|`, `|teampreview|`, `requestState = 'teampreview'` | nothing |
| first request | `{ teamPreview: true, maxChosenTeamSize: undefined, side: … }` | `{ active: […], side: … }` (move request) |
| first choice | `>p1 team …` / `default` → `Side#chooseTeam` may **reorder `side.pokemon`** | none |
| leads | whatever the team order says after the pick | `side.pokemon[0]` — literally slot 1, generator order |
| then | `'start'` action | `'start'` action |

The `'start'` action itself is identical in both (`sim/battle.ts:2690-2721`): per side
`pokemonLeft = pokemon.length` and `|teamsize|SIDE|N|`, then `|start|`, then a `BattleStart`
singleEvent per Pokemon's species condition, then `format.onBattleStart` + per-rule `onBattleStart`
(none in either format), then `actions.switchIn(side.pokemon[i], i)` for each active slot.

**Init draw differences: none from the format itself.** Specifically:
- Team generation does **not** touch `battle.prng`. `Battle#getTeam` (`sim/battle.ts:3186-3202`)
  returns `options.team` verbatim if supplied; only when it is absent does it build a
  `Teams.getGenerator(this.format, options.seed)` — a **separate** `PRNG` seeded from the *player*
  seed (`sim/teams.ts:628-648`). So we can safely record under the real `gen9randombattle` formatid
  while still passing pre-generated packed teams: the battle stream is untouched.
- The only unlogged construction draws remain the `sample(['M','F'])` gender rolls in `new Pokemon`,
  which are **set-driven, not format-driven** (already modelled by
  `crates/cosim/src/seedgate.rs:171-…` `init_gender_rolls`; randbats sets carry explicit genders → 0).
- The rules-as-pseudo-weather loop draws nothing (§0).
- Team-preview `makeRequest`/`chooseTeam` draw nothing.

---

## 6. `Species Clause`, `Obtainable`, `Cancel Mod`, `PotD`

**`Species Clause`** (`data/rulesets.ts:788-805`) — `effectType: 'ValidatorRule'`. Its `onBegin`
emits `|rule|Species Clause: Limit one of each Pokémon`; its `onValidateTeam` rejects duplicate
`species.num`. The sim never calls `onValidateTeam`. **No-op for us beyond the `|rule|` line**, and
doubly so for generator-produced teams: `data/random-battles/gen9/teams.ts` already enforces
one-per-`baseSpecies` when building a team.

**`Obtainable`** (`data/rulesets.ts:166-...`) — `ValidatorRule` with sub-rules `Obtainable Moves /
Abilities / Formes`, `EV Limit = Auto`, `Obtainable Misc`, banlist `Unreleased, Unobtainable,
Nonexistent`. Its own `onValidateTeam` (Kyurem/Necrozma/Calyrex fusion-count checks) and
`obtainablemisc`'s `onChangeSet` are validator-time only. Randbats teams are generated legal.
**No-op — with ONE exception that is easy to miss:** `obtainableabilities` is read *at battle time*
in `nextTurn`'s trap-inference block (`sim/battle.ts:1741-1752`):
```ts
							const ruleTable = this.ruleTable;
							if ((ruleTable.has('+hackmons') || !ruleTable.has('obtainableabilities')) && !this.format.team) {
								// hackmons format
								continue;
							}
```
In **customgame** (`obtainableabilities` absent **and** `format.team` unset) this `continue` fires
and PS **skips** the whole `FoeMaybeTrapPokemon` sweep over each foe species' possible abilities. In
**randbats** it does not fire — both because `obtainableabilities` is present and because
`format.team === 'random'` — so PS runs `singleEvent('FoeMaybeTrapPokemon', ability, …)` for every
legal ability of every foe's *apparent* species (Arena Trap / Shadow Tag / Magnet Pull), setting
`pokemon.maybeTrapped`. That flag surfaces in the move request. **This is a genuine
customgame→randbats behavioural delta in the request layer that our corpus has never exercised.**
No PRNG (singleEvent, no draws), but request-shape-visible.

**`Cancel Mod`** (`data/rulesets.ts:1370-1377`) — `onBegin() { this.supportCancel = true; }`, no
`|rule|` line. Two readers: `getRequests` (`sim/battle.ts:1455`) sets `requests[i].noCancel = true`
when `!supportCancel || !multipleRequestsExist`; and `sim/battle.ts:3085` sets
`side.choice.cantUndo = true` when `!supportCancel`. Both formats have Cancel Mod, so this is a
wash today; for a bot that never issues `undo` it is inert anyway. Keep it as a flag purely so the
request JSON's `noCancel` field matches.

**`PotD`** (`data/rulesets.ts:483-491`):
```ts
	potd: {
		effectType: 'Rule',
		name: 'PotD',
		desc: "Forces the Pokemon of the Day onto every random team.",
		onBegin() {
			if (global.Config?.potd) {
				this.add('rule', "Pokemon of the Day: " + this.dex.species.get(Config.potd).name);
			}
		},
	},
```
**Default state: off.** `config/config-example.js:175` is `exports.potd = '';` — falsy — so the
`|rule|` line is not emitted and the two generator reads
(`data/random-battles/gen9/teams.ts:1749-1750` and `:1959-1960`,
`const usePotD = global.Config && Config.potd && ruleTable.has('potd')`) are false. **Complete no-op
unless a server operator sets it.** Model it as a flag hard-wired to `false`; if it were ever set it
would change team *generation*, which we do not do.

---

## 7. Other rules / format fields in this format not covered above

- **`bestOfDefault: true`** — server-side only (`Bo3` challenge default). No sim effect.
- **`mod: 'gen9'`, `team: 'random'`** — `team` selects the generator **and** participates in the
  trap-inference guard above (§6). `mod` selects the dex.
- **Complex ban `Hypnosis + Gengarite`** (from Sleep Clause Mod's `banlist`) — validator only, and
  unreachable in gen 9 (no Mega Stones).
- **Tag rules** appended by `ruleTable.getTagRules()` — validator only.
- **`|rule|` lines actually emitted at battle start**, in RuleTable order (this is the exact expected
  prefix; `potd` emits nothing by default, `cancelmod` emits nothing):
  ```
  |rule|Species Clause: Limit one of each Pokémon
  |rule|HP Percentage Mod: HP is shown in percentages
  |rule|Sleep Clause Mod: Limit one foe put to sleep
  |rule|Illusion Level Mod: Illusion disguises the Pokémon's true level
  ```
  `gen9customgame` emits **no** `|rule|` lines at all (Team Preview's `onBegin` only speaks when
  `teratypepreview` is on; the value rules have no `onBegin`).

---

## 8. `format.debug: true` — the hidden customgame delta

`sim/battle.ts:211` `this.debugMode = format.debug || !!options.debug;` and `:225`
`this.reportExactHP = !!format.debug;`. Consequences for our corpus that vanish under randbats:

1. **Exact HP in the shared stream** (§2).
2. **`|debug|…` lines** — `Battle#debug` only appends when `debugMode`; there are ~40 call sites
   (`sim/battle.ts:604, 609, 613, 620, 839, 866, 2114, 2120, 2185, 2754, …`, plus `sim/pokemon.ts`
   and `sim/battle-actions.ts`). Under randbats they all disappear.
3. **`checkEVBalance()`** runs at `sim/battle.ts:1952-1954` only in debug mode; it can emit
   `|bigerror|Warning: One player isn't adhering to a 510 EV limit…`. Gone under randbats.
4. **`forceRandomChance`** (`sim/battle.ts:213`) is only honoured in debug mode — relevant if any
   harness path uses it.

---

## 9. `format.battle: { trunc: Math.trunc }` — a REAL mechanics delta

`sim/battle.ts:201` sets `this.trunc = this.dex.trunc`, where (`sim/dex.ts:360-367`):

```ts
	/**
	 * Truncate a number into an unsigned 32-bit integer, for
	 * compatibility with the cartridge games' math systems.
	 */
	trunc(this: void, num: number, bits = 0) {
		if (bits) return (num >>> 0) % (2 ** bits);
		return num >>> 0;
	}
```

Then `sim/battle.ts:208`: `if (format.battle) Object.assign(this, format.battle);` — which for
**customgame only** replaces it with `Math.trunc`, a function that **ignores the `bits` argument
entirely.** Two call sites take `bits`:

- `sim/battle-actions.ts:1845` `return tr(baseDamage, 16);` and `:1863` `let damage = tr(baseDamage, 16);`
  — damage is reduced mod 65536 in every normal format, **but not in customgame**.
- `sim/pokemon.ts:649` `return this.battle.trunc(speed, 13);` in `getActionSpeed` — effective Speed
  is reduced **mod 8192** in every normal format, **but not in customgame**.

The 13-bit Speed wrap is reachable in real randbats (base 504 → +6 = 2016 → Choice Scarf 3024 →
Tailwind 6048 → Chlorophyll/Swift Swim 12096 → wraps to 3904), and it changes turn order, so it also
changes which branches of the move-order speed tie shuffle happen — i.e. it is **draw-order
relevant**, not just numeric. The 16-bit damage wrap is effectively unreachable at legal levels but
is free to implement.

**Our engine was calibrated entirely on the `Math.trunc` arm.** Audit `damage.rs` /
`getActionSpeed`-equivalent for these two truncations before trusting randbats parity.

---

## 10. Draw-order hazards — consolidated

Flagging these explicitly, highest risk first:

**H1 — the suppressed `random(2,5)`.** A Sleep-Clause-blocked sleep does not roll the duration
(§1). Every downstream draw in the game shifts by one. Our engine already has
`sleep_clause_blocks` (`crates/engine/src/generate.rs:1389-1400`) wired into all five status sites
(`:9019, 9531, 9702, 10766, 12433`) and the `sleep_survived_or_discard_duration` draw-and-discard
machinery for the *cured* path — but the *blocked* path must consume **nothing**, not
draw-and-discard. Verify the distinction; they are different draw shapes.

**H2 — `sleep_clause` has been hard-`false` for the entire corpus.** `trace::sleep_clause_for_format`
(`crates/cosim/src/trace.rs:162-165`) returns `false` for every recorded trace, by construction
(`harness/cosim.mjs:1006-1009` rewrites any `*random*` format to `gen9customgame`). `State::EMPTY`
defaults it to **`true`** (`crates/engine/src/state.rs:416`), so anything that builds a state without
going through the trace path gets the opposite default. Once real randbats traces exist, both the
format→flag mapping and that default need to be re-derived from the trace's `format` field, and the
`sleep_clause_for_format` doc comment (which currently asserts "It never is") becomes wrong.

**H3 — the 13-bit Speed truncation (§9)** changes turn order and therefore tie-shuffle draws.

**H4 — `speedSort` tie groups gain a member if we ever add a second subOrder-5 rule handler.** Today
Sleep Clause Mod is alone at `(∞, 0, 0, 5, 0)` in `SetStatus` lists, so it is shuffle-neutral. A
future Freeze Clause Mod / Stadium Sleep Clause in the same format, or a Pokemon whose
`getActionSpeed` truncates to exactly 0, creates a 2-element tie group → one extra
`prng.shuffle` → one extra `random()`. Add a debug assertion that no `SetStatus` handler list
contains two subOrder-5 entries.

**H5 — first-decision shape.** `replay.rs:57-60`, `seedgate.rs:357-359`, `drawdiff.rs`, and
`protocol_emit.rs` all hard-require `first.request_state == "teampreview"`. Real randbats has no
teampreview decision at all (§5). Every cosim entry point needs a format-aware entry contract, or
the recorder must synthesise a zero-choice "pre-start" decision.

**H6 — `maybeTrapped` in the move request** is computed differently under randbats (§6,
`sim/battle.ts:1741-1752`). Request-only, no draws, but it will show up as a legality/request diff.

**H7 — percent-HP rounding.** The `99` clamp is missing from `hp_frac` (§2). Protocol-only.

**H8 — the `|rule|` prefix and the disappearance of `|debug|`/exact HP** (§7, §8) shift every
protocol-parity baseline recorded under customgame.

---

## 11. Proposed Rust design

### 11.1 A `Ruleset` config on `Battle` init

Add a small, `Copy`, all-`bool`/scalar struct. It is **not** part of `State` (which is `Copy` and
diffed field-by-field against traces) except for the fields that already live there; instead it is a
sibling carried by the battle/branch context. `State.sleep_clause` already exists and is load-bearing
inside `generate.rs`, so the pragmatic move is:

```rust
/// Format-level rules. Constructed once per battle; never mutated after init.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ruleset {
    // ---- core mechanics (affect state transitions and/or the PRNG stream) ----
    /// Sleep Clause Mod. Blocks foe-inflicted `slp` when a party member is already foe-slept,
    /// BEFORE the `random(2,5)` duration roll. data/rulesets.ts:1378.
    pub sleep_clause: bool,
    /// Endless Battle Clause proper (staleness → forced win/tie). NOT in either preset.
    pub endless_battle_clause: bool,
    /// `format.battle.trunc = Math.trunc` (customgame) disables the 13-bit Speed and 16-bit
    /// damage truncations. `true` = PS default `(x >>> 0) % 2^bits`. sim/dex.ts:364.
    pub bit_truncation: bool,
    /// `!ruleTable.has('obtainableabilities') && !format.team` skips the foe-ability
    /// `FoeMaybeTrapPokemon` sweep. sim/battle.ts:1741.
    pub infer_foe_trapping_abilities: bool,

    // ---- observation layer (protocol + request JSON only) ----
    /// `format.debug` → exact HP in the shared stream, `|debug|` lines, `checkEVBalance`.
    pub report_exact_hp: bool,
    pub emit_debug_lines: bool,
    /// Illusion Level Mod: `|switch|` details use the copied mon's level. data/rulesets.ts:2916.
    pub illusion_level_mod: bool,
    /// Cancel Mod → omit `noCancel` from requests when both sides have a request.
    pub cancel_mod: bool,
    /// Team Preview: emit `|clearpoke|`/`|poke|`/`|teampreview|` and issue a teampreview request.
    pub team_preview: bool,
    /// The `|rule|` lines emitted at `start()`, in RuleTable order.
    pub rule_lines: &'static [&'static str],

    // ---- scalars from RuleTable::resolveNumbers ----
    pub max_team_size: u8,     // 6 / 24
    pub max_move_count: u8,    // 4 / 24
    pub picked_team_size: Option<u8>, // None for both presets
}
```

Deliberately **not** modelled (documented no-ops, §6): `PotD` (Config default `''`),
`Species Clause`, `Obtainable` and its sub-rules — except that `obtainableabilities` is folded into
`infer_foe_trapping_abilities`, and Species Clause / HP Percentage Mod survive only as entries in
`rule_lines`. `HP Percentage Mod` gets **no flag of its own**: in gen 9 the percent path is
unconditional (`reportPercentages || gen >= 7`), so the real switch is `report_exact_hp`.

### 11.2 Presets

```rust
impl Ruleset {
    pub const GEN9_RANDOM_BATTLE: Ruleset = Ruleset {
        sleep_clause: true,
        endless_battle_clause: false,
        bit_truncation: true,
        infer_foe_trapping_abilities: true,
        report_exact_hp: false,
        emit_debug_lines: false,
        illusion_level_mod: true,
        cancel_mod: true,
        team_preview: false,
        rule_lines: &[
            "Species Clause: Limit one of each Pokémon",
            "HP Percentage Mod: HP is shown in percentages",
            "Sleep Clause Mod: Limit one foe put to sleep",
            "Illusion Level Mod: Illusion disguises the Pokémon's true level",
        ],
        max_team_size: 6, max_move_count: 4, picked_team_size: None,
    };

    pub const GEN9_CUSTOM_GAME: Ruleset = Ruleset {
        sleep_clause: false,
        endless_battle_clause: false,
        bit_truncation: false,          // format.battle.trunc = Math.trunc
        infer_foe_trapping_abilities: false,
        report_exact_hp: true,          // format.debug
        emit_debug_lines: true,
        illusion_level_mod: false,
        cancel_mod: true,
        team_preview: true,
        rule_lines: &[],
        max_team_size: 24, max_move_count: 24, picked_team_size: None,
    };

    pub fn from_format(id: &str) -> Option<Ruleset> { … }  // replaces trace::sleep_clause_for_format
}
```

`from_format` becomes the single source of truth and **must key off the trace's `format` field
directly** — not off the `*random* → customgame` rewrite that `cosim.mjs` currently performs. That
rewrite is what makes today's mapping correct-but-inverted; it has to be deleted from the recorder at
the same time (§11.5), or the two will disagree.

### 11.3 Where each flag hooks into the existing handler-ordering model

The engine already models PS's `speedSort`/`subOrder` semantics explicitly (see the commentary in
`crates/engine/src/generate.rs:199, 640, 816, 856-859, 911, 2294-2302`). Rules slot in as follows:

- **`sleep_clause`** — already wired: `sleep_clause_blocks(state, side)`
  (`generate.rs:1389-1400`) is consulted at the five status-application sites. Its position in the
  handler chain is **last** among `SetStatus` handlers (subOrder 5, after abilities → terrains →
  Safeguard, §1), which is what the current call sites effectively encode by checking it after the
  immunity/terrain/Safeguard guards. Add a comment recording the derivation so the ordering is not
  re-litigated. The **only** draw effect is the omission of the `random(2,5)`.
- **`bit_truncation`** — a pure numeric switch inside the damage formula and the
  `get_action_speed` equivalent. If `true`, apply `(x as u32) % 8192` / `% 65536`. Because the Speed
  truncation feeds turn order, gate it *before* the move-order comparison, not after.
- **`infer_foe_trapping_abilities`** — request layer (`request.rs`); it only widens
  `maybe_trapped`. No draws.
- **`report_exact_hp` / `emit_debug_lines` / `illusion_level_mod` / `rule_lines`** — protocol layer
  (`protocol.rs`, `narrate.rs`). `HpStyle` already exists; derive it from the ruleset instead of
  passing it in ad hoc, and fix the 99 clamp while you are there.
- **`team_preview`** — recorder/replay entry contract (§11.5), plus a `|clearpoke|`/`|poke|`/
  `|teampreview|` emitter if we ever need customgame protocol parity again.
- **`endless_battle_clause`** — leave unimplemented behind the flag. **Do implement the
  format-independent part** (turn-limit tie at turn > 1000, `|bigerror|` schedule at
  500/600/…/900/910/…/980/990+) unconditionally, since it is not part of the clause.

### 11.4 Core-mechanics vs observation-layer summary

| flag | layer | draws? |
|---|---|---|
| `sleep_clause` | **core** | **yes** — suppresses `random(2,5)` |
| `bit_truncation` | **core** | **yes, indirectly** — Speed wrap → turn order → tie shuffles |
| `endless_battle_clause` | **core** (game end) | no |
| `infer_foe_trapping_abilities` | observation (request) | no |
| `report_exact_hp`, `emit_debug_lines`, `illusion_level_mod`, `rule_lines`, `cancel_mod` | observation (protocol/request) | no |
| `team_preview` | observation + entry contract | no |
| `max_team_size`, `max_move_count`, `picked_team_size` | observation (request shape) | no |

### 11.5 Verification plan

1. **Recorder change.** Delete the rewrite at `harness/cosim.mjs:1006-1009`
   (`formatid: FORMAT.includes('random') ? 'gen9customgame' : FORMAT`) and pass the real `FORMAT`.
   Keep passing explicit packed teams — `Battle#getTeam` returns them verbatim and never touches
   `battle.prng` (§5), so the battle stream stays reproducible from the battle seed alone. Also
   update the doc comments at `crates/cosim/src/trace.rs:148-160` and
   `DRAW_EXACT_SCOREBOARD.md:2150-2160`, which currently assert the clause is never active.
2. **Entry contract.** Teach `replay.rs`, `seedgate.rs`, `drawdiff.rs`, `protocol_emit.rs` that a
   trace whose ruleset has `team_preview == false` starts at a `move` decision (H5). Prefer a shared
   helper over four independent edits.
3. **Corpus format field.** Traces already carry `format` at the top level
   (`harness/TRACE_FORMAT.md:12`), and the slim seed fixtures carry it too
   (`seedgate.rs:114, 139`). Nothing to add — just stop discarding it via the rewrite. Existing
   `rb*.fx.json.gz` fixtures say `gen9randombattle` but were **recorded as customgame**; they must
   be either re-recorded or re-stamped `gen9customgame`, otherwise `Ruleset::from_format` will
   activate the clause on games that were played without it and the gate will go red.
   **Do this before landing the new mapping** — it is the same inversion that bit us before.
4. **New recordings.** A fresh seed range recorded under the true format, sized like the existing
   one, plus a **directed sleep-clause tranche**: teams with two sleep-inducers and multiple sleep
   targets (Spore/Sleep Powder/Hypnosis/Yawn + Relic Song/Dire Claw/Tri Attack secondaries), and a
   Rest user on the receiving side to exercise both Rest exclusions (`source.isAlly` early return and
   the `statusState.source.isAlly` scan skip). Assert the corpus contains at least one
   `|-message|Sleep Clause Mod activated.` and at least one Rest-asleep-party-member-does-not-block
   case.
5. **Gates.** The existing seed gate + differ must hold on the new corpus with **zero** regressions
   on the retained customgame corpora (which now run with `Ruleset::GEN9_CUSTOM_GAME`). The draw
   differ is the acceptance instrument for H1/H3: a blocked sleep must show **no** draw at the status
   site, and a >8192 Speed must show the wrapped ordering.
6. **Protocol parity.** Re-baseline `harness/protocol-parity.mjs` (currently pinned to
   `gen9customgame` at `:148`) for the randbats preset: `|rule|` prefix present, no `|debug|` lines,
   percent HP with the 99 clamp, illusion details using the copied level.
7. **Assertions.** (a) no two subOrder-5 handlers in a `SetStatus` list (H4); (b)
   `Ruleset::from_format` is total over the formats appearing in the corpus and errors loudly
   otherwise, instead of silently defaulting.
