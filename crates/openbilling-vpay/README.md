# openbilling-vpay

The [vpay](https://github.com/vaam-apps/vpay) adapter of the payment provider port, `PaymentProvider`
([ADR 0003](../../docs/adr/0003-credits-from-a-payment-provider.md)). Every fact it relies on about vpay is listed,
with the vpay revision and file it was read in, in ADR 0003's *Agent notes* and in the module comments.

| Port call | vpay |
|---|---|
| `create_payment` | `POST /v1/payment_intents` (amount in minor units, lower-case currency, `payment_method_types[]`, `metadata[openbilling_owner]`, `metadata[openbilling_label]`), then `POST /v1/checkout/sessions` (`ui_mode=hosted`, `success_url`, `cancel_url`). Each with an `Idempotency-Key` of `openbilling:<key>:payment_intent` and `…:checkout_session`. Answers the `pi_…` id and the session's `cs_…` id and `url` |
| `read_payment` | `GET /v1/payment_intents/{id}`: status (`requires_payment_method`, `requires_action`, `processing` are pending), amount, currency, and the owner and label from `metadata`; with either missing, the payment is not openbilling's |
| `read_refund` | `GET /v1/refunds/{id}` |
| `verify_webhook` | `Vpay-Signature`, verified as vpay's own SDK does (`src/webhook.rs`), then `type` and `data.object.id` only: `payment_intent.*` names a payment, `charge.refunded` and `charge.refund.updated` a refund, anything else nothing |

- **Amounts** are scaled between micro-units and vpay's minor units with each configured currency's exponent
  (0 to 6). A top-up with no exact minor-unit value is refused, `inexact_amount`, before vpay is asked.
- **Requests** are bracket-nested form bodies written as vpay's own SDK writes them (`src/form.rs`): literal
  brackets, `encodeURIComponent` escaping. An id is escaped the same way as a path segment.
- **Authentication** is a bearer token for vpay's `/v1`, read from a file on every call (`Credential::TokenFile`),
  so whatever mints and rotates it needs no restart. The vpay deployment must permit `machine_tokens`. Minting the
  token (`client_credentials` with an RS256 `private_key_jwt`) and vpay's signed transport are not built.
- **Errors** carry a class: vpay's `400` is `invalid_request` (`idempotency_error` is `conflict`), `404`
  `not_found`, `409` `conflict`, `429` and `502`–`504` and a network failure `unavailable`, `401`/`403` and other
  statuses `internal`. No message holds the token, a secret or a response body; `Debug` hides them too.
- **HTTP** is reqwest 0.13, as CrateStack's client builds it: rustls with ring, no redirect followed, a timeout per
  call, `https` only (or `http` to a loopback address), no user name or password in the base URL.

## API

`VpayProvider::new(VpayConfig)` checks the configuration (`ConfigError`) and implements
`openbilling_core::PaymentProvider`. `VpayConfig`: `base_url`, `credential`, `webhook_secrets` (any one verifies;
two during a rotation), `webhook_tolerance_secs` (`webhook::DEFAULT_TOLERANCE_SECS`, 300), `currencies`
(`CurrencyUnit { currency, exponent }`), `payment_method_types` (vpay rail codes or method types; at least one),
`timeout`. `webhook::signature_header(body, at, secrets)` builds the header vpay sends, for tests and stand-ins.

The feature **`stand-in`** adds `stand_in`: a stand-in for the parts of vpay's `/v1` the adapter calls, served in
process on a loopback port, and a `ProviderFixture` for `openbilling-testkit`'s payment suite. It is test code; no
shipping binary enables it.

## Tests

| Test | Needs |
|---|---|
| Unit tests in `src/` (the form encoding, the signature against a vector computed with `openssl`, malformed headers, the literal `t`, the tolerance both ways, two headers, event names, unit scaling, status mapping, error classes, the base URL, configurations refused, secrets never printed) | nothing |
| `tests/conformance.rs`: the payment conformance suite of `openbilling-testkit`, 7 cases, against the stand-in | nothing |
| `tests/requests.rs`: the exact bodies, keys and bearer the adapter sends, a key reused for another top-up, a currency not configured, the token file read on every call, vpay's faults and an unreachable vpay classed, an id escaped as a path segment | nothing |
| `tests/settle.rs`: a paid top-up credits once however often it is settled, foreign and canceled payments book nothing, only a succeeded refund reverses and only once, a label the owner lacks is refused by the ledger | `OPENBILLING_TEST_DATABASE_URL` (skips without it; fails under `OPENBILLING_TEST_REQUIRE_DB=1`) |
