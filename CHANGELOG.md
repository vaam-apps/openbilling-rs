# Changelog

## [0.3.0](https://github.com/vaam-apps/openbilling-rs/compare/v0.2.0...v0.3.0) (2026-11-27)


### ⚠ BREAKING CHANGES

* **ledger:** LedgerStore::allowed takes an AllowedPolicy and Allowed has new fields. Under the default policy, a debt limit of 0 now refuses getAllowed with debt_limit for an owner who owes nothing (it answered balance or allowed before). getAllowed's answer gains eight fields.

### Features

* **ledger:** the allowed check compares as the deployer's YAML configuration says ([#19](https://github.com/vaam-apps/openbilling-rs/issues/19)) ([be9f04b](https://github.com/vaam-apps/openbilling-rs/commit/be9f04bb2c291faaf436fe440982dc519e1a35cc))


### Continuous Integration

* **deps:** Bump actions/create-github-app-token from 3.2.0 to 3.4.0 ([#14](https://github.com/vaam-apps/openbilling-rs/issues/14)) ([f887c24](https://github.com/vaam-apps/openbilling-rs/commit/f887c244d87cca1d6bb372e5ece18ee5161a6fd8))
* **deps:** Bump actions/upload-artifact from 4.6.2 to 7.0.2 ([#12](https://github.com/vaam-apps/openbilling-rs/issues/12)) ([05d38dc](https://github.com/vaam-apps/openbilling-rs/commit/05d38dc6b729f479480617ad0066202ae9a2fa1f))
* **deps:** Bump azure/setup-helm from 4.3.1 to 5.1.0 ([#13](https://github.com/vaam-apps/openbilling-rs/issues/13)) ([1d39b79](https://github.com/vaam-apps/openbilling-rs/commit/1d39b79c254c24b76274555c89f1066818ae6c8f))
* **deps:** Bump docker/build-push-action from 6.19.2 to 7.4.0 ([#16](https://github.com/vaam-apps/openbilling-rs/issues/16)) ([7643579](https://github.com/vaam-apps/openbilling-rs/commit/764357969a4f4ab8f5a48807abb39c4839e80426))
* **deps:** Bump docker/login-action from 3.7.0 to 4.8.0 ([#15](https://github.com/vaam-apps/openbilling-rs/issues/15)) ([0e863c2](https://github.com/vaam-apps/openbilling-rs/commit/0e863c23be9ab0dac706922def5a160f253c7bf9))

## [0.2.0](https://github.com/vaam-apps/openbilling-rs/compare/v0.1.1...v0.2.0) (2026-11-27)


### ⚠ BREAKING CHANGES

* an end-to-end suite on PostgreSQL 19, a JWT verifier, usage always booked as debt, and the API to drive them ([#17](https://github.com/vaam-apps/openbilling-rs/issues/17))

### Features

* an end-to-end suite on PostgreSQL 19, a JWT verifier, usage always booked as debt, and the API to drive them ([#17](https://github.com/vaam-apps/openbilling-rs/issues/17)) ([f0f2427](https://github.com/vaam-apps/openbilling-rs/commit/f0f24274bafe4fd7a7754bc49cdacf6dfb0abd81))


### Chores

* **skills:** vendor the vpay skills from vaam-apps/vpay-skills ([#10](https://github.com/vaam-apps/openbilling-rs/issues/10)) ([c66c047](https://github.com/vaam-apps/openbilling-rs/commit/c66c047f73ef25dc977bc1c5d32ec28e862b2f7e))

## [0.1.1](https://github.com/vaam-apps/openbilling-rs/compare/v0.1.0...v0.1.1) (2026-11-27)


### Build System

* **release:** release-please, a signed versioned image and a Helm chart on bjw-s common v5 ([#8](https://github.com/vaam-apps/openbilling-rs/issues/8)) ([12211a5](https://github.com/vaam-apps/openbilling-rs/commit/12211a5f87914485180e139fe75808772445fa5d))
