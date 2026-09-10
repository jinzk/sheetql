use std::collections::HashMap;
use std::sync::Arc;

use crate::evaluator::ExprId;
use crate::value::GroupKey;
use crate::value::Value;
use crate::value::group_key;

/// Incremental summary of a group's argument values, shared by `COUNT`, `SUM`,
/// `AVG`, `MIN` and `MAX` so each argument expression is evaluated once per
/// group and reused across every aggregate that references it.
#[derive(Debug, Clone, Default)]
pub struct AggregateSummary {
    pub count: i64,
    pub distinct_count: i64,
    pub numeric_count: usize,
    pub is_float: bool,
    pub sum_int: Option<i64>,
    pub sum_int_overflow: bool,
    pub sum_float: f64,
    pub distinct_numeric_count: usize,
    pub distinct_is_float: bool,
    pub distinct_sum_int: Option<i64>,
    pub distinct_sum_int_overflow: bool,
    pub distinct_sum_float: f64,
    pub distinct: std::collections::HashSet<GroupKey>,
    pub non_numeric: Option<Value>,
}

impl AggregateSummary {
    /// Scan a set of values and fold them into a summary. Shared by grouped
    /// aggregates and window aggregates so their `COUNT`/`SUM`/`AVG` semantics
    /// (distinct tracking, NaN handling, integer overflow) never diverge.
    pub(crate) fn from_values<'a>(values: impl IntoIterator<Item = &'a Value>) -> Self {
        let mut summary = Self::default();
        for value in values {
            if value.is_null() {
                continue;
            }
            summary.count += 1;
            let is_new_distinct = summary.distinct.insert(group_key(value));
            let is_nan = matches!(value, Value::Float(number) if number.is_nan());
            if let Value::Float(_) = value
                && !is_nan
            {
                summary.is_float = true;
            }
            if matches!(value, Value::Int(_) | Value::Float(_)) && !is_nan {
                summary.numeric_count += 1;
                summary.sum_float += value.as_f64().unwrap_or(0.0);
                if let Value::Int(number) = value {
                    summary.sum_int = Some(match summary.sum_int.unwrap_or(0).checked_add(*number) {
                        Some(sum) => sum,
                        None => {
                            summary.sum_int_overflow = true;
                            0
                        }
                    });
                }
                if is_new_distinct {
                    summary.distinct_numeric_count += 1;
                    if let Value::Float(_) = value {
                        summary.distinct_is_float = true;
                    }
                    summary.distinct_sum_float += value.as_f64().unwrap_or(0.0);
                    if let Value::Int(number) = value {
                        summary.distinct_sum_int = Some(
                            match summary.distinct_sum_int.unwrap_or(0).checked_add(*number) {
                                Some(sum) => sum,
                                None => {
                                    summary.distinct_sum_int_overflow = true;
                                    0
                                }
                            },
                        );
                    }
                }
            } else if !is_nan && summary.non_numeric.is_none() {
                summary.non_numeric = Some(value.clone());
            }
        }
        summary.distinct_count = summary.distinct.len() as i64;
        summary
    }
}

/// Per-group caches owned by the query execution: evaluated argument values
/// and their aggregate summaries, both keyed by the planned expression id.
#[derive(Debug, Default)]
pub(crate) struct GroupState {
    pub(crate) argument_values: HashMap<ExprId, Arc<[Value]>>,
    pub(crate) summaries: HashMap<ExprId, AggregateSummary>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summarize(values: Vec<Value>) -> AggregateSummary {
        AggregateSummary::from_values(values.iter())
    }

    #[test]
    fn from_values_counts_and_sums_mixed_numeric_variants() {
        let summary = summarize(vec![
            Value::Int(2),
            Value::Float(2.5),
            Value::Int(1),
            Value::Null,
        ]);
        assert_eq!(summary.count, 3);
        assert_eq!(summary.numeric_count, 3);
        assert!(summary.is_float);
        assert_eq!(summary.sum_int, Some(3));
        assert_eq!(summary.sum_float, 5.5);
        assert_eq!(summary.non_numeric, None);
    }

    #[test]
    fn from_values_ignores_null_values() {
        let summary = summarize(vec![Value::Null, Value::Null]);
        assert_eq!(summary.count, 0);
        assert_eq!(summary.numeric_count, 0);
        assert_eq!(summary.sum_int, None);
        assert_eq!(summary.sum_float, 0.0);
        assert_eq!(summary.distinct_count, 0);
        assert!(summary.distinct.is_empty());
    }

    #[test]
    fn from_values_unifies_distinct_int_and_float() {
        let summary = summarize(vec![
            Value::Int(1),
            Value::Float(1.0),
            Value::Float(2.5),
            Value::Int(2),
        ]);
        assert_eq!(summary.count, 4);
        // 1 and 1.0 share one group key.
        assert_eq!(summary.distinct_count, 3);
        assert_eq!(summary.distinct_numeric_count, 3);
        assert!(summary.distinct_is_float);
        // Distinct values are 1, 2.5 and 2.
        assert_eq!(summary.distinct_sum_float, 5.5);
        // The distinct int path only folds integer-typed distinct values.
        assert_eq!(summary.distinct_sum_int, Some(3));
    }

    #[test]
    fn from_values_skips_nan_for_numeric_tracking() {
        let summary = summarize(vec![Value::Float(f64::NAN), Value::Int(1)]);
        // NaN contributes to the count and distinct set but not the numeric sums.
        assert_eq!(summary.count, 2);
        assert_eq!(summary.distinct_count, 2);
        assert_eq!(summary.numeric_count, 1);
        assert!(!summary.is_float);
        assert_eq!(summary.sum_float, 1.0);
        assert_eq!(summary.non_numeric, None);
    }

    #[test]
    fn from_values_flags_integer_overflow() {
        let summary = summarize(vec![Value::Int(i64::MAX), Value::Int(1)]);
        assert!(summary.sum_int_overflow);
        assert_eq!(summary.numeric_count, 2);
    }

    #[test]
    fn from_values_does_not_overflow_distinct_sum_on_duplicates() {
        let summary = summarize(vec![Value::Int(i64::MAX), Value::Int(i64::MAX)]);
        // Two identical values overflow the plain sum but collapse into a
        // single distinct value, so the distinct sum never overflows.
        assert_eq!(summary.count, 2);
        assert_eq!(summary.distinct_count, 1);
        assert!(summary.sum_int_overflow);
        assert!(!summary.distinct_sum_int_overflow);
        assert_eq!(summary.distinct_sum_int, Some(i64::MAX));
    }

    #[test]
    fn from_values_records_first_non_numeric_value() {
        let summary = summarize(vec![
            Value::Int(1),
            Value::Text("oops".into()),
            Value::Bool(true),
        ]);
        assert_eq!(summary.count, 3);
        assert_eq!(summary.numeric_count, 1);
        assert_eq!(summary.sum_int, Some(1));
        assert_eq!(summary.non_numeric, Some(Value::Text("oops".into())));
    }
}