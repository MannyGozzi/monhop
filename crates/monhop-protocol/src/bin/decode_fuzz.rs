//! Standalone deterministic decoder fuzz target. It uses only the stable Rust
//! toolchain and can run with `cargo run -p monhop-protocol --bin decode_fuzz`.

use monhop_protocol::{Frame, MAX_FRAME_LEN, Message, SessionEpoch, decode};

fn next(seed: &mut u64) -> u64 {
    *seed ^= *seed << 13;
    *seed ^= *seed >> 7;
    *seed ^= *seed << 17;
    *seed
}

fn main() {
    let epoch = SessionEpoch::new(1).expect("constant epoch is nonzero");
    let mut valid = Vec::new();
    Frame::new(epoch, 0, Message::ReleaseAll)
        .encode_into(&mut valid)
        .expect("constant frame is valid");

    let mut seed = 0xC0DE_CAFE_5EED_1234_u64;
    for case_number in 0..10_000 {
        let length = (next(&mut seed) as usize) % (MAX_FRAME_LEN + 2);
        let mut input = vec![0_u8; length];
        for byte in &mut input {
            *byte = next(&mut seed) as u8;
        }
        if case_number % 3 == 0 && !input.is_empty() {
            let copied = valid.len().min(input.len());
            input[..copied].copy_from_slice(&valid[..copied]);
        }
        let _ = decode(&input);
    }
}
