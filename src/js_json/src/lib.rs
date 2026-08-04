//! JSON serialization for values embedded directly in JavaScript source.

use serde::Serialize;

/// Serializes `value` as JSON safe to paste into JS source: the U+2028 and
/// U+2029 code points, which plain JSON leaves raw and a JS string literal
/// reads as line terminators, are escaped by the formatter below.
/// `None` when the value's `Serialize` impl fails.
pub fn to_js_json<T: Serialize + ?Sized>(value: &T) -> Option<String> {
    let mut out = Vec::new();
    let mut ser = serde_json::Serializer::with_formatter(&mut out, JsSourceFormatter);
    value.serialize(&mut ser).ok()?;
    String::from_utf8(out).ok()
}

struct JsSourceFormatter;

impl serde_json::ser::Formatter for JsSourceFormatter {
    fn write_string_fragment<W>(&mut self, writer: &mut W, fragment: &str) -> std::io::Result<()>
    where
        W: ?Sized + std::io::Write,
    {
        let mut rest = fragment;
        while let Some(at) = rest.find(['\u{2028}', '\u{2029}']) {
            writer.write_all(&rest.as_bytes()[..at])?;
            let (sep, tail) = rest[at..].split_at('\u{2028}'.len_utf8());
            writer.write_all(if sep == "\u{2028}" {
                b"\\u2028"
            } else {
                b"\\u2029"
            })?;
            rest = tail;
        }
        writer.write_all(rest.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_separators_inside_strings_are_escaped() {
        assert_eq!(
            to_js_json("a\u{2028}b\u{2029}c").as_deref(),
            Some("\"a\\u2028b\\u2029c\"")
        );
    }

    #[test]
    fn line_separators_in_object_keys_are_escaped() {
        let mut map = std::collections::BTreeMap::new();
        map.insert("k\u{2028}", 1);
        assert_eq!(to_js_json(&map).as_deref(), Some("{\"k\\u2028\":1}"));
    }

    #[test]
    fn output_matches_serde_json_when_no_line_separators() {
        let value = serde_json::json!({"a": [1, 2, "x\"y\n"], "b": null});
        assert_eq!(
            to_js_json(&value),
            serde_json::to_string(&value).ok(),
            "plain values must serialize identically"
        );
    }
}
