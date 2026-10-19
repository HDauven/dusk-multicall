//! A Rusk node over HTTP: contract queries, GraphQL, node info, transaction
//! propagation and driver uploads.
use std::io::Read;
use std::thread::sleep;
use std::time::{Duration, Instant};

use dusk_multicall::ContractId;
use serde_json::Value;

#[derive(Clone)]
pub struct Node {
    url: String,
    agent: ureq::Agent,
}

/// Rusk reads a body as bytes, and answers in bytes, only with this content
/// type; otherwise it reads text (`0x`-prefixed hex or UTF-8) and answers hex.
const BINARY: [(&str, &str); 1] = [("Content-Type", "application/octet-stream")];

/// An HTTP answer other than 200.
#[derive(Debug, Clone)]
pub struct Refusal {
    pub status: u16,
    pub body: String,
}

impl Node {
    pub fn new(url: &str) -> Self {
        let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(120)).build();
        Self { url: url.trim_end_matches('/').into(), agent }
    }

    fn post(&self, path: &str, body: &[u8], headers: &[(&str, &str)]) -> (u16, Vec<u8>) {
        let mut request = self.agent.post(&format!("{}/{path}", self.url));
        for (name, value) in headers {
            request = request.set(name, value);
        }
        let response = match request.send_bytes(body) {
            Ok(response) => response,
            Err(ureq::Error::Status(_, response)) => response,
            Err(error) => panic!("node unreachable: {error}"),
        };
        let status = response.status();
        let mut bytes = Vec::new();
        response.into_reader().read_to_end(&mut bytes).expect("response body");
        (status, bytes)
    }

    fn answer(status: u16, body: Vec<u8>) -> Result<Vec<u8>, Refusal> {
        match status {
            200 | 202 => Ok(body),
            _ => Err(Refusal { status, body: String::from_utf8_lossy(&body).into() }),
        }
    }

    /// `POST /on/contracts:<id>/<fn>` with raw argument bytes.
    pub fn query(
        &self,
        contract: &ContractId,
        fn_name: &str,
        arg: &[u8],
    ) -> Result<Vec<u8>, Refusal> {
        let (status, body) =
            self.post(&contract_path("contracts", contract, fn_name), arg, &BINARY);
        Self::answer(status, body)
    }

    /// The same as a feeder query: the node streams back whatever the call
    /// feeds and drops what it returns.
    pub fn feeder_query(
        &self,
        contract: &ContractId,
        fn_name: &str,
        arg: &[u8],
    ) -> Result<Vec<u8>, Refusal> {
        let headers = [BINARY[0], ("Rusk-Feeder", "1")];
        let (status, body) =
            self.post(&contract_path("contracts", contract, fn_name), arg, &headers);
        Self::answer(status, body)
    }

    /// The same as text, the way plain curl sends it: the node decodes a
    /// `0x`-prefixed body as hex and answers in hex.
    pub fn query_text(
        &self,
        contract: &ContractId,
        fn_name: &str,
        body: &str,
    ) -> Result<String, Refusal> {
        let (status, body) =
            self.post(&contract_path("contracts", contract, fn_name), body.as_bytes(), &[]);
        Self::answer(status, body).map(|body| String::from_utf8_lossy(&body).into())
    }

    /// The same with a JSON argument, which the node encodes and decodes
    /// with the contract's uploaded data driver.
    pub fn query_json(
        &self,
        contract: &ContractId,
        fn_name: &str,
        json: &str,
    ) -> Result<Value, Refusal> {
        let headers = [("Content-Type", "application/json")];
        let (status, body) =
            self.post(&contract_path("contracts", contract, fn_name), json.as_bytes(), &headers);
        Self::answer(status, body).map(|body| serde_json::from_slice(&body).expect("JSON reply"))
    }

    pub fn graphql(&self, query: &str) -> Value {
        let (status, body) = self.post("on/graphql/query", query.as_bytes(), &[]);
        assert_eq!(status, 200, "GraphQL {query}: {}", String::from_utf8_lossy(&body));
        let value: Value = serde_json::from_slice(&body).expect("GraphQL JSON");
        value.get("data").cloned().unwrap_or(value)
    }

    pub fn tip(&self) -> u64 {
        self.graphql("{block(height:-1){header{height}}}")["block"]["header"]["height"]
            .as_u64()
            .expect("tip height")
    }

    pub fn info(&self) -> Value {
        let (status, body) = self.post("on/node/info", &[], &[]);
        assert_eq!(status, 200, "node info");
        serde_json::from_slice(&body).expect("node info JSON")
    }

    pub fn propagate(&self, tx: &[u8]) -> Result<(), Refusal> {
        let (status, body) = self.post("on/transactions/propagate", tx, &BINARY);
        Self::answer(status, body).map(|_| ())
    }

    /// Waits for a transaction to land in a block.
    pub fn receipt(&self, hash: &str) -> Value {
        let query = format!(
            "{{tx(hash:\"{hash}\"){{id err gasSpent blockHeight tx{{gasLimit gasPrice isDeploy contractId}}}}}}"
        );
        let start = Instant::now();
        loop {
            let tx = self.graphql(&query)["tx"].clone();
            if !tx.is_null() {
                return tx;
            }
            assert!(start.elapsed() < Duration::from_secs(120), "no receipt for {hash}");
            sleep(Duration::from_millis(250));
        }
    }

    pub fn upload_driver(
        &self,
        contract: &ContractId,
        signature: &str,
        driver: &[u8],
    ) -> Result<String, Refusal> {
        let path = format!("on/contract:{}/upload_driver", hex::encode(contract.as_bytes()));
        let (status, body) = self.post(&path, driver, &[BINARY[0], ("sign", signature)]);
        Self::answer(status, body).map(|body| String::from_utf8_lossy(&body).into())
    }

    pub fn metadata(&self, contract: &ContractId) -> Value {
        let path = format!("on/contract:{}/metadata", hex::encode(contract.as_bytes()));
        let (status, body) = self.post(&path, &[], &[]);
        assert_eq!(status, 200, "metadata: {}", String::from_utf8_lossy(&body));
        serde_json::from_slice(&body).expect("metadata JSON")
    }
}

fn contract_path(target: &str, contract: &ContractId, topic: &str) -> String {
    format!("on/{target}:{}/{topic}", hex::encode(contract.as_bytes()))
}
