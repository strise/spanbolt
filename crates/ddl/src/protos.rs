//! The types a proto bundle's `FileDescriptorSet` declares.

use std::collections::{BTreeMap, BTreeSet};

use prost::Message;
use prost_types::{DescriptorProto, EnumDescriptorProto, FileDescriptorSet};

/// What a full name in a descriptor set is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtoKind {
    Message,
    Enum,
}

/// A message or enum of a descriptor set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtoType {
    pub kind: ProtoKind,
    /// The types it needs, as Cloud Spanner checks them when types leave a proto bundle: the
    /// message it is nested in, and its fields' types other than its own nested types.
    pub references: BTreeSet<String>,
    /// An enum's numbers, sorted.
    pub numbers: Vec<i32>,
}

/// Every message and enum `descriptors` declares (nested ones too), by full name. `None` when
/// the bytes are not a `FileDescriptorSet`.
pub fn proto_types(descriptors: &[u8]) -> Option<BTreeMap<String, ProtoType>> {
    let set = FileDescriptorSet::decode(descriptors).ok()?;
    let mut out = BTreeMap::new();
    for file in &set.file {
        let prefix = file.package.clone().unwrap_or_default();
        for e in &file.enum_type {
            out.insert(join(&prefix, e.name()), enum_type(None, e));
        }
        for m in &file.message_type {
            add_message(&mut out, &prefix, None, m);
        }
    }
    Some(out)
}

/// [`proto_types`] without the references.
pub fn proto_kinds(descriptors: &[u8]) -> Option<BTreeMap<String, ProtoKind>> {
    Some(
        proto_types(descriptors)?
            .into_iter()
            .map(|(k, t)| (k, t.kind))
            .collect(),
    )
}

fn enum_type(parent: Option<&str>, e: &EnumDescriptorProto) -> ProtoType {
    let mut numbers: Vec<i32> = e.value.iter().map(|v| v.number()).collect();
    numbers.sort_unstable();
    numbers.dedup();
    ProtoType {
        kind: ProtoKind::Enum,
        references: parent.into_iter().map(str::to_string).collect(),
        numbers,
    }
}

fn add_message(
    out: &mut BTreeMap<String, ProtoType>,
    prefix: &str,
    parent: Option<&str>,
    m: &DescriptorProto,
) {
    let name = join(prefix, m.name());
    for e in &m.enum_type {
        out.insert(join(&name, e.name()), enum_type(Some(&name), e));
    }
    for nested in &m.nested_type {
        add_message(out, &name, Some(&name), nested);
    }
    let own = format!("{name}.");
    let mut references: BTreeSet<String> = parent.into_iter().map(str::to_string).collect();
    for f in &m.field {
        if let Some(t) = f.type_name.as_deref() {
            let t = t.strip_prefix('.').unwrap_or(t);
            if !t.starts_with(&own) && t != name {
                references.insert(t.to_string());
            }
        }
    }
    out.insert(
        name,
        ProtoType {
            kind: ProtoKind::Message,
            references,
            numbers: Vec::new(),
        },
    );
}

/// The descriptor set Cloud Spanner keeps for a bundle of `types`: one file per package, named
/// `package:<package>`, holding the top-level messages and enums of `descriptors` that are, or
/// contain, a bundle type, in file order. No syntax, imports or options. `None` when the bytes
/// are not a `FileDescriptorSet`.
pub fn production_set<K>(descriptors: &[u8], types: &BTreeMap<String, K>) -> Option<Vec<u8>> {
    use prost_types::FileDescriptorProto;
    let set = FileDescriptorSet::decode(descriptors).ok()?;
    let wanted = |name: String| {
        let nested = format!("{name}.");
        types.contains_key(&name)
            || types
                .range(nested.clone()..)
                .next()
                .is_some_and(|(t, _)| t.starts_with(&nested))
    };
    let mut files: BTreeMap<String, FileDescriptorProto> = BTreeMap::new();
    for file in set.file {
        let package = file.package.clone().unwrap_or_default();
        let messages: Vec<_> = file
            .message_type
            .into_iter()
            .filter(|m| wanted(join(&package, m.name())))
            .collect();
        let enums: Vec<_> = file
            .enum_type
            .into_iter()
            .filter(|e| types.contains_key(&join(&package, e.name())))
            .collect();
        if messages.is_empty() && enums.is_empty() {
            continue;
        }
        let out = files
            .entry(package.clone())
            .or_insert_with(|| FileDescriptorProto {
                name: Some(format!("package:{package}")),
                package: Some(package),
                ..Default::default()
            });
        out.message_type.extend(messages);
        out.enum_type.extend(enums);
    }
    Some(
        FileDescriptorSet {
            file: files.into_values().collect(),
        }
        .encode_to_vec(),
    )
}

fn join(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix}.{name}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost_types::{EnumValueDescriptorProto, FieldDescriptorProto, FileDescriptorProto};

    #[test]
    fn lists_nested_messages_and_enums() {
        let set = FileDescriptorSet {
            file: vec![FileDescriptorProto {
                package: Some("a.b".into()),
                message_type: vec![DescriptorProto {
                    name: Some("M".into()),
                    nested_type: vec![DescriptorProto {
                        name: Some("N".into()),
                        ..Default::default()
                    }],
                    enum_type: vec![EnumDescriptorProto {
                        name: Some("E".into()),
                        ..Default::default()
                    }],
                    ..Default::default()
                }],
                enum_type: vec![EnumDescriptorProto {
                    name: Some("Top".into()),
                    ..Default::default()
                }],
                ..Default::default()
            }],
        };
        let kinds = proto_kinds(&set.encode_to_vec()).expect("decodes");
        let got: Vec<_> = kinds.iter().map(|(k, v)| (k.as_str(), *v)).collect();
        assert_eq!(
            got,
            [
                ("a.b.M", ProtoKind::Message),
                ("a.b.M.E", ProtoKind::Enum),
                ("a.b.M.N", ProtoKind::Message),
                ("a.b.Top", ProtoKind::Enum),
            ]
        );
        assert!(proto_kinds(&[1, 2, 3, 4]).is_none());
    }

    #[test]
    fn references_and_enum_numbers() {
        let field = |t: &str| FieldDescriptorProto {
            type_name: Some(t.into()),
            ..Default::default()
        };
        let value = |n| EnumValueDescriptorProto {
            number: Some(n),
            ..Default::default()
        };
        let set = FileDescriptorSet {
            file: vec![FileDescriptorProto {
                package: Some("p".into()),
                message_type: vec![DescriptorProto {
                    name: Some("M".into()),
                    field: vec![field(".p.M.N"), field(".p.E"), field(".p.M")],
                    nested_type: vec![DescriptorProto {
                        name: Some("N".into()),
                        ..Default::default()
                    }],
                    ..Default::default()
                }],
                enum_type: vec![EnumDescriptorProto {
                    name: Some("E".into()),
                    value: vec![value(3), value(-1), value(3)],
                    ..Default::default()
                }],
                ..Default::default()
            }],
        };
        let types = proto_types(&set.encode_to_vec()).expect("decodes");
        // Its own nested type and itself aren't references; a nested type refers to its parent.
        assert_eq!(types["p.M"].references, BTreeSet::from(["p.E".to_string()]));
        assert_eq!(
            types["p.M.N"].references,
            BTreeSet::from(["p.M".to_string()])
        );
        assert_eq!(types["p.E"].numbers, [-1, 3]);
    }
}
