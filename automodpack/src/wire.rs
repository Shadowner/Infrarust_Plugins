use thiserror::Error;

pub const MAX_STRING_CHARS: usize = 32767;
pub const MAX_VAR_INT_BYTES: usize = 5;
const MAX_UTF8_BYTES_PER_CHAR: usize = 3;
const CONTINUE_BIT: u8 = 0x80;
const SEGMENT_BITS: u8 = 0x7F;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum WireError {
    #[error("the buffer ended before the value was complete")]
    Truncated,
    #[error("a VarInt ran past {MAX_VAR_INT_BYTES} bytes")]
    VarIntTooLong,
    #[error("a string declared the negative length {0}")]
    NegativeLength(i32),
    #[error("a string of {bytes} bytes exceeds the limit of {max_chars} chars")]
    StringBytesOverLimit { bytes: usize, max_chars: usize },
    #[error("a string of {chars} UTF-16 units exceeds the limit of {max_chars} chars")]
    StringCharsOverLimit { chars: usize, max_chars: usize },
    #[error("a string is not valid UTF-8")]
    InvalidUtf8,
    #[error("{0} bytes follow the value where none were expected")]
    TrailingBytes(usize),
}

pub fn read_var_int(buf: &[u8]) -> Result<(i32, usize), WireError> {
    let mut value: u32 = 0;
    for (index, byte) in buf.iter().copied().enumerate() {
        if index == MAX_VAR_INT_BYTES {
            return Err(WireError::VarIntTooLong);
        }
        value |= u32::from(byte & SEGMENT_BITS) << (7 * index);
        if byte & CONTINUE_BIT == 0 {
            return Ok((value as i32, index + 1));
        }
    }
    if buf.len() >= MAX_VAR_INT_BYTES {
        Err(WireError::VarIntTooLong)
    } else {
        Err(WireError::Truncated)
    }
}

pub fn write_var_int(value: i32, out: &mut Vec<u8>) {
    let mut remaining = value as u32;
    loop {
        let segment = (remaining & u32::from(SEGMENT_BITS)) as u8;
        remaining >>= 7;
        if remaining == 0 {
            out.push(segment);
            return;
        }
        out.push(segment | CONTINUE_BIT);
    }
}

pub fn read_string(buf: &[u8], max_chars: usize) -> Result<(String, usize), WireError> {
    let (declared, prefix) = read_var_int(buf)?;
    let bytes = usize::try_from(declared).map_err(|_| WireError::NegativeLength(declared))?;
    if bytes > max_chars.saturating_mul(MAX_UTF8_BYTES_PER_CHAR) {
        return Err(WireError::StringBytesOverLimit { bytes, max_chars });
    }
    let end = prefix.checked_add(bytes).ok_or(WireError::Truncated)?;
    let raw = buf.get(prefix..end).ok_or(WireError::Truncated)?;
    let text = std::str::from_utf8(raw).map_err(|_| WireError::InvalidUtf8)?;
    let chars = text.encode_utf16().count();
    if chars > max_chars {
        return Err(WireError::StringCharsOverLimit { chars, max_chars });
    }
    Ok((text.to_owned(), end))
}

pub fn write_string(value: &str, max_chars: usize, out: &mut Vec<u8>) -> Result<(), WireError> {
    let chars = value.encode_utf16().count();
    if chars > max_chars {
        return Err(WireError::StringCharsOverLimit { chars, max_chars });
    }
    let bytes = value.len();
    if bytes > max_chars.saturating_mul(MAX_UTF8_BYTES_PER_CHAR) {
        return Err(WireError::StringBytesOverLimit { bytes, max_chars });
    }
    let declared =
        i32::try_from(bytes).map_err(|_| WireError::StringBytesOverLimit { bytes, max_chars })?;
    write_var_int(declared, out);
    out.extend_from_slice(value.as_bytes());
    Ok(())
}

pub fn read_single_string(payload: &[u8]) -> Result<String, WireError> {
    let (text, consumed) = read_string(payload, MAX_STRING_CHARS)?;
    match payload.len() - consumed {
        0 => Ok(text),
        trailing => Err(WireError::TrailingBytes(trailing)),
    }
}

pub fn single_string_payload(value: &str) -> Result<Vec<u8>, WireError> {
    let mut out = Vec::with_capacity(value.len() + MAX_VAR_INT_BYTES);
    write_string(value, MAX_STRING_CHARS, &mut out)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn var_int_bytes(value: i32) -> Vec<u8> {
        let mut out = Vec::new();
        write_var_int(value, &mut out);
        out
    }

    #[test]
    fn var_int_matches_the_minecraft_reference_encodings() {
        let cases: [(i32, &[u8]); 9] = [
            (0, &[0x00]),
            (1, &[0x01]),
            (127, &[0x7f]),
            (128, &[0x80, 0x01]),
            (255, &[0xff, 0x01]),
            (25565, &[0xdd, 0xc7, 0x01]),
            (2_097_151, &[0xff, 0xff, 0x7f]),
            (i32::MAX, &[0xff, 0xff, 0xff, 0xff, 0x07]),
            (-1, &[0xff, 0xff, 0xff, 0xff, 0x0f]),
        ];
        for (value, encoded) in cases {
            assert_eq!(var_int_bytes(value), encoded, "encoding {value}");
            assert_eq!(
                read_var_int(encoded),
                Ok((value, encoded.len())),
                "decoding {value}"
            );
        }
        assert_eq!(
            read_var_int(&[0x80, 0x80, 0x80, 0x80, 0x08]),
            Ok((i32::MIN, 5))
        );
    }

    #[test]
    fn var_int_round_trips_across_the_whole_range() {
        let mut value: i64 = i64::from(i32::MIN);
        while value <= i64::from(i32::MAX) {
            let narrow = value as i32;
            let encoded = var_int_bytes(narrow);
            assert!(encoded.len() <= MAX_VAR_INT_BYTES);
            assert_eq!(read_var_int(&encoded), Ok((narrow, encoded.len())));
            value += 9_973_211;
        }
    }

    #[test]
    fn var_int_reports_truncation_and_overlong_input() {
        assert_eq!(read_var_int(&[]), Err(WireError::Truncated));
        assert_eq!(read_var_int(&[0x80]), Err(WireError::Truncated));
        assert_eq!(
            read_var_int(&[0x80, 0x80, 0x80, 0x80]),
            Err(WireError::Truncated)
        );
        assert_eq!(
            read_var_int(&[0x80, 0x80, 0x80, 0x80, 0x80]),
            Err(WireError::VarIntTooLong)
        );
        assert_eq!(
            read_var_int(&[0x80, 0x80, 0x80, 0x80, 0x80, 0x01]),
            Err(WireError::VarIntTooLong)
        );
    }

    #[test]
    fn var_int_read_stops_at_the_first_complete_value() {
        assert_eq!(read_var_int(&[0x05, 0xff, 0xff]), Ok((5, 1)));
    }

    #[test]
    fn string_round_trips_with_its_length_prefix() {
        let mut out = Vec::new();
        write_string("héllo", MAX_STRING_CHARS, &mut out).unwrap();
        assert_eq!(out[0], 6);
        assert_eq!(&out[1..], "héllo".as_bytes());
        assert_eq!(
            read_string(&out, MAX_STRING_CHARS),
            Ok(("héllo".to_owned(), 7))
        );
    }

    #[test]
    fn string_limit_counts_utf16_units_like_minecraft() {
        let at_limit = "a".repeat(MAX_STRING_CHARS);
        let payload = single_string_payload(&at_limit).unwrap();
        assert_eq!(
            read_single_string(&payload).unwrap().len(),
            MAX_STRING_CHARS
        );

        let over = "a".repeat(MAX_STRING_CHARS + 1);
        assert_eq!(
            single_string_payload(&over),
            Err(WireError::StringCharsOverLimit {
                chars: MAX_STRING_CHARS + 1,
                max_chars: MAX_STRING_CHARS
            })
        );

        let mut forged = Vec::new();
        write_var_int(over.len() as i32, &mut forged);
        forged.extend_from_slice(over.as_bytes());
        assert_eq!(
            read_single_string(&forged),
            Err(WireError::StringCharsOverLimit {
                chars: MAX_STRING_CHARS + 1,
                max_chars: MAX_STRING_CHARS
            })
        );
    }

    #[test]
    fn string_limit_on_bytes_is_checked_before_reading_the_body() {
        let mut forged = Vec::new();
        write_var_int(16 * 3 + 1, &mut forged);
        assert_eq!(
            read_string(&forged, 16),
            Err(WireError::StringBytesOverLimit {
                bytes: 49,
                max_chars: 16
            })
        );
    }

    #[test]
    fn supplementary_characters_count_as_two_units() {
        let emoji = "\u{1F600}".repeat(8);
        let mut out = Vec::new();
        write_string(&emoji, 16, &mut out).unwrap();
        assert_eq!(
            read_string(&out, 16).map(|(text, _)| text),
            Ok(emoji.clone())
        );
        let one_more = format!("{emoji}a");
        assert_eq!(
            write_string(&one_more, 16, &mut Vec::new()),
            Err(WireError::StringCharsOverLimit {
                chars: 17,
                max_chars: 16
            })
        );
    }

    #[test]
    fn string_rejects_negative_length_truncation_and_bad_utf8() {
        let mut negative = Vec::new();
        write_var_int(-1, &mut negative);
        assert_eq!(
            read_string(&negative, MAX_STRING_CHARS),
            Err(WireError::NegativeLength(-1))
        );
        assert_eq!(
            read_string(&[0x05, b'a', b'b'], MAX_STRING_CHARS),
            Err(WireError::Truncated)
        );
        assert_eq!(
            read_string(&[0x02, 0xc3, 0x28], MAX_STRING_CHARS),
            Err(WireError::InvalidUtf8)
        );
    }

    #[test]
    fn single_string_rejects_trailing_bytes() {
        let mut payload = single_string_payload("false").unwrap();
        payload.push(0);
        assert_eq!(
            read_single_string(&payload),
            Err(WireError::TrailingBytes(1))
        );
    }

    #[test]
    fn empty_string_is_one_zero_byte() {
        assert_eq!(single_string_payload("").unwrap(), vec![0]);
        assert_eq!(read_single_string(&[0]), Ok(String::new()));
    }
}
