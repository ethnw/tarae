//! Processes from `/proc`: a process's parent (`/proc/<pid>/stat`), and the processes a pid has started —
//! what `--children` seeds at attach (later ones the BPF side follows as they fork).

use std::collections::{HashMap, VecDeque};

/// The parent pid in a `/proc/<pid>/stat` line. The comm (2nd field) is in parentheses and may hold spaces
/// and parentheses itself, so the fields are counted after the last `)`.
pub fn ppid(stat: &str) -> Option<u32> {
    let rest = &stat[stat.rfind(')')? + 1..];
    let mut f = rest.split_whitespace();
    let _state = f.next()?;
    f.next()?.parse().ok()
}

/// Every descendant of `root`, breadth first, from (pid, parent) pairs.
pub fn descendants(pairs: &[(u32, u32)], root: u32) -> Vec<u32> {
    let mut kids: HashMap<u32, Vec<u32>> = HashMap::new();
    for &(pid, parent) in pairs {
        kids.entry(parent).or_default().push(pid);
    }
    let (mut out, mut queue) = (Vec::new(), VecDeque::from([root]));
    while let Some(p) = queue.pop_front() {
        for &k in kids.get(&p).map(Vec::as_slice).unwrap_or_default() {
            if k != root && !out.contains(&k) {
                out.push(k);
                queue.push_back(k);
            }
        }
    }
    out
}

/// Whether `pid` runs — there and not a zombie.
pub fn alive(pid: u32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else { return false };
    stat.rfind(')').and_then(|i| stat[i + 1..].split_whitespace().next()) != Some("Z")
}

/// (pid, parent) of every process in `/proc` now.
pub fn all() -> Vec<(u32, u32)> {
    let Ok(dir) = std::fs::read_dir("/proc") else { return Vec::new() };
    dir.flatten()
        .filter_map(|e| {
            let pid: u32 = e.file_name().to_str()?.parse().ok()?;
            let stat = std::fs::read_to_string(e.path().join("stat")).ok()?;
            Some((pid, ppid(&stat)?))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The parent is found past a comm with spaces and parentheses.
    #[test]
    fn the_parent_past_any_comm() {
        assert_eq!(ppid("4242 (tarae) S 4100 4242 4100 34816 …"), Some(4100));
        assert_eq!(ppid("77 (rust-analyzer (x) y) R 4242 77 …"), Some(4242));
        assert_eq!(ppid("77 (no end"), None);
    }

    /// Children and their children, not the root, not other trees.
    #[test]
    fn descendants_of_a_pid() {
        let pairs = [(10, 1), (11, 10), (12, 10), (13, 11), (20, 1), (21, 20)];
        assert_eq!(descendants(&pairs, 10), [11, 12, 13]);
        assert_eq!(descendants(&pairs, 13), Vec::<u32>::new());
    }
}
