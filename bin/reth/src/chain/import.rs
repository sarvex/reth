use crate::{
    chain::FileClient,
    dirs::{ConfigPath, DbPath, ImportPath, PlatformPath},
    node::handle_events,
    utils::chainspec::chain_spec_value_parser,
};
use clap::Parser;
use eyre::Context;
use futures::StreamExt;
use reth_consensus::BeaconConsensus;
use reth_db::mdbx::{Env, WriteMap};
use reth_downloaders::{bodies, headers};
use reth_interfaces::consensus::Consensus;
use reth_primitives::ChainSpec;
use reth_staged_sync::{
    utils::init::{init_db, init_genesis},
    Config,
};
use reth_stages::{
    sets::{OfflineStages, OnlineStages},
    stages::{ExecutionStage, SenderRecoveryStage, TotalDifficultyStage},
    Pipeline, StageSet,
};
use std::sync::Arc;
use tracing::info;

#[derive(Debug, Parser)]
pub struct ImportCommand {
    /// The path to the configuration file to use.
    #[arg(long, value_name = "FILE", verbatim_doc_comment, default_value_t)]
    config: PlatformPath<ConfigPath>,

    /// The path to the database folder.
    ///
    /// Defaults to the OS-specific data directory:
    ///
    /// - Linux: `$XDG_DATA_HOME/reth/db` or `$HOME/.local/share/reth/db`
    /// - Windows: `{FOLDERID_RoamingAppData}/reth/db`
    /// - macOS: `$HOME/Library/Application Support/reth/db`
    #[arg(long, value_name = "PATH", verbatim_doc_comment, default_value_t)]
    db: PlatformPath<DbPath>,

    /// The chain this node is running.
    ///
    /// Possible values are either a built-in chain or the path to a chain specification file.
    ///
    /// Built-in chains:
    /// - mainnet
    /// - goerli
    /// - sepolia
    #[arg(
        long,
        value_name = "CHAIN_OR_PATH",
        verbatim_doc_comment,
        default_value = "mainnet",
        value_parser = chain_spec_value_parser
    )]
    chain: ChainSpec,

    /// The file to import.
    #[arg(long, value_name = "IMPORT_PATH", verbatim_doc_comment, default_value_t)]
    path: PlatformPath<ImportPath>,
}

impl ImportCommand {
    pub async fn execute(&self) -> eyre::Result<()> {
        info!(target: "reth::cli", "reth import starting");

        let config: Config = self.load_config()?;
        info!(target: "reth::cli", path = %self.db, "Configuration loaded");

        info!(target: "reth::cli", path = %self.db, "Opening database");
        let db = Arc::new(init_db(&self.db)?);
        info!(target: "reth::cli", "Database opened");

        // self.start_metrics_endpoint()?;

        init_genesis(db.clone(), self.chain.clone())?;

        let consensus = self.init_consensus()?;
        info!(target: "reth::cli", "Consensus engine initialized");

        // create a new FileClient
        let file_client = Arc::new(FileClient::new(&self.path).await?);

        // construct downloaders and start pipeline
        let mut pipeline = self.build_pipeline(&config, &file_client, &consensus, &db).await?;

        tokio::spawn(handle_events(pipeline.events().map(Into::into)));

        // Run pipeline
        info!(target: "reth::cli", "Starting sync pipeline");
        pipeline.run(db.clone()).await?;

        info!(target: "reth::cli", "Finishing up");
        Ok(())
    }

    async fn build_pipeline(
        &self,
        config: &Config,
        file_client: &Arc<FileClient>,
        consensus: &Arc<dyn Consensus>,
        db: &Arc<Env<WriteMap>>,
    ) -> eyre::Result<Pipeline<Env<WriteMap>, Arc<FileClient>>> {
        let header_downloader =
            self.spawn_headers_downloader(config, consensus, &file_client.clone());
        let body_downloader =
            self.spawn_bodies_downloader(config, consensus, &file_client.clone(), db);
        let stage_conf = &config.stages;

        let pipeline = Pipeline::builder()
            .with_sync_state_updater(file_client.clone())
            .add_stages(
                OnlineStages::new(consensus.clone(), header_downloader, body_downloader).set(
                    TotalDifficultyStage {
                        commit_threshold: stage_conf.total_difficulty.commit_threshold,
                    },
                ),
            )
            .add_stages(
                OfflineStages::default()
                    .set(SenderRecoveryStage {
                        batch_size: stage_conf.sender_recovery.batch_size,
                        commit_threshold: stage_conf.execution.commit_threshold,
                    })
                    .set(ExecutionStage {
                        chain_spec: self.chain.clone(),
                        commit_threshold: stage_conf.execution.commit_threshold,
                    }),
            )
            .build();

        Ok(pipeline)
    }

    fn init_consensus(&self) -> eyre::Result<Arc<dyn Consensus>> {
        let beacon_consensus = Arc::new(BeaconConsensus::new(self.chain.clone()));

        // TODO: the node command requires a tip update, so we need to figure out how to do that
        // with only a file
        todo!()
    }

    fn spawn_headers_downloader(
        &self,
        config: &Config,
        consensus: &Arc<dyn Consensus>,
        file_client: &Arc<FileClient>,
    ) -> reth_downloaders::headers::task::TaskDownloader {
        // TODO: how to deal with the fact that this is a reverse downloader, and the blocks are
        // written forwards in the file? RLP can only be parsed forwards
        // do we need a forward downloader?
        let headers_conf = &config.stages.headers;
        headers::task::TaskDownloader::spawn(
            headers::linear::LinearDownloadBuilder::default()
                .request_limit(headers_conf.downloader_batch_size)
                .stream_batch_size(headers_conf.commit_threshold as usize)
                .build(consensus.clone(), file_client.clone()),
        )
    }

    fn spawn_bodies_downloader(
        &self,
        config: &Config,
        consensus: &Arc<dyn Consensus>,
        file_client: &Arc<FileClient>,
        db: &Arc<Env<WriteMap>>,
    ) -> reth_downloaders::bodies::task::TaskDownloader {
        let bodies_conf = &config.stages.bodies;
        bodies::task::TaskDownloader::spawn(
            bodies::concurrent::ConcurrentDownloaderBuilder::default()
                .with_stream_batch_size(bodies_conf.downloader_stream_batch_size)
                .with_request_limit(bodies_conf.downloader_request_limit)
                .with_max_buffered_responses(bodies_conf.downloader_max_buffered_responses)
                .with_concurrent_requests_range(
                    bodies_conf.downloader_min_concurrent_requests..=
                        bodies_conf.downloader_max_concurrent_requests,
                )
                .build(file_client.clone(), consensus.clone(), db.clone()),
        )
    }

    fn load_config(&self) -> eyre::Result<Config> {
        confy::load_path::<Config>(&self.config).wrap_err("Could not load config")
    }
}
