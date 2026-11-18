//! The [`Hal`] over Core Audio: a private, PID-scoped, stereo-mixdown process tap that mutes
//! the app on the Mac only while it is read, a private tap-only aggregate device carrying it,
//! and a C-function IOProc reading that (AudioHardwareTapping.h, CATapDescription.h,
//! AudioHardware.h). Every method blocks on coreaudiod and runs only on the control thread.

use std::ffi::{CStr, c_void};
use std::mem::{MaybeUninit, size_of};
use std::ptr::{self, NonNull};
use std::sync::mpsc::Sender;

use block2::RcBlock;
use objc2::runtime::NSObjectProtocol;
use objc2::{AllocAnyThread, sel};
use objc2_core_audio::{
    AudioDeviceCreateIOProcID, AudioDeviceDestroyIOProcID, AudioDeviceIOProcID, AudioDeviceStart,
    AudioDeviceStop, AudioHardwareCreateAggregateDevice, AudioHardwareCreateProcessTap,
    AudioHardwareDestroyAggregateDevice, AudioHardwareDestroyProcessTap,
    AudioObjectAddPropertyListenerBlock, AudioObjectGetPropertyData, AudioObjectID,
    AudioObjectPropertyAddress, AudioObjectRemovePropertyListenerBlock, CATapDescription,
    CATapMuteBehavior, kAudioAggregateDeviceIsPrivateKey, kAudioAggregateDeviceNameKey,
    kAudioAggregateDeviceTapAutoStartKey, kAudioAggregateDeviceTapListKey,
    kAudioAggregateDeviceUIDKey, kAudioDevicePropertyDeviceIsAlive,
    kAudioDevicePropertyNominalSampleRate, kAudioHardwareIllegalOperationError,
    kAudioHardwarePropertyProcessObjectList, kAudioHardwarePropertyServiceRestarted,
    kAudioHardwarePropertyTranslatePIDToProcessObject, kAudioObjectPropertyElementMain,
    kAudioObjectPropertyScopeGlobal, kAudioSubTapDriftCompensationKey, kAudioSubTapUIDKey,
    kAudioTapPropertyFormat, kAudioTapPropertyUID,
};
use objc2_core_audio_types::{
    AudioBufferList, AudioStreamBasicDescription, AudioTimeStamp, kAudioFormatFlagIsBigEndian,
    kAudioFormatFlagIsFloat, kAudioFormatLinearPCM,
};
use objc2_core_foundation::{CFArray, CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_foundation::{NSArray, NSNumber, NSString, NSUUID};

use crate::control::{
    Command, Hal, ListenerKey, ObjectId, SYSTEM_OBJECT, Status, TapFormat, TapObject, Watch,
};
use crate::ioproc::{IoContext, io_proc};
use crate::time;

/// [`io_proc`]'s type, with raw pointers so that a null from the HAL can never become an
/// invalid `NonNull`.
type RawIoProc = unsafe extern "C" fn(
    AudioObjectID,
    *const AudioTimeStamp,
    *const AudioBufferList,
    *const AudioTimeStamp,
    *mut AudioBufferList,
    *const AudioTimeStamp,
    *mut c_void,
) -> i32;

/// `AudioDeviceIOProc` as objc2-core-audio declares it.
type BoundIoProc = unsafe extern "C-unwind" fn(
    AudioObjectID,
    NonNull<AudioTimeStamp>,
    NonNull<AudioBufferList>,
    NonNull<AudioTimeStamp>,
    NonNull<AudioBufferList>,
    NonNull<AudioTimeStamp>,
    *mut c_void,
) -> i32;

type ListenerBlock = RcBlock<dyn Fn(u32, NonNull<AudioObjectPropertyAddress>)>;

struct Registered {
    object: AudioObjectID,
    address: AudioObjectPropertyAddress,
    block: ListenerBlock,
}

pub(crate) struct CoreAudio {
    notify: Sender<Command>,
    listeners: Vec<Option<Registered>>,
    aggregates: u64,
}

impl CoreAudio {
    /// Listener blocks forward each notification to the control thread through `notify`.
    pub fn new(notify: Sender<Command>) -> CoreAudio {
        CoreAudio {
            notify,
            listeners: Vec::new(),
            aggregates: 0,
        }
    }
}

impl Hal for CoreAudio {
    type Proc = AudioDeviceIOProcID;

    fn now_ns(&self) -> i64 {
        time::now_ns()
    }

    fn process_exists(&self, pid: i32) -> bool {
        // kill(0) and kill(-1) would ask about whole process groups.
        if pid <= 0 {
            return false;
        }
        // SAFETY: signal 0 sends nothing; it only checks that the process exists.
        let found = unsafe { libc::kill(pid, 0) } == 0;
        found || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }

    fn translate_pid(&mut self, pid: i32) -> Result<ObjectId, Status> {
        // SAFETY: the property is an AudioObjectID qualified by a pid_t.
        unsafe {
            get(
                SYSTEM_OBJECT,
                kAudioHardwarePropertyTranslatePIDToProcessObject,
                Some(&(pid as libc::pid_t)),
            )
        }
    }

    fn create_tap(&mut self, process: ObjectId) -> Result<TapObject, Status> {
        let processes = NSArray::from_retained_slice(&[NSNumber::new_u32(process)]);
        // SAFETY: the initializer takes an array of process object IDs as NSNumbers.
        let description = unsafe {
            CATapDescription::initStereoMixdownOfProcesses(CATapDescription::alloc(), &processes)
        };
        // Every field is set because the defaults are not what they seem: on macOS 27 a fresh
        // description is already private and has processRestoreEnabled = YES, with which the
        // HAL fills in the process's bundle ID and would tap anything else that shares it.
        // SAFETY: plain property setters on a description nothing else holds.
        unsafe {
            description.setName(&NSString::from_str("OpenLoop"));
            description.setUUID(&NSUUID::UUID());
            description.setPrivate(true);
            description.setMuteBehavior(CATapMuteBehavior::MutedWhenTapped);
            description.setMono(false);
            description.setExclusive(false);
            description.setMixdown(true);
            if description.respondsToSelector(sel!(setProcessRestoreEnabled:)) {
                description.setProcessRestoreEnabled(false);
            }
        }
        let mut tap: AudioObjectID = 0;
        // SAFETY: a complete description and a valid out pointer.
        check(unsafe { AudioHardwareCreateProcessTap(Some(&description), &mut tap) })?;
        // SAFETY: the tap UID property is a CFString the caller releases.
        match unsafe { get_string(tap, kAudioTapPropertyUID) } {
            Ok(uid) => Ok(TapObject { id: tap, uid }),
            Err(status) => {
                // SAFETY: the tap was created above and nothing else refers to it.
                unsafe { AudioHardwareDestroyProcessTap(tap) };
                Err(status)
            }
        }
    }

    fn tap_format(&mut self, tap: ObjectId) -> Result<TapFormat, Status> {
        // SAFETY: the tap format property is an AudioStreamBasicDescription.
        let format: AudioStreamBasicDescription =
            unsafe { get(tap, kAudioTapPropertyFormat, None::<&()>) }?;
        Ok(TapFormat {
            sample_rate: format.mSampleRate,
            channels: format.mChannelsPerFrame,
            float32: format.mFormatID == kAudioFormatLinearPCM
                && format.mFormatFlags & kAudioFormatFlagIsFloat != 0
                && format.mFormatFlags & kAudioFormatFlagIsBigEndian == 0
                && format.mBitsPerChannel == 32,
        })
    }

    fn destroy_tap(&mut self, tap: ObjectId) -> Result<(), Status> {
        // SAFETY: a tap this process created.
        check(unsafe { AudioHardwareDestroyProcessTap(tap) })
    }

    /// Private (gone with this process), tap only (no output device whose loss could stop its
    /// clock), no drift compensation, and tap autostart off, so starting never waits for the
    /// app's first audio (AudioHardware.h, kAudioAggregateDeviceTapAutoStartKey).
    fn create_aggregate(&mut self, tap_uid: &str, pid: i32) -> Result<ObjectId, Status> {
        // Destruction is asynchronous (AudioHardwareDestroyAggregateDevice), so a rebuilt
        // device never reuses a UID.
        self.aggregates += 1;
        let uid = CFString::from_str(&format!(
            "dev.openloop.audiotap.{pid}.{tap_uid}.{}",
            self.aggregates
        ));
        let name = CFString::from_str("OpenLoop Capture");
        let tap_uid = CFString::from_str(tap_uid);
        let (one, zero) = (CFNumber::new_i32(1), CFNumber::new_i32(0));

        let sub_tap_keys = [
            key(kAudioSubTapUIDKey),
            key(kAudioSubTapDriftCompensationKey),
        ];
        let sub_tap = CFDictionary::<CFString, CFType>::from_slices(
            &[&*sub_tap_keys[0], &*sub_tap_keys[1]],
            &[as_type(&*tap_uid), as_type(&*zero)],
        );
        let taps = CFArray::from_objects(&[&*sub_tap]);
        let keys = [
            key(kAudioAggregateDeviceNameKey),
            key(kAudioAggregateDeviceUIDKey),
            key(kAudioAggregateDeviceIsPrivateKey),
            key(kAudioAggregateDeviceTapAutoStartKey),
            key(kAudioAggregateDeviceTapListKey),
        ];
        let key_refs: Vec<&CFString> = keys.iter().map(|k| &**k).collect();
        let values = [
            as_type(&*name),
            as_type(&*uid),
            as_type(&*one),
            as_type(&*zero),
            as_type(&*taps),
        ];
        let description = CFDictionary::<CFString, CFType>::from_slices(&key_refs, &values);

        let mut device: AudioObjectID = 0;
        // SAFETY: a dictionary of the documented aggregate keys and a valid out pointer.
        check(unsafe {
            AudioHardwareCreateAggregateDevice(description.as_opaque(), NonNull::from(&mut device))
        })?;
        Ok(device)
    }

    fn device_alive(&mut self, device: ObjectId) -> Result<bool, Status> {
        // SAFETY: the property is a UInt32.
        unsafe { get::<u32, ()>(device, kAudioDevicePropertyDeviceIsAlive, None) }
            .map(|alive| alive != 0)
    }

    fn destroy_aggregate(&mut self, device: ObjectId) -> Result<(), Status> {
        // SAFETY: an aggregate device this process created.
        check(unsafe { AudioHardwareDestroyAggregateDevice(device) })
    }

    fn create_io_proc(
        &mut self,
        device: ObjectId,
        context: NonNull<IoContext>,
    ) -> Result<AudioDeviceIOProcID, Status> {
        // SAFETY: the same ABI. NonNull<T> is a transparent non-null pointer, and a "C" function
        // cannot unwind, which satisfies "C-unwind".
        let proc = unsafe { std::mem::transmute::<RawIoProc, BoundIoProc>(io_proc) };
        let mut id: AudioDeviceIOProcID = None;
        // SAFETY: the controller keeps `context` alive until the IOProc is destroyed.
        check(unsafe {
            AudioDeviceCreateIOProcID(
                device,
                Some(proc),
                context.as_ptr().cast(),
                NonNull::from(&mut id),
            )
        })?;
        Ok(id)
    }

    fn start(&mut self, device: ObjectId, proc: AudioDeviceIOProcID) -> Result<(), Status> {
        // SAFETY: an IOProc registered on this device.
        check(unsafe { AudioDeviceStart(device, proc) })
    }

    fn stop(&mut self, device: ObjectId, proc: AudioDeviceIOProcID) -> Result<(), Status> {
        // SAFETY: an IOProc registered on this device.
        check(unsafe { AudioDeviceStop(device, proc) })
    }

    fn destroy_io_proc(
        &mut self,
        device: ObjectId,
        proc: AudioDeviceIOProcID,
    ) -> Result<(), Status> {
        // SAFETY: an IOProc registered on this device.
        check(unsafe { AudioDeviceDestroyIOProcID(device, proc) })
    }

    fn add_listener(&mut self, object: ObjectId, watch: Watch) -> Result<ListenerKey, Status> {
        let mut address = address(match watch {
            Watch::ProcessList => kAudioHardwarePropertyProcessObjectList,
            Watch::ServiceRestarted => kAudioHardwarePropertyServiceRestarted,
            Watch::TapFormat => kAudioTapPropertyFormat,
            Watch::SampleRate => kAudioDevicePropertyNominalSampleRate,
            Watch::DeviceAlive => kAudioDevicePropertyDeviceIsAlive,
        });
        let notify = self.notify.clone();
        // Runs on a HAL notification thread: it only forwards, and the control thread makes
        // any calls the change needs. Notifications may repeat; each is just a reason to look.
        let block: ListenerBlock = RcBlock::new(
            move |_count: u32, _addresses: NonNull<AudioObjectPropertyAddress>| {
                let _ = notify.send(Command::Changed(watch));
            },
        );
        // SAFETY: a valid address and block; the HAL copies the block, and this keeps its own
        // reference until the listener is removed.
        check(unsafe {
            AudioObjectAddPropertyListenerBlock(
                object,
                NonNull::from(&mut address),
                None,
                RcBlock::as_ptr(&block),
            )
        })?;
        let registered = Some(Registered {
            object,
            address,
            block,
        });
        let slot = match self.listeners.iter().position(Option::is_none) {
            Some(slot) => {
                self.listeners[slot] = registered;
                slot
            }
            None => {
                self.listeners.push(registered);
                self.listeners.len() - 1
            }
        };
        Ok(ListenerKey(slot as u32))
    }

    fn remove_listener(&mut self, key: ListenerKey) -> Result<(), Status> {
        let Some(mut registered) = self
            .listeners
            .get_mut(key.0 as usize)
            .and_then(Option::take)
        else {
            return Err(Status(kAudioHardwareIllegalOperationError));
        };
        // SAFETY: the object, address, queue and block the listener was added with.
        check(unsafe {
            AudioObjectRemovePropertyListenerBlock(
                registered.object,
                NonNull::from(&mut registered.address),
                None,
                RcBlock::as_ptr(&registered.block),
            )
        })
    }

    fn forget_listeners(&mut self) {
        self.listeners.clear();
    }
}

fn address(selector: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    }
}

fn check(status: i32) -> Result<(), Status> {
    if status == 0 {
        Ok(())
    } else {
        Err(Status(status))
    }
}

/// Reads a fixed-size property.
///
/// # Safety
/// `T` is the property's plain-data type and `Q` its qualifier's.
unsafe fn get<T: Copy, Q>(
    object: AudioObjectID,
    selector: u32,
    qualifier: Option<&Q>,
) -> Result<T, Status> {
    let mut address = address(selector);
    let mut value = MaybeUninit::<T>::zeroed();
    let mut size = size_of::<T>() as u32;
    let (qualifier_size, qualifier) = match qualifier {
        Some(q) => (size_of::<Q>() as u32, ptr::from_ref(q).cast::<c_void>()),
        None => (0, ptr::null()),
    };
    // SAFETY: `value` has room for `size` bytes and the qualifier pointer matches its size.
    check(unsafe {
        AudioObjectGetPropertyData(
            object,
            NonNull::from(&mut address),
            qualifier_size,
            qualifier,
            NonNull::from(&mut size),
            NonNull::from(&mut value).cast(),
        )
    })?;
    // SAFETY: zeroed plain data, overwritten by the HAL.
    Ok(unsafe { value.assume_init() })
}

/// # Safety
/// The property is a CFString that the caller releases.
unsafe fn get_string(object: AudioObjectID, selector: u32) -> Result<String, Status> {
    // SAFETY: per this function's contract.
    let raw: *mut CFString = unsafe { get(object, selector, None::<&()>) }?;
    let string = NonNull::new(raw).ok_or(Status(kAudioHardwareIllegalOperationError))?;
    // SAFETY: the HAL hands over a +1 reference.
    Ok(unsafe { CFRetained::from_raw(string) }.to_string())
}

fn key(key: &CStr) -> CFRetained<CFString> {
    CFString::from_str(key.to_str().unwrap_or_default())
}

fn as_type<T: AsRef<CFType> + ?Sized>(value: &T) -> &CFType {
    value.as_ref()
}
