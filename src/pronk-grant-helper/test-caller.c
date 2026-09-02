#include "caller.h"

#include <assert.h>
#include <errno.h>
#include <stdio.h>

int main(void)
{
	uid_t uid;

	assert(pronk_caller_parse_uid("1000", &uid) == 0);
	assert(uid == 1000);
	assert(pronk_caller_parse_uid("65534", &uid) == 0);
	assert(uid == 65534);
	assert(pronk_caller_parse_uid(NULL, &uid) == -EINVAL);
	assert(pronk_caller_parse_uid("", &uid) == -EINVAL);
	assert(pronk_caller_parse_uid("0", &uid) == -ERANGE);
	assert(pronk_caller_parse_uid("-1", &uid) == -EINVAL);
	assert(pronk_caller_parse_uid("+1", &uid) == -EINVAL);
	assert(pronk_caller_parse_uid(" 1", &uid) == -EINVAL);
	assert(pronk_caller_parse_uid("1 ", &uid) == -EINVAL);
	assert(pronk_caller_parse_uid("1x", &uid) == -EINVAL);
	assert(pronk_caller_parse_uid("4294967295", &uid) == -ERANGE);
	assert(pronk_caller_parse_uid("4294967296", &uid) == -ERANGE);

	puts("C helper caller tests passed");
	return 0;
}
