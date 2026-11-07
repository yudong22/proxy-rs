//! The background writer thread that batches rows into one transaction.

use super::*;

/// Drains the request queue and commits rows in batches.
///
/// Runs on its own OS thread for the lifetime of the `StatsDb`. The first
/// `recv` blocks until work arrives; everything already queued is then drained
/// into a single transaction, so a burst of requests costs one commit.
pub(crate) fn writer_loop(conn: Connection, rx: Receiver<RequestRow>) {
    let mut stmt = match conn.prepare(RequestRow::insert_sql()) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("stats writer failed to prepare insert: {}", e);
            return;
        }
    };

    // recv() returns Err only when every sender has been dropped, i.e. the
    // StatsDb is gone — that is the signal to exit.
    //
    // Each batch is unwrapped individually. This is the only consumer of the
    // queue: if it panicked, every later `record_request_log` would fail
    // forever while the proxy kept serving requests perfectly — the 请求日志 and
    // 用量统计 panels would silently freeze with nothing anywhere to explain
    // why. Containing the panic keeps one bad row from ending the thread's
    // life, and logs it instead.
    while let Ok(first) = rx.recv() {
        let mut batch = vec![first];
        while let Ok(next) = rx.try_recv() {
            batch.push(next);
        }

        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            insert_batch(&conn, &mut stmt, &batch)
        }));
        match outcome {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::warn!("stats writer dropped {} row(s): {}", batch.len(), e);
            }
            Err(_) => {
                // The statement may be left in a broken state; rebuild it so
                // the next batch starts clean instead of failing the same way.
                tracing::error!(
                    "stats writer panicked on {} row(s); recovering",
                    batch.len()
                );
                match conn.prepare(RequestRow::insert_sql()) {
                    Ok(fresh) => stmt = fresh,
                    Err(e) => {
                        tracing::error!("stats writer could not rebuild its statement: {}", e);
                        return;
                    }
                }
            }
        }
    }
}

fn insert_batch(
    conn: &Connection,
    stmt: &mut rusqlite::Statement<'_>,
    batch: &[RequestRow],
) -> rusqlite::Result<()> {
    let tx = conn.unchecked_transaction()?;
    for row in batch {
        row.bind_to(stmt)?;
    }
    tx.commit()
}
