//! Main entrypoint for the KAI (Krill Agent Interface) standalone CLI binary.

use clap::Parser;
use kai_cli::args::{CliArgs, Commands, RunCommand};
use kai_cli::config::KaiConfig;
use kai_cli::{commands, ui};

#[tokio::main]
async fn main() {
    ui::init_terminal();

    let args = CliArgs::parse();

    let config = match KaiConfig::resolve(
        args.base_url,
        args.model,
        args.api_key,
        args.working_dir,
        args.max_turns,
        args.yes,
    ) {
        Ok(cfg) => cfg,
        Err(err) => {
            ui::print_error(&format!("Configuration resolution error: {err}"));
            std::process::exit(1);
        }
    };

    let result = match args.command {
        Some(Commands::Run(cmd)) => commands::run::execute(cmd, config).await,
        Some(Commands::Chat(cmd)) => commands::chat::execute(cmd, config).await,
        Some(Commands::Daemon(cmd)) => commands::daemon::execute(cmd, config).await,
        Some(Commands::Tools(cmd)) => commands::tools::execute(cmd),
        None => {
            if let Some(task) = args.prompt {
                commands::run::execute(RunCommand { task }, config).await
            } else {
                commands::chat::execute(kai_cli::args::ChatCommand {}, config).await
            }
        }
    };

    if let Err(err) = result {
        ui::print_error(&format!("{err}"));
        std::process::exit(1);
    }
}
