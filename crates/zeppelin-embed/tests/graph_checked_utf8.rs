#![allow(clippy::unwrap_used)]
use zeppelin_embed::property_graph::{Utf8CheckError, checked_utf8};
#[test]
fn checked_utf8_retains_four_byte_codepoints_split_at_every_window_boundary() {
    for start in [65533, 65534, 65535] {
        let text = format!("{}😀{}", "a".repeat(start), "λ".repeat(32769));
        let mut polls = 0;
        let result = checked_utf8(text.as_bytes(), || {
            polls += 1;
            Ok::<_, u8>(())
        })
        .unwrap();
        assert_eq!(result, text);
        assert_eq!(result.as_ptr(), text.as_ptr());
        assert_eq!(polls, 3);
    }
}
#[test]
fn checked_utf8_rejects_invalid_and_truncated_sequences_at_and_after_boundaries() {
    for tail in [
        &[0xff][..],
        &[0xf0, 0x9f, 0x98][..],
        &[0xc0, 0x80][..],
        &[0xed, 0xa0, 0x80][..],
        &[0xf4, 0x90, 0x80, 0x80][..],
    ] {
        for prefix in [0, 65535, 65536, 131072] {
            let mut bytes = vec![b'a'; prefix];
            bytes.extend_from_slice(tail);
            assert_eq!(
                checked_utf8(&bytes, || Ok::<_, u8>(())),
                Err(Utf8CheckError::Invalid)
            );
        }
    }
}
#[test]
fn checked_utf8_polls_each_window_and_preserves_exact_caller_error() {
    let text = "😀".repeat(32769);
    for fire in 1..=3 {
        let mut polls = 0;
        assert_eq!(
            checked_utf8(text.as_bytes(), || {
                polls += 1;
                if polls == fire { Err(73) } else { Ok(()) }
            }),
            Err(Utf8CheckError::Control(73))
        );
        assert_eq!(polls, fire);
    }
    assert_eq!(checked_utf8(&[], || Ok::<_, u8>(())), Ok(""));
}
