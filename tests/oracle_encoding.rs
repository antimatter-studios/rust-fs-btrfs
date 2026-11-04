//! `guest_quote` and `guest_base64` carry every argument a test hands the
//! guest, so a mistake in either reaches the tool as a different argument.
//! Each expected string is written out here rather than computed: the shell's
//! quoting rules for one, RFC 4648's test vectors for the other.
//!
//! Pure string work: no tool, no fixture, no VM.

use fs_btrfs_test_support::{guest_base64, guest_quote};

#[test]
fn an_argument_is_wrapped_whole_in_single_quotes() {
    assert_eq!(guest_quote(""), "''");
    assert_eq!(guest_quote("plain"), "'plain'");
    // Nothing inside single quotes is special to the shell, so none of these
    // is escaped.
    assert_eq!(guest_quote(r#"a b$c`d"e\f*"#), r#"'a b$c`d"e\f*'"#);
    assert_eq!(guest_quote("two\nlines"), "'two\nlines'");
}

#[test]
fn a_single_quote_closes_the_string_is_escaped_and_reopens_it() {
    assert_eq!(guest_quote("it's"), r"'it'\''s'");
    assert_eq!(guest_quote("'"), r"''\'''");
    assert_eq!(guest_quote("''"), r"''\'''\'''");
}

#[test]
fn every_remainder_modulo_three_is_padded_as_rfc_4648_says() {
    for (input, expected) in [
        // Remainder 0.
        ("", ""),
        ("foo", "Zm9v"),
        ("foobar", "Zm9vYmFy"),
        // Remainder 1.
        ("f", "Zg=="),
        ("foob", "Zm9vYg=="),
        // Remainder 2.
        ("fo", "Zm8="),
        ("fooba", "Zm9vYmE="),
    ] {
        assert_eq!(guest_base64(input.as_bytes()), expected, "input {input:?}");
    }
}

#[test]
fn every_symbol_of_the_alphabet_comes_from_its_own_six_bits() {
    let bytes = [
        0x00, 0x10, 0x83, 0x10, 0x51, 0x87, 0x20, 0x92, 0x8b, 0x30, 0xd3, 0x8f, 0x41, 0x14, 0x93,
        0x51, 0x55, 0x97, 0x61, 0x96, 0x9b, 0x71, 0xd7, 0x9f, 0x82, 0x18, 0xa3, 0x92, 0x59, 0xa7,
        0xa2, 0x9a, 0xab, 0xb2, 0xdb, 0xaf, 0xc3, 0x1c, 0xb3, 0xd3, 0x5d, 0xb7, 0xe3, 0x9e, 0xbb,
        0xf3, 0xdf, 0xbf,
    ];
    assert_eq!(
        guest_base64(&bytes),
        "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"
    );
    assert_eq!(guest_base64(&[0xff]), "/w==");
}
