//! Read numeric SCK status only for callbacks that could not yield pixels.
use objc2_core_foundation::{CFDictionary, CFNumber, CFNumberType, CFType};
use objc2_core_media::CMSampleBuffer;
use objc2_screen_capture_kit::SCStreamFrameInfoStatus;
use std::ffi::c_void;

fn dictionary_status(dictionary: &CFDictionary) -> Option<i64> {
    unsafe {
        let raw = dictionary.value(SCStreamFrameInfoStatus as *const _ as *const c_void);
        let value = (raw as *const CFType)
            .as_ref()?
            .downcast_ref::<CFNumber>()?;
        let mut status = 0i64;
        value
            .value(
                CFNumberType::SInt64Type,
                &mut status as *mut _ as *mut c_void,
            )
            .then_some(status)
    }
}

pub(crate) fn sample_status(sample: &CMSampleBuffer) -> Option<i64> {
    unsafe {
        let array = sample.sample_attachments_array(false)?;
        if array.count() == 0 {
            return None;
        }
        let item = (array.value_at_index(0) as *const CFType).as_ref()?;
        dictionary_status(item.downcast_ref::<CFDictionary>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use objc2_core_foundation::{
        kCFTypeDictionaryKeyCallBacks, kCFTypeDictionaryValueCallBacks, CFBoolean, CFRetained,
    };
    unsafe fn with_value(value: &CFType) -> CFRetained<CFDictionary> {
        let mut keys = [SCStreamFrameInfoStatus as *const _ as *const c_void];
        let mut values = [value as *const _ as *const c_void];
        CFDictionary::new(
            None,
            keys.as_mut_ptr(),
            values.as_mut_ptr(),
            1,
            &kCFTypeDictionaryKeyCallBacks,
            &kCFTypeDictionaryValueCallBacks,
        )
        .unwrap()
    }
    #[test]
    fn reads_numeric_status_without_treating_boolean_as_number() {
        unsafe {
            for status in 0..=5 {
                let value = CFNumber::new_i64(status);
                assert_eq!(dictionary_status(&with_value(&value)), Some(status));
            }
            assert_eq!(dictionary_status(&with_value(CFBoolean::new(true))), None);
        }
    }
}
