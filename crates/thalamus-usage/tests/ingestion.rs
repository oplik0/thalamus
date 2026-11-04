use chrono::{Duration, Utc};
use sqlx::PgPool;
use std::io::{Read, Write};
use std::net::TcpListener;
use thalamus_usage::{PrometheusSource, UsageStore};

fn source_with_responses(
    responses: Vec<&'static str>,
) -> (PrometheusSource, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        for response in responses {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 8192];
            let _ = stream.read(&mut request).unwrap();
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", response.len(), response).unwrap();
        }
    });
    (
        PrometheusSource::new(
            &format!("http://{address}"),
            "input_total",
            "output_total",
            3600,
        )
        .unwrap(),
        server,
    )
}

const EMPTY: &str = r#"{"status":"success","data":{"resultType":"matrix","result":[]}}"#;
const PARTIAL: &str = r#"{"status":"success","data":{"resultType":"matrix","result":[{"metric":{"team_id":"00000000-0000-0000-0000-000000000001","project_id":"00000000-0000-0000-0000-000000000002","service":"chat","traffic_class":"user","key_id":"secret"},"values":[[0,"5"]]}]}}"#;

#[sqlx::test(migrations = "./migrations")]
async fn empty_and_incomplete_observations_persist_and_retry_within_bounds(pool: PgPool) {
    let store = UsageStore::new(pool.clone());
    let today = Utc::now().date_naive();
    let empty_day = today - Duration::days(3);
    let partial_day = today - Duration::days(2);
    let (source, server) = source_with_responses(vec![EMPTY, EMPTY, PARTIAL, EMPTY]);
    assert_eq!(store.ingest_day(&source, empty_day).await.unwrap(), 0);
    assert_eq!(store.ingest_day(&source, partial_day).await.unwrap(), 1);
    server.join().unwrap();
    let rows: Vec<(chrono::NaiveDate, bool, chrono::DateTime<Utc>)> =
        sqlx::query_as("SELECT day, complete, observed_at FROM usage_ingestion_days ORDER BY day")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!((rows[0].0, rows[0].1), (empty_day, false));
    assert_eq!((rows[1].0, rows[1].1), (partial_day, false));
    assert!(rows.iter().all(|r| r.2 <= Utc::now()));
    let observed_before = Utc::now() + Duration::seconds(1);
    assert_eq!(
        store
            .next_incomplete_day(empty_day, today, observed_before)
            .await
            .unwrap(),
        Some(empty_day)
    );
    assert_eq!(
        store
            .next_incomplete_day(partial_day, today, observed_before)
            .await
            .unwrap(),
        Some(partial_day)
    );
    assert_eq!(
        store
            .next_incomplete_day(today - Duration::days(1), today, observed_before)
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        store
            .next_incomplete_day(empty_day, today, Utc::now() - Duration::hours(1))
            .await
            .unwrap(),
        None
    );
    let counters: (Option<f64>, Option<f64>) =
        sqlx::query_as("SELECT input_tokens, output_tokens FROM usage_daily WHERE day = $1")
            .bind(partial_day)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(counters, (None, None));
}

#[sqlx::test(migrations = "./migrations")]
async fn transport_failure_does_not_replace_previous_day_observation(pool: PgPool) {
    let store = UsageStore::new(pool.clone());
    let day = Utc::now().date_naive() - Duration::days(1);
    let (source, server) = source_with_responses(vec![EMPTY, EMPTY]);
    store.ingest_day(&source, day).await.unwrap();
    server.join().unwrap();
    let before: (chrono::DateTime<Utc>, bool) =
        sqlx::query_as("SELECT observed_at, complete FROM usage_ingestion_days WHERE day = $1")
            .bind(day)
            .fetch_one(&pool)
            .await
            .unwrap();
    // The test server has closed after its two responses.
    assert!(store.ingest_day(&source, day).await.is_err());
    let after: (chrono::DateTime<Utc>, bool) =
        sqlx::query_as("SELECT observed_at, complete FROM usage_ingestion_days WHERE day = $1")
            .bind(day)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(before, after);
}
