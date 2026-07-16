# 在 revm 29.0.1 中实现 EIP-7702 Delegated Safety

> 设计日期：2026-07-16
>
> revm 基线：`87318f6f`（crates.io `revm 29.0.1`）
>
> 本地研究分支：`/Users/gx/ws/git/block/alloy/revm` 的 `gravity/eip7702-safety-v29.0.1`

相关文档：

- [Grevm-only 设计](./eip-7702-delegated-create-reserve-balance-design.md)
- [Monad EIP-7702 pipeline nonce/balance 防护学习笔记](../../monad/docs/eip-7702-pipeline-nonce-balance-defense-study.md)
- [Gravity invalid transaction skip 设计](../../mono-grav/docs/invalid-transaction-skipped-receipt-design.md)
- [Gravity invalid transaction skip 风险分析](../../mono-grav/docs/invalid-transaction-skipped-receipt-risk-analysis.md)

## 1. 结论

从修改 revm 的角度，推荐把两个功能实现为一组可配置的 chain extension：

1. 在 revm 原生 `CREATE/CREATE2` instruction 中、创建 frame 之前拒绝 delegated execution recipient。
2. 在 revm 原生 `Journal` 中跟踪 execution balance mutation；延迟顶层成功 frame 的现有 checkpoint commit，在 handler 已完成 gas/refund 计算后检查“预计交易最终余额”，违反 reserve 时直接用这个 checkpoint 回滚 execution 效果。

这条路线比 grevm-only 方案在 EVM 生命周期上更自然：它复用顶层 call/create frame 本来就存在的 checkpoint，因此不需要再套一层 transaction checkpoint，也不需要在 create transaction 回滚后手工补回 sender nonce。

但 reserve 的预算来源不能完全放进 revm。revm 一次只执行一笔交易，不知道当前 block suffix，更不知道其他 inflight blocks。合理分层是：

```text
gravity-reth / grevm
  - 决定 hardfork 是否启用
  - 构建当前 block ReservePlan
  - 未来接收共识侧 pipeline liability
  - 为每个 tx incarnation 构造只读 ReservePolicy
                        │
                        ▼
revm fork
  - 识别 delegated execution context
  - 跟踪所有原生 balance debit/credit 路径
  - 在正确的顶层 frame checkpoint 上检查并回滚
  - 输出收费的 status=0 execution failure
```

如果只比较最终代码的执行正确性与结构，修改 revm 更干净；如果比较长期维护、升级成本和影响范围，grevm-only 更好。结合当前 grevm 已经回到 upstream `revm 29.0.1`、且 block execution 已收敛到 grevm，本文最终仍推荐先实现 grevm-only 方案。只有当 Gravity 决定把这两项规则作为所有 EVM consumers 都必须遵守的长期链级语义时，才建议正式维护 revm fork。

## 2. 正确的 revm 基线

grevm 当前 `Cargo.toml`/`Cargo.lock` 使用：

| crate | version |
| --- | --- |
| `revm` | `29.0.1` |
| `revm-handler` | `10.0.1` |
| `revm-context` | `9.1.0` |
| `revm-context-interface` | `10.2.0` |
| `revm-interpreter` | `25.0.3` |

crates.io `revm 29.0.1` 对应 Galxe/revm 历史提交：

```text
87318f6f bump: tag v87 revm v29.0.1
```

`origin/v29.0.1-gravity` 的 head 是 `bfa048c6`，它在 `87318f6f` 上额外加入了旧 lazy-reward patch，修改了 handler 返回类型等 21 个文件。当前 grevm 已用 `NoRewardHandler` 和 commit-time reward 替代这组 patch，因此新的 delegated-safety fork 应从 `87318f6f` 开始，不应顺带恢复旧 lazy-reward API。

建议后续远端分支命名为：

```text
Galxe/revm: gravity-v29.0.1-delegated-safety
base:       87318f6f
```

grevm 在开发期可使用本地 path patch；合并后固定 git `rev`，不要只依赖可移动 branch：

```toml
revm = { git = "https://github.com/Galxe/revm", rev = "<commit>", default-features = false, features = ["gravity"] }
```

所有直接依赖的 `revm-*` crates 必须来自同一个 workspace commit，避免 crates.io 与 git fork 混用后出现 trait/type identity 分裂。

## 3. 共识语义

### 3.1 Delegated CREATE/CREATE2

判断对象是 interpreter 的 execution recipient：

```text
recipient    = interpreter.input.target_address()
code address = delegation target
```

规则为：

- delegated EOA `A` 直接执行 `CREATE/CREATE2`：exceptional halt；
- `A` 经 `DELEGATECALL/CALLCODE` 后执行：recipient 仍是 `A`，同样 halt；
- `A` 普通 `CALL` 合约 `B`，由 `B` 执行 create：允许；
- 普通合约 create：允许。

第一版无条件禁止，不根据“当前 block 后面是否还有 `A` 的交易”放宽。revm 看不到完整 execution-delay pipeline；当前 block suffix 为空不能证明下一个 inflight block 没有 `sender=A` 或 `authority=A`。

### 3.2 Reserve Balance

只对本交易实际发生非零 balance debit、且属于下列 subject 的账户进入 reserve 慢路径：

1. transaction 开始时已经是 EIP-7702 delegation designator 的账户；
2. 本交易成功 authorization 设置或清除 delegation 的 authority；
3. 将来由共识输入显式标记为 pipeline-protected 的账户。

不能只按 transaction type 或顶层 `to` 判定“delegate 交易”，因为 `X -> B -> A(delegated)`、nested call、create value 和 `SELFDESTRUCT` 都可能减少 `A.balance`。

对候选账户 `A`，在 reimbursement 后的预计最终余额上检查：

```text
fixed_floor(A) = min(max_reserve_balance(A), pre_tx_balance(A))

sender_fixed_floor(A)
  = fixed_floor(A) - actual_charged_fee(tx)，下限为 0

required_final(A)
  = max(fixed/sender_fixed_floor(A), external_required_after(txid, A))
```

`actual_charged_fee` 包括实际 execution gas fee 和 blob fee。`external_required_after` 第一版来自 grevm 当前 block 的 suffix plan；以后可再叠加共识侧 pipeline liability。

当前 block suffix 继续采用 Grevm-only 文档中的逆序公式：

```text
M_j = tx.max_balance_spending()
E_j = tx.effective_balance_spending(basefee, blob_price)

need_before(T_j) = max(M_j, E_j + need_after(T_j))
```

这样既保证每笔未来交易入场时能覆盖 max fee/value，也不会重复锁定最终会退款的 fee headroom。

完整跨 block 保证仍要求共识侧维护 inflight liability，或证明协议级 reserve cap 覆盖整个 execution-delay window。只改 revm 不能生成这项信息。

### 3.3 Reserve violation

violation 是已经通过 transaction validation、完成了 EVM 执行后产生的收费失败，不是 invalid transaction：

```text
保留：
  - call/create transaction sender nonce 增量
  - EIP-7702 authorization code 和 authority nonce
  - 实际 gas/blob fee
  - beneficiary reward（并行路径仍延迟到 commit）

回滚：
  - 顶层 execution frame 的 value transfer
  - storage/code/internal nonce
  - logs/create/selfdestruct

输出：
  - receipt status = 0
  - 空 output 的 Revert 等价结果
  - 清零 storage 与 EIP-7702 refund；未使用 gas 仍退回
  - TxExecutionOutcome::Executed(failure)，绝不能是 Skipped
```

## 4. revm 改动设计

### 4.1 功能开关

在 `crates/context/interface/src/cfg.rs` 的 `Cfg` trait 增加带默认值的方法，在 `CfgEnv` 增加对应字段：

```rust
fn is_delegated_create_disabled(&self) -> bool {
    false
}
```

建议只把 opcode 规则放在 `Cfg`。reserve 是否启用及阈值属于每个 block/tx incarnation 的 policy，不应塞进全局 `CfgEnv`。

这两个语义必须由 Gravity hardfork/block timestamp 派生，不能由环境变量或节点本地配置决定。RPC simulation 可显式选择目标 block 的 chain config，但不能自行开关共识规则。

### 4.2 在原生 CREATE instruction 中拒绝 delegated recipient

修改文件：

- `crates/context/interface/src/host.rs`
- `crates/context/src/context.rs`
- `crates/interpreter/src/instructions/contract.rs`

给 `Host` 增加 `is_delegated_create_disabled()`；标准 `Context` 实现转发到 `Cfg`，`DummyHost` 返回 `false`。

在 `create<WIRE, IS_CREATE2, H>()` 中按以下顺序处理：

```text
1. require_non_staticcall
2. CREATE2 hardfork availability check
3. 若 delegated-create rule 未启用，直接走原逻辑
4. recipient = interpreter.input.target_address()
5. host.load_account_delegated(recipient)
6. AccountLoad.is_delegate_account_cold == Some(_) -> exceptional halt
7. DB error -> FatalExternalError
8. 非 delegated -> 继续原 create 参数、gas、FrameInput 构造
```

检查发生在 stack 参数弹出、initcode memory copy、create gas 扣除和 `FrameInput::Create` 构造之前。这样被禁用的 create 不会进入 `EthFrame::make_create_frame()`，creator nonce 不会增加。

第一版可复用 `InstructionResult::NotActivated` 的 exceptional-halt gas 分类，减少跨 crate enum 改动；若 trace 必须暴露专用原因，再增加 `DelegatedCreateForbidden`，但要固定它与 exceptional halt 相同的 gas 语义。

### 4.3 ReservePolicy 是 revm 与 grevm 的边界

在 `revm-context-interface` 定义只读策略接口，并提供零成本默认实现：

```rust
pub trait BalanceReservePolicy: Clone {
    fn enabled(&self) -> bool;

    fn max_reserve_balance(&self, address: Address) -> U256;

    fn external_required_after(
        &self,
        address: Address,
    ) -> Result<U256, BalanceReserveError>;

    fn is_pipeline_protected(&self, address: Address) -> bool {
        false
    }
}

#[derive(Clone, Copy, Default)]
pub struct NoBalanceReserve;
```

`BalanceReserveError` 放在 `revm-context-interface`，至少保留 `tx_index` 和稳定 reason：

```rust
pub enum BalanceReserveError {
    OverflowPayment { tx_index: usize },
    InvalidPolicyInput { tx_index: usize },
    InvalidCheckpointState,
}
```

同时在 `ContextError<DBError>` 增加 `BalanceReserve(BalanceReserveError)`。这样 policy overflow 能通过现有 `EvmTrError: From<ContextError<_>>` 链路结构化返回，grevm 可以把它映射为带真实 future txid 的 fatal abort；不能把它伪装成 DB error、当前执行 tx 的 reserve violation，或只剩一段需要解析的字符串。

`Journal` 增加第三个默认泛型参数：

```rust
pub struct Journal<DB, ENTRY = JournalEntry, POLICY = NoBalanceReserve> {
    database: DB,
    inner: JournalInner<ENTRY>,
    reserve: ReserveBalance<POLICY>,
}
```

grevm 的 `GravityReservePolicy` 持有：

```rust
struct GravityReservePolicy {
    txid: TxId,
    plan: Arc<OnceLock<Result<ReservePlan, ReservePlanError>>>,
    max_reserve_balance: U256,
    enabled: bool,
}
```

policy 是 immutable block data，可在并行 worker 间共享；每个 incarnation 只保存 `txid` 和 `Arc`。revm 负责何时查询、检查和回滚，grevm 只负责预算公式。

### 4.4 原生 Journal tracking

新增 `ReserveBalance<POLICY>`，状态至少包含：

```rust
struct ReserveBalance<P> {
    policy: P,
    phase: Phase, // Off / PreExecution / Authorization / Execution
    sender: Address,
    sender_pre_tx_balance: U256,
    sender_was_delegated: bool,
    authorization_sensitive: SmallSet<Address>,
    original_balances: SmallMap<Address, U256>,
    debited_accounts: SmallSet<Address>,
    checkpoints: Vec<ReserveCheckpoint>,
}
```

在 `JournalTr` 增加带默认 no-op 的 transaction/frame hook；`Journal<..., POLICY>` 实现真实逻辑：

```rust
begin_reserve_transaction(sender)
begin_reserve_authorization()
end_reserve_authorization()
begin_reserve_execution()
defer_top_level_commit(checkpoint)
finalize_reserve(projected_reimbursement, actual_charged_fee)
end_reserve_transaction()
```

在原生 Journal 的统一余额入口 tracking：

- `caller_accounting_journal_entry`：保存 upfront deduction 前的 sender balance，但不标记 execution debit；
- `transfer`：成功非零 transfer 记录 `from` debit，并保存 `from/to` 首次 mutation 前余额；
- `create_account_checkpoint`：非零 create value 记录 caller debit；
- `selfdestruct`：有余额实际离开时记录 source debit；
- `balance_incr`：非零 credit 保存 original balance，纯 credit 不进入 debit candidate；
- `set_code_with_hash`：authorization phase 内记录成功处理的 authority；
- `checkpoint/commit/revert`：同步 reserve tracker checkpoint，清除已回滚子 frame 的候选与快照；
- `discard_tx/commit_tx/finalize`：清空 tracker，避免并行 incarnation 污染。

最终检查只遍历 `debited_accounts`，不扫描完整 journal/state。subject 判定复用已经加载的 code；DB 错误必须返回 fatal error，不能按非 delegated 放行。

### 4.5 延迟顶层 frame 的现有 checkpoint commit

这是修改 revm 方案相对 grevm-only 最重要的结构优势。

revm 29.0.1 的时序是：

```text
call transaction:
  upfront gas + sender nonce + authorization
  -> make_call_frame checkpoint
  -> value/execution

create transaction:
  upfront gas + authorization
  -> make_create_frame 增加 sender nonce
  -> create_account_checkpoint
  -> value/initcode execution
```

两个 frame checkpoint 都恰好位于必须保留的 transaction/auth/creator nonce 之后、必须回滚的 execution effect 之前。不要在 handler 外层再建 checkpoint，而应让成功的顶层 frame 暂缓 commit：

```text
depth > 0:
  完全保持 upstream frame commit/revert 行为

depth == 0 且 reserve enabled:
  execution failure -> 立即按 upstream revert
  execution success -> 保存 pending checkpoint，不 commit
```

需要覆盖三类顶层完成路径：

1. `EthFrame::process_next_action()` 的普通 call；
2. `return_create()` 的 create code-deposit 成功路径；
3. `make_call_frame()` 中直接返回的 precompile 成功路径。

create 成功时必须先在 checkpoint 内 `set_code`，再把 checkpoint 标成 pending；不能沿用 upstream 当前“commit 后 set_code”的顺序，否则 reserve rollback 无法删除部署代码。

内部 frame、失败 frame、无 reserve policy 的 Ethereum 路径保持原样。`JournalTr` 默认 hook 为 no-op，其他 revm consumers 不启用 Gravity policy 时不改变结果。

### 4.6 Handler 最终检查顺序

修改 `Handler::run_without_catch_error/post_execution`，推荐顺序：

```text
1. validate
2. begin_reserve_transaction(sender)
3. pre_execution:
   - 捕获 sender pre-tx balance/delegation
   - upfront gas deduction、call tx nonce
   - authorization tracking
4. begin_reserve_execution
5. execution；成功顶层 checkpoint 保持 pending
6. last_frame_result
7. refund + EIP-7623 floor
8. 计算 projected_reimbursement 和 actual_charged_fee，但暂不写余额
9. finalize_reserve:
   a. 无 protected debit -> pending checkpoint commit
   b. 全部余额满足 -> pending checkpoint commit
   c. violation -> pending checkpoint revert；把结果改为空 Revert；refund=0
10. reimburse_caller
11. reward_beneficiary / lazy reward
12. execution_result + commit_tx
```

预计最终余额为：

```text
final_balance(A) = journal_current_balance(A)

若 A == sender：
  final_balance(A) += projected_reimbursement
```

因为检查发生在 reimbursement 写入前，violation rollback 不会误回滚退款，也不需要退款两次。修改结果后保留 execution 已经消耗的 gas 和 remaining gas，只清空 refund counter，因此交易支付实际执行 gas，但不能利用 storage/auth refund 降低处罚。

顶层 create sender nonce 位于 `create_account_checkpoint` 之前，checkpoint revert 后天然保留一次，不再需要 grevm-only 方案中的“记录 nonce bump、回滚后重新增加”特例。

### 4.7 错误与结果分类

建议第一版不扩展 receipt/result 公共结构：

- delegated create 使用现有 exceptional halt；
- reserve violation 转换为 `InstructionResult::Revert`，清空 output/log/refund；
- 内部 metrics/debug flag 记录 `ReserveViolation`，用于定位；
- policy overflow、DB error、pending-checkpoint 状态机错误属于结构化 fatal error；
- reserve violation 永远不是 `InvalidTransaction`。

如果以后 RPC/trace 需要机器可读原因，可增加 `ExecutionResult` side metadata，但不能改变 status、gas 与 state-transition 语义。

## 5. grevm 集成

### 5.1 构建统一 EVM

并行和 sequential fallback 都必须使用：

```rust
Journal<CacheDB<...>, JournalEntry, GravityReservePolicy>
```

以及同一套 Gravity `CfgEnv`。不要让 sequential fallback 继续走未配置 policy 的 `EthEvm::transact_raw`。

推荐抽出统一 builder，差异只剩：

| 路径 | nonce check | beneficiary reward |
| --- | --- | --- |
| parallel | disabled，commit 时复核 | deferred |
| sequential fallback | enabled | immediate 或等价顺序结算 |

delegated create 和 reserve 规则必须完全相同。

### 5.2 ReservePlan

`ReservePlan` 仍放在 grevm scheduler：

- 输入是固定 block order、`TxEnv`、base fee/blob price；
- 使用 `OnceLock<Result<ReservePlan, ReservePlanError>>` 惰性构建；
- 第一次 protected debit 才做 `O(block_size)` 构建；
- 后续 `external_required_after(txid, address)` 为 `O(log n)`；
- checked arithmetic overflow 返回真正 future txid 的 fatal error；
- policy 查询不读取可变 scheduler 状态，不依赖 speculative execution outcome。

balance/code 的最终读取仍通过当前 incarnation 的 Journal/`CacheDB`，自然进入 grevm MV read set。前序交易改变 balance/delegation 时，validation 会使旧 incarnation 失效并重执行。

### 5.3 第一阶段 skip

分类保持：

```text
recoverable transaction-validation invalid
  -> Skipped，无状态、无收费

delegated create / reserve violation / 普通 EVM revert
  -> Executed(failure)，正常收费

DB/policy overflow/internal invariant
  -> fatal，触发 grevm 既有 abort/fallback 策略
```

reserve 防护减少动态 invalid；第一阶段 skip 仍是余额初始就不足、跨 block budget 尚未完整接入等情况下的最终活性兜底。

## 6. 性能设计

普通交易不新增 checkpoint；只是把已经存在的顶层 frame commit 从 execution 尾部移动到 post-execution 中。

未发生 protected balance debit 时的成本是：

```text
每笔交易：少量可预测 phase/enable 分支
每次原生 balance mutation：一次 enable 分支
顶层成功：一次 pending-checkpoint 分支
```

不会：

- 每笔交易扫描 journal/state；
- 每个 call frame 查询 delegation；
- 无条件构建 ReservePlan；
- 对 ERC-20 storage transfer 做 balance policy 计算；
- 为 reserve 额外创建 transaction checkpoint。

实际非零 native credit 需要保存一次 original balance，以正确处理“先 credit 后 debit”；普通 ETH transfer 通常只产生 1～2 个 small-map entry。只有候选 debit 在 final check 时才查询 code/plan。

`CREATE/CREATE2` 的 delegation 检查只在实际执行 opcode 时发生，普通不执行 create 的交易没有额外 account lookup。

上线前至少比较：普通 ETH、ERC-20、Uniswap、0.1%/1%/10% delegated workload，以及大量 reverted subcall 的 tracker checkpoint 成本。普通 workload 吞吐回退门槛应在实施前固定。

## 7. 测试计划

### 7.1 revm 单元/集成测试

1. delegated recipient 直接 `CREATE`、`CREATE2` 均 exceptional halt，creator nonce 不增加。
2. delegated recipient 经 `DELEGATECALL` 执行 create 仍失败。
3. delegated recipient 普通 `CALL B`，`B` create 成功。
4. delegation load DB error 返回 fatal error。
5. reserve violation 回滚 call value/storage/log/selfdestruct/internal create。
6. 顶层 create transaction violation 删除部署结果，但 sender nonce 只增加一次。
7. precompile value transfer violation 使用 pending checkpoint 回滚。
8. 最终余额等于 threshold 成功，少 1 wei 失败。
9. 临时低于 threshold、结束前 credit 恢复时成功。
10. reverted child frame 的 debit candidate 随 tracker checkpoint 回滚。
11. authorization 设置和清除 delegation 后都进入 sensitive subject。
12. violation 清零 refund、保留 unused gas reimbursement 和实际收费。
13. `NoBalanceReserve + flag=false` 与 `87318f6f` Ethereum state/result differential 完全一致。

### 7.2 grevm differential/performance

1. parallel、强制 sequential、低于并行阈值三条路径 state/receipt/reward 一致。
2. speculative pass→violation 与 violation→pass 都能通过 MV validation 重执行收敛。
3. current-block reverse suffix 公式覆盖 EIP-1559 max/effective fee、value、blob fee。
4. reserve violation 为 `Executed(failure)`，不进入 skip。
5. incarnation reset 不残留 tracker/pending checkpoint。
6. 未出现 protected debit 的 block 不初始化 ReservePlan。
7. 使用真实 mainnet fixture 做 fork-disabled differential 和吞吐 A/B。

## 8. 升级与维护风险

fork 修改跨越 `context-interface`、`context`、`interpreter`、`handler` 四个 crate，升级 revm 时必须逐项复核：

1. CREATE/CREATE2 是否仍共享相同 instruction 和 frame 构造；
2. top-level precompile 是否仍绕过普通 frame loop；
3. create code deposit 与 checkpoint commit 的顺序；
4. sender nonce、authorization、upfront fee 的时序；
5. 新增 balance mutation API 是否接入 tracker；
6. gas refund/EIP floor/blob fee 的计算顺序；
7. inspector handler 是否复用相同 transaction lifecycle。

fork patch 应拆成三个可独立 review/cherry-pick 的 commit：

```text
1. cfg/host + delegated CREATE restriction
2. generic BalanceReservePolicy + Journal tracker
3. top-level pending checkpoint + Handler enforcement + tests
```

不要把 lazy reward、invalid skip 或 grevm scheduler 逻辑重新塞进同一 revm patch；它们有不同的演进周期和回滚边界。

## 9. 修改 grevm 与修改 revm 的对比

| 维度 | 只修改 grevm | 修改 revm fork |
| --- | --- | --- |
| CREATE 限制 | 自定义 instruction table，足够直接 | 原生 instruction，所有启用 fork 的 consumer 自动一致 |
| reserve checkpoint | 新增 outer checkpoint | 复用已有 top-level frame checkpoint |
| create tx nonce | 回滚后必须精确 reapply | checkpoint 天然位于 nonce bump 之后，无特例 |
| precompile/early return | handler outer checkpoint 自动覆盖 | 必须修改 revm 的 top-level precompile pending-commit 路径 |
| balance tracking | `TrackingJournal<J>` wrapper | 原生 Journal tracker，入口覆盖更强 |
| block suffix plan | grevm | 仍然必须在 grevm |
| cross-block pipeline | 需要共识输入 | 同样需要共识输入 |
| 普通路径成本 | 额外 outer checkpoint + wrapper branch | 无额外 checkpoint，原生 branch 更容易 inline |
| 影响范围 | 仅 grevm block execution | 所有引用 fork 且启用规则的 EVM consumer |
| RPC/trace 一致性 | 需要显式接入 grevm 或相同 extension | 使用同一 fork/config 时天然一致 |
| upstream 升级 | 主要适配公开 trait | 需要长期 forward-port 多 crate patch |
| 回滚/灰度 | 依赖切换简单 | fork dependency 和全链 consumer 升级更重 |
| 代码所有权 | chain-specific 逻辑留在 grevm | 通用 EVM 库承载 Gravity-specific 共识差异 |

两条路线都无法单独解决完整跨 block pipeline liability；因此“修改 revm 后就不需要改 grevm/共识”是不成立的。修改 revm 真正减少的是 execution lifecycle 的适配复杂度，不是预算层复杂度。

## 10. 最终建议

当前条件下推荐顺序是：

1. **生产第一版选 grevm-only。** 当前所有共识 block execution（包括 disable parallel）已经收敛到 grevm；不重新引入 revm fork，发布面和升级成本更小。
2. 把本文的 revm 方案保留为第二选择，尤其是“复用 top-level frame checkpoint、延迟 commit”的实现基准，用来 review grevm-only 外层 checkpoint 是否完整覆盖 nonce/gas/auth/precompile。
3. 如果后续确认 delegated safety 是 Gravity 永久协议差异，并要求 `eth_call`、trace、block replay、其他 executor 全部复用同一语义，则切换到本文的窄 revm fork 更合理。
4. 无论选择哪条路线，都必须单独完成当前 block ReservePlan，并推动共识侧 pipeline liability/cap 设计；否则固定 reserve 只能降低清空余额攻击，不能形成完整跨 block solvency 证明。

一句话总结：**revm fork 的执行实现更漂亮，grevm-only 的系统方案更划算；在当前架构下优先修改 grevm。**
