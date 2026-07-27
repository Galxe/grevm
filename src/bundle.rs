use crate::ParallelState;
use rayon::prelude::{IntoParallelIterator, ParallelIterator};
use revm::DatabaseRef;
use revm_database::{
    AccountRevert, BundleAccount, BundleState, TransitionState,
    states::bundle_state::BundleRetention,
};
use revm_primitives::{Address, B256};
use revm_state::Bytecode;

struct ProcessedAccount {
    address: Address,
    present: BundleAccount,
    revert: Option<AccountRevert>,
    state_size: usize,
    revert_size: usize,
}

struct ProcessedTransition {
    contract: Option<(B256, Bytecode)>,
    account: Option<ProcessedAccount>,
}

/// Parallel transition application for an initially empty [`BundleState`].
///
/// The implementation has two phases: it derives immutable per-account bundle/revert data in
/// parallel, then updates the destination maps and accounting fields serially. Keeping mutation
/// in the second phase avoids aliased access to the bundle while preserving revm's transition
/// order and accounting semantics.
pub trait ParallelBundleState {
    /// Applies transitions and creates reverts, using parallel preparation when `self` is empty.
    ///
    /// If `self` already contains state, contracts, or reverts, this delegates to revm's canonical
    /// occupied-entry merge because existing entries may need to be combined with the new
    /// transitions.
    fn parallel_apply_transitions_and_create_reverts(
        &mut self,
        transitions: TransitionState,
        retention: BundleRetention,
    );
}

impl ParallelBundleState for BundleState {
    fn parallel_apply_transitions_and_create_reverts(
        &mut self,
        transitions: TransitionState,
        retention: BundleRetention,
    ) {
        if !self.state.is_empty() || !self.contracts.is_empty() || !self.reverts.is_empty() {
            self.apply_transitions_and_create_reverts(transitions, retention);
            return;
        }

        let include_reverts = retention.includes_reverts();
        let transitions: Vec<_> = transitions.transitions.into_iter().collect();
        let transition_count = transitions.len();
        // This phase is pure with respect to `self`; each transition produces an independent
        // value that can be prepared safely on any Rayon worker.
        let processed: Vec<_> = transitions
            .into_par_iter()
            .map(|(address, transition)| {
                let contract =
                    transition.has_new_contract().map(|(hash, code)| (hash, code.clone()));
                let present = transition.present_bundle_account();
                let account = transition.create_revert().map(|revert| {
                    let (revert, revert_size) = if include_reverts {
                        let size = revert.size_hint();
                        (Some(revert), size)
                    } else {
                        (None, 0)
                    };
                    ProcessedAccount {
                        address,
                        state_size: present.size_hint(),
                        revert_size,
                        present,
                        revert,
                    }
                });
                ProcessedTransition { contract, account }
            })
            .collect();

        let revert_capacity = if include_reverts { transition_count } else { 0 };
        let mut reverts = Vec::with_capacity(revert_capacity);
        self.state.reserve(transition_count);
        // Mutate the bundle only after parallel preparation has finished. `collect` preserves the
        // indexed input order, so this phase matches the transition order used by revm.
        for ProcessedTransition { contract, account } in processed {
            if let Some((hash, code)) = contract {
                self.contracts.insert(hash, code);
            }
            if let Some(account) = account {
                self.state_size += account.state_size;
                self.reverts_size += account.revert_size;
                self.state.insert(account.address, account.present);
                if let Some(revert) = account.revert {
                    reverts.push((account.address, revert));
                }
            }
        }
        self.reverts.push(reverts);
    }
}

/// Parallel bundle extraction from `ParallelState`.
pub trait ParallelTakeBundle {
    /// Finalizes pending transitions and takes the accumulated bundle.
    ///
    /// Pending transitions are drained and the returned bundle is replaced by an empty bundle in
    /// `ParallelState`.
    fn parallel_take_bundle(&mut self, retention: BundleRetention) -> BundleState;
}

impl<DB: DatabaseRef> ParallelTakeBundle for ParallelState<DB> {
    fn parallel_take_bundle(&mut self, retention: BundleRetention) -> BundleState {
        if let Some(transitions) = self.transition_state.as_mut().map(TransitionState::take) {
            self.bundle_state.parallel_apply_transitions_and_create_reverts(transitions, retention);
        }
        self.take_bundle()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use revm_database::{AccountStatus, TransitionAccount};
    use revm_primitives::{AddressMap, Bytes};
    use revm_state::AccountInfo;

    fn transitions(count: usize) -> TransitionState {
        TransitionState {
            transitions: (0..count)
                .map(|index| {
                    (
                        Address::from([index as u8; 20]),
                        TransitionAccount {
                            info: Some(AccountInfo { nonce: index as u64, ..Default::default() }),
                            status: AccountStatus::InMemoryChange,
                            previous_status: AccountStatus::LoadedNotExisting,
                            ..Default::default()
                        },
                    )
                })
                .collect::<AddressMap<_>>(),
        }
    }

    #[test]
    fn parallel_bundle_matches_revm_for_empty_bundle() {
        for include_reverts in [false, true] {
            let retention = || {
                if include_reverts { BundleRetention::Reverts } else { BundleRetention::PlainState }
            };
            let transitions = transitions(128);
            let mut expected = BundleState::default();
            expected.apply_transitions_and_create_reverts(transitions.clone(), retention());

            let mut actual = BundleState::default();
            actual.parallel_apply_transitions_and_create_reverts(transitions, retention());

            assert_eq!(actual, expected);
        }
    }

    #[test]
    fn existing_bundle_data_uses_revm_merge_semantics() {
        let mut expected = BundleState::default();
        expected
            .contracts
            .insert(B256::from([0x42; 32]), Bytecode::new_raw(Bytes::from_static(&[0x00])));
        let mut actual = expected.clone();
        let transitions = transitions(32);

        expected
            .apply_transitions_and_create_reverts(transitions.clone(), BundleRetention::Reverts);
        actual.parallel_apply_transitions_and_create_reverts(transitions, BundleRetention::Reverts);

        assert_eq!(actual, expected);
    }
}
