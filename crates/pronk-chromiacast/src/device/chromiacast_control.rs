use std::fmt::Debug;
use std::net::SocketAddr;

use async_trait::async_trait;
use chromiacast::{AppAvailability, CastApp, CastConnection, SetupInfoOutcome, APP_MIRRORING};
use pronk_backend_protocol::ControlOperation;

use super::{
    ControlDeviceInfo, ControlSetupInfo, DeviceConnector, DeviceControl, DeviceControlError,
    MirroringAvailability,
};
use crate::transport::{
    NegotiatedVideoTransport, VideoTransportConfiguration, VideoTransportError,
    VideoTransportNegotiator,
};

#[derive(Debug, Default)]
pub(crate) struct ChromiacastDeviceConnector;

#[async_trait]
impl DeviceConnector for ChromiacastDeviceConnector {
    async fn connect(
        &self,
        endpoint: SocketAddr,
    ) -> Result<Box<dyn DeviceControl>, DeviceControlError> {
        let connection = CastConnection::connect_address(endpoint)
            .await
            .map_err(|error| DeviceControlError::Connect(error.to_string()))?;
        Ok(Box::new(ChromiacastDeviceControl {
            connection,
            active_app: None,
        }))
    }
}

struct ChromiacastDeviceControl {
    connection: CastConnection,
    active_app: Option<CastApp>,
}

impl Debug for ChromiacastDeviceControl {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ChromiacastDeviceControl")
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl VideoTransportNegotiator for ChromiacastDeviceControl {
    async fn negotiate_video(
        &mut self,
        configuration: VideoTransportConfiguration,
    ) -> Result<NegotiatedVideoTransport, VideoTransportError> {
        if self.active_app.is_some() {
            return Err(VideoTransportError::new(
                "a Cast mirroring application is already active",
            ));
        }
        let app = self
            .connection
            .launch(APP_MIRRORING)
            .await
            .map_err(|error| VideoTransportError::new(format!("launch mirroring app: {error}")))?;
        self.active_app = Some(app);
        let result = crate::cast_transport::negotiate_launched_video(
            &self.connection,
            self.active_app.as_ref().expect("launched app was recorded"),
            configuration,
        )
        .await;
        if result.is_err() {
            let _ = self.stop_video().await;
        }
        result
    }

    async fn stop_video(&mut self) -> Result<(), VideoTransportError> {
        // Stopping crosses a network boundary.  Once the request is sent, a
        // timeout or connection error leaves the receiver's app lifetime
        // ambiguous: it may already have stopped, or the receiver may retain
        // it.  Do not retain a stale local handle that would prevent the next
        // media generation from negotiating a fresh mirroring app.
        let Some(app) = self.active_app.take() else {
            return Ok(());
        };
        self.connection
            .stop(&app)
            .await
            .map_err(|error| VideoTransportError::new(format!("stop Cast mirroring app: {error}")))
    }
}

#[async_trait]
impl DeviceControl for ChromiacastDeviceControl {
    async fn get_device_info(&self) -> Result<ControlDeviceInfo, DeviceControlError> {
        let info = self
            .connection
            .get_device_info()
            .await
            .map_err(|error| DeviceControlError::DeviceInfo(error.to_string()))?;
        Ok(ControlDeviceInfo {
            device_id: info.device_id().into(),
            device_model: info.device_model().map(str::to_owned),
            capabilities: info.capabilities(),
        })
    }

    async fn get_setup_info(&self) -> Result<ControlSetupInfo, DeviceControlError> {
        match self
            .connection
            .get_setup_device_info()
            .await
            .map_err(|error| DeviceControlError::SetupInfo(error.to_string()))?
        {
            SetupInfoOutcome::Available(info) => Ok(ControlSetupInfo::Available {
                manufacturer: info.manufacturer().map(str::to_owned),
                product_name: info.product_name().map(str::to_owned),
                ssdp_udn: info.ssdp_udn().map(str::to_owned),
            }),
            SetupInfoOutcome::Unsupported => Ok(ControlSetupInfo::Unsupported),
            _ => Err(DeviceControlError::SetupInfo(
                "unsupported setup-info outcome".into(),
            )),
        }
    }

    async fn get_mirroring_availability(
        &self,
    ) -> Result<MirroringAvailability, DeviceControlError> {
        match self
            .connection
            .get_app_availability(APP_MIRRORING)
            .await
            .map_err(|error| DeviceControlError::MirroringAvailability(error.to_string()))?
        {
            AppAvailability::Available => Ok(MirroringAvailability::Available),
            AppAvailability::Unavailable => Ok(MirroringAvailability::Unavailable),
            _ => Err(DeviceControlError::MirroringAvailability(
                "unsupported application-availability outcome".into(),
            )),
        }
    }

    async fn transmit_control(
        &mut self,
        operation: &ControlOperation,
    ) -> Result<(), DeviceControlError> {
        super::control::transmit(&self.connection, operation).await
    }

    async fn close(mut self: Box<Self>) -> Result<(), DeviceControlError> {
        // Device shutdown must release its local control owner immediately.
        // A receiver STOP or graceful Cast connection close can wait on remote
        // I/O, so leave that best-effort work to the connection task after its
        // sender is dropped instead of awaiting it in the shutdown path.
        self.active_app.take();
        Ok(())
    }
}
