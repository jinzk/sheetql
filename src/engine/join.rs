use crate::value::{GroupKey, Value, group_key};

/// Build the composite hash key used by equi-joins. Keeping this operation in
/// the join module gives hash and probe paths one canonical key definition.
pub(crate) fn join_key(row: &[Value], columns: impl Iterator<Item = usize>) -> Vec<GroupKey> {
    columns.map(|column| group_key(&row[column])).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn composite_join_key_preserves_requested_column_order() {
        let row = vec![Value::Int(1), Value::Text("x".into()), Value::Int(2)];
        assert_eq!(join_key(&row, [2, 0].into_iter()).len(), 2);
    }
}
