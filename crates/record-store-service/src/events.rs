//! Shared bucket and object application services.

use std::sync::Arc;

use record_store_events::{EventRepository, StorageEvent};

pub(crate) async fn publish_event(events: &Option<Arc<dyn EventRepository>>, event: StorageEvent) {
    let Some(events) = events else { return };
    if let Err(error) = events.publish(&event).await {
        tracing::error!(event_id = %event.id, %error, "durable storage event publication failed");
    }
}
