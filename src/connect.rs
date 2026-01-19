use std::pin::Pin;
use std::time::Duration;

use futures::StreamExt;
use helius_laserstream::{
    client,
    grpc::{SubscribeRequest, SubscribeRequestPing, SubscribeUpdate},
    LaserstreamConfig, LaserstreamError,
};
use tokio::time;

use crate::types::DlpSyncError;

/// Stream type alias for Laserstream updates.
pub type LaserStream =
    Pin<Box<dyn futures::Stream<Item = Result<SubscribeUpdate, LaserstreamError>> + Send>>;

/// Establishes a connection to Laserstream and performs health check.
///
/// Takes a config and subscription request, sends a ping to establish the connection,
/// and performs a health check to ensure the stream is working.
pub async fn connect(
    config: &LaserstreamConfig,
    request: SubscribeRequest,
) -> Result<LaserStream, DlpSyncError> {
    let (stream, handle) = client::subscribe(config.clone(), request);
    let mut stream = Box::pin(stream);

    // Send ping to establish connection
    handle
        .write(SubscribeRequest {
            ping: Some(SubscribeRequestPing { id: 0 }),
            ..Default::default()
        })
        .await
        .map_err(DlpSyncError::LaserStream)?;

    // Health check: wait for first update with timeout
    time::timeout(Duration::from_secs(5), stream.next())
        .await
        .map_err(|_| DlpSyncError::Connection("health check timed out"))?
        .ok_or_else(|| DlpSyncError::Connection("stream closed before first update"))?
        .map_err(DlpSyncError::LaserStream)?;

    Ok(stream)
}
