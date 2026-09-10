//! Integration-style tests for the query engine, grouped by topic. These live
//! inside the crate so they can reach `pub(crate)` APIs such as
//! `strip_into_outfile`.

use crate::database::{Database, Schema, Table};
use crate::engine::{run_query, QueryResult};
use crate::value::Value;

mod aggregate;
mod join;
mod like;
mod limit;
mod metadata;
mod outfile;
mod select;
mod set;
mod subquery;
mod temporary;
mod window;

pub(crate) fn make_database() -> Database {
    let mut database = Database::named("test");
    database.add_table(Table {
        name: "people".to_string(),
        columns: vec![
            "id".to_string(),
            "name".to_string(),
            "age".to_string(),
            "city".to_string(),
        ],
        rows: vec![
            vec![
                Value::Int(1),
                Value::Text("Alice".into()),
                Value::Int(30),
                Value::Text("NY".into()),
            ],
            vec![
                Value::Int(2),
                Value::Text("Bob".into()),
                Value::Int(25),
                Value::Text("LA".into()),
            ],
            vec![
                Value::Int(3),
                Value::Text("Carol".into()),
                Value::Int(35),
                Value::Text("SF".into()),
            ],
            vec![
                Value::Int(4),
                Value::Text("Dan".into()),
                Value::Int(40),
                Value::Text("NY".into()),
            ],
            vec![
                Value::Int(5),
                Value::Text("Eve".into()),
                Value::Int(28),
                Value::Text("LA".into()),
            ],
        ],
    });
    database.add_table(Table {
        name: "orders".to_string(),
        columns: vec![
            "order_id".to_string(),
            "customer_id".to_string(),
            "amount".to_string(),
        ],
        rows: vec![
            vec![Value::Int(101), Value::Int(1), Value::Float(50.5)],
            vec![Value::Int(102), Value::Int(2), Value::Float(20.0)],
            vec![Value::Int(103), Value::Int(1), Value::Float(99.9)],
        ],
    });
    database
}

pub(crate) fn make_schema() -> Schema {
    let mut schema = Schema::new();
    schema.add_database(make_database());
    schema.set_current_database("test").unwrap();
    schema
}

pub(crate) fn run(schema: &mut Schema, sql: &str) -> QueryResult {
    run_query(schema, sql).unwrap_or_else(|error| panic!("query `{sql}` failed: {error}"))
}