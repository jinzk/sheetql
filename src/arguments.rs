use crate::printer::OutputFormat;

#[derive(Debug, PartialEq)]
pub struct Arguments {
    pub files: Vec<String>,
    pub analysis: bool,
    pub pagination: bool,
    pub page_size: usize,
    pub output_format: OutputFormat,
    pub output_file: Option<String>,
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
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum Command {
    ReplMode(Arguments),
    QueryMode(String, Arguments),
    Help,
    Version,
    Error(String),
}

pub fn parse_arguments(args: &[String]) -> Command {
    if args.iter().any(|argument| argument == "--help" || argument == "-h") {
        return Command::Help;
    }

    if args.iter().any(|argument| argument == "--version" || argument == "-v") {
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
                if arg_index >= args.len() {
                    return Command::Error(format!(
                        "Argument {arg} must be followed by one or more file paths"
                    ));
                }

                loop {
                    if arg_index >= args.len() {
                        break;
                    }

                    let file = &args[arg_index];
                    if !file.starts_with('-') {
                        arguments.files.push(file.to_string());
                        arg_index += 1;
                        continue;
                    }

                    break;
                }
            }
            "--query" | "-q" => {
                arg_index += 1;
                if arg_index >= args.len() {
                    return Command::Error(format!(
                        "Argument {arg} must be followed by the query"
                    ));
                }

                optional_query = Some(args[arg_index].to_string());
                arg_index += 1;
            }
            "--analysis" | "-a" => {
                arguments.analysis = true;
                arg_index += 1;
            }
            "--pagination" | "-p" => {
                arguments.pagination = true;
                arg_index += 1;
            }
            "--pagesize" | "-ps" => {
                arg_index += 1;
                if arg_index >= args.len() {
                    return Command::Error(format!(
                        "Argument {arg} must be followed by the page size"
                    ));
                }

                match args[arg_index].parse::<usize>() {
                    Ok(page_size) => {
                        arguments.page_size = page_size;
                        arg_index += 1;
                    }
                    Err(_) => return Command::Error("Invalid page size".to_string()),
                }
            }
            "--output" | "-o" => {
                arg_index += 1;
                if arg_index >= args.len() {
                    return Command::Error(format!(
                        "Argument {arg} must be followed by output format"
                    ));
                }

                arguments.output_format = match args[arg_index].to_lowercase().as_str() {
                    "csv" => OutputFormat::Csv,
                    "json" => OutputFormat::Json,
                    "yaml" => OutputFormat::Yaml,
                    "render" | "table" => OutputFormat::Table,
                    _ => {
                        return Command::Error(
                            "Invalid output format, expected one of: render, json, csv, yaml"
                                .to_string(),
                        )
                    }
                };
                arg_index += 1;
            }
            "--save" | "-s" => {
                arg_index += 1;
                if arg_index >= args.len() {
                    return Command::Error(format!(
                        "Argument {arg} must be followed by an output file path"
                    ));
                }
                arguments.output_file = Some(args[arg_index].to_string());
                arg_index += 1;
            }
            _ => return Command::Error(format!("Unknown argument {arg}")),
        }
    }

    if arguments.files.is_empty() {
        return Command::Error(
            "Missing file paths, use `-f` to pass one or more xls/xlsx/csv files".to_string(),
        );
    }

    if arguments.output_file.is_some() && optional_query.is_none() {
        return Command::Error("--save requires --query".to_string());
    }

    match optional_query {
        Some(query) => Command::QueryMode(query, arguments),
        None => Command::ReplMode(arguments),
    }
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
        match parse_arguments(&args(&["-f", "data.csv", "-q", "SELECT 1", "-s", "out.csv"])) {
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
                assert!(message.contains("--save requires --query"), "got: {message}")
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