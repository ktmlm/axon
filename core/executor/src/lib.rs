#![allow(clippy::uninlined_format_args, clippy::box_default)]

pub mod adapter;
#[cfg(test)]
mod debugger;
mod precompiles;
pub mod system_contract;
#[cfg(test)]
mod tests;
mod utils;
mod vm;

pub use crate::adapter::{AxonExecutorAdapter, MPTTrie, RocksTrieDB};
pub use crate::utils::{
    code_address, decode_revert_msg, logs_bloom, DefaultFeeAllocator, FeeInlet,
};

use std::collections::BTreeMap;
use std::iter::FromIterator;

use arc_swap::ArcSwap;
use evm::executor::stack::{MemoryStackState, PrecompileFn, StackExecutor, StackSubstateMetadata};
use evm::CreateScheme;

use common_merkle::TrieMerkle;
use protocol::codec::ProtocolCodec;
use protocol::traits::{ApplyBackend, Backend, Executor, ExecutorAdapter as Adapter};
use protocol::types::{
    data_gas_cost, Account, Config, ExecResp, Hasher, SignedTransaction, TransactionAction, TxResp,
    ValidatorExtend, GAS_CALL_TRANSACTION, GAS_CREATE_TRANSACTION, H160, NIL_DATA, RLP_NULL, U256,
};

use crate::precompiles::build_precompile_set;
use crate::system_contract::{system_contract_dispatch, NativeTokenContract, SystemContract};

lazy_static::lazy_static! {
    pub static ref FEE_ALLOCATOR: ArcSwap<Box<dyn FeeAllocate>> = ArcSwap::from_pointee(Box::new(DefaultFeeAllocator::default()));
}

pub trait FeeAllocate: Sync + Send {
    fn allocate(
        &self,
        block_number: U256,
        fee_collect: U256,
        proposer: H160,
        validators: &[ValidatorExtend],
    ) -> Vec<FeeInlet>;
}

#[derive(Default)]
pub struct AxonExecutor;

impl Executor for AxonExecutor {
    // Used for query data API, this function will not modify the world state.
    fn call<B: Backend>(
        &self,
        backend: &B,
        gas_limit: u64,
        from: Option<H160>,
        to: Option<H160>,
        value: U256,
        data: Vec<u8>,
    ) -> TxResp {
        let config = Config::london();
        let metadata = StackSubstateMetadata::new(gas_limit, &config);
        let state = MemoryStackState::new(metadata, backend);
        let precompiles = build_precompile_set();
        let mut executor = StackExecutor::new_with_precompiles(state, &config, &precompiles);

        let base_gas = if to.is_some() {
            GAS_CALL_TRANSACTION + data_gas_cost(&data)
        } else {
            GAS_CREATE_TRANSACTION + GAS_CALL_TRANSACTION + data_gas_cost(&data)
        };

        let (exit, res) = if let Some(addr) = &to {
            executor.transact_call(
                from.unwrap_or_default(),
                *addr,
                value,
                data,
                gas_limit,
                Vec::new(),
            )
        } else {
            executor.transact_create(from.unwrap_or_default(), value, data, gas_limit, Vec::new())
        };

        let used_gas = executor.used_gas() + base_gas;

        TxResp {
            exit_reason:  exit,
            ret:          res,
            remain_gas:   executor.gas(),
            gas_used:     used_gas,
            fee_cost:     backend
                .gas_price()
                .checked_mul(used_gas.into())
                .unwrap_or(U256::max_value()),
            logs:         vec![],
            code_address: if to.is_none() {
                Some(
                    executor
                        .create_address(CreateScheme::Legacy {
                            caller: from.unwrap_or_default(),
                        })
                        .into(),
                )
            } else {
                None
            },
            removed:      false,
        }
    }

    // Function execute returns exit_reason, ret_data and remain_gas.
    fn exec<B: Backend + ApplyBackend + Adapter>(
        &self,
        backend: &mut B,
        txs: &[SignedTransaction],
        validators: &[ValidatorExtend],
    ) -> ExecResp {
        let txs_len = txs.len();
        let block_number = backend.block_number();
        let mut res = Vec::with_capacity(txs_len);
        let mut hashes = Vec::with_capacity(txs_len);
        let (mut gas, mut fee) = (0u64, U256::zero());
        let precompiles = build_precompile_set();
        let config = Config::london();

        for tx in txs.iter() {
            backend.set_gas_price(tx.transaction.unsigned.gas_price());
            backend.set_origin(tx.sender);

            // Execute a transaction, if system contract dispatch return None, means the
            // transaction called EVM
            let mut r = system_contract_dispatch(backend, tx)
                .unwrap_or_else(|| Self::evm_exec(backend, &config, &precompiles, tx));

            r.logs = backend.get_logs();
            gas += r.gas_used;
            fee = fee.checked_add(r.fee_cost).unwrap_or(U256::max_value());

            hashes.push(Hasher::digest(&r.ret));
            res.push(r);
        }

        // Allocate collected fee for validators
        if !block_number.is_zero() {
            let alloc =
                (*FEE_ALLOCATOR)
                    .load()
                    .allocate(block_number, fee, backend.origin(), validators);

            for i in alloc.iter() {
                if !i.amount.is_zero() {
                    let mut account = backend.get_account(&i.address);
                    account.balance += i.amount;
                    backend.save_account(&i.address, &account);
                }
            }
        }

        // commit changes by all txs included in this block only once
        let new_state_root = backend.commit();

        ExecResp {
            state_root:   new_state_root,
            receipt_root: TrieMerkle::from_iter(hashes.iter().enumerate())
                .root_hash()
                .unwrap_or_default(),
            gas_used:     gas,
            tx_resp:      res,
        }
    }

    fn get_account<B: Backend + Adapter>(&self, backend: &B, address: &H160) -> Account {
        match backend.get(address.as_bytes()) {
            Some(bytes) => Account::decode(bytes).unwrap(),
            None => Account {
                nonce:        Default::default(),
                balance:      Default::default(),
                storage_root: RLP_NULL,
                code_hash:    NIL_DATA,
            },
        }
    }
}

impl AxonExecutor {
    pub fn evm_exec<B: Backend + ApplyBackend + Adapter>(
        backend: &mut B,
        config: &Config,
        precompiles: &BTreeMap<H160, PrecompileFn>,
        tx: &SignedTransaction,
    ) -> TxResp {
        // Deduct pre-pay gas
        let sender = tx.sender;
        let tx_gas_price = backend.gas_price();
        let gas_limit = tx.transaction.unsigned.gas_limit();
        let prepay_gas = tx_gas_price * gas_limit;

        let mut account = backend.get_account(&sender);
        account.balance = account.balance.saturating_sub(prepay_gas);
        backend.save_account(&sender, &account);

        let old_nonce = backend.basic(tx.sender).nonce;

        let metadata = StackSubstateMetadata::new(gas_limit.as_u64(), config);
        let mut executor = StackExecutor::new_with_precompiles(
            MemoryStackState::new(metadata, backend),
            config,
            precompiles,
        );

        let access_list = tx
            .transaction
            .unsigned
            .access_list()
            .into_iter()
            .map(|x| (x.address, x.storage_keys))
            .collect::<Vec<_>>();

        let (exit, res) = match tx.transaction.unsigned.action() {
            TransactionAction::Call(addr) => executor.transact_call(
                tx.sender,
                *addr,
                *tx.transaction.unsigned.value(),
                tx.transaction.unsigned.data().to_vec(),
                gas_limit.as_u64(),
                access_list,
            ),
            TransactionAction::Create => executor.transact_create(
                tx.sender,
                *tx.transaction.unsigned.value(),
                tx.transaction.unsigned.data().to_vec(),
                gas_limit.as_u64(),
                access_list,
            ),
        };

        let remained_gas = executor.gas();
        let used_gas = executor.used_gas();

        let code_addr = if tx.transaction.unsigned.action() == &TransactionAction::Create
            && exit.is_succeed()
        {
            Some(code_address(&tx.sender, &old_nonce))
        } else {
            None
        };

        if exit.is_succeed() {
            let (values, logs) = executor.into_state().deconstruct();
            backend.apply(values, logs, true);
        }

        let mut account = backend.get_account(&tx.sender);
        account.nonce = old_nonce + U256::one();

        // Add remain gas
        if remained_gas != 0 {
            let remain_gas = U256::from(remained_gas)
                .checked_mul(tx_gas_price)
                .unwrap_or_else(U256::max_value);
            account.balance = account
                .balance
                .checked_add(remain_gas)
                .unwrap_or_else(U256::max_value);
        }

        backend.save_account(&tx.sender, &account);

        TxResp {
            exit_reason:  exit,
            ret:          res,
            remain_gas:   remained_gas,
            gas_used:     used_gas,
            fee_cost:     tx_gas_price
                .checked_mul(used_gas.into())
                .unwrap_or(U256::max_value()),
            logs:         vec![],
            code_address: code_addr,
            removed:      false,
        }
    }
}

pub fn is_call_system_script(action: &TransactionAction) -> bool {
    match action {
        TransactionAction::Call(addr) => addr == &NativeTokenContract::ADDRESS,
        TransactionAction::Create => false,
    }
}

pub fn is_transaction_call(action: &TransactionAction, addr: &H160) -> bool {
    action == &TransactionAction::Call(*addr)
}
