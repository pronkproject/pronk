#pragma once

#include "device.h"
#include "protocol.h"

#include <stdint.h>

struct pronk_grant {
	int holder_fd;
	int control_fd;
	uint32_t grant_id;
	uint32_t connector_id;
	uint32_t output_index;
	uint32_t rights;
	uint32_t flags;
	uint32_t state;
};

int pronk_grant_create(const struct pronk_create_request *request,
		       const struct pronk_device *device,
		       struct pronk_grant *grant,
		       enum pronk_diagnostic_stage *failure_stage);
void pronk_grant_clear(struct pronk_grant *grant);
