/* SPDX-License-Identifier: MIT */
/* Ordinary KMS setup only; the Rust client performs every capture operation. */
#include <drm_fourcc.h>
#include <errno.h>
#include <fcntl.h>
#include <linux/dma-buf.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <unistd.h>
#include <xf86drm.h>
#include <xf86drmMode.h>

#define REQUIRE(condition) do { \
	if (!(condition)) { \
		fprintf(stderr, "fixture.c:%d: %s: errno=%d\n", __LINE__, #condition, errno); \
		exit(1); \
	} \
} while (0)

struct image {
	struct drm_mode_create_dumb allocation;
	uint32_t framebuffer;
	int dma;
};

struct fixture {
	int device;
	uint32_t crtc, connector, plane;
	drmModeModeInfo mode;
	struct image source, changed, destination;
};

struct fixture_info {
	int32_t device, source, destination;
	uint32_t crtc, connector, stride, width, height;
	uint64_t size;
};

static struct image image_create(int device, unsigned char fill)
{
	struct image image = { .allocation = { .width = 640, .height = 480, .bpp = 32 } };
	struct drm_mode_map_dumb mapping = {};
	uint32_t handles[4] = {}, strides[4] = {}, offsets[4] = {};
	void *pixels;

	REQUIRE(drmIoctl(device, DRM_IOCTL_MODE_CREATE_DUMB, &image.allocation) == 0);
	mapping.handle = image.allocation.handle;
	REQUIRE(drmIoctl(device, DRM_IOCTL_MODE_MAP_DUMB, &mapping) == 0);
	pixels = mmap(NULL, image.allocation.size, PROT_READ | PROT_WRITE,
		      MAP_SHARED, device, mapping.offset);
	REQUIRE(pixels != MAP_FAILED);
	memset(pixels, fill, image.allocation.size);
	REQUIRE(munmap(pixels, image.allocation.size) == 0);
	handles[0] = image.allocation.handle;
	strides[0] = image.allocation.pitch;
	REQUIRE(drmModeAddFB2(device, 640, 480, DRM_FORMAT_XRGB8888,
			    handles, strides, offsets, &image.framebuffer, 0) == 0);
	REQUIRE(drmPrimeHandleToFD(device, handles[0], DRM_CLOEXEC | DRM_RDWR, &image.dma) == 0);
	return image;
}

static void image_destroy(int device, struct image *image)
{
	struct drm_mode_destroy_dumb destroy = { .handle = image->allocation.handle };

	REQUIRE(close(image->dma) == 0);
	REQUIRE(drmModeRmFB(device, image->framebuffer) == 0);
	REQUIRE(drmIoctl(device, DRM_IOCTL_MODE_DESTROY_DUMB, &destroy) == 0);
}

static int select_output(struct fixture *fixture, const drmModeRes *resources)
{
	for (int i = 0; i < resources->count_connectors; i++) {
		drmModeConnector *connector = drmModeGetConnector(fixture->device, resources->connectors[i]);
		const drmModeModeInfo *mode = NULL;
		int crtc_index = -1;

		REQUIRE(connector);
		if (connector->connection == DRM_MODE_CONNECTED) {
			for (int j = 0; j < connector->count_modes; j++) {
				if (connector->modes[j].hdisplay == 640 && connector->modes[j].vdisplay == 480) {
					mode = &connector->modes[j];
					break;
				}
			}
		}
		for (int j = 0; mode && crtc_index < 0 && j < connector->count_encoders; j++) {
			drmModeEncoder *encoder = drmModeGetEncoder(fixture->device, connector->encoders[j]);

			REQUIRE(encoder);
			for (int k = 0; k < resources->count_crtcs; k++) {
				if (encoder->possible_crtcs & (1U << k)) {
					crtc_index = k;
					break;
				}
			}
			drmModeFreeEncoder(encoder);
		}
		if (crtc_index >= 0) {
			fixture->mode = *mode;
			fixture->connector = connector->connector_id;
			fixture->crtc = resources->crtcs[crtc_index];
		}
		drmModeFreeConnector(connector);
		if (crtc_index >= 0)
			return crtc_index;
	}
	return -1;
}

static bool plane_is_primary(int device, uint32_t plane)
{
	drmModeObjectProperties *properties = drmModeObjectGetProperties(device, plane, DRM_MODE_OBJECT_PLANE);
	bool primary = false;

	REQUIRE(properties);
	for (uint32_t i = 0; i < properties->count_props; i++) {
		drmModePropertyRes *property = drmModeGetProperty(device, properties->props[i]);

		REQUIRE(property);
		if (!strcmp(property->name, "type"))
			primary = properties->prop_values[i] == DRM_PLANE_TYPE_PRIMARY;
		drmModeFreeProperty(property);
	}
	drmModeFreeObjectProperties(properties);
	return primary;
}

static uint32_t select_primary_plane(int device, int crtc_index)
{
	drmModePlaneRes *planes = drmModeGetPlaneResources(device);
	uint32_t selected = 0;

	REQUIRE(planes);
	for (uint32_t i = 0; i < planes->count_planes; i++) {
		drmModePlane *plane = drmModeGetPlane(device, planes->planes[i]);

		REQUIRE(plane);
		if ((plane->possible_crtcs & (1U << crtc_index)) && plane_is_primary(device, plane->plane_id)) {
			REQUIRE(!selected);
			selected = plane->plane_id;
		}
		drmModeFreePlane(plane);
	}
	drmModeFreePlaneResources(planes);
	REQUIRE(selected);
	return selected;
}

struct fixture *capture_fixture_open(const char *path)
{
	struct fixture *fixture = calloc(1, sizeof(*fixture));
	drmVersion *version;
	drmModeRes *resources;
	int crtc_index;

	REQUIRE(fixture);
	fixture->device = open(path, O_RDWR | O_CLOEXEC);
	REQUIRE(fixture->device >= 0);
	version = drmGetVersion(fixture->device);
	REQUIRE(version && !strcmp(version->name, "castkms"));
	REQUIRE(version->version_major == 0 && version->version_minor == 0);
	drmFreeVersion(version);
	REQUIRE(drmIsMaster(fixture->device));
	REQUIRE(drmSetClientCap(fixture->device, DRM_CLIENT_CAP_ATOMIC, 1) == 0);
	resources = drmModeGetResources(fixture->device);
	REQUIRE(resources && resources->count_crtcs > 0 && resources->count_crtcs <= 32);
	crtc_index = select_output(fixture, resources);
	REQUIRE(crtc_index >= 0);
	drmModeFreeResources(resources);
	fixture->plane = select_primary_plane(fixture->device, crtc_index);
	REQUIRE(fixture->mode.clock);
	fixture->source = image_create(fixture->device, 0x49);
	fixture->changed = image_create(fixture->device, 0x68);
	fixture->destination = image_create(fixture->device, 0x33);
	REQUIRE(drmModeSetCrtc(fixture->device, fixture->crtc, fixture->source.framebuffer,
			     0, 0, &fixture->connector, 1, &fixture->mode) == 0);
	return fixture;
}

struct fixture_info capture_fixture_info(const struct fixture *fixture)
{
	return (struct fixture_info) {
		.device = fixture->device, .source = fixture->source.dma,
		.destination = fixture->destination.dma, .crtc = fixture->crtc,
		.connector = fixture->connector, .stride = fixture->destination.allocation.pitch,
		.width = 640, .height = 480, .size = fixture->destination.allocation.size,
	};
}

static void select_framebuffer(const struct fixture *fixture, uint32_t framebuffer)
{
	drmModeObjectProperties *properties;
	drmModeAtomicReq *update = drmModeAtomicAlloc();
	uint32_t framebuffer_property = 0;

	REQUIRE(update);
	properties = drmModeObjectGetProperties(fixture->device, fixture->plane, DRM_MODE_OBJECT_PLANE);
	REQUIRE(properties);
	for (uint32_t i = 0; i < properties->count_props; i++) {
		drmModePropertyRes *property = drmModeGetProperty(fixture->device, properties->props[i]);

		REQUIRE(property);
		if (!strcmp(property->name, "FB_ID"))
			framebuffer_property = property->prop_id;
		drmModeFreeProperty(property);
	}
	drmModeFreeObjectProperties(properties);
	REQUIRE(framebuffer_property);
	REQUIRE(drmModeAtomicAddProperty(update, fixture->plane, framebuffer_property,
				       framebuffer) >= 0);
	REQUIRE(drmModeAtomicCommit(fixture->device, update, 0, NULL) == 0);
	drmModeAtomicFree(update);
}

void capture_fixture_flip(const struct fixture *fixture)
{
	select_framebuffer(fixture, fixture->changed.framebuffer);
}

void capture_fixture_restore(const struct fixture *fixture)
{
	select_framebuffer(fixture, fixture->source.framebuffer);
}

void capture_buffer_check_pixels(int dma, uint32_t width, uint32_t height,
				uint32_t stride, unsigned char expected)
{
	struct dma_buf_sync sync = { .flags = DMA_BUF_SYNC_START | DMA_BUF_SYNC_READ };
	const unsigned char *pixels;
	size_t size = (size_t)stride * height;

	REQUIRE(width <= UINT32_MAX / 4 && stride >= width * 4 && size);
	pixels = mmap(NULL, size, PROT_READ, MAP_SHARED, dma, 0);
	REQUIRE(pixels != MAP_FAILED);
	REQUIRE(ioctl(dma, DMA_BUF_IOCTL_SYNC, &sync) == 0);
	for (unsigned int y = 0; y < height; y++) {
		for (unsigned int x = 0; x < width * 4; x++)
			REQUIRE(pixels[y * stride + x] == (x % 4 == 3 ? 0xff : expected));
	}
	sync.flags = DMA_BUF_SYNC_END | DMA_BUF_SYNC_READ;
	REQUIRE(ioctl(dma, DMA_BUF_IOCTL_SYNC, &sync) == 0);
	REQUIRE(munmap((void *)pixels, size) == 0);
}

void capture_fixture_check_pixels(const struct fixture *fixture, unsigned char expected)
{
	capture_buffer_check_pixels(fixture->destination.dma, 640, 480,
				    fixture->destination.allocation.pitch, expected);
}

void capture_fixture_close(struct fixture *fixture)
{
	REQUIRE(drmModeSetCrtc(fixture->device, fixture->crtc, 0, 0, 0, NULL, 0, NULL) == 0);
	image_destroy(fixture->device, &fixture->source);
	image_destroy(fixture->device, &fixture->changed);
	image_destroy(fixture->device, &fixture->destination);
	REQUIRE(close(fixture->device) == 0);
	free(fixture);
}
