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
    Scan,
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
    /// Seed the known Playwright compat user (`playwright` /
    /// `playwright-test-pw`) plus a handful of placeholder items.
    /// Idempotent — re-running is a no-op for existing rows.
    SeedPlaywrightUser,
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
}
