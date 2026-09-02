use std::ffi::OsStr;
use std::ffi::OsString;
use std::process::Command;

use anyhow::Context;
use futures_util::StreamExt;
use nix::unistd::{Uid, User};
use pronk_dbus::{
    cast_display_object_path, CastDisplay1Proxy, DeviceSelection, DisplaySetupOptions,
    Manager1Proxy, MediaSession1Proxy, MediaSessionState, Operation1Proxy, OperationStage,
    OperationState, API_FEATURE_CAST_DISPLAY_DYNAMIC_STATE, API_FEATURE_CAST_DISPLAY_LIFECYCLE,
    API_FEATURE_CAST_DISPLAY_STATE, API_FEATURE_DEVICE_INVENTORY, API_FEATURE_MEDIA_SESSION_STATE,
    API_MAJOR,
};

const USAGE: &str = "usage:
  pronkctl [--session|--system] list-devices
  pronkctl [--session|--system] list-displays
  pronkctl [--session|--system] add-display --device <backend-id>:<device-id> [--no-audio]
  pronkctl [--session|--system] remove-display <display-id>";
const PKEXEC_PATH: &str = "/usr/bin/pkexec";
const SYSTEM_SERVICE_USER: &str = "pronk";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Bus {
    Session,
    System,
}

#[derive(Debug, PartialEq, Eq)]
enum Action {
    Version,
    Help,
    ListDevices,
    ListDisplays,
    AddDisplay {
        device: OsString,
        audio_enabled: bool,
    },
    RemoveDisplay(OsString),
}

impl Action {
    fn uses_service(&self) -> bool {
        !matches!(self, Self::Version | Self::Help)
    }
}

fn main() -> anyhow::Result<()> {
    let arguments: Vec<_> = std::env::args_os().skip(1).collect();
    let (bus, action) = parse_arguments(arguments.clone())?;
    if bus == Bus::System && action.uses_service() {
        reexecute_as_system_service_user(&arguments)?;
    }

    match &action {
        Action::Version => {
            println!("pronkctl {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Action::Help => {
            println!("{USAGE}");
            return Ok(());
        }
        _ => {}
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("create Tokio runtime")?;
    runtime.block_on(run_action(bus, action))
}

fn parse_arguments(mut arguments: Vec<OsString>) -> anyhow::Result<(Bus, Action)> {
    let bus = match arguments.first().and_then(|argument| argument.to_str()) {
        Some("--system") => {
            arguments.remove(0);
            Bus::System
        }
        Some("--session") => {
            arguments.remove(0);
            Bus::Session
        }
        _ => Bus::Session,
    };
    let action = match arguments.as_slice() {
        [command] if command == "--version" => Action::Version,
        [command] if command == "--help" || command == "-h" => Action::Help,
        [command] if command == "list-devices" => Action::ListDevices,
        [command] if command == "list-displays" => Action::ListDisplays,
        [command, option, device] if command == "add-display" && option == "--device" => {
            Action::AddDisplay {
                device: device.clone(),
                audio_enabled: true,
            }
        }
        [command, option, device, audio]
            if command == "add-display" && option == "--device" && audio == "--no-audio" =>
        {
            Action::AddDisplay {
                device: device.clone(),
                audio_enabled: false,
            }
        }
        [command, display_id] if command == "remove-display" => {
            Action::RemoveDisplay(display_id.clone())
        }
        _ => anyhow::bail!(USAGE),
    };
    Ok((bus, action))
}

fn reexecute_as_system_service_user(arguments: &[OsString]) -> anyhow::Result<()> {
    let service_user = User::from_name(SYSTEM_SERVICE_USER)
        .context("resolve the pronk system account")?
        .context("the pronk system account is not installed")?;
    anyhow::ensure!(
        !service_user.uid.is_root(),
        "the pronk system account must not be root"
    );
    if Uid::effective() == service_user.uid {
        return Ok(());
    }

    let executable = std::env::current_exe().context("resolve the current pronkctl executable")?;
    let status = Command::new(PKEXEC_PATH)
        .arg("--user")
        .arg(SYSTEM_SERVICE_USER)
        .arg(executable)
        .args(arguments)
        .status()
        .context("authorize system-service control with polkit")?;
    if let Some(code) = status.code() {
        std::process::exit(code);
    }
    anyhow::bail!("pkexec was terminated before pronkctl completed")
}

async fn run_action(bus: Bus, action: Action) -> anyhow::Result<()> {
    match action {
        Action::ListDevices => list_devices(bus).await,
        Action::ListDisplays => list_displays(bus).await,
        Action::AddDisplay {
            device,
            audio_enabled,
        } => add_display(bus, &device, audio_enabled).await,
        Action::RemoveDisplay(display_id) => remove_display(bus, &display_id).await,
        Action::Version | Action::Help => unreachable!("local actions return before runtime setup"),
    }
}

async fn list_displays(bus: Bus) -> anyhow::Result<()> {
    let client = Client::connect(bus).await?;
    let proxy = client
        .manager_with_feature(
            API_FEATURE_CAST_DISPLAY_LIFECYCLE
                | API_FEATURE_CAST_DISPLAY_STATE
                | API_FEATURE_CAST_DISPLAY_DYNAMIC_STATE
                | API_FEATURE_MEDIA_SESSION_STATE,
            "cast-display lifecycle, dynamic state, and media state",
        )
        .await?;
    let snapshot = proxy.list_displays().await.context("list cast displays")?;
    snapshot
        .validate()
        .context("Pronk returned an invalid cast-display inventory")?;

    if snapshot.displays.is_empty() {
        println!("No cast displays set up.");
        return Ok(());
    }

    for display in snapshot.displays {
        let path = cast_display_object_path(&display.display_id)?;
        let display_proxy = CastDisplay1Proxy::builder(&client.connection)
            .path(path.clone())?
            .build()
            .await
            .with_context(|| format!("connect to cast display {}", display.display_id))?;
        let media_proxy = MediaSession1Proxy::builder(&client.connection)
            .path(path)?
            .build()
            .await
            .with_context(|| format!("connect to media session for {}", display.display_id))?;
        let state = display_proxy
            .get_state()
            .await
            .with_context(|| format!("read cast-display state for {}", display.display_id))?;
        state
            .validate()
            .with_context(|| format!("invalid cast-display state for {}", display.display_id))?;
        let media = media_proxy
            .get_state()
            .await
            .with_context(|| format!("read media-session state for {}", display.display_id))?;
        media
            .validate()
            .with_context(|| format!("invalid media-session state for {}", display.display_id))?;

        println!("{}", state.device.display_name);
        println!("  ID: {}", display.display_id);
        println!("  Device: {}:{}", display.backend_id, display.device_id);
        println!("  Status: {}", state.device.availability);
        println!("  Attachment: {}", state.attachment_state);
        match state.routed_mode {
            Some(mode) => println!(
                "  Route: {} ({}x{} @ {:.3} Hz)",
                state.route_state,
                mode.width,
                mode.height,
                f64::from(mode.refresh_millihz) / 1_000.0
            ),
            None => println!("  Route: {}", state.route_state),
        }
        println!("  Media: {}", format_media_status(&media));
        println!(
            "  Audio: {}",
            if media.audio_enabled {
                "Enabled"
            } else {
                "Disabled"
            }
        );
        if !media.error.is_empty() {
            println!("  Media error: {}", media.error);
        }
        let product = match (
            display.manufacturer_name.is_empty(),
            display.product_name.is_empty(),
        ) {
            (false, false) => format!("{} {}", display.manufacturer_name, display.product_name),
            (false, true) => display.manufacturer_name,
            (true, false) => display.product_name,
            (true, true) => display.pnp_id.clone(),
        };
        println!("  Product: {product} ({})", display.pnp_id);
        println!(
            "  Output: {} (connector {}, slot {})",
            display.connector_name, display.connector_id, display.output_index
        );
    }
    Ok(())
}

fn format_media_status(state: &MediaSessionState) -> String {
    if state.media_generation == 0 {
        state.phase.to_string()
    } else {
        format!("{} (generation {})", state.phase, state.media_generation)
    }
}

async fn list_devices(bus: Bus) -> anyhow::Result<()> {
    let client = Client::connect(bus).await?;
    let proxy = client
        .manager_with_feature(API_FEATURE_DEVICE_INVENTORY, "Device inventory")
        .await?;
    let snapshot = proxy.list_devices().await.context("list casting devices")?;
    snapshot
        .validate()
        .context("Pronk returned an invalid device inventory")?;

    if snapshot.devices.is_empty() {
        println!("No casting devices found.");
        return Ok(());
    }

    for device in snapshot.devices {
        println!("{}", device.display_name);
        println!("  ID: {}:{}", device.backend_id, device.device_id);
        println!("  Status: {}", device.availability);
    }
    Ok(())
}

async fn add_display(bus: Bus, device_argument: &OsStr, audio_enabled: bool) -> anyhow::Result<()> {
    let device_argument = device_argument
        .to_str()
        .context("Device selector is not valid UTF-8")?;
    let (backend_id, device_id) = parse_device_target(device_argument)?;
    let client = Client::connect(bus).await?;
    let proxy = client
        .manager_with_feature(
            API_FEATURE_DEVICE_INVENTORY | API_FEATURE_CAST_DISPLAY_LIFECYCLE,
            "Device inventory and cast-display lifecycle",
        )
        .await?;
    let inventory = proxy.list_devices().await.context("list casting Devices")?;
    inventory
        .validate()
        .context("Pronk returned an invalid Device inventory")?;
    let device = inventory
        .devices
        .iter()
        .find(|device| device.backend_id == backend_id && device.device_id == device_id)
        .with_context(|| format!("Device {device_argument:?} was not found"))?;
    let operation_path = proxy
        .add_display(
            DeviceSelection::from_device(device),
            DisplaySetupOptions { audio_enabled },
        )
        .await
        .with_context(|| format!("start setup for Device {device_argument:?}"))?;
    let operation = Operation1Proxy::builder(&client.connection)
        .path(operation_path.clone())?
        .build()
        .await
        .context("connect to the AddDisplay operation")?;

    println!("Setting up {} ({device_argument})", device.display_name);
    let state = wait_for_operation(&operation).await?;
    match state.stage {
        OperationStage::Added => {
            println!("Added cast display {}", state.display_id);
            Ok(())
        }
        OperationStage::Cancelled | OperationStage::Failed => anyhow::bail!(
            "AddDisplay {:?}: {:?}: {}",
            state.stage,
            state.error_code,
            state.error
        ),
        _ => unreachable!("operation waiter returned a nonterminal state"),
    }
}

async fn wait_for_operation(operation: &Operation1Proxy<'_>) -> anyhow::Result<OperationState> {
    // Subscribe first so a transition between subscription and GetState is
    // either represented by the snapshot or remains queued in the stream.
    let mut changes = operation
        .receive_state_changed()
        .await
        .context("subscribe to AddDisplay state")?;
    let mut state = operation
        .get_state()
        .await
        .context("read initial AddDisplay state")?;
    state.validate().context("invalid AddDisplay state")?;
    print_operation_stage(state.stage);
    let mut cancellation_requested = false;

    while !state.stage.is_terminal() {
        tokio::select! {
            signal = changes.next() => {
                let signal = signal.context("AddDisplay state stream closed")?;
                let next = signal.args()?.state().clone();
                next.validate().context("invalid AddDisplay state change")?;
                // A signal queued before GetState may describe an older stage.
                if operation_stage_rank(next.stage) >= operation_stage_rank(state.stage) {
                    if next.stage != state.stage {
                        print_operation_stage(next.stage);
                    }
                    state = next;
                }
            }
            result = tokio::signal::ctrl_c(), if !cancellation_requested => {
                result.context("wait for interrupt")?;
                cancellation_requested = true;
                if operation.cancel().await.context("cancel AddDisplay operation")? {
                    eprintln!("Cancellation requested");
                } else {
                    state = operation.get_state().await.context("read terminal AddDisplay state")?;
                    state.validate().context("invalid terminal AddDisplay state")?;
                }
            }
        }
    }
    Ok(state)
}

async fn remove_display(bus: Bus, display_id: &OsStr) -> anyhow::Result<()> {
    let display_id = display_id
        .to_str()
        .context("cast-display ID is not valid UTF-8")?;
    let client = Client::connect(bus).await?;
    let proxy = client
        .manager_with_feature(API_FEATURE_CAST_DISPLAY_LIFECYCLE, "cast-display lifecycle")
        .await?;
    proxy
        .remove_display(display_id.to_owned())
        .await
        .with_context(|| format!("remove cast display {display_id}"))?;
    println!("Removed cast display {display_id}");
    Ok(())
}

fn parse_device_target(value: &str) -> anyhow::Result<(&str, &str)> {
    let (backend_id, device_id) = value
        .split_once(':')
        .context("Device selector must be <backend-id>:<device-id>")?;
    anyhow::ensure!(!backend_id.is_empty(), "Device selector has no backend ID");
    anyhow::ensure!(!device_id.is_empty(), "Device selector has no Device ID");
    Ok((backend_id, device_id))
}

fn operation_stage_rank(stage: OperationStage) -> u32 {
    match stage {
        OperationStage::Validating => 1,
        OperationStage::Authorizing => 2,
        OperationStage::PreparingDevice => 3,
        OperationStage::Attaching => 4,
        OperationStage::Added | OperationStage::Cancelled | OperationStage::Failed => 5,
    }
}

fn print_operation_stage(stage: OperationStage) {
    let name = match stage {
        OperationStage::Validating => "Validating",
        OperationStage::Authorizing => "Authorizing",
        OperationStage::PreparingDevice => "Preparing Device",
        OperationStage::Attaching => "Attaching display",
        OperationStage::Added => "Added",
        OperationStage::Cancelled => "Cancelled",
        OperationStage::Failed => "Failed",
    };
    println!("  {name}");
}

struct Client {
    connection: zbus::Connection,
}

impl Client {
    async fn connect(bus: Bus) -> anyhow::Result<Self> {
        let connection = match bus {
            Bus::Session => zbus::Connection::session()
                .await
                .context("connect to the session bus")?,
            Bus::System => zbus::Connection::system()
                .await
                .context("connect to the system bus")?,
        };
        Ok(Self { connection })
    }

    async fn manager_with_feature(
        &self,
        features: u64,
        feature_name: &str,
    ) -> anyhow::Result<Manager1Proxy<'_>> {
        let proxy = Manager1Proxy::new(&self.connection)
            .await
            .context("connect to Pronk")?;
        let version = proxy
            .get_version()
            .await
            .context("query Pronk API version")?;
        anyhow::ensure!(
            version.major == API_MAJOR,
            "Pronk API major {} is incompatible with pronkctl major {API_MAJOR}",
            version.major
        );
        anyhow::ensure!(
            version.features & features == features,
            "Pronk does not advertise {feature_name} support"
        );
        Ok(proxy)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_target_keeps_colons_in_the_backend_stable_id() {
        assert_eq!(
            parse_device_target("mock:living-room:cast").unwrap(),
            ("mock", "living-room:cast")
        );
        assert!(parse_device_target("mock").is_err());
        assert!(parse_device_target(":living-room").is_err());
        assert!(parse_device_target("mock:").is_err());
    }

    #[test]
    fn terminal_operation_stages_have_one_progress_rank() {
        assert_eq!(operation_stage_rank(OperationStage::Added), 5);
        assert_eq!(operation_stage_rank(OperationStage::Cancelled), 5);
        assert_eq!(operation_stage_rank(OperationStage::Failed), 5);
    }

    #[test]
    fn media_status_preserves_public_phase_and_generation() {
        let mut state = MediaSessionState {
            revision: 1,
            phase: pronk_dbus::MediaSessionPhase::Inactive,
            media_generation: 0,
            audio_enabled: false,
            error: String::new(),
        };
        assert_eq!(format_media_status(&state), "Inactive");

        state.phase = pronk_dbus::MediaSessionPhase::Recovering;
        state.media_generation = 4;
        assert_eq!(format_media_status(&state), "Recovering (generation 4)");
    }

    #[test]
    fn system_mode_parses_before_requesting_polkit_authorization() {
        let (bus, action) = parse_arguments(vec![
            "--system".into(),
            "add-display".into(),
            "--device".into(),
            "mock:living-room".into(),
            "--no-audio".into(),
        ])
        .unwrap();
        assert_eq!(bus, Bus::System);
        assert_eq!(
            action,
            Action::AddDisplay {
                device: "mock:living-room".into(),
                audio_enabled: false,
            }
        );
        assert!(action.uses_service());

        let (bus, action) = parse_arguments(vec!["--system".into(), "--help".into()]).unwrap();
        assert_eq!(bus, Bus::System);
        assert_eq!(action, Action::Help);
        assert!(!action.uses_service());
        assert!(parse_arguments(vec!["--system".into(), "unknown".into()]).is_err());
    }
}
