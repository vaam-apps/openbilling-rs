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
- **Authentication** is a bearer token for vpay's `/v1`; the vpay deployment must permit `machine_tokens`. Either
  `Credential::TokenFile`, read on every call, so whatever mints and rotates it needs no restart; or
  `Credential::ClientCredentials`, where **the adapter mints it** (`src/token.rs`): an OAuth 2.0 `client_credentials`
  grant at `{base}/v1/oauth/token` with `audience=vpay:v1`, the client authenticating with an RS256 `private_key_jwt`
  (RFC 7523) signed by ring. The token is minted on first use, replaced before it expires (by `refresh_skew`, at most
  half its life), minted by one caller while the others wait, and a transient failure (network, `408`, `429`, `5xx`) is
  retried with a doubling, bounded backoff; each retry is a new assertion. A refusal (`400`, `401`) is not retried:
  an `internal` error that names vpay's RFC 6749 error code and nothing else. A `401` from `/v1` drops the cached token.
  The private key is a PEM file (PKCS#8 or PKCS#1), checked at start and read again at each mint; the token type has
  no `Display` and a hidden `Debug`. vpay's signed transport (COSE_Sign1) is not built.
- **Errors** carry a class: vpay's `400` is `invalid_request` (`idempotency_error` is `conflict`), `404`
  `not_found`, `409` `conflict`, `429` and `502`–`504` and a network failure `unavailable`, `401`/`403` and other
  statuses `internal`. No message holds the token, a secret or a response body; `Debug` hides them too.
- **HTTP** is reqwest 0.13, as CrateStack's client builds it: rustls with ring, no redirect followed, a timeout per
  call, `https` only (or `http` to a loopback address), no user name or password in the base URL.

## API

`VpayProvider::new(VpayConfig)` checks the configuration (`ConfigError`) and implements
`openbilling_core::PaymentProvider`. `ClientCredentials::new(client_id, KeySource::File(path))` carries the minting settings, each with a documented
default: `key_id` (none), `token_url` (`{base_url}/v1/oauth/token`), `assertion_audience` (the token URL; set it when
vpay is reached by a URL other than the one it publishes as its own), `audience` (`vpay:v1`), `scope` (none: vpay
grants the client's registered scopes), `refresh_skew` (30 s, at most 600 s), `retries` (2, at most 10),
`retry_backoff` (200 ms, doubling, at most 5 s). A bad value is a `ConfigError` at `VpayProvider::new`.
`VpayConfig`: `base_url`, `credential`, `webhook_secrets` (any one verifies;
two during a rotation), `webhook_tolerance_secs` (`webhook::DEFAULT_TOLERANCE_SECS`, 300), `currencies`
(`CurrencyUnit { currency, exponent }`), `payment_method_types` (vpay rail codes or method types; at least one),
`timeout`. `webhook::signature_header(body, at, secrets)` builds the header vpay sends, for tests and stand-ins.

The feature **`stand-in`** adds `stand_in`: a stand-in for the parts of vpay's `/v1` the adapter calls, served in
process on a loopback port, and a `ProviderFixture` for `openbilling-testkit`'s payment suite. Its `/v1/oauth/token`
verifies a client assertion as vpay's OP does (RS256 under the registered key, `iss`, `sub`, `aud`, `exp` at most 300 s
ahead and at most 60 s past, a `jti` spent once, a form content type, `audience`, `scope`; `stand_in::assertion` signs a
forged one for a test), issues tokens that expire, can be told to fail (`token_fail`) or to be slow, and
its `/v1` calls need a token it issued (or the static `TOKEN`). The registered key is a checked-in test key
(`tests/vectors/test-client-key.pem`). It is test code; no shipping binary enables it.

## Tests

| Test | Needs |
|---|---|
| Unit tests in `src/` (the form encoding, the signature against a vector computed with `openssl`, malformed headers, the literal `t`, the tolerance both ways, two headers, event names, unit scaling, status mapping, error classes, the base URL, configurations refused, secrets never printed) | nothing |
| `tests/conformance.rs`: the payment conformance suite of `openbilling-testkit`, 7 cases, against the stand-in, through a minted token | nothing |
| `tests/token.rs`: the minting: first use, one request shape, reuse, refresh before expiry, one mint under concurrent callers, a refusal and its code, retries and their bound, a replayed, tampered or over-long assertion refused as vpay's OP refuses it, a lifetime that would overflow the clock, `kid`, scope, a refused token dropped, key rotation without a restart, settings refused, no secret in any error or `Debug` | nothing |
| `tests/sandbox.rs`: 3 of the 7 conformance cases (the ones that need no payer) against a real vpay | `VPAY_SANDBOX_*` (skips without them; fails under `VPAY_SANDBOX_REQUIRE=1`); run by `.github/workflows/vpay-sandbox.yml`, never by CI's gate |
| `tests/requests.rs`: the exact bodies, keys and bearer the adapter sends, a key reused for another top-up, a currency not configured, the token file read on every call, vpay's faults and an unreachable vpay classed, an id escaped as a path segment | nothing |
| `tests/settle.rs`: a paid top-up credits once however often it is settled, foreign and canceled payments book nothing, only a succeeded refund reverses and only once, a label the owner lacks is refused by the ledger | `OPENBILLING_TEST_DATABASE_URL` (skips without it; fails under `OPENBILLING_TEST_REQUIRE_DB=1`) |
