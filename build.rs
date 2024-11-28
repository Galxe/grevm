//! This crate contains the build script for the grevm project.

use std::{path::Path, process::Command};

struct TestSubmodule {
    path: &'static str,
    url: &'static str,
    branch: &'static str,
    commit: Option<&'static str>,
}

fn git_clone(submodule: &TestSubmodule) -> Command {
    let mut command = Command::new("git");
    command.args(&["clone", "-b", submodule.branch, submodule.url, submodule.path]);
    command
}

fn git_fetch(submodule: &TestSubmodule) -> Command {
    let mut command = Command::new("git");
    command.args(&["-C", submodule.path, "fetch", "origin", submodule.branch]);
    command
}

fn git_checkout(submodule: &TestSubmodule) -> Command {
    let mut command = Command::new("git");
    if let Some(commit) = submodule.commit {
        command.args(&["-C", submodule.path, "checkout", commit]);
    } else {
        command.args(&["-C", submodule.path, "checkout", &format!("origin/{}", submodule.branch)]);
    }
    command
}

fn update_submodule(submodule: &TestSubmodule) {
    let test_data = Path::new(submodule.path);
    if !test_data.exists() && !git_clone(submodule).status().unwrap().success() {
        panic!("Failed to clone submodule {}", submodule.path);
    }

    if submodule.commit.is_some() {
        if git_checkout(submodule).status().unwrap().success() {
            return;
        }
    }

    if !git_fetch(submodule).status().unwrap().success() {
        panic!("Failed to fetch branch {} in submodule {}", submodule.branch, submodule.path);
    }

    if !git_checkout(submodule).status().unwrap().success() {
        panic!("Failed to checkout {} in submodule {}", submodule.branch, submodule.path);
    }
}

fn update_submodules(submodules: &[TestSubmodule]) {
    std::thread::scope(|s| {
        for submodule in submodules {
            s.spawn(|| {
                println!("Updating submodule {}", submodule.path);
                update_submodule(submodule);
                println!("Updating submodule {} done", submodule.path);
            });
        }
    });
}

fn main() {
    #[allow(unused_mut)]
    let mut submodules = vec![];

    #[cfg(feature = "update-submodule-test-data")]
    submodules.push(TestSubmodule {
        path: "test_data",
        url: "https://github.com/Galxe/grevm-test-data",
        branch: "main",
        commit: Some("4264bdf"),
    });

    #[cfg(feature = "update-submodule-ethereum-tests")]
    submodules.push(TestSubmodule {
        path: "tests/ethereum/tests",
        url: "https://github.com/ethereum/tests",
        branch: "develop",
        commit: Some("4f65a0a"),
    });

    update_submodules(&submodules);
}
