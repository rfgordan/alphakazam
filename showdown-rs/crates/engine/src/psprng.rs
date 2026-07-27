//! Bit-exact port of Pokémon Showdown's battle PRNG at pin b9dc987d.
//!
//! # Which generator?
//! `sim/prng.ts`'s [`PRNG`] class dispatches on the seed *string*: seeds beginning
//! `sodium,` use ChaCha20 (libsodium-compatible), `gen5,` or a leading digit use the
//! Gen-5 on-cartridge LCG. The cosim recorder (`harness/cosim.mjs`) constructs every
//! battle with `seed: [SEED_NUM, SEED_NUM+7, SEED_NUM+13, SEED_NUM+29]` — a 4-number
//! array. `PRNG`'s constructor `join(',')`s the array to `"1,8,14,30"`, whose first
//! char is a digit, so `setSeed` takes the **Gen5RNG** branch. Battle seeds are therefore
//! the Gen-5 64-bit LCG over a `[4×u16]` state — this is the generator we port. (Sodium is
//! only the default when *no* seed is supplied, which the harness never does.)
//!
//! # The LCG
//! State is a 64-bit integer `x`. Advance: `x ← (a·x + c) mod 2^64` with
//! `a = 0x5D588B656C078965`, `c = 0x00269EC3`. PS stores the state as four big-endian
//! `u16` limbs (`Gen5RNGSeed = [hi, .., lo]`) and computes `a·x+c` via 16-bit long
//! multiplication (`multiplyAdd`); that is exactly `wrapping_mul`/`wrapping_add` on `u64`.
//! `next()` returns the **upper 32 bits**: PS computes `(seed[0] << 16) + seed[1]`, which
//! is bits 63..32 of the post-advance state.
//!
//! # Draw-consumption contract (must match PS exactly)
//! - `random()`  → one `next()`, float in [0,1).            (unused by battle mechanics)
//! - `random(n)` → one `next()`, int in [0,n).
//! - `random(m,n)` → one `next()`, int in [m,n).
//! - `randomChance(n,d)` = `random(d) < n` → **one** `next()`.
//! - `sample(items)` = `random(len)` → **one** `next()`.
//! - `shuffle(items,start,end)` → Fisher-Yates: **one** `next()` per `start` in
//!   `[start, end-1)` (i.e. `end-1-start` draws), via `random(start,end)`.
//!
//! JS truthiness quirks in `random` are preserved: `if (from)` / `if (to)` treat a **0**
//! argument as absent (`random(0, n)` behaves like `random(n)` shifted, `random(m, 0)`
//! like `random(m)`), and rounding uses `Math.floor` on an f64 product.

/// LCG multiplier `a = 0x5D588B656C078965`.
const A: u64 = 0x5D58_8B65_6C07_8965;
/// LCG increment `c = 0x00269EC3`.
const C: u64 = 0x0000_0000_0026_9EC3;

/// Bit-exact Gen-5 battle PRNG.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PsPrng {
    seed: u64,
}

impl PsPrng {
    /// Build from PS's 4×u16 big-endian seed limbs (`[hi, .., lo]`).
    pub fn from_limbs(limbs: [u16; 4]) -> Self {
        let seed = ((limbs[0] as u64) << 48)
            | ((limbs[1] as u64) << 32)
            | ((limbs[2] as u64) << 16)
            | (limbs[3] as u64);
        PsPrng { seed }
    }

    /// Build from the raw 64-bit state directly.
    pub fn from_u64(seed: u64) -> Self {
        PsPrng { seed }
    }

    /// Current state as four big-endian u16 limbs (matches PS `getSeed()` limbs).
    pub fn limbs(&self) -> [u16; 4] {
        [
            (self.seed >> 48) as u16,
            (self.seed >> 32) as u16,
            (self.seed >> 16) as u16,
            self.seed as u16,
        ]
    }

    /// Advance the LCG and return PS's 32-bit output (upper 32 bits of the new state).
    pub fn next(&mut self) -> u32 {
        self.seed = self.seed.wrapping_mul(A).wrapping_add(C);
        (self.seed >> 32) as u32
    }

    /// `PRNG.random()` — real number in [0,1). One draw. (Not used by battle mechanics.)
    pub fn random_float(&mut self) -> f64 {
        let result = self.next() as f64;
        result / 4_294_967_296.0 // 2^32
    }

    /// `PRNG.random(n)` — integer in [0, n). One draw.
    /// Mirrors JS: `n` is `Math.floor`ed; if `n` is 0 (falsy) PS returns a float — callers
    /// never do this, so we `Math.floor` a 0-scaled product which is 0.
    pub fn random_n(&mut self, n: u32) -> u32 {
        let result = self.next() as f64;
        (result * (n as f64) / 4_294_967_296.0).floor() as u32
    }

    /// `PRNG.random(m, n)` — integer in [m, n). One draw.
    pub fn random_range(&mut self, m: u32, n: u32) -> u32 {
        let result = self.next() as f64;
        (result * ((n - m) as f64) / 4_294_967_296.0).floor() as u32 + m
    }

    /// `PRNG.randomChance(numerator, denominator)` = `random(denominator) < numerator`.
    /// One draw.
    pub fn random_chance(&mut self, numerator: u32, denominator: u32) -> bool {
        self.random_n(denominator) < numerator
    }

    /// `PRNG.sample(items)` — returns the chosen **index** into a length-`len` array. One draw.
    pub fn sample_index(&mut self, len: u32) -> u32 {
        self.random_n(len)
    }

    /// `PRNG.shuffle(items, start, end)` — Fisher-Yates over the slice `[start, end)`.
    /// Mutates `items` in place, consuming one draw per position in `[start, end-1)`.
    pub fn shuffle<T>(&mut self, items: &mut [T], start: usize, end: usize) {
        let mut s = start;
        while s < end.saturating_sub(1) {
            let next_index = self.random_range(s as u32, end as u32) as usize;
            if s != next_index {
                items.swap(s, next_index);
            }
            s += 1;
        }
    }

    /// Convenience: shuffle the whole slice `[0, len)`.
    pub fn shuffle_all<T>(&mut self, items: &mut [T]) {
        let len = items.len();
        self.shuffle(items, 0, len);
    }
}

#[cfg(test)]
mod gate {
    use super::*;
    use std::path::Path;

    // Fixture format (dep-free, pipe-delimited — one call per line):
    //   call | s0,s1,s2,s3 | params | count | fnvhex | out0,..,out15
    // params is `k=v,k=v` (may be empty). `fnvhex` is FNV-1a-64 over the FULL `count`-long
    // output stream (each output folded as 8 LE bytes); the trailing values are the first 16
    // outputs, checked literally for debuggability. This verifies 100k draws per pattern with
    // a tiny fixture. Regenerate with:
    //   node harness/psprng-gate.mjs > crates/engine/tests/psprng_gate.txt

    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

    fn fnv_push(mut h: u64, value: u64) -> u64 {
        let mut v = value;
        for _ in 0..8 {
            let b = v & 0xff;
            h = (h ^ b).wrapping_mul(FNV_PRIME);
            v >>= 8;
        }
        h
    }

    fn parse_seed(s: &str) -> [u16; 4] {
        let mut it = s.split(',').map(|x| x.trim().parse::<u16>().unwrap());
        [
            it.next().unwrap(),
            it.next().unwrap(),
            it.next().unwrap(),
            it.next().unwrap(),
        ]
    }

    fn parse_params(s: &str) -> std::collections::HashMap<String, u32> {
        let mut m = std::collections::HashMap::new();
        for kv in s.split(',') {
            let kv = kv.trim();
            if kv.is_empty() {
                continue;
            }
            let (k, v) = kv.split_once('=').unwrap();
            m.insert(k.to_string(), v.trim().parse::<u32>().unwrap());
        }
        m
    }

    #[test]
    fn raw_gate_matches_ps() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/psprng_gate.txt");
        let text = std::fs::read_to_string(&fixture).unwrap_or_else(|_| {
            panic!(
                "missing gate fixture {}; run `node harness/psprng-gate.mjs > {}`",
                fixture.display(),
                fixture.display()
            )
        });

        let mut cases = 0usize;
        let mut total_draws = 0u64;
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let parts: Vec<&str> = line.split('|').collect();
            assert_eq!(parts.len(), 6, "bad fixture line: {line}");
            let call = parts[0].trim();
            let limbs = parse_seed(parts[1].trim());
            let params = parse_params(parts[2].trim());
            let count: usize = parts[3].trim().parse().unwrap();
            let want_hash = u64::from_str_radix(parts[4].trim(), 16).unwrap();
            let preview: Vec<u64> = parts[5]
                .trim()
                .split(',')
                .filter(|s| !s.is_empty())
                .map(|s| s.parse::<u64>().unwrap())
                .collect();
            let mut prng = PsPrng::from_limbs(limbs);
            let mut h = FNV_OFFSET;

            // Produce the i-th output for this call pattern.
            let check_preview = |i: usize, v: u64| {
                if i < preview.len() {
                    assert_eq!(v, preview[i], "{call} preview[{i}] seed {limbs:?} params {params:?}");
                }
            };

            if call == "shuffle" {
                let len = params["len"] as usize;
                let mut items: Vec<u32> = (0..len as u32).collect();
                prng.shuffle_all(&mut items);
                for (i, &v) in items.iter().enumerate() {
                    h = fnv_push(h, v as u64);
                    check_preview(i, v as u64);
                }
                total_draws += len.saturating_sub(1) as u64;
            } else {
                for i in 0..count {
                    let v: u64 = match call {
                        "next" => prng.next() as u64,
                        "random_n" => prng.random_n(params["n"]) as u64,
                        "random_range" => prng.random_range(params["m"], params["n"]) as u64,
                        "random_chance" => prng.random_chance(params["num"], params["den"]) as u64,
                        "sample" => prng.sample_index(params["len"]) as u64,
                        other => panic!("unknown call {other}"),
                    };
                    h = fnv_push(h, v);
                    check_preview(i, v);
                }
                total_draws += count as u64;
            }
            assert_eq!(h, want_hash, "{call} FNV mismatch seed {limbs:?} params {params:?}");
            cases += 1;
        }
        assert!(cases > 0, "no gate cases loaded");
        eprintln!("psprng raw gate: {cases} cases / {total_draws} draws verified bit-identical");
    }

    #[test]
    fn limb_roundtrip() {
        let p = PsPrng::from_limbs([1, 8, 14, 30]);
        assert_eq!(p.limbs(), [1, 8, 14, 30]);
        assert_eq!(PsPrng::from_u64(p.seed).limbs(), [1, 8, 14, 30]);
    }
}
