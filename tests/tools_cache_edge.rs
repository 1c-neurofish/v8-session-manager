//! Edge-cases для ADR-0035 tools-cache (Phase 4, дополнения tester'а).
//!
//! Дополняют integration набор из `tools_cache_race.rs`:
//! * reset без аргумента пишет валидный пустой JSON и не оставляет мусора;
//! * `update_tools` от уже зарегистрированной сессии bump'ит кеш
//!   (расширение §3.3 ADR-0035 — кеш должен догонять live-сессию).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;
use tempfile::TempDir;

use v8_session_manager::config::model::{AppConfig, McpConfig, ToolsCacheConfig};
use v8_session_manager::mcp::server::McpToolServer;
use v8_session_manager::session_manager::protocol::{SessionRegisterParams, ToolDescriptor};
use v8_session_manager::session_manager::registry::SessionRegistry;
use v8_session_manager::session_manager::tools_cache::{
    ToolsCacheConfig as CacheConfig, ToolsCacheStore,
};

fn td(name: &str) -> ToolDescriptor {
    ToolDescriptor {
        name: name.to_owned(),
        description: Some(format!("desc for {name}")),
        input_schema: json!({"type": "object"}),
    }
}

fn register_params(uid: &str, kind: &str, tools: Vec<ToolDescriptor>) -> SessionRegisterParams {
    SessionRegisterParams {
        client_uid: uid.to_owned(),
        kind: kind.to_owned(),
        version: "1.0".to_owned(),
        infobase_name: "test_db".to_owned(),
        ib_session_number: 1,
        tools,
        config_id: None,
        host_id: None,
        pid: None,
        resources: None,
        prompts: None,
        extras: None,
    }
}

fn app_config(work_path: PathBuf) -> Arc<AppConfig> {
    Arc::new(AppConfig {
        work_path,
        mcp: McpConfig::default(),
        tools_cache: ToolsCacheConfig::default(),
    })
}

fn make_store(
    work_path: &std::path::Path,
    hook: Arc<dyn Fn() + Send + Sync>,
) -> Arc<ToolsCacheStore> {
    ToolsCacheStore::load_or_empty(
        &CacheConfig {
            enabled: true,
            cache_life: Duration::from_secs(60 * 60),
            storage_path: work_path.join("tools_cache.json"),
        },
        hook,
    )
}

/// Edge: `tools_cache_reset` без аргумента (full reset) — несколько записей,
/// после reset файл существует, является валидным пустым JSON (`entries: []`),
/// мусора `.tmp*` в каталоге нет.
#[tokio::test]
async fn reset_all_writes_valid_empty_json_without_leftovers() {
    let dir = TempDir::new().unwrap();
    let registry = Arc::new(SessionRegistry::new());
    let hook = {
        let reg = Arc::clone(&registry);
        Arc::new(move || reg.mark_tools_changed_external()) as Arc<dyn Fn() + Send + Sync>
    };
    let cache = make_store(dir.path(), hook);
    registry.attach_tools_cache(Arc::clone(&cache));

    // Несколько разных kind / config_id.
    registry
        .register(
            register_params("DRIVE-1", "DRIVE", vec![td("find"), td("describe")]),
            Instant::now(),
            None,
        )
        .unwrap();
    registry
        .register(
            register_params("PAM-1", "PAM", vec![td("pam_run")]),
            Instant::now(),
            None,
        )
        .unwrap();
    let mut p = register_params("VA-1", "VA", vec![td("va_step")]);
    p.config_id = Some("VA_LOCAL".to_owned());
    registry.register(p, Instant::now(), None).unwrap();

    assert_eq!(cache.list_all().len(), 3);
    let path = dir.path().join("tools_cache.json");
    assert!(path.exists());

    let removed = cache.reset_all();
    assert_eq!(removed, 3, "all three entries removed");
    assert!(cache.list_all().is_empty());

    // Файл — валидный JSON, entries пустой.
    let txt = std::fs::read_to_string(&path).expect("cache file must still exist after reset");
    let parsed: serde_json::Value = serde_json::from_str(&txt).expect("valid JSON");
    assert_eq!(parsed["version"], json!(1));
    assert_eq!(
        parsed["entries"].as_array().expect("entries array").len(),
        0
    );

    // Никакого .tmp* мусора в tempdir.
    let leftovers: Vec<String> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n != "tools_cache.json")
        .collect();
    assert!(leftovers.is_empty(), "no leftover tempfiles: {leftovers:?}");
}

/// Edge / §3.3 ADR-0035 extension: `update_tools` от уже зарегистрированной
/// сессии bump'ит кеш — это нужно, чтобы кеш не отставал от живой сессии
/// в пределах одной WS-сессии (после disconnect AI-агент видит актуальный
/// набор tools, не «снимок на момент register»).
#[tokio::test]
async fn update_tools_bumps_cache_entry_for_active_session() {
    let dir = TempDir::new().unwrap();
    let registry = Arc::new(SessionRegistry::new());
    let hook = {
        let reg = Arc::clone(&registry);
        Arc::new(move || reg.mark_tools_changed_external()) as Arc<dyn Fn() + Send + Sync>
    };
    let cache = make_store(dir.path(), hook);
    registry.attach_tools_cache(Arc::clone(&cache));

    let config = app_config(dir.path().to_path_buf());
    let server = McpToolServer::new(config)
        .with_session_registry(Arc::clone(&registry))
        .with_tools_cache(Arc::clone(&cache));

    // Register со старым набором.
    registry
        .register(
            register_params("DRIVE-1", "DRIVE", vec![td("find")]),
            Instant::now(),
            None,
        )
        .unwrap();
    let entries_initial = cache.list_all();
    assert_eq!(entries_initial.len(), 1);
    assert_eq!(entries_initial[0].tools.len(), 1);
    let last_seen_before = entries_initial[0].last_seen_at;

    // Чуть подождать, чтобы `last_seen_at` отличался монотонно.
    tokio::time::sleep(Duration::from_millis(5)).await;

    // update_tools — пришёл notification session.tools_changed.
    let updated = registry.update_tools("DRIVE-1", vec![td("find"), td("describe"), td("query")]);
    assert!(updated, "session must be found");

    let entries_after = cache.list_all();
    assert_eq!(entries_after.len(), 1, "still one cache entry");
    assert_eq!(entries_after[0].tools.len(), 3, "cache reflects new set");
    let names: Vec<&str> = entries_after[0]
        .tools
        .iter()
        .map(|t| t.name.as_str())
        .collect();
    assert!(names.contains(&"find"));
    assert!(names.contains(&"describe"));
    assert!(names.contains(&"query"));
    assert!(
        entries_after[0].last_seen_at >= last_seen_before,
        "last_seen_at must not regress"
    );

    // На диске cache отражает обновлённый набор — это критичная гарантия для
    // рестарта менеджера в пределах одной активной сессии.
    let txt = std::fs::read_to_string(dir.path().join("tools_cache.json")).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&txt).unwrap();
    let tools_arr = parsed["entries"][0]["tools"].as_array().unwrap();
    assert_eq!(tools_arr.len(), 3);

    // tools/list через MCP-сервер также показывает обновлённый набор.
    let names: Vec<String> = server
        .list_all_tools_inner()
        .into_iter()
        .map(|t| t.name.to_string())
        .collect();
    assert!(names.contains(&"find".to_owned()));
    assert!(names.contains(&"describe".to_owned()));
    assert!(names.contains(&"query".to_owned()));

    // Disconnect — кеш всё ещё держит расширенный набор.
    registry.mark_disconnected("DRIVE-1", Instant::now());
    let entries_after_disconnect = cache.list_all();
    assert_eq!(entries_after_disconnect[0].tools.len(), 3);
}
