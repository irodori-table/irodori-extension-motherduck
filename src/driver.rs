use std::collections::{BTreeMap, HashMap};
use std::sync::{Mutex, OnceLock};

use serde_json::{json, Value};

use crate::abi::{self, IrodoriConnectorBuffer};
use crate::{ABI_VERSION, CONFIG_JSON, ENGINE, MANIFEST_JSON};

static CONNECTIONS: OnceLock<Mutex<HashMap<String, duckdb::Connection>>> = OnceLock::new();

#[derive(Default)]
struct ObjectMeta {
    schema: String,
    name: String,
    kind: String,
    columns: Vec<Value>,
}

type QueryRows = Vec<Vec<Value>>;
type QueryOutput = (Vec<String>, QueryRows, bool);

fn connections() -> &'static Mutex<HashMap<String, duckdb::Connection>> {
    CONNECTIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn call_json(request: IrodoriConnectorBuffer) -> IrodoriConnectorBuffer {
    let request = match abi::parse_request(request) {
        Ok(request) => request,
        Err(response) => return response,
    };
    let method = match abi::request_method(request.as_ref()) {
        Ok(method) => method,
        Err(response) => return response,
    };

    match method {
        "health" | "ping" => abi::ok(serde_json::Map::from_iter([
            ("engine".to_string(), Value::String(ENGINE.to_string())),
            ("abiVersion".to_string(), json!(ABI_VERSION)),
            ("driverLinked".to_string(), Value::Bool(true)),
        ])),
        "describe" | "capabilities" => abi::ok(serde_json::Map::from_iter([
            ("engine".to_string(), Value::String(ENGINE.to_string())),
            ("abiVersion".to_string(), json!(ABI_VERSION)),
            ("driverLinked".to_string(), Value::Bool(true)),
            (
                "manifest".to_string(),
                serde_json::from_str(MANIFEST_JSON).unwrap_or(Value::Null),
            ),
            (
                "config".to_string(),
                serde_json::from_str(CONFIG_JSON).unwrap_or(Value::Null),
            ),
        ])),
        "manifest" => abi::owned_buffer(MANIFEST_JSON.to_string()),
        "config" => abi::owned_buffer(CONFIG_JSON.to_string()),
        "connect" => connect(request.as_ref().expect("connect has request")),
        "query" => query(request.as_ref().expect("query has request")),
        "metadata" => metadata(request.as_ref().expect("metadata has request")),
        "close" => close(request.as_ref().expect("close has request")),
        other => abi::error(
            "connector.unknownMethod",
            format!("unknown connector method: {other}"),
        ),
    }
}

/// What the profile is actually asking us to open.
///
/// This connector shipped as a byte-identical copy of the DuckDB one, so an
/// `md:` target was handed straight to `duckdb::Connection::open` — which does
/// not resolve MotherDuck and, on most platforms, quietly creates a *local file
/// named `md:whatever`*. The token the user typed was never read at all.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Target {
    InMemory,
    LocalFile(String),
    /// A MotherDuck database, with the part after `md:` (empty means the
    /// account's default database).
    MotherDuck(String),
}

impl Target {
    fn from_request(request: &Value) -> Self {
        let raw = option_string(request, &["database", "url", "connectionString", "dsn"])
            .unwrap_or_default();
        let raw = raw.trim();
        match raw {
            "" | ":memory:" => Self::InMemory,
            _ => match motherduck_database(raw) {
                Some(database) => Self::MotherDuck(database),
                None => Self::LocalFile(raw.to_string()),
            },
        }
    }
}

/// The database part of a MotherDuck target, for the spellings the connection
/// form offers: `md:`, `md:name`, `motherduck:name`, and the URL form
/// `motherduck://token@md/name` the placeholder shows.
fn motherduck_database(raw: &str) -> Option<String> {
    // Case-insensitive: a user who types `MD:analytics` means MotherDuck, and
    // treating it as a local path is the exact failure this connector had.
    let lowered = raw.to_ascii_lowercase();
    for prefix in ["md:", "motherduck:"] {
        if lowered.starts_with(prefix) {
            // Slice the original, not the lowercased copy — a database name is
            // case-sensitive even though its scheme is not.
            let rest = &raw[prefix.len()..];
            let rest = rest.trim_start_matches('/');
            // `motherduck://token@md/name`
            if let Some((_, after_at)) = rest.split_once('@') {
                let name = after_at.trim_start_matches("md").trim_start_matches('/');
                return Some(name.to_string());
            }
            return Some(rest.to_string());
        }
    }
    None
}

/// The MotherDuck service token, or `None` when the profile carries none.
///
/// The connection form labels this engine's password box "MotherDuck token", so
/// `password` is where a token entered through the UI arrives. The environment
/// variable is MotherDuck's own convention and stays as the last resort.
fn motherduck_token(request: &Value) -> Option<String> {
    option_string(
        request,
        &["motherduckToken", "motherduck_token", "token", "password"],
    )
    .or_else(|| std::env::var("motherduck_token").ok())
    .map(|value| value.trim().to_string())
    .filter(|value| !value.is_empty())
}

/// Resolve a field from anywhere the host may put it.
///
/// `abi::profile_field` looks at the request and its `profile`, but connector
/// options arrive under `profile.options` — so a token entered as a connector
/// option would be invisible to it. This walks the same containers the other
/// connectors' `option_string` does.
fn option_string(request: &Value, fields: &[&str]) -> Option<String> {
    let containers = [
        Some(request),
        request.get("profile"),
        request.get("options"),
        request.get("secrets"),
        request.get("profile").and_then(|p| p.get("options")),
        request.get("profile").and_then(|p| p.get("secrets")),
    ];
    containers.into_iter().flatten().find_map(|container| {
        fields.iter().find_map(|field| {
            container
                .get(*field)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned)
        })
    })
}

/// Open a MotherDuck database: load the extension, apply the token, then attach.
///
/// The token is set through DuckDB's settings rather than embedded in the
/// connection string so it never appears in an error message that quotes the
/// target.
fn open_motherduck(database: &str, token: Option<&str>) -> Result<duckdb::Connection, String> {
    let conn = duckdb::Connection::open_in_memory()
        .map_err(|err| format!("MotherDuck connect failed: {err}"))?;
    conn.execute_batch("install motherduck; load motherduck;")
        .map_err(|err| format!("loading the MotherDuck extension failed: {err}"))?;
    if let Some(token) = token {
        conn.execute_batch(&format!("set motherduck_token = {}", sql_string(token)))
            .map_err(|_| "applying the MotherDuck token failed.".to_string())?;
    }
    let attach = if database.is_empty() {
        "attach 'md:'".to_string()
    } else {
        format!("attach {}", sql_string(&format!("md:{database}")))
    };
    conn.execute_batch(&attach)
        .map_err(|err| format!("attaching the MotherDuck database failed: {err}"))?;
    Ok(conn)
}

fn sql_string(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn connect(request: &Value) -> IrodoriConnectorBuffer {
    let connection_id = abi::connection_id(Some(request));
    let target = Target::from_request(request);
    let conn = match &target {
        Target::InMemory => {
            duckdb::Connection::open_in_memory().map_err(|err| format!("connect failed: {err}"))
        }
        Target::LocalFile(path) => {
            duckdb::Connection::open(path).map_err(|err| format!("connect failed: {err}"))
        }
        Target::MotherDuck(database) => {
            let token = motherduck_token(request);
            if token.is_none() {
                return abi::error(
                    "connector.invalidRequest",
                    "MotherDuck needs a service token. Enter it in the MotherDuck \
                     token field, or set the motherduck_token environment variable.",
                );
            }
            open_motherduck(database, token.as_deref())
        }
    };
    let conn = match conn {
        Ok(conn) => conn,
        Err(err) => return abi::error("connector.connectFailed", err),
    };
    let server_version = duckdb_version(&conn).unwrap_or_else(|| "unknown".to_string());
    if should_seed_sample(request, &connection_id) {
        if let Err(err) = seed_sample(&conn) {
            return abi::error("connector.seedFailed", err);
        }
    }
    let mut guard = match connections().lock() {
        Ok(guard) => guard,
        Err(_) => {
            return abi::error(
                "connector.statePoisoned",
                "Connector connection state is poisoned.",
            )
        }
    };
    guard.insert(connection_id.clone(), conn);
    abi::ok(serde_json::Map::from_iter([
        ("engine".to_string(), Value::String(ENGINE.to_string())),
        ("connectionId".to_string(), Value::String(connection_id)),
        ("serverVersion".to_string(), Value::String(server_version)),
        ("driverLinked".to_string(), Value::Bool(true)),
    ]))
}

fn query(request: &Value) -> IrodoriConnectorBuffer {
    let connection_id = abi::connection_id(Some(request));
    let Some(sql) = abi::string_field(request, "sql") else {
        return abi::error(
            "connector.invalidRequest",
            "query requires a string sql field.",
        );
    };
    let mut guard = match connections().lock() {
        Ok(guard) => guard,
        Err(_) => {
            return abi::error(
                "connector.statePoisoned",
                "Connector connection state is poisoned.",
            )
        }
    };
    let Some(conn) = guard.get_mut(&connection_id) else {
        return abi::error(
            "connector.connectionNotFound",
            format!("no open connection: {connection_id}"),
        );
    };
    match run_query(conn, sql, abi::max_rows(request)) {
        Ok((columns, rows, truncated)) => abi::ok(serde_json::Map::from_iter([
            ("connectionId".to_string(), Value::String(connection_id)),
            (
                "columns".to_string(),
                Value::Array(columns.into_iter().map(Value::String).collect()),
            ),
            (
                "rows".to_string(),
                Value::Array(
                    rows.into_iter()
                        .map(|row| Value::Array(row.into_iter().collect()))
                        .collect(),
                ),
            ),
            ("truncated".to_string(), Value::Bool(truncated)),
        ])),
        Err(err) => abi::error("connector.queryFailed", err),
    }
}

fn metadata(request: &Value) -> IrodoriConnectorBuffer {
    let connection_id = abi::connection_id(Some(request));
    let mut guard = match connections().lock() {
        Ok(guard) => guard,
        Err(_) => {
            return abi::error(
                "connector.statePoisoned",
                "Connector connection state is poisoned.",
            )
        }
    };
    let Some(conn) = guard.get_mut(&connection_id) else {
        return abi::error(
            "connector.connectionNotFound",
            format!("no open connection: {connection_id}"),
        );
    };
    match load_metadata(conn) {
        Ok(metadata) => abi::ok(serde_json::Map::from_iter([
            ("connectionId".to_string(), Value::String(connection_id)),
            ("metadata".to_string(), metadata),
        ])),
        Err(err) => abi::error("connector.metadataFailed", err),
    }
}

fn close(request: &Value) -> IrodoriConnectorBuffer {
    let connection_id = abi::connection_id(Some(request));
    let mut guard = match connections().lock() {
        Ok(guard) => guard,
        Err(_) => {
            return abi::error(
                "connector.statePoisoned",
                "Connector connection state is poisoned.",
            )
        }
    };
    let existed = guard.remove(&connection_id).is_some();
    abi::ok(serde_json::Map::from_iter([
        ("connectionId".to_string(), Value::String(connection_id)),
        ("closed".to_string(), Value::Bool(existed)),
    ]))
}

fn duckdb_version(conn: &duckdb::Connection) -> Option<String> {
    conn.query_row("select version()", [], |row| row.get::<_, String>(0))
        .ok()
}

fn should_seed_sample(request: &Value, connection_id: &str) -> bool {
    request
        .get("seedSample")
        .or_else(|| {
            request
                .get("profile")
                .and_then(|profile| profile.get("seedSample"))
        })
        .and_then(Value::as_bool)
        .unwrap_or(matches!(
            connection_id,
            "duckdb-memory" | "motherduck-memory"
        ))
}

fn seed_sample(conn: &duckdb::Connection) -> Result<(), String> {
    conn.execute_batch("create table if not exists customers (id integer, name varchar);")
        .map_err(|err| format!("duckdb sample schema failed: {err}"))?;
    let existing = conn
        .query_row("select count(*) from customers", [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap_or(0);
    if existing == 0 {
        conn.execute_batch("insert into customers values (1, 'Kawase Foods'), (2, 'Minato Labs');")
            .map_err(|err| format!("duckdb sample data failed: {err}"))?;
    }
    Ok(())
}

fn run_query(conn: &duckdb::Connection, sql: &str, cap: usize) -> Result<QueryOutput, String> {
    let lead = sql.trim_start().to_ascii_lowercase();
    let is_query = [
        "select", "with", "show", "pragma", "explain", "describe", "values", "table", "call",
    ]
    .iter()
    .any(|keyword| lead.starts_with(keyword));
    if !is_query {
        conn.execute(sql, [])
            .map_err(|err| format!("query failed: {err}"))?;
        return Ok((Vec::new(), Vec::new(), false));
    }

    let mut stmt = conn
        .prepare(sql)
        .map_err(|err| format!("query failed: {err}"))?;
    let mut duck_rows = stmt
        .query([])
        .map_err(|err| format!("query failed: {err}"))?;
    let columns: Vec<String> = match duck_rows.as_ref() {
        Some(stmt) => stmt
            .column_names()
            .iter()
            .map(|column| column.to_string())
            .collect(),
        None => Vec::new(),
    };
    let column_count = columns.len();
    let mut rows = Vec::new();
    let mut truncated = false;
    while let Some(row) = duck_rows
        .next()
        .map_err(|err| format!("query failed: {err}"))?
    {
        if rows.len() >= cap {
            truncated = true;
            break;
        }
        rows.push(
            (0..column_count)
                .map(|index| cell_to_json(row, index))
                .collect(),
        );
    }
    Ok((columns, rows, truncated))
}

fn load_metadata(conn: &duckdb::Connection) -> Result<Value, String> {
    let mut objects: BTreeMap<(String, String), ObjectMeta> = BTreeMap::new();
    let mut stmt = conn
        .prepare(
            "select table_schema, table_name, table_type \
             from information_schema.tables \
             where table_schema not in ('information_schema', 'pg_catalog') \
             order by table_schema, table_name",
        )
        .map_err(|err| format!("metadata objects failed: {err}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .map_err(|err| format!("metadata objects failed: {err}"))?;
    for row in rows {
        let (schema, name, table_type) =
            row.map_err(|err| format!("metadata objects failed: {err}"))?;
        let kind = if table_type.eq_ignore_ascii_case("VIEW") {
            "view"
        } else {
            "table"
        };
        objects.insert(
            (schema.clone(), name.clone()),
            ObjectMeta {
                schema,
                name,
                kind: kind.to_string(),
                columns: Vec::new(),
            },
        );
    }

    let mut stmt = conn
        .prepare(
            "select table_schema, table_name, column_name, data_type, is_nullable, ordinal_position \
             from information_schema.columns \
             where table_schema not in ('information_schema', 'pg_catalog') \
             order by table_schema, table_name, ordinal_position",
        )
        .map_err(|err| format!("metadata columns failed: {err}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, i32>(5)?,
            ))
        })
        .map_err(|err| format!("metadata columns failed: {err}"))?;
    for row in rows {
        let (schema, table, name, data_type, nullable, ordinal) =
            row.map_err(|err| format!("metadata columns failed: {err}"))?;
        if let Some(object) = objects.get_mut(&(schema, table)) {
            object.columns.push(json!({
                "name": name,
                "dataType": data_type,
                "nullable": nullable.eq_ignore_ascii_case("YES"),
                "ordinal": ordinal
            }));
        }
    }

    let mut schemas: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for object in objects.into_values() {
        schemas
            .entry(object.schema.clone())
            .or_default()
            .push(json!({
                "schema": object.schema,
                "name": object.name,
                "kind": object.kind,
                "columns": object.columns
            }));
    }
    Ok(json!({
        "schemas": schemas
            .into_iter()
            .map(|(name, objects)| json!({ "name": name, "objects": objects }))
            .collect::<Vec<_>>()
    }))
}

fn cell_to_json(row: &duckdb::Row, index: usize) -> Value {
    use duckdb::types::Value as DuckValue;
    match row.get::<usize, DuckValue>(index) {
        Ok(DuckValue::Null) => Value::Null,
        Ok(DuckValue::Boolean(value)) => Value::Bool(value),
        Ok(DuckValue::TinyInt(value)) => json!(value),
        Ok(DuckValue::SmallInt(value)) => json!(value),
        Ok(DuckValue::Int(value)) => json!(value),
        Ok(DuckValue::BigInt(value)) => json!(value),
        Ok(DuckValue::UTinyInt(value)) => json!(value),
        Ok(DuckValue::USmallInt(value)) => json!(value),
        Ok(DuckValue::UInt(value)) => json!(value),
        Ok(DuckValue::UBigInt(value)) => json!(value),
        Ok(DuckValue::Float(value)) => json!(value as f64),
        Ok(DuckValue::Double(value)) => json!(value),
        Ok(DuckValue::Text(value)) => Value::String(value),
        Ok(DuckValue::Blob(value)) => Value::String(format!("\\x{}", hex_encode(&value))),
        Ok(other) => Value::String(format!("{other:?}")),
        Err(_) => Value::Null,
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push_str(&format!("{byte:02x}"));
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    use crate::{
        irodori_connector_call_json, irodori_connector_free_buffer, IrodoriConnectorBuffer,
    };

    fn buffer_from_str(value: &'static str) -> IrodoriConnectorBuffer {
        IrodoriConnectorBuffer {
            ptr: value.as_ptr(),
            len: value.len(),
        }
    }

    fn buffer_to_json(buffer: IrodoriConnectorBuffer) -> Value {
        let bytes = unsafe { std::slice::from_raw_parts(buffer.ptr, buffer.len) };
        let value = serde_json::from_slice(bytes).unwrap();
        irodori_connector_free_buffer(buffer);
        value
    }

    fn call(request: &'static str) -> Value {
        buffer_to_json(irodori_connector_call_json(buffer_from_str(request)))
    }

    #[test]
    fn connect_query_metadata_and_close_use_real_duckdb_driver() {
        let connected = call(r#"{"method":"connect","connectionId":"test","database":":memory:"}"#);
        assert_eq!(connected["ok"], true);
        assert_eq!(connected["driverLinked"], true);

        assert_eq!(
            call(
                r#"{"method":"query","connectionId":"test","sql":"create table numbers (n integer, label varchar)"}"#
            )["ok"],
            true
        );
        assert_eq!(
            call(
                r#"{"method":"query","connectionId":"test","sql":"insert into numbers values (1, 'one'), (2, 'two')"}"#
            )["ok"],
            true
        );
        let result = call(
            r#"{"method":"query","connectionId":"test","sql":"select n, label from numbers order by n","maxRows":10}"#,
        );
        assert_eq!(result["ok"], true);
        assert_eq!(result["columns"], json!(["n", "label"]));
        assert_eq!(result["rows"], json!([[1, "one"], [2, "two"]]));

        let metadata = call(r#"{"method":"metadata","connectionId":"test"}"#);
        assert_eq!(metadata["ok"], true);
        let schemas = metadata["metadata"]["schemas"].as_array().unwrap();
        assert!(schemas.iter().any(|schema| schema["objects"]
            .as_array()
            .unwrap()
            .iter()
            .any(|object| object["name"] == "numbers")));

        assert_eq!(
            call(r#"{"method":"close","connectionId":"test"}"#)["closed"],
            true
        );
        let missing = call(r#"{"method":"query","connectionId":"test","sql":"select 1"}"#);
        assert_eq!(missing["ok"], false);
        assert_eq!(missing["error"]["code"], "connector.connectionNotFound");
    }

    #[test]
    fn query_reports_driver_errors() {
        let _ = call(r#"{"method":"connect","connectionId":"errors","database":":memory:"}"#);
        let response = call(
            r#"{"method":"query","connectionId":"errors","sql":"select * from missing_table"}"#,
        );
        assert_eq!(response["ok"], false);
        assert_eq!(response["error"]["code"], "connector.queryFailed");
    }

    #[test]
    fn a_local_path_is_still_a_local_file() {
        assert_eq!(
            Target::from_request(&json!({ "profile": { "database": "/tmp/analytics.duckdb" } })),
            Target::LocalFile("/tmp/analytics.duckdb".to_string())
        );
        assert_eq!(
            Target::from_request(&json!({ "profile": { "database": ":memory:" } })),
            Target::InMemory
        );
        assert_eq!(
            Target::from_request(&json!({ "profile": {} })),
            Target::InMemory
        );
    }

    #[test]
    fn an_md_target_is_recognised_rather_than_opened_as_a_file() {
        // The bug this fixes: `md:analytics` used to reach
        // `duckdb::Connection::open`, which creates a local file with that
        // name instead of resolving MotherDuck.
        assert_eq!(
            Target::from_request(&json!({ "profile": { "database": "md:analytics" } })),
            Target::MotherDuck("analytics".to_string())
        );
        assert_eq!(
            Target::from_request(&json!({ "profile": { "database": "md:" } })),
            Target::MotherDuck(String::new())
        );
        assert_eq!(
            Target::from_request(&json!({ "profile": { "database": "motherduck:sales" } })),
            Target::MotherDuck("sales".to_string())
        );
    }

    #[test]
    fn the_scheme_is_case_insensitive_but_the_name_is_not() {
        assert_eq!(
            motherduck_database("MD:Analytics"),
            Some("Analytics".to_string())
        );
        assert_eq!(
            motherduck_database("MotherDuck:Sales"),
            Some("Sales".to_string())
        );
        // A local path must not be mistaken for a MotherDuck target.
        assert_eq!(motherduck_database("/tmp/analytics.duckdb"), None);
        assert_eq!(motherduck_database("analytics.duckdb"), None);
    }

    #[test]
    fn the_url_form_from_the_form_placeholder_is_understood() {
        // `motherduck://token@md/database` is what the connection form shows.
        assert_eq!(
            motherduck_database("motherduck://tok@md/warehouse"),
            Some("warehouse".to_string())
        );
    }

    #[test]
    fn the_token_comes_from_the_password_field() {
        // The form labels this engine's password box "MotherDuck token".
        assert_eq!(
            motherduck_token(&json!({ "profile": { "password": "md_tok" } })).as_deref(),
            Some("md_tok")
        );
        assert_eq!(
            motherduck_token(
                &json!({ "profile": { "options": { "motherduckToken": "explicit" } } })
            )
            .as_deref(),
            Some("explicit")
        );
    }

    #[test]
    fn a_blank_token_is_no_token() {
        assert_eq!(
            motherduck_token(&json!({ "profile": { "password": "   " } })),
            None
        );
    }

    #[test]
    fn the_token_is_quoted_against_injection() {
        assert_eq!(sql_string("it's"), "'it''s'");
    }
}
