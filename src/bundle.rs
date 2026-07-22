use crate::{ParallelState, utils::fork_join_util};
use parking_lot::Mutex;
use revm::DatabaseRef;
use revm_database::{BundleState, TransitionState, states::bundle_state::BundleRetention};
use revm_primitives::Address;
use std::{
    cell::UnsafeCell,
    sync::atomic::{AtomicUsize, Ordering},
};

/// A vector supporting concurrent writes to caller-guaranteed disjoint indices.
struct DisjointVec<T>(UnsafeCell<Vec<T>>);

// SAFETY: all use sites partition indices through `fork_join_util`.
unsafe impl<T: Send> Sync for DisjointVec<T> {}

impl<T> DisjointVec<T> {
    fn new(values: Vec<T>) -> Self {
        Self(UnsafeCell::new(values))
    }

    /// SAFETY: no other thread may access `index` concurrently.
    unsafe fn set(&self, index: usize, value: T) {
        unsafe { (&mut (*self.0.get()))[index] = value };
    }

    fn into_inner(self) -> Vec<T> {
        self.0.into_inner()
    }
}

/// Parallel transition application for an initially empty bundle.
pub trait ParallelBundleState {
    /// Apply transitions and create reverts in parallel.
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
        if !self.state.is_empty() {
            self.apply_transitions_and_create_reverts(transitions, retention);
            return;
        }

        let include_reverts = retention.includes_reverts();
        let revert_capacity = if include_reverts { transitions.transitions.len() } else { 0 };
        let transitions = transitions.transitions;
        let addresses: Vec<Address> = transitions.keys().copied().collect();
        let reverts = DisjointVec::new(vec![None; revert_capacity]);
        let bundle = DisjointVec::new(vec![None; transitions.len()]);
        let state_size = AtomicUsize::new(0);
        let reverts_size = AtomicUsize::new(0);
        let contracts = Mutex::new(revm_primitives::HashMap::default());

        fork_join_util(transitions.len(), None, |start, end, _| {
            for (position, address) in addresses.iter().enumerate().take(end).skip(start) {
                let transition = transitions.get(address).cloned().expect("address comes from map");
                if let Some((hash, bytecode)) = transition.has_new_contract() {
                    contracts.lock().insert(hash, bytecode.clone());
                }
                let present = transition.present_bundle_account();
                if let Some(revert) = transition.create_revert() {
                    state_size.fetch_add(present.size_hint(), Ordering::Relaxed);
                    unsafe { bundle.set(position, Some((*address, present))) };
                    if include_reverts {
                        reverts_size.fetch_add(revert.size_hint(), Ordering::Relaxed);
                        unsafe { reverts.set(position, Some((*address, revert))) };
                    }
                }
            }
        });

        self.state_size = state_size.load(Ordering::Acquire);
        self.reverts_size = reverts_size.load(Ordering::Acquire);
        self.state.reserve(transitions.len());
        self.state.extend(bundle.into_inner().into_iter().flatten());
        self.reverts.push(reverts.into_inner().into_iter().flatten().collect());
        self.contracts = contracts.into_inner();
    }
}

/// Parallel bundle extraction from `ParallelState`.
pub trait ParallelTakeBundle {
    /// Take the accumulated bundle using parallel transition application.
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
