use crate::storage::CachedStorageData;
use revm::db::BundleState;
use revm_primitives::{
    db::DatabaseRef, Account, AccountStatus, Address, ExecutionResult, ResultAndState, U256,
};

pub trait AsyncCommit {
    fn commit(&mut self, result_and_state: ResultAndState, cache: &CachedStorageData);

    fn push(&mut self, result_and_state: ResultAndState);

    fn get_results(&mut self) -> &mut Vec<ResultAndState>;
}

pub struct StateAsyncCommit {
    coinbase: Address,
    miner_account: Option<Account>,
    results: Vec<ResultAndState>,
}

impl StateAsyncCommit {
    pub fn new<DB: DatabaseRef>(coinbase: Address, db: &DB) -> Self {
        let miner_account = match db.basic_ref(coinbase.clone()) {
            Ok(miner) => miner.map(|info| Account {
                info,
                storage: Default::default(),
                status: Default::default(),
            }),
            Err(_) => {
                panic!("Failed to get miner account")
            }
        };
        Self { coinbase, miner_account, results: vec![] }
    }

    pub fn take_result(&mut self) -> Vec<ResultAndState> {
        std::mem::take(&mut self.results)
    }
}

impl AsyncCommit for StateAsyncCommit {
    fn commit(&mut self, mut result_and_state: ResultAndState, _cache: &CachedStorageData) {
        if self.miner_account.is_none() {
            let mut miner = Account::default();
            miner.status = AccountStatus::Touched | AccountStatus::LoadedAsNotExisting;
            self.miner_account = Some(miner);
        } else if let Some(miner) = &mut self.miner_account {
            miner.status = AccountStatus::Touched;
        }
        let prev_miner = self.miner_account.as_mut().unwrap();
        let miner_account: &mut Account =
            result_and_state.state.entry(self.coinbase).or_insert_with(|| prev_miner.clone());
        miner_account.status = prev_miner.status;

        let new_balance =
            miner_account.info.balance.saturating_add(U256::from(result_and_state.rewards));
        miner_account.info.balance = new_balance;
        prev_miner.info = miner_account.info.clone();
        self.results.push(result_and_state);
    }

    fn push(&mut self, result_and_state: ResultAndState) {
        self.results.push(result_and_state);
    }

    fn get_results(&mut self) -> &mut Vec<ResultAndState> {
        &mut self.results
    }
}

pub struct TransitionAsyncCommit {
    coinbase: Address,
    miner_account: Option<Account>,
    results: Vec<ResultAndState>,
}

impl TransitionAsyncCommit {
    fn take_result(&mut self) -> (Vec<ExecutionResult>, BundleState) {
        todo!()
    }
}

impl AsyncCommit for TransitionAsyncCommit {
    fn commit(&mut self, result_and_state: ResultAndState, cache: &CachedStorageData) {
        todo!()
    }

    fn push(&mut self, result_and_state: ResultAndState) {
        todo!()
    }

    fn get_results(&mut self) -> &mut Vec<ResultAndState> {
        todo!()
    }
}
