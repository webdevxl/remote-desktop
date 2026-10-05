// LanKVM Microphone: an Audio Server plug-in (a driver coreaudiod loads from
// /Library/Audio/Plug-Ins/HAL) that gives this Mac a microphone any app can pick, so a Mac that
// views or controls this one can talk into this Mac's apps (a call, a recording, dictation).
//
// It makes two devices that share one clock and one ring buffer:
// - "LanKVM Microphone", input only, for apps: it plays what was written to the other one.
// - "LanKVM Microphone Feed", output only and hidden (no Sound settings or app lists it; LanKVM
//   finds it by its UID). LanKVM plays the other Mac's microphone into it.
//
// The ring buffer holds each frame with the sample time it was written for, and a frame is read
// only at that time: what nobody writes reads as silence, never as audio from a lap before.
// 48 kHz mono 32-bit float on both sides, so nothing converts anything.
//
// Built by scripts/build-audio-driver.sh, which also runs driver-test.c against it.

#include <CoreAudio/AudioServerPlugIn.h>
#include <mach/mach_time.h>
#include <stdatomic.h>
#include <stddef.h>
#include <stdint.h>
#include <string.h>

#pragma mark Objects and format

enum {
    kObjectPlugIn = kAudioObjectPlugInObject,
    kObjectMic = 2,
    kObjectMicStream = 3,
    kObjectFeed = 4,
    kObjectFeedStream = 5,
};

#define kMicUID "dev.lankvm.microphone"
#define kFeedUID "dev.lankvm.microphone.feed"
#define kModelUID "dev.lankvm.microphone.model"

static const Float64 kSampleRate = 48000.0;
// Frames in the ring buffer, and the zero time stamp period: 341 ms, far more than the writer can
// be ahead of the reader (an IO buffer and a safety offset each way).
#define kRingFrames 16384
// Both devices tick with one clock: the same domain tells aggregate devices they never drift.
static const UInt32 kClockDomain = 'LKVM';

static AudioStreamBasicDescription Format(void) {
    AudioStreamBasicDescription f = {0};
    f.mSampleRate = kSampleRate;
    f.mFormatID = kAudioFormatLinearPCM;
    f.mFormatFlags = kAudioFormatFlagIsFloat | kAudioFormatFlagsNativeEndian | kAudioFormatFlagIsPacked;
    f.mBytesPerPacket = sizeof(Float32);
    f.mFramesPerPacket = 1;
    f.mBytesPerFrame = sizeof(Float32);
    f.mChannelsPerFrame = 1;
    f.mBitsPerChannel = 32;
    return f;
}

static bool IsDevice(AudioObjectID id) { return id == kObjectMic || id == kObjectFeed; }
static bool IsStream(AudioObjectID id) { return id == kObjectMicStream || id == kObjectFeedStream; }
static AudioObjectID StreamOf(AudioObjectID device) { return device == kObjectMic ? kObjectMicStream : kObjectFeedStream; }
static AudioObjectID DeviceOf(AudioObjectID stream) { return stream == kObjectMicStream ? kObjectMic : kObjectFeed; }
// The mic's stream is input, the feed's output.
static bool IsInput(AudioObjectID object) { return object == kObjectMic || object == kObjectMicStream; }

#pragma mark State

static AudioServerPlugInHostRef gHost;
static _Atomic UInt32 gRefCount;
// Host time the clock started at, and host ticks per frame.
static UInt64 gAnchorHostTime;
static Float64 gTicksPerFrame;
// IO clients running, per device (index 0: mic, 1: feed).
static _Atomic SInt32 gRunning[2];
static _Atomic UInt32 gStreamActive[2] = {1, 1};

static float gRing[kRingFrames];
// The sample time each frame of the ring was written for (INT64_MIN: none yet).
static _Atomic int64_t gWritten[kRingFrames];

static int Slot(AudioObjectID object) { return IsInput(object) ? 0 : 1; }

static void WriteRing(Float64 sampleTime, const float *in, UInt32 frames) {
    int64_t t = (int64_t)sampleTime;
    for (UInt32 i = 0; i < frames; i++, t++) {
        uint32_t slot = (uint32_t)(t & (kRingFrames - 1));
        gRing[slot] = in[i];
        atomic_store_explicit(&gWritten[slot], t, memory_order_release);
    }
}

static void ReadRing(Float64 sampleTime, float *out, UInt32 frames) {
    int64_t t = (int64_t)sampleTime;
    for (UInt32 i = 0; i < frames; i++, t++) {
        uint32_t slot = (uint32_t)(t & (kRingFrames - 1));
        out[i] = atomic_load_explicit(&gWritten[slot], memory_order_acquire) == t ? gRing[slot] : 0.0f;
    }
}

#pragma mark Properties

// One property's value. Arrays (itemSize != 0) are cut to what the caller's buffer holds; other
// values need room for all of it.
typedef struct {
    UInt32 size;
    UInt32 itemSize;
    union {
        UInt32 u32;
        Float64 f64;
        CFStringRef string;
        AudioObjectID ids[2];
        UInt32 u32s[2];
        AudioStreamBasicDescription format;
        AudioStreamRangedDescription rangedFormat;
        AudioValueRange range;
        AudioChannelLayout layout;
    } v;
} Value;

static Value U32(UInt32 x) { Value v = {.size = sizeof(UInt32)}; v.v.u32 = x; return v; }
static Value F64(Float64 x) { Value v = {.size = sizeof(Float64)}; v.v.f64 = x; return v; }
// A constant string: releasing it (as callers do) does nothing.
static Value String(CFStringRef s) { Value v = {.size = sizeof(CFStringRef)}; v.v.string = s; return v; }
static Value Ids(UInt32 count, AudioObjectID a, AudioObjectID b) {
    Value v = {.size = count * (UInt32)sizeof(AudioObjectID), .itemSize = sizeof(AudioObjectID)};
    v.v.ids[0] = a;
    v.v.ids[1] = b;
    return v;
}

static OSStatus PlugInProperty(const AudioObjectPropertyAddress *a, UInt32 qualifierSize, const void *qualifier, Value *out) {
    switch (a->mSelector) {
    case kAudioObjectPropertyBaseClass: *out = U32(kAudioObjectClassID); return noErr;
    case kAudioObjectPropertyClass: *out = U32(kAudioPlugInClassID); return noErr;
    case kAudioObjectPropertyOwner: *out = U32(kAudioObjectUnknown); return noErr;
    case kAudioObjectPropertyManufacturer: *out = String(CFSTR("LanKVM")); return noErr;
    case kAudioObjectPropertyOwnedObjects:
    case kAudioPlugInPropertyDeviceList: *out = Ids(2, kObjectMic, kObjectFeed); return noErr;
    case kAudioPlugInPropertyTranslateUIDToDevice: {
        AudioObjectID found = kAudioObjectUnknown;
        if (qualifier && qualifierSize == sizeof(CFStringRef)) {
            CFStringRef uid = *(const CFStringRef *)qualifier;
            if (uid && CFStringCompare(uid, CFSTR(kMicUID), 0) == kCFCompareEqualTo) found = kObjectMic;
            if (uid && CFStringCompare(uid, CFSTR(kFeedUID), 0) == kCFCompareEqualTo) found = kObjectFeed;
        }
        *out = U32(found);
        return noErr;
    }
    case kAudioPlugInPropertyResourceBundle: *out = String(CFSTR("")); return noErr;
    default: return kAudioHardwareUnknownPropertyError;
    }
}

static OSStatus DeviceProperty(AudioObjectID device, const AudioObjectPropertyAddress *a, Value *out) {
    bool mic = device == kObjectMic;
    // Whether the address's scope covers the device's one stream.
    bool inScope = a->mScope == kAudioObjectPropertyScopeGlobal
        || a->mScope == (mic ? kAudioObjectPropertyScopeInput : kAudioObjectPropertyScopeOutput);
    switch (a->mSelector) {
    case kAudioObjectPropertyBaseClass: *out = U32(kAudioObjectClassID); return noErr;
    case kAudioObjectPropertyClass: *out = U32(kAudioDeviceClassID); return noErr;
    case kAudioObjectPropertyOwner: *out = U32(kObjectPlugIn); return noErr;
    case kAudioObjectPropertyName:
        *out = String(mic ? CFSTR("LanKVM Microphone") : CFSTR("LanKVM Microphone Feed"));
        return noErr;
    case kAudioObjectPropertyManufacturer: *out = String(CFSTR("LanKVM")); return noErr;
    case kAudioObjectPropertyOwnedObjects:
    case kAudioDevicePropertyStreams:
        *out = inScope ? Ids(1, StreamOf(device), 0) : Ids(0, 0, 0);
        return noErr;
    case kAudioObjectPropertyControlList: *out = Ids(0, 0, 0); return noErr;
    case kAudioDevicePropertyDeviceUID: *out = String(mic ? CFSTR(kMicUID) : CFSTR(kFeedUID)); return noErr;
    case kAudioDevicePropertyModelUID: *out = String(CFSTR(kModelUID)); return noErr;
    case kAudioDevicePropertyTransportType: *out = U32(kAudioDeviceTransportTypeVirtual); return noErr;
    case kAudioDevicePropertyRelatedDevices: *out = Ids(1, device, 0); return noErr;
    case kAudioDevicePropertyClockDomain: *out = U32(kClockDomain); return noErr;
    case kAudioDevicePropertyDeviceIsAlive: *out = U32(1); return noErr;
    case kAudioDevicePropertyDeviceIsRunning: *out = U32(atomic_load(&gRunning[Slot(device)]) > 0); return noErr;
    // Apps may make the microphone their default input; nothing may pick the feed.
    case kAudioDevicePropertyDeviceCanBeDefaultDevice: *out = U32(mic && a->mScope != kAudioObjectPropertyScopeOutput); return noErr;
    case kAudioDevicePropertyDeviceCanBeDefaultSystemDevice: *out = U32(0); return noErr;
    case kAudioDevicePropertyLatency:
    case kAudioDevicePropertySafetyOffset: *out = U32(0); return noErr;
    case kAudioDevicePropertyNominalSampleRate: *out = F64(kSampleRate); return noErr;
    case kAudioDevicePropertyAvailableNominalSampleRates:
        out->size = sizeof(AudioValueRange);
        out->itemSize = sizeof(AudioValueRange);
        out->v.range = (AudioValueRange){kSampleRate, kSampleRate};
        return noErr;
    case kAudioDevicePropertyIsHidden: *out = U32(!mic); return noErr;
    case kAudioDevicePropertyPreferredChannelsForStereo:
        out->size = 2 * sizeof(UInt32);
        out->itemSize = 0;
        out->v.u32s[0] = 1;
        out->v.u32s[1] = 1;
        return noErr;
    case kAudioDevicePropertyPreferredChannelLayout:
        out->size = (UInt32)sizeof(AudioChannelLayout);
        out->itemSize = 0;
        memset(&out->v.layout, 0, sizeof(AudioChannelLayout));
        out->v.layout.mChannelLayoutTag = kAudioChannelLayoutTag_UseChannelDescriptions;
        out->v.layout.mNumberChannelDescriptions = 1;
        out->v.layout.mChannelDescriptions[0].mChannelLabel = kAudioChannelLabel_Mono;
        return noErr;
    case kAudioDevicePropertyZeroTimeStampPeriod: *out = U32(kRingFrames); return noErr;
    default: return kAudioHardwareUnknownPropertyError;
    }
}

static OSStatus StreamProperty(AudioObjectID stream, const AudioObjectPropertyAddress *a, Value *out) {
    switch (a->mSelector) {
    case kAudioObjectPropertyBaseClass: *out = U32(kAudioObjectClassID); return noErr;
    case kAudioObjectPropertyClass: *out = U32(kAudioStreamClassID); return noErr;
    case kAudioObjectPropertyOwner: *out = U32(DeviceOf(stream)); return noErr;
    case kAudioObjectPropertyOwnedObjects: *out = Ids(0, 0, 0); return noErr;
    case kAudioObjectPropertyName:
        *out = String(IsInput(stream) ? CFSTR("LanKVM Microphone") : CFSTR("LanKVM Microphone Feed"));
        return noErr;
    case kAudioStreamPropertyIsActive: *out = U32(atomic_load(&gStreamActive[Slot(stream)])); return noErr;
    case kAudioStreamPropertyDirection: *out = U32(IsInput(stream) ? 1 : 0); return noErr;
    case kAudioStreamPropertyTerminalType:
        *out = U32(IsInput(stream) ? kAudioStreamTerminalTypeMicrophone : kAudioStreamTerminalTypeLine);
        return noErr;
    case kAudioStreamPropertyStartingChannel: *out = U32(1); return noErr;
    case kAudioStreamPropertyLatency: *out = U32(0); return noErr;
    case kAudioStreamPropertyVirtualFormat:
    case kAudioStreamPropertyPhysicalFormat:
        out->size = sizeof(AudioStreamBasicDescription);
        out->itemSize = 0;
        out->v.format = Format();
        return noErr;
    case kAudioStreamPropertyAvailableVirtualFormats:
    case kAudioStreamPropertyAvailablePhysicalFormats:
        out->size = sizeof(AudioStreamRangedDescription);
        out->itemSize = sizeof(AudioStreamRangedDescription);
        out->v.rangedFormat.mFormat = Format();
        out->v.rangedFormat.mSampleRateRange = (AudioValueRange){kSampleRate, kSampleRate};
        return noErr;
    default: return kAudioHardwareUnknownPropertyError;
    }
}

static OSStatus Lookup(AudioObjectID object, const AudioObjectPropertyAddress *a, UInt32 qualifierSize, const void *qualifier, Value *out) {
    if (!a) return kAudioHardwareIllegalOperationError;
    memset(out, 0, sizeof(*out));
    if (object == kObjectPlugIn) return PlugInProperty(a, qualifierSize, qualifier, out);
    if (IsDevice(object)) return DeviceProperty(object, a, out);
    if (IsStream(object)) return StreamProperty(object, a, out);
    return kAudioHardwareBadObjectError;
}

#pragma mark Driver interface

static HRESULT QueryInterface(void *driver, REFIID uuid, LPVOID *outInterface);
static ULONG AddRef(void *driver);
static ULONG Release(void *driver);
static OSStatus Initialize(AudioServerPlugInDriverRef driver, AudioServerPlugInHostRef host);
static OSStatus CreateDevice(AudioServerPlugInDriverRef driver, CFDictionaryRef description, const AudioServerPlugInClientInfo *client, AudioObjectID *outDevice);
static OSStatus DestroyDevice(AudioServerPlugInDriverRef driver, AudioObjectID device);
static OSStatus AddDeviceClient(AudioServerPlugInDriverRef driver, AudioObjectID device, const AudioServerPlugInClientInfo *client);
static OSStatus RemoveDeviceClient(AudioServerPlugInDriverRef driver, AudioObjectID device, const AudioServerPlugInClientInfo *client);
static OSStatus PerformDeviceConfigurationChange(AudioServerPlugInDriverRef driver, AudioObjectID device, UInt64 action, void *info);
static OSStatus AbortDeviceConfigurationChange(AudioServerPlugInDriverRef driver, AudioObjectID device, UInt64 action, void *info);
static Boolean HasProperty(AudioServerPlugInDriverRef driver, AudioObjectID object, pid_t client, const AudioObjectPropertyAddress *address);
static OSStatus IsPropertySettable(AudioServerPlugInDriverRef driver, AudioObjectID object, pid_t client, const AudioObjectPropertyAddress *address, Boolean *outSettable);
static OSStatus GetPropertyDataSize(AudioServerPlugInDriverRef driver, AudioObjectID object, pid_t client, const AudioObjectPropertyAddress *address, UInt32 qualifierSize, const void *qualifier, UInt32 *outSize);
static OSStatus GetPropertyData(AudioServerPlugInDriverRef driver, AudioObjectID object, pid_t client, const AudioObjectPropertyAddress *address, UInt32 qualifierSize, const void *qualifier, UInt32 dataSize, UInt32 *outSize, void *outData);
static OSStatus SetPropertyData(AudioServerPlugInDriverRef driver, AudioObjectID object, pid_t client, const AudioObjectPropertyAddress *address, UInt32 qualifierSize, const void *qualifier, UInt32 dataSize, const void *data);
static OSStatus StartIO(AudioServerPlugInDriverRef driver, AudioObjectID device, UInt32 client);
static OSStatus StopIO(AudioServerPlugInDriverRef driver, AudioObjectID device, UInt32 client);
static OSStatus GetZeroTimeStamp(AudioServerPlugInDriverRef driver, AudioObjectID device, UInt32 client, Float64 *outSampleTime, UInt64 *outHostTime, UInt64 *outSeed);
static OSStatus WillDoIOOperation(AudioServerPlugInDriverRef driver, AudioObjectID device, UInt32 client, UInt32 operation, Boolean *outWillDo, Boolean *outInPlace);
static OSStatus BeginIOOperation(AudioServerPlugInDriverRef driver, AudioObjectID device, UInt32 client, UInt32 operation, UInt32 frames, const AudioServerPlugInIOCycleInfo *cycle);
static OSStatus DoIOOperation(AudioServerPlugInDriverRef driver, AudioObjectID device, AudioObjectID stream, UInt32 client, UInt32 operation, UInt32 frames, const AudioServerPlugInIOCycleInfo *cycle, void *mainBuffer, void *secondaryBuffer);
static OSStatus EndIOOperation(AudioServerPlugInDriverRef driver, AudioObjectID device, UInt32 client, UInt32 operation, UInt32 frames, const AudioServerPlugInIOCycleInfo *cycle);

static AudioServerPlugInDriverInterface gInterface = {
    NULL,
    QueryInterface, AddRef, Release,
    Initialize, CreateDevice, DestroyDevice, AddDeviceClient, RemoveDeviceClient,
    PerformDeviceConfigurationChange, AbortDeviceConfigurationChange,
    HasProperty, IsPropertySettable, GetPropertyDataSize, GetPropertyData, SetPropertyData,
    StartIO, StopIO, GetZeroTimeStamp, WillDoIOOperation, BeginIOOperation, DoIOOperation, EndIOOperation,
};
static AudioServerPlugInDriverInterface *gInterfacePtr = &gInterface;
static AudioServerPlugInDriverRef gDriver = &gInterfacePtr;

// The factory named in Info.plist (CFPlugInFactories).
__attribute__((visibility("default"))) void *LanKVMMicrophone_Create(CFAllocatorRef allocator, CFUUIDRef type) {
    (void)allocator;
    return CFEqual(type, kAudioServerPlugInTypeUUID) ? gDriver : NULL;
}

static HRESULT QueryInterface(void *driver, REFIID uuid, LPVOID *outInterface) {
    if (driver != gDriver) return kAudioHardwareBadObjectError;
    if (!outInterface) return kAudioHardwareIllegalOperationError;
    CFUUIDRef requested = CFUUIDCreateFromUUIDBytes(NULL, uuid);
    if (!requested) return kAudioHardwareIllegalOperationError;
    HRESULT result = E_NOINTERFACE;
    if (CFEqual(requested, IUnknownUUID) || CFEqual(requested, kAudioServerPlugInDriverInterfaceUUID)) {
        atomic_fetch_add(&gRefCount, 1);
        *outInterface = gDriver;
        result = S_OK;
    }
    CFRelease(requested);
    return result;
}

// The driver is static: counting is all there is.
static ULONG AddRef(void *driver) {
    return driver == gDriver ? atomic_fetch_add(&gRefCount, 1) + 1 : 0;
}

static ULONG Release(void *driver) {
    if (driver != gDriver) return 0;
    UInt32 count = atomic_load(&gRefCount);
    while (count > 0 && !atomic_compare_exchange_weak(&gRefCount, &count, count - 1)) {}
    return count > 0 ? count - 1 : 0;
}

static OSStatus Initialize(AudioServerPlugInDriverRef driver, AudioServerPlugInHostRef host) {
    if (driver != gDriver) return kAudioHardwareBadObjectError;
    gHost = host;
    mach_timebase_info_data_t timebase;
    mach_timebase_info(&timebase);
    Float64 ticksPerSecond = 1e9 * (Float64)timebase.denom / (Float64)timebase.numer;
    gTicksPerFrame = ticksPerSecond / kSampleRate;
    gAnchorHostTime = mach_absolute_time();
    for (int i = 0; i < kRingFrames; i++) atomic_store(&gWritten[i], INT64_MIN);
    return noErr;
}

static OSStatus CreateDevice(AudioServerPlugInDriverRef driver, CFDictionaryRef description, const AudioServerPlugInClientInfo *client, AudioObjectID *outDevice) {
    (void)description; (void)client; (void)outDevice;
    return driver == gDriver ? kAudioHardwareUnsupportedOperationError : kAudioHardwareBadObjectError;
}

static OSStatus DestroyDevice(AudioServerPlugInDriverRef driver, AudioObjectID device) {
    (void)device;
    return driver == gDriver ? kAudioHardwareUnsupportedOperationError : kAudioHardwareBadObjectError;
}

static OSStatus AddDeviceClient(AudioServerPlugInDriverRef driver, AudioObjectID device, const AudioServerPlugInClientInfo *client) {
    (void)client;
    if (driver != gDriver || !IsDevice(device)) return kAudioHardwareBadObjectError;
    return noErr;
}

static OSStatus RemoveDeviceClient(AudioServerPlugInDriverRef driver, AudioObjectID device, const AudioServerPlugInClientInfo *client) {
    (void)client;
    if (driver != gDriver || !IsDevice(device)) return kAudioHardwareBadObjectError;
    return noErr;
}

// The devices never ask to change their configuration.
static OSStatus PerformDeviceConfigurationChange(AudioServerPlugInDriverRef driver, AudioObjectID device, UInt64 action, void *info) {
    (void)action; (void)info;
    if (driver != gDriver || !IsDevice(device)) return kAudioHardwareBadObjectError;
    return noErr;
}

static OSStatus AbortDeviceConfigurationChange(AudioServerPlugInDriverRef driver, AudioObjectID device, UInt64 action, void *info) {
    (void)action; (void)info;
    if (driver != gDriver || !IsDevice(device)) return kAudioHardwareBadObjectError;
    return noErr;
}

static Boolean HasProperty(AudioServerPlugInDriverRef driver, AudioObjectID object, pid_t client, const AudioObjectPropertyAddress *address) {
    (void)client;
    Value value;
    return driver == gDriver && Lookup(object, address, 0, NULL, &value) == noErr;
}

// The sample rate, the formats and whether a stream is active can be "set", to what they are.
static bool Settable(AudioObjectID object, const AudioObjectPropertyAddress *a) {
    if (IsDevice(object)) return a->mSelector == kAudioDevicePropertyNominalSampleRate;
    if (IsStream(object)) {
        return a->mSelector == kAudioStreamPropertyIsActive || a->mSelector == kAudioStreamPropertyVirtualFormat
            || a->mSelector == kAudioStreamPropertyPhysicalFormat;
    }
    return false;
}

static OSStatus IsPropertySettable(AudioServerPlugInDriverRef driver, AudioObjectID object, pid_t client, const AudioObjectPropertyAddress *address, Boolean *outSettable) {
    (void)client;
    if (driver != gDriver) return kAudioHardwareBadObjectError;
    if (!outSettable) return kAudioHardwareIllegalOperationError;
    Value value;
    OSStatus status = Lookup(object, address, 0, NULL, &value);
    if (status != noErr) return status;
    *outSettable = Settable(object, address);
    return noErr;
}

static OSStatus GetPropertyDataSize(AudioServerPlugInDriverRef driver, AudioObjectID object, pid_t client, const AudioObjectPropertyAddress *address, UInt32 qualifierSize, const void *qualifier, UInt32 *outSize) {
    (void)client;
    if (driver != gDriver) return kAudioHardwareBadObjectError;
    if (!outSize) return kAudioHardwareIllegalOperationError;
    Value value;
    OSStatus status = Lookup(object, address, qualifierSize, qualifier, &value);
    if (status == noErr) *outSize = value.size;
    return status;
}

static OSStatus GetPropertyData(AudioServerPlugInDriverRef driver, AudioObjectID object, pid_t client, const AudioObjectPropertyAddress *address, UInt32 qualifierSize, const void *qualifier, UInt32 dataSize, UInt32 *outSize, void *outData) {
    (void)client;
    if (driver != gDriver) return kAudioHardwareBadObjectError;
    if (!outSize || !outData) return kAudioHardwareIllegalOperationError;
    Value value;
    OSStatus status = Lookup(object, address, qualifierSize, qualifier, &value);
    if (status != noErr) return status;
    UInt32 size = value.size;
    if (value.itemSize) {
        // As many items as fit.
        if (dataSize < size) size = dataSize / value.itemSize * value.itemSize;
    } else if (dataSize < size) {
        return kAudioHardwareBadPropertySizeError;
    }
    memcpy(outData, &value.v, size);
    *outSize = size;
    return noErr;
}

static OSStatus SetPropertyData(AudioServerPlugInDriverRef driver, AudioObjectID object, pid_t client, const AudioObjectPropertyAddress *address, UInt32 qualifierSize, const void *qualifier, UInt32 dataSize, const void *data) {
    (void)client; (void)qualifierSize; (void)qualifier;
    if (driver != gDriver) return kAudioHardwareBadObjectError;
    if (!address || !data) return kAudioHardwareIllegalOperationError;
    Value value;
    OSStatus status = Lookup(object, address, 0, NULL, &value);
    if (status != noErr) return status;
    if (!Settable(object, address)) return kAudioHardwareUnsupportedOperationError;
    switch (address->mSelector) {
    case kAudioDevicePropertyNominalSampleRate:
        if (dataSize != sizeof(Float64)) return kAudioHardwareBadPropertySizeError;
        return *(const Float64 *)data == kSampleRate ? noErr : kAudioHardwareIllegalOperationError;
    case kAudioStreamPropertyIsActive:
        if (dataSize != sizeof(UInt32)) return kAudioHardwareBadPropertySizeError;
        atomic_store(&gStreamActive[Slot(object)], *(const UInt32 *)data != 0);
        return noErr;
    default: {
        if (dataSize != sizeof(AudioStreamBasicDescription)) return kAudioHardwareBadPropertySizeError;
        const AudioStreamBasicDescription *f = data;
        AudioStreamBasicDescription ours = Format();
        bool same = f->mFormatID == ours.mFormatID && f->mFormatFlags == ours.mFormatFlags
            && f->mChannelsPerFrame == ours.mChannelsPerFrame && f->mBitsPerChannel == ours.mBitsPerChannel
            && (f->mSampleRate == 0 || f->mSampleRate == ours.mSampleRate);
        return same ? noErr : kAudioDeviceUnsupportedFormatError;
    }
    }
}

#pragma mark IO

static OSStatus StartIO(AudioServerPlugInDriverRef driver, AudioObjectID device, UInt32 client) {
    (void)client;
    if (driver != gDriver || !IsDevice(device)) return kAudioHardwareBadObjectError;
    atomic_fetch_add(&gRunning[Slot(device)], 1);
    return noErr;
}

static OSStatus StopIO(AudioServerPlugInDriverRef driver, AudioObjectID device, UInt32 client) {
    (void)client;
    if (driver != gDriver || !IsDevice(device)) return kAudioHardwareBadObjectError;
    SInt32 running = atomic_load(&gRunning[Slot(device)]);
    while (running > 0 && !atomic_compare_exchange_weak(&gRunning[Slot(device)], &running, running - 1)) {}
    return noErr;
}

// One clock for both devices, from when the plug-in started: a frame written to the feed at a
// sample time is read from the microphone at that same sample time.
static OSStatus GetZeroTimeStamp(AudioServerPlugInDriverRef driver, AudioObjectID device, UInt32 client, Float64 *outSampleTime, UInt64 *outHostTime, UInt64 *outSeed) {
    (void)client;
    if (driver != gDriver || !IsDevice(device)) return kAudioHardwareBadObjectError;
    Float64 ticksPerPeriod = gTicksPerFrame * kRingFrames;
    UInt64 periods = (UInt64)((Float64)(mach_absolute_time() - gAnchorHostTime) / ticksPerPeriod);
    *outSampleTime = (Float64)(periods * kRingFrames);
    *outHostTime = gAnchorHostTime + (UInt64)((Float64)periods * ticksPerPeriod);
    *outSeed = 1;
    return noErr;
}

static OSStatus WillDoIOOperation(AudioServerPlugInDriverRef driver, AudioObjectID device, UInt32 client, UInt32 operation, Boolean *outWillDo, Boolean *outInPlace) {
    (void)client;
    if (driver != gDriver || !IsDevice(device)) return kAudioHardwareBadObjectError;
    bool willDo = (device == kObjectMic && operation == kAudioServerPlugInIOOperationReadInput)
        || (device == kObjectFeed && operation == kAudioServerPlugInIOOperationWriteMix);
    if (outWillDo) *outWillDo = willDo;
    if (outInPlace) *outInPlace = true;
    return noErr;
}

static OSStatus BeginIOOperation(AudioServerPlugInDriverRef driver, AudioObjectID device, UInt32 client, UInt32 operation, UInt32 frames, const AudioServerPlugInIOCycleInfo *cycle) {
    (void)client; (void)operation; (void)frames; (void)cycle;
    return driver == gDriver && IsDevice(device) ? noErr : kAudioHardwareBadObjectError;
}

static OSStatus DoIOOperation(AudioServerPlugInDriverRef driver, AudioObjectID device, AudioObjectID stream, UInt32 client, UInt32 operation, UInt32 frames, const AudioServerPlugInIOCycleInfo *cycle, void *mainBuffer, void *secondaryBuffer) {
    (void)client; (void)secondaryBuffer;
    if (driver != gDriver || !IsDevice(device) || stream != StreamOf(device)) return kAudioHardwareBadObjectError;
    if (!cycle || !mainBuffer) return noErr;
    if (device == kObjectMic && operation == kAudioServerPlugInIOOperationReadInput) {
        ReadRing(cycle->mInputTime.mSampleTime, mainBuffer, frames);
    } else if (device == kObjectFeed && operation == kAudioServerPlugInIOOperationWriteMix) {
        WriteRing(cycle->mOutputTime.mSampleTime, mainBuffer, frames);
    }
    return noErr;
}

static OSStatus EndIOOperation(AudioServerPlugInDriverRef driver, AudioObjectID device, UInt32 client, UInt32 operation, UInt32 frames, const AudioServerPlugInIOCycleInfo *cycle) {
    (void)client; (void)operation; (void)frames; (void)cycle;
    return driver == gDriver && IsDevice(device) ? noErr : kAudioHardwareBadObjectError;
}
