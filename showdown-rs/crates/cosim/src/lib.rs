//! Library surface of the cosim crate: the PS<->engine state bridge, reusable by other crates
//! (e.g. `pybridge`'s `export_state`). The verifier binary (`main.rs`) keeps its own module tree
//! (including the campaign-owned `seedgate`/`drawdiff`), so these two modules are compiled into
//! both the bin and this lib from the same source — a deliberate, cheap duplication that avoids
//! touching the binary's off-limits internals.

pub mod convert;
pub mod export;
