/*
See the LICENSE.txt file for this sample’s licensing information.

Abstract:
A minimal user-space driver.
*/

/*==================================================================================================
	SalsaHAL.c
==================================================================================================*/

//==================================================================================================
//	Includes
//==================================================================================================

//	System Includes
#include <CoreAudio/AudioServerPlugIn.h>
#include <CoreAudio/AudioHardware.h>
#include <CoreAudio/AudioHardwareDeprecated.h>
#include <dispatch/dispatch.h>
#include <mach/mach_time.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syslog.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <fcntl.h>
#include <unistd.h>
#include <errno.h>
#include "salsa_carrier_shared.h"
#include "SalsaHALProbe.h"

//==================================================================================================
#pragma mark -
#pragma mark Macros
//==================================================================================================

#if TARGET_RT_BIG_ENDIAN
	#define	FourCCToCString(the4CC)	{ ((char*)&the4CC)[0], ((char*)&the4CC)[1], ((char*)&the4CC)[2], ((char*)&the4CC)[3], 0 }
#else
	#define	FourCCToCString(the4CC)	{ ((char*)&the4CC)[3], ((char*)&the4CC)[2], ((char*)&the4CC)[1], ((char*)&the4CC)[0], 0 }
#endif

#if DEBUG

	#define	DebugMsg(inFormat, ...)	printf(inFormat "\n", ## __VA_ARGS__)

	#define	FailIf(inCondition, inHandler, inMessage)									\
			if(inCondition)																\
			{																			\
				DebugMsg(inMessage);													\
				goto inHandler;															\
			}

	#define	FailWithAction(inCondition, inAction, inHandler, inMessage)					\
			if(inCondition)																\
			{																			\
				DebugMsg(inMessage);													\
				{ inAction; }															\
				goto inHandler;															\
			}

#else

	#define	DebugMsg(inFormat, ...)
	
	#define	FailIf(inCondition, inHandler, inMessage)									\
			if(inCondition)																\
			{																			\
				goto inHandler;															\
			}

	#define	FailWithAction(inCondition, inAction, inHandler, inMessage)					\
			if(inCondition)																\
			{																			\
				{ inAction; }															\
				goto inHandler;															\
			}

#endif

//==================================================================================================
#pragma mark -
#pragma mark SalsaHAL State
//==================================================================================================

//	The purpose of the SalsaHAL sample is to provide a bare bones implementations to
//	illustrate the minimal set of things a driver has to do. The sample driver has the following
//	qualities:
//	- a plug-in
//		- custom property with the selector kPlugIn_CustomPropertyID = 'PCst'
//	- a box
//	- a device
//		- supports 44100 and 48000 sample rates
//		- provides a rate scalar of 1.0 via hard coding
//	- a single input stream
//		- supports 2 channels of 32 bit float LPCM samples
//		- always produces zeros 
//	- a single output stream
//		- supports 2 channels of 32 bit float LPCM samples
//		- data written to it is ignored
//	- controls
//		- master input volume
//		- master output volume
//		- master input mute
//		- master output mute
//		- master input data source
//		- master output data source
//		- master play-through data destination
//		- all are for illustration purposes only and do not actually manipulate data


//	Declare the internal object ID numbers for all the objects this driver implements. Note that
//	this driver has a fixed set of objects that never grows or shrinks. If this were not the case,
//	the driver would need to have a means to dynamically allocate these IDs. It's important to
//	realize that a lot of the structure of this driver is vastly simpler when the IDs are all
//	known a priori. Comments in the code will try to identify some of these simplifications and
//	point out what a more complicated driver will need to do.
enum
{
	kObjectID_PlugIn					= kAudioObjectPlugInObject,
	kObjectID_Box						= 2,
	kObjectID_Device					= 3,
	kObjectID_Stream_Input				= 4,
	kObjectID_Volume_Input_Master		= 5,
	kObjectID_Mute_Input_Master			= 6,
	kObjectID_DataSource_Input_Master	= 7,
	kObjectID_Stream_Output				= 8,
	kObjectID_Volume_Output_Master		= 9,
	kObjectID_Mute_Output_Master		= 10,
	kObjectID_DataSource_Output_Master	= 11,
	kObjectID_DataDestination_PlayThru_Master	= 12
};

//	Declare the stuff that tracks the state of the plug-in, the device and its sub-objects.
//	Note that we use global variables here because this driver only ever has a single device. If
//	multiple devices were supported, this state would need to be encapsulated in one or more structs
//	so that each object's state can be tracked individually.
//	Note also that we share a single mutex across all objects to be thread safe for the same reason.
#ifndef SALSA_HAL_BUNDLE_ID
#define SALSA_HAL_BUNDLE_ID "dev.salsa.audio.SalsaHAL"
#endif
#define										kPlugIn_BundleID				SALSA_HAL_BUNDLE_ID
static pthread_mutex_t						gPlugIn_StateMutex				= PTHREAD_MUTEX_INITIALIZER;
static UInt32								gPlugIn_RefCount				= 0;
static AudioServerPlugInHostRef				gPlugIn_Host					= NULL;
static const AudioObjectPropertySelector	kPlugIn_CustomPropertyID		= 'PCst';

#ifndef SALSA_HAL_BOX_UID
#define SALSA_HAL_BOX_UID "salsa-atmos-box"
#endif
#ifndef SALSA_HAL_DEVICE_UID
#define SALSA_HAL_DEVICE_UID "salsa-atmos"
#endif
#ifndef SALSA_HAL_DEVICE_NAME
#define SALSA_HAL_DEVICE_NAME "Salsa Atmos"
#endif
#ifndef SALSA_HAL_MODEL_UID
#define SALSA_HAL_MODEL_UID "salsa-atmos-model"
#endif

#define										kBox_UID						SALSA_HAL_BOX_UID
static CFStringRef							gBox_Name						= NULL;
static Boolean								gBox_Acquired					= true;

#define										kDevice_UID						SALSA_HAL_DEVICE_UID
#define										kDevice_ModelUID				SALSA_HAL_MODEL_UID
static pthread_mutex_t						gDevice_IOMutex					= PTHREAD_MUTEX_INITIALIZER;
static Float64								gDevice_SampleRate				= 192000.0;
static UInt64								gDevice_IOIsRunning				= 0;
static const UInt32							kDevice_RingBufferSize			= 16384;
static const UInt32							kDefaultBufferFrames			= 512;
static const UInt32							kDevice_SafetyOffsetFrames		= 0;
#ifndef SALSA_HAL_LATENCY_FRAMES
#define SALSA_HAL_LATENCY_FRAMES 0
#endif
static const UInt32							kDevice_LatencyFrames			= SALSA_HAL_LATENCY_FRAMES;
static Float64								gDevice_HostTicksPerFrame		= 0.0;
static UInt64								gDevice_NumberTimeStamps		= 0;
static UInt64								gDevice_AnchorHostTime			= 0;

// Production carrier builds are output-only. The loopback input stream is a
// diagnostic path and must not present itself as a microphone.
#ifndef SALSA_HAL_OUTPUT_ONLY
#define SALSA_HAL_OUTPUT_ONLY 0
#endif
static pid_t								gHogModePID						= -1;
static bool									gStream_Input_IsActive			= !SALSA_HAL_OUTPUT_ONLY;
static bool									gStream_Output_IsActive			= true;

//	Carrier formats
//
//	mpv's coreaudio_exclusive SPDIF path requests kAudioFormat60958AC3 at 192 kHz
//	with 16-bit packed carrier words. E-AC-3 uses a two-channel carrier; TrueHD
//	uses the eight-channel MAT carrier. The payload bytes are opaque here; the
//	HAL must not mix, convert, or remap them.
#define										kSalsaHAL_CarrierRate			192000.0
#define										kSalsaHAL_CarrierBits			16
#define										kSalsaHAL_OutputChannelsMin		2
#define										kSalsaHAL_OutputChannelsMax		8
#define										kSalsaHAL_CaptureChannels		8
#ifndef SALSA_HAL_FIXED_CHANNELS
#define SALSA_HAL_FIXED_CHANNELS 0
#endif
#define kSalsaHAL_DefaultChannels (SALSA_HAL_FIXED_CHANNELS ? SALSA_HAL_FIXED_CHANNELS : 2)

//	Loopback capture ring. The output callback copies the opaque carrier words
//	here; the input stream (read by a separate capture client) drains them. The
//	ring is bounded and the I/O callbacks only do fixed-size copies and atomic
//	index updates.
#define										kCarrierRingFrames				192000
#define										kCarrierRingStrideBytes		(kSalsaHAL_CaptureChannels * 2)

static uint8_t								gCarrierRing[kCarrierRingFrames][kCarrierRingStrideBytes];
static _Atomic uint64_t						gCarrierWriteFrame				= 0;
static _Atomic uint64_t						gCarrierReadFrame				= 0;
static _Atomic uint64_t						gCarrierDroppedFrames			= 0;
static _Atomic uint32_t						gCarrierOutputChannels			= 2;
static _Atomic uint64_t						gSessionGeneration				= 0;

static AudioStreamBasicDescription			gStream_Output_Format;
static AudioStreamBasicDescription			gStream_Input_Format;

static _Atomic uint64_t gProbeStarts;
static _Atomic uint64_t gProbeStops;
static _Atomic uint64_t gProbeTimeStamps;
static _Atomic uint64_t gProbeWrites;
static _Atomic uint64_t gProbeWriteFrames;
static _Atomic uint64_t gProbeReads;
static _Atomic uint64_t gProbeLastBufferFrames;
// Test-only recording. Arm and read this buffer while IO is stopped. The
// independent loopback client is suspended when mpv owns the device in hog mode.
#define kProbeRecordCapacity (32u * 192000u * 16u)
static UInt8 *gProbeRecordData;
static _Atomic uint64_t gProbeRecordedBytes;
static _Atomic uint64_t gProbeRecordOverflow;

static SalsaCarrierShared *gCarrierShared;
static _Atomic int32_t gCarrierTransportStatus = -1;

static AudioStreamBasicDescription SalsaHAL_MakeCarrierFormat(UInt32 inChannels)
{
	AudioStreamBasicDescription theFormat = {0};
	theFormat.mSampleRate = kSalsaHAL_CarrierRate;
#ifdef SALSA_HAL_PCM_TEST
	theFormat.mFormatID = kAudioFormatLinearPCM;
	theFormat.mFormatFlags = kAudioFormatFlagIsPacked | kAudioFormatFlagIsSignedInteger | kAudioFormatFlagsNativeEndian;
#else
	theFormat.mFormatID = kAudioFormat60958AC3;
	theFormat.mFormatFlags = kAudioFormatFlagIsPacked | kAudioFormatFlagIsSignedInteger | kAudioFormatFlagIsNonMixable;
#endif
	theFormat.mBytesPerFrame = inChannels * (kSalsaHAL_CarrierBits / 8);
#if defined(SALSA_HAL_PCM_TEST) || defined(SALSA_HAL_FRAME_PACKETS)
	theFormat.mFramesPerPacket = 1;
#else
	// HAL derives the compressed IO quantum from the packet frame count.
	theFormat.mFramesPerPacket = inChannels == 2 ? 6144 : 3840;
#endif
	theFormat.mBytesPerPacket = theFormat.mBytesPerFrame * theFormat.mFramesPerPacket;
	theFormat.mChannelsPerFrame = inChannels;
	theFormat.mBitsPerChannel = kSalsaHAL_CarrierBits;
	return theFormat;
}

static AudioStreamBasicDescription SalsaHAL_MakeCaptureFormat(UInt32 inChannels)
{
	AudioStreamBasicDescription theFormat = {0};
	theFormat.mSampleRate = kSalsaHAL_CarrierRate;
	theFormat.mFormatID = kAudioFormatLinearPCM;
	theFormat.mFormatFlags = kAudioFormatFlagIsPacked | kAudioFormatFlagIsSignedInteger | kAudioFormatFlagsNativeEndian;
	theFormat.mBytesPerPacket = inChannels * 2;
	theFormat.mFramesPerPacket = 1;
	theFormat.mBytesPerFrame = theFormat.mBytesPerPacket;
	theFormat.mChannelsPerFrame = inChannels;
	theFormat.mBitsPerChannel = 16;
	return theFormat;
}

static bool SalsaHAL_IsSupportedOutputFormat(const AudioStreamBasicDescription* inFormat)
{
	if(inFormat == NULL)
	{
		return false;
	}
	if((inFormat->mChannelsPerFrame != 2) && (inFormat->mChannelsPerFrame != 8))
	{
		return false;
	}
	if(SALSA_HAL_FIXED_CHANNELS && inFormat->mChannelsPerFrame != SALSA_HAL_FIXED_CHANNELS)
	{
		return false;
	}
	AudioStreamBasicDescription theExpected = SalsaHAL_MakeCarrierFormat(inFormat->mChannelsPerFrame);
	const AudioFormatFlags theFlagMask = kAudioFormatFlagIsPacked | kAudioFormatFlagIsSignedInteger | kAudioFormatFlagIsFloat | kAudioFormatFlagIsBigEndian;
	return (inFormat->mFormatID == theExpected.mFormatID) &&
		   (inFormat->mSampleRate == theExpected.mSampleRate) &&
		   ((inFormat->mFormatFlags & theFlagMask) == (theExpected.mFormatFlags & theFlagMask)) &&
		   (inFormat->mBytesPerPacket == theExpected.mBytesPerPacket) &&
		   (inFormat->mFramesPerPacket == theExpected.mFramesPerPacket) &&
		   (inFormat->mBytesPerFrame == theExpected.mBytesPerFrame) &&
		   (inFormat->mBitsPerChannel == theExpected.mBitsPerChannel);
}

static bool SalsaHAL_IsSupportedCaptureFormat(const AudioStreamBasicDescription* inFormat)
{
	if(inFormat == NULL)
	{
		return false;
	}
	if(inFormat->mFormatID != kAudioFormatLinearPCM)
	{
		return false;
	}
	if((inFormat->mChannelsPerFrame != 2) && (inFormat->mChannelsPerFrame != 8))
	{
		return false;
	}
	AudioStreamBasicDescription theExpected = SalsaHAL_MakeCaptureFormat(inFormat->mChannelsPerFrame);
	return (inFormat->mSampleRate == theExpected.mSampleRate) &&
		   (inFormat->mFormatFlags == theExpected.mFormatFlags) &&
		   (inFormat->mBytesPerPacket == theExpected.mBytesPerPacket) &&
		   (inFormat->mFramesPerPacket == theExpected.mFramesPerPacket) &&
		   (inFormat->mBytesPerFrame == theExpected.mBytesPerFrame) &&
		   (inFormat->mBitsPerChannel == theExpected.mBitsPerChannel);
}

static UInt32 SalsaHAL_FormatCount(AudioObjectID stream)
{
	return SALSA_HAL_FIXED_CHANNELS ? (stream == kObjectID_Stream_Output ? 1 : 2) : 2;
}

static UInt32 SalsaHAL_CurrentChannels(AudioObjectPropertyScope scope)
{
	pthread_mutex_lock(&gPlugIn_StateMutex);
	UInt32 channels = scope == kAudioObjectPropertyScopeInput ?
		gStream_Input_Format.mChannelsPerFrame : gStream_Output_Format.mChannelsPerFrame;
	pthread_mutex_unlock(&gPlugIn_StateMutex);
	return channels;
}

static const Float32						kVolume_MinDB					= -96.0;
static const Float32						kVolume_MaxDB					= 6.0;
static Float32								gVolume_Input_Master_Value		= 0.0;
static Float32								gVolume_Output_Master_Value		= 0.0;

static bool									gMute_Input_Master_Value		= false;
static bool									gMute_Output_Master_Value		= false;

static const UInt32							kDataSource_NumberItems			= 4;
#define										kDataSource_ItemNamePattern		"Data Source Item %d"
static UInt32								gDataSource_Input_Master_Value	= 0;
static UInt32								gDataSource_Output_Master_Value	= 0;
static UInt32								gDataDestination_PlayThru_Master_Value	= 0;

//==================================================================================================
#pragma mark -
#pragma mark AudioServerPlugInDriverInterface Implementation
//==================================================================================================

#pragma mark Prototypes

//	Entry points for the COM methods
void*				SalsaHAL_Create(CFAllocatorRef inAllocator, CFUUIDRef inRequestedTypeUUID);
static HRESULT		SalsaHAL_QueryInterface(void* inDriver, REFIID inUUID, LPVOID* outInterface);
static ULONG		SalsaHAL_AddRef(void* inDriver);
static ULONG		SalsaHAL_Release(void* inDriver);
static OSStatus		SalsaHAL_Initialize(AudioServerPlugInDriverRef inDriver, AudioServerPlugInHostRef inHost);
static OSStatus		SalsaHAL_CreateDevice(AudioServerPlugInDriverRef inDriver, CFDictionaryRef inDescription, const AudioServerPlugInClientInfo* inClientInfo, AudioObjectID* outDeviceObjectID);
static OSStatus		SalsaHAL_DestroyDevice(AudioServerPlugInDriverRef inDriver, AudioObjectID inDeviceObjectID);
static OSStatus		SalsaHAL_AddDeviceClient(AudioServerPlugInDriverRef inDriver, AudioObjectID inDeviceObjectID, const AudioServerPlugInClientInfo* inClientInfo);
static OSStatus		SalsaHAL_RemoveDeviceClient(AudioServerPlugInDriverRef inDriver, AudioObjectID inDeviceObjectID, const AudioServerPlugInClientInfo* inClientInfo);
static OSStatus		SalsaHAL_PerformDeviceConfigurationChange(AudioServerPlugInDriverRef inDriver, AudioObjectID inDeviceObjectID, UInt64 inChangeAction, void* inChangeInfo);
static OSStatus		SalsaHAL_AbortDeviceConfigurationChange(AudioServerPlugInDriverRef inDriver, AudioObjectID inDeviceObjectID, UInt64 inChangeAction, void* inChangeInfo);
static Boolean		SalsaHAL_HasProperty(AudioServerPlugInDriverRef inDriver, AudioObjectID inObjectID, pid_t inClientProcessID, const AudioObjectPropertyAddress* inAddress);
static OSStatus		SalsaHAL_IsPropertySettable(AudioServerPlugInDriverRef inDriver, AudioObjectID inObjectID, pid_t inClientProcessID, const AudioObjectPropertyAddress* inAddress, Boolean* outIsSettable);
static OSStatus		SalsaHAL_GetPropertyDataSize(AudioServerPlugInDriverRef inDriver, AudioObjectID inObjectID, pid_t inClientProcessID, const AudioObjectPropertyAddress* inAddress, UInt32 inQualifierDataSize, const void* inQualifierData, UInt32* outDataSize);
static OSStatus		SalsaHAL_GetPropertyData(AudioServerPlugInDriverRef inDriver, AudioObjectID inObjectID, pid_t inClientProcessID, const AudioObjectPropertyAddress* inAddress, UInt32 inQualifierDataSize, const void* inQualifierData, UInt32 inDataSize, UInt32* outDataSize, void* outData);
static OSStatus		SalsaHAL_SetPropertyData(AudioServerPlugInDriverRef inDriver, AudioObjectID inObjectID, pid_t inClientProcessID, const AudioObjectPropertyAddress* inAddress, UInt32 inQualifierDataSize, const void* inQualifierData, UInt32 inDataSize, const void* inData);
static OSStatus		SalsaHAL_StartIO(AudioServerPlugInDriverRef inDriver, AudioObjectID inDeviceObjectID, UInt32 inClientID);
static OSStatus		SalsaHAL_StopIO(AudioServerPlugInDriverRef inDriver, AudioObjectID inDeviceObjectID, UInt32 inClientID);
static OSStatus		SalsaHAL_GetZeroTimeStamp(AudioServerPlugInDriverRef inDriver, AudioObjectID inDeviceObjectID, UInt32 inClientID, Float64* outSampleTime, UInt64* outHostTime, UInt64* outSeed);
static OSStatus		SalsaHAL_WillDoIOOperation(AudioServerPlugInDriverRef inDriver, AudioObjectID inDeviceObjectID, UInt32 inClientID, UInt32 inOperationID, Boolean* outWillDo, Boolean* outWillDoInPlace);
static OSStatus		SalsaHAL_BeginIOOperation(AudioServerPlugInDriverRef inDriver, AudioObjectID inDeviceObjectID, UInt32 inClientID, UInt32 inOperationID, UInt32 inIOBufferFrameSize, const AudioServerPlugInIOCycleInfo* inIOCycleInfo);
static OSStatus		SalsaHAL_DoIOOperation(AudioServerPlugInDriverRef inDriver, AudioObjectID inDeviceObjectID, AudioObjectID inStreamObjectID, UInt32 inClientID, UInt32 inOperationID, UInt32 inIOBufferFrameSize, const AudioServerPlugInIOCycleInfo* inIOCycleInfo, void* ioMainBuffer, void* ioSecondaryBuffer);
static OSStatus		SalsaHAL_EndIOOperation(AudioServerPlugInDriverRef inDriver, AudioObjectID inDeviceObjectID, UInt32 inClientID, UInt32 inOperationID, UInt32 inIOBufferFrameSize, const AudioServerPlugInIOCycleInfo* inIOCycleInfo);

//	Implementation
static Boolean		SalsaHAL_HasPlugInProperty(AudioServerPlugInDriverRef inDriver, AudioObjectID inObjectID, pid_t inClientProcessID, const AudioObjectPropertyAddress* inAddress);
static OSStatus		SalsaHAL_IsPlugInPropertySettable(AudioServerPlugInDriverRef inDriver, AudioObjectID inObjectID, pid_t inClientProcessID, const AudioObjectPropertyAddress* inAddress, Boolean* outIsSettable);
static OSStatus		SalsaHAL_GetPlugInPropertyDataSize(AudioServerPlugInDriverRef inDriver, AudioObjectID inObjectID, pid_t inClientProcessID, const AudioObjectPropertyAddress* inAddress, UInt32 inQualifierDataSize, const void* inQualifierData, UInt32* outDataSize);
static OSStatus		SalsaHAL_GetPlugInPropertyData(AudioServerPlugInDriverRef inDriver, AudioObjectID inObjectID, pid_t inClientProcessID, const AudioObjectPropertyAddress* inAddress, UInt32 inQualifierDataSize, const void* inQualifierData, UInt32 inDataSize, UInt32* outDataSize, void* outData);
static OSStatus		SalsaHAL_SetPlugInPropertyData(AudioServerPlugInDriverRef inDriver, AudioObjectID inObjectID, pid_t inClientProcessID, const AudioObjectPropertyAddress* inAddress, UInt32 inQualifierDataSize, const void* inQualifierData, UInt32 inDataSize, const void* inData, UInt32* outNumberPropertiesChanged, AudioObjectPropertyAddress outChangedAddresses[2]);

static Boolean		SalsaHAL_HasBoxProperty(AudioServerPlugInDriverRef inDriver, AudioObjectID inObjectID, pid_t inClientProcessID, const AudioObjectPropertyAddress* inAddress);
static OSStatus		SalsaHAL_IsBoxPropertySettable(AudioServerPlugInDriverRef inDriver, AudioObjectID inObjectID, pid_t inClientProcessID, const AudioObjectPropertyAddress* inAddress, Boolean* outIsSettable);
static OSStatus		SalsaHAL_GetBoxPropertyDataSize(AudioServerPlugInDriverRef inDriver, AudioObjectID inObjectID, pid_t inClientProcessID, const AudioObjectPropertyAddress* inAddress, UInt32 inQualifierDataSize, const void* inQualifierData, UInt32* outDataSize);
static OSStatus		SalsaHAL_GetBoxPropertyData(AudioServerPlugInDriverRef inDriver, AudioObjectID inObjectID, pid_t inClientProcessID, const AudioObjectPropertyAddress* inAddress, UInt32 inQualifierDataSize, const void* inQualifierData, UInt32 inDataSize, UInt32* outDataSize, void* outData);
static OSStatus		SalsaHAL_SetBoxPropertyData(AudioServerPlugInDriverRef inDriver, AudioObjectID inObjectID, pid_t inClientProcessID, const AudioObjectPropertyAddress* inAddress, UInt32 inQualifierDataSize, const void* inQualifierData, UInt32 inDataSize, const void* inData, UInt32* outNumberPropertiesChanged, AudioObjectPropertyAddress outChangedAddresses[2]);

static Boolean		SalsaHAL_HasDeviceProperty(AudioServerPlugInDriverRef inDriver, AudioObjectID inObjectID, pid_t inClientProcessID, const AudioObjectPropertyAddress* inAddress);
static OSStatus		SalsaHAL_IsDevicePropertySettable(AudioServerPlugInDriverRef inDriver, AudioObjectID inObjectID, pid_t inClientProcessID, const AudioObjectPropertyAddress* inAddress, Boolean* outIsSettable);
static OSStatus		SalsaHAL_GetDevicePropertyDataSize(AudioServerPlugInDriverRef inDriver, AudioObjectID inObjectID, pid_t inClientProcessID, const AudioObjectPropertyAddress* inAddress, UInt32 inQualifierDataSize, const void* inQualifierData, UInt32* outDataSize);
static OSStatus		SalsaHAL_GetDevicePropertyData(AudioServerPlugInDriverRef inDriver, AudioObjectID inObjectID, pid_t inClientProcessID, const AudioObjectPropertyAddress* inAddress, UInt32 inQualifierDataSize, const void* inQualifierData, UInt32 inDataSize, UInt32* outDataSize, void* outData);
static OSStatus		SalsaHAL_SetDevicePropertyData(AudioServerPlugInDriverRef inDriver, AudioObjectID inObjectID, pid_t inClientProcessID, const AudioObjectPropertyAddress* inAddress, UInt32 inQualifierDataSize, const void* inQualifierData, UInt32 inDataSize, const void* inData, UInt32* outNumberPropertiesChanged, AudioObjectPropertyAddress outChangedAddresses[2]);

static Boolean		SalsaHAL_HasStreamProperty(AudioServerPlugInDriverRef inDriver, AudioObjectID inObjectID, pid_t inClientProcessID, const AudioObjectPropertyAddress* inAddress);
static OSStatus		SalsaHAL_IsStreamPropertySettable(AudioServerPlugInDriverRef inDriver, AudioObjectID inObjectID, pid_t inClientProcessID, const AudioObjectPropertyAddress* inAddress, Boolean* outIsSettable);
static OSStatus		SalsaHAL_GetStreamPropertyDataSize(AudioServerPlugInDriverRef inDriver, AudioObjectID inObjectID, pid_t inClientProcessID, const AudioObjectPropertyAddress* inAddress, UInt32 inQualifierDataSize, const void* inQualifierData, UInt32* outDataSize);
static OSStatus		SalsaHAL_GetStreamPropertyData(AudioServerPlugInDriverRef inDriver, AudioObjectID inObjectID, pid_t inClientProcessID, const AudioObjectPropertyAddress* inAddress, UInt32 inQualifierDataSize, const void* inQualifierData, UInt32 inDataSize, UInt32* outDataSize, void* outData);
static OSStatus		SalsaHAL_SetStreamPropertyData(AudioServerPlugInDriverRef inDriver, AudioObjectID inObjectID, pid_t inClientProcessID, const AudioObjectPropertyAddress* inAddress, UInt32 inQualifierDataSize, const void* inQualifierData, UInt32 inDataSize, const void* inData, UInt32* outNumberPropertiesChanged, AudioObjectPropertyAddress outChangedAddresses[2]);

static Boolean		SalsaHAL_HasControlProperty(AudioServerPlugInDriverRef inDriver, AudioObjectID inObjectID, pid_t inClientProcessID, const AudioObjectPropertyAddress* inAddress);
static OSStatus		SalsaHAL_IsControlPropertySettable(AudioServerPlugInDriverRef inDriver, AudioObjectID inObjectID, pid_t inClientProcessID, const AudioObjectPropertyAddress* inAddress, Boolean* outIsSettable);
static OSStatus		SalsaHAL_GetControlPropertyDataSize(AudioServerPlugInDriverRef inDriver, AudioObjectID inObjectID, pid_t inClientProcessID, const AudioObjectPropertyAddress* inAddress, UInt32 inQualifierDataSize, const void* inQualifierData, UInt32* outDataSize);
static OSStatus		SalsaHAL_GetControlPropertyData(AudioServerPlugInDriverRef inDriver, AudioObjectID inObjectID, pid_t inClientProcessID, const AudioObjectPropertyAddress* inAddress, UInt32 inQualifierDataSize, const void* inQualifierData, UInt32 inDataSize, UInt32* outDataSize, void* outData);
static OSStatus		SalsaHAL_SetControlPropertyData(AudioServerPlugInDriverRef inDriver, AudioObjectID inObjectID, pid_t inClientProcessID, const AudioObjectPropertyAddress* inAddress, UInt32 inQualifierDataSize, const void* inQualifierData, UInt32 inDataSize, const void* inData, UInt32* outNumberPropertiesChanged, AudioObjectPropertyAddress outChangedAddresses[2]);

#pragma mark The Interface

static AudioServerPlugInDriverInterface	gAudioServerPlugInDriverInterface =
{
	NULL,
	SalsaHAL_QueryInterface,
	SalsaHAL_AddRef,
	SalsaHAL_Release,
	SalsaHAL_Initialize,
	SalsaHAL_CreateDevice,
	SalsaHAL_DestroyDevice,
	SalsaHAL_AddDeviceClient,
	SalsaHAL_RemoveDeviceClient,
	SalsaHAL_PerformDeviceConfigurationChange,
	SalsaHAL_AbortDeviceConfigurationChange,
	SalsaHAL_HasProperty,
	SalsaHAL_IsPropertySettable,
	SalsaHAL_GetPropertyDataSize,
	SalsaHAL_GetPropertyData,
	SalsaHAL_SetPropertyData,
	SalsaHAL_StartIO,
	SalsaHAL_StopIO,
	SalsaHAL_GetZeroTimeStamp,
	SalsaHAL_WillDoIOOperation,
	SalsaHAL_BeginIOOperation,
	SalsaHAL_DoIOOperation,
	SalsaHAL_EndIOOperation
};
static AudioServerPlugInDriverInterface*	gAudioServerPlugInDriverInterfacePtr	= &gAudioServerPlugInDriverInterface;
static AudioServerPlugInDriverRef			gAudioServerPlugInDriverRef				= &gAudioServerPlugInDriverInterfacePtr;

#pragma mark Factory

void*	SalsaHAL_Create(CFAllocatorRef inAllocator, CFUUIDRef inRequestedTypeUUID)
{
	//	This is the CFPlugIn factory function. Its job is to create the implementation for the given
	//	type provided that the type is supported. Because this driver is simple and all its
	//	initialization is handled via static iniitalization when the bundle is loaded, all that
	//	needs to be done is to return the AudioServerPlugInDriverRef that points to the driver's
	//	interface. A more complicated driver would create any base line objects it needs to satisfy
	//	the IUnknown methods that are used to discover that actual interface to talk to the driver.
	//	The majority of the driver's initilization should be handled in the Initialize() method of
	//	the driver's AudioServerPlugInDriverInterface.
	
	#pragma unused(inAllocator)
    void* theAnswer = NULL;
    if(CFEqual(inRequestedTypeUUID, kAudioServerPlugInTypeUUID))
    {
		theAnswer = gAudioServerPlugInDriverRef;
    }
    return theAnswer;
}

#pragma mark Inheritence

static HRESULT	SalsaHAL_QueryInterface(void* inDriver, REFIID inUUID, LPVOID* outInterface)
{
	//	This function is called by the HAL to get the interface to talk to the plug-in through.
	//	AudioServerPlugIns are required to support the IUnknown interface and the
	//	AudioServerPlugInDriverInterface. As it happens, all interfaces must also provide the
	//	IUnknown interface, so we can always just return the single interface we made with
	//	gAudioServerPlugInDriverInterfacePtr regardless of which one is asked for.

	//	declare the local variables
	HRESULT theAnswer = 0;
	CFUUIDRef theRequestedUUID = NULL;
	
	//	validate the arguments
	FailWithAction(inDriver != gAudioServerPlugInDriverRef, theAnswer = kAudioHardwareBadObjectError, Done, "SalsaHAL_QueryInterface: bad driver reference");
	FailWithAction(outInterface == NULL, theAnswer = kAudioHardwareIllegalOperationError, Done, "SalsaHAL_QueryInterface: no place to store the returned interface");

	//	make a CFUUIDRef from inUUID
	theRequestedUUID = CFUUIDCreateFromUUIDBytes(NULL, inUUID);
	FailWithAction(theRequestedUUID == NULL, theAnswer = kAudioHardwareIllegalOperationError, Done, "SalsaHAL_QueryInterface: failed to create the CFUUIDRef");

	//	AudioServerPlugIns only support two interfaces, IUnknown (which has to be supported by all
	//	CFPlugIns and AudioServerPlugInDriverInterface (which is the actual interface the HAL will
	//	use).
	if(CFEqual(theRequestedUUID, IUnknownUUID) || CFEqual(theRequestedUUID, kAudioServerPlugInDriverInterfaceUUID))
	{
		pthread_mutex_lock(&gPlugIn_StateMutex);
		++gPlugIn_RefCount;
		pthread_mutex_unlock(&gPlugIn_StateMutex);
		*outInterface = gAudioServerPlugInDriverRef;
	}
	else
	{
		theAnswer = E_NOINTERFACE;
	}
	
	//	make sure to release the UUID we created
	CFRelease(theRequestedUUID);
		
Done:
	return theAnswer;
}

static ULONG	SalsaHAL_AddRef(void* inDriver)
{
	//	This call returns the resulting reference count after the increment.
	
	//	declare the local variables
	ULONG theAnswer = 0;
	
	//	check the arguments
	FailIf(inDriver != gAudioServerPlugInDriverRef, Done, "SalsaHAL_AddRef: bad driver reference");

	//	decrement the refcount
	pthread_mutex_lock(&gPlugIn_StateMutex);
	if(gPlugIn_RefCount < UINT32_MAX)
	{
		++gPlugIn_RefCount;
	}
	theAnswer = gPlugIn_RefCount;
	pthread_mutex_unlock(&gPlugIn_StateMutex);

Done:
	return theAnswer;
}

static ULONG	SalsaHAL_Release(void* inDriver)
{
	//	This call returns the resulting reference count after the decrement.

	//	declare the local variables
	ULONG theAnswer = 0;
	
	//	check the arguments
	FailIf(inDriver != gAudioServerPlugInDriverRef, Done, "SalsaHAL_Release: bad driver reference");

	//	increment the refcount
	pthread_mutex_lock(&gPlugIn_StateMutex);
	if(gPlugIn_RefCount > 0)
	{
		--gPlugIn_RefCount;
		//	Note that we don't do anything special if the refcount goes to zero as the HAL
		//	will never fully release a plug-in it opens. We keep managing the refcount so that
		//	the API semantics are correct though.
	}
	theAnswer = gPlugIn_RefCount;
	pthread_mutex_unlock(&gPlugIn_StateMutex);

Done:
	return theAnswer;
}

#pragma mark Basic Operations

static void SalsaHAL_TransportOpen(void)
{
	if(gCarrierShared != NULL)
	{
		return;
	}
	int theFD = shm_open(SALSA_CARRIER_SHM_NAME, O_CREAT | O_RDWR, 0666);
	if(theFD < 0)
	{
		atomic_store(&gCarrierTransportStatus, errno);
		return;
	}
	struct stat theStat;
	if(fstat(theFD, &theStat) != 0 || theStat.st_size < (off_t)sizeof(SalsaCarrierShared))
	{
		if(ftruncate(theFD, sizeof(SalsaCarrierShared)) != 0)
		{
			atomic_store(&gCarrierTransportStatus, errno);
			close(theFD);
			return;
		}
	}
	void *theMap = mmap(NULL, sizeof(SalsaCarrierShared), PROT_READ | PROT_WRITE, MAP_SHARED, theFD, 0);
	if(theMap == MAP_FAILED)
	{
		atomic_store(&gCarrierTransportStatus, errno);
		close(theFD);
		return;
	}
	SalsaCarrierShared *theShared = (SalsaCarrierShared *)theMap;
	if(atomic_load(&theShared->control.magic) != SALSA_CARRIER_SHM_MAGIC ||
	   atomic_load(&theShared->control.version) != SALSA_CARRIER_SHM_VERSION)
	{
		SalsaCarrierInitialize(theShared);
	}
	gCarrierShared = theShared;
	close(theFD);
	atomic_store(&gCarrierTransportStatus, 0);
}

static UInt32 SalsaHAL_DeclaredLatencyFrames(void)
{
	if(gCarrierShared != NULL && atomic_load(&gCarrierShared->control.latency_valid))
	{
		uint64_t theFrames = atomic_load(&gCarrierShared->control.declared_latency_frames);
		if(theFrames <= UINT32_MAX)
		{
			return (UInt32)theFrames;
		}
	}
	return kDevice_LatencyFrames;
}

// Retain a gap until a later slot can publish it to the consumer.
static _Atomic UInt32 gCarrierPendingFlags = 0;

static void SalsaHAL_TransportReset(void)
{
	if(gCarrierShared == NULL)
	{
		return;
	}
	// Keep ring indices monotonic. Only the consumer advances tail, so a
	// session change cannot overwrite a slot while the consumer copies it.
	atomic_store(&gCarrierShared->control.dropped_frames, 0);
	atomic_fetch_or(&gCarrierPendingFlags, SALSA_CARRIER_FLAG_DISCONTINUITY);
	atomic_store(&gCarrierShared->control.generation, atomic_load(&gSessionGeneration));
	atomic_store(&gCarrierShared->control.output_channels, atomic_load(&gCarrierOutputChannels));
	atomic_store(&gCarrierShared->control.bytes_per_frame, atomic_load(&gCarrierOutputChannels) * 2);
	atomic_store(&gCarrierShared->control.buffer_frames, kDevice_RingBufferSize);
}

static void SalsaHAL_TransportWrite(const AudioServerPlugInIOCycleInfo *inCycleInfo, UInt32 inFrames, UInt32 inChannels, const UInt8 *inPayload)
{
	if(gCarrierShared == NULL || inCycleInfo == NULL || inPayload == NULL)
	{
		return;
	}
	size_t theBytes = (size_t)inFrames * inChannels * 2;
	if(theBytes > SALSA_CARRIER_SLOT_BYTES)
	{
		atomic_fetch_add(&gCarrierShared->control.dropped_frames, inFrames);
		atomic_fetch_or(&gCarrierPendingFlags, SALSA_CARRIER_FLAG_DROPPED);
		return;
	}
	uint64_t theHead = atomic_load_explicit(&gCarrierShared->control.head, memory_order_relaxed);
	uint64_t theTail = atomic_load_explicit(&gCarrierShared->control.tail, memory_order_acquire);
	if(theHead - theTail >= SALSA_CARRIER_SLOT_COUNT)
	{
		atomic_fetch_add(&gCarrierShared->control.dropped_frames, inFrames);
		atomic_fetch_or(&gCarrierPendingFlags, SALSA_CARRIER_FLAG_DROPPED);
		return;
	}
	SalsaCarrierSlot *theSlot = &gCarrierShared->slots[theHead % SALSA_CARRIER_SLOT_COUNT];
	theSlot->header.generation = atomic_load_explicit(&gCarrierShared->control.generation, memory_order_relaxed);
	theSlot->header.start_sample = (uint64_t)inCycleInfo->mOutputTime.mSampleTime;
	theSlot->header.host_time = inCycleInfo->mOutputTime.mHostTime;
	theSlot->header.frames = inFrames;
	theSlot->header.channels = inChannels;
	theSlot->header.bytes_per_frame = inChannels * 2;
	theSlot->header.payload_bytes = (uint32_t)theBytes;
	theSlot->header.flags = atomic_exchange(&gCarrierPendingFlags, 0);
	theSlot->header.reserved = 0;
	memcpy(theSlot->payload, inPayload, theBytes);
	atomic_store_explicit(&gCarrierShared->control.head, theHead + 1, memory_order_release);
}

static OSStatus	SalsaHAL_Initialize(AudioServerPlugInDriverRef inDriver, AudioServerPlugInHostRef inHost)
{
	//	The job of this method is, as the name implies, to get the driver initialized. One specific
	//	thing that needs to be done is to store the AudioServerPlugInHostRef so that it can be used
	//	later. Note that when this call returns, the HAL will scan the various lists the driver
	//	maintains (such as the device list) to get the inital set of objects the driver is
	//	publishing. So, there is no need to notifiy the HAL about any objects created as part of the
	//	execution of this method.

	//	declare the local variables
	OSStatus theAnswer = 0;
	
	//	check the arguments
	FailWithAction(inDriver != gAudioServerPlugInDriverRef, theAnswer = kAudioHardwareBadObjectError, Done, "SalsaHAL_Initialize: bad driver reference");
	
	//	store the AudioServerPlugInHostRef
	gPlugIn_Host = inHost;
	
	//	initialize the box acquired property from the settings
	CFPropertyListRef theSettingsData = NULL;
	gPlugIn_Host->CopyFromStorage(gPlugIn_Host, CFSTR("box acquired"), &theSettingsData);
	if(theSettingsData != NULL)
	{
		if(CFGetTypeID(theSettingsData) == CFBooleanGetTypeID())
		{
			gBox_Acquired = CFBooleanGetValue((CFBooleanRef)theSettingsData);
		}
		else if(CFGetTypeID(theSettingsData) == CFNumberGetTypeID())
		{
			SInt32 theValue = 0;
			CFNumberGetValue((CFNumberRef)theSettingsData, kCFNumberSInt32Type, &theValue);
			gBox_Acquired = theValue ? 1 : 0;
		}
		CFRelease(theSettingsData);
	}
	
	//	initialize the box name from the settings
	gPlugIn_Host->CopyFromStorage(gPlugIn_Host, CFSTR("box acquired"), &theSettingsData);
	if(theSettingsData != NULL)
	{
		if(CFGetTypeID(theSettingsData) == CFStringGetTypeID())
		{
			gBox_Name = (CFStringRef)theSettingsData;
			CFRetain(gBox_Name);
		}
		CFRelease(theSettingsData);
	}
	
	//	set the box name directly as a last resort
	if(gBox_Name == NULL)
	{
		gBox_Name = CFSTR("Salsa");
	}

	// The fixed TrueHD build starts at eight channels. Other builds start at
	// two channels. The diagnostic input initially exposes eight channels.
	gStream_Output_Format = SalsaHAL_MakeCarrierFormat(kSalsaHAL_DefaultChannels);
	gStream_Input_Format = SalsaHAL_MakeCaptureFormat(kSalsaHAL_CaptureChannels);
	atomic_store(&gCarrierOutputChannels, kSalsaHAL_DefaultChannels);
	atomic_store(&gCarrierWriteFrame, 0);
	atomic_store(&gCarrierReadFrame, 0);
	atomic_store(&gCarrierDroppedFrames, 0);
	atomic_store(&gSessionGeneration, 0);
	
	SalsaHAL_TransportOpen();

	//	calculate the host ticks per frame
	struct mach_timebase_info theTimeBaseInfo;
	mach_timebase_info(&theTimeBaseInfo);
	Float64 theHostClockFrequency = (Float64)theTimeBaseInfo.denom / (Float64)theTimeBaseInfo.numer;
	theHostClockFrequency *= 1000000000.0;
	gDevice_HostTicksPerFrame = theHostClockFrequency / gDevice_SampleRate;
	
Done:
	return theAnswer;
}

static OSStatus	SalsaHAL_CreateDevice(AudioServerPlugInDriverRef inDriver, CFDictionaryRef inDescription, const AudioServerPlugInClientInfo* inClientInfo, AudioObjectID* outDeviceObjectID)
{
	//	This method is used to tell a driver that implements the Transport Manager semantics to
	//	create an AudioEndpointDevice from a set of AudioEndpoints. Since this driver is not a
	//	Transport Manager, we just check the arguments and return
	//	kAudioHardwareUnsupportedOperationError.
	
	#pragma unused(inDescription, inClientInfo, outDeviceObjectID)
	
	//	declare the local variables
	OSStatus theAnswer = kAudioHardwareUnsupportedOperationError;
	
	//	check the arguments
	FailWithAction(inDriver != gAudioServerPlugInDriverRef, theAnswer = kAudioHardwareBadObjectError, Done, "SalsaHAL_CreateDevice: bad driver reference");

Done:
	return theAnswer;
}

static OSStatus	SalsaHAL_DestroyDevice(AudioServerPlugInDriverRef inDriver, AudioObjectID inDeviceObjectID)
{
	//	This method is used to tell a driver that implements the Transport Manager semantics to
	//	destroy an AudioEndpointDevice. Since this driver is not a Transport Manager, we just check
	//	the arguments and return kAudioHardwareUnsupportedOperationError.
	
	#pragma unused(inDeviceObjectID)
	
	//	declare the local variables
	OSStatus theAnswer = kAudioHardwareUnsupportedOperationError;
	
	//	check the arguments
	FailWithAction(inDriver != gAudioServerPlugInDriverRef, theAnswer = kAudioHardwareBadObjectError, Done, "SalsaHAL_DestroyDevice: bad driver reference");

Done:
	return theAnswer;
}

static OSStatus	SalsaHAL_AddDeviceClient(AudioServerPlugInDriverRef inDriver, AudioObjectID inDeviceObjectID, const AudioServerPlugInClientInfo* inClientInfo)
{
	//	This method is used to inform the driver about a new client that is using the given device.
	//	This allows the device to act differently depending on who the client is. This driver does
	//	not need to track the clients using the device, so we just check the arguments and return
	//	successfully.
	
	#pragma unused(inClientInfo)
	
	//	declare the local variables
	OSStatus theAnswer = 0;
	
	//	check the arguments
	FailWithAction(inDriver != gAudioServerPlugInDriverRef, theAnswer = kAudioHardwareBadObjectError, Done, "SalsaHAL_AddDeviceClient: bad driver reference");
	FailWithAction(inDeviceObjectID != kObjectID_Device, theAnswer = kAudioHardwareBadObjectError, Done, "SalsaHAL_AddDeviceClient: bad device ID");

Done:
	return theAnswer;
}

static OSStatus	SalsaHAL_RemoveDeviceClient(AudioServerPlugInDriverRef inDriver, AudioObjectID inDeviceObjectID, const AudioServerPlugInClientInfo* inClientInfo)
{
	//	This method is used to inform the driver about a client that is no longer using the given
	//	device. This driver does not track clients, so we just check the arguments and return
	//	successfully.
	
	#pragma unused(inClientInfo)
	
	//	declare the local variables
	OSStatus theAnswer = 0;
	
	//	check the arguments
	FailWithAction(inDriver != gAudioServerPlugInDriverRef, theAnswer = kAudioHardwareBadObjectError, Done, "SalsaHAL_RemoveDeviceClient: bad driver reference");
	FailWithAction(inDeviceObjectID != kObjectID_Device, theAnswer = kAudioHardwareBadObjectError, Done, "SalsaHAL_RemoveDeviceClient: bad device ID");

Done:
	return theAnswer;
}

static OSStatus	SalsaHAL_PerformDeviceConfigurationChange(AudioServerPlugInDriverRef inDriver, AudioObjectID inDeviceObjectID, UInt64 inChangeAction, void* inChangeInfo)
{
	//	This method is called to tell the device that it can perform the configuation change that it
	//	had requested via a call to the host method, RequestDeviceConfigurationChange(). The
	//	arguments, inChangeAction and inChangeInfo are the same as what was passed to
	//	RequestDeviceConfigurationChange().
	//
	//	The HAL guarantees that IO will be stopped while this method is in progress. The HAL will
	//	also handle figuring out exactly what changed for the non-control related properties. This
	//	means that the only notifications that would need to be sent here would be for either
	//	custom properties the HAL doesn't know about or for controls.
	//
	// Stream changes encode the stream ID in the upper word and the channel
	// count in the lower word. No IO format changes before this callback.
	
	#pragma unused(inChangeInfo)

	//	declare the local variables
	OSStatus theAnswer = 0;
	
	//	check the arguments
	FailWithAction(inDriver != gAudioServerPlugInDriverRef, theAnswer = kAudioHardwareBadObjectError, Done, "SalsaHAL_PerformDeviceConfigurationChange: bad driver reference");
	FailWithAction(inDeviceObjectID != kObjectID_Device, theAnswer = kAudioHardwareBadObjectError, Done, "SalsaHAL_PerformDeviceConfigurationChange: bad device ID");
	AudioObjectID stream = (AudioObjectID)(inChangeAction >> 32);
	UInt32 channels = (UInt32)inChangeAction;
	if(inChangeAction != 192000)
	{
		FailWithAction(stream != kObjectID_Stream_Input && stream != kObjectID_Stream_Output, theAnswer = kAudioHardwareBadObjectError, Done, "SalsaHAL_PerformDeviceConfigurationChange: bad stream");
		FailWithAction(channels != 2 && channels != 8, theAnswer = kAudioDeviceUnsupportedFormatError, Done, "SalsaHAL_PerformDeviceConfigurationChange: bad channels");
	}
	
	//	lock the state mutex
	pthread_mutex_lock(&gPlugIn_StateMutex);
	
	//	change the sample rate
	gDevice_SampleRate = kSalsaHAL_CarrierRate;
	if(stream == kObjectID_Stream_Input)
	{
		gStream_Input_Format = SalsaHAL_MakeCaptureFormat(channels);
	}
	else if(stream == kObjectID_Stream_Output)
	{
		gStream_Output_Format = SalsaHAL_MakeCarrierFormat(channels);
		atomic_store(&gCarrierOutputChannels, channels);
		// A format change is a new transport session. Publish a new generation
		// so the service flushes queued decode/render state instead of mixing
		// two carrier formats in one timeline.
		atomic_fetch_add(&gSessionGeneration, 1);
		SalsaHAL_TransportReset();
	}
	atomic_store(&gCarrierWriteFrame, 0);
	atomic_store(&gCarrierReadFrame, 0);
	
	//	recalculate the state that depends on the sample rate
	struct mach_timebase_info theTimeBaseInfo;
	mach_timebase_info(&theTimeBaseInfo);
	Float64 theHostClockFrequency = (Float64)theTimeBaseInfo.denom / (Float64)theTimeBaseInfo.numer;
	theHostClockFrequency *= 1000000000.0;
	gDevice_HostTicksPerFrame = theHostClockFrequency / gDevice_SampleRate;

	//	unlock the state mutex
	pthread_mutex_unlock(&gPlugIn_StateMutex);
	
Done:
	return theAnswer;
}

static OSStatus	SalsaHAL_AbortDeviceConfigurationChange(AudioServerPlugInDriverRef inDriver, AudioObjectID inDeviceObjectID, UInt64 inChangeAction, void* inChangeInfo)
{
	//	This method is called to tell the driver that a request for a config change has been denied.
	//	This provides the driver an opportunity to clean up any state associated with the request.
	//	For this driver, an aborted config change requires no action. So we just check the arguments
	//	and return

	#pragma unused(inChangeAction, inChangeInfo)

	//	declare the local variables
	OSStatus theAnswer = 0;
	
	//	check the arguments
	FailWithAction(inDriver != gAudioServerPlugInDriverRef, theAnswer = kAudioHardwareBadObjectError, Done, "SalsaHAL_PerformDeviceConfigurationChange: bad driver reference");
	FailWithAction(inDeviceObjectID != kObjectID_Device, theAnswer = kAudioHardwareBadObjectError, Done, "SalsaHAL_PerformDeviceConfigurationChange: bad device ID");

Done:
	return theAnswer;
}

#pragma mark Property Operations

static Boolean	SalsaHAL_HasProperty(AudioServerPlugInDriverRef inDriver, AudioObjectID inObjectID, pid_t inClientProcessID, const AudioObjectPropertyAddress* inAddress)
{
	//	This method returns whether or not the given object has the given property.
	
	//	declare the local variables
	Boolean theAnswer = false;
	
	//	check the arguments
	FailIf(inDriver != gAudioServerPlugInDriverRef, Done, "SalsaHAL_HasProperty: bad driver reference");
	FailIf(inAddress == NULL, Done, "SalsaHAL_HasProperty: no address");
	
	//	Note that for each object, this driver implements all the required properties plus a few
	//	extras that are useful but not required. There is more detailed commentary about each
	//	property in the SalsaHAL_GetPropertyData() method.
	switch(inObjectID)
	{
		case kObjectID_PlugIn:
			theAnswer = SalsaHAL_HasPlugInProperty(inDriver, inObjectID, inClientProcessID, inAddress);
			break;
		
		case kObjectID_Box:
			theAnswer = SalsaHAL_HasBoxProperty(inDriver, inObjectID, inClientProcessID, inAddress);
			break;
		
		case kObjectID_Device:
			theAnswer = SalsaHAL_HasDeviceProperty(inDriver, inObjectID, inClientProcessID, inAddress);
			break;
		
		case kObjectID_Stream_Input:
		case kObjectID_Stream_Output:
			theAnswer = SalsaHAL_HasStreamProperty(inDriver, inObjectID, inClientProcessID, inAddress);
			break;
		
		case kObjectID_Volume_Input_Master:
		case kObjectID_Volume_Output_Master:
		case kObjectID_Mute_Input_Master:
		case kObjectID_Mute_Output_Master:
		case kObjectID_DataSource_Input_Master:
		case kObjectID_DataSource_Output_Master:
		case kObjectID_DataDestination_PlayThru_Master:
			theAnswer = SalsaHAL_HasControlProperty(inDriver, inObjectID, inClientProcessID, inAddress);
			break;
	};

Done:
	return theAnswer;
}

static OSStatus	SalsaHAL_IsPropertySettable(AudioServerPlugInDriverRef inDriver, AudioObjectID inObjectID, pid_t inClientProcessID, const AudioObjectPropertyAddress* inAddress, Boolean* outIsSettable)
{
	//	This method returns whether or not the given property on the object can have its value
	//	changed.
	
	//	declare the local variables
	OSStatus theAnswer = 0;
	
	//	check the arguments
	FailWithAction(inDriver != gAudioServerPlugInDriverRef, theAnswer = kAudioHardwareBadObjectError, Done, "SalsaHAL_IsPropertySettable: bad driver reference");
	FailWithAction(inAddress == NULL, theAnswer = kAudioHardwareIllegalOperationError, Done, "SalsaHAL_IsPropertySettable: no address");
	FailWithAction(outIsSettable == NULL, theAnswer = kAudioHardwareIllegalOperationError, Done, "SalsaHAL_IsPropertySettable: no place to put the return value");
	
	//	Note that for each object, this driver implements all the required properties plus a few
	//	extras that are useful but not required. There is more detailed commentary about each
	//	property in the SalsaHAL_GetPropertyData() method.
	switch(inObjectID)
	{
		case kObjectID_PlugIn:
			theAnswer = SalsaHAL_IsPlugInPropertySettable(inDriver, inObjectID, inClientProcessID, inAddress, outIsSettable);
			break;
		
		case kObjectID_Box:
			theAnswer = SalsaHAL_IsBoxPropertySettable(inDriver, inObjectID, inClientProcessID, inAddress, outIsSettable);
			break;
		
		case kObjectID_Device:
			theAnswer = SalsaHAL_IsDevicePropertySettable(inDriver, inObjectID, inClientProcessID, inAddress, outIsSettable);
			break;
		
		case kObjectID_Stream_Input:
		case kObjectID_Stream_Output:
			theAnswer = SalsaHAL_IsStreamPropertySettable(inDriver, inObjectID, inClientProcessID, inAddress, outIsSettable);
			break;
		
		case kObjectID_Volume_Input_Master:
		case kObjectID_Volume_Output_Master:
		case kObjectID_Mute_Input_Master:
		case kObjectID_Mute_Output_Master:
		case kObjectID_DataSource_Input_Master:
		case kObjectID_DataSource_Output_Master:
		case kObjectID_DataDestination_PlayThru_Master:
			theAnswer = SalsaHAL_IsControlPropertySettable(inDriver, inObjectID, inClientProcessID, inAddress, outIsSettable);
			break;
				
		default:
			theAnswer = kAudioHardwareBadObjectError;
			break;
	};

Done:
	return theAnswer;
}

static OSStatus	SalsaHAL_GetPropertyDataSize(AudioServerPlugInDriverRef inDriver, AudioObjectID inObjectID, pid_t inClientProcessID, const AudioObjectPropertyAddress* inAddress, UInt32 inQualifierDataSize, const void* inQualifierData, UInt32* outDataSize)
{
	//	This method returns the byte size of the property's data.
	
	//	declare the local variables
	OSStatus theAnswer = 0;
	
	//	check the arguments
	FailWithAction(inDriver != gAudioServerPlugInDriverRef, theAnswer = kAudioHardwareBadObjectError, Done, "SalsaHAL_GetPropertyDataSize: bad driver reference");
	FailWithAction(inAddress == NULL, theAnswer = kAudioHardwareIllegalOperationError, Done, "SalsaHAL_GetPropertyDataSize: no address");
	FailWithAction(outDataSize == NULL, theAnswer = kAudioHardwareIllegalOperationError, Done, "SalsaHAL_GetPropertyDataSize: no place to put the return value");
	
	//	Note that for each object, this driver implements all the required properties plus a few
	//	extras that are useful but not required. There is more detailed commentary about each
	//	property in the SalsaHAL_GetPropertyData() method.
	switch(inObjectID)
	{
		case kObjectID_PlugIn:
			theAnswer = SalsaHAL_GetPlugInPropertyDataSize(inDriver, inObjectID, inClientProcessID, inAddress, inQualifierDataSize, inQualifierData, outDataSize);
			break;
		
		case kObjectID_Box:
			theAnswer = SalsaHAL_GetBoxPropertyDataSize(inDriver, inObjectID, inClientProcessID, inAddress, inQualifierDataSize, inQualifierData, outDataSize);
			break;
		
		case kObjectID_Device:
			theAnswer = SalsaHAL_GetDevicePropertyDataSize(inDriver, inObjectID, inClientProcessID, inAddress, inQualifierDataSize, inQualifierData, outDataSize);
			break;
		
		case kObjectID_Stream_Input:
		case kObjectID_Stream_Output:
			theAnswer = SalsaHAL_GetStreamPropertyDataSize(inDriver, inObjectID, inClientProcessID, inAddress, inQualifierDataSize, inQualifierData, outDataSize);
			break;
		
		case kObjectID_Volume_Input_Master:
		case kObjectID_Volume_Output_Master:
		case kObjectID_Mute_Input_Master:
		case kObjectID_Mute_Output_Master:
		case kObjectID_DataSource_Input_Master:
		case kObjectID_DataSource_Output_Master:
		case kObjectID_DataDestination_PlayThru_Master:
			theAnswer = SalsaHAL_GetControlPropertyDataSize(inDriver, inObjectID, inClientProcessID, inAddress, inQualifierDataSize, inQualifierData, outDataSize);
			break;
				
		default:
			theAnswer = kAudioHardwareBadObjectError;
			break;
	};

Done:
	return theAnswer;
}

static OSStatus	SalsaHAL_GetPropertyData(AudioServerPlugInDriverRef inDriver, AudioObjectID inObjectID, pid_t inClientProcessID, const AudioObjectPropertyAddress* inAddress, UInt32 inQualifierDataSize, const void* inQualifierData, UInt32 inDataSize, UInt32* outDataSize, void* outData)
{
	//	declare the local variables
	OSStatus theAnswer = 0;
	
	//	check the arguments
	FailWithAction(inDriver != gAudioServerPlugInDriverRef, theAnswer = kAudioHardwareBadObjectError, Done, "SalsaHAL_GetPropertyData: bad driver reference");
	FailWithAction(inAddress == NULL, theAnswer = kAudioHardwareIllegalOperationError, Done, "SalsaHAL_GetPropertyData: no address");
	FailWithAction(outDataSize == NULL, theAnswer = kAudioHardwareIllegalOperationError, Done, "SalsaHAL_GetPropertyData: no place to put the return value size");
	FailWithAction(outData == NULL, theAnswer = kAudioHardwareIllegalOperationError, Done, "SalsaHAL_GetPropertyData: no place to put the return value");
	
	//	Note that for each object, this driver implements all the required properties plus a few
	//	extras that are useful but not required.
	//
	//	Also, since most of the data that will get returned is static, there are few instances where
	//	it is necessary to lock the state mutex.
	switch(inObjectID)
	{
		case kObjectID_PlugIn:
			theAnswer = SalsaHAL_GetPlugInPropertyData(inDriver, inObjectID, inClientProcessID, inAddress, inQualifierDataSize, inQualifierData, inDataSize, outDataSize, outData);
			break;
		
		case kObjectID_Box:
			theAnswer = SalsaHAL_GetBoxPropertyData(inDriver, inObjectID, inClientProcessID, inAddress, inQualifierDataSize, inQualifierData, inDataSize, outDataSize, outData);
			break;
		
		case kObjectID_Device:
			theAnswer = SalsaHAL_GetDevicePropertyData(inDriver, inObjectID, inClientProcessID, inAddress, inQualifierDataSize, inQualifierData, inDataSize, outDataSize, outData);
			break;
		
		case kObjectID_Stream_Input:
		case kObjectID_Stream_Output:
			theAnswer = SalsaHAL_GetStreamPropertyData(inDriver, inObjectID, inClientProcessID, inAddress, inQualifierDataSize, inQualifierData, inDataSize, outDataSize, outData);
			break;
		
		case kObjectID_Volume_Input_Master:
		case kObjectID_Volume_Output_Master:
		case kObjectID_Mute_Input_Master:
		case kObjectID_Mute_Output_Master:
		case kObjectID_DataSource_Input_Master:
		case kObjectID_DataSource_Output_Master:
		case kObjectID_DataDestination_PlayThru_Master:
			theAnswer = SalsaHAL_GetControlPropertyData(inDriver, inObjectID, inClientProcessID, inAddress, inQualifierDataSize, inQualifierData, inDataSize, outDataSize, outData);
			break;
				
		default:
			theAnswer = kAudioHardwareBadObjectError;
			break;
	};

Done:
	return theAnswer;
}

static OSStatus	SalsaHAL_SetPropertyData(AudioServerPlugInDriverRef inDriver, AudioObjectID inObjectID, pid_t inClientProcessID, const AudioObjectPropertyAddress* inAddress, UInt32 inQualifierDataSize, const void* inQualifierData, UInt32 inDataSize, const void* inData)
{
	//	declare the local variables
	OSStatus theAnswer = 0;
	UInt32 theNumberPropertiesChanged = 0;
	AudioObjectPropertyAddress theChangedAddresses[2];
	
	//	check the arguments
	FailWithAction(inDriver != gAudioServerPlugInDriverRef, theAnswer = kAudioHardwareBadObjectError, Done, "SalsaHAL_SetPropertyData: bad driver reference");
	FailWithAction(inAddress == NULL, theAnswer = kAudioHardwareIllegalOperationError, Done, "SalsaHAL_SetPropertyData: no address");
	
	//	Note that for each object, this driver implements all the required properties plus a few
	//	extras that are useful but not required. There is more detailed commentary about each
	//	property in the SalsaHAL_GetPropertyData() method.
	switch(inObjectID)
	{
		case kObjectID_PlugIn:
			theAnswer = SalsaHAL_SetPlugInPropertyData(inDriver, inObjectID, inClientProcessID, inAddress, inQualifierDataSize, inQualifierData, inDataSize, inData, &theNumberPropertiesChanged, theChangedAddresses);
			break;
		
		case kObjectID_Box:
			theAnswer = SalsaHAL_SetBoxPropertyData(inDriver, inObjectID, inClientProcessID, inAddress, inQualifierDataSize, inQualifierData, inDataSize, inData, &theNumberPropertiesChanged, theChangedAddresses);
			break;
		
		case kObjectID_Device:
			theAnswer = SalsaHAL_SetDevicePropertyData(inDriver, inObjectID, inClientProcessID, inAddress, inQualifierDataSize, inQualifierData, inDataSize, inData, &theNumberPropertiesChanged, theChangedAddresses);
			break;
		
		case kObjectID_Stream_Input:
		case kObjectID_Stream_Output:
			theAnswer = SalsaHAL_SetStreamPropertyData(inDriver, inObjectID, inClientProcessID, inAddress, inQualifierDataSize, inQualifierData, inDataSize, inData, &theNumberPropertiesChanged, theChangedAddresses);
			break;
		
		case kObjectID_Volume_Input_Master:
		case kObjectID_Volume_Output_Master:
		case kObjectID_Mute_Input_Master:
		case kObjectID_Mute_Output_Master:
		case kObjectID_DataSource_Input_Master:
		case kObjectID_DataSource_Output_Master:
		case kObjectID_DataDestination_PlayThru_Master:
			theAnswer = SalsaHAL_SetControlPropertyData(inDriver, inObjectID, inClientProcessID, inAddress, inQualifierDataSize, inQualifierData, inDataSize, inData, &theNumberPropertiesChanged, theChangedAddresses);
			break;
				
		default:
			theAnswer = kAudioHardwareBadObjectError;
			break;
	};

	//	send any notifications
	if(theNumberPropertiesChanged > 0)
	{
		gPlugIn_Host->PropertiesChanged(gPlugIn_Host, inObjectID, theNumberPropertiesChanged, theChangedAddresses);
	}

Done:
	return theAnswer;
}

#pragma mark PlugIn Property Operations

static Boolean	SalsaHAL_HasPlugInProperty(AudioServerPlugInDriverRef inDriver, AudioObjectID inObjectID, pid_t inClientProcessID, const AudioObjectPropertyAddress* inAddress)
{
	//	This method returns whether or not the plug-in object has the given property.
	
	#pragma unused(inClientProcessID)
	
	//	declare the local variables
	Boolean theAnswer = false;
	
	//	check the arguments
	FailIf(inDriver != gAudioServerPlugInDriverRef, Done, "SalsaHAL_HasPlugInProperty: bad driver reference");
	FailIf(inAddress == NULL, Done, "SalsaHAL_HasPlugInProperty: no address");
	FailIf(inObjectID != kObjectID_PlugIn, Done, "SalsaHAL_HasPlugInProperty: not the plug-in object");
	
	
	//	Note that for each object, this driver implements all the required properties plus a few
	//	extras that are useful but not required. There is more detailed commentary about each
	//	property in the SalsaHAL_GetPlugInPropertyData() method.
	switch(inAddress->mSelector)
	{
		case kAudioObjectPropertyBaseClass:
		case kAudioObjectPropertyClass:
		case kAudioObjectPropertyOwner:
		case kAudioObjectPropertyManufacturer:
		case kAudioObjectPropertyOwnedObjects:
		case kAudioPlugInPropertyBoxList:
		case kAudioPlugInPropertyTranslateUIDToBox:
		case kAudioPlugInPropertyDeviceList:
		case kAudioPlugInPropertyTranslateUIDToDevice:
		case kAudioPlugInPropertyResourceBundle:
		case kAudioObjectPropertyCustomPropertyInfoList:
		case kPlugIn_CustomPropertyID:
			theAnswer = true;
			break;
	};

Done:
	return theAnswer;
}

static OSStatus	SalsaHAL_IsPlugInPropertySettable(AudioServerPlugInDriverRef inDriver, AudioObjectID inObjectID, pid_t inClientProcessID, const AudioObjectPropertyAddress* inAddress, Boolean* outIsSettable)
{
	//	This method returns whether or not the given property on the plug-in object can have its
	//	value changed.
	
	#pragma unused(inClientProcessID)
	
	//	declare the local variables
	OSStatus theAnswer = 0;
	
	//	check the arguments
	FailWithAction(inDriver != gAudioServerPlugInDriverRef, theAnswer = kAudioHardwareBadObjectError, Done, "SalsaHAL_IsPlugInPropertySettable: bad driver reference");
	FailWithAction(inAddress == NULL, theAnswer = kAudioHardwareIllegalOperationError, Done, "SalsaHAL_IsPlugInPropertySettable: no address");
	FailWithAction(outIsSettable == NULL, theAnswer = kAudioHardwareIllegalOperationError, Done, "SalsaHAL_IsPlugInPropertySettable: no place to put the return value");
	FailWithAction(inObjectID != kObjectID_PlugIn, theAnswer = kAudioHardwareBadObjectError, Done, "SalsaHAL_IsPlugInPropertySettable: not the plug-in object");
	
	//	Note that for each object, this driver implements all the required properties plus a few
	//	extras that are useful but not required. There is more detailed commentary about each
	//	property in the SalsaHAL_GetPlugInPropertyData() method.
	switch(inAddress->mSelector)
	{
		case kAudioObjectPropertyBaseClass:
		case kAudioObjectPropertyClass:
		case kAudioObjectPropertyOwner:
		case kAudioObjectPropertyManufacturer:
		case kAudioObjectPropertyOwnedObjects:
		case kAudioPlugInPropertyBoxList:
		case kAudioPlugInPropertyTranslateUIDToBox:
		case kAudioPlugInPropertyDeviceList:
		case kAudioPlugInPropertyTranslateUIDToDevice:
		case kAudioPlugInPropertyResourceBundle:
		case kAudioObjectPropertyCustomPropertyInfoList:
			*outIsSettable = false;
			break;
		
		case kPlugIn_CustomPropertyID:
			*outIsSettable = true;
			break;
		
		default:
			theAnswer = kAudioHardwareUnknownPropertyError;
			break;
	};

Done:
	return theAnswer;
}

static OSStatus	SalsaHAL_GetPlugInPropertyDataSize(AudioServerPlugInDriverRef inDriver, AudioObjectID inObjectID, pid_t inClientProcessID, const AudioObjectPropertyAddress* inAddress, UInt32 inQualifierDataSize, const void* inQualifierData, UInt32* outDataSize)
{
	//	This method returns the byte size of the property's data.
	
	#pragma unused(inClientProcessID, inQualifierDataSize, inQualifierData)
	
	//	declare the local variables
	OSStatus theAnswer = 0;
	
	//	check the arguments
	FailWithAction(inDriver != gAudioServerPlugInDriverRef, theAnswer = kAudioHardwareBadObjectError, Done, "SalsaHAL_GetPlugInPropertyDataSize: bad driver reference");
	FailWithAction(inAddress == NULL, theAnswer = kAudioHardwareIllegalOperationError, Done, "SalsaHAL_GetPlugInPropertyDataSize: no address");
	FailWithAction(outDataSize == NULL, theAnswer = kAudioHardwareIllegalOperationError, Done, "SalsaHAL_GetPlugInPropertyDataSize: no place to put the return value");
	FailWithAction(inObjectID != kObjectID_PlugIn, theAnswer = kAudioHardwareBadObjectError, Done, "SalsaHAL_GetPlugInPropertyDataSize: not the plug-in object");
	
	//	Note that for each object, this driver implements all the required properties plus a few
	//	extras that are useful but not required. There is more detailed commentary about each
	//	property in the SalsaHAL_GetPlugInPropertyData() method.
	switch(inAddress->mSelector)
	{
		case kAudioObjectPropertyBaseClass:
			*outDataSize = sizeof(AudioClassID);
			break;
			
		case kAudioObjectPropertyClass:
			*outDataSize = sizeof(AudioClassID);
			break;
			
		case kAudioObjectPropertyOwner:
			*outDataSize = sizeof(AudioObjectID);
			break;
			
		case kAudioObjectPropertyManufacturer:
			*outDataSize = sizeof(CFStringRef);
			break;
			
		case kAudioObjectPropertyOwnedObjects:
			if(gBox_Acquired)
			{
				*outDataSize = 2 * sizeof(AudioClassID);
			}
			else
			{
				*outDataSize = sizeof(AudioClassID);
			}
			break;
			
		case kAudioPlugInPropertyBoxList:
			*outDataSize = sizeof(AudioClassID);
			break;
			
		case kAudioPlugInPropertyTranslateUIDToBox:
			*outDataSize = sizeof(AudioObjectID);
			break;
			
		case kAudioPlugInPropertyDeviceList:
			if(gBox_Acquired)
			{
				*outDataSize = sizeof(AudioClassID);
			}
			else
			{
				*outDataSize = 0;
			}
			break;
			
		case kAudioPlugInPropertyTranslateUIDToDevice:
			*outDataSize = sizeof(AudioObjectID);
			break;
			
		case kAudioPlugInPropertyResourceBundle:
			*outDataSize = sizeof(CFStringRef);
			break;
			
		case kAudioObjectPropertyCustomPropertyInfoList:
			*outDataSize = sizeof(AudioServerPlugInCustomPropertyInfo);
			break;
			
		case kPlugIn_CustomPropertyID:
			FailWithAction(inQualifierDataSize != sizeof(CFStringRef), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetPlugInPropertyDataSize: the qualifier is the wrong size for kPlugIn_CustomPropertyID");
			FailWithAction(inQualifierData == NULL, theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetPlugInPropertyDataSize: no qualifier for kPlugIn_CustomPropertyID");
			DebugMsg("SalsaHAL_GetPlugInPropertyDataSize: the qualifier passed to us was:");
			CFShow(*((CFPropertyListRef*)inQualifierData));
			*outDataSize = sizeof(CFPropertyListRef);
			break;
			
		default:
			theAnswer = kAudioHardwareUnknownPropertyError;
			break;
	};

Done:
	return theAnswer;
}

static OSStatus	SalsaHAL_GetPlugInPropertyData(AudioServerPlugInDriverRef inDriver, AudioObjectID inObjectID, pid_t inClientProcessID, const AudioObjectPropertyAddress* inAddress, UInt32 inQualifierDataSize, const void* inQualifierData, UInt32 inDataSize, UInt32* outDataSize, void* outData)
{
	#pragma unused(inClientProcessID)
	
	//	declare the local variables
	OSStatus theAnswer = 0;
	UInt32 theNumberItemsToFetch;
	
	//	check the arguments
	FailWithAction(inDriver != gAudioServerPlugInDriverRef, theAnswer = kAudioHardwareBadObjectError, Done, "SalsaHAL_GetPlugInPropertyData: bad driver reference");
	FailWithAction(inAddress == NULL, theAnswer = kAudioHardwareIllegalOperationError, Done, "SalsaHAL_GetPlugInPropertyData: no address");
	FailWithAction(outDataSize == NULL, theAnswer = kAudioHardwareIllegalOperationError, Done, "SalsaHAL_GetPlugInPropertyData: no place to put the return value size");
	FailWithAction(outData == NULL, theAnswer = kAudioHardwareIllegalOperationError, Done, "SalsaHAL_GetPlugInPropertyData: no place to put the return value");
	FailWithAction(inObjectID != kObjectID_PlugIn, theAnswer = kAudioHardwareBadObjectError, Done, "SalsaHAL_GetPlugInPropertyData: not the plug-in object");
	
	//	Note that for each object, this driver implements all the required properties plus a few
	//	extras that are useful but not required.
	//
	//	Also, since most of the data that will get returned is static, there are few instances where
	//	it is necessary to lock the state mutex.
	switch(inAddress->mSelector)
	{
		case kAudioObjectPropertyBaseClass:
			//	The base class for kAudioPlugInClassID is kAudioObjectClassID
			FailWithAction(inDataSize < sizeof(AudioClassID), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetPlugInPropertyData: not enough space for the return value of kAudioObjectPropertyBaseClass for the plug-in");
			*((AudioClassID*)outData) = kAudioObjectClassID;
			*outDataSize = sizeof(AudioClassID);
			break;
			
		case kAudioObjectPropertyClass:
			//	The class is always kAudioPlugInClassID for regular drivers
			FailWithAction(inDataSize < sizeof(AudioClassID), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetPlugInPropertyData: not enough space for the return value of kAudioObjectPropertyClass for the plug-in");
			*((AudioClassID*)outData) = kAudioPlugInClassID;
			*outDataSize = sizeof(AudioClassID);
			break;
			
		case kAudioObjectPropertyOwner:
			//	The plug-in doesn't have an owning object
			FailWithAction(inDataSize < sizeof(AudioObjectID), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetPlugInPropertyData: not enough space for the return value of kAudioObjectPropertyOwner for the plug-in");
			*((AudioObjectID*)outData) = kAudioObjectUnknown;
			*outDataSize = sizeof(AudioObjectID);
			break;
			
		case kAudioObjectPropertyManufacturer:
			//	This is the human readable name of the maker of the plug-in.
			FailWithAction(inDataSize < sizeof(CFStringRef), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetPlugInPropertyData: not enough space for the return value of kAudioObjectPropertyManufacturer for the plug-in");
			*((CFStringRef*)outData) = CFSTR("Apple Inc.");
			*outDataSize = sizeof(CFStringRef);
			break;
			
		case kAudioObjectPropertyOwnedObjects:
			//	Calculate the number of items that have been requested. Note that this
			//	number is allowed to be smaller than the actual size of the list. In such
			//	case, only that number of items will be returned
			theNumberItemsToFetch = inDataSize / sizeof(AudioObjectID);
			
			//	Clamp that to the number of boxes this driver implements (which is just 1)
			if(theNumberItemsToFetch > (gBox_Acquired ? 2 : 1))
			{
				theNumberItemsToFetch = (gBox_Acquired ? 2 : 1);
			}
			
			//	Write the devices' object IDs into the return value
			if(theNumberItemsToFetch > 1)
			{
				((AudioObjectID*)outData)[0] = kObjectID_Box;
				((AudioObjectID*)outData)[0] = kObjectID_Device;
			}
			else if(theNumberItemsToFetch > 0)
			{
				((AudioObjectID*)outData)[0] = kObjectID_Box;
			}
			
			//	Return how many bytes we wrote to
			*outDataSize = theNumberItemsToFetch * sizeof(AudioClassID);
			break;
			
		case kAudioPlugInPropertyBoxList:
			//	Calculate the number of items that have been requested. Note that this
			//	number is allowed to be smaller than the actual size of the list. In such
			//	case, only that number of items will be returned
			theNumberItemsToFetch = inDataSize / sizeof(AudioObjectID);
			
			//	Clamp that to the number of boxes this driver implements (which is just 1)
			if(theNumberItemsToFetch > 1)
			{
				theNumberItemsToFetch = 1;
			}
			
			//	Write the devices' object IDs into the return value
			if(theNumberItemsToFetch > 0)
			{
				((AudioObjectID*)outData)[0] = kObjectID_Box;
			}
			
			//	Return how many bytes we wrote to
			*outDataSize = theNumberItemsToFetch * sizeof(AudioClassID);
			break;
			
		case kAudioPlugInPropertyTranslateUIDToBox:
			//	This property takes the CFString passed in the qualifier and converts that
			//	to the object ID of the box it corresponds to. For this driver, there is
			//	just the one box. Note that it is not an error if the string in the
			//	qualifier doesn't match any devices. In such case, kAudioObjectUnknown is
			//	the object ID to return.
			FailWithAction(inDataSize < sizeof(AudioObjectID), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetPlugInPropertyData: not enough space for the return value of kAudioPlugInPropertyTranslateUIDToBox");
			FailWithAction(inQualifierDataSize != sizeof(CFStringRef), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetPlugInPropertyData: the qualifier is the wrong size for kAudioPlugInPropertyTranslateUIDToBox");
			FailWithAction(inQualifierData == NULL, theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetPlugInPropertyData: no qualifier for kAudioPlugInPropertyTranslateUIDToBox");
			if(CFStringCompare(*((CFStringRef*)inQualifierData), CFSTR(kBox_UID), 0) == kCFCompareEqualTo)
			{
				*((AudioObjectID*)outData) = kObjectID_Box;
			}
			else
			{
				*((AudioObjectID*)outData) = kAudioObjectUnknown;
			}
			*outDataSize = sizeof(AudioObjectID);
			break;
			
		case kAudioPlugInPropertyDeviceList:
			//	Calculate the number of items that have been requested. Note that this
			//	number is allowed to be smaller than the actual size of the list. In such
			//	case, only that number of items will be returned
			theNumberItemsToFetch = inDataSize / sizeof(AudioObjectID);
			
			//	Clamp that to the number of devices this driver implements (which is just 1 if the
			//	box has been acquired)
			if(theNumberItemsToFetch > (gBox_Acquired ? 1 : 0))
			{
				theNumberItemsToFetch = (gBox_Acquired ? 1 : 0);
			}
			
			//	Write the devices' object IDs into the return value
			if(theNumberItemsToFetch > 0)
			{
				((AudioObjectID*)outData)[0] = kObjectID_Device;
			}
			
			//	Return how many bytes we wrote to
			*outDataSize = theNumberItemsToFetch * sizeof(AudioClassID);
			break;
			
		case kAudioPlugInPropertyTranslateUIDToDevice:
			//	This property takes the CFString passed in the qualifier and converts that
			//	to the object ID of the device it corresponds to. For this driver, there is
			//	just the one device. Note that it is not an error if the string in the
			//	qualifier doesn't match any devices. In such case, kAudioObjectUnknown is
			//	the object ID to return.
			FailWithAction(inDataSize < sizeof(AudioObjectID), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetPlugInPropertyData: not enough space for the return value of kAudioPlugInPropertyTranslateUIDToDevice");
			FailWithAction(inQualifierDataSize != sizeof(CFStringRef), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetPlugInPropertyData: the qualifier is the wrong size for kAudioPlugInPropertyTranslateUIDToDevice");
			FailWithAction(inQualifierData == NULL, theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetPlugInPropertyData: no qualifier for kAudioPlugInPropertyTranslateUIDToDevice");
			if(CFStringCompare(*((CFStringRef*)inQualifierData), CFSTR(kDevice_UID), 0) == kCFCompareEqualTo)
			{
				*((AudioObjectID*)outData) = kObjectID_Device;
			}
			else
			{
				*((AudioObjectID*)outData) = kAudioObjectUnknown;
			}
			*outDataSize = sizeof(AudioObjectID);
			break;
			
		case kAudioPlugInPropertyResourceBundle:
			//	The resource bundle is a path relative to the path of the plug-in's bundle.
			//	To specify that the plug-in bundle itself should be used, we just return the
			//	empty string.
			FailWithAction(inDataSize < sizeof(AudioObjectID), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetPlugInPropertyData: not enough space for the return value of kAudioPlugInPropertyResourceBundle");
			*((CFStringRef*)outData) = CFSTR("");
			*outDataSize = sizeof(CFStringRef);
			break;
			
		case kAudioObjectPropertyCustomPropertyInfoList:
			//	This property returns an array of AudioServerPlugInCustomPropertyInfo's that
			//	describe the type of data used by any custom properties. For this example,
			//	the plug-in supports a single property whose data type is a CFString and
			//	whose qualifier is a CFString.
			FailWithAction(inDataSize < sizeof(AudioServerPlugInCustomPropertyInfo), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetPlugInPropertyData: not enough space for the return value of kAudioObjectPropertyCustomPropertyInfoList");
			((AudioServerPlugInCustomPropertyInfo*)outData)->mSelector = kPlugIn_CustomPropertyID;
			((AudioServerPlugInCustomPropertyInfo*)outData)->mPropertyDataType = kAudioServerPlugInCustomPropertyDataTypeCFString;
			((AudioServerPlugInCustomPropertyInfo*)outData)->mQualifierDataType = kAudioServerPlugInCustomPropertyDataTypeCFPropertyList;
			*outDataSize = sizeof(AudioServerPlugInCustomPropertyInfo);
			break;
			
		case kPlugIn_CustomPropertyID:
			FailWithAction(inDataSize < sizeof(CFStringRef), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetPlugInPropertyData: not enough space for the return value of kPlugIn_CustomPropertyID");
			FailWithAction(inQualifierDataSize != sizeof(CFPropertyListRef), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetPlugInPropertyData: the qualifier is the wrong size for kPlugIn_CustomPropertyID");
			FailWithAction(inQualifierData == NULL, theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetPlugInPropertyData: no qualifier for kPlugIn_CustomPropertyID");
			DebugMsg("SalsaHAL_GetPlugInPropertyData: the qualifier passed to us was:");
			CFShow(*((CFPropertyListRef*)inQualifierData));
			*((CFStringRef*)outData) = CFSTR("SalsaHAL PlugIn Custom Property");
			*outDataSize = sizeof(CFStringRef);
			break;
			
		default:
			theAnswer = kAudioHardwareUnknownPropertyError;
			break;
	};

Done:
	return theAnswer;
}

static OSStatus	SalsaHAL_SetPlugInPropertyData(AudioServerPlugInDriverRef inDriver, AudioObjectID inObjectID, pid_t inClientProcessID, const AudioObjectPropertyAddress* inAddress, UInt32 inQualifierDataSize, const void* inQualifierData, UInt32 inDataSize, const void* inData, UInt32* outNumberPropertiesChanged, AudioObjectPropertyAddress outChangedAddresses[2])
{
	#pragma unused(inClientProcessID, inQualifierDataSize, inQualifierData, inDataSize, inData)
	
	//	declare the local variables
	OSStatus theAnswer = 0;
	
	//	check the arguments
	FailWithAction(inDriver != gAudioServerPlugInDriverRef, theAnswer = kAudioHardwareBadObjectError, Done, "SalsaHAL_SetPlugInPropertyData: bad driver reference");
	FailWithAction(inAddress == NULL, theAnswer = kAudioHardwareIllegalOperationError, Done, "SalsaHAL_SetPlugInPropertyData: no address");
	FailWithAction(outNumberPropertiesChanged == NULL, theAnswer = kAudioHardwareIllegalOperationError, Done, "SalsaHAL_SetPlugInPropertyData: no place to return the number of properties that changed");
	FailWithAction(outChangedAddresses == NULL, theAnswer = kAudioHardwareIllegalOperationError, Done, "SalsaHAL_SetPlugInPropertyData: no place to return the properties that changed");
	FailWithAction(inObjectID != kObjectID_PlugIn, theAnswer = kAudioHardwareBadObjectError, Done, "SalsaHAL_SetPlugInPropertyData: not the plug-in object");
	
	//	initialize the returned number of changed properties
	*outNumberPropertiesChanged = 0;
	
	//	Note that for each object, this driver implements all the required properties plus a few
	//	extras that are useful but not required. There is more detailed commentary about each
	//	property in the SalsaHAL_GetPlugInPropertyData() method.
	switch(inAddress->mSelector)
	{
		case kPlugIn_CustomPropertyID:
			FailWithAction(inDataSize != sizeof(CFStringRef), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_SetPlugInPropertyData: not enough space for the return value of kPlugIn_CustomPropertyID");
			FailWithAction(inQualifierDataSize != sizeof(CFPropertyListRef), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_SetPlugInPropertyData: the qualifier is the wrong size for kPlugIn_CustomPropertyID");
			FailWithAction(inQualifierData == NULL, theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_SetPlugInPropertyData: no qualifier for kPlugIn_CustomPropertyID");
			DebugMsg("SalsaHAL_SetPlugInPropertyData: the qualifier passed to us was:");
			CFShow(*((CFPropertyListRef*)inQualifierData));
			DebugMsg("SalsaHAL_SetPlugInPropertyData: the data passed to us was:");
			CFShow(*((CFStringRef*)inData));
			break;
			
		default:
			theAnswer = kAudioHardwareUnknownPropertyError;
			break;
	};

Done:
	return theAnswer;
}

#pragma mark Box Property Operations

static Boolean	SalsaHAL_HasBoxProperty(AudioServerPlugInDriverRef inDriver, AudioObjectID inObjectID, pid_t inClientProcessID, const AudioObjectPropertyAddress* inAddress)
{
	//	This method returns whether or not the box object has the given property.
	
	#pragma unused(inClientProcessID)
	
	//	declare the local variables
	Boolean theAnswer = false;
	
	//	check the arguments
	FailIf(inDriver != gAudioServerPlugInDriverRef, Done, "SalsaHAL_HasBoxProperty: bad driver reference");
	FailIf(inAddress == NULL, Done, "SalsaHAL_HasBoxProperty: no address");
	FailIf(inObjectID != kObjectID_Box, Done, "SalsaHAL_HasBoxProperty: not the box object");
	
	
	//	Note that for each object, this driver implements all the required properties plus a few
	//	extras that are useful but not required. There is more detailed commentary about each
	//	property in the SalsaHAL_GetBoxPropertyData() method.
	switch(inAddress->mSelector)
	{
		case kAudioObjectPropertyBaseClass:
		case kAudioObjectPropertyClass:
		case kAudioObjectPropertyOwner:
		case kAudioObjectPropertyName:
		case kAudioObjectPropertyModelName:
		case kAudioObjectPropertyManufacturer:
		case kAudioObjectPropertyOwnedObjects:
		case kAudioObjectPropertyIdentify:
		case kAudioObjectPropertySerialNumber:
		case kAudioObjectPropertyFirmwareVersion:
		case kAudioBoxPropertyBoxUID:
		case kAudioBoxPropertyTransportType:
		case kAudioBoxPropertyHasAudio:
		case kAudioBoxPropertyHasVideo:
		case kAudioBoxPropertyHasMIDI:
		case kAudioBoxPropertyIsProtected:
		case kAudioBoxPropertyAcquired:
		case kAudioBoxPropertyAcquisitionFailed:
		case kAudioBoxPropertyDeviceList:
			theAnswer = true;
			break;
	};

Done:
	return theAnswer;
}

static OSStatus	SalsaHAL_IsBoxPropertySettable(AudioServerPlugInDriverRef inDriver, AudioObjectID inObjectID, pid_t inClientProcessID, const AudioObjectPropertyAddress* inAddress, Boolean* outIsSettable)
{
	//	This method returns whether or not the given property on the plug-in object can have its
	//	value changed.
	
	#pragma unused(inClientProcessID)
	
	//	declare the local variables
	OSStatus theAnswer = 0;
	
	//	check the arguments
	FailWithAction(inDriver != gAudioServerPlugInDriverRef, theAnswer = kAudioHardwareBadObjectError, Done, "SalsaHAL_IsBoxPropertySettable: bad driver reference");
	FailWithAction(inAddress == NULL, theAnswer = kAudioHardwareIllegalOperationError, Done, "SalsaHAL_IsBoxPropertySettable: no address");
	FailWithAction(outIsSettable == NULL, theAnswer = kAudioHardwareIllegalOperationError, Done, "SalsaHAL_IsBoxPropertySettable: no place to put the return value");
	FailWithAction(inObjectID != kObjectID_Box, theAnswer = kAudioHardwareBadObjectError, Done, "SalsaHAL_IsBoxPropertySettable: not the plug-in object");
	
	//	Note that for each object, this driver implements all the required properties plus a few
	//	extras that are useful but not required. There is more detailed commentary about each
	//	property in the SalsaHAL_GetBoxPropertyData() method.
	switch(inAddress->mSelector)
	{
		case kAudioObjectPropertyBaseClass:
		case kAudioObjectPropertyClass:
		case kAudioObjectPropertyOwner:
		case kAudioObjectPropertyModelName:
		case kAudioObjectPropertyManufacturer:
		case kAudioObjectPropertyOwnedObjects:
		case kAudioObjectPropertySerialNumber:
		case kAudioObjectPropertyFirmwareVersion:
		case kAudioBoxPropertyBoxUID:
		case kAudioBoxPropertyTransportType:
		case kAudioBoxPropertyHasAudio:
		case kAudioBoxPropertyHasVideo:
		case kAudioBoxPropertyHasMIDI:
		case kAudioBoxPropertyIsProtected:
		case kAudioBoxPropertyAcquisitionFailed:
		case kAudioBoxPropertyDeviceList:
			*outIsSettable = false;
			break;
		
		case kAudioObjectPropertyName:
		case kAudioObjectPropertyIdentify:
		case kAudioBoxPropertyAcquired:
			*outIsSettable = true;
			break;
		
		default:
			theAnswer = kAudioHardwareUnknownPropertyError;
			break;
	};

Done:
	return theAnswer;
}

static OSStatus	SalsaHAL_GetBoxPropertyDataSize(AudioServerPlugInDriverRef inDriver, AudioObjectID inObjectID, pid_t inClientProcessID, const AudioObjectPropertyAddress* inAddress, UInt32 inQualifierDataSize, const void* inQualifierData, UInt32* outDataSize)
{
	//	This method returns the byte size of the property's data.
	
	#pragma unused(inClientProcessID, inQualifierDataSize, inQualifierData)
	
	//	declare the local variables
	OSStatus theAnswer = 0;
	
	//	check the arguments
	FailWithAction(inDriver != gAudioServerPlugInDriverRef, theAnswer = kAudioHardwareBadObjectError, Done, "SalsaHAL_GetBoxPropertyDataSize: bad driver reference");
	FailWithAction(inAddress == NULL, theAnswer = kAudioHardwareIllegalOperationError, Done, "SalsaHAL_GetBoxPropertyDataSize: no address");
	FailWithAction(outDataSize == NULL, theAnswer = kAudioHardwareIllegalOperationError, Done, "SalsaHAL_GetBoxPropertyDataSize: no place to put the return value");
	FailWithAction(inObjectID != kObjectID_Box, theAnswer = kAudioHardwareBadObjectError, Done, "SalsaHAL_GetBoxPropertyDataSize: not the plug-in object");
	
	//	Note that for each object, this driver implements all the required properties plus a few
	//	extras that are useful but not required. There is more detailed commentary about each
	//	property in the SalsaHAL_GetBoxPropertyData() method.
	switch(inAddress->mSelector)
	{
		case kAudioObjectPropertyBaseClass:
			*outDataSize = sizeof(AudioClassID);
			break;
			
		case kAudioObjectPropertyClass:
			*outDataSize = sizeof(AudioClassID);
			break;
			
		case kAudioObjectPropertyOwner:
			*outDataSize = sizeof(AudioObjectID);
			break;
			
		case kAudioObjectPropertyName:
			*outDataSize = sizeof(CFStringRef);
			break;
			
		case kAudioObjectPropertyModelName:
			*outDataSize = sizeof(CFStringRef);
			break;
			
		case kAudioObjectPropertyManufacturer:
			*outDataSize = sizeof(CFStringRef);
			break;
			
		case kAudioObjectPropertyOwnedObjects:
			*outDataSize = 0;
			break;
			
		case kAudioObjectPropertyIdentify:
			*outDataSize = sizeof(UInt32);
			break;
			
		case kAudioObjectPropertySerialNumber:
			*outDataSize = sizeof(CFStringRef);
			break;
			
		case kAudioObjectPropertyFirmwareVersion:
			*outDataSize = sizeof(CFStringRef);
			break;
			
		case kAudioBoxPropertyBoxUID:
			*outDataSize = sizeof(CFStringRef);
			break;
			
		case kAudioBoxPropertyTransportType:
			*outDataSize = sizeof(UInt32);
			break;
			
		case kAudioBoxPropertyHasAudio:
			*outDataSize = sizeof(UInt32);
			break;
			
		case kAudioBoxPropertyHasVideo:
			*outDataSize = sizeof(UInt32);
			break;
			
		case kAudioBoxPropertyHasMIDI:
			*outDataSize = sizeof(UInt32);
			break;
			
		case kAudioBoxPropertyIsProtected:
			*outDataSize = sizeof(UInt32);
			break;
			
		case kAudioBoxPropertyAcquired:
			*outDataSize = sizeof(UInt32);
			break;
			
		case kAudioBoxPropertyAcquisitionFailed:
			*outDataSize = sizeof(UInt32);
			break;
			
		case kAudioBoxPropertyDeviceList:
			{
				pthread_mutex_lock(&gPlugIn_StateMutex);
				*outDataSize = gBox_Acquired ? sizeof(AudioObjectID) : 0;
				pthread_mutex_unlock(&gPlugIn_StateMutex);
			}
			break;
			
		default:
			theAnswer = kAudioHardwareUnknownPropertyError;
			break;
	};

Done:
	return theAnswer;
}

static OSStatus	SalsaHAL_GetBoxPropertyData(AudioServerPlugInDriverRef inDriver, AudioObjectID inObjectID, pid_t inClientProcessID, const AudioObjectPropertyAddress* inAddress, UInt32 inQualifierDataSize, const void* inQualifierData, UInt32 inDataSize, UInt32* outDataSize, void* outData)
{
	#pragma unused(inClientProcessID, inQualifierDataSize, inQualifierData)
	
	//	declare the local variables
	OSStatus theAnswer = 0;
	
	//	check the arguments
	FailWithAction(inDriver != gAudioServerPlugInDriverRef, theAnswer = kAudioHardwareBadObjectError, Done, "SalsaHAL_GetBoxPropertyData: bad driver reference");
	FailWithAction(inAddress == NULL, theAnswer = kAudioHardwareIllegalOperationError, Done, "SalsaHAL_GetBoxPropertyData: no address");
	FailWithAction(outDataSize == NULL, theAnswer = kAudioHardwareIllegalOperationError, Done, "SalsaHAL_GetBoxPropertyData: no place to put the return value size");
	FailWithAction(outData == NULL, theAnswer = kAudioHardwareIllegalOperationError, Done, "SalsaHAL_GetBoxPropertyData: no place to put the return value");
	FailWithAction(inObjectID != kObjectID_Box, theAnswer = kAudioHardwareBadObjectError, Done, "SalsaHAL_GetBoxPropertyData: not the plug-in object");
	
	//	Note that for each object, this driver implements all the required properties plus a few
	//	extras that are useful but not required.
	//
	//	Also, since most of the data that will get returned is static, there are few instances where
	//	it is necessary to lock the state mutex.
	switch(inAddress->mSelector)
	{
		case kAudioObjectPropertyBaseClass:
			//	The base class for kAudioBoxClassID is kAudioObjectClassID
			FailWithAction(inDataSize < sizeof(AudioClassID), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetBoxPropertyData: not enough space for the return value of kAudioObjectPropertyBaseClass for the box");
			*((AudioClassID*)outData) = kAudioObjectClassID;
			*outDataSize = sizeof(AudioClassID);
			break;
			
		case kAudioObjectPropertyClass:
			//	The class is always kAudioBoxClassID for regular drivers
			FailWithAction(inDataSize < sizeof(AudioClassID), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetBoxPropertyData: not enough space for the return value of kAudioObjectPropertyClass for the box");
			*((AudioClassID*)outData) = kAudioBoxClassID;
			*outDataSize = sizeof(AudioClassID);
			break;
			
		case kAudioObjectPropertyOwner:
			//	The owner is the plug-in object
			FailWithAction(inDataSize < sizeof(AudioObjectID), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetBoxPropertyData: not enough space for the return value of kAudioObjectPropertyOwner for the box");
			*((AudioObjectID*)outData) = kObjectID_PlugIn;
			*outDataSize = sizeof(AudioObjectID);
			break;
			
		case kAudioObjectPropertyName:
			//	This is the human readable name of the maker of the box.
			FailWithAction(inDataSize < sizeof(CFStringRef), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetBoxPropertyData: not enough space for the return value of kAudioObjectPropertyManufacturer for the box");
			pthread_mutex_lock(&gPlugIn_StateMutex);
			*((CFStringRef*)outData) = gBox_Name;
			pthread_mutex_unlock(&gPlugIn_StateMutex);
			if(*((CFStringRef*)outData) != NULL)
			{
				CFRetain(*((CFStringRef*)outData));
			}
			*outDataSize = sizeof(CFStringRef);
			break;
			
		case kAudioObjectPropertyModelName:
			//	This is the human readable name of the maker of the box.
			FailWithAction(inDataSize < sizeof(CFStringRef), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetBoxPropertyData: not enough space for the return value of kAudioObjectPropertyManufacturer for the box");
			*((CFStringRef*)outData) = CFSTR("Null Model");
			*outDataSize = sizeof(CFStringRef);
			break;
			
		case kAudioObjectPropertyManufacturer:
			//	This is the human readable name of the maker of the box.
			FailWithAction(inDataSize < sizeof(CFStringRef), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetBoxPropertyData: not enough space for the return value of kAudioObjectPropertyManufacturer for the box");
			*((CFStringRef*)outData) = CFSTR("Apple Inc.");
			*outDataSize = sizeof(CFStringRef);
			break;
			
		case kAudioObjectPropertyOwnedObjects:
			//	This returns the objects directly owned by the object. Boxes don't own anything.
			*outDataSize = 0;
			break;
			
		case kAudioObjectPropertyIdentify:
			//	This is used to highling the device in the UI, but it's value has no meaning
			FailWithAction(inDataSize < sizeof(UInt32), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetBoxPropertyData: not enough space for the return value of kAudioObjectPropertyIdentify for the box");
			*((UInt32*)outData) = 0;
			*outDataSize = sizeof(UInt32);
			break;
			
		case kAudioObjectPropertySerialNumber:
			//	This is the human readable serial number of the box.
			FailWithAction(inDataSize < sizeof(CFStringRef), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetBoxPropertyData: not enough space for the return value of kAudioObjectPropertySerialNumber for the box");
			*((CFStringRef*)outData) = CFSTR("00000001");
			*outDataSize = sizeof(CFStringRef);
			break;
			
		case kAudioObjectPropertyFirmwareVersion:
			//	This is the human readable firmware version of the box.
			FailWithAction(inDataSize < sizeof(CFStringRef), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetBoxPropertyData: not enough space for the return value of kAudioObjectPropertyFirmwareVersion for the box");
			*((CFStringRef*)outData) = CFSTR("1.0");
			*outDataSize = sizeof(CFStringRef);
			break;
			
		case kAudioBoxPropertyBoxUID:
			//	Boxes have UIDs the same as devices
			FailWithAction(inDataSize < sizeof(CFStringRef), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetBoxPropertyData: not enough space for the return value of kAudioObjectPropertyManufacturer for the box");
			*((CFStringRef*)outData) = CFSTR(kBox_UID);
			break;
			
		case kAudioBoxPropertyTransportType:
			//	This value represents how the device is attached to the system. This can be
			//	any 32 bit integer, but common values for this property are defined in
			//	<CoreAudio/AudioHardwareBase.h>
			FailWithAction(inDataSize < sizeof(UInt32), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetBoxPropertyData: not enough space for the return value of kAudioDevicePropertyTransportType for the box");
			*((UInt32*)outData) = kAudioDeviceTransportTypeVirtual;
			*outDataSize = sizeof(UInt32);
			break;
			
		case kAudioBoxPropertyHasAudio:
			//	Indicates whether or not the box has audio capabilities
			FailWithAction(inDataSize < sizeof(UInt32), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetBoxPropertyData: not enough space for the return value of kAudioBoxPropertyHasAudio for the box");
			*((UInt32*)outData) = 1;
			*outDataSize = sizeof(UInt32);
			break;
			
		case kAudioBoxPropertyHasVideo:
			//	Indicates whether or not the box has video capabilities
			FailWithAction(inDataSize < sizeof(UInt32), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetBoxPropertyData: not enough space for the return value of kAudioBoxPropertyHasVideo for the box");
			*((UInt32*)outData) = 0;
			*outDataSize = sizeof(UInt32);
			break;
			
		case kAudioBoxPropertyHasMIDI:
			//	Indicates whether or not the box has MIDI capabilities
			FailWithAction(inDataSize < sizeof(UInt32), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetBoxPropertyData: not enough space for the return value of kAudioBoxPropertyHasMIDI for the box");
			*((UInt32*)outData) = 0;
			*outDataSize = sizeof(UInt32);
			break;
			
		case kAudioBoxPropertyIsProtected:
			//	Indicates whether or not the box has requires authentication to use
			FailWithAction(inDataSize < sizeof(UInt32), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetBoxPropertyData: not enough space for the return value of kAudioBoxPropertyIsProtected for the box");
			*((UInt32*)outData) = 0;
			*outDataSize = sizeof(UInt32);
			break;
			
		case kAudioBoxPropertyAcquired:
			//	When set to a non-zero value, the device is acquired for use by the local machine
			FailWithAction(inDataSize < sizeof(UInt32), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetBoxPropertyData: not enough space for the return value of kAudioBoxPropertyAcquired for the box");
			pthread_mutex_lock(&gPlugIn_StateMutex);
			*((UInt32*)outData) = gBox_Acquired ? 1 : 0;
			pthread_mutex_unlock(&gPlugIn_StateMutex);
			*outDataSize = sizeof(UInt32);
			break;
			
		case kAudioBoxPropertyAcquisitionFailed:
			//	This is used for notifications to say when an attempt to acquire a device has failed.
			FailWithAction(inDataSize < sizeof(UInt32), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetBoxPropertyData: not enough space for the return value of kAudioBoxPropertyAcquisitionFailed for the box");
			*((UInt32*)outData) = 0;
			*outDataSize = sizeof(UInt32);
			break;
			
		case kAudioBoxPropertyDeviceList:
			//	This is used to indicate which devices came from this box
			pthread_mutex_lock(&gPlugIn_StateMutex);
			if(gBox_Acquired)
			{
				FailWithAction(inDataSize < sizeof(AudioObjectID), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetBoxPropertyData: not enough space for the return value of kAudioBoxPropertyDeviceList for the box");
				*((AudioObjectID*)outData) = kObjectID_Device;
				*outDataSize = sizeof(AudioObjectID);
			}
			else
			{
				*outDataSize = 0;
			}
			pthread_mutex_unlock(&gPlugIn_StateMutex);
			break;
			
		default:
			theAnswer = kAudioHardwareUnknownPropertyError;
			break;
	};

Done:
	return theAnswer;
}

static OSStatus	SalsaHAL_SetBoxPropertyData(AudioServerPlugInDriverRef inDriver, AudioObjectID inObjectID, pid_t inClientProcessID, const AudioObjectPropertyAddress* inAddress, UInt32 inQualifierDataSize, const void* inQualifierData, UInt32 inDataSize, const void* inData, UInt32* outNumberPropertiesChanged, AudioObjectPropertyAddress outChangedAddresses[2])
{
	#pragma unused(inClientProcessID, inQualifierDataSize, inQualifierData, inDataSize, inData)
	
	//	declare the local variables
	OSStatus theAnswer = 0;
	
	//	check the arguments
	FailWithAction(inDriver != gAudioServerPlugInDriverRef, theAnswer = kAudioHardwareBadObjectError, Done, "SalsaHAL_SetBoxPropertyData: bad driver reference");
	FailWithAction(inAddress == NULL, theAnswer = kAudioHardwareIllegalOperationError, Done, "SalsaHAL_SetBoxPropertyData: no address");
	FailWithAction(outNumberPropertiesChanged == NULL, theAnswer = kAudioHardwareIllegalOperationError, Done, "SalsaHAL_SetBoxPropertyData: no place to return the number of properties that changed");
	FailWithAction(outChangedAddresses == NULL, theAnswer = kAudioHardwareIllegalOperationError, Done, "SalsaHAL_SetBoxPropertyData: no place to return the properties that changed");
	FailWithAction(inObjectID != kObjectID_Box, theAnswer = kAudioHardwareBadObjectError, Done, "SalsaHAL_SetBoxPropertyData: not the box object");
	
	//	initialize the returned number of changed properties
	*outNumberPropertiesChanged = 0;
	
	//	Note that for each object, this driver implements all the required properties plus a few
	//	extras that are useful but not required. There is more detailed commentary about each
	//	property in the SalsaHAL_GetPlugInPropertyData() method.
	switch(inAddress->mSelector)
	{
		case kAudioObjectPropertyName:
			//	Boxes should allow their name to be editable
			{
				FailWithAction(inDataSize != sizeof(CFStringRef), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_SetBoxPropertyData: wrong size for the data for kAudioObjectPropertyName");
				FailWithAction(inData == NULL, theAnswer = kAudioHardwareIllegalOperationError, Done, "SalsaHAL_SetBoxPropertyData: no data to set for kAudioObjectPropertyName");
				CFStringRef* theNewName = (CFStringRef*)inData;
				pthread_mutex_lock(&gPlugIn_StateMutex);
				if((theNewName != NULL) && (*theNewName != NULL))
				{
					CFRetain(*theNewName);
				}
				if(gBox_Name != NULL)
				{
					CFRelease(gBox_Name);
				}
				gBox_Name = *theNewName;
				pthread_mutex_unlock(&gPlugIn_StateMutex);
				*outNumberPropertiesChanged = 1;
				outChangedAddresses[0].mSelector = kAudioObjectPropertyName;
				outChangedAddresses[0].mScope = kAudioObjectPropertyScopeGlobal;
                outChangedAddresses[0].mElement = kAudioObjectPropertyElementMain;
			}
			break;
			
		case kAudioObjectPropertyIdentify:
			//	since we don't have any actual hardware to flash, we will schedule a notificaiton for
			//	this property off into the future as a testing thing. Note that a real implementation
			//	of this property should only send the notificaiton if the hardware wants the app to
			//	flash it's UI for the device.
			{
				syslog(LOG_NOTICE, "The identify property has been set on the Box implemented by the SalsaHAL driver.");
				FailWithAction(inDataSize != sizeof(UInt32), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_SetBoxPropertyData: wrong size for the data for kAudioObjectPropertyIdentify");
				dispatch_after(dispatch_time(0, 2ULL * 1000ULL * 1000ULL * 1000ULL), dispatch_get_global_queue(DISPATCH_QUEUE_PRIORITY_DEFAULT, 0),	^()
																																		{
																																			AudioObjectPropertyAddress theAddress = { kAudioObjectPropertyIdentify, kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain };
																																			gPlugIn_Host->PropertiesChanged(gPlugIn_Host, kObjectID_Box, 1, &theAddress);
																																		});
			}
			break;
			
		case kAudioBoxPropertyAcquired:
			//	When the box is acquired, it means the contents, namely the device, are available to the system
			{
				FailWithAction(inDataSize != sizeof(UInt32), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_SetBoxPropertyData: wrong size for the data for kAudioBoxPropertyAcquired");
				pthread_mutex_lock(&gPlugIn_StateMutex);
				if(gBox_Acquired != (*((UInt32*)inData) != 0))
				{
					//	the new value is different from the old value, so save it
					gBox_Acquired = *((UInt32*)inData) != 0;
					gPlugIn_Host->WriteToStorage(gPlugIn_Host, CFSTR("box acquired"), gBox_Acquired ? kCFBooleanTrue : kCFBooleanFalse);
					
					//	and it means that this property and the device list property have changed
					*outNumberPropertiesChanged = 2;
					outChangedAddresses[0].mSelector = kAudioBoxPropertyAcquired;
					outChangedAddresses[0].mScope = kAudioObjectPropertyScopeGlobal;
					outChangedAddresses[0].mElement = kAudioObjectPropertyElementMain;
					outChangedAddresses[1].mSelector = kAudioBoxPropertyDeviceList;
					outChangedAddresses[1].mScope = kAudioObjectPropertyScopeGlobal;
					outChangedAddresses[1].mElement = kAudioObjectPropertyElementMain;
					
					//	but it also means that the device list has changed for the plug-in too
					dispatch_async(dispatch_get_global_queue(DISPATCH_QUEUE_PRIORITY_DEFAULT, 0),	^()
																									{
																										AudioObjectPropertyAddress theAddress = { kAudioPlugInPropertyDeviceList, kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain };
																										gPlugIn_Host->PropertiesChanged(gPlugIn_Host, kObjectID_PlugIn, 1, &theAddress);
																									});
				}
				pthread_mutex_unlock(&gPlugIn_StateMutex);
			}
			break;
			
		default:
			theAnswer = kAudioHardwareUnknownPropertyError;
			break;
	};

Done:
	return theAnswer;
}

#pragma mark Device Property Operations

static Boolean	SalsaHAL_HasDeviceProperty(AudioServerPlugInDriverRef inDriver, AudioObjectID inObjectID, pid_t inClientProcessID, const AudioObjectPropertyAddress* inAddress)
{
	//	This method returns whether or not the given object has the given property.
	
	#pragma unused(inClientProcessID)
	
	//	declare the local variables
	Boolean theAnswer = false;
	
	//	check the arguments
	FailIf(inDriver != gAudioServerPlugInDriverRef, Done, "SalsaHAL_HasDeviceProperty: bad driver reference");
	FailIf(inAddress == NULL, Done, "SalsaHAL_HasDeviceProperty: no address");
	FailIf(inObjectID != kObjectID_Device, Done, "SalsaHAL_HasDeviceProperty: not the device object");
	
	//	Note that for each object, this driver implements all the required properties plus a few
	//	extras that are useful but not required. There is more detailed commentary about each
	//	property in the SalsaHAL_GetDevicePropertyData() method.
	switch(inAddress->mSelector)
	{
		case kAudioObjectPropertyBaseClass:
		case kAudioObjectPropertyClass:
		case kAudioObjectPropertyOwner:
		case kAudioObjectPropertyName:
		case kAudioObjectPropertyManufacturer:
		case kAudioObjectPropertyOwnedObjects:
		case kAudioDevicePropertyDeviceUID:
		case kAudioDevicePropertyModelUID:
		case kAudioDevicePropertyTransportType:
		case kAudioDevicePropertyRelatedDevices:
		case kAudioDevicePropertyClockDomain:
		case kAudioDevicePropertyDeviceIsAlive:
		case kAudioDevicePropertyDeviceIsRunning:
		case kAudioObjectPropertyControlList:
		case kAudioDevicePropertyNominalSampleRate:
		case kAudioDevicePropertyAvailableNominalSampleRates:
		case kAudioDevicePropertyIsHidden:
		case kAudioDevicePropertyZeroTimeStampPeriod:
		case kAudioDevicePropertyIcon:
		case kAudioDevicePropertyStreams:
		case kAudioDevicePropertySupportsMixing:
		case kAudioDevicePropertyHogMode:
		case kAudioDevicePropertyDeviceIsRunningSomewhere:
		case kSalsaHALProbeProperty:
		case kSalsaHALRecordProperty:
		case kSalsaHALClockProperty:
		case kAudioObjectPropertyCustomPropertyInfoList:
			theAnswer = true;
			break;
			
		case kAudioDevicePropertyDeviceCanBeDefaultDevice:
		case kAudioDevicePropertyDeviceCanBeDefaultSystemDevice:
		case kAudioDevicePropertyLatency:
		case kAudioDevicePropertySafetyOffset:
		case kAudioDevicePropertyPreferredChannelsForStereo:
		case kAudioDevicePropertyPreferredChannelLayout:
		case kAudioDevicePropertyStreamConfiguration:
			theAnswer = (inAddress->mScope == kAudioObjectPropertyScopeInput) || (inAddress->mScope == kAudioObjectPropertyScopeOutput);
			break;
		
		case kAudioObjectPropertyElementName:
			theAnswer = inAddress->mElement <= 2;
			break;
	};

Done:
	return theAnswer;
}

static OSStatus	SalsaHAL_IsDevicePropertySettable(AudioServerPlugInDriverRef inDriver, AudioObjectID inObjectID, pid_t inClientProcessID, const AudioObjectPropertyAddress* inAddress, Boolean* outIsSettable)
{
	//	This method returns whether or not the given property on the object can have its value
	//	changed.
	
	#pragma unused(inClientProcessID)
	
	//	declare the local variables
	OSStatus theAnswer = 0;
	
	//	check the arguments
	FailWithAction(inDriver != gAudioServerPlugInDriverRef, theAnswer = kAudioHardwareBadObjectError, Done, "SalsaHAL_IsDevicePropertySettable: bad driver reference");
	FailWithAction(inAddress == NULL, theAnswer = kAudioHardwareIllegalOperationError, Done, "SalsaHAL_IsDevicePropertySettable: no address");
	FailWithAction(outIsSettable == NULL, theAnswer = kAudioHardwareIllegalOperationError, Done, "SalsaHAL_IsDevicePropertySettable: no place to put the return value");
	FailWithAction(inObjectID != kObjectID_Device, theAnswer = kAudioHardwareBadObjectError, Done, "SalsaHAL_IsDevicePropertySettable: not the device object");
	
	//	Note that for each object, this driver implements all the required properties plus a few
	//	extras that are useful but not required. There is more detailed commentary about each
	//	property in the SalsaHAL_GetDevicePropertyData() method.
	switch(inAddress->mSelector)
	{
		case kAudioObjectPropertyBaseClass:
		case kAudioObjectPropertyClass:
		case kAudioObjectPropertyOwner:
		case kAudioObjectPropertyName:
		case kAudioObjectPropertyManufacturer:
		case kAudioObjectPropertyElementName:
		case kAudioObjectPropertyOwnedObjects:
		case kAudioDevicePropertyDeviceUID:
		case kAudioDevicePropertyModelUID:
		case kAudioDevicePropertyTransportType:
		case kAudioDevicePropertyRelatedDevices:
		case kAudioDevicePropertyClockDomain:
		case kAudioDevicePropertyDeviceIsAlive:
		case kAudioDevicePropertyDeviceIsRunning:
		case kAudioDevicePropertyDeviceCanBeDefaultDevice:
		case kAudioDevicePropertyDeviceCanBeDefaultSystemDevice:
		case kAudioDevicePropertyLatency:
		case kAudioDevicePropertyStreams:
		case kAudioObjectPropertyControlList:
		case kAudioDevicePropertySafetyOffset:
		case kAudioDevicePropertyAvailableNominalSampleRates:
		case kAudioDevicePropertyIsHidden:
		case kAudioDevicePropertyPreferredChannelsForStereo:
		case kAudioDevicePropertyPreferredChannelLayout:
		case kAudioDevicePropertyZeroTimeStampPeriod:
		case kAudioDevicePropertyIcon:
		case kAudioDevicePropertyStreamConfiguration:
		case kAudioDevicePropertyDeviceIsRunningSomewhere:
		case kSalsaHALProbeProperty:
		case kSalsaHALClockProperty:
		case kAudioObjectPropertyCustomPropertyInfoList:
			*outIsSettable = false;
			break;
		
		case kAudioDevicePropertyNominalSampleRate:
		case kSalsaHALRecordProperty:
			*outIsSettable = true;
			break;
		
		default:
			theAnswer = kAudioHardwareUnknownPropertyError;
			break;
	};

Done:
	return theAnswer;
}

static OSStatus	SalsaHAL_GetDevicePropertyDataSize(AudioServerPlugInDriverRef inDriver, AudioObjectID inObjectID, pid_t inClientProcessID, const AudioObjectPropertyAddress* inAddress, UInt32 inQualifierDataSize, const void* inQualifierData, UInt32* outDataSize)
{
	//	This method returns the byte size of the property's data.
	
	#pragma unused(inClientProcessID, inQualifierDataSize, inQualifierData)
	
	//	declare the local variables
	OSStatus theAnswer = 0;
	
	//	check the arguments
	FailWithAction(inDriver != gAudioServerPlugInDriverRef, theAnswer = kAudioHardwareBadObjectError, Done, "SalsaHAL_GetDevicePropertyDataSize: bad driver reference");
	FailWithAction(inAddress == NULL, theAnswer = kAudioHardwareIllegalOperationError, Done, "SalsaHAL_GetDevicePropertyDataSize: no address");
	FailWithAction(outDataSize == NULL, theAnswer = kAudioHardwareIllegalOperationError, Done, "SalsaHAL_GetDevicePropertyDataSize: no place to put the return value");
	FailWithAction(inObjectID != kObjectID_Device, theAnswer = kAudioHardwareBadObjectError, Done, "SalsaHAL_GetDevicePropertyDataSize: not the device object");
	
	//	Note that for each object, this driver implements all the required properties plus a few
	//	extras that are useful but not required. There is more detailed commentary about each
	//	property in the SalsaHAL_GetDevicePropertyData() method.
	switch(inAddress->mSelector)
	{
		case kAudioObjectPropertyBaseClass:
			*outDataSize = sizeof(AudioClassID);
			break;
			
		case kAudioObjectPropertyClass:
			*outDataSize = sizeof(AudioClassID);
			break;
			
		case kAudioObjectPropertyOwner:
			*outDataSize = sizeof(AudioObjectID);
			break;
			
		case kAudioObjectPropertyName:
			*outDataSize = sizeof(CFStringRef);
			break;
			
		case kAudioObjectPropertyManufacturer:
			*outDataSize = sizeof(CFStringRef);
			break;
			
		case kAudioObjectPropertyElementName:
			*outDataSize = sizeof(CFStringRef);
			break;
			
		case kAudioObjectPropertyOwnedObjects:
			switch(inAddress->mScope)
			{
				case kAudioObjectPropertyScopeGlobal:
					*outDataSize = 8 * sizeof(AudioObjectID);
					break;
					
				case kAudioObjectPropertyScopeInput:
					*outDataSize = 4 * sizeof(AudioObjectID);
					break;
					
				case kAudioObjectPropertyScopeOutput:
					*outDataSize = 4 * sizeof(AudioObjectID);
					break;
			};
			break;

		case kAudioDevicePropertyDeviceUID:
			*outDataSize = sizeof(CFStringRef);
			break;

		case kAudioDevicePropertyModelUID:
			*outDataSize = sizeof(CFStringRef);
			break;

		case kAudioDevicePropertyTransportType:
			*outDataSize = sizeof(UInt32);
			break;

		case kAudioDevicePropertyRelatedDevices:
			*outDataSize = sizeof(AudioObjectID);
			break;

		case kAudioDevicePropertyClockDomain:
			*outDataSize = sizeof(UInt32);
			break;

		case kAudioDevicePropertyDeviceIsAlive:
			*outDataSize = sizeof(AudioClassID);
			break;

		case kAudioDevicePropertyDeviceIsRunning:
			*outDataSize = sizeof(UInt32);
			break;

		case kAudioDevicePropertyDeviceCanBeDefaultDevice:
			*outDataSize = sizeof(UInt32);
			break;

		case kAudioDevicePropertyDeviceCanBeDefaultSystemDevice:
			*outDataSize = sizeof(UInt32);
			break;

		case kAudioDevicePropertyLatency:
			*outDataSize = sizeof(UInt32);
			break;

		case kAudioDevicePropertyStreams:
			switch(inAddress->mScope)
			{
				case kAudioObjectPropertyScopeGlobal:
#if SALSA_HAL_OUTPUT_ONLY
					*outDataSize = 1 * sizeof(AudioObjectID);
#else
					*outDataSize = 2 * sizeof(AudioObjectID);
#endif
					break;
					
				case kAudioObjectPropertyScopeInput:
#if SALSA_HAL_OUTPUT_ONLY
					*outDataSize = 0;
#else
					*outDataSize = 1 * sizeof(AudioObjectID);
#endif
					break;
					
				case kAudioObjectPropertyScopeOutput:
					*outDataSize = 1 * sizeof(AudioObjectID);
					break;
			};
			break;

		case kAudioObjectPropertyControlList:
			*outDataSize = 7 * sizeof(AudioObjectID);
			break;

		case kAudioDevicePropertySafetyOffset:
		case kAudioDevicePropertySupportsMixing:
		case kAudioDevicePropertyHogMode:
		case kAudioDevicePropertyDeviceIsRunningSomewhere:
			*outDataSize = sizeof(UInt32);
			break;

		case kAudioDevicePropertyStreamConfiguration:
			*outDataSize = sizeof(AudioBufferList);
			break;

		case kSalsaHALProbeProperty:
		case kSalsaHALRecordProperty:
		case kSalsaHALClockProperty:
			*outDataSize = sizeof(CFPropertyListRef);
			break;

		case kAudioObjectPropertyCustomPropertyInfoList:
			*outDataSize = 3 * sizeof(AudioServerPlugInCustomPropertyInfo);
			break;

		case kAudioDevicePropertyNominalSampleRate:
			*outDataSize = sizeof(Float64);
			break;

		case kAudioDevicePropertyAvailableNominalSampleRates:
			*outDataSize = 1 * sizeof(AudioValueRange);
			break;
		
		case kAudioDevicePropertyIsHidden:
			*outDataSize = sizeof(UInt32);
			break;

		case kAudioDevicePropertyPreferredChannelsForStereo:
			*outDataSize = 2 * sizeof(UInt32);
			break;

		case kAudioDevicePropertyPreferredChannelLayout:
			*outDataSize = offsetof(AudioChannelLayout, mChannelDescriptions) + (SalsaHAL_CurrentChannels(inAddress->mScope) * sizeof(AudioChannelDescription));
			break;

		case kAudioDevicePropertyZeroTimeStampPeriod:
			*outDataSize = sizeof(UInt32);
			break;

		case kAudioDevicePropertyIcon:
			*outDataSize = sizeof(CFURLRef);
			break;

		default:
			theAnswer = kAudioHardwareUnknownPropertyError;
			break;
	};

Done:
	return theAnswer;
}

static OSStatus	SalsaHAL_GetDevicePropertyData(AudioServerPlugInDriverRef inDriver, AudioObjectID inObjectID, pid_t inClientProcessID, const AudioObjectPropertyAddress* inAddress, UInt32 inQualifierDataSize, const void* inQualifierData, UInt32 inDataSize, UInt32* outDataSize, void* outData)
{
	#pragma unused(inClientProcessID, inQualifierDataSize, inQualifierData)
	
	//	declare the local variables
	OSStatus theAnswer = 0;
	UInt32 theNumberItemsToFetch;
	UInt32 theItemIndex;
	
	//	check the arguments
	FailWithAction(inDriver != gAudioServerPlugInDriverRef, theAnswer = kAudioHardwareBadObjectError, Done, "SalsaHAL_GetDevicePropertyData: bad driver reference");
	FailWithAction(inAddress == NULL, theAnswer = kAudioHardwareIllegalOperationError, Done, "SalsaHAL_GetDevicePropertyData: no address");
	FailWithAction(outDataSize == NULL, theAnswer = kAudioHardwareIllegalOperationError, Done, "SalsaHAL_GetDevicePropertyData: no place to put the return value size");
	FailWithAction(outData == NULL, theAnswer = kAudioHardwareIllegalOperationError, Done, "SalsaHAL_GetDevicePropertyData: no place to put the return value");
	FailWithAction(inObjectID != kObjectID_Device, theAnswer = kAudioHardwareBadObjectError, Done, "SalsaHAL_GetDevicePropertyData: not the device object");
	
	//	Note that for each object, this driver implements all the required properties plus a few
	//	extras that are useful but not required.
	//
	//	Also, since most of the data that will get returned is static, there are few instances where
	//	it is necessary to lock the state mutex.
	switch(inAddress->mSelector)
	{
		case kAudioObjectPropertyBaseClass:
			//	The base class for kAudioDeviceClassID is kAudioObjectClassID
			FailWithAction(inDataSize < sizeof(AudioClassID), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetDevicePropertyData: not enough space for the return value of kAudioObjectPropertyBaseClass for the device");
			*((AudioClassID*)outData) = kAudioObjectClassID;
			*outDataSize = sizeof(AudioClassID);
			break;
			
		case kAudioObjectPropertyClass:
			//	The class is always kAudioDeviceClassID for devices created by drivers
			FailWithAction(inDataSize < sizeof(AudioClassID), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetDevicePropertyData: not enough space for the return value of kAudioObjectPropertyClass for the device");
			*((AudioClassID*)outData) = kAudioDeviceClassID;
			*outDataSize = sizeof(AudioClassID);
			break;
			
		case kAudioObjectPropertyOwner:
			//	The device's owner is the plug-in object
			FailWithAction(inDataSize < sizeof(AudioObjectID), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetDevicePropertyData: not enough space for the return value of kAudioObjectPropertyOwner for the device");
			*((AudioObjectID*)outData) = kObjectID_PlugIn;
			*outDataSize = sizeof(AudioObjectID);
			break;
			
		case kAudioObjectPropertyName:
			//	This is the human readable name of the device.
			FailWithAction(inDataSize < sizeof(CFStringRef), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetDevicePropertyData: not enough space for the return value of kAudioObjectPropertyName for the device");
			*((CFStringRef*)outData) = CFSTR(SALSA_HAL_DEVICE_NAME);
			*outDataSize = sizeof(CFStringRef);
			break;
			
		case kAudioObjectPropertyManufacturer:
			//	This is the human readable name of the maker of the plug-in.
			FailWithAction(inDataSize < sizeof(CFStringRef), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetDevicePropertyData: not enough space for the return value of kAudioObjectPropertyManufacturer for the device");
			*((CFStringRef*)outData) = CFSTR("Salsa");
			*outDataSize = sizeof(CFStringRef);
			break;
			
		case kAudioObjectPropertyElementName:
			//	This is the human readable name of the maker of the plug-in.
			FailWithAction(inDataSize < sizeof(CFStringRef), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetDevicePropertyData: not enough space for the return value of kAudioObjectPropertyElementName for the device");
			switch(inAddress->mElement)
			{
				case 0:
					*((CFStringRef*)outData) = CFSTR("MasterElementName");
					break;
				
				case 1:
					*((CFStringRef*)outData) = CFSTR("LeftElementName");
					break;
				
				case 2:
					*((CFStringRef*)outData) = CFSTR("RightElementName");
					break;
				
				default:
					*((CFStringRef*)outData) = CFSTR("unknown");
					break;
				
			};
			*outDataSize = sizeof(CFStringRef);
			break;
			
		case kAudioObjectPropertyOwnedObjects:
			//	Calculate the number of items that have been requested. Note that this
			//	number is allowed to be smaller than the actual size of the list. In such
			//	case, only that number of items will be returned
			theNumberItemsToFetch = inDataSize / sizeof(AudioObjectID);
			
			//	The device owns its streams and controls. Note that what is returned here
			//	depends on the scope requested.
			switch(inAddress->mScope)
			{
				case kAudioObjectPropertyScopeGlobal:
					//	global scope means return all objects
					if(theNumberItemsToFetch > 9)
					{
						theNumberItemsToFetch = 9;
					}
					
					//	fill out the list with as many objects as requested, which is everything
					for(theItemIndex = 0; theItemIndex < theNumberItemsToFetch; ++theItemIndex)
					{
						((AudioObjectID*)outData)[theItemIndex] = kObjectID_Stream_Input + theItemIndex;
					}
					break;
					
				case kAudioObjectPropertyScopeInput:
					//	input scope means just the objects on the input side
					if(theNumberItemsToFetch > 4)
					{
						theNumberItemsToFetch = 4;
					}
					
					//	fill out the list with the right objects
					for(theItemIndex = 0; theItemIndex < theNumberItemsToFetch; ++theItemIndex)
					{
						((AudioObjectID*)outData)[theItemIndex] = kObjectID_Stream_Input + theItemIndex;
					}
					break;
					
				case kAudioObjectPropertyScopeOutput:
					//	output scope means just the objects on the output side
					if(theNumberItemsToFetch > 4)
					{
						theNumberItemsToFetch = 4;
					}
					
					//	fill out the list with the right objects
					for(theItemIndex = 0; theItemIndex < theNumberItemsToFetch; ++theItemIndex)
					{
						((AudioObjectID*)outData)[theItemIndex] = kObjectID_Stream_Output + theItemIndex;
					}
					break;
			};
			
			//	report how much we wrote
			*outDataSize = theNumberItemsToFetch * sizeof(AudioObjectID);
			break;

		case kAudioDevicePropertyDeviceUID:
			//	This is a CFString that is a persistent token that can identify the same
			//	audio device across boot sessions. Note that two instances of the same
			//	device must have different values for this property.
			FailWithAction(inDataSize < sizeof(CFStringRef), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetDevicePropertyData: not enough space for the return value of kAudioDevicePropertyDeviceUID for the device");
			*((CFStringRef*)outData) = CFSTR(kDevice_UID);
			*outDataSize = sizeof(CFStringRef);
			break;

		case kAudioDevicePropertyModelUID:
			//	This is a CFString that is a persistent token that can identify audio
			//	devices that are the same kind of device. Note that two instances of the
			//	save device must have the same value for this property.
			FailWithAction(inDataSize < sizeof(CFStringRef), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetDevicePropertyData: not enough space for the return value of kAudioDevicePropertyModelUID for the device");
			*((CFStringRef*)outData) = CFSTR(kDevice_ModelUID);
			*outDataSize = sizeof(CFStringRef);
			break;

		case kAudioDevicePropertyTransportType:
			//	This value represents how the device is attached to the system. This can be
			//	any 32 bit integer, but common values for this property are defined in
			//	<CoreAudio/AudioHardwareBase.h>
			FailWithAction(inDataSize < sizeof(UInt32), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetDevicePropertyData: not enough space for the return value of kAudioDevicePropertyTransportType for the device");
			*((UInt32*)outData) = kAudioDeviceTransportTypeVirtual;
			*outDataSize = sizeof(UInt32);
			break;

		case kAudioDevicePropertyRelatedDevices:
			//	The related devices property identifys device objects that are very closely
			//	related. Generally, this is for relating devices that are packaged together
			//	in the hardware such as when the input side and the output side of a piece
			//	of hardware can be clocked separately and therefore need to be represented
			//	as separate AudioDevice objects. In such case, both devices would report
			//	that they are related to each other. Note that at minimum, a device is
			//	related to itself, so this list will always be at least one item long.

			//	Calculate the number of items that have been requested. Note that this
			//	number is allowed to be smaller than the actual size of the list. In such
			//	case, only that number of items will be returned
			theNumberItemsToFetch = inDataSize / sizeof(AudioObjectID);
			
			//	we only have the one device...
			if(theNumberItemsToFetch > 1)
			{
				theNumberItemsToFetch = 1;
			}
			
			//	Write the devices' object IDs into the return value
			if(theNumberItemsToFetch > 0)
			{
				((AudioObjectID*)outData)[0] = kObjectID_Device;
			}
			
			//	report how much we wrote
			*outDataSize = theNumberItemsToFetch * sizeof(AudioObjectID);
			break;

		case kAudioDevicePropertyClockDomain:
			//	This property allows the device to declare what other devices it is
			//	synchronized with in hardware. The way it works is that if two devices have
			//	the same value for this property and the value is not zero, then the two
			//	devices are synchronized in hardware. Note that a device that either can't
			//	be synchronized with others or doesn't know should return 0 for this
			//	property.
			FailWithAction(inDataSize < sizeof(UInt32), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetDevicePropertyData: not enough space for the return value of kAudioDevicePropertyClockDomain for the device");
			*((UInt32*)outData) = 0;
			*outDataSize = sizeof(UInt32);
			break;

		case kAudioDevicePropertyDeviceIsAlive:
			//	This property returns whether or not the device is alive. Note that it is
			//	note uncommon for a device to be dead but still momentarily availble in the
			//	device list. In the case of this device, it will always be alive.
			FailWithAction(inDataSize < sizeof(UInt32), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetDevicePropertyData: not enough space for the return value of kAudioDevicePropertyDeviceIsAlive for the device");
			*((UInt32*)outData) = 1;
			*outDataSize = sizeof(UInt32);
			break;

		case kAudioDevicePropertyDeviceIsRunning:
			//	This property returns whether or not IO is running for the device. Note that
			//	we need to take both the state lock to check this value for thread safety.
			FailWithAction(inDataSize < sizeof(UInt32), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetDevicePropertyData: not enough space for the return value of kAudioDevicePropertyDeviceIsRunning for the device");
			pthread_mutex_lock(&gPlugIn_StateMutex);
			*((UInt32*)outData) = ((gDevice_IOIsRunning > 0) > 0) ? 1 : 0;
			pthread_mutex_unlock(&gPlugIn_StateMutex);
			*outDataSize = sizeof(UInt32);
			break;

		case kAudioDevicePropertyDeviceCanBeDefaultDevice:
			//	Do not become the system default; the virtual carrier device is
			//	selected explicitly by the player.
			FailWithAction(inDataSize < sizeof(UInt32), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetDevicePropertyData: not enough space for the return value of kAudioDevicePropertyDeviceCanBeDefaultDevice for the device");
			*((UInt32*)outData) = 0;
			*outDataSize = sizeof(UInt32);
			break;

		case kAudioDevicePropertyDeviceCanBeDefaultSystemDevice:
			//	This property returns whether or not the device wants to be the system
			//	default device. This is the device that is used to play interface sounds and
			//	other incidental or UI-related sounds on. Most devices should allow this
			//	although devices with lots of latency may not want to.
			FailWithAction(inDataSize < sizeof(UInt32), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetDevicePropertyData: not enough space for the return value of kAudioDevicePropertyDeviceCanBeDefaultSystemDevice for the device");
			*((UInt32*)outData) = 0;
			*outDataSize = sizeof(UInt32);
			break;

		case kAudioDevicePropertyLatency:
			//	Report the fixed session delay declared by the service. The value
			//	is in this device's 192 kHz carrier frames.
			FailWithAction(inDataSize < sizeof(UInt32), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetDevicePropertyData: not enough space for the return value of kAudioDevicePropertyLatency for the device");
			*((UInt32*)outData) = SalsaHAL_DeclaredLatencyFrames();
			*outDataSize = sizeof(UInt32);
			break;



		case kAudioDevicePropertyStreams:
			//	Calculate the number of items that have been requested. Note that this
			//	number is allowed to be smaller than the actual size of the list. In such
			//	case, only that number of items will be returned
			theNumberItemsToFetch = inDataSize / sizeof(AudioObjectID);
			
			//	Note that what is returned here depends on the scope requested.
			switch(inAddress->mScope)
			{
				case kAudioObjectPropertyScopeGlobal:
#if SALSA_HAL_OUTPUT_ONLY
					//	global scope means return all streams; production builds
					//	expose only the carrier output.
					if(theNumberItemsToFetch > 1)
					{
						theNumberItemsToFetch = 1;
					}
					if(theNumberItemsToFetch > 0)
					{
						((AudioObjectID*)outData)[0] = kObjectID_Stream_Output;
					}
#else
					//	global scope means return all streams
					if(theNumberItemsToFetch > 2)
					{
						theNumberItemsToFetch = 2;
					}
					
					//	fill out the list with as many objects as requested
					if(theNumberItemsToFetch > 0)
					{
						((AudioObjectID*)outData)[0] = kObjectID_Stream_Input;
					}
					if(theNumberItemsToFetch > 1)
					{
						((AudioObjectID*)outData)[1] = kObjectID_Stream_Output;
					}
#endif
					break;
					
				case kAudioObjectPropertyScopeInput:
#if SALSA_HAL_OUTPUT_ONLY
					theNumberItemsToFetch = 0;
#else
					//	input scope means just the objects on the input side
					if(theNumberItemsToFetch > 1)
					{
						theNumberItemsToFetch = 1;
					}
					
					//	fill out the list with as many objects as requested
					if(theNumberItemsToFetch > 0)
					{
						((AudioObjectID*)outData)[0] = kObjectID_Stream_Input;
					}
#endif
					break;
					
				case kAudioObjectPropertyScopeOutput:
					//	output scope means just the objects on the output side
					if(theNumberItemsToFetch > 1)
					{
						theNumberItemsToFetch = 1;
					}
					
					//	fill out the list with as many objects as requested
					if(theNumberItemsToFetch > 0)
					{
						((AudioObjectID*)outData)[0] = kObjectID_Stream_Output;
					}
					break;
			};
			
			//	report how much we wrote
			*outDataSize = theNumberItemsToFetch * sizeof(AudioObjectID);
			break;

		case kAudioObjectPropertyControlList:
			//	Calculate the number of items that have been requested. Note that this
			//	number is allowed to be smaller than the actual size of the list. In such
			//	case, only that number of items will be returned
			theNumberItemsToFetch = inDataSize / sizeof(AudioObjectID);
			if(theNumberItemsToFetch > 7)
			{
				theNumberItemsToFetch = 7;
			}
			
			//	fill out the list with as many objects as requested, which is everything
			for(theItemIndex = 0; theItemIndex < theNumberItemsToFetch; ++theItemIndex)
			{
				if(theItemIndex < 3)
				{
					((AudioObjectID*)outData)[theItemIndex] = kObjectID_Volume_Input_Master + theItemIndex;
				}
				else
				{
					((AudioObjectID*)outData)[theItemIndex] = kObjectID_Volume_Output_Master + (theItemIndex - 3);
				}
			}
			
			//	report how much we wrote
			*outDataSize = theNumberItemsToFetch * sizeof(AudioObjectID);
			break;

		case kAudioDevicePropertySupportsMixing:
			FailWithAction(inDataSize < sizeof(UInt32), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetDevicePropertyData: not enough space for kAudioDevicePropertySupportsMixing for the device");
			*((UInt32*)outData) = 0;
			*outDataSize = sizeof(UInt32);
			break;

		case kAudioDevicePropertyHogMode:
			FailWithAction(inDataSize < sizeof(pid_t), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetDevicePropertyData: not enough space for kAudioDevicePropertyHogMode for the device");
			*((pid_t*)outData) = gHogModePID;
			*outDataSize = sizeof(pid_t);
			break;

		case kAudioDevicePropertyDeviceIsRunningSomewhere:
			FailWithAction(inDataSize < sizeof(UInt32), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetDevicePropertyData: not enough space for kAudioDevicePropertyDeviceIsRunningSomewhere for the device");
			*((UInt32*)outData) = (gDevice_IOIsRunning > 0) ? 1 : 0;
			*outDataSize = sizeof(UInt32);
			break;

		case kSalsaHALProbeProperty:
		{
			FailWithAction(inDataSize < sizeof(CFPropertyListRef), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetDevicePropertyData: probe buffer too small");
			SalsaHALProbe probe = {
				.starts = atomic_load(&gProbeStarts),
				.stops = atomic_load(&gProbeStops),
				.timestamps = atomic_load(&gProbeTimeStamps),
				.writes = atomic_load(&gProbeWrites),
				.writeFrames = atomic_load(&gProbeWriteFrames),
				.reads = atomic_load(&gProbeReads),
				.lastBufferFrames = atomic_load(&gProbeLastBufferFrames),
				.droppedFrames = atomic_load(&gCarrierDroppedFrames),
				.recordedBytes = atomic_load(&gProbeRecordedBytes),
				.recordOverflow = atomic_load(&gProbeRecordOverflow),
				.generation = atomic_load(&gSessionGeneration),
				.transportStatus = atomic_load(&gCarrierTransportStatus),
			};
			*(CFDataRef*)outData = CFDataCreate(NULL, (const UInt8*)&probe, sizeof(probe));
			*outDataSize = sizeof(CFDataRef);
			break;
		}

		case kSalsaHALClockProperty:
		{
			FailWithAction(inDataSize < sizeof(CFDataRef), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetDevicePropertyData: clock buffer too small");
			Float64 sample = 0;
			UInt64 host = 0;
			UInt64 seed = 0;
			SalsaHAL_GetZeroTimeStamp(gAudioServerPlugInDriverRef, kObjectID_Device, 0, &sample, &host, &seed);
			SalsaHALClock clock = {
				.sample = (uint64_t)sample,
				.host = host,
				.seed = seed,
			};
			*(CFDataRef*)outData = CFDataCreate(NULL, (const UInt8*)&clock, sizeof(clock));
			*outDataSize = sizeof(CFDataRef);
			break;
		}

		case kAudioObjectPropertyCustomPropertyInfoList:
			FailWithAction(inDataSize < 3 * sizeof(AudioServerPlugInCustomPropertyInfo), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetDevicePropertyData: custom info buffer too small");
			*(AudioServerPlugInCustomPropertyInfo*)outData = (AudioServerPlugInCustomPropertyInfo){
				.mSelector = kSalsaHALProbeProperty,
				.mPropertyDataType = kAudioServerPlugInCustomPropertyDataTypeCFPropertyList,
				.mQualifierDataType = kAudioServerPlugInCustomPropertyDataTypeNone,
			};
			((AudioServerPlugInCustomPropertyInfo*)outData)[1] = ((AudioServerPlugInCustomPropertyInfo*)outData)[0];
			((AudioServerPlugInCustomPropertyInfo*)outData)[1].mSelector = kSalsaHALRecordProperty;
			((AudioServerPlugInCustomPropertyInfo*)outData)[2] = ((AudioServerPlugInCustomPropertyInfo*)outData)[0];
			((AudioServerPlugInCustomPropertyInfo*)outData)[2].mSelector = kSalsaHALClockProperty;
			*outDataSize = 3 * sizeof(AudioServerPlugInCustomPropertyInfo);
			break;

		case kSalsaHALRecordProperty:
			FailWithAction(inDataSize < sizeof(CFDataRef), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetDevicePropertyData: record buffer too small");
			pthread_mutex_lock(&gPlugIn_StateMutex);
			if(gDevice_IOIsRunning != 0 || gProbeRecordData == NULL)
				theAnswer = kAudioHardwareIllegalOperationError;
			else
			{
				*(CFDataRef*)outData = CFDataCreate(NULL, gProbeRecordData, atomic_load(&gProbeRecordedBytes));
				*outDataSize = sizeof(CFDataRef);
			}
			pthread_mutex_unlock(&gPlugIn_StateMutex);
			break;

		case kAudioDevicePropertyStreamConfiguration:
			{
				FailWithAction(inDataSize < sizeof(AudioBufferList), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetDevicePropertyData: not enough space for kAudioDevicePropertyStreamConfiguration for the device");
#if SALSA_HAL_OUTPUT_ONLY
				if(inAddress->mScope == kAudioObjectPropertyScopeInput)
				{
					((AudioBufferList*)outData)->mNumberBuffers = 0;
					*outDataSize = sizeof(AudioBufferList);
					break;
				}
#endif
				UInt32 theChannels = (inAddress->mScope == kAudioObjectPropertyScopeInput) ? gStream_Input_Format.mChannelsPerFrame : gStream_Output_Format.mChannelsPerFrame;
				if(theChannels == 0)
				{
					theChannels = 2;
				}
				AudioBufferList* theList = (AudioBufferList*)outData;
				theList->mNumberBuffers = 1;
				theList->mBuffers[0].mNumberChannels = theChannels;
				theList->mBuffers[0].mDataByteSize = theChannels * 2 * kDefaultBufferFrames;
				theList->mBuffers[0].mData = NULL;
				*outDataSize = sizeof(AudioBufferList);
			}
			break;

		case kAudioDevicePropertySafetyOffset:
			//	This property returns the how close to now the HAL can read and write. For
			//	this, device, the value is 0 due to the fact that it always vends silence.
			FailWithAction(inDataSize < sizeof(UInt32), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetDevicePropertyData: not enough space for the return value of kAudioDevicePropertySafetyOffset for the device");
			*((UInt32*)outData) = kDevice_SafetyOffsetFrames;
			*outDataSize = sizeof(UInt32);
			break;

		case kAudioDevicePropertyNominalSampleRate:
			//	This property returns the nominal sample rate of the device. Note that we
			//	only need to take the state lock to get this value.
			FailWithAction(inDataSize < sizeof(Float64), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetDevicePropertyData: not enough space for the return value of kAudioDevicePropertyNominalSampleRate for the device");
			pthread_mutex_lock(&gPlugIn_StateMutex);
			*((Float64*)outData) = gDevice_SampleRate;
			pthread_mutex_unlock(&gPlugIn_StateMutex);
			*outDataSize = sizeof(Float64);
			break;

		case kAudioDevicePropertyAvailableNominalSampleRates:
			//	This returns all nominal sample rates the device supports as an array of
			//	AudioValueRangeStructs. Note that for discrete sampler rates, the range
			//	will have the minimum value equal to the maximum value.
			
			//	Calculate the number of items that have been requested. Note that this
			//	number is allowed to be smaller than the actual size of the list. In such
			//	case, only that number of items will be returned
			theNumberItemsToFetch = inDataSize / sizeof(AudioValueRange);
			
			//	clamp it to the number of items we have
			if(theNumberItemsToFetch > 1)
			{
				theNumberItemsToFetch = 1;
			}
			
			//	fill out the return array
			if(theNumberItemsToFetch > 0)
			{
				((AudioValueRange*)outData)[0].mMinimum = kSalsaHAL_CarrierRate;
				((AudioValueRange*)outData)[0].mMaximum = kSalsaHAL_CarrierRate;
			}
			
			//	report how much we wrote
			*outDataSize = theNumberItemsToFetch * sizeof(AudioValueRange);
			break;
		
		case kAudioDevicePropertyIsHidden:
			//	This returns whether or not the device is visible to clients.
			FailWithAction(inDataSize < sizeof(UInt32), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetDevicePropertyData: not enough space for the return value of kAudioDevicePropertyIsHidden for the device");
			*((UInt32*)outData) = 0;
			*outDataSize = sizeof(UInt32);
			break;

		case kAudioDevicePropertyPreferredChannelsForStereo:
			//	This property returns which two channesl to use as left/right for stereo
			//	data by default. Note that the channel numbers are 1-based.xz
			FailWithAction(inDataSize < (2 * sizeof(UInt32)), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetDevicePropertyData: not enough space for the return value of kAudioDevicePropertyPreferredChannelsForStereo for the device");
			((UInt32*)outData)[0] = 1;
			((UInt32*)outData)[1] = 2;
			*outDataSize = 2 * sizeof(UInt32);
			break;

		case kAudioDevicePropertyPreferredChannelLayout:
			//	L R C LFE Ls Rs Lsd Rsd, the exact order mpv uses for 7.1.
			{
				static const AudioChannelLabel theLabels[8] = {
					kAudioChannelLabel_Left, kAudioChannelLabel_Right,
					kAudioChannelLabel_Center, kAudioChannelLabel_LFEScreen,
					kAudioChannelLabel_LeftSurround, kAudioChannelLabel_RightSurround,
					kAudioChannelLabel_LeftSurroundDirect, kAudioChannelLabel_RightSurroundDirect,
				};
				UInt32 channels = SalsaHAL_CurrentChannels(inAddress->mScope);
				UInt32 theACLSize = offsetof(AudioChannelLayout, mChannelDescriptions) + (channels * sizeof(AudioChannelDescription));
				FailWithAction(inDataSize < theACLSize, theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetDevicePropertyData: not enough space for the return value of kAudioDevicePropertyPreferredChannelLayout for the device");
				((AudioChannelLayout*)outData)->mChannelLayoutTag = kAudioChannelLayoutTag_UseChannelDescriptions;
				((AudioChannelLayout*)outData)->mChannelBitmap = 0;
				((AudioChannelLayout*)outData)->mNumberChannelDescriptions = channels;
				for(theItemIndex = 0; theItemIndex < channels; ++theItemIndex)
				{
					((AudioChannelLayout*)outData)->mChannelDescriptions[theItemIndex].mChannelLabel = theLabels[theItemIndex];
					((AudioChannelLayout*)outData)->mChannelDescriptions[theItemIndex].mChannelFlags = 0;
					((AudioChannelLayout*)outData)->mChannelDescriptions[theItemIndex].mCoordinates[0] = 0;
					((AudioChannelLayout*)outData)->mChannelDescriptions[theItemIndex].mCoordinates[1] = 0;
					((AudioChannelLayout*)outData)->mChannelDescriptions[theItemIndex].mCoordinates[2] = 0;
				}
				*outDataSize = theACLSize;
			}
			break;

		case kAudioDevicePropertyZeroTimeStampPeriod:
			//	This property returns how many frames the HAL should expect to see between
			//	successive sample times in the zero time stamps this device provides.
			FailWithAction(inDataSize < sizeof(UInt32), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetDevicePropertyData: not enough space for the return value of kAudioDevicePropertyZeroTimeStampPeriod for the device");
			*((UInt32*)outData) = kDevice_RingBufferSize;
			*outDataSize = sizeof(UInt32);
			break;

		case kAudioDevicePropertyIcon:
			{
				//	This is a CFURL that points to the device's Icon in the plug-in's resource bundle.
				FailWithAction(inDataSize < sizeof(CFURLRef), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetDevicePropertyData: not enough space for the return value of kAudioDevicePropertyDeviceUID for the device");
				CFBundleRef theBundle = CFBundleGetBundleWithIdentifier(CFSTR(kPlugIn_BundleID));
				FailWithAction(theBundle == NULL, theAnswer = kAudioHardwareUnspecifiedError, Done, "SalsaHAL_GetDevicePropertyData: could not get the plug-in bundle for kAudioDevicePropertyIcon");
				CFURLRef theURL = CFBundleCopyResourceURL(theBundle, CFSTR("DeviceIcon.icns"), NULL, NULL);
				FailWithAction(theURL == NULL, theAnswer = kAudioHardwareUnspecifiedError, Done, "SalsaHAL_GetDevicePropertyData: could not get the URL for kAudioDevicePropertyIcon");
				*((CFURLRef*)outData) = theURL;
				*outDataSize = sizeof(CFURLRef);
			}
			break;
			
		default:
			theAnswer = kAudioHardwareUnknownPropertyError;
			break;
	};

Done:
	return theAnswer;
}

static OSStatus	SalsaHAL_SetDevicePropertyData(AudioServerPlugInDriverRef inDriver, AudioObjectID inObjectID, pid_t inClientProcessID, const AudioObjectPropertyAddress* inAddress, UInt32 inQualifierDataSize, const void* inQualifierData, UInt32 inDataSize, const void* inData, UInt32* outNumberPropertiesChanged, AudioObjectPropertyAddress outChangedAddresses[2])
{
	#pragma unused(inClientProcessID, inQualifierDataSize, inQualifierData)
	
	//	declare the local variables
	OSStatus theAnswer = 0;
	Float64 theOldSampleRate;
	UInt64 theNewSampleRate;
	
	//	check the arguments
	FailWithAction(inDriver != gAudioServerPlugInDriverRef, theAnswer = kAudioHardwareBadObjectError, Done, "SalsaHAL_SetDevicePropertyData: bad driver reference");
	FailWithAction(inAddress == NULL, theAnswer = kAudioHardwareIllegalOperationError, Done, "SalsaHAL_SetDevicePropertyData: no address");
	FailWithAction(outNumberPropertiesChanged == NULL, theAnswer = kAudioHardwareIllegalOperationError, Done, "SalsaHAL_SetDevicePropertyData: no place to return the number of properties that changed");
	FailWithAction(outChangedAddresses == NULL, theAnswer = kAudioHardwareIllegalOperationError, Done, "SalsaHAL_SetDevicePropertyData: no place to return the properties that changed");
	FailWithAction(inObjectID != kObjectID_Device, theAnswer = kAudioHardwareBadObjectError, Done, "SalsaHAL_SetDevicePropertyData: not the device object");
	
	//	initialize the returned number of changed properties
	*outNumberPropertiesChanged = 0;
	
	//	Note that for each object, this driver implements all the required properties plus a few
	//	extras that are useful but not required. There is more detailed commentary about each
	//	property in the SalsaHAL_GetDevicePropertyData() method.
	switch(inAddress->mSelector)
	{
		case kSalsaHALRecordProperty:
			FailWithAction(inDataSize != sizeof(CFBooleanRef), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_SetDevicePropertyData: record command size");
			FailWithAction(*(CFTypeRef*)inData != kCFBooleanTrue && *(CFTypeRef*)inData != kCFBooleanFalse, theAnswer = kAudioHardwareIllegalOperationError, Done, "SalsaHAL_SetDevicePropertyData: record command must be Boolean");
			pthread_mutex_lock(&gPlugIn_StateMutex);
			if(gDevice_IOIsRunning != 0)
				theAnswer = kAudioHardwareIllegalOperationError;
			else
			{
				free(gProbeRecordData);
				gProbeRecordData = NULL;
				if(*(CFTypeRef*)inData == kCFBooleanTrue)
				{
					gProbeRecordData = malloc(kProbeRecordCapacity);
					if(gProbeRecordData == NULL)
						theAnswer = kAudioHardwareUnspecifiedError;
					else
						memset(gProbeRecordData, 0, kProbeRecordCapacity);
					atomic_store(&gProbeRecordedBytes, 0);
					atomic_store(&gProbeRecordOverflow, 0);
				}
			}
			pthread_mutex_unlock(&gPlugIn_StateMutex);
			break;

		case kAudioDevicePropertySupportsMixing:
			FailWithAction(inDataSize != sizeof(UInt32), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_SetDevicePropertyData: wrong size for kAudioDevicePropertySupportsMixing");
			*outNumberPropertiesChanged = 1;
			outChangedAddresses[0].mSelector = kAudioDevicePropertySupportsMixing;
			outChangedAddresses[0].mScope = kAudioObjectPropertyScopeGlobal;
			outChangedAddresses[0].mElement = kAudioObjectPropertyElementMain;
			break;

		case kAudioDevicePropertyHogMode:
			FailWithAction(inDataSize != sizeof(pid_t), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_SetDevicePropertyData: wrong size for kAudioDevicePropertyHogMode");
			pthread_mutex_lock(&gPlugIn_StateMutex);
			gHogModePID = *((const pid_t*)inData);
			pthread_mutex_unlock(&gPlugIn_StateMutex);
			*outNumberPropertiesChanged = 1;
			outChangedAddresses[0].mSelector = kAudioDevicePropertyHogMode;
			outChangedAddresses[0].mScope = kAudioObjectPropertyScopeGlobal;
			outChangedAddresses[0].mElement = kAudioObjectPropertyElementMain;
			break;

		case kAudioDevicePropertyNominalSampleRate:
			//	Changing the sample rate needs to be handled via the
			//	RequestConfigChange/PerformConfigChange machinery.

			//	check the arguments
			FailWithAction(inDataSize != sizeof(Float64), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_SetDevicePropertyData: wrong size for the data for kAudioDevicePropertyNominalSampleRate");
			FailWithAction(*((const Float64*)inData) != kSalsaHAL_CarrierRate, theAnswer = kAudioHardwareIllegalOperationError, Done, "SalsaHAL_SetDevicePropertyData: unsupported value for kAudioDevicePropertyNominalSampleRate");
			
			//	make sure that the new value is different than the old value
			pthread_mutex_lock(&gPlugIn_StateMutex);
			theOldSampleRate = gDevice_SampleRate;
			pthread_mutex_unlock(&gPlugIn_StateMutex);
			if(*((const Float64*)inData) != theOldSampleRate)
			{
				*outNumberPropertiesChanged = 1;
				outChangedAddresses[0].mSelector = kAudioDevicePropertyNominalSampleRate;
				outChangedAddresses[0].mScope = kAudioObjectPropertyScopeGlobal;
				outChangedAddresses[0].mElement = kAudioObjectPropertyElementMain;
				//	we dispatch this so that the change can happen asynchronously
				theOldSampleRate = *((const Float64*)inData);
				theNewSampleRate = (UInt64)theOldSampleRate;
				dispatch_async(dispatch_get_global_queue(DISPATCH_QUEUE_PRIORITY_DEFAULT, 0), ^{ gPlugIn_Host->RequestDeviceConfigurationChange(gPlugIn_Host, kObjectID_Device, theNewSampleRate, NULL); });
			}
			break;
		
		default:
			theAnswer = kAudioHardwareUnknownPropertyError;
			break;
	};

Done:
	return theAnswer;
}

#pragma mark Stream Property Operations

static Boolean	SalsaHAL_HasStreamProperty(AudioServerPlugInDriverRef inDriver, AudioObjectID inObjectID, pid_t inClientProcessID, const AudioObjectPropertyAddress* inAddress)
{
	//	This method returns whether or not the given object has the given property.
	
	#pragma unused(inClientProcessID)
	
	//	declare the local variables
	Boolean theAnswer = false;
	
	//	check the arguments
	FailIf(inDriver != gAudioServerPlugInDriverRef, Done, "SalsaHAL_HasStreamProperty: bad driver reference");
	FailIf(inAddress == NULL, Done, "SalsaHAL_HasStreamProperty: no address");
	FailIf((inObjectID != kObjectID_Stream_Input) && (inObjectID != kObjectID_Stream_Output), Done, "SalsaHAL_HasStreamProperty: not a stream object");
	
	//	Note that for each object, this driver implements all the required properties plus a few
	//	extras that are useful but not required. There is more detailed commentary about each
	//	property in the SalsaHAL_GetStreamPropertyData() method.
	switch(inAddress->mSelector)
	{
		case kAudioObjectPropertyBaseClass:
		case kAudioObjectPropertyClass:
		case kAudioObjectPropertyOwner:
		case kAudioObjectPropertyOwnedObjects:
		case kAudioObjectPropertyName:
		case kAudioStreamPropertyIsActive:
		case kAudioStreamPropertyDirection:
		case kAudioStreamPropertyTerminalType:
		case kAudioStreamPropertyStartingChannel:
		case kAudioStreamPropertyLatency:
		case kAudioStreamPropertyVirtualFormat:
		case kAudioStreamPropertyPhysicalFormat:
		case kAudioStreamPropertyAvailableVirtualFormats:
		case kAudioStreamPropertyAvailablePhysicalFormats:
			theAnswer = true;
			break;
	};

Done:
	return theAnswer;
}

static OSStatus	SalsaHAL_IsStreamPropertySettable(AudioServerPlugInDriverRef inDriver, AudioObjectID inObjectID, pid_t inClientProcessID, const AudioObjectPropertyAddress* inAddress, Boolean* outIsSettable)
{
	//	This method returns whether or not the given property on the object can have its value
	//	changed.
	
	#pragma unused(inClientProcessID)
	
	//	declare the local variables
	OSStatus theAnswer = 0;
	
	//	check the arguments
	FailWithAction(inDriver != gAudioServerPlugInDriverRef, theAnswer = kAudioHardwareBadObjectError, Done, "SalsaHAL_IsStreamPropertySettable: bad driver reference");
	FailWithAction(inAddress == NULL, theAnswer = kAudioHardwareIllegalOperationError, Done, "SalsaHAL_IsStreamPropertySettable: no address");
	FailWithAction(outIsSettable == NULL, theAnswer = kAudioHardwareIllegalOperationError, Done, "SalsaHAL_IsStreamPropertySettable: no place to put the return value");
	FailWithAction((inObjectID != kObjectID_Stream_Input) && (inObjectID != kObjectID_Stream_Output), theAnswer = kAudioHardwareBadObjectError, Done, "SalsaHAL_IsStreamPropertySettable: not a stream object");
	
	//	Note that for each object, this driver implements all the required properties plus a few
	//	extras that are useful but not required. There is more detailed commentary about each
	//	property in the SalsaHAL_GetStreamPropertyData() method.
	switch(inAddress->mSelector)
	{
		case kAudioObjectPropertyBaseClass:
		case kAudioObjectPropertyClass:
		case kAudioObjectPropertyOwner:
		case kAudioObjectPropertyOwnedObjects:
		case kAudioObjectPropertyName:
		case kAudioStreamPropertyDirection:
		case kAudioStreamPropertyTerminalType:
		case kAudioStreamPropertyStartingChannel:
		case kAudioStreamPropertyLatency:
		case kAudioStreamPropertyAvailableVirtualFormats:
		case kAudioStreamPropertyAvailablePhysicalFormats:
			*outIsSettable = false;
			break;
		
		case kAudioStreamPropertyIsActive:
		case kAudioStreamPropertyVirtualFormat:
		case kAudioStreamPropertyPhysicalFormat:
			*outIsSettable = true;
			break;
		
		default:
			theAnswer = kAudioHardwareUnknownPropertyError;
			break;
	};

Done:
	return theAnswer;
}

static OSStatus	SalsaHAL_GetStreamPropertyDataSize(AudioServerPlugInDriverRef inDriver, AudioObjectID inObjectID, pid_t inClientProcessID, const AudioObjectPropertyAddress* inAddress, UInt32 inQualifierDataSize, const void* inQualifierData, UInt32* outDataSize)
{
	//	This method returns the byte size of the property's data.
	
	#pragma unused(inClientProcessID, inQualifierDataSize, inQualifierData)
	
	//	declare the local variables
	OSStatus theAnswer = 0;
	
	//	check the arguments
	FailWithAction(inDriver != gAudioServerPlugInDriverRef, theAnswer = kAudioHardwareBadObjectError, Done, "SalsaHAL_GetStreamPropertyDataSize: bad driver reference");
	FailWithAction(inAddress == NULL, theAnswer = kAudioHardwareIllegalOperationError, Done, "SalsaHAL_GetStreamPropertyDataSize: no address");
	FailWithAction(outDataSize == NULL, theAnswer = kAudioHardwareIllegalOperationError, Done, "SalsaHAL_GetStreamPropertyDataSize: no place to put the return value");
	FailWithAction((inObjectID != kObjectID_Stream_Input) && (inObjectID != kObjectID_Stream_Output), theAnswer = kAudioHardwareBadObjectError, Done, "SalsaHAL_GetStreamPropertyDataSize: not a stream object");
	
	//	Note that for each object, this driver implements all the required properties plus a few
	//	extras that are useful but not required. There is more detailed commentary about each
	//	property in the SalsaHAL_GetStreamPropertyData() method.
	switch(inAddress->mSelector)
	{
		case kAudioObjectPropertyBaseClass:
			*outDataSize = sizeof(AudioClassID);
			break;

		case kAudioObjectPropertyClass:
			*outDataSize = sizeof(AudioClassID);
			break;

		case kAudioObjectPropertyOwner:
			*outDataSize = sizeof(AudioObjectID);
			break;

		case kAudioObjectPropertyOwnedObjects:
			*outDataSize = 0 * sizeof(AudioObjectID);
			break;

		case kAudioObjectPropertyName:
			*outDataSize = sizeof(CFStringRef);
			break;

		case kAudioStreamPropertyIsActive:
			*outDataSize = sizeof(UInt32);
			break;

		case kAudioStreamPropertyDirection:
			*outDataSize = sizeof(UInt32);
			break;

		case kAudioStreamPropertyTerminalType:
			*outDataSize = sizeof(UInt32);
			break;

		case kAudioStreamPropertyStartingChannel:
			*outDataSize = sizeof(UInt32);
			break;
		
		case kAudioStreamPropertyLatency:
			*outDataSize = sizeof(UInt32);
			break;

		case kAudioStreamPropertyVirtualFormat:
		case kAudioStreamPropertyPhysicalFormat:
			*outDataSize = sizeof(AudioStreamBasicDescription);
			break;

		case kAudioStreamPropertyAvailableVirtualFormats:
		case kAudioStreamPropertyAvailablePhysicalFormats:
			*outDataSize = SalsaHAL_FormatCount(inObjectID) * sizeof(AudioStreamRangedDescription);
			break;

		default:
			theAnswer = kAudioHardwareUnknownPropertyError;
			break;
	};

Done:
	return theAnswer;
}

static OSStatus	SalsaHAL_GetStreamPropertyData(AudioServerPlugInDriverRef inDriver, AudioObjectID inObjectID, pid_t inClientProcessID, const AudioObjectPropertyAddress* inAddress, UInt32 inQualifierDataSize, const void* inQualifierData, UInt32 inDataSize, UInt32* outDataSize, void* outData)
{
	#pragma unused(inClientProcessID, inQualifierDataSize, inQualifierData)
	
	//	declare the local variables
	OSStatus theAnswer = 0;
	UInt32 theNumberItemsToFetch;
	
	//	check the arguments
	FailWithAction(inDriver != gAudioServerPlugInDriverRef, theAnswer = kAudioHardwareBadObjectError, Done, "SalsaHAL_GetStreamPropertyData: bad driver reference");
	FailWithAction(inAddress == NULL, theAnswer = kAudioHardwareIllegalOperationError, Done, "SalsaHAL_GetStreamPropertyData: no address");
	FailWithAction(outDataSize == NULL, theAnswer = kAudioHardwareIllegalOperationError, Done, "SalsaHAL_GetStreamPropertyData: no place to put the return value size");
	FailWithAction(outData == NULL, theAnswer = kAudioHardwareIllegalOperationError, Done, "SalsaHAL_GetStreamPropertyData: no place to put the return value");
	FailWithAction((inObjectID != kObjectID_Stream_Input) && (inObjectID != kObjectID_Stream_Output), theAnswer = kAudioHardwareBadObjectError, Done, "SalsaHAL_GetStreamPropertyData: not a stream object");
	
	//	Note that for each object, this driver implements all the required properties plus a few
	//	extras that are useful but not required.
	//
	//	Also, since most of the data that will get returned is static, there are few instances where
	//	it is necessary to lock the state mutex.
	switch(inAddress->mSelector)
	{
		case kAudioObjectPropertyBaseClass:
			//	The base class for kAudioStreamClassID is kAudioObjectClassID
			FailWithAction(inDataSize < sizeof(AudioClassID), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetStreamPropertyData: not enough space for the return value of kAudioObjectPropertyBaseClass for the stream");
			*((AudioClassID*)outData) = kAudioObjectClassID;
			*outDataSize = sizeof(AudioClassID);
			break;
			
		case kAudioObjectPropertyClass:
			//	The class is always kAudioStreamClassID for streams created by drivers
			FailWithAction(inDataSize < sizeof(AudioClassID), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetStreamPropertyData: not enough space for the return value of kAudioObjectPropertyClass for the stream");
			*((AudioClassID*)outData) = kAudioStreamClassID;
			*outDataSize = sizeof(AudioClassID);
			break;
			
		case kAudioObjectPropertyOwner:
			//	The stream's owner is the device object
			FailWithAction(inDataSize < sizeof(AudioObjectID), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetStreamPropertyData: not enough space for the return value of kAudioObjectPropertyOwner for the stream");
			*((AudioObjectID*)outData) = kObjectID_Device;
			*outDataSize = sizeof(AudioObjectID);
			break;
			
		case kAudioObjectPropertyOwnedObjects:
			//	Streams do not own any objects
			*outDataSize = 0 * sizeof(AudioObjectID);
			break;

		case kAudioObjectPropertyName:
			//	This is the human readable name of the stream
			FailWithAction(inDataSize < sizeof(CFStringRef), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetStreamPropertyData: not enough space for the return value of kAudioObjectPropertyName for the stream");
			*((CFStringRef*)outData) = (inObjectID == kObjectID_Stream_Input) ? CFSTR("InputStreamName") : CFSTR("OutputStreamName");
			*outDataSize = sizeof(CFStringRef);
			break;

		case kAudioStreamPropertyIsActive:
			//	This property tells the device whether or not the given stream is going to
			//	be used for IO. Note that we need to take the state lock to examine this
			//	value.
			FailWithAction(inDataSize < sizeof(UInt32), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetStreamPropertyData: not enough space for the return value of kAudioStreamPropertyIsActive for the stream");
			pthread_mutex_lock(&gPlugIn_StateMutex);
			*((UInt32*)outData) = (inObjectID == kObjectID_Stream_Input) ? gStream_Input_IsActive : gStream_Output_IsActive;
			pthread_mutex_unlock(&gPlugIn_StateMutex);
			*outDataSize = sizeof(UInt32);
			break;

		case kAudioStreamPropertyDirection:
			//	This returns whether the stream is an input stream or an output stream.
			FailWithAction(inDataSize < sizeof(UInt32), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetStreamPropertyData: not enough space for the return value of kAudioStreamPropertyDirection for the stream");
			*((UInt32*)outData) = (inObjectID == kObjectID_Stream_Input) ? 1 : 0;
			*outDataSize = sizeof(UInt32);
			break;

		case kAudioStreamPropertyTerminalType:
			//	This returns a value that indicates what is at the other end of the stream
			//	such as a speaker or headphones, or a microphone. Values for this property
			//	are defined in <CoreAudio/AudioHardwareBase.h>
			FailWithAction(inDataSize < sizeof(UInt32), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetStreamPropertyData: not enough space for the return value of kAudioStreamPropertyTerminalType for the stream");
			*((UInt32*)outData) = (inObjectID == kObjectID_Stream_Input) ? kAudioStreamTerminalTypeDigitalAudioInterface : kAudioStreamTerminalTypeSpeaker;
			*outDataSize = sizeof(UInt32);
			break;

		case kAudioStreamPropertyStartingChannel:
			//	This property returns the absolute channel number for the first channel in
			//	the stream. For exmaple, if a device has two output streams with two
			//	channels each, then the starting channel number for the first stream is 1
			//	and ths starting channel number fo the second stream is 3.
			FailWithAction(inDataSize < sizeof(UInt32), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetStreamPropertyData: not enough space for the return value of kAudioStreamPropertyStartingChannel for the stream");
			*((UInt32*)outData) = 1;
			*outDataSize = sizeof(UInt32);
			break;

		case kAudioStreamPropertyLatency:
			//	This property returns any additonal presentation latency the stream has.
			FailWithAction(inDataSize < sizeof(UInt32), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetStreamPropertyData: not enough space for the return value of kAudioStreamPropertyStartingChannel for the stream");
			*((UInt32*)outData) = 0;
			*outDataSize = sizeof(UInt32);
			break;

		case kAudioStreamPropertyVirtualFormat:
		case kAudioStreamPropertyPhysicalFormat:
			//	The carrier device does not mix or convert; the virtual format is
			//	the physical format. The capture stream is 16-bit integer PCM so
			//	the raw carrier words can be recovered without a conversion.
			FailWithAction(inDataSize < sizeof(AudioStreamBasicDescription), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetStreamPropertyData: not enough space for the return value of kAudioStreamPropertyVirtualFormat for the stream");
			pthread_mutex_lock(&gPlugIn_StateMutex);
			if(inObjectID == kObjectID_Stream_Input)
			{
				memcpy(outData, &gStream_Input_Format, sizeof(AudioStreamBasicDescription));
			}
			else
			{
				memcpy(outData, &gStream_Output_Format, sizeof(AudioStreamBasicDescription));
			}
			pthread_mutex_unlock(&gPlugIn_StateMutex);
			*outDataSize = sizeof(AudioStreamBasicDescription);
			break;

		case kAudioStreamPropertyAvailableVirtualFormats:
		case kAudioStreamPropertyAvailablePhysicalFormats:
			//	This returns an array of AudioStreamRangedDescriptions that describe what
			//	formats are supported.

			//	Calculate the number of items that have been requested. Note that this
			//	number is allowed to be smaller than the actual size of the list. In such
			//	case, only that number of items will be returned
			theNumberItemsToFetch = inDataSize / sizeof(AudioStreamRangedDescription);
			
			//	clamp it to the number of items we have
			if(theNumberItemsToFetch > SalsaHAL_FormatCount(inObjectID))
			{
				theNumberItemsToFetch = SalsaHAL_FormatCount(inObjectID);
			}
			
			//	fill out the return array: two-channel and eight-channel
			//	carriers for output; two-channel and eight-channel 16-bit
			//	PCM loopback for capture.
			if(theNumberItemsToFetch > 0)
			{
				((AudioStreamRangedDescription*)outData)[0].mFormat = (inObjectID == kObjectID_Stream_Input) ? SalsaHAL_MakeCaptureFormat(2) : SalsaHAL_MakeCarrierFormat(kSalsaHAL_DefaultChannels);
				((AudioStreamRangedDescription*)outData)[0].mSampleRateRange.mMinimum = kSalsaHAL_CarrierRate;
				((AudioStreamRangedDescription*)outData)[0].mSampleRateRange.mMaximum = kSalsaHAL_CarrierRate;
			}
			if(theNumberItemsToFetch > 1)
			{
				((AudioStreamRangedDescription*)outData)[1].mFormat = (inObjectID == kObjectID_Stream_Input) ? SalsaHAL_MakeCaptureFormat(8) : SalsaHAL_MakeCarrierFormat(8);
				((AudioStreamRangedDescription*)outData)[1].mSampleRateRange.mMinimum = kSalsaHAL_CarrierRate;
				((AudioStreamRangedDescription*)outData)[1].mSampleRateRange.mMaximum = kSalsaHAL_CarrierRate;
			}
			
			//	report how much we wrote
			*outDataSize = theNumberItemsToFetch * sizeof(AudioStreamRangedDescription);
			break;

		default:
			theAnswer = kAudioHardwareUnknownPropertyError;
			break;
	};

Done:
	return theAnswer;
}

static OSStatus	SalsaHAL_SetStreamPropertyData(AudioServerPlugInDriverRef inDriver, AudioObjectID inObjectID, pid_t inClientProcessID, const AudioObjectPropertyAddress* inAddress, UInt32 inQualifierDataSize, const void* inQualifierData, UInt32 inDataSize, const void* inData, UInt32* outNumberPropertiesChanged, AudioObjectPropertyAddress outChangedAddresses[2])
{
	#pragma unused(inClientProcessID, inQualifierDataSize, inQualifierData)
	
	//	declare the local variables
	OSStatus theAnswer = 0;
	
	//	check the arguments
	FailWithAction(inDriver != gAudioServerPlugInDriverRef, theAnswer = kAudioHardwareBadObjectError, Done, "SalsaHAL_SetStreamPropertyData: bad driver reference");
	FailWithAction(inAddress == NULL, theAnswer = kAudioHardwareIllegalOperationError, Done, "SalsaHAL_SetStreamPropertyData: no address");
	FailWithAction(outNumberPropertiesChanged == NULL, theAnswer = kAudioHardwareIllegalOperationError, Done, "SalsaHAL_SetStreamPropertyData: no place to return the number of properties that changed");
	FailWithAction(outChangedAddresses == NULL, theAnswer = kAudioHardwareIllegalOperationError, Done, "SalsaHAL_SetStreamPropertyData: no place to return the properties that changed");
	FailWithAction((inObjectID != kObjectID_Stream_Input) && (inObjectID != kObjectID_Stream_Output), theAnswer = kAudioHardwareBadObjectError, Done, "SalsaHAL_SetStreamPropertyData: not a stream object");
	
	//	initialize the returned number of changed properties
	*outNumberPropertiesChanged = 0;
	
	//	Note that for each object, this driver implements all the required properties plus a few
	//	extras that are useful but not required. There is more detailed commentary about each
	//	property in the SalsaHAL_GetStreamPropertyData() method.
	switch(inAddress->mSelector)
	{
		case kAudioStreamPropertyIsActive:
			//	Changing the active state of a stream doesn't affect IO or change the structure
			//	so we can just save the state and send the notification.
			FailWithAction(inDataSize != sizeof(UInt32), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_SetStreamPropertyData: wrong size for the data for kAudioDevicePropertyNominalSampleRate");
			pthread_mutex_lock(&gPlugIn_StateMutex);
			if(inObjectID == kObjectID_Stream_Input)
			{
				if(gStream_Input_IsActive != (*((const UInt32*)inData) != 0))
				{
					gStream_Input_IsActive = *((const UInt32*)inData) != 0;
					*outNumberPropertiesChanged = 1;
					outChangedAddresses[0].mSelector = kAudioStreamPropertyIsActive;
					outChangedAddresses[0].mScope = kAudioObjectPropertyScopeGlobal;
					outChangedAddresses[0].mElement = kAudioObjectPropertyElementMain;
				}
			}
			else
			{
				if(gStream_Output_IsActive != (*((const UInt32*)inData) != 0))
				{
					gStream_Output_IsActive = *((const UInt32*)inData) != 0;
					*outNumberPropertiesChanged = 1;
					outChangedAddresses[0].mSelector = kAudioStreamPropertyIsActive;
					outChangedAddresses[0].mScope = kAudioObjectPropertyScopeGlobal;
					outChangedAddresses[0].mElement = kAudioObjectPropertyElementMain;
				}
			}
			pthread_mutex_unlock(&gPlugIn_StateMutex);
			break;
			
		case kAudioStreamPropertyVirtualFormat:
		case kAudioStreamPropertyPhysicalFormat:
			// Format changes must wait for the host to stop IO and rebuild its graph.
			FailWithAction(inDataSize != sizeof(AudioStreamBasicDescription), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_SetStreamPropertyData: wrong size for the data for kAudioStreamPropertyPhysicalFormat");
			if(inObjectID == kObjectID_Stream_Input)
			{
				FailWithAction(!SalsaHAL_IsSupportedCaptureFormat((const AudioStreamBasicDescription*)inData), theAnswer = kAudioDeviceUnsupportedFormatError, Done, "SalsaHAL_SetStreamPropertyData: unsupported capture format");
			}
			else
			{
				FailWithAction(!SalsaHAL_IsSupportedOutputFormat((const AudioStreamBasicDescription*)inData), theAnswer = kAudioDeviceUnsupportedFormatError, Done, "SalsaHAL_SetStreamPropertyData: unsupported carrier format");
			}
			{
				UInt32 channels = ((const AudioStreamBasicDescription*)inData)->mChannelsPerFrame;
				pthread_mutex_lock(&gPlugIn_StateMutex);
				UInt32 current = inObjectID == kObjectID_Stream_Input ?
					gStream_Input_Format.mChannelsPerFrame : gStream_Output_Format.mChannelsPerFrame;
				pthread_mutex_unlock(&gPlugIn_StateMutex);
				if(channels != current)
				{
					UInt64 action = ((UInt64)inObjectID << 32) | channels;
					dispatch_async(dispatch_get_global_queue(DISPATCH_QUEUE_PRIORITY_DEFAULT, 0), ^{
						gPlugIn_Host->RequestDeviceConfigurationChange(gPlugIn_Host, kObjectID_Device, action, NULL);
					});
				}
			}
			break;
		
		default:
			theAnswer = kAudioHardwareUnknownPropertyError;
			break;
	};

Done:
	return theAnswer;
}

#pragma mark Control Property Operations

static Boolean	SalsaHAL_HasControlProperty(AudioServerPlugInDriverRef inDriver, AudioObjectID inObjectID, pid_t inClientProcessID, const AudioObjectPropertyAddress* inAddress)
{
	//	This method returns whether or not the given object has the given property.
	
	#pragma unused(inClientProcessID)
	
	//	declare the local variables
	Boolean theAnswer = false;
	
	//	check the arguments
	FailIf(inDriver != gAudioServerPlugInDriverRef, Done, "SalsaHAL_HasControlProperty: bad driver reference");
	FailIf(inAddress == NULL, Done, "SalsaHAL_HasControlProperty: no address");
	
	//	Note that for each object, this driver implements all the required properties plus a few
	//	extras that are useful but not required. There is more detailed commentary about each
	//	property in the SalsaHAL_GetControlPropertyData() method.
	switch(inObjectID)
	{
		case kObjectID_Volume_Input_Master:
		case kObjectID_Volume_Output_Master:
			switch(inAddress->mSelector)
			{
				case kAudioObjectPropertyBaseClass:
				case kAudioObjectPropertyClass:
				case kAudioObjectPropertyOwner:
				case kAudioObjectPropertyOwnedObjects:
				case kAudioControlPropertyScope:
				case kAudioControlPropertyElement:
				case kAudioLevelControlPropertyScalarValue:
				case kAudioLevelControlPropertyDecibelValue:
				case kAudioLevelControlPropertyDecibelRange:
				case kAudioLevelControlPropertyConvertScalarToDecibels:
				case kAudioLevelControlPropertyConvertDecibelsToScalar:
					theAnswer = true;
					break;
			};
			break;
		
		case kObjectID_Mute_Input_Master:
		case kObjectID_Mute_Output_Master:
			switch(inAddress->mSelector)
			{
				case kAudioObjectPropertyBaseClass:
				case kAudioObjectPropertyClass:
				case kAudioObjectPropertyOwner:
				case kAudioObjectPropertyOwnedObjects:
				case kAudioControlPropertyScope:
				case kAudioControlPropertyElement:
				case kAudioBooleanControlPropertyValue:
					theAnswer = true;
					break;
			};
			break;
		
		case kObjectID_DataSource_Input_Master:
		case kObjectID_DataSource_Output_Master:
		case kObjectID_DataDestination_PlayThru_Master:
			switch(inAddress->mSelector)
			{
				case kAudioObjectPropertyBaseClass:
				case kAudioObjectPropertyClass:
				case kAudioObjectPropertyOwner:
				case kAudioObjectPropertyOwnedObjects:
				case kAudioControlPropertyScope:
				case kAudioControlPropertyElement:
				case kAudioSelectorControlPropertyCurrentItem:
				case kAudioSelectorControlPropertyAvailableItems:
				case kAudioSelectorControlPropertyItemName:
					theAnswer = true;
					break;
			};
			break;
	};

Done:
	return theAnswer;
}

static OSStatus	SalsaHAL_IsControlPropertySettable(AudioServerPlugInDriverRef inDriver, AudioObjectID inObjectID, pid_t inClientProcessID, const AudioObjectPropertyAddress* inAddress, Boolean* outIsSettable)
{
	//	This method returns whether or not the given property on the object can have its value
	//	changed.
	
	#pragma unused(inClientProcessID)
	
	//	declare the local variables
	OSStatus theAnswer = 0;
	
	//	check the arguments
	FailWithAction(inDriver != gAudioServerPlugInDriverRef, theAnswer = kAudioHardwareBadObjectError, Done, "SalsaHAL_IsControlPropertySettable: bad driver reference");
	FailWithAction(inAddress == NULL, theAnswer = kAudioHardwareIllegalOperationError, Done, "SalsaHAL_IsControlPropertySettable: no address");
	FailWithAction(outIsSettable == NULL, theAnswer = kAudioHardwareIllegalOperationError, Done, "SalsaHAL_IsControlPropertySettable: no place to put the return value");
	
	//	Note that for each object, this driver implements all the required properties plus a few
	//	extras that are useful but not required. There is more detailed commentary about each
	//	property in the SalsaHAL_GetControlPropertyData() method.
	switch(inObjectID)
	{
		case kObjectID_Volume_Input_Master:
		case kObjectID_Volume_Output_Master:
			switch(inAddress->mSelector)
			{
				case kAudioObjectPropertyBaseClass:
				case kAudioObjectPropertyClass:
				case kAudioObjectPropertyOwner:
				case kAudioObjectPropertyOwnedObjects:
				case kAudioControlPropertyScope:
				case kAudioControlPropertyElement:
				case kAudioLevelControlPropertyDecibelRange:
				case kAudioLevelControlPropertyConvertScalarToDecibels:
				case kAudioLevelControlPropertyConvertDecibelsToScalar:
					*outIsSettable = false;
					break;
				
				case kAudioLevelControlPropertyScalarValue:
				case kAudioLevelControlPropertyDecibelValue:
					*outIsSettable = true;
					break;
				
				default:
					theAnswer = kAudioHardwareUnknownPropertyError;
					break;
			};
			break;
		
		case kObjectID_Mute_Input_Master:
		case kObjectID_Mute_Output_Master:
			switch(inAddress->mSelector)
			{
				case kAudioObjectPropertyBaseClass:
				case kAudioObjectPropertyClass:
				case kAudioObjectPropertyOwner:
				case kAudioObjectPropertyOwnedObjects:
				case kAudioControlPropertyScope:
				case kAudioControlPropertyElement:
					*outIsSettable = false;
					break;
				
				case kAudioBooleanControlPropertyValue:
					*outIsSettable = true;
					break;
				
				default:
					theAnswer = kAudioHardwareUnknownPropertyError;
					break;
			};
			break;
		
		case kObjectID_DataSource_Input_Master:
		case kObjectID_DataSource_Output_Master:
		case kObjectID_DataDestination_PlayThru_Master:
			switch(inAddress->mSelector)
			{
				case kAudioObjectPropertyBaseClass:
				case kAudioObjectPropertyClass:
				case kAudioObjectPropertyOwner:
				case kAudioObjectPropertyOwnedObjects:
				case kAudioControlPropertyScope:
				case kAudioControlPropertyElement:
				case kAudioSelectorControlPropertyAvailableItems:
				case kAudioSelectorControlPropertyItemName:
					*outIsSettable = false;
					break;
				
				case kAudioSelectorControlPropertyCurrentItem:
					*outIsSettable = true;
					break;
				
				default:
					theAnswer = kAudioHardwareUnknownPropertyError;
					break;
			};
			break;
				
		default:
			theAnswer = kAudioHardwareBadObjectError;
			break;
	};

Done:
	return theAnswer;
}

static OSStatus	SalsaHAL_GetControlPropertyDataSize(AudioServerPlugInDriverRef inDriver, AudioObjectID inObjectID, pid_t inClientProcessID, const AudioObjectPropertyAddress* inAddress, UInt32 inQualifierDataSize, const void* inQualifierData, UInt32* outDataSize)
{
	//	This method returns the byte size of the property's data.
	
	#pragma unused(inClientProcessID, inQualifierDataSize, inQualifierData)
	
	//	declare the local variables
	OSStatus theAnswer = 0;
	
	//	check the arguments
	FailWithAction(inDriver != gAudioServerPlugInDriverRef, theAnswer = kAudioHardwareBadObjectError, Done, "SalsaHAL_GetControlPropertyDataSize: bad driver reference");
	FailWithAction(inAddress == NULL, theAnswer = kAudioHardwareIllegalOperationError, Done, "SalsaHAL_GetControlPropertyDataSize: no address");
	FailWithAction(outDataSize == NULL, theAnswer = kAudioHardwareIllegalOperationError, Done, "SalsaHAL_GetControlPropertyDataSize: no place to put the return value");
	
	//	Note that for each object, this driver implements all the required properties plus a few
	//	extras that are useful but not required. There is more detailed commentary about each
	//	property in the SalsaHAL_GetControlPropertyData() method.
	switch(inObjectID)
	{
		case kObjectID_Volume_Input_Master:
		case kObjectID_Volume_Output_Master:
			switch(inAddress->mSelector)
			{
				case kAudioObjectPropertyBaseClass:
					*outDataSize = sizeof(AudioClassID);
					break;

				case kAudioObjectPropertyClass:
					*outDataSize = sizeof(AudioClassID);
					break;

				case kAudioObjectPropertyOwner:
					*outDataSize = sizeof(AudioObjectID);
					break;

				case kAudioObjectPropertyOwnedObjects:
					*outDataSize = 0 * sizeof(AudioObjectID);
					break;

				case kAudioControlPropertyScope:
					*outDataSize = sizeof(AudioObjectPropertyScope);
					break;

				case kAudioControlPropertyElement:
					*outDataSize = sizeof(AudioObjectPropertyElement);
					break;

				case kAudioLevelControlPropertyScalarValue:
					*outDataSize = sizeof(Float32);
					break;

				case kAudioLevelControlPropertyDecibelValue:
					*outDataSize = sizeof(Float32);
					break;

				case kAudioLevelControlPropertyDecibelRange:
					*outDataSize = sizeof(AudioValueRange);
					break;

				case kAudioLevelControlPropertyConvertScalarToDecibels:
					*outDataSize = sizeof(Float32);
					break;

				case kAudioLevelControlPropertyConvertDecibelsToScalar:
					*outDataSize = sizeof(Float32);
					break;

				default:
					theAnswer = kAudioHardwareUnknownPropertyError;
					break;
			};
			break;
		
		case kObjectID_Mute_Input_Master:
		case kObjectID_Mute_Output_Master:
			switch(inAddress->mSelector)
			{
				case kAudioObjectPropertyBaseClass:
					*outDataSize = sizeof(AudioClassID);
					break;

				case kAudioObjectPropertyClass:
					*outDataSize = sizeof(AudioClassID);
					break;

				case kAudioObjectPropertyOwner:
					*outDataSize = sizeof(AudioObjectID);
					break;

				case kAudioObjectPropertyOwnedObjects:
					*outDataSize = 0 * sizeof(AudioObjectID);
					break;

				case kAudioControlPropertyScope:
					*outDataSize = sizeof(AudioObjectPropertyScope);
					break;

				case kAudioControlPropertyElement:
					*outDataSize = sizeof(AudioObjectPropertyElement);
					break;

				case kAudioBooleanControlPropertyValue:
					*outDataSize = sizeof(UInt32);
					break;

				default:
					theAnswer = kAudioHardwareUnknownPropertyError;
					break;
			};
			break;
		
		case kObjectID_DataSource_Input_Master:
		case kObjectID_DataSource_Output_Master:
		case kObjectID_DataDestination_PlayThru_Master:
			switch(inAddress->mSelector)
			{
				case kAudioObjectPropertyBaseClass:
					*outDataSize = sizeof(AudioClassID);
					break;

				case kAudioObjectPropertyClass:
					*outDataSize = sizeof(AudioClassID);
					break;

				case kAudioObjectPropertyOwner:
					*outDataSize = sizeof(AudioObjectID);
					break;

				case kAudioObjectPropertyOwnedObjects:
					*outDataSize = 0 * sizeof(AudioObjectID);
					break;

				case kAudioControlPropertyScope:
					*outDataSize = sizeof(AudioObjectPropertyScope);
					break;

				case kAudioControlPropertyElement:
					*outDataSize = sizeof(AudioObjectPropertyElement);
					break;

				case kAudioSelectorControlPropertyCurrentItem:
					*outDataSize = sizeof(UInt32);
					break;

				case kAudioSelectorControlPropertyAvailableItems:
					*outDataSize = kDataSource_NumberItems * sizeof(UInt32);
					break;

				case kAudioSelectorControlPropertyItemName:
					*outDataSize = sizeof(CFStringRef);
					break;

				default:
					theAnswer = kAudioHardwareUnknownPropertyError;
					break;
			};
			break;
				
		default:
			theAnswer = kAudioHardwareBadObjectError;
			break;
	};

Done:
	return theAnswer;
}

static OSStatus	SalsaHAL_GetControlPropertyData(AudioServerPlugInDriverRef inDriver, AudioObjectID inObjectID, pid_t inClientProcessID, const AudioObjectPropertyAddress* inAddress, UInt32 inQualifierDataSize, const void* inQualifierData, UInt32 inDataSize, UInt32* outDataSize, void* outData)
{
	#pragma unused(inClientProcessID)
	
	//	declare the local variables
	OSStatus theAnswer = 0;
	UInt32 theNumberItemsToFetch;
	UInt32 theItemIndex;
	
	//	check the arguments
	FailWithAction(inDriver != gAudioServerPlugInDriverRef, theAnswer = kAudioHardwareBadObjectError, Done, "SalsaHAL_GetControlPropertyData: bad driver reference");
	FailWithAction(inAddress == NULL, theAnswer = kAudioHardwareIllegalOperationError, Done, "SalsaHAL_GetControlPropertyData: no address");
	FailWithAction(outDataSize == NULL, theAnswer = kAudioHardwareIllegalOperationError, Done, "SalsaHAL_GetControlPropertyData: no place to put the return value size");
	FailWithAction(outData == NULL, theAnswer = kAudioHardwareIllegalOperationError, Done, "SalsaHAL_GetControlPropertyData: no place to put the return value");
	
	//	Note that for each object, this driver implements all the required properties plus a few
	//	extras that are useful but not required.
	//
	//	Also, since most of the data that will get returned is static, there are few instances where
	//	it is necessary to lock the state mutex.
	switch(inObjectID)
	{
		case kObjectID_Volume_Input_Master:
		case kObjectID_Volume_Output_Master:
			switch(inAddress->mSelector)
			{
				case kAudioObjectPropertyBaseClass:
					//	The base class for kAudioVolumeControlClassID is kAudioLevelControlClassID
					FailWithAction(inDataSize < sizeof(AudioClassID), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetControlPropertyData: not enough space for the return value of kAudioObjectPropertyBaseClass for the volume control");
					*((AudioClassID*)outData) = kAudioLevelControlClassID;
					*outDataSize = sizeof(AudioClassID);
					break;
					
				case kAudioObjectPropertyClass:
					//	Volume controls are of the class, kAudioVolumeControlClassID
					FailWithAction(inDataSize < sizeof(AudioClassID), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetControlPropertyData: not enough space for the return value of kAudioObjectPropertyClass for the volume control");
					*((AudioClassID*)outData) = kAudioVolumeControlClassID;
					*outDataSize = sizeof(AudioClassID);
					break;
					
				case kAudioObjectPropertyOwner:
					//	The control's owner is the device object
					FailWithAction(inDataSize < sizeof(AudioObjectID), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetControlPropertyData: not enough space for the return value of kAudioObjectPropertyOwner for the volume control");
					*((AudioObjectID*)outData) = kObjectID_Device;
					*outDataSize = sizeof(AudioObjectID);
					break;
					
				case kAudioObjectPropertyOwnedObjects:
					//	Controls do not own any objects
					*outDataSize = 0 * sizeof(AudioObjectID);
					break;

				case kAudioControlPropertyScope:
					//	This property returns the scope that the control is attached to.
					FailWithAction(inDataSize < sizeof(AudioObjectPropertyScope), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetControlPropertyData: not enough space for the return value of kAudioControlPropertyScope for the volume control");
					*((AudioObjectPropertyScope*)outData) = (inObjectID == kObjectID_Volume_Input_Master) ? kAudioObjectPropertyScopeInput : kAudioObjectPropertyScopeOutput;
					*outDataSize = sizeof(AudioObjectPropertyScope);
					break;

				case kAudioControlPropertyElement:
					//	This property returns the element that the control is attached to.
					FailWithAction(inDataSize < sizeof(AudioObjectPropertyElement), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetControlPropertyData: not enough space for the return value of kAudioControlPropertyElement for the volume control");
					*((AudioObjectPropertyElement*)outData) = kAudioObjectPropertyElementMain;
					*outDataSize = sizeof(AudioObjectPropertyElement);
					break;

				case kAudioLevelControlPropertyScalarValue:
					//	This returns the value of the control in the normalized range of 0 to 1.
					//	Note that we need to take the state lock to examine the value.
					FailWithAction(inDataSize < sizeof(Float32), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetControlPropertyData: not enough space for the return value of kAudioLevelControlPropertyScalarValue for the volume control");
					pthread_mutex_lock(&gPlugIn_StateMutex);
					*((Float32*)outData) = (inObjectID == kObjectID_Volume_Input_Master) ? gVolume_Input_Master_Value : gVolume_Output_Master_Value;
					pthread_mutex_unlock(&gPlugIn_StateMutex);
					*outDataSize = sizeof(Float32);
					break;

				case kAudioLevelControlPropertyDecibelValue:
					//	This returns the dB value of the control.
					//	Note that we need to take the state lock to examine the value.
					FailWithAction(inDataSize < sizeof(Float32), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetControlPropertyData: not enough space for the return value of kAudioLevelControlPropertyDecibelValue for the volume control");
					pthread_mutex_lock(&gPlugIn_StateMutex);
					*((Float32*)outData) = (inObjectID == kObjectID_Volume_Input_Master) ? gVolume_Input_Master_Value : gVolume_Output_Master_Value;
					pthread_mutex_unlock(&gPlugIn_StateMutex);
					
					//	Note that we square the scalar value before converting to dB so as to
					//	provide a better curve for the slider
					*((Float32*)outData) *= *((Float32*)outData);
					*((Float32*)outData) = kVolume_MinDB + (*((Float32*)outData) * (kVolume_MaxDB - kVolume_MinDB));
					
					//	report how much we wrote
					*outDataSize = sizeof(Float32);
					break;

				case kAudioLevelControlPropertyDecibelRange:
					//	This returns the dB range of the control.
					FailWithAction(inDataSize < sizeof(AudioValueRange), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetControlPropertyData: not enough space for the return value of kAudioLevelControlPropertyDecibelRange for the volume control");
					((AudioValueRange*)outData)->mMinimum = kVolume_MinDB;
					((AudioValueRange*)outData)->mMaximum = kVolume_MaxDB;
					*outDataSize = sizeof(AudioValueRange);
					break;

				case kAudioLevelControlPropertyConvertScalarToDecibels:
					//	This takes the scalar value in outData and converts it to dB.
					FailWithAction(inDataSize < sizeof(Float32), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetControlPropertyData: not enough space for the return value of kAudioLevelControlPropertyDecibelValue for the volume control");
					
					//	clamp the value to be between 0 and 1
					if(*((Float32*)outData) < 0.0)
					{
						*((Float32*)outData) = 0;
					}
					if(*((Float32*)outData) > 1.0)
					{
						*((Float32*)outData) = 1.0;
					}
					
					//	Note that we square the scalar value before converting to dB so as to
					//	provide a better curve for the slider
					*((Float32*)outData) *= *((Float32*)outData);
					*((Float32*)outData) = kVolume_MinDB + (*((Float32*)outData) * (kVolume_MaxDB - kVolume_MinDB));
					
					//	report how much we wrote
					*outDataSize = sizeof(Float32);
					break;

				case kAudioLevelControlPropertyConvertDecibelsToScalar:
					//	This takes the dB value in outData and converts it to scalar.
					FailWithAction(inDataSize < sizeof(Float32), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetControlPropertyData: not enough space for the return value of kAudioLevelControlPropertyDecibelValue for the volume control");
					
					//	clamp the value to be between kVolume_MinDB and kVolume_MaxDB
					if(*((Float32*)outData) < kVolume_MinDB)
					{
						*((Float32*)outData) = kVolume_MinDB;
					}
					if(*((Float32*)outData) > kVolume_MaxDB)
					{
						*((Float32*)outData) = kVolume_MaxDB;
					}
					
					//	Note that we square the scalar value before converting to dB so as to
					//	provide a better curve for the slider. We undo that here.
					*((Float32*)outData) = *((Float32*)outData) - kVolume_MinDB;
					*((Float32*)outData) /= kVolume_MaxDB - kVolume_MinDB;
					*((Float32*)outData) = sqrtf(*((Float32*)outData));
					
					//	report how much we wrote
					*outDataSize = sizeof(Float32);
					break;

				default:
					theAnswer = kAudioHardwareUnknownPropertyError;
					break;
			};
			break;
		
		case kObjectID_Mute_Input_Master:
		case kObjectID_Mute_Output_Master:
			switch(inAddress->mSelector)
			{
				case kAudioObjectPropertyBaseClass:
					//	The base class for kAudioMuteControlClassID is kAudioBooleanControlClassID
					FailWithAction(inDataSize < sizeof(AudioClassID), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetControlPropertyData: not enough space for the return value of kAudioObjectPropertyBaseClass for the mute control");
					*((AudioClassID*)outData) = kAudioBooleanControlClassID;
					*outDataSize = sizeof(AudioClassID);
					break;
					
				case kAudioObjectPropertyClass:
					//	Mute controls are of the class, kAudioMuteControlClassID
					FailWithAction(inDataSize < sizeof(AudioClassID), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetControlPropertyData: not enough space for the return value of kAudioObjectPropertyClass for the mute control");
					*((AudioClassID*)outData) = kAudioMuteControlClassID;
					*outDataSize = sizeof(AudioClassID);
					break;
					
				case kAudioObjectPropertyOwner:
					//	The control's owner is the device object
					FailWithAction(inDataSize < sizeof(AudioObjectID), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetControlPropertyData: not enough space for the return value of kAudioObjectPropertyOwner for the mute control");
					*((AudioObjectID*)outData) = kObjectID_Device;
					*outDataSize = sizeof(AudioObjectID);
					break;
					
				case kAudioObjectPropertyOwnedObjects:
					//	Controls do not own any objects
					*outDataSize = 0 * sizeof(AudioObjectID);
					break;

				case kAudioControlPropertyScope:
					//	This property returns the scope that the control is attached to.
					FailWithAction(inDataSize < sizeof(AudioObjectPropertyScope), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetControlPropertyData: not enough space for the return value of kAudioControlPropertyScope for the mute control");
					*((AudioObjectPropertyScope*)outData) = (inObjectID == kObjectID_Mute_Input_Master) ? kAudioObjectPropertyScopeInput : kAudioObjectPropertyScopeOutput;
					*outDataSize = sizeof(AudioObjectPropertyScope);
					break;

				case kAudioControlPropertyElement:
					//	This property returns the element that the control is attached to.
					FailWithAction(inDataSize < sizeof(AudioObjectPropertyElement), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetControlPropertyData: not enough space for the return value of kAudioControlPropertyElement for the mute control");
					*((AudioObjectPropertyElement*)outData) = kAudioObjectPropertyElementMain;
					*outDataSize = sizeof(AudioObjectPropertyElement);
					break;

				case kAudioBooleanControlPropertyValue:
					//	This returns the value of the mute control where 0 means that mute is off
					//	and audio can be heard and 1 means that mute is on and audio cannot be heard.
					//	Note that we need to take the state lock to examine this value.
					FailWithAction(inDataSize < sizeof(UInt32), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetControlPropertyData: not enough space for the return value of kAudioBooleanControlPropertyValue for the mute control");
					pthread_mutex_lock(&gPlugIn_StateMutex);
					*((UInt32*)outData) = (inObjectID == kObjectID_Mute_Input_Master) ? (gMute_Input_Master_Value ? 1 : 0) : (gMute_Output_Master_Value ? 1 : 0);
					pthread_mutex_unlock(&gPlugIn_StateMutex);
					*outDataSize = sizeof(UInt32);
					break;

				default:
					theAnswer = kAudioHardwareUnknownPropertyError;
					break;
			};
			break;
		
		case kObjectID_DataSource_Input_Master:
		case kObjectID_DataSource_Output_Master:
		case kObjectID_DataDestination_PlayThru_Master:
			switch(inAddress->mSelector)
			{
				case kAudioObjectPropertyBaseClass:
					//	The base class for kAudioDataSourceControlClassID is kAudioSelectorControlClassID
					FailWithAction(inDataSize < sizeof(AudioClassID), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetControlPropertyData: not enough space for the return value of kAudioObjectPropertyBaseClass for the data source control");
					*((AudioClassID*)outData) = kAudioSelectorControlClassID;
					*outDataSize = sizeof(AudioClassID);
					break;
					
				case kAudioObjectPropertyClass:
					//	Data Source controls are of the class, kAudioDataSourceControlClassID
					FailWithAction(inDataSize < sizeof(AudioClassID), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetControlPropertyData: not enough space for the return value of kAudioObjectPropertyClass for the data source control");
					*((AudioClassID*)outData) = kAudioDataSourceControlClassID;
					switch(inObjectID)
					{
						case kObjectID_DataSource_Input_Master:
						case kObjectID_DataSource_Output_Master:
							*((AudioClassID*)outData) = kAudioDataSourceControlClassID;
							break;
							
						case kObjectID_DataDestination_PlayThru_Master:
							*((AudioClassID*)outData) = kAudioDataDestinationControlClassID;
							break;
							
					};
					*outDataSize = sizeof(AudioClassID);
					break;
					
				case kAudioObjectPropertyOwner:
					//	The control's owner is the device object
					FailWithAction(inDataSize < sizeof(AudioObjectID), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetControlPropertyData: not enough space for the return value of kAudioObjectPropertyOwner for the data source control");
					*((AudioObjectID*)outData) = kObjectID_Device;
					*outDataSize = sizeof(AudioObjectID);
					break;
					
				case kAudioObjectPropertyOwnedObjects:
					//	Controls do not own any objects
					*outDataSize = 0 * sizeof(AudioObjectID);
					break;

				case kAudioControlPropertyScope:
					//	This property returns the scope that the control is attached to.
					FailWithAction(inDataSize < sizeof(AudioObjectPropertyScope), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetControlPropertyData: not enough space for the return value of kAudioControlPropertyScope for the data source control");
					switch(inObjectID)
					{
						case kObjectID_DataSource_Input_Master:
							*((AudioObjectPropertyScope*)outData) = kAudioObjectPropertyScopeInput;
							break;
							
						case kObjectID_DataSource_Output_Master:
							*((AudioObjectPropertyScope*)outData) = kAudioObjectPropertyScopeOutput;
							break;
							
						case kObjectID_DataDestination_PlayThru_Master:
							*((AudioObjectPropertyScope*)outData) = kAudioObjectPropertyScopePlayThrough;
							break;
							
					};
					*outDataSize = sizeof(AudioObjectPropertyScope);
					break;

				case kAudioControlPropertyElement:
					//	This property returns the element that the control is attached to.
					FailWithAction(inDataSize < sizeof(AudioObjectPropertyElement), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetControlPropertyData: not enough space for the return value of kAudioControlPropertyElement for the data source control");
					*((AudioObjectPropertyElement*)outData) = kAudioObjectPropertyElementMain;
					*outDataSize = sizeof(AudioObjectPropertyElement);
					break;

				case kAudioSelectorControlPropertyCurrentItem:
					//	This returns the value of the data source selector.
					//	Note that we need to take the state lock to examine this value.
					FailWithAction(inDataSize < sizeof(UInt32), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetControlPropertyData: not enough space for the return value of kAudioSelectorControlPropertyCurrentItem for the data source control");
					pthread_mutex_lock(&gPlugIn_StateMutex);
					switch(inObjectID)
					{
						case kObjectID_DataSource_Input_Master:
							*((UInt32*)outData) = gDataSource_Input_Master_Value;
							break;
							
						case kObjectID_DataSource_Output_Master:
							*((UInt32*)outData) = gDataSource_Output_Master_Value;
							break;
							
						case kObjectID_DataDestination_PlayThru_Master:
							*((UInt32*)outData) = gDataDestination_PlayThru_Master_Value;
							break;
							
					};
					pthread_mutex_unlock(&gPlugIn_StateMutex);
					*outDataSize = sizeof(UInt32);
					break;

				case kAudioSelectorControlPropertyAvailableItems:
					//	This returns the IDs for all the items the data source control supports.
					
					//	Calculate the number of items that have been requested. Note that this
					//	number is allowed to be smaller than the actual size of the list. In such
					//	case, only that number of items will be returned
					theNumberItemsToFetch = inDataSize / sizeof(UInt32);
					
					//	clamp it to the number of items we have
					if(theNumberItemsToFetch > kDataSource_NumberItems)
					{
						theNumberItemsToFetch = kDataSource_NumberItems;
					}
					
					//	fill out the return array
					for(theItemIndex = 0; theItemIndex < theNumberItemsToFetch; ++theItemIndex)
					{
						((UInt32*)outData)[theItemIndex] = theItemIndex;
					}
					
					//	report how much we wrote
					*outDataSize = theNumberItemsToFetch * sizeof(UInt32);
					break;

				case kAudioSelectorControlPropertyItemName:
					//	This returns the user-readable name for the selector item in the qualifier
					FailWithAction(inDataSize < sizeof(CFStringRef), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetControlPropertyData: not enough space for the return value of kAudioSelectorControlPropertyItemName for the data source control");
					FailWithAction(inQualifierDataSize != sizeof(UInt32), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_GetControlPropertyData: wrong size for the qualifier of kAudioSelectorControlPropertyItemName for the data source control");
					FailWithAction(*((const UInt32*)inQualifierData) >= kDataSource_NumberItems, theAnswer = kAudioHardwareIllegalOperationError, Done, "SalsaHAL_GetControlPropertyData: the item in the qualifier is not valid for kAudioSelectorControlPropertyItemName for the data source control");
					*((CFStringRef*)outData) = CFStringCreateWithFormat(NULL, NULL, CFSTR(kDataSource_ItemNamePattern), *((const UInt32*)inQualifierData));
					*outDataSize = sizeof(CFStringRef);
					break;

				default:
					theAnswer = kAudioHardwareUnknownPropertyError;
					break;
			};
			break;
				
		default:
			theAnswer = kAudioHardwareBadObjectError;
			break;
	};

Done:
	return theAnswer;
}

static OSStatus	SalsaHAL_SetControlPropertyData(AudioServerPlugInDriverRef inDriver, AudioObjectID inObjectID, pid_t inClientProcessID, const AudioObjectPropertyAddress* inAddress, UInt32 inQualifierDataSize, const void* inQualifierData, UInt32 inDataSize, const void* inData, UInt32* outNumberPropertiesChanged, AudioObjectPropertyAddress outChangedAddresses[2])
{
	#pragma unused(inClientProcessID, inQualifierDataSize, inQualifierData)
	
	//	declare the local variables
	OSStatus theAnswer = 0;
	Float32 theNewVolume;
	
	//	check the arguments
	FailWithAction(inDriver != gAudioServerPlugInDriverRef, theAnswer = kAudioHardwareBadObjectError, Done, "SalsaHAL_SetControlPropertyData: bad driver reference");
	FailWithAction(inAddress == NULL, theAnswer = kAudioHardwareIllegalOperationError, Done, "SalsaHAL_SetControlPropertyData: no address");
	FailWithAction(outNumberPropertiesChanged == NULL, theAnswer = kAudioHardwareIllegalOperationError, Done, "SalsaHAL_SetControlPropertyData: no place to return the number of properties that changed");
	FailWithAction(outChangedAddresses == NULL, theAnswer = kAudioHardwareIllegalOperationError, Done, "SalsaHAL_SetControlPropertyData: no place to return the properties that changed");
	
	//	initialize the returned number of changed properties
	*outNumberPropertiesChanged = 0;
	
	//	Note that for each object, this driver implements all the required properties plus a few
	//	extras that are useful but not required. There is more detailed commentary about each
	//	property in the SalsaHAL_GetControlPropertyData() method.
	switch(inObjectID)
	{
		case kObjectID_Volume_Input_Master:
		case kObjectID_Volume_Output_Master:
			switch(inAddress->mSelector)
			{
				case kAudioLevelControlPropertyScalarValue:
					//	For the scalar volume, we clamp the new value to [0, 1]. Note that if this
					//	value changes, it implies that the dB value changed too.
					FailWithAction(inDataSize != sizeof(Float32), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_SetControlPropertyData: wrong size for the data for kAudioLevelControlPropertyScalarValue");
					theNewVolume = *((const Float32*)inData);
					if(theNewVolume < 0.0)
					{
						theNewVolume = 0.0;
					}
					else if(theNewVolume > 1.0)
					{
						theNewVolume = 1.0;
					}
					pthread_mutex_lock(&gPlugIn_StateMutex);
					if(inObjectID == kObjectID_Volume_Input_Master)
					{
						if(gVolume_Input_Master_Value != theNewVolume)
						{
							gVolume_Input_Master_Value = theNewVolume;
							*outNumberPropertiesChanged = 2;
							outChangedAddresses[0].mSelector = kAudioLevelControlPropertyScalarValue;
							outChangedAddresses[0].mScope = kAudioObjectPropertyScopeGlobal;
							outChangedAddresses[0].mElement = kAudioObjectPropertyElementMain;
							outChangedAddresses[1].mSelector = kAudioLevelControlPropertyDecibelValue;
							outChangedAddresses[1].mScope = kAudioObjectPropertyScopeGlobal;
							outChangedAddresses[1].mElement = kAudioObjectPropertyElementMain;
						}
					}
					else
					{
						if(gVolume_Output_Master_Value != theNewVolume)
						{
							gVolume_Output_Master_Value = theNewVolume;
							*outNumberPropertiesChanged = 2;
							outChangedAddresses[0].mSelector = kAudioLevelControlPropertyScalarValue;
							outChangedAddresses[0].mScope = kAudioObjectPropertyScopeGlobal;
							outChangedAddresses[0].mElement = kAudioObjectPropertyElementMain;
							outChangedAddresses[1].mSelector = kAudioLevelControlPropertyDecibelValue;
							outChangedAddresses[1].mScope = kAudioObjectPropertyScopeGlobal;
							outChangedAddresses[1].mElement = kAudioObjectPropertyElementMain;
						}
					}
					pthread_mutex_unlock(&gPlugIn_StateMutex);
					break;
				
				case kAudioLevelControlPropertyDecibelValue:
					//	For the dB value, we first convert it to a scalar value since that is how
					//	the value is tracked. Note that if this value changes, it implies that the
					//	scalar value changes as well.
					FailWithAction(inDataSize != sizeof(Float32), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_SetControlPropertyData: wrong size for the data for kAudioLevelControlPropertyScalarValue");
					theNewVolume = *((const Float32*)inData);
					if(theNewVolume < kVolume_MinDB)
					{
						theNewVolume = kVolume_MinDB;
					}
					else if(theNewVolume > kVolume_MaxDB)
					{
						theNewVolume = kVolume_MaxDB;
					}
					//	Note that we square the scalar value before converting to dB so as to
					//	provide a better curve for the slider. We undo that here.
					theNewVolume = theNewVolume - kVolume_MinDB;
					theNewVolume /= kVolume_MaxDB - kVolume_MinDB;
					theNewVolume = sqrtf(theNewVolume);
					pthread_mutex_lock(&gPlugIn_StateMutex);
					if(inObjectID == kObjectID_Volume_Input_Master)
					{
						if(gVolume_Input_Master_Value != theNewVolume)
						{
							gVolume_Input_Master_Value = theNewVolume;
							*outNumberPropertiesChanged = 2;
							outChangedAddresses[0].mSelector = kAudioLevelControlPropertyScalarValue;
							outChangedAddresses[0].mScope = kAudioObjectPropertyScopeGlobal;
							outChangedAddresses[0].mElement = kAudioObjectPropertyElementMain;
							outChangedAddresses[1].mSelector = kAudioLevelControlPropertyDecibelValue;
							outChangedAddresses[1].mScope = kAudioObjectPropertyScopeGlobal;
							outChangedAddresses[1].mElement = kAudioObjectPropertyElementMain;
						}
					}
					else
					{
						if(gVolume_Output_Master_Value != theNewVolume)
						{
							gVolume_Output_Master_Value = theNewVolume;
							*outNumberPropertiesChanged = 2;
							outChangedAddresses[0].mSelector = kAudioLevelControlPropertyScalarValue;
							outChangedAddresses[0].mScope = kAudioObjectPropertyScopeGlobal;
							outChangedAddresses[0].mElement = kAudioObjectPropertyElementMain;
							outChangedAddresses[1].mSelector = kAudioLevelControlPropertyDecibelValue;
							outChangedAddresses[1].mScope = kAudioObjectPropertyScopeGlobal;
							outChangedAddresses[1].mElement = kAudioObjectPropertyElementMain;
						}
					}
					pthread_mutex_unlock(&gPlugIn_StateMutex);
					break;
				
				default:
					theAnswer = kAudioHardwareUnknownPropertyError;
					break;
			};
			break;
		
		case kObjectID_Mute_Input_Master:
		case kObjectID_Mute_Output_Master:
			switch(inAddress->mSelector)
			{
				case kAudioBooleanControlPropertyValue:
					FailWithAction(inDataSize != sizeof(UInt32), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_SetControlPropertyData: wrong size for the data for kAudioBooleanControlPropertyValue");
					pthread_mutex_lock(&gPlugIn_StateMutex);
					if(inObjectID == kObjectID_Mute_Input_Master)
					{
						if(gMute_Input_Master_Value != (*((const UInt32*)inData) != 0))
						{
							gMute_Input_Master_Value = *((const UInt32*)inData) != 0;
							*outNumberPropertiesChanged = 1;
							outChangedAddresses[0].mSelector = kAudioBooleanControlPropertyValue;
							outChangedAddresses[0].mScope = kAudioObjectPropertyScopeGlobal;
							outChangedAddresses[0].mElement = kAudioObjectPropertyElementMain;
						}
					}
					else
					{
						if(gMute_Output_Master_Value != (*((const UInt32*)inData) != 0))
						{
							gMute_Output_Master_Value = *((const UInt32*)inData) != 0;
							*outNumberPropertiesChanged = 1;
							outChangedAddresses[0].mSelector = kAudioBooleanControlPropertyValue;
							outChangedAddresses[0].mScope = kAudioObjectPropertyScopeGlobal;
							outChangedAddresses[0].mElement = kAudioObjectPropertyElementMain;
						}
					}
					pthread_mutex_unlock(&gPlugIn_StateMutex);
					break;
				
				default:
					theAnswer = kAudioHardwareUnknownPropertyError;
					break;
			};
			break;
		
		case kObjectID_DataSource_Input_Master:
		case kObjectID_DataSource_Output_Master:
		case kObjectID_DataDestination_PlayThru_Master:
			switch(inAddress->mSelector)
			{
				case kAudioSelectorControlPropertyCurrentItem:
					//	For selector controls, we check to make sure the requested value is in the
					//	available items list and just store the value.
					FailWithAction(inDataSize != sizeof(UInt32), theAnswer = kAudioHardwareBadPropertySizeError, Done, "SalsaHAL_SetControlPropertyData: wrong size for the data for kAudioSelectorControlPropertyCurrentItem");
					FailWithAction(*((const UInt32*)inData) >= kDataSource_NumberItems, theAnswer = kAudioHardwareIllegalOperationError, Done, "SalsaHAL_SetControlPropertyData: requested item not in available items list for kAudioSelectorControlPropertyCurrentItem");
					pthread_mutex_lock(&gPlugIn_StateMutex);
					switch(inObjectID)
					{
						case kObjectID_DataSource_Input_Master:
							{
								if(gDataSource_Input_Master_Value != *((const UInt32*)inData))
								{
									gDataSource_Input_Master_Value = *((const UInt32*)inData);
									*outNumberPropertiesChanged = 1;
									outChangedAddresses[0].mSelector = kAudioSelectorControlPropertyCurrentItem;
									outChangedAddresses[0].mScope = kAudioObjectPropertyScopeGlobal;
									outChangedAddresses[0].mElement = kAudioObjectPropertyElementMain;
								}
							}
							break;
							
						case kObjectID_DataSource_Output_Master:
							{
								if(gDataSource_Output_Master_Value != *((const UInt32*)inData))
								{
									gDataSource_Output_Master_Value = *((const UInt32*)inData);
									*outNumberPropertiesChanged = 1;
									outChangedAddresses[0].mSelector = kAudioSelectorControlPropertyCurrentItem;
									outChangedAddresses[0].mScope = kAudioObjectPropertyScopeGlobal;
									outChangedAddresses[0].mElement = kAudioObjectPropertyElementMain;
								}
							}
							break;
							
						case kObjectID_DataDestination_PlayThru_Master:
							{
								if(gDataDestination_PlayThru_Master_Value != *((const UInt32*)inData))
								{
									gDataDestination_PlayThru_Master_Value = *((const UInt32*)inData);
									*outNumberPropertiesChanged = 1;
									outChangedAddresses[0].mSelector = kAudioSelectorControlPropertyCurrentItem;
									outChangedAddresses[0].mScope = kAudioObjectPropertyScopeGlobal;
									outChangedAddresses[0].mElement = kAudioObjectPropertyElementMain;
								}
							}
							break;
					};
					pthread_mutex_unlock(&gPlugIn_StateMutex);
					break;
				
				default:
					theAnswer = kAudioHardwareUnknownPropertyError;
					break;
			};
			break;
				
		default:
			theAnswer = kAudioHardwareBadObjectError;
			break;
	};

Done:
	return theAnswer;
}

#pragma mark IO Operations

static OSStatus	SalsaHAL_StartIO(AudioServerPlugInDriverRef inDriver, AudioObjectID inDeviceObjectID, UInt32 inClientID)
{
	//	This call tells the device that IO is starting for the given client. When this routine
	//	returns, the device's clock is running and it is ready to have data read/written. It is
	//	important to note that multiple clients can have IO running on the device at the same time.
	//	So, work only needs to be done when the first client starts. All subsequent starts simply
	//	increment the counter.
	
	#pragma unused(inClientID)
	
	//	declare the local variables
	OSStatus theAnswer = 0;
	
	//	check the arguments
	FailWithAction(inDriver != gAudioServerPlugInDriverRef, theAnswer = kAudioHardwareBadObjectError, Done, "SalsaHAL_StartIO: bad driver reference");
	FailWithAction(inDeviceObjectID != kObjectID_Device, theAnswer = kAudioHardwareBadObjectError, Done, "SalsaHAL_StartIO: bad device ID");
	atomic_fetch_add(&gProbeStarts, 1);

	//	we need to hold the state lock
	pthread_mutex_lock(&gPlugIn_StateMutex);
	
	//	figure out what we need to do
	if(gDevice_IOIsRunning == UINT64_MAX)
	{
		//	overflowing is an error
		theAnswer = kAudioHardwareIllegalOperationError;
	}
	else if(gDevice_IOIsRunning == 0)
	{
		//	We need to start the hardware, which in this case is just anchoring the time line.
		gDevice_IOIsRunning = 1;
		pthread_mutex_lock(&gDevice_IOMutex);
		gDevice_NumberTimeStamps = 0;
		gDevice_AnchorHostTime = mach_absolute_time();
		pthread_mutex_unlock(&gDevice_IOMutex);

		//	New output session: discard carrier data from the previous one.
		atomic_store(&gCarrierWriteFrame, 0);
		atomic_store(&gCarrierReadFrame, 0);
		atomic_store(&gCarrierDroppedFrames, 0);
		atomic_fetch_add(&gSessionGeneration, 1);
		SalsaHAL_TransportReset();
	}
	else
	{
		//	IO is already running, so just bump the counter
		++gDevice_IOIsRunning;
	}
	
	//	unlock the state lock
	pthread_mutex_unlock(&gPlugIn_StateMutex);
	
Done:
	return theAnswer;
}

static OSStatus	SalsaHAL_StopIO(AudioServerPlugInDriverRef inDriver, AudioObjectID inDeviceObjectID, UInt32 inClientID)
{
	//	This call tells the device that the client has stopped IO. The driver can stop the hardware
	//	once all clients have stopped.
	
	#pragma unused(inClientID)
	
	//	declare the local variables
	OSStatus theAnswer = 0;
	
	//	check the arguments
	FailWithAction(inDriver != gAudioServerPlugInDriverRef, theAnswer = kAudioHardwareBadObjectError, Done, "SalsaHAL_StopIO: bad driver reference");
	FailWithAction(inDeviceObjectID != kObjectID_Device, theAnswer = kAudioHardwareBadObjectError, Done, "SalsaHAL_StopIO: bad device ID");
	atomic_fetch_add(&gProbeStops, 1);

	//	we need to hold the state lock
	pthread_mutex_lock(&gPlugIn_StateMutex);
	
	//	figure out what we need to do
	if(gDevice_IOIsRunning == 0)
	{
		//	underflowing is an error
		theAnswer = kAudioHardwareIllegalOperationError;
	}
	else if(gDevice_IOIsRunning == 1)
	{
		//	We need to stop the hardware, which in this case means that there's nothing to do.
		gDevice_IOIsRunning = 0;
	}
	else
	{
		//	IO is still running, so just bump the counter
		--gDevice_IOIsRunning;
	}
	
	//	unlock the state lock
	pthread_mutex_unlock(&gPlugIn_StateMutex);
	
Done:
	return theAnswer;
}

static OSStatus	SalsaHAL_GetZeroTimeStamp(AudioServerPlugInDriverRef inDriver, AudioObjectID inDeviceObjectID, UInt32 inClientID, Float64* outSampleTime, UInt64* outHostTime, UInt64* outSeed)
{
	//	This method returns the current zero time stamp for the device. The HAL models the timing of
	//	a device as a series of time stamps that relate the sample time to a host time. The zero
	//	time stamps are spaced such that the sample times are the value of
	//	kAudioDevicePropertyZeroTimeStampPeriod apart. This is often modeled using a ring buffer
	//	where the zero time stamp is updated when wrapping around the ring buffer.
	//
	//	For this device, the zero time stamps' sample time increments every kDevice_RingBufferSize
	//	frames and the host time increments by kDevice_RingBufferSize * gDevice_HostTicksPerFrame.
	
	#pragma unused(inClientID)
	
	//	declare the local variables
	OSStatus theAnswer = 0;
	UInt64 theCurrentHostTime;
	Float64 theHostTicksPerRingBuffer;
	
	//	check the arguments
	FailWithAction(inDriver != gAudioServerPlugInDriverRef, theAnswer = kAudioHardwareBadObjectError, Done, "SalsaHAL_GetZeroTimeStamp: bad driver reference");
	FailWithAction(inDeviceObjectID != kObjectID_Device, theAnswer = kAudioHardwareBadObjectError, Done, "SalsaHAL_GetZeroTimeStamp: bad device ID");
	atomic_fetch_add_explicit(&gProbeTimeStamps, 1, memory_order_relaxed);

	uint64_t theClockSample = 0;
	uint64_t theClockHost = 0;
	uint64_t theClockSeed = 0;
	if(gCarrierShared != NULL && SalsaCarrierReadClock(gCarrierShared, &theClockSample, &theClockHost, &theClockSeed))
	{
		uint64_t theNow = mach_absolute_time();
		uint64_t theElapsedFrames = (theNow > theClockHost) ? (uint64_t)((double)(theNow - theClockHost) / gDevice_HostTicksPerFrame) : 0;
		uint64_t theCurrentSample = theClockSample + theElapsedFrames;
		uint64_t theZeroIndex = theCurrentSample / kDevice_RingBufferSize;
		uint64_t theZeroSample = theZeroIndex * kDevice_RingBufferSize;
		double theHostOffset = ((double)theZeroSample - (double)theClockSample) * gDevice_HostTicksPerFrame;
		double theHost = (double)theClockHost + theHostOffset;
		*outSampleTime = (Float64)theZeroSample;
		*outHostTime = (theHost > 0.0) ? (UInt64)theHost : 0;
		*outSeed = theClockSeed;
		goto Done;
	}

	//	we need to hold the locks
	pthread_mutex_lock(&gDevice_IOMutex);
	
	//	get the current host time
	theCurrentHostTime = mach_absolute_time();
	
	//	calculate the next host time
	theHostTicksPerRingBuffer = gDevice_HostTicksPerFrame * ((Float64)kDevice_RingBufferSize);
	gDevice_NumberTimeStamps = (UInt64)((theCurrentHostTime - gDevice_AnchorHostTime) / theHostTicksPerRingBuffer);
	
	//	set the return values
	*outSampleTime = gDevice_NumberTimeStamps * kDevice_RingBufferSize;
	*outHostTime = gDevice_AnchorHostTime + (((Float64)gDevice_NumberTimeStamps) * theHostTicksPerRingBuffer);
	*outSeed = atomic_load(&gSessionGeneration);
	
	//	unlock the state lock
	pthread_mutex_unlock(&gDevice_IOMutex);
	
Done:
	return theAnswer;
}

static OSStatus	SalsaHAL_WillDoIOOperation(AudioServerPlugInDriverRef inDriver, AudioObjectID inDeviceObjectID, UInt32 inClientID, UInt32 inOperationID, Boolean* outWillDo, Boolean* outWillDoInPlace)
{
	//	This method returns whether or not the device will do a given IO operation. For this device,
	//	we only support reading input data and writing output data.
	
	#pragma unused(inClientID)
	
	//	declare the local variables
	OSStatus theAnswer = 0;
	
	//	check the arguments
	FailWithAction(inDriver != gAudioServerPlugInDriverRef, theAnswer = kAudioHardwareBadObjectError, Done, "SalsaHAL_WillDoIOOperation: bad driver reference");
	FailWithAction(inDeviceObjectID != kObjectID_Device, theAnswer = kAudioHardwareBadObjectError, Done, "SalsaHAL_WillDoIOOperation: bad device ID");

	//	figure out if we support the operation
	bool willDo = false;
	bool willDoInPlace = true;
	switch(inOperationID)
	{
		case kAudioServerPlugInIOOperationReadInput:
			willDo = !SALSA_HAL_OUTPUT_ONLY;
			willDoInPlace = true;
			break;
			
		case kAudioServerPlugInIOOperationWriteMix:
			willDo = true;
			willDoInPlace = true;
			break;
			
	};
	
	//	fill out the return values
	if(outWillDo != NULL)
	{
		*outWillDo = willDo;
	}
	if(outWillDoInPlace != NULL)
	{
		*outWillDoInPlace = willDoInPlace;
	}

Done:
	return theAnswer;
}

static OSStatus	SalsaHAL_BeginIOOperation(AudioServerPlugInDriverRef inDriver, AudioObjectID inDeviceObjectID, UInt32 inClientID, UInt32 inOperationID, UInt32 inIOBufferFrameSize, const AudioServerPlugInIOCycleInfo* inIOCycleInfo)
{
	//	This is called at the beginning of an IO operation. This device doesn't do anything, so just
	//	check the arguments and return.
	
	#pragma unused(inClientID, inOperationID, inIOBufferFrameSize, inIOCycleInfo)
	
	//	declare the local variables
	OSStatus theAnswer = 0;
	
	//	check the arguments
	FailWithAction(inDriver != gAudioServerPlugInDriverRef, theAnswer = kAudioHardwareBadObjectError, Done, "SalsaHAL_BeginIOOperation: bad driver reference");
	FailWithAction(inDeviceObjectID != kObjectID_Device, theAnswer = kAudioHardwareBadObjectError, Done, "SalsaHAL_BeginIOOperation: bad device ID");

Done:
	return theAnswer;
}

static OSStatus	SalsaHAL_DoIOOperation(AudioServerPlugInDriverRef inDriver, AudioObjectID inDeviceObjectID, AudioObjectID inStreamObjectID, UInt32 inClientID, UInt32 inOperationID, UInt32 inIOBufferFrameSize, const AudioServerPlugInIOCycleInfo* inIOCycleInfo, void* ioMainBuffer, void* ioSecondaryBuffer)
{
	//	WriteMix copies the opaque carrier words from the player into the
	//	bounded loopback ring. ReadInput drains that ring for the capture
	//	client. This callback performs only fixed-size copies and atomic index
	//	updates: no allocation, logging, file access, or blocking IPC.
	
	#pragma unused(inClientID, ioSecondaryBuffer)
	
	//	declare the local variables
	OSStatus theAnswer = 0;
	
	//	check the arguments
	FailWithAction(inDriver != gAudioServerPlugInDriverRef, theAnswer = kAudioHardwareBadObjectError, Done, "SalsaHAL_DoIOOperation: bad driver reference");
	FailWithAction(inDeviceObjectID != kObjectID_Device, theAnswer = kAudioHardwareBadObjectError, Done, "SalsaHAL_DoIOOperation: bad device ID");
	FailWithAction((inStreamObjectID != kObjectID_Stream_Input) && (inStreamObjectID != kObjectID_Stream_Output), theAnswer = kAudioHardwareBadObjectError, Done, "SalsaHAL_DoIOOperation: bad stream ID");

	if(inOperationID == kAudioServerPlugInIOOperationWriteMix)
	{
		atomic_fetch_add_explicit(&gProbeWrites, 1, memory_order_relaxed);
		atomic_fetch_add_explicit(&gProbeWriteFrames, inIOBufferFrameSize, memory_order_relaxed);
		atomic_store_explicit(&gProbeLastBufferFrames, inIOBufferFrameSize, memory_order_relaxed);
		if(inStreamObjectID != kObjectID_Stream_Output || ioMainBuffer == NULL)
		{
			goto Done;
		}

		UInt32 theChannels = atomic_load_explicit(&gCarrierOutputChannels, memory_order_relaxed);
		if((theChannels != 2) && (theChannels != 8))
		{
			theChannels = 2;
		}
		const UInt8* theSource = (const UInt8*)ioMainBuffer;
		if(gProbeRecordData != NULL)
		{
			uint64_t used = atomic_load_explicit(&gProbeRecordedBytes, memory_order_relaxed);
			size_t bytes = (size_t)inIOBufferFrameSize * theChannels * 2;
			if(bytes <= kProbeRecordCapacity - used)
			{
				memcpy(gProbeRecordData + used, theSource, bytes);
				atomic_store_explicit(&gProbeRecordedBytes, used + bytes, memory_order_relaxed);
			}
			else
				atomic_fetch_add_explicit(&gProbeRecordOverflow, 1, memory_order_relaxed);
		}
		SalsaHAL_TransportWrite(inIOCycleInfo, inIOBufferFrameSize, theChannels, theSource);
#if !SALSA_HAL_OUTPUT_ONLY
		UInt64 theWriteFrame = atomic_load_explicit(&gCarrierWriteFrame, memory_order_relaxed);
		UInt64 theReadFrame = atomic_load_explicit(&gCarrierReadFrame, memory_order_acquire);
		if(theWriteFrame - theReadFrame + inIOBufferFrameSize > kCarrierRingFrames)
		{
			// Drop the new block. The producer never changes the consumer index.
			atomic_fetch_add_explicit(&gCarrierDroppedFrames, inIOBufferFrameSize, memory_order_relaxed);
			goto Done;
		}
		for(UInt32 theFrame = 0; theFrame < inIOBufferFrameSize; ++theFrame)
		{
			UInt8* theDestination = gCarrierRing[(theWriteFrame + theFrame) % kCarrierRingFrames];
			memset(theDestination, 0, kCarrierRingStrideBytes);
			memcpy(theDestination, theSource + ((size_t)theFrame * theChannels * 2), (size_t)theChannels * 2);
		}
		atomic_store_explicit(&gCarrierWriteFrame, theWriteFrame + inIOBufferFrameSize, memory_order_release);
#endif
	}
	else if(inOperationID == kAudioServerPlugInIOOperationReadInput)
	{
		atomic_fetch_add_explicit(&gProbeReads, 1, memory_order_relaxed);
		if(inStreamObjectID != kObjectID_Stream_Input || ioMainBuffer == NULL)
		{
			goto Done;
		}

		UInt32 theChannels = gStream_Input_Format.mChannelsPerFrame;
		if((theChannels != 2) && (theChannels != 8))
		{
			theChannels = kSalsaHAL_CaptureChannels;
		}
		UInt8* theDestination = (UInt8*)ioMainBuffer;
		UInt64 theWriteFrame = atomic_load_explicit(&gCarrierWriteFrame, memory_order_acquire);
		UInt64 theReadFrame = atomic_load_explicit(&gCarrierReadFrame, memory_order_relaxed);
		UInt64 theAvailableFrames = (theWriteFrame > theReadFrame) ? (theWriteFrame - theReadFrame) : 0;
		for(UInt32 theFrame = 0; theFrame < inIOBufferFrameSize; ++theFrame)
		{
			UInt8* theFrameDestination = theDestination + ((size_t)theFrame * theChannels * 2);
			if((UInt64)theFrame < theAvailableFrames)
			{
				const UInt8* theSource = gCarrierRing[(theReadFrame + theFrame) % kCarrierRingFrames];
				memcpy(theFrameDestination, theSource, (size_t)theChannels * 2);
			}
			else
			{
				memset(theFrameDestination, 0, (size_t)theChannels * 2);
			}
		}
		if(theAvailableFrames > inIOBufferFrameSize)
		{
			theAvailableFrames = inIOBufferFrameSize;
		}
		atomic_store_explicit(&gCarrierReadFrame, theReadFrame + theAvailableFrames, memory_order_release);
	}

Done:
	return theAnswer;
}

static OSStatus	SalsaHAL_EndIOOperation(AudioServerPlugInDriverRef inDriver, AudioObjectID inDeviceObjectID, UInt32 inClientID, UInt32 inOperationID, UInt32 inIOBufferFrameSize, const AudioServerPlugInIOCycleInfo* inIOCycleInfo)
{
	//	This is called at the end of an IO operation. This device doesn't do anything, so just check
	//	the arguments and return.
	
	#pragma unused(inClientID, inOperationID, inIOBufferFrameSize, inIOCycleInfo)
	
	//	declare the local variables
	OSStatus theAnswer = 0;
	
	//	check the arguments
	FailWithAction(inDriver != gAudioServerPlugInDriverRef, theAnswer = kAudioHardwareBadObjectError, Done, "SalsaHAL_EndIOOperation: bad driver reference");
	FailWithAction(inDeviceObjectID != kObjectID_Device, theAnswer = kAudioHardwareBadObjectError, Done, "SalsaHAL_EndIOOperation: bad device ID");

Done:
	return theAnswer;
}
