# Grevm 内实现 EIP-7702 Delegated CREATE 与 Reserve Balance 防护

> 设计日期：2026-07-16
>
> 适用版本：grevm 当前使用的 upstream `revm 29.0.1` 及其 `revm-handler 10.0.1`
>
> 参考：Monad 的 delegated `CREATE/CREATE2` 限制与 Reserve Balance 语义

相关分析：

- [Monad EIP-7702 pipeline nonce/balance 防护学习笔记](../../monad/docs/eip-7702-pipeline-nonce-balance-defense-study.md)
- [Gravity invalid transaction skip 设计](../../mono-grav/docs/invalid-transaction-skipped-receipt-design.md)
- [Gravity invalid transaction skip 收费风险分析](../../mono-grav/docs/invalid-transaction-skipped-receipt-risk-analysis.md)

## 1. 结论

本阶段不需要 fork 或修改 revm。两个功能都可以在 grevm 内完成：

1. 通过自定义 `EthInstructions` 替换 `CREATE`、`CREATE2` opcode，在创建 frame 和增加 creator nonce 之前拒绝 delegated execution context。
2. 把现有 `NoRewardHandler` 扩展为 grevm 自己的 transaction handler，在 transaction pre-execution 之后建立 journal checkpoint；顶层执行结束后统一检查受保护账户的最终余额。如果违反 reserve，则回滚该 checkpoint 内的 EVM 状态，但保留 transaction nonce、upfront gas 和 EIP-7702 authorization 的效果，最终生成一笔收费的失败交易。

推荐语义如下：

| 问题 | 推荐决定 |
| --- | --- |
| delegated context 中的 `CREATE/CREATE2` | 无条件禁用，不根据当前 block 是否还有后续交易放宽 |
| 普通合约中的 `CREATE/CREATE2` | 保持 upstream revm 语义 |
| delegated account 通过普通 `CALL` 调用合约 `B`，由 `B` 创建合约 | 允许，因为增加的是 `B.nonce` |
| 余额检查时机 | 顶层 EVM frame 结束后统一检查，不逐个修改转账 opcode |
| 余额阈值 | `max(Monad 式 reserve floor, 当前 block max-validation/effective-spending 逆序预算)` |
| reserve violation | `Executed` 的 status=0 收费失败，不能记为 `Skipped` |
| revm 依赖 | 继续使用 upstream `revm 29.0.1` |

这里必须明确一个边界：**只修改 grevm 可以实现 Monad 的 execution-side enforcement，但不能凭空获得 Monad 共识侧维护的完整 inflight pipeline 预算。** 当前 block 的交易列表可以由 grevm 自己分析；其他已经排序但尚未执行的 block，grevm 当前接口不可见。因此本文方案对当前 block 提供完整的 suffix 保护；固定 reserve floor 对跨 block 清空余额只是一项 mitigation。要证明整个跨 block pipeline 的 gas solvency，仍要求 reserve cap 与共识侧最大 inflight gas budget 对齐，或者以后把完整 pipeline liability 作为只读输入传给 grevm。没有这项输入时，不能把本方案称为完整 Monad 等价实现。

## 2. 安全目标与非目标

### 2.1 安全目标

- delegated EOA 的代码不能通过 `CREATE/CREATE2` 在执行期间额外增加该 EOA nonce。
- delegated code 不能通过 `CALL`、nested call、create value、`SELFDESTRUCT` 等方式侵蚀受保护余额。
- reserve violation 只回滚顶层 EVM execution frame；transaction nonce、实际 gas 收费和已经处理的 authorization 仍然生效。
- 并行执行与 sequential fallback 使用完全相同的 opcode 和 reserve 语义。
- 投机执行得到的 reserve 结果必须参与 grevm 的读写集校验，不能在错误的前缀状态上提前固化。
- 第一阶段的 invalid transaction skip 继续作为最终兜底；reserve violation 本身不是 invalid transaction。
- 普通交易走 fast path：不扫描 journal、不构建/查询 reserve plan、不执行余额公式。

### 2.2 非目标

- 不修改 `eth_call`、trace 或 gravity-reth 中绕过 grevm 的独立 EVM 路径。本设计以“所有共识 block execution 均进入 grevm，包括 disable parallel 时的 grevm sequential fallback”为前提。
- 不承诺仅靠当前 block 列表就能精确推导其他 inflight blocks 的负债。
- 不为 reserve violation 增加新的 receipt 字段。Ethereum receipt 的 status、gas used 和 state transition 已足够表达共识结果。
- 不把 `Inspector` 作为安全边界。Inspector 适合观测，不适合承担必须覆盖所有状态写入的共识语义。

## 3. 威胁模型与必须维持的不变量

设 `A` 是 delegated EOA，前序交易可以在 `A` 的 execution context 执行任意 delegate code，后续已经排序的交易又以 `A` 为 sender。

需要维持两个不变量：

```text
Nonce invariant:
  任意 EVM code 都不能产生共识 overlay 未预测的 transaction-capable account nonce 增量。

Balance invariant:
  任意 EVM code 都不能花掉已经为当前 block suffix 或 inflight pipeline 保留的余额。
```

第一阶段的 `Skipped` receipt 解决的是“不变量已经被破坏之后 block 仍能完成执行”；本设计的目标是在危险的前序交易中阻止破坏发生，并让该前序交易正常付费失败。

## 4. Delegated context 禁止 CREATE/CREATE2

### 4.1 判定的是 execution recipient，不是 code address

EIP-7702 顶层 delegated call 的语义是：

```text
execution recipient = delegated EOA A
code address        = delegation target D
```

判断必须使用当前 interpreter input 的 `target_address()`，即 storage、balance、nonce 所属的 execution recipient。不能使用当前 bytecode address，也不能只判断顶层 transaction 是否为 EIP-7702 类型。

这会自然得到与 Monad 一致的嵌套语义：

- `A` 的 delegated code 直接执行 `CREATE/CREATE2`：拒绝。
- `A` 再经 `DELEGATECALL/CALLCODE` 执行其他代码：recipient 仍是 `A`，拒绝。
- `A` 通过普通 `CALL` 调用合约 `B`，`B` 执行 `CREATE/CREATE2`：recipient 是 `B`，允许。
- 普通合约执行 `CREATE/CREATE2`：允许。

### 4.2 Grevm 实现方式

新增例如 `src/delegated_safety.rs`，提供构造自定义 instruction table 的函数：

```rust
fn gravity_instructions<CTX>() -> EthInstructions<EthInterpreter, CTX> {
    let mut table = EthInstructions::new_mainnet();
    table.insert_instruction(CREATE, gravity_create::<false>);
    table.insert_instruction(CREATE2, gravity_create::<true>);
    table
}
```

`gravity_create` 的逻辑顺序为：

```text
1. 保留 upstream 的 static-call / hardfork availability 检查；
2. current = interpreter.input.target_address();
3. host.load_account_delegated(current)；
4. is_delegate_account_cold 为 Some(_) 表示 current code 是 delegation designator；
5. delegated -> 以 exceptional halt 结束当前 frame；
6. 非 delegated -> 调用 upstream CREATE/CREATE2 instruction。
```

数据库读取失败必须传播为 `FatalExternalError`，不能误当成“非 delegated”后继续创建。

建议第一版复用 revm 已有的 `InstructionResult::NotActivated` 作为 exceptional halt：它会消耗当前 frame 的剩余 gas，且不会创建 create frame，也不会增加 creator nonce。若该异常发生在可被上层 frame 捕获的内部调用中，上层仍可按普通 EVM 规则观察失败并继续；关键不变量是 create 没有发生。receipt 不暴露内部 halt reason，因此无需为了命名更漂亮而修改 revm 的 `HaltReason` 类型。若以后 trace API 需要区分原因，可以在 grevm 内扩展 halt reason，但不应成为第一版的必要条件。

检查必须发生在 upstream create instruction 解析参数、分配 create gas、构造 `FrameInput::Create` 之前。这样 delegated creator nonce 根本没有机会增加。

### 4.3 为什么不按“是否存在后续 delegated sender”有条件放行

仅在当前 block 内看，如果 `A` 后面不再作为 sender 或 authorization authority，允许 `A` 创建合约确实不会使**当前 block**出现 nonce mismatch。

但 grevm 当前只收到一个 block 的 `Vec<TxEnv>`，无法证明：

- 下一个已经排序但尚未执行的 block 中没有 `sender=A`；
- 其他 inflight block 中没有以 `A` 为 authority 的 EIP-7702 authorization；
- execution-delay window 内没有其他 nonce overlay 依赖 `A`。

因此“当前 block suffix 中没有 `A`”不等于“pipeline 中没有 `A`”。基于这个局部条件放行，会重新打开跨 block nonce 攻击。

第一版应与 Monad 一样无条件禁用 delegated `CREATE/CREATE2`。将来只有在 grevm 得到覆盖完整 execution-delay window 的 `PipelineNonceDependencies`，并且该输入属于共识数据时，才可以讨论安全放宽。为节省少量兼容性而引入这套跨层协议，不值得放在第一版。

## 5. Reserve Balance 语义

### 5.1 可以采用“执行后统一检查”，但不能只识别 delegate transfer

用户提出的简化方向是正确的：一笔交易的顶层 frame 结束后，检查最终 balance；不足则回滚 execution frame。

实现上不应只在某个 `CALL` 被标记为“delegate 转账”时检查，因为余额可能通过以下路径离开：

- 顶层或嵌套 `CALL/CALLCODE` value；
- create transaction 或内部 `CREATE/CREATE2` 的 value；
- `SELFDESTRUCT`；
- precompile 或以后复用 journal balance API 的新路径；
- 一笔交易内多次 debit/credit 的组合。

推荐在 grevm 内用 `TrackingJournal<J>` 包装 upstream journal，在统一余额变更入口记录本交易的 balance mutation，而不是扫描每个 opcode，也不是跟踪每个 call frame。只有本交易实际修改过 delegated/authorization-sensitive account 的余额时，才进入 reserve 慢路径并检查最终余额。

这里的“delegate 交易”不能只按 transaction type 或顶层 `to` 判断。下面三种余额 debit 都必须被追踪：

```text
sender=A(delegated) -> 普通合约 B
X -> A(delegated)
X -> 普通合约 B -> A(delegated)
```

第三种在交易入场时无法静态判断，但不需要跟踪 frame：只要 delegated `A` 最终尝试转出余额，必然经过 journal 的 `transfer`、`create_account_checkpoint`、`selfdestruct` 或其他统一 balance API。`TrackingJournal` 在这些入口记录 `A` 即可。若 delegated code 只读写 storage、没有余额变化，就不存在 reserve violation，不需要执行余额公式。

`TrackingJournal` 还应记录账户在本交易第一次 balance mutation 之前的余额。这样同一交易内先 credit、后 debit，或者先临时低于阈值、结束前再补回，都按 pre-transaction reserve 和最终净余额判断，而不是在中间状态提前失败。这与 Monad 的 `State::on_credit/on_debit + 顶层最终扫描` 语义一致。

### 5.2 两部分 reserve 阈值

对交易 `T_i` 结束后的账户 `A`，定义：

```text
required_after(i, A) = max(monad_floor_after(i, A), suffix_liability(i, A))
```

#### A. Monad 式 reserve floor

设：

```text
B0(A) = 当前 transaction 开始前 A 的余额
C      = Gravity 的协议级 max reserve cap
R0(A)  = min(C, B0(A))
```

对不是当前 sender、但属于 fixed-floor subject 的账户：

```text
monad_floor_after(i, A) = R0(A)
```

对当前 sender：

```text
monad_floor_after(i, sender)
  = R0(sender) - actual_charged_fee(T_i)，下限为 0
```

这里在 gas reimbursement 之后检查，因此使用最终实际收费；这与 Monad 在 reimbursement 前用“reserve 减最大 gas allowance”检查是等价的表达。blob fee 若会从 sender balance 扣除，也必须进入 `actual_charged_fee`。

含义是：delegated sender 可以用 reserve 支付本交易 gas，但 value transfer 或任意内部代码不能把余额压到 gas 扣除后的 floor 以下。非 sender 的 delegated account 不能因为被别人调用而侵蚀自己的 reserve。

`C` 是共识参数，不能使用环境变量或节点本地配置。第一版应在 grevm 内以链级常量或由 chain spec 确定的配置启用，并用 activation block/spec 固定。不能未经 Gravity 的 inflight gas 上限分析直接照抄 Monad 的 `10 MON` 数值。

#### B. 当前 block suffix 的精确义务

grevm 已经持有当前 block 的完整 `Vec<TxEnv>`，可以为每个 caller 逆序预计算后续交易真正需要的余额。不能简单求和所有 `max_balance_spending()`：每笔交易的 `max_fee` headroom 在执行后会退款，重复累加会过度锁定余额。

对 caller `A` 的一笔未来交易 `T_j`，定义：

```text
M_j = T_j.max_balance_spending()
    = gas_limit * max_fee_per_gas + value + max_blob_fee

E_j = T_j.effective_balance_spending(basefee, blob_gasprice)
    = gas_limit * effective_gas_price + value + effective_blob_fee
```

其中 `M_j` 是 upstream revm 在交易入场时要求能够覆盖的瞬时最大余额，`E_j` 是该交易执行后最坏情况下真正从余额中消耗的金额。忽略 gas refund 和其他交易给 `A` 的 incoming credit，是安全的保守估计。

按同一 caller 的交易逆序计算：

```text
need_after_last = 0

need_before(T_j) = max(
    M_j,
    E_j + need_after(T_j)
)

need_after(previous_tx_of_A) = need_before(T_j)

suffix_liability(i, A)
  = 第一笔满足 j > i 且 T_j.caller == A 的 need_before(T_j)
```

这个递推同时保证：

1. 每笔未来交易执行前满足 upstream revm 的 `max_balance_spending()` validation；
2. 按 `effective_balance_spending()` 扣除本交易最坏实际支出后，仍能覆盖再后面的交易；
3. 不会为每笔交易重复锁定最终会退款的 `max_fee - effective_fee` headroom。

schedule 对 block body 中所有尚未执行的 future transaction 建立义务；不会根据投机执行猜测某笔交易将来是否 `Skipped`。如果 future transaction 最终因其他动态原因被 skip，这会形成保守的额外预留，但能避免 speculative outcome 反向影响前序交易的共识语义。正常网络中 skip 应是异常兜底，不能用它优化 reserve plan。

它与 Gravity `filter_invalid_txs` 的现有模型一致：filter 先用 `max_fee * gas_limit + value` 做单笔入场校验，再从模拟余额扣除 `effective_gas_price * gas_limit + value`。本文要求不修改 revm，因此仍必须保护 future value 和 max-fee validation；这比 Monad 的 gas-only reserve 更严格，但不会像简单求和 `M_j` 那样过度保守。

实现可以使用：

```rust
struct ReservePlan {
    // 每个 caller 的 txid、M/E 及逆序 need_before schedule
    accounts: HashMap<Address, AccountReserveSchedule>,
}
```

`ReservePlan` 使用 `OnceLock<Result<ReservePlan, ReservePlanError>>` 或等价结构惰性构建：整个 block 没有 delegated balance debit 时，不计算 suffix schedule。首次激活 reserve 时构建复杂度为 `O(block_size)`，之后查询 `required_after(txid, address)` 可用二分做到 `O(log n)`。

算术采用 checked 语义：`M_j` 直接调用 revm `max_balance_spending()`；`E_j + need_after` 使用 `checked_add()`。任一 overflow 都生成 `ReservePlanError { txid: j, reason: OverflowPayment }`，作为 block fatal error 返回，不能包装成 reserve violation、不能记到首次初始化 plan 的更早 transaction，也不能 wrapping/饱和后继续。Gravity filter 正常情况下已用 saturating upper bound 移除这类交易，因此该错误是防御性不变量检查；两边必须增加 bypass/filter differential 测试。

### 5.3 为什么两部分都需要

只使用 block suffix 有一个明显漏洞：本 block 中 `A` 的最后一笔交易看到的 suffix 为 0，可以把余额全部转走，但下一个 inflight block 仍可能已有 `sender=A` 的交易。

只使用固定 floor 则无法保证当前 block 后续交易的较大 value 或 `max_fee_per_gas` 仍满足 upstream revm 的严格 balance validation。

因此推荐：

```text
固定 Monad floor
  -> 禁止在 grevm 不可见的 block 边界处清空余额，并提供 pipeline cushion

当前 block suffix liability
  -> 按 max-validation/effective-spending 递推，精确保证本 block 后续交易仍满足 revm 余额校验
```

完整的跨 block 证明仍依赖：共识侧保证 execution-delay window 内累计的实际 gas liability 不超过 `C`。如果 Gravity 暂时没有这项预算，固定 floor 仍能阻止“余额归零”攻击，但它只是 mitigation，不能声称已经达到 Monad 的完整 pipeline solvency 保证；第一阶段 `Skipped` receipt 仍是必要的最终活性兜底。

### 5.4 哪些账户需要检查

固定 Monad floor 推荐保护：

1. frame 开始时 code 为 EIP-7702 delegation designator 的账户；
2. authorization 在本交易 pre-execution 中刚设置或清除 delegation 的 authority；
3. 将来若共识提供完整 pipeline metadata，由 metadata 明确标记为 reserve-protected 的 EOA。

当前 block suffix liability 只在 `TrackingJournal` 发现 protected account 的 balance mutation 后查询，应用于本交易实际涉及的 delegated/authorization-sensitive 账户。不会为了保护普通 EOA 而让每笔普通交易进入慢路径。

这里不能直接照抄“所有空 code EOA 都应用 fixed floor”。Monad 共识侧知道 ordinary EOA 是否满足 emptying-transaction 例外；grevm 独立执行一个 block 时没有完整 pipeline metadata。如果无条件保护所有空 EOA，会使普通 ETH sender 只能支付 gas、不能正常转 value。第一版应把 fixed floor 和 suffix check 都收敛到本交易实际涉及的 delegated/authorization-sensitive 账户。普通 smart contract 不需要 fixed floor，因为它不能签名发起未来 transaction。

系统账户或特殊 precompile 若需要豁免，必须通过链级静态列表定义并测试，不能使用调用方临时开关。

subject 判定使用 transaction-start/authorization 语义，不能只看交易结束时的 code：

- sender 是否在 authorization 之前已经 delegated，应在 `validate_against_state_and_deduct_caller()` 加载 caller code 后记录；
- 本交易 authorization 成功设置或清除 delegation 的 authority，应加入 authorization-sensitive set；
- execution 中普通合约 code/storage 变化不能把一个原本受保护的 delegated EOA 从候选集中移除。

sender 的 pre-transaction balance 必须在 upfront gas deduction 前单独保存。其他账户由 `TrackingJournal` 在 execution tracking 启用后的第一次 balance mutation 时保存原始余额；authorization 本身不改变 balance，因此 checkpoint 开始时余额就是它们的 pre-transaction balance。

### 5.5 Reserve violation 的状态转换

正确的失败语义是：

```text
保留：
  - transaction sender nonce 增量
  - upfront gas deduction 与未使用 gas reimbursement
  - beneficiary priority fee（并行路径仍由 lazy reward 在 commit 时应用）
  - EIP-7702 authorization code/authority nonce 处理

回滚：
  - 顶层 EVM frame 内的 balance transfer
  - storage/code/nonce 变化
  - logs
  - selfdestruct/create 等执行效果

输出：
  - receipt status = 0
  - gasUsed = 本交易实际消耗，reserve violation 不享受 EVM 或 EIP-7702 gas refund
  - TxExecutionOutcome::Executed(ExecutionResult::Revert 或等价失败)
```

它不能返回 `InvalidTransaction`，也不能进入 `SkipReason`。否则危险的前序交易会再次变成免费 no-op，违背第二阶段目的。

## 6. Handler 生命周期设计

当前并行路径使用 `NoRewardHandler`，只重写 `reward_beneficiary`；sequential fallback 则通过 `EthEvm::transact_raw` 使用默认 handler。实现本设计后，两条路径必须统一到 grevm handler。

建议把实现拆成两个独立组件：

```rust
struct GravityHandler<'a, EVM, ERROR, FRAME> {
    txid: TxId,
    reserve_plan: &'a OnceLock<Result<ReservePlan, ReservePlanError>>,
    reward_mode: RewardMode, // Deferred / Immediate
}

struct TrackingJournal<J> {
    inner: J,
    reserve: ReserveTracker,
}

struct ReserveTracker {
    phase: TrackingPhase, // Off / PreExecution / Authorization / Execution
    sender: Address,
    sender_pre_tx_balance: U256,
    sender_was_delegated: bool,
    authorization_sensitive: SmallSet<Address>,
    original_balances: SmallMap<Address, U256>, // address -> pre-execution balance
    debited_accounts: SmallSet<Address>,
    create_tx_sender_nonce_bumped: bool,
    checkpoints: Vec<TrackerCheckpoint>,
}
```

`TrackingJournal<J>` 实现 `JournalTr` 并 inline 转发所有普通操作，只在以下统一状态入口增加轻量 tracking：

- `load_account_code`：在 pre-execution tracking 期间复用 revm 已有的 caller load，记录 sender 在 authorization 前是否 delegated；
- `caller_accounting_journal_entry`：使用其 `old_balance` 参数记录 sender 的 pre-transaction balance；
- `transfer`：仅当成功转移的 value 非零时记录 debit 的 `from`，并为 `from/to` 保存首次 balance mutation 前余额；零值 touch 不进入 reserve candidate；
- `create_account_checkpoint`：仅在 create value 非零时记录 caller debit 及首次 mutation 前余额；
- `selfdestruct`：仅在 `had_balance` 非零时记录被销毁账户 debit，并为 address/target 保存首次 mutation 前余额；
- `balance_incr`：在 execution tracking 且 amount 非零时只保存首次 credit 前余额，不把纯 credit 账户标记为 debit；
- `set_code_with_hash`：在 authorization tracking 期间记录成功设置/清除 delegation 的 authority；
- `nonce_bump_journal_entry`：结合当前 phase 记录 authorization-sensitive authority，或识别顶层 create transaction sender nonce 是否在 outer checkpoint 内增加；
- `checkpoint/checkpoint_commit/checkpoint_revert`：同步维护 tracker checkpoint，使已回滚子 frame 的候选项和快照不会残留。

这比 `GravityFrame` 更接近 Monad 的统一 State tracking：所有现有及未来复用 `JournalTr` balance API 的路径自动受保护，不需要复制 upstream `EthFrame::make_call_frame`，也不需要在每个 call frame 额外判断 delegation。

debit candidate 是否属于 protected subject 在 transaction-final check 时根据 transaction-start code、authorization-sensitive set 和 journal 中已经加载的 account code 判断。普通合约 debit 可以留在很小的 candidate set 中并被快速过滤，不触发 reserve policy；纯 credit 不触发慢路径，但其 original balance snapshot 会供同交易后续 debit 使用。

自定义 `run_without_catch_error` 的顺序：

```text
1. validate()
2. reserve_tracker.begin_transaction(tx.caller)
3. 拆分执行 pre_execution()：
   - start_pre_execution_tracking()
   - validate_against_state_and_deduct_caller()
     - 复用 load_account_code 结果捕获 sender 是否 delegated
     - 复用 caller_accounting_journal_entry.old_balance 捕获 sender pre-transaction balance
     - 扣除 gas 并增加 call-transaction nonce
   - stop_pre_execution_tracking()
   - load/warm accounts
   - start_authorization_tracking()
   - apply EIP-7702 authorization
   - stop_authorization_tracking()，保留成功应用的 authorization-sensitive authorities
4. journal.checkpoint()，记为 execution_checkpoint
5. reserve_tracker.start_execution_tracking()
6. execution()
7. reserve_tracker.stop_execution_tracking()
8. refund() + EIP-7623 floor calculation
9. reimburse_caller()
10a. 没有 protected debit candidate：
     - 直接 checkpoint_commit()
     - 不初始化 ReservePlan，不执行余额公式
10b. 存在 protected debit candidate：
     - 惰性初始化 ReservePlan
     - 计算 debit candidate 的 final balance 与 required_after
11a. 全部满足：checkpoint_commit()，随后 reward_beneficiary()
11b. 任一违反：
     - 在 revert 前保存 create_tx_sender_nonce_bumped
     - checkpoint_revert(execution_checkpoint)
     - 把 frame result 改为空 output 的 Revert
     - 清零全部 gas refund，保留已经消耗/剩余的 gas
     - 如果顶层 create transaction sender nonce 曾在 checkpoint 内增加，重新增加一次
     - 重新 reimburse_caller()
     - reward_beneficiary()
12. execution_result()
```

第 9 步先 reimbursement 再检查，能直接读取真正的 transaction-final balance。发生 violation 时 checkpoint revert 会同时撤销第一次 reimbursement；随后按失败 frame 的 gas 数据重新 reimbursement，因此不会重复退款。

checkpoint 必须在 `pre_execution()` **之后**建立。若放在 transaction 最外层，violation 会把 sender nonce、gas deduction 和 authorization 一起回滚，错误地退化成免费 skip。

为了覆盖运行到中途才发生的 `X -> B -> A(delegated)` debit，execution checkpoint 仍需为每笔已通过 pre-execution 的交易建立；否则发现 `A` 余额变化时已经来不及回滚 `B` 之前产生的顶层状态。`checkpoint()`/`checkpoint_commit()` 只是 journal depth/index 的 `O(1)` 操作。

### 6.1 顶层 create transaction 的 nonce 特例

revm 29.0.1 的 transaction nonce 时序存在一个必须显式处理的差异：

- call transaction 的 sender nonce 在 `validate_against_state_and_deduct_caller()` 中增加，位于 execution checkpoint 之前；
- create transaction 的 sender nonce 在 `EthFrame::make_create_frame()` 中增加，位于 execution checkpoint 之后。

因此 reserve violation 回滚 outer checkpoint 时，call transaction nonce 会自然保留，但 create transaction nonce 会被一起回滚。直接依赖“checkpoint 位于 pre-execution 后”不足以满足文档的 nonce 语义。

`TrackingJournal::nonce_bump_journal_entry()` 应在 execution tracking 期间识别：

```text
tx.kind == Create
且 bumped address == tx.caller
```

命中后设置 `create_tx_sender_nonce_bumped = true`。发生 reserve violation 时，必须在 `checkpoint_revert()` 前把该 flag 保存到 Handler 局部变量；tracker checkpoint rollback 不能让 Handler 丢失这个事实。随后仅在保存值为 true 时重新对 sender 增加一次 nonce 并写入正常 journal entry。这样：

- transaction nonce 仍按合法收费失败交易推进一次；
- initcode 内部 `CREATE/CREATE2` 或其他合约 nonce 全部随 execution rollback；
- authorization authority nonce 仍因位于 checkpoint 之前而保留。

不能无条件重增 create sender nonce，因为 nonce overflow、frame 初始化提前失败等路径可能根本没有完成原始 nonce bump。

发生 violation 后把 refund 设为 0，但保留执行到检查点为止的剩余 gas。因此它是收费 `REVERT` 语义，不是消耗整个 gas limit 的任意惩罚，也不是成功交易。

### 6.2 TrackingJournal 的正确性要求

tracker 是非共识 sidecar，但它决定是否执行共识检查，因此必须与 journal frame 生命周期一致：

1. `checkpoint()` 同时保存 candidate/snapshot undo position；
2. `checkpoint_revert()` 删除被回滚子 frame 新增的 candidates，并恢复其 snapshot；
3. `checkpoint_commit()` 合并 tracker frame；
4. `discard_tx()`、`finalize()` 和 worker incarnation reset 都清空 tracker；
5. reimbursement、beneficiary reward 和 lazy reward 不属于 EVM reserve debit tracking，必须在 `stop_execution_tracking()` 后执行。

保存 pre-balance 时不能为了方便再次访问底层 DB。应利用 balance API 已加载的 account，或者在变更前/后从 journal cache 推导旧余额。若某个新 revm 版本为 balance 增加新的 `JournalTr` 入口，wrapper 因 trait 变化编译失败，强制开发者显式决定是否纳入 tracking；这比复制 frame/opcode 实现更容易安全升级。

### 6.3 普通交易 fast path 的成本预算

未触及 delegated context 的普通交易只增加：

```text
一次 transaction-level checkpoint()       O(1)
JournalTr balance API 上一次 tracking 布尔分支
一次 checkpoint_commit()                  O(1)
```

若 execution 确实发生非零原生余额变化，还会按实际 mutation address 向 small-map/small-set 插入一次；普通原生 ETH transfer 通常只有 1～2 个地址，并在 subject filter 后直接结束，不构建 `ReservePlan`。

它不会：

- 构建 `ReservePlan`；
- 遍历完整 journal entries；
- 额外读取 account balance；
- 额外查询 account code；
- 计算 suffix liability；
- 包装或复制 `EthFrame`；
- 改变现有 lazy reward、MV memory 或 commit 流程。

典型 ERC-20/Uniswap 交易的 EVM execution 很少发生原生 balance debit，因此 tracker 热路径主要是可预测的 `tracking` 分支。`CREATE/CREATE2` 的 delegation 检查也只在实际执行这两个 opcode 时发生。上线门槛应包含普通 ETH transfer、ERC-20 transfer 和 Uniswap workload 的 A/B benchmark；若普通 workload 的吞吐回退超过预先设定阈值，应优化 wrapper inline、small-map 和 tracker checkpoint 数据布局，不能退化为重复 account lookup 或逐 call-frame 检查。

## 7. 与 Grevm 并行执行的集成

### 7.1 两条执行路径必须共用同一构造函数

新增类似：

```rust
fn build_gravity_evm(db, cfg, block, precompiles) -> GrevmEvm
```

统一完成：

- upstream mainnet context；
- grevm-local `TrackingJournal<Journal<DB>>`；
- Gravity `CREATE/CREATE2` instruction table；
- static/custom precompiles；
- 并行或顺序路径所需的 DB；
- `GravityHandler` 所需类型。

并行 worker：

- 继续设置 `cfg.disable_nonce_check = true`；
- 使用 `RewardMode::Deferred`，保持现有 lazy reward；
- 每次执行传入当前 `txid` 和共享的 `Arc<OnceLock<Result<ReservePlan, ReservePlanError>>>`；
- 每次 incarnation 开始前重置 `TrackingJournal::ReserveTracker`。

sequential fallback：

- 不再直接调用 `EthEvm::transact_raw`；
- 与并行路径一样执行 `set_tx -> GravityHandler::run -> finalize`；
- 使用 `RewardMode::Immediate`，或沿用等价的顺序 reward 结算；
- recoverable invalid transaction 仍按第一阶段规则生成 `Skipped`。

这样 `MIN_PARALLEL_TXS`、`FALLBACK_SEQUENTIAL`、并行异常 fallback 与正常 parallel execution 不会出现协议语义分叉。

### 7.2 读写集与投机重执行

delegation 判断、pre-balance、final balance、subject-account code 都必须经当前 `CacheDB`/journal 读取，使其进入 grevm read set。

如果前序交易改变了某账户的 balance 或 delegation code：

```text
投机执行可能先得到 pass 或 violation
  -> validation 发现读版本改变
  -> 当前 incarnation 作废
  -> 在更新后的前缀状态重新执行
  -> 到 commit frontier 后结果才最终确定
```

`ReservePlan` 只依赖 block 内固定的 tx order 和 `TxEnv`，可以通过 `OnceLock` 安全地跨线程惰性初始化和共享。reserve violation 产生的回滚 state 进入正常 write set；不能绕过 validation 直接提交。

### 7.3 与第一阶段 Skipped Receipt 的关系

结果分类保持：

```text
revm transaction validation 的 recoverable invalid
  -> TxExecutionOutcome::Skipped(reason)

正常 EVM revert/halt
  -> TxExecutionOutcome::Executed(failed result)

delegated CREATE/CREATE2 halt
  -> 正常 EVM frame failure；若未被上层捕获则是 Executed(failed result)

reserve violation
  -> TxExecutionOutcome::Executed(failed result)
```

禁止 delegated create 或 reserve violation 不能新增 `SkipReason`。

### 7.4 激活与可扩展策略接口

两个功能都是 Gravity 共识执行语义，必须由同一个 chain-derived 配置控制，不能使用环境变量或节点本地开关：

```rust
struct DelegatedSafetyConfig {
    enabled: bool,
    max_reserve_balance: U256,
    reserve_policy_version: ReservePolicyVersion,
}
```

`enabled` 必须由 block number/timestamp 对应的 Gravity hardfork 决定，并同时作用于：

- parallel instruction table；
- sequential fallback instruction table；
- `TrackingJournal` reserve tracking；
- `GravityHandler` final enforcement。

不要把 current-block schedule 的具体公式硬编码进 Handler。建议通过内部只读策略接口隔离：

```rust
trait ReservePolicy {
    fn required_after(
        &self,
        txid: TxId,
        account: Address,
        pre_tx_balance: U256,
        actual_charged_fee: U256,
    ) -> Result<U256, ReservePlanError>;
}
```

第一版实现 `FixedFloor + CurrentBlockSuffixPolicy`；将来如果共识提供 pipeline liabilities，只需增加 `PipelineBudgetPolicy` 或把其结果作为额外 floor，不需要修改 journal tracking、rollback 和 reward 逻辑。

## 8. 对两个简化问题的直接回答

### 8.1 后续没有 delegated account 发起交易时，能否允许 CREATE/CREATE2

理论上可以，但条件必须是“完整 execution-delay pipeline 中没有任何 sender/authority nonce 依赖”，而不是“当前 block 后面没有”。grevm 当前无法独立证明该条件。

所以第一版答案是：**不允许，始终禁用 delegated context 中的 `CREATE/CREATE2`。**

### 8.2 能否在 delegate 转账执行后检查余额，不够支付后续 gas 和转账就 revert

**可以，这正是推荐的实现形态，但要做三点修正：**

1. 在整个顶层 frame 结束后检查，而不是只钩住某一笔 delegate `CALL`。
2. 通过 `TrackingJournal` 覆盖所有 debit 路径并检查最终净余额；当前 block 后续交易用 `max_balance_spending()` 通过单笔入场门槛，再用 `effective_balance_spending()` 逆序递推真正的 suffix 预算。
3. “revert”只回滚 EVM frame，必须保留 nonce、authorization 和实际 gas 收费。

此外，当前 block suffix 不能代表完整 pipeline。即使 suffix 为 0，仍应用 Monad 式固定 reserve floor，不能允许 delegated account 在 block 末尾清空余额。

余额慢路径只在 `TrackingJournal` 发现 delegated/authorization-sensitive account 的 balance mutation 后启用。普通交易只承担 transaction checkpoint/commit 和 balance API 上的 tracking 分支，不扫描完整 journal、不构建 `ReservePlan`。

还有一个无法由 execution rollback 修复的前置条件：如果账户初始余额本来就不足以同时覆盖当前交易实际 gas 与未来预算，回滚 value transfer 也不会创造 gas 资金。Monad 用共识侧 inflight budget 保证这个前置条件。Gravity 若暂时没有对应预算，仍保留第一阶段 skip 作为活性兜底，但不能把 grevm execution reserve 宣称为完整的跨 block 资金证明。

## 9. 测试计划

### 9.1 CREATE/CREATE2

1. delegated EOA 直接执行 `CREATE`：失败，creator nonce 不增加，交易收费。
2. delegated EOA 经 `DELEGATECALL` 执行 `CREATE2`：失败。
3. delegated EOA 普通 `CALL` 合约 `B`，由 `B` 执行 `CREATE`：成功。
4. 普通合约 `CREATE/CREATE2`：与 upstream revm 一致。
5. 当前 block 最后一笔 delegated sender 交易执行 `CREATE`：仍失败。
6. delegation/code DB load error：返回 fatal error，不继续执行 create。

### 9.2 Reserve Balance

1. `A` drain 后当前 block 还有 `sender=A`：drain tx 收费失败，转账/storage/log 回滚，后续交易正常执行。
2. `X` 调用 delegated `A` 并尝试 drain `A`：`X` 的交易收费失败，`A` reserve 保留。
3. 最终余额恰好等于阈值：通过；少 1 wei：失败。
4. 当前 block 最后一笔 delegated tx 试图清空余额：因固定 floor 失败。
5. balance 先低于阈值、结束前补回：通过。
6. `SELFDESTRUCT`、create value、nested calls 均被覆盖。
7. incoming credit 增加可用余额，最终阈值判断正确。
8. EIP-1559 max fee、effective gas price、transaction value、blob fee 的逆序 suffix 递推正确。
9. reserve violation 清除 logs/refund，但保留 sender nonce、authorization 与实际 gas fee。
10. reserve violation 结果是 `Executed(failure)`，不是 `Skipped`。
11. delegated sender 的顶层 create transaction 触发 reserve violation：sender transaction nonce 仍只增加一次。
12. 顶层 create frame 在 nonce bump 前失败：reserve handler 不得凭空增加 sender nonce。
13. `max_fee` 远高于 effective gas price 时，suffix 使用逆序 `max(M, E + next)`，不会重复累计 fee headroom。
14. internal frame 中 delegated debit 后 revert：tracker candidate/snapshot 随 tracker checkpoint 回滚。

### 9.3 Parallel / fallback 一致性

1. 前序交易给 `A` credit，使投机 violation 变为 pass：validation 后重执行。
2. 前序交易 debit `A`，使投机 pass 变为 violation：validation 后重执行。
3. parallel、强制 sequential、低于 `MIN_PARALLEL_TXS` fallback 的 state、receipts、gas、reward 完全一致。
4. 并行执行中 reserve failure 的 lazy beneficiary reward 与顺序 immediate reward 最终一致。
5. 已提交 prefix 后触发 fallback，只重放 suffix 时仍使用相同 `ReservePlan(txid)`。
6. recoverable invalid、delegated-create halt、reserve violation 混合时结果顺序稳定。
7. `X -> B -> A(delegated)` 的 debit 能激活 reserve，不能通过普通合约中转绕过。
8. delegated code 只读写 storage、未改变 protected balance 时不初始化 `ReservePlan`。
9. worker re-execution/incarnation reset 后 tracker 不保留上一次投机候选项。

### 9.4 性能回归

1. 普通 ETH transfer、ERC-20 transfer、Uniswap workload 启用前后对比吞吐、CPU 和 re-execution 次数。
2. 无 delegated balance mutation 的 block 验证 `ReservePlan` 保持未初始化。
3. delegated 交易比例为 0%、0.1%、1%、10% 时分别测试慢路径成本。
4. delegated 交易产生超长 storage journal、但只有少量 balance mutation 时，tracker 成本与 balance mutation 数量而不是完整 journal 长度相关。

## 10. 实施顺序

1. 抽取统一的 `build_gravity_evm`，先消除 parallel 与 sequential handler/instruction 构造差异。
2. 实现并测试 delegated `CREATE/CREATE2` instruction override。
3. 实现 `TrackingJournal<Journal<DB>>`、tracker checkpoint 和普通交易 fast path。
4. 实现惰性 `ReservePlan` 的 `max-validation/effective-spending` 逆序递推与 overflow 处理。
5. 将 `NoRewardHandler` 重构为支持 checkpoint、reserve final check 和 reward mode 的 `GravityHandler`。
6. 补齐 create transaction nonce rollback/reapply 和 authorization-sensitive tracking。
7. 完成 parallel conflict/re-execution、sequential equivalence、filter differential 和普通 workload 性能测试。
8. 用 hardfork/activation block 同时启用两个协议语义，并在启用前固定 Gravity reserve cap；若没有共识侧 inflight gas budget，发布说明必须把跨 block 部分标为 mitigation。

## 11. 最终建议

- 不 fork revm；所有差异放在 grevm 的 instruction table、`TrackingJournal`、handler 和 scheduler policy data 中。
- delegated `CREATE/CREATE2` 第一版无条件禁用，不做当前 block suffix 特判。
- 采用顶层执行后统一余额检查；这是可行且覆盖面最完整的简化。
- 用 `TrackingJournal` 在统一 balance API 记录 mutation，不复制 `EthFrame`、不逐 opcode 拦截、不扫描完整 journal。
- reserve 慢路径只对实际发生 protected balance mutation 的交易启用，`ReservePlan` 惰性初始化。
- 余额阈值采用“Monad 固定 floor + 当前 block 逆序 max/effective suffix liability”的较大值。
- reserve failure 必须是保留 nonce/gas/auth 的收费失败交易。
- create transaction nonce 位于 revm execution checkpoint 内，reserve rollback 后必须按记录精确 reapply 一次。
- 把“grevm execution enforcement”“当前 block 完整 suffix 保护”和“跨 block pipeline guarantee”分开验收；前两者可只改 grevm，最后一项仍需要共识预算或完整 pipeline liability 输入。

## 12. 实现落地记录

实现日期：2026-07-16。

本轮实现仍保持 upstream `revm 29.0.1`，没有 fork revm。协议实现集中在 `src/delegated_safety/`：

- `DelegatedSafetyConfig`：默认 disabled，避免现有 `Scheduler::new` 调用发生共识语义变化。
- `GrevmConfig::delegated_safety`：由上层在 Gravity 激活高度和 reserve cap 已确定后显式启用。
- `gravity_instructions()`：替换 `CREATE/CREATE2` opcode。执行顺序为 upstream static/hardfork gate -> delegated recipient 检查 -> upstream create。
- `TrackingJournal<Journal<DB>>`：包装 upstream journal，记录 execution phase 中的 balance debit、原始余额、delegated subject、authorization-sensitive account、create transaction sender nonce bump，并随 checkpoint commit/revert 同步回滚 tracker sidecar。
- `GravityHandler`：统一 parallel 和 sequential fallback 的 transaction 生命周期。reserve violation 回滚 pre-execution 之后的 outer checkpoint，输出收费 `Revert`，并保留 sender nonce、EIP-7702 authorization、实际 gas 收费。
- `TrackingPrecompilesMap`：为 tracking context 适配 alloy `PrecompilesMap`，保证 custom/stateful precompile 仍能通过 `EvmInternals` 访问 journal。

当前 enabled 后两条执行路径语义一致：

- parallel worker 使用 `RewardMode::Deferred`，保持 grevm lazy reward；
- grevm sequential fallback 使用同一套 instructions/journal/handler，但 `RewardMode::Immediate`；
- disabled 时仍走原有 EVM 构造和 `NoRewardHandler`/`EthEvm::transact_raw` 路径，不影响现有性能。

### 12.1 三轮审计记录

第一轮：nonce/auth/gas/checkpoint。

- 确认 call transaction sender nonce 位于 outer checkpoint 之前，reserve rollback 自然保留。
- 确认 create transaction sender nonce 位于 create frame 内，已通过 tracker flag 在 violation 后精确 reapply 一次。
- 确认 EIP-7702 authorization 位于 outer checkpoint 之前；成功 authorization 通过前后 nonce snapshot 标记为 authorization-sensitive，避免把无效 authorization 误纳入保护集合。
- 确认 reserve violation 清零 refund，但保留已消耗 gas，不退化为 free skip。

第二轮：parallel/MV/fallback/perf。

- parallel 和 fallback 共用 safety EVM 构造；ReservePlan 按原始 txid 查询 future suffix，已提交 prefix 不影响 suffix replay。
- reserve/delegation/final balance 读取均经过当前 journal/CacheDB，投机执行读集可被 validation 捕获并重放。
- 新增 parallel/fallback bundle differential 测试，覆盖 lazy reward 与 immediate reward 的最终状态一致性。
- 修正 `balance_incr` tracking：只有 execution phase 才保存 original balance，post-execution reimbursement/reward 不走无意义 slow tracking。

第三轮：adversarial opcode / frame path。

- 修正 delegated CREATE guard 的顺序：先执行 upstream static-call 和 CREATE2 hardfork gate，再做 delegated recipient 检查，避免改变 static/hardfork 场景语义。
- 补充 delegated `CREATE2` 被阻止测试。
- 补充 delegated code 普通 `CALL` 合约 `B`、由 `B` 执行 `CREATE` 仍成功的测试。
- 补充 `SELFDESTRUCT` drain 触发 reserve violation 并回滚的 safety 测试。

### 12.2 已执行验证

已执行命令：

```bash
cargo check
cargo test --lib
cargo test --features test-utils --test eip-7702
cargo test --features test-utils
```

验证结果：

- `cargo check` 通过。
- `cargo test --lib` 通过，包含 `ReservePlan` 逆序公式单测。
- `cargo test --features test-utils --test eip-7702` 与 `--test delegated_safety` 通过，当前分别包含 12 个通用 EIP-7702 回归测试和 8 个 delegated safety 端到端测试。
- `cargo test --features test-utils` 通过，包含 lib、EIP-7702、ERC20、mainnet fixture、native transfer、Uniswap 和 doctest。

### 12.3 当前仍需上层接入的内容

grevm 已提供执行侧能力，但默认不启用。Gravity 上层仍需明确：

1. 激活高度或 chain spec 条件；
2. 协议级 `max_reserve_balance`；
3. 若要声明完整跨 block pipeline solvency，需要共识侧 inflight gas budget 或 pipeline liability 输入。当前 grevm 实现只覆盖当前 block suffix + fixed floor mitigation。

## 13. 2026-07-17 抽象与执行路径整改

本轮在不改变上述共识语义的前提下，对配置、调度、EVM 驱动和测试进行了分层整改。

### 13.1 统一配置

新增 `GrevmConfig`，统一承载：

- `concurrency_level`
- `force_sequential`
- `min_parallel_txs`
- `delegated_safety`

`GrevmConfig::from_env()` 只在 scheduler 构造时读取一次兼容环境变量；需要共识稳定配置的上层应显式构造配置并调用 `Scheduler::new_with_config(...)`。`Scheduler::execute()` 完全使用该配置。旧 `Scheduler::new(...)` 和 `parallel_execute(Some(...))` 保留兼容，`new_with_delegated_safety(...)` 标记为 deprecated。

旧 `ASYNC_COMMIT_STATE=false` 会推进 commit cursor 却不提交 state/outcome，并不是合法的“同步提交”模式。本轮安全审计将其移除；parallel execution 现在始终完成有序 state/outcome commit，benchmark 也测量完整执行成本。

### 13.2 单一调度执行状态机

删除原先重复的 `execute_with_safety`。现在：

1. `StandardExecutor` 与 `SafetyExecutor` 只负责 EVM 构造、handler 驱动和 journal finalize；
2. 两者实现同一个 `ParallelTransactionExecutor` 接口；
3. scheduler 只有一个 `run_worker` 和一个 `execute_task`，冲突检测、read/write set、MVMemory、dependency、incarnation 和 validation 状态推进只有一份实现；
4. sequential fallback 也只有一个 `execute_sequential_suffix` 负责 invalid skip、结果排序和 metrics，standard/safety 仅提供不同的 transact closure。

disabled fast path 仍构造原始 upstream journal/EVM，不引入 `TrackingJournal`、自定义 instruction 或逐交易 safety 判断。enabled 判断仅在每个 worker 和 fallback 建立执行器时发生一次。

### 13.3 模块职责

- `src/config.rs`：公开运行配置及环境变量兼容入口。
- `src/model.rs`：scheduler 与 speculative database 共享的内部状态模型和 MVMemory 类型。
- `src/outcome.rs`：公开执行结果、skip reason 和错误类型。
- `src/scheduler/context.rs`：lock-free cursor 与 logical timestamp。
- `src/scheduler/executor.rs`：standard/safety EVM adapter 与 lazy reward handler。
- `src/scheduler/fallback.rs`：顺序 suffix replay。
- `src/scheduler/metrics.rs`：metrics 定义与采集。
- `src/scheduler.rs`：只保留 block orchestration、并发 finality/commit 和调度状态机。
- `src/cache_db.rs`：speculative MVMemory database adapter。
- `src/bundle.rs`：parallel transition/bundle/revert 生成。
- `src/utils.rs`：无业务语义的 fork-join 分区和连续索引工具；`fork_join_util` 仅在 crate root 保留兼容 re-export。
- `src/delegated_safety/*`：协议策略、instruction、tracking journal、handler、precompile adapter。

### 13.4 新增测试覆盖

delegate safety 测试从通用 EIP-7702 回归文件中独立到 `tests/delegated_safety.rs`，覆盖：

1. delegated `CREATE` 与 `CREATE2` 均阻止且不产生额外 nonce；
2. safety disabled 时保持 upstream delegated CREATE 语义；
3. delegated code 普通 CALL 到非 delegated 合约后，后者 CREATE 仍允许；
4. reserve violation 回滚 value/state，保留 authorization、nonce 和 sponsor 实际 gas fee；
5. 最终余额恰好等于 reserve 时允许；
6. inner frame debit 后 revert 不产生 tracker 假阳性；
7. `SELFDESTRUCT` 与 CALL debit 使用同一 reserve policy；
8. parallel 与配置强制 sequential 的 outcomes、bundle 完全一致。

另新增 tracker checkpoint commit/revert、普通账户 debit fast filter、reserve floor/suffix 组合和统一配置的单元测试。

### 13.5 本轮审计结论

- 抽象审计：standard/safety 不再复制 scheduler 状态机；配置不再散落读取；lib、scheduler、fallback、EVM adapter 和公共结果类型职责分离。
- 安全审计：复核 authorization 位于 outer checkpoint 前、execution effects 位于 checkpoint 内、reserve failure 的 refund/nonce/gas 处理，以及 tracker 子 frame checkpoint 回滚；新增边界和 adversarial frame 测试。
- 性能审计：disabled 路径无 tracking wrapper；enabled 路径 ReservePlan 仍只在存在 protected debit 时惰性初始化；worker 内不做动态分发，trait 通过泛型静态单态化。

### 13.6 最终验证

```bash
cargo test --features test-utils
cargo check --release --all-targets --features test-utils
cargo fmt --check
git diff --check
```

全部通过。全量测试包含 19 个 lib 单测、8 个 delegated safety E2E、12 个通用 EIP-7702 回归测试，以及 ERC20、mainnet fixture、native transfer、Uniswap 和 doctest。

`cargo clippy --lib --features test-utils --no-deps` 可完成检查；仓库既有 `hint.rs`、`parallel_state.rs`、原 CacheDB 实现和 test-utils 仍有历史 clippy warning，因此当前尚未把全仓 `-D warnings` 作为通过条件。本轮新增的 config、scheduler adapter/context/fallback/metrics、bundle 和 delegated-safety 变更未产生新增 clippy warning。
