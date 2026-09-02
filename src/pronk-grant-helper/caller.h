#pragma once

#include "protocol.h"
#include "transport.h"

#include <sys/types.h>

struct pronk_caller {
	pid_t pid;
	uid_t uid;
	int pidfd;
};

int pronk_caller_parse_uid(const char *value, uid_t *uid);
int pronk_caller_begin(const struct pronk_peer_credentials *peer,
		       uid_t pkexec_uid, struct pronk_caller *caller);
int pronk_caller_validate_request(const struct pronk_caller *caller,
			  const struct pronk_create_request *request);
int pronk_caller_require_trusted(const struct pronk_caller *caller);
void pronk_caller_clear(struct pronk_caller *caller);
