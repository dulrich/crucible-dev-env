//! `cde-login` entrypoint: flags, logging, socket set-up, then the poll loop.

use std::path::PathBuf;
use std::time::Duration;

use crucible_login::daemon::{Config, Daemon};
use crucible_login::server;
use crucible_login::vt::RealVt;
use crucible_login::worker::PamBackend;

const USAGE: &str = "\
cde-login - the CDE login daemon (runs as root; replaces greetd)

Usage: cde-login [options]

Runs the greeter on its VT, serves the greetd protocol on GREETD_SOCK, starts the user
session on the session VT in graphics mode and switches to it once the session reports ready.

Options:
  --greeter-user <user>    account the greeter runs as (default: _greetd)
  --greeter-cmd <cmd>      greeter command (default: \"/opt/cde/bin/crucible greeter --profile /var/lib/cde-greeter\")
  --greeter-vt <n>         greeter VT (default: 7)
  --session-vt <n>         user session VT (default: 8)
  --ready-timeout <secs>   switch to the session VT after this long without ready (default: 10)
  --runtime-dir <dir>      directory for the greeter socket (default: /run/cde-login)
  -h, --help               show this help
  --build                  print the build id
  --version                print the version and build id
";

struct Args {
    cfg: Config,
    runtime_dir: PathBuf,
}

fn parse(args: &[String]) -> Result<Args, String> {
    let mut cfg = Config {
        greeter_user: "_greetd".into(),
        greeter_cmd: Vec::new(),
        greeter_vt: 7,
        session_vt: 8,
        ready_timeout: Duration::from_secs(10),
        sock_path: String::new(),
    };
    let mut cmd = "/opt/cde/bin/crucible greeter --profile /var/lib/cde-greeter".to_owned();
    let mut runtime_dir = PathBuf::from("/run/cde-login");
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        let mut val = || {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{flag} needs a value"))
        };
        let num = |v: String| v.parse::<u32>().map_err(|e| format!("{flag}: {e}"));
        match flag.as_str() {
            "--greeter-user" => cfg.greeter_user = val()?,
            "--greeter-cmd" => cmd = val()?,
            "--greeter-vt" => cfg.greeter_vt = num(val()?)?,
            "--session-vt" => cfg.session_vt = num(val()?)?,
            "--ready-timeout" => cfg.ready_timeout = Duration::from_secs(num(val()?)?.into()),
            "--runtime-dir" => runtime_dir = PathBuf::from(val()?),
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    cfg.greeter_cmd = cmd.split_whitespace().map(str::to_owned).collect();
    if cfg.greeter_cmd.is_empty() {
        return Err("--greeter-cmd is empty".into());
    }
    if cfg.greeter_vt == cfg.session_vt {
        return Err("--greeter-vt and --session-vt must differ".into());
    }
    Ok(Args { cfg, runtime_dir })
}

fn run(args: Args) -> Result<(), String> {
    let mut cfg = args.cfg;
    let (uid, gid) = server::lookup_user(&cfg.greeter_user)
        .map_err(|e| format!("greeter user {:?}: {e}", cfg.greeter_user))?;
    let (listener, path) = server::bind_socket(&args.runtime_dir, uid, gid)
        .map_err(|e| format!("greeter socket in {}: {e}", args.runtime_dir.display()))?;
    cfg.sock_path = path.to_string_lossy().into_owned();
    tracing::info!("cde-login listening on {}", cfg.sock_path);
    let daemon = Daemon::new(PamBackend, RealVt, cfg);
    server::serve(daemon, listener, &path, uid).map_err(|e| format!("serve: {e}"))
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print!("{USAGE}");
        return;
    }
    if args.iter().any(|a| a == "--build") {
        println!("{}", env!("FLEET_BUILD_ID"));
        return;
    }
    if args.iter().any(|a| a == "--version") {
        println!(
            "cde-login {} ({})",
            env!("CARGO_PKG_VERSION"),
            env!("FLEET_BUILD_ID")
        );
        return;
    }
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let parsed = match parse(&args) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("cde-login: {e}\n\n{USAGE}");
            std::process::exit(2);
        }
    };
    if let Err(e) = run(parsed) {
        tracing::error!("{e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| (*x).to_owned()).collect()
    }

    #[test]
    fn defaults_and_overrides() {
        let a = parse(&[]).unwrap();
        assert_eq!(a.cfg.greeter_user, "_greetd");
        assert_eq!(
            a.cfg.greeter_cmd,
            s(&[
                "/opt/cde/bin/crucible",
                "greeter",
                "--profile",
                "/var/lib/cde-greeter"
            ])
        );
        assert_eq!((a.cfg.greeter_vt, a.cfg.session_vt), (7, 8));
        assert_eq!(a.runtime_dir, PathBuf::from("/run/cde-login"));
        let a = parse(&s(&["--session-vt", "9", "--ready-timeout", "3"])).unwrap();
        assert_eq!(a.cfg.session_vt, 9);
        assert_eq!(a.cfg.ready_timeout, Duration::from_secs(3));
        assert!(parse(&s(&["--session-vt", "7"])).is_err());
        assert!(parse(&s(&["--bogus"])).is_err());
    }
}
