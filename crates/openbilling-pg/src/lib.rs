//! The PostgreSQL store of openbilling-rs.
//!
//! [ADR 0001](https://github.com/vaam-apps/openbilling-rs/blob/main/docs/adr/0001-postgres-is-the-queue.md) makes
//! PostgreSQL the queue, the once-only record and the ledger. This crate will hold its tables, its migrations and the
//! hand-written SQL the ADR calls for (intake, the drain, the partition job). For now it opens a pool and checks
//! that the database answers; the tables come with the next slice.
//!
//! PostgreSQL 19 is the minimum (ADR 0001, amended 2026-11-27). The store uses its features and refuses to start
//! against an older server.

use std::time::Duration;

use openbilling_core::{Classify, ErrorClass};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

/// How long [`PgStore::connect`] and every query wait for a free connection.
const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(5);

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
        let pool = PgPoolOptions::new()
            .acquire_timeout(ACQUIRE_TIMEOUT)
            .connect(url)
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
            Self::UnexpectedAnswer | Self::UnsupportedServer { .. } => ErrorClass::Internal,
        }
    }
}

/// The class of a driver error: what a caller should do about it.
fn class_of(error: &sqlx::Error) -> ErrorClass {
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
