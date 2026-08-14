use crate::printer::OutputFormat;

#[derive(Debug, PartialEq)]
pub struct Arguments {
    pub files: Vec<String>,
    pub analysis: bool,
    pub pagination: bool,
    pub page_size: usize,
    pub output_format: OutputFormat,
}

impl Arguments {
    fn new() -> Arguments {
        Arguments {
            files: vec![],
            analysis: false,
            pagination: false,
            page_size: 10,
            output_format: OutputFormat::Table,
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
            _ => return Command::Error(format!("Unknown argument {arg}")),
        }
    }

    if arguments.files.is_empty() {
        return Command::Error(
            "Missing file paths, use `-f` to pass one or more xls/xlsx/csv files".to_string(),
        );
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
    println!("-a,  --analysis              Print Query analysis");
    println!("-h,  --help                  Print Sheetql help");
    println!("-v,  --version               Print Sheetql Current Version");
}