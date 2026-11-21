//! uid/gid mapping for the container's user namespace (pure logic: parsing and argument building).

/// A range from /etc/subuid or /etc/subgid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubRange {
    pub start: u32,
    pub count: u32,
}

/// How container ids map onto host ids.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mapping {
    /// Container root is the calling user; no other id exists (a one-line uid_map written by ourselves).
    Single { uid: u32, gid: u32 },
    /// Container root is the calling user, container ids 1..=count map onto the subordinate ranges
    /// (written by the setuid helpers `newuidmap`/`newgidmap`).
    Full { uid: u32, gid: u32, sub_uid: SubRange, sub_gid: SubRange },
}

impl Mapping {
    pub fn is_full(&self) -> bool {
        matches!(self, Mapping::Full { .. })
    }
}

/// First usable range for `name` (or the numeric `uid`) in the text of /etc/subuid or /etc/subgid.
pub fn parse_subid(text: &str, name: &str, uid: u32) -> Option<SubRange> {
    text.lines().find_map(|l| {
        let mut f = l.trim().split(':');
        let who = f.next()?;
        let (start, count) = (f.next()?.parse::<u32>().ok()?, f.next()?.parse::<u32>().ok()?);
        ((who == name || who == uid.to_string()) && count > 0 && start.checked_add(count).is_some()).then_some(SubRange { start, count })
    })
}

/// User name for `uid` from the text of /etc/passwd.
pub fn user_name(passwd: &str, uid: u32) -> Option<String> {
    passwd.lines().find_map(|l| {
        let f: Vec<&str> = l.split(':').collect();
        (f.len() > 2 && f[2].parse() == Ok(uid)).then(|| f[0].to_string())
    })
}

/// Contents of /proc/<pid>/uid_map or gid_map for a single-id mapping.
pub fn single_line(host_id: u32) -> String {
    format!("0 {host_id} 1\n")
}

/// Arguments for `newuidmap`/`newgidmap <pid> ...`: container 0 = the user, 1.. = the subordinate range.
pub fn newidmap_args(pid: u32, host_id: u32, sub: SubRange) -> Vec<String> {
    [pid, 0, host_id, 1, 1, sub.start, sub.count].iter().map(u32::to_string).collect()
}

/// True when a /proc/self/uid_map text maps exactly one id (the single-id fallback).
pub fn is_single_map(uid_map: &str) -> bool {
    let total: u64 = uid_map.lines().filter_map(|l| l.split_whitespace().nth(2)?.parse::<u64>().ok()).sum();
    total <= 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subid_by_name_or_uid() {
        let t = "alice:100000:65536\nrl:165536:65536\n1001:300000:10\n";
        assert_eq!(parse_subid(t, "rl", 1001), Some(SubRange { start: 165536, count: 65536 }));
        assert_eq!(parse_subid(t, "bob", 1001), Some(SubRange { start: 300000, count: 10 }));
        assert_eq!(parse_subid(t, "bob", 5), None);
        assert_eq!(parse_subid("rl:1:0\n", "rl", 1), None);
        assert_eq!(parse_subid("garbage\nrl:x:y\n", "rl", 1), None);
    }

    #[test]
    fn passwd_lookup() {
        assert_eq!(user_name("root:x:0:0::/root:/bin/sh\nrl:x:1001:1001::/home/rl:/bin/sh\n", 1001).as_deref(), Some("rl"));
        assert_eq!(user_name("root:x:0:0\n", 7), None);
    }

    #[test]
    fn map_text() {
        assert_eq!(single_line(1001), "0 1001 1\n");
        assert_eq!(
            newidmap_args(42, 1001, SubRange { start: 165536, count: 65536 }),
            ["42", "0", "1001", "1", "1", "165536", "65536"]
        );
        assert!(is_single_map("         0       1001          1\n"));
        assert!(!is_single_map("         0       1001          1\n         1     165536      65536\n"));
        assert!(!is_single_map("         0          0 4294967295\n"));
    }
}
