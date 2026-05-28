//! Integration test harness: spawn a regtest `bitcoind` via
//! `corepc-node`, front it with a tiny in-process esplora-shaped HTTP
//! server, and run the `mdk-recovery` binary as a subprocess.
//!
//! The mock esplora exposes only the two endpoints `mdk-recovery`
//! actually hits (`GET /scripthash/:hash/utxo` and `POST /tx`),
//! translating each into the bitcoind RPC equivalent
//! (`scantxoutset`, `sendrawtransaction`). Scripts must be
//! pre-registered via [`MockEsplora::register_script`] so the mock
//! can map a queried scripthash back to its raw script.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use assert_cmd::Command as AssertCommand;
use axum::{
    Json, Router,
    body::Bytes,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
};
use bitcoin::hashes::{Hash, sha256};
use bitcoin::hex::DisplayHex;
use bitcoin::{Address, Amount, ScriptBuf, Txid};
use corepc_node::Node;
use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::Mutex;

// ---------------------------------------------------------------------------
// BitcoindRpc — minimal async JSON-RPC client.
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct BitcoindRpc {
    base_url: String,
    user: String,
    pass: String,
    http: reqwest::Client,
}

impl BitcoindRpc {
    fn from_node(node: &Node) -> Self {
        let cookie =
            std::fs::read_to_string(&node.params.cookie_file).expect("read bitcoind cookie file");
        let (user, pass) = cookie.split_once(':').expect("cookie format user:pass");
        Self {
            base_url: node.rpc_url(),
            user: user.to_string(),
            pass: pass.to_string(),
            http: reqwest::Client::new(),
        }
    }

    /// Call an RPC method. Routes through the default wallet so
    /// wallet-y RPCs (`getnewaddress`, `sendtoaddress`) work without
    /// callers picking a URL.
    pub async fn call(&self, method: &str, params: Value) -> Value {
        let url = format!("{}/wallet/default", self.base_url);
        let body = json!({
            "jsonrpc": "1.0",
            "id": "test",
            "method": method,
            "params": params,
        });
        let resp: Value = self
            .http
            .post(&url)
            .basic_auth(&self.user, Some(&self.pass))
            .json(&body)
            .send()
            .await
            .unwrap_or_else(|e| panic!("rpc {method} send: {e}"))
            .json()
            .await
            .unwrap_or_else(|e| panic!("rpc {method} decode: {e}"));
        if !resp["error"].is_null() {
            panic!("rpc {method} returned error: {}", resp["error"]);
        }
        resp["result"].clone()
    }
}

// ---------------------------------------------------------------------------
// TestBitcoind — bitcoind regtest with a funded "default" wallet.
// ---------------------------------------------------------------------------

pub struct TestBitcoind {
    _node: Node,
    pub rpc: BitcoindRpc,
}

impl TestBitcoind {
    /// Spawn bitcoind, create the default wallet (idempotent), and
    /// mine 101 blocks so the first coinbase is spendable.
    pub async fn new() -> Self {
        let exe = std::env::var("BITCOIND_EXE")
            .expect("BITCOIND_EXE must be set (use `nix develop` or the nix sandbox)");
        let node = Node::new(exe).expect("spawn bitcoind");
        let rpc = BitcoindRpc::from_node(&node);

        // corepc-node may already create "default"; treat duplicate as ok.
        let _ = rpc
            .http
            .post(rpc.base_url.to_string())
            .basic_auth(&rpc.user, Some(&rpc.pass))
            .json(&json!({
                "jsonrpc": "1.0",
                "id": "test",
                "method": "createwallet",
                "params": ["default"],
            }))
            .send()
            .await;

        let addr = rpc.call("getnewaddress", json!([])).await;
        let addr = addr.as_str().expect("getnewaddress -> string").to_string();
        rpc.call("generatetoaddress", json!([101, addr])).await;

        Self { _node: node, rpc }
    }

    /// Send `amount` to `addr` from the default wallet and mine one
    /// block to confirm. Returns the funding txid.
    pub async fn fund(&self, addr: &Address, amount: Amount) -> Txid {
        let txid: Value = self
            .rpc
            .call("sendtoaddress", json!([addr.to_string(), amount.to_btc()]))
            .await;
        let txid: Txid = txid
            .as_str()
            .expect("txid string")
            .parse()
            .expect("parse txid");
        self.mine(1).await;
        txid
    }

    /// Mine `n` blocks to a freshly-allocated address (we don't care
    /// where they go; coinbases land in the default wallet).
    pub async fn mine(&self, n: u32) {
        let addr = self.rpc.call("getnewaddress", json!([])).await;
        let addr = addr.as_str().expect("getnewaddress -> string").to_string();
        self.rpc.call("generatetoaddress", json!([n, addr])).await;
    }

    /// Sum of UTXO values at `script` according to bitcoind's UTXO
    /// set. Used by tests to assert the sweep moved funds.
    pub async fn balance_at(&self, script: &ScriptBuf) -> Amount {
        let desc = format!("raw({})", script.as_bytes().to_lower_hex_string());
        let result = self
            .rpc
            .call("scantxoutset", json!(["start", [{ "desc": desc }]]))
            .await;
        let total = result["total_amount"].as_f64().unwrap_or(0.0);
        Amount::from_btc(total).expect("amount from btc")
    }
}

// ---------------------------------------------------------------------------
// MockEsplora — axum router proxying two endpoints to bitcoind.
// ---------------------------------------------------------------------------

struct MockState {
    rpc: BitcoindRpc,
    /// Map from `sha256(spk)` (hex) to the raw script. Populated by
    /// the test before it expects the recovery binary to query that
    /// script. Anything not in the map returns an empty UTXO set.
    scripts: Mutex<HashMap<String, ScriptBuf>>,
    /// `scantxoutset` holds a process-wide bitcoind lock; concurrent
    /// callers see `Scan already in progress`. Serialise the calls
    /// from this mock so the recovery binary's `buffer_unordered`
    /// fetcher can fan out without the test failing on contention.
    scan_lock: Mutex<()>,
}

#[derive(Serialize)]
struct EsploraUtxo {
    txid: Txid,
    vout: u32,
    value: u64,
    status: EsploraStatus,
}

#[derive(Serialize)]
struct EsploraStatus {
    confirmed: bool,
    block_height: Option<u32>,
}

pub struct MockEsplora {
    state: Arc<MockState>,
    addr: SocketAddr,
}

impl MockEsplora {
    pub async fn start(rpc: BitcoindRpc) -> Self {
        let state = Arc::new(MockState {
            rpc,
            scripts: Mutex::new(HashMap::new()),
            scan_lock: Mutex::new(()),
        });

        let app = Router::new()
            .route("/scripthash/:hash/utxo", get(handle_utxo))
            .route("/tx", post(handle_broadcast))
            .with_state(state.clone());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");

        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });

        Self { state, addr }
    }

    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Tell the mock that subsequent `/scripthash/.../utxo` queries
    /// against `script` should be answered by walking the bitcoind
    /// UTXO set.
    pub async fn register_script(&self, script: ScriptBuf) {
        let hash = sha256::Hash::hash(script.as_bytes());
        self.state
            .scripts
            .lock()
            .await
            .insert(format!("{hash:x}"), script);
    }
}

async fn handle_utxo(
    State(state): State<Arc<MockState>>,
    Path(hash): Path<String>,
) -> Result<Json<Vec<EsploraUtxo>>, AppError> {
    let scripts = state.scripts.lock().await;
    let Some(spk) = scripts.get(&hash).cloned() else {
        return Ok(Json(Vec::new()));
    };
    drop(scripts);

    let desc = format!("raw({})", spk.as_bytes().to_lower_hex_string());
    let result = {
        let _guard = state.scan_lock.lock().await;
        state
            .rpc
            .call("scantxoutset", json!(["start", [{ "desc": desc }]]))
            .await
    };

    let mut out = Vec::new();
    for entry in result["unspents"].as_array().unwrap_or(&Vec::new()) {
        let txid: Txid = entry["txid"]
            .as_str()
            .ok_or(AppError::Bad("missing txid"))?
            .parse()
            .map_err(|_| AppError::Bad("invalid txid"))?;
        let vout = entry["vout"].as_u64().unwrap_or(0) as u32;
        let value_btc = entry["amount"].as_f64().unwrap_or(0.0);
        let value = (value_btc * 100_000_000.0).round() as u64;
        let block_height = entry["height"].as_u64().map(|h| h as u32);
        out.push(EsploraUtxo {
            txid,
            vout,
            value,
            status: EsploraStatus {
                confirmed: block_height.is_some(),
                block_height,
            },
        });
    }
    Ok(Json(out))
}

async fn handle_broadcast(
    State(state): State<Arc<MockState>>,
    body: Bytes,
) -> Result<String, AppError> {
    let hex = std::str::from_utf8(&body).map_err(|_| AppError::Bad("non-utf8 body"))?;
    let result = state.rpc.call("sendrawtransaction", json!([hex])).await;
    let txid = result
        .as_str()
        .ok_or(AppError::Bad("sendrawtransaction did not return string"))?;
    Ok(txid.to_string())
}

enum AppError {
    Bad(&'static str),
}

impl IntoResponse for AppError {
    fn into_response(self) -> axum::response::Response {
        match self {
            AppError::Bad(msg) => (StatusCode::BAD_REQUEST, msg).into_response(),
        }
    }
}

// ---------------------------------------------------------------------------
// recovery_command — `assert_cmd` builder pre-wired to the mock esplora.
// ---------------------------------------------------------------------------

/// Build an [`assert_cmd::Command`] for the `mdk-recovery` binary
/// with [`MDK_RECOVERY_ESPLORA_URL`] pointing at `esplora_url`.
/// Each test composes the subcommand and assertions itself; this
/// helper only handles binary discovery and the env-var wire-up.
pub fn recovery_command(esplora_url: &str) -> AssertCommand {
    let mut cmd = AssertCommand::cargo_bin("mdk-recovery").expect("locate mdk-recovery binary");
    cmd.env("MDK_RECOVERY_ESPLORA_URL", esplora_url);
    cmd
}
