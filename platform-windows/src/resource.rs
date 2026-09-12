//! Pure ownership helpers for device-local capture resources.

/// A failed rebuild leaves no resource live. DXGI forbids new-first replacement.
pub(crate) fn replace_after_drop<T, E>(slot: &mut Option<T>, create: impl FnOnce() -> Result<T, E>) -> Result<(), E> {
    drop(slot.take());
    *slot = Some(create()?);
    Ok(())
}

/// One resource per descriptor, owned by a single device's readback context.
pub(crate) fn cached<K: PartialEq, T, E>(
    slot: &mut Option<(K, T)>, key: K, create: impl FnOnce() -> Result<T, E>,
) -> Result<&T, E> {
    if slot.as_ref().is_none_or(|(old, _)| old != &key) {
        let value = create()?;
        *slot = Some((key, value));
    }
    Ok(&slot.as_ref().unwrap().1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn staging_reuses_descriptor_and_retries_failed_resize() {
        let mut slot = None;
        assert_eq!(*cached(&mut slot, (1920, 1080, 87), || Ok::<_, ()>(1)).unwrap(), 1);
        assert_eq!(*cached(&mut slot, (1920, 1080, 87), || -> Result<i32, ()> { panic!("allocated per frame") }).unwrap(), 1);
        assert!(cached(&mut slot, (1280, 720, 87), || Err(())).is_err());
        assert_eq!(*cached(&mut slot, (1280, 720, 87), || Ok::<_, ()>(2)).unwrap(), 2);
        assert_eq!(*cached(&mut slot, (1280, 720, 88), || Ok::<_, ()>(3)).unwrap(), 3);
    }

    #[test]
    fn duplication_is_dropped_before_reopen_even_on_failure() {
        struct Dup<'a>(&'a Cell<bool>);
        impl Drop for Dup<'_> { fn drop(&mut self) { self.0.set(false); } }
        let live = Cell::new(true);
        let mut slot = Some(Dup(&live));
        replace_after_drop(&mut slot, || {
            assert!(!live.get(), "DuplicateOutput while old duplication lives");
            live.set(true);
            Ok::<_, ()>(Dup(&live))
        }).unwrap();
        assert!(replace_after_drop(&mut slot, || {
            assert!(!live.get());
            Err(())
        }).is_err());
        assert!(slot.is_none());
    }
}
