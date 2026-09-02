#pragma once

#include <stddef.h>
#include <stdint.h>
#include <sys/types.h>

struct pronk_peer_credentials {
	pid_t pid;
	uid_t uid;
	gid_t gid;
};

int pronk_transport_validate_socket(int fd,
				    struct pronk_peer_credentials *peer);
int pronk_transport_receive(int fd, uint8_t *payload, size_t capacity,
			    size_t *length);
int pronk_transport_send(int fd, const uint8_t *payload, size_t length,
			 const int *transferred_fds,
			 size_t transferred_fd_count);
