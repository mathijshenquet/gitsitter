#[cfg(windows)]
mod imp {
    use std::ffi::OsString;
    use std::time::Duration;

    use anyhow::{Context, Result};
    use tokio::runtime::Runtime;
    use windows_service::define_windows_service;
    use windows_service::service::{
        ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
        ServiceType,
    };
    use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
    use windows_service::service_dispatcher;

    use crate::daemon;
    use crate::transport::{self, Request, Response};

    pub const SERVICE_NAME: &str = "gitsitter";
    pub const SERVICE_DISPLAY_NAME: &str = "gitsitter";

    define_windows_service!(ffi_service_main, service_main);

    pub fn run_service_dispatcher() -> Result<()> {
        service_dispatcher::start(SERVICE_NAME, ffi_service_main)
            .context("failed to start Windows service dispatcher")
    }

    fn service_main(_args: Vec<OsString>) {
        if let Err(err) = run_service() {
            eprintln!("service error: {err:#}");
        }
    }

    fn run_service() -> Result<()> {
        let (shutdown_tx, shutdown_rx) = std::sync::mpsc::channel::<()>();

        let status_handle = service_control_handler::register(SERVICE_NAME, move |control| {
            match control {
                ServiceControl::Stop | ServiceControl::Shutdown => {
                    let _ = shutdown_tx.send(());
                    ServiceControlHandlerResult::NoError
                }
                ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
                _ => ServiceControlHandlerResult::NotImplemented,
            }
        })
        .context("failed to register Windows service control handler")?;

        status_handle
            .set_service_status(service_status(
                ServiceState::StartPending,
                ServiceControlAccept::empty(),
            ))
            .context("failed to mark Windows service start pending")?;

        let runtime = Runtime::new().context("failed to create Tokio runtime for service")?;

        runtime.block_on(async move {
            let daemon_task = tokio::spawn(async { daemon::run_daemon().await });

            wait_for_daemon_ready().await;

            status_handle
                .set_service_status(service_status(
                    ServiceState::Running,
                    ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
                ))
                .context("failed to mark Windows service running")?;

            let _ = shutdown_rx.recv();
            request_daemon_shutdown().await;

            let daemon_result = daemon_task
                .await
                .context("Windows service daemon task join failed")?;

            status_handle
                .set_service_status(service_status(
                    ServiceState::Stopped,
                    ServiceControlAccept::empty(),
                ))
                .context("failed to mark Windows service stopped")?;

            daemon_result
        })
    }

    fn service_status(
        current_state: ServiceState,
        controls_accepted: ServiceControlAccept,
    ) -> ServiceStatus {
        ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state,
            controls_accepted,
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: Duration::from_secs(5),
            process_id: None,
        }
    }

    async fn wait_for_daemon_ready() {
        for _ in 0..40 {
            if transport::is_daemon_running() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn request_daemon_shutdown() {
        let Ok(mut stream) = transport::connect_to_daemon().await else {
            return;
        };
        let _ = transport::send_request(&mut stream, &Request::Shutdown).await;
        let _ = transport::recv_response(&mut stream).await.map(|resp| match resp {
            Response::Ok { .. } | Response::Error { .. } => (),
            _ => (),
        });
    }
}

#[cfg(windows)]
pub use imp::{run_service_dispatcher, SERVICE_DISPLAY_NAME, SERVICE_NAME};

#[cfg(not(windows))]
pub const SERVICE_NAME: &str = "gitsitter";
#[cfg(not(windows))]
pub const SERVICE_DISPLAY_NAME: &str = "gitsitter";

#[cfg(not(windows))]
pub fn run_service_dispatcher() -> anyhow::Result<()> {
    anyhow::bail!("Windows service mode is only available on Windows")
}
