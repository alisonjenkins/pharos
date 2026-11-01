//! T011: metric labels are a dashboard contract. Every label is a bounded,
//! stable `[a-z_]+` string, and no two mean the same thing.

use std::collections::HashSet;

use capnp::introspect::{Introspect, TypeVariant};
use capnp::schema::StructSchema;
use pharos_api::labels::Outcome;
use pharos_api::pharos_capnp::error;
use pharos_api::server::INTERFACES;

fn is_label(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
}

fn assert_distinct<'a>(what: &str, labels: impl IntoIterator<Item = &'a str>) {
    let mut seen = HashSet::new();
    for l in labels {
        assert!(is_label(l), "{what} label {l:?} is not [a-z_]+");
        assert!(seen.insert(l), "{what} label {l:?} appears twice");
    }
}

#[test]
fn outcome_labels_are_distinct() {
    assert_distinct("outcome", Outcome::ALL.iter().map(|o| o.label()));
}

#[test]
fn interface_and_method_labels_are_distinct() {
    assert_distinct("interface", INTERFACES.iter().map(|i| i.label));
    let mut pairs = HashSet::new();
    for i in INTERFACES {
        assert_distinct(i.label, i.methods.iter().map(|m| m.method));
        for m in i.methods {
            assert_eq!(m.interface, i.label);
            assert_eq!(m.span, format!("rpc.{}.{}", m.interface, m.method));
            assert!(pairs.insert((m.interface, m.method)));
        }
    }
    for expected in [
        "bootstrap",
        "session",
        "library",
        "user_data",
        "playback_grant",
        "events",
    ] {
        assert!(
            INTERFACES.iter().any(|i| i.label == expected),
            "generated table lacks {expected}"
        );
    }
}

fn struct_schema(ty: capnp::introspect::Type) -> StructSchema {
    match ty.which() {
        TypeVariant::Struct(raw) => StructSchema::new(raw),
        other => panic!("not a struct: {other:?}"),
    }
}

/// A variant appended to `Error.kind` must get its own outcome label in the
/// same change, or its failures would all read `other`.
#[test]
fn every_error_kind_variant_has_an_outcome() {
    let error = struct_schema(<error::Owned as Introspect>::introspect());
    let kind = error.get_field_by_name("kind").unwrap();
    let kind = struct_schema(kind.get_type());
    let variants = kind.get_union_fields().unwrap();
    assert_ne!(variants.len(), 0);
    for v in variants {
        let name = v.get_proto().get_name().unwrap().to_str().unwrap();
        let outcome = Outcome::from_error_kind(name)
            .unwrap_or_else(|| panic!("Error.kind.{name} has no Outcome"));
        if name != "other" {
            assert_ne!(
                outcome,
                Outcome::Other,
                "Error.kind.{name} collapses into `other`"
            );
        }
    }
}
