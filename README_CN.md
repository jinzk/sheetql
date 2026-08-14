<h1 align="center">Sheetql - 电子表格查询语言</h1></br>

<p align="center">
Sheetql 是一个让你可以在 <code>xls</code>、<code>xlsx</code> 和 <code>csv</code> 文件上运行类 SQL 查询的工具。
它使用 <a href="https://github.com/apache/datafusion-sqlparser-rs">sqlparser-rs</a>（MySQL 方言）解析查询，并由自研引擎在内存表中执行，因此无需数据库即可查询电子表格。
</p>

---

### 安装

从源码构建

```
git clone https://github.com/AmrDeveloper/sheetql.git
cd sheetql
cargo build
```

---

### 用法

```
Sheetql 是一种运行在 xls、xlsx 和 csv 文件上的类 SQL 查询语言
用法: Sheetql [OPTIONS]

选项:
  -f,  --files <paths>        要查询的 xls/xlsx/csv 文件路径
  -q,  --query <SQL Query>    要在所选文件上运行的 Sheetql 查询
  -p,  --pagination           启用分页打印结果
  -ps, --pagesize             设置分页大小 [默认: 10]
  -o,  --output               设置输出格式 [render, json, csv, yaml]
  -a,  --analysis             打印查询分析信息
  -h,  --help                 打印 Sheetql 帮助
  -v,  --version              打印 Sheetql 当前版本
```

选项说明:

| 选项 | 说明 |
| ---- | ---- |
| `-f, --files` | 一个或多个 `xls` / `xlsx` / `xlsm` / `csv` 文件路径，多个文件用空格分隔。 |
| `-q, --query` | 运行单条查询后退出。不使用该选项时，Sheetql 启动交互式 REPL。 |
| `-p, --pagination` | 大结果集分页打印（仅表格格式）。 |
| `-ps, --pagesize` | 启用 `-p` 时每页显示的行数，默认 `10`。 |
| `-o, --output` | 输出格式：`render`（默认）、`json`、`csv`、`yaml`。 |
| `-a, --analysis` | 在结果后打印行数与执行耗时。 |
| `-h, --help` | 打印帮助。 |
| `-v, --version` | 打印当前版本。 |

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

| 文件                  | 工作表   | 数据库              | 表        |
| --------------------- | -------- | ------------------- | --------- |
| `data/sales.csv`      | -        | `data_sales_csv`    | `sales`   |
| `data/customers.csv`  | -        | `data_customers_csv` | `customers` |
| `data/report.xlsx`    | Sales    | `data_report_xlsx`  | `sales`   |
| `data/report.xlsx`    | Sheet 2  | `data_report_xlsx`  | `sheet_2` |

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

| 输入                       | 数据库              | 表                    |
| -------------------------- | ------------------- | --------------------- |
| `data/sales.csv`           | `data_sales_csv`    | `sales`               |
| `C:\data\sales.csv`        | `c__data_sales_csv` | `sales`               |
| `data/report.xlsx` (Sales) | `data_report_xlsx`  | `sales`               |

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
- 重复表头追加数字后缀（`name`、`name_1`、`name_2`、…）。

### 数据类型

单元格值自动推断。CSV 单元格解析为 `Integer`、`Float`、`Boolean`（`true` / `false`）或 `Text`；空单元格与电子表格错误值变为 `NULL`。电子表格单元格保留其原生类型。

| 类型    | 示例              |
| ------- | ----------------- |
| Integer | `42`, `-7`        |
| Float   | `3.14`, `1.5e3`   |
| Boolean | `true`, `false`   |
| Text    | `Alice`, `NY`     |
| NULL    | 空单元格          |

`DESCRIBE <table>` 显示每列及其推断类型。

---

### 支持的 SQL 特性

- `SELECT` 投影、`*`、`table.*` 与列别名（`AS`）
- `FROM` 支持多表与 `JOIN` / `LEFT JOIN` / `RIGHT JOIN` / `FULL OUTER JOIN` / `CROSS JOIN`，含逗号分隔的表，以及 `ON` 与 `USING`
- `WHERE` 支持比较、`AND` / `OR` / `NOT`、`LIKE` / `ILIKE`、`IN`、`BETWEEN`、`IS NULL`、`IS TRUE` / `IS FALSE`、`CASE`、`CAST`
- `GROUP BY` 配合聚合函数，以及 `HAVING`
- `ORDER BY` 支持 `ASC` / `DESC`
- `LIMIT` / `OFFSET` 与 `SELECT DISTINCT`
- 元数据命令：`SHOW DATABASES`（别名 `SHOW SCHEMAS`）、`SHOW TABLES [FROM <database>]`、`SHOW COLUMNS FROM <table>`、`DESCRIBE <table>`、`USE <database>`；`SHOW DATABASES` / `SHOW TABLES` 支持 `LIKE '<pattern>'`，通配符为 `%` / `_`

暂不支持：子查询、`UNION` / 集合运算、窗口函数、`INSERT` / `UPDATE` / `DELETE`（Sheetql 为只读工具）。

### 标量函数

| 函数 | 参数 | 说明 |
| ---- | ---- | ---- |
| `LEN` / `LENGTH` | 1 | 文本长度 |
| `LOWER` / `LCASE` | 1 | 转为小写 |
| `UPPER` / `UCASE` | 1 | 转为大写 |
| `TRIM` | 1 | 去除首尾空白 |
| `CONCAT` | 2+ | 拼接值 |
| `SUBSTRING` / `SUBSTR` | 2 或 3 | 提取子串（`文本, 起始[, 长度]`，从 1 开始） |
| `REPLACE` | 3 | 替换子串出现 |
| `LEFT` | 2 | 返回文本前 `n` 个字符 |
| `RIGHT` | 2 | 返回文本后 `n` 个字符 |
| `INSTR` | 2 | 返回子串位置（从 1 开始，未找到返回 0） |
| `ABS` | 1 | 绝对值 |
| `ROUND` | 1 或 2 | 四舍五入，可指定小数位 `n` |
| `FLOOR` | 1 | 向下取整 |
| `CEIL` / `CEILING` | 1 | 向上取整 |
| `MOD` | 2 | 整数除法余数 |
| `POWER` / `POW` | 2 | 求幂 |
| `SQRT` | 1 | 平方根 |
| `GREATEST` | 2+ | 返回参数中的最大值 |
| `LEAST` | 2+ | 返回参数中的最小值 |
| `NOW` / `CURRENT_TIMESTAMP` | 0 | 当前本地日期和时间 |
| `DATE` | 0 或 1 | 当前日期，或提取值的日期部分 |
| `IFNULL` / `ISNULL` | 2 | 第一个值非 `NULL` 时返回它，否则返回第二个 |
| `COALESCE` | 2+ | 返回第一个非 `NULL` 值 |

### 聚合函数

| 函数 | 说明 |
| ---- | ---- |
| `COUNT(*)` / `COUNT(expr)` | 行数 / 非 `NULL` 值个数 |
| `SUM(expr)` | 数值之和 |
| `AVG(expr)` | 数值平均值 |
| `MIN(expr)` | 最小值 |
| `MAX(expr)` | 最大值 |

---

### 输出格式

使用 `-o <format>` 切换输出。以下所有示例均查询 `sales` 表：`SELECT name, age FROM sales WHERE city = "NY"`。

表格（默认，`render`）:

```
╭───────┬─────╮
│name │age│
╞═══════╪═════╡
│Alice│30 │
├───────┼─────┤
│ Dan │40 │
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