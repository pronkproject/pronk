// SPDX-License-Identifier: GPL-2.0-only

#include <errno.h>
#include <getopt.h>
#include <limits.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#include <drm/drm_fourcc.h>

#include <pipewire/pipewire.h>
#include <spa/buffer/meta.h>
#include <spa/param/format-utils.h>
#include <spa/param/video/format-utils.h>
#include <spa/pod/builder.h>
#include <spa/utils/result.h>

#define DEFAULT_FRAME_COUNT 30
#define DEFAULT_TIMEOUT_SEC 15
#define FINAL_RELEASE_GRACE_MSEC 1000
#define REQUIRED_BUFFER_COUNT 4
#define MAX_FRAME_DIMENSION 8192U

struct consumer {
	struct pw_main_loop *loop;
	struct pw_context *context;
	struct pw_core *core;
	struct spa_hook core_listener;
	struct pw_stream *stream;
	struct spa_hook stream_listener;
	struct spa_source *timer;

	uint32_t width;
	uint32_t height;
	bool format_negotiated;
	bool streaming;
	bool target_reached;
	bool wait_for_disconnect;
	bool disconnected_after_target;
	bool timed_out;

	int target_frames;
	int frames_received;
	uint64_t last_sequence;
	int64_t last_pts;
	int sequence_errors;
	int timestamp_errors;
	int header_errors;
	int damage_errors;
	int sync_errors;
	int data_errors;
	int format_errors;
};

static void quit_after_final_release(struct consumer *consumer)
{
	struct timespec grace = {
		.tv_sec = FINAL_RELEASE_GRACE_MSEC / 1000,
		.tv_nsec = (FINAL_RELEASE_GRACE_MSEC % 1000) * 1000000L,
	};

	consumer->target_reached = true;
	if (consumer->wait_for_disconnect) {
		printf("frames_target_reached=%d\n", consumer->frames_received);
		fflush(stdout);
		return;
	}
	pw_loop_update_timer(pw_main_loop_get_loop(consumer->loop),
			     consumer->timer, &grace, NULL, false);
}

static void on_timeout(void *data, uint64_t expirations)
{
	struct consumer *consumer = data;

	(void)expirations;
	if (!consumer->target_reached || consumer->wait_for_disconnect)
		consumer->timed_out = true;
	pw_main_loop_quit(consumer->loop);
}

static void on_state_changed(void *data, enum pw_stream_state old,
			     enum pw_stream_state state, const char *error)
{
	struct consumer *consumer = data;

	(void)old;
	fprintf(stderr, "consumer stream: %s",
		pw_stream_state_as_string(state));
	if (error)
		fprintf(stderr, " (%s)", error);
	fputc('\n', stderr);

	switch (state) {
	case PW_STREAM_STATE_STREAMING:
		consumer->streaming = true;
		break;
	case PW_STREAM_STATE_ERROR:
		pw_main_loop_quit(consumer->loop);
		break;
	case PW_STREAM_STATE_UNCONNECTED:
		if (consumer->streaming) {
			consumer->disconnected_after_target =
				consumer->target_reached;
			pw_main_loop_quit(consumer->loop);
		}
		break;
	default:
		break;
	}
}

static void on_param_changed(void *data, uint32_t id,
			     const struct spa_pod *param)
{
	struct consumer *consumer = data;
	struct spa_video_info_raw info = { 0 };
	uint8_t params_buffer[2048];
	struct spa_pod_builder builder;
	const struct spa_pod *params[4];
	struct spa_pod_frame frame;
	uint32_t media_type;
	uint32_t media_subtype;
	int n_params = 0;
	int result;

	if (!param || id != SPA_PARAM_Format)
		return;

	result = spa_format_parse(param, &media_type, &media_subtype);
	if (result < 0 || media_type != SPA_MEDIA_TYPE_video ||
	    media_subtype != SPA_MEDIA_SUBTYPE_raw ||
	    spa_format_video_raw_parse(param, &info) < 0 ||
	    info.format != SPA_VIDEO_FORMAT_BGRx ||
	    !(info.flags & SPA_VIDEO_FLAG_MODIFIER) ||
	    info.modifier != DRM_FORMAT_MOD_LINEAR || !info.size.width ||
	    !info.size.height || info.size.width > MAX_FRAME_DIMENSION ||
	    info.size.height > MAX_FRAME_DIMENSION) {
		fprintf(stderr, "unsupported negotiated video format\n");
		consumer->format_errors++;
		pw_main_loop_quit(consumer->loop);
		return;
	}

	consumer->width = info.size.width;
	consumer->height = info.size.height;
	consumer->format_negotiated = true;

	spa_pod_builder_init(&builder, params_buffer, sizeof(params_buffer));
	spa_pod_builder_push_object(&builder, &frame,
				    SPA_TYPE_OBJECT_ParamBuffers,
				    SPA_PARAM_Buffers);
	spa_pod_builder_add(&builder,
		SPA_PARAM_BUFFERS_buffers,
			SPA_POD_CHOICE_RANGE_Int(REQUIRED_BUFFER_COUNT,
						 REQUIRED_BUFFER_COUNT,
						 REQUIRED_BUFFER_COUNT),
		SPA_PARAM_BUFFERS_blocks, SPA_POD_Int(3),
		SPA_PARAM_BUFFERS_dataType,
			SPA_POD_CHOICE_FLAGS_Int(1 << SPA_DATA_DmaBuf), 0);
	spa_pod_builder_prop(&builder, SPA_PARAM_BUFFERS_metaType,
			     SPA_POD_PROP_FLAG_MANDATORY);
	spa_pod_builder_int(&builder, 1 << SPA_META_SyncTimeline);
	params[n_params++] = spa_pod_builder_pop(&builder, &frame);

	params[n_params++] = spa_pod_builder_add_object(&builder,
		SPA_TYPE_OBJECT_ParamMeta, SPA_PARAM_Meta,
		SPA_PARAM_META_type, SPA_POD_Id(SPA_META_Header),
		SPA_PARAM_META_size,
			SPA_POD_Int(sizeof(struct spa_meta_header)));
	params[n_params++] = spa_pod_builder_add_object(&builder,
		SPA_TYPE_OBJECT_ParamMeta, SPA_PARAM_Meta,
		SPA_PARAM_META_type, SPA_POD_Id(SPA_META_VideoDamage),
		SPA_PARAM_META_size,
			SPA_POD_Int(sizeof(struct spa_meta_region)));
	params[n_params++] = spa_pod_builder_add_object(&builder,
		SPA_TYPE_OBJECT_ParamMeta, SPA_PARAM_Meta,
		SPA_PARAM_META_type, SPA_POD_Id(SPA_META_SyncTimeline),
		SPA_PARAM_META_size,
			SPA_POD_Int(sizeof(struct spa_meta_sync_timeline)));

	result = pw_stream_update_params(consumer->stream, params, n_params);
	if (result < 0) {
		fprintf(stderr, "pw_stream_update_params: %s\n",
			spa_strerror(result));
		consumer->format_errors++;
		pw_main_loop_quit(consumer->loop);
	}
}

static bool valid_dma_buf(const struct consumer *consumer,
			  const struct spa_data *data)
{
	uint64_t minimum_size;

	if (data->type != SPA_DATA_DmaBuf || data->fd < 0 ||
	    data->fd > INT_MAX || !(data->flags & SPA_DATA_FLAG_READABLE) ||
	    !data->chunk || data->chunk->stride <= 0 ||
	    data->chunk->offset > data->maxsize ||
	    data->chunk->size > data->maxsize - data->chunk->offset)
		return false;
	if ((uint32_t)data->chunk->stride < consumer->width * 4U)
		return false;
	minimum_size = (uint64_t)(uint32_t)data->chunk->stride *
		       consumer->height;
	return minimum_size <= data->chunk->size;
}

static bool valid_syncobj(const struct spa_data *data)
{
	return data->type == SPA_DATA_SyncObj && data->fd >= 0 &&
	       data->fd <= INT_MAX;
}

static void validate_header(struct consumer *consumer,
			    struct spa_buffer *buffer)
{
	struct spa_meta_header *header;

	header = spa_buffer_find_meta_data(buffer, SPA_META_Header,
				   sizeof(*header));
	if (!header || !header->seq || header->pts <= 0) {
		consumer->header_errors++;
		return;
	}
	if (consumer->frames_received > 0) {
		if (header->seq <= consumer->last_sequence)
			consumer->sequence_errors++;
		if (header->pts <= consumer->last_pts)
			consumer->timestamp_errors++;
	}
	consumer->last_sequence = header->seq;
	consumer->last_pts = header->pts;
}

static void validate_damage(struct consumer *consumer,
			    struct spa_buffer *buffer)
{
	struct spa_meta_region *damage;
	uint64_t right;
	uint64_t bottom;

	damage = spa_buffer_find_meta_data(buffer, SPA_META_VideoDamage,
				   sizeof(*damage));
	if (!damage || damage->region.position.x < 0 ||
	    damage->region.position.y < 0 || !damage->region.size.width ||
	    !damage->region.size.height) {
		consumer->damage_errors++;
		return;
	}
	right = (uint32_t)damage->region.position.x +
		damage->region.size.width;
	bottom = (uint32_t)damage->region.position.y +
		 damage->region.size.height;
	if (right > consumer->width || bottom > consumer->height)
		consumer->damage_errors++;
}

static void validate_sync_timeline(struct consumer *consumer,
				   struct spa_buffer *buffer)
{
	struct spa_meta_sync_timeline *sync;

	sync = spa_buffer_find_meta_data(buffer, SPA_META_SyncTimeline,
				 sizeof(*sync));
	if (!sync ||
	    sync->flags != SPA_META_SYNC_TIMELINE_UNSCHEDULED_RELEASE ||
	    sync->padding != 0 || !sync->acquire_point ||
	    sync->release_point != sync->acquire_point)
		consumer->sync_errors++;
}

static void on_process(void *data)
{
	struct consumer *consumer = data;
	struct pw_buffer *pipewire_buffer;
	struct spa_buffer *buffer;

	pipewire_buffer = pw_stream_dequeue_buffer(consumer->stream);
	if (!pipewire_buffer)
		return;
	buffer = pipewire_buffer->buffer;
	if (!buffer || buffer->n_datas != 3 ||
	    !valid_dma_buf(consumer, &buffer->datas[0]) ||
	    !valid_syncobj(&buffer->datas[1]) ||
	    !valid_syncobj(&buffer->datas[2])) {
		consumer->data_errors++;
	} else {
		validate_header(consumer, buffer);
		validate_damage(consumer, buffer);
		validate_sync_timeline(consumer, buffer);
	}

	consumer->frames_received++;
	if (pw_stream_queue_buffer(consumer->stream, pipewire_buffer) < 0) {
		consumer->data_errors++;
		pw_main_loop_quit(consumer->loop);
		return;
	}
	/* This test consumer has no clocked processing loop of its own. Ask the
	 * application-driven source to schedule the graph after returning the
	 * buffer, just as a real downstream driver would. */
	if (pw_stream_trigger_process(consumer->stream) < 0) {
		consumer->data_errors++;
		pw_main_loop_quit(consumer->loop);
		return;
	}
	if (consumer->frames_received >= consumer->target_frames &&
	    !consumer->target_reached)
		quit_after_final_release(consumer);
}

static const struct pw_stream_events stream_events = {
	PW_VERSION_STREAM_EVENTS,
	.state_changed = on_state_changed,
	.param_changed = on_param_changed,
	.process = on_process,
};

static void on_core_error(void *data, uint32_t id, int sequence, int result,
			  const char *message)
{
	struct consumer *consumer = data;

	(void)sequence;
	fprintf(stderr, "core error id=%u: %s (%s)\n", id, message,
		spa_strerror(result));
	if (id == PW_ID_CORE && result == -EPIPE)
		pw_main_loop_quit(consumer->loop);
}

static const struct pw_core_events core_events = {
	PW_VERSION_CORE_EVENTS,
	.error = on_core_error,
};

static void usage(const char *program)
{
	fprintf(stderr,
		"Usage: %s --node-name NAME [--frames COUNT] [--timeout SEC] [--wait-for-disconnect]\n",
		program);
}

int main(int argc, char *argv[])
{
	struct consumer state = {
		.target_frames = DEFAULT_FRAME_COUNT,
	};
	struct consumer *consumer = &state;
	uint8_t format_buffer[1024];
	struct spa_pod_builder builder;
	const struct spa_pod *params[1];
	struct pw_properties *properties;
	const char *node_name = NULL;
	struct timespec timeout = { .tv_sec = DEFAULT_TIMEOUT_SEC };
	int timeout_seconds = DEFAULT_TIMEOUT_SEC;
	bool passed;
	int option;
	int result = EXIT_FAILURE;
	static const struct option options[] = {
		{ "node-name", required_argument, NULL, 'n' },
		{ "frames", required_argument, NULL, 'f' },
		{ "timeout", required_argument, NULL, 't' },
		{ "wait-for-disconnect", no_argument, NULL, 'd' },
		{ "help", no_argument, NULL, 'h' },
		{ 0 },
	};

	while ((option = getopt_long(argc, argv, "n:f:t:dh", options, NULL)) != -1) {
		switch (option) {
		case 'n':
			node_name = optarg;
			break;
		case 'f':
			consumer->target_frames = atoi(optarg);
			break;
		case 't':
			timeout_seconds = atoi(optarg);
			break;
		case 'd':
			consumer->wait_for_disconnect = true;
			break;
		case 'h':
			usage(argv[0]);
			return EXIT_SUCCESS;
		default:
			usage(argv[0]);
			return EXIT_FAILURE;
		}
	}
	if (!node_name || !node_name[0] || consumer->target_frames <= 0 ||
	    timeout_seconds <= 0 || optind != argc) {
		usage(argv[0]);
		return EXIT_FAILURE;
	}
	timeout.tv_sec = timeout_seconds;

	pw_init(&argc, &argv);
	consumer->loop = pw_main_loop_new(NULL);
	if (!consumer->loop) {
		fprintf(stderr, "pw_main_loop_new failed\n");
		goto out_deinit;
	}
	consumer->context = pw_context_new(
		pw_main_loop_get_loop(consumer->loop), NULL, 0);
	if (!consumer->context) {
		fprintf(stderr, "pw_context_new failed\n");
		goto out_loop;
	}
	consumer->core = pw_context_connect(consumer->context, NULL, 0);
	if (!consumer->core) {
		fprintf(stderr, "pw_context_connect: %s\n", strerror(errno));
		goto out_context;
	}
	pw_core_add_listener(consumer->core, &consumer->core_listener,
			     &core_events, consumer);
	consumer->timer = pw_loop_add_timer(
		pw_main_loop_get_loop(consumer->loop), on_timeout, consumer);
	if (!consumer->timer) {
		fprintf(stderr, "pw_loop_add_timer failed\n");
		goto out_core;
	}
	pw_loop_update_timer(pw_main_loop_get_loop(consumer->loop),
			     consumer->timer, &timeout, NULL, false);

	properties = pw_properties_new(
		PW_KEY_MEDIA_TYPE, "Video",
		PW_KEY_MEDIA_CATEGORY, "Capture",
		PW_KEY_MEDIA_ROLE, "Screen",
		PW_KEY_TARGET_OBJECT, node_name,
		PW_KEY_NODE_DONT_RECONNECT, "true",
		"node.dont-fallback", "true",
		NULL);
	consumer->stream = pw_stream_new(consumer->core,
					 "pronk-pipewire-vm-consumer",
					 properties);
	if (!consumer->stream) {
		fprintf(stderr, "pw_stream_new failed\n");
		goto out_timer;
	}
	pw_stream_add_listener(consumer->stream, &consumer->stream_listener,
			       &stream_events, consumer);

	spa_pod_builder_init(&builder, format_buffer, sizeof(format_buffer));
	params[0] = spa_pod_builder_add_object(&builder,
		SPA_TYPE_OBJECT_Format, SPA_PARAM_EnumFormat,
		SPA_FORMAT_mediaType, SPA_POD_Id(SPA_MEDIA_TYPE_video),
		SPA_FORMAT_mediaSubtype, SPA_POD_Id(SPA_MEDIA_SUBTYPE_raw),
		SPA_FORMAT_VIDEO_format, SPA_POD_Id(SPA_VIDEO_FORMAT_BGRx),
		SPA_FORMAT_VIDEO_modifier, SPA_POD_Long(DRM_FORMAT_MOD_LINEAR),
		SPA_FORMAT_VIDEO_size,
			SPA_POD_CHOICE_RANGE_Rectangle(
				&SPA_RECTANGLE(1920, 1080),
				&SPA_RECTANGLE(1, 1),
				&SPA_RECTANGLE(MAX_FRAME_DIMENSION,
					       MAX_FRAME_DIMENSION)),
		SPA_FORMAT_VIDEO_framerate,
			SPA_POD_CHOICE_RANGE_Fraction(
				&SPA_FRACTION(60, 1),
				&SPA_FRACTION(1, 1),
				&SPA_FRACTION(240, 1)));
	if (pw_stream_connect(consumer->stream, PW_DIRECTION_INPUT, PW_ID_ANY,
			      PW_STREAM_FLAG_AUTOCONNECT |
			      PW_STREAM_FLAG_DONT_RECONNECT, params, 1) < 0) {
		fprintf(stderr, "pw_stream_connect failed\n");
		goto out_stream;
	}

	pw_main_loop_run(consumer->loop);
	passed = consumer->format_negotiated && consumer->streaming &&
		 consumer->target_reached && !consumer->timed_out &&
		 (!consumer->wait_for_disconnect ||
		  consumer->disconnected_after_target) &&
		 consumer->frames_received == consumer->target_frames &&
		 !consumer->sequence_errors && !consumer->timestamp_errors &&
		 !consumer->header_errors && !consumer->damage_errors &&
		 !consumer->sync_errors && !consumer->data_errors &&
		 !consumer->format_errors;
	printf("pw_connected=%d\n", consumer->streaming ? 1 : 0);
	printf("frames_received=%d\n", consumer->frames_received);
	printf("header_metadata=%d\n", consumer->header_errors == 0 ? 1 : 0);
	printf("damage_metadata=%d\n", consumer->damage_errors == 0 ? 1 : 0);
	printf("sync_timeline_metadata=%d\n",
	       consumer->sync_errors == 0 ? 1 : 0);
	printf("dma_buf_layout=%d\n", consumer->data_errors == 0 ? 1 : 0);
	printf("source_disconnected=%d\n",
	       consumer->disconnected_after_target ? 1 : 0);
	printf("pw_pronk_consumer=%s\n", passed ? "pass" : "fail");
	result = passed ? EXIT_SUCCESS : EXIT_FAILURE;

out_stream:
	pw_stream_destroy(consumer->stream);
out_timer:
	pw_loop_destroy_source(pw_main_loop_get_loop(consumer->loop),
			       consumer->timer);
out_core:
	pw_core_disconnect(consumer->core);
out_context:
	pw_context_destroy(consumer->context);
out_loop:
	pw_main_loop_destroy(consumer->loop);
out_deinit:
	pw_deinit();
	return result;
}
