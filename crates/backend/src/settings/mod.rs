use std::fs;
use std::path::Path;

use furumi_backend_api::SettingsSnapshot;
use rusqlite::{Connection, OptionalExtension, params};

const MIGRATIONS: &[(i64, &str)] = &[
    (
        1,
        r"
    CREATE TABLE app_settings (
        singleton_id       INTEGER PRIMARY KEY CHECK (singleton_id = 1),
        network_id         TEXT NOT NULL,
        library_path       TEXT NOT NULL,
        federation_enabled INTEGER NOT NULL CHECK (federation_enabled IN (0, 1)),
        language           TEXT NOT NULL
    );
    INSERT INTO app_settings (
        singleton_id, network_id, library_path, federation_enabled, language
    ) VALUES (1, 'furumi', '~/Music/Furumi', 1, 'English');
    ",
    ),
    (
        2,
        "ALTER TABLE app_settings ADD COLUMN save_federated_on_listen INTEGER NOT NULL DEFAULT 1 CHECK (save_federated_on_listen IN (0, 1));",
    ),
    (
        3,
        "ALTER TABLE app_settings ADD COLUMN device_name TEXT NOT NULL DEFAULT '';",
    ),
];

pub struct SettingsStore {
    connection: Connection,
}

impl SettingsStore {
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
        }
        let connection = Connection::open(path)?;
        let mut store = Self { connection };
        store.migrate()?;
        Ok(store)
    }

    #[cfg(test)]
    fn in_memory() -> rusqlite::Result<Self> {
        let connection = Connection::open_in_memory()?;
        let mut store = Self { connection };
        store.migrate()?;
        Ok(store)
    }

    pub fn load(&self) -> rusqlite::Result<SettingsSnapshot> {
        self.connection.query_row(
            "SELECT network_id, library_path, federation_enabled, language,
                    save_federated_on_listen, device_name
             FROM app_settings WHERE singleton_id = 1",
            [],
            |row| {
                Ok(SettingsSnapshot {
                    network_id: row.get(0)?,
                    library_path: row.get(1)?,
                    federation_enabled: row.get::<_, i64>(2)? != 0,
                    language: row.get(3)?,
                    save_federated_on_listen: row.get::<_, i64>(4)? != 0,
                    device_name: row.get(5)?,
                })
            },
        )
    }

    pub fn save(&self, settings: &SettingsSnapshot) -> rusqlite::Result<()> {
        self.connection.execute(
            "UPDATE app_settings
                SET network_id = ?1,
                    library_path = ?2,
                    federation_enabled = ?3,
                    language = ?4,
                    save_federated_on_listen = ?5,
                    device_name = ?6
              WHERE singleton_id = 1",
            params![
                settings.network_id,
                settings.library_path,
                i64::from(settings.federation_enabled),
                settings.language,
                i64::from(settings.save_federated_on_listen),
                settings.device_name,
            ],
        )?;
        Ok(())
    }

    fn migrate(&mut self) -> rusqlite::Result<()> {
        self.connection.execute_batch(
            "PRAGMA foreign_keys = ON;
             CREATE TABLE IF NOT EXISTS schema_migrations (
                 version    INTEGER PRIMARY KEY,
                 applied_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
             );",
        )?;

        for &(version, sql) in MIGRATIONS {
            let applied = self
                .connection
                .query_row(
                    "SELECT 1 FROM schema_migrations WHERE version = ?1",
                    [version],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            if applied {
                continue;
            }
            let transaction = self.connection.transaction()?;
            transaction.execute_batch(sql)?;
            transaction.execute(
                "INSERT INTO schema_migrations (version) VALUES (?1)",
                [version],
            )?;
            transaction.commit()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_creates_defaults_and_settings_round_trip() {
        let store = SettingsStore::in_memory().unwrap();
        let mut settings = store.load().unwrap();
        assert_eq!(settings.network_id, "furumi");
        assert!(settings.federation_enabled);
        assert!(settings.save_federated_on_listen);
        assert!(settings.device_name.is_empty());

        settings.network_id = "friends".into();
        settings.device_name = "Studio Mac".into();
        settings.library_path = "/music/library".into();
        settings.federation_enabled = false;
        store.save(&settings).unwrap();

        assert_eq!(store.load().unwrap(), settings);
    }
}
