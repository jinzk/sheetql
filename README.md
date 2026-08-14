<h1 align="center">Sheetql - Spreadsheet Query Language</h1></br>

<p align="center">
Sheetql is a tool that allows you to run SQL-like queries on <code>xls</code>, <code>xlsx</code> and <code>csv</code> files.
It parses queries with <a href="https://github.com/apache/datafusion-sqlparser-rs">sqlparser-rs</a> (MySQL dialect) and executes them on in-memory tables with a custom engine, so you can query spreadsheets without a database.
</p>

---

### Installation

- Build from source code

```
git clone https://github.com/jinzk/sheetql.git
cd sheetql
cargo build
```

---

### Usage

```
Sheetql is a SQL like query language to run on xls, xlsx and csv files
Usage: Sheetql [OPTIONS]

Options:
  -f,  --files <paths>        Paths to xls/xlsx/csv files to query
  -q,  --query <SQL Query>    Sheetql query to run on selected files
  -p,  --pagination           Enable print result with pagination
  -ps, --pagesize             Set pagination page size [default: 10]
  -o,  --output               Set output format [render, json, csv, yaml]
  -s,  --save <path>          Save --query result as a CSV file
  -a,  --analysis             Print Query analysis
  -h,  --help                 Print Sheetql help
  -v,  --version              Print Sheetql Current Version
```

Option details:

| Option | Description |
| ------ | ----------- |
| `-f, --files` | One or more paths to `xls` / `xlsx` / `xlsm` / `csv` files. Pass multiple files separated by spaces. |
| `-q, --query` | Run a single query and exit. Without it, Sheetql starts an interactive REPL. |
| `-p, --pagination` | Print large results page by page (table format only). |
| `-ps, --pagesize` | Number of rows per page when `-p` is enabled. Defaults to `10`. |
| `-o, --output` | Output format: `render` (default), `json`, `csv`, `yaml`. |
| `-s, --save` | Write the `--query` result to `<path>` as CSV. Only valid with `--query` (REPL prints an error). |
| `-a, --analysis` | Print the row count and execution time after the result. |
| `-h, --help` | Print help. |
| `-v, --version` | Print the current version. |

Run a single query:

```sh
sheetql -f data/sales.csv -q "SELECT * FROM sales"
```

Run an interactive REPL (type `exit` to quit):

```sh
sheetql -f data/sales.csv data/customers.csv
```

Pipe queries through stdin (prompt is hidden when stdin is not a terminal):

```sh
printf "USE data_sales_csv\nSHOW TABLES\n" | sheetql -f data/sales.csv data/report.xlsx
```

---

### Example

Given the following files:

```
data/sales.csv
data/customers.csv
data/report.xlsx   (sheets: "Sales", "Sheet 2")
```

Each file is loaded as one database; each sheet (or a single CSV) is a table inside that database:

| File               | Sheet   | Database            | Table     |
| ------------------ | ------- | ------------------- | --------- |
| `data/sales.csv`   | -       | `data_sales_csv`    | `sales`   |
| `data/customers.csv` | -     | `data_customers_csv` | `customers` |
| `data/report.xlsx` | Sales   | `data_report_xlsx`  | `sales`   |
| `data/report.xlsx` | Sheet 2 | `data_report_xlsx`  | `sheet_2` |

Sample queries:

```sql
SHOW DATABASES

SELECT * FROM sales                          -- unqualified; resolved across databases
SELECT * FROM data_sales_csv.sales           -- qualified: database.table
SELECT * FROM data_report_xlsx.sheet_2
SELECT name, age FROM sales WHERE age >= 30 ORDER BY age DESC
SELECT city, COUNT(*) AS cnt FROM sales GROUP BY city HAVING COUNT(*) > 1
SELECT DISTINCT city FROM sales

SELECT c.name, o.amount
FROM customers AS c
JOIN data_report_xlsx.sales AS o ON c.id = o.customer_id

SELECT name, LOWER(name) AS lower_name FROM sales WHERE name LIKE "A%"
SELECT CASE WHEN age >= 35 THEN "senior" ELSE "junior" END AS tier FROM sales

DESCRIBE sales
```

All keywords and identifiers are case-insensitive, similar to SQL.

---

### Database and table naming

Each file becomes a database whose name is the sanitized full path (as given, including the extension), lowercased. Inside a database, a CSV has one table named after the file stem; a spreadsheet has one table per sheet, named after the sheet.

| Input                      | Database              | Table                   |
| -------------------------- | --------------------- | ----------------------- |
| `data/sales.csv`           | `data_sales_csv`      | `sales`                 |
| `C:\data\sales.csv`        | `c__data_sales_csv`   | `sales`                 |
| `data/report.xlsx` (Sales) | `data_report_xlsx`    | `sales`                 |

Rules:

- Path separators (`\` and `/`) and dots are replaced with `_`.
- Other characters that are not letters or digits are replaced with `_`.
- Names are lowercased.
- Consecutive `_` are **not** merged.
- A leading digit is prefixed with `_`.
- If a generated name collides with an existing one, a numeric suffix (`_1`, `_2`, ...) is appended.

### Databases, tables and `USE`

- `SHOW DATABASES` lists every loaded file as a database.
- A table is referenced as `table` (resolved against the current database first, then every other database) or `database.table`.
- `USE <database>` selects the current database (persists across REPL queries). With a single loaded file the current database is selected automatically.
- An unqualified table name matching tables in multiple databases is rejected as ambiguous; qualify it as `database.table`.

### Column naming

The first row of each sheet / CSV file is treated as the header.

- Headers are sanitized with the same rules as table names.
- Empty headers become `col1`, `col2`, ... based on position.
- Duplicate headers get a numeric suffix (`name`, `name_1`, `name_2`, ...).

### Data types

Cell values are inferred automatically. CSV cells are parsed as `Integer`, `Float`, `Boolean` (`true` / `false`) or `Text`; empty cells and spreadsheet errors become `NULL`. Spreadsheet cells keep their native type.

| Type    | Examples            |
| ------- | ------------------- |
| Integer | `42`, `-7`          |
| Float   | `3.14`, `1.5e3`     |
| Boolean | `true`, `false`     |
| Text    | `Alice`, `NY`       |
| NULL    | empty cells         |

`DESCRIBE <table>` shows each column with its inferred type.

---

### Supported SQL features

- `SELECT` projections, `*`, `table.*` and column aliases (`AS`)
- `FROM` with multiple tables and `JOIN` / `LEFT JOIN` / `RIGHT JOIN` / `FULL OUTER JOIN` / `CROSS JOIN`, including comma-separated tables, with `ON` and `USING`
- `WHERE` with comparisons, `AND` / `OR` / `NOT`, `LIKE` / `ILIKE`, `IN`, `BETWEEN`, `IS NULL`, `IS TRUE` / `IS FALSE`, `CASE`, `CAST`
- `GROUP BY` with aggregate functions, and `HAVING`
- `ORDER BY` with `ASC` / `DESC`
- `LIMIT` / `OFFSET` and `SELECT DISTINCT`
- Metadata commands: `SHOW DATABASES` (alias `SHOW SCHEMAS`), `SHOW TABLES [FROM <database>]`, `SHOW COLUMNS FROM <table>`, `DESCRIBE <table>`, `USE <database>`; `SHOW DATABASES` / `SHOW TABLES` support `LIKE '<pattern>'` with `%` / `_` wildcards

Not yet supported: subqueries, `UNION` / set operations, window functions, `INSERT` / `UPDATE` / `DELETE` (Sheetql is read-only).

### Scalar functions

| Function | Arguments | Description |
| -------- | --------- | ----------- |
| `LEN` / `LENGTH` | 1 | Length of a text value |
| `LOWER` / `LCASE` | 1 | Convert to lowercase |
| `UPPER` / `UCASE` | 1 | Convert to uppercase |
| `TRIM` | 1 | Trim leading and trailing whitespace |
| `CONCAT` | 2+ | Concatenate values |
| `SUBSTRING` / `SUBSTR` | 2 or 3 | Extract a substring (`text, start[, length]`, 1-based) |
| `REPLACE` | 3 | Replace occurrences of a substring |
| `LEFT` | 2 | First `n` characters of a text value |
| `RIGHT` | 2 | Last `n` characters of a text value |
| `INSTR` | 2 | 1-based position of a substring (0 if not found) |
| `STARTSWITH` / `ENDSWITH` | 2 | Whether text starts/ends with the given prefix/suffix (returns boolean) |
| `SPLIT` | 3 | Split text by a separator and return the n-th part (1-based; `NULL` if out of range) |
| `ABS` | 1 | Absolute value |
| `ROUND` | 1 or 2 | Round a number, optionally to `n` decimal places |
| `FLOOR` | 1 | Round down |
| `CEIL` / `CEILING` | 1 | Round up |
| `MOD` | 2 | Remainder of integer division |
| `POWER` / `POW` | 2 | Raise a number to a power |
| `SQRT` | 1 | Square root |
| `GREATEST` | 2+ | Largest value among arguments |
| `LEAST` | 2+ | Smallest value among arguments |
| `NOW` / `CURRENT_TIMESTAMP` | 0 | Current local date and time |
| `DATE` | 0 or 1 | Current date, or the date part of a value |
| `IFNULL` / `ISNULL` | 2 | First value if not `NULL`, otherwise the second |
| `COALESCE` | 2+ | First non-`NULL` value |

### Aggregate functions

| Function | Description |
| -------- | ----------- |
| `COUNT(*)` / `COUNT(expr)` | Number of rows / non-`NULL` values |
| `SUM(expr)` | Sum of numeric values |
| `AVG(expr)` | Average of numeric values |
| `MIN(expr)` | Minimum value |
| `MAX(expr)` | Maximum value |

---

### Output formats

Use `-o <format>` to change the output. All examples below query the `sales` table with `SELECT name, age FROM sales WHERE city = "NY"`.

Table (default, `render`):

```
╭───────┬─────╮
│name │age│
╞═══════╪═════╡
│Alice│30 │
├───────┼─────┤
│ Dan │40 │
╰───────┴─────╯
```

JSON (`-o json`):

```json
[
  { "age": 30, "name": "Alice" },
  { "age": 40, "name": "Dan" }
]
```

CSV (`-o csv`):

```
name,age
Alice,30
Dan,40
```

YAML (`-o yaml`):

```yaml
- age: 30
  name: Alice
- age: 40
  name: Dan
```

---

### Saving query results to a file

There are two ways to write a result set to disk as CSV.

**1. `--save` / `-s` (command-line flag)**

`-s <path>` writes the result of a `--query` to `<path>` as CSV (with a header row). It only works together with `--query`; using it in REPL mode prints an error (`--save requires --query`). The terminal still prints the result as usual.

```sh
sheetql -f data/sales.csv -q "SELECT name, city FROM sales" -s result.csv
```

`result.csv` will contain:

```
name,city
Alice,NY
Bob,LA
```

**2. `INTO OUTFILE` (SQL clause)**

Append `INTO OUTFILE 'path'` to a `SELECT` statement to write the result as CSV. MySQL-style `INTO OUTFILE` is not understood by sqlparser, so Sheetql strips it before parsing and then writes the file. If the file already exists the query fails with `Output file 'path' already exists` (it never overwrites). The query itself returns a single `Status` row confirming the write.

```sql
SELECT name, city FROM sales ORDER BY name INTO OUTFILE 'result.csv'
```

Both options always produce CSV (header + comma-separated rows), regardless of `-o`.

---

### License
```
MIT License

Copyright (c) 2026 jinzk

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```
