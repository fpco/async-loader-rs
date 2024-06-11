use std::{
    collections::HashMap,
    fmt::{Debug, Display},
    sync::Arc,
    time::{Duration, Instant},
};

use axum::{
    body::Body,
    http::{HeaderValue, Response},
    response::IntoResponse,
    Json,
};
use parking_lot::{Mutex, RwLock};
use reqwest::{header::CACHE_CONTROL, StatusCode};
use tokio::sync::broadcast;

/// Provides access to a cached value with automatic reload.
pub struct AsyncLoader<Loader: CacheLoader> {
    pub inner: Arc<Inner<Loader>>,
}

// Manual impl to avoid a Clone bound on Loader
impl<Loader: CacheLoader> Clone for AsyncLoader<Loader> {
    fn clone(&self) -> Self {
        AsyncLoader {
            inner: self.inner.clone(),
        }
    }
}

type ArcInnerMut<Loader> = Arc<Mutex<InnerMut<Loader>>>;

pub struct Inner<Loader: CacheLoader> {
    /// Mutable pieces of the inner storage, per key
    per_key: RwLock<HashMap<Loader::Key, ArcInnerMut<Loader>>>,
    pub config: AsyncLoaderConfig,
    /// Actual value used for loading new values
    loader: Loader,
}

#[derive(Debug, serde::Serialize)]
pub struct LoaderMetric {
    length: usize,
    capacity: usize,
}

impl<Loader: CacheLoader> Inner<Loader> {
    pub fn metrics(&self) -> LoaderMetric {
        let key = self.per_key.read();
        let length = key.len();
        let capacity = key.capacity();
        LoaderMetric { length, capacity }
    }
}

pub struct AsyncLoaderBuilder {
    /// How long after we load a value should we trigger a reload?
    ///
    /// Default: 1 minute
    pub reload_delay: Option<Duration>,
    /// How long after the reload delay should we consider a value too old and refuse to return it?
    ///
    /// Default: same as reload_delay
    pub stale_delay: StaleDelay,
    /// Should we store cache ?
    ///
    /// Default: true
    pub store_cache: bool,
    /// Should we disable cache loader ?
    ///
    /// Default: false
    pub disable_loader: bool,
}

#[derive(Default)]
pub enum StaleDelay {
    /// Default will be twice the [AsyncLoaderBuilder::reload_delay]
    #[default]
    Default,
    Duration(Duration),
    /// Allows last computed stale value in case of an error during
    /// the load step.
    AllowStale,
}

pub struct AsyncLoaderConfig {
    reload_delay: Duration,
    /// None if we never go stale
    reload_and_stale_delay: Option<Duration>,
    /// Store cache
    store_cache: bool,
    /// Disable cache
    disable_loader: bool,
}

impl From<&AsyncLoaderBuilder> for AsyncLoaderConfig {
    fn from(value: &AsyncLoaderBuilder) -> Self {
        let reload_delay = value
            .reload_delay
            .unwrap_or_else(|| Duration::from_secs(60));
        AsyncLoaderConfig {
            reload_delay,
            reload_and_stale_delay: match value.stale_delay {
                StaleDelay::Default => Some(reload_delay + reload_delay),
                StaleDelay::Duration(stale_delay) => Some(reload_delay + stale_delay),
                StaleDelay::AllowStale => None,
            },
            store_cache: value.store_cache,
            disable_loader: value.disable_loader,
        }
    }
}

struct InnerMut<Loader: CacheLoader> {
    /// The latest value loaded, if available.
    latest: Option<Latest<Loader>>,
    /// A receiver to subscribe to the newest update.
    ///
    /// Only set if there is currently an update occurring.
    broadcast: Option<broadcast::Sender<ValueForRender<Loader>>>,
}

pub struct Latest<Loader: CacheLoader> {
    /// When was this value loaded? Used for cache calculations
    pub loaded: Instant,
    pub value: Arc<Loader::Output>,
}

impl<Loader: CacheLoader> Clone for Latest<Loader> {
    fn clone(&self) -> Self {
        Latest {
            loaded: self.loaded,
            value: self.value.clone(),
        }
    }
}

/// Can we use the latest value?
enum UseLatest<Loader: CacheLoader> {
    /// No value found, or the value found was too old
    NoValue,
    /// Value found, but it's stale and shouldn't be used unless explicitly looking for stale values
    StaleValue(Latest<Loader>),
    /// We can use a value, but we should start reloading for a fresher cache
    UseAndReload(ValueForRender<Loader>),
    /// Value is recent enough, no reloading required
    UseNoReload(ValueForRender<Loader>),
}

impl<Loader: CacheLoader> Debug for UseLatest<Loader> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UseLatest::NoValue => write!(f, "NoValue"),
            UseLatest::StaleValue(_) => write!(f, "StaleValue"),
            UseLatest::UseAndReload(_) => write!(f, "UseAndReload"),
            UseLatest::UseNoReload(_) => write!(f, "UseNoReload"),
        }
    }
}

pub enum ValueForRender<Loader: CacheLoader> {
    /// Value was loaded successfully
    Success {
        value: Arc<Loader::Output>,
        loaded: Instant,
    },
    /// Error while loading the value
    Error {
        message: Arc<str>,
        status: StatusCode,
        /// If there's a cached value available that's considered stale, provide it here.
        stale_cache: Option<Latest<Loader>>,
    },
}

/// An error type that wraps up a [axum::Response].
///
/// If you return this as an error value from [CacheLoader::load],
/// it will be used as the error response.
#[derive(Clone, Debug)]
pub struct ErrorResponse {
    pub status: StatusCode,
    pub message: String,
}

impl std::error::Error for ErrorResponse {}

impl Display for ErrorResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl<Loader: CacheLoader> Clone for ValueForRender<Loader> {
    fn clone(&self) -> Self {
        match self {
            ValueForRender::Success { value, loaded } => ValueForRender::Success {
                value: value.clone(),
                loaded: *loaded,
            },
            ValueForRender::Error {
                message,
                status,
                stale_cache,
            } => ValueForRender::Error {
                message: message.clone(),
                status: *status,
                stale_cache: stale_cache.clone(),
            },
        }
    }
}

impl<Loader: CacheLoader> ValueForRender<Loader> {
    /// Convert into an axum Response
    ///
    /// Handles all the details of cache headers, status codes, and content-type.
    pub fn into_response(self, config: &AsyncLoaderConfig) -> axum::response::Response {
        match self {
            ValueForRender::Success { value, loaded } => {
                let mut res = Loader::into_response(&*value);
                if let Some(cache_time) = config.reload_delay.checked_sub(loaded.elapsed()) {
                    if let Ok(value) =
                        HeaderValue::from_str(&format!("public, max-age={}", cache_time.as_secs()))
                    {
                        res.headers_mut().insert(CACHE_CONTROL, value);
                    }
                }
                res
            }
            ValueForRender::Error {
                message,
                status,
                stale_cache: _,
            } => (status, message.as_ref().to_owned()).into_response(),
        }
    }
}

impl<Loader: CacheLoader> Latest<Loader> {
    /// Check whether we're allowed to use this value given how much time has passed.
    fn can_use(&self, config: &AsyncLoaderConfig) -> UseLatest<Loader> {
        let age = self.loaded.elapsed();

        if let Some(delay) = config.reload_and_stale_delay {
            if age > delay {
                // Too old, return no value or the stale value
                return UseLatest::StaleValue(self.clone());
            }
        }

        // We can use the value, but determine whether we need to refresh the cache
        if age > config.reload_delay {
            UseLatest::UseAndReload(ValueForRender::Success {
                value: self.value.clone(),
                loaded: self.loaded,
            })
        } else {
            UseLatest::UseNoReload(ValueForRender::Success {
                value: self.value.clone(),
                loaded: self.loaded,
            })
        }
    }
}

/// Something that can load a value for the cacher.
#[axum::async_trait]
pub trait CacheLoader: Send + Sync + 'static {
    type Key: std::hash::Hash + Eq + Clone + Send + Sync + 'static;
    type Output: serde::Serialize + Send + Sync + 'static;

    async fn load(&self, key: Self::Key) -> anyhow::Result<Self::Output>;

    /// Convert the [Self::Output] value into an axum [Response].
    ///
    /// By default, this will convert to a JSON representation.
    fn into_response(output: &Self::Output) -> Response<Body> {
        Json(output).into_response()
    }
}

impl AsyncLoaderBuilder {
    pub fn build<Loader: CacheLoader>(&self, loader: Loader) -> AsyncLoader<Loader> {
        AsyncLoader {
            inner: Arc::new(Inner {
                per_key: RwLock::new(HashMap::new()),
                config: self.into(),
                loader,
            }),
        }
    }
}

enum BroadcastOrValue<Loader: CacheLoader> {
    Broadcast {
        broadcast: broadcast::Receiver<ValueForRender<Loader>>,
        stale_cache: Option<Latest<Loader>>,
    },
    Value(ValueForRender<Loader>),
}

impl<Loader: CacheLoader> Debug for BroadcastOrValue<Loader> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Broadcast {
                broadcast: _,
                stale_cache: _,
            } => write!(f, "Broadcast"),
            Self::Value(_) => write!(f, "value"),
        }
    }
}

impl<Loader: CacheLoader> AsyncLoader<Loader> {
    /// Get a new response from this cacher.
    ///
    /// Response may be an error response.
    pub async fn get_response(&self, key: Loader::Key) -> axum::response::Response {
        self.get_value(key).await.into_response(&self.inner.config)
    }

    /// Get a new response from this cacher, but allows stale value.
    ///
    /// Response may be an error response.
    pub async fn get_response_allow_stale(&self, key: Loader::Key) -> axum::response::Response {
        self.get_value_allow_stale(key)
            .await
            .into_response(&self.inner.config)
    }

    /// Get the underlying value and how old it is.
    pub async fn get_value(&self, key: Loader::Key) -> ValueForRender<Loader> {
        let config = self.get_cacher_config();

        // Get either the current value or a channel to wait for values on.
        let broadcast_or_value = if config.disable_loader {
            self.new_broadcast(key)
        } else {
            self.get_broadcast_or_value(key, false)
        };
        match broadcast_or_value {
            // Need to wait for a new value to be broadcast
            BroadcastOrValue::Broadcast {
                mut broadcast,
                stale_cache,
            } => match broadcast.recv().await {
                // Success!
                Ok(value) => value,
                // This should never happen, it indicates a bug in the code. It
                // may occur if the loader itself panics.
                Err(err) => {
                    tracing::error!("AsyncLoader::get_response: impossible Err received: {err:?}");
                    ValueForRender::Error {
                        message: "Internal error detected: broadcast channel is gone".into(),
                        status: StatusCode::INTERNAL_SERVER_ERROR,
                        stale_cache,
                    }
                }
            },
            // Value was already available
            BroadcastOrValue::Value(value) => value,
        }
    }

    /// Get the underlying value, but fetch it newly this time
    pub async fn get_latest_value(&self, key: Loader::Key) -> ValueForRender<Loader> {
        let config = self.get_cacher_config();

        // Get either the current value or a channel to wait for values on.
        let broadcast_or_value = if config.disable_loader {
            self.new_broadcast(key)
        } else {
            self.get_broadcast_or_value(key, true)
        };
        match broadcast_or_value {
            // Need to wait for a new value to be broadcast
            BroadcastOrValue::Broadcast {
                mut broadcast,
                stale_cache,
            } => match broadcast.recv().await {
                // Success!
                Ok(value) => value,
                // This should never happen, it indicates a bug in the code. It
                // may occur if the loader itself panics.
                Err(err) => {
                    tracing::error!("AsyncLoader::get_response: impossible Err received: {err:?}");
                    ValueForRender::Error {
                        message: "Internal error detected: broadcast channel is gone".into(),
                        status: StatusCode::INTERNAL_SERVER_ERROR,
                        stale_cache,
                    }
                }
            },
            // Value was already available
            BroadcastOrValue::Value(value) => value,
        }
    }

    /// Get cacher config
    pub fn get_cacher_config(&self) -> &AsyncLoaderConfig {
        &self.inner.config
    }

    /// Get a value without paying attention to whether it's stale
    ///
    /// Returns an error if there is no cached value and loading fails.
    pub async fn get_value_allow_stale(&self, key: Loader::Key) -> ValueForRender<Loader> {
        let result = self.get_value(key).await;
        match result {
            ValueForRender::Success { .. } => result,
            ValueForRender::Error {
                stale_cache: Some(stale_cache),
                ..
            } => ValueForRender::Success {
                value: stale_cache.value,
                loaded: stale_cache.loaded,
            },
            ValueForRender::Error { .. } => result,
        }
    }

    /// Builds on [Self::get_value_allow_stale] by returning a Result.
    pub async fn get_value_or_error(
        &self,
        key: Loader::Key,
    ) -> anyhow::Result<Arc<Loader::Output>> {
        match self.get_value_allow_stale(key).await {
            ValueForRender::Success { value, loaded: _ } => Ok(value),
            ValueForRender::Error {
                message,
                status: _,
                stale_cache,
            } => match stale_cache {
                Some(latest) => Ok(latest.value),
                None => Err(anyhow::anyhow!("{message}")),
            },
        }
    }

    fn get_mutex_by_key(&self, key: &Loader::Key) -> Arc<Mutex<InnerMut<Loader>>> {
        {
            // First: optimistically try to get the existing value with just a read guard
            let read_guard = self.inner.per_key.read();

            if let Some(value) = read_guard.get(key) {
                return value.clone();
            }
        }

        // That failed, we've dropped the read guard, now try again with a write guard
        let mut write_guard = self.inner.per_key.write();

        if let Some(value) = write_guard.get(key) {
            return value.clone();
        }

        // Write guard also didn't have it, so now we fill in the structure.
        let value = Arc::new(Mutex::new(InnerMut {
            latest: None,
            broadcast: None,
        }));

        write_guard.insert(key.clone(), value.clone());
        value
    }

    fn new_broadcast(&self, key: Loader::Key) -> BroadcastOrValue<Loader> {
        let (sender, receiver) = broadcast::channel(1);
        tokio::task::spawn(self.clone().do_update(sender, key, None));
        BroadcastOrValue::Broadcast {
            broadcast: receiver,
            stale_cache: None,
        }
    }

    fn get_broadcast_or_value(
        &self,
        key: Loader::Key,
        force_reload: bool,
    ) -> BroadcastOrValue<Loader> {
        // Danger Will Robinson! We only consider it safe to lock here based on
        // the rest of this method performing no long-lived blocking actions.
        let mutex_by_key = self.get_mutex_by_key(&key);
        let mut guard = mutex_by_key.lock();

        // Check if the latest value can be used or not
        let use_latest = guard.latest.as_ref().map_or(UseLatest::NoValue, |latest| {
            latest.can_use(&self.inner.config)
        });
        let use_latest = if force_reload {
            UseLatest::NoValue
        } else {
            use_latest
        };

        let (value, stale_cache) = match use_latest {
            // Need to start a new loader task, _and_ need to wait for it
            UseLatest::NoValue => (None, None),
            // Need to start a new loader task, and can only provide a stale value if loading fails
            UseLatest::StaleValue(value) => (None, Some(value)),
            // Need a loader task, but use this value
            UseLatest::UseAndReload(value) => {
                // Future updates might fail, so it is essential that
                // we propagate the proper stale_cache value here
                let stale_cache = match &value {
                    ValueForRender::Success { value, loaded } => Some(Latest {
                        value: value.clone(),
                        loaded: *loaded,
                    }),
                    ValueForRender::Error {
                        message: _,
                        status: _,
                        stale_cache,
                    } => stale_cache.clone(),
                };
                (Some(value), stale_cache)
            }
            // Need a loader task, but afterwards just return the value
            UseLatest::UseNoReload(value) => return BroadcastOrValue::Value(value),
        };

        // Since we got this far, we need a loader task. Check if one is already
        // running. Note the interplay here with locking the mutex and then checking
        // values, versus the do_update method, and comments in that part of the code.
        match &guard.broadcast {
            // Already have a task running, either return the value or subscribe
            // for an update.
            Some(broadcast) => match value {
                Some(value) => BroadcastOrValue::Value(value),
                None => BroadcastOrValue::Broadcast {
                    broadcast: broadcast.subscribe(),
                    stale_cache,
                },
            },
            // No task running, so spawn it and track it inside our mutex.
            None => {
                // We're only ever going to broadcast a single message, so 1 is fine.
                let (sender, receiver) = broadcast::channel(1);
                guard.broadcast = Some(sender.clone());
                tokio::task::spawn(self.clone().do_update(sender, key, stale_cache.clone()));
                BroadcastOrValue::Broadcast {
                    broadcast: receiver,
                    stale_cache,
                }
            }
        }
    }

    /// Background update
    async fn do_update(
        self,
        sender: broadcast::Sender<ValueForRender<Loader>>,
        key: Loader::Key,
        stale_cache: Option<Latest<Loader>>,
    ) {
        // Run the loader and then render to JSON.
        let res = self.inner.loader.load(key.clone()).await.map(Arc::new);
        let config = self.get_cacher_config();

        {
            // New block to avoid holding the guard for too long.
            let mutex_by_key = self.get_mutex_by_key(&key);
            let mut guard = mutex_by_key.lock();

            // First we wipe out the old broadcast. We do this before
            // broadcasting the updates to ensure that, if a task gets a Receiver out of
            // get_broadcast_or_value, it's guaranteed to receive the message below.
            guard.broadcast = None;

            // And if the load was successful, update the cache.
            if let Ok(value) = &res {
                guard.latest = Some(Latest {
                    loaded: Instant::now(),
                    value: value.clone(),
                })
            }
        }

        if !config.store_cache {
            // If we are not storing the cache, then take a write lock
            // and remove the cached item
            let mut write_guard = self.inner.per_key.write();
            write_guard.remove(&key);
        }

        // Convert into a ValueForRender form.
        let value = match res {
            Ok(value) => ValueForRender::Success {
                value,
                loaded: Instant::now(),
            },
            Err(err) => {
                tracing::error!("AsyncLoader::do_update: {err:?}");
                let (message, status) = match err.downcast() {
                    Ok(ErrorResponse { status, message }) => (message, status),
                    Err(err) => (err.to_string(), StatusCode::INTERNAL_SERVER_ERROR),
                };
                ValueForRender::Error {
                    message: message.into(),
                    status,
                    stale_cache,
                }
            }
        };

        // Update any tasks that are waiting for this broadcaster.
        // We ignore any errors from the send, since it's fine if there's no one
        // waiting for our response.
        let _ = sender.send(value);
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Mutex, time::Duration};

    use anyhow::anyhow;
    use axum::async_trait;
    use tokio::task::JoinSet;

    use super::{AsyncLoader, AsyncLoaderBuilder, CacheLoader, StaleDelay, ValueForRender};

    struct MyData {
        num: u8,
        failures: u8,
    }

    pub struct TestLoader {
        num: Mutex<MyData>,
    }

    impl TestLoader {
        pub fn build() -> AsyncLoader<Self> {
            AsyncLoaderBuilder {
                reload_delay: Some(Duration::from_secs(2)),
                stale_delay: StaleDelay::Duration(Duration::from_secs(1)),
                store_cache: true,
                disable_loader: false,
            }
            .build(TestLoader {
                num: Mutex::new(MyData {
                    num: 0,
                    failures: 0,
                }),
            })
        }
        pub fn build_stale() -> AsyncLoader<Self> {
            AsyncLoaderBuilder {
                reload_delay: Some(Duration::from_secs(1)),
                stale_delay: StaleDelay::AllowStale,
                store_cache: true,
                disable_loader: false,
            }
            .build(TestLoader {
                num: Mutex::new(MyData {
                    num: 0,
                    failures: 0,
                }),
            })
        }
        pub fn build_without_cache() -> AsyncLoader<Self> {
            AsyncLoaderBuilder {
                reload_delay: Some(Duration::from_secs(1)),
                stale_delay: StaleDelay::AllowStale,
                store_cache: false,
                disable_loader: false,
            }
            .build(TestLoader {
                num: Mutex::new(MyData {
                    num: 0,
                    failures: 0,
                }),
            })
        }
        pub fn build_with_no_loader() -> AsyncLoader<Self> {
            AsyncLoaderBuilder {
                reload_delay: Some(Duration::from_secs(2)),
                stale_delay: StaleDelay::Duration(Duration::from_secs(1)),
                store_cache: true,
                disable_loader: true,
            }
            .build(TestLoader {
                num: Mutex::new(MyData {
                    num: 0,
                    failures: 0,
                }),
            })
        }
    }

    impl<Loader: CacheLoader> ValueForRender<Loader> {
        fn is_success(&self) -> bool {
            match self {
                ValueForRender::Success { .. } => true,
                ValueForRender::Error { .. } => false,
            }
        }

        fn get_success_value(&self) -> &Loader::Output {
            match self {
                ValueForRender::Success { value, loaded: _ } => value,
                ValueForRender::Error { .. } => panic!("Cannot get a value from error"),
            }
        }
    }

    #[async_trait]
    impl CacheLoader for TestLoader {
        type Key = ();
        type Output = u8;

        async fn load(&self, (): ()) -> anyhow::Result<u8> {
            let mut guard = self.num.lock().unwrap();
            if guard.num == 0 || guard.failures >= 1 {
                guard.num += 1;
                return Ok(guard.num);
            } else {
                guard.failures += 1;
                Err(anyhow!("Falied executing load"))
            }
        }
    }

    #[tokio::test]
    async fn test_reload_happens() {
        let cacher = TestLoader::build();
        let item = cacher.get_value(()).await;
        assert!(item.is_success());

        // This tests the UseNoReload case
        tokio::time::sleep(Duration::from_secs(1)).await;
        let item = cacher.get_value(()).await;
        assert!(item.is_success());

        // This tests the UseAndReload case, but the first attempt will be a failure
        tokio::time::sleep(Duration::from_secs(1)).await;
        let item = cacher.get_value(()).await;
        assert!(!item.is_success());

        // Let 1 seconds pass to make the stale value invalid and this
        // time we will get fresh data
        tokio::time::sleep(Duration::from_secs(1)).await;
        let item = cacher.get_value(()).await;
        assert!(item.is_success());
        match item {
            ValueForRender::Success { value, loaded: _ } => {
                assert_eq!(*value, 2, "Got new value in the reload");
            }
            ValueForRender::Error { .. } => panic!("Retry would have succedded"),
        }

        // Let 2 seconds pass so that a fresh value is tried now
        tokio::time::sleep(Duration::from_secs(2)).await;
        let item = cacher.get_value(()).await;
        assert!(item.is_success());
        match item {
            ValueForRender::Success { value, loaded: _ } => {
                assert_eq!(*value, 3, "Got stale value in the reload");
            }
            ValueForRender::Error { .. } => panic!("Retry would have succedded"),
        }
    }

    #[tokio::test]
    async fn test_staleness() {
        let cacher = TestLoader::build_stale();
        let item = cacher.get_value_allow_stale(()).await;
        assert_eq!(item.get_success_value(), &1);

        tokio::time::sleep(Duration::from_secs(2)).await;
        let item = cacher.get_value_allow_stale(()).await;
        assert_eq!(*item.get_success_value(), 1);

        tokio::time::sleep(Duration::from_secs(2)).await;
        let item = cacher.get_value_allow_stale(()).await;
        assert_eq!(*item.get_success_value(), 2);

        tokio::time::sleep(Duration::from_secs(2)).await;
        let item = cacher.get_value_allow_stale(()).await;
        assert_eq!(*item.get_success_value(), 3);
    }

    #[tokio::test]
    async fn test_staleness_in_get_value() {
        let cacher = TestLoader::build_stale();
        let item = cacher.get_value(()).await;
        assert_eq!(item.get_success_value(), &1);

        tokio::time::sleep(Duration::from_secs(2)).await;
        let item = cacher.get_value(()).await;
        assert!(!item.is_success());

        tokio::time::sleep(Duration::from_secs(2)).await;
        let item = cacher.get_value(()).await;
        assert_eq!(*item.get_success_value(), 2);
    }

    #[tokio::test]
    async fn test_force_reload() {
        let cacher = TestLoader::build();
        let item = cacher.get_value(()).await;
        assert_eq!(item.get_success_value(), &1);

        let item = cacher.get_latest_value(()).await;
        assert!(!item.is_success());

        let item = cacher.get_latest_value(()).await;
        assert_eq!(*item.get_success_value(), 2);

        let item = cacher.get_latest_value(()).await;
        assert_eq!(*item.get_success_value(), 3);
    }

    #[tokio::test]
    async fn test_without_store() {
        async fn test_task(cacher: AsyncLoader<TestLoader>) -> String {
            let result = cacher.get_value(()).await;
            match result {
                ValueForRender::Success { value, .. } => value.to_string(),
                ValueForRender::Error { .. } => "error".to_owned(),
            }
        }

        let cacher = TestLoader::build_without_cache();

        let item = cacher.get_value(()).await;
        assert_eq!(item.get_success_value(), &1);

        let item = cacher.get_value(()).await;
        assert!(!item.is_success());

        let mut set = JoinSet::new();
        for _ in 0..5 {
            let cacher = cacher.clone();
            set.spawn(test_task(cacher));
        }

        // Checks that even with the burst of request we get the value
        // 2.  This confirms that all the five task didn't lead
        // to computation of new value.
        let mut index = 0;
        while let Some(res) = set.join_next().await {
            assert_eq!(res.unwrap(), "2".to_owned());
            index += 1;
        }

        assert_eq!(index, 5, "5 tokio tasks executed");

        tokio::time::sleep(Duration::from_secs(2)).await;
        let item = cacher.get_value(()).await;
        assert_eq!(*item.get_success_value(), 3);
    }

    #[tokio::test]
    async fn test_with_store_disable_loader() {
        async fn test_task(cacher: AsyncLoader<TestLoader>) -> String {
            let result = cacher.get_value(()).await;
            match result {
                ValueForRender::Success { value, .. } => value.to_string(),
                ValueForRender::Error { .. } => "error".to_owned(),
            }
        }

        let cacher = TestLoader::build_with_no_loader();

        let item = cacher.get_value(()).await;
        assert_eq!(item.get_success_value(), &1);

        let item = cacher.get_value(()).await;
        assert!(!item.is_success());

        let mut set = JoinSet::new();
        for _ in 0..5 {
            let cacher = cacher.clone();
            set.spawn(test_task(cacher));
        }

        let mut index = 0;
        let possible_result = vec![
            "2".to_owned(),
            "3".to_owned(),
            "4".to_owned(),
            "5".to_owned(),
            "6".to_owned(),
        ];
        let mut actual_result = vec![];
        while let Some(res) = set.join_next().await {
            actual_result.push(res.unwrap());
            index += 1;
        }
        actual_result.sort();
        assert_eq!(actual_result, possible_result, "All values were uncached");

        assert_eq!(index, 5, "5 tokio tasks executed");
    }
}
