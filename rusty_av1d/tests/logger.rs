//! The safe API's `Settings::set_logger`: the decoder's diagnostic messages can be
//! discarded or routed to a callback instead of going to standard error.

use std::sync::{Arc, Mutex};

use rusty_av1d::{Decoder, Logger, Rav1dError, Settings};

/// A sequence header OBU with the reserved `seq_profile` 7: fails to parse.
const BAD_SEQUENCE_HEADER: [u8; 4] = [0x0a, 0x02, 0xff, 0xff];

fn decode_bad_header(logger: Logger) -> Result<(), Rav1dError> {
    let mut settings = Settings::new();
    settings.set_n_threads(1);
    settings.set_max_frame_delay(1);
    settings.set_logger(logger);
    let mut decoder = Decoder::with_settings(&settings).expect("decoder");
    decoder.send_data(Box::new(BAD_SEQUENCE_HEADER), None, None, None)
}

#[test]
fn messages_reach_a_callback_logger() {
    let seen = Arc::new(Mutex::new(String::new()));
    let sink = seen.clone();
    let result = decode_bad_header(Logger::Callback(Arc::new(move |message: &str| {
        sink.lock().unwrap().push_str(message);
    })));
    assert_eq!(result, Err(Rav1dError::InvalidArgument));
    let seen = seen.lock().unwrap();
    assert!(
        seen.contains("Error parsing sequence header"),
        "got {seen:?}"
    );
}

#[test]
fn messages_can_be_discarded() {
    assert_eq!(
        decode_bad_header(Logger::None),
        Err(Rav1dError::InvalidArgument)
    );
}
