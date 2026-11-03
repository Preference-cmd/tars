//! ModelGateway wrapper that counts every request attempt and attributes usage per purpose.

use super::hub::UsageHub;
use super::types::UsagePurpose;
use causa::kernel::{
    CallControl, ModelGateway, ModelInvokeError, ModelOutput, ModelRequest, ModelStream,
    StreamDelta,
};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};

/// Gateway layer placed between the retry layer and the protocol gateway to count every attempt.
pub(crate) struct CountingGateway {
    inner: Arc<dyn ModelGateway>,
    purpose: UsagePurpose,
    hub: Arc<UsageHub>,
}

impl CountingGateway {
    /// Construct a new counting gateway wrapping the given inner gateway.
    pub(crate) fn new(
        inner: Arc<dyn ModelGateway>,
        purpose: UsagePurpose,
        hub: Arc<UsageHub>,
    ) -> Self {
        Self {
            inner,
            purpose,
            hub,
        }
    }
}

#[async_trait::async_trait]
impl ModelGateway for CountingGateway {
    async fn invoke(
        &self,
        request: &ModelRequest,
        control: &CallControl,
    ) -> Result<ModelOutput, ModelInvokeError> {
        match self.inner.invoke(request, control).await {
            Ok(output) => {
                self.hub
                    .record_call_success(self.purpose, output.usage.as_ref());
                Ok(output)
            }
            Err(error) => {
                self.hub.record_call_failure(self.purpose);
                Err(error)
            }
        }
    }

    async fn stream(
        &self,
        request: &ModelRequest,
        control: &CallControl,
    ) -> Result<ModelStream, ModelInvokeError> {
        match self.inner.stream(request, control).await {
            Ok(inner) => Ok(Box::pin(CountingStream::new(
                inner,
                self.purpose,
                self.hub.clone(),
            ))),
            Err(error) => {
                self.hub.record_call_failure(self.purpose);
                Err(error)
            }
        }
    }
}

/// Stream wrapper ensuring streaming attempts are counted exactly once.
struct CountingStream {
    inner: ModelStream,
    purpose: UsagePurpose,
    hub: Arc<UsageHub>,
    settled: bool,
}

impl CountingStream {
    fn new(inner: ModelStream, purpose: UsagePurpose, hub: Arc<UsageHub>) -> Self {
        Self {
            inner,
            purpose,
            hub,
            settled: false,
        }
    }
}

impl futures_util::Stream for CountingStream {
    type Item = StreamDelta;

    fn poll_next(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.settled {
            return Poll::Ready(None);
        }
        match this.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(StreamDelta::Done { final_output })) => {
                this.settled = true;
                this.hub
                    .record_call_success(this.purpose, final_output.usage.as_ref());
                Poll::Ready(Some(StreamDelta::Done { final_output }))
            }
            Poll::Ready(Some(StreamDelta::Error { error })) => {
                this.settled = true;
                this.hub.record_call_failure(this.purpose);
                Poll::Ready(Some(StreamDelta::Error { error }))
            }
            Poll::Ready(Some(delta)) => Poll::Ready(Some(delta)),
            Poll::Ready(None) => {
                // Stream completed without StreamDelta::Done.
                this.settled = true;
                this.hub.record_call_failure(this.purpose);
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Drop for CountingStream {
    fn drop(&mut self) {
        if !self.settled {
            self.settled = true;
            self.hub.record_call_failure(self.purpose);
        }
    }
}
