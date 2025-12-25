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
use std::{ffi::OsString, fmt, io, path::PathBuf, sync::Arc, time::Instant};
use tracing::warn;

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

    #[arg(long = "snapshot.snapshots-dir", env = "RETH_SNAPSHOT_SNAPSHOTS_DIR")]
    pub snapshots_dir: Option<PathBuf>,
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

        let snapshot_path = if snapshot.snapshot_enabled {
            snapshot.snapshots_dir.as_ref().map(|dir| {
                let mut path = dir.clone();
                path.push(node_config.chain.chain_id().to_string());
                path.push("mdbx.dat");
                path
            })
        } else {
            None
        };

        if snapshot.snapshot_enabled {
            if let Some(snapshot_path) = snapshot_path.as_ref() {
                let db_file = db_path.join("mdbx.dat");
                if let Some(parent) = db_file.parent() {
                    if let Err(err) = tokio::fs::create_dir_all(parent).await {
                        warn!(target: "reth::cli", ?err, path = ?parent, "failed to create db dir");
                    }
                }

                let tmp_path = db_file.with_extension(format!("tmp-{}", std::process::id()));

                let restore_started = Instant::now();
                let snapshot_src = snapshot_path.clone();
                let tmp_path_for_restore = tmp_path.clone();
                let db_file_for_restore = db_file.clone();
                let restore_res = tokio::task::spawn_blocking(move || {
                    let _ = std::fs::remove_file(&tmp_path_for_restore);
                    fn copy_large(mut src: std::fs::File, mut dst: std::fs::File) -> io::Result<u64> {
                        use std::io::{Read, Write};

                        let mut buf = vec![0u8; 8 * 1024 * 1024];
                        let mut written = 0u64;
                        loop {
                            let n = match src.read(&mut buf) {
                                Ok(0) => break,
                                Ok(n) => n,
                                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                                Err(e) => return Err(e),
                            };
                            dst.write_all(&buf[..n])?;
                            written += n as u64;
                        }
                        Ok(written)
                    }

                    let src = std::fs::File::open(&snapshot_src)?;
                    let len = src.metadata().map(|m| m.len()).ok();
                    let dst = std::fs::OpenOptions::new()
                        .create(true)
                        .truncate(true)
                        .write(true)
                        .open(&tmp_path_for_restore)?;
                    if let Some(len) = len {
                        let _ = dst.set_len(len);
                    }
                    let dst_for_copy = dst.try_clone()?;
                    drop(dst);
                    let _ = copy_large(src, dst_for_copy)?;
                    std::fs::rename(&tmp_path_for_restore, &db_file_for_restore)?;
                    Ok::<_, io::Error>(())
                })
                .await;

                match restore_res {
                    Ok(Ok(())) => {
                        tracing::info!(
                            target: "reth::cli",
                            path = ?snapshot_path,
                            elapsed_ms = restore_started.elapsed().as_millis(),
                            "snapshot restored"
                        );
                        let _ = tokio::fs::remove_file(db_path.join("mdbx.lck")).await;
                    }
                    Ok(Err(err)) if err.kind() == io::ErrorKind::NotFound => {
                        tracing::info!(target: "reth::cli", path = ?snapshot_path, "no snapshot found, skipping restore");
                    }
                    Ok(Err(err)) => {
                        let _ = tokio::fs::remove_file(&tmp_path).await;
                        warn!(
                            target: "reth::cli",
                            err = %err,
                            "snapshot restore failed, continuing without restore"
                        );
                    }
                    Err(err) => {
                        let _ = tokio::fs::remove_file(&tmp_path).await;
                        warn!(
                            target: "reth::cli",
                            err = %err,
                            "snapshot restore task join error, continuing without restore"
                        );
                    }
                }
            } else {
                warn!(target: "reth::cli", "snapshot enabled but no snapshots dir configured; skipping snapshot restore");
            }
        }

        tracing::info!(target: "reth::cli", path = ?db_path, "Opening database");
        let database = match init_db(db_path.clone(), db.database_args()) {
            Ok(db) => Arc::new(db.with_metrics()),
            Err(err) => {
                if snapshot.snapshot_enabled {
                    warn!(
                        target: "reth::cli",
                        err = %err,
                        path = ?db_path,
                        "failed to open database after snapshot restore; removing restored db and continuing without snapshot"
                    );
                    let _ = tokio::fs::remove_file(db_path.join("mdbx.dat")).await;
                    let _ = tokio::fs::remove_file(db_path.join("mdbx.lck")).await;
                    Arc::new(init_db(db_path.clone(), db.database_args())?.with_metrics())
                } else {
                    return Err(err);
                }
            }
        };

        if snapshot.snapshot_enabled {
            if let Some(snapshot_path) = snapshot_path {
                let db_path_for_snapshot_task = db_path.clone();
                let database = database.clone();
                ctx.task_executor.spawn_critical_with_graceful_shutdown_signal(
                    "mdbx-snapshot-file",
                    move |shutdown| async move {
                        let guard = shutdown.await;

                        if let Some(parent) = snapshot_path.parent() {
                            if let Err(err) = tokio::fs::create_dir_all(parent).await {
                                warn!(target: "reth::cli", ?err, path = ?parent, "failed to create snapshots dir");
                            }
                        }

                        let mut flags = CopyFlags::DONT_FLUSH;
                        if std::env::var("RETH_SNAPSHOT_MDBX_THROTTLE_MVCC").ok().as_deref() == Some("1") {
                            flags |= CopyFlags::THROTTLE_MVCC;
                        }

                        let local_tmp = db_path_for_snapshot_task
                            .join(format!("mdbx.snapshot.tmp-{}", std::process::id()));

                        let snapshot_started = Instant::now();
                        let db_for_snapshot = database.clone();
                        let flags_for_snapshot = flags;
                        let local_tmp_for_snapshot = local_tmp.clone();
                        let snapshot_res = tokio::task::spawn_blocking(move || {
                            let _ = std::fs::remove_file(&local_tmp_for_snapshot);
                            db_for_snapshot
                                .snapshot_to_path(&local_tmp_for_snapshot, flags_for_snapshot)
                                .map_err(|e| e.to_string())
                        })
                        .await;

                        match snapshot_res {
                            Ok(Ok(())) => {
                                tracing::info!(
                                    target: "reth::cli",
                                    path = ?local_tmp,
                                    elapsed_ms = snapshot_started.elapsed().as_millis(),
                                    "mdbx snapshot created"
                                );
                            }
                            Ok(Err(err)) => {
                                warn!(target: "reth::cli", err = %err, "failed to snapshot mdbx");
                                let _ = tokio::fs::remove_file(&local_tmp).await;
                                drop(guard);
                                return
                            }
                            Err(err) => {
                                warn!(target: "reth::cli", err = %err, "snapshot task join error");
                                let _ = tokio::fs::remove_file(&local_tmp).await;
                                drop(guard);
                                return
                            }
                        }

                        let src_len = match tokio::fs::metadata(&local_tmp).await {
                            Ok(m) => m.len(),
                            Err(_) => 0,
                        };
                        if src_len == 0 {
                            tracing::error!(target: "reth::cli", path = ?local_tmp, "mdbx snapshot file is empty");
                            let _ = tokio::fs::remove_file(&local_tmp).await;
                            drop(guard);
                            return
                        }

                        let _ = tokio::fs::remove_file(&snapshot_path).await;

                        let upload_started = Instant::now();
                        let local_tmp_for_upload = local_tmp.clone();
                        let snapshot_path_for_upload = snapshot_path.clone();
                        let upload_res = tokio::task::spawn_blocking(move || {
                            use std::io::{Read, Write};

                            fn copy_large(mut src: std::fs::File, mut dst: std::fs::File) -> io::Result<u64> {
                                let mut buf = vec![0u8; 8 * 1024 * 1024];
                                let mut written = 0u64;
                                loop {
                                    let n = match src.read(&mut buf) {
                                        Ok(0) => break,
                                        Ok(n) => n,
                                        Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                                        Err(e) => return Err(e),
                                    };
                                    dst.write_all(&buf[..n])?;
                                    written += n as u64;
                                }
                                dst.flush()?;
                                let _ = dst.sync_all();
                                Ok(written)
                            }

                            let src = std::fs::File::open(&local_tmp_for_upload)?;
                            let src_len = src.metadata()?.len();
                            let dst = std::fs::OpenOptions::new()
                                .create(true)
                                .truncate(true)
                                .write(true)
                                .open(&snapshot_path_for_upload)?;
                            let written = copy_large(src, dst)?;

                            if written != src_len {
                                return Err(io::Error::new(
                                    io::ErrorKind::Other,
                                    format!("short write uploading snapshot: expected {src_len} bytes, wrote {written} bytes"),
                                ));
                            }

                            Ok::<_, io::Error>(written)
                        })
                        .await;

                        match upload_res {
                            Ok(Ok(_)) => {
                                tracing::info!(
                                    target: "reth::cli",
                                    path = ?snapshot_path,
                                    elapsed_ms = upload_started.elapsed().as_millis(),
                                    "snapshot uploaded"
                                );
                            }
                            Ok(Err(err)) => {
                                tracing::error!(target: "reth::cli", err = %err, dest = ?snapshot_path, "failed to upload snapshot");
                                let _ = tokio::fs::remove_file(&snapshot_path).await;
                                let _ = tokio::fs::remove_file(&local_tmp).await;
                                drop(guard);
                                return
                            }
                            Err(err) => {
                                tracing::error!(target: "reth::cli", err = %err, dest = ?snapshot_path, "snapshot upload task join error");
                                let _ = tokio::fs::remove_file(&snapshot_path).await;
                                let _ = tokio::fs::remove_file(&local_tmp).await;
                                drop(guard);
                                return
                            }
                        }

                        let _ = tokio::fs::remove_file(&local_tmp).await;

                        drop(guard);
                    }
                );
            } else {
                warn!(target: "reth::cli", "snapshot enabled but no snapshots dir configured; skipping snapshot backup");
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
