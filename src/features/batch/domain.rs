//! Batch job domain
//!
//! OpenAI-style batch API for asynchronous, lower-priority processing of
//! multiple chat completion requests.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::Result;
use crate::features::llm_proxy::domain::ProxyService;
use crate::features::routing::queue::Priority;
use crate::shared::models::{ChatRequest, ChatResponse};

/// High-level status of a batch job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "batch_job_status", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum BatchJobStatus {
    Pending,
    Processing,
    Completed,
    Failed,
    Cancelled,
}

impl std::fmt::Display for BatchJobStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pending => write!(f, "pending"),
            Self::Processing => write!(f, "processing"),
            Self::Completed => write!(f, "completed"),
            Self::Failed => write!(f, "failed"),
            Self::Cancelled => write!(f, "cancelled"),
        }
    }
}

/// A stored batch job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchJob {
    pub id: Uuid,
    pub team_id: Option<Uuid>,
    pub user_id: Option<Uuid>,
    pub request_body: BatchRequestBody,
    pub status: BatchJobStatus,
    pub response_body: Option<serde_json::Value>,
    pub error_message: Option<String>,
    pub request_count: i32,
    pub completed_count: i32,
    pub failed_count: i32,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
}

/// Request body accepted by the batch creation endpoint.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BatchRequestBody {
    pub requests: Vec<ChatRequest>,
}

/// Result item for a single request within a batch.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum BatchResultItem {
    Success {
        #[serde(flatten)]
        response: ChatResponse,
    },
    Error {
        error: String,
    },
}

/// Repository for batch job persistence.
#[async_trait::async_trait]
pub trait BatchRepository: Send + Sync {
    async fn create(&self, job: &BatchJob) -> Result<()>;
    async fn get(&self, id: Uuid) -> Result<Option<BatchJob>>;
    async fn claim_pending(&self, worker_id: Uuid, limit: usize) -> Result<Vec<BatchJob>>;
    async fn mark_processing(&self, id: Uuid) -> Result<()>;
    async fn mark_completed(
        &self,
        id: Uuid,
        results: serde_json::Value,
        completed: usize,
        failed: usize,
    ) -> Result<()>;
    async fn mark_failed(&self, id: Uuid, error: &str) -> Result<()>;
}

/// Service that creates batch jobs and can spawn a background worker to
/// process them at low priority.
pub struct BatchService {
    pub(crate) repository: Arc<dyn BatchRepository>,
    pub(crate) proxy: Arc<ProxyService>,
}

impl std::fmt::Debug for BatchService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BatchService").finish_non_exhaustive()
    }
}

impl BatchService {
    #[must_use]
    pub fn new(repository: Arc<dyn BatchRepository>, proxy: Arc<ProxyService>) -> Self {
        Self { repository, proxy }
    }

    /// Create a new batch job. Returns the job id.
    pub async fn create_job(
        &self,
        body: BatchRequestBody,
        team_id: Option<Uuid>,
        user_id: Option<Uuid>,
    ) -> Result<Uuid> {
        let request_count = body.requests.len();
        let job = BatchJob {
            id: Uuid::new_v4(),
            team_id,
            user_id,
            request_body: body,
            status: BatchJobStatus::Pending,
            response_body: None,
            error_message: None,
            request_count: request_count as i32,
            completed_count: 0,
            failed_count: 0,
            created_at: Utc::now(),
            started_at: None,
            completed_at: None,
        };

        let id = job.id;
        self.repository.create(&job).await?;
        Ok(id)
    }

    /// Fetch a batch job by id.
    pub async fn get_job(&self, id: Uuid) -> Result<Option<BatchJob>> {
        self.repository.get(id).await
    }

    /// Process a single pending job synchronously. Intended for use by the
    /// background worker.
    pub async fn process_job(&self, job: &BatchJob) -> Result<()> {
        self.repository.mark_processing(job.id).await?;

        let mut completed = 0usize;
        let mut failed = 0usize;
        let mut results = Vec::with_capacity(job.request_body.requests.len());

        for request in &job.request_body.requests {
            let unified = crate::shared::models::LlmRequest::Chat(request.clone());
            match self.proxy.handle(unified, Priority::Batch).await {
                Ok(response) => {
                    completed += 1;
                    results.push(BatchResultItem::Success { response });
                }
                Err(error) => {
                    failed += 1;
                    results.push(BatchResultItem::Error {
                        error: error.to_string(),
                    });
                }
            }
        }

        let response_body = serde_json::to_value(results)?;
        self.repository
            .mark_completed(job.id, response_body, completed, failed)
            .await?;

        Ok(())
    }
}
