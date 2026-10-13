//! `$filter` translation that binds datetime literals as `time::OffsetDateTime`.
//!
//! The platform filter builder binds the parser's `chrono` literals, which sqlx writes to
//! `SQLite` as `...+00:00`, while stored `time` values read `...Z`: compared as text,
//! `updated_at gt <x>` would match `x` itself. The builder has no hook to change the bound type,
//! so [`filter_condition`] walks the filter tree itself, binds the datetime fields it is told
//! about as normalised `time` values ([`ts::normalize`], the shape of every stored value), and
//! hands every other node to the platform builder. Lists with a datetime column (chats now,
//! messages with `created_at` later) call it with their field enum, datetime fields and column
//! mapper.

use sea_orm::{ColumnTrait, Condition};
use time::OffsetDateTime;
use toolkit_db::odata::{FieldToColumn, filter_node_to_condition};
use toolkit_odata::Error as ODataError;
use toolkit_odata::filter::{FilterField, FilterNode, FilterOp, ODataValue};

use crate::infra::db::ts;

/// Converts the parser's UTC literal into a stored-shape value.
///
/// # Errors
/// `InvalidFilter` when the literal is outside the representable range.
fn to_stored(dt: &chrono::DateTime<chrono::Utc>) -> Result<OffsetDateTime, ODataError> {
    let out_of_range = |_| ODataError::InvalidFilter(format!("Datetime out of range: {dt}"));
    let secs = OffsetDateTime::from_unix_timestamp(dt.timestamp()).map_err(out_of_range)?;
    // A leap second is reported as 1_000_000_000..2_000_000_000 ns; clamp it into the second.
    let nanos = Ord::min(dt.timestamp_subsec_nanos(), 999_999_999);
    let at = secs.replace_nanosecond(nanos).map_err(out_of_range)?;
    Ok(ts::normalize(at))
}

/// Translates `node` into a condition. Comparisons (`eq ne gt ge lt le`) and `in` on a field of
/// `time_fields` bind `time` values; `and`/`or`/`not` are traversed; everything else is built by
/// the platform with mapper `M`.
///
/// # Errors
/// `InvalidFilter` for an operator the field does not take or a literal out of range.
pub fn filter_condition<F, M>(
    node: &FilterNode<F>,
    time_fields: &[F],
) -> Result<Condition, ODataError>
where
    F: FilterField,
    M: FieldToColumn<F>,
{
    let invalid = ODataError::InvalidFilter;
    match node {
        FilterNode::Binary {
            field,
            op,
            value: ODataValue::DateTime(dt),
        } if time_fields.contains(field) => {
            let col = M::map_field(*field);
            let at = to_stored(dt)?;
            let expr = match op {
                FilterOp::Eq => col.eq(at),
                FilterOp::Ne => col.ne(at),
                FilterOp::Gt => col.gt(at),
                FilterOp::Ge => col.gte(at),
                FilterOp::Lt => col.lt(at),
                FilterOp::Le => col.lte(at),
                other => {
                    return Err(invalid(format!(
                        "Operator {other:?} not valid for {}",
                        field.name()
                    )));
                }
            };
            Ok(Condition::all().add(expr))
        }
        FilterNode::InList { field, values } if time_fields.contains(field) => {
            let times = values
                .iter()
                .map(|v| match v {
                    ODataValue::DateTime(dt) => to_stored(dt),
                    other => Err(invalid(format!("Expected a datetime, got {other:?}"))),
                })
                .collect::<Result<Vec<_>, _>>()?;
            if times.is_empty() {
                return Err(invalid("IN list must not be empty".to_owned()));
            }
            Ok(Condition::all().add(M::map_field(*field).is_in(times)))
        }
        FilterNode::Composite { op, children } => {
            let base = match op {
                FilterOp::And => Condition::all(),
                FilterOp::Or => Condition::any(),
                other => return Err(invalid(format!("Invalid composite operator: {other:?}"))),
            };
            children.iter().try_fold(base, |acc, child| {
                Ok(acc.add(filter_condition::<F, M>(child, time_fields)?))
            })
        }
        FilterNode::Not(inner) => Ok(Condition::all()
            .add(filter_condition::<F, M>(inner, time_fields)?)
            .not()),
        other => filter_node_to_condition::<F, M>(other).map_err(invalid),
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone as _;

    use super::*;

    #[test]
    fn literals_are_normalised_and_out_of_range_is_an_error() {
        let dt = chrono::Utc
            .timestamp_opt(1_790_000_000, 123_456_789)
            .unwrap();
        let at = to_stored(&dt).unwrap();
        assert_eq!(at.unix_timestamp(), 1_790_000_000);
        assert_eq!(at.nanosecond(), 123_456_001);

        assert!(matches!(
            to_stored(&chrono::DateTime::<chrono::Utc>::MAX_UTC),
            Err(ODataError::InvalidFilter(_))
        ));
    }
}
