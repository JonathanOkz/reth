//! Main node command for launching a node

use crate::launcher::Launcher;
use clap::{value_parser, Args, Parser};
use reth_chainspec::{EthChainSpec, EthereumHardforks};
use reth_cli::chainspec::ChainSpecParser;
use reth_cli_runner::CliContext;
use reth_db::init_db;
use reth_db::mdbx::CopyFlags;
use reth_node_builder::NodeBuilder;
use reth_node_core::{
    args::{
        DatabaseArgs, DatadirArgs, DebugArgs, DevArgs, EngineArgs, EraArgs, MetricArgs,
        NetworkArgs, PayloadBuilderArgs, PruningArgs, RpcServerArgs, StaticFilesArgs, TxPoolArgs,
    },
    node_config::NodeConfig,
    version,
};
use std::{ffi::OsString, fmt, path::PathBuf, sync::Arc};
use tracing::warn;

use crate::gcs_snapshot;

/// Start the node
#[derive(Debug, Parser)]
pub struct NodeCommand<C: ChainSpecParser, Ext: clap::Args + fmt::Debug = NoArgs> {
    /// The path to the configuration file to use.
    #[arg(long, value_name = "FILE", verbatim_doc_comment)]
    pub config: Option<PathBuf>,

    /// The chain this node is running.
    ///
    /// Possible values are either a built-in chain or the path to a chain specification file.
    #[arg(
        long,
        value_name = "CHAIN_OR_PATH",
        long_help = C::help_message(),
        default_value = C::default_value(),
        default_value_if("dev", "true", "dev"),
        value_parser = C::parser(),
        required = false,
    )]
    pub chain: Arc<C::ChainSpec>,

    /// Prometheus metrics configuration.
    #[command(flatten)]
    pub metrics: MetricArgs,

    /// Add a new instance of a node.
    ///
    /// Configures the ports of the node to avoid conflicts with the defaults.
    /// This is useful for running multiple nodes on the same machine.
    ///
    /// Max number of instances is 200. It is chosen in a way so that it's not possible to have
    /// port numbers that conflict with each other.
    ///
    /// Changes to the following port numbers:
    /// - `DISCOVERY_PORT`: default + `instance` - 1
    /// - `AUTH_PORT`: default + `instance` * 100 - 100
    /// - `HTTP_RPC_PORT`: default - `instance` + 1
    /// - `WS_RPC_PORT`: default + `instance` * 2 - 2
    /// - `IPC_PATH`: default + `-instance`
    #[arg(long, value_name = "INSTANCE", global = true, value_parser = value_parser!(u16).range(1..=200))]
    pub instance: Option<u16>,

    /// Sets all ports to unused, allowing the OS to choose random unused ports when sockets are
    /// bound.
    ///
    /// Mutually exclusive with `--instance`.
    #[arg(long, conflicts_with = "instance", global = true)]
    pub with_unused_ports: bool,

    /// All datadir related arguments
    #[command(flatten)]
    pub datadir: DatadirArgs,

    /// All networking related arguments
    #[command(flatten)]
    pub network: NetworkArgs,

    /// All rpc related arguments
    #[command(flatten)]
    pub rpc: RpcServerArgs,

    /// All txpool related arguments with --txpool prefix
    #[command(flatten)]
    pub txpool: TxPoolArgs,

    /// All payload builder related arguments
    #[command(flatten)]
    pub builder: PayloadBuilderArgs,

    /// All debug related arguments with --debug prefix
    #[command(flatten)]
    pub debug: DebugArgs,

    /// All database related arguments
    #[command(flatten)]
    pub db: DatabaseArgs,

    /// All dev related arguments with --dev prefix
    #[command(flatten)]
    pub dev: DevArgs,

    /// All pruning related arguments
    #[command(flatten)]
    pub pruning: PruningArgs,

    /// Engine cli arguments
    #[command(flatten, next_help_heading = "Engine")]
    pub engine: EngineArgs,

    /// All ERA related arguments with --era prefix
    #[command(flatten, next_help_heading = "ERA")]
    pub era: EraArgs,

    /// All static files related arguments
    #[command(flatten, next_help_heading = "Static Files")]
    pub static_files: StaticFilesArgs,

    /// Additional cli arguments
    #[command(flatten, next_help_heading = "Extension")]
    pub ext: Ext,

    #[command(flatten, next_help_heading = "Snapshot")]
    pub snapshot: SnapshotArgs,
}

#[derive(Debug, Args, Default, Clone)]
pub struct SnapshotArgs {
    #[arg(long = "snapshot.enabled", env = "RETH_SNAPSHOT_ENABLED", default_value_t = false)]
    pub snapshot_enabled: bool,

    #[arg(long = "snapshot.gcs-bucket", env = "RETH_SNAPSHOT_GCS_BUCKET")]
    pub gcs_bucket: Option<String>,

    #[arg(long = "snapshot.gcs-object", env = "RETH_SNAPSHOT_GCS_OBJECT")]
    pub gcs_object: Option<String>,
}

impl<C: ChainSpecParser> NodeCommand<C> {
    /// Parsers only the default CLI arguments
    pub fn parse_args() -> Self {
        Self::parse()
    }

    /// Parsers only the default [`NodeCommand`] arguments from the given iterator
    pub fn try_parse_args_from<I, T>(itr: I) -> Result<Self, clap::error::Error>
    where
        I: IntoIterator<Item = T>,
        T: Into<OsString> + Clone,
    {
        Self::try_parse_from(itr)
    }
}

impl<C, Ext> NodeCommand<C, Ext>
where
    C: ChainSpecParser,
    C::ChainSpec: EthChainSpec + EthereumHardforks,
    Ext: clap::Args + fmt::Debug,
{
    /// Launches the node
    ///
    /// This transforms the node command into a node config and launches the node using the given
    /// launcher.
    pub async fn execute<L>(self, ctx: CliContext, launcher: L) -> eyre::Result<()>
    where
        L: Launcher<C, Ext>,
    {
        tracing::info!(target: "reth::cli", version = ?version::version_metadata().short_version, "Starting {}",  version::version_metadata().name_client);

        let Self {
            datadir,
            config,
            chain,
            metrics,
            instance,
            with_unused_ports,
            network,
            rpc,
            txpool,
            builder,
            debug,
            db,
            dev,
            pruning,
            engine,
            era,
            static_files,
            ext,
            snapshot,
        } = self;

        // set up node config
        let mut node_config = NodeConfig {
            datadir,
            config,
            chain,
            metrics,
            instance,
            network,
            rpc,
            txpool,
            builder,
            debug,
            db,
            dev,
            pruning,
            engine,
            era,
            static_files,
        };

        let data_dir = node_config.datadir();
        let db_path = data_dir.db();

        let snapshot_http_client = if snapshot.snapshot_enabled && snapshot.gcs_bucket.is_some() {
            match gcs_snapshot::default_client() {
                Ok(client) => Some(client),
                Err(err) => {
                    warn!(target: "reth::cli", ?err, "failed to create http client for snapshots");
                    None
                }
            }
        } else {
            None
        };

        if snapshot.snapshot_enabled {
            if let (Some(bucket), Some(client)) =
                (snapshot.gcs_bucket.as_deref(), snapshot_http_client.as_ref())
            {
                let object = snapshot
                    .gcs_object
                    .clone()
                    .unwrap_or_else(|| format!("{}/mdbx.dat", node_config.chain.chain_id()));
                let db_file = db_path.join("mdbx.dat");
                match gcs_snapshot::download_to_path(client, bucket, &object, &db_file).await {
                    Ok(true) => {
                        tracing::info!(target: "reth::cli", bucket = %bucket, object = %object, "snapshot restored");
                        let _ = tokio::fs::remove_file(db_path.join("mdbx.lck")).await;
                    }
                    Ok(false) => {
                        tracing::info!(target: "reth::cli", bucket = %bucket, object = %object, "no snapshot found, skipping restore");
                    }
                    Err(err) => {
                        warn!(target: "reth::cli", ?err, "snapshot restore failed, continuing without restore");
                    }
                }
            } else {
                warn!(target: "reth::cli", "snapshot enabled but no GCS bucket configured; skipping snapshot restore");
            }
        }

        tracing::info!(target: "reth::cli", path = ?db_path, "Opening database");
        let database = Arc::new(init_db(db_path.clone(), db.database_args())?.with_metrics());

        if snapshot.snapshot_enabled {
            if let (Some(bucket), Some(client)) = (snapshot.gcs_bucket.clone(), snapshot_http_client)
            {
                let object = snapshot
                    .gcs_object
                    .clone()
                    .unwrap_or_else(|| format!("{}/mdbx.dat", node_config.chain.chain_id()));
                let database = database.clone();
                ctx.task_executor.spawn_critical_with_graceful_shutdown_signal(
                    "mdbx-snapshot-gcs",
                    move |shutdown| async move {
                        let guard = shutdown.await;

                    let tmp_path = std::env::temp_dir().join(format!(
                        "reth-mdbx-snapshot-{}.dat",
                        std::process::id()
                    ));
                    let _ = tokio::fs::remove_file(&tmp_path).await;

                    let mut flags = CopyFlags::DONT_FLUSH;
                    if std::env::var("RETH_SNAPSHOT_MDBX_THROTTLE_MVCC").ok().as_deref() == Some("1") {
                        flags |= CopyFlags::THROTTLE_MVCC;
                    }

                    let tmp_path_for_snapshot = tmp_path.clone();
                    let db_for_snapshot = database.clone();
                    let flags_for_snapshot = flags;
                    let snapshot_res = tokio::task::spawn_blocking(move || {
                        db_for_snapshot
                            .snapshot_to_path(&tmp_path_for_snapshot, flags_for_snapshot)
                            .map_err(|e| e.to_string())
                    })
                    .await;

                    match snapshot_res {
                        Ok(Ok(())) => {}
                        Ok(Err(err)) => {
                            warn!(target: "reth::cli", %err, "failed to snapshot mdbx");
                            let _ = tokio::fs::remove_file(&tmp_path).await;
                            drop(guard);
                            return
                        }
                        Err(err) => {
                            warn!(target: "reth::cli", %err, "snapshot task join error");
                            let _ = tokio::fs::remove_file(&tmp_path).await;
                            drop(guard);
                            return
                        }
                    }

                    if let Err(err) = gcs_snapshot::upload_from_path(&client, &bucket, &object, &tmp_path).await {
                        warn!(target: "reth::cli", ?err, "failed to upload snapshot to gcs");
                    } else {
                        tracing::info!(target: "reth::cli", bucket = %bucket, object = %object, "snapshot uploaded");
                    }

                        let _ = tokio::fs::remove_file(&tmp_path).await;
                        drop(guard);
                    }
                );
            } else {
                warn!(target: "reth::cli", "snapshot enabled but no GCS bucket configured; skipping snapshot backup");
            }
        }

        if with_unused_ports {
            node_config = node_config.with_unused_ports();
        }

        let builder = NodeBuilder::new(node_config)
            .with_database(database)
            .with_launch_context(ctx.task_executor);

        launcher.entrypoint(builder, ext).await
    }
}

impl<C: ChainSpecParser, Ext: clap::Args + fmt::Debug> NodeCommand<C, Ext> {
    /// Returns the underlying chain being used to run this command
    pub fn chain_spec(&self) -> Option<&Arc<C::ChainSpec>> {
        Some(&self.chain)
    }
}

/// No Additional arguments
#[derive(Debug, Clone, Copy, Default, Args)]
#[non_exhaustive]
pub struct NoArgs;

#[cfg(test)]
mod tests {
    use super::*;
    use reth_discv4::DEFAULT_DISCOVERY_PORT;
    use reth_ethereum_cli::chainspec::{EthereumChainSpecParser, SUPPORTED_CHAINS};
    use std::{
        net::{IpAddr, Ipv4Addr, SocketAddr},
        path::Path,
    };

    #[test]
    fn parse_help_node_command() {
        let err = NodeCommand::<EthereumChainSpecParser>::try_parse_args_from(["reth", "--help"])
            .unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayHelp);
    }

    #[test]
    fn parse_common_node_command_chain_args() {
        for chain in SUPPORTED_CHAINS {
            let args: NodeCommand<EthereumChainSpecParser> =
                NodeCommand::parse_from(["reth", "--chain", chain]);
            assert_eq!(args.chain.chain, chain.parse::<reth_chainspec::Chain>().unwrap());
        }
    }

    #[test]
    fn parse_discovery_addr() {
        let cmd: NodeCommand<EthereumChainSpecParser> =
            NodeCommand::try_parse_args_from(["reth", "--discovery.addr", "127.0.0.1"]).unwrap();
        assert_eq!(cmd.network.discovery.addr, IpAddr::V4(Ipv4Addr::LOCALHOST));
    }

    #[test]
    fn parse_addr() {
        let cmd: NodeCommand<EthereumChainSpecParser> = NodeCommand::try_parse_args_from([
            "reth",
            "--discovery.addr",
            "127.0.0.1",
            "--addr",
            "127.0.0.1",
        ])
        .unwrap();
        assert_eq!(cmd.network.discovery.addr, IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert_eq!(cmd.network.addr, IpAddr::V4(Ipv4Addr::LOCALHOST));
    }

    #[test]
    fn parse_discovery_port() {
        let cmd: NodeCommand<EthereumChainSpecParser> =
            NodeCommand::try_parse_args_from(["reth", "--discovery.port", "300"]).unwrap();
        assert_eq!(cmd.network.discovery.port, 300);
    }

    #[test]
    fn parse_port() {
        let cmd: NodeCommand<EthereumChainSpecParser> =
            NodeCommand::try_parse_args_from(["reth", "--discovery.port", "300", "--port", "99"])
                .unwrap();
        assert_eq!(cmd.network.discovery.port, 300);
        assert_eq!(cmd.network.port, 99);
    }

    #[test]
    fn parse_metrics_port() {
        let cmd: NodeCommand<EthereumChainSpecParser> =
            NodeCommand::try_parse_args_from(["reth", "--metrics", "9001"]).unwrap();
        assert_eq!(
            cmd.metrics.prometheus,
            Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 9001))
        );

        let cmd: NodeCommand<EthereumChainSpecParser> =
            NodeCommand::try_parse_args_from(["reth", "--metrics", ":9001"]).unwrap();
        assert_eq!(
            cmd.metrics.prometheus,
            Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 9001))
        );

        let cmd: NodeCommand<EthereumChainSpecParser> =
            NodeCommand::try_parse_args_from(["reth", "--metrics", "localhost:9001"]).unwrap();
        assert_eq!(
            cmd.metrics.prometheus,
            Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 9001))
        );
    }

    #[test]
    fn parse_config_path() {
        let cmd: NodeCommand<EthereumChainSpecParser> =
            NodeCommand::try_parse_args_from(["reth", "--config", "my/path/to/reth.toml"]).unwrap();
        // always store reth.toml in the data dir, not the chain specific data dir
        let data_dir = cmd.datadir.resolve_datadir(cmd.chain.chain);
        let config_path = cmd.config.unwrap_or_else(|| data_dir.config());
        assert_eq!(config_path, Path::new("my/path/to/reth.toml"));

        let cmd: NodeCommand<EthereumChainSpecParser> =
            NodeCommand::try_parse_args_from(["reth"]).unwrap();

        // always store reth.toml in the data dir, not the chain specific data dir
        let data_dir = cmd.datadir.resolve_datadir(cmd.chain.chain);
        let config_path = cmd.config.clone().unwrap_or_else(|| data_dir.config());
        let end = format!("{}/reth.toml", SUPPORTED_CHAINS[0]);
        assert!(config_path.ends_with(end), "{:?}", cmd.config);
    }

    #[test]
    fn parse_db_path() {
        let cmd: NodeCommand<EthereumChainSpecParser> =
            NodeCommand::try_parse_args_from(["reth"]).unwrap();
        let data_dir = cmd.datadir.resolve_datadir(cmd.chain.chain);

        let db_path = data_dir.db();
        let end = format!("reth/{}/db", SUPPORTED_CHAINS[0]);
        assert!(db_path.ends_with(end), "{:?}", cmd.config);

        let cmd: NodeCommand<EthereumChainSpecParser> =
            NodeCommand::try_parse_args_from(["reth", "--datadir", "my/custom/path"]).unwrap();
        let data_dir = cmd.datadir.resolve_datadir(cmd.chain.chain);

        let db_path = data_dir.db();
        assert_eq!(db_path, Path::new("my/custom/path/db"));
    }

    #[test]
    fn parse_instance() {
        let mut cmd: NodeCommand<EthereumChainSpecParser> = NodeCommand::parse_from(["reth"]);
        cmd.rpc.adjust_instance_ports(cmd.instance);
        cmd.network.port = DEFAULT_DISCOVERY_PORT;
        // check rpc port numbers
        assert_eq!(cmd.rpc.auth_port, 8551);
        assert_eq!(cmd.rpc.http_port, 8545);
        assert_eq!(cmd.rpc.ws_port, 8546);
        // check network listening port number
        assert_eq!(cmd.network.port, 30303);

        let mut cmd: NodeCommand<EthereumChainSpecParser> =
            NodeCommand::parse_from(["reth", "--instance", "2"]);
        cmd.rpc.adjust_instance_ports(cmd.instance);
        cmd.network.port = DEFAULT_DISCOVERY_PORT + 2 - 1;
        // check rpc port numbers
        assert_eq!(cmd.rpc.auth_port, 8651);
        assert_eq!(cmd.rpc.http_port, 8544);
        assert_eq!(cmd.rpc.ws_port, 8548);
        // check network listening port number
        assert_eq!(cmd.network.port, 30304);

        let mut cmd: NodeCommand<EthereumChainSpecParser> =
            NodeCommand::parse_from(["reth", "--instance", "3"]);
        cmd.rpc.adjust_instance_ports(cmd.instance);
        cmd.network.port = DEFAULT_DISCOVERY_PORT + 3 - 1;
        // check rpc port numbers
        assert_eq!(cmd.rpc.auth_port, 8751);
        assert_eq!(cmd.rpc.http_port, 8543);
        assert_eq!(cmd.rpc.ws_port, 8550);
        // check network listening port number
        assert_eq!(cmd.network.port, 30305);
    }

    #[test]
    fn parse_with_unused_ports() {
        let cmd: NodeCommand<EthereumChainSpecParser> =
            NodeCommand::parse_from(["reth", "--with-unused-ports"]);
        assert!(cmd.with_unused_ports);
    }

    #[test]
    fn with_unused_ports_conflicts_with_instance() {
        let err = NodeCommand::<EthereumChainSpecParser>::try_parse_args_from([
            "reth",
            "--with-unused-ports",
            "--instance",
            "2",
        ])
        .unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn with_unused_ports_check_zero() {
        let mut cmd: NodeCommand<EthereumChainSpecParser> = NodeCommand::parse_from(["reth"]);
        cmd.rpc = cmd.rpc.with_unused_ports();
        cmd.network = cmd.network.with_unused_ports();

        // make sure the rpc ports are zero
        assert_eq!(cmd.rpc.auth_port, 0);
        assert_eq!(cmd.rpc.http_port, 0);
        assert_eq!(cmd.rpc.ws_port, 0);

        // make sure the network ports are zero
        assert_eq!(cmd.network.port, 0);
        assert_eq!(cmd.network.discovery.port, 0);

        // make sure the ipc path is not the default
        assert_ne!(cmd.rpc.ipcpath, String::from("/tmp/reth.ipc"));
    }
}
