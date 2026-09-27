//! Резолюция `(Srvr, Ref)` регистрации 1С → `(GUID кластера, GUID ИБ)`
//! через RAS/`rac`. Это единственный источник правды об идентичности базы:
//! адаптер присылает только `cluster_server` и имя ИБ, GUID-ы менеджер
//! получает сам из кластера — подмена на стороне адаптера невозможна.
//!
//! Конфигурация — только переменные окружения:
//! - `V8SM_RAC_PATH` — путь к утилите `rac` (по умолчанию
//!   `/opt/1cv8/current/rac`);
//! - `V8SM_RAS_ADDRESS` — опциональный фиксированный адрес RAS
//!   `host[:port]`; без него `rac` адресуется строкой `cluster_server`
//!   (Srvr) как есть;
//! - `V8SM_RAS_CLUSTER_USER`, `V8SM_RAS_CLUSTER_PASSWORD` — опциональные
//!   креды администратора кластера. Пустые/отсутствующие значения — штатно
//!   (RAS без аутентификации): соответствующие флаги `--cluster-user` /
//!   `--cluster-pwd` rac не передаются. Пароль нигде не логируется.
//!
//! RAS недоступен (`rac` нет, сеть, отказ) — резолюция возвращает `None`,
//! регистрация не блокируется: сессия получает generated-ключ
//! `gen:<srvr>/<ref>`.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::process::Command;
use uuid::Uuid;

const ENV_RAC_PATH: &str = "V8SM_RAC_PATH";
const ENV_RAS_ADDRESS: &str = "V8SM_RAS_ADDRESS";
const ENV_CLUSTER_USER: &str = "V8SM_RAS_CLUSTER_USER";
const ENV_CLUSTER_PASSWORD: &str = "V8SM_RAS_CLUSTER_PASSWORD";
const DEFAULT_RAC_PATH: &str = "/opt/1cv8/current/rac";
/// Дедлайн всей резолюции регистрации (несколько вызовов rac).
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(5);
/// TTL кеша резолюций `(srvr, имя ИБ) → GUID-ы`.
const RESOLVE_CACHE_TTL: Duration = Duration::from_secs(300);
/// TTL промахов резолюции (RAS недоступен/ИБ не найдена): короткий —
/// после ремонта RAS база должна получить `ras:`-ключ быстро, а не
/// ждать полный TTL.
const RESOLVE_MISS_TTL: Duration = Duration::from_secs(30);

/// Детерминированная идентичность базы: пара GUID-ов кластера и ИБ.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedDatabaseIdentity {
    pub cluster_guid: Uuid,
    pub infobase_guid: Uuid,
}

type RacExecFuture = Pin<Box<dyn Future<Output = Result<String, String>> + Send>>;

/// Флаги аутентификации администратора кластера: только при непустых
/// значениях — RAS без кредов (штатная схема) флагов не получает.
fn credential_args(user: Option<&str>, password: Option<&str>) -> Vec<String> {
    let mut args = Vec::new();
    if let Some(user) = user.filter(|v| !v.is_empty()) {
        args.push(format!("--cluster-user={user}"));
    }
    if let Some(password) = password.filter(|v| !v.is_empty()) {
        args.push(format!("--cluster-pwd={password}"));
    }
    args
}
type RacExec = Arc<dyn Fn(Vec<String>) -> RacExecFuture + Send + Sync>;
/// `(srvr, имя ИБ) → (момент, результат)` — кеш позитивных и кратковременных
/// негативных резолюций (регистрации повторяются при переподключениях).
type ResolutionCache =
    Mutex<HashMap<(String, String), (Instant, Option<ResolvedDatabaseIdentity>)>>;

/// Резолвер через RAS. Кеширует и позитивные, и негативные резолюции
/// (регистрации повторяются при каждом переподключении прокси).
pub struct RasResolver {
    timeout: Duration,
    ttl: Duration,
    exec: RacExec,
    /// Фиксированный адрес RAS `host[:port]` из `V8SM_RAS_ADDRESS`;
    /// `None` — rac адресуется строкой `cluster_server` (Srvr) как есть.
    ras_address: Option<String>,
    cache: ResolutionCache,
}

impl RasResolver {
    /// Резолвер из переменных окружения (`V8SM_RAC_PATH`,
    /// `V8SM_RAS_ADDRESS`, `V8SM_RAS_CLUSTER_USER`,
    /// `V8SM_RAS_CLUSTER_PASSWORD`). Пустые строки env трактуются как
    /// отсутствующие — креды без значения не передаются в rac вообще.
    pub fn from_env() -> Self {
        let rac_path = std::env::var(ENV_RAC_PATH)
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_RAC_PATH.to_owned());
        let ras_address = std::env::var(ENV_RAS_ADDRESS)
            .ok()
            .filter(|v| !v.trim().is_empty());
        let cluster_user = std::env::var(ENV_CLUSTER_USER)
            .ok()
            .filter(|v| !v.is_empty());
        let cluster_pwd = std::env::var(ENV_CLUSTER_PASSWORD)
            .ok()
            .filter(|v| !v.is_empty());
        Self::with_exec(
            ras_address,
            Arc::new(move |mut args: Vec<String>| {
                let rac_path = rac_path.clone();
                let cluster_user = cluster_user.clone();
                let cluster_pwd = cluster_pwd.clone();
                Box::pin(async move {
                    // Аутентификация администратора кластера — общие опции
                    // rac. Пустые креды (RAS без пароля — штатная схема) —
                    // флаги не передаются вовсе.
                    args.extend(credential_args(
                        cluster_user.as_deref(),
                        cluster_pwd.as_deref(),
                    ));
                    let output = Command::new(&rac_path)
                        .args(&args)
                        .output()
                        .await
                        .map_err(|e| format!("spawn {rac_path}: {e}"))?;
                    if !output.status.success() {
                        // Пароль не логируется: args содержат только флаги,
                        // stderr rac сам креды не возвращает.
                        return Err(format!(
                            "rac {:?} exited {:?}: {}",
                            args.iter()
                                .map(|a| {
                                    if a.starts_with("--cluster-pwd=") {
                                        "--cluster-pwd=<redacted>"
                                    } else {
                                        a.as_str()
                                    }
                                })
                                .collect::<Vec<_>>(),
                            output.status.code(),
                            String::from_utf8_lossy(&output.stderr).trim()
                        ));
                    }
                    String::from_utf8(output.stdout)
                        .map_err(|e| format!("rac output is not UTF-8: {e}"))
                })
            }),
        )
    }

    /// Тестовый конструктор: `exec` получает args rac (с адресом RAS
    /// последним элементом — позиционный `<host>[:<port>]`).
    fn with_exec(ras_address: Option<String>, exec: RacExec) -> Self {
        Self {
            timeout: RESOLVE_TIMEOUT,
            ttl: RESOLVE_CACHE_TTL,
            ras_address,
            exec,
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// `cluster_server` — значение `Srvr` из строки соединения ИБ.
    /// `ib_name` — имя ИБ (`Ref` из строки соединения).
    ///
    /// `None` — RAS недоступен, ИБ не найдена или имя встречается в
    /// нескольких кластерах (ambiguous — административно разрулить).
    pub async fn resolve(
        &self,
        cluster_server: &str,
        ib_name: &str,
    ) -> Option<ResolvedDatabaseIdentity> {
        let key = (cluster_server.to_owned(), ib_name.to_lowercase());
        if let Some((at, cached)) = self.cache.lock().unwrap().get(&key) {
            // Промахи держатся короче хитов (RESOLVE_MISS_TTL): без кеша
            // промахов каждая перерегистрация уходила бы в rac на полный
            // таймаут, а длинный кеш откладывал бы подхват починенного RAS.
            let ttl = if cached.is_some() {
                self.ttl
            } else {
                RESOLVE_MISS_TTL
            };
            if at.elapsed() < ttl {
                return *cached;
            }
        }
        let resolved =
            tokio::time::timeout(self.timeout, self.resolve_uncached(cluster_server, ib_name))
                .await
                .ok()
                .flatten();
        self.cache
            .lock()
            .unwrap()
            .insert(key, (Instant::now(), resolved));
        resolved
    }

    /// Один адрес резолюции: `V8SM_RAS_ADDRESS` либо `Srvr` как есть —
    /// список хостов не разбирается, перебора нет.
    async fn resolve_uncached(
        &self,
        cluster_server: &str,
        ib_name: &str,
    ) -> Option<ResolvedDatabaseIdentity> {
        let wanted = ib_name.trim().to_lowercase();
        let ras = self
            .ras_address
            .clone()
            .unwrap_or_else(|| cluster_server.trim().to_owned());
        let clusters = match self.rac(&["cluster", "list", &ras]).await {
            Ok(out) => parse_blocks(&out)
                .into_iter()
                .filter_map(|b| b.get("cluster").and_then(|v| Uuid::parse_str(v).ok()))
                .collect::<Vec<_>>(),
            Err(err) => {
                tracing::debug!(ras, error = %err, "ras: cluster list failed");
                return None;
            }
        };
        let mut matches = Vec::new();
        for cluster in clusters {
            let cid = cluster.to_string();
            let out = match self
                .rac(&["infobase", "summary", "list", "--cluster", &cid, &ras])
                .await
            {
                Ok(out) => out,
                Err(err) => {
                    tracing::debug!(ras, cluster = %cid, error = %err, "ras: infobase list failed");
                    continue;
                }
            };
            for block in parse_blocks(&out) {
                let Some(name) = block.get("name") else {
                    continue;
                };
                if name.trim_matches('"').to_lowercase() != wanted {
                    continue;
                }
                if let Some(ib) = block.get("infobase").and_then(|v| Uuid::parse_str(v).ok()) {
                    matches.push(ResolvedDatabaseIdentity {
                        cluster_guid: cluster,
                        infobase_guid: ib,
                    });
                }
            }
        }
        match matches.len() {
            1 => Some(matches[0]),
            0 => None,
            // Одно имя ИБ в нескольких кластерах хоста — неоднозначность,
            // fail-closed: регистрация останется без ras-ключа.
            _ => {
                tracing::warn!(
                    cluster_server,
                    ib_name,
                    matches = matches.len(),
                    "ras: infobase name is ambiguous across clusters"
                );
                None
            }
        }
    }

    async fn rac(&self, args: &[&str]) -> Result<String, String> {
        (self.exec)(args.iter().map(|a| a.to_string()).collect()).await
    }
}

/// Вывод rac — блоки `ключ : значение`, разделённые пустыми строками.
fn parse_blocks(stdout: &str) -> Vec<HashMap<String, String>> {
    let mut blocks = Vec::new();
    let mut cur = HashMap::new();
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            if !cur.is_empty() {
                blocks.push(std::mem::take(&mut cur));
            }
            continue;
        }
        if let Some((k, v)) = line.split_once(':') {
            cur.insert(k.trim().to_lowercase(), v.trim().to_owned());
        }
    }
    if !cur.is_empty() {
        blocks.push(cur);
    }
    blocks
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolver_with(scripted: &'static str) -> RasResolver {
        // Простейший exec: по режиму в args возвращает заранее заданный вывод.
        let mut r = RasResolver::with_exec(
            None,
            Arc::new(move |args: Vec<String>| {
                let scripted = scripted.to_string();
                Box::pin(async move {
                    if args.windows(2).any(|w| w[0] == "cluster" && w[1] == "list") {
                        return Ok(scripted.split("###").next().unwrap_or_default().to_string());
                    }
                    if args.iter().any(|a| a == "infobase") {
                        return Ok(scripted.split("###").nth(1).unwrap_or_default().to_string());
                    }
                    Err("unexpected args".to_string())
                })
            }),
        );
        r.ttl = Duration::ZERO;
        r
    }

    #[tokio::test]
    async fn resolves_guid_pair_by_srvr_and_ref() {
        let r = resolver_with(
            "cluster : 0de031da-e8d9-43de-bb39-7c8bd4d9855c\n\
             host : onec-infra\n\
             \n\
             ###\n\
             infobase : 45d06b68-c1d7-4bd6-b739-08d25ba4a4c4\n\
             name : dssl_ut_ai\n\
             \n\
             infobase : 320f6387-89b5-43fc-b344-67b11f957472\n\
             name : gbig_pam_ai\n",
        );
        let id = r.resolve("onec-infra:1545", "gbig_pam_ai").await.unwrap();
        assert_eq!(
            id.cluster_guid,
            Uuid::parse_str("0de031da-e8d9-43de-bb39-7c8bd4d9855c").unwrap()
        );
        assert_eq!(
            id.infobase_guid,
            Uuid::parse_str("320f6387-89b5-43fc-b344-67b11f957472").unwrap()
        );
        // Регистронезависимое сравнение имени ИБ.
        assert_eq!(r.resolve("onec-infra:1545", "GBIG_PAM_AI").await, Some(id));
    }

    #[tokio::test]
    async fn ras_address_override_used_verbatim() {
        // V8SM_RAS_ADDRESS фиксирует адрес RAS — даже когда Srvr указывает
        // на недоступный хост, резолюция идёт через env-адрес.
        let r = RasResolver::with_exec(
            Some("ras-gw:1545".to_owned()),
            Arc::new(move |args: Vec<String>| {
                Box::pin(async move {
                    let ras = args.last().cloned().unwrap_or_default();
                    if ras != "ras-gw:1545" {
                        return Err(format!("unexpected ras target {ras}"));
                    }
                    if args.windows(2).any(|w| w[0] == "cluster" && w[1] == "list") {
                        return Ok("cluster : 0de031da-e8d9-43de-bb39-7c8bd4d9855c\n".to_string());
                    }
                    Ok(
                        "infobase : 320f6387-89b5-43fc-b344-67b11f957472\nname : gbig_pam_ai\n"
                            .to_string(),
                    )
                })
            }),
        );
        assert!(r.resolve("unreachable:1541", "gbig_pam_ai").await.is_some());
    }

    #[test]
    fn empty_credentials_omit_rac_flags() {
        assert!(credential_args(None, None).is_empty());
        assert!(credential_args(Some(""), Some("")).is_empty());
        assert_eq!(
            credential_args(Some("admin"), Some("")),
            vec!["--cluster-user=admin".to_string()]
        );
        assert_eq!(
            credential_args(Some("admin"), Some("secret")),
            vec![
                "--cluster-user=admin".to_string(),
                "--cluster-pwd=secret".to_string()
            ]
        );
    }

    #[tokio::test]
    async fn unknown_infobase_or_unreachable_ras_give_none() {
        let r = resolver_with(
            "cluster : 0de031da-e8d9-43de-bb39-7c8bd4d9855c\n###\n\
             infobase : 45d06b68-c1d7-4bd6-b739-08d25ba4a4c4\nname : dssl_ut_ai\n",
        );
        assert!(r.resolve("onec-infra", "gbig_pam_ai").await.is_none());

        let down = RasResolver::with_exec(
            None,
            Arc::new(|_| Box::pin(async { Err("Connection refused".to_string()) })),
        );
        assert!(down.resolve("onec-infra", "gbig_pam_ai").await.is_none());
    }
}
