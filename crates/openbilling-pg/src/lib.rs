//! The PostgreSQL store of openbilling-rs.
//!
//! [ADR 0001](https://github.com/vaam-apps/openbilling-rs/blob/main/docs/adr/0001-postgres-is-the-queue.md) makes
//! PostgreSQL the queue, the once-only record and the ledger, and
//! [ADR 0002](https://github.com/vaam-apps/openbilling-rs/blob/main/docs/adr/0002-an-owner-holds-several-accounts.md)
//! gives an owner several accounts. Every rule of the ledger is SQL in `migrations/` (agent guide, rule 12);
//! [`PgLedger`] implements the [`LedgerStore`](openbilling_core::LedgerStore) port by calling it, and
//! [`sqlstate`] maps what it raises. It also implements the [`PriceBook`](openbilling_core::PriceBook) port
//! (ADR 0004): it parses a YAML price list and the SQL checks and loads it.
//!
//! PostgreSQL 19 is the minimum (ADR 0001, amended 2026-11-27). The store uses its features and refuses to start
//! against an older server.

use std::borrow::Cow;
use std::time::Duration;

use openbilling_core::{Classify, ErrorClass};
use sqlx::migrate::{Migration, MigrationType, Migrator};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{PgPool, SqlSafeStr};

mod ledger;
mod price_list;
pub mod sqlstate;

pub use ledger::{LedgerSettings, NOTIFY_CHANNEL, PgLedger, QueueStats, Wakeup, Woken};

/// The migrations, in order: hand-written SQL, embedded in the binary.
const MIGRATIONS: [(i64, &str, &str); 10] = [
    (
        20_261_127_000_001,
        "configuration and accounts",
        include_str!("../migrations/20261127000001_configuration_and_accounts.sql"),
    ),
    (
        20_261_127_000_002,
        "ledger",
        include_str!("../migrations/20261127000002_ledger.sql"),
    ),
    (
        20_261_127_000_003,
        "usage queue",
        include_str!("../migrations/20261127000003_usage_queue.sql"),
    ),
    (
        20_261_127_000_004,
        "ledger functions",
        include_str!("../migrations/20261127000004_ledger_functions.sql"),
    ),
    (
        20_261_127_000_005,
        "rated events",
        include_str!("../migrations/20261127000005_rated_events.sql"),
    ),
    (
        20_261_127_000_006,
        "price book",
        include_str!("../migrations/20261127000006_price_book.sql"),
    ),
    (
        20_261_127_000_007,
        "always book debt",
        include_str!("../migrations/20261127000007_always_book_debt.sql"),
    ),
    (
        20_261_127_000_008,
        "allowed policy",
        include_str!("../migrations/20261127000008_allowed_policy.sql"),
    ),
    (
        20_261_128_000_001,
        "partition retention",
        include_str!("../migrations/20261128000001_partition_retention.sql"),
    ),
    (
        20_261_128_000_002,
        "top ups",
        include_str!("../migrations/20261128000002_top_ups.sql"),
    ),
];

/// The migrator of the schema. Each migration runs in its own transaction, and sqlx sends its file whole, as one
/// simple-protocol query, so the `BEGIN ATOMIC` bodies inside it are never split on `;` (*verified 2026-11-27*,
/// sqlx-postgres 0.9.0 source: `migrate.rs` executes the file's text with no arguments, which
/// `connection/executor.rs` sends as one `Query` message).
#[must_use]
pub fn migrator() -> Migrator {
    Migrator::with_migrations(
        MIGRATIONS
            .iter()
            .map(|(version, description, sql)| {
                Migration::new(
                    *version,
                    Cow::Borrowed(*description),
                    MigrationType::Simple,
                    sql.into_sql_str(),
                    false,
                )
            })
            .collect(),
    )
}

/// How long [`PgStore::connect`] and every query wait for a free connection.
const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(5);

/// How many connections [`PgStore::connect`] opens at most.
pub const DEFAULT_MAX_CONNECTIONS: u32 = 10;

/// The oldest server the store accepts, as `server_version_num`: PostgreSQL 19.0.
pub const MIN_SERVER_VERSION_NUM: i32 = 190_000;

/// A pool of connections to one PostgreSQL database.
#[derive(Clone, Debug)]
pub struct PgStore {
    pool: PgPool,
}

impl PgStore {
    /// Connects to the database at `url` (`postgres://…`), opens one connection to prove it answers, and checks that
    /// the server is PostgreSQL 19 or later.
    ///
    /// # Errors
    ///
    /// - [`Error::Connect`] when the URL is malformed or the database cannot be reached. The error's message never
    ///   holds the URL, which can carry a password.
    /// - [`Error::UnsupportedServer`] when the server is older than PostgreSQL 19. The pool is closed.
    pub async fn connect(url: &str) -> Result<Self, Error> {
        let options: PgConnectOptions = url.parse().map_err(Error::Connect)?;
        Self::connect_with(options, DEFAULT_MAX_CONNECTIONS).await
    }

    /// [`PgStore::connect`] with parsed options and a pool of at most `max_connections`.
    ///
    /// # Errors
    ///
    /// As [`PgStore::connect`].
    pub async fn connect_with(
        options: PgConnectOptions,
        max_connections: u32,
    ) -> Result<Self, Error> {
        let pool = PgPoolOptions::new()
            .acquire_timeout(ACQUIRE_TIMEOUT)
            .max_connections(max_connections)
            .connect_with(options)
            .await
            .map_err(Error::Connect)?;
        let store = Self { pool };
        let version = store.server_version_num().await?;
        if version < MIN_SERVER_VERSION_NUM {
            store.close().await;
            return Err(Error::UnsupportedServer { version });
        }
        Ok(store)
    }

    /// The server's `server_version_num`, for example `190000` for PostgreSQL 19.0.
    ///
    /// # Errors
    ///
    /// [`Error::Query`] when the database does not answer.
    pub async fn server_version_num(&self) -> Result<i32, Error> {
        sqlx::query_scalar("SELECT current_setting('server_version_num')::integer")
            .fetch_one(&self.pool)
            .await
            .map_err(Error::Query)
    }

    /// Runs `SELECT 1` on a connection of the pool.
    ///
    /// # Errors
    ///
    /// [`Error::Query`] when the database does not answer.
    pub async fn ping(&self) -> Result<(), Error> {
        let one: i32 = sqlx::query_scalar("SELECT 1")
            .fetch_one(&self.pool)
            .await
            .map_err(Error::Query)?;
        if one == 1 {
            Ok(())
        } else {
            Err(Error::UnexpectedAnswer)
        }
    }

    /// Applies the migrations not applied yet ([`migrator`]).
    ///
    /// # Errors
    ///
    /// [`Error::Migrate`] when a migration fails or one applied earlier was changed.
    pub async fn migrate(&self) -> Result<(), Error> {
        migrator().run(&self.pool).await.map_err(Error::Migrate)
    }

    /// The pool.
    #[must_use]
    pub const fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Closes every connection of the pool and waits for them to close.
    pub async fn close(self) {
        self.pool.close().await;
    }
}

/// The errors of `openbilling-pg`.
///
/// The messages are fixed text. The driver's error is the [`source`](std::error::Error::source), so a caller that
/// logs only the message logs no URL, no SQL and no value.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Connecting to the database failed.
    #[error("could not connect to PostgreSQL")]
    Connect(#[source] sqlx::Error),
    /// A statement failed.
    #[error("a PostgreSQL statement failed")]
    Query(#[source] sqlx::Error),
    /// A migration failed, or one applied earlier no longer matches its file.
    #[error("a PostgreSQL migration failed")]
    Migrate(#[source] sqlx::migrate::MigrateError),
    /// The database answered something no PostgreSQL answers.
    #[error("PostgreSQL gave an unexpected answer")]
    UnexpectedAnswer,
    /// The server is older than PostgreSQL 19, the minimum.
    #[error("PostgreSQL {version} is not supported: openbilling-rs needs PostgreSQL 19 or later")]
    UnsupportedServer {
        /// The server's `server_version_num`.
        version: i32,
    },
}

impl Classify for Error {
    fn class(&self) -> ErrorClass {
        match self {
            Self::Connect(source) | Self::Query(source) => class_of(source),
            Self::Migrate(
                sqlx::migrate::MigrateError::Execute(source)
                | sqlx::migrate::MigrateError::ExecuteMigration(source, _),
            ) => class_of(source),
            Self::Migrate(_) => ErrorClass::Internal,
            Self::UnexpectedAnswer | Self::UnsupportedServer { .. } => ErrorClass::Internal,
        }
    }
}

/// The class of a driver error: what a caller should do about it.
pub(crate) fn class_of(error: &sqlx::Error) -> ErrorClass {
    match error {
        sqlx::Error::Io(_)
        | sqlx::Error::Tls(_)
        | sqlx::Error::PoolTimedOut
        | sqlx::Error::PoolClosed
        | sqlx::Error::WorkerCrashed => ErrorClass::Unavailable,
        sqlx::Error::Database(database) => match database.code() {
            Some(code) if is_transient_sqlstate(&code) => ErrorClass::Unavailable,
            _ => ErrorClass::Internal,
        },
        _ => ErrorClass::Internal,
    }
}

/// SQLSTATEs after which the same statement may succeed later: connection exceptions (class `08`), insufficient
/// resources (`53`) and operator intervention (`57P01` to `57P03`: shutdown, crash shutdown, cannot connect now).
fn is_transient_sqlstate(code: &str) -> bool {
    code.starts_with("08") || code.starts_with("53") || matches!(code, "57P01" | "57P02" | "57P03")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unreachable_database_is_unavailable() {
        let io = std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "refused");
        assert_eq!(
            Error::Connect(sqlx::Error::Io(io)).class(),
            ErrorClass::Unavailable
        );
        assert_eq!(
            Error::Query(sqlx::Error::PoolTimedOut).class(),
            ErrorClass::Unavailable
        );
        assert_eq!(
            Error::Query(sqlx::Error::PoolClosed).class(),
            ErrorClass::Unavailable
        );
    }

    #[test]
    fn a_bad_configuration_is_internal() {
        let error = Error::Connect(sqlx::Error::Configuration("bad url".into()));
        assert_eq!(error.class(), ErrorClass::Internal);
        assert_eq!(Error::UnexpectedAnswer.class(), ErrorClass::Internal);
    }

    #[test]
    fn transient_sqlstates() {
        for code in [
            "08000", "08006", "53100", "53300", "57P01", "57P02", "57P03",
        ] {
            assert!(is_transient_sqlstate(code), "{code}");
        }
        for code in ["23505", "23514", "42P01", "40001", "57014", "XX000"] {
            assert!(!is_transient_sqlstate(code), "{code}");
        }
    }

    #[test]
    fn the_message_never_holds_the_source() {
        let error = Error::Connect(sqlx::Error::Configuration(
            "postgres://user:hunter2@db/x".into(),
        ));
        assert_eq!(error.to_string(), "could not connect to PostgreSQL");
    }

    #[test]
    fn an_old_server_is_the_deployments_to_fix() {
        let error = Error::UnsupportedServer { version: 180_001 };
        assert_eq!(error.class(), ErrorClass::Internal);
        assert_eq!(
            error.to_string(),
            "PostgreSQL 180001 is not supported: openbilling-rs needs PostgreSQL 19 or later"
        );
    }

    #[tokio::test]
    async fn a_malformed_url_is_a_connect_error() {
        let error = PgStore::connect("not a url").await.expect_err("malformed");
        assert!(matches!(error, Error::Connect(_)));
    }
}
