use std::{hash::Hash, marker::PhantomData, pin::Pin, sync::Arc, time::Duration};

use futures_util::stream::{self, BoxStream};
use tower::{
    balance::p2c::Balance,
    buffer::{Buffer, BufferLayer},
    discover::Change,
    layer::{util::Stack, Layer},
    limit::RateLimit,
    retry::Retry,
    timeout::Timeout,
    Service, ServiceBuilder,
};
use vector_config::configurable_component;

pub use crate::sinks::util::service::{
    concurrency::{concurrency_is_none, Concurrency},
    health::{HealthConfig, HealthLogic, HealthService},
    map::Map,
};
use crate::{
    internal_events::OpenGauge,
    sinks::util::{
        adaptive_concurrency::{
            AdaptiveConcurrencyLimit, AdaptiveConcurrencyLimitLayer, AdaptiveConcurrencySettings,
        },
        retries::{FixedRetryPolicy, RetryLogic},
        service::map::MapLayer,
        sink::Response,
        Batch, BatchSink, Partition, PartitionBatchSink,
    },
};

mod concurrency;
mod health;
mod map;
pub mod udp;

pub type Svc<S, L> = RateLimit<AdaptiveConcurrencyLimit<Retry<FixedRetryPolicy<L>, Timeout<S>>, L>>;
pub type TowerBatchedSink<S, B, RL> = BatchSink<Svc<S, RL>, B>;
pub type TowerPartitionSink<S, B, RL, K> = PartitionBatchSink<Svc<S, RL>, B, K>;

// Distributed service types
pub type DistributedService<S, RL, HL, K, Req> = RateLimit<
    Retry<FixedRetryPolicy<RL>, Buffer<Balance<DiscoveryService<S, RL, HL, K>, Req>, Req>>,
>;
pub type DiscoveryService<S, RL, HL, K> =
    BoxStream<'static, Result<Change<K, SingleDistributedService<S, RL, HL>>, crate::Error>>;
pub type SingleDistributedService<S, RL, HL> =
    AdaptiveConcurrencyLimit<HealthService<Timeout<S>, HL>, RL>;

pub trait ServiceBuilderExt<L> {
    fn map<R1, R2, F>(self, f: F) -> ServiceBuilder<Stack<MapLayer<R1, R2>, L>>
    where
        F: Fn(R1) -> R2 + Send + Sync + 'static;

    fn settings<RL, Request>(
        self,
        settings: TowerRequestSettings,
        retry_logic: RL,
    ) -> ServiceBuilder<Stack<TowerRequestLayer<RL, Request>, L>>;
}

impl<L> ServiceBuilderExt<L> for ServiceBuilder<L> {
    fn map<R1, R2, F>(self, f: F) -> ServiceBuilder<Stack<MapLayer<R1, R2>, L>>
    where
        F: Fn(R1) -> R2 + Send + Sync + 'static,
    {
        self.layer(MapLayer::new(Arc::new(f)))
    }

    fn settings<RL, Request>(
        self,
        settings: TowerRequestSettings,
        retry_logic: RL,
    ) -> ServiceBuilder<Stack<TowerRequestLayer<RL, Request>, L>> {
        self.layer(TowerRequestLayer {
            settings,
            retry_logic,
            _pd: std::marker::PhantomData,
        })
    }
}

/// Middleware settings for outbound requests.
///
/// Various settings can be configured, such as concurrency and rate limits, timeouts, etc.
#[configurable_component]
#[derive(Clone, Copy, Debug)]
pub struct TowerRequestConfig {
    #[configurable(derived)]
    #[serde(default)]
    #[serde(skip_serializing_if = "concurrency_is_none")]
    pub concurrency: Concurrency,

    /// The maximum time a request can take before being aborted.
    ///
    /// It is highly recommended that you do not lower this value below the service’s internal timeout, as this could
    /// create orphaned requests, pile on retries, and result in duplicate data downstream.
    pub timeout_secs: Option<u64>,

    /// The time window, in seconds, used for the `rate_limit_num` option.
    pub rate_limit_duration_secs: Option<u64>,

    /// The maximum number of requests allowed within the `rate_limit_duration_secs` time window.
    pub rate_limit_num: Option<u64>,

    /// The maximum number of retries to make for failed requests.
    ///
    /// The default, for all intents and purposes, represents an infinite number of retries.
    pub retry_attempts: Option<usize>,

    /// The maximum amount of time, in seconds, to wait between retries.
    pub retry_max_duration_secs: Option<u64>,

    /// The amount of time to wait before attempting the first retry for a failed request.
    ///
    /// After the first retry has failed, the fibonacci sequence will be used to select future backoffs.
    pub retry_initial_backoff_secs: Option<u64>,

    #[configurable(derived)]
    #[serde(default)]
    pub adaptive_concurrency: AdaptiveConcurrencySettings,
}

pub const CONCURRENCY_DEFAULT: Concurrency = Concurrency::None;
pub const RATE_LIMIT_DURATION_SECONDS_DEFAULT: u64 = 1;
pub const RATE_LIMIT_NUM_DEFAULT: u64 = i64::max_value() as u64; // i64 avoids TOML deserialize issue
pub const RETRY_ATTEMPTS_DEFAULT: usize = isize::max_value() as usize; // isize avoids TOML deserialize issue
pub const RETRY_MAX_DURATION_SECONDS_DEFAULT: u64 = 3_600;
pub const RETRY_INITIAL_BACKOFF_SECONDS_DEFAULT: u64 = 1;
pub const TIMEOUT_SECONDS_DEFAULT: u64 = 60;

impl Default for TowerRequestConfig {
    fn default() -> Self {
        Self::new(CONCURRENCY_DEFAULT)
    }
}

impl TowerRequestConfig {
    pub const fn new(concurrency: Concurrency) -> Self {
        Self {
            concurrency,
            timeout_secs: Some(TIMEOUT_SECONDS_DEFAULT),
            rate_limit_duration_secs: Some(RATE_LIMIT_DURATION_SECONDS_DEFAULT),
            rate_limit_num: Some(RATE_LIMIT_NUM_DEFAULT),
            retry_attempts: Some(RETRY_ATTEMPTS_DEFAULT),
            retry_max_duration_secs: Some(RETRY_MAX_DURATION_SECONDS_DEFAULT),
            retry_initial_backoff_secs: Some(RETRY_INITIAL_BACKOFF_SECONDS_DEFAULT),
            adaptive_concurrency: AdaptiveConcurrencySettings::const_default(),
        }
    }

    pub const fn const_default() -> Self {
        Self::new(CONCURRENCY_DEFAULT)
    }

    pub const fn timeout_secs(mut self, timeout_secs: u64) -> Self {
        self.timeout_secs = Some(timeout_secs);
        self
    }

    pub const fn rate_limit_duration_secs(mut self, rate_limit_duration_secs: u64) -> Self {
        self.rate_limit_duration_secs = Some(rate_limit_duration_secs);
        self
    }

    pub const fn rate_limit_num(mut self, rate_limit_num: u64) -> Self {
        self.rate_limit_num = Some(rate_limit_num);
        self
    }

    pub const fn retry_attempts(mut self, retry_attempts: usize) -> Self {
        self.retry_attempts = Some(retry_attempts);
        self
    }

    pub const fn retry_max_duration_secs(mut self, retry_max_duration_secs: u64) -> Self {
        self.retry_max_duration_secs = Some(retry_max_duration_secs);
        self
    }

    pub const fn retry_initial_backoff_secs(mut self, retry_initial_backoff_secs: u64) -> Self {
        self.retry_initial_backoff_secs = Some(retry_initial_backoff_secs);
        self
    }

    pub fn unwrap_with(&self, defaults: &Self) -> TowerRequestSettings {
        TowerRequestSettings {
            concurrency: self.concurrency.parse_concurrency(defaults.concurrency),
            timeout: Duration::from_secs(
                self.timeout_secs
                    .or(defaults.timeout_secs)
                    .unwrap_or(TIMEOUT_SECONDS_DEFAULT),
            ),
            rate_limit_duration: Duration::from_secs(
                self.rate_limit_duration_secs
                    .or(defaults.rate_limit_duration_secs)
                    .unwrap_or(RATE_LIMIT_DURATION_SECONDS_DEFAULT),
            ),
            rate_limit_num: self
                .rate_limit_num
                .or(defaults.rate_limit_num)
                .unwrap_or(RATE_LIMIT_NUM_DEFAULT),
            retry_attempts: self
                .retry_attempts
                .or(defaults.retry_attempts)
                .unwrap_or(RETRY_ATTEMPTS_DEFAULT),
            retry_max_duration_secs: Duration::from_secs(
                self.retry_max_duration_secs
                    .or(defaults.retry_max_duration_secs)
                    .unwrap_or(RETRY_MAX_DURATION_SECONDS_DEFAULT),
            ),
            retry_initial_backoff_secs: Duration::from_secs(
                self.retry_initial_backoff_secs
                    .or(defaults.retry_initial_backoff_secs)
                    .unwrap_or(RETRY_INITIAL_BACKOFF_SECONDS_DEFAULT),
            ),
            adaptive_concurrency: self.adaptive_concurrency,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TowerRequestSettings {
    pub concurrency: Option<usize>,
    pub timeout: Duration,
    pub rate_limit_duration: Duration,
    pub rate_limit_num: u64,
    pub retry_attempts: usize,
    pub retry_max_duration_secs: Duration,
    pub retry_initial_backoff_secs: Duration,
    pub adaptive_concurrency: AdaptiveConcurrencySettings,
}

impl TowerRequestSettings {
    pub const fn retry_policy<L: RetryLogic>(&self, logic: L) -> FixedRetryPolicy<L> {
        FixedRetryPolicy::new(
            self.retry_attempts,
            self.retry_initial_backoff_secs,
            self.retry_max_duration_secs,
            logic,
        )
    }

    /// Note: This has been deprecated, please do not use when creating new Sinks.
    pub fn partition_sink<B, RL, S, K>(
        &self,
        retry_logic: RL,
        service: S,
        batch: B,
        batch_timeout: Duration,
    ) -> TowerPartitionSink<S, B, RL, K>
    where
        RL: RetryLogic<Response = S::Response>,
        S: Service<B::Output> + Clone + Send + 'static,
        S::Error: Into<crate::Error> + Send + Sync + 'static,
        S::Response: Send + Response,
        S::Future: Send + 'static,
        B: Batch,
        B::Input: Partition<K>,
        B::Output: Send + Clone + 'static,
        K: Hash + Eq + Clone + Send + 'static,
    {
        let service = ServiceBuilder::new()
            .settings(self.clone(), retry_logic)
            .service(service);
        PartitionBatchSink::new(service, batch, batch_timeout)
    }

    /// Note: This has been deprecated, please do not use when creating new Sinks.
    pub fn batch_sink<B, RL, S>(
        &self,
        retry_logic: RL,
        service: S,
        batch: B,
        batch_timeout: Duration,
    ) -> TowerBatchedSink<S, B, RL>
    where
        RL: RetryLogic<Response = S::Response>,
        S: Service<B::Output> + Clone + Send + 'static,
        S::Error: Into<crate::Error> + Send + Sync + 'static,
        S::Response: Send + Response,
        S::Future: Send + 'static,
        B: Batch,
        B::Output: Send + Clone + 'static,
    {
        let service = ServiceBuilder::new()
            .settings(self.clone(), retry_logic)
            .service(service);
        BatchSink::new(service, batch, batch_timeout)
    }

    /// Distributes requests to services [(Endpoint, service, healthcheck)]
    pub fn distributed_service<Req, RL, HL, S>(
        self,
        retry_logic: RL,
        services: Vec<(String, S)>,
        health_config: HealthConfig,
        health_logic: HL,
    ) -> DistributedService<S, RL, HL, usize, Req>
    where
        Req: Clone + Send + 'static,
        RL: RetryLogic<Response = S::Response>,
        HL: HealthLogic<Response = S::Response, Error = crate::Error>,
        S: Service<Req> + Clone + Send + 'static,
        S::Error: Into<crate::Error> + Send + Sync + 'static,
        S::Response: Send,
        S::Future: Send + 'static,
    {
        let policy = self.retry_policy(retry_logic.clone());
        let settings = self.clone();

        // Build services
        let open = OpenGauge::new();
        let max_concurrency = services.len() * AdaptiveConcurrencySettings::max_concurrency();
        let services = services
            .into_iter()
            .map(|(endpoint, inner)| {
                // Build individual service
                ServiceBuilder::new()
                    .layer(AdaptiveConcurrencyLimitLayer::new(
                        settings.concurrency,
                        settings.adaptive_concurrency,
                        retry_logic.clone(),
                    ))
                    .service(
                        health_config.build(
                            health_logic.clone(),
                            ServiceBuilder::new()
                                .timeout(settings.timeout)
                                .service(inner),
                            open.clone(),
                            endpoint,
                        ), // NOTE: there is a version conflict for crate `tracing` between `tracing_tower` crate
                           // and Vector. Once that is resolved, this can be used instead of passing endpoint everywhere.
                           // .trace_service(|_| info_span!("endpoint", %endpoint)),
                    )
            })
            .enumerate()
            .map(|(i, service)| Ok(Change::Insert(i, service)))
            .collect::<Vec<_>>();

        // Build sink service
        ServiceBuilder::new()
            .rate_limit(self.rate_limit_num, self.rate_limit_duration)
            .retry(policy)
            .layer(BufferLayer::new(max_concurrency))
            .service(Balance::new(Box::pin(stream::iter(services)) as Pin<Box<_>>))
    }
}

#[derive(Debug, Clone)]
pub struct TowerRequestLayer<L, Request> {
    settings: TowerRequestSettings,
    retry_logic: L,
    _pd: PhantomData<Request>,
}

impl<S, RL, Request> Layer<S> for TowerRequestLayer<RL, Request>
where
    S: Service<Request> + Send + 'static,
    S::Response: Send + 'static,
    S::Error: Into<crate::Error> + Send + Sync + 'static,
    S::Future: Send + 'static,
    RL: RetryLogic<Response = S::Response> + Send + 'static,
    Request: Clone + Send + 'static,
{
    type Service = Svc<S, RL>;

    fn layer(&self, inner: S) -> Self::Service {
        let policy = self.settings.retry_policy(self.retry_logic.clone());
        ServiceBuilder::new()
            .rate_limit(
                self.settings.rate_limit_num,
                self.settings.rate_limit_duration,
            )
            .layer(AdaptiveConcurrencyLimitLayer::new(
                self.settings.concurrency,
                self.settings.adaptive_concurrency,
                self.retry_logic.clone(),
            ))
            .retry(policy)
            .timeout(self.settings.timeout)
            .service(inner)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicBool, Ordering::AcqRel},
        Arc, Mutex,
    };

    use futures::{future, stream, FutureExt, SinkExt, StreamExt};
    use tokio::time::Duration;

    use super::*;
    use crate::sinks::util::{
        retries::{RetryAction, RetryLogic},
        BatchSettings, EncodedEvent, PartitionBuffer, PartitionInnerBuffer, VecBuffer,
    };

    const TIMEOUT: Duration = Duration::from_secs(10);

    #[test]
    fn concurrency_param_works() {
        let cfg = TowerRequestConfig::default();
        let toml = toml::to_string(&cfg).unwrap();
        toml::from_str::<TowerRequestConfig>(&toml).expect("Default config failed");

        let cfg = toml::from_str::<TowerRequestConfig>("").expect("Empty config failed");
        assert_eq!(cfg.concurrency, Concurrency::None);

        let cfg = toml::from_str::<TowerRequestConfig>("concurrency = 10")
            .expect("Fixed concurrency failed");
        assert_eq!(cfg.concurrency, Concurrency::Fixed(10));

        let cfg = toml::from_str::<TowerRequestConfig>(r#"concurrency = "adaptive""#)
            .expect("Adaptive concurrency setting failed");
        assert_eq!(cfg.concurrency, Concurrency::Adaptive);

        toml::from_str::<TowerRequestConfig>(r#"concurrency = "broken""#)
            .expect_err("Invalid concurrency setting didn't fail");

        toml::from_str::<TowerRequestConfig>(r#"concurrency = 0"#)
            .expect_err("Invalid concurrency setting didn't fail on zero");

        toml::from_str::<TowerRequestConfig>(r#"concurrency = -9"#)
            .expect_err("Invalid concurrency setting didn't fail on negative number");
    }

    #[test]
    fn config_merging_defaults_concurrency_to_none_if_unset() {
        let cfg = TowerRequestConfig::default().unwrap_with(&TowerRequestConfig::default());

        assert_eq!(cfg.concurrency, None);
    }

    #[tokio::test]
    async fn partition_sink_retry_concurrency() {
        let cfg = TowerRequestConfig {
            concurrency: Concurrency::Fixed(1),
            ..TowerRequestConfig::default()
        };
        let settings = cfg.unwrap_with(&TowerRequestConfig::default());

        let sent_requests = Arc::new(Mutex::new(Vec::new()));

        let svc = {
            let sent_requests = Arc::clone(&sent_requests);
            let delay = Arc::new(AtomicBool::new(true));
            tower::service_fn(move |req: PartitionInnerBuffer<_, _>| {
                let (req, _) = req.into_parts();
                if delay.swap(false, AcqRel) {
                    // Error on first request
                    future::err::<(), _>(std::io::Error::new(std::io::ErrorKind::Other, "")).boxed()
                } else {
                    sent_requests.lock().unwrap().push(req);
                    future::ok::<_, std::io::Error>(()).boxed()
                }
            })
        };

        let mut batch_settings = BatchSettings::default();
        batch_settings.size.bytes = 9999;
        batch_settings.size.events = 10;

        let mut sink = settings.partition_sink(
            RetryAlways,
            svc,
            PartitionBuffer::new(VecBuffer::new(batch_settings.size)),
            TIMEOUT,
        );
        sink.ordered();

        let input = (0..20).into_iter().map(|i| PartitionInnerBuffer::new(i, 0));
        sink.sink_map_err(drop)
            .send_all(&mut stream::iter(input).map(|item| Ok(EncodedEvent::new(item, 0))))
            .await
            .unwrap();

        let output = sent_requests.lock().unwrap();
        assert_eq!(
            &*output,
            &vec![
                (0..10).into_iter().collect::<Vec<_>>(),
                (10..20).into_iter().collect::<Vec<_>>(),
            ]
        );
    }

    #[derive(Clone, Debug, Copy)]
    struct RetryAlways;

    impl RetryLogic for RetryAlways {
        type Error = std::io::Error;
        type Response = ();

        fn is_retriable_error(&self, _: &Self::Error) -> bool {
            true
        }

        fn should_retry_response(&self, _response: &Self::Response) -> RetryAction {
            // Treat the default as the request is successful
            RetryAction::Successful
        }
    }
}
