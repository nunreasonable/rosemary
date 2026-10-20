use corefoundation::api as cf;
use corefoundation::base::CFTypeRef;
use iokit::{api, hid::Device, registry};
use std::ffi::c_void;
use std::sync::Mutex;

#[derive(Default)]
struct Events {
    values: Vec<(u32, u32, i64)>,
    reports: Vec<Vec<u8>>,
    removed: usize,
    close_on_value: bool,
}
unsafe extern "C" fn value(
    context: *mut c_void,
    _result: i32,
    sender: *mut c_void,
    value: CFTypeRef,
) {
    let events = unsafe { &*(context as *const Mutex<Events>) };
    let element = unsafe { api::IOHIDValueGetElement(value) };
    let owner = unsafe { api::IOHIDElementGetDevice(element) };
    let id = unsafe { api::IOHIDDeviceGetService(owner) };
    let cookie = unsafe { api::IOHIDElementGetCookie(element) };
    let integer = unsafe { api::IOHIDValueGetIntegerValue(value) } as i64;
    let close = {
        let mut events = events.lock().unwrap();
        events.values.push((id, cookie, integer));
        events.close_on_value
    };
    if close {
        unsafe {
            api::IOHIDManagerClose(sender, 0);
        }
    }
}
unsafe extern "C" fn report(
    context: *mut c_void,
    _result: i32,
    _sender: *mut c_void,
    _kind: u32,
    id: u32,
    bytes: *mut u8,
    length: isize,
) {
    assert_eq!(id, 0);
    let events = unsafe { &*(context as *const Mutex<Events>) };
    events
        .lock()
        .unwrap()
        .reports
        .push(unsafe { std::slice::from_raw_parts(bytes, length as usize) }.to_vec());
}
unsafe extern "C" fn removed(context: *mut c_void, _result: i32, _sender: *mut c_void) {
    unsafe { &*(context as *const Mutex<Events>) }
        .lock()
        .unwrap()
        .removed += 1;
}
fn string(value: &[u8]) -> CFTypeRef {
    corefoundation::string::from_bytes(value, false)
}
fn turn(mode: CFTypeRef) {
    unsafe {
        cf::CFRunLoopRunInMode(mode, 0.04, 0);
    }
}
fn attach(vendor: u16, product: u16, kind: u16, axes: u16, buttons: u16, hats: u16) -> u32 {
    sdl3::virtual_joystick::attach(&sdl3::virtual_joystick::Desc {
        kind,
        vendor_id: vendor,
        product_id: product,
        naxes: axes,
        nbuttons: buttons,
        nhats: hats,
        name: c"no raw HID regression".as_ptr(),
        ..Default::default()
    })
    .expect("virtual attachment")
}

fn numbers(entries: &[(&[u8], i64)]) -> CFTypeRef {
    let entries: Vec<_> = entries
        .iter()
        .map(|(key, value)| {
            (
                string(key),
                corefoundation::collections::new_number(
                    corefoundation::collections::Number::Integer(*value),
                ),
            )
        })
        .collect();
    let dictionary = corefoundation::collections::new_dictionary(
        &entries,
        corefoundation::callbacks::Policy::from_key_callbacks(),
        corefoundation::callbacks::Policy::from_type_callbacks(),
        true,
    );
    for (key, value) in entries {
        unsafe {
            cf::CFRelease(key);
            cf::CFRelease(value);
        }
    }
    dictionary
}

#[test]
fn no_raw_hid_input_works_without_a_scheduled_manager_and_from_a_worker() {
    if !sdl3::available() || !sdl3::virtual_joystick::available() {
        eprintln!("skipped: SDL3 virtual joystick support unavailable");
        return;
    }
    corefoundation::bootstrap();
    sdl3::init_controllers().unwrap();
    let pool = unsafe { objc::api::objc_autoreleasePoolPush() };
    let sony = attach(0x054c, 0x0ce6, 1, 6, 15, 0);
    assert!(sdl3::virtual_joystick::set_mapping(
        sony,
        "a:b3,b:b2,x:b1,y:b0,leftx:a1,lefty:a0,rightx:a3,righty:a2,lefttrigger:a5,righttrigger:a4,dpup:b11,dpdown:b12,dpleft:b13,dpright:b14,"
    ));
    let microsoft = attach(
        sdl3::usb_vendor::MICROSOFT,
        sdl3::usb_product::XBOX_SERIES_CONTROLLER,
        1,
        6,
        15,
        0,
    );
    let stick = attach(0x1234, 0x5678, 4, 8, 9, 2);
    let device = unsafe { api::IOHIDDeviceCreate(std::ptr::null(), sony) };
    assert!(!device.is_null(), "creation must not depend on any manager");
    let d = unsafe { &*(device as *const Device) };
    assert!(!d.has_raw_reports());
    assert_eq!(
        (d.vendor, d.product),
        (
            sdl3::usb_vendor::MICROSOFT,
            sdl3::usb_product::XBOX_360_CONTROLLER
        )
    );
    assert_eq!(d.max_input_report_size(), 14);
    let second = unsafe { api::IOHIDDeviceCreate(std::ptr::null(), microsoft) };
    let generic = unsafe { api::IOHIDDeviceCreate(std::ptr::null(), stick) };
    let generic_d = unsafe { &*(generic as *const Device) };
    assert_eq!(generic_d.elements[0].usage, iokit::hid::usage::JOYSTICK);
    assert_eq!(generic_d.max_input_report_size(), 20);
    unsafe {
        let root = registry::IORegistryGetRootEntry(0);
        let mut iterator = 0;
        assert_eq!(
            registry::IORegistryEntryCreateIterator(root, c"IOService".as_ptr(), 1, &mut iterator),
            0
        );
        let mut found = Vec::new();
        loop {
            let entry = registry::IOIteratorNext(iterator);
            if entry == 0 {
                break;
            }
            found.push(entry);
            let mut parent = 0;
            assert_eq!(
                registry::IORegistryEntryGetParentEntry(entry, c"IOService".as_ptr(), &mut parent),
                0
            );
            assert_eq!(registry::IOObjectIsEqualTo(root, parent), 1);
            let mut path = [0i8; 512];
            assert_eq!(
                registry::IORegistryEntryGetPath(entry, c"IOService".as_ptr(), path.as_mut_ptr()),
                0
            );
            let reopened = registry::IORegistryEntryFromPath(0, path.as_ptr());
            assert_eq!(registry::IOObjectIsEqualTo(entry, reopened), 1);
            registry::IOObjectRelease(reopened);
            registry::IOObjectRelease(parent);
            registry::IOObjectRelease(entry);
        }
        assert!([sony, microsoft, stick].iter().all(|id| found.contains(id)));
        registry::IOObjectRelease(iterator);
        registry::IOObjectRelease(root);
    }

    unsafe {
        assert_eq!(
            registry::IOServiceGetMatchingService(
                0,
                registry::IORegistryEntryIDMatching(sony as u64)
            ),
            sony
        );
        assert_eq!(
            registry::IOServiceGetMatchingService(0, numbers(&[(b"VendorID", 0x054c)])),
            0,
            "registry filters must use the published compatibility identity"
        );
        assert_eq!(
            registry::IOServiceGetMatchingService(0, numbers(&[(b"DeviceUsage", 4)])),
            stick
        );
        assert_eq!(
            registry::IOObjectConformsTo(sony, c"IOHIDDevice".as_ptr()),
            1
        );
        let mut iterator = 0;
        assert_eq!(
            registry::IOServiceGetMatchingServices(
                0,
                registry::IOServiceMatching(c"IOAudioDevice".as_ptr()),
                &mut iterator
            ),
            0
        );
        assert_ne!(iterator, 0, "empty results still return a valid iterator");
        assert_eq!(registry::IOIteratorIsValid(iterator), 1);
        assert_eq!(registry::IOIteratorNext(iterator), 0);
        registry::IOObjectRelease(iterator);
    }

    let joystick = sdl3::open(sony).unwrap();
    let stick_handle = sdl3::open(stick).unwrap();
    let events = Mutex::new(Events::default());
    let other_events = Mutex::new(Events::default());
    let mode = string(b"rosemary HID test mode");
    let run_loop = unsafe { cf::CFRunLoopGetCurrent() };
    let mut buffer = [0; 14];
    unsafe {
        api::IOHIDDeviceRegisterInputValueCallback(
            device,
            Some(value),
            (&events as *const Mutex<Events>).cast_mut().cast(),
        );
        api::IOHIDDeviceRegisterInputValueCallback(
            second,
            Some(value),
            (&other_events as *const Mutex<Events>).cast_mut().cast(),
        );
        api::IOHIDDeviceRegisterInputReportCallback(
            device,
            buffer.as_mut_ptr(),
            14,
            Some(report),
            (&events as *const Mutex<Events>).cast_mut().cast(),
        );
        api::IOHIDDeviceRegisterRemovalCallback(
            device,
            Some(removed),
            (&events as *const Mutex<Events>).cast_mut().cast(),
        );
        api::IOHIDDeviceScheduleWithRunLoop(device, run_loop, mode);
        api::IOHIDDeviceScheduleWithRunLoop(device, run_loop, mode);
        api::IOHIDDeviceScheduleWithRunLoop(second, run_loop, mode);
        sdl3::virtual_joystick::set_axis(joystick, 1, 20_000);
        sdl3::virtual_joystick::set_axis(joystick, 2, i16::MIN);
        sdl3::virtual_joystick::set_button(joystick, 3, true);
        sdl3::virtual_joystick::set_axis(joystick, 5, i16::MAX);
        sdl3::virtual_joystick::set_axis(joystick, 4, i16::MIN);
        sdl3::virtual_joystick::set_hat(stick_handle, 0, 3);
        sdl3::virtual_joystick::set_button(stick_handle, 8, true);
    }
    turn(mode);
    assert!(events.lock().unwrap().values.contains(&(sony, 1, 20_000)));
    assert!(events.lock().unwrap().values.contains(&(sony, 3, 32_767)));
    assert!(events.lock().unwrap().values.contains(&(sony, 6, 0)));
    assert!(
        events.lock().unwrap().values.contains(&(sony, 5, 32_767)),
        "inverted endpoint must remain in range"
    );
    assert!(
        events.lock().unwrap().values.contains(&(sony, 7, 1)),
        "semantic A must follow SDL's mapping"
    );
    assert!(
        other_events
            .lock()
            .unwrap()
            .values
            .iter()
            .all(|(id, _, _)| *id == microsoft)
    );
    assert_eq!(
        events.lock().unwrap().reports.len(),
        1,
        "repeated scheduling must not duplicate reports"
    );
    let values = events.lock().unwrap().values.len();
    turn(mode);
    assert_eq!(events.lock().unwrap().values.len(), values);
    assert_eq!(events.lock().unwrap().reports.len(), 1);
    assert_eq!(generic_d.input_report()[16..], [1, 8, 0, 1]);
    let matching = corefoundation::collections::new_dictionary(
        &[(
            string(b"UsagePage"),
            corefoundation::collections::new_number(corefoundation::collections::Number::Integer(
                9,
            )),
        )],
        corefoundation::callbacks::Policy::from_key_callbacks(),
        corefoundation::callbacks::Policy::from_type_callbacks(),
        false,
    );
    let elements = unsafe { api::IOHIDDeviceCopyMatchingElements(device, matching, 0) };
    assert_eq!(unsafe { cf::CFArrayGetCount(elements) }, 15);
    assert_eq!(
        unsafe { api::IOHIDElementGetParent(d.handle(1)) },
        d.handle(0)
    );
    assert!(unsafe { api::IOHIDElementHasNullState(generic_d.handle(9)) });
    let mut bytes = [0; 14];
    let mut length = 14;
    unsafe {
        sdl3::virtual_joystick::set_axis(joystick, 1, -12_345);
    }
    assert_eq!(
        unsafe { api::IOHIDDeviceGetReport(device, 0, 0, bytes.as_mut_ptr(), &mut length) },
        0
    );
    assert_eq!(
        i16::from_le_bytes([bytes[0], bytes[1]]),
        -12_345,
        "synchronous reads refresh SDL state"
    );
    unsafe {
        api::IOHIDDeviceUnscheduleFromRunLoop(device, run_loop, mode);
    }
    unsafe {
        sdl3::virtual_joystick::set_axis(joystick, 1, 4567);
    }
    let count = events.lock().unwrap().values.len();
    turn(mode);
    assert_eq!(events.lock().unwrap().values.len(), count);

    // Video owns this thread, but a HID client on another run loop still needs
    // freshly decoded input. The dummy driver requires no display or window.
    sdl3::video::set_hint(c"SDL_VIDEO_DRIVER", c"dummy");
    assert!(
        sdl3::video::init_video(),
        "dummy video initialization: {}",
        sdl3::error()
    );
    let device_address = device as usize;
    let joystick_address = joystick as usize;
    std::thread::spawn(move || {
        assert!(!sdl3::may_pump());
        let events = Mutex::new(Events::default());
        let mut buffer = [0; 14];
        let device = device_address as CFTypeRef;
        let run_loop = unsafe { cf::CFRunLoopGetCurrent() };
        let mode = string(b"rosemary HID worker mode");
        unsafe {
            api::IOHIDDeviceRegisterInputValueCallback(
                device,
                Some(value),
                (&events as *const Mutex<Events>).cast_mut().cast(),
            );
            api::IOHIDDeviceRegisterInputReportCallback(
                device,
                buffer.as_mut_ptr(),
                14,
                Some(report),
                (&events as *const Mutex<Events>).cast_mut().cast(),
            );
            api::IOHIDDeviceScheduleWithRunLoop(device, run_loop, mode);
            sdl3::virtual_joystick::set_axis(joystick_address as sdl3::Joystick, 1, 9876);
        }
        turn(mode);
        assert!(events.lock().unwrap().values.contains(&(sony, 1, 9876)));
        assert_eq!(i16::from_le_bytes([buffer[0], buffer[1]]), 9876);
        unsafe {
            api::IOHIDDeviceUnscheduleFromRunLoop(device, run_loop, mode);
            api::IOHIDDeviceRegisterInputValueCallback(device, None, std::ptr::null_mut());
            api::IOHIDDeviceRegisterInputReportCallback(
                device,
                std::ptr::null_mut(),
                0,
                None,
                std::ptr::null_mut(),
            );
            cf::CFRelease(mode);
        }
    })
    .join()
    .unwrap();
    sdl3::video::quit_video();

    let enumeration = unsafe { api::IOHIDManagerCreate(std::ptr::null(), 0) };
    let manager_matching = numbers(&[(b"DeviceUsage", 4)]);
    unsafe {
        assert_eq!(api::IOHIDManagerOpen(enumeration, 0), 0);
        api::IOHIDManagerSetDeviceMatching(enumeration, manager_matching);
        let devices = api::IOHIDManagerCopyDevices(enumeration);
        assert_eq!(cf::CFSetGetCount(devices), 1);
        let mut entry = std::ptr::null();
        cf::CFSetGetValues(devices, &mut entry);
        assert_eq!(entry, generic);
        cf::CFRelease(devices);
        api::IOHIDManagerSetDeviceMatching(enumeration, std::ptr::null());
        let devices = api::IOHIDManagerCopyDevices(enumeration);
        assert_eq!(cf::CFSetGetCount(devices), 3);
        cf::CFRelease(devices);
        api::IOHIDManagerClose(enumeration, 0);
        cf::CFRelease(manager_matching);
    }

    // Manager histories are independent, and a manager may close from inside
    // a callback without more callbacks arriving from the same batch.
    let managers: Vec<_> = (0..2)
        .map(|_| unsafe { api::IOHIDManagerCreate(std::ptr::null(), 0) })
        .collect();
    let manager_events = [Mutex::new(Events::default()), Mutex::new(Events::default())];
    for (manager, events) in managers.iter().zip(&manager_events) {
        unsafe {
            api::IOHIDManagerRegisterInputValueCallback(
                *manager,
                Some(value),
                (events as *const Mutex<Events>).cast_mut().cast(),
            );
            api::IOHIDManagerScheduleWithRunLoop(*manager, run_loop, mode);
        }
    }
    let both_events = Mutex::new(Events::default());
    unsafe {
        api::IOHIDDeviceRegisterInputValueCallback(
            device,
            Some(value),
            (&both_events as *const Mutex<Events>).cast_mut().cast(),
        );
        api::IOHIDDeviceScheduleWithRunLoop(device, run_loop, mode);
    }
    turn(mode);
    for events in &manager_events {
        assert!(events.lock().unwrap().values.contains(&(sony, 1, 9876)));
        events.lock().unwrap().values.clear();
    }
    both_events.lock().unwrap().values.clear();
    unsafe {
        sdl3::virtual_joystick::set_axis(joystick, 1, 4321);
    }
    turn(mode);
    for events in [&both_events, &manager_events[0], &manager_events[1]] {
        assert_eq!(
            events
                .lock()
                .unwrap()
                .values
                .iter()
                .filter(|entry| **entry == (sony, 1, 4321))
                .count(),
            1,
            "each registration delivers once when device and managers are scheduled together"
        );
    }
    unsafe {
        api::IOHIDDeviceUnscheduleFromRunLoop(device, run_loop, mode);
        api::IOHIDDeviceRegisterInputValueCallback(device, None, std::ptr::null_mut());
    }
    for (manager, events) in managers.iter().zip(&manager_events) {
        events.lock().unwrap().values.clear();
        events.lock().unwrap().close_on_value = true;
        unsafe {
            api::IOHIDManagerRegisterInputValueCallback(
                *manager,
                Some(value),
                (events as *const Mutex<Events>).cast_mut().cast(),
            );
        }
    }
    turn(mode);
    for events in &manager_events {
        assert_eq!(events.lock().unwrap().values.len(), 1);
    }
    unsafe {
        api::IOHIDDeviceScheduleWithRunLoop(device, run_loop, mode);
    }
    assert!(sdl3::virtual_joystick::detach(sony));
    turn(mode);
    assert_eq!(events.lock().unwrap().removed, 1);
    assert!(!d.is_connected());
    assert_eq!(
        unsafe { api::IOHIDDeviceGetReport(device, 0, 0, bytes.as_mut_ptr(), &mut length) },
        -536_870_212
    );
    turn(mode);
    assert_eq!(events.lock().unwrap().removed, 1);
    unsafe {
        api::IOHIDDeviceClose(device, 0);
        api::IOHIDDeviceClose(second, 0);
        api::IOHIDDeviceRegisterRemovalCallback(device, None, std::ptr::null_mut());
        api::IOHIDDeviceRegisterInputValueCallback(second, None, std::ptr::null_mut());
        sdl3::close(joystick);
        sdl3::close(stick_handle);
        cf::CFRelease(elements);
        cf::CFRelease(matching);
        cf::CFRelease(mode);
        objc::api::objc_autoreleasePoolPop(pool);
    }
    sdl3::virtual_joystick::detach(microsoft);
    sdl3::virtual_joystick::detach(stick);
    Device::discover();
}
