//! Generation-safe ownership of the CastKMS userspace CEC transport.
//!
//! This layer understands only the connector-scoped kernel UAPI. HDMI-CEC
//! opcode policy and Device-specific control remain above it.

use std::num::{NonZeroU32, NonZeroU64};
use std::os::fd::AsRawFd;

use castkms_sys::{
    drm_ioctl_castkms_cec_bind_transport, drm_ioctl_castkms_cec_query_caps,
    drm_ioctl_castkms_cec_receive, drm_ioctl_castkms_cec_set_transport_state,
    drm_ioctl_castkms_cec_tx_complete, drm_ioctl_castkms_cec_unbind_transport,
    DrmCastkmsCecBindTransport, DrmCastkmsCecQueryCaps, DrmCastkmsCecReceive,
    DrmCastkmsCecSetTransportState, DrmCastkmsCecTxComplete, DrmCastkmsCecUnbindTransport,
    CEC_CAP_ASYNC_TX, CEC_CAP_EDID_PHYS_ADDR, CEC_CAP_RX_INJECT, CEC_CAP_TRANSPORT_STATE,
    CEC_STATE_MASK, CEC_STATE_MONITOR_ATTACHED, CEC_STATE_TRANSPORT_ONLINE, CEC_TRANSPORT_ONLINE,
    CEC_TX_STATUS_ERROR, CEC_TX_STATUS_NACK, CEC_TX_STATUS_OK, CEC_UAPI_MAJOR, CEC_UAPI_MINOR,
    GRANT_MANAGE_CEC,
};

use super::{CastKmsClient, CastKmsError, CecTransmitEvent};

pub const CEC_REQUIRED_CAPABILITIES: u64 =
    CEC_CAP_ASYNC_TX | CEC_CAP_RX_INJECT | CEC_CAP_TRANSPORT_STATE | CEC_CAP_EDID_PHYS_ADDR;
const CEC_MAX_MESSAGE_SIZE: u32 = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CecCapabilities {
    connector_id: NonZeroU32,
    output_index: u32,
    capabilities: u64,
    uapi_minor: u32,
}

impl CecCapabilities {
    pub fn connector_id(self) -> NonZeroU32 {
        self.connector_id
    }

    pub fn output_index(self) -> u32 {
        self.output_index
    }

    pub fn capabilities(self) -> u64 {
        self.capabilities
    }

    pub fn uapi_minor(self) -> u32 {
        self.uapi_minor
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CecTransportBinding {
    pub transport_id: NonZeroU32,
    pub transport_generation: NonZeroU64,
    pub state_generation: NonZeroU64,
    pub output_index: u32,
    pub physical_address: u16,
    pub logical_address_mask: u16,
    pub online: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CecCompletion {
    status: u8,
    arb_lost_count: u8,
    nack_count: u8,
    low_drive_count: u8,
    error_count: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CecTransmitAdmission {
    Accepted,
    Stale,
}

impl CecCompletion {
    pub const fn succeeded() -> Self {
        Self {
            status: CEC_TX_STATUS_OK,
            arb_lost_count: 0,
            nack_count: 0,
            low_drive_count: 0,
            error_count: 0,
        }
    }

    pub const fn not_acknowledged(attempts: u8) -> Self {
        Self {
            status: CEC_TX_STATUS_NACK,
            arb_lost_count: 0,
            nack_count: attempts,
            low_drive_count: 0,
            error_count: 0,
        }
    }

    pub const fn failed() -> Self {
        Self {
            status: CEC_TX_STATUS_ERROR,
            arb_lost_count: 0,
            nack_count: 0,
            low_drive_count: 0,
            error_count: 1,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PendingTransmit {
    transport_generation: NonZeroU64,
    cookie: NonZeroU64,
}

#[derive(Debug, Default)]
pub(super) struct CecTracker {
    binding: Option<CecTransportBinding>,
    pending: Option<PendingTransmit>,
    requires_state_advance: bool,
}

impl CecTracker {
    pub(super) fn is_bound(&self) -> bool {
        self.binding.is_some()
    }

    fn binding(&self) -> Result<CecTransportBinding, CastKmsError> {
        self.binding
            .ok_or(CastKmsError::InvalidCecState("CEC transport is not bound"))
    }

    fn install(&mut self, binding: CecTransportBinding) -> Result<(), CastKmsError> {
        if self.binding.is_some() {
            return Err(CastKmsError::InvalidCecState(
                "CEC transport is already bound",
            ));
        }
        self.binding = Some(binding);
        Ok(())
    }

    fn set_online(&mut self, online: bool) -> Result<(), CastKmsError> {
        if self.pending.is_some() && !online {
            return Err(CastKmsError::InvalidCecState(
                "cannot take CEC transport offline with a pending transmit",
            ));
        }
        let binding = self
            .binding
            .as_mut()
            .ok_or(CastKmsError::InvalidCecState("CEC transport is not bound"))?;
        if binding.online != online {
            // CastKMS advances its state generation for both transitions.
            // Require the next transmit event to prove it belongs to the new
            // online interval before accepting it.
            self.requires_state_advance = true;
            binding.online = online;
        }
        Ok(())
    }

    fn record_authority_suspended(&mut self) -> Result<(), CastKmsError> {
        let binding = self
            .binding
            .as_mut()
            .ok_or(CastKmsError::InvalidCecState("CEC transport is not bound"))?;
        binding.online = false;
        self.pending = None;
        self.requires_state_advance = true;
        Ok(())
    }

    fn record_transmit(
        &mut self,
        event: &CecTransmitEvent,
    ) -> Result<CecTransmitAdmission, CastKmsError> {
        let mut binding = self.binding()?;
        if !binding.online {
            return Ok(CecTransmitAdmission::Stale);
        }
        if event.transport_id != binding.transport_id.get()
            || event.transport_generation != binding.transport_generation.get()
            || event.output_index != binding.output_index
        {
            return Err(CastKmsError::InvalidCecMetadata(
                "transmit binding identity",
            ));
        }
        let state_generation = NonZeroU64::new(event.state_generation).ok_or(
            CastKmsError::InvalidCecMetadata("transmit state generation"),
        )?;
        if state_generation < binding.state_generation
            || (self.requires_state_advance && state_generation == binding.state_generation)
        {
            return Ok(CecTransmitAdmission::Stale);
        }
        let pending = PendingTransmit {
            transport_generation: NonZeroU64::new(event.transport_generation).ok_or(
                CastKmsError::InvalidCecMetadata("transmit transport generation"),
            )?,
            cookie: NonZeroU64::new(event.cookie)
                .ok_or(CastKmsError::InvalidCecMetadata("transmit cookie"))?,
        };
        if self.pending.is_some() {
            return Err(CastKmsError::InvalidCecState(
                "a second CEC transmit arrived before completion",
            ));
        }
        binding.state_generation = state_generation;
        self.binding = Some(binding);
        self.requires_state_advance = false;
        self.pending = Some(pending);
        Ok(CecTransmitAdmission::Accepted)
    }

    fn validate_completion(&self, event: &CecTransmitEvent) -> Result<(), CastKmsError> {
        let pending = self.pending.ok_or(CastKmsError::InvalidCecState(
            "CEC transmit has no pending transaction",
        ))?;
        if pending.transport_generation.get() != event.transport_generation
            || pending.cookie.get() != event.cookie
        {
            return Err(CastKmsError::InvalidCecMetadata(
                "transmit completion identity",
            ));
        }
        Ok(())
    }

    fn complete(&mut self) {
        self.pending = None;
    }

    fn remove(&mut self) -> Result<CecTransportBinding, CastKmsError> {
        if self.pending.is_some() {
            return Err(CastKmsError::InvalidCecState(
                "cannot unbind CEC transport with a pending transmit",
            ));
        }
        let binding = self
            .binding
            .take()
            .ok_or(CastKmsError::InvalidCecState("CEC transport is not bound"))?;
        self.requires_state_advance = false;
        Ok(binding)
    }
}

fn decode_cec_capabilities(
    connector_id: u32,
    query: DrmCastkmsCecQueryCaps,
) -> Result<Option<CecCapabilities>, CastKmsError> {
    if query.connector_id != connector_id {
        return Err(CastKmsError::InvalidCecMetadata("connector identity"));
    }
    if query.flags != 0 || query.reserved != 0 {
        return Err(CastKmsError::InvalidCecMetadata("capability padding"));
    }
    if query.uapi_major != CEC_UAPI_MAJOR || query.uapi_minor < CEC_UAPI_MINOR {
        return Err(CastKmsError::InvalidCecMetadata("UAPI version"));
    }
    if query.capabilities & CEC_REQUIRED_CAPABILITIES != CEC_REQUIRED_CAPABILITIES {
        return Err(CastKmsError::InvalidCecMetadata("required capability bits"));
    }
    if query.max_msg_size != CEC_MAX_MESSAGE_SIZE {
        return Err(CastKmsError::InvalidCecMetadata("maximum message size"));
    }
    if query.has_adapter > 1 {
        return Err(CastKmsError::InvalidCecMetadata("adapter availability"));
    }
    if query.has_adapter == 0 {
        return Ok(None);
    }
    Ok(Some(CecCapabilities {
        connector_id: NonZeroU32::new(query.connector_id)
            .ok_or(CastKmsError::InvalidCecMetadata("connector ID"))?,
        output_index: query.output_index,
        capabilities: query.capabilities,
        uapi_minor: query.uapi_minor,
    }))
}

impl CastKmsClient {
    pub fn cec_transport_bound(&self) -> bool {
        self.cec.is_bound()
    }

    pub fn cec_transport_online(&self) -> bool {
        self.cec.binding().is_ok_and(|binding| binding.online)
    }

    /// Reconcile the local tracker with CastKMS's synchronous authority
    /// suspension hook. The binding remains owned by this holder, but any
    /// pending transaction has already been aborted and the transport must be
    /// explicitly brought online after authority becomes active again.
    pub fn record_cec_authority_suspended(&mut self) -> Result<(), CastKmsError> {
        self.cec.record_authority_suspended()
    }

    pub fn query_cec_capabilities(&self) -> Result<Option<CecCapabilities>, CastKmsError> {
        self.require_rights(GRANT_MANAGE_CEC)?;
        let mut query = DrmCastkmsCecQueryCaps {
            connector_id: self.connector_id(),
            ..DrmCastkmsCecQueryCaps::default()
        };
        // SAFETY: `query` is the checked-in fixed-width UAPI structure and
        // remains writable for the synchronous ioctl.
        unsafe { drm_ioctl_castkms_cec_query_caps(self.as_raw_fd(), &mut query) }
            .map_err(CastKmsError::QueryCecCapabilities)?;
        decode_cec_capabilities(self.connector_id(), query)
    }

    pub fn bind_cec_transport(
        &mut self,
        capabilities: CecCapabilities,
    ) -> Result<CecTransportBinding, CastKmsError> {
        self.require_rights(GRANT_MANAGE_CEC)?;
        if self.cec.is_bound() {
            return Err(CastKmsError::InvalidCecState(
                "CEC transport is already bound",
            ));
        }
        if capabilities.connector_id.get() != self.connector_id() {
            return Err(CastKmsError::InvalidCecMetadata(
                "capability connector identity",
            ));
        }
        let mut bind = DrmCastkmsCecBindTransport {
            connector_id: self.connector_id(),
            ..DrmCastkmsCecBindTransport::default()
        };
        // SAFETY: `bind` is the checked-in fixed-width UAPI structure and
        // remains writable for the synchronous ioctl.
        unsafe { drm_ioctl_castkms_cec_bind_transport(self.as_raw_fd(), &mut bind) }
            .map_err(CastKmsError::BindCecTransport)?;
        if bind.connector_id != self.connector_id()
            || bind.flags != 0
            || bind.reserved != 0
            || bind.pad0 != 0
        {
            return Err(CastKmsError::InvalidCecMetadata(
                "binding identity or padding",
            ));
        }
        if bind.output_index != capabilities.output_index {
            return Err(CastKmsError::InvalidCecMetadata("binding output identity"));
        }
        if bind.state_flags & !CEC_STATE_MASK != 0
            || bind.state_flags & CEC_STATE_TRANSPORT_ONLINE != 0
            || bind.state_flags & CEC_STATE_MONITOR_ATTACHED == 0
        {
            return Err(CastKmsError::InvalidCecMetadata("initial binding state"));
        }
        let binding = CecTransportBinding {
            transport_id: NonZeroU32::new(bind.transport_id)
                .ok_or(CastKmsError::InvalidCecMetadata("transport ID"))?,
            transport_generation: NonZeroU64::new(bind.transport_generation)
                .ok_or(CastKmsError::InvalidCecMetadata("transport generation"))?,
            state_generation: NonZeroU64::new(bind.state_generation)
                .ok_or(CastKmsError::InvalidCecMetadata("state generation"))?,
            output_index: bind.output_index,
            physical_address: bind.phys_addr,
            logical_address_mask: bind.logical_addr_mask,
            online: false,
        };
        self.cec.install(binding)?;
        Ok(binding)
    }

    pub fn set_cec_transport_online(&mut self, online: bool) -> Result<(), CastKmsError> {
        let binding = self.cec.binding()?;
        if binding.online == online {
            return Ok(());
        }
        if !online && self.cec.pending.is_some() {
            return Err(CastKmsError::InvalidCecState(
                "cannot take CEC transport offline with a pending transmit",
            ));
        }
        let state = DrmCastkmsCecSetTransportState {
            connector_id: self.connector_id(),
            transport_id: binding.transport_id.get(),
            flags: if online { CEC_TRANSPORT_ONLINE } else { 0 },
            ..DrmCastkmsCecSetTransportState::default()
        };
        // SAFETY: `state` is a checked-in fixed-width input structure that
        // remains valid for the synchronous ioctl.
        unsafe { drm_ioctl_castkms_cec_set_transport_state(self.as_raw_fd(), &state) }
            .map_err(CastKmsError::SetCecTransportState)?;
        self.cec.set_online(online)
    }

    pub fn record_cec_transmit(
        &mut self,
        event: &CecTransmitEvent,
    ) -> Result<CecTransmitAdmission, CastKmsError> {
        if event.connector_id != self.connector_id() {
            return Err(CastKmsError::InvalidCecMetadata(
                "transmit connector identity",
            ));
        }
        self.cec.record_transmit(event)
    }

    pub fn complete_cec_transmit(
        &mut self,
        event: &CecTransmitEvent,
        completion: CecCompletion,
    ) -> Result<(), CastKmsError> {
        self.cec.validate_completion(event)?;
        let binding = self.cec.binding()?;
        let complete = DrmCastkmsCecTxComplete {
            connector_id: self.connector_id(),
            transport_id: binding.transport_id.get(),
            transport_generation: event.transport_generation,
            cookie: event.cookie,
            status: completion.status,
            arb_lost_cnt: completion.arb_lost_count,
            nack_cnt: completion.nack_count,
            low_drive_cnt: completion.low_drive_count,
            error_cnt: completion.error_count,
            ..DrmCastkmsCecTxComplete::default()
        };
        // SAFETY: `complete` is a checked-in fixed-width input structure that
        // remains valid for the synchronous ioctl.
        unsafe { drm_ioctl_castkms_cec_tx_complete(self.as_raw_fd(), &complete) }
            .map_err(CastKmsError::CompleteCecTransmit)?;
        self.cec.complete();
        Ok(())
    }

    pub fn inject_cec_message(&mut self, message: &[u8]) -> Result<(), CastKmsError> {
        if message.is_empty() || message.len() > CEC_MAX_MESSAGE_SIZE as usize {
            return Err(CastKmsError::InvalidCecMetadata("receive message size"));
        }
        let binding = self.cec.binding()?;
        if !binding.online {
            return Err(CastKmsError::InvalidCecState(
                "cannot inject CEC message while transport is offline",
            ));
        }
        let mut receive = DrmCastkmsCecReceive {
            connector_id: self.connector_id(),
            transport_id: binding.transport_id.get(),
            transport_generation: binding.transport_generation.get(),
            length: message.len() as u8,
            ..DrmCastkmsCecReceive::default()
        };
        receive.msg[..message.len()].copy_from_slice(message);
        // SAFETY: `receive` is a checked-in fixed-width input structure that
        // remains valid for the synchronous ioctl.
        unsafe { drm_ioctl_castkms_cec_receive(self.as_raw_fd(), &receive) }
            .map_err(CastKmsError::ReceiveCecMessage)?;
        Ok(())
    }

    pub fn unbind_cec_transport(&mut self) -> Result<(), CastKmsError> {
        let binding = self.cec.binding()?;
        if binding.online {
            return Err(CastKmsError::InvalidCecState(
                "CEC transport must be offline before unbind",
            ));
        }
        if self.cec.pending.is_some() {
            return Err(CastKmsError::InvalidCecState(
                "cannot unbind CEC transport with a pending transmit",
            ));
        }
        let unbind = DrmCastkmsCecUnbindTransport {
            connector_id: self.connector_id(),
            transport_id: binding.transport_id.get(),
            ..DrmCastkmsCecUnbindTransport::default()
        };
        // SAFETY: `unbind` is a checked-in fixed-width input structure that
        // remains valid for the synchronous ioctl.
        unsafe { drm_ioctl_castkms_cec_unbind_transport(self.as_raw_fd(), &unbind) }
            .map_err(CastKmsError::UnbindCecTransport)?;
        let removed = self.cec.remove()?;
        debug_assert_eq!(removed, binding);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(has_adapter: u32) -> DrmCastkmsCecQueryCaps {
        DrmCastkmsCecQueryCaps {
            connector_id: 13,
            uapi_major: CEC_UAPI_MAJOR,
            uapi_minor: CEC_UAPI_MINOR,
            capabilities: CEC_REQUIRED_CAPABILITIES,
            max_msg_size: CEC_MAX_MESSAGE_SIZE,
            output_index: 2,
            has_adapter,
            ..DrmCastkmsCecQueryCaps::default()
        }
    }

    fn binding() -> CecTransportBinding {
        CecTransportBinding {
            transport_id: NonZeroU32::new(3).unwrap(),
            transport_generation: NonZeroU64::new(5).unwrap(),
            state_generation: NonZeroU64::new(7).unwrap(),
            output_index: 2,
            physical_address: 0x1000,
            logical_address_mask: 1,
            online: false,
        }
    }

    fn event() -> CecTransmitEvent {
        CecTransmitEvent {
            transport_id: 3,
            transport_generation: 5,
            state_generation: 8,
            cookie: 11,
            connector_id: 13,
            output_index: 2,
            attempts: 2,
            signal_free_time: 1,
            message_length: 2,
            message: [0; 16],
        }
    }

    #[test]
    fn tracker_requires_online_single_transaction_lifecycle() {
        let mut tracker = CecTracker::default();
        tracker.install(binding()).unwrap();
        assert_eq!(
            tracker.record_transmit(&event()).unwrap(),
            CecTransmitAdmission::Stale
        );
        tracker.set_online(true).unwrap();
        assert_eq!(
            tracker.record_transmit(&event()).unwrap(),
            CecTransmitAdmission::Accepted
        );
        assert!(matches!(
            tracker.record_transmit(&event()),
            Err(CastKmsError::InvalidCecState(_))
        ));
        tracker.validate_completion(&event()).unwrap();
        assert!(matches!(
            tracker.set_online(false),
            Err(CastKmsError::InvalidCecState(_))
        ));
        tracker.complete();
        tracker.set_online(false).unwrap();
        let mut expected = binding();
        expected.state_generation = NonZeroU64::new(8).unwrap();
        assert_eq!(tracker.remove().unwrap(), expected);
        assert!(!tracker.is_bound());
    }

    #[test]
    fn tracker_rejects_stale_binding_and_completion_identity() {
        let mut tracker = CecTracker::default();
        let mut online = binding();
        online.online = true;
        tracker.install(online).unwrap();

        let mut stale = event();
        stale.transport_generation += 1;
        assert!(matches!(
            tracker.record_transmit(&stale),
            Err(CastKmsError::InvalidCecMetadata(_))
        ));

        assert_eq!(
            tracker.record_transmit(&event()).unwrap(),
            CecTransmitAdmission::Accepted
        );
        let mut wrong_cookie = event();
        wrong_cookie.cookie += 1;
        assert!(matches!(
            tracker.validate_completion(&wrong_cookie),
            Err(CastKmsError::InvalidCecMetadata(_))
        ));
    }

    #[test]
    fn authority_suspension_aborts_pending_and_rejects_the_old_state_interval() {
        let mut tracker = CecTracker::default();
        tracker.install(binding()).unwrap();
        tracker.set_online(true).unwrap();
        assert_eq!(
            tracker.record_transmit(&event()).unwrap(),
            CecTransmitAdmission::Accepted
        );

        tracker.record_authority_suspended().unwrap();
        assert!(!tracker.binding().unwrap().online);
        assert!(tracker.pending.is_none());
        tracker.set_online(true).unwrap();

        assert_eq!(
            tracker.record_transmit(&event()).unwrap(),
            CecTransmitAdmission::Stale
        );
        let mut resumed = event();
        resumed.state_generation += 2;
        assert_eq!(
            tracker.record_transmit(&resumed).unwrap(),
            CecTransmitAdmission::Accepted
        );
    }

    #[test]
    fn completion_constructors_report_consistent_kernel_status() {
        assert_eq!(CecCompletion::succeeded().status, CEC_TX_STATUS_OK);
        assert_eq!(CecCompletion::not_acknowledged(3).nack_count, 3);
        assert_eq!(CecCompletion::failed().error_count, 1);
    }

    #[test]
    fn capability_query_accepts_an_unavailable_adapter() {
        assert_eq!(decode_cec_capabilities(13, query(0)).unwrap(), None);
    }

    #[test]
    fn capability_query_reports_an_available_adapter() {
        let capabilities = decode_cec_capabilities(13, query(1)).unwrap().unwrap();
        assert_eq!(capabilities.connector_id().get(), 13);
        assert_eq!(capabilities.output_index(), 2);
    }

    #[test]
    fn capability_query_rejects_an_invalid_adapter_state() {
        assert!(matches!(
            decode_cec_capabilities(13, query(2)),
            Err(CastKmsError::InvalidCecMetadata("adapter availability"))
        ));
    }
}
