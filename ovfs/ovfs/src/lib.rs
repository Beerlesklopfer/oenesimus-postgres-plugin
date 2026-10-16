//! `ovfs`: Bareos catalog directory trees as JSON for restores.
//!
//! Reads the Bareos catalog (schema 2250) unchanged. Functions are
//! `SECURITY INVOKER`: callers need SELECT on the catalog tables they read
//! (`path`, `file`, `basefiles`, `job`, `client`, `fileset`, `jobmedia`,
//! `media`), resolved through the caller's `search_path`.

use pgrx::pgrx_sql_entity_graph::metadata::{ArgumentError, ReturnsError, ReturnsRef, SqlMappingRef, SqlTranslatable};
use pgrx::prelude::*;
use pgrx::Json;
use serde_json::json;

use ovfs_core::tree::{self, Entry};

::pgrx::pg_module_magic!(name, version);

/// JSON text returned as `json` without parsing it into a value tree first;
/// a tree of a million entries is a few hundred MB of text.
pub struct JsonText(String);

impl IntoDatum for JsonText {
    fn into_datum(self) -> Option<pg_sys::Datum> {
        self.0.as_str().into_datum()
    }

    fn type_oid() -> pg_sys::Oid {
        pg_sys::JSONOID
    }
}

// SAFETY: the datum is a text varlena, which is the representation of json
unsafe impl pgrx::callconv::BoxRet for JsonText {
    unsafe fn box_into<'fcx>(self, fcinfo: &mut pgrx::callconv::FcInfo<'fcx>) -> pgrx::datum::Datum<'fcx> {
        unsafe { fcinfo.return_optional_datum(self.into_datum()) }
    }
}

// SAFETY: maps to the built-in json type, whose datum IntoDatum produces
unsafe impl SqlTranslatable for JsonText {
    const TYPE_IDENT: &'static str = "json";
    const TYPE_ORIGIN: pgrx::pgrx_sql_entity_graph::metadata::TypeOrigin =
        pgrx::pgrx_sql_entity_graph::metadata::TypeOrigin::External;
    const ARGUMENT_SQL: Result<SqlMappingRef, ArgumentError> = Ok(SqlMappingRef::literal("json"));
    const RETURN_SQL: Result<ReturnsRef, ReturnsError> = Ok(ReturnsRef::One(SqlMappingRef::literal("json")));
}

/// Catalog rows of the chain for the tree, sorted by the bytes of path and
/// name (depth-first order, all versions of an entry adjacent). `{filter}`
/// selects the paths, `{path_key}`/`{name_key}` give byte order.
/// Files of base jobs referenced by a job of the chain count with the
/// base job's JobId, FileIndex and JobTDate, as in Bareos `GetFileList`.
const ROWS_SQL: &str = "\
    SELECT textsend(p.path), textsend(f.name), f.jobid, j.jobtdate, f.fileid, f.fileindex, f.deltaseq::int, f.lstat, \
        {path_key} AS path_key, {name_key} AS name_key \
    FROM path p JOIN file f ON f.pathid = p.pathid JOIN job j ON j.jobid = f.jobid \
    WHERE f.jobid = ANY($1) AND {filter} \
    UNION ALL \
    SELECT textsend(p.path), textsend(f.name), f.jobid, j.jobtdate, f.fileid, f.fileindex, f.deltaseq::int, f.lstat, \
        {path_key}, {name_key} \
    FROM basefiles b JOIN file f ON f.fileid = b.fileid JOIN path p ON p.pathid = f.pathid JOIN job j ON j.jobid = f.jobid \
    WHERE b.jobid = ANY($1) AND {filter} \
    ORDER BY path_key, name_key";

/// Bareos databases use `LC_COLLATE 'C'`: text order is byte order, so the
/// range scan on `path_name_idx` already delivers the paths sorted.
fn bytewise_sql() -> String {
    ROWS_SQL
        .replace("{filter}", "p.path >= $2 AND p.path < $3")
        .replace("{path_key}", "p.path")
        .replace("{name_key}", "f.name")
}

/// Other collations: prefix match and an explicit sort on the bytes.
fn other_collation_sql() -> String {
    ROWS_SQL
        .replace("{filter}", "starts_with(p.path, $2)")
        .replace("{path_key}", "textsend(p.path)")
        .replace("{name_key}", "textsend(f.name)")
}

/// Byte-wise ordering, as Bareos creates its database (`LC_COLLATE 'C'`).
const BYTEWISE_COLLATION_SQL: &str = "\
    SELECT datlocprovider = 'c' AND datcollate IN ('C', 'POSIX') \
        OR datlocprovider = 'b' AND datlocale = 'C' \
    FROM pg_database WHERE datname = current_database()";

const MISSING_JOBS_SQL: &str = "\
    SELECT array_agg(id ORDER BY id) FROM unnest($1::int[]) AS id \
    WHERE NOT EXISTS (SELECT 1 FROM job WHERE jobid = id)";

fn invalid(message: String) -> ! {
    ereport!(ERROR, PgSqlErrorCode::ERRCODE_INVALID_PARAMETER_VALUE, message);
}

fn catalog_error(error: pgrx::spi::Error) -> ! {
    error!("catalog query failed: {error}")
}

fn normalize_dir(dir: &str) -> Vec<u8> {
    tree::normalize_root(dir).unwrap_or_else(|_| invalid(format!("invalid directory: {dir:?}")))
}

/// Rows fetched from the cursor at a time.
const FETCH_ROWS: i64 = 10_000;

/// Stream the catalog rows of the chain at or below `root` (all versions),
/// sorted by path and name, through a cursor into `add`; only one batch of
/// rows is in memory at a time.
fn fetch(jobids: Vec<Option<i32>>, root: &[u8], mut add: impl FnMut(Entry)) {
    let Some(jobids) = jobids.into_iter().collect::<Option<Vec<i32>>>() else {
        invalid("jobids must not contain NULL".into());
    };
    if jobids.is_empty() {
        invalid("jobids must not be empty".into());
    }

    Spi::connect(|client| -> Result<(), pgrx::spi::Error> {
        let missing = client.select(MISSING_JOBS_SQL, Some(1), &[jobids.clone().into()])?.first().get_one::<Vec<i32>>()?;
        match missing.as_deref() {
            None | Some([]) => {}
            Some([jobid]) => invalid(format!("job {jobid} does not exist")),
            Some(jobs) => invalid(format!("jobs {jobs:?} do not exist")),
        }

        // Paths are compared as text; root is valid UTF-8 and ends with '/',
        // so the upper bound only bumps that final byte and stays valid text.
        let root_text = String::from_utf8(root.to_vec()).expect("root comes from &str");
        let bytewise = client.select(BYTEWISE_COLLATION_SQL, Some(1), &[])?.first().get_one::<bool>()?.unwrap_or(false);
        let mut cursor = match (bytewise, tree::prefix_upper_bound(root)) {
            (true, Some(upper)) => {
                let upper = String::from_utf8(upper).expect("only the trailing '/' is bumped");
                client.try_open_cursor(bytewise_sql().as_str(), &[jobids.into(), root_text.into(), upper.into()])?
            }
            _ => {
                client.try_open_cursor(other_collation_sql().as_str(), &[jobids.into(), root_text.into()])?
            }
        };

        loop {
            let rows = cursor.fetch(FETCH_ROWS as _)?;
            if rows.is_empty() {
                break;
            }
            for row in rows {
                add(Entry {
                    path: row.get::<Vec<u8>>(1)?.unwrap_or_default(),
                    name: row.get::<Vec<u8>>(2)?.unwrap_or_default(),
                    jobid: row.get::<i32>(3)?.unwrap_or_default(),
                    jobtdate: row.get::<i64>(4)?.unwrap_or_default(),
                    fileid: row.get::<i64>(5)?.unwrap_or_default(),
                    fileindex: row.get::<i32>(6)?.unwrap_or_default(),
                    delta_seq: row.get::<i32>(7)?.unwrap_or_default(),
                    lstat: row.get::<String>(8)?.unwrap_or_default(),
                });
            }
            // pgrx keeps every fetched batch until the SPI connection ends;
            // free it now so only one batch is in memory at a time.
            // SAFETY: SPI_tuptable is the batch just consumed, nothing refers to it
            unsafe { pg_sys::SPI_freetuptable(pg_sys::SPI_tuptable) };
        }
        Ok(())
    })
    .unwrap_or_else(|error| catalog_error(error))
}

/// Directory tree of one job below `root` as JSON; see the job chain variant.
#[pg_extern(stable, parallel_safe, name = "tree")]
fn tree_of_job(jobid: i32, root: &str, depth: i32, max_entries: default!(i64, 1000000)) -> Option<JsonText> {
    tree(vec![Some(jobid)], root, depth, max_entries)
}

/// Directory tree of a job chain below `root` as JSON, down to `depth`
/// levels (1: `root` and its direct children).
///
/// `jobids` is the chain of a restore (Full, Differential, Incrementals);
/// the order does not matter. Per path and name the version of the newest
/// job (`JobTDate`) wins, deletions recorded by a newer job remove the
/// entry, files of base jobs and delta parts are included as in Bareos.
/// Every node carries `jobid` and `fileindex`. Directories at the depth
/// limit have no `children` but `entries`; a tree model loads them with
/// `ls` when they are expanded.
///
/// The rows are streamed and resolved one entry at a time, so memory is the
/// output, which `max_entries` limits (nodes in the result). The result is
/// `json`, not `jsonb`: a jsonb array is limited to 256 MB.
///
/// Returns NULL if no live entry remains at or below `root`. Raises an error
/// for an empty list, unknown jobs, an invalid root, a negative depth, or a
/// result of more than `max_entries` nodes.
#[pg_extern(stable, parallel_safe)]
fn tree(jobids: Vec<Option<i32>>, root: &str, depth: i32, max_entries: default!(i64, 1000000)) -> Option<JsonText> {
    let root = normalize_dir(root);
    let Ok(depth) = usize::try_from(depth) else {
        invalid(format!("depth must not be negative, got {depth}"));
    };
    if max_entries < 1 {
        invalid(format!("max_entries must be positive, got {max_entries}"));
    }
    let mut writer = tree::TreeWriter::new(&root, depth);
    fetch(jobids, &root, |entry| {
        writer.add(entry);
        if writer.nodes() as i64 > max_entries {
            ereport!(
                ERROR,
                PgSqlErrorCode::ERRCODE_PROGRAM_LIMIT_EXCEEDED,
                format!("the tree has more than {max_entries} entries; choose a smaller depth or a deeper root")
            );
        }
    });
    writer.finish().map(JsonText)
}

/// Direct children of `dir` in a job chain, one row each, for a tree model
/// that loads a directory when it is expanded.
///
/// Resolution and order are the same as for `tree` (files first, then
/// directories); `kind` is the node `type` of the JSON tree. Directory rows
/// carry `path` (the argument for the next call) and `entries` (live entries
/// below). The rows below `dir` are streamed and only counted, so memory is
/// the children of `dir`, however large the backup is.
#[allow(clippy::type_complexity)]
#[pg_extern(stable, parallel_safe)]
fn ls(
    jobids: Vec<Option<i32>>,
    dir: &str,
) -> TableIterator<
    'static,
    (
        name!(name, String),
        name!(name_b64, Option<String>),
        name!(path, Option<String>),
        name!(path_b64, Option<String>),
        name!(kind, String),
        name!(jobid, Option<i32>),
        name!(fileid, Option<i64>),
        name!(fileindex, Option<i32>),
        name!(delta_seq, Option<i32>),
        name!(delta_parts, Option<Json>),
        name!(mode, Option<i64>),
        name!(uid, Option<i64>),
        name!(gid, Option<i64>),
        name!(size, Option<i64>),
        name!(mtime, Option<i64>),
        name!(link_fileindex, Option<i64>),
        name!(entries, Option<i64>),
    ),
> {
    let dir = normalize_dir(dir);
    let mut lister = tree::DirLister::new(&dir);
    fetch(jobids, &dir, |entry| lister.add(entry));
    let children = lister.finish();
    TableIterator::new(children.into_iter().map(|child| {
        let (name, name_b64) = text_and_b64(&child.name);
        let (path, path_b64) = match &child.path {
            Some(path) => text_and_b64(path),
            None => (None, None),
        };
        let version = child.version.as_ref();
        let stat = version.and_then(|v| v.stat);
        let delta_parts = version.filter(|v| !v.delta_parts.is_empty()).map(|v| {
            Json(json!(v.delta_parts.iter().map(|p| json!({"jobid": p.jobid, "fileindex": p.fileindex})).collect::<Vec<_>>()))
        });
        (
            name.unwrap_or_default(),
            name_b64,
            path,
            path_b64,
            child.kind.as_str().to_string(),
            version.map(|v| v.jobid),
            version.map(|v| v.fileid),
            version.map(|v| v.fileindex),
            version.filter(|v| v.delta_seq > 0).map(|v| v.delta_seq),
            delta_parts,
            stat.map(|s| s.mode),
            stat.map(|s| s.uid),
            stat.map(|s| s.gid),
            stat.map(|s| s.size),
            stat.map(|s| s.mtime),
            stat.map(|s| s.link_fi).filter(|fi| *fi != 0),
            child.entries.map(|n| n as i64),
        )
    }))
}

/// Text form (lossy if not UTF-8) and, only if not UTF-8, the exact bytes.
fn text_and_b64(bytes: &[u8]) -> (Option<String>, Option<String>) {
    use base64::Engine;
    match std::str::from_utf8(bytes) {
        Ok(text) => (Some(text.to_string()), None),
        Err(_) => (
            Some(String::from_utf8_lossy(bytes).into_owned()),
            Some(base64::engine::general_purpose::STANDARD.encode(bytes)),
        ),
    }
}

const CLIENT_SQL: &str = "SELECT (SELECT clientid FROM client WHERE name = $1)";

/// FileSets of the client's backups (`uar_sel_fileset`, limited to backups).
const CLIENT_FILESETS_SQL: &str = "\
    SELECT array_agg(DISTINCT fs.fileset ORDER BY fs.fileset) \
    FROM job j JOIN fileset fs ON fs.filesetid = j.filesetid \
    WHERE j.clientid = $1 AND j.type = 'B'";

/// Most recent backup of a client and FileSet, as Bareos restore option 5
/// selects it (`uar_last_full_no_pool`, `uar_full`, `uar_dif`, `uar_inc`):
/// the last Full, the last Differential after it and all Incrementals after
/// Full or Differential. Only jobs on an enabled volume are part of the chain;
/// the Full anchors it even without one, as in Bareos.
const LATEST_CHAIN_SQL: &str = "\
    WITH candidate AS ( \
        SELECT j.jobid, j.jobtdate, j.level, EXISTS ( \
                SELECT 1 FROM jobmedia jm JOIN media m ON m.mediaid = jm.mediaid \
                WHERE jm.jobid = j.jobid AND m.enabled = 1) AS on_media \
        FROM job j JOIN fileset fs ON fs.filesetid = j.filesetid \
        WHERE j.clientid = $1 AND fs.fileset = $2 AND j.type = 'B' \
            AND j.jobstatus IN ('T', 'W') AND j.starttime < localtimestamp), \
    full_job AS (SELECT * FROM candidate WHERE level = 'F' ORDER BY jobtdate DESC LIMIT 1), \
    diff_job AS ( \
        SELECT c.* FROM candidate c, full_job f \
        WHERE c.level = 'D' AND c.on_media AND c.jobtdate > f.jobtdate \
        ORDER BY c.jobtdate DESC LIMIT 1), \
    incr_job AS ( \
        SELECT c.* FROM candidate c, full_job f \
        WHERE c.level = 'I' AND c.on_media \
            AND c.jobtdate > greatest(f.jobtdate, (SELECT jobtdate FROM diff_job))) \
    SELECT (SELECT jobid FROM full_job), (SELECT on_media FROM full_job), \
        coalesce((SELECT array_agg(jobid ORDER BY jobtdate, jobid) FROM ( \
            SELECT * FROM full_job WHERE on_media \
            UNION ALL SELECT * FROM diff_job \
            UNION ALL SELECT * FROM incr_job) chain), '{}')";

/// Job chain for a restore of the client's most recent backup: last Full,
/// then Differential and Incrementals up to now, ordered by `JobTDate`.
///
/// `fileset` may be omitted if the client has backups of one FileSet only.
/// If the Full is on no enabled volume, the chain lacks it (as in Bareos)
/// and a WARNING is raised: a restore would be incomplete.
#[pg_extern(stable, parallel_safe)]
fn jobids_latest(client: &str, fileset: default!(Option<&str>, "NULL")) -> Vec<i32> {
    Spi::connect(|spi| -> Result<Vec<i32>, pgrx::spi::Error> {
        let Some(clientid) = spi.select(CLIENT_SQL, Some(1), &[client.into()])?.first().get_one::<i32>()? else {
            invalid(format!("client {client:?} does not exist"));
        };
        let fileset = match fileset {
            Some(fileset) => fileset.to_string(),
            None => {
                let filesets = spi
                    .select(CLIENT_FILESETS_SQL, Some(1), &[clientid.into()])?
                    .first()
                    .get_one::<Vec<String>>()?
                    .unwrap_or_default();
                match <[String; 1]>::try_from(filesets) {
                    Ok([fileset]) => fileset,
                    Err(filesets) if filesets.is_empty() => invalid(format!("client {client:?} has no backups")),
                    Err(filesets) => {
                        invalid(format!("client {client:?} has backups of several FileSets {filesets:?}; choose one"))
                    }
                }
            }
        };

        let row = spi.select(LATEST_CHAIN_SQL, Some(1), &[clientid.into(), fileset.as_str().into()])?;
        let row = row.first();
        let Some(full) = row.get::<i32>(1)? else {
            invalid(format!("no Full backup of client {client:?} with FileSet {fileset:?} found"));
        };
        let jobids = row.get::<Vec<i32>>(3)?.unwrap_or_default();
        if jobids.is_empty() {
            invalid(format!("no job of client {client:?} with FileSet {fileset:?} is on an enabled volume"));
        }
        if !row.get::<bool>(2)?.unwrap_or(false) {
            warning!(
                "Full backup job {full} of client {client:?} is on no enabled volume; \
                 the chain {jobids:?} lacks it and a restore would be incomplete"
            );
        }
        Ok(jobids)
    })
    .unwrap_or_else(|error| catalog_error(error))
}

/// Directory tree of the client's most recent backup below `root`: the job
/// chain of `jobids_latest`, resolved like `tree(int[], ...)`.
#[pg_extern(stable, parallel_safe)]
fn tree_latest(
    client: &str,
    root: &str,
    depth: i32,
    fileset: default!(Option<&str>, "NULL"),
    max_entries: default!(i64, 1000000),
) -> Option<JsonText> {
    tree(jobids_latest(client, fileset).into_iter().map(Some).collect(), root, depth, max_entries)
}

/// Every backed-up version of one file of a client, newest first, with the
/// volumes that hold it (`.bvfs_versions`: Job.Type B, A, a and, with
/// `copies`, C; the FileIndex must lie in a JobMedia range).
const VERSIONS_SQL: &str = "\
    SELECT f.jobid, j.level::text, j.type::text, j.jobtdate, j.starttime, f.fileid, f.fileindex, \
        f.deltaseq::int, f.md5, f.lstat, \
        array_agg(DISTINCT m.volumename ORDER BY m.volumename), bool_or(m.inchanger <> 0) \
    FROM path p \
    JOIN file f ON f.pathid = p.pathid \
    JOIN job j ON j.jobid = f.jobid \
    JOIN client c ON c.clientid = j.clientid \
    JOIN jobmedia jm ON jm.jobid = j.jobid AND f.fileindex BETWEEN jm.firstindex AND jm.lastindex \
    JOIN media m ON m.mediaid = jm.mediaid \
    WHERE p.path = $1 AND f.name = $2 AND c.name = $3 \
        AND (j.type IN ('B', 'A', 'a') OR $4 AND j.type = 'C') \
    GROUP BY f.fileid, j.jobid \
    ORDER BY j.jobtdate DESC, f.fileid DESC";

/// All versions of the file `name` in directory `path` of `client`, newest
/// first, for choosing an older version to restore. `name = ''` lists the
/// versions of the directory itself.
#[allow(clippy::type_complexity)]
#[pg_extern(stable, parallel_safe)]
fn versions(
    client: &str,
    path: &str,
    name: &str,
    copies: default!(bool, false),
) -> TableIterator<
    'static,
    (
        name!(jobid, i32),
        name!(level, String),
        name!(job_type, String),
        name!(jobtdate, i64),
        name!(starttime, Option<Timestamp>),
        name!(fileid, i64),
        name!(fileindex, i32),
        name!(delta_seq, i32),
        name!(md5, String),
        name!(mode, Option<i64>),
        name!(uid, Option<i64>),
        name!(gid, Option<i64>),
        name!(size, Option<i64>),
        name!(mtime, Option<i64>),
        name!(volumes, Vec<String>),
        name!(inchanger, bool),
    ),
> {
    let path = String::from_utf8(normalize_dir(path)).expect("path comes from &str");
    let rows = Spi::connect(|spi| {
        let mut rows = Vec::new();
        for row in spi.select(VERSIONS_SQL, None, &[path.as_str().into(), name.into(), client.into(), copies.into()])? {
            let stat = ovfs_core::lstat::decode(&row.get::<String>(10)?.unwrap_or_default());
            rows.push((
                row.get::<i32>(1)?.unwrap_or_default(),
                row.get::<String>(2)?.unwrap_or_default(),
                row.get::<String>(3)?.unwrap_or_default(),
                row.get::<i64>(4)?.unwrap_or_default(),
                row.get::<Timestamp>(5)?,
                row.get::<i64>(6)?.unwrap_or_default(),
                row.get::<i32>(7)?.unwrap_or_default(),
                row.get::<i32>(8)?.unwrap_or_default(),
                row.get::<String>(9)?.unwrap_or_default(),
                stat.map(|s| s.mode),
                stat.map(|s| s.uid),
                stat.map(|s| s.gid),
                stat.map(|s| s.size),
                stat.map(|s| s.mtime),
                row.get::<Vec<String>>(11)?.unwrap_or_default(),
                row.get::<bool>(12)?.unwrap_or(false),
            ));
        }
        Ok::<_, pgrx::spi::Error>(rows)
    })
    .unwrap_or_else(|error| catalog_error(error));
    TableIterator::new(rows)
}

#[cfg(any(test, feature = "pg_test"))]
#[pg_schema]
mod tests {
    use pgrx::prelude::*;
    use pgrx::Json;
    use serde_json::{json, Value};

    const BAREOS_DDL: &str = include_str!("../../tests/fixtures/bareos-25.1.2-pre-postgresql.sql");

    /// LStat with mode, size and mtime set, in Bareos base64 digits:
    /// dev ino mode nlink uid gid rdev size blksize blocks atime mtime ctime LinkFI flags stream
    const DIR: &str = "A A EHt B A A A A BAA A A BlU/EA A A A A";
    const FILE_644: &str = "A A IGk B A A A BJn BAA A A BlU/EA A A A A";
    const FILE_600: &str = "A A IGA B A A A BJn BAA A A BlU/EA A A A A";

    fn setup() {
        // The DDL wraps one block in begin/commit, which SPI does not allow
        let ddl = BAREOS_DDL.replace("\nbegin;\n", "\n").replace("\ncommit;\n", "\n");
        Spi::run(&ddl).unwrap();
        Spi::run(&format!(
            "INSERT INTO job (jobid, job, name, type, level, jobstatus, jobtdate) VALUES \
                 (1, 'backup.2026-10-16', 'backup', 'B', 'F', 'T', 1792108800), \
                 (2, 'other.2026-10-16', 'other', 'B', 'F', 'T', 1792108800), \
                 (3, 'backup.2026-10-17', 'backup', 'B', 'I', 'T', 1792195200); \
             INSERT INTO path (pathid, path) VALUES \
                 (1, '/'), (2, '/etc/'), (3, '/etc/ssh/'), (4, '/etcetera/'), \
                 (5, '/home/user/Übersicht mit Leerzeichen/'), (6, '/data%_/'); \
             INSERT INTO file (fileindex, jobid, pathid, lstat, md5, name) VALUES \
                 (1, 1, 2, '{DIR}', '', ''), \
                 (2, 1, 2, '{FILE_644}', '', 'passwd'), \
                 (3, 1, 3, '{DIR}', '', ''), \
                 (4, 1, 3, '{FILE_644}', '', 'sshd_config'), \
                 (0, 1, 2, '{FILE_644}', '', 'deleted'), \
                 (5, 1, 4, '{FILE_644}', '', 'not-below-etc'), \
                 (6, 1, 5, '{FILE_644}', '', '日本語.txt'), \
                 (7, 1, 6, '{FILE_644}', '', 'pattern'), \
                 (1, 2, 2, '{FILE_644}', '', 'other-job'), \
                 (1, 3, 2, '{DIR}', '', ''), \
                 (2, 3, 2, '{FILE_600}', '', 'passwd'), \
                 (0, 3, 3, '{FILE_644}', '', 'sshd_config'), \
                 (3, 3, 3, '{FILE_644}', '', 'ssh_config')"
        ))
        .unwrap();
    }

    fn tree(jobid: i32, root: &str) -> Option<Value> {
        crate::tree_of_job(jobid, root, i32::MAX, 1_000_000).map(|text| serde_json::from_str(&text.0).unwrap())
    }

    fn chain(jobids: &[i32], root: &str) -> Option<Value> {
        crate::tree(jobids.iter().copied().map(Some).collect(), root, i32::MAX, 1_000_000).map(|text| serde_json::from_str(&text.0).unwrap())
    }

    fn child<'a>(node: &'a Value, name: &str) -> &'a Value {
        node["children"].as_array().unwrap().iter().find(|c| c["name"] == name).unwrap()
    }

    fn names(node: &Value) -> Vec<String> {
        node["children"].as_array().unwrap().iter().map(|c| c["name"].as_str().unwrap().to_string()).collect()
    }

    #[pg_test]
    fn lstat_fixtures_decode() {
        let dir = ovfs_core::lstat::decode(DIR).unwrap();
        assert_eq!(dir.mode, 0o040755);
        let file = ovfs_core::lstat::decode(FILE_644).unwrap();
        assert_eq!((file.mode, file.size, file.mtime), (0o100644, 4711, 1_700_000_000));
    }

    #[pg_test]
    fn tree_of_one_job() {
        setup();
        let etc = tree(1, "/etc").unwrap();
        assert_eq!(etc["path"], "/etc/");
        assert_eq!(etc["type"], "directory");
        assert_eq!(etc["mode"], 0o040755);
        assert_eq!(names(&etc), ["passwd", "ssh"]);
        assert_eq!(etc["children"][0]["size"], 4711);
        assert_eq!(etc["children"][0]["fileindex"], 2);
        assert_eq!(names(&etc["children"][1]), ["sshd_config"]);
    }

    #[pg_test]
    fn whole_job_from_root() {
        setup();
        let root = tree(1, "/").unwrap();
        assert_eq!(names(&root), ["data%_", "etc", "etcetera", "home"]);
        let user = &root["children"][3]["children"][0];
        assert_eq!(user["path"], "/home/user/");
        assert_eq!(names(&user["children"][0]), ["日本語.txt"]);
    }

    #[pg_test]
    fn other_jobs_and_deleted_files_stay_out() {
        setup();
        let etc = tree(1, "/etc/").unwrap();
        let all = serde_json::to_string(&etc).unwrap();
        assert!(!all.contains("other-job"));
        assert!(!all.contains("deleted"));
        assert!(!all.contains("not-below-etc"));
    }

    #[pg_test]
    fn like_metacharacters_are_literal() {
        setup();
        assert_eq!(names(&tree(1, "/data%_/").unwrap()), ["pattern"]);
        assert_eq!(tree(1, "/data/"), None);
    }

    #[pg_test]
    fn empty_subtree_is_null() {
        setup();
        assert_eq!(tree(1, "/nothing/here/"), None);
    }

    #[pg_test]
    #[should_panic(expected = "job 99 does not exist")]
    fn unknown_job_is_an_error() {
        setup();
        tree(99, "/");
    }

    #[pg_test]
    #[should_panic(expected = "invalid directory")]
    fn empty_root_is_an_error() {
        setup();
        tree(1, "");
    }

    #[pg_test]
    #[should_panic(expected = "the tree has more than 2 entries")]
    fn max_entries_is_enforced() {
        setup();
        crate::tree_of_job(1, "/etc/", 9, 2);
    }

    #[pg_test]
    fn sql_api() {
        setup();
        let json = Spi::get_one::<Json>("SELECT ovfs.tree(1, '/etc/ssh/', 9)").unwrap().unwrap().0;
        assert_eq!(json["path"], json!("/etc/ssh/"));
        // tree(int) and tree(int[]) overload, so a bare NULL needs a type
        assert!(Spi::get_one::<Json>("SELECT ovfs.tree(NULL::int, '/', 9)").unwrap().is_none());
        assert!(Spi::get_one::<Json>("SELECT ovfs.tree(NULL::int[], '/', 9)").unwrap().is_none());
    }

    #[pg_test]
    fn lstat_600_decodes() {
        assert_eq!(ovfs_core::lstat::decode(FILE_600).unwrap().mode, 0o100600);
    }

    #[pg_test]
    fn chain_takes_newest_versions() {
        setup();
        let etc = chain(&[3, 1], "/etc/").unwrap();
        assert_eq!(etc["jobid"], 3);
        assert_eq!(names(&etc), ["passwd", "ssh"]);
        let passwd = child(&etc, "passwd");
        assert_eq!((passwd["jobid"].as_i64(), passwd["fileindex"].as_i64()), (Some(3), Some(2)));
        assert_eq!(passwd["mode"], 0o100600);
        // ssh/ itself is unchanged in job 3, sshd_config was deleted there
        let ssh = child(&etc, "ssh");
        assert_eq!(ssh["jobid"], 1);
        assert_eq!(names(ssh), ["ssh_config"]);
        assert_eq!(child(ssh, "ssh_config")["jobid"], 3);
    }

    #[pg_test]
    fn chain_keeps_unchanged_parts_of_older_jobs() {
        setup();
        let root = chain(&[1, 3], "/").unwrap();
        assert_eq!(names(&root), ["data%_", "etc", "etcetera", "home"]);
        assert_eq!(child(&root, "etcetera")["children"][0]["jobid"], 1);
        // single job view is untouched by the newer job
        assert_eq!(names(&child(&tree(1, "/").unwrap(), "etc")["children"][1]), ["sshd_config"]);
    }

    #[pg_test]
    fn chain_of_only_deletions_below_root_is_null() {
        setup();
        assert_eq!(names(&chain(&[3], "/etc/ssh/").unwrap()), ["ssh_config"]);
        Spi::run("DELETE FROM file WHERE jobid = 3 AND name = 'ssh_config'").unwrap();
        assert_eq!(chain(&[3], "/etc/ssh/"), None);
    }

    #[pg_test]
    #[should_panic(expected = "jobs [98, 99] do not exist")]
    fn unknown_jobs_in_chain_are_an_error() {
        setup();
        chain(&[1, 99, 98], "/");
    }

    #[pg_test]
    #[should_panic(expected = "jobids must not be empty")]
    fn empty_chain_is_an_error() {
        setup();
        chain(&[], "/");
    }

    #[pg_test]
    #[should_panic(expected = "jobids must not contain NULL")]
    fn null_in_chain_is_an_error() {
        setup();
        crate::tree(vec![Some(1), None], "/", 9, 1_000_000);
    }

    #[pg_test]
    fn sql_api_for_chains() {
        setup();
        let get = |sql: &str| Spi::get_one::<Json>(sql).unwrap().unwrap().0;
        let chain = get("SELECT ovfs.tree(ARRAY[1, 3], '/etc/', 9)");
        assert_eq!(child(&chain, "passwd")["jobid"], 3);
        let single = get("SELECT ovfs.tree(1, '/etc/', 9)");
        assert_eq!(child(&single, "passwd")["jobid"], 1);
        assert_eq!(get("SELECT ovfs.tree(ARRAY[1], '/etc/', 9)"), single);
        let kind = Spi::get_one::<String>("SELECT pg_typeof(ovfs.tree(ARRAY[1], '/', 1))::text").unwrap();
        assert_eq!(kind.as_deref(), Some("json"));
        let limited = get("SELECT ovfs.tree(jobids => '{1,3}', root => '/etc/', depth => 9, max_entries => 10)");
        assert_eq!(limited, chain);
    }

    /// Clients and jobs for the most recent backup, as Bareos restore option 5:
    /// fd1 has two Fulls; 20 (FileSet version 3 with the same name) is the last.
    fn setup_clients() {
        setup();
        Spi::run(&format!(
            "INSERT INTO client (clientid, name, uname) VALUES \
                 (1, 'fd1', ''), (2, 'fd2', ''), (3, 'fd3', ''), (4, 'fd4', ''), (5, 'fd5', ''), (6, 'fd6', ''); \
             INSERT INTO fileset (filesetid, fileset, md5, createtime) VALUES \
                 (1, 'LinuxAll', 'a', now()), (2, 'Other', 'b', now()), (3, 'LinuxAll', 'c', now()); \
             INSERT INTO media (mediaid, volumename, mediatype, volstatus, enabled) VALUES \
                 (1, 'Full-0001', 'File', 'Full', 1), (2, 'Full-0002', 'File', 'Full', 0); \
             UPDATE media SET inchanger = 1 WHERE mediaid = 1; \
             INSERT INTO job (jobid, job, name, type, level, jobstatus, clientid, filesetid, jobtdate, starttime) VALUES \
                 (10, 'j10', 'b', 'B', 'F', 'T', 1, 1, 100, '2026-10-01'), \
                 (11, 'j11', 'b', 'B', 'I', 'T', 1, 1, 110, '2026-10-02'), \
                 (20, 'j20', 'b', 'B', 'F', 'T', 1, 3, 200, '2026-10-08'), \
                 (21, 'j21', 'b', 'B', 'I', 'T', 1, 3, 210, '2026-10-09'), \
                 (22, 'j22', 'b', 'B', 'D', 'T', 1, 3, 220, '2026-10-10'), \
                 (23, 'j23', 'b', 'B', 'I', 'T', 1, 3, 230, '2026-10-11'), \
                 (24, 'j24', 'b', 'B', 'I', 'E', 1, 3, 240, '2026-10-12'), \
                 (25, 'j25', 'b', 'B', 'I', 'T', 1, 3, 250, '2026-10-13'), \
                 (26, 'j26', 'b', 'B', 'I', 'W', 1, 3, 260, '2026-10-14'), \
                 (27, 'j27', 'r', 'R', 'F', 'T', 1, 3, 270, '2026-10-15'), \
                 (28, 'j28', 'b', 'B', 'I', 'T', 1, 3, 280, localtimestamp + interval '1 day'), \
                 (30, 'j30', 'b', 'B', 'F', 'T', 2, 1, 300, '2026-10-01'), \
                 (31, 'j31', 'b', 'B', 'F', 'T', 2, 2, 310, '2026-10-01'), \
                 (40, 'j40', 'b', 'B', 'F', 'T', 4, 1, 400, '2026-10-01'), \
                 (50, 'j50', 'b', 'B', 'F', 'T', 5, 1, 500, '2026-10-01'), \
                 (51, 'j51', 'b', 'B', 'I', 'T', 5, 1, 510, '2026-10-02'), \
                 (60, 'j60', 'b', 'B', 'B', 'T', 6, 1, 600, '2026-10-01'), \
                 (61, 'j61', 'b', 'B', 'F', 'T', 6, 1, 610, '2026-10-02'); \
             INSERT INTO jobmedia (jobid, mediaid, firstindex, lastindex) \
                 SELECT jobid, CASE WHEN jobid IN (25, 40, 50) THEN 2 ELSE 1 END, 1, 100 FROM job WHERE jobid >= 10; \
             INSERT INTO path (pathid, path) VALUES (10, '/srv/'), (11, '/opt/'), (12, '/vm/'); \
             INSERT INTO file (fileindex, jobid, pathid, lstat, md5, name) VALUES \
                 (1, 20, 10, '{FILE_644}', '', 'a'), \
                 (2, 20, 10, '{FILE_644}', '', 'b'), \
                 (1, 21, 10, '{FILE_644}', '', 'only-before-diff'), \
                 (1, 23, 10, '{FILE_600}', '', 'a'), \
                 (0, 23, 10, '{FILE_644}', '', 'b'), \
                 (1, 25, 10, '{FILE_644}', '', 'on-disabled-volume'), \
                 (1, 26, 10, '{FILE_644}', '', 'd'), \
                 (1, 10, 10, '{FILE_644}', '', 'from-old-full'), \
                 (1, 60, 11, '{FILE_644}', '', 'lib.so'), \
                 (2, 60, 11, '{FILE_644}', '', 'not-referenced'), \
                 (1, 61, 11, '{FILE_644}', '', 'new'); \
             INSERT INTO basefiles (jobid, fileid, fileindex, basejobid) \
                 SELECT 61, fileid, fileindex, 60 FROM file WHERE jobid = 60 AND name = 'lib.so'; \
             INSERT INTO file (fileindex, jobid, pathid, lstat, md5, name, deltaseq) VALUES \
                 (3, 20, 12, '{FILE_644}', 'm20', 'disk.img', 0), \
                 (2, 22, 12, '{FILE_644}', 'm22', 'disk.img', 1), \
                 (4, 23, 12, '{FILE_644}', 'm23', 'disk.img', 2)"
        ))
        .unwrap();
    }

    #[pg_test]
    fn latest_chain_follows_bareos_restore() {
        setup_clients();
        assert_eq!(crate::jobids_latest("fd1", None), [20, 22, 23, 26]);
        assert_eq!(crate::jobids_latest("fd1", Some("LinuxAll")), [20, 22, 23, 26]);
        assert_eq!(crate::jobids_latest("fd2", Some("Other")), [31]);
    }

    #[pg_test]
    fn latest_tree_is_state_of_last_job() {
        setup_clients();
        let srv: Value = serde_json::from_str(&crate::tree_latest("fd1", "/srv/", 9, None, 1_000_000).unwrap().0).unwrap();
        assert_eq!(names(&srv), ["a", "d"]);
        assert_eq!(child(&srv, "a")["jobid"], 23);
        assert_eq!(child(&srv, "d")["jobid"], 26);
    }

    #[pg_test]
    #[should_panic(expected = "several FileSets [\"LinuxAll\", \"Other\"]; choose one")]
    fn several_filesets_need_a_choice() {
        setup_clients();
        crate::jobids_latest("fd2", None);
    }

    #[pg_test]
    #[should_panic(expected = "client \"nope\" does not exist")]
    fn unknown_client_is_an_error() {
        setup_clients();
        crate::jobids_latest("nope", None);
    }

    #[pg_test]
    #[should_panic(expected = "client \"fd3\" has no backups")]
    fn client_without_backups_is_an_error() {
        setup_clients();
        crate::jobids_latest("fd3", None);
    }

    #[pg_test]
    #[should_panic(expected = "no Full backup of client \"fd1\" with FileSet \"Other\" found")]
    fn fileset_without_full_is_an_error() {
        setup_clients();
        crate::jobids_latest("fd1", Some("Other"));
    }

    #[pg_test]
    #[should_panic(expected = "is on an enabled volume")]
    fn full_on_disabled_volume_only_is_an_error() {
        setup_clients();
        crate::jobids_latest("fd4", None);
    }

    #[pg_test]
    fn sql_api_for_latest() {
        setup_clients();
        let jobids = Spi::get_one::<Vec<i32>>("SELECT ovfs.jobids_latest('fd2', fileset => 'LinuxAll')").unwrap();
        assert_eq!(jobids, Some(vec![30]));
        let get = |sql: &str| Spi::get_one::<Json>(sql).unwrap().unwrap().0;
        let latest = get("SELECT ovfs.tree_latest('fd1', '/srv/', 9)");
        assert_eq!(latest, get("SELECT ovfs.tree(ovfs.jobids_latest('fd1'), '/srv/', 9)"));
        assert_eq!(names(&get("SELECT ovfs.tree(20, '/srv/', 9)")), ["a", "b"]);
    }

    #[pg_test]
    fn full_on_disabled_volume_warns_and_keeps_the_rest() {
        setup_clients();
        assert_eq!(crate::jobids_latest("fd5", None), [51]);
    }

    #[pg_test]
    fn base_job_files_are_part_of_the_tree() {
        setup_clients();
        let opt: Value = serde_json::from_str(&crate::tree(vec![Some(61)], "/opt/", 9, 1_000_000).unwrap().0).unwrap();
        assert_eq!(names(&opt), ["lib.so", "new"]);
        assert_eq!((child(&opt, "lib.so")["jobid"].as_i64(), child(&opt, "lib.so")["fileindex"].as_i64()), (Some(60), Some(1)));
        assert_eq!(child(&opt, "new")["jobid"], 61);
    }

    #[pg_test]
    fn delta_parts_across_the_chain() {
        setup_clients();
        let vm: Value = serde_json::from_str(&crate::tree_latest("fd1", "/vm/", 9, None, 1_000_000).unwrap().0).unwrap();
        let disk = child(&vm, "disk.img");
        assert_eq!((disk["jobid"].as_i64(), disk["fileindex"].as_i64(), disk["delta_seq"].as_i64()), (Some(23), Some(4), Some(2)));
        assert_eq!(disk["delta_parts"], json!([{"jobid": 20, "fileindex": 3}, {"jobid": 22, "fileindex": 2}]));
    }

    #[pg_test]
    fn ls_lists_one_level_like_tree() {
        setup();
        let rows = Spi::connect(|spi| {
            spi.select("SELECT name, kind, path, jobid, fileindex, entries FROM ovfs.ls(ARRAY[1, 3], '/etc')", None, &[])
                .unwrap()
                .map(|r| {
                    (
                        r.get::<String>(1).unwrap().unwrap(),
                        r.get::<String>(2).unwrap().unwrap(),
                        r.get::<String>(3).unwrap(),
                        r.get::<i32>(4).unwrap(),
                        r.get::<i32>(5).unwrap(),
                        r.get::<i64>(6).unwrap(),
                    )
                })
                .collect::<Vec<_>>()
        });
        assert_eq!(
            rows,
            [
                ("passwd".into(), "file".into(), None, Some(3), Some(2), None),
                ("ssh".into(), "directory".into(), Some("/etc/ssh/".into()), Some(1), Some(3), Some(1)),
            ]
        );
        let tree = chain(&[1, 3], "/").unwrap();
        let top = Spi::get_one::<Vec<String>>("SELECT array_agg(name) FROM ovfs.ls(ARRAY[1, 3], '/')").unwrap().unwrap();
        assert_eq!(top, names(&tree));
        let empty = Spi::get_one::<i64>("SELECT count(*) FROM ovfs.ls(ARRAY[1], '/nothing/')").unwrap();
        assert_eq!(empty, Some(0));
    }

    #[pg_test]
    fn ls_shows_delta_parts() {
        setup_clients();
        let parts = Spi::get_one::<Json>("SELECT delta_parts FROM ovfs.ls(ovfs.jobids_latest('fd1'), '/vm/')").unwrap().unwrap().0;
        assert_eq!(parts, json!([{"jobid": 20, "fileindex": 3}, {"jobid": 22, "fileindex": 2}]));
    }

    #[pg_test]
    fn versions_lists_all_backups_of_a_file() {
        setup_clients();
        let rows = Spi::connect(|spi| {
            spi.select("SELECT jobid, level, fileindex, volumes, inchanger, mode FROM ovfs.versions('fd1', '/srv', 'a')", None, &[])
                .unwrap()
                .map(|r| {
                    (
                        r.get::<i32>(1).unwrap().unwrap(),
                        r.get::<String>(2).unwrap().unwrap(),
                        r.get::<i32>(3).unwrap().unwrap(),
                        r.get::<Vec<String>>(4).unwrap().unwrap(),
                        r.get::<bool>(5).unwrap().unwrap(),
                        r.get::<i64>(6).unwrap().unwrap(),
                    )
                })
                .collect::<Vec<_>>()
        });
        assert_eq!(
            rows,
            [
                (23, "I".into(), 1, vec!["Full-0001".to_string()], true, 0o100600),
                (20, "F".into(), 1, vec!["Full-0001".to_string()], true, 0o100644),
            ]
        );
        let deltas = Spi::get_one::<Vec<i32>>("SELECT array_agg(delta_seq) FROM ovfs.versions('fd1', '/vm/', 'disk.img')").unwrap();
        assert_eq!(deltas, Some(vec![2, 1, 0]));
        let other = Spi::get_one::<i64>("SELECT count(*) FROM ovfs.versions('fd2', '/srv/', 'a')").unwrap();
        assert_eq!(other, Some(0));
    }

    #[pg_test]
    fn depth_one_tree_is_ls_as_json() {
        setup();
        let get = |sql: &str| Spi::get_one::<Json>(sql).unwrap().unwrap().0;
        let top = get("SELECT ovfs.tree(ARRAY[1, 3], '/', 1)");
        let from_ls = get(
            "SELECT json_agg(json_build_object('name', name, 'entries', entries)) FROM ovfs.ls(ARRAY[1, 3], '/')",
        );
        let from_tree: Vec<Value> = top["children"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| json!({"name": c["name"], "entries": c["entries"]}))
            .collect();
        assert_eq!(Value::Array(from_tree), from_ls);
        assert!(top["children"][0].get("children").is_none());
        let etc = get("SELECT ovfs.tree(ARRAY[1, 3], '/', 2)");
        assert_eq!(names(child(&etc, "etc")), ["passwd", "ssh"]);
        assert_eq!(child(child(&etc, "etc"), "ssh")["entries"], 1);
    }

    #[pg_test]
    #[should_panic(expected = "depth must not be negative")]
    fn negative_depth_is_an_error() {
        setup();
        crate::tree(vec![Some(1)], "/", -1, 1_000_000);
    }
}

/// This module is required by `cargo pgrx test` invocations.
/// It must be visible at the root of your extension crate.
#[cfg(test)]
pub mod pg_test {
    pub fn setup(_options: Vec<&str>) {
        // perform one-off initialization when the pg_test framework starts
    }

    #[must_use]
    pub fn postgresql_conf_options() -> Vec<&'static str> {
        // return any postgresql.conf settings that are required for your tests
        vec![]
    }
}
