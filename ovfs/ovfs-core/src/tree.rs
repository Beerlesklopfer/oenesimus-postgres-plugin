//! Build a JSON directory tree from the Bareos catalog rows of one job.
//!
//! Catalog conventions (Bareos schema 2250):
//! - `Path.Path` is a directory and always ends with `/` (`/`, `/etc/`, `C:/`).
//! - A directory's own attributes are a `File` row in that path with `Name = ''`.
//! - `FileIndex = 0` marks an entry deleted since the previous job (accurate
//!   mode); such rows are not part of the job's view and are skipped.
//! - Path and name are bytes in a `SQL_ASCII` database and need not be UTF-8.

use std::collections::BTreeMap;

use base64::Engine;
use serde_json::{json, Map, Value};

use crate::lstat;

/// One `File` row joined with its `Path`.
#[derive(Debug, Clone)]
pub struct Entry {
    pub path: Vec<u8>,
    pub name: Vec<u8>,
    pub fileid: i64,
    pub fileindex: i32,
    pub lstat: String,
}

#[derive(Debug, Clone)]
struct Attrs {
    fileid: i64,
    fileindex: i32,
    stat: Option<lstat::Stat>,
}

#[derive(Debug, Default)]
struct Dir {
    attrs: Option<Attrs>,
    dirs: BTreeMap<Vec<u8>, Dir>,
    files: BTreeMap<Vec<u8>, Attrs>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum TreeError {
    /// The root must be a directory path ending with `/`.
    InvalidRoot,
}

/// Normalize a directory path to the catalog form (trailing `/`).
pub fn normalize_root(root: &str) -> Result<Vec<u8>, TreeError> {
    if root.is_empty() || root.contains('\0') {
        return Err(TreeError::InvalidRoot);
    }
    let mut bytes = root.as_bytes().to_vec();
    if !bytes.ends_with(b"/") {
        bytes.push(b'/');
    }
    Ok(bytes)
}

/// Exclusive upper bound for a byte-wise range scan over all paths starting
/// with `prefix` (`prefix <= path < upper`, compared with `COLLATE "C"`).
/// `None` means there is no upper bound (prefix is all `0xff`).
pub fn prefix_upper_bound(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut upper = prefix.to_vec();
    while let Some(last) = upper.pop() {
        if last < 0xff {
            upper.push(last + 1);
            return Some(upper);
        }
    }
    None
}

/// Build the tree below `root`. Rows outside `root` are ignored.
/// Returns `None` if the job has no live entry at or below `root`.
pub fn build(root: &[u8], entries: impl IntoIterator<Item = Entry>) -> Option<Value> {
    let mut top = Dir::default();
    let mut found = false;

    for entry in entries {
        if entry.fileindex <= 0 {
            continue;
        }
        let Some(relative) = entry.path.strip_prefix(root) else {
            continue;
        };
        found = true;
        let dir = relative
            .split(|b| *b == b'/')
            .filter(|component| !component.is_empty())
            .fold(&mut top, |dir, component| dir.dirs.entry(component.to_vec()).or_default());
        let attrs = Attrs {
            fileid: entry.fileid,
            fileindex: entry.fileindex,
            stat: lstat::decode(&entry.lstat),
        };
        if entry.name.is_empty() {
            dir.attrs = Some(attrs);
        } else {
            dir.files.insert(entry.name, attrs);
        }
    }

    found.then(|| dir_json(root, root, &top))
}

fn text(map: &mut Map<String, Value>, key: &str, bytes: &[u8]) {
    match std::str::from_utf8(bytes) {
        Ok(s) => {
            map.insert(key.into(), json!(s));
        }
        Err(_) => {
            // Keep the exact bytes for restores; the readable form is lossy
            map.insert(key.into(), json!(String::from_utf8_lossy(bytes)));
            map.insert(
                format!("{key}_b64"),
                json!(base64::engine::general_purpose::STANDARD.encode(bytes)),
            );
        }
    }
}

fn attrs_json(map: &mut Map<String, Value>, attrs: &Attrs) {
    map.insert("fileid".into(), json!(attrs.fileid));
    map.insert("fileindex".into(), json!(attrs.fileindex));
    if let Some(stat) = attrs.stat {
        map.insert("mode".into(), json!(stat.mode));
        map.insert("uid".into(), json!(stat.uid));
        map.insert("gid".into(), json!(stat.gid));
        map.insert("size".into(), json!(stat.size));
        map.insert("mtime".into(), json!(stat.mtime));
        if stat.link_fi != 0 {
            map.insert("link_fileindex".into(), json!(stat.link_fi));
        }
    }
}

fn dir_json(name: &[u8], path: &[u8], dir: &Dir) -> Value {
    let mut map = Map::new();
    text(&mut map, "name", name);
    text(&mut map, "path", path);
    map.insert("type".into(), json!("directory"));
    if let Some(attrs) = &dir.attrs {
        attrs_json(&mut map, attrs);
    }

    // Children sorted by name bytes; directories before files on equal names
    let mut children: Vec<(&[u8], Value)> = Vec::with_capacity(dir.dirs.len() + dir.files.len());
    for (child, sub) in &dir.dirs {
        let mut child_path = path.to_vec();
        child_path.extend_from_slice(child);
        child_path.push(b'/');
        children.push((child, dir_json(child, &child_path, sub)));
    }
    for (child, attrs) in &dir.files {
        let mut file = Map::new();
        text(&mut file, "name", child);
        let kind = attrs.stat.map_or(lstat::Kind::Other, |s| s.kind());
        file.insert("type".into(), json!(kind.as_str()));
        attrs_json(&mut file, attrs);
        children.push((child, Value::Object(file)));
    }
    children.sort_by(|a, b| a.0.cmp(b.0));
    map.insert("children".into(), Value::Array(children.into_iter().map(|(_, v)| v).collect()));
    Value::Object(map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lstat::tests::encode;

    fn stat(mode: i64, size: i64) -> String {
        encode([0, 0, mode, 1, 0, 0, 0, size, 4096, 0, 0, 1_700_000_000, 0, 0, 0, 0])
    }

    fn row(path: &str, name: &str, fileid: i64, fileindex: i32, mode: i64) -> Entry {
        Entry {
            path: path.as_bytes().to_vec(),
            name: name.as_bytes().to_vec(),
            fileid,
            fileindex,
            lstat: stat(mode, fileid * 10),
        }
    }

    fn names(node: &Value) -> Vec<&str> {
        node["children"].as_array().unwrap().iter().map(|c| c["name"].as_str().unwrap()).collect()
    }

    #[test]
    fn root_normalization() {
        assert_eq!(normalize_root("/etc").unwrap(), b"/etc/");
        assert_eq!(normalize_root("/").unwrap(), b"/");
        assert_eq!(normalize_root("C:/").unwrap(), b"C:/");
        assert_eq!(normalize_root(""), Err(TreeError::InvalidRoot));
        assert_eq!(normalize_root("/a\0b"), Err(TreeError::InvalidRoot));
    }

    #[test]
    fn upper_bound() {
        assert_eq!(prefix_upper_bound(b"/etc/").unwrap(), b"/etc0");
        assert_eq!(prefix_upper_bound(b"/a\xff").unwrap(), b"/b");
        assert_eq!(prefix_upper_bound(b"\xff\xff"), None);
    }

    #[test]
    fn builds_nested_tree() {
        let tree = build(
            b"/etc/",
            [
                row("/etc/", "", 1, 1, 0o040755),
                row("/etc/", "passwd", 2, 2, 0o100644),
                row("/etc/ssh/", "", 3, 3, 0o040755),
                row("/etc/ssh/", "sshd_config", 4, 4, 0o100600),
                row("/etc/", "localtime", 5, 5, 0o120777),
                row("/var/", "log", 6, 6, 0o100644),
            ],
        )
        .unwrap();

        assert_eq!(tree["path"], "/etc/");
        assert_eq!(tree["fileid"], 1);
        assert_eq!(names(&tree), ["localtime", "passwd", "ssh"]);
        let children = tree["children"].as_array().unwrap();
        assert_eq!(children[0]["type"], "symlink");
        assert_eq!(children[1]["type"], "file");
        assert_eq!(children[1]["size"], 20);
        assert_eq!(children[2]["type"], "directory");
        assert_eq!(children[2]["path"], "/etc/ssh/");
        assert_eq!(names(&children[2]), ["sshd_config"]);
    }

    #[test]
    fn creates_intermediate_directories() {
        // Only a deep path was backed up; "/" and "/home/" have no own rows
        let tree = build(b"/", [row("/home/user/", "notes.txt", 1, 1, 0o100644)]).unwrap();
        assert!(tree.get("fileid").is_none());
        let home = &tree["children"][0];
        assert_eq!(home["path"], "/home/");
        assert!(home.get("fileid").is_none());
        assert_eq!(names(&home["children"][0]), ["notes.txt"]);
    }

    #[test]
    fn skips_deleted_entries() {
        let tree = build(
            b"/data/",
            [
                row("/data/", "kept", 1, 1, 0o100644),
                row("/data/", "deleted", 2, 0, 0o100644),
                row("/data/gone/", "", 3, 0, 0o040755),
            ],
        )
        .unwrap();
        assert_eq!(names(&tree), ["kept"]);
    }

    #[test]
    fn empty_result_is_none() {
        assert!(build(b"/nothing/", [row("/etc/", "passwd", 1, 1, 0o100644)]).is_none());
        assert!(build(b"/etc/", [row("/etc/", "deleted", 1, 0, 0o100644)]).is_none());
    }

    #[test]
    fn prefix_must_match_whole_component() {
        // "/etc/" must not match "/etcetera/" — guaranteed by the trailing slash
        assert!(build(b"/etc/", [row("/etcetera/", "x", 1, 1, 0o100644)]).is_none());
    }

    #[test]
    fn keeps_non_utf8_names_exactly() {
        let tree = build(
            b"/",
            [Entry { path: b"/".to_vec(), name: b"caf\xe9".to_vec(), fileid: 1, fileindex: 1, lstat: stat(0o100644, 1) }],
        )
        .unwrap();
        let file = &tree["children"][0];
        assert_eq!(file["name"], "caf\u{fffd}");
        assert_eq!(file["name_b64"], "Y2Fm6Q==");
    }

    #[test]
    fn unicode_and_spaces() {
        let tree = build(b"/", [row("/Übersicht mit Leerzeichen/", "日本語.txt", 1, 1, 0o100644)]).unwrap();
        let dir = &tree["children"][0];
        assert_eq!(dir["name"], "Übersicht mit Leerzeichen");
        assert!(dir.get("name_b64").is_none());
        assert_eq!(names(dir), ["日本語.txt"]);
    }

    #[test]
    fn output_is_deterministic() {
        let rows = vec![row("/b/", "z", 1, 1, 0o100644), row("/a/", "y", 2, 2, 0o100644), row("/", "x", 3, 3, 0o100644)];
        let mut reversed = rows.clone();
        reversed.reverse();
        assert_eq!(build(b"/", rows), build(b"/", reversed));
    }
}
