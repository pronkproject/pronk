/* SPDX-License-Identifier: GPL-2.0-only WITH Linux-syscall-note */

#pragma once

/*
 * Minimal userspace copy of the CastKMS 0.12 grant UAPI. Keep every copied
 * definition byte-for-byte compatible with include/uapi/drm/castkms_drm.h in
 * the CastKMS kernel source. The helper intentionally has no dependency on a
 * sibling source checkout or on private kernel headers.
 */

#include <drm/drm.h>

#define DRM_CASTKMS_CAPTURE_UAPI_MAJOR 0
#define DRM_CASTKMS_CAPTURE_UAPI_MINOR 12

#define DRM_CASTKMS_CAPTURE_CAP_GRANT_FD (1ULL << 3)
#define DRM_CASTKMS_CAPTURE_CAP_GRANT_CONTROL_FD (1ULL << 4)

#define DRM_CASTKMS_GRANT_CAPTURE_PIXELS (1U << 0)
#define DRM_CASTKMS_GRANT_MANAGE_ATTACHMENT (1U << 1)
#define DRM_CASTKMS_GRANT_UPDATE_EDID (1U << 2)
#define DRM_CASTKMS_GRANT_READ_CURSOR (1U << 3)
#define DRM_CASTKMS_GRANT_MANAGE_CEC (1U << 4)
#define DRM_CASTKMS_GRANT_CAPTURE_AUDIO (1U << 5)

#define DRM_CASTKMS_GRANT_CREATE_ADMIN (1U << 0)
#define DRM_CASTKMS_GRANT_CREATE_DELEGATED (1U << 1)

#define DRM_CASTKMS_GRANT_FLAG_ADMIN (1U << 0)
#define DRM_CASTKMS_GRANT_FLAG_DELEGATED (1U << 1)
#define DRM_CASTKMS_GRANT_FLAGS_MASK \
	(DRM_CASTKMS_GRANT_FLAG_ADMIN | DRM_CASTKMS_GRANT_FLAG_DELEGATED)

/*
 * fd is the restricted holder. control_fd is the grantor's close-to-revoke
 * endpoint and becomes permanently POLLHUP after the grant becomes terminal.
 * control_fd must be -1 on input; reserved must be zero.
 */
struct drm_castkms_create_grant {
	__u32 connector_id;
	__u32 rights;
	__u32 flags;
	__s32 fd;
	__u32 grant_id;
	__u32 fd_flags;
	__s32 control_fd;
	__u32 reserved;
};

#define DRM_CASTKMS_GRANT_STATE_PENDING 0
#define DRM_CASTKMS_GRANT_STATE_ACTIVE 1
#define DRM_CASTKMS_GRANT_STATE_SUSPENDED_NO_MASTER 2
#define DRM_CASTKMS_GRANT_STATE_SUSPENDED_OTHER_MASTER 3
#define DRM_CASTKMS_GRANT_STATE_SUSPENDED_FOREIGN_CONTENT 4
#define DRM_CASTKMS_GRANT_STATE_REVOKED 5

struct drm_castkms_get_grant {
	__u32 grant_id;
	__u32 connector_id;
	__u32 rights;
	__u32 state;
	__u32 flags;
	__u32 output_index;
	__u64 reserved;
};

struct drm_castkms_capture_query_caps {
	__u32 uapi_major;
	__u32 uapi_minor;
	__u32 crtc_id;
	__u32 format_count;
	__u64 flags;
	__u64 formats_ptr;
	__u32 max_registered_buffers;
	__u32 reserved;
};

#define DRM_CASTKMS_CAPTURE_QUERY_CAPS 0x00
#define DRM_CASTKMS_CREATE_GRANT 0x11
#define DRM_CASTKMS_GET_GRANT 0x13

#define DRM_IOCTL_CASTKMS_CAPTURE_QUERY_CAPS \
	DRM_IOWR(DRM_COMMAND_BASE + DRM_CASTKMS_CAPTURE_QUERY_CAPS, \
		 struct drm_castkms_capture_query_caps)
#define DRM_IOCTL_CASTKMS_CREATE_GRANT \
	DRM_IOWR(DRM_COMMAND_BASE + DRM_CASTKMS_CREATE_GRANT, \
		 struct drm_castkms_create_grant)
#define DRM_IOCTL_CASTKMS_GET_GRANT \
	DRM_IOWR(DRM_COMMAND_BASE + DRM_CASTKMS_GET_GRANT, \
		 struct drm_castkms_get_grant)
