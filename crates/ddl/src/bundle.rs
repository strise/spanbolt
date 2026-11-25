//! `CREATE/ALTER/DROP PROTO BUNDLE`, and the proto and enum column types it allows.
//!
//! Production and the emulator check different things and word every error differently; `compat`
//! picks the emulator's (`--emulator-compat`).

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::DdlError;
use crate::protos::{ProtoKind, ProtoType, proto_types};
use crate::schema::*;

type R<T> = Result<T, DdlError>;

fn no_bundle(verb: &str, compat: bool) -> DdlError {
    let m = format!("Proto bundle does not yet exist; cannot {verb} it");
    if compat {
        DdlError::Precondition(m)
    } else {
        DdlError::Syntax(m)
    }
}

/// The request's descriptors, decoded.
fn decode(descriptors: Option<&Arc<Vec<u8>>>, compat: bool) -> R<BTreeMap<String, ProtoType>> {
    let bytes = descriptors.map(|d| d.as_slice()).unwrap_or_default();
    if compat && bytes.is_empty() {
        return Err(DdlError::Syntax(
            "Contents of proto_descriptor_file cannot be empty".into(),
        ));
    }
    proto_types(bytes).ok_or_else(|| {
        DdlError::Syntax(if compat {
            "GOOGLESQL_RET_CHECK failure (backend/schema/catalog/proto_bundle.cc:58) \
             provided_descriptor_set.ParseFromString(proto_descriptor_bytes) Could not parse \
             contents of proto_descriptor_file"
                .into()
        } else {
            "Failed to deserialize proto descriptors into google.protobuf.FileDescriptorSet. Error:"
                .into()
        })
    })
}

fn missing_descriptor(t: &str, compat: bool) -> DdlError {
    DdlError::NotFound(if compat {
        format!("Missing descriptor for `{t}`")
    } else {
        format!(
            "Type not found in proto descriptors. {t}. Did you miss including the proto file in \
             your protoc command?"
        )
    })
}

fn already_in_bundle(t: &str, compat: bool) -> DdlError {
    DdlError::Exists(if compat {
        format!(
            "GOOGLESQL_RET_CHECK failure (backend/schema/catalog/proto_bundle.cc:92) ret.second \
             Type already exists: `{t}` the proto bundle"
        )
    } else {
        format!("Type already exists: {t}")
    })
}

/// The kind of each of `types`, from the request's descriptors.
fn look_up(
    types: &[String],
    found: &BTreeMap<String, ProtoType>,
    into: &mut BTreeMap<String, ProtoKind>,
    compat: bool,
) -> R<()> {
    for t in types {
        let info = found.get(t).ok_or_else(|| missing_descriptor(t, compat))?;
        if into.insert(t.clone(), info.kind).is_some() {
            return Err(already_in_bundle(t, compat));
        }
    }
    Ok(())
}

/// The numbers of the enums among `kinds`: from `found` (the request's descriptors), else `old`.
fn enum_numbers(
    kinds: &BTreeMap<String, ProtoKind>,
    found: &BTreeMap<String, ProtoType>,
    old: &BTreeMap<String, Arc<[i32]>>,
) -> BTreeMap<String, Arc<[i32]>> {
    kinds
        .iter()
        .filter(|(_, k)| **k == ProtoKind::Enum)
        .filter_map(|(t, _)| {
            let numbers = match found.get(t) {
                Some(info) => Arc::from(info.numbers.as_slice()),
                None => old.get(t)?.clone(),
            };
            Some((t.clone(), numbers))
        })
        .collect()
}

/// The proto or enum a column type names, if any (arrays included).
fn proto_name(ty: &ColumnType) -> Option<&str> {
    match ty {
        ColumnType::Proto(n) | ColumnType::Enum(n) => Some(n),
        ColumnType::Array(inner) => proto_name(inner),
        _ => None,
    }
}

impl Schema {
    pub(crate) fn proto_bundle_stmt(&mut self, stmt: &Statement, compat: bool) -> R<()> {
        match stmt {
            Statement::CreateProtoBundle { types, descriptors } => {
                if self.proto_bundle.is_some() && !compat {
                    return Err(DdlError::Syntax(
                        "Proto bundle already exists; cannot create it".into(),
                    ));
                }
                let found = decode(descriptors.as_ref(), compat)?;
                let mut kinds = BTreeMap::new();
                look_up(types, &found, &mut kinds, compat)?;
                // The emulator replaces an existing bundle.
                self.check_removed(&kinds, &BTreeMap::new(), compat)?;
                self.proto_bundle = Some(ProtoBundle {
                    enum_numbers: enum_numbers(&kinds, &found, &BTreeMap::new()),
                    types: kinds,
                    descriptors: descriptors.clone().unwrap_or_default(),
                });
            }
            Statement::AlterProtoBundle {
                change,
                descriptors,
            } => {
                let Some(old) = &self.proto_bundle else {
                    return Err(no_bundle("alter", compat));
                };
                let found = decode(descriptors.as_ref(), compat)?;
                let mut kinds = old.types.clone();
                look_up(&change.insert, &found, &mut kinds, compat)?;
                for t in &change.update {
                    let Some(kind) = kinds.get_mut(t) else {
                        return Err(DdlError::NotFound(if compat {
                            format!("Tried to update `{t}` which does not already exist.")
                        } else {
                            format!("Message type not found in proto bundle: {t}")
                        }));
                    };
                    *kind = found
                        .get(t)
                        .ok_or_else(|| missing_descriptor(t, compat))?
                        .kind;
                }
                for t in &change.delete {
                    if kinds.remove(t).is_none() {
                        return Err(DdlError::NotFound(if compat {
                            format!(
                                "GOOGLESQL_RET_CHECK failure (backend/schema/catalog/\
                                 proto_bundle.cc:114) instance_->types_.erase(t) == 1 (0 vs. 1) \
                                 Tried to remove `{t}` which does not already exist in the proto \
                                 bundle."
                            )
                        } else {
                            format!("Type not found in proto bundle: {t}")
                        }));
                    }
                }
                self.check_removed(&kinds, &found, compat)?;
                if let Some(b) = &mut self.proto_bundle {
                    // Inserted and updated enums take the new numbers, the others keep theirs.
                    let mut fresh = found;
                    fresh.retain(|t, _| change.insert.contains(t) || change.update.contains(t));
                    b.enum_numbers = enum_numbers(&kinds, &fresh, &b.enum_numbers);
                    b.types = kinds;
                    if let Some(d) = descriptors {
                        b.descriptors = d.clone();
                    }
                }
            }
            Statement::DropProtoBundle => {
                if self.proto_bundle.is_none() {
                    return Err(no_bundle("drop", compat));
                }
                self.check_removed(&BTreeMap::new(), &BTreeMap::new(), compat)?;
                self.proto_bundle = None;
            }
            _ => {}
        }
        Ok(())
    }

    /// Checks that the types leaving the bundle when it becomes `kept` are unused: no kept type
    /// refers to one (production only, from `found`, the request's descriptors) and no column
    /// has one.
    fn check_removed(
        &self,
        kept: &BTreeMap<String, ProtoKind>,
        found: &BTreeMap<String, ProtoType>,
        compat: bool,
    ) -> R<()> {
        let Some(old) = &self.proto_bundle else {
            return Ok(());
        };
        let removed = |t: &str| old.types.contains_key(t) && !kept.contains_key(t);
        if !compat {
            for (t, info) in kept.keys().filter_map(|t| Some((t, found.get(t)?))) {
                if let Some(r) = info.references.iter().find(|r| removed(r)) {
                    return Err(DdlError::NotFound(format!(
                        "Type {t} references type {r} which is to be removed from the proto bundle"
                    )));
                }
            }
        }
        let columns = self
            .tables
            .values()
            .flat_map(|t| t.columns.iter().map(move |c| (t, c)))
            .filter_map(|(t, c)| Some((t, c, proto_name(&c.ty)?)))
            .filter(|(_, _, n)| removed(n));
        if compat {
            if let Some((t, c, n)) = columns.into_iter().next() {
                return Err(DdlError::Precondition(format!(
                    "Removed type `{n}` is still being used by column {}.{}",
                    t.name, c.name
                )));
            }
            return Ok(());
        }
        let mut uses: BTreeMap<&str, usize> = BTreeMap::new();
        for (_, _, n) in columns {
            *uses.entry(n).or_default() += 1;
        }
        if let Some((n, count)) = uses.into_iter().next() {
            return Err(DdlError::Syntax(format!(
                "Cannot set new proto bundle: removed type {n} is still used by {count} columns"
            )));
        }
        Ok(())
    }

    /// `column`'s type with the bundle's enums made `Enum`, or the error for a proto or enum
    /// type the bundle doesn't have.
    pub(crate) fn resolve_proto_type(&self, column: &mut Column, compat: bool) -> R<()> {
        fn resolve(ty: &mut ColumnType, bundle: Option<&ProtoBundle>) -> Result<(), String> {
            match ty {
                ColumnType::Array(inner) => resolve(inner, bundle),
                ColumnType::Proto(name) | ColumnType::Enum(name) => {
                    let kind = bundle
                        .and_then(|b| b.types.get(name.as_str()))
                        .ok_or_else(|| name.clone())?;
                    *ty = match kind {
                        ProtoKind::Message => ColumnType::Proto(std::mem::take(name)),
                        ProtoKind::Enum => ColumnType::Enum(std::mem::take(name)),
                    };
                    Ok(())
                }
                _ => Ok(()),
            }
        }
        resolve(&mut column.ty, self.proto_bundle.as_ref()).map_err(|name| {
            if compat {
                DdlError::Internal(format!(
                    "GOOGLESQL_RET_CHECK failure (backend/schema/updater/ddl_type_conversion.cc:\
                     242) false Unrecognized ddl::ColumnDefinition: column_name: \"{}\" type: \
                     NONE proto_type_name: \"{name}\"",
                    column.name
                ))
            } else {
                DdlError::NotFound(format!("Type not found: {name}."))
            }
        })
    }

    /// `stmt` with its column types resolved by [`Schema::resolve_proto_type`], when it declares
    /// a proto or enum column.
    pub(crate) fn resolve_proto_columns(
        &self,
        stmt: &Statement,
        compat: bool,
    ) -> R<Option<Statement>> {
        let names_proto = |c: &Column| proto_name(&c.ty).is_some();
        let mut out = match stmt {
            Statement::CreateTable { table, .. } if table.columns.iter().any(names_proto) => {
                stmt.clone()
            }
            Statement::AlterTable {
                action: AlterTable::AddColumn { column, .. } | AlterTable::AlterColumn { column },
                ..
            } if names_proto(column) => stmt.clone(),
            _ => return Ok(None),
        };
        match &mut out {
            Statement::CreateTable { table, .. } => {
                for c in &mut table.columns {
                    self.resolve_proto_type(c, compat)?;
                }
            }
            Statement::AlterTable {
                action: AlterTable::AddColumn { column, .. } | AlterTable::AlterColumn { column },
                ..
            } => self.resolve_proto_type(column, compat)?,
            _ => {}
        }
        Ok(Some(out))
    }
}
