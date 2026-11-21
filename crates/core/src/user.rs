//! Resolve a Dockerfile/image `USER` (`name`, `uid`, `name:group`, `uid:gid`) against /etc/passwd and /etc/group.
use crate::error::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ids {
    pub uid: u32,
    pub gid: u32,
}

fn bad(msg: String) -> Error {
    Error::Usage(msg)
}

/// `name:x:uid:gid:...` -> (uid, gid) for a user name or numeric uid.
fn passwd_entry(passwd: &str, who: &str) -> Option<Ids> {
    passwd.lines().find_map(|l| {
        let f: Vec<&str> = l.split(':').collect();
        let (uid, gid) = (f.get(2)?.parse().ok()?, f.get(3)?.parse().ok()?);
        (f[0] == who || f[2] == who).then_some(Ids { uid, gid })
    })
}

fn group_gid(group: &str, name: &str) -> Option<u32> {
    group.lines().find_map(|l| {
        let f: Vec<&str> = l.split(':').collect();
        (f.first() == Some(&name)).then(|| f.get(2)?.parse().ok())?
    })
}

pub fn resolve(spec: &str, passwd: &str, group: &str) -> Result<Ids> {
    if spec.is_empty() {
        return Ok(Ids { uid: 0, gid: 0 });
    }
    let (user, grp) = match spec.split_once(':') {
        Some((u, g)) => (u, Some(g)),
        None => (spec, None),
    };
    let mut ids = match (passwd_entry(passwd, user), user.parse::<u32>()) {
        (Some(ids), _) => ids,
        (None, Ok(uid)) => Ids { uid, gid: 0 }, // numeric uid without a passwd entry is allowed
        (None, Err(_)) => return Err(bad(format!("unknown user {user:?} in the image"))),
    };
    if let Some(g) = grp {
        ids.gid = match g.parse::<u32>() {
            Ok(n) => n,
            Err(_) => group_gid(group, g).ok_or_else(|| bad(format!("unknown group {g:?} in the image")))?,
        };
    }
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PASSWD: &str = "root:x:0:0:root:/root:/bin/sh\nnginx:x:101:101::/var/cache:/sbin/nologin\n";
    const GROUP: &str = "root:x:0:\nwww:x:82:nginx\n";

    #[test]
    fn resolves_names_numbers_and_groups() {
        assert_eq!(resolve("", PASSWD, GROUP).unwrap(), Ids { uid: 0, gid: 0 });
        assert_eq!(resolve("nginx", PASSWD, GROUP).unwrap(), Ids { uid: 101, gid: 101 });
        assert_eq!(resolve("101", PASSWD, GROUP).unwrap(), Ids { uid: 101, gid: 101 });
        assert_eq!(resolve("nginx:www", PASSWD, GROUP).unwrap(), Ids { uid: 101, gid: 82 });
        assert_eq!(resolve("nginx:5", PASSWD, GROUP).unwrap(), Ids { uid: 101, gid: 5 });
        assert_eq!(resolve("4242", PASSWD, GROUP).unwrap(), Ids { uid: 4242, gid: 0 });
    }

    #[test]
    fn unknown_names_are_errors() {
        assert!(resolve("ghost", PASSWD, GROUP).is_err());
        assert!(resolve("root:ghosts", PASSWD, GROUP).is_err());
    }
}
