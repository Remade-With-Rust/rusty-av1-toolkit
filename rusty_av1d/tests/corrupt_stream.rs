//! A corrupted stream must come back as errors from the safe API, never as a panic.
//!
//! The input is a 10-bit AV1 test clip (`testsrc2`, 192x108, libaom) with bytes mutated by
//! a fuzzer, stored as the packets a demuxer hands out: a 4-byte little-endian length, then
//! the temporal unit (the `av1C` sequence header OBUs prepended to the first). One of its
//! frames fails inside `rav1d_decode_frame`, which releases the frame header, and the
//! frame-error cleanup in `rav1d_submit_frame` then unwrapped that header and panicked.

use rusty_av1d::{Decoder, Rav1dError, Settings};

fn packets(mut data: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    while data.len() >= 4 {
        let len = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
        let end = (4 + len).min(data.len());
        out.push(data[4..end].to_vec());
        data = &data[end..];
    }
    out
}

#[test]
fn frame_error_after_the_header_is_released_is_an_error_not_a_panic() {
    let packets = packets(include_bytes!(
        "data/corrupt_frame_after_header_release.tus"
    ));
    assert_eq!(packets.len(), 18);
    let mut settings = Settings::new();
    settings.set_n_threads(1);
    settings.set_max_frame_delay(1);
    let mut decoder = Decoder::with_settings(&settings).expect("decoder");
    let mut errors = 0;
    for data in packets {
        let mut sent = decoder.send_data(data.into_boxed_slice(), None, None, None);
        while let Err(Rav1dError::TryAgain) = sent {
            while decoder.get_picture().is_ok() {}
            sent = decoder.send_pending_data();
        }
        if sent.is_err() {
            errors += 1;
        }
        while decoder.get_picture().is_ok() {}
    }
    assert!(
        errors > 0,
        "the corrupted stream decoded without a single error"
    );
}
