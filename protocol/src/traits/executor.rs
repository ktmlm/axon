pub use evm::backend::{ApplyBackend, Backend};

use crate::types::{
    Account, Bytes, ExecResp, ExecutorContext, Log, MerkleRoot, SignedTransaction, TxResp,
    ValidatorExtend, H160, U256,
};

pub trait ExecutorAdapter {
    fn set_origin(&mut self, origin: H160);

    fn set_gas_price(&mut self, gas_price: U256);

    fn get_logs(&mut self) -> Vec<Log>;

    fn commit(&mut self) -> MerkleRoot;

    fn get(&self, key: &[u8]) -> Option<Bytes>;

    fn get_ctx(&self) -> ExecutorContext;

    fn get_account(&self, address: &H160) -> Account;

    fn save_account(&mut self, address: &H160, account: &Account);
}

pub trait Executor: Send + Sync {
    fn call<B: Backend>(
        &self,
        backend: &B,
        gas_limit: u64,
        from: Option<H160>,
        to: Option<H160>,
        value: U256,
        data: Vec<u8>,
    ) -> TxResp;

    fn exec<B: Backend + ApplyBackend + ExecutorAdapter>(
        &self,
        backend: &mut B,
        txs: &[SignedTransaction],
        validators: &[ValidatorExtend],
    ) -> ExecResp;

    fn get_account<B: Backend + ExecutorAdapter>(&self, backend: &B, address: &H160) -> Account;
}
