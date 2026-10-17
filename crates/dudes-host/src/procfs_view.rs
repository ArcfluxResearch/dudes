#[cfg(target_os = "linux")]
pub(crate) fn read_current<T>(read: impl FnOnce() -> T) -> Option<T> {
    let status = std::fs::read_to_string("/proc/self/status").ok();
    read_if_aligned(status.as_deref(), std::process::id(), read)
}

fn read_if_aligned<T>(
    status: Option<&str>,
    current_tgid: u32,
    read: impl FnOnce() -> T,
) -> Option<T> {
    let status = status?;
    if current_tgid == 0 || current_tgid > i32::MAX as u32 {
        return None;
    }
    let mut observed = None;
    for line in status.lines() {
        if !line.trim_start().starts_with("NStgid") {
            continue;
        }
        let values = line.strip_prefix("NStgid:")?;
        if observed.is_some() {
            return None;
        }
        let mut values = values.split_ascii_whitespace();
        let value = values.next()?;
        if values.next().is_some()
            || value.starts_with('0')
            || !value.bytes().all(|byte| byte.is_ascii_digit())
        {
            return None;
        }
        let tgid = value.parse::<u32>().ok()?;
        if tgid != current_tgid {
            return None;
        }
        observed = Some(tgid);
    }
    observed.map(|_| read())
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    #[test]
    fn one_matching_positive_thread_group_certifies_the_pid_view() {
        for (pid, status) in [
            (42, "NStgid:\t42\n"),
            (1, "NStgid:\t1\n"),
            (42, "Name:\tagent\nNSpid:\t900\nNStgid:\t42\nThreads:\t4\n"),
            (2_147_483_647, "NStgid:\t2147483647\n"),
        ] {
            assert_eq!(
                read_if_aligned(Some(status), pid, || "observed"),
                Some("observed")
            );
        }
    }

    #[test]
    fn nested_or_uncertain_views_never_read_namespace_or_leader_evidence() {
        for status in [
            None,
            Some(""),
            Some("NSpid:\t42\n"),
            Some("NStgid:\t42\t42\n"),
            Some("NStgid:\t99\t42\n"),
            Some("NStgid:\t42\nNStgid:\t42\n"),
            Some("NStgid:\t42\nNStgid 42\n"),
            Some("NStgid:\n"),
            Some("NStgid 42\n"),
            Some("NStgid :42\n"),
            Some(" NStgid:\t42\n"),
            Some("NStgid:\t0\n"),
            Some("NStgid:\t43\n"),
            Some("NStgid:\t-42\n"),
            Some("NStgid:\t+42\n"),
            Some("NStgid:\t042\n"),
            Some("NStgid:\t4.2\n"),
            Some("NStgid:\t42x\n"),
            Some("NStgid:\t42\0\n"),
            Some("NStgid:\t2147483648\n"),
            Some("NStgid:\t4294967296\n"),
            Some("NStgid:\t18446744073709551616\n"),
        ] {
            let reads = Cell::new(0);
            assert_eq!(
                read_if_aligned(status, 42, || {
                    reads.set(reads.get() + 1);
                    "pid:[4026531836]"
                }),
                None,
                "status {status:?}"
            );
            assert_eq!(
                read_if_aligned(status, 42, || {
                    reads.set(reads.get() + 1);
                    999_u64
                }),
                None,
                "status {status:?}"
            );
            assert_eq!(reads.get(), 0);
        }
    }

    #[test]
    fn matching_alignment_cannot_retroactively_certify_an_uncertified_cache() {
        assert_eq!(
            read_if_aligned(Some("NStgid:\t42\n"), 42, || None::<String>).flatten(),
            None
        );
        assert_eq!(
            read_if_aligned(Some("NStgid:\t42\n"), 42, || Some("pid:[7]".to_owned())).flatten(),
            Some("pid:[7]".to_owned())
        );
        assert_eq!(
            read_if_aligned(Some("NStgid:\t42\t42\n"), 42, || Some("pid:[7]".to_owned())).flatten(),
            None
        );
    }

    #[test]
    fn invalid_current_thread_group_values_never_certify() {
        for pid in [0, 2_147_483_648, u32::MAX] {
            assert_eq!(
                read_if_aligned(Some(&format!("NStgid:\t{pid}\n")), pid, || ()),
                None
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn this_process_has_an_aligned_procfs_thread_group_view() {
        assert_eq!(read_current(std::process::id), Some(std::process::id()));
    }
}
