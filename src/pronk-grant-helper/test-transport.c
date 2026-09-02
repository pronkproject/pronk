#define _GNU_SOURCE

#include "transport.h"

#include <assert.h>
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <unistd.h>

static int send_fd(int socket_fd, int fd)
{
	uint8_t payload = 1;
	uint8_t control[CMSG_SPACE(sizeof(fd))] = { 0 };
	struct iovec iov = {
		.iov_base = &payload,
		.iov_len = sizeof(payload),
	};
	struct msghdr message = {
		.msg_iov = &iov,
		.msg_iovlen = 1,
		.msg_control = control,
		.msg_controllen = sizeof(control),
	};
	struct cmsghdr *control_message = CMSG_FIRSTHDR(&message);

	control_message->cmsg_level = SOL_SOCKET;
	control_message->cmsg_type = SCM_RIGHTS;
	control_message->cmsg_len = CMSG_LEN(sizeof(fd));
	memcpy(CMSG_DATA(control_message), &fd, sizeof(fd));
	return sendmsg(socket_fd, &message, MSG_NOSIGNAL);
}

static void test_validation_and_payload(void)
{
	const uint8_t sent[] = "request";
	uint8_t received[32];
	struct pronk_peer_credentials peer;
	int sockets[2];
	size_t length;

	assert(socketpair(AF_UNIX, SOCK_SEQPACKET | SOCK_CLOEXEC, 0,
			  sockets) == 0);
	assert(pronk_transport_validate_socket(sockets[0], &peer) == 0);
	assert(peer.pid == getpid());
	assert(peer.uid == getuid());
	assert(peer.gid == getgid());
	assert(send(sockets[1], sent, sizeof(sent), MSG_NOSIGNAL) ==
	       (ssize_t)sizeof(sent));
	assert(pronk_transport_receive(sockets[0], received, sizeof(received),
				       &length) == 0);
	assert(length == sizeof(sent));
	assert(!memcmp(received, sent, sizeof(sent)));
	close(sockets[0]);
	close(sockets[1]);

	assert(socketpair(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0, sockets) ==
	       0);
	assert(pronk_transport_validate_socket(sockets[0], &peer) ==
	       -EPROTOTYPE);
	close(sockets[0]);
	close(sockets[1]);
}

static void test_rejected_descriptor_is_closed(void)
{
	uint8_t received[32];
	int pipe_fds[2];
	int sockets[2];
	size_t length;

	assert(signal(SIGPIPE, SIG_IGN) != SIG_ERR);
	assert(socketpair(AF_UNIX, SOCK_SEQPACKET | SOCK_CLOEXEC, 0,
			  sockets) == 0);
	assert(pipe2(pipe_fds, O_CLOEXEC) == 0);
	assert(send_fd(sockets[1], pipe_fds[0]) == 1);
	assert(close(pipe_fds[0]) == 0);
	assert(pronk_transport_receive(sockets[0], received, sizeof(received),
				       &length) == -EPROTO);
	assert(write(pipe_fds[1], "x", 1) == -1);
	assert(errno == EPIPE);
	close(pipe_fds[1]);
	close(sockets[0]);
	close(sockets[1]);
}

static void test_truncated_packet_is_rejected(void)
{
	uint8_t oversized[64] = { 0 };
	uint8_t received[32];
	int sockets[2];
	size_t length;

	assert(socketpair(AF_UNIX, SOCK_SEQPACKET | SOCK_CLOEXEC, 0,
			  sockets) == 0);
	assert(send(sockets[1], oversized, sizeof(oversized), MSG_NOSIGNAL) ==
	       (ssize_t)sizeof(oversized));
	assert(pronk_transport_receive(sockets[0], received, sizeof(received),
				       &length) == -EMSGSIZE);
	close(sockets[0]);
	close(sockets[1]);
}

static void test_two_descriptors_are_sent_in_order(void)
{
	const uint8_t sent[] = "result";
	uint8_t received[32];
	uint8_t control[CMSG_SPACE(2U * sizeof(int))] = { 0 };
	struct iovec iov = {
		.iov_base = received,
		.iov_len = sizeof(received),
	};
	struct msghdr message = {
		.msg_iov = &iov,
		.msg_iovlen = 1,
		.msg_control = control,
		.msg_controllen = sizeof(control),
	};
	struct cmsghdr *control_message;
	struct stat original_status;
	struct stat received_status;
	int transferred_fds[2];
	int received_fds[2];
	int sockets[2];
	ssize_t length;
	size_t index;

	assert(socketpair(AF_UNIX, SOCK_SEQPACKET | SOCK_CLOEXEC, 0,
			  sockets) == 0);
	transferred_fds[0] = open("/dev/null", O_RDONLY | O_CLOEXEC);
	transferred_fds[1] = open("/dev/zero", O_RDONLY | O_CLOEXEC);
	assert(transferred_fds[0] >= 0);
	assert(transferred_fds[1] >= 0);
	assert(pronk_transport_send(sockets[0], sent, sizeof(sent),
				    transferred_fds, 2) == 0);

	length = recvmsg(sockets[1], &message, MSG_CMSG_CLOEXEC);
	assert(length == (ssize_t)sizeof(sent));
	assert(!memcmp(received, sent, sizeof(sent)));
	assert(!(message.msg_flags & (MSG_TRUNC | MSG_CTRUNC)));
	control_message = CMSG_FIRSTHDR(&message);
	assert(control_message);
	assert(control_message->cmsg_level == SOL_SOCKET);
	assert(control_message->cmsg_type == SCM_RIGHTS);
	assert(control_message->cmsg_len == CMSG_LEN(sizeof(received_fds)));
	memcpy(received_fds, CMSG_DATA(control_message), sizeof(received_fds));
	assert(CMSG_NXTHDR(&message, control_message) == NULL);

	for (index = 0; index < 2; index++) {
		assert(fstat(transferred_fds[index], &original_status) == 0);
		assert(fstat(received_fds[index], &received_status) == 0);
		assert(original_status.st_rdev == received_status.st_rdev);
		assert(fcntl(received_fds[index], F_GETFD) & FD_CLOEXEC);
		close(received_fds[index]);
		close(transferred_fds[index]);
	}
	close(sockets[0]);
	close(sockets[1]);
}

static void test_send_rejects_invalid_descriptor_sets(void)
{
	const uint8_t payload[] = "result";
	const int too_many[3] = { STDIN_FILENO, STDOUT_FILENO,
				  STDERR_FILENO };
	const int invalid = -1;
	int sockets[2];

	assert(socketpair(AF_UNIX, SOCK_SEQPACKET | SOCK_CLOEXEC, 0,
			  sockets) == 0);
	assert(pronk_transport_send(sockets[0], payload, sizeof(payload),
				    NULL, 1) == -EINVAL);
	assert(pronk_transport_send(sockets[0], payload, sizeof(payload),
				    too_many, 3) == -EINVAL);
	assert(pronk_transport_send(sockets[0], payload, sizeof(payload),
				    &invalid, 1) == -EINVAL);
	close(sockets[0]);
	close(sockets[1]);
}

int main(void)
{
	test_validation_and_payload();
	test_rejected_descriptor_is_closed();
	test_truncated_packet_is_rejected();
	test_two_descriptors_are_sent_in_order();
	test_send_rejects_invalid_descriptor_sets();
	puts("C helper transport tests passed");
	return 0;
}
