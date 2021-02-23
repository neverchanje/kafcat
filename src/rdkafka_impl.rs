use crate::configs::KafkaConsumerConfig;
use crate::configs::KafkaOffset;
use crate::configs::KafkaProducerConfig;
use crate::error::KafcatError;
use crate::interface::CustomConsumer;
use crate::interface::CustomMessage;
use crate::interface::CustomProducer;
use crate::interface::KafkaInterface;
use crate::timeout_stream::TimeoutStreamExt;
use futures::stream::MapErr;
use futures::{Stream, TryFuture, TryFutureExt};
use futures::{StreamExt, TryStreamExt};
use rdkafka::client::DefaultClientContext;
use rdkafka::consumer::{Consumer, MessageStream};
use rdkafka::consumer::{DefaultConsumerContext, StreamConsumer};
use rdkafka::error::{KafkaError, KafkaResult};
use rdkafka::message::BorrowedMessage;
use rdkafka::producer::FutureProducer;
use rdkafka::producer::FutureRecord;
use rdkafka::ClientConfig;
use rdkafka::Message;
use rdkafka::Offset;
use rdkafka::TopicPartitionList;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::sync::{Mutex, OwnedMutexGuard};
use tokio::task::spawn_blocking;
use tokio::task::JoinHandle;

pub struct RdKafka {}
impl KafkaInterface for RdKafka {
    type Consumer = RdkafkaConsumer;
    type Message = RdkafkaMessage;
    type Producer = RdkafkaProducer;
}

pub struct RdkafkaConsumer {
    stream: Arc<Mutex<StreamConsumer>>,
    config: KafkaConsumerConfig,
}

#[async_trait]
impl CustomConsumer for RdkafkaConsumer {
    type Message = RdkafkaMessage;
    type StreamType = RdkafkaStreamImpl;

    fn from_config(kafka_config: KafkaConsumerConfig) -> Self
    where
        Self: Sized,
    {
        let stream: StreamConsumer = ClientConfig::new()
            .set("group.id", &kafka_config.group_id)
            .set("bootstrap.servers", &kafka_config.brokers)
            .set("enable.partition.eof", "false")
            .set("session.timeout.ms", "6000")
            .set("enable.auto.commit", "false")
            .create()
            .expect("Consumer creation failed");

        RdkafkaConsumer {
            stream: Arc::new(Mutex::new(stream)),
            config: kafka_config,
        }
    }

    async fn set_offset(&self, offset: KafkaOffset) -> Result<(), KafcatError> {
        info!("offset {:?}", offset);
        let mut tpl = TopicPartitionList::new();
        let partition = self.config.partition.unwrap_or(0);
        let topic = self.config.topic.clone();
        let offset = match offset {
            KafkaOffset::Beginning => Offset::Beginning,
            KafkaOffset::End => Offset::End,
            KafkaOffset::Stored => Offset::Stored,
            KafkaOffset::Offset(o) if o >= 0 => Offset::Offset(o as _),
            KafkaOffset::Offset(o) => Offset::OffsetTail((-o - 1) as _),
            KafkaOffset::OffsetInterval(b, _) => Offset::Offset(b as _),
            KafkaOffset::TimeInterval(b, _e) => {
                let consumer = Arc::clone(&self.stream);
                async move {
                    let consumer = consumer.lock_owned().await;
                    let r: JoinHandle<KafkaResult<_>> = spawn_blocking(move || {
                        let mut tpl_b = TopicPartitionList::new();
                        tpl_b.add_partition_offset(&topic, partition, Offset::Offset(b as _))?;
                        tpl_b = consumer.offsets_for_times(tpl_b, Duration::from_secs(1))?;
                        Ok(tpl_b.find_partition(&topic, partition).unwrap().offset())
                    });
                    r.await
                }
                .await
                .expect("")
                .map_err(|x| anyhow::Error::from(x))?
            },
        };

        tpl.add_partition_offset(&self.config.topic, partition, offset).unwrap();
        self.stream.lock().await.assign(&tpl).map_err(|x| anyhow::Error::from(x))?;
        Ok(())
    }

    async fn for_each<Fut, F>(&self, mut func: F) -> Result<(), KafcatError>
    where
        F: FnMut(Self::Message) -> Fut + Send,
        Fut: TryFuture<Ok = (), Error = KafcatError> + Send,
    {
        let stream = Arc::clone(&self.stream).lock_owned().await;
        let handler = stream.stream().timeout(if self.config.exit_on_done { Duration::from_secs(3) } else { Duration::from_secs(3600) });
        let handler = handler.map_err(|x| anyhow::Error::from(x).into()).try_for_each(|x| {
            let msg = x.detach();
            let msg = RdkafkaMessage {
                key:       msg.key().map(Vec::from).unwrap_or(vec![]),
                payload:   msg.payload().map(Vec::from).unwrap_or(vec![]),
                timestamp: msg.timestamp().to_millis().unwrap(),
            };
            (func)(msg)
        });
        handler.await
    }

    async fn stream(&self) -> Self::StreamType {
        let stream = Arc::clone(&self.stream).lock_owned().await;
        let stream2 = stream.stream();
        let stream3 = stream2.map_err(process_err);
        let stream4 = stream3.map_ok(process);
        RdkafkaStreamImpl { stream, stream2, stream3, stream4 }
    }
}

pub struct RdkafkaStreamImpl {
    stream:  OwnedMutexGuard<StreamConsumer>,
    stream2: MessageStream<'static, DefaultConsumerContext>,
    stream3: MapErr<MessageStream<'static, DefaultConsumerContext>, fn(KafkaError) -> KafcatError>,
    stream4: futures::stream::MapOk<futures::stream::MapErr<MessageStream<'static, DefaultConsumerContext>, fn(KafkaError) -> KafcatError>, for<'r> fn(BorrowedMessage<'r>) -> RdkafkaMessage>,
}
impl Stream for RdkafkaStreamImpl {
    type Item = Result<RdkafkaMessage, KafcatError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> { self.stream4.poll_next(cx) }
}
fn process_err(x: KafkaError) -> KafcatError { anyhow::Error::from(x).into() }
fn process(x: BorrowedMessage) -> RdkafkaMessage {
    let msg = x.detach();
    RdkafkaMessage {
        key:       msg.key().map(Vec::from).unwrap_or(vec![]),
        payload:   msg.payload().map(Vec::from).unwrap_or(vec![]),
        timestamp: msg.timestamp().to_millis().unwrap(),
    }
}
pub struct RdkafkaProducer {
    producer: FutureProducer,
    config:   KafkaProducerConfig,
}

#[async_trait]
impl CustomProducer for RdkafkaProducer {
    type Message = RdkafkaMessage;

    fn from_config(kafka_config: KafkaProducerConfig) -> Self
    where
        Self: Sized,
    {
        let producer = ClientConfig::new()
            .set("bootstrap.servers", &kafka_config.brokers)
            .set("message.timeout.ms", "5000")
            .create()
            .expect("Producer creation error");
        RdkafkaProducer { producer, config: kafka_config }
    }

    async fn write_one(&self, msg: Self::Message) -> Result<(), KafcatError> {
        let mut record = FutureRecord::to(&self.config.topic);
        let key = msg.get_key();
        if key.len() > 0 {
            record = record.key(key);
        }
        let payload = msg.get_payload();
        if payload.len() > 0 {
            record = record.payload(payload)
        }
        self.producer.send(record, Duration::from_secs(0)).await.map_err(|(err, _msg)| anyhow::Error::from(err))?;
        Ok(())
    }
}

pub struct RdkafkaMessage {
    key:       Vec<u8>,
    payload:   Vec<u8>,
    timestamp: i64,
}

impl CustomMessage for RdkafkaMessage {
    fn get_key(&self) -> &[u8] { &self.key }

    fn get_payload(&self) -> &[u8] { &self.payload }

    fn get_timestamp(&self) -> i64 { self.timestamp }

    fn set_key(&mut self, key: Vec<u8>) { self.key = key; }

    fn set_payload(&mut self, payload: Vec<u8>) { self.payload = payload; }

    fn set_timestamp(&mut self, timestamp: i64) { self.timestamp = timestamp; }
}
