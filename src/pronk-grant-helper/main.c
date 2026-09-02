#define _GNU_SOURCE

#include "caller.h"
#include "config.h"
#include "device.h"
#include "grant.h"
#include "protocol.h"
#include "transport.h"

#include <errno.h>
#include <grp.h>
#include <linux/capability.h>
#include <linux/close_range.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <unistd.h>

#define PRONK_PROTOCOL_FD STDOUT_FILENO

static const char *diagnostic_stage_name(enum pronk_diagnostic_stage stage)
{
	switch (stage) {
	case PRONK_DIAGNOSTIC_STAGE_PROTOCOL:
		return "protocol";
	case PRONK_DIAGNOSTIC_STAGE_CALLER:
		return "caller";
	case PRONK_DIAGNOSTIC_STAGE_DEVICE:
		return "device";
	case PRONK_DIAGNOSTIC_STAGE_CONNECTOR:
		return "connector";
	case PRONK_DIAGNOSTIC_STAGE_CREATE_GRANT:
		return "grant creation";
	case PRONK_DIAGNOSTIC_STAGE_VERIFY_GRANT:
		return "grant verification";
	case PRONK_DIAGNOSTIC_STAGE_SEND_RESULT:
		return "result transfer";
	case PRONK_DIAGNOSTIC_STAGE_DROP_MASTER:
		return "drop DRM master";
	case PRONK_DIAGNOSTIC_STAGE_NONE:
	default:
		return "unknown";
	}
}

static void report_failure(enum pronk_diagnostic_stage stage, int result)
{
	int error = result < 0 ? -result : EIO;

	fprintf(stderr, "pronk-grant-helper: %s failed: %s (%d)\n",
		diagnostic_stage_name(stage), strerror(error), error);
}

static int send_hello(uid_t pkexec_uid, pid_t parent_pid)
{
	const struct pronk_helper_hello hello = {
		.build_major = PRONK_BUILD_MAJOR,
		.build_minor = PRONK_BUILD_MINOR,
		.build_patch = PRONK_BUILD_PATCH,
		.pkexec_uid = pkexec_uid,
		.helper_pid = getpid(),
		.parent_pid = parent_pid,
		.supported_profiles = PRONK_PROFILE_MASK_DISPLAY_V1 |
			PRONK_PROFILE_MASK_DISPLAY_CEC_V1 |
			PRONK_PROFILE_MASK_DISPLAY_CEC_AUDIO_V1,
		.helper_features = PRONK_HELPER_FEATURE_ADMIN_CONTROL_FD,
	};
	uint8_t payload[PRONK_PROTOCOL_HELLO_LENGTH];
	enum pronk_protocol_error protocol_error;

	protocol_error = pronk_protocol_encode_hello(payload, &hello);
	if (protocol_error)
		return -EPROTO;
	return pronk_transport_send(PRONK_PROTOCOL_FD, payload,
				    sizeof(payload), NULL, 0);
}

static int send_result(const struct pronk_create_request *request,
		       const struct pronk_device *device,
		       const struct pronk_grant *grant, int status,
		       enum pronk_diagnostic_stage stage,
		       const int *transferred_fds,
		       size_t transferred_fd_count)
{
	struct pronk_create_result result = {
		.request_id = request->request_id,
		.status = status,
		.diagnostic_stage = stage,
		.capture_uapi_major = device->capture_uapi_major,
		.capture_uapi_minor = device->capture_uapi_minor,
		.helper_features = PRONK_HELPER_FEATURE_ADMIN_CONTROL_FD,
	};
	uint8_t payload[PRONK_PROTOCOL_CREATE_RESULT_LENGTH];
	enum pronk_protocol_error protocol_error;

	if (!status && grant) {
		result.grant_id = grant->grant_id;
		result.connector_id = grant->connector_id;
		result.output_index = grant->output_index;
		result.actual_rights = grant->rights;
		result.grant_flags = grant->flags;
		result.initial_grant_state = grant->state;
	}
	protocol_error = pronk_protocol_encode_create_result(payload, &result);
	if (protocol_error)
		return -EPROTO;
	return pronk_transport_send(PRONK_PROTOCOL_FD, payload,
				    sizeof(payload), transferred_fds,
				    transferred_fd_count);
}

static int send_failure(const struct pronk_create_request *request,
			const struct pronk_device *device, int status,
			enum pronk_diagnostic_stage stage)
{
	int send_status;

	if (status >= 0)
		status = -EIO;
	report_failure(stage, status);
	send_status = send_result(request, device, NULL, status, stage, NULL, 0);
	if (send_status < 0)
		report_failure(PRONK_DIAGNOSTIC_STAGE_SEND_RESULT, send_status);
	return EXIT_FAILURE;
}

static int harden_process(void)
{
	struct __user_cap_header_struct capability_header = {
		.version = _LINUX_CAPABILITY_VERSION_3,
		.pid = 0,
	};
	struct __user_cap_data_struct capabilities[_LINUX_CAPABILITY_U32S_3] = {
		{ 0 },
	};
	const unsigned int admin_word = CAP_SYS_ADMIN / 32U;
	const uint32_t admin_bit = 1U << (CAP_SYS_ADMIN % 32U);
	const unsigned int ptrace_word = CAP_SYS_PTRACE / 32U;
	const uint32_t ptrace_bit = 1U << (CAP_SYS_PTRACE % 32U);

	if (prctl(PR_SET_DUMPABLE, 0, 0, 0, 0) < 0)
		return -errno;
	if (setgroups(0, NULL) < 0)
		return -errno;
	capabilities[admin_word].effective |= admin_bit;
	capabilities[admin_word].permitted |= admin_bit;
	capabilities[ptrace_word].effective |= ptrace_bit;
	capabilities[ptrace_word].permitted |= ptrace_bit;
	if (syscall(SYS_capset, &capability_header, capabilities) < 0)
		return -errno;
	if (prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) < 0)
		return -errno;
	if (syscall(SYS_close_range, 3U, ~0U, CLOSE_RANGE_UNSHARE) < 0)
		return -errno;
	umask(077);
	if (chdir("/") < 0)
		return -errno;
	return 0;
}

int main(int argc, char **argv)
{
	uint8_t request_payload[PRONK_PROTOCOL_MAX_MESSAGE_LENGTH];
	struct pronk_peer_credentials peer;
	struct pronk_create_request request;
	struct pronk_caller caller = {
		.pidfd = -1,
	};
	struct pronk_device device = {
		.fd = -1,
	};
	struct pronk_grant grant = {
		.holder_fd = -1,
		.control_fd = -1,
	};
	enum pronk_diagnostic_stage grant_failure_stage;
	enum pronk_protocol_error protocol_error;
	const char *pkexec_uid_value;
	uid_t pkexec_uid;
	size_t request_length;
	int transferred_fds[2];
	int result;
	int exit_status = EXIT_FAILURE;

	(void)argv;
	if (argc != 1) {
		fprintf(stderr, "pronk-grant-helper: expected no arguments\n");
		return 2;
	}
	result = harden_process();
	if (result < 0) {
		report_failure(PRONK_DIAGNOSTIC_STAGE_CALLER, result);
		return EXIT_FAILURE;
	}

	pkexec_uid_value = getenv("PKEXEC_UID");
	result = pronk_caller_parse_uid(pkexec_uid_value, &pkexec_uid);
	if (result < 0) {
		report_failure(PRONK_DIAGNOSTIC_STAGE_CALLER, result);
		return EXIT_FAILURE;
	}
	if (clearenv() < 0) {
		report_failure(PRONK_DIAGNOSTIC_STAGE_CALLER, -errno);
		return EXIT_FAILURE;
	}
	result = pronk_transport_validate_socket(PRONK_PROTOCOL_FD, &peer);
	if (result < 0) {
		report_failure(PRONK_DIAGNOSTIC_STAGE_PROTOCOL, result);
		return EXIT_FAILURE;
	}
	result = pronk_caller_begin(&peer, pkexec_uid, &caller);
	if (result < 0) {
		report_failure(PRONK_DIAGNOSTIC_STAGE_CALLER, result);
		return EXIT_FAILURE;
	}

	result = send_hello(pkexec_uid, caller.pid);
	if (result < 0) {
		report_failure(PRONK_DIAGNOSTIC_STAGE_PROTOCOL, result);
		goto out;
	}
	result = pronk_transport_receive(PRONK_PROTOCOL_FD, request_payload,
					 sizeof(request_payload),
					 &request_length);
	if (result < 0) {
		report_failure(PRONK_DIAGNOSTIC_STAGE_PROTOCOL, result);
		goto out;
	}
	protocol_error = pronk_protocol_decode_create_request(
		request_payload, request_length, &request);
	if (protocol_error) {
		fprintf(stderr, "pronk-grant-helper: invalid request: %s\n",
			pronk_protocol_error_string(protocol_error));
		goto out;
	}

	result = pronk_caller_validate_request(&caller, &request);
	if (result < 0) {
		exit_status = send_failure(&request, &device, result,
					   PRONK_DIAGNOSTIC_STAGE_CALLER);
		goto out;
	}
	result = pronk_device_open(&request, &device);
	if (result < 0) {
		exit_status = send_failure(&request, &device, result,
					   PRONK_DIAGNOSTIC_STAGE_DEVICE);
		goto out;
	}
	result = pronk_device_validate_connector(&request, &device);
	if (result < 0) {
		exit_status = send_failure(&request, &device, result,
					   PRONK_DIAGNOSTIC_STAGE_CONNECTOR);
		goto out;
	}

	/* Keep the authorization-to-grant-creation window as small as possible. */
	result = pronk_caller_require_trusted(&caller);
	if (result < 0) {
		exit_status = send_failure(&request, &device, result,
					   PRONK_DIAGNOSTIC_STAGE_CALLER);
		goto out;
	}
	result = pronk_grant_create(&request, &device, &grant,
				    &grant_failure_stage);
	if (result < 0) {
		exit_status = send_failure(&request, &device, result,
					   grant_failure_stage);
		goto out;
	}

	result = pronk_caller_require_trusted(&caller);
	if (result < 0) {
		pronk_grant_clear(&grant);
		exit_status = send_failure(&request, &device, result,
					   PRONK_DIAGNOSTIC_STAGE_CALLER);
		goto out;
	}

	transferred_fds[0] = grant.holder_fd;
	transferred_fds[1] = grant.control_fd;
	result = send_result(&request, &device, &grant, 0,
			     PRONK_DIAGNOSTIC_STAGE_NONE, transferred_fds,
			     sizeof(transferred_fds) /
				     sizeof(transferred_fds[0]));
	if (result < 0) {
		report_failure(PRONK_DIAGNOSTIC_STAGE_SEND_RESULT, result);
		goto out;
	}

	exit_status = EXIT_SUCCESS;

out:
	pronk_grant_clear(&grant);
	pronk_device_clear(&device);
	pronk_caller_clear(&caller);
	close(PRONK_PROTOCOL_FD);
	return exit_status;
}
