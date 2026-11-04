/* Record a CoreAudio input device to raw interleaved Float32. */

#include <CoreAudio/CoreAudio.h>
#include <CoreFoundation/CoreFoundation.h>
#include <mach/mach_time.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

static volatile sig_atomic_t gStop;

static void onSignal(int sig)
{
    (void)sig;
    gStop = 1;
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

static AudioStreamBasicDescription makeFormat(double rate, UInt32 channels)
{
    AudioStreamBasicDescription format = {0};
    format.mSampleRate = rate;
    format.mFormatID = kAudioFormatLinearPCM;
    format.mFormatFlags = kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked | kAudioFormatFlagsNativeEndian;
    format.mBytesPerPacket = channels * sizeof(float);
    format.mFramesPerPacket = 1;
    format.mBytesPerFrame = format.mBytesPerPacket;
    format.mChannelsPerFrame = channels;
    format.mBitsPerChannel = 32;
    return format;
}

static OSStatus setFormat(AudioStreamID stream, AudioStreamBasicDescription format)
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

static uint64_t hostToNs(uint64_t host, const struct mach_timebase_info *tb)
{
    return (uint64_t)((double)host * (double)tb->numer / (double)tb->denom);
}

struct context {
    FILE *out;
    uint64_t callbacks;
    uint64_t first_host;
    uint64_t first_sample;
    uint64_t last_host;
    uint64_t last_sample;
    int have_first;
};

static OSStatus captureIOProc(
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
    (void)outOutputData;
    (void)inOutputTime;
    struct context *ctx = (struct context *)inClientData;
    if(inInputData == NULL || inInputData->mNumberBuffers == 0)
    {
        return noErr;
    }
    const AudioBuffer *buffer = &inInputData->mBuffers[0];
    if(buffer->mData != NULL && buffer->mDataByteSize > 0)
    {
        fwrite(buffer->mData, 1, buffer->mDataByteSize, ctx->out);
    }
    ctx->callbacks += 1;
    if(!ctx->have_first)
    {
        ctx->first_host = inInputTime->mHostTime;
        ctx->first_sample = (uint64_t)inInputTime->mSampleTime;
        ctx->have_first = 1;
    }
    ctx->last_host = inInputTime->mHostTime;
    ctx->last_sample = (uint64_t)inInputTime->mSampleTime;
    return noErr;
}

int main(int argc, char **argv)
{
    const char *uid = "BlackHole2ch_UID";
    const char *outPath = "loopback.raw";
    const char *metaPath = NULL;
    double rate = 48000.0;
    UInt32 channels = 2;
    double seconds = 0.0;
    int setUnityFlag = 0;

    for(int i = 1; i < argc; ++i)
    {
        if(strcmp(argv[i], "--device-uid") == 0 && i + 1 < argc)
        {
            uid = argv[++i];
        }
        else if(strcmp(argv[i], "--out") == 0 && i + 1 < argc)
        {
            outPath = argv[++i];
        }
        else if(strcmp(argv[i], "--meta") == 0 && i + 1 < argc)
        {
            metaPath = argv[++i];
        }
        else if(strcmp(argv[i], "--rate") == 0 && i + 1 < argc)
        {
            rate = strtod(argv[++i], NULL);
        }
        else if(strcmp(argv[i], "--channels") == 0 && i + 1 < argc)
        {
            channels = (UInt32)strtoul(argv[++i], NULL, 0);
        }
        else if(strcmp(argv[i], "--seconds") == 0 && i + 1 < argc)
        {
            seconds = strtod(argv[++i], NULL);
        }
        else if(strcmp(argv[i], "--unity") == 0)
        {
            setUnityFlag = 1;
        }
        else
        {
            fprintf(stderr, "unknown argument: %s\n", argv[i]);
            return 2;
        }
    }

    signal(SIGINT, onSignal);
    signal(SIGTERM, onSignal);

    AudioDeviceID device = kAudioObjectUnknown;
    OSStatus status = findDevice(uid, &device);
    if(status != noErr || device == kAudioObjectUnknown)
    {
        fprintf(stderr, "input device %s not found (%d)\n", uid, (int)status);
        return 1;
    }
    if(setUnityFlag)
    {
        setUnity(device, kAudioObjectPropertyScopeInput);
    }
    AudioStreamID stream = firstStream(device, kAudioObjectPropertyScopeInput);
    if(stream == kAudioObjectUnknown)
    {
        fprintf(stderr, "input device has no input stream\n");
        return 1;
    }
    AudioStreamBasicDescription format = makeFormat(rate, channels);
    status = setFormat(stream, format);
    if(status != noErr)
    {
        fprintf(stderr, "set input format failed: %d\n", (int)status);
        return 1;
    }
    FILE *out = fopen(outPath, "wb");
    if(out == NULL)
    {
        perror(outPath);
        return 1;
    }
    setvbuf(out, NULL, _IOFBF, 4u * 1024u * 1024u);

    struct context ctx = {.out = out};
    AudioDeviceIOProcID procID = NULL;
    status = AudioDeviceCreateIOProcID(device, captureIOProc, &ctx, &procID);
    if(status != noErr)
    {
        fprintf(stderr, "AudioDeviceCreateIOProcID failed: %d\n", (int)status);
        return 1;
    }
    status = AudioDeviceStart(device, procID);
    if(status != noErr)
    {
        fprintf(stderr, "AudioDeviceStart failed: %d\n", (int)status);
        return 1;
    }
    printf("loopback capture ready device=%s rate=%.0f channels=%u\n", uid, rate, (unsigned)channels);
    fflush(stdout);

    struct mach_timebase_info tb;
    mach_timebase_info(&tb);
    uint64_t start = mach_absolute_time();
    while(!gStop)
    {
        usleep(100000);
        if(seconds > 0.0)
        {
            uint64_t now = mach_absolute_time();
            double elapsed = (double)(now - start) * (double)tb.numer / (double)tb.denom / 1e9;
            if(elapsed >= seconds)
            {
                break;
            }
        }
    }
    AudioDeviceStop(device, procID);
    AudioDeviceDestroyIOProcID(device, procID);
    fflush(out);
    fclose(out);

    if(metaPath != NULL)
    {
        FILE *meta = fopen(metaPath, "w");
        if(meta != NULL)
        {
            struct mach_timebase_info tb;
            mach_timebase_info(&tb);
            fprintf(meta,
                "{\"callbacks\": %llu, \"first_host\": %llu, \"first_host_ns\": %llu, \"first_sample\": %llu, "
                "\"last_host\": %llu, \"last_host_ns\": %llu, \"last_sample\": %llu, \"rate\": %.0f, \"channels\": %u}\n",
                (unsigned long long)ctx.callbacks,
                (unsigned long long)ctx.first_host,
                (unsigned long long)hostToNs(ctx.first_host, &tb),
                (unsigned long long)ctx.first_sample,
                (unsigned long long)ctx.last_host,
                (unsigned long long)hostToNs(ctx.last_host, &tb),
                (unsigned long long)ctx.last_sample,
                rate, (unsigned)channels);
            fclose(meta);
        }
    }
    printf("loopback capture stopped callbacks=%llu first_host=%llu first_sample=%llu last_host=%llu last_sample=%llu\n",
        (unsigned long long)ctx.callbacks,
        (unsigned long long)ctx.first_host,
        (unsigned long long)ctx.first_sample,
        (unsigned long long)ctx.last_host,
        (unsigned long long)ctx.last_sample);
    return 0;
}
