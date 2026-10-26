fn main() {
	git_version();
	log_directives();
}

fn git_version() {
	// resolved by git: the package dir need not be the repo root, and in a worktree `.git` is a file
	if let Some(paths) = std::process::Command::new("git")
		.args(["rev-parse", "--git-path", "HEAD", "--git-path", "refs"])
		.output()
		.ok()
		.filter(|o| o.status.success())
	{
		for path in String::from_utf8_lossy(&paths.stdout).lines() {
			println!("cargo:rerun-if-changed={path}");
		}
	}
	// Embed git commit hash (fallback to "unknown" if git unavailable, e.g., in Nix sandbox)
	let git_hash = std::process::Command::new("git")
		.args(["rev-parse", "--short", "HEAD"])
		.output()
		.ok()
		.and_then(|o| if o.status.success() { Some(o.stdout) } else { None })
		.and_then(|stdout| String::from_utf8(stdout).ok())
		.map(|s| s.trim().to_string())
		.unwrap_or_else(|| "unknown".to_string());
	println!("cargo:rustc-env=GIT_HASH={git_hash}");
}

fn log_directives() {
	// Embed log directives if .cargo/log_directives exists
	println!("cargo:rerun-if-changed=.cargo/log_directives");
	if let Ok(directives) = std::fs::read_to_string(".cargo/log_directives") {
		let directives = directives.trim();
		if !directives.is_empty() {
			println!("cargo:rustc-env=LOG_DIRECTIVES={directives}");
		}
	}
}
