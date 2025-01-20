extern crate error_chain;
#[macro_use]
extern crate log;

extern crate electrs;

use electrs::new_index::transaction_update::TransactionUpdate;
use error_chain::ChainedError;
use std::fs::OpenOptions;
use std::io::Write;
use std::process;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use electrs::{
    config::Config,
    daemon::Daemon,
    electrum::RPC as ElectrumRPC,
    errors::*,
    metrics::Metrics,
    new_index::{precache, ChainQuery, FetchFrom, Indexer, Mempool, Query, Store},
    rest,
    signal::Waiter,
};

#[cfg(feature = "liquid")]
use electrs::elements::AssetRegistry;

fn fetch_from(config: &Config, store: &Store) -> FetchFrom {
    let mut jsonrpc_import = config.jsonrpc_import;
    if !jsonrpc_import {
        // switch over to jsonrpc after the initial sync is done
        jsonrpc_import = store.done_initial_sync();
    }

    if jsonrpc_import {
        // slower, uses JSONRPC (good for incremental updates)
        FetchFrom::Bitcoind
    } else {
        // faster, uses blk*.dat files (good for initial indexing)
        FetchFrom::BlkFiles
    }
}

fn append_to_file(filename: &str, data: &str) -> std::io::Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(filename)?;
    writeln!(file, "{}", data)?;
    Ok(())
}

fn run_server(config: Arc<Config>) -> Result<()> {
    let signal = Waiter::start();
    let metrics = Metrics::new(config.monitoring_addr);
    metrics.start();

    let daemon = Arc::new(Daemon::new(
        config.daemon_dir.clone(),
        config.blocks_dir.clone(),
        config.daemon_rpc_addr,
        config.cookie_getter(),
        config.network_type,
        config.magic,
        signal.clone(),
        &metrics,
    )?);
    let store = Arc::new(Store::open(&config.db_path.join("newindex"), &config));
    let mut indexer = Indexer::open(
        Arc::clone(&store),
        fetch_from(&config, &store),
        &config,
        &metrics,
    );
    let mut transaction_update = TransactionUpdate::default();
    let (mut tip, updated_txns) = indexer.update(&daemon)?;
    transaction_update.update_block(updated_txns);

    let chain = Arc::new(ChainQuery::new(
        Arc::clone(&store),
        Arc::clone(&daemon),
        &config,
        &metrics,
    ));

    let mempool = Arc::new(RwLock::new(Mempool::new(
        Arc::clone(&chain),
        &metrics,
        Arc::clone(&config),
    )));
    loop {
        match Mempool::update(&mempool, &daemon) {
            Ok(_) => break,
            Err(e) => {
                warn!(
                    "Error performing initial mempool update, trying again in 5 seconds: {}",
                    e.display_chain()
                );
                signal.wait(Duration::from_secs(5), false)?;
            }
        }
    }

    #[cfg(feature = "liquid")]
    let asset_db = config.asset_db_path.as_ref().map(|db_dir| {
        let asset_db = Arc::new(RwLock::new(AssetRegistry::new(db_dir.clone())));
        AssetRegistry::spawn_sync(asset_db.clone());
        asset_db
    });

    let query = Arc::new(Query::new(
        Arc::clone(&chain),
        Arc::clone(&mempool),
        Arc::clone(&daemon),
        Arc::clone(&config),
        #[cfg(feature = "liquid")]
        asset_db,
    ));

    // TODO: configuration for which servers to start
    let rest_server = rest::start(Arc::clone(&config), Arc::clone(&query), &metrics);
    let electrum_server = ElectrumRPC::start(Arc::clone(&config), Arc::clone(&query), &metrics);

    if let Some(ref precache_file) = config.precache_scripts {
        let precache_scripthashes = precache::scripthashes_from_file(precache_file.to_string())
            .expect("cannot load scripts to precache");
        precache::precache(
            Arc::clone(&chain),
            precache_scripthashes,
            config.precache_threads,
        );
    }

    loop {
        if let Err(err) = signal.wait(Duration::from_millis(config.main_loop_delay), true) {
            info!("stopping server: {}", err);

            electrs::util::spawn_thread("shutdown-thread-checker", || {
                let mut counter = 40;
                let interval_ms = 500;

                while counter > 0 {
                    electrs::util::with_spawned_threads(|threads| {
                        debug!("Threads during shutdown: {:?}", threads);
                    });
                    std::thread::sleep(std::time::Duration::from_millis(interval_ms));
                    counter -= 1;
                }
            });

            rest_server.stop();
            // the electrum server is stopped when dropped
            break;
        }

        // Index new blocks
        let current_tip = daemon.getbestblockhash()?;
        if current_tip != tip {
            let (new_tip, new_updated_txns) = indexer.update(&daemon)?;
            tip = new_tip;
            transaction_update.update_block(new_updated_txns);
        };

        // Update mempool
        let mempool_update = Mempool::update(&mempool, &daemon);
        match mempool_update {
            Ok(mempool_txns) => {
                transaction_update.update_mempool(mempool_txns);
            }
            Err(e) => {
                warn!(
                    "Error updating mempool, skipping mempool update: {}",
                    e.display_chain()
                );
            }
        }

        let categorized = transaction_update.categorize();

        if !categorized.new_in_mempool_only.is_empty() {
            if let Err(e) = append_to_file(
                "new_in_mempool_only.txt",
                format!("{:?}", categorized.new_in_mempool_only).as_str(),
            ) {
                warn!("Failed to append to file: {}", e);
            }
            warn!("new txns in mempool: {:?}", categorized.new_in_mempool_only);
        }

        if !categorized.new_block_only.is_empty() {
            if let Err(e) = append_to_file(
                "new_block_only.txt",
                format!("{:?}", categorized.new_block_only).as_str(),
            ) {
                warn!("Failed to append to file: {}", e);
            }
            warn!(
                "new txns straight from block: {:?}",
                categorized.new_block_only
            );
        }

        if !categorized.reorged_out_entirely.is_empty() {
            if let Err(e) = append_to_file(
                "reorged_out_entirely.txt",
                format!("{:?}", categorized.reorged_out_entirely).as_str(),
            ) {
                warn!("Failed to append to file: {}", e);
            }
            warn!(
                "txns reorged out entirely: {:?}",
                categorized.reorged_out_entirely
            );
        }

        if !categorized.mempool_rbf_or_evicted.is_empty() {
            if let Err(e) = append_to_file(
                "mempool_rbf_or_evicted.txt",
                format!("{:?}", categorized.mempool_rbf_or_evicted).as_str(),
            ) {
                warn!("Failed to append to file: {}", e);
            }
            warn!(
                "mempool rbf or evicted: {:?}",
                categorized.mempool_rbf_or_evicted
            );
        }

        let final_update = transaction_update.categorize_to_change_set();
        if !final_update.added.is_empty() {
            if let Err(e) =
                append_to_file("new_txns.txt", format!("{:?}", final_update.added).as_str())
            {
                warn!("Failed to append to file: {}", e);
            }
            warn!("new txns: {:?}", final_update.added);
        }

        if !final_update.removed.is_empty() {
            if let Err(e) = append_to_file(
                "replaced_txns.txt",
                format!("{:?}", final_update.removed).as_str(),
            ) {
                warn!("Failed to append to file: {}", e);
            }
            warn!("replaced txns: {:?}", final_update.removed);
        }

        // Update subscribed clients
        electrum_server.notify(final_update.into());

        // Clear the transaction update
        transaction_update.reset();
    }
    info!("server stopped");
    Ok(())
}

fn main() {
    let config = Arc::new(Config::from_args());
    if let Err(e) = run_server(config) {
        error!("server failed: {}", e.display_chain());
        process::exit(1);
    }
    electrs::util::with_spawned_threads(|threads| {
        debug!("Threads before closing: {:?}", threads);
    });
}
