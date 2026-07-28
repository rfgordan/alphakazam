//! Decision-point flow driver: advances a battle between *requests* (PS's request model),
//! sampling one stochastic path per step. This is the training MDP with the real rules:
//! faint replacements are their own decision phase (no free hit), pivot moves pause for a
//! landing choice after their damage resolves (PS timing), and Terastallization is an action
//! property. See DECISION_POINTS.md.
//!
//! Composition strategy: turns where neither chosen move is a self-switch move run through the
//! whole-turn sampled resolver unchanged (zero composition risk — PS issues faint-replacement
//! requests only after residuals). Only pivot turns decompose into first-move / pause /
//! second-move / end-of-turn, using the same crate-internal building blocks the whole-turn
//! resolver itself is made of; `tests/request_flow.rs` pins the composition to the whole-turn
//! distribution on pivot-free turns.

use crate::data::move_data;
use crate::generate::{
    apply_end_of_turn, apply_switch, apply_tera, battle_over, execute_move, generate_instructions_sampled,
    move_order, switch_into_pair, Action, Branch, Exec, MoveChoice, Order, Pivot,
};
use crate::instruction::Instruction;
use crate::state::{SideId, State};

/// One player's answer to a request.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PlayerChoice {
    /// Use move `slot` (0..=3), optionally Terastallizing first.
    Move { slot: u8, tera: bool },
    /// Switch the active to party `slot` (0..=5, not the current active). Also the answer
    /// shape for `Replace` and `PivotLanding` requests.
    Switch { slot: u8 },
}

/// What the battle is waiting on.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Request {
    /// Normal simultaneous turn: both sides choose.
    Turn,
    /// Faint replacement(s): each flagged side chooses a switch. Issued after residuals,
    /// exactly like PS; chained hazard faints re-issue it.
    Replace { sides: [bool; 2] },
    /// A pivot move connected; `side` chooses its landing before the opponent's queued
    /// action executes.
    PivotLanding { side: SideId },
    /// Revival Blessing connected; `side` chooses a FAINTED party member to revive (answered
    /// with `PlayerChoice::Switch { slot }` pointing at the fainted slot). The revived mon stays
    /// benched; the turn then continues before the opponent's queued action executes.
    Revive { side: SideId },
    /// Battle over: -1 draw, 0 side one, 1 side two.
    Terminal { winner: i64 },
}

enum Pending {
    Turn,
    Pivot { side: SideId, second: Option<Action>, switched: [bool; 2], pass_sub: bool },
    Revive { side: SideId, second: Option<Action>, switched: [bool; 2] },
    Replace { sides: [bool; 2] },
    Done { winner: i64 },
}

/// Result of executing one move action in the decomposed (pivot) path: whether its transcript
/// ended in a deferred landing/revive choice, and for a pivot whether the Substitute is passed.
enum Pause {
    None,
    Pivot { side: SideId, pass_sub: bool },
    Revive { side: SideId },
}

/// A battle advanced request-by-request with sampled stochastics.
pub struct Flow {
    pub state: State,
    pub rng: u64,
    pending: Pending,
    /// Optional PS-protocol capture (off = `None` = zero cost on the hot path). When enabled,
    /// each resolved turn appends its `|turn|…|upkeep` protocol block via `engine::protocol`.
    protocol_log: Option<Vec<String>>,
    /// Pre-turn state + move choices stashed for the decomposed (pivot) path, whose full
    /// instruction list is only complete at `finish_turn`.
    turn_pre: State,
    turn_m: [MoveChoice; 2],
}

fn winner_of(state: &State) -> i64 {
    let alive = |sd: SideId| {
        state.side(sd).pokemon.iter().any(|p| p.species != crate::ids::Species::None && p.is_alive())
    };
    match (alive(SideId::One), alive(SideId::Two)) {
        (true, false) => 0,
        (false, true) => 1,
        _ => -1,
    }
}

fn alive_bench(state: &State, side: SideId) -> Option<u8> {
    let s = state.side(side);
    (0..6u8).find(|&i| {
        i != s.active_index
            && s.pokemon[i as usize].species != crate::ids::Species::None
            && s.pokemon[i as usize].is_alive()
    })
}

impl Flow {
    pub fn new(state: State, seed: u64) -> Flow {
        Flow {
            state,
            rng: seed.wrapping_add(1),
            pending: Pending::Turn,
            protocol_log: None,
            turn_pre: state,
            turn_m: [MoveChoice::Move(0); 2],
        }
    }

    /// Turn PS-protocol capture on/off. When turning on, starts a fresh empty log. Off frees it
    /// (zero cost afterward). Fetch accumulated lines with [`Flow::take_protocol_log`].
    pub fn set_protocol_capture(&mut self, on: bool) {
        self.protocol_log = if on { Some(Vec::new()) } else { None };
    }

    /// Drain the accumulated protocol lines (leaving capture enabled with an empty buffer).
    pub fn take_protocol_log(&mut self) -> Vec<String> {
        match &mut self.protocol_log {
            Some(v) => std::mem::take(v),
            None => Vec::new(),
        }
    }

    /// Emit one resolved turn's protocol block (no-op when capture is off).
    fn emit_protocol(&mut self, pre: &State, m1: MoveChoice, m2: MoveChoice, ins: &[Instruction]) {
        if let Some(log) = &mut self.protocol_log {
            let style = crate::protocol::HpStyle::for_ruleset(&pre.ruleset);
            crate::protocol::protocol_turn(pre, m1, m2, ins, style, log);
        }
    }

    pub fn request(&self) -> Request {
        match &self.pending {
            Pending::Turn => Request::Turn,
            Pending::Pivot { side, .. } => Request::PivotLanding { side: *side },
            Pending::Revive { side, .. } => Request::Revive { side: *side },
            Pending::Replace { sides } => Request::Replace { sides: *sides },
            Pending::Done { winner } => Request::Terminal { winner: *winner },
        }
    }

    /// Answer the pending request and advance to the next one. `choices[i]` must be `Some`
    /// exactly for the sides the pending request names (both for `Turn`). Illegal switch
    /// targets are coerced to the first alive benched mon.
    pub fn submit(&mut self, choices: [Option<PlayerChoice>; 2]) -> Request {
        match std::mem::replace(&mut self.pending, Pending::Turn) {
            Pending::Done { winner } => {
                self.pending = Pending::Done { winner };
            }
            Pending::Turn => self.run_turn(choices),
            Pending::Pivot { side, second, switched, pass_sub } => self.resume_pivot(side, second, switched, pass_sub, choices),
            Pending::Revive { side, second, switched } => self.resume_revive(side, second, switched, choices),
            Pending::Replace { sides } => self.run_replacements(sides, choices),
        }
        self.request()
    }

    // ---- turn ---------------------------------------------------------------------------

    fn run_turn(&mut self, choices: [Option<PlayerChoice>; 2]) {
        let c1 = choices[0].expect("Turn request needs side-one choice");
        let c2 = choices[1].expect("Turn request needs side-two choice");

        let as_move_choice = |c: PlayerChoice, side: SideId, state: &State| match c {
            PlayerChoice::Move { slot, .. } => MoveChoice::Move(slot),
            PlayerChoice::Switch { slot } => {
                let s = state.side(side);
                // A trapped active cannot switch out (Arena Trap / Shadow Tag / Magnet Pull /
                // partial-trap / Mean Look / …). Coerce a stray switch choice into a move so the
                // Flow never forces a trapped mon out — the legal mask already suppresses it.
                if crate::generate::is_trapped(state, side) {
                    let m = s.active().moves.iter().position(|m| m.id != crate::ids::MoveId::None && m.pp > 0);
                    return MoveChoice::Move(m.unwrap_or(0) as u8);
                }
                let p = &s.pokemon[slot as usize];
                if slot != s.active_index && p.species != crate::ids::Species::None && p.is_alive() {
                    MoveChoice::Switch(slot)
                } else {
                    MoveChoice::Switch(alive_bench(state, side).unwrap_or(s.active_index))
                }
            }
        };
        let tera = [
            matches!(c1, PlayerChoice::Move { tera: true, .. }) && !self.state.side(SideId::One).tera_used,
            matches!(c2, PlayerChoice::Move { tera: true, .. }) && !self.state.side(SideId::Two).tera_used,
        ];
        let m1 = as_move_choice(c1, SideId::One, &self.state);
        let m2 = as_move_choice(c2, SideId::Two, &self.state);
        // Pre-turn snapshot for protocol capture (both the fast path and the decomposed path,
        // whose full instruction list only completes at `finish_turn`).
        let pre = self.state;

        // A chosen move pauses for a landing/revive choice. A normal self-switch move (U-turn,
        // Teleport, Shed Tail, …) pauses only when the side has somewhere to go (PS: U-turn with
        // an empty bench just stays in). Revival Blessing instead pauses to pick a FAINTED ally to
        // revive, so it gates on a fainted bench, not an alive one.
        let wants_pause = |side: SideId, mc: MoveChoice| match mc {
            MoveChoice::Move(slot) => {
                let mv = self.state.side(side).active().moves[slot as usize];
                if mv.id.to_id() == "revivalblessing" {
                    crate::generate::has_fainted_bench(&self.state, side)
                } else {
                    move_data(mv.id).self_switch && alive_bench(&self.state, side).is_some()
                }
            }
            MoveChoice::Switch(_) => false,
        };
        let pause = [wants_pause(SideId::One, m1), wants_pause(SideId::Two, m2)];

        if !pause[0] && !pause[1] {
            // Fast path: whole-turn sampled resolution (identical to the certified resolver).
            let si = generate_instructions_sampled(&self.state, m1, m2, [Pivot::Stay; 2], tera, &mut self.rng);
            self.emit_protocol(&pre, m1, m2, &si.instructions);
            self.state.apply_instructions(&si.instructions);
            self.state.turn += 1;
            self.after_turn();
            return;
        }

        // Decomposed (pivot) path: the full instruction list is assembled across pause boundaries
        // and only complete at `finish_turn`, which emits the protocol from `turn_pre`/`turn_m`.
        self.turn_pre = pre;
        self.turn_m = [m1, m2];

        // Decomposed path (a pivot move is in play), mirroring generate_instructions_ctx:
        // custap -> switches -> tera -> ordered moves (pausing on PivotPending) -> EOT.
        let mut b = Branch { prob: 100.0, state: self.state, ins: Vec::new(), draws: Vec::new(), move_failed: false , pivot_update_done: false, per_hit_procs_done: false, pending_damaging_hit: None, drag_tie_speeds: None, after_hit_user_alive: true, late_self_damage: 0, move_any_damage: false };
        let mut exec = Exec::Sample(self.rng);
        let custap = crate::generate::custap_stage(std::slice::from_mut(&mut b), &self.state, m1, m2);

        let mut switch_actions: Vec<(SideId, u8)> = Vec::new();
        if let MoveChoice::Switch(t) = m1 {
            switch_actions.push((SideId::One, t));
        }
        if let MoveChoice::Switch(t) = m2 {
            switch_actions.push((SideId::Two, t));
        }
        // (Both-switch ordering is unreachable here: the decomposed path requires a move.)
        for (side, target) in switch_actions {
            apply_switch(&mut b, side, target);
        }
        for (i, side) in [SideId::One, SideId::Two].into_iter().enumerate() {
            if tera[i] && matches!([m1, m2][i], MoveChoice::Move(_)) {
                apply_tera(&mut b, side);
            }
        }

        let mk_action = |side: SideId, mc: MoveChoice, paused: bool, cu: bool| match mc {
            MoveChoice::Move(slot) => Some(Action {
                side,
                move_idx: slot,
                pivot: if paused { Pivot::Pause } else { Pivot::Stay },
                foe_pending_move: None,
                shell_phys: None,
                custap: cu,
                struggling: crate::generate::no_usable_move(&b.state, side),
                external_move: None,
                called: false,
            }),
            MoveChoice::Switch(_) => None,
        };
        let a1 = mk_action(SideId::One, m1, pause[0], custap[0]);
        let a2 = mk_action(SideId::Two, m2, pause[1], custap[1]);
        let switched = [matches!(m1, MoveChoice::Switch(_)), matches!(m2, MoveChoice::Switch(_))];

        match (a1, a2) {
            (Some(a), None) | (None, Some(a)) => {
                let (b, pause) = exec_one_move(b, a, &mut exec);
                self.rng = exec_state(&exec);
                if self.park_if_paused(&b, pause, None, switched) {
                    return;
                }
                self.finish_turn(b, switched);
            }
            (Some(a), Some(bb)) => {
                let (mut first, mut second) = match move_order(&b.state, &a, &bb) {
                    Order::First(f) if f == a.side => (a, bb),
                    Order::First(_) => (bb, a),
                    Order::Tie => {
                        if exec.coin().unwrap_or(true) {
                            (a, bb)
                        } else {
                            (bb, a)
                        }
                    }
                };
                first.foe_pending_move =
                    Some(b.state.side(second.side).active().moves[second.move_idx as usize].id);
                second.foe_pending_move = None;

                let (b, pause) = exec_one_move(b, first, &mut exec);
                self.rng = exec_state(&exec);
                if self.park_if_paused(&b, pause, Some(second), switched) {
                    return;
                }
                self.continue_after_first(b, second, switched);
            }
            (None, None) => unreachable!("decomposed path requires at least one move action"),
        }
    }

    /// If the just-executed move paused (pivot landing or revive), store the pending request and
    /// return true. `second` is the not-yet-run slower move (resumed after the choice).
    fn park_if_paused(&mut self, b: &Branch, pause: Pause, second: Option<Action>, switched: [bool; 2]) -> bool {
        match pause {
            Pause::None => false,
            Pause::Pivot { side, pass_sub } => {
                self.state = b.state;
                self.pending = Pending::Pivot { side, second, switched, pass_sub };
                true
            }
            Pause::Revive { side } => {
                self.state = b.state;
                self.pending = Pending::Revive { side, second, switched };
                true
            }
        }
    }

    /// Run the second mover (gated exactly as `sequence_two_moves` gates it), then EOT.
    fn continue_after_first(&mut self, b: Branch, second: Action, switched: [bool; 2]) {
        let gate = b.state.side(second.side).active().is_alive()
            && !b.state.side(second.side).volatiles.contains(crate::volatile::VolatileStatus::Flinch)
            && !battle_over(&b.state);
        if !gate {
            self.finish_turn(b, switched);
            return;
        }
        let mut exec = Exec::Sample(self.rng);
        let (b, pause) = exec_one_move(b, second, &mut exec);
        self.rng = exec_state(&exec);
        if self.park_if_paused(&b, pause, None, switched) {
            return;
        }
        self.finish_turn(b, switched);
    }

    fn resume_pivot(&mut self, side: SideId, second: Option<Action>, mut switched: [bool; 2], pass_sub: bool, choices: [Option<PlayerChoice>; 2]) {
        let picked = match choices[side.index()] {
            Some(PlayerChoice::Switch { slot }) => Some(slot),
            _ => alive_bench(&self.state, side),
        };
        let slot = picked.and_then(|slot| {
            let s = self.state.side(side);
            let ok = (slot as usize) < 6 && slot != s.active_index && {
                let p = &s.pokemon[slot as usize];
                p.species != crate::ids::Species::None && p.is_alive()
            };
            if ok { Some(slot) } else { alive_bench(&self.state, side) }
        });
        let Some(slot) = slot else {
            // A pivot pause whose bench emptied before the landing resolved. The root is FIXED:
            // `wants_pause` below grants `Pivot::Pause` off the CHOSEN move, and `run_move_action`
            // then substitutes the move (Struggle via `no_usable_move`, or the Encore
            // `OverrideAction` redirect) — a PP-stalled mon with an all-fainted bench picked
            // Revival Blessing, Struggled, and Struggle's damaging path inherited the pause (then
            // its own recoil killed the user). `generate.rs` re-derives the pause from the EXECUTED
            // move and gates every `PivotPending` on `has_alive_bench`; `tests/pivot_landing_bench.rs`
            // is the property, `tests/pivot_pause_survives_move_substitution.rs` the unit repro.
            // This arm STAYS as a tripwire: PS's behavior with nowhere to go is "stay in"
            // (`sim/battle.ts:2904` clears `switchFlag` when `!canSwitch`), which is what skipping
            // the switch does, and a 4096-env trainer must not die on a future regression.
            eprintln!(
                "[engine] PivotLanding with no live bench (side {:?}, turn {}) — staying in",
                side, self.state.turn
            );
            let b = Branch { prob: 100.0, state: self.state, ins: Vec::new(), draws: Vec::new(), move_failed: false, pivot_update_done: false, per_hit_procs_done: false, pending_damaging_hit: None, drag_tie_speeds: None, after_hit_user_alive: true, late_self_damage: 0, move_any_damage: false };
            match second {
                Some(a) => self.continue_after_first(b, a, switched),
                None => self.finish_turn(b, switched),
            }
            return;
        };
        let mut b = Branch { prob: 100.0, state: self.state, ins: Vec::new(), draws: Vec::new(), move_failed: false , pivot_update_done: false, per_hit_procs_done: false, pending_damaging_hit: None, drag_tie_speeds: None, after_hit_user_alive: true, late_self_damage: 0, move_any_damage: false };
        // Shed Tail passes its Substitute to the lander; every other pivot clears it.
        if pass_sub {
            crate::generate::apply_switch_pass_sub(&mut b, side, slot);
        } else {
            apply_switch(&mut b, side, slot);
        }
        switched[side.index()] = true;
        match second {
            Some(a) => self.continue_after_first(b, a, switched),
            None => self.finish_turn(b, switched),
        }
    }

    /// Resume after a `Revive` request: revive the chosen (or coerced) fainted party member,
    /// keeping it benched. The active is unchanged, so the turn continues normally.
    fn resume_revive(&mut self, side: SideId, second: Option<Action>, switched: [bool; 2], choices: [Option<PlayerChoice>; 2]) {
        let wanted = match choices[side.index()] {
            Some(PlayerChoice::Switch { slot }) => Some(slot),
            _ => None,
        };
        let valid = |slot: u8| {
            let s = self.state.side(side);
            slot != s.active_index
                && s.pokemon[slot as usize].species != crate::ids::Species::None
                && !s.pokemon[slot as usize].is_alive()
        };
        let slot = match wanted {
            Some(slot) if valid(slot) => Some(slot),
            _ => fainted_bench(&self.state, side),
        };
        let mut b = Branch { prob: 100.0, state: self.state, ins: Vec::new(), draws: Vec::new(), move_failed: false , pivot_update_done: false, per_hit_procs_done: false, pending_damaging_hit: None, drag_tie_speeds: None, after_hit_user_alive: true, late_self_damage: 0, move_any_damage: false };
        match slot {
            Some(slot) => crate::generate::apply_revive(&mut b, side, slot),
            // Same defensive stance as the pivot fallback above: a Revive request with no
            // fainted ally is an engine-state bug to hunt, not a reason to kill the trainer.
            None => eprintln!(
                "[engine] Revive with no fainted ally (side {:?}, turn {}) — skipping",
                side, self.state.turn
            ),
        }
        match second {
            Some(a) => self.continue_after_first(b, a, switched),
            None => self.finish_turn(b, switched),
        }
    }

    /// End-of-turn residuals (skipped when the battle already ended), turn increment, and the
    /// post-turn faint scan.
    fn finish_turn(&mut self, b: Branch, switched: [bool; 2]) {
        let mut b = b;
        if !battle_over(&b.state) {
            let mut exec = Exec::Sample(self.rng);
            let out = exec.prune(apply_end_of_turn(b, switched));
            self.rng = exec_state(&exec);
            b = out.into_iter().next().expect("EOT produced no branches");
        }
        // Decomposed-turn protocol: the pivot path's full instruction list (moves across the pause
        // boundary + landing switches + EOT) is complete here.
        if self.protocol_log.is_some() {
            let (pre, m1, m2) = (self.turn_pre, self.turn_m[0], self.turn_m[1]);
            self.emit_protocol(&pre, m1, m2, &b.ins);
        }
        self.state = b.state;
        self.state.turn += 1;
        self.after_turn();
    }

    // ---- replacements ---------------------------------------------------------------------

    fn run_replacements(&mut self, sides: [bool; 2], choices: [Option<PlayerChoice>; 2]) {
        let pick = |state: &State, side: SideId, c: Option<PlayerChoice>| -> u8 {
            let wanted = match c {
                Some(PlayerChoice::Switch { slot }) => Some(slot),
                _ => None,
            };
            let s = state.side(side);
            match wanted {
                Some(slot)
                    if slot != s.active_index
                        && s.pokemon[slot as usize].species != crate::ids::Species::None
                        && s.pokemon[slot as usize].is_alive() =>
                {
                    slot
                }
                _ => alive_bench(state, side).expect("Replace issued with no bench"),
            }
        };
        let pre = self.state;
        let ins = match sides {
            [true, true] => {
                let t1 = pick(&self.state, SideId::One, choices[0]);
                let t2 = pick(&self.state, SideId::Two, choices[1]);
                // Double replacement: both enter, hazards, then switch-in abilities in speed
                // order — each ability sees the other fresh mon (PS semantics; distinct from a
                // chosen double switch).
                switch_into_pair(&mut self.state, [(SideId::One, t1), (SideId::Two, t2)])
            }
            [true, false] => {
                let t = pick(&self.state, SideId::One, choices[0]);
                crate::generate::switch_into(&mut self.state, SideId::One, t)
            }
            [false, true] => {
                let t = pick(&self.state, SideId::Two, choices[1]);
                crate::generate::switch_into(&mut self.state, SideId::Two, t)
            }
            [false, false] => unreachable!("Replace with no sides"),
        };
        // Protocol: the switch-in(s) + entry-hazard damage / switch-in ability effects, in order,
        // from `switch_into`'s returned instruction stream.
        if let Some(log) = &mut self.protocol_log {
            let style = crate::protocol::HpStyle::for_ruleset(&pre.ruleset);
            crate::protocol::emit_instructions(&pre, &ins, style, log);
        }
        self.after_turn();
    }

    /// Scan for terminal / replacement needs; hazard faints on freshly-entered replacements
    /// re-issue `Replace` (the chained-faint cascade).
    fn after_turn(&mut self) {
        let need = |side: SideId| -> Option<bool> {
            let s = self.state.side(side);
            if s.active().is_alive() {
                return Some(false);
            }
            // Fainted active: replaceable if any bench mon lives; else this side has lost.
            if alive_bench(&self.state, side).is_some() {
                Some(true)
            } else {
                None
            }
        };
        match (need(SideId::One), need(SideId::Two)) {
            (None, _) | (_, None) => {
                self.pending = Pending::Done { winner: winner_of(&self.state) };
            }
            (Some(false), Some(false)) => {
                self.pending = Pending::Turn;
            }
            (Some(a), Some(b)) => {
                self.pending = Pending::Replace { sides: [a, b] };
            }
        }
    }
}

/// Execute one move action with sampling, returning the surviving branch and, if its transcript
/// ends in a deferred choice, the pause it raised (pivot landing or Revival Blessing revive).
fn exec_one_move(b: Branch, a: Action, exec: &mut Exec) -> (Branch, Pause) {
    let before = b.ins.len();
    let out = exec.prune(execute_move(b, a));
    let b = out.into_iter().next().expect("move produced no branches");
    let mut pause = Pause::None;
    for i in &b.ins[before..] {
        match i {
            Instruction::PivotPending { side } => {
                // Only Shed Tail passes its Substitute across the pivot; the actor is still
                // the pivot user here (it has not switched yet).
                let pass_sub = b.state.side(a.side).active().moves[a.move_idx as usize].id.to_id() == "shedtail";
                pause = Pause::Pivot { side: *side, pass_sub };
            }
            Instruction::RevivePending { side } => {
                pause = Pause::Revive { side: *side };
            }
            _ => {}
        }
    }
    (b, pause)
}

/// First non-active party slot that has fainted (a Revival Blessing target).
fn fainted_bench(state: &State, side: SideId) -> Option<u8> {
    let s = state.side(side);
    (0..6u8).find(|&i| {
        i != s.active_index
            && s.pokemon[i as usize].species != crate::ids::Species::None
            && !s.pokemon[i as usize].is_alive()
    })
}

fn exec_state(exec: &Exec) -> u64 {
    match exec {
        Exec::Sample(s) => *s,
        Exec::Enumerate => unreachable!("flow always samples"),
    }
}

#[cfg(test)]
mod protocol_tests {
    use super::*;

    #[test]
    fn flow_captures_protocol_when_enabled() {
        // Off by default = zero cost, empty log.
        let mut f = Flow::new(crate::team::default_matchup(), 7);
        assert!(f.take_protocol_log().is_empty());

        // On: a resolved Turn appends a |turn|…|upkeep block.
        f.set_protocol_capture(true);
        let mv = |slot| Some(PlayerChoice::Move { slot, tera: false });
        let _ = f.submit([mv(0), mv(0)]);
        let log = f.take_protocol_log();
        assert!(log.iter().any(|l| l.starts_with("|turn|")), "expected a |turn| line: {log:?}");
        assert!(log.iter().any(|l| l == "|upkeep"), "expected |upkeep|: {log:?}");
        assert!(log.iter().any(|l| l.starts_with("|move|")), "expected a |move| line: {log:?}");
        // Draining leaves capture enabled with a fresh buffer.
        assert!(f.take_protocol_log().is_empty());
    }
}
