use std::collections::HashMap;

use super::join::ColumnRef;
use crate::error::Error;

pub(crate) fn object_name_to_parts(name: &sqlparser::ast::ObjectName) -> Vec<String> {
    name.0
        .iter()
        .filter_map(|part| part.as_ident())
        .map(|ident| ident.value.to_lowercase())
        .collect()
}

/// Build the case-normalized lookup used by expression evaluation. Bare names
/// become ambiguous when more than one relation exposes the same column.
pub(crate) fn build_lookup(schema: &[ColumnRef]) -> Result<HashMap<String, usize>, Error> {
    let mut map: HashMap<String, Vec<usize>> = HashMap::new();
    for (index, column) in schema.iter().enumerate() {
        for name in [
            column.column.clone(),
            format!("{}.{}", column.qualifier, column.column),
            format!("{}.{}", column.table_name, column.column),
        ] {
            let indices = map.entry(name).or_default();
            if !indices.contains(&index) {
                indices.push(index);
            }
        }
    }
    Ok(map
        .into_iter()
        .map(|(name, indices)| {
            let index = if indices.len() == 1 {
                indices[0]
            } else {
                usize::MAX
            };
            (name, index)
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_marks_duplicate_bare_columns_ambiguous() {
        let schema = vec![
            ColumnRef {
                table_name: "a".into(),
                qualifier: "a".into(),
                column: "id".into(),
            },
            ColumnRef {
                table_name: "b".into(),
                qualifier: "b".into(),
                column: "id".into(),
            },
        ];
        let lookup = build_lookup(&schema).unwrap();
        assert_eq!(lookup["id"], usize::MAX);
        assert_eq!(lookup["a.id"], 0);
        assert_eq!(lookup["b.id"], 1);
    }
}
