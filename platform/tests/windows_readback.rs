// Compile the actual OS-free gate used by WGC on every test host.
#[allow(dead_code)]
#[path = "../../platform-windows/src/copy.rs"]
mod copy;

#[test]
fn excess_arrivals_do_not_execute_readback() {
    let interval = copy::interval_ns(30);
    let mut last = copy::initial_last_ns(0, interval);
    let mut copies = 0;
    for n in 0..60 {
        let now = n * copy::interval_ns(60);
        copy::readback_if_due(&mut last, now, interval, || { copies += 1; Some(()) });
    }
    assert_eq!(copies, 30);
}

#[test]
fn failed_readback_retries_without_advancing_clock() {
    let mut last = 0;
    let mut calls = 0;
    assert_eq!(copy::readback_if_due(&mut last, 9, 10, || { calls += 1; Some(7) }), None);
    assert_eq!(calls, 0);
    assert_eq!(copy::readback_if_due::<()>(&mut last, 10, 10, || None), None);
    assert_eq!(last, 0);
    assert_eq!(copy::readback_if_due(&mut last, 10, 10, || Some(7)), Some(7));
    assert_eq!(last, 10);
}
