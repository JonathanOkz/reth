//! Main node command for launching a node

use crate::launcher::Launcher;
use clap::{value_parser, Args, Parser};
use reth_chainspec::{EthChainSpec, EthereumHardforks};
use reth_cli::chainspec::ChainSpecParser;
use reth_cli_runner::CliContext;
use reth_db::DatabaseEnv;
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
use std::{
    ffi::OsString,
    fmt,
    io,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tar::{Archive as TarArchive, Builder as TarBuilder};
use zstd::stream::{read::Decoder as ZstdDecoder, write::Encoder as ZstdEncoder};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SnapshotInputsFingerprint {
    mdbx_len: u64,
    mdbx_modified: Option<SystemTime>,
    static_files_total_len: u64,
    static_files_max_modified: Option<SystemTime>,
    static_files_file_count: u64,
}

fn snapshot_inputs_fingerprint(
    db_path: &Path,
    static_files_path: &Path,
) -> io::Result<SnapshotInputsFingerprint> {
    fn safe_modified(meta: &std::fs::Metadata) -> Option<SystemTime> {
        meta.modified().ok()
    }

    fn walk_dir_stats(path: &Path) -> io::Result<(u64, Option<SystemTime>, u64)> {
        let mut total_len = 0u64;
        let mut max_modified: Option<SystemTime> = None;
        let mut files = 0u64;

        let read_dir = match std::fs::read_dir(path) {
            Ok(it) => it,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok((0, None, 0)),
            Err(err) => return Err(err),
        };

        for entry in read_dir {
            let entry = entry?;
            let p = entry.path();
            let meta = match entry.metadata() {
                Ok(m) => m,
                Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
                Err(err) => return Err(err),
            };

            if meta.is_dir() {
                let (len, modified, file_count) = walk_dir_stats(&p)?;
                total_len = total_len.saturating_add(len);
                files = files.saturating_add(file_count);
                if let Some(m) = modified {
                    if max_modified.is_none_or(|cur| m > cur) {
                        max_modified = Some(m);
                    }
                }
            } else if meta.is_file() {
                total_len = total_len.saturating_add(meta.len());
                files = files.saturating_add(1);
                if let Some(m) = safe_modified(&meta) {
                    if max_modified.is_none_or(|cur| m > cur) {
                        max_modified = Some(m);
                    }
                }
            }
        }

        Ok((total_len, max_modified, files))
    }

    let mdbx_path = db_path.join("mdbx.dat");
    let (mdbx_len, mdbx_modified) = match std::fs::metadata(&mdbx_path) {
        Ok(meta) => (meta.len(), safe_modified(&meta)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => (0, None),
        Err(err) => return Err(err),
    };

    let (static_files_total_len, static_files_max_modified, static_files_file_count) =
        walk_dir_stats(static_files_path)?;

    Ok(SnapshotInputsFingerprint {
        mdbx_len,
        mdbx_modified,
        static_files_total_len,
        static_files_max_modified,
        static_files_file_count,
    })
}

async fn maybe_run_snapshot_backup(
    snapshot_backup_has_run: Arc<AtomicBool>,
    database_for_snapshot: Arc<DatabaseEnv>,
    db_path: PathBuf,
    static_files_path: PathBuf,
    snapshot_path_zst: PathBuf,
    snapshot: SnapshotArgs,
) {
    if snapshot_backup_has_run.swap(true, Ordering::SeqCst) {
        tracing::info!(target: "reth::cli", "snapshot backup already executed, skipping");
        return;
    }

    tracing::info!(target: "reth::cli", path = ?snapshot_path_zst, "starting snapshot backup");

    let settle_started = Instant::now();
    let settle_max = Duration::from_millis(snapshot.settle_max_ms);
    let settle_interval = Duration::from_millis(snapshot.settle_interval_ms);

    let mut last_fp: Option<SnapshotInputsFingerprint> = None;
    let mut stable_iters = 0usize;

    if settle_max > Duration::ZERO && snapshot.settle_stable_iters > 0 {
        while settle_started.elapsed() < settle_max {
            let db_path_for_fp = db_path.clone();
            let static_files_path_for_fp = static_files_path.clone();
            let fp_res = tokio::task::spawn_blocking(move || {
                snapshot_inputs_fingerprint(&db_path_for_fp, &static_files_path_for_fp)
            })
            .await;

            let fp = match fp_res {
                Ok(Ok(fp)) => fp,
                Ok(Err(err)) => {
                    tracing::warn!(target: "reth::cli", err = %err, "failed to fingerprint snapshot inputs; proceeding without settle");
                    break;
                }
                Err(err) => {
                    tracing::warn!(target: "reth::cli", err = %err, "fingerprint task join error; proceeding without settle");
                    break;
                }
            };

            if last_fp == Some(fp) {
                stable_iters += 1;
                if stable_iters >= snapshot.settle_stable_iters {
                    break;
                }
            } else {
                stable_iters = 0;
                last_fp = Some(fp);
            }

            tokio::time::sleep(settle_interval).await;
        }
    }

    let pid = std::process::id();
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);

    let default_stage_dir = db_path.join("snapshots-zst");
    let mut stage_dir = snapshot
        .snapshots_zst_dir
        .as_ref()
        .cloned()
        .unwrap_or_else(|| default_stage_dir.clone());
    if let Err(err) = tokio::fs::create_dir_all(&stage_dir).await {
        tracing::warn!(target: "reth::cli", err = %err, path = ?stage_dir, "failed to create snapshots-zst dir");
        stage_dir = default_stage_dir;
        if let Err(err) = tokio::fs::create_dir_all(&stage_dir).await {
            tracing::error!(target: "reth::cli", err = %err, path = ?stage_dir, "failed to create fallback snapshots-zst dir");
            return;
        }
    }

    if let Some(parent) = snapshot_path_zst.parent() {
        if let Err(err) = tokio::fs::create_dir_all(parent).await {
            tracing::error!(target: "reth::cli", ?err, path = ?parent, "failed to create snapshots dir");
            return;
        }
    } else {
        tracing::error!(target: "reth::cli", path = ?snapshot_path_zst, "snapshot destination has no parent");
        return;
    }

    let mut flags = CopyFlags::DONT_FLUSH | CopyFlags::COMPACT | CopyFlags::FORCE_DYNAMIC_SIZE;
    if std::env::var("RETH_SNAPSHOT_MDBX_THROTTLE_MVCC").ok().as_deref() == Some("1") {
        flags |= CopyFlags::THROTTLE_MVCC;
    }

    let staged_snapshot_dir_tmp = stage_dir.join(format!("mdbx.snapshot.tmp-{pid}"));
    let staged_snapshot_dat = staged_snapshot_dir_tmp.join("mdbx.dat");
    let staged_zst_tmp = stage_dir.join(format!("mdbx.dat.zst.tmp-{pid}-{unique}"));
    let staged_zst = stage_dir.join("mdbx.dat.zst");

    let snapshot_started = Instant::now();
    let db_for_snapshot = database_for_snapshot.clone();
    let flags_for_snapshot = flags;
    let staged_snapshot_dir_tmp_for_snapshot = staged_snapshot_dir_tmp.clone();
    let staged_snapshot_dat_for_snapshot = staged_snapshot_dat.clone();
    let snapshot_res = tokio::task::spawn_blocking(move || {
        if staged_snapshot_dir_tmp_for_snapshot.exists() {
            if let Some(parent) = staged_snapshot_dir_tmp_for_snapshot.parent() {
                let base = staged_snapshot_dir_tmp_for_snapshot
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_else(|| "mdbx.snapshot.tmp".to_string());
                let stale = parent.join(format!("{base}.stale-{unique}"));
                let _ = std::fs::rename(&staged_snapshot_dir_tmp_for_snapshot, &stale);
            }
        }

        std::fs::create_dir_all(&staged_snapshot_dir_tmp_for_snapshot).map_err(|e| e.to_string())?;
        db_for_snapshot
            .snapshot_to_path(&staged_snapshot_dat_for_snapshot, flags_for_snapshot)
            .map_err(|e| e.to_string())
    })
    .await;

    match snapshot_res {
        Ok(Ok(())) => {
            tracing::info!(
                target: "reth::cli",
                path = ?staged_snapshot_dat,
                elapsed_ms = snapshot_started.elapsed().as_millis(),
                "mdbx snapshot created"
            );
        }
        Ok(Err(err)) => {
            tracing::error!(target: "reth::cli", err = %err, "failed to snapshot mdbx");
            return;
        }
        Err(err) => {
            tracing::error!(target: "reth::cli", err = %err, "snapshot task join error");
            return;
        }
    }

    let src_len = match tokio::fs::metadata(&staged_snapshot_dat).await {
        Ok(m) => m.len(),
        Err(_) => 0,
    };
    if src_len == 0 {
        tracing::error!(target: "reth::cli", path = ?staged_snapshot_dat, "mdbx snapshot file is empty");
        return;
    }

    tracing::info!(target: "reth::cli", path = ?staged_snapshot_dat, src_len, "starting snapshot compression");

    let compress_started = Instant::now();
    let staged_snapshot_dat_for_compress = staged_snapshot_dat.clone();
    let staged_zst_tmp_for_compress = staged_zst_tmp.clone();
    let static_files_path_for_compress = static_files_path.clone();

    let zstd_level = snapshot.zstd_level;

    let chain_dir_for_compress = match db_path.parent() {
        Some(parent) => parent.to_path_buf(),
        None => {
            tracing::error!(target: "reth::cli", path = ?db_path, "db path has no parent; cannot locate chain dir");
            return;
        }
    };
    let blobstore_path_for_compress = chain_dir_for_compress.join("blobstore");
    let invalid_block_hooks_path_for_compress = chain_dir_for_compress.join("invalid_block_hooks");
    let rocksdb_path_for_compress = chain_dir_for_compress.join("rocksdb");
    let txpool_transactions_path_for_compress = chain_dir_for_compress.join("txpool_transactions");
    let exex_wal_path_for_compress = chain_dir_for_compress.join("exex_wal");

    let compress_res = tokio::task::spawn_blocking(move || {
        use std::io::Write;

        let src = std::fs::File::open(&staged_snapshot_dat_for_compress)?;
        let src_len = src.metadata()?.len();
        if src_len == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "snapshot source is empty"))
        }

        let dst = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&staged_zst_tmp_for_compress)?;
        let encoder = ZstdEncoder::new(dst, zstd_level)?;
        let mut tar = TarBuilder::new(encoder);
        tar.append_path_with_name(&staged_snapshot_dat_for_compress, "db/mdbx.dat")?;
        tar.append_dir_all("static_files", &static_files_path_for_compress)?;

        if blobstore_path_for_compress.exists() {
            tar.append_dir_all("blobstore", &blobstore_path_for_compress)?;
        }
        if invalid_block_hooks_path_for_compress.exists() {
            tar.append_dir_all("invalid_block_hooks", &invalid_block_hooks_path_for_compress)?;
        }
        if rocksdb_path_for_compress.exists() {
            tar.append_dir_all("rocksdb", &rocksdb_path_for_compress)?;
        }
        if txpool_transactions_path_for_compress.exists() {
            tar.append_dir_all("txpool_transactions", &txpool_transactions_path_for_compress)?;
        }
        if exex_wal_path_for_compress.exists() {
            tar.append_dir_all("exex_wal", &exex_wal_path_for_compress)?;
        }

        tar.finish()?;

        let encoder = tar.into_inner()?;
        let mut dst = encoder.finish()?;
        dst.flush()?;
        let written = dst.metadata().map(|m| m.len()).unwrap_or(0);
        if written == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "compressed snapshot is empty",
            ))
        }
        Ok::<_, io::Error>(())
    })
    .await;

    match compress_res {
        Ok(Ok(())) => {
            tracing::info!(
                target: "reth::cli",
                path = ?staged_zst_tmp,
                elapsed_ms = compress_started.elapsed().as_millis(),
                "snapshot compressed"
            );
        }
        Ok(Err(err)) => {
            tracing::error!(target: "reth::cli", err = %err, "failed to compress snapshot");
            return;
        }
        Err(err) => {
            tracing::error!(target: "reth::cli", err = %err, "snapshot compress task join error");
            return;
        }
    }

    if tokio::fs::metadata(&staged_zst).await.is_ok() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let staged_prev = stage_dir.join(format!("mdbx.dat.zst.prev-{pid}-{unique}"));
        let _ = tokio::fs::rename(&staged_zst, &staged_prev).await;
    }
    if let Err(err) = tokio::fs::rename(&staged_zst_tmp, &staged_zst).await {
        tracing::error!(target: "reth::cli", err = %err, src = ?staged_zst_tmp, dest = ?staged_zst, "failed to finalize compressed snapshot");
        return;
    }

    // Cleanup: the raw snapshot copy can be huge; keep only compressed artifact.

    let upload_started = Instant::now();
    let staged_zst_for_upload = staged_zst.clone();
    let snapshot_path_for_upload = snapshot_path_zst.clone();
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
            Ok(written)
        }

        let src = std::fs::File::open(&staged_zst_for_upload)?;
        let src_len = src.metadata()?.len();
        if src_len == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "upload source is empty"))
        }

        let dest_parent = snapshot_path_for_upload
            .parent()
            .map(|p| p.to_path_buf())
            .ok_or_else(|| io::Error::other("snapshot destination has no parent"))?;

        let dest_len = std::fs::metadata(&snapshot_path_for_upload).map(|m| m.len()).unwrap_or(0);

        if dest_len > 0 && src_len < dest_len / 2 {
            return Err(io::Error::other(format!(
                "refusing to overwrite larger existing snapshot ({dest_len} bytes) with smaller one ({src_len} bytes)"
            )))
        }

        let dest_tmp = dest_parent.join(format!("mdbx.dat.zst.tmp-{pid}-{unique}"));

        let dst = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&dest_tmp)?;
        let written = copy_large(src, dst)?;

        if written != src_len {
            return Err(io::Error::other(format!(
                "short write uploading snapshot: expected {src_len} bytes, wrote {written} bytes"
            )));
        }

        let dest_backup = dest_parent.join(format!("mdbx.dat.zst.bak-{pid}-{unique}"));
        let moved_old = std::fs::rename(&snapshot_path_for_upload, &dest_backup).is_ok();

        match std::fs::rename(&dest_tmp, &snapshot_path_for_upload) {
            Ok(()) => {
                let _ = moved_old;
            }
            Err(err) => {
                if moved_old {
                    let _ = std::fs::rename(&dest_backup, &snapshot_path_for_upload);
                }
                return Err(err);
            }
        }

        Ok::<_, io::Error>(written)
    })
    .await;

    match upload_res {
        Ok(Ok(_)) => {
            tracing::info!(
                target: "reth::cli",
                path = ?snapshot_path_zst,
                elapsed_ms = upload_started.elapsed().as_millis(),
                "snapshot uploaded"
            );
        }
        Ok(Err(err)) => {
            tracing::error!(target: "reth::cli", err = %err, dest = ?snapshot_path_zst, "failed to upload snapshot");
        }
        Err(err) => {
            tracing::error!(target: "reth::cli", err = %err, dest = ?snapshot_path_zst, "snapshot upload task join error");
        }
    }
}

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

    #[arg(long = "snapshot.snapshots-zst", env = "RETH_SNAPSHOT_SNAPSHOTS_ZST")]
    pub snapshots_zst_dir: Option<PathBuf>,

    #[arg(
        long = "snapshot.settle-max-ms",
        env = "RETH_SNAPSHOT_SETTLE_MAX_MS",
        default_value_t = 0
    )]
    pub settle_max_ms: u64,

    #[arg(
        long = "snapshot.settle-interval-ms",
        env = "RETH_SNAPSHOT_SETTLE_INTERVAL_MS",
        default_value_t = 200
    )]
    pub settle_interval_ms: u64,

    #[arg(
        long = "snapshot.settle-stable-iters",
        env = "RETH_SNAPSHOT_SETTLE_STABLE_ITERS",
        default_value_t = 2
    )]
    pub settle_stable_iters: usize,

    #[arg(
        long = "snapshot.zstd-level",
        env = "RETH_SNAPSHOT_ZSTD_LEVEL",
        default_value_t = 1,
        value_parser = clap::value_parser!(i32).range(1..=22)
    )]
    pub zstd_level: i32,
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
        let static_files_path = data_dir.static_files();

        let snapshots_base_dir = if snapshot.snapshot_enabled {
            snapshot.snapshots_dir.as_ref().map(|dir| {
                let mut path = dir.clone();
                path.push(node_config.chain.chain_id().to_string());
                path
            })
        } else {
            None
        };
        let snapshot_path_zst = snapshots_base_dir.as_ref().map(|base| {
            let mut path = base.clone();
            path.push("mdbx.dat.zst");
            path
        });

        if snapshot.snapshot_enabled {
            if let Some(snapshot_path_zst) = snapshot_path_zst.as_ref() {
                tracing::info!(
                    target: "reth::cli",
                    path = ?snapshot_path_zst,
                    "snapshot enabled: restore expects mdbx.dat.zst (tar.zst: db/ + static_files/ + optional: blobstore/, invalid_block_hooks/, rocksdb/, txpool_transactions/, exex_wal/); backup runs on shutdown"
                );

                let force_restore = std::env::var("RETH_SNAPSHOT_FORCE_RESTORE")
                    .ok()
                    .as_deref()
                    .is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"));

                let existing_db_len = tokio::fs::metadata(db_path.join("mdbx.dat"))
                    .await
                    .map(|m| m.len())
                    .unwrap_or(0);
                let existing_static_files_any = match tokio::fs::read_dir(&static_files_path).await {
                    Ok(mut dir) => dir.next_entry().await.ok().flatten().is_some(),
                    Err(_) => false,
                };

                if !force_restore && (existing_db_len > 0 || existing_static_files_any) {
                    tracing::warn!(
                        target: "reth::cli",
                        existing_db_len,
                        existing_static_files_any,
                        "existing datadir detected, skipping snapshot restore (set RETH_SNAPSHOT_FORCE_RESTORE=1 to override)"
                    );
                } else {
                    if let Some(parent) = db_path.parent() {
                        if let Err(err) = tokio::fs::create_dir_all(parent).await {
                            tracing::error!(target: "reth::cli", ?err, path = ?parent, "failed to create chain data dir");
                        }
                    }

                    let snapshot_zst_len = tokio::fs::metadata(snapshot_path_zst)
                        .await
                        .map(|m| m.len())
                        .unwrap_or(0);

                    if snapshot_zst_len == 0 {
                        tracing::error!(
                            target: "reth::cli",
                            path = ?snapshot_path_zst,
                            "no snapshot found, skipping restore"
                        );
                    } else {
                        let restore_started = Instant::now();
                        let snapshot_src = snapshot_path_zst.clone();
                        let snapshot_src_for_logs = snapshot_src.clone();
                        let db_dir_for_restore = db_path.clone();
                        let static_files_dir_for_restore = static_files_path.clone();
                        let restore_res = tokio::task::spawn_blocking(move || {
                            let chain_dir = db_dir_for_restore
                                .parent()
                                .ok_or_else(|| io::Error::other("db dir has no parent"))?
                                .to_path_buf();

                            let pid = std::process::id();
                            let restore_tmp_dir = chain_dir.join(format!("snapshot.restore.tmp-{pid}"));
                            let _ = std::fs::remove_dir_all(&restore_tmp_dir);
                            std::fs::create_dir_all(&restore_tmp_dir)?;

                            fn copy_large(
                                mut src: impl std::io::Read,
                                mut dst: std::fs::File,
                            ) -> io::Result<u64> {
                                use std::io::Write;
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
                                Ok(written)
                            }

                            let restored_db_dir = restore_tmp_dir.join("db");
                            let restored_db_file = restored_db_dir.join("mdbx.dat");
                            let restored_static_files_dir = restore_tmp_dir.join("static_files");

                            let is_tar = {
                                let src_file = std::fs::File::open(&snapshot_src)?;
                                let src_len = src_file.metadata().map(|m| m.len())?;
                                if src_len == 0 {
                                    return Err(io::Error::new(
                                        io::ErrorKind::UnexpectedEof,
                                        "snapshot source is empty",
                                    ))
                                }

                                let mut decoder = ZstdDecoder::new(src_file)?;
                                let mut header = [0u8; 512];
                                let n = std::io::Read::read(&mut decoder, &mut header)?;
                                if n != 512 {
                                    return Err(io::Error::new(
                                        io::ErrorKind::UnexpectedEof,
                                        "snapshot header is truncated",
                                    ))
                                }
                                header.get(257..262) == Some(b"ustar")
                            };

                            let unpacked_static_files = if is_tar {
                                let src_file = std::fs::File::open(&snapshot_src)?;
                                let decoder = ZstdDecoder::new(src_file)?;
                                let mut archive = TarArchive::new(decoder);
                                archive.unpack(&restore_tmp_dir)?;
                                true
                            } else {
                                let _ = std::fs::remove_dir_all(&restore_tmp_dir);
                                std::fs::create_dir_all(&restore_tmp_dir)?;
                                std::fs::create_dir_all(&restored_db_dir)?;

                                let src_file = std::fs::File::open(&snapshot_src)?;
                                let decoder = ZstdDecoder::new(src_file)?;
                                let dst = std::fs::OpenOptions::new()
                                    .create(true)
                                    .truncate(true)
                                    .write(true)
                                    .open(&restored_db_file)?;
                                let written = copy_large(decoder, dst)?;
                                if written == 0 {
                                    return Err(io::Error::new(
                                        io::ErrorKind::UnexpectedEof,
                                        "restored db is empty",
                                    ))
                                }
                                false
                            };

                            let restored_db_len =
                                restored_db_file.metadata().map(|m| m.len()).unwrap_or(0);
                            if restored_db_len == 0 {
                                return Err(io::Error::new(
                                    io::ErrorKind::UnexpectedEof,
                                    "restored db is empty",
                                ))
                            }

                            let restored_static_files_file_count: u64 = if unpacked_static_files {
                                let static_files_has_any = std::fs::read_dir(&restored_static_files_dir)
                                    .ok()
                                    .and_then(|mut it| it.next())
                                    .is_some();
                                if !static_files_has_any {
                                    return Err(io::Error::new(
                                        io::ErrorKind::InvalidData,
                                        "snapshot does not contain static_files; regenerate snapshot",
                                    ))
                                }
                                std::fs::read_dir(&restored_static_files_dir)
                                    .ok()
                                    .map(|it| it.filter(|e| e.is_ok()).count() as u64)
                                    .unwrap_or(0)
                            } else {
                                let static_files_has_any = std::fs::read_dir(&static_files_dir_for_restore)
                                    .ok()
                                    .and_then(|mut it| it.next())
                                    .is_some();
                                if !static_files_has_any {
                                    return Err(io::Error::new(
                                        io::ErrorKind::InvalidData,
                                        "legacy snapshot restored db only, but static_files are missing on disk; regenerate snapshot",
                                    ))
                                }
                                0
                            };

                            if db_dir_for_restore.exists() {
                                let _ = std::fs::remove_dir_all(&db_dir_for_restore);
                            }
                            std::fs::rename(&restored_db_dir, &db_dir_for_restore)?;

                            if unpacked_static_files {
                                if static_files_dir_for_restore.exists() {
                                    let _ = std::fs::remove_dir_all(&static_files_dir_for_restore);
                                }
                                std::fs::rename(
                                    &restored_static_files_dir,
                                    &static_files_dir_for_restore,
                                )?;
                            }

                            for dir_name in [
                                "blobstore",
                                "invalid_block_hooks",
                                "rocksdb",
                                "txpool_transactions",
                                "exex_wal",
                            ] {
                                let restored = restore_tmp_dir.join(dir_name);
                                if restored.exists() {
                                    let dest = chain_dir.join(dir_name);
                                    if dest.exists() {
                                        let _ = std::fs::remove_dir_all(&dest);
                                    }
                                    std::fs::rename(&restored, &dest)?;
                                }
                            }

                            let _ = std::fs::remove_dir_all(&restore_tmp_dir);
                            Ok::<_, io::Error>((
                                unpacked_static_files,
                                restored_db_len,
                                restored_static_files_file_count,
                            ))
                        })
                        .await;

                        match restore_res {
                            Ok(Ok((unpacked_static_files, restored_db_len, restored_static_files_file_count))) => {
                                tracing::info!(
                                    target: "reth::cli",
                                    path = ?snapshot_src_for_logs,
                                    elapsed_ms = restore_started.elapsed().as_millis(),
                                    unpacked_static_files,
                                    restored_db_len,
                                    restored_static_files_file_count,
                                    "snapshot restored"
                                );
                                let _ = tokio::fs::remove_file(db_path.join("mdbx.lck")).await;
                            }
                            Ok(Err(err)) if err.kind() == io::ErrorKind::NotFound => {
                                tracing::error!(target: "reth::cli", err = %err, path = ?snapshot_src_for_logs, "no snapshot found, skipping restore");
                            }
                            Ok(Err(err)) => {
                                tracing::error!(target: "reth::cli", err = %err, src = ?snapshot_src_for_logs, dest = ?db_path, "snapshot restore failed, continuing without restore");
                            }
                            Err(err) => {
                                tracing::error!(target: "reth::cli", err = %err, src = ?snapshot_src_for_logs, dest = ?db_path, "snapshot restore task join error, continuing without restore");
                            }
                        }
                    }
                }
            } else {
                tracing::error!(target: "reth::cli", "snapshot enabled but no snapshots dir configured; skipping snapshot restore");
            }
        }

        tracing::info!(target: "reth::cli", path = ?db_path, "Opening database");
        let database = match init_db(db_path.clone(), db.database_args()) {
            Ok(db) => Arc::new(db.with_metrics()),
            Err(err) => {
                if snapshot.snapshot_enabled {
                    tracing::error!(
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

        if with_unused_ports {
            node_config = node_config.with_unused_ports();
        }

        let database_for_snapshot = database.clone();
        let snapshot_backup_has_run = Arc::new(AtomicBool::new(false));

        // Note: snapshot backup is executed after the node stops (see below), to ensure all
        // in-memory canonical blocks have been persisted before snapshotting.
        let builder = NodeBuilder::new(node_config)
            .with_database(database)
            .with_launch_context(ctx.task_executor);

        let run_res = launcher.entrypoint(builder, ext).await;

        if snapshot.snapshot_enabled {
            if let Some(snapshot_path_zst) = snapshot_path_zst {
                maybe_run_snapshot_backup(
                    snapshot_backup_has_run,
                    database_for_snapshot,
                    db_path,
                    static_files_path,
                    snapshot_path_zst,
                    snapshot,
                )
                .await;
            } else {
                tracing::warn!(target: "reth::cli", "snapshot enabled but no snapshots dir configured; skipping snapshot backup");
            }
        }

        run_res
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
