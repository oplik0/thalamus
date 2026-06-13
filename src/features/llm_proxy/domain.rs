use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use futures::Stream;

use crate::Result;
use crate::features::backends::domain::{BackendClient, BackendRegistry, EndpointId};
use crate::features::plugin::guardrail_bridge::GuardrailService;
use crate::features::routing::infra::RouterService;
use crate::features::routing::queue::Priority;
use crate::shared::models::{ChatResponse, EmbeddingRequest, LlmRequest, StreamEvent};

pub struct ProxyService {
    router: Arc<RouterService>,
    client: Arc<dyn BackendClient>,
    registry: Arc<dyn BackendRegistry>,
    guardrails: GuardrailService,
}

impl std::fmt::Debug for ProxyService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxyService").finish_non_exhaustive()
    }
}

impl ProxyService {
    #[must_use]
    pub fn new(
        router: Arc<RouterService>,
        client: Arc<dyn BackendClient>,
        registry: Arc<dyn BackendRegistry>,
        guardrails: GuardrailService,
    ) -> Self {
        Self {
            router,
            client,
            registry,
            guardrails,
        }
    }

    pub async fn handle(&self, request: LlmRequest, priority: Priority) -> Result<ChatResponse> {
        self.guardrails.inspect_request(&request)?;

        let endpoint = self.router.route_or_queue(&request, priority).await?;
        let mut result = self.client.send(&endpoint, &request).await;
        self.registry.release(&endpoint.id);
        self.dispatch_queue().await;

        if let Ok(ref response) = result
            && let Err(guardrail_err) = self.guardrails.inspect_response(response)
        {
            result = Err(guardrail_err);
        }

        result
    }

    pub async fn handle_stream(
        &self,
        request: LlmRequest,
        priority: Priority,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        self.guardrails.inspect_request(&request)?;

        let endpoint = self.router.route_or_queue(&request, priority).await?;

        match self.client.send_stream(&endpoint, &request).await {
            Ok(stream) => Ok(Box::pin(ReleaseOnDropStream::new(
                stream,
                Arc::clone(&self.registry),
                Arc::clone(&self.router),
                endpoint.id,
            ))),
            Err(error) => {
                self.registry.release(&endpoint.id);
                self.dispatch_queue().await;
                Err(error)
            }
        }
    }

    pub async fn handle_embedding(
        &self,
        request: EmbeddingRequest,
        priority: Priority,
    ) -> Result<serde_json::Value> {
        let envelope = LlmRequest::Embedding(request.clone());
        self.guardrails.inspect_request(&envelope)?;

        let endpoint = self.router.route_or_queue(&envelope, priority).await?;
        let result = self.client.send_embedding(&endpoint, &request).await;
        self.registry.release(&endpoint.id);
        self.dispatch_queue().await;
        result
    }

    async fn dispatch_queue(&self) {
        let router = Arc::clone(&self.router);
        self.router
            .queue_manager()
            .try_dispatch(&move |req| router.dispatch_one_queued(req))
            .await;
    }
}

pub struct ReleaseOnDropStream {
    inner: Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>,
    registry: Arc<dyn BackendRegistry>,
    router: Arc<RouterService>,
    endpoint_id: EndpointId,
    released: bool,
}

impl std::fmt::Debug for ReleaseOnDropStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReleaseOnDropStream")
            .finish_non_exhaustive()
    }
}

impl ReleaseOnDropStream {
    #[must_use]
    pub fn new(
        inner: Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>,
        registry: Arc<dyn BackendRegistry>,
        router: Arc<RouterService>,
        endpoint_id: EndpointId,
    ) -> Self {
        Self {
            inner,
            registry,
            router,
            endpoint_id,
            released: false,
        }
    }

    fn release_once(&mut self) {
        if !self.released {
            self.registry.release(&self.endpoint_id);
            self.released = true;

            // Spawn dispatch so the stream's Drop doesn't block on queue locks.
            let router = Arc::clone(&self.router);
            tokio::spawn(async move {
                router
                    .queue_manager()
                    .try_dispatch(&move |req| router.dispatch_one_queued(req))
                    .await;
            });
        }
    }
}

impl Stream for ReleaseOnDropStream {
    type Item = Result<StreamEvent>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.as_mut().get_mut();
        match this.inner.as_mut().poll_next(cx) {
            Poll::Ready(None) => {
                this.release_once();
                Poll::Ready(None)
            }
            other => other,
        }
    }
}

impl Drop for ReleaseOnDropStream {
    fn drop(&mut self) {
        self.release_once();
    }
}
