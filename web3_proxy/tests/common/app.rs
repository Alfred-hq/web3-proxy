use super::{anvil::TestAnvil, mysql::TestMysql};
use ethers::{
    prelude::{Http, Provider},
    signers::LocalWallet,
    types::Address,
};
use hashbrown::HashMap;
use migration::sea_orm::DatabaseConnection;
use parking_lot::Mutex;
use serde_json::json;
use std::{
    env,
    str::FromStr,
    sync::atomic::{AtomicU16, Ordering},
};
use std::{sync::Arc, time::Duration};
use tokio::{
    sync::{
        broadcast::{self, error::SendError},
        mpsc, oneshot,
    },
    task::JoinHandle,
    time::{sleep, Instant},
};
use tracing::info;
use web3_proxy::{
    config::{AppConfig, TopConfig, Web3RpcConfig},
    stats::FlushedStats,
    sub_commands::ProxydSubCommand,
};

pub struct TestApp {
    /// anvil shuts down when this guard is dropped.
    pub anvil: TestAnvil,

    /// connection to anvil.
    pub anvil_provider: Provider<Http>,

    /// keep track of the database so it can be stopped on drop
    pub db: Option<TestMysql>,

    /// spawn handle for the proxy.
    pub proxy_handle: Mutex<Option<JoinHandle<anyhow::Result<()>>>>,

    /// connection to the proxy that is connected to anil.
    pub proxy_provider: Provider<Http>,

    /// tell the app to flush stats to the database
    flush_stat_buffer_sender: mpsc::Sender<oneshot::Sender<FlushedStats>>,

    /// tell the app to shut down (use `self.stop()`).
    shutdown_sender: broadcast::Sender<()>,
}

impl TestApp {
    pub async fn spawn(anvil: TestAnvil, db: Option<TestMysql>) -> Self {
        let chain_id = anvil.instance.chain_id();
        let num_workers = 2;

        // TODO: move basic setup into a test fixture
        let path = env::var("PATH").unwrap();

        info!(%path);

        let anvil_provider = Provider::<Http>::try_from(anvil.instance.endpoint()).unwrap();

        let db_url = db.as_ref().map(|x| x.url.clone());

        // make a test TopConfig
        // TODO: test influx
        // TODO: test redis
        let app_config: AppConfig = serde_json::from_value(json!({
            "chain_id": chain_id,
            "db_url": db_url,
            "default_user_max_requests_per_period": Some(6_000_000),
            "deposit_factory_contract": Address::from_str(
                "4e3BC2054788De923A04936C6ADdB99A05B0Ea36",
            )
            .ok(),
            "min_sum_soft_limit": 1,
            "min_synced_rpcs": 1,
            "public_requests_per_period": Some(1_000_000),
            "response_cache_max_bytes": 10_u64.pow(7),
        }))
        .unwrap();

        let top_config = TopConfig {
            app: app_config,
            balanced_rpcs: HashMap::from([(
                "anvil".to_string(),
                Web3RpcConfig {
                    http_url: Some(anvil.instance.endpoint()),
                    ws_url: Some(anvil.instance.ws_endpoint()),
                    ..Default::default()
                },
            )]),
            private_rpcs: None,
            bundler_4337_rpcs: None,
            extra: Default::default(),
        };

        let (shutdown_sender, _shutdown_receiver) = broadcast::channel(1);

        let frontend_port_arc = Arc::new(AtomicU16::new(0));
        let prometheus_port_arc = Arc::new(AtomicU16::new(0));

        let (flush_stat_buffer_sender, flush_stat_buffer_receiver) = mpsc::channel(1);

        // spawn the app
        // TODO: spawn in a thread so we can run from non-async tests and so the Drop impl can wait for it to stop
        let handle = {
            tokio::spawn(ProxydSubCommand::_main(
                top_config,
                None,
                frontend_port_arc.clone(),
                prometheus_port_arc,
                num_workers,
                shutdown_sender.clone(),
                flush_stat_buffer_sender.clone(),
                flush_stat_buffer_receiver,
            ))
        };

        let mut frontend_port = frontend_port_arc.load(Ordering::Relaxed);
        let start = Instant::now();
        while frontend_port == 0 {
            // we have to give it some time because it might have to do migrations
            if start.elapsed() > Duration::from_secs(10) {
                panic!("took too long to start!");
            }

            sleep(Duration::from_millis(10)).await;
            frontend_port = frontend_port_arc.load(Ordering::Relaxed);
        }

        let proxy_endpoint = format!("http://127.0.0.1:{}", frontend_port);

        let proxy_provider = Provider::<Http>::try_from(proxy_endpoint).unwrap();

        Self {
            anvil,
            anvil_provider,
            db,
            proxy_handle: Mutex::new(Some(handle)),
            proxy_provider,
            flush_stat_buffer_sender,
            shutdown_sender,
        }
    }

    #[allow(unused)]
    pub fn db_conn(&self) -> &DatabaseConnection {
        self.db.as_ref().unwrap().conn()
    }

    #[allow(unused)]
    pub async fn flush_stats(&self) -> anyhow::Result<FlushedStats> {
        let (tx, rx) = oneshot::channel();

        self.flush_stat_buffer_sender.send(tx).await?;

        let x = rx.await?;

        Ok(x)
    }

    pub fn stop(&self) -> Result<usize, SendError<()>> {
        self.shutdown_sender.send(())
    }

    #[allow(unused)]
    pub async fn wait(&self) {
        let _ = self.stop();

        // TODO: lock+take feels weird, but it works
        let handle = self.proxy_handle.lock().take();

        if let Some(handle) = handle {
            info!("waiting for the app to stop...");
            handle.await.unwrap().unwrap();
        }
    }

    #[allow(unused)]
    pub fn wallet(&self, id: usize) -> LocalWallet {
        self.anvil.instance.keys()[id].clone().into()
    }
}

impl Drop for TestApp {
    fn drop(&mut self) {
        let _ = self.stop();

        // TODO: do we care about waiting for it to stop? it will slow our tests down so we probably only care about waiting in some tests
    }
}
