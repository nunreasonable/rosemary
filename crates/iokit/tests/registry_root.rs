use corefoundation::{api as cf, string};
use iokit::registry::*;
use std::ffi::CStr;

#[test]
fn root_handles_own_references_to_one_registry_identity() {
    unsafe {
        let first = IORegistryGetRootEntry(0);
        let second = IORegistryGetRootEntry(0);
        assert_ne!(first, 0);
        assert_ne!(first, second);
        assert_eq!(IOObjectIsEqualTo(first, second), 1);
        let (mut a, mut b) = (0, 0);
        assert_eq!(IORegistryEntryGetRegistryEntryID(first, &mut a), 0);
        assert_eq!(IORegistryEntryGetRegistryEntryID(second, &mut b), 0);
        assert_eq!(a, b);
        assert_eq!(IOObjectConformsTo(first, c"IORegistryEntry".as_ptr()), 1);
        assert_eq!(IOObjectConformsTo(first, c"IOHIDDevice".as_ptr()), 0);
        assert_eq!(IOObjectRetain(first), 0);
        assert_eq!(IOObjectRelease(first), 0);
        assert_eq!(IORegistryEntryGetRegistryEntryID(first, &mut a), 0);
        assert_eq!(IOObjectRelease(first), 0);
        assert_ne!(IORegistryEntryGetRegistryEntryID(first, &mut a), 0);
        assert_eq!(a, 0);
        assert_eq!(IORegistryEntryGetRegistryEntryID(second, &mut b), 0);
        IOObjectRelease(second);
        assert_eq!(IORegistryGetRootEntry(123), 0);
    }
}

#[test]
fn root_properties_and_paths_describe_only_the_supported_registry() {
    corefoundation::bootstrap();
    unsafe {
        let root = IORegistryGetRootEntry(0);
        let mut path = [0i8; 512];
        assert_eq!(
            IORegistryEntryGetPath(root, c"IOService".as_ptr(), path.as_mut_ptr()),
            0
        );
        assert_eq!(CStr::from_ptr(path.as_ptr()), c"IOService:/");
        let reopened = IORegistryEntryFromPath(0, path.as_ptr());
        assert_eq!(IOObjectIsEqualTo(root, reopened), 1);
        IOObjectRelease(reopened);
        let key = string::from_bytes(b"IORegistryPlanes", false);
        let plane = string::from_bytes(b"IOService", false);
        let planes = IORegistryEntryCreateCFProperty(root, key, std::ptr::null(), 0);
        assert!(!planes.is_null());
        assert_eq!(cf::CFDictionaryGetCount(planes), 1);
        assert_eq!(
            cf::CFEqual(cf::CFDictionaryGetValue(planes, plane), plane),
            1
        );
        let mut properties = std::ptr::null();
        assert_eq!(
            IORegistryEntryCreateCFProperties(root, &mut properties, std::ptr::null(), 0),
            0
        );
        let copied_planes = cf::CFDictionaryGetValue(properties, key);
        assert_eq!(cf::CFDictionaryGetCount(copied_planes), 1);
        assert_eq!(
            cf::CFEqual(cf::CFDictionaryGetValue(copied_planes, plane), plane),
            1
        );
        for value in [key, plane, planes, properties] {
            cf::CFRelease(value);
        }
        assert_ne!(
            IORegistryEntryGetPath(root, c"IOUSB".as_ptr(), path.as_mut_ptr()),
            0
        );
        assert_eq!(path[0], 0);
        let mut parent = 123;
        assert_ne!(
            IORegistryEntryGetParentEntry(root, c"IOService".as_ptr(), &mut parent),
            0
        );
        assert_eq!(parent, 0);
        IOObjectRelease(root);
    }
}

#[test]
fn root_iterator_remains_valid_even_when_empty() {
    unsafe {
        let root = IORegistryGetRootEntry(0);
        let mut iterator = 0;
        assert_eq!(
            IORegistryEntryCreateIterator(root, c"IOService".as_ptr(), 1, &mut iterator),
            0
        );
        assert_ne!(iterator, 0);
        let mut entries = Vec::new();
        loop {
            let device = IOIteratorNext(iterator);
            if device == 0 {
                break;
            }
            let mut parent = 0;
            assert_eq!(
                IORegistryEntryGetParentEntry(device, c"IOService".as_ptr(), &mut parent),
                0
            );
            assert_eq!(IOObjectIsEqualTo(root, parent), 1);
            IOObjectRelease(parent);
            entries.push(device);
        }
        assert_eq!(IOIteratorIsValid(iterator), 1);
        IOIteratorReset(iterator);
        for device in entries {
            assert_eq!(IOIteratorNext(iterator), device);
            IOObjectRelease(device);
        }
        assert_eq!(IOIteratorNext(iterator), 0);
        IOObjectRelease(iterator);
        iterator = 123;
        assert_ne!(
            IORegistryEntryCreateIterator(root, c"IOUSB".as_ptr(), 0, &mut iterator),
            0
        );
        assert_eq!(iterator, 0);
        assert_ne!(
            IORegistryEntryCreateIterator(root, c"IOService".as_ptr(), 2, &mut iterator),
            0
        );
        assert_eq!(iterator, 0);
        IOObjectRelease(root);
    }
}
