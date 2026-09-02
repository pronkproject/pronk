#include "grant.h"

#include "castkms-uapi.h"

#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <stdint.h>
#include <sys/ioctl.h>
#include <unistd.h>

#include <xf86drm.h>

#define PRONK_DISPLAY_V1_RIGHTS \
	(DRM_CASTKMS_GRANT_CAPTURE_PIXELS | \
	 DRM_CASTKMS_GRANT_MANAGE_ATTACHMENT | \
	 DRM_CASTKMS_GRANT_UPDATE_EDID | DRM_CASTKMS_GRANT_READ_CURSOR)

#define PRONK_DISPLAY_CEC_V1_RIGHTS \
	(PRONK_DISPLAY_V1_RIGHTS | DRM_CASTKMS_GRANT_MANAGE_CEC)

#define PRONK_DISPLAY_CEC_AUDIO_V1_RIGHTS \
	(PRONK_DISPLAY_CEC_V1_RIGHTS | DRM_CASTKMS_GRANT_CAPTURE_AUDIO)

static int profile_rights(uint16_t profile, uint32_t *rights)
{
	if (!rights)
		return -EINVAL;
	switch (profile) {
	case PRONK_GRANT_PROFILE_DISPLAY_V1:
		*rights = PRONK_DISPLAY_V1_RIGHTS;
		return 0;
	case PRONK_GRANT_PROFILE_DISPLAY_CEC_V1:
		*rights = PRONK_DISPLAY_CEC_V1_RIGHTS;
		return 0;
	case PRONK_GRANT_PROFILE_DISPLAY_CEC_AUDIO_V1:
		*rights = PRONK_DISPLAY_CEC_AUDIO_V1_RIGHTS;
		return 0;
	default:
		return -EOPNOTSUPP;
	}
}

static int validate_holder_descriptor(int fd)
{
	int descriptor_flags;
	int status_flags;

	descriptor_flags = fcntl(fd, F_GETFD);
	if (descriptor_flags < 0)
		return -errno;
	if (!(descriptor_flags & FD_CLOEXEC))
		return -EPROTO;
	status_flags = fcntl(fd, F_GETFL);
	if (status_flags < 0)
		return -errno;
	if (!(status_flags & O_NONBLOCK))
		return -EPROTO;
	return 0;
}

static int validate_grant_query(const struct drm_castkms_get_grant *query,
				uint32_t grant_id, uint32_t connector_id,
				uint32_t rights)
{
	if (query->grant_id != grant_id ||
	    query->connector_id != connector_id || query->rights != rights)
		return -EPROTO;
	if ((query->flags & DRM_CASTKMS_GRANT_FLAGS_MASK) !=
		DRM_CASTKMS_GRANT_FLAG_ADMIN ||
	    query->flags & ~DRM_CASTKMS_GRANT_FLAGS_MASK)
		return -EPROTO;
	if (query->state >
		DRM_CASTKMS_GRANT_STATE_SUSPENDED_FOREIGN_CONTENT ||
	    query->state == DRM_CASTKMS_GRANT_STATE_REVOKED)
		return -EPROTO;
	if (query->reserved)
		return -EPROTO;
	return 0;
}

static int drop_device_master(int fd)
{
	if (!drmIsMaster(fd))
		return 0;
	if (ioctl(fd, DRM_IOCTL_DROP_MASTER, 0) < 0)
		return -errno;
	if (drmIsMaster(fd))
		return -EBUSY;
	return 0;
}

static int validate_control_descriptor(int fd)
{
	struct pollfd poll_fd = {
		.fd = fd,
	};
	int descriptor_flags;
	int result;

	descriptor_flags = fcntl(fd, F_GETFD);
	if (descriptor_flags < 0)
		return -errno;
	if (!(descriptor_flags & FD_CLOEXEC))
		return -EPROTO;
	do {
		poll_fd.revents = 0;
		result = poll(&poll_fd, 1, 0);
	} while (result < 0 && errno == EINTR);
	if (result < 0)
		return -errno;
	if (poll_fd.revents & (POLLHUP | POLLERR | POLLNVAL))
		return -EPROTO;
	return 0;
}

int pronk_grant_create(const struct pronk_create_request *request,
		       const struct pronk_device *device,
		       struct pronk_grant *grant,
		       enum pronk_diagnostic_stage *failure_stage)
{
	struct drm_castkms_create_grant create = {
		/* System mode explicitly requests the administrative grant class. */
		.flags = DRM_CASTKMS_GRANT_CREATE_ADMIN,
		.fd = -1,
		.fd_flags = O_NONBLOCK,
		.control_fd = -1,
	};
	struct drm_castkms_get_grant holder_query = { 0 };
	int result;

	if (!request || !device || device->fd < 0 || !grant || !failure_stage)
		return -EINVAL;
	*failure_stage = PRONK_DIAGNOSTIC_STAGE_CREATE_GRANT;
	result = profile_rights(request->profile, &create.rights);
	if (result < 0)
		return result;
	create.connector_id = request->connector_id;

	if (ioctl(device->fd, DRM_IOCTL_CASTKMS_CREATE_GRANT, &create) < 0)
		return -errno;
	*failure_stage = PRONK_DIAGNOSTIC_STAGE_VERIFY_GRANT;
	if (create.fd < 0 || create.control_fd < 0 ||
	    create.fd == create.control_fd || !create.grant_id ||
	    create.reserved)
		goto fail_protocol;
	result = validate_holder_descriptor(create.fd);
	if (result < 0)
		goto fail;
	result = validate_control_descriptor(create.control_fd);
	if (result < 0)
		goto fail;
	*failure_stage = PRONK_DIAGNOSTIC_STAGE_DROP_MASTER;
	result = drop_device_master(device->fd);
	if (result < 0)
		goto fail;

	*failure_stage = PRONK_DIAGNOSTIC_STAGE_VERIFY_GRANT;
	if (ioctl(create.fd, DRM_IOCTL_CASTKMS_GET_GRANT, &holder_query) < 0) {
		result = -errno;
		goto fail;
	}
	result = validate_grant_query(&holder_query, create.grant_id,
				      request->connector_id,
				      create.rights);
	if (result < 0)
		goto fail;
	grant->holder_fd = create.fd;
	grant->control_fd = create.control_fd;
	grant->grant_id = create.grant_id;
	grant->connector_id = holder_query.connector_id;
	grant->output_index = holder_query.output_index;
	grant->rights = holder_query.rights;
	grant->flags = holder_query.flags;
	grant->state = holder_query.state;
	return 0;

fail_protocol:
	result = -EPROTO;
fail:
	/* The control endpoint is the revocation authority, so close it first. */
	if (create.control_fd >= 0)
		close(create.control_fd);
	if (create.fd >= 0 && create.fd != create.control_fd)
		close(create.fd);
	return result;
}

void pronk_grant_clear(struct pronk_grant *grant)
{
	if (!grant)
		return;
	if (grant->control_fd >= 0)
		close(grant->control_fd);
	if (grant->holder_fd >= 0)
		close(grant->holder_fd);
	grant->control_fd = -1;
	grant->holder_fd = -1;
}
