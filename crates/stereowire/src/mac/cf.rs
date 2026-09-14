//! Bridges between the Foundation and CoreFoundation type worlds.
//!
//! The Apple media APIs take CoreFoundation containers, but building those by
//! hand is far more code than using their toll-free-bridged Foundation twins.
//! These casts rely on that bridging: both sides are thin pointers to the same
//! object.

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_core_foundation::{CFArray, CFDictionary, CFString, CFType};
use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSString};

/// Reinterprets any CoreFoundation type as the generic `CFType`.
pub fn as_cf<T>(value: &T) -> &CFType {
    unsafe { &*(value as *const T as *const CFType) }
}

/// A CoreFoundation string key viewed as its Foundation twin.
pub fn key(name: &CFString) -> &NSString {
    unsafe { &*(name as *const CFString as *const NSString) }
}

/// An `NSDictionary` viewed as the `CFDictionary` the media APIs expect.
pub fn as_cf_dict(dict: &NSDictionary<NSString, AnyObject>) -> &CFDictionary {
    unsafe { &*(dict as *const NSDictionary<NSString, AnyObject> as *const CFDictionary) }
}

/// A `CFArray` viewed as an `NSArray` so it can be indexed with objc2 methods.
pub fn as_ns_array(array: &CFArray) -> &NSArray<NSDictionary<NSString, AnyObject>> {
    unsafe { &*(array as *const CFArray as *const NSArray<NSDictionary<NSString, AnyObject>>) }
}

pub fn bool_value(value: bool) -> Retained<AnyObject> {
    unsafe { Retained::cast_unchecked(NSNumber::numberWithBool(value)) }
}

pub fn i32_value(value: i32) -> Retained<AnyObject> {
    unsafe { Retained::cast_unchecked(NSNumber::numberWithInt(value)) }
}

pub fn f32_value(value: f32) -> Retained<AnyObject> {
    unsafe { Retained::cast_unchecked(NSNumber::numberWithFloat(value)) }
}

/// Builds a Foundation dictionary from CoreFoundation string keys.
pub fn dict(
    entries: &[(&CFString, Retained<AnyObject>)],
) -> Retained<NSDictionary<NSString, AnyObject>> {
    let keys: Vec<&NSString> = entries.iter().map(|(k, _)| key(k)).collect();
    let values: Vec<&AnyObject> = entries.iter().map(|(_, v)| &**v).collect();
    NSDictionary::from_slices(&keys, &values)
}
