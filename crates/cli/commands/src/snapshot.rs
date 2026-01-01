use reth_db::DatabaseEnv;
use reth_db::init_db;
use reth_db::mdbx::CopyFlags;
use reth_snapshotter::{BackupOptions, RestoreOptions};
use std::{
    io,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant, SystemTime},
};
use tokio::sync::oneshot;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SnapshotInputsFingerprint {
    mdbx_len: u64,
    mdbx_modified: Option<SystemTime>,
    static_files_total_len: u64,
    static_files_max_modified: Option<SystemTime>,
    static_files_file_count: u64,
    extra_total_len: u64,
    extra_max_modified: Option<SystemTime>,
    extra_file_count: u64,
}

fn snapshot_inputs_fingerprint(
    db_path: &Path,
    static_files_path: &Path,
    chain_dir: &Path,
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

    let mut extra_total_len = 0u64;
    let mut extra_max_modified: Option<SystemTime> = None;
    let mut extra_file_count = 0u64;

    for dir_name in [
        "blobstore",
        "invalid_block_hooks",
        "rocksdb",
        "txpool_transactions",
        "exex_wal",
    ] {
        let (len, modified, file_count) = walk_dir_stats(&chain_dir.join(dir_name))?;
        extra_total_len = extra_total_len.saturating_add(len);
        extra_file_count = extra_file_count.saturating_add(file_count);
        if let Some(m) = modified {
            if extra_max_modified.is_none_or(|cur| m > cur) {
                extra_max_modified = Some(m);
            }
        }
    }

    Ok(SnapshotInputsFingerprint {
        mdbx_len,
        mdbx_modified,
        static_files_total_len,
        static_files_max_modified,
        static_files_file_count,
        extra_total_len,
        extra_max_modified,
        extra_file_count,
    })
}

fn env_flag(name: &str) -> bool {
    std::env::var(name)
        .ok()
        .as_deref()
        .is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
}

#[derive(Debug, clap::Args, Default, Clone)]
pub struct SnapshotArgs {
    #[arg(long = "snapshot.enabled", env = "RETH_SNAPSHOT_ENABLED", default_value_t = false)]
    pub snapshot_enabled: bool,

    #[arg(long = "snapshot.destination", env = "RETH_SNAPSHOT_DESTINATION")]
    pub snapshots_destination: Option<PathBuf>,

    #[arg(long = "snapshot.project-id", env = "RETH_SNAPSHOT_PROJECT_ID")]
    pub project_id: Option<String>,

    #[arg(long = "snapshot.staging", env = "RETH_SNAPSHOT_STAGING")]
    pub snapshots_staging: Option<PathBuf>,

    #[arg(long = "snapshot.secure-copy", env = "RETH_SNAPSHOT_SECURE_COPY", default_value_t = false)]
    pub secure_copy: bool,

    #[arg(long = "snapshot.force-restore", default_value_t = false)]
    pub force_restore: bool,

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
        value_parser = clap::value_parser!(i32).range(-7..=22)
    )]
    pub zstd_level: i32,
}

impl SnapshotArgs {
    pub fn snapshots_base_dir(&self, chain_id: u64) -> Option<PathBuf> {
        if !self.snapshot_enabled {
            return None;
        }
        self.snapshots_destination.as_ref().map(|dir| {
            let mut path = dir.clone();
            let scope = self.project_id.clone().unwrap_or_else(|| chain_id.to_string());
            path.push(scope);
            path
        })
    }

    pub fn snapshot_path_zst(&self, chain_id: u64) -> Option<PathBuf> {
        self.snapshots_base_dir(chain_id).map(|mut base| {
            base.push("mdbx.dat.zst");
            base
        })
    }
}

pub async fn maybe_restore_snapshot(snapshot: &SnapshotArgs, chain_id: u64, db_path: &Path, static_files_path: &Path) {
    if !snapshot.snapshot_enabled {
        return;
    }

    let Some(snapshot_path_zst) = snapshot.snapshot_path_zst(chain_id) else {
        tracing::error!(target: "reth::cli", "snapshot enabled but no snapshot destination configured; skipping snapshot restore");
        return;
    };

    tracing::info!(
        target: "reth::cli",
        path = ?snapshot_path_zst,
        "snapshot enabled: restore expects mdbx.dat.zst (tar.zst: db/ + static_files/ + optional: blobstore/, invalid_block_hooks/, rocksdb/, txpool_transactions/, exex_wal/); backup runs on shutdown"
    );

    let require_ok = env_flag("RETH_SNAPSHOT_REQUIRE_OK");
    let force_restore = snapshot.force_restore || env_flag("RETH_SNAPSHOT_FORCE_RESTORE");

    let existing_db_len = tokio::fs::metadata(db_path.join("mdbx.dat"))
        .await
        .map(|m| m.len())
        .unwrap_or(0);
    let existing_static_files_any = match tokio::fs::read_dir(static_files_path).await {
        Ok(mut dir) => dir.next_entry().await.ok().flatten().is_some(),
        Err(_) => false,
    };

    if !force_restore && (existing_db_len > 0 || existing_static_files_any) {
        tracing::warn!(
            target: "reth::cli",
            existing_db_len,
            existing_static_files_any,
            "existing datadir detected, skipping snapshot restore (set --snapshot.force-restore or RETH_SNAPSHOT_FORCE_RESTORE=1 to override)"
        );
        return;
    }

    if let Some(parent) = db_path.parent() {
        if let Err(err) = tokio::fs::create_dir_all(parent).await {
            tracing::error!(target: "reth::cli", ?err, path = ?parent, "failed to create chain data dir");
        }
    }

    let snapshot_zst_exists = tokio::fs::metadata(&snapshot_path_zst).await.is_ok();

    if !snapshot_zst_exists {
        tracing::error!(
            target: "reth::cli",
            path = ?snapshot_path_zst,
            "no snapshot found, skipping restore"
        );
        return;
    }

    let snapshot_ok_path = snapshot_path_zst.with_extension("zst.ok");
    let ok_exists = tokio::fs::metadata(&snapshot_ok_path).await.is_ok();
    if require_ok && !ok_exists {
        tracing::error!(
            target: "reth::cli",
            path = ?snapshot_ok_path,
            "snapshot ok marker missing, skipping restore"
        );
    }
    if !require_ok && !ok_exists {
        tracing::warn!(
            target: "reth::cli",
            path = ?snapshot_ok_path,
            "snapshot ok marker missing; attempting restore anyway"
        );
    }

    let should_restore = ok_exists || !require_ok;
    let remove_after_restore = env_flag("RETH_SNAPSHOT_REMOVE_AFTER_RESTORE");

    if !should_restore {
        tracing::warn!(
            target: "reth::cli",
            path = ?snapshot_path_zst,
            "snapshot restore skipped"
        );
        return;
    }

    let restore_started = Instant::now();
    let snapshot_src = snapshot_path_zst.clone();
    let snapshot_src_for_logs = snapshot_src.clone();
    let db_dir_for_restore = db_path.to_path_buf();
    let static_files_dir_for_restore = static_files_path.to_path_buf();

    let chain_dir = match db_dir_for_restore.parent() {
        Some(p) => p.to_path_buf(),
        None => {
            tracing::error!(target: "reth::cli", path = ?db_dir_for_restore, "db dir has no parent; cannot locate chain dir");
            return;
        }
    };

    let extra_dirs = [
        ("blobstore", chain_dir.join("blobstore")),
        ("invalid_block_hooks", chain_dir.join("invalid_block_hooks")),
        ("rocksdb", chain_dir.join("rocksdb")),
        ("txpool_transactions", chain_dir.join("txpool_transactions")),
        ("exex_wal", chain_dir.join("exex_wal")),
    ];

    let restore_root = chain_dir;

    let restore_res = tokio::task::spawn_blocking(move || {
        reth_snapshotter::restore_snapshot_tar_zst(
            &snapshot_src,
            &restore_root,
            &db_dir_for_restore,
            &static_files_dir_for_restore,
            &extra_dirs,
            RestoreOptions { io_buf_size: 8 * 1024 * 1024 },
        )
    })
    .await;

    match restore_res {
        Ok(Ok(report)) => {
            tracing::info!(
                target: "reth::cli",
                path = ?snapshot_src_for_logs,
                elapsed_ms = restore_started.elapsed().as_millis(),
                unpacked_static_files = report.unpacked_static_files,
                restored_db_len = report.restored_db_len,
                restored_static_files_file_count = report.restored_static_files_file_count,
                "snapshot restored"
            );
            let _ = tokio::fs::remove_file(db_path.join("mdbx.lck")).await;
            if remove_after_restore {
                match tokio::fs::remove_file(&snapshot_src_for_logs).await {
                    Ok(()) => {
                        tracing::info!(
                            target: "reth::cli",
                            path = ?snapshot_src_for_logs,
                            "snapshot removed after successful restore"
                        );
                        let _ = tokio::fs::remove_file(snapshot_src_for_logs.with_extension("zst.ok")).await;
                    }
                    Err(err) => {
                        tracing::warn!(
                            target: "reth::cli",
                            err = %err,
                            path = ?snapshot_src_for_logs,
                            "failed to remove snapshot after successful restore"
                        );
                    }
                }
            } else {
                tracing::info!(
                    target: "reth::cli",
                    path = ?snapshot_src_for_logs,
                    "snapshot kept after successful restore"
                );
            }
        }
        Ok(Err(err)) => {
            tracing::error!(target: "reth::cli", err = %err, src = ?snapshot_src_for_logs, dest = ?db_path, "snapshot restore failed, continuing without restore");
        }
        Err(err) => {
            tracing::error!(target: "reth::cli", err = %err, src = ?snapshot_src_for_logs, dest = ?db_path, "snapshot restore task join error, continuing without restore");
        }
    }
}

pub async fn maybe_run_snapshot_backup(
    snapshot_backup_has_run: Arc<AtomicBool>,
    database_for_snapshot: Arc<DatabaseEnv>,
    chain_id: u64,
    db_path: PathBuf,
    static_files_path: PathBuf,
    snapshot: SnapshotArgs,
) {
    if snapshot_backup_has_run.swap(true, Ordering::SeqCst) {
        tracing::info!(target: "reth::cli", "snapshot backup already executed, skipping");
        return;
    }

    let Some(snapshot_path_zst) = snapshot.snapshot_path_zst(chain_id) else {
        tracing::error!(target: "reth::cli", "snapshot enabled but no snapshot destination configured; skipping snapshot backup");
        return;
    };

    tracing::info!(target: "reth::cli", path = ?snapshot_path_zst, "starting snapshot backup");

    let settle_started = Instant::now();
    let settle_max = Duration::from_millis(snapshot.settle_max_ms);
    let settle_interval = Duration::from_millis(snapshot.settle_interval_ms);

    let mut last_fp: Option<SnapshotInputsFingerprint> = None;
    let mut stable_iters = 0usize;

    let chain_dir_for_fp = db_path.parent().map(|p| p.to_path_buf());

    if settle_max > Duration::ZERO && snapshot.settle_stable_iters > 0 {
        while settle_started.elapsed() < settle_max {
            let db_path_for_fp = db_path.clone();
            let static_files_path_for_fp = static_files_path.clone();
            let chain_dir_for_fp = chain_dir_for_fp.clone();
            let fp_res = tokio::task::spawn_blocking(move || {
                let Some(chain_dir_for_fp) = chain_dir_for_fp else {
                    return Err(io::Error::other("db path has no parent; cannot locate chain dir"));
                };
                snapshot_inputs_fingerprint(&db_path_for_fp, &static_files_path_for_fp, &chain_dir_for_fp)
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

    if let Some(parent) = snapshot_path_zst.parent() {
        if let Err(err) = tokio::fs::create_dir_all(parent).await {
            tracing::error!(target: "reth::cli", ?err, path = ?parent, "failed to create snapshots dir");
            return;
        }
    } else {
        tracing::error!(target: "reth::cli", path = ?snapshot_path_zst, "snapshot destination has no parent");
        return;
    }

    let mdbx_path_for_compress = if snapshot.secure_copy {
        let stage_dir = match snapshot.snapshots_staging.as_ref().cloned() {
            Some(dir) => dir,
            None => {
                tracing::error!(
                    target: "reth::cli",
                    "snapshot.secure-copy is enabled but snapshot.staging is not set"
                );
                return;
            }
        };
        if let Err(err) = tokio::fs::create_dir_all(&stage_dir).await {
            tracing::error!(
                target: "reth::cli",
                err = %err,
                path = ?stage_dir,
                "failed to create snapshots-staging dir"
            );
            return;
        }

        let mut flags = CopyFlags::DONT_FLUSH | CopyFlags::COMPACT | CopyFlags::FORCE_DYNAMIC_SIZE;
        if env_flag("RETH_SNAPSHOT_MDBX_THROTTLE_MVCC") {
            flags |= CopyFlags::THROTTLE_MVCC;
        }

        let staged_snapshot_dat = stage_dir.join("mdbx.dat");
        let snapshot_started = Instant::now();
        let db_for_snapshot = database_for_snapshot.clone();
        let flags_for_snapshot = flags;
        let staged_snapshot_dat_for_snapshot = staged_snapshot_dat.clone();
        let snapshot_res = tokio::task::spawn_blocking(move || {
            let parent = staged_snapshot_dat_for_snapshot
                .parent()
                .ok_or_else(|| "snapshot staging path has no parent".to_string())?;
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            db_for_snapshot
                .snapshot_to_path(&staged_snapshot_dat_for_snapshot, flags_for_snapshot)
                .map_err(|e| e.to_string())
        })
        .await;

        match snapshot_res {
            Ok(Ok(())) => {
                tracing::info!(
                    target: "reth::cli",
                    snapshot_dir = ?stage_dir,
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

        staged_snapshot_dat
    } else {
        let live_mdbx = db_path.join("mdbx.dat");
        tracing::warn!(
            target: "reth::cli",
            path = ?live_mdbx,
            "snapshot.secure-copy is disabled: compressing live mdbx.dat (unsafe)"
        );
        live_mdbx
    };

    let compress_started = Instant::now();
    let mdbx_path_for_compress_task = mdbx_path_for_compress.clone();
    let snapshot_path_zst_for_compress = snapshot_path_zst.clone();
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

    let extra_dirs = [
        ("blobstore", blobstore_path_for_compress),
        ("invalid_block_hooks", invalid_block_hooks_path_for_compress),
        ("rocksdb", rocksdb_path_for_compress),
        ("txpool_transactions", txpool_transactions_path_for_compress),
        ("exex_wal", exex_wal_path_for_compress),
    ];

    let compress_res = tokio::task::spawn_blocking(move || {
        reth_snapshotter::create_snapshot_tar_zst(
            &snapshot_path_zst_for_compress,
            &mdbx_path_for_compress_task,
            &static_files_path_for_compress,
            &extra_dirs,
            BackupOptions { zstd_level, io_buf_size: 8 * 1024 * 1024 },
        )
    })
    .await;

    match compress_res {
        Ok(Ok(report)) => {
            if report.snapshot_len == 0 {
                tracing::warn!(
                    target: "reth::cli",
                    path = ?snapshot_path_zst,
                    "compressed snapshot file size is reported as 0; continuing (metadata may be stale on remote fs)"
                );
            }
            tracing::info!(
                target: "reth::cli",
                path = ?snapshot_path_zst,
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

    tracing::info!(target: "reth::cli", path = ?snapshot_path_zst, "snapshot written");
}

pub async fn open_db_with_optional_snapshot_recovery(
    snapshot: &SnapshotArgs,
    db_path: PathBuf,
    static_files_path: PathBuf,
    db_args: reth_db::mdbx::DatabaseArguments,
) -> eyre::Result<Arc<DatabaseEnv>> {
    let database = match init_db(db_path.clone(), db_args.clone()) {
        Ok(db) => Arc::new(db.with_metrics()),
        Err(err) => {
            if snapshot.snapshot_enabled {
                tracing::error!(
                    target: "reth::cli",
                    err = %err,
                    path = ?db_path,
                    "failed to open database after snapshot restore; removing restored db and continuing without snapshot"
                );
                let chain_dir = db_path.parent().map(|p| p.to_path_buf());

                let _ = tokio::fs::remove_dir_all(&db_path).await;
                let _ = tokio::fs::remove_dir_all(&static_files_path).await;
                if let Some(chain_dir) = chain_dir {
                    for dir_name in [
                        "blobstore",
                        "invalid_block_hooks",
                        "rocksdb",
                        "txpool_transactions",
                        "exex_wal",
                    ] {
                        let _ = tokio::fs::remove_dir_all(chain_dir.join(dir_name)).await;
                    }
                }

                Arc::new(init_db(db_path.clone(), db_args)?.with_metrics())
            } else {
                return Err(err);
            }
        }
    };
    Ok(database)
}

pub fn spawn_backup_on_shutdown(
    ctx_task_executor: reth_tasks::TaskExecutor,
    node_stopped_rx: oneshot::Receiver<()>,
    snapshot_backup_has_run: Arc<AtomicBool>,
    database_for_snapshot: Arc<DatabaseEnv>,
    chain_id: u64,
    db_path: PathBuf,
    static_files_path: PathBuf,
    snapshot: SnapshotArgs,
) {
    let task_executor_task = ctx_task_executor.clone();
    ctx_task_executor.spawn_with_graceful_shutdown_signal(move |shutdown| async move {
        let guard = shutdown.await;

        let _ = node_stopped_rx.await;

        while task_executor_task.graceful_tasks_count() > 1 {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        maybe_run_snapshot_backup(
            snapshot_backup_has_run,
            database_for_snapshot,
            chain_id,
            db_path,
            static_files_path,
            snapshot,
        )
        .await;

        drop(guard)
    });
}
