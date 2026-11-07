//! Daily aggregate queries.

use super::*;

impl StatsDb {
    /// Return statistics for today (local date). Returns a zeroed `DayStats`
    /// with today's date if no requests have been recorded yet.
    pub fn query_today(&self) -> Result<DayStats> {
        let date = local_date_string();
        self.query_date(&date)
    }

    /// Return statistics for an arbitrary date (`YYYY-MM-DD`), aggregated from
    /// the request logs for that day.
    pub fn query_date(&self, date: &str) -> Result<DayStats> {
        let handle = self.read_conn();
        let result = handle.get().query_row(
            "SELECT
                COUNT(*),
                COALESCE(SUM(CASE WHEN status >= 200 AND status < 300 AND error IS NULL
                                  THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN status >= 200 AND status < 300 AND error IS NULL
                                  THEN 0 ELSE 1 END), 0),
                COALESCE(SUM(input_tokens), 0),
                COALESCE(SUM(cache_read_tokens), 0),
                COALESCE(SUM(cache_write_tokens), 0),
                COALESCE(SUM(output_tokens), 0),
                COALESCE(SUM(CASE WHEN override_key != '' THEN 1 ELSE 0 END), 0)
             FROM request_logs WHERE date = ?1",
            params![date],
            |row| {
                Ok(DayStats {
                    date: date.to_string(),
                    requests_total: row.get(0)?,
                    requests_success: row.get(1)?,
                    requests_failed: row.get(2)?,
                    tokens_input: row.get(3)?,
                    tokens_cache_read: row.get(4)?,
                    tokens_cache_write: row.get(5)?,
                    tokens_output: row.get(6)?,
                    overrides_total: row.get(7)?,
                })
            },
        );

        match result {
            Ok(stats) => Ok(stats),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(DayStats {
                date: date.to_string(),
                ..Default::default()
            }),
            Err(e) => Err(e.into()),
        }
    }
}
