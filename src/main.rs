use std::process;

use database_migrator::cli::{Cli, emit_error};

#[tokio::main]
async fn main() {
    let cli = match Cli::try_parse_cli() {
        Ok(cli) => cli,
        Err(error) => {
            let exit_code = error.exit_code();
            let _ = error.print();
            process::exit(exit_code);
        }
    };

    let command = cli.command_name();
    let json_output = cli.is_json_output();
    if let Err(error) = cli.run().await {
        if json_output {
            if let Err(output_error) = emit_error(command, &error) {
                eprintln!("error: {output_error}");
            }
        } else {
            eprintln!("error: {error}");
        }
        process::exit(error.exit_code());
    }
}
