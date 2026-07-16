use controller_wire::{
    FRAME_HEADER_BYTES, FrameHeaderV1, LogicalChannel, WireError, decode_rkyv_frame_v1,
    decode_rkyv_frame_v1_on_channel, fuzz_json_frame, fuzz_rkyv_frame,
};

const LIMIT: usize = 1024 * 1024;

#[test]
fn oversized_declared_payload_is_rejected_from_the_header_only() {
    let mut frame = [0_u8; FRAME_HEADER_BYTES];
    frame[..4].copy_from_slice(b"LCRK");
    frame[4] = 1;
    frame[6..8].copy_from_slice(&7_u16.to_le_bytes());
    frame[8..12].copy_from_slice(&u32::MAX.to_le_bytes());

    assert!(matches!(
        decode_rkyv_frame_v1(&frame, 1024),
        Err(WireError::PayloadTooLarge {
            declared: value,
            limit: 1024
        }) if value == u32::MAX as usize
    ));
}

#[test]
fn every_golden_root_rejects_every_wrong_logical_channel() {
    let channels = [
        LogicalChannel::McpSession,
        LogicalChannel::SessionControl,
        LogicalChannel::Command,
        LogicalChannel::RuntimeEvents,
    ];
    for frame in golden_frames() {
        let header = FrameHeaderV1::decode(&frame, LIMIT).expect("golden header");
        for channel in channels {
            let result = decode_rkyv_frame_v1_on_channel(&frame, LIMIT, channel);
            if channel == header.kind.logical_channel() {
                assert!(result.is_ok(), "kind {:?} on expected channel", header.kind);
            } else {
                assert!(matches!(
                    result,
                    Err(WireError::InvalidLogicalChannel { .. })
                ));
            }
        }
    }
}

#[test]
fn deterministic_arbitrary_and_unaligned_inputs_never_panic() {
    let mut state = 0x6a09_e667_f3bc_c909_u64;
    for _ in 0..20_000 {
        let length = (next(&mut state) as usize) % 513;
        let mut storage = Vec::with_capacity(length + 1);
        storage.push(0xa5);
        storage.extend((0..length).map(|_| next(&mut state) as u8));
        let input = &storage[1..];
        let limit = (next(&mut state) as usize) % 2049;

        if let Err(error) = decode_rkyv_frame_v1(input, limit) {
            assert_bounded_category(&error);
        }
        fuzz_rkyv_frame(input, limit);
        fuzz_json_frame(input, limit);
    }
}

#[test]
fn mutations_of_every_golden_root_use_only_bounded_error_categories() {
    for frame in golden_frames() {
        for index in 0..frame.len() {
            for mask in [0x01, 0x80, 0xff] {
                let mut mutated = frame.clone();
                mutated[index] ^= mask;
                if let Err(error) = decode_rkyv_frame_v1(&mutated, LIMIT) {
                    assert_bounded_category(&error);
                }
            }
        }

        let mut trailing = frame.clone();
        trailing.extend_from_slice(&[0, 0xff]);
        assert!(matches!(
            decode_rkyv_frame_v1(&trailing, LIMIT),
            Err(WireError::LengthMismatch { .. })
        ));
        for length in 0..frame.len() {
            let _ = decode_rkyv_frame_v1(&frame[..length], LIMIT);
        }
    }
}

fn assert_bounded_category(error: &WireError) {
    let category = error.category();
    assert!(category.len() <= 32);
    assert!(
        category
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    );
}

fn next(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

fn golden_frames() -> Vec<Vec<u8>> {
    include_str!("../fixtures/runtime-rkyv-v1.hex")
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| {
            let (_, hex) = line.split_once('=').expect("kind=hex fixture line");
            hex.as_bytes()
                .chunks_exact(2)
                .map(|pair| {
                    u8::from_str_radix(std::str::from_utf8(pair).expect("fixture UTF-8"), 16)
                        .expect("fixture hex")
                })
                .collect()
        })
        .collect()
}
