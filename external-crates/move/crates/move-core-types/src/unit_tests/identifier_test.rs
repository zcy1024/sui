// Copyright (c) The Diem Core Contributors
// Copyright (c) The Move Contributors
// SPDX-License-Identifier: Apache-2.0

use crate::{
    gas_algebra::AbstractMemorySize,
    identifier::{ALLOWED_IDENTIFIERS, IdentStr, Identifier},
};
use bcs::test_helpers::assert_canonical_encode_decode;
use once_cell::sync::Lazy;
use proptest::prelude::*;
use regex::Regex;
use std::borrow::Borrow;

#[test]
fn valid_identifiers() {
    let valid_identifiers = [
        "foo",
        "FOO",
        "Foo",
        "foo0",
        "FOO_0",
        "_Foo1",
        "FOO2_",
        "foo_bar_baz",
        "_0",
        "__",
        "____________________",
    ];
    for identifier in &valid_identifiers {
        assert!(
            Identifier::is_valid(identifier),
            "Identifier '{}' should be valid",
            identifier
        );
    }
}

#[test]
fn invalid_identifiers() {
    let invalid_identifiers = [
        "",
        "_",
        "0",
        "01",
        "9876",
        "0foo",
        ":foo",
        "fo\\o",
        "fo/o",
        "foo.",
        "foo-bar",
        "foo\u{1f389}",
        ">>><<<",
        "foo::bar::<<>>",
        "foo!!bar!!<<>>",
        // <SELF> is no longer an exception.
        "<SELF>",
    ];
    for identifier in &invalid_identifiers {
        assert!(
            !Identifier::is_valid(identifier),
            "Identifier '{}' should be invalid",
            identifier
        );
    }
}

#[test]
fn invalid_identifier_deser() {
    let invalid_identifiers = [
        "",
        "_",
        "0",
        "01",
        "9876",
        "0foo",
        ":foo",
        "fo\\o",
        "fo/o",
        "foo.",
        "foo-bar",
        "foo\u{1f389}",
        ">>><<<",
        "foo::bar::<<>>",
        "foo!!bar!!<<>>",
        // <SELF> is no longer an exception.
        "<SELF>",
    ];
    for identifier in &invalid_identifiers {
        let bytes = bcs::to_bytes(identifier).unwrap();
        bcs::from_bytes::<Identifier>(&bytes).unwrap_err();
    }
}

proptest! {
    #[test]
    fn invalid_identifiers_proptest(identifier in invalid_identifier_strategy()) {
        // This effectively checks that if a string doesn't match the ALLOWED_IDENTIFIERS regex, it
        // will be rejected by the is_valid validator. Note that the converse is checked by the
        // Arbitrary impl for Identifier.
        prop_assert!(!Identifier::is_valid(identifier));
    }

    #[test]
    fn valid_identifiers_proptest(identifier in ALLOWED_IDENTIFIERS) {
        prop_assert!(Identifier::is_valid(identifier));
    }

    #[test]
    fn identifier_string_roundtrip(identifier in any::<Identifier>()) {
        let s = identifier.clone().into_string();
        let id2 = Identifier::new(s).expect("identifier should parse correctly");
        prop_assert_eq!(identifier, id2);
    }

    #[test]
    fn identifier_ident_str_equivalence(identifier in any::<Identifier>()) {
        let s = identifier.clone().into_string();
        let ident_str = IdentStr::new(&s).expect("identifier should parse correctly");
        prop_assert_eq!(ident_str, identifier.as_ident_str());
        prop_assert_eq!(ident_str, identifier.as_ref());
        prop_assert_eq!(ident_str, identifier.borrow());
        prop_assert_eq!(ident_str.to_owned(), identifier);
    }

    #[test]
    fn serde_json_roundtrip(identifier in any::<Identifier>()) {
        let ser = serde_json::to_string(&identifier).expect("should serialize correctly");
        let id2 = serde_json::from_str(&ser).expect("should deserialize correctly");
        prop_assert_eq!(identifier, id2);
    }

    #[test]
    fn identifier_canonical_serialization(identifier in any::<Identifier>()) {
        assert_canonical_encode_decode(identifier);
    }
}

fn invalid_identifier_strategy() -> impl Strategy<Value = String> {
    static ALLOWED_IDENTIFIERS_REGEX: Lazy<Regex> = Lazy::new(|| {
        // Need to add anchors to ensure the entire string is matched.
        Regex::new(&format!("^(?:{})$", ALLOWED_IDENTIFIERS)).unwrap()
    });

    ".*".prop_filter("Valid identifiers should not be generated", |s| {
        // Most strings won't match the regex above, so local rejects are OK.
        !ALLOWED_IDENTIFIERS_REGEX.is_match(s)
    })
}

/// Ensure that Identifier instances serialize into strings directly, with no wrapper.
#[test]
fn serde_serialize_no_wrapper() {
    let foobar = Identifier::new("foobar").expect("should parse correctly");
    let s = serde_json::to_string(&foobar).expect("Identifier should serialize correctly");
    assert_eq!(s, "\"foobar\"");
}

/// Ensure abstract size is properly computed
#[test]
fn test_abstract_size() {
    let foobar = Identifier::new("foobar").expect("should parse correctly");

    // Size should be 6 chars * 1 byte/char + = 6 bytes.
    let expected = AbstractMemorySize::new(6);
    let actual = foobar.abstract_size_for_gas_metering();
    assert_eq!(expected, actual);
}
