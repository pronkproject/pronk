#define _GNU_SOURCE

#include "transport.h"

#include <errno.h>
#include <poll.h>
#include <stdbool.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/un.h>
#include <unistd.h>

#define PRONK_TRANSPORT_TIMEOUT_MSEC 15000
#define PRONK_TRANSPORT_REJECTED_FD_CAPACITY 8U

static int wait_for_socket(int fd, short events)
{
	struct pollfd poll_fd = {
		.fd = fd,
		.events = events,
	};
	int result;

	do {
		result = poll(&poll_fd, 1, PRONK_TRANSPORT_TIMEOUT_MSEC);
	} while (result < 0 && errno == EINTR);
	if (result < 0)
		return -errno;
	if (!result)
		return -ETIMEDOUT;
	if (poll_fd.revents & POLLNVAL)
		return -EBADF;
	if (poll_fd.revents & POLLERR)
		return -EIO;
	if (!(poll_fd.revents & events))
		return -EPIPE;

	return 0;
}

int pronk_transport_validate_socket(int fd,
				    struct pronk_peer_credentials *peer)
{
	struct sockaddr_un address = { 0 };
	struct stat status;
	struct ucred credentials;
	socklen_t address_length = sizeof(address);
	socklen_t value_length;
	int accepting;
	int domain;
	int type;

	if (fd < 0 || !peer)
		return -EINVAL;
	if (fstat(fd, &status) < 0)
		return -errno;
	if (!S_ISSOCK(status.st_mode))
		return -ENOTSOCK;

	value_length = sizeof(domain);
	if (getsockopt(fd, SOL_SOCKET, SO_DOMAIN, &domain, &value_length) < 0)
		return -errno;
	if (value_length != sizeof(domain) || domain != AF_UNIX)
		return -EPROTOTYPE;

	value_length = sizeof(type);
	if (getsockopt(fd, SOL_SOCKET, SO_TYPE, &type, &value_length) < 0)
		return -errno;
	if (value_length != sizeof(type) || type != SOCK_SEQPACKET)
		return -EPROTOTYPE;

	value_length = sizeof(accepting);
	if (getsockopt(fd, SOL_SOCKET, SO_ACCEPTCONN, &accepting,
		       &value_length) < 0)
		return -errno;
	if (value_length != sizeof(accepting) || accepting)
		return -EPROTOTYPE;

	if (getpeername(fd, (struct sockaddr *)&address, &address_length) < 0)
		return -errno;
	if (address_length < sizeof(address.sun_family) ||
	    address.sun_family != AF_UNIX)
		return -ENOTCONN;

	value_length = sizeof(credentials);
	if (getsockopt(fd, SOL_SOCKET, SO_PEERCRED, &credentials,
		       &value_length) < 0)
		return -errno;
	if (value_length != sizeof(credentials) || credentials.pid <= 0)
		return -EPROTO;

	peer->pid = credentials.pid;
	peer->uid = credentials.uid;
	peer->gid = credentials.gid;
	return 0;
}

static bool close_received_fds(struct msghdr *message)
{
	struct cmsghdr *control_message;
	bool received_control = false;

	for (control_message = CMSG_FIRSTHDR(message); control_message;
	     control_message = CMSG_NXTHDR(message, control_message)) {
		size_t payload_length;
		size_t fd_count;
		int *fds;
		size_t index;

		received_control = true;
		if (control_message->cmsg_level != SOL_SOCKET ||
		    control_message->cmsg_type != SCM_RIGHTS ||
		    control_message->cmsg_len < CMSG_LEN(0))
			continue;

		payload_length = control_message->cmsg_len - CMSG_LEN(0);
		fd_count = payload_length / sizeof(*fds);
		fds = (int *)CMSG_DATA(control_message);
		for (index = 0; index < fd_count; index++)
			close(fds[index]);
	}

	return received_control;
}

int pronk_transport_receive(int fd, uint8_t *payload, size_t capacity,
			    size_t *length)
{
	uint8_t control[CMSG_SPACE(sizeof(int) *
				   PRONK_TRANSPORT_REJECTED_FD_CAPACITY)] = { 0 };
	struct iovec iov = {
		.iov_base = payload,
		.iov_len = capacity,
	};
	struct msghdr message = {
		.msg_iov = &iov,
		.msg_iovlen = 1,
		.msg_control = control,
		.msg_controllen = sizeof(control),
	};
	bool received_control;
	ssize_t received;
	int result;

	if (fd < 0 || !payload || !capacity || !length)
		return -EINVAL;

	result = wait_for_socket(fd, POLLIN);
	if (result < 0)
		return result;
	received = recvmsg(fd, &message,
			   MSG_DONTWAIT | MSG_CMSG_CLOEXEC);
	if (received < 0)
		return -errno;

	received_control = close_received_fds(&message);
	if (message.msg_flags & (MSG_TRUNC | MSG_CTRUNC))
		return -EMSGSIZE;
	if (received_control)
		return -EPROTO;
	if (!received)
		return -ECONNRESET;
	if ((size_t)received > capacity)
		return -EMSGSIZE;

	*length = received;
	return 0;
}

int pronk_transport_send(int fd, const uint8_t *payload, size_t length,
			 const int *transferred_fds,
			 size_t transferred_fd_count)
{
	uint8_t control[CMSG_SPACE(2U * sizeof(*transferred_fds))] = { 0 };
	struct iovec iov = {
		.iov_base = (void *)payload,
		.iov_len = length,
	};
	struct msghdr message = {
		.msg_iov = &iov,
		.msg_iovlen = 1,
	};
	struct cmsghdr *control_message;
	ssize_t sent;
	int result;

	if (fd < 0 || !payload || !length || transferred_fd_count > 2U ||
	    (transferred_fd_count && !transferred_fds))
		return -EINVAL;
	for (size_t index = 0; index < transferred_fd_count; index++) {
		if (transferred_fds[index] < 0)
			return -EINVAL;
	}
	if (transferred_fd_count) {
		size_t transferred_fds_size =
			transferred_fd_count * sizeof(*transferred_fds);

		message.msg_control = control;
		message.msg_controllen = CMSG_SPACE(transferred_fds_size);
		control_message = CMSG_FIRSTHDR(&message);
		control_message->cmsg_level = SOL_SOCKET;
		control_message->cmsg_type = SCM_RIGHTS;
		control_message->cmsg_len = CMSG_LEN(transferred_fds_size);
		memcpy(CMSG_DATA(control_message), transferred_fds,
		       transferred_fds_size);
	}

	result = wait_for_socket(fd, POLLOUT);
	if (result < 0)
		return result;
	sent = sendmsg(fd, &message, MSG_DONTWAIT | MSG_NOSIGNAL);
	if (sent < 0)
		return -errno;
	if ((size_t)sent != length)
		return -EIO;

	return 0;
}
