// SPDX-License-Identifier: GPL-2.0-only
//
// Author: Aurelien DESBRIERES <aurelien@hackers.camp>

//! Where this lab's files are, in one place instead of four.
//!
//! The build directory, the Yocto MACHINE and the image name were
//! literals in deploy.rs and bench.rs, one of them a full absolute
//! path with a user name in it. The tool built here and nowhere else,
//! and it could only ever target x86-64 -- which is a problem now that
//! the validation chain requires BX and BB to run the same image, and
//! that image may be either architecture.
//!
//! Every value keeps the exact default it had. With no variable set
//! the behaviour is identical; the environment only widens what is
//! possible. The test at the bottom asserts that equality, and it is
//! the whole safety argument for the change.

use std::sync::OnceLock;

fn from_env(var: &str, default: &str) -> String {
    std::env::var(var)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default.to_string())
}

fn from_home(var: &str, suffix: &str) -> String {
    std::env::var(var)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| {
            format!("{}{}", std::env::var("HOME").unwrap_or_default(), suffix)
        })
}

macro_rules! once {
    ($vis:vis $name:ident, $calc:ident) => {
        #[must_use]
        $vis fn $name() -> &'static str {
            static V: OnceLock<String> = OnceLock::new();
            V.get_or_init($calc).as_str()
        }
    };
}

fn calc_poky_dir() -> String {
    from_home("XFSTESTS_POKY_DIR", "/yocto/poky")
}

/// The build directory name, not its path.
///
/// Derived from the machine unless said otherwise. The two are a pair
/// and setting one without the other yields a path that does not
/// exist -- which is a poor way to learn that a campaign is aimed at
/// the wrong architecture. One variable selects a chain.
fn calc_build_dir_name() -> String {
    if let Ok(v) = std::env::var("XFSTESTS_BUILD_DIR") {
        let v = v.trim();
        if !v.is_empty() {
            return v.to_string();
        }
    }
    match calc_machine().as_str() {
        "qemuarm64" => "build-qemu-arm64".to_string(),
        "qemux86-64" => "build-qemux86".to_string(),
        // An unknown machine gets the Yocto convention rather than a
        // guess: bitbake's own layout is build-<machine>.
        m => format!("build-{m}"),
    }
}

/// The Yocto MACHINE the build targets.
fn calc_machine() -> String {
    from_env("XFSTESTS_MACHINE", "qemux86-64")
}

/// The image recipe name. It says arm64 and serves both architectures;
/// renaming it would break every path the two harnesses already use.
#[cfg_attr(not(test), allow(dead_code))]
fn calc_image_name() -> String {
    from_env("XFSTESTS_IMAGE", "hpc-arm64-research-beamfs")
}

fn calc_build_dir() -> String {
    format!("{}/{}", calc_poky_dir(), calc_build_dir_name())
}

/// Where bitbake leaves the images.
fn calc_deploy_dir() -> String {
    format!("{}/tmp/deploy/images/{}", calc_build_dir(), calc_machine())
}

/// The kernel binary. x86 calls it bzImage, arm64 calls it Image.
fn calc_kernel_image() -> String {
    let name = if calc_machine().starts_with("qemuarm") || calc_machine().contains("aarch64") {
        "Image"
    } else {
        "bzImage"
    };
    format!("{}/{}", calc_deploy_dir(), name)
}

// Only what is called. An accessor kept "for later" is dead code that
// -D warnings turns into a build failure, and the calc_ functions stay
// reachable through the tests either way.
once!(pub machine, calc_machine);
once!(pub deploy_dir, calc_deploy_dir);
once!(pub kernel_image, calc_kernel_image);

#[cfg(test)]
mod tests {
    use super::*;

    /// The environment belongs to the process and cargo runs tests in
    /// threads, so two tests setting the same variable read each
    /// other's value. Every test that touches the environment takes
    /// this first.
    fn clear() {
        for v in [
            "XFSTESTS_POKY_DIR",
            "XFSTESTS_BUILD_DIR",
            "XFSTESTS_MACHINE",
            "XFSTESTS_IMAGE",
        ] {
            unsafe { std::env::remove_var(v) };
        }
    }

    #[test]
    fn the_defaults_are_the_old_literals() {
        let _g = crate::env_lock();
        clear();
        let home = std::env::var("HOME").unwrap_or_default();
        assert_eq!(calc_build_dir_name(), "build-qemux86");
        assert_eq!(calc_machine(), "qemux86-64");
        assert_eq!(calc_image_name(), "hpc-arm64-research-beamfs");
        assert_eq!(
            calc_deploy_dir(),
            format!("{home}/yocto/poky/build-qemux86/tmp/deploy/images/qemux86-64")
        );
        assert_eq!(
            calc_kernel_image(),
            format!("{home}/yocto/poky/build-qemux86/tmp/deploy/images/qemux86-64/bzImage")
        );
    }

    #[test]
    fn arm64_gets_image_not_bzimage() {
        let _g = crate::env_lock();
        clear();
        unsafe { std::env::set_var("XFSTESTS_MACHINE", "qemuarm64") };
        unsafe { std::env::set_var("XFSTESTS_BUILD_DIR", "build-qemu-arm64") };
        assert!(calc_kernel_image().ends_with("/Image"));
        assert!(calc_deploy_dir().ends_with("/images/qemuarm64"));
        clear();
    }

    #[test]
    fn the_machine_alone_selects_a_chain() {
        // Setting the machine without the build directory used to give
        // a path that does not exist. One variable now names a chain.
        let _g = crate::env_lock();
        clear();
        unsafe { std::env::set_var("XFSTESTS_MACHINE", "qemuarm64") };
        assert_eq!(calc_build_dir_name(), "build-qemu-arm64");
        assert!(calc_deploy_dir().ends_with("build-qemu-arm64/tmp/deploy/images/qemuarm64"));
        assert!(calc_kernel_image().ends_with("/Image"));
        clear();
        assert_eq!(calc_build_dir_name(), "build-qemux86");
    }

    #[test]
    fn an_explicit_build_dir_still_wins() {
        let _g = crate::env_lock();
        clear();
        unsafe { std::env::set_var("XFSTESTS_MACHINE", "qemuarm64") };
        unsafe { std::env::set_var("XFSTESTS_BUILD_DIR", "build-ailleurs") };
        assert_eq!(calc_build_dir_name(), "build-ailleurs");
        clear();
    }

    #[test]
    fn an_empty_variable_is_not_an_override() {
        let _g = crate::env_lock();
        clear();
        unsafe { std::env::set_var("XFSTESTS_MACHINE", "   ") };
        assert_eq!(calc_machine(), "qemux86-64");
        clear();
    }
}
