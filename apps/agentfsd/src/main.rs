mod config;
use agentfs_engine::{Adapters, Engine, EngineConfig};
use agentfs_local::LocalBackend;
use agentfs_mcp::{GatewayConfig, router};
use agentfs_model::*;
use agentfs_platform::{
    ContainerValidation, HostDirectories, NativeMountDriver, SystemClock, local_identity,
};
use agentfs_ports::*;
use agentfs_s3::S3Backend;
use clap::{Parser, Subcommand};
use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Parser)]
#[command(version, about = "AgentFS workspace daemon")]
struct Arguments {
    #[command(subcommand)]
    command: Command,
}
#[derive(Debug, Subcommand)]
enum Command {
    /// Create private configuration and credentials in a new directory.
    Init {
        #[arg(long)]
        directory: PathBuf,
        #[arg(long, default_value = "127.0.0.1:7421")]
        listen: SocketAddr,
    },
    /// Serve authenticated MCP clients and native filesystem mounts.
    Serve {
        #[arg(long)]
        config: PathBuf,
    },
}
#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "agentfs=info,agentfsd=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    if let Err(error) = run(Arguments::parse()).await {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
async fn run(arguments: Arguments) -> Result<()> {
    match arguments.command {
        Command::Init { directory, listen } => {
            let path = config::initialize(&directory, listen)?;
            println!("{}", path.display());
            Ok(())
        }
        Command::Serve { config } => serve(config::Config::load(&config)?).await,
    }
}
async fn serve(config: config::Config) -> Result<()> {
    let credentials = config.credentials()?;
    let local = Arc::new(LocalBackend::open(&config.data_dir)?);
    let driver = NativeMountDriver::new(config.mount_roots)?;
    let remote = config.remote.map(S3Backend::new).transpose()?;
    let engine = Engine::open(
        Adapters {
            state: local.clone(),
            objects: local.clone(),
            working: local.clone(),
            remote_objects: remote
                .clone()
                .map(|store| store as Arc<dyn RemoteObjectStore>),
            remote_refs: remote.map(|store| store as Arc<dyn RemoteRefStore>),
            clock: Arc::new(SystemClock::default()),
            mounts: driver,
            directories: HostDirectories::new(config.directory_roots)?,
            validation: Arc::new(ContainerValidation::new(config.docker)),
        },
        EngineConfig {
            cache: config.cache,
            identity: local_identity(),
            validation: config.validation,
        },
    )
    .await?;
    let cancellation = CancellationToken::new();
    let listener = tokio::net::TcpListener::bind(config.listen).await?;
    let address = listener.local_addr()?;
    let app = router(
        engine.services.clone(),
        credentials,
        GatewayConfig {
            allowed_hosts: vec![address.to_string(), format!("localhost:{}", address.port())],
            allowed_origins: config.allowed_origins,
            cancellation: cancellation.clone(),
        },
    )?;
    tracing::info!(endpoint = %format!("http://{address}/mcp"), location = %engine.runtime.location, "AgentFS is ready");
    let mut workers = tokio::task::JoinSet::new();
    for kind in ["autosave", "sync", "cache"] {
        let engine = engine.clone();
        let cancellation = cancellation.clone();
        workers.spawn(async move {
            let period = match kind {
                "autosave" => 1,
                "sync" => 3,
                _ => 30,
            };
            let mut interval = tokio::time::interval(Duration::from_secs(period));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! { _ = cancellation.cancelled() => break, _ = interval.tick() => {} }
                let result = match kind {
                    "autosave" => engine.autosave_due().await.map(|_| ()),
                    "sync" => engine.sync_pending().await,
                    _ => engine.services.cache.collect().await.map(|_| ()),
                };
                if let Err(error) = result {
                    tracing::warn!(task = kind, %error, "background operation will be retried");
                }
            }
        });
    }
    let shutdown = cancellation.clone();
    let server_result = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            shutdown.cancel();
        })
        .await;
    cancellation.cancel();
    while let Some(result) = workers.join_next().await {
        if let Err(error) = result {
            tracing::error!(%error, "background worker stopped unexpectedly");
        }
    }
    engine.flush_local().await?;
    for mount in local.mounts().await? {
        if mount.state == MountState::Ready {
            let context = RequestContext {
                principal: mount.principal.clone(),
                request_id: format!("shutdown-{}", OperationId::new()),
            };
            match engine
                .services
                .mounts
                .unmount(context, mount.id, mount.generation)
                .await
            {
                Ok(record)
                    if record.error.is_none()
                        && record.phase != OperationPhase::WaitingForQuiesce => {}
                Ok(record) => {
                    tracing::warn!(mount = %mount.id, phase = ?record.phase, "mount requires recovery on next startup")
                }
                Err(error) => {
                    tracing::warn!(mount = %mount.id, %error, "mount requires recovery on next startup")
                }
            }
        }
    }
    server_result.map_err(Into::into)
}
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        if let Ok(mut terminate) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}
