use std::io::{self, BufRead, IsTerminal};
use std::path::Path;
use std::time::Instant;

mod arguments;
mod completion;
mod database;
mod engine;
mod evaluator;
mod functions;
mod highlight;
mod loader;
mod naming;
mod printer;
mod server;
mod ui;
mod value;

use arguments::{Command, parse_arguments};
use database::Schema;
use printer::{OutputFormat, render};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match parse_arguments(&args) {
        Command::ReplMode(arguments) => {
            if let Err(error) = launch_repl(arguments) {
                fail(1, error);
            }
        }
        Command::QueryMode(query, arguments) => {
            if let Err(error) = validate_files(&arguments.files, arguments.max_file_bytes) {
                fail(3, error);
            }

            let csv_options = csv_options(&arguments);
            let mut schema =
                match loader::load_schema(&arguments.files, arguments.max_rows, &csv_options) {
                    Ok(schema) => schema,
                    Err(error) => fail(3, error),
                };
            if let Err(error) = execute_query(&query, &arguments, &mut schema) {
                fail(4, error);
            }
        }
        Command::ServerMode(arguments) => {
            if let Err(error) = validate_files(&arguments.files, arguments.max_file_bytes) {
                fail(3, error);
            }

            let csv_options = csv_options(&arguments);
            let mut schema =
                match loader::load_schema(&arguments.files, arguments.max_rows, &csv_options) {
                    Ok(schema) => schema,
                    Err(error) => fail(3, error),
                };
            if let Err(error) = server::run(&mut schema, arguments.export_root.as_deref()) {
                fail(1, error);
            }
        }
        Command::Help => arguments::print_help_list(),
        Command::Version => println!("Sheetql version {}", env!("CARGO_PKG_VERSION")),
        Command::Error(error) => fail(2, error),
    }
}

fn fail(code: i32, error: String) -> ! {
    eprintln!("{error}");
    std::process::exit(code);
}

fn validate_files(files: &[String], max_file_bytes: Option<u64>) -> Result<(), String> {
    for file in files {
        if !Path::new(file).exists() {
            return Err(format!("File `{file}` does not exist"));
        }

        if !loader::is_supported_file(file) {
            return Err(format!(
                "Unsupported file format for `{file}`, expected one of: xls, xlsx, xlsm, csv"
            ));
        }

        if let Some(max_file_bytes) = max_file_bytes {
            let size = std::fs::metadata(file)
                .map_err(|error| format!("Cannot inspect file `{file}`: {error}"))?
                .len();
            if size > max_file_bytes {
                return Err(format!(
                    "File `{file}` is {size} bytes, exceeding the maximum of {max_file_bytes} bytes"
                ));
            }
        }
    }
    Ok(())
}

fn execute_query(
    query: &str,
    arguments: &arguments::Arguments,
    schema: &mut Schema,
) -> Result<(), String> {
    let start = Instant::now();
    match engine::run_query(schema, query) {
        Ok(result) => {
            let duration = start.elapsed();
            print_result(arguments, &result.columns, &result.rows);

            save_result(arguments, &result.columns, &result.rows)?;

            if arguments.analysis {
                let plural = if result.rows.len() == 1 { "" } else { "s" };
                println!(
                    "{} row{plural} in set ({:?}); scanned {} row(s)",
                    result.rows.len(),
                    duration,
                    result.stats.input_rows
                );
            }
            Ok(())
        }
        Err(error) => Err(error),
    }
}

fn print_result(arguments: &arguments::Arguments, columns: &[String], rows: &[Vec<value::Value>]) {
    if arguments.output_format == OutputFormat::Table
        && arguments.pagination
        && rows.len() > arguments.page_size
    {
        let total_pages = rows.len().div_ceil(arguments.page_size);
        for page in 0..total_pages {
            let start = page * arguments.page_size;
            let end = usize::min(start + arguments.page_size, rows.len());
            print!(
                "{}",
                render(OutputFormat::Table, columns, &rows[start..end])
            );

            if page + 1 < total_pages {
                print!(
                    "\n[Page {}/{} - Press Enter to continue]",
                    page + 1,
                    total_pages
                );
                std::io::Write::flush(&mut std::io::stdout()).expect("flush failed!");
                let mut input = String::new();
                std::io::stdin().read_line(&mut input).unwrap_or(0);
            }
        }
        return;
    }

    print!("{}", render(arguments.output_format, columns, rows));
}

fn save_result(
    arguments: &arguments::Arguments,
    columns: &[String],
    rows: &[Vec<value::Value>],
) -> Result<(), String> {
    if let Some(path) = &arguments.output_file {
        let content = render(OutputFormat::Csv, columns, rows);
        std::fs::write(path, content)
            .map_err(|error| format!("Cannot write result to `{path}`: {error}"))?;
        if arguments.analysis {
            eprintln!("Result saved to `{path}`");
        }
    }
    Ok(())
}

fn launch_repl(arguments: arguments::Arguments) -> Result<(), String> {
    validate_files(&arguments.files, arguments.max_file_bytes)?;
    let csv_options = csv_options(&arguments);
    let mut schema = loader::load_schema(&arguments.files, arguments.max_rows, &csv_options)?;

    if io::stdin().is_terminal() {
        ui::run(&mut schema).map_err(|error| error.to_string())
    } else {
        run_piped_queries(&mut schema)
    }
}

fn csv_options(arguments: &arguments::Arguments) -> loader::CsvOptions {
    loader::CsvOptions {
        delimiter: arguments.csv_delimiter,
        has_header: !arguments.csv_no_header,
        null_value: arguments.csv_null_value.clone(),
    }
}

/// Execute one SQL statement per stdin line without enabling terminal raw mode.
fn run_piped_queries(schema: &mut Schema) -> Result<(), String> {
    for line in io::stdin().lock().lines() {
        let query = line.map_err(|error| format!("Cannot read query: {error}"))?;
        let query = query.trim();
        if query.is_empty() {
            continue;
        }
        match engine::run_query(schema, query) {
            Ok(result) => print!(
                "{}",
                render(OutputFormat::Table, &result.columns, &result.rows)
            ),
            Err(error) => eprintln!("{error}"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_files_rejects_files_over_byte_limit() {
        let path =
            std::env::temp_dir().join(format!("sheetql_file_limit_{}.csv", std::process::id()));
        std::fs::write(&path, "a\n12345\n").unwrap();
        let files = vec![path.to_string_lossy().into_owned()];
        let error = validate_files(&files, Some(1)).unwrap_err();
        assert!(
            error.contains("exceeding the maximum of 1 bytes"),
            "got: {error}"
        );
        std::fs::remove_file(path).ok();
    }
}
