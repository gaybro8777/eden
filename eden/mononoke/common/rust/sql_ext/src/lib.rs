/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#[cfg(not(fbcode_build))]
mod oss;
pub mod replication;
mod sqlite;

use sql::{Connection, Transaction};

pub use sqlite::{open_existing_sqlite_path, open_sqlite_in_memory, open_sqlite_path};

#[derive(Clone)]
pub struct SqlConnections {
    pub write_connection: Connection,
    pub read_connection: Connection,
    pub read_master_connection: Connection,
}

impl SqlConnections {
    /// Create SqlConnections from a single connection.
    pub fn new_single(connection: Connection) -> Self {
        Self {
            write_connection: connection.clone(),
            read_connection: connection.clone(),
            read_master_connection: connection,
        }
    }
}

#[derive(Clone)]
pub struct SqlShardedConnections {
    pub write_connections: Vec<Connection>,
    pub read_connections: Vec<Connection>,
    pub read_master_connections: Vec<Connection>,
}

impl SqlShardedConnections {
    pub fn is_empty(&self) -> bool {
        self.write_connections.is_empty()
    }
}

impl From<Vec<SqlConnections>> for SqlShardedConnections {
    fn from(shard_connections: Vec<SqlConnections>) -> Self {
        let mut write_connections = Vec::with_capacity(shard_connections.len());
        let mut read_connections = Vec::with_capacity(shard_connections.len());
        let mut read_master_connections = Vec::with_capacity(shard_connections.len());
        for connections in shard_connections.into_iter() {
            write_connections.push(connections.write_connection);
            read_connections.push(connections.read_connection);
            read_master_connections.push(connections.read_master_connection);
        }

        Self {
            read_connections,
            read_master_connections,
            write_connections,
        }
    }
}

#[must_use]
pub enum TransactionResult {
    Succeeded(Transaction),
    Failed,
}

pub mod facebook {
    #[cfg(fbcode_build)]
    mod r#impl;

    use std::fmt::{self, Debug};

    #[cfg(fbcode_build)]
    pub use r#impl::{
        create_myrouter_connections, create_mysql_connections_sharded,
        create_mysql_connections_unsharded, create_raw_xdb_connections,
        deprecated_create_mysql_pool_unsharded,
        myadmin::{MyAdmin, MyAdminLagMonitor},
        myrouter_ready, PoolConfig, SharedConnectionPool,
    };

    #[cfg(not(fbcode_build))]
    pub use crate::oss::{
        create_myrouter_connections, create_mysql_connections_sharded,
        create_mysql_connections_unsharded, create_raw_xdb_connections,
        deprecated_create_mysql_pool_unsharded, myrouter_ready, MyAdmin, MyAdminLagMonitor,
        PoolConfig, SharedConnectionPool,
    };

    /// Way to connect to the DB: via myrouter connections, raw xdb or Mysql client
    #[derive(Clone)]
    pub enum MysqlConnectionType {
        Myrouter(u16),
        RawXDB,
        Mysql(SharedConnectionPool, PoolConfig),
    }

    impl MysqlConnectionType {
        pub fn per_key_limit(&self) -> Option<usize> {
            match self {
                Self::Myrouter(_) => None,
                Self::RawXDB => None,
                Self::Mysql(_, pool_config) => {
                    #[cfg(fbcode_build)]
                    {
                        Some(pool_config.per_key_limit as usize)
                    }
                    #[cfg(not(fbcode_build))]
                    {
                        None
                    }
                }
            }
        }
    }

    impl Debug for MysqlConnectionType {
        fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
            match &self {
                Self::Myrouter(port) => write!(f, "MyRouter(port: {:?})", port),
                Self::Mysql(_, config) => write!(f, "MySQL with config {:?}", config),
                Self::RawXDB => write!(f, "RawXDB"),
            }
        }
    }

    #[derive(Debug, Clone)]
    pub struct MysqlOptions {
        pub connection_type: MysqlConnectionType,
        pub master_only: bool,
    }

    impl MysqlOptions {
        pub fn read_connection_type(&self) -> ReadConnectionType {
            if self.master_only {
                ReadConnectionType::Master
            } else {
                ReadConnectionType::Replica
            }
        }
    }

    #[derive(Copy, Clone, Debug)]
    pub enum ReadConnectionType {
        Replica,
        Master,
    }

    #[derive(Copy, Clone, Debug)]
    pub struct PoolSizeConfig {
        pub write_pool_size: usize,
        pub read_pool_size: usize,
        pub read_master_pool_size: usize,
    }
}
