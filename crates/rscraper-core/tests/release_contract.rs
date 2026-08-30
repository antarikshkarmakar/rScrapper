use serde_json::Value;
use std::collections::HashSet;
use std::process::Command;

#[test]
fn release_contract_uses_0_2_and_the_approved_dom_dependency_lines() {
    assert_eq!(rscraper_core::VERSION, "0.2.0");

    let output = Command::new(env!("CARGO"))
        .args(["metadata", "--locked", "--format-version", "1"])
        .current_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."))
        .output()
        .expect("cargo metadata must run");
    assert!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let metadata: Value = serde_json::from_slice(&output.stdout).expect("metadata must be JSON");
    let workspace_members: HashSet<&str> = metadata["workspace_members"]
        .as_array()
        .expect("workspace_members must be an array")
        .iter()
        .map(|member| {
            member
                .as_str()
                .expect("workspace member IDs must be strings")
        })
        .collect();
    let packages = metadata["packages"]
        .as_array()
        .expect("packages must be an array");

    let mut workspace_package_count = 0;
    let mut resolved_scraper_0_27 = false;
    let mut resolved_ego_tree_0_11 = false;
    for package in packages {
        let id = package["id"].as_str().expect("package IDs must be strings");
        let name = package["name"]
            .as_str()
            .expect("package names must be strings");
        let version = package["version"]
            .as_str()
            .expect("package versions must be strings");
        if workspace_members.contains(id) {
            workspace_package_count += 1;
            assert_eq!(
                version, "0.2.0",
                "workspace package {name} has the wrong release identity"
            );
        }
        resolved_scraper_0_27 |= name == "scraper" && version == "0.27.0";
        resolved_ego_tree_0_11 |= name == "ego-tree" && version.starts_with("0.11.");
    }

    assert_eq!(workspace_package_count, 5);
    assert!(resolved_scraper_0_27, "scraper 0.27.0 is not resolved");
    assert!(
        resolved_ego_tree_0_11,
        "the scraper 0.27-compatible ego-tree 0.11 line is not resolved"
    );
}
