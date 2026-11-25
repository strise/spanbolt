//! Conversions between wire types and storage types.

use spanbolt_ddl::ColumnType;
use spanbolt_proto::spanner as pb;
use spanbolt_storage::{KeyRange, KeySet, Timestamp, decode_key, decode_point_key};
use tonic::Status;

use crate::status::to_status;

pub fn spanner_type(t: &ColumnType) -> pb::Type {
    use pb::TypeCode as C;
    let code = |c: C| pb::Type {
        code: c as i32,
        ..Default::default()
    };
    match t {
        ColumnType::Bool => code(C::Bool),
        ColumnType::Int64 => code(C::Int64),
        ColumnType::Float32 => code(C::Float32),
        ColumnType::Float64 => code(C::Float64),
        ColumnType::Numeric => code(C::Numeric),
        ColumnType::String(_) => code(C::String),
        ColumnType::Bytes(_) | ColumnType::TokenList => code(C::Bytes),
        ColumnType::Date => code(C::Date),
        ColumnType::Timestamp => code(C::Timestamp),
        ColumnType::Json => code(C::Json),
        ColumnType::Interval => code(C::Interval),
        ColumnType::Uuid => code(C::Uuid),
        ColumnType::Proto(name) => pb::Type {
            code: C::Proto as i32,
            proto_type_fqn: name.clone(),
            ..Default::default()
        },
        ColumnType::Array(e) => pb::Type {
            code: C::Array as i32,
            array_element_type: Some(Box::new(spanner_type(e))),
            ..Default::default()
        },
        ColumnType::Struct(fields) => pb::Type {
            code: C::Struct as i32,
            struct_type: Some(pb::StructType {
                fields: fields
                    .iter()
                    .map(|(n, t)| pb::struct_type::Field {
                        name: n.clone().unwrap_or_default(),
                        r#type: Some(spanner_type(t)),
                    })
                    .collect(),
            }),
            ..Default::default()
        },
    }
}

/// The key columns a Read or delete key set is decoded against, for its error messages.
pub struct KeyColumns<'a> {
    /// The table or index, as named in key-arity errors.
    pub name: &'a str,
    /// The table production Spanner names in a value error: the table, or `_Index_<index>`.
    pub table: String,
    pub names: Vec<&'a str>,
    pub types: Vec<ColumnType>,
}

/// Decodes a Read or delete key set. With `prod_messages`, errors are worded as production
/// Spanner words them: key arity with the key as JSON, and a value that doesn't decode as its
/// key column's type as `Invalid value for column C in table T: Expected <type>.`.
pub fn keyset(
    ks: Option<&pb::KeySet>,
    cols: &KeyColumns<'_>,
    prod_messages: bool,
) -> spanbolt_storage::Result<KeySet> {
    let Some(ks) = ks else {
        return Ok(KeySet::default());
    };
    if ks.all {
        return Ok(KeySet::all());
    }
    let types = &cols.types[..];
    let value_error = |list: &prost_types::ListValue, e| match prod_messages {
        true => prod_value_error(list, cols, e),
        false => e,
    };
    let mut out = KeySet::default();
    for k in &ks.keys {
        out.keys.push(
            decode_point_key(k, types, cols.name, prod_messages).map_err(|e| {
                if k.values.len() == types.len() {
                    value_error(k, e)
                } else {
                    e
                }
            })?,
        );
    }
    let range_key =
        |l: &prost_types::ListValue| decode_key(l, types).map_err(|e| value_error(l, e));
    use pb::key_range::{EndKeyType, StartKeyType};
    for r in &ks.ranges {
        let (start, start_closed) = match &r.start_key_type {
            Some(StartKeyType::StartClosed(l)) => (range_key(l)?, true),
            Some(StartKeyType::StartOpen(l)) => (range_key(l)?, false),
            None => (Vec::new(), true),
        };
        let (end, end_closed) = match &r.end_key_type {
            Some(EndKeyType::EndClosed(l)) => (range_key(l)?, true),
            Some(EndKeyType::EndOpen(l)) => (range_key(l)?, false),
            None => (Vec::new(), true),
        };
        out.ranges.push(KeyRange {
            start,
            start_closed,
            end,
            end_closed,
        });
    }
    Ok(out)
}

/// Production Spanner's error for the first part of `list` that doesn't decode (`e` is the
/// emulator-worded error for it): NUMERIC strings get the NUMERIC parser's OUT_OF_RANGE error,
/// everything else names the column and the type it expected.
fn prod_value_error(
    list: &prost_types::ListValue,
    cols: &KeyColumns<'_>,
    e: spanbolt_storage::Error,
) -> spanbolt_storage::Error {
    use prost_types::value::Kind;
    use spanbolt_storage::error::{Code, Error};
    let Some((i, v)) = list
        .values
        .iter()
        .zip(&cols.types)
        .enumerate()
        .find_map(|(i, (v, t))| {
            spanbolt_storage::wire::decode(v, t)
                .is_err()
                .then_some((i, v))
        })
    else {
        return e;
    };
    let ty = &cols.types[i];
    if let (ColumnType::Numeric, Some(Kind::StringValue(s))) = (ty, &v.kind) {
        return match numeric_parse_error(s) {
            Some(detail) => {
                Error::new(Code::OutOfRange, format!("Invalid NUMERIC value: {detail}"))
            }
            None => e,
        };
    }
    let expected = match ty {
        ColumnType::Bytes(_) => "Base64 encoded BYTES".to_string(),
        t => spanbolt_storage::value::type_name(t),
    };
    Error::failed_precondition(format!(
        "Invalid value for column {} in table {}: Expected {expected}.",
        cols.names.get(i).copied().unwrap_or_default(),
        cols.table
    ))
}

/// Why production Spanner's NUMERIC parser rejects `s`, for the plain decimal forms recorded
/// from it; `None` for anything else (exponents), which keeps the emulator's message.
fn numeric_parse_error(s: &str) -> Option<String> {
    let digits = s.strip_prefix(['+', '-']).unwrap_or(s);
    if let Some(c) = digits.chars().find(|c| !c.is_ascii_digit() && *c != '.') {
        if c == 'e' || c == 'E' {
            return None;
        }
        return Some(format!("invalidCharacter {c} found in {s}"));
    }
    let (whole, frac) = digits.split_once('.').unwrap_or((digits, ""));
    if frac.contains('.') {
        return Some(format!("{s} has more than one decimal point"));
    }
    if whole.trim_start_matches('0').len() > 29 {
        return Some(format!("whole component of NUMERIC {s} too large"));
    }
    if frac.trim_end_matches('0').len() > 9 {
        return Some(format!("fractional component of NUMERIC {s} too large"));
    }
    None
}

pub fn timestamp(t: &prost_types::Timestamp) -> Result<Timestamp, Status> {
    Timestamp::from_proto(t).ok_or_else(|| Status::invalid_argument("Invalid timestamp"))
}

pub fn duration(d: &prost_types::Duration) -> Result<std::time::Duration, Status> {
    if d.seconds < 0 || d.nanos < 0 {
        return Err(Status::invalid_argument("Staleness must be non-negative."));
    }
    Ok(std::time::Duration::new(d.seconds as u64, d.nanos as u32))
}

pub fn storage_err(e: spanbolt_storage::Error) -> Status {
    to_status(e)
}

#[cfg(test)]
mod tests {
    use super::numeric_parse_error;

    #[test]
    fn numeric_parse_errors_as_spanner_words_them() {
        let e = |s| numeric_parse_error(s);
        assert_eq!(e("abc").as_deref(), Some("invalidCharacter a found in abc"));
        assert_eq!(
            e("1.2.3").as_deref(),
            Some("1.2.3 has more than one decimal point")
        );
        assert_eq!(
            e("1.0000000001").as_deref(),
            Some("fractional component of NUMERIC 1.0000000001 too large")
        );
        assert_eq!(
            e("100000000000000000000000000000").as_deref(),
            Some("whole component of NUMERIC 100000000000000000000000000000 too large")
        );
        assert_eq!(e("-1.5"), None);
        assert_eq!(e("1e5"), None);
    }
}
