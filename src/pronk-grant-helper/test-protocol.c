#include "protocol.h"

#include <assert.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

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

static void put_u64(uint8_t *bytes, size_t offset, uint64_t value)
{
	put_u32(bytes, offset, value);
	put_u32(bytes, offset + 4, value >> 32);
}

static uint16_t get_u16(const uint8_t *bytes, size_t offset)
{
	return bytes[offset] | (uint16_t)bytes[offset + 1] << 8;
}

static uint32_t get_u32(const uint8_t *bytes, size_t offset)
{
	return bytes[offset] | (uint32_t)bytes[offset + 1] << 8 |
	       (uint32_t)bytes[offset + 2] << 16 |
	       (uint32_t)bytes[offset + 3] << 24;
}

static uint64_t get_u64(const uint8_t *bytes, size_t offset)
{
	return get_u32(bytes, offset) | (uint64_t)get_u32(bytes, offset + 4) << 32;
}

static void make_request(uint8_t bytes[PRONK_PROTOCOL_CREATE_REQUEST_LENGTH])
{
	memset(bytes, 0, PRONK_PROTOCOL_CREATE_REQUEST_LENGTH);
	memcpy(bytes, PRONK_PROTOCOL_MAGIC, 4);
	put_u16(bytes, 4, PRONK_PROTOCOL_MAJOR);
	put_u16(bytes, 6, PRONK_PROTOCOL_MINOR);
	put_u16(bytes, 8, PRONK_PROTOCOL_MESSAGE_CREATE_REQUEST);
	put_u16(bytes, 10, PRONK_PROTOCOL_HEADER_LENGTH);
	put_u32(bytes, 12, PRONK_PROTOCOL_CREATE_REQUEST_LENGTH);
	put_u64(bytes, 16, 0x0123456789abcdefULL);
	put_u32(bytes, 32, 4242);
	put_u32(bytes, 36, 226);
	put_u32(bytes, 40, 0);
	put_u32(bytes, 44, 37);
	put_u16(bytes, 48, PRONK_GRANT_PROFILE_DISPLAY_V1);
}

static void test_hello(void)
{
	const struct pronk_helper_hello hello = {
		.build_major = 1,
		.build_minor = 2,
		.build_patch = 3,
		.pkexec_uid = 1000,
		.helper_pid = 2000,
		.parent_pid = 1999,
		.supported_profiles = PRONK_PROFILE_MASK_DISPLAY_V1,
		.helper_features = PRONK_HELPER_FEATURE_ADMIN_CONTROL_FD,
	};
	uint8_t bytes[PRONK_PROTOCOL_HELLO_LENGTH];

	assert(pronk_protocol_encode_hello(bytes, &hello) == PRONK_PROTOCOL_OK);
	assert(!memcmp(bytes, "PRNK", 4));
	assert(get_u16(bytes, 4) == PRONK_PROTOCOL_MAJOR);
	assert(get_u16(bytes, 6) == PRONK_PROTOCOL_MINOR);
	assert(get_u16(bytes, 8) == PRONK_PROTOCOL_MESSAGE_HELLO);
	assert(get_u16(bytes, 10) == PRONK_PROTOCOL_HEADER_LENGTH);
	assert(get_u32(bytes, 12) == PRONK_PROTOCOL_HELLO_LENGTH);
	assert(get_u64(bytes, 16) == 0);
	assert(get_u32(bytes, 24) == 0);
	assert(get_u32(bytes, 28) == 0);
	assert(get_u32(bytes, 32) == 1);
	assert(get_u32(bytes, 36) == 2);
	assert(get_u32(bytes, 40) == 3);
	assert(get_u32(bytes, 44) == 1000);
	assert(get_u32(bytes, 48) == 2000);
	assert(get_u32(bytes, 52) == 1999);
	assert(get_u32(bytes, 56) == PRONK_PROFILE_MASK_DISPLAY_V1);
	assert(get_u32(bytes, 60) ==
	       PRONK_HELPER_FEATURE_ADMIN_CONTROL_FD);
}

static void test_request(void)
{
	uint8_t bytes[PRONK_PROTOCOL_CREATE_REQUEST_LENGTH];
	struct pronk_create_request request;

	make_request(bytes);
	assert(pronk_protocol_decode_create_request(bytes, sizeof(bytes),
						    &request) ==
	       PRONK_PROTOCOL_OK);
	assert(request.request_id == 0x0123456789abcdefULL);
	assert(request.expected_daemon_pid == 4242);
	assert(request.device_major == 226);
	assert(request.device_minor == 0);
	assert(request.connector_id == 37);
	assert(request.profile == PRONK_GRANT_PROFILE_DISPLAY_V1);

	assert(pronk_protocol_decode_create_request(bytes, sizeof(bytes) - 1,
						    &request) ==
	       PRONK_PROTOCOL_ERROR_LENGTH);

	make_request(bytes);
	bytes[0] = 'X';
	assert(pronk_protocol_decode_create_request(bytes, sizeof(bytes),
						    &request) ==
	       PRONK_PROTOCOL_ERROR_MAGIC);

	make_request(bytes);
	put_u16(bytes, 4, PRONK_PROTOCOL_MAJOR + 1);
	assert(pronk_protocol_decode_create_request(bytes, sizeof(bytes),
						    &request) ==
	       PRONK_PROTOCOL_ERROR_MAJOR);

	make_request(bytes);
	put_u16(bytes, 6, PRONK_PROTOCOL_MINOR + 1);
	assert(pronk_protocol_decode_create_request(bytes, sizeof(bytes),
						    &request) ==
	       PRONK_PROTOCOL_ERROR_MINOR);

	make_request(bytes);
	put_u16(bytes, 8, PRONK_PROTOCOL_MESSAGE_HELLO);
	assert(pronk_protocol_decode_create_request(bytes, sizeof(bytes),
						    &request) ==
	       PRONK_PROTOCOL_ERROR_MESSAGE_TYPE);

	make_request(bytes);
	put_u16(bytes, 10, 24);
	assert(pronk_protocol_decode_create_request(bytes, sizeof(bytes),
						    &request) ==
	       PRONK_PROTOCOL_ERROR_HEADER_LENGTH);

	make_request(bytes);
	put_u32(bytes, 12, sizeof(bytes) - 8);
	assert(pronk_protocol_decode_create_request(bytes, sizeof(bytes),
						    &request) ==
	       PRONK_PROTOCOL_ERROR_DECLARED_LENGTH);

	make_request(bytes);
	put_u64(bytes, 16, 0);
	assert(pronk_protocol_decode_create_request(bytes, sizeof(bytes),
						    &request) ==
	       PRONK_PROTOCOL_ERROR_REQUEST_ID);

	make_request(bytes);
	put_u32(bytes, 24, 1);
	assert(pronk_protocol_decode_create_request(bytes, sizeof(bytes),
						    &request) ==
	       PRONK_PROTOCOL_ERROR_FLAGS);

	make_request(bytes);
	put_u32(bytes, 28, 1);
	assert(pronk_protocol_decode_create_request(bytes, sizeof(bytes),
						    &request) ==
	       PRONK_PROTOCOL_ERROR_RESERVED);

	make_request(bytes);
	put_u16(bytes, 48, 99);
	assert(pronk_protocol_decode_create_request(bytes, sizeof(bytes),
						    &request) ==
	       PRONK_PROTOCOL_ERROR_PROFILE);

	make_request(bytes);
	bytes[50] = 1;
	assert(pronk_protocol_decode_create_request(bytes, sizeof(bytes),
						    &request) ==
	       PRONK_PROTOCOL_ERROR_RESERVED);
}

static void test_result(void)
{
	const struct pronk_create_result success = {
		.request_id = 0x0123456789abcdefULL,
		.status = 0,
		.diagnostic_stage = PRONK_DIAGNOSTIC_STAGE_NONE,
		.grant_id = 11,
		.connector_id = 37,
		.output_index = 4,
		.actual_rights = 0xf,
		.grant_flags = 1,
		.initial_grant_state = 1,
		.capture_uapi_major = 0,
		.capture_uapi_minor = 12,
		.helper_features = PRONK_HELPER_FEATURE_ADMIN_CONTROL_FD,
	};
	struct pronk_create_result invalid;
	uint8_t bytes[PRONK_PROTOCOL_CREATE_RESULT_LENGTH];

	assert(pronk_protocol_encode_create_result(bytes, &success) ==
	       PRONK_PROTOCOL_OK);
	assert(!memcmp(bytes, "PRNK", 4));
	assert(get_u16(bytes, 8) == PRONK_PROTOCOL_MESSAGE_CREATE_RESULT);
	assert(get_u32(bytes, 12) == PRONK_PROTOCOL_CREATE_RESULT_LENGTH);
	assert(get_u64(bytes, 16) == success.request_id);
	assert(get_u32(bytes, 32) == 0);
	assert(get_u32(bytes, 36) == PRONK_DIAGNOSTIC_STAGE_NONE);
	assert(get_u32(bytes, 40) == 11);
	assert(get_u32(bytes, 44) == 37);
	assert(get_u32(bytes, 48) == 4);
	assert(get_u32(bytes, 52) == 0xf);
	assert(get_u32(bytes, 56) == 1);
	assert(get_u32(bytes, 60) == 1);
	assert(get_u16(bytes, 64) == 0);
	assert(get_u16(bytes, 66) == 12);
	assert(get_u32(bytes, 68) ==
	       PRONK_HELPER_FEATURE_ADMIN_CONTROL_FD);
	assert(get_u64(bytes, 72) == 0);

	invalid = success;
	invalid.request_id = 0;
	assert(pronk_protocol_encode_create_result(bytes, &invalid) ==
	       PRONK_PROTOCOL_ERROR_REQUEST_ID);
	invalid = success;
	invalid.status = 1;
	assert(pronk_protocol_encode_create_result(bytes, &invalid) ==
	       PRONK_PROTOCOL_ERROR_STATUS);
	invalid = success;
	invalid.diagnostic_stage = PRONK_DIAGNOSTIC_STAGE_CALLER;
	assert(pronk_protocol_encode_create_result(bytes, &invalid) ==
	       PRONK_PROTOCOL_ERROR_SUCCESS_STAGE);
	invalid = success;
	invalid.status = -13;
	invalid.diagnostic_stage = PRONK_DIAGNOSTIC_STAGE_CALLER;
	assert(pronk_protocol_encode_create_result(bytes, &invalid) ==
	       PRONK_PROTOCOL_ERROR_FAILURE_METADATA);
	invalid.grant_id = 0;
	invalid.connector_id = 0;
	invalid.output_index = 0;
	invalid.actual_rights = 0;
	invalid.grant_flags = 0;
	invalid.initial_grant_state = 0;
	assert(pronk_protocol_encode_create_result(bytes, &invalid) ==
	       PRONK_PROTOCOL_OK);
}

int main(void)
{
	test_hello();
	test_request();
	test_result();
	puts("C helper protocol tests passed");
	return 0;
}
