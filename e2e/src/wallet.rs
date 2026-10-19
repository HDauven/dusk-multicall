//! The local chain's genesis accounts and the Moonlight transactions the
//! checks send, built and signed with dusk-core 1.7.0.
use dusk_bytes::Serializable;
use dusk_core::abi::ContractId;
use dusk_core::signatures::bls::{PublicKey, SecretKey};
use dusk_core::transfer::data::{ContractCall, ContractDeploy, TransactionData};
use dusk_core::transfer::moonlight::AccountData;
use dusk_core::transfer::{TRANSFER_CONTRACT, Transaction};
use rand_chacha::ChaCha12Rng;
use rand_chacha::rand_core::SeedableRng;
use serde_json::Value;
use sha2::{Digest, Sha256, Sha512};

use crate::node::{Node, Refusal};
use crate::{decode, encode};

pub struct Account {
    pub sk: SecretKey,
    pub pk: PublicKey,
}

impl Account {
    /// Profile `index` of a recovery phrase, as wallet-core 1.7 derives it
    /// (`wallet-core/src/keys/mod.rs`): the BIP-39 seed, then
    /// SHA-256(seed ‖ index as u64 LE ‖ "SK") seeds ChaCha12 for the key.
    pub fn from_phrase(phrase: &str, index: u8) -> Self {
        let words = phrase.split_whitespace().collect::<Vec<_>>().join(" ");
        let mut seed = [0; 64];
        pbkdf2::pbkdf2_hmac::<Sha512>(words.as_bytes(), b"mnemonic", 2048, &mut seed);
        let mut hash = Sha256::new();
        hash.update(seed);
        hash.update(u64::from(index).to_le_bytes());
        hash.update(b"SK");
        let sk = SecretKey::random(&mut ChaCha12Rng::from_seed(hash.finalize().into()));
        let pk = PublicKey::from(&sk);
        Self { sk, pk }
    }

    pub fn address(&self) -> String {
        bs58::encode(self.pk.to_bytes()).into_string()
    }

    /// Owner bytes for a deployment: the 96-byte public key.
    pub fn owner(&self) -> Vec<u8> {
        self.pk.to_bytes().to_vec()
    }
}

pub struct Receipt {
    pub err: Option<String>,
    pub gas_spent: u64,
    pub gas_limit: u64,
    pub height: u64,
    pub contract_id: Option<String>,
}

pub struct Chain {
    pub node: Node,
    pub chain_id: u8,
    pub deploy_price: u64,
}

impl Chain {
    pub fn account(&self, account: &Account) -> AccountData {
        let reply = self.node.query(&TRANSFER_CONTRACT, "account", &encode(&account.pk));
        decode(&reply.expect("account query"))
    }

    pub fn deploy(
        &self,
        from: &Account,
        wasm: &[u8],
        owner: Vec<u8>,
        nonce: u64,
        gas_limit: u64,
    ) -> Result<Receipt, Refusal> {
        let deploy = ContractDeploy::new(wasm.to_vec(), owner, None, nonce);
        self.send(from, gas_limit, self.deploy_price, TransactionData::Deploy(deploy))
    }

    pub fn call(
        &self,
        from: &Account,
        contract: ContractId,
        fn_name: &str,
        fn_args: Vec<u8>,
        gas_limit: u64,
    ) -> Result<Receipt, Refusal> {
        let call = ContractCall { contract, fn_name: fn_name.into(), fn_args };
        self.send(from, gas_limit, 1, TransactionData::Call(call))
    }

    fn send(
        &self,
        from: &Account,
        gas_limit: u64,
        gas_price: u64,
        data: TransactionData,
    ) -> Result<Receipt, Refusal> {
        let nonce = self.account(from).nonce + 1;
        let tx = Transaction::moonlight(
            &from.sk,
            None,
            0,
            0,
            gas_limit,
            gas_price,
            nonce,
            self.chain_id,
            Some(data),
        )
        .expect("transaction");
        self.node.propagate(&tx.to_var_bytes())?;
        let r = self.node.receipt(&hex::encode(tx.hash().to_bytes()));
        let number = |v: &Value| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok()));
        Ok(Receipt {
            err: r["err"].as_str().map(String::from),
            gas_spent: number(&r["gasSpent"]).expect("gasSpent"),
            gas_limit: number(&r["tx"]["gasLimit"]).expect("gasLimit"),
            height: number(&r["blockHeight"]).expect("blockHeight"),
            contract_id: r["tx"]["contractId"].as_str().map(String::from),
        })
    }
}
