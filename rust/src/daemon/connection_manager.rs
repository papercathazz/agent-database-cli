use crate::adapters::deadline::{debug_log, log_cleanup, query_timeout, timeout_error};
use crate::adapters::{create_adapter, DatabaseAdapter, Operation};
use crate::config::get_database_config;
use crate::security::assert_command_allowed;
use crate::ssh_tunnel::{start_ssh_tunnel, StartedSshTunnel};
use crate::types::{AppConfig, DatabaseConfig, MetadataRequest, QueryResult};
use anyhow::Result;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{Mutex as AsyncMutex, Notify};
use tokio::time::{timeout, timeout_at, Instant};

struct Activity {
    in_flight: usize,
    last_used: Instant,
}

struct Entry {
    adapter: AsyncMutex<Box<dyn DatabaseAdapter>>,
    mysql_pool: Option<mysql_async::Pool>,
    config: DatabaseConfig,
    adapter_config: DatabaseConfig,
    tunnel: Option<StartedSshTunnel>,
    activity: Mutex<Activity>,
    finished: Notify,
}

/// 使用权在缓存锁内取得；回收也在同一锁内检查并移除，消除检查与移除的竞态。
struct Lease(Arc<Entry>);

impl Lease {
    fn acquire(entry: Arc<Entry>) -> Self {
        entry.activity.lock().expect("连接活动锁中毒").in_flight += 1;
        Self(entry)
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        let mut activity = self.0.activity.lock().expect("连接活动锁中毒");
        activity.last_used = Instant::now();
        activity.in_flight -= 1;
        self.0.finished.notify_waiters();
    }
}

enum EntrySlot {
    Ready(Arc<Entry>),
    Initializing(Arc<Notify>),
}

pub struct ConnectionManager {
    config: AppConfig,
    // 该锁仅保护缓存和使用权转换，绝不跨 await 持有。
    entries: Mutex<HashMap<String, EntrySlot>>,
}

/// 初始化超时、取消和失败都要移除占位并唤醒等待者，避免数据库永久卡在 initializing。
struct Initializing<'a> {
    manager: &'a ConnectionManager,
    name: &'a str,
    notify: Arc<Notify>,
}

impl Drop for Initializing<'_> {
    fn drop(&mut self) {
        let mut entries = self.manager.entries.lock().expect("连接缓存锁中毒");
        if matches!(entries.get(self.name), Some(EntrySlot::Initializing(current)) if Arc::ptr_eq(current, &self.notify))
        {
            entries.remove(self.name);
        }
        self.notify.notify_waiters();
    }
}

impl ConnectionManager {
    pub fn new(config: AppConfig) -> Self {
        Self {
            config,
            entries: Mutex::new(HashMap::new()),
        }
    }

    pub async fn test(&self, name: &str) -> Result<Value> {
        self.run(name, Operation::Test, query_timeout()).await?;
        Ok(json!({ "ok": true }))
    }

    pub async fn execute(&self, name: &str, command: &str) -> Result<QueryResult> {
        let config = get_database_config(&self.config, name)?;
        assert_command_allowed(config, command)?;
        self.run(
            name,
            Operation::Execute(command.to_string()),
            query_timeout(),
        )
        .await
    }

    pub async fn metadata(&self, name: &str, request: MetadataRequest) -> Result<QueryResult> {
        self.run(name, Operation::Metadata(request), query_timeout())
            .await
    }

    /// 同一个截止时间覆盖初始化、等待互斥锁/池连接和执行，清理异步进行以保证及时响应。
    async fn run(
        &self,
        name: &str,
        operation: Operation,
        duration: Duration,
    ) -> Result<QueryResult> {
        let deadline = Instant::now() + duration;
        let lease = self.get_entry(name, deadline).await?;
        if let Some(pool) = &lease.0.mysql_pool {
            return operation.run_mysql(pool, deadline).await;
        }
        let mut adapter = timeout_at(deadline, lease.0.adapter.lock())
            .await
            .map_err(|_| timeout_error())?;
        match timeout_at(deadline, operation.run(adapter.as_mut())).await {
            Ok(result) => result,
            Err(_) => {
                debug_log(&format!("数据库 {name} 查询超时，替换连接并清理旧适配器"));
                let replacement = create_adapter(&lease.0.adapter_config)?;
                let mut old = std::mem::replace(&mut *adapter, replacement);
                // 部分驱动关闭需要网络交互，不能让它延长调用方的响应期限。
                tokio::spawn(async move {
                    match timeout(Duration::from_secs(1), old.disconnect()).await {
                        Ok(result) => log_cleanup(result),
                        Err(_) => debug_log("超时适配器关闭超过 1 秒，已丢弃"),
                    }
                });
                Err(timeout_error())
            }
        }
    }

    pub async fn reset(&self, name: &str) -> Result<Value> {
        let slot = self.entries.lock().expect("连接缓存锁中毒").remove(name);
        match slot {
            Some(EntrySlot::Ready(entry)) => close_entry(entry).await?,
            Some(EntrySlot::Initializing(notify)) => notify.notify_waiters(),
            None => {}
        }
        Ok(json!({ "reset": name }))
    }

    pub async fn close_all(&self) -> Result<()> {
        let slots = self
            .entries
            .lock()
            .expect("连接缓存锁中毒")
            .drain()
            .map(|(_, slot)| slot)
            .collect::<Vec<_>>();
        for slot in slots {
            match slot {
                EntrySlot::Ready(entry) => close_entry(entry).await?,
                EntrySlot::Initializing(notify) => notify.notify_waiters(),
            }
        }
        Ok(())
    }

    pub async fn cleanup_idle(&self) -> Result<()> {
        let expired = {
            let mut entries = self.entries.lock().expect("连接缓存锁中毒");
            let now = Instant::now();
            let mut expired = Vec::new();
            entries.retain(|_, slot| {
                let EntrySlot::Ready(entry) = slot else {
                    return true;
                };
                let activity = entry.activity.lock().expect("连接活动锁中毒");
                let idle = activity.in_flight == 0
                    && now.duration_since(activity.last_used)
                        >= Duration::from_secs(entry.config.keep_alive_seconds.unwrap_or(180));
                if idle {
                    expired.push(entry.clone());
                }
                !idle
            });
            expired
        };
        for entry in expired {
            close_entry(entry).await?;
        }
        Ok(())
    }

    pub async fn status(&self) -> Value {
        let entries = self.entries.lock().expect("连接缓存锁中毒");
        let connections = entries
            .iter()
            .map(|(name, slot)| {
                let EntrySlot::Ready(entry) = slot else {
                    return json!({ "name": name, "initializing": true });
                };
                let activity = entry.activity.lock().expect("连接活动锁中毒");
                json!({
                    "name": name,
                    "type": format!("{:?}", entry.config.db_type).to_lowercase(),
                    "keepAliveSeconds": entry.config.keep_alive_seconds.unwrap_or(180),
                    "sshTunnel": entry.tunnel.is_some(),
                    "busy": activity.in_flight > 0,
                    "inFlight": activity.in_flight,
                })
            })
            .collect::<Vec<_>>();
        json!({ "connections": connections })
    }

    async fn get_entry(&self, name: &str, deadline: Instant) -> Result<Lease> {
        loop {
            let (notify, initialize) = {
                let mut entries = self.entries.lock().expect("连接缓存锁中毒");
                match entries.get(name) {
                    Some(EntrySlot::Ready(entry)) => return Ok(Lease::acquire(entry.clone())),
                    Some(EntrySlot::Initializing(notify)) => (notify.clone(), false),
                    None => {
                        let notify = Arc::new(Notify::new());
                        entries.insert(name.to_string(), EntrySlot::Initializing(notify.clone()));
                        (notify, true)
                    }
                }
            };
            if initialize {
                let _initializing = Initializing {
                    manager: self,
                    name,
                    notify: notify.clone(),
                };
                let entry = timeout_at(deadline, create_entry(&self.config, name))
                    .await
                    .map_err(|_| timeout_error())??;
                let entry = Arc::new(entry);
                let lease = {
                    let mut entries = self.entries.lock().expect("连接缓存锁中毒");
                    if matches!(entries.get(name), Some(EntrySlot::Initializing(current)) if Arc::ptr_eq(current, &notify))
                    {
                        entries.insert(name.to_string(), EntrySlot::Ready(entry.clone()));
                        Some(Lease::acquire(entry.clone()))
                    } else {
                        None
                    }
                };
                if let Some(lease) = lease {
                    return Ok(lease);
                }
                close_entry(entry).await?;
                anyhow::bail!("数据库连接初始化已取消: {name}");
            }
            // 注册后重查占位是否仍然有效，避免 notify_waiters 发生在订阅之前导致漏唤醒。
            let notified = notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let still_initializing = matches!(
                self.entries.lock().expect("连接缓存锁中毒").get(name),
                Some(EntrySlot::Initializing(current)) if Arc::ptr_eq(current, &notify)
            );
            if still_initializing {
                timeout_at(deadline, notified)
                    .await
                    .map_err(|_| timeout_error())?;
            }
        }
    }
}

/// reset/配置切换先从缓存移除，再等待旧使用权结束；不会关闭新建的同名连接。
async fn close_entry(entry: Arc<Entry>) -> Result<()> {
    loop {
        let notified = entry.finished.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if entry.activity.lock().expect("连接活动锁中毒").in_flight == 0 {
            break;
        }
        notified.await;
    }
    entry.adapter.lock().await.disconnect().await
}

async fn create_entry(config: &AppConfig, name: &str) -> Result<Entry> {
    let config = get_database_config(config, name)?.clone();
    let tunnel = start_ssh_tunnel(&config).await?;
    let mut adapter_config = config.clone();
    if let Some(tunnel) = &tunnel {
        adapter_config.url = tunnel.url.clone();
        if tunnel.redis_cluster.is_some() {
            adapter_config.redis_cluster = tunnel.redis_cluster.clone();
        }
    }
    let mut adapter = create_adapter(&adapter_config)?;
    adapter.connect().await?;
    let mysql_pool = adapter.mysql_pool();
    Ok(Entry {
        adapter: AsyncMutex::new(adapter),
        mysql_pool,
        config,
        adapter_config,
        tunnel,
        activity: Mutex::new(Activity {
            in_flight: 0,
            last_used: Instant::now(),
        }),
        finished: Notify::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct WaitingAdapter {
        started: Arc<Notify>,
        closed: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl DatabaseAdapter for WaitingAdapter {
        async fn connect(&mut self) -> Result<()> {
            Ok(())
        }
        async fn disconnect(&mut self) -> Result<()> {
            self.closed.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn test(&mut self) -> Result<()> {
            self.execute("").await.map(|_| ())
        }
        async fn execute(&mut self, _: &str) -> Result<QueryResult> {
            self.started.notify_one();
            std::future::pending().await
        }
        async fn metadata(&mut self, _: MetadataRequest) -> Result<QueryResult> {
            self.execute("").await
        }
    }

    fn fixture() -> (
        Arc<ConnectionManager>,
        Arc<Entry>,
        Arc<Notify>,
        Arc<AtomicUsize>,
    ) {
        let config: DatabaseConfig = serde_json::from_value(json!({
            "type": "mysql", "url": "mysql://localhost/test", "keepAliveSeconds": 1,
        }))
        .unwrap();
        let manager = Arc::new(ConnectionManager::new(AppConfig {
            databases: HashMap::from([("db".to_string(), config.clone())]),
        }));
        let started = Arc::new(Notify::new());
        let closed = Arc::new(AtomicUsize::new(0));
        let entry = Arc::new(Entry {
            adapter: AsyncMutex::new(Box::new(WaitingAdapter {
                started: started.clone(),
                closed: closed.clone(),
            })),
            mysql_pool: None,
            config: config.clone(),
            adapter_config: config,
            tunnel: None,
            activity: Mutex::new(Activity {
                in_flight: 0,
                last_used: Instant::now() - Duration::from_secs(10),
            }),
            finished: Notify::new(),
        });
        manager
            .entries
            .lock()
            .unwrap()
            .insert("db".to_string(), EntrySlot::Ready(entry.clone()));
        (manager, entry, started, closed)
    }

    #[tokio::test]
    async fn active_lease_survives_idle_cleanup_and_refreshes_activity_on_drop() {
        let (manager, entry, _, closed) = fixture();
        let lease = manager
            .get_entry("db", Instant::now() + Duration::from_secs(1))
            .await
            .unwrap();
        manager.cleanup_idle().await.unwrap();
        assert_eq!(closed.load(Ordering::SeqCst), 0);
        let status = manager.status().await;
        assert_eq!(status["connections"][0]["busy"], true);
        assert_eq!(status["connections"][0]["inFlight"], 1);
        drop(lease);
        manager.cleanup_idle().await.unwrap();
        assert_eq!(
            manager.status().await["connections"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        entry.activity.lock().unwrap().last_used -= Duration::from_secs(10);
        manager.cleanup_idle().await.unwrap();
        assert_eq!(closed.load(Ordering::SeqCst), 1);
        assert!(manager.status().await["connections"]
            .as_array()
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn timeout_releases_busy_state_and_cleans_only_old_adapter() {
        let (manager, entry, _, closed) = fixture();
        let started = Instant::now();
        let result = manager
            .run("db", Operation::Test, Duration::from_millis(30))
            .await;
        assert!(result.unwrap_err().to_string().contains("查询超时"));
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(entry.activity.lock().unwrap().in_flight, 0);
        timeout(Duration::from_secs(1), async {
            while closed.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(manager.status().await["connections"][0]["busy"], false);
    }

    #[tokio::test]
    async fn queued_request_timeout_and_task_cancellation_balance_leases() {
        let (manager, entry, started, _) = fixture();
        let current = manager.clone();
        let request = tokio::spawn(async move {
            current
                .run("db", Operation::Test, Duration::from_secs(5))
                .await
        });
        started.notified().await;
        assert!(manager
            .run("db", Operation::Test, Duration::from_millis(20))
            .await
            .is_err());
        assert_eq!(entry.activity.lock().unwrap().in_flight, 1);
        request.abort();
        assert!(request.await.unwrap_err().is_cancelled());
        assert_eq!(entry.activity.lock().unwrap().in_flight, 0);
    }

    #[tokio::test]
    async fn reset_waits_for_existing_lease_without_closing_replacement() {
        let (manager, old, _, old_closed) = fixture();
        let (_, replacement, _, new_closed) = fixture();
        let lease = manager
            .get_entry("db", Instant::now() + Duration::from_secs(1))
            .await
            .unwrap();
        let current = manager.clone();
        let reset = tokio::spawn(async move { current.reset("db").await });
        timeout(Duration::from_secs(1), async {
            while manager.entries.lock().unwrap().contains_key("db") {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(!reset.is_finished());
        manager
            .entries
            .lock()
            .unwrap()
            .insert("db".to_string(), EntrySlot::Ready(replacement.clone()));
        drop(lease);
        reset.await.unwrap().unwrap();
        assert_eq!(old.activity.lock().unwrap().in_flight, 0);
        assert_eq!(old_closed.load(Ordering::SeqCst), 1);
        assert_eq!(new_closed.load(Ordering::SeqCst), 0);
        assert!(
            matches!(manager.entries.lock().unwrap().get("db"), Some(EntrySlot::Ready(entry)) if Arc::ptr_eq(entry, &replacement))
        );
    }

    #[tokio::test]
    async fn cancelled_initialization_clears_slot_and_wakes_waiters() {
        let manager = ConnectionManager::new(AppConfig {
            databases: HashMap::new(),
        });
        let notify = Arc::new(Notify::new());
        manager
            .entries
            .lock()
            .unwrap()
            .insert("db".to_string(), EntrySlot::Initializing(notify.clone()));
        let notified = notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        drop(Initializing {
            manager: &manager,
            name: "db",
            notify: notify.clone(),
        });
        timeout(Duration::from_secs(1), notified).await.unwrap();
        assert!(!manager.entries.lock().unwrap().contains_key("db"));
    }

    #[tokio::test]
    #[ignore = "需要 AGENT_DB_TEST_MYSQL_URL 指向隔离的 MySQL"]
    async fn mysql_manager_runs_same_database_queries_concurrently_and_tracks_busy() {
        let url = std::env::var("AGENT_DB_TEST_MYSQL_URL").unwrap();
        let config = serde_json::from_value(json!({ "databases": { "db": { "type": "mysql", "url": url, "keepAliveSeconds": 1 } } })).unwrap();
        let manager = Arc::new(ConnectionManager::new(config));
        manager.test("db").await.unwrap();
        let current = manager.clone();
        let long = tokio::spawn(async move { current.execute("db", "SELECT SLEEP(2)").await });
        timeout(Duration::from_secs(1), async {
            loop {
                if manager.status().await["connections"][0]["busy"] == true {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let lease = manager
            .get_entry("db", Instant::now() + Duration::from_secs(1))
            .await
            .unwrap();
        let entry = lease.0.clone();
        drop(lease);
        timeout(
            Duration::from_millis(500),
            manager.execute("db", "SELECT 1"),
        )
        .await
        .unwrap()
        .unwrap();
        entry.activity.lock().unwrap().last_used -= Duration::from_secs(10);
        manager.cleanup_idle().await.unwrap();
        assert_eq!(manager.status().await["connections"][0]["busy"], true);
        long.await.unwrap().unwrap();
        manager.close_all().await.unwrap();
    }

    #[tokio::test]
    #[ignore = "需要 AGENT_DB_TEST_MYSQL_URL 指向隔离的 MySQL"]
    async fn mysql_initialization_failure_is_not_cached() {
        let mut url = url::Url::parse(&std::env::var("AGENT_DB_TEST_MYSQL_URL").unwrap()).unwrap();
        url.set_path("/agent_db_deliberately_missing_database");
        let config = serde_json::from_value(
            json!({ "databases": { "db": { "type": "mysql", "url": url.as_str() } } }),
        )
        .unwrap();
        let manager = ConnectionManager::new(config);
        for _ in 0..2 {
            assert!(manager.test("db").await.is_err());
            assert!(manager.status().await["connections"]
                .as_array()
                .unwrap()
                .is_empty());
        }
    }
}
