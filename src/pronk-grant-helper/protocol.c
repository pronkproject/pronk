#include "protocol.h"

#include <string.h>

static uint16_t get_u16(const uint8_t *bytes, size_t offset)
{
	return (uint16_t)bytes[offset] |
	       (uint16_t)((uint16_t)bytes[offset + 1] << 8);
}

static uint32_t get_u32(const uint8_t *bytes, size_t offset)
{
	return (uint32_t)bytes[offset] |
	       ((uint32_t)bytes[offset + 1] << 8) |
	       ((uint32_t)bytes[offset + 2] << 16) |
	       ((uint32_t)bytes[offset + 3] << 24);
}

static uint64_t get_u64(const uint8_t *bytes, size_t offset)
{
	return (uint64_t)get_u32(bytes, offset) |
	       ((uint64_t)get_u32(bytes, offset + 4) << 32);
}

static void put_u16(uint8_t *bytes, size_t offset, uint16_t value)
{
	bytes[offset] = value;
	bytes[offset + 1] = value >> 8;
}

static void put_u32(uint8_t *bytes, size_t offset, uint32_t value)
{
	bytes[offset] = value;
	bytes[offset + 1] = value >> 8;
	bytes[offset + 2] = value >> 16;
	bytes[offset + 3] = value >> 24;
}

static void put_i32(uint8_t *bytes, size_t offset, int32_t value)
{
	put_u32(bytes, offset, (uint32_t)value);
}

static void put_u64(uint8_t *bytes, size_t offset, uint64_t value)
{
	put_u32(bytes, offset, value);
	put_u32(bytes, offset + 4, value >> 32);
}

static void encode_header(uint8_t *output, uint16_t message_type,
			  uint32_t total_length, uint64_t request_id)
{
	memset(output, 0, total_length);
	memcpy(output, PRONK_PROTOCOL_MAGIC, 4);
	put_u16(output, 4, PRONK_PROTOCOL_MAJOR);
	put_u16(output, 6, PRONK_PROTOCOL_MINOR);
	put_u16(output, 8, message_type);
	put_u16(output, 10, PRONK_PROTOCOL_HEADER_LENGTH);
	put_u32(output, 12, total_length);
	put_u64(output, 16, request_id);
}

static enum pronk_protocol_error decode_request_header(const uint8_t *input,
					       size_t length,
					       uint64_t *request_id)
{
	if (length < PRONK_PROTOCOL_HEADER_LENGTH ||
	    length > PRONK_PROTOCOL_MAX_MESSAGE_LENGTH ||
	    length != PRONK_PROTOCOL_CREATE_REQUEST_LENGTH)
		return PRONK_PROTOCOL_ERROR_LENGTH;
	if (length % 8)
		return PRONK_PROTOCOL_ERROR_ALIGNMENT;
	if (memcmp(input, PRONK_PROTOCOL_MAGIC, 4))
		return PRONK_PROTOCOL_ERROR_MAGIC;
	if (get_u16(input, 4) != PRONK_PROTOCOL_MAJOR)
		return PRONK_PROTOCOL_ERROR_MAJOR;
	if (get_u16(input, 6) != PRONK_PROTOCOL_MINOR)
		return PRONK_PROTOCOL_ERROR_MINOR;
	if (get_u16(input, 8) != PRONK_PROTOCOL_MESSAGE_CREATE_REQUEST)
		return PRONK_PROTOCOL_ERROR_MESSAGE_TYPE;
	if (get_u16(input, 10) != PRONK_PROTOCOL_HEADER_LENGTH)
		return PRONK_PROTOCOL_ERROR_HEADER_LENGTH;
	if (get_u32(input, 12) != length)
		return PRONK_PROTOCOL_ERROR_DECLARED_LENGTH;
	if (get_u32(input, 24))
		return PRONK_PROTOCOL_ERROR_FLAGS;
	if (get_u32(input, 28))
		return PRONK_PROTOCOL_ERROR_RESERVED;

	*request_id = get_u64(input, 16);
	if (!*request_id)
		return PRONK_PROTOCOL_ERROR_REQUEST_ID;

	return PRONK_PROTOCOL_OK;
}

enum pronk_protocol_error
pronk_protocol_encode_hello(uint8_t output[PRONK_PROTOCOL_HELLO_LENGTH],
			    const struct pronk_helper_hello *hello)
{
	if (!output || !hello)
		return PRONK_PROTOCOL_ERROR_LENGTH;

	encode_header(output, PRONK_PROTOCOL_MESSAGE_HELLO,
		      PRONK_PROTOCOL_HELLO_LENGTH, 0);
	put_u32(output, 32, hello->build_major);
	put_u32(output, 36, hello->build_minor);
	put_u32(output, 40, hello->build_patch);
	put_u32(output, 44, hello->pkexec_uid);
	put_u32(output, 48, hello->helper_pid);
	put_u32(output, 52, hello->parent_pid);
	put_u32(output, 56, hello->supported_profiles);
	put_u32(output, 60, hello->helper_features);

	return PRONK_PROTOCOL_OK;
}

enum pronk_protocol_error
pronk_protocol_decode_create_request(const uint8_t *input, size_t length,
			     struct pronk_create_request *request)
{
	enum pronk_protocol_error error;
	uint16_t profile;
	size_t index;

	if (!input || !request)
		return PRONK_PROTOCOL_ERROR_LENGTH;

	error = decode_request_header(input, length, &request->request_id);
	if (error)
		return error;
	profile = get_u16(input, 48);
	if (profile != PRONK_GRANT_PROFILE_DISPLAY_V1 &&
	    profile != PRONK_GRANT_PROFILE_DISPLAY_CEC_V1 &&
	    profile != PRONK_GRANT_PROFILE_DISPLAY_CEC_AUDIO_V1)
		return PRONK_PROTOCOL_ERROR_PROFILE;
	for (index = 50; index < length; index++) {
		if (input[index])
			return PRONK_PROTOCOL_ERROR_RESERVED;
	}

	request->expected_daemon_pid = get_u32(input, 32);
	request->device_major = get_u32(input, 36);
	request->device_minor = get_u32(input, 40);
	request->connector_id = get_u32(input, 44);
	request->profile = profile;
	return PRONK_PROTOCOL_OK;
}

enum pronk_protocol_error
pronk_protocol_encode_create_result(
	uint8_t output[PRONK_PROTOCOL_CREATE_RESULT_LENGTH],
	const struct pronk_create_result *result)
{
	if (!output || !result)
		return PRONK_PROTOCOL_ERROR_LENGTH;
	if (!result->request_id)
		return PRONK_PROTOCOL_ERROR_REQUEST_ID;
	if (result->status > 0)
		return PRONK_PROTOCOL_ERROR_STATUS;
	if (!result->status &&
	    result->diagnostic_stage != PRONK_DIAGNOSTIC_STAGE_NONE)
		return PRONK_PROTOCOL_ERROR_SUCCESS_STAGE;
	if (result->status < 0 &&
	    (result->grant_id || result->connector_id || result->output_index ||
	     result->actual_rights || result->grant_flags ||
	     result->initial_grant_state))
		return PRONK_PROTOCOL_ERROR_FAILURE_METADATA;

	encode_header(output, PRONK_PROTOCOL_MESSAGE_CREATE_RESULT,
		      PRONK_PROTOCOL_CREATE_RESULT_LENGTH, result->request_id);
	put_i32(output, 32, result->status);
	put_u32(output, 36, result->diagnostic_stage);
	put_u32(output, 40, result->grant_id);
	put_u32(output, 44, result->connector_id);
	put_u32(output, 48, result->output_index);
	put_u32(output, 52, result->actual_rights);
	put_u32(output, 56, result->grant_flags);
	put_u32(output, 60, result->initial_grant_state);
	put_u16(output, 64, result->capture_uapi_major);
	put_u16(output, 66, result->capture_uapi_minor);
	put_u32(output, 68, result->helper_features);
	return PRONK_PROTOCOL_OK;
}

const char *pronk_protocol_error_string(enum pronk_protocol_error error)
{
	switch (error) {
	case PRONK_PROTOCOL_OK:
		return "success";
	case PRONK_PROTOCOL_ERROR_LENGTH:
		return "invalid message length";
	case PRONK_PROTOCOL_ERROR_ALIGNMENT:
		return "message length is not eight-byte aligned";
	case PRONK_PROTOCOL_ERROR_MAGIC:
		return "invalid protocol magic";
	case PRONK_PROTOCOL_ERROR_MAJOR:
		return "unsupported protocol major";
	case PRONK_PROTOCOL_ERROR_MINOR:
		return "unsupported protocol minor";
	case PRONK_PROTOCOL_ERROR_MESSAGE_TYPE:
		return "unexpected message type";
	case PRONK_PROTOCOL_ERROR_HEADER_LENGTH:
		return "invalid protocol header length";
	case PRONK_PROTOCOL_ERROR_DECLARED_LENGTH:
		return "declared message length differs from datagram length";
	case PRONK_PROTOCOL_ERROR_FLAGS:
		return "unknown protocol flags";
	case PRONK_PROTOCOL_ERROR_RESERVED:
		return "reserved protocol field is nonzero";
	case PRONK_PROTOCOL_ERROR_REQUEST_ID:
		return "invalid request ID";
	case PRONK_PROTOCOL_ERROR_PROFILE:
		return "unknown grant profile";
	case PRONK_PROTOCOL_ERROR_STATUS:
		return "result status must be zero or a negative errno";
	case PRONK_PROTOCOL_ERROR_FAILURE_METADATA:
		return "failed result contains grant metadata";
	case PRONK_PROTOCOL_ERROR_SUCCESS_STAGE:
		return "successful result contains a diagnostic stage";
	default:
		return "unknown protocol error";
	}
}
