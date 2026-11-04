// Exercise the driver contract without loading a system audio plug-in.
#include <assert.h>
#include <stdio.h>
#include <sys/mman.h>
#include "../src/SalsaHAL.c"

static dispatch_semaphore_t requested;
static UInt64 pendingAction;

static OSStatus copyStorage(AudioServerPlugInHostRef host, CFStringRef key, CFPropertyListRef *data)
{
    *data = NULL;
    return noErr;
}

static OSStatus requestChange(AudioServerPlugInHostRef host, AudioObjectID device, UInt64 action, void *info)
{
    assert(device == kObjectID_Device);
    assert(info == NULL);
    pendingAction = action;
    dispatch_semaphore_signal(requested);
    return noErr;
}

static void setChannels(AudioObjectID stream, UInt32 channels)
{
    AudioStreamBasicDescription old = stream == kObjectID_Stream_Input ? gStream_Input_Format : gStream_Output_Format;
    AudioStreamBasicDescription format = stream == kObjectID_Stream_Input ? SalsaHAL_MakeCaptureFormat(channels) : SalsaHAL_MakeCarrierFormat(channels);
    AudioObjectPropertyAddress address = {kAudioStreamPropertyPhysicalFormat, kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain};
    AudioObjectPropertyAddress changed[2];
    UInt32 count = 0;
    assert(SalsaHAL_SetStreamPropertyData(gAudioServerPlugInDriverRef, stream, 0, &address, 0, NULL, sizeof(format), &format, &count, changed) == noErr);
    assert(count == 0);
    if(channels != old.mChannelsPerFrame)
    {
        assert(dispatch_semaphore_wait(requested, dispatch_time(DISPATCH_TIME_NOW, NSEC_PER_SEC)) == 0);
        assert((stream == kObjectID_Stream_Input ? gStream_Input_Format : gStream_Output_Format).mChannelsPerFrame == old.mChannelsPerFrame);
        assert(SalsaHAL_PerformDeviceConfigurationChange(gAudioServerPlugInDriverRef, kObjectID_Device, pendingAction, NULL) == noErr);
    }
    assert((stream == kObjectID_Stream_Input ? gStream_Input_Format : gStream_Output_Format).mChannelsPerFrame == channels);
}

static void testTransportRecovery(void)
{
    SalsaCarrierControl *control = &gCarrierShared->control;
    AudioServerPlugInIOCycleInfo cycle = {0};
    uint16_t payload[8] = {1, 2, 3, 4, 5, 6, 7, 8};
    uint64_t head = atomic_load(&control->head);
    atomic_store(&control->tail, head);
    for(unsigned i = 0; i < SALSA_CARRIER_SLOT_COUNT; ++i)
        SalsaHAL_TransportWrite(&cycle, 4, 2, (const UInt8 *)payload);
    head = atomic_load(&control->head);
    uint64_t dropped = atomic_load(&control->dropped_frames);
    SalsaHAL_TransportWrite(&cycle, 4, 2, (const UInt8 *)payload);
    assert(atomic_load(&control->head) == head);
    assert(atomic_load(&control->dropped_frames) == dropped + 4);
    atomic_fetch_add(&control->tail, 1);
    SalsaHAL_TransportWrite(&cycle, 4, 2, (const UInt8 *)payload);
    assert(gCarrierShared->slots[head % SALSA_CARRIER_SLOT_COUNT].header.flags & SALSA_CARRIER_FLAG_DROPPED);
    ++head;
    atomic_fetch_add(&control->tail, 1);
    SalsaHAL_TransportWrite(&cycle, 4, 2, (const UInt8 *)payload);
    assert(gCarrierShared->slots[head % SALSA_CARRIER_SLOT_COUNT].header.flags == 0);
    ++head;
    uint64_t tail = atomic_load(&control->tail);
    atomic_fetch_add(&gSessionGeneration, 1);
    SalsaHAL_TransportReset();
    assert(atomic_load(&control->head) == head);
    assert(atomic_load(&control->tail) == tail);
    atomic_store(&control->tail, head);
    SalsaHAL_TransportWrite(&cycle, 4, 2, (const UInt8 *)payload);
    SalsaCarrierSlotHeader *header = &gCarrierShared->slots[head % SALSA_CARRIER_SLOT_COUNT].header;
    assert(header->generation == atomic_load(&gSessionGeneration));
    assert(header->flags & SALSA_CARRIER_FLAG_DISCONTINUITY);
    atomic_store(&control->tail, head + 1);
    // Oversized writes have the same gap semantics as a full ring.
    SalsaHAL_TransportWrite(&cycle, SALSA_CARRIER_SLOT_BYTES, 2, (const UInt8 *)payload);
    SalsaHAL_TransportWrite(&cycle, 4, 2, (const UInt8 *)payload);
    assert(gCarrierShared->slots[(head + 1) % SALSA_CARRIER_SLOT_COUNT].header.flags & SALSA_CARRIER_FLAG_DROPPED);
    atomic_store(&control->tail, head + 2);
    printf("PASS: shared ring overflow flags and monotonic session reset\n");
}

int main(void)
{
    requested = dispatch_semaphore_create(0);
    struct AudioServerPlugInHostInterface host = {.CopyFromStorage = copyStorage, .RequestDeviceConfigurationChange = requestChange};
    assert(SalsaHAL_Initialize(gAudioServerPlugInDriverRef, &host) == noErr);
    assert(gCarrierShared != NULL);
    assert(!SalsaHAL_IsSupportedOutputFormat(NULL));
    testTransportRecovery();

#if SALSA_HAL_OUTPUT_ONLY
    AudioObjectPropertyAddress inputStreams = {
        .mSelector = kAudioDevicePropertyStreams,
        .mScope = kAudioObjectPropertyScopeInput,
        .mElement = kAudioObjectPropertyElementMain,
    };
    UInt32 inputStreamBytes = 1;
    assert(SalsaHAL_GetDevicePropertyDataSize(gAudioServerPlugInDriverRef, kObjectID_Device, 0, &inputStreams, 0, NULL, &inputStreamBytes) == noErr);
    assert(inputStreamBytes == 0);
    Boolean willRead = true;
    assert(SalsaHAL_WillDoIOOperation(gAudioServerPlugInDriverRef, kObjectID_Device, 0, kAudioServerPlugInIOOperationReadInput, &willRead, NULL) == noErr);
    assert(!willRead);
    printf("PASS: output-only build exposes no input stream\n");
    shm_unlink(SALSA_CARRIER_SHM_NAME);
    return 0;
#endif

    atomic_store(&gCarrierShared->control.declared_latency_frames, 1234);
    atomic_store(&gCarrierShared->control.latency_valid, 1);
    AudioObjectPropertyAddress latencyAddress = {kAudioDevicePropertyLatency, kAudioObjectPropertyScopeOutput, kAudioObjectPropertyElementMain};
    UInt32 latencyValue = 0;
    UInt32 latencySize = sizeof(latencyValue);
    assert(SalsaHAL_GetDevicePropertyData(gAudioServerPlugInDriverRef, kObjectID_Device, 0, &latencyAddress, 0, NULL, sizeof(latencyValue), &latencySize, &latencyValue) == noErr);
    assert(latencyValue == 1234);

    SalsaCarrierPublishClock(gCarrierShared, 1920000, mach_absolute_time(), 42);
    Float64 clockSample = 0;
    UInt64 clockHost = 0;
    UInt64 clockSeed = 0;
    assert(SalsaHAL_GetZeroTimeStamp(gAudioServerPlugInDriverRef, kObjectID_Device, 0, &clockSample, &clockHost, &clockSeed) == noErr);
    assert(clockSeed == 42);
    assert((uint64_t)clockSample > 0);
    assert(((uint64_t)clockSample % kDevice_RingBufferSize) == 0);
    atomic_store(&gCarrierShared->control.clock_valid, 0);
    atomic_store(&gCarrierShared->control.latency_valid, 0);
#if SALSA_HAL_FIXED_CHANNELS
    const UInt32 modes[] = {SALSA_HAL_FIXED_CHANNELS};
#else
    const UInt32 modes[] = {2, 8, 2};
#endif
    for(size_t mode = 0; mode < sizeof(modes) / sizeof(modes[0]); ++mode)
    {
        UInt32 channels = modes[mode];
        AudioStreamBasicDescription format = SalsaHAL_MakeCarrierFormat(channels);
        assert(SalsaHAL_IsSupportedOutputFormat(&format));
        format.mFormatID = 'bad!';
        assert(!SalsaHAL_IsSupportedOutputFormat(&format));
        setChannels(kObjectID_Stream_Input, channels);
        setChannels(kObjectID_Stream_Output, channels);
        AudioObjectPropertyAddress recordAddress = {kSalsaHALRecordProperty, kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain};
        AudioObjectPropertyAddress changes[2];
        UInt32 count;
        CFBooleanRef command = kCFBooleanTrue;
        assert(SalsaHAL_SetDevicePropertyData(gAudioServerPlugInDriverRef, kObjectID_Device, 0, &recordAddress, 0, NULL, sizeof(command), &command, &count, changes) == noErr);
        assert(SalsaHAL_StartIO(gAudioServerPlugInDriverRef, kObjectID_Device, 1) == noErr);
        uint64_t expectedSeed = atomic_load(&gSessionGeneration);
        assert(SalsaHAL_SetDevicePropertyData(gAudioServerPlugInDriverRef, kObjectID_Device, 0, &recordAddress, 0, NULL, sizeof(command), &command, &count, changes) == kAudioHardwareIllegalOperationError);

        // Include every S16 word, negative values, and channel-dependent bytes.
        uint16_t sent[65536], received[65536];
        for(size_t i = 0; i < 65536; ++i) sent[i] = (uint16_t)i;
        memset(received, 0, sizeof(received));
        UInt32 frames = 65536 / channels;
        assert(SalsaHAL_DoIOOperation(gAudioServerPlugInDriverRef, kObjectID_Device, kObjectID_Stream_Output, 1, kAudioServerPlugInIOOperationWriteMix, frames, NULL, sent, NULL) == noErr);
        assert(SalsaHAL_DoIOOperation(gAudioServerPlugInDriverRef, kObjectID_Device, kObjectID_Stream_Input, 1, kAudioServerPlugInIOOperationReadInput, frames, NULL, received, NULL) == noErr);
        assert(memcmp(sent, received, sizeof(sent)) == 0);

        // A full ring drops the new block without moving the reader.
        uint64_t read = atomic_load(&gCarrierReadFrame);
        atomic_store(&gCarrierWriteFrame, read + kCarrierRingFrames);
        assert(SalsaHAL_DoIOOperation(gAudioServerPlugInDriverRef, kObjectID_Device, kObjectID_Stream_Output, 1, kAudioServerPlugInIOOperationWriteMix, frames, NULL, sent, NULL) == noErr);
        assert(atomic_load(&gCarrierReadFrame) == read);
        assert(atomic_load(&gCarrierWriteFrame) == read + kCarrierRingFrames);
        assert(atomic_load(&gCarrierDroppedFrames) == frames);

        // A delayed poll must return the current period, not advance once.
        gDevice_AnchorHostTime -= (UInt64)(3 * kDevice_RingBufferSize * gDevice_HostTicksPerFrame);
        Float64 sample;
        UInt64 ticks, seed;
        assert(SalsaHAL_GetZeroTimeStamp(gAudioServerPlugInDriverRef, kObjectID_Device, 1, &sample, &ticks, &seed) == noErr);
        assert(sample >= 3 * kDevice_RingBufferSize);
        assert(seed == expectedSeed);
        assert(SalsaHAL_StopIO(gAudioServerPlugInDriverRef, kObjectID_Device, 1) == noErr);
        CFDataRef recording = NULL;
        UInt32 size = sizeof(recording);
        assert(SalsaHAL_GetDevicePropertyData(gAudioServerPlugInDriverRef, kObjectID_Device, 0, &recordAddress, 0, NULL, size, &size, &recording) == noErr);
        assert(CFDataGetLength(recording) == 2 * sizeof(sent));
        assert(memcmp(CFDataGetBytePtr(recording), sent, sizeof(sent)) == 0);
        CFRelease(recording);
        command = kCFBooleanFalse;
        assert(SalsaHAL_SetDevicePropertyData(gAudioServerPlugInDriverRef, kObjectID_Device, 0, &recordAddress, 0, NULL, sizeof(command), &command, &count, changes) == noErr);
        assert(SalsaHAL_GetDevicePropertyData(gAudioServerPlugInDriverRef, kObjectID_Device, 0, &recordAddress, 0, NULL, size, &size, &recording) == kAudioHardwareIllegalOperationError);
        // The shared transport writes one block with carrier position and payload.
        uint16_t small[4096];
        for(size_t i = 0; i < 4096; ++i) small[i] = (uint16_t)(i ^ 0x5a5a);
        AudioServerPlugInIOCycleInfo cycle = {0};
        cycle.mOutputTime.mSampleTime = 12345;
        cycle.mOutputTime.mHostTime = mach_absolute_time();
        cycle.mOutputTime.mFlags = kAudioTimeStampSampleTimeValid | kAudioTimeStampHostTimeValid;
        uint64_t head = atomic_load(&gCarrierShared->control.head);
        assert(SalsaHAL_DoIOOperation(gAudioServerPlugInDriverRef, kObjectID_Device, kObjectID_Stream_Output, 1, kAudioServerPlugInIOOperationWriteMix, 4096 / channels, &cycle, small, NULL) == noErr);
        uint64_t head2 = atomic_load(&gCarrierShared->control.head);
        assert(head2 == head + 1);
        SalsaCarrierSlot *slot = &gCarrierShared->slots[head % SALSA_CARRIER_SLOT_COUNT];
        assert(slot->header.start_sample == 12345);
        assert(slot->header.frames == 4096 / channels);
        assert(slot->header.channels == channels);
        assert(slot->header.payload_bytes == 8192);
        assert(memcmp(slot->payload, small, 8192) == 0);

        printf("PASS: %u channels, deferred configuration, all S16 words, overflow, clock, recording\n", channels);
    }
    dispatch_release(requested);
    shm_unlink(SALSA_CARRIER_SHM_NAME);
    return 0;
}
