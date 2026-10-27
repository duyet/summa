/**
 * Antigravity Source
 *
 * Fetches usage data from Antigravity CLI conversations (~/.gemini/antigravity-cli/).
 * Parses SQLite databases for exact token counts, and estimates older encrypted Protobuf logs.
 */

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use crate::model::{DataSource, EventRow, EventsSnapshotData, SourceResult};
use crate::util::date::ch_now;
use crate::util::hash::{hash_project_name_sync, make_dedup_key};
use crate::util::pricing::estimate_model_cost;
use async_trait::async_trait;

// ---------------------------------------------------------------------------
// Protobuf decoding (varint and field tags)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct ProtoField {
    _type: &'static str,
    value: ProtoValue,
}

#[derive(Debug, Clone)]
enum ProtoValue {
    Varint(u64),
    Fixed64(Vec<u8>),
    Bytes(Vec<u8>),
    Fixed32(Vec<u8>),
    Sub(DecodedProto),
}

type DecodedProto = HashMap<u32, Vec<ProtoField>>;

fn parse_varint(data: &[u8], mut pos: usize) -> (u64, usize) {
    let mut val: u64 = 0;
    let mut shift: u32 = 0;
    while pos < data.len() {
        let b = data[pos];
        pos += 1;
        // Malformed leftover blobs can request shift >= 64; stop instead of panic.
        if shift >= 64 {
            break;
        }
        val |= ((b & 0x7f) as u64) << shift;
        shift += 7;
        if (b & 0x80) == 0 {
            break;
        }
    }
    (val, pos)
}

fn decode_proto(data: &[u8], start: usize, end: usize) -> DecodedProto {
    let mut res: HashMap<u32, Vec<ProtoField>> = HashMap::new();
    let end = end.min(data.len());
    let mut pos = start.min(end);
    while pos < end {
        let (key, next_pos) = parse_varint(data, pos);
        if next_pos <= pos || next_pos > end {
            break;
        }
        pos = next_pos;
        let wire_type = (key & 0x7) as u32;
        let field_num = (key >> 3) as u32;

        match wire_type {
            0 => {
                let (val, next_pos2) = parse_varint(data, pos);
                if next_pos2 <= pos || next_pos2 > end {
                    break;
                }
                pos = next_pos2;
                res.entry(field_num)
                    .or_default()
                    .push(ProtoField { _type: "varint", value: ProtoValue::Varint(val) });
            }
            1 => {
                if pos + 8 > end {
                    break;
                }
                let val = data[pos..pos + 8].to_vec();
                pos += 8;
                res.entry(field_num)
                    .or_default()
                    .push(ProtoField { _type: "fixed64", value: ProtoValue::Fixed64(val) });
            }
            2 => {
                let (len, content_pos) = parse_varint(data, pos);
                if content_pos < pos || content_pos > end {
                    break;
                }
                let len_usize = len as usize;
                if content_pos + len_usize > end {
                    break;
                }
                let slice = data[content_pos..content_pos + len_usize].to_vec();
                pos = content_pos + len_usize;
                let sub = if len > 0 {
                    let decoded = decode_proto(&slice, 0, len_usize);
                    if !decoded.is_empty() {
                        ProtoValue::Sub(decoded)
                    } else {
                        ProtoValue::Bytes(slice)
                    }
                } else {
                    ProtoValue::Bytes(slice)
                };
                res.entry(field_num)
                    .or_default()
                    .push(ProtoField { _type: "bytes", value: sub });
            }
            5 => {
                if pos + 4 > end {
                    break;
                }
                let val = data[pos..pos + 4].to_vec();
                pos += 4;
                res.entry(field_num)
                    .or_default()
                    .push(ProtoField { _type: "fixed32", value: ProtoValue::Fixed32(val) });
            }
            _ => break,
        }
    }
    res
}

fn get_varint(fields: &DecodedProto, field_num: u32) -> u64 {
    fields.get(&field_num).and_then(|list| {
        list.iter().find_map(|f| {
            if let ProtoValue::Varint(v) = f.value { Some(v) } else { None }
        })
    }).unwrap_or(0)
}

fn extract_tokens(decoded: &DecodedProto) -> Option<(u64, u64, u64)> {
    let f1 = decoded.get(&1)?.get(0)?;
    let f1_inner = match &f1.value {
        ProtoValue::Sub(map) => map,
        _ => return None,
    };

    let f4 = f1_inner.get(&4)?.get(0)?;
    let f4_inner = match &f4.value {
        ProtoValue::Sub(map) => map,
        _ => return None,
    };

    let prompt = get_varint(f4_inner, 2);
    let cached = get_varint(f4_inner, 5);
    let comp = get_varint(f4_inner, 3);

    if prompt >= 9_000_000_000_000_000_000 { return None; }
    if cached >= 9_000_000_000_000_000_000 { return None; }
    if comp >= 9_000_000_000_000_000_000 { return None; }

    Some((prompt, cached, comp))
}

fn extract_model(decoded: &DecodedProto) -> String {
    if let Some(f1) = decoded.get(&1).and_then(|list| list.first()) {
        if let ProtoValue::Sub(f1_inner) = &f1.value {
            if let Some(bytes) = f1_inner.get(&19).and_then(|list| list.first()).and_then(|f| {
                match &f.value {
                    ProtoValue::Bytes(b) => Some(b.clone()),
                    _ => None,
                }
            }) {
                if !bytes.is_empty() {
                    return String::from_utf8_lossy(&bytes).to_string();
                }
            }
            if let Some(bytes) = f1_inner.get(&21).and_then(|list| list.first()).and_then(|f| {
                match &f.value {
                    ProtoValue::Bytes(b) => Some(b.clone()),
                    _ => None,
                }
            }) {
                if !bytes.is_empty() {
                    return String::from_utf8_lossy(&bytes).to_string();
                }
            }
        }
    }
    "gemini-3.5-flash-medium".to_string()
}

fn extract_timestamp(decoded: &DecodedProto) -> Option<i64> {
    if let Some(f1) = decoded.get(&1).and_then(|list| list.first()) {
        if let ProtoValue::Sub(f1_inner) = &f1.value {
            if let Some(f9) = f1_inner.get(&9).and_then(|list| list.first()) {
                if let ProtoValue::Sub(f9_inner) = &f9.value {
                    let seconds = get_varint(f9_inner, 1);
                    if seconds > 0 {
                        return Some(seconds as i64);
                    }
                }
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Options
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct AntigravitySourceOptions {
    pub machine_name: String,
    pub hash_projects: bool,
    pub verbose: bool,
    pub days_back: Option<i64>,
    pub since: Option<String>,
    pub end_date: Option<String>,
    pub import_id: String,
    /// Override Antigravity CLI dir (tests). When None, uses `~/.gemini/antigravity-cli`.
    pub cli_dir: Option<PathBuf>,
    /// Override where the per-run DB copies go (tests). When None, uses the system temp dir.
    pub temp_dir: Option<PathBuf>,
}

// ---------------------------------------------------------------------------
// Source
// ---------------------------------------------------------------------------

pub struct AntigravitySource {
    opts: AntigravitySourceOptions,
}

impl AntigravitySource {
    pub fn new(opts: AntigravitySourceOptions) -> Self {
        Self { opts }
    }

    pub fn name(&self) -> &'static str {
        "antigravity"
    }
}

#[async_trait]
impl DataSource for AntigravitySource {
    fn name(&self) -> &'static str {
        "antigravity"
    }

    async fn fetch(&self) -> anyhow::Result<SourceResult> {
        let AntigravitySourceOptions {
            machine_name,
            hash_projects,
            verbose,
            days_back,
            since,
            end_date,
            import_id,
            cli_dir,
            temp_dir,
        } = &self.opts;

        let effective_since = if let Some(s) = since {
            Some(s.clone())
        } else if let Some(days) = days_back {
            if *days > 0 {
                let d = chrono::Utc::now() - chrono::Duration::days(*days);
                Some(d.format("%Y-%m-%d").to_string())
            } else {
                None
            }
        } else {
            None
        };

        let cli_dir = resolve_cli_dir(cli_dir.clone());
        let conv_dir = cli_dir.join("conversations");
        let history_file = cli_dir.join("history.jsonl");

        let mut events: Vec<EventRow> = Vec::new();
        let now = ch_now();

        if !conv_dir.exists() {
            if *verbose {
                eprintln!("Antigravity conversations dir not found: {}", conv_dir.display());
            }
            return Ok(SourceResult {
                source_name: self.name().to_string(),
                data: EventsSnapshotData { events },
                fetched_at: chrono::Utc::now().to_rfc3339(),
                error: None,
            });
        }

        // 1. Parse history.jsonl to map conversation IDs -> workspaces/projects and dates
        let mut projects_map: HashMap<String, String> = HashMap::new();
        let mut history_prompts: HashMap<String, Vec<(String, i64)>> = HashMap::new();

        if history_file.exists() {
            if let Ok(content) = fs::read_to_string(&history_file) {
                for line in content.lines() {
                    if line.trim().is_empty() { continue; }
                    if let Ok(entry) = serde_json::from_str::<serde_json::Value>(line) {
                        if let (Some(cid), Some(workspace)) = (
                            entry.get("conversationId").and_then(|v| v.as_str()),
                            entry.get("workspace").and_then(|v| v.as_str()),
                        ) {
                            projects_map.insert(cid.to_string(), workspace.to_string());
                        }
                        if let (Some(cid), Some(ts)) = (
                            entry.get("conversationId").and_then(|v| v.as_str()),
                            entry.get("timestamp").and_then(|v| v.as_i64()),
                        ) {
                            let date = chrono::DateTime::from_timestamp(ts, 0)
                                .map(|dt| dt.format("%Y-%m-%d").to_string())
                                .unwrap_or_default();
                            if !date.is_empty() {
                                history_prompts.entry(cid.to_string()).or_default().push((date, ts));
                            }
                        }
                    }
                }
            }
        }

        // List all conversation files (.db and .pb)
        let files: Vec<String> = fs::read_dir(&conv_dir)?
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
            .filter(|e| {
                let name = e.file_name().to_string_lossy().to_string();
                name.ends_with(".db") || name.ends_with(".pb")
            })
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();

        let db_files: Vec<_> = files.iter().filter(|f| f.ends_with(".db")).collect();
        let pb_files: Vec<_> = files.iter().filter(|f| f.ends_with(".pb")).collect();

        // 2. Parse exact SQLite (.db) conversations
        let mut db_daily_sums: HashMap<String, (u64, u64, u64, u64, String, String)> = HashMap::new();
        let mut db_session_sums: HashMap<String, (u64, u64, u64, String, String, String)> = HashMap::new();

        for file in &db_files {
            let db_path = conv_dir.join(file);
            let cid = file.trim_end_matches(".db").to_string();
            let workspace = projects_map.get(&cid).cloned().unwrap_or_else(|| cid.clone());

            let temp_dir = temp_dir.clone().unwrap_or_else(std::env::temp_dir);
            let temp_db = temp_dir.join(format!("{}-{}.db", cid, uuid::Uuid::new_v4()));

            let result: anyhow::Result<()> = (|| {
                fs::copy(&db_path, &temp_db)?;
                let wal_path = db_path.with_extension("db-wal");
                let shm_path = db_path.with_extension("db-shm");
                if wal_path.exists() {
                    fs::copy(&wal_path, temp_dir.join(format!("{}-{}.db-wal", cid, uuid::Uuid::new_v4())))?;
                }
                if shm_path.exists() {
                    fs::copy(&shm_path, temp_dir.join(format!("{}-{}.db-shm", cid, uuid::Uuid::new_v4())))?;
                }

                let _conn = rusqlite::Connection::open(&temp_db)?;
                let mut stmt = _conn.prepare("SELECT data FROM gen_metadata")?;
                let rows = stmt.query_map([], |row| {
                    let data: Vec<u8> = row.get(0)?;
                    Ok(data)
                })?;

                for row_result in rows {
                    let data = row_result?;
                    if data.is_empty() { continue; }
                    let decoded = decode_proto(&data, 0, data.len());
                    let tokens = extract_tokens(&decoded);
                    let timestamp = extract_timestamp(&decoded);
                    let model = extract_model(&decoded);

                    if let Some((prompt, cached, comp)) = tokens {
                        if let Some(ts) = timestamp {
                            let date = chrono::DateTime::from_timestamp(ts, 0)
                                .map(|dt| dt.format("%Y-%m-%d").to_string())
                                .unwrap_or_default();
                            if date.is_empty() { continue; }

                            if let Some(ref eff) = effective_since {
                                if &date < eff { continue; }
                            }
                            if let Some(ref ed) = end_date {
                                if &date > ed { continue; }
                            }

                            let daily_key = format!("{}|{}", date, model);
                            let entry = db_daily_sums.entry(daily_key).or_insert((0, 0, 0, 0, model.clone(), workspace.clone()));
                            entry.0 += prompt;
                            entry.1 += cached;
                            entry.2 += comp;
                            entry.3 += 1;

                            let session_key = format!("{}|{}|{}", cid, date, model);
                            let sentry = db_session_sums.entry(session_key).or_insert((0, 0, 0, model.clone(), workspace.clone(), date.clone()));
                            sentry.0 += prompt;
                            sentry.1 += cached;
                            sentry.2 += comp;
                        }
                    }
                }
                Ok(())
            })();

            let _ = result;
            let _ = fs::remove_file(&temp_db);
        }

        // Build SQLite daily rows
        for (key, sum) in &db_daily_sums {
            let (prompt, cached, comp, _count, model, workspace) = sum;
            let parts: Vec<&str> = key.split('|').collect();
            if parts.len() < 2 { continue; }
            let date = parts[0];
            let hashed_proj = hash_project_name_sync(workspace, *hash_projects);

            let raw_key = format!("antigravity|{}|daily|{}|{}|{}", machine_name, date, model, date);
            let dedup_key = make_dedup_key(&raw_key);
            let cost = estimate_model_cost(model, *prompt, *cached, 0, *comp);

            events.push(make_antigravity_row(
                &now, date, "daily", date, "antigravity", machine_name,
                model, "", &hashed_proj,
                *prompt, *comp, 0, *cached, 0,
                prompt + comp + cached,
                cost, &dedup_key, import_id,
            ));
        }

        // Build SQLite session rows
        for (key, sum) in &db_session_sums {
            let (prompt, cached, comp, model, workspace, _date) = sum;
            let parts: Vec<&str> = key.split('|').collect();
            if parts.len() < 3 { continue; }
            let cid = parts[0];
            let date_str = parts[1];
            let hashed_cid = hash_project_name_sync(cid, *hash_projects);
            let hashed_proj = hash_project_name_sync(workspace, *hash_projects);

            let raw_key = format!("antigravity|{}|session|{}|{}|{}", machine_name, date_str, model, hashed_cid);
            let dedup_key = make_dedup_key(&raw_key);
            let cost = estimate_model_cost(model, *prompt, *cached, 0, *comp);

            events.push(make_antigravity_row(
                &now, date_str, "session", &hashed_cid, "antigravity", machine_name,
                model, &hashed_cid, &hashed_proj,
                *prompt, *comp, 0, *cached, 0,
                prompt + comp + cached,
                cost, &dedup_key, import_id,
            ));
        }

        // Encrypted leftover `.pb` conversations and `implicit/*.pb` have no
        // decodable token records. Do not invent per-prompt or per-byte estimates
        // (those were showing up on burn.duyet.net as "Google Antigravity").
        let _ = (pb_files, history_prompts);

        if *verbose {
            eprintln!("Antigravity Source parsed {} rows.", events.len());
        }

        Ok(SourceResult {
            source_name: self.name().to_string(),
            data: EventsSnapshotData { events },
            fetched_at: chrono::Utc::now().to_rfc3339(),
            error: None,
        })
    }
}

fn resolve_cli_dir(override_dir: Option<PathBuf>) -> PathBuf {
    if let Some(dir) = override_dir {
        return dir;
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".gemini/antigravity-cli")
}

// ---------------------------------------------------------------------------
// Row builder helper
// ---------------------------------------------------------------------------

fn make_antigravity_row(
    now: &str,
    date: &str,
    record_type: &str,
    record_key: &str,
    source: &str,
    machine_name: &str,
    model_name: &str,
    session_id: &str,
    project_path: &str,
    input_tokens: u64,
    output_tokens: u64,
    cache_creation_tokens: u64,
    cache_read_tokens: u64,
    reasoning_tokens: u64,
    total_tokens: u64,
    cost: f64,
    dedup_key: &str,
    import_id: &str,
) -> EventRow {
    EventRow {
        date: date.to_string(),
        record_type: record_type.to_string(),
        record_key: record_key.to_string(),
        source: source.to_string(),
        machine_name: machine_name.to_string(),
        account_id: String::new(),
        api_key_id: String::new(),
        model_name: model_name.to_string(),
        session_id: session_id.to_string(),
        project_path: project_path.to_string(),
        input_tokens,
        output_tokens,
        cache_creation_tokens,
        cache_read_tokens,
        reasoning_tokens,
        total_tokens,
        cost,
        dedup_key: dedup_key.to_string(),
        import_id: import_id.to_string(),
        block_id: String::new(),
        start_time: None,
        end_time: None,
        actual_end_time: None,
        is_active: 0,
        is_gap: 0,
        entries: 1,
        burn_rate: 0.0,
        projection: 0.0,
        usage_limit_reset_time: None,
        created_at: now.to_string(),
        updated_at: now.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::Path;
    use tempfile::TempDir;

    fn opts_for(cli_dir: PathBuf) -> AntigravitySourceOptions {
        AntigravitySourceOptions {
            machine_name: "m1".into(),
            hash_projects: false,
            verbose: false,
            days_back: None,
            since: None,
            end_date: None,
            import_id: "test-import".into(),
            cli_dir: Some(cli_dir),
            temp_dir: None,
        }
    }

    fn fetch(cli_dir: PathBuf) -> SourceResult {
        let src = AntigravitySource::new(opts_for(cli_dir));
        futures::executor::block_on(async move { src.fetch().await }).unwrap()
    }

    fn encode_varint(mut v: u64) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let mut b = (v & 0x7f) as u8;
            v >>= 7;
            if v != 0 {
                b |= 0x80;
            }
            out.push(b);
            if v == 0 {
                break;
            }
        }
        out
    }

    fn encode_key(field: u32, wire: u32) -> Vec<u8> {
        encode_varint(((field as u64) << 3) | wire as u64)
    }

    fn encode_varint_field(field: u32, value: u64) -> Vec<u8> {
        let mut out = encode_key(field, 0);
        out.extend(encode_varint(value));
        out
    }

    fn encode_len_field(field: u32, payload: &[u8]) -> Vec<u8> {
        let mut out = encode_key(field, 2);
        out.extend(encode_varint(payload.len() as u64));
        out.extend_from_slice(payload);
        out
    }

    /// Real gen_metadata blob: tokens + timestamp + model (not leftover estimates).
    fn real_token_blob(prompt: u64, cached: u64, comp: u64, ts: u64, model: &str) -> Vec<u8> {
        let mut tokens = Vec::new();
        tokens.extend(encode_varint_field(2, prompt));
        tokens.extend(encode_varint_field(5, cached));
        tokens.extend(encode_varint_field(3, comp));

        let ts_inner = encode_varint_field(1, ts);
        let mut inner = Vec::new();
        inner.extend(encode_len_field(4, &tokens));
        inner.extend(encode_len_field(9, &ts_inner));
        inner.extend(encode_len_field(19, model.as_bytes()));
        encode_len_field(1, &inner)
    }

    #[test]
    fn test_name() {
        let src = AntigravitySource::new(opts_for(PathBuf::from("/nope")));
        assert_eq!(src.name(), "antigravity");
    }

    #[test]
    fn test_fetch_returns_ok_when_missing_dir() {
        let tmp = TempDir::new().unwrap();
        let missing = tmp.path().join("no-such-antigravity-cli");
        let r = fetch(missing);
        assert_eq!(r.source_name, "antigravity");
        assert!(r.data.events.is_empty());
    }

    #[test]
    fn leftover_encrypted_pb_implicit_and_history_emit_no_daily_rows() {
        // Leftover CLI files without decodable token records must not become
        // source=antigravity daily rows (the old per-prompt / per-byte estimates).
        let tmp = TempDir::new().unwrap();
        let cli = tmp.path();
        fs::create_dir_all(cli.join("conversations")).unwrap();
        fs::create_dir_all(cli.join("implicit")).unwrap();
        fs::write(cli.join("conversations").join("leftover.pb"), [0xC3, 0x91, 0x34, 0xF7]).unwrap();
        fs::write(cli.join("implicit").join("subagent.pb"), vec![0xAAu8; 4096]).unwrap();
        let mut hist = fs::File::create(cli.join("history.jsonl")).unwrap();
        writeln!(
            hist,
            r#"{{"conversationId":"leftover","timestamp":1781481600000,"workspace":"/tmp/demo"}}"#
        )
        .unwrap();

        let r = fetch(cli.to_path_buf());
        assert_eq!(r.source_name, "antigravity");
        assert!(
            r.data.events.is_empty(),
            "fabricated leftover rows: {:?}",
            r.data.events.iter().map(|e| (&e.record_type, e.total_tokens)).collect::<Vec<_>>()
        );
    }

    #[test]
    fn decoded_sqlite_token_records_emit_antigravity_rows() {
        let tmp = TempDir::new().unwrap();
        let cli = tmp.path();
        let conv = cli.join("conversations");
        fs::create_dir_all(&conv).unwrap();

        let db_path = conv.join("cid-real.db");
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute("CREATE TABLE gen_metadata (data BLOB)", []).unwrap();
        // 2026-06-15T00:00:00Z
        let ts = 1_781_481_600u64;
        let blob = real_token_blob(1_000_000, 10_000_000, 100_000, ts, "gemini-3.5-flash-medium");
        conn.execute("INSERT INTO gen_metadata (data) VALUES (?1)", [&blob]).unwrap();
        drop(conn);

        let r = fetch(cli.to_path_buf());
        let daily: Vec<_> = r.data.events.iter().filter(|e| e.record_type == "daily").collect();
        assert_eq!(daily.len(), 1, "events={:?}", r.data.events);
        assert_eq!(daily[0].source, "antigravity");
        assert_eq!(daily[0].date, "2026-06-15");
        assert_eq!(daily[0].model_name, "gemini-3.5-flash-medium");
        assert_eq!(daily[0].input_tokens, 1_000_000);
        assert_eq!(daily[0].cache_read_tokens, 10_000_000);
        assert_eq!(daily[0].output_tokens, 100_000);
        assert_eq!(daily[0].total_tokens, 11_100_000);
        // 1M * $1.50 + 10M * $0.15 + 0.1M * $9 = $1.50 + $1.50 + $0.90
        assert!((daily[0].cost - 3.90).abs() < 0.001);
        assert!(r.data.events.iter().any(|e| e.record_type == "session"));
    }

    /// A live conversation DB: the table is checkpointed into the main file,
    /// the token record sits only in the WAL, and the writer stays open.
    fn conversation_db_with_record_in_wal(conv: &Path) -> rusqlite::Connection {
        let writer = rusqlite::Connection::open(conv.join("cid-live.db")).unwrap();
        writer
            .execute_batch(
                "PRAGMA journal_mode = WAL;
                CREATE TABLE gen_metadata (data BLOB);
                PRAGMA wal_checkpoint(TRUNCATE);",
            )
            .unwrap();
        // 2026-06-15T00:00:00Z
        let blob = real_token_blob(1_000, 2_000, 300, 1_781_481_600, "gemini-3.5-flash-medium");
        writer.execute("INSERT INTO gen_metadata (data) VALUES (?1)", [&blob]).unwrap();
        assert!(conv.join("cid-live.db-wal").exists());
        writer
    }

    fn fetch_with_temp_dir(cli_dir: &Path, temp_dir: &Path) -> SourceResult {
        let src = AntigravitySource::new(AntigravitySourceOptions {
            temp_dir: Some(temp_dir.to_path_buf()),
            ..opts_for(cli_dir.to_path_buf())
        });
        futures::executor::block_on(async move { src.fetch().await }).unwrap()
    }

    #[test]
    fn fetch_reads_records_still_in_the_wal() {
        let cli = TempDir::new().unwrap();
        let scratch = TempDir::new().unwrap();
        let conv = cli.path().join("conversations");
        fs::create_dir_all(&conv).unwrap();
        let _writer = conversation_db_with_record_in_wal(&conv);

        let r = fetch_with_temp_dir(cli.path(), scratch.path());

        let daily: Vec<_> = r
            .data
            .events
            .iter()
            .filter(|e| e.record_type == "daily")
            .map(|e| e.total_tokens)
            .collect();
        assert_eq!(daily, vec![3_300], "the WAL-only record was not read");
    }

    #[test]
    fn fetch_leaves_no_files_in_temp_dir() {
        let cli = TempDir::new().unwrap();
        let scratch = TempDir::new().unwrap();
        let conv = cli.path().join("conversations");
        fs::create_dir_all(&conv).unwrap();
        let _writer = conversation_db_with_record_in_wal(&conv);

        fetch_with_temp_dir(cli.path(), scratch.path());

        let left: Vec<_> = fs::read_dir(scratch.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert!(left.is_empty(), "DB copies left behind: {left:?}");
    }

    #[test]
    fn test_proto_decode_simple() {
        let data: Vec<u8> = vec![0x08, 0x01]; // field 1, varint 1
        let decoded = decode_proto(&data, 0, data.len());
        assert!(decoded.contains_key(&1));
    }

    #[test]
    fn parse_varint_stops_on_oversized_continuation() {
        // 10 continuation bytes would shift past u64; must not panic.
        let data = vec![0x80u8; 16];
        let (val, pos) = parse_varint(&data, 0);
        assert!(pos > 0);
        let _ = val;
        let decoded = decode_proto(&data, 0, data.len());
        assert!(decoded.is_empty() || decoded.len() < 100);
    }

    #[test]
    fn test_extract_tokens_decodes_correctly() {
        // Build a fake protobuf:
        // message Outer { message Inner { int64 prompt=2; int64 cached=5; int64 comp=3; } inner=1; }
        // Inner: field 2 varint 100 (0x10 0x64), field 5 varint 50 (0x28 0x32), field 3 varint 25 (0x18 0x19)
        // Middle (field 4): 0x22 0x06 + inner (6 bytes)
        // Outer (field 1): 0x0a 0x08 + middle (8 bytes)
        // Total 10 bytes: 0a 08 22 06 10 64 28 32 18 19
        let data = vec![
            0x0a, 0x08,  // field 1, length 8
            0x22, 0x06,  // field 4, length 6
            0x10, 0x64,  // field 2, varint 100
            0x28, 0x32,  // field 5, varint 50
            0x18, 0x19,  // field 3, varint 25
        ];

        let decoded = decode_proto(&data, 0, data.len());
        let tokens = extract_tokens(&decoded);
        assert_eq!(tokens, Some((100, 50, 25)));
    }
}
