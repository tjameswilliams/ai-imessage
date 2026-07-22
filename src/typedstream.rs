//! Minimal extraction of message text from Apple `typedstream` blobs.
//!
//! Modern macOS versions of Messages frequently leave the `message.text`
//! column NULL and store the body only in `message.attributedBody`, an
//! `NSAttributedString` serialized in the NeXT-era typedstream format.
//!
//! This is a clean-room implementation based on public descriptions of the
//! format and inspection of real blobs. It does not attempt to decode the
//! full object graph: the message body is always the first `NSString` (or
//! `NSMutableString`) value in the stream, so we locate the string type
//! marker (`+`, 0x2B) that follows the class declaration, decode the length,
//! and read the UTF-8 bytes. Everything after the base string (attribute
//! runs, fonts, mentions) is deliberately ignored for now.
//!
//! Length encoding: a single byte below 0x80 is the length itself; 0x81 is
//! followed by a little-endian u16; 0x82 is followed by a little-endian u32.

/// Every typedstream blob starts with a version byte (4) and the
/// length-prefixed ASCII string "streamtyped".
pub const STREAMTYPED_HEADER: &[u8] = b"\x04\x0bstreamtyped";

const STRING_VALUE_MARKER: u8 = 0x2b; // '+', precedes a length-prefixed string
/// How far past the class name we scan for the string value marker. Covers
/// the observed layouts for both `NSString` and `NSMutableString` streams.
const MARKER_SCAN_WINDOW: usize = 24;

/// Extract the plain-text body from an `attributedBody` typedstream blob.
///
/// Returns `None` if the blob is not a typedstream, contains no string, or
/// is malformed/truncated. Never panics on arbitrary input.
pub fn extract_text(blob: &[u8]) -> Option<String> {
    if !blob.starts_with(STREAMTYPED_HEADER) {
        return None;
    }
    // The base string's class is declared before its value. Prefer the first
    // NSString declaration; fall back to NSMutableString (whose inheritance
    // chain normally embeds NSString anyway).
    let class_end = find_subsequence(blob, b"NSString")
        .map(|i| i + b"NSString".len())
        .or_else(|| {
            find_subsequence(blob, b"NSMutableString").map(|i| i + b"NSMutableString".len())
        })?;

    let window_end = class_end.saturating_add(MARKER_SCAN_WINDOW).min(blob.len());
    let marker = (class_end..window_end).find(|&i| blob[i] == STRING_VALUE_MARKER)?;

    let (len, consumed) = decode_length(blob.get(marker + 1..)?)?;
    let start = marker + 1 + consumed;
    let end = start.checked_add(len)?;
    if end > blob.len() {
        return None;
    }
    std::str::from_utf8(&blob[start..end])
        .ok()
        .map(str::to_owned)
}

fn decode_length(bytes: &[u8]) -> Option<(usize, usize)> {
    match *bytes.first()? {
        0x81 => {
            let raw: [u8; 2] = bytes.get(1..3)?.try_into().ok()?;
            Some((u16::from_le_bytes(raw) as usize, 3))
        }
        0x82 => {
            let raw: [u8; 4] = bytes.get(1..5)?.try_into().ok()?;
            Some((u32::from_le_bytes(raw) as usize, 5))
        }
        b if b < 0x80 => Some((b as usize, 1)),
        _ => None,
    }
}

/// Encode a message body as a typedstream blob with the same layout real
/// `attributedBody` blobs use.
///
/// This exists for building synthetic test fixtures (we must never require a
/// developer's live Messages database for tests) and doubles as the
/// round-trip counterpart to [`extract_text`].
pub fn encode_text(text: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(64 + text.len());
    out.extend_from_slice(STREAMTYPED_HEADER);
    out.extend_from_slice(&[0x81, 0xe8, 0x03]); // system version 1000
    out.extend_from_slice(&[0x84, 0x01, b'@']); // typed value: object
    out.extend_from_slice(&[0x84, 0x84, 0x84, 0x12]);
    out.extend_from_slice(b"NSAttributedString");
    out.push(0x00);
    out.extend_from_slice(&[0x84, 0x84, 0x08]);
    out.extend_from_slice(b"NSObject");
    out.push(0x00);
    out.extend_from_slice(&[0x85, 0x92, 0x84, 0x84, 0x84, 0x08]);
    out.extend_from_slice(b"NSString");
    out.extend_from_slice(&[0x01, 0x94, 0x84, 0x01, STRING_VALUE_MARKER]);
    encode_length(text.len(), &mut out);
    out.extend_from_slice(text.as_bytes());
    // Trailing attribute-run bookkeeping (truncated; ignored by the parser).
    out.extend_from_slice(&[0x86, 0x84, 0x02, b'i', b'I']);
    out
}

fn encode_length(len: usize, out: &mut Vec<u8>) {
    if len < 0x80 {
        out.push(len as u8);
    } else if len <= u16::MAX as usize {
        out.push(0x81);
        out.extend_from_slice(&(len as u16).to_le_bytes());
    } else {
        out.push(0x82);
        out.extend_from_slice(&(len as u32).to_le_bytes());
    }
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_simple_text() {
        let blob = encode_text("Hello from typedstream");
        assert_eq!(
            extract_text(&blob).as_deref(),
            Some("Hello from typedstream")
        );
    }

    #[test]
    fn roundtrip_empty_string() {
        let blob = encode_text("");
        assert_eq!(extract_text(&blob).as_deref(), Some(""));
    }

    #[test]
    fn roundtrip_unicode_and_emoji() {
        let text = "Grüße aus Köln 🎉🇩🇪 — 中文也可以";
        let blob = encode_text(text);
        assert_eq!(extract_text(&blob).as_deref(), Some(text));
    }

    #[test]
    fn roundtrip_long_text_uses_u16_length() {
        let text = "a".repeat(5000);
        let blob = encode_text(&text);
        // Verify the encoder actually took the 0x81/u16 path.
        assert!(blob.windows(3).any(|w| w == [0x81, 0x88, 0x13])); // 5000 LE
        assert_eq!(extract_text(&blob).as_deref(), Some(text.as_str()));
    }

    #[test]
    fn roundtrip_very_long_text_uses_u32_length() {
        let text = "b".repeat(70_000);
        let blob = encode_text(&text);
        assert_eq!(extract_text(&blob).as_deref(), Some(text.as_str()));
    }

    #[test]
    fn roundtrip_text_at_length_boundaries() {
        for len in [0x7e, 0x7f, 0x80, 0x81, 0xff, 0x100] {
            let text = "x".repeat(len);
            assert_eq!(
                extract_text(&encode_text(&text)).as_deref(),
                Some(text.as_str()),
                "failed at length {len}"
            );
        }
    }

    #[test]
    fn text_containing_class_names_is_not_confused() {
        // The literal string "NSString" inside the body must not derail the
        // parser, since the class declaration comes first in the stream.
        let text = "did you mean NSString or NSMutableString?";
        assert_eq!(extract_text(&encode_text(text)).as_deref(), Some(text));
    }

    #[test]
    fn empty_blob_is_rejected() {
        assert_eq!(extract_text(b""), None);
    }

    #[test]
    fn non_typedstream_blob_is_rejected() {
        assert_eq!(extract_text(b"this is definitely not a typedstream"), None);
        assert_eq!(extract_text(&[0x00, 0x01, 0x02, 0x03]), None);
    }

    #[test]
    fn header_only_blob_is_rejected() {
        assert_eq!(extract_text(STREAMTYPED_HEADER), None);
    }

    #[test]
    fn truncated_blob_is_rejected() {
        let blob = encode_text("Hello, this message will be cut off");
        // Cut into the middle of the text bytes.
        let truncated = &blob[..blob.len() - 20];
        assert_eq!(extract_text(truncated), None);
    }

    #[test]
    fn truncated_length_prefix_is_rejected() {
        let mut blob = encode_text(&"y".repeat(300));
        // Find the 0x81 length marker and cut immediately after it.
        let pos = blob.iter().position(|&b| b == 0x81).unwrap();
        blob.truncate(pos + 1);
        assert_eq!(extract_text(&blob), None);
    }

    #[test]
    fn invalid_utf8_is_rejected() {
        let mut blob = encode_text("abcd");
        let len = blob.len();
        // Corrupt the text bytes (they sit just before the 5-byte trailer).
        blob[len - 7] = 0xff;
        blob[len - 6] = 0xfe;
        assert_eq!(extract_text(&blob), None);
    }

    #[test]
    fn declared_length_beyond_blob_is_rejected() {
        let mut blob = encode_text("hi");
        // Bump the single-byte length far past the real end.
        let pos = blob.iter().position(|&b| b == STRING_VALUE_MARKER).unwrap();
        blob[pos + 1] = 0x7f;
        assert_eq!(extract_text(&blob), None);
    }

    #[test]
    fn garbage_after_header_is_rejected_without_panic() {
        let mut blob = STREAMTYPED_HEADER.to_vec();
        blob.extend_from_slice(&[0xde, 0xad, 0xbe, 0xef, 0x84, 0x92, 0x2b]);
        assert_eq!(extract_text(&blob), None);
    }

    #[test]
    fn reserved_length_markers_are_rejected() {
        assert_eq!(decode_length(&[0x85, 0x01, 0x02]), None);
        assert_eq!(decode_length(&[0xff]), None);
        assert_eq!(decode_length(&[]), None);
    }

    #[test]
    fn decode_length_handles_all_encodings() {
        assert_eq!(decode_length(&[0x05]), Some((5, 1)));
        assert_eq!(decode_length(&[0x7f]), Some((127, 1)));
        assert_eq!(decode_length(&[0x81, 0x00, 0x01]), Some((256, 3)));
        assert_eq!(
            decode_length(&[0x82, 0x00, 0x00, 0x01, 0x00]),
            Some((65536, 5))
        );
    }
}
