/// Sanitize a name into a valid, lowercase SQL identifier.
///
/// Rules:
/// 1. `/` and `\` are replaced with `_`
/// 2. Any character that is not a Unicode alphanumeric or `_` is replaced with `_`
/// 3. Consecutive `_` are NOT merged
/// 4. The result is lowercased
/// 5. A leading digit is prefixed with `_`
pub fn sanitize(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        if c == '_' || c.is_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else {
            out.push('_');
        }
    }

    if out.starts_with(|c: char| c.is_ascii_digit()) {
        out.insert(0, '_');
    }

    out
}

/// Build the database name for a file: the sanitized full path including
/// extension. Since paths are unique, database names never collide.
pub fn database_name(path: &str) -> String {
    sanitize(path)
}

/// Build the table name for a plain single-table file (csv): the sanitized
/// file stem (file name without extension).
pub fn csv_table_name(path: &str) -> String {
    let stem = std::path::Path::new(path)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or(path);
    sanitize(stem)
}

/// Build the table name for a spreadsheet sheet.
pub fn spreadsheet_table_name(sheet_name: &str) -> String {
    sanitize(sheet_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_replaces_separators_and_lowercases() {
        assert_eq!(sanitize("data/sales.csv"), "data_sales_csv");
        assert_eq!(sanitize(r"C:\data\sales.csv"), "c__data_sales_csv");
        assert_eq!(sanitize("Hello World"), "hello_world");
        assert_eq!(sanitize("na-me"), "na_me");
    }

    #[test]
    fn sanitize_does_not_merge_consecutive_underscores() {
        assert_eq!(sanitize("a--b"), "a__b");
        assert_eq!(sanitize("a b"), "a_b");
    }

    #[test]
    fn sanitize_prefixes_leading_digit() {
        assert_eq!(sanitize("123abc"), "_123abc");
    }

    #[test]
    fn sanitize_keeps_unicode_alphanumerics() {
        assert_eq!(sanitize("café"), "café");
        assert_eq!(sanitize("日本語"), "日本語");
    }

    #[test]
    fn database_name_is_sanitized_full_path() {
        assert_eq!(database_name("data/sales.csv"), "data_sales_csv");
        assert_eq!(database_name("sales.csv"), "sales_csv");
        assert_eq!(database_name(r"C:\data\report.xlsx"), "c__data_report_xlsx");
    }

    #[test]
    fn csv_table_name_is_sanitized_file_stem() {
        assert_eq!(csv_table_name("data/sales.csv"), "sales");
        assert_eq!(csv_table_name("sales.csv"), "sales");
        assert_eq!(csv_table_name("report.2024.csv"), "report_2024");
    }

    #[test]
    fn spreadsheet_table_name_is_sanitized_sheet() {
        assert_eq!(spreadsheet_table_name("Sheet 1"), "sheet_1");
        assert_eq!(spreadsheet_table_name("Sales"), "sales");
        assert_eq!(spreadsheet_table_name("Sales Data"), "sales_data");
    }
}