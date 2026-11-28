# openbilling-core

The types of openbilling-rs and its storage ports. No I/O, no async runtime, one dependency (`thiserror`). Every other
crate shares this vocabulary, taken from [ADR 0001](../../docs/adr/0001-postgres-is-the-queue.md),
[ADR 0002](../../docs/adr/0002-an-owner-holds-several-accounts.md) and
[ADR 0004](../../docs/adr/0004-a-versioned-price-book-in-sql.md).

**No rule of the ledger is here** (agent guide, rule 12). The draw order, the window, the checks of a batch, the
once-only keys, the reversal cap, the reads, and the price book's checks, rating and rounding are SQL, in
`openbilling-pg`'s migrations. This crate names what goes
in and out of them; its constructors only hold what a type is (an id's alphabet, an amount's range).

| Type | What |
|---|---|
| `EventId`, `AccountId`, `MemberId`, `CreditId` | Caller-given ids: 1 to 128 bytes of printable ASCII (`!` to `~`). No whitespace, control character or non-ASCII text, so ids that look alike are the same bytes and log safely |
| `OwnerId` | The external subject that holds accounts, such as an OpenID Connect `sub`: the same alphabet, up to 255 bytes |
| `CurrencyCode`, `AccountLabel` | The operator's names: 1 to 16 bytes of `A`–`Z`, `0`–`9`; 1 to 64 bytes of `a`–`z`, `0`–`9`, `_`, `-` |
| `MeterId`, `DimensionKey`, `DimensionValue` | The price book's names: 1 to 64 bytes of `a`–`z`, `0`–`9`, `.`, `_`, `-` (`tokens.input`, `model`); a value follows the rule of ids |
| `Amount` | Money as an integer number of **micro-units** of one currency. Never a float. Checked arithmetic |
| `Timestamp`, `UtcDay`, `UtcMonth` | UTC to the microsecond since the Unix epoch, a day, a calendar month; no time library |
| `LedgerConfig`, `LotOrder`, `WithinKind`, `Window` | What the operator configures: currencies, labels in draw order, lot kinds in draw order, the intake window |
| `CreditSource`, `LotKind`, `ReversalKind` | Closed sets: `payment`, `promotion`, `adjustment`; `allowance`, `promotion`, `adjustment`, `payment`; `refund`, `chargeback` |
| `NewAccount`, `Credit`, `CreditTarget`, `Reversal`, `UsageEvent` | What a caller asks the ledger to write. A `UsageEvent` is pre-priced: it carries its amount |
| `RawEvent`, `Quantity`, `Dimensions` | A raw usage event: a quantity (a whole number, zero or more) of a meter, with its dimensions, rated at intake (ADR 0004) |
| `Rated`, `Loaded`, `PriceVersionId`, `PriceVersionView` | What the price book answers: an amount and the version that priced it; a loaded version, its time and content hash, and whether it was a replay |
| `Opened`, `Credited`, `IntakeReport`, `DrainReport`, `Balance`, `Allowed`, `LotView`, `OutcomeView` | What it answers. A decision and an outcome name the price book version of a raw event |
| `EventOutcome`, `RefusalReason` | What a drain decided: `applied`, or `refused` with `no_price` (reserved), `unknown_account`, `id_reused` or `overflow`. No `limit`: usage is booked as debt past the limit (ADR 0001, amended 2026-11-27, E1) |
| `AllowedPolicy`, `RefuseWhen`, `NotAllowed` | How `LedgerStore::allowed` turns its numbers into a verdict: one `RefuseWhen` per check (`debt_limit`, `balance`, `member_cap`), each `AtOrAbove` (the default), `Above` or `Never`; the deployer's choice, which the store only compares by (the owner's decision of 2026-11-27, ADR 0001, amended, F1 to F4). `Allowed` carries the numbers whatever the verdict |
| `LedgerStore` | The storage port: a trait whose methods return `Send` futures. `openbilling-pg` implements it; `openbilling-testkit` holds its conformance suite. `allowed` takes an `AllowedPolicy` (since 2026-11-27: a change every implementation must follow) |
| `PartitionRetention`, `RetentionMargin`, `RetentionReport` | The partition retention port (ADR 0001, point 10, amended 2026-11-28): `drop_expired_partitions(margin)` drops the usage partitions whose whole day is older than the window plus the margin, and answers the days it dropped (or that another run held the lock). The margin is the deployer's, 1 to 3650 days, default 90 (the owner's decision of 2026-11-28). The rule is SQL |
| `PriceBook` | The price book's port, an extension of `LedgerStore`: `load_price_list` (YAML), `price_versions`, `rate`, `intake_raw` |
| `LedgerError`, `Refusal`, `Rule`, `ListPosition` | The port's error: a refusal by a named rule (which, for a batch, names the offending events, and for a price list its line, column and key) or a fault of the store with its class |
| `PaymentProvider` | The payment provider port ([ADR 0003](../../docs/adr/0003-credits-from-a-payment-provider.md)): create a payment, read a payment, read a refund, verify a webhook. `openbilling-vpay` implements it; `openbilling-testkit` holds its conformance suite |
| `PaymentRequest`, `PaymentTarget`, `ReturnUrls`, `IdempotencyKey`, `CreatedPayment`, `Checkout`, `PaymentView`, `RefundView`, `PaymentStatus`, `RefundStatus`, `WebhookHeaders`, `WebhookHint`, `PaymentError`, `PaymentRefusal` | What goes in and out of it. A webhook names an object to read back, never a status; a checkout URL is hidden from `Debug` |
| `settle_payment`, `settle_refund`, `settle_hint`, `Settled`, `SettleError` | Composition of the ports, and nothing else: a payment read back is handed to the ledger's `TopUpLedger`, which compares it with the recorded top-up and, when it succeeded and agrees, books the *recorded* terms as a `payment` credit keyed by the provider's id; a refund read back as succeeded a reversal, after its payment is settled. `SettleError::ReadBackMismatch(MismatchField)` (class `conflict`) is a payment that differs from its top-up: it names the field, never a value. The once-only keys and the cap are the ledger's |
| `TopUp`, `TopUpLedger`, `TopUpSettled`, `MismatchField`, `UnrecordedTopUps` | Top-ups ([ADR 0003](../../docs/adr/0003-credits-from-a-payment-provider.md), amended 2026-11-28): the extension of the storage port that records what was asked for (`record_top_up`, once-only by the payment id; other terms are `Rule::TopUpConflict`), settles a payment read back against it (`settle_top_up`) claims the pending ones that are due (`claim_pending_top_ups`), claims those that reached `stop_after` for a last read (`claim_expiring_top_ups`), moves one still pending after it to `expired` (`expire_top_up`, `false` when it was settled meanwhile) and, as a backstop at `stop_after + last_read_for`, the unread ones (`expire_stale_top_ups`). `MismatchField` is `owner`, `label`, `currency`, `amount` or `unrecorded`. `UnrecordedTopUps` (`Refuse`, the default, or `Credit`) is what the deployment does with a succeeded payment no top-up was recorded for. A new required trait: an implementer of the storage must add it where the service composes `settle_payment` |
| `reconcile_pending`, `ReconcileSettings`, `ReconcileReport` | One pass of reading back the pending top-ups: the backstop expiry, then the last reads of those that reached `stop_after` (`report.last`, a `ReadOutcomes` with the same counts and `settled_meanwhile`; `last.pending` are the ones moved to `expired`; `expired_unread` those whose read failed for good), then the due ones: claim, then `settle_payment` each; one failure never stops the others; at most `2 * batch` reads. The report counts `credited`, `replayed`, `pending`, `not_ours`, `canceled`, `mismatched` and `failed`, each once, and `expired` (top-ups moved to `expired` first by `TopUpLedger::expire_stale_top_ups`, not part of `total()`), and `expire_error`, the class of an error that stopped the expiry (the claim still runs). `ReconcileSettings::default()` is 15 minutes before the first read, 15 minutes between reads, a last read at 7 days with `last_read_for` 1 day of grace, 100 a pass |
| `Error`, `ErrorClass`, `Classify` | Errors with a class (`invalid`, `conflict`, `unavailable`, `internal`). Callers decide from the class, never from the variant. An error never holds the value it refused |

## Why `Amount` is an `i64` of micro-units

- Micro-units (10^-6 of the currency's major unit), not the minor unit, because one usage event is often worth less
  than one minor unit (ADR 0001, amended 2026-11-27, B1).
- It is PostgreSQL's `bigint`, which the driver maps directly. An `i128` would need `numeric` in the database.
- `2^63 - 1` micro-units is about 9.2 × 10^12 major units: enough for any one balance.
- Overflow is never silent: `checked_add` and `checked_sub` return `None`, and the SQL refuses a sum or a debt that
  would pass the largest amount.

The range is symmetric: `i64::MIN` is refused, so every amount has a negation and every entry has its balancing
entry. An `Amount` carries no currency: each account has one, the operator names the set, and nothing converts
between them.

## Tests

`cargo test -p openbilling-core`: unit tests next to each module (ids and their alphabets, the price book's names,
amounts, quantities, dimensions, version ids, the calendar, the default lot order and window, refusal messages,
positions and classes, the answer types, the comparisons' names and the default policy, idempotency keys, payment refusals and their classes, webhook headers, a
hidden checkout URL). No database, no environment variable. `settle_*` is tested where both ports have an
implementation: `openbilling-vpay`'s `tests/settle.rs` and `openbilling-http`'s `tests/flow.rs`.
