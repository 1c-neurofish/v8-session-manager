//! Интеграционные тесты ADR-0035 tools-cache.
//!
//! Сценарии T1/T2/T4 из task design.md §8. Используют публичные точки
//! `McpToolServer` (без необходимости поднимать MCP HTTP):
//! * `list_all_tools_inner` — содержимое `tools/list`;
//! * `tools_cache_reset` (через rmcp-обёртку — недоступно, поэтому здесь
//!   используем `ToolsCacheStore::reset_*` напрямую, эквивалентно tool-вызову,
//!   плюс отдельный smoke на public API через wrapper в `helpers`).
//!
//! Реальный `serve_session_manager` мы не запускаем: его MCP HTTP — это
//! комплексный server-stack, и для верификации T1/T2/T4 достаточно
//! semantics-level доступа. T1 race-сценарий разворачивается через
//! WS-transport (publish ↔ disconnect) и проверку `list_all_tools_inner`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;
use tempfile::TempDir;

use v8_session_manager::config::model::{AppConfig, McpConfig, ToolsCacheConfig};
use v8_session_manager::mcp::server::McpToolServer;
use v8_session_manager::session_manager::management;
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

/// T1 (часть semantics): после `session.register` `tools/list` включает
/// проксированный tool. После disconnect — кеш сохраняет публикацию.
#[tokio::test]
async fn t1_register_publishes_then_disconnect_preserves_via_cache() {
    let dir = TempDir::new().unwrap();
    let registry = Arc::new(SessionRegistry::new());
    let store_hook = {
        let reg = Arc::clone(&registry);
        Arc::new(move || reg.mark_tools_changed_external()) as Arc<dyn Fn() + Send + Sync>
    };
    let cache = make_store(dir.path(), store_hook);
    registry.attach_tools_cache(Arc::clone(&cache));

    let config = app_config(dir.path().to_path_buf());
    let server = McpToolServer::new(config)
        .with_session_registry(Arc::clone(&registry))
        .with_tools_cache(Arc::clone(&cache));

    // До регистрации — только встроенные tools (session_list + tools_cache_reset).
    let names_initial: Vec<String> = server
        .list_all_tools_inner()
        .into_iter()
        .map(|t| t.name.to_string())
        .collect();
    assert!(names_initial.contains(&"session_list".to_owned()));
    assert!(names_initial.contains(&"tools_cache_reset".to_owned()));
    assert!(!names_initial.contains(&"find".to_owned()));

    // session.register для DRIVE с tool `find`.
    registry
        .register(
            register_params("DRIVE-1", "DRIVE", vec![td("find")]),
            Instant::now(),
            None,
        )
        .unwrap();

    let names_active: Vec<String> = server
        .list_all_tools_inner()
        .into_iter()
        .map(|t| t.name.to_string())
        .collect();
    assert!(
        names_active.contains(&"find".to_owned()),
        "find must be published"
    );

    // Disconnect (без grace).
    registry.mark_disconnected("DRIVE-1", Instant::now());

    let names_after: Vec<String> = server
        .list_all_tools_inner()
        .into_iter()
        .map(|t| t.name.to_string())
        .collect();
    assert!(
        names_after.contains(&"find".to_owned()),
        "find must remain via persistent cache after disconnect"
    );
}

/// T2: после рестарта менеджера `tools/list` сразу полный без live-сессии.
#[tokio::test]
async fn t2_tools_cache_survives_manager_restart() {
    let dir = TempDir::new().unwrap();
    // Первый «процесс» — регистрируем, файл записывается.
    {
        let registry = Arc::new(SessionRegistry::new());
        let hook = {
            let reg = Arc::clone(&registry);
            Arc::new(move || reg.mark_tools_changed_external()) as Arc<dyn Fn() + Send + Sync>
        };
        let cache = make_store(dir.path(), hook);
        registry.attach_tools_cache(Arc::clone(&cache));

        registry
            .register(
                register_params("DRIVE-1", "DRIVE", vec![td("find"), td("describe")]),
                Instant::now(),
                None,
            )
            .unwrap();
        assert!(dir.path().join("tools_cache.json").exists());
        // drop everything здесь
    }

    // Второй «процесс» — новый registry и cache с тем же workPath.
    let registry2 = Arc::new(SessionRegistry::new());
    let hook2 = {
        let reg = Arc::clone(&registry2);
        Arc::new(move || reg.mark_tools_changed_external()) as Arc<dyn Fn() + Send + Sync>
    };
    let cache2 = make_store(dir.path(), hook2);
    registry2.attach_tools_cache(Arc::clone(&cache2));

    let config2 = app_config(dir.path().to_path_buf());
    let server2 = McpToolServer::new(config2)
        .with_session_registry(Arc::clone(&registry2))
        .with_tools_cache(Arc::clone(&cache2));

    // session.list должен быть пуст: live-сессий нет.
    let listed = management::list(&registry2);
    assert!(listed.sessions.is_empty());

    // tools/list — содержит проксированные tools из persistent cache.
    let names: Vec<String> = server2
        .list_all_tools_inner()
        .into_iter()
        .map(|t| t.name.to_string())
        .collect();
    assert!(
        names.contains(&"find".to_owned()) && names.contains(&"describe".to_owned()),
        "cached tools must be visible immediately after restart, got {names:?}"
    );
}

/// T4: tools_cache_reset с config_id чистит только указанную запись.
#[tokio::test]
async fn t4_reset_by_config_id_removes_only_target() {
    let dir = TempDir::new().unwrap();
    let registry = Arc::new(SessionRegistry::new());
    let hook = {
        let reg = Arc::clone(&registry);
        Arc::new(move || reg.mark_tools_changed_external()) as Arc<dyn Fn() + Send + Sync>
    };
    let cache = make_store(dir.path(), hook);
    registry.attach_tools_cache(Arc::clone(&cache));

    // Зарегистрируем две сессии разных kind.
    registry
        .register(
            register_params("DRIVE-1", "DRIVE", vec![td("drive_find")]),
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
    // Disconnect обе — пусть кеш остаётся единственным источником.
    registry.mark_disconnected("DRIVE-1", Instant::now());
    registry.mark_disconnected("PAM-1", Instant::now());

    // Снапшот эпохи до reset.
    let epoch_before = registry.tools_epoch();

    // Симулируем MCP вызов tools_cache_reset({config_id:"DRIVE"}):
    let removed = cache.reset_by_config_id("DRIVE");
    assert_eq!(removed, 1, "exactly DRIVE entry removed");

    // Эпоха должна увеличиться (notifier hook дёрнут).
    assert!(
        registry.tools_epoch() > epoch_before,
        "tools_epoch must bump after cache reset"
    );

    // Файл атомарно обновлён, на диске не осталось .tmp* мусора.
    let entries: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name())
        .collect();
    assert_eq!(entries.len(), 1, "no leftover tempfiles: {entries:?}");

    // В кеше остался только PAM.
    let listed = cache.list_all();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].kind, "PAM");
}

/// T5: kind берётся как fallback для config_id, и session_list это отражает.
#[tokio::test]
async fn t5_session_list_carries_config_id_fallback_to_kind() {
    let dir = TempDir::new().unwrap();
    let registry = Arc::new(SessionRegistry::new());
    let hook = {
        let reg = Arc::clone(&registry);
        Arc::new(move || reg.mark_tools_changed_external()) as Arc<dyn Fn() + Send + Sync>
    };
    let cache = make_store(dir.path(), hook);
    registry.attach_tools_cache(Arc::clone(&cache));

    registry
        .register(
            register_params("DRIVE-1", "DRIVE", vec![td("find")]),
            Instant::now(),
            None,
        )
        .unwrap();

    let listed = management::list(&registry);
    assert_eq!(listed.sessions.len(), 1);
    assert_eq!(listed.sessions[0].kind, "DRIVE");
    assert_eq!(listed.sessions[0].config_id, "DRIVE");

    // А если задать config_id явно — он попадает в session_list.
    let mut p = register_params("PAM-1", "PAM", vec![td("pam_run")]);
    p.config_id = Some("PAM_LOCAL".to_owned());
    registry.register(p, Instant::now(), None).unwrap();

    let listed2 = management::list(&registry);
    let pam = listed2.sessions.iter().find(|s| s.kind == "PAM").unwrap();
    assert_eq!(pam.config_id, "PAM_LOCAL");
}
