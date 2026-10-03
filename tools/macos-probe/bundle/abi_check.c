// Compile-time check that the Rust mirror in plugin/src/abi.rs matches the
// SDK: `clang -fsyntax-only bundle/abi_check.c`.
#include <CoreAudio/AudioServerPlugIn.h>
#include <stddef.h>

#define SAME(a, b) _Static_assert((a) == (b), #a " != " #b)

SAME(kAudioObjectPlugInObject, 1);
SAME(kAudioObjectUnknown, 0);
SAME(kAudioObjectClassID, 'aobj');
SAME(kAudioPlugInClassID, 'aplg');
SAME(kAudioObjectPropertyBaseClass, 'bcls');
SAME(kAudioObjectPropertyClass, 'clas');
SAME(kAudioObjectPropertyOwner, 'stdv');
SAME(kAudioObjectPropertyManufacturer, 'lmak');
SAME(kAudioObjectPropertyOwnedObjects, 'ownd');
SAME(kAudioPlugInPropertyBoxList, 'box#');
SAME(kAudioPlugInPropertyTranslateUIDToBox, 'uidb');
SAME(kAudioPlugInPropertyDeviceList, 'dev#');
SAME(kAudioPlugInPropertyTranslateUIDToDevice, 'uidd');
SAME(kAudioPlugInPropertyResourceBundle, 'rsrc');
SAME(kAudioHardwareUnknownPropertyError, 'who?');
SAME(kAudioHardwareBadPropertySizeError, '!siz');
SAME(kAudioHardwareIllegalOperationError, 'nope');
SAME(kAudioHardwareBadObjectError, '!obj');
SAME(kAudioHardwareUnsupportedOperationError, 'unop');
SAME(kCFStringEncodingUTF8, 0x08000100);
SAME(E_NOINTERFACE, (HRESULT)0x80000004);

SAME(sizeof(AudioObjectPropertyAddress), 12);
SAME(sizeof(SMPTETime), 24);
SAME(sizeof(AudioTimeStamp), 64);
SAME(offsetof(AudioTimeStamp, mSMPTETime), 32);
SAME(offsetof(AudioTimeStamp, mFlags), 56);
SAME(offsetof(AudioServerPlugInClientInfo, mIsNativeEndian), 8);
SAME(offsetof(AudioServerPlugInClientInfo, mBundleID), 16);
SAME(sizeof(AudioServerPlugInClientInfo), 24);
SAME(offsetof(AudioServerPlugInIOCycleInfo, mCurrentTime), 16);
SAME(offsetof(AudioServerPlugInIOCycleInfo, mInputTime), 80);
SAME(offsetof(AudioServerPlugInIOCycleInfo, mOutputTime), 144);
SAME(offsetof(AudioServerPlugInIOCycleInfo, mMainHostTicksPerFrame), 208);
SAME(offsetof(AudioServerPlugInIOCycleInfo, mDeviceHostTicksPerFrame), 216);
SAME(sizeof(AudioServerPlugInIOCycleInfo), 224);

// 23 function pointers after the reserved pointer, in this order.
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
