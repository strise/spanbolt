//! PROTO and ENUM expressions: field access, `NEW`, `REPLACE_FIELDS`, casts and `TO_JSON`.

use std::sync::Arc;

use googlesql_sys::proto::any_resolved_expr_proto::Node as E;
use googlesql_sys::proto::*;
use prost_reflect::{FieldDescriptor, MessageDescriptor};
use spanbolt_storage::{Code, Error, Result, Value};

use super::{Planner, Scope, type_of};
use crate::exec::{Cx, Env, Eval};
use crate::protos::{self, Bundle, Errors, FieldGetter};
use crate::types::{FILE_DESCRIPTOR_SET, Type, unimplemented};

/// A cast between a bundle type and another type.
type CastFn = Arc<dyn Fn(&Value) -> Result<Value> + Send + Sync>;

impl Planner<'_> {
    fn bundle(&self) -> Result<Arc<Bundle>> {
        self.cat
            .protos
            .clone()
            .ok_or_else(|| Error::internal("PROTO expression without a proto bundle"))
    }

    fn proto_errors(&self) -> Errors {
        Errors {
            emulator_compat: self.opts.emulator_compat,
        }
    }

    fn field_of(
        &self,
        bundle: &Bundle,
        f: Option<&FieldDescriptorRefProto>,
    ) -> Result<FieldDescriptor> {
        let f = f.ok_or_else(|| Error::internal("proto field without descriptor"))?;
        let message = f
            .containing_proto
            .as_ref()
            .and_then(|p| p.proto_name.as_deref())
            .ok_or_else(|| Error::internal("proto field without message"))?;
        not_proto_bundle(message)?;
        bundle.field(message, f.number.unwrap_or(0))
    }

    fn message(&self, name: &str) -> Result<MessageDescriptor> {
        not_proto_bundle(name)?;
        self.bundle()?.message(name)
    }

    /// `proto.field` and `proto.has_field`.
    pub(super) fn get_proto_field(
        &mut self,
        e: &AnyResolvedExprProto,
        g: &ResolvedGetProtoFieldProto,
        scope: &Scope,
    ) -> Result<Eval> {
        let bundle = self.bundle()?;
        let field = self.field_of(&bundle, g.field_descriptor.as_ref())?;
        let has_bit = g.get_has_bit == Some(true);
        let default = if has_bit {
            Value::Null
        } else {
            let ty = type_of(e)?;
            crate::literal::value(g.default_value.as_ref().and_then(|d| d.value.as_ref()), &ty)?
        };
        let inner = g
            .expr
            .as_deref()
            .ok_or_else(|| Error::internal("proto field without expr"))?;
        let x = self.expr(inner, scope)?;
        let getter = FieldGetter::new(field, has_bit, default, self.proto_errors());
        Ok(Arc::new(move |cx: &Cx, env: &Env| getter.get(&x(cx, env)?)))
    }

    /// `NEW m(v AS f, ...)`.
    pub(super) fn make_proto(
        &mut self,
        e: &AnyResolvedExprProto,
        m: &ResolvedMakeProtoProto,
        scope: &Scope,
    ) -> Result<Eval> {
        let bundle = self.bundle()?;
        let Type::Proto(name) = type_of(e)? else {
            return Err(Error::internal("NEW of a non-proto type"));
        };
        let message = self.message(&name)?;
        let mut fields = Vec::with_capacity(m.field_list.len());
        for f in &m.field_list {
            let fd = self.field_of(&bundle, f.field_descriptor.as_ref())?;
            let ex = f
                .expr
                .as_ref()
                .ok_or_else(|| Error::internal("NEW field without expr"))?;
            fields.push((fd, self.expr(ex, scope)?));
        }
        Ok(Arc::new(move |cx: &Cx, env: &Env| {
            let mut vals = Vec::with_capacity(fields.len());
            for (fd, x) in &fields {
                vals.push((fd.clone(), x(cx, env)?));
            }
            protos::make(&message, &vals)
        }))
    }

    /// `REPLACE_FIELDS(p, v AS path, ...)`.
    pub(super) fn replace_fields(
        &mut self,
        e: &AnyResolvedExprProto,
        r: &ResolvedReplaceFieldProto,
        scope: &Scope,
    ) -> Result<Eval> {
        let bundle = self.bundle()?;
        let Type::Proto(name) = type_of(e)? else {
            return Err(unimplemented("REPLACE_FIELDS on a STRUCT"));
        };
        let message = self.message(&name)?;
        let inner = r
            .expr
            .as_deref()
            .ok_or_else(|| Error::internal("REPLACE_FIELDS without expr"))?;
        let x = self.expr(inner, scope)?;
        let mut items = Vec::with_capacity(r.replace_field_item_list.len());
        for item in &r.replace_field_item_list {
            if !item.struct_index_path.is_empty() {
                return Err(unimplemented("REPLACE_FIELDS of a STRUCT field"));
            }
            let path = item
                .proto_field_path
                .iter()
                .map(|f| self.field_of(&bundle, Some(f)))
                .collect::<Result<Vec<_>>>()?;
            let ex = item
                .expr
                .as_ref()
                .ok_or_else(|| Error::internal("REPLACE_FIELDS item without expr"))?;
            items.push((path, self.expr(ex, scope)?));
        }
        Ok(Arc::new(move |cx: &Cx, env: &Env| {
            let proto = x(cx, env)?;
            if proto.is_null() {
                return Ok(Value::Null);
            }
            let mut vals = Vec::with_capacity(items.len());
            for (path, ex) in &items {
                vals.push((path.clone(), ex(cx, env)?));
            }
            protos::replace(&message, &proto, &vals)
        }))
    }

    /// `UPDATE t SET proto_col.field = v`: the column's id and its new value.
    pub(crate) fn proto_field_update(
        &mut self,
        target: &AnyResolvedExprProto,
        value: Eval,
        scope: &Scope,
    ) -> Result<(i64, Eval)> {
        let Some(E::ResolvedGetProtoFieldNode(g)) = target.node.as_ref() else {
            return Err(unimplemented("UPDATE of a nested field"));
        };
        let column = g
            .expr
            .as_deref()
            .ok_or_else(|| Error::internal("proto field without expr"))?;
        let Some(E::ResolvedColumnRefNode(c)) = column.node.as_ref() else {
            return Err(unimplemented("UPDATE of a field of a nested proto"));
        };
        let id = c.column.as_ref().and_then(|c| c.column_id).unwrap_or(-1);
        let bundle = self.bundle()?;
        let field = self.field_of(&bundle, g.field_descriptor.as_ref())?;
        let Type::Proto(name) = type_of(column)? else {
            return Err(Error::internal("proto field of a non-proto"));
        };
        let message = self.message(&name)?;
        let current = self.expr(column, scope)?;
        let path = vec![field];
        let ev: Eval = Arc::new(move |cx: &Cx, env: &Env| {
            let proto = current(cx, env)?;
            if proto.is_null() {
                return Err(unimplemented("UPDATE of a field of a NULL proto"));
            }
            protos::replace(&message, &proto, &[(path.clone(), value(cx, env)?)])
        });
        Ok((id, ev))
    }

    /// A cast that involves a bundle type, or `None` when neither side is one.
    pub(super) fn proto_cast(&self, from: &Type, to: &Type) -> Result<Option<CastFn>> {
        if !has_bundle_type(from) && !has_bundle_type(to) {
            return Ok(None);
        }
        let errors = self.proto_errors();
        let identity: CastFn = Arc::new(|v: &Value| Ok(v.clone()));
        Ok(Some(match (from, to) {
            (Type::Proto(a), Type::Proto(b)) | (Type::Enum(a), Type::Enum(b)) if a == b => identity,
            (Type::Proto(_), Type::Bytes(_)) | (Type::Bytes(_), Type::Proto(_)) => identity,
            (Type::Enum(_), Type::Int64) => identity,
            (Type::Proto(m), Type::String(_)) => {
                let m = self.message(m)?;
                Arc::new(move |v: &Value| protos::to_text(&m, v, errors))
            }
            (Type::String(_), Type::Proto(m)) => {
                let m = self.message(m)?;
                Arc::new(move |v: &Value| protos::from_text(&m, v, errors))
            }
            (Type::Enum(e), Type::String(_)) => {
                let e = self.bundle()?.enumeration(e)?;
                Arc::new(move |v: &Value| protos::enum_name(&e, v, errors))
            }
            (Type::String(_), Type::Enum(e)) => {
                let e = self.bundle()?.enumeration(e)?;
                Arc::new(move |v: &Value| protos::enum_from_name(&e, v, errors))
            }
            (Type::Int64, Type::Enum(e)) => {
                let e = self.bundle()?.enumeration(e)?;
                Arc::new(move |v: &Value| protos::enum_from_number(&e, v, errors))
            }
            _ => return Err(unimplemented(format!("CAST from {from:?} to {to:?}"))),
        }))
    }

    /// `TO_JSON` of a PROTO or ENUM, or `None` for other arguments.
    pub(super) fn proto_to_json(
        &self,
        name: &str,
        args: &[Eval],
        types: &[Type],
    ) -> Result<Option<Eval>> {
        if !matches!(name, "to_json" | "safe_to_json") {
            return Ok(None);
        }
        let Some(t) = types.first() else {
            return Ok(None);
        };
        if !has_bundle_type(t) {
            return Ok(None);
        }
        let x = args[0].clone();
        let errors = self.proto_errors();
        Ok(Some(match t {
            Type::Enum(e) => {
                let e = self.bundle()?.enumeration(e)?;
                Arc::new(move |cx: &Cx, env: &Env| {
                    let v = x(cx, env)?;
                    if v.is_null() {
                        return Ok(Value::Json(Arc::from("null")));
                    }
                    let Value::String(s) = protos::enum_name(&e, &v, errors)? else {
                        return Err(Error::internal("enum name"));
                    };
                    crate::func_json::from_serde(&serde_json::Value::String(s.to_string()))
                })
            }
            Type::Proto(m) if !self.opts.emulator_compat => {
                let m = self.message(m)?;
                Arc::new(
                    move |cx: &Cx, env: &Env| match protos::to_json(&m, &x(cx, env)?)? {
                        Some(j) => crate::func_json::from_serde(&j),
                        None => Ok(Value::Json(Arc::from("null"))),
                    },
                )
            }
            Type::Proto(m) => {
                return Err(Error::new(
                    Code::Unimplemented,
                    format!("Unsupported argument type {m} for TO_JSON"),
                ));
            }
            _ => {
                return Err(unimplemented(
                    "TO_JSON of an array or struct of PROTO or ENUM values",
                ));
            }
        }))
    }
}

/// A PROTO or ENUM type, also inside an array or struct.
pub(super) fn has_bundle_type(t: &Type) -> bool {
    match t {
        Type::Proto(_) | Type::Enum(_) => true,
        Type::Array(e) => has_bundle_type(e),
        Type::Struct(fields) => fields.iter().any(|f| has_bundle_type(&f.1)),
        _ => false,
    }
}

/// `INFORMATION_SCHEMA.SCHEMATA.PROTO_BUNDLE` can be scanned, returned and compared with NULL,
/// nothing else.
fn not_proto_bundle(message: &str) -> Result<()> {
    if message == FILE_DESCRIPTOR_SET || message.starts_with("google.protobuf.FileDescriptor") {
        return Err(unimplemented("Using a PROTO_BUNDLE value"));
    }
    Ok(())
}
