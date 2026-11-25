//! Decodes an `ExecuteSql` response nested deeper than prost allows.
//!
//! prost stops at 100 nested messages. A STRUCT level of a result type is three of them
//! (`Type`, `StructType`, `Field`) and a level of a value two (`Value`, `ListValue`), so the
//! generated `ResultSet` failed on types 32 STRUCTs deep and values 50 deep, which both the emulator
//! and Cloud Spanner return. These mirrors of the messages a result nests through decode the
//! recursive fields (`Field.type`, `ListValue.values`) with a fresh `DecodeContext`, count their own
//! depth instead, and convert into the generated types. The same fix #161 made in the server's
//! GoogleSQL decoder.

use std::cell::Cell;

use prost::bytes::{Buf, BufMut};
use prost::encoding::{DecodeContext, WireType, message, skip_field, string};
use spanbolt_proto::spanner as pb;

/// The deepest type or value decoded. The emulator answers about 5000 nested STRUCT types, spanbolt
/// 2000 and Cloud Spanner about 1200.
pub const MAX: u32 = 10_000;
/// Stack for the runtime threads that decode: one level of a type or value takes a few hundred
/// bytes (address space, used only as deep as a response goes).
pub const STACK: usize = 128 << 20;

thread_local! {
    static DEPTH: Cell<u32> = const { Cell::new(0) };
}

/// One nested type or list, for as long as it decodes.
struct Level;

impl Level {
    fn enter() -> Result<Level, prost::DecodeError> {
        let depth = DEPTH.get() + 1;
        if depth > MAX {
            #[allow(deprecated)]
            return Err(prost::DecodeError::new("nested too deep"));
        }
        DEPTH.set(depth);
        Ok(Level)
    }
}

impl Drop for Level {
    fn drop(&mut self) {
        DEPTH.set(DEPTH.get() - 1);
    }
}

/// `google.spanner.v1.ResultSet`.
#[derive(Clone, PartialEq, prost::Message)]
pub struct ResultSet {
    #[prost(message, optional, tag = "1")]
    pub metadata: Option<ResultSetMetadata>,
    #[prost(message, repeated, tag = "2")]
    pub rows: Vec<ListValue>,
    #[prost(message, optional, tag = "3")]
    pub stats: Option<pb::ResultSetStats>,
    #[prost(message, optional, tag = "5")]
    pub precommit_token: Option<pb::MultiplexedSessionPrecommitToken>,
    #[prost(message, optional, tag = "6")]
    pub cache_update: Option<pb::CacheUpdate>,
}

/// `google.spanner.v1.ResultSetMetadata`.
#[derive(Clone, PartialEq, prost::Message)]
pub struct ResultSetMetadata {
    #[prost(message, optional, tag = "1")]
    pub row_type: Option<StructType>,
    #[prost(message, optional, tag = "2")]
    pub transaction: Option<pb::Transaction>,
    #[prost(message, optional, tag = "3")]
    pub undeclared_parameters: Option<StructType>,
}

/// `google.spanner.v1.StructType`.
#[derive(Clone, PartialEq, prost::Message)]
pub struct StructType {
    #[prost(message, repeated, tag = "1")]
    pub fields: Vec<Field>,
}

/// `google.spanner.v1.Type`.
#[derive(Clone, PartialEq, prost::Message)]
pub struct Type {
    #[prost(int32, tag = "1")]
    pub code: i32,
    #[prost(message, optional, boxed, tag = "2")]
    pub array_element_type: Option<Box<Type>>,
    #[prost(message, optional, tag = "3")]
    pub struct_type: Option<StructType>,
    #[prost(int32, tag = "4")]
    pub type_annotation: i32,
    #[prost(string, tag = "5")]
    pub proto_type_fqn: String,
}

/// `google.spanner.v1.StructType.Field`: its type decodes with a fresh `DecodeContext`.
#[derive(Clone, PartialEq, Debug, Default)]
pub struct Field {
    pub name: String,
    pub r#type: Option<Type>,
}

impl prost::Message for Field {
    fn encode_raw(&self, buf: &mut impl BufMut) {
        if !self.name.is_empty() {
            string::encode(1, &self.name, buf);
        }
        if let Some(ty) = &self.r#type {
            message::encode(2, ty, buf);
        }
    }
    fn merge_field(
        &mut self,
        tag: u32,
        wire_type: WireType,
        buf: &mut impl Buf,
        ctx: DecodeContext,
    ) -> Result<(), prost::DecodeError> {
        match tag {
            1 => string::merge(wire_type, &mut self.name, buf, ctx),
            2 => {
                let _level = Level::enter()?;
                let ty = self.r#type.get_or_insert_with(Default::default);
                message::merge(wire_type, ty, buf, DecodeContext::default())
            }
            _ => skip_field(wire_type, tag, buf, ctx),
        }
    }
    fn encoded_len(&self) -> usize {
        (if self.name.is_empty() {
            0
        } else {
            string::encoded_len(1, &self.name)
        }) + self
            .r#type
            .as_ref()
            .map_or(0, |ty| message::encoded_len(2, ty))
    }
    fn clear(&mut self) {
        *self = Self::default();
    }
}

/// `google.protobuf.Value`, without `struct_value` (Spanner never sends one).
#[derive(Clone, PartialEq, prost::Message)]
pub struct Value {
    #[prost(oneof = "Kind", tags = "1, 2, 3, 4, 6")]
    pub kind: Option<Kind>,
}

#[derive(Clone, PartialEq, prost::Oneof)]
pub enum Kind {
    #[prost(enumeration = "prost_types::NullValue", tag = "1")]
    Null(i32),
    #[prost(double, tag = "2")]
    Number(f64),
    #[prost(string, tag = "3")]
    String(String),
    #[prost(bool, tag = "4")]
    Bool(bool),
    #[prost(message, tag = "6")]
    List(ListValue),
}

/// `google.protobuf.ListValue`: its values decode with a fresh `DecodeContext`.
#[derive(Clone, PartialEq, Debug, Default)]
pub struct ListValue {
    pub values: Vec<Value>,
}

impl prost::Message for ListValue {
    fn encode_raw(&self, buf: &mut impl BufMut) {
        for v in &self.values {
            message::encode(1, v, buf);
        }
    }
    fn merge_field(
        &mut self,
        tag: u32,
        wire_type: WireType,
        buf: &mut impl Buf,
        ctx: DecodeContext,
    ) -> Result<(), prost::DecodeError> {
        match tag {
            1 => {
                let _level = Level::enter()?;
                message::merge_repeated(wire_type, &mut self.values, buf, DecodeContext::default())
            }
            _ => skip_field(wire_type, tag, buf, ctx),
        }
    }
    fn encoded_len(&self) -> usize {
        message::encoded_len_repeated(1, &self.values)
    }
    fn clear(&mut self) {
        self.values.clear();
    }
}

impl From<ResultSet> for pb::ResultSet {
    fn from(rs: ResultSet) -> Self {
        pb::ResultSet {
            metadata: rs.metadata.map(|m| pb::ResultSetMetadata {
                row_type: m.row_type.map(Into::into),
                transaction: m.transaction,
                undeclared_parameters: m.undeclared_parameters.map(Into::into),
            }),
            rows: rs.rows.into_iter().map(Into::into).collect(),
            stats: rs.stats,
            precommit_token: rs.precommit_token,
            cache_update: rs.cache_update,
        }
    }
}

impl From<StructType> for pb::StructType {
    fn from(s: StructType) -> Self {
        pb::StructType {
            fields: s
                .fields
                .into_iter()
                .map(|f| pb::struct_type::Field {
                    name: f.name,
                    r#type: f.r#type.map(Into::into),
                })
                .collect(),
        }
    }
}

impl From<Type> for pb::Type {
    fn from(t: Type) -> Self {
        pb::Type {
            code: t.code,
            array_element_type: t.array_element_type.map(|e| Box::new((*e).into())),
            struct_type: t.struct_type.map(Into::into),
            type_annotation: t.type_annotation,
            proto_type_fqn: t.proto_type_fqn,
        }
    }
}

impl From<ListValue> for prost_types::ListValue {
    fn from(l: ListValue) -> Self {
        prost_types::ListValue {
            values: l.values.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<Value> for prost_types::Value {
    fn from(v: Value) -> Self {
        use prost_types::value::Kind as K;
        prost_types::Value {
            kind: v.kind.map(|k| match k {
                Kind::Null(n) => K::NullValue(n),
                Kind::Number(n) => K::NumberValue(n),
                Kind::String(s) => K::StringValue(s),
                Kind::Bool(b) => K::BoolValue(b),
                Kind::List(l) => K::ListValue(l.into()),
            }),
        }
    }
}

/// `ExecuteSql` decoding its response with [`ResultSet`].
pub async fn execute_sql<T>(
    channel: T,
    req: pb::ExecuteSqlRequest,
) -> Result<tonic::Response<pb::ResultSet>, tonic::Status>
where
    T: tonic::client::GrpcService<tonic::body::Body>,
    T::Error: Into<tonic::codegen::StdError>,
    T::ResponseBody: tonic::codegen::Body<Data = prost::bytes::Bytes> + Send + 'static,
    <T::ResponseBody as tonic::codegen::Body>::Error: Into<tonic::codegen::StdError> + Send,
{
    let mut grpc = tonic::client::Grpc::new(channel)
        .max_decoding_message_size(crate::runner::MAX_MSG)
        .max_encoding_message_size(crate::runner::MAX_MSG);
    grpc.ready()
        .await
        .map_err(|e| tonic::Status::unknown(format!("Service was not ready: {}", e.into())))?;
    let codec = tonic_prost::ProstCodec::<pb::ExecuteSqlRequest, ResultSet>::default();
    let path = tonic::codegen::http::uri::PathAndQuery::from_static(
        "/google.spanner.v1.Spanner/ExecuteSql",
    );
    let mut req = tonic::Request::new(req);
    req.extensions_mut().insert(tonic::GrpcMethod::new(
        "google.spanner.v1.Spanner",
        "ExecuteSql",
    ));
    let resp = grpc.unary(req, path, codec).await?;
    Ok(resp.map(Into::into))
}
