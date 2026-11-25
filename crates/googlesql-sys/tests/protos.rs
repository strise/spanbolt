//! Proto bundle types: PROTO and ENUM columns, and the bundle's names in SQL.

use std::sync::Arc;

use googlesql_sys::{Catalog, Protos, Table};
use prost::Message;
use prost_types::field_descriptor_proto::{Label, Type};
use prost_types::{
    DescriptorProto, EnumDescriptorProto, EnumValueDescriptorProto, FieldDescriptorProto,
    FileDescriptorProto, FileDescriptorSet,
};

const FEATURES: &[&str] = &["FEATURE_PROTO_BASE", "FEATURE_REPLACE_FIELDS"];

fn field(name: &str, number: i32, ty: Type, type_name: Option<&str>) -> FieldDescriptorProto {
    FieldDescriptorProto {
        name: Some(name.into()),
        number: Some(number),
        label: Some(Label::Optional as i32),
        r#type: Some(ty as i32),
        type_name: type_name.map(Into::into),
        ..Default::default()
    }
}

/// `examples.music`: SingerInfo (with Nested and a Timestamp field whose import the set leaves
/// out) and Genre.
fn music() -> Vec<u8> {
    let info = DescriptorProto {
        name: Some("SingerInfo".into()),
        field: vec![
            field("singer_id", 1, Type::Int64, None),
            field("genre", 2, Type::Enum, Some(".examples.music.Genre")),
            field(
                "nested",
                3,
                Type::Message,
                Some(".examples.music.SingerInfo.Nested"),
            ),
            field("born", 4, Type::Message, Some(".google.protobuf.Timestamp")),
        ],
        nested_type: vec![DescriptorProto {
            name: Some("Nested".into()),
            field: vec![field("x", 1, Type::Int64, None)],
            ..Default::default()
        }],
        ..Default::default()
    };
    let genre = EnumDescriptorProto {
        name: Some("Genre".into()),
        value: ["POP", "JAZZ", "ROCK"]
            .iter()
            .zip(0..)
            .map(|(n, i)| EnumValueDescriptorProto {
                name: Some((*n).into()),
                number: Some(i),
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    };
    FileDescriptorSet {
        file: vec![FileDescriptorProto {
            name: Some("music.proto".into()),
            package: Some("examples.music".into()),
            dependency: vec!["google/protobuf/timestamp.proto".into()],
            message_type: vec![info],
            enum_type: vec![genre],
            syntax: Some("proto2".into()),
            ..Default::default()
        }],
    }
    .encode_to_vec()
}

fn bundle() -> Arc<Protos> {
    Arc::new(
        Protos::new(
            &music(),
            &["examples.music.SingerInfo", "examples.music.Genre"],
        )
        .expect("bundle"),
    )
}

fn catalog(protos: &Arc<Protos>) -> Catalog {
    let table = Table::with_protos(
        r#"{"name":"Singers","columns":[
            {"name":"Id","type":"INT64"},
            {"name":"Info","type":"PROTO<examples.music.SingerInfo>"},
            {"name":"G","type":"ENUM<examples.music.Genre>"},
            {"name":"Infos","type":"ARRAY<PROTO<examples.music.SingerInfo>>"},
            {"name":"Gs","type":"ARRAY<ENUM<examples.music.Genre>>"}],
            "primary_key":[0]}"#,
        protos,
    )
    .expect("table");
    Catalog::from_tables_with_protos(vec![Arc::new(table)], FEATURES, Some(protos.clone()))
        .expect("catalog")
}

#[test]
fn bundle_types_resolve_in_sql() {
    let protos = bundle();
    let cat = catalog(&protos);
    for sql in [
        "SELECT Info.singer_id, Info.genre, Info.nested.x, Info.born, G FROM Singers WHERE G = 'ROCK'",
        "SELECT i.singer_id, g FROM Singers, UNNEST(Infos) AS i, UNNEST(Gs) AS g",
        "SELECT CAST('JAZZ' AS examples.music.Genre), CAST(b'\\x08\\x05' AS examples.music.SingerInfo)",
        "SELECT NEW examples.music.SingerInfo(5 AS singer_id, 'JAZZ' AS genre)",
        "SELECT REPLACE_FIELDS(Info, 9 AS singer_id) FROM Singers",
        "SELECT ARRAY<examples.music.Genre>['JAZZ', 'ROCK']",
        "SELECT Info.has_singer_id FROM Singers",
    ] {
        cat.analyze(sql, "")
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    let params = r#"{"p":"PROTO<examples.music.SingerInfo>","g":"ENUM<examples.music.Genre>","gs":"ARRAY<ENUM<examples.music.Genre>>"}"#;
    cat.analyze(
        "SELECT @p.singer_id, @g, @gs FROM Singers WHERE G = @g",
        params,
    )
    .expect("parameters");
}

#[test]
fn errors_name_the_bundle_types() {
    let protos = bundle();
    let cat = catalog(&protos);
    let err = cat
        .analyze("SELECT Id FROM Singers WHERE Info = Info", "")
        .expect_err("proto equality");
    assert_eq!(
        err.message,
        "Equality is not defined for arguments of type PROTO<examples.music.SingerInfo> [at 1:30]"
    );
    // Nested is a type of the descriptors but not of the bundle.
    let err = cat
        .analyze("SELECT CAST(b'' AS examples.music.SingerInfo.Nested)", "")
        .expect_err("type outside the bundle");
    assert!(
        err.message.contains("examples.music.SingerInfo.Nested"),
        "{}",
        err.message
    );
}

#[test]
fn bad_bundles_are_errors() {
    let err = Protos::new(&music(), &["examples.music.Nope"])
        .err()
        .expect("unknown type");
    assert!(
        err.message.contains("examples.music.Nope"),
        "{}",
        err.message
    );
    assert!(Protos::new(b"\xff", &[]).is_err());

    let protos = bundle();
    // A PROTO column of an enum, and an enum not in the bundle.
    for ty in ["PROTO<examples.music.Genre>", "ENUM<examples.music.Other>"] {
        let json = format!(r#"{{"name":"T","columns":[{{"name":"c","type":"{ty}"}}]}}"#);
        assert!(Table::with_protos(&json, &protos).is_err(), "{ty}");
    }
    // Bundle types need the bundle.
    assert!(
        Table::new(
            r#"{"name":"T","columns":[{"name":"c","type":"PROTO<examples.music.SingerInfo>"}]}"#
        )
        .is_err()
    );
}

#[test]
fn tables_keep_the_bundle_alive() {
    let protos = bundle();
    let cat = catalog(&protos);
    drop(protos);
    cat.analyze("SELECT Info.nested.x FROM Singers", "")
        .expect("after drop");
}
