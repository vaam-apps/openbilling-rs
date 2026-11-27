//! The payment provider conformance suite of `openbilling-testkit`, against the vpay stand-in.

use openbilling_vpay::stand_in as support;

openbilling_testkit::payment_conformance!(support::fixture);
