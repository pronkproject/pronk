//! Polkit authorization for control calls on the public system bus.

use std::collections::HashMap;

use zbus::names::UniqueName;
use zbus::{Connection, Proxy};
use zvariant::{OwnedValue, Str};

const POLKIT_BUS_NAME: &str = "org.freedesktop.PolicyKit1";
const POLKIT_OBJECT_PATH: &str = "/org/freedesktop/PolicyKit1/Authority";
const POLKIT_INTERFACE: &str = "org.freedesktop.PolicyKit1.Authority";
const CONTROL_ACTION: &str = "io.github.pronkproject.Pronk.control-system-service";
const NON_INTERACTIVE: u32 = 0;

pub async fn authorize_control(
    connection: &Connection,
    sender: &UniqueName<'_>,
) -> Result<(), AuthorizationError> {
    let proxy = Proxy::new(
        connection,
        POLKIT_BUS_NAME,
        POLKIT_OBJECT_PATH,
        POLKIT_INTERFACE,
    )
    .await
    .map_err(AuthorizationError::CreateProxy)?;
    let mut subject_details = HashMap::new();
    subject_details.insert(
        "name".to_string(),
        OwnedValue::from(Str::from(sender.as_str())),
    );
    let subject = ("system-bus-name", subject_details);
    let details = HashMap::<String, String>::new();
    let (authorized, _challenge, _details): (bool, bool, HashMap<String, String>) = proxy
        .call(
            "CheckAuthorization",
            // The service never asks Polkit to interact with the user. Clients
            // must explicitly acquire this permission before invoking control
            // methods, for example through a GtkLockButton.
            &(subject, CONTROL_ACTION, details, NON_INTERACTIVE, ""),
        )
        .await
        .map_err(AuthorizationError::CheckAuthorization)?;
    if !authorized {
        return Err(AuthorizationError::Denied);
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum AuthorizationError {
    #[error("create polkit authorization proxy: {0}")]
    CreateProxy(zbus::Error),
    #[error("check polkit authorization: {0}")]
    CheckAuthorization(zbus::Error),
    #[error("polkit denied control of the Pronk system service")]
    Denied,
}
