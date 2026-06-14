//! Batch job infrastructure

use std::sync::Arc;

use sqlx::PgPool;
use uuid::Uuid;

use crate::Result;
use crate::features::batch::domain::{BatchJob, BatchJobStatus, BatchRepository};

/// PostgreSQL-backed batch job repository.
#[derive(Debug)]
pub struct SqlxBatchRepository {
    pool: PgPool,
}

impl SqlxBatchRepository {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl BatchRepository for SqlxBatchRepository {
    async fn create(&self, job: &BatchJob) -> Result<()> {
        let request_body = serde_json::to_value(&job.request_body)?;

        sqlx::query!(
            r#"
            INSERT INTO batch_jobs (
                id, team_id, user_id,
                request_body, status, request_count,
                created_at
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7)
            "#,
            job.id,
            job.team_id,
            job.user_id,
            request_body,
            job.status as BatchJobStatus,
            job.request_count,
            job.created_at
        )
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    async fn get(&self, id: Uuid) -> Result<Option<BatchJob>> {
        let row = sqlx::query!(
            r#"
            SELECT
                id, team_id, user_id,
                request_body as "request_body!: serde_json::Value",
                status as "status!: BatchJobStatus",
                response_body as "response_body?: serde_json::Value",
                error_message,
                request_count,
                completed_count,
                failed_count,
                created_at,
                started_at,
                completed_at
            FROM batch_jobs
            WHERE id = $1
            "#,
            id
        )
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|r| BatchJob {
            id: r.id,
            team_id: r.team_id,
            user_id: r.user_id,
            request_body: serde_json::from_value(r.request_body).unwrap_or_default(),
            status: r.status,
            response_body: r.response_body,
            error_message: r.error_message,
            request_count: r.request_count,
            completed_count: r.completed_count,
            failed_count: r.failed_count,
            created_at: r.created_at,
            started_at: r.started_at,
            completed_at: r.completed_at,
        }))
    }

    async fn claim_pending(&self, _worker_id: Uuid, limit: usize) -> Result<Vec<BatchJob>> {
        // For simplicity, the worker pulls the oldest pending jobs. A future
        // improvement would use row-level locking / advisory locks for multiple
        // workers.
        let rows = sqlx::query!(
            r#"
            SELECT
                id, team_id, user_id,
                request_body as "request_body!: serde_json::Value",
                status as "status!: BatchJobStatus",
                response_body as "response_body?: serde_json::Value",
                error_message,
                request_count,
                completed_count,
                failed_count,
                created_at,
                started_at,
                completed_at
            FROM batch_jobs
            WHERE status = 'pending'
            ORDER BY created_at ASC
            LIMIT $1
            "#,
            limit as i64
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|r| BatchJob {
                id: r.id,
                team_id: r.team_id,
                user_id: r.user_id,
                request_body: serde_json::from_value(r.request_body).unwrap_or_default(),
                status: r.status,
                response_body: r.response_body,
                error_message: r.error_message,
                request_count: r.request_count,
                completed_count: r.completed_count,
                failed_count: r.failed_count,
                created_at: r.created_at,
                started_at: r.started_at,
                completed_at: r.completed_at,
            })
            .collect())
    }

    async fn mark_processing(&self, id: Uuid) -> Result<()> {
        sqlx::query!(
            r#"
            UPDATE batch_jobs
            SET status = 'processing', started_at = NOW()
            WHERE id = $1
            "#,
            id
        )
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    async fn mark_completed(
        &self,
        id: Uuid,
        results: serde_json::Value,
        completed: usize,
        failed: usize,
    ) -> Result<()> {
        sqlx::query!(
            r#"
            UPDATE batch_jobs
            SET status = 'completed',
                response_body = $2,
                completed_count = $3,
                failed_count = $4,
                completed_at = NOW()
            WHERE id = $1
            "#,
            id,
            results,
            completed as i32,
            failed as i32
        )
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    async fn mark_failed(&self, id: Uuid, error: &str) -> Result<()> {
        sqlx::query!(
            r#"
            UPDATE batch_jobs
            SET status = 'failed', error_message = $2, completed_at = NOW()
            WHERE id = $1
            "#,
            id,
            error
        )
        .execute(&self.pool)
        .await?;

        Ok(())
    }
}

/// Spawn a background worker that polls for pending batch jobs and processes
/// them at low priority.
pub fn spawn_batch_worker(
    repository: Arc<dyn BatchRepository>,
    proxy: Arc<crate::features::llm_proxy::domain::ProxyService>,
    shutdown: tokio_util::sync::CancellationToken,
) {
    let service = Arc::new(crate::features::batch::domain::BatchService::new(
        repository, proxy,
    ));

    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(5));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = interval.tick() => {
                    match service.repository.claim_pending(Uuid::new_v4(), 10).await {
                        Ok(jobs) => {
                            for job in jobs {
                                let service = Arc::clone(&service);
                                tokio::spawn(async move {
                                    if let Err(error) = service.process_job(&job).await {
                                        tracing::error!(job_id = %job.id, error = %error, "Failed to process batch job");
                                        let _ = service.repository.mark_failed(job.id, &error.to_string()).await;
                                    }
                                });
                            }
                        }
                        Err(error) => {
                            tracing::error!(error = %error, "Batch worker failed to claim jobs");
                        }
                    }
                }
                _ = shutdown.cancelled() => break,
            }
        }
    });
}
