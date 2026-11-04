/*
 * salsa_carrier_capture
 *
 * Reads the loopback input stream of the Salsa HAL device and writes the
 * received 16-bit carrier words verbatim. The HAL I/O callback only copies
 * into a bounded ring, and this separate process drains it.
 */

#include <CoreAudio/CoreAudio.h>
#include <CoreFoundation/CoreFoundation.h>
#include <mach/mach_time.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <math.h>
#include "../src/SalsaHALProbe.h"

static volatile sig_atomic_t gStop = 0;

static void onSignal(int sig)
{
	(void)sig;
	gStop = 1;
}

static void fail(OSStatus status, const char *what)
{
	fprintf(stderr, "%s failed: %d\n", what, (int)status);
	exit(1);
}

static AudioDeviceID findDevice(const char *uid)
{
	CFStringRef cfuid = CFStringCreateWithCString(NULL, uid, kCFStringEncodingUTF8);
	if(cfuid == NULL)
	{
		fprintf(stderr, "cannot create CFString for device UID\n");
		exit(1);
	}

	AudioDeviceID device = kAudioObjectUnknown;
	AudioValueTranslation translation = {
		.mInputData = &cfuid,
		.mInputDataSize = sizeof(cfuid),
		.mOutputData = &device,
		.mOutputDataSize = sizeof(device),
	};
	UInt32 size = sizeof(translation);
	AudioObjectPropertyAddress address = {
		.mSelector = kAudioHardwarePropertyDeviceForUID,
		.mScope = kAudioObjectPropertyScopeGlobal,
		.mElement = kAudioObjectPropertyElementMain,
	};
	OSStatus status = AudioObjectGetPropertyData(kAudioObjectSystemObject, &address, 0, NULL, &size, &translation);
	CFRelease(cfuid);
	if(status != noErr)
	{
		fail(status, "AudioObjectGetPropertyData(kAudioHardwarePropertyDeviceForUID)");
	}
	if(device == kAudioObjectUnknown)
	{
		fprintf(stderr, "device UID %s not found\n", uid);
		exit(1);
	}
	return device;
}

static AudioStreamID firstStream(AudioDeviceID device, AudioObjectPropertyScope scope)
{
	AudioObjectPropertyAddress address = {
		.mSelector = kAudioDevicePropertyStreams,
		.mScope = scope,
		.mElement = kAudioObjectPropertyElementMain,
	};
	UInt32 size = 0;
	OSStatus status = AudioObjectGetPropertyDataSize(device, &address, 0, NULL, &size);
	if(status != noErr || size < sizeof(AudioStreamID))
	{
		return kAudioObjectUnknown;
	}
	AudioStreamID *streams = (AudioStreamID *)calloc(1, size);
	if(streams == NULL)
	{
		return kAudioObjectUnknown;
	}
	status = AudioObjectGetPropertyData(device, &address, 0, NULL, &size, streams);
	AudioStreamID result = (status == noErr) ? streams[0] : kAudioObjectUnknown;
	free(streams);
	return result;
}

static AudioStreamBasicDescription getFormat(AudioStreamID stream, AudioObjectPropertySelector selector)
{
	AudioStreamBasicDescription format = {0};
	UInt32 size = sizeof(format);
	AudioObjectPropertyAddress address = {
		.mSelector = selector,
		.mScope = kAudioObjectPropertyScopeGlobal,
		.mElement = kAudioObjectPropertyElementMain,
	};
	OSStatus status = AudioObjectGetPropertyData(stream, &address, 0, NULL, &size, &format);
	if(status != noErr)
	{
		fail(status, "AudioObjectGetPropertyData(format)");
	}
	return format;
}

static AudioStreamBasicDescription makeCaptureFormat(UInt32 channels)
{
	AudioStreamBasicDescription format = {0};
	format.mSampleRate = 192000.0;
	format.mFormatID = kAudioFormatLinearPCM;
	format.mFormatFlags = kAudioFormatFlagIsPacked | kAudioFormatFlagIsSignedInteger |
		kAudioFormatFlagsNativeEndian;
	format.mBytesPerPacket = channels * 2;
	format.mFramesPerPacket = 1;
	format.mBytesPerFrame = format.mBytesPerPacket;
	format.mChannelsPerFrame = channels;
	format.mBitsPerChannel = 16;
	return format;
}

static void printProbe(AudioDeviceID device)
{
	AudioObjectPropertyAddress address = {kSalsaHALProbeProperty, kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain};
	SalsaHALProbe probe = {0};
	CFDataRef data = NULL;
	UInt32 size = sizeof(data);
	OSStatus status = AudioObjectGetPropertyData(device, &address, 0, NULL, &size, &data);
	if(status == noErr && data && CFGetTypeID(data) == CFDataGetTypeID() && CFDataGetLength(data) == sizeof(probe))
		memcpy(&probe, CFDataGetBytePtr(data), sizeof(probe));
	else if(status == noErr)
		status = kAudioHardwareBadPropertySizeError;
	if(data) CFRelease(data);
	printf("driver probe: status=%d starts=%llu stops=%llu timestamps=%llu writes=%llu write_frames=%llu reads=%llu last_buffer=%llu dropped=%llu recorded_bytes=%llu record_overflow=%llu generation=%llu transport=%d\n",
		(int)status, probe.starts, probe.stops, probe.timestamps, probe.writes,
		probe.writeFrames, probe.reads, probe.lastBufferFrames, probe.droppedFrames,
		probe.recordedBytes, probe.recordOverflow, probe.generation, (int)probe.transportStatus);
}

static void prepareOutput(AudioStreamID stream, UInt32 channels)
{
	AudioObjectPropertyAddress address = {kAudioStreamPropertyAvailablePhysicalFormats, kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain};
	AudioStreamRangedDescription formats[2];
	UInt32 size = sizeof(formats);
	OSStatus status = AudioObjectGetPropertyData(stream, &address, 0, NULL, &size, formats);
	if(status != noErr) fail(status, "get available output formats");
	for(UInt32 i = 0; i < size / sizeof(formats[0]); ++i)
	{
		if(formats[i].mFormat.mChannelsPerFrame != channels) continue;
		address.mSelector = kAudioStreamPropertyPhysicalFormat;
		status = AudioObjectSetPropertyData(stream, &address, 0, NULL, sizeof(formats[i].mFormat), &formats[i].mFormat);
		if(status != noErr) fail(status, "prepare output format");
		for(int attempt = 0; attempt < 300; ++attempt)
		{
			if(getFormat(stream, kAudioStreamPropertyPhysicalFormat).mChannelsPerFrame == channels) return;
			usleep(10000);
		}
		fprintf(stderr, "output format change timed out\n");
		exit(1);
	}
	fprintf(stderr, "output channel count not available\n");
	exit(1);
}

static void recordCommand(AudioDeviceID device, bool start)
{
	AudioObjectPropertyAddress address = {kSalsaHALRecordProperty, kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain};
	CFBooleanRef command = start ? kCFBooleanTrue : kCFBooleanFalse;
	OSStatus status = AudioObjectSetPropertyData(device, &address, 0, NULL, sizeof(command), &command);
	if(status != noErr) fail(status, "record command (IO must be stopped)");
}

static void saveRecord(AudioDeviceID device, const char *path)
{
	AudioObjectPropertyAddress address = {kSalsaHALRecordProperty, kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain};
	CFDataRef data = NULL;
	UInt32 size = sizeof(data);
	OSStatus status = AudioObjectGetPropertyData(device, &address, 0, NULL, &size, &data);
	if(status != noErr) fail(status, "read record (IO must be stopped)");
	if(!data || CFGetTypeID(data) != CFDataGetTypeID()) fail(kAudioHardwareBadPropertySizeError, "record type");
	FILE *out = fopen(path, "wb");
	if(!out) { perror(path); exit(1); }
	size_t bytes = (size_t)CFDataGetLength(data);
	bool failed = fwrite(CFDataGetBytePtr(data), 1, bytes, out) != bytes;
	failed |= fclose(out) != 0;
	CFRelease(data);
	if(failed) { fprintf(stderr, "record write failed\n"); exit(1); }
	printf("saved driver record: %zu bytes\n", bytes);
	recordCommand(device, false);
}

static void printFormat(const char *label, AudioStreamBasicDescription format)
{
	printf("%s: %.0f Hz %u ch %c%c%c%c flags=%u bits=%u bytes/frame=%u bytes/packet=%u frames/packet=%u\n",
		label, format.mSampleRate, format.mChannelsPerFrame,
		(char)(format.mFormatID >> 24), (char)(format.mFormatID >> 16),
		(char)(format.mFormatID >> 8), (char)format.mFormatID,
		format.mFormatFlags, format.mBitsPerChannel, format.mBytesPerFrame,
		format.mBytesPerPacket, format.mFramesPerPacket);
}

struct captureContext {
	uint8_t *memory;
	size_t capacity;
	size_t used;
	uint32_t channels;
	uint32_t bytesPerFrame;
	int invalidBuffer;
	uint64_t callbacks;
	uint64_t firstSampleTime;
	uint64_t lastSampleTime;
	uint64_t firstHostTime;
	uint64_t lastHostTime;
	int haveTime;
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
	struct captureContext *ctx = (struct captureContext *)inClientData;

	if(inInputData == NULL || inInputData->mNumberBuffers == 0)
	{
		return noErr;
	}
	const AudioBuffer *buffer = &inInputData->mBuffers[0];
	if(buffer->mData == NULL || buffer->mDataByteSize == 0)
	{
		return noErr;
	}
	if(inInputData->mNumberBuffers != 1 || buffer->mNumberChannels != ctx->channels ||
		buffer->mDataByteSize % ctx->bytesPerFrame != 0)
	{
		ctx->invalidBuffer = 1;
		gStop = 1;
		return noErr;
	}

	if(!ctx->haveTime)
	{
		ctx->firstSampleTime = (uint64_t)inInputTime->mSampleTime;
		ctx->firstHostTime = inInputTime->mHostTime;
		ctx->haveTime = 1;
	}
	ctx->lastSampleTime = (uint64_t)inInputTime->mSampleTime;
	ctx->lastHostTime = inInputTime->mHostTime;
	ctx->callbacks += 1;

	{
		size_t room = ctx->capacity - ctx->used;
		size_t take = buffer->mDataByteSize;
		if(take > room)
		{
			take = room;
			gStop = 1;
		}
		memcpy(ctx->memory + ctx->used, buffer->mData, take);
		ctx->used += take;
	}
	return noErr;
}

static void usage(const char *argv0)
{
	fprintf(stderr,
		"usage: %s [options]\n"
		"  --device UID        HAL device UID (default: salsa-atmos)\n"
		"  --channels N        capture channels: 2 or 8, or 0 to follow output (default: 0)\n"
		"  --out PATH          output file (default: carrier.raw)\n"
		"  --memory BYTES      buffer in memory and write at exit (default: 536870912)\n"
		"  --probe             print stream formats and driver counters, then exit\n"
		"  --prepare-output N  set output to 2 or 8 channels before the probe\n"
		"  --record-start      arm a bounded driver recording while IO is stopped\n"
		"  --record-save PATH  save the driver recording after IO stops\n"
		"  --seconds N         stop after N seconds (default: 0 = until SIGINT)\n",
		argv0);
}

int main(int argc, char **argv)
{
	const char *uid = "salsa-atmos";
	const char *outPath = "carrier.raw";
	uint32_t requestedChannels = 0;
	size_t memoryBytes = 512u * 1024u * 1024u;
	int probeOnly = 0;
	UInt32 prepareChannels = 0;
	bool recordStart = false;
	const char *recordPath = NULL;
	double seconds = 0.0;

	for(int i = 1; i < argc; ++i)
	{
		if(strcmp(argv[i], "--device") == 0 && i + 1 < argc)
		{
			uid = argv[++i];
		}
		else if(strcmp(argv[i], "--channels") == 0 && i + 1 < argc)
		{
			requestedChannels = (uint32_t)strtoul(argv[++i], NULL, 0);
		}
		else if(strcmp(argv[i], "--out") == 0 && i + 1 < argc)
		{
			outPath = argv[++i];
		}
		else if(strcmp(argv[i], "--memory") == 0 && i + 1 < argc)
		{
			memoryBytes = (size_t)strtoull(argv[++i], NULL, 0);
		}
		else if(strcmp(argv[i], "--probe") == 0)
		{
			probeOnly = 1;
		}
		else if(strcmp(argv[i], "--prepare-output") == 0 && i + 1 < argc)
		{
			prepareChannels = (UInt32)strtoul(argv[++i], NULL, 10);
			probeOnly = 1;
		}
		else if(strcmp(argv[i], "--record-start") == 0)
		{
			recordStart = true;
			probeOnly = 1;
		}
		else if(strcmp(argv[i], "--record-save") == 0 && i + 1 < argc)
		{
			recordPath = argv[++i];
			probeOnly = 1;
		}
		else if(strcmp(argv[i], "--seconds") == 0 && i + 1 < argc)
		{
			seconds = strtod(argv[++i], NULL);
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

	signal(SIGINT, onSignal);
	signal(SIGTERM, onSignal);

	AudioDeviceID device = findDevice(uid);
	AudioStreamID outputStream = firstStream(device, kAudioObjectPropertyScopeOutput);
	AudioStreamID inputStream = firstStream(device, kAudioObjectPropertyScopeInput);
	if(prepareChannels) prepareOutput(outputStream, prepareChannels);
	if(recordStart) recordCommand(device, true);
	if(recordPath) saveRecord(device, recordPath);
	if(probeOnly)
	{
		if(outputStream != kAudioObjectUnknown)
		{
			printFormat("output physical", getFormat(outputStream, kAudioStreamPropertyPhysicalFormat));
			printFormat("output virtual", getFormat(outputStream, kAudioStreamPropertyVirtualFormat));
		}
		if(inputStream != kAudioObjectUnknown)
			printFormat("input virtual", getFormat(inputStream, kAudioStreamPropertyVirtualFormat));
		AudioObjectPropertyAddress address = {kAudioDevicePropertyBufferFrameSize, kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain};
		UInt32 size = sizeof(UInt32), frames = 0;
		OSStatus status = AudioObjectGetPropertyData(device, &address, 0, NULL, &size, &frames);
		printf("buffer: status=%d frames=%u\n", (int)status, frames);
		printProbe(device);
		return 0;
	}
	if(inputStream == kAudioObjectUnknown)
	{
		fprintf(stderr, "device has no input loopback stream\n");
		return 1;
	}

	AudioStreamBasicDescription outputFormat = {0};
	if(outputStream != kAudioObjectUnknown)
	{
		outputFormat = getFormat(outputStream, kAudioStreamPropertyVirtualFormat);
	}

	uint32_t channels = requestedChannels;
	if(channels == 0)
	{
		channels = outputFormat.mChannelsPerFrame;
	}
	if((channels != 2) && (channels != 8))
	{
		channels = 2;
	}

	AudioStreamBasicDescription captureFormat = makeCaptureFormat(channels);
	AudioObjectPropertyAddress formatAddress = {
		.mSelector = kAudioStreamPropertyPhysicalFormat,
		.mScope = kAudioObjectPropertyScopeGlobal,
		.mElement = kAudioObjectPropertyElementMain,
	};
	OSStatus status = AudioObjectSetPropertyData(inputStream, &formatAddress, 0, NULL, sizeof(captureFormat), &captureFormat);
	if(status != noErr)
	{
		fail(status, "set input physical format");
	}
	AudioStreamBasicDescription actualFormat = {0};
	for(int attempt = 0; attempt < 300; ++attempt)
	{
		actualFormat = getFormat(inputStream, kAudioStreamPropertyVirtualFormat);
		if(actualFormat.mChannelsPerFrame == channels) break;
		usleep(10000);
	}
	printFormat("actual capture format", actualFormat);
	AudioFormatFlags flags = actualFormat.mFormatFlags & ~kAudioFormatFlagIsNonMixable;
	bool captureFloat = flags == (kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked | kAudioFormatFlagsNativeEndian) && actualFormat.mBitsPerChannel == 32;
	bool captureInteger = flags == captureFormat.mFormatFlags && actualFormat.mBitsPerChannel == 16;
	if(actualFormat.mFormatID != kAudioFormatLinearPCM || actualFormat.mSampleRate != 192000 ||
		actualFormat.mChannelsPerFrame != channels || (!captureFloat && !captureInteger) ||
		actualFormat.mFramesPerPacket != 1 ||
		actualFormat.mBytesPerFrame != channels * (captureFloat ? 4u : 2u) ||
		actualFormat.mBytesPerPacket != actualFormat.mBytesPerFrame)
	{
		fprintf(stderr, "unsupported actual capture format\n");
		return 1;
	}

	UInt32 bufferFrameSize = 512;
	AudioObjectPropertyAddress bufferAddress = {
		.mSelector = kAudioDevicePropertyBufferFrameSize,
		.mScope = kAudioObjectPropertyScopeGlobal,
		.mElement = kAudioObjectPropertyElementMain,
	};
	(void)AudioObjectSetPropertyData(device, &bufferAddress, 0, NULL, sizeof(bufferFrameSize), &bufferFrameSize);
	UInt32 actualBufferFrameSize = 0;
	UInt32 bufferSize = sizeof(actualBufferFrameSize);
	(void)AudioObjectGetPropertyData(device, &bufferAddress, 0, NULL, &bufferSize, &actualBufferFrameSize);

	struct captureContext ctx;
	memset(&ctx, 0, sizeof(ctx));
	ctx.channels = channels;
	ctx.bytesPerFrame = actualFormat.mBytesPerFrame;
	{
		ctx.memory = (uint8_t *)malloc(memoryBytes);
		if(ctx.memory == NULL)
		{
			perror("malloc");
			return 1;
		}
		ctx.capacity = memoryBytes;
	}

	printf("device: %s\n", uid);
	printf("output carrier: %.0f Hz, %u ch, format %c%c%c%c, %u B/frame\n",
		outputFormat.mSampleRate, outputFormat.mChannelsPerFrame,
		(char)(outputFormat.mFormatID >> 24), (char)(outputFormat.mFormatID >> 16),
		(char)(outputFormat.mFormatID >> 8), (char)outputFormat.mFormatID,
		outputFormat.mBytesPerFrame);
	printf("input loopback: %.0f Hz, %u ch, format %c%c%c%c, %u B/frame, %u frames/callback\n",
		actualFormat.mSampleRate, actualFormat.mChannelsPerFrame,
		(char)(actualFormat.mFormatID >> 24), (char)(actualFormat.mFormatID >> 16),
		(char)(actualFormat.mFormatID >> 8), (char)actualFormat.mFormatID,
		actualFormat.mBytesPerFrame, actualBufferFrameSize);
	fflush(stdout);

	AudioDeviceIOProcID procID = NULL;
	status = AudioDeviceCreateIOProcID(device, captureIOProc, &ctx, &procID);
	if(status != noErr)
	{
		fail(status, "AudioDeviceCreateIOProcID");
	}
	status = AudioDeviceStart(device, procID);
	if(status != noErr)
	{
		printProbe(device);
		fail(status, "AudioDeviceStart");
	}
	printf("capture started\n");
	fflush(stdout);

	const uint64_t startHost = mach_absolute_time();
	while(!gStop)
	{
		usleep(100000);
		if(seconds > 0.0)
		{
			struct mach_timebase_info timebase;
			mach_timebase_info(&timebase);
			uint64_t now = mach_absolute_time();
			double elapsed = (double)(now - startHost) * (double)timebase.numer / (double)timebase.denom / 1e9;
			if(elapsed >= seconds)
			{
				break;
			}
		}
	}

	AudioDeviceStop(device, procID);
	AudioDeviceDestroyIOProcID(device, procID);
	printProbe(device);
	if(ctx.invalidBuffer)
	{
		fprintf(stderr, "input buffer does not match the negotiated format\n");
		free(ctx.memory);
		return 1;
	}

	size_t byteCount = ctx.used;
	if(captureFloat)
	{
		// Check each inverse conversion. Do not hide gain, remapping, or rounding.
		for(size_t i = 0; i < ctx.used / sizeof(float); ++i)
		{
			float value;
			memcpy(&value, ctx.memory + i * sizeof(float), sizeof(value));
			float word = value * 32768.0f;
			if(!isfinite(word) || word < -32768 || word > 32767 || truncf(word) != word)
			{
				fprintf(stderr, "non-exact S16 recovery at word %zu: %.9g\n", i, value);
				free(ctx.memory);
				return 1;
			}
			int16_t recovered = (int16_t)word;
			memcpy(ctx.memory + i * sizeof(recovered), &recovered, sizeof(recovered));
		}
		ctx.used /= 2;
	}
	{
		FILE *out = fopen(outPath, "wb");
		if(out == NULL)
		{
			perror(outPath);
			free(ctx.memory);
			return 1;
		}
		size_t written = fwrite(ctx.memory, 1, ctx.used, out);
		fclose(out);
		if(written != ctx.used)
		{
			fprintf(stderr, "short write: %zu/%zu\n", written, ctx.used);
			free(ctx.memory);
			return 1;
		}
		byteCount = ctx.used;
		free(ctx.memory);
	}

	printf("callbacks: %llu\n", (unsigned long long)ctx.callbacks);
	printf("frames: %llu\n", (unsigned long long)(byteCount / (channels * 2)));
	printf("bytes: %zu\n", byteCount);
	if(ctx.haveTime)
	{
		printf("first sample time: %llu, host time: %llu\n",
			(unsigned long long)ctx.firstSampleTime, (unsigned long long)ctx.firstHostTime);
		printf("last sample time: %llu, host time: %llu\n",
			(unsigned long long)ctx.lastSampleTime, (unsigned long long)ctx.lastHostTime);
	}
	return 0;
}
