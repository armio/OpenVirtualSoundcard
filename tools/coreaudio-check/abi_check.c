// Compile-time check that src/ca.rs matches the SDK:
// `clang -fsyntax-only abi_check.c`.
#include <CoreAudio/CoreAudio.h>
// Some device properties are declared only for drivers.
#include <CoreAudio/AudioServerPlugIn.h>
#include <stddef.h>

#define SAME(a, b) _Static_assert((a) == (b), #a " != " #b)

SAME(kAudioObjectSystemObject, 1);
SAME(kAudioHardwarePropertyDevices, 'dev#');
SAME(kAudioHardwarePropertyTranslateUIDToDevice, 'uidd');
SAME(kAudioObjectPropertyName, 'lnam');
SAME(kAudioObjectPropertyManufacturer, 'lmak');
SAME(kAudioDevicePropertyDeviceUID, 'uid ');
SAME(kAudioDevicePropertyModelUID, 'muid');
SAME(kAudioDevicePropertyNominalSampleRate, 'nsrt');
SAME(kAudioDevicePropertyActualSampleRate, 'asrt');
SAME(kAudioDevicePropertyAvailableNominalSampleRates, 'nsr#');
SAME(kAudioDevicePropertyStreamConfiguration, 'slay');
SAME(kAudioDevicePropertyBufferFrameSize, 'fsiz');
SAME(kAudioDevicePropertyLatency, 'ltnc');
SAME(kAudioDevicePropertySafetyOffset, 'saft');
SAME(kAudioDevicePropertyDeviceIsAlive, 'livn');
SAME(kAudioDevicePropertyDeviceIsRunningSomewhere, 'gone');
SAME(kAudioDevicePropertyTransportType, 'tran');
SAME(kAudioDevicePropertyClockDomain, 'clkd');
SAME(kAudioDevicePropertyZeroTimeStampPeriod, 'ring');
SAME(kAudioDevicePropertyClockAlgorithm, 'clok');
SAME(kAudioDevicePropertyIsHidden, 'hidn');
SAME(kAudioDevicePropertyStreams, 'stm#');
SAME(kAudioStreamPropertyVirtualFormat, 'sfmt');
SAME(kAudioDevicePropertyBufferFrameSizeRange, 'fsz#');
SAME(kAudioObjectPropertyElementName, 'lchn');
SAME(kAudioObjectPropertyOwnedObjects, 'ownd');
SAME(kAudioObjectPropertyBaseClass, 'bcls');
SAME(kAudioObjectPropertyClass, 'clas');
SAME(kAudioObjectPropertyOwner, 'stdv');
SAME(kAudioObjectPropertyControlList, 'ctrl');
SAME(kAudioObjectPropertyCustomPropertyInfoList, 'cust');
SAME(kAudioDevicePropertyDeviceIsRunning, 'goin');
SAME(kAudioDevicePropertyRelatedDevices, 'akin');
SAME(kAudioDevicePropertyClockIsStable, 'cstb');
SAME(kAudioDevicePropertyPreferredChannelsForStereo, 'dch2');
SAME(kAudioDevicePropertyPreferredChannelLayout, 'srnd');
SAME(kAudioDevicePropertyDeviceCanBeDefaultDevice, 'dflt');
SAME(kAudioDevicePropertyDeviceCanBeDefaultSystemDevice, 'sflt');
SAME(kAudioStreamPropertyPhysicalFormat, 'pft ');
SAME(kAudioStreamPropertyAvailableVirtualFormats, 'sfma');
SAME(kAudioStreamPropertyAvailablePhysicalFormats, 'pfta');
SAME(kAudioStreamPropertyDirection, 'sdir');
SAME(kAudioStreamPropertyTerminalType, 'term');
SAME(kAudioStreamPropertyStartingChannel, 'schn');
SAME(kAudioStreamPropertyIsActive, 'sact');
SAME(kAudioObjectPropertyScopeGlobal, 'glob');
SAME(kAudioObjectPropertyScopeInput, 'inpt');
SAME(kAudioObjectPropertyScopeOutput, 'outp');
SAME(kAudioObjectPropertyElementMain, 0);
SAME(kAudioFormatLinearPCM, 'lpcm');
SAME(kAudioFormatFlagIsFloat, 1);
SAME(kAudioTimeStampSampleTimeValid, 1);
SAME(kAudioTimeStampHostTimeValid, 2);
SAME(kCFStringEncodingUTF8, 0x08000100);

SAME(sizeof(AudioObjectPropertyAddress), 12);
SAME(sizeof(AudioTimeStamp), 64);
SAME(offsetof(AudioTimeStamp, mFlags), 56);
SAME(sizeof(AudioBuffer), 16);
SAME(offsetof(AudioBufferList, mBuffers), 8);
SAME(sizeof(AudioStreamBasicDescription), 40);
SAME(sizeof(AudioValueRange), 16);
