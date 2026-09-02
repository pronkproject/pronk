#pragma once

#include "protocol.h"

#include <stdint.h>

struct pronk_device {
	int fd;
	uint16_t capture_uapi_major;
	uint16_t capture_uapi_minor;
};

int pronk_device_open(const struct pronk_create_request *request,
		      struct pronk_device *device);
int pronk_device_validate_connector(const struct pronk_create_request *request,
				    const struct pronk_device *device);
void pronk_device_clear(struct pronk_device *device);
