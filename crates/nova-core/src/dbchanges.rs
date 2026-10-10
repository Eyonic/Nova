//! Database change channels (prototype): MariaDB row changes become NOVA
//! Live invalidations, whoever made them (PHP, a cron task, an admin tool,
//! another application).
//!
//! NOVA reads the server's row-based binary log as a replication client.
//! Every committed change to table `T` in a site's database publishes the
//! channel `db:<t>` (table name lowercased) for that site, and drops the
//! site's micro-cache, so `data-nova-subscribe="db:orders"` regions refresh
//! and cached pages never outlive the data. Only table names leave this
//! module; row contents are never looked at.
//!
//! Commits are collected and published every [`FLUSH`] at most, so a bulk
//! update of 10 000 rows is one refresh. Rolled-back transactions publish
//! nothing (events are taken at the commit marker).
//!
//! Security note: a replication user can read every database's changes.
//! This prototype runs the reader inside the HTTP worker; a production
//! version should run it in its own sandboxed process that only emits
//! table names.

use crate::live::LiveHub;
use crate::microcache::MicroCache;
use futures_util::StreamExt;
use mysql_async::binlog::events::EventData;
use mysql_async::prelude::Queryable;
use mysql_async::{BinlogStreamRequest, Conn, OptsBuilder};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::watch;

/// Coalescing window for published changes.
pub const FLUSH: Duration = Duration::from_millis(100);

/// What to watch on one database server.
pub struct Source {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    /// NOVA's replica id (must differ from the server's `server_id`).
    pub server_id: u32,
    /// Database name -> sites using it.
    pub sites: HashMap<String, Vec<String>>,
}

/// Channel for a table, or `None` when the name cannot be one.
pub fn channel(table: &str) -> Option<String> {
    let c = format!("db:{}", table.to_ascii_lowercase());
    crate::live::valid_channel(&c).then_some(c)
}

/// Follow `src` until `stop` flips, reconnecting with backoff.
pub async fn run(
    src: Source,
    live: Arc<LiveHub>,
    micro: Arc<MicroCache>,
    mut stop: watch::Receiver<bool>,
) {
    let pending: Arc<Mutex<HashSet<(String, String)>>> = Arc::default();
    let flusher = {
        let (pending, live, micro, mut stop) =
            (Arc::clone(&pending), Arc::clone(&live), micro, stop.clone());
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(FLUSH);
            loop {
                tokio::select! {
                    _ = tick.tick() => {}
                    _ = stop.changed() => return,
                }
                let batch: Vec<_> = pending.lock().unwrap().drain().collect();
                for (site, channel) in batch {
                    // Pages tagged with this table, plus untagged pages
                    // (their dependencies are unknown).
                    micro.purge_tags(&site, std::slice::from_ref(&channel), true);
                    live.publish(&site, &channel);
                }
            }
        })
    };
    let mut backoff = Duration::from_millis(500);
    loop {
        let result = tokio::select! {
            r = follow(&src, &pending) => r,
            _ = stop.changed() => break,
        };
        match result {
            Ok(()) => tracing::warn!(host = src.host, "binlog stream ended; reconnecting"),
            Err(e) => {
                tracing::warn!(host = src.host, error = %e, "binlog stream failed; reconnecting")
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(backoff) => {}
            _ = stop.changed() => break,
        }
        backoff = (backoff * 2).min(Duration::from_secs(10));
    }
    flusher.abort();
}

async fn follow(
    src: &Source,
    pending: &Mutex<HashSet<(String, String)>>,
) -> mysql_async::Result<()> {
    let opts = OptsBuilder::default()
        .ip_or_hostname(src.host.clone())
        .tcp_port(src.port)
        .user(Some(src.user.clone()))
        .pass(Some(src.password.clone()));
    let mut conn = Conn::new(opts).await?;
    // Start at the current end of the log: only future changes matter.
    let status: mysql_async::Row = conn
        .query_first("SHOW BINLOG STATUS")
        .await?
        .ok_or_else(|| mysql_async::Error::Other("binary logging is off (log_bin)".into()))?;
    // File, Position, Binlog_Do_DB, Binlog_Ignore_DB
    let (Some(file), Some(pos)) = (status.get::<String, _>(0), status.get::<u64, _>(1)) else {
        return Err(mysql_async::Error::Other(
            "unexpected SHOW BINLOG STATUS row".into(),
        ));
    };
    tracing::info!(host = src.host, file, pos, "following database changes");
    let mut stream = conn
        .get_binlog_stream(
            BinlogStreamRequest::new(src.server_id)
                .with_filename(file.as_bytes())
                .with_pos(pos),
        )
        .await?;
    // (database, table) changed in the transaction being read.
    let mut txn: HashSet<(String, String)> = HashSet::new();
    while let Some(event) = stream.next().await {
        let event = event?;
        match event.read_data()? {
            Some(EventData::RowsEvent(rows)) => {
                if let Some(tme) = stream.get_tme(rows.table_id()) {
                    txn.insert((
                        tme.database_name().into_owned(),
                        tme.table_name().into_owned(),
                    ));
                }
            }
            // InnoDB commit marker.
            Some(EventData::XidEvent(_)) => commit(src, &mut txn, pending),
            // Non-transactional engines end with a COMMIT query.
            Some(EventData::QueryEvent(q)) if q.query().trim().eq_ignore_ascii_case("COMMIT") => {
                commit(src, &mut txn, pending)
            }
            _ => {}
        }
    }
    Ok(())
}

fn commit(
    src: &Source,
    txn: &mut HashSet<(String, String)>,
    pending: &Mutex<HashSet<(String, String)>>,
) {
    let mut pending = pending.lock().unwrap();
    for (db, table) in txn.drain() {
        let (Some(sites), Some(channel)) = (src.sites.get(&db), channel(&table)) else {
            continue;
        };
        for site in sites {
            pending.insert((site.clone(), channel.clone()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_channels() {
        assert_eq!(channel("wp_posts").as_deref(), Some("db:wp_posts"));
        assert_eq!(channel("Orders").as_deref(), Some("db:orders"));
        assert_eq!(channel("weird table"), None);
        assert_eq!(channel(&"x".repeat(80)), None);
    }

    #[test]
    fn only_watched_databases_are_published() {
        let src = Source {
            host: "db".into(),
            port: 3306,
            user: "u".into(),
            password: "p".into(),
            server_id: 4242,
            sites: HashMap::from([("nova_shop".to_string(), vec!["shop".to_string()])]),
        };
        let pending = Mutex::new(HashSet::new());
        let mut txn = HashSet::from([
            ("nova_shop".to_string(), "orders".to_string()),
            ("other_db".to_string(), "secrets".to_string()),
        ]);
        commit(&src, &mut txn, &pending);
        let got: Vec<_> = pending.lock().unwrap().iter().cloned().collect();
        assert_eq!(got, vec![("shop".to_string(), "db:orders".to_string())]);
        assert!(txn.is_empty());
    }
}
