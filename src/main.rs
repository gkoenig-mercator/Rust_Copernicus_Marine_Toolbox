use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand, ValueEnum};
use log::error;

use copernicusmarine::credentials::{login, LoginOptions};
use copernicusmarine::describe::{describe, DescribeOptions};
use copernicusmarine::get::{get, GetOptions};
use copernicusmarine::subset::{subset, CoordinatesSelectionMethod, SubsetOptions, VerticalAxis};

#[derive(Parser)]
#[command(
    name = "copernicusmarine",
    version,
    about = "Barebone Rust version of the Copernicus Marine Toolbox: \
             access Copernicus Marine data from the command line."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, ValueEnum)]
enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
    Critical,
    Quiet,
}

#[derive(Args, Clone)]
struct CommonOptions {
    /// Set the details printed to console by the command
    #[arg(long, value_enum, default_value = "info", ignore_case = true)]
    log_level: LogLevel,
    /// Maximum number of concurrent requests
    #[arg(long, default_value_t = 15)]
    max_concurrent_requests: usize,
    /// Flag to hide the progress bar
    #[arg(long)]
    disable_progress_bar: bool,
    /// Use the staging (pre-production) environment
    #[arg(long, hide = true)]
    staging: bool,
}

#[derive(Args, Clone)]
struct CredentialsOptions {
    /// Copernicus Marine username (or COPERNICUSMARINE_SERVICE_USERNAME)
    #[arg(long)]
    username: Option<String>,
    /// Copernicus Marine password (or COPERNICUSMARINE_SERVICE_PASSWORD)
    #[arg(long)]
    password: Option<String>,
    /// Path to a credentials file (toolbox credentials, .netrc or motuclient file)
    #[arg(long)]
    credentials_file: Option<PathBuf>,
}

#[derive(Args, Clone)]
struct DatasetOptions {
    /// The datasetID
    #[arg(short = 'i', long)]
    dataset_id: String,
    /// Force the selection of a specific dataset version
    #[arg(long)]
    dataset_version: Option<String>,
    /// Force the selection of a specific dataset part
    #[arg(long)]
    dataset_part: Option<String>,
}

#[derive(Subcommand)]
enum Command {
    /// Create a configuration file with your Copernicus Marine credentials
    Login {
        #[arg(long)]
        username: Option<String>,
        #[arg(long)]
        password: Option<String>,
        /// Directory where the credentials file is stored
        #[arg(long)]
        configuration_file_directory: Option<PathBuf>,
        /// Overwrite the existing credentials file without asking
        #[arg(long)]
        force_overwrite: bool,
        /// Only check that the saved/provided credentials are valid
        #[arg(long)]
        check_credentials_valid: bool,
        #[arg(long, value_enum, default_value = "info", ignore_case = true)]
        log_level: LogLevel,
    },
    /// Print the catalogue metadata (products, datasets, services...) as JSON
    Describe {
        /// Only return the given product
        #[arg(short = 'p', long)]
        product_id: Option<String>,
        /// Only return the given dataset
        #[arg(short = 'i', long)]
        dataset_id: Option<String>,
        /// Filter the results with these strings (case insensitive)
        #[arg(short = 'c', long, num_args = 1..)]
        contains: Vec<String>,
        /// Show all versions and parts, not only the latest released ones
        #[arg(long)]
        show_all_versions: bool,
        /// Comma separated list of fields to return, e.g. "product_id,dataset_id"
        #[arg(short = 'r', long)]
        return_fields: Option<String>,
        /// Comma separated list of fields to exclude, e.g. "keywords,description"
        #[arg(short = 'e', long)]
        exclude_fields: Option<String>,
        #[command(flatten)]
        common: CommonOptions,
    },
    /// Download original files of a dataset
    Get {
        #[command(flatten)]
        dataset: DatasetOptions,
        #[command(flatten)]
        credentials: CredentialsOptions,
        /// The destination folder for the downloaded files
        #[arg(short = 'o', long, default_value = ".")]
        output_directory: PathBuf,
        /// Download files without the remote directory structure
        #[arg(short = 'n', long)]
        no_directories: bool,
        /// Shell-like glob filter on the file paths, e.g. "*_2023-01*"
        #[arg(long)]
        filter: Option<String>,
        /// Regular expression filter on the file paths
        #[arg(long)]
        regex: Option<String>,
        /// Path to a text file listing the files to download (one per line)
        #[arg(long)]
        file_list: Option<PathBuf>,
        /// Only create a file list (.txt or .csv) with the selected files
        #[arg(long)]
        create_file_list: Option<String>,
        /// Overwrite existing files
        #[arg(long)]
        overwrite: bool,
        /// Skip files that already exist locally
        #[arg(long)]
        skip_existing: bool,
        /// Print the files that would be downloaded without downloading them
        #[arg(long)]
        dry_run: bool,
        #[command(flatten)]
        common: CommonOptions,
    },
    /// Extract a subset of a dataset (area, time, depth, variables) to NetCDF
    Subset {
        #[command(flatten)]
        dataset: DatasetOptions,
        #[command(flatten)]
        credentials: CredentialsOptions,
        /// Variable name or standard name (can be repeated)
        #[arg(short = 'v', long = "variable")]
        variables: Vec<String>,
        #[arg(short = 'x', long, allow_hyphen_values = true)]
        minimum_longitude: Option<f64>,
        #[arg(short = 'X', long, allow_hyphen_values = true)]
        maximum_longitude: Option<f64>,
        #[arg(short = 'y', long, allow_hyphen_values = true)]
        minimum_latitude: Option<f64>,
        #[arg(short = 'Y', long, allow_hyphen_values = true)]
        maximum_latitude: Option<f64>,
        #[arg(short = 'z', long, allow_hyphen_values = true)]
        minimum_depth: Option<f64>,
        #[arg(short = 'Z', long, allow_hyphen_values = true)]
        maximum_depth: Option<f64>,
        /// Start datetime, e.g. "2024-01-01" or "2024-01-01T12:00:00"
        #[arg(short = 't', long)]
        start_datetime: Option<String>,
        /// End datetime, e.g. "2024-01-31" or "now"
        #[arg(short = 'T', long)]
        end_datetime: Option<String>,
        #[arg(long, value_enum, default_value = "inside")]
        coordinates_selection_method: CoordinatesSelectionMethod,
        #[arg(long, value_enum, default_value = "depth")]
        vertical_axis: VerticalAxis,
        /// Force a service: arco-geo-series (geoseries), arco-time-series (timeseries)...
        #[arg(short = 's', long)]
        service: Option<String>,
        #[arg(short = 'o', long, default_value = ".")]
        output_directory: PathBuf,
        #[arg(short = 'f', long)]
        output_filename: Option<String>,
        #[arg(long)]
        overwrite: bool,
        #[arg(long)]
        skip_existing: bool,
        /// Print the request summary without downloading
        #[arg(long)]
        dry_run: bool,
        #[command(flatten)]
        common: CommonOptions,
    },
}

fn init_logger(level: LogLevel) {
    let filter = match level {
        LogLevel::Debug => log::LevelFilter::Debug,
        LogLevel::Info => log::LevelFilter::Info,
        LogLevel::Warn => log::LevelFilter::Warn,
        LogLevel::Error | LogLevel::Critical => log::LevelFilter::Error,
        LogLevel::Quiet => log::LevelFilter::Off,
    };
    env_logger::Builder::new()
        .filter_level(log::LevelFilter::Warn)
        .filter_module("copernicusmarine", filter)
        .format(|buf, record| {
            writeln!(
                buf,
                "{} - {} - {}",
                record.level(),
                chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ"),
                record.args()
            )
        })
        .target(env_logger::Target::Stderr)
        .init();
}

fn print_json<T: serde::Serialize>(value: &T) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

async fn run(command: Command) -> anyhow::Result<bool> {
    match command {
        Command::Login {
            username,
            password,
            configuration_file_directory,
            force_overwrite,
            check_credentials_valid,
            log_level,
        } => {
            init_logger(log_level);
            login(LoginOptions {
                username,
                password,
                configuration_file_directory,
                force_overwrite,
                check_credentials_valid,
            })
            .await
        }
        Command::Describe {
            product_id,
            dataset_id,
            contains,
            show_all_versions,
            return_fields,
            exclude_fields,
            common,
        } => {
            init_logger(common.log_level);
            let json = describe(&DescribeOptions {
                product_id,
                dataset_id,
                contains,
                show_all_versions,
                return_fields,
                exclude_fields,
                max_concurrent_requests: common.max_concurrent_requests,
                staging: common.staging,
                show_progress: !common.disable_progress_bar,
            })
            .await?;
            print_json(&json)?;
            Ok(true)
        }
        Command::Get {
            dataset,
            credentials,
            output_directory,
            no_directories,
            filter,
            regex,
            file_list,
            create_file_list,
            overwrite,
            skip_existing,
            dry_run,
            common,
        } => {
            init_logger(common.log_level);
            let response = get(&GetOptions {
                dataset_id: dataset.dataset_id,
                dataset_version: dataset.dataset_version,
                dataset_part: dataset.dataset_part,
                username: credentials.username,
                password: credentials.password,
                credentials_file: credentials.credentials_file,
                output_directory,
                no_directories,
                filter,
                regex,
                file_list,
                create_file_list,
                overwrite,
                skip_existing,
                dry_run,
                max_concurrent_requests: common.max_concurrent_requests,
                show_progress: !common.disable_progress_bar,
                staging: common.staging,
            })
            .await?;
            if dry_run {
                print_json(&response)?;
            }
            Ok(true)
        }
        Command::Subset {
            dataset,
            credentials,
            variables,
            minimum_longitude,
            maximum_longitude,
            minimum_latitude,
            maximum_latitude,
            minimum_depth,
            maximum_depth,
            start_datetime,
            end_datetime,
            coordinates_selection_method,
            vertical_axis,
            service,
            output_directory,
            output_filename,
            overwrite,
            skip_existing,
            dry_run,
            common,
        } => {
            init_logger(common.log_level);
            let response = subset(&SubsetOptions {
                dataset_id: dataset.dataset_id,
                dataset_version: dataset.dataset_version,
                dataset_part: dataset.dataset_part,
                username: credentials.username,
                password: credentials.password,
                credentials_file: credentials.credentials_file,
                variables,
                minimum_longitude,
                maximum_longitude,
                minimum_latitude,
                maximum_latitude,
                minimum_depth,
                maximum_depth,
                start_datetime,
                end_datetime,
                coordinates_selection_method,
                vertical_axis,
                service,
                output_directory,
                output_filename,
                overwrite,
                skip_existing,
                dry_run,
                max_concurrent_requests: common.max_concurrent_requests,
                show_progress: !common.disable_progress_bar,
                staging: common.staging,
            })
            .await?;
            if dry_run {
                print_json(&response)?;
            }
            Ok(true)
        }
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli.command).await {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            error!("{e:#}");
            // Make sure the error is visible even with --log-level quiet
            if !log::log_enabled!(target: "copernicusmarine", log::Level::Error) {
                eprintln!("ERROR - {e:#}");
            }
            ExitCode::FAILURE
        }
    }
}
