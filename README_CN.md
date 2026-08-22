<h1 align="center">SheetQL - 电子表格查询语言</h1></br>

<p align="center">
SheetQL 是一个让你可以在 <code>xls</code>、<code>xlsx</code> 和 <code>csv</code> 文件上运行类 SQL 查询的工具。
它使用 <a href="https://github.com/apache/datafusion-sqlparser-rs">sqlparser-rs</a>（MySQL 方言）解析查询，并由自研引擎在内存表中执行，因此无需数据库即可查询电子表格。
</p>

---

### 安装

从源码构建

```
git clone https://github.com/jinzk/sheetql.git
cd sheetql
cargo build
```

---

### 用法

```
SheetQL 是一种运行在 xls、xlsx 和 csv 文件上的类 SQL 查询语言
用法: sheetql [OPTIONS]

选项:
  -f,  --files <paths>        要查询的 xls/xlsx/csv 文件路径
  -q,  --query <SQL Query>    要在所选文件上运行的 Sheetql 查询
  -p,  --pagination           启用分页打印结果
  -ps, --pagesize             设置分页大小 [默认: 10]
  -o,  --output               设置输出格式 [render, json, csv, yaml]
  -s,  --save <path>          将 --query 结果保存为 CSV 文件
  -S,  --server               在 stdin/stdout 上启动 JSONL 服务器
       --export-root <path>   将服务器导出限制在此目录
       --max-rows <number>    超过此数据行数时拒绝加载输入表
       --max-file-bytes <n>   超过此字节数时拒绝加载输入文件
       --delimiter <char>     CSV 字段分隔符 [默认: ,]
       --no-header            将 CSV 行视为数据并生成列名
       --null-value <text>    将此 CSV 值视为 NULL
  -a,  --analysis             打印查询分析信息
  -h,  --help                 打印 Sheetql 帮助
  -v,  --version              打印 Sheetql 当前版本
```

选项说明:

| 选项                 | 说明                                                                                       |
| -------------------- | ------------------------------------------------------------------------------------------ |
| `-f, --files`      | 一个或多个`xls` / `xlsx` / `xlsm` / `csv` 文件路径，多个文件用空格分隔。           |
| `-q, --query`      | 运行单条查询后退出。不使用该选项时，Sheetql 启动交互式 REPL。                              |
| `-p, --pagination` | 大结果集分页打印（仅表格格式）。                                                           |
| `-ps, --pagesize`  | 启用`-p` 时每页显示的行数，默认 `10`。必须大于零。                               |
| `-o, --output`     | 输出格式：`render`（默认）、`json`、`csv`、`yaml`。                                |
| `-s, --save`       | 将`--query` 的结果以 CSV 写入 `<path>`。仅能与 `--query` 配合使用（REPL 下会报错）。 |
| `-S, --server`    | 在 stdin/stdout 上启动 JSONL 服务器，供程序化访问（见[服务器模式](#服务器模式)）。   |
| `--export-root`   | 将 `op=export` 限制在该目录内；仅能与 `--server` 配合，路径必须是相对路径且不能包含 `..`。 |
| `--max-rows`      | 单个输入表超过此数据行数时拒绝加载。默认不限制；适用于 CSV 和电子表格中的每个工作表。 |
| `--max-file-bytes` | 输入文件超过此字节数时在加载前拒绝。默认不限制。 |
| `--delimiter`      | 设置 CSV 字段分隔符，必须是一个 ASCII 字符，默认为 `,`。 |
| `--no-header`      | 将 CSV 每一行都视为数据，并生成 `col1`、`col2` 等列名。 |
| `--null-value`     | 将完全匹配的 CSV 字段值视为 `NULL`。 |
| `-a, --analysis`   | 在结果后打印行数与执行耗时。                                                               |
| `-h, --help`       | 打印帮助。                                                                                 |
| `-v, --version`    | 打印当前版本。                                                                             |

运行单条查询:

```sh
sheetql -f data/sales.csv -q "SELECT * FROM sales"
```

运行交互式 REPL（输入 `exit` 退出）:

```sh
sheetql -f data/sales.csv data/customers.csv
```

通过 stdin 管道传入查询（stdin 不是终端时隐藏提示符）:

```sh
printf "USE data_sales_csv\nSHOW TABLES\n" | sheetql -f data/sales.csv data/report.xlsx
```

---

### 示例

给定以下文件:

```
data/sales.csv
data/customers.csv
data/report.xlsx   (工作表: "Sales", "Sheet 2")
```

每个文件作为一个数据库加载；每个工作表（或单个 CSV）是该数据库中的一张表:

| 文件                   | 工作表  | 数据库                 | 表            |
| ---------------------- | ------- | ---------------------- | ------------- |
| `data/sales.csv`     | -       | `data_sales_csv`     | `sales`     |
| `data/customers.csv` | -       | `data_customers_csv` | `customers` |
| `data/report.xlsx`   | Sales   | `data_report_xlsx`   | `sales`     |
| `data/report.xlsx`   | Sheet 2 | `data_report_xlsx`   | `sheet_2`   |

示例查询:

```sql
SHOW DATABASES

SELECT * FROM sales                          -- 未限定：跨数据库解析
SELECT * FROM data_sales_csv.sales           -- 限定：数据库.表
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

所有关键字与标识符均不区分大小写，与 SQL 一致。

---

### 数据库与表的命名

每个文件成为一个数据库，库名为净化后的完整路径（按传入内容、含扩展名），并转为小写。数据库内，CSV 只有一张表，以文件主名命名；电子表格每个工作表一张表，以工作表名命名。

| 输入                         | 数据库                | 表        |
| ---------------------------- | --------------------- | --------- |
| `data/sales.csv`           | `data_sales_csv`    | `sales` |
| `C:\data\sales.csv`        | `c__data_sales_csv` | `sales` |
| `data/report.xlsx` (Sales) | `data_report_xlsx`  | `sales` |

规则:

- 路径分隔符（`\` 和 `/`）与点号替换为 `_`。
- 其他非字母或数字的字符替换为 `_`。
- 名称转为小写。
- 连续的 `_` **不会**合并。
- 以数字开头的名称前加 `_` 前缀。
- 若生成的名称与已有名称冲突，追加数字后缀（`_1`、`_2`、…）。

### 数据库、表与 `USE`

- `SHOW DATABASES` 将每个已加载文件作为一个数据库列出。
- 表通过 `table`（先在当前数据库解析，再到其他数据库）或 `database.table` 引用。
- `USE <database>` 选择当前数据库（在 REPL 中跨查询持久生效）。只加载单个文件时自动选中当前数据库。
- 未限定的表名若在多个数据库中都匹配，会被判定为歧义并拒绝；请使用 `database.table` 限定。

### 列命名

每个工作表 / CSV 文件的第一行视为表头。

- 表头使用与表名相同的规则净化。
- 空表头按位置命名为 `col1`、`col2`、…。
- 重复表头追加数字后缀（`name`、`name_2`、`name_3`、…）。

### 数据类型

单元格值自动推断。CSV 单元格解析为 `Integer`、`Float`、`Boolean`（`true` / `false`）或 `Text`；空单元格与电子表格错误值变为 `NULL`。电子表格日期表示为 `Date` 或 `DateTime`。

| 类型    | 示例                |
| ------- | ------------------- |
| Integer | `42`, `-7`      |
| Float   | `3.14`, `1.5e3` |
| Boolean | `true`, `false` |
| Text    | `Alice`, `NY`   |
| Date    | `2026-08-14` |
| DateTime | `2026-08-14 10:30:00` |
| NULL    | 空单元格            |

边界情况:

- 超出 `i64` 范围的纯整数（如 `12345678901234567890`）存储为 `Text`，避免精度静默丢失。
- 非有限浮点数（`NaN`、`inf`、`-inf`）存储为 `Text`。
- CSV 行的字段数多于表头时拒绝并报错（包含行号）。
- CSV 行的字段数少于表头时用 `NULL` 填充。

`DESCRIBE <table>` 显示每列及其推断类型。

---

### 支持的 SQL 特性

- `SELECT` 投影、`*`、`table.*` 与列别名（`AS`）
- `FROM` 支持多表与 `JOIN` / `LEFT JOIN` / `RIGHT JOIN` / `FULL OUTER JOIN` / `CROSS JOIN`，含逗号分隔的表，以及 `ON` 与 `USING`
- `WHERE` 支持比较、`AND` / `OR` / `NOT`、`LIKE` / `ILIKE`、`IN`、`BETWEEN`、`IS NULL`、`IS TRUE` / `IS FALSE`、`CASE`、`CAST`。`LIKE` / `ILIKE` 支持 `%`（任意字符序列）与 `_`（单个字符）通配符，并可选用 `ESCAPE '<char>'` 子句转义；当任一操作数为 `NULL` 时结果为 `NULL`（而非 `false`）。`AND` / `OR` 短路求值：左侧已确定结果时不计算右侧。`IN` / `NOT IN` 列表中含 `NULL` 且值不匹配时返回 `NULL`（而非 `false`）。
- `GROUP BY` 配合聚合函数，以及 `HAVING`
- `ORDER BY` 支持 `ASC` / `DESC`
- `LIMIT` / `OFFSET` 与 `SELECT DISTINCT`
- 元数据命令：`SHOW DATABASES`（别名 `SHOW SCHEMAS`）、`SHOW TABLES [FROM <database>]`、`SHOW COLUMNS FROM <table>`、`DESCRIBE <table>`、`USE <database>`；`SHOW DATABASES` / `SHOW TABLES` 支持 `LIKE '<pattern>'`，通配符为 `%` / `_`

暂不支持：子查询、`UNION` / 集合运算、窗口函数、`INSERT` / `UPDATE` / `DELETE`（Sheetql 为只读工具）。

### 交互式 REPL

不带 `-q` 运行 `sheetql -f <files>` 会启动交互式 TUI，提供:

- **SQL 语法高亮** — 关键字、字符串、数字和注释以不同颜色显示。
- **自动补全** — 输入部分单词后弹出候选列表，匹配关键字、函数、表名和列名。按 `Tab` 或 `Enter` 接受，`Up`/`Down` 导航，`Esc` 关闭。关键字始终以大写插入。
- **历史记录** — `Up`/`Down` 箭头在历史查询中切换（弹窗未打开时）。
- **管道输入** — 可通过 stdin 管道传入查询；stdin 非终端时隐藏提示符。

输入 `exit` 或 `quit`（不区分大小写）退出 REPL。

### 标量函数

| 函数                            | 参数   | 说明                                                             |
| ------------------------------- | ------ | ---------------------------------------------------------------- |
| `LEN` / `LENGTH`            | 1      | 文本长度                                                         |
| `LOWER` / `LCASE`           | 1      | 转为小写                                                         |
| `UPPER` / `UCASE`           | 1      | 转为大写                                                         |
| `TRIM`                        | 1      | 去除首尾空白                                                     |
| `CONCAT`                      | 2+     | 拼接值                                                           |
| `SUBSTRING` / `SUBSTR`      | 2 或 3 | 提取子串（`文本, 起始[, 长度]`，从 1 开始）                    |
| `REPLACE`                     | 3      | 替换子串出现                                                     |
| `LEFT`                        | 2      | 返回文本前`n` 个字符                                           |
| `RIGHT`                       | 2      | 返回文本后`n` 个字符                                           |
| `INSTR`                       | 2      | 返回子串位置（从 1 开始，未找到返回 0）                          |
| `STARTSWITH` / `ENDSWITH`   | 2      | 文本是否以给定前缀/后缀开头/结尾（返回布尔）                     |
| `SPLIT`                       | 3      | 按分隔符拆分文本并返回第 n 段（从 1 开始；超出范围返回`NULL`） |
| `ABS`                         | 1      | 绝对值                                                           |
| `ROUND`                       | 1 或 2 | 四舍五入，可指定小数位`n`                                      |
| `FLOOR`                       | 1      | 向下取整                                                         |
| `CEIL` / `CEILING`          | 1      | 向上取整                                                         |
| `MOD`                         | 2      | 整数除法余数                                                     |
| `POWER` / `POW`             | 2      | 求幂                                                             |
| `SQRT`                        | 1      | 平方根                                                           |
| `GREATEST`                    | 2+     | 返回参数中的最大值                                               |
| `LEAST`                       | 2+     | 返回参数中的最小值                                               |
| `NOW` / `CURRENT_TIMESTAMP` | 0      | 当前本地日期和时间                                               |
| `DATE`                        | 0 或 1 | 当前日期，或提取值的日期部分                                     |
| `IFNULL` / `ISNULL`         | 2      | 第一个值非`NULL` 时返回它，否则返回第二个                      |
| `COALESCE`                    | 2+     | 返回第一个非`NULL` 值                                          |

### 聚合函数

| 函数                              | 说明                                                 |
| --------------------------------- | ---------------------------------------------------- |
| `COUNT(*)` / `COUNT(expr)`    | 行数 / 非`NULL` 值个数                            |
| `COUNT(DISTINCT expr)`         | 去重后非`NULL` 值个数（数值类型自动统一 Int/Float） |
| `SUM(expr)`                     | 数值之和（溢出时报错）                               |
| `AVG(expr)`                     | 数值平均值                                           |
| `MIN(expr)`                     | 最小值                                               |
| `MAX(expr)`                     | 最大值                                               |

---

### 输出格式

使用 `-o <format>` 切换输出。以下所有示例均查询 `sales` 表：`SELECT name, age FROM sales WHERE city = "NY"`。

表格（默认，`render`）:

```
╭───────┬─────╮
│ name  │ age │
╞═══════╪═════╡
│ Alice │ 30  │
├───────┼─────┤
│  Dan  │ 40  │
╰───────┴─────╯
```

JSON（`-o json`）:

```json
[
  { "age": 30, "name": "Alice" },
  { "age": 40, "name": "Dan" }
]
```

CSV（`-o csv`）:

```
name,age
Alice,30
Dan,40
```

YAML（`-o yaml`）:

```yaml
- age: 30
  name: Alice
- age: 40
  name: Dan
```

---

### 将查询结果保存到文件

有两种方式可将结果集以 CSV 写入磁盘。

**1. `--save` / `-s`（命令行参数）**

`-s <path>` 将 `--query` 的结果以 CSV（含表头）写入 `<path>`。它只能与 `--query` 配合使用；在 REPL 模式下使用会报错（`--save requires --query`）。终端仍会照常打印结果。

```sh
sheetql -f data/sales.csv -q "SELECT name, city FROM sales" -s result.csv
```

`result.csv` 内容：

```
name,city
Alice,NY
Bob,LA
```

**2. `INTO OUTFILE`（SQL 子句）**

在 `SELECT` 语句后追加 `INTO OUTFILE 'path'` 即可将结果写成 CSV。由于 sqlparser 不解析 MySQL 风格的 `INTO OUTFILE`，Sheetql 会在解析前将其剥离，再写出文件。若文件已存在，查询会失败并返回 `Output file 'path' already exists`（绝不覆盖）。查询本身会返回一行 `Status` 以确认写入。

```sql
SELECT name, city FROM sales ORDER BY name INTO OUTFILE 'result.csv'
```

两种方式均始终输出 CSV（表头 + 逗号分隔的行），与 `-o` 无关。

---

### 服务器模式

`-S, --server` 在 stdin/stdout 上启动一个持久的 JSONL 服务器。stdin 的每一行是一条 JSON 请求；每条响应都是一个 JSON 对象，打印到 stdout 并逐条 flush。文件在服务器启动时加载一次，之后常驻内存供多次请求使用。`--server` 不能与 `--query` 或 `--save` 同时使用。

```sh
sheetql -f data/sales.csv data/customers.csv --server
```

限制导出只能写入指定目录：

```sh
sheetql -f data/sales.csv --server --export-root ./exports
```

请求与响应:

| 请求 | 说明 |
| --- | --- |
| `{ "op": "query", "sql": "...", "db": "…", "format": "json" }` | 运行查询。`db` 可选，仅将本次查询限定在该数据库内（进程级 `USE` 状态不变）。`format` 控制 `text` 字段：`json`（默认）、`csv`、`yaml`、`table`。 |
| `{ "op": "list" }` | 列出所有数据库、其中的表及各表列名。 |
| `{ "op": "export", "sql": "...", "path": "out.csv", "overwrite": true }` | 运行查询并将结果写成 CSV。`overwrite` 默认 `false`；设为 `true` 才会替换已存在的文件。 |
| `{ "op": "exit" }` | 先返回 `{ "ok": true }` 再退出。stdin 关闭（EOF）时服务器也会干净退出。 |

查询成功响应:

```json
{ "ok": true, "columns": ["name","city"], "rows": [["Alice","NY"],["Bob","LA"]], "text": "…", "elapsed_ms": 2, "stats": { "elapsed_ms": 2, "input_rows": 100, "output_rows": 2 } }
```

`rows` 是二维数组。如果请求包含 `id`，响应会原样返回该值。错误包含机器可读的 `code`，例如 `{ "ok": false, "code": "query_error", "error": "…" }`，并且**不会**终止服务器。

`list` 成功响应:

```json
{ "ok": true, "data": { "current": "data_sales_csv", "databases": [ { "name": "data_sales_csv", "tables": [ { "name": "sales", "columns": ["name","city"] } ] } ] } }
```

导出成功响应:

```json
{ "ok": true, "path": "out.csv", "elapsed_ms": 3 }
```

会话示例:

```
> {"op":"list"}
< {"ok":true,"data":{"current":null,"databases":[{"name":"data_sales_csv","tables":[{"name":"sales","columns":["name","city"]}]}]}}
> {"op":"query","sql":"SELECT name, city FROM sales ORDER BY name"}
< {"ok":true,"columns":["name","city"],"rows":[["Alice","NY"],["Bob","LA"]],"text":"…","elapsed_ms":1}
> {"op":"export","sql":"SELECT * FROM sales","path":"sales.csv"}
< {"ok":true,"path":"sales.csv","elapsed_ms":1}
> {"op":"exit"}
< {"ok":true}
```

服务器在单进程内顺序处理请求，面向本地可信调用方；不含鉴权或加密。

---

### 许可证

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
