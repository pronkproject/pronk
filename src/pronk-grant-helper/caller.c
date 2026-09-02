#define _GNU_SOURCE

#include "caller.h"
#include "config.h"

#include <ctype.h>
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <poll.h>
#include <pwd.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <systemd/sd-login.h>
#include <unistd.h>

#define PRONK_PROC_FILE_CAPACITY 8192U

static int parse_u64_token(const char *begin, const char *end, uint64_t *value)
{
	uint64_t parsed = 0;
	const char *cursor;

	if (begin == end)
		return -EINVAL;
	for (cursor = begin; cursor < end; cursor++) {
		unsigned int digit;

		if (!isdigit((unsigned char)*cursor))
			return -EINVAL;
		digit = *cursor - '0';
		if (parsed > (UINT64_MAX - digit) / 10)
			return -ERANGE;
		parsed = parsed * 10 + digit;
	}

	*value = parsed;
	return 0;
}

static int read_small_file_at(int directory_fd, const char *name,
			      char *buffer, size_t capacity)
{
	size_t used = 0;
	ssize_t count;
	int fd;

	fd = openat(directory_fd, name, O_RDONLY | O_CLOEXEC | O_NOFOLLOW);
	if (fd < 0)
		return -errno;
	while (used + 1 < capacity) {
		count = read(fd, buffer + used, capacity - used - 1);
		if (count < 0 && errno == EINTR)
			continue;
		if (count < 0) {
			int error = -errno;

			close(fd);
			return error;
		}
		if (!count)
			break;
		used += count;
	}
	if (close(fd) < 0)
		return -errno;
	if (used + 1 == capacity)
		return -EOVERFLOW;
	buffer[used] = '\0';
	return 0;
}

static int parse_real_uid(char *status_contents, uid_t *uid)
{
	char *line = status_contents;

	while (*line) {
		char *line_end = strchr(line, '\n');
		char *cursor;
		char *number_end;
		uint64_t parsed;
		int result;

		if (!line_end)
			line_end = line + strlen(line);
		if ((size_t)(line_end - line) < 4 || memcmp(line, "Uid:", 4)) {
			line = *line_end ? line_end + 1 : line_end;
			continue;
		}

		cursor = line + 4;
		while (cursor < line_end && (*cursor == ' ' || *cursor == '\t'))
			cursor++;
		number_end = cursor;
		while (number_end < line_end &&
		       isdigit((unsigned char)*number_end))
			number_end++;
		result = parse_u64_token(cursor, number_end, &parsed);
		if (result < 0)
			return result;
		if (parsed > UINT32_MAX || (uid_t)parsed != parsed)
			return -ERANGE;
		*uid = parsed;
		return 0;
	}

	return -EINVAL;
}

static int open_process_directory(pid_t pid)
{
	char path[64];
	int fd;

	if (pid <= 0)
		return -EINVAL;
	if (snprintf(path, sizeof(path), "/proc/%ld", (long)pid) >=
	    (int)sizeof(path))
		return -EOVERFLOW;
	fd = open(path, O_RDONLY | O_DIRECTORY | O_CLOEXEC | O_NOFOLLOW);
	return fd < 0 ? -errno : fd;
}

static int validate_process_uid(pid_t pid, uid_t expected_uid)
{
	char contents[PRONK_PROC_FILE_CAPACITY];
	uid_t actual_uid;
	int directory_fd;
	int result;

	directory_fd = open_process_directory(pid);
	if (directory_fd < 0)
		return directory_fd;
	result = read_small_file_at(directory_fd, "status", contents,
				    sizeof(contents));
	if (close(directory_fd) < 0 && !result)
		result = -errno;
	if (result < 0)
		return result;
	result = parse_real_uid(contents, &actual_uid);
	if (result < 0)
		return result;
	return actual_uid == expected_uid ? 0 : -EACCES;
}

static int validate_process_executable(pid_t pid)
{
	struct stat actual_status;
	struct stat installed_status;
	int directory_fd;
	int actual_fd = -1;
	int installed_fd = -1;
	int result = 0;

	directory_fd = open_process_directory(pid);
	if (directory_fd < 0)
		return directory_fd;
	actual_fd = openat(directory_fd, "exe", O_PATH | O_CLOEXEC);
	if (actual_fd < 0) {
		result = -errno;
		goto out;
	}
	installed_fd = open(PRONKD_PATH,
			    O_PATH | O_CLOEXEC | O_NOFOLLOW);
	if (installed_fd < 0) {
		result = -errno;
		goto out;
	}
	if (fstat(actual_fd, &actual_status) < 0 ||
	    fstat(installed_fd, &installed_status) < 0) {
		result = -errno;
		goto out;
	}
	if (!S_ISREG(installed_status.st_mode) || installed_status.st_uid != 0 ||
	    (installed_status.st_mode & (S_IWGRP | S_IWOTH)) ||
	    actual_status.st_dev != installed_status.st_dev ||
	    actual_status.st_ino != installed_status.st_ino)
		result = -EACCES;

out:
	if (installed_fd >= 0)
		close(installed_fd);
	if (actual_fd >= 0)
		close(actual_fd);
	if (close(directory_fd) < 0 && !result)
		result = -errno;
	return result;
}

static int validate_process_unit(pid_t pid)
{
	char *unit = NULL;
	int result;

	result = sd_pid_get_unit(pid, &unit);
	if (result < 0)
		return result;
	if (strcmp(unit, PRONKD_SYSTEM_UNIT))
		result = -EACCES;
	else
		result = 0;
	free(unit);
	return result;
}

static int validate_service_account(uid_t uid)
{
	struct passwd *password;

	errno = 0;
	password = getpwnam(PRONK_SERVICE_USER);
	if (!password)
		return errno ? -errno : -ENOENT;
	if (!password->pw_uid || password->pw_uid != uid)
		return -EACCES;
	return 0;
}

int pronk_caller_parse_uid(const char *value, uid_t *uid)
{
	uint64_t parsed;
	const char *end;
	int result;

	if (!value || !*value || !uid)
		return -EINVAL;
	end = value;
	while (*end)
		end++;
	result = parse_u64_token(value, end, &parsed);
	if (result < 0)
		return result;
	if (!parsed || parsed > UINT32_MAX || (uid_t)parsed != parsed ||
	    (uid_t)parsed == (uid_t)-1)
		return -ERANGE;

	*uid = parsed;
	return 0;
}

int pronk_caller_begin(const struct pronk_peer_credentials *peer,
		       uid_t pkexec_uid, struct pronk_caller *caller)
{
	uid_t real_uid;
	uid_t effective_uid;
	uid_t saved_uid;
	int result;

	if (!peer || !caller || !pkexec_uid)
		return -EINVAL;
	if (getresuid(&real_uid, &effective_uid, &saved_uid) < 0)
		return -errno;
	if (real_uid || effective_uid || saved_uid)
		return -EPERM;
	if (peer->uid != pkexec_uid || peer->pid != getppid())
		return -EACCES;
	result = validate_service_account(pkexec_uid);
	if (result < 0)
		return result;

	caller->pidfd = syscall(SYS_pidfd_open, peer->pid, 0);
	if (caller->pidfd < 0)
		return -errno;
	caller->pid = peer->pid;
	caller->uid = pkexec_uid;
	result = pronk_caller_require_trusted(caller);
	if (result < 0)
		pronk_caller_clear(caller);
	return result;
}

int pronk_caller_validate_request(const struct pronk_caller *caller,
			  const struct pronk_create_request *request)
{
	if (!caller || !request)
		return -EINVAL;
	if (request->expected_daemon_pid != (uint32_t)caller->pid)
		return -EACCES;
	if (!request->connector_id)
		return -EINVAL;
	if (request->profile != PRONK_GRANT_PROFILE_DISPLAY_V1 &&
	    request->profile != PRONK_GRANT_PROFILE_DISPLAY_CEC_V1 &&
	    request->profile != PRONK_GRANT_PROFILE_DISPLAY_CEC_AUDIO_V1)
		return -EOPNOTSUPP;
	return pronk_caller_require_trusted(caller);
}

int pronk_caller_require_trusted(const struct pronk_caller *caller)
{
	struct pollfd descriptor;
	int result;

	if (!caller || caller->pidfd < 0)
		return -EINVAL;
	if (getppid() != caller->pid)
		return -ESRCH;

	descriptor = (struct pollfd) {
		.fd = caller->pidfd,
		.events = POLLIN,
	};
	do {
		result = poll(&descriptor, 1, 0);
	} while (result < 0 && errno == EINTR);
	if (result < 0)
		return -errno;
	if (result || descriptor.revents)
		return -ESRCH;
	result = validate_process_uid(caller->pid, caller->uid);
	if (!result)
		result = validate_process_executable(caller->pid);
	if (!result)
		result = validate_process_unit(caller->pid);
	return result;
}

void pronk_caller_clear(struct pronk_caller *caller)
{
	if (!caller)
		return;
	if (caller->pidfd >= 0)
		close(caller->pidfd);
	caller->pidfd = -1;
}
