use super::deadline::{debug_log, log_cleanup, query_timeout, timeout_error};
use super::DatabaseAdapter;
use crate::types::{MetadataRequest, MetadataType, QueryResult};
use anyhow::Result;
use async_trait::async_trait;
use mysql_async::{prelude::Queryable, Conn, Opts, Pool, Row, Value as MyValue};
use serde_json::{Map, Value};
use std::time::Duration;
use tokio::time::{timeout, timeout_at, Instant};
use url::Url;

pub struct MySqlAdapter {
    url: String,
    pool: Option<Pool>,
}

impl MySqlAdapter {
    pub fn new(url: String) -> Self {
        Self { url, pool: None }
    }
}

/// 未完整消费结果的连接必须显式关闭，不能交回 recycler 等待慢查询排空。
/// 同时覆盖查询超时、调用任务取消及 panic 路径；成功时才允许正常归还。
struct QueryConnection(Option<Conn>);

impl Drop for QueryConnection {
    fn drop(&mut self) {
        if let Some(conn) = self.0.take() {
            tokio::spawn(async move {
                // disconnect 会先标记连接失效；即使关闭超时，drop 也不会排空旧查询。
                match timeout(Duration::from_secs(1), conn.disconnect()).await {
                    Ok(result) => log_cleanup(result.map_err(Into::into)),
                    Err(_) => debug_log("MySQL 超时连接关闭超过 1 秒，已丢弃"),
                }
            });
        }
    }
}

/// 一个绝对截止时间覆盖排队取连接及完整读取结果，超时只影响当前连接。
pub async fn query_with_pool(pool: &Pool, command: &str, deadline: Instant) -> Result<QueryResult> {
    let conn = timeout_at(deadline, pool.get_conn())
        .await
        .map_err(|_| timeout_error())??;
    let mut conn = QueryConnection(Some(conn));
    let rows: Vec<Row> = match timeout_at(
        deadline,
        conn.0.as_mut().expect("查询连接存在").query(command),
    )
    .await
    {
        Ok(result) => result?,
        Err(_) => {
            debug_log("MySQL 查询超时，关闭当前连接");
            return Err(timeout_error());
        }
    };
    // query 已消费全部结果，此时归还连接不会等待服务端慢查询。
    drop(conn.0.take());
    let fields = rows
        .first()
        .map(|row| {
            row.columns_ref()
                .iter()
                .map(|c| c.name_str().to_string())
                .collect()
        })
        .unwrap_or_default();
    let values = rows.into_iter().map(row_to_json).collect::<Vec<_>>();
    Ok(QueryResult {
        row_count: Some(values.len() as u64),
        rows: values,
        fields: Some(fields),
    })
}

pub fn metadata_command(request: MetadataRequest) -> Result<String> {
    match request.request_type {
        MetadataType::Tables => Ok("show tables".to_string()),
        MetadataType::Columns => {
            let table = request
                .table
                .ok_or_else(|| anyhow::anyhow!("columns 元信息查询必须提供 --table"))?
                .replace('`', "``");
            Ok(format!("show columns from `{table}`"))
        }
        _ => anyhow::bail!("当前数据库不支持元信息类型: {:?}", request.request_type),
    }
}

#[async_trait]
impl DatabaseAdapter for MySqlAdapter {
    async fn connect(&mut self) -> Result<()> {
        if self.pool.is_none() {
            let pool = Pool::new(build_opts(&self.url)?);
            // 池本身是惰性的；只有真实探测通过后才缓存，避免假连接进入状态列表。
            if let Err(error) =
                query_with_pool(&pool, "SELECT 1", Instant::now() + query_timeout()).await
            {
                tokio::spawn(async move {
                    log_cleanup(pool.disconnect().await.map_err(Into::into));
                });
                return Err(error);
            }
            self.pool = Some(pool);
        }
        Ok(())
    }

    async fn disconnect(&mut self) -> Result<()> {
        if let Some(pool) = self.pool.take() {
            pool.disconnect().await?;
        }
        Ok(())
    }

    async fn test(&mut self) -> Result<()> {
        self.execute("select 1").await.map(|_| ())
    }

    async fn execute(&mut self, command: &str) -> Result<QueryResult> {
        self.connect().await?;
        query_with_pool(
            self.pool.as_ref().expect("池已初始化"),
            command,
            Instant::now() + query_timeout(),
        )
        .await
    }

    async fn metadata(&mut self, request: MetadataRequest) -> Result<QueryResult> {
        self.execute(&metadata_command(request)?).await
    }

    fn mysql_pool(&self) -> Option<Pool> {
        self.pool.clone()
    }
}

/// 先补齐缺省约束再解析 URL，避免显式 pool_min 超过驱动默认 max 时被误拒绝。
fn build_opts(value: &str) -> Result<Opts> {
    let mut url = Url::parse(&normalize_mysql_url(value)?)?;
    let pairs = url.query_pairs().into_owned().collect::<Vec<_>>();
    let explicit_min = pairs
        .iter()
        .find(|(key, _)| key == "pool_min")
        .map(|(_, value)| value.parse::<usize>())
        .transpose()?;
    let explicit_max = pairs
        .iter()
        .find(|(key, _)| key == "pool_max")
        .map(|(_, value)| value.parse::<usize>())
        .transpose()?;
    let max = explicit_max.unwrap_or(4.max(explicit_min.unwrap_or(0)));
    let min = explicit_min.unwrap_or(4.min(max));
    if explicit_min.is_none() {
        url.query_pairs_mut()
            .append_pair("pool_min", &min.to_string());
    }
    if explicit_max.is_none() {
        url.query_pairs_mut()
            .append_pair("pool_max", &max.to_string());
    }
    Ok(Opts::from_url(url.as_str())?)
}

fn row_to_json(row: Row) -> Value {
    let columns = row.columns_ref().to_vec();
    let values = row.unwrap();
    let mut object = Map::new();
    for (index, column) in columns.iter().enumerate() {
        object.insert(
            column.name_str().to_string(),
            mysql_value_to_json(values.get(index).cloned().unwrap_or(MyValue::NULL)),
        );
    }
    Value::Object(object)
}

fn mysql_value_to_json(value: MyValue) -> Value {
    match value {
        MyValue::NULL => Value::Null,
        MyValue::Bytes(bytes) => Value::String(String::from_utf8_lossy(&bytes).to_string()),
        MyValue::Int(value) => Value::Number(value.into()),
        MyValue::UInt(value) => Value::Number(value.into()),
        MyValue::Float(value) => serde_json::Number::from_f64(value as f64)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        MyValue::Double(value) => serde_json::Number::from_f64(value)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        other => Value::String(format!("{:?}", other)),
    }
}

fn normalize_mysql_url(value: &str) -> Result<String> {
    let mut parsed = Url::parse(value)?;
    let supported = [
        "pool_min",
        "pool_max",
        "inactive_connection_ttl",
        "ttl_check_interval",
        "conn_ttl",
        "tcp_keepalive_time_ms",
        "tcp_connect_timeout_ms",
        "stmt_cache_size",
        "prefer_socket",
        "socket",
        "compression",
        "ssl-mode",
    ];
    let pairs = parsed
        .query_pairs()
        .filter(|(key, _)| supported.contains(&key.as_ref()))
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect::<Vec<_>>();
    parsed.set_query(None);
    if !pairs.is_empty() {
        let query = pairs
            .into_iter()
            .map(|(key, value)| format!("{}={}", key, value))
            .collect::<Vec<_>>()
            .join("&");
        parsed.set_query(Some(&query));
    }
    Ok(parsed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pool_defaults_preserve_explicit_constraints_and_ttl() {
        for (query, min, max) in [
            ("", 4, 4),
            ("pool_min=10", 10, 10),
            ("pool_min=200", 200, 200),
            ("pool_min=1", 1, 4),
            ("pool_max=2", 2, 2),
            ("pool_min=1&pool_max=8", 1, 8),
        ] {
            let opts = build_opts(&format!("mysql://test:test@localhost/db?{query}&inactive_connection_ttl=60&ttl_check_interval=7&conn_ttl=80")).unwrap();
            assert_eq!(opts.pool_opts().constraints().min(), min);
            assert_eq!(opts.pool_opts().constraints().max(), max);
            assert_eq!(
                opts.pool_opts().inactive_connection_ttl(),
                Duration::from_secs(60)
            );
            assert_eq!(
                opts.pool_opts().ttl_check_interval(),
                Duration::from_secs(7)
            );
            assert_eq!(opts.conn_ttl(), Some(Duration::from_secs(80)));
        }
        assert!(build_opts("mysql://localhost/db?pool_min=5&pool_max=2").is_err());
    }

    async fn integration_pool(max: usize) -> Pool {
        let url = std::env::var("AGENT_DB_TEST_MYSQL_URL").expect("须配置隔离测试 MySQL URL");
        let mut url = Url::parse(&url).unwrap();
        url.query_pairs_mut()
            .append_pair("pool_min", "0")
            .append_pair("pool_max", &max.to_string())
            .append_pair("prefer_socket", "false");
        let pool = Pool::new(build_opts(url.as_str()).unwrap());
        query_with_pool(&pool, "SELECT 1", Instant::now() + Duration::from_secs(10))
            .await
            .unwrap();
        pool
    }

    #[tokio::test]
    #[ignore = "需要 AGENT_DB_TEST_MYSQL_URL 指向隔离的 MySQL"]
    async fn mysql_slow_query_does_not_block_fast_query_or_other_connection_on_timeout() {
        let pool = integration_pool(2).await;
        let long_pool = pool.clone();
        let long = tokio::spawn(async move {
            query_with_pool(
                &long_pool,
                "SELECT SLEEP(1)",
                Instant::now() + Duration::from_secs(3),
            )
            .await
        });
        // 等待慢 SQL 取得连接，确保快查询确实与慢查询重叠。
        tokio::time::sleep(Duration::from_millis(100)).await;
        query_with_pool(
            &pool,
            "SELECT 1",
            Instant::now() + Duration::from_millis(500),
        )
        .await
        .unwrap();
        let result = query_with_pool(
            &pool,
            "SELECT SLEEP(10)",
            Instant::now() + Duration::from_millis(100),
        )
        .await;
        assert!(result.unwrap_err().to_string().contains("查询超时"));
        assert!(
            long.await.unwrap().is_ok(),
            "另一连接上的查询不能被单次超时关闭"
        );
        query_with_pool(
            &pool,
            "SELECT 1",
            Instant::now() + Duration::from_millis(500),
        )
        .await
        .unwrap();
        pool.disconnect().await.unwrap();
    }

    #[tokio::test]
    #[ignore = "需要 AGENT_DB_TEST_MYSQL_URL 指向隔离的 MySQL"]
    async fn mysql_repeated_timeouts_and_cancellation_release_pool_slots() {
        let pool = integration_pool(4).await;
        for _ in 0..3 {
            let mut queries = Vec::new();
            for _ in 0..4 {
                let pool = pool.clone();
                queries.push(tokio::spawn(async move {
                    query_with_pool(
                        &pool,
                        "SELECT SLEEP(20)",
                        Instant::now() + Duration::from_millis(150),
                    )
                    .await
                }));
            }
            for query in queries {
                assert!(query
                    .await
                    .unwrap()
                    .unwrap_err()
                    .to_string()
                    .contains("查询超时"));
            }
            query_with_pool(&pool, "SELECT 1", Instant::now() + Duration::from_secs(1))
                .await
                .unwrap();
        }
        let active_pool = pool.clone();
        let query = tokio::spawn(async move {
            query_with_pool(
                &active_pool,
                "SELECT SLEEP(20)",
                Instant::now() + Duration::from_secs(30),
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        query.abort();
        assert!(query.await.unwrap_err().is_cancelled());
        // 占满所有槽位，任一泄漏都会让其中一次取连接超时。
        let mut conns = Vec::new();
        for _ in 0..4 {
            conns.push(
                timeout(Duration::from_secs(1), pool.get_conn())
                    .await
                    .unwrap()
                    .unwrap(),
            );
        }
        drop(conns);
        pool.disconnect().await.unwrap();
    }

    #[tokio::test]
    #[ignore = "需要 AGENT_DB_TEST_MYSQL_URL 指向隔离的 MySQL"]
    async fn mysql_pool_wait_is_bounded_without_closing_active_query() {
        let pool = integration_pool(1).await;
        let conn = pool.get_conn().await.unwrap();
        let started = Instant::now();
        assert!(
            query_with_pool(&pool, "SELECT 1", started + Duration::from_millis(50))
                .await
                .is_err()
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        drop(conn);
        query_with_pool(&pool, "SELECT 1", Instant::now() + Duration::from_secs(1))
            .await
            .unwrap();
        pool.disconnect().await.unwrap();
    }
}
