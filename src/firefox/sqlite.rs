//! Read Firefox profile groups through macOS's SQLite library.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr;

const SQLITE_OK: c_int = 0;
const SQLITE_ROW: c_int = 100;
const SQLITE_DONE: c_int = 101;
const SQLITE_TEXT: c_int = 3;
const SQLITE_BLOB: c_int = 4;
const SQLITE_FLOAT: c_int = 2;
const SQLITE_OPEN_READONLY: c_int = 1;
const SQLITE_NOMEM: c_int = 7;

#[link(name = "sqlite3")]
unsafe extern "C" {
    fn sqlite3_open_v2(
        path: *const c_char,
        db: *mut *mut c_void,
        flags: c_int,
        vfs: *const c_char,
    ) -> c_int;
    fn sqlite3_close(db: *mut c_void) -> c_int;
    fn sqlite3_busy_timeout(db: *mut c_void, ms: c_int) -> c_int;
    fn sqlite3_errmsg(db: *mut c_void) -> *const c_char;
    fn sqlite3_errcode(db: *mut c_void) -> c_int;
    fn sqlite3_prepare_v2(
        db: *mut c_void,
        sql: *const c_char,
        len: c_int,
        stmt: *mut *mut c_void,
        tail: *mut *const c_char,
    ) -> c_int;
    fn sqlite3_step(stmt: *mut c_void) -> c_int;
    fn sqlite3_finalize(stmt: *mut c_void) -> c_int;
    fn sqlite3_column_type(stmt: *mut c_void, col: c_int) -> c_int;
    fn sqlite3_column_double(stmt: *mut c_void, col: c_int) -> f64;
    fn sqlite3_column_text(stmt: *mut c_void, col: c_int) -> *const u8;
    fn sqlite3_column_blob(stmt: *mut c_void, col: c_int) -> *const c_void;
    fn sqlite3_column_bytes(stmt: *mut c_void, col: c_int) -> c_int;
}

struct Database(*mut c_void);
struct Statement<'a> {
    pointer: *mut c_void,
    db: &'a Database,
}

impl Drop for Database {
    fn drop(&mut self) {
        // Statements are dropped before their connection.
        unsafe { sqlite3_close(self.0) };
    }
}

impl Drop for Statement<'_> {
    fn drop(&mut self) {
        unsafe { sqlite3_finalize(self.pointer) };
    }
}

impl Database {
    fn open(path: &Path) -> Result<Self, String> {
        let path = CString::new(path.as_os_str().as_bytes()).map_err(|error| error.to_string())?;
        let mut pointer = ptr::null_mut();
        let result = unsafe {
            sqlite3_open_v2(
                path.as_ptr(),
                &mut pointer,
                SQLITE_OPEN_READONLY,
                ptr::null(),
            )
        };
        if pointer.is_null() {
            return Err("SQLite could not allocate a database connection".to_string());
        }
        let db = Self(pointer);
        if result != SQLITE_OK {
            return Err(db.error());
        }
        if unsafe { sqlite3_busy_timeout(db.0, 500) } != SQLITE_OK {
            return Err(db.error());
        }
        Ok(db)
    }

    fn error(&self) -> String {
        let message = unsafe { sqlite3_errmsg(self.0) };
        if message.is_null() {
            return "unknown SQLite error".to_string();
        }
        unsafe { CStr::from_ptr(message) }
            .to_string_lossy()
            .into_owned()
    }

    fn prepare(&self) -> Result<Statement<'_>, String> {
        let mut statement = ptr::null_mut();
        let result = unsafe {
            sqlite3_prepare_v2(
                self.0,
                c"SELECT name, path FROM Profiles ORDER BY id;".as_ptr(),
                -1,
                &mut statement,
                ptr::null_mut(),
            )
        };
        if result != SQLITE_OK {
            return Err(self.error());
        }
        if statement.is_null() {
            return Err("SQLite returned an empty statement".to_string());
        }
        Ok(Statement {
            pointer: statement,
            db: self,
        })
    }
}

impl Statement<'_> {
    fn string(&self, col: c_int) -> Result<Option<String>, String> {
        let kind = unsafe { sqlite3_column_type(self.pointer, col) };
        let pointer = match kind {
            SQLITE_TEXT => unsafe { sqlite3_column_text(self.pointer, col) }.cast::<c_void>(),
            SQLITE_BLOB => unsafe { sqlite3_column_blob(self.pointer, col) },
            SQLITE_FLOAT if !unsafe { sqlite3_column_double(self.pointer, col) }.is_finite() => {
                return Err("invalid numeric value in Firefox profile group".to_string());
            }
            _ => return Ok(None),
        };
        let length = unsafe { sqlite3_column_bytes(self.pointer, col) } as usize;
        if pointer.is_null() {
            if unsafe { sqlite3_errcode(self.db.0) } == SQLITE_NOMEM {
                return Err(self.db.error());
            }
            return if length == 0 {
                Ok(Some(String::new()))
            } else {
                Err("SQLite returned an empty column pointer".to_string())
            };
        }
        // SQLite owns the bytes until the next step; copy them before advancing.
        let bytes = unsafe { std::slice::from_raw_parts(pointer.cast::<u8>(), length) };
        if kind == SQLITE_BLOB {
            return Ok(Some(bytes.iter().map(|byte| char::from(*byte)).collect()));
        }
        let bytes = bytes.split(|byte| *byte == 0).next().unwrap_or_default();
        let value = std::str::from_utf8(bytes).map_err(|error| error.to_string())?;
        Ok(Some(value.to_string()))
    }
}

pub(super) fn profiles(path: &Path, root: &Path) -> Result<Vec<(String, PathBuf)>, String> {
    let db = Database::open(path)?;
    let statement = db.prepare()?;
    let mut profiles = Vec::new();
    loop {
        match unsafe { sqlite3_step(statement.pointer) } {
            SQLITE_DONE => return Ok(profiles),
            SQLITE_ROW => {
                if let (Some(name), Some(path)) = (statement.string(0)?, statement.string(1)?)
                    && !name.is_empty()
                    && !path.is_empty()
                {
                    profiles.push((name, root.join(path)));
                }
            }
            _ => return Err(db.error()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::profiles;
    use std::io::{BufRead, BufReader, Write};
    use std::path::{Path, PathBuf};
    use std::process::{Child, ChildStdin, Command, Stdio};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    fn directory() -> PathBuf {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "grinch-sqlite-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn writer(path: &Path, sql: &str) -> (Child, ChildStdin) {
        let mut child = Command::new("/usr/bin/sqlite3")
            .arg("-batch")
            .arg(path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut input = child.stdin.take().unwrap();
        writeln!(input, "{sql}\nSELECT 'grinch-ready';").unwrap();
        input.flush().unwrap();
        let mut output = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        loop {
            line.clear();
            assert_ne!(
                output.read_line(&mut line).unwrap(),
                0,
                "sqlite writer exited"
            );
            if line.trim() == "grinch-ready" {
                break;
            }
        }
        (child, input)
    }

    #[test]
    fn reads_live_wal_and_matches_cli_values_without_changing_data() {
        let root = directory();
        let path = root.join("group.sqlite");
        let (mut child, input) = writer(
            &path,
            r#"
            PRAGMA journal_mode=WAL;
            PRAGMA wal_autocheckpoint=0;
            CREATE TABLE Profiles(id INTEGER, name, path);
            INSERT INTO Profiles VALUES
                (1, 'Work', 'Profiles/work'),
                (2, x'c3a9', x'61'),
                (3, CAST(x'610062' AS TEXT), 'Profiles/a'),
                (4, 123, 'ignored'),
                (5, 'ignored', NULL);
        "#,
        );
        let wal = root.join("group.sqlite-wal");
        let before = std::fs::read(&path).unwrap();
        let before_wal = std::fs::read(&wal).unwrap();
        let output = Command::new("/usr/bin/sqlite3")
            .args(["-batch", "-readonly", "-json", "-init", "/dev/null"])
            .arg(&path)
            .arg("SELECT name, path FROM Profiles ORDER BY id;")
            .output()
            .unwrap();
        assert!(output.status.success());
        let rows: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout).unwrap();
        let expected: Vec<_> = rows
            .iter()
            .filter_map(|row| {
                let name = row["name"].as_str()?;
                let path = row["path"].as_str()?;
                (!name.is_empty() && !path.is_empty()).then(|| (name.to_string(), root.join(path)))
            })
            .collect();
        assert_eq!(profiles(&path, &root).unwrap(), expected);
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(std::fs::read(&wal).unwrap(), before_wal);
        drop(input);
        assert!(child.wait().unwrap().success());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn read_only_errors_do_not_create_files_and_locked_store_times_out() {
        let root = directory();
        let missing = root.join("missing.sqlite");
        assert!(profiles(&missing, &root).is_err());
        assert!(!missing.exists());
        let corrupt = root.join("corrupt.sqlite");
        std::fs::write(&corrupt, "not a database").unwrap();
        assert!(profiles(&corrupt, &root).is_err());
        let locked = root.join("locked.sqlite");
        let (mut child, input) = writer(
            &locked,
            "CREATE TABLE Profiles(id INTEGER, name, path); BEGIN EXCLUSIVE;",
        );
        let start = Instant::now();
        let error = profiles(&locked, &root).unwrap_err();
        assert!(error.contains("locked"), "{error}");
        assert!(start.elapsed() >= Duration::from_millis(400));
        assert!(start.elapsed() < Duration::from_secs(5));
        drop(input);
        assert!(child.wait().unwrap().success());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_values_that_made_the_cli_output_invalid() {
        let root = directory();
        for (name, value) in [("utf8", "CAST(x'ff' AS TEXT)"), ("infinity", "1e999")] {
            let path = root.join(format!("{name}.sqlite"));
            let sql = format!(
                "CREATE TABLE Profiles(id INTEGER, name, path); INSERT INTO Profiles VALUES (1, {value}, 'ignored'), (2, 'Work', 'Profiles/work');"
            );
            let status = Command::new("/usr/bin/sqlite3")
                .arg(&path)
                .arg(sql)
                .status()
                .unwrap();
            assert!(status.success());
            let output = Command::new("/usr/bin/sqlite3")
                .args(["-json", "-readonly"])
                .arg(&path)
                .arg("SELECT name, path FROM Profiles ORDER BY id;")
                .output()
                .unwrap();
            assert!(output.status.success());
            assert!(serde_json::from_slice::<serde_json::Value>(&output.stdout).is_err());
            assert!(profiles(&path, &root).is_err(), "{name}");
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
