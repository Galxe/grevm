//! Stable, revm-version-independent on-disk schema for replaying real Ethereum mainnet blocks
//! through grevm.
//!
//! The previous mainnet replay harness serialized revm's own `EnvWithHandlerCfg` / `TxEnv` /
//! `PlainAccount` types directly, so every revm upgrade silently broke the fixture format (and
//! it was eventually deleted in the v27 upgrade). This module instead defines grevm's *own*
//! plain serde structs ([`BlockFixture`], [`TxFixture`], [`AccountFixture`]) and converts them
//! to/from revm types through small, explicit functions. revm can change its internal layout
//! freely; only the converters here need to follow, and the JSON fixtures keep working.
//!
//! A fixture lives in `test_data/mainnet_blocks/<number>/`:
//! - `block.json`     — [`BlockFixture`]: block environment + chain id + hardfork name.
//! - `txs.json`       — `Vec<`[`TxFixture`]`>`: the block's transactions, in order.
//! - `pre_state.json` — `BTreeMap<Address,`[`AccountFixture`]`>`: the block's opening state (every
//!   account/slot the transactions read, merged "first-seen wins" across the block).
//!
//! The `fetch_block` binary (`src/bin/fetch_block.rs`, built with `--features tools`) populates
//! these from a JSON-RPC node via `debug_traceBlockByNumber` + `prestateTracer`.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{self, BufReader, BufWriter},
    path::{Path, PathBuf},
    str::FromStr,
};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use revm::context_interface::block::BlobExcessGasAndPrice;
use revm_context::{
    BlockEnv, CfgEnv, TxEnv,
    either::Either,
    transaction::{AccessList, AccessListItem, Authorization, SignedAuthorization},
};
use revm_database::PlainAccount;
use revm_primitives::{
    Address, B256, Bytes, HashMap, KECCAK_EMPTY, TxKind, U256, hardfork::SpecId,
};
use revm_state::{AccountInfo, Bytecode};

use super::storage::InMemoryDB;

/// Default location of the single-block fixtures, relative to the crate root.
pub const DEFAULT_MAINNET_BLOCKS_DIR: &str = "test_data/mainnet_blocks";

/// Default location of the merged "big block" fixtures, relative to the crate root.
pub const DEFAULT_CONTINUOUS_BLOCKS_DIR: &str = "test_data/con_eth_blocks";

/// The opening state of a block: account address -> account snapshot.
pub type PreState = BTreeMap<Address, AccountFixture>;

/// Block-level execution environment, decoupled from revm's `BlockEnv`/`CfgEnv`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockFixture {
    /// Block number (height).
    pub number: u64,
    /// Block beneficiary / coinbase / miner.
    pub coinbase: Address,
    /// Block timestamp (seconds since UNIX epoch).
    pub timestamp: u64,
    /// Block gas limit.
    pub gas_limit: u64,
    /// Base fee per gas (EIP-1559); `0` before London.
    pub basefee: u64,
    /// Difficulty; `0` after the merge (replaced by `prevrandao`).
    #[serde(default)]
    pub difficulty: U256,
    /// `mix_hash` / `prevrandao` (EIP-4399), if present.
    #[serde(default)]
    pub prevrandao: Option<B256>,
    /// Excess blob gas (EIP-4844), if present.
    #[serde(default)]
    pub excess_blob_gas: Option<u64>,
    /// Chain id (1 for mainnet).
    pub chain_id: u64,
    /// Hardfork name, parsed via [`SpecId::from_str`] (e.g. `"Prague"`). Falls back to
    /// [`SpecId::PRAGUE`] if unrecognized.
    pub spec_id: String,
    /// Recent block hashes for the `BLOCKHASH` opcode (block number -> hash), normally the last
    /// 256 ancestors. Not part of any account's state, so `prestateTracer` cannot provide them;
    /// the fetcher captures them separately. Empty if not captured.
    #[serde(default)]
    pub block_hashes: BTreeMap<u64, B256>,
}

impl BlockFixture {
    /// Resolve the [`SpecId`] for this block, defaulting to [`SpecId::PRAGUE`].
    pub fn spec_id(&self) -> SpecId {
        SpecId::from_str(&self.spec_id).unwrap_or(SpecId::PRAGUE)
    }

    /// Build the revm [`CfgEnv`] for this block. `disable_nonce_check` is left `false`: real
    /// mainnet transactions carry correct nonces.
    pub fn to_cfg(&self) -> CfgEnv {
        let mut cfg = CfgEnv::new_with_spec(self.spec_id());
        cfg.chain_id = self.chain_id;
        cfg
    }

    /// Build the revm [`BlockEnv`] for this block.
    pub fn to_block_env(&self) -> BlockEnv {
        let mut env = BlockEnv {
            number: U256::from(self.number),
            beneficiary: self.coinbase,
            timestamp: U256::from(self.timestamp),
            gas_limit: self.gas_limit,
            basefee: self.basefee,
            difficulty: self.difficulty,
            prevrandao: self.prevrandao,
            blob_excess_gas_and_price: None,
        };
        if let Some(excess) = self.excess_blob_gas {
            // Record the real `excess_blob_gas` but pin the blob gas price to the protocol minimum
            // (1 wei). The blob base fee depends on the active fork's update fraction, and
            // post-Prague forks (Fusaka/Osaka) use a larger fraction that this revm build does not
            // model; recomputing with the Prague fraction would massively overprice blob gas and
            // spuriously reject otherwise-valid type-3 transactions. The parallel-vs-sequential
            // oracle only needs both executors to see identical inputs, so the floor price is
            // sufficient (blob-fee accounting is intentionally not mainnet-faithful — see the
            // module docs).
            env.blob_excess_gas_and_price =
                Some(BlobExcessGasAndPrice { excess_blob_gas: excess, blob_gasprice: 1 });
        }
        env
    }
}

/// One EIP-2930 access-list entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccessFixture {
    /// Accessed address.
    pub address: Address,
    /// Accessed storage keys under `address`.
    #[serde(default)]
    pub storage_keys: Vec<B256>,
}

/// One EIP-7702 authorization tuple, kept *signed* so revm recovers the authority exactly as
/// mainnet did (no off-chain recovery, no fidelity loss).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthFixture {
    /// Chain id the authorization is valid on (`0` means any chain).
    pub chain_id: U256,
    /// Delegation target address (`0x0` clears the delegation).
    pub address: Address,
    /// Authority account nonce the tuple is bound to.
    pub nonce: u64,
    /// Signature `y_parity`.
    pub y_parity: u8,
    /// Signature `r`.
    pub r: U256,
    /// Signature `s`.
    pub s: U256,
}

/// A transaction, decoupled from revm's `TxEnv`. Covers tx types 0–4.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxFixture {
    /// EIP-2718 transaction type (0 legacy, 1 2930, 2 1559, 3 4844, 4 7702).
    pub tx_type: u8,
    /// Sender.
    pub caller: Address,
    /// Recipient; `None` is a contract-creation.
    pub to: Option<Address>,
    /// Sender nonce.
    pub nonce: u64,
    /// Transferred value.
    pub value: U256,
    /// Call data / init code.
    pub data: Bytes,
    /// Gas limit.
    pub gas_limit: u64,
    /// Gas price; for EIP-1559+ this is the max fee per gas.
    pub gas_price: u128,
    /// Max priority fee per gas (EIP-1559+).
    #[serde(default)]
    pub gas_priority_fee: Option<u128>,
    /// Chain id (EIP-155); `None` for pre-155 legacy txs.
    #[serde(default)]
    pub chain_id: Option<u64>,
    /// EIP-2930 access list.
    #[serde(default)]
    pub access_list: Vec<AccessFixture>,
    /// EIP-4844 blob versioned hashes.
    #[serde(default)]
    pub blob_hashes: Vec<B256>,
    /// EIP-4844 max fee per blob gas.
    #[serde(default)]
    pub max_fee_per_blob_gas: u128,
    /// EIP-7702 authorization list.
    #[serde(default)]
    pub authorization_list: Vec<AuthFixture>,
}

impl TxFixture {
    /// Convert into a revm [`TxEnv`]. EIP-7702 authorizations are passed as
    /// [`Either::Left`]`(`[`SignedAuthorization`]`)` so revm recovers the authority itself,
    /// matching on-chain execution byte-for-byte.
    pub fn to_tx_env(&self) -> TxEnv {
        let kind = match self.to {
            Some(addr) => TxKind::Call(addr),
            None => TxKind::Create,
        };
        let access_list = AccessList(
            self.access_list
                .iter()
                .map(|a| AccessListItem {
                    address: a.address,
                    storage_keys: a.storage_keys.clone(),
                })
                .collect(),
        );
        let authorization_list = self
            .authorization_list
            .iter()
            .map(|a| {
                let auth =
                    Authorization { chain_id: a.chain_id, address: a.address, nonce: a.nonce };
                Either::Left(SignedAuthorization::new_unchecked(auth, a.y_parity, a.r, a.s))
            })
            .collect();
        TxEnv {
            tx_type: self.tx_type,
            caller: self.caller,
            gas_limit: self.gas_limit,
            gas_price: self.gas_price,
            kind,
            value: self.value,
            data: self.data.clone(),
            nonce: self.nonce,
            chain_id: self.chain_id,
            access_list,
            gas_priority_fee: self.gas_priority_fee,
            blob_hashes: self.blob_hashes.clone(),
            max_fee_per_blob_gas: self.max_fee_per_blob_gas,
            authorization_list,
        }
    }
}

/// A single account's opening snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountFixture {
    /// Account balance.
    pub balance: U256,
    /// Account nonce.
    pub nonce: u64,
    /// Account code. `None`/empty for EOAs; for EIP-7702-delegated accounts this is the raw
    /// `0xef0100||target` designator (decoded into the proper bytecode variant on load).
    #[serde(default)]
    pub code: Option<Bytes>,
    /// Account storage (slot -> value).
    #[serde(default)]
    pub storage: BTreeMap<U256, U256>,
}

/// Convert a [`PreState`] into an [`InMemoryDB`]. Code (legacy, EOF, or EIP-7702 designator) is
/// decoded via [`Bytecode::new_raw_checked`], which recognizes the `0xef0100` prefix and yields
/// the matching variant with the correct `code_hash`.
pub fn pre_state_to_db(pre_state: &PreState) -> InMemoryDB {
    let mut accounts: HashMap<Address, PlainAccount> = HashMap::default();
    let mut bytecodes: HashMap<B256, Bytecode> = HashMap::default();
    for (addr, acc) in pre_state {
        let (code_hash, code) = match acc.code.as_ref() {
            Some(bytes) if !bytes.is_empty() => {
                let bytecode = Bytecode::new_raw_checked(bytes.clone())
                    .expect("invalid bytecode in pre_state");
                let hash = bytecode.hash_slow();
                bytecodes.insert(hash, bytecode.clone());
                (hash, Some(bytecode))
            }
            _ => (KECCAK_EMPTY, None),
        };
        let info = AccountInfo { balance: acc.balance, nonce: acc.nonce, code_hash, code };
        let storage = acc.storage.iter().map(|(k, v)| (*k, *v)).collect();
        accounts.insert(*addr, PlainAccount { info, storage });
    }
    // Block hashes aren't account state; they're carried on `BlockFixture::block_hashes` and
    // installed by `MainnetBlock::from_fixtures`, so leave them empty here.
    InMemoryDB::new(accounts, bytecodes, HashMap::default())
}

/// EIP-7702 delegation designator prefix: `0xef0100 || target_address` (23 bytes total).
const EIP7702_MAGIC: [u8; 3] = [0xef, 0x01, 0x00];

/// Collect the EIP-7702 delegation **target** addresses referenced by a block: every
/// `authorization_list` entry of its type-4 transactions (delegations applied during the block),
/// plus every `pre_state` account already holding a `0xef0100 || target` designator (delegations
/// applied in an earlier block).
///
/// `prestateTracer` does **not** report the code of these targets, yet that code is executed when a
/// delegated account is called. Fetchers should resolve each target's account (code/balance/nonce)
/// over RPC and add it to the prestate, otherwise a call into a delegated account replays as a call
/// to an empty account. The zero address (delegation clear) is excluded.
pub fn delegation_targets(txs: &[TxFixture], pre_state: &PreState) -> Vec<Address> {
    let mut targets: BTreeSet<Address> = BTreeSet::new();
    for tx in txs {
        for auth in &tx.authorization_list {
            if auth.address != Address::ZERO {
                targets.insert(auth.address);
            }
        }
    }
    for acc in pre_state.values() {
        if let Some(code) = acc.code.as_ref() {
            let bytes = code.as_ref();
            if bytes.len() == 23 && bytes[..3] == EIP7702_MAGIC {
                targets.insert(Address::from_slice(&bytes[3..23]));
            }
        }
    }
    targets.into_iter().collect()
}

/// A fully-loaded mainnet block, ready to feed into the executor.
#[derive(Debug)]
pub struct MainnetBlock {
    /// Fixture directory name — a block number (`25323281`) or a range (`25326115_25326124`).
    /// Used to select a single fixture when replaying.
    pub label: String,
    /// Block number.
    pub number: u64,
    /// Resolved hardfork.
    pub spec: SpecId,
    /// Configuration environment.
    pub cfg: CfgEnv,
    /// Block environment.
    pub block_env: BlockEnv,
    /// Transactions in execution order.
    pub txs: Vec<TxEnv>,
    /// Opening state.
    pub db: InMemoryDB,
}

impl MainnetBlock {
    /// Build a [`MainnetBlock`] from already-parsed fixtures (from disk or fetched in memory).
    pub fn from_fixtures(
        label: String,
        block: &BlockFixture,
        txs: &[TxFixture],
        pre_state: &PreState,
    ) -> Self {
        let mut db = pre_state_to_db(pre_state);
        // Feed captured block hashes to the `BLOCKHASH` opcode (else `InMemoryDB` returns a wrong
        // fallback hash, diverging any tx that uses `blockhash()`).
        db.block_hashes = block.block_hashes.iter().map(|(k, v)| (*k, *v)).collect();
        Self {
            label,
            number: block.number,
            spec: block.spec_id(),
            cfg: block.to_cfg(),
            block_env: block.to_block_env(),
            txs: txs.iter().map(TxFixture::to_tx_env).collect(),
            db,
        }
    }
}

/// Root directory holding the single-block fixtures. Overridable via the `GREVM_MAINNET_BLOCKS`
/// environment variable; defaults to [`DEFAULT_MAINNET_BLOCKS_DIR`].
pub fn mainnet_blocks_root() -> PathBuf {
    std::env::var("GREVM_MAINNET_BLOCKS")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(DEFAULT_MAINNET_BLOCKS_DIR))
}

/// Root directory holding the merged "big block" fixtures. Overridable via the
/// `GREVM_CONTINUOUS_BLOCKS` environment variable; defaults to [`DEFAULT_CONTINUOUS_BLOCKS_DIR`].
pub fn continuous_blocks_root() -> PathBuf {
    std::env::var("GREVM_CONTINUOUS_BLOCKS")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(DEFAULT_CONTINUOUS_BLOCKS_DIR))
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> io::Result<T> {
    let file = File::open(path)?;
    serde_json::from_reader(BufReader::new(file))
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{}: {e}", path.display())))
}

/// Load a single block fixture from `dir` (a `test_data/mainnet_blocks/<number>/` directory).
pub fn load_mainnet_block(dir: &Path) -> io::Result<MainnetBlock> {
    let block: BlockFixture = read_json(&dir.join("block.json"))?;
    let txs: Vec<TxFixture> = read_json(&dir.join("txs.json"))?;
    let pre_state: PreState = read_json(&dir.join("pre_state.json"))?;

    let label = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    Ok(MainnetBlock::from_fixtures(label, &block, &txs, &pre_state))
}

/// Load every block fixture directly under `root`, sorted by (leading) block number. Returns an
/// empty vector if the directory does not exist, so tests/benches skip gracefully when the
/// fixtures are absent. Directory names may be a plain block number (`25323281`) or a range
/// (`25326120_25326139`, for merged big blocks); sorting keys off the leading number.
pub fn load_blocks_from(root: &Path) -> Vec<MainnetBlock> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_dir() && p.join("block.json").is_file())
        .collect();
    dirs.sort_by_key(|p| {
        p.file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.split('_').next())
            .and_then(|n| n.parse::<u64>().ok())
            .unwrap_or(u64::MAX)
    });
    dirs.iter()
        .map(|dir| {
            load_mainnet_block(dir).unwrap_or_else(|e| panic!("load {}: {e}", dir.display()))
        })
        .collect()
}

/// Load every single-block fixture under [`mainnet_blocks_root`].
pub fn load_mainnet_blocks() -> Vec<MainnetBlock> {
    load_blocks_from(&mainnet_blocks_root())
}

/// Load every merged "big block" fixture under [`continuous_blocks_root`].
pub fn load_continuous_blocks() -> Vec<MainnetBlock> {
    load_blocks_from(&continuous_blocks_root())
}

/// Write a block's `{block,txs,pre_state}.json` into `dir`, creating it as needed.
pub fn write_block(
    dir: &Path,
    block: &BlockFixture,
    txs: &[TxFixture],
    pre_state: &PreState,
) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    write_json(&dir.join("block.json"), block)?;
    write_json(&dir.join("txs.json"), &txs)?;
    write_json(&dir.join("pre_state.json"), pre_state)?;
    Ok(())
}

/// Write a single-block fixture to `<root>/<number>/`. Used by the `fetch_block` binary.
pub fn write_mainnet_block(
    root: &Path,
    block: &BlockFixture,
    txs: &[TxFixture],
    pre_state: &PreState,
) -> io::Result<PathBuf> {
    let dir = root.join(block.number.to_string());
    write_block(&dir, block, txs, pre_state)?;
    Ok(dir)
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    let file = File::create(path)?;
    serde_json::to_writer_pretty(BufWriter::new(file), value)
        .map_err(|e| io::Error::other(format!("{}: {e}", path.display())))
}

// ---------------------------------------------------------------------------------------------
// JSON-RPC ingestion: map node responses (`serde_json::Value`) into the fixtures above. Kept in
// the library (rather than in the fetcher binaries) so the single-block and continuous-block
// fetchers share one implementation. Only `serde_json` is needed here; the HTTP client lives in
// the `src/bin/` fetchers.
// ---------------------------------------------------------------------------------------------

/// Mainnet hardfork activation by block timestamp (recent forks only). The replay oracle is
/// parallel-vs-sequential, and this revm build models up to Prague, so we cap there; pass an
/// explicit spec to the fetcher to override.
pub fn spec_for_timestamp(ts: u64) -> &'static str {
    match ts {
        t if t >= 1_746_612_311 => "Prague",   // Pectra,  2025-05-07
        t if t >= 1_710_338_135 => "Cancun",   // Dencun,  2024-03-13
        t if t >= 1_681_338_479 => "Shanghai", // Shapella, 2023-04-12
        _ => "Merge",
    }
}

impl BlockFixture {
    /// Build a [`BlockFixture`] from an `eth_getBlockByNumber` result.
    pub fn from_rpc(
        number: u64,
        chain_id: u64,
        spec_id: String,
        block: &Value,
    ) -> Result<Self, String> {
        Ok(Self {
            number,
            coinbase: parse_addr(req_str(block, "miner")?)?,
            timestamp: hex_u64(req_str(block, "timestamp")?),
            gas_limit: hex_u64(req_str(block, "gasLimit")?),
            basefee: block.get("baseFeePerGas").and_then(Value::as_str).map(hex_u64).unwrap_or(0),
            difficulty: opt_u256(block.get("difficulty"))?.unwrap_or(U256::ZERO),
            prevrandao: block.get("mixHash").and_then(Value::as_str).map(parse_b256).transpose()?,
            excess_blob_gas: block.get("excessBlobGas").and_then(Value::as_str).map(hex_u64),
            chain_id,
            spec_id,
            block_hashes: BTreeMap::new(), // filled separately (see `Rpc::fetch_block_hashes`)
        })
    }
}

impl TxFixture {
    /// Build a [`TxFixture`] from a full transaction object (`eth_getBlockByNumber` with `true`).
    pub fn from_rpc(tx: &Value) -> Result<Self, String> {
        let tx_type = tx.get("type").and_then(Value::as_str).map(hex_u64).unwrap_or(0) as u8;
        let to = tx.get("to").and_then(Value::as_str).map(parse_addr).transpose()?;

        // EIP-1559+ carry maxFeePerGas/maxPriorityFeePerGas; legacy/2930 carry gasPrice.
        let max_fee = tx.get("maxFeePerGas").and_then(Value::as_str).map(hex_u128);
        let gas_price = max_fee
            .or_else(|| tx.get("gasPrice").and_then(Value::as_str).map(hex_u128))
            .unwrap_or(0);

        let access_list = match tx.get("accessList").and_then(Value::as_array) {
            Some(items) => items.iter().map(parse_access).collect::<Result<_, _>>()?,
            None => Vec::new(),
        };
        let blob_hashes = match tx.get("blobVersionedHashes").and_then(Value::as_array) {
            Some(hs) => {
                hs.iter().filter_map(Value::as_str).map(parse_b256).collect::<Result<_, _>>()?
            }
            None => Vec::new(),
        };
        let authorization_list = match tx.get("authorizationList").and_then(Value::as_array) {
            Some(auths) => auths.iter().map(parse_auth).collect::<Result<_, _>>()?,
            None => Vec::new(),
        };

        Ok(TxFixture {
            tx_type,
            caller: parse_addr(req_str(tx, "from")?)?,
            to,
            nonce: hex_u64(req_str(tx, "nonce")?),
            value: parse_u256(req_str(tx, "value")?)?,
            data: parse_bytes(req_str(tx, "input")?)?,
            gas_limit: hex_u64(req_str(tx, "gas")?),
            gas_price,
            gas_priority_fee: tx.get("maxPriorityFeePerGas").and_then(Value::as_str).map(hex_u128),
            chain_id: tx.get("chainId").and_then(Value::as_str).map(hex_u64),
            access_list,
            blob_hashes,
            max_fee_per_blob_gas: tx
                .get("maxFeePerBlobGas")
                .and_then(Value::as_str)
                .map(hex_u128)
                .unwrap_or(0),
            authorization_list,
        })
    }
}

fn parse_access(item: &Value) -> Result<AccessFixture, String> {
    Ok(AccessFixture {
        address: parse_addr(req_str(item, "address")?)?,
        storage_keys: match item.get("storageKeys").and_then(Value::as_array) {
            Some(ks) => {
                ks.iter().filter_map(Value::as_str).map(parse_b256).collect::<Result<_, _>>()?
            }
            None => Vec::new(),
        },
    })
}

fn parse_auth(a: &Value) -> Result<AuthFixture, String> {
    Ok(AuthFixture {
        chain_id: parse_u256(req_str(a, "chainId")?)?,
        address: parse_addr(req_str(a, "address")?)?,
        nonce: hex_u64(req_str(a, "nonce")?),
        y_parity: hex_u64(
            a.get("yParity").or_else(|| a.get("v")).and_then(Value::as_str).unwrap_or("0x0"),
        ) as u8,
        r: parse_u256(req_str(a, "r")?)?,
        s: parse_u256(req_str(a, "s")?)?,
    })
}

/// Accumulate one block's `debug_traceBlockByNumber` (default `prestateTracer`) response into
/// `merged`, applying "first-seen wins" at account and storage-slot granularity. Call once per
/// block, in block order, to reconstruct the opening state of a single block or a contiguous
/// range of blocks.
pub fn accumulate_prestate(merged: &mut PreState, trace: &Value) -> Result<(), String> {
    let entries = trace.as_array().ok_or("debug_traceBlockByNumber did not return an array")?;
    for entry in entries {
        // Each element is `{ "txHash": ..., "result": { "0xaddr": {..}, .. } }`, where `result`
        // is the transaction's prestate (account -> snapshot) map directly.
        let result = entry.get("result").unwrap_or(entry);
        let Some(pre) = result.as_object() else { continue };
        for (addr_str, acc) in pre {
            let addr = parse_addr(addr_str)?;
            // Membership — not zero-ness — marks "seen", so a genuinely empty opening account is
            // not overwritten by a later transaction that funded it.
            if let std::collections::btree_map::Entry::Vacant(slot) = merged.entry(addr) {
                slot.insert(AccountFixture {
                    balance: opt_u256(acc.get("balance"))?.unwrap_or(U256::ZERO),
                    nonce: acc.get("nonce").and_then(Value::as_u64).unwrap_or(0),
                    code: acc
                        .get("code")
                        .and_then(Value::as_str)
                        .filter(|s| *s != "0x")
                        .map(parse_bytes)
                        .transpose()?,
                    storage: BTreeMap::new(),
                });
            }
            // Storage is merged at slot granularity: the first transaction to touch a slot wins.
            if let Some(storage) = acc.get("storage").and_then(Value::as_object) {
                let account = merged.get_mut(&addr).expect("just inserted");
                for (slot, val) in storage {
                    let slot = parse_u256(slot)?;
                    let val = parse_u256(val.as_str().unwrap_or("0x0"))?;
                    account.storage.entry(slot).or_insert(val);
                }
            }
        }
    }
    Ok(())
}

fn req_str<'a>(v: &'a Value, key: &str) -> Result<&'a str, String> {
    v.get(key).and_then(Value::as_str).ok_or_else(|| format!("missing field `{key}`"))
}

fn hex_u64(s: &str) -> u64 {
    u64::from_str_radix(s.trim_start_matches("0x"), 16).unwrap_or(0)
}

fn hex_u128(s: &str) -> u128 {
    u128::from_str_radix(s.trim_start_matches("0x"), 16).unwrap_or(0)
}

fn opt_u256(v: Option<&Value>) -> Result<Option<U256>, String> {
    v.and_then(Value::as_str).map(parse_u256).transpose()
}

fn parse_u256(s: &str) -> Result<U256, String> {
    U256::from_str_radix(s.trim_start_matches("0x"), 16).map_err(|e| e.to_string())
}

fn parse_addr(s: &str) -> Result<Address, String> {
    s.parse::<Address>().map_err(|e| e.to_string())
}

fn parse_b256(s: &str) -> Result<B256, String> {
    s.parse::<B256>().map_err(|e| e.to_string())
}

fn parse_bytes(s: &str) -> Result<Bytes, String> {
    s.parse::<Bytes>().map_err(|e| e.to_string())
}
