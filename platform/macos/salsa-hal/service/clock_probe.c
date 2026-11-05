/* Compare the HAL clock to the service-published physical clock mapping. */

#include <CoreAudio/CoreAudio.h>
#include <CoreFoundation/CoreFoundation.h>
#include <mach/mach_time.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <fcntl.h>
#include <unistd.h>
#include "../../../common/salsa_carrier_shared.h"
#include "../src/SalsaHALProbe.h"

static volatile sig_atomic_t gStop;
static mach_timebase_info_data_t gTimebase;

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

int main(int argc, char **argv)
{
    const char *uid = "salsa-atmos";
    const char *outPath = NULL;
    double duration = 5.0;
    useconds_t intervalUs = 100000;

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
        else if(strcmp(argv[i], "--duration") == 0 && i + 1 < argc)
        {
            duration = strtod(argv[++i], NULL);
        }
        else if(strcmp(argv[i], "--interval-ms") == 0 && i + 1 < argc)
        {
            intervalUs = (useconds_t)(strtod(argv[++i], NULL) * 1000.0);
        }
    }

    mach_timebase_info(&gTimebase);
    signal(SIGINT, onSignal);
    signal(SIGTERM, onSignal);

    AudioDeviceID device = kAudioObjectUnknown;
    OSStatus status = findDevice(uid, &device);
    if(status != noErr || device == kAudioObjectUnknown)
    {
        fprintf(stderr, "device %s not found (%d)\n", uid, (int)status);
        return 1;
    }

    int shmFD = shm_open(SALSA_CARRIER_SHM_NAME, O_RDWR, 0666);
    if(shmFD < 0)
    {
        perror("shm_open");
        return 1;
    }
    SalsaCarrierShared *shared = mmap(NULL, sizeof(SalsaCarrierShared), PROT_READ | PROT_WRITE, MAP_SHARED, shmFD, 0);
    close(shmFD);
    if(shared == MAP_FAILED)
    {
        perror("mmap");
        return 1;
    }

    FILE *out = stdout;
    if(outPath != NULL)
    {
        out = fopen(outPath, "w");
        if(out == NULL)
        {
            perror(outPath);
            return 1;
        }
    }
    fprintf(out, "hal_host_ns,hal_sample,hal_seed,shm_sample,shm_host_ns,shm_seed,generation,latency,predicted,delta\n");

    AudioObjectPropertyAddress clockAddress = {
        .mSelector = kSalsaHALClockProperty,
        .mScope = kAudioObjectPropertyScopeGlobal,
        .mElement = kAudioObjectPropertyElementMain,
    };
    uint64_t start = mach_absolute_time();
    while(!gStop)
    {
        CFDataRef data = NULL;
        UInt32 dataSize = sizeof(data);
        status = AudioObjectGetPropertyData(device, &clockAddress, 0, NULL, &dataSize, &data);
        if(status == noErr && data != NULL && CFDataGetLength(data) == sizeof(SalsaHALClock))
        {
            SalsaHALClock clock;
            memcpy(&clock, CFDataGetBytePtr(data), sizeof(clock));
            uint64_t shmSample = 0, shmHost = 0, shmSeed = 0;
            int clockValid = SalsaCarrierReadClock(shared, &shmSample, &shmHost, &shmSeed);
            uint64_t halHostNs = hostToNs(clock.host);
            uint64_t shmHostNs = clockValid ? hostToNs(shmHost) : 0;
            double predicted = clockValid ? (double)shmSample +
                ((double)halHostNs - (double)shmHostNs) * 192000.0 / 1e9 : 0.0;
            fprintf(out, "%llu,%llu,%llu,%llu,%llu,%llu,%llu,%llu,%.0f,%.0f\n",
                (unsigned long long)halHostNs,
                (unsigned long long)clock.sample,
                (unsigned long long)clock.seed,
                clockValid ? (unsigned long long)shmSample : 0ull,
                clockValid ? (unsigned long long)shmHostNs : 0ull,
                clockValid ? (unsigned long long)shmSeed : 0ull,
                (unsigned long long)atomic_load(&shared->control.generation),
                (unsigned long long)atomic_load(&shared->control.declared_latency_frames),
                predicted,
                (double)clock.sample - predicted);
        }
        if(data != NULL)
        {
            CFRelease(data);
        }
        double elapsed = (double)(mach_absolute_time() - start) * (double)gTimebase.numer /
            (double)gTimebase.denom / 1e9;
        if(duration > 0.0 && elapsed >= duration)
        {
            break;
        }
        usleep(intervalUs);
    }
    if(out != stdout)
    {
        fclose(out);
    }
    return 0;
}
