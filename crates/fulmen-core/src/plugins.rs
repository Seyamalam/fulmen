//! Plugin metadata and dependency resolution (DESIGN.md §13).
//!
//! Plugins are Python objects, but their `requires`/`conflicts` graph is
//! pure data: the bridge turns each registered plugin into a [`PluginDecl`]
//! (its position in the slice is its registration index) and
//! [`resolve_order`] answers either the deterministic setup order or every
//! problem found — missing requirements, declared conflicts, duplicate
//! names and dependency cycles (§14.1).
//!
//! Ordering (§13.2): plugins are topologically sorted by `requires`; ties
//! keep registration order. Kahn's algorithm with an index-ordered ready
//! set emits the lexicographically smallest valid order, which is exactly
//! "as close to `use()` order as dependencies allow". Members of a
//! dependency cycle never enter the order — each cycle becomes a
//! [`PluginProblem::Cycle`] naming its members — while everything else is
//! still emitted so setup can proceed best-effort under error collection.

use std::collections::{BTreeSet, HashMap};
use std::fmt;

/// One registered plugin's identity and declared relationships.
///
/// The registration index (position in the declaration slice) is the
/// tie-breaker for ordering; `version` stays with the caller — it is
/// display metadata, not an ordering input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginDecl {
    /// Unique plugin name, e.g. `"fulmen.jwt"`.
    pub name: String,
    /// Names of plugins that must be set up before this one.
    pub requires: Vec<String>,
    /// Names of plugins that cannot be used together with this one.
    pub conflicts: Vec<String>,
}

/// One plugin-resolution problem; all of them are startup errors (§14.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginProblem {
    /// Two `use()` calls registered the same plugin name.
    DuplicateName {
        /// The duplicated name.
        name: String,
    },
    /// A `requires` entry names a plugin that was never registered.
    MissingRequirement {
        /// The plugin asking for the dependency.
        plugin: String,
        /// The missing name.
        requires: String,
    },
    /// A `conflicts` entry names a plugin that is also registered.
    Conflict {
        /// The plugin declaring the conflict.
        plugin: String,
        /// The conflicting name.
        conflicts: String,
    },
    /// A `requires` cycle: the path loops back to its first member.
    Cycle {
        /// The cycle as a name path, e.g. `a -> b -> a`.
        path: Vec<String>,
    },
}

impl fmt::Display for PluginProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateName { name } => {
                write!(f, "plugin `{name}` is registered more than once")
            }
            Self::MissingRequirement { plugin, requires } => {
                write!(
                    f,
                    "plugin `{plugin}` requires `{requires}`, which is not registered"
                )
            }
            Self::Conflict { plugin, conflicts } if plugin == conflicts => {
                write!(f, "plugin `{plugin}` conflicts with itself")
            }
            Self::Conflict { plugin, conflicts } => {
                write!(
                    f,
                    "plugin `{plugin}` conflicts with registered plugin `{conflicts}`"
                )
            }
            Self::Cycle { path } => {
                write!(f, "plugin dependency cycle: {}", path.join(" -> "))
            }
        }
    }
}

impl std::error::Error for PluginProblem {}

/// Resolve the setup order of `decls` (§13.2, §6.4.2).
///
/// Returns `(order, problems)`: `order` lists declaration indices in
/// setup order — every requirement precedes its dependents and ties keep
/// registration order. `problems` aggregates every resolution error
/// found; members of a dependency cycle are the only declarations ever
/// left out of `order`.
#[must_use]
pub fn resolve_order(decls: &[PluginDecl]) -> (Vec<usize>, Vec<PluginProblem>) {
    let count = decls.len();
    let mut problems = Vec::new();

    // `name -> first registration index`: requires/conflicts resolve
    // against the first registration; later duplicates are problems.
    let mut index_by_name: HashMap<&str, usize> = HashMap::with_capacity(count);
    for (index, decl) in decls.iter().enumerate() {
        if index_by_name.insert(decl.name.as_str(), index).is_some() {
            problems.push(PluginProblem::DuplicateName {
                name: decl.name.clone(),
            });
        }
    }

    // `deps[i]` = the indices `i` requires; `dependents[i]` = who requires `i`.
    // Self-requires stay in the graph so the cycle pass reports `x -> x`.
    let mut deps: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); count];
    let mut dependents: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); count];
    for (index, decl) in decls.iter().enumerate() {
        for required in &decl.requires {
            match index_by_name.get(required.as_str()) {
                Some(&dependency) => {
                    deps[index].insert(dependency);
                    dependents[dependency].insert(index);
                }
                None => problems.push(PluginProblem::MissingRequirement {
                    plugin: decl.name.clone(),
                    requires: required.clone(),
                }),
            }
        }
        for other in &decl.conflicts {
            if index_by_name.contains_key(other.as_str()) {
                problems.push(PluginProblem::Conflict {
                    plugin: decl.name.clone(),
                    conflicts: other.clone(),
                });
            }
        }
    }

    let all: BTreeSet<usize> = (0..count).collect();
    let mut order = kahn(&deps, &dependents, &all);

    // What Kahn left un-emitted contains every cycle member plus the
    // plugins blocked by one. Report each concrete cycle, remove its
    // members, and repeat — the remainder is a DAG and emits normally.
    let emitted: BTreeSet<usize> = order.iter().copied().collect();
    let mut leftover: BTreeSet<usize> = all.into_iter().filter(|i| !emitted.contains(i)).collect();
    while let Some(cycle) = find_cycle(&deps, &leftover) {
        for member in &cycle {
            leftover.remove(member);
        }
        problems.push(PluginProblem::Cycle {
            path: cycle.iter().map(|&i| decls[i].name.clone()).collect(),
        });
    }
    order.extend(kahn(&deps, &dependents, &leftover));

    (order, problems)
}

/// Stable topological sort over `nodes`: each node's `deps` come first,
/// ready nodes emit in registration-index order. Only edges inside
/// `nodes` count, so calling it on a leftover set ignores edges to
/// already-handled declarations.
fn kahn(
    deps: &[BTreeSet<usize>],
    dependents: &[BTreeSet<usize>],
    nodes: &BTreeSet<usize>,
) -> Vec<usize> {
    let mut indegree: HashMap<usize, usize> = nodes
        .iter()
        .map(|&i| (i, deps[i].iter().filter(|d| nodes.contains(d)).count()))
        .collect();
    let mut ready: BTreeSet<usize> = nodes.iter().copied().filter(|i| indegree[i] == 0).collect();
    let mut order = Vec::with_capacity(nodes.len());
    while let Some(&node) = ready.iter().next() {
        ready.remove(&node);
        order.push(node);
        for &dependent in &dependents[node] {
            let Some(degree) = indegree.get_mut(&dependent) else {
                continue;
            };
            *degree -= 1;
            if *degree == 0 {
                ready.insert(dependent);
            }
        }
    }
    order
}

/// One concrete cycle inside `nodes`, as `[a, b, …, a]` indices —
/// `None` when the induced subgraph is acyclic. Three-colour iterative
/// DFS; a gray edge target closes a loop on the current path.
fn find_cycle(deps: &[BTreeSet<usize>], nodes: &BTreeSet<usize>) -> Option<Vec<usize>> {
    const GRAY: u8 = 1;
    const BLACK: u8 = 2;
    let mut color = vec![0u8; deps.len()];
    for &start in nodes {
        if color[start] != 0 {
            continue;
        }
        // The DFS path and a per-level edge cursor; `path` is exactly the
        // set of gray nodes.
        let mut path: Vec<usize> = vec![start];
        let mut cursors: Vec<std::collections::btree_set::Iter<'_, usize>> =
            vec![deps[start].iter()];
        color[start] = GRAY;
        while let Some(cursor) = cursors.last_mut() {
            match cursor.next() {
                Some(&next) if !nodes.contains(&next) || color[next] == BLACK => {}
                Some(&next) if color[next] == GRAY => {
                    if let Some(pos) = path.iter().position(|&n| n == next) {
                        let mut cycle = path[pos..].to_vec();
                        cycle.push(next);
                        return Some(cycle);
                    }
                }
                Some(&next) => {
                    color[next] = GRAY;
                    path.push(next);
                    cursors.push(deps[next].iter());
                }
                None => {
                    if let Some(finished) = path.pop() {
                        color[finished] = BLACK;
                    }
                    cursors.pop();
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decl(name: &str, requires: &[&str], conflicts: &[&str]) -> PluginDecl {
        PluginDecl {
            name: name.to_owned(),
            requires: requires.iter().map(|s| (*s).to_owned()).collect(),
            conflicts: conflicts.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    fn names<'a>(decls: &'a [PluginDecl], order: &[usize]) -> Vec<&'a str> {
        order.iter().map(|&i| decls[i].name.as_str()).collect()
    }

    #[test]
    fn empty_resolves_to_empty() {
        let (order, problems) = resolve_order(&[]);
        assert!(order.is_empty() && problems.is_empty());
    }

    #[test]
    fn independent_plugins_keep_registration_order() {
        let decls = [
            decl("a", &[], &[]),
            decl("b", &[], &[]),
            decl("c", &[], &[]),
        ];
        let (order, problems) = resolve_order(&decls);
        assert!(problems.is_empty());
        assert_eq!(names(&decls, &order), ["a", "b", "c"]);
    }

    #[test]
    fn requirements_setup_first_regardless_of_registration_order() {
        let decls = [decl("late", &["early"], &[]), decl("early", &[], &[])];
        let (order, problems) = resolve_order(&decls);
        assert!(problems.is_empty());
        assert_eq!(names(&decls, &order), ["early", "late"]);
    }

    #[test]
    fn ties_keep_registration_order_not_depth_first() {
        // `d` was registered first but requires `c`; a/b are unrelated.
        // The stable order is as close to registration order as the
        // dependency allows: [a, b, c, d], not [c, d, a, b].
        let decls = [
            decl("d", &["c"], &[]),
            decl("a", &[], &[]),
            decl("b", &[], &[]),
            decl("c", &[], &[]),
        ];
        let (order, problems) = resolve_order(&decls);
        assert!(problems.is_empty());
        assert_eq!(names(&decls, &order), ["a", "b", "c", "d"]);
    }

    #[test]
    fn transitive_requirements_resolve() {
        let decls = [
            decl("c", &["b"], &[]),
            decl("a", &[], &[]),
            decl("b", &["a"], &[]),
        ];
        let (order, problems) = resolve_order(&decls);
        assert!(problems.is_empty());
        assert_eq!(names(&decls, &order), ["a", "b", "c"]);
    }

    #[test]
    fn missing_requirement_is_a_problem_and_others_still_order() {
        let decls = [decl("a", &["ghost"], &[]), decl("b", &[], &[])];
        let (order, problems) = resolve_order(&decls);
        assert_eq!(
            problems,
            [PluginProblem::MissingRequirement {
                plugin: "a".to_owned(),
                requires: "ghost".to_owned(),
            }]
        );
        // `a` still emits — its missing edge can't order it anyway.
        assert_eq!(names(&decls, &order), ["a", "b"]);
    }

    #[test]
    fn declared_conflict_is_a_problem() {
        let decls = [decl("a", &[], &["b"]), decl("b", &[], &[])];
        let (_order, problems) = resolve_order(&decls);
        assert_eq!(
            problems,
            [PluginProblem::Conflict {
                plugin: "a".to_owned(),
                conflicts: "b".to_owned(),
            }]
        );
    }

    #[test]
    fn conflict_with_self_is_a_problem() {
        let decls = [decl("a", &[], &["a"])];
        let (_order, problems) = resolve_order(&decls);
        assert_eq!(problems.len(), 1);
        assert_eq!(problems[0].to_string(), "plugin `a` conflicts with itself");
    }

    #[test]
    fn duplicate_name_is_a_problem() {
        let decls = [decl("a", &[], &[]), decl("a", &[], &[])];
        let (_order, problems) = resolve_order(&decls);
        assert_eq!(
            problems,
            [PluginProblem::DuplicateName {
                name: "a".to_owned()
            }]
        );
    }

    #[test]
    fn self_requirement_is_a_cycle() {
        let decls = [decl("x", &["x"], &[])];
        let (order, problems) = resolve_order(&decls);
        assert!(order.is_empty());
        assert_eq!(problems.len(), 1);
        assert_eq!(problems[0].to_string(), "plugin dependency cycle: x -> x");
    }

    #[test]
    fn cycle_members_are_excluded_and_named() {
        let decls = [
            decl("a", &["c"], &[]),
            decl("b", &["a"], &[]),
            decl("c", &["b"], &[]),
            decl("free", &[], &[]),
        ];
        let (order, problems) = resolve_order(&decls);
        assert_eq!(names(&decls, &order), ["free"]);
        assert_eq!(problems.len(), 1);
        let text = problems[0].to_string();
        assert!(text.starts_with("plugin dependency cycle: "));
        for member in ["a", "b", "c"] {
            assert!(
                text.contains(member),
                "cycle message misses {member}: {text}"
            );
        }
    }

    #[test]
    fn dependent_of_a_cycle_still_emits() {
        // `d` needs `c`, which is cyclic — `d` itself is not cyclic, so
        // it stays in the best-effort order while the cycle is reported.
        let decls = [
            decl("a", &["b"], &[]),
            decl("b", &["a"], &[]),
            decl("c", &["a"], &[]),
            decl("d", &["c"], &[]),
        ];
        let (order, problems) = resolve_order(&decls);
        assert_eq!(problems.len(), 1);
        assert_eq!(names(&decls, &order), ["c", "d"]);
    }

    #[test]
    fn problems_aggregate() {
        let decls = [
            decl("a", &["ghost"], &["b"]),
            decl("b", &[], &[]),
            decl("a", &[], &[]),
        ];
        let (_order, problems) = resolve_order(&decls);
        assert_eq!(problems.len(), 3);
    }
}
