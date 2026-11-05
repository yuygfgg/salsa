#ifndef SALSA_CARRIER_SHARED_H
#define SALSA_CARRIER_SHARED_H

#include <stdatomic.h>
#include <stdint.h>

#ifndef SALSA_CARRIER_SHM_NAME
#define SALSA_CARRIER_SHM_NAME "/salsa-hal-carrier"
#endif
#define SALSA_CARRIER_SHM_MAGIC 0x53414c53u
#define SALSA_CARRIER_SHM_VERSION 1u
#define SALSA_CARRIER_SLOT_COUNT 16u
#define SALSA_CARRIER_SLOT_BYTES (64u * 1024u)

enum {
    SALSA_CARRIER_FLAG_DISCONTINUITY = 1u << 0,
    SALSA_CARRIER_FLAG_DROPPED = 1u << 1,
};

typedef struct {
    uint64_t generation;
    uint64_t start_sample;
    uint64_t host_time;
    uint32_t frames;
    uint32_t channels;
    uint32_t bytes_per_frame;
    uint32_t payload_bytes;
    uint32_t flags;
    uint32_t reserved;
} SalsaCarrierSlotHeader;

typedef struct {
    SalsaCarrierSlotHeader header;
    uint8_t payload[SALSA_CARRIER_SLOT_BYTES];
} SalsaCarrierSlot;

typedef struct {
    _Atomic uint32_t magic;
    _Atomic uint32_t version;
    _Atomic uint64_t generation;
    _Atomic uint64_t declared_latency_frames;
    _Atomic uint32_t latency_valid;
    _Atomic uint32_t reserved0;
    _Atomic uint64_t buffer_frames;
    _Atomic uint64_t output_channels;
    _Atomic uint64_t bytes_per_frame;
    _Atomic uint64_t dropped_frames;
    _Atomic uint64_t head;
    _Atomic uint64_t tail;
    _Atomic uint64_t clock_sample;
    _Atomic uint64_t clock_host;
    _Atomic uint64_t clock_seed;
    _Atomic uint32_t clock_valid;
    _Atomic uint32_t clock_sequence;
    _Atomic uint64_t service_heartbeat;
    _Atomic uint64_t service_markers;
    _Atomic uint64_t rendered_markers;
    _Atomic uint64_t late_markers;
    uint8_t reserved[32];
} SalsaCarrierControl;

typedef struct {
    SalsaCarrierControl control;
    SalsaCarrierSlot slots[SALSA_CARRIER_SLOT_COUNT];
} SalsaCarrierShared;

static inline void SalsaCarrierInitialize(SalsaCarrierShared *shared)
{
    atomic_store(&shared->control.magic, SALSA_CARRIER_SHM_MAGIC);
    atomic_store(&shared->control.version, SALSA_CARRIER_SHM_VERSION);
    atomic_store(&shared->control.generation, 0);
    atomic_store(&shared->control.declared_latency_frames, 0);
    atomic_store(&shared->control.latency_valid, 0);
    atomic_store(&shared->control.buffer_frames, 0);
    atomic_store(&shared->control.output_channels, 0);
    atomic_store(&shared->control.bytes_per_frame, 0);
    atomic_store(&shared->control.dropped_frames, 0);
    atomic_store(&shared->control.head, 0);
    atomic_store(&shared->control.tail, 0);
    atomic_store(&shared->control.clock_sample, 0);
    atomic_store(&shared->control.clock_host, 0);
    atomic_store(&shared->control.clock_seed, 0);
    atomic_store(&shared->control.clock_valid, 0);
    atomic_store(&shared->control.clock_sequence, 0);
    atomic_store(&shared->control.service_heartbeat, 0);
    atomic_store(&shared->control.service_markers, 0);
    atomic_store(&shared->control.rendered_markers, 0);
    atomic_store(&shared->control.late_markers, 0);
}

static inline void SalsaCarrierPublishClock(SalsaCarrierShared *shared,
                                             uint64_t sample,
                                             uint64_t host,
                                             uint64_t seed)
{
    atomic_fetch_add_explicit(&shared->control.clock_sequence, 1, memory_order_acq_rel);
    atomic_store_explicit(&shared->control.clock_sample, sample, memory_order_relaxed);
    atomic_store_explicit(&shared->control.clock_host, host, memory_order_relaxed);
    atomic_store_explicit(&shared->control.clock_seed, seed, memory_order_relaxed);
    atomic_store_explicit(&shared->control.clock_valid, 1, memory_order_relaxed);
    atomic_fetch_add_explicit(&shared->control.clock_sequence, 1, memory_order_acq_rel);
}

static inline int SalsaCarrierReadClock(SalsaCarrierShared *shared,
                                        uint64_t *sample,
                                        uint64_t *host,
                                        uint64_t *seed)
{
    for (int attempt = 0; attempt < 4; ++attempt) {
        uint32_t seq1 = atomic_load_explicit(&shared->control.clock_sequence, memory_order_acquire);
        if (seq1 & 1u) {
            continue;
        }
        uint64_t s = atomic_load_explicit(&shared->control.clock_sample, memory_order_relaxed);
        uint64_t h = atomic_load_explicit(&shared->control.clock_host, memory_order_relaxed);
        uint64_t g = atomic_load_explicit(&shared->control.clock_seed, memory_order_relaxed);
        uint32_t valid = atomic_load_explicit(&shared->control.clock_valid, memory_order_relaxed);
        uint32_t seq2 = atomic_load_explicit(&shared->control.clock_sequence, memory_order_acquire);
        if (seq1 == seq2 && valid) {
            *sample = s;
            *host = h;
            *seed = g;
            return 1;
        }
    }
    return 0;
}

#endif
