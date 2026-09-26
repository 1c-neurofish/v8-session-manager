//! Идентичность базы сессии для masking-контура (раздел O2).
//!
//! База идентифицируется непрозрачным ключом, который менеджер вычисляет
//! один раз при `session.register`:
//! - `ras:<cluster_guid>:<infobase_guid>` — после успешной RAS-резолюции
//!   (см. `ras.rs`);
//! - `gen:<srvr>/<ref>` — строки `Srvr`/`Ref` как есть, когда RAS
//!   недоступен.
//!
//! Сопоставление везде — только точное равенство ключа: никакой
//! нормализации координат, перебора хостов и фолбэков. RAS- и
//! generated-ключи одних и тех же координат — разные записи сервиса;
//! перехода generated→ras нет (настройки переносятся export/import).
//!
//! Менеджер не доверяет идентификаторам с провода: поле `database_key`
//! в `SessionRegisterParams` помечено `serde(skip)` и заполняется только
//! результатом вычисления менеджера.

use crate::session_manager::masking::ras::ResolvedDatabaseIdentity;
use crate::session_manager::registry::SessionRecord;

/// Идентичность базы сессии: готовый ключ `instance_id` плюс
/// отображаемые координаты (Srvr/Ref) — сервис показывает их в админке,
/// в сопоставлении они не участвуют.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionDatabaseIdentity {
    /// `ras:<c>:<i>` либо `gen:<srvr>/<ref>` — ключ `databases.instance_id`.
    pub instance_id: String,
    /// `Srvr` из строки соединения (`cluster_server` в `session.register`).
    pub cluster_server: String,
    /// Имя ИБ из регистрации (`infobase_name`).
    pub infobase_name: String,
}

/// Ключ по результату RAS-резолюции либо generated-ключ из строк
/// `Srvr`/`Ref` как есть (без нормализации).
pub fn database_key(
    cluster_server: &str,
    infobase_name: &str,
    resolved: Option<ResolvedDatabaseIdentity>,
) -> String {
    match resolved {
        Some(id) => format!("ras:{}:{}", id.cluster_guid, id.infobase_guid),
        None => format!("gen:{}/{}", cluster_server, infobase_name),
    }
}

impl SessionDatabaseIdentity {
    /// Берёт identity из записи сессии. `None` — только когда регистрация
    /// не прислала `cluster_server` (файловая база): такой вызов
    /// отклоняется `DATABASE_IDENTITY_UNVERIFIED`.
    pub fn from_record(record: &SessionRecord) -> Option<Self> {
        let instance_id = record.database_key.clone()?;
        Some(Self {
            instance_id,
            cluster_server: record.cluster_server.clone().unwrap_or_default(),
            infobase_name: record.infobase_name.clone(),
        })
    }
}
