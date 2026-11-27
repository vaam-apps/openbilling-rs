# openbilling-jwt

A JWT bearer verifier for the API of openbilling-rs: `JwtVerifier`, an implementation of `openbilling-http`'s
`TokenVerifier` port against one issuer's keys
([ADR 0003](../../docs/adr/0003-credits-from-a-payment-provider.md), amended 2026-11-27). The service uses it when
an issuer and an audience are configured; without them it keeps `RefuseAll`.

## What it accepts

Fail closed: a token is accepted only when every check below holds, and refused with the first that fails.

| Check | Refused as |
|---|---|
| A compact JWS of at most 16 KiB, with no `crit` header | `malformed` |
| `alg` is `RS256`, `ES256` or `EdDSA` (Ed25519); never `none`, never `HS*`; and it is the algorithm of the key its `kid` names | `algorithm` |
| A `kid` that is a signature key of the issuer's JWKS | `unknown_key` |
| The signature verifies (ring) | `signature` |
| `exp` has not passed; `nbf`, when there, has come (60 s of leeway) | `expired`, `not_yet_valid` |
| `iss` is exactly the issuer; `aud` holds exactly the audience | `issuer`, `audience` |
| `exp`, `iss`, `aud` and `sub` are there | `missing_claim` |
| `sub` is an owner id (1 to 255 bytes of printable ASCII) | `subject` |
| The issuer's keys can be read, or were read recently enough | `unavailable` (a `503`, never an acceptance) |

Every refusal but `unavailable` is a `401`. A refusal never holds the token or a claim.

**Roles** come from a configured claim (a dotted path, `roles` by default, such as `realm_access.roles`): an array of
strings or one space-separated string. A `RoleMap` maps the issuer's values to the API's two roles, `operator` and
`producer` (by default `operator=operator,producer=producer`); a value it does not name is dropped. An owner needs no
role: the token's `sub` is the owner whose accounts it reads.

**Keys.** The JWKS is the `jwks_uri` of the issuer's discovery document (`<issuer>/.well-known/openid-configuration`,
which must name the issuer exactly), or a configured URL. URLs are `https`, or `http` to a loopback host. The JWKS is
fetched on the first token, kept 5 minutes, fetched again early for a `kid` it lacks (at most once per 30 seconds),
and served for an hour past its refresh while the issuer cannot be reached. Keys without a `kid`, with `use` other
than `sig`, an `alg` that is not their shape's, an RSA modulus under 2048 bits, or a curve other than P-256 and
Ed25519 are left out. Documents are read up to 256 KiB, with no redirect.

## Crypto

jsonwebtoken 11 checks the claims; its signature backends (aws-lc-rs, or the RustCrypto crates with `rsa`) are not
used. `provider` is a jsonwebtoken `CryptoProvider` on ring, the provider rustls already uses here: it verifies the
three algorithms, signs nothing, and refuses everything else. `JwtVerifier::new` installs it process-wide and refuses
to start if another provider was installed first.

## API

`JwtConfig::new(issuer, audience)` with every other setting at its default (`jwks_url`, `roles_claim`, `roles`,
`refresh`, `stale_for`, `min_refetch`, `timeout`); `JwtVerifier::new(config)`, which fetches nothing;
`JwtVerifier::check(token)`, the principal or a `JwtRefusal`; and the `TokenVerifier` impl, which maps `unavailable`
to `TokenRefusal::Unavailable` and the rest to `Invalid`. `RoleMap::parse("value=operator,value=producer")`.
`SetupError` and `JwtRefusal` carry a class (`Classify`).

The feature `test-issuer` adds `test_issuer::TestIssuer`, **test code** that no shipping binary enables: discovery
and a JWKS on a loopback port, and tokens signed with a fresh `ES256` and `EdDSA` key or RFC 7515's published `RS256`
example key, plus one `ES256` key it does not publish. The end-to-end suite (`e2e/`) mints its tokens with it.

## Tests

`cargo test -p openbilling-jwt`, no database: the unit tests (the role map, the claim path, the configuration, the
JWKS filter, the URLs, discovery) and `tests/verify.rs` against the test issuer. Each refusal has a control that is
accepted: the three algorithms accepted; `none`, HS256 keyed with the RSA modulus, an algorithm that is not its key's
and RS512, PS256, ES384 refused; a wrong or missing `aud` and `iss`; an expired token, one within the leeway, a
missing `exp`, an `nbf` to come; an unknown `kid`, refetched at most once per interval and accepted once published; a
tampered payload, a bad or missing `sub`, `crit`, junk; roles through a nested claim and a map; an issuer that is
down (`unavailable`, then stale keys serving, then refused once too stale); and the provider against the examples of
RFC 7515 A.2 (RS256) and RFC 8037 A.4 (EdDSA).
