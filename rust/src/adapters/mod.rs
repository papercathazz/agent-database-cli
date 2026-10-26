use crate::types::{DatabaseConfig, DatabaseType, MetadataRequest, QueryResult};
use anyhow::Result;
use async_trait::async_trait;

pub mod deadline;
pub mod mongodb;
pub mod mysql;
pub mod oracle_driver;
pub mod oracle_sqlcl;
pub mod postgres;
pub mod redis_driver;

/// 请求本身拥有参数，便于在串行适配器和共享连接池之间复用分发逻辑。
pub enum Operation {
    Test,
    Execute(String),
    Metadata(MetadataRequest),
}

impl Operation {
    pub async fn run(self, adapter: &mut dyn DatabaseAdapter) -> Result<QueryResult> {
        match self {
            Self::Test => {
                adapter.test().await?;
                Ok(QueryResult {
                    rows: vec![],
                    fields: None,
                    row_count: None,
                })
            }
            Self::Execute(command) => adapter.execute(&command).await,
            Self::Metadata(request) => adapter.metadata(request).await,
        }
    }

    pub async fn run_mysql(
        self,
        pool: &mysql_async::Pool,
        deadline: tokio::time::Instant,
    ) -> Result<QueryResult> {
        let command = match self {
            Self::Test => "SELECT 1".to_string(),
            Self::Execute(command) => command,
            Self::Metadata(request) => mysql::metadata_command(request)?,
        };
        mysql::query_with_pool(pool, &command, deadline).await
    }
}

#[async_trait]
pub trait DatabaseAdapter: Send {
    async fn connect(&mut self) -> Result<()>;
    async fn disconnect(&mut self) -> Result<()>;
    async fn test(&mut self) -> Result<()>;
    async fn execute(&mut self, command: &str) -> Result<QueryResult>;
    async fn metadata(&mut self, request: MetadataRequest) -> Result<QueryResult>;

    /// MySQL 查询仅共享线程安全的池句柄，不在执行期间占用适配器互斥锁。
    fn mysql_pool(&self) -> Option<mysql_async::Pool> {
        None
    }
}

pub fn create_adapter(config: &DatabaseConfig) -> Result<Box<dyn DatabaseAdapter>> {
    match config.db_type {
        DatabaseType::Mysql => Ok(Box::new(mysql::MySqlAdapter::new(config.url.clone()))),
        DatabaseType::Postgres => Ok(Box::new(postgres::PostgresAdapter::new(config.url.clone()))),
        DatabaseType::Redis => Ok(Box::new(redis_driver::RedisAdapter::new(
            config.url.clone(),
            config.redis_cluster.clone(),
        ))),
        DatabaseType::Mongodb => Ok(Box::new(mongodb::MongoDbAdapter::new(
            config.url.clone(),
            config.database.clone(),
        ))),
        DatabaseType::Oracle => {
            if matches!(
                config.oracle_driver,
                Some(crate::types::OracleDriver::Oracle | crate::types::OracleDriver::Oracledb)
            ) {
                Ok(Box::new(oracle_driver::OracleAdapter::new(
                    config.url.clone(),
                )))
            } else {
                Ok(Box::new(oracle_sqlcl::OracleSqlclAdapter::new(
                    config.url.clone(),
                    config
                        .sqlcl_path
                        .clone()
                        .unwrap_or_else(|| "sql".to_string()),
                    config.java_home.clone(),
                )))
            }
        }
    }
}
