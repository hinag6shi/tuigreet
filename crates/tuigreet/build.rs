use std::{env, process::Command};

fn main() {
  let version =
    git_version().unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());
  println!("cargo:rustc-env=VERSION={version}");
  println!("cargo:rustc-env=TARGET={}", env::var("TARGET").unwrap());
}

fn git_version() -> Option<String> {
  let out = Command::new("git")
    .args(["describe", "--long"])
    .output()
    .ok()?;
  if !out.status.success() {
    return None;
  }
  let s = String::from_utf8(out.stdout).ok()?;
  Some(s.trim().replacen('-', ".r", 1).replacen('-', ".", 1))
}
