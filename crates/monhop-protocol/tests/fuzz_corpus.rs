use monhop_protocol::{Frame, MAX_FRAME_LEN, Message, SessionEpoch, decode};

fn next(seed: &mut u64) -> u64 {
    *seed ^= *seed << 13;
    *seed ^= *seed >> 7;
    *seed ^= *seed << 17;
    *seed
}

#[test]
fn deterministic_malicious_input_corpus_never_panics() {
    let epoch = SessionEpoch::new(1).expect("nonzero epoch");
    let mut valid = Vec::new();
    Frame::new(epoch, 0, Message::ReleaseAll)
        .encode_into(&mut valid)
        .expect("encode seed frame");

    let fixed_cases = [
        Vec::new(),
        vec![0; MAX_FRAME_LEN + 1],
        vec![0xFF; MAX_FRAME_LEN],
        valid.clone(),
    ];
    for case in fixed_cases {
        let _ = decode(&case);
    }

    let mut seed = 0xC0DE_CAFE_5EED_1234_u64;
    for case_number in 0..2_048 {
        let length = (next(&mut seed) as usize) % (MAX_FRAME_LEN + 2);
        let mut case = vec![0_u8; length];
        for byte in &mut case {
            *byte = next(&mut seed) as u8;
        }
        if case_number % 3 == 0 && !case.is_empty() {
            let source = &valid[..valid.len().min(case.len())];
            case[..source.len()].copy_from_slice(source);
        }
        let first = decode(&case);
        let second = decode(&case);
        assert_eq!(first, second, "case {case_number} was nondeterministic");
    }
}
