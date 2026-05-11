//! Persistent кеш проксированных tools (ADR-0035).
//!
//! Менеджер ведёт per-`(kind, config_id)` снимок `ToolDescriptor[]`,
//! полученный от 1С‑клиента в `session.register` / `session.tools_changed`.
//! Кеш авторитетен для `tools/list`: AI‑агент видит набор tools сразу
//! после рестарта менеджера, даже без живых WS‑сессий. Это страховка для
//! MCP‑харнесов, которые нестабильно реагируют на
//! `notifications/tools/list_changed` (в частности Claude Code).
//!
//! Дизайн:
//! * In-memory `HashMap<(kind, config_id), ToolsCacheEntry>` под `RwLock`.
//! * После каждой mutating операции — атомарная запись `tools_cache.json`
//!   в `storage_path` через [`crate::support::atomic_write`].
//! * Lazy eviction: вызов `list_all()` или `upsert(...)` сначала удаляет
//!   записи с `now - last_seen_at > cache_life`; никаких background-таймеров.
//! * Сигнал `tools/list_changed` поднимается через `notifier_hook` —
//!   тонкая лямбда, дёргающая `SessionRegistry::mark_tools_changed()`.
//!
//! `enabled: false` ⇒ кеш в no-op режиме: `upsert` / `list_all` / reset не
//! делают ничего и файл не трогается.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::session_manager::protocol::ToolDescriptor;
use crate::support::atomic_write::write_json_atomic;

/// Текущая версия on-disk формата `tools_cache.json`.
const FORMAT_VERSION: u32 = 1;

/// Снимок tools одной `(kind, config_id)` группы.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolsCacheEntry {
    pub kind: String,
    pub config_id: String,
    pub tools: Vec<ToolDescriptor>,
    pub last_seen_at: DateTime<Utc>,
}

/// On-disk представление кеша. Версионируется через `version` (R7).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct PersistedCache {
    version: u32,
    entries: Vec<ToolsCacheEntry>,
}

/// Конфигурация кеша. Парсится из `AppConfig.tools_cache`.
#[derive(Debug, Clone)]
pub struct ToolsCacheConfig {
    pub enabled: bool,
    pub cache_life: Duration,
    pub storage_path: PathBuf,
}

/// In-memory store + persistent backup.
///
/// `notifier_hook` — лямбда, которую store вызывает после mutating операций
/// (`upsert` с изменённым набором, eviction, reset). Внутри лямбды
/// предполагается вызов `SessionRegistry::mark_tools_changed()`.
pub struct ToolsCacheStore {
    inner: RwLock<HashMap<(String, String), ToolsCacheEntry>>,
    storage_path: PathBuf,
    cache_life: Duration,
    enabled: bool,
    notifier_hook: NotifierHook,
}

type NotifierHook = Arc<dyn Fn() + Send + Sync>;

impl ToolsCacheStore {
    /// Создаёт store, читая существующий файл (если есть). Любая ошибка
    /// чтения → warning + старт с пустым кешем (R7).
    pub fn load_or_empty(cfg: &ToolsCacheConfig, notifier_hook: NotifierHook) -> Arc<Self> {
        let inner = if cfg.enabled {
            load_from_disk(&cfg.storage_path)
        } else {
            HashMap::new()
        };
        Arc::new(Self {
            inner: RwLock::new(inner),
            storage_path: cfg.storage_path.clone(),
            cache_life: cfg.cache_life,
            enabled: cfg.enabled,
            notifier_hook,
        })
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Обновить запись `(kind, config_id)`. Lazy-evict до записи.
    /// Возвращает `true`, если состав tools изменился (или это новая запись).
    pub fn upsert(&self, kind: String, config_id: String, tools: Vec<ToolDescriptor>) -> bool {
        if !self.enabled {
            return false;
        }
        let evicted = self.evict_expired();
        let changed = {
            let mut guard = self.inner.write().expect("tools_cache poisoned");
            let key = (kind.clone(), config_id.clone());
            let now = Utc::now();
            match guard.get_mut(&key) {
                Some(existing) => {
                    existing.last_seen_at = now;
                    if existing.tools != tools {
                        existing.tools = tools;
                        true
                    } else {
                        false
                    }
                }
                None => {
                    guard.insert(
                        key,
                        ToolsCacheEntry {
                            kind,
                            config_id,
                            tools,
                            last_seen_at: now,
                        },
                    );
                    true
                }
            }
        };
        if changed || evicted {
            self.persist();
        }
        if changed {
            (self.notifier_hook)();
        }
        changed
    }

    /// Снимок всех записей после lazy-eviction.
    pub fn list_all(&self) -> Vec<ToolsCacheEntry> {
        if !self.enabled {
            return Vec::new();
        }
        let _ = self.evict_expired();
        let guard = self.inner.read().expect("tools_cache poisoned");
        let mut out: Vec<ToolsCacheEntry> = guard.values().cloned().collect();
        out.sort_by(|a, b| {
            a.kind
                .cmp(&b.kind)
                .then_with(|| a.config_id.cmp(&b.config_id))
        });
        out
    }

    /// Очистить весь кеш. Шлёт `tools/list_changed`, если что-то реально
    /// было удалено.
    pub fn reset_all(&self) -> usize {
        if !self.enabled {
            return 0;
        }
        let removed = {
            let mut guard = self.inner.write().expect("tools_cache poisoned");
            let n = guard.len();
            guard.clear();
            n
        };
        if removed > 0 {
            self.persist();
            (self.notifier_hook)();
        }
        removed
    }

    /// Удалить все записи с указанным `config_id` (среди всех `kind`).
    pub fn reset_by_config_id(&self, config_id: &str) -> usize {
        if !self.enabled {
            return 0;
        }
        let removed = {
            let mut guard = self.inner.write().expect("tools_cache poisoned");
            let before = guard.len();
            guard.retain(|(_, cid), _| cid != config_id);
            before - guard.len()
        };
        if removed > 0 {
            self.persist();
            (self.notifier_hook)();
        }
        removed
    }

    /// Удалить записи, у которых `now - last_seen_at > cache_life`.
    /// Возвращает `true`, если что-то удалили (caller сам решает, нужно ли
    /// слать `tools/list_changed` — в `list_all` мы не уведомляем, чтобы
    /// чтение не флапало; в `upsert` уведомление за нас сделает сам upsert).
    fn evict_expired(&self) -> bool {
        let now = Utc::now();
        let cache_life = chrono::Duration::from_std(self.cache_life)
            .unwrap_or_else(|_| chrono::Duration::days(365 * 100));
        let removed_any = {
            let mut guard = self.inner.write().expect("tools_cache poisoned");
            let before = guard.len();
            guard.retain(|_, entry| now.signed_duration_since(entry.last_seen_at) <= cache_life);
            before != guard.len()
        };
        if removed_any {
            // Запись отражает обновлённое состояние; нотификация — задача
            // вызывающего (например, после list_all мы её не шлём,
            // потому что list уже отдаёт корректный набор).
            self.persist();
            // Однако: ADR-0035 R4 требует list_changed при eviction.
            // Дёргаем хук — coalescer 200мс защитит от шторма.
            (self.notifier_hook)();
        }
        removed_any
    }

    fn persist(&self) {
        if !self.enabled {
            return;
        }
        let entries: Vec<ToolsCacheEntry> = {
            let guard = self.inner.read().expect("tools_cache poisoned");
            let mut v: Vec<ToolsCacheEntry> = guard.values().cloned().collect();
            v.sort_by(|a, b| {
                a.kind
                    .cmp(&b.kind)
                    .then_with(|| a.config_id.cmp(&b.config_id))
            });
            v
        };
        let snap = PersistedCache {
            version: FORMAT_VERSION,
            entries,
        };
        if let Err(err) = write_json_atomic(&self.storage_path, &snap) {
            warn!(
                ?err,
                path = %self.storage_path.display(),
                "tools_cache: persist failed"
            );
        }
    }
}

impl std::fmt::Debug for ToolsCacheStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolsCacheStore")
            .field("storage_path", &self.storage_path)
            .field("cache_life", &self.cache_life)
            .field("enabled", &self.enabled)
            .finish()
    }
}

fn load_from_disk(path: &Path) -> HashMap<(String, String), ToolsCacheEntry> {
    if !path.exists() {
        return HashMap::new();
    }
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(err) => {
            warn!(?err, path = %path.display(), "tools_cache: failed to read file, starting empty");
            return HashMap::new();
        }
    };
    let parsed: PersistedCache = match serde_json::from_str(&text) {
        Ok(p) => p,
        Err(err) => {
            warn!(
                ?err,
                path = %path.display(),
                "tools_cache: failed to parse cache JSON, starting empty"
            );
            return HashMap::new();
        }
    };
    if parsed.version != FORMAT_VERSION {
        warn!(
            version = parsed.version,
            expected = FORMAT_VERSION,
            "tools_cache: unsupported format version, starting empty"
        );
        return HashMap::new();
    }
    parsed
        .entries
        .into_iter()
        .map(|e| ((e.kind.clone(), e.config_id.clone()), e))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::tempdir;

    fn td(name: &str) -> ToolDescriptor {
        ToolDescriptor {
            name: name.to_owned(),
            description: None,
            input_schema: json!({"type": "object"}),
        }
    }

    fn cfg(dir: &Path, life: Duration) -> ToolsCacheConfig {
        ToolsCacheConfig {
            enabled: true,
            cache_life: life,
            storage_path: dir.join("tools_cache.json"),
        }
    }

    #[test]
    fn upsert_and_list_returns_entries() {
        let dir = tempdir().unwrap();
        let counter = Arc::new(AtomicUsize::new(0));
        let hook = {
            let c = Arc::clone(&counter);
            Arc::new(move || {
                c.fetch_add(1, Ordering::SeqCst);
            }) as NotifierHook
        };
        let store = ToolsCacheStore::load_or_empty(&cfg(dir.path(), Duration::from_secs(60)), hook);
        let changed = store.upsert("DRIVE".to_owned(), "DRIVE".to_owned(), vec![td("find")]);
        assert!(changed);
        let entries = store.list_all();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].kind, "DRIVE");
        assert_eq!(entries[0].config_id, "DRIVE");
        assert_eq!(entries[0].tools[0].name, "find");
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn upsert_identical_tools_does_not_signal() {
        let dir = tempdir().unwrap();
        let counter = Arc::new(AtomicUsize::new(0));
        let hook = {
            let c = Arc::clone(&counter);
            Arc::new(move || {
                c.fetch_add(1, Ordering::SeqCst);
            }) as NotifierHook
        };
        let store = ToolsCacheStore::load_or_empty(&cfg(dir.path(), Duration::from_secs(60)), hook);
        store.upsert("DRIVE".to_owned(), "DRIVE".to_owned(), vec![td("find")]);
        let changed = store.upsert("DRIVE".to_owned(), "DRIVE".to_owned(), vec![td("find")]);
        assert!(!changed, "identical upsert should report no change");
        assert_eq!(counter.load(Ordering::SeqCst), 1, "single signal");
    }

    /// T3 из плана тестов: eviction по TTL + notifier дёрнут.
    #[test]
    fn evicts_entries_older_than_cache_life() {
        let dir = tempdir().unwrap();
        let counter = Arc::new(AtomicUsize::new(0));
        let hook = {
            let c = Arc::clone(&counter);
            Arc::new(move || {
                c.fetch_add(1, Ordering::SeqCst);
            }) as NotifierHook
        };
        let store =
            ToolsCacheStore::load_or_empty(&cfg(dir.path(), Duration::from_millis(60)), hook);
        store.upsert("DRIVE".to_owned(), "DRIVE".to_owned(), vec![td("find")]);
        assert_eq!(counter.load(Ordering::SeqCst), 1);
        std::thread::sleep(Duration::from_millis(90));
        let entries = store.list_all();
        assert!(entries.is_empty(), "entry must be evicted after TTL");
        // notifier дёрнут: 1 раз за upsert + 1 раз за eviction = 2
        assert_eq!(counter.load(Ordering::SeqCst), 2);

        let on_disk = std::fs::read_to_string(dir.path().join("tools_cache.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&on_disk).unwrap();
        assert_eq!(parsed["entries"].as_array().unwrap().len(), 0);
    }

    /// T4: reset_by_config_id чистит только указанный config_id.
    #[test]
    fn reset_by_config_id_removes_only_target() {
        let dir = tempdir().unwrap();
        let store = ToolsCacheStore::load_or_empty(
            &cfg(dir.path(), Duration::from_secs(60)),
            Arc::new(|| {}),
        );
        store.upsert("DRIVE".to_owned(), "DRIVE".to_owned(), vec![td("a")]);
        store.upsert("PAM".to_owned(), "PAM".to_owned(), vec![td("b")]);
        let removed = store.reset_by_config_id("DRIVE");
        assert_eq!(removed, 1);
        let entries = store.list_all();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].kind, "PAM");
    }

    #[test]
    fn reset_all_clears_everything() {
        let dir = tempdir().unwrap();
        let store = ToolsCacheStore::load_or_empty(
            &cfg(dir.path(), Duration::from_secs(60)),
            Arc::new(|| {}),
        );
        store.upsert("DRIVE".to_owned(), "DRIVE".to_owned(), vec![td("a")]);
        store.upsert("PAM".to_owned(), "PAM".to_owned(), vec![td("b")]);
        assert_eq!(store.reset_all(), 2);
        assert!(store.list_all().is_empty());
    }

    /// T6: corrupted file → empty cache + warn, без паники.
    #[test]
    fn load_or_empty_with_corrupted_json_starts_empty() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("tools_cache.json");
        std::fs::write(&path, b"{not-json").unwrap();
        let store = ToolsCacheStore::load_or_empty(
            &ToolsCacheConfig {
                enabled: true,
                cache_life: Duration::from_secs(60),
                storage_path: path.clone(),
            },
            Arc::new(|| {}),
        );
        assert!(store.list_all().is_empty());
    }

    #[test]
    fn load_or_empty_with_unsupported_version_starts_empty() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("tools_cache.json");
        std::fs::write(&path, br#"{"version":99,"entries":[]}"#).unwrap();
        let store = ToolsCacheStore::load_or_empty(
            &ToolsCacheConfig {
                enabled: true,
                cache_life: Duration::from_secs(60),
                storage_path: path,
            },
            Arc::new(|| {}),
        );
        assert!(store.list_all().is_empty());
    }

    /// T2 (часть): после рестарта store читает сохранённое.
    #[test]
    fn persists_across_store_reload() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("tools_cache.json");
        {
            let store = ToolsCacheStore::load_or_empty(
                &ToolsCacheConfig {
                    enabled: true,
                    cache_life: Duration::from_secs(60),
                    storage_path: path.clone(),
                },
                Arc::new(|| {}),
            );
            store.upsert("DRIVE".to_owned(), "DRIVE".to_owned(), vec![td("find")]);
        }
        let store2 = ToolsCacheStore::load_or_empty(
            &ToolsCacheConfig {
                enabled: true,
                cache_life: Duration::from_secs(60),
                storage_path: path,
            },
            Arc::new(|| {}),
        );
        let entries = store2.list_all();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].kind, "DRIVE");
        assert_eq!(entries[0].tools[0].name, "find");
    }

    #[test]
    fn disabled_store_is_noop() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("tools_cache.json");
        let store = ToolsCacheStore::load_or_empty(
            &ToolsCacheConfig {
                enabled: false,
                cache_life: Duration::from_secs(60),
                storage_path: path.clone(),
            },
            Arc::new(|| {}),
        );
        let changed = store.upsert("DRIVE".to_owned(), "DRIVE".to_owned(), vec![td("a")]);
        assert!(!changed);
        assert!(store.list_all().is_empty());
        assert!(!path.exists(), "disabled cache must not create file");
    }

    /// Edge: concurrent upsert под малым TTL (10ms). Eviction может бежать
    /// одновременно с upsert; финальное состояние корректное (либо запись
    /// жива, либо чисто пуста; на диске нет .tmp* остатков).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_upsert_under_short_ttl_keeps_store_consistent() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("tools_cache.json");
        let store = ToolsCacheStore::load_or_empty(
            &ToolsCacheConfig {
                enabled: true,
                cache_life: Duration::from_millis(10),
                storage_path: path.clone(),
            },
            Arc::new(|| {}),
        );

        let mut handles = Vec::new();
        for i in 0..8 {
            let s = Arc::clone(&store);
            handles.push(tokio::spawn(async move {
                // Микро-jitter для большего перекрытия с лента-эвикшеном.
                if i % 2 == 0 {
                    tokio::time::sleep(Duration::from_millis(3)).await;
                }
                s.upsert(
                    "DRIVE".to_owned(),
                    "DRIVE".to_owned(),
                    vec![td(&format!("t{i}"))],
                );
            }));
        }
        for h in handles {
            h.await.unwrap();
        }

        // Дать eviction шанс отработать.
        tokio::time::sleep(Duration::from_millis(30)).await;
        let entries = store.list_all();
        // Финал: либо ровно одна запись (DRIVE,DRIVE), либо пусто после eviction.
        assert!(
            entries.len() <= 1,
            "must be 0 or 1 entry, never half-baked, got {entries:?}"
        );
        if let Some(entry) = entries.first() {
            assert_eq!(entry.kind, "DRIVE");
            assert_eq!(entry.config_id, "DRIVE");
            assert_eq!(entry.tools.len(), 1, "exactly one tool snapshot wins");
        }

        // На диске нет .tmp* остатков ни в каком случае.
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        for n in &names {
            assert!(
                !n.starts_with(".tmp") && !n.contains(".tmp"),
                "tempfile leftover: {n}"
            );
        }
    }

    /// Edge: запись в storage_path внутри несуществующего предка, у которого
    /// один из компонентов — обычный файл, а не каталог. `atomic_write`
    /// провалится на `create_dir_all`, но in-memory должно обновиться.
    #[test]
    fn upsert_with_unwritable_storage_path_keeps_inmemory_in_sync() {
        let dir = tempdir().unwrap();
        // Создаём regular-file `blocker` и пытаемся записать кеш внутрь него.
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"not a directory").unwrap();
        let storage = blocker.join("nested").join("tools_cache.json");

        let counter = Arc::new(AtomicUsize::new(0));
        let hook = {
            let c = Arc::clone(&counter);
            Arc::new(move || {
                c.fetch_add(1, Ordering::SeqCst);
            }) as NotifierHook
        };
        let store = ToolsCacheStore::load_or_empty(
            &ToolsCacheConfig {
                enabled: true,
                cache_life: Duration::from_secs(60),
                storage_path: storage.clone(),
            },
            hook,
        );
        let changed = store.upsert("DRIVE".to_owned(), "DRIVE".to_owned(), vec![td("find")]);
        // Несмотря на ошибку записи, in-memory изменился и notifier дёрнут.
        assert!(changed);
        let entries = store.list_all();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].tools[0].name, "find");
        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "notifier must fire even if persist failed"
        );
        // Файл не создался (или хотя бы не валидный JSON — главное, что в
        // памяти всё консистентно). Проверим, что верхушка-blocker — всё ещё
        // обычный файл и менеджер не упал.
        assert!(blocker.is_file(), "blocker must stay a regular file");
    }

    /// T7: 10 параллельных upsert'ов → ровно одна запись, без мусора.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_upserts_converge_to_one_entry() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("tools_cache.json");
        let store = ToolsCacheStore::load_or_empty(
            &ToolsCacheConfig {
                enabled: true,
                cache_life: Duration::from_secs(60),
                storage_path: path.clone(),
            },
            Arc::new(|| {}),
        );

        let mut handles = Vec::new();
        for i in 0..10 {
            let s = Arc::clone(&store);
            handles.push(tokio::spawn(async move {
                s.upsert(
                    "DRIVE".to_owned(),
                    "DRIVE".to_owned(),
                    vec![td(&format!("t{i}"))],
                );
            }));
        }
        for h in handles {
            h.await.unwrap();
        }

        let entries = store.list_all();
        assert_eq!(entries.len(), 1);
        let tool_names: std::collections::HashSet<String> =
            (0..10).map(|i| format!("t{i}")).collect();
        assert!(tool_names.contains(&entries[0].tools[0].name));

        // На диске нет .tmp* остатков.
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name())
            .collect();
        assert_eq!(names.len(), 1, "tempfile leftovers: {names:?}");
    }
}
