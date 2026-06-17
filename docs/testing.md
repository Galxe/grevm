# Testing & Benchmarking Grevm

This guide covers how to run Grevm's tests and benchmarks, how to replay real Ethereum mainnet
blocks (including EIP-7702 transactions) through the parallel scheduler, and the environment
variables that tune execution.

## Cargo features

| Feature | Pulls in | Used by |
| --- | --- | --- |
| `test-utils` | mock accounts, `InMemoryDB`, the execute/compare helpers, the mainnet fixture schema (`serde`, `serde_json`, `revm-primitives/serde`, `metrics-util`) | every test and benchmark |
| `tools` | `test-utils` **+** a blocking HTTP client (`ureq`) | the `fetch_block` / `fetch_continuous` fetcher binaries only |

`tools` is kept separate from `test-utils` so the test/bench path never compiles the HTTP client,
and a normal library build (e.g. when Grevm is a dependency) pulls in neither.

## Running the tests

```bash
cargo test --features test-utils
```

This builds and runs:

| Target | What it covers |
| --- | --- |
| lib unit tests | core data-structure and scheduler invariants |
| `tests/erc20.rs` | ERC-20 transfer workloads |
| `tests/native_transfers.rs` | raw value-transfer workloads (independent / chained / hybrid) |
| `tests/uniswap.rs` | Uniswap swap workloads |
| `tests/eip-7702.rs` | synthetic EIP-7702 scenarios: delegate / re-delegate / reset / multi-authority; storage preservation when an already-delegated EOA with storage is re-delegated (the block-22546209 bug); and interaction with `CREATE`/`CREATE2` and `SELFDESTRUCT` |
| `tests/mainnet.rs` | replays real mainnet blocks from fixtures (skips if none present) |

Every integration test compares **Grevm parallel execution against a sequential revm reference**
and asserts both the per-transaction results and the final bundle state match.

> Plain `cargo test` (without `--features test-utils`) only builds the library; the integration
> tests and benches require the feature.

## Environment-variable knobs

| Variable | Default | Effect |
| --- | --- | --- |
| `GREVM_MIN_PARALLEL_TXS` | `64` | Blocks with fewer transactions fall back to sequential. Set to `0` to force the parallel path even for tiny blocks (needed when replaying small real blocks). |
| `GREVM_FALLBACK_SEQUENTIAL` | `false` | Force sequential execution for every block. |
| `GREVM_CONCURRENT_LEVEL` | number of CPU cores | Worker/partition count for parallel execution. |
| `ASYNC_COMMIT_STATE` | `true` | Asynchronously bundle execution results during parallel execution. |
| `GREVM_MAINNET_BLOCKS` | `test_data/mainnet_blocks` | Directory the mainnet replay test reads single-block fixtures from. |
| `GREVM_CONTINUOUS_BLOCKS` | `test_data/con_eth_blocks` | Directory the `continuous` bench reads merged "big block" fixtures from. |

Benchmark-only tuning (read by `benches/gigagas.rs`): `NUM_EOA` (default `100000`), `HOT_RATIO`
(`0.0`), `DB_LATENCY_US` (`0`), `WITH_HINTS` (`false`), `DEPENDENCY_RATIO` (`0.1`),
`DEPENDENCY_DISTANCE` (`8`), `FILTER` (substring filter for which sub-benchmarks to run).

## Replaying real mainnet blocks

Grevm can replay real mainnet blocks and check that parallel execution matches sequential revm on
the exact same inputs. The block's *execution environment* is downloaded over JSON-RPC and stored
as a self-contained fixture; no full node or archive database is needed.

### 1. Fetch a block

Requires a JSON-RPC endpoint with the `debug` namespace enabled (it uses `debug_traceBlockByNumber`
with the `prestateTracer`). Find blocks containing EIP-7702 (type-4) transactions via
<https://etherscan.io/txnauthlist>.

```bash
cargo run --bin fetch_block --features tools -- <block> <rpc_url> [spec] [out_dir]
```

- `block`   — decimal (`25323281`) or hex (`0x1826711`).
- `rpc_url` — HTTP JSON-RPC endpoint.
- `spec`    — optional hardfork override (e.g. `Prague`); inferred from the block timestamp otherwise.
- `out_dir` — optional output root (default `test_data/mainnet_blocks`).

This writes `test_data/mainnet_blocks/<block>/{block,txs,pre_state}.json`.

### 2. Run the replay

```bash
# Replay every single-block fixture under test_data/mainnet_blocks/
GREVM_MIN_PARALLEL_TXS=0 cargo test --features test-utils --test mainnet replay_mainnet_blocks

# Replay just one block
GREVM_MIN_PARALLEL_TXS=0 GREVM_MAINNET_BLOCK=25323281 \
  cargo test --features test-utils --test mainnet replay_mainnet_blocks
```

`GREVM_MIN_PARALLEL_TXS=0` forces the parallel path even though real blocks are frequently smaller
than 64 transactions. Without `GREVM_MAINNET_BLOCK` the test loads every fixture under
`GREVM_MAINNET_BLOCKS` and asserts parallel == sequential for each.

### 3. Pipelined discover-and-replay (`replay_7702`)

To validate many real 7702 blocks without committing fixtures, the `replay_7702` binary discovers
type-4 blocks over RPC and replays each in-process, prefetching the next block while replaying the
current one (a background thread fetches block N+1 while the main thread replays block N):

```bash
cargo run --bin replay_7702 --features tools -- <rpc_url> [start_block] [count] [out_dir]
```

Blocks are scanned **upward** toward the chain head.

- `start_block` — where to start scanning. Default: the mainnet EIP-7702 activation block
  (`22431084`, Pectra, 2025-05-07).
- `count`       — how many 7702 blocks to replay. Default: **all** of them up to the chain head.
- `out_dir`     — optional. If given, each replayed block's fixture is written to
  `<out_dir>/<number>/`. Omitted ⇒ fetched in memory only, **nothing is written to disk** (so the
  "all" mode doesn't fill the disk with millions of fixtures).

```bash
# Replay ALL 7702 blocks since activation (large job — dominated by one debug_traceBlockByNumber
# per block; interrupt any time), in memory only:
cargo run --bin replay_7702 --features tools -- <rpc_url>

# Replay 20 blocks from a height AND persist them under test_data/mainnet_blocks/:
cargo run --bin replay_7702 --features tools -- <rpc_url> 25323281 20 test_data/mainnet_blocks
```

Blocks are validated as they arrive. On the **first** divergence (grevm parallel result !=
sequential) it prints the offending block (the assertion above it shows the diverging
account/value) and exits non-zero. Without `out_dir` nothing is written to disk — use `fetch_block`
or pass `out_dir` to persist a fixture.

### Oracle & scope

The replay oracle is **parallel == sequential on identical inputs**. It is intentionally *not*
mainnet-state-root faithful:

- Block-level system calls (EIP-4788/2935/7002/7251) and withdrawals are not applied (they are not
  part of the transaction list).
- Blob gas price is pinned to the protocol minimum (this revm build models blob fees up to Prague
  only), so type-3 transactions are never spuriously rejected.

Two inputs that `prestateTracer` does **not** report are captured separately so transactions that
depend on them still replay faithfully:

- the last 256 block hashes (for the `BLOCKHASH` opcode), and
- the code of EIP-7702 delegation targets (the contract executed when a delegated account is called;
  collected from each type-4 tx's `authorization_list` and from existing `0xef0100` designators).

Both executors see exactly the same environment, so the comparison stays valid regardless.

## Merged "big block" benchmark

To stress single-block parallelism beyond what one real block provides, `fetch_continuous` merges a
range of consecutive blocks into one oversized block: all transactions are concatenated, their
prestate read sets merged, a single (lowest-basefee) block environment is chosen, and any
transaction that cannot execute under that merged environment is filtered out.

```bash
# Merge `count` consecutive blocks starting at `start` into test_data/con_eth_blocks/<start>_<end>/
cargo run --bin fetch_continuous --features tools -- <start> <count> <rpc_url> [out_dir]

# Benchmark Grevm parallel vs revm sequential over the merged block(s)
cargo bench --features test-utils --bench continuous

# Replay them as a correctness test (all ranges, or one via GREVM_CONTINUOUS_RANGE)
GREVM_MIN_PARALLEL_TXS=0 cargo test --features test-utils --test mainnet replay_continuous_blocks
GREVM_MIN_PARALLEL_TXS=0 GREVM_CONTINUOUS_RANGE=25326115_25326124 \
  cargo test --features test-utils --test mainnet replay_continuous_blocks
```

The `continuous` benchmark first verifies parallel == sequential on each big block, then times both.
It skips cleanly when no fixtures are present. For a ~1-gigagas block, merge roughly 30+ blocks.

## Benchmarks

```bash
# Synthetic workloads (ERC-20, Uniswap, raw transfers, hybrid)
JEMALLOC_SYS_WITH_MALLOC_CONF="thp:always,metadata_thp:always" \
NUM_EOA=<n> HOT_RATIO=<r> DB_LATENCY_US=<us> \
cargo bench --features test-utils --bench gigagas

# Real merged mainnet block(s) (see above)
cargo bench --features test-utils --bench continuous
```

## `test_data/` layout & fetching the fixtures

The fixtures live in a separate repository,
[`grevm-test-data`](https://github.com/Galxe/grevm-test-data), wired in as a **git submodule** at
`test_data/`. Fetch it with standard git:

```bash
# Fresh clone, including submodules:
git clone --recurse-submodules <grevm-url>

# Or, in an existing checkout:
git submodule update --init test_data
```

When the submodule is not checked out, the mainnet replay test and the `continuous` benchmark skip
gracefully.

```
test_data/
├── mainnet_blocks/<block>/{block,txs,pre_state}.json        # single real blocks (fetch_block)
└── con_eth_blocks/<start>_<end>/{block,txs,pre_state}.json   # merged big blocks (fetch_continuous)
```

Each fixture is self-contained (account code is stored inline) and uses Grevm's own stable JSON
schema (see `src/test_utils/common/mainnet.rs`) rather than serializing revm's internal types, so it
survives revm upgrades.

> When you add new fixtures: commit & push them in the `grevm-test-data` repo, then in this repo
> `cd test_data && git checkout <new-commit>`, `git add test_data`, and commit the bumped submodule
> pointer so CI / fresh checkouts pick up the new data.
