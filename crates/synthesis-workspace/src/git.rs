//! The single place that spawns `git`. See the crate docs for the threat model.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use synthesis_config::Env;

use crate::{Error, Result};

const NULL_DEVICE: &str = if cfg!(windows) { "NUL" } else { "/dev/null" };

#[derive(Clone)]
pub(crate) struct Git {
    exe: PathBuf,
    env: BTreeMap<String, String>,
    base_c: Vec<String>,
}

impl Git {
    /// `state_dir` hosts an empty directory used as `core.hooksPath`.
    pub fn new(env: &Env, state_dir: &Path, allow_global_config: bool) -> Result<Git> {
        let exe = find_git(env).ok_or(Error::GitMissing)?;
        let empty = state_dir.join("git-empty-hooks");
        std::fs::create_dir_all(&empty)?;
        let empty = synthesis_core::canonicalize(empty)?;
        // Allowlisted env only (no ambient env, no GIT_* from the caller, no secrets).
        let mut env: BTreeMap<String, String> = env
            .child_env()
            .iter()
            .filter(|(k, _)| !k.to_ascii_uppercase().starts_with("GIT_"))
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        for (k, v) in [
            ("GIT_CONFIG_NOSYSTEM", "1"),
            ("GIT_TERMINAL_PROMPT", "0"),
            ("GIT_OPTIONAL_LOCKS", "0"),
            ("GIT_ATTR_NOSYSTEM", "1"),
            ("GIT_PAGER", "cat"),
            ("GIT_EDITOR", "true"),
            ("GIT_ASKPASS", "true"),
            ("GIT_EXTERNAL_DIFF", ""),
        ] {
            env.insert(k.into(), v.into());
        }
        if !allow_global_config {
            env.insert("GIT_CONFIG_GLOBAL".into(), NULL_DEVICE.into());
        }
        let s = |k: &str, v: &str| ["-c".to_string(), format!("{k}={v}")];
        let hooks = empty.to_string_lossy().into_owned();
        let base_c = [
            ("core.hooksPath", hooks.as_str()),
            ("core.fsmonitor", "false"),
            ("core.sshCommand", "false"),
            ("protocol.allow", "never"),
            ("diff.external", ""),
            ("core.pager", "cat"),
            ("core.editor", "true"),
            ("core.askPass", ""),
            ("core.attributesFile", NULL_DEVICE),
            ("credential.helper", ""),
            ("gc.auto", "0"),
            ("maintenance.auto", "false"),
            ("commit.gpgsign", "false"),
            ("tag.gpgsign", "false"),
            ("core.quotePath", "false"),
        ]
        .iter()
        .flat_map(|(k, v)| s(k, v))
        .collect();
        Ok(Git { exe, env, base_c })
    }

    /// A copy whose every call is pinned to `git_dir` / `work_tree` (env beats `.git` discovery).
    pub fn pinned(&self, git_dir: &Path, work_tree: &Path) -> Git {
        let mut g = self.clone();
        g.env
            .insert("GIT_DIR".into(), git_dir.to_string_lossy().into_owned());
        g.env.insert(
            "GIT_WORK_TREE".into(),
            work_tree.to_string_lossy().into_owned(),
        );
        g
    }

    /// Runs with the base flags only (no per-repo guards): rev-parse / config probes.
    pub fn probe(&self, dir: &Path, args: &[&str]) -> Result<Output> {
        self.exec(dir, &[], args, None)
    }

    /// Runs with the base flags plus an override for every filter / textconv / merge-driver
    /// that the repo config defines. Returns the raw output (caller checks the status).
    pub fn run(&self, dir: &Path, args: &[&str], stdin: Option<&[u8]>) -> Result<Output> {
        let guards = self.guards(dir)?;
        self.exec(dir, &guards, args, stdin)
    }

    /// `run` + success check; returns stdout.
    pub fn out(&self, dir: &Path, args: &[&str]) -> Result<Vec<u8>> {
        self.check(args, self.run(dir, args, None)?)
    }

    pub fn out_str(&self, dir: &Path, args: &[&str]) -> Result<String> {
        Ok(String::from_utf8_lossy(&self.out(dir, args)?)
            .trim_end()
            .to_string())
    }

    pub fn check(&self, args: &[&str], o: Output) -> Result<Vec<u8>> {
        if o.status.success() {
            Ok(o.stdout)
        } else {
            Err(Error::Git {
                args: args.join(" "),
                code: o.status.code(),
                stderr: String::from_utf8_lossy(&o.stderr).trim().to_string(),
            })
        }
    }

    fn guards(&self, dir: &Path) -> Result<Vec<String>> {
        let o = self.exec(dir, &[], &["config", "--list", "-z"], None)?;
        if !o.status.success() {
            return Ok(Vec::new()); // not a repo / no config: nothing to neutralise
        }
        let mut out = Vec::new();
        for entry in o.stdout.split(|b| *b == 0) {
            let entry = String::from_utf8_lossy(entry);
            let key = entry.split('\n').next().unwrap_or("");
            let Some((section, rest)) = key.split_once('.') else {
                continue;
            };
            let Some((name, var)) = rest.rsplit_once('.') else {
                continue;
            };
            let var = var.to_ascii_lowercase();
            let section = section.to_ascii_lowercase();
            let (vars, value): (&[&str], &str) = match section.as_str() {
                "filter" => (&["clean", "smudge", "process"], ""),
                "diff" => (&["textconv", "command"], ""),
                "merge" => (&["driver", "recursivedriver"], ""),
                _ => continue,
            };
            if vars.contains(&var.as_str()) {
                out.push("-c".into());
                out.push(format!("{section}.{name}.{var}={value}"));
                if section == "filter" {
                    out.push("-c".into());
                    out.push(format!("filter.{name}.required=false"));
                }
            }
        }
        Ok(out)
    }

    /// Like [`Self::run`] but gives up after `limit`: git runs in its own process group (Unix) that
    /// is killed at the deadline (Windows: the git process itself), and the call returns
    /// `Error::Refused`. For network operations (submodule clones).
    pub fn run_deadline(
        &self,
        dir: &Path,
        args: &[&str],
        limit: std::time::Duration,
    ) -> Result<Output> {
        use std::io::Read;
        let guards = self.guards(dir)?;
        let mut c = self.command(dir, &guards, args, false);
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut c, 0);
        let mut child = c.spawn()?;
        let drain = |r: Option<Box<dyn Read + Send>>| {
            std::thread::spawn(move || {
                let mut v = Vec::new();
                if let Some(mut r) = r {
                    let _ = r.read_to_end(&mut v);
                }
                v
            })
        };
        let out = drain(
            child
                .stdout
                .take()
                .map(|s| Box::new(s) as Box<dyn Read + Send>),
        );
        let err = drain(
            child
                .stderr
                .take()
                .map(|s| Box::new(s) as Box<dyn Read + Send>),
        );
        let start = std::time::Instant::now();
        let status = loop {
            if let Some(st) = child.try_wait()? {
                break Some(st);
            }
            if start.elapsed() >= limit {
                #[cfg(unix)]
                if let Some(pid) = rustix::process::Pid::from_raw(child.id() as i32) {
                    let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
                }
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        };
        let (stdout, stderr) = (
            out.join().unwrap_or_default(),
            err.join().unwrap_or_default(),
        );
        match status {
            Some(status) => Ok(Output {
                status,
                stdout,
                stderr,
            }),
            None => Err(Error::Refused(format!(
                "git {} did not finish in {} s and was killed",
                args.iter()
                    .find(|a| !a.starts_with('-') && !a.contains('='))
                    .unwrap_or(&""),
                limit.as_secs()
            ))),
        }
    }

    fn command(&self, dir: &Path, extra: &[String], args: &[&str], piped_stdin: bool) -> Command {
        let mut c = Command::new(&self.exe);
        c.env_clear()
            .envs(&self.env)
            .current_dir(dir)
            .args(&self.base_c)
            .args(extra)
            .args(args)
            .stdin(if piped_stdin {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        c
    }

    fn exec(
        &self,
        dir: &Path,
        extra: &[String],
        args: &[&str],
        stdin: Option<&[u8]>,
    ) -> Result<Output> {
        let mut child = self.command(dir, extra, args, stdin.is_some()).spawn()?;
        if let (Some(data), Some(mut si)) = (stdin, child.stdin.take()) {
            // Feed from a thread so a full stdout pipe cannot deadlock us.
            let data = data.to_vec();
            std::thread::spawn(move || si.write_all(&data));
        }
        Ok(child.wait_with_output()?)
    }
}

/// Resolve `git` from the injected PATH to an absolute path, so a `git.exe` planted in the
/// (untrusted) working directory can never be picked up on Windows.
pub fn find_git(env: &Env) -> Option<PathBuf> {
    let path: OsString = env.get("PATH").or_else(|| env.get("Path"))?.into();
    let name = if cfg!(windows) { "git.exe" } else { "git" };
    std::env::split_paths(&path)
        .filter(|d| d.is_absolute())
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}
