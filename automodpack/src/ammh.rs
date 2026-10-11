pub const AMMH: [u8; 4] = *b"AMMH";
pub const AMOK: [u8; 4] = *b"AMOK";
pub const HEADER_LEN: usize = AMMH.len() + 2;
pub const MAX_HOST_BYTES: usize = 255;
pub const MAX_PREAMBLE_LEN: usize = HEADER_LEN + MAX_HOST_BYTES;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ammh {
    NoMatch,
    NeedMore(usize),
    Malformed(MalformedHost),
    Complete { host: String, consumed: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MalformedHost {
    Empty,
    TooLong(usize),
    InvalidUtf8,
    ControlCharacter,
}

pub fn parse(data: &[u8]) -> Ammh {
    let magic_seen = data.len().min(AMMH.len());
    if data[..magic_seen] != AMMH[..magic_seen] {
        return Ammh::NoMatch;
    }
    if data.len() < HEADER_LEN {
        return Ammh::NeedMore(HEADER_LEN);
    }
    let host_len = usize::from(u16::from_be_bytes([data[4], data[5]]));
    if let Err(malformed) = check_host_len(host_len) {
        return Ammh::Malformed(malformed);
    }
    let consumed = HEADER_LEN + host_len;
    let Some(raw_host) = data.get(HEADER_LEN..consumed) else {
        return Ammh::NeedMore(consumed);
    };
    match validate_host(raw_host) {
        Ok(host) => Ammh::Complete {
            host: host.to_owned(),
            consumed,
        },
        Err(malformed) => Ammh::Malformed(malformed),
    }
}

pub fn validate_host(raw: &[u8]) -> Result<&str, MalformedHost> {
    check_host_len(raw.len())?;
    let host = std::str::from_utf8(raw).map_err(|_| MalformedHost::InvalidUtf8)?;
    if host.chars().any(char::is_control) {
        return Err(MalformedHost::ControlCharacter);
    }
    Ok(host)
}

pub fn lookup_key(host: &str) -> String {
    host.strip_suffix('.').unwrap_or(host).to_ascii_lowercase()
}

pub fn encode(host: &str) -> Result<Vec<u8>, MalformedHost> {
    let host = validate_host(host.as_bytes())?;
    let mut preamble = Vec::with_capacity(HEADER_LEN + host.len());
    preamble.extend_from_slice(&AMMH);
    let host_len = u16::try_from(host.len()).map_err(|_| MalformedHost::TooLong(host.len()))?;
    preamble.extend_from_slice(&host_len.to_be_bytes());
    preamble.extend_from_slice(host.as_bytes());
    Ok(preamble)
}

fn check_host_len(len: usize) -> Result<(), MalformedHost> {
    match len {
        0 => Err(MalformedHost::Empty),
        1..=MAX_HOST_BYTES => Ok(()),
        _ => Err(MalformedHost::TooLong(len)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn preamble(host: &[u8]) -> Vec<u8> {
        let mut bytes = AMMH.to_vec();
        bytes.extend_from_slice(&(host.len() as u16).to_be_bytes());
        bytes.extend_from_slice(host);
        bytes
    }

    #[test]
    fn magic_constants_are_the_java_ints() {
        assert_eq!(u32::from_be_bytes(AMMH), 0x414D_4D48);
        assert_eq!(u32::from_be_bytes(AMOK), 0x414D_4F4B);
    }

    #[test]
    fn complete_preamble_reports_host_and_consumed_length() {
        let mut data = preamble(b"play.example.com");
        data.extend_from_slice(&[0x16, 0x03, 0x01]);
        assert_eq!(
            parse(&data),
            Ammh::Complete {
                host: "play.example.com".to_owned(),
                consumed: 22
            }
        );
    }

    #[test]
    fn matches_the_bytes_lane_connector_writes() {
        let java_written = [
            0x41, 0x4d, 0x4d, 0x48, 0x00, 0x09, b'l', b'o', b'c', b'a', b'l', b'h', b'o', b's',
            b't',
        ];
        assert_eq!(
            parse(&java_written),
            Ammh::Complete {
                host: "localhost".to_owned(),
                consumed: 15
            }
        );
        assert_eq!(encode("localhost").unwrap(), java_written);
    }

    #[test]
    fn every_strict_prefix_asks_for_more() {
        let data = preamble(b"mc.example.net");
        for cut in 0..data.len() {
            let expected = if cut < HEADER_LEN {
                HEADER_LEN
            } else {
                data.len()
            };
            assert_eq!(
                parse(&data[..cut]),
                Ammh::NeedMore(expected),
                "cut at {cut}"
            );
        }
    }

    #[test]
    fn anything_that_diverges_from_the_magic_is_no_match() {
        assert_eq!(parse(b"X"), Ammh::NoMatch);
        assert_eq!(parse(b"AX"), Ammh::NoMatch);
        assert_eq!(parse(b"AMMX"), Ammh::NoMatch);
        assert_eq!(parse(b"AMOK"), Ammh::NoMatch);
        assert_eq!(parse(&[0x10, 0x00, 0xff, 0x05]), Ammh::NoMatch);
        assert_eq!(parse(&[0xfe, 0x01, 0xfa]), Ammh::NoMatch);
        assert_eq!(parse(&[0x16, 0x03, 0x01, 0x02, 0x00]), Ammh::NoMatch);
    }

    #[test]
    fn a_minecraft_handshake_of_length_65_is_not_mistaken_for_magic() {
        let mut handshake = vec![0x41, 0x00, 0xf2, 0x05];
        handshake.resize(66, b'a');
        assert_eq!(parse(&handshake), Ammh::NoMatch);
        assert_eq!(parse(&handshake[..1]), Ammh::NeedMore(HEADER_LEN));
    }

    #[test]
    fn host_length_is_rejected_from_the_header_alone() {
        assert_eq!(
            parse(&[b'A', b'M', b'M', b'H', 0, 0]),
            Ammh::Malformed(MalformedHost::Empty)
        );
        assert_eq!(
            parse(&[b'A', b'M', b'M', b'H', 0x01, 0x00]),
            Ammh::Malformed(MalformedHost::TooLong(256))
        );
        assert_eq!(
            parse(&[b'A', b'M', b'M', b'H', 0xff, 0xff]),
            Ammh::Malformed(MalformedHost::TooLong(65535))
        );
    }

    #[test]
    fn longest_allowed_host_is_accepted() {
        let host = "a".repeat(MAX_HOST_BYTES);
        let data = preamble(host.as_bytes());
        assert_eq!(data.len(), MAX_PREAMBLE_LEN);
        assert_eq!(
            parse(&data),
            Ammh::Complete {
                host,
                consumed: MAX_PREAMBLE_LEN
            }
        );
    }

    #[test]
    fn host_must_be_utf8_without_control_characters() {
        assert_eq!(
            parse(&preamble(&[0xc3, 0x28])),
            Ammh::Malformed(MalformedHost::InvalidUtf8)
        );
        assert_eq!(
            parse(&preamble(b"host\0FML3\0")),
            Ammh::Malformed(MalformedHost::ControlCharacter)
        );
        assert_eq!(
            parse(&preamble(b"evil\nhost")),
            Ammh::Malformed(MalformedHost::ControlCharacter)
        );
        assert_eq!(
            parse(&preamble("mündchen.de".as_bytes())),
            Ammh::Complete {
                host: "mündchen.de".to_owned(),
                consumed: HEADER_LEN + "mündchen.de".len()
            }
        );
    }

    #[test]
    fn lookup_key_lowercases_and_drops_one_trailing_dot() {
        assert_eq!(lookup_key("Play.Example.COM."), "play.example.com");
        assert_eq!(lookup_key("play.example.com"), "play.example.com");
        assert_eq!(lookup_key("2001:DB8::1"), "2001:db8::1");
        assert_eq!(lookup_key("a.."), "a.");
    }

    #[test]
    fn parsing_keeps_the_original_spelling_for_replay() {
        let data = preamble(b"Play.Example.COM.");
        let Ammh::Complete { host, consumed } = parse(&data) else {
            panic!("expected a complete preamble");
        };
        assert_eq!(host, "Play.Example.COM.");
        assert_eq!(&data[..consumed], encode(&host).unwrap().as_slice());
    }

    #[test]
    fn encode_refuses_hosts_the_parser_would_refuse() {
        assert_eq!(encode(""), Err(MalformedHost::Empty));
        assert_eq!(
            encode(&"a".repeat(MAX_HOST_BYTES + 1)),
            Err(MalformedHost::TooLong(256))
        );
        assert_eq!(encode("a\tb"), Err(MalformedHost::ControlCharacter));
    }
}
