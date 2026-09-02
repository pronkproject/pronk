#pragma once

#include <stddef.h>
#include <stdint.h>

#define PRONK_PROTOCOL_MAGIC "PRNK"
#define PRONK_PROTOCOL_MAJOR 2U
#define PRONK_PROTOCOL_MINOR 0U

#define PRONK_PROTOCOL_HEADER_LENGTH 32U
#define PRONK_PROTOCOL_MAX_MESSAGE_LENGTH 128U
#define PRONK_PROTOCOL_HELLO_LENGTH 64U
#define PRONK_PROTOCOL_CREATE_REQUEST_LENGTH 64U
#define PRONK_PROTOCOL_CREATE_RESULT_LENGTH 80U

#define PRONK_HELPER_FEATURE_ADMIN_CONTROL_FD (1U << 0)

#define PRONK_PROFILE_MASK_DISPLAY_V1 (1U << 0)
#define PRONK_PROFILE_MASK_DISPLAY_CEC_V1 (1U << 1)
#define PRONK_PROFILE_MASK_DISPLAY_CEC_AUDIO_V1 (1U << 2)

enum pronk_protocol_message_type {
	PRONK_PROTOCOL_MESSAGE_HELLO = 1,
	PRONK_PROTOCOL_MESSAGE_CREATE_REQUEST = 2,
	PRONK_PROTOCOL_MESSAGE_CREATE_RESULT = 3,
};

enum pronk_grant_profile {
	PRONK_GRANT_PROFILE_DISPLAY_V1 = 1,
	PRONK_GRANT_PROFILE_DISPLAY_CEC_V1 = 2,
	PRONK_GRANT_PROFILE_DISPLAY_CEC_AUDIO_V1 = 3,
};

enum pronk_diagnostic_stage {
	PRONK_DIAGNOSTIC_STAGE_NONE = 0,
	PRONK_DIAGNOSTIC_STAGE_PROTOCOL = 1,
	PRONK_DIAGNOSTIC_STAGE_CALLER = 2,
	PRONK_DIAGNOSTIC_STAGE_DEVICE = 3,
	PRONK_DIAGNOSTIC_STAGE_CONNECTOR = 4,
	PRONK_DIAGNOSTIC_STAGE_CREATE_GRANT = 5,
	PRONK_DIAGNOSTIC_STAGE_VERIFY_GRANT = 6,
	PRONK_DIAGNOSTIC_STAGE_DROP_MASTER = 7,
	PRONK_DIAGNOSTIC_STAGE_SEND_RESULT = 8,
};

enum pronk_protocol_error {
	PRONK_PROTOCOL_OK = 0,
	PRONK_PROTOCOL_ERROR_LENGTH,
	PRONK_PROTOCOL_ERROR_ALIGNMENT,
	PRONK_PROTOCOL_ERROR_MAGIC,
	PRONK_PROTOCOL_ERROR_MAJOR,
	PRONK_PROTOCOL_ERROR_MINOR,
	PRONK_PROTOCOL_ERROR_MESSAGE_TYPE,
	PRONK_PROTOCOL_ERROR_HEADER_LENGTH,
	PRONK_PROTOCOL_ERROR_DECLARED_LENGTH,
	PRONK_PROTOCOL_ERROR_FLAGS,
	PRONK_PROTOCOL_ERROR_RESERVED,
	PRONK_PROTOCOL_ERROR_REQUEST_ID,
	PRONK_PROTOCOL_ERROR_PROFILE,
	PRONK_PROTOCOL_ERROR_STATUS,
	PRONK_PROTOCOL_ERROR_FAILURE_METADATA,
	PRONK_PROTOCOL_ERROR_SUCCESS_STAGE,
};

struct pronk_helper_hello {
	uint32_t build_major;
	uint32_t build_minor;
	uint32_t build_patch;
	uint32_t pkexec_uid;
	uint32_t helper_pid;
	uint32_t parent_pid;
	uint32_t supported_profiles;
	uint32_t helper_features;
};

struct pronk_create_request {
	uint64_t request_id;
	uint32_t expected_daemon_pid;
	uint32_t device_major;
	uint32_t device_minor;
	uint32_t connector_id;
	uint16_t profile;
};

struct pronk_create_result {
	uint64_t request_id;
	int32_t status;
	uint32_t diagnostic_stage;
	uint32_t grant_id;
	uint32_t connector_id;
	uint32_t output_index;
	uint32_t actual_rights;
	uint32_t grant_flags;
	uint32_t initial_grant_state;
	uint16_t capture_uapi_major;
	uint16_t capture_uapi_minor;
	uint32_t helper_features;
};

enum pronk_protocol_error
pronk_protocol_encode_hello(uint8_t output[PRONK_PROTOCOL_HELLO_LENGTH],
			    const struct pronk_helper_hello *hello);

enum pronk_protocol_error
pronk_protocol_decode_create_request(const uint8_t *input, size_t length,
			     struct pronk_create_request *request);

enum pronk_protocol_error
pronk_protocol_encode_create_result(
	uint8_t output[PRONK_PROTOCOL_CREATE_RESULT_LENGTH],
	const struct pronk_create_result *result);

const char *pronk_protocol_error_string(enum pronk_protocol_error error);
