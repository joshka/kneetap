//! The `kneetap` command.

use clap::Parser;

/// Command-line arguments.
#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {}

fn main() {
    Cli::parse();
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::Cli;

    #[test]
    fn command_definition_is_valid() {
        Cli::command().debug_assert();
    }
}
