use std::ffi::CStr;
use std::fs::File;
use std::io;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::Path;
use std::sync::{Arc, Mutex};

use ash::vk;

/// A Vulkan device selected by an opened DRM render node, never enumeration order.
///
/// The loader opens its own native descriptors. The selected node is an identity
/// check, not a way to make a Vulkan driver adopt a brokered descriptor.
#[derive(Clone)]
pub struct Device {
    pub(super) inner: Arc<DeviceInner>,
}

/// Vulkan identities for physical-device and driver compatibility checks.
///
/// Equality does not qualify a particular external format, modifier or handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceIdentity {
    pub device: [u8; vk::UUID_SIZE],
    pub driver: [u8; vk::UUID_SIZE],
}

struct Instance {
    raw: ash::Instance,
    // Keep the dynamically loaded function pointers valid until instance teardown.
    _entry: ash::Entry,
}

pub(super) struct DeviceInner {
    pub(super) raw: ash::Device,
    instance: Instance,
    pub(super) physical: vk::PhysicalDevice,
    pub(super) queue_family: u32,
    pub(super) shader_int64: bool,
    pub(super) submission: Mutex<()>,
    _render_node: File,
}

impl Device {
    /// Whether this logical device enabled exact 64-bit shader arithmetic.
    pub fn supports_shader_int64(&self) -> bool {
        self.inner.shader_int64
    }

    /// Whether both owners refer to the same logical Vulkan device instance.
    ///
    /// Clones share an instance. Separate opens of one render node do not,
    /// even when their physical-device and driver UUIDs are equal.
    pub fn is_same_instance(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    /// Physical-device and driver UUIDs for external-image compatibility checks.
    pub fn identity(&self) -> DeviceIdentity {
        let mut identity = vk::PhysicalDeviceIDProperties::default();
        let mut properties = vk::PhysicalDeviceProperties2::default().push_next(&mut identity);
        // SAFETY: This retained physical device supports the Vulkan 1.1 query;
        // the initialized output chain lives until the call returns.
        unsafe {
            self.inner
                .instance()
                .get_physical_device_properties2(self.inner.physical, &mut properties)
        };
        DeviceIdentity {
            device: identity.device_uuid,
            driver: identity.driver_uuid,
        }
    }

    /// Diagnostic name of the device selected by render-node identity.
    pub fn name(&self) -> String {
        // SAFETY: The physical device belongs to the retained live instance.
        let properties = unsafe {
            self.inner
                .instance()
                .get_physical_device_properties(self.inner.physical)
        };
        // SAFETY: Vulkan returns a NUL-terminated name in the fixed-size array.
        unsafe { CStr::from_ptr(properties.device_name.as_ptr()) }
            .to_string_lossy()
            .into_owned()
    }

    /// Open the exact render node and require the initial shared-image capabilities.
    pub fn open(render_node: impl AsRef<Path>) -> io::Result<Self> {
        let node = File::options().read(true).write(true).open(render_node)?;
        let metadata = node.metadata()?;
        if !metadata.file_type().is_char_device() {
            return Err(unsupported("selected path is not a DRM render node"));
        }
        let major = nix::sys::stat::major(metadata.rdev());
        let minor = nix::sys::stat::minor(metadata.rdev());
        // SAFETY: The system Vulkan loader is trusted native code. Entry remains
        // alive through every instance and device call via the owning hierarchy.
        let entry = unsafe { ash::Entry::load() }.map_err(io::Error::other)?;
        let app = vk::ApplicationInfo::default()
            .application_name(c"pronk-gpu")
            .api_version(vk::API_VERSION_1_1);
        let create = vk::InstanceCreateInfo::default().application_info(&app);
        // SAFETY: All create-info pointers reference live local values.
        let raw = unsafe { entry.create_instance(&create, None) }.map_err(native)?;
        let instance = Instance { raw, _entry: entry };
        // SAFETY: The instance is live and enumeration retains no borrowed pointers.
        let physicals = unsafe { instance.raw.enumerate_physical_devices() }.map_err(native)?;
        for physical in physicals {
            // SAFETY: The physical device was enumerated from the live instance.
            let extensions =
                unsafe { instance.raw.enumerate_device_extension_properties(physical) }
                    .map_err(native)?;
            if !has_extension(&extensions, ash::ext::physical_device_drm::NAME) {
                continue;
            }
            let mut drm = vk::PhysicalDeviceDrmPropertiesEXT::default();
            let mut properties = vk::PhysicalDeviceProperties2::default().push_next(&mut drm);
            // SAFETY: The supported query uses a valid output chain.
            unsafe {
                instance
                    .raw
                    .get_physical_device_properties2(physical, &mut properties)
            };
            let api_version = properties.properties.api_version;
            if drm.has_render != vk::TRUE
                || drm.render_major != major as i64
                || drm.render_minor != minor as i64
            {
                continue;
            }
            if api_version < vk::API_VERSION_1_1 {
                return Err(unsupported("selected GPU requires Vulkan 1.1"));
            }
            let required = [
                ash::ext::physical_device_drm::NAME,
                ash::khr::external_memory_fd::NAME,
                ash::ext::external_memory_dma_buf::NAME,
                ash::ext::image_drm_format_modifier::NAME,
                ash::khr::image_format_list::NAME,
                ash::khr::external_semaphore_fd::NAME,
                ash::ext::queue_family_foreign::NAME,
            ];
            if required
                .iter()
                .any(|name| !has_extension(&extensions, name))
            {
                return Err(unsupported("selected GPU lacks shared-image extensions"));
            }
            let query = vk::PhysicalDeviceExternalSemaphoreInfo::default()
                .handle_type(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD);
            let mut semaphore = vk::ExternalSemaphoreProperties::default();
            // SAFETY: Live physical device, valid input and output structures.
            unsafe {
                instance
                    .raw
                    .get_physical_device_external_semaphore_properties(
                        physical,
                        &query,
                        &mut semaphore,
                    )
            };
            if !semaphore.external_semaphore_features.contains(
                vk::ExternalSemaphoreFeatureFlags::IMPORTABLE
                    | vk::ExternalSemaphoreFeatureFlags::EXPORTABLE,
            ) {
                return Err(unsupported(
                    "selected GPU lacks importable/exportable sync files",
                ));
            }
            // SAFETY: The physical device belongs to the live instance.
            let queues = unsafe {
                instance
                    .raw
                    .get_physical_device_queue_family_properties(physical)
            };
            let queue = queues
                .iter()
                .position(|queue| {
                    queue.queue_count > 0 && queue.queue_flags.contains(vk::QueueFlags::GRAPHICS)
                })
                .ok_or_else(|| unsupported("selected GPU lacks a graphics queue"))?;
            let priorities = [1.0];
            let queues = [vk::DeviceQueueCreateInfo::default()
                .queue_family_index(queue as u32)
                .queue_priorities(&priorities)];
            let names: Vec<_> = required.iter().map(|name| name.as_ptr()).collect();
            // SAFETY: The physical device belongs to the live instance.
            let available_features = unsafe { instance.raw.get_physical_device_features(physical) };
            let enabled_features = vk::PhysicalDeviceFeatures::default()
                .shader_int64(available_features.shader_int64 == vk::TRUE);
            let create = vk::DeviceCreateInfo::default()
                .queue_create_infos(&queues)
                .enabled_extension_names(&names)
                .enabled_features(&enabled_features);
            // SAFETY: Queried extensions and queue family belong to this physical
            // device; all create-info pointers live through the synchronous call.
            let raw =
                unsafe { instance.raw.create_device(physical, &create, None) }.map_err(native)?;
            return Ok(Self {
                inner: Arc::new(DeviceInner {
                    raw,
                    instance,
                    physical,
                    queue_family: queue as u32,
                    shader_int64: available_features.shader_int64 == vk::TRUE,
                    submission: Mutex::new(()),
                    _render_node: node,
                }),
            });
        }
        Err(unsupported(
            "no Vulkan device matches the selected DRM render node",
        ))
    }
}

impl DeviceInner {
    pub(super) fn instance(&self) -> &ash::Instance {
        &self.instance.raw
    }
}

impl Drop for DeviceInner {
    fn drop(&mut self) {
        // SAFETY: Child resources retain an Arc to this device. Submitted jobs
        // retain their resources until completion or device loss permits teardown.
        unsafe { self.raw.destroy_device(None) };
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        // SAFETY: Device teardown precedes instance teardown, with Entry still live.
        unsafe { self.raw.destroy_instance(None) };
    }
}

fn has_extension(extensions: &[vk::ExtensionProperties], name: &CStr) -> bool {
    extensions.iter().any(|extension| {
        // SAFETY: Vulkan returns a NUL-terminated extension name in this array.
        unsafe { CStr::from_ptr(extension.extension_name.as_ptr()) == name }
    })
}

pub(super) fn native(error: vk::Result) -> io::Error {
    io::Error::other(error)
}
pub(super) fn unsupported(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, message)
}
