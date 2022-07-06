use axum::extract::ws::Message;
use dashmap::mapref::entry::Entry as DashMapEntry;
use dashmap::DashMap;
use ethers::prelude::Transaction;
use ethers::prelude::{Block, TxHash, H256};
use futures::future::Abortable;
use futures::future::{join_all, AbortHandle};
use futures::stream::FuturesUnordered;
use futures::stream::StreamExt;
use linkedhashmap::LinkedHashMap;
use parking_lot::RwLock;
use redis_cell_client::MultiplexedConnection;
use serde_json::json;
use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{self, AtomicUsize};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_stream::wrappers::{BroadcastStream, WatchStream};
use tracing::{debug, info, info_span, instrument, trace, warn, Instrument};

use crate::config::{RpcConfig, Web3ConnectionConfig};
use crate::connections::Web3Connections;
use crate::jsonrpc::JsonRpcForwardedResponse;
use crate::jsonrpc::JsonRpcForwardedResponseEnum;
use crate::jsonrpc::JsonRpcRequest;
use crate::jsonrpc::JsonRpcRequestEnum;

static APP_USER_AGENT: &str = concat!(
    "satoshiandkin/",
    env!("CARGO_PKG_NAME"),
    "/",
    env!("CARGO_PKG_VERSION"),
);

// TODO: put this in config? what size should we do?
const RESPONSE_CACHE_CAP: usize = 1024;

/// TODO: these types are probably very bad keys and values. i couldn't get caching of warp::reply::Json to work
type CacheKey = (Option<H256>, String, Option<String>);

type ResponseLrcCache = RwLock<LinkedHashMap<CacheKey, JsonRpcForwardedResponse>>;

type ActiveRequestsMap = DashMap<CacheKey, watch::Receiver<bool>>;

pub type AnyhowJoinHandle<T> = JoinHandle<anyhow::Result<T>>;

pub async fn flatten_handle<T>(handle: AnyhowJoinHandle<T>) -> anyhow::Result<T> {
    match handle.await {
        Ok(Ok(result)) => Ok(result),
        Ok(Err(err)) => Err(err),
        Err(err) => Err(err.into()),
    }
}

pub async fn flatten_handles<T>(
    mut handles: FuturesUnordered<AnyhowJoinHandle<T>>,
) -> anyhow::Result<()> {
    while let Some(x) = handles.next().await {
        match x {
            Err(e) => return Err(e.into()),
            Ok(Err(e)) => return Err(e),
            Ok(Ok(_)) => {}
        }
    }

    Ok(())
}

// TODO: think more about TxState. d
#[derive(Clone)]
pub enum TxState {
    Pending(Transaction),
    Confirmed(Transaction),
    Orphaned(Transaction),
}

/// The application
// TODO: this debug impl is way too verbose. make something smaller
// TODO: if Web3ProxyApp is always in an Arc, i think we can avoid having at least some of these internal things in arcs
// TODO: i'm sure this is more arcs than necessary, but spawning futures makes references hard
pub struct Web3ProxyApp {
    chain_id: usize,
    http_client: Option<reqwest::Client>,
    rate_limiter_conn: Option<MultiplexedConnection>,
    /// Send requests to the best server available
    balanced_rpcs: Arc<Web3Connections>,
    /// Send private requests (like eth_sendRawTransaction) to all these servers
    private_rpcs: Arc<Web3Connections>,
    // TODO: move this into redis?
    incoming_requests: ActiveRequestsMap,
    // TODO: move this into redis?
    response_cache: ResponseLrcCache,
    head_block_receiver: watch::Receiver<Block<TxHash>>,
    pending_tx_sender: broadcast::Sender<TxState>,
    pending_transactions: Arc<DashMap<TxHash, TxState>>,
    // next_subscription_id should be per connection and not per app
    next_subscription_id: AtomicUsize,
}

impl fmt::Debug for Web3ProxyApp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // TODO: the default formatter takes forever to write. this is too quiet though
        f.debug_struct("Web3ProxyApp").finish_non_exhaustive()
    }
}

impl Web3ProxyApp {
    pub fn get_pending_transactions(&self) -> &DashMap<TxHash, TxState> {
        &self.pending_transactions
    }

    pub async fn update_config(
        &self,
        new_config: RpcConfig,
    ) -> anyhow::Result<FuturesUnordered<JoinHandle<anyhow::Result<()>>>> {
        // TODO: save these handles somewhere else instead?
        let handles = FuturesUnordered::new();

        // let mut new_balanced_rpcs: Vec<(String, Web3ConnectionConfig)> = vec![];
        // let mut old_balanced_rpcs: Vec<String> = vec![];

        // do not allow the chain id to change
        if new_config.shared.chain_id != self.chain_id {
            return Err(anyhow::anyhow!("cannot change chain id!"));
        }

        // TODO: allow changing the redis address or other shared config?

        // TODO: create new connections
        // TODO: attach context to this error
        let (balanced_rpcs, balanced_handle) = Web3Connections::spawn(
            self.chain_id,
            self.http_client.as_ref(),
            self.rate_limiter_conn.as_ref(),
            None,
            Some(self.pending_tx_sender.clone()),
            self.pending_transactions.clone(),
            new_config.balanced_rpcs,
        )
        .await?;

        handles.push(balanced_handle);

        let private_rpcs = if new_config.private_rpcs.is_empty() {
            info!("No private relays configured. Any transactions will be broadcast to the public mempool!");
            balanced_rpcs.clone()
        } else {
            // TODO: attach context to this error
            let (private_rpcs, private_handle) = Web3Connections::spawn(
                self.chain_id,
                self.http_client.as_ref(),
                self.rate_limiter_conn.as_ref(),
                // subscribing to new heads here won't work well
                None,
                // TODO: subscribe to pending transactions on the private rpcs?
                Some(self.pending_tx_sender.clone()),
                self.pending_transactions.clone(),
                new_config.private_rpcs,
            )
            .await?;

            handles.push(private_handle);

            private_rpcs
        };

        // swap the connections. self.balanced_rpcs owns head_block_sender
        self.balanced_rpcs.swap_rpcs(balanced_rpcs).await?;
        self.private_rpcs.swap_rpcs(private_rpcs).await?;

        // TODO: what do we do with the handles? we need to error if any of them error
        Ok(handles)
    }

    pub async fn spawn_with_watched_config(
        config_receiver: flume::Receiver<RpcConfig>,
    ) -> anyhow::Result<(
        Arc<Web3ProxyApp>,
        FuturesUnordered<JoinHandle<anyhow::Result<()>>>,
    )> {
        // there should already be a config ready for the receiver
        let first_config = config_receiver.try_recv()?;

        let (app, app_handles) = Web3ProxyApp::spawn(
            first_config.shared.chain_id,
            first_config.shared.rate_limit_redis,
            first_config.balanced_rpcs,
            first_config.private_rpcs,
        )
        .await?;

        let config_handle = {
            let app = app.clone();

            tokio::spawn(async move {
                while let Ok(new_config) = config_receiver.recv_async().await {
                    let new_handles = app.update_config(new_config).await?;

                    // TODO: what should we do with this? probably spawn with something to log errors and exit
                    drop(new_handles);
                }

                Ok(())
            })
        };

        app_handles.push(config_handle);

        Ok((app, app_handles))
    }

    pub async fn spawn(
        chain_id: usize,
        redis_address: Option<String>,
        balanced_rpcs: HashMap<String, Web3ConnectionConfig>,
        private_rpcs: HashMap<String, Web3ConnectionConfig>,
    ) -> anyhow::Result<(
        Arc<Web3ProxyApp>,
        FuturesUnordered<JoinHandle<anyhow::Result<()>>>,
    )> {
        // TODO: save these handles somewhere else instead?
        let handles = FuturesUnordered::new();

        // make a http shared client
        // TODO: how should we configure the connection pool?
        // TODO: 5 minutes is probably long enough. unlimited is a bad idea if something is wrong with the remote server
        let http_client = Some(
            reqwest::ClientBuilder::new()
                .connect_timeout(Duration::from_secs(5))
                .timeout(Duration::from_secs(60))
                .user_agent(APP_USER_AGENT)
                .build()?,
        );

        let rate_limiter_conn = match redis_address {
            Some(redis_address) => {
                info!("Connecting to redis on {}", redis_address);
                let redis_client = redis_cell_client::Client::open(redis_address)?;

                // TODO: r2d2 connection pool?
                let redis_conn = redis_client.get_multiplexed_tokio_connection().await?;

                Some(redis_conn)
            }
            None => {
                info!("No redis address");
                None
            }
        };

        // TODO: subscribe to pending transactions on the private rpcs, too?
        let (head_block_sender, head_block_receiver) = watch::channel(Block::default());
        // TODO: will one receiver lagging be okay?
        let (pending_tx_sender, pending_tx_receiver) = broadcast::channel(16);

        let pending_transactions = Arc::new(DashMap::new());

        // TODO: don't drop the pending_tx_receiver. instead, read it to mark transactions as "seen". once seen, we won't re-send them
        // TODO: once a transaction is "Confirmed" we remove it from the map. this should prevent major memory leaks.
        // TODO: we should still have some sort of expiration or maximum size limit for the map

        // TODO: attach context to this error
        let (balanced_rpcs, balanced_handle) = Web3Connections::spawn(
            chain_id,
            http_client.as_ref(),
            rate_limiter_conn.as_ref(),
            Some(head_block_sender),
            Some(pending_tx_sender.clone()),
            pending_transactions.clone(),
            balanced_rpcs,
        )
        .await?;

        handles.push(balanced_handle);

        let private_rpcs = if private_rpcs.is_empty() {
            warn!("No private relays configured. Any transactions will be broadcast to the public mempool!");
            balanced_rpcs.clone()
        } else {
            // TODO: attach context to this error
            let (private_rpcs, private_handle) = Web3Connections::spawn(
                chain_id,
                http_client.as_ref(),
                rate_limiter_conn.as_ref(),
                // subscribing to new heads here won't work well
                None,
                // TODO: subscribe to pending transactions on the private rpcs?
                Some(pending_tx_sender.clone()),
                pending_transactions.clone(),
                private_rpcs,
            )
            .await?;

            handles.push(private_handle);

            private_rpcs
        };

        // TODO: use this? it could listen for confirmed transactions and then clear pending_transactions, but the head_block_sender is doing that
        drop(pending_tx_receiver);

        let app = Self {
            chain_id,
            http_client,
            rate_limiter_conn,
            balanced_rpcs,
            private_rpcs,
            incoming_requests: Default::default(),
            response_cache: Default::default(),
            head_block_receiver,
            pending_tx_sender,
            pending_transactions,
            next_subscription_id: 1.into(),
        };

        let app = Arc::new(app);

        Ok((app, handles))
    }

    pub async fn eth_subscribe(
        self: Arc<Self>,
        payload: JsonRpcRequest,
        // TODO: taking a sender for Message instead of the exact json we are planning to send feels wrong, but its easier for now
        subscription_tx: flume::Sender<Message>,
    ) -> anyhow::Result<(AbortHandle, JsonRpcForwardedResponse)> {
        let (subscription_abort_handle, subscription_registration) = AbortHandle::new_pair();

        // TODO: this only needs to be unique per connection. we don't need it globably unique
        let subscription_id = self
            .next_subscription_id
            .fetch_add(1, atomic::Ordering::SeqCst);
        let subscription_id = format!("{:#x}", subscription_id);

        // save the id so we can use it in the response
        let id = payload.id.clone();

        match payload.params.as_deref().unwrap().get() {
            r#"["newHeads"]"# => {
                let head_block_receiver = self.head_block_receiver.clone();

                let subscription_id = subscription_id.clone();

                trace!(?subscription_id, "new heads subscription");
                tokio::spawn(async move {
                    let mut head_block_receiver = Abortable::new(
                        WatchStream::new(head_block_receiver),
                        subscription_registration,
                    );

                    while let Some(new_head) = head_block_receiver.next().await {
                        // TODO: make a struct for this? using our JsonRpcForwardedResponse won't work because it needs an id
                        let msg = json!({
                            "jsonrpc": "2.0",
                            "method":"eth_subscription",
                            "params": {
                                "subscription": subscription_id,
                                "result": new_head,
                            },
                        });

                        let msg = Message::Text(serde_json::to_string(&msg).unwrap());

                        if subscription_tx.send_async(msg).await.is_err() {
                            // TODO: cancel this subscription earlier? select on head_block_receiver.next() and an abort handle?
                            break;
                        };
                    }

                    trace!(?subscription_id, "closed new heads subscription");
                });
            }
            r#"["newPendingTransactions"]"# => {
                let pending_tx_receiver = self.pending_tx_sender.subscribe();

                let mut pending_tx_receiver = Abortable::new(
                    BroadcastStream::new(pending_tx_receiver),
                    subscription_registration,
                );

                let subscription_id = subscription_id.clone();

                trace!(?subscription_id, "pending transactions subscription");
                tokio::spawn(async move {
                    while let Some(Ok(new_tx_state)) = pending_tx_receiver.next().await {
                        let new_tx = match new_tx_state {
                            TxState::Pending(tx) => tx,
                            TxState::Confirmed(..) => continue,
                            TxState::Orphaned(tx) => tx,
                        };

                        // TODO: make a struct for this? using our JsonRpcForwardedResponse won't work because it needs an id
                        let msg = json!({
                            "jsonrpc": "2.0",
                            "method": "eth_subscription",
                            "params": {
                                "subscription": subscription_id,
                                "result": new_tx.hash,
                            },
                        });

                        let msg = Message::Text(serde_json::to_string(&msg).unwrap());

                        if subscription_tx.send_async(msg).await.is_err() {
                            // TODO: cancel this subscription earlier? select on head_block_receiver.next() and an abort handle?
                            break;
                        };
                    }

                    trace!(?subscription_id, "closed new heads subscription");
                });
            }
            r#"["newPendingFullTransactions"]"# => {
                // TODO: too much copy/pasta with newPendingTransactions
                let pending_tx_receiver = self.pending_tx_sender.subscribe();

                let mut pending_tx_receiver = Abortable::new(
                    BroadcastStream::new(pending_tx_receiver),
                    subscription_registration,
                );

                let subscription_id = subscription_id.clone();

                trace!(?subscription_id, "pending transactions subscription");

                // TODO: do something with this handle?
                tokio::spawn(async move {
                    while let Some(Ok(new_tx_state)) = pending_tx_receiver.next().await {
                        let new_tx = match new_tx_state {
                            TxState::Pending(tx) => tx,
                            TxState::Confirmed(..) => continue,
                            TxState::Orphaned(tx) => tx,
                        };

                        // TODO: make a struct for this? using our JsonRpcForwardedResponse won't work because it needs an id
                        let msg = json!({
                            "jsonrpc": "2.0",
                            "method": "eth_subscription",
                            "params": {
                                "subscription": subscription_id,
                                // upstream just sends the txid, but we want to send the whole transaction
                                "result": new_tx,
                            },
                        });

                        let msg = Message::Text(serde_json::to_string(&msg).unwrap());

                        if subscription_tx.send_async(msg).await.is_err() {
                            // TODO: cancel this subscription earlier? select on head_block_receiver.next() and an abort handle?
                            break;
                        };
                    }

                    trace!(?subscription_id, "closed new heads subscription");
                });
            }
            r#"["newPendingRawTransactions"]"# => {
                // TODO: too much copy/pasta with newPendingTransactions
                let pending_tx_receiver = self.pending_tx_sender.subscribe();

                let mut pending_tx_receiver = Abortable::new(
                    BroadcastStream::new(pending_tx_receiver),
                    subscription_registration,
                );

                let subscription_id = subscription_id.clone();

                trace!(?subscription_id, "pending transactions subscription");

                // TODO: do something with this handle?
                tokio::spawn(async move {
                    while let Some(Ok(new_tx_state)) = pending_tx_receiver.next().await {
                        let new_tx = match new_tx_state {
                            TxState::Pending(tx) => tx,
                            TxState::Confirmed(..) => continue,
                            TxState::Orphaned(tx) => tx,
                        };

                        // TODO: make a struct for this? using our JsonRpcForwardedResponse won't work because it needs an id
                        let msg = json!({
                            "jsonrpc": "2.0",
                            "method": "eth_subscription",
                            "params": {
                                "subscription": subscription_id,
                                // upstream just sends the txid, but we want to send the whole transaction
                                "result": new_tx.rlp(),
                            },
                        });

                        let msg = Message::Text(serde_json::to_string(&msg).unwrap());

                        if subscription_tx.send_async(msg).await.is_err() {
                            // TODO: cancel this subscription earlier? select on head_block_receiver.next() and an abort handle?
                            break;
                        };
                    }

                    trace!(?subscription_id, "closed new heads subscription");
                });
            }
            _ => return Err(anyhow::anyhow!("unimplemented")),
        }

        // TODO: do something with subscription_join_handle?

        let response = JsonRpcForwardedResponse::from_string(subscription_id, id);

        // TODO: make a `SubscriptonHandle(AbortHandle, JoinHandle)` struct?

        Ok((subscription_abort_handle, response))
    }

    pub fn get_balanced_rpcs(&self) -> &Web3Connections {
        &self.balanced_rpcs
    }

    pub fn get_private_rpcs(&self) -> &Web3Connections {
        &self.private_rpcs
    }

    pub fn get_active_requests(&self) -> &ActiveRequestsMap {
        &self.incoming_requests
    }

    /// send the request to the approriate RPCs
    /// TODO: dry this up
    #[instrument(skip_all)]
    pub async fn proxy_web3_rpc(
        &self,
        request: JsonRpcRequestEnum,
    ) -> anyhow::Result<JsonRpcForwardedResponseEnum> {
        // TODO: i don't always see this in the logs. why?
        debug!("Received request: {:?}", request);

        // even though we have timeouts on the requests to our backend providers,
        // we need a timeout for the incoming request so that delays from
        let max_time = Duration::from_secs(60);

        let response = match request {
            JsonRpcRequestEnum::Single(request) => JsonRpcForwardedResponseEnum::Single(
                timeout(max_time, self.proxy_web3_rpc_request(request)).await??,
            ),
            JsonRpcRequestEnum::Batch(requests) => JsonRpcForwardedResponseEnum::Batch(
                timeout(max_time, self.proxy_web3_rpc_requests(requests)).await??,
            ),
        };

        // TODO: i don't always see this in the logs. why?
        debug!("Forwarding response: {:?}", response);

        Ok(response)
    }

    // #[instrument(skip_all)]
    async fn proxy_web3_rpc_requests(
        &self,
        requests: Vec<JsonRpcRequest>,
    ) -> anyhow::Result<Vec<JsonRpcForwardedResponse>> {
        // TODO: we should probably change ethers-rs to support this directly
        // we cut up the request and send to potentually different servers. this could be a problem.
        // if the client needs consistent blocks, they should specify instead of assume batches work on the same
        // TODO: is spawning here actually slower?
        let num_requests = requests.len();
        let responses = join_all(
            requests
                .into_iter()
                .map(|request| self.proxy_web3_rpc_request(request))
                .collect::<Vec<_>>(),
        )
        .await;

        // TODO: i'm sure this could be done better with iterators
        let mut collected: Vec<JsonRpcForwardedResponse> = Vec::with_capacity(num_requests);
        for response in responses {
            collected.push(response?);
        }

        Ok(collected)
    }

    fn get_cached_response(
        &self,
        request: &JsonRpcRequest,
    ) -> (
        CacheKey,
        Result<JsonRpcForwardedResponse, &ResponseLrcCache>,
    ) {
        // TODO: inspect the request to pick the right cache
        // TODO: https://github.com/ethereum/web3.py/blob/master/web3/middleware/cache.py

        // TODO: Some requests should skip caching on the head_block_hash
        let head_block_hash = Some(self.balanced_rpcs.get_head_block_hash());

        // TODO: better key? benchmark this
        let key = (
            head_block_hash,
            request.method.clone(),
            request.params.clone().map(|x| x.to_string()),
        );

        if let Some(response) = self.response_cache.read().get(&key) {
            // TODO: emit a stat
            trace!("{:?} cache hit!", request);

            // TODO: can we make references work? maybe put them in an Arc?
            return (key, Ok(response.to_owned()));
        } else {
            // TODO: emit a stat
            trace!("{:?} cache miss!", request);
        }

        // TODO: multiple caches. if head_block_hash is None, have a persistent cache (disk backed?)
        let cache = &self.response_cache;

        (key, Err(cache))
    }

    // #[instrument(skip_all)]
    async fn proxy_web3_rpc_request(
        &self,
        request: JsonRpcRequest,
    ) -> anyhow::Result<JsonRpcForwardedResponse> {
        trace!("Received request: {:?}", request);

        // TODO: if eth_chainId or net_version, serve those without querying the backend

        // TODO: how much should we retry? probably with a timeout and not with a count like this
        // TODO: think more about this loop.
        // // TODO: add more to this span such as
        let span = info_span!("rpc_request");
        // let _enter = span.enter(); // DO NOT ENTER! we can't use enter across awaits! (clippy lint soon)
        match &request.method[..] {
            "admin_addPeer"
            | "admin_datadir"
            | "admin_startRPC"
            | "admin_startWS"
            | "admin_stopRPC"
            | "admin_stopWS"
            | "debug_chaindbCompact"
            | "debug_freezeClient"
            | "debug_goTrace"
            | "debug_mutexProfile"
            | "debug_setBlockProfileRate"
            | "debug_setGCPercent"
            | "debug_setHead"
            | "debug_setMutexProfileFraction"
            | "debug_standardTraceBlockToFile"
            | "debug_standardTraceBadBlockToFile"
            | "debug_startCPUProfile"
            | "debug_startGoTrace"
            | "debug_stopCPUProfile"
            | "debug_stopGoTrace"
            | "debug_writeBlockProfile"
            | "debug_writeMemProfile"
            | "debug_writeMutexProfile"
            | "les_addBalance"
            | "les_setClientParams"
            | "les_setDefaultParams"
            | "miner_setExtra"
            | "miner_setGasPrice"
            | "miner_start"
            | "miner_stop"
            | "miner_setEtherbase"
            | "miner_setGasLimit"
            | "personal_importRawKey"
            | "personal_listAccounts"
            | "personal_lockAccount"
            | "personal_newAccount"
            | "personal_unlockAccount"
            | "personal_sendTransaction"
            | "personal_sign"
            | "personal_ecRecover" => {
                // TODO: proper error code
                Err(anyhow::anyhow!("unimplemented"))
            }
            "eth_sendRawTransaction" => {
                // there are private rpcs configured and the request is eth_sendSignedTransaction. send to all private rpcs
                // TODO: think more about this lock. i think it won't actually help the herd. it probably makes it worse if we have a tight lag_limit
                self.private_rpcs
                    .try_send_all_upstream_servers(request)
                    .instrument(span)
                    .await
            }
            method => {
                // this is not a private transaction (or no private relays are configured)

                let (cache_key, response_cache) = match self.get_cached_response(&request) {
                    (cache_key, Ok(response)) => {
                        let _ = self.incoming_requests.remove(&cache_key);

                        return Ok(response);
                    }
                    (cache_key, Err(response_cache)) => (cache_key, response_cache),
                };

                // check if this request is already in flight
                // TODO: move this logic into an IncomingRequestHandler (ActiveRequestHandler has an rpc, but this won't)
                let (incoming_tx, incoming_rx) = watch::channel(true);
                let mut other_incoming_rx = None;
                match self.incoming_requests.entry(cache_key.clone()) {
                    DashMapEntry::Occupied(entry) => {
                        other_incoming_rx = Some(entry.get().clone());
                    }
                    DashMapEntry::Vacant(entry) => {
                        entry.insert(incoming_rx);
                    }
                }

                if let Some(mut other_incoming_rx) = other_incoming_rx {
                    // wait for the other request to finish. it might have finished successfully or with an error
                    trace!("{:?} waiting on in-flight request", request);

                    let _ = other_incoming_rx.changed().await;

                    // now that we've waited, lets check the cache again
                    if let Some(cached) = response_cache.read().get(&cache_key) {
                        let _ = self.incoming_requests.remove(&cache_key);
                        let _ = incoming_tx.send(false);

                        // TODO: emit a stat
                        trace!(
                            "{:?} cache hit after waiting for in-flight request!",
                            request
                        );

                        return Ok(cached.to_owned());
                    } else {
                        // TODO: emit a stat
                        trace!(
                            "{:?} cache miss after waiting for in-flight request!",
                            request
                        );
                    }
                }

                let response = match method {
                    "eth_getTransactionByHash" | "eth_getTransactionReceipt" => {
                        // TODO: try_send_all serially with retries instead of parallel
                        self.private_rpcs
                            .try_send_all_upstream_servers(request)
                            .await?
                    }
                    _ => {
                        // TODO: retries?
                        self.balanced_rpcs
                            .try_send_best_upstream_server(request)
                            .await?
                    }
                };

                // TODO: small race condidition here. parallel requests with the same query will both be saved to the cache
                let mut response_cache = response_cache.write();

                // TODO: cache the warp::reply to save us serializing every time
                response_cache.insert(cache_key.clone(), response.clone());
                if response_cache.len() >= RESPONSE_CACHE_CAP {
                    // TODO: this isn't an LRU. it's a "least recently created". does that have a fancy name? should we make it an lru? these caches only live for one block
                    response_cache.pop_front();
                }

                drop(response_cache);

                let _ = self.incoming_requests.remove(&cache_key);
                let _ = incoming_tx.send(false);

                Ok(response)
            }
        }
    }
}
