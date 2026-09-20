//! Constraint registry: which contracts are logically related, and how.
//!
//! This module owns three things the solver cannot work without: the shape of a
//! constraint group, the *meaning* of each relation expressed as the set of
//! resolutions it permits, and the reverse index from a contract to the groups
//! containing it. Loading `config/registry.toml` and reconciling it against
//! venue market metadata is phase 5; nothing here reads a file.
//!
//! The reverse index exists because dirty marking must be cheap. A price update
//! touches one or two groups out of thousands, and finding them has to be a
//! hash lookup rather than a scan, so the index is built once at load time.

use crate::types::ContractId;
use std::collections::HashMap;

/// Registry-scoped group identifier. Ordering is derived so the solver can sort
/// by it and produce the same output order on every replay of the same input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct GroupId(pub u32);

/// One way the world can resolve, as the set of member contracts paying $1.
///
/// Anything not in the set pays nothing. This is the entire semantic content of
/// a [`Relation`]: two relations that permit the same resolutions are the same
/// constraint, and a trade is risk-free exactly when its payoff is acceptable
/// under every state in this list.
pub type ResolutionState = Vec<ContractId>;

/// A logical relationship between contracts that constrains their prices.
///
/// [`Monotone`](Relation::Monotone) is a generalization of
/// [`Implies`](Relation::Implies), not a separate idea: a ladder of `n` rungs is
/// `n - 1` chained implications, and expressing it as one group means a rung
/// appears once rather than twice and the solver evaluates the whole ladder on a
/// single dirty mark.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Relation {
    /// A binary contract and its own negation: yes and no sum to exactly $1.
    Complement { contract: ContractId },
    /// Mutually exclusive and exhaustive outcomes: exactly one pays $1.
    Exhaustive { members: Vec<ContractId> },
    /// A threshold ladder, ordered from the weakest claim to the strongest.
    ///
    /// Each rung's yes event contains the next rung's, so probabilities must be
    /// non-increasing along the vector. "BTC above 70k" comes before "BTC above
    /// 80k".
    Monotone { ordered: Vec<ContractId> },
    /// `antecedent` resolving yes forces `consequent` to resolve yes.
    Implies {
        antecedent: ContractId,
        consequent: ContractId,
    },
    /// The same real-world event listed on two venues.
    ///
    /// `verified` stays false until a human has confirmed both contracts settle
    /// on the same source, at the same time, with the same tie handling. An
    /// unverified pair never produces a signal: a cross-venue trade on two
    /// contracts that turn out to differ is not an arbitrage, it is a naked
    /// directional position taken by accident.
    Equivalent {
        a: ContractId,
        b: ContractId,
        verified: bool,
    },
}

impl Relation {
    /// Every contract this relation constrains, in the relation's own order.
    pub fn members(&self) -> Vec<ContractId> {
        match self {
            Relation::Complement { contract } => vec![*contract],
            Relation::Exhaustive { members } => members.clone(),
            Relation::Monotone { ordered } => ordered.clone(),
            Relation::Implies {
                antecedent,
                consequent,
            } => vec![*antecedent, *consequent],
            Relation::Equivalent { a, b, .. } => vec![*a, *b],
        }
    }

    /// Enumerate every resolution the relation permits.
    ///
    /// This is what makes the payoff guarantee checkable rather than asserted.
    /// The solver prices a trade against the worst state in this list, so a
    /// relation that is wrong here produces a position that looks risk-free and
    /// is not — which is why the relation semantics live in one place and every
    /// fast path is priced through them instead of hardcoding "payoff is 100".
    pub fn resolution_states(&self) -> Vec<ResolutionState> {
        match self {
            Relation::Complement { contract } => vec![vec![*contract], Vec::new()],
            Relation::Exhaustive { members } => members.iter().map(|m| vec![*m]).collect(),
            // A ladder admits exactly one state per cut point: the first k rungs
            // resolve yes and the rest no. Any other combination would break the
            // containment that defines the ladder.
            Relation::Monotone { ordered } => (0..=ordered.len())
                .map(|cut| ordered[..cut].to_vec())
                .collect(),
            Relation::Implies {
                antecedent,
                consequent,
            } => vec![
                Vec::new(),
                vec![*consequent],
                vec![*consequent, *antecedent],
            ],
            Relation::Equivalent { a, b, .. } => vec![vec![*a, *b], Vec::new()],
        }
    }
}

/// A relation plus the identity and timing the solver needs to rank it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConstraintGroup {
    pub id: GroupId,
    pub relation: Relation,
    /// Derived from `relation` at construction; never set independently, so the
    /// two cannot drift apart.
    members: Vec<ContractId>,
    /// When the underlying event settles and the locked capital comes back.
    /// Ranking is meaningless without it, so it is required rather than optional.
    pub resolves_at_ms: u64,
}

impl ConstraintGroup {
    pub fn members(&self) -> &[ContractId] {
        &self.members
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistryError {
    /// A set, ladder, or pair that does not have enough distinct members to
    /// constrain anything.
    TooFewMembers { group: GroupId, got: usize },
    /// The same contract listed twice in one group. Almost always a config typo,
    /// and it would silently double-count depth on one book.
    DuplicateMember {
        group: GroupId,
        contract: ContractId,
    },
}

impl std::fmt::Display for RegistryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RegistryError::TooFewMembers { group, got } => {
                write!(f, "group {} has only {got} member(s)", group.0)
            }
            RegistryError::DuplicateMember { group, contract } => {
                write!(f, "group {} lists contract {} twice", group.0, contract.0)
            }
        }
    }
}

impl std::error::Error for RegistryError {}

/// Constraint groups plus the contract-to-group reverse index.
#[derive(Debug, Default)]
pub struct Registry {
    groups: Vec<ConstraintGroup>,
    by_contract: HashMap<ContractId, Vec<GroupId>>,
}

impl Registry {
    /// Build a registry from relations, assigning [`GroupId`]s in input order.
    ///
    /// Validation happens here rather than at query time because a malformed
    /// group is a configuration mistake, and the only useful moment to discover
    /// it is before the engine starts trading on it.
    pub fn new(
        relations: impl IntoIterator<Item = (Relation, u64)>,
    ) -> Result<Self, RegistryError> {
        let mut registry = Registry::default();
        for (relation, resolves_at_ms) in relations {
            let id = GroupId(u32::try_from(registry.groups.len()).unwrap_or(u32::MAX));
            let members = relation.members();
            if members.len() < 2 && !matches!(relation, Relation::Complement { .. }) {
                return Err(RegistryError::TooFewMembers {
                    group: id,
                    got: members.len(),
                });
            }
            for (i, contract) in members.iter().enumerate() {
                if members[..i].contains(contract) {
                    return Err(RegistryError::DuplicateMember {
                        group: id,
                        contract: *contract,
                    });
                }
            }
            for contract in &members {
                registry.by_contract.entry(*contract).or_default().push(id);
            }
            registry.groups.push(ConstraintGroup {
                id,
                relation,
                members,
                resolves_at_ms,
            });
        }
        Ok(registry)
    }

    pub fn groups(&self) -> &[ConstraintGroup] {
        &self.groups
    }

    pub fn group(&self, id: GroupId) -> Option<&ConstraintGroup> {
        self.groups.get(id.0 as usize)
    }

    /// Groups containing this contract. The reverse index that makes dirty
    /// marking a hash lookup instead of a scan over every group.
    pub fn groups_for(&self, contract: ContractId) -> &[GroupId] {
        self.by_contract
            .get(&contract)
            .map_or(&[][..], Vec::as_slice)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(id: u32) -> ContractId {
        ContractId(id)
    }

    #[test]
    fn reverse_index_finds_every_group_a_contract_belongs_to() {
        let registry = Registry::new([
            (
                Relation::Exhaustive {
                    members: vec![c(0), c(1), c(2)],
                },
                1_000,
            ),
            (Relation::Complement { contract: c(1) }, 1_000),
        ])
        .unwrap();
        assert_eq!(registry.groups_for(c(1)), &[GroupId(0), GroupId(1)]);
        assert_eq!(registry.groups_for(c(2)), &[GroupId(0)]);
        // A contract nobody constrains is not an error, it just has no groups.
        assert!(registry.groups_for(c(9)).is_empty());
    }

    #[test]
    fn malformed_groups_are_rejected_at_construction() {
        assert_eq!(
            Registry::new([(
                Relation::Exhaustive {
                    members: vec![c(0)]
                },
                1
            )])
            .err(),
            Some(RegistryError::TooFewMembers {
                group: GroupId(0),
                got: 1
            })
        );
        assert_eq!(
            Registry::new([(
                Relation::Exhaustive {
                    members: vec![c(0), c(1), c(0)]
                },
                1
            )])
            .err(),
            Some(RegistryError::DuplicateMember {
                group: GroupId(0),
                contract: c(0)
            })
        );
    }

    #[test]
    fn ladder_states_are_exactly_the_cut_points() {
        let states = Relation::Monotone {
            ordered: vec![c(0), c(1), c(2)],
        }
        .resolution_states();
        assert_eq!(
            states,
            vec![vec![], vec![c(0)], vec![c(0), c(1)], vec![c(0), c(1), c(2)],]
        );
        // A two-rung ladder and the implication it encodes agree exactly.
        let ladder = Relation::Monotone {
            ordered: vec![c(0), c(1)],
        }
        .resolution_states();
        let implies = Relation::Implies {
            antecedent: c(1),
            consequent: c(0),
        }
        .resolution_states();
        assert_eq!(ladder, implies);
    }
}
