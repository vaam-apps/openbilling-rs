# openbilling-sdk

A Rust client for the [openbilling-rs](https://github.com/vaam-apps/openbilling-rs) usage billing API
([ADR 0008](https://github.com/vaam-apps/openbilling-rs/blob/main/docs/adr/0008-a-rust-sdk-on-crates-io.md)). Typed
calls for every operation of the API, over a client that CrateStack generates from the API's schema, and a thin
hand-written facade.

```toml
[dependencies]
openbilling-sdk = "0.7"
```

## Use

```rust,no_run
use openbilling_sdk::generated::cratestack_schema::{procedures, types};
use openbilling_sdk::{Classify, Client, ErrorClass, IdempotencyKey};

# async fn demo() -> Result<(), openbilling_sdk::Error> {
// The service's root: `/v1` is added.
let client = Client::new("https://billing.example.com")?;
// A JWT your identity provider signed for the service's audience. Call it again to refresh the token.
client.set_token("<token>");

// A read.
let balances = client
    .get_balance(&procedures::get_balance::Args {
        args: types::OwnerCurrency { owner: "tenant-1".into(), currency: "EUR".into() },
    })
    .await?;
println!("{} accounts", balances.len());

// A write takes an `IdempotencyKey`, which the API requires. After a failure that says `Unavailable`, send the same
// call under the same key: the API replays the first answer, so it cannot happen twice.
let key = IdempotencyKey::new("open-acct-tenant-1-main")?;
# let open: procedures::open_account::Args = todo!();
match client.open_account(&open, &key).await {
    Ok(opened) => println!("opened {}", opened.accountId),
    Err(e) if e.class() == ErrorClass::Unavailable => { /* retry under the same key */ }
    Err(e) => return Err(e),
}
# Ok(()) }
```

## What is in it

| | |
|---|---|
| `Client` | `new(base_url)`, `set_token`, `clear_token`; one method per procedure of the API (`get_balance`, `get_allowed`, `list_credits`, `get_statement`, `get_usage_series`; and, each with an `IdempotencyKey`, `configure`, `open_account`, `grant_credit`, `record_usage`, `record_raw_usage`, `set_debt_limit`, `set_member_cap`, `load_price_list`, `start_top_up`, `reconcile_payment`, `resolve_top_up`, `reconcile_refund`); `raw()` is the generated client, for the models (`accounts`, `lots`, `usage_outcomes`: list and get) |
| `IdempotencyKey` | 1 to 255 visible ASCII characters. A write cannot be called without one |
| `Error`, `ErrorClass`, `Classify` | `Invalid`, `Conflict`, `Unavailable`, `Internal`: decide from the class, never from the variant. An error never holds the token |
| `generated` | What CrateStack generates: the argument and answer types (`procedures::<name>::Args` and `Output`, `types::*`), and the raw client |

Amounts are CrateStack `BigInt`s: micro-units (10^-6) of the account's currency, decimal strings on the wire, `i64`
through `From<BigInt> for i64`. Never floats. The client speaks JSON.

Which role a call needs (`operator`, `producer`, or the owner's own token) is in the API's contract:
[`docs/api/ops.json`](https://github.com/vaam-apps/openbilling-rs/blob/main/docs/api/ops.json).

## Where the code comes from

`schema/openbilling.cstack` is a copy of the server's schema, and the typed client is generated from it when the crate
builds (`include_client_schema!` of CrateStack 0.20.6; its CLI has no Rust generator). A test and CI fail when the copy
differs from `crates/openbilling-http/schema/openbilling.cstack`. The crate has no server dependency: no axum, no
sqlx. Its version is the workspace's, and a release publishes it ([ADR 0008](https://github.com/vaam-apps/openbilling-rs/blob/main/docs/adr/0008-a-rust-sdk-on-crates-io.md)).

## Tests

`tests/facade.rs` needs nothing: keys, base URLs, classes, that the token is in no output, that the schema copy is the
server's and that every operation has a call. The end-to-end scenario `e2e/sdk.rs` runs the SDK against the real
`openbilling serve` and needs `OPENBILLING_TEST_DATABASE_URL` (it skips without it, and fails instead under
`OPENBILLING_TEST_REQUIRE_DB=1`).
