// Compile-time check that the Rust mirror in src/abi.rs matches the SDK:
// `clang -fsyntax-only abi_check.c`. tests/abi_consistency.rs checks that
// every four-character code in src/abi.rs has a SAME() line here.
#include <CoreAudio/AudioServerPlugIn.h>
// The HAL-synthesized device properties are declared only for clients.
#include <CoreAudio/AudioHardware.h>
#include <stddef.h>

#define SAME(a, b) _Static_assert((a) == (b), #a " != " #b)

// The UUID macros expand to CFUUIDGetConstantUUIDWithBytes(allocator, 16
// bytes). Redefining that name turns them into two 64-bit halves.
#undef CFUUIDGetConstantUUIDWithBytes
#define CFUUIDGetConstantUUIDWithBytes(a, b0, b1, b2, b3, b4, b5, b6, b7, b8, b9, b10, b11, b12,  \
                                       b13, b14, b15)                                             \
    ((unsigned long long)(b0) << 56 | (unsigned long long)(b1) << 48 |                            \
     (unsigned long long)(b2) << 40 | (unsigned long long)(b3) << 32 |                            \
     (unsigned long long)(b4) << 24 | (unsigned long long)(b5) << 16 |                            \
     (unsigned long long)(b6) << 8 | (unsigned long long)(b7)),                                   \
        ((unsigned long long)(b8) << 56 | (unsigned long long)(b9) << 48 |                        \
         (unsigned long long)(b10) << 40 | (unsigned long long)(b11) << 32 |                      \
         (unsigned long long)(b12) << 24 | (unsigned long long)(b13) << 16 |                      \
         (unsigned long long)(b14) << 8 | (unsigned long long)(b15))
#define UUID_HI_(hi, lo) hi
#define UUID_LO_(hi, lo) lo
#define UUID_HI(...) UUID_HI_(__VA_ARGS__)
#define UUID_LO(...) UUID_LO_(__VA_ARGS__)
#define SAME_UUID(u, hi, lo) SAME(UUID_HI(u), hi##ULL); SAME(UUID_LO(u), lo##ULL)

SAME_UUID(kAudioServerPlugInTypeUUID, 0x443ABAB8E7B3491A, 0xB985BEB9187030DB);
SAME_UUID(kAudioServerPlugInDriverInterfaceUUID, 0xEEA5773DCC4349F1, 0x8E008F96E7D23B17);
SAME_UUID(IUnknownUUID, 0x0000000000000000, 0xC000000000000046);

SAME(S_OK, 0);
SAME(E_NOINTERFACE, (HRESULT)0x80000004);
SAME(kCFStringEncodingUTF8, 0x08000100);

// Objects.
SAME(kAudioObjectUnknown, 0);
SAME(kAudioObjectPlugInObject, 1);

// Classes.
SAME(kAudioObjectClassID, 'aobj');
SAME(kAudioPlugInClassID, 'aplg');
SAME(kAudioDeviceClassID, 'adev');
SAME(kAudioStreamClassID, 'astr');

// Scopes and elements.
SAME(kAudioObjectPropertyScopeGlobal, 'glob');
SAME(kAudioObjectPropertyScopeInput, 'inpt');
SAME(kAudioObjectPropertyScopeOutput, 'outp');
SAME(kAudioObjectPropertyScopeWildcard, '****');
SAME(kAudioObjectPropertySelectorWildcard, '****');
SAME(kAudioObjectPropertyElementMain, 0);
SAME(kAudioObjectPropertyElementWildcard, 0xFFFFFFFF);

// Properties of every object.
SAME(kAudioObjectPropertyBaseClass, 'bcls');
SAME(kAudioObjectPropertyClass, 'clas');
SAME(kAudioObjectPropertyOwner, 'stdv');
SAME(kAudioObjectPropertyName, 'lnam');
SAME(kAudioObjectPropertyManufacturer, 'lmak');
SAME(kAudioObjectPropertyElementName, 'lchn');
SAME(kAudioObjectPropertyOwnedObjects, 'ownd');
SAME(kAudioObjectPropertyControlList, 'ctrl');
SAME(kAudioObjectPropertyCustomPropertyInfoList, 'cust');

// Plug-in properties.
SAME(kAudioPlugInPropertyBoxList, 'box#');
SAME(kAudioPlugInPropertyTranslateUIDToBox, 'uidb');
SAME(kAudioPlugInPropertyDeviceList, 'dev#');
SAME(kAudioPlugInPropertyTranslateUIDToDevice, 'uidd');
SAME(kAudioPlugInPropertyResourceBundle, 'rsrc');

// Device properties.
SAME(kAudioDevicePropertyDeviceUID, 'uid ');
SAME(kAudioDevicePropertyModelUID, 'muid');
SAME(kAudioDevicePropertyTransportType, 'tran');
SAME(kAudioDevicePropertyRelatedDevices, 'akin');
SAME(kAudioDevicePropertyClockDomain, 'clkd');
SAME(kAudioDevicePropertyDeviceIsAlive, 'livn');
SAME(kAudioDevicePropertyDeviceIsRunning, 'goin');
SAME(kAudioDevicePropertyDeviceCanBeDefaultDevice, 'dflt');
SAME(kAudioDevicePropertyDeviceCanBeDefaultSystemDevice, 'sflt');
SAME(kAudioDevicePropertyLatency, 'ltnc');
SAME(kAudioDevicePropertyStreams, 'stm#');
SAME(kAudioDevicePropertySafetyOffset, 'saft');
SAME(kAudioDevicePropertyNominalSampleRate, 'nsrt');
SAME(kAudioDevicePropertyAvailableNominalSampleRates, 'nsr#');
SAME(kAudioDevicePropertyIsHidden, 'hidn');
SAME(kAudioDevicePropertyPreferredChannelsForStereo, 'dch2');
SAME(kAudioDevicePropertyPreferredChannelLayout, 'srnd');
SAME(kAudioDevicePropertyZeroTimeStampPeriod, 'ring');
SAME(kAudioDevicePropertyClockAlgorithm, 'clok');
SAME(kAudioDevicePropertyClockIsStable, 'cstb');
// Declared from the macOS 26 SDK on (AudioHardware.h in 26.5); older SDKs
// lack them, and the driver uses them as literals.
#if defined(__MAC_26_0) && __MAC_OS_X_VERSION_MAX_ALLOWED >= __MAC_26_0
SAME(kAudioDevicePropertyWantsControlsRestored, 'resc');
SAME(kAudioDevicePropertyWantsStreamFormatsRestored, 'resf');
#endif
SAME(kAudioDevicePropertyDeviceIsRunningSomewhere, 'gone');
SAME(kAudioDevicePropertyHogMode, 'oink');
SAME(kAudioDevicePropertyBufferFrameSize, 'fsiz');
SAME(kAudioDevicePropertyBufferFrameSizeRange, 'fsz#');
SAME(kAudioDevicePropertyActualSampleRate, 'asrt');
SAME(kAudioDevicePropertyIOThreadOSWorkgroup, 'oswg');

// Device property values.
SAME(kAudioDeviceTransportTypeVirtual, 'virt');
SAME(kAudioDeviceClockAlgorithmRaw, 'raww');
SAME(kAudioDeviceClockAlgorithmSimpleIIR, 'iirf');
SAME(kAudioDeviceClockAlgorithm12PtMovingWindowAverage, 'mavg');

// Stream properties.
SAME(kAudioStreamPropertyIsActive, 'sact');
SAME(kAudioStreamPropertyDirection, 'sdir');
SAME(kAudioStreamPropertyTerminalType, 'term');
SAME(kAudioStreamPropertyStartingChannel, 'schn');
SAME(kAudioStreamPropertyLatency, 'ltnc');
SAME(kAudioStreamPropertyVirtualFormat, 'sfmt');
SAME(kAudioStreamPropertyAvailableVirtualFormats, 'sfma');
SAME(kAudioStreamPropertyPhysicalFormat, 'pft ');
SAME(kAudioStreamPropertyAvailablePhysicalFormats, 'pfta');
SAME(kAudioStreamTerminalTypeLine, 'line');

// Custom property data types.
SAME(kAudioServerPlugInCustomPropertyDataTypeNone, 0);
SAME(kAudioServerPlugInCustomPropertyDataTypeCFString, 'cfst');
SAME(kAudioServerPlugInCustomPropertyDataTypeCFPropertyList, 'plst');

// Formats and channel layouts.
SAME(kAudioFormatLinearPCM, 'lpcm');
SAME(kAudioFormatFlagIsFloat, 1);
SAME(kAudioFormatFlagIsBigEndian, 2);
SAME(kAudioFormatFlagIsPacked, 8);
SAME(kAudioFormatFlagIsNonInterleaved, 32);
SAME(kAudioFormatFlagsNativeFloatPacked, 9);
SAME(kAudioChannelLayoutTag_UseChannelDescriptions, 0);
SAME(kAudioChannelLabel_Discrete_0, 0x10000);
SAME(kAudioChannelLabel_Unknown, 0xFFFFFFFF);

// IO operations.
SAME(kAudioServerPlugInIOOperationThread, 'thrd');
SAME(kAudioServerPlugInIOOperationCycle, 'cycl');
SAME(kAudioServerPlugInIOOperationReadInput, 'read');
SAME(kAudioServerPlugInIOOperationConvertInput, 'cinp');
SAME(kAudioServerPlugInIOOperationProcessInput, 'pinp');
SAME(kAudioServerPlugInIOOperationProcessOutput, 'pout');
SAME(kAudioServerPlugInIOOperationMixOutput, 'mixo');
SAME(kAudioServerPlugInIOOperationProcessMix, 'pmix');
SAME(kAudioServerPlugInIOOperationConvertMix, 'cmix');
SAME(kAudioServerPlugInIOOperationWriteMix, 'rite');

// Errors.
SAME(kAudioHardwareNoError, 0);
SAME(kAudioHardwareNotRunningError, 'stop');
SAME(kAudioHardwareUnspecifiedError, 'what');
SAME(kAudioHardwareUnknownPropertyError, 'who?');
SAME(kAudioHardwareBadPropertySizeError, '!siz');
SAME(kAudioHardwareIllegalOperationError, 'nope');
SAME(kAudioHardwareBadObjectError, '!obj');
SAME(kAudioHardwareBadDeviceError, '!dev');
SAME(kAudioHardwareBadStreamError, '!str');
SAME(kAudioHardwareUnsupportedOperationError, 'unop');
SAME(kAudioHardwareNotReadyError, 'nrdy');
SAME(kAudioDeviceUnsupportedFormatError, '!dat');
SAME(kAudioDevicePermissionsError, '!hog');

// Sizes and offsets.
SAME(sizeof(CFUUIDBytes), 16);
SAME(sizeof(pid_t), 4);
SAME(sizeof(AudioObjectPropertyAddress), 12);
SAME(offsetof(AudioObjectPropertyAddress, mScope), 4);
SAME(offsetof(AudioObjectPropertyAddress, mElement), 8);

SAME(offsetof(AudioServerPlugInClientInfo, mProcessID), 4);
SAME(offsetof(AudioServerPlugInClientInfo, mIsNativeEndian), 8);
SAME(offsetof(AudioServerPlugInClientInfo, mBundleID), 8 + sizeof(void *));
SAME(sizeof(AudioServerPlugInClientInfo), 8 + 2 * sizeof(void *));

SAME(sizeof(SMPTETime), 24);
SAME(offsetof(SMPTETime, mCounter), 4);
SAME(offsetof(SMPTETime, mType), 8);
SAME(offsetof(SMPTETime, mFlags), 12);
SAME(offsetof(SMPTETime, mHours), 16);
SAME(offsetof(SMPTETime, mFrames), 22);
SAME(sizeof(AudioTimeStamp), 64);
SAME(offsetof(AudioTimeStamp, mHostTime), 8);
SAME(offsetof(AudioTimeStamp, mRateScalar), 16);
SAME(offsetof(AudioTimeStamp, mWordClockTime), 24);
SAME(offsetof(AudioTimeStamp, mSMPTETime), 32);
SAME(offsetof(AudioTimeStamp, mFlags), 56);
SAME(offsetof(AudioTimeStamp, mReserved), 60);

SAME(offsetof(AudioServerPlugInIOCycleInfo, mNominalIOBufferFrameSize), 8);
SAME(offsetof(AudioServerPlugInIOCycleInfo, mCurrentTime), 16);
SAME(offsetof(AudioServerPlugInIOCycleInfo, mInputTime), 80);
SAME(offsetof(AudioServerPlugInIOCycleInfo, mOutputTime), 144);
SAME(offsetof(AudioServerPlugInIOCycleInfo, mMainHostTicksPerFrame), 208);
SAME(offsetof(AudioServerPlugInIOCycleInfo, mDeviceHostTicksPerFrame), 216);
SAME(sizeof(AudioServerPlugInIOCycleInfo), 224);

SAME(sizeof(AudioStreamBasicDescription), 40);
SAME(offsetof(AudioStreamBasicDescription, mFormatID), 8);
SAME(offsetof(AudioStreamBasicDescription, mFormatFlags), 12);
SAME(offsetof(AudioStreamBasicDescription, mBytesPerPacket), 16);
SAME(offsetof(AudioStreamBasicDescription, mFramesPerPacket), 20);
SAME(offsetof(AudioStreamBasicDescription, mBytesPerFrame), 24);
SAME(offsetof(AudioStreamBasicDescription, mChannelsPerFrame), 28);
SAME(offsetof(AudioStreamBasicDescription, mBitsPerChannel), 32);
SAME(offsetof(AudioStreamBasicDescription, mReserved), 36);
SAME(sizeof(AudioValueRange), 16);
SAME(offsetof(AudioValueRange, mMaximum), 8);
SAME(sizeof(AudioStreamRangedDescription), 56);
SAME(offsetof(AudioStreamRangedDescription, mSampleRateRange), 40);

// AudioChannelLayout: a 12-byte header, then the descriptions.
SAME(offsetof(AudioChannelLayout, mChannelBitmap), 4);
SAME(offsetof(AudioChannelLayout, mNumberChannelDescriptions), 8);
SAME(offsetof(AudioChannelLayout, mChannelDescriptions), 12);
SAME(sizeof(AudioChannelDescription), 20);
SAME(offsetof(AudioChannelDescription, mChannelFlags), 4);
SAME(offsetof(AudioChannelDescription, mCoordinates), 8);

SAME(sizeof(AudioServerPlugInCustomPropertyInfo), 12);
SAME(offsetof(AudioServerPlugInCustomPropertyInfo, mPropertyDataType), 4);
SAME(offsetof(AudioServerPlugInCustomPropertyInfo, mQualifierDataType), 8);

// The host interface: 5 function pointers in this order.
#define HOST_SLOT(field, n) \
    SAME(offsetof(AudioServerPlugInHostInterface, field), (n) * sizeof(void *))
HOST_SLOT(PropertiesChanged, 0);
HOST_SLOT(CopyFromStorage, 1);
HOST_SLOT(WriteToStorage, 2);
HOST_SLOT(DeleteFromStorage, 3);
HOST_SLOT(RequestDeviceConfigurationChange, 4);
SAME(sizeof(AudioServerPlugInHostInterface), 5 * sizeof(void *));

// The driver interface: 23 pointer-sized slots, the reserved one and 22
// function pointers.
#define SLOT(field, n) SAME(offsetof(AudioServerPlugInDriverInterface, field), (n) * sizeof(void *))
SLOT(_reserved, 0);
SLOT(QueryInterface, 1);
SLOT(AddRef, 2);
SLOT(Release, 3);
SLOT(Initialize, 4);
SLOT(CreateDevice, 5);
SLOT(DestroyDevice, 6);
SLOT(AddDeviceClient, 7);
SLOT(RemoveDeviceClient, 8);
SLOT(PerformDeviceConfigurationChange, 9);
SLOT(AbortDeviceConfigurationChange, 10);
SLOT(HasProperty, 11);
SLOT(IsPropertySettable, 12);
SLOT(GetPropertyDataSize, 13);
SLOT(GetPropertyData, 14);
SLOT(SetPropertyData, 15);
SLOT(StartIO, 16);
SLOT(StopIO, 17);
SLOT(GetZeroTimeStamp, 18);
SLOT(WillDoIOOperation, 19);
SLOT(BeginIOOperation, 20);
SLOT(DoIOOperation, 21);
SLOT(EndIOOperation, 22);
SAME(sizeof(AudioServerPlugInDriverInterface), 23 * sizeof(void *));
