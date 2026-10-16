//! `ovfs`: Bareos catalog directory trees as JSON for restores.
//!
//! Reads the Bareos catalog (schema 2250) unchanged. Functions are
//! `SECURITY INVOKER`: callers need SELECT on `path`, `file` and `job`,
//! and the tables are resolved through the caller's `search_path`.

use pgrx::prelude::*;
use pgrx::JsonB;

use ovfs_core::tree::{self, Entry};

::pgrx::pg_module_magic!(name, version);

/// Rows of the job below the root, read through `path_name_idx`
/// (range scan) and `file_jpfid_idx` (jobid, pathid).
const TREE_RANGE_SQL: &str = "\
    SELECT textsend(p.path), textsend(f.name), f.fileid, f.fileindex, f.lstat \
    FROM path p JOIN file f ON f.pathid = p.pathid \
    WHERE f.jobid = $1 AND p.path >= $2 AND p.path < $3";

/// Same rows without relying on byte order of the database collation.
const TREE_PREFIX_SQL: &str = "\
    SELECT textsend(p.path), textsend(f.name), f.fileid, f.fileindex, f.lstat \
    FROM path p JOIN file f ON f.pathid = p.pathid \
    WHERE f.jobid = $1 AND starts_with(p.path, $2)";

/// Byte-wise ordering, as Bareos creates its database (`LC_COLLATE 'C'`).
const BYTEWISE_COLLATION_SQL: &str = "\
    SELECT datlocprovider = 'c' AND datcollate IN ('C', 'POSIX') \
        OR datlocprovider = 'b' AND datlocale = 'C' \
    FROM pg_database WHERE datname = current_database()";

const JOB_EXISTS_SQL: &str = "SELECT EXISTS (SELECT 1 FROM job WHERE jobid = $1)";

fn invalid(message: String) -> ! {
    ereport!(ERROR, PgSqlErrorCode::ERRCODE_INVALID_PARAMETER_VALUE, message);
}

/// Directory tree of one job below `root` as JSON.
///
/// Returns NULL if the job has no live entry at or below `root`. Raises an
/// error for an unknown job, an invalid root, or more than `max_entries`
/// catalog rows below `root`.
#[pg_extern(stable, parallel_safe)]
fn tree(jobid: i32, root: &str, max_entries: default!(i64, 1000000)) -> Option<JsonB> {
    let root = tree::normalize_root(root).unwrap_or_else(|_| invalid(format!("invalid root directory: {root:?}")));
    if max_entries < 1 {
        invalid(format!("max_entries must be positive, got {max_entries}"));
    }

    let entries = Spi::connect(|client| -> Result<Vec<Entry>, pgrx::spi::Error> {
        let exists = client
            .select(JOB_EXISTS_SQL, Some(1), &[jobid.into()])?
            .first()
            .get_one::<bool>()?
            .unwrap_or(false);
        if !exists {
            invalid(format!("job {jobid} does not exist"));
        }

        // Paths are compared as text; root is valid UTF-8 and ends with '/',
        // so the upper bound only bumps that final byte and stays valid text.
        let root_text = String::from_utf8(root.clone()).expect("root comes from &str");
        let bytewise = client.select(BYTEWISE_COLLATION_SQL, Some(1), &[])?.first().get_one::<bool>()?.unwrap_or(false);
        let limit = Some(max_entries.saturating_add(1));
        let rows = match (bytewise, tree::prefix_upper_bound(&root)) {
            (true, Some(upper)) => {
                let upper = String::from_utf8(upper).expect("only the trailing '/' is bumped");
                client.select(TREE_RANGE_SQL, limit, &[jobid.into(), root_text.into(), upper.into()])?
            }
            _ => client.select(TREE_PREFIX_SQL, limit, &[jobid.into(), root_text.into()])?,
        };

        if rows.len() as i64 > max_entries {
            ereport!(
                ERROR,
                PgSqlErrorCode::ERRCODE_PROGRAM_LIMIT_EXCEEDED,
                format!("more than {max_entries} catalog entries below the root; raise max_entries or choose a deeper root")
            );
        }

        let mut entries = Vec::with_capacity(rows.len());
        for row in rows {
            entries.push(Entry {
                path: row.get::<Vec<u8>>(1)?.unwrap_or_default(),
                name: row.get::<Vec<u8>>(2)?.unwrap_or_default(),
                fileid: row.get::<i64>(3)?.unwrap_or_default(),
                fileindex: row.get::<i32>(4)?.unwrap_or_default(),
                lstat: row.get::<String>(5)?.unwrap_or_default(),
            });
        }
        Ok(entries)
    })
    .unwrap_or_else(|error| error!("catalog query failed: {error}"));

    tree::build(&root, entries).map(JsonB)
}

#[cfg(any(test, feature = "pg_test"))]
#[pg_schema]
mod tests {
    use pgrx::prelude::*;
    use pgrx::JsonB;
    use serde_json::{json, Value};

    const BAREOS_DDL: &str = include_str!("../../tests/fixtures/bareos-25.1.2-pre-postgresql.sql");

    /// LStat with mode, size and mtime set, in Bareos base64 digits:
    /// dev ino mode nlink uid gid rdev size blksize blocks atime mtime ctime LinkFI flags stream
    const DIR: &str = "A A EHt B A A A A BAA A A BlU/EA A A A A";
    const FILE_644: &str = "A A IGk B A A A BJn BAA A A BlU/EA A A A A";

    fn setup() {
        // The DDL wraps one block in begin/commit, which SPI does not allow
        let ddl = BAREOS_DDL.replace("\nbegin;\n", "\n").replace("\ncommit;\n", "\n");
        Spi::run(&ddl).unwrap();
        Spi::run(&format!(
            "INSERT INTO job (jobid, job, name, type, level, jobstatus) VALUES \
                 (1, 'backup.2026-10-16', 'backup', 'B', 'F', 'T'), \
                 (2, 'other.2026-10-16', 'other', 'B', 'F', 'T'); \
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
                 (1, 2, 2, '{FILE_644}', '', 'other-job')"
        ))
        .unwrap();
    }

    fn tree(jobid: i32, root: &str) -> Option<Value> {
        crate::tree(jobid, root, 1_000_000).map(|JsonB(value)| value)
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
    #[should_panic(expected = "invalid root directory")]
    fn empty_root_is_an_error() {
        setup();
        tree(1, "");
    }

    #[pg_test]
    #[should_panic(expected = "more than 2 catalog entries")]
    fn max_entries_is_enforced() {
        setup();
        crate::tree(1, "/etc/", 2);
    }

    #[pg_test]
    fn sql_api() {
        setup();
        let json = Spi::get_one::<JsonB>("SELECT ovfs.tree(1, '/etc/ssh/')").unwrap().unwrap().0;
        assert_eq!(json["path"], json!("/etc/ssh/"));
        let null = Spi::get_one::<JsonB>("SELECT ovfs.tree(NULL, '/')").unwrap();
        assert!(null.is_none());
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
