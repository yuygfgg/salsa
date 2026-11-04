#ifndef SALSA_HAL_PROBE_H
#define SALSA_HAL_PROBE_H

#include <stdint.h>

enum { kSalsaHALProbeProperty = 'sdia', kSalsaHALRecordProperty = 'srec', kSalsaHALClockProperty = 'sclk' };

// Read-only diagnostic counters. The callback only updates atomics.
typedef struct {
    uint64_t starts;
    uint64_t stops;
    uint64_t timestamps;
    uint64_t writes;
    uint64_t writeFrames;
    uint64_t reads;
    uint64_t lastBufferFrames;
    uint64_t droppedFrames;
    uint64_t recordedBytes;
    uint64_t recordOverflow;
    uint64_t generation;
    int32_t transportStatus;
} SalsaHALProbe;

typedef struct {
    uint64_t sample;
    uint64_t host;
    uint64_t seed;
} SalsaHALClock;

#endif
