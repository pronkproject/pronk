//! Native image requirements follow the operations available on their owner.

use ash::vk;

#[derive(Clone, Copy)]
pub(in crate::vulkan) enum ImageUse {
    ImportedSource,
    ImportedDestination,
    OwnedStorage,
}

impl ImageUse {
    pub(in crate::vulkan) fn flags(self) -> vk::ImageUsageFlags {
        match self {
            Self::ImportedSource => vk::ImageUsageFlags::TRANSFER_SRC,
            Self::ImportedDestination => vk::ImageUsageFlags::TRANSFER_DST,
            Self::OwnedStorage => {
                vk::ImageUsageFlags::TRANSFER_SRC | vk::ImageUsageFlags::TRANSFER_DST
            }
        }
    }

    pub(in crate::vulkan) fn sharing(self) -> vk::ExternalMemoryFeatureFlags {
        match self {
            Self::ImportedSource | Self::ImportedDestination => {
                vk::ExternalMemoryFeatureFlags::IMPORTABLE
            }
            Self::OwnedStorage => vk::ExternalMemoryFeatureFlags::EXPORTABLE,
        }
    }

    pub(in crate::vulkan) fn supports_modifier(
        self,
        properties: &vk::DrmFormatModifierPropertiesEXT,
        modifier: u64,
    ) -> bool {
        let needed = match self {
            Self::ImportedSource => vk::FormatFeatureFlags::BLIT_SRC,
            Self::ImportedDestination => vk::FormatFeatureFlags::TRANSFER_DST,
            Self::OwnedStorage => {
                vk::FormatFeatureFlags::BLIT_SRC | vk::FormatFeatureFlags::BLIT_DST
            }
        };
        properties.drm_format_modifier == modifier
            && properties.drm_format_modifier_plane_count == 1
            && properties
                .drm_format_modifier_tiling_features
                .contains(needed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_imports_require_reading_but_not_destination_writes() {
        assert_eq!(
            ImageUse::ImportedSource.flags(),
            vk::ImageUsageFlags::TRANSFER_SRC
        );
        assert_eq!(
            ImageUse::ImportedSource.sharing(),
            vk::ExternalMemoryFeatureFlags::IMPORTABLE
        );
        let properties = vk::DrmFormatModifierPropertiesEXT {
            drm_format_modifier: 7,
            drm_format_modifier_plane_count: 1,
            drm_format_modifier_tiling_features: vk::FormatFeatureFlags::BLIT_SRC,
        };
        assert!(ImageUse::ImportedSource.supports_modifier(&properties, 7));
        assert!(!ImageUse::OwnedStorage.supports_modifier(&properties, 7));
    }

    #[test]
    fn destination_imports_require_writing_but_not_source_reads() {
        assert_eq!(
            ImageUse::ImportedDestination.flags(),
            vk::ImageUsageFlags::TRANSFER_DST
        );
        assert_eq!(
            ImageUse::ImportedDestination.sharing(),
            vk::ExternalMemoryFeatureFlags::IMPORTABLE
        );
        let properties = vk::DrmFormatModifierPropertiesEXT {
            drm_format_modifier: 7,
            drm_format_modifier_plane_count: 1,
            drm_format_modifier_tiling_features: vk::FormatFeatureFlags::TRANSFER_DST,
        };
        assert!(ImageUse::ImportedDestination.supports_modifier(&properties, 7));
        assert!(!ImageUse::ImportedSource.supports_modifier(&properties, 7));
    }

    #[test]
    fn owned_storage_retains_read_write_and_export_requirements() {
        assert_eq!(
            ImageUse::OwnedStorage.flags(),
            vk::ImageUsageFlags::TRANSFER_SRC | vk::ImageUsageFlags::TRANSFER_DST
        );
        assert_eq!(
            ImageUse::OwnedStorage.sharing(),
            vk::ExternalMemoryFeatureFlags::EXPORTABLE
        );
        let properties = vk::DrmFormatModifierPropertiesEXT {
            drm_format_modifier: 7,
            drm_format_modifier_plane_count: 1,
            drm_format_modifier_tiling_features: vk::FormatFeatureFlags::BLIT_SRC
                | vk::FormatFeatureFlags::BLIT_DST,
        };
        assert!(ImageUse::OwnedStorage.supports_modifier(&properties, 7));
    }

    #[test]
    fn image_uses_require_exact_modifier_plane_and_operation_support() {
        for usage in [
            ImageUse::ImportedSource,
            ImageUse::ImportedDestination,
            ImageUse::OwnedStorage,
        ] {
            for (modifier, planes, features) in [
                (
                    8,
                    1,
                    vk::FormatFeatureFlags::BLIT_SRC | vk::FormatFeatureFlags::BLIT_DST,
                ),
                (
                    7,
                    0,
                    vk::FormatFeatureFlags::BLIT_SRC | vk::FormatFeatureFlags::BLIT_DST,
                ),
                (
                    7,
                    2,
                    vk::FormatFeatureFlags::BLIT_SRC | vk::FormatFeatureFlags::BLIT_DST,
                ),
            ] {
                let properties = vk::DrmFormatModifierPropertiesEXT {
                    drm_format_modifier: modifier,
                    drm_format_modifier_plane_count: planes,
                    drm_format_modifier_tiling_features: features,
                };
                assert!(!usage.supports_modifier(&properties, 7));
            }
        }
        for (usage, features) in [
            (ImageUse::ImportedSource, vk::FormatFeatureFlags::BLIT_DST),
            (
                ImageUse::ImportedDestination,
                vk::FormatFeatureFlags::TRANSFER_SRC,
            ),
            (ImageUse::OwnedStorage, vk::FormatFeatureFlags::BLIT_DST),
        ] {
            let properties = vk::DrmFormatModifierPropertiesEXT {
                drm_format_modifier: 7,
                drm_format_modifier_plane_count: 1,
                drm_format_modifier_tiling_features: features,
            };
            assert!(!usage.supports_modifier(&properties, 7));
        }
    }
}
