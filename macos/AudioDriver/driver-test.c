// Drives a built LanKVMMicrophone.driver the way coreaudiod does, without installing it: loads the
// bundle as a CFPlugIn, finds the Audio Server plug-in factory its Info.plist names, and checks the
// objects and properties apps see, and audio written to the feed coming out of the microphone.
// Built and run by scripts/build-audio-driver.sh: driver-test path/to/LanKVMMicrophone.driver

#include <CoreAudio/AudioServerPlugIn.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static int failures;

#define CHECK(cond, ...)                                                                        \
    do {                                                                                        \
        if (!(cond)) {                                                                          \
            failures++;                                                                         \
            fprintf(stderr, "FAIL %s:%d: %s: ", __FILE__, __LINE__, #cond);                     \
            fprintf(stderr, __VA_ARGS__);                                                       \
            fprintf(stderr, "\n");                                                              \
        }                                                                                       \
    } while (0)

static AudioServerPlugInDriverRef driver;

static AudioObjectPropertyAddress Address(AudioObjectPropertySelector selector, AudioObjectPropertyScope scope) {
    return (AudioObjectPropertyAddress){selector, scope, kAudioObjectPropertyElementMain};
}

static OSStatus Get(AudioObjectID object, AudioObjectPropertySelector selector, AudioObjectPropertyScope scope,
                    UInt32 qualifierSize, const void *qualifier, UInt32 capacity, void *out, UInt32 *outSize) {
    AudioObjectPropertyAddress a = Address(selector, scope);
    UInt32 size = 0;
    OSStatus status = (*driver)->GetPropertyDataSize(driver, object, 0, &a, qualifierSize, qualifier, &size);
    if (status != noErr) return status;
    UInt32 got = 0;
    status = (*driver)->GetPropertyData(driver, object, 0, &a, qualifierSize, qualifier, capacity, &got, out);
    if (status == noErr) {
        CHECK(got == size || got < size, "object %u '%.4s': data size %u, said %u", object, (char *)&selector, got, size);
        if (outSize) *outSize = got;
    }
    return status;
}

static UInt32 GetU32(AudioObjectID object, AudioObjectPropertySelector selector, AudioObjectPropertyScope scope) {
    UInt32 value = 0xdeadbeef, size = 0;
    OSStatus status = Get(object, selector, scope, 0, NULL, sizeof(value), &value, &size);
    CHECK(status == noErr && size == sizeof(value), "object %u selector %u: status %d size %u", object, selector, (int)status, size);
    return value;
}

static bool StringIs(AudioObjectID object, AudioObjectPropertySelector selector, const char *expected) {
    CFStringRef value = NULL;
    UInt32 size = 0;
    if (Get(object, selector, kAudioObjectPropertyScopeGlobal, 0, NULL, sizeof(value), &value, &size) != noErr || !value) return false;
    char text[256] = {0};
    CFStringGetCString(value, text, sizeof(text), kCFStringEncodingUTF8);
    CFRelease(value);
    if (strcmp(text, expected) != 0) fprintf(stderr, "  object %u: \"%s\", expected \"%s\"\n", object, text, expected);
    return strcmp(text, expected) == 0;
}

static AudioObjectID Translate(const char *uid) {
    CFStringRef string = CFStringCreateWithCString(NULL, uid, kCFStringEncodingUTF8);
    AudioObjectID device = 12345;
    UInt32 size = 0;
    OSStatus status = Get(kAudioObjectPlugInObject, kAudioPlugInPropertyTranslateUIDToDevice, kAudioObjectPropertyScopeGlobal,
                          sizeof(CFStringRef), &string, sizeof(device), &device, &size);
    CFRelease(string);
    CHECK(status == noErr, "translate %s: %d", uid, (int)status);
    return device;
}

static UInt32 Ids(AudioObjectID object, AudioObjectPropertySelector selector, AudioObjectPropertyScope scope, AudioObjectID *ids, UInt32 max) {
    UInt32 size = 0;
    OSStatus status = Get(object, selector, scope, 0, NULL, max * sizeof(AudioObjectID), ids, &size);
    CHECK(status == noErr, "object %u selector %u: %d", object, selector, (int)status);
    return size / sizeof(AudioObjectID);
}

// The host's side of the interface: nothing changes properties or asks for storage.
static OSStatus PropertiesChanged(AudioServerPlugInHostRef host, AudioObjectID object, UInt32 count, const AudioObjectPropertyAddress *addresses) {
    (void)host; (void)object; (void)count; (void)addresses;
    return noErr;
}
static OSStatus CopyFromStorage(AudioServerPlugInHostRef host, CFStringRef key, CFPropertyListRef *out) {
    (void)host; (void)key;
    *out = NULL;
    return noErr;
}
static OSStatus WriteToStorage(AudioServerPlugInHostRef host, CFStringRef key, CFPropertyListRef data) {
    (void)host; (void)key; (void)data;
    return noErr;
}
static OSStatus DeleteFromStorage(AudioServerPlugInHostRef host, CFStringRef key) {
    (void)host; (void)key;
    return noErr;
}
static OSStatus RequestDeviceConfigurationChange(AudioServerPlugInHostRef host, AudioObjectID device, UInt64 action, void *info) {
    (void)host; (void)device; (void)action; (void)info;
    return noErr;
}
static AudioServerPlugInHostInterface host = {PropertiesChanged, CopyFromStorage, WriteToStorage, DeleteFromStorage, RequestDeviceConfigurationChange};

static void Cycle(AudioObjectID device, AudioObjectID stream, UInt32 operation, Float64 sampleTime, float *buffer, UInt32 frames) {
    AudioServerPlugInIOCycleInfo cycle = {0};
    cycle.mNominalIOBufferFrameSize = frames;
    cycle.mInputTime.mSampleTime = sampleTime;
    cycle.mOutputTime.mSampleTime = sampleTime;
    Boolean willDo = false, inPlace = false;
    CHECK((*driver)->WillDoIOOperation(driver, device, 1, operation, &willDo, &inPlace) == noErr && willDo && inPlace,
          "device %u does '%.4s'", device, (char *)&operation);
    CHECK((*driver)->BeginIOOperation(driver, device, 1, operation, frames, &cycle) == noErr, "begin");
    CHECK((*driver)->DoIOOperation(driver, device, stream, 1, operation, frames, &cycle, buffer, NULL) == noErr, "do");
    CHECK((*driver)->EndIOOperation(driver, device, 1, operation, frames, &cycle) == noErr, "end");
}

// What coreaudiod does with a bundle in /Library/Audio/Plug-Ins/HAL.
static AudioServerPlugInDriverRef Load(const char *path) {
    CFURLRef url = CFURLCreateFromFileSystemRepresentation(NULL, (const UInt8 *)path, (CFIndex)strlen(path), true);
    CFPlugInRef plugIn = url ? CFPlugInCreate(NULL, url) : NULL;
    if (url) CFRelease(url);
    CHECK(plugIn != NULL, "load %s as a CFPlugIn", path);
    if (!plugIn) return NULL;
    CFArrayRef factories = CFPlugInFindFactoriesForPlugInTypeInPlugIn(kAudioServerPlugInTypeUUID, plugIn);
    CHECK(factories && CFArrayGetCount(factories) == 1, "one Audio Server plug-in factory");
    if (!factories || CFArrayGetCount(factories) == 0) return NULL;
    CFUUIDRef factory = CFArrayGetValueAtIndex(factories, 0);
    IUnknownVTbl **unknown = CFPlugInInstanceCreate(NULL, factory, kAudioServerPlugInTypeUUID);
    CFRelease(factories);
    CHECK(unknown != NULL, "the factory makes the driver");
    if (!unknown) return NULL;
    AudioServerPlugInDriverRef found = NULL;
    CFUUIDBytes bytes = CFUUIDGetUUIDBytes(kAudioServerPlugInDriverInterfaceUUID);
    CHECK((*unknown)->QueryInterface(unknown, bytes, (void **)&found) == S_OK && found != NULL, "query the driver interface");
    (*unknown)->Release(unknown);
    return found;
}

int main(int argc, char **argv) {
    if (argc != 2) {
        fprintf(stderr, "usage: %s path/to/LanKVMMicrophone.driver\n", argv[0]);
        return 2;
    }
    driver = Load(argv[1]);
    if (!driver) return 1;
    CHECK((*driver)->Initialize(driver, &host) == noErr, "initialize");

    // The plug-in and its two devices.
    CHECK(GetU32(kAudioObjectPlugInObject, kAudioObjectPropertyClass, kAudioObjectPropertyScopeGlobal) == kAudioPlugInClassID, "plug-in class");
    AudioObjectID devices[4] = {0};
    CHECK(Ids(kAudioObjectPlugInObject, kAudioPlugInPropertyDeviceList, kAudioObjectPropertyScopeGlobal, devices, 4) == 2, "two devices");
    AudioObjectID mic = Translate("dev.lankvm.microphone"), feed = Translate("dev.lankvm.microphone.feed");
    CHECK(mic != kAudioObjectUnknown && feed != kAudioObjectUnknown && mic != feed, "both UIDs translate: %u %u", mic, feed);
    CHECK(Translate("something.else") == kAudioObjectUnknown, "unknown UIDs don't");
    CHECK((devices[0] == mic && devices[1] == feed) || (devices[0] == feed && devices[1] == mic), "the list has both");
    CHECK(Ids(kAudioObjectPlugInObject, kAudioPlugInPropertyDeviceList, kAudioObjectPropertyScopeGlobal, devices, 1) == 1,
          "a list is cut to the buffer");

    CHECK(StringIs(mic, kAudioObjectPropertyName, "LanKVM Microphone"), "mic name");
    CHECK(StringIs(feed, kAudioObjectPropertyName, "LanKVM Microphone Feed"), "feed name");
    CHECK(StringIs(mic, kAudioDevicePropertyDeviceUID, "dev.lankvm.microphone"), "mic UID");
    CHECK(StringIs(feed, kAudioDevicePropertyDeviceUID, "dev.lankvm.microphone.feed"), "feed UID");
    CHECK(GetU32(mic, kAudioObjectPropertyClass, kAudioObjectPropertyScopeGlobal) == kAudioDeviceClassID, "device class");
    CHECK(GetU32(mic, kAudioObjectPropertyOwner, kAudioObjectPropertyScopeGlobal) == kAudioObjectPlugInObject, "owned by the plug-in");
    CHECK(GetU32(mic, kAudioDevicePropertyIsHidden, kAudioObjectPropertyScopeGlobal) == 0, "apps see the microphone");
    CHECK(GetU32(feed, kAudioDevicePropertyIsHidden, kAudioObjectPropertyScopeGlobal) == 1, "nobody sees the feed");
    CHECK(GetU32(mic, kAudioDevicePropertyDeviceCanBeDefaultDevice, kAudioObjectPropertyScopeInput) == 1, "the mic can be the default input");
    CHECK(GetU32(feed, kAudioDevicePropertyDeviceCanBeDefaultDevice, kAudioObjectPropertyScopeOutput) == 0, "the feed is never a default");
    CHECK(GetU32(mic, kAudioDevicePropertyDeviceCanBeDefaultSystemDevice, kAudioObjectPropertyScopeGlobal) == 0, "never the system device");
    CHECK(GetU32(mic, kAudioDevicePropertyClockDomain, kAudioObjectPropertyScopeGlobal)
              == GetU32(feed, kAudioDevicePropertyClockDomain, kAudioObjectPropertyScopeGlobal), "one clock");
    CHECK(GetU32(mic, kAudioDevicePropertyTransportType, kAudioObjectPropertyScopeGlobal) == kAudioDeviceTransportTypeVirtual, "virtual");
    CHECK(GetU32(mic, kAudioDevicePropertyDeviceIsAlive, kAudioObjectPropertyScopeGlobal) == 1, "alive");
    Float64 rate = 0;
    CHECK(Get(mic, kAudioDevicePropertyNominalSampleRate, kAudioObjectPropertyScopeGlobal, 0, NULL, sizeof(rate), &rate, NULL) == noErr
              && rate == 48000, "48 kHz: %f", rate);

    // Streams: one input on the mic, one output on the feed.
    AudioObjectID streams[2] = {0};
    CHECK(Ids(mic, kAudioDevicePropertyStreams, kAudioObjectPropertyScopeInput, streams, 2) == 1, "the mic has an input stream");
    AudioObjectID micStream = streams[0];
    CHECK(Ids(mic, kAudioDevicePropertyStreams, kAudioObjectPropertyScopeOutput, streams, 2) == 0, "and no output");
    CHECK(Ids(feed, kAudioDevicePropertyStreams, kAudioObjectPropertyScopeOutput, streams, 2) == 1, "the feed has an output stream");
    AudioObjectID feedStream = streams[0];
    CHECK(Ids(feed, kAudioDevicePropertyStreams, kAudioObjectPropertyScopeInput, streams, 2) == 0, "and no input");
    CHECK(GetU32(micStream, kAudioStreamPropertyDirection, kAudioObjectPropertyScopeGlobal) == 1, "input");
    CHECK(GetU32(feedStream, kAudioStreamPropertyDirection, kAudioObjectPropertyScopeGlobal) == 0, "output");
    CHECK(GetU32(micStream, kAudioStreamPropertyTerminalType, kAudioObjectPropertyScopeGlobal) == kAudioStreamTerminalTypeMicrophone, "a microphone");
    CHECK(GetU32(micStream, kAudioObjectPropertyOwner, kAudioObjectPropertyScopeGlobal) == mic, "the mic's");
    AudioStreamBasicDescription format = {0};
    CHECK(Get(micStream, kAudioStreamPropertyVirtualFormat, kAudioObjectPropertyScopeGlobal, 0, NULL, sizeof(format), &format, NULL) == noErr,
          "virtual format");
    CHECK(format.mSampleRate == 48000 && format.mChannelsPerFrame == 1 && format.mBitsPerChannel == 32
              && (format.mFormatFlags & kAudioFormatFlagIsFloat) && format.mBytesPerFrame == 4, "48 kHz mono float");
    AudioStreamRangedDescription ranged[2];
    UInt32 size = 0;
    CHECK(Get(feedStream, kAudioStreamPropertyAvailablePhysicalFormats, kAudioObjectPropertyScopeGlobal, 0, NULL, sizeof(ranged), ranged, &size)
              == noErr && size == sizeof(AudioStreamRangedDescription), "one format");
    UInt32 tooSmall = 0;
    AudioObjectPropertyAddress formatAddress = Address(kAudioStreamPropertyVirtualFormat, kAudioObjectPropertyScopeGlobal);
    CHECK((*driver)->GetPropertyData(driver, micStream, 0, &formatAddress, 0, NULL, 4, &size, &tooSmall) == kAudioHardwareBadPropertySizeError,
          "a value that doesn't fit is refused");

    // Settings: only what they are.
    AudioObjectPropertyAddress rateAddress = Address(kAudioDevicePropertyNominalSampleRate, kAudioObjectPropertyScopeGlobal);
    Boolean settable = false;
    CHECK((*driver)->IsPropertySettable(driver, mic, 0, &rateAddress, &settable) == noErr && settable, "the rate is settable");
    Float64 same = 48000, other = 44100;
    CHECK((*driver)->SetPropertyData(driver, mic, 0, &rateAddress, 0, NULL, sizeof(same), &same) == noErr, "to 48 kHz");
    CHECK((*driver)->SetPropertyData(driver, mic, 0, &rateAddress, 0, NULL, sizeof(other), &other) != noErr, "not to 44.1");
    CHECK((*driver)->SetPropertyData(driver, micStream, 0, &formatAddress, 0, NULL, sizeof(format), &format) == noErr, "its own format");
    AudioObjectPropertyAddress nameAddress = Address(kAudioObjectPropertyName, kAudioObjectPropertyScopeGlobal);
    CHECK((*driver)->IsPropertySettable(driver, mic, 0, &nameAddress, &settable) == noErr && !settable, "the name isn't");
    AudioObjectPropertyAddress unknown = Address('nope', kAudioObjectPropertyScopeGlobal);
    CHECK(!(*driver)->HasProperty(driver, mic, 0, &unknown), "unknown properties");
    CHECK((*driver)->HasProperty(driver, mic, 0, &nameAddress), "known ones");
    CHECK(!(*driver)->HasProperty(driver, 99, 0, &nameAddress), "unknown objects");

    // IO: the clock, then audio through the ring.
    CHECK((*driver)->StartIO(driver, mic, 1) == noErr && (*driver)->StartIO(driver, feed, 1) == noErr, "start");
    CHECK(GetU32(mic, kAudioDevicePropertyDeviceIsRunning, kAudioObjectPropertyScopeGlobal) == 1, "running");
    Float64 micTime = -1, feedTime = -2;
    UInt64 micHost = 0, feedHost = 1, seed = 0;
    CHECK((*driver)->GetZeroTimeStamp(driver, mic, 1, &micTime, &micHost, &seed) == noErr, "mic time stamp");
    CHECK((*driver)->GetZeroTimeStamp(driver, feed, 1, &feedTime, &feedHost, &seed) == noErr, "feed time stamp");
    CHECK(micTime == feedTime && micHost == feedHost, "one timeline: %f/%llu vs %f/%llu", micTime, micHost, feedTime, feedHost);
    CHECK((UInt64)micTime % 16384 == 0, "a whole period");

    enum { kFrames = 512 };
    float in[kFrames], out[kFrames];
    for (int i = 0; i < kFrames; i++) in[i] = (float)i / kFrames - 0.5f;
    Float64 t = 100000;
    Cycle(feed, feedStream, kAudioServerPlugInIOOperationWriteMix, t, in, kFrames);
    memset(out, 0x7f, sizeof(out));
    Cycle(mic, micStream, kAudioServerPlugInIOOperationReadInput, t, out, kFrames);
    CHECK(memcmp(in, out, sizeof(in)) == 0, "the mic plays what the feed got, at the same time");
    Cycle(mic, micStream, kAudioServerPlugInIOOperationReadInput, t + 256, out, kFrames);
    CHECK(memcmp(in + 256, out, 256 * sizeof(float)) == 0, "from anywhere in it");
    bool silent = true;
    for (int i = 256; i < kFrames; i++) silent = silent && out[i] == 0;
    CHECK(silent, "and silence where nothing was written");
    Cycle(mic, micStream, kAudioServerPlugInIOOperationReadInput, t + 16384, out, kFrames);
    silent = true;
    for (int i = 0; i < kFrames; i++) silent = silent && out[i] == 0;
    CHECK(silent, "a lap later the old audio is gone");
    Boolean willDo = true, inPlace = false;
    CHECK((*driver)->WillDoIOOperation(driver, mic, 1, kAudioServerPlugInIOOperationWriteMix, &willDo, &inPlace) == noErr && !willDo,
          "the mic takes no output");
    CHECK((*driver)->WillDoIOOperation(driver, feed, 1, kAudioServerPlugInIOOperationReadInput, &willDo, &inPlace) == noErr && !willDo,
          "the feed gives no input");
    CHECK((*driver)->StopIO(driver, mic, 1) == noErr && (*driver)->StopIO(driver, feed, 1) == noErr, "stop");
    CHECK(GetU32(mic, kAudioDevicePropertyDeviceIsRunning, kAudioObjectPropertyScopeGlobal) == 0, "stopped");
    CHECK((*driver)->StopIO(driver, mic, 1) == noErr && GetU32(mic, kAudioDevicePropertyDeviceIsRunning, kAudioObjectPropertyScopeGlobal) == 0,
          "an extra stop doesn't go negative");

    if (failures) {
        fprintf(stderr, "%d check(s) failed\n", failures);
        return 1;
    }
    printf("LanKVM Microphone driver: all checks passed\n");
    return 0;
}
