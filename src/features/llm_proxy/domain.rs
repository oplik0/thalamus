use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use futures::Stream;

use crate::Result;
use crate::features::backends::domain::{BackendClient, BackendRegistry, EndpointId};
use crate::features::routing::infra::RouterService;
use crate::shared::models::{ChatResponse, EmbeddingRequest, LlmRequest, StreamEvent};

pub struct ProxyService {
    router: Arc<RouterService>,
    client: Arc<dyn BackendClient>,
    registry: Arc<dyn BackendRegistry>,
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
    ) -> Self {
        Self {
            router,
            client,
            registry,
        }
    }

    pub async fn handle(&self, request: LlmRequest) -> Result<ChatResponse> {
        let endpoint = self.router.route(&request)?;
        self.registry.acquire(&endpoint.id);
        let result = self.client.send(&endpoint, &request).await;
        self.registry.release(&endpoint.id);
        result
    }

    pub async fn handle_stream(
        &self,
        request: LlmRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        let endpoint = self.router.route(&request)?;
        self.registry.acquire(&endpoint.id);

        match self.client.send_stream(&endpoint, &request).await {
            Ok(stream) => Ok(Box::pin(ReleaseOnDropStream::new(
                stream,
                Arc::clone(&self.registry),
                endpoint.id,
            ))),
            Err(error) => {
                self.registry.release(&endpoint.id);
                Err(error)
            }
        }
    }

    pub async fn handle_embedding(&self, request: EmbeddingRequest) -> Result<serde_json::Value> {
        let envelope = LlmRequest::Embedding(request.clone());
        let endpoint = self.router.route(&envelope)?;
        self.registry.acquire(&endpoint.id);
        let result = self.client.send_embedding(&endpoint, &request).await;
        self.registry.release(&endpoint.id);
        result
    }
}

pub struct ReleaseOnDropStream {
    inner: Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>,
    registry: Arc<dyn BackendRegistry>,
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
        endpoint_id: EndpointId,
    ) -> Self {
        Self {
            inner,
            registry,
            endpoint_id,
            released: false,
        }
    }

    fn release_once(&mut self) {
        if !self.released {
            self.registry.release(&self.endpoint_id);
            self.released = true;
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
