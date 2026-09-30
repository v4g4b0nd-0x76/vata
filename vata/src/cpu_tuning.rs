use std::io::{Error, ErrorKind, Result};

#[cfg(target_os = "linux")]
fn checked_cpu(cpu: usize) -> Result<()> {
    if cpu >= libc::CPU_SETSIZE as usize {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            format!("CPU {cpu} exceeds CPU_SETSIZE"),
        ));
    }
    Ok(())
}

/// Fails when `cpu` is not in this process's cpuset.
#[cfg(target_os = "linux")]
pub fn validate_cpu(cpu: usize) -> Result<()> {
    checked_cpu(cpu)?;
    let mut set = unsafe { std::mem::zeroed::<libc::cpu_set_t>() };
    if unsafe { libc::sched_getaffinity(0, std::mem::size_of_val(&set), &mut set) } != 0 {
        return Err(Error::last_os_error());
    }
    if unsafe { libc::CPU_ISSET(cpu, &set) } {
        Ok(())
    } else {
        Err(Error::new(
            ErrorKind::InvalidInput,
            format!("CPU {cpu} is outside this process's cpuset"),
        ))
    }
}

/// Pins the calling OS thread. Pin the allocator thread before arena creation to keep
/// Linux's first-touch placement on that CPU's NUMA node.
#[cfg(target_os = "linux")]
pub fn pin_current_thread(cpu: usize) -> Result<()> {
    validate_cpu(cpu)?;
    let mut set = unsafe { std::mem::zeroed::<libc::cpu_set_t>() };
    unsafe {
        libc::CPU_ZERO(&mut set);
        libc::CPU_SET(cpu, &mut set);
    }
    if unsafe { libc::sched_setaffinity(0, std::mem::size_of_val(&set), &set) } != 0 {
        return Err(Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn validate_cpu(_: usize) -> Result<()> {
    Err(Error::new(
        ErrorKind::Unsupported,
        "CPU pinning requires Linux",
    ))
}

#[cfg(not(target_os = "linux"))]
pub fn pin_current_thread(_: usize) -> Result<()> {
    Err(Error::new(
        ErrorKind::Unsupported,
        "CPU pinning requires Linux",
    ))
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn pin_current_thread_restricts_a_worker_to_an_allowed_cpu() {
        std::thread::spawn(|| {
            let mut before = unsafe { std::mem::zeroed::<libc::cpu_set_t>() };
            assert_eq!(
                unsafe { libc::sched_getaffinity(0, std::mem::size_of_val(&before), &mut before) },
                0
            );
            let cpu = (0..libc::CPU_SETSIZE as usize)
                .find(|&cpu| unsafe { libc::CPU_ISSET(cpu, &before) })
                .expect("test process must have an allowed CPU");

            pin_current_thread(cpu).unwrap();

            let mut after = unsafe { std::mem::zeroed::<libc::cpu_set_t>() };
            assert_eq!(
                unsafe { libc::sched_getaffinity(0, std::mem::size_of_val(&after), &mut after) },
                0
            );
            assert!(unsafe { libc::CPU_ISSET(cpu, &after) });
        })
        .join()
        .unwrap();
    }
}
