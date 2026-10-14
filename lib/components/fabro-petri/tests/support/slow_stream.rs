use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt as _;
use lithos_llm::Client;
use lithos_llm::adapter::{ProviderAdapter, ResolvedCall};
use lithos_llm::catalog::AdapterId;
use lithos_llm::types::{Error, Response, ResponseStream, StreamEvent};
use pebble_coding_agent::test_support::{self, ScriptedCall, ScriptedProvider};
use tokio::time;

struct SlowStream {
    inner: ScriptedProvider,
}

#[async_trait::async_trait]
impl ProviderAdapter for SlowStream {
    fn id(&self) -> &AdapterId {
        self.inner.id()
    }
    async fn complete(&self, call: &ResolvedCall) -> Result<Response, Error> {
        self.inner.complete(call).await
    }
    async fn stream(&self, call: &ResolvedCall) -> Result<ResponseStream, Error> {
        let stream = self.inner.stream(call).await?;
        Ok(ResponseStream::new(stream.then(|event| async move {
            time::sleep(Duration::from_millis(200)).await;
            event
        })))
    }
}

pub(crate) fn client() -> Client {
    let response = test_support::text_response(&"tick ".repeat(15));
    let mut events = Vec::new();
    for event in test_support::events_for(&response) {
        if let Ok(StreamEvent::TextDelta { id, .. }) = &event {
            for _ in 0..15 {
                events.push(Ok(StreamEvent::TextDelta {
                    id:   id.clone(),
                    text: "tick ".to_string(),
                }));
            }
        } else {
            events.push(event);
        }
    }
    let adapter = Arc::new(SlowStream {
        inner: ScriptedProvider::new(vec![ScriptedCall::Events(events)]),
    });
    Client::builder()
        .catalog(test_support::test_catalog())
        .adapter_arc("test", adapter)
        .build()
        .expect("slow streaming client builds")
        .client
}
