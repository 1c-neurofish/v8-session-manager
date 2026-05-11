//! Атомарная запись JSON-файлов через `tempfile::NamedTempFile::persist`.
//!
//! Использование: для файлов, которые могут одновременно читаться/писаться
//! и должны переживать crash менеджера без половинчатого состояния.
//! ADR-0035: `tools_cache.json` сохраняется этим путём.

use std::io;
use std::path::Path;

use serde::Serialize;

/// Атомарно записать сериализованный `value` в `path`.
///
/// Алгоритм: `tempfile::NamedTempFile::new_in(parent_dir)` → write → `sync_all` →
/// `persist(path)`. На POSIX `persist` использует `rename(2)`, на Windows
/// `MoveFileExW(MOVEFILE_REPLACE_EXISTING)` — оба варианта атомарны в
/// пределах одной файловой системы и заменяют существующий файл.
///
/// Если родительский каталог не существует, он создаётся (`fs::create_dir_all`).
pub fn write_json_atomic<T>(path: &Path, value: &T) -> io::Result<()>
where
    T: Serialize,
{
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("atomic_write: path '{}' has no parent dir", path.display()),
        )
    })?;
    if !parent.as_os_str().is_empty() {
        std::fs::create_dir_all(parent)?;
    }

    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer(&mut tmp, value).map_err(io_err)?;
    // fsync содержимого до атомарного rename.
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;

    // best-effort fsync директории (Linux/Unix): даёт устойчивость к crash
    // в окне между rename и записью dir-entry. На Windows File::open(dir)
    // не поддерживается стандартным API, поэтому только cfg(unix).
    #[cfg(unix)]
    {
        if let Ok(dir) = std::fs::File::open(parent) {
            let _ = dir.sync_all();
        }
    }

    Ok(())
}

fn io_err(err: serde_json::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    #[test]
    fn writes_json_to_new_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("out.json");
        write_json_atomic(&path, &json!({"a": 1})).unwrap();
        let read = std::fs::read_to_string(&path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&read).unwrap();
        assert_eq!(parsed, json!({"a": 1}));
    }

    #[test]
    fn replaces_existing_file_atomically() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("out.json");
        std::fs::write(&path, b"OLD").unwrap();
        write_json_atomic(&path, &json!({"b": 2})).unwrap();
        let read = std::fs::read_to_string(&path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&read).unwrap();
        assert_eq!(parsed, json!({"b": 2}));
    }

    #[test]
    fn creates_missing_parent_dirs() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nested/sub/out.json");
        write_json_atomic(&path, &json!([1, 2, 3])).unwrap();
        let read = std::fs::read_to_string(&path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&read).unwrap();
        assert_eq!(parsed, json!([1, 2, 3]));
    }

    #[test]
    fn no_leftover_tempfiles_after_success() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("out.json");
        write_json_atomic(&path, &json!({"x": "y"})).unwrap();
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name())
            .collect();
        // Должен остаться только сам файл out.json, никаких ".tmpXXXX".
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0], "out.json");
    }
}
