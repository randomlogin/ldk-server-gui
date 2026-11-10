// Records what the dashboard was built from, as BUILD_INFO_GUI=`version|branch|commit|dirty`, so the
// dashboard can show it next to the versions of the node it talks to. `dirty` is `true` when tracked
// files have uncommitted changes. Every field is `unknown` outside a git checkout.

use std::env;
use std::path::Path;
use std::process::Command;

fn main() {
	println!("cargo:rerun-if-changed=build.rs");

	let dir = Path::new(".");
	let version = env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "unknown".to_string());
	let unknown = || "unknown".to_string();
	let (branch, commit, dirty) = if git(&["rev-parse", "--git-dir"]).is_some() {
		// Run again when the commit changes, and when a source file does (it may make the tree
		// dirty).
		if let Some(git_dir) = git(&["rev-parse", "--git-dir"]) {
			watch(&dir.join(git_dir).join("HEAD"));
		}
		if let Some(common_dir) = git(&["rev-parse", "--git-common-dir"]) {
			let common_dir = dir.join(common_dir);
			watch(&common_dir.join("packed-refs"));
			if let Some(ref_path) = git(&["symbolic-ref", "-q", "HEAD"]) {
				watch(&common_dir.join(ref_path));
			}
		}
		watch(&dir.join("src"));
		watch(&dir.join("static"));
		watch(&dir.join("Cargo.toml"));

		let branch = git(&["branch", "--show-current"]).unwrap_or_else(|| "detached".to_string());
		let commit = git(&["rev-parse", "--short=8", "HEAD"]).unwrap_or_else(unknown);
		let dirty = git(&["status", "--porcelain", "--untracked-files=no"]).is_some();
		(branch, commit, dirty.to_string())
	} else {
		(unknown(), unknown(), unknown())
	};
	println!("cargo:rustc-env=BUILD_INFO_GUI={version}|{branch}|{commit}|{dirty}");
}

fn git(args: &[&str]) -> Option<String> {
	let output = Command::new("git").args(args).output().ok()?;
	if !output.status.success() {
		return None;
	}
	let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
	if stdout.is_empty() {
		None
	} else {
		Some(stdout)
	}
}

fn watch(path: &Path) {
	println!("cargo:rerun-if-changed={}", path.display());
}
