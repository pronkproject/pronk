#include "device.h"

#include "castkms-uapi.h"

#include <assert.h>
#include <errno.h>
#include <fcntl.h>
#include <stddef.h>
#include <stdbool.h>
#include <stdint.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <unistd.h>

#include <systemd/sd-device.h>
#include <xf86drm.h>
#include <xf86drmMode.h>

static_assert(sizeof(struct drm_castkms_capture_query_caps) == 40,
	      "CastKMS capability-query ABI size changed");
static_assert(sizeof(struct drm_castkms_create_grant) == 32,
	      "CastKMS create-grant ABI size changed");
static_assert(offsetof(struct drm_castkms_create_grant, fd) == 12,
	      "CastKMS grant-fd ABI offset changed");
static_assert(offsetof(struct drm_castkms_create_grant, grant_id) == 16,
	      "CastKMS grant-id ABI offset changed");
static_assert(offsetof(struct drm_castkms_create_grant, fd_flags) == 20,
	      "CastKMS grant-fd-flags ABI offset changed");
static_assert(offsetof(struct drm_castkms_create_grant, control_fd) == 24,
	      "CastKMS grant-control-fd ABI offset changed");
static_assert(offsetof(struct drm_castkms_create_grant, reserved) == 28,
	      "CastKMS grant-reserved ABI offset changed");
static_assert(sizeof(struct drm_castkms_get_grant) == 32,
	      "CastKMS get-grant ABI size changed");

static bool is_primary_node_name(const char *sysname, const char *devname)
{
	const char *suffix;

	if (!sysname || !devname || strncmp(sysname, "card", 4))
		return false;
	suffix = sysname + 4;
	if (!*suffix)
		return false;
	while (*suffix) {
		if (*suffix < '0' || *suffix > '9')
			return false;
		suffix++;
	}
	if (strncmp(devname, "/dev/dri/", strlen("/dev/dri/")))
		return false;
	return !strcmp(devname + strlen("/dev/dri/"), sysname);
}

static int validate_open_file(int fd, dev_t requested_devnum)
{
	drmVersionPtr version;
	struct stat status;
	int descriptor_flags;
	int result = 0;

	if (fstat(fd, &status) < 0)
		return -errno;
	if (!S_ISCHR(status.st_mode) || status.st_rdev != requested_devnum)
		return -ENODEV;

	descriptor_flags = fcntl(fd, F_GETFD);
	if (descriptor_flags < 0)
		return -errno;
	if (!(descriptor_flags & FD_CLOEXEC))
		return -EPROTO;
	if (drmGetNodeTypeFromFd(fd) != DRM_NODE_PRIMARY)
		return -ENODEV;

	version = drmGetVersion(fd);
	if (!version)
		return errno ? -errno : -ENODEV;
	if (!version->name || version->name_len != strlen("castkms") ||
	    memcmp(version->name, "castkms", strlen("castkms")))
		result = -ENODEV;
	drmFreeVersion(version);
	return result;
}

static int query_capture_uapi(int fd, uint16_t *uapi_major,
			      uint16_t *uapi_minor)
{
	struct drm_castkms_capture_query_caps query = { 0 };
	drmModeResPtr resources;
	int result = 0;

	resources = drmModeGetResources(fd);
	if (!resources)
		return errno ? -errno : -ENODEV;
	if (resources->count_crtcs <= 0 || !resources->crtcs) {
		result = -ENODEV;
		goto out;
	}

	query.crtc_id = resources->crtcs[0];
	if (ioctl(fd, DRM_IOCTL_CASTKMS_CAPTURE_QUERY_CAPS, &query) < 0) {
		result = -errno;
		goto out;
	}
	if (query.uapi_major <= UINT16_MAX)
		*uapi_major = query.uapi_major;
	if (query.uapi_minor <= UINT16_MAX)
		*uapi_minor = query.uapi_minor;
	if (query.uapi_major != DRM_CASTKMS_CAPTURE_UAPI_MAJOR ||
	    query.uapi_minor < DRM_CASTKMS_CAPTURE_UAPI_MINOR ||
	    query.uapi_major > UINT16_MAX || query.uapi_minor > UINT16_MAX ||
	    !(query.flags & DRM_CASTKMS_CAPTURE_CAP_GRANT_FD) ||
	    !(query.flags & DRM_CASTKMS_CAPTURE_CAP_GRANT_CONTROL_FD) ||
	    query.reserved) {
		result = -EPROTONOSUPPORT;
		goto out;
	}

out:
	drmModeFreeResources(resources);
	return result;
}

int pronk_device_open(const struct pronk_create_request *request,
		      struct pronk_device *device)
{
	const char *devname;
	const char *subsystem;
	const char *sysname;
	sd_device *system_device = NULL;
	dev_t requested_devnum;
	dev_t resolved_devnum;
	int fd = -1;
	int result;

	if (!request || !device)
		return -EINVAL;
	requested_devnum = makedev(request->device_major, request->device_minor);
	if (major(requested_devnum) != request->device_major ||
	    minor(requested_devnum) != request->device_minor)
		return -ERANGE;

	result = sd_device_new_from_devnum(&system_device, 'c',
					   requested_devnum);
	if (result < 0)
		return result;
	result = sd_device_get_subsystem(system_device, &subsystem);
	if (result < 0 || strcmp(subsystem, "drm")) {
		result = result < 0 ? result : -ENODEV;
		goto out;
	}
	result = sd_device_get_sysname(system_device, &sysname);
	if (result < 0)
		goto out;
	result = sd_device_get_devname(system_device, &devname);
	if (result < 0)
		goto out;
	result = sd_device_get_devnum(system_device, &resolved_devnum);
	if (result < 0)
		goto out;
	if (resolved_devnum != requested_devnum ||
	    !is_primary_node_name(sysname, devname)) {
		result = -ENODEV;
		goto out;
	}

	fd = open(devname,
		  O_RDWR | O_CLOEXEC | O_NONBLOCK | O_NOFOLLOW | O_NOCTTY);
	if (fd < 0) {
		result = -errno;
		goto out;
	}
	result = validate_open_file(fd, requested_devnum);
	if (result < 0)
		goto out;
	result = query_capture_uapi(fd, &device->capture_uapi_major,
				    &device->capture_uapi_minor);
	if (result < 0)
		goto out;

	device->fd = fd;
	fd = -1;

out:
	if (fd >= 0)
		close(fd);
	sd_device_unref(system_device);
	return result;
}

int pronk_device_validate_connector(const struct pronk_create_request *request,
				    const struct pronk_device *device)
{
	drmModeConnectorPtr connector;
	int result = 0;

	if (!request || !device || device->fd < 0)
		return -EINVAL;
	connector = drmModeGetConnector(device->fd, request->connector_id);
	if (!connector)
		return errno ? -errno : -ENOENT;
	if (connector->connector_id != request->connector_id ||
	    connector->connector_type != DRM_MODE_CONNECTOR_VIRTUAL)
		result = -EINVAL;
	drmModeFreeConnector(connector);
	return result;
}

void pronk_device_clear(struct pronk_device *device)
{
	if (!device)
		return;
	if (device->fd >= 0)
		close(device->fd);
	device->fd = -1;
}
