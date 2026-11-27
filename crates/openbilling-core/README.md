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
| `EventOutcome`, `RefusalReason` | What a drain decided: `applied`, or `refused` with `limit`, `no_price`, `unknown_account` or `id_reused` |
| `LedgerStore` | The storage port: a trait whose methods return `Send` futures. `openbilling-pg` implements it; `openbilling-testkit` holds its conformance suite |
| `PriceBook` | The price book's port, an extension of `LedgerStore`: `load_price_list` (YAML), `price_versions`, `rate`, `intake_raw` |
| `LedgerError`, `Refusal`, `Rule`, `ListPosition` | The port's error: a refusal by a named rule (which, for a batch, names the offending events, and for a price list its line, column and key) or a fault of the store with its class |
| `PaymentProvider` | The payment provider port ([ADR 0003](../../docs/adr/0003-credits-from-a-payment-provider.md)): create a payment, read a payment, read a refund, verify a webhook. `openbilling-vpay` implements it; `openbilling-testkit` holds its conformance suite |
| `PaymentRequest`, `PaymentTarget`, `ReturnUrls`, `IdempotencyKey`, `CreatedPayment`, `Checkout`, `PaymentView`, `RefundView`, `PaymentStatus`, `RefundStatus`, `WebhookHeaders`, `WebhookHint`, `PaymentError`, `PaymentRefusal` | What goes in and out of it. A webhook names an object to read back, never a status; a checkout URL is hidden from `Debug` |
| `settle_payment`, `settle_refund`, `settle_hint`, `Settled`, `SettleError` | Composition of the two ports, and nothing else: a payment read back as succeeded becomes a `payment` credit keyed by its provider id; a refund read back as succeeded a reversal, after its payment is settled. The once-only keys and the cap are the ledger's |
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
positions and classes, the answer types, idempotency keys, payment refusals and their classes, webhook headers, a
hidden checkout URL). No database, no environment variable. `settle_*` is tested where both ports have an
implementation: `openbilling-vpay`'s `tests/settle.rs` and `openbilling-http`'s `tests/flow.rs`.
