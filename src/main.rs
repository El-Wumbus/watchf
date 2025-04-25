#![feature(exit_status_error)]

use clap::Parser;
use eyre::Context;
use notify::{EventKind, RecursiveMode, Watcher};
use serde::Deserialize;
use std::{
    any::Any, arch::x86_64::_MM_FROUND_RAISE_EXC, collections::HashMap, fs, io::Read, path::{Path, PathBuf}, process::{Command, Stdio}, sync::mpsc, time::SystemTime
};

#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    /// Name of the person to greet
    #[arg(short, long, default_value = "watchf.toml")]
    config_path: PathBuf,
    #[command(subcommand)]
    command:     Subcommand,
}

#[derive(Debug, clap::Subcommand, PartialEq)]
enum Subcommand {
    /// Build using the configured build command
    Build,
    /// Run using the configured run command
    Run,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
struct Config {
    /// The build command to use
    build_cmd: Vec<String>,
    /// The run command to use
    run_cmd:   Vec<String>,

    /// Files/directories to watch
    watch: Vec<PathBuf>,
}

impl Config {
    fn load(from: &Path) -> eyre::Result<Self> {
        let contents = std::fs::read_to_string(from)?;
        Ok(toml::de::from_str(&contents)?)
    }
}

fn main() -> eyre::Result<()> {
    let args = Args::parse();
    let config = Config::load(&args.config_path)?;
    let mut bins = HashMap::new();
    let mut rebuild = true;
    let mut last_rebuild = SystemTime::now();

    let (tx, rx) = mpsc::channel::<notify::Result<notify::Event>>();
    let (run_tx, run_rx) = mpsc::channel::<()>();
    
    let mut watcher = notify::recommended_watcher(tx)?;

    for f in config.watch.iter().map(PathBuf::as_path) {
        watcher.watch(f, RecursiveMode::Recursive)?;
    }
    
    // Use a condvar instead?
    if args.command == Subcommand::Run {
        std::thread::spawn({
            let run_cmd = config.run_cmd.clone();
            move || {
                let mut prog = Command::new(&run_cmd[0])
                    .args(&run_cmd[1..])
                    .spawn()
                    .unwrap();

                for _ in run_rx {
                    prog.kill().context(format!("failed to kill child with pid {}", prog.id())).unwrap();
                    prog.wait().unwrap();
                    prog = Command::new(&run_cmd[0])
                        .args(&run_cmd[1..])
                        .spawn()
                        .unwrap();
                }
            }
        });
    }

    loop {
        if rebuild {
            rebuild = false;
            last_rebuild = SystemTime::now();
            for path in build(&config)? {
                let meta = fs::metadata(&path)?;
                let modified = meta.modified()?;
                bins.insert(path.clone(), modified);
            }
            if args.command == Subcommand::Run {
                run_tx.send(()).unwrap();
            }
        }

        if let Ok(res) = rx.recv() {
            match res {
                Ok(event) if !matches!(event.kind, EventKind::Remove(_)) => {
                    for path in event.paths.iter() {
                        // Files can be removed and subsequently recreated when
                        // they're created by  editors. If the path doesn't
                        // exist, that's fine.
                        let Ok(meta) = fs::metadata(&path) else {
                            continue;
                        };
                        let modified = meta.modified()?;
                        if modified > last_rebuild && bins.values().any(|b| modified > *b)
                        {
                            rebuild = true;
                            break;
                        }
                    }
                }
                Err(e) => eprintln!("Watch error: {e}"),
                _ => {}
            }
        }
    }
}

/// Build the executable and return the paths of the executable build artifacts
fn build(config: &Config) -> eyre::Result<Vec<PathBuf>> {
    use serde_json::Value;

    #[derive(Default, Debug, Clone, PartialEq, Deserialize)]
    pub struct CompilerArtifact {
        pub reason:        String,
        pub package_id:    String,
        pub manifest_path: String,
        pub target:        Target,
        pub features:      Vec<String>,
        pub filenames:     Vec<String>,
        pub executable:    Option<PathBuf>,
        pub fresh:         bool,
    }

    #[derive(Default, Debug, Clone, PartialEq, Deserialize)]
    pub struct Target {
        pub kind:        Vec<String>,
        pub crate_types: Vec<String>,
        pub name:        String,
        pub src_path:    String,
        pub edition:     String,
        pub doc:         bool,
        pub doctest:     bool,
        pub test:        bool,
    }

    let mut cmd = Command::new(&config.build_cmd[0])
        .args(&config.build_cmd[1..])
        .args(["--message-format", "json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?;
    cmd.wait()?.exit_ok()?;

    let mut stdout = String::new();
    let mut stdout_r = cmd.stdout.unwrap();
    stdout_r.read_to_string(&mut stdout)?;

    let artifacts = stdout
        .lines()
        .flat_map(|line| serde_json::de::from_str::<Value>(line).ok())
        .filter(|v| {
            v.get("reason")
                .is_some_and(|reason| reason == "compiler-artifact")
        })
        .filter_map(|v| serde_json::from_value::<CompilerArtifact>(v.clone()).ok())
        .filter(|artifact| {
            artifact.executable.is_some()
                && artifact.target.kind.iter().any(|x| x == "bin")
        })
        .filter_map(|artifact| artifact.executable)
        .collect::<Vec<_>>();
    Ok(artifacts)
}
