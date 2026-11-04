/*
 * Salsa timing service prototype.
 *
 * Reads carrier blocks from /salsa-hal-carrier, schedules a click pair tied to
 * the first IEC 61937 burst, and renders those clicks to a selected CoreAudio
 * output device. It also publishes the physical output clock mapping for the
 * HAL's GetZeroTimeStamp().
 */

#include <CoreAudio/CoreAudio.h>
#include <CoreFoundation/CoreFoundation.h>
#include <mach/mach_time.h>
#include <pthread.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <fcntl.h>
#include <unistd.h>
#include <errno.h>
#include <math.h>
#include "../src/SalsaCarrierShared.h"

#define CARRIER_RATE 192000.0
#define OUTPUT_RATE 48000.0
#define OUTPUT_CHANNELS 2
#define SCHEDULING_ALLOWANCE_NS (200ull * 1000ull * 1000ull)
#define DRIFT_SYNC_INTERVAL 50u
#define MAX_MARKERS 8192u
#define CLICK_MS 2.0
#define CLICK_HZ 1000.0

typedef struct {
    uint64_t host_ns;
    uint64_t carrier_sample;
    uint32_t kind;
    uint32_t reserved;
} Marker;

static volatile sig_atomic_t gStop;
static SalsaCarrierShared *gShared;
static uint64_t gLatencyFrames;
static uint64_t gClockOffsetFrames;
static uint64_t gLatencyNs;
static AudioDeviceID gOutputDevice = kAudioObjectUnknown;
static AudioStreamID gOutputStream = kAudioObjectUnknown;
static double gOutputRate = OUTPUT_RATE;
static UInt32 gOutputChannels = OUTPUT_CHANNELS;
static mach_timebase_info_data_t gTimebase;
static Marker gMarkers[MAX_MARKERS];
static _Atomic uint64_t gMarkerHead;
static _Atomic uint64_t gMarkerTail;
static _Atomic uint64_t gRenderedMarkers;
static _Atomic uint64_t gLateMarkers;
static _Atomic uint64_t gCarrierAnchorSample;
static _Atomic double gPhysicalAnchorSample;
static _Atomic uint64_t gAnchorSeed;
static _Atomic int gHaveAnchor;
static FILE *gMarkerLog;
static pthread_mutex_t gLogLock = PTHREAD_MUTEX_INITIALIZER;

static void onSignal(int sig)
{
    (void)sig;
    gStop = 1;
}

static uint64_t hostToNs(uint64_t host)
{
    return (uint64_t)((double)host * (double)gTimebase.numer / (double)gTimebase.denom);
}

static OSStatus findDevice(const char *uid, AudioDeviceID *device)
{
    CFStringRef cfuid = CFStringCreateWithCString(NULL, uid, kCFStringEncodingUTF8);
    if(cfuid == NULL)
    {
        return kAudioHardwareBadObjectError;
    }
    AudioValueTranslation translation = {
        .mInputData = &cfuid,
        .mInputDataSize = sizeof(cfuid),
        .mOutputData = device,
        .mOutputDataSize = sizeof(*device),
    };
    UInt32 size = sizeof(translation);
    AudioObjectPropertyAddress address = {
        .mSelector = kAudioHardwarePropertyDeviceForUID,
        .mScope = kAudioObjectPropertyScopeGlobal,
        .mElement = kAudioObjectPropertyElementMain,
    };
    OSStatus status = AudioObjectGetPropertyData(kAudioObjectSystemObject, &address, 0, NULL, &size, &translation);
    CFRelease(cfuid);
    return status;
}

static AudioStreamID firstStream(AudioDeviceID device, AudioObjectPropertyScope scope)
{
    AudioObjectPropertyAddress address = {
        .mSelector = kAudioDevicePropertyStreams,
        .mScope = scope,
        .mElement = kAudioObjectPropertyElementMain,
    };
    UInt32 size = 0;
    if(AudioObjectGetPropertyDataSize(device, &address, 0, NULL, &size) != noErr || size < sizeof(AudioStreamID))
    {
        return kAudioObjectUnknown;
    }
    AudioStreamID *streams = (AudioStreamID *)calloc(1, size);
    if(streams == NULL)
    {
        return kAudioObjectUnknown;
    }
    OSStatus status = AudioObjectGetPropertyData(device, &address, 0, NULL, &size, streams);
    AudioStreamID result = (status == noErr) ? streams[0] : kAudioObjectUnknown;
    free(streams);
    return result;
}

static AudioStreamBasicDescription makeOutputFormat(void)
{
    AudioStreamBasicDescription format = {0};
    format.mSampleRate = gOutputRate;
    format.mFormatID = kAudioFormatLinearPCM;
    format.mFormatFlags = kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked | kAudioFormatFlagsNativeEndian;
    format.mBytesPerPacket = gOutputChannels * sizeof(float);
    format.mFramesPerPacket = 1;
    format.mBytesPerFrame = format.mBytesPerPacket;
    format.mChannelsPerFrame = gOutputChannels;
    format.mBitsPerChannel = 32;
    return format;
}

static OSStatus setStreamFormat(AudioStreamID stream, AudioStreamBasicDescription format)
{
    AudioObjectPropertyAddress address = {
        .mSelector = kAudioStreamPropertyPhysicalFormat,
        .mScope = kAudioObjectPropertyScopeGlobal,
        .mElement = kAudioObjectPropertyElementMain,
    };
    OSStatus status = AudioObjectSetPropertyData(stream, &address, 0, NULL, sizeof(format), &format);
    if(status != noErr)
    {
        return status;
    }
    address.mSelector = kAudioStreamPropertyVirtualFormat;
    return AudioObjectSetPropertyData(stream, &address, 0, NULL, sizeof(format), &format);
}

static void renderClick(float *out, UInt32 frames, UInt32 offset, UInt32 clickFrames)
{
    if(offset >= frames)
    {
        return;
    }
    UInt32 count = clickFrames;
    if(offset + count > frames)
    {
        count = frames - offset;
    }
    for(UInt32 i = 0; i < count; ++i)
    {
        double env = 0.5 - 0.5 * cos(2.0 * M_PI * (double)i / (double)(clickFrames - 1));
        float sample = (float)(0.5 * env * sin(2.0 * M_PI * CLICK_HZ * (double)i / gOutputRate));
        out[(offset + i) * gOutputChannels] += sample;
        out[(offset + i) * gOutputChannels + 1] += sample;
    }
}

static OSStatus outputIOProc(
    AudioObjectID inDevice,
    const AudioTimeStamp *inNow,
    const AudioBufferList *inInputData,
    const AudioTimeStamp *inInputTime,
    AudioBufferList *outOutputData,
    const AudioTimeStamp *inOutputTime,
    void *inClientData)
{
    (void)inDevice;
    (void)inNow;
    (void)inInputData;
    (void)inInputTime;
    (void)inClientData;

    if(outOutputData == NULL || outOutputData->mNumberBuffers == 0 || inOutputTime == NULL)
    {
        return noErr;
    }
    AudioBuffer *buffer = &outOutputData->mBuffers[0];
    UInt32 frames = buffer->mDataByteSize / (gOutputChannels * sizeof(float));
    float *out = (float *)buffer->mData;
    memset(out, 0, buffer->mDataByteSize);

    uint64_t bufferStartNs = hostToNs(inOutputTime->mHostTime);
    uint64_t bufferNs = (uint64_t)((double)frames * 1e9 / gOutputRate);
    uint64_t bufferEndNs = bufferStartNs + bufferNs;
    UInt32 clickFrames = (UInt32)(CLICK_MS * gOutputRate / 1000.0);

    uint64_t tail = atomic_load_explicit(&gMarkerTail, memory_order_relaxed);
    uint64_t head = atomic_load_explicit(&gMarkerHead, memory_order_acquire);
    while(tail < head)
    {
        Marker marker = gMarkers[tail % MAX_MARKERS];
        if(marker.host_ns >= bufferEndNs)
        {
            break;
        }
        if(marker.host_ns < bufferStartNs)
        {
            atomic_fetch_add_explicit(&gLateMarkers, 1, memory_order_relaxed);
        }
        else
        {
            UInt32 offset = (UInt32)((double)(marker.host_ns - bufferStartNs) * gOutputRate / 1e9);
            renderClick(out, frames, offset, clickFrames);
            atomic_fetch_add_explicit(&gRenderedMarkers, 1, memory_order_relaxed);
        }
        ++tail;
    }
    atomic_store_explicit(&gMarkerTail, tail, memory_order_release);
    return noErr;
}

static void enqueueMarker(uint64_t hostNs, uint64_t carrierSample, uint32_t kind)
{
    uint64_t head = atomic_load_explicit(&gMarkerHead, memory_order_relaxed);
    uint64_t tail = atomic_load_explicit(&gMarkerTail, memory_order_acquire);
    if(head - tail >= MAX_MARKERS)
    {
        atomic_fetch_add_explicit(&gLateMarkers, 1, memory_order_relaxed);
        return;
    }
    gMarkers[head % MAX_MARKERS] = (Marker){.host_ns = hostNs, .carrier_sample = carrierSample, .kind = kind};
    atomic_store_explicit(&gMarkerHead, head + 1, memory_order_release);
}

static void logMarker(uint64_t syncIndex, uint64_t carrierSample, uint64_t hostNs, uint32_t kind)
{
    if(gMarkerLog == NULL)
    {
        return;
    }
    pthread_mutex_lock(&gLogLock);
    fprintf(gMarkerLog, "%llu,%llu,%llu,%llu,%u\n",
        (unsigned long long)syncIndex,
        (unsigned long long)carrierSample,
        (unsigned long long)hostNs,
        (unsigned long long)gLatencyFrames,
        kind);
    fflush(gMarkerLog);
    pthread_mutex_unlock(&gLogLock);
}

static void setAnchor(uint64_t carrierSample, uint64_t generation)
{
    AudioTimeStamp now = {0};
    now.mFlags = kAudioTimeStampSampleTimeValid | kAudioTimeStampHostTimeValid;
    if(AudioDeviceGetCurrentTime(gOutputDevice, &now) == noErr)
    {
        atomic_store_explicit(&gCarrierAnchorSample, carrierSample, memory_order_relaxed);
        atomic_store_explicit(&gPhysicalAnchorSample, now.mSampleTime, memory_order_relaxed);
        atomic_store_explicit(&gAnchorSeed, generation, memory_order_relaxed);
        atomic_store_explicit(&gHaveAnchor, 1, memory_order_release);
    }
}

static void *clockThread(void *arg)
{
    (void)arg;
    while(!gStop)
    {
        if(atomic_load_explicit(&gHaveAnchor, memory_order_acquire))
        {
            AudioTimeStamp now = {0};
            now.mFlags = kAudioTimeStampSampleTimeValid | kAudioTimeStampHostTimeValid;
            if(AudioDeviceGetCurrentTime(gOutputDevice, &now) == noErr)
            {
                double carrierSample = (double)atomic_load_explicit(&gCarrierAnchorSample, memory_order_relaxed) +
                    (now.mSampleTime - atomic_load_explicit(&gPhysicalAnchorSample, memory_order_relaxed)) * (CARRIER_RATE / gOutputRate);
                SalsaCarrierPublishClock(gShared, (uint64_t)llround(carrierSample) + gClockOffsetFrames,
                    now.mHostTime, atomic_load_explicit(&gAnchorSeed, memory_order_relaxed));
                atomic_store(&gShared->control.service_heartbeat, mach_absolute_time());
            }
        }
        usleep(50000);
    }
    return NULL;
}

static int checkSync(const uint8_t *payload, uint32_t bytes, uint32_t *offset, uint32_t *dataType)
{
    for(uint32_t o = 0; o + 7 < bytes; o += 2)
    {
        uint16_t w0 = (uint16_t)(payload[o] | (payload[o + 1] << 8));
        uint16_t w1 = (uint16_t)(payload[o + 2] | (payload[o + 3] << 8));
        if(w0 != 0xF872 || w1 != 0x4E1F)
        {
            continue;
        }
        uint16_t type = (uint16_t)(payload[o + 4] | (payload[o + 5] << 8));
        uint16_t length = (uint16_t)(payload[o + 6] | (payload[o + 7] << 8));
        if((type == 21 || type == 22) && length > 0)
        {
            *offset = o;
            *dataType = type;
            return 1;
        }
    }
    return 0;
}

static void *carrierThread(void *arg)
{
    (void)arg;
    uint64_t lastGeneration = UINT64_MAX;
    uint64_t syncIndex = 0;
    while(!gStop)
    {
        uint64_t head = atomic_load_explicit(&gShared->control.head, memory_order_acquire);
        uint64_t tail = atomic_load_explicit(&gShared->control.tail, memory_order_relaxed);
        while(tail < head)
        {
            SalsaCarrierSlot *slot = &gShared->slots[tail % SALSA_CARRIER_SLOT_COUNT];
            SalsaCarrierSlotHeader header = slot->header;
            uint32_t offset = 0;
            uint32_t dataType = 0;
            if(header.generation != lastGeneration)
            {
                lastGeneration = header.generation;
                syncIndex = 0;
                atomic_store_explicit(&gHaveAnchor, 0, memory_order_release);
            }
            if(checkSync(slot->payload, header.payload_bytes, &offset, &dataType))
            {
                if(!atomic_load_explicit(&gHaveAnchor, memory_order_acquire))
                {
                    setAnchor(header.start_sample, header.generation);
                }
                uint32_t frameOffset = offset / header.bytes_per_frame;
                uint64_t carrierSample = header.start_sample + frameOffset;
                uint64_t markerNs = hostToNs(header.host_time) +
                    (uint64_t)((double)frameOffset * 1e9 / CARRIER_RATE) +
                    SCHEDULING_ALLOWANCE_NS;
                if(syncIndex == 0)
                {
                    enqueueMarker(markerNs, carrierSample, 0);
                    logMarker(syncIndex, carrierSample, markerNs, 0);
                    if(gLatencyNs > 0)
                    {
                        enqueueMarker(markerNs + gLatencyNs, carrierSample, 1);
                        logMarker(syncIndex, carrierSample, markerNs + gLatencyNs, 1);
                    }
                }
                else if((syncIndex % DRIFT_SYNC_INTERVAL) == 0)
                {
                    enqueueMarker(markerNs, carrierSample, 2);
                    logMarker(syncIndex, carrierSample, markerNs, 2);
                }
                atomic_fetch_add_explicit(&gShared->control.service_markers, 1, memory_order_relaxed);
                ++syncIndex;
            }
            ++tail;
        }
        atomic_store_explicit(&gShared->control.tail, tail, memory_order_release);
        usleep(1000);
    }
    return NULL;
}

static void setUnity(AudioDeviceID device, AudioObjectPropertyScope scope)
{
    AudioObjectPropertyAddress address = {
        .mSelector = kAudioDevicePropertyVolumeScalar,
        .mScope = scope,
        .mElement = kAudioObjectPropertyElementMain,
    };
    Float32 volume = 1.0f;
    if(AudioObjectHasProperty(device, &address))
    {
        (void)AudioObjectSetPropertyData(device, &address, 0, NULL, sizeof(volume), &volume);
    }
    address.mSelector = kAudioDevicePropertyMute;
    UInt32 mute = 0;
    if(AudioObjectHasProperty(device, &address))
    {
        (void)AudioObjectSetPropertyData(device, &address, 0, NULL, sizeof(mute), &mute);
    }
}

static void usage(const char *name)
{
    fprintf(stderr,
        "usage: %s [options]\n"
        "  --output-uid UID      CoreAudio output device UID (default BlackHole2ch_UID)\n"
        "  --latency-frames N    declared carrier latency in 192 kHz frames (default 0)\n"
        "  --clock-offset-frames N  add N 192 kHz frames to the published clock\n"
        "  --duration SECONDS    stop after N seconds (default 0 = until SIGINT)\n"
        "  --marker-log PATH     write scheduled marker CSV\n"
        "  --unity               set output volume to 1.0 and unmute\n",
        name);
}

int main(int argc, char **argv)
{
    const char *outputUid = "BlackHole2ch_UID";
    const char *markerLogPath = NULL;
    double duration = 0.0;
    int setUnityFlag = 0;

    mach_timebase_info(&gTimebase);

    for(int i = 1; i < argc; ++i)
    {
        if(strcmp(argv[i], "--output-uid") == 0 && i + 1 < argc)
        {
            outputUid = argv[++i];
        }
        else if(strcmp(argv[i], "--latency-frames") == 0 && i + 1 < argc)
        {
            gLatencyFrames = strtoull(argv[++i], NULL, 0);
        }
        else if(strcmp(argv[i], "--clock-offset-frames") == 0 && i + 1 < argc)
        {
            gClockOffsetFrames = strtoull(argv[++i], NULL, 0);
        }
        else if(strcmp(argv[i], "--duration") == 0 && i + 1 < argc)
        {
            duration = strtod(argv[++i], NULL);
        }
        else if(strcmp(argv[i], "--marker-log") == 0 && i + 1 < argc)
        {
            markerLogPath = argv[++i];
        }
        else if(strcmp(argv[i], "--unity") == 0)
        {
            setUnityFlag = 1;
        }
        else if(strcmp(argv[i], "--help") == 0 || strcmp(argv[i], "-h") == 0)
        {
            usage(argv[0]);
            return 0;
        }
        else
        {
            fprintf(stderr, "unknown argument: %s\n", argv[i]);
            usage(argv[0]);
            return 2;
        }
    }

    gLatencyNs = (uint64_t)llround((double)gLatencyFrames * 1e9 / CARRIER_RATE);

    int shmFD = shm_open(SALSA_CARRIER_SHM_NAME, O_CREAT | O_RDWR, 0666);
    if(shmFD < 0)
    {
        perror("shm_open");
        return 1;
    }
    struct stat shmStat;
    if(fstat(shmFD, &shmStat) != 0 || shmStat.st_size < (off_t)sizeof(SalsaCarrierShared))
    {
        if(ftruncate(shmFD, sizeof(SalsaCarrierShared)) != 0)
        {
            perror("ftruncate");
            return 1;
        }
    }
    gShared = mmap(NULL, sizeof(SalsaCarrierShared), PROT_READ | PROT_WRITE, MAP_SHARED, shmFD, 0);
    if(gShared == MAP_FAILED)
    {
        perror("mmap");
        return 1;
    }
    close(shmFD);
    if(atomic_load(&gShared->control.magic) != SALSA_CARRIER_SHM_MAGIC ||
       atomic_load(&gShared->control.version) != SALSA_CARRIER_SHM_VERSION)
    {
        SalsaCarrierInitialize(gShared);
    }
    atomic_store(&gShared->control.declared_latency_frames, gLatencyFrames);
    atomic_store(&gShared->control.latency_valid, 1);

    OSStatus status = findDevice(outputUid, &gOutputDevice);
    if(status != noErr || gOutputDevice == kAudioObjectUnknown)
    {
        fprintf(stderr, "output device %s not found (%d)\n", outputUid, (int)status);
        return 1;
    }
    if(setUnityFlag)
    {
        setUnity(gOutputDevice, kAudioObjectPropertyScopeOutput);
    }
    gOutputStream = firstStream(gOutputDevice, kAudioObjectPropertyScopeOutput);
    if(gOutputStream == kAudioObjectUnknown)
    {
        fprintf(stderr, "output device has no stream\n");
        return 1;
    }
    AudioStreamBasicDescription outputFormat = makeOutputFormat();
    status = setStreamFormat(gOutputStream, outputFormat);
    if(status != noErr)
    {
        fprintf(stderr, "set output format failed: %d\n", (int)status);
        return 1;
    }
    UInt32 bufferFrames = 512;
    AudioObjectPropertyAddress bufferAddress = {
        .mSelector = kAudioDevicePropertyBufferFrameSize,
        .mScope = kAudioObjectPropertyScopeGlobal,
        .mElement = kAudioObjectPropertyElementMain,
    };
    (void)AudioObjectSetPropertyData(gOutputDevice, &bufferAddress, 0, NULL, sizeof(bufferFrames), &bufferFrames);

    if(markerLogPath != NULL)
    {
        gMarkerLog = fopen(markerLogPath, "w");
        if(gMarkerLog == NULL)
        {
            perror(markerLogPath);
            return 1;
        }
        fprintf(gMarkerLog, "sync,carrier_sample,host_ns,latency_frames,kind\n");
    }

    signal(SIGINT, onSignal);
    signal(SIGTERM, onSignal);

    AudioDeviceIOProcID procID = NULL;
    status = AudioDeviceCreateIOProcID(gOutputDevice, outputIOProc, NULL, &procID);
    if(status != noErr)
    {
        fprintf(stderr, "AudioDeviceCreateIOProcID failed: %d\n", (int)status);
        return 1;
    }
    status = AudioDeviceStart(gOutputDevice, procID);
    if(status != noErr)
    {
        fprintf(stderr, "AudioDeviceStart failed: %d\n", (int)status);
        return 1;
    }

    printf("salsa-audiod ready output=%s latency=%llu frames marker_log=%s\n",
        outputUid, (unsigned long long)gLatencyFrames, markerLogPath ? markerLogPath : "-");
    fflush(stdout);

    pthread_t clockThreadId;
    pthread_t carrierThreadId;
    pthread_create(&clockThreadId, NULL, clockThread, NULL);
    pthread_create(&carrierThreadId, NULL, carrierThread, NULL);

    uint64_t startNs = hostToNs(mach_absolute_time());
    while(!gStop)
    {
        usleep(100000);
        if(duration > 0.0)
        {
            uint64_t nowNs = hostToNs(mach_absolute_time());
            if((double)(nowNs - startNs) / 1e9 >= duration)
            {
                break;
            }
        }
    }
    gStop = 1;
    pthread_join(clockThreadId, NULL);
    pthread_join(carrierThreadId, NULL);
    AudioDeviceStop(gOutputDevice, procID);
    AudioDeviceDestroyIOProcID(gOutputDevice, procID);

    printf("salsa-audiod stopped markers=%llu rendered=%llu late=%llu dropped=%llu\n",
        (unsigned long long)atomic_load(&gShared->control.service_markers),
        (unsigned long long)atomic_load(&gRenderedMarkers),
        (unsigned long long)atomic_load(&gLateMarkers),
        (unsigned long long)atomic_load(&gShared->control.dropped_frames));
    if(gMarkerLog != NULL)
    {
        fclose(gMarkerLog);
    }
    return 0;
}
