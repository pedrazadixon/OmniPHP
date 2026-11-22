//! The Windows hosts file. Each OmniPHP install owns one clearly marked
//! block, tagged with the install's folder, and rewrites only that; everything
//! else in the file, including another install's block, is left as it is.

use std::path::{Path, PathBuf};

fn begin(root: &Path) -> String {
    format!("# BEGIN OmniPHP [{}] (managed automatically, do not edit)", root.display())
}

fn end(root: &Path) -> String {
    format!("# END OmniPHP [{}]", root.display())
}
pub fn hosts_path() -> PathBuf {
    let windows = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
    PathBuf::from(windows).join("System32").join("drivers").join("etc").join("hosts")
}

/// Letters, digits, `-`, `_` and `.` only: nothing that could smuggle a second
/// entry or a comment into the hosts file.
pub fn is_valid_hostname(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 253
        && !name.starts_with(['.', '-'])
        && !name.ends_with(['.', '-'])
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Hostnames currently inside the block of the install at `root`.
pub fn managed_names(hosts: &str, root: &Path) -> Vec<String> {
    let (begin, end) = (begin(root), end(root));
    hosts
        .lines()
        .skip_while(|line| line.trim() != begin)
        .skip(1)
        .take_while(|line| line.trim() != end)
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            (parts.next()? == "127.0.0.1").then(|| parts.next().map(str::to_string))?
        })
        .collect()
}

/// Returns `hosts` with the block of the install at `root` replaced by one
/// listing `names` (or removed when `names` is empty).
pub fn merge(hosts: &str, root: &Path, names: &[String]) -> String {
    let (begin, end) = (begin(root), end(root));
    let mut kept: Vec<&str> = Vec::new();
    let mut inside = false;
    for line in hosts.lines() {
        let trimmed = line.trim();
        if trimmed == begin {
            inside = true;
        } else if trimmed == end {
            inside = false;
        } else if !inside {
            kept.push(line);
        }
    }
    while kept.last().is_some_and(|line| line.trim().is_empty()) {
        kept.pop();
    }

    let mut out = String::new();
    for line in kept {
        out.push_str(line);
        out.push_str("\r\n");
    }
    if !names.is_empty() {
        if !out.is_empty() {
            out.push_str("\r\n");
        }
        out.push_str(&begin);
        out.push_str("\r\n");
        for name in names {
            out.push_str(&format!("127.0.0.1\t{name}\r\n"));
        }
        out.push_str(&end);
        out.push_str("\r\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|name| name.to_string()).collect()
    }

    fn root() -> &'static Path {
        Path::new(r"C:\OmniPHP")
    }

    #[test]
    fn adds_a_block_and_keeps_everything_else() {
        let original = "# Copyright\r\n127.0.0.1 ci4.test #laragon magic!\r\n\r\n";
        let merged = merge(original, root(), &names(&["shop.test", "blog.test"]));
        assert!(merged.starts_with(
            "# Copyright\r\n127.0.0.1 ci4.test #laragon magic!\r\n\r\n# BEGIN OmniPHP [C:\\OmniPHP]"
        ));
        assert_eq!(managed_names(&merged, root()), ["shop.test", "blog.test"]);
        assert!(merged.ends_with("# END OmniPHP [C:\\OmniPHP]\r\n"));
    }

    #[test]
    fn replaces_the_previous_block_instead_of_stacking() {
        let first = merge("1.2.3.4 other\n", root(), &names(&["a.test", "b.test"]));
        let second = merge(&first, root(), &names(&["c.test"]));
        assert_eq!(managed_names(&second, root()), ["c.test"]);
        assert_eq!(second.matches("# BEGIN OmniPHP").count(), 1);
        assert!(second.contains("1.2.3.4 other"));
        assert!(!second.contains("a.test"));
    }

    #[test]
    fn two_installs_keep_separate_blocks() {
        let other = Path::new(r"D:\OmniPHP");
        let hosts = merge("", root(), &names(&["test1.test"]));
        let hosts = merge(&hosts, other, &names(&["demo-a.test"]));
        assert_eq!(managed_names(&hosts, root()), ["test1.test"], "the first install's block survives");
        assert_eq!(managed_names(&hosts, other), ["demo-a.test"]);

        let hosts = merge(&hosts, other, &[]);
        assert_eq!(managed_names(&hosts, root()), ["test1.test"]);
        assert!(managed_names(&hosts, other).is_empty());
    }

    #[test]
    fn merging_is_stable() {
        let once = merge("1.2.3.4 other\r\n", root(), &names(&["a.test"]));
        assert_eq!(merge(&once, root(), &names(&["a.test"])), once);
    }

    #[test]
    fn empty_list_removes_the_block() {
        let with_block = merge("1.2.3.4 other\r\n", root(), &names(&["a.test"]));
        assert_eq!(merge(&with_block, root(), &[]), "1.2.3.4 other\r\n");
    }

    #[test]
    fn rejects_names_that_could_inject_lines() {
        assert!(is_valid_hostname("shop.test"));
        assert!(is_valid_hostname("dev_sac.test"));
        assert!(!is_valid_hostname("a.test evil.com"));
        assert!(!is_valid_hostname("a.test\n10.0.0.1 bank.com"));
        assert!(!is_valid_hostname("a.test#x"));
        assert!(!is_valid_hostname("*.a.test"));
        assert!(!is_valid_hostname(""));
    }
}
