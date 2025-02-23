pub mod config;

use std::{
    marker::PhantomData,
    pin::Pin,
    task::{Context, Poll},
};

use apibara_core::node::v1alpha2::{
    stream_client::StreamClient, stream_data_response, Cursor, DataFinality, StreamDataRequest,
    StreamDataResponse,
};
use futures::Stream;
use pin_project::pin_project;
use prost::Message;
use tokio::sync::mpsc::{self, Receiver, Sender};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{
    metadata::{errors::InvalidMetadataValue, MetadataValue},
    transport::Channel,
    Streaming,
};
use tracing::debug;

// Re-export tonic Uri
pub use tonic::transport::Uri;

pub use crate::config::Configuration;

#[derive(Debug, thiserror::Error)]
pub enum ClientBuilderError {
    #[error("Failed to build indexer")]
    FailedToBuildIndexer,
    #[error(transparent)]
    TonicError(#[from] tonic::transport::Error),
    #[error(transparent)]
    FailedToConfigureStream(Box<dyn std::error::Error>),
    #[error(transparent)]
    InvalidMetadata(#[from] InvalidMetadataValue),
    #[error(transparent)]
    StreamError(#[from] tonic::Status),
}

/// A message generated by [DataStream].
#[derive(Debug)]
pub enum DataMessage<D: Message + Default> {
    /// A new batch of data.
    Data {
        /// The batch starting cursor.
        cursor: Option<Cursor>,
        /// The batch end cursor.
        ///
        /// Use this value as the start cursor to receive data for the next batch.
        end_cursor: Cursor,
        /// The data finality.
        finality: DataFinality,
        /// The batch of data.
        batch: Vec<D>,
    },
    /// Invalidate all data received after the given cursor.
    Invalidate {
        /// The cursor.
        cursor: Option<Cursor>,
    },
}

/// Data stream builder.
///
/// This struct is used to configure and connect to an Apibara data stream.
#[derive(Default)]
pub struct ClientBuilder<F, D>
where
    F: Message + Default,
    D: Message + Default,
{
    token: Option<String>,
    configuration: Option<Configuration<F>>,
    _data: PhantomData<D>,
}

/// A stream of on-chain data.
#[derive(Debug)]
#[pin_project]
pub struct DataStream<F, D>
where
    F: Message + Default,
    D: Message + Default,
{
    stream_id: u64,
    configuration_rx: Receiver<Configuration<F>>,
    #[pin]
    inner: Streaming<StreamDataResponse>,
    inner_tx: Sender<StreamDataRequest>,
    _data: PhantomData<D>,
}

/// A client used to control a data stream.
pub type DataStreamClient<F> = Sender<Configuration<F>>;

impl<F, D> ClientBuilder<F, D>
where
    F: Message + Default,
    D: Message + Default,
{
    /// Use the given `token` to authenticate with the server.
    pub fn with_bearer_token(mut self, token: String) -> Self {
        self.token = Some(token);
        self
    }

    /// Send the given configuration upon connect.
    pub fn with_configuration(mut self, configuration: Configuration<F>) -> Self {
        self.configuration = Some(configuration);
        self
    }

    /// Create and connect to the stream at the given url.
    ///
    /// If a configuration was provided, the client will immediately send it to the server upon
    /// connecting.
    pub async fn connect(
        self,
        url: Uri,
    ) -> Result<(DataStream<F, D>, DataStreamClient<F>), ClientBuilderError> {
        let channel = Channel::builder(url).connect().await?;

        let mut default_client =
            StreamClient::with_interceptor(channel, move |mut req: tonic::Request<()>| {
                if let Some(token) = self.token.clone() {
                    let token: MetadataValue<_> = format!("Bearer {token}").parse().unwrap();
                    req.metadata_mut().insert("authorization", token);
                }
                Ok(req)
            });

        let (configuration_tx, configuration_rx) = mpsc::channel(128);
        let (inner_tx, inner_rx) = mpsc::channel(128);

        if let Some(configuration) = self.configuration {
            configuration_tx.send(configuration).await.unwrap();
        }

        let inner_stream = default_client
            .stream_data(ReceiverStream::new(inner_rx))
            .await?
            .into_inner();

        let stream = DataStream {
            stream_id: 0,
            configuration_rx,
            inner: inner_stream,
            inner_tx,
            _data: PhantomData::default(),
        };

        Ok((stream, configuration_tx))
    }
}

impl<F, D> Stream for DataStream<F, D>
where
    F: Message + Default,
    D: Message + Default,
{
    type Item = Result<DataMessage<D>, Box<dyn std::error::Error>>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.configuration_rx.poll_recv(cx) {
            Poll::Ready(None) => return Poll::Ready(None),
            Poll::Ready(Some(configuration)) => {
                self.stream_id += 1;
                let request = StreamDataRequest {
                    stream_id: Some(self.stream_id),
                    batch_size: Some(configuration.batch_size),
                    starting_cursor: configuration.starting_cursor,
                    finality: configuration.finality.map(|f| f as i32),
                    filter: configuration.filter.encode_to_vec(),
                };

                self.inner_tx.try_send(request)?;
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
            Poll::Pending => {}
        }

        match Pin::new(&mut self.inner).poll_next(cx) {
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
            Poll::Ready(Some(Err(e))) => Poll::Ready(Some(Err(Box::new(e)))),
            Poll::Ready(Some(Ok(response))) => {
                if response.stream_id != self.stream_id {
                    cx.waker().wake_by_ref();
                    return Poll::Pending;
                }

                match response.message {
                    None => {
                        cx.waker().wake_by_ref();
                        Poll::Pending
                    }
                    Some(stream_data_response::Message::Data(data)) => {
                        let batch = data
                            .data
                            .into_iter()
                            .map(|b| D::decode(b.as_slice()))
                            .filter_map(|b| b.ok())
                            .collect::<Vec<D>>();
                        let message = DataMessage::Data {
                            cursor: data.cursor,
                            end_cursor: data.end_cursor.unwrap_or_default(),
                            finality: DataFinality::from_i32(data.finality).unwrap_or_default(),
                            batch,
                        };
                        Poll::Ready(Some(Ok(message)))
                    }
                    Some(stream_data_response::Message::Invalidate(invalidate)) => {
                        let message = DataMessage::Invalidate {
                            cursor: invalidate.cursor,
                        };
                        Poll::Ready(Some(Ok(message)))
                    }
                    Some(stream_data_response::Message::Heartbeat(_)) => {
                        debug!("received heartbeat");
                        cx.waker().wake_by_ref();
                        Poll::Pending
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{ClientBuilder, Configuration, Uri};
    use apibara_core::starknet::v1alpha2::{Block, Filter, HeaderFilter};
    use futures_util::{StreamExt, TryStreamExt};

    #[tokio::test]
    async fn test_apibara_high_level_api() -> Result<(), Box<dyn std::error::Error>> {
        let (stream, configuration_handle) = ClientBuilder::<Filter, Block>::default()
            .with_bearer_token("my_auth_token".into())
            // Using default server aka. mainnet
            .connect(Uri::from_static("https://mainnet.starknet.a5a.ch"))
            .await?;

        configuration_handle
            .send(
                Configuration::<Filter>::default()
                    .with_starting_block(21600)
                    .with_filter(|mut filter| {
                        filter.with_header(HeaderFilter { weak: false }).build()
                    }),
            )
            .await?;

        let mut stream = stream.take(2);
        while let Some(response) = stream.try_next().await? {
            println!("Response: {:?}", response);
        }

        Ok(())
    }
}
