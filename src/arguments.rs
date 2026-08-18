use std::collections::HashSet;

use crate::printer::OutputFormat;

#[derive(Debug, PartialEq)]
pub struct Arguments {
    pub files: Vec<String>,
    pub analysis: bool,
    pub pagination: bool,
    pub page_size: usize,
    pub output_format: OutputFormat,
    pub output_file: Option<String>,
    pub server: bool,
}

impl Arguments {
    fn new() -> Arguments {
        Arguments {
            files: vec![],
            analysis: false,
            pagination: false,
            page_size: 10,
            output_format: OutputFormat::Table,
            output_file: None,
            server: false,
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum Command {
    ReplMode(Arguments),
    QueryMode(String, Arguments),
    ServerMode(Arguments),
    Help,
    Version,
    Error(String),
}

pub fn parse_arguments(args: &[String]) -> Command {
    if args
        .iter()
        .any(|argument| argument == "--help" || argument == "-h")
    {
        return Command::Help;
    }

    if args
        .iter()
        .any(|argument| argument == "--version" || argument == "-v")
    {
        return Command::Version;
    }

    let mut optional_query: Option<String> = None;
    let mut arguments = Arguments::new();

    let mut arg_index = 1;
    loop {
        if arg_index >= args.len() {
            break;
        }

        let arg = &args[arg_index];
        if !arg.starts_with('-') {
            return Command::Error(format!("Unknown argument {arg}"));
        }

        match arg.as_str() {
            "--files" | "-f" => {
                arg_index += 1;
                let mut found = false;
                while arg_index < args.len() && !args[arg_index].starts_with('-') {
                    arguments.files.push(args[arg_index].clone());
                    arg_index += 1;
                    found = true;
                }
                if !found {
                    return Command::Error(format!(
                        "Argument {arg} must be followed by one or more file paths"
                    ));
                }
            }
            "--query" | "-q" => match take_value(args, &mut arg_index, arg, "the query") {
                Ok(value) => optional_query = Some(value.to_string()),
                Err(message) => return Command::Error(message),
            },
            "--analysis" | "-a" => {
                arguments.analysis = true;
                arg_index += 1;
            }
            "--pagination" | "-p" => {
                arguments.pagination = true;
                arg_index += 1;
            }
            "--pagesize" | "-ps" => match take_value(args, &mut arg_index, arg, "the page size") {
                Ok(page_size) => match page_size.parse::<usize>() {
                    Ok(page_size) if page_size > 0 => {
                        arguments.page_size = page_size;
                    }
                    Ok(_) => {
                        return Command::Error("Page size must be greater than zero".to_string());
                    }
                    Err(_) => return Command::Error("Invalid page size".to_string()),
                },
                Err(message) => return Command::Error(message),
            },
            "--output" | "-o" => match take_value(args, &mut arg_index, arg, "the output format") {
                Ok(output) => {
                    arguments.output_format = match output.to_lowercase().as_str() {
                        "csv" => OutputFormat::Csv,
                        "json" => OutputFormat::Json,
                        "yaml" => OutputFormat::Yaml,
                        "render" | "table" => OutputFormat::Table,
                        _ => {
                            return Command::Error(
                                "Invalid output format, expected one of: render, json, csv, yaml"
                                    .to_string(),
                            );
                        }
                    };
                }
                Err(message) => return Command::Error(message),
            },
            "--save" | "-s" => match take_value(args, &mut arg_index, arg, "an output file path") {
                Ok(path) => arguments.output_file = Some(path.to_string()),
                Err(message) => return Command::Error(message),
            },
            "--server" | "-S" => {
                arguments.server = true;
                arg_index += 1;
            }
            _ => return Command::Error(format!("Unknown argument {arg}")),
        }
    }

    match expand_file_patterns(&arguments.files) {
        Ok(files) => arguments.files = files,
        Err(error) => return Command::Error(error),
    }

    if arguments.files.is_empty() {
        return Command::Error(
            "Missing file paths, use `-f` to pass one or more xls/xlsx/csv files".to_string(),
        );
    }

    if arguments.output_file.is_some() && optional_query.is_none() {
        return Command::Error("--save requires --query".to_string());
    }

    if arguments.server {
        return Command::ServerMode(arguments);
    }

    match optional_query {
        Some(query) => Command::QueryMode(query, arguments),
        None => Command::ReplMode(arguments),
    }
}

/// Consume the value following a flag argument, advancing `arg_index` past it.
fn take_value<'a>(
    args: &'a [String],
    arg_index: &mut usize,
    arg: &str,
    what: &str,
) -> Result<&'a str, String> {
    *arg_index += 1;
    if *arg_index >= args.len() {
        return Err(format!("Argument {arg} must be followed by {what}"));
    }
    let value = args[*arg_index].as_str();
    *arg_index += 1;
    Ok(value)
}

fn expand_file_patterns(files: &[String]) -> Result<Vec<String>, String> {
    let mut expanded = Vec::new();
    let mut seen = HashSet::new();

    for file in files {
        if !contains_glob_magic(file) {
            if seen.insert(path_key(file)) {
                expanded.push(file.clone());
            }
            continue;
        }

        let matches =
            glob::glob(file).map_err(|error| format!("Invalid file pattern `{file}`: {error}"))?;
        let mut matched = false;
        for path in matches {
            let path =
                path.map_err(|error| format!("Cannot expand file pattern `{file}`: {error}"))?;
            let path = path.to_string_lossy().into_owned();
            matched = true;
            if seen.insert(path_key(&path)) {
                expanded.push(path);
            }
        }

        if !matched {
            return Err(format!("File pattern `{file}` matched no files"));
        }
    }

    Ok(expanded)
}

fn contains_glob_magic(path: &str) -> bool {
    path.chars()
        .any(|character| matches!(character, '*' | '?' | '['))
}

fn path_key(path: &str) -> String {
    path.replace('\\', "/")
}

pub fn print_help_list() {
    println!("Sheetql is a tool to run SQL-like queries on xls, xlsx and csv files");
    println!();
    println!("Usage: Sheetql [OPTIONS]");
    println!();
    println!("Options:");
    println!("-f,  --files <paths>        Paths to xls/xlsx/csv files to query");
    println!("-q,  --query <SQL Query>     Sheetql query to run on selected files");
    println!("-p,  --pagination            Enable print result with pagination");
    println!("-ps, --pagesize              Set pagination page size [default: 10]");
    println!("-o,  --output                Set output format [render, json, csv, yaml]");
    println!("-s,  --save <path>           Save --query result as a CSV file");
    println!("-S,  --server                Start a JSONL server on stdin/stdout");
    println!("-a,  --analysis              Print Query analysis");
    println!("-h,  --help                  Print Sheetql help");
    println!("-v,  --version               Print Sheetql Current Version");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(parts: &[&str]) -> Vec<String> {
        let mut full: Vec<String> = vec!["sheetql".to_string()];
        full.extend(parts.iter().map(|part| part.to_string()));
        full
    }

    #[test]
    fn query_mode_without_save() {
        match parse_arguments(&args(&["-f", "data.csv", "-q", "SELECT 1"])) {
            Command::QueryMode(query, arguments) => {
                assert_eq!(query, "SELECT 1");
                assert!(arguments.output_file.is_none());
            }
            other => panic!("expected QueryMode, got {other:?}"),
        }
    }

    #[test]
    fn query_mode_with_save_sets_output_file() {
        match parse_arguments(&args(&[
            "-f", "data.csv", "-q", "SELECT 1", "-s", "out.csv",
        ])) {
            Command::QueryMode(query, arguments) => {
                assert_eq!(query, "SELECT 1");
                assert_eq!(arguments.output_file.as_deref(), Some("out.csv"));
            }
            other => panic!("expected QueryMode, got {other:?}"),
        }
    }

    #[test]
    fn save_without_query_is_rejected() {
        match parse_arguments(&args(&["-f", "data.csv", "-s", "out.csv"])) {
            Command::Error(message) => {
                assert!(
                    message.contains("--save requires --query"),
                    "got: {message}"
                )
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn repl_mode_without_query() {
        assert!(matches!(
            parse_arguments(&args(&["-f", "data.csv"])),
            Command::ReplMode(_)
        ));
    }

    #[test]
    fn server_mode_flag() {
        assert!(matches!(
            parse_arguments(&args(&["-f", "data.csv", "-S"])),
            Command::ServerMode(_)
        ));
        assert!(matches!(
            parse_arguments(&args(&["-f", "data.csv", "--server"])),
            Command::ServerMode(_)
        ));
    }

    #[test]
    fn file_patterns_are_expanded_and_deduplicated() {
        match parse_arguments(&args(&["-f", "test_data/*.csv", "test_data/orders.csv"])) {
            Command::ReplMode(arguments) => {
                assert_eq!(arguments.files.len(), 3);
                assert!(arguments.files.iter().all(|file| file.ends_with(".csv")));
                assert_eq!(
                    arguments
                        .files
                        .iter()
                        .filter(|file| file.ends_with("orders.csv"))
                        .count(),
                    1
                );
            }
            other => panic!("expected ReplMode, got {other:?}"),
        }
    }

    #[test]
    fn unmatched_file_pattern_is_rejected() {
        match parse_arguments(&args(&["-f", "test_data/*.does-not-exist"])) {
            Command::Error(message) => assert!(message.contains("matched no files")),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn output_format_is_parsed() {
        match parse_arguments(&args(&["-f", "data.csv", "-q", "SELECT 1", "-o", "json"])) {
            Command::QueryMode(_, arguments) => {
                assert_eq!(arguments.output_format, OutputFormat::Json)
            }
            other => panic!("expected QueryMode, got {other:?}"),
        }
    }

    #[test]
    fn missing_files_is_rejected() {
        assert!(matches!(
            parse_arguments(&args(&["-q", "SELECT 1"])),
            Command::Error(_)
        ));
    }

    #[test]
    fn page_size_must_be_positive() {
        match parse_arguments(&args(&["-f", "data.csv", "-p", "-ps", "0"])) {
            Command::Error(message) => assert!(message.contains("greater than zero")),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn help_is_recognized() {
        assert!(matches!(parse_arguments(&args(&["--help"])), Command::Help));
        assert!(matches!(parse_arguments(&args(&["-h"])), Command::Help));
    }

    #[test]
    fn version_is_recognized() {
        assert!(matches!(
            parse_arguments(&args(&["--version"])),
            Command::Version
        ));
    }
}
