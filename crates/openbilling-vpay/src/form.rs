//! vpay's request body: bracket-nested `application/x-www-form-urlencoded`, as vpay's own SDKs write it.
//!
//! *Verified 2026-11-27*, vpay `8b56831`, `sdks/rust/src/form.rs` (the encoder) and
//! `backends/crates/vpay-api/src/form.rs` (the decoder): brackets are structural and written literally around each
//! key segment; each segment and each value is escaped as JavaScript's `encodeURIComponent` escapes, `A`–`Z`,
//! `a`–`z`, `0`–`9` and `-_.!~*'()` passing through and every other byte becoming `%XX` in upper-case hex. The
//! WHATWG form serializer (`serde_urlencoded`) would escape the structural brackets, which vpay's decoder then reads
//! as one flat key, and would write a space as `+`, which vpay reads as a literal plus.

use std::fmt::Write as _;

/// The bytes `encodeURIComponent` leaves as they are.
fn is_safe_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric()
        || matches!(
            b,
            b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')'
        )
}

/// `encodeURIComponent`, byte for byte. Also how a path segment (an id) is escaped.
pub(crate) fn percent_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        if is_safe_byte(byte) {
            out.push(char::from(byte));
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

/// A form body under construction.
#[derive(Debug, Default)]
pub(crate) struct Form {
    pairs: Vec<(String, String)>,
}

impl Form {
    /// `name=value`.
    pub(crate) fn field(mut self, name: &str, value: &str) -> Self {
        self.pairs
            .push((percent_encode(name), percent_encode(value)));
        self
    }

    /// `name[key]=value`: one entry of an object such as `metadata`.
    pub(crate) fn entry(mut self, name: &str, key: &str, value: &str) -> Self {
        self.pairs.push((
            format!("{}[{}]", percent_encode(name), percent_encode(key)),
            percent_encode(value),
        ));
        self
    }

    /// `name[0]=…&name[1]=…`: an array, indexed.
    pub(crate) fn list<'a>(
        mut self,
        name: &str,
        values: impl IntoIterator<Item = &'a str>,
    ) -> Self {
        for (index, value) in values.into_iter().enumerate() {
            self.pairs.push((
                format!("{}[{index}]", percent_encode(name)),
                percent_encode(value),
            ));
        }
        self
    }

    /// The body.
    pub(crate) fn encode(&self) -> String {
        self.pairs
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("&")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brackets_are_structural_and_segments_escaped() {
        let body = Form::default()
            .field("amount", "5000")
            .list("payment_method_types", ["a_b", "c"])
            .entry("metadata", "k[1]", "v w+x")
            .encode();
        assert_eq!(
            body,
            "amount=5000&payment_method_types[0]=a_b&payment_method_types[1]=c&metadata[k%5B1%5D]=v%20w%2Bx"
        );
    }

    #[test]
    fn the_escape_is_encode_uri_component() {
        // The same set the vpay SDK's own test pins: these pass through, `/`, `:`, `?`, `#`, `&`, `=` do not.
        assert_eq!(percent_encode("-_.!~*'()aZ09"), "-_.!~*'()aZ09");
        assert_eq!(
            percent_encode("https://x/y?a=b&c#d"),
            "https%3A%2F%2Fx%2Fy%3Fa%3Db%26c%23d"
        );
        assert_eq!(percent_encode("\u{e9}"), "%C3%A9");
    }
}
