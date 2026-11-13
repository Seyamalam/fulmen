//! The plugin-order bridge: `Fulmen.use()` calls are handed here as
//! `(name, requires, conflicts)` triples and come back as the
//! deterministic setup order plus the aggregated problem list
//! (`fulmen_core::plugins::resolve_order`, DESIGN.md §13.2).
//!
//! The function is deliberately dumb: plugin objects, `PluginContext`
//! and the whole registration flow stay in Python — only the pure-data
//! dependency graph crosses the bridge, so the logic is unit-tested in
//! `fulmen-core` without an interpreter.

use pyo3::prelude::*;

/// One `use()` registration: `(name, requires, conflicts)` — the
/// plugin's declared relationships, positionally indexed by
/// registration order.
type UseDecl = (String, Vec<String>, Vec<String>);

/// Resolve `use()` registrations into a setup order.
///
/// `decls[i]` describes the i-th `app.use()` / `group.use()` call in
/// registration order. Returns `(order, problems)`: `order` holds
/// declaration indices — requirements first, registration order
/// breaking ties (§13.2) — and `problems` aggregates every resolution
/// error (missing requirements, conflicts, duplicate names, cycles) in
/// the format `StartupError` lists.
#[pyfunction]
pub(crate) fn resolve_plugin_order(decls: Vec<UseDecl>) -> (Vec<usize>, Vec<String>) {
    let decls: Vec<fulmen_core::PluginDecl> = decls
        .into_iter()
        .map(|(name, requires, conflicts)| fulmen_core::PluginDecl {
            name,
            requires,
            conflicts,
        })
        .collect();
    let (order, problems) = fulmen_core::resolve_order(&decls);
    (order, problems.iter().map(ToString::to_string).collect())
}
