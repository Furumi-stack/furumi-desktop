use std::fs;
use std::path::Path;

use furumi_backend_api::SettingsSnapshot;
use rusqlite::{Connection, OptionalExtension, params};

const LEGACY_DEFAULT_LIBRARY_PATH: &str = "~/Music/Furumi";
const PLATFORM_LIBRARY_PATH_MIGRATION: i64 = 4;

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
    (
        5,
        r"
        ALTER TABLE app_settings ADD COLUMN similarity_enabled INTEGER NOT NULL DEFAULT 0 CHECK (similarity_enabled IN (0, 1));
        ALTER TABLE app_settings ADD COLUMN similarity_model TEXT NOT NULL DEFAULT 'discogs-effnet-bsdynamic-1';
        ALTER TABLE app_settings ADD COLUMN similarity_profile TEXT NOT NULL DEFAULT 'furumi-full-track-v1';
        ALTER TABLE app_settings ADD COLUMN similarity_workers INTEGER NOT NULL DEFAULT 2;
        ALTER TABLE app_settings ADD COLUMN similarity_minimum_score REAL NOT NULL DEFAULT 0.70;
        ALTER TABLE app_settings ADD COLUMN similarity_max_tracks_per_artist INTEGER NOT NULL DEFAULT 5;
        ALTER TABLE app_settings ADD COLUMN similarity_federation_consent INTEGER NOT NULL DEFAULT 0 CHECK (similarity_federation_consent IN (0, 1));
        ALTER TABLE app_settings ADD COLUMN similarity_active_profile TEXT;
        ",
    ),
];

pub struct SettingsStore {
    connection: Connection,
}

impl SettingsStore {
    pub fn open(path: &Path, default_library_path: &Path) -> rusqlite::Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
        }
        let connection = Connection::open(path)?;
        let mut store = Self { connection };
        store.migrate(default_library_path)?;
        Ok(store)
    }

    #[cfg(test)]
    fn in_memory(default_library_path: &Path) -> rusqlite::Result<Self> {
        let connection = Connection::open_in_memory()?;
        let mut store = Self { connection };
        store.migrate(default_library_path)?;
        Ok(store)
    }

    pub fn load(&self) -> rusqlite::Result<SettingsSnapshot> {
        self.connection.query_row(
            "SELECT network_id, library_path, federation_enabled, language,
                    save_federated_on_listen, device_name,
                    similarity_enabled, similarity_model, similarity_profile,
                    similarity_workers, similarity_minimum_score,
                    similarity_max_tracks_per_artist, similarity_federation_consent,
                    similarity_active_profile
             FROM app_settings WHERE singleton_id = 1",
            [],
            |row| {
                let similarity = furumi_backend_api::SimilaritySettingsSnapshot {
                    enabled: row.get::<_, i64>(6)? != 0,
                    model: row.get(7)?,
                    profile: row.get(8)?,
                    workers: usize::try_from(row.get::<_, i64>(9)?).unwrap_or(1),
                    minimum_score: row.get(10)?,
                    max_tracks_per_artist: usize::try_from(row.get::<_, i64>(11)?).unwrap_or(1),
                    federation_consent: row.get::<_, i64>(12)? != 0,
                    active_profile: row.get(13)?,
                }
                .normalized();
                Ok(SettingsSnapshot {
                    network_id: row.get(0)?,
                    library_path: row.get(1)?,
                    federation_enabled: row.get::<_, i64>(2)? != 0,
                    language: row.get(3)?,
                    save_federated_on_listen: row.get::<_, i64>(4)? != 0,
                    device_name: row.get(5)?,
                    similarity,
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
                    device_name = ?6,
                    similarity_enabled = ?7,
                    similarity_model = ?8,
                    similarity_profile = ?9,
                    similarity_workers = ?10,
                    similarity_minimum_score = ?11,
                    similarity_max_tracks_per_artist = ?12,
                    similarity_federation_consent = ?13,
                    similarity_active_profile = ?14
              WHERE singleton_id = 1",
            params![
                settings.network_id,
                settings.library_path,
                i64::from(settings.federation_enabled),
                settings.language,
                i64::from(settings.save_federated_on_listen),
                settings.device_name,
                i64::from(settings.similarity.enabled),
                settings.similarity.model,
                settings.similarity.profile,
                i64::try_from(settings.similarity.workers).unwrap_or(16),
                settings.similarity.minimum_score,
                i64::try_from(settings.similarity.max_tracks_per_artist).unwrap_or(50),
                i64::from(settings.similarity.federation_consent),
                settings.similarity.active_profile,
            ],
        )?;
        Ok(())
    }

    fn migrate(&mut self, default_library_path: &Path) -> rusqlite::Result<()> {
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
            if version == 5 {
                transaction.execute(
                    "UPDATE app_settings SET similarity_workers = ?1 WHERE singleton_id = 1",
                    [i64::try_from(
                        furumi_backend_api::SimilaritySettingsSnapshot::default().workers,
                    )
                    .unwrap_or(1)],
                )?;
            }
            transaction.execute(
                "INSERT INTO schema_migrations (version) VALUES (?1)",
                [version],
            )?;
            transaction.commit()?;
        }
        self.migrate_platform_library_path(default_library_path)?;
        Ok(())
    }

    fn migrate_platform_library_path(
        &mut self,
        default_library_path: &Path,
    ) -> rusqlite::Result<()> {
        let applied = self
            .connection
            .query_row(
                "SELECT 1 FROM schema_migrations WHERE version = ?1",
                [PLATFORM_LIBRARY_PATH_MIGRATION],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if applied {
            return Ok(());
        }
        let default_library_path = default_library_path.to_string_lossy();
        let transaction = self.connection.transaction()?;
        transaction.execute(
            "UPDATE app_settings
                SET library_path = ?1
              WHERE library_path = ?2 OR trim(library_path) = ''",
            params![default_library_path.as_ref(), LEGACY_DEFAULT_LIBRARY_PATH],
        )?;
        transaction.execute(
            "INSERT INTO schema_migrations (version) VALUES (?1)",
            [PLATFORM_LIBRARY_PATH_MIGRATION],
        )?;
        transaction.commit()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_creates_defaults_and_settings_round_trip() {
        let default_library_path = Path::new("/platform/furumi/federation-media");
        let store = SettingsStore::in_memory(default_library_path).unwrap();
        let mut settings = store.load().unwrap();
        assert_eq!(settings.network_id, "furumi");
        assert_eq!(
            settings.library_path,
            default_library_path.to_string_lossy()
        );
        assert!(settings.federation_enabled);
        assert!(settings.save_federated_on_listen);
        assert!(settings.device_name.is_empty());
        assert!(!settings.similarity.enabled);
        assert_eq!(
            settings.similarity.workers,
            furumi_backend_api::SimilaritySettingsSnapshot::default().workers
        );
        assert!((settings.similarity.minimum_score - 0.70).abs() < f32::EPSILON);

        settings.network_id = "friends".into();
        settings.device_name = "Studio Mac".into();
        settings.library_path = "/music/library".into();
        settings.federation_enabled = false;
        settings.similarity.enabled = true;
        settings.similarity.workers = 7;
        settings.similarity.minimum_score = 0.82;
        settings.similarity.max_tracks_per_artist = 9;
        settings.similarity.federation_consent = true;
        settings.similarity.active_profile = Some("sim1:test".into());
        store.save(&settings).unwrap();

        assert_eq!(store.load().unwrap(), settings);
    }

    #[test]
    fn platform_default_migration_preserves_a_custom_library_path() {
        let first_default = Path::new("/first/furumi/federation-media");
        let mut store = SettingsStore::in_memory(first_default).unwrap();
        let mut settings = store.load().unwrap();
        settings.library_path = "/custom/music".into();
        store.save(&settings).unwrap();
        store
            .connection
            .execute(
                "DELETE FROM schema_migrations WHERE version = ?1",
                [PLATFORM_LIBRARY_PATH_MIGRATION],
            )
            .unwrap();

        store
            .migrate(Path::new("/second/furumi/federation-media"))
            .unwrap();

        assert_eq!(store.load().unwrap().library_path, "/custom/music");
    }
}
