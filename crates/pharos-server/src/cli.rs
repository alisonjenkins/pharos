use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(name = "pharos", version, about = "Rust media server")]
pub struct Cli {
    /// Path to TOML config.
    #[arg(short, long, env = "PHAROS_CONFIG", default_value = "config.toml")]
    pub config: PathBuf,

    #[command(subcommand)]
    pub cmd: Cmd,
}

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// Start HTTP server.
    Serve,
    /// One-shot library scan.
    Scan {
        /// Re-probe every file even when unchanged on disk. Use after a
        /// probe-schema change (e.g. embedded-font MediaAttachments) to
        /// backfill the new fields onto already-indexed items — the
        /// incremental scan skips unchanged files by `(mtime, size)` and
        /// would never otherwise re-read them.
        #[arg(long)]
        force: bool,
    },
    /// Admin operations.
    Admin {
        #[command(subcommand)]
        op: AdminOp,
    },
}

#[derive(Debug, Subcommand)]
pub enum AdminOp {
    /// Print resolved config.
    PrintConfig,
    /// Create a user. This is the supported way to bootstrap the first
    /// admin on a fresh deployment. The password is read from `--password`
    /// or the `PHAROS_ADMIN_PASSWORD` env var (prefer the env var — a flag
    /// is visible in the process list and shell history). Idempotent: a
    /// name collision is reported and left as-is.
    CreateUser {
        /// Username.
        #[arg(long)]
        name: String,
        /// Password. Prefer the `PHAROS_ADMIN_PASSWORD` env var over the
        /// flag so the secret stays out of the process list / shell history.
        #[arg(long, env = "PHAROS_ADMIN_PASSWORD", hide_env_values = true)]
        password: String,
        /// Grant admin privileges. Needed for the first bootstrap user.
        #[arg(long)]
        admin: bool,
    },
    /// Reset an existing user's password out-of-band. The recovery path when
    /// a password is lost — works even when every admin is locked out, since
    /// it writes the store directly rather than going through the admin API.
    /// Password is read from `--password` or `PHAROS_ADMIN_PASSWORD` (prefer
    /// the env var — a flag is visible in the process list + shell history).
    ResetPassword {
        /// Username whose password to reset.
        #[arg(long)]
        name: String,
        /// New password. Prefer the `PHAROS_ADMIN_PASSWORD` env var over the
        /// flag so the secret stays out of the process list / shell history.
        #[arg(long, env = "PHAROS_ADMIN_PASSWORD", hide_env_values = true)]
        password: String,
    },
    /// Seed the known Playwright compat user (`playwright` /
    /// `playwright-test-pw`) plus a handful of placeholder items.
    /// Idempotent — re-running is a no-op for existing rows.
    ///
    /// Test tooling only — compiled out of release builds (the hardcoded
    /// credential must never ship). Use `create-user` in production.
    #[cfg(debug_assertions)]
    SeedPlaywrightUser,
    /// Just create the `playwright` admin user without generating
    /// any media fixtures. Useful when media is pre-populated by
    /// another process (e.g. the test-media OCI image dev-stack
    /// ships) and `pharos scan` registers the items separately.
    ///
    /// Test tooling only — compiled out of release builds. Use
    /// `create-user` in production.
    #[cfg(debug_assertions)]
    CreatePlaywrightUser,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use clap::Parser;

    #[test]
    fn parses_serve() {
        let cli = Cli::try_parse_from(["pharos", "--config", "x.toml", "serve"]).unwrap();
        assert_eq!(cli.config, PathBuf::from("x.toml"));
        assert!(matches!(cli.cmd, Cmd::Serve));
    }

    #[test]
    fn parses_admin_print_config() {
        let cli = Cli::try_parse_from(["pharos", "admin", "print-config"]).unwrap();
        assert!(matches!(
            cli.cmd,
            Cmd::Admin {
                op: AdminOp::PrintConfig
            }
        ));
    }

    #[test]
    fn parses_admin_create_user_with_admin_flag() {
        let cli = Cli::try_parse_from([
            "pharos",
            "admin",
            "create-user",
            "--name",
            "ali",
            "--password",
            "s3cret",
            "--admin",
        ])
        .unwrap();
        match cli.cmd {
            Cmd::Admin {
                op:
                    AdminOp::CreateUser {
                        name,
                        password,
                        admin,
                    },
            } => {
                assert_eq!(name, "ali");
                assert_eq!(password, "s3cret");
                assert!(admin);
            }
            other => panic!("expected create-user, got {other:?}"),
        }
    }

    #[test]
    fn create_user_defaults_admin_false() {
        let cli = Cli::try_parse_from([
            "pharos",
            "admin",
            "create-user",
            "--name",
            "bob",
            "--password",
            "pw",
        ])
        .unwrap();
        assert!(matches!(
            cli.cmd,
            Cmd::Admin {
                op: AdminOp::CreateUser { admin: false, .. }
            }
        ));
    }
}
