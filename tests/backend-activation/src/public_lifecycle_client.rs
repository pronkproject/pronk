use std::time::Duration;

use anyhow::{ensure, Context};
use futures_util::StreamExt;
use pronk_dbus::{
    DeviceSelection, DisplaySetupOptions, Manager1Proxy, Operation1Proxy, OperationErrorCode,
    OperationStage,
};
use tokio::time::timeout;

const OPERATION_TIMEOUT: Duration = Duration::from_secs(5);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let connection = zbus::Connection::session()
        .await
        .context("connect to the session bus")?;
    let manager = Manager1Proxy::new(&connection)
        .await
        .context("connect to Pronk")?;
    let inventory = manager.list_devices().await?;
    let device = inventory
        .devices
        .iter()
        .find(|device| device.device_id == "living-room")
        .context("mock living-room Device is missing")?;
    let mut stale = DeviceSelection::from_device(device);
    stale.device_revision = stale
        .device_revision
        .checked_add(1)
        .context("mock Device revision is exhausted")?;

    let path = match manager
        .add_display(stale, DisplaySetupOptions::default())
        .await
    {
        Ok(path) => path,
        Err(error)
            if error
                .to_string()
                .contains("no active local graphical login session") =>
        {
            println!("public_add_display_graphical_session=skip");
            return Ok(());
        }
        Err(error) => return Err(error).context("start stale AddDisplay operation"),
    };
    let operation = Operation1Proxy::builder(&connection)
        .path(path.clone())?
        .build()
        .await?;
    let mut changes = operation.receive_state_changed().await?;
    let mut state = operation.get_state().await?;
    timeout(OPERATION_TIMEOUT, async {
        while !state.stage.is_terminal() {
            let signal = changes
                .next()
                .await
                .context("operation StateChanged stream closed")?;
            state = signal.args()?.state().clone();
        }
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("stale AddDisplay operation did not finish")??;
    state.validate()?;
    ensure!(
        state.stage == OperationStage::Failed,
        "stale AddDisplay ended in {:?}: {}",
        state.stage,
        state.error
    );
    ensure!(
        state.error_code == OperationErrorCode::DeviceChanged,
        "stale AddDisplay returned error code {:?}",
        state.error_code
    );
    ensure!(
        state.error.contains("changed since it was selected"),
        "stale AddDisplay returned the wrong diagnostic: {}",
        state.error
    );
    ensure!(
        manager.list_displays().await?.displays.is_empty(),
        "failed AddDisplay created a public cast display"
    );
    ensure!(
        !operation.cancel().await?,
        "terminal AddDisplay operation accepted cancellation"
    );
    manager.remove_display(state.display_id).await?;

    println!("public_add_display_operation=pass");
    println!("public_stale_device_no_side_effect=pass");
    println!("public_remove_display_idempotent=pass");
    Ok(())
}
