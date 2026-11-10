// Records what the dashboard was built from, as BUILD_INFO_GUI=`version|branch|commit|dirty|url`, so
// the dashboard can show it next to the versions of the node it talks to. `dirty` is `true` when
// tracked files have uncommitted changes, and `url` is the repository's https address. Every field
// is `unknown` outside a git checkout.

use std::env;
use std::path::Path;
use std::process::Command;

fn main() {
	println!("cargo:rerun-if-changed=build.rs");

	let dir = Path::new(".");
	let version = env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "unknown".to_string());
	let unknown = || "unknown".to_string();
	let (branch, commit, dirty, url) = if git(&["rev-parse", "--git-dir"]).is_some() {
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
		(branch, commit, dirty.to_string(), repo_url())
	} else {
		(unknown(), unknown(), unknown(), unknown())
	};
	println!("cargo:rustc-env=BUILD_INFO_GUI={version}|{branch}|{commit}|{dirty}|{url}");
}

// The https address of the repository this branch is on: the remote it tracks, else `deploy`,
// `mine` or `origin`. `unknown` if there is none.
fn repo_url() -> String {
	let tracked = git(&["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{upstream}"])
		.and_then(|upstream| upstream.split('/').next().map(str::to_string));
	let fallbacks = ["deploy", "mine", "origin"].map(str::to_string);
	for remote in tracked.into_iter().chain(fallbacks) {
		if let Some(url) = git(&["remote", "get-url", &remote]).as_deref().and_then(https_url) {
			return url;
		}
	}
	"unknown".to_string()
}

// A git remote address (`https://...`, `ssh://git@host/...` or `git@host:path`) as the https address
// of the same repository, without credentials, port or `.git`. `None` for anything else.
fn https_url(remote: &str) -> Option<String> {
	let remote = remote.trim().trim_end_matches('/');
	let remote = remote.strip_suffix(".git").unwrap_or(remote);
	let (authority, path) = match remote.split_once("://") {
		Some((_, rest)) => rest.split_once('/')?,
		None => remote.split_once(':')?,
	};
	let host = authority.rsplit('@').next()?.split(':').next()?;
	let path = path.trim_start_matches('/');
	let plain = |s: &str| s.chars().all(|c| c.is_ascii_alphanumeric() || "._-/".contains(c));
	if host.is_empty() || path.is_empty() || !plain(host) || !plain(path) {
		return None;
	}
	Some(format!("https://{host}/{path}"))
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
