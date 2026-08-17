use std::collections::HashMap;

use crate::value::Value;

#[derive(Debug, Clone)]
pub struct Table {
    pub name: String,
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
}

#[derive(Debug, Default)]
pub struct Database {
    pub name: String,
    pub tables: Vec<Table>,
    by_name: HashMap<String, usize>,
}

impl Database {
    pub fn new() -> Self {
        Self {
            name: String::new(),
            tables: vec![],
            by_name: HashMap::new(),
        }
    }

    pub fn named(name: impl Into<String>) -> Self {
        let mut database = Self::new();
        database.name = name.into();
        database
    }

    pub fn add_table(&mut self, mut table: Table) {
        let base_name = table.name.clone();
        let mut final_name = base_name.clone();
        let mut counter = 1;
        while self.by_name.contains_key(&final_name) {
            final_name = format!("{}_{}", base_name, counter);
            counter += 1;
        }
        table.name = final_name.clone();
        self.by_name.insert(final_name, self.tables.len());
        self.tables.push(table);
    }

    pub fn get_table(&self, name: &str) -> Option<&Table> {
        self.by_name.get(name).map(|index| &self.tables[*index])
    }

    pub fn table_names(&self) -> Vec<&str> {
        self.tables.iter().map(|t| t.name.as_str()).collect()
    }
}

/// A collection of named databases (one per file), plus the currently
/// selected database for unqualified table references.
#[derive(Debug, Default)]
pub struct Schema {
    pub databases: Vec<Database>,
    current: Option<String>,
}

impl Schema {
    pub fn new() -> Self {
        Self {
            databases: vec![],
            current: None,
        }
    }

    pub fn add_database(&mut self, mut database: Database) {
        let base_name = database.name.clone();
        let mut final_name = base_name.clone();
        let mut counter = 1;
        while self
            .databases
            .iter()
            .any(|existing| existing.name == final_name)
        {
            final_name = format!("{}_{}", base_name, counter);
            counter += 1;
        }
        database.name = final_name;
        self.databases.push(database);
    }

    pub fn get_database(&self, name: &str) -> Option<&Database> {
        self.databases.iter().find(|db| db.name == name)
    }

    pub fn database_names(&self) -> Vec<&str> {
        self.databases.iter().map(|db| db.name.as_str()).collect()
    }

    pub fn set_current_database(&mut self, name: &str) -> Result<(), String> {
        if self.get_database(name).is_none() {
            return Err(format!("Unknown database `{name}`"));
        }
        self.current = Some(name.to_string());
        Ok(())
    }

    pub fn current_database(&self) -> Option<&str> {
        self.current.as_deref()
    }

    /// Resolve a table reference. With a database name, looks only in that
    /// database. Without one, searches the current database first, then every
    /// other database, reporting ambiguity if the name matches in multiple.
    pub fn resolve_table(
        &self,
        database: Option<&str>,
        table: &str,
    ) -> Result<(&Database, &Table), String> {
        match database {
            Some(name) => {
                let database = self
                    .get_database(name)
                    .ok_or_else(|| format!("Unknown database `{name}`"))?;
                let table = database
                    .get_table(table)
                    .ok_or_else(|| format!("Table `{table}` not found in database `{name}`"))?;
                Ok((database, table))
            }
            None => {
                if let Some(current) = &self.current
                    && let Some(database) = self.get_database(current)
                    && let Some(table) = database.get_table(table)
                {
                    return Ok((database, table));
                }
                let mut found: Vec<&Database> = vec![];
                for database in &self.databases {
                    if database.get_table(table).is_some() {
                        found.push(database);
                    }
                }
                match found.len() {
                    0 => Err(format!("Table `{table}` not found")),
                    1 => {
                        let database = found[0];
                        Ok((database, database.get_table(table).expect("just found")))
                    }
                    _ => Err(format!(
                        "Table `{table}` is ambiguous, qualify it as `database.table`"
                    )),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(name: &str) -> Table {
        Table {
            name: name.to_string(),
            columns: vec![],
            rows: vec![],
        }
    }

    #[test]
    fn add_table_dedups_colliding_names() {
        let mut database = Database::new();
        database.add_table(table("sales"));
        database.add_table(table("sales"));
        database.add_table(table("sales"));
        assert_eq!(database.table_names(), vec!["sales", "sales_1", "sales_2"]);
        assert_eq!(database.tables.len(), 3);
    }

    #[test]
    fn add_table_keeps_distinct_names() {
        let mut database = Database::new();
        database.add_table(table("a"));
        database.add_table(table("b"));
        assert_eq!(database.table_names(), vec!["a", "b"]);
    }

    #[test]
    fn get_table_returns_registered_table() {
        let mut database = Database::new();
        database.add_table(Table {
            name: "people".to_string(),
            columns: vec!["id".to_string(), "name".to_string()],
            rows: vec![],
        });
        let table = database.get_table("people").expect("table exists");
        assert_eq!(table.columns.get(1), Some(&"name".to_string()));
        assert!(database.get_table("nope").is_none());
    }

    fn make_schema() -> Schema {
        let mut schema = Schema::new();
        let mut sales = Database::named("data_sales_csv");
        sales.add_table(table("sales"));
        let mut backup = Database::named("backup_sales_csv");
        backup.add_table(table("sales"));
        let mut report = Database::named("data_report_xlsx");
        report.add_table(table("sheet_1"));
        schema.add_database(sales);
        schema.add_database(backup);
        schema.add_database(report);
        schema
    }

    #[test]
    fn schema_keeps_distinct_database_names() {
        let schema = make_schema();
        assert_eq!(
            schema.database_names(),
            vec!["data_sales_csv", "backup_sales_csv", "data_report_xlsx"]
        );
        assert_eq!(schema.current_database(), None);
    }

    #[test]
    fn schema_set_current_database_validates() {
        let mut schema = make_schema();
        schema.set_current_database("data_sales_csv").unwrap();
        assert_eq!(schema.current_database(), Some("data_sales_csv"));
        assert!(schema.set_current_database("nope").is_err());
    }

    #[test]
    fn schema_resolve_qualified_table() {
        let schema = make_schema();
        let (db, table) = schema
            .resolve_table(Some("backup_sales_csv"), "sales")
            .unwrap();
        assert_eq!(db.name, "backup_sales_csv");
        assert_eq!(table.name, "sales");
        assert!(
            schema
                .resolve_table(Some("data_sales_csv"), "missing")
                .is_err()
        );
        assert!(schema.resolve_table(Some("nope"), "sales").is_err());
    }

    #[test]
    fn schema_resolve_unqualified_uses_current_database() {
        let mut schema = make_schema();
        schema.set_current_database("backup_sales_csv").unwrap();
        let (db, _) = schema.resolve_table(None, "sales").unwrap();
        assert_eq!(db.name, "backup_sales_csv");
    }

    #[test]
    fn schema_resolve_unqualified_reports_ambiguity() {
        let schema = make_schema();
        let err = schema.resolve_table(None, "sales").unwrap_err();
        assert!(err.contains("ambiguous"), "got: {err}");
        assert!(schema.resolve_table(None, "missing").is_err());
        let (db, _) = schema.resolve_table(None, "sheet_1").unwrap();
        assert_eq!(db.name, "data_report_xlsx");
    }
}
