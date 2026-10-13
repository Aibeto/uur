#define _GNU_SOURCE
/*
 * uur-pw-capture — PipeWire ScreenCast consumer.
 *
 * Receives an inherited portal PipeWire fd (UUR_PW_FD) and the authorized
 * video node id (UUR_PW_NODE), streams the monitor as BGRx and publishes
 * frames into the private UUR_FRAME_PATH following the seqlock protocol in
 * docs/capture-protocol.md.
 *
 * The shared file is created only after the stream format is negotiated,
 * because its size depends on the negotiated stride.  The uur PE hook maps
 * it read-only through Wine's Z: drive whenever the client captures.
 */

#include <locale.h>
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <unistd.h>

#include <pipewire/pipewire.h>
#include <spa/param/video/format-utils.h>
#include <drm/drm_fourcc.h>
#include "../hook/frame-protocol.h"

#define FRAME_FORMAT_BGRX 1

struct frame_header {
    uint32_t magic;
    uint32_t version;
    uint32_t width;
    uint32_t height;
    uint32_t stride;
    uint32_t format;
    _Atomic uint64_t total_frames;
    uint8_t reserved[32];
};

_Static_assert(sizeof(struct frame_header) == UURF_HEADER_BYTES,
               "frame protocol header size changed");

struct frame_slot {
    _Atomic uint64_t seq;
    /* frame bytes follow */
};

struct capture {
    struct pw_main_loop *loop;
    struct pw_stream *stream;
    struct spa_hook listener;
    uint32_t width;
    uint32_t height;
    uint32_t stride;
    enum spa_video_format format;
    uint64_t counter;
    int shm_fd;
    uint8_t *shm;
    size_t shm_size;
    int dmabuf_usable;
    char frame_path[PATH_MAX];
};

static struct capture *active_capture;

static void stop_capture(int signal_number)
{
    (void)signal_number;
    if (active_capture != NULL && active_capture->loop != NULL)
        pw_main_loop_quit(active_capture->loop);
}

static struct frame_slot *slot_at(struct capture *c, uint64_t n)
{
    size_t slot_bytes = sizeof(struct frame_slot) +
                        (size_t)c->stride * c->height;
    uint8_t *base = c->shm + uurf_slot_offset(n, slot_bytes);
    return (struct frame_slot *)base;
}

static uint8_t *slot_frame(struct frame_slot *slot)
{
    return (uint8_t *)slot + sizeof(struct frame_slot);
}

/* Create (or resize) the shared file for the negotiated geometry and write
 * the header.  Called once, from param_changed, before any frame exists. */
static int shm_setup(struct capture *c)
{
    size_t slot_bytes = sizeof(struct frame_slot) +
                        (size_t)c->stride * c->height;
    size_t total = UURF_HEADER_BYTES + UURF_SLOT_COUNT * slot_bytes;

    if (c->stride == 0 || c->height == 0 ||
        (size_t)c->stride >
            (SIZE_MAX - UURF_HEADER_BYTES) / c->height / UURF_SLOT_COUNT) {
        fprintf(stderr, "invalid capture geometry\n");
        return -1;
    }

    c->shm_fd = open(c->frame_path, O_RDWR | O_CREAT | O_TRUNC | O_CLOEXEC,
                     0600);
    if (c->shm_fd < 0) {
        perror("open frame transport");
        return -1;
    }
    if (ftruncate(c->shm_fd, (off_t)total) < 0) {
        perror("ftruncate");
        return -1;
    }
    c->shm = mmap(NULL, total, PROT_READ | PROT_WRITE, MAP_SHARED,
                  c->shm_fd, 0);
    if (c->shm == MAP_FAILED) {
        perror("mmap");
        c->shm = NULL;
        return -1;
    }
    c->shm_size = total;

    struct frame_header *header = (struct frame_header *)c->shm;
    header->magic = 0x46525555u; /* "UURF" */
    header->version = UURF_VERSION;
    header->width = c->width;
    header->height = c->height;
    header->stride = c->stride;
    header->format = FRAME_FORMAT_BGRX;
    atomic_init(&header->total_frames, 0);
    for (uint32_t i = 0; i < UURF_SLOT_COUNT; ++i) {
        atomic_init(&slot_at(c, (uint64_t)i)->seq, 0);
    }
    return 0;
}

static void publish_frame(struct capture *c, const uint8_t *pixels,
                          uint32_t src_stride, uint32_t src_height)
{
    if (c->shm == NULL)
        return;

    c->counter += 1;
    struct frame_slot *slot = slot_at(c, c->counter);
    atomic_store_explicit(&slot->seq, 2 * c->counter - 1,
                          memory_order_release); /* writing */

    uint8_t *dst = slot_frame(slot);
    uint32_t rows = src_height < c->height ? src_height : c->height;
    for (uint32_t row = 0; row < rows; ++row) {
        uint8_t *target = dst + (size_t)row * c->stride;
        const uint8_t *source = pixels + (size_t)row * src_stride;
        if (c->format == SPA_VIDEO_FORMAT_BGRx ||
            c->format == SPA_VIDEO_FORMAT_BGRA) {
            memcpy(target, source, c->stride);
        } else if (c->format == SPA_VIDEO_FORMAT_RGBx ||
                   c->format == SPA_VIDEO_FORMAT_RGBA) {
            for (uint32_t column = 0; column < c->width; ++column) {
                target[column * 4 + 0] = source[column * 4 + 2];
                target[column * 4 + 1] = source[column * 4 + 1];
                target[column * 4 + 2] = source[column * 4 + 0];
                target[column * 4 + 3] = 0;
            }
        } else {
            int rgb = c->format == SPA_VIDEO_FORMAT_RGB;
            for (uint32_t column = 0; column < c->width; ++column) {
                target[column * 4 + 0] = source[column * 3 + (rgb ? 2 : 0)];
                target[column * 4 + 1] = source[column * 3 + 1];
                target[column * 4 + 2] = source[column * 3 + (rgb ? 0 : 2)];
                target[column * 4 + 3] = 0;
            }
        }
    }

    atomic_store_explicit(&slot->seq, 2 * c->counter, memory_order_release);
    struct frame_header *header = (struct frame_header *)c->shm;
    atomic_store_explicit(&header->total_frames, c->counter,
                          memory_order_release);
}

static void on_process(void *userdata)
{
    struct capture *c = userdata;
    struct pw_buffer *buffer = pw_stream_dequeue_buffer(c->stream);
    if (buffer == NULL)
        return;
    struct spa_buffer *buf = buffer->buffer;

    if (buf->n_datas >= 1 && c->shm != NULL) {
        struct spa_data *plane = &buf->datas[0];
        uint8_t *pixels = NULL;
        void *mapping = MAP_FAILED;
        size_t mapping_size = 0;

        if (plane->chunk == NULL) {
            pw_stream_queue_buffer(c->stream, buffer);
            return;
        }
        /* Bytes the frame occupies at the negotiated geometry.  DMA-BUF
         * producers leave both chunk->size and maxsize at 1, so the extent
         * has to come from the negotiated stride and height. */
        size_t frame_bytes = (size_t)c->stride * c->height;

        if (plane->data != NULL) {
            pixels = plane->data + plane->chunk->offset;
        } else if (plane->type == SPA_DATA_DmaBuf && plane->fd >= 0) {
            /* niri always delivers DMA-BUF here.  The session negotiated
             * DRM_FORMAT_MOD_LINEAR, so the fd is directly mappable without
             * GBM/EGL; a tiled modifier would need an importer instead. */
            if (!c->dmabuf_usable) {
                pw_stream_queue_buffer(c->stream, buffer);
                return;
            }
            size_t want = plane->maxsize > frame_bytes ? plane->maxsize
                                                       : frame_bytes;
            mapping = mmap(NULL, want, PROT_READ, MAP_SHARED, plane->fd, 0);
            if (mapping == MAP_FAILED) {
                static int once = 0;
                if (!once++)
                    fprintf(stderr, "dmabuf mmap failed: %s (fd=%d size=%zu)\n",
                            strerror(errno), (int)plane->fd, want);
            } else {
                mapping_size = want;
                pixels = (uint8_t *)mapping + plane->chunk->offset;
            }
        } else if (plane->type == SPA_DATA_MemFd && plane->fd >= 0) {
            long page_size = sysconf(_SC_PAGESIZE);
            uint64_t offset = (uint64_t)plane->mapoffset + plane->chunk->offset;
            uint64_t page_mask = (uint64_t)(page_size > 0 ? page_size : 4096) - 1;
            off_t aligned = (off_t)(offset & ~page_mask);
            size_t delta = (size_t)(offset - (uint64_t)aligned);
            mapping_size = delta + frame_bytes;
            mapping = mmap(NULL, mapping_size, PROT_READ, MAP_PRIVATE,
                           plane->fd, aligned);
            if (mapping != MAP_FAILED)
                pixels = (uint8_t *)mapping + delta;
        }
        if (pixels != NULL) {
            int32_t signed_stride = plane->chunk->stride;
            uint32_t stride = signed_stride > 0
                                  ? (uint32_t)signed_stride
                                  : (c->format == SPA_VIDEO_FORMAT_BGR ||
                                             c->format == SPA_VIDEO_FORMAT_RGB
                                         ? c->width * 3
                                         : c->width * 4);
            uint32_t rows = c->height;
            if (stride > 0) {
                size_t span = plane->chunk->size > stride ? plane->chunk->size
                                                          : frame_bytes;
                uint32_t from_span = (uint32_t)(span / stride);
                if (from_span < rows)
                    rows = from_span;
            }
            publish_frame(c, pixels, stride, rows);
            if (mapping != MAP_FAILED)
                munmap(mapping, mapping_size);
        }
    }

    pw_stream_queue_buffer(c->stream, buffer);
}

static void on_param_changed(void *userdata, uint32_t id,
                             const struct spa_pod *param)
{
    struct capture *c = userdata;
    if (param == NULL || id != SPA_PARAM_Format)
        return;

    struct spa_video_info_raw info;
    if (spa_format_video_raw_parse(param, &info) < 0)
        return;
    if (info.format != SPA_VIDEO_FORMAT_BGRx &&
        info.format != SPA_VIDEO_FORMAT_BGRA &&
        info.format != SPA_VIDEO_FORMAT_RGBx &&
        info.format != SPA_VIDEO_FORMAT_RGBA &&
        info.format != SPA_VIDEO_FORMAT_BGR &&
        info.format != SPA_VIDEO_FORMAT_RGB) {
        fprintf(stderr, "negotiated unexpected format %d\n", info.format);
        return;
    }

    c->width = (uint32_t)info.size.width;
    c->height = (uint32_t)info.size.height;
    c->format = info.format;
    c->stride = SPA_ROUND_UP_N(c->width * 4, 4);
    /* The DMA-BUF path mmaps the fd directly, which is only correct for a
     * linear layout.  A tiled modifier needs a real importer (GBM/EGL), so
     * refuse to publish garbage rather than emit a corrupt frame. */
    c->dmabuf_usable = info.modifier == DRM_FORMAT_MOD_LINEAR ||
                       info.modifier == DRM_FORMAT_MOD_INVALID;
    fprintf(stderr, "negotiated: %ux%u stride %u modifier %#llx%s\n", c->width,
            c->height, c->stride, (unsigned long long)info.modifier,
            c->dmabuf_usable ? "" : " (not linear; capture cannot map it)");

    if (c->shm == NULL && shm_setup(c) < 0) {
        pw_main_loop_quit(c->loop);
    }
}

static void on_state_changed(void *userdata, enum pw_stream_state old,
                             enum pw_stream_state state, const char *error)
{
    (void)userdata;
    (void)old;
    fprintf(stderr, "stream state: %s%s%s\n",
            pw_stream_state_as_string(state),
            error != NULL && error[0] != '\0' ? " - " : "",
            error != NULL ? error : "");
}

static const struct pw_stream_events stream_events = {
    .version = PW_VERSION_STREAM_EVENTS,
    .process = on_process,
    .param_changed = on_param_changed,
    .state_changed = on_state_changed,
};

int main(void)
{
    const char *fd_text = getenv("UUR_PW_FD");
    const char *node_text = getenv("UUR_PW_NODE");
    if (fd_text == NULL || node_text == NULL) {
        fprintf(stderr,
                "UUR_PW_FD and UUR_PW_NODE must be set by 'uur capture'\n");
        return 2;
    }
    const char *frame_path = getenv("UUR_FRAME_PATH");
    if (frame_path == NULL || frame_path[0] != '/' ||
        strlen(frame_path) >= PATH_MAX) {
        fprintf(stderr, "UUR_FRAME_PATH must be a short absolute path\n");
        return 2;
    }
    int pw_fd = atoi(fd_text);
    uint32_t node_id = (uint32_t)strtoul(node_text, NULL, 10);

    pw_init(NULL, NULL);

    struct capture c;
    memset(&c, 0, sizeof(c));
    c.shm_fd = -1;
    memcpy(c.frame_path, frame_path, strlen(frame_path) + 1);
    active_capture = &c;
    signal(SIGINT, stop_capture);
    signal(SIGTERM, stop_capture);
    signal(SIGHUP, stop_capture);

    struct pw_main_loop *loop = pw_main_loop_new(NULL);
    if (loop == NULL) {
        fprintf(stderr, "pw_main_loop_new failed\n");
        return 1;
    }
    c.loop = loop;

    struct pw_context *context = pw_context_new(pw_main_loop_get_loop(loop),
                                                 NULL, 0);
    if (context == NULL) {
        fprintf(stderr, "pw_context_new failed\n");
        return 1;
    }

    /* Connect through the portal's private PipeWire instance: only the
     * consented node is reachable on this descriptor. */
    struct pw_core *core = pw_context_connect_fd(context, pw_fd, NULL, 0);
    if (core == NULL) {
        fprintf(stderr, "pw_context_connect_fd failed\n");
        return 1;
    }

    struct pw_properties *props = pw_properties_new(
        PW_KEY_MEDIA_TYPE, "Video", PW_KEY_MEDIA_CATEGORY, "Capture",
        PW_KEY_MEDIA_ROLE, "Screen", NULL);
    c.stream = pw_stream_new(core, "uur-capture", props);
    if (c.stream == NULL) {
        fprintf(stderr, "pw_stream_new failed\n");
        return 1;
    }
    pw_stream_add_listener(c.stream, &c.listener, &stream_events, &c);

    uint8_t pod_buffer[1024];
    struct spa_pod_builder pod_builder =
        SPA_POD_BUILDER_INIT(pod_buffer, sizeof(pod_buffer));
    const struct spa_pod *params[1];
    params[0] = spa_pod_builder_add_object(
        &pod_builder, SPA_TYPE_OBJECT_Format, SPA_PARAM_EnumFormat,
        SPA_FORMAT_mediaType, SPA_POD_Id(SPA_MEDIA_TYPE_video),
        SPA_FORMAT_mediaSubtype, SPA_POD_Id(SPA_MEDIA_SUBTYPE_raw),
        SPA_FORMAT_VIDEO_format,
        SPA_POD_CHOICE_ENUM_Id(6, SPA_VIDEO_FORMAT_BGRx,
                               SPA_VIDEO_FORMAT_BGRA,
                               SPA_VIDEO_FORMAT_RGBx,
                               SPA_VIDEO_FORMAT_RGBA,
                               SPA_VIDEO_FORMAT_BGR,
                               SPA_VIDEO_FORMAT_RGB),
        /* Some compositors (niri) mark the modifier MANDATORY, and SPA
         * filtering fails unless both sides carry the property -- which
         * surfaces as "no more input formats".  Offer LINEAR, the layout the
         * frame protocol assumes. */
        SPA_FORMAT_VIDEO_modifier,
        SPA_POD_CHOICE_ENUM_Long(2, DRM_FORMAT_MOD_LINEAR,
                                 DRM_FORMAT_MOD_INVALID));

    if (pw_stream_connect(c.stream, PW_DIRECTION_INPUT, node_id,
                          PW_STREAM_FLAG_MAP_BUFFERS |
                              PW_STREAM_FLAG_AUTOCONNECT |
                              PW_STREAM_FLAG_RT_PROCESS,
                          params, 1) < 0) {
        fprintf(stderr, "pw_stream_connect failed\n");
        return 1;
    }

    pw_main_loop_run(loop);

    active_capture = NULL;
    pw_stream_destroy(c.stream);
    pw_context_destroy(context);
    pw_main_loop_destroy(loop);
    if (c.shm != NULL)
        munmap(c.shm, c.shm_size);
    if (c.shm_fd >= 0)
        close(c.shm_fd);
    unlink(c.frame_path);
    pw_deinit();
    return 0;
}
