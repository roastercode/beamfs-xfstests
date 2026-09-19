// SPDX-License-Identifier: GPL-2.0-only
//
// Author: Aurelien Desbrieres <aurelien@hackers.camp>

//! Which machines to use and what to run the tests on.
//!
//! Defaults match the lab as it stands; every field can be overridden
//! from the environment so a different rig does not need a rebuild.

use std::time::Duration;

/// One machine in the pool.
#[derive(Debug, Clone)]
pub struct Node {
    pub name: String,
    pub host: String,
    /// TEST_DEV: mounted for the duration of a test.
    pub test_dev: String,
    /// SCRATCH_DEV: formatted and mounted by the suite itself, per test.
    ///
    /// Kept small on purpose. A test that fills the device -- generic/275
    /// and friends -- takes fourteen hours against 58 GiB at the write
    /// rate this filesystem manages under emulation, and minutes against
    /// 1 GiB. The suite does not care which; the operator does.
    pub scratch_dev: String,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub nodes: Vec<Node>,
    pub ssh_key: String,
    pub user: String,
    /// Killed past this. 300s covers everything measured so far; the
    /// slowest legitimate test was generic/247 at 583s on a busy node,
    /// so this will occasionally cost a real result. Better than a run
    /// that stops.
    pub per_test_timeout: Duration,
    pub mkfs_options: String,
    /// Which filesystem the suite runs against.
    ///
    /// beamfs unless told otherwise. The point of being able to change
    /// it is differential: this station has no ECC -- dmidecode says
    /// "Error Correction Type: None", EDAC says "No ECC support" -- so
    /// a bit flipped in host memory is silent, and a run that reports
    /// no kernel error proves nothing on its own. The same load, the
    /// same VM and the same devices against a filesystem known to be
    /// sound is what separates "the machine corrupts" from "beamfs
    /// corrupts".
    pub fstyp: String,
    /// Resume rather than restart: tests already recorded are skipped.
    pub resume: bool,
}

impl Default for Config {
    fn default() -> Self {
        let mk = |n: &str, h: &str, s: &str| Node {
            name: n.into(),
            host: h.into(),
            test_dev: "vdb".into(),
            scratch_dev: s.into(),
        };
        Self {
            nodes: vec![
                mk("master", "192.168.56.10", "vdc"),
                mk("compute01", "192.168.56.11", "vdh"),
                mk("compute02", "192.168.56.12", "vdh"),
                mk("compute03", "192.168.56.13", "vdh"),
            ],
            ssh_key: format!("{}/.ssh/hpclab_admin", std::env::var("HOME").unwrap_or_default()),
            user: "hpcadmin".into(),
            per_test_timeout: Duration::from_secs(300),
            mkfs_options: "-N 16384".into(),
            fstyp: "beamfs".into(),
            resume: true,
        }
    }
}

impl Config {
    /// Overlay the environment onto the defaults.
    #[must_use]
    pub fn from_env() -> Self {
        let mut c = Self::default();
        if let Ok(v) = std::env::var("XFSTESTS_TIMEOUT") {
            if let Ok(s) = v.parse::<u64>() {
                c.per_test_timeout = Duration::from_secs(s);
            }
        }
        if let Ok(v) = std::env::var("XFSTESTS_FSTYP") {
            if !v.trim().is_empty() {
                c.fstyp = v.trim().to_string();
            }
        }
        if let Ok(v) = std::env::var("XFSTESTS_MKFS_OPTIONS") {
            c.mkfs_options = v;
        }
        if let Ok(v) = std::env::var("XFSTESTS_NODES") {
            // host:test_dev:scratch_dev, comma separated
            let nodes: Vec<Node> = v
                .split(',')
                .filter_map(|spec| {
                    // Collected rather than pulled field by field:
                    // p.clone().next() for the name left the iterator
                    // where it was, so host got the name and every ssh
                    // went to a hostname that does not resolve.
                    let f: Vec<&str> = spec.split(':').collect();
                    if f.len() < 2 {
                        return None;
                    }
                    Some(Node {
                        name: f[0].into(),
                        host: f[1].into(),
                        test_dev: f.get(2).unwrap_or(&"vdb").to_string(),
                        scratch_dev: f.get(3).unwrap_or(&"vdc").to_string(),
                    })
                })
                .collect();
            if !nodes.is_empty() {
                c.nodes = nodes;
            }
        }
        if std::env::var("XFSTESTS_NO_RESUME").is_ok() {
            c.resume = false;
        }
        c
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_describe_the_lab() {
        let c = Config::default();
        assert_eq!(c.nodes.len(), 4);
        assert_eq!(c.nodes[0].name, "master");
        // master has the Kingston as scratch; the compute nodes have a
        // virtual 1 GiB disk, so the USB sticks reserved for injection
        // campaigns are not worn down by test formatting.
        assert_eq!(c.nodes[0].scratch_dev, "vdc");
        assert!(c.nodes[1..].iter().all(|n| n.scratch_dev == "vdh"));
    }

    /// The differential the station needs: no ECC here, so a run that
    /// reports no error proves nothing until the same load has been
    /// put through a filesystem known to be sound.
    #[test]
    fn the_filesystem_under_test_can_be_changed() {
        let _g = crate::env_lock();

        let c = Config::from_env();
        assert_eq!(c.fstyp, "beamfs");

        unsafe { std::env::set_var("XFSTESTS_FSTYP", "ext2") };
        let c = Config::from_env();
        unsafe { std::env::remove_var("XFSTESTS_FSTYP") };
        assert_eq!(c.fstyp, "ext2");
    }

    #[test]
    fn an_empty_value_leaves_the_default() {
        let _g = crate::env_lock();

        unsafe { std::env::set_var("XFSTESTS_FSTYP", "  ") };
        let c = Config::from_env();
        unsafe { std::env::remove_var("XFSTESTS_FSTYP") };
        assert_eq!(c.fstyp, "beamfs");
    }

    #[test]
    fn a_node_spec_parses_into_the_right_fields() {
        let _g = crate::env_lock();

        std::env::set_var("XFSTESTS_NODES", "c1:10.0.0.1:vdb:vdh,c2:10.0.0.2");
        let c = Config::from_env();
        std::env::remove_var("XFSTESTS_NODES");
        assert_eq!(c.nodes.len(), 2);
        assert_eq!(c.nodes[0].name, "c1");
        assert_eq!(c.nodes[0].host, "10.0.0.1");
        assert_eq!(c.nodes[0].scratch_dev, "vdh");
        // Defaults fill in what the spec omits.
        assert_eq!(c.nodes[1].host, "10.0.0.2");
        assert_eq!(c.nodes[1].test_dev, "vdb");
    }

    #[test]
    fn timeout_is_generous_enough_for_measured_tests() {
        // generic/013 took 164s, generic/069 342s. 300 is a compromise
        // that will occasionally cut a slow but valid test short.
        assert!(Config::default().per_test_timeout.as_secs() >= 300);
    }
}
