use sha2::{Digest, Sha256};

use crate::wire;

pub const MARKER_PROTOCOL_VERSION: u8 = 1;
pub const SUPPORTED_FLAGS: u8 = 0;
pub const NONCE_BITS: u32 = 26;
pub const NONCE_LIMIT: u32 = 1 << NONCE_BITS;
pub const MAX_USERNAME_CHARS: usize = 16;
pub const UUID_BYTES: usize = 16;

const MASK_DOMAIN: &[u8] = b"mcholepunch:uuid-mask";
const FIXED_MARKER: [u8; 10] = [0x17, 0x12, 0xf7, 0xcf, 0x8f, 0x1b, 0xf5, 0xe4, 0xfe, 0xd3];
const FIXED_BITS: usize = 80;
const PROTOCOL_VERSION_OFFSET: usize = 80;
const FLAGS_OFFSET: usize = 88;
const NONCE_OFFSET: usize = 96;
const MASKED_BITS: usize = 96;
const PAYLOAD_BITS: usize = 122;
const UUID_BITS: u8 = 128;
const VERSION_BYTE: usize = 6;
const VARIANT_BYTE: usize = 8;
const RANDOM_UUID_VERSION: u8 = 4;
const IETF_VARIANT: u8 = 0b10;
const PAYLOAD_BIT_POSITIONS: [u8; PAYLOAD_BITS] = payload_bit_positions();

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkerInfo {
    pub username: String,
    pub uuid: [u8; UUID_BYTES],
    pub nonce: u32,
}

pub fn is_marker_uuid(uuid: &[u8; UUID_BYTES]) -> bool {
    marker_nonce(uuid).is_some()
}

pub fn marker_nonce(uuid: &[u8; UUID_BYTES]) -> Option<u32> {
    if !has_random_uuid_layout(uuid) {
        return None;
    }
    let mut bits = [false; PAYLOAD_BITS];
    for (bit, position) in bits.iter_mut().zip(PAYLOAD_BIT_POSITIONS) {
        *bit = wire_bit(uuid, position);
    }
    let nonce = bits_to_u32(&bits[NONCE_OFFSET..]);
    let mask = nonce_mask(nonce);
    for (index, bit) in bits[..MASKED_BITS].iter_mut().enumerate() {
        *bit ^= sequence_bit(&mask, index);
    }
    let fixed_matches = bits[..FIXED_BITS]
        .iter()
        .enumerate()
        .all(|(index, bit)| *bit == sequence_bit(&FIXED_MARKER, index));
    let protocol_version = bits_to_u32(&bits[PROTOCOL_VERSION_OFFSET..FLAGS_OFFSET]);
    let flags = bits_to_u32(&bits[FLAGS_OFFSET..NONCE_OFFSET]);
    let accepted = fixed_matches
        && protocol_version == u32::from(MARKER_PROTOCOL_VERSION)
        && flags & !u32::from(SUPPORTED_FLAGS) == 0;
    accepted.then_some(nonce)
}

pub fn login_start_marker(payload: &[u8]) -> Option<MarkerInfo> {
    let (username, name_end) = wire::read_string(payload, MAX_USERNAME_CHARS).ok()?;
    if username.is_empty() {
        return None;
    }
    let uuid: [u8; UUID_BYTES] = payload.get(name_end..)?.try_into().ok()?;
    let nonce = marker_nonce(&uuid)?;
    Some(MarkerInfo {
        username,
        uuid,
        nonce,
    })
}

fn has_random_uuid_layout(uuid: &[u8; UUID_BYTES]) -> bool {
    uuid[VERSION_BYTE] >> 4 == RANDOM_UUID_VERSION && uuid[VARIANT_BYTE] >> 6 == IETF_VARIANT
}

const fn is_layout_position(position: u8) -> bool {
    matches!(position, 76..=79 | 62..=63)
}

const fn payload_bit_positions() -> [u8; PAYLOAD_BITS] {
    let mut positions = [0u8; PAYLOAD_BITS];
    let mut next = 0;
    let mut position = UUID_BITS;
    while position > 0 {
        position -= 1;
        if !is_layout_position(position) {
            positions[next] = position;
            next += 1;
        }
    }
    positions
}

fn wire_bit(uuid: &[u8; UUID_BYTES], position: u8) -> bool {
    let byte = uuid[UUID_BYTES - 1 - usize::from(position / 8)];
    (byte >> (position % 8)) & 1 == 1
}

fn sequence_bit(bytes: &[u8], index: usize) -> bool {
    (bytes[index / 8] >> (7 - index % 8)) & 1 == 1
}

fn bits_to_u32(bits: &[bool]) -> u32 {
    bits.iter()
        .fold(0, |value, bit| (value << 1) | u32::from(*bit))
}

fn nonce_mask(nonce: u32) -> [u8; 32] {
    Sha256::new()
        .chain_update(MASK_DOMAIN)
        .chain_update(nonce.to_be_bytes())
        .finalize()
        .into()
}

#[cfg(test)]
pub(crate) fn encode_marker(protocol_version: u8, flags: u8, nonce: u32) -> [u8; UUID_BYTES] {
    assert!(
        nonce < NONCE_LIMIT,
        "nonce {nonce} does not fit in {NONCE_BITS} bits"
    );
    let mask = nonce_mask(nonce);
    let mut uuid = [0u8; UUID_BYTES];
    for (index, position) in PAYLOAD_BIT_POSITIONS.into_iter().enumerate() {
        let field = if index < FIXED_BITS {
            sequence_bit(&FIXED_MARKER, index)
        } else if index < FLAGS_OFFSET {
            (protocol_version >> (FLAGS_OFFSET - 1 - index)) & 1 == 1
        } else if index < NONCE_OFFSET {
            (flags >> (NONCE_OFFSET - 1 - index)) & 1 == 1
        } else {
            (nonce >> (PAYLOAD_BITS - 1 - index)) & 1 == 1
        };
        let masked = index < MASKED_BITS && sequence_bit(&mask, index);
        if field ^ masked {
            uuid[UUID_BYTES - 1 - usize::from(position / 8)] |= 1 << (position % 8);
        }
    }
    uuid[VERSION_BYTE] = (uuid[VERSION_BYTE] & 0x0f) | (RANDOM_UUID_VERSION << 4);
    uuid[VARIANT_BYTE] = (uuid[VARIANT_BYTE] & 0x3f) | (IETF_VARIANT << 6);
    uuid
}

#[cfg(test)]
pub(crate) fn login_start_payload(username: &str, uuid: &[u8; UUID_BYTES]) -> Vec<u8> {
    let mut payload = Vec::new();
    wire::write_string(username, wire::MAX_STRING_CHARS, &mut payload).unwrap();
    payload.extend_from_slice(uuid);
    payload
}

#[cfg(test)]
pub(crate) mod test_support {
    pub(crate) struct SplitMix64(u64);

    impl SplitMix64 {
        pub(crate) fn new(seed: u64) -> Self {
            SplitMix64(seed)
        }

        pub(crate) fn next_u64(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^ (z >> 31)
        }

        pub(crate) fn next_uuid(&mut self) -> [u8; 16] {
            let mut uuid = [0u8; 16];
            uuid[..8].copy_from_slice(&self.next_u64().to_be_bytes());
            uuid[8..].copy_from_slice(&self.next_u64().to_be_bytes());
            uuid
        }

        pub(crate) fn next_below(&mut self, limit: u32) -> u32 {
            (self.next_u64() % u64::from(limit)) as u32
        }
    }

    pub(crate) fn hex(text: &str) -> Vec<u8> {
        assert!(text.len().is_multiple_of(2), "odd hex length");
        (0..text.len())
            .step_by(2)
            .map(|at| u8::from_str_radix(&text[at..at + 2], 16).unwrap())
            .collect()
    }

    pub(crate) fn uuid_from_hex(text: &str) -> [u8; 16] {
        hex(text).try_into().unwrap()
    }
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    use super::test_support::{SplitMix64, hex, uuid_from_hex};
    use super::*;

    const VECTORS: &str = include_str!("../testdata/holepunch_vectors.json");

    #[derive(Deserialize)]
    struct Vectors {
        markers: Vec<MarkerVector>,
        rejected: Vec<RejectedVector>,
        bit_flips_of_nonce_19088743: Vec<FlipVector>,
        login_start_frames: Vec<LoginStartVector>,
    }

    #[derive(Deserialize)]
    struct MarkerVector {
        flags: u8,
        nonce: u32,
        uuid: String,
        jar_decodes: bool,
        jar_nonce: Option<u32>,
    }

    #[derive(Deserialize)]
    struct RejectedVector {
        kind: String,
        uuid: String,
        jar_decodes: bool,
    }

    #[derive(Deserialize)]
    struct FlipVector {
        flipped_wire_bit: u32,
        uuid: String,
        jar_decodes: bool,
    }

    #[derive(Deserialize)]
    struct LoginStartVector {
        username: String,
        nonce: Option<u32>,
        uuid: String,
        frame: String,
        jar_decodes: bool,
    }

    fn vectors() -> Vectors {
        serde_json::from_str(VECTORS).unwrap()
    }

    fn login_start_payload_of_frame(frame: &[u8]) -> &[u8] {
        let (frame_len, prefix) = wire::read_var_int(frame).unwrap();
        assert_eq!(
            frame_len as usize,
            frame.len() - prefix,
            "frame length prefix"
        );
        let (packet_id, id_len) = wire::read_var_int(&frame[prefix..]).unwrap();
        assert_eq!(packet_id, 0, "Login Start packet id");
        &frame[prefix + id_len..]
    }

    #[test]
    fn fixed_marker_is_the_sha256_prefix_of_its_domain() {
        let digest = Sha256::digest(b"mcholepunch:marker-hash");
        assert_eq!(digest[..10], FIXED_MARKER);
    }

    #[test]
    fn payload_positions_skip_exactly_the_uuid_version_and_variant_bits() {
        assert_eq!(PAYLOAD_BIT_POSITIONS.len(), 122);
        assert_eq!(PAYLOAD_BIT_POSITIONS[0], 127);
        assert_eq!(PAYLOAD_BIT_POSITIONS[PAYLOAD_BITS - 1], 0);
        assert!(
            PAYLOAD_BIT_POSITIONS
                .windows(2)
                .all(|pair| pair[0] > pair[1])
        );
        for reserved in [76, 77, 78, 79, 62, 63] {
            assert!(
                !PAYLOAD_BIT_POSITIONS.contains(&reserved),
                "position {reserved} is reserved"
            );
        }
    }

    #[test]
    fn every_jar_marker_decodes_with_the_jar_nonce() {
        for vector in vectors().markers {
            let uuid = uuid_from_hex(&vector.uuid);
            assert!(vector.jar_decodes);
            assert_eq!(vector.flags, 0);
            assert_eq!(vector.jar_nonce, Some(vector.nonce));
            assert_eq!(
                marker_nonce(&uuid),
                Some(vector.nonce),
                "uuid {}",
                vector.uuid
            );
            assert!(is_marker_uuid(&uuid));
        }
    }

    #[test]
    fn test_encoder_reproduces_every_jar_marker_bit_for_bit() {
        for vector in vectors().markers {
            assert_eq!(
                encode_marker(MARKER_PROTOCOL_VERSION, vector.flags, vector.nonce),
                uuid_from_hex(&vector.uuid),
                "nonce {}",
                vector.nonce
            );
        }
    }

    #[test]
    fn every_uuid_the_jar_rejects_is_rejected() {
        let vectors = vectors();
        assert!(!vectors.rejected.is_empty());
        for vector in vectors.rejected {
            assert!(!vector.jar_decodes, "{}", vector.kind);
            assert!(
                !is_marker_uuid(&uuid_from_hex(&vector.uuid)),
                "{} {}",
                vector.kind,
                vector.uuid
            );
        }
    }

    #[test]
    fn flipping_any_single_bit_of_a_jar_marker_breaks_it() {
        let flips = vectors().bit_flips_of_nonce_19088743;
        assert_eq!(flips.len(), 128);
        for vector in flips {
            assert!(!vector.jar_decodes);
            assert!(
                !is_marker_uuid(&uuid_from_hex(&vector.uuid)),
                "flipping wire bit {} still decodes",
                vector.flipped_wire_bit
            );
        }
    }

    #[test]
    fn jar_login_start_frames_decode_like_the_jar() {
        for vector in vectors().login_start_frames {
            let frame = hex(&vector.frame);
            let payload = login_start_payload_of_frame(&frame);
            let uuid = uuid_from_hex(&vector.uuid);
            assert_eq!(
                payload,
                login_start_payload(&vector.username, &uuid).as_slice()
            );
            match login_start_marker(payload) {
                Some(info) => {
                    assert!(
                        vector.jar_decodes,
                        "{} decoded but the jar refused it",
                        vector.username
                    );
                    assert_eq!(info.username, vector.username);
                    assert_eq!(info.uuid, uuid);
                    assert_eq!(Some(info.nonce), vector.nonce);
                }
                None => assert!(
                    !vector.jar_decodes,
                    "{} refused but the jar decoded it",
                    vector.username
                ),
            }
        }
    }

    #[test]
    fn seeded_random_uuids_are_never_markers() {
        let mut rng = SplitMix64::new(0x4155_544f_4d4f_4450);
        for _ in 0..200_000 {
            let mut uuid = rng.next_uuid();
            assert!(!is_marker_uuid(&uuid), "{uuid:02x?}");
            uuid[VERSION_BYTE] = (uuid[VERSION_BYTE] & 0x0f) | 0x40;
            uuid[VARIANT_BYTE] = (uuid[VARIANT_BYTE] & 0x3f) | 0x80;
            assert!(!is_marker_uuid(&uuid), "{uuid:02x?}");
        }
    }

    #[test]
    fn encoder_round_trips_seeded_nonces() {
        let mut rng = SplitMix64::new(7);
        let mut nonces = vec![0, 1, NONCE_LIMIT - 1, NONCE_LIMIT / 2];
        nonces.extend((0..5_000).map(|_| rng.next_below(NONCE_LIMIT)));
        for nonce in nonces {
            let uuid = encode_marker(MARKER_PROTOCOL_VERSION, 0, nonce);
            assert_eq!(marker_nonce(&uuid), Some(nonce));
            let payload = login_start_payload("Steve", &uuid);
            assert_eq!(
                login_start_marker(&payload),
                Some(MarkerInfo {
                    username: "Steve".to_owned(),
                    uuid,
                    nonce
                })
            );
        }
    }

    #[test]
    fn any_uuid_version_but_four_is_rejected() {
        let marker = encode_marker(MARKER_PROTOCOL_VERSION, 0, 0x00ab_cdef);
        for version in (0..16u8).filter(|version| *version != RANDOM_UUID_VERSION) {
            let mut tampered = marker;
            tampered[VERSION_BYTE] = (tampered[VERSION_BYTE] & 0x0f) | (version << 4);
            assert!(!is_marker_uuid(&tampered), "version {version}");
        }
    }

    #[test]
    fn any_variant_but_ietf_is_rejected() {
        let marker = encode_marker(MARKER_PROTOCOL_VERSION, 0, 0x00ab_cdef);
        for variant in [0b00u8, 0b01, 0b11] {
            let mut tampered = marker;
            tampered[VARIANT_BYTE] = (tampered[VARIANT_BYTE] & 0x3f) | (variant << 6);
            assert!(!is_marker_uuid(&tampered), "variant {variant:02b}");
        }
    }

    #[test]
    fn other_protocol_versions_and_any_flag_are_rejected() {
        let mut rng = SplitMix64::new(99);
        for _ in 0..64 {
            let nonce = rng.next_below(NONCE_LIMIT);
            for protocol_version in [0u8, 2, 3, 0x80, 0xff] {
                assert!(
                    !is_marker_uuid(&encode_marker(protocol_version, 0, nonce)),
                    "protocol version {protocol_version}"
                );
            }
            for flags in [1u8, 2, 4, 0x40, 0x80, 0xff] {
                assert!(
                    !is_marker_uuid(&encode_marker(MARKER_PROTOCOL_VERSION, flags, nonce)),
                    "flags {flags:#x}"
                );
            }
        }
    }

    #[test]
    fn login_start_requires_the_exact_name_then_uuid_layout() {
        let uuid = encode_marker(MARKER_PROTOCOL_VERSION, 0, 42);
        let payload = login_start_payload("Alex", &uuid);
        assert!(login_start_marker(&payload).is_some());

        let mut trailing = payload.clone();
        trailing.push(0);
        assert_eq!(login_start_marker(&trailing), None, "trailing byte");

        assert_eq!(
            login_start_marker(&payload[..payload.len() - 1]),
            None,
            "missing uuid byte"
        );
        assert_eq!(login_start_marker(&payload[..5]), None, "name only");
        assert_eq!(login_start_marker(&[]), None, "empty payload");
    }

    #[test]
    fn login_start_name_must_be_one_to_sixteen_chars_of_utf8() {
        let uuid = encode_marker(MARKER_PROTOCOL_VERSION, 0, 7);
        assert!(login_start_marker(&login_start_payload(&"x".repeat(16), &uuid)).is_some());
        assert!(login_start_marker(&login_start_payload("é", &uuid)).is_some());
        assert_eq!(
            login_start_marker(&login_start_payload(&"x".repeat(17), &uuid)),
            None
        );
        assert_eq!(login_start_marker(&login_start_payload("", &uuid)), None);

        let mut bad_utf8 = vec![2, 0xc3, 0x28];
        bad_utf8.extend_from_slice(&uuid);
        assert_eq!(login_start_marker(&bad_utf8), None);
    }

    #[test]
    fn pre_1_20_2_login_start_layouts_are_not_markers() {
        let uuid = encode_marker(MARKER_PROTOCOL_VERSION, 0, 9);
        let mut with_has_uuid_flag = Vec::new();
        wire::write_string("Alex", 16, &mut with_has_uuid_flag).unwrap();
        with_has_uuid_flag.push(0);
        with_has_uuid_flag.push(1);
        with_has_uuid_flag.extend_from_slice(&uuid);
        assert_eq!(login_start_marker(&with_has_uuid_flag), None);

        let mut name_only = Vec::new();
        wire::write_string("Alex", 16, &mut name_only).unwrap();
        assert_eq!(login_start_marker(&name_only), None);
    }

    #[test]
    fn a_regular_player_login_start_is_not_a_marker() {
        let offline_notch = uuid_from_hex("b50ad385829d3141a2167e7d7539ba7f");
        assert_eq!(
            login_start_marker(&login_start_payload("Notch", &offline_notch)),
            None
        );
        let online_notch = uuid_from_hex("069a79f444e94726a5befca90e38aaf5");
        assert_eq!(
            login_start_marker(&login_start_payload("Notch", &online_notch)),
            None
        );
    }
}
