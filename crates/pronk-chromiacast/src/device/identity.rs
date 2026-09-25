use pronk_backend_protocol::{
    DisplayIdentity, IdentitySource, Validate, MAX_MANUFACTURER_NAME_BYTES, MAX_PRODUCT_NAME_BYTES,
};

use super::{ControlSetupInfo, DeviceActorError, DeviceControl, MirroringAvailability};
use crate::discovery::DeviceRecord;

pub(super) async fn query_identity(
    selected: &DeviceRecord,
    control: &dyn DeviceControl,
) -> Result<DisplayIdentity, DeviceActorError> {
    let (device_info, setup_info, mirroring) = tokio::join!(
        control.get_device_info(),
        control.get_setup_info(),
        control.get_mirroring_availability(),
    );
    let device_info =
        device_info.map_err(|error| DeviceActorError::DeviceInfoFailed(error.to_string()))?;
    let setup_info = match setup_info {
        Ok(info) => usable_setup_info(selected, info),
        Err(error) => {
            tracing::debug!(%error, "optional Cast setup metadata is unavailable");
            None
        }
    };
    let mirroring = mirroring
        .map_err(|error| DeviceActorError::MirroringAvailabilityFailed(error.to_string()))?;

    if normalize_cast_device_id(&selected.info.device_id)?
        != normalize_cast_device_id(&device_info.device_id)?
    {
        return Err(DeviceActorError::DeviceIdentityChanged);
    }
    if device_info
        .capabilities
        .is_some_and(|capabilities| capabilities & 1 == 0)
    {
        return Err(DeviceActorError::VideoUnavailable);
    }
    if mirroring != MirroringAvailability::Available {
        return Err(DeviceActorError::MirroringUnavailable);
    }

    let (manufacturer, product, product_source) = match setup_info {
        Some(ControlSetupIdentity {
            manufacturer,
            product_name,
        }) => {
            let (product, source) = match product_name {
                Some(product) => (Some(product), IdentitySource::SetupEndpoint),
                None => {
                    let product = device_info
                        .device_model
                        .map(|model| {
                            bounded_identity("device model", model, MAX_PRODUCT_NAME_BYTES)
                        })
                        .transpose()?;
                    let source = if product.is_some() {
                        IdentitySource::AuthenticatedDeviceInfo
                    } else {
                        IdentitySource::Absent
                    };
                    (product, source)
                }
            };
            (manufacturer, product, source)
        }
        None => {
            let product = device_info
                .device_model
                .map(|model| bounded_identity("device model", model, MAX_PRODUCT_NAME_BYTES))
                .transpose()?;
            let source = if product.is_some() {
                IdentitySource::AuthenticatedDeviceInfo
            } else {
                IdentitySource::Absent
            };
            (None, product, source)
        }
    };
    let identity = DisplayIdentity {
        manufacturer_source: if manufacturer.is_some() {
            IdentitySource::SetupEndpoint
        } else {
            IdentitySource::Absent
        },
        manufacturer_name: manufacturer,
        product_name: product,
        product_source,
        pnp_id: None,
    };
    identity
        .validate()
        .map_err(|error| DeviceActorError::InvalidIdentity(error.to_string()))?;
    Ok(identity)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ControlSetupIdentity {
    manufacturer: Option<String>,
    product_name: Option<String>,
}

fn usable_setup_info(
    selected: &DeviceRecord,
    setup: ControlSetupInfo,
) -> Option<ControlSetupIdentity> {
    let ControlSetupInfo::Available {
        manufacturer,
        product_name,
        ssdp_udn,
    } = setup
    else {
        return None;
    };
    let validated = (|| {
        if let Some(ssdp_udn) = ssdp_udn {
            if normalize_cast_device_id(&selected.info.device_id)?
                != normalize_cast_device_id(&ssdp_udn)?
            {
                return Err(DeviceActorError::DeviceIdentityChanged);
            }
        }
        Ok(ControlSetupIdentity {
            manufacturer: manufacturer
                .map(|value| bounded_identity("manufacturer", value, MAX_MANUFACTURER_NAME_BYTES))
                .transpose()?,
            product_name: product_name
                .map(|value| bounded_identity("product name", value, MAX_PRODUCT_NAME_BYTES))
                .transpose()?,
        })
    })();
    match validated {
        Ok(info) => Some(info),
        Err(error) => {
            tracing::debug!(%error, "ignoring unusable Cast setup metadata");
            None
        }
    }
}

pub(super) fn normalize_cast_device_id(value: &str) -> Result<String, DeviceActorError> {
    let value = value.trim();
    let value = value
        .get(..5)
        .filter(|prefix| prefix.eq_ignore_ascii_case("uuid:"))
        .map_or(value, |_| &value[5..]);
    let normalized: String = value
        .bytes()
        .filter(|byte| *byte != b'-')
        .map(|byte| (byte as char).to_ascii_lowercase())
        .collect();
    if normalized.len() != 32 || !normalized.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(DeviceActorError::InvalidIdentity(
            "Cast device ID is not a UUID".into(),
        ));
    }
    Ok(normalized)
}

fn bounded_identity(
    field: &'static str,
    value: String,
    maximum: usize,
) -> Result<String, DeviceActorError> {
    let value = value.trim();
    if value.is_empty() || value.len() > maximum || value.chars().any(char::is_control) {
        return Err(DeviceActorError::InvalidIdentity(format!(
            "{field} is empty, too long, or contains a control character"
        )));
    }
    Ok(value.into())
}
