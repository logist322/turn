#[cfg(test)]
mod server_test;

pub mod config;
pub mod request;

use crate::{
    allocation::{allocation_manager::*, five_tuple::FiveTuple, AllocationMap},
    auth::AuthHandler,
    error::*,
    proto::lifetime::DEFAULT_LIFETIME,
};
use config::*;
use request::*;

use futures::FutureExt as _;
use std::{collections::HashMap, sync::Arc};
use tokio::{
    sync::{
        broadcast::{self, error::RecvError},
        mpsc, Mutex,
    },
    time::{Duration, Instant},
};
use util::Conn;

const INBOUND_MTU: usize = 1500;

/// The protocol to communicate between the [`Server`]'s public methods
/// and the threads spawned in the [`read_loop`] method.
#[derive(Clone)]
enum Command {
    /// Command to delete [`crate::allocation::Allocation`] by provided
    /// `username`.
    DeleteAllocations(String, Arc<mpsc::Receiver<()>>),

    GetAllocations(Arc<mpsc::Sender<AllocationMap>>),

    GetMetrics(FiveTuple, Arc<mpsc::Sender<Result<usize>>>),

    /// Command to close the [`Server`].
    Close(Arc<mpsc::Receiver<()>>),
}

/// Server is an instance of the TURN Server
pub struct Server {
    auth_handler: Arc<dyn AuthHandler + Send + Sync>,
    realm: String,
    channel_bind_timeout: Duration,
    pub(crate) nonces: Arc<Mutex<HashMap<String, Instant>>>,
    handle: Mutex<Option<broadcast::Sender<Command>>>,
}

impl Server {
    /// creates the TURN server
    pub async fn new(config: ServerConfig) -> Result<Self> {
        config.validate()?;

        let (handle, _) = broadcast::channel(16);
        let mut s = Server {
            auth_handler: config.auth_handler,
            realm: config.realm,
            channel_bind_timeout: config.channel_bind_timeout,
            nonces: Arc::new(Mutex::new(HashMap::new())),
            handle: Mutex::new(Some(handle.clone())),
        };

        if s.channel_bind_timeout == Duration::from_secs(0) {
            s.channel_bind_timeout = DEFAULT_LIFETIME;
        }

        for p in config.conn_configs.into_iter() {
            let nonces = Arc::clone(&s.nonces);
            let auth_handler = Arc::clone(&s.auth_handler);
            let realm = s.realm.clone();
            let channel_bind_timeout = s.channel_bind_timeout;
            let handle_rx = handle.subscribe();
            let conn = p.conn;
            let allocation_manager = Arc::new(Manager::new(ManagerConfig {
                relay_addr_generator: p.relay_addr_generator,
                gather_metrics: p.gather_metrics,
            }));

            tokio::spawn({
                let allocation_manager = Arc::clone(&allocation_manager);

                async move {
                    Server::read_loop(
                        conn,
                        allocation_manager,
                        nonces,
                        auth_handler,
                        realm,
                        channel_bind_timeout,
                        handle_rx,
                    )
                    .await;
                }
            });
        }

        Ok(s)
    }

    async fn read_loop(
        conn: Arc<dyn Conn + Send + Sync>,
        allocation_manager: Arc<Manager>,
        nonces: Arc<Mutex<HashMap<String, Instant>>>,
        auth_handler: Arc<dyn AuthHandler + Send + Sync>,
        realm: String,
        channel_bind_timeout: Duration,
        mut handle_rx: broadcast::Receiver<Command>,
    ) {
        let mut buf = vec![0u8; INBOUND_MTU];
        loop {
            let (n, addr) = futures::select! {
                v = conn.recv_from(&mut buf).fuse() => {
                    match v {
                        Ok(v) => v,
                        Err(err) => {
                            log::debug!("exit read loop on error: {err}");
                            break;
                        }
                    }
                },
                cmd = handle_rx.recv().fuse() => {
                    match cmd {
                        Ok(Command::DeleteAllocations(name, _)) => {
                            allocation_manager
                                .delete_allocations_by_username(name)
                                .await;
                            continue;
                        },
                        Ok(Command::GetAllocations(sender)) => {
                            drop(sender.send(allocation_manager.get_allocations().await).await);

                            continue
                        },
                        Ok(Command::GetMetrics(five_tuple, sender)) => {
                            drop(sender.send(allocation_manager.get_metrics(five_tuple).await).await);

                            continue
                        },
                        Err(RecvError::Closed) | Ok(Command::Close(_)) => break,
                        Err(RecvError::Lagged(n)) => {
                            log::error!("Turn server has lagged by {n} messages");
                            continue
                        },
                    }
                }
            };

            let mut r = Request {
                conn: Arc::clone(&conn),
                src_addr: addr,
                buff: buf[..n].to_vec(),
                allocation_manager: Arc::clone(&allocation_manager),
                nonces: Arc::clone(&nonces),
                auth_handler: Arc::clone(&auth_handler),
                realm: realm.clone(),
                channel_bind_timeout,
            };

            if let Err(err) = r.handle_request().await {
                log::error!("error when handling datagram: {}", err);
            }
        }

        let _ = allocation_manager.close().await;
        let _ = conn.close().await;
    }

    /// Deletes the [`crate::allocation::Allocation`] by the provided `username`.
    pub async fn delete_allocation(&self, username: String) -> Result<()> {
        let tx = self.handle.lock().await.clone();
        if let Some(tx) = tx {
            let (closed_tx, closed_rx) = mpsc::channel(1);
            tx.send(Command::DeleteAllocations(username, Arc::new(closed_rx)))
                .map_err(|_| Error::ErrClosed)?;

            closed_tx.closed().await;

            Ok(())
        } else {
            Err(Error::ErrClosed)
        }
    }

    pub async fn get_allocations(&self) -> Result<AllocationMap> {
        let tx = self.handle.lock().await.clone();
        if let Some(tx) = tx {
            let (allocation_tx, mut allocation_rx) = mpsc::channel(1);
            tx.send(Command::GetAllocations(Arc::new(allocation_tx)))
                .map_err(|_| Error::ErrClosed)?;

            Ok(allocation_rx.recv().await.ok_or_else(|| Error::ErrClosed)?)
        } else {
            Err(Error::ErrClosed)
        }
    }

    pub async fn get_metrics(&self, five_tuple: FiveTuple) -> Result<usize> {
        let tx = self.handle.lock().await.clone();
        if let Some(tx) = tx {
            let (metrics_tx, mut metrics_rx) = mpsc::channel(1);
            tx.send(Command::GetMetrics(five_tuple, Arc::new(metrics_tx)))
                .map_err(|_| Error::ErrClosed)?;

            metrics_rx.recv().await.ok_or_else(|| Error::ErrClosed)?
        } else {
            Err(Error::ErrClosed)
        }
    }

    /// Close stops the TURN Server. It cleans up any associated state and closes all connections it is managing
    pub async fn close(&self) -> Result<()> {
        let tx = self.handle.lock().await.take();
        if let Some(tx) = tx {
            if tx.receiver_count() == 0 {
                return Ok(());
            }

            let (closed_tx, closed_rx) = mpsc::channel(1);
            let _ = tx.send(Command::Close(Arc::new(closed_rx)));
            closed_tx.closed().await
        }

        Ok(())
    }
}
