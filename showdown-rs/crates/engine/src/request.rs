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
    /// Battle over: -1 draw, 0 side one, 1 side two.
    Terminal { winner: i64 },
}

enum Pending {
    Turn,
    Pivot { side: SideId, second: Option<Action>, switched: [bool; 2] },
    Replace { sides: [bool; 2] },
    Done { winner: i64 },
}

/// A battle advanced request-by-request with sampled stochastics.
pub struct Flow {
    pub state: State,
    pub rng: u64,
    pending: Pending,
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
        Flow { state, rng: seed.wrapping_add(1), pending: Pending::Turn }
    }

    pub fn request(&self) -> Request {
        match &self.pending {
            Pending::Turn => Request::Turn,
            Pending::Pivot { side, .. } => Request::PivotLanding { side: *side },
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
            Pending::Pivot { side, second, switched } => self.resume_pivot(side, second, switched, choices),
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

        // A chosen move pauses for a landing only if it is a self-switch move AND the side has
        // somewhere to go (PS: U-turn with an empty bench just stays in).
        let wants_pause = |side: SideId, mc: MoveChoice| match mc {
            MoveChoice::Move(slot) => {
                let mv = self.state.side(side).active().moves[slot as usize];
                move_data(mv.id).self_switch && alive_bench(&self.state, side).is_some()
            }
            MoveChoice::Switch(_) => false,
        };
        let pause = [wants_pause(SideId::One, m1), wants_pause(SideId::Two, m2)];

        if !pause[0] && !pause[1] {
            // Fast path: whole-turn sampled resolution (identical to the certified resolver).
            let si = generate_instructions_sampled(&self.state, m1, m2, [Pivot::Stay; 2], tera, &mut self.rng);
            self.state.apply_instructions(&si.instructions);
            self.state.turn += 1;
            self.after_turn();
            return;
        }

        // Decomposed path (a pivot move is in play), mirroring generate_instructions_ctx:
        // switches -> tera -> ordered moves (pausing on PivotPending) -> EOT.
        let mut b = Branch { prob: 100.0, state: self.state, ins: Vec::new() };
        let mut exec = Exec::Sample(self.rng);

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

        let mk_action = |side: SideId, mc: MoveChoice, paused: bool| match mc {
            MoveChoice::Move(slot) => Some(Action {
                side,
                move_idx: slot,
                pivot: if paused { Pivot::Pause } else { Pivot::Stay },
                foe_pending_move: None,
            }),
            MoveChoice::Switch(_) => None,
        };
        let a1 = mk_action(SideId::One, m1, pause[0]);
        let a2 = mk_action(SideId::Two, m2, pause[1]);
        let switched = [matches!(m1, MoveChoice::Switch(_)), matches!(m2, MoveChoice::Switch(_))];

        match (a1, a2) {
            (Some(a), None) | (None, Some(a)) => {
                let (b, paused_side) = exec_one_move(b, a, &mut exec);
                self.rng = exec_state(&exec);
                if let Some(side) = paused_side {
                    self.state = b.state;
                    self.pending = Pending::Pivot { side, second: None, switched };
                    return;
                }
                self.finish_turn(b, switched);
            }
            (Some(a), Some(bb)) => {
                let (mut first, mut second) = match move_order(&b.state, a.side, a.move_idx, bb.side, bb.move_idx) {
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

                let (b, paused_side) = exec_one_move(b, first, &mut exec);
                self.rng = exec_state(&exec);
                if let Some(side) = paused_side {
                    self.state = b.state;
                    self.pending = Pending::Pivot { side, second: Some(second), switched };
                    return;
                }
                self.continue_after_first(b, second, switched);
            }
            (None, None) => unreachable!("decomposed path requires at least one move action"),
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
        let (b, paused_side) = exec_one_move(b, second, &mut exec);
        self.rng = exec_state(&exec);
        if let Some(side) = paused_side {
            self.state = b.state;
            self.pending = Pending::Pivot { side, second: None, switched };
            return;
        }
        self.finish_turn(b, switched);
    }

    fn resume_pivot(&mut self, side: SideId, second: Option<Action>, mut switched: [bool; 2], choices: [Option<PlayerChoice>; 2]) {
        let slot = match choices[side.index()] {
            Some(PlayerChoice::Switch { slot }) => slot,
            _ => alive_bench(&self.state, side).expect("PivotLanding issued with no bench"),
        };
        let slot = {
            let s = self.state.side(side);
            let p = &s.pokemon[slot as usize];
            if slot != s.active_index && p.species != crate::ids::Species::None && p.is_alive() {
                slot
            } else {
                alive_bench(&self.state, side).expect("PivotLanding issued with no bench")
            }
        };
        let mut b = Branch { prob: 100.0, state: self.state, ins: Vec::new() };
        apply_switch(&mut b, side, slot);
        switched[side.index()] = true;
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
        match sides {
            [true, true] => {
                let t1 = pick(&self.state, SideId::One, choices[0]);
                let t2 = pick(&self.state, SideId::Two, choices[1]);
                // Double replacement: both enter, hazards, then switch-in abilities in speed
                // order — each ability sees the other fresh mon (PS semantics; distinct from a
                // chosen double switch).
                switch_into_pair(&mut self.state, [(SideId::One, t1), (SideId::Two, t2)]);
            }
            [true, false] => {
                let t = pick(&self.state, SideId::One, choices[0]);
                crate::generate::switch_into(&mut self.state, SideId::One, t);
            }
            [false, true] => {
                let t = pick(&self.state, SideId::Two, choices[1]);
                crate::generate::switch_into(&mut self.state, SideId::Two, t);
            }
            [false, false] => unreachable!("Replace with no sides"),
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

/// Execute one move action with sampling, returning the surviving branch and, if its
/// transcript ends in a pending pivot, the side awaiting a landing choice.
fn exec_one_move(b: Branch, a: Action, exec: &mut Exec) -> (Branch, Option<SideId>) {
    let before = b.ins.len();
    let out = exec.prune(execute_move(b, a));
    let b = out.into_iter().next().expect("move produced no branches");
    let paused = b.ins[before..]
        .iter()
        .find_map(|i| match i {
            Instruction::PivotPending { side } => Some(*side),
            _ => None,
        });
    (b, paused)
}

fn exec_state(exec: &Exec) -> u64 {
    match exec {
        Exec::Sample(s) => *s,
        Exec::Enumerate => unreachable!("flow always samples"),
    }
}
