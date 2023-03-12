use rdkafka::{admin::AdminClient, client::DefaultClientContext, ClientConfig};
use tokio::{
    sync::{broadcast, mpsc},
    task::JoinHandle,
    time::{interval, Duration},
};

use crate::internals::Emitter;
use crate::kafka_types::{Broker, TopicPartitionsStatus};

const CHANNEL_SIZE: usize = 1;
const SEND_TIMEOUT: Duration = Duration::from_millis(100);

const FETCH_TIMEOUT: Duration = Duration::from_secs(5);
const FETCH_INTERVAL: Duration = Duration::from_secs(10);

/// This is a `Send`-able struct to carry Kafka Cluster status across thread boundaries.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct ClusterStatus {
    /// A vector of [`TopicPartitionsStatus`].
    ///
    /// It reflects the status of Topics (and Partitions) as reported by the Kafka cluster.
    pub topics: Vec<TopicPartitionsStatus>,

    /// A vector of [`Broker`].
    ///
    /// It reflects the status of Brokers as reported by the Kafka cluster.
    pub brokers: Vec<Broker>,
}

/// Emits [`ClusterStatus`] via a provided [`mpsc::channel`].
///
/// It wraps an Admin Kafka Client, regularly requests it for the cluster metadata,
/// and then emits it as [`ClusterStatus`].
///
/// It shuts down by sending a unit via a provided [`broadcast`].
pub struct ClusterStatusEmitter {
    admin_client_config: ClientConfig,
}

impl ClusterStatusEmitter {
    /// Create a new [`ClusterStatusEmitter`]
    ///
    /// # Arguments
    ///
    /// * `client_config` - Kafka admin client configuration, used to fetch the Cluster current status
    pub fn new(client_config: ClientConfig) -> ClusterStatusEmitter {
        ClusterStatusEmitter {
            admin_client_config: client_config,
        }
    }
}

impl Emitter for ClusterStatusEmitter {
    type Emitted = ClusterStatus;

    /// Spawn a new async task to run the business logic of this struct.
    ///
    /// When this emitter gets spawned, it returns a [`broadcast::Receiver`] for [`ClusterStatus`],
    /// and a [`JoinHandle`] to help join on the task spawned internally.
    /// The task concludes (joins) only ones the inner task of the emitter terminates.
    ///
    /// # Arguments
    ///
    /// * `shutdown_rx`: A [`broadcast::Receiver`] to request the internal async task to shutdown.
    ///
    fn spawn(
        &self,
        mut shutdown_rx: broadcast::Receiver<()>,
    ) -> (mpsc::Receiver<Self::Emitted>, JoinHandle<()>) {
        let admin_client: AdminClient<DefaultClientContext> = self
            .admin_client_config
            .create()
            .expect("Failed to allocate Admin Client");

        let (sx, rx) = mpsc::channel::<ClusterStatus>(CHANNEL_SIZE);

        let join_handle = tokio::spawn(async move {
            let mut interval = interval(FETCH_INTERVAL);

            'outer: loop {
                match admin_client.inner().fetch_metadata(None, FETCH_TIMEOUT) {
                    Ok(m) => {
                        // NOTE: Turn metadata into our `Send`-able type
                        let status = ClusterStatus {
                            topics: m
                                .topics()
                                .iter()
                                .map(TopicPartitionsStatus::from)
                                .collect(),
                            brokers: m
                                .brokers()
                                .iter()
                                .map(Broker::from)
                                .collect(),
                        };

                        let ch_cap = sx.capacity();
                        if ch_cap == 0 {
                            warn!("Emitting channel saturated: receiver too slow?");
                        }

                        tokio::select! {
                            // Send the latest `ClusterStatus`
                            res = sx.send_timeout(status, SEND_TIMEOUT) => {
                                if let Err(e) = res {
                                    error!("Failed to emit cluster status: {e}");
                                }
                            },

                            // Initiate shutdown: by letting this task conclude,
                            // the receiver of `ClusterStatus` will detect the channel is closing
                            // on the sender end, and conclude its own activity/task.
                            _ = shutdown_rx.recv() => {
                                info!("Received shutdown signal");
                                break 'outer;
                            },
                        }
                    },
                    Err(e) => {
                        error!("Failed to fetch cluster metadata: {e}");
                    },
                }

                interval.tick().await;
            }
        });

        (rx, join_handle)
    }
}
