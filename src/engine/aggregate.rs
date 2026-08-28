/// Return the first source row of a group. Empty groups use an empty row so
/// aggregate functions can still produce their SQL NULL/zero result.
pub(crate) fn representative_index(group: &[usize]) -> Option<usize> {
    group.first().copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn representative_row_selects_group_head() {
        let _rows = [10, 20, 30];
        assert_eq!(representative_index(&[2, 0]), Some(2));
    }
}
