#define _GNU_SOURCE

#include <alsa/asoundlib.h>
#include <alsa/pcm_external.h>

#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/timerfd.h>
#include <time.h>
#include <unistd.h>

#include "salsa_carrier_shared.h"

#define SALSA_DEFAULT_RATE 192000u
#define SALSA_POLL_RUNNING_NS 1000000L
#define SALSA_POLL_IDLE_NS 10000000L

typedef struct {
    snd_pcm_ioplug_t io;
    SalsaCarrierShared *shared;
    int shm_fd;
    char *shm_name;
    int timer_fd;
    unsigned int rate;
    unsigned int channels;
    unsigned int frame_bytes;
    uint64_t written_frames;
    uint64_t hw_base;
    uint64_t generation;
    struct timespec start_time;
    int running;
    int paused;
    uint32_t pending_flags;
} salsa_t;

static uint64_t salsa_now_ns(void)
{
    struct timespec now;
    clock_gettime(CLOCK_MONOTONIC, &now);
    return (uint64_t)now.tv_sec * 1000000000ull + (uint64_t)now.tv_nsec;
}

static uint64_t salsa_frames_since(const salsa_t *salsa, uint64_t now)
{
    uint64_t start = (uint64_t)salsa->start_time.tv_sec * 1000000000ull +
                     (uint64_t)salsa->start_time.tv_nsec;
    uint64_t elapsed = now >= start ? now - start : 0;
    return elapsed * salsa->rate / 1000000000ull;
}

static uint64_t salsa_consumed(salsa_t *salsa)
{
    if (!salsa->running)
        return salsa->hw_base;
    uint64_t now = salsa_now_ns();
    uint64_t elapsed = salsa_frames_since(salsa, now);
    uint64_t target = salsa->hw_base + elapsed;
    if (target > salsa->written_frames) {
        salsa->hw_base = salsa->written_frames;
        salsa->start_time.tv_sec = (time_t)(now / 1000000000ull);
        salsa->start_time.tv_nsec = (long)(now % 1000000000ull);
        target = salsa->written_frames;
    }
    return target;
}

static void salsa_arm_timer(salsa_t *salsa)
{
    struct itimerspec timer;
    memset(&timer, 0, sizeof(timer));
    timer.it_value.tv_nsec = salsa->running ? SALSA_POLL_RUNNING_NS : SALSA_POLL_IDLE_NS;
    if (timer.it_value.tv_nsec >= 1000000000L) {
        timer.it_value.tv_sec = timer.it_value.tv_nsec / 1000000000L;
        timer.it_value.tv_nsec %= 1000000000L;
    }
    timerfd_settime(salsa->timer_fd, 0, &timer, NULL);
}

static void salsa_publish(salsa_t *salsa, const uint8_t *data, size_t bytes, unsigned int frames)
{
    SalsaCarrierControl *control = &salsa->shared->control;
    uint64_t head = atomic_load_explicit(&control->head, memory_order_relaxed);
    uint64_t tail = atomic_load_explicit(&control->tail, memory_order_acquire);
    if (head - tail >= SALSA_CARRIER_SLOT_COUNT) {
        atomic_fetch_add_explicit(&control->dropped_frames, frames, memory_order_relaxed);
        salsa->pending_flags |= SALSA_CARRIER_FLAG_DROPPED;
        return;
    }

    SalsaCarrierSlot *slot = &salsa->shared->slots[head % SALSA_CARRIER_SLOT_COUNT];
    slot->header.generation = salsa->generation;
    slot->header.start_sample = salsa->written_frames;
    slot->header.host_time = salsa_now_ns();
    slot->header.frames = frames;
    slot->header.channels = salsa->channels;
    slot->header.bytes_per_frame = salsa->frame_bytes;
    slot->header.payload_bytes = (uint32_t)bytes;
    slot->header.flags = salsa->pending_flags;
    slot->header.reserved = 0;
    salsa->pending_flags = 0;
    memcpy(slot->payload, data, bytes);
    atomic_store_explicit(&control->head, head + 1, memory_order_release);
}

static void salsa_publish_pcm(salsa_t *salsa, const uint8_t *data, unsigned int frames)
{
    unsigned int max_frames = SALSA_CARRIER_SLOT_BYTES / salsa->frame_bytes;
    if (max_frames == 0)
        max_frames = 1;

    while (frames > 0) {
        unsigned int chunk = frames < max_frames ? frames : max_frames;
        salsa_publish(salsa, data, (size_t)chunk * salsa->frame_bytes, chunk);
        data += (size_t)chunk * salsa->frame_bytes;
        salsa->written_frames += chunk;
        frames -= chunk;
    }
}

static int salsa_start(snd_pcm_ioplug_t *io)
{
    salsa_t *salsa = io->private_data;
    salsa->hw_base = 0;
    clock_gettime(CLOCK_MONOTONIC, &salsa->start_time);
    salsa->running = 1;
    salsa->paused = 0;
    return 0;
}

static int salsa_stop(snd_pcm_ioplug_t *io)
{
    salsa_t *salsa = io->private_data;
    salsa->hw_base = salsa_consumed(salsa);
    salsa->running = 0;
    salsa->paused = 0;
    return 0;
}

static snd_pcm_sframes_t salsa_pointer(snd_pcm_ioplug_t *io)
{
    salsa_t *salsa = io->private_data;
    if (io->buffer_size == 0)
        return 0;
    return (snd_pcm_sframes_t)(salsa_consumed(salsa) % io->buffer_size);
}

static snd_pcm_sframes_t salsa_transfer(snd_pcm_ioplug_t *io,
                                        const snd_pcm_channel_area_t *areas,
                                        snd_pcm_uframes_t offset,
                                        snd_pcm_uframes_t size)
{
    salsa_t *salsa = io->private_data;
    size_t frame_bytes = salsa->frame_bytes;
    const uint8_t *data;

    if (size == 0)
        return 0;
    if (areas == NULL || areas[0].addr == NULL)
        return -EINVAL;
    if ((areas[0].step / 8) != frame_bytes)
        return -EINVAL;
    data = (const uint8_t *)areas[0].addr + areas[0].first / 8 + offset * frame_bytes;
    salsa_publish_pcm(salsa, data, (unsigned int)size);
    return (snd_pcm_sframes_t)size;
}

static int salsa_hw_params(snd_pcm_ioplug_t *io, snd_pcm_hw_params_t *params)
{
    salsa_t *salsa = io->private_data;
    (void)params;
    salsa->rate = io->rate;
    salsa->channels = io->channels;
    salsa->frame_bytes = io->channels * 2;
    return 0;
}

static int salsa_prepare(snd_pcm_ioplug_t *io)
{
    salsa_t *salsa = io->private_data;
    SalsaCarrierControl *control = &salsa->shared->control;

    salsa->written_frames = 0;
    salsa->hw_base = 0;
    salsa->running = 0;
    salsa->paused = 0;
    salsa->pending_flags = SALSA_CARRIER_FLAG_DISCONTINUITY;
    salsa->generation = atomic_fetch_add_explicit(&control->generation, 1, memory_order_acq_rel) + 1;
    atomic_store_explicit(&control->dropped_frames, 0, memory_order_release);
    atomic_store_explicit(&control->output_channels, salsa->channels, memory_order_release);
    atomic_store_explicit(&control->bytes_per_frame, salsa->frame_bytes, memory_order_release);
    atomic_store_explicit(&control->buffer_frames, io->buffer_size, memory_order_release);
    return 0;
}

static int salsa_pause(snd_pcm_ioplug_t *io, int enable)
{
    salsa_t *salsa = io->private_data;
    if (enable) {
        salsa->hw_base = salsa_consumed(salsa);
        salsa->running = 0;
        salsa->paused = 1;
    } else {
        clock_gettime(CLOCK_MONOTONIC, &salsa->start_time);
        salsa->running = 1;
        salsa->paused = 0;
    }
    return 0;
}

static int salsa_poll_descriptors_count(snd_pcm_ioplug_t *io)
{
    (void)io;
    return 1;
}

static int salsa_poll_descriptors(snd_pcm_ioplug_t *io, struct pollfd *pfds, unsigned int space)
{
    salsa_t *salsa = io->private_data;
    if (space < 1 || pfds == NULL)
        return 0;
    salsa_arm_timer(salsa);
    pfds[0].fd = salsa->timer_fd;
    pfds[0].events = POLLIN;
    return 1;
}

static int salsa_poll_revents(snd_pcm_ioplug_t *io, struct pollfd *pfds,
                              unsigned int nfds, unsigned short *revents)
{
    salsa_t *salsa = io->private_data;
    uint64_t expirations;
    (void)io;
    (void)pfds;
    if (nfds > 0) {
        while (read(salsa->timer_fd, &expirations, sizeof(expirations)) < 0 && errno == EINTR)
            ;
        *revents = POLLIN;
    } else {
        *revents = 0;
    }
    return 0;
}

static int salsa_close(snd_pcm_ioplug_t *io)
{
    salsa_t *salsa = io->private_data;
    if (salsa->shared != NULL)
        munmap(salsa->shared, sizeof(SalsaCarrierShared));
    if (salsa->shm_fd >= 0)
        close(salsa->shm_fd);
    if (salsa->timer_fd >= 0)
        close(salsa->timer_fd);
    free(salsa->shm_name);
    free(salsa);
    io->private_data = NULL;
    return 0;
}

static const snd_pcm_ioplug_callback_t salsa_callbacks = {
    .start = salsa_start,
    .stop = salsa_stop,
    .pointer = salsa_pointer,
    .transfer = salsa_transfer,
    .close = salsa_close,
    .hw_params = salsa_hw_params,
    .prepare = salsa_prepare,
    .pause = salsa_pause,
    .poll_descriptors_count = salsa_poll_descriptors_count,
    .poll_descriptors = salsa_poll_descriptors,
    .poll_revents = salsa_poll_revents,
};

static int salsa_open_shared(salsa_t *salsa, const char *name)
{
    int fd = shm_open(name, O_CREAT | O_RDWR, 0666);
    if (fd < 0)
        return -errno;

    struct stat status;
    if (fstat(fd, &status) != 0) {
        int err = -errno;
        close(fd);
        return err;
    }
    if ((size_t)status.st_size < sizeof(SalsaCarrierShared)) {
        if (ftruncate(fd, sizeof(SalsaCarrierShared)) != 0) {
            int err = -errno;
            close(fd);
            return err;
        }
    }

    void *map = mmap(NULL, sizeof(SalsaCarrierShared), PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (map == MAP_FAILED) {
        int err = -errno;
        close(fd);
        return err;
    }

    salsa->shared = map;
    salsa->shm_fd = fd;
    if (atomic_load(&salsa->shared->control.magic) != SALSA_CARRIER_SHM_MAGIC ||
        atomic_load(&salsa->shared->control.version) != SALSA_CARRIER_SHM_VERSION)
        SalsaCarrierInitialize(salsa->shared);
    return 0;
}

SND_PCM_PLUGIN_DEFINE_FUNC(salsa)
{
    snd_config_iterator_t i, next;
    const char *shm_name = NULL;
    long configured_channels = 0;
    long configured_rate = SALSA_DEFAULT_RATE;
    salsa_t *salsa;
    int err;

    if (stream != SND_PCM_STREAM_PLAYBACK)
        return -ENOTSUP;

    snd_config_for_each(i, next, conf) {
        snd_config_t *node = snd_config_iterator_entry(i);
        const char *id;
        if (snd_config_get_id(node, &id) < 0)
            continue;
        if (strcmp(id, "comment") == 0 || strcmp(id, "type") == 0 ||
            strcmp(id, "hint") == 0 || strcmp(id, "name") == 0)
            continue;
        if (strcmp(id, "shm") == 0 || strcmp(id, "shm_name") == 0) {
            if (snd_config_get_string(node, &shm_name) < 0)
                return -EINVAL;
            continue;
        }
        if (strcmp(id, "channels") == 0) {
            if (snd_config_get_integer(node, &configured_channels) < 0)
                return -EINVAL;
            continue;
        }
        if (strcmp(id, "rate") == 0) {
            if (snd_config_get_integer(node, &configured_rate) < 0)
                return -EINVAL;
            continue;
        }
        if (strcmp(id, "AES0") == 0 || strcmp(id, "AES1") == 0 ||
            strcmp(id, "AES2") == 0 || strcmp(id, "AES3") == 0)
            continue;
        SNDERR("unknown salsa PCM field '%s'", id);
        return -EINVAL;
    }

    if (shm_name == NULL)
        shm_name = SALSA_CARRIER_SHM_NAME;
    if (configured_rate != SALSA_DEFAULT_RATE) {
        SNDERR("salsa PCM requires %u Hz, got %ld", SALSA_DEFAULT_RATE, configured_rate);
        return -EINVAL;
    }
    if (configured_channels != 0 && configured_channels != 2 && configured_channels != 8) {
        SNDERR("salsa PCM supports 2 or 8 channels, got %ld", configured_channels);
        return -EINVAL;
    }

    salsa = calloc(1, sizeof(*salsa));
    if (salsa == NULL)
        return -ENOMEM;
    salsa->shm_fd = -1;
    salsa->timer_fd = -1;
    salsa->channels = configured_channels != 0 ? (unsigned int)configured_channels : 2;
    salsa->frame_bytes = salsa->channels * 2;
    salsa->rate = SALSA_DEFAULT_RATE;
    salsa->shm_name = strdup(shm_name);
    if (salsa->shm_name == NULL) {
        free(salsa);
        return -ENOMEM;
    }

    salsa->timer_fd = timerfd_create(CLOCK_MONOTONIC, TFD_CLOEXEC | TFD_NONBLOCK);
    if (salsa->timer_fd < 0) {
        err = -errno;
        goto error;
    }
    err = salsa_open_shared(salsa, shm_name);
    if (err < 0)
        goto error;

    salsa->io.version = SND_PCM_IOPLUG_VERSION;
    salsa->io.name = "Salsa IEC 61937 carrier";
    salsa->io.flags = SND_PCM_IOPLUG_FLAG_LISTED | SND_PCM_IOPLUG_FLAG_MONOTONIC;
    salsa->io.callback = &salsa_callbacks;
    salsa->io.private_data = salsa;
    err = snd_pcm_ioplug_create(&salsa->io, name, stream, mode);
    if (err < 0)
        goto error;

    static const unsigned int formats[] = { SND_PCM_FORMAT_S16_LE };
    static const unsigned int accesses[] = { SND_PCM_ACCESS_RW_INTERLEAVED };
    static const unsigned int rates[] = { SALSA_DEFAULT_RATE };
    static const unsigned int periods[] = { 2, 4, 8 };
    unsigned int channels[1];
    unsigned int channel_count = 1;

    err = snd_pcm_ioplug_set_param_list(&salsa->io, SND_PCM_IOPLUG_HW_FORMAT,
                                        sizeof(formats) / sizeof(formats[0]), formats);
    if (err < 0)
        goto error;
    err = snd_pcm_ioplug_set_param_list(&salsa->io, SND_PCM_IOPLUG_HW_ACCESS,
                                        sizeof(accesses) / sizeof(accesses[0]), accesses);
    if (err < 0)
        goto error;
    err = snd_pcm_ioplug_set_param_list(&salsa->io, SND_PCM_IOPLUG_HW_RATE,
                                        sizeof(rates) / sizeof(rates[0]), rates);
    if (err < 0)
        goto error;
    if (configured_channels != 0) {
        channels[0] = (unsigned int)configured_channels;
        err = snd_pcm_ioplug_set_param_list(&salsa->io, SND_PCM_IOPLUG_HW_CHANNELS,
                                            channel_count, channels);
    } else {
        err = snd_pcm_ioplug_set_param_minmax(&salsa->io, SND_PCM_IOPLUG_HW_CHANNELS, 2, 8);
    }
    if (err < 0)
        goto error;
    err = snd_pcm_ioplug_set_param_minmax(&salsa->io, SND_PCM_IOPLUG_HW_PERIOD_BYTES,
                                          4, SALSA_CARRIER_SLOT_BYTES);
    if (err < 0)
        goto error;
    err = snd_pcm_ioplug_set_param_minmax(&salsa->io, SND_PCM_IOPLUG_HW_BUFFER_BYTES,
                                          16, SALSA_CARRIER_SLOT_BYTES * 4);
    if (err < 0)
        goto error;
    err = snd_pcm_ioplug_set_param_list(&salsa->io, SND_PCM_IOPLUG_HW_PERIODS,
                                        sizeof(periods) / sizeof(periods[0]), periods);
    if (err < 0)
        goto error;

    *pcmp = salsa->io.pcm;
    return 0;

error:
    if (salsa->io.pcm != NULL)
        snd_pcm_ioplug_delete(&salsa->io);
    else {
        if (salsa->shared != NULL)
            munmap(salsa->shared, sizeof(SalsaCarrierShared));
        if (salsa->shm_fd >= 0)
            close(salsa->shm_fd);
        if (salsa->timer_fd >= 0)
            close(salsa->timer_fd);
        free(salsa->shm_name);
        free(salsa);
    }
    return err;
}

SND_PCM_PLUGIN_SYMBOL(salsa);
